//! Manually defined gRPC message types
//!
//! These types mirror the proto definitions without requiring protoc.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Job status enum matching proto definition
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[repr(i32)]
pub enum GrpcJobStatus {
    Unspecified = 0,
    Pending = 1,
    Scheduled = 2,
    Processing = 3,
    Completed = 4,
    Failed = 5,
    Deadletter = 6,
    Cancelled = 7,
}

impl From<&str> for GrpcJobStatus {
    fn from(s: &str) -> Self {
        match s.to_lowercase().as_str() {
            "pending" => GrpcJobStatus::Pending,
            "scheduled" => GrpcJobStatus::Scheduled,
            "processing" => GrpcJobStatus::Processing,
            "completed" => GrpcJobStatus::Completed,
            "failed" => GrpcJobStatus::Failed,
            "deadletter" => GrpcJobStatus::Deadletter,
            "cancelled" => GrpcJobStatus::Cancelled,
            _ => GrpcJobStatus::Unspecified,
        }
    }
}

impl std::fmt::Display for GrpcJobStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            GrpcJobStatus::Unspecified => write!(f, "unspecified"),
            GrpcJobStatus::Pending => write!(f, "pending"),
            GrpcJobStatus::Scheduled => write!(f, "scheduled"),
            GrpcJobStatus::Processing => write!(f, "processing"),
            GrpcJobStatus::Completed => write!(f, "completed"),
            GrpcJobStatus::Failed => write!(f, "failed"),
            GrpcJobStatus::Deadletter => write!(f, "deadletter"),
            GrpcJobStatus::Cancelled => write!(f, "cancelled"),
        }
    }
}

/// Job message
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GrpcJob {
    pub id: String,
    pub organization_id: String,
    pub queue_name: String,
    pub status: GrpcJobStatus,
    pub payload: String,
    pub result: Option<String>,
    pub retry_count: i32,
    pub max_retries: i32,
    pub last_error: Option<String>,
    pub created_at: Option<DateTime<Utc>>,
    pub scheduled_at: Option<DateTime<Utc>>,
    pub started_at: Option<DateTime<Utc>>,
    pub completed_at: Option<DateTime<Utc>>,
    pub expires_at: Option<DateTime<Utc>>,
    pub priority: i32,
    pub tags: Option<String>,
    pub timeout_seconds: i32,
    pub parent_job_id: Option<String>,
    pub completion_webhook: Option<String>,
    pub assigned_worker_id: Option<String>,
    pub lease_id: Option<String>,
    pub lease_expires_at: Option<DateTime<Utc>>,
    pub idempotency_key: Option<String>,
}

/// Maximum payload size for gRPC requests (1MB)
pub const MAX_GRPC_PAYLOAD_SIZE: usize = 1024 * 1024;

/// Maximum lease duration in seconds (1 hour)
pub const MAX_LEASE_DURATION_SECS: i32 = 3600;

/// Minimum lease duration in seconds (5 seconds)
pub const MIN_LEASE_DURATION_SECS: i32 = 5;

/// Request to enqueue a job
///
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EnqueueJobRequest {
    pub organization_id: String,
    pub queue_name: String,
    pub payload: String,
    pub priority: i32,
    pub max_retries: i32,
    pub timeout_seconds: i32,
    pub scheduled_at: Option<DateTime<Utc>>,
    pub idempotency_key: Option<String>,
    pub tags: Option<String>,
    pub completion_webhook: Option<String>,
    pub parent_job_id: Option<String>,
    pub expires_at: Option<DateTime<Utc>>,
}

impl EnqueueJobRequest {
    /// Validate request payload size and fields
    pub fn validate(&self) -> Result<(), String> {
        if self.payload.len() > MAX_GRPC_PAYLOAD_SIZE {
            return Err(format!(
                "Payload too large: {} bytes (max: {} bytes)",
                self.payload.len(),
                MAX_GRPC_PAYLOAD_SIZE
            ));
        }
        if self.queue_name.is_empty() || self.queue_name.len() > 255 {
            return Err("Queue name must be 1-255 characters".to_string());
        }
        if !self
            .queue_name
            .chars()
            .all(|c| c.is_alphanumeric() || c == '-' || c == '_' || c == '.')
        {
            return Err("Queue name contains invalid characters".to_string());
        }
        Ok(())
    }
}

