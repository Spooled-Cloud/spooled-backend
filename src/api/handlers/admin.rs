//! Admin API handlers
//!
//! Platform administration endpoints for managing organizations,
//! viewing usage, and configuring plans.
//!
//! All endpoints require X-Admin-Key header authentication.

use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    Json,
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::api::handlers::billing::reconcile_org_from_subscription_id;
use crate::api::middleware::limits::{
    get_resource_counts, get_usage_info, ResourceCounts, UsageInfo,
};
use crate::api::AppState;
use crate::config::PlanLimits;
use crate::error::{AppError, AppResult};
use crate::models::Organization;

// ============================================================================
// Organizations List
// ============================================================================

/// Query parameters for listing organizations
#[derive(Debug, Deserialize)]
pub struct ListOrgsQuery {
    /// Filter by plan tier
    pub plan_tier: Option<String>,
    /// Search by name or slug
    pub search: Option<String>,
    /// Pagination limit (default: 50, max: 100)
    pub limit: Option<i64>,
    /// Pagination offset
    pub offset: Option<i64>,
    /// Sort by field (default: created_at)
    pub sort_by: Option<String>,
    /// Sort order (asc/desc, default: desc)
    pub sort_order: Option<String>,
}

/// Organization with usage stats for admin view
#[derive(Debug, Serialize)]
pub struct AdminOrganization {
    pub id: String,
    pub name: String,
    pub slug: String,
    pub plan_tier: String,
    pub billing_email: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub usage: AdminUsageStats,
}

/// Summary usage stats for list view
#[derive(Debug, Serialize)]
pub struct AdminUsageStats {
    pub jobs_today: u64,
    pub active_jobs: u64,
    pub queues: u64,
    pub workers: u64,
    pub api_keys: u64,
}

impl From<ResourceCounts> for AdminUsageStats {
    fn from(counts: ResourceCounts) -> Self {
        Self {
            jobs_today: counts.jobs_today,
            active_jobs: counts.active_jobs,
            queues: counts.queues,
            workers: counts.workers,
            api_keys: counts.api_keys,
        }
    }
}

/// Response for listing organizations
#[derive(Debug, Serialize)]
pub struct ListOrgsResponse {
    pub organizations: Vec<AdminOrganization>,
    pub total: i64,
    pub limit: i64,
    pub offset: i64,
}

