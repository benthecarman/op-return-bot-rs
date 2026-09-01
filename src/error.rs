use axum::{
    Json,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde_json::json;
use thiserror::Error;

pub type AppResult<T> = Result<T, AppError>;

#[derive(Debug, Error)]
pub enum AppError {
    #[error("configuration error: {0}")]
    Config(String),
    #[error("database error: {0}")]
    Database(#[from] sqlx::Error),
    #[error("database migration error: {0}")]
    Migration(#[from] sqlx::migrate::MigrateError),
    #[error("invalid request: {0}")]
    InvalidRequest(String),
    #[error("too many requests")]
    RateLimited,
    #[error("not found: {0}")]
    NotFound(String),
    #[error("upstream service error: {0}")]
    Upstream(String),
    #[error("bitcoin data error: {0}")]
    Bitcoin(#[from] bitcoin::consensus::encode::Error),
    #[error("internal error: {0}")]
    Internal(String),
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let status = match self {
            Self::InvalidRequest(_) => StatusCode::BAD_REQUEST,
            Self::RateLimited => StatusCode::TOO_MANY_REQUESTS,
            Self::NotFound(_) => StatusCode::NOT_FOUND,
            Self::Config(_)
            | Self::Database(_)
            | Self::Migration(_)
            | Self::Bitcoin(_)
            | Self::Upstream(_)
            | Self::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
        };

        let public = matches!(
            self,
            Self::InvalidRequest(_) | Self::RateLimited | Self::NotFound(_)
        );
        if !public {
            tracing::error!(error = %self, "request failed");
        }
        let message = if public {
            self.to_string()
        } else {
            "internal error".to_owned()
        };
        (status, Json(json!({ "error": message }))).into_response()
    }
}
