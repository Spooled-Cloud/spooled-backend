//! One JSON shape for every error response.
//!
//! Most handlers return `AppError`, which serializes as `{"code", "message",
//! "details"?}`. Several paths did not: axum's own extractor rejections (plain
//! text such as "Failed to deserialize the JSON body ..."), handlers that return
//! `(StatusCode, String)` (plain text such as "Schedule not found"), the router's
//! empty 404/405 bodies, and older JSON bodies shaped `{"error", "message"}` or
//! `{"error", "code", "details"}`. Clients had to handle four shapes.
//!
//! [`normalize_error_body`] runs on every response with status >= 400 and makes
//! sure the body is JSON carrying at least `code` and `message`. Existing JSON
//! fields are kept (`error`, `details`, `resource`, `limit`, ...), so nothing a
//! client already reads disappears; plain-text and empty bodies are wrapped.

use axum::{
    body::{to_bytes, Body},
    http::{header, HeaderValue, Request, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
    Json,
};

use crate::error::ErrorResponse;

/// Error bodies are small; anything larger is passed through untouched.
const MAX_ERROR_BODY_BYTES: usize = 64 * 1024;

/// Stable code for a status when the body does not carry one. Matches the codes
/// `AppError` already emits for the same statuses.
fn code_for_status(status: StatusCode) -> &'static str {
    match status {
        StatusCode::BAD_REQUEST => "BAD_REQUEST",
        StatusCode::UNAUTHORIZED => "UNAUTHORIZED",
        StatusCode::FORBIDDEN => "ACCESS_DENIED",
        StatusCode::NOT_FOUND => "NOT_FOUND",
        StatusCode::METHOD_NOT_ALLOWED => "METHOD_NOT_ALLOWED",
        StatusCode::REQUEST_TIMEOUT => "REQUEST_TIMEOUT",
        StatusCode::CONFLICT => "CONFLICT",
        StatusCode::PAYLOAD_TOO_LARGE => "PAYLOAD_TOO_LARGE",
        StatusCode::UNSUPPORTED_MEDIA_TYPE => "UNSUPPORTED_MEDIA_TYPE",
        StatusCode::UNPROCESSABLE_ENTITY => "VALIDATION_ERROR",
        StatusCode::TOO_MANY_REQUESTS => "RATE_LIMIT_EXCEEDED",
        StatusCode::SERVICE_UNAVAILABLE => "SERVICE_UNAVAILABLE",
        s if s.is_server_error() => "INTERNAL_ERROR",
        _ => "ERROR",
    }
}

fn default_message(status: StatusCode) -> String {
    status
        .canonical_reason()
        .unwrap_or("Request failed")
        .to_string()
}

/// Upper-snake-case an `error` value like `"feature_disabled"` into a code.
/// Free-text values ("Validation failed") are not codes and return `None`.
fn code_from_error_field(error: &str) -> Option<String> {
    let looks_like_code = !error.is_empty()
        && error.len() <= 64
        && error
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '.');
    looks_like_code.then(|| error.replace(['-', '.'], "_").to_ascii_uppercase())
}

/// Produce the normalized body for an error response, or `None` if the body is
/// already in the standard shape.
fn normalized_body(status: StatusCode, is_json: bool, bytes: &[u8]) -> Option<Vec<u8>> {
    if is_json {
        if let Ok(serde_json::Value::Object(mut obj)) = serde_json::from_slice(bytes) {
            let has_code = obj.get("code").is_some_and(|v| v.is_string());
            let has_message = obj.get("message").is_some_and(|v| v.is_string());
            if has_code && has_message {
                return None;
            }
            let error = obj
                .get("error")
                .and_then(|v| v.as_str())
                .map(str::to_string);
            if !has_code {
                let code = error
                    .as_deref()
                    .and_then(code_from_error_field)
                    .unwrap_or_else(|| code_for_status(status).to_string());
                obj.insert("code".into(), code.into());
            }
            if !has_message {
                // Prefer the first field-level message (validation errors), then a
                // free-text `error`, then the status reason.
                let detail = obj
                    .get("details")
                    .and_then(|d| d.as_array())
                    .and_then(|d| d.first())
                    .and_then(|d| {
                        let message = d.get("message")?.as_str()?;
                        Some(match d.get("field").and_then(|f| f.as_str()) {
                            Some(field) if !message.contains(field) => {
                                format!("{}: {}", field, message)
                            }
                            _ => message.to_string(),
                        })
                    });
                let message = detail.or(error).unwrap_or_else(|| default_message(status));
                obj.insert("message".into(), message.into());
            }
            return serde_json::to_vec(&serde_json::Value::Object(obj)).ok();
        }
    }

    // Plain text, empty, or unparseable: wrap the text as the message.
    let text = String::from_utf8_lossy(bytes).trim().to_string();
    let message = if text.is_empty() {
        default_message(status)
    } else {
        text
    };
    serde_json::to_vec(&ErrorResponse {
        code: code_for_status(status).to_string(),
        message,
        details: None,
    })
    .ok()
}