/// Response after enqueueing
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EnqueueJobResponse {
    pub job_id: String,
    pub created: bool,
}

/// Request to dequeue a job
///
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DequeueJobRequest {
    pub organization_id: String,
    pub queue_name: String,
    pub worker_id: String,
    pub lease_duration_seconds: i32,
}

impl DequeueJobRequest {
    /// Validate and clamp lease duration to safe range
    pub fn safe_lease_duration(&self) -> i32 {
        self.lease_duration_seconds
            .clamp(MIN_LEASE_DURATION_SECS, MAX_LEASE_DURATION_SECS)
    }
}

/// Response after dequeuing
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DequeueJobResponse {
    pub job: Option<GrpcJob>,
}

/// Request to complete a job
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompleteJobRequest {
    pub job_id: String,
    pub worker_id: String,
    pub result: Option<String>,
}

/// Request to fail a job
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FailJobRequest {
    pub job_id: String,
    pub worker_id: String,
    pub error_message: String,
}

/// Request to renew a lease
///
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RenewLeaseRequest {
    pub job_id: String,
    pub worker_id: String,
    pub lease_duration_seconds: i32,
}

impl RenewLeaseRequest {
    /// Validate and clamp lease duration to safe range
    pub fn safe_lease_duration(&self) -> i32 {
        self.lease_duration_seconds
            .clamp(MIN_LEASE_DURATION_SECS, MAX_LEASE_DURATION_SECS)
    }
}

/// Response after renewing lease
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RenewLeaseResponse {
    pub renewed: bool,
    pub new_lease_expires_at: Option<DateTime<Utc>>,
}

/// Worker registration request
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegisterWorkerRequest {
    pub worker_id: String,
    pub organization_id: String,
    pub queue_names: Vec<String>,
    pub max_concurrent_jobs: i32,
    pub hostname: Option<String>,
    pub metadata: Option<String>,
}

/// Worker registration response
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegisterWorkerResponse {
    pub registered: bool,
    pub heartbeat_interval_seconds: i32,
}

/// Worker heartbeat request
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HeartbeatRequest {
    pub worker_id: String,
    pub current_job_ids: Vec<String>,
    pub status: String,
}

/// Worker heartbeat response
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HeartbeatResponse {
    pub acknowledged: bool,
}

/// Stream jobs request (for bidirectional streaming)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamJobsRequest {
    pub organization_id: String,
    pub queue_names: Vec<String>,
    pub worker_id: String,
    pub max_concurrent: i32,
}

/// Get job request
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GetJobRequest {
    pub job_id: String,
}

/// Get job response
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GetJobResponse {
    pub job: Option<GrpcJob>,
}

/// Get queue stats request
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GetQueueStatsRequest {
    pub queue_name: String,
}

/// Get queue stats response
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GetQueueStatsResponse {
    pub queue_name: String,
    pub pending: i64,
    pub scheduled: i64,
    pub processing: i64,
    pub completed: i64,
    pub failed: i64,
    pub deadletter: i64,
    pub total: i64,
    pub max_age_ms: Option<i64>,
}

/// Worker deregister request
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeregisterRequest {
    pub worker_id: String,
}

/// Worker deregister response
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeregisterResponse {
    pub success: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_grpc_job_status_from_str() {
        assert_eq!(GrpcJobStatus::from("pending"), GrpcJobStatus::Pending);
        assert_eq!(GrpcJobStatus::from("COMPLETED"), GrpcJobStatus::Completed);
        assert_eq!(GrpcJobStatus::from("unknown"), GrpcJobStatus::Unspecified);
    }

    #[test]
    fn test_grpc_job_status_display() {
        assert_eq!(GrpcJobStatus::Pending.to_string(), "pending");
        assert_eq!(GrpcJobStatus::Completed.to_string(), "completed");
    }

    #[test]
    fn test_enqueue_request_serialization() {
        let req = EnqueueJobRequest {
            organization_id: "org-1".to_string(),
            queue_name: "emails".to_string(),
            payload: r#"{"to": "test@example.com"}"#.to_string(),
            priority: 0,
            max_retries: 3,
            timeout_seconds: 300,
            scheduled_at: None,
            idempotency_key: None,
            tags: None,
            completion_webhook: None,
            parent_job_id: None,
            expires_at: None,
        };

        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains("org-1"));
        assert!(json.contains("emails"));
    }
}
