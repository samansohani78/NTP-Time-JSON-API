// Items here are part of the planned P0-2 wiring; suppress dead_code
// warnings until then (same pattern as protocol.rs).
#![allow(dead_code)]

//! Packet-level async NTP client (P0-1).
//!
//! Captures real T1/T4 timestamps and parses measured T2/T3, root_delay,
//! root_dispersion, and precision directly from the NTP packet — fields
//! that the previous `rsntp`-based client discarded.

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use std::time::{Duration, Instant, SystemTime};
use tokio::net::UdpSocket;

use super::protocol::{
    LI_ALARM_UNSYNCHRONIZED, LI_NO_WARNING, MODE_CLIENT, NTP_VERSION, STRATUM_UNSPECIFIED,
    STRATUM_UNSYNCHRONIZED, serialize_packet,
    {NtpPacket, ntp_short_to_ms, ntp_to_unix_ms, parse_server_response, unix_ms_to_ntp},
};

/// All fields measured or parsed from a single NTP exchange.
///
/// T2 and T3 come directly from the server's packet bytes — they are NOT
/// reconstructed from offset/delay (contrast with the former rsntp path).
#[derive(Debug, Clone)]
pub struct NtpSample {
    pub server: String,
    /// T1: client transmit (unix epoch ms, from SystemTime immediately before send)
    pub t1_unix_ms: i64,
    /// T2: server receive (unix epoch ms, parsed from reply `receive_timestamp`)
    pub t2_unix_ms: i64,
    /// T3: server transmit (unix epoch ms, parsed from reply `transmit_timestamp`)
    pub t3_unix_ms: i64,
    /// T4: client receive (unix epoch ms, from SystemTime immediately after recv)
    pub t4_unix_ms: i64,
    /// T1 as monotonic Instant (for RTT, step-immune)
    pub t1_instant: Instant,
    /// T4 as monotonic Instant (for RTT, step-immune)
    pub t4_instant: Instant,
    /// Clock offset θ = ((T2-T1)+(T3-T4))/2  [ms]
    pub offset_ms: i64,
    /// Round-trip delay δ = (T4-T1)-(T3-T2)  [ms]
    pub delay_ms: i64,
    /// Upstream root delay (NTP short → ms), parsed from reply
    pub root_delay_ms: u32,
    /// Upstream root dispersion (NTP short → ms), parsed from reply
    pub root_dispersion_ms: u32,
    /// Precision as log2(seconds), parsed from reply
    pub precision_log2: i8,
    pub stratum: u8,
    pub leap: u8,
    pub reference_id: u32,
    pub poll: i8,
}

/// Trait for NTP querying — production impl is [`PacketNtpClient`];
/// tests inject [`MockNtpClient`] via `Arc<dyn NtpClient>`.
#[async_trait]
pub trait NtpClient: Send + Sync {
    async fn query(&self, server: &str, timeout: Duration) -> Result<NtpSample>;
}

/// Production NTP client: sends a UDP NTPv4 packet and parses the response.
pub struct PacketNtpClient;

#[async_trait]
impl NtpClient for PacketNtpClient {
    async fn query(&self, server: &str, timeout: Duration) -> Result<NtpSample> {
        query_impl(server, timeout).await
    }
}

