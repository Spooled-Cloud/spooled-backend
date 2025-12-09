//! Job model and related types
//!
//! Jobs are the core unit of work in Spooled. Each job represents a task
//! to be processed by a worker.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::FromRow;
use validator::Validate;

/// Job status enumeration
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq, sqlx::Type)]
#[sqlx(type_name = "text", rename_all = "lowercase")]
#[serde(rename_all = "lowercase")]
pub enum JobStatus {
    /// Job is waiting to be processed
    #[default]
    Pending,
    /// Job is scheduled for future execution
    Scheduled,
    /// Job is currently being processed
    Processing,
    /// Job completed successfully
    Completed,
    /// Job failed and will not be retried
    Failed,
    /// Job failed after max retries
    Deadletter,
    /// Job was cancelled
    Cancelled,
}

impl std::fmt::Display for JobStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            JobStatus::Pending => write!(f, "pending"),
            JobStatus::Scheduled => write!(f, "scheduled"),
            JobStatus::Processing => write!(f, "processing"),
            JobStatus::Completed => write!(f, "completed"),
            JobStatus::Failed => write!(f, "failed"),
            JobStatus::Deadletter => write!(f, "deadletter"),
            JobStatus::Cancelled => write!(f, "cancelled"),
        }
    }
}

impl TryFrom<String> for JobStatus {
    type Error = String;

    fn try_from(s: String) -> Result<Self, Self::Error> {
        match s.as_str() {
            "pending" => Ok(JobStatus::Pending),
            "scheduled" => Ok(JobStatus::Scheduled),
            "processing" => Ok(JobStatus::Processing),
            "completed" => Ok(JobStatus::Completed),
            "failed" => Ok(JobStatus::Failed),
            "deadletter" => Ok(JobStatus::Deadletter),
            "cancelled" => Ok(JobStatus::Cancelled),
            _ => Err(format!("Invalid job status: {}", s)),
        }
    }
}

/// Job entity representing a unit of work
#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct Job {
    /// Unique identifier
    pub id: String,
    /// Organization that owns this job
    pub organization_id: String,
    /// Queue this job belongs to
    pub queue_name: String,
    /// Current status
    pub status: String,
    /// Job payload (JSON)
    pub payload: serde_json::Value,
    /// Result after completion (JSON)
    pub result: Option<serde_json::Value>,
    /// Number of retry attempts
    pub retry_count: i32,
    /// Maximum retry attempts
    pub max_retries: i32,
    /// Last error message
    pub last_error: Option<String>,
    /// Creation timestamp
    pub created_at: DateTime<Utc>,
    /// Scheduled execution time
    pub scheduled_at: Option<DateTime<Utc>>,
    /// When processing started
    pub started_at: Option<DateTime<Utc>>,
    /// When job completed
    pub completed_at: Option<DateTime<Utc>>,
    /// Job expiration time
    pub expires_at: Option<DateTime<Utc>>,
    /// Priority (higher = more urgent)
    pub priority: i32,
    /// Optional tags for filtering
    pub tags: Option<serde_json::Value>,
    /// Timeout in seconds
    pub timeout_seconds: i32,
    /// Parent job ID for DAG workflows
    pub parent_job_id: Option<String>,
    /// Webhook URL for completion notification
    pub completion_webhook: Option<String>,
    /// Assigned worker ID
    pub assigned_worker_id: Option<String>,
    /// Current lease ID
    pub lease_id: Option<String>,
    /// Lease expiration time
    pub lease_expires_at: Option<DateTime<Utc>>,
    /// Idempotency key for deduplication
    pub idempotency_key: Option<String>,
    /// Last update timestamp
    pub updated_at: DateTime<Utc>,
    /// Workflow ID this job belongs to (from migration 20241209000003)
    pub workflow_id: Option<String>,
    /// Dependency mode: 'all' (all deps must complete) or 'any' (any dep completes)
    pub dependency_mode: Option<String>,
    /// Whether all dependencies are met and job can be processed
    pub dependencies_met: Option<bool>,
}

/// Maximum payload size in bytes (1MB)
pub const MAX_JOB_PAYLOAD_SIZE: usize = 1024 * 1024;

