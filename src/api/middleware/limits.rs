//! Plan limits checking middleware and utilities
//!
//! Provides functions to check and enforce plan-based resource limits.

use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde::Serialize;
use sqlx::PgPool;

use crate::config::{LimitError, PlanLimits};

/// Response returned when a plan limit is exceeded
#[derive(Debug, Serialize)]
pub struct LimitExceededResponse {
    pub error: String,
    /// Stable machine-readable code so SDKs classify this correctly (previously
    /// the body had no `code`, so clients fell back to "UNKNOWN_ERROR").
    pub code: String,
    pub message: String,
    pub resource: String,
    pub current: u64,
    pub limit: u64,
    pub plan: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub upgrade_to: Option<String>,
}

impl From<LimitError> for LimitExceededResponse {
    fn from(err: LimitError) -> Self {
        Self {
            error: "limit_exceeded".to_string(),
            code: "QUOTA_EXCEEDED".to_string(),
            message: err.to_string(),
            resource: err.resource,
            current: err.current,
            limit: err.limit,
            plan: err.plan,
            upgrade_to: err.upgrade_to,
        }
    }
}

impl IntoResponse for LimitExceededResponse {
    fn into_response(self) -> Response {
        // 429 (not 403): exceeding a plan quota/limit is a "too many" condition,
        // matching the concurrency-race path that already returned 429.
        (StatusCode::TOO_MANY_REQUESTS, Json(self)).into_response()
    }
}

/// Response returned when a feature is disabled on the current plan
#[derive(Debug, Serialize)]
pub struct FeatureDisabledResponse {
    pub error: String,
    pub message: String,
    pub feature: String,
    pub plan: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub upgrade_to: Option<String>,
}

impl IntoResponse for FeatureDisabledResponse {
    fn into_response(self) -> Response {
        (StatusCode::FORBIDDEN, Json(self)).into_response()
    }
}

/// Organization resource counts from database
#[derive(Debug, Default)]
pub struct ResourceCounts {
    pub active_jobs: u64,
    pub queues: u64,
    pub workers: u64,
    pub api_keys: u64,
    pub schedules: u64,
    pub workflows: u64,
    pub webhooks: u64,
    pub jobs_today: u64,
}

/// Get current resource counts for an organization
pub async fn get_resource_counts(
    pool: &PgPool,
    org_id: &str,
) -> Result<ResourceCounts, sqlx::Error> {
    let row: (i64, i64, i64, i64, i64, i64, i64, i64) =
        sqlx::query_as("SELECT * FROM get_org_resource_counts($1)")
            .bind(org_id)
            .fetch_one(pool)
            .await?;

    Ok(ResourceCounts {
        active_jobs: row.0 as u64,
        queues: row.1 as u64,
        workers: row.2 as u64,
        api_keys: row.3 as u64,
        schedules: row.4 as u64,
        workflows: row.5 as u64,
        webhooks: row.6 as u64,
        jobs_today: row.7 as u64,
    })
}

/// Get the plan tier for an organization
pub async fn get_org_plan_tier(pool: &PgPool, org_id: &str) -> Result<String, sqlx::Error> {
    let row: (String,) = sqlx::query_as("SELECT plan_tier FROM organizations WHERE id = $1")
        .bind(org_id)
        .fetch_one(pool)
        .await?;

    Ok(row.0)
}

/// Get the plan tier and custom limits for an organization
pub async fn get_org_plan_and_limits(
    pool: &PgPool,
    org_id: &str,
) -> Result<(String, Option<serde_json::Value>), sqlx::Error> {
    let row: (String, Option<serde_json::Value>) =
        sqlx::query_as("SELECT plan_tier, custom_limits FROM organizations WHERE id = $1")
            .bind(org_id)
            .fetch_one(pool)
            .await?;

    Ok(row)
}

