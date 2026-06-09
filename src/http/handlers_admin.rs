use super::state::{AppState, ManualOverrideState};
use crate::metrics::RejectLabel;
use axum::{Json, extract::State, http::StatusCode};
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::Instant;
use tracing::warn;

#[derive(Debug, Deserialize)]
pub struct SetOverrideRequest {
    pub epoch_ms: i64,
    pub reason: String,
    pub ttl_seconds: u32,
    pub operator: Option<String>,
    pub force: Option<bool>,
}

/// GET /admin/time/override
///
/// Returns the current manual override state.  Active if an override has been
/// set and has not yet expired or been deleted.
pub async fn get_override(State(state): State<Arc<AppState>>) -> (StatusCode, Json<Value>) {
    if state.timebase.is_manual_active() {
        let guard = state.override_state.read();
        if let Some(ov) = guard.as_ref() {
            let age_ms = ov.set_at_instant.elapsed().as_millis() as i64;
            let now_approx_ms = ov.set_at_ms + age_ms;
            let ttl_remaining_secs = ((ov.expires_at_ms - now_approx_ms) / 1000).max(0);
            return (
                StatusCode::OK,
                Json(json!({
                    "active": true,
                    "override": {
                        "epoch_ms": ov.epoch_ms,
                        "set_at_ms": ov.set_at_ms,
                        "expires_at_ms": ov.expires_at_ms,
                        "reason": ov.reason,
                        "operator": ov.operator,
                        "jump_ms": ov.jump_ms,
                        "ttl_remaining_secs": ttl_remaining_secs,
                    }
                })),
            );
        }
    }
    (StatusCode::OK, Json(json!({ "active": false })))
}

