//! Authentication middleware
//!
//! This module implements dual authentication:
//! - API key authentication with bcrypt verification (for programmatic access)
//! - JWT token authentication (for dashboard/browser access)
//!
//! JWT tokens start with "eyJ" (base64 encoded header).
//! API keys use "sp_" prefix (new standard) or "sk_" prefix (legacy, for backward compatibility).

use axum::{
    extract::{Extension, Request, State},
    http::StatusCode,
    middleware::Next,
    response::{IntoResponse, Response},
    Json,
};
use chrono::Utc;
use jsonwebtoken::{decode, DecodingKey, Validation};
use sqlx::FromRow;

use crate::api::handlers::auth::Claims;
use crate::api::AppState;
use crate::error::ErrorResponse;
use crate::models::ApiKeyContext;

/// Map an `(StatusCode, String)` auth failure into a JSON error response so
/// SDKs and the dashboard get the same `{code, message}` shape they get for
/// every other API error. Without this, 401s leaked back as `text/plain`,
/// which forced every client to special-case auth failures.
fn auth_error_response(status: StatusCode, message: String) -> Response {
    let code = match status {
        StatusCode::UNAUTHORIZED => "UNAUTHORIZED",
        StatusCode::FORBIDDEN => "ACCESS_DENIED",
        StatusCode::INTERNAL_SERVER_ERROR => "INTERNAL_ERROR",
        _ => "ERROR",
    };
    let body = Json(ErrorResponse {
        code: code.to_string(),
        message,
        details: None,
    });
    (status, body).into_response()
}

/// Type alias for organization context extractor
/// Use `Option<Extension<ApiKeyContext>>` in handlers to get the org context
/// from authenticated requests. Returns None if not authenticated.
pub type OrgContext = Extension<ApiKeyContext>;

/// API key record from database
#[derive(Debug, FromRow)]
struct ApiKeyRecord {
    id: String,
    organization_id: String,
    key_hash: String,
    queues: Vec<String>,
    rate_limit: Option<i32>,
    is_active: bool,
    expires_at: Option<chrono::DateTime<Utc>>,
}

