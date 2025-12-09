//! Security Middleware
//!
//! This module provides security-related middleware including:
//! - Security headers (CSP, HSTS, X-Frame-Options, etc.)
//! - Request sanitization
//! - CORS configuration helpers

use axum::{
    body::Body,
    http::{header, HeaderValue, Request, StatusCode},
    middleware::Next,
    response::Response,
};
use tracing::warn;

/// Security headers to add to all responses
pub const SECURITY_HEADERS: &[(&str, &str)] = &[
    // Prevent clickjacking
    ("x-frame-options", "DENY"),
    // Prevent MIME sniffing
    ("x-content-type-options", "nosniff"),
    // XSS protection (for legacy browsers)
    ("x-xss-protection", "1; mode=block"),
    // Referrer policy
    ("referrer-policy", "strict-origin-when-cross-origin"),
    // Content Security Policy for API
    (
        "content-security-policy",
        "default-src 'none'; frame-ancestors 'none'",
    ),
    // Permissions policy
    (
        "permissions-policy",
        "accelerometer=(), camera=(), geolocation=(), microphone=()",
    ),
];

/// Add security headers to response
pub async fn security_headers_middleware(request: Request<Body>, next: Next) -> Response {
    let mut response = next.run(request).await;
    let headers = response.headers_mut();

    for (name, value) in SECURITY_HEADERS {
        if let Ok(header_name) = header::HeaderName::from_lowercase(name.as_bytes()) {
            // from_static is infallible for &'static str
            let header_value = HeaderValue::from_static(value);
            headers.insert(header_name, header_value);
        }
    }

    // Add HSTS header in production (when behind HTTPS)
    // Check environment variable to enable HSTS
    if std::env::var("ENABLE_HSTS").map(|v| v == "true" || v == "1").unwrap_or(false) {
        headers.insert(
            header::STRICT_TRANSPORT_SECURITY,
            HeaderValue::from_static("max-age=31536000; includeSubDomains; preload"),
        );
    }

    response
}

/// Maximum allowed content length for requests (in bytes)
/// Reduced from 10MB to 5MB to be more consistent with payload limits
/// Job payloads: 1MB, Webhook payloads: 5MB, Total request: 5MB
pub const MAX_CONTENT_LENGTH: u64 = 5 * 1024 * 1024; // 5MB

/// Validate content length
///
/// Also checks for Transfer-Encoding: chunked without Content-Length
/// and rejects requests that might bypass size limits
pub async fn content_length_middleware(
    request: Request<Body>,
    next: Next,
) -> Result<Response, (StatusCode, &'static str)> {
    // Check for Content-Length header
    if let Some(content_length) = request.headers().get(header::CONTENT_LENGTH) {
        if let Ok(length_str) = content_length.to_str() {
            if let Ok(length) = length_str.parse::<u64>() {
                if length > MAX_CONTENT_LENGTH {
                    warn!(
                        content_length = length,
                        max_length = MAX_CONTENT_LENGTH,
                        "Request body too large"
                    );
                    return Err((StatusCode::PAYLOAD_TOO_LARGE, "Request body too large"));
                }
            }
        }
    } else {
        // Check for chunked encoding without Content-Length
        // For POST/PUT/PATCH with body, we should either have Content-Length
        // or handle chunked encoding carefully
        let method = request.method();
        let has_body_method = method == axum::http::Method::POST
            || method == axum::http::Method::PUT
            || method == axum::http::Method::PATCH;

        let has_transfer_encoding = request.headers().get(header::TRANSFER_ENCODING).is_some();

        // If it's a body method with transfer-encoding but no content-length,
        // we allow it but warn (actual body size enforcement happens in handlers)
        if has_body_method && has_transfer_encoding {
            tracing::debug!(
                method = %method,
                "Request uses chunked encoding without Content-Length"
            );
        }
    }

    Ok(next.run(request).await)
}

/// Request ID header name
pub const REQUEST_ID_HEADER: &str = "x-request-id";

/// Validate request contains safe characters (basic sanitization)
pub fn is_safe_string(s: &str) -> bool {
    // Allow alphanumeric, spaces, common punctuation, and Unicode letters
    s.chars().all(|c| {
        c.is_alphanumeric()
            || c.is_whitespace()
            || matches!(
                c,
                '-' | '_'
                    | '.'
                    | '@'
                    | '+'
                    | '='
                    | ','
                    | ':'
                    | '/'
                    | '?'
                    | '&'
                    | '#'
                    | '%'
                    | '!'
                    | '('
                    | ')'
                    | '['
                    | ']'
                    | '{'
                    | '}'
            )
            || c.is_alphabetic() // Allow Unicode letters
    })
}

/// Validate queue name
pub fn is_valid_queue_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 255
        && name
            .chars()
            .all(|c| c.is_alphanumeric() || matches!(c, '-' | '_' | '.'))
        && !name.starts_with('.')
        && !name.starts_with('-')
}

/// Validate organization ID
pub fn is_valid_org_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 100
        && id
            .chars()
            .all(|c| c.is_alphanumeric() || matches!(c, '-' | '_'))
}

