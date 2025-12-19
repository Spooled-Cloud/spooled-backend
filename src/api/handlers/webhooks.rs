//! Webhook handlers

use axum::{
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    Json,
};
use chrono::Utc;
use uuid::Uuid;

use crate::api::AppState;
use crate::config::Environment;
use crate::error::{AppError, AppResult};
use crate::models::CustomWebhookRequest;

/// Maximum webhook payload size (5MB)
const MAX_WEBHOOK_PAYLOAD_SIZE: usize = 5 * 1024 * 1024;

/// Validate that webhooks are received over HTTPS in production
///
/// Checks the X-Forwarded-Proto header (set by reverse proxies like nginx, ALB, etc.)
/// to ensure the original request was made over HTTPS.
fn validate_https_in_production(state: &AppState, headers: &HeaderMap) -> AppResult<()> {
    // Only enforce in production
    if state.settings.server.environment != Environment::Production {
        return Ok(());
    }

    // Check X-Forwarded-Proto header (set by reverse proxy)
    let proto = headers
        .get("X-Forwarded-Proto")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("http");

    if proto != "https" {
        tracing::warn!(
            proto = proto,
            "Webhook rejected: HTTPS required in production"
        );
        return Err(AppError::BadRequest(
            "HTTPS is required for webhooks in production".to_string(),
        ));
    }

    Ok(())
}

/// Validate queue name for safety
fn validate_queue_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 255
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// Sanitize a string for use in queue name
fn sanitize_queue_name(name: &str) -> String {
    name.chars()
        .take(64) // Limit length
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect()
}

// GitHub and Stripe webhook handlers removed as requested
// - GitHub: Not needed for this platform
// - Stripe (for job ingestion): Removed because it used a global secret which doesn't
//   work for multi-tenant. The /billing/webhook endpoint handles platform billing.
// See git history for implementation details

/// Handle custom webhook
///
/// Now requires organization_id in path
/// Now validates webhook authorization token
/// In production mode, requires HTTPS.
pub async fn custom(
    State(state): State<AppState>,
    Path(org_id): Path<String>,
    headers: HeaderMap,
    Json(request): Json<CustomWebhookRequest>,
) -> AppResult<StatusCode> {
    // In production, require HTTPS for webhooks
    validate_https_in_production(&state, &headers)?;
    // Validate webhook authorization token
    let webhook_token = headers.get("X-Webhook-Token").and_then(|v| v.to_str().ok());

    let org_data: Option<(String, Option<String>)> = sqlx::query_as(
        "SELECT id, settings->>'webhook_token' as webhook_token FROM organizations WHERE id = $1",
    )
    .bind(&org_id)
    .fetch_optional(state.db.pool())
    .await?;

    let Some((_org_id, configured_token)) = org_data else {
        return Err(AppError::NotFound(format!(
            "Organization {} not found",
            org_id
        )));
    };

    // SECURITY: Always require a token.
    // Historically, the token was optional which made the ingestion endpoint effectively public
    // for orgs without a token configured. That is unsafe for multi-tenant environments.
    require_webhook_token(configured_token.as_deref(), webhook_token)?;

    let job_id = Uuid::new_v4().to_string();
    let now = Utc::now();
    let priority = request.priority.unwrap_or(0);

    sqlx::query(
        r#"
        INSERT INTO jobs (
            id, organization_id, queue_name, status, payload, priority,
            max_retries, timeout_seconds, created_at, updated_at, idempotency_key
        )
        VALUES ($1, $2, $3, 'pending', $4::JSONB, $5, 3, 300, $6, $6, $7)
        ON CONFLICT (organization_id, idempotency_key) WHERE idempotency_key IS NOT NULL
        DO UPDATE SET updated_at = NOW()
        "#,
    )
    .bind(&job_id)
    .bind(&org_id) // Use org_id from path
    .bind(&request.queue_name)
    .bind(serde_json::to_string(&request.payload).unwrap_or_default())
    .bind(priority)
    .bind(now)
    .bind(&request.idempotency_key)
    .execute(state.db.pool())
    .await?;

    // Record webhook delivery
    let event_type = request.event_type.unwrap_or_else(|| "custom".to_string());
    sqlx::query(
        r#"
        INSERT INTO webhook_deliveries (id, organization_id, provider, event_type, payload, matched_queue, created_at)
        VALUES ($1, $2, 'custom', $3, $4::JSONB, $5, $6)
        "#,
    )
    .bind(Uuid::new_v4().to_string())
    .bind(&org_id) // Use org_id from path
    .bind(&event_type)
    .bind(serde_json::to_string(&request.payload).unwrap_or_default())
    .bind(&request.queue_name)
    .bind(now)
    .execute(state.db.pool())
    .await?;

    state.metrics.jobs_enqueued.inc();

    // Publish to Redis with org context to prevent cross-tenant leakage
    if let Some(ref cache) = state.cache {
        let _ = cache
            .publish(
                &format!("org:{}:queue:{}", org_id, request.queue_name),
                &job_id,
            )
            .await;
    }

    Ok(StatusCode::OK)
}

/// Constant-time string comparison to prevent timing attacks
///
/// The early return on length mismatch leaks timing information.
/// We now use a proper constant-time comparison that doesn't reveal length.
fn constant_time_compare(a: &str, b: &str) -> bool {
    use subtle::ConstantTimeEq;

    // If lengths differ, the signatures are definitely different
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

/// Require an incoming webhook token.
///
/// Returns Authentication error if:
/// - No token is configured for the org (misconfiguration)
/// - Token header is missing
/// - Token does not match
fn require_webhook_token(
    configured_token: Option<&str>,
    provided_token: Option<&str>,
) -> AppResult<()> {
    let Some(expected) = configured_token else {
        // Do not allow public ingestion; force token configuration via dashboard/API.
        return Err(AppError::Authentication(
            "Webhook token is not configured for this organization".to_string(),
        ));
    };

    match provided_token {
        Some(provided) if constant_time_compare(provided, expected) => Ok(()),
        _ => Err(AppError::Authentication(
            "Invalid or missing X-Webhook-Token".to_string(),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_constant_time_compare() {
        assert!(constant_time_compare("abc", "abc"));
        assert!(!constant_time_compare("abc", "abd"));
        assert!(!constant_time_compare("abc", "abcd"));
    }

    #[test]
    fn test_require_webhook_token_requires_configured_token() {
        let err = require_webhook_token(None, Some("whk_x")).unwrap_err();
        let msg = err.to_string();
        assert!(msg.to_lowercase().contains("not configured"));
    }

    #[test]
    fn test_require_webhook_token_requires_header() {
        let err = require_webhook_token(Some("whk_expected"), None).unwrap_err();
        let msg = err.to_string();
        assert!(msg.to_lowercase().contains("missing"));
    }

    #[test]
    fn test_require_webhook_token_rejects_wrong_token() {
        let err = require_webhook_token(Some("whk_expected"), Some("whk_wrong")).unwrap_err();
        let msg = err.to_string();
        assert!(msg.to_lowercase().contains("invalid"));
    }

    #[test]
    fn test_require_webhook_token_accepts_correct_token() {
        require_webhook_token(Some("whk_expected"), Some("whk_expected")).unwrap();
    }
}
