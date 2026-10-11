//! gRPC Server Bootstrap
//!
//! Sets up the tonic gRPC server with all services, health checks, and reflection.
//! Supports optional TLS for Cloudflare Tunnel compatibility.
//!
//! Performance optimizations:
//! - HTTP/2 keepalive for connection reuse
//! - TCP keepalive for long-lived connections
//! - TCP_NODELAY to disable Nagle's algorithm
//! - Configurable concurrency limits
//!
//! Health follows the database while serving; on shutdown it turns
//! NOT_SERVING before a bounded drain of the in-flight calls.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::watch;
use tonic::server::NamedService;
use tonic::transport::server::{Router, TcpIncoming};
use tonic::transport::{Identity, Server, ServerTlsConfig};
use tonic_health::server::{health_reporter, HealthReporter};
use tonic_health::ServingStatus;
use tracing::{info, warn};

use crate::api::handlers::health::RuntimeHealth;
use crate::grpc::proto::queue_service_server::QueueServiceServer;
use crate::grpc::proto::worker_service_server::WorkerServiceServer;
use crate::grpc::proto::FILE_DESCRIPTOR_SET;
use crate::grpc::services::{QueueServiceImpl, WorkerServiceImpl};
use crate::observability::Metrics;

type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// Maximum message size for gRPC requests (4MB)
use crate::grpc::constants::MAX_GRPC_MESSAGE_SIZE as MAX_MESSAGE_SIZE;

/// HTTP/2 keepalive interval (seconds) - ping to keep connection alive
const HTTP2_KEEPALIVE_INTERVAL_SECS: u64 = 10;

/// HTTP/2 keepalive timeout (seconds) - timeout for keepalive ping
const HTTP2_KEEPALIVE_TIMEOUT_SECS: u64 = 20;

/// TCP keepalive interval (seconds)
const TCP_KEEPALIVE_SECS: u64 = 60;

/// Initial connection window size (bytes) - larger for better throughput
const INITIAL_CONNECTION_WINDOW_SIZE: u32 = 1024 * 1024; // 1MB

/// Initial stream window size (bytes)
const INITIAL_STREAM_WINDOW_SIZE: u32 = 512 * 1024; // 512KB

/// Maximum concurrent streams per connection
const MAX_CONCURRENT_STREAMS: u32 = 200;

/// How long in-flight calls may run after shutdown starts. Long-lived streams
/// (StreamJobs, ProcessJobs, health Watch) never end on their own, so the
/// drain is bounded and whatever is still open afterwards ends with the
/// process.
pub const GRPC_DRAIN_TIMEOUT: Duration = Duration::from_secs(10);

/// How often the health service re-checks the database.
const HEALTH_PROBE_INTERVAL: Duration = Duration::from_secs(10);

/// Upper bound for one database probe of the health service.
const HEALTH_PROBE_TIMEOUT: Duration = Duration::from_secs(3);

/// gRPC TLS configuration loaded from environment
pub struct GrpcTlsConfig {
    pub enabled: bool,
    pub cert_path: Option<String>,
    pub key_path: Option<String>,
}

impl GrpcTlsConfig {
    /// Load gRPC TLS config from environment variables
    ///
    /// Environment variables:
    /// - `GRPC_TLS_ENABLED`: Enable TLS (default: false)
    /// - `GRPC_TLS_CERT_PATH`: Path to TLS certificate file (PEM format)
    /// - `GRPC_TLS_KEY_PATH`: Path to TLS private key file (PEM format)
    pub fn from_env() -> Self {
        let enabled = std::env::var("GRPC_TLS_ENABLED")
            .map(|v| v == "true" || v == "1" || v == "yes")
            .unwrap_or(false);

        let cert_path = std::env::var("GRPC_TLS_CERT_PATH").ok();
        let key_path = std::env::var("GRPC_TLS_KEY_PATH").ok();

        Self {
            enabled,
            cert_path,
            key_path,
        }
    }

    /// Validate the TLS configuration
    pub fn validate(&self) -> Result<(), String> {
        if self.enabled {
            if self.cert_path.is_none() {
                return Err("GRPC_TLS_CERT_PATH is required when GRPC_TLS_ENABLED=true".to_string());
            }
            if self.key_path.is_none() {
                return Err("GRPC_TLS_KEY_PATH is required when GRPC_TLS_ENABLED=true".to_string());
            }
        }
        Ok(())
    }
}

