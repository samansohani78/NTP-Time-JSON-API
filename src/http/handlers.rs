use super::state::AppState;
use crate::errors::AppError;
use axum::{
    Json,
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::Instant;

/// GET /time - Returns current NTP-derived time.
///
/// Returns `Result<Response, AppError>`. The `AppError::NotSynced`
/// variant triggers a 503 with the configured `MSG_ERROR` /
/// `ERROR_TEXT_NO_SYNC` strings, preserving the pre-refactor JSON
/// shape exactly. Success paths (real NTP time and the
/// `REQUIRE_SYNC=false` system-clock fallback) build 200 responses
/// inline.
pub async fn time_handler(State(state): State<Arc<AppState>>) -> Result<Response, AppError> {
    let start = Instant::now();

    // Build the response (or error) based on timebase state.
    let result: Result<Response, AppError> = match state.timebase.now_ms() {
        Some(epoch_ms) => Ok(build_time_response(&state, epoch_ms)),
        None if state.config.ntp.require_sync => Err(AppError::NotSynced {
            message: state.config.messages.error.clone(),
            error: state.config.messages.error_no_sync.clone(),
        }),
        None => Ok(build_system_clock_response(&state)),
    };

    // Record perf metrics. 2xx → success; 5xx (NotSynced) → error.
    let latency_us = start.elapsed().as_micros() as u64;
    match &result {
        Ok(_) => state.perf_metrics.record_success(latency_us),
        Err(_) => state.perf_metrics.record_error(),
    }

    result
}

/// Build the 200 OK response for the synced path. Uses the
/// pre-serialized JSON cache (zero-copy via `Arc<String>`) so the
/// hot path stays fast.
fn build_time_response(state: &AppState, epoch_ms: i64) -> Response {
    // Determine if serving from cache (staleness > max_staleness_secs)
    let is_stale = state
        .get_staleness_seconds()
        .map(|s| s > state.config.ntp.max_staleness_secs)
        .unwrap_or(false);

    // PERFORMANCE: Update cache with current time, then get
    // pre-serialized JSON. This avoids json!() macro and serde
    // overhead on the hot path.
    state.time_cache.update(epoch_ms, is_stale);
    let json_body = state.time_cache.get_json(is_stale);

    axum::response::Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "application/json")
        .body(axum::body::Body::from((*json_body).clone()))
        .expect("failed to build /time response")
}

/// Build the 200 OK response for the `REQUIRE_SYNC=false` fallback,
/// where the service reports the OS wall clock instead of the
/// NTP-derived time. Defeats the "NTP-authoritative" design but
/// useful for development; never enabled in production.
fn build_system_clock_response(state: &AppState) -> Response {
    let epoch_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);

    let body = json!({
        "message": &state.config.messages.ok,
        "status": 200,
        "data": epoch_ms,
    });

    (StatusCode::OK, Json(body)).into_response()
}

/// GET /healthz - Liveness probe
pub async fn healthz_handler() -> (StatusCode, Json<Value>) {
    (
        StatusCode::OK,
        Json(json!({
            "status": "ok"
        })),
    )
}

/// GET /readyz - Readiness probe
pub async fn readyz_handler(State(state): State<Arc<AppState>>) -> (StatusCode, Json<Value>) {
    // Before first sync: not ready (if REQUIRE_SYNC=true)
    // After first sync: always ready (even if NTP later fails)
    if state.config.ntp.require_sync && !state.timebase.has_synced() {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({
                "status": "not_ready",
                "reason": "not_yet_synced"
            })),
        );
    }

    (
        StatusCode::OK,
        Json(json!({
            "status": "ready"
        })),
    )
}

/// GET /startupz - Startup probe
pub async fn startupz_handler(State(state): State<Arc<AppState>>) -> (StatusCode, Json<Value>) {
    // With REQUIRE_SYNC=true: return 503 until first successful sync
    // With REQUIRE_SYNC=false: return 200 once HTTP server is up
    if state.config.ntp.require_sync && !state.timebase.has_synced() {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({
                "status": "not_ready",
                "reason": "startup_in_progress"
            })),
        );
    }

    (
        StatusCode::OK,
        Json(json!({
            "status": "ready"
        })),
    )
}

/// GET /metrics - Prometheus metrics
pub async fn metrics_handler(State(state): State<Arc<AppState>>) -> String {
    state.metrics.encode()
}

