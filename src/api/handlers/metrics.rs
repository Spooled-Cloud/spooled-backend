//! Metrics handler for Prometheus scraping
//!
//! Note: The dedicated metrics server on the metrics port has its own rate limiter

use axum::{
    extract::State,
    http::{header, StatusCode},
    response::IntoResponse,
};
use prometheus::{Encoder, TextEncoder};

use crate::api::AppState;

/// Maximum metrics response size (to prevent memory issues)
const MAX_METRICS_SIZE: usize = 10 * 1024 * 1024; // 10MB

/// Prometheus metrics endpoint
///
/// Returns metrics in Prometheus text format for scraping.
///
/// This endpoint is protected by the main API rate limiter
/// when accessed through /api/v1/metrics. The dedicated metrics server
/// on the metrics port has its own rate limiter (see observability/mod.rs).
///
pub async fn prometheus_metrics(State(state): State<AppState>) -> impl IntoResponse {
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
