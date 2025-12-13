//! Organization handlers

use axum::{
    extract::{Extension, Path, State},
    http::StatusCode,
    Json,
};
use chrono::{DateTime, Utc};
use serde::Serialize;
use uuid::Uuid;

use crate::api::middleware::ValidatedJson;
use crate::api::AppState;
use crate::error::{AppError, AppResult};
use crate::models::{
    ApiKeyContext, CreateOrganizationRequest, Organization, OrganizationSummary,
    UpdateOrganizationRequest,
};

/// Organization member representation (dashboard UI)
///
/// Note: Spooled does not currently have "user accounts" stored in the DB.
/// The dashboard "members" view is backed by active API keys that have access
/// to the organization.
#[derive(Debug, Serialize)]
pub struct OrganizationMember {
    pub id: String,
    pub user_id: String,
    pub email: String,
    pub name: String,
    pub role: String,
    pub joined_at: DateTime<Utc>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub invited_by: Option<String>,
}

/// Response for organization creation - includes initial API key
#[derive(Debug, Serialize)]
pub struct CreateOrganizationResponse {
    /// The created organization
    pub organization: Organization,
    /// Initial API key (only shown once!)
    pub api_key: InitialApiKey,
}

/// Initial API key returned during org creation
#[derive(Debug, Serialize)]
pub struct InitialApiKey {
    /// API key ID
    pub id: String,
    /// The raw API key - SAVE THIS, it's only shown once!
    pub key: String,
    /// Key name
    pub name: String,
    /// When the key was created
    pub created_at: DateTime<Utc>,
}

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
/// Registration mode controls access:
/// - "open": Anyone can create organizations (self-hosted default)
/// - "closed": Requires X-Admin-Key header matching ADMIN_API_KEY env var
/// - "invite": Requires valid invite code (future)
///
/// Returns both the organization and an initial API key for immediate access.
pub async fn create(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
    ValidatedJson(request): ValidatedJson<CreateOrganizationRequest>,
) -> AppResult<(StatusCode, Json<CreateOrganizationResponse>)> {
    use crate::config::RegistrationMode;

    // Check registration mode
    match state.settings.registration.mode {
        RegistrationMode::Open => {
            // Anyone can create organizations
        }
        RegistrationMode::Closed => {
            // Require admin API key
            let admin_key = headers.get("X-Admin-Key").and_then(|v| v.to_str().ok());

            match (&state.settings.registration.admin_api_key, admin_key) {
                (Some(expected), Some(provided)) if expected == provided => {
                    // Valid admin key
                }
                (Some(_), _) => {
                    return Err(AppError::Authorization(
                        "Organization creation is disabled. Contact admin for access.".to_string(),
                    ));
                }
                (None, _) => {
                    return Err(AppError::Authorization(
                        "Organization creation is disabled and no admin key is configured."
                            .to_string(),
                    ));
                }
            }
        }
        RegistrationMode::Invite => {
            // Future: Check invite code in request
            return Err(AppError::Authorization(
                "Invite-based registration is not yet implemented.".to_string(),
            ));
        }
    }

    // Validate slug format
    validate_slug(&request.slug)?;

    let org_id = Uuid::new_v4().to_string();
    let now = Utc::now();

    // Create organization
    let org = sqlx::query_as::<_, Organization>(
        r#"
        INSERT INTO organizations (id, name, slug, plan_tier, billing_email, settings, created_at, updated_at)
        VALUES ($1, $2, $3, 'free', $4, '{}', $5, $5)
        RETURNING *
        "#,
    )
    .bind(&org_id)
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

    // Create initial API key for the organization
    let api_key_id = Uuid::new_v4().to_string();
    let raw_key = format!(
        "sk_{}_{}",
        if state.settings.server.environment == crate::config::Environment::Production {
            "live"
        } else {
            "test"
        },
        generate_api_key()
    );
    let key_prefix: String = raw_key.chars().take(8).collect();
    let key_hash = bcrypt::hash(&raw_key, bcrypt::DEFAULT_COST)
        .map_err(|e| AppError::Internal(format!("Failed to hash API key: {}", e)))?;

    let key_name = "Initial Admin Key".to_string();
    let queues: Vec<String> = vec!["*".to_string()]; // Full access to all queues

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

    Ok((
        StatusCode::CREATED,
        Json(CreateOrganizationResponse {
            organization: org,
            api_key: InitialApiKey {
                id: api_key_id,
                key: raw_key, // This is the ONLY time the raw key is returned!
                name: key_name,
                created_at: now,
            },
        }),
    ))
}

/// Generate a secure random API key
fn generate_api_key() -> String {
    use rand::Rng;
    let mut rng = rand::rng();
    let mut bytes = [0u8; 32];
    rng.fill(&mut bytes);
    base64::Engine::encode(&base64::engine::general_purpose::URL_SAFE_NO_PAD, bytes)
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

/// Get organization members (API keys with access)
///
/// GET /api/v1/organizations/{id}/members
pub async fn members(
    State(state): State<AppState>,
    Extension(ctx): Extension<ApiKeyContext>,
    Path(id): Path<String>,
) -> AppResult<Json<Vec<OrganizationMember>>> {
    // Verify caller belongs to this organization
    if id != ctx.organization_id {
        return Err(AppError::NotFound(format!("Organization {} not found", id)));
    }

    let org = sqlx::query_as::<_, Organization>("SELECT * FROM organizations WHERE id = $1")
        .bind(&id)
        .fetch_optional(state.db.pool())
        .await?
        .ok_or_else(|| AppError::NotFound(format!("Organization {} not found", id)))?;

    #[derive(sqlx::FromRow)]
    struct ApiKeyRow {
        id: String,
        name: String,
        queues: Vec<String>,
        created_at: DateTime<Utc>,
    }

    let keys: Vec<ApiKeyRow> = sqlx::query_as(
        r#"
        SELECT id, name, queues, created_at
        FROM api_keys
        WHERE organization_id = $1 AND is_active = TRUE
        ORDER BY created_at ASC
        "#,
    )
    .bind(&id)
    .fetch_all(state.db.pool())
    .await?;

    let email = org.billing_email.unwrap_or_default();

    let members = keys
        .into_iter()
        .map(|k| {
            let role = if k.queues.iter().any(|q| q == "*") {
                "owner"
            } else {
                "member"
            };

            OrganizationMember {
                id: k.id.clone(),
                user_id: k.id,
                email: email.clone(),
                name: k.name,
                role: role.to_string(),
                joined_at: k.created_at,
                invited_by: None,
            }
        })
        .collect();

    Ok(Json(members))
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

/// Get organization usage and plan limits
///
/// Returns the current organization's resource usage against plan limits.
/// This is used to display usage warnings and upgrade prompts in the dashboard.
pub async fn usage(
    State(state): State<AppState>,
    Extension(ctx): Extension<ApiKeyContext>,
) -> AppResult<Json<crate::api::middleware::limits::UsageInfo>> {
    let usage_info =
        crate::api::middleware::limits::get_usage_info(state.db.pool(), &ctx.organization_id)
            .await?;

    Ok(Json(usage_info))
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
            custom_limits: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };

        let summary: OrganizationSummary = org.into();
        assert_eq!(summary.slug, "test-org");
    }
}
