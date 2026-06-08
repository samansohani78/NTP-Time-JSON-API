use super::selection::{NtpResult, ServerSelector};
use super::stats::ServerStats;
use crate::config::NtpConfig;
use anyhow::{Context, Result};
use rsntp::SntpClient;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::RwLock;
use tokio::time::timeout;
use tracing::{error, info, warn};

#[derive(Debug, Clone)]
pub struct SyncResult {
    pub epoch_ms: i64,
    pub server: String,
    pub rtt: Duration,
    pub instant: Instant, // The Instant when epoch_ms was calculated
}

pub struct NtpSyncer {
    config: Arc<NtpConfig>,
    stats: Arc<RwLock<HashMap<String, ServerStats>>>,
    current_server: Arc<RwLock<Option<String>>>, // Sticky server selection
}

impl NtpSyncer {
    pub fn new(config: Arc<NtpConfig>) -> Self {
        let mut stats_map = HashMap::new();
        for server in &config.servers {
            stats_map.insert(server.clone(), ServerStats::new(server.clone()));
        }

        Self {
            config,
            stats: Arc::new(RwLock::new(stats_map)),
            current_server: Arc::new(RwLock::new(None)),
        }
    }

    /// Perform a full sync operation using configured strategy
    pub async fn sync(&self) -> Result<SyncResult> {
        // SMART STICKY: Query ALL servers every time to find the best,
        // but only switch if significantly better
        let all_servers: Vec<String> = self.config.servers.clone();
        let current_server_opt = self.current_server.read().await.clone();

        info!(
            servers = ?all_servers,
            total_count = all_servers.len(),
            "Testing all NTP servers to find best one"
        );

        // Query all servers in parallel
        let mut query_tasks = Vec::new();
        for server in all_servers {
            let server_clone = server.clone();
            let timeout_duration = Duration::from_secs(self.config.timeout_secs);
            // Biases are captured per task so query_ntp_server can
            // apply them without needing &self.
            let offset_bias = self.config.offset_bias_ms;
            let asymmetry_bias = self.config.asymmetry_bias_ms;
            let task = tokio::spawn(async move {
                Self::query_ntp_server(&server_clone, timeout_duration, offset_bias, asymmetry_bias)
                    .await
            });
            query_tasks.push((server, task));
        }

        // Collect results
        let mut results = Vec::new();
        for (server, task) in query_tasks {
            match task.await {
                Ok(Ok(result)) => {
                    info!(
                        server = %server,
                        rtt_ms = result.rtt.as_millis(),
                        "NTP query successful"
                    );
                    results.push(result.clone());
                    let mut stats_write = self.stats.write().await;
                    if let Some(stat) = stats_write.get_mut(&server) {
                        let was_disabled = stat.record_success(result.rtt);
                        if was_disabled {
                            info!(
                                server = %server,
                                "NTP server re-enabled after successful response"
                            );
                        }
                    }
                    drop(stats_write);
                }
                Ok(Err(e)) => {
                    warn!(server = %server, error = %e, "NTP query failed");
                    self.record_server_failure(&server).await;
                }
                Err(e) => {
                    error!(server = %server, error = %e, "NTP query task panicked");
                    self.record_server_failure(&server).await;
                }
            }
        }

        if results.is_empty() {
            anyhow::bail!("All NTP servers failed");
        }

        // Log summary of tested servers
        let successful_count = results.len();
        let total_count = self.config.servers.len();
        let failed_count = total_count - successful_count;
        info!(
            successful = successful_count,
            failed = failed_count,
            total = total_count,
            "NTP server test summary"
        );

        // Select best result using outlier filtering + RTT-min
        let best =
            ServerSelector::select_best_result(results.clone(), self.config.max_offset_skew_ms)
                .context("No valid NTP result after outlier filtering")?;

        // SMART STICKY: Decide whether to switch to the new best server
        let selected_result = if let Some(current_server) = current_server_opt {
            // We have a current server - check if we should switch
            if let Some(current_result) = results.iter().find(|r| r.server == current_server) {
                // Current server is still responding
                let current_rtt_ms = current_result.rtt.as_millis();
                let best_rtt_ms = best.rtt.as_millis();
                // Positive = current is slower than best (improvement available).
                // Negative = current is faster than best (no improvement).
                let rtt_diff_ms = current_rtt_ms as i64 - best_rtt_ms as i64;

                // Switch if new server is significantly better (50ms+ improvement)
                // OR if current server is no longer the best by accuracy
                if best.server == current_server {
                    // Current server is still the best - keep using it
                    info!(
                        server = %current_server,
                        rtt_ms = current_rtt_ms,
                        "Current NTP server is still the best (sticky)"
                    );
                    current_result.clone()
                } else if rtt_diff_ms >= 50 {
                    // New server is significantly faster (50ms+ better)
                    *self.current_server.write().await = Some(best.server.clone());
                    info!(
                        old_server = %current_server,
                        old_rtt_ms = current_rtt_ms,
                        new_server = %best.server,
                        new_rtt_ms = best_rtt_ms,
                        rtt_diff_ms = rtt_diff_ms,
                        "Switching to better NTP server (50ms+ faster)"
                    );
                    best
                } else {
                    // New server is only slightly better - keep current for consistency
                    info!(
                        current_server = %current_server,
                        current_rtt_ms = current_rtt_ms,
                        best_server = %best.server,
                        best_rtt_ms = best_rtt_ms,
                        rtt_diff_ms = rtt_diff_ms,
                        "Keeping current NTP server (new server not significantly better)"
                    );
                    current_result.clone()
                }
            } else {
                // Current server failed or not in results - switch to new best
                *self.current_server.write().await = Some(best.server.clone());
                warn!(
                    old_server = %current_server,
                    new_server = %best.server,
                    new_rtt_ms = best.rtt.as_millis(),
                    "Current NTP server failed, switching to new best server"
                );
                best
            }
        } else {
            // No current server - select the best one
            *self.current_server.write().await = Some(best.server.clone());
            info!(
                server = %best.server,
                rtt_ms = best.rtt.as_millis(),
                epoch_ms = best.epoch_ms,
                "Selected initial NTP server (first sync)"
            );
            best
        };

        // epoch_ms already includes OFFSET_BIAS_MS and ASYMMETRY_BIAS_MS
        // (applied inside query_ntp_server). No further adjustment here.
        Ok(SyncResult {
            epoch_ms: selected_result.epoch_ms,
            server: selected_result.server,
            rtt: selected_result.rtt,
            instant: selected_result.instant,
        })
    }

