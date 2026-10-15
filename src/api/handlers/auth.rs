//! Authentication handlers
//!
//! This module provides JWT-based authentication endpoints including:
//! - Login (API key to JWT exchange)
//! - Token refresh
//! - Logout (token invalidation)
//! - Current user info

use axum::{
    body::Bytes,
    extract::{Extension, State},
    http::StatusCode,
    Json,
};
use chrono::{DateTime, Duration, Utc};
use jsonwebtoken::{decode, encode, DecodingKey, EncodingKey, Header, TokenData, Validation};
use serde::{Deserialize, Serialize};
use tracing::{error, info, warn};
use validator::Validate;

use crate::api::middleware::validation::ValidatedJson;
use crate::api::AppState;
use crate::models::ApiKeyContext;

/// JWT Claims structure
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Claims {
    /// Subject (organization ID)
    pub sub: String,
    /// API Key ID
    pub api_key_id: String,
    /// Organization ID (duplicated for convenience)
    pub org_id: String,
    /// Issued at (Unix timestamp)
    pub iat: i64,
    /// Expiration time (Unix timestamp)
    pub exp: i64,
    /// Not before (Unix timestamp)
    pub nbf: i64,
    /// JWT ID (unique token identifier)
    pub jti: String,
    /// Allowed queues
    pub queues: Vec<String>,
    /// Token type (access or refresh)
    pub token_type: String,
    /// Login session id, shared by the access and refresh token a login mints
    /// and by every access token later refreshed from them. Logout revokes the
    /// session, so a refresh token cannot outlive it. Absent on tokens issued
    /// before 0.1.114, which fall back to per-token (`jti`) revocation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sid: Option<String>,
}

/// Redis key that marks a whole login session as logged out.
pub fn session_revocation_key(sid: &str) -> String {
    format!("session_revoked:{}", sid)
}

/// Whether a token was revoked by logout — the token itself (its `jti`) or the
/// login session it belongs to (its `sid`).
///
/// `revoked_sessions` is the source of truth for sessions; Redis is a cache of
/// it plus the `jti` blacklist. A missing Redis key is not enough —
/// allkeys-lru can evict `session_revoked:{sid}` while the refresh token is
/// still inside its exp — so a session id is always checked in Postgres.
///
/// A Redis error falls back to Postgres for tokens with a session id (logout
/// revokes the whole session there). Sid-less tokens have only the Redis
/// blacklist, so a Redis error fails closed for them. A Postgres error is
/// returned to the caller as unavailable: reporting it as "revoked" would log
/// every dashboard user out on a database blip.
pub async fn is_token_revoked(
    cache: Option<&crate::cache::RedisCache>,
    pool: &sqlx::PgPool,
    jti: &str,
    sid: Option<&str>,
) -> Result<bool, sqlx::Error> {
    if let Some(cache) = cache {
        let mut keys = vec![format!("token_blacklist:{}", jti)];
        if let Some(sid) = sid {
            keys.push(session_revocation_key(sid));
        }
        for key in &keys {
            match cache.get(key).await {
                Ok(Some(_)) => return Ok(true),
                Ok(None) => {}
                Err(e) if sid.is_some() => {
                    warn!(error = %e, "Token revocation lookup failed in Redis; checking Postgres");
                    break;
                }
                Err(e) => {
                    warn!(error = %e, "Token blacklist lookup failed; treating the sid-less token as revoked");
                    return Ok(true);
                }
            }
        }
    }

    let Some(sid) = sid else {
        return Ok(false);
    };
    let revoked = sqlx::query_scalar::<_, i32>(
        "SELECT 1 FROM revoked_sessions WHERE sid = $1 AND expires_at > NOW()",
    )
    .bind(sid)
    .fetch_optional(pool)
    .await?;
    Ok(revoked.is_some())
}

/// Login request
#[derive(Debug, Deserialize, Validate)]
pub struct LoginRequest {
    /// API key for authentication
    #[validate(length(min = 10, message = "API key must be at least 10 characters"))]
    pub api_key: String,
}

/// Login response
#[derive(Debug, Serialize)]
pub struct LoginResponse {
    /// Access token (short-lived)
    pub access_token: String,
    /// Refresh token (longer-lived)
    pub refresh_token: String,
    /// Token type (always "Bearer")
    pub token_type: String,
    /// Access token expiration in seconds
    pub expires_in: i64,
    /// Refresh token expiration in seconds
    pub refresh_expires_in: i64,
}