/// Check if creating a resource would exceed limits
///
/// Returns Ok(()) if allowed, Err with response if limit exceeded
pub async fn check_resource_limit(
    pool: &PgPool,
    org_id: &str,
    resource: &str,
    adding: u64,
) -> Result<(), Response> {
    let (plan_tier, custom_limits) = get_org_plan_and_limits(pool, org_id).await.map_err(|e| {
        tracing::error!(error = %e, "Failed to get org plan tier");
        (StatusCode::INTERNAL_SERVER_ERROR, "Internal error").into_response()
    })?;

    let limits = PlanLimits::for_tier_with_overrides(&plan_tier, custom_limits.as_ref());

    // Check if feature is disabled
    if limits.is_disabled(resource) {
        return Err(FeatureDisabledResponse {
            error: "feature_disabled".to_string(),
            message: format!(
                "{} is not available on the {} plan. Upgrade to unlock this feature.",
                resource, limits.display_name
            ),
            feature: resource.to_string(),
            plan: limits.tier.clone(),
            upgrade_to: Some("starter".to_string()),
        }
        .into_response());
    }

    let counts = get_resource_counts(pool, org_id).await.map_err(|e| {
        tracing::error!(error = %e, "Failed to get resource counts");
        (StatusCode::INTERNAL_SERVER_ERROR, "Internal error").into_response()
    })?;

    let current = match resource {
        "jobs_per_day" => counts.jobs_today,
        "active_jobs" => counts.active_jobs,
        "queues" => counts.queues,
        "workers" => counts.workers,
        "api_keys" => counts.api_keys,
        "schedules" => counts.schedules,
        "workflows" => counts.workflows,
        "webhooks" => counts.webhooks,
        _ => return Ok(()), // Unknown resource, allow
    };

    limits
        .check_limit(resource, current, adding)
        .map_err(|e| LimitExceededResponse::from(e).into_response())
}

/// Acquire a per-`(org, resource)` transaction-scoped advisory lock so that
/// concurrent creators of the same resource for the same org serialize. The lock
/// releases automatically when the transaction commits/rolls back. Must be held
/// across [`check_resource_limit_conn`] AND the caller's INSERT so the cap is
/// enforced atomically (otherwise concurrent creates each read count < limit).
pub async fn lock_resource(
    conn: &mut sqlx::PgConnection,
    org_id: &str,
    resource: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query("SELECT pg_advisory_xact_lock(hashtext($1))")
        .bind(format!("res_limit:{}:{}", org_id, resource))
        .execute(conn)
        .await?;
    Ok(())
}

/// Same as [`check_resource_limit`] but runs the plan + count reads on a
/// caller-provided connection, so the check and the caller's INSERT can share
/// one transaction guarded by [`lock_resource`]. This is what makes the resource
/// cap exact under concurrency.
pub async fn check_resource_limit_conn(
    conn: &mut sqlx::PgConnection,
    org_id: &str,
    resource: &str,
    adding: u64,
) -> Result<(), Response> {
    let (plan_tier, custom_limits): (String, Option<serde_json::Value>) =
        sqlx::query_as("SELECT plan_tier, custom_limits FROM organizations WHERE id = $1")
            .bind(org_id)
            .fetch_one(&mut *conn)
            .await
            .map_err(|e| {
                tracing::error!(error = %e, "Failed to get org plan tier");
                (StatusCode::INTERNAL_SERVER_ERROR, "Internal error").into_response()
            })?;

    let limits = PlanLimits::for_tier_with_overrides(&plan_tier, custom_limits.as_ref());

    if limits.is_disabled(resource) {
        return Err(FeatureDisabledResponse {
            error: "feature_disabled".to_string(),
            message: format!(
                "{} is not available on the {} plan. Upgrade to unlock this feature.",
                resource, limits.display_name
            ),
            feature: resource.to_string(),
            plan: limits.tier.clone(),
            upgrade_to: Some("starter".to_string()),
        }
        .into_response());
    }

    let row: (i64, i64, i64, i64, i64, i64, i64, i64) =
        sqlx::query_as("SELECT * FROM get_org_resource_counts($1)")
            .bind(org_id)
            .fetch_one(&mut *conn)
            .await
            .map_err(|e| {
                tracing::error!(error = %e, "Failed to get resource counts");
                (StatusCode::INTERNAL_SERVER_ERROR, "Internal error").into_response()
            })?;

    let current = match resource {
        "jobs_per_day" => row.7 as u64,
        "active_jobs" => row.0 as u64,
        "queues" => row.1 as u64,
        "workers" => row.2 as u64,
        "api_keys" => row.3 as u64,
        "schedules" => row.4 as u64,
        "workflows" => row.5 as u64,
        "webhooks" => row.6 as u64,
        _ => return Ok(()), // Unknown resource, allow
    };

    limits
        .check_limit(resource, current, adding)
        .map_err(|e| LimitExceededResponse::from(e).into_response())
}

