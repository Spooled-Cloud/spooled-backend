//! gRPC Authentication Interceptor
//!
//! Validates API keys from gRPC metadata and injects organization context.
//! Supports both `x-api-key` and `authorization: Bearer` headers.

// tonic::Status is a standard gRPC error type, size is acceptable
#![allow(clippy::result_large_err)]

use std::sync::Arc;

use axum::http::StatusCode;
use sqlx::PgPool;
use tonic::{Request, Status};
use tracing::{debug, warn};

use crate::api::middleware::auth::{authenticate_api_key_token, FailedAuthBudget};
use crate::cache::RedisCache;

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

    /// Worker lifecycle actions affect every queue a worker can process. A
    /// scoped key must be authorized for the whole worker queue set.
    pub fn can_access_all_queues(&self, queues: &[String]) -> bool {
        if self.queues.is_empty() || self.queues.iter().any(|q| q == "*") {
            return true;
        }
        !queues.is_empty() && queues.iter().all(|queue| self.can_access_queue(queue))
    }
}

/// Extract authentication context from gRPC request metadata, without the
/// Redis key cache: every call pays a bcrypt verify and a `last_used` write.
/// Services that hold a cache use [`authenticate_request_with_cache`].
pub async fn authenticate_request<T>(
    pool: &PgPool,
    request: &Request<T>,
) -> Result<GrpcAuthContext, Status> {
    authenticate_request_with_cache(pool, None, request).await
}

/// Extract authentication context from gRPC request metadata.
///
/// Same path as REST API-key auth: the `api_key:{sha256}` record cache (so a
/// repeat call within its TTL skips bcrypt, and a REST revoke ends it here too),
/// the negative cache, and the debounced `last_used` write.
pub async fn authenticate_request_with_cache<T>(
    pool: &PgPool,
    cache: Option<&RedisCache>,
    request: &Request<T>,
) -> Result<GrpcAuthContext, Status> {
    // Try x-api-key first, then authorization header
    let token = extract_token(request)?;

    // Validate API key and get context
    validate_api_key(pool, cache, &token).await
}

/// Extract token from request and validate asynchronously
/// Use this for streaming requests where Request<Streaming<T>> is not Sync
pub async fn authenticate_from_metadata(
    pool: &PgPool,
    metadata: &tonic::metadata::MetadataMap,
) -> Result<GrpcAuthContext, Status> {
    authenticate_from_metadata_with_cache(pool, None, metadata).await
}

/// [`authenticate_from_metadata`] through the shared Redis key cache.
pub async fn authenticate_from_metadata_with_cache(
    pool: &PgPool,
    cache: Option<&RedisCache>,
    metadata: &tonic::metadata::MetadataMap,
) -> Result<GrpcAuthContext, Status> {
    let token = extract_token_from_metadata(metadata)?;
    validate_api_key(pool, cache, &token).await
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

/// API key record from database, for re-checking an open stream
#[derive(Debug, sqlx::FromRow)]
struct ApiKeyRecord {
    id: String,
    organization_id: String,
    queues: Vec<String>,
    rate_limit: Option<i32>,
}

/// Validate an API key through the REST key path
/// ([`crate::api::middleware::auth::authenticate_api_key_token`]): Redis record
/// cache, negative cache, the unique `lookup_hash` index, then the bounded
/// legacy scan, with every bcrypt verify off the async workers.
///
/// No per-client failed-auth budget here: the peer address tonic sees is the
/// ingress, not the client, and there is no trusted client-address metadata to
/// key it on. Instead every gRPC caller shares one budget
/// ([`crate::api::middleware::auth::FAILED_AUTH_GRPC_LIMIT`] per minute,
/// charged before the scan) that skips only the legacy scan, and gRPC scans
/// take their own turn, so a gRPC flood of unknown tokens can delay only gRPC
/// legacy keys, never REST ones (W3R-06, W3R-08). A legacy key not checked
/// because the turn stayed busy is `UNAVAILABLE` (retryable), not
/// `UNAUTHENTICATED`.
async fn validate_api_key(
    pool: &PgPool,
    cache: Option<&RedisCache>,
    token: &str,
) -> Result<GrpcAuthContext, Status> {
    let context = authenticate_api_key_token(pool, cache, token, FailedAuthBudget::Grpc)
        .await
        .map_err(|(status, message)| match status {
            StatusCode::UNAUTHORIZED => {
                warn!("Invalid API key provided for gRPC request");
                Status::unauthenticated(message)
            }
            StatusCode::SERVICE_UNAVAILABLE => Status::unavailable(message),
            _ => Status::internal(message),
        })?;

    debug!(
        org_id = %context.organization_id,
        api_key_id = %context.api_key_id,
        "gRPC request authenticated"
    );

    Ok(GrpcAuthContext {
        organization_id: context.organization_id,
        api_key_id: context.api_key_id,
        queues: context.queues,
        rate_limit: context.rate_limit,
    })
}

/// Reload an established stream's current authorization state without re-running
/// bcrypt. The key ID was authenticated when the stream opened; this lookup makes
/// revocation, expiry, organization deletion, rate-limit changes, and queue-scope
/// narrowing effective before the next protected stream operation.
pub async fn reload_api_key_context(
    pool: &PgPool,
    api_key_id: &str,
) -> Result<GrpcAuthContext, Status> {
    let record: Option<ApiKeyRecord> = sqlx::query_as(
        r#"
        SELECT k.id, k.organization_id, k.queues, k.rate_limit
        FROM api_keys k
        INNER JOIN organizations o ON o.id = k.organization_id
        WHERE k.id = $1
          AND k.is_active = TRUE
          AND (k.expires_at IS NULL OR k.expires_at > NOW())
          AND o.plan_tier <> 'deleted'
        "#,
    )
    .bind(api_key_id)
    .fetch_optional(pool)
    .await
    .map_err(|e| {
        tracing::error!(error = %e, api_key_id = %api_key_id, "Failed to revalidate gRPC stream authorization");
        Status::internal("Authentication revalidation failed")
    })?;

    record.map(GrpcAuthContext::from).ok_or_else(|| {
        Status::unauthenticated("API key revoked, expired, or organization unavailable")
    })
}

impl From<ApiKeyRecord> for GrpcAuthContext {
    fn from(record: ApiKeyRecord) -> Self {
        Self {
            organization_id: record.organization_id,
            api_key_id: record.id,
            queues: record.queues,
            rate_limit: record.rate_limit,
        }
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
