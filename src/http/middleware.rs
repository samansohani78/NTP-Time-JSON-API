use crate::http::state::AppState;
use axum::{
    extract::{Request, State},
    http::{StatusCode, header::AUTHORIZATION},
    middleware::Next,
    response::Response,
};
use std::sync::Arc;
use std::time::Instant;

/// Admin auth middleware — requires `Authorization: Bearer <token>` matching
/// `config.admin.token`.  Missing and wrong tokens return an identical 401
/// body so the response is not an oracle for distinguishing the two cases.
///
/// SECURITY: The token is NEVER logged or included in any error message.
/// Comparison uses `subtle::ConstantTimeEq` to avoid timing side-channels.
pub async fn require_admin_auth(
    State(state): State<Arc<AppState>>,
    request: Request,
    next: Next,
) -> Response {
    use subtle::ConstantTimeEq;

    let provided = request
        .headers()
        .get(AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer "))
        .unwrap_or("");

    let expected = state.config.admin.token.as_bytes();
    let provided_bytes = provided.as_bytes();

    let valid: bool = if expected.len() == provided_bytes.len() {
        expected.ct_eq(provided_bytes).into()
    } else {
        // Lengths differ: still run a dummy comparison so the branch takes
        // similar time and the compiler cannot elide the constant-time path.
        let _ = expected.ct_eq(expected);
        false
    };

    if !valid {
        return Response::builder()
            .status(StatusCode::UNAUTHORIZED)
            .header("content-type", "application/json")
            .body(axum::body::Body::from(
                r#"{"status":401,"error":"Unauthorized","message":"error"}"#,
            ))
            .expect("static 401 body");
    }

    next.run(request).await
}

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
