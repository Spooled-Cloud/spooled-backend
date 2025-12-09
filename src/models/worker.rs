//! Worker model and related types
//!
//! Workers are job processors that dequeue and execute jobs from queues.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::FromRow;
use validator::Validate;

/// Maximum metadata size in bytes (64KB)
pub const MAX_WORKER_METADATA_SIZE: usize = 64 * 1024;

/// Valid worker statuses
pub const VALID_WORKER_STATUSES: &[&str] = &["healthy", "degraded", "draining", "offline"];

/// Validate hostname for safe characters
fn validate_hostname(hostname: &str) -> Result<(), validator::ValidationError> {
    // Hostnames should only contain alphanumeric, dots, dashes, and underscores
    if !hostname
        .chars()
        .all(|c| c.is_alphanumeric() || c == '.' || c == '-' || c == '_')
    {
        let mut err = validator::ValidationError::new("invalid_hostname");
        err.message = Some(std::borrow::Cow::Borrowed(
            "Hostname can only contain alphanumeric characters, dots, dashes, and underscores",
        ));
        return Err(err);
    }
    // Must not start or end with dot or dash
    if hostname.starts_with('.')
        || hostname.starts_with('-')
        || hostname.ends_with('.')
        || hostname.ends_with('-')
    {
        let mut err = validator::ValidationError::new("invalid_hostname_format");
        err.message = Some(std::borrow::Cow::Borrowed(
            "Hostname cannot start or end with dots or dashes",
        ));
        return Err(err);
    }
    Ok(())
}

/// Validate worker status
fn validate_worker_status(status: &str) -> Result<(), validator::ValidationError> {
    if !VALID_WORKER_STATUSES.contains(&status) {
        let mut err = validator::ValidationError::new("invalid_status");
        err.message = Some(std::borrow::Cow::Owned(format!(
            "Status must be one of: {}",
            VALID_WORKER_STATUSES.join(", ")
        )));
        return Err(err);
    }
    Ok(())
}

/// Validate metadata size
fn validate_metadata_size(metadata: &serde_json::Value) -> Result<(), validator::ValidationError> {
    let json_str = serde_json::to_string(metadata).unwrap_or_default();
    if json_str.len() > MAX_WORKER_METADATA_SIZE {
        let mut err = validator::ValidationError::new("metadata_too_large");
        err.message = Some(std::borrow::Cow::Owned(format!(
            "Metadata must be at most {} bytes (got {} bytes)",
            MAX_WORKER_METADATA_SIZE,
            json_str.len()
        )));
        return Err(err);
    }
    Ok(())
}

/// Worker status enumeration
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum WorkerStatus {
    /// Worker is healthy and processing jobs
    #[default]
    Healthy,
    /// Worker is degraded (e.g., above capacity)
    Degraded,
    /// Worker is offline
    Offline,
    /// Worker is draining (not accepting new jobs)
    Draining,
}

impl std::fmt::Display for WorkerStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WorkerStatus::Healthy => write!(f, "healthy"),
            WorkerStatus::Degraded => write!(f, "degraded"),
            WorkerStatus::Offline => write!(f, "offline"),
            WorkerStatus::Draining => write!(f, "draining"),
        }
    }
}

/// Worker entity representing a job processor
///
/// Updated to support multiple queue names (array)
/// Worker database model - column names match code conventions (with migration 20241209000007)
#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct Worker {
    /// Unique identifier
    pub id: String,
    /// Organization that owns this worker
    pub organization_id: String,
    /// Queue this worker processes (legacy single queue - deprecated)
    /// Now uses queue_names array, but kept for backwards compatibility
    pub queue_name: String,
    /// Queues this worker processes (array)
    #[serde(default)]
    pub queue_names: Vec<String>,
    /// Hostname where worker runs
    pub hostname: String,
    /// Worker type (e.g., "http", "email", "generic")
    pub worker_type: Option<String>,
    /// Maximum concurrent jobs
    pub max_concurrent_jobs: i32,
    /// Current number of jobs being processed
    pub current_job_count: i32,
    /// Current status
    pub status: String,
    /// Last heartbeat timestamp
    pub last_heartbeat: DateTime<Utc>,
    /// Additional metadata
    pub metadata: serde_json::Value,
    /// Worker version
    pub version: Option<String>,
    /// Creation timestamp
    pub created_at: DateTime<Utc>,
    /// Last update timestamp  
    pub updated_at: DateTime<Utc>,
}

/// Request to register a new worker
///
#[derive(Debug, Deserialize, Validate)]
pub struct RegisterWorkerRequest {
    /// Queue to process (required)
    #[validate(length(min = 1, max = 100, message = "Queue name must be 1-100 characters"))]
    pub queue_name: String,

    /// Hostname (required)
    /// Now validates for safe characters
    #[validate(length(min = 1, max = 255, message = "Hostname must be 1-255 characters"))]
    #[validate(custom(function = "validate_hostname"))]
    pub hostname: String,

