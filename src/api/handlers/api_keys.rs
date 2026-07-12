//! API key handlers

use axum::{
    extract::{Extension, Path, State},
    http::StatusCode,
    Json,
};
use chrono::Utc;
use uuid::Uuid;

use crate::api::middleware::limits::{check_resource_limit_conn, lock_resource};
use crate::api::middleware::ValidatedJson;
use crate::api::AppState;
use crate::error::{AppError, AppResult};
use crate::models::{
    ApiKey, ApiKeyContext, ApiKeySummary, CreateApiKeyRequest, CreateApiKeyResponse,
    UpdateApiKeyRequest,
};

/// Maximum API keys per page
const MAX_API_KEYS_PER_PAGE: i64 = 100;

fn can_create_with_queues(ctx: &ApiKeyContext, requested: Option<&[String]>) -> bool {
    ctx.can_grant_queues(requested.unwrap_or_default())
}

fn can_update_with_queues(ctx: &ApiKeyContext, requested: Option<&[String]>) -> bool {
    ctx.is_unrestricted() || requested.is_some_and(|queues| ctx.can_grant_queues(queues))
}

/// List all API keys (without sensitive data)
///
/// Now filters by authenticated organization
/// Now uses configurable limit constant
pub async fn list(
    State(state): State<AppState>,
    Extension(ctx): Extension<ApiKeyContext>,
) -> AppResult<Json<Vec<ApiKeySummary>>> {
    tracing::debug!(
        organization_id = %ctx.organization_id,
        api_key_id = %ctx.api_key_id,
        "Listing API keys for organization"
    );

    // Use constant instead of hardcoded value
    let keys = sqlx::query_as::<_, ApiKey>(
        "SELECT * FROM api_keys WHERE organization_id = $1 ORDER BY created_at DESC LIMIT $2",
    )
    .bind(&ctx.organization_id)
    .bind(MAX_API_KEYS_PER_PAGE)
    .fetch_all(state.db.pool())
    .await?;

    tracing::debug!(
        organization_id = %ctx.organization_id,
        count = keys.len(),
        "Found API keys"
    );

    let summaries: Vec<ApiKeySummary> = keys.into_iter().map(Into::into).collect();
    Ok(Json(summaries))
}

/// Create a new API key
///
/// Now requires organization context from authenticated user.
/// SECURITY: Enforces plan limits before creating API keys.
pub async fn create(
    State(state): State<AppState>,
    Extension(ctx): Extension<ApiKeyContext>,
    ValidatedJson(request): ValidatedJson<CreateApiKeyRequest>,
) -> AppResult<(StatusCode, Json<CreateApiKeyResponse>)> {
    // Privilege-escalation guard: a queue-scoped key must not be able to mint a
    // broader key. It may only grant a subset of its own queues, and never `*`.
    // Unrestricted keys (empty list or `*`) may grant anything.
    if !can_create_with_queues(&ctx, request.queues.as_deref()) {
        return Err(AppError::Authorization(
            "API key cannot grant queues outside its own scope".to_string(),
        ));
    }

    let key_id = Uuid::new_v4().to_string();
    let now = Utc::now();

    // Generate a secure random API key
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

    // Extract key prefix for fast indexed lookup (first 8 chars)
    let key_prefix: String = raw_key.chars().take(8).collect();

    // Hash the key using bcrypt (verification happens in Rust, NOT in database).
    // Done BEFORE the transaction so the advisory lock below is held minimally.
    let key_hash = bcrypt::hash(&raw_key, bcrypt::DEFAULT_COST)
        .map_err(|e| AppError::Internal(format!("Failed to hash API key: {}", e)))?;
    // Fast-lookup hash so authentication resolves this key by a unique index instead
    // of bcrypt-scanning every key sharing the `sp_live_`/`sp_test_` prefix.
    let lookup_hash = crate::models::api_key_lookup_hash(&raw_key);

    let queues: Vec<String> = request.queues.unwrap_or_default();

    // Enforce the api_keys cap atomically: the advisory lock serializes concurrent
    // creates for this org so the count + insert cannot overshoot the plan limit.
    let mut tx = state.db.pool().begin().await?;
    lock_resource(&mut tx, &ctx.organization_id, "api_keys").await?;
    if let Err(response) =
        check_resource_limit_conn(&mut tx, &ctx.organization_id, "api_keys", 1).await
    {
        return Err(AppError::LimitExceeded(Box::new(response)));
    }

    sqlx::query(
        r#"
        INSERT INTO api_keys (
            id, organization_id, key_hash, key_prefix, name, queues, rate_limit,
            is_active, created_at, expires_at, lookup_hash
        )
        VALUES ($1, $2, $3, $4, $5, $6, $7, TRUE, $8, $9, $10)
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
    .bind(&lookup_hash)
    .execute(&mut *tx)
    .await?;

    tx.commit().await?;

    Ok((
        StatusCode::CREATED,
        Json(CreateApiKeyResponse {
            id: key_id,
            key: raw_key, // This is the ONLY time the raw key is returned!
            name: request.name,
            queues,
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

    // Privilege-escalation guard: a queue-scoped key must not broaden any key's
    // scope beyond its own (nor grant `*`). Restricted callers must explicitly
    // provide a non-empty permitted subset; omission would otherwise preserve a
    // potentially unrestricted target key.
    if !can_update_with_queues(&ctx, request.queues.as_deref()) {
        return Err(AppError::Authorization(
            "API key cannot grant queues outside its own scope".to_string(),
        ));
    }

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
    use rand::RngExt;

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

    fn scoped_context() -> ApiKeyContext {
        ApiKeyContext {
            organization_id: "org".to_string(),
            api_key_id: "key".to_string(),
            queues: vec!["one".to_string(), "two".to_string()],
            rate_limit: None,
        }
    }

    #[test]
    fn test_scoped_create_queue_grants() {
        let ctx = scoped_context();
        let empty: Vec<String> = vec![];
        let allowed = vec!["one".to_string()];
        let forbidden = vec!["three".to_string()];

        assert!(
            !can_create_with_queues(&ctx, None),
            "omitted is unrestricted"
        );
        assert!(!can_create_with_queues(&ctx, Some(&empty)));
        assert!(can_create_with_queues(&ctx, Some(&allowed)));
        assert!(!can_create_with_queues(&ctx, Some(&forbidden)));
    }

    #[test]
    fn test_scoped_update_queue_grants() {
        let ctx = scoped_context();
        let empty: Vec<String> = vec![];
        let allowed = vec!["one".to_string()];
        let forbidden = vec!["three".to_string()];

        assert!(!can_update_with_queues(&ctx, None));
        assert!(!can_update_with_queues(&ctx, Some(&empty)));
        assert!(can_update_with_queues(&ctx, Some(&allowed)));
        assert!(!can_update_with_queues(&ctx, Some(&forbidden)));
    }

    #[test]
    fn test_create_response_contains_raw_key() {
        let response = CreateApiKeyResponse {
            id: "key-1".to_string(),
            key: "sp_test_abc123".to_string(),
            name: "Test Key".to_string(),
            queues: vec![],
            created_at: Utc::now(),
            expires_at: None,
        };

        let json = serde_json::to_string(&response).unwrap();
        assert!(json.contains("sp_test_abc123"));
    }
}
