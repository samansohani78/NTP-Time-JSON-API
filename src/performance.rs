use arc_swap::ArcSwap;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::time::Instant;

/// Zero-copy time cache - pre-serialized JSON responses
/// Updates are lock-free using arc-swap
pub struct TimeCache {
    // Raw epoch milliseconds
    epoch_ms: AtomicI64,

    // Pre-serialized JSON responses (zero-copy, just Arc cloning)
    json_response: Arc<ArcSwap<String>>,
    json_response_cache: Arc<ArcSwap<String>>, // For cache (stale) responses

    // Last update timestamp
    last_update: AtomicI64,

    // Configuration
    message_ok: String,
    message_ok_cache: String,
}

impl TimeCache {
    pub fn new(message_ok: String, message_ok_cache: String) -> Self {
        let initial_json = Arc::new(String::from(r#"{"message":"initializing","status":503}"#));

        Self {
            epoch_ms: AtomicI64::new(0),
            json_response: Arc::new(ArcSwap::from_pointee((*initial_json).clone())),
            json_response_cache: Arc::new(ArcSwap::from_pointee((*initial_json).clone())),
            last_update: AtomicI64::new(0),
            message_ok,
            message_ok_cache,
        }
    }

    /// Update cache with new time (lock-free, atomic)
    pub fn update(&self, epoch_ms: i64, _is_stale: bool) {
        // Store epoch
        self.epoch_ms.store(epoch_ms, Ordering::Release);
        self.last_update.store(
            Instant::now().elapsed().as_millis() as i64,
            Ordering::Release,
        );

        // Pre-serialize fresh JSON
        let fresh_json = format!(
            r#"{{"data":{},"message":"{}","status":200}}"#,
            epoch_ms, self.message_ok
        );

        // Pre-serialize stale JSON
        let stale_json = format!(
            r#"{{"data":{},"message":"{}","status":200}}"#,
            epoch_ms, self.message_ok_cache
        );

        // Lock-free atomic swap
        self.json_response.store(Arc::new(fresh_json));
        self.json_response_cache.store(Arc::new(stale_json));
    }

    /// Get pre-serialized JSON (zero-copy - just Arc clone)
    /// Returns Arc<String> which is just a pointer increment
    pub fn get_json(&self, is_stale: bool) -> Arc<String> {
        if is_stale {
            self.json_response_cache.load_full()
        } else {
            self.json_response.load_full()
        }
    }

    /// Get raw epoch (for non-HTTP uses)
    #[allow(dead_code)]
    pub fn get_epoch(&self) -> i64 {
        self.epoch_ms.load(Ordering::Acquire)
    }

    /// Check if cache has been initialized
    #[allow(dead_code)]
    pub fn is_initialized(&self) -> bool {
        self.epoch_ms.load(Ordering::Acquire) > 0
    }
}

/// Lock-free performance metrics using atomics
/// Zero overhead - no mutex contention
#[allow(dead_code)]
pub struct LockFreeMetrics {
    // Request counters
    pub total_requests: AtomicU64,
    pub success_requests: AtomicU64,
    pub error_requests: AtomicU64,

    // Time measurements
    pub total_latency_us: AtomicU64, // Microseconds
    pub min_latency_us: AtomicU64,
    pub max_latency_us: AtomicU64,

    // Cache metrics
    pub cache_hits: AtomicU64,
    pub cache_updates: AtomicU64,

    // Start time for rate calculations
    start_time: Instant,
}

#[allow(dead_code)]
impl LockFreeMetrics {
    pub fn new() -> Self {
        Self {
            total_requests: AtomicU64::new(0),
            success_requests: AtomicU64::new(0),
            error_requests: AtomicU64::new(0),
            total_latency_us: AtomicU64::new(0),
            min_latency_us: AtomicU64::new(u64::MAX),
            max_latency_us: AtomicU64::new(0),
            cache_hits: AtomicU64::new(0),
            cache_updates: AtomicU64::new(0),
            start_time: Instant::now(),
        }
    }

    /// Record successful request (lock-free)
    #[inline]
    pub fn record_success(&self, latency_us: u64) {
        self.total_requests.fetch_add(1, Ordering::Relaxed);
        self.success_requests.fetch_add(1, Ordering::Relaxed);
        self.total_latency_us
            .fetch_add(latency_us, Ordering::Relaxed);

        // Update min/max with compare-and-swap
        self.update_min(latency_us);
        self.update_max(latency_us);
    }

    /// Record error request (lock-free)
    #[inline]
    pub fn record_error(&self) {
        self.total_requests.fetch_add(1, Ordering::Relaxed);
        self.error_requests.fetch_add(1, Ordering::Relaxed);
    }

    /// Record cache hit (lock-free)
    #[inline]
    pub fn record_cache_hit(&self) {
        self.cache_hits.fetch_add(1, Ordering::Relaxed);
    }

    /// Record cache update (lock-free)
    #[inline]
    pub fn record_cache_update(&self) {
        self.cache_updates.fetch_add(1, Ordering::Relaxed);
    }

    /// Get requests per second
    pub fn requests_per_second(&self) -> f64 {
        let total = self.total_requests.load(Ordering::Relaxed);
        let elapsed = self.start_time.elapsed().as_secs_f64();

        if elapsed > 0.0 {
            total as f64 / elapsed
        } else {
            0.0
        }
    }

    /// Get average latency in microseconds
    pub fn avg_latency_us(&self) -> f64 {
        let total_latency = self.total_latency_us.load(Ordering::Relaxed);
        let success = self.success_requests.load(Ordering::Relaxed);

        if success > 0 {
            total_latency as f64 / success as f64
        } else {
            0.0
        }
    }

    /// Get error rate (0.0 - 1.0)
    pub fn error_rate(&self) -> f64 {
        let total = self.total_requests.load(Ordering::Relaxed);
        let errors = self.error_requests.load(Ordering::Relaxed);

        if total > 0 {
            errors as f64 / total as f64
        } else {
            0.0
        }
    }

    /// Get cache hit rate (0.0 - 1.0)
    pub fn cache_hit_rate(&self) -> f64 {
        let total = self.total_requests.load(Ordering::Relaxed);
        let hits = self.cache_hits.load(Ordering::Relaxed);

        if total > 0 {
            hits as f64 / total as f64
        } else {
            0.0
        }
    }

    /// Update minimum latency (lock-free with CAS)
    fn update_min(&self, value: u64) {
        let mut current = self.min_latency_us.load(Ordering::Relaxed);
        while value < current {
            match self.min_latency_us.compare_exchange_weak(
                current,
                value,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => break,
                Err(actual) => current = actual,
            }
        }
    }

    /// Update maximum latency (lock-free with CAS)
    fn update_max(&self, value: u64) {
        let mut current = self.max_latency_us.load(Ordering::Relaxed);
        while value > current {
            match self.max_latency_us.compare_exchange_weak(
                current,
                value,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => break,
                Err(actual) => current = actual,
            }
        }
    }

    /// Get min latency
    pub fn min_latency_us(&self) -> u64 {
        let min = self.min_latency_us.load(Ordering::Relaxed);
        if min == u64::MAX { 0 } else { min }
    }

    /// Get max latency
    pub fn max_latency_us(&self) -> u64 {
        self.max_latency_us.load(Ordering::Relaxed)
    }
}

impl Default for LockFreeMetrics {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_time_cache_update() {
        let cache = TimeCache::new("done".to_string(), "done (cached)".to_string());

        assert!(!cache.is_initialized());

        cache.update(1234567890000, false);

        assert!(cache.is_initialized());
        assert_eq!(cache.get_epoch(), 1234567890000);

        let json = cache.get_json(false);
        assert!(json.contains("1234567890000"));
        assert!(json.contains("done"));
    }

    #[test]
    fn test_time_cache_zero_copy() {
        let cache = TimeCache::new("ok".to_string(), "ok (stale)".to_string());
        cache.update(1000000, false);

        // Get same JSON multiple times - should be zero-copy (same Arc)
        let json1 = cache.get_json(false);
        let json2 = cache.get_json(false);

        // Arc pointers should point to same data
        assert!(Arc::ptr_eq(&json1, &json2));
    }

    #[test]
    fn test_lock_free_metrics() {
        let metrics = LockFreeMetrics::new();

        metrics.record_success(100);
        metrics.record_success(200);
        metrics.record_success(300);

        assert_eq!(metrics.total_requests.load(Ordering::Relaxed), 3);
        assert_eq!(metrics.success_requests.load(Ordering::Relaxed), 3);
        assert_eq!(metrics.avg_latency_us(), 200.0);
        assert_eq!(metrics.min_latency_us(), 100);
        assert_eq!(metrics.max_latency_us(), 300);

        metrics.record_error();
        assert_eq!(metrics.error_rate(), 0.25); // 1 error out of 4 requests
    }

    #[test]
    fn test_cache_hit_rate() {
        let metrics = LockFreeMetrics::new();

        metrics.record_success(100);
        metrics.record_cache_hit();

        metrics.record_success(100);
        metrics.record_cache_hit();

        metrics.record_success(100);
        // No cache hit for this one

        assert_eq!(metrics.cache_hit_rate(), 2.0 / 3.0);
    }
}