/// What the gRPC services are built from.
pub struct GrpcServerDeps {
    pub db: Arc<crate::db::Database>,
    pub metrics: Arc<Metrics>,
    pub cache: Option<crate::cache::RedisCache>,
    pub outgoing_webhooks: Arc<crate::outgoing_webhooks::service::OutgoingWebhookService>,
    pub queue_defaults: crate::config::QueueSettings,
    pub rate_limit_fail_closed: bool,
    pub worker_settings: crate::config::WorkerSettings,
}

/// A configured gRPC server whose listener is already bound.
///
/// Features:
/// - QueueService: Job queue operations
/// - WorkerService: Worker lifecycle management
/// - Health service: gRPC health checks
/// - Reflection service: gRPC reflection for debugging
/// - Optional TLS: For Cloudflare Tunnel HTTPS origin
pub struct GrpcServer {
    router: Router,
    incoming: TcpIncoming,
    local_addr: SocketAddr,
    health: HealthReporter,
    db: Arc<crate::db::Database>,
    health_probe_interval: Duration,
}

impl GrpcServer {
    /// Build the services, load TLS and bind the listener.
    ///
    /// Every startup failure (TLS misconfiguration, unreadable or invalid
    /// PEM, busy port) is returned from here, before anything is spawned, so
    /// the caller can stop the process instead of running without gRPC.
    pub async fn bind(addr: SocketAddr, deps: GrpcServerDeps) -> Result<Self, BoxError> {
        let pool = deps.db.pool_arc();

        // Load TLS configuration
        let tls_config = GrpcTlsConfig::from_env();
        tls_config.validate().map_err(BoxError::from)?;

        // Create service implementations
        let queue_service = QueueServiceImpl::new(
            pool.clone(),
            deps.metrics.clone(),
            deps.cache.clone(),
            deps.outgoing_webhooks.clone(),
            deps.queue_defaults,
            deps.rate_limit_fail_closed,
            deps.worker_settings.clone(),
        );
        let worker_service = WorkerServiceImpl::new(
            pool.clone(),
            deps.metrics.clone(),
            deps.cache.clone(),
            deps.outgoing_webhooks,
            deps.rate_limit_fail_closed,
            deps.worker_settings,
        );

        // Setup health service
        let (health_reporter, health_service) = health_reporter();

        // Mark services as serving
        health_reporter
            .set_serving::<QueueServiceServer<QueueServiceImpl>>()
            .await;
        health_reporter
            .set_serving::<WorkerServiceServer<WorkerServiceImpl>>()
            .await;

        // Setup reflection service with health service descriptor
        let reflection_service = tonic_reflection::server::Builder::configure()
            .register_encoded_file_descriptor_set(FILE_DESCRIPTOR_SET)
            .register_encoded_file_descriptor_set(tonic_health::pb::FILE_DESCRIPTOR_SET)
            .build_v1()?;

        // Build server with performance optimizations
        let mut server = Server::builder()
            // HTTP/2 keepalive - keeps connections alive and reduces latency
            .http2_keepalive_interval(Some(Duration::from_secs(HTTP2_KEEPALIVE_INTERVAL_SECS)))
            .http2_keepalive_timeout(Some(Duration::from_secs(HTTP2_KEEPALIVE_TIMEOUT_SECS)))
            // Larger window sizes for better throughput
            .initial_connection_window_size(Some(INITIAL_CONNECTION_WINDOW_SIZE))
            .initial_stream_window_size(Some(INITIAL_STREAM_WINDOW_SIZE))
            // Concurrency limit
            .max_concurrent_streams(Some(MAX_CONCURRENT_STREAMS));

        info!(
            http2_keepalive_interval_secs = HTTP2_KEEPALIVE_INTERVAL_SECS,
            tcp_nodelay = true,
            max_concurrent_streams = MAX_CONCURRENT_STREAMS,
            "gRPC server configured with performance optimizations"
        );

        // Configure TLS if enabled
        if tls_config.enabled {
            let cert_path = tls_config.cert_path.as_ref().unwrap();
            let key_path = tls_config.key_path.as_ref().unwrap();

            info!(
                cert_path = %cert_path,
                key_path = %key_path,
                "Loading gRPC TLS certificate"
            );

            let cert = tokio::fs::read_to_string(cert_path)
                .await
                .map_err(|e| format!("Failed to read TLS cert from {}: {}", cert_path, e))?;
            let key = tokio::fs::read_to_string(key_path)
                .await
                .map_err(|e| format!("Failed to read TLS key from {}: {}", key_path, e))?;

            let identity = Identity::from_pem(cert, key);
            let tls = ServerTlsConfig::new().identity(identity);

            server = server.tls_config(tls)?;

            info!(%addr, "Starting gRPC server (tonic with TLS)");
        } else {
            warn!(%addr, "Starting gRPC server (tonic without TLS) - enable TLS for Cloudflare Tunnel");
        }

        let router = server
            .accept_http1(false)
            .add_service(health_service)
            .add_service(reflection_service)
            .add_service(
                QueueServiceServer::new(queue_service)
                    .max_decoding_message_size(MAX_MESSAGE_SIZE)
                    .max_encoding_message_size(MAX_MESSAGE_SIZE),
            )
            .add_service(
                WorkerServiceServer::new(worker_service)
                    .max_decoding_message_size(MAX_MESSAGE_SIZE)
                    .max_encoding_message_size(MAX_MESSAGE_SIZE),
            );

        // Bind here rather than inside `serve` so a busy port is a startup
        // error. The socket options the builder would apply on its own bind
        // (TCP keepalive, TCP_NODELAY) are set on the listener instead.
        info!(%addr, "Binding gRPC server to address");
        let incoming = TcpIncoming::bind(addr)
            .map_err(|e| format!("Failed to bind gRPC server to {}: {}", addr, e))?
            .with_nodelay(Some(true))
            .with_keepalive(Some(Duration::from_secs(TCP_KEEPALIVE_SECS)));
        let local_addr = incoming.local_addr()?;

        Ok(Self {
            router,
            incoming,
            local_addr,
            health: health_reporter,
            db: deps.db,
            health_probe_interval: HEALTH_PROBE_INTERVAL,
        })
    }

