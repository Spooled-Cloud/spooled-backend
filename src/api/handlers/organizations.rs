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

use crate::api::handlers::require_unrestricted_key;
use crate::api::middleware::ValidatedJson;
use crate::api::AppState;
use crate::error::{AppError, AppResult};
use crate::models::{
    ApiKeyContext, CreateOrganizationRequest, Organization, OrganizationSummary,
    UpdateOrganizationRequest,
};

/// Rate limit: max slug checks per IP per minute
const MAX_SLUG_CHECKS_PER_MINUTE: i64 = 60;
/// Rate limit: max slug checks per IP per hour
const MAX_SLUG_CHECKS_PER_HOUR: i64 = 300;

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
    let client_ip = extract_client_ip(&state, &headers);

    // Rate limiting lives in Redis, like every other limiter in this codebase.
    //
    // It used to INSERT a marker row into `email_login_codes` per request (with
    // `code` holding the IP) and COUNT those rows twice per request with a
    // `LIKE 'slug_check:%'` scan. This is the highest-frequency public endpoint
    // in the product — the signup form calls it as the user types — so it was
    // the largest contributor to unbounded growth of a table the LOGIN path
    // scans on every attempt, and it got slower the more it was used.
    check_slug_rate_limit(
        &state,
        &client_ip,
        "minute",
        MAX_SLUG_CHECKS_PER_MINUTE,
        60,
        "Too many requests. Please try again in a minute.",
    )
    .await?;
    check_slug_rate_limit(
        &state,
        &client_ip,
        "hour",
        MAX_SLUG_CHECKS_PER_HOUR,
        3600,
        "Too many requests. Please try again later.",
    )
    .await?;

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

/// Request body for generating a slug
#[derive(Debug, Deserialize)]
pub struct GenerateSlugRequest {
    /// Organization name to generate slug from
    pub name: String,
}

/// Response for slug generation
#[derive(Debug, Serialize)]
pub struct GenerateSlugResponse {
    /// Generated slug
    pub slug: String,
}

/// Generate a unique slug from an organization name
///
/// POST /api/v1/organizations/generate-slug
///
/// Takes an organization name and generates a URL-safe, unique slug.
pub async fn generate_slug(
    State(state): State<AppState>,
    Json(request): Json<GenerateSlugRequest>,
) -> AppResult<Json<GenerateSlugResponse>> {
    let name = request.name.trim();

    if name.is_empty() || name.len() > 200 {
        return Err(AppError::Validation(
            "Name must be 1-200 characters".to_string(),
        ));
    }

    // Convert name to slug: lowercase, replace spaces/special chars with hyphens
    let base_slug: String = name
        .to_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();

    // Clean up consecutive hyphens and trim
    let mut base_slug: String = base_slug
        .split('-')
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("-");

    // Ensure it doesn't start with a number
    if base_slug
        .chars()
        .next()
        .map(|c| c.is_ascii_digit())
        .unwrap_or(false)
    {
        base_slug = format!("org-{}", base_slug);
    }

    // Truncate if too long (leave room for unique suffix)
    if base_slug.len() > 80 {
        base_slug = base_slug[..80].to_string();
        // Trim trailing hyphen if present
        base_slug = base_slug.trim_end_matches('-').to_string();
    }

    // Check if base slug is available
    let exists: Option<(String,)> = sqlx::query_as("SELECT id FROM organizations WHERE slug = $1")
        .bind(&base_slug)
        .fetch_optional(state.db.pool())
        .await?;

    let final_slug = if exists.is_some() {
        // Add unique suffix
        format!("{}-{}", base_slug, &Uuid::new_v4().to_string()[..6])
    } else {
        base_slug
    };

    Ok(Json(GenerateSlugResponse { slug: final_slug }))
}

/// Constant-time string comparison to prevent timing attacks
fn constant_time_compare(a: &str, b: &str) -> bool {
    use subtle::ConstantTimeEq;

    // If lengths differ, the signatures are definitely different
    // But we still do constant-time work to not leak the length difference
    let len_eq = a.len() == b.len();

    // Pad shorter string to match longer one (constant time padding)
    let max_len = a.len().max(b.len());
    let a_bytes: Vec<u8> = a
        .bytes()
        .chain(std::iter::repeat(0u8))
        .take(max_len)
        .collect();
    let b_bytes: Vec<u8> = b
        .bytes()
        .chain(std::iter::repeat(0u8))
        .take(max_len)
        .collect();

    // Constant-time byte comparison
    let bytes_eq = a_bytes.ct_eq(&b_bytes).into();

    // Both length and content must match
    len_eq && bytes_eq
}