/// Authenticate API key or JWT token middleware
///
/// This middleware accepts both:
/// - API keys (sp_ or legacy sk_ prefix) - for programmatic access
/// - JWT tokens (start with "eyJ") - for dashboard/browser access
///
/// For API keys:
/// 1. Looks up the API key record by a fast hash lookup
/// 2. Verifies the bcrypt hash in Rust (NOT database) for horizontal scaling
/// 3. Checks expiration and active status
/// 4. Updates last_used timestamp asynchronously
///
/// For JWT tokens:
/// 1. Validates the JWT signature and expiration
/// 2. Extracts claims to build the API key context
/// 3. Checks token blacklist (for logged out tokens)
pub async fn authenticate_api_key(
    State(state): State<AppState>,
    mut request: Request,
    next: Next,
) -> Result<Response, Response> {
    // Check Authorization header first
    let token_from_header = request
        .headers()
        .get("Authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|header| header.strip_prefix("Bearer ").map(|s| s.to_string()));

    // Fallback to query parameter 'token' or 'api_key'
    let token = if let Some(t) = token_from_header {
        t
    } else {
        let query = request.uri().query().unwrap_or("");
        let mut found_token = None;
        for pair in query.split('&') {
            let mut parts = pair.splitn(2, '=');
            if let (Some(key), Some(value)) = (parts.next(), parts.next()) {
                if key == "token" || key == "api_key" {
                    found_token = Some(value.to_string());
                    break;
                }
            }
        }

        match found_token {
            Some(t) => t,
            None => {
                return Err(auth_error_response(
                    StatusCode::UNAUTHORIZED,
                    "Missing or invalid Authorization header or token/api_key query parameter"
                        .to_string(),
                ));
            }
        }
    };

    // Detect token type: JWT tokens start with "eyJ" (base64 encoded JSON header)
    let context = if token.starts_with("eyJ") {
        authenticate_jwt_token(&state, &token)
            .await
            .map_err(|(s, m)| auth_error_response(s, m))?
    } else {
        authenticate_api_key_token(&state, &token)
            .await
            .map_err(|(s, m)| auth_error_response(s, m))?
    };

    // Store context for downstream handlers
    request.extensions_mut().insert(context);

    Ok(next.run(request).await)
}

/// Authenticate JWT token and return API key context
async fn authenticate_jwt_token(
    state: &AppState,
    token: &str,
) -> Result<ApiKeyContext, (StatusCode, String)> {
    // Decode and validate JWT
    let token_data = decode::<Claims>(
        token,
        &DecodingKey::from_secret(state.settings.jwt.secret.as_bytes()),
        &Validation::default(),
    )
    .map_err(|e| {
        tracing::debug!(error = %e, "JWT validation failed");
        (StatusCode::UNAUTHORIZED, "Invalid token".to_string())
    })?;

    // Verify it's an access token (not a refresh token)
    if token_data.claims.token_type != "access" {
        return Err((StatusCode::UNAUTHORIZED, "Invalid token type".to_string()));
    }

    // Check if token is blacklisted (logged out)
    if let Some(ref cache) = state.cache {
        let blacklist_key = format!("token_blacklist:{}", token_data.claims.jti);
        if let Ok(Some(_)) = cache.get(&blacklist_key).await {
            tracing::warn!(jti = %token_data.claims.jti, "Attempted to use blacklisted token");
            return Err((
                StatusCode::UNAUTHORIZED,
                "Token has been revoked".to_string(),
            ));
        }
    }

    // Verify the API key is still active
    let api_key_active: Option<(bool, Option<i32>)> =
        sqlx::query_as("SELECT is_active, rate_limit FROM api_keys WHERE id = $1")
            .bind(&token_data.claims.api_key_id)
            .fetch_optional(state.db.pool())
            .await
            .map_err(|e| {
                tracing::error!(error = %e, "Failed to check API key status");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "Database error".to_string(),
                )
            })?;

    match api_key_active {
        Some((true, rate_limit)) => {
            // API key is active, create context from JWT claims
            Ok(ApiKeyContext {
                api_key_id: token_data.claims.api_key_id,
                organization_id: token_data.claims.org_id,
                queues: token_data.claims.queues,
                rate_limit,
            })
        }
        Some((false, _)) => {
            tracing::warn!(
                api_key_id = %token_data.claims.api_key_id,
                "JWT token references revoked API key"
            );
            Err((
                StatusCode::UNAUTHORIZED,
                "API key has been revoked".to_string(),
            ))
        }
        None => {
            tracing::warn!(
                api_key_id = %token_data.claims.api_key_id,
                "JWT token references deleted API key"
            );
            Err((StatusCode::UNAUTHORIZED, "API key not found".to_string()))
        }
    }
}

/// Authenticate API key token and return context
async fn authenticate_api_key_token(
    state: &AppState,
    token: &str,
) -> Result<ApiKeyContext, (StatusCode, String)> {
    // Try cache first for hot keys (99% hit rate expected)
    let api_key = if let Some(ref cache) = state.cache {
        let cache_key = format!("api_key:{}", hash_for_lookup(token));
        match cache.get_json::<ApiKeyRecord>(&cache_key).await {
            Ok(Some(cached)) => {
                // Cache hit - trust it! The cache key already includes token hash.
                // No need to re-verify bcrypt (saves 50-100ms per request).
                // Bcrypt was already verified when initially cached.
                cached
            }
            _ => fetch_and_cache_api_key(state, token).await?,
        }
    } else {
        fetch_api_key(state, token).await?
    };

    // Verify active status
    if !api_key.is_active {
        return Err((StatusCode::UNAUTHORIZED, "API key is inactive".to_string()));
    }

    // Verify expiration
    if let Some(expires) = api_key.expires_at {
        if expires < Utc::now() {
            return Err((StatusCode::UNAUTHORIZED, "API key has expired".to_string()));
        }
    }

    // Update last_used timestamp asynchronously (fire-and-forget)
    let db = state.db.pool_arc();
    let key_id = api_key.id.clone();
    tokio::spawn(async move {
        let _ = sqlx::query("UPDATE api_keys SET last_used = NOW() WHERE id = $1")
            .bind(&key_id)
            .execute(&*db)
            .await;
    });

    Ok(ApiKeyContext {
        api_key_id: api_key.id,
        organization_id: api_key.organization_id,
        queues: api_key.queues,
        rate_limit: api_key.rate_limit,
    })
}

