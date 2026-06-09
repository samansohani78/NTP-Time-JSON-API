use axum::{
    Json,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde_json::json;
use thiserror::Error;

/// Errors that handlers can return via `Result<_, AppError>`. Each
/// variant maps to a specific HTTP status code and JSON body shape
/// in [`IntoResponse`].
#[derive(Error, Debug)]
pub enum AppError {
    /// Service has not completed its first NTP sync yet. Carries the
    /// user-facing `message` and `error` strings so the response
    /// shape matches the inline JSON previously built by
    /// `time_handler` (configurable via `MSG_ERROR` / `ERROR_TEXT_NO_SYNC`).
    #[error("NTP sync not yet completed: {error}")]
    NotSynced { message: String, error: String },

    /// Service is synced but time uncertainty exceeds the configured SLA
    /// threshold and `ALLOW_DEGRADED=false`. The serve/stop policy (P0-4)
    /// prevents serving time that is too uncertain.
    #[error("Service stopped due to excessive time uncertainty: {error}")]
    ServeStopped {
        message: String,
        error: String,
        serve_state: String,
    },

    /// Unexpected internal error. Wraps `anyhow::Error` so handlers
    /// can use `?` on any error type implementing
    /// `std::error::Error + Send + Sync + 'static`.
    #[error("Internal server error")]
    Internal(#[from] anyhow::Error),
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        match self {
            AppError::NotSynced { message, error } => {
                let body = Json(json!({
                    "message": message,
                    "status": 503,
                    "data": 0,
                    "error": error,
                }));
                (StatusCode::SERVICE_UNAVAILABLE, body).into_response()
            }
            AppError::ServeStopped {
                message,
                error,
                serve_state,
            } => {
                let body = Json(json!({
                    "message": message,
                    "status": 503,
                    "data": 0,
                    "error": error,
                    "serve_state": serve_state,
                }));
                (StatusCode::SERVICE_UNAVAILABLE, body).into_response()
            }
            AppError::Internal(_) => {
                let body = Json(json!({
                    "message": "error",
                    "status": 500,
                    "data": 0,
                    "error": "Internal server error",
                }));
                (StatusCode::INTERNAL_SERVER_ERROR, body).into_response()
            }
        }
    }
}
