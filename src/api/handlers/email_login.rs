//! Email login handlers for passwordless authentication
//!
//! This module provides endpoints for:
//! - Starting email login flow (sends 6-digit code)
//! - Verifying email login code (returns JWT tokens)

use axum::{
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use chrono::{Duration, Utc};
use jsonwebtoken::{encode, EncodingKey, Header};
use rand::RngExt;
use serde::{Deserialize, Serialize};
use tracing::{error, info, warn};
use uuid::Uuid;
use validator::Validate;

use crate::api::middleware::client_ip::{resolve_client_addr, PeerAddr};
use crate::api::middleware::validation::ValidatedJson;
use crate::api::AppState;
use crate::config::{EmailProvider, RegistrationMode};
use crate::error::{AppError, AppResult, ErrorResponse};

/// Login-code guesses per (address, client IP) per code-validity window, and
/// the per-code attempt cap of billing-email codes (and of login codes while
/// Redis is unavailable).
const MAX_CODE_ATTEMPTS: i32 = 5;
/// Login-code guesses per address from all clients per code-validity window.
///
/// Only a caller spread over more than four client addresses (IPv6: /64s) can
/// exhaust it, and it costs the owner a wait of at most one window, never
/// their codes.
/// With at most [`MAX_CODES_PER_HOUR`] codes per address, a code alive for one
/// window, and each guess compared with every live code, it bounds a
/// distributed brute force to roughly 200 code comparisons per address per
/// hour out of 10^6 possible codes.
const MAX_VERIFY_ATTEMPTS_PER_EMAIL: i32 = 20;
/// Per-code attempt cap of a login code while the Redis budgets are enforced.
///
/// Above the per-address ceiling (a code's life can straddle two windows), so
/// guessing within the budgets never invalidates a code: a stranger can make
/// the owner wait, but cannot make them start over.
const MAX_ATTEMPTS_PER_LOGIN_CODE: i32 = 2 * MAX_VERIFY_ATTEMPTS_PER_EMAIL;
/// Login code validity in minutes
const CODE_VALIDITY_MINUTES: i64 = 10;
/// Rate limit: max codes per email per hour
const MAX_CODES_PER_HOUR: i64 = 5;
/// Rate limit: max email checks per IP per minute
const MAX_EMAIL_CHECKS_PER_MINUTE: i64 = 10;
/// Rate limit: max email checks per IP per hour
const MAX_EMAIL_CHECKS_PER_HOUR: i64 = 60;
/// Rate limit: login-code requests per client IP per minute (burst)
const MAX_STARTS_PER_IP_PER_MINUTE: u32 = 3;
/// Rate limit: login-code requests per client IP per hour
const MAX_STARTS_PER_IP_PER_HOUR: u32 = 10;
/// Upper bound for one Turnstile siteverify call.
const TURNSTILE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// Error of the public email-code routes: an [`AppError`], or a finished
/// response for the answers `AppError` has no shape for — a 429 that names
/// the wait (with `Retry-After`) and a 400 `CAPTCHA_FAILED`.
pub enum EmailCodeError {
    App(AppError),
    Response(Box<Response>),
}

impl From<AppError> for EmailCodeError {
    fn from(err: AppError) -> Self {
        Self::App(err)
    }
}

impl From<sqlx::Error> for EmailCodeError {
    fn from(err: sqlx::Error) -> Self {
        Self::App(AppError::Database(err))
    }
}

impl IntoResponse for EmailCodeError {
    fn into_response(self) -> Response {
        match self {
            Self::App(err) => err.into_response(),
            Self::Response(res) => *res,
        }
    }
}

/// 429 `RATE_LIMIT_EXCEEDED` with `Retry-After` and a message naming the wait.
fn too_many_requests(message: String, retry_after_secs: u64) -> EmailCodeError {
    let body = ErrorResponse {
        code: "RATE_LIMIT_EXCEEDED".to_string(),
        message,
        details: Some(serde_json::json!({ "retry_after_seconds": retry_after_secs })),
    };
    let mut res = (StatusCode::TOO_MANY_REQUESTS, Json(body)).into_response();
    res.headers_mut().insert(
        axum::http::header::RETRY_AFTER,
        axum::http::HeaderValue::from(retry_after_secs),
    );
    EmailCodeError::Response(Box::new(res))
}

/// 400 `CAPTCHA_FAILED` (contract C2).
fn captcha_failed(message: &str) -> EmailCodeError {
    let body = ErrorResponse {
        code: "CAPTCHA_FAILED".to_string(),
        message: message.to_string(),
        details: None,
    };
    EmailCodeError::Response(Box::new(
        (StatusCode::BAD_REQUEST, Json(body)).into_response(),
    ))
}

/// "45 seconds", "1 minute", "7 minutes" — a wait as a person reads it.
fn wait_text(secs: u64) -> String {
    let secs = secs.max(1);
    if secs < 60 {
        format!("{secs} second{}", if secs == 1 { "" } else { "s" })
    } else {
        let minutes = secs.div_ceil(60);
        format!("{minutes} minute{}", if minutes == 1 { "" } else { "s" })
    }
}

/// Outcome of spending one unit of a fixed-window budget kept in Redis.
enum Budget {
    Spent,
    /// Exhausted; the window resets in this many seconds.
    Exhausted(u64),
    /// No Redis configured, or it failed: the caller picks its fallback.
    Unavailable,
}

async fn spend_budget(state: &AppState, key: &str, limit: u32, window_secs: u64) -> Budget {
    let Some(ref cache) = state.cache else {
        return Budget::Unavailable;
    };
    match cache.check_rate_limit(key, limit.max(1), window_secs).await {
        Ok(result) if !result.allowed => {
            Budget::Exhausted((result.reset_at - Utc::now()).num_seconds().max(1) as u64)
        }
        Ok(_) => Budget::Spent,
        Err(e) => {
            warn!(error = %e, "Redis error while spending an email-login budget");
            Budget::Unavailable
        }
    }
}

/// Resolve the live organization whose VERIFIED billing email is `$1`
/// (already lower-cased). An unverified address never resolves a login: anyone
/// with an API key can type any address into their org settings.
const VERIFIED_ORG_BY_EMAIL: &str = "SELECT id FROM organizations \
     WHERE lower(billing_email) = $1 \
       AND billing_email_verified_at IS NOT NULL \
       AND plan_tier <> 'deleted' \
     ORDER BY billing_email_verified_at ASC \
     LIMIT 1";

/// Prefix for billing-email verification codes stored in `email_login_codes`.
/// They must never be accepted by the login flow: the code proves the address,
/// it does not authorize a login into the organization that requested it.
const VERIFY_CODE_PREFIX: &str = "verify:";

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
    /// Whether the email signup flow is accepting new accounts.
    ///
    /// The marketing signup page already gates on this field. Without it the
    /// form sent a verification code and then failed at `signup/complete` when
    /// `EMAIL_SIGNUP_ENABLED=false`.
    pub signup_enabled: bool,
}

