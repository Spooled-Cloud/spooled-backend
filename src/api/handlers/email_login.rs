//! Email login handlers for passwordless authentication
//!
//! This module provides endpoints for:
//! - Starting email login flow (sends 6-digit code)
//! - Verifying email login code (returns JWT tokens)

use axum::{extract::State, Json};
use chrono::{Duration, Utc};
use jsonwebtoken::{encode, EncodingKey, Header};
use rand::RngExt;
use serde::{Deserialize, Serialize};
use tracing::{error, info, warn};
use uuid::Uuid;
use validator::Validate;

use crate::api::middleware::validation::ValidatedJson;
use crate::api::AppState;
use crate::config::EmailProvider;
use crate::error::{AppError, AppResult};

/// Maximum login code attempts before invalidation
const MAX_CODE_ATTEMPTS: i32 = 5;
/// Login code validity in minutes
const CODE_VALIDITY_MINUTES: i64 = 10;
/// Rate limit: max codes per email per hour
const MAX_CODES_PER_HOUR: i64 = 5;
/// Rate limit: max email checks per IP per minute
const MAX_EMAIL_CHECKS_PER_MINUTE: i64 = 10;
/// Rate limit: max email checks per IP per hour
const MAX_EMAIL_CHECKS_PER_HOUR: i64 = 60;

/// Check email availability request
#[derive(Debug, Deserialize, Validate)]
pub struct CheckEmailRequest {
    #[validate(email(message = "Invalid email format"))]
    pub email: String,
}

/// Check email availability response
#[derive(Debug, Serialize)]
pub struct CheckEmailResponse {
    /// Whether the email is available (no account exists)
    pub available: bool,
    /// Whether an account exists with this email
    pub exists: bool,
}

/// Check if an email is already registered
///
/// GET /api/v1/auth/check-email?email=...
///
/// Rate limited to prevent email enumeration attacks.
pub async fn check_email(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
    axum::extract::Query(query): axum::extract::Query<CheckEmailRequest>,
) -> AppResult<Json<CheckEmailResponse>> {
    // The derive was present but `.validate()` was never called, so arbitrary
    // strings reached the query (and, before this rewrite, the database).
    query
        .validate()
        .map_err(|e| AppError::Validation(e.to_string()))?;

    let email = query.email.trim().to_lowercase();

    // Extract IP for rate limiting
    let client_ip = extract_client_ip(&state, &headers);

    // Rate limiting lives in Redis, like every other limiter in this codebase.
    //
    // It used to be implemented by INSERTing a marker row into
    // `email_login_codes` per request (with `code` holding the IP and `email`
    // holding "check:<address>") and then COUNTing those rows twice per request.
    // That made an unauthenticated endpoint write a permanent row per call, into
    // a table it also scanned — so it got slower the more it was abused, and it
    // slowed the login path with it. `verify()` even carries a defensive
    // `code NOT LIKE 'signup:%'` filter because this column overloading has
    // already broken login once.
    check_email_rate_limit(
        &state,
        &client_ip,
        "minute",
        MAX_EMAIL_CHECKS_PER_MINUTE,
        60,
        "Too many requests. Please try again in a minute.",
    )
    .await?;
    check_email_rate_limit(
        &state,
        &client_ip,
        "hour",
        MAX_EMAIL_CHECKS_PER_HOUR,
        3600,
        "Too many requests. Please try again later.",
    )
    .await?;

    // Check if email is associated with any organization
    let exists: Option<(String,)> = sqlx::query_as(
        "SELECT id FROM organizations WHERE billing_email = $1 AND plan_tier <> 'deleted'",
    )
    .bind(&email)
    .fetch_optional(state.db.pool())
    .await?;

    Ok(Json(CheckEmailResponse {
        available: exists.is_none(),
        exists: exists.is_some(),
    }))
}

/// One window of the `check-email` per-IP throttle.
///
/// Fails OPEN when Redis is unavailable, matching the self-host default in
/// `plan_rate_limit` — this endpoint gates signup UX, and breaking signup for
/// every self-hosted deployment without Redis would be worse than the
/// enumeration risk it mitigates. When `RATE_LIMIT_FAIL_CLOSED` is set (hosted
/// SaaS), it denies instead.
async fn check_email_rate_limit(
    state: &AppState,
    client_ip: &str,
    window_label: &str,
    max_requests: i64,
    window_secs: u64,
    message: &str,
) -> AppResult<()> {
    let Some(ref cache) = state.cache else {
        if state.settings.rate_limit.fail_closed_on_redis_error {
            warn!("Redis not configured; denying check-email (RATE_LIMIT_FAIL_CLOSED)");
            return Err(AppError::RateLimit(message.to_string()));
        }
        return Ok(());
    };

    let key = format!("check_email:{}:{}", window_label, client_ip);
    match cache
        .check_rate_limit(&key, max_requests.max(1) as u32, window_secs)
        .await
    {
        Ok(result) if !result.allowed => {
            warn!(ip = %client_ip, window = %window_label, "Email check rate limit exceeded");
            Err(AppError::RateLimit(message.to_string()))
        }
        Ok(_) => Ok(()),
        Err(e) => {
            if state.settings.rate_limit.fail_closed_on_redis_error {
                warn!(error = %e, "Redis error during check-email rate limit; denying (fail-closed)");
                return Err(AppError::RateLimit(message.to_string()));
            }
            warn!(error = %e, "Redis error during check-email rate limit; allowing");
            Ok(())
        }
    }
}

/// Client IP for abuse controls.
///
/// Delegates to the shared, trusted-proxy-aware extractor so this endpoint and
/// the rate-limit middleware agree on who the caller is. This module previously
/// had its own extractor that (correctly) preferred `CF-Connecting-IP` while the
/// middleware's (incorrectly) took the client-supplied leftmost
/// `X-Forwarded-For` — two implementations, and the security-critical one was
/// the weaker.
fn extract_client_ip(state: &AppState, headers: &axum::http::HeaderMap) -> String {
    crate::api::middleware::client_ip::client_ip(headers, &state.settings.trusted_proxy)
}

/// Start email login request
#[derive(Debug, Deserialize, Validate)]
pub struct StartEmailLoginRequest {
    #[validate(email(message = "Invalid email format"))]
    pub email: String,
}

/// Start email login response
#[derive(Debug, Serialize)]
pub struct StartEmailLoginResponse {
    pub message: String,
    /// Email sent to (masked for privacy)
    pub email_sent_to: String,
}

