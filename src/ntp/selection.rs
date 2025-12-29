use super::stats::ServerStats;
use std::time::Duration;

#[derive(Debug, Clone)]
pub struct NtpResult {
    pub server: String,
    pub epoch_ms: i64,
    pub rtt: Duration,
    pub offset_ms: i64,
    pub instant: std::time::Instant,
}

pub struct ServerSelector;

impl ServerSelector {
    /// Select servers to query based on RTT-min strategy
    #[allow(dead_code)]
    pub fn select_servers_for_query(stats: &[ServerStats], sample_count: usize) -> Vec<String> {
        // Filter out disabled servers first
        let mut server_list: Vec<_> = stats.iter().filter(|s| !s.disabled).collect();

        // If all servers are disabled, include them anyway (give them a chance to recover)
        if server_list.is_empty() {
            server_list = stats.iter().collect();
        }

        // Sort by RTT (healthy servers with low RTT first, then others)
        server_list.sort_by(|a, b| match (a.rtt_score(), b.rtt_score()) {
            (Some(rtt_a), Some(rtt_b)) => rtt_a.cmp(&rtt_b),
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (None, None) => std::cmp::Ordering::Equal,
        });

        // Take top N servers
        server_list
            .into_iter()
            .take(sample_count.max(1))
            .map(|s| s.address.clone())
            .collect()
    }

    /// Select the best result from multiple NTP responses using RTT-min + outlier filtering
    pub fn select_best_result(
        mut results: Vec<NtpResult>,
        max_offset_skew_ms: i64,
    ) -> Option<NtpResult> {
        if results.is_empty() {
            return None;
        }

        if results.len() == 1 {
            return results.into_iter().next();
        }

        // Calculate median offset for outlier detection
        let mut offsets: Vec<i64> = results.iter().map(|r| r.offset_ms).collect();
        offsets.sort_unstable();
        let median_offset = offsets[offsets.len() / 2];

        // Filter outliers
        let inliers: Vec<_> = results
            .iter()
            .filter(|r| (r.offset_ms - median_offset).abs() <= max_offset_skew_ms)
            .cloned()
            .collect();

        if inliers.is_empty() {
            // If all are outliers, just return the one with minimum RTT from all results
            results.sort_by_key(|r| r.rtt);
            return results.into_iter().next();
        }

        // Among inliers, select the one with minimum RTT
        inliers.into_iter().min_by_key(|r| r.rtt)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_select_servers_for_query() {
        let mut stats = vec![
            ServerStats::new("server1:123".to_string()),
            ServerStats::new("server2:123".to_string()),
            ServerStats::new("server3:123".to_string()),
        ];

        // Server 2 has best RTT
        stats[1].record_success(Duration::from_millis(10));
        // Server 1 has worse RTT
        stats[0].record_success(Duration::from_millis(50));
        // Server 3 has no success yet

        let selected = ServerSelector::select_servers_for_query(&stats, 2);
        assert_eq!(selected.len(), 2);
        // Server 2 should be first (best RTT)
        assert_eq!(selected[0], "server2:123");

        // Test disabled server filtering
        stats[1].disabled = true; // Disable the best server
        let selected = ServerSelector::select_servers_for_query(&stats, 2);
        assert_eq!(selected.len(), 2);
        // Server 2 should not be in the list now
        assert!(!selected.contains(&"server2:123".to_string()));
    }

    #[test]
    fn test_select_best_result_single() {
        let results = vec![NtpResult {
            server: "server1:123".to_string(),
            epoch_ms: 1000000,
            rtt: Duration::from_millis(50),
            offset_ms: 100,
            instant: std::time::Instant::now(),
        }];

        let best = ServerSelector::select_best_result(results, 1000);
        assert!(best.is_some());
        assert_eq!(best.unwrap().server, "server1:123");
    }

    #[test]
    fn test_select_best_result_outlier_filtering() {
        let now = std::time::Instant::now();
        let results = vec![
            NtpResult {
                server: "server1:123".to_string(),
                epoch_ms: 1000000,
                rtt: Duration::from_millis(30),
                offset_ms: 100,
                instant: now,
            },
            NtpResult {
                server: "server2:123".to_string(),
                epoch_ms: 1000050,
                rtt: Duration::from_millis(20),
                offset_ms: 150,
                instant: now,
            },
            NtpResult {
                server: "server3:123".to_string(),
                epoch_ms: 2000000, // Outlier
                rtt: Duration::from_millis(10),
                offset_ms: 10000,
                instant: now,
            },
        ];

        // With strict skew threshold, server3 should be filtered out
        let best = ServerSelector::select_best_result(results, 500);
        assert!(best.is_some());
        let result = best.unwrap();
        // Should pick server2 (min RTT among inliers)
        assert_eq!(result.server, "server2:123");
    }

    #[test]
    fn test_select_best_result_min_rtt() {
        let now = std::time::Instant::now();
        let results = vec![
            NtpResult {
                server: "server1:123".to_string(),
                epoch_ms: 1000000,
                rtt: Duration::from_millis(50),
                offset_ms: 100,
                instant: now,
            },
            NtpResult {
                server: "server2:123".to_string(),
                epoch_ms: 1000100,
                rtt: Duration::from_millis(20),
                offset_ms: 110,
                instant: now,
            },
        ];

        let best = ServerSelector::select_best_result(results, 1000);
        assert!(best.is_some());
        // Should pick server2 (min RTT)
        assert_eq!(best.unwrap().server, "server2:123");
    }
}