/// Check if an email is already registered
///
/// GET /api/v1/auth/check-email?email=...
///
/// Rate limited to prevent email enumeration attacks.
pub async fn check_email(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
    PeerAddr(peer): PeerAddr,
    axum::extract::Query(query): axum::extract::Query<CheckEmailRequest>,
) -> AppResult<Json<CheckEmailResponse>> {
    // The derive was present but `.validate()` was never called, so arbitrary
    // strings reached the query (and, before this rewrite, the database).
    query
        .validate()
        .map_err(|e| AppError::Validation(e.to_string()))?;

    let email = query.email.trim().to_lowercase();

    // Extract IP for rate limiting
    let client_ip = extract_client_ip(&state, &headers, peer);

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
    let exists: Option<(String,)> = sqlx::query_as(VERIFIED_ORG_BY_EMAIL)
        .bind(&email)
        .fetch_optional(state.db.pool())
        .await?;

    Ok(Json(CheckEmailResponse {
        available: exists.is_none(),
        exists: exists.is_some(),
        signup_enabled: state.settings.registration.email_signup_enabled,
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
/// the weaker. The TCP peer decides whether edge headers are believed at all.
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

/// Per-client throttle on `POST /auth/email/start`.
///
/// The per-address cap does nothing against one client mailing many different
/// addresses (mail bombing third parties, burning the sender's reputation and
/// provider quota). Fails open without Redis like the other throttles here,
/// and denies when `RATE_LIMIT_FAIL_CLOSED` is set.
async fn check_start_rate_limit(state: &AppState, client_ip: &str) -> Result<(), EmailCodeError> {
    for (window, limit, window_secs) in [
        ("minute", MAX_STARTS_PER_IP_PER_MINUTE, 60),
        ("hour", MAX_STARTS_PER_IP_PER_HOUR, 3600),
    ] {
        let key = format!("email_start:{window}:{client_ip}");
        match spend_budget(state, &key, limit, window_secs).await {
            Budget::Spent => {}
            Budget::Exhausted(retry_after) => {
                warn!(ip = %client_ip, window, "Email login start rate limit exceeded");
                return Err(too_many_requests(
                    format!(
                        "Too many login codes requested from this network. Please try again in {}.",
                        wait_text(retry_after)
                    ),
                    retry_after,
                ));
            }
            Budget::Unavailable if state.settings.rate_limit.fail_closed_on_redis_error => {
                warn!("Email login start throttle unavailable; denying (RATE_LIMIT_FAIL_CLOSED)");
                return Err(too_many_requests(
                    "Login codes are temporarily unavailable. Please try again shortly."
                        .to_string(),
                    5,
                ));
            }
            Budget::Unavailable => return Ok(()),
        }
    }
    Ok(())
}

/// Cloudflare Turnstile siteverify answer.
#[derive(Debug, Deserialize)]
struct TurnstileVerdict {
    success: bool,
    #[serde(default, rename = "error-codes")]
    error_codes: Vec<String>,
}

/// Check a Turnstile token with Cloudflare (only called when
/// `TURNSTILE_SECRET_KEY` is set).
///
/// Fails closed: a token that could not be verified is not a verified token.
/// The widget hands out a fresh token on retry, so a passing siteverify hiccup
/// costs the user one more click.
async fn verify_turnstile(
    verify_url: &str,
    secret: &str,
    token: Option<&str>,
    remote_ip: Option<&str>,
) -> Result<(), EmailCodeError> {
    let Some(token) = token.map(str::trim).filter(|t| !t.is_empty()) else {
        return Err(captcha_failed(
            "Complete the verification challenge and try again.",
        ));
    };
    let mut form = vec![("secret", secret), ("response", token)];
    if let Some(ip) = remote_ip {
        form.push(("remoteip", ip));
    }

    let verdict = match reqwest::Client::builder()
        .timeout(TURNSTILE_TIMEOUT)
        .build()
    {
        Ok(client) => match client.post(verify_url).form(&form).send().await {
            Ok(res) if res.status().is_success() => match res.json::<TurnstileVerdict>().await {
                Ok(verdict) => Some(verdict),
                Err(e) => {
                    error!(error = %e, "Turnstile siteverify returned an unreadable answer");
                    None
                }
            },
            Ok(res) => {
                error!(status = %res.status(), "Turnstile siteverify failed");
                None
            }
            Err(e) => {
                error!(error = %e, "Turnstile siteverify unreachable");
                None
            }
        },
        Err(e) => {
            error!(error = %e, "Failed to build the Turnstile client");
            None
        }
    };

    match verdict {
        Some(verdict) if verdict.success => Ok(()),
        Some(verdict) => {
            warn!(errors = ?verdict.error_codes, "Turnstile token rejected");
            Err(captcha_failed(
                "Verification failed. Please complete the challenge again.",
            ))
        }
        None => Err(captcha_failed(
            "Verification could not be completed. Please try again.",
        )),
    }
}

/// Start email login request
#[derive(Debug, Deserialize, Validate)]
pub struct StartEmailLoginRequest {
    #[validate(email(message = "Invalid email format"))]
    pub email: String,
    /// Cloudflare Turnstile token; checked only when `TURNSTILE_SECRET_KEY` is set.
    #[serde(default)]
    #[validate(length(max = 2048, message = "Invalid verification token"))]
    pub turnstile_token: Option<String>,
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
    headers: axum::http::HeaderMap,
    PeerAddr(peer): PeerAddr,
    ValidatedJson(request): ValidatedJson<StartEmailLoginRequest>,
) -> Result<Json<StartEmailLoginResponse>, EmailCodeError> {
    // Per client first: it is cheap, and it also bounds how often one client
    // can make us call Turnstile.
    let client_ip = extract_client_ip(&state, &headers, peer);
    check_start_rate_limit(&state, &client_ip).await?;

    if let Some(secret) = state.settings.turnstile.secret_key.as_deref() {
        let remote_ip = resolve_client_addr(&headers, peer, &state.settings.trusted_proxy);
        verify_turnstile(
            &state.settings.turnstile.verify_url,
            secret,
            request.turnstile_token.as_deref(),
            remote_ip.as_deref(),
        )
        .await?;
    }

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
        )
        .into());
    }

    // Check if email is associated with an organization
    let org: Option<(String,)> = sqlx::query_as(VERIFIED_ORG_BY_EMAIL)
        .bind(&email)
        .fetch_optional(state.db.pool())
        .await?;

    let org_id = org.map(|(id,)| id);

    // Generate a 6-digit code
    let code: String = {
        let mut rng = rand::rng();
        format!("{:06}", rng.random_range(0..1000000))
    };

    // Send first, store second: the row is what counts against the address's
    // hourly allowance, so a code the provider failed to deliver must not use
    // one up (an outage used to lock users out for the hour).
    send_login_code_email(&state, &email, &code).await?;

    let expires_at = Utc::now() + Duration::minutes(CODE_VALIDITY_MINUTES);
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
    /// An organization already uses this address as an UNCONFIRMED billing
    /// email. Its owner should sign in with an API key and confirm the address
    /// instead of creating a second organization; anyone else can sign up.
    pub unverified_org_exists: bool,
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
    /// Login session id shared by the access/refresh pair (see `auth::Claims::sid`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    sid: Option<String>,
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
    headers: axum::http::HeaderMap,
    PeerAddr(peer): PeerAddr,
    ValidatedJson(request): ValidatedJson<VerifyEmailLoginRequest>,
) -> Result<Json<VerifyEmailResponse>, EmailCodeError> {
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
    // Guesses are bounded per address by the budgets below and per code by the
    // attempt cap, so this grants the attacker no extra guesses.
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
          -- Billing-email verification codes prove an address for one org;
          -- accepting them here would log their recipient into that org.
          AND code NOT LIKE 'verify:%'
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
        )
        .into());
    }

    // Attempt budgets. Comparing a guess against every live code (which is
    // what stops a third party from invalidating the victim's code) would
    // otherwise multiply the attacker's reach: with five codes outstanding, one
    // request tests five values while burning one per-code attempt.
    let client_ip = extract_client_ip(&state, &headers, peer);
    let budgets_enforced = spend_verify_attempt(&state, &email, &client_ip).await?;
    // Without Redis the per-code cap is the only bound left, so it falls back
    // to the strict budget (a stranger can then spend the owner's code, as
    // before the budgets were keyed per client).
    let per_code_cap = if budgets_enforced {
        MAX_ATTEMPTS_PER_LOGIN_CODE
    } else {
        MAX_CODE_ATTEMPTS
    };

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
    .bind(per_code_cap)
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
        )
        .into());
    };

    // SECURITY: Use constant-time comparison to prevent timing attacks
    // An attacker could otherwise determine the correct code digit-by-digit
    if !constant_time_compare(&code, &stored_code) {
        warn!(email = %mask_email(&email), attempts = attempts_now, "Invalid login code");
        return Err(AppError::Authentication("Invalid code".to_string()).into());
    }

    // Spend the code. `used_at IS NULL` makes this the one place a code is
    // consumed: of two verifies racing on the same code, only the one whose
    // UPDATE lands goes on to mint a session (or a signup token).
    let consumed = sqlx::query(
        "UPDATE email_login_codes SET used_at = NOW() WHERE id = $1 AND used_at IS NULL",
    )
    .bind(code_id)
    .execute(state.db.pool())
    .await?;
    if consumed.rows_affected() == 0 {
        warn!(email = %mask_email(&email), "Login code was already used");
        return Err(AppError::Authentication(
            "Invalid or expired code. Please request a new one.".to_string(),
        )
        .into());
    }

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
                    )
                    .into());
                }
                Some(_) => {}
            }

            // Existing user - return login tokens
            let api_key_id = get_or_create_email_api_key(&state, &existing_org_id, &email).await?;

            let now = Utc::now();
            let access_expiration = state.settings.jwt.expiration_hours as i64;
            let refresh_expiration = access_expiration * 24;

            let sid = Uuid::new_v4().to_string();
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
                sid: Some(sid.clone()),
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
                sid: Some(sid),
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

            // Only a hint: the code proves the mailbox, but it must never link
            // the caller to an org whose address was never confirmed. A pending
            // billing email (a change of a verified one that waits for its code)
            // is just as unconfirmed.
            let (unverified_org_exists,): (bool,) = sqlx::query_as(
                "SELECT EXISTS (SELECT 1 FROM organizations \
                 WHERE ((lower(billing_email) = $1 AND billing_email_verified_at IS NULL) \
                        OR lower(pending_billing_email) = $1) \
                   AND plan_tier <> 'deleted')",
            )
            .bind(&email)
            .fetch_one(state.db.pool())
            .await?;

            info!(email = %mask_email(&email), "Email verified, signup token issued");

            Ok(Json(VerifyEmailResponse::Signup(
                VerifyEmailSignupResponse {
                    signup_token,
                    email: email.clone(),
                    expires_in: 900, // 15 minutes
                    unverified_org_exists,
                },
            )))
        }
    }
}

