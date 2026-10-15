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

// Generated protobuf types. `double_must_use` (new in clippy 1.99) fires on
// tonic's generated async-trait methods; `unknown_lints` keeps older toolchains
// that do not know that lint from failing under `-D warnings`.
#[allow(unknown_lints, clippy::large_enum_variant, clippy::double_must_use)]
pub mod proto {
    tonic::include_proto!("spooled.v1");

    /// File descriptor set for gRPC reflection
    pub const FILE_DESCRIPTOR_SET: &[u8] =
        tonic::include_file_descriptor_set!("spooled_descriptor");
}

// Re-exports

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

    /// Maximum batch size for dequeue
    pub const MAX_BATCH_SIZE: i32 = 100;

    /// Largest message the server encodes (`max_encoding_message_size`, 4 MiB).
    /// tonic enforces it only after a handler returns, so claim paths check a
    /// job against it before handing it out.
    pub const MAX_GRPC_MESSAGE_SIZE: usize = 4 * 1024 * 1024;
}

/// Validation functions for gRPC requests (exported for testing)
#[allow(clippy::result_large_err)]
pub mod validation {
    use tonic::Status;

    /// Validate queue name format
    pub fn validate_queue_name(name: &str) -> Result<(), Status> {
        // Characters, not bytes: RegisterWorker and REST count characters, so
        // a 100-character name of 3-byte letters registered and then failed
        // here on Dequeue.
        if name.is_empty() || name.chars().count() > 255 {
            return Err(Status::invalid_argument(
                "Queue name must be 1-255 characters",
            ));
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

    /// Clamp a requested lease duration to the safe range (5 s to 1 h).
    pub fn safe_lease_duration(secs: i32) -> i32 {
        use super::constants::*;
        secs.clamp(MIN_LEASE_DURATION_SECS, MAX_LEASE_DURATION_SECS)
    }

    /// Lease for a claim or renewal. Proto3 sends an omitted int32 as 0, so
    /// `<= 0` means "server default": `default_secs` is
    /// `GRPC_DEFAULT_LEASE_DURATION_SECS` (300 unless configured), not the
    /// 30 s REST default, because the Go SDK's `StreamJobs` cannot send a lease.
    pub fn lease_duration_or_default(secs: i32, default_secs: u64) -> i32 {
        if secs <= 0 {
            safe_lease_duration(i32::try_from(default_secs).unwrap_or(i32::MAX))
        } else {
            safe_lease_duration(secs)
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

    #[cfg(test)]
    mod tests {
        use super::*;

        // BEGRPC-10 / W3R-03: an omitted lease is the configured gRPC default,
        // clamped like any requested one.
        #[test]
        fn test_lease_duration_or_default() {
            assert_eq!(lease_duration_or_default(0, 300), 300);
            assert_eq!(lease_duration_or_default(0, 30), 30);
            assert_eq!(lease_duration_or_default(-10, 30), 30);
            assert_eq!(lease_duration_or_default(0, 7200), 3600);
            assert_eq!(lease_duration_or_default(0, 1), 5);
            assert_eq!(lease_duration_or_default(0, u64::MAX), 3600);
            assert_eq!(lease_duration_or_default(1, 30), 5);
            assert_eq!(lease_duration_or_default(60, 30), 60);
            assert_eq!(lease_duration_or_default(10_000, 30), 3600);
        }
    }
}
