//! Error handling module for Spooled Backend
//!
//! This module defines the error types and conversion implementations
//! for consistent error handling across the application.

use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde::Serialize;
use thiserror::Error;
use tracing::error;
use validator::ValidationErrors;

/// Application-wide error type
#[derive(Error, Debug)]
pub enum AppError {
    /// Database errors
    #[error("Database error: {0}")]
    Database(#[from] sqlx::Error),

    /// Authentication errors
    #[error("Authentication failed: {0}")]
    Authentication(String),

    /// Unauthorized (missing authentication)
    #[error("Unauthorized: {0}")]
    Unauthorized(String),

    /// Authorization errors
    #[error("Access denied: {0}")]
    Authorization(String),

    /// Validation errors
    #[error("Validation failed: {0}")]
    Validation(String),

    /// Resource not found
    #[error("Resource not found: {0}")]
    NotFound(String),

    /// Conflict (e.g., duplicate resource)
    #[error("Conflict: {0}")]
    Conflict(String),

    /// Rate limit exceeded with optional retry-after seconds
    #[error("Rate limit exceeded")]
    RateLimitExceeded,

    /// Rate limit exceeded with retry-after header
    #[error("Rate limit exceeded, retry after {0} seconds")]
    RateLimitExceededWithRetry(u64),

    /// Payload too large
    #[error("Payload too large")]
    PayloadTooLarge,

    /// Bad request
    #[error("Bad request: {0}")]
    BadRequest(String),

    /// Redis errors
    #[error("Cache error: {0}")]
    Cache(String),

    /// Internal server error
    #[error("Internal error: {0}")]
    Internal(String),

    /// Configuration error
    #[error("Configuration error: {0}")]
    Configuration(String),

    /// Plan limit exceeded (wraps a pre-built Response)
    #[error("Plan limit exceeded")]
    LimitExceeded(Response),
}

/// Error response body
#[derive(Serialize)]
pub struct ErrorResponse {
    /// Error code for programmatic handling
    pub code: String,
    /// Human-readable error message
    pub message: String,
    /// Optional additional details
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<serde_json::Value>,
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        // Handle pre-built limit exceeded response
        if let AppError::LimitExceeded(response) = self {
            return response;
        }

        // Check for rate limit with retry-after
        if let AppError::RateLimitExceededWithRetry(retry_after) = &self {
            let body = Json(ErrorResponse {
                code: "RATE_LIMIT_EXCEEDED".to_string(),
                message: format!(
                    "Too many requests, please retry after {} seconds",
                    retry_after
                ),
                details: Some(serde_json::json!({ "retry_after_seconds": retry_after })),
            });

            return (
                StatusCode::TOO_MANY_REQUESTS,
                [(
                    axum::http::header::RETRY_AFTER,
                    axum::http::HeaderValue::from_str(&retry_after.to_string())
                        .unwrap_or_else(|_| axum::http::HeaderValue::from_static("60")),
                )],
                body,
            )
                .into_response();
        }

