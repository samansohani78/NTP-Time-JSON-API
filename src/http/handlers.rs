use super::state::AppState;
use axum::{Json, extract::State, http::StatusCode};
use serde_json::{Value, json};
use std::sync::Arc;

/// GET /time - Returns current NTP-derived time
pub async fn time_handler(State(state): State<Arc<AppState>>) -> (StatusCode, Json<Value>) {
    match state.timebase.now_ms() {
        Some(epoch_ms) => {
            // Determine if serving from cache
            let is_stale = state
                .get_staleness_seconds()
                .map(|s| s > state.config.ntp.max_staleness_secs)
                .unwrap_or(false);

            let message = if is_stale {
                &state.config.messages.ok_cache
            } else {
                &state.config.messages.ok
            };

            (
                StatusCode::OK,
                Json(json!({
                    "message": message,
                    "status": 200,
                    "data": epoch_ms,
                })),
            )
        }
        None => {
            // Not yet synced
            if state.config.ntp.require_sync {
                (
                    StatusCode::SERVICE_UNAVAILABLE,
                    Json(json!({
                        "message": &state.config.messages.error,
                        "status": 503,
                        "data": 0,
                        "error": &state.config.messages.error_no_sync,
                    })),
                )
            } else {
                // If REQUIRE_SYNC is false, return system time
                let epoch_ms = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_millis() as i64;

                (
                    StatusCode::OK,
                    Json(json!({
                        "message": &state.config.messages.ok,
                        "status": 200,
                        "data": epoch_ms,
                    })),
                )
            }
        }
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::metrics::Metrics;
    use crate::timebase::TimeBase;

    fn create_test_state() -> Arc<AppState> {
        let config = Arc::new(Config::default());
        let timebase = TimeBase::new(true);
        let metrics = Arc::new(Metrics::new());
        Arc::new(AppState::new(config, timebase, metrics))
    }

    #[tokio::test]
    async fn test_healthz() {
        let (status, _) = healthz_handler().await;
        assert_eq!(status, StatusCode::OK);
    }

    #[tokio::test]
    async fn test_time_before_sync() {
        let state = create_test_state();
        let (status, response) = time_handler(State(state.clone())).await;

        if state.config.ntp.require_sync {
            assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
            assert_eq!(response["status"], 503);
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
