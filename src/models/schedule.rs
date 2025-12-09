//! Schedule models for recurring jobs
//!
//! This module defines the models for cron-like job scheduling.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::FromRow;
use validator::Validate;

/// A scheduled/recurring job definition
#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct Schedule {
    /// Unique schedule ID
    pub id: String,
    /// Organization ID (for RLS)
    pub organization_id: String,
    /// Human-readable name for the schedule
    pub name: String,
    /// Optional description
    pub description: Option<String>,
    /// Cron expression (e.g., "0 */5 * * * *" for every 5 minutes)
    pub cron_expression: String,
    /// Timezone for the cron expression
    pub timezone: String,
    /// Target queue name
    pub queue_name: String,
    /// Job payload template (JSON)
    pub payload_template: serde_json::Value,
    /// Job priority
    pub priority: i32,
    /// Max retries for scheduled jobs
    pub max_retries: i32,
    /// Timeout in seconds
    pub timeout_seconds: i32,
    /// Whether the schedule is active
    pub is_active: bool,
    /// Last time a job was created from this schedule
    pub last_run_at: Option<DateTime<Utc>>,
    /// Next scheduled run time
    pub next_run_at: Option<DateTime<Utc>>,
    /// Number of times this schedule has triggered
    pub run_count: i64,
    /// Optional tags for the created jobs
    pub tags: Option<serde_json::Value>,
    /// Optional metadata
    pub metadata: Option<serde_json::Value>,
    /// Created at
    pub created_at: DateTime<Utc>,
    /// Updated at
    pub updated_at: DateTime<Utc>,
}

/// Validate payload template size (for required field)
fn validate_payload_template_size_required(
    payload: &serde_json::Value,
) -> Result<(), validator::ValidationError> {
    let json_str = serde_json::to_string(payload).unwrap_or_default();
    if json_str.len() > 1024 * 1024 {
        // 1MB
        let mut err = validator::ValidationError::new("payload_template_too_large");
        err.message = Some(std::borrow::Cow::Borrowed(
            "Payload template too large (max 1MB)",
        ));
        return Err(err);
    }
    Ok(())
}

/// Validate timezone string
fn validate_timezone(timezone: &str) -> Result<(), validator::ValidationError> {
    // List of valid IANA timezones (common ones)
    // In production, use chrono-tz crate for full validation
    let valid_timezones = [
        "UTC",
        "GMT",
        "America/New_York",
        "America/Los_Angeles",
        "America/Chicago",
        "America/Denver",
        "America/Toronto",
        "America/Vancouver",
        "America/Mexico_City",
        "America/Sao_Paulo",
        "Europe/London",
        "Europe/Paris",
        "Europe/Berlin",
        "Europe/Moscow",
        "Europe/Rome",
        "Asia/Tokyo",
        "Asia/Shanghai",
        "Asia/Singapore",
        "Asia/Hong_Kong",
        "Asia/Seoul",
        "Asia/Dubai",
        "Asia/Kolkata",
        "Asia/Bangkok",
        "Australia/Sydney",
        "Australia/Melbourne",
        "Australia/Perth",
        "Pacific/Auckland",
        "Pacific/Honolulu",
        "Africa/Cairo",
        "Africa/Johannesburg",
        "Africa/Lagos",
    ];

    // Check if timezone is valid or starts with valid region
    if valid_timezones.contains(&timezone) {
        return Ok(());
    }

    // Also allow Etc/GMT+N and Etc/GMT-N formats
    if timezone.starts_with("Etc/GMT") {
        return Ok(());
    }

    // Allow offset formats like +00:00, -05:00
    if (timezone.starts_with('+') || timezone.starts_with('-'))
        && timezone.len() == 6
        && timezone.chars().nth(3) == Some(':')
    {
        return Ok(());
    }

    let mut err = validator::ValidationError::new("invalid_timezone");
    err.message = Some(std::borrow::Cow::Borrowed(
        "Invalid timezone. Use IANA timezone names like 'UTC', 'America/New_York', etc.",
    ));
    Err(err)
}

