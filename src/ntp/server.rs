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
use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};
use tokio::net::UdpSocket;
use tracing::{debug, error, info, warn};

/// Maximum packet we will accept from a client. NTPv4 is 48 bytes but
/// control messages can be longer; we cap defensively to avoid letting
/// peers DoS us with jumbo datagrams.
pub const DEFAULT_MAX_PACKET_SIZE: usize = 1024;

/// Default per-IP rate limit for the UDP NTP server.
/// Limits each source IP to this many requests per second to reduce
/// the amplification risk from spoofed-source attacks.
pub const DEFAULT_UDP_RATE_LIMIT: u32 = 100;

/// Fixed-window per-IP rate limiter for UDP NTP requests.
///
/// Each source IP is allowed up to `limit_per_second` packets within
/// any 1-second window. The window is reset when the first packet in a
/// new second arrives. Entries older than 1 second are cleaned up lazily.
///
/// The mutex is only held for the duration of a hash-map lookup and
/// increment — no I/O happens under the lock.
struct UdpRateLimiter {
    map: parking_lot::Mutex<HashMap<IpAddr, (u32, Instant)>>,
    limit_per_second: u32,
}

impl UdpRateLimiter {
    fn new(limit_per_second: u32) -> Self {
        Self {
            map: parking_lot::Mutex::new(HashMap::new()),
            limit_per_second,
        }
    }

    /// Returns `true` if the request from `ip` should be allowed,
    /// `false` if it exceeds the rate limit.
    ///
    /// `limit_per_second == 0` means unlimited — every request is allowed.
    fn allow(&self, ip: IpAddr) -> bool {
        if self.limit_per_second == 0 {
            return true; // rate limiting disabled
        }

        let now = Instant::now();
        let mut map = self.map.lock();

        match map.get_mut(&ip) {
            Some((count, window_start)) => {
                if now.duration_since(*window_start) >= Duration::from_secs(1) {
                    // New window — reset counter.
                    *count = 1;
                    *window_start = now;
                    true
                } else if *count < self.limit_per_second {
                    *count += 1;
                    true
                } else {
                    false
                }
            }
            None => {
                map.insert(ip, (1, now));
                // Lazy cleanup: remove stale entries whenever the map grows large.
                if map.len() > 10_000 {
                    map.retain(|_, (_, window_start)| {
                        now.duration_since(*window_start) < Duration::from_secs(2)
                    });
                }
                true
            }
        }
    }
}

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
    /// RTT of the most recent upstream NTP sync in milliseconds.
    /// Shared from the sync loop so we can report a non-zero
    /// `root_delay` to downstream clients (RFC 5905 §7.3).
    last_rtt_ms: Arc<AtomicU64>,
    rate_limiter: UdpRateLimiter,
}

impl NtpServer {
    pub fn new(
        addr: SocketAddr,
        timebase: TimeBase,
        metrics: Arc<Metrics>,
        last_rtt_ms: Arc<AtomicU64>,
    ) -> Self {
        Self {
            addr,
            timebase,
            metrics,
            max_packet_size: DEFAULT_MAX_PACKET_SIZE,
            last_rtt_ms,
            rate_limiter: UdpRateLimiter::new(DEFAULT_UDP_RATE_LIMIT),
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
        self.metrics.ntp_udp_server_requests_total.inc();

        // Per-IP rate limiting — drop silently (no amplification response).
        if !self.rate_limiter.allow(peer.ip()) {
            debug!(peer = %peer, "NTP request rate-limited (dropped)");
            self.metrics.ntp_udp_server_errors_total.inc();
            return;
        }

        let request = match parse_packet(bytes) {
            Ok(p) => p,
            Err(e) => {
                // Short / malformed / non-client packets are normal
                // background noise; only warn when unusual.
                debug!(peer = %peer, error = %e, "dropping NTP packet");
                self.metrics.ntp_udp_server_errors_total.inc();
                return;
            }
        };

        // Capture receive timestamp as early as possible.
        let receive_ntp = unix_ms_to_ntp(self.timebase.now_ms().unwrap_or_else(system_unix_ms));

        let upstream_rtt_ms = self.last_rtt_ms.load(Ordering::Relaxed);
        let response = build_response(&self.timebase, &request, receive_ntp, upstream_rtt_ms);
        let wire = serialize_packet(&response);

        // Capture transmit timestamp as late as possible (just before send).
        let transmit_ntp = unix_ms_to_ntp(self.timebase.now_ms().unwrap_or_else(system_unix_ms));
        let mut wire = wire;
        write_transmit(&mut wire, transmit_ntp);

        match socket.send_to(&wire, peer).await {
            Ok(_) => {
                self.metrics.ntp_udp_server_responses_total.inc();
                if !self.timebase.has_synced() {
                    self.metrics.ntp_udp_server_unsynced_responses_total.inc();
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
                self.metrics.ntp_udp_server_errors_total.inc();
            }
        }
    }
}

/// Convert milliseconds to NTP short format (32-bit fixed point:
/// 16 bits seconds + 16 bits fractions of a second).
/// `ms=0` → `0`; values ≥ 65535 ms are capped at the max short value.
fn ms_to_ntp_short(ms: u64) -> u32 {
    // NTP short format: seconds × 2^16 + fractions × 2^16
    // For sub-second values: ms/1000 * 65536 = ms * 65536 / 1000
    let ntp_units = ms.saturating_mul(65536) / 1000;
    ntp_units.min(u32::MAX as u64) as u32
}

fn build_response(
    timebase: &TimeBase,
    request: &NtpPacket,
    receive_ntp: u64,
    upstream_rtt_ms: u64,
) -> NtpPacket {
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

    // root_delay = RTT to our upstream NTP server, in NTP short format.
    // When synced, propagate the measured RTT so downstream clients can
    // budget their uncertainty correctly (RFC 5905 §7.3).
    // root_dispersion is left at 0; we do not track upstream dispersion.
    let root_delay = if synced {
        ms_to_ntp_short(upstream_rtt_ms)
    } else {
        0
    };

    NtpPacket {
        li,
        vn: NTP_VERSION,
        mode: 4, // server
        stratum,
        poll: request.poll,
        // Precision: 2^-10 ≈ 1 ms. The actual clock is read from
        // Rust's Instant (CLOCK_MONOTONIC on Linux, ns resolution)
        // but our time is derived from an upstream NTP sync at
        // ~30 s intervals, so the real-world accuracy is bounded
        // by the sync interval and the upstream's stratum, not by
        // our local counter. 1 ms is the honest lie.
        precision: -10,
        root_delay,
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
    use std::sync::atomic::AtomicU64;
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
            offset_ms: 0,
            t1_client_send_ms: 0,
            t2_server_recv_ms: 0,
            t3_server_send_ms: 0,
            t4_client_recv_ms: 0,
        });
        tb
    }

