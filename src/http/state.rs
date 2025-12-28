use crate::config::Config;
use crate::metrics::SharedMetrics;
use crate::timebase::TimeBase;
use std::sync::Arc;
use std::time::Instant;

#[derive(Clone)]
pub struct AppState {
    pub config: Arc<Config>,
    pub timebase: TimeBase,
    pub metrics: SharedMetrics,
    pub last_sync_time: Arc<parking_lot::RwLock<Option<Instant>>>,
    pub consecutive_failures: Arc<parking_lot::RwLock<u32>>,
}

impl AppState {
    pub fn new(config: Arc<Config>, timebase: TimeBase, metrics: SharedMetrics) -> Self {
        Self {
            config,
            timebase,
            metrics,
            last_sync_time: Arc::new(parking_lot::RwLock::new(None)),
            consecutive_failures: Arc::new(parking_lot::RwLock::new(0)),
        }
    }

    pub fn record_sync_success(&self) {
        *self.last_sync_time.write() = Some(Instant::now());
        *self.consecutive_failures.write() = 0;
    }

    pub fn record_sync_failure(&self) {
        *self.consecutive_failures.write() += 1;
    }

    pub fn get_staleness_seconds(&self) -> Option<u64> {
        self.last_sync_time
            .read()
            .as_ref()
            .map(|t| t.elapsed().as_secs())
    }

    pub fn get_consecutive_failures(&self) -> u32 {
        *self.consecutive_failures.read()
    }
}
