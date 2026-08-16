//! Outgoing webhook handlers
//!
//! CRUD operations for managing outgoing webhook configurations.
//! These webhooks send notifications to external URLs when events occur.
//!
//! # Security
//! All webhook URLs are validated against SSRF attacks:
//! - Private IP ranges are blocked
//! - Internal hostnames are blocked  
//! - Cloud metadata endpoints are blocked
//! - HTTPS is required in production

use axum::{
    extract::{Path, State},
    http::StatusCode,
    Extension, Json,
};
use chrono::Utc;
use serde::Serialize;
use uuid::Uuid;
use validator::Validate;

use crate::api::handlers::require_unrestricted_key;
use crate::api::middleware::limits::{check_resource_limit_conn, lock_resource};
use crate::api::AppState;
use crate::error::{AppError, AppResult};
use crate::models::{
    ApiKeyContext, CreateOutgoingWebhookRequest, OutgoingWebhook, OutgoingWebhookDelivery,
    OutgoingWebhookSummary, TestWebhookResponse, UpdateOutgoingWebhookRequest,
    VALID_WEBHOOK_EVENTS,
};
use crate::security::{build_outbound_http_client, validate_webhook_url, UrlValidationOptions};

/// Validate that all event types are valid
fn validate_events(events: &[String]) -> AppResult<()> {
    for event in events {
        if !VALID_WEBHOOK_EVENTS.contains(&event.as_str()) {
            return Err(AppError::Validation(format!(
                "Invalid event type: '{}'. Valid types: {:?}",
                event, VALID_WEBHOOK_EVENTS
            )));
        }
    }
    Ok(())
}

/// List all outgoing webhooks for the organization
///
/// GET /api/v1/outgoing-webhooks
pub async fn list(
    State(state): State<AppState>,
    Extension(ctx): Extension<ApiKeyContext>,
) -> AppResult<Json<Vec<OutgoingWebhookSummary>>> {
    require_unrestricted_key(&ctx, "manage outgoing webhooks")?;

    let webhooks: Vec<OutgoingWebhook> = sqlx::query_as(
        r#"
        SELECT id, organization_id, name, url, secret, events, enabled,
               failure_count, last_triggered_at, last_status, created_at, updated_at
        FROM outgoing_webhooks
        WHERE organization_id = $1
        ORDER BY created_at DESC
        "#,
    )
    .bind(&ctx.organization_id)
    .fetch_all(state.db.pool())
    .await?;

    let summaries: Vec<OutgoingWebhookSummary> = webhooks.into_iter().map(|w| w.into()).collect();

    Ok(Json(summaries))
}

/// Create a new outgoing webhook
///
/// POST /api/v1/outgoing-webhooks
///
/// # Security
/// The URL is validated against SSRF attacks. Private IPs, internal hostnames,
/// and cloud metadata endpoints are blocked. HTTPS is required in production.
/// Plan limits are enforced before creating webhooks.
pub async fn create(
    State(state): State<AppState>,
    Extension(ctx): Extension<ApiKeyContext>,
    Json(request): Json<CreateOutgoingWebhookRequest>,
) -> AppResult<(StatusCode, Json<OutgoingWebhook>)> {
    require_unrestricted_key(&ctx, "manage outgoing webhooks")?;

    // Validate request
    request
        .validate()
        .map_err(|e| AppError::Validation(e.to_string()))?;

    // Validate event types
    validate_events(&request.events)?;

    // SECURITY: Validate URL against SSRF attacks
    let url_options = UrlValidationOptions::default();
    validate_webhook_url(&request.url, &url_options).map_err(|e| {
        tracing::warn!(
            url = %request.url,
            error = %e,
            organization_id = %ctx.organization_id,
            "Webhook URL rejected (SSRF protection)"
        );
        AppError::Validation(format!("Invalid webhook URL: {}", e))
    })?;

    let id = Uuid::new_v4().to_string();
    let now = Utc::now();

    // Enforce the webhooks cap atomically under an advisory lock. Counts track
    // ENABLED webhooks, so a disabled webhook adds 0.
    let adding = if request.enabled { 1 } else { 0 };
    let mut tx = state.db.pool().begin().await?;
    lock_resource(&mut tx, &ctx.organization_id, "webhooks").await?;
    if let Err(response) =
        check_resource_limit_conn(&mut tx, &ctx.organization_id, "webhooks", adding).await
    {
        return Err(AppError::LimitExceeded(Box::new(response)));
    }

    let webhook: OutgoingWebhook = sqlx::query_as(
        r#"
        INSERT INTO outgoing_webhooks (
            id, organization_id, name, url, secret, events, enabled,
            failure_count, created_at, updated_at
        )
        VALUES ($1, $2, $3, $4, $5, $6, $7, 0, $8, $8)
        RETURNING id, organization_id, name, url, secret, events, enabled,
                  failure_count, last_triggered_at, last_status, created_at, updated_at
        "#,
    )
    .bind(&id)
    .bind(&ctx.organization_id)
    .bind(&request.name)
    .bind(&request.url)
    .bind(&request.secret)
    .bind(&request.events)
    .bind(request.enabled)
    .bind(now)
    .fetch_one(&mut *tx)
    .await?;

    tx.commit().await?;

    tracing::info!(
        webhook_id = %id,
        organization_id = %ctx.organization_id,
        name = %request.name,
        events = ?request.events,
        "Outgoing webhook created"
    );

    Ok((StatusCode::CREATED, Json(webhook)))
}

