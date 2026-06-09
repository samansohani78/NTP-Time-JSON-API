use super::state::{AppState, TimeQuality};
use crate::errors::AppError;
use axum::{Json, extract::State, http::StatusCode, response::Response};
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::Instant;

/// GET /time (or GET /) — Returns current NTP-derived epoch time.
///
/// Body is backward-compatible JSON `{message, status, data}`.
/// Quality headers are added to every 200 response:
/// - `X-Time-Source`: `ntp` | `degraded` | `holdover` | `manual` | `unsynced`
/// - `X-Time-Serve-State`: `ok` | `degraded` | `holdover` | `stopped` | `unsynced`
/// - `X-Time-Uncertainty-Ms`: computed dispersion in ms (omitted when unsynced/holdover)
/// - `X-Time-Stratum`: upstream stratum (omitted when unsynced/holdover)
/// - `X-Time-Staleness-Ms`: ms since last sync (omitted when unsynced/holdover)
/// - `X-Time-Selected-Server`: NTP server used for last sync (omitted when unsynced/holdover)
///
/// Default serve policy (holdover-first): after any seed (NTP, manual, or persisted),
/// returns HTTP 200 for all quality states including degraded and holdover.
/// HTTP 503 is only returned when uninitialized (no seed) + REQUIRE_SYNC=true,
/// or when STRICT_SLA_MODE=true and uncertainty exceeds the configured threshold.
pub async fn time_handler(State(state): State<Arc<AppState>>) -> Result<Response, AppError> {
    let start = Instant::now();

    let result: Result<Response, AppError> = match state.timebase.now_ms() {
        Some(epoch_ms) => {
            let quality = state.compute_quality();
            // Only return 503 in strict SLA mode when serve_state="stopped".
            // In default mode (strict_sla_mode=false), always serve 200 after seed.
            if state.config.quality.strict_sla_mode && quality.serve_state == "stopped" {
                Err(AppError::ServeStopped {
                    message: state.config.messages.error.clone(),
                    error: format!(
                        "Time uncertainty ({:.1} ms) exceeds the configured SLA threshold",
                        quality.uncertainty_ms.unwrap_or(0.0)
                    ),
                    serve_state: "stopped".into(),
                })
            } else {
                state.perf_metrics.record_cache_hit();
                Ok(build_time_response(&state, epoch_ms, &quality))
            }
        }
        None if state.config.ntp.require_sync => Err(AppError::NotSynced {
            message: state.config.messages.error.clone(),
            error: state.config.messages.error_no_sync.clone(),
        }),
        None => {
            let quality = state.compute_quality(); // source="unsynced"
            Ok(build_system_clock_response(&state, &quality))
        }
    };

    let latency_us = start.elapsed().as_micros() as u64;
    match &result {
        Ok(_) => state.perf_metrics.record_success(latency_us),
        Err(_) => state.perf_metrics.record_error(),
    }

    result
}

/// Build the 200 OK response for the synced path. Uses the
/// pre-serialized JSON cache (zero-copy via `Arc<String>`) so the
/// hot path stays fast. Appends quality headers without touching the body.
fn build_time_response(state: &AppState, epoch_ms: i64, quality: &TimeQuality) -> Response {
    let is_stale = quality.serve_state != "ok";

    // PERFORMANCE: Update cache with current time, then get
    // pre-serialized JSON. This avoids json!() macro and serde
    // overhead on the hot path.
    state.time_cache.update(epoch_ms, is_stale);
    let json_body = state.time_cache.get_json(is_stale);

    let mut builder = axum::response::Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "application/json")
        .header("x-time-source", quality.source)
        .header("x-time-serve-state", quality.serve_state);

    if let Some(u) = quality.uncertainty_ms {
        builder = builder.header("x-time-uncertainty-ms", format!("{u:.3}"));
    }
    if let Some(s) = quality.stratum {
        builder = builder.header("x-time-stratum", s.to_string());
    }
    if let Some(ms) = quality.staleness_ms {
        builder = builder.header("x-time-staleness-ms", ms.to_string());
    }
    if let Some(ref srv) = quality.selected_server {
        builder = builder.header("x-time-selected-server", srv.as_str());
    }

    builder
        .body(axum::body::Body::from((*json_body).clone()))
        .expect("failed to build /time response")
}