/// Token refresh request
#[derive(Debug, Deserialize, Validate)]
pub struct RefreshTokenRequest {
    /// Refresh token
    #[validate(length(min = 10, message = "Invalid refresh token"))]
    pub refresh_token: String,
}

/// Token refresh response
#[derive(Debug, Serialize)]
pub struct RefreshTokenResponse {
    /// New access token
    pub access_token: String,
    /// Token type
    pub token_type: String,
    /// Access token expiration in seconds
    pub expires_in: i64,
}

/// Current user info response
#[derive(Debug, Serialize)]
pub struct CurrentUserResponse {
    /// Organization ID
    pub organization_id: String,
    /// API Key ID
    pub api_key_id: String,
    /// Allowed queues
    pub queues: Vec<String>,
    /// Token issued at
    pub issued_at: chrono::DateTime<Utc>,
    /// Token expires at
    pub expires_at: chrono::DateTime<Utc>,
    /// Organization details
    pub organization: Option<OrganizationInfo>,
}

/// Organization info for /auth/me response
#[derive(Debug, Serialize)]
pub struct OrganizationInfo {
    /// Organization ID
    pub id: String,
    /// Organization name
    pub name: String,
    /// Organization slug
    pub slug: String,
    /// Current plan tier
    pub plan_tier: String,
    /// Billing email
    pub billing_email: Option<String>,
}

/// Maximum login attempts per key prefix per minute
const LOGIN_RATE_LIMIT: i64 = 5;
/// Rate limit window in seconds
const LOGIN_RATE_LIMIT_WINDOW: i64 = 60;

/// Login handler - exchange API key for JWT tokens
///
/// POST /api/v1/auth/login
///
/// Previously fetched ANY active API key instead of the provided one.
/// Now resolves the key exactly as the auth middleware does.
///
/// Key format: sp_live_XXXXX or sp_test_XXXXX
///
/// `503` with `Retry-After` when the key could not be checked (the legacy-key
/// scan stayed busy, or the database is unreachable): a retry can succeed, so
/// it must not look like a bad key (401) or a server bug (500).
pub async fn login(
    State(state): State<AppState>,
    crate::api::middleware::client_ip::PeerAddr(peer): crate::api::middleware::client_ip::PeerAddr,
    headers: axum::http::HeaderMap,
    ValidatedJson(req): ValidatedJson<LoginRequest>,
) -> Result<Json<LoginResponse>, axum::response::Response> {
    use axum::response::IntoResponse;

    login_inner(state, peer, headers, req)
        .await
        .map_err(|(status, body)| {
            let mut response = (status, body).into_response();
            if status == StatusCode::SERVICE_UNAVAILABLE {
                response.headers_mut().insert(
                    axum::http::header::RETRY_AFTER,
                    axum::http::HeaderValue::from(
                        crate::api::middleware::auth::AUTH_RETRY_AFTER_SECS,
                    ),
                );
            }
            response
        })
}

