use std::collections::VecDeque;
use std::time::{Duration, Instant};

const JITTER_RING_SIZE: usize = 8;

#[derive(Debug, Clone)]
pub struct ServerStats {
    /// Server address. Kept for log/debug visibility; not used by the
    /// stats logic itself (callers index the map by address).
    #[allow(dead_code)]
    pub address: String,
    pub last_rtt: Option<Duration>,
    pub last_success: Option<Instant>,
    pub last_failure: Option<Instant>,
    pub consecutive_failures: u32,
    pub total_queries: u64,
    pub total_failures: u64,
    pub disabled: bool,
    /// Ring buffer of the last JITTER_RING_SIZE offset_ms values for this server.
    recent_offsets: VecDeque<i64>,
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
            recent_offsets: VecDeque::with_capacity(JITTER_RING_SIZE),
        }
    }

    /// Record a new offset sample for jitter computation.
    pub fn record_offset(&mut self, offset_ms: i64) {
        if self.recent_offsets.len() >= JITTER_RING_SIZE {
            self.recent_offsets.pop_front();
        }
        self.recent_offsets.push_back(offset_ms);
    }

    /// Compute jitter as the population standard deviation of recent offsets (ms).
    /// Returns 0 when fewer than 2 samples have been recorded.
    pub fn jitter_ms(&self) -> u64 {
        let n = self.recent_offsets.len();
        if n < 2 {
            return 0;
        }
        let mean = self.recent_offsets.iter().map(|&x| x as f64).sum::<f64>() / n as f64;
        let var = self
            .recent_offsets
            .iter()
            .map(|&x| {
                let d = x as f64 - mean;
                d * d
            })
            .sum::<f64>()
            / n as f64;
        var.sqrt().ceil() as u64
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
}

#[cfg(test)]
impl ServerStats {
    pub fn is_available(&self) -> bool {
        !self.disabled && self.last_success.is_some()
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
