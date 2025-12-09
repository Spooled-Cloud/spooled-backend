//! Organization handlers

use axum::{
    extract::{Extension, Path, State},
    http::StatusCode,
    Json,
};
use chrono::Utc;
use uuid::Uuid;

use crate::api::middleware::ValidatedJson;
use crate::api::AppState;
use crate::error::{AppError, AppResult};
use crate::models::{
    ApiKeyContext, CreateOrganizationRequest, Organization, OrganizationSummary,
    UpdateOrganizationRequest,
};

/// List organizations (returns only the authenticated user's organization)
///
/// Previously listed ALL organizations (critical data leak)
/// Now only returns the authenticated user's organization
pub async fn list(
    State(state): State<AppState>,
    Extension(ctx): Extension<ApiKeyContext>,
) -> AppResult<Json<Vec<OrganizationSummary>>> {
    // Users can only see their own organization
    let organizations =
        sqlx::query_as::<_, Organization>("SELECT * FROM organizations WHERE id = $1")
            .bind(&ctx.organization_id)
            .fetch_all(state.db.pool())
            .await?;

    let summaries: Vec<OrganizationSummary> = organizations.into_iter().map(Into::into).collect();
    Ok(Json(summaries))
}

/// Validate slug format
fn validate_slug(slug: &str) -> Result<(), AppError> {
    if slug.is_empty() || slug.len() > 100 {
        return Err(AppError::Validation(
            "Slug must be 1-100 characters".to_string(),
        ));
    }
    if !slug
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
    {
        return Err(AppError::Validation(
            "Slug can only contain lowercase letters, digits, and hyphens".to_string(),
        ));
    }
    if slug.starts_with('-') || slug.ends_with('-') {
        return Err(AppError::Validation(
            "Slug cannot start or end with a hyphen".to_string(),
        ));
    }
    Ok(())
}

/// Maximum settings JSON size
const MAX_SETTINGS_SIZE: usize = 64 * 1024; // 64KB

/// Create a new organization
///
/// Now validates slug format
pub async fn create(
    State(state): State<AppState>,
    ValidatedJson(request): ValidatedJson<CreateOrganizationRequest>,
) -> AppResult<(StatusCode, Json<Organization>)> {
    // Validate slug format
    validate_slug(&request.slug)?;

    let id = Uuid::new_v4().to_string();
    let now = Utc::now();

    let org = sqlx::query_as::<_, Organization>(
        r#"
        INSERT INTO organizations (id, name, slug, plan_tier, billing_email, settings, created_at, updated_at)
        VALUES ($1, $2, $3, 'free', $4, '{}', $5, $5)
        RETURNING *
        "#,
    )
    .bind(&id)
    .bind(&request.name)
    .bind(&request.slug)
    .bind(&request.billing_email)
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

    Ok((StatusCode::CREATED, Json(org)))
}

/// Get an organization by ID
///
/// Users can only view their own organization
pub async fn get(
    State(state): State<AppState>,
    Extension(ctx): Extension<ApiKeyContext>,
    Path(id): Path<String>,
) -> AppResult<Json<Organization>> {
    // Verify user belongs to this organization
    if id != ctx.organization_id {
        return Err(AppError::NotFound(format!("Organization {} not found", id)));
    }

    let org = sqlx::query_as::<_, Organization>("SELECT * FROM organizations WHERE id = $1")
        .bind(&id)
        .fetch_optional(state.db.pool())
        .await?
        .ok_or_else(|| AppError::NotFound(format!("Organization {} not found", id)))?;

    Ok(Json(org))
}