/// Validate webhook URL for SSRF protection
fn validate_webhook_url(url: &str) -> Result<(), validator::ValidationError> {
    // Parse URL
    let parsed = match url::Url::parse(url) {
        Ok(u) => u,
        Err(_) => {
            let mut err = validator::ValidationError::new("invalid_url");
            err.message = Some(std::borrow::Cow::Borrowed("Invalid URL format"));
            return Err(err);
        }
    };

    // Must be HTTPS in production
    let is_production = std::env::var("RUST_ENV")
        .map(|v| v == "production")
        .unwrap_or(false);

    if is_production && parsed.scheme() != "https" {
        let mut err = validator::ValidationError::new("https_required");
        err.message = Some(std::borrow::Cow::Borrowed(
            "Webhook URL must use HTTPS in production",
        ));
        return Err(err);
    }

    // Check for private/internal IPs (SSRF protection)
    if let Some(host) = parsed.host_str() {
        let is_private = host == "localhost" 
            || host == "127.0.0.1"
            || host.starts_with("192.168.")
            || host.starts_with("10.")
            || host.starts_with("172.16.")
            || host.starts_with("172.17.")
            || host.starts_with("172.18.")
            || host.starts_with("172.19.")
            || host.starts_with("172.2")
            || host.starts_with("172.30.")
            || host.starts_with("172.31.")
            || host == "169.254.169.254" // AWS metadata
            || host.ends_with(".internal")
            || host.ends_with(".local");

        if is_private && is_production {
            let mut err = validator::ValidationError::new("private_ip");
            err.message = Some(std::borrow::Cow::Borrowed(
                "Webhook URL cannot point to private/internal addresses",
            ));
            return Err(err);
        }
    }

    Ok(())
}

/// Validate job payload size
fn validate_payload_size(payload: &serde_json::Value) -> Result<(), validator::ValidationError> {
    let json_str = serde_json::to_string(payload).unwrap_or_default();
    if json_str.len() > MAX_JOB_PAYLOAD_SIZE {
        let mut err = validator::ValidationError::new("payload_too_large");
        err.message = Some(std::borrow::Cow::Owned(format!(
            "Payload too large: {} bytes (max: {} bytes)",
            json_str.len(),
            MAX_JOB_PAYLOAD_SIZE
        )));
        return Err(err);
    }
    Ok(())
}

/// Maximum tags size in bytes (64KB)
const MAX_TAGS_SIZE: usize = 64 * 1024;

/// Validate tags size
fn validate_tags_size(tags: &serde_json::Value) -> Result<(), validator::ValidationError> {
    let json_str = serde_json::to_string(tags).unwrap_or_default();
    if json_str.len() > MAX_TAGS_SIZE {
        let mut err = validator::ValidationError::new("tags_too_large");
        err.message = Some(std::borrow::Cow::Owned(format!(
            "Tags too large: {} bytes (max: {} bytes)",
            json_str.len(),
            MAX_TAGS_SIZE
        )));
        return Err(err);
    }
    Ok(())
}

/// Validate queue name characters
fn validate_queue_name_chars(name: &str) -> Result<(), validator::ValidationError> {
    if !name
        .chars()
        .all(|c| c.is_alphanumeric() || c == '-' || c == '_' || c == '.')
    {
        let mut err = validator::ValidationError::new("invalid_queue_name");
        err.message = Some(std::borrow::Cow::Borrowed(
            "Queue name can only contain alphanumeric characters, dashes, underscores, and dots",
        ));
        return Err(err);
    }
    if name.starts_with('.') || name.starts_with('-') {
        let mut err = validator::ValidationError::new("invalid_queue_name");
        err.message = Some(std::borrow::Cow::Borrowed(
            "Queue name cannot start with dots or dashes",
        ));
        return Err(err);
    }
    Ok(())
}

/// Request to create a new job
///
#[derive(Debug, Deserialize, Validate)]
pub struct CreateJobRequest {
    /// Queue name (required)
    #[validate(length(min = 1, max = 100, message = "Queue name must be 1-100 characters"))]
    #[validate(custom(function = "validate_queue_name_chars"))]
    pub queue_name: String,

    /// Job payload (required)
    /// Validated for size
    #[validate(custom(function = "validate_payload_size"))]
    pub payload: serde_json::Value,

    /// Priority (default: 0)
    #[validate(range(min = -100, max = 100, message = "Priority must be between -100 and 100"))]
    pub priority: Option<i32>,

    /// Maximum retries (default: 3)
    #[validate(range(min = 0, max = 100, message = "Max retries must be between 0 and 100"))]
    pub max_retries: Option<i32>,