/// Start email login - sends a 6-digit code
///
/// POST /api/v1/auth/email/start
pub async fn start(
    State(state): State<AppState>,
    ValidatedJson(request): ValidatedJson<StartEmailLoginRequest>,
) -> AppResult<Json<StartEmailLoginResponse>> {
    let email = request.email.to_lowercase().trim().to_string();

    // Rate limit: check how many codes sent in the last hour
    let recent_codes: (i64,) = sqlx::query_as(
        r#"
        SELECT COUNT(*) 
        FROM email_login_codes 
        WHERE email = $1 AND created_at > NOW() - INTERVAL '1 hour'
        "#,
    )
    .bind(&email)
    .fetch_one(state.db.pool())
    .await?;

    if recent_codes.0 >= MAX_CODES_PER_HOUR {
        warn!(email = %email, "Email login rate limit exceeded");
        return Err(AppError::RateLimit(
            "Too many login attempts. Please try again later.".to_string(),
        ));
    }

    // Check if email is associated with an organization
    let org: Option<(String,)> = sqlx::query_as(
        "SELECT id FROM organizations WHERE billing_email = $1 AND plan_tier <> 'deleted'",
    )
    .bind(&email)
    .fetch_optional(state.db.pool())
    .await?;

    let org_id = org.map(|(id,)| id);

    // Generate a 6-digit code
    let code: String = {
        let mut rng = rand::rng();
        format!("{:06}", rng.random_range(0..1000000))
    };

    let expires_at = Utc::now() + Duration::minutes(CODE_VALIDITY_MINUTES);

    // Store the code
    sqlx::query(
        r#"
        INSERT INTO email_login_codes (email, code, organization_id, expires_at)
        VALUES ($1, $2, $3, $4)
        "#,
    )
    .bind(&email)
    .bind(&code)
    .bind(&org_id)
    .bind(expires_at)
    .execute(state.db.pool())
    .await?;

    // Send the email
    send_login_code_email(&state, &email, &code).await?;

    // Mask email for response
    let masked_email = mask_email(&email);

    info!(email = %masked_email, "Login code sent");

    Ok(Json(StartEmailLoginResponse {
        message: "Login code sent to your email".to_string(),
        email_sent_to: masked_email,
    }))
}

/// Verify email login request
#[derive(Debug, Deserialize, Validate)]
pub struct VerifyEmailLoginRequest {
    #[validate(email(message = "Invalid email format"))]
    pub email: String,
    #[validate(length(equal = 6, message = "Code must be 6 digits"))]
    pub code: String,
}

/// Verify email login response (same as API key login)
#[derive(Debug, Serialize)]
pub struct VerifyEmailLoginResponse {
    pub access_token: String,
    pub refresh_token: String,
    pub token_type: String,
    pub expires_in: i64,
    pub refresh_expires_in: i64,
}

/// Verify email response for signup flow (no existing account)
#[derive(Debug, Serialize)]
pub struct VerifyEmailSignupResponse {
    /// Signup token to complete registration (short-lived)
    pub signup_token: String,
    /// The verified email
    pub email: String,
    /// Token expiration in seconds
    pub expires_in: i64,
}

/// Combined response for email verification
#[derive(Debug, Serialize)]
#[serde(tag = "type")]
pub enum VerifyEmailResponse {
    /// Existing account - logged in
    #[serde(rename = "login")]
    Login(VerifyEmailLoginResponse),
    /// New email - signup needed
    #[serde(rename = "signup")]
    Signup(VerifyEmailSignupResponse),
}

/// JWT Claims structure (same as auth.rs)
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Claims {
    sub: String,
    api_key_id: String,
    org_id: String,
    iat: i64,
    exp: i64,
    nbf: i64,
    jti: String,
    queues: Vec<String>,
    token_type: String,
    /// Email used for login (for email-based auth)
    email: Option<String>,
}

