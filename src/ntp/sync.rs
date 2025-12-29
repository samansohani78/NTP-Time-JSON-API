use super::selection::{NtpResult, ServerSelector};
use super::stats::ServerStats;
use crate::config::NtpConfig;
use anyhow::{Context, Result};
use rsntp::SntpClient;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::sync::RwLock;
use tokio::time::timeout;
use tracing::{error, info, warn};

#[derive(Debug, Clone)]
pub struct SyncResult {
    pub epoch_ms: i64,
    pub server: String,
    pub rtt: Duration,
}

pub struct NtpSyncer {
    config: Arc<NtpConfig>,
    stats: Arc<RwLock<HashMap<String, ServerStats>>>,
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
        }
    }

    /// Perform a full sync operation using configured strategy
    pub async fn sync(&self) -> Result<SyncResult> {
        // Query ALL servers to test them and select the best one
        let all_servers: Vec<String> = self.config.servers.clone();

        info!(
            servers = ?all_servers,
            total_count = all_servers.len(),
            "Testing all NTP servers"
        );

        // Query all servers in parallel
        let mut query_tasks = Vec::new();
        for server in all_servers {
            let server_clone = server.clone();
            let timeout_duration = Duration::from_secs(self.config.timeout_secs);
            let task = tokio::spawn(async move {
                Self::query_ntp_server(&server_clone, timeout_duration).await
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
                    let mut stats_write = self.stats.write().await;
                    if let Some(stat) = stats_write.get_mut(&server) {
                        let just_disabled =
                            stat.record_failure(self.config.max_consecutive_failures);
                        if just_disabled {
                            warn!(
                                server = %server,
                                consecutive_failures = stat.consecutive_failures,
                                threshold = self.config.max_consecutive_failures,
                                "NTP server disabled after exceeding failure threshold"
                            );
                        }
                    }
                    drop(stats_write);
                }
                Err(e) => {
                    error!(server = %server, error = %e, "NTP query task panicked");
                    let mut stats_write = self.stats.write().await;
                    if let Some(stat) = stats_write.get_mut(&server) {
                        let just_disabled =
                            stat.record_failure(self.config.max_consecutive_failures);
                        if just_disabled {
                            warn!(
                                server = %server,
                                consecutive_failures = stat.consecutive_failures,
                                threshold = self.config.max_consecutive_failures,
                                "NTP server disabled after exceeding failure threshold"
                            );
                        }
                    }
                    drop(stats_write);
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
        let best = ServerSelector::select_best_result(results, self.config.max_offset_skew_ms)
            .context("No valid NTP result after outlier filtering")?;

        info!(
            server = %best.server,
            rtt_ms = best.rtt.as_millis(),
            epoch_ms = best.epoch_ms,
            "Selected best NTP server (lowest latency)"
        );

        Ok(SyncResult {
            epoch_ms: best.epoch_ms + self.config.offset_bias_ms,
            server: best.server,
            rtt: best.rtt,
        })
    }

    /// Query a single NTP server
    async fn query_ntp_server(server: &str, timeout_duration: Duration) -> Result<NtpResult> {
        let start = Instant::now();

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

        let rtt = start.elapsed();

        // Convert NTP timestamp to Unix epoch milliseconds
        let ntp_seconds = result.clock_offset().as_secs_f64();
        let system_time = SystemTime::now();
        let unix_time = system_time
            .duration_since(UNIX_EPOCH)
            .context("System time before UNIX epoch")?;

        // Calculate actual NTP time
        let ntp_time_secs = unix_time.as_secs_f64() + ntp_seconds;
        let epoch_ms = (ntp_time_secs * 1000.0) as i64;
        let offset_ms = (ntp_seconds * 1000.0) as i64;

        Ok(NtpResult {
            server: server.to_string(),
            epoch_ms,
            rtt,
            offset_ms,
        })
    }

    /// Get current server statistics
    pub async fn get_stats(&self) -> HashMap<String, ServerStats> {
        self.stats.read().await.clone()
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
            sample_servers_per_sync: 3,
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