/// Error type for limit check failures (usable by HTTP, gRPC, etc.)
#[derive(Debug, Clone)]
pub enum LimitCheckError {
    /// Database error during limit check
    Database(String),
    /// Job limit exceeded
    LimitExceeded(crate::config::LimitError),
    /// Payload too large for current plan
    PayloadTooLarge {
        current: u64,
        limit: u64,
        plan: String,
        upgrade_to: Option<String>,
    },
}

impl std::fmt::Display for LimitCheckError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LimitCheckError::Database(msg) => write!(f, "Database error: {}", msg),
            LimitCheckError::LimitExceeded(err) => write!(f, "{}", err),
            LimitCheckError::PayloadTooLarge {
                current,
                limit,
                plan,
                upgrade_to,
            } => {
                write!(
                    f,
                    "payload size limit reached ({}/{}) on plan {}",
                    current, limit, plan
                )?;
                if let Some(upgrade) = upgrade_to {
                    write!(f, ". Upgrade to {} for higher limits.", upgrade)?;
                }
                Ok(())
            }
        }
    }
}

impl std::error::Error for LimitCheckError {}

/// Check job creation limits (both daily and active) - generic version
/// Returns LimitCheckError which can be converted to HTTP Response or gRPC Status
pub async fn check_job_limits_generic(
    pool: &PgPool,
    org_id: &str,
    job_count: u64,
) -> Result<(), LimitCheckError> {
    let (plan_tier, custom_limits) = get_org_plan_and_limits(pool, org_id)
        .await
        .map_err(|e| LimitCheckError::Database(e.to_string()))?;

    let limits = PlanLimits::for_tier_with_overrides(&plan_tier, custom_limits.as_ref());
    let counts = get_resource_counts(pool, org_id)
        .await
        .map_err(|e| LimitCheckError::Database(e.to_string()))?;

    // Check daily limit
    limits
        .check_limit("jobs_per_day", counts.jobs_today, job_count)
        .map_err(LimitCheckError::LimitExceeded)?;

    // Check active jobs limit
    limits
        .check_limit("active_jobs", counts.active_jobs, job_count)
        .map_err(LimitCheckError::LimitExceeded)?;

    Ok(())
}

/// Check payload size against plan limits - generic version (usable by HTTP, gRPC, etc.)
pub async fn check_payload_size_generic(
    pool: &PgPool,
    org_id: &str,
    payload_size: usize,
) -> Result<(), LimitCheckError> {
    let (plan_tier, custom_limits) = get_org_plan_and_limits(pool, org_id)
        .await
        .map_err(|e| LimitCheckError::Database(e.to_string()))?;

    let limits = PlanLimits::for_tier_with_overrides(&plan_tier, custom_limits.as_ref());

    if payload_size > limits.max_payload_size_bytes {
        let upgrade_to = match limits.tier.as_str() {
            "free" => Some("starter".to_string()),
            "starter" => Some("pro".to_string()),
            "pro" => Some("enterprise".to_string()),
            _ => None,
        };

        return Err(LimitCheckError::PayloadTooLarge {
            current: payload_size as u64,
            limit: limits.max_payload_size_bytes as u64,
            plan: limits.tier,
            upgrade_to,
        });
    }

    Ok(())
}