    /// Query a single NTP server.
    ///
    /// Captures the RFC 5905 §8 four-tuple (T1, T2, T3, T4):
    /// * T1 — client transmit (SystemTime captured just before the
    ///   blocking call enters spawn_blocking)
    /// * T2 — server receive (derived: T1 + offset + delay/2)
    /// * T3 — server transmit (derived: T4 - offset + delay/2)
    /// * T4 — client receive (SystemTime captured immediately after
    ///   the blocking call returns)
    ///
    /// `epoch_ms` is the corrected unix-epoch at T4, i.e. `T4 + θ`.
    /// T2 and T3 are derived from T1, T4, θ (offset), and δ (delay)
    /// because the upstream `rsntp` library does not expose the raw
    /// server-side timestamps. The derivations are exact (modulo
    /// float→i64 rounding of θ/δ):
    ///
    /// ```text
    ///   θ = ((T2 - T1) + (T3 - T4)) / 2
    ///   δ =  (T4 - T1) - (T3 - T2)
    ///   T2 = T1 + θ + δ/2
    ///   T3 = T4 - θ + δ/2
    /// ```
    async fn query_ntp_server(
        server: &str,
        timeout_duration: Duration,
        offset_bias_ms: i64,
        asymmetry_bias_ms: i64,
    ) -> Result<NtpResult> {
        // T1 in two clocks: the Instant pair is used to compute RTT
        // (immune to wall-clock skew); the SystemTime pair participates
        // in the RFC 5905 four-tuple.
        let start = Instant::now();
        let t1_system = std::time::SystemTime::now();
        let t1_unix_ms = system_time_unix_ms(t1_system);

        // Parse server address
        let addr = server.to_string();

        // Perform NTP query with timeout
        let result = timeout(timeout_duration, async {
            tokio::task::spawn_blocking(move || {
                let client = SntpClient::new();
                client.synchronize(&addr)
            })
            .await
            .context("Task join error")?
            .context("SNTP synchronize failed")
        })
        .await
        .context("NTP query timeout")??;

        // CRITICAL: Capture both system time and instant IMMEDIATELY after NTP query completes
        // These are paired together to avoid timing mismatches
        let after_query_instant = Instant::now();
        let t4_system = std::time::SystemTime::now();
        let t4_unix_ms = system_time_unix_ms(t4_system);

        let rtt = start.elapsed();

        // θ = server-reported clock offset (signed: positive = local
        // is behind server, negative = local is ahead of server)
        let offset = result.clock_offset();
        let offset_ms = (offset.as_secs_f64() * 1000.0) as i64;

        // δ = server-reported round-trip delay
        let delay_ms = (result.round_trip_delay().as_secs_f64() * 1000.0) as i64;

        // Derive T2 and T3 from T1, T4, θ, δ. Solving the
        // RFC 5905 §8 linear system:
        //   θ = ((T2 - T1) + (T3 - T4)) / 2
        //   δ =  (T4 - T1) - (T3 - T2)
        // yields:
        //   T2 = T1 + θ + δ/2
        //   T3 = T4 + θ - δ/2
        // Both are saturating additions to defend against i64 overflow
        // on inputs with extreme values.
        let half_delay_ms = delay_ms / 2;
        let t2_unix_ms = t1_unix_ms
            .saturating_add(offset_ms)
            .saturating_add(half_delay_ms);
        let t3_unix_ms = t4_unix_ms
            .saturating_add(offset_ms)
            .saturating_sub(half_delay_ms);

        // corrected_time = T4 + θ. Apply the user-configured
        // OFFSET_BIAS_MS and ASYMMETRY_BIAS_MS on top.
        // ASYMMETRY_BIAS_MS is a manual compensation for network
        // paths that are known to be asymmetric; the algorithm
        // itself is already the optimal symmetric correction.
        let ntp_time = apply_offset_to_systemtime(t4_system, offset)
            .context("Failed to apply offset to T4")?;

        let unix_time = ntp_time
            .duration_since(std::time::UNIX_EPOCH)
            .context("Time before UNIX epoch")?;

        let epoch_ms = unix_time.as_millis() as i64 + offset_bias_ms + asymmetry_bias_ms;

        Ok(NtpResult {
            server: server.to_string(),
            epoch_ms,
            rtt,
            offset_ms,
            t1_client_send_ms: t1_unix_ms,
            t2_server_recv_ms: t2_unix_ms,
            t3_server_send_ms: t3_unix_ms,
            t4_client_recv_ms: t4_unix_ms,
            instant: after_query_instant,
        })
    }