async fn login_inner(
    state: AppState,
    peer: Option<std::net::IpAddr>,
    headers: axum::http::HeaderMap,
    req: LoginRequest,
) -> Result<Json<LoginResponse>, (StatusCode, Json<ErrorResponse>)> {
    // `sp_live_` / `sp_test_`: safe to log, unlike the key itself.
    let key_prefix = req.api_key.chars().take(8).collect::<String>();

    // Rate limit login attempts to prevent brute force.
    // Bucket on a hash of the FULL submitted key: bucketing on the 8-char
    // prefix put every "sp_live_" key in one shared bucket, so 5 failed
    // attempts by anyone locked out logins for every customer (and gave
    // attackers a trivial login DoS).
    let rate_limit_id = {
        use sha2::{Digest, Sha256};
        let mut hasher = Sha256::new();
        hasher.update(req.api_key.as_bytes());
        hex::encode(&hasher.finalize()[..8])
    };

    if let Some(ref cache) = state.cache {
        let rate_key = format!("login_attempts:{}", rate_limit_id);

        // Check current attempt count
        if let Ok(Some(count_str)) = cache.get(&rate_key).await {
            if let Ok(count) = count_str.parse::<i64>() {
                if count >= LOGIN_RATE_LIMIT {
                    warn!(prefix = %key_prefix, "Login rate limit exceeded");
                    return Err((
                        StatusCode::TOO_MANY_REQUESTS,
                        Json(ErrorResponse::rate_limited()),
                    ));
                }
            }
        }

        // Increment attempt count
        let _ = cache
            .increment(&rate_key, LOGIN_RATE_LIMIT_WINDOW as u64)
            .await;
    }

    // Resolve the key through the auth middleware's path: record cache, negative
    // cache, the indexed lookup_hash, then the bounded legacy scan behind the
    // per-address failed-auth budget. /auth/login used to carry its own copy, with
    // bcrypt inline on the async worker; it is UNAUTHENTICATED, so a bogus key here
    // is as cheap to send as anywhere.
    let client_ip = crate::api::middleware::client_ip::client_ip_from_peer(
        &headers,
        peer,
        &state.settings.trusted_proxy,
    );
    let api_key = match crate::api::middleware::auth::resolve_api_key(
        state.db.pool(),
        state.cache.as_ref(),
        &req.api_key,
        crate::api::middleware::auth::FailedAuthBudget::ClientIp(&client_ip),
    )
    .await
    {
        Ok(key) if key.is_active => key,
        Ok(_) | Err((StatusCode::UNAUTHORIZED, _)) => {
            warn!(prefix = %key_prefix, "Login attempt with invalid API key");
            return Err((
                StatusCode::UNAUTHORIZED,
                Json(ErrorResponse::unauthorized()),
            ));
        }
        // Not checked: the legacy scan stayed busy or the database is down.
        Err((StatusCode::SERVICE_UNAVAILABLE, _)) => {
            return Err((
                StatusCode::SERVICE_UNAVAILABLE,
                Json(ErrorResponse::unavailable()),
            ));
        }
        // Logged where it happened.
        Err(_) => {
            return Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ErrorResponse::internal()),
            ));
        }
    };

    // Check expiration
    if let Some(expires_at) = api_key.expires_at {
        if expires_at < Utc::now() {
            warn!(api_key_id = %api_key.id, "Login attempt with expired API key");
            return Err((
                StatusCode::UNAUTHORIZED,
                Json(ErrorResponse::unauthorized()),
            ));
        }
    }

    // Generate tokens
    let now = Utc::now();
    let access_expiration = state.settings.jwt.expiration_hours as i64;
    let refresh_expiration = access_expiration * 24; // Refresh token lasts 24x longer

    let sid = uuid::Uuid::new_v4().to_string();
    let access_claims = Claims {
        sub: api_key.organization_id.clone(),
        api_key_id: api_key.id.clone(),
        org_id: api_key.organization_id.clone(),
        iat: now.timestamp(),
        exp: (now + Duration::hours(access_expiration)).timestamp(),
        nbf: now.timestamp(),
        jti: uuid::Uuid::new_v4().to_string(),
        queues: api_key.queues.clone(),
        token_type: "access".to_string(),
        sid: Some(sid.clone()),
    };

    let refresh_claims = Claims {
        sub: api_key.organization_id.clone(),
        api_key_id: api_key.id.clone(),
        org_id: api_key.organization_id.clone(),
        iat: now.timestamp(),
        exp: (now + Duration::hours(refresh_expiration)).timestamp(),
        nbf: now.timestamp(),
        jti: uuid::Uuid::new_v4().to_string(),
        queues: api_key.queues.clone(),
        token_type: "refresh".to_string(),
        sid: Some(sid),
    };

    let access_token = encode(
        &Header::default(),
        &access_claims,
        &EncodingKey::from_secret(state.settings.jwt.secret.as_bytes()),
    )
    .map_err(|e| {
        error!(error = %e, "Failed to encode access token");
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorResponse::internal()),
        )
    })?;

    let refresh_token = encode(
        &Header::default(),
        &refresh_claims,
        &EncodingKey::from_secret(state.settings.jwt.secret.as_bytes()),
    )
    .map_err(|e| {
        error!(error = %e, "Failed to encode refresh token");
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorResponse::internal()),
        )
    })?;

    // Update last_used timestamp
    let key_id = api_key.id.clone();
    let db = state.db.pool_arc();
    tokio::spawn(async move {
        let _ = sqlx::query("UPDATE api_keys SET last_used = NOW() WHERE id = $1")
            .bind(&key_id)
            .execute(&*db)
            .await;
    });

    info!(api_key_id = %api_key.id, org_id = %api_key.organization_id, "Successful login");

    Ok(Json(LoginResponse {
        access_token,
        refresh_token,
        token_type: "Bearer".to_string(),
        expires_in: access_expiration * 3600,
        refresh_expires_in: refresh_expiration * 3600,
    }))
}