/// Check job creation limits (both daily and active) - HTTP version
pub async fn check_job_limits(pool: &PgPool, org_id: &str, job_count: u64) -> Result<(), Response> {
    check_job_limits_generic(pool, org_id, job_count)
        .await
        .map_err(|e| match e {
            LimitCheckError::Database(msg) => {
                tracing::error!(error = %msg, "Failed during limit check");
                (StatusCode::INTERNAL_SERVER_ERROR, "Internal error").into_response()
            }
            LimitCheckError::LimitExceeded(err) => LimitExceededResponse::from(err).into_response(),
            LimitCheckError::PayloadTooLarge { .. } => {
                // Not expected for job count checks; treat as internal mapping error
                (StatusCode::INTERNAL_SERVER_ERROR, "Internal error").into_response()
            }
        })
}

/// Check payload size against plan limits
pub async fn check_payload_size(
    pool: &PgPool,
    org_id: &str,
    payload_size: usize,
) -> Result<(), Response> {
    check_payload_size_generic(pool, org_id, payload_size)
        .await
        .map_err(|e| match e {
            LimitCheckError::Database(msg) => {
                tracing::error!(error = %msg, "Failed to get org plan tier");
                (StatusCode::INTERNAL_SERVER_ERROR, "Internal error").into_response()
            }
            LimitCheckError::LimitExceeded(_) => {
                // Not expected for payload checks; treat as internal mapping error
                (StatusCode::INTERNAL_SERVER_ERROR, "Internal error").into_response()
            }
            LimitCheckError::PayloadTooLarge {
                current,
                limit,
                plan,
                upgrade_to,
            } => (
                // Must carry an explicit status: `Json(..).into_response()` defaults to
                // HTTP 200, so clients received an error body under a success status and
                // treated the rejected job as accepted. 413 is the correct status for a
                // payload that exceeds the (plan) size limit.
                StatusCode::PAYLOAD_TOO_LARGE,
                Json(serde_json::json!({
                    "error": "payload_too_large",
                    "code": "PAYLOAD_TOO_LARGE",
                    "message": format!(
                        "Payload size ({} bytes) exceeds plan limit ({} bytes). Upgrade for larger payloads.",
                        current, limit
                    ),
                    "current": current,
                    "limit": limit,
                    "plan": plan,
                    "upgrade_to": upgrade_to.is_some()
                })),
            )
                .into_response(),
        })
}

/// Increment daily job counter
///
/// Returns the new count
pub async fn increment_daily_jobs(
    pool: &PgPool,
    org_id: &str,
    count: i32,
) -> Result<i64, sqlx::Error> {
    let row: (i64,) = sqlx::query_as("SELECT increment_daily_jobs($1, $2)")
        .bind(org_id)
        .bind(count)
        .fetch_one(pool)
        .await?;

    Ok(row.0)
}

/// The org plan's daily jobs cap as an `i64` (`-1` = unlimited).
pub async fn daily_jobs_limit(pool: &PgPool, org_id: &str) -> Result<i64, sqlx::Error> {
    let (plan_tier, custom_limits) = get_org_plan_and_limits(pool, org_id).await?;
    let limits = PlanLimits::for_tier_with_overrides(&plan_tier, custom_limits.as_ref());
    Ok(limits.max_jobs_per_day.map(|v| v as i64).unwrap_or(-1))
}

/// Atomically enforce **and** increment the daily job counter.
///
/// Returns the new daily count, or `-1` if the increment would exceed `limit`
/// (`limit < 0` = unlimited). This replaces the previous check-then-increment
/// pattern that let concurrent enqueues overshoot the cap.
pub async fn try_increment_daily_jobs(
    pool: &PgPool,
    org_id: &str,
    count: i32,
    limit: i64,
) -> Result<i64, sqlx::Error> {
    let row: (i64,) = sqlx::query_as("SELECT try_increment_daily_jobs($1, $2, $3)")
        .bind(org_id)
        .bind(count)
        .bind(limit)
        .fetch_one(pool)
        .await?;

    Ok(row.0)
}

/// Get organization usage and limits info for display
#[derive(Debug, Serialize)]
pub struct UsageInfo {
    pub plan: String,
    pub plan_display_name: String,
    pub limits: PlanLimits,
    pub usage: ResourceUsage,
    pub warnings: Vec<UsageWarning>,
}