/// Spend one login-code guess against `email` from `client_ip`.
///
/// Two budgets per code-validity window:
/// - [`MAX_CODE_ATTEMPTS`] per (address, client): the strict one. Keyed on the
///   address alone it let anyone who knew an address lock its owner out with
///   five wrong guesses, and requesting a new code did not help.
/// - [`MAX_VERIFY_ATTEMPTS_PER_EMAIL`] per address: the ceiling for a guesser
///   spread over many clients. It only makes everyone wait for the window to
///   end; codes stay valid.
///
/// A client over its own budget is not charged to the ceiling, so one client
/// cannot spend it for everyone. Returns whether the budgets were enforced
/// (false without a working Redis).
async fn spend_verify_attempt(
    state: &AppState,
    email: &str,
    client_ip: &str,
) -> Result<bool, EmailCodeError> {
    let window_secs = (CODE_VALIDITY_MINUTES.max(1) as u64) * 60;
    let budgets = [
        (
            format!("email_verify_attempts:{email}:{client_ip}"),
            MAX_CODE_ATTEMPTS,
            "Too many attempts.",
        ),
        (
            format!("email_verify_ceiling:{email}"),
            MAX_VERIFY_ATTEMPTS_PER_EMAIL,
            "Too many attempts for this address.",
        ),
    ];
    for (key, limit, reason) in budgets {
        match spend_budget(state, &key, limit.max(1) as u32, window_secs).await {
            Budget::Spent => {}
            Budget::Exhausted(retry_after) => {
                warn!(email = %mask_email(email), ip = %client_ip, reason, "Login verify attempt budget exhausted");
                return Err(too_many_requests(
                    format!("{reason} Please try again in {}.", wait_text(retry_after)),
                    retry_after,
                ));
            }
            Budget::Unavailable => {
                warn!("Login verify attempt budget unavailable; relying on per-code cap");
                return Ok(false);
            }
        }
    }
    Ok(true)
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

/// Gate for `POST /api/v1/auth/signup/complete`.
///
/// Matches `POST /organizations`: `EMAIL_SIGNUP_ENABLED=false` blocks the
/// email flow entirely; `REGISTRATION_MODE=closed` still allows it when
/// `X-Admin-Key` matches (the marketing site Pages Function does this).
fn authorize_email_signup(
    email_signup_enabled: bool,
    mode: &RegistrationMode,
    configured_admin_key: Option<&str>,
    headers: &axum::http::HeaderMap,
) -> AppResult<()> {
    if !email_signup_enabled {
        return Err(AppError::Authorization(
            "Email signup is currently disabled".to_string(),
        ));
    }

    match mode {
        RegistrationMode::Open => Ok(()),
        RegistrationMode::Closed => {
            let provided = headers.get("X-Admin-Key").and_then(|v| v.to_str().ok());
            // An empty configured key must never match an empty header.
            match (configured_admin_key.filter(|key| !key.is_empty()), provided) {
                (Some(expected), Some(provided)) if constant_time_compare(expected, provided) => {
                    Ok(())
                }
                // Closed registration: the deployment's own website completes signup
                // (it holds the admin key). Say so, and that the verified signup
                // token was not consumed, instead of implying signup is off.
                (Some(_), _) => Err(AppError::Authorization(
                    "Signup on this deployment is completed through its website, not \
                     directly against the API. Your signup token was not used and stays \
                     valid until it expires."
                        .to_string(),
                )),
                (None, _) => Err(AppError::Authorization(
                    "Email signup is currently disabled and no admin key is configured."
                        .to_string(),
                )),
            }
        }
        RegistrationMode::Invite => Err(AppError::Authorization(
            "Invite-based registration is not yet implemented.".to_string(),
        )),
    }
}

/// The slug a website signup asks for, trimmed and lower-cased, or the reason
/// it is refused. `complete_signup` calls this before the signup token is
/// consumed, so a refused slug leaves the token valid for a retry.
///
/// Characters and hyphens are what `GET /organizations/check-slug` (the
/// website's pre-check) accepts, so a slug it calls valid still signs up. The
/// names in `RESERVED_SLUGS` are refused here as they are by
/// `POST /organizations`: a signup used to be able to take `admin`, `api` or
/// `dashboard`. `POST /organizations` also requires `SLUG_REGEX` (a leading
/// letter, no `--`); signup does not, because the website suggests slugs that
/// start with a digit (`3m-labs-x7k2q9`).
fn normalize_signup_slug(raw: &str) -> AppResult<String> {
    let slug = raw.to_lowercase().trim().to_string();
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
    if let Err(e) = crate::models::validate_slug_not_reserved(&slug) {
        return Err(AppError::Validation(
            e.message
                .map(|m| m.into_owned())
                .unwrap_or_else(|| "This slug is reserved and cannot be used".to_string()),
        ));
    }
    Ok(slug)
}

/// Complete signup with verified email
///
/// POST /api/v1/auth/signup/complete
pub async fn complete_signup(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
    ValidatedJson(request): ValidatedJson<CompleteSignupRequest>,
) -> AppResult<(axum::http::StatusCode, Json<CompleteSignupResponse>)> {
    authorize_email_signup(
        state.settings.registration.email_signup_enabled,
        &state.settings.registration.mode,
        state.settings.registration.admin_api_key.as_deref(),
        &headers,
    )?;

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

    // Validate the slug (format and reserved names) before the token is consumed.
    let slug = normalize_signup_slug(&request.slug)?;

    // Check for duplicate email (race condition guard)
    let existing_email: Option<(String,)> = sqlx::query_as(VERIFIED_ORG_BY_EMAIL)
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
        INSERT INTO organizations (id, name, slug, plan_tier, billing_email, billing_email_verified_at, settings, created_at, updated_at)
        VALUES ($1, $2, $3, 'free', $4, $6, $5, $6, $6)
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

    let sid = Uuid::new_v4().to_string();
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
        sid: Some(sid.clone()),
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
        sid: Some(sid),
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
    send_code_email(state, email, code, CodePurpose::Login).await
}

/// What a one-time code e-mail is for; only the wording differs.
#[derive(Debug, Clone, Copy)]
enum CodePurpose {
    Login,
    BillingEmailVerification,
}

async fn send_code_email(
    state: &AppState,
    email: &str,
    code: &str,
    purpose: CodePurpose,
) -> AppResult<()> {
    let (subject, explanation, label) = match purpose {
        CodePurpose::Login => ("Your Spooled Cloud Login Code", None, "Your login code is"),
        CodePurpose::BillingEmailVerification => (
            "Confirm your Spooled Cloud billing email",
            Some(
                "Someone set this address as the billing email of a Spooled Cloud \
                 organization. If that was you, enter this code in the dashboard to confirm \
                 it. Confirming also lets you log in to that organization with this address.",
            ),
            "Your confirmation code is",
        ),
    };
    let intro = match explanation {
        Some(text) => format!("{text}\n\n{label}"),
        None => label.to_string(),
    };
    let html_explanation = explanation
        .map(|text| format!("<p>{text}</p>"))
        .unwrap_or_default();
    let body = format!(
        r#"Hi,

{}: {}

This code will expire in {} minutes.

If you didn't request this code, you can safely ignore this email.

Best,
The Spooled Cloud Team"#,
        intro, code, CODE_VALIDITY_MINUTES
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
    {}
    <p>{}:</p>
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
        html_explanation, label, code, CODE_VALIDITY_MINUTES
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
///
/// The key is a system key (`is_system`): the customer never created it, so it
/// does not count toward `max_api_keys`, and it is looked up by that flag rather
/// than by its name, which a customer key may share.
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
          AND is_system = TRUE
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
        INSERT INTO api_keys (id, organization_id, name, key_hash, key_prefix, queues, is_active, created_at, lookup_hash, is_system)
        VALUES ($1, $2, 'Email Login', $3, $4, ARRAY['*'], TRUE, $5, $6, TRUE)
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

/// Billing-email verification state returned by both verification routes.
#[derive(Debug, Serialize)]
pub struct BillingEmailVerificationResponse {
    /// `code_sent` after a code went out, `verified` once the address is proven.
    pub status: String,
    /// Masked address the code was sent to (only with `code_sent`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub email_sent_to: Option<String>,
}

/// Request body for `POST /organizations/billing-email/verify`.
#[derive(Debug, Deserialize, Validate)]
pub struct VerifyBillingEmailRequest {
    #[validate(length(equal = 6, message = "Code must be 6 digits"))]
    pub code: String,
}

/// The address the confirmation flow works on, for live orgs only, and whether
/// it is verified already: a pending billing-email change when there is one
/// (never verified: verifying it is what moves it into `billing_email`), else
/// the billing email itself.
async fn billing_email_to_verify(
    state: &AppState,
    org_id: &str,
) -> AppResult<(Option<String>, bool)> {
    type Row = (
        Option<String>,
        Option<chrono::DateTime<Utc>>,
        Option<String>,
    );
    let row: Option<Row> = sqlx::query_as(
        "SELECT billing_email, billing_email_verified_at, pending_billing_email \
         FROM organizations WHERE id = $1 AND plan_tier <> 'deleted'",
    )
    .bind(org_id)
    .fetch_optional(state.db.pool())
    .await?;
    let Some((email, verified_at, pending)) = row else {
        return Err(AppError::NotFound("Organization not found".to_string()));
    };
    let normalize = |e: String| crate::api::handlers::organizations::normalize_billing_email(&e);
    Ok(match pending {
        Some(pending) => (Some(normalize(pending)), false),
        None => (email.map(normalize), verified_at.is_some()),
    })
}

fn verified_response() -> Json<BillingEmailVerificationResponse> {
    Json(BillingEmailVerificationResponse {
        status: "verified".to_string(),
        email_sent_to: None,
    })
}

/// Send a confirmation code to the organization's pending billing-email
/// change, or else to its unverified billing email.
///
/// POST /api/v1/organizations/billing-email/verification
///
/// Until the address is confirmed, email login does not resolve the
/// organization by it (see `VERIFIED_ORG_BY_EMAIL`); a pending change leaves
/// the verified address in place meanwhile.
pub async fn start_billing_email_verification(
    State(state): State<AppState>,
    axum::extract::Extension(ctx): axum::extract::Extension<crate::models::ApiKeyContext>,
) -> AppResult<Json<BillingEmailVerificationResponse>> {
    crate::api::handlers::require_unrestricted_key(&ctx, "verify the billing email")?;

    let (email, verified) = billing_email_to_verify(&state, &ctx.organization_id).await?;
    let Some(email) = email else {
        return Err(AppError::BadRequest(
            "Set a billing email on the organization before verifying it".to_string(),
        ));
    };
    if verified {
        return Ok(verified_response());
    }
    if crate::api::handlers::organizations::billing_email_taken(
        state.db.pool(),
        &email,
        Some(&ctx.organization_id),
    )
    .await?
    {
        return Err(AppError::Conflict(
            "Another account already uses this billing email".to_string(),
        ));
    }

    // Shares the per-address hourly budget with login codes, so this route
    // cannot be used to mail an address more often than the login flow can.
    let (recent,): (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM email_login_codes \
         WHERE email = $1 AND created_at > NOW() - INTERVAL '1 hour'",
    )
    .bind(&email)
    .fetch_one(state.db.pool())
    .await?;
    if recent >= MAX_CODES_PER_HOUR {
        return Err(AppError::RateLimit(
            "Too many codes sent to this address. Please try again later.".to_string(),
        ));
    }

    let code: String = {
        let mut rng = rand::rng();
        format!("{:06}", rng.random_range(0..1000000))
    };
    // Sent before it is stored, as in `start`: an undelivered code must not
    // count against the address's hourly allowance.
    send_code_email(&state, &email, &code, CodePurpose::BillingEmailVerification).await?;
    sqlx::query(
        "INSERT INTO email_login_codes (email, code, organization_id, expires_at) \
         VALUES ($1, $2, $3, $4)",
    )
    .bind(&email)
    .bind(format!("{VERIFY_CODE_PREFIX}{code}"))
    .bind(&ctx.organization_id)
    .bind(Utc::now() + Duration::minutes(CODE_VALIDITY_MINUTES))
    .execute(state.db.pool())
    .await?;

    let masked = mask_email(&email);
    info!(org_id = %ctx.organization_id, email = %masked, "Billing email confirmation code sent");
    Ok(Json(BillingEmailVerificationResponse {
        status: "code_sent".to_string(),
        email_sent_to: Some(masked),
    }))
}

/// Confirm the organization's billing email (or its pending change, which then
/// becomes the verified billing email) with the code sent to it.
///
/// POST /api/v1/organizations/billing-email/verify
pub async fn verify_billing_email(
    State(state): State<AppState>,
    axum::extract::Extension(ctx): axum::extract::Extension<crate::models::ApiKeyContext>,
    ValidatedJson(request): ValidatedJson<VerifyBillingEmailRequest>,
) -> AppResult<Json<BillingEmailVerificationResponse>> {
    crate::api::handlers::require_unrestricted_key(&ctx, "verify the billing email")?;

    let (email, verified) = billing_email_to_verify(&state, &ctx.organization_id).await?;
    let Some(email) = email else {
        return Err(AppError::BadRequest(
            "The organization has no billing email to verify".to_string(),
        ));
    };
    if verified {
        return Ok(verified_response());
    }

    // Codes are bound to the address they were sent to: changing the billing
    // email again makes earlier codes useless.
    let candidates: Vec<(Uuid, String)> = sqlx::query_as(
        r#"
        SELECT id, code
        FROM email_login_codes
        WHERE email = $1
          AND organization_id = $2
          AND code LIKE 'verify:%'
          AND expires_at > NOW()
          AND used_at IS NULL
        ORDER BY created_at DESC
        LIMIT $3
        "#,
    )
    .bind(&email)
    .bind(&ctx.organization_id)
    .bind(MAX_CODES_PER_HOUR)
    .fetch_all(state.db.pool())
    .await?;

    // 400, not 401: this route is authenticated, and clients treat a 401 as an
    // expired session (the dashboard refreshes and replays the request, which
    // spent a second attempt on the same wrong code, or logged the user out).
    let invalid = || AppError::BadRequest("Invalid or expired code".to_string());
    if candidates.is_empty() {
        return Err(invalid());
    }

    // Per-address budget, as in the login flow: a guess is compared against
    // every live code, so without it the reach grows with the codes issued.
    if let Some(ref cache) = state.cache {
        let key = format!("billing_verify_attempts:{}:{}", ctx.organization_id, email);
        let window_secs = (CODE_VALIDITY_MINUTES.max(1) as u64) * 60;
        match cache
            .check_rate_limit(&key, MAX_CODE_ATTEMPTS.max(1) as u32, window_secs)
            .await
        {
            Ok(result) if !result.allowed => {
                return Err(AppError::BadRequest(
                    "Too many attempts. Please request a new code.".to_string(),
                ));
            }
            Ok(_) => {}
            Err(e) => {
                warn!(error = %e, "Billing email verify budget unavailable; relying on per-code cap");
            }
        }
    }

    let submitted = format!("{VERIFY_CODE_PREFIX}{}", request.code.trim());
    let matched = candidates
        .iter()
        .find(|(_, stored)| constant_time_compare(&submitted, stored));
    // A wrong guess is charged to the newest code, as in the login flow.
    let (code_id, _) = matched.unwrap_or(&candidates[0]).clone();

    let charged: Option<(i32,)> = sqlx::query_as(
        "UPDATE email_login_codes SET attempts = attempts + 1 \
         WHERE id = $1 AND attempts < $2 RETURNING attempts",
    )
    .bind(code_id)
    .bind(MAX_CODE_ATTEMPTS)
    .fetch_optional(state.db.pool())
    .await?;
    if charged.is_none() {
        sqlx::query("UPDATE email_login_codes SET used_at = NOW() WHERE id = $1")
            .bind(code_id)
            .execute(state.db.pool())
            .await?;
        return Err(AppError::BadRequest(
            "Too many attempts. Please request a new code.".to_string(),
        ));
    }
    if matched.is_none() {
        return Err(invalid());
    }

    sqlx::query("UPDATE email_login_codes SET used_at = NOW() WHERE id = $1")
        .bind(code_id)
        .execute(state.db.pool())
        .await?;

    if crate::api::handlers::organizations::billing_email_taken(
        state.db.pool(),
        &email,
        Some(&ctx.organization_id),
    )
    .await?
    {
        return Err(AppError::Conflict(
            "Another account verified this billing email first".to_string(),
        ));
    }

    // Only verify the address the code was sent to: if the email changed in the
    // meantime, nothing is updated and the caller has to start over. A verified
    // pending change replaces the old billing email, which stops being an
    // email-login identity only now.
    let updated = sqlx::query(
        "UPDATE organizations \
         SET billing_email = $2, billing_email_verified_at = NOW(), \
             pending_billing_email = NULL, updated_at = NOW() \
         WHERE id = $1 \
           AND (lower(pending_billing_email) = $2 \
                OR (pending_billing_email IS NULL \
                    AND lower(billing_email) = $2 \
                    AND billing_email_verified_at IS NULL))",
    )
    .bind(&ctx.organization_id)
    .bind(&email)
    .execute(state.db.pool())
    .await?;
    if updated.rows_affected() == 0 {
        return Err(invalid());
    }

    info!(org_id = %ctx.organization_id, email = %mask_email(&email), "Billing email verified");
    Ok(verified_response())
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
            turnstile_token: None,
        };
        assert!(valid.validate().is_ok());

        // Invalid email
        let invalid = StartEmailLoginRequest {
            email: "not-an-email".to_string(),
            turnstile_token: None,
        };
        assert!(invalid.validate().is_err());

        // Empty email
        let empty = StartEmailLoginRequest {
            email: "".to_string(),
            turnstile_token: None,
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
            sid: None,
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
            sid: None,
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
            signup_enabled: true,
        };
        let json = serde_json::to_string(&exists_response).unwrap();
        assert!(json.contains("\"available\":false"));
        assert!(json.contains("\"exists\":true"));
        assert!(json.contains("\"signup_enabled\":true"));

        // Email available
        let available_response = CheckEmailResponse {
            available: true,
            exists: false,
            signup_enabled: false,
        };
        let json = serde_json::to_string(&available_response).unwrap();
        assert!(json.contains("\"available\":true"));
        assert!(json.contains("\"exists\":false"));
        assert!(json.contains("\"signup_enabled\":false"));
    }

    #[test]
    fn test_verify_email_signup_response_serialization() {
        let response = VerifyEmailSignupResponse {
            signup_token: "abc123-token".to_string(),
            email: "test@example.com".to_string(),
            expires_in: 900,
            unverified_org_exists: false,
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
            unverified_org_exists: false,
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

    fn refusal(raw: &str) -> String {
        match normalize_signup_slug(raw) {
            Err(AppError::Validation(message)) => message,
            other => panic!("{raw:?} should be refused with a validation error, got {other:?}"),
        }
    }

    /// FESF-17 (backend half): a website signup could take `admin`, `api` or
    /// `dashboard` because `complete_signup` never looked at `RESERVED_SLUGS`.
    #[test]
    fn test_signup_slug_refuses_every_reserved_name() {
        for reserved in crate::models::RESERVED_SLUGS {
            assert!(
                refusal(reserved).contains("reserved"),
                "{reserved} is reserved for POST /organizations and must be refused at signup"
            );
        }
        for name in ["admin", "api", "dashboard"] {
            assert!(crate::models::RESERVED_SLUGS.contains(&name));
        }
    }

    #[test]
    fn test_signup_slug_is_normalised_before_the_reserved_check() {
        // `complete_signup` lower-cases and trims what the visitor typed: the check has to see
        // that form, or `Admin` and ` api ` would still get through.
        for typed in ["Admin", "ADMIN", " admin ", "  Dashboard\t", "\nApi"] {
            assert!(refusal(typed).contains("reserved"), "{typed:?}");
        }
    }

    #[test]
    fn test_signup_slug_accepts_what_it_accepted_before() {
        // Only reserved names are new. Names that merely contain one, a leading digit and a
        // double hyphen (the website suggests `3m-labs-x7k2q9`; check-slug calls both valid) keep
        // working.
        for (typed, stored) in [
            ("my-company", "my-company"),
            ("My-Company", "my-company"),
            (" acme ", "acme"),
            ("admin-1a2b3c", "admin-1a2b3c"),
            ("api2", "api2"),
            ("my-api", "my-api"),
            ("3m-labs-x7k2q9", "3m-labs-x7k2q9"),
            ("123", "123"),
            ("acme--co", "acme--co"),
        ] {
            assert_eq!(normalize_signup_slug(typed).unwrap(), stored, "{typed:?}");
        }
    }

    #[test]
    fn test_signup_slug_keeps_the_format_rules_and_their_messages() {
        assert!(refusal("my_company").contains("lowercase letters, digits, and hyphens"));
        assert!(refusal("my company").contains("lowercase letters, digits, and hyphens"));
        assert!(refusal("-acme").contains("start or end with a hyphen"));
        assert!(refusal("acme-").contains("start or end with a hyphen"));
    }

    /// The reserved name has to be refused BEFORE the one-time signup token is
    /// consumed: a refused slug must leave the token usable for a retry.
    #[test]
    fn test_complete_signup_checks_the_slug_before_it_consumes_the_token() {
        let source = include_str!("email_login.rs");
        let body = &source[source
            .find("pub async fn complete_signup(")
            .expect("complete_signup")..];
        let checked = body
            .find("normalize_signup_slug(&request.slug)?")
            .expect("complete_signup validates the slug with normalize_signup_slug");
        let consumed = body
            .find("UPDATE email_login_codes SET used_at = NOW() WHERE id = $1 AND used_at IS NULL")
            .expect("complete_signup consumes the token");
        assert!(
            checked < consumed,
            "the slug check must come before the token is consumed"
        );
        // The organization is created after both.
        let created = body
            .find("INSERT INTO organizations")
            .expect("complete_signup creates the organization");
        assert!(consumed < created);
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
    use crate::api::middleware::client_ip::client_ip_from_peer;
    use crate::config::TrustedProxySettings;

    fn client_ip(headers: &axum::http::HeaderMap, settings: &TrustedProxySettings) -> String {
        client_ip_from_peer(headers, None, settings)
    }

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

    // BEAUTH-06: a throttled verify says how long to wait.
    #[test]
    fn wait_text_names_the_remaining_window() {
        assert_eq!(wait_text(0), "1 second");
        assert_eq!(wait_text(45), "45 seconds");
        assert_eq!(wait_text(60), "1 minute");
        assert_eq!(wait_text(61), "2 minutes");
        assert_eq!(wait_text(600), "10 minutes");
    }

    fn admin_headers(key: &str) -> axum::http::HeaderMap {
        let mut headers = axum::http::HeaderMap::new();
        headers.insert("X-Admin-Key", key.parse().unwrap());
        headers
    }

    #[test]
    fn email_signup_open_mode_does_not_need_admin_key() {
        assert!(authorize_email_signup(
            true,
            &RegistrationMode::Open,
            Some("secret"),
            &axum::http::HeaderMap::new(),
        )
        .is_ok());
    }

    #[test]
    fn email_signup_disabled_is_blocked_even_in_open_mode() {
        let err = authorize_email_signup(
            false,
            &RegistrationMode::Open,
            Some("secret"),
            &admin_headers("secret"),
        )
        .unwrap_err();
        assert!(matches!(err, AppError::Authorization(_)));
    }

    #[test]
    fn email_signup_closed_mode_accepts_matching_admin_key() {
        assert!(authorize_email_signup(
            true,
            &RegistrationMode::Closed,
            Some("secret"),
            &admin_headers("secret"),
        )
        .is_ok());
    }

    #[test]
    fn email_signup_closed_mode_rejects_missing_or_wrong_admin_key() {
        let no_header = authorize_email_signup(
            true,
            &RegistrationMode::Closed,
            Some("secret"),
            &axum::http::HeaderMap::new(),
        )
        .unwrap_err();
        assert!(matches!(no_header, AppError::Authorization(_)));

        let wrong = authorize_email_signup(
            true,
            &RegistrationMode::Closed,
            Some("secret"),
            &admin_headers("nope"),
        )
        .unwrap_err();
        assert!(matches!(wrong, AppError::Authorization(_)));
    }

    #[test]
    fn email_signup_closed_mode_rejects_when_no_admin_key_is_configured() {
        let err = authorize_email_signup(
            true,
            &RegistrationMode::Closed,
            None,
            &admin_headers("anything"),
        )
        .unwrap_err();
        assert!(matches!(err, AppError::Authorization(_)));
    }

    #[test]
    fn email_signup_invite_mode_is_not_implemented() {
        let err = authorize_email_signup(
            true,
            &RegistrationMode::Invite,
            Some("secret"),
            &admin_headers("secret"),
        )
        .unwrap_err();
        assert!(matches!(err, AppError::Authorization(_)));
    }
}