/// Request to create a new schedule
#[derive(Debug, Deserialize, Validate)]
pub struct CreateScheduleRequest {
    /// Human-readable name
    #[validate(length(min = 1, max = 255, message = "Name must be 1-255 characters"))]
    pub name: String,
    /// Optional description
    #[validate(length(max = 1000, message = "Description must be at most 1000 characters"))]
    pub description: Option<String>,
    /// Cron expression
    #[validate(length(min = 9, max = 100, message = "Invalid cron expression length"))]
    pub cron_expression: String,
    /// Timezone (defaults to UTC)
    /// Now validated against allowed timezone values
    #[validate(custom(function = "validate_timezone"))]
    pub timezone: Option<String>,
    /// Target queue
    #[validate(length(min = 1, max = 255, message = "Queue name must be 1-255 characters"))]
    pub queue_name: String,
    /// Payload template (max 1MB)
    /// Now validated for size
    #[validate(custom(function = "validate_payload_template_size_required"))]
    pub payload_template: serde_json::Value,
    /// Priority (default 0)
    #[validate(range(min = -100, max = 100, message = "Priority must be -100 to 100"))]
    pub priority: Option<i32>,
    /// Max retries (default 3)
    #[validate(range(min = 0, max = 100, message = "Max retries must be 0-100"))]
    pub max_retries: Option<i32>,
    /// Timeout seconds (default 300)
    #[validate(range(min = 1, max = 86400, message = "Timeout must be 1-86400 seconds"))]
    pub timeout_seconds: Option<i32>,
    /// Tags for jobs
    pub tags: Option<serde_json::Value>,
    /// Additional metadata
    pub metadata: Option<serde_json::Value>,
}

/// Response after creating a schedule
#[derive(Debug, Serialize)]
pub struct CreateScheduleResponse {
    pub id: String,
    pub name: String,
    pub cron_expression: String,
    pub next_run_at: Option<DateTime<Utc>>,
}

/// Validate optional timezone string
fn validate_optional_timezone(timezone: &str) -> Result<(), validator::ValidationError> {
    validate_timezone(timezone)
}

/// Maximum payload template size in bytes (1MB)
const MAX_PAYLOAD_TEMPLATE_SIZE: usize = 1024 * 1024;

/// Validate payload template size
fn validate_payload_template_size(
    payload: &serde_json::Value,
) -> Result<(), validator::ValidationError> {
    let json_str = serde_json::to_string(payload).unwrap_or_default();
    if json_str.len() > MAX_PAYLOAD_TEMPLATE_SIZE {
        let mut err = validator::ValidationError::new("payload_template_too_large");
        err.message = Some(std::borrow::Cow::Owned(format!(
            "Payload template too large: {} bytes (max: {} bytes)",
            json_str.len(),
            MAX_PAYLOAD_TEMPLATE_SIZE
        )));
        return Err(err);
    }
    Ok(())
}

/// Maximum error message length
const MAX_ERROR_MESSAGE_LENGTH: usize = 4096;

/// Request to update a schedule
///
#[derive(Debug, Deserialize, Validate)]
pub struct UpdateScheduleRequest {
    /// Human-readable name
    #[validate(length(min = 1, max = 255, message = "Name must be 1-255 characters"))]
    pub name: Option<String>,
    /// Optional description
    #[validate(length(max = 1000, message = "Description must be at most 1000 characters"))]
    pub description: Option<String>,
    /// Cron expression
    #[validate(length(min = 9, max = 100, message = "Invalid cron expression length"))]
    pub cron_expression: Option<String>,
    /// Timezone
    /// Now validated against allowed timezone values
    #[validate(custom(function = "validate_optional_timezone"))]
    pub timezone: Option<String>,
    /// Payload template
    /// Now validated for size
    #[validate(custom(function = "validate_payload_template_size"))]
    pub payload_template: Option<serde_json::Value>,
    /// Priority
    #[validate(range(min = -100, max = 100, message = "Priority must be -100 to 100"))]
    pub priority: Option<i32>,
    /// Max retries
    #[validate(range(min = 0, max = 100, message = "Max retries must be 0-100"))]
    pub max_retries: Option<i32>,
    /// Timeout seconds
    #[validate(range(min = 1, max = 86400, message = "Timeout must be 1-86400 seconds"))]
    pub timeout_seconds: Option<i32>,
    /// Whether active
    pub is_active: Option<bool>,
    /// Tags
    pub tags: Option<serde_json::Value>,
    /// Metadata
    pub metadata: Option<serde_json::Value>,
}

