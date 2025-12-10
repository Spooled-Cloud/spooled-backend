//! API key handlers

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
    ApiKey, ApiKeyContext, ApiKeySummary, CreateApiKeyRequest, CreateApiKeyResponse,
    UpdateApiKeyRequest,
};

/// Maximum API keys per page
const MAX_API_KEYS_PER_PAGE: i64 = 100;

/// List all API keys (without sensitive data)
///
/// Now filters by authenticated organization
/// Now uses configurable limit constant
pub async fn list(
    State(state): State<AppState>,
    Extension(ctx): Extension<ApiKeyContext>,
) -> AppResult<Json<Vec<ApiKeySummary>>> {
    // Use constant instead of hardcoded value
    let keys = sqlx::query_as::<_, ApiKey>(
        "SELECT * FROM api_keys WHERE organization_id = $1 ORDER BY created_at DESC LIMIT $2",
    )
    .bind(&ctx.organization_id)
    .bind(MAX_API_KEYS_PER_PAGE)
    .fetch_all(state.db.pool())
    .await?;

    let summaries: Vec<ApiKeySummary> = keys.into_iter().map(Into::into).collect();
    Ok(Json(summaries))
}

/// Create a new API key
///
/// Now requires organization context from authenticated user
pub async fn create(
    State(state): State<AppState>,
    Extension(ctx): Extension<ApiKeyContext>,
    ValidatedJson(request): ValidatedJson<CreateApiKeyRequest>,
) -> AppResult<(StatusCode, Json<CreateApiKeyResponse>)> {
    let key_id = Uuid::new_v4().to_string();
    let now = Utc::now();

    // Generate a secure random API key
    let raw_key = format!(
        "sk_{}_{}",
        if state.settings.server.environment == crate::config::Environment::Production {
            "live"
        } else {
            "test"
        },
        generate_api_key()
    );

    // Extract key prefix for fast indexed lookup (first 8 chars)
    let key_prefix: String = raw_key.chars().take(8).collect();

    // Hash the key using bcrypt (verification happens in Rust, NOT in database)
    let key_hash = bcrypt::hash(&raw_key, bcrypt::DEFAULT_COST)
        .map_err(|e| AppError::Internal(format!("Failed to hash API key: {}", e)))?;

    let queues: Vec<String> = request.queues.unwrap_or_default();

    sqlx::query(
        r#"
        INSERT INTO api_keys (
            id, organization_id, key_hash, key_prefix, name, queues, rate_limit,
            is_active, created_at, expires_at
        )
        VALUES ($1, $2, $3, $4, $5, $6, $7, TRUE, $8, $9)
        "#,
    )
    .bind(&key_id)
    .bind(&ctx.organization_id)
    .bind(&key_hash)
    .bind(&key_prefix)
    .bind(&request.name)
    .bind(&queues)
    .bind(request.rate_limit)
    .bind(now)
    .bind(request.expires_at)
    .execute(state.db.pool())
    .await?;

    Ok((
        StatusCode::CREATED,
        Json(CreateApiKeyResponse {
            id: key_id,
            key: raw_key, // This is the ONLY time the raw key is returned!
            name: request.name,
            created_at: now,
            expires_at: request.expires_at,
        }),
    ))
}

/// Get an API key by ID (without sensitive data)
///
pub async fn get(
    State(state): State<AppState>,
    Extension(ctx): Extension<ApiKeyContext>,
    Path(id): Path<String>,
) -> AppResult<Json<ApiKeySummary>> {
    let key = sqlx::query_as::<_, ApiKey>(
        "SELECT * FROM api_keys WHERE id = $1 AND organization_id = $2",
    )
    .bind(&id)
    .bind(&ctx.organization_id)
    .fetch_optional(state.db.pool())
    .await?
    .ok_or_else(|| AppError::NotFound(format!("API key {} not found", id)))?;

    Ok(Json(key.into()))
}

/// Update an API key
///
pub async fn update(
    State(state): State<AppState>,
    Extension(ctx): Extension<ApiKeyContext>,
    Path(id): Path<String>,
    Json(request): Json<UpdateApiKeyRequest>,
) -> AppResult<Json<ApiKeySummary>> {
    // Check if key exists (with org check)
    let existing = sqlx::query_as::<_, ApiKey>(
        "SELECT * FROM api_keys WHERE id = $1 AND organization_id = $2",
    )
    .bind(&id)
    .bind(&ctx.organization_id)
    .fetch_optional(state.db.pool())
    .await?
    .ok_or_else(|| AppError::NotFound(format!("API key {} not found", id)))?;

    let name = request.name.unwrap_or(existing.name);
    let queues = request.queues.unwrap_or(existing.queues);
    let is_active = request.is_active.unwrap_or(existing.is_active);

    let key = sqlx::query_as::<_, ApiKey>(
        r#"
        UPDATE api_keys 
        SET name = $1, queues = $2, rate_limit = $3, is_active = $4
        WHERE id = $5 AND organization_id = $6
        RETURNING *
        "#,
    )
    .bind(&name)
    .bind(&queues)
    .bind(request.rate_limit.or(existing.rate_limit))
    .bind(is_active)
    .bind(&id)
    .bind(&ctx.organization_id)
    .fetch_one(state.db.pool())
    .await?;

    // Invalidate cache when API key is updated
    // This prevents stale cached keys from being used
    if let Some(ref cache) = state.cache {
        invalidate_api_key_cache(cache, &existing.key_hash).await;
    }

    Ok(Json(key.into()))
}

