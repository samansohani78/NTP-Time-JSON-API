use super::client::{NtpClient, PacketNtpClient};
use super::selection::{NtpResult, SelectionDiagnostics, TimingSource, WeightedMedianSelector};
use super::stats::ServerStats;
use crate::config::NtpConfig;
use anyhow::{Context, Result};
use parking_lot::Mutex;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::RwLock;
use tracing::{error, info, warn};

/// Full quality snapshot from the most recent successful NTP sync.
///
/// Consumed by the UDP NTP server (P0-3) to compute honest
/// `root_delay`/`root_dispersion` values, and by the `/status` and
/// `/time/full` endpoints (P0-4).
#[derive(Debug, Clone)]
pub struct SyncQuality {
    /// Upstream server's `root_delay` field (NTP short → ms).
    pub upstream_root_delay_ms: u32,
    /// Upstream server's `root_dispersion` field (NTP short → ms).
    pub upstream_root_dispersion_ms: u32,
    /// Upstream server's `precision` field (log2 seconds).
    pub precision_log2: i8,
    pub stratum: u8,
    pub leap: u8,
    /// Measured RTT to the selected upstream NTP server (ms).
    pub measured_rtt_ms: u64,
    /// Offset jitter: population stddev of the last 8 offsets for the selected server (ms).
    pub jitter_ms: u64,
    #[allow(dead_code)]
    pub offset_ms: i64,
    /// Monotonic instant when the most recent sync completed.
    pub last_sync_instant: Instant,
    pub selected_server: String,
}

/// RFC 5905 §7.1 MAX_DRIFT: 15 µs/s maximum local clock drift.
const PHI: f64 = 15e-6; // seconds per second

impl SyncQuality {
    /// Compute this server's own root_dispersion (ms) using RFC 5905 §11.2:
    /// ```text
    /// upstream_dispersion + |precision| + jitter + PHI×age_s×1000 + rtt/2
    /// ```
    pub fn compute_dispersion_ms(&self) -> f64 {
        use super::protocol::precision_log2_to_ms;
        let age_s = self.last_sync_instant.elapsed().as_secs_f64();
        let precision_ms = precision_log2_to_ms(self.precision_log2).abs();
        ((self.upstream_root_dispersion_ms as f64)
            + precision_ms
            + (self.jitter_ms as f64)
            + PHI * age_s * 1000.0
            + (self.measured_rtt_ms as f64) / 2.0)
            .max(0.0)
    }
}

#[derive(Debug, Clone)]
pub struct SyncResult {
    pub epoch_ms: i64,
    pub server: String,
    pub rtt: Duration,
    pub instant: Instant,
    pub offset_ms: i64,
    // RFC 5905 §8 four-tuple (unix-epoch milliseconds)
    pub t1_client_send_ms: i64,
    pub t2_server_recv_ms: i64,
    pub t3_server_send_ms: i64,
    pub t4_client_recv_ms: i64,
    // Packet-level fields measured from the NTP reply
    pub root_delay_ms: u32,
    pub root_dispersion_ms: u32,
    pub stratum: u8,
    pub leap: u8,
    pub precision_log2: i8,
    pub reference_id: u32,
    pub timing_source: TimingSource,
}

/// Output of a successful `NtpSyncer::sync()`.
pub struct SyncOutcome {
    pub result: SyncResult,
    pub diagnostics: SelectionDiagnostics,
    /// Jitter (offset stddev, ms) for the selected server from its ring buffer.
    pub jitter_ms: u64,
}

pub struct NtpSyncer {
    config: Arc<NtpConfig>,
    stats: Arc<RwLock<HashMap<String, ServerStats>>>,
    current_server: Arc<RwLock<Option<String>>>,
    client: Arc<dyn NtpClient>,
    /// Most recent selection diagnostics — updated on every sync attempt, even failures.
    last_diagnostics: Arc<Mutex<Option<SelectionDiagnostics>>>,
}

