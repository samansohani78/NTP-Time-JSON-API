use super::state::AppState;
use axum::{Json, extract::State, http::StatusCode, response::Response};
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::Instant;

/// GET /time - Returns current NTP-derived time (zero-copy from cache)
pub async fn time_handler(State(state): State<Arc<AppState>>) -> Response {
    let start = Instant::now();

    let response = match state.timebase.now_ms() {
        Some(_epoch_ms) => {
            // Determine if serving from cache
            let is_stale = state
                .get_staleness_seconds()
                .map(|s| s > state.config.ntp.max_staleness_secs)
                .unwrap_or(false);

            // Get pre-serialized JSON from zero-copy cache
            let json_body = state.time_cache.get_json(is_stale);

            // Record cache hit
            state.perf_metrics.record_cache_hit();

            // Return pre-serialized response (zero-copy - just Arc clone)
            axum::response::Response::builder()
                .status(StatusCode::OK)
                .header("content-type", "application/json")
                .body(axum::body::Body::from((*json_body).clone()))
                .unwrap()
        }
        None => {
            // Not yet synced
            if state.config.ntp.require_sync {
                let body = json!({
                    "message": &state.config.messages.error,
                    "status": 503,
                    "data": 0,
                    "error": &state.config.messages.error_no_sync,
                });

                axum::response::Response::builder()
                    .status(StatusCode::SERVICE_UNAVAILABLE)
                    .header("content-type", "application/json")
                    .body(axum::body::Body::from(
                        serde_json::to_string(&body).unwrap(),
                    ))
                    .unwrap()
            } else {
                // If REQUIRE_SYNC is false, return system time
                let epoch_ms = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_millis() as i64;

                let body = json!({
                    "message": &state.config.messages.ok,
                    "status": 200,
                    "data": epoch_ms,
                });

                axum::response::Response::builder()
                    .status(StatusCode::OK)
                    .header("content-type", "application/json")
                    .body(axum::body::Body::from(
                        serde_json::to_string(&body).unwrap(),
                    ))
                    .unwrap()
            }
        }
    };

    // Record latency
    let latency_us = start.elapsed().as_micros() as u64;
    state.perf_metrics.record_success(latency_us);

    response
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
        let response = time_handler(State(state.clone())).await;

        if state.config.ntp.require_sync {
            assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
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
}
