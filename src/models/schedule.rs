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
    // Validate against the FULL IANA database via chrono-tz — the same engine the scheduler
    // uses in `next_run_after_in_timezone`. A previous hardcoded ~30-zone allow-list rejected
    // valid zones the scheduler could actually honor (e.g. "Europe/Kyiv"), so validation and
    // execution disagreed. This keeps them in lockstep.
    let tz = timezone.trim();

    // Full IANA names (covers UTC, GMT, Etc/GMT±N, and every region/city).
    if tz.parse::<chrono_tz::Tz>().is_ok() {
        return Ok(());
    }

    // Fixed UTC offsets like "+05:30" / "-08:00" (honored by the scheduler's parse_fixed_offset).
    if parse_fixed_offset(tz).is_some() {
        return Ok(());
    }

    let mut err = validator::ValidationError::new("invalid_timezone");
    err.message = Some(std::borrow::Cow::Borrowed(
        "Invalid timezone. Use an IANA name like 'UTC' or 'America/New_York', or a fixed offset like '+05:30'.",
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

/// Parse a "+HH:MM"/"-HH:MM" timezone string into a FixedOffset.
///
/// These are accepted by the schedule validator alongside IANA names, so the
/// scheduler must be able to honor them too.
fn parse_fixed_offset(s: &str) -> Option<chrono::FixedOffset> {
    let (sign, rest) = match s.as_bytes().first()? {
        b'+' => (1, &s[1..]),
        b'-' => (-1, &s[1..]),
        _ => return None,
    };
    let (h, m) = rest.split_once(':')?;
    let hours: i32 = h.parse().ok()?;
    let minutes: i32 = m.parse().ok()?;
    if hours > 23 || minutes > 59 {
        return None;
    }
    chrono::FixedOffset::east_opt(sign * (hours * 3600 + minutes * 60))
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
    /// `*/N`. `min` is the field's minimum (0 for sec/min/hour/dow, 1 for
    /// day-of-month and month) so that `*/N` counts from the field minimum as
    /// standard cron requires (e.g. day-of-month `*/2` = 1,3,5,... not 2,4,6,...).
    Step {
        step: u8,
        min: u8,
    },
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
        let mut parts: Vec<&str> = expression.split_whitespace().collect();

        // Accept standard 5-field cron (min hour dom mon dow) by defaulting the
        // seconds field to 0, alongside the 6-field (leading seconds) form. Most
        // users — and the SDK README examples — write 5-field cron.
        if parts.len() == 5 {
            parts.insert(0, "0");
        }

        if parts.len() != 6 {
            return Err(format!(
                "expected 5 fields (min hour dom mon dow) or 6 fields (with leading seconds), got {}",
                parts.len()
            ));
        }

        Ok(CronSchedule {
            second: Self::parse_field(parts[0], 0, 59)?,
            minute: Self::parse_field(parts[1], 0, 59)?,
            hour: Self::parse_field(parts[2], 0, 23)?,
            day_of_month: Self::parse_field(parts[3], 1, 31)?,
            month: Self::parse_field(parts[4], 1, 12)?,
            // Day-of-week accepts 0-7 where both 0 and 7 mean Sunday (standard cron).
            day_of_week: Self::parse_field(parts[5], 0, 7)?,
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
            return Ok(CronField::Step { step, min });
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
            // Standard cron: `*/N` fires at min, min+N, min+2N, ... within the
            // field range. Counting from `min` is required for day-of-month and
            // month (min=1); counting from 0 there fires on the wrong dates.
            CronField::Step { step, min } => value >= *min && (value - *min).is_multiple_of(*step),
            CronField::List(values) => values.contains(&value),
        }
    }

    /// Calculate the next run time from a given time (UTC wall clock)
    pub fn next_run_after(&self, after: DateTime<Utc>) -> Option<DateTime<Utc>> {
        self.next_run_after_tz(after, &Utc)
    }

    /// Calculate the next run time evaluating the cron fields against the wall
    /// clock of `timezone` (an IANA name like "America/New_York" or a fixed
    /// offset like "+05:30"), returning the result in UTC.
    ///
    /// Unrecognized/empty timezones fall back to UTC with a warning — a
    /// mis-stored timezone must degrade to the previous behavior, not stop the
    /// schedule.
    pub fn next_run_after_in_timezone(
        &self,
        after: DateTime<Utc>,
        timezone: &str,
    ) -> Option<DateTime<Utc>> {
        let tz = timezone.trim();
        if tz.is_empty() || tz.eq_ignore_ascii_case("UTC") {
            return self.next_run_after(after);
        }
        if let Ok(named) = tz.parse::<chrono_tz::Tz>() {
            return self.next_run_after_tz(after, &named);
        }
        if let Some(offset) = parse_fixed_offset(tz) {
            return self.next_run_after_tz(after, &offset);
        }
        tracing::warn!(timezone = %timezone, "Unrecognized schedule timezone; falling back to UTC");
        self.next_run_after(after)
    }

    /// Calculate the next run time from a given time, matching cron fields in
    /// the supplied timezone's wall clock.
    ///
    /// DST is handled by stepping through instants: wall times skipped by a
    /// spring-forward transition never match (that occurrence is skipped), and
    /// wall times repeated by a fall-back transition match their first
    /// occurrence.
    ///
    /// Optimized to prevent CPU exhaustion attacks
    /// Previously iterated up to 31M times (1 year in seconds), now uses smarter stepping
    pub fn next_run_after_tz<Z: chrono::TimeZone>(
        &self,
        after: DateTime<Utc>,
        tz: &Z,
    ) -> Option<DateTime<Utc>> {
        use chrono::{Datelike, Duration, Timelike};

        let after_local = after.with_timezone(tz);
        // Wall-clock of the reference instant. During a DST fall-back the same wall
        // clock occurs at two instants; without this guard next_run computed from a
        // fire instant returns the *repeated* wall time one hour later, double-firing
        // the schedule. Requiring the match's wall clock to be strictly after this
        // skips the repeat while leaving every normal (monotonic) case untouched.
        let after_naive = after_local.naive_local();
        let mut current = after_local + Duration::seconds(1);
        // Set to start of second
        current = current.clone().with_nanosecond(0).unwrap_or(current);

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

            // Day-of-month vs day-of-week follow standard (Vixie) cron semantics:
            // when BOTH fields are restricted (neither is `*`), the day matches if
            // EITHER matches (OR); if either field is `*`, they combine with AND
            // (the restricted field gates, the `*` is always true). Previously this
            // was an unconditional AND, so e.g. `0 0 0 15 * 1` fired only on a 15th
            // that is also a Monday instead of "the 15th OR any Monday".
            let dom_match = Self::field_matches(&self.day_of_month, day);
            // Sunday is both 0 and 7 in cron; chrono reports it as 0, so also
            // match a literal 7 in the day-of-week field.
            let dow_match = Self::field_matches(&self.day_of_week, weekday)
                || (weekday == 0 && Self::field_matches(&self.day_of_week, 7));
            let dom_restricted = !matches!(self.day_of_month, CronField::Any);
            let dow_restricted = !matches!(self.day_of_week, CronField::Any);
            let day_matches = if dom_restricted && dow_restricted {
                dom_match || dow_match
            } else {
                dom_match && dow_match
            };

            if Self::field_matches(&self.second, second)
                && Self::field_matches(&self.minute, minute)
                && Self::field_matches(&self.hour, hour)
                && Self::field_matches(&self.month, month)
                && day_matches
                // DST fall-back guard: never return a wall clock that is not strictly
                // after the reference wall clock (skips the repeated hour's re-occurrence).
                && current.naive_local() > after_naive
            {
                return Some(current.with_timezone(&Utc));
            }

            // Smart stepping - skip ahead by the coarsest unit that cannot match yet.
            // This reduces iterations dramatically for typical cron expressions.
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
            } else if !Self::field_matches(&self.hour, hour) {
                // Second and minute match but the hour does not: jump to the next
                // hour boundary rather than crawling minute by minute.
                Duration::seconds(3600 - (minute as i64 * 60) - second as i64)
            } else {
                // Time-of-day (sec/min/hour) fully matches but a DATE field
                // (day-of-month/month/day-of-week) does not: jump to the start of the
                // next day, so sparse schedules (e.g. once a year) resolve in ~365
                // iterations instead of exhausting the iteration cap and stalling.
                Duration::seconds(
                    86400 - (hour as i64 * 3600) - (minute as i64 * 60) - second as i64,
                )
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
        assert!(matches!(
            schedule.minute,
            CronField::Step { step: 5, min: 0 }
        ));
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
    fn test_next_run_in_timezone_est() {
        // Noon in New York during EST (winter, UTC-5) is 17:00 UTC.
        let schedule = CronSchedule::parse("0 0 12 * * *").unwrap();
        let now = Utc.with_ymd_and_hms(2026, 1, 15, 0, 0, 0).unwrap();
        let next = schedule
            .next_run_after_in_timezone(now, "America/New_York")
            .unwrap();
        assert_eq!(next, Utc.with_ymd_and_hms(2026, 1, 15, 17, 0, 0).unwrap());
    }

    #[test]
    fn test_next_run_in_timezone_edt() {
        // Noon in New York during EDT (summer, UTC-4) is 16:00 UTC.
        let schedule = CronSchedule::parse("0 0 12 * * *").unwrap();
        let now = Utc.with_ymd_and_hms(2026, 7, 15, 0, 0, 0).unwrap();
        let next = schedule
            .next_run_after_in_timezone(now, "America/New_York")
            .unwrap();
        assert_eq!(next, Utc.with_ymd_and_hms(2026, 7, 15, 16, 0, 0).unwrap());
    }

    #[test]
    fn test_next_run_shifts_across_dst_transition() {
        // US DST starts 2026-03-08 02:00 local: the 17:00 UTC fire on the 7th
        // becomes 16:00 UTC from the 8th onward.
        let schedule = CronSchedule::parse("0 0 12 * * *").unwrap();
        let before = Utc.with_ymd_and_hms(2026, 3, 7, 0, 0, 0).unwrap();
        let first = schedule
            .next_run_after_in_timezone(before, "America/New_York")
            .unwrap();
        assert_eq!(first, Utc.with_ymd_and_hms(2026, 3, 7, 17, 0, 0).unwrap());

        let second = schedule
            .next_run_after_in_timezone(first, "America/New_York")
            .unwrap();
        assert_eq!(second, Utc.with_ymd_and_hms(2026, 3, 8, 16, 0, 0).unwrap());
    }

    #[test]
    fn test_next_run_in_fixed_offset_timezone() {
        // 09:00 at +05:30 (no DST) is 03:30 UTC.
        let schedule = CronSchedule::parse("0 0 9 * * *").unwrap();
        let now = Utc.with_ymd_and_hms(2026, 1, 15, 0, 0, 0).unwrap();
        let next = schedule.next_run_after_in_timezone(now, "+05:30").unwrap();
        assert_eq!(next, Utc.with_ymd_and_hms(2026, 1, 15, 3, 30, 0).unwrap());
    }

    #[test]
    fn test_next_run_unknown_timezone_falls_back_to_utc() {
        let schedule = CronSchedule::parse("0 0 12 * * *").unwrap();
        let now = Utc.with_ymd_and_hms(2026, 1, 15, 0, 0, 0).unwrap();
        let next = schedule
            .next_run_after_in_timezone(now, "Not/A_Zone")
            .unwrap();
        assert_eq!(next, Utc.with_ymd_and_hms(2026, 1, 15, 12, 0, 0).unwrap());
    }

    #[test]
    fn test_parse_accepts_5_field_cron() {
        // Standard 5-field cron (min hour dom mon dow) — seconds default to 0.
        let five = CronSchedule::parse("0 9 * * *").unwrap();
        let six = CronSchedule::parse("0 0 9 * * *").unwrap();
        let now = Utc.with_ymd_and_hms(2026, 7, 8, 0, 0, 0).unwrap();
        assert_eq!(five.next_run_after(now), six.next_run_after(now));
        assert_eq!(
            five.next_run_after(now),
            Some(Utc.with_ymd_and_hms(2026, 7, 8, 9, 0, 0).unwrap())
        );
        // Other field counts are still rejected.
        assert!(CronSchedule::parse("0 9 * *").is_err());
        assert!(CronSchedule::parse("0 0 0 9 * * *").is_err());
    }

    #[test]
    fn test_cron_dom_dow_or_semantics() {
        // Standard cron: when BOTH day-of-month and day-of-week are restricted,
        // the schedule fires when EITHER matches (OR). "0 0 0 15 * 1" = midnight
        // on the 15th OR any Monday. From Thu 2026-07-09, next Monday is 07-13
        // (before the 15th), so next_run = 2026-07-13.
        let s = CronSchedule::parse("0 0 0 15 * 1").unwrap();
        let now = Utc.with_ymd_and_hms(2026, 7, 9, 0, 0, 0).unwrap();
        assert_eq!(
            s.next_run_after(now),
            Some(Utc.with_ymd_and_hms(2026, 7, 13, 0, 0, 0).unwrap())
        );
        // Only day-of-month restricted (dow = *) -> AND semantics: just the 15th.
        let dom_only = CronSchedule::parse("0 0 0 15 * *").unwrap();
        assert_eq!(
            dom_only.next_run_after(now),
            Some(Utc.with_ymd_and_hms(2026, 7, 15, 0, 0, 0).unwrap())
        );
        // Only day-of-week restricted (dom = *): next Monday.
        let dow_only = CronSchedule::parse("0 0 0 * * 1").unwrap();
        assert_eq!(
            dow_only.next_run_after(now),
            Some(Utc.with_ymd_and_hms(2026, 7, 13, 0, 0, 0).unwrap())
        );
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
        assert!(CronSchedule::field_matches(
            &CronField::Step { step: 5, min: 0 },
            10
        ));
        assert!(!CronSchedule::field_matches(
            &CronField::Step { step: 5, min: 0 },
            11
        ));
        // Day-of-month/month are 1-based: `*/2` dom = 1,3,5,... and `*/3` month = 1,4,7,10.
        let dom = CronSchedule::parse("0 0 0 */2 * *").unwrap().day_of_month;
        assert!(CronSchedule::field_matches(&dom, 1));
        assert!(!CronSchedule::field_matches(&dom, 2));
        assert!(CronSchedule::field_matches(&dom, 3));
        let mon = CronSchedule::parse("0 0 0 1 */3 *").unwrap().month;
        assert!(CronSchedule::field_matches(&mon, 1));
        assert!(CronSchedule::field_matches(&mon, 4));
        assert!(!CronSchedule::field_matches(&mon, 3));
        assert!(CronSchedule::field_matches(&mon, 10));
        assert!(CronSchedule::field_matches(
            &CronField::List(vec![1, 3, 5]),
            3
        ));
        assert!(!CronSchedule::field_matches(
            &CronField::List(vec![1, 3, 5]),
            2
        ));
    }

    #[test]
    fn test_next_run_no_double_fire_on_dst_fall_back() {
        // US DST ends 2026-11-01: 02:00 EDT -> 01:00 EST, so 01:30 local occurs
        // twice (05:30 UTC in EDT, 06:30 UTC in EST). A 01:30 daily schedule must
        // fire ONCE on Nov 1, not twice.
        let schedule = CronSchedule::parse("0 30 1 * * *").unwrap();
        let start = Utc.with_ymd_and_hms(2026, 10, 31, 12, 0, 0).unwrap();
        let first = schedule
            .next_run_after_in_timezone(start, "America/New_York")
            .unwrap();
        assert_eq!(first, Utc.with_ymd_and_hms(2026, 11, 1, 5, 30, 0).unwrap());
        let second = schedule
            .next_run_after_in_timezone(first, "America/New_York")
            .unwrap();
        // The repeated 06:30 UTC occurrence is skipped; next fire is the following day.
        assert_eq!(second, Utc.with_ymd_and_hms(2026, 11, 2, 6, 30, 0).unwrap());
    }

    #[test]
    fn test_next_run_skips_dst_spring_forward_gap() {
        // US DST starts 2026-03-08 02:00 local: clocks jump 02:00 -> 03:00, so
        // 02:30 never exists that day. After the Mar 7 02:30 fire, next must be
        // Mar 9 02:30 EDT (06:30 UTC), not a phantom Mar 8 instant.
        let schedule = CronSchedule::parse("0 30 2 * * *").unwrap();
        let start = Utc.with_ymd_and_hms(2026, 3, 7, 0, 0, 0).unwrap();
        let first = schedule
            .next_run_after_in_timezone(start, "America/New_York")
            .unwrap();
        assert_eq!(first, Utc.with_ymd_and_hms(2026, 3, 7, 7, 30, 0).unwrap());
        let second = schedule
            .next_run_after_in_timezone(first, "America/New_York")
            .unwrap();
        assert_eq!(second, Utc.with_ymd_and_hms(2026, 3, 9, 6, 30, 0).unwrap());
    }

    #[test]
    fn test_next_run_europe_kyiv_eet_eest() {
        // IANA Europe/Kyiv still observes DST in 2026 (EET UTC+2 winter /
        // EEST UTC+3 summer). Parliament voted to abolish DST but the law was
        // not signed — tzdb keeps the transitions. 12:00 local → 10:00Z Jan,
        // 09:00Z Jul.
        let schedule = CronSchedule::parse("0 0 12 * * *").unwrap();
        let winter = Utc.with_ymd_and_hms(2026, 1, 15, 0, 0, 0).unwrap();
        let summer = Utc.with_ymd_and_hms(2026, 7, 15, 0, 0, 0).unwrap();
        assert_eq!(
            schedule
                .next_run_after_in_timezone(winter, "Europe/Kyiv")
                .unwrap(),
            Utc.with_ymd_and_hms(2026, 1, 15, 10, 0, 0).unwrap()
        );
        assert_eq!(
            schedule
                .next_run_after_in_timezone(summer, "Europe/Kyiv")
                .unwrap(),
            Utc.with_ymd_and_hms(2026, 7, 15, 9, 0, 0).unwrap()
        );
    }

    #[test]
    fn test_next_run_europe_kyiv_no_double_fire_on_dst_fall_back() {
        // Kyiv DST ends 2026-10-25 04:00 EEST -> 03:00 EET. 03:30 local occurs
        // twice; daily 03:30 must fire once.
        let schedule = CronSchedule::parse("0 30 3 * * *").unwrap();
        let start = Utc.with_ymd_and_hms(2026, 10, 24, 12, 0, 0).unwrap();
        let first = schedule
            .next_run_after_in_timezone(start, "Europe/Kyiv")
            .unwrap();
        // 03:30 EEST on Oct 25 = 00:30 UTC
        assert_eq!(first, Utc.with_ymd_and_hms(2026, 10, 25, 0, 30, 0).unwrap());
        let second = schedule
            .next_run_after_in_timezone(first, "Europe/Kyiv")
            .unwrap();
        // Skip repeated 01:30 UTC; next is Oct 26 03:30 EET = 01:30 UTC
        assert_eq!(second, Utc.with_ymd_and_hms(2026, 10, 26, 1, 30, 0).unwrap());
    }

    #[test]
    fn test_next_run_sparse_yearly_schedule_resolves() {
        // Once-a-year schedule must resolve without exhausting the iteration cap.
        let schedule = CronSchedule::parse("0 0 0 1 1 *").unwrap(); // Jan 1 00:00
        let start = Utc.with_ymd_and_hms(2026, 3, 15, 0, 0, 0).unwrap();
        let next = schedule.next_run_after(start).unwrap();
        assert_eq!(next, Utc.with_ymd_and_hms(2027, 1, 1, 0, 0, 0).unwrap());
    }

    #[test]
    fn test_validate_timezone_accepts_full_iana_and_offsets() {
        // Previously-listed zones still pass.
        assert!(validate_timezone("UTC").is_ok());
        assert!(validate_timezone("America/New_York").is_ok());
        // Valid IANA zones that the old hardcoded list rejected but the scheduler supports.
        assert!(validate_timezone("Europe/Kyiv").is_ok());
        assert!(validate_timezone("America/Argentina/Buenos_Aires").is_ok());
        assert!(validate_timezone("Etc/GMT+5").is_ok());
        // Fixed offsets.
        assert!(validate_timezone("+05:30").is_ok());
        assert!(validate_timezone("-08:00").is_ok());
        // Still rejects nonsense.
        assert!(validate_timezone("Not/A_Zone").is_err());
        assert!(validate_timezone("+99:99").is_err());
    }
}