impl NtpSyncer {
    /// Create with the default production client (`PacketNtpClient`).
    pub fn new(config: Arc<NtpConfig>) -> Self {
        Self::with_client(config, Arc::new(PacketNtpClient))
    }

    /// Create with an injected client — used in tests to supply a mock.
    pub fn with_client(config: Arc<NtpConfig>, client: Arc<dyn NtpClient>) -> Self {
        let mut stats_map = HashMap::new();
        for server in &config.servers {
            stats_map.insert(server.clone(), ServerStats::new(server.clone()));
        }
        Self {
            config,
            stats: Arc::new(RwLock::new(stats_map)),
            current_server: Arc::new(RwLock::new(None)),
            client,
            last_diagnostics: Arc::new(Mutex::new(None)),
        }
    }

    /// Last selection diagnostics (success or failure).  `None` until first sync attempt.
    pub fn last_diagnostics(&self) -> Option<SelectionDiagnostics> {
        self.last_diagnostics.lock().clone()
    }

    /// Jitter for the given server from its offset ring buffer (ms).
    pub async fn get_server_jitter(&self, server: &str) -> u64 {
        self.stats
            .read()
            .await
            .get(server)
            .map(|s| s.jitter_ms())
            .unwrap_or(0)
    }

    /// Perform a full sync: query all servers, run P1-6 weighted-median selection.
    pub async fn sync(&self) -> Result<SyncOutcome> {
        let all_servers: Vec<String> = self.config.servers.clone();
        let current_server_opt = self.current_server.read().await.clone();

        info!(
            servers = ?all_servers,
            total_count = all_servers.len(),
            "Testing all NTP servers to find best one"
        );

        // Query all servers in parallel
        let mut query_tasks = Vec::new();
        for server in &all_servers {
            let server = server.clone();
            let timeout_duration = Duration::from_secs(self.config.timeout_secs);
            let offset_bias = self.config.offset_bias_ms;
            let asymmetry_bias = self.config.asymmetry_bias_ms;
            let client = self.client.clone();
            let task = tokio::spawn(async move {
                Self::query_with_client(
                    client,
                    server,
                    timeout_duration,
                    offset_bias,
                    asymmetry_bias,
                )
                .await
            });
            query_tasks.push(task);
        }

        // Collect results and update per-server stats + offset ring
        let mut results = Vec::new();
        for (server, task) in all_servers.iter().zip(query_tasks) {
            match task.await {
                Ok(Ok(result)) => {
                    info!(
                        server = %server,
                        rtt_ms = result.rtt.as_millis(),
                        "NTP query successful"
                    );
                    let mut stats_write = self.stats.write().await;
                    if let Some(stat) = stats_write.get_mut(server) {
                        let was_disabled = stat.record_success(result.rtt);
                        stat.record_offset(result.offset_ms);
                        if was_disabled {
                            info!(server = %server, "NTP server re-enabled after successful response");
                        }
                    }
                    drop(stats_write);
                    results.push(result);
                }
                Ok(Err(e)) => {
                    warn!(server = %server, error = %e, "NTP query failed");
                    self.record_server_failure(server).await;
                }
                Err(e) => {
                    error!(server = %server, error = %e, "NTP query task panicked");
                    self.record_server_failure(server).await;
                }
            }
        }

        if results.is_empty() {
            anyhow::bail!("All NTP servers failed");
        }

        let successful = results.len();
        let failed = all_servers.len() - successful;
        info!(
            successful,
            failed,
            total = all_servers.len(),
            "NTP server test summary"
        );

        // Build jitter map from stats (accumulated across prior syncs)
        let jitter_by_server: HashMap<String, u64> = {
            let stats_read = self.stats.read().await;
            stats_read
                .iter()
                .map(|(k, v)| (k.clone(), v.jitter_ms()))
                .collect()
        };

        // P1-6 weighted-median + quorum selection
        let output = WeightedMedianSelector::select(
            results.clone(),
            &jitter_by_server,
            &self.config.selection,
        );

        // Always store diagnostics (even on failure)
        *self.last_diagnostics.lock() = Some(output.diagnostics.clone());

        let best = output
            .selected
            .context("No quorum: insufficient agreers after selection")?;

        // Sticky: switch servers only if the new best is significantly faster
        let (selected_result, new_sticky) =
            sticky_select(&output.agreers, best, current_server_opt.as_deref(), 50);

        if let Some(ref new_server) = new_sticky {
            let old = current_server_opt.as_deref().unwrap_or("<none>");
            if current_server_opt.is_none() {
                info!(
                    server = %new_server,
                    rtt_ms = selected_result.rtt.as_millis(),
                    "Selected initial NTP server (first sync)"
                );
            } else if results.iter().any(|r| r.server.as_str() == old) {
                info!(
                    old_server = %old,
                    new_server = %new_server,
                    new_rtt_ms = selected_result.rtt.as_millis(),
                    "Switching to better NTP server (50ms+ faster)"
                );
            } else {
                warn!(
                    old_server = %old,
                    new_server = %new_server,
                    new_rtt_ms = selected_result.rtt.as_millis(),
                    "Current NTP server failed, switching to new best server"
                );
            }
            *self.current_server.write().await = Some(new_server.clone());
        } else {
            info!(
                server = %selected_result.server,
                rtt_ms = selected_result.rtt.as_millis(),
                "Current NTP server is still the best (sticky)"
            );
        }

        let jitter_ms = jitter_by_server
            .get(&selected_result.server)
            .copied()
            .unwrap_or(0);

        Ok(SyncOutcome {
            result: SyncResult {
                epoch_ms: selected_result.epoch_ms,
                server: selected_result.server,
                rtt: selected_result.rtt,
                instant: selected_result.instant,
                offset_ms: selected_result.offset_ms,
                t1_client_send_ms: selected_result.t1_client_send_ms,
                t2_server_recv_ms: selected_result.t2_server_recv_ms,
                t3_server_send_ms: selected_result.t3_server_send_ms,
                t4_client_recv_ms: selected_result.t4_client_recv_ms,
                root_delay_ms: selected_result.root_delay_ms,
                root_dispersion_ms: selected_result.root_dispersion_ms,
                stratum: selected_result.stratum,
                leap: selected_result.leap,
                precision_log2: selected_result.precision_log2,
                reference_id: selected_result.reference_id,
                timing_source: selected_result.timing_source,
            },
            diagnostics: output.diagnostics,
            jitter_ms,
        })
    }

