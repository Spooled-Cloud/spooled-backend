//! Distributed Tracing Support
//!
//! This module provides distributed tracing configuration using
//! the tracing ecosystem. It supports:
//! - Structured logging
//! - Span propagation
//! - Context correlation
//! - Request tracing
//! - OpenTelemetry/Jaeger integration (with `otel` feature)

use std::env;

use tracing::Level;
use tracing_subscriber::{
    fmt::{self, format::FmtSpan},
    layer::SubscriberExt,
    util::SubscriberInitExt,
    EnvFilter, Layer, Registry,
};

/// Tracing configuration
#[derive(Debug, Clone)]
pub struct TracingConfig {
    /// Service name for tracing
    pub service_name: String,
    /// Service version
    pub service_version: String,
    /// Environment (development, staging, production)
    pub environment: String,
    /// Log level
    pub log_level: Level,
    /// Whether to enable JSON logging
    pub json_logs: bool,
    /// Whether to log span events (enter/exit)
    pub log_span_events: bool,
    /// OpenTelemetry OTLP endpoint (e.g., http://jaeger:4317)
    pub otlp_endpoint: Option<String>,
}

impl Default for TracingConfig {
    fn default() -> Self {
        Self {
            service_name: "spooled-backend".to_string(),
            service_version: env!("CARGO_PKG_VERSION").to_string(),
            environment: env::var("ENVIRONMENT").unwrap_or_else(|_| "development".to_string()),
            log_level: Level::INFO,
            json_logs: false,
            log_span_events: false,
            otlp_endpoint: None,
        }
    }
}

impl TracingConfig {
    /// Create config from environment variables
    pub fn from_env() -> Self {
        let log_level = env::var("RUST_LOG")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(Level::INFO);

        let json_logs = env::var("JSON_LOGS")
            .map(|v| v == "true" || v == "1")
            .unwrap_or(false);

        let environment = env::var("ENVIRONMENT").unwrap_or_else(|_| "development".to_string());

        // Enable JSON logs and span events in production
        let log_span_events = environment == "production";

        // OpenTelemetry endpoint from environment
        let otlp_endpoint = env::var("OTEL_EXPORTER_OTLP_ENDPOINT").ok();

        Self {
            service_name: env::var("OTEL_SERVICE_NAME")
                .or_else(|_| env::var("SERVICE_NAME"))
                .unwrap_or_else(|_| "spooled-backend".to_string()),
            service_version: env!("CARGO_PKG_VERSION").to_string(),
            environment,
            log_level,
            json_logs,
            log_span_events,
            otlp_endpoint,
        }
    }

    /// Create production config
    pub fn production() -> Self {
        Self {
            service_name: "spooled-backend".to_string(),
            service_version: env!("CARGO_PKG_VERSION").to_string(),
            environment: "production".to_string(),
            log_level: Level::INFO,
            json_logs: true,
            log_span_events: true,
            otlp_endpoint: env::var("OTEL_EXPORTER_OTLP_ENDPOINT").ok(),
        }
    }

    /// Check if OpenTelemetry is configured
    pub fn has_otel(&self) -> bool {
        self.otlp_endpoint.is_some()
    }
}

/// Initialize tracing with the given configuration
///
/// If the `otel` feature is enabled and OTEL_EXPORTER_OTLP_ENDPOINT is set,
/// traces will be exported to the configured OpenTelemetry collector (e.g., Jaeger).
pub fn init_tracing(config: &TracingConfig) {
    let env_filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new(config.log_level.to_string()));

    let span_events = if config.log_span_events {
        FmtSpan::NEW | FmtSpan::CLOSE
    } else {
        FmtSpan::NONE
    };

    // Build the logging layer
    let fmt_layer: Box<dyn Layer<Registry> + Send + Sync> = if config.json_logs {
        Box::new(
            fmt::layer()
                .json()
                .with_span_events(span_events)
                .with_current_span(true)
                .with_target(true)
                .with_file(false)
                .with_line_number(false)
                .with_filter(env_filter),
        )
    } else {
        Box::new(
            fmt::layer()
                .pretty()
                .with_span_events(span_events)
                .with_target(true)
                .with_file(true)
                .with_line_number(true)
                .with_filter(env_filter),
        )
    };

    // Initialize with or without OpenTelemetry
    #[cfg(feature = "otel")]
    {
        if let Some(ref endpoint) = config.otlp_endpoint {
            init_with_otel(config, fmt_layer, endpoint);
            return;
        }
    }

    // Standard initialization without OpenTelemetry
    Registry::default().with(fmt_layer).init();

    tracing::info!(
        service = %config.service_name,
        version = %config.service_version,
        environment = %config.environment,
        "Tracing initialized"
    );
}