/// List all organizations with usage stats
///
/// GET /api/v1/admin/organizations
///
/// SECURITY: Uses parameterized queries to prevent SQL injection.
/// Sort fields are validated against an allowlist.
pub async fn list_organizations(
    State(state): State<AppState>,
    Query(query): Query<ListOrgsQuery>,
) -> AppResult<Json<ListOrgsResponse>> {
    let limit = query.limit.unwrap_or(50).clamp(1, 100);
    let offset = query.offset.unwrap_or(0).max(0);

    // Validate sort fields against allowlist to prevent SQL injection
    let sort_by = match query.sort_by.as_deref().unwrap_or("created_at") {
        "name" => "name",
        "slug" => "slug",
        "plan_tier" => "plan_tier",
        "updated_at" => "updated_at",
        _ => "created_at",
    };
    let sort_order = match query
        .sort_order
        .as_deref()
        .unwrap_or("desc")
        .to_lowercase()
        .as_str()
    {
        "asc" => "ASC",
        _ => "DESC",
    };

    // Validate plan_tier against allowlist if provided
    let valid_plans = ["free", "starter", "pro", "enterprise", "deleted"];
    let plan_tier_filter = query.plan_tier.as_ref().and_then(|p| {
        if valid_plans.contains(&p.to_lowercase().as_str()) {
            Some(p.to_lowercase())
        } else {
            tracing::warn!(plan_tier = %p, "Invalid plan_tier filter ignored");
            None
        }
    });

    // Sanitize search query - remove SQL wildcards and limit length
    let search_filter = query
        .search
        .as_ref()
        .map(|s| {
            s.chars()
                .filter(|c| c.is_alphanumeric() || *c == ' ' || *c == '-' || *c == '_')
                .take(100)
                .collect::<String>()
        })
        .filter(|s| !s.is_empty());

    // Use parameterized queries based on filter combination
    // This is more verbose but completely safe from SQL injection
    let (orgs, total): (Vec<Organization>, i64) = match (&plan_tier_filter, &search_filter) {
        (Some(plan), Some(search)) => {
            let search_pattern = format!("%{}%", search);
            let count: (i64,) = sqlx::query_as(
                "SELECT COUNT(*) FROM organizations WHERE plan_tier = $1 AND (name ILIKE $2 OR slug ILIKE $2)"
            )
            .bind(plan)
            .bind(&search_pattern)
            .fetch_one(state.db.pool())
            .await?;

            // Dynamic ORDER BY requires separate queries per sort field
            let orgs = match sort_by {
                "name" if sort_order == "ASC" => {
                    sqlx::query_as::<_, Organization>(
                        "SELECT * FROM organizations WHERE plan_tier = $1 AND (name ILIKE $2 OR slug ILIKE $2) ORDER BY name ASC LIMIT $3 OFFSET $4"
                    )
                    .bind(plan).bind(&search_pattern).bind(limit).bind(offset)
                    .fetch_all(state.db.pool()).await?
                }
                "name" => {
                    sqlx::query_as::<_, Organization>(
                        "SELECT * FROM organizations WHERE plan_tier = $1 AND (name ILIKE $2 OR slug ILIKE $2) ORDER BY name DESC LIMIT $3 OFFSET $4"
                    )
                    .bind(plan).bind(&search_pattern).bind(limit).bind(offset)
                    .fetch_all(state.db.pool()).await?
                }
                "slug" if sort_order == "ASC" => {
                    sqlx::query_as::<_, Organization>(
                        "SELECT * FROM organizations WHERE plan_tier = $1 AND (name ILIKE $2 OR slug ILIKE $2) ORDER BY slug ASC LIMIT $3 OFFSET $4"
                    )
                    .bind(plan).bind(&search_pattern).bind(limit).bind(offset)
                    .fetch_all(state.db.pool()).await?
                }
                "slug" => {
                    sqlx::query_as::<_, Organization>(
                        "SELECT * FROM organizations WHERE plan_tier = $1 AND (name ILIKE $2 OR slug ILIKE $2) ORDER BY slug DESC LIMIT $3 OFFSET $4"
                    )
                    .bind(plan).bind(&search_pattern).bind(limit).bind(offset)
                    .fetch_all(state.db.pool()).await?
                }
                "plan_tier" if sort_order == "ASC" => {
                    sqlx::query_as::<_, Organization>(
                        "SELECT * FROM organizations WHERE plan_tier = $1 AND (name ILIKE $2 OR slug ILIKE $2) ORDER BY plan_tier ASC LIMIT $3 OFFSET $4"
                    )
                    .bind(plan).bind(&search_pattern).bind(limit).bind(offset)
                    .fetch_all(state.db.pool()).await?
                }
                "plan_tier" => {
                    sqlx::query_as::<_, Organization>(
                        "SELECT * FROM organizations WHERE plan_tier = $1 AND (name ILIKE $2 OR slug ILIKE $2) ORDER BY plan_tier DESC LIMIT $3 OFFSET $4"
                    )
                    .bind(plan).bind(&search_pattern).bind(limit).bind(offset)
                    .fetch_all(state.db.pool()).await?
                }
                "updated_at" if sort_order == "ASC" => {
                    sqlx::query_as::<_, Organization>(
                        "SELECT * FROM organizations WHERE plan_tier = $1 AND (name ILIKE $2 OR slug ILIKE $2) ORDER BY updated_at ASC LIMIT $3 OFFSET $4"
                    )
                    .bind(plan).bind(&search_pattern).bind(limit).bind(offset)
                    .fetch_all(state.db.pool()).await?
                }
                "updated_at" => {
                    sqlx::query_as::<_, Organization>(
                        "SELECT * FROM organizations WHERE plan_tier = $1 AND (name ILIKE $2 OR slug ILIKE $2) ORDER BY updated_at DESC LIMIT $3 OFFSET $4"
                    )
                    .bind(plan).bind(&search_pattern).bind(limit).bind(offset)
                    .fetch_all(state.db.pool()).await?
                }
                // created_at (default)
                _ if sort_order == "ASC" => {
                    sqlx::query_as::<_, Organization>(
                        "SELECT * FROM organizations WHERE plan_tier = $1 AND (name ILIKE $2 OR slug ILIKE $2) ORDER BY created_at ASC LIMIT $3 OFFSET $4"
                    )
                    .bind(plan).bind(&search_pattern).bind(limit).bind(offset)
                    .fetch_all(state.db.pool()).await?
                }
                _ => {
                    sqlx::query_as::<_, Organization>(
                        "SELECT * FROM organizations WHERE plan_tier = $1 AND (name ILIKE $2 OR slug ILIKE $2) ORDER BY created_at DESC LIMIT $3 OFFSET $4"
                    )
                    .bind(plan).bind(&search_pattern).bind(limit).bind(offset)
                    .fetch_all(state.db.pool()).await?
                }
            };
            (orgs, count.0)
        }
        (Some(plan), None) => {
            let count: (i64,) =
                sqlx::query_as("SELECT COUNT(*) FROM organizations WHERE plan_tier = $1")
                    .bind(plan)
                    .fetch_one(state.db.pool())
                    .await?;

            let orgs = match sort_by {
                "name" if sort_order == "ASC" => {
                    sqlx::query_as::<_, Organization>("SELECT * FROM organizations WHERE plan_tier = $1 ORDER BY name ASC LIMIT $2 OFFSET $3")
                        .bind(plan).bind(limit).bind(offset).fetch_all(state.db.pool()).await?
                }
                "name" => {
                    sqlx::query_as::<_, Organization>("SELECT * FROM organizations WHERE plan_tier = $1 ORDER BY name DESC LIMIT $2 OFFSET $3")
                        .bind(plan).bind(limit).bind(offset).fetch_all(state.db.pool()).await?
                }
                "created_at" if sort_order == "ASC" => {
                    sqlx::query_as::<_, Organization>("SELECT * FROM organizations WHERE plan_tier = $1 ORDER BY created_at ASC LIMIT $2 OFFSET $3")
                        .bind(plan).bind(limit).bind(offset).fetch_all(state.db.pool()).await?
                }
                _ => {
                    sqlx::query_as::<_, Organization>("SELECT * FROM organizations WHERE plan_tier = $1 ORDER BY created_at DESC LIMIT $2 OFFSET $3")
                        .bind(plan).bind(limit).bind(offset).fetch_all(state.db.pool()).await?
                }
            };
            (orgs, count.0)
        }
        (None, Some(search)) => {
            let search_pattern = format!("%{}%", search);
            let count: (i64,) = sqlx::query_as(
                "SELECT COUNT(*) FROM organizations WHERE name ILIKE $1 OR slug ILIKE $1",
            )
            .bind(&search_pattern)
            .fetch_one(state.db.pool())
            .await?;

            let orgs = match sort_by {
                "name" if sort_order == "ASC" => {
                    sqlx::query_as::<_, Organization>("SELECT * FROM organizations WHERE name ILIKE $1 OR slug ILIKE $1 ORDER BY name ASC LIMIT $2 OFFSET $3")
                        .bind(&search_pattern).bind(limit).bind(offset).fetch_all(state.db.pool()).await?
                }
                "name" => {
                    sqlx::query_as::<_, Organization>("SELECT * FROM organizations WHERE name ILIKE $1 OR slug ILIKE $1 ORDER BY name DESC LIMIT $2 OFFSET $3")
                        .bind(&search_pattern).bind(limit).bind(offset).fetch_all(state.db.pool()).await?
                }
                "created_at" if sort_order == "ASC" => {
                    sqlx::query_as::<_, Organization>("SELECT * FROM organizations WHERE name ILIKE $1 OR slug ILIKE $1 ORDER BY created_at ASC LIMIT $2 OFFSET $3")
                        .bind(&search_pattern).bind(limit).bind(offset).fetch_all(state.db.pool()).await?
                }
                _ => {
                    sqlx::query_as::<_, Organization>("SELECT * FROM organizations WHERE name ILIKE $1 OR slug ILIKE $1 ORDER BY created_at DESC LIMIT $2 OFFSET $3")
                        .bind(&search_pattern).bind(limit).bind(offset).fetch_all(state.db.pool()).await?
                }
            };
            (orgs, count.0)
        }
        (None, None) => {
            let count: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM organizations")
                .fetch_one(state.db.pool())
                .await?;

            let orgs =
                match sort_by {
                    "name" if sort_order == "ASC" => {
                        sqlx::query_as::<_, Organization>(
                            "SELECT * FROM organizations ORDER BY name ASC LIMIT $1 OFFSET $2",
                        )
                        .bind(limit)
                        .bind(offset)
                        .fetch_all(state.db.pool())
                        .await?
                    }
                    "name" => {
                        sqlx::query_as::<_, Organization>(
                            "SELECT * FROM organizations ORDER BY name DESC LIMIT $1 OFFSET $2",
                        )
                        .bind(limit)
                        .bind(offset)
                        .fetch_all(state.db.pool())
                        .await?
                    }
                    "created_at" if sort_order == "ASC" => sqlx::query_as::<_, Organization>(
                        "SELECT * FROM organizations ORDER BY created_at ASC LIMIT $1 OFFSET $2",
                    )
                    .bind(limit)
                    .bind(offset)
                    .fetch_all(state.db.pool())
                    .await?,
                    _ => sqlx::query_as::<_, Organization>(
                        "SELECT * FROM organizations ORDER BY created_at DESC LIMIT $1 OFFSET $2",
                    )
                    .bind(limit)
                    .bind(offset)
                    .fetch_all(state.db.pool())
                    .await?,
                };
            (orgs, count.0)
        }
    };

    // Fetch usage for each org
    let mut admin_orgs = Vec::with_capacity(orgs.len());
    for org in orgs {
        let usage = get_resource_counts(state.db.pool(), &org.id)
            .await
            .unwrap_or_default();

        admin_orgs.push(AdminOrganization {
            id: org.id,
            name: org.name,
            slug: org.slug,
            plan_tier: org.plan_tier,
            billing_email: org.billing_email,
            created_at: org.created_at,
            updated_at: org.updated_at,
            usage: usage.into(),
        });
    }

    Ok(Json(ListOrgsResponse {
        organizations: admin_orgs,
        total,
        limit,
        offset,
    }))
}

