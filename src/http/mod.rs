pub mod handlers;
pub mod middleware;
pub mod state;

use axum::{Router, http::StatusCode, middleware as axum_middleware, routing::get};
use state::AppState;
use std::sync::Arc;
use tower_http::{
    limit::RequestBodyLimitLayer,
    timeout::TimeoutLayer,
    trace::{DefaultMakeSpan, DefaultOnResponse, TraceLayer},
};
use tracing::Level;

pub fn create_router(state: Arc<AppState>) -> Router {
    let config = &state.config;

    Router::new()
        // Main endpoints
        .route("/time", get(handlers::time_handler))
        .route("/", get(handlers::time_handler)) // Alias
        // Probe endpoints
        .route("/healthz", get(handlers::healthz_handler))
        .route("/readyz", get(handlers::readyz_handler))
        .route("/startupz", get(handlers::startupz_handler))
        // Metrics
        .route("/metrics", get(handlers::metrics_handler))
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
        )
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
        let config = Arc::new(Config::default());
        let timebase = TimeBase::new(true);
        let metrics = Arc::new(Metrics::new());
        let state = Arc::new(AppState::new(config, timebase, metrics));

        let app = create_router(state);

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