/// One window of the `check-slug` per-IP throttle.
///
/// Fails OPEN when Redis is unavailable (unless `RATE_LIMIT_FAIL_CLOSED`),
/// matching `check-email`: this endpoint gates the signup form, and breaking
/// signup for every self-hosted deployment without Redis would be worse than
/// the enumeration risk it mitigates.
async fn check_slug_rate_limit(
    state: &AppState,
    client_ip: &str,
    window_label: &str,
    max_requests: i64,
    window_secs: u64,
    message: &str,
) -> AppResult<()> {
    let Some(ref cache) = state.cache else {
        if state.settings.rate_limit.fail_closed_on_redis_error {
            warn!("Redis not configured; denying check-slug (RATE_LIMIT_FAIL_CLOSED)");
            return Err(AppError::RateLimit(message.to_string()));
        }
        return Ok(());
    };

    let key = format!("check_slug:{}:{}", window_label, client_ip);
    match cache
        .check_rate_limit(&key, max_requests.max(1) as u32, window_secs)
        .await
    {
        Ok(result) if !result.allowed => {
            warn!(ip = %client_ip, window = %window_label, "Slug check rate limit exceeded");
            Err(AppError::RateLimit(message.to_string()))
        }
        Ok(_) => Ok(()),
        Err(e) => {
            if state.settings.rate_limit.fail_closed_on_redis_error {
                warn!(error = %e, "Redis error during check-slug rate limit; denying (fail-closed)");
                return Err(AppError::RateLimit(message.to_string()));
            }
            warn!(error = %e, "Redis error during check-slug rate limit; allowing");
            Ok(())
        }
    }
}