/// Schedule history entry
///
#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct ScheduleRun {
    /// Run ID
    pub id: String,
    /// Schedule ID
    pub schedule_id: String,
    /// Created job ID (if successful)
    pub job_id: Option<String>,
    /// Run status
    pub status: String,
    /// Error message if failed (max 4KB - truncated if longer)
    /// Error messages are truncated to MAX_ERROR_MESSAGE_LENGTH
    pub error_message: Option<String>,
    /// When the run started
    pub started_at: DateTime<Utc>,
    /// When the run completed
    pub completed_at: Option<DateTime<Utc>>,
}

impl ScheduleRun {
    /// Truncate error message if too long
    pub fn truncate_error_message(msg: &str) -> String {
        if msg.len() > MAX_ERROR_MESSAGE_LENGTH {
            format!("{}... [truncated]", &msg[..MAX_ERROR_MESSAGE_LENGTH - 15])
        } else {
            msg.to_string()
        }
    }
}

/// Parsed cron expression for scheduling
#[derive(Debug, Clone)]
pub struct CronSchedule {
    pub second: CronField,
    pub minute: CronField,
    pub hour: CronField,
    pub day_of_month: CronField,
    pub month: CronField,
    pub day_of_week: CronField,
}

/// A single field in a cron expression
#[derive(Debug, Clone)]
pub enum CronField {
    Any,
    Value(u8),
    Range(u8, u8),
    Step(u8),
    List(Vec<u8>),
}

impl CronSchedule {
    /// Parse a cron expression string
    /// Supports: second minute hour day-of-month month day-of-week
    /// Examples:
    /// "0 * * * * *" - every minute
    /// "0 */5 * * * *" - every 5 minutes
    /// "0 0 */2 * * *" - every 2 hours
    /// "0 0 0 * * *" - daily at midnight
    /// "0 0 0 * * 1" - every Monday at midnight
    pub fn parse(expression: &str) -> Result<Self, String> {
        let parts: Vec<&str> = expression.split_whitespace().collect();

        if parts.len() != 6 {
            return Err(format!(
                "Invalid cron expression: expected 6 fields, got {}",
                parts.len()
            ));
        }

        Ok(CronSchedule {
            second: Self::parse_field(parts[0], 0, 59)?,
            minute: Self::parse_field(parts[1], 0, 59)?,
            hour: Self::parse_field(parts[2], 0, 23)?,
            day_of_month: Self::parse_field(parts[3], 1, 31)?,
            month: Self::parse_field(parts[4], 1, 12)?,
            day_of_week: Self::parse_field(parts[5], 0, 6)?,
        })
    }