/// Refresh token handler - exchange refresh token for new access token
///
/// POST /api/v1/auth/refresh
pub async fn refresh_token(
    State(state): State<AppState>,
    ValidatedJson(req): ValidatedJson<RefreshTokenRequest>,
) -> Result<Json<RefreshTokenResponse>, (StatusCode, Json<ErrorResponse>)> {
    // Decode and validate refresh token
    let token_data: TokenData<Claims> = decode(
        &req.refresh_token,
        &DecodingKey::from_secret(state.settings.jwt.secret.as_bytes()),
        &Validation::default(),
    )
    .map_err(|e| {
        warn!(error = %e, "Invalid refresh token");
        (
            StatusCode::UNAUTHORIZED,
            Json(ErrorResponse::unauthorized()),
        )
    })?;

    // Verify it's a refresh token
    if token_data.claims.token_type != "refresh" {
        warn!("Attempted to refresh with non-refresh token");
        return Err((
            StatusCode::UNAUTHORIZED,
            Json(ErrorResponse::unauthorized()),
        ));
    }

    // Redis plus the durable revoked_sessions row. A Redis miss is not "still
    // logged in": the session key may have been evicted.
    let revoked = is_token_revoked(
        state.cache.as_ref(),
        state.db.pool(),
        &token_data.claims.jti,
        token_data.claims.sid.as_deref(),
    )
    .await
    .map_err(|e| {
        error!(error = %e, "Failed to check session revocation on refresh");
        (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(ErrorResponse::unavailable()),
        )
    })?;
    if revoked {
        warn!(jti = %token_data.claims.jti, "Attempted to use revoked refresh token");
        return Err((
            StatusCode::UNAUTHORIZED,
            Json(ErrorResponse::unauthorized()),
        ));
    }

    // Verify API key is still active before refreshing
    // Previously, revoked API keys could still use their refresh tokens
    let api_key_active: Option<(bool,)> =
        sqlx::query_as("SELECT is_active FROM api_keys WHERE id = $1")
            .bind(&token_data.claims.api_key_id)
            .fetch_optional(state.db.pool())
            .await
            .map_err(|e| {
                error!(error = %e, "Failed to check API key status");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(ErrorResponse::internal()),
                )
            })?;

    match api_key_active {
        Some((true,)) => {
            // API key is active, proceed with refresh
        }
        Some((false,)) => {
            warn!(
                api_key_id = %token_data.claims.api_key_id,
                "Attempted to refresh token for revoked API key"
            );
            return Err((
                StatusCode::UNAUTHORIZED,
                Json(ErrorResponse::unauthorized()),
            ));
        }
        None => {
            warn!(
                api_key_id = %token_data.claims.api_key_id,
                "Attempted to refresh token for deleted API key"
            );
            return Err((
                StatusCode::UNAUTHORIZED,
                Json(ErrorResponse::unauthorized()),
            ));
        }
    }

    // Also check if API key has expired
    let api_key_expiry: Option<(Option<DateTime<Utc>>,)> =
        sqlx::query_as("SELECT expires_at FROM api_keys WHERE id = $1")
            .bind(&token_data.claims.api_key_id)
            .fetch_optional(state.db.pool())
            .await
            .map_err(|e| {
                error!(error = %e, "Failed to check API key expiration");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(ErrorResponse::internal()),
                )
            })?;

    if let Some((Some(expires_at),)) = api_key_expiry {
        if expires_at < Utc::now() {
            warn!(
                api_key_id = %token_data.claims.api_key_id,
                "Attempted to refresh token for expired API key"
            );
            return Err((
                StatusCode::UNAUTHORIZED,
                Json(ErrorResponse::unauthorized()),
            ));
        }
    }

    // Generate new access token
    let now = Utc::now();
    let access_expiration = state.settings.jwt.expiration_hours as i64;

    let access_claims = Claims {
        sub: token_data.claims.sub,
        api_key_id: token_data.claims.api_key_id,
        org_id: token_data.claims.org_id,
        iat: now.timestamp(),
        exp: (now + Duration::hours(access_expiration)).timestamp(),
        nbf: now.timestamp(),
        jti: uuid::Uuid::new_v4().to_string(),
        queues: token_data.claims.queues,
        token_type: "access".to_string(),
        sid: token_data.claims.sid,
    };

    let access_token = encode(
        &Header::default(),
        &access_claims,
        &EncodingKey::from_secret(state.settings.jwt.secret.as_bytes()),
    )
    .map_err(|e| {
        error!(error = %e, "Failed to encode access token");
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorResponse::internal()),
        )
    })?;

    info!(org_id = %access_claims.org_id, "Token refreshed successfully");

    Ok(Json(RefreshTokenResponse {
        access_token,
        token_type: "Bearer".to_string(),
        expires_in: access_expiration * 3600,
    }))
}