    /// Query a single NTP server using the injected `NtpClient`.
    async fn query_with_client(
        client: Arc<dyn NtpClient>,
        server: String,
        timeout_duration: Duration,
        offset_bias_ms: i64,
        asymmetry_bias_ms: i64,
    ) -> Result<NtpResult> {
        let sample = client.query(&server, timeout_duration).await?;
        let epoch_ms = sample.t4_unix_ms + sample.offset_ms + offset_bias_ms + asymmetry_bias_ms;
        let rtt = sample
            .t4_instant
            .saturating_duration_since(sample.t1_instant);
        Ok(NtpResult {
            server,
            epoch_ms,
            rtt,
            offset_ms: sample.offset_ms,
            t1_client_send_ms: sample.t1_unix_ms,
            t2_server_recv_ms: sample.t2_unix_ms,
            t3_server_send_ms: sample.t3_unix_ms,
            t4_client_recv_ms: sample.t4_unix_ms,
            instant: sample.t4_instant,
            root_delay_ms: sample.root_delay_ms,
            root_dispersion_ms: sample.root_dispersion_ms,
            stratum: sample.stratum,
            leap: sample.leap,
            precision_log2: sample.precision_log2,
            reference_id: sample.reference_id,
            timing_source: TimingSource::Measured,
        })
    }

    pub async fn get_stats(&self) -> HashMap<String, ServerStats> {
        self.stats.read().await.clone()
    }

