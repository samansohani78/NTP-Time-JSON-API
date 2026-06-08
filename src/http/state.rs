use crate::config::Config;
use crate::metrics::SharedMetrics;
use crate::performance::{LockFreeMetrics, TimeCache};
use crate::timebase::TimeBase;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::time::Instant;

/// RFC 5905 §8 four-tuple timing data from the most recent successful
/// NTP sync. Exposed via the `/performance` endpoint so callers can
/// audit the upstream clock exchange.
#[derive(Debug, Clone)]
pub struct NtpTimingSummary {
    pub server: String,
    /// T1 — client transmit time (unix-epoch ms)
    pub t1_client_send_ms: i64,
    /// T2 — server receive time (unix-epoch ms, derived)
    pub t2_server_recv_ms: i64,
    /// T3 — server transmit time (unix-epoch ms, derived)
    pub t3_server_send_ms: i64,
    /// T4 — client receive time (unix-epoch ms)
    pub t4_client_recv_ms: i64,
    /// θ = ((T2-T1) + (T3-T4)) / 2  (positive = local behind server)
    pub offset_ms: i64,
    /// δ = (T4-T1) - (T3-T2)
    pub rtt_ms: u64,
}

#[derive(Clone)]
pub struct AppState {
    pub config: Arc<Config>,
    pub timebase: TimeBase,
    pub metrics: SharedMetrics,
    pub time_cache: Arc<TimeCache>,
    pub perf_metrics: Arc<LockFreeMetrics>,
    pub last_sync_time: Arc<parking_lot::RwLock<Option<Instant>>>,
    pub consecutive_failures: Arc<parking_lot::RwLock<u32>>,
    /// RTT of the most recent successful NTP sync in milliseconds.
    /// Used by the UDP NTP server to populate `root_delay`.
    /// Zero means no successful sync has occurred yet.
    pub last_rtt_ms: Arc<AtomicU64>,
    /// RFC 5905 four-tuple from the most recent successful NTP sync.
    /// `None` until the first sync completes.
    pub last_ntp_timing: Arc<parking_lot::RwLock<Option<NtpTimingSummary>>>,
}

impl AppState {
    pub fn new(
        config: Arc<Config>,
        timebase: TimeBase,
        metrics: SharedMetrics,
        time_cache: Arc<TimeCache>,
        perf_metrics: Arc<LockFreeMetrics>,
    ) -> Self {
        Self {
            config,
            timebase,
            metrics,
            time_cache,
            perf_metrics,
            last_sync_time: Arc::new(parking_lot::RwLock::new(None)),
            consecutive_failures: Arc::new(parking_lot::RwLock::new(0)),
            last_rtt_ms: Arc::new(AtomicU64::new(0)),
            last_ntp_timing: Arc::new(parking_lot::RwLock::new(None)),
        }
    }

    pub fn record_sync_success(&self) {
        *self.last_sync_time.write() = Some(Instant::now());
        *self.consecutive_failures.write() = 0;
    }

    pub fn record_sync_failure(&self) {
        *self.consecutive_failures.write() += 1;
    }

    pub fn get_staleness_seconds(&self) -> Option<u64> {
        self.last_sync_time
            .read()
            .as_ref()
            .map(|t| t.elapsed().as_secs())
    }

    pub fn get_consecutive_failures(&self) -> u32 {
        *self.consecutive_failures.read()
    }
}
