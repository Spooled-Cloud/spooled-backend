//! Workflow and Job Dependency Models
//!
//! This module contains models for job workflows and dependencies,
//! enabling complex job orchestration with DAG-based relationships.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::FromRow;
use validator::Validate;

/// Maximum jobs per workflow (prevent DoS)
/// Reduced from 1000 to 100 to prevent atomic creation DoS
pub const MAX_JOBS_PER_WORKFLOW: usize = 100;

/// Validate workflow job key for safe characters
fn validate_job_key(key: &str) -> Result<(), validator::ValidationError> {
    if !key
        .chars()
        .all(|c| c.is_alphanumeric() || c == '-' || c == '_')
    {
        let mut err = validator::ValidationError::new("invalid_job_key");
        err.message = Some(std::borrow::Cow::Borrowed(
            "Job key can only contain alphanumeric characters, dashes, and underscores",
        ));
        return Err(err);
    }
    Ok(())
}

/// Validate queue name for safe characters
fn validate_workflow_queue_name(name: &str) -> Result<(), validator::ValidationError> {
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
        let mut err = validator::ValidationError::new("invalid_queue_name_start");
        err.message = Some(std::borrow::Cow::Borrowed(
            "Queue name cannot start with dots or dashes",
        ));
        return Err(err);
    }
    Ok(())
}

/// Workflow status
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum WorkflowStatus {
    #[default]
    Pending,
    Running,
    Completed,
    Failed,
    Cancelled,
}

impl std::fmt::Display for WorkflowStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WorkflowStatus::Pending => write!(f, "pending"),
            WorkflowStatus::Running => write!(f, "running"),
            WorkflowStatus::Completed => write!(f, "completed"),
            WorkflowStatus::Failed => write!(f, "failed"),
            WorkflowStatus::Cancelled => write!(f, "cancelled"),
        }
    }
}

impl From<String> for WorkflowStatus {
    fn from(s: String) -> Self {
        match s.to_lowercase().as_str() {
            "running" => WorkflowStatus::Running,
            "completed" => WorkflowStatus::Completed,
            "failed" => WorkflowStatus::Failed,
            "cancelled" => WorkflowStatus::Cancelled,
            _ => WorkflowStatus::Pending,
        }
    }
}

/// Dependency mode for job dependencies
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum DependencyMode {
    /// All dependencies must complete before job can run
    #[default]
    All,
    /// Any one dependency completing is sufficient
    Any,
}

impl std::fmt::Display for DependencyMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DependencyMode::All => write!(f, "all"),
            DependencyMode::Any => write!(f, "any"),
        }
    }
}

impl From<String> for DependencyMode {
    fn from(s: String) -> Self {
        match s.to_lowercase().as_str() {
            "any" => DependencyMode::Any,
            _ => DependencyMode::All,
        }
    }
}

/// Maximum payload size per workflow job
pub const MAX_WORKFLOW_JOB_PAYLOAD_SIZE: usize = 1024 * 1024; // 1MB

/// Validate workflow name for safe characters
fn validate_workflow_name(name: &str) -> Result<(), validator::ValidationError> {
    // Allow alphanumeric, spaces, hyphens, underscores, and common punctuation
    if !name.chars().all(|c| {
        c.is_alphanumeric() || c == ' ' || c == '-' || c == '_' || c == '.' || c == '(' || c == ')'
    }) {
        let mut err = validator::ValidationError::new("invalid_workflow_name");
        err.message = Some(std::borrow::Cow::Borrowed(
            "Workflow name contains invalid characters",
        ));
        return Err(err);
    }
    Ok(())
}

/// Validate payload size
fn validate_payload_size(payload: &serde_json::Value) -> Result<(), validator::ValidationError> {
    let size = serde_json::to_string(payload).map(|s| s.len()).unwrap_or(0);

    if size > MAX_WORKFLOW_JOB_PAYLOAD_SIZE {
        let mut err = validator::ValidationError::new("payload_too_large");
        err.message = Some(std::borrow::Cow::Owned(format!(
            "Job payload too large: {} bytes (max: {} bytes)",
            size, MAX_WORKFLOW_JOB_PAYLOAD_SIZE
        )));
        return Err(err);
    }
    Ok(())
}

/// Workflow model
#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct Workflow {
    pub id: String,
    pub organization_id: String,
    pub name: String,
    pub description: Option<String>,
    pub status: String,
    pub total_jobs: i32,
    pub completed_jobs: i32,
    pub failed_jobs: i32,
    pub created_at: DateTime<Utc>,
    pub started_at: Option<DateTime<Utc>>,
    pub completed_at: Option<DateTime<Utc>>,
    pub metadata: Option<serde_json::Value>,
}

