use crate::ntp::SyncResult;
use crate::performance::TimeCache;
use once_cell::sync::Lazy;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::time::Instant;
use tracing::debug;

// Global reference instant for lock-free time calculations
// This is created once at program startup and never changes
static REFERENCE_INSTANT: Lazy<Instant> = Lazy::new(Instant::now);

/// Monotonic time base that avoids OS wall clock authority
/// Uses NTP-synced epoch time + monotonic clock progression
#[derive(Clone)]
pub struct TimeBase {
    /// Base NTP epoch time in milliseconds (set on successful sync)
    base_epoch_ms: Arc<AtomicI64>,

    /// Base monotonic instant as nanoseconds since REFERENCE_INSTANT (set on successful sync)
    /// PERFORMANCE: Using AtomicU64 instead of RwLock eliminates all locks in read path
    base_instant_nanos: Arc<AtomicU64>,

    /// Last served epoch_ms (for monotonic output clamping)
    last_served_ms: Arc<AtomicI64>,

    /// Whether monotonic output clamping is enabled
    monotonic_output: bool,

    /// Whether we've had at least one successful sync
    has_synced: Arc<AtomicBool>,

    /// Optional zero-copy JSON cache
    time_cache: Option<Arc<TimeCache>>,

    // ── Manual override atomics (P1-7) ────────────────────────────────────────
    /// True while a manual time override is active and not expired.
    manual_active: Arc<AtomicBool>,
    /// Manual override epoch_ms (the forced time value).
    manual_base_epoch_ms: Arc<AtomicI64>,
    /// Monotonic nanos (since REFERENCE_INSTANT) at the moment set_manual() was called.
    manual_base_instant_nanos: Arc<AtomicU64>,
    /// Monotonic nanos (since REFERENCE_INSTANT) when the override expires.
    manual_expires_at_nanos: Arc<AtomicU64>,
}

impl TimeBase {
    pub fn new(monotonic_output: bool) -> Self {
        Self {
            base_epoch_ms: Arc::new(AtomicI64::new(0)),
            base_instant_nanos: Arc::new(AtomicU64::new(0)),
            last_served_ms: Arc::new(AtomicI64::new(0)),
            monotonic_output,
            has_synced: Arc::new(AtomicBool::new(false)),
            time_cache: None,
            manual_active: Arc::new(AtomicBool::new(false)),
            manual_base_epoch_ms: Arc::new(AtomicI64::new(0)),
            manual_base_instant_nanos: Arc::new(AtomicU64::new(0)),
            manual_expires_at_nanos: Arc::new(AtomicU64::new(0)),
        }
    }

    pub fn with_cache(mut self, cache: Arc<TimeCache>) -> Self {
        self.time_cache = Some(cache);
        self
    }

    /// Update the time base with a new NTP sync result
    pub fn update(&self, sync_result: &SyncResult) {
        // CRITICAL: Use the instant from when epoch_ms was calculated, not current time
        // This prevents timing mismatches between epoch_ms and the monotonic clock

        // Convert Instant to nanoseconds offset from REFERENCE_INSTANT for atomic storage
        let instant_nanos = sync_result
            .instant
            .duration_since(*REFERENCE_INSTANT)
            .as_nanos() as u64;

        // PERFORMANCE: Use Release ordering - ensures all prior writes are visible
        // before this update becomes visible to other threads
        self.base_epoch_ms
            .store(sync_result.epoch_ms, Ordering::Release);
        self.base_instant_nanos
            .store(instant_nanos, Ordering::Release);
        self.has_synced.store(true, Ordering::Release);

        debug!(
            base_epoch_ms = sync_result.epoch_ms,
            server = %sync_result.server,
            "Updated time base"
        );
    }

