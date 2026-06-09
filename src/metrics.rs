use prometheus_client::encoding::EncodeLabelSet;
use prometheus_client::encoding::text::encode;
use prometheus_client::metrics::counter::Counter;
use prometheus_client::metrics::family::Family;
use prometheus_client::metrics::gauge::Gauge;
use prometheus_client::metrics::histogram::{Histogram, exponential_buckets};
use prometheus_client::registry::Registry;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;

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

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
pub struct RejectLabel {
    pub reason: String,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
pub struct ReplicaLabel {
    pub replica_id: String,
}

pub struct Metrics {
    registry: Registry,

    // HTTP metrics
    pub http_requests_total: Family<HttpLabels, Counter>,
    pub http_request_duration_seconds: Family<HttpLabels, Histogram>,
    pub http_inflight_requests: Gauge,

    // NTP client metrics
    pub ntp_sync_total: Counter,
    pub ntp_sync_errors_total: Counter,
    pub ntp_last_sync_timestamp_seconds: Gauge,
    pub ntp_staleness_seconds: Gauge,
    pub ntp_offset_seconds: Gauge<f64, AtomicU64>,
    pub ntp_rtt_seconds: Histogram,
    pub ntp_server_up: Family<ServerLabel, Gauge>,
    /// Most recent RTT for each NTP *client* server, in milliseconds.
    pub ntp_server_rtt_milliseconds: Family<ServerLabel, Gauge>,
    pub ntp_consecutive_failures: Gauge,

    // NTP server (responds to NTP clients on UDP) metrics
    pub ntp_udp_server_requests_total: Counter,
    pub ntp_udp_server_responses_total: Counter,
    pub ntp_udp_server_errors_total: Counter,
    pub ntp_udp_server_unsynced_responses_total: Counter,
    /// Most recently advertised `root_dispersion` in synced UDP NTP replies (seconds).
    pub ntp_udp_server_root_dispersion_seconds: Gauge<f64, AtomicU64>,

    // Time-quality envelope metrics (P0-4)
    /// Computed time uncertainty (ms) from the most recent sync quality snapshot.
    pub time_uncertainty_milliseconds: Gauge<f64, AtomicU64>,
    /// Encoded time source mode: 0=ntp, 1=degraded, 2=unsynced, 3=manual, 4=holdover.
    pub time_source_mode: Gauge,
    /// Encoded serve state: 0=ok, 1=degraded, 2=stopped, 3=unsynced, 4=holdover.
    pub time_serve_state: Gauge,

    // P1-6 selection metrics
    /// Number of agreers in the most recent weighted-median selection.
    pub ntp_selection_quorum_size: Gauge,
    /// Servers eliminated by a hard gate in the most recent selection.
    pub ntp_selection_falsetickers_total: Counter,
    /// Per-server λ (root distance) from the most recent selection (ms).
    pub ntp_sample_uncertainty_milliseconds: Family<ServerLabel, Gauge<f64, AtomicU64>>,
    /// Combined uncertainty of the selected result (ms); set after each sync.
    pub ntp_combined_uncertainty_milliseconds: Gauge<f64, AtomicU64>,
    /// Total samples rejected in selection, broken down by reason.
    pub ntp_selection_rejected_total: Family<RejectLabel, Counter>,
    /// 1 when the last selection had one provider group dominating the agreers.
    pub ntp_selection_single_provider: Gauge,

    // P1F-12 interval-intersection metrics
    /// Truechimer count from the most recent Marzullo sweep (0 when disabled or failed).
    pub ntp_intersection_truechimers: Gauge,
    /// Total falsetickers discarded by the interval-intersection filter across all syncs.
    pub ntp_intersection_falsetickers_total: Counter,
    /// Width of the intersection region from the most recent sync (ms); 0 if no intersection.
    pub ntp_intersection_width_milliseconds: Gauge<f64, AtomicU64>,
    /// Total intersection failures broken down by reason (no_intersection, ambiguous_cluster, …).
    pub ntp_intersection_failures_total: Family<RejectLabel, Counter>,
    /// Number of competing clusters found in the most recent sweep (≥ 2 → AmbiguousCluster).
    pub ntp_intersection_ambiguous_clusters: Gauge,

    // P1-8 replica drift visibility metrics (labeled with replica_id)
    /// Current NTP offset of this replica (ms). Populated after each sync.
    pub time_replica_offset_milliseconds: Family<ReplicaLabel, Gauge<f64, AtomicU64>>,
    /// Current combined time uncertainty of this replica (ms).
    pub time_replica_uncertainty_milliseconds: Family<ReplicaLabel, Gauge<f64, AtomicU64>>,
    /// Serve state of this replica: 0=ok, 1=degraded, 2=stopped, 3=unsynced, 4=holdover.
    pub time_replica_serve_state: Family<ReplicaLabel, Gauge>,
    /// Time source mode of this replica: 0=ntp, 1=degraded, 2=unsynced, 3=manual, 4=holdover.
    pub time_replica_source_mode: Family<ReplicaLabel, Gauge>,

