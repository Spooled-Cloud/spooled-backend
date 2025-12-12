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
        (StatusCode::FORBIDDEN, Json(self)).into_response()
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
    let row: (String, Option<serde_json::Value>) = sqlx::query_as(
        "SELECT plan_tier, custom_limits FROM organizations WHERE id = $1",
    )
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

/// Check job creation limits (both daily and active)
pub async fn check_job_limits(pool: &PgPool, org_id: &str, job_count: u64) -> Result<(), Response> {
    let (plan_tier, custom_limits) = get_org_plan_and_limits(pool, org_id).await.map_err(|e| {
        tracing::error!(error = %e, "Failed to get org plan tier");
        (StatusCode::INTERNAL_SERVER_ERROR, "Internal error").into_response()
    })?;

    let limits = PlanLimits::for_tier_with_overrides(&plan_tier, custom_limits.as_ref());
    let counts = get_resource_counts(pool, org_id).await.map_err(|e| {
        tracing::error!(error = %e, "Failed to get resource counts");
        (StatusCode::INTERNAL_SERVER_ERROR, "Internal error").into_response()
    })?;

    // Check daily limit
    limits
        .check_limit("jobs_per_day", counts.jobs_today, job_count)
        .map_err(|e| LimitExceededResponse::from(e).into_response())?;

    // Check active jobs limit
    limits
        .check_limit("active_jobs", counts.active_jobs, job_count)
        .map_err(|e| LimitExceededResponse::from(e).into_response())?;

    Ok(())
}

/// Check payload size against plan limits
pub async fn check_payload_size(
    pool: &PgPool,
    org_id: &str,
    payload_size: usize,
) -> Result<(), Response> {
    let (plan_tier, custom_limits) = get_org_plan_and_limits(pool, org_id).await.map_err(|e| {
        tracing::error!(error = %e, "Failed to get org plan tier");
        (StatusCode::INTERNAL_SERVER_ERROR, "Internal error").into_response()
    })?;

    let limits = PlanLimits::for_tier_with_overrides(&plan_tier, custom_limits.as_ref());

    if payload_size > limits.max_payload_size_bytes {
        return Err(Json(serde_json::json!({
            "error": "payload_too_large",
            "message": format!(
                "Payload size ({} bytes) exceeds plan limit ({} bytes). Upgrade for larger payloads.",
                payload_size, limits.max_payload_size_bytes
            ),
            "current": payload_size,
            "limit": limits.max_payload_size_bytes,
            "plan": limits.tier,
            "upgrade_to": limits.tier != "enterprise"
        }))
        .into_response());
    }

    Ok(())
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
