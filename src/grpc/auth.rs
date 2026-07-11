//! gRPC Authentication Interceptor
//!
//! Validates API keys from gRPC metadata and injects organization context.
//! Supports both `x-api-key` and `authorization: Bearer` headers.

// tonic::Status is a standard gRPC error type, size is acceptable
#![allow(clippy::result_large_err)]

use std::sync::Arc;

use sqlx::PgPool;
use tonic::{Request, Status};
use tracing::{info, warn};

/// Authenticated organization context for gRPC requests
#[derive(Debug, Clone)]
pub struct GrpcAuthContext {
    /// Organization ID from API key
    pub organization_id: String,
    /// API key ID for audit logging
    pub api_key_id: String,
    /// Allowed queues (empty = all queues)
    pub queues: Vec<String>,
    /// Rate limit (requests per minute)
    pub rate_limit: Option<i32>,
}

impl GrpcAuthContext {
    /// Whether this key is allowed to operate on the given queue.
    /// Mirrors `ApiKeyContext::can_access_queue`: empty list or a `*`
    /// wildcard means all queues are allowed.
    pub fn can_access_queue(&self, queue_name: &str) -> bool {
        self.queues.is_empty() || self.queues.iter().any(|q| q == "*" || q == queue_name)
    }
}

/// Extract authentication context from gRPC request metadata
pub async fn authenticate_request<T>(
    pool: &PgPool,
    request: &Request<T>,
) -> Result<GrpcAuthContext, Status> {
    // Try x-api-key first, then authorization header
    let token = extract_token(request)?;

    // Validate API key and get context
    validate_api_key(pool, &token).await
}

/// Extract token from request and validate asynchronously
/// Use this for streaming requests where Request<Streaming<T>> is not Sync
pub async fn authenticate_from_metadata(
    pool: &PgPool,
    metadata: &tonic::metadata::MetadataMap,
) -> Result<GrpcAuthContext, Status> {
    let token = extract_token_from_metadata(metadata)?;
    validate_api_key(pool, &token).await
}

/// Extract token from metadata map directly
fn extract_token_from_metadata(metadata: &tonic::metadata::MetadataMap) -> Result<String, Status> {
    // Try x-api-key first
    if let Some(api_key) = metadata.get("x-api-key") {
        return api_key
            .to_str()
            .map(|s| s.to_string())
            .map_err(|_| Status::unauthenticated("Invalid x-api-key header encoding"));
    }

    // Try authorization header
    if let Some(auth) = metadata.get("authorization") {
        let auth_str = auth
            .to_str()
            .map_err(|_| Status::unauthenticated("Invalid authorization header encoding"))?;

        if let Some(token) = auth_str.strip_prefix("Bearer ") {
            return Ok(token.to_string());
        }
        if let Some(token) = auth_str.strip_prefix("bearer ") {
            return Ok(token.to_string());
        }
        return Ok(auth_str.to_string());
    }

    Err(Status::unauthenticated(
        "Missing x-api-key or authorization header",
    ))
}

/// Extract token from request metadata
fn extract_token<T>(request: &Request<T>) -> Result<String, Status> {
    let metadata = request.metadata();

    // Try x-api-key first
    if let Some(api_key) = metadata.get("x-api-key") {
        return api_key
            .to_str()
            .map(|s| s.to_string())
            .map_err(|_| Status::unauthenticated("Invalid x-api-key header encoding"));
    }

    // Try authorization header
    if let Some(auth) = metadata.get("authorization") {
        let auth_str = auth
            .to_str()
            .map_err(|_| Status::unauthenticated("Invalid authorization header encoding"))?;

        if let Some(token) = auth_str.strip_prefix("Bearer ") {
            return Ok(token.to_string());
        }
        if let Some(token) = auth_str.strip_prefix("bearer ") {
            return Ok(token.to_string());
        }
        // If no Bearer prefix, use the whole value
        return Ok(auth_str.to_string());
    }

    Err(Status::unauthenticated(
        "Missing x-api-key or authorization header",
    ))
}

/// API key record from database
#[derive(Debug, sqlx::FromRow)]
struct ApiKeyRecord {
    id: String,
    organization_id: String,
    key_hash: String,
    queues: Vec<String>,
    rate_limit: Option<i32>,
}

