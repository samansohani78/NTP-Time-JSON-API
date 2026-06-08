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

    /// Unexpected internal error. Wraps `anyhow::Error` so handlers
    /// can use `?` on any error type implementing
    /// `std::error::Error + Send + Sync + 'static`.
    #[error("Internal server error")]
    Internal(#[from] anyhow::Error),
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let (status, message, error): (StatusCode, String, String) = match self {
            AppError::NotSynced { message, error } => {
                (StatusCode::SERVICE_UNAVAILABLE, message, error)
            }
            AppError::Internal(_) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                "error".to_string(),
                "Internal server error".to_string(),
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