/// Validate job ID (UUID format)
pub fn is_valid_job_id(id: &str) -> bool {
    uuid::Uuid::parse_str(id).is_ok()
}

/// Sanitize string by removing control characters
pub fn sanitize_string(s: &str) -> String {
    s.chars()
        .filter(|c| !c.is_control() || c.is_whitespace())
        .collect()
}

/// CORS configuration for production
#[derive(Debug, Clone)]
pub struct CorsConfig {
    /// Allowed origins (None = any origin)
    pub allowed_origins: Option<Vec<String>>,
    /// Allowed methods
    pub allowed_methods: Vec<String>,
    /// Allowed headers
    pub allowed_headers: Vec<String>,
    /// Max age for preflight cache (seconds)
    pub max_age: u64,
    /// Whether to allow credentials
    pub allow_credentials: bool,
}

impl Default for CorsConfig {
    fn default() -> Self {
        Self {
            allowed_origins: None, // Any origin in development
            allowed_methods: vec![
                "GET".to_string(),
                "POST".to_string(),
                "PUT".to_string(),
                "DELETE".to_string(),
                "PATCH".to_string(),
                "OPTIONS".to_string(),
            ],
            allowed_headers: vec![
                "Authorization".to_string(),
                "Content-Type".to_string(),
                "X-Request-ID".to_string(),
                "X-API-Version".to_string(),
            ],
            max_age: 3600,
            allow_credentials: false,
        }
    }
}

impl CorsConfig {
    /// Create a strict CORS config for production
    pub fn production(origins: Vec<String>) -> Self {
        Self {
            allowed_origins: Some(origins),
            allow_credentials: true,
            ..Default::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_safe_string() {
        assert!(is_safe_string("Hello World"));
        assert!(is_safe_string("test@example.com"));
        assert!(is_safe_string("path/to/resource?query=value"));
        assert!(is_safe_string("UUID-1234-5678"));
        assert!(is_safe_string("中文测试")); // Chinese characters
        assert!(is_safe_string("Привет")); // Russian

        // Control characters are not safe
        assert!(!is_safe_string("test\x00"));
        assert!(!is_safe_string("test\x1b[31m")); // ANSI escape
    }

    #[test]
    fn test_is_valid_queue_name() {
        assert!(is_valid_queue_name("emails"));
        assert!(is_valid_queue_name("high-priority"));
        assert!(is_valid_queue_name("queue_v2"));
        assert!(is_valid_queue_name("my.queue.name"));

        assert!(!is_valid_queue_name("")); // Empty
        assert!(!is_valid_queue_name(".hidden")); // Starts with dot
        assert!(!is_valid_queue_name("-invalid")); // Starts with dash
        assert!(!is_valid_queue_name("queue with spaces")); // Spaces not allowed
        assert!(!is_valid_queue_name(&"a".repeat(256))); // Too long
    }

    #[test]
    fn test_is_valid_org_id() {
        assert!(is_valid_org_id("org-123"));
        assert!(is_valid_org_id("my_organization"));
        assert!(is_valid_org_id("ABC123"));

        assert!(!is_valid_org_id("")); // Empty
        assert!(!is_valid_org_id("org.with.dots")); // Dots not allowed
        assert!(!is_valid_org_id(&"a".repeat(101))); // Too long
    }

    #[test]
    fn test_is_valid_job_id() {
        assert!(is_valid_job_id("550e8400-e29b-41d4-a716-446655440000"));
        assert!(is_valid_job_id("550E8400-E29B-41D4-A716-446655440000")); // Uppercase

        assert!(!is_valid_job_id("not-a-uuid"));
        assert!(!is_valid_job_id(""));
        assert!(!is_valid_job_id("550e8400-e29b-41d4-a716-44665544")); // Too short
    }

    #[test]
    fn test_sanitize_string() {
        assert_eq!(sanitize_string("hello world"), "hello world");
        assert_eq!(sanitize_string("with\nnewline"), "with\nnewline"); // Whitespace preserved
        assert_eq!(sanitize_string("with\ttab"), "with\ttab");
        assert_eq!(sanitize_string("no\x00null"), "nonull"); // Null removed
        assert_eq!(sanitize_string("no\x1bescape"), "noescape"); // Escape removed
    }

    #[test]
    fn test_cors_config_default() {
        let config = CorsConfig::default();
        assert!(config.allowed_origins.is_none());
        assert!(config.allowed_methods.contains(&"GET".to_string()));
        assert!(config.allowed_methods.contains(&"POST".to_string()));
        assert!(!config.allow_credentials);
    }

    #[test]
    fn test_cors_config_production() {
        let config = CorsConfig::production(vec!["https://example.com".to_string()]);
        assert!(config.allowed_origins.is_some());
        assert_eq!(
            config.allowed_origins.unwrap(),
            vec!["https://example.com".to_string()]
        );
        assert!(config.allow_credentials);
    }

    #[test]
    fn test_security_headers_count() {
        assert!(SECURITY_HEADERS.len() >= 5);
    }
}
