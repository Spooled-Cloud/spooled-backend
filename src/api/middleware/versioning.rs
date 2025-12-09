//! API Versioning Middleware
//!
//! This module provides middleware for API versioning through headers.
//! The current API version is v1, and clients can request specific versions
//! using the X-API-Version header.

use axum::{
    body::Body,
    http::{header::HeaderName, HeaderValue, Request, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
};
use tracing::debug;

/// Current API version
pub const CURRENT_API_VERSION: &str = "1";

/// Supported API versions
pub const SUPPORTED_VERSIONS: &[&str] = &["1"];

/// Header name for API version request
pub const API_VERSION_REQUEST_HEADER: &str = "x-api-version";

/// Header name for API version response
pub const API_VERSION_RESPONSE_HEADER: &str = "x-api-version";

/// Header name for deprecation warnings
pub const API_DEPRECATION_HEADER: &str = "x-api-deprecation";

/// Header name for sunset date
pub const API_SUNSET_HEADER: &str = "x-api-sunset";

/// Extracted API version from request
#[derive(Debug, Clone)]
pub struct ApiVersion {
    /// The requested API version
    pub version: String,
    /// Whether the version is deprecated
    pub is_deprecated: bool,
    /// Sunset date if deprecated (RFC3339 format)
    pub sunset_date: Option<String>,
}

impl Default for ApiVersion {
    fn default() -> Self {
        Self {
            version: CURRENT_API_VERSION.to_string(),
            is_deprecated: false,
            sunset_date: None,
        }
    }
}

impl ApiVersion {
    /// Create a new API version
    pub fn new(version: &str) -> Self {
        Self {
            version: version.to_string(),
            is_deprecated: false,
            sunset_date: None,
        }
    }

    /// Mark version as deprecated with optional sunset date
    pub fn deprecated(mut self, sunset_date: Option<String>) -> Self {
        self.is_deprecated = true;
        self.sunset_date = sunset_date;
        self
    }
}

/// Maximum length for version header to prevent DoS
const MAX_VERSION_LENGTH: usize = 16;

/// API versioning middleware
///
/// This middleware:
/// 1. Extracts the requested API version from the X-API-Version header
/// 2. Validates that the version is supported
/// 3. Adds the API version to the response headers
/// 4. Adds deprecation warnings if applicable
///
pub async fn api_versioning_middleware(request: Request<Body>, next: Next) -> Response {
    // Extract requested version from header, default to current
    let requested_version = request
        .headers()
        .get(API_VERSION_REQUEST_HEADER)
        .and_then(|v| v.to_str().ok())
        .unwrap_or(CURRENT_API_VERSION);

    // Validate version header - prevent injection and DoS
    // Only allow alphanumeric and dots (for semantic versioning like "1.0.0")
    let requested_version = if requested_version.len() > MAX_VERSION_LENGTH
        || !requested_version
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '.')
    {
        debug!(
            version = %requested_version,
            "Invalid version header format, using default"
        );
        CURRENT_API_VERSION.to_string()
    } else {
        requested_version.to_string()
    };

    // Validate version is supported
    if !SUPPORTED_VERSIONS.contains(&requested_version.as_str()) {
        return (
            StatusCode::BAD_REQUEST,
            [(
                HeaderName::from_static(API_VERSION_RESPONSE_HEADER),
                HeaderValue::from_static(CURRENT_API_VERSION),
            )],
            format!(
                "Unsupported API version: {}. Supported versions: {}",
                requested_version,
                SUPPORTED_VERSIONS.join(", ")
            ),
        )
            .into_response();
    }

    debug!(version = %requested_version, "API version requested");

    // Run the next middleware/handler
    let mut response = next.run(request).await;

    // Add version header to response
    let headers = response.headers_mut();
    headers.insert(
        HeaderName::from_static(API_VERSION_RESPONSE_HEADER),
        HeaderValue::from_str(&requested_version)
            .unwrap_or(HeaderValue::from_static(CURRENT_API_VERSION)),
    );

    // Add deprecation headers if version is deprecated (for future use)
    // Currently all versions are supported, this is for future deprecation
    if let Some(deprecation_info) = get_deprecation_info(&requested_version) {
        headers.insert(
            HeaderName::from_static(API_DEPRECATION_HEADER),
            HeaderValue::from_static("true"),
        );
        if let Some(sunset) = deprecation_info {
            if let Ok(sunset_value) = HeaderValue::from_str(&sunset) {
                headers.insert(HeaderName::from_static(API_SUNSET_HEADER), sunset_value);
            }
        }
    }

    response
}