/// Logout request body (optional)
#[derive(Debug, Deserialize)]
pub struct LogoutRequest {
    /// Refresh token to invalidate together with the access token. Without it
    /// the refresh token stays usable until it expires — clients should always
    /// send it.
    pub refresh_token: Option<String>,
}

/// Logout handler - invalidate tokens
///
/// POST /api/v1/auth/logout
///
/// Body is optional. Clients often send `Content-Type: application/json` with an
/// empty body; soft-parse so that does not become 422.
pub async fn logout(
    State(state): State<AppState>,
    Extension(context): Extension<ApiKeyContext>,
    headers: axum::http::HeaderMap,
    body: Bytes,
) -> Result<StatusCode, (StatusCode, Json<ErrorResponse>)> {
    // Extract token from Authorization header
    let auth_header = headers
        .get("Authorization")
        .and_then(|v| v.to_str().ok())
        .ok_or((
            StatusCode::UNAUTHORIZED,
            Json(ErrorResponse::unauthorized()),
        ))?;

    let token = auth_header.strip_prefix("Bearer ").ok_or((
        StatusCode::UNAUTHORIZED,
        Json(ErrorResponse::unauthorized()),
    ))?;

    // Decode token to get JTI
    let token_data: TokenData<Claims> = decode(
        token,
        &DecodingKey::from_secret(state.settings.jwt.secret.as_bytes()),
        &Validation::default(),
    )
    .map_err(|_| {
        (
            StatusCode::UNAUTHORIZED,
            Json(ErrorResponse::unauthorized()),
        )
    })?;

    // Durable session revocation first. Redis is only a cache of this row:
    // eviction or a later Redis outage must not resurrect the refresh token.
    // Sid-less tokens (issued before sessions) still depend on the Redis jti
    // blacklist, so a Redis failure for those stays a 503.
    let sid = token_data.claims.sid.clone();
    if let Some(ref sid) = sid {
        let session_ttl = state
            .settings
            .jwt
            .expiration_hours
            .saturating_mul(24 * 3600)
            .max(1);
        let expires_at = Utc::now() + Duration::seconds(session_ttl as i64);
        if let Err(e) = sqlx::query("DELETE FROM revoked_sessions WHERE expires_at <= NOW()")
            .execute(state.db.pool())
            .await
        {
            warn!(error = %e, "Failed to prune revoked_sessions");
        }
        if let Err(e) = sqlx::query(
            "INSERT INTO revoked_sessions (sid, expires_at) VALUES ($1, $2) \
             ON CONFLICT (sid) DO UPDATE SET expires_at = EXCLUDED.expires_at",
        )
        .bind(sid)
        .bind(expires_at)
        .execute(state.db.pool())
        .await
        {
            error!(error = %e, "Failed to persist session revocation on logout");
            return Err((
                StatusCode::SERVICE_UNAVAILABLE,
                Json(ErrorResponse::internal()),
            ));
        }
    }

    let unavailable = (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(ErrorResponse::internal()),
    );
    // Redis records are best-effort once the session row is durable. Without a
    // sid, they are the only revocation, so a Redis failure is still a 503.
    let Some(ref cache) = state.cache else {
        if sid.is_some() {
            info!(org_id = %context.organization_id, "User logged out");
            return Ok(StatusCode::NO_CONTENT);
        }
        error!(org_id = %context.organization_id, "Logout requested but Redis is unavailable; tokens cannot be invalidated");
        return Err(unavailable);
    };
    let redis_result = {
        let blacklist_key = format!("token_blacklist:{}", token_data.claims.jti);
        let ttl = (token_data.claims.exp - Utc::now().timestamp()).max(1) as u64;
        let access_blacklisted = cache.set(&blacklist_key, "1", ttl).await;
        let session_cached = if access_blacklisted.is_ok() {
            if let Some(ref sid) = sid {
                let session_ttl = state
                    .settings
                    .jwt
                    .expiration_hours
                    .saturating_mul(24 * 3600)
                    .max(1);
                cache
                    .set(&session_revocation_key(sid), "1", session_ttl)
                    .await
            } else {
                Ok(())
            }
        } else {
            access_blacklisted
        };

        // Soft-parse optional body so empty JSON + Content-Type still logs out.
        // Blacklist the refresh token as well when the client supplies it.
        let logout_body: Option<LogoutRequest> = if body.is_empty() {
            None
        } else {
            serde_json::from_slice(&body).ok()
        };
        let refresh_blacklisted = if session_cached.is_ok() {
            if let Some(LogoutRequest {
                refresh_token: Some(refresh_token),
            }) = logout_body
            {
                match decode::<Claims>(
                    &refresh_token,
                    &DecodingKey::from_secret(state.settings.jwt.secret.as_bytes()),
                    &Validation::default(),
                ) {
                    Ok(refresh_data) if refresh_data.claims.token_type == "refresh" => {
                        let key = format!("token_blacklist:{}", refresh_data.claims.jti);
                        let ttl = (refresh_data.claims.exp - Utc::now().timestamp()).max(1) as u64;
                        cache.set(&key, "1", ttl).await
                    }
                    _ => {
                        warn!(org_id = %context.organization_id, "Ignoring invalid refresh token supplied to logout");
                        Ok(())
                    }
                }
            } else {
                Ok(())
            }
        } else {
            session_cached
        };
        refresh_blacklisted
    };

    if let Err(e) = redis_result {
        if sid.is_some() {
            // The session row covers the refresh token. Redis can catch up.
            warn!(error = %e, org_id = %context.organization_id, "Redis revocation failed after the session was revoked in the database");
        } else {
            error!(error = %e, "Failed to revoke token in Redis on logout");
            return Err(unavailable);
        }
    }

    info!(org_id = %context.organization_id, "User logged out");

    Ok(StatusCode::NO_CONTENT)
}