/// Initialize tracing with OpenTelemetry export
#[cfg(feature = "otel")]
fn init_with_otel(
    config: &TracingConfig,
    fmt_layer: Box<dyn Layer<Registry> + Send + Sync>,
    endpoint: &str,
) {
    use opentelemetry::trace::TracerProvider;
    use opentelemetry::KeyValue;
    use opentelemetry_otlp::{SpanExporter, WithExportConfig};
    use opentelemetry_sdk::{trace::SdkTracerProvider, Resource};
    use tracing_opentelemetry::OpenTelemetryLayer;

    // Build the OTLP exporter
    let exporter = SpanExporter::builder()
        .with_tonic()
        .with_endpoint(endpoint)
        .build()
        .expect("Failed to create OTLP exporter");

    // Build the resource with service metadata using builder pattern
    let resource = Resource::builder()
        .with_service_name(config.service_name.clone())
        .with_attributes([
            KeyValue::new("service.version", config.service_version.clone()),
            KeyValue::new("deployment.environment", config.environment.clone()),
        ])
        .build();

    // Build the tracer provider using the new API
    let provider = SdkTracerProvider::builder()
        .with_batch_exporter(exporter)
        .with_resource(resource)
        .build();

    let tracer = provider.tracer(config.service_name.clone());

    // Set the global tracer provider
    opentelemetry::global::set_tracer_provider(provider);

    // Create the OpenTelemetry layer
    let otel_layer = OpenTelemetryLayer::new(tracer);

    // Initialize with both layers
    Registry::default().with(fmt_layer).with(otel_layer).init();

    tracing::info!(
        service = %config.service_name,
        version = %config.service_version,
        environment = %config.environment,
        otlp_endpoint = %endpoint,
        "Tracing initialized with OpenTelemetry"
    );
}

/// Shutdown OpenTelemetry tracer provider gracefully
///
/// Note: In OpenTelemetry 0.31+, shutdown is handled by storing the provider
/// reference and calling `provider.shutdown()`. For global tracer, this is
/// a no-op as the provider will be shut down when dropped.
#[cfg(feature = "otel")]
pub fn shutdown_tracer() {
    // OpenTelemetry 0.31+ removed global::shutdown_tracer_provider()
    // Shutdown is now handled by the provider owner calling provider.shutdown()
    // The global provider will flush on drop
    tracing::info!("OpenTelemetry tracer shutdown requested");
}

/// Span fields for request tracing
pub mod span_fields {
    /// HTTP request method
    pub const HTTP_METHOD: &str = "http.method";
    /// HTTP request path
    pub const HTTP_PATH: &str = "http.path";
    /// HTTP response status code
    pub const HTTP_STATUS_CODE: &str = "http.status_code";
    /// Request ID
    pub const REQUEST_ID: &str = "request_id";
    /// Organization ID
    pub const ORG_ID: &str = "org_id";
    /// Job ID
    pub const JOB_ID: &str = "job_id";
    /// Queue name
    pub const QUEUE_NAME: &str = "queue_name";
    /// Worker ID
    pub const WORKER_ID: &str = "worker_id";
    /// Duration in milliseconds
    pub const DURATION_MS: &str = "duration_ms";
    /// Error message
    pub const ERROR_MESSAGE: &str = "error.message";
    /// Error type
    pub const ERROR_TYPE: &str = "error.type";
}

/// Create a span for an HTTP request
#[macro_export]
macro_rules! request_span {
    ($method:expr, $path:expr, $request_id:expr) => {
        tracing::info_span!(
            "http_request",
            http.method = %$method,
            http.path = %$path,
            request_id = %$request_id,
        )
    };
}

/// Create a span for a job operation
#[macro_export]
macro_rules! job_span {
    ($operation:expr, $job_id:expr, $queue:expr) => {
        tracing::info_span!(
            $operation,
            job_id = %$job_id,
            queue_name = %$queue,
        )
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_tracing_config_default() {
        let config = TracingConfig::default();
        assert_eq!(config.service_name, "spooled-backend");
        assert!(!config.json_logs);
        assert!(!config.log_span_events);
        assert!(config.otlp_endpoint.is_none());
        assert!(!config.has_otel());
    }

    #[test]
    fn test_tracing_config_production() {
        let config = TracingConfig::production();
        assert_eq!(config.environment, "production");
        assert!(config.json_logs);
        assert!(config.log_span_events);
    }

    #[test]
    fn test_span_fields() {
        // Ensure span field constants are valid
        assert_eq!(span_fields::HTTP_METHOD, "http.method");
        assert_eq!(span_fields::REQUEST_ID, "request_id");
        assert_eq!(span_fields::JOB_ID, "job_id");
    }

    #[test]
    fn test_has_otel() {
        let mut config = TracingConfig::default();
        assert!(!config.has_otel());

        config.otlp_endpoint = Some("http://jaeger:4317".to_string());
        assert!(config.has_otel());
    }
}
