use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Serialize;

#[allow(dead_code)]
#[derive(Debug, Serialize)]
pub struct ApiError {
    pub code: &'static str,
    pub message: String,
}

#[allow(dead_code)]
#[derive(Debug)]
pub enum MemoryError {
    NotFound(String),
    ValidationFailed(String),
    DatabaseError(String),
    Conflict(String),
}

impl MemoryError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::NotFound(_) => "NOT_FOUND",
            Self::ValidationFailed(_) => "VALIDATION_FAILED",
            Self::DatabaseError(_) => "DATABASE_ERROR",
            Self::Conflict(_) => "CONFLICT",
        }
    }

    pub fn status(&self) -> StatusCode {
        match self {
            Self::NotFound(_) => StatusCode::NOT_FOUND,
            Self::ValidationFailed(_) => StatusCode::BAD_REQUEST,
            Self::DatabaseError(_) => StatusCode::INTERNAL_SERVER_ERROR,
            Self::Conflict(_) => StatusCode::CONFLICT,
        }
    }

    pub fn message(&self) -> &str {
        match self {
            Self::NotFound(m)
            | Self::ValidationFailed(m)
            | Self::DatabaseError(m)
            | Self::Conflict(m) => m,
        }
    }
}

impl IntoResponse for MemoryError {
    fn into_response(self) -> Response {
        let body = ApiError {
            code: self.code(),
            message: self.message().to_string(),
        };
        (self.status(), Json(body)).into_response()
    }
}

impl std::fmt::Display for MemoryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "[{}] {}", self.code(), self.message())
    }
}

impl From<anyhow::Error> for MemoryError {
    fn from(e: anyhow::Error) -> Self {
        Self::DatabaseError(e.to_string())
    }
}

impl From<rusqlite::Error> for MemoryError {
    fn from(e: rusqlite::Error) -> Self {
        Self::DatabaseError(e.to_string())
    }
}