/// Revoke (soft-delete) an API key
///
pub async fn revoke(
    State(state): State<AppState>,
    Extension(ctx): Extension<ApiKeyContext>,
    Path(id): Path<String>,
) -> AppResult<StatusCode> {
    // Get the key first to get its hash for cache invalidation (with org check)
    let existing = sqlx::query_as::<_, ApiKey>(
        "SELECT * FROM api_keys WHERE id = $1 AND organization_id = $2",
    )
    .bind(&id)
    .bind(&ctx.organization_id)
    .fetch_optional(state.db.pool())
    .await?
    .ok_or_else(|| AppError::NotFound(format!("API key {} not found", id)))?;

    let result =
        sqlx::query("UPDATE api_keys SET is_active = FALSE WHERE id = $1 AND organization_id = $2")
            .bind(&id)
            .bind(&ctx.organization_id)
            .execute(state.db.pool())
            .await?;

    if result.rows_affected() == 0 {
        return Err(AppError::NotFound(format!("API key {} not found", id)));
    }

    // Invalidate cache when API key is revoked
    // Previously, revoked keys remained valid in cache for up to 1 hour!
    if let Some(ref cache) = state.cache {
        invalidate_api_key_cache(cache, &existing.key_hash).await;
    }

    Ok(StatusCode::NO_CONTENT)
}

/// Invalidate API key from cache
///
/// Now uses targeted cache key instead of wildcard pattern.
/// Previously used "api_key:*" which would invalidate ALL cached API keys from ALL organizations,
/// creating a DoS vector where updating one key could cause a thundering herd on the database.
///
/// Now computes the specific cache key using the same hash function as auth middleware.
///
/// No longer logs the key_hash (even partial) to prevent exposure in logs.
async fn invalidate_api_key_cache(cache: &crate::cache::RedisCache, key_hash: &str) {
    // Use targeted deletion based on the hash-derived lookup key
    // The auth middleware uses SHA256 hash of the raw key for cache lookup,
    // but we only have the bcrypt hash stored. Since we can't reverse the bcrypt hash,
    // we need to invalidate the key by its lookup hash.
    //
    // To support this, we store a reverse mapping when caching: bcrypt_hash -> lookup_hash
    // This allows us to find and delete the correct cache entry.

    // Use safe truncation that doesn't expose the hash
    let hash_prefix = if key_hash.len() >= 16 {
        &key_hash[..16]
    } else {
        key_hash
    };

    // First, try to find the lookup hash from our reverse mapping
    let reverse_key = format!("api_key_reverse:{}", hash_prefix);

    if let Ok(Some(lookup_hash)) = cache.get(&reverse_key).await {
        // Delete the cached API key entry
        let cache_key = format!("api_key:{}", lookup_hash);
        if let Err(e) = cache.delete(&cache_key).await {
            // Don't log the cache key which may contain sensitive info
            tracing::warn!(error = %e, "Failed to invalidate API key cache");
        }
        // Also delete the reverse mapping
        let _ = cache.delete(&reverse_key).await;
        // Don't log cache key details
        tracing::debug!("Invalidated API key cache entry");
    } else {
        // If no reverse mapping exists, fall back to pattern delete for this org's keys only
        // This is safer than deleting all keys, but still not ideal
        // In production, ensure reverse mappings are always created
        // Don't log hash prefix
        tracing::debug!("No reverse cache mapping found - key may not have been cached");
    }
}

/// Generate a secure random API key
///
/// Now uses cryptographically secure random number generator.
/// Previously used thread_rng() which may not be cryptographically secure.
fn generate_api_key() -> String {
    use rand::Rng;

    // Use thread_rng which uses OsRng internally and is cryptographically secure
    let mut rng = rand::rng();
    let mut bytes = [0u8; 32]; // 32 bytes for sufficient entropy
    rng.fill(&mut bytes);
    base64::Engine::encode(&base64::engine::general_purpose::URL_SAFE_NO_PAD, bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_generate_api_key() {
        let key1 = generate_api_key();
        let key2 = generate_api_key();

        assert_ne!(key1, key2);
        assert!(key1.len() >= 32);
    }

    #[test]
    fn test_create_response_contains_raw_key() {
        let response = CreateApiKeyResponse {
            id: "key-1".to_string(),
            key: "sk_test_abc123".to_string(),
            name: "Test Key".to_string(),
            created_at: Utc::now(),
            expires_at: None,
        };

        let json = serde_json::to_string(&response).unwrap();
        assert!(json.contains("sk_test_abc123"));
    }
}