        // Sanitize error messages to prevent information disclosure
        // User-facing messages should not contain internal details
        let (status, code, message) = match &self {
            AppError::Database(e) => {
                error!(error = %e, "Database error");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "DATABASE_ERROR",
                    "An internal database error occurred".to_string(),
                )
            }
            AppError::Authentication(msg) => {
                // Don't reveal specific authentication failure reasons
                // to prevent enumeration attacks
                let safe_msg =
                    if msg.contains("key") || msg.contains("token") || msg.contains("password") {
                        "Authentication failed".to_string()
                    } else {
                        msg.clone()
                    };
                (StatusCode::UNAUTHORIZED, "AUTHENTICATION_FAILED", safe_msg)
            }
            AppError::Unauthorized(msg) => (StatusCode::UNAUTHORIZED, "UNAUTHORIZED", msg.clone()),
            AppError::Authorization(msg) => (StatusCode::FORBIDDEN, "ACCESS_DENIED", msg.clone()),
            AppError::Validation(msg) => (StatusCode::BAD_REQUEST, "VALIDATION_ERROR", msg.clone()),
            AppError::NotFound(msg) => {
                // Don't reveal internal IDs in not found messages
                let safe_msg = if msg.contains("not found") {
                    "Resource not found".to_string()
                } else {
                    msg.clone()
                };
                (StatusCode::NOT_FOUND, "NOT_FOUND", safe_msg)
            }
            AppError::Conflict(msg) => (StatusCode::CONFLICT, "CONFLICT", msg.clone()),
            AppError::RateLimitExceeded => (
                StatusCode::TOO_MANY_REQUESTS,
                "RATE_LIMIT_EXCEEDED",
                "Too many requests, please try again later".to_string(),
            ),
            AppError::RateLimitExceededWithRetry(_) | AppError::LimitExceeded(_) => {
                // Already handled above
                unreachable!()
            }
            AppError::PayloadTooLarge => (
                StatusCode::PAYLOAD_TOO_LARGE,
                "PAYLOAD_TOO_LARGE",
                "Request payload exceeds maximum allowed size".to_string(),
            ),
            AppError::BadRequest(msg) => {
                // Sanitize bad request messages that might contain SQL or system info
                let safe_msg =
                    if msg.contains("SQL") || msg.contains("query") || msg.contains("column") {
                        "Invalid request".to_string()
                    } else {
                        msg.clone()
                    };
                (StatusCode::BAD_REQUEST, "BAD_REQUEST", safe_msg)
            }
            AppError::Cache(msg) => {
                error!(error = %msg, "Cache error");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "CACHE_ERROR",
                    "An internal cache error occurred".to_string(),
                )
            }
            AppError::Internal(msg) => {
                error!(error = %msg, "Internal error");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "INTERNAL_ERROR",
                    "An internal error occurred".to_string(),
                )
            }
            AppError::Configuration(msg) => {
                error!(error = %msg, "Configuration error");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "CONFIGURATION_ERROR",
                    "A configuration error occurred".to_string(),
                )
            }
        };

        let body = Json(ErrorResponse {
            code: code.to_string(),
            message,
            details: None,
        });

        (status, body).into_response()
    }
}

/// Result type alias for AppError
pub type AppResult<T> = Result<T, AppError>;

impl From<anyhow::Error> for AppError {
    fn from(err: anyhow::Error) -> Self {
        AppError::Internal(err.to_string())
    }
}

impl From<redis::RedisError> for AppError {
    fn from(err: redis::RedisError) -> Self {
        AppError::Cache(err.to_string())
    }
}