    /// Worker type (optional)
    #[validate(length(max = 50, message = "Worker type must be at most 50 characters"))]
    pub worker_type: Option<String>,

    /// Maximum concurrent jobs (default: 5)
    #[validate(range(
        min = 1,
        max = 100,
        message = "Max concurrency must be between 1 and 100"
    ))]
    pub max_concurrency: Option<i32>,

    /// Additional metadata (optional, max 64KB)
    /// Now validates size
    #[validate(custom(function = "validate_metadata_size"))]
    pub metadata: Option<serde_json::Value>,

    /// Worker version (optional)
    #[validate(length(max = 50, message = "Version must be at most 50 characters"))]
    pub version: Option<String>,
}

/// Response after registering a worker
#[derive(Debug, Serialize)]
pub struct RegisterWorkerResponse {
    /// Worker ID
    pub id: String,
    /// Assigned queue
    pub queue_name: String,
    /// Lease duration in seconds
    pub lease_duration_secs: u64,
    /// Heartbeat interval in seconds
    pub heartbeat_interval_secs: u64,
}

/// Worker heartbeat request
///
#[derive(Debug, Deserialize, Validate)]
pub struct WorkerHeartbeatRequest {
    /// Current number of jobs being processed
    #[validate(range(
        min = 0,
        max = 1000,
        message = "Current jobs must be between 0 and 1000"
    ))]
    pub current_jobs: i32,
    /// Optional status override (must be valid status)
    /// Now validates against allowed values
    #[validate(length(max = 20, message = "Status must be at most 20 characters"))]
    #[validate(custom(function = "validate_worker_status"))]
    pub status: Option<String>,
    /// Optional metadata update (max 64KB)
    /// Now validates size
    #[validate(custom(function = "validate_metadata_size"))]
    pub metadata: Option<serde_json::Value>,
}

/// Worker summary for list responses
#[derive(Debug, Serialize)]
pub struct WorkerSummary {
    pub id: String,
    pub queue_name: String,
    pub hostname: String,
    pub status: String,
    pub current_jobs: i32,
    pub max_concurrency: i32,
    pub last_heartbeat: DateTime<Utc>,
}

impl From<Worker> for WorkerSummary {
    fn from(worker: Worker) -> Self {
        Self {
            id: worker.id,
            queue_name: worker.queue_name,
            hostname: worker.hostname,
            status: worker.status,
            current_jobs: worker.current_job_count,
            max_concurrency: worker.max_concurrent_jobs,
            last_heartbeat: worker.last_heartbeat,
        }
    }
}

/// Worker statistics
#[derive(Debug, Serialize)]
pub struct WorkerStats {
    pub total: i64,
    pub healthy: i64,
    pub degraded: i64,
    pub offline: i64,
    pub draining: i64,
    pub total_capacity: i64,
    pub current_load: i64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_worker_status_display() {
        assert_eq!(WorkerStatus::Healthy.to_string(), "healthy");
        assert_eq!(WorkerStatus::Degraded.to_string(), "degraded");
        assert_eq!(WorkerStatus::Offline.to_string(), "offline");
    }

    #[test]
    fn test_worker_summary_from_worker() {
        let now = Utc::now();
        let worker = Worker {
            id: "worker-1".to_string(),
            organization_id: "org-1".to_string(),
            queue_name: "default".to_string(),
            queue_names: vec!["default".to_string()],
            hostname: "worker-host-1".to_string(),
            worker_type: Some("http".to_string()),
            max_concurrent_jobs: 5,
            current_job_count: 2,
            status: "healthy".to_string(),
            last_heartbeat: now,
            metadata: serde_json::json!({}),
            version: Some("1.0.0".to_string()),
            created_at: now,
            updated_at: now,
        };

        let summary = WorkerSummary::from(worker.clone());
        assert_eq!(summary.id, worker.id);
        assert_eq!(summary.queue_name, worker.queue_name);
        assert_eq!(summary.current_jobs, 2); // Uses current_job_count now
    }

    #[test]
    fn test_worker_queue_names_array() {
        // Test that queue_names array works
        let now = Utc::now();
        let worker = Worker {
            id: "worker-2".to_string(),
            organization_id: "org-1".to_string(),
            queue_name: "emails".to_string(),
            queue_names: vec!["emails".to_string(), "notifications".to_string()],
            hostname: "worker-host-2".to_string(),
            worker_type: None,
            max_concurrent_jobs: 10,
            current_job_count: 0,
            status: "healthy".to_string(),
            last_heartbeat: now,
            metadata: serde_json::json!({}),
            version: None,
            created_at: now,
            updated_at: now,
        };

        assert_eq!(worker.queue_names.len(), 2);
        assert!(worker.queue_names.contains(&"emails".to_string()));
        assert!(worker.queue_names.contains(&"notifications".to_string()));
    }
}
