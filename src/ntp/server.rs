//! UDP NTP/SNTP server.
//!
//! Listens on a configurable UDP address (default `0.0.0.0:123`) and
//! responds to NTP client (Mode 3) requests with the time held in
//! [`crate::timebase::TimeBase`].
//!
//! Behavior:
//! - When `TimeBase::has_synced()` is `true`, replies with LI=0,
//!   Stratum=2, Reference ID = `"LOCL"`, receive_timestamp set at packet
//!   intake, transmit_timestamp set just before send, and origin_timestamp
//!   echoed from the client's transmit_timestamp.
//! - When not synced, replies with LI=3, Stratum=16, and the system clock
//!   (so the client at least gets something monotonic) — but the
//!   unsynced kiss code signals "do not trust this server".
//!
//! The server shares the same Tokio runtime as the HTTP server, the
//! NTP sync loop, and the probe loop.

use super::protocol::{
    LI_ALARM_UNSYNCHRONIZED, LI_NO_WARNING, NTP_VERSION, NtpPacket, STRATUM_PRIMARY,
    STRATUM_UNSYNCHRONIZED, parse_packet, serialize_packet, system_unix_ms, unix_ms_to_ntp,
};
use crate::metrics::Metrics;
use crate::timebase::TimeBase;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::net::UdpSocket;
use tracing::{debug, error, info, warn};

/// Maximum packet we will accept from a client. NTPv4 is 48 bytes but
/// control messages can be longer; we cap defensively to avoid letting
/// peers DoS us with jumbo datagrams.
pub const DEFAULT_MAX_PACKET_SIZE: usize = 1024;

/// Reference ID for a Stratum-2 server that is "local": RFC 5905 §7.3
/// allows 4 ASCII chars (KISS codes) for Stratum >= 2.
const REFERENCE_ID_LOCAL: u32 = u32::from_be_bytes(*b"LOCL");

/// The NTP server. Cheap to construct; call [`NtpServer::run`] to serve
/// forever (or until the runtime shuts down).
pub struct NtpServer {
    addr: SocketAddr,
    timebase: TimeBase,
    metrics: Arc<Metrics>,
    max_packet_size: usize,
}

impl NtpServer {
    pub fn new(addr: SocketAddr, timebase: TimeBase, metrics: Arc<Metrics>) -> Self {
        Self {
            addr,
            timebase,
            metrics,
            max_packet_size: DEFAULT_MAX_PACKET_SIZE,
        }
    }

    pub fn with_max_packet_size(mut self, max: usize) -> Self {
        self.max_packet_size = max.max(48);
        self
    }

