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

/// Validate API key using bcrypt verification
async fn validate_api_key(pool: &PgPool, token: &str) -> Result<GrpcAuthContext, Status> {
    // Extract key prefix for efficient indexed lookup
    let key_prefix: String = token.chars().take(8).collect();

    // Fetch candidate keys with matching prefix.
    // Use a higher LIMIT to handle many keys sharing the same prefix (e.g. sk_test_).
    let records: Vec<ApiKeyRecord> = sqlx::query_as(
        r#"
        SELECT id, organization_id, key_hash, queues, rate_limit
        FROM api_keys 
        WHERE is_active = TRUE 
          AND (key_prefix = $1 OR key_prefix IS NULL)
          AND (expires_at IS NULL OR expires_at > NOW())
        LIMIT 100
        "#,
    )
    .bind(&key_prefix)
    .fetch_all(pool)
    .await
    .map_err(|e| {
        tracing::error!(error = %e, "Database error during gRPC authentication");
        Status::internal("Authentication failed")
    })?;

    // Find matching key using bcrypt verification
    for record in records {
        if bcrypt::verify(token, &record.key_hash).unwrap_or(false) {
            info!(
                org_id = %record.organization_id,
                api_key_id = %record.id,
                "gRPC request authenticated"
            );

            // Update last_used asynchronously (fire-and-forget)
            let pool_clone = pool.clone();
            let key_id = record.id.clone();
            tokio::spawn(async move {
                let _ = sqlx::query("UPDATE api_keys SET last_used = NOW() WHERE id = $1")
                    .bind(&key_id)
                    .execute(&pool_clone)
                    .await;
            });

            return Ok(GrpcAuthContext {
                organization_id: record.organization_id,
                api_key_id: record.id,
                queues: record.queues,
                rate_limit: record.rate_limit,
            });
        }
    }

    warn!("Invalid API key provided for gRPC request");
    Err(Status::unauthenticated("Invalid API key"))
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
        let token = "sk_test_abc123";
        let prefix: String = token.chars().take(8).collect();
        assert_eq!(prefix, "sk_test_");
    }
}