async fn query_impl(server: &str, timeout_dur: Duration) -> Result<NtpSample> {
    // 1. Resolve host:port → SocketAddr
    let addr = tokio::net::lookup_host(server)
        .await
        .with_context(|| format!("DNS resolution failed for {server}"))?
        .next()
        .with_context(|| format!("No address resolved for {server}"))?;

    // 2. Bind ephemeral UDP socket and connect
    let socket = UdpSocket::bind("0.0.0.0:0")
        .await
        .context("Failed to bind UDP socket")?;
    socket
        .connect(addr)
        .await
        .context("Failed to connect UDP socket")?;

    // 3. Capture T1 and build request — both captures happen back-to-back
    //    to minimise the skew between the two clocks.
    let t1_instant = Instant::now();
    let t1_sys = SystemTime::now();
    let t1_unix_ms = system_time_unix_ms(t1_sys);

    let mut request = NtpPacket::new(LI_NO_WARNING, NTP_VERSION, MODE_CLIENT);
    // The server echoes this back as origin_timestamp; we verify it to detect
    // stale or spoofed replies.
    request.transmit_timestamp = unix_ms_to_ntp(t1_unix_ms);
    let buf = serialize_packet(&request);

    socket
        .send(&buf)
        .await
        .context("Failed to send NTP request")?;

    // 4. Receive with timeout; capture T4 immediately on return
    let mut recv_buf = [0u8; 512];
    let n = tokio::time::timeout(timeout_dur, socket.recv(&mut recv_buf))
        .await
        .context("NTP query timed out")?
        .context("Failed to receive NTP response")?;

    let t4_instant = Instant::now();
    let t4_sys = SystemTime::now();
    let t4_unix_ms = system_time_unix_ms(t4_sys);

    // 5. Parse the server response packet
    let reply =
        parse_server_response(&recv_buf[..n]).context("Failed to parse NTP server response")?;

    // 6. Safety-critical validations (must happen before we use any reply fields)
    validate_response(&reply, request.transmit_timestamp)?;

    // 7. Extract MEASURED T2/T3 directly from packet bytes
    let t2_unix_ms = ntp_to_unix_ms(reply.receive_timestamp);
    let t3_unix_ms = ntp_to_unix_ms(reply.transmit_timestamp);

    // 8. Compute offset and delay (RFC 5905 §8)
    //    θ = ((T2-T1)+(T3-T4))/2
    //    δ = (T4-T1)-(T3-T2)
    let offset_ms = ((t2_unix_ms - t1_unix_ms) + (t3_unix_ms - t4_unix_ms)) / 2;
    let delay_ms = (t4_unix_ms - t1_unix_ms) - (t3_unix_ms - t2_unix_ms);

    if delay_ms < 0 {
        bail!(
            "NTP response has negative delay ({delay_ms} ms) — \
             possible clock error or spoofed reply"
        );
    }

    // 9. Parse root fields (NTP short format → ms)
    let root_delay_ms = ntp_short_to_ms(reply.root_delay) as u32;
    let root_dispersion_ms = ntp_short_to_ms(reply.root_dispersion) as u32;

    Ok(NtpSample {
        server: server.to_string(),
        t1_unix_ms,
        t2_unix_ms,
        t3_unix_ms,
        t4_unix_ms,
        t1_instant,
        t4_instant,
        offset_ms,
        delay_ms,
        root_delay_ms,
        root_dispersion_ms,
        precision_log2: reply.precision,
        stratum: reply.stratum,
        leap: reply.li,
        reference_id: reply.reference_id,
        poll: reply.poll,
    })
}

/// Validate an NTP server reply against the safety-critical conditions
/// required by RFC 5905 §8 and common security guidance.
fn validate_response(reply: &NtpPacket, our_transmit_ts: u64) -> Result<()> {
    // Origin timestamp must echo our transmit timestamp exactly.
    // Mismatch means a stale reply (retransmit storm) or a spoofed packet.
    if reply.origin_timestamp != our_transmit_ts {
        bail!(
            "NTP origin timestamp mismatch (stale or spoofed reply): \
             expected {:#018x}, got {:#018x}",
            our_transmit_ts,
            reply.origin_timestamp
        );
    }

    // Stratum 0 = Kiss-of-Death (KoD); server is telling us to back off.
    if reply.stratum == STRATUM_UNSPECIFIED {
        bail!("NTP Kiss-of-Death reply (stratum=0)");
    }

    // Stratum >= 16 = server is unsynchronized.
    if reply.stratum >= STRATUM_UNSYNCHRONIZED {
        bail!("NTP server is unsynchronized (stratum={})", reply.stratum);
    }

    // LI=3 (alarm) means the server has lost synchronization.
    if reply.li == LI_ALARM_UNSYNCHRONIZED {
        bail!("NTP server reports leap alarm (LI=3, unsynchronized)");
    }

    // Zero transmit timestamp means the server did not fill in the time.
    if reply.transmit_timestamp == 0 {
        bail!("NTP transmit timestamp is zero (server not synced or bogus reply)");
    }

    Ok(())
}

