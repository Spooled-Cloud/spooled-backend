//! Request validation middleware using the validator crate
//!
//! Provides a custom Axum extractor that automatically validates request bodies.

use axum::{
    extract::{FromRequest, Request},
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde::de::DeserializeOwned;
use validator::Validate;

/// A validated JSON request body
///
/// This extractor will parse the request body as JSON and validate it using
/// the validator crate. If validation fails, it returns a 400 Bad Request
/// with detailed error information.
#[derive(Debug, Clone, Copy, Default)]
pub struct ValidatedJson<T>(pub T);

/// Validation error response
#[derive(Debug, serde::Serialize)]
pub struct ValidationErrorResponse {
    pub error: String,
    pub code: String,
    pub details: Vec<ValidationFieldError>,
}

/// Individual field validation error
#[derive(Debug, serde::Serialize)]
pub struct ValidationFieldError {
    pub field: String,
    pub message: String,
    pub code: Option<String>,
}

/// Validation rejection type
pub struct ValidationRejection {
    errors: validator::ValidationErrors,
}

impl IntoResponse for ValidationRejection {
    fn into_response(self) -> Response {
        let field_errors: Vec<ValidationFieldError> = self
            .errors
            .field_errors()
            .iter()
            .flat_map(|(field, errors)| {
                errors.iter().map(move |error| ValidationFieldError {
                    field: field.to_string(),
                    message: error
                        .message
                        .as_ref()
                        .map(|m| m.to_string())
                        .unwrap_or_else(|| format!("Validation failed for field '{}'", field)),
                    code: error.code.to_string().into(),
                })
            })
            .collect();

        let response = ValidationErrorResponse {
            error: "Validation failed".to_string(),
            code: "VALIDATION_ERROR".to_string(),
            details: field_errors,
        };

        (StatusCode::BAD_REQUEST, Json(response)).into_response()
    }
}

impl<S, T> FromRequest<S> for ValidatedJson<T>
where
    T: DeserializeOwned + Validate,
    S: Send + Sync,
{
    type Rejection = Response;

    async fn from_request(req: Request, state: &S) -> Result<Self, Self::Rejection> {
        // First, extract as JSON
        // Sanitize parse error messages to avoid exposing internal details
        let Json(value) = Json::<T>::from_request(req, state).await.map_err(|e| {
            // Log the actual error but return sanitized message
            tracing::debug!(error = %e.body_text(), "JSON parse error");

            // Sanitize the error message to avoid exposing internal structure
            let sanitized_message = if e.body_text().contains("expected") {
                "Invalid JSON format".to_string()
            } else if e.body_text().contains("missing field") {
                "Missing required field".to_string()
            } else if e.body_text().contains("EOF") || e.body_text().contains("empty") {
                "Request body is empty or incomplete".to_string()
            } else {
                "Invalid request body".to_string()
            };

            let response = ValidationErrorResponse {
                error: "Invalid JSON".to_string(),
                code: "INVALID_JSON".to_string(),
                details: vec![ValidationFieldError {
                    field: "body".to_string(),
                    message: sanitized_message, // Use sanitized message
                    code: Some("PARSE_ERROR".to_string()),
                }],
            };
            (StatusCode::BAD_REQUEST, Json(response)).into_response()
        })?;

        // Then validate
        value
            .validate()
            .map_err(|errors| ValidationRejection { errors }.into_response())?;

        Ok(ValidatedJson(value))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_validation_error_serialization() {
        let response = ValidationErrorResponse {
            error: "Validation failed".to_string(),
            code: "VALIDATION_ERROR".to_string(),
            details: vec![ValidationFieldError {
                field: "name".to_string(),
                message: "Name is required".to_string(),
                code: Some("required".to_string()),
            }],
        };

        let json = serde_json::to_string(&response).unwrap();
        assert!(json.contains("VALIDATION_ERROR"));
        assert!(json.contains("Name is required"));
    }
}
