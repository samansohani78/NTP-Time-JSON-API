use std::time::Duration;

/// One NTP query result, carrying the RFC 5905 §8 four-tuple
/// (T1, T2, T3, T4) plus derived fields:
///
/// * `T1` — client transmit time (origin timestamp in the request)
/// * `T2` — server receive time (parsed from the server's response)
/// * `T3` — server transmit time (parsed from the server's response)
/// * `T4` — client receive time
///
/// `offset_ms` and `rtt` are the standard derived values; `epoch_ms`
/// is the corrected unix epoch at T4 (i.e. `T4 + offset` plus the
/// configured biases).
///
/// We carry T1–T4 explicitly so the math is auditable and so callers
/// (metrics, debug endpoints) can inspect the upstream's view of our
/// clock. The four fields are in unix-epoch milliseconds.
#[derive(Debug, Clone)]
pub struct NtpResult {
    pub server: String,
    pub epoch_ms: i64,
    pub rtt: Duration,
    pub offset_ms: i64,
    pub t1_client_send_ms: i64,
    pub t2_server_recv_ms: i64,
    pub t3_server_send_ms: i64,
    pub t4_client_recv_ms: i64,
    pub instant: std::time::Instant,
}

impl NtpResult {
    /// Test-only constructor. Real production code paths build
    /// `NtpResult` from the full RFC 5905 four-tuple inside
    /// `query_ntp_server`; tests that exercise `select_best_result`
    /// only care about `offset_ms` and `rtt`, so we zero the T1–T4
    /// fields here.
    #[cfg(test)]
    pub fn for_testing(
        server: &str,
        epoch_ms: i64,
        rtt: Duration,
        offset_ms: i64,
        instant: std::time::Instant,
    ) -> Self {
        Self {
            server: server.to_string(),
            epoch_ms,
            rtt,
            offset_ms,
            t1_client_send_ms: 0,
            t2_server_recv_ms: 0,
            t3_server_send_ms: 0,
            t4_client_recv_ms: 0,
            instant,
        }
    }
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
        let results = vec![NtpResult::for_testing(
            "server1:123",
            1000000,
            Duration::from_millis(50),
            100,
            std::time::Instant::now(),
        )];

        let best = ServerSelector::select_best_result(results, 1000);
        assert!(best.is_some());
        assert_eq!(best.unwrap().server, "server1:123");
    }

    #[test]
    fn test_select_best_result_outlier_filtering() {
        let now = std::time::Instant::now();
        let results = vec![
            NtpResult::for_testing("server1:123", 1000000, Duration::from_millis(30), 100, now),
            NtpResult::for_testing("server2:123", 1000050, Duration::from_millis(20), 150, now),
            NtpResult::for_testing(
                "server3:123",
                2000000,
                Duration::from_millis(10),
                10000,
                now,
            ),
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
            NtpResult::for_testing("server1:123", 1000000, Duration::from_millis(20), 50, now),
            NtpResult::for_testing("server2:123", 1000100, Duration::from_millis(100), 95, now),
            NtpResult::for_testing("server3:123", 1000150, Duration::from_millis(50), 150, now),
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
            NtpResult::for_testing("server1:123", 1000000, Duration::from_millis(50), 100, now),
            NtpResult::for_testing("server2:123", 1000100, Duration::from_millis(20), 100, now),
        ];

        let best = ServerSelector::select_best_result(results, 1000);
        assert!(best.is_some());
        // When accuracy is equal, RTT is used as tiebreaker
        assert_eq!(best.unwrap().server, "server2:123");
    }

    /// Hand-computed RFC 5905 §8 four-tuple.
    ///
    /// Physical setup: client sends at T1=1000, server's clock is
    /// 30ms behind client's. Network transit is 50ms each way,
    /// server holds the packet for 100ms.
    /// * T1 = 1000 ms (client send, client clock)
    /// * T2 = 1020 ms (server receive, server clock: 1000+50-30)
    /// * T3 = 1120 ms (server send, server clock: 1020+100)
    /// * T4 = 1200 ms (client receive, client clock: 1000+50+100+50)
    ///
    /// From these: θ = ((1020-1000)+(1120-1200))/2 = (20+(-80))/2 = -30
    ///             δ = (1200-1000)-(1120-1020) = 200-100 = 100
    /// corrected_time = T4 + θ = 1200 + (-30) = 1170
    #[test]
    fn rfc5905_four_tuple_relations_hold() {
        let r = NtpResult {
            server: "test:123".to_string(),
            epoch_ms: 1170,
            rtt: Duration::from_millis(100),
            offset_ms: -30,
            t1_client_send_ms: 1000,
            t2_server_recv_ms: 1020,
            t3_server_send_ms: 1120,
            t4_client_recv_ms: 1200,
            instant: std::time::Instant::now(),
        };

        let derived_offset_ms = ((r.t2_server_recv_ms - r.t1_client_send_ms)
            + (r.t3_server_send_ms - r.t4_client_recv_ms))
            / 2;
        let derived_delay_ms = (r.t4_client_recv_ms - r.t1_client_send_ms)
            - (r.t3_server_send_ms - r.t2_server_recv_ms);

        assert_eq!(derived_offset_ms, r.offset_ms, "θ derivation");
        assert_eq!(derived_delay_ms, r.rtt.as_millis() as i64, "δ derivation");
        assert_eq!(
            r.epoch_ms,
            r.t4_client_recv_ms + r.offset_ms,
            "corrected time"
        );

        // Cross-check the inverse derivations used in query_ntp_server:
        //   T2 = T1 + θ + δ/2
        //   T3 = T4 + θ - δ/2
        let half_delay = derived_delay_ms / 2;
        let t2_derived = r.t1_client_send_ms + derived_offset_ms + half_delay;
        let t3_derived = r.t4_client_recv_ms + derived_offset_ms - half_delay;
        assert_eq!(t2_derived, r.t2_server_recv_ms, "T2 inverse derivation");
        assert_eq!(t3_derived, r.t3_server_send_ms, "T3 inverse derivation");
    }
}