    /// The address the listener is bound to (resolves port 0).
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// Override how often the health service re-checks the database.
    pub fn with_health_probe_interval(mut self, interval: Duration) -> Self {
        self.health_probe_interval = interval;
        self
    }

    /// Serve until `shutdown` turns true, then drain.
    ///
    /// Shutdown first turns every health status NOT_SERVING, so load
    /// balancers and clients watching health move away, then stops accepting
    /// connections and lets in-flight calls finish for up to `drain_timeout`.
    /// Calls and streams still open after that are left to the process exit.
    /// `runtime`, when given, tracks whether the server is accepting calls.
    pub async fn serve(
        self,
        shutdown: watch::Receiver<bool>,
        drain_timeout: Duration,
        runtime: Option<Arc<RuntimeHealth>>,
    ) -> Result<(), BoxError> {
        let Self {
            router,
            incoming,
            local_addr,
            health,
            db,
            health_probe_interval,
        } = self;

        let probe = tokio::spawn(probe_database(db, health.clone(), health_probe_interval));
        let probe_abort = probe.abort_handle();

        let mut stop = shutdown.clone();
        let signal_runtime = runtime.clone();
        let signal = async move {
            // A dropped sender can never signal again; treat it as shutdown.
            let _ = stop.wait_for(|stop| *stop).await;
            // Stop the probe first so it cannot turn a status back to SERVING.
            probe.abort();
            let _ = probe.await;
            set_health(&health, ServingStatus::NotServing).await;
            if let Some(runtime) = &signal_runtime {
                runtime.set_grpc_serving(false);
            }
            info!(
                ?drain_timeout,
                "gRPC health is NOT_SERVING, draining in-flight calls"
            );
        };

        if let Some(runtime) = &runtime {
            runtime.set_grpc_serving(true);
        }
        info!(addr = %local_addr, "gRPC server serving");

        let mut drain_started = shutdown;
        let drain_deadline = async move {
            let _ = drain_started.wait_for(|stop| *stop).await;
            tokio::time::sleep(drain_timeout).await;
        };
        let result = tokio::select! {
            result = router.serve_with_incoming_shutdown(incoming, signal) => {
                result.map_err(BoxError::from)
            }
            _ = drain_deadline => {
                warn!(
                    ?drain_timeout,
                    "gRPC drain timed out, leaving the calls still open to the process exit"
                );
                Ok(())
            }
        };

        probe_abort.abort();
        if let Some(runtime) = &runtime {
            runtime.set_grpc_serving(false);
        }
        match &result {
            Ok(()) => info!("gRPC server stopped"),
            Err(e) => warn!(error = %e, "gRPC server error"),
        }
        result
    }
}