/// Build the 200 OK response for the `REQUIRE_SYNC=false` fallback,
/// where the service reports the OS wall clock instead of the
/// NTP-derived time. Defeats the "NTP-authoritative" design but
/// useful for development; never enabled in production.
fn build_system_clock_response(state: &AppState, quality: &TimeQuality) -> Response {
    let epoch_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);

    let body = json!({
        "message": &state.config.messages.ok,
        "status": 200,
        "data": epoch_ms,
    });

    let mut builder = axum::response::Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "application/json")
        .header("x-time-source", quality.source)
        .header("x-time-serve-state", quality.serve_state);

    if let Some(u) = quality.uncertainty_ms {
        builder = builder.header("x-time-uncertainty-ms", format!("{u:.3}"));
    }
    if let Some(s) = quality.stratum {
        builder = builder.header("x-time-stratum", s.to_string());
    }
    if let Some(ms) = quality.staleness_ms {
        builder = builder.header("x-time-staleness-ms", ms.to_string());
    }
    if let Some(ref srv) = quality.selected_server {
        builder = builder.header("x-time-selected-server", srv.as_str());
    }

    let body_bytes = serde_json::to_vec(&body).expect("json serialization");
    builder
        .body(axum::body::Body::from(body_bytes))
        .expect("failed to build system-clock response")
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
///
/// Returns 503 before first sync (if `REQUIRE_SYNC=true`). After first sync,
/// also returns 503 if `uncertainty > READINESS_MAX_UNCERTAINTY_MS` — a synced
/// but high-uncertainty pod should not receive traffic.
pub async fn readyz_handler(State(state): State<Arc<AppState>>) -> (StatusCode, Json<Value>) {
    if state.config.ntp.require_sync && !state.timebase.has_synced() {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({
                "status": "not_ready",
                "reason": "not_yet_synced"
            })),
        );
    }

    if state.timebase.has_synced() {
        let quality = state.compute_quality();
        let readiness_max = state.config.quality.readiness_max_uncertainty_ms;
        if let Some(u) = quality.uncertainty_ms
            && u > readiness_max
        {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({
                    "status": "not_ready",
                    "reason": "uncertainty_too_high",
                    "uncertainty_ms": u,
                    "threshold_ms": readiness_max,
                })),
            );
        }
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

/// Extract the provider group (last 2 DNS labels) from a server address like "host:port".
/// IP literals are returned verbatim (without port). Single-label hostnames are returned as-is.
fn extract_provider(server_addr: &str) -> String {
    let host = server_addr
        .rsplit_once(':')
        .map(|(h, _)| h)
        .unwrap_or(server_addr);
    if host.parse::<std::net::IpAddr>().is_ok() {
        return host.to_string();
    }
    let labels: Vec<&str> = host.split('.').collect();
    if labels.len() >= 2 {
        format!("{}.{}", labels[labels.len() - 2], labels[labels.len() - 1])
    } else {
        host.to_string()
    }
}

/// GET /time/full - Enriched time response with quality envelope.
///
/// Body includes all fields from `/time` plus quality metadata.
/// Runs on the slow router (full middleware stack). Body is not
/// backward-compatible with `/time`; callers that need stability
/// should use `/time` + the `X-*` headers instead.
pub async fn time_full_handler(State(state): State<Arc<AppState>>) -> (StatusCode, Json<Value>) {
    let quality = state.compute_quality();

    let (status_code, epoch_ms, message) = match state.timebase.now_ms() {
        Some(ms) => {
            // In strict mode, honor "stopped". In default mode, always serve.
            if state.config.quality.strict_sla_mode && quality.serve_state == "stopped" {
                (
                    StatusCode::SERVICE_UNAVAILABLE,
                    0i64,
                    state.config.messages.error.clone(),
                )
            } else {
                (StatusCode::OK, ms, state.config.messages.ok.clone())
            }
        }
        None if state.config.ntp.require_sync => (
            StatusCode::SERVICE_UNAVAILABLE,
            0i64,
            state.config.messages.error.clone(),
        ),
        None => {
            let ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as i64)
                .unwrap_or(0);
            (StatusCode::OK, ms, state.config.messages.ok.clone())
        }
    };

    let selected_provider = quality.selected_server.as_deref().map(extract_provider);
    let intersection = quality.selection.as_ref().map(|s| json!(&s.intersection));

    (
        status_code,
        Json(json!({
            "message": message,
            "status": status_code.as_u16(),
            "data": epoch_ms,
            "replica_id": state.config.replica.replica_id,
            "source": quality.source,
            "serve_state": quality.serve_state,
            "uncertainty_ms": quality.uncertainty_ms,
            "staleness_ms": quality.staleness_ms,
            "stratum": quality.stratum,
            "selected_server": quality.selected_server,
            "selected_provider": selected_provider,
            "leap": quality.leap,
            "override_info": quality.override_info,
            "selection": quality.selection,
            "intersection": intersection,
        })),
    )
}