/// Get an outgoing webhook by ID
///
/// GET /api/v1/outgoing-webhooks/{id}
pub async fn get(
    State(state): State<AppState>,
    Extension(ctx): Extension<ApiKeyContext>,
    Path(id): Path<String>,
) -> AppResult<Json<OutgoingWebhook>> {
    require_unrestricted_key(&ctx, "manage outgoing webhooks")?;

    let webhook: Option<OutgoingWebhook> = sqlx::query_as(
        r#"
        SELECT id, organization_id, name, url, secret, events, enabled,
               failure_count, last_triggered_at, last_status, created_at, updated_at
        FROM outgoing_webhooks
        WHERE id = $1 AND organization_id = $2
        "#,
    )
    .bind(&id)
    .bind(&ctx.organization_id)
    .fetch_optional(state.db.pool())
    .await?;

    match webhook {
        Some(w) => Ok(Json(w)),
        None => Err(AppError::NotFound(format!("Webhook {} not found", id))),
    }
}

/// Update an outgoing webhook
///
/// PUT /api/v1/outgoing-webhooks/{id}
///
/// # Security
/// If URL is being updated, it is validated against SSRF attacks.
pub async fn update(
    State(state): State<AppState>,
    Extension(ctx): Extension<ApiKeyContext>,
    Path(id): Path<String>,
    Json(request): Json<UpdateOutgoingWebhookRequest>,
) -> AppResult<Json<OutgoingWebhook>> {
    require_unrestricted_key(&ctx, "manage outgoing webhooks")?;

    // Validate request
    request
        .validate()
        .map_err(|e| AppError::Validation(e.to_string()))?;
    // `secret` is Option<Option<String>> so the derive cannot reach it.
    request.validate_secret().map_err(AppError::Validation)?;

    // Validate event types if provided
    if let Some(ref events) = request.events {
        validate_events(events)?;
    }

    // SECURITY: Validate URL against SSRF attacks if being updated
    if let Some(ref url) = request.url {
        let url_options = UrlValidationOptions::default();
        validate_webhook_url(url, &url_options).map_err(|e| {
            tracing::warn!(
                url = %url,
                error = %e,
                organization_id = %ctx.organization_id,
                webhook_id = %id,
                "Webhook URL update rejected (SSRF protection)"
            );
            AppError::Validation(format!("Invalid webhook URL: {}", e))
        })?;
    }

    // Check if webhook exists
    let existing: Option<OutgoingWebhook> = sqlx::query_as(
        r#"
        SELECT id, organization_id, name, url, secret, events, enabled,
               failure_count, last_triggered_at, last_status, created_at, updated_at
        FROM outgoing_webhooks
        WHERE id = $1 AND organization_id = $2
        "#,
    )
    .bind(&id)
    .bind(&ctx.organization_id)
    .fetch_optional(state.db.pool())
    .await?;

    let existing =
        existing.ok_or_else(|| AppError::NotFound(format!("Webhook {} not found", id)))?;

    // Build update query dynamically
    let name = request.name.unwrap_or(existing.name);
    let url = request.url.unwrap_or(existing.url);
    let events = request.events.unwrap_or(existing.events);
    // Three-state: absent → keep, null → clear, value → replace.
    let secret = match request.secret {
        None => existing.secret,
        Some(new_secret) => new_secret,
    };
    let enabled = request.enabled.unwrap_or(existing.enabled);

    // The plan cap counts ENABLED webhooks (see get_org_resource_counts), and
    // create() charges 0 for a disabled one. Without this check an org could
    // create N disabled webhooks for free and then enable them all, so the
    // false→true transition is where the cap must actually be paid. Turning a
    // webhook OFF must never fail on quota, hence the narrow condition.
    let is_enabling = enabled && !existing.enabled;

    let mut tx = state.db.pool().begin().await?;
    if is_enabling {
        lock_resource(&mut tx, &ctx.organization_id, "webhooks").await?;
        if let Err(response) =
            check_resource_limit_conn(&mut tx, &ctx.organization_id, "webhooks", 1).await
        {
            return Err(AppError::LimitExceeded(Box::new(response)));
        }
    }

    let webhook: OutgoingWebhook = sqlx::query_as(
        r#"
        UPDATE outgoing_webhooks
        SET name = $1, url = $2, events = $3, secret = $4, enabled = $5, updated_at = NOW()
        WHERE id = $6 AND organization_id = $7
        RETURNING id, organization_id, name, url, secret, events, enabled,
                  failure_count, last_triggered_at, last_status, created_at, updated_at
        "#,
    )
    .bind(&name)
    .bind(&url)
    .bind(&events)
    .bind(&secret)
    .bind(enabled)
    .bind(&id)
    .bind(&ctx.organization_id)
    .fetch_one(&mut *tx)
    .await?;

    tx.commit().await?;

    tracing::info!(
        webhook_id = %id,
        organization_id = %ctx.organization_id,
        "Outgoing webhook updated"
    );

    Ok(Json(webhook))
}