    // Manual override metrics (P1-7)
    /// 1 while a manual time override is active, 0 otherwise.
    pub manual_override_active: Gauge,
    /// Total number of manual time overrides set since process start.
    pub manual_override_total: Counter,
    /// Unix timestamp (seconds) when the current override expires; 0 if none active.
    pub manual_override_expiry_timestamp_seconds: Gauge,
    /// Total override requests rejected, broken down by reason label.
    pub manual_override_rejected_total: Family<RejectLabel, Counter>,

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

        let ntp_offset_seconds = Gauge::<f64, AtomicU64>::default();
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

        let ntp_server_rtt_milliseconds = Family::<ServerLabel, Gauge>::default();
        registry.register(
            "ntp_server_rtt_milliseconds",
            "Last RTT for each NTP *client* server in milliseconds",
            ntp_server_rtt_milliseconds.clone(),
        );

        let ntp_consecutive_failures = Gauge::default();
        registry.register(
            "ntp_consecutive_failures",
            "Number of consecutive NTP sync failures",
            ntp_consecutive_failures.clone(),
        );

        // P1-6 selection metrics
        let ntp_selection_quorum_size = Gauge::default();
        registry.register(
            "ntp_selection_quorum_size",
            "Number of agreers in the most recent weighted-median NTP selection",
            ntp_selection_quorum_size.clone(),
        );

        let ntp_selection_falsetickers_total = Counter::default();
        registry.register(
            "ntp_selection_falsetickers_total",
            "Total NTP samples rejected by hard gates across all syncs",
            ntp_selection_falsetickers_total.clone(),
        );

        let ntp_sample_uncertainty_milliseconds =
            Family::<ServerLabel, Gauge<f64, AtomicU64>>::default();
        registry.register(
            "ntp_sample_uncertainty_milliseconds",
            "Per-server root-distance λ from the most recent selection (ms)",
            ntp_sample_uncertainty_milliseconds.clone(),
        );

        let ntp_combined_uncertainty_milliseconds = Gauge::<f64, AtomicU64>::default();
        registry.register(
            "ntp_combined_uncertainty_milliseconds",
            "Combined uncertainty of the selected NTP result (ms)",
            ntp_combined_uncertainty_milliseconds.clone(),
        );

        let ntp_selection_rejected_total = Family::<RejectLabel, Counter>::default();
        registry.register(
            "ntp_selection_rejected_total",
            "Total NTP samples rejected by selection hard gates, by reason",
            ntp_selection_rejected_total.clone(),
        );

        let ntp_selection_single_provider = Gauge::default();
        registry.register(
            "ntp_selection_single_provider",
            "1 when one provider group dominates the agreers (uncertainty doubled)",
            ntp_selection_single_provider.clone(),
        );

        // P1F-12 interval-intersection metrics
        let ntp_intersection_truechimers = Gauge::default();
        registry.register(
            "ntp_intersection_truechimers",
            "Truechimer count from the most recent Marzullo sweep",
            ntp_intersection_truechimers.clone(),
        );

        let ntp_intersection_falsetickers_total = Counter::default();
        registry.register(
            "ntp_intersection_falsetickers_total",
            "Total falsetickers discarded by the interval-intersection filter",
            ntp_intersection_falsetickers_total.clone(),
        );

        let ntp_intersection_width_milliseconds = Gauge::<f64, AtomicU64>::default();
        registry.register(
            "ntp_intersection_width_milliseconds",
            "Width of the intersection region from the most recent sync (ms)",
            ntp_intersection_width_milliseconds.clone(),
        );

        let ntp_intersection_failures_total = Family::<RejectLabel, Counter>::default();
        registry.register(
            "ntp_intersection_failures_total",
            "Total intersection failures by reason (no_intersection, ambiguous_cluster, …)",
            ntp_intersection_failures_total.clone(),
        );

        let ntp_intersection_ambiguous_clusters = Gauge::default();
        registry.register(
            "ntp_intersection_ambiguous_clusters",
            "Competing cluster count from the most recent Marzullo sweep (>= 2 → AmbiguousCluster)",
            ntp_intersection_ambiguous_clusters.clone(),
        );

        // NTP server (inbound UDP) metrics
        let ntp_udp_server_requests_total = Counter::default();
        registry.register(
            "ntp_udp_server_requests_total",
            "Total UDP NTP server requests received",
            ntp_udp_server_requests_total.clone(),
        );

        let ntp_udp_server_responses_total = Counter::default();
        registry.register(
            "ntp_udp_server_responses_total",
            "Total UDP NTP server responses sent",
            ntp_udp_server_responses_total.clone(),
        );

        let ntp_udp_server_errors_total = Counter::default();
        registry.register(
            "ntp_udp_server_errors_total",
            "Total UDP NTP server errors (malformed packets, send failures)",
            ntp_udp_server_errors_total.clone(),
        );

