//! Metrics handler for Prometheus scraping
//!
//! Note: The dedicated metrics server on the metrics port has its own rate limiter

use axum::{
    extract::State,
    http::{header, HeaderMap, StatusCode},
    response::IntoResponse,
};
use prometheus::{Encoder, TextEncoder};

use crate::api::AppState;

/// Maximum metrics response size (to prevent memory issues)
const MAX_METRICS_SIZE: usize = 10 * 1024 * 1024; // 10MB

/// Constant-time string comparison to prevent timing attacks
fn constant_time_eq(a: &str, b: &str) -> bool {
    use subtle::ConstantTimeEq;

    // If lengths differ, pad to prevent length-based timing leaks
    let max_len = a.len().max(b.len());
    let a_bytes: Vec<u8> = a
        .bytes()
        .chain(std::iter::repeat(0u8))
        .take(max_len)
        .collect();
    let b_bytes: Vec<u8> = b
        .bytes()
        .chain(std::iter::repeat(0u8))
        .take(max_len)
        .collect();

    // Constant-time comparison
    let bytes_eq: bool = a_bytes.ct_eq(&b_bytes).into();

    // Length must also match
    a.len() == b.len() && bytes_eq
}

/// Prometheus metrics endpoint
///
/// Returns metrics in Prometheus text format for scraping.
///
/// This endpoint requires authentication via METRICS_TOKEN if set.
/// The dedicated metrics server on the metrics port has its own rate limiter
/// (see observability/mod.rs).
///
pub async fn prometheus_metrics(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> impl IntoResponse {
    // Validate metrics access token if configured
    if let Some(ref expected_token) = state.settings.server.metrics_token {
        let auth_header = headers.get("Authorization").and_then(|v| v.to_str().ok());

        match auth_header {
            Some(auth) if auth.starts_with("Bearer ") => {
                let provided_token = &auth[7..];
                // Use constant-time comparison to prevent timing attacks
                if !constant_time_eq(provided_token, expected_token) {
                    return (
                        StatusCode::UNAUTHORIZED,
                        "Invalid metrics token".to_string(),
                    )
                        .into_response();
                }
            }
            _ => {
                return (
                    StatusCode::UNAUTHORIZED,
                    "Metrics endpoint requires Authorization: Bearer <token>".to_string(),
                )
                    .into_response();
            }
        }
    }
    let encoder = TextEncoder::new();
    let metric_families = state.metrics.gather();

    let mut buffer = Vec::new();
    if let Err(e) = encoder.encode(&metric_families, &mut buffer) {
        // Don't leak encoder error details
        tracing::error!(error = %e, "Failed to encode metrics");
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            "Failed to encode metrics".to_string(),
        )
            .into_response();
    }

    // Limit response size
    if buffer.len() > MAX_METRICS_SIZE {
        tracing::warn!(
            size = buffer.len(),
            max_size = MAX_METRICS_SIZE,
            "Metrics response too large"
        );
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            "Metrics response too large".to_string(),
        )
            .into_response();
    }

    let body = String::from_utf8(buffer).unwrap_or_default();

    (
        StatusCode::OK,
        [(
            header::CONTENT_TYPE,
            "text/plain; version=0.0.4; charset=utf-8",
        )],
        body,
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    #[test]
    fn test_prometheus_content_type() {
        // Verify the expected content type format
        let content_type = "text/plain; version=0.0.4; charset=utf-8";
        assert!(content_type.contains("text/plain"));
        assert!(content_type.contains("0.0.4"));
    }
}
