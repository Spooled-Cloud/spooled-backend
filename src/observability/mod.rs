//! Observability module for Spooled Backend
//!
//! This module provides logging, metrics, and tracing functionality.

pub mod tracing_config;

use std::net::SocketAddr;
use std::sync::Arc;

use axum::{routing::get, Router};
use prometheus::{Encoder, Histogram, HistogramOpts, IntCounter, IntGauge, Registry, TextEncoder};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

// Tracing configuration re-exports for advanced usage
#[allow(unused_imports)]
pub use tracing_config::{init_tracing as init_tracing_with_config, TracingConfig};

/// Initialize tracing/logging with defaults
pub fn init_tracing() {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("spooled_backend=debug,tower_http=debug,sqlx=warn"));

    tracing_subscriber::registry()
        .with(filter)
        .with(tracing_subscriber::fmt::layer().json())
        .init();
}

/// Metrics collection
#[derive(Clone)]
pub struct Metrics {
    registry: Arc<Registry>,

    // Job metrics
    pub jobs_enqueued: IntCounter,
    pub jobs_completed: IntCounter,
    pub jobs_failed: IntCounter,
    pub jobs_retried: IntCounter,
    pub jobs_deadlettered: IntCounter,
    pub jobs_pending: IntGauge,
    pub jobs_processing: IntGauge,
    pub job_duration: Histogram,
    pub job_max_age_seconds: IntGauge,

    // API metrics
    pub api_requests_total: IntCounter,
    pub api_request_duration: Histogram,

    // Worker metrics
    pub workers_active: IntGauge,
    pub workers_healthy: IntGauge,

    // Infrastructure metrics
    pub database_connections: IntGauge,
    pub redis_operations: IntCounter,
}

impl Metrics {
    /// Create a new metrics instance
    pub fn new() -> Self {
        let registry = Registry::new();

        // Job metrics
        let jobs_enqueued = IntCounter::new("spooled_jobs_enqueued_total", "Total jobs enqueued")
            .expect("metric creation failed");
        let jobs_completed = IntCounter::new(
            "spooled_jobs_completed_total",
            "Total jobs completed successfully",
        )
        .expect("metric creation failed");
        let jobs_failed = IntCounter::new("spooled_jobs_failed_total", "Total jobs failed")
            .expect("metric creation failed");
        let jobs_retried = IntCounter::new("spooled_jobs_retried_total", "Total job retries")
            .expect("metric creation failed");
        let jobs_deadlettered =
            IntCounter::new("spooled_jobs_deadlettered_total", "Total jobs moved to DLQ")
                .expect("metric creation failed");
        let jobs_pending = IntGauge::new("spooled_jobs_pending", "Number of pending jobs")
            .expect("metric creation failed");
        let jobs_processing = IntGauge::new(
            "spooled_jobs_processing",
            "Number of jobs currently processing",
        )
        .expect("metric creation failed");

        let job_duration_opts = HistogramOpts::new(
            "spooled_job_duration_seconds",
            "Job processing duration in seconds",
        )
        .buckets(vec![
            0.01, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0, 60.0,
        ]);
        let job_duration = Histogram::with_opts(job_duration_opts).expect("metric creation failed");

        let job_max_age_seconds = IntGauge::new(
            "spooled_job_max_age_seconds",
            "Age of oldest pending job in seconds",
        )
        .expect("metric creation failed");

        // API metrics
        let api_requests_total =
            IntCounter::new("spooled_api_requests_total", "Total API requests")
                .expect("metric creation failed");

        let api_request_duration_opts = HistogramOpts::new(
            "spooled_api_request_duration_seconds",
            "API request duration in seconds",
        )
        .buckets(vec![
            0.001, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0,
        ]);
        let api_request_duration =
            Histogram::with_opts(api_request_duration_opts).expect("metric creation failed");

        // Worker metrics
        let workers_active = IntGauge::new("spooled_workers_active", "Number of active workers")
            .expect("metric creation failed");
        let workers_healthy = IntGauge::new("spooled_workers_healthy", "Number of healthy workers")
            .expect("metric creation failed");

        // Infrastructure metrics
        let database_connections = IntGauge::new(
            "spooled_database_connections_active",
            "Active database connections",
        )
        .expect("metric creation failed");
        let redis_operations =
            IntCounter::new("spooled_redis_operations_total", "Total Redis operations")
                .expect("metric creation failed");

        // Register all metrics
        registry.register(Box::new(jobs_enqueued.clone())).unwrap();
        registry.register(Box::new(jobs_completed.clone())).unwrap();
        registry.register(Box::new(jobs_failed.clone())).unwrap();
        registry.register(Box::new(jobs_retried.clone())).unwrap();
        registry
            .register(Box::new(jobs_deadlettered.clone()))
            .unwrap();
        registry.register(Box::new(jobs_pending.clone())).unwrap();
        registry
            .register(Box::new(jobs_processing.clone()))
            .unwrap();
        registry.register(Box::new(job_duration.clone())).unwrap();
        registry
            .register(Box::new(job_max_age_seconds.clone()))
            .unwrap();
        registry
            .register(Box::new(api_requests_total.clone()))
            .unwrap();
        registry
            .register(Box::new(api_request_duration.clone()))
            .unwrap();
        registry.register(Box::new(workers_active.clone())).unwrap();
        registry
            .register(Box::new(workers_healthy.clone()))
            .unwrap();
        registry
            .register(Box::new(database_connections.clone()))
            .unwrap();
        registry
            .register(Box::new(redis_operations.clone()))
            .unwrap();

        Self {
            registry: Arc::new(registry),
            jobs_enqueued,
            jobs_completed,
            jobs_failed,
            jobs_retried,
            jobs_deadlettered,
            jobs_pending,
            jobs_processing,
            job_duration,
            job_max_age_seconds,
            api_requests_total,
            api_request_duration,
            workers_active,
            workers_healthy,
            database_connections,
            redis_operations,
        }
    }

