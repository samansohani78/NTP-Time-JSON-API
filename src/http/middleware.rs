use crate::http::state::AppState;
use axum::{
    extract::{Request, State},
    middleware::Next,
    response::Response,
};
use std::sync::Arc;
use std::time::Instant;

pub async fn track_metrics(
    State(state): State<Arc<AppState>>,
    request: Request,
    next: Next,
) -> Response {
    let start = Instant::now();
    let method = request.method().to_string();
    let path = request.uri().path().to_string();

    // Increment inflight requests
    state.metrics.http_inflight_requests.inc();

    // Process request
    let response = next.run(request).await;

    // Decrement inflight requests
    state.metrics.http_inflight_requests.dec();

    let duration = start.elapsed();
    let status = response.status().as_u16();

    // Record metrics
    state
        .metrics
        .record_http_request(&method, &path, status, duration);

    response
}