/// Get deprecation info for a version
/// Returns Some(Some(sunset_date)) if deprecated with date,
/// Some(None) if deprecated without date, None if not deprecated
fn get_deprecation_info(_version: &str) -> Option<Option<String>> {
    // Currently no versions are deprecated
    // This function is prepared for future deprecation
    // Example when deprecating version "0":
    //   if version == "0" { return Some(Some("2025-06-01".to_string())); }
    None
}

/// Extract API version from request extension
pub fn get_api_version(version_header: Option<&str>) -> ApiVersion {
    let version = version_header.unwrap_or(CURRENT_API_VERSION);

    if SUPPORTED_VERSIONS.contains(&version) {
        ApiVersion::new(version)
    } else {
        ApiVersion::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::Request;
    use axum::{routing::get, Router};
    use tower::ServiceExt;

    async fn test_handler() -> &'static str {
        "OK"
    }

    #[tokio::test]
    async fn test_default_version() {
        let app = Router::new()
            .route("/", get(test_handler))
            .layer(axum::middleware::from_fn(api_versioning_middleware));

        let response = app
            .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers().get(API_VERSION_RESPONSE_HEADER).unwrap(),
            CURRENT_API_VERSION
        );
    }

    #[tokio::test]
    async fn test_explicit_version() {
        let app = Router::new()
            .route("/", get(test_handler))
            .layer(axum::middleware::from_fn(api_versioning_middleware));

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/")
                    .header(API_VERSION_REQUEST_HEADER, "1")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers().get(API_VERSION_RESPONSE_HEADER).unwrap(),
            "1"
        );
    }

    #[tokio::test]
    async fn test_unsupported_version() {
        let app = Router::new()
            .route("/", get(test_handler))
            .layer(axum::middleware::from_fn(api_versioning_middleware));

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/")
                    .header(API_VERSION_REQUEST_HEADER, "99")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[test]
    fn test_api_version_default() {
        let version = ApiVersion::default();
        assert_eq!(version.version, CURRENT_API_VERSION);
        assert!(!version.is_deprecated);
        assert!(version.sunset_date.is_none());
    }

    #[test]
    fn test_api_version_deprecated() {
        let version = ApiVersion::new("1").deprecated(Some("2025-12-31".to_string()));
        assert_eq!(version.version, "1");
        assert!(version.is_deprecated);
        assert_eq!(version.sunset_date, Some("2025-12-31".to_string()));
    }

    #[test]
    fn test_get_api_version() {
        // Default version
        let v = get_api_version(None);
        assert_eq!(v.version, CURRENT_API_VERSION);

        // Valid version
        let v = get_api_version(Some("1"));
        assert_eq!(v.version, "1");

        // Invalid version falls back to default
        let v = get_api_version(Some("invalid"));
        assert_eq!(v.version, CURRENT_API_VERSION);
    }

    #[test]
    fn test_supported_versions() {
        assert!(SUPPORTED_VERSIONS.contains(&"1"));
        assert!(!SUPPORTED_VERSIONS.contains(&"0"));
        assert!(!SUPPORTED_VERSIONS.contains(&"2"));
    }

    #[test]
    fn test_deprecation_info() {
        // Current versions are not deprecated
        assert!(get_deprecation_info("1").is_none());
    }
}