/// Verify email login - validates code and returns JWT or signup token
///
/// POST /api/v1/auth/email/verify
///
/// Returns:
/// - If email has an existing organization: login tokens
/// - If email is new: signup token to complete registration
pub async fn verify(
    State(state): State<AppState>,
    ValidatedJson(request): ValidatedJson<VerifyEmailLoginRequest>,
) -> AppResult<Json<VerifyEmailResponse>> {
    let email = request.email.to_lowercase().trim().to_string();
    let code = request.code.trim().to_string();

    // Consider EVERY live code for this address, not just the newest.
    //
    // Selecting only the newest coupled two unrelated things: issuing a code and
    // invalidating the previous one. Because `start()` is unauthenticated and
    // keyed solely on someone else's address, anyone who knew a victim's email
    // could call it to supersede the code the victim had just received — the
    // victim's code would then never be the newest and could never verify. After
    // MAX_CODES_PER_HOUR issuances the victim also could not obtain a fresh one,
    // producing an hour-long login lockout repeatable indefinitely.
    //
    // The per-code attempt cap is unchanged (each row still allows at most
    // MAX_CODE_ATTEMPTS), so this grants the attacker no extra guesses: the
    // ceiling is still MAX_CODES_PER_HOUR × MAX_CODE_ATTEMPTS per address.
    let candidates: Vec<(Uuid, String, Option<String>, i32)> = sqlx::query_as(
        r#"
        SELECT id, code, organization_id, attempts
        FROM email_login_codes
        WHERE email = $1
          AND expires_at > NOW()
          AND used_at IS NULL
          -- Signup tokens ("signup:<token>") live in this same table; excluding them
          -- stops a pending signup row from shadowing the real 6-digit login code.
          AND code NOT LIKE 'signup:%'
        ORDER BY created_at DESC
        LIMIT $2
        "#,
    )
    .bind(&email)
    .bind(MAX_CODES_PER_HOUR)
    .fetch_all(state.db.pool())
    .await?;

    if candidates.is_empty() {
        warn!(email = %mask_email(&email), "No valid login code found");
        return Err(AppError::Authentication(
            "Invalid or expired code. Please request a new one.".to_string(),
        ));
    }

    // Per-ADDRESS attempt budget.
    //
    // Comparing a guess against every live code (which is what stops a third
    // party from invalidating the victim's code) would otherwise multiply the
    // attacker's reach: with five codes outstanding, one request tests five
    // values while burning one per-code attempt. This budget makes the total
    // number of guesses against an address independent of how many codes exist.
    //
    // Deliberately NOT implemented by charging every candidate row: that would
    // let an attacker burn all live codes with five wrong guesses and re-create
    // the lockout this change exists to remove.
    if let Some(ref cache) = state.cache {
        let key = format!("email_verify_attempts:{}", email);
        let window_secs = (CODE_VALIDITY_MINUTES.max(1) as u64) * 60;
        match cache
            .check_rate_limit(&key, MAX_CODE_ATTEMPTS.max(1) as u32, window_secs)
            .await
        {
            Ok(result) if !result.allowed => {
                warn!(email = %mask_email(&email), "Login verify attempt budget exhausted");
                return Err(AppError::Authentication(
                    "Too many attempts. Please request a new code.".to_string(),
                ));
            }
            Ok(_) => {}
            Err(e) => {
                // Redis unavailable: the per-code cap below is still enforced in
                // Postgres, so brute force stays bounded — just less tightly.
                warn!(error = %e, "Login verify attempt budget unavailable; relying on per-code cap");
            }
        }
    }

    // Constant-time compare against each candidate. On a match we charge the
    // attempt to THAT row; with no match we charge the newest, so a wrong guess
    // still burns exactly one attempt and the cap cannot be sidestepped by
    // spreading guesses across rows.
    let matched = candidates
        .iter()
        .find(|(_, stored, _, _)| constant_time_compare(&code, stored));

    let (code_id, stored_code, org_id, _attempts) = match matched {
        Some(hit) => hit.clone(),
        None => candidates[0].clone(),
    };

    // Atomically consume one attempt. Combining the cap check and the increment into a
    // single conditional UPDATE (guarded by `attempts < MAX`) is what makes the cap safe
    // under concurrency: the previous read-then-check-then-increment let many parallel
    // verifications all read a low `attempts`, all pass the guard, and all get a guess —
    // bypassing the per-code lockout and amplifying brute force against the code space.
    let new_attempts: Option<(i32,)> = sqlx::query_as(
        "UPDATE email_login_codes SET attempts = attempts + 1 \
         WHERE id = $1 AND attempts < $2 RETURNING attempts",
    )
    .bind(code_id)
    .bind(MAX_CODE_ATTEMPTS)
    .fetch_optional(state.db.pool())
    .await?;

    let Some((attempts_now,)) = new_attempts else {
        // Cap already reached (this row's attempts are >= MAX): invalidate and reject.
        sqlx::query("UPDATE email_login_codes SET used_at = NOW() WHERE id = $1")
            .bind(code_id)
            .execute(state.db.pool())
            .await?;

        return Err(AppError::Authentication(
            "Too many attempts. Please request a new code.".to_string(),
        ));
    };

    // SECURITY: Use constant-time comparison to prevent timing attacks
    // An attacker could otherwise determine the correct code digit-by-digit
    if !constant_time_compare(&code, &stored_code) {
        warn!(email = %mask_email(&email), attempts = attempts_now, "Invalid login code");
        return Err(AppError::Authentication("Invalid code".to_string()));
    }

    // Mark code as used
    sqlx::query("UPDATE email_login_codes SET used_at = NOW() WHERE id = $1")
        .bind(code_id)
        .execute(state.db.pool())
        .await?;

    // Check if organization exists
    match org_id {
        Some(existing_org_id) => {
            // Soft-deleted orgs must not be revived via a pre-delete login code.
            // `start()` refuses new codes for deleted orgs, but a code issued before
            // soft-delete still carries organization_id; without this check,
            // get_or_create_email_api_key would mint a fresh active key and restore access.
            let org_tier: Option<(String,)> =
                sqlx::query_as("SELECT plan_tier FROM organizations WHERE id = $1")
                    .bind(&existing_org_id)
                    .fetch_optional(state.db.pool())
                    .await?;
            match org_tier.as_ref().map(|(t,)| t.as_str()) {
                Some("deleted") | None => {
                    warn!(
                        email = %mask_email(&email),
                        org_id = %existing_org_id,
                        "Rejecting email verify for missing or soft-deleted organization"
                    );
                    return Err(AppError::Authentication(
                        "Invalid or expired code. Please request a new one.".to_string(),
                    ));
                }
                Some(_) => {}
            }

            // Existing user - return login tokens
            let api_key_id = get_or_create_email_api_key(&state, &existing_org_id, &email).await?;

            let now = Utc::now();
            let access_expiration = state.settings.jwt.expiration_hours as i64;
            let refresh_expiration = access_expiration * 24;

            let access_claims = Claims {
                sub: existing_org_id.clone(),
                api_key_id: api_key_id.clone(),
                org_id: existing_org_id.clone(),
                iat: now.timestamp(),
                exp: (now + Duration::hours(access_expiration)).timestamp(),
                nbf: now.timestamp(),
                jti: Uuid::new_v4().to_string(),
                queues: vec!["*".to_string()],
                token_type: "access".to_string(),
                email: Some(email.clone()),
            };

            let refresh_claims = Claims {
                sub: existing_org_id.clone(),
                api_key_id: api_key_id.clone(),
                org_id: existing_org_id.clone(),
                iat: now.timestamp(),
                exp: (now + Duration::hours(refresh_expiration)).timestamp(),
                nbf: now.timestamp(),
                jti: Uuid::new_v4().to_string(),
                queues: vec!["*".to_string()],
                token_type: "refresh".to_string(),
                email: Some(email.clone()),
            };

            let access_token = encode(
                &Header::default(),
                &access_claims,
                &EncodingKey::from_secret(state.settings.jwt.secret.as_bytes()),
            )
            .map_err(|e| {
                error!(error = %e, "Failed to encode access token");
                AppError::Internal("Failed to generate token".to_string())
            })?;

            let refresh_token = encode(
                &Header::default(),
                &refresh_claims,
                &EncodingKey::from_secret(state.settings.jwt.secret.as_bytes()),
            )
            .map_err(|e| {
                error!(error = %e, "Failed to encode refresh token");
                AppError::Internal("Failed to generate token".to_string())
            })?;

            info!(email = %mask_email(&email), org_id = %existing_org_id, "Email login successful");

            Ok(Json(VerifyEmailResponse::Login(VerifyEmailLoginResponse {
                access_token,
                refresh_token,
                token_type: "Bearer".to_string(),
                expires_in: access_expiration * 3600,
                refresh_expires_in: refresh_expiration * 3600,
            })))
        }
        None => {
            // New user - return signup token
            let signup_token = generate_signup_token(&state, &email).await?;

            info!(email = %mask_email(&email), "Email verified, signup token issued");

            Ok(Json(VerifyEmailResponse::Signup(
                VerifyEmailSignupResponse {
                    signup_token,
                    email: email.clone(),
                    expires_in: 900, // 15 minutes
                },
            )))
        }
    }
}

/// Signup token validity in minutes
const SIGNUP_TOKEN_VALIDITY_MINUTES: i64 = 15;