/// Get current user info from JWT
///
/// GET /api/v1/auth/me
///
/// Now checks token blacklist before returning user info.
/// Previously, a logged-out token could still access this endpoint.
/// Now includes full organization details in the response.
pub async fn me(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
) -> Result<Json<CurrentUserResponse>, (StatusCode, Json<ErrorResponse>)> {
    let auth_header = headers
        .get("Authorization")
        .and_then(|v| v.to_str().ok())
        .ok_or((
            StatusCode::UNAUTHORIZED,
            Json(ErrorResponse::unauthorized()),
        ))?;

    let token = auth_header.strip_prefix("Bearer ").ok_or((
        StatusCode::UNAUTHORIZED,
        Json(ErrorResponse::unauthorized()),
    ))?;

    // Same checks as every other authenticated route: access token, revocation,
    // key active and unexpired, org not deleted. /auth/me used to skip key
    // expiry, so an expired key's unexpired JWT still returned the org.
    let ctx = crate::api::middleware::auth::authenticate_jwt_token(&state, token)
        .await
        .map_err(|(status, _message)| {
            let body = match status {
                StatusCode::SERVICE_UNAVAILABLE => ErrorResponse::unavailable(),
                StatusCode::INTERNAL_SERVER_ERROR => ErrorResponse::internal(),
                _ => ErrorResponse::unauthorized(),
            };
            (status, Json(body))
        })?;

    let token_data: TokenData<Claims> = decode(
        token,
        &DecodingKey::from_secret(state.settings.jwt.secret.as_bytes()),
        &Validation::default(),
    )
    .map_err(|_| {
        (
            StatusCode::UNAUTHORIZED,
            Json(ErrorResponse::unauthorized()),
        )
    })?;

    // Fetch organization details
    let org_info: Option<(String, String, String, String, Option<String>)> = sqlx::query_as(
        "SELECT id, name, slug, plan_tier, billing_email FROM organizations WHERE id = $1",
    )
    .bind(&ctx.organization_id)
    .fetch_optional(state.db.pool())
    .await
    .map_err(|e| {
        error!(error = %e, "Failed to fetch organization for /auth/me");
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorResponse::internal()),
        )
    })?;

    let organization =
        org_info.map(
            |(id, name, slug, plan_tier, billing_email)| OrganizationInfo {
                id,
                name,
                slug,
                plan_tier,
                billing_email,
            },
        );

    Ok(Json(CurrentUserResponse {
        organization_id: ctx.organization_id,
        api_key_id: ctx.api_key_id,
        queues: ctx.queues,
        issued_at: chrono::DateTime::from_timestamp(token_data.claims.iat, 0)
            .unwrap_or_else(Utc::now),
        expires_at: chrono::DateTime::from_timestamp(token_data.claims.exp, 0)
            .unwrap_or_else(Utc::now),
        organization,
    }))
}

