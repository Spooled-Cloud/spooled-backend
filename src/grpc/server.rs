//! gRPC Server Bootstrap
//!
//! Sets up the tonic gRPC server with all services, health checks, and reflection.

use std::net::SocketAddr;
use std::sync::Arc;

use tonic::transport::Server;
use tonic_health::server::health_reporter;
use tracing::info;

use crate::grpc::proto::queue_service_server::QueueServiceServer;
use crate::grpc::proto::worker_service_server::WorkerServiceServer;
use crate::grpc::proto::FILE_DESCRIPTOR_SET;
use crate::grpc::services::{QueueServiceImpl, WorkerServiceImpl};
use crate::observability::Metrics;

/// Maximum message size for gRPC requests (4MB)
const MAX_MESSAGE_SIZE: usize = 4 * 1024 * 1024;

/// Start the gRPC server on the specified address
///
/// Features:
/// - QueueService: Job queue operations
/// - WorkerService: Worker lifecycle management
/// - Health service: gRPC health checks
/// - Reflection service: gRPC reflection for debugging
pub async fn start_grpc_server(
    addr: SocketAddr,
    db: Arc<crate::db::Database>,
    metrics: Arc<Metrics>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let pool = db.pool_arc();

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

    info!(%addr, "Starting gRPC server (tonic)");

    // Build and run the server
    Server::builder()
        // Configure message size limits
        .max_frame_size(MAX_MESSAGE_SIZE as u32)
        // Enable gzip compression
        .accept_http1(false)
        // Add services
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
}
