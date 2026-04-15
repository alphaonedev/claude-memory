// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_codes() {
        assert_eq!(MemoryError::NotFound("x".into()).code(), "NOT_FOUND");
        assert_eq!(
            MemoryError::ValidationFailed("x".into()).code(),
            "VALIDATION_FAILED"
        );
        assert_eq!(
            MemoryError::DatabaseError("x".into()).code(),
            "DATABASE_ERROR"
        );
        assert_eq!(MemoryError::Conflict("x".into()).code(), "CONFLICT");
    }

    #[test]
    fn error_status_codes() {
        assert_eq!(
            MemoryError::NotFound("x".into()).status(),
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            MemoryError::ValidationFailed("x".into()).status(),
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            MemoryError::DatabaseError("x".into()).status(),
            StatusCode::INTERNAL_SERVER_ERROR
        );
        assert_eq!(
            MemoryError::Conflict("x".into()).status(),
            StatusCode::CONFLICT
        );
    }

    #[test]
    fn error_messages() {
        assert_eq!(
            MemoryError::NotFound("not here".into()).message(),
            "not here"
        );
        assert_eq!(
            MemoryError::ValidationFailed("bad input".into()).message(),
            "bad input"
        );
    }

    #[test]
    fn error_display() {
        let err = MemoryError::NotFound("memory xyz".into());
        let display = format!("{err}");
        assert!(display.contains("NOT_FOUND"));
        assert!(display.contains("memory xyz"));
    }

    #[test]
    fn from_anyhow() {
        let err: MemoryError = anyhow::anyhow!("db broke").into();
        assert_eq!(err.code(), "DATABASE_ERROR");
        assert!(err.message().contains("db broke"));
    }

    #[test]
    fn api_error_serializes() {
        let api_err = ApiError {
            code: "TEST",
            message: "test msg".into(),
        };
        let json = serde_json::to_value(&api_err).unwrap();
        assert_eq!(json["code"], "TEST");
        assert_eq!(json["message"], "test msg");
    }
}