/// Validate a JWT token
///
/// POST /api/v1/auth/validate
pub async fn validate_token(
    State(state): State<AppState>,
    Json(body): Json<ValidateTokenRequest>,
) -> Result<Json<ValidateTokenResponse>, (StatusCode, Json<ErrorResponse>)> {
    // Same acceptance rule as the auth middleware. A signature-only check used
    // to report valid:true for refresh tokens, revoked keys, and expired keys.
    // 500/503 stay errors: reporting them as valid:false would log the user out.
    match crate::api::middleware::auth::authenticate_jwt_token(&state, &body.token).await {
        Ok(_) => {
            let token_data = decode::<Claims>(
                &body.token,
                &DecodingKey::from_secret(state.settings.jwt.secret.as_bytes()),
                &Validation::default(),
            )
            .map_err(|_| {
                (
                    StatusCode::UNAUTHORIZED,
                    Json(ErrorResponse::unauthorized()),
                )
            })?;
            Ok(Json(ValidateTokenResponse {
                valid: true,
                error: None,
                claims: Some(token_data.claims),
            }))
        }
        Err((status, message)) if status == StatusCode::UNAUTHORIZED => {
            Ok(Json(ValidateTokenResponse {
                valid: false,
                error: Some(message),
                claims: None,
            }))
        }
        Err((status, _)) => {
            let body = if status == StatusCode::SERVICE_UNAVAILABLE {
                ErrorResponse::unavailable()
            } else {
                ErrorResponse::internal()
            };
            Err((status, Json(body)))
        }
    }
}

/// Token validation request
#[derive(Debug, Deserialize)]
pub struct ValidateTokenRequest {
    pub token: String,
}

/// Token validation response
#[derive(Debug, Serialize)]
pub struct ValidateTokenResponse {
    pub valid: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub claims: Option<Claims>,
}

/// Standard error response
#[derive(Debug, Serialize)]
pub struct ErrorResponse {
    pub error: String,
    pub message: String,
}

impl ErrorResponse {
    pub fn unauthorized() -> Self {
        Self {
            error: "unauthorized".to_string(),
            message: "Invalid credentials".to_string(),
        }
    }

    pub fn internal() -> Self {
        Self {
            error: "internal_error".to_string(),
            message: "An internal error occurred".to_string(),
        }
    }

    /// The key could not be checked right now; retry after `Retry-After`.
    pub fn unavailable() -> Self {
        Self {
            error: "service_unavailable".to_string(),
            message: "Authentication is temporarily unavailable".to_string(),
        }
    }

    /// Rate limit error response
    pub fn rate_limited() -> Self {
        Self {
            error: "rate_limit_exceeded".to_string(),
            message: "Too many login attempts. Please try again later.".to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_claims_without_sid_still_decode() {
        // Tokens issued before 0.1.114 carry no `sid`; they must keep working
        // (and keep per-jti revocation) instead of failing to decode.
        let legacy = serde_json::json!({
            "sub": "org", "api_key_id": "key", "org_id": "org",
            "iat": 1, "exp": 2, "nbf": 1, "jti": "j",
            "queues": [], "token_type": "refresh"
        });
        let claims: Claims = serde_json::from_value(legacy).unwrap();
        assert!(claims.sid.is_none());
        // And a sid-less token serializes without the field.
        assert!(!serde_json::to_string(&claims).unwrap().contains("sid"));
        assert_eq!(session_revocation_key("abc"), "session_revoked:abc");
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
            queues: vec!["emails".to_string(), "notifications".to_string()],
            token_type: "access".to_string(),
            sid: Some("sid-1".to_string()),
        };

        let json = serde_json::to_string(&claims).unwrap();
        assert!(json.contains("org-123"));
        assert!(json.contains("access"));

        let decoded: Claims = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.sub, claims.sub);
        assert_eq!(decoded.queues, claims.queues);
    }

    #[test]
    fn test_login_request_validation() {
        let valid_req = LoginRequest {
            api_key: "sp_test_abc123def456".to_string(),
        };
        assert!(valid_req.validate().is_ok());

        let invalid_req = LoginRequest {
            api_key: "short".to_string(),
        };
        assert!(invalid_req.validate().is_err());
    }