impl Workflow {
    /// Get workflow status as enum
    pub fn status_enum(&self) -> WorkflowStatus {
        WorkflowStatus::from(self.status.clone())
    }

    /// Calculate progress percentage
    pub fn progress_percent(&self) -> f64 {
        if self.total_jobs == 0 {
            0.0
        } else {
            (self.completed_jobs as f64 / self.total_jobs as f64) * 100.0
        }
    }

    /// Check if workflow is terminal
    pub fn is_terminal(&self) -> bool {
        matches!(
            self.status_enum(),
            WorkflowStatus::Completed | WorkflowStatus::Failed | WorkflowStatus::Cancelled
        )
    }
}

/// Job dependency model
#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct JobDependency {
    pub id: String,
    pub job_id: String,
    pub depends_on_job_id: String,
    pub dependency_type: String,
    pub created_at: DateTime<Utc>,
}

/// Request to create a workflow
///
/// Reduced max jobs from 1000 to 100
#[derive(Debug, Clone, Serialize, Deserialize, Validate)]
pub struct CreateWorkflowRequest {
    #[validate(length(min = 1, max = 255))]
    #[validate(custom(function = "validate_workflow_name"))]
    pub name: String,

    #[validate(length(max = 1000))]
    pub description: Option<String>,

    /// Jobs to create as part of this workflow
    /// Reduced from 1000 to 100 to prevent DoS
    #[validate(length(min = 1, max = 100, message = "Workflow must have 1-100 jobs"))]
    pub jobs: Vec<WorkflowJobDefinition>,

    /// Optional metadata
    pub metadata: Option<serde_json::Value>,
}

/// Job definition within a workflow
///
#[derive(Debug, Clone, Serialize, Deserialize, Validate)]
pub struct WorkflowJobDefinition {
    /// Unique key for this job within the workflow (for dependency references)
    /// Now validates for safe characters
    #[validate(length(min = 1, max = 100))]
    #[validate(custom(function = "validate_job_key"))]
    pub key: String,

    /// Queue to enqueue the job to
    /// Now validates for safe characters
    #[validate(length(min = 1, max = 255))]
    #[validate(custom(function = "validate_workflow_queue_name"))]
    pub queue_name: String,

    /// Job payload
    /// Now validates payload size
    #[validate(custom(function = "validate_payload_size"))]
    pub payload: serde_json::Value,

    /// Dependencies (keys of other jobs in this workflow)
    #[serde(default)]
    pub depends_on: Vec<String>,

    /// Dependency mode
    #[serde(default)]
    pub dependency_mode: DependencyMode,

    /// Job priority
    #[serde(default)]
    #[validate(range(min = -100, max = 100, message = "Priority must be between -100 and 100"))]
    pub priority: i32,

    /// Maximum retries
    #[validate(range(min = 0, max = 100, message = "Max retries must be between 0 and 100"))]
    pub max_retries: Option<i32>,

    /// Timeout in seconds
    #[validate(range(
        min = 1,
        max = 86400,
        message = "Timeout must be between 1 and 86400 seconds"
    ))]
    pub timeout_seconds: Option<i32>,
}

/// Response after creating a workflow
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateWorkflowResponse {
    pub workflow_id: String,
    pub job_ids: Vec<WorkflowJobMapping>,
    pub status: String,
}

/// Mapping from workflow job key to actual job ID
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkflowJobMapping {
    pub key: String,
    pub job_id: String,
}

/// Workflow summary response
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkflowResponse {
    pub id: String,
    pub name: String,
    pub description: Option<String>,
    pub status: String,
    pub total_jobs: i32,
    pub completed_jobs: i32,
    pub failed_jobs: i32,
    pub progress_percent: f64,
    pub created_at: DateTime<Utc>,
    pub started_at: Option<DateTime<Utc>>,
    pub completed_at: Option<DateTime<Utc>>,
    pub metadata: Option<serde_json::Value>,
}

impl From<Workflow> for WorkflowResponse {
    fn from(w: Workflow) -> Self {
        let progress = w.progress_percent();
        Self {
            id: w.id,
            name: w.name,
            description: w.description,
            status: w.status,
            total_jobs: w.total_jobs,
            completed_jobs: w.completed_jobs,
            failed_jobs: w.failed_jobs,
            progress_percent: progress,
            created_at: w.created_at,
            started_at: w.started_at,
            completed_at: w.completed_at,
            metadata: w.metadata,
        }
    }
}

/// Request to add dependencies to an existing job
#[derive(Debug, Clone, Serialize, Deserialize, Validate)]
pub struct AddDependenciesRequest {
    /// IDs of jobs that must complete before this job
    #[validate(length(min = 1, max = 100))]
    pub depends_on: Vec<String>,