impl From<ValidationErrors> for AppError {
    fn from(err: ValidationErrors) -> Self {
        let messages: Vec<String> = err
            .field_errors()
            .iter()
            .flat_map(|(field, errors)| {
                errors.iter().map(move |e| {
                    e.message
                        .as_ref()
                        .map(|m| m.to_string())
                        .unwrap_or_else(|| format!("Invalid value for '{}'", field))
                })
            })
            .collect();
        AppError::Validation(messages.join("; "))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_error_response_serialization() {
        let response = ErrorResponse {
            code: "TEST_ERROR".to_string(),
            message: "Test message".to_string(),
            details: None,
        };

        let json = serde_json::to_string(&response).unwrap();
        assert!(json.contains("TEST_ERROR"));
        assert!(json.contains("Test message"));
    }

    #[test]
    fn test_app_error_display() {
        let error = AppError::NotFound("Job xyz not found".to_string());
        assert_eq!(error.to_string(), "Resource not found: Job xyz not found");
    }

    #[test]
    fn test_all_error_variants_display() {
        let errors = vec![
            (
                AppError::Database(sqlx::Error::RowNotFound),
                "Database error",
            ),
            (
                AppError::Authentication("bad credentials".to_string()),
                "Authentication failed",
            ),
            (
                AppError::Unauthorized("missing token".to_string()),
                "Unauthorized",
            ),
            (
                AppError::Authorization("not allowed".to_string()),
                "Access denied",
            ),
            (
                AppError::Validation("invalid email".to_string()),
                "Validation failed",
            ),
            (
                AppError::NotFound("resource missing".to_string()),
                "Resource not found",
            ),
            (AppError::Conflict("duplicate".to_string()), "Conflict"),
            (AppError::RateLimitExceeded, "Rate limit exceeded"),
            (AppError::PayloadTooLarge, "Payload too large"),
            (AppError::BadRequest("bad input".to_string()), "Bad request"),
            (AppError::Cache("redis error".to_string()), "Cache error"),
            (
                AppError::Internal("server error".to_string()),
                "Internal error",
            ),
            (
                AppError::Configuration("bad config".to_string()),
                "Configuration error",
            ),
        ];

        for (error, expected_prefix) in errors {
            assert!(
                error.to_string().contains(expected_prefix),
                "Error '{}' should contain '{}'",
                error.to_string(),
                expected_prefix
            );
        }
    }

    #[test]
    fn test_error_response_with_details() {
        let response = ErrorResponse {
            code: "VALIDATION_ERROR".to_string(),
            message: "Invalid input".to_string(),
            details: Some(serde_json::json!({
                "field": "email",
                "reason": "invalid format"
            })),
        };

        let json = serde_json::to_string(&response).unwrap();
        assert!(json.contains("VALIDATION_ERROR"));
        assert!(json.contains("email"));
        assert!(json.contains("invalid format"));
    }

    #[test]
    fn test_error_response_skips_null_details() {
        let response = ErrorResponse {
            code: "NOT_FOUND".to_string(),
            message: "Resource not found".to_string(),
            details: None,
        };

        let json = serde_json::to_string(&response).unwrap();
        assert!(!json.contains("details"));
    }

    #[test]
    fn test_from_anyhow_error() {
        let anyhow_err = anyhow::anyhow!("Something went wrong");
        let app_error: AppError = anyhow_err.into();

        match app_error {
            AppError::Internal(msg) => assert!(msg.contains("Something went wrong")),
            _ => panic!("Expected Internal error"),
        }
    }

    #[test]
    fn test_from_validation_errors() {
        let mut errors = validator::ValidationErrors::new();
        errors.add("email", validator::ValidationError::new("invalid"));

        let app_error: AppError = errors.into();

        match app_error {
            AppError::Validation(msg) => assert!(msg.contains("email")),
            _ => panic!("Expected Validation error"),
        }
    }

    #[test]
    fn test_error_status_codes() {
        use axum::response::IntoResponse;

        // Test that errors produce correct status codes
        let test_cases = vec![
            (AppError::NotFound("test".to_string()), 404),
            (AppError::BadRequest("test".to_string()), 400),
            (AppError::Validation("test".to_string()), 400),
            (AppError::Unauthorized("test".to_string()), 401),
            (AppError::Authentication("test".to_string()), 401),
            (AppError::Authorization("test".to_string()), 403),
            (AppError::Conflict("test".to_string()), 409),
            (AppError::RateLimitExceeded, 429),
            (AppError::RateLimitExceededWithRetry(60), 429),
            (AppError::PayloadTooLarge, 413),
            (AppError::Internal("test".to_string()), 500),
            (AppError::Cache("test".to_string()), 500),
            (AppError::Configuration("test".to_string()), 500),
        ];

        for (error, expected_status) in test_cases {
            let response = error.into_response();
            assert_eq!(
                response.status().as_u16(),
                expected_status,
                "Error should return status {}",
                expected_status
            );
        }
    }

    #[test]
    fn test_rate_limit_with_retry_after_header() {
        use axum::response::IntoResponse;

        let error = AppError::RateLimitExceededWithRetry(30);
        let response = error.into_response();

        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);

        // Check for Retry-After header
        let retry_after = response.headers().get(axum::http::header::RETRY_AFTER);
        assert!(
            retry_after.is_some(),
            "Response should have Retry-After header"
        );
        assert_eq!(retry_after.unwrap().to_str().unwrap(), "30");
    }
}