// ============================================================================
// Organization Detail
// ============================================================================

/// Full organization detail with usage info
#[derive(Debug, Serialize)]
pub struct AdminOrganizationDetail {
    pub id: String,
    pub name: String,
    pub slug: String,
    pub plan_tier: String,
    pub billing_email: Option<String>,
    pub settings: serde_json::Value,
    pub custom_limits: Option<serde_json::Value>,
    pub stripe_customer_id: Option<String>,
    pub stripe_subscription_id: Option<String>,
    pub stripe_subscription_status: Option<String>,
    pub stripe_current_period_end: Option<DateTime<Utc>>,
    pub stripe_cancel_at_period_end: Option<bool>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub usage_info: UsageInfo,
    pub api_keys_count: i64,
    pub total_jobs: i64,
}

/// Get organization details with full usage info
///
/// GET /api/v1/admin/organizations/:id
pub async fn get_organization(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> AppResult<Json<AdminOrganizationDetail>> {
    let org: Organization = sqlx::query_as("SELECT * FROM organizations WHERE id = $1")
        .bind(&id)
        .fetch_optional(state.db.pool())
        .await?
        .ok_or_else(|| AppError::NotFound(format!("Organization {} not found", id)))?;

    let usage_info = get_usage_info(state.db.pool(), &id).await?;

    let api_keys_count: (i64,) =
        sqlx::query_as("SELECT COUNT(*) FROM api_keys WHERE organization_id = $1")
            .bind(&id)
            .fetch_one(state.db.pool())
            .await?;

    let total_jobs: (i64,) = sqlx::query_as(
        "SELECT COALESCE(total_jobs_created, 0) FROM organization_usage WHERE organization_id = $1",
    )
    .bind(&id)
    .fetch_optional(state.db.pool())
    .await?
    .unwrap_or((0,));

    Ok(Json(AdminOrganizationDetail {
        id: org.id,
        name: org.name,
        slug: org.slug,
        plan_tier: org.plan_tier,
        billing_email: org.billing_email,
        settings: org.settings,
        custom_limits: org.custom_limits,
        stripe_customer_id: org.stripe_customer_id,
        stripe_subscription_id: org.stripe_subscription_id,
        stripe_subscription_status: org.stripe_subscription_status,
        stripe_current_period_end: org.stripe_current_period_end,
        stripe_cancel_at_period_end: org.stripe_cancel_at_period_end,
        created_at: org.created_at,
        updated_at: org.updated_at,
        usage_info,
        api_keys_count: api_keys_count.0,
        total_jobs: total_jobs.0,
    }))
}

// ============================================================================
// Update Organization
// ============================================================================

/// Request to update organization (admin)
#[derive(Debug, Deserialize)]
pub struct UpdateOrgRequest {
    /// New plan tier
    pub plan_tier: Option<String>,
    /// New billing email
    pub billing_email: Option<String>,
    /// New settings (merged with existing)
    pub settings: Option<serde_json::Value>,
    /// Custom limit overrides (null = use plan defaults, empty object = reset to defaults)
    pub custom_limits: Option<serde_json::Value>,
    /// Stripe customer ID (omit to keep current; set to null to clear)
    pub stripe_customer_id: Option<Option<String>>,
    /// Stripe subscription ID (omit to keep current; set to null to clear)
    pub stripe_subscription_id: Option<Option<String>>,
}

/// Update organization plan or settings
///
/// PATCH /api/v1/admin/organizations/:id
pub async fn update_organization(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(request): Json<UpdateOrgRequest>,
) -> AppResult<Json<Organization>> {
    // Verify org exists
    let existing: Organization = sqlx::query_as("SELECT * FROM organizations WHERE id = $1")
        .bind(&id)
        .fetch_optional(state.db.pool())
        .await?
        .ok_or_else(|| AppError::NotFound(format!("Organization {} not found", id)))?;

    // Validate plan tier if provided
    if let Some(ref tier) = request.plan_tier {
        let valid_tiers = ["free", "starter", "pro", "enterprise"];
        if !valid_tiers.contains(&tier.to_lowercase().as_str()) {
            return Err(AppError::Validation(format!(
                "Invalid plan tier: {}. Must be one of: {}",
                tier,
                valid_tiers.join(", ")
            )));
        }
    }

    let old_plan = existing.plan_tier.clone();
    let plan_tier = request.plan_tier.unwrap_or(existing.plan_tier);
    let billing_email = request.billing_email.or(existing.billing_email);
    let settings = request.settings.unwrap_or(existing.settings);
    let stripe_customer_id = request
        .stripe_customer_id
        .unwrap_or(existing.stripe_customer_id);
    let stripe_subscription_id = request
        .stripe_subscription_id
        .unwrap_or(existing.stripe_subscription_id);
    // For custom_limits: None means keep existing, Some(null) or empty object means reset to defaults
    let custom_limits = match &request.custom_limits {
        Some(limits) if limits.is_null() || limits == &serde_json::json!({}) => None,
        Some(limits) => Some(limits.clone()),
        None => existing.custom_limits,
    };

    let updated: Organization = sqlx::query_as(
        r#"
        UPDATE organizations
        SET
            plan_tier = $1,
            billing_email = $2,
            settings = $3,
            custom_limits = $4,
            stripe_customer_id = $5,
            stripe_subscription_id = $6,
            updated_at = NOW()
        WHERE id = $7
        RETURNING *
        "#,
    )
    .bind(&plan_tier)
    .bind(&billing_email)
    .bind(&settings)
    .bind(&custom_limits)
    .bind(&stripe_customer_id)
    .bind(&stripe_subscription_id)
    .bind(&id)
    .fetch_one(state.db.pool())
    .await?;

    // Best-effort: if admin sets a subscription id, reconcile immediately from Stripe so the org
    // reflects the correct plan_tier/status even if Stripe webhooks were missed (e.g. DB reset).
    if let Some(ref sub_id) = stripe_subscription_id {
        if state.stripe.secret_key.as_ref().is_some() {
            if let Err(e) = reconcile_org_from_subscription_id(&state, &id, sub_id).await {
                tracing::warn!(
                    org_id = %id,
                    subscription_id = %sub_id,
                    error = %e,
                    "Failed to reconcile org billing after admin update"
                );
            }
        }
    }

    tracing::info!(
        org_id = %id,
        old_plan = %old_plan,
        new_plan = %plan_tier,
        "Organization updated by admin"
    );

    Ok(Json(updated))
}

// ============================================================================
// Delete Organization
// ============================================================================

/// Delete an organization (soft delete by marking inactive, or hard delete)
///
/// DELETE /api/v1/admin/organizations/:id
pub async fn delete_organization(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(query): Query<DeleteOrgQuery>,
) -> AppResult<StatusCode> {
    // Check org exists
    let existing: Option<Organization> =
        sqlx::query_as("SELECT * FROM organizations WHERE id = $1")
            .bind(&id)
            .fetch_optional(state.db.pool())
            .await?;

    if existing.is_none() {
        return Err(AppError::NotFound(format!("Organization {} not found", id)));
    }

    if query.hard_delete.unwrap_or(false) {
        // Hard delete - manually delete related data first (some tables lack ON DELETE CASCADE)
        // Order matters due to foreign key constraints
        // Note: job_dependencies has ON DELETE CASCADE from jobs, so no need to delete explicitly

        // 1. Delete jobs (job_dependencies will cascade automatically)
        sqlx::query("DELETE FROM jobs WHERE organization_id = $1")
            .bind(&id)
            .execute(state.db.pool())
            .await?;

        // 3. Delete workflows (references organizations)
        sqlx::query("DELETE FROM workflows WHERE organization_id = $1")
            .bind(&id)
            .execute(state.db.pool())
            .await?;

        // 4. Delete other related tables (these have CASCADE but be explicit)
        sqlx::query("DELETE FROM workers WHERE organization_id = $1")
            .bind(&id)
            .execute(state.db.pool())
            .await?;

        sqlx::query("DELETE FROM schedules WHERE organization_id = $1")
            .bind(&id)
            .execute(state.db.pool())
            .await?;

        sqlx::query("DELETE FROM api_keys WHERE organization_id = $1")
            .bind(&id)
            .execute(state.db.pool())
            .await?;

        sqlx::query("DELETE FROM outgoing_webhooks WHERE organization_id = $1")
            .bind(&id)
            .execute(state.db.pool())
            .await?;

        sqlx::query("DELETE FROM queue_configs WHERE organization_id = $1")
            .bind(&id)
            .execute(state.db.pool())
            .await?;

        // 5. Finally delete the organization
        sqlx::query("DELETE FROM organizations WHERE id = $1")
            .bind(&id)
            .execute(state.db.pool())
            .await?;

        tracing::warn!(org_id = %id, "Organization hard deleted by admin (all related data removed)");
    } else {
        // Soft delete - just mark plan as "deleted" (could add a status column)
        sqlx::query(
            "UPDATE organizations SET plan_tier = 'deleted', updated_at = NOW() WHERE id = $1",
        )
        .bind(&id)
        .execute(state.db.pool())
        .await?;

        tracing::info!(org_id = %id, "Organization soft deleted by admin");
    }

    Ok(StatusCode::NO_CONTENT)
}

#[derive(Debug, Deserialize)]
pub struct DeleteOrgQuery {
    /// If true, permanently delete including all data
    pub hard_delete: Option<bool>,
}

// ============================================================================
// Platform Stats
// ============================================================================

/// Platform-wide statistics
#[derive(Debug, Serialize)]
pub struct PlatformStats {
    pub organizations: OrgStats,
    pub jobs: JobStats,
    pub workers: WorkerStats,
    pub system: SystemStats,
}

#[derive(Debug, Serialize)]
pub struct OrgStats {
    pub total: i64,
    pub by_plan: Vec<PlanCount>,
    pub created_today: i64,
    pub created_this_week: i64,
}

#[derive(Debug, Serialize)]
pub struct PlanCount {
    pub plan: String,
    pub count: i64,
}

#[derive(Debug, Serialize)]
pub struct JobStats {
    pub total_active: i64,
    pub pending: i64,
    pub processing: i64,
    pub completed_24h: i64,
    pub failed_24h: i64,
}

#[derive(Debug, Serialize)]
pub struct WorkerStats {
    pub total: i64,
    pub healthy: i64,
    pub degraded: i64,
}

#[derive(Debug, Serialize)]
pub struct SystemStats {
    pub api_version: String,
    pub uptime_seconds: u64,
}

/// Get platform-wide statistics
///
/// GET /api/v1/admin/stats
pub async fn get_platform_stats(State(state): State<AppState>) -> AppResult<Json<PlatformStats>> {
    // Organization stats
    let total_orgs: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM organizations")
        .fetch_one(state.db.pool())
        .await?;

    let orgs_by_plan: Vec<(String, i64)> = sqlx::query_as(
        "SELECT plan_tier, COUNT(*) FROM organizations GROUP BY plan_tier ORDER BY COUNT(*) DESC",
    )
    .fetch_all(state.db.pool())
    .await?;

    let orgs_today: (i64,) =
        sqlx::query_as("SELECT COUNT(*) FROM organizations WHERE created_at > CURRENT_DATE")
            .fetch_one(state.db.pool())
            .await?;

    let orgs_week: (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM organizations WHERE created_at > CURRENT_DATE - INTERVAL '7 days'",
    )
    .fetch_one(state.db.pool())
    .await?;

    // Job stats
    let job_counts: (i64, i64, i64, i64, i64) = sqlx::query_as(
        r#"
        SELECT
            COUNT(*) FILTER (WHERE status IN ('pending', 'processing', 'scheduled')),
            COUNT(*) FILTER (WHERE status = 'pending'),
            COUNT(*) FILTER (WHERE status = 'processing'),
            COUNT(*) FILTER (WHERE status = 'completed' AND completed_at > NOW() - INTERVAL '24 hours'),
            COUNT(*) FILTER (WHERE status IN ('failed', 'deadletter') AND updated_at > NOW() - INTERVAL '24 hours')
        FROM jobs
        "#
    )
    .fetch_one(state.db.pool())
    .await?;

    // Worker stats
    let worker_counts: (i64, i64, i64) = sqlx::query_as(
        r#"
        SELECT
            COUNT(*),
            COUNT(*) FILTER (WHERE status = 'healthy'),
            COUNT(*) FILTER (WHERE status = 'degraded')
        FROM workers
        WHERE last_heartbeat > NOW() - INTERVAL '1 minute'
        "#,
    )
    .fetch_one(state.db.pool())
    .await?;

    Ok(Json(PlatformStats {
        organizations: OrgStats {
            total: total_orgs.0,
            by_plan: orgs_by_plan
                .into_iter()
                .map(|(plan, count)| PlanCount { plan, count })
                .collect(),
            created_today: orgs_today.0,
            created_this_week: orgs_week.0,
        },
        jobs: JobStats {
            total_active: job_counts.0,
            pending: job_counts.1,
            processing: job_counts.2,
            completed_24h: job_counts.3,
            failed_24h: job_counts.4,
        },
        workers: WorkerStats {
            total: worker_counts.0,
            healthy: worker_counts.1,
            degraded: worker_counts.2,
        },
        system: SystemStats {
            api_version: "v1".to_string(),
            uptime_seconds: 0, // Would need to track server start time
        },
    }))
}

// ============================================================================
// Plans
// ============================================================================

/// List all available plan tiers with their limits
///
/// GET /api/v1/admin/plans
pub async fn list_plans() -> Json<Vec<PlanLimits>> {
    Json(PlanLimits::all_tiers())
}

// ============================================================================
// Admin Organization Creation
// ============================================================================

/// Request to create an organization as admin
#[derive(Debug, Deserialize)]
pub struct AdminCreateOrgRequest {
    /// Organization name
    pub name: String,
    /// URL-friendly slug
    pub slug: String,
    /// Optional billing email
    pub billing_email: Option<String>,
    /// Optional plan tier (defaults to "free")
    pub plan_tier: Option<String>,
}

/// Response for admin organization creation
#[derive(Debug, Serialize)]
pub struct AdminCreateOrgResponse {
    /// The created organization
    pub organization: Organization,
    /// Initial API key (only shown once!)
    pub api_key: AdminApiKeyResponse,
}

/// API key info in admin response
#[derive(Debug, Serialize)]
pub struct AdminApiKeyResponse {
    pub id: String,
    /// The raw API key - SAVE THIS, it's only shown once!
    pub key: String,
    pub name: String,
    pub created_at: chrono::DateTime<Utc>,
}

/// Create a new organization (admin-only)
///
/// POST /api/v1/admin/organizations
///
/// Unlike the public registration endpoint, admin can:
/// - Create organizations regardless of registration mode
/// - Set the initial plan tier
pub async fn create_organization(
    State(state): State<AppState>,
    Json(request): Json<AdminCreateOrgRequest>,
) -> AppResult<(StatusCode, Json<AdminCreateOrgResponse>)> {
    use uuid::Uuid;

    // Validate name
    if request.name.len() < 3 || request.name.len() > 100 {
        return Err(AppError::Validation(
            "Name must be 3-100 characters".to_string(),
        ));
    }

    // Validate slug
    if request.slug.len() < 3 || request.slug.len() > 50 {
        return Err(AppError::Validation(
            "Slug must be 3-50 characters".to_string(),
        ));
    }
    if !request
        .slug
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
    {
        return Err(AppError::Validation(
            "Slug can only contain lowercase letters, digits, and hyphens".to_string(),
        ));
    }
    if request.slug.starts_with('-') || request.slug.ends_with('-') {
        return Err(AppError::Validation(
            "Slug cannot start or end with a hyphen".to_string(),
        ));
    }

    // Validate plan tier if provided
    let plan_tier = match &request.plan_tier {
        Some(tier) => {
            let valid_plans = ["free", "starter", "pro", "enterprise"];
            if !valid_plans.contains(&tier.to_lowercase().as_str()) {
                return Err(AppError::Validation(format!(
                    "Invalid plan tier: {}. Valid options: {:?}",
                    tier, valid_plans
                )));
            }
            tier.to_lowercase()
        }
        None => "free".to_string(),
    };

    let org_id = Uuid::new_v4().to_string();
    let now = Utc::now();

    // Generate a secure webhook token for incoming webhooks
    let webhook_token = generate_webhook_token();
    let initial_settings = serde_json::json!({
        "webhook_token": webhook_token
    });

    // Create organization with specified plan tier
    let org = sqlx::query_as::<_, Organization>(
        r#"
        INSERT INTO organizations (id, name, slug, plan_tier, billing_email, settings, created_at, updated_at)
        VALUES ($1, $2, $3, $4, $5, $6, $7, $7)
        RETURNING *
        "#,
    )
    .bind(&org_id)
    .bind(&request.name)
    .bind(&request.slug)
    .bind(&plan_tier)
    .bind(&request.billing_email)
    .bind(&initial_settings)
    .bind(now)
    .fetch_one(state.db.pool())
    .await
    .map_err(|e| {
        if e.to_string().contains("duplicate key") {
            AppError::Conflict("Organization with this slug already exists".to_string())
        } else {
            AppError::Database(e)
        }
    })?;

    // Create initial API key
    let api_key_id = Uuid::new_v4().to_string();
    let raw_key = format!(
        "sk_{}_{}",
        if state.settings.server.environment == crate::config::Environment::Production {
            "live"
        } else {
            "test"
        },
        generate_secure_key()
    );
    let key_prefix: String = raw_key.chars().take(8).collect();
    let key_hash = bcrypt::hash(&raw_key, bcrypt::DEFAULT_COST)
        .map_err(|e| AppError::Internal(format!("Failed to hash API key: {}", e)))?;

    let key_name = "Initial Admin Key".to_string();
    let queues: Vec<String> = vec!["*".to_string()];

    sqlx::query(
        r#"
        INSERT INTO api_keys (
            id, organization_id, key_hash, key_prefix, name, queues, rate_limit,
            is_active, created_at, expires_at
        )
        VALUES ($1, $2, $3, $4, $5, $6, NULL, TRUE, $7, NULL)
        "#,
    )
    .bind(&api_key_id)
    .bind(&org_id)
    .bind(&key_hash)
    .bind(&key_prefix)
    .bind(&key_name)
    .bind(&queues)
    .bind(now)
    .execute(state.db.pool())
    .await?;

    tracing::info!(
        org_id = %org_id,
        org_name = %request.name,
        plan_tier = %plan_tier,
        "Admin created organization"
    );

    Ok((
        StatusCode::CREATED,
        Json(AdminCreateOrgResponse {
            organization: org,
            api_key: AdminApiKeyResponse {
                id: api_key_id,
                key: raw_key,
                name: key_name,
                created_at: now,
            },
        }),
    ))
}

// ============================================================================
// Admin API Key Management
// ============================================================================

/// Request to create an API key for an organization
#[derive(Debug, Deserialize)]
pub struct AdminCreateApiKeyRequest {
    /// Key name
    pub name: String,
    /// Queue access patterns (["*"] for all)
    pub queues: Option<Vec<String>>,
}

/// Create an API key for an organization (admin-only)
///
/// POST /api/v1/admin/organizations/{id}/api-keys
pub async fn create_api_key(
    State(state): State<AppState>,
    Path(org_id): Path<String>,
    Json(request): Json<AdminCreateApiKeyRequest>,
) -> AppResult<(StatusCode, Json<AdminApiKeyResponse>)> {
    use uuid::Uuid;

    // Verify organization exists
    let org: Option<Organization> = sqlx::query_as("SELECT * FROM organizations WHERE id = $1")
        .bind(&org_id)
        .fetch_optional(state.db.pool())
        .await?;

    let org = org.ok_or_else(|| AppError::NotFound("Organization not found".to_string()))?;

    // Validate name
    if request.name.is_empty() || request.name.len() > 100 {
        return Err(AppError::Validation(
            "Key name must be 1-100 characters".to_string(),
        ));
    }

    let now = Utc::now();
    let api_key_id = Uuid::new_v4().to_string();
    let raw_key = format!(
        "sk_{}_{}",
        if state.settings.server.environment == crate::config::Environment::Production {
            "live"
        } else {
            "test"
        },
        generate_secure_key()
    );
    let key_prefix: String = raw_key.chars().take(8).collect();
    let key_hash = bcrypt::hash(&raw_key, bcrypt::DEFAULT_COST)
        .map_err(|e| AppError::Internal(format!("Failed to hash API key: {}", e)))?;

    let queues = request.queues.unwrap_or_else(|| vec!["*".to_string()]);

    sqlx::query(
        r#"
        INSERT INTO api_keys (
            id, organization_id, key_hash, key_prefix, name, queues, rate_limit,
            is_active, created_at, expires_at
        )
        VALUES ($1, $2, $3, $4, $5, $6, NULL, TRUE, $7, NULL)
        "#,
    )
    .bind(&api_key_id)
    .bind(&org_id)
    .bind(&key_hash)
    .bind(&key_prefix)
    .bind(&request.name)
    .bind(&queues)
    .bind(now)
    .execute(state.db.pool())
    .await?;

    tracing::info!(
        org_id = %org_id,
        org_name = %org.name,
        key_id = %api_key_id,
        key_name = %request.name,
        "Admin created API key for organization"
    );

    Ok((
        StatusCode::CREATED,
        Json(AdminApiKeyResponse {
            id: api_key_id,
            key: raw_key,
            name: request.name,
            created_at: now,
        }),
    ))
}

// ============================================================================
// Admin Usage Management
// ============================================================================

/// Reset organization usage counters
///
/// POST /api/v1/admin/organizations/{id}/reset-usage
///
/// Resets daily job counter to 0. Useful for support scenarios.
pub async fn reset_usage(
    State(state): State<AppState>,
    Path(org_id): Path<String>,
) -> AppResult<StatusCode> {
    // Verify organization exists
    let org: Option<Organization> = sqlx::query_as("SELECT * FROM organizations WHERE id = $1")
        .bind(&org_id)
        .fetch_optional(state.db.pool())
        .await?;

    let org = org.ok_or_else(|| AppError::NotFound("Organization not found".to_string()))?;

    // Reset usage counters - reset daily counter and last_daily_reset to today
    // so the counter effectively becomes 0 for today
    sqlx::query(
        r#"
        UPDATE organization_usage
        SET jobs_created_today = 0,
            last_daily_reset = CURRENT_DATE,
            updated_at = CURRENT_TIMESTAMP
        WHERE organization_id = $1
        "#,
    )
    .bind(&org_id)
    .execute(state.db.pool())
    .await?;

    tracing::info!(
        org_id = %org_id,
        org_name = %org.name,
        "Admin reset usage counters for organization"
    );

    Ok(StatusCode::NO_CONTENT)
}

/// Generate a secure random API key
fn generate_secure_key() -> String {
    use rand::Rng;
    let mut rng = rand::rng();
    let mut bytes = [0u8; 32];
    rng.fill(&mut bytes);
    base64::Engine::encode(&base64::engine::general_purpose::URL_SAFE_NO_PAD, bytes)
}

/// Generate a secure webhook token for incoming webhooks
fn generate_webhook_token() -> String {
    use rand::Rng;
    let mut rng = rand::rng();
    let mut bytes = [0u8; 24]; // 24 bytes = 32 base64 chars
    rng.fill(&mut bytes);
    format!(
        "whk_{}",
        base64::Engine::encode(&base64::engine::general_purpose::URL_SAFE_NO_PAD, bytes)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_admin_usage_stats_from_resource_counts() {
        let counts = ResourceCounts {
            jobs_today: 100,
            active_jobs: 25,
            queues: 5,
            workers: 10,
            api_keys: 3,
            schedules: 2,
            workflows: 1,
            webhooks: 4,
        };

        let stats: AdminUsageStats = counts.into();
        assert_eq!(stats.jobs_today, 100);
        assert_eq!(stats.active_jobs, 25);
        assert_eq!(stats.queues, 5);
        assert_eq!(stats.workers, 10);
        assert_eq!(stats.api_keys, 3);
    }

    #[test]
    fn test_admin_organization_serialization() {
        let org = AdminOrganization {
            id: "org-123".to_string(),
            name: "Test Organization".to_string(),
            slug: "test-org".to_string(),
            plan_tier: "starter".to_string(),
            billing_email: Some("billing@test.com".to_string()),
            created_at: Utc::now(),
            updated_at: Utc::now(),
            usage: AdminUsageStats {
                jobs_today: 50,
                active_jobs: 10,
                queues: 3,
                workers: 5,
                api_keys: 2,
            },
        };

        let json = serde_json::to_string(&org).unwrap();
        assert!(json.contains("org-123"));
        assert!(json.contains("Test Organization"));
        assert!(json.contains("starter"));
        assert!(json.contains("billing@test.com"));
    }

    #[test]
    fn test_admin_organization_without_billing_email() {
        let org = AdminOrganization {
            id: "org-456".to_string(),
            name: "No Email Org".to_string(),
            slug: "no-email".to_string(),
            plan_tier: "free".to_string(),
            billing_email: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
            usage: AdminUsageStats {
                jobs_today: 0,
                active_jobs: 0,
                queues: 0,
                workers: 0,
                api_keys: 1,
            },
        };

        let json = serde_json::to_string(&org).unwrap();
        assert!(json.contains("no-email"));
        assert!(json.contains("free"));
    }

    #[test]
    fn test_list_orgs_query_defaults() {
        let query: ListOrgsQuery = serde_json::from_str("{}").unwrap();
        assert!(query.plan_tier.is_none());
        assert!(query.search.is_none());
        assert!(query.limit.is_none());
        assert!(query.offset.is_none());
        assert!(query.sort_by.is_none());
        assert!(query.sort_order.is_none());
    }

    #[test]
    fn test_list_orgs_query_with_values() {
        let json = r#"{
            "plan_tier": "pro",
            "search": "test",
            "limit": 25,
            "offset": 50,
            "sort_by": "name",
            "sort_order": "asc"
        }"#;
        let query: ListOrgsQuery = serde_json::from_str(json).unwrap();
        assert_eq!(query.plan_tier, Some("pro".to_string()));
        assert_eq!(query.search, Some("test".to_string()));
        assert_eq!(query.limit, Some(25));
        assert_eq!(query.offset, Some(50));
        assert_eq!(query.sort_by, Some("name".to_string()));
        assert_eq!(query.sort_order, Some("asc".to_string()));
    }

    #[test]
    fn test_list_orgs_response_serialization() {
        let response = ListOrgsResponse {
            organizations: vec![],
            total: 0,
            limit: 50,
            offset: 0,
        };

        let json = serde_json::to_string(&response).unwrap();
        assert!(json.contains("\"total\":0"));
        assert!(json.contains("\"limit\":50"));
        assert!(json.contains("\"offset\":0"));
    }

    #[test]
    fn test_generate_secure_key_length() {
        let key = generate_secure_key();
        // 32 bytes base64 encoded (URL safe, no padding) = 43 characters
        assert_eq!(key.len(), 43);
    }

    #[test]
    fn test_generate_secure_key_uniqueness() {
        let key1 = generate_secure_key();
        let key2 = generate_secure_key();
        assert_ne!(key1, key2);
    }

    #[test]
    fn test_generate_secure_key_characters() {
        let key = generate_secure_key();
        // URL-safe base64 only contains alphanumeric, -, and _
        assert!(key
            .chars()
            .all(|c| c.is_alphanumeric() || c == '-' || c == '_'));
    }

    #[test]
    fn test_admin_create_org_request_deserialization() {
        let json = r#"{
            "name": "New Org",
            "slug": "new-org",
            "billing_email": "billing@neworg.com",
            "plan_tier": "starter"
        }"#;
        let request: AdminCreateOrgRequest = serde_json::from_str(json).unwrap();
        assert_eq!(request.name, "New Org");
        assert_eq!(request.slug, "new-org");
        assert_eq!(
            request.billing_email,
            Some("billing@neworg.com".to_string())
        );
        assert_eq!(request.plan_tier, Some("starter".to_string()));
    }

    #[test]
    fn test_admin_create_org_request_defaults() {
        let json = r#"{
            "name": "Minimal Org",
            "slug": "minimal-org"
        }"#;
        let request: AdminCreateOrgRequest = serde_json::from_str(json).unwrap();
        assert_eq!(request.plan_tier, None);
        assert_eq!(request.billing_email, None);
    }

    #[test]
    fn test_admin_create_api_key_request_deserialization() {
        let json = r#"{
            "name": "Test Key",
            "queues": ["default", "emails"]
        }"#;
        let request: AdminCreateApiKeyRequest = serde_json::from_str(json).unwrap();
        assert_eq!(request.name, "Test Key");
        assert_eq!(
            request.queues,
            Some(vec!["default".to_string(), "emails".to_string()])
        );
    }

    #[test]
    fn test_admin_api_key_response_serialization() {
        let response = AdminApiKeyResponse {
            id: "key-123".to_string(),
            name: "My API Key".to_string(),
            key: "sp_test_abc123xyz".to_string(),
            created_at: Utc::now(),
        };

        let json = serde_json::to_string(&response).unwrap();
        assert!(json.contains("key-123"));
        assert!(json.contains("sp_test_abc123xyz"));
        assert!(json.contains("My API Key"));
    }
}
