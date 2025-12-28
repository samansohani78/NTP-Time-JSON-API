use axum::{
    Json,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde_json::json;
use thiserror::Error;

#[allow(dead_code)]
#[derive(Error, Debug)]
pub enum AppError {
    #[error("NTP sync not yet completed")]
    NotSynced,

    #[error("NTP sync failed: {0}")]
    NtpError(String),

    #[error("Internal server error")]
    Internal(#[from] anyhow::Error),

    #[error("Request timeout")]
    Timeout,
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let (status, error_message) = match self {
            AppError::NotSynced => (StatusCode::SERVICE_UNAVAILABLE, self.to_string()),
            AppError::NtpError(_) => (StatusCode::INTERNAL_SERVER_ERROR, self.to_string()),
            AppError::Internal(_) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Internal server error".to_string(),
            ),
            AppError::Timeout => (StatusCode::REQUEST_TIMEOUT, self.to_string()),
        };

        let body = Json(json!({
            "message": "error",
            "status": status.as_u16(),
            "data": 0,
            "error": error_message,
        }));

        (status, body).into_response()
    }
}
