//! One error type for every handler. Renders `{ "error": code, "message": text }`.

use axum::extract::rejection::{JsonRejection, PathRejection, QueryRejection};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Serialize;
use utoipa::ToSchema;

/// The body of every error response.
#[derive(Serialize, ToSchema)]
pub struct ErrorBody {
    /// Machine-readable code: `bad_request`, `forbidden`, `not_in_pool`, `entry_not_found` or `internal`.
    pub error: String,
    /// Human-readable detail.
    pub message: String,
}

#[derive(Debug, thiserror::Error)]
pub enum AppError {
    #[error("caller secret missing or wrong")]
    Forbidden,
    #[error("{0}")]
    BadRequest(String),
    #[error("user is not in the match pool")]
    NotInPool,
    #[error("no such current entry for this user")]
    EntryNotFound,
    #[error("database error")]
    Sqlx(#[from] sqlx::Error),
}

/// Extractor rejections would otherwise render as plain text with axum's own
/// status codes (415 / 422); they all become the JSON `bad_request` envelope.
macro_rules! bad_request_from_rejection {
    ($($rejection:ty),+) => {$(
        impl From<$rejection> for AppError {
            fn from(rej: $rejection) -> Self {
                AppError::BadRequest(rej.body_text())
            }
        }
    )+};
}

bad_request_from_rejection!(JsonRejection, PathRejection, QueryRejection);

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let (status, code) = match &self {
            AppError::Forbidden => (StatusCode::FORBIDDEN, "forbidden"),
            AppError::BadRequest(_) => (StatusCode::BAD_REQUEST, "bad_request"),
            AppError::NotInPool => (StatusCode::NOT_FOUND, "not_in_pool"),
            AppError::EntryNotFound => (StatusCode::NOT_FOUND, "entry_not_found"),
            AppError::Sqlx(e) => {
                tracing::error!(error = %e, "database error");
                (StatusCode::INTERNAL_SERVER_ERROR, "internal")
            }
        };
        (
            status,
            Json(ErrorBody {
                error: code.into(),
                message: self.to_string(),
            }),
        )
            .into_response()
    }
}