/// Delete an outgoing webhook
///
/// DELETE /api/v1/outgoing-webhooks/{id}
pub async fn delete(
    State(state): State<AppState>,
    Extension(ctx): Extension<ApiKeyContext>,
    Path(id): Path<String>,
) -> AppResult<StatusCode> {
    require_unrestricted_key(&ctx, "manage outgoing webhooks")?;

    let result = sqlx::query(
        r#"
        DELETE FROM outgoing_webhooks
        WHERE id = $1 AND organization_id = $2
        "#,
    )
    .bind(&id)
    .bind(&ctx.organization_id)
    .execute(state.db.pool())
    .await?;

    if result.rows_affected() == 0 {
        return Err(AppError::NotFound(format!("Webhook {} not found", id)));
    }

    tracing::info!(
        webhook_id = %id,
        organization_id = %ctx.organization_id,
        "Outgoing webhook deleted"
    );

    Ok(StatusCode::NO_CONTENT)
}

/// Test an outgoing webhook by sending a test payload
///
/// POST /api/v1/outgoing-webhooks/{id}/test
///
/// # Security
/// The stored URL is re-validated before making the request to prevent
/// SSRF attacks (in case validation rules were updated or URL was stored
/// before validation was added).
pub async fn test(
    State(state): State<AppState>,
    Extension(ctx): Extension<ApiKeyContext>,
    Path(id): Path<String>,
) -> AppResult<Json<TestWebhookResponse>> {
    require_unrestricted_key(&ctx, "manage outgoing webhooks")?;

    // Get the webhook
    let webhook: Option<OutgoingWebhook> = sqlx::query_as(
        r#"
        SELECT id, organization_id, name, url, secret, events, enabled,
               failure_count, last_triggered_at, last_status, created_at, updated_at
        FROM outgoing_webhooks
        WHERE id = $1 AND organization_id = $2
        "#,
    )
    .bind(&id)
    .bind(&ctx.organization_id)
    .fetch_optional(state.db.pool())
    .await?;

    let webhook = webhook.ok_or_else(|| AppError::NotFound(format!("Webhook {} not found", id)))?;

    // SECURITY: Re-validate URL before making request
    let url_options = UrlValidationOptions::default();
    validate_webhook_url(&webhook.url, &url_options).map_err(|e| {
        tracing::warn!(
            url = %webhook.url,
            error = %e,
            webhook_id = %id,
            organization_id = %ctx.organization_id,
            "Webhook test blocked (SSRF protection)"
        );
        AppError::Validation(format!("Webhook URL no longer valid: {}", e))
    })?;

    // Create test payload
    let test_payload = serde_json::json!({
        "event": "webhook.test",
        "webhook_id": webhook.id,
        "organization_id": ctx.organization_id,
        "timestamp": Utc::now().to_rfc3339(),
        "test": true,
        "message": "This is a test webhook from Spooled Cloud"
    });

    // Send test request
    // SECURITY: Use hardened outbound client to prevent SSRF via redirects.
    let client = build_outbound_http_client(
        std::time::Duration::from_secs(10),
        std::time::Duration::from_secs(5),
        "Spooled-WebhookTest/1.0",
    );

    let start = std::time::Instant::now();

    // Generate signature if secret is configured
    let timestamp = Utc::now().timestamp();
    let payload_json = serde_json::to_string(&test_payload).unwrap_or_default();

    let mut request_builder = client
        .post(&webhook.url)
        .header("Content-Type", "application/json")
        .header("X-Spooled-Event", "webhook.test")
        .header("X-Spooled-Timestamp", timestamp.to_string())
        .header("X-Spooled-Delivery-Attempt", "1");

    // Add signature if secret is configured
    if let Some(ref secret) = webhook.secret {
        use hmac::{Hmac, KeyInit, Mac};
        use sha2::Sha256;
        type HmacSha256 = Hmac<Sha256>;

        let message = format!("{}.{}", timestamp, payload_json);
        let mut mac = HmacSha256::new_from_slice(secret.as_bytes())
            .map_err(|_| AppError::Internal("Invalid HMAC key".to_string()))?;
        mac.update(message.as_bytes());
        let signature = format!("sha256={}", hex::encode(mac.finalize().into_bytes()));
        request_builder = request_builder.header("X-Spooled-Signature", signature);
    }

    let result = request_builder.body(payload_json).send().await;

    let response_time_ms = start.elapsed().as_millis() as i64;

    match result {
        Ok(response) => {
            let status = response.status();
            let success = status.is_success();

            Ok(Json(TestWebhookResponse {
                success,
                status_code: Some(status.as_u16()),
                response_time_ms,
                error: if success {
                    None
                } else {
                    Some(format!("HTTP {}", status))
                },
            }))
        }
        Err(e) => {
            tracing::warn!(
                webhook_id = %id,
                error = %e,
                "Webhook test failed"
            );

            Ok(Json(TestWebhookResponse {
                success: false,
                status_code: None,
                response_time_ms,
                error: Some("Request failed".to_string()),
            }))
        }
    }
}