    /// Encode metrics to Prometheus format
    pub fn encode(&self) -> String {
        let encoder = TextEncoder::new();
        let metric_families = self.registry.gather();
        let mut buffer = Vec::new();
        encoder.encode(&metric_families, &mut buffer).unwrap();
        String::from_utf8(buffer).unwrap()
    }

    /// Get all metric families for Prometheus scraping
    pub fn gather(&self) -> Vec<prometheus::proto::MetricFamily> {
        self.registry.gather()
    }
}

impl Default for Metrics {
    fn default() -> Self {
        Self::new()
    }
}

/// Constant-time string comparison to prevent timing attacks
///
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

/// Simple in-memory rate limiter for metrics endpoint
/// Fixed race condition in window reset using compare_exchange
struct MetricsRateLimiter {
    requests: std::sync::atomic::AtomicU64,
    window_start: std::sync::atomic::AtomicU64,
}

impl MetricsRateLimiter {
    const MAX_REQUESTS_PER_MINUTE: u64 = 60; // 1 request per second average
    /// Grace period at window boundary to prevent burst bypass
    const WINDOW_GRACE_SECS: u64 = 5;

    fn new() -> Self {
        Self {
            requests: std::sync::atomic::AtomicU64::new(0),
            window_start: std::sync::atomic::AtomicU64::new(
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_secs(),
            ),
        }
    }

    /// Use atomic compare_exchange to prevent race condition
    /// where multiple threads could reset window and bypass rate limit
    fn check_rate_limit(&self) -> bool {
        use std::sync::atomic::Ordering;

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();

        loop {
            let window_start = self.window_start.load(Ordering::Acquire);

            // Check if window needs reset (60+ seconds elapsed)
            if now - window_start >= 60 + Self::WINDOW_GRACE_SECS {
                // Use compare_exchange to atomically reset window
                // Only one thread will succeed, others will retry
                match self.window_start.compare_exchange(
                    window_start,
                    now,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                ) {
                    Ok(_) => {
                        // Successfully reset window, this is the first request
                        self.requests.store(1, Ordering::Release);
                        return true;
                    }
                    Err(_) => {
                        // Another thread reset the window, retry with new window
                        continue;
                    }
                }
            }

            // Window is current, increment counter
            let requests = self.requests.fetch_add(1, Ordering::AcqRel);
            return requests < Self::MAX_REQUESTS_PER_MINUTE;
        }
    }
}

