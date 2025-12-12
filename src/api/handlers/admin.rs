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

    let updated: Organization = sqlx::query_as(
        r#"
        UPDATE organizations
        SET plan_tier = $1, billing_email = $2, settings = $3, updated_at = NOW()
        WHERE id = $4
        RETURNING *
        "#,
    )
    .bind(&plan_tier)
    .bind(&billing_email)
    .bind(&settings)
    .bind(&id)
    .fetch_one(state.db.pool())
    .await?;

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
        // Hard delete - cascades to all related data
        sqlx::query("DELETE FROM organizations WHERE id = $1")
            .bind(&id)
            .execute(state.db.pool())
            .await?;

        tracing::warn!(org_id = %id, "Organization hard deleted by admin");
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
