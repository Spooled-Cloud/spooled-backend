//! Organization handlers

use axum::{
    extract::{Extension, Path, State},
    http::StatusCode,
    Json,
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tracing::warn;
use uuid::Uuid;

use crate::api::middleware::ValidatedJson;
use crate::api::AppState;
use crate::error::{AppError, AppResult};
use crate::models::{
    ApiKeyContext, CreateOrganizationRequest, Organization, OrganizationSummary,
    UpdateOrganizationRequest,
};

/// Rate limit: max slug checks per IP per minute
const MAX_SLUG_CHECKS_PER_MINUTE: i64 = 20;
/// Rate limit: max slug checks per IP per hour
const MAX_SLUG_CHECKS_PER_HOUR: i64 = 120;

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

/// Check slug availability request
#[derive(Debug, serde::Deserialize)]
pub struct CheckSlugQuery {
    pub slug: String,
}

/// Check slug availability response
#[derive(Debug, Serialize)]
pub struct CheckSlugResponse {
    /// Whether the slug is available
    pub available: bool,
    /// Whether slug format is valid
    pub valid: bool,
    /// Validation error message if any
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Suggested alternative if taken
    #[serde(skip_serializing_if = "Option::is_none")]
    pub suggestion: Option<String>,
}

/// Check if a slug is available
///
/// GET /api/v1/organizations/check-slug?slug=...
///
/// Rate limited to prevent slug enumeration attacks.
pub async fn check_slug(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
    axum::extract::Query(query): axum::extract::Query<CheckSlugQuery>,
) -> AppResult<Json<CheckSlugResponse>> {
    let slug = query.slug.to_lowercase().trim().to_string();

    // Extract IP for rate limiting
    let client_ip = extract_client_ip(&headers);

    // Rate limit: check per-minute limit
    let recent_minute: (i64,) = sqlx::query_as(
        r#"
        SELECT COUNT(*) 
        FROM email_login_codes 
        WHERE email LIKE 'slug_check:%' 
          AND code = $1 
          AND created_at > NOW() - INTERVAL '1 minute'
        "#,
    )
    .bind(&client_ip)
    .fetch_one(state.db.pool())
    .await
    .unwrap_or((0,));

    if recent_minute.0 >= MAX_SLUG_CHECKS_PER_MINUTE {
        warn!(ip = %client_ip, "Slug check rate limit exceeded (per minute)");
        return Err(AppError::RateLimit(
            "Too many requests. Please try again in a minute.".to_string(),
        ));
    }

    // Rate limit: check per-hour limit
    let recent_hour: (i64,) = sqlx::query_as(
        r#"
        SELECT COUNT(*) 
        FROM email_login_codes 
        WHERE email LIKE 'slug_check:%' 
          AND code = $1 
          AND created_at > NOW() - INTERVAL '1 hour'
        "#,
    )
    .bind(&client_ip)
    .fetch_one(state.db.pool())
    .await
    .unwrap_or((0,));

    if recent_hour.0 >= MAX_SLUG_CHECKS_PER_HOUR {
        warn!(ip = %client_ip, "Slug check rate limit exceeded (per hour)");
        return Err(AppError::RateLimit(
            "Too many requests. Please try again later.".to_string(),
        ));
    }

    // Log the check for rate limiting (using email_login_codes table with special prefix)
    let _ = sqlx::query(
        r#"
        INSERT INTO email_login_codes (email, code, expires_at, used_at)
        VALUES ($1, $2, NOW() + INTERVAL '1 hour', NOW())
        "#,
    )
    .bind(format!("slug_check:{}", slug))
    .bind(&client_ip)
    .execute(state.db.pool())
    .await;

    // Validate slug format
    if let Err(e) = validate_slug(&slug) {
        return Ok(Json(CheckSlugResponse {
            available: false,
            valid: false,
            error: Some(e.to_string()),
            suggestion: None,
        }));
    }

    // Check reserved slugs
    if crate::models::RESERVED_SLUGS.contains(&slug.as_str()) {
        return Ok(Json(CheckSlugResponse {
            available: false,
            valid: true,
            error: Some("This slug is reserved".to_string()),
            suggestion: Some(format!("{}-{}", slug, &Uuid::new_v4().to_string()[..6])),
        }));
    }

    // Check if slug exists in database
    let exists: Option<(String,)> = sqlx::query_as("SELECT id FROM organizations WHERE slug = $1")
        .bind(&slug)
        .fetch_optional(state.db.pool())
        .await?;

    if exists.is_some() {
        // Generate a suggestion
        let suggestion = format!("{}-{}", slug, &Uuid::new_v4().to_string()[..6]);
        return Ok(Json(CheckSlugResponse {
            available: false,
            valid: true,
            error: None,
            suggestion: Some(suggestion),
        }));
    }

    Ok(Json(CheckSlugResponse {
        available: true,
        valid: true,
        error: None,
        suggestion: None,
    }))
}

/// Extract client IP from headers for rate limiting
fn extract_client_ip(headers: &axum::http::HeaderMap) -> String {
    // Check CF-Connecting-IP (Cloudflare)
    if let Some(cf_ip) = headers
        .get("CF-Connecting-IP")
        .and_then(|v| v.to_str().ok())
    {
        return cf_ip.trim().to_string();
    }

    // Check X-Real-IP
    if let Some(real_ip) = headers.get("X-Real-IP").and_then(|v| v.to_str().ok()) {
        return real_ip.trim().to_string();
    }

    // Check X-Forwarded-For (first IP)
    if let Some(forwarded) = headers.get("X-Forwarded-For").and_then(|v| v.to_str().ok()) {
        if let Some(first_ip) = forwarded.split(',').next() {
            let trimmed = first_ip.trim();
            if !trimmed.is_empty() {
                return trimmed.to_string();
            }
        }
    }

    "unknown".to_string()
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

    // Check for duplicate billing email
    if let Some(ref email) = request.billing_email {
        let existing: Option<(String,)> =
            sqlx::query_as("SELECT id FROM organizations WHERE billing_email = $1")
                .bind(email)
                .fetch_optional(state.db.pool())
                .await?;

        if existing.is_some() {
            return Err(AppError::Conflict(
                "An account with this email already exists. Please log in instead.".to_string(),
            ));
        }
    }

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

/// Response for webhook token endpoints
#[derive(Debug, Serialize)]
pub struct WebhookTokenResponse {
    /// The webhook token (full value)
    pub webhook_token: Option<String>,
    /// The webhook URL for this organization
    pub webhook_url: String,
}

/// Get the current webhook token for the organization
pub async fn get_webhook_token(
    State(state): State<AppState>,
    Extension(ctx): Extension<ApiKeyContext>,
) -> AppResult<Json<WebhookTokenResponse>> {
    let org: Organization = sqlx::query_as("SELECT * FROM organizations WHERE id = $1")
        .bind(&ctx.organization_id)
        .fetch_one(state.db.pool())
        .await
        .map_err(|_| AppError::NotFound("Organization not found".to_string()))?;

    let webhook_token = org
        .settings
        .get("webhook_token")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    let base_url = state
        .settings
        .server
        .external_url
        .clone()
        .unwrap_or_else(|| "https://api.spooled.cloud".to_string());

    Ok(Json(WebhookTokenResponse {
        webhook_token,
        webhook_url: format!(
            "{}/api/v1/webhooks/{}/custom",
            base_url, ctx.organization_id
        ),
    }))
}

/// Response for regenerating webhook token
#[derive(Debug, Serialize)]
pub struct RegenerateWebhookTokenResponse {
    /// The new webhook token
    pub webhook_token: String,
    /// The webhook URL for this organization
    pub webhook_url: String,
}

/// Regenerate the webhook token for the organization
pub async fn regenerate_webhook_token(
    State(state): State<AppState>,
    Extension(ctx): Extension<ApiKeyContext>,
) -> AppResult<Json<RegenerateWebhookTokenResponse>> {
    // Generate new webhook token
    let new_token = generate_webhook_token();

    // Get current settings and update webhook_token
    let org: Organization = sqlx::query_as("SELECT * FROM organizations WHERE id = $1")
        .bind(&ctx.organization_id)
        .fetch_one(state.db.pool())
        .await
        .map_err(|_| AppError::NotFound("Organization not found".to_string()))?;

    let mut settings = org.settings.clone();
    if let Some(obj) = settings.as_object_mut() {
        obj.insert("webhook_token".to_string(), serde_json::json!(new_token));
    }

    sqlx::query("UPDATE organizations SET settings = $1, updated_at = $2 WHERE id = $3")
        .bind(&settings)
        .bind(Utc::now())
        .bind(&ctx.organization_id)
        .execute(state.db.pool())
        .await?;

    let base_url = state
        .settings
        .server
        .external_url
        .clone()
        .unwrap_or_else(|| "https://api.spooled.cloud".to_string());

    tracing::info!(
        org_id = %ctx.organization_id,
        "Webhook token regenerated"
    );

    Ok(Json(RegenerateWebhookTokenResponse {
        webhook_token: new_token,
        webhook_url: format!(
            "{}/api/v1/webhooks/{}/custom",
            base_url, ctx.organization_id
        ),
    }))
}

/// Request to clear the webhook token (make webhook accept requests without token)
#[derive(Debug, Deserialize)]
pub struct ClearWebhookTokenRequest {
    /// Must be true to confirm clearing the token
    pub confirm: bool,
}

/// Clear the webhook token (disable webhook authentication)
pub async fn clear_webhook_token(
    State(state): State<AppState>,
    Extension(ctx): Extension<ApiKeyContext>,
    Json(request): Json<ClearWebhookTokenRequest>,
) -> AppResult<Json<WebhookTokenResponse>> {
    if !request.confirm {
        return Err(AppError::Validation(
            "You must set confirm: true to clear the webhook token".to_string(),
        ));
    }

    // Get current settings and remove webhook_token
    let org: Organization = sqlx::query_as("SELECT * FROM organizations WHERE id = $1")
        .bind(&ctx.organization_id)
        .fetch_one(state.db.pool())
        .await
        .map_err(|_| AppError::NotFound("Organization not found".to_string()))?;

    let mut settings = org.settings.clone();
    if let Some(obj) = settings.as_object_mut() {
        obj.remove("webhook_token");
    }

    sqlx::query("UPDATE organizations SET settings = $1, updated_at = $2 WHERE id = $3")
        .bind(&settings)
        .bind(Utc::now())
        .bind(&ctx.organization_id)
        .execute(state.db.pool())
        .await?;

    let base_url = state
        .settings
        .server
        .external_url
        .clone()
        .unwrap_or_else(|| "https://api.spooled.cloud".to_string());

    tracing::warn!(
        org_id = %ctx.organization_id,
        "Webhook token cleared - webhook now accepts unauthenticated requests"
    );

    Ok(Json(WebhookTokenResponse {
        webhook_token: None,
        webhook_url: format!(
            "{}/api/v1/webhooks/{}/custom",
            base_url, ctx.organization_id
        ),
    }))
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
    fn test_organization_summary_conversion() {
        let org = Organization {
            id: "test-id".to_string(),
            name: "Test Org".to_string(),
            slug: "test-org".to_string(),
            plan_tier: "free".to_string(),
            billing_email: None,
            settings: serde_json::json!({}),
            custom_limits: None,
            stripe_customer_id: None,
            stripe_subscription_id: None,
            stripe_subscription_status: None,
            stripe_current_period_end: None,
            stripe_cancel_at_period_end: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };

        let summary: OrganizationSummary = org.into();
        assert_eq!(summary.slug, "test-org");
    }

    #[test]
    fn test_check_slug_response_available() {
        let response = CheckSlugResponse {
            available: true,
            valid: true,
            error: None,
            suggestion: None,
        };

        let json = serde_json::to_string(&response).unwrap();
        assert!(json.contains("\"available\":true"));
        assert!(json.contains("\"valid\":true"));
        // error and suggestion should not be present (skip_serializing_if)
        assert!(!json.contains("\"error\""));
        assert!(!json.contains("\"suggestion\""));
    }

    #[test]
    fn test_check_slug_response_taken() {
        let response = CheckSlugResponse {
            available: false,
            valid: true,
            error: None,
            suggestion: Some("my-org-abc123".to_string()),
        };

        let json = serde_json::to_string(&response).unwrap();
        assert!(json.contains("\"available\":false"));
        assert!(json.contains("\"suggestion\":\"my-org-abc123\""));
    }

    #[test]
    fn test_check_slug_response_invalid() {
        let response = CheckSlugResponse {
            available: false,
            valid: false,
            error: Some("Slug must start with a letter".to_string()),
            suggestion: None,
        };

        let json = serde_json::to_string(&response).unwrap();
        assert!(json.contains("\"valid\":false"));
        assert!(json.contains("Slug must start with a letter"));
    }

    #[test]
    fn test_validate_slug_valid() {
        assert!(validate_slug("my-org").is_ok());
        assert!(validate_slug("test123").is_ok());
        assert!(validate_slug("a1b2c3").is_ok());
        assert!(validate_slug("company-name").is_ok());
        assert!(validate_slug("abc").is_ok());
    }

    #[test]
    fn test_validate_slug_too_short() {
        // Empty slug
        assert!(validate_slug("").is_err());
        // Would fail if we had a min-length check - but current impl allows any length > 0
    }

    #[test]
    fn test_validate_slug_too_long() {
        let long_slug = "a".repeat(101);
        assert!(validate_slug(&long_slug).is_err());
    }

    #[test]
    fn test_validate_slug_invalid_chars() {
        assert!(validate_slug("my_org").is_err()); // underscore
        assert!(validate_slug("My-Org").is_err()); // uppercase
        assert!(validate_slug("my org").is_err()); // space
        assert!(validate_slug("my@org").is_err()); // special char
    }

    #[test]
    fn test_validate_slug_hyphen_position() {
        assert!(validate_slug("-starts-with").is_err());
        assert!(validate_slug("ends-with-").is_err());
        assert!(validate_slug("-both-").is_err());
    }

    #[test]
    fn test_organization_member_serialization() {
        let member = OrganizationMember {
            id: "key-123".to_string(),
            user_id: "key-123".to_string(),
            email: "user@example.com".to_string(),
            name: "Initial Admin Key".to_string(),
            role: "owner".to_string(),
            joined_at: Utc::now(),
            invited_by: None,
        };

        let json = serde_json::to_string(&member).unwrap();
        assert!(json.contains("key-123"));
        assert!(json.contains("user@example.com"));
        assert!(json.contains("owner"));
        // invited_by is None so should not appear (skip_serializing_if)
        assert!(!json.contains("invited_by"));
    }

    #[test]
    fn test_organization_member_with_inviter() {
        let member = OrganizationMember {
            id: "key-456".to_string(),
            user_id: "key-456".to_string(),
            email: "member@example.com".to_string(),
            name: "Team Member Key".to_string(),
            role: "member".to_string(),
            joined_at: Utc::now(),
            invited_by: Some("admin@example.com".to_string()),
        };

        let json = serde_json::to_string(&member).unwrap();
        assert!(json.contains("member"));
        assert!(json.contains("invited_by"));
        assert!(json.contains("admin@example.com"));
    }

    #[test]
    fn test_initial_api_key_serialization() {
        let api_key = InitialApiKey {
            id: "key-789".to_string(),
            key: "sk_test_abcdef123456".to_string(),
            name: "Initial Admin Key".to_string(),
            created_at: Utc::now(),
        };

        let json = serde_json::to_string(&api_key).unwrap();
        assert!(json.contains("key-789"));
        assert!(json.contains("sk_test_abcdef123456"));
        assert!(json.contains("Initial Admin Key"));
    }

    #[test]
    fn test_extract_client_ip_cloudflare() {
        let mut headers = axum::http::HeaderMap::new();
        headers.insert("CF-Connecting-IP", "203.0.113.50".parse().unwrap());
        headers.insert("X-Forwarded-For", "10.0.0.1, 172.16.0.1".parse().unwrap());

        let ip = extract_client_ip(&headers);
        assert_eq!(ip, "203.0.113.50");
    }

    #[test]
    fn test_extract_client_ip_real_ip() {
        let mut headers = axum::http::HeaderMap::new();
        headers.insert("X-Real-IP", "192.168.1.100".parse().unwrap());
        headers.insert("X-Forwarded-For", "10.0.0.1".parse().unwrap());

        let ip = extract_client_ip(&headers);
        assert_eq!(ip, "192.168.1.100");
    }

    #[test]
    fn test_extract_client_ip_forwarded_for() {
        let mut headers = axum::http::HeaderMap::new();
        headers.insert(
            "X-Forwarded-For",
            "203.0.113.195, 70.41.3.18, 150.172.238.178"
                .parse()
                .unwrap(),
        );

        let ip = extract_client_ip(&headers);
        assert_eq!(ip, "203.0.113.195");
    }

    #[test]
    fn test_extract_client_ip_unknown() {
        let headers = axum::http::HeaderMap::new();
        let ip = extract_client_ip(&headers);
        assert_eq!(ip, "unknown");
    }

    #[test]
    fn test_extract_client_ip_empty_forwarded() {
        let mut headers = axum::http::HeaderMap::new();
        headers.insert("X-Forwarded-For", "".parse().unwrap());

        let ip = extract_client_ip(&headers);
        assert_eq!(ip, "unknown");
    }

    #[test]
    fn test_extract_client_ip_whitespace_handling() {
        let mut headers = axum::http::HeaderMap::new();
        headers.insert("CF-Connecting-IP", "  192.168.1.1  ".parse().unwrap());

        let ip = extract_client_ip(&headers);
        assert_eq!(ip, "192.168.1.1");
    }
}