    fn parse_field(field: &str, min: u8, max: u8) -> Result<CronField, String> {
        if field == "*" {
            return Ok(CronField::Any);
        }

        // Handle step values (*/5)
        if let Some(step) = field.strip_prefix("*/") {
            let step: u8 = step
                .parse()
                .map_err(|_| format!("Invalid step value: {}", step))?;
            // Step value of 0 would cause infinite loop (value % 0 = panic)
            // and step=1 with certain values could cause very long iterations
            if step == 0 {
                return Err("Step value cannot be 0 - would cause infinite loop".to_string());
            }
            if step > max {
                return Err(format!(
                    "Step value {} exceeds maximum {} for this field",
                    step, max
                ));
            }
            return Ok(CronField::Step(step));
        }

        // Handle ranges (1-5)
        if field.contains('-') {
            let parts: Vec<&str> = field.split('-').collect();
            if parts.len() != 2 {
                return Err(format!("Invalid range: {}", field));
            }
            let start: u8 = parts[0]
                .parse()
                .map_err(|_| format!("Invalid range start: {}", parts[0]))?;
            let end: u8 = parts[1]
                .parse()
                .map_err(|_| format!("Invalid range end: {}", parts[1]))?;
            if start > end || start < min || end > max {
                return Err(format!("Range out of bounds: {}-{}", start, end));
            }
            return Ok(CronField::Range(start, end));
        }

        // Handle lists (1,3,5)
        if field.contains(',') {
            let values: Result<Vec<u8>, _> = field.split(',').map(|v| v.parse::<u8>()).collect();
            let values = values.map_err(|_| format!("Invalid list value in: {}", field))?;
            for &v in &values {
                if v < min || v > max {
                    return Err(format!("List value out of bounds: {}", v));
                }
            }
            return Ok(CronField::List(values));
        }

        // Single value
        let value: u8 = field
            .parse()
            .map_err(|_| format!("Invalid field value: {}", field))?;
        if value < min || value > max {
            return Err(format!("Value out of bounds: {}", value));
        }
        Ok(CronField::Value(value))
    }

    /// Check if a field matches a value
    fn field_matches(field: &CronField, value: u8) -> bool {
        match field {
            CronField::Any => true,
            CronField::Value(v) => *v == value,
            CronField::Range(start, end) => value >= *start && value <= *end,
            CronField::Step(step) => value.is_multiple_of(*step),
            CronField::List(values) => values.contains(&value),
        }
    }

    /// Calculate the next run time from a given time
    ///
    /// Optimized to prevent CPU exhaustion attacks
    /// Previously iterated up to 31M times (1 year in seconds), now uses smarter stepping
    pub fn next_run_after(&self, after: DateTime<Utc>) -> Option<DateTime<Utc>> {
        use chrono::{Datelike, Duration, Timelike};

        let mut current = after + Duration::seconds(1);
        // Set to start of second
        current = current.with_nanosecond(0).unwrap_or(current);

        // Use smarter iteration with larger steps when possible
        // Max iterations reduced to prevent CPU exhaustion
        // Each iteration now steps by at least 1 second, but can step by minutes/hours
        let max_iterations = 525600; // 1 year in minutes (much smaller than before)
        let mut iterations = 0;

        while iterations < max_iterations {
            let second = current.second() as u8;
            let minute = current.minute() as u8;
            let hour = current.hour() as u8;
            let day = current.day() as u8;
            let month = current.month() as u8;
            let weekday = current.weekday().num_days_from_sunday() as u8;

            if Self::field_matches(&self.second, second)
                && Self::field_matches(&self.minute, minute)
                && Self::field_matches(&self.hour, hour)
                && Self::field_matches(&self.day_of_month, day)
                && Self::field_matches(&self.month, month)
                && Self::field_matches(&self.day_of_week, weekday)
            {
                return Some(current);
            }

            // Smart stepping - if seconds don't match, skip to next minute
            // This reduces iterations dramatically for typical cron expressions
            let step = if !Self::field_matches(&self.minute, minute)
                && !Self::field_matches(&self.hour, hour)
            {
                // Skip to next hour if neither minute nor hour match
                Duration::seconds(3600 - (minute as i64 * 60) - second as i64)
            } else if !Self::field_matches(&self.minute, minute) {
                // Skip to next minute if minute doesn't match
                Duration::seconds(60 - second as i64)
            } else if !Self::field_matches(&self.second, second) {
                // Just step by 1 second if only second doesn't match
                Duration::seconds(1)
            } else {
                // Something else doesn't match, step by 1 minute to be safe
                Duration::seconds(60)
            };

            current += step.max(Duration::seconds(1));
            iterations += 1;
        }

        // Log warning if we hit iteration limit (potential attack or bad cron)
        tracing::warn!(
            iterations = iterations,
            "next_run_after hit iteration limit - possible invalid cron or far future schedule"
        );

        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Timelike};

