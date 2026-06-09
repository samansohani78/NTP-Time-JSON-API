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
use super::sync::SyncQuality;
use crate::metrics::Metrics;
use crate::timebase::TimeBase;
use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
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

/// Reference ID advertised when a manual time override is active.
const REFERENCE_ID_MANUAL: u32 = u32::from_be_bytes(*b"MANU");

/// The NTP server. Cheap to construct; call [`NtpServer::run`] to serve
/// forever (or until the runtime shuts down).
pub struct NtpServer {
    addr: SocketAddr,
    timebase: TimeBase,
    metrics: Arc<Metrics>,
    max_packet_size: usize,
    /// Full quality snapshot from the most recent upstream NTP sync.
    /// Used to compute honest `root_delay` / `root_dispersion` per
    /// RFC 5905 §11.2.
    last_sync_quality: Arc<parking_lot::RwLock<Option<SyncQuality>>>,
    /// Hard ceiling on advertised `root_dispersion` (ms).
    max_root_dispersion_ms: u64,
    rate_limiter: UdpRateLimiter,
    /// Base root_dispersion (ms) when a manual override is active.
    /// Grows with override age at RFC 5905 PHI rate (15 ms/s).  Default: 1000 ms.
    manual_dispersion_ms: u64,
}

impl NtpServer {
    pub fn new(
        addr: SocketAddr,
        timebase: TimeBase,
        metrics: Arc<Metrics>,
        last_sync_quality: Arc<parking_lot::RwLock<Option<SyncQuality>>>,
        max_root_dispersion_ms: u64,
    ) -> Self {
        Self {
            addr,
            timebase,
            metrics,
            max_packet_size: DEFAULT_MAX_PACKET_SIZE,
            last_sync_quality,
            max_root_dispersion_ms,
            rate_limiter: UdpRateLimiter::new(DEFAULT_UDP_RATE_LIMIT),
            manual_dispersion_ms: 1000,
        }
    }

    pub fn with_max_packet_size(mut self, max: usize) -> Self {
        self.max_packet_size = max.max(48);
        self
    }

    pub fn with_manual_dispersion_ms(mut self, ms: u64) -> Self {
        self.manual_dispersion_ms = ms;
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
        self.serve_loop(socket).await
    }

    /// Like [`run`] but notifies the caller of the actual bound address via
    /// `ready_tx` once the socket is open.  Useful when `self.addr.port() == 0`
    /// (OS-assigned ephemeral port) and the caller needs to know the real port,
    /// e.g. integration tests.
    pub async fn run_with_ready(
        self,
        ready_tx: tokio::sync::oneshot::Sender<SocketAddr>,
    ) -> anyhow::Result<()> {
        let socket = UdpSocket::bind(self.addr).await?;
        let local = socket.local_addr()?;
        info!(addr = %local, "NTP server listening on UDP");
        if local.port() < 1024 {
            warn!(
                port = local.port(),
                "NTP server bound to a privileged port; requires CAP_NET_BIND_SERVICE or root"
            );
        }
        let _ = ready_tx.send(local);
        self.serve_loop(socket).await
    }