    fn unsynced_timebase() -> TimeBase {
        let cache = Arc::new(TimeCache::new("ok".into(), "ok".into()));
        TimeBase::new(false).with_cache(cache)
    }

    fn zero_rtt() -> Arc<AtomicU64> {
        Arc::new(AtomicU64::new(0))
    }

    #[tokio::test]
    async fn server_responds_to_client_request() {
        let metrics = Arc::new(Metrics::new());
        let tb = synced_timebase();
        let rtt = zero_rtt();
        let _server = NtpServer::new(
            "127.0.0.1:0".parse().unwrap(),
            tb.clone(),
            metrics.clone(),
            rtt,
        )
        .with_max_packet_size(128);

        // Bind a socket to a random port, then run the server on it.
        let probe = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let addr = probe.local_addr().unwrap();
        drop(probe);

        let server_handle = tokio::spawn(async move {
            // Re-bind on the same port we just probed.
            let s = NtpServer::new(addr, tb, metrics, zero_rtt());
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
            let s = NtpServer::new(addr, tb_clone, metrics_clone, zero_rtt());
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
        let r = build_response(&tb, &req, 0x1111_2222_3333_4444, 0);
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
        let r = build_response(&tb, &req, 0, 0);
        assert_eq!(r.li, LI_ALARM_UNSYNCHRONIZED);
        assert_eq!(r.stratum, STRATUM_UNSYNCHRONIZED);
        assert_eq!(r.reference_id, 0);
        assert_eq!(r.ref_timestamp, 0);
    }

    #[test]
    fn build_response_synced_includes_root_delay() {
        let tb = synced_timebase();
        let req = NtpPacket::new(0, 4, 3);
        // 10ms RTT → root_delay = 10 * 65536 / 1000 = 655
        let r = build_response(&tb, &req, 0, 10);
        assert_eq!(r.root_delay, 655);
    }

    #[test]
    fn build_response_unsynced_root_delay_is_zero() {
        let tb = unsynced_timebase();
        let req = NtpPacket::new(0, 4, 3);
        let r = build_response(&tb, &req, 0, 50);
        assert_eq!(r.root_delay, 0, "unsynced path must always report 0");
    }

    #[test]
    fn ms_to_ntp_short_values() {
        assert_eq!(ms_to_ntp_short(0), 0);
        // 10ms → 655
        assert_eq!(ms_to_ntp_short(10), 655);
        // 1000ms (1 sec) → 65536
        assert_eq!(ms_to_ntp_short(1000), 65536);
    }

    // ── UdpRateLimiter tests ─────────────────────────────────────────────────

    #[test]
    fn rate_limiter_allows_within_limit() {
        let rl = UdpRateLimiter::new(5);
        let ip: IpAddr = "1.2.3.4".parse().unwrap();
        for _ in 0..5 {
            assert!(rl.allow(ip), "first 5 requests should be allowed");
        }
    }

    #[test]
    fn rate_limiter_blocks_over_limit() {
        let rl = UdpRateLimiter::new(3);
        let ip: IpAddr = "1.2.3.4".parse().unwrap();
        // First 3 pass
        for _ in 0..3 {
            rl.allow(ip);
        }
        // 4th should be blocked
        assert!(!rl.allow(ip), "4th request should be blocked");
    }

    #[test]
    fn rate_limiter_different_ips_are_independent() {
        let rl = UdpRateLimiter::new(1);
        let ip1: IpAddr = "1.2.3.4".parse().unwrap();
        let ip2: IpAddr = "5.6.7.8".parse().unwrap();
        assert!(rl.allow(ip1));
        // ip1 is now at limit
        assert!(!rl.allow(ip1));
        // ip2 is a fresh entry
        assert!(rl.allow(ip2));
    }

    #[test]
    fn rate_limiter_zero_limit_means_unlimited() {
        let rl = UdpRateLimiter::new(0);
        let ip: IpAddr = "1.2.3.4".parse().unwrap();
        // limit=0 → disabled → all requests allowed
        for _ in 0..100 {
            assert!(rl.allow(ip), "limit=0 should allow everything");
        }
    }
}
