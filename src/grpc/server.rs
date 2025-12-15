//! gRPC Server Bootstrap
//!
//! Sets up the tonic gRPC server with all services, health checks, and reflection.
//! Supports optional TLS for Cloudflare Tunnel compatibility.

use std::net::SocketAddr;
use std::sync::Arc;

use tonic::transport::{Identity, Server, ServerTlsConfig};
use tonic_health::server::health_reporter;
use tracing::{info, warn};

use crate::grpc::proto::queue_service_server::QueueServiceServer;
use crate::grpc::proto::worker_service_server::WorkerServiceServer;
use crate::grpc::proto::FILE_DESCRIPTOR_SET;
use crate::grpc::services::{QueueServiceImpl, WorkerServiceImpl};
use crate::observability::Metrics;

/// Maximum message size for gRPC requests (4MB)
const MAX_MESSAGE_SIZE: usize = 4 * 1024 * 1024;

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

/// Start the gRPC server on the specified address
///
/// Features:
/// - QueueService: Job queue operations
/// - WorkerService: Worker lifecycle management
/// - Health service: gRPC health checks
/// - Reflection service: gRPC reflection for debugging
/// - Optional TLS: For Cloudflare Tunnel HTTPS origin
pub async fn start_grpc_server(
    addr: SocketAddr,
    db: Arc<crate::db::Database>,
    metrics: Arc<Metrics>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let pool = db.pool_arc();

    // Load TLS configuration
    let tls_config = GrpcTlsConfig::from_env();
    tls_config
        .validate()
        .map_err(Box::<dyn std::error::Error + Send + Sync>::from)?;

    // Create service implementations
    let queue_service = QueueServiceImpl::new(pool.clone(), metrics.clone());
    let worker_service = WorkerServiceImpl::new(pool.clone(), metrics.clone());

    // Setup health service
    let (mut health_reporter, health_service) = health_reporter();

    // Mark services as serving
    health_reporter
        .set_serving::<QueueServiceServer<QueueServiceImpl>>()
        .await;
    health_reporter
        .set_serving::<WorkerServiceServer<WorkerServiceImpl>>()
        .await;

    // Setup reflection service
    let reflection_service = tonic_reflection::server::Builder::configure()
        .register_encoded_file_descriptor_set(FILE_DESCRIPTOR_SET)
        .build_v1()?;

    // Build server with optional TLS
    let mut server = Server::builder();

    // Configure TLS if enabled
    if tls_config.enabled {
        let cert_path = tls_config.cert_path.as_ref().unwrap();
        let key_path = tls_config.key_path.as_ref().unwrap();

        info!(
            cert_path = %cert_path,
            key_path = %key_path,
            "Loading gRPC TLS certificate"
        );

        let cert = tokio::fs::read_to_string(cert_path).await.map_err(|e| {
            format!("Failed to read TLS cert from {}: {}", cert_path, e)
        })?;
        let key = tokio::fs::read_to_string(key_path).await.map_err(|e| {
            format!("Failed to read TLS key from {}: {}", key_path, e)
        })?;

        let identity = Identity::from_pem(cert, key);
        let tls = ServerTlsConfig::new().identity(identity);

        server = server.tls_config(tls)?;

        info!(%addr, "Starting gRPC server (tonic with TLS)");
    } else {
        warn!(%addr, "Starting gRPC server (tonic without TLS) - enable TLS for Cloudflare Tunnel");
    }

    // Configure and run server
    server
        .max_frame_size(MAX_MESSAGE_SIZE as u32)
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
        )
        .serve(addr)
        .await?;

    Ok(())
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

    #[test]
    fn test_tls_config_disabled_by_default() {
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