    /// Dependency mode
    #[serde(default)]
    pub dependency_mode: DependencyMode,
}

/// Response after adding dependencies
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AddDependenciesResponse {
    pub dependencies_added: i32,
    pub dependencies_met: bool,
}

/// Job with dependency info
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobWithDependencies {
    pub job_id: String,
    pub dependencies: Vec<DependencyInfo>,
    pub dependents: Vec<DependencyInfo>,
    pub dependencies_met: bool,
}

/// Dependency info for a job
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DependencyInfo {
    pub job_id: String,
    pub queue_name: String,
    pub status: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_workflow_status_display() {
        assert_eq!(WorkflowStatus::Pending.to_string(), "pending");
        assert_eq!(WorkflowStatus::Running.to_string(), "running");
        assert_eq!(WorkflowStatus::Completed.to_string(), "completed");
        assert_eq!(WorkflowStatus::Failed.to_string(), "failed");
        assert_eq!(WorkflowStatus::Cancelled.to_string(), "cancelled");
    }

    #[test]
    fn test_workflow_status_from_string() {
        assert_eq!(
            WorkflowStatus::from("pending".to_string()),
            WorkflowStatus::Pending
        );
        assert_eq!(
            WorkflowStatus::from("RUNNING".to_string()),
            WorkflowStatus::Running
        );
        assert_eq!(
            WorkflowStatus::from("invalid".to_string()),
            WorkflowStatus::Pending
        );
    }

    #[test]
    fn test_dependency_mode_display() {
        assert_eq!(DependencyMode::All.to_string(), "all");
        assert_eq!(DependencyMode::Any.to_string(), "any");
    }

    #[test]
    fn test_dependency_mode_from_string() {
        assert_eq!(DependencyMode::from("all".to_string()), DependencyMode::All);
        assert_eq!(DependencyMode::from("ANY".to_string()), DependencyMode::Any);
        assert_eq!(
            DependencyMode::from("invalid".to_string()),
            DependencyMode::All
        );
    }

    #[test]
    fn test_workflow_progress() {
        let workflow = Workflow {
            id: uuid::Uuid::new_v4().to_string(),
            organization_id: "org".to_string(),
            name: "test".to_string(),
            description: None,
            status: "running".to_string(),
            total_jobs: 10,
            completed_jobs: 5,
            failed_jobs: 0,
            created_at: Utc::now(),
            started_at: Some(Utc::now()),
            completed_at: None,
            metadata: None,
        };

        assert_eq!(workflow.progress_percent(), 50.0);
    }

    #[test]
    fn test_workflow_progress_empty() {
        let workflow = Workflow {
            id: uuid::Uuid::new_v4().to_string(),
            organization_id: "org".to_string(),
            name: "test".to_string(),
            description: None,
            status: "pending".to_string(),
            total_jobs: 0,
            completed_jobs: 0,
            failed_jobs: 0,
            created_at: Utc::now(),
            started_at: None,
            completed_at: None,
            metadata: None,
        };

        assert_eq!(workflow.progress_percent(), 0.0);
    }

    #[test]
    fn test_workflow_is_terminal() {
        let mut workflow = Workflow {
            id: uuid::Uuid::new_v4().to_string(),
            organization_id: "org".to_string(),
            name: "test".to_string(),
            description: None,
            status: "pending".to_string(),
            total_jobs: 0,
            completed_jobs: 0,
            failed_jobs: 0,
            created_at: Utc::now(),
            started_at: None,
            completed_at: None,
            metadata: None,
        };

        assert!(!workflow.is_terminal());

        workflow.status = "completed".to_string();
        assert!(workflow.is_terminal());

        workflow.status = "failed".to_string();
        assert!(workflow.is_terminal());

        workflow.status = "cancelled".to_string();
        assert!(workflow.is_terminal());
    }

    #[test]
    fn test_create_workflow_validation() {
        let request = CreateWorkflowRequest {
            name: "".to_string(), // Invalid: empty
            description: None,
            jobs: vec![], // Invalid: empty
            metadata: None,
        };

        let result = request.validate();
        assert!(result.is_err());
    }

    #[test]
    fn test_workflow_response_from_workflow() {
        let workflow = Workflow {
            id: uuid::Uuid::new_v4().to_string(),
            organization_id: "org".to_string(),
            name: "test".to_string(),
            description: Some("desc".to_string()),
            status: "completed".to_string(),
            total_jobs: 10,
            completed_jobs: 10,
            failed_jobs: 0,
            created_at: Utc::now(),
            started_at: Some(Utc::now()),
            completed_at: Some(Utc::now()),
            metadata: None,
        };

        let response: WorkflowResponse = workflow.into();
        assert_eq!(response.progress_percent, 100.0);
        assert_eq!(response.status, "completed");
    }
}