/// Start the metrics server
///
/// Metrics endpoint now requires Bearer token authentication
/// or can be accessed only from localhost/internal network
///
/// Token comparison now uses constant-time algorithm to prevent timing attacks
///
pub async fn start_metrics_server(
    addr: SocketAddr,
    metrics: Arc<Metrics>,
    metrics_token: Option<String>,
) {
    let metrics_clone = metrics.clone();
    let token = metrics_token.clone();
    let rate_limiter = Arc::new(MetricsRateLimiter::new());
    let rate_limiter_clone = rate_limiter.clone();

    let app = Router::new()
        .route(
            "/metrics",
            get(move |headers: axum::http::HeaderMap| {
                let rate_limiter = rate_limiter_clone.clone();
                let metrics = metrics_clone.clone();
                let token = token.clone();
                async move {
                    // Check rate limit before processing
                    if !rate_limiter.check_rate_limit() {
                        return (
                            axum::http::StatusCode::TOO_MANY_REQUESTS,
                            "Rate limit exceeded for metrics endpoint".to_string(),
                        );
                    }

                    // Validate metrics access token if configured
                    if let Some(ref expected_token) = token {
                        let auth_header =
                            headers.get("Authorization").and_then(|v| v.to_str().ok());

                        match auth_header {
                            Some(auth) if auth.starts_with("Bearer ") => {
                                let provided_token = &auth[7..];
                                // Use constant-time comparison to prevent timing attacks
                                if !constant_time_eq(provided_token, expected_token) {
                                    return (
                                        axum::http::StatusCode::UNAUTHORIZED,
                                        "Invalid metrics token".to_string(),
                                    );
                                }
                            }
                            _ => {
                                return (
                                    axum::http::StatusCode::UNAUTHORIZED,
                                    "Metrics endpoint requires Authorization: Bearer <token>"
                                        .to_string(),
                                );
                            }
                        }
                    }

                    (axum::http::StatusCode::OK, metrics.encode())
                }
            }),
        )
        .route("/health", get(|| async { "OK" }))
        .route("/ready", get(|| async { "READY" })); // Added readiness probe

    tracing::info!(%addr, token_required = metrics_token.is_some(), "Metrics server starting");

    let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
    axum::serve(listener, app).await.unwrap();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_metrics_creation() {
        let metrics = Metrics::new();
        metrics.jobs_enqueued.inc();
        metrics.jobs_pending.set(10);

        let output = metrics.encode();
        assert!(output.contains("spooled_jobs_enqueued_total"));
        assert!(output.contains("spooled_jobs_pending"));
    }

    #[test]
    fn test_histogram_observation() {
        let metrics = Metrics::new();
        metrics.job_duration.observe(0.5);
        metrics.job_duration.observe(1.5);

        let output = metrics.encode();
        assert!(output.contains("spooled_job_duration_seconds"));
    }

    #[test]
    fn test_all_counter_metrics() {
        let metrics = Metrics::new();

        // Test all counters
        metrics.jobs_enqueued.inc();
        metrics.jobs_completed.inc();
        metrics.jobs_failed.inc();
        metrics.jobs_retried.inc();
        metrics.jobs_deadlettered.inc();

        let output = metrics.encode();
        assert!(output.contains("spooled_jobs_enqueued_total"));
        assert!(output.contains("spooled_jobs_completed_total"));
        assert!(output.contains("spooled_jobs_failed_total"));
        assert!(output.contains("spooled_jobs_retried_total"));
        assert!(output.contains("spooled_jobs_deadlettered_total"));
    }

    #[test]
    fn test_all_gauge_metrics() {
        let metrics = Metrics::new();

        // Test all gauges
        metrics.jobs_pending.set(100);
        metrics.jobs_processing.set(50);
        metrics.job_max_age_seconds.set(3600);
        metrics.workers_active.set(10);
        metrics.workers_healthy.set(8);
        metrics.database_connections.set(20);

        let output = metrics.encode();
        // Check that all gauge metrics are present in output
        assert!(output.contains("spooled_jobs_pending"));
        assert!(output.contains("spooled_jobs_processing"));
        assert!(output.contains("spooled_job_max_age_seconds"));
        assert!(output.contains("spooled_workers_active"));
        assert!(output.contains("spooled_workers_healthy"));
        assert!(output.contains("spooled_database_connections"));
        // Check values (may have different formats)
        assert!(output.contains("100"));
        assert!(output.contains("50"));
        assert!(output.contains("3600"));
    }

    #[test]
    fn test_counter_inc_by() {
        let metrics = Metrics::new();

        metrics.jobs_enqueued.inc_by(5);
        metrics.jobs_retried.inc_by(3);

        let output = metrics.encode();
        assert!(output.contains("spooled_jobs_enqueued_total 5"));
        assert!(output.contains("spooled_jobs_retried_total 3"));
    }

    #[test]
    fn test_gauge_inc_dec() {
        let metrics = Metrics::new();

        metrics.jobs_pending.set(0);
        metrics.jobs_pending.inc();
        metrics.jobs_pending.inc();
        metrics.jobs_pending.inc();

        let output = metrics.encode();
        assert!(output.contains("spooled_jobs_pending 3"));

        metrics.jobs_pending.dec();
        let output = metrics.encode();
        assert!(output.contains("spooled_jobs_pending 2"));
    }

    #[test]
    fn test_histogram_buckets() {
        let metrics = Metrics::new();

        // Observe values in different buckets
        metrics.job_duration.observe(0.001); // Very fast
        metrics.job_duration.observe(0.1); // Fast
        metrics.job_duration.observe(1.0); // Normal
        metrics.job_duration.observe(10.0); // Slow
        metrics.job_duration.observe(100.0); // Very slow

        let output = metrics.encode();
        assert!(output.contains("spooled_job_duration_seconds_bucket"));
        assert!(output.contains("spooled_job_duration_seconds_count 5"));
    }

    #[test]
    fn test_api_metrics() {
        let metrics = Metrics::new();

        metrics.api_requests_total.inc();
        metrics.api_requests_total.inc();
        metrics.api_requests_total.inc();

        let output = metrics.encode();
        assert!(output.contains("spooled_api_requests_total"));
        assert!(output.contains("3")); // Should have 3 requests
    }

    #[test]
    fn test_api_request_duration() {
        let metrics = Metrics::new();

        metrics.api_request_duration.observe(0.050); // 50ms
        metrics.api_request_duration.observe(0.100); // 100ms
        metrics.api_request_duration.observe(0.500); // 500ms

        let output = metrics.encode();
        assert!(output.contains("spooled_api_request_duration_seconds"));
        assert!(output.contains("_count 3"));
    }

    #[test]
    fn test_metrics_default() {
        let metrics = Metrics::default();
        assert!(!metrics.encode().is_empty());
    }

    #[test]
    fn test_metrics_gather() {
        let metrics = Metrics::new();
        metrics.jobs_enqueued.inc();

        let families = metrics.gather();
        assert!(!families.is_empty());
    }

    #[test]
    fn test_redis_operations_metric() {
        let metrics = Metrics::new();

        metrics.redis_operations.inc();
        metrics.redis_operations.inc();

        let output = metrics.encode();
        assert!(output.contains("spooled_redis_operations_total"));
        assert!(output.contains("2")); // Should have 2 operations
    }
}
