//! Queue configuration model and related types
//!
//! Queue configurations define per-queue settings for an organization.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::FromRow;
use validator::Validate;

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
        let mut err = validator::ValidationError::new("invalid_queue_name_start");
        err.message = Some(std::borrow::Cow::Borrowed(
            "Queue name cannot start with dots or dashes",
        ));
        return Err(err);
    }
    Ok(())
}

/// Maximum length for pause reason
const MAX_PAUSE_REASON_LENGTH: usize = 500;

/// Queue configuration entity
#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct QueueConfig {
    /// Unique identifier
    pub id: String,
    /// Organization that owns this config
    pub organization_id: String,
    /// Queue name
    pub queue_name: String,
    /// Maximum retries for jobs in this queue
    pub max_retries: i32,
    /// Default timeout in seconds
    pub default_timeout: i32,
    /// Rate limit (jobs per second, null = unlimited)
    pub rate_limit: Option<i32>,
    /// Whether the queue is enabled
    pub enabled: bool,
    /// Additional settings as JSON
    pub settings: serde_json::Value,
    /// Creation timestamp
    pub created_at: DateTime<Utc>,
    /// Last update timestamp
    pub updated_at: DateTime<Utc>,
}

/// Request to create or update queue configuration
///
#[derive(Debug, Deserialize, Validate)]
pub struct UpsertQueueConfigRequest {
    /// Queue name
    #[validate(length(min = 1, max = 100, message = "Queue name must be 1-100 characters"))]
    #[validate(custom(function = "validate_queue_name_chars"))]
    pub queue_name: String,

    /// Maximum retries
    #[validate(range(min = 0, max = 100, message = "Max retries must be between 0 and 100"))]
    pub max_retries: Option<i32>,

    /// Default timeout in seconds
    #[validate(range(min = 1, max = 86400, message = "Timeout must be between 1 and 86400"))]
    pub default_timeout: Option<i32>,

    /// Rate limit (jobs per second)
    #[validate(range(
        min = 1,
        max = 100000,
        message = "Rate limit must be between 1 and 100000"
    ))]
    pub rate_limit: Option<i32>,

    /// Whether the queue is enabled
    pub enabled: Option<bool>,

    /// Additional settings (max 64KB)
    /// Now validated for size
    #[validate(custom(function = "validate_settings_size"))]
    pub settings: Option<serde_json::Value>,
}

/// Maximum settings size in bytes (64KB)
const MAX_SETTINGS_SIZE: usize = 64 * 1024;

/// Validate settings size
fn validate_settings_size(settings: &serde_json::Value) -> Result<(), validator::ValidationError> {
    let json_str = serde_json::to_string(settings).unwrap_or_default();
    if json_str.len() > MAX_SETTINGS_SIZE {
        let mut err = validator::ValidationError::new("settings_too_large");
        err.message = Some(std::borrow::Cow::Owned(format!(
            "Settings too large: {} bytes (max: {} bytes)",
            json_str.len(),
            MAX_SETTINGS_SIZE
        )));
        return Err(err);
    }
    Ok(())
}

/// Queue configuration summary
#[derive(Debug, Serialize)]
pub struct QueueConfigSummary {
    pub queue_name: String,
    pub max_retries: i32,
    pub default_timeout: i32,
    pub rate_limit: Option<i32>,
    pub enabled: bool,
}

impl From<QueueConfig> for QueueConfigSummary {
    fn from(config: QueueConfig) -> Self {
        Self {
            queue_name: config.queue_name,
            max_retries: config.max_retries,
            default_timeout: config.default_timeout,
            rate_limit: config.rate_limit,
            enabled: config.enabled,
        }
    }
}

/// Queue statistics
#[derive(Debug, Serialize)]
pub struct QueueStats {
    pub queue_name: String,
    pub pending_jobs: i64,
    pub processing_jobs: i64,
    pub completed_jobs_24h: i64,
    pub failed_jobs_24h: i64,
    pub avg_processing_time_ms: Option<f64>,
    pub max_job_age_seconds: Option<i64>,
    pub active_workers: i64,
}

/// Validate pause reason length
fn validate_pause_reason(reason: &str) -> Result<(), validator::ValidationError> {
    if reason.len() > MAX_PAUSE_REASON_LENGTH {
        let mut err = validator::ValidationError::new("reason_too_long");
        err.message = Some(std::borrow::Cow::Owned(format!(
            "Pause reason must be at most {} characters",
            MAX_PAUSE_REASON_LENGTH
        )));
        return Err(err);
    }
    Ok(())
}

/// Request to pause a queue
///
#[derive(Debug, Deserialize, Validate)]
pub struct PauseQueueRequest {
    /// Reason for pausing (optional, max 500 chars)
    #[validate(custom(function = "validate_pause_reason"))]
    pub reason: Option<String>,
}

/// Response for pause operation
#[derive(Debug, Serialize)]
pub struct PauseQueueResponse {
    pub queue_name: String,
    pub paused: bool,
    pub paused_at: DateTime<Utc>,
    pub reason: Option<String>,
}

/// Response for resume operation
#[derive(Debug, Serialize)]
pub struct ResumeQueueResponse {
    pub queue_name: String,
    pub resumed: bool,
    pub paused_duration_secs: i64,
}

/// Extended queue info including pause state
#[derive(Debug, Serialize)]
pub struct QueueInfo {
    pub queue_name: String,
    pub enabled: bool,
    pub paused: bool,
    pub paused_at: Option<DateTime<Utc>>,
    pub paused_reason: Option<String>,
    pub config: QueueConfigSummary,
    pub stats: QueueStats,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_queue_config_summary() {
        let config = QueueConfig {
            id: "config-1".to_string(),
            organization_id: "org-1".to_string(),
            queue_name: "emails".to_string(),
            max_retries: 5,
            default_timeout: 600,
            rate_limit: Some(100),
            enabled: true,
            settings: serde_json::json!({}),
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };

        let summary = QueueConfigSummary::from(config);
        assert_eq!(summary.queue_name, "emails");
        assert_eq!(summary.max_retries, 5);
        assert!(summary.enabled);
    }
}