    /// Timeout in seconds (default: 300)
    #[validate(range(
        min = 1,
        max = 86400,
        message = "Timeout must be between 1 and 86400 seconds"
    ))]
    pub timeout_seconds: Option<i32>,

    /// Scheduled execution time (optional)
    pub scheduled_at: Option<DateTime<Utc>>,

    /// Job expiration time (optional)
    pub expires_at: Option<DateTime<Utc>>,

    /// Tags for filtering (optional, max 64KB)
    /// Now validated for size
    #[validate(custom(function = "validate_tags_size"))]
    pub tags: Option<serde_json::Value>,

    /// Parent job ID for DAG workflows (optional)
    pub parent_job_id: Option<String>,

    /// Webhook URL for completion notification (optional)
    #[validate(custom(function = "validate_webhook_url"))]
    pub completion_webhook: Option<String>,

    /// Idempotency key for deduplication (optional)
    #[validate(length(max = 255, message = "Idempotency key must be at most 255 characters"))]
    pub idempotency_key: Option<String>,
}

/// Response after creating a job
#[derive(Debug, Serialize)]
pub struct CreateJobResponse {
    /// Job ID (new or existing if idempotent)
    pub id: String,
    /// Whether this was a new job or existing (idempotent)
    pub created: bool,
}

/// Job summary for list responses
#[derive(Debug, Serialize)]
pub struct JobSummary {
    pub id: String,
    pub queue_name: String,
    pub status: String,
    pub priority: i32,
    pub retry_count: i32,
    pub created_at: DateTime<Utc>,
    pub scheduled_at: Option<DateTime<Utc>>,
    pub started_at: Option<DateTime<Utc>>,
    pub completed_at: Option<DateTime<Utc>>,
}

impl From<Job> for JobSummary {
    fn from(job: Job) -> Self {
        Self {
            id: job.id,
            queue_name: job.queue_name,
            status: job.status,
            priority: job.priority,
            retry_count: job.retry_count,
            created_at: job.created_at,
            scheduled_at: job.scheduled_at,
            started_at: job.started_at,
            completed_at: job.completed_at,
        }
    }
}

/// Allowed order_by fields (whitelist to prevent SQL injection)
pub const ALLOWED_ORDER_BY_FIELDS: &[&str] = &[
    "created_at",
    "updated_at",
    "scheduled_at",
    "completed_at",
    "priority",
    "status",
    "queue_name",
    "retry_count",
];

/// Query parameters for listing jobs
///
#[derive(Debug, Deserialize, Default)]
pub struct ListJobsQuery {
    /// Filter by queue name
    pub queue_name: Option<String>,
    /// Filter by status
    pub status: Option<String>,
    /// Maximum number of results (default: 50)
    pub limit: Option<i64>,
    /// Offset for pagination
    pub offset: Option<i64>,
    /// Order by field (default: created_at)
    pub order_by: Option<String>,
    /// Order direction (asc/desc, default: desc)
    pub order_dir: Option<String>,
}

impl ListJobsQuery {
    /// Get safe order_by field (validated against whitelist)
    pub fn safe_order_by(&self) -> &str {
        match &self.order_by {
            Some(field) if ALLOWED_ORDER_BY_FIELDS.contains(&field.as_str()) => field,
            _ => "created_at",
        }
    }

    /// Get safe order direction
    pub fn safe_order_dir(&self) -> &str {
        match &self.order_dir {
            Some(dir) if dir.eq_ignore_ascii_case("asc") => "ASC",
            _ => "DESC",
        }
    }
}

/// Job statistics
#[derive(Debug, Serialize)]
pub struct JobStats {
    pub pending: i64,
    pub scheduled: i64,
    pub processing: i64,
    pub completed: i64,
    pub failed: i64,
    pub deadletter: i64,
    pub cancelled: i64,
    pub total: i64,
}

/// Maximum job history details size (64KB)
const MAX_HISTORY_DETAILS_SIZE: usize = 64 * 1024;

/// Job history event
///
#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct JobHistory {
    pub id: String,
    pub job_id: String,
    pub event_type: String,
    /// Worker ID (sanitized - no control characters)
    /// Should be sanitized before storage
    pub worker_id: Option<String>,
    /// Event details (max 64KB - truncated if larger)
    /// Size-bounded to prevent large storage
    pub details: serde_json::Value,
    pub created_at: DateTime<Utc>,
}

