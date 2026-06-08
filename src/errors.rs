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
///
/// `NtpError` and `Timeout` are part of the planned public API
/// surface but no in-tree handler currently constructs them; they
/// are reserved for future endpoints (admin sync, request-budget
/// enforcement). The `#[allow(dead_code)]` keeps clippy happy
/// without suppressing the warning on the variants we *do* use.
#[allow(dead_code)]
#[derive(Error, Debug)]
pub enum AppError {
    /// Service has not completed its first NTP sync yet. Carries the
    /// user-facing `message` and `error` strings so the response
    /// shape matches the inline JSON previously built by
    /// `time_handler` (configurable via `MSG_ERROR` / `ERROR_TEXT_NO_SYNC`).
    #[error("NTP sync not yet completed: {error}")]
    NotSynced { message: String, error: String },

    /// Upstream NTP query failed. Stringified for the log line; the
    /// HTTP body returns a generic "NTP sync failed" message.
    #[error("NTP sync failed: {0}")]
    NtpError(String),

    /// Unexpected internal error. Wraps `anyhow::Error` so handlers
    /// can use `?` on any error type implementing
    /// `std::error::Error + Send + Sync + 'static`.
    #[error("Internal server error")]
    Internal(#[from] anyhow::Error),

    /// Request exceeded the configured timeout.
    #[error("Request timeout")]
    Timeout,
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let (status, message, error): (StatusCode, String, String) = match self {
            AppError::NotSynced { message, error } => {
                (StatusCode::SERVICE_UNAVAILABLE, message, error)
            }
            AppError::NtpError(_) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                "error".to_string(),
                self.to_string(),
            ),
            AppError::Internal(_) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                "error".to_string(),
                "Internal server error".to_string(),
            ),
            AppError::Timeout => (
                StatusCode::REQUEST_TIMEOUT,
                "error".to_string(),
                "Request timeout".to_string(),
            ),
        };

        let body = Json(json!({
            "message": message,
            "status": status.as_u16(),
            "data": 0,
            "error": error,
        }));

        (status, body).into_response()
    }
}
