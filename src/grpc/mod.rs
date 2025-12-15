//! gRPC module for Spooled Backend
//!
//! Real gRPC implementation using tonic for HTTP/2 + Protobuf.
//!
//! This module provides:
//! - `QueueService`: Job queue operations (enqueue, dequeue, complete, fail, etc.)
//! - `WorkerService`: Worker lifecycle management (register, heartbeat, deregister)
//! - Server-streaming: `StreamJobs` for continuous job delivery
//! - Bidirectional streaming: `ProcessJobs` for real-time job processing

pub mod auth;
pub mod convert;
pub mod server;
pub mod services;

// Generated protobuf types
#[allow(clippy::large_enum_variant)]
pub mod proto {
    tonic::include_proto!("spooled.v1");

    /// File descriptor set for gRPC reflection
    pub const FILE_DESCRIPTOR_SET: &[u8] =
        tonic::include_file_descriptor_set!("spooled_descriptor");
}

// Re-exports
pub use server::start_grpc_server;

/// Constants for gRPC validation (re-exported for testing)
pub mod constants {
    /// Maximum payload size in bytes (1MB)
    pub const MAX_GRPC_PAYLOAD_SIZE: usize = 1024 * 1024;

    /// Maximum result size in bytes (1MB)
    pub const MAX_RESULT_SIZE: usize = 1024 * 1024;

    /// Minimum lease duration in seconds
    pub const MIN_LEASE_DURATION_SECS: i32 = 5;

    /// Maximum lease duration in seconds (1 hour)
    pub const MAX_LEASE_DURATION_SECS: i32 = 3600;

    /// Default lease duration in seconds (5 minutes)
    pub const DEFAULT_LEASE_DURATION_SECS: i32 = 300;

    /// Maximum batch size for dequeue
    pub const MAX_BATCH_SIZE: i32 = 100;
}

/// Validation functions for gRPC requests (exported for testing)
#[allow(clippy::result_large_err)]
pub mod validation {
    use tonic::Status;

    /// Validate queue name format
    pub fn validate_queue_name(name: &str) -> Result<(), Status> {
        if name.is_empty() || name.len() > 255 {
            return Err(Status::invalid_argument("Queue name must be 1-255 characters"));
        }
        if !name
            .chars()
            .all(|c| c.is_alphanumeric() || c == '-' || c == '_' || c == '.')
        {
            return Err(Status::invalid_argument(
                "Queue name can only contain alphanumeric characters, dashes, underscores, and dots",
            ));
        }
        if name.starts_with('.') || name.starts_with('-') {
            return Err(Status::invalid_argument(
                "Queue name cannot start with dots or dashes",
            ));
        }
        Ok(())
    }

    /// Validate and clamp lease duration to safe range
    pub fn safe_lease_duration(secs: i32) -> i32 {
        use super::constants::*;
        if secs <= 0 {
            DEFAULT_LEASE_DURATION_SECS
        } else {
            secs.clamp(MIN_LEASE_DURATION_SECS, MAX_LEASE_DURATION_SECS)
        }
    }

    /// Validate hostname format
    pub fn validate_hostname(hostname: &str) -> Result<(), Status> {
        const MAX_HOSTNAME_LENGTH: usize = 255;
        if hostname.len() > MAX_HOSTNAME_LENGTH {
            return Err(Status::invalid_argument("Hostname too long"));
        }
        if !hostname
            .chars()
            .all(|c| c.is_alphanumeric() || c == '-' || c == '.' || c == '_')
        {
            return Err(Status::invalid_argument(
                "Hostname can only contain alphanumeric characters, dots, dashes, and underscores",
            ));
        }
        if hostname.starts_with('.') || hostname.starts_with('-') {
            return Err(Status::invalid_argument(
                "Hostname cannot start with dots or dashes",
            ));
        }
        Ok(())
    }
}