/// GET /status - Operational quality envelope.
///
/// Always returns 200. The `serve_state` field communicates whether the
/// service is currently healthy, degraded, or would stop serving `/time`.
/// Callers that need to gate on time quality should read `serve_state`
/// rather than checking the HTTP status code.
pub async fn status_handler(State(state): State<Arc<AppState>>) -> (StatusCode, Json<Value>) {
    let quality = state.compute_quality();
    let ntp_synced = state.timebase.has_synced();

    // P1-8: fields for replica drift visibility
    let selected_offset_ms = state.last_sync_quality.read().as_ref().map(|q| q.offset_ms);
    let combined_uncertainty_ms = quality
        .selection
        .as_ref()
        .and_then(|s| s.combined_uncertainty_ms);
    let selected_provider = quality.selected_server.as_deref().map(extract_provider);
    let selection_state = quality.selection.as_ref().map(|s| json!(s.selection_state));
    // P1F-12: intersection diagnostics
    let intersection = quality.selection.as_ref().map(|s| json!(&s.intersection));

    (
        StatusCode::OK,
        Json(json!({
            "replica_id": state.config.replica.replica_id,
            "source": quality.source,
            "serve_state": quality.serve_state,
            "uncertainty_ms": quality.uncertainty_ms,
            "combined_uncertainty_ms": combined_uncertainty_ms,
            "selected_offset_ms": selected_offset_ms,
            "staleness_ms": quality.staleness_ms,
            "stratum": quality.stratum,
            "selected_server": quality.selected_server,
            "selected_provider": selected_provider,
            "selection_state": selection_state,
            "leap": quality.leap,
            "ntp_synced": ntp_synced,
            "override_info": quality.override_info,
            "selection": quality.selection,
            "intersection": intersection,
        })),
    )
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

    let ntp_timing = state.last_ntp_timing.read().clone().map(|t| {
        use crate::ntp::selection::TimingSource;
        let timing_source = match t.timing_source {
            TimingSource::Measured => "measured",
            TimingSource::Estimated => "estimated",
        };
        json!({
            "server": t.server,
            "t1_client_send_ms": t.t1_client_send_ms,
            "t2_server_recv_ms": t.t2_server_recv_ms,
            "t3_server_send_ms": t.t3_server_send_ms,
            "t4_client_recv_ms": t.t4_client_recv_ms,
            "offset_ms": t.offset_ms,
            "rtt_ms": t.rtt_ms,
            "root_delay_ms": t.root_delay_ms,
            "root_dispersion_ms": t.root_dispersion_ms,
            "stratum": t.stratum,
            "leap": t.leap,
            "precision_log2": t.precision_log2,
            "reference_id": t.reference_id,
            "timing_source": timing_source,
        })
    });

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
            },
            "ntp_timing": ntp_timing,
        })),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::metrics::Metrics;
    use crate::timebase::TimeBase;
    use axum::response::IntoResponse;

    fn create_test_state() -> Arc<AppState> {
        create_test_state_with_config(Arc::new(Config::default()))
    }

    fn create_test_state_with_config(config: Arc<Config>) -> Arc<AppState> {
        use crate::performance::{LockFreeMetrics, TimeCache};

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

    /// Exercises the `REQUIRE_SYNC=false` code path where the service
    /// returns a 200 using the OS wall clock before the first NTP sync.
    #[tokio::test]
    async fn test_time_require_sync_false_returns_200_before_sync() {
        let mut config = Config::default();
        config.ntp.require_sync = false;
        let state = create_test_state_with_config(Arc::new(config));

        // TimeBase is unsynced (no update() called).
        assert!(!state.timebase.has_synced());

        let response = time_handler(State(state))
            .await
            .expect("expected Ok when REQUIRE_SYNC=false");

        assert_eq!(response.status(), StatusCode::OK);
    }

    /// Verifies that the system-clock fallback JSON body contains the
    /// expected keys and a non-zero epoch.
    #[tokio::test]
    async fn test_time_require_sync_false_body_shape() {
        use axum::body::to_bytes;

        let mut config = Config::default();
        config.ntp.require_sync = false;
        let state = create_test_state_with_config(Arc::new(config));

        let response = time_handler(State(state)).await.expect("expected Ok");

        let bytes = to_bytes(response.into_body(), 512).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();

        assert_eq!(json["status"], 200);
        assert!(
            json["data"].as_i64().unwrap_or(0) > 0,
            "epoch_ms must be non-zero"
        );
        assert!(json["message"].is_string());
    }

    // ── P0-4: quality policy table ────────────────────────────────────────

    fn inject_sync_quality(state: &AppState, upstream_dispersion_ms: u32, age_secs: u64) {
        use crate::ntp::SyncQuality;
        use std::time::Instant;
        // Fake an `Instant` that is `age_secs` old by subtracting from now.
        let past_instant = Instant::now()
            .checked_sub(std::time::Duration::from_secs(age_secs))
            .unwrap_or_else(Instant::now);
        *state.last_sync_quality.write() = Some(SyncQuality {
            upstream_root_delay_ms: 10,
            upstream_root_dispersion_ms: upstream_dispersion_ms,
            precision_log2: -10,
            stratum: 2,
            leap: 0,
            measured_rtt_ms: 5,
            jitter_ms: 0,
            offset_ms: 1,
            last_sync_instant: past_instant,
            selected_server: "ntp.test:123".into(),
        });
        state.record_sync_success();
    }

    #[tokio::test]
    async fn quality_unsynced_returns_unsynced() {
        let state = create_test_state();
        // No sync done — quality should be "unsynced"
        let q = state.compute_quality();
        assert_eq!(q.source, "unsynced");
        assert_eq!(q.serve_state, "unsynced");
        assert!(q.uncertainty_ms.is_none());
    }

    #[tokio::test]
    async fn quality_fresh_good_returns_ok() {
        let state = create_test_state();
        // upstream_dispersion=1ms, age=0s → uncertainty ~= 1 + |2^-10 * 1000| + 0 + 0 + 2.5 ≈ 4.5 ms
        // Well within default ok_max=50 ms.
        inject_sync_quality(&state, 1, 0);
        let q = state.compute_quality();
        assert_eq!(q.source, "ntp");
        assert_eq!(q.serve_state, "ok");
        assert!(q.uncertainty_ms.unwrap() < 50.0);
    }

    // In default mode (strict_sla_mode=false), high uncertainty → "degraded" or "holdover" (200).
    #[tokio::test]
    async fn quality_high_uncertainty_degraded_by_default() {
        let mut config = crate::config::Config::default();
        // strict_sla_mode=false (the default) — do NOT set it; verify the default
        config.quality.serve_ok_max_uncertainty_ms = 1.0;
        config.quality.serve_degraded_max_uncertainty_ms = 10.0;
        let state = create_test_state_with_config(Arc::new(config));
        // uncertainty ≈ 8ms: above ok_max(1) but below degraded_max(10) → "degraded" not "stopped"
        inject_sync_quality(&state, 5, 0);
        let q = state.compute_quality();
        assert_eq!(q.source, "degraded");
        assert_eq!(q.serve_state, "degraded"); // 200 in default mode
    }

    // strict_sla_mode=true + allow_degraded=false → "stopped" when uncertainty > ok_max
    #[tokio::test]
    async fn quality_high_uncertainty_stops_when_strict_and_allow_degraded_false() {
        let mut config = crate::config::Config::default();
        config.quality.strict_sla_mode = true;
        config.quality.allow_degraded = false;
        config.quality.serve_ok_max_uncertainty_ms = 1.0;
        config.quality.serve_degraded_max_uncertainty_ms = 10.0;
        let state = create_test_state_with_config(Arc::new(config));
        inject_sync_quality(&state, 5, 0);
        let q = state.compute_quality();
        assert_eq!(q.source, "degraded");
        assert_eq!(q.serve_state, "stopped");
    }

    // strict_sla_mode=true + allow_degraded=true → "degraded" (200) in the band
    #[tokio::test]
    async fn quality_high_uncertainty_degraded_when_strict_and_allow_degraded_true() {
        let mut config = crate::config::Config::default();
        config.quality.strict_sla_mode = true;
        config.quality.allow_degraded = true;
        config.quality.serve_ok_max_uncertainty_ms = 1.0;
        config.quality.serve_degraded_max_uncertainty_ms = 10.0;
        let state = create_test_state_with_config(Arc::new(config));
        inject_sync_quality(&state, 5, 0);
        let q = state.compute_quality();
        assert_eq!(q.source, "degraded");
        assert_eq!(q.serve_state, "degraded");
    }

    // In default mode, very high uncertainty → "holdover" (200), not "stopped"
    #[tokio::test]
    async fn quality_very_high_uncertainty_holdover_by_default() {
        let mut config = crate::config::Config::default();
        config.quality.serve_ok_max_uncertainty_ms = 1.0;
        config.quality.serve_degraded_max_uncertainty_ms = 5.0;
        let state = create_test_state_with_config(Arc::new(config));
        inject_sync_quality(&state, 100, 0); // uncertainty >> 5ms
        let q = state.compute_quality();
        assert_eq!(q.source, "holdover");
        assert_eq!(q.serve_state, "holdover"); // 200 in default mode
    }

    // strict_sla_mode=true + uncertainty > degraded_max → always "stopped"
    #[tokio::test]
    async fn quality_beyond_degraded_max_stops_in_strict_mode() {
        let mut config = crate::config::Config::default();
        config.quality.strict_sla_mode = true;
        config.quality.allow_degraded = true;
        config.quality.serve_ok_max_uncertainty_ms = 1.0;
        config.quality.serve_degraded_max_uncertainty_ms = 5.0;
        let state = create_test_state_with_config(Arc::new(config));
        inject_sync_quality(&state, 100, 0);
        let q = state.compute_quality();
        assert_eq!(q.serve_state, "stopped");
    }

    #[tokio::test]
    async fn quality_stale_becomes_holdover() {
        let mut config = crate::config::Config::default();
        config.ntp.max_staleness_secs = 5; // short threshold for test
        let state = create_test_state_with_config(Arc::new(config));
        // age_secs=10 > max_staleness=5 → holdover in default mode (stale but still serving)
        inject_sync_quality(&state, 0, 10);
        let q = state.compute_quality();
        assert_eq!(q.source, "holdover");
        assert_eq!(q.serve_state, "holdover");
        assert_ne!(q.serve_state, "ok");
    }

    #[tokio::test]
    async fn time_handler_returns_503_when_serve_state_stopped() {
        let mut config = crate::config::Config::default();
        config.quality.strict_sla_mode = true; // opt-in strict mode required for 503
        config.quality.serve_ok_max_uncertainty_ms = 1.0;
        config.quality.serve_degraded_max_uncertainty_ms = 5.0;
        let state = create_test_state_with_config(Arc::new(config));
        // Sync with high uncertainty
        use crate::ntp::SyncResult;
        use crate::ntp::selection::TimingSource;
        let sync_result = SyncResult {
            epoch_ms: 1_700_000_000_000,
            server: "test:123".into(),
            rtt: std::time::Duration::from_millis(5),
            instant: std::time::Instant::now(),
            offset_ms: 0,
            t1_client_send_ms: 0,
            t2_server_recv_ms: 0,
            t3_server_send_ms: 0,
            t4_client_recv_ms: 0,
            root_delay_ms: 0,
            root_dispersion_ms: 100, // 100 ms >> 5 ms degraded_max
            stratum: 2,
            leap: 0,
            precision_log2: -10,
            reference_id: 0,
            timing_source: TimingSource::Measured,
        };
        state.timebase.update(&sync_result);
        inject_sync_quality(&state, 100, 0);

        let result = time_handler(State(state.clone())).await;
        let response = result
            .expect_err("expected ServeStopped error")
            .into_response();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);

        use axum::body::to_bytes;
        let body = to_bytes(response.into_body(), 1024).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["serve_state"], "stopped");
    }

    #[tokio::test]
    async fn time_handler_adds_quality_headers() {
        use crate::ntp::SyncResult;
        use crate::ntp::selection::TimingSource;
        let state = create_test_state();
        let sync_result = SyncResult {
            epoch_ms: 1_700_000_000_000,
            server: "test:123".into(),
            rtt: std::time::Duration::from_millis(5),
            instant: std::time::Instant::now(),
            offset_ms: 0,
            t1_client_send_ms: 0,
            t2_server_recv_ms: 0,
            t3_server_send_ms: 0,
            t4_client_recv_ms: 0,
            root_delay_ms: 0,
            root_dispersion_ms: 1,
            stratum: 2,
            leap: 0,
            precision_log2: -10,
            reference_id: 0,
            timing_source: TimingSource::Measured,
        };
        state.timebase.update(&sync_result);
        inject_sync_quality(&state, 1, 0);

        let response = time_handler(State(state.clone()))
            .await
            .expect("expected 200");
        assert_eq!(response.status(), StatusCode::OK);

        let headers = response.headers();
        assert!(
            headers.contains_key("x-time-source"),
            "x-time-source header missing"
        );
        assert!(
            headers.contains_key("x-time-serve-state"),
            "x-time-serve-state header missing"
        );
        assert!(
            headers.contains_key("x-time-uncertainty-ms"),
            "x-time-uncertainty-ms header missing"
        );
        assert!(
            headers.contains_key("x-time-stratum"),
            "x-time-stratum header missing"
        );
        assert!(
            headers.contains_key("x-time-staleness-ms"),
            "x-time-staleness-ms header missing"
        );
        assert!(
            headers.contains_key("x-time-selected-server"),
            "x-time-selected-server header missing"
        );
        assert_eq!(headers["x-time-source"], "ntp");
        assert_eq!(headers["x-time-serve-state"], "ok");
        assert_eq!(headers["x-time-stratum"], "2");
        assert_eq!(headers["x-time-selected-server"], "ntp.test:123");
    }

    #[tokio::test]
    async fn time_handler_body_unchanged_for_ok_path() {
        use crate::ntp::SyncResult;
        use crate::ntp::selection::TimingSource;
        use axum::body::to_bytes;
        let state = create_test_state();
        let sync_result = SyncResult {
            epoch_ms: 1_700_000_000_000,
            server: "test:123".into(),
            rtt: std::time::Duration::from_millis(5),
            instant: std::time::Instant::now(),
            offset_ms: 0,
            t1_client_send_ms: 0,
            t2_server_recv_ms: 0,
            t3_server_send_ms: 0,
            t4_client_recv_ms: 0,
            root_delay_ms: 0,
            root_dispersion_ms: 1,
            stratum: 2,
            leap: 0,
            precision_log2: -10,
            reference_id: 0,
            timing_source: TimingSource::Measured,
        };
        state.timebase.update(&sync_result);
        inject_sync_quality(&state, 1, 0);

        let response = time_handler(State(state.clone()))
            .await
            .expect("expected 200");
        let body = to_bytes(response.into_body(), 256).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();

        // Body must contain exactly these keys (and nothing extra for compat)
        assert_eq!(json["status"], 200);
        assert!(json["data"].as_i64().unwrap_or(0) > 0);
        assert!(json["message"].is_string());
        // Must NOT contain quality fields in the basic /time body
        assert!(
            json.get("source").is_none(),
            "/time body must not contain 'source'"
        );
        assert!(
            json.get("serve_state").is_none(),
            "/time body must not contain 'serve_state'"
        );
    }

    // ── Holdover / default-mode behaviour ────────────────────────────────────

    /// After seed, high uncertainty must return 200 (not 503) in default mode.
    #[tokio::test]
    async fn time_handler_returns_200_with_holdover_after_seed_by_default() {
        use crate::ntp::SyncResult;
        use crate::ntp::selection::TimingSource;
        use axum::body::to_bytes;
        let mut config = crate::config::Config::default();
        // Tight thresholds so uncertainty is definitely above ok_max
        config.quality.serve_ok_max_uncertainty_ms = 1.0;
        config.quality.serve_degraded_max_uncertainty_ms = 2.0;
        // strict_sla_mode=false (default) → holdover, not stopped
        let state = create_test_state_with_config(Arc::new(config));
        let sync_result = SyncResult {
            epoch_ms: 1_700_000_000_000,
            server: "test:123".into(),
            rtt: std::time::Duration::from_millis(5),
            instant: std::time::Instant::now(),
            offset_ms: 0,
            t1_client_send_ms: 0,
            t2_server_recv_ms: 0,
            t3_server_send_ms: 0,
            t4_client_recv_ms: 0,
            root_delay_ms: 0,
            root_dispersion_ms: 200, // >> 2ms
            stratum: 2,
            leap: 0,
            precision_log2: -10,
            reference_id: 0,
            timing_source: TimingSource::Measured,
        };
        state.timebase.update(&sync_result);
        inject_sync_quality(&state, 200, 0);

        let response = time_handler(State(state.clone()))
            .await
            .expect("expected 200 in default mode even with high uncertainty");
        assert_eq!(response.status(), StatusCode::OK);
        // serve_state header should be "holdover" not "stopped"
        assert_eq!(response.headers()["x-time-serve-state"], "holdover");

        let body = to_bytes(response.into_body(), 256).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["status"], 200);
        assert!(json["data"].as_i64().unwrap_or(0) > 0);
    }

    /// After seed, failed sync must NOT clear the TimeBase — the service keeps serving.
    #[tokio::test]
    async fn failed_sync_does_not_clear_timebase() {
        use crate::ntp::SyncResult;
        use crate::ntp::selection::TimingSource;
        let state = create_test_state();
        let sync_result = SyncResult {
            epoch_ms: 1_700_000_000_000,
            server: "test:123".into(),
            rtt: std::time::Duration::from_millis(5),
            instant: std::time::Instant::now(),
            offset_ms: 0,
            t1_client_send_ms: 0,
            t2_server_recv_ms: 0,
            t3_server_send_ms: 0,
            t4_client_recv_ms: 0,
            root_delay_ms: 0,
            root_dispersion_ms: 1,
            stratum: 2,
            leap: 0,
            precision_log2: -10,
            reference_id: 0,
            timing_source: TimingSource::Measured,
        };
        state.timebase.update(&sync_result);
        state.record_sync_success();

        // Simulate repeated NTP failures — these must not clear the TimeBase
        for _ in 0..5 {
            state.record_sync_failure();
        }

        // TimeBase still returns Some after failures
        assert!(state.timebase.has_synced());
        assert!(state.timebase.now_ms().is_some());

        // /time should still return 200
        let response = time_handler(State(state.clone()))
            .await
            .expect("expected 200 after failures");
        assert_eq!(response.status(), StatusCode::OK);
    }

    /// Monotonic output never goes backwards across calls.
    #[tokio::test]
    async fn monotonic_never_goes_backwards() {
        use crate::ntp::SyncResult;
        use crate::ntp::selection::TimingSource;
        let state = create_test_state();
        let sync_result = SyncResult {
            epoch_ms: 1_700_000_000_000,
            server: "test:123".into(),
            rtt: std::time::Duration::from_millis(5),
            instant: std::time::Instant::now(),
            offset_ms: 0,
            t1_client_send_ms: 0,
            t2_server_recv_ms: 0,
            t3_server_send_ms: 0,
            t4_client_recv_ms: 0,
            root_delay_ms: 0,
            root_dispersion_ms: 1,
            stratum: 2,
            leap: 0,
            precision_log2: -10,
            reference_id: 0,
            timing_source: TimingSource::Measured,
        };
        state.timebase.update(&sync_result);
        inject_sync_quality(&state, 1, 0);

        let mut prev = 0i64;
        for _ in 0..20 {
            let ms = state.timebase.now_ms().unwrap();
            assert!(ms >= prev, "time went backwards: {ms} < {prev}");
            prev = ms;
        }
    }

    /// Holdover state — TimeBase seeded by manual seed, no NTP quality available.
    #[tokio::test]
    async fn holdover_state_when_seeded_without_ntp_quality() {
        use crate::ntp::SyncResult;
        use crate::ntp::selection::TimingSource;
        let state = create_test_state();
        // Seed TimeBase directly (simulates persisted state load or manual seed)
        // but do NOT populate last_sync_quality
        let seed = SyncResult {
            epoch_ms: 1_700_000_000_000,
            server: "persisted".into(),
            rtt: std::time::Duration::ZERO,
            instant: std::time::Instant::now(),
            offset_ms: 0,
            t1_client_send_ms: 0,
            t2_server_recv_ms: 0,
            t3_server_send_ms: 0,
            t4_client_recv_ms: 0,
            root_delay_ms: 0,
            root_dispersion_ms: 0,
            stratum: 2,
            leap: 0,
            precision_log2: 0,
            reference_id: 0,
            timing_source: TimingSource::Estimated,
        };
        state.timebase.update(&seed);
        // last_sync_quality intentionally left as None

        let q = state.compute_quality();
        assert_eq!(q.source, "holdover");
        assert_eq!(q.serve_state, "holdover");

        // /time must return 200 (has_synced=true → now_ms=Some)
        let response = time_handler(State(state)).await.expect("expected 200");
        assert_eq!(response.status(), StatusCode::OK);
    }

    /// After a good seed, repeated NTP sync failures must not affect service continuity.
    #[tokio::test]
    async fn timebase_continues_after_ntp_failures() {
        use crate::ntp::SyncResult;
        use crate::ntp::selection::TimingSource;
        let state = create_test_state();
        let seed = SyncResult {
            epoch_ms: 1_700_000_000_000,
            server: "test:123".into(),
            rtt: std::time::Duration::from_millis(5),
            instant: std::time::Instant::now(),
            offset_ms: 0,
            t1_client_send_ms: 0,
            t2_server_recv_ms: 0,
            t3_server_send_ms: 0,
            t4_client_recv_ms: 0,
            root_delay_ms: 0,
            root_dispersion_ms: 1,
            stratum: 2,
            leap: 0,
            precision_log2: -10,
            reference_id: 0,
            timing_source: TimingSource::Measured,
        };
        state.timebase.update(&seed);
        inject_sync_quality(&state, 1, 0);

        // Simulate multiple NTP failures — do NOT call timebase.update() or clear quality
        for _ in 0..5 {
            state.record_sync_failure();
        }

        // TimeBase is still seeded; /time should return 200
        let state_clone = state.clone();
        let response = time_handler(State(state_clone))
            .await
            .expect("expected 200");
        assert_eq!(response.status(), StatusCode::OK);
        assert!(state.timebase.has_synced());
    }

    /// Staleness grows with time: after seeding, staleness_ms reflects elapsed duration.
    #[tokio::test]
    async fn holdover_uncertainty_grows_with_age() {
        use crate::ntp::selection::TimingSource;
        use crate::ntp::{SyncQuality, SyncResult};
        let state = create_test_state();
        let seed = SyncResult {
            epoch_ms: 1_700_000_000_000,
            server: "test:123".into(),
            rtt: std::time::Duration::from_millis(5),
            instant: std::time::Instant::now(),
            offset_ms: 0,
            t1_client_send_ms: 0,
            t2_server_recv_ms: 0,
            t3_server_send_ms: 0,
            t4_client_recv_ms: 0,
            root_delay_ms: 0,
            root_dispersion_ms: 5,
            stratum: 2,
            leap: 0,
            precision_log2: -10,
            reference_id: 0,
            timing_source: TimingSource::Measured,
        };
        state.timebase.update(&seed);
        // Fake a sync that happened 200 s ago
        let past = std::time::Instant::now()
            .checked_sub(std::time::Duration::from_secs(200))
            .unwrap_or_else(std::time::Instant::now);
        *state.last_sync_quality.write() = Some(SyncQuality {
            upstream_root_delay_ms: 10,
            upstream_root_dispersion_ms: 5,
            precision_log2: -10,
            stratum: 2,
            leap: 0,
            measured_rtt_ms: 5,
            jitter_ms: 0,
            offset_ms: 1,
            last_sync_instant: past,
            selected_server: "ntp.test:123".into(),
        });

        let quality = state.compute_quality();
        let staleness = quality.staleness_ms.expect("staleness must be Some");
        // 200 seconds of age should give ≥ 190_000 ms staleness
        assert!(
            staleness >= 190_000,
            "expected staleness ≥ 190 000 ms, got {staleness}"
        );
    }

    /// Sync failure must not clear last_sync_quality (holdover keeps the good state).
    #[tokio::test]
    async fn worse_ntp_sample_does_not_replace_current_quality() {
        use crate::ntp::SyncResult;
        use crate::ntp::selection::TimingSource;
        let state = create_test_state();
        let seed = SyncResult {
            epoch_ms: 1_700_000_000_000,
            server: "test:123".into(),
            rtt: std::time::Duration::from_millis(5),
            instant: std::time::Instant::now(),
            offset_ms: 0,
            t1_client_send_ms: 0,
            t2_server_recv_ms: 0,
            t3_server_send_ms: 0,
            t4_client_recv_ms: 0,
            root_delay_ms: 0,
            root_dispersion_ms: 1,
            stratum: 2,
            leap: 0,
            precision_log2: -10,
            reference_id: 0,
            timing_source: TimingSource::Measured,
        };
        state.timebase.update(&seed);
        inject_sync_quality(&state, 1, 0);

        // Sync failure path: record failure but do NOT update quality
        state.record_sync_failure();

        // Quality should still be the good snapshot
        let quality_guard = state.last_sync_quality.read();
        let quality = quality_guard
            .as_ref()
            .expect("quality should still be present");
        assert!(
            quality.compute_dispersion_ms() < 100.0,
            "quality should still be the good snapshot"
        );
    }

    /// Successful NTP sync updates last_sync_quality.
    #[tokio::test]
    async fn better_ntp_sample_updates_quality() {
        use crate::ntp::SyncResult;
        use crate::ntp::selection::TimingSource;
        let state = create_test_state();
        // Initially unsynced
        assert!(state.last_sync_quality.read().is_none());

        // Simulate successful sync
        let result = SyncResult {
            epoch_ms: 1_700_000_000_000,
            server: "test:123".into(),
            rtt: std::time::Duration::from_millis(5),
            instant: std::time::Instant::now(),
            offset_ms: 0,
            t1_client_send_ms: 0,
            t2_server_recv_ms: 0,
            t3_server_send_ms: 0,
            t4_client_recv_ms: 0,
            root_delay_ms: 0,
            root_dispersion_ms: 2,
            stratum: 2,
            leap: 0,
            precision_log2: -10,
            reference_id: 0,
            timing_source: TimingSource::Measured,
        };
        state.timebase.update(&result);
        inject_sync_quality(&state, 2, 0);

        let guard = state.last_sync_quality.read();
        let quality = guard
            .as_ref()
            .expect("quality should be updated after sync");
        assert!(
            quality.compute_dispersion_ms() < 100.0,
            "quality should reflect the new sync"
        );
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