/// Generate a short-lived signup token for verified emails
async fn generate_signup_token(state: &AppState, email: &str) -> AppResult<String> {
    let token = Uuid::new_v4().to_string();
    let expires_at = Utc::now() + Duration::minutes(SIGNUP_TOKEN_VALIDITY_MINUTES);

    // Store in email_login_codes table with a special marker
    sqlx::query(
        r#"
        INSERT INTO email_login_codes (email, code, expires_at, used_at)
        VALUES ($1, $2, $3, NULL)
        "#,
    )
    .bind(email)
    .bind(format!("signup:{}", token))
    .bind(expires_at)
    .execute(state.db.pool())
    .await?;

    Ok(token)
}

/// Complete signup request
#[derive(Debug, Deserialize, Validate)]
pub struct CompleteSignupRequest {
    /// Signup token from email verification
    pub signup_token: String,
    /// Organization name
    #[validate(length(min = 3, max = 100, message = "Name must be 3-100 characters"))]
    pub name: String,
    /// Organization slug
    #[validate(length(min = 3, max = 50, message = "Slug must be 3-50 characters"))]
    pub slug: String,
}

/// Complete signup response
#[derive(Debug, Serialize)]
pub struct CompleteSignupResponse {
    /// The created organization
    pub organization: SignupOrganization,
    /// API key (only shown once!)
    pub api_key: String,
    /// Access token for immediate login
    pub access_token: String,
    /// Refresh token
    pub refresh_token: String,
    pub token_type: String,
    pub expires_in: i64,
}

/// Organization info in signup response
#[derive(Debug, Serialize)]
pub struct SignupOrganization {
    pub id: String,
    pub name: String,
    pub slug: String,
    pub billing_email: String,
}

/// Complete signup with verified email
///
/// POST /api/v1/auth/signup/complete
pub async fn complete_signup(
    State(state): State<AppState>,
    ValidatedJson(request): ValidatedJson<CompleteSignupRequest>,
) -> AppResult<(axum::http::StatusCode, Json<CompleteSignupResponse>)> {
    // Registration gate: self-serve email signup creates a brand-new org + API key, so
    // it must honor the same registration controls as POST /organizations (which the
    // email path previously ignored — `email_signup_enabled` was defined but never
    // enforced). Reject when email signup is disabled or registration is not open.
    if !state.settings.registration.email_signup_enabled
        || !matches!(
            state.settings.registration.mode,
            crate::config::RegistrationMode::Open
        )
    {
        return Err(AppError::Authorization(
            "Email signup is currently disabled".to_string(),
        ));
    }

    // Validate signup token
    let token_code = format!("signup:{}", request.signup_token);

    let signup_record: Option<(Uuid, String)> = sqlx::query_as(
        r#"
        SELECT id, email
        FROM email_login_codes
        WHERE code = $1 
          AND expires_at > NOW()
          AND used_at IS NULL
        "#,
    )
    .bind(&token_code)
    .fetch_optional(state.db.pool())
    .await?;

    let Some((record_id, email)) = signup_record else {
        return Err(AppError::Authentication(
            "Invalid or expired signup token. Please verify your email again.".to_string(),
        ));
    };

    // NOTE: the one-time token is consumed further below, only AFTER slug/duplicate
    // validation passes — burning it here meant a bad slug or a taken email/slug left
    // the user unable to retry without re-verifying their email.

    // Validate slug format
    let slug = request.slug.to_lowercase().trim().to_string();
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

    // Check for duplicate email (race condition guard)
    let existing_email: Option<(String,)> = sqlx::query_as(
        "SELECT id FROM organizations WHERE billing_email = $1 AND plan_tier <> 'deleted'",
    )
    .bind(&email)
    .fetch_optional(state.db.pool())
    .await?;

    if existing_email.is_some() {
        return Err(AppError::Conflict(
            "An account with this email already exists.".to_string(),
        ));
    }

    // Check for duplicate slug
    let existing_slug: Option<(String,)> =
        sqlx::query_as("SELECT id FROM organizations WHERE slug = $1")
            .bind(&slug)
            .fetch_optional(state.db.pool())
            .await?;

    if existing_slug.is_some() {
        return Err(AppError::Conflict(
            "This slug is already taken. Please choose another.".to_string(),
        ));
    }

    // Consume the one-time signup token now that validation has passed. The conditional
    // UPDATE (used_at IS NULL) also blocks a concurrent double-use of the same token.
    let consumed: Option<(Uuid,)> = sqlx::query_as(
        "UPDATE email_login_codes SET used_at = NOW() WHERE id = $1 AND used_at IS NULL RETURNING id",
    )
    .bind(record_id)
    .fetch_optional(state.db.pool())
    .await?;
    if consumed.is_none() {
        return Err(AppError::Authentication(
            "Invalid or expired signup token. Please verify your email again.".to_string(),
        ));
    }

    // Create organization with auto-generated webhook token
    let org_id = Uuid::new_v4().to_string();
    let now = Utc::now();

    // Generate a secure webhook token for incoming webhooks
    let webhook_token = generate_webhook_token();
    let initial_settings = serde_json::json!({
        "webhook_token": webhook_token
    });

    sqlx::query(
        r#"
        INSERT INTO organizations (id, name, slug, plan_tier, billing_email, settings, created_at, updated_at)
        VALUES ($1, $2, $3, 'free', $4, $5, $6, $6)
        "#,
    )
    .bind(&org_id)
    .bind(&request.name)
    .bind(&slug)
    .bind(&email)
    .bind(&initial_settings)
    .bind(now)
    .execute(state.db.pool())
    .await
    .map_err(|e| {
        if e.to_string().contains("duplicate key") {
            AppError::Conflict("Organization creation failed due to conflict".to_string())
        } else {
            AppError::Database(e)
        }
    })?;

    // Create API key
    let api_key_id = Uuid::new_v4().to_string();
    // Uses sp_ prefix (new standard); sk_ is legacy and only accepted for backward compatibility
    let raw_key = format!(
        "sp_{}_{}",
        if state.settings.server.environment == crate::config::Environment::Production {
            "live"
        } else {
            "test"
        },
        generate_api_key_string()
    );
    let key_prefix: String = raw_key.chars().take(8).collect();
    let key_hash = bcrypt::hash(&raw_key, bcrypt::DEFAULT_COST)
        .map_err(|e| AppError::Internal(format!("Failed to hash API key: {}", e)))?;
    let lookup_hash = crate::models::api_key_lookup_hash(&raw_key);

    sqlx::query(
        r#"
        INSERT INTO api_keys (id, organization_id, key_hash, key_prefix, name, queues, is_active, created_at, lookup_hash)
        VALUES ($1, $2, $3, $4, 'Initial Admin Key', ARRAY['*'], TRUE, $5, $6)
        "#,
    )
    .bind(&api_key_id)
    .bind(&org_id)
    .bind(&key_hash)
    .bind(&key_prefix)
    .bind(now)
    .bind(&lookup_hash)
    .execute(state.db.pool())
    .await?;

    // Generate JWT tokens
    let access_expiration = state.settings.jwt.expiration_hours as i64;

    let access_claims = Claims {
        sub: org_id.clone(),
        api_key_id: api_key_id.clone(),
        org_id: org_id.clone(),
        iat: now.timestamp(),
        exp: (now + Duration::hours(access_expiration)).timestamp(),
        nbf: now.timestamp(),
        jti: Uuid::new_v4().to_string(),
        queues: vec!["*".to_string()],
        token_type: "access".to_string(),
        email: Some(email.clone()),
    };

    let refresh_claims = Claims {
        sub: org_id.clone(),
        api_key_id: api_key_id.clone(),
        org_id: org_id.clone(),
        iat: now.timestamp(),
        exp: (now + Duration::hours(access_expiration * 24)).timestamp(),
        nbf: now.timestamp(),
        jti: Uuid::new_v4().to_string(),
        queues: vec!["*".to_string()],
        token_type: "refresh".to_string(),
        email: Some(email.clone()),
    };

    let access_token = encode(
        &Header::default(),
        &access_claims,
        &EncodingKey::from_secret(state.settings.jwt.secret.as_bytes()),
    )
    .map_err(|e| {
        error!(error = %e, "Failed to encode access token");
        AppError::Internal("Failed to generate token".to_string())
    })?;

    let refresh_token = encode(
        &Header::default(),
        &refresh_claims,
        &EncodingKey::from_secret(state.settings.jwt.secret.as_bytes()),
    )
    .map_err(|e| {
        error!(error = %e, "Failed to encode refresh token");
        AppError::Internal("Failed to generate token".to_string())
    })?;

    info!(email = %mask_email(&email), org_id = %org_id, "Signup completed");

    Ok((
        axum::http::StatusCode::CREATED,
        Json(CompleteSignupResponse {
            organization: SignupOrganization {
                id: org_id,
                name: request.name,
                slug,
                billing_email: email,
            },
            api_key: raw_key,
            access_token,
            refresh_token,
            token_type: "Bearer".to_string(),
            expires_in: access_expiration * 3600,
        }),
    ))
}

