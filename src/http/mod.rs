pub mod handlers;
pub mod handlers_admin;
pub mod middleware;
pub mod state;
pub mod websocket;

use axum::{Router, http::StatusCode, middleware as axum_middleware, routing::get};
use state::AppState;
use std::sync::Arc;
use std::time::Duration;
use tower_governor::{GovernorLayer, governor::GovernorConfigBuilder};
use tower_http::{
    cors::{Any, CorsLayer},
    limit::RequestBodyLimitLayer,
    timeout::TimeoutLayer,
    trace::{DefaultMakeSpan, DefaultOnResponse, TraceLayer},
};
use tracing::Level;

pub fn create_router(state: Arc<AppState>) -> Router {
    let enable_rate_limiting = !state.config.http.disable_rate_limiting;
    create_router_internal(state, enable_rate_limiting)
}

pub fn create_router_for_test(state: Arc<AppState>) -> Router {
    create_router_internal(state, false)
}

fn create_router_internal(state: Arc<AppState>, enable_rate_limiting: bool) -> Router {
    let config = &state.config;

    // PERFORMANCE: Fast path - NO middleware for hot endpoints
    // This eliminates tracing, metrics, timeout, and body limit overhead
    // Expected: 20-30% latency reduction on /time endpoint
    let fast_router = Router::new()
        .route("/time", get(handlers::time_handler))
        .route("/", get(handlers::time_handler)) // Alias
        .with_state(state.clone());

    // Slow path - full middleware stack for less critical endpoints
    let slow_router = Router::new()
        // WebSocket endpoint
        .route("/stream", get(websocket::websocket_handler))
        // Probe endpoints (Kubernetes probes don't need full middleware)
        .route("/healthz", get(handlers::healthz_handler))
        .route("/readyz", get(handlers::readyz_handler))
        .route("/startupz", get(handlers::startupz_handler))
        // Metrics (needs full stack for monitoring)
        .route("/metrics", get(handlers::metrics_handler))
        .route("/performance", get(handlers::performance_handler))
        // Time-quality envelope endpoints (P0-4)
        .route("/time/full", get(handlers::time_full_handler))
        .route("/status", get(handlers::status_handler))
        .with_state(state.clone())
        // Middleware - applied bottom-up
        .layer(axum_middleware::from_fn_with_state(
            state.clone(),
            middleware::track_metrics,
        ))
        .layer(RequestBodyLimitLayer::new(config.http.body_limit_bytes))
        .layer(TimeoutLayer::with_status_code(
            StatusCode::REQUEST_TIMEOUT,
            config.request_timeout(),
        ))
        .layer(
            TraceLayer::new_for_http()
                .make_span_with(DefaultMakeSpan::new().level(Level::INFO))
                .on_response(DefaultOnResponse::new().level(Level::INFO)),
        );

    // CORS configuration - allow all origins for public time API
    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods(Any)
        .allow_headers(Any)
        .max_age(Duration::from_secs(3600));

    // Admin router — only registered when ADMIN_API_ENABLED=true.
    // If disabled, /admin/* routes return 404 (not 401), per security contract.
    let router = if config.admin.enabled {
        let admin_router = Router::new()
            .route(
                "/admin/time/override",
                get(handlers_admin::get_override)
                    .post(handlers_admin::post_override)
                    .delete(handlers_admin::delete_override),
            )
            .with_state(state.clone())
            .layer(axum_middleware::from_fn_with_state(
                state.clone(),
                middleware::require_admin_auth,
            ))
            .layer(RequestBodyLimitLayer::new(config.http.body_limit_bytes));
        Router::new()
            .merge(fast_router)
            .merge(slow_router)
            .merge(admin_router)
    } else {
        Router::new().merge(fast_router).merge(slow_router)
    };

    // Apply rate limiting in production only (requires real IP addresses)
    let router = if enable_rate_limiting {
        // Rate limiting configuration (1000 req/sec per IP, burst of 100)
        let governor_conf = Arc::new(
            GovernorConfigBuilder::default()
                .per_second(1000)
                .burst_size(100)
                .finish()
                .unwrap(),
        );
        router.layer(GovernorLayer::new(governor_conf))
    } else {
        router
    };

    router.layer(cors)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::metrics::Metrics;
    use crate::ntp::SyncResult;
    use crate::performance::{LockFreeMetrics, TimeCache};
    use crate::timebase::TimeBase;
    use axum::{body::Body, body::to_bytes, http::Request};
    use std::time::{Duration, Instant};
    use tokio::net::UdpSocket;
    use tower::ServiceExt;

    // ── Helpers ──────────────────────────────────────────────────────────────

    /// Build a test `AppState` with the default config (REQUIRE_SYNC=true).
    fn make_state() -> Arc<AppState> {
        make_state_with_config(Arc::new(Config::default()))
    }

    fn make_state_with_config(config: Arc<Config>) -> Arc<AppState> {
        let time_cache = Arc::new(TimeCache::new(
            config.messages.ok.clone(),
            config.messages.ok_cache.clone(),
        ));
        let perf_metrics = Arc::new(LockFreeMetrics::new());
        let timebase = TimeBase::new(config.ntp.require_sync).with_cache(time_cache.clone());
        let metrics = Arc::new(Metrics::new());
        Arc::new(AppState::new(
            config,
            timebase,
            metrics,
            time_cache,
            perf_metrics,
        ))
    }

    /// Start a mock UDP NTP server that always returns `epoch_ms` as the
    /// current time. Returns the bound address and a join handle.
    async fn start_mock_ntp_server(
        epoch_ms: i64,
    ) -> (std::net::SocketAddr, tokio::task::JoinHandle<()>) {
        use crate::ntp::protocol::{
            LI_NO_WARNING, NTP_VERSION, NtpPacket, parse_packet, serialize_packet, unix_ms_to_ntp,
        };

        let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let addr = socket.local_addr().unwrap();

        let handle = tokio::spawn(async move {
            let mut buf = [0u8; 1024];
            loop {
                let Ok((len, peer)) = socket.recv_from(&mut buf).await else {
                    break;
                };
                let Ok(request) = parse_packet(&buf[..len]) else {
                    continue;
                };
                let ntp_ts = unix_ms_to_ntp(epoch_ms);
                let response = NtpPacket {
                    li: LI_NO_WARNING,
                    vn: NTP_VERSION,
                    mode: 4, // server
                    stratum: 1,
                    poll: request.poll,
                    precision: -20,
                    root_delay: 0,
                    root_dispersion: 0,
                    reference_id: u32::from_be_bytes(*b"GPS "),
                    ref_timestamp: ntp_ts,
                    origin_timestamp: request.transmit_timestamp,
                    receive_timestamp: ntp_ts,
                    transmit_timestamp: ntp_ts,
                };
                let wire = serialize_packet(&response);
                let _ = socket.send_to(&wire, peer).await;
            }
        });

        (addr, handle)
    }

    // ── Tests ─────────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_router_creation() {
        let app = create_router_for_test(make_state());
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/healthz")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
    }

    #[tokio::test]
    async fn test_time_before_sync_returns_503() {
        let state = make_state(); // REQUIRE_SYNC=true, not synced
        let app = create_router_for_test(state);
        let response = app
            .oneshot(Request::builder().uri("/time").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), 503);
    }

    #[tokio::test]
    async fn test_readyz_before_sync_returns_503() {
        let state = make_state();
        let app = create_router_for_test(state);
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/readyz")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), 503);
    }

    #[tokio::test]
    async fn test_startupz_before_sync_returns_503() {
        let state = make_state();
        let app = create_router_for_test(state);
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/startupz")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), 503);
    }

    /// Full pipeline: mock NTP server → sync → /time returns 200 with the
    /// correct epoch. Uses a real UDP socket and the in-house PacketNtpClient.
    #[tokio::test]
    async fn test_time_after_sync_returns_correct_epoch() {
        use crate::ntp::sync::NtpSyncer;

        // Known timestamp: 2024-01-01T00:00:00Z
        let fixed_epoch_ms: i64 = 1_704_067_200_000;

        let (ntp_addr, ntp_handle) = start_mock_ntp_server(fixed_epoch_ms).await;

        // Build NtpConfig pointing at the mock server.
        let mut config = Config::default();
        config.ntp.servers = vec![ntp_addr.to_string()];
        config.ntp.timeout_secs = 5;
        config.ntp.require_sync = true;
        config.ntp.selection.min_quorum = 1;
        let config = Arc::new(config);

        let state = make_state_with_config(config.clone());
        let syncer = NtpSyncer::new(config.ntp.clone().into());

        // Drive one real NTP sync.
        let outcome = syncer
            .sync()
            .await
            .expect("NTP sync should succeed against mock");

        // The epoch returned may differ slightly from `fixed_epoch_ms`
        // due to RTT and offset calculation, but should be within a few seconds.
        assert!(
            (outcome.result.epoch_ms - fixed_epoch_ms).abs() < 5_000,
            "epoch_ms {} too far from expected {}",
            outcome.result.epoch_ms,
            fixed_epoch_ms
        );

        // Update the timebase.
        state.timebase.update(&outcome.result);
        state.record_sync_success();

        // /time should now return 200.
        let app = create_router_for_test(state);
        let response = app
            .oneshot(Request::builder().uri("/time").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), 200);

        let body = to_bytes(response.into_body(), 256).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["status"], 200);
        let returned_epoch = json["data"].as_i64().unwrap_or(0);
        assert!(returned_epoch > 0, "epoch must be positive");
        assert!(
            (returned_epoch - fixed_epoch_ms).abs() < 5_000,
            "returned epoch {} too far from expected {}",
            returned_epoch,
            fixed_epoch_ms
        );

        ntp_handle.abort();
    }

    /// After a sync, /readyz and /startupz must return 200.
    #[tokio::test]
    async fn test_probes_return_200_after_sync() {
        use crate::ntp::selection::TimingSource;
        let state = make_state();

        // Inject a sync result directly without going through the network.
        let sync_result = SyncResult {
            epoch_ms: 1_700_000_000_000,
            server: "test:123".into(),
            rtt: Duration::from_millis(5),
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
        };
        state.timebase.update(&sync_result);
        state.record_sync_success();

        let app = create_router_for_test(state);

        for path in ["/readyz", "/startupz"] {
            let response = app
                .clone()
                .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(response.status(), 200, "{path} should be 200 after sync");
        }
    }

    /// /time values must be non-decreasing across sequential requests (monotonic).
    #[tokio::test]
    async fn test_time_is_monotonic() {
        use crate::ntp::selection::TimingSource;
        let state = make_state();

        let sync_result = SyncResult {
            epoch_ms: 1_700_000_000_000,
            server: "test:123".into(),
            rtt: Duration::from_millis(5),
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
        };
        state.timebase.update(&sync_result);
        state.record_sync_success();

        let mut prev_epoch: i64 = 0;
        for _ in 0..10 {
            let app = create_router_for_test(state.clone());
            let response = app
                .oneshot(Request::builder().uri("/time").body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(response.status(), 200);

            let body = to_bytes(response.into_body(), 256).await.unwrap();
            let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
            let epoch = json["data"].as_i64().unwrap_or(0);

            assert!(
                epoch >= prev_epoch,
                "time went backwards: {} < {}",
                epoch,
                prev_epoch
            );
            prev_epoch = epoch;
        }
    }

    /// /metrics endpoint must include the always-present Prometheus metric
    /// families. `http_requests_total` is a Family that only appears once
    /// a request goes through the tracking middleware, so we don't assert
    /// on it here.
    #[tokio::test]
    async fn test_metrics_endpoint_contains_required_families() {
        let state = make_state();
        let app = create_router_for_test(state);
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/metrics")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), 200);

        let body = to_bytes(response.into_body(), 8192).await.unwrap();
        let text = std::str::from_utf8(&body).unwrap();
        // These counters are registered unconditionally in Metrics::new(),
        // so they must appear even before any request is processed.
        for metric in &["build_info", "ntp_sync_total", "ntp_staleness_seconds"] {
            assert!(text.contains(metric), "metrics output missing {metric}");
        }
    }

    /// /performance endpoint returns 200 with the expected JSON structure.
    /// The response shape is: `{"status": "ok", "metrics": {"requests": {...}, ...}}`.
    #[tokio::test]
    async fn test_performance_endpoint_structure() {
        let state = make_state();
        let app = create_router_for_test(state);
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/performance")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), 200);

        let body = to_bytes(response.into_body(), 2048).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let metrics = &json["metrics"];
        assert!(metrics.is_object(), "missing top-level 'metrics' key");
        assert!(metrics["requests"].is_object(), "missing 'requests' key");
        assert!(
            metrics["latency_microseconds"].is_object(),
            "missing 'latency_microseconds' key"
        );
        assert!(metrics["cache"].is_object(), "missing 'cache' key");
        // ntp_timing is null before first sync
        assert!(
            json["ntp_timing"].is_null(),
            "ntp_timing should be null before sync"
        );
    }

    /// After a successful sync, /performance must include the RFC 5905
    /// four-tuple and packet-level fields in the `ntp_timing` object.
    #[tokio::test]
    async fn test_performance_includes_ntp_timing_after_sync() {
        use crate::http::state::NtpTimingSummary;
        use crate::ntp::selection::TimingSource;

        let state = make_state();

        // Inject timing data directly (simulating what sync_loop does).
        *state.last_ntp_timing.write() = Some(NtpTimingSummary {
            server: "ntp.test:123".into(),
            t1_client_send_ms: 1_700_000_001_000,
            t2_server_recv_ms: 1_700_000_001_010,
            t3_server_send_ms: 1_700_000_001_011,
            t4_client_recv_ms: 1_700_000_001_021,
            offset_ms: 5,
            rtt_ms: 21,
            root_delay_ms: 10,
            root_dispersion_ms: 5,
            stratum: 2,
            leap: 0,
            precision_log2: -20,
            reference_id: u32::from_be_bytes(*b"GPS "),
            timing_source: TimingSource::Measured,
        });

        let app = create_router_for_test(state);
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/performance")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), 200);

        let body = to_bytes(response.into_body(), 2048).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();

        let timing = &json["ntp_timing"];
        assert!(
            timing.is_object(),
            "ntp_timing must be an object after sync"
        );
        assert_eq!(timing["server"], "ntp.test:123");
        assert_eq!(timing["t1_client_send_ms"], 1_700_000_001_000_i64);
        assert_eq!(timing["t4_client_recv_ms"], 1_700_000_001_021_i64);
        assert_eq!(timing["offset_ms"], 5_i64);
        assert_eq!(timing["rtt_ms"], 21_u64);
        // Packet-level fields must be present after P0-2
        assert_eq!(timing["root_delay_ms"], 10_u32);
        assert_eq!(timing["root_dispersion_ms"], 5_u32);
        assert_eq!(timing["stratum"], 2_u8);
        assert_eq!(timing["leap"], 0_u8);
        assert_eq!(timing["precision_log2"], -20_i8);
        assert_eq!(timing["timing_source"], "measured");
    }

    // ── P0-4: /status and /time/full endpoint tests ───────────────────────────

    fn inject_quality(state: &AppState, upstream_dispersion_ms: u32) {
        use crate::ntp::SyncQuality;
        *state.last_sync_quality.write() = Some(SyncQuality {
            upstream_root_delay_ms: 10,
            upstream_root_dispersion_ms: upstream_dispersion_ms,
            precision_log2: -10,
            stratum: 2,
            leap: 0,
            measured_rtt_ms: 5,
            jitter_ms: 0,
            offset_ms: 1,
            last_sync_instant: std::time::Instant::now(),
            selected_server: "ntp.test:123".into(),
        });
        state.record_sync_success();
    }

    #[tokio::test]
    async fn status_returns_200_with_full_envelope() {
        use crate::ntp::selection::TimingSource;

        let state = make_state();
        let sync_result = SyncResult {
            epoch_ms: 1_700_000_000_000,
            server: "ntp.test:123".into(),
            rtt: Duration::from_millis(5),
            instant: Instant::now(),
            offset_ms: 0,
            t1_client_send_ms: 0,
            t2_server_recv_ms: 0,
            t3_server_send_ms: 0,
            t4_client_recv_ms: 0,
            root_delay_ms: 10,
            root_dispersion_ms: 1,
            stratum: 2,
            leap: 0,
            precision_log2: -10,
            reference_id: 0,
            timing_source: TimingSource::Measured,
        };
        state.timebase.update(&sync_result);
        inject_quality(&state, 1);

        let app = create_router_for_test(state);
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/status")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), 200);

        let body = to_bytes(response.into_body(), 1024).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();

        assert!(json["source"].is_string(), "source field missing");
        assert!(json["serve_state"].is_string(), "serve_state field missing");
        assert!(json["ntp_synced"].as_bool().unwrap());
        // uncertainty_ms, staleness_ms, stratum, selected_server, leap must be present (not null)
        assert!(
            !json["uncertainty_ms"].is_null(),
            "uncertainty_ms should not be null after sync"
        );
        assert_eq!(json["stratum"], 2);
        assert_eq!(json["selected_server"], "ntp.test:123");
    }

    #[tokio::test]
    async fn status_unsynced_reports_unsynced_fields() {
        let state = make_state();
        // No sync done
        let app = create_router_for_test(state);
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/status")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), 200, "/status always returns 200");

        let body = to_bytes(response.into_body(), 1024).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();

        assert_eq!(json["source"], "unsynced");
        assert_eq!(json["serve_state"], "unsynced");
        assert!(json["uncertainty_ms"].is_null());
        assert!(!json["ntp_synced"].as_bool().unwrap());
    }

    #[tokio::test]
    async fn time_full_returns_enriched_json_after_sync() {
        use crate::ntp::selection::TimingSource;

        let state = make_state();
        let sync_result = SyncResult {
            epoch_ms: 1_700_000_000_000,
            server: "ntp.test:123".into(),
            rtt: Duration::from_millis(5),
            instant: Instant::now(),
            offset_ms: 0,
            t1_client_send_ms: 0,
            t2_server_recv_ms: 0,
            t3_server_send_ms: 0,
            t4_client_recv_ms: 0,
            root_delay_ms: 10,
            root_dispersion_ms: 1,
            stratum: 2,
            leap: 0,
            precision_log2: -10,
            reference_id: 0,
            timing_source: TimingSource::Measured,
        };
        state.timebase.update(&sync_result);
        inject_quality(&state, 1);

        let app = create_router_for_test(state);
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/time/full")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), 200);

        let body = to_bytes(response.into_body(), 1024).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();

        // Standard /time fields must be present
        assert_eq!(json["status"], 200);
        assert!(json["data"].as_i64().unwrap_or(0) > 0);
        assert!(json["message"].is_string());
        // Quality fields must be present
        assert!(json["source"].is_string());
        assert!(json["serve_state"].is_string());
        assert!(!json["uncertainty_ms"].is_null());
        assert_eq!(json["stratum"], 2);
    }

    #[tokio::test]
    async fn time_headers_present_after_sync() {
        use crate::ntp::selection::TimingSource;

        let state = make_state();
        let sync_result = SyncResult {
            epoch_ms: 1_700_000_000_000,
            server: "ntp.test:123".into(),
            rtt: Duration::from_millis(5),
            instant: Instant::now(),
            offset_ms: 0,
            t1_client_send_ms: 0,
            t2_server_recv_ms: 0,
            t3_server_send_ms: 0,
            t4_client_recv_ms: 0,
            root_delay_ms: 10,
            root_dispersion_ms: 1,
            stratum: 2,
            leap: 0,
            precision_log2: -10,
            reference_id: 0,
            timing_source: TimingSource::Measured,
        };
        state.timebase.update(&sync_result);
        inject_quality(&state, 1);

        let app = create_router_for_test(state);
        let response = app
            .oneshot(Request::builder().uri("/time").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), 200);

        let headers = response.headers();
        assert!(
            headers.contains_key("x-time-source"),
            "x-time-source header missing from /time"
        );
        assert!(
            headers.contains_key("x-time-serve-state"),
            "x-time-serve-state header missing"
        );
        assert!(
            headers.contains_key("x-time-uncertainty-ms"),
            "x-time-uncertainty-ms missing"
        );
        assert!(
            headers.contains_key("x-time-stratum"),
            "x-time-stratum missing"
        );
        assert!(
            headers.contains_key("x-time-staleness-ms"),
            "x-time-staleness-ms missing"
        );
        assert!(
            headers.contains_key("x-time-selected-server"),
            "x-time-selected-server missing"
        );
    }
}