    /// Bind the UDP socket and serve until the process exits.
    ///
    /// Returns only on fatal bind errors. Per-packet errors are logged
    /// and counted in metrics.
    pub async fn run(self) -> anyhow::Result<()> {
        let socket = UdpSocket::bind(self.addr).await?;
        let local = socket.local_addr().ok();
        info!(
            addr = %self.addr,
            local = ?local,
            "NTP server listening on UDP"
        );
        if self.addr.port() < 1024 {
            warn!(
                port = self.addr.port(),
                "NTP server bound to a privileged port; requires CAP_NET_BIND_SERVICE or root"
            );
        }

        let mut buf = vec![0u8; self.max_packet_size];
        loop {
            match socket.recv_from(&mut buf).await {
                Ok((len, peer)) => self.handle_request(&socket, peer, &buf[..len]).await,
                Err(e) => {
                    error!(error = %e, "NTP socket recv error");
                    // Brief pause to avoid a tight error loop if the socket
                    // has been closed underneath us.
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
            }
        }
    }

    async fn handle_request(&self, socket: &UdpSocket, peer: SocketAddr, bytes: &[u8]) {
        self.metrics.ntp_server_requests_total.inc();

        let request = match parse_packet(bytes) {
            Ok(p) => p,
            Err(e) => {
                // Short / malformed / non-client packets are normal
                // background noise; only warn when unusual.
                debug!(peer = %peer, error = %e, "dropping NTP packet");
                self.metrics.ntp_server_errors_total.inc();
                return;
            }
        };

        // Capture receive timestamp as early as possible.
        let receive_ntp = unix_ms_to_ntp(self.timebase.now_ms().unwrap_or_else(system_unix_ms));

        let response = build_response(&self.timebase, &request, receive_ntp);
        let wire = serialize_packet(&response);

        // Capture transmit timestamp as late as possible (just before send).
        let transmit_ntp = unix_ms_to_ntp(self.timebase.now_ms().unwrap_or_else(system_unix_ms));
        let mut wire = wire;
        write_transmit(&mut wire, transmit_ntp);

        match socket.send_to(&wire, peer).await {
            Ok(_) => {
                self.metrics.ntp_server_responses_total.inc();
                if !self.timebase.has_synced() {
                    self.metrics.ntp_server_unsynced_responses_total.inc();
                }
                debug!(
                    peer = %peer,
                    stratum = response.stratum,
                    li = response.li,
                    "NTP response sent"
                );
            }
            Err(e) => {
                error!(peer = %peer, error = %e, "failed to send NTP response");
                self.metrics.ntp_server_errors_total.inc();
            }
        }
    }
}

fn build_response(timebase: &TimeBase, request: &NtpPacket, receive_ntp: u64) -> NtpPacket {
    let synced = timebase.has_synced();

    let (li, stratum) = if synced {
        (LI_NO_WARNING, STRATUM_PRIMARY + 1) // Stratum 2: secondary server
    } else {
        (LI_ALARM_UNSYNCHRONIZED, STRATUM_UNSYNCHRONIZED)
    };

    // Reference ID:
    //   - Stratum 1 (primary) encodes a 4-char ASCII clock source.
    //   - Stratum 2-15 (secondary) encodes the upstream IPv4 (or "LOCL").
    //   - We advertise "LOCL" — a generic "local clock" kiss code, which
    //     is acceptable for a Stratum-2 server and avoids hard-coding an
    //     upstream IP that may not match reality.
    let reference_id = if synced { REFERENCE_ID_LOCAL } else { 0 };

    // Reference timestamp = the time we last received a clean sync.
    // For the unsynced path, set it to 0 (RFC 5905 §7.3).
    let ref_timestamp = if synced { receive_ntp } else { 0 };

    // Stratum-2 server delay/dispersion in NTP short format: 0 means
    // "unmeasured". A real implementation would track these; we are
    // honest and report 0.
    NtpPacket {
        li,
        vn: NTP_VERSION,
        mode: 4, // server
        stratum,
        poll: request.poll,
        // Precision: 2^-20 ≈ 1 microsecond. The actual jitter is higher
        // than this but we don't have a good signal yet.
        precision: -20,
        root_delay: 0,
        root_dispersion: 0,
        reference_id,
        ref_timestamp,
        // RFC 5905 §7.3: "Origin Timestamp = request Transmit Timestamp".
        origin_timestamp: request.transmit_timestamp,
        receive_timestamp: receive_ntp,
        // transmit_timestamp is filled in by the caller (handle_request)
        // after serialize_packet; this value is overwritten on the wire.
        transmit_timestamp: receive_ntp,
    }
}

/// Overwrite the 8-byte Transmit Timestamp field in an already-serialized
/// packet. We do this so the transmit timestamp can be captured as late
/// as possible (just before `send_to`).
fn write_transmit(buf: &mut [u8], transmit_ntp: u64) {
    if buf.len() < 48 {
        return;
    }
    buf[40..48].copy_from_slice(&transmit_ntp.to_be_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metrics::Metrics;
    use crate::performance::TimeCache;
    use crate::timebase::TimeBase;
    use std::time::{Duration, Instant};
    use tokio::net::UdpSocket;
    use tokio::time::timeout;

    fn synced_timebase() -> TimeBase {
        // Use the time_cache to satisfy TimeBase::with_cache requirements.
        let cache = Arc::new(TimeCache::new("ok".into(), "ok".into()));
        let tb = TimeBase::new(false).with_cache(cache);
        tb.update(&crate::ntp::SyncResult {
            epoch_ms: 1_704_067_200_000,
            server: "test:123".into(),
            rtt: Duration::from_millis(10),
            instant: Instant::now(),
        });
        tb
    }

    fn unsynced_timebase() -> TimeBase {
        let cache = Arc::new(TimeCache::new("ok".into(), "ok".into()));
        TimeBase::new(false).with_cache(cache)
    }

    #[tokio::test]
    async fn server_responds_to_client_request() {
        let metrics = Arc::new(Metrics::new());
        let tb = synced_timebase();
        let _server = NtpServer::new("127.0.0.1:0".parse().unwrap(), tb.clone(), metrics.clone())
            .with_max_packet_size(128);

        // Bind a socket to a random port, then run the server on it.
        let probe = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let addr = probe.local_addr().unwrap();
        drop(probe);

        let server_handle = tokio::spawn(async move {
            // Re-bind on the same port we just probed.
            let s = NtpServer::new(addr, tb, metrics);
            let _ = s.run().await;
        });

        // Give the server a moment to bind.
        tokio::time::sleep(Duration::from_millis(100)).await;

        // Client: send a Mode 3 request and read the response.
        let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let mut req = [0u8; 48];
        req[0] = 0x23; // LI=0 VN=4 Mode=3
        req[1] = 0; // Stratum
        req[2] = 4; // Poll
        req[3] = 0xEC; // Precision = -20
        // Origin timestamp in the request is the client's "transmit" ts.
        // The server will echo it back into its response origin_timestamp.
        req[40..48].copy_from_slice(&0x0123_4567_89AB_CDEF_u64.to_be_bytes());

        client.send_to(&req, addr).await.unwrap();

        let mut resp = [0u8; 48];
        let n = timeout(Duration::from_secs(2), client.recv_from(&mut resp))
            .await
            .expect("timed out waiting for NTP response")
            .unwrap();

        assert_eq!(n.0, 48);
        let b0 = resp[0];
        assert_eq!(b0 >> 6 & 0x03, LI_NO_WARNING, "synced → LI=0");
        assert_eq!(b0 >> 3 & 0x07, 4, "Version=4");
        assert_eq!(b0 & 0x07, 4, "Mode=4 (server)");
        assert_eq!(resp[1], 2, "Stratum=2 (secondary)");

        // Reference ID should be "LOCL" when synced.
        assert_eq!(&resp[12..16], b"LOCL");

        // Origin timestamp should echo the client's transmit timestamp.
        assert_eq!(&resp[24..32], &req[40..48]);

        // Receive / Transmit timestamps should be non-zero.
        assert_ne!(&resp[32..40], &[0u8; 8]);
        assert_ne!(&resp[40..48], &[0u8; 8]);

        server_handle.abort();
    }

    #[tokio::test]
    async fn server_responds_with_unsynced_when_timebase_empty() {
        let metrics = Arc::new(Metrics::new());
        let tb = unsynced_timebase();
        let probe = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let addr = probe.local_addr().unwrap();
        drop(probe);

        let metrics_clone = metrics.clone();
        let tb_clone = tb.clone();
        let server_handle = tokio::spawn(async move {
            let s = NtpServer::new(addr, tb_clone, metrics_clone);
            let _ = s.run().await;
        });

        tokio::time::sleep(Duration::from_millis(100)).await;

        let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let req = [0x23u8; 48];
        client.send_to(&req, addr).await.unwrap();

        let mut resp = [0u8; 48];
        let n = timeout(Duration::from_secs(2), client.recv_from(&mut resp))
            .await
            .expect("timed out")
            .unwrap();
        assert_eq!(n.0, 48);

        let b0 = resp[0];
        assert_eq!(b0 >> 6 & 0x03, LI_ALARM_UNSYNCHRONIZED, "unsynced → LI=3");
        assert_eq!(resp[1], STRATUM_UNSYNCHRONIZED, "unsynced → Stratum=16");

        server_handle.abort();
    }

    #[test]
    fn build_response_synced_uses_stratum_2() {
        let tb = synced_timebase();
        let mut req = NtpPacket::new(0, 4, 3);
        req.transmit_timestamp = 0xAAAA_BBBB_CCCC_DDDD;
        let r = build_response(&tb, &req, 0x1111_2222_3333_4444);
        assert_eq!(r.li, 0);
        assert_eq!(r.stratum, 2);
        assert_eq!(r.mode, 4);
        assert_eq!(r.origin_timestamp, 0xAAAA_BBBB_CCCC_DDDD);
        assert_eq!(r.reference_id, REFERENCE_ID_LOCAL);
    }

    #[test]
    fn build_response_unsynced_uses_stratum_16() {
        let tb = unsynced_timebase();
        let req = NtpPacket::new(0, 4, 3);
        let r = build_response(&tb, &req, 0);
        assert_eq!(r.li, LI_ALARM_UNSYNCHRONIZED);
        assert_eq!(r.stratum, STRATUM_UNSYNCHRONIZED);
        assert_eq!(r.reference_id, 0);
        assert_eq!(r.ref_timestamp, 0);
    }
}