    #[test]
    fn test_parse_every_minute() {
        let schedule = CronSchedule::parse("0 * * * * *").unwrap();
        assert!(matches!(schedule.second, CronField::Value(0)));
        assert!(matches!(schedule.minute, CronField::Any));
    }

    #[test]
    fn test_parse_every_5_minutes() {
        let schedule = CronSchedule::parse("0 */5 * * * *").unwrap();
        assert!(matches!(schedule.minute, CronField::Step(5)));
    }

    #[test]
    fn test_parse_range() {
        let schedule = CronSchedule::parse("0 0 9-17 * * *").unwrap();
        assert!(matches!(schedule.hour, CronField::Range(9, 17)));
    }

    #[test]
    fn test_parse_list() {
        let schedule = CronSchedule::parse("0 0 0 * * 1,3,5").unwrap();
        match schedule.day_of_week {
            CronField::List(days) => {
                assert_eq!(days, vec![1, 3, 5]);
            }
            _ => panic!("Expected list"),
        }
    }

    #[test]
    fn test_invalid_cron_expression() {
        assert!(CronSchedule::parse("* * *").is_err());
        assert!(CronSchedule::parse("0 60 * * * *").is_err());
        assert!(CronSchedule::parse("0 * 25 * * *").is_err());
    }

    #[test]
    fn test_next_run_every_minute() {
        let schedule = CronSchedule::parse("0 * * * * *").unwrap();
        let now = Utc.with_ymd_and_hms(2024, 1, 1, 12, 30, 45).unwrap();
        let next = schedule.next_run_after(now).unwrap();

        assert_eq!(next.minute(), 31);
        assert_eq!(next.second(), 0);
    }

    #[test]
    fn test_next_run_every_5_minutes() {
        let schedule = CronSchedule::parse("0 */5 * * * *").unwrap();
        let now = Utc.with_ymd_and_hms(2024, 1, 1, 12, 3, 0).unwrap();
        let next = schedule.next_run_after(now).unwrap();

        assert_eq!(next.minute(), 5);
        assert_eq!(next.second(), 0);
    }

    #[test]
    fn test_create_schedule_request_validation() {
        let valid = CreateScheduleRequest {
            name: "Daily Report".to_string(),
            description: Some("Generate daily report".to_string()),
            cron_expression: "0 0 0 * * *".to_string(),
            timezone: Some("UTC".to_string()),
            queue_name: "reports".to_string(),
            payload_template: serde_json::json!({"type": "daily"}),
            priority: Some(0),
            max_retries: Some(3),
            timeout_seconds: Some(300),
            tags: None,
            metadata: None,
        };
        assert!(valid.validate().is_ok());

        let invalid_name = CreateScheduleRequest {
            name: "".to_string(),
            description: None,
            cron_expression: "0 0 0 * * *".to_string(),
            timezone: None,
            queue_name: "reports".to_string(),
            payload_template: serde_json::json!({}),
            priority: None,
            max_retries: None,
            timeout_seconds: None,
            tags: None,
            metadata: None,
        };
        assert!(invalid_name.validate().is_err());
    }

    #[test]
    fn test_cron_field_matches() {
        assert!(CronSchedule::field_matches(&CronField::Any, 5));
        assert!(CronSchedule::field_matches(&CronField::Value(5), 5));
        assert!(!CronSchedule::field_matches(&CronField::Value(5), 6));
        assert!(CronSchedule::field_matches(&CronField::Range(3, 7), 5));
        assert!(!CronSchedule::field_matches(&CronField::Range(3, 7), 2));
        assert!(CronSchedule::field_matches(&CronField::Step(5), 10));
        assert!(!CronSchedule::field_matches(&CronField::Step(5), 11));
        assert!(CronSchedule::field_matches(
            &CronField::List(vec![1, 3, 5]),
            3
        ));
        assert!(!CronSchedule::field_matches(
            &CronField::List(vec![1, 3, 5]),
            2
        ));
    }
}