        let ntp_udp_server_unsynced_responses_total = Counter::default();
        registry.register(
            "ntp_udp_server_unsynced_responses_total",
            "UDP NTP server responses sent while the timebase was unsynced (LI=3, Stratum=16)",
            ntp_udp_server_unsynced_responses_total.clone(),
        );

        let ntp_udp_server_root_dispersion_seconds = Gauge::<f64, AtomicU64>::default();
        registry.register(
            "ntp_udp_server_root_dispersion_seconds",
            "Most recently advertised root_dispersion in synced UDP NTP replies (seconds)",
            ntp_udp_server_root_dispersion_seconds.clone(),
        );

        // Time-quality envelope metrics (P0-4)
        let time_uncertainty_milliseconds = Gauge::<f64, AtomicU64>::default();
        registry.register(
            "time_uncertainty_milliseconds",
            "Computed time uncertainty (ms) from most recent NTP sync quality (RFC 5905 §11.2)",
            time_uncertainty_milliseconds.clone(),
        );

        let time_source_mode = Gauge::default();
        registry.register(
            "time_source_mode",
            "Time source mode: 0=ntp, 1=degraded, 2=unsynced, 3=manual, 4=holdover",
            time_source_mode.clone(),
        );

        let time_serve_state = Gauge::default();
        registry.register(
            "time_serve_state",
            "Serve state: 0=ok, 1=degraded, 2=stopped, 3=unsynced, 4=holdover",
            time_serve_state.clone(),
        );

        // P1-8 replica drift visibility metrics
        let time_replica_offset_milliseconds =
            Family::<ReplicaLabel, Gauge<f64, AtomicU64>>::default();
        registry.register(
            "time_replica_offset_milliseconds",
            "NTP offset of this replica in milliseconds (updated on each sync)",
            time_replica_offset_milliseconds.clone(),
        );

        let time_replica_uncertainty_milliseconds =
            Family::<ReplicaLabel, Gauge<f64, AtomicU64>>::default();
        registry.register(
            "time_replica_uncertainty_milliseconds",
            "Combined time uncertainty of this replica in milliseconds",
            time_replica_uncertainty_milliseconds.clone(),
        );

        let time_replica_serve_state = Family::<ReplicaLabel, Gauge>::default();
        registry.register(
            "time_replica_serve_state",
            "Serve state of this replica: 0=ok, 1=degraded, 2=stopped, 3=unsynced, 4=holdover",
            time_replica_serve_state.clone(),
        );

        let time_replica_source_mode = Family::<ReplicaLabel, Gauge>::default();
        registry.register(
            "time_replica_source_mode",
            "Time source mode of this replica: 0=ntp, 1=degraded, 2=unsynced, 3=manual, 4=holdover",
            time_replica_source_mode.clone(),
        );

        // Manual override metrics (P1-7)
        let manual_override_active = Gauge::default();
        registry.register(
            "manual_override_active",
            "Whether a manual time override is currently active (0/1)",
            manual_override_active.clone(),
        );

        let manual_override_total = Counter::default();
        registry.register(
            "manual_override_total",
            "Total number of manual time overrides set since process start",
            manual_override_total.clone(),
        );

        let manual_override_expiry_timestamp_seconds = Gauge::default();
        registry.register(
            "manual_override_expiry_timestamp_seconds",
            "Unix timestamp (seconds) when the current manual override expires; 0 if none active",
            manual_override_expiry_timestamp_seconds.clone(),
        );

        let manual_override_rejected_total = Family::<RejectLabel, Counter>::default();
        registry.register(
            "manual_override_rejected_total",
            "Total manual override requests rejected, by reason",
            manual_override_rejected_total.clone(),
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
            ntp_server_rtt_milliseconds,
            ntp_consecutive_failures,
            ntp_selection_quorum_size,
            ntp_selection_falsetickers_total,
            ntp_sample_uncertainty_milliseconds,
            ntp_combined_uncertainty_milliseconds,
            ntp_selection_rejected_total,
            ntp_selection_single_provider,
            ntp_intersection_truechimers,
            ntp_intersection_falsetickers_total,
            ntp_intersection_width_milliseconds,
            ntp_intersection_failures_total,
            ntp_intersection_ambiguous_clusters,
            time_replica_offset_milliseconds,
            time_replica_uncertainty_milliseconds,
            time_replica_serve_state,
            time_replica_source_mode,
            ntp_udp_server_requests_total,
            ntp_udp_server_responses_total,
            ntp_udp_server_errors_total,
            ntp_udp_server_unsynced_responses_total,
            ntp_udp_server_root_dispersion_seconds,
            time_uncertainty_milliseconds,
            time_source_mode,
            time_serve_state,
            manual_override_active,
            manual_override_total,
            manual_override_expiry_timestamp_seconds,
            manual_override_rejected_total,
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
