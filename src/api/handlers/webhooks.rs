//! Webhook handlers

use axum::{
    body::Bytes,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    Json,
};
use chrono::Utc;
use hmac::{Hmac, Mac};
use sha2::Sha256;
use uuid::Uuid;

use crate::api::AppState;
use crate::error::{AppError, AppResult};
use crate::models::{CustomWebhookRequest, GitHubWebhookEvent, StripeWebhookEvent};

use crate::config::Environment;

type HmacSha256 = Hmac<Sha256>;

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

/// Handle GitHub webhook
///
/// Now requires organization_id in path instead of hardcoded "default-org"
/// Webhook URLs should be in the format: /api/v1/webhooks/{org_id}/github
///
/// Now validates webhook secret token from X-Webhook-Token header.
/// Organizations must configure a webhook secret and include it in requests.
///
/// In production mode, webhooks are rejected if not received over HTTPS
/// (detected via X-Forwarded-Proto header from reverse proxy).
///
pub async fn github(
    State(state): State<AppState>,
    Path(org_id): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> AppResult<StatusCode> {
    // In production, require HTTPS for webhooks to prevent man-in-the-middle attacks
    validate_https_in_production(&state, &headers)?;
    // Validate payload size
    if body.len() > MAX_WEBHOOK_PAYLOAD_SIZE {
        return Err(AppError::BadRequest(format!(
            "Webhook payload too large: {} bytes (max: {} bytes)",
            body.len(),
            MAX_WEBHOOK_PAYLOAD_SIZE
        )));
    }
    // Validate webhook authorization token
    // Organizations can configure a webhook token that must be provided
    let webhook_token = headers.get("X-Webhook-Token").and_then(|v| v.to_str().ok());

    // Check if org exists and get its webhook token
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

    // If org has configured a webhook token, validate it
    if let Some(ref expected_token) = configured_token {
        match webhook_token {
            Some(provided) if constant_time_compare(provided, expected_token) => {
                // Token matches - continue
            }
            _ => {
                return Err(AppError::Authentication(
                    "Invalid or missing X-Webhook-Token".to_string(),
                ));
            }
        }
    }

    // Get the signature from headers
    let signature = headers
        .get("X-Hub-Signature-256")
        .and_then(|v| v.to_str().ok())
        .ok_or_else(|| {
            AppError::Authentication("Missing X-Hub-Signature-256 header".to_string())
        })?;

    // Get the event type
    let event_type = headers
        .get("X-GitHub-Event")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("unknown")
        .to_string();

    // Verify signature - REQUIRED, cannot be bypassed
    // If github_secret is not configured, reject the webhook to prevent forgery
    match &state.settings.webhooks.github_secret {
        Some(secret) => {
            verify_github_signature(secret, &body, signature)?;
        }
        None => {
            tracing::error!("GitHub webhook received but GITHUB_WEBHOOK_SECRET is not configured");
            return Err(AppError::Internal(
                "Webhook signature verification not configured. Set GITHUB_WEBHOOK_SECRET environment variable.".to_string()
            ));
        }
    }

    // Parse the payload
    let payload: GitHubWebhookEvent = serde_json::from_slice(&body)
        .map_err(|e| AppError::BadRequest(format!("Invalid JSON: {}", e)))?;

    // Sanitize queue name derived from event type
    let sanitized_event = sanitize_queue_name(&event_type);
    let queue_name = format!("github-{}", sanitized_event);

    // Validate the final queue name
    if !validate_queue_name(&queue_name) {
        return Err(AppError::BadRequest(
            "Invalid event type for queue name".to_string(),
        ));
    }

    // Create job from webhook
    let job_id = Uuid::new_v4().to_string();
    let now = Utc::now();

    sqlx::query(
        r#"
        INSERT INTO jobs (
            id, organization_id, queue_name, status, payload, priority,
            max_retries, timeout_seconds, created_at, updated_at
        )
        VALUES ($1, $2, $3, 'pending', $4::JSONB, 0, 3, 300, $5, $5)
        "#,
    )
    .bind(&job_id)
    .bind(&org_id) // Use org_id from path
    .bind(&queue_name)
    .bind(serde_json::to_string(&payload.extra).unwrap_or_default())
    .bind(now)
    .execute(state.db.pool())
    .await?;

    // Record webhook delivery
    sqlx::query(
        r#"
        INSERT INTO webhook_deliveries (id, organization_id, provider, event_type, payload, signature, matched_queue, created_at)
        VALUES ($1, $2, 'github', $3, $4::JSONB, $5, $6, $7)
        "#,
    )
    .bind(Uuid::new_v4().to_string())
    .bind(&org_id) // Use org_id from path
    .bind(&event_type)
    .bind(serde_json::to_string(&payload.extra).unwrap_or_default())
    .bind(signature)
    .bind(&queue_name)
    .bind(now)
    .execute(state.db.pool())
    .await?;

    state.metrics.jobs_enqueued.inc();

    // Publish to Redis with org context to prevent cross-tenant leakage
    if let Some(ref cache) = state.cache {
        let _ = cache
            .publish(&format!("org:{}:queue:{}", org_id, queue_name), &job_id)
            .await;
    }

    Ok(StatusCode::OK)
}

/// Handle Stripe webhook
///
/// Now requires organization_id in path
/// Now validates webhook authorization token
/// In production mode, requires HTTPS.
pub async fn stripe(
    State(state): State<AppState>,
    Path(org_id): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> AppResult<StatusCode> {
    // In production, require HTTPS for webhooks
    validate_https_in_production(&state, &headers)?;
    // Validate payload size
    if body.len() > MAX_WEBHOOK_PAYLOAD_SIZE {
        return Err(AppError::BadRequest(format!(
            "Webhook payload too large: {} bytes (max: {} bytes)",
            body.len(),
            MAX_WEBHOOK_PAYLOAD_SIZE
        )));
    }
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

    // If org has configured a webhook token, validate it
    if let Some(ref expected_token) = configured_token {
        match webhook_token {
            Some(provided) if constant_time_compare(provided, expected_token) => {}
            _ => {
                return Err(AppError::Authentication(
                    "Invalid or missing X-Webhook-Token".to_string(),
                ));
            }
        }
    }

    // Get the signature from headers
    let signature = headers
        .get("Stripe-Signature")
        .and_then(|v| v.to_str().ok())
        .ok_or_else(|| AppError::Authentication("Missing Stripe-Signature header".to_string()))?;

    // Verify signature - REQUIRED, cannot be bypassed
    // If stripe_secret is not configured, reject the webhook to prevent forgery
    match &state.settings.webhooks.stripe_secret {
        Some(secret) => {
            verify_stripe_signature(secret, &body, signature)?;
        }
        None => {
            tracing::error!("Stripe webhook received but STRIPE_WEBHOOK_SECRET is not configured");
            return Err(AppError::Internal(
                "Webhook signature verification not configured. Set STRIPE_WEBHOOK_SECRET environment variable.".to_string()
            ));
        }
    }

    // Parse the payload
    let event: StripeWebhookEvent = serde_json::from_slice(&body)
        .map_err(|e| AppError::BadRequest(format!("Invalid JSON: {}", e)))?;

    // Sanitize queue name - event_type comes from external input
    let sanitized_type = sanitize_queue_name(&event.event_type.replace('.', "-"));
    let queue_name = format!("stripe-{}", sanitized_type);

    // Validate the final queue name
    if !validate_queue_name(&queue_name) {
        return Err(AppError::BadRequest(
            "Invalid event type for queue name".to_string(),
        ));
    }

    // Create job from webhook
    let job_id = Uuid::new_v4().to_string();
    let now = Utc::now();

    sqlx::query(
        r#"
        INSERT INTO jobs (
            id, organization_id, queue_name, status, payload, priority,
            max_retries, timeout_seconds, created_at, updated_at, idempotency_key
        )
        VALUES ($1, $2, $3, 'pending', $4::JSONB, 0, 3, 300, $5, $5, $6)
        ON CONFLICT (organization_id, idempotency_key) WHERE idempotency_key IS NOT NULL
        DO UPDATE SET updated_at = NOW()
        "#,
    )
    .bind(&job_id)
    .bind(&org_id) // Use org_id from path
    .bind(&queue_name)
    .bind(serde_json::to_string(&event.data.object).unwrap_or_default())
    .bind(now)
    .bind(&event.id) // Use Stripe event ID as idempotency key
    .execute(state.db.pool())
    .await?;

    // Record webhook delivery
    sqlx::query(
        r#"
        INSERT INTO webhook_deliveries (id, organization_id, provider, event_type, payload, signature, matched_queue, created_at)
        VALUES ($1, $2, 'stripe', $3, $4::JSONB, $5, $6, $7)
        "#,
    )
    .bind(Uuid::new_v4().to_string())
    .bind(&org_id) // Use org_id from path
    .bind(&event.event_type)
    .bind(serde_json::to_string(&event.data.object).unwrap_or_default())
    .bind(signature)
    .bind(&queue_name)
    .bind(now)
    .execute(state.db.pool())
    .await?;

    state.metrics.jobs_enqueued.inc();

    // Publish to Redis with org context to prevent cross-tenant leakage
    if let Some(ref cache) = state.cache {
        let _ = cache
            .publish(&format!("org:{}:queue:{}", org_id, queue_name), &job_id)
            .await;
    }

    Ok(StatusCode::OK)
}

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

    // If org has configured a webhook token, validate it
    if let Some(ref expected_token) = configured_token {
        match webhook_token {
            Some(provided) if constant_time_compare(provided, expected_token) => {}
            _ => {
                return Err(AppError::Authentication(
                    "Invalid or missing X-Webhook-Token".to_string(),
                ));
            }
        }
    }

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

/// Verify GitHub webhook signature using HMAC-SHA256
fn verify_github_signature(secret: &str, body: &[u8], signature: &str) -> AppResult<()> {
    let expected = signature
        .strip_prefix("sha256=")
        .ok_or_else(|| AppError::Authentication("Invalid signature format".to_string()))?;

    let mut mac = HmacSha256::new_from_slice(secret.as_bytes())
        .map_err(|_| AppError::Internal("Invalid HMAC key".to_string()))?;
    mac.update(body);

    let computed = hex::encode(mac.finalize().into_bytes());

    if !constant_time_compare(&computed, expected) {
        return Err(AppError::Authentication(
            "Invalid webhook signature".to_string(),
        ));
    }

    Ok(())
}

/// Maximum age for Stripe webhook timestamps (5 minutes)
const STRIPE_TIMESTAMP_TOLERANCE_SECS: i64 = 300;

/// Verify Stripe webhook signature
///
/// Now validates timestamp to prevent replay attacks.
/// Rejects webhooks with timestamps older than 5 minutes.
fn verify_stripe_signature(secret: &str, body: &[u8], signature: &str) -> AppResult<()> {
    // Parse Stripe signature format: t=timestamp,v1=signature
    let mut timestamp = None;
    let mut v1_signature = None;

    for part in signature.split(',') {
        if let Some((key, value)) = part.split_once('=') {
            match key {
                "t" => timestamp = Some(value),
                "v1" => v1_signature = Some(value),
                _ => {}
            }
        }
    }

    let timestamp_str = timestamp
        .ok_or_else(|| AppError::Authentication("Missing timestamp in signature".to_string()))?;
    let expected =
        v1_signature.ok_or_else(|| AppError::Authentication("Missing v1 signature".to_string()))?;

    // Validate timestamp to prevent replay attacks
    let timestamp_secs: i64 = timestamp_str
        .parse()
        .map_err(|_| AppError::Authentication("Invalid timestamp format".to_string()))?;

    let now = Utc::now().timestamp();
    let age = now - timestamp_secs;

    // Reject if timestamp is too old (replay attack) or too far in the future (clock skew attack)
    if age > STRIPE_TIMESTAMP_TOLERANCE_SECS {
        return Err(AppError::Authentication(format!(
            "Webhook timestamp too old: {} seconds ago (max: {} seconds)",
            age, STRIPE_TIMESTAMP_TOLERANCE_SECS
        )));
    }
    if age < -60 {
        // Allow 1 minute of clock skew for future timestamps
        return Err(AppError::Authentication(
            "Webhook timestamp is in the future".to_string(),
        ));
    }

    // Create signed payload
    let signed_payload = format!("{}.{}", timestamp_str, String::from_utf8_lossy(body));

    let mut mac = HmacSha256::new_from_slice(secret.as_bytes())
        .map_err(|_| AppError::Internal("Invalid HMAC key".to_string()))?;
    mac.update(signed_payload.as_bytes());

    let computed = hex::encode(mac.finalize().into_bytes());

    if !constant_time_compare(&computed, expected) {
        return Err(AppError::Authentication(
            "Invalid webhook signature".to_string(),
        ));
    }

    Ok(())
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
    fn test_github_signature_format() {
        // Ensure we handle the sha256= prefix correctly
        let signature = "sha256=abc123";
        let stripped = signature.strip_prefix("sha256=");
        assert_eq!(stripped, Some("abc123"));
    }
}
