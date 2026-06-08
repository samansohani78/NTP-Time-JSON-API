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
    /// Select the best result from multiple NTP responses using accuracy-first algorithm
    ///
    /// Algorithm:
    /// 1. Calculate median offset (represents consensus time)
    /// 2. Filter outliers (servers disagreeing with consensus)
    /// 3. Among inliers, prefer server closest to median (most accurate)
    /// 4. Use RTT as tiebreaker for servers with similar accuracy
    pub fn select_best_result(
        mut results: Vec<NtpResult>,
        max_offset_skew_ms: i64,
    ) -> Option<NtpResult> {
        use tracing::info;

        if results.is_empty() {
            return None;
        }

        if results.len() == 1 {
            return results.into_iter().next();
        }

        // Calculate median offset for outlier detection.
        // Convention: for even-length inputs we pick the upper of the
        // two middle values. This biases outlier detection slightly
        // toward "include the optimistic half" — acceptable for our
        // use case where we then take the inlier closest to median.
        let mut offsets: Vec<i64> = results.iter().map(|r| r.offset_ms).collect();
        offsets.sort_unstable();
        let median_offset = offsets[offsets.len() / 2];

        // Calculate standard deviation for quality assessment
        let mean_offset: f64 =
            offsets.iter().map(|&x| x as f64).sum::<f64>() / offsets.len() as f64;
        let variance: f64 = offsets
            .iter()
            .map(|&x| {
                let diff = x as f64 - mean_offset;
                diff * diff
            })
            .sum::<f64>()
            / offsets.len() as f64;
        let std_dev = variance.sqrt();

        info!(
            total_servers = results.len(),
            median_offset_ms = median_offset,
            std_dev_ms = std_dev as i64,
            "Server offset statistics (lower std_dev = better agreement)"
        );

        // Filter outliers
        let inliers: Vec<_> = results
            .iter()
            .filter(|r| (r.offset_ms - median_offset).abs() <= max_offset_skew_ms)
            .cloned()
            .collect();

        if inliers.is_empty() {
            tracing::warn!(
                "All servers are outliers! Using fallback (min RTT). This may indicate network issues."
            );
            // If all are outliers, just return the one with minimum RTT from all results
            results.sort_by_key(|r| r.rtt);
            return results.into_iter().next();
        }

        let outlier_count = results.len() - inliers.len();
        if outlier_count > 0 {
            info!(
                outliers_filtered = outlier_count,
                inliers_remaining = inliers.len(),
                "Outlier filtering complete"
            );
        }

        // CRITICAL CHANGE: Select server with offset closest to median (most accurate)
        // Use RTT only as tiebreaker when accuracy is similar.
        // (offset_dist, rtt) ordering gives lexicographic comparison:
        // primary key is accuracy, secondary key is latency.
        let best = inliers
            .iter()
            .min_by_key(|r| {
                let offset_dist = (r.offset_ms - median_offset).abs();
                (offset_dist, r.rtt)
            })
            .cloned()?;

        let offset_from_median = (best.offset_ms - median_offset).abs();
        info!(
            selected_server = %best.server,
            offset_from_median_ms = offset_from_median,
            "Selected server with best accuracy (closest to consensus)"
        );

        Some(best)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        // Median offset = 150, so server1 (offset=100) and server2 (offset=150) are inliers
        // Should pick server2 because it's closer to median (offset_dist=0 vs 50)
        let best = ServerSelector::select_best_result(results, 500);
        assert!(best.is_some());
        let result = best.unwrap();
        assert_eq!(result.server, "server2:123");
    }

    #[test]
    fn test_select_best_result_accuracy_first() {
        let now = std::time::Instant::now();
        let results = vec![
            NtpResult {
                server: "server1:123".to_string(),
                epoch_ms: 1000000,
                rtt: Duration::from_millis(20), // Lower RTT
                offset_ms: 50,                  // Further from median (100)
                instant: now,
            },
            NtpResult {
                server: "server2:123".to_string(),
                epoch_ms: 1000100,
                rtt: Duration::from_millis(100), // Higher RTT
                offset_ms: 95,                   // Closer to median (100)
                instant: now,
            },
            NtpResult {
                server: "server3:123".to_string(),
                epoch_ms: 1000150,
                rtt: Duration::from_millis(50),
                offset_ms: 150, // Further from median
                instant: now,
            },
        ];

        let best = ServerSelector::select_best_result(results, 1000);
        assert!(best.is_some());
        // Median of [50, 95, 150] = 95
        // Should pick server2 (offset=95, closest to median) despite higher RTT
        // This prioritizes accuracy over latency
        assert_eq!(best.unwrap().server, "server2:123");
    }

    #[test]
    fn test_select_best_result_rtt_tiebreaker() {
        let now = std::time::Instant::now();
        let results = vec![
            NtpResult {
                server: "server1:123".to_string(),
                epoch_ms: 1000000,
                rtt: Duration::from_millis(50),
                offset_ms: 100, // Same distance from median
                instant: now,
            },
            NtpResult {
                server: "server2:123".to_string(),
                epoch_ms: 1000100,
                rtt: Duration::from_millis(20), // Lower RTT
                offset_ms: 100,                 // Same distance from median
                instant: now,
            },
        ];

        let best = ServerSelector::select_best_result(results, 1000);
        assert!(best.is_some());
        // When accuracy is equal, RTT is used as tiebreaker
        assert_eq!(best.unwrap().server, "server2:123");
    }
}