    async fn serve_loop(self, socket: UdpSocket) -> anyhow::Result<()> {
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

        // Clone the sync quality snapshot (releases the lock immediately).
        let quality_snapshot = self.last_sync_quality.read().clone();

        // Pre-compute advertised dispersion_ms (f64) for metric emission.
        // Only set when synced and quality data is available.
        let advertised_dispersion_ms: Option<f64> = if self.timebase.has_synced() {
            quality_snapshot
                .as_ref()
                .map(|q| compute_dispersion_ms(q, self.max_root_dispersion_ms))
        } else {
            None
        };

        let response = build_response(
            &self.timebase,
            &request,
            receive_ntp,
            quality_snapshot.as_ref(),
            self.max_root_dispersion_ms,
            self.manual_dispersion_ms,
        );
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
                // Update the advertised root_dispersion gauge when synced.
                if let Some(disp_ms) = advertised_dispersion_ms {
                    self.metrics
                        .ntp_udp_server_root_dispersion_seconds
                        .set(disp_ms / 1000.0);
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

/// Like `ms_to_ntp_short` but accepts f64 milliseconds.
///
/// Used for dispersion so that sub-millisecond contributions (e.g.
/// PHI × age, precision) are preserved in the NTP short-format output
/// and not truncated by an early integer cast.
fn ms_to_ntp_short_f64(ms: f64) -> u32 {
    let ntp_units = (ms * 65536.0 / 1000.0).max(0.0);
    ntp_units.min(u32::MAX as f64) as u32
}

/// Compute the `root_dispersion` to advertise, in f64 milliseconds, clamped
/// to `[0, max_ms]`. Delegates the core formula to
/// [`SyncQuality::compute_dispersion_ms`] (RFC 5905 §11.2).
fn compute_dispersion_ms(quality: &SyncQuality, max_ms: u64) -> f64 {
    quality.compute_dispersion_ms().min(max_ms as f64)
}

/// Compute the `root_delay` to advertise, in milliseconds.
///
/// Per RFC 5905 §11.2: `root_delay = upstream.root_delay + local_RTT`.
/// As a Stratum-2 relay we include both the upstream's path delay and
/// our own measured RTT to that upstream.
fn compute_root_delay_ms(quality: &SyncQuality) -> u64 {
    (quality.upstream_root_delay_ms as u64).saturating_add(quality.measured_rtt_ms)
}

fn build_response(
    timebase: &TimeBase,
    request: &NtpPacket,
    receive_ntp: u64,
    quality: Option<&SyncQuality>,
    max_root_dispersion_ms: u64,
    manual_dispersion_ms: u64,
) -> NtpPacket {
    // ── Manual override path ──────────────────────────────────────────────────
    if timebase.is_manual_active() {
        // root_dispersion grows with age: base + PHI * age_secs (PHI = 15 µs/s = 0.015 ms/s, RFC 5905 §11.2)
        let age_ms = timebase.manual_age_ms();
        let phi_ms = age_ms.saturating_mul(15) / 1_000_000;
        let disp_ms = manual_dispersion_ms
            .saturating_add(phi_ms)
            .min(max_root_dispersion_ms);
        return NtpPacket {
            li: LI_NO_WARNING,
            vn: NTP_VERSION,
            mode: 4,
            stratum: STRATUM_PRIMARY + 1, // Stratum 2
            poll: request.poll,
            precision: -10,
            root_delay: 0,
            root_dispersion: ms_to_ntp_short(disp_ms),
            reference_id: REFERENCE_ID_MANUAL,
            ref_timestamp: receive_ntp,
            origin_timestamp: request.transmit_timestamp,
            receive_timestamp: receive_ntp,
            transmit_timestamp: receive_ntp,
        };
    }

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

    // root_delay and root_dispersion: use quality data when available.
    // If synced but quality is not yet populated (race at startup),
    // both fields default to 0.
    let (root_delay, root_dispersion) = if synced {
        match quality {
            Some(q) => (
                ms_to_ntp_short(compute_root_delay_ms(q)),
                ms_to_ntp_short_f64(compute_dispersion_ms(q, max_root_dispersion_ms)),
            ),
            None => (0, 0),
        }
    } else {
        (0, 0)
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
        root_dispersion,
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
    use crate::ntp::sync::SyncQuality;
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
            root_delay_ms: 0,
            root_dispersion_ms: 0,
            stratum: 1,
            leap: 0,
            precision_log2: 0,
            reference_id: 0,
            timing_source: crate::ntp::selection::TimingSource::Estimated,
        });
        tb
    }

    fn unsynced_timebase() -> TimeBase {
        let cache = Arc::new(TimeCache::new("ok".into(), "ok".into()));
        TimeBase::new(false).with_cache(cache)
    }

    /// Build a `SyncQuality` for use in tests.
    fn make_sync_quality(
        measured_rtt_ms: u64,
        upstream_root_delay_ms: u32,
        upstream_root_dispersion_ms: u32,
    ) -> SyncQuality {
        SyncQuality {
            upstream_root_delay_ms,
            upstream_root_dispersion_ms,
            precision_log2: -20,
            stratum: 1,
            leap: 0,
            measured_rtt_ms,
            jitter_ms: 0,
            offset_ms: 0,
            last_sync_instant: Instant::now(),
            selected_server: "test.ntp.org:123".into(),
        }
    }

    /// Wrap a `SyncQuality` in the `Arc<RwLock<Option<_>>>` that `NtpServer` holds.
    fn quality_arc(q: SyncQuality) -> Arc<parking_lot::RwLock<Option<SyncQuality>>> {
        Arc::new(parking_lot::RwLock::new(Some(q)))
    }

    fn no_quality() -> Arc<parking_lot::RwLock<Option<SyncQuality>>> {
        Arc::new(parking_lot::RwLock::new(None))
    }

    // ── build_response unit tests ──────────────────────────────────────────

    #[test]
    fn build_response_synced_uses_stratum_2() {
        let tb = synced_timebase();
        let mut req = NtpPacket::new(0, 4, 3);
        req.transmit_timestamp = 0xAAAA_BBBB_CCCC_DDDD;
        let r = build_response(&tb, &req, 0x1111_2222_3333_4444, None, 16_000, 1000);
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
        let r = build_response(&tb, &req, 0, None, 16_000, 1000);
        assert_eq!(r.li, LI_ALARM_UNSYNCHRONIZED);
        assert_eq!(r.stratum, STRATUM_UNSYNCHRONIZED);
        assert_eq!(r.reference_id, 0);
        assert_eq!(r.ref_timestamp, 0);
    }

    #[test]
    fn build_response_synced_includes_root_delay() {
        let tb = synced_timebase();
        let req = NtpPacket::new(0, 4, 3);
        // upstream_root_delay_ms=0, measured_rtt_ms=10
        // → root_delay = ms_to_ntp_short(10) = 655
        let q = make_sync_quality(10, 0, 0);
        let r = build_response(&tb, &req, 0, Some(&q), 16_000, 1000);
        assert_eq!(r.root_delay, 655);
    }

    #[test]
    fn build_response_unsynced_root_delay_is_zero() {
        let tb = unsynced_timebase();
        let req = NtpPacket::new(0, 4, 3);
        let q = make_sync_quality(50, 10, 5);
        let r = build_response(&tb, &req, 0, Some(&q), 16_000, 1000);
        assert_eq!(r.root_delay, 0, "unsynced path must always report 0");
        assert_eq!(r.root_dispersion, 0, "unsynced path must always report 0");
    }

    #[test]
    fn root_dispersion_nonzero_when_synced() {
        let tb = synced_timebase();
        let req = NtpPacket::new(0, 4, 3);
        // upstream_dispersion=5ms, rtt=10ms → dispersion >= 5 + 5 = 10ms
        let q = make_sync_quality(10, 0, 5);
        let r = build_response(&tb, &req, 0, Some(&q), 16_000, 1000);
        assert!(
            r.root_dispersion > 0,
            "synced response with quality must have root_dispersion > 0"
        );
    }

    #[test]
    fn root_dispersion_grows_with_age() {
        let tb = synced_timebase();
        let req = NtpPacket::new(0, 4, 3);

        let q_fresh = make_sync_quality(10, 0, 5);
        let r_fresh = build_response(&tb, &req, 0, Some(&q_fresh), 16_000, 1000);

        // Simulate a sync that is 10 seconds old.
        let q_old = SyncQuality {
            last_sync_instant: Instant::now() - Duration::from_secs(10),
            ..make_sync_quality(10, 0, 5)
        };
        let r_old = build_response(&tb, &req, 0, Some(&q_old), 16_000, 1000);

        assert!(
            r_old.root_dispersion > r_fresh.root_dispersion,
            "older sync ({}  NTP-short) must have larger dispersion than fresh sync ({} NTP-short)",
            r_old.root_dispersion,
            r_fresh.root_dispersion
        );
    }

    #[test]
    fn root_dispersion_clamps_at_max() {
        let tb = synced_timebase();
        let req = NtpPacket::new(0, 4, 3);
        // Very old sync will produce huge drift; clamp at 1000 ms.
        let q = SyncQuality {
            last_sync_instant: Instant::now() - Duration::from_secs(1_000_000),
            ..make_sync_quality(10, 0, 0)
        };
        let r = build_response(&tb, &req, 0, Some(&q), 1_000, 1000);
        let expected = ms_to_ntp_short(1_000);
        assert_eq!(
            r.root_dispersion, expected,
            "dispersion must be clamped to max_root_dispersion_ms"
        );
    }

    #[test]
    fn root_delay_includes_upstream() {
        let tb = synced_timebase();
        let req = NtpPacket::new(0, 4, 3);
        // upstream_root_delay=50ms, measured_rtt=10ms → total=60ms
        let q = make_sync_quality(10, 50, 0);
        let r = build_response(&tb, &req, 0, Some(&q), 16_000, 1000);
        let expected = ms_to_ntp_short(60);
        assert_eq!(
            r.root_delay, expected,
            "root_delay must include upstream root_delay + local RTT"
        );
        assert!(
            r.root_delay >= ms_to_ntp_short(50),
            "root_delay must be >= upstream_root_delay"
        );
    }

    #[test]
    fn ms_to_ntp_short_values() {
        assert_eq!(ms_to_ntp_short(0), 0);
        // 10ms → 655
        assert_eq!(ms_to_ntp_short(10), 655);
        // 1000ms (1 sec) → 65536
        assert_eq!(ms_to_ntp_short(1000), 65536);
    }

    #[test]
    fn manual_dispersion_phi_rate_is_15_micros_per_second() {
        // RFC 5905 §11.2: PHI = 15 µs/s = 0.015 ms/s.
        // After 1000 s (= 1_000_000 ms) the PHI contribution must be 15 ms.
        let age_ms: u64 = 1_000_000;
        let phi_ms = age_ms.saturating_mul(15) / 1_000_000;
        assert_eq!(
            phi_ms, 15,
            "1000 s PHI contribution must be 15 ms, not 15_000 ms"
        );

        // After 100 s: 0.015 ms/s × 100 s = 1.5 ms → floors to 1 ms in integer.
        let phi_100s = 100_000u64.saturating_mul(15) / 1_000_000;
        assert_eq!(phi_100s, 1);

        // After 60 s: 0.015 × 60 = 0.9 ms → floors to 0.
        let phi_1min = 60_000u64.saturating_mul(15) / 1_000_000;
        assert_eq!(phi_1min, 0);
    }

    // ── Live UDP loopback tests ────────────────────────────────────────────

    #[tokio::test]
    async fn server_responds_to_client_request() {
        let metrics = Arc::new(Metrics::new());
        let tb = synced_timebase();
        let quality = quality_arc(make_sync_quality(10, 5, 3));

        let probe = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let addr = probe.local_addr().unwrap();
        drop(probe);

        let tb_clone = tb.clone();
        let metrics_clone = metrics.clone();
        let quality_clone = quality.clone();
        let server_handle = tokio::spawn(async move {
            let s = NtpServer::new(addr, tb_clone, metrics_clone, quality_clone, 16_000);
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

        // root_delay (bytes 4-7) should be non-zero (upstream=5 + rtt=10 = 15ms)
        let root_delay = u32::from_be_bytes(resp[4..8].try_into().unwrap());
        assert!(
            root_delay > 0,
            "root_delay must be non-zero when quality data is present"
        );

        // root_dispersion (bytes 8-11) should be non-zero
        let root_dispersion = u32::from_be_bytes(resp[8..12].try_into().unwrap());
        assert!(
            root_dispersion > 0,
            "root_dispersion must be non-zero when quality data is present"
        );

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
        let quality = no_quality();
        let server_handle = tokio::spawn(async move {
            let s = NtpServer::new(addr, tb_clone, metrics_clone, quality, 16_000);
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

        // root_delay and root_dispersion must be 0 when unsynced
        let root_delay = u32::from_be_bytes(resp[4..8].try_into().unwrap());
        let root_dispersion = u32::from_be_bytes(resp[8..12].try_into().unwrap());
        assert_eq!(root_delay, 0, "unsynced path must have root_delay=0");
        assert_eq!(
            root_dispersion, 0,
            "unsynced path must have root_dispersion=0"
        );

        server_handle.abort();
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

    // ── Unused AtomicU64 import guard ─────────────────────────────────────
    // (referenced by the outer test helpers that use Arc<AtomicU64>)
    #[allow(dead_code)]
    fn _uses_atomic_u64(_: Arc<AtomicU64>) {}
}
