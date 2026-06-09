use crate::config::Config;
use crate::metrics::SharedMetrics;
use crate::ntp::selection::{SelectionDiagnostics, TimingSource};
use crate::performance::{LockFreeMetrics, TimeCache};
use crate::timebase::TimeBase;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::time::Instant;

/// RFC 5905 §8 four-tuple timing data from the most recent successful
/// NTP sync. After P0-1/P0-2 the T2/T3 values and root fields are
/// measured directly from packet bytes; `timing_source` records which.
#[derive(Debug, Clone)]
pub struct NtpTimingSummary {
    pub server: String,
    /// T1 — client transmit time (unix-epoch ms)
    pub t1_client_send_ms: i64,
    /// T2 — server receive time (unix-epoch ms, measured after P0-2)
    pub t2_server_recv_ms: i64,
    /// T3 — server transmit time (unix-epoch ms, measured after P0-2)
    pub t3_server_send_ms: i64,
    /// T4 — client receive time (unix-epoch ms)
    pub t4_client_recv_ms: i64,
    /// θ = ((T2-T1) + (T3-T4)) / 2  (positive = local behind server)
    pub offset_ms: i64,
    /// δ = (T4-T1) - (T3-T2)
    pub rtt_ms: u64,
    // Packet-level fields (measured from NTP reply bytes after P0-2)
    pub root_delay_ms: u32,
    pub root_dispersion_ms: u32,
    pub stratum: u8,
    pub leap: u8,
    pub precision_log2: i8,
    pub reference_id: u32,
    pub timing_source: TimingSource,
}

// `SyncQuality` is defined in `ntp::sync` (to keep ntp→http dependency-free)
// and re-exported here for convenience.
pub use crate::ntp::SyncQuality;

/// Snapshot of the current manual time override, stored in `AppState`.
/// Populated by `POST /admin/time/override` and cleared on expiry or DELETE.
#[derive(Debug)]
pub struct ManualOverrideState {
    /// The forced epoch_ms value.
    pub epoch_ms: i64,
    /// Epoch ms at the moment the override was set (from timebase at set time).
    pub set_at_ms: i64,
    /// Epoch ms when the override expires (set_at_ms + ttl_secs * 1000).
    pub expires_at_ms: i64,
    /// Monotonic instant at set time, used to compute current age.
    pub set_at_instant: Instant,
    /// Human-readable reason for the override (required, non-empty).
    pub reason: String,
    /// Optional operator identifier.
    pub operator: Option<String>,
    /// epoch_ms − NTP_time at set time (positive = override is ahead of NTP).
    pub jump_ms: i64,
}

/// Override info included in quality responses when a manual override is active.
#[derive(Debug, Clone, serde::Serialize)]
pub struct OverrideInfo {
    pub epoch_ms: i64,
    pub set_at_ms: i64,
    pub expires_at_ms: i64,
    pub reason: String,
    pub operator: Option<String>,
    pub jump_ms: i64,
    pub ttl_remaining_secs: i64,
}

/// Result of the time-quality computation for a single request.
///
/// Computed by [`AppState::compute_quality`] from the last `SyncQuality`
/// snapshot plus the configured SLA thresholds. Drives the serve/stop
/// policy, response headers, `/status`, `/time/full`, and WS ticks.
#[derive(Debug, Clone)]
pub struct TimeQuality {
    /// `"ntp"` | `"degraded"` | `"unsynced"` | `"manual"`
    pub source: &'static str,
    /// `"ok"` | `"degraded"` | `"stopped"` | `"unsynced"`
    pub serve_state: &'static str,
    /// RFC 5905 §11.2 dispersion (ms). `None` when unsynced.
    pub uncertainty_ms: Option<f64>,
    /// Milliseconds since last successful sync (or since override was set). `None` when unsynced.
    pub staleness_ms: Option<u64>,
    pub stratum: Option<u8>,
    pub selected_server: Option<String>,
    pub leap: Option<u8>,
    /// Present when source="manual"; null otherwise.
    pub override_info: Option<OverrideInfo>,
    /// P1-6 selection diagnostics from the most recent sync; None until first sync or when source="manual".
    pub selection: Option<SelectionDiagnostics>,
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
    /// Full quality snapshot for use by the UDP server (P0-3) and
    /// `/status` endpoint (P0-4). `None` until the first sync.
    pub last_sync_quality: Arc<parking_lot::RwLock<Option<SyncQuality>>>,
    /// P1-6 selection diagnostics from the most recent sync (success or failure).
    pub last_selection_diagnostics: Arc<parking_lot::RwLock<Option<SelectionDiagnostics>>>,
    /// Active manual time override state (P1-7).  `None` when no override is set.
    pub override_state: Arc<parking_lot::RwLock<Option<ManualOverrideState>>>,
    /// Handle to the background expiry task for the current override.
    /// Aborted and replaced on each new POST, aborted on DELETE.
    pub override_task: Arc<parking_lot::Mutex<Option<tokio::task::AbortHandle>>>,
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
            last_sync_quality: Arc::new(parking_lot::RwLock::new(None)),
            last_selection_diagnostics: Arc::new(parking_lot::RwLock::new(None)),
            override_state: Arc::new(parking_lot::RwLock::new(None)),
            override_task: Arc::new(parking_lot::Mutex::new(None)),
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