/// Update an organization
///
/// Users can only update their own organization
pub async fn update(
    State(state): State<AppState>,
    Extension(ctx): Extension<ApiKeyContext>,
    Path(id): Path<String>,
    ValidatedJson(request): ValidatedJson<UpdateOrganizationRequest>,
) -> AppResult<Json<Organization>> {
    // Verify user belongs to this organization
    if id != ctx.organization_id {
        return Err(AppError::NotFound(format!("Organization {} not found", id)));
    }

    // Check if org exists
    let existing = sqlx::query_as::<_, Organization>("SELECT * FROM organizations WHERE id = $1")
        .bind(&id)
        .fetch_optional(state.db.pool())
        .await?
        .ok_or_else(|| AppError::NotFound(format!("Organization {} not found", id)))?;

    let name = request.name.unwrap_or(existing.name);
    let billing_email = request.billing_email.or(existing.billing_email);

    // Validate settings size to prevent large payloads
    let settings = if let Some(ref new_settings) = request.settings {
        let settings_str = serde_json::to_string(new_settings)
            .map_err(|_| AppError::BadRequest("Invalid settings JSON".to_string()))?;
        if settings_str.len() > MAX_SETTINGS_SIZE {
            return Err(AppError::BadRequest(format!(
                "Settings too large: {} bytes (max: {} bytes)",
                settings_str.len(),
                MAX_SETTINGS_SIZE
            )));
        }
        new_settings.clone()
    } else {
        existing.settings
    };

    let org = sqlx::query_as::<_, Organization>(
        r#"
        UPDATE organizations 
        SET name = $1, billing_email = $2, settings = $3, updated_at = NOW()
        WHERE id = $4
        RETURNING *
        "#,
    )
    .bind(&name)
    .bind(&billing_email)
    .bind(&settings)
    .bind(&id)
    .fetch_one(state.db.pool())
    .await?;

    Ok(Json(org))
}

/// Delete an organization
///
/// Users can only delete their own organization
/// Now checks for active resources before deletion to prevent orphaned data
/// Note: This is a dangerous operation and should have additional safeguards
pub async fn delete(
    State(state): State<AppState>,
    Extension(ctx): Extension<ApiKeyContext>,
    Path(id): Path<String>,
) -> AppResult<StatusCode> {
    // Verify user belongs to this organization
    if id != ctx.organization_id {
        return Err(AppError::NotFound(format!("Organization {} not found", id)));
    }

    // Check for active jobs before deletion
    let (active_jobs,): (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM jobs WHERE organization_id = $1 AND status IN ('pending', 'processing', 'scheduled')"
    )
    .bind(&id)
    .fetch_one(state.db.pool())
    .await?;

    if active_jobs > 0 {
        return Err(AppError::Conflict(format!(
            "Cannot delete organization with {} active jobs. Cancel or complete all jobs first.",
            active_jobs
        )));
    }

    // Check for active workers
    let (active_workers,): (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM workers WHERE organization_id = $1 AND status IN ('healthy', 'degraded', 'draining')"
    )
    .bind(&id)
    .fetch_one(state.db.pool())
    .await?;

    if active_workers > 0 {
        return Err(AppError::Conflict(format!(
            "Cannot delete organization with {} active workers. Deregister all workers first.",
            active_workers
        )));
    }

    // Check for active schedules
    let (active_schedules,): (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM schedules WHERE organization_id = $1 AND is_active = TRUE",
    )
    .bind(&id)
    .fetch_one(state.db.pool())
    .await?;

    if active_schedules > 0 {
        return Err(AppError::Conflict(format!(
            "Cannot delete organization with {} active schedules. Pause or delete schedules first.",
            active_schedules
        )));
    }

    let result = sqlx::query("DELETE FROM organizations WHERE id = $1")
        .bind(&id)
        .execute(state.db.pool())
        .await?;

    if result.rows_affected() == 0 {
        return Err(AppError::NotFound(format!("Organization {} not found", id)));
    }

    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_organization_summary_conversion() {
        let org = Organization {
            id: "test-id".to_string(),
            name: "Test Org".to_string(),
            slug: "test-org".to_string(),
            plan_tier: "free".to_string(),
            billing_email: None,
            settings: serde_json::json!({}),
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };

        let summary: OrganizationSummary = org.into();
        assert_eq!(summary.slug, "test-org");
    }
}