/// Fetch API key from database.
///
/// Fast path: `lookup_hash = sha256(token)` is unique, so a single indexed row read
/// yields the one candidate, which is then bcrypt-verified. This replaces the old
/// prefix scan, where every `sp_live_` key shared the same `key_prefix` so a bogus
/// key cost up to 100 bcrypt verifications (~7.7s) — a CPU-amplification vector.
///
/// Legacy fallback: rows created before `lookup_hash` existed have it NULL and can't
/// be backfilled offline (the plaintext isn't recoverable from bcrypt). For those we
/// fall back to the bounded prefix scan and, on a match, backfill `lookup_hash` so
/// the key uses the fast path from then on. The fallback only scans `lookup_hash IS
/// NULL` rows, so it shrinks to empty as keys re-authenticate after deploy.
async fn fetch_api_key(
    state: &AppState,
    token: &str,
) -> Result<ApiKeyRecord, (StatusCode, String)> {
    let db_err = |e: sqlx::Error| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Database error: {}", e),
        )
    };

    let lookup_hash = crate::models::api_key_lookup_hash(token);

    // Fast path: exactly one row (or none). Mirrors the old scan's `is_active = TRUE`
    // filter; expiry is checked by the caller from the returned `expires_at`.
    let by_lookup: Option<ApiKeyRecord> = sqlx::query_as(
        "SELECT id, organization_id, key_hash, queues, rate_limit, is_active, expires_at
         FROM api_keys
         WHERE lookup_hash = $1 AND is_active = TRUE
         LIMIT 1",
    )
    .bind(&lookup_hash)
    .fetch_optional(state.db.pool())
    .await
    .map_err(db_err)?;

    if let Some(record) = by_lookup {
        // Defense in depth: verify bcrypt even though lookup_hash matched, so a
        // (practically impossible) SHA-256 collision can't authenticate.
        if bcrypt::verify(token, &record.key_hash).unwrap_or(false) {
            return Ok(record);
        }
        // Collision without a bcrypt match: fall through to the legacy scan rather
        // than trust the lookup_hash hit.
    }

    // Legacy fallback: only keys not yet carrying a lookup_hash.
    let key_prefix: String = token.chars().take(8).collect();
    let records: Vec<ApiKeyRecord> = sqlx::query_as(
        "SELECT id, organization_id, key_hash, queues, rate_limit, is_active, expires_at
         FROM api_keys
         WHERE is_active = TRUE
           AND lookup_hash IS NULL
           AND (key_prefix = $1 OR key_prefix IS NULL)
         LIMIT 100",
    )
    .bind(&key_prefix)
    .fetch_all(state.db.pool())
    .await
    .map_err(db_err)?;

    for record in records {
        if bcrypt::verify(token, &record.key_hash).unwrap_or(false) {
            // Backfill lookup_hash so the next auth for this key is O(1). Fire-and-forget;
            // guarded by `lookup_hash IS NULL` so concurrent backfills are idempotent.
            let db = state.db.pool_arc();
            let key_id = record.id.clone();
            let lh = lookup_hash.clone();
            tokio::spawn(async move {
                let _ = sqlx::query(
                    "UPDATE api_keys SET lookup_hash = $1 WHERE id = $2 AND lookup_hash IS NULL",
                )
                .bind(&lh)
                .bind(&key_id)
                .execute(&*db)
                .await;
            });
            return Ok(record);
        }
    }

    Err((StatusCode::UNAUTHORIZED, "Invalid API key".to_string()))
}

