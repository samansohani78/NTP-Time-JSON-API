pub mod handlers;
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
    create_router_internal(state, true)
}

#[cfg(test)]
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

    let router = Router::new().merge(fast_router).merge(slow_router);

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
    use crate::timebase::TimeBase;
    use axum::{body::Body, http::Request};
    use tower::ServiceExt;

    #[tokio::test]
    async fn test_router_creation() {
        use crate::performance::{LockFreeMetrics, TimeCache};

        let config = Arc::new(Config::default());
        let time_cache = Arc::new(TimeCache::new(
            config.messages.ok.clone(),
            config.messages.ok_cache.clone(),
        ));
        let perf_metrics = Arc::new(LockFreeMetrics::new());
        let timebase = TimeBase::new(true).with_cache(time_cache.clone());
        let metrics = Arc::new(Metrics::new());
        let state = Arc::new(AppState::new(
            config,
            timebase,
            metrics,
            time_cache,
            perf_metrics,
        ));

        let app = create_router_for_test(state);

        // Test healthz endpoint
        let response: axum::response::Response = app
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
}