/// Generate a secure random API key string
fn generate_api_key_string() -> String {
    use rand::RngExt;
    let mut rng = rand::rng();
    let mut bytes = [0u8; 32];
    rng.fill(&mut bytes);
    base64::Engine::encode(&base64::engine::general_purpose::URL_SAFE_NO_PAD, bytes)
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

/// Send login code email
async fn send_login_code_email(state: &AppState, email: &str, code: &str) -> AppResult<()> {
    let subject = "Your Spooled Cloud Login Code";
    let body = format!(
        r#"Hi,

Your login code for Spooled Cloud is: {}

This code will expire in {} minutes.

If you didn't request this code, you can safely ignore this email.

Best,
The Spooled Cloud Team"#,
        code, CODE_VALIDITY_MINUTES
    );

    let html_body = format!(
        r#"<!DOCTYPE html>
<html>
<head><meta charset="UTF-8"></head>
<body style="font-family: -apple-system, BlinkMacSystemFont, 'Segoe UI', Roboto, sans-serif; max-width: 600px; margin: 0 auto; padding: 20px;">
    <div style="text-align: center; margin-bottom: 30px;">
        <h1 style="color: #10b981; margin: 0;">Spooled Cloud</h1>
    </div>
    <p>Hi,</p>
    <p>Your login code is:</p>
    <div style="text-align: center; margin: 30px 0;">
        <span style="font-size: 32px; font-weight: bold; letter-spacing: 8px; background: #f3f4f6; padding: 16px 32px; border-radius: 8px; font-family: monospace;">{}</span>
    </div>
    <p style="color: #6b7280; font-size: 14px;">This code will expire in {} minutes.</p>
    <p style="color: #6b7280; font-size: 14px;">If you didn't request this code, you can safely ignore this email.</p>
    <hr style="border: none; border-top: 1px solid #e5e7eb; margin: 30px 0;">
    <p style="color: #9ca3af; font-size: 12px; text-align: center;">
        Spooled Cloud - Distributed Job Queue
    </p>
</body>
</html>"#,
        code, CODE_VALIDITY_MINUTES
    );

    match state.settings.email.provider {
        EmailProvider::Console => {
            // Development: log to console
            info!(
                email = %email,
                code = %code,
                "LOGIN CODE EMAIL (console mode - not sent)"
            );
            println!("\n========== LOGIN CODE EMAIL ==========");
            println!("To: {}", email);
            println!("Subject: {}", subject);
            println!("Code: {}", code);
            println!("=======================================\n");
        }
        EmailProvider::Resend => {
            send_via_resend(state, email, subject, &body, &html_body).await?;
        }
        EmailProvider::Sendgrid => {
            send_via_sendgrid(state, email, subject, &body, &html_body).await?;
        }
        EmailProvider::Postmark => {
            send_via_postmark(state, email, subject, &body, &html_body).await?;
        }
        EmailProvider::Smtp => {
            send_via_smtp(state, email, subject, &body, &html_body).await?;
        }
    }

    Ok(())
}

/// Send email via Resend
async fn send_via_resend(
    state: &AppState,
    to: &str,
    subject: &str,
    text: &str,
    html: &str,
) -> AppResult<()> {
    let api_key =
        state.settings.email.api_key.as_ref().ok_or_else(|| {
            AppError::Internal("EMAIL_API_KEY not configured for Resend".to_string())
        })?;

    let client = reqwest::Client::new();
    let response = client
        .post("https://api.resend.com/emails")
        .header("Authorization", format!("Bearer {}", api_key))
        .json(&serde_json::json!({
            "from": format!("{} <{}>", state.settings.email.from_name, state.settings.email.from_address),
            "to": [to],
            "subject": subject,
            "text": text,
            "html": html
        }))
        .send()
        .await
        .map_err(|e| AppError::Internal(format!("Failed to send email: {}", e)))?;

    if !response.status().is_success() {
        let error = response.text().await.unwrap_or_default();
        error!(error = %error, "Resend email failed");
        return Err(AppError::Internal("Failed to send email".to_string()));
    }

    Ok(())
}

/// Send email via SendGrid
async fn send_via_sendgrid(
    state: &AppState,
    to: &str,
    subject: &str,
    text: &str,
    html: &str,
) -> AppResult<()> {
    let api_key = state.settings.email.api_key.as_ref().ok_or_else(|| {
        AppError::Internal("EMAIL_API_KEY not configured for SendGrid".to_string())
    })?;

    let client = reqwest::Client::new();
    let response = client
        .post("https://api.sendgrid.com/v3/mail/send")
        .header("Authorization", format!("Bearer {}", api_key))
        .json(&serde_json::json!({
            "personalizations": [{
                "to": [{"email": to}]
            }],
            "from": {
                "email": state.settings.email.from_address,
                "name": state.settings.email.from_name
            },
            "subject": subject,
            "content": [
                {"type": "text/plain", "value": text},
                {"type": "text/html", "value": html}
            ]
        }))
        .send()
        .await
        .map_err(|e| AppError::Internal(format!("Failed to send email: {}", e)))?;

    if !response.status().is_success() {
        let error = response.text().await.unwrap_or_default();
        error!(error = %error, "SendGrid email failed");
        return Err(AppError::Internal("Failed to send email".to_string()));
    }

    Ok(())
}

/// Send email via Postmark
async fn send_via_postmark(
    state: &AppState,
    to: &str,
    subject: &str,
    text: &str,
    html: &str,
) -> AppResult<()> {
    let api_key = state.settings.email.api_key.as_ref().ok_or_else(|| {
        AppError::Internal("EMAIL_API_KEY not configured for Postmark".to_string())
    })?;

    let client = reqwest::Client::new();
    let response = client
        .post("https://api.postmarkapp.com/email")
        .header("X-Postmark-Server-Token", api_key)
        .header("Accept", "application/json")
        .json(&serde_json::json!({
            "From": format!("{} <{}>", state.settings.email.from_name, state.settings.email.from_address),
            "To": to,
            "Subject": subject,
            "TextBody": text,
            "HtmlBody": html
        }))
        .send()
        .await
        .map_err(|e| AppError::Internal(format!("Failed to send email: {}", e)))?;

    if !response.status().is_success() {
        let error = response.text().await.unwrap_or_default();
        error!(error = %error, "Postmark email failed");
        return Err(AppError::Internal("Failed to send email".to_string()));
    }

    Ok(())
}

/// Send email via SMTP (using lettre)
async fn send_via_smtp(
    state: &AppState,
    to: &str,
    subject: &str,
    text: &str,
    html: &str,
) -> AppResult<()> {
    use lettre::{
        message::{header::ContentType, Mailbox, MultiPart, SinglePart},
        transport::smtp::authentication::Credentials,
        AsyncSmtpTransport, AsyncTransport, Message, Tokio1Executor,
    };

    let smtp_host = state
        .settings
        .email
        .smtp_host
        .as_ref()
        .ok_or_else(|| AppError::Internal("SMTP_HOST not configured".to_string()))?;

    let smtp_port = state.settings.email.smtp_port.unwrap_or(587);

    let from_mailbox: Mailbox = format!(
        "{} <{}>",
        state.settings.email.from_name, state.settings.email.from_address
    )
    .parse()
    .map_err(|e| AppError::Internal(format!("Invalid from address: {}", e)))?;

    let to_mailbox: Mailbox = to
        .parse()
        .map_err(|e| AppError::Internal(format!("Invalid to address: {}", e)))?;

    let email = Message::builder()
        .from(from_mailbox)
        .to(to_mailbox)
        .subject(subject)
        .multipart(
            MultiPart::alternative()
                .singlepart(
                    SinglePart::builder()
                        .header(ContentType::TEXT_PLAIN)
                        .body(text.to_string()),
                )
                .singlepart(
                    SinglePart::builder()
                        .header(ContentType::TEXT_HTML)
                        .body(html.to_string()),
                ),
        )
        .map_err(|e| AppError::Internal(format!("Failed to build email: {}", e)))?;

    // Build SMTP transport
    let transport = if let (Some(username), Some(password)) = (
        state.settings.email.smtp_username.as_ref(),
        state.settings.email.smtp_password.as_ref(),
    ) {
        let creds = Credentials::new(username.clone(), password.clone());
        AsyncSmtpTransport::<Tokio1Executor>::starttls_relay(smtp_host)
            .map_err(|e| AppError::Internal(format!("Failed to create SMTP transport: {}", e)))?
            .port(smtp_port)
            .credentials(creds)
            .build()
    } else {
        // No auth
        AsyncSmtpTransport::<Tokio1Executor>::starttls_relay(smtp_host)
            .map_err(|e| AppError::Internal(format!("Failed to create SMTP transport: {}", e)))?
            .port(smtp_port)
            .build()
    };

    transport
        .send(email)
        .await
        .map_err(|e| AppError::Internal(format!("Failed to send email via SMTP: {}", e)))?;

    info!(to = %to, "Email sent via SMTP");

    Ok(())
}

/// Get or create API key for email login
async fn get_or_create_email_api_key(
    state: &AppState,
    org_id: &str,
    email: &str,
) -> AppResult<String> {
    // Check for existing email-based API key
    let existing: Option<(String,)> = sqlx::query_as(
        r#"
        SELECT id FROM api_keys 
        WHERE organization_id = $1 
          AND name = 'Email Login'
          AND is_active = TRUE
        LIMIT 1
        "#,
    )
    .bind(org_id)
    .fetch_optional(state.db.pool())
    .await?;

    if let Some((id,)) = existing {
        return Ok(id);
    }

    // Create new API key
    let key_id = Uuid::new_v4().to_string();
    let now = Utc::now();

    // Generate a random key (we won't expose it, just for internal use)
    // Uses sp_ prefix (new standard)
    let raw_key = format!("sp_email_{}", Uuid::new_v4().to_string().replace('-', ""));
    let key_hash = bcrypt::hash(&raw_key, 10)
        .map_err(|e| AppError::Internal(format!("Failed to hash key: {}", e)))?;
    let key_prefix = raw_key.chars().take(8).collect::<String>();
    let lookup_hash = crate::models::api_key_lookup_hash(&raw_key);

    sqlx::query(
        r#"
        INSERT INTO api_keys (id, organization_id, name, key_hash, key_prefix, queues, is_active, created_at, lookup_hash)
        VALUES ($1, $2, 'Email Login', $3, $4, ARRAY['*'], TRUE, $5, $6)
        "#,
    )
    .bind(&key_id)
    .bind(org_id)
    .bind(&key_hash)
    .bind(&key_prefix)
    .bind(now)
    .bind(&lookup_hash)
    .execute(state.db.pool())
    .await?;

    info!(
        org_id = %org_id,
        email = %mask_email(email),
        "Created API key for email login"
    );

    Ok(key_id)
}

/// Constant-time string comparison to prevent timing attacks
fn constant_time_compare(a: &str, b: &str) -> bool {
    use subtle::ConstantTimeEq;

    // If lengths differ, the values are definitely different
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

/// Mask email for privacy in logs and responses
fn mask_email(email: &str) -> String {
    let parts: Vec<&str> = email.split('@').collect();
    if parts.len() != 2 {
        return "***@***".to_string();
    }

    let local = parts[0];
    let domain = parts[1];

    let masked_local = if local.len() <= 2 {
        "*".repeat(local.len())
    } else {
        format!("{}***{}", &local[..1], &local[local.len() - 1..])
    };

    format!("{}@{}", masked_local, domain)
}

#[cfg(test)]
mod tests {
    use super::*;
    use validator::Validate;

    #[test]
    fn test_mask_email() {
        assert_eq!(mask_email("john@example.com"), "j***n@example.com");
        assert_eq!(mask_email("ab@example.com"), "**@example.com");
        assert_eq!(mask_email("a@test.com"), "*@test.com");
        assert_eq!(mask_email("test.user@company.co"), "t***r@company.co");
    }

    #[test]
    fn test_mask_email_invalid() {
        assert_eq!(mask_email("invalid"), "***@***");
        assert_eq!(mask_email(""), "***@***");
    }

    #[test]
    fn test_mask_email_edge_cases() {
        // Three character local part
        assert_eq!(mask_email("abc@example.com"), "a***c@example.com");
        // Email with dots
        assert_eq!(mask_email("first.last@domain.com"), "f***t@domain.com");
        // Email with plus sign
        assert_eq!(mask_email("user+tag@gmail.com"), "u***g@gmail.com");
    }

    #[test]
    fn test_start_email_login_request_validation() {
        // Valid email
        let valid = StartEmailLoginRequest {
            email: "test@example.com".to_string(),
        };
        assert!(valid.validate().is_ok());

        // Invalid email
        let invalid = StartEmailLoginRequest {
            email: "not-an-email".to_string(),
        };
        assert!(invalid.validate().is_err());

        // Empty email
        let empty = StartEmailLoginRequest {
            email: "".to_string(),
        };
        assert!(empty.validate().is_err());
    }

    #[test]
    fn test_verify_email_login_request_validation() {
        // Valid request
        let valid = VerifyEmailLoginRequest {
            email: "test@example.com".to_string(),
            code: "123456".to_string(),
        };
        assert!(valid.validate().is_ok());

        // Invalid email
        let invalid_email = VerifyEmailLoginRequest {
            email: "invalid".to_string(),
            code: "123456".to_string(),
        };
        assert!(invalid_email.validate().is_err());

        // Code too short
        let short_code = VerifyEmailLoginRequest {
            email: "test@example.com".to_string(),
            code: "12345".to_string(),
        };
        assert!(short_code.validate().is_err());

        // Code too long
        let long_code = VerifyEmailLoginRequest {
            email: "test@example.com".to_string(),
            code: "1234567".to_string(),
        };
        assert!(long_code.validate().is_err());
    }

    #[test]
    fn test_start_email_login_response_serialization() {
        let response = StartEmailLoginResponse {
            message: "Code sent".to_string(),
            email_sent_to: "t***t@example.com".to_string(),
        };

        let json = serde_json::to_string(&response).unwrap();
        assert!(json.contains("Code sent"));
        assert!(json.contains("t***t@example.com"));
    }

    #[test]
    fn test_verify_email_login_response_serialization() {
        let response = VerifyEmailLoginResponse {
            access_token: "eyJ...".to_string(),
            refresh_token: "eyJ...".to_string(),
            token_type: "Bearer".to_string(),
            expires_in: 86400,
            refresh_expires_in: 2592000,
        };

        let json = serde_json::to_string(&response).unwrap();
        assert!(json.contains("Bearer"));
        assert!(json.contains("access_token"));
        assert!(json.contains("refresh_token"));
        assert!(json.contains("86400"));
    }

    #[test]
    fn test_claims_serialization() {
        let claims = Claims {
            sub: "org-123".to_string(),
            api_key_id: "key-456".to_string(),
            org_id: "org-123".to_string(),
            iat: 1700000000,
            exp: 1700003600,
            nbf: 1700000000,
            jti: "jti-789".to_string(),
            queues: vec!["*".to_string()],
            token_type: "access".to_string(),
            email: Some("test@example.com".to_string()),
        };

        let json = serde_json::to_string(&claims).unwrap();
        assert!(json.contains("org-123"));
        assert!(json.contains("test@example.com"));
        assert!(json.contains("access"));

        // Deserialize back
        let decoded: Claims = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.sub, claims.sub);
        assert_eq!(decoded.email, claims.email);
    }

    #[test]
    fn test_claims_without_email() {
        let claims = Claims {
            sub: "org-123".to_string(),
            api_key_id: "key-456".to_string(),
            org_id: "org-123".to_string(),
            iat: 1700000000,
            exp: 1700003600,
            nbf: 1700000000,
            jti: "jti-789".to_string(),
            queues: vec!["emails".to_string()],
            token_type: "access".to_string(),
            email: None,
        };

        let json = serde_json::to_string(&claims).unwrap();
        // Should still serialize without email
        assert!(json.contains("org-123"));

        let decoded: Claims = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.email, None);
    }

    #[test]
    fn test_check_email_request_validation() {
        // Valid email
        let valid = CheckEmailRequest {
            email: "test@example.com".to_string(),
        };
        assert!(valid.validate().is_ok());

        // Invalid email
        let invalid = CheckEmailRequest {
            email: "not-an-email".to_string(),
        };
        assert!(invalid.validate().is_err());

        // Empty email
        let empty = CheckEmailRequest {
            email: "".to_string(),
        };
        assert!(empty.validate().is_err());
    }

    #[test]
    fn test_check_email_response_serialization() {
        // Email exists
        let exists_response = CheckEmailResponse {
            available: false,
            exists: true,
        };
        let json = serde_json::to_string(&exists_response).unwrap();
        assert!(json.contains("\"available\":false"));
        assert!(json.contains("\"exists\":true"));

        // Email available
        let available_response = CheckEmailResponse {
            available: true,
            exists: false,
        };
        let json = serde_json::to_string(&available_response).unwrap();
        assert!(json.contains("\"available\":true"));
        assert!(json.contains("\"exists\":false"));
    }

    #[test]
    fn test_verify_email_signup_response_serialization() {
        let response = VerifyEmailSignupResponse {
            signup_token: "abc123-token".to_string(),
            email: "test@example.com".to_string(),
            expires_in: 900,
        };

        let json = serde_json::to_string(&response).unwrap();
        assert!(json.contains("abc123-token"));
        assert!(json.contains("test@example.com"));
        assert!(json.contains("900"));
    }

    #[test]
    fn test_verify_email_response_login_variant() {
        let login_response = VerifyEmailResponse::Login(VerifyEmailLoginResponse {
            access_token: "access-token".to_string(),
            refresh_token: "refresh-token".to_string(),
            token_type: "Bearer".to_string(),
            expires_in: 86400,
            refresh_expires_in: 2592000,
        });

        let json = serde_json::to_string(&login_response).unwrap();
        assert!(json.contains("\"type\":\"login\""));
        assert!(json.contains("access-token"));
        assert!(json.contains("Bearer"));
    }

    #[test]
    fn test_verify_email_response_signup_variant() {
        let signup_response = VerifyEmailResponse::Signup(VerifyEmailSignupResponse {
            signup_token: "signup-token-123".to_string(),
            email: "new@user.com".to_string(),
            expires_in: 900,
        });

        let json = serde_json::to_string(&signup_response).unwrap();
        assert!(json.contains("\"type\":\"signup\""));
        assert!(json.contains("signup-token-123"));
        assert!(json.contains("new@user.com"));
    }

    #[test]
    fn test_complete_signup_request_validation() {
        // Valid request
        let valid = CompleteSignupRequest {
            signup_token: "abc123".to_string(),
            name: "My Company".to_string(),
            slug: "my-company".to_string(),
        };
        assert!(valid.validate().is_ok());

        // Name too short
        let short_name = CompleteSignupRequest {
            signup_token: "abc123".to_string(),
            name: "ab".to_string(),
            slug: "my-company".to_string(),
        };
        assert!(short_name.validate().is_err());

        // Slug too short
        let short_slug = CompleteSignupRequest {
            signup_token: "abc123".to_string(),
            name: "My Company".to_string(),
            slug: "ab".to_string(),
        };
        assert!(short_slug.validate().is_err());

        // Name too long (over 100 chars)
        let long_name = CompleteSignupRequest {
            signup_token: "abc123".to_string(),
            name: "a".repeat(101),
            slug: "my-company".to_string(),
        };
        assert!(long_name.validate().is_err());

        // Slug too long (over 50 chars)
        let long_slug = CompleteSignupRequest {
            signup_token: "abc123".to_string(),
            name: "My Company".to_string(),
            slug: "a".repeat(51),
        };
        assert!(long_slug.validate().is_err());
    }

    #[test]
    fn test_complete_signup_response_serialization() {
        let response = CompleteSignupResponse {
            organization: SignupOrganization {
                id: "org-123".to_string(),
                name: "Test Org".to_string(),
                slug: "test-org".to_string(),
                billing_email: "test@example.com".to_string(),
            },
            api_key: "sp_test_abc123".to_string(),
            access_token: "jwt-access-token".to_string(),
            refresh_token: "jwt-refresh-token".to_string(),
            token_type: "Bearer".to_string(),
            expires_in: 86400,
        };

        let json = serde_json::to_string(&response).unwrap();
        assert!(json.contains("org-123"));
        assert!(json.contains("test-org"));
        assert!(json.contains("sp_test_abc123"));
        assert!(json.contains("jwt-access-token"));
        assert!(json.contains("Bearer"));
    }

    #[test]
    fn test_signup_organization_serialization() {
        let org = SignupOrganization {
            id: "org-456".to_string(),
            name: "New Organization".to_string(),
            slug: "new-org".to_string(),
            billing_email: "billing@neworg.com".to_string(),
        };

        let json = serde_json::to_string(&org).unwrap();
        assert!(json.contains("org-456"));
        assert!(json.contains("New Organization"));
        assert!(json.contains("new-org"));
        assert!(json.contains("billing@neworg.com"));
    }

    // Client-IP extraction now lives in `crate::api::middleware::client_ip` and
    // is shared with the rate limiters, which is the point: this module used to
    // have its own (correct) extractor while the security-critical middleware
    // had a different, spoofable one. These tests pin the behaviour this module
    // depends on; the exhaustive table is in the shared module.
    //
    // Note the deleted test `test_extract_client_ip_forwarded_for` asserted that
    // the LEFTMOST `X-Forwarded-For` entry wins. That was the bug: the leftmost
    // entry is supplied by the caller, so it must never be trusted.
    use crate::api::middleware::client_ip::client_ip;
    use crate::config::TrustedProxySettings;

    fn edge_settings() -> TrustedProxySettings {
        TrustedProxySettings {
            client_ip_header: Some("CF-Connecting-IP".to_string()),
            forwarded_hops: 1,
        }
    }

    #[test]
    fn client_ip_prefers_the_edge_header_over_forwarded_for() {
        let mut headers = axum::http::HeaderMap::new();
        headers.insert("CF-Connecting-IP", "203.0.113.50".parse().unwrap());
        headers.insert("X-Forwarded-For", "10.0.0.1, 172.16.0.1".parse().unwrap());

        assert_eq!(client_ip(&headers, &edge_settings()), "203.0.113.50");
    }

    #[test]
    fn client_ip_trims_whitespace() {
        let mut headers = axum::http::HeaderMap::new();
        headers.insert("CF-Connecting-IP", "  192.168.1.1  ".parse().unwrap());

        assert_eq!(client_ip(&headers, &edge_settings()), "192.168.1.1");
    }

    #[test]
    fn client_ip_ignores_untrusted_forwarded_for_by_default() {
        let mut headers = axum::http::HeaderMap::new();
        headers.insert(
            "X-Forwarded-For",
            "203.0.113.195, 70.41.3.18, 150.172.238.178"
                .parse()
                .unwrap(),
        );

        // Default config trusts no proxy, so a client-supplied chain buys nothing.
        assert_eq!(
            client_ip(&headers, &TrustedProxySettings::default()),
            "unknown"
        );

        // With one trusted hop the entry OUR proxy appended is the last one —
        // everything to its left was supplied by the caller.
        assert_eq!(client_ip(&headers, &edge_settings()), "150.172.238.178");
    }

    #[test]
    fn client_ip_falls_back_to_unknown() {
        let headers = axum::http::HeaderMap::new();
        assert_eq!(client_ip(&headers, &edge_settings()), "unknown");

        let mut empty_xff = axum::http::HeaderMap::new();
        empty_xff.insert("X-Forwarded-For", "".parse().unwrap());
        assert_eq!(client_ip(&empty_xff, &edge_settings()), "unknown");
    }
}