/// Client IP for abuse controls.
///
/// Delegates to the shared, trusted-proxy-aware extractor. This module used to
/// carry its own copy that trusted the LEFTMOST `X-Forwarded-For` entry — the
/// one the caller supplies — so `check-slug`'s per-IP limits could be bypassed
/// by varying a header, and each bypassed request still wrote a permanent row.
fn extract_client_ip(state: &AppState, headers: &axum::http::HeaderMap) -> String {
    crate::api::middleware::client_ip::client_ip(headers, &state.settings.trusted_proxy)
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
                (Some(expected), Some(provided)) if constant_time_compare(expected, provided) => {
                    // Valid admin key (constant-time comparison prevents timing attacks)
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
    // SECURITY: Generate a webhook token for the org so incoming webhook ingestion is never public by default.
    let org_settings = serde_json::json!({
        "webhook_token": generate_webhook_token(),
    });

    // Create organization
    let org = sqlx::query_as::<_, Organization>(
        r#"
        INSERT INTO organizations (id, name, slug, plan_tier, billing_email, settings, created_at, updated_at)
        VALUES ($1, $2, $3, 'free', $4, $5, $6, $6)
        RETURNING *
        "#,
    )
    .bind(&org_id)
    .bind(&request.name)
    .bind(&request.slug)
    .bind(&request.billing_email)
    .bind(&org_settings)
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
    // Uses sp_ prefix (new standard); sk_ is legacy and only accepted for backward compatibility
    let raw_key = format!(
        "sp_{}_{}",
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
    let lookup_hash = crate::models::api_key_lookup_hash(&raw_key);

    let key_name = "Initial Admin Key".to_string();
    let queues: Vec<String> = vec!["*".to_string()]; // Full access to all queues

    sqlx::query(
        r#"
        INSERT INTO api_keys (
            id, organization_id, key_hash, key_prefix, name, queues, rate_limit,
            is_active, created_at, expires_at, lookup_hash
        )
        VALUES ($1, $2, $3, $4, $5, $6, NULL, TRUE, $7, NULL, $8)
        "#,
    )
    .bind(&api_key_id)
    .bind(&org_id)
    .bind(&key_hash)
    .bind(&key_prefix)
    .bind(&key_name)
    .bind(&queues)
    .bind(now)
    .bind(&lookup_hash)
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
    use rand::RngExt;
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

    let mut org = sqlx::query_as::<_, Organization>("SELECT * FROM organizations WHERE id = $1")
        .bind(&id)
        .fetch_optional(state.db.pool())
        .await?
        .ok_or_else(|| AppError::NotFound(format!("Organization {} not found", id)))?;

    // Queue-scoped keys must not read the inbound webhook ingestion secret.
    if !ctx.is_unrestricted() {
        if let Some(obj) = org.settings.as_object_mut() {
            obj.remove("webhook_token");
        }
    }

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
    require_unrestricted_key(&ctx, "list organization members")?;

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
    require_unrestricted_key(&ctx, "update organization settings")?;

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
        // Preserve the security-critical webhook_token. This handler replaces the whole
        // settings object, so a PUT that omits webhook_token would silently delete it —
        // even though clear_webhook_token is meant to be the only way to remove it. Carry
        // the existing token over unless the caller explicitly provides a new one.
        let mut merged = new_settings.clone();
        if let Some(existing_tok) = existing.settings.get("webhook_token").cloned() {
            if let Some(obj) = merged.as_object_mut() {
                obj.entry("webhook_token".to_string())
                    .or_insert(existing_tok);
            }
        }
        merged
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
    require_unrestricted_key(&ctx, "delete the organization")?;

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

    // CRITICAL: Get API key hashes BEFORE deleting, for cache invalidation
    let api_key_hashes: Vec<(String,)> =
        sqlx::query_as("SELECT key_hash FROM api_keys WHERE organization_id = $1")
            .bind(&id)
            .fetch_all(state.db.pool())
            .await?;

    let result = sqlx::query("DELETE FROM organizations WHERE id = $1")
        .bind(&id)
        .execute(state.db.pool())
        .await?;

    if result.rows_affected() == 0 {
        return Err(AppError::NotFound(format!("Organization {} not found", id)));
    }

    // CRITICAL: Invalidate all cached API keys for this org
    // Without this, deleted org's API keys remain valid in cache for up to 1 hour!
    if let Some(ref cache) = state.cache {
        for (key_hash,) in api_key_hashes {
            let hash_prefix = if key_hash.len() >= 16 {
                &key_hash[..16]
            } else {
                &key_hash
            };
            let reverse_key = format!("api_key_reverse:{}", hash_prefix);
            if let Ok(Some(lookup_hash)) = cache.get(&reverse_key).await {
                let cache_key = format!("api_key:{}", lookup_hash);
                let _ = cache.delete(&cache_key).await;
                let _ = cache.delete(&reverse_key).await;
            }
        }
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
    require_unrestricted_key(&ctx, "view organization usage")?;

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
    require_unrestricted_key(&ctx, "read organization webhook tokens")?;

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
    require_unrestricted_key(&ctx, "regenerate organization webhook tokens")?;

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

/// Request to clear the webhook token
#[derive(Debug, Deserialize)]
pub struct ClearWebhookTokenRequest {
    /// Must be true to confirm clearing the token
    pub confirm: bool,
}

/// Clear the webhook token
pub async fn clear_webhook_token(
    State(state): State<AppState>,
    Extension(ctx): Extension<ApiKeyContext>,
    Json(request): Json<ClearWebhookTokenRequest>,
) -> AppResult<Json<WebhookTokenResponse>> {
    // SECURITY: Disallow disabling webhook authentication.
    // A public ingestion endpoint is a critical risk for multi-tenant systems.
    let _ = (state, ctx, request);
    Err(AppError::Validation(
        "Clearing the webhook token is not allowed. Use /organizations/webhook-token/regenerate to rotate it."
            .to_string(),
    ))
}

/// Generate a secure webhook token for incoming webhooks
fn generate_webhook_token() -> String {
    use rand::RngExt;
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
            key: "sp_test_abcdef123456".to_string(),
            name: "Initial Admin Key".to_string(),
            created_at: Utc::now(),
        };

        let json = serde_json::to_string(&api_key).unwrap();
        assert!(json.contains("key-789"));
        assert!(json.contains("sp_test_abcdef123456"));
        assert!(json.contains("Initial Admin Key"));
    }

    // Client-IP extraction moved to crate::api::middleware::client_ip, which is
    // trusted-proxy aware and shared with the rate limiters. The exhaustive
    // table lives there; the removed tests here asserted the old behaviour,
    // including that the LEFTMOST X-Forwarded-For entry wins — which was the
    // bypass, since that entry is supplied by the caller.
}
