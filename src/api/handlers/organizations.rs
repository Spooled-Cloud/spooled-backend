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

/// An organization as `GET`/`PUT /organizations/{id}` return it: every
/// [`Organization`] field plus the billing-email change awaiting verification.
#[derive(Debug, Serialize, sqlx::FromRow)]
pub struct OrganizationWithPendingEmail {
    #[serde(flatten)]
    #[sqlx(flatten)]
    pub organization: Organization,
    /// A new billing email waiting for its confirmation code. The verified
    /// `billing_email` stays the email-login identity until this one is
    /// verified and replaces it. Null when no change is pending.
    #[sqlx(default)]
    pub pending_billing_email: Option<String>,
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
    crate::api::middleware::client_ip::PeerAddr(peer): crate::api::middleware::client_ip::PeerAddr,
    headers: axum::http::HeaderMap,
    axum::extract::Query(query): axum::extract::Query<CheckSlugQuery>,
) -> AppResult<Json<CheckSlugResponse>> {
    let slug = query.slug.to_lowercase().trim().to_string();

    // Extract IP for rate limiting
    let client_ip = extract_client_ip(&state, &headers, peer);

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

    // Validate slug format: the length signup accepts, then the characters.
    if let Err(e) = validate_signup_slug_length(&slug).and_then(|()| validate_slug(&slug)) {
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
            suggestion: Some(with_random_suffix(&slug)),
        }));
    }

    // Check if slug exists in database
    if slug_taken(&state, &slug).await? {
        // Generate a suggestion
        let suggestion = with_random_suffix(&slug);
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
/// Takes an organization name and generates a URL-safe, unique slug that
/// `GET /organizations/check-slug`, website signup and `POST /organizations`
/// all accept: 3-50 characters, a leading letter, never a name in
/// `RESERVED_SLUGS`. The bare base is offered when it is long enough, not
/// reserved and not taken; otherwise it gets a random suffix (`acme-1f2e3d`),
/// and a name with no ASCII letter or digit gets `org-` and the suffix.
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

    let base_slug = slug_base_for_name(name);
    // Only a base that could be offered bare is looked up.
    let offer_bare = !base_needs_suffix(&base_slug) && !slug_taken(&state, &base_slug).await?;
    let final_slug = if offer_bare {
        base_slug
    } else {
        with_random_suffix(&base_slug)
    };

    Ok(Json(GenerateSlugResponse { slug: final_slug }))
}

/// Shortest and longest slug website signup (`CompleteSignupRequest`) and
/// `POST /organizations` (`CreateOrganizationRequest`) accept.
pub(crate) const SLUG_MIN_LEN: usize = 3;
pub(crate) const SLUG_MAX_LEN: usize = 50;

/// `-` and six random hex characters, added to a slug that is taken, reserved
/// or too short.
const SLUG_SUFFIX_LEN: usize = 7;

/// The signup length rule, counted the way the `validator` length check of the
/// signup request counts (characters), on the trimmed and lower-cased slug.
/// `check-slug` applies it so that a slug it reports valid is never refused for
/// its length at signup (W9-R1). `POST /organizations` keeps [`validate_slug`]
/// plus its own 3-50 request rule.
fn validate_signup_slug_length(slug: &str) -> Result<(), AppError> {
    let len = slug.chars().count();
    if (SLUG_MIN_LEN..=SLUG_MAX_LEN).contains(&len) {
        Ok(())
    } else {
        Err(AppError::Validation(format!(
            "Slug must be {SLUG_MIN_LEN}-{SLUG_MAX_LEN} characters"
        )))
    }
}

/// The slug part of an organization name: lower-cased ASCII letters and digits,
/// every other run of characters one hyphen, none at either end, `org-` in
/// front of a leading digit, cut to [`SLUG_MAX_LEN`]. Empty when the name has
/// no ASCII letter or digit.
fn slug_base_for_name(name: &str) -> String {
    let mapped: String = name
        .to_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    let mut base = mapped
        .split('-')
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("-");
    if base.starts_with(|c: char| c.is_ascii_digit()) {
        base = format!("org-{base}");
    }
    truncate_slug(&base, SLUG_MAX_LEN).to_string()
}