/// Get delivery history for an outgoing webhook
///
/// GET /api/v1/outgoing-webhooks/{id}/deliveries
pub async fn deliveries(
    State(state): State<AppState>,
    Extension(ctx): Extension<ApiKeyContext>,
    Path(id): Path<String>,
) -> AppResult<Json<Vec<OutgoingWebhookDelivery>>> {
    // Delivery rows carry the full event envelope, including the job payload of
    // whichever queue produced the event. There is no queue column to filter on,
    // so reading delivery history is an org-admin operation — a queue-scoped key
    // must not use it to observe queues outside its scope.
    require_unrestricted_key(&ctx, "manage outgoing webhooks")?;

    // Verify webhook belongs to organization
    let webhook_exists: Option<(String,)> = sqlx::query_as(
        r#"
        SELECT id FROM outgoing_webhooks
        WHERE id = $1 AND organization_id = $2
        "#,
    )
    .bind(&id)
    .bind(&ctx.organization_id)
    .fetch_optional(state.db.pool())
    .await?;

    if webhook_exists.is_none() {
        return Err(AppError::NotFound(format!("Webhook {} not found", id)));
    }

    let deliveries: Vec<OutgoingWebhookDelivery> = sqlx::query_as(
        r#"
        SELECT id, webhook_id, event, payload, status, status_code,
               response_body, error, attempts, created_at, delivered_at
        FROM outgoing_webhook_deliveries
        WHERE webhook_id = $1
        ORDER BY created_at DESC
        LIMIT 100
        "#,
    )
    .bind(&id)
    .fetch_all(state.db.pool())
    .await?;

    Ok(Json(deliveries))
}