    /// Get current epoch time in milliseconds.
    /// Returns None if not yet synced (and no manual override is active).
    ///
    /// Precedence: manual override (if active and not expired) → NTP synced → None.
    /// Monotonic clamping applies to ALL sources unconditionally.
    ///
    /// PERFORMANCE: This is the hot path - fully lock-free using atomics.
    pub fn now_ms(&self) -> Option<i64> {
        // ── Manual override path ─────────────────────────────────────────────
        if self.manual_active.load(Ordering::Acquire) {
            let now_nanos = Instant::now().duration_since(*REFERENCE_INSTANT).as_nanos() as u64;
            let expires_nanos = self.manual_expires_at_nanos.load(Ordering::Acquire);
            if now_nanos < expires_nanos {
                let base_nanos = self.manual_base_instant_nanos.load(Ordering::Acquire);
                let base_epoch = self.manual_base_epoch_ms.load(Ordering::Acquire);
                let elapsed_ms = (now_nanos.saturating_sub(base_nanos) / 1_000_000) as i64;
                let mut current_ms = base_epoch + elapsed_ms;
                if self.monotonic_output {
                    let last = self.last_served_ms.load(Ordering::Acquire);
                    if current_ms <= last {
                        current_ms = last + 1;
                    }
                    self.last_served_ms.store(current_ms, Ordering::Release);
                }
                return Some(current_ms);
            }
            // Lazy expiry: silently clear (background task emits the audit log)
            self.manual_active.store(false, Ordering::Release);
        }

        // ── NTP path ─────────────────────────────────────────────────────────
        if !self.has_synced.load(Ordering::Acquire) {
            return None;
        }
        let base_instant_nanos = self.base_instant_nanos.load(Ordering::Acquire);
        let base_epoch_ms = self.base_epoch_ms.load(Ordering::Acquire);
        let now_nanos = Instant::now().duration_since(*REFERENCE_INSTANT).as_nanos() as u64;
        let elapsed_nanos = now_nanos.saturating_sub(base_instant_nanos);
        let elapsed_ms = (elapsed_nanos / 1_000_000) as i64;
        let mut current_ms = base_epoch_ms + elapsed_ms;
        if self.monotonic_output {
            let last_served = self.last_served_ms.load(Ordering::Acquire);
            if current_ms <= last_served {
                current_ms = last_served + 1;
            }
            self.last_served_ms.store(current_ms, Ordering::Release);
        }
        Some(current_ms)
    }

    /// Check if we've had at least one successful sync
    pub fn has_synced(&self) -> bool {
        self.has_synced.load(Ordering::Acquire)
    }

    // ── Manual override methods (P1-7) ────────────────────────────────────────

    /// Activate a manual time override.  All writes use Release ordering so the
    /// subsequent `manual_active` store is the publication barrier.
    pub fn set_manual(&self, epoch_ms: i64, ttl_secs: u32) {
        let base_nanos = Instant::now().duration_since(*REFERENCE_INSTANT).as_nanos() as u64;
        let expires_nanos =
            base_nanos.saturating_add((ttl_secs as u64).saturating_mul(1_000_000_000));
        self.manual_base_epoch_ms.store(epoch_ms, Ordering::Release);
        self.manual_base_instant_nanos
            .store(base_nanos, Ordering::Release);
        self.manual_expires_at_nanos
            .store(expires_nanos, Ordering::Release);
        // Publish last — ensures all other stores are visible before `manual_active` flips.
        self.manual_active.store(true, Ordering::Release);
    }

    /// Deactivate the manual override.  Safe to call multiple times.
    pub fn clear_manual(&self) {
        self.manual_active.store(false, Ordering::Release);
    }

    /// Returns true if a manual override is currently active and not expired.
    /// Performs a lazy expiry check; clears the active flag if expired.
    pub fn is_manual_active(&self) -> bool {
        if !self.manual_active.load(Ordering::Acquire) {
            return false;
        }
        let now_nanos = Instant::now().duration_since(*REFERENCE_INSTANT).as_nanos() as u64;
        let expires_nanos = self.manual_expires_at_nanos.load(Ordering::Acquire);
        if now_nanos >= expires_nanos {
            self.manual_active.store(false, Ordering::Release);
            return false;
        }
        true
    }

    /// Returns the current NTP-derived epoch_ms, bypassing any active manual override.
    /// Used by the admin handler to validate jump magnitude.
    pub fn ntp_base_now_ms(&self) -> Option<i64> {
        if !self.has_synced.load(Ordering::Acquire) {
            return None;
        }
        let base_nanos = self.base_instant_nanos.load(Ordering::Acquire);
        let base_epoch = self.base_epoch_ms.load(Ordering::Acquire);
        let now_nanos = Instant::now().duration_since(*REFERENCE_INSTANT).as_nanos() as u64;
        let elapsed_ms = (now_nanos.saturating_sub(base_nanos) / 1_000_000) as i64;
        Some(base_epoch + elapsed_ms)
    }

    /// Returns milliseconds elapsed since `set_manual()` was called.
    /// Returns 0 if no override has ever been set.
    pub fn manual_age_ms(&self) -> u64 {
        let base_nanos = self.manual_base_instant_nanos.load(Ordering::Acquire);
        let now_nanos = Instant::now().duration_since(*REFERENCE_INSTANT).as_nanos() as u64;
        now_nanos.saturating_sub(base_nanos) / 1_000_000
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn create_test_sync_result(epoch_ms: i64) -> SyncResult {
        use crate::ntp::selection::TimingSource;
        SyncResult {
            epoch_ms,
            server: "test:123".to_string(),
            rtt: Duration::from_millis(10),
            instant: Instant::now(),
            offset_ms: 0,
            t1_client_send_ms: 0,
            t2_server_recv_ms: 0,
            t3_server_send_ms: 0,
            t4_client_recv_ms: 0,
            root_delay_ms: 0,
            root_dispersion_ms: 0,
            stratum: 1,
            leap: 0,
            precision_log2: 0,
            reference_id: 0,
            timing_source: TimingSource::Estimated,
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