impl JobHistory {
    /// Sanitize worker_id by removing control characters
    pub fn sanitize_worker_id(worker_id: &str) -> String {
        worker_id
            .chars()
            .filter(|c| !c.is_control() || c.is_whitespace())
            .take(255) // Max 255 chars
            .collect()
    }

    /// Truncate details if too large
    pub fn truncate_details(details: &serde_json::Value) -> serde_json::Value {
        let json_str = serde_json::to_string(details).unwrap_or_default();
        if json_str.len() > MAX_HISTORY_DETAILS_SIZE {
            serde_json::json!({
                "truncated": true,
                "original_size": json_str.len(),
                "message": "Details truncated due to size limit"
            })
        } else {
            details.clone()
        }
    }
}

/// Dead letter queue entry
#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct DeadLetterEntry {
    pub id: String,
    pub job_id: String,
    pub organization_id: String,
    pub queue_name: String,
    pub reason: String,
    pub original_payload: serde_json::Value,
    pub error_details: Option<serde_json::Value>,
    pub created_at: DateTime<Utc>,
}

/// Request for bulk job enqueue
///
#[derive(Debug, Serialize, Deserialize, Validate)]
pub struct BulkEnqueueRequest {
    /// Queue name for all jobs
    #[validate(length(min = 1, max = 100, message = "Queue name must be 1-100 characters"))]
    #[validate(custom(function = "validate_queue_name_chars"))]
    pub queue_name: String,

    /// Jobs to enqueue (max 100)
    #[validate(length(min = 1, max = 100, message = "Must provide 1-100 jobs"))]
    pub jobs: Vec<BulkJobItem>,

    /// Default priority for all jobs
    pub default_priority: Option<i32>,

    /// Default max retries for all jobs
    pub default_max_retries: Option<i32>,

    /// Default timeout for all jobs
    pub default_timeout_seconds: Option<i32>,
}

/// Individual job item in bulk enqueue
///
#[derive(Debug, Serialize, Deserialize, Validate)]
pub struct BulkJobItem {
    /// Job payload (required)
    /// Now validated for size (max 1MB)
    #[validate(custom(function = "validate_payload_size"))]
    pub payload: serde_json::Value,

    /// Override priority for this job
    #[validate(range(min = -100, max = 100, message = "Priority must be between -100 and 100"))]
    pub priority: Option<i32>,

    /// Idempotency key for this job
    #[validate(length(max = 255, message = "Idempotency key must be at most 255 characters"))]
    pub idempotency_key: Option<String>,

    /// Schedule time for this job
    pub scheduled_at: Option<DateTime<Utc>>,
}

/// Response for bulk job enqueue
#[derive(Debug, Serialize)]
pub struct BulkEnqueueResponse {
    /// Successfully enqueued jobs
    pub succeeded: Vec<BulkJobResult>,
    /// Failed jobs
    pub failed: Vec<BulkJobError>,
    /// Total jobs processed
    pub total: usize,
    /// Count of successfully enqueued
    pub success_count: usize,
    /// Count of failed
    pub failure_count: usize,
}

/// Result for a successfully enqueued job
#[derive(Debug, Serialize)]
pub struct BulkJobResult {
    /// Index in the original request
    pub index: usize,
    /// Job ID
    pub job_id: String,
    /// Whether job was newly created
    pub created: bool,
}

/// Error for a failed job in bulk enqueue
#[derive(Debug, Serialize)]
pub struct BulkJobError {
    /// Index in the original request
    pub index: usize,
    /// Error message
    pub error: String,
}

/// Maximum DLQ retry limit per request
/// Reduced from 1000 to prevent timeout/DoS
pub const MAX_DLQ_RETRY_LIMIT: i64 = 100;

/// Request to retry jobs from dead-letter queue
///
/// Reduced max limit to prevent DoS
#[derive(Debug, Serialize, Deserialize, Validate)]
pub struct RetryDlqRequest {
    /// Specific job IDs to retry (optional)
    pub job_ids: Option<Vec<String>>,

    /// Queue name filter (optional)
    #[validate(length(max = 100, message = "Queue name must be at most 100 characters"))]
    pub queue_name: Option<String>,