/// Whether a generated base cannot be offered as it is, whoever holds it: it
/// is shorter than signup accepts (empty included) or a reserved name (W9-R2).
fn base_needs_suffix(base: &str) -> bool {
    base.len() < SLUG_MIN_LEN || crate::models::RESERVED_SLUGS.contains(&base)
}

/// `base` with `-` and six random hex characters, `base` cut first so the
/// result is at most [`SLUG_MAX_LEN`]. An empty base becomes `org-` and the
/// suffix. `base` is an ASCII slug (letters, digits, hyphens).
fn with_random_suffix(base: &str) -> String {
    let base = truncate_slug(base, SLUG_MAX_LEN - SLUG_SUFFIX_LEN);
    let base = if base.is_empty() { "org" } else { base };
    format!("{}-{}", base, &Uuid::new_v4().to_string()[..6])
}

/// `slug` cut to at most `max` bytes, without a trailing hyphen. Slugs are
/// ASCII, so a byte cut is a character cut.
fn truncate_slug(slug: &str, max: usize) -> &str {
    let cut = slug.get(..max).unwrap_or(slug);
    cut.trim_end_matches('-')
}

/// Whether an organization already holds `slug`.
async fn slug_taken(state: &AppState, slug: &str) -> AppResult<bool> {
    let exists: Option<(String,)> = sqlx::query_as("SELECT id FROM organizations WHERE slug = $1")
        .bind(slug)
        .fetch_optional(state.db.pool())
        .await?;
    Ok(exists.is_some())
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
fn extract_client_ip(
    state: &AppState,
    headers: &axum::http::HeaderMap,
    peer: Option<std::net::IpAddr>,
) -> String {
    crate::api::middleware::client_ip::client_ip_from_peer(
        headers,
        peer,
        &state.settings.trusted_proxy,
    )
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

/// Normalize a billing email the way every lookup compares it.
pub fn normalize_billing_email(email: &str) -> String {
    email.trim().to_lowercase()
}

/// Whether another live organization already holds this billing email as a
/// VERIFIED address.
///
/// Email login resolves an account by its verified billing email (lower-cased),
/// so two live orgs sharing one would make the login land in an arbitrary one of
/// them. Every path that sets a billing email — public create, admin create, and
/// both update routes — must refuse a duplicate. Unverified addresses do not
/// count: they never resolve a login, and letting them block an address would
/// let anyone stop its real owner from signing up. Soft-deleted orgs do not
/// count either, matching the email-login lookups.
pub async fn billing_email_taken(
    pool: &sqlx::PgPool,
    email: &str,
    exclude_org_id: Option<&str>,
) -> Result<bool, sqlx::Error> {
    let (taken,): (bool,) = sqlx::query_as(
        r#"
        SELECT EXISTS (
            SELECT 1 FROM organizations
            WHERE lower(billing_email) = lower($1)
              AND billing_email_verified_at IS NOT NULL
              AND plan_tier <> 'deleted'
              AND ($2::TEXT IS NULL OR id <> $2)
        )
        "#,
    )
    .bind(email.trim())
    .bind(exclude_org_id)
    .fetch_one(pool)
    .await?;
    Ok(taken)
}

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

            // An empty configured key must never match an empty header.
            let configured = state
                .settings
                .registration
                .admin_api_key
                .as_ref()
                .filter(|key| !key.is_empty());
            match (configured, admin_key) {
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
        if billing_email_taken(state.db.pool(), email, None).await? {
            return Err(AppError::Conflict(
                "An account with this email already exists. Please log in instead.".to_string(),
            ));
        }
    }

    // Stored unverified: whoever calls this route has not proven they own the
    // address, so email login ignores it until the verification code flow runs.
    let billing_email = request
        .billing_email
        .as_deref()
        .map(normalize_billing_email);

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
    .bind(&billing_email)
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
) -> AppResult<Json<OrganizationWithPendingEmail>> {
    // Verify user belongs to this organization
    if id != ctx.organization_id {
        return Err(AppError::NotFound(format!("Organization {} not found", id)));
    }

    let mut org = sqlx::query_as::<_, OrganizationWithPendingEmail>(
        "SELECT * FROM organizations WHERE id = $1",
    )
    .bind(&id)
    .fetch_optional(state.db.pool())
    .await?
    .ok_or_else(|| AppError::NotFound(format!("Organization {} not found", id)))?;

    // Queue-scoped keys must not read the inbound webhook ingestion secret.
    if !ctx.is_unrestricted() {
        if let Some(obj) = org.organization.settings.as_object_mut() {
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
) -> AppResult<Json<OrganizationWithPendingEmail>> {
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

    if let Some(ref email) = request.billing_email {
        if billing_email_taken(state.db.pool(), email, Some(&id)).await? {
            return Err(AppError::Conflict(
                "Another account already uses this billing email".to_string(),
            ));
        }
    }

    let name = request.name.unwrap_or(existing.name);
    // None keeps the billing email (and any pending change) as it is.
    let billing_email = request
        .billing_email
        .as_deref()
        .map(normalize_billing_email);

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
        preserve_webhook_token(&existing.settings, new_settings.clone())?
    } else {
        existing.settings
    };

    // A new address has not been proven yet, so email login must not resolve
    // this org by it until the verification code flow runs. An org with a
    // verified address keeps it (and email login with it) meanwhile: the new
    // one waits in `pending_billing_email`, and sending the active address
    // again withdraws it. Without a verified address there is nothing to keep,
    // and the new one replaces it, unverified. Every CASE reads the old row.
    let org = sqlx::query_as::<_, OrganizationWithPendingEmail>(
        r#"
        UPDATE organizations
        SET name = $1,
            billing_email = CASE
                WHEN $2::TEXT IS NULL OR lower(billing_email) IS NOT DISTINCT FROM $2
                THEN billing_email
                WHEN billing_email IS NOT NULL AND billing_email_verified_at IS NOT NULL
                THEN billing_email
                ELSE $2
            END,
            billing_email_verified_at = CASE
                WHEN $2::TEXT IS NULL OR lower(billing_email) IS NOT DISTINCT FROM $2
                THEN billing_email_verified_at
                WHEN billing_email IS NOT NULL AND billing_email_verified_at IS NOT NULL
                THEN billing_email_verified_at
                ELSE NULL
            END,
            pending_billing_email = CASE
                WHEN $2::TEXT IS NULL THEN pending_billing_email
                WHEN lower(billing_email) IS NOT DISTINCT FROM $2 THEN NULL
                WHEN billing_email IS NOT NULL AND billing_email_verified_at IS NOT NULL
                THEN $2
                ELSE NULL
            END,
            settings = $3,
            updated_at = NOW()
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

    // Stop billing first: if the subscription cannot be canceled, keep the org.
    let org = sqlx::query_as::<_, Organization>("SELECT * FROM organizations WHERE id = $1")
        .bind(&id)
        .fetch_optional(state.db.pool())
        .await?
        .ok_or_else(|| AppError::NotFound(format!("Organization {} not found", id)))?;
    crate::api::handlers::billing::cancel_subscription_before_org_delete(&state, &org).await?;

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

    let settings = settings_with_webhook_token(org.settings, &new_token);

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

/// Insert `webhook_token` into settings, even when settings is not an object.
///
/// `POST .../webhook-token/regenerate` used `as_object_mut()` and skipped the
/// write when settings was an array/string, then still returned the new token.
fn settings_with_webhook_token(existing: serde_json::Value, token: &str) -> serde_json::Value {
    let mut settings = if existing.is_object() {
        existing
    } else {
        serde_json::json!({})
    };
    if let Some(obj) = settings.as_object_mut() {
        obj.insert(
            "webhook_token".to_string(),
            serde_json::Value::String(token.to_string()),
        );
    }
    settings
}

/// Carry `webhook_token` across a settings replace.
///
/// `PUT /organizations/{id}` and `PATCH /admin/organizations/{id}` replace
/// `settings` wholesale. `as_object_mut()` is None for a JSON array/string/number,
/// so a merge that only touched objects dropped the token — even though
/// `clear_webhook_token` is not allowed to remove it.
pub(crate) fn preserve_webhook_token(
    existing: &serde_json::Value,
    incoming: serde_json::Value,
) -> Result<serde_json::Value, AppError> {
    let existing_tok = existing.get("webhook_token").cloned();
    match incoming {
        serde_json::Value::Object(mut obj) => {
            if let Some(tok) = existing_tok {
                obj.entry("webhook_token".to_string()).or_insert(tok);
            }
            Ok(serde_json::Value::Object(obj))
        }
        other => {
            if existing_tok.is_some() {
                Err(AppError::BadRequest(
                    "settings must be a JSON object while a webhook token is configured"
                        .to_string(),
                ))
            } else {
                Ok(other)
            }
        }
    }
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
    fn settings_with_webhook_token_writes_into_object() {
        let existing = serde_json::json!({"description": "keep"});
        let got = settings_with_webhook_token(existing, "whk_new");
        assert_eq!(got["webhook_token"], "whk_new");
        assert_eq!(got["description"], "keep");
    }

    #[test]
    fn settings_with_webhook_token_replaces_non_object() {
        let got = settings_with_webhook_token(serde_json::json!(["theme"]), "whk_new");
        assert_eq!(got, serde_json::json!({"webhook_token": "whk_new"}));
    }

    #[test]
    fn preserve_webhook_token_keeps_token_on_object_replace() {
        let existing = serde_json::json!({"webhook_token": "whk_keep", "description": "old"});
        let incoming = serde_json::json!({"description": "new"});
        let got = preserve_webhook_token(&existing, incoming).unwrap();
        assert_eq!(got["webhook_token"], "whk_keep");
        assert_eq!(got["description"], "new");
    }

    #[test]
    fn preserve_webhook_token_rejects_array_that_would_drop_token() {
        let existing = serde_json::json!({"webhook_token": "whk_keep"});
        let err = preserve_webhook_token(&existing, serde_json::json!(["theme"])).unwrap_err();
        match err {
            AppError::BadRequest(msg) => assert!(msg.contains("webhook token")),
            other => panic!("expected BadRequest, got {other:?}"),
        }
    }

    #[test]
    fn preserve_webhook_token_allows_non_object_when_no_token() {
        let existing = serde_json::json!({"description": "x"});
        let got = preserve_webhook_token(&existing, serde_json::json!(["theme"])).unwrap();
        assert_eq!(got, serde_json::json!(["theme"]));
    }

    #[test]
    fn test_organization_summary_conversion() {
        let org = Organization {
            id: "test-id".to_string(),
            name: "Test Org".to_string(),
            slug: "test-org".to_string(),
            plan_tier: "free".to_string(),
            billing_email: None,
            billing_email_verified_at: None,
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

    /// The signup request's own rule for `slug` (3-50 characters as sent).
    fn signup_accepts(slug: &str) -> bool {
        use validator::Validate;
        crate::api::handlers::email_login::CompleteSignupRequest {
            signup_token: "t".to_string(),
            name: "Valid Name".to_string(),
            slug: slug.to_string(),
        }
        .validate()
        .is_ok()
    }

    /// `POST /organizations`: its request rules (3-50, `SLUG_REGEX`, not
    /// reserved), then `validate_slug`.
    fn create_accepts(slug: &str) -> bool {
        use validator::Validate;
        CreateOrganizationRequest {
            name: "Valid Name".to_string(),
            slug: slug.to_string(),
            billing_email: None,
        }
        .validate()
        .is_ok()
            && validate_slug(slug).is_ok()
    }

    /// What `check-slug` calls valid, before the reserved and taken checks.
    fn check_slug_valid(slug: &str) -> bool {
        validate_signup_slug_length(slug)
            .and_then(|()| validate_slug(slug))
            .is_ok()
    }

    /// W9-R1: `check-slug` used to take 1-100 characters, signup 3-50, so a
    /// 1-2 or 51-100 character slug was green at the pre-check and a 400 at
    /// signup. Both rules now agree at every length.
    #[test]
    fn test_check_slug_length_matches_signup() {
        for len in (0..=60).chain([99, 100, 101]) {
            let slug = "a".repeat(len);
            assert_eq!(
                check_slug_valid(&slug),
                signup_accepts(&slug),
                "length {len}: check-slug and signup disagree"
            );
            assert_eq!(check_slug_valid(&slug), (3..=50).contains(&len), "{len}");
        }
        let err = validate_signup_slug_length("ab").unwrap_err().to_string();
        assert!(err.contains("Slug must be 3-50 characters"), "{err}");
        // The character rules are still check-slug's own.
        assert!(!check_slug_valid("ab_"));
        assert!(!check_slug_valid("-abc"));
    }

    /// A generated slug must pass every rule a slug meets later: check-slug,
    /// signup and `POST /organizations` (which also wants a leading letter and
    /// no `--`), and must not be reserved.
    fn assert_accepted_everywhere(slug: &str, from: &str) {
        assert!(check_slug_valid(slug), "{from:?} -> {slug:?}: check-slug");
        assert!(signup_accepts(slug), "{from:?} -> {slug:?}: signup");
        assert!(
            create_accepts(slug),
            "{from:?} -> {slug:?}: POST /organizations"
        );
        assert!(
            !crate::models::RESERVED_SLUGS.contains(&slug),
            "{from:?} -> {slug:?}: reserved"
        );
    }

    /// The slug generate-slug offers when no organization holds the base.
    fn generated_untaken(name: &str) -> String {
        let base = slug_base_for_name(name);
        if base_needs_suffix(&base) {
            with_random_suffix(&base)
        } else {
            base
        }
    }

    /// W9-R2: `Admin` came back as `admin`, a slug check-slug reports as
    /// reserved and signup refuses.
    #[test]
    fn test_generate_slug_never_offers_a_reserved_slug() {
        for reserved in crate::models::RESERVED_SLUGS {
            let upper = reserved.to_uppercase();
            let capitalized = format!("{}{}", &upper[..1], &reserved[1..]);
            for name in [
                reserved.to_string(),
                upper,
                capitalized,
                format!("  {reserved}!  "),
            ] {
                let base = slug_base_for_name(&name);
                assert_eq!(base, *reserved, "{name:?}");
                assert!(base_needs_suffix(&base), "{name:?}");
                let slug = generated_untaken(&name);
                assert!(slug.starts_with(&format!("{reserved}-")), "{slug}");
                assert_eq!(slug.len(), reserved.len() + SLUG_SUFFIX_LEN, "{slug}");
                assert_accepted_everywhere(&slug, &name);
            }
        }
        // A name that only contains a reserved word is offered as it is.
        assert_eq!(generated_untaken("Admin Tools"), "admin-tools");
        assert!(!base_needs_suffix("admin-tools"));
    }

    /// A name with no ASCII letter or digit used to yield an empty slug.
    #[test]
    fn test_generate_slug_without_ascii_letters_is_org_and_a_suffix() {
        for name in ["!!!", "---", "日本語", "Ωμέγα ✓", "·"] {
            assert_eq!(slug_base_for_name(name), "", "{name:?}");
            let slug = generated_untaken(name);
            assert!(slug.starts_with("org-"), "{name:?} -> {slug}");
            assert_eq!(slug.len(), "org".len() + SLUG_SUFFIX_LEN, "{slug}");
            assert!(slug[4..].chars().all(|c| c.is_ascii_hexdigit()), "{slug}");
            assert_accepted_everywhere(&slug, name);
        }
    }

    /// Short and long names fit the 3-50 rule, with or without the suffix.
    #[test]
    fn test_generate_slug_fits_the_signup_length() {
        // Too short to offer bare.
        for name in ["A", "AB", "x!"] {
            let base = slug_base_for_name(name);
            assert!(base_needs_suffix(&base), "{name:?}");
            let slug = generated_untaken(name);
            assert!(slug.starts_with(&format!("{base}-")), "{slug}");
            assert_accepted_everywhere(&slug, name);
        }
        assert_eq!(generated_untaken("ABC"), "abc");
        assert_eq!(generated_untaken("3M Labs"), "org-3m-labs");
        assert_eq!(
            generated_untaken("  My   Company, Inc.  "),
            "my-company-inc"
        );

        // Long names: the bare base and the suffixed form both stay within 50,
        // and a cut never leaves a trailing hyphen.
        let long_names = [
            "a".repeat(200),
            "word ".repeat(40),
            format!("{} tail", "x".repeat(42)),
            format!("{} tail", "y".repeat(49)),
            "1234567890 ".repeat(18),
        ];
        for name in &long_names {
            let base = slug_base_for_name(name);
            assert!(base.len() <= SLUG_MAX_LEN, "{base}");
            assert_accepted_everywhere(&base, name);
            let suffixed = with_random_suffix(&base);
            assert!(suffixed.len() <= SLUG_MAX_LEN, "{suffixed}");
            assert_accepted_everywhere(&suffixed, name);
        }
        // check-slug's suggestion for a taken 50-character slug fits too.
        let suggestion = with_random_suffix(&"z".repeat(50));
        assert_eq!(suggestion.len(), SLUG_MAX_LEN, "{suggestion}");
        assert_accepted_everywhere(&suggestion, "z * 50");
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