/// Rewrite non-standard error bodies into `{"code", "message", ...}` JSON.
pub async fn normalize_error_body(request: Request<Body>, next: Next) -> Response {
    let response = next.run(request).await;
    let status = response.status();
    if !(status.is_client_error() || status.is_server_error()) {
        return response;
    }

    let is_json = response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|ct| ct.starts_with("application/json"));

    let (mut parts, body) = response.into_parts();
    let bytes = match to_bytes(body, MAX_ERROR_BODY_BYTES).await {
        Ok(bytes) => bytes,
        // Oversized or failed body: nothing sensible to rewrite.
        Err(_) => {
            return (
                parts.status,
                Json(ErrorResponse {
                    code: code_for_status(parts.status).to_string(),
                    message: default_message(parts.status),
                    details: None,
                }),
            )
                .into_response()
        }
    };

    match normalized_body(status, is_json, &bytes) {
        None => Response::from_parts(parts, Body::from(bytes)),
        Some(new_body) => {
            parts.headers.insert(
                header::CONTENT_TYPE,
                HeaderValue::from_static("application/json"),
            );
            parts.headers.remove(header::CONTENT_LENGTH);
            Response::from_parts(parts, Body::from(new_body))
        }
    }
}

/// JSON 404 for paths no route matches (axum's default is an empty body).
pub async fn route_not_found() -> Response {
    (
        StatusCode::NOT_FOUND,
        Json(ErrorResponse {
            code: "NOT_FOUND".to_string(),
            message: "No route matches this path".to_string(),
            details: None,
        }),
    )
        .into_response()
}

/// JSON 405 for a known path called with the wrong method.
pub async fn method_not_allowed() -> Response {
    (
        StatusCode::METHOD_NOT_ALLOWED,
        Json(ErrorResponse {
            code: "METHOD_NOT_ALLOWED".to_string(),
            message: "This path does not support the request method".to_string(),
            details: None,
        }),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(bytes: Vec<u8>) -> serde_json::Value {
        serde_json::from_slice(&bytes).unwrap()
    }

    #[test]
    fn standard_body_is_left_alone() {
        let body = br#"{"code":"NOT_FOUND","message":"Resource not found"}"#;
        assert!(normalized_body(StatusCode::NOT_FOUND, true, body).is_none());
    }

    #[test]
    fn plain_text_is_wrapped() {
        let out =
            parse(normalized_body(StatusCode::NOT_FOUND, false, b"Schedule not found").unwrap());
        assert_eq!(out["code"], "NOT_FOUND");
        assert_eq!(out["message"], "Schedule not found");
    }

    #[test]
    fn empty_body_gets_reason_phrase() {
        let out = parse(normalized_body(StatusCode::METHOD_NOT_ALLOWED, false, b"").unwrap());
        assert_eq!(out["code"], "METHOD_NOT_ALLOWED");
        assert_eq!(out["message"], "Method Not Allowed");
    }

    #[test]
    fn legacy_error_message_body_gains_code_and_keeps_error() {
        let body = br#"{"error":"unauthorized","message":"Invalid credentials"}"#;
        let out = parse(normalized_body(StatusCode::UNAUTHORIZED, true, body).unwrap());
        assert_eq!(out["code"], "UNAUTHORIZED");
        assert_eq!(out["message"], "Invalid credentials");
        assert_eq!(out["error"], "unauthorized");
    }

    #[test]
    fn validation_body_gains_message_from_first_detail() {
        let body = br#"{"error":"Validation failed","code":"VALIDATION_ERROR","details":[{"field":"name","message":"Name must be 1-100 characters","code":"length"}]}"#;
        let out = parse(normalized_body(StatusCode::BAD_REQUEST, true, body).unwrap());
        assert_eq!(out["code"], "VALIDATION_ERROR");
        assert_eq!(out["message"], "name: Name must be 1-100 characters");
        assert!(out["details"].is_array());
    }

    #[test]
    fn free_text_error_is_not_turned_into_a_code() {
        let body = br#"{"error":"Something broke badly"}"#;
        let out = parse(normalized_body(StatusCode::INTERNAL_SERVER_ERROR, true, body).unwrap());
        assert_eq!(out["code"], "INTERNAL_ERROR");
        assert_eq!(out["message"], "Something broke badly");
    }

    #[test]
    fn snake_case_error_becomes_code() {
        let body = br#"{"error":"feature_disabled","message":"x","feature":"workflows"}"#;
        let out = parse(normalized_body(StatusCode::FORBIDDEN, true, body).unwrap());
        assert_eq!(out["code"], "FEATURE_DISABLED");
        assert_eq!(out["feature"], "workflows");
    }
}