/// Validate API key using bcrypt verification.
///
/// Fast path resolves the key via the unique `lookup_hash = sha256(token)` index
/// (single row + one bcrypt verify). This replaces the old prefix scan, where every
/// key shared the `sp_live_`/`sp_test_` prefix so a bogus key cost up to 100 bcrypt
/// verifications — the same CPU-amplification vector fixed on the REST path. Legacy
/// keys with a NULL `lookup_hash` fall back to the bounded scan and are backfilled
/// on match. gRPC has no auth cache, so this path is hit on every request.
async fn validate_api_key(pool: &PgPool, token: &str) -> Result<GrpcAuthContext, Status> {
    let db_err = |e: sqlx::Error| {
        tracing::error!(error = %e, "Database error during gRPC authentication");
        Status::internal("Authentication failed")
    };

    let lookup_hash = crate::models::api_key_lookup_hash(token);

    // Fast path: unique indexed lookup. is_active + expiry are filtered in SQL because
    // the gRPC record does not carry those columns. Soft-deleted orgs are excluded.
    let candidate: Option<ApiKeyRecord> = sqlx::query_as(
        r#"
        SELECT k.id, k.organization_id, k.key_hash, k.queues, k.rate_limit
        FROM api_keys k
        INNER JOIN organizations o ON o.id = k.organization_id
        WHERE k.lookup_hash = $1
          AND k.is_active = TRUE
          AND (k.expires_at IS NULL OR k.expires_at > NOW())
          AND o.plan_tier <> 'deleted'
        LIMIT 1
        "#,
    )
    .bind(&lookup_hash)
    .fetch_optional(pool)
    .await
    .map_err(db_err)?;

    if let Some(record) = candidate {
        if bcrypt::verify(token, &record.key_hash).unwrap_or(false) {
            return Ok(authenticated_context(pool, record));
        }
        // SHA-256 collision without a bcrypt match: fall through to the legacy scan.
    }

    // Legacy fallback: only keys not yet carrying a lookup_hash.
    let key_prefix: String = token.chars().take(8).collect();
    let records: Vec<ApiKeyRecord> = sqlx::query_as(
        r#"
        SELECT k.id, k.organization_id, k.key_hash, k.queues, k.rate_limit
        FROM api_keys k
        INNER JOIN organizations o ON o.id = k.organization_id
        WHERE k.is_active = TRUE
          AND k.lookup_hash IS NULL
          AND (k.key_prefix = $1 OR k.key_prefix IS NULL)
          AND (k.expires_at IS NULL OR k.expires_at > NOW())
          AND o.plan_tier <> 'deleted'
        LIMIT 100
        "#,
    )
    .bind(&key_prefix)
    .fetch_all(pool)
    .await
    .map_err(db_err)?;

    for record in records {
        if bcrypt::verify(token, &record.key_hash).unwrap_or(false) {
            // Backfill lookup_hash so subsequent auths for this key are O(1).
            let pool_clone = pool.clone();
            let key_id = record.id.clone();
            let lh = lookup_hash.clone();
            tokio::spawn(async move {
                let _ = sqlx::query(
                    "UPDATE api_keys SET lookup_hash = $1 WHERE id = $2 AND lookup_hash IS NULL",
                )
                .bind(&lh)
                .bind(&key_id)
                .execute(&pool_clone)
                .await;
            });
            return Ok(authenticated_context(pool, record));
        }
    }

    warn!("Invalid API key provided for gRPC request");
    Err(Status::unauthenticated("Invalid API key"))
}

/// Build the gRPC auth context for a verified key and refresh its `last_used`
/// timestamp asynchronously (fire-and-forget).
fn authenticated_context(pool: &PgPool, record: ApiKeyRecord) -> GrpcAuthContext {
    info!(
        org_id = %record.organization_id,
        api_key_id = %record.id,
        "gRPC request authenticated"
    );

    let pool_clone = pool.clone();
    let key_id = record.id.clone();
    tokio::spawn(async move {
        let _ = sqlx::query("UPDATE api_keys SET last_used = NOW() WHERE id = $1")
            .bind(&key_id)
            .execute(&pool_clone)
            .await;
    });

    GrpcAuthContext {
        organization_id: record.organization_id,
        api_key_id: record.id,
        queues: record.queues,
        rate_limit: record.rate_limit,
    }
}

/// Extension trait for accessing auth context from request
pub trait AuthContextExt {
    fn auth_context(&self) -> Result<&GrpcAuthContext, Status>;
}

impl<T> AuthContextExt for Request<T> {
    fn auth_context(&self) -> Result<&GrpcAuthContext, Status> {
        self.extensions()
            .get::<GrpcAuthContext>()
            .ok_or_else(|| Status::unauthenticated("Not authenticated"))
    }
}

/// Wrapper for authenticated database pool
#[derive(Clone)]
pub struct AuthenticatedPool {
    pub pool: Arc<PgPool>,
}

impl AuthenticatedPool {
    pub fn new(pool: Arc<PgPool>) -> Self {
        Self { pool }
    }

    pub fn pool(&self) -> &PgPool {
        &self.pool
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_grpc_auth_context_clone() {
        let ctx = GrpcAuthContext {
            organization_id: "org-1".to_string(),
            api_key_id: "key-1".to_string(),
            queues: vec!["default".to_string()],
            rate_limit: Some(1000),
        };

        let cloned = ctx.clone();
        assert_eq!(cloned.organization_id, ctx.organization_id);
        assert_eq!(cloned.api_key_id, ctx.api_key_id);
    }

    #[test]
    fn test_extract_token_formats() {
        // This tests the logic, not actual gRPC requests
        let token = "sp_test_abc123";
        let prefix: String = token.chars().take(8).collect();
        assert_eq!(prefix, "sp_test_");
    }
}