/// Health names this server reports: the whole server ("") and each service.
const HEALTH_NAMES: [&str; 3] = [
    "",
    <QueueServiceServer<QueueServiceImpl> as NamedService>::NAME,
    <WorkerServiceServer<WorkerServiceImpl> as NamedService>::NAME,
];

async fn set_health(health: &HealthReporter, status: ServingStatus) {
    for name in HEALTH_NAMES {
        health.set_service_status(name, status).await;
    }
}

/// Keep the health statuses in step with the database. Neither service can
/// do anything useful without it, so clients and load balancers that check
/// gRPC health see NOT_SERVING while it is unreachable.
async fn probe_database(db: Arc<crate::db::Database>, health: HealthReporter, every: Duration) {
    let mut ticker = tokio::time::interval(every);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // `bind` marked everything SERVING.
    let mut serving = true;
    loop {
        ticker.tick().await;
        let reachable = matches!(
            tokio::time::timeout(HEALTH_PROBE_TIMEOUT, db.health_check()).await,
            Ok(Ok(true))
        );
        if reachable == serving {
            continue;
        }
        serving = reachable;
        if reachable {
            info!("Database reachable again, gRPC health back to SERVING");
            set_health(&health, ServingStatus::Serving).await;
        } else {
            warn!("Database unreachable, gRPC health set to NOT_SERVING");
            set_health(&health, ServingStatus::NotServing).await;
        }
    }
}

/// Bind the gRPC server on `addr` and serve it until `shutdown` turns true.
pub async fn start_grpc_server(
    addr: SocketAddr,
    deps: GrpcServerDeps,
    shutdown: watch::Receiver<bool>,
) -> Result<(), BoxError> {
    GrpcServer::bind(addr, deps)
        .await?
        .serve(shutdown, GRPC_DRAIN_TIMEOUT, None)
        .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_max_message_size() {
        // Ensure max message size is reasonable
        // Using runtime checks to satisfy clippy
        let min_size = 1024 * 1024;
        let max_size = 16 * 1024 * 1024;
        assert!(
            MAX_MESSAGE_SIZE >= min_size,
            "Message size should be at least 1MB"
        );
        assert!(
            MAX_MESSAGE_SIZE <= max_size,
            "Message size should be at most 16MB"
        );
    }

    // Both TLS tests mutate the same process-global env vars; tests run in
    // parallel, so they must serialize against each other or they flake.
    static TLS_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn test_tls_config_disabled_by_default() {
        // Recover a poisoned lock rather than cascading the panic: if the other
        // TLS test panicked mid-section, the env-var invariant is re-established
        // below, so the poison is not meaningful here.
        let _guard = TLS_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());

        // Clear env vars for test
        std::env::remove_var("GRPC_TLS_ENABLED");
        std::env::remove_var("GRPC_TLS_CERT_PATH");
        std::env::remove_var("GRPC_TLS_KEY_PATH");

        let config = GrpcTlsConfig::from_env();
        assert!(!config.enabled);
        assert!(config.validate().is_ok());
    }

    #[test]
    fn test_tls_config_requires_paths_when_enabled() {
        let _guard = TLS_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());

        std::env::set_var("GRPC_TLS_ENABLED", "true");
        std::env::remove_var("GRPC_TLS_CERT_PATH");
        std::env::remove_var("GRPC_TLS_KEY_PATH");

        let config = GrpcTlsConfig::from_env();
        assert!(config.enabled);
        assert!(config.validate().is_err());

        // Cleanup
        std::env::remove_var("GRPC_TLS_ENABLED");
    }
}