    /// Compute the current time-quality envelope.
    ///
    /// Source precedence:
    /// 1. Manual override active → source="manual", serve_state="ok"
    /// 2. NTP synced (with thresholds ok_max=50 ms, degraded_max=250 ms by default)
    /// 3. Unsynced → source="unsynced", serve_state="unsynced"
    pub fn compute_quality(&self) -> TimeQuality {
        // ── Manual override path (highest priority) ───────────────────────────
        if self.timebase.is_manual_active() {
            let guard = self.override_state.read();
            if let Some(ov) = guard.as_ref() {
                let age_ms = ov.set_at_instant.elapsed().as_millis() as i64;
                let now_approx_ms = ov.set_at_ms + age_ms;
                let ttl_remaining_ms = (ov.expires_at_ms - now_approx_ms).max(0);
                let ttl_remaining_secs = ttl_remaining_ms / 1000;
                let override_info = OverrideInfo {
                    epoch_ms: ov.epoch_ms,
                    set_at_ms: ov.set_at_ms,
                    expires_at_ms: ov.expires_at_ms,
                    reason: ov.reason.clone(),
                    operator: ov.operator.clone(),
                    jump_ms: ov.jump_ms,
                    ttl_remaining_secs,
                };
                return TimeQuality {
                    source: "manual",
                    serve_state: "ok",
                    uncertainty_ms: Some(self.config.admin.dispersion_ms as f64),
                    staleness_ms: Some(age_ms as u64),
                    stratum: Some(2),
                    selected_server: None,
                    leap: Some(0),
                    override_info: Some(override_info),
                    selection: None,
                };
            }
        }

        // ── NTP path ──────────────────────────────────────────────────────────
        let quality_guard = self.last_sync_quality.read();
        match quality_guard.as_ref() {
            None => TimeQuality {
                source: "unsynced",
                serve_state: "unsynced",
                uncertainty_ms: None,
                staleness_ms: None,
                stratum: None,
                selected_server: None,
                leap: None,
                override_info: None,
                selection: self.last_selection_diagnostics.read().clone(),
            },
            Some(q) => {
                let uncertainty_ms = q.compute_dispersion_ms();
                let age_ms = q.last_sync_instant.elapsed().as_millis() as u64;
                let age_secs = age_ms / 1000;
                let is_stale = age_secs > self.config.ntp.max_staleness_secs;
                let ok_max = self.config.quality.serve_ok_max_uncertainty_ms;
                let degraded_max = self.config.quality.serve_degraded_max_uncertainty_ms;

                let (source, serve_state) = if !is_stale && uncertainty_ms <= ok_max {
                    ("ntp", "ok")
                } else if uncertainty_ms <= degraded_max {
                    if self.config.quality.allow_degraded {
                        ("degraded", "degraded")
                    } else {
                        ("degraded", "stopped")
                    }
                } else {
                    ("degraded", "stopped")
                };

                TimeQuality {
                    source,
                    serve_state,
                    uncertainty_ms: Some(uncertainty_ms),
                    staleness_ms: Some(age_ms),
                    stratum: Some(q.stratum),
                    selected_server: Some(q.selected_server.clone()),
                    leap: Some(q.leap),
                    override_info: None,
                    selection: self.last_selection_diagnostics.read().clone(),
                }
            }
        }
    }
}