    /// Get current server statistics
    pub async fn get_stats(&self) -> HashMap<String, ServerStats> {
        self.stats.read().await.clone()
    }

    /// Record a server-side failure (query error or task panic) for the
    /// given server, auto-disabling it if the consecutive-failure
    /// threshold is reached.
    ///
    /// Both the query-failed (`Ok(Err(_))`) and task-panicked (`Err(_)`)
    /// arms of the result-collection loop funnel through here so the
    /// stats + log lines stay in lockstep.
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

/// Convert a `SystemTime` to unix-epoch milliseconds. Returns 0 if
/// `t` is before the unix epoch (pre-1970).
fn system_time_unix_ms(t: std::time::SystemTime) -> i64 {
    t.duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Apply a signed rsntp `SntpDuration` offset to a `SystemTime`,
/// returning the corrected time. Sign convention matches rsntp:
/// positive offset means "add this much to local time to get server
/// time".
fn apply_offset_to_systemtime(
    base: std::time::SystemTime,
    offset: rsntp::SntpDuration,
) -> Result<std::time::SystemTime> {
    let abs = offset
        .abs_as_std_duration()
        .context("Failed to convert offset to duration")?;
    if offset.signum() >= 0 {
        base.checked_add(abs)
            .context("Time overflow when adding offset")
    } else {
        base.checked_sub(abs)
            .context("Time underflow when subtracting offset")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::SelectionStrategy;

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
            selection_strategy: SelectionStrategy::RttMin,
            max_offset_skew_ms: 1000,
            monotonic_output: true,
            offset_bias_ms: 0,
            asymmetry_bias_ms: 0,
            max_consecutive_failures: 10,
        });
        let syncer = NtpSyncer::new(config);

        let stats = syncer.get_stats().await;
        // Should have stats for all configured servers
        assert!(!stats.is_empty());
    }

    // Note: Testing actual NTP queries requires network access
    // In production tests, use mock servers or integration tests
}