#[derive(Debug, Serialize)]
pub struct ResourceUsage {
    pub jobs_today: UsageItem,
    pub active_jobs: UsageItem,
    pub queues: UsageItem,
    pub workers: UsageItem,
    pub api_keys: UsageItem,
    pub schedules: UsageItem,
    pub workflows: UsageItem,
    pub webhooks: UsageItem,
}

#[derive(Debug, Serialize)]
pub struct UsageItem {
    pub current: u64,
    pub limit: Option<u64>,
    pub percentage: Option<f64>,
    pub is_disabled: bool,
}

#[derive(Debug, Serialize)]
pub struct UsageWarning {
    pub resource: String,
    pub message: String,
    pub severity: String, // "warning" or "critical"
}

/// Get full usage info for an organization
pub async fn get_usage_info(pool: &PgPool, org_id: &str) -> Result<UsageInfo, sqlx::Error> {
    let (plan_tier, custom_limits) = get_org_plan_and_limits(pool, org_id).await?;
    let limits = PlanLimits::for_tier_with_overrides(&plan_tier, custom_limits.as_ref());
    let counts = get_resource_counts(pool, org_id).await?;

    let warning_threshold = limits.warning_threshold();
    let mut warnings = Vec::new();

    // Helper to create usage item and check for warnings
    let mut make_item = |resource: &str, current: u64, limit: Option<u64>| -> UsageItem {
        let percentage = limit.map(|l| (current as f64 / l as f64) * 100.0);
        let is_disabled = limits.is_disabled(resource);

        if let Some(pct) = percentage {
            if pct >= 100.0 {
                warnings.push(UsageWarning {
                    resource: resource.to_string(),
                    message: format!("{} limit reached", resource.replace('_', " ")),
                    severity: "critical".to_string(),
                });
            } else if pct >= warning_threshold {
                warnings.push(UsageWarning {
                    resource: resource.to_string(),
                    message: format!("{} at {:.0}% of limit", resource.replace('_', " "), pct),
                    severity: "warning".to_string(),
                });
            }
        }

        UsageItem {
            current,
            limit,
            percentage,
            is_disabled,
        }
    };

    let usage = ResourceUsage {
        jobs_today: make_item("jobs_per_day", counts.jobs_today, limits.max_jobs_per_day),
        active_jobs: make_item("active_jobs", counts.active_jobs, limits.max_active_jobs),
        queues: make_item("queues", counts.queues, limits.max_queues.map(|v| v as u64)),
        workers: make_item(
            "workers",
            counts.workers,
            limits.max_workers.map(|v| v as u64),
        ),
        api_keys: make_item(
            "api_keys",
            counts.api_keys,
            limits.max_api_keys.map(|v| v as u64),
        ),
        schedules: make_item(
            "schedules",
            counts.schedules,
            limits.max_schedules.map(|v| v as u64),
        ),
        workflows: make_item(
            "workflows",
            counts.workflows,
            limits.max_workflows.map(|v| v as u64),
        ),
        webhooks: make_item(
            "webhooks",
            counts.webhooks,
            limits.max_webhooks.map(|v| v as u64),
        ),
    };

    Ok(UsageInfo {
        plan: limits.tier.clone(),
        plan_display_name: limits.display_name.clone(),
        limits,
        usage,
        warnings,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::LimitError;

    #[test]
    fn test_limit_exceeded_response_from_limit_error() {
        let err = LimitError {
            resource: "active_jobs".to_string(),
            current: 100,
            limit: 50,
            plan: "free".to_string(),
            upgrade_to: Some("starter".to_string()),
        };

        let response: LimitExceededResponse = err.into();
        assert_eq!(response.error, "limit_exceeded");
        assert_eq!(response.resource, "active_jobs");
        assert_eq!(response.current, 100);
        assert_eq!(response.limit, 50);
        assert_eq!(response.plan, "free");
        assert_eq!(response.upgrade_to, Some("starter".to_string()));
    }

    #[test]
    fn test_limit_exceeded_response_serialization() {
        let response = LimitExceededResponse {
            error: "limit_exceeded".to_string(),
            code: "QUOTA_EXCEEDED".to_string(),
            message: "You've reached the limit".to_string(),
            resource: "queues".to_string(),
            current: 10,
            limit: 5,
            plan: "starter".to_string(),
            upgrade_to: Some("pro".to_string()),
        };

        let json = serde_json::to_string(&response).unwrap();
        assert!(json.contains("limit_exceeded"));
        assert!(json.contains("queues"));
        assert!(json.contains("\"current\":10"));
        assert!(json.contains("\"limit\":5"));
        assert!(json.contains("pro"));
    }

    #[test]
    fn test_limit_exceeded_response_without_upgrade() {
        let response = LimitExceededResponse {
            error: "limit_exceeded".to_string(),
            code: "QUOTA_EXCEEDED".to_string(),
            message: "Maximum reached".to_string(),
            resource: "workers".to_string(),
            current: 200,
            limit: 100,
            plan: "enterprise".to_string(),
            upgrade_to: None,
        };

        let json = serde_json::to_string(&response).unwrap();
        assert!(json.contains("enterprise"));
        assert!(json.contains("QUOTA_EXCEEDED"));
        // upgrade_to should be skipped
        assert!(!json.contains("upgrade_to"));
    }

    #[test]
    fn test_feature_disabled_response_serialization() {
        let response = FeatureDisabledResponse {
            error: "feature_disabled".to_string(),
            message: "Workflows are not available on free plan".to_string(),
            feature: "workflows".to_string(),
            plan: "free".to_string(),
            upgrade_to: Some("starter".to_string()),
        };

        let json = serde_json::to_string(&response).unwrap();
        assert!(json.contains("feature_disabled"));
        assert!(json.contains("workflows"));
        assert!(json.contains("free"));
        assert!(json.contains("starter"));
    }

    #[test]
    fn test_resource_counts_default() {
        let counts = ResourceCounts::default();
        assert_eq!(counts.active_jobs, 0);
        assert_eq!(counts.queues, 0);
        assert_eq!(counts.workers, 0);
        assert_eq!(counts.api_keys, 0);
        assert_eq!(counts.schedules, 0);
        assert_eq!(counts.workflows, 0);
        assert_eq!(counts.webhooks, 0);
        assert_eq!(counts.jobs_today, 0);
    }

    #[test]
    fn test_resource_counts_with_values() {
        let counts = ResourceCounts {
            active_jobs: 50,
            queues: 5,
            workers: 10,
            api_keys: 3,
            schedules: 2,
            workflows: 1,
            webhooks: 4,
            jobs_today: 1000,
        };

        assert_eq!(counts.active_jobs, 50);
        assert_eq!(counts.jobs_today, 1000);
    }

    #[test]
    fn test_usage_item_serialization() {
        let item = UsageItem {
            current: 50,
            limit: Some(100),
            percentage: Some(50.0),
            is_disabled: false,
        };

        let json = serde_json::to_string(&item).unwrap();
        assert!(json.contains("\"current\":50"));
        assert!(json.contains("\"limit\":100"));
        assert!(json.contains("\"percentage\":50.0"));
    }

    #[test]
    fn test_usage_item_unlimited() {
        let item = UsageItem {
            current: 500,
            limit: None,
            percentage: None,
            is_disabled: false,
        };

        let json = serde_json::to_string(&item).unwrap();
        assert!(json.contains("\"current\":500"));
    }

    #[test]
    fn test_usage_item_disabled() {
        let item = UsageItem {
            current: 0,
            limit: Some(0),
            percentage: None,
            is_disabled: true,
        };

        let json = serde_json::to_string(&item).unwrap();
        assert!(json.contains("\"is_disabled\":true"));
    }

    #[test]
    fn test_usage_warning_serialization() {
        let warning = UsageWarning {
            resource: "active_jobs".to_string(),
            message: "You're approaching your limit".to_string(),
            severity: "warning".to_string(),
        };

        let json = serde_json::to_string(&warning).unwrap();
        assert!(json.contains("active_jobs"));
        assert!(json.contains("warning"));
        assert!(json.contains("approaching"));
    }
}