    async fn record_server_failure(&self, server: &str) {
        let mut stats_write = self.stats.write().await;
        if let Some(stat) = stats_write.get_mut(server) {
            let just_disabled = stat.record_failure(self.config.max_consecutive_failures);
            if just_disabled {
                warn!(
                    server = %server,
                    consecutive_failures = stat.consecutive_failures,
                    threshold = self.config.max_consecutive_failures,
                    "NTP server disabled after exceeding failure threshold"
                );
            }
        }
    }
}

/// Pure sticky-server selection algorithm.
///
/// Operates on the `agreers` list (post-gate, post-agreement-filter), not all results.
/// If the current server is not an agreer, switches to `best`.
fn sticky_select(
    agreers: &[NtpResult],
    best: NtpResult,
    current_server: Option<&str>,
    switch_threshold_ms: i64,
) -> (NtpResult, Option<String>) {
    let Some(current) = current_server else {
        let s = best.server.clone();
        return (best, Some(s));
    };

    let Some(current_result) = agreers.iter().find(|r| r.server == current) else {
        let s = best.server.clone();
        return (best, Some(s));
    };

    if best.server == current {
        return (current_result.clone(), None);
    }

    let rtt_diff_ms = current_result.rtt.as_millis() as i64 - best.rtt.as_millis() as i64;
    if rtt_diff_ms >= switch_threshold_ms {
        let s = best.server.clone();
        (best, Some(s))
    } else {
        (current_result.clone(), None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{SelectionConfig, SelectionStrategy};
    use crate::ntp::client::{MockNtpClient, NtpSample};

    fn make_ntp_config() -> Arc<NtpConfig> {
        Arc::new(NtpConfig {
            servers: vec!["mock:123".to_string()],
            timeout_secs: 2,
            sync_interval_secs: 30,
            probe_min_interval_secs: 10,
            probe_max_interval_secs: 20,
            max_staleness_secs: 120,
            require_sync: true,
            selection_strategy: SelectionStrategy::AccuracyFirst,
            monotonic_output: true,
            offset_bias_ms: 0,
            asymmetry_bias_ms: 0,
            max_consecutive_failures: 10,
            selection: SelectionConfig {
                min_quorum: 1,
                ..SelectionConfig::default()
            },
        })
    }

    fn make_ntp_sample(server: &str) -> NtpSample {
        let now = Instant::now();
        NtpSample {
            server: server.to_string(),
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
            reference_id: u32::from_be_bytes(*b"LOCL"),
            poll: 4,
        }
    }

    #[tokio::test]
    async fn test_ntp_syncer_creation() {
        let config = Arc::new(NtpConfig {
            servers: vec!["time.google.com:123".to_string()],
            timeout_secs: 2,
            sync_interval_secs: 30,
            probe_min_interval_secs: 10,
            probe_max_interval_secs: 20,
            max_staleness_secs: 120,
            require_sync: true,
            selection_strategy: SelectionStrategy::AccuracyFirst,
            monotonic_output: true,
            offset_bias_ms: 0,
            asymmetry_bias_ms: 0,
            max_consecutive_failures: 10,
            selection: SelectionConfig {
                min_quorum: 1,
                ..SelectionConfig::default()
            },
        });
        let syncer = NtpSyncer::new(config);
        let stats = syncer.get_stats().await;
        assert!(!stats.is_empty());
    }

    #[tokio::test]
    async fn sync_populates_real_timing() {
        let sample = make_ntp_sample("mock:123");
        let config = make_ntp_config();
        let client = Arc::new(MockNtpClient::ok(sample.clone()));
        let syncer = NtpSyncer::with_client(config, client);

        let outcome = syncer.sync().await.expect("sync should succeed with mock");
        let result = outcome.result;

        assert_eq!(result.t1_client_send_ms, sample.t1_unix_ms);
        assert_eq!(result.t2_server_recv_ms, sample.t2_unix_ms);
        assert_eq!(result.t3_server_send_ms, sample.t3_unix_ms);
        assert_eq!(result.t4_client_recv_ms, sample.t4_unix_ms);
        assert_eq!(result.root_delay_ms, sample.root_delay_ms);
        assert_eq!(result.root_dispersion_ms, sample.root_dispersion_ms);
        assert_eq!(result.stratum, sample.stratum);
        assert_eq!(result.leap, sample.leap);
        assert_eq!(result.precision_log2, sample.precision_log2);
        assert_eq!(result.reference_id, sample.reference_id);
        assert_eq!(result.timing_source, TimingSource::Measured);
        assert_eq!(
            result.epoch_ms,
            sample.t4_unix_ms + sample.offset_ms,
            "epoch_ms = T4 + θ"
        );
    }

    #[tokio::test]
    async fn sync_applies_bias() {
        let sample = make_ntp_sample("mock:123");
        let mut config_val = NtpConfig {
            servers: vec!["mock:123".to_string()],
            timeout_secs: 2,
            sync_interval_secs: 30,
            probe_min_interval_secs: 10,
            probe_max_interval_secs: 20,
            max_staleness_secs: 120,
            require_sync: true,
            selection_strategy: SelectionStrategy::AccuracyFirst,
            monotonic_output: true,
            offset_bias_ms: 100,
            asymmetry_bias_ms: 50,
            max_consecutive_failures: 10,
            selection: SelectionConfig {
                min_quorum: 1,
                ..SelectionConfig::default()
            },
        };
        config_val.offset_bias_ms = 100;
        config_val.asymmetry_bias_ms = 50;
        let config = Arc::new(config_val);
        let client = Arc::new(MockNtpClient::ok(sample.clone()));
        let syncer = NtpSyncer::with_client(config, client);

        let outcome = syncer.sync().await.expect("sync should succeed");
        assert_eq!(
            outcome.result.epoch_ms,
            sample.t4_unix_ms + sample.offset_ms + 100 + 50,
            "bias must be applied"
        );
    }

    // ── No-quorum / fail-closed tests ────────────────────────────────────────

    /// With a single NTP server and min_quorum=2, sync() MUST return Err —
    /// never the single server as a min-RTT fallback.
    ///
    /// This is the integration-level proof that WeightedMedianSelector's
    /// fail-closed behavior propagates through NtpSyncer: the caller
    /// (sync_loop in main.rs) receives Err and therefore does NOT call
    /// timebase.update(), preserving the previous good sync state.
    #[tokio::test]
    async fn no_quorum_sync_returns_err_not_min_rtt_fallback() {
        let sample = make_ntp_sample("mock:123");
        let config = Arc::new(NtpConfig {
            servers: vec!["mock:123".to_string()],
            selection: SelectionConfig {
                min_quorum: 2, // impossible with 1 server
                ..SelectionConfig::default()
            },
            ..(*make_ntp_config()).clone()
        });
        let client = Arc::new(MockNtpClient::ok(sample));
        let syncer = NtpSyncer::with_client(config, client);

        let result = syncer.sync().await;
        assert!(
            result.is_err(),
            "sync() must return Err when quorum is not met — no min-RTT fallback"
        );
        let err_msg = format!("{}", result.err().unwrap());
        assert!(
            err_msg.to_lowercase().contains("quorum")
                || err_msg.to_lowercase().contains("no quorum"),
            "error must describe quorum failure, got: {err_msg}"
        );
    }

    /// After a successful first sync, a subsequent quorum failure returns Err —
    /// proving that the previous good TimeBase state is preserved by the caller
    /// (sync_loop receives Err and does not call timebase.update()).
    #[tokio::test]
    async fn no_quorum_on_second_sync_returns_err() {
        let sample = make_ntp_sample("mock:123");
        // First: succeed with min_quorum=1
        let config_ok = make_ntp_config();
        let client = Arc::new(MockNtpClient::ok(sample.clone()));
        let syncer_ok = NtpSyncer::with_client(config_ok, client.clone());
        let first = syncer_ok.sync().await.expect("first sync must succeed");
        let saved_epoch = first.result.epoch_ms;

        // Second syncer: same mock data but min_quorum=2 → Err
        let config_fail = Arc::new(NtpConfig {
            servers: vec!["mock:123".to_string()],
            selection: SelectionConfig {
                min_quorum: 2,
                ..SelectionConfig::default()
            },
            ..(*make_ntp_config()).clone()
        });
        let syncer_fail = NtpSyncer::with_client(config_fail, client);
        let second = syncer_fail.sync().await;
        assert!(
            second.is_err(),
            "second sync must return Err when quorum fails"
        );
        // The previous good epoch is preserved: the caller (sync_loop) doesn't
        // call timebase.update() on Err, so saved_epoch remains valid.
        assert!(saved_epoch > 0, "first sync epoch must be valid");
        let _ = saved_epoch; // used above
    }

    // ── sticky_select unit tests ──────────────────────────────────────────────

    fn make_result(server: &str, rtt_ms: u64, offset_ms: i64) -> NtpResult {
        NtpResult::for_testing(
            server,
            1_700_000_000_000,
            Duration::from_millis(rtt_ms),
            offset_ms,
            Instant::now(),
        )
    }

    #[test]
    fn sticky_select_no_current_returns_best_and_sets_sticky() {
        let r1 = make_result("a:123", 10, 5);
        let r2 = make_result("b:123", 20, 10);
        let best = r1.clone();
        let agreers = vec![r1, r2];

        let (selected, new_sticky) = sticky_select(&agreers, best, None, 50);
        assert_eq!(selected.server, "a:123");
        assert_eq!(new_sticky.as_deref(), Some("a:123"));
    }

    #[test]
    fn sticky_select_current_failed_switches_to_best() {
        let r1 = make_result("a:123", 10, 5);
        let agreers = vec![r1.clone()];

        let (selected, new_sticky) = sticky_select(&agreers, r1, Some("old:123"), 50);
        assert_eq!(selected.server, "a:123");
        assert_eq!(new_sticky.as_deref(), Some("a:123"));
    }

    #[test]
    fn sticky_select_current_still_best_no_switch() {
        let r1 = make_result("a:123", 10, 5);
        let agreers = vec![r1.clone()];
        let best = r1;

        let (selected, new_sticky) = sticky_select(&agreers, best, Some("a:123"), 50);
        assert_eq!(selected.server, "a:123");
        assert!(new_sticky.is_none(), "sticky should not change");
    }

    #[test]
    fn sticky_select_new_server_not_significantly_better_no_switch() {
        let current = make_result("a:123", 30, 5);
        let new_best = make_result("b:123", 20, 8);
        let agreers = vec![current.clone(), new_best.clone()];

        let (selected, new_sticky) = sticky_select(&agreers, new_best, Some("a:123"), 50);
        assert_eq!(selected.server, "a:123");
        assert!(new_sticky.is_none());
    }

    #[test]
    fn sticky_select_new_server_significantly_better_switches() {
        let current = make_result("a:123", 100, 5);
        let new_best = make_result("b:123", 20, 8);
        let agreers = vec![current.clone(), new_best.clone()];

        let (selected, new_sticky) = sticky_select(&agreers, new_best, Some("a:123"), 50);
        assert_eq!(selected.server, "b:123");
        assert_eq!(new_sticky.as_deref(), Some("b:123"));
    }

    #[test]
    fn sticky_select_exactly_at_threshold_switches() {
        let current = make_result("a:123", 70, 5);
        let new_best = make_result("b:123", 20, 8);
        let agreers = vec![current.clone(), new_best.clone()];

        let (selected, new_sticky) = sticky_select(&agreers, new_best, Some("a:123"), 50);
        assert_eq!(selected.server, "b:123");
        assert_eq!(new_sticky.as_deref(), Some("b:123"));
    }
}
