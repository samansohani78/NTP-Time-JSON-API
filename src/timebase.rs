use crate::ntp::SyncResult;
use crate::performance::TimeCache;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::time::Instant;
use tracing::debug;

/// Monotonic time base that avoids OS wall clock authority
/// Uses NTP-synced epoch time + monotonic clock progression
#[derive(Clone)]
pub struct TimeBase {
    /// Base NTP epoch time in milliseconds (set on successful sync)
    base_epoch_ms: Arc<AtomicI64>,

    /// Base monotonic instant (set on successful sync)
    base_instant: Arc<parking_lot::RwLock<Option<Instant>>>,

    /// Last served epoch_ms (for monotonic output clamping)
    last_served_ms: Arc<AtomicI64>,

    /// Whether monotonic output clamping is enabled
    monotonic_output: bool,

    /// Whether we've had at least one successful sync
    has_synced: Arc<AtomicBool>,

    /// Optional zero-copy JSON cache
    time_cache: Option<Arc<TimeCache>>,
}

impl TimeBase {
    pub fn new(monotonic_output: bool) -> Self {
        Self {
            base_epoch_ms: Arc::new(AtomicI64::new(0)),
            base_instant: Arc::new(parking_lot::RwLock::new(None)),
            last_served_ms: Arc::new(AtomicI64::new(0)),
            monotonic_output,
            has_synced: Arc::new(AtomicBool::new(false)),
            time_cache: None,
        }
    }

    pub fn with_cache(mut self, cache: Arc<TimeCache>) -> Self {
        self.time_cache = Some(cache);
        self
    }

    /// Update the time base with a new NTP sync result
    pub fn update(&self, sync_result: &SyncResult) {
        self.base_epoch_ms
            .store(sync_result.epoch_ms, Ordering::SeqCst);

        // CRITICAL: Use the instant from when epoch_ms was calculated, not current time
        // This prevents timing mismatches between epoch_ms and the monotonic clock
        *self.base_instant.write() = Some(sync_result.instant);

        self.has_synced.store(true, Ordering::SeqCst);

        // Update zero-copy cache if available
        if let Some(cache) = &self.time_cache {
            cache.update(sync_result.epoch_ms, false);
        }

        debug!(
            base_epoch_ms = sync_result.epoch_ms,
            server = %sync_result.server,
            "Updated time base"
        );
    }

    /// Get current epoch time in milliseconds
    /// Returns None if not yet synced
    pub fn now_ms(&self) -> Option<i64> {
        if !self.has_synced.load(Ordering::SeqCst) {
            return None;
        }

        let base_instant_guard = self.base_instant.read();
        let base_instant = base_instant_guard.as_ref()?;

        let base_epoch_ms = self.base_epoch_ms.load(Ordering::SeqCst);

        // Calculate elapsed time since base
        let elapsed = Instant::now().duration_since(*base_instant);
        let elapsed_ms = elapsed.as_millis() as i64;

        let mut current_ms = base_epoch_ms + elapsed_ms;

        // Apply monotonic clamping if enabled
        if self.monotonic_output {
            let last_served = self.last_served_ms.load(Ordering::SeqCst);
            if current_ms <= last_served {
                current_ms = last_served + 1;
            }
            self.last_served_ms.store(current_ms, Ordering::SeqCst);
        }

        Some(current_ms)
    }

    /// Check if we've had at least one successful sync
    pub fn has_synced(&self) -> bool {
        self.has_synced.load(Ordering::SeqCst)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn create_test_sync_result(epoch_ms: i64) -> SyncResult {
        SyncResult {
            epoch_ms,
            server: "test:123".to_string(),
            rtt: Duration::from_millis(10),
            instant: Instant::now(),
        }
    }

    #[test]
    fn test_timebase_before_sync() {
        let tb = TimeBase::new(true);
        assert!(!tb.has_synced());
        assert!(tb.now_ms().is_none());
    }

    #[test]
    fn test_timebase_after_sync() {
        let tb = TimeBase::new(true);
        let sync_result = create_test_sync_result(1000000);

        tb.update(&sync_result);

        assert!(tb.has_synced());
        let now = tb.now_ms();
        assert!(now.is_some());
        // Should be close to base time (within a few ms)
        let diff = (now.unwrap() - 1000000).abs();
        assert!(diff < 100);
    }

    #[test]
    fn test_monotonic_progression() {
        let tb = TimeBase::new(true);
        let sync_result = create_test_sync_result(1000000);

        tb.update(&sync_result);

        let t1 = tb.now_ms().unwrap();
        std::thread::sleep(Duration::from_millis(5));
        let t2 = tb.now_ms().unwrap();

        // Time should always increase
        assert!(t2 > t1);
    }

    #[test]
    fn test_monotonic_clamping() {
        let tb = TimeBase::new(true);
        let sync_result = create_test_sync_result(1000000);

        tb.update(&sync_result);

        let t1 = tb.now_ms().unwrap();

        // Manually set last_served to a higher value (simulating time jump back)
        tb.last_served_ms.store(t1 + 1000, Ordering::SeqCst);

        let t2 = tb.now_ms().unwrap();

        // Should be clamped to last_served + 1
        assert!(t2 > t1 + 1000);
    }

    #[test]
    fn test_no_monotonic_clamping() {
        let tb = TimeBase::new(false);
        let sync_result = create_test_sync_result(1000000);

        tb.update(&sync_result);

        let t1 = tb.now_ms().unwrap();
        std::thread::sleep(Duration::from_millis(5));
        let t2 = tb.now_ms().unwrap();

        // Should still progress (based on Instant)
        assert!(t2 > t1);
    }
}
