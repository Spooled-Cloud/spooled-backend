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

/// Parse a serde_json error message and produce a structured field error.
///
/// `serde_json` body-text from Axum looks like one of:
///   * `Failed to deserialize the JSON body into the target type: missing field `queue_name` at line 1 column 2`
///   * `Failed to deserialize the JSON body into the target type: invalid type: integer `123`, expected a string at line 1 column 18`
///   * `Failed to parse the request body as JSON: EOF while parsing a value at line 1 column 0`
///
/// We extract the actionable bit (field name + concise message) so callers can
/// fix their request without guessing. The previous "sanitized" implementation
/// returned `Invalid JSON / Missing required field` for ALL of these, which was
/// both inaccurate (the JSON parsed fine; a *field* was missing) and unhelpful
/// (it never told the caller *which* field).
fn parse_json_error(body_text: &str) -> (StatusCode, ValidationErrorResponse) {
    let lower = body_text.to_ascii_lowercase();

    // missing field `name`
    if let Some(rest) = body_text.split("missing field `").nth(1) {
        if let Some(field) = rest.split('`').next() {
            return (
                StatusCode::UNPROCESSABLE_ENTITY,
                ValidationErrorResponse {
                    error: "Validation failed".to_string(),
                    code: "VALIDATION_ERROR".to_string(),
                    details: vec![ValidationFieldError {
                        field: field.to_string(),
                        message: format!("Field `{}` is required", field),
                        code: Some("missing_field".to_string()),
                    }],
                },
            );
        }
    }

    // invalid type: ..., expected ...
    if let Some(rest) = body_text.split("invalid type:").nth(1) {
        // Try to find the field via the last "field `X`" hint, otherwise leave blank.
        let field = body_text
            .rsplit("field `")
            .next()
            .and_then(|s| s.split('`').next())
            .filter(|s| !s.contains(' ') && !s.is_empty())
            .unwrap_or("body")
            .to_string();
        let msg = rest
            .split(" at line")
            .next()
            .unwrap_or(rest)
            .trim()
            .trim_end_matches(',');
        return (
            StatusCode::UNPROCESSABLE_ENTITY,
            ValidationErrorResponse {
                error: "Validation failed".to_string(),
                code: "VALIDATION_ERROR".to_string(),
                details: vec![ValidationFieldError {
                    field,
                    message: format!("Invalid type: {}", msg),
                    code: Some("invalid_type".to_string()),
                }],
            },
        );
    }

    // empty / EOF
    if lower.contains("eof") || lower.contains("empty") {
        return (
            StatusCode::BAD_REQUEST,
            ValidationErrorResponse {
                error: "Invalid JSON".to_string(),
                code: "INVALID_JSON".to_string(),
                details: vec![ValidationFieldError {
                    field: "body".to_string(),
                    message: "Request body is empty".to_string(),
                    code: Some("empty_body".to_string()),
                }],
            },
        );
    }

    // Generic syntactic JSON error.
    let trimmed = body_text
        .split_once(": ")
        .map(|(_, rest)| rest)
        .unwrap_or(body_text);
    let message = trimmed
        .split(" at line")
        .next()
        .unwrap_or(trimmed)
        .trim()
        .to_string();

    (
        StatusCode::BAD_REQUEST,
        ValidationErrorResponse {
            error: "Invalid JSON".to_string(),
            code: "INVALID_JSON".to_string(),
            details: vec![ValidationFieldError {
                field: "body".to_string(),
                message: if message.is_empty() {
                    "Malformed JSON".to_string()
                } else {
                    message
                },
                code: Some("invalid_json".to_string()),
            }],
        },
    )
}

impl<S, T> FromRequest<S> for ValidatedJson<T>
where
    T: DeserializeOwned + Validate,
    S: Send + Sync,
{
    type Rejection = Response;

    async fn from_request(req: Request, state: &S) -> Result<Self, Self::Rejection> {
        let Json(value) = Json::<T>::from_request(req, state).await.map_err(|e| {
            let body_text = e.body_text();
            tracing::debug!(error = %body_text, "JSON parse error");
            let (status, response) = parse_json_error(&body_text);
            (status, Json(response)).into_response()
        })?;

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

    #[test]
    fn test_parse_missing_field() {
        let body = "Failed to deserialize the JSON body into the target type: missing field `queue_name` at line 1 column 2";
        let (status, resp) = parse_json_error(body);
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(resp.code, "VALIDATION_ERROR");
        assert_eq!(resp.details[0].field, "queue_name");
        assert!(resp.details[0].message.contains("queue_name"));
        assert_eq!(resp.details[0].code.as_deref(), Some("missing_field"));
    }

    #[test]
    fn test_parse_invalid_type() {
        let body = "Failed to deserialize the JSON body into the target type: invalid type: integer `123`, expected a string at line 1 column 18";
        let (status, resp) = parse_json_error(body);
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        assert!(resp.details[0].message.contains("expected a string"));
        assert_eq!(resp.details[0].code.as_deref(), Some("invalid_type"));
    }

    #[test]
    fn test_parse_empty_body() {
        let body = "Failed to parse the request body as JSON: EOF while parsing a value at line 1 column 0";
        let (status, resp) = parse_json_error(body);
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(resp.code, "INVALID_JSON");
        assert!(resp.details[0].message.to_lowercase().contains("empty"));
    }
}