/// POST /admin/time/override
///
/// Sets a manual time override.  The override takes precedence over NTP until
/// it expires (TTL) or is deleted.  Replaces any existing active override.
///
/// Safety rules enforced:
/// - `reason` must be non-empty.
/// - `ttl_seconds` must be in (0, MANUAL_OVERRIDE_MAX_TTL_SECS].
/// - `|epoch_ms − current_ntp_ms|` must be ≤ MANUAL_OVERRIDE_MAX_JUMP_MS (when synced), unless
///   `force=true` and `MANUAL_OVERRIDE_ALLOW_FORCE=true`.
/// - `force=true` with `MANUAL_OVERRIDE_ALLOW_FORCE=false` is rejected 400.
/// - Monotonic clamp is unconditional regardless of force.
pub async fn post_override(
    State(state): State<Arc<AppState>>,
    Json(body): Json<SetOverrideRequest>,
) -> (StatusCode, Json<Value>) {
    // force=true: check whether the operator config permits it.
    let force_bypass = if body.force == Some(true) {
        if !state.config.admin.allow_force {
            state
                .metrics
                .manual_override_rejected_total
                .get_or_create(&RejectLabel {
                    reason: "force_not_allowed".into(),
                })
                .inc();
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({
                    "status": 400,
                    "error": "ForceNotAllowed",
                    "message": "force=true requires MANUAL_OVERRIDE_ALLOW_FORCE=true"
                })),
            );
        }
        true // allow_force=true: jump check will be skipped
    } else {
        false
    };

    // Validate reason.
    if body.reason.trim().is_empty() {
        state
            .metrics
            .manual_override_rejected_total
            .get_or_create(&RejectLabel {
                reason: "empty_reason".into(),
            })
            .inc();
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "status": 400,
                "error": "ValidationError",
                "message": "reason must not be empty"
            })),
        );
    }

    // Validate TTL.
    let max_ttl = state.config.admin.max_ttl_secs;
    if body.ttl_seconds == 0 || body.ttl_seconds > max_ttl {
        state
            .metrics
            .manual_override_rejected_total
            .get_or_create(&RejectLabel {
                reason: "invalid_ttl".into(),
            })
            .inc();
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "status": 400,
                "error": "ValidationError",
                "message": format!("ttl_seconds must be between 1 and {max_ttl}")
            })),
        );
    }

    // Validate epoch_ms jump against current NTP time (when synced).
    // Skipped when force_bypass=true (MANUAL_OVERRIDE_ALLOW_FORCE=true + force=true).
    let max_jump = state.config.admin.max_jump_ms as i64;
    if !force_bypass && let Some(ntp_now) = state.timebase.ntp_base_now_ms() {
        let jump = (body.epoch_ms - ntp_now).abs();
        if jump > max_jump {
            state
                .metrics
                .manual_override_rejected_total
                .get_or_create(&RejectLabel {
                    reason: "jump_too_large".into(),
                })
                .inc();
            return (
                StatusCode::UNPROCESSABLE_ENTITY,
                Json(json!({
                    "status": 422,
                    "error": "JumpTooLarge",
                    "message": format!("epoch_ms jump of {jump}ms exceeds max_jump_ms={max_jump}")
                })),
            );
        }
    }

    // Compute timing metadata.
    let set_at_instant = Instant::now();
    let set_at_ms = state
        .timebase
        .now_ms()
        .or_else(|| {
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .ok()
                .map(|d| d.as_millis() as i64)
        })
        .unwrap_or(0);
    let expires_at_ms = set_at_ms + (body.ttl_seconds as i64) * 1000;
    let jump_ms = state
        .timebase
        .ntp_base_now_ms()
        .map(|ntp| body.epoch_ms - ntp)
        .unwrap_or(0);

    // Activate override in TimeBase atomics.
    state.timebase.set_manual(body.epoch_ms, body.ttl_seconds);

    // Abort previous expiry task (if any).
    {
        let mut task = state.override_task.lock();
        if let Some(handle) = task.take() {
            handle.abort();
        }
    }

    // Store override state.
    *state.override_state.write() = Some(ManualOverrideState {
        epoch_ms: body.epoch_ms,
        set_at_ms,
        expires_at_ms,
        set_at_instant,
        reason: body.reason.clone(),
        operator: body.operator.clone(),
        jump_ms,
    });

    // Spawn background expiry task.
    let state_clone = state.clone();
    let log_epoch = body.epoch_ms;
    let log_reason = body.reason.clone();
    let log_operator = body.operator.clone();
    let log_set_at = set_at_ms;
    let expires_std = set_at_instant + std::time::Duration::from_secs(body.ttl_seconds as u64);
    let expiry_handle = tokio::spawn(async move {
        tokio::time::sleep_until(tokio::time::Instant::from_std(expires_std)).await;
        state_clone.timebase.clear_manual();
        *state_clone.override_state.write() = None;
        state_clone.metrics.manual_override_active.set(0);
        state_clone
            .metrics
            .manual_override_expiry_timestamp_seconds
            .set(0);
        warn!(
            action = "expired",
            epoch_ms = log_epoch,
            reason = %log_reason,
            operator = ?log_operator,
            set_at_ms = log_set_at,
            "manual time override expired"
        );
    });

    {
        let mut task = state.override_task.lock();
        *task = Some(expiry_handle.abort_handle());
    }

    // Update metrics.
    state.metrics.manual_override_active.set(1);
    state.metrics.manual_override_total.inc();
    state
        .metrics
        .manual_override_expiry_timestamp_seconds
        .set(expires_at_ms / 1000);
    state.metrics.time_source_mode.set(3); // manual

    // Audit log — token is NEVER included in any log field.
    warn!(
        action = "set",
        epoch_ms = body.epoch_ms,
        ttl_seconds = body.ttl_seconds,
        reason = %body.reason,
        operator = ?body.operator,
        jump_ms,
        "manual time override set"
    );

    (
        StatusCode::OK,
        Json(json!({
            "status": 200,
            "override": {
                "epoch_ms": body.epoch_ms,
                "set_at_ms": set_at_ms,
                "expires_at_ms": expires_at_ms,
                "reason": body.reason,
                "operator": body.operator,
                "jump_ms": jump_ms,
                "ttl_remaining_secs": body.ttl_seconds as i64,
            }
        })),
    )
}

/// DELETE /admin/time/override
///
/// Clears any active manual time override.  Idempotent: returns 200 even
/// when no override is active.
pub async fn delete_override(State(state): State<Arc<AppState>>) -> (StatusCode, Json<Value>) {
    // Abort expiry task regardless.
    {
        let mut task = state.override_task.lock();
        if let Some(handle) = task.take() {
            handle.abort();
        }
    }

    let was_active = state.timebase.is_manual_active();
    let prev_state = state.override_state.write().take();
    state.timebase.clear_manual();

    if was_active || prev_state.is_some() {
        state.metrics.manual_override_active.set(0);
        state
            .metrics
            .manual_override_expiry_timestamp_seconds
            .set(0);
        // Update time_source_mode to reflect the NTP/degraded/unsynced state.
        let quality = state.compute_quality();
        state.metrics.time_source_mode.set(match quality.source {
            "ntp" => 0,
            "degraded" => 1,
            "manual" => 3,
            _ => 2, // unsynced
        });

        if let Some(ov) = prev_state {
            warn!(
                action = "cleared",
                epoch_ms = ov.epoch_ms,
                reason = %ov.reason,
                operator = ?ov.operator,
                "manual time override cleared"
            );
        }

        return (
            StatusCode::OK,
            Json(json!({ "status": 200, "message": "override cleared" })),
        );
    }

    (
        StatusCode::OK,
        Json(json!({ "status": 200, "message": "no active override" })),
    )
}