/// GET /performance - Advanced performance metrics
pub async fn performance_handler(State(state): State<Arc<AppState>>) -> (StatusCode, Json<Value>) {
    let perf = &state.perf_metrics;

    let total = perf
        .total_requests
        .load(std::sync::atomic::Ordering::Relaxed);
    let success = perf
        .success_requests
        .load(std::sync::atomic::Ordering::Relaxed);
    let errors = perf
        .error_requests
        .load(std::sync::atomic::Ordering::Relaxed);
    let cache_hits = perf.cache_hits.load(std::sync::atomic::Ordering::Relaxed);
    let total_latency = perf
        .total_latency_us
        .load(std::sync::atomic::Ordering::Relaxed);
    let min_latency = perf.min_latency_us();
    let max_latency = perf.max_latency_us();

    let avg_latency_us = if success > 0 {
        total_latency as f64 / success as f64
    } else {
        0.0
    };

    let cache_hit_rate = if total > 0 {
        cache_hits as f64 / total as f64
    } else {
        0.0
    };

    let error_rate = if total > 0 {
        errors as f64 / total as f64
    } else {
        0.0
    };

    (
        StatusCode::OK,
        Json(json!({
            "status": "ok",
            "metrics": {
                "requests": {
                    "total": total,
                    "success": success,
                    "errors": errors,
                },
                "latency_microseconds": {
                    "min": min_latency,
                    "avg": format!("{:.2}", avg_latency_us),
                    "max": max_latency,
                },
                "latency_milliseconds": {
                    "min": format!("{:.3}", min_latency as f64 / 1000.0),
                    "avg": format!("{:.3}", avg_latency_us / 1000.0),
                    "max": format!("{:.3}", max_latency as f64 / 1000.0),
                },
                "cache": {
                    "hits": cache_hits,
                    "hit_rate": format!("{:.4}", cache_hit_rate),
                },
                "rates": {
                    "error_rate": format!("{:.4}", error_rate),
                },
            }
        })),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::metrics::Metrics;
    use crate::timebase::TimeBase;

    fn create_test_state() -> Arc<AppState> {
        use crate::performance::{LockFreeMetrics, TimeCache};

        let config = Arc::new(Config::default());
        let time_cache = Arc::new(TimeCache::new(
            config.messages.ok.clone(),
            config.messages.ok_cache.clone(),
        ));
        let perf_metrics = Arc::new(LockFreeMetrics::new());
        let timebase = TimeBase::new(true).with_cache(time_cache.clone());
        let metrics = Arc::new(Metrics::new());
        Arc::new(AppState::new(
            config,
            timebase,
            metrics,
            time_cache,
            perf_metrics,
        ))
    }

    #[tokio::test]
    async fn test_healthz() {
        let (status, _) = healthz_handler().await;
        assert_eq!(status, StatusCode::OK);
    }

    #[tokio::test]
    async fn test_time_before_sync() {
        let state = create_test_state();
        let result = time_handler(State(state.clone())).await;

        if state.config.ntp.require_sync {
            // The handler should return Err(NotSynced) which
            // IntoResponse maps to 503.
            let err = result.expect_err("expected Err before first sync");
            let response = err.into_response();
            assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        } else {
            // REQUIRE_SYNC=false: the system-clock fallback is
            // returned as a 200 Ok.
            let response = result.expect("expected Ok with REQUIRE_SYNC=false");
            assert_eq!(response.status(), StatusCode::OK);
        }
    }

    #[tokio::test]
    async fn test_readyz_before_sync() {
        let state = create_test_state();
        let (status, _) = readyz_handler(State(state.clone())).await;

        if state.config.ntp.require_sync {
            assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        }
    }

    #[tokio::test]
    async fn test_metrics() {
        let state = create_test_state();
        let metrics_output = metrics_handler(State(state)).await;

        assert!(metrics_output.contains("build_info"));
    }

    #[tokio::test]
    async fn app_error_not_synced_json_shape_matches_handler() {
        // The JSON body for the 503 path must match what the
        // handler used to build inline. Carries both `message`
        // (typically "error" or the configured value) and the
        // human-readable `error` text.
        use crate::errors::AppError;
        use axum::body::to_bytes;
        use axum::response::IntoResponse;

        let err = AppError::NotSynced {
            message: "error".to_string(),
            error: "Service not yet synchronized with NTP".to_string(),
        };
        let response = err.into_response();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);

        let body = to_bytes(response.into_body(), 1024)
            .await
            .expect("body read");
        let json: serde_json::Value = serde_json::from_slice(&body).expect("body parse");
        assert_eq!(json["message"], "error");
        assert_eq!(json["status"], 503);
        assert_eq!(json["data"], 0);
        assert_eq!(json["error"], "Service not yet synchronized with NTP");
    }
}