fn system_time_unix_ms(t: SystemTime) -> i64 {
    t.duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// Test-only mock client
// ---------------------------------------------------------------------------

/// A scripted mock NTP client for unit tests.  Inject via `Arc<dyn NtpClient>`.
#[cfg(test)]
pub struct MockNtpClient {
    pub response: Result<NtpSample, String>,
}

#[cfg(test)]
impl MockNtpClient {
    pub fn ok(sample: NtpSample) -> Self {
        Self {
            response: Ok(sample),
        }
    }
    pub fn err(msg: impl Into<String>) -> Self {
        Self {
            response: Err(msg.into()),
        }
    }
}

#[cfg(test)]
#[async_trait]
impl NtpClient for MockNtpClient {
    async fn query(&self, _server: &str, _timeout: Duration) -> Result<NtpSample> {
        match &self.response {
            Ok(s) => Ok(s.clone()),
            Err(e) => bail!("{}", e),
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::super::protocol::{
        LI_NO_WARNING, MODE_SERVER, NTP_VERSION, NtpPacket, STRATUM_PRIMARY, ntp_to_unix_ms,
        parse_packet, serialize_packet, unix_ms_to_ntp,
    };
    use super::*;
    use tokio::net::UdpSocket;

    // ── Mock NTP server helper ────────────────────────────────────────────

    struct MockServer {
        addr: String,
        handle: tokio::task::JoinHandle<()>,
    }

    impl MockServer {
        /// Spawn a one-shot UDP server. `build_reply` receives the parsed
        /// client request and returns the response packet to send back.
        async fn start(build_reply: impl Fn(&NtpPacket) -> NtpPacket + Send + 'static) -> Self {
            let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
            let addr = socket.local_addr().unwrap().to_string();
            let handle = tokio::spawn(async move {
                let mut buf = [0u8; 512];
                if let Ok((n, peer)) = socket.recv_from(&mut buf).await
                    && let Ok(req) = parse_packet(&buf[..n])
                {
                    let reply = build_reply(&req);
                    let bytes = serialize_packet(&reply);
                    let _ = socket.send_to(&bytes, peer).await;
                }
            });
            MockServer { addr, handle }
        }

        /// Spawn a silent server (receives but never replies).
        async fn start_silent() -> Self {
            let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
            let addr = socket.local_addr().unwrap().to_string();
            let handle = tokio::spawn(async move {
                let mut buf = [0u8; 512];
                let _ = socket.recv_from(&mut buf).await; // receive, discard
                // Hold socket open so the client's send doesn't get ECONNREFUSED
                tokio::time::sleep(Duration::from_secs(60)).await;
            });
            MockServer { addr, handle }
        }

        fn addr(&self) -> &str {
            &self.addr
        }
    }

    impl Drop for MockServer {
        fn drop(&mut self) {
            self.handle.abort();
        }
    }

    /// Build a well-formed server reply that echoes the client's transmit
    /// timestamp and carries the given T2/T3/root fields.
    fn good_reply(
        req: &NtpPacket,
        t2_ntp: u64,
        t3_ntp: u64,
        root_delay: u32,
        root_dispersion: u32,
    ) -> NtpPacket {
        NtpPacket {
            li: LI_NO_WARNING,
            vn: NTP_VERSION,
            mode: MODE_SERVER,
            stratum: STRATUM_PRIMARY,
            poll: 4,
            precision: -20,
            root_delay,
            root_dispersion,
            reference_id: u32::from_be_bytes(*b"LOCL"),
            ref_timestamp: t2_ntp,
            origin_timestamp: req.transmit_timestamp, // echo client T1
            receive_timestamp: t2_ntp,
            transmit_timestamp: t3_ntp,
        }
    }

    // ── Tests ─────────────────────────────────────────────────────────────

    /// T2 and T3 in the returned sample must be byte-identical to the
    /// values the mock placed in receive_timestamp / transmit_timestamp —
    /// they must NOT be algebraic reconstructions.
    #[tokio::test]
    async fn reads_real_t2_t3_byte_for_byte() {
        // T3 < T2: physically odd (server transmits before it receives) but
        // valid as a unit-test fixture — it gives delay = RTT - (T3-T2) =
        // RTT + 1000 > 0, keeping the test free of the negative-delay check
        // while still using distinct values for the two packet fields.
        // Whole-second values give an exact unix_ms ↔ NTP roundtrip.
        let t2_ms: i64 = 1_700_000_002_000; // receive_timestamp byte value
        let t3_ms: i64 = 1_700_000_001_000; // transmit_timestamp byte value
        let t2_ntp = unix_ms_to_ntp(t2_ms);
        let t3_ntp = unix_ms_to_ntp(t3_ms);

        let mock = MockServer::start(move |req| good_reply(req, t2_ntp, t3_ntp, 0, 0)).await;

        let sample = PacketNtpClient
            .query(mock.addr(), Duration::from_secs(2))
            .await
            .expect("query should succeed");

        // These must equal the exact decoded bytes, not a reconstruction.
        assert_eq!(
            sample.t2_unix_ms,
            ntp_to_unix_ms(t2_ntp),
            "T2 must come directly from packet receive_timestamp"
        );
        assert_eq!(
            sample.t3_unix_ms,
            ntp_to_unix_ms(t3_ntp),
            "T3 must come directly from packet transmit_timestamp"
        );
    }

    /// root_delay and root_dispersion must decode via ntp_short_to_ms.
    #[tokio::test]
    async fn parses_root_delay_dispersion() {
        // 0x00040000 = 4 seconds = 4000 ms; 0x00050000 = 5000 ms
        let rd: u32 = 0x0004_0000;
        let rdisp: u32 = 0x0005_0000;

        let mock = MockServer::start(move |req| {
            let now = unix_ms_to_ntp(1_700_000_000_000);
            good_reply(req, now, now, rd, rdisp)
        })
        .await;

        let sample = PacketNtpClient
            .query(mock.addr(), Duration::from_secs(2))
            .await
            .expect("query should succeed");

        assert_eq!(sample.root_delay_ms, 4000, "root_delay_ms should be 4000");
        assert_eq!(
            sample.root_dispersion_ms, 5000,
            "root_dispersion_ms should be 5000"
        );
    }

    /// RFC 5905 offset/delay formulas must hold exactly.
    #[tokio::test]
    async fn offset_delay_formula_holds() {
        // T3 < T2 → delay = RTT + 1000 > 0 (see reads_real_t2_t3_byte_for_byte).
        let t2_ntp = unix_ms_to_ntp(1_700_000_002_000);
        let t3_ntp = unix_ms_to_ntp(1_700_000_001_000);

        let mock = MockServer::start(move |req| good_reply(req, t2_ntp, t3_ntp, 0, 0)).await;

        let s = PacketNtpClient
            .query(mock.addr(), Duration::from_secs(2))
            .await
            .expect("query should succeed");

        let expected_offset = ((s.t2_unix_ms - s.t1_unix_ms) + (s.t3_unix_ms - s.t4_unix_ms)) / 2;
        let expected_delay = (s.t4_unix_ms - s.t1_unix_ms) - (s.t3_unix_ms - s.t2_unix_ms);

        assert_eq!(s.offset_ms, expected_offset, "offset formula must hold");
        assert_eq!(s.delay_ms, expected_delay, "delay formula must hold");
    }

    /// A reply whose origin_timestamp does not match our T1 must be rejected.
    #[tokio::test]
    async fn rejects_origin_mismatch() {
        let mock = MockServer::start(|req| {
            let t = unix_ms_to_ntp(1_700_000_000_000);
            NtpPacket {
                li: LI_NO_WARNING,
                vn: NTP_VERSION,
                mode: MODE_SERVER,
                stratum: STRATUM_PRIMARY,
                poll: 4,
                precision: -20,
                root_delay: 0,
                root_dispersion: 0,
                reference_id: 0,
                ref_timestamp: t,
                origin_timestamp: req.transmit_timestamp.wrapping_add(1), // wrong!
                receive_timestamp: t,
                transmit_timestamp: t,
            }
        })
        .await;

        let err = PacketNtpClient
            .query(mock.addr(), Duration::from_secs(2))
            .await
            .unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("origin timestamp mismatch"),
            "expected origin mismatch error, got: {msg}"
        );
    }

    /// Stratum 0 = Kiss-of-Death; must be rejected.
    #[tokio::test]
    async fn rejects_kiss_of_death() {
        let mock = MockServer::start(|req| {
            let t = unix_ms_to_ntp(1_700_000_000_000);
            NtpPacket {
                li: LI_NO_WARNING,
                vn: NTP_VERSION,
                mode: MODE_SERVER,
                stratum: 0, // KoD
                poll: 4,
                precision: -20,
                root_delay: 0,
                root_dispersion: 0,
                reference_id: 0,
                ref_timestamp: t,
                origin_timestamp: req.transmit_timestamp,
                receive_timestamp: t,
                transmit_timestamp: t,
            }
        })
        .await;

        let err = PacketNtpClient
            .query(mock.addr(), Duration::from_secs(2))
            .await
            .unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("Kiss-of-Death"),
            "expected KoD error, got: {msg}"
        );
    }

    /// LI=3 (alarm / unsynchronized) must be rejected.
    #[tokio::test]
    async fn rejects_leap_alarm() {
        let mock = MockServer::start(|req| {
            let t = unix_ms_to_ntp(1_700_000_000_000);
            NtpPacket {
                li: LI_ALARM_UNSYNCHRONIZED, // LI=3
                vn: NTP_VERSION,
                mode: MODE_SERVER,
                stratum: STRATUM_PRIMARY,
                poll: 4,
                precision: -20,
                root_delay: 0,
                root_dispersion: 0,
                reference_id: 0,
                ref_timestamp: t,
                origin_timestamp: req.transmit_timestamp,
                receive_timestamp: t,
                transmit_timestamp: t,
            }
        })
        .await;

        let err = PacketNtpClient
            .query(mock.addr(), Duration::from_secs(2))
            .await
            .unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("leap alarm"),
            "expected leap alarm error, got: {msg}"
        );
    }

    /// transmit_timestamp = 0 must be rejected.
    #[tokio::test]
    async fn rejects_zero_transmit() {
        let mock = MockServer::start(|req| {
            let t = unix_ms_to_ntp(1_700_000_000_000);
            NtpPacket {
                li: LI_NO_WARNING,
                vn: NTP_VERSION,
                mode: MODE_SERVER,
                stratum: STRATUM_PRIMARY,
                poll: 4,
                precision: -20,
                root_delay: 0,
                root_dispersion: 0,
                reference_id: 0,
                ref_timestamp: t,
                origin_timestamp: req.transmit_timestamp,
                receive_timestamp: t,
                transmit_timestamp: 0, // zero
            }
        })
        .await;

        let err = PacketNtpClient
            .query(mock.addr(), Duration::from_secs(2))
            .await
            .unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("zero"),
            "expected zero-transmit error, got: {msg}"
        );
    }

    /// delay < 0 must be rejected.
    ///
    /// To force delay < 0 set T3-T2 >> T4-T1 by giving the server an
    /// impossibly long "processing time" (T2=epoch, T3=epoch+100000s).
    #[tokio::test]
    async fn rejects_negative_delay() {
        let mock = MockServer::start(|req| {
            // T2 = 0 ms (Unix epoch), T3 = 100_000_000 ms (100000s later)
            // Since actual RTT ≈ 1ms: delay = RTT - (T3-T2) ≈ 1 - 100_000_000 < 0
            let t2_ntp = unix_ms_to_ntp(0);
            let t3_ntp = unix_ms_to_ntp(100_000_000);
            good_reply(req, t2_ntp, t3_ntp, 0, 0)
        })
        .await;

        let err = PacketNtpClient
            .query(mock.addr(), Duration::from_secs(2))
            .await
            .unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("negative delay"),
            "expected negative-delay error, got: {msg}"
        );
    }

    /// A server that never replies must time out.
    #[tokio::test]
    async fn times_out_on_silence() {
        let mock = MockServer::start_silent().await;

        let err = PacketNtpClient
            .query(mock.addr(), Duration::from_millis(100))
            .await
            .unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("timed out") || msg.contains("timeout"),
            "expected timeout error, got: {msg}"
        );
    }

    /// MockNtpClient works correctly for injection tests (used in P0-2+).
    #[tokio::test]
    async fn mock_client_returns_scripted_sample() {
        let now = Instant::now();
        let sample = NtpSample {
            server: "mock:123".to_string(),
            t1_unix_ms: 1_700_000_000_000,
            t2_unix_ms: 1_700_000_000_100,
            t3_unix_ms: 1_700_000_000_150,
            t4_unix_ms: 1_700_000_000_200,
            t1_instant: now,
            t4_instant: now + Duration::from_millis(200),
            offset_ms: 25,
            delay_ms: 150,
            root_delay_ms: 10,
            root_dispersion_ms: 5,
            precision_log2: -20,
            stratum: 2,
            leap: 0,
            reference_id: 0,
            poll: 4,
        };
        let mock = MockNtpClient::ok(sample.clone());
        let result = mock
            .query("mock:123", Duration::from_secs(1))
            .await
            .expect("mock should return Ok");
        assert_eq!(result.t2_unix_ms, sample.t2_unix_ms);
        assert_eq!(result.root_delay_ms, sample.root_delay_ms);
    }

    #[tokio::test]
    async fn mock_client_returns_error() {
        let mock = MockNtpClient::err("simulated failure");
        let err = mock
            .query("mock:123", Duration::from_secs(1))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("simulated failure"));
    }
}