    /// Maximum jobs to retry (default: 100, max: 100)
    /// Reduced max from 1000 to 100 to prevent timeout
    #[validate(range(min = 1, max = 100, message = "Limit must be between 1 and 100"))]
    pub limit: Option<i64>,
}

impl RetryDlqRequest {
    /// Get safe limit capped at MAX_DLQ_RETRY_LIMIT
    pub fn safe_limit(&self) -> i64 {
        self.limit.unwrap_or(100).min(MAX_DLQ_RETRY_LIMIT)
    }
}

/// Response for DLQ retry operation
#[derive(Debug, Serialize)]
pub struct RetryDlqResponse {
    /// Jobs successfully retried
    pub retried_count: i64,
    /// Job IDs that were retried
    pub retried_jobs: Vec<String>,
}

/// Maximum jobs to purge per request
pub const MAX_PURGE_DLQ_LIMIT: i64 = 10000;

/// Request to purge dead-letter queue
///
#[derive(Debug, Serialize, Deserialize, Validate)]
pub struct PurgeDlqRequest {
    /// Queue name filter (optional)
    #[validate(length(max = 100, message = "Queue name must be at most 100 characters"))]
    pub queue_name: Option<String>,

    /// Only purge jobs older than this (optional)
    pub older_than: Option<DateTime<Utc>>,

    /// Maximum jobs to purge (default: 10000, max: 10000)
    #[validate(range(min = 1, max = 10000, message = "Limit must be between 1 and 10000"))]
    pub limit: Option<i64>,

    /// Confirm purge operation
    pub confirm: bool,
}

/// Response for DLQ purge operation
#[derive(Debug, Serialize)]
pub struct PurgeDlqResponse {
    /// Jobs purged
    pub purged_count: i64,
}

/// Request to boost job priority
#[derive(Debug, Serialize, Deserialize, Validate)]
pub struct BoostPriorityRequest {
    /// New priority (higher = processed first)
    #[validate(range(min = -100, max = 100, message = "Priority must be between -100 and 100"))]
    pub priority: i32,
}

/// Response for priority boost
#[derive(Debug, Serialize)]
pub struct BoostPriorityResponse {
    /// Job ID
    pub job_id: String,
    /// Old priority
    pub old_priority: i32,
    /// New priority
    pub new_priority: i32,
}

/// Paginated response wrapper
#[derive(Debug, Serialize)]
pub struct PaginatedResponse<T> {
    /// Items in current page
    pub items: Vec<T>,
    /// Total count (if available)
    pub total: Option<i64>,
    /// Current page offset
    pub offset: i64,
    /// Page size limit
    pub limit: i64,
    /// Has more pages
    pub has_more: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_job_status_display() {
        assert_eq!(JobStatus::Pending.to_string(), "pending");
        assert_eq!(JobStatus::Processing.to_string(), "processing");
        assert_eq!(JobStatus::Deadletter.to_string(), "deadletter");
    }

    #[test]
    fn test_job_status_from_string() {
        assert_eq!(
            JobStatus::try_from("pending".to_string()).unwrap(),
            JobStatus::Pending
        );
        assert_eq!(
            JobStatus::try_from("processing".to_string()).unwrap(),
            JobStatus::Processing
        );
        assert!(JobStatus::try_from("invalid".to_string()).is_err());
    }

    #[test]
    fn test_job_summary_from_job() {
        let now = Utc::now();
        let job = Job {
            id: "test-id".to_string(),
            organization_id: "org-id".to_string(),
            queue_name: "default".to_string(),
            status: "pending".to_string(),
            payload: serde_json::json!({}),
            result: None,
            retry_count: 0,
            max_retries: 3,
            last_error: None,
            created_at: now,
            scheduled_at: None,
            started_at: None,
            completed_at: None,
            expires_at: None,
            priority: 0,
            tags: None,
            timeout_seconds: 300,
            parent_job_id: None,
            completion_webhook: None,
            assigned_worker_id: None,
            lease_id: None,
            lease_expires_at: None,
            idempotency_key: None,
            updated_at: now,
            workflow_id: None,
            dependency_mode: None,
            dependencies_met: None,
        };

        let summary = JobSummary::from(job.clone());
        assert_eq!(summary.id, job.id);
        assert_eq!(summary.queue_name, job.queue_name);
    }

