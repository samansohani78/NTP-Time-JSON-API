use prometheus_client::encoding::EncodeLabelSet;
use prometheus_client::encoding::text::encode;
use prometheus_client::metrics::counter::Counter;
use prometheus_client::metrics::family::Family;
use prometheus_client::metrics::gauge::Gauge;
use prometheus_client::metrics::histogram::{Histogram, exponential_buckets};
use prometheus_client::registry::Registry;
use std::sync::Arc;

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
pub struct HttpLabels {
    pub method: String,
    pub path: String,
    pub status: String,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
pub struct ServerLabel {
    pub server: String,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
pub struct BuildInfoLabels {
    pub version: String,
    pub git_sha: String,
}

pub struct Metrics {
    registry: Registry,

    // HTTP metrics
    pub http_requests_total: Family<HttpLabels, Counter>,
    pub http_request_duration_seconds: Family<HttpLabels, Histogram>,
    pub http_inflight_requests: Gauge,

    // NTP metrics
    pub ntp_sync_total: Counter,
    pub ntp_sync_errors_total: Counter,
    pub ntp_last_sync_timestamp_seconds: Gauge,
    pub ntp_staleness_seconds: Gauge,
    #[allow(dead_code)]
    pub ntp_offset_seconds: Gauge,
    pub ntp_rtt_seconds: Histogram,
    pub ntp_server_up: Family<ServerLabel, Gauge>,
    pub ntp_server_rtt_seconds: Family<ServerLabel, Gauge>,
    pub ntp_consecutive_failures: Gauge,

    // Build info
    #[allow(dead_code)]
    pub build_info: Family<BuildInfoLabels, Gauge>,
}

impl Metrics {
    pub fn new() -> Self {
        let mut registry = Registry::default();

        // HTTP metrics
        let http_requests_total = Family::<HttpLabels, Counter>::default();
        registry.register(
            "http_requests_total",
            "Total number of HTTP requests",
            http_requests_total.clone(),
        );

        let http_request_duration_seconds =
            Family::<HttpLabels, Histogram>::new_with_constructor(|| {
                Histogram::new(
                    exponential_buckets(0.001, 2.0, 10), // 1ms to ~1s
                )
            });
        registry.register(
            "http_request_duration_seconds",
            "HTTP request duration in seconds",
            http_request_duration_seconds.clone(),
        );

        let http_inflight_requests = Gauge::default();
        registry.register(
            "http_inflight_requests",
            "Number of HTTP requests currently being processed",
            http_inflight_requests.clone(),
        );

        // NTP metrics
        let ntp_sync_total = Counter::default();
        registry.register(
            "ntp_sync_total",
            "Total number of NTP sync attempts",
            ntp_sync_total.clone(),
        );

        let ntp_sync_errors_total = Counter::default();
        registry.register(
            "ntp_sync_errors_total",
            "Total number of failed NTP sync attempts",
            ntp_sync_errors_total.clone(),
        );

        let ntp_last_sync_timestamp_seconds = Gauge::default();
        registry.register(
            "ntp_last_sync_timestamp_seconds",
            "Unix timestamp of last successful NTP sync",
            ntp_last_sync_timestamp_seconds.clone(),
        );

        let ntp_staleness_seconds = Gauge::default();
        registry.register(
            "ntp_staleness_seconds",
            "Seconds since last successful NTP sync",
            ntp_staleness_seconds.clone(),
        );

        let ntp_offset_seconds = Gauge::default();
        registry.register(
            "ntp_offset_seconds",
            "Current NTP time offset in seconds",
            ntp_offset_seconds.clone(),
        );

        let ntp_rtt_seconds = Histogram::new(
            exponential_buckets(0.001, 2.0, 10), // 1ms to ~1s
        );
        registry.register(
            "ntp_rtt_seconds",
            "NTP round-trip time in seconds",
            ntp_rtt_seconds.clone(),
        );

        let ntp_server_up = Family::<ServerLabel, Gauge>::default();
        registry.register(
            "ntp_server_up",
            "Whether NTP server is considered healthy (1=up, 0=down)",
            ntp_server_up.clone(),
        );

        let ntp_server_rtt_seconds = Family::<ServerLabel, Gauge>::default();
        registry.register(
            "ntp_server_rtt_milliseconds",
            "Last RTT for each NTP server in milliseconds",
            ntp_server_rtt_seconds.clone(),
        );

        let ntp_consecutive_failures = Gauge::default();
        registry.register(
            "ntp_consecutive_failures",
            "Number of consecutive NTP sync failures",
            ntp_consecutive_failures.clone(),
        );

        // Build info
        let build_info = Family::<BuildInfoLabels, Gauge>::default();
        registry.register("build_info", "Build information", build_info.clone());

        // Set build info
        let version = env!("CARGO_PKG_VERSION").to_string();
        let git_sha = option_env!("GIT_SHA").unwrap_or("unknown").to_string();
        build_info
            .get_or_create(&BuildInfoLabels { version, git_sha })
            .set(1);

        Self {
            registry,
            http_requests_total,
            http_request_duration_seconds,
            http_inflight_requests,
            ntp_sync_total,
            ntp_sync_errors_total,
            ntp_last_sync_timestamp_seconds,
            ntp_staleness_seconds,
            ntp_offset_seconds,
            ntp_rtt_seconds,
            ntp_server_up,
            ntp_server_rtt_seconds,
            ntp_consecutive_failures,
            build_info,
        }
    }

    pub fn encode(&self) -> String {
        let mut buffer = String::new();
        encode(&mut buffer, &self.registry).unwrap();
        buffer
    }

    pub fn record_http_request(
        &self,
        method: &str,
        path: &str,
        status: u16,
        duration: std::time::Duration,
    ) {
        let labels = HttpLabels {
            method: method.to_string(),
            path: path.to_string(),
            status: status.to_string(),
        };

        self.http_requests_total.get_or_create(&labels).inc();
        self.http_request_duration_seconds
            .get_or_create(&labels)
            .observe(duration.as_secs_f64());
    }
}

impl Default for Metrics {
    fn default() -> Self {
        Self::new()
    }
}

pub type SharedMetrics = Arc<Metrics>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_metrics_creation() {
        let metrics = Metrics::new();
        let encoded = metrics.encode();

        // Should contain build_info
        assert!(encoded.contains("build_info"));
    }

    #[test]
    fn test_http_metrics() {
        let metrics = Metrics::new();

        metrics.record_http_request("GET", "/time", 200, std::time::Duration::from_millis(10));

        let encoded = metrics.encode();
        assert!(encoded.contains("http_requests_total"));
        assert!(encoded.contains("http_request_duration_seconds"));
    }

    #[test]
    fn test_ntp_metrics() {
        let metrics = Metrics::new();

        metrics.ntp_sync_total.inc();
        metrics.ntp_staleness_seconds.set(30);

        let encoded = metrics.encode();
        assert!(encoded.contains("ntp_sync_total"));
        assert!(encoded.contains("ntp_staleness_seconds"));
    }
}