    #[test]
    fn test_login_response_structure() {
        let response = LoginResponse {
            access_token: "eyJ...".to_string(),
            refresh_token: "eyJ...".to_string(),
            token_type: "Bearer".to_string(),
            expires_in: 3600,
            refresh_expires_in: 86400,
        };

        let json = serde_json::to_string(&response).unwrap();
        assert!(json.contains("Bearer"));
        assert!(json.contains("access_token"));
    }

    #[test]
    fn test_error_response() {
        let unauthorized = ErrorResponse::unauthorized();
        assert_eq!(unauthorized.error, "unauthorized");

        let internal = ErrorResponse::internal();
        assert_eq!(internal.error, "internal_error");
    }

    #[test]
    fn test_validate_token_response_serialization() {
        // Valid response
        let valid = ValidateTokenResponse {
            valid: true,
            error: None,
            claims: Some(Claims {
                sub: "org-123".to_string(),
                api_key_id: "key-456".to_string(),
                org_id: "org-123".to_string(),
                iat: 1700000000,
                exp: 1700003600,
                nbf: 1700000000,
                jti: "jti-789".to_string(),
                queues: vec![],
                token_type: "access".to_string(),
                sid: None,
            }),
        };

        let json = serde_json::to_string(&valid).unwrap();
        assert!(json.contains("true"));
        assert!(json.contains("claims"));
        assert!(!json.contains("error"));

        // Invalid response
        let invalid = ValidateTokenResponse {
            valid: false,
            error: Some("Token expired".to_string()),
            claims: None,
        };

        let json = serde_json::to_string(&invalid).unwrap();
        assert!(json.contains("false"));
        assert!(json.contains("Token expired"));
        assert!(!json.contains("claims"));
    }

    #[test]
    fn test_organization_info_serialization() {
        let org = OrganizationInfo {
            id: "org-123".to_string(),
            name: "My Organization".to_string(),
            slug: "my-org".to_string(),
            plan_tier: "starter".to_string(),
            billing_email: Some("billing@example.com".to_string()),
        };

        let json = serde_json::to_string(&org).unwrap();
        assert!(json.contains("org-123"));
        assert!(json.contains("My Organization"));
        assert!(json.contains("my-org"));
        assert!(json.contains("starter"));
        assert!(json.contains("billing@example.com"));
    }

    #[test]
    fn test_organization_info_without_billing_email() {
        let org = OrganizationInfo {
            id: "org-456".to_string(),
            name: "Test Org".to_string(),
            slug: "test-org".to_string(),
            plan_tier: "free".to_string(),
            billing_email: None,
        };

        let json = serde_json::to_string(&org).unwrap();
        assert!(json.contains("org-456"));
        assert!(json.contains("free"));
        // billing_email should be null
        assert!(json.contains("null") || !json.contains("billing_email"));
    }

    #[test]
    fn test_current_user_response_with_organization() {
        let response = CurrentUserResponse {
            organization_id: "org-123".to_string(),
            api_key_id: "key-456".to_string(),
            queues: vec!["*".to_string()],
            issued_at: Utc::now(),
            expires_at: Utc::now(),
            organization: Some(OrganizationInfo {
                id: "org-123".to_string(),
                name: "My Org".to_string(),
                slug: "my-org".to_string(),
                plan_tier: "pro".to_string(),
                billing_email: None,
            }),
        };

        let json = serde_json::to_string(&response).unwrap();
        assert!(json.contains("org-123"));
        assert!(json.contains("key-456"));
        assert!(json.contains("organization"));
        assert!(json.contains("My Org"));
        assert!(json.contains("pro"));
    }

    #[test]
    fn test_current_user_response_without_organization() {
        let response = CurrentUserResponse {
            organization_id: "org-123".to_string(),
            api_key_id: "key-456".to_string(),
            queues: vec!["emails".to_string()],
            issued_at: Utc::now(),
            expires_at: Utc::now(),
            organization: None,
        };

        let json = serde_json::to_string(&response).unwrap();
        assert!(json.contains("org-123"));
        assert!(json.contains("key-456"));
        assert!(json.contains("emails"));
        // organization should be null
        assert!(json.contains("null"));
    }

    #[test]
    fn test_rate_limited_error_response() {
        let rate_limited = ErrorResponse::rate_limited();
        assert_eq!(rate_limited.error, "rate_limit_exceeded");
        assert!(rate_limited.message.contains("Too many"));
    }
}
