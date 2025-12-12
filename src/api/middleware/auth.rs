//! Authentication middleware
//!
//! This module implements dual authentication:
//! - API key authentication with bcrypt verification (for programmatic access)
//! - JWT token authentication (for dashboard/browser access)
//!
//! JWT tokens start with "eyJ" (base64 encoded header), API keys start with "sk_"

use axum::{
    extract::{Extension, Request, State},
    http::StatusCode,
    middleware::Next,
    response::Response,
};
use chrono::Utc;
use jsonwebtoken::{decode, DecodingKey, Validation};
use sqlx::FromRow;

use crate::api::handlers::auth::Claims;
use crate::api::AppState;
use crate::models::ApiKeyContext;

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
/// - API keys (start with "sk_") - for programmatic access
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
) -> Result<Response, (StatusCode, String)> {
    let auth_header = request
        .headers()
        .get("Authorization")
        .and_then(|v| v.to_str().ok());

    let token = match auth_header {
        Some(header) if header.starts_with("Bearer ") => &header[7..],
        _ => {
            return Err((
                StatusCode::UNAUTHORIZED,
                "Missing or invalid Authorization header".to_string(),
            ));
        }
    };

    // Detect token type: JWT tokens start with "eyJ" (base64 encoded JSON header)
    let context = if token.starts_with("eyJ") {
        // JWT token authentication
        authenticate_jwt_token(&state, token).await?
    } else {
        // API key authentication (starts with "sk_")
        authenticate_api_key_token(&state, token).await?
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
                // CRITICAL: Verify the cached key matches the provided token
                if bcrypt::verify(token, &cached.key_hash).unwrap_or(false) {
                    cached
                } else {
                    // Cache hit but wrong key - fetch fresh
                    fetch_and_cache_api_key(state, token).await?
                }
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

/// Fetch API key from database
///
/// Uses key_prefix (first 8 chars) for indexed lookup before bcrypt verification.
/// This prevents DoS attacks where an attacker could slow down authentication
/// by creating many API keys (each requiring bcrypt verification).
/// With key_prefix filtering, we only verify a small subset of keys (typically 1-2).
async fn fetch_api_key(
    state: &AppState,
    token: &str,
) -> Result<ApiKeyRecord, (StatusCode, String)> {
    // Extract key_prefix (first 8 chars) for efficient indexed lookup
    let key_prefix: String = token.chars().take(8).collect();

    // Use key_prefix for indexed lookup - dramatically reduces bcrypt verifications
    // The (key_prefix = $1 OR key_prefix IS NULL) handles legacy keys without prefix
    // LIMIT 10 prevents excessive results even with prefix collisions
    let records: Vec<ApiKeyRecord> = sqlx::query_as(
        "SELECT id, organization_id, key_hash, queues, rate_limit, is_active, expires_at 
         FROM api_keys 
         WHERE is_active = TRUE 
           AND (key_prefix = $1 OR key_prefix IS NULL)
         LIMIT 10",
    )
    .bind(&key_prefix)
    .fetch_all(state.db.pool())
    .await
    .map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Database error: {}", e),
        )
    })?;

    // Find the matching key by verifying bcrypt hash
    // Now only verifying a small subset (typically 1-2 keys) instead of all keys
    for record in records {
        // Use bcrypt to verify this is the correct key
        if bcrypt::verify(token, &record.key_hash).unwrap_or(false) {
            return Ok(record);
        }
    }

    Err((StatusCode::UNAUTHORIZED, "Invalid API key".to_string()))
}

/// Fetch API key from database and cache it
async fn fetch_and_cache_api_key(
    state: &AppState,
    token: &str,
) -> Result<ApiKeyRecord, (StatusCode, String)> {
    let record = fetch_api_key(state, token).await?;

    // Cache for 1 hour
    if let Some(ref cache) = state.cache {
        let cache_key = format!("api_key:{}", hash_for_lookup(token));
        let _ = cache.set_json(&cache_key, &record, 3600).await;
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
        let hash1 = hash_for_lookup("sk_test_abc123");
        let hash2 = hash_for_lookup("sk_test_abc123");
        let hash3 = hash_for_lookup("sk_test_xyz789");

        assert_eq!(hash1, hash2);
        assert_ne!(hash1, hash3);
        assert_eq!(hash1.len(), 16);
    }
}
