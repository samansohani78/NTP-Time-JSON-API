use std::time::{Duration, Instant};

#[derive(Debug, Clone)]
pub struct ServerStats {
    pub address: String,
    pub last_rtt: Option<Duration>,
    pub last_success: Option<Instant>,
    pub last_failure: Option<Instant>,
    pub consecutive_failures: u32,
    pub total_queries: u64,
    pub total_failures: u64,
    pub disabled: bool,
}

impl ServerStats {
    pub fn new(address: String) -> Self {
        Self {
            address,
            last_rtt: None,
            last_success: None,
            last_failure: None,
            consecutive_failures: 0,
            total_queries: 0,
            total_failures: 0,
            disabled: false,
        }
    }

    pub fn record_success(&mut self, rtt: Duration) -> bool {
        self.last_rtt = Some(rtt);
        self.last_success = Some(Instant::now());
        self.consecutive_failures = 0;
        self.total_queries += 1;

        // Re-enable server if it was disabled
        let was_disabled = self.disabled;
        self.disabled = false;
        was_disabled
    }

    pub fn record_failure(&mut self, max_consecutive_failures: u32) -> bool {
        self.last_failure = Some(Instant::now());
        self.consecutive_failures += 1;
        self.total_queries += 1;
        self.total_failures += 1;

        // Disable server if it exceeds the threshold
        if self.consecutive_failures >= max_consecutive_failures && !self.disabled {
            self.disabled = true;
            return true; // Return true if server was just disabled
        }
        false
    }

    pub fn is_healthy(&self) -> bool {
        // Server is healthy if not disabled
        !self.disabled
    }

    #[allow(dead_code)]
    pub fn is_available(&self) -> bool {
        // Server is available if not disabled and has had at least one success
        !self.disabled && self.last_success.is_some()
    }

    #[allow(dead_code)]
    pub fn rtt_score(&self) -> Option<Duration> {
        if self.is_healthy() {
            self.last_rtt
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_server_stats_lifecycle() {
        let mut stats = ServerStats::new("time.google.com:123".to_string());
        let max_failures: u32 = 10;

        // Initially no success
        assert!(stats.is_healthy()); // Not disabled yet
        assert!(!stats.is_available()); // But not available (no success yet)

        // Record success
        let was_disabled = stats.record_success(Duration::from_millis(50));
        assert!(!was_disabled); // Was not disabled before
        assert!(stats.is_healthy());
        assert!(stats.is_available());
        assert_eq!(stats.consecutive_failures, 0);
        assert_eq!(stats.total_queries, 1);

        // Record a few failures
        stats.record_failure(max_failures);
        stats.record_failure(max_failures);
        assert!(stats.is_healthy()); // Still healthy with 2 failures
        assert_eq!(stats.consecutive_failures, 2);
        assert!(!stats.disabled);

        // Many failures (reach threshold)
        for _ in 0..8 {
            stats.record_failure(max_failures);
        }
        assert!(!stats.is_healthy()); // Now disabled
        assert!(stats.disabled);
        assert_eq!(stats.consecutive_failures, 10);

        // Success re-enables the server
        let was_disabled = stats.record_success(Duration::from_millis(60));
        assert!(was_disabled); // Was disabled before success
        assert!(stats.is_healthy());
        assert!(!stats.disabled);
        assert_eq!(stats.consecutive_failures, 0);
    }
}