/// Response for retry delivery
#[derive(Debug, Serialize)]
pub struct RetryDeliveryResponse {
    /// Whether the retry was successful
    pub success: bool,
    /// Status message
    pub message: String,
}

/// Maximum manual retry attempts allowed per delivery
const MAX_MANUAL_RETRY_ATTEMPTS: i32 = 10;

/// Retry a specific webhook delivery
///
/// POST /api/v1/outgoing-webhooks/{id}/retry/{delivery_id}
///
/// SECURITY: Now enforces:
/// - Cannot retry successful deliveries
/// - Maximum 10 manual retry attempts per delivery
pub async fn retry_delivery(
    State(state): State<AppState>,
    Extension(ctx): Extension<ApiKeyContext>,
    Path((webhook_id, delivery_id)): Path<(String, String)>,
) -> AppResult<Json<RetryDeliveryResponse>> {
    // Re-firing a stored delivery re-sends another queue's event payload to the
    // org endpoint, so this is an org-admin operation like the rest of this file.
    require_unrestricted_key(&ctx, "manage outgoing webhooks")?;

    // Validate UUID shape, but bind as TEXT — columns are TEXT PKs (BE-15).
    Uuid::parse_str(&webhook_id)
        .map_err(|_| AppError::Validation("Invalid webhook ID format".to_string()))?;
    let parsed_delivery_id = Uuid::parse_str(&delivery_id)
        .map_err(|_| AppError::Validation("Invalid delivery ID format".to_string()))?;

    // Verify webhook belongs to organization
    let webhook: Option<OutgoingWebhook> =
        sqlx::query_as("SELECT * FROM outgoing_webhooks WHERE id = $1 AND organization_id = $2")
            .bind(&webhook_id)
            .bind(&ctx.organization_id)
            .fetch_optional(state.db.pool())
            .await?;

    if webhook.is_none() {
        return Err(AppError::NotFound("Webhook not found".to_string()));
    }

    // Verify delivery exists and belongs to webhook
    let delivery: Option<OutgoingWebhookDelivery> = sqlx::query_as(
        "SELECT * FROM outgoing_webhook_deliveries WHERE id = $1 AND webhook_id = $2",
    )
    .bind(&delivery_id)
    .bind(&webhook_id)
    .fetch_optional(state.db.pool())
    .await?;

    let delivery = match delivery {
        Some(d) => d,
        None => return Err(AppError::NotFound("Delivery not found".to_string())),
    };

    // SECURITY: Cannot retry successful deliveries
    if delivery.status == "success" {
        return Err(AppError::BadRequest(
            "Cannot retry a successful delivery".to_string(),
        ));
    }

    // SECURITY: Enforce maximum manual retry attempts
    if delivery.attempts >= MAX_MANUAL_RETRY_ATTEMPTS {
        return Err(AppError::BadRequest(format!(
            "Maximum retry attempts ({}) exceeded for this delivery",
            MAX_MANUAL_RETRY_ATTEMPTS
        )));
    }

    let retry_success = state
        .outgoing_webhooks
        .retry_delivery(parsed_delivery_id)
        .await
        .map_err(|e| AppError::Internal(format!("Failed to retry delivery: {}", e)))?;

    Ok(Json(RetryDeliveryResponse {
        success: retry_success,
        message: if retry_success {
            "Delivery retried successfully".to_string()
        } else {
            "Delivery retry attempted but target did not return success".to_string()
        },
    }))
}