/// TTL for cached API-key records. Kept short so that if a key is revoked in the
/// narrow window that races the cache write (revoke invalidates the entry before
/// this write lands), the stale-but-active record self-expires quickly instead of
/// staying valid for an hour.
const API_KEY_CACHE_TTL_SECS: u64 = 60;

/// Fetch API key from database and cache it
async fn fetch_and_cache_api_key(
    state: &AppState,
    token: &str,
) -> Result<ApiKeyRecord, (StatusCode, String)> {
    let record = fetch_api_key(state, token).await?;

    if let Some(ref cache) = state.cache {
        // Re-check is_active immediately before caching. fetch_api_key's SELECT runs
        // BEFORE the (slow) bcrypt verify, so a revoke that commits during bcrypt would
        // otherwise be papered over by caching a stale active record for the full TTL.
        // If the key was revoked in that window, refuse it and do not cache.
        let still_active: Option<(bool,)> =
            sqlx::query_as("SELECT is_active FROM api_keys WHERE id = $1")
                .bind(&record.id)
                .fetch_optional(state.db.pool())
                .await
                .ok()
                .flatten();
        if !matches!(still_active, Some((true,))) {
            return Err((StatusCode::UNAUTHORIZED, "API key is inactive".to_string()));
        }

        let lookup_hash = hash_for_lookup(token);
        let cache_key = format!("api_key:{}", lookup_hash);
        let _ = cache
            .set_json(&cache_key, &record, API_KEY_CACHE_TTL_SECS)
            .await;

        // CRITICAL: Also set reverse mapping (bcrypt hash prefix -> lookup hash)
        // This allows cache invalidation when the API key is revoked.
        let hash_prefix = if record.key_hash.len() >= 16 {
            &record.key_hash[..16]
        } else {
            &record.key_hash
        };
        let reverse_key = format!("api_key_reverse:{}", hash_prefix);
        let _ = cache
            .set(&reverse_key, &lookup_hash, API_KEY_CACHE_TTL_SECS)
            .await;
    }

    Ok(record)
}

/// Generate a lookup hash for the API key (for fast DB/cache lookup)
/// This is NOT for security - just for efficient indexing
fn hash_for_lookup(token: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(token.as_bytes());
    hex::encode(hasher.finalize())[..16].to_string()
}

// Implement Serialize/Deserialize for ApiKeyRecord to cache it
impl serde::Serialize for ApiKeyRecord {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        use serde::ser::SerializeStruct;
        let mut state = serializer.serialize_struct("ApiKeyRecord", 7)?;
        state.serialize_field("id", &self.id)?;
        state.serialize_field("organization_id", &self.organization_id)?;
        state.serialize_field("key_hash", &self.key_hash)?;
        state.serialize_field("queues", &self.queues)?;
        state.serialize_field("rate_limit", &self.rate_limit)?;
        state.serialize_field("is_active", &self.is_active)?;
        state.serialize_field("expires_at", &self.expires_at)?;
        state.end()
    }
}

impl<'de> serde::Deserialize<'de> for ApiKeyRecord {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(serde::Deserialize)]
        struct Helper {
            id: String,
            organization_id: String,
            key_hash: String,
            queues: Vec<String>,
            rate_limit: Option<i32>,
            is_active: bool,
            expires_at: Option<chrono::DateTime<Utc>>,
        }

        let helper = Helper::deserialize(deserializer)?;
        Ok(ApiKeyRecord {
            id: helper.id,
            organization_id: helper.organization_id,
            key_hash: helper.key_hash,
            queues: helper.queues,
            rate_limit: helper.rate_limit,
            is_active: helper.is_active,
            expires_at: helper.expires_at,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_hash_for_lookup() {
        let hash1 = hash_for_lookup("sp_test_abc123");
        let hash2 = hash_for_lookup("sp_test_abc123");
        let hash3 = hash_for_lookup("sp_test_xyz789");

        assert_eq!(hash1, hash2);
        assert_ne!(hash1, hash3);
        assert_eq!(hash1.len(), 16);
    }
}