    #[test]
    fn test_bulk_enqueue_request_validation() {
        let request = BulkEnqueueRequest {
            queue_name: "test-queue".to_string(),
            jobs: vec![
                BulkJobItem {
                    payload: serde_json::json!({"key": "value1"}),
                    priority: Some(10),
                    idempotency_key: None,
                    scheduled_at: None,
                },
                BulkJobItem {
                    payload: serde_json::json!({"key": "value2"}),
                    priority: None,
                    idempotency_key: Some("idem-key".to_string()),
                    scheduled_at: None,
                },
            ],
            default_priority: Some(5),
            default_max_retries: Some(3),
            default_timeout_seconds: Some(600),
        };

        assert_eq!(request.queue_name, "test-queue");
        assert_eq!(request.jobs.len(), 2);
        assert_eq!(request.jobs[0].priority, Some(10));
        assert_eq!(request.default_priority, Some(5));
    }

    #[test]
    fn test_bulk_enqueue_response_serialization() {
        let response = BulkEnqueueResponse {
            succeeded: vec![
                BulkJobResult {
                    index: 0,
                    job_id: "job-1".to_string(),
                    created: true,
                },
                BulkJobResult {
                    index: 1,
                    job_id: "job-2".to_string(),
                    created: true,
                },
            ],
            failed: vec![BulkJobError {
                index: 2,
                error: "Duplicate key".to_string(),
            }],
            total: 3,
            success_count: 2,
            failure_count: 1,
        };

        let json = serde_json::to_string(&response).unwrap();
        assert!(json.contains("job-1"));
        assert!(json.contains("job-2"));
        assert!(json.contains("Duplicate key"));
        assert_eq!(response.success_count, 2);
        assert_eq!(response.failure_count, 1);
    }

    #[test]
    fn test_boost_priority_request() {
        let request = BoostPriorityRequest { priority: 50 };
        let json = serde_json::to_string(&request).unwrap();
        assert!(json.contains("50"));
    }

    #[test]
    fn test_boost_priority_response() {
        let response = BoostPriorityResponse {
            job_id: "job-123".to_string(),
            old_priority: 0,
            new_priority: 100,
        };

        assert_eq!(response.old_priority, 0);
        assert_eq!(response.new_priority, 100);
        assert_eq!(response.job_id, "job-123");
    }

    #[test]
    fn test_retry_dlq_request() {
        let request = RetryDlqRequest {
            job_ids: Some(vec!["job-1".to_string(), "job-2".to_string()]),
            queue_name: None,
            limit: Some(50),
        };

        assert_eq!(request.job_ids.as_ref().unwrap().len(), 2);
        assert_eq!(request.limit, Some(50));
    }

    #[test]
    fn test_retry_dlq_response() {
        let response = RetryDlqResponse {
            retried_count: 5,
            retried_jobs: vec!["job-1".to_string(), "job-2".to_string()],
        };

        assert_eq!(response.retried_count, 5);
        assert_eq!(response.retried_jobs.len(), 2);
    }

    #[test]
    fn test_purge_dlq_request() {
        let request = PurgeDlqRequest {
            queue_name: Some("failed-queue".to_string()),
            older_than: None,
            limit: Some(1000),
            confirm: true,
        };

        assert!(request.confirm);
        assert_eq!(request.queue_name, Some("failed-queue".to_string()));
        assert_eq!(request.limit, Some(1000));
    }

    #[test]
    fn test_purge_dlq_response() {
        let response = PurgeDlqResponse { purged_count: 42 };
        assert_eq!(response.purged_count, 42);
    }

    #[test]
    fn test_paginated_response() {
        let response: PaginatedResponse<String> = PaginatedResponse {
            items: vec!["item1".to_string(), "item2".to_string()],
            total: Some(100),
            offset: 0,
            limit: 10,
            has_more: true,
        };

        assert_eq!(response.items.len(), 2);
        assert_eq!(response.total, Some(100));
        assert!(response.has_more);
    }

    #[test]
    fn test_job_stats_total_calculation() {
        let stats = JobStats {
            pending: 10,
            scheduled: 5,
            processing: 3,
            completed: 100,
            failed: 2,
            deadletter: 1,
            cancelled: 4,
            total: 125,
        };

        let calculated_total = stats.pending
            + stats.scheduled
            + stats.processing
            + stats.completed
            + stats.failed
            + stats.deadletter
            + stats.cancelled;
        assert_eq!(calculated_total, stats.total);
    }
}
