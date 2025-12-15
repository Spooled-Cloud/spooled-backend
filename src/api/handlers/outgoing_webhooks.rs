//! Outgoing webhook handlers
//!
//! CRUD operations for managing outgoing webhook configurations.
//! These webhooks send notifications to external URLs when events occur.

use axum::{
    extract::{Path, State},
    http::StatusCode,
    Extension, Json,
};
use chrono::Utc;
use uuid::Uuid;
use validator::Validate;

use crate::api::AppState;
use crate::error::{AppError, AppResult};
use crate::models::{
    ApiKeyContext, CreateOutgoingWebhookRequest, OutgoingWebhook, OutgoingWebhookDelivery,
    OutgoingWebhookSummary, TestWebhookResponse, UpdateOutgoingWebhookRequest,
    VALID_WEBHOOK_EVENTS,
};
use crate::outgoing_webhooks::service::OutgoingWebhookService;

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
pub async fn create(
    State(state): State<AppState>,
    Extension(ctx): Extension<ApiKeyContext>,
    Json(request): Json<CreateOutgoingWebhookRequest>,
) -> AppResult<(StatusCode, Json<OutgoingWebhook>)> {
    // Validate request
    request
        .validate()
        .map_err(|e| AppError::Validation(e.to_string()))?;

    // Validate event types
    validate_events(&request.events)?;

    // Validate URL scheme (only HTTPS in production)
    let parsed_url = url::Url::parse(&request.url)
        .map_err(|e| AppError::Validation(format!("Invalid URL: {}", e)))?;

    if parsed_url.scheme() != "https" && parsed_url.scheme() != "http" {
        return Err(AppError::Validation(
            "URL must use HTTP or HTTPS scheme".to_string(),
        ));
    }

    let id = Uuid::new_v4().to_string();
    let now = Utc::now();

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
    .fetch_one(state.db.pool())
    .await?;

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
pub async fn update(
    State(state): State<AppState>,
    Extension(ctx): Extension<ApiKeyContext>,
    Path(id): Path<String>,
    Json(request): Json<UpdateOutgoingWebhookRequest>,
) -> AppResult<Json<OutgoingWebhook>> {
    // Validate request
    request
        .validate()
        .map_err(|e| AppError::Validation(e.to_string()))?;

    // Validate event types if provided
    if let Some(ref events) = request.events {
        validate_events(events)?;
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
    let secret = request.secret.or(existing.secret);
    let enabled = request.enabled.unwrap_or(existing.enabled);

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
    .fetch_one(state.db.pool())
    .await?;

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
pub async fn test(
    State(state): State<AppState>,
    Extension(ctx): Extension<ApiKeyContext>,
    Path(id): Path<String>,
) -> AppResult<Json<TestWebhookResponse>> {
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
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .map_err(|e| AppError::Internal(format!("Failed to create HTTP client: {}", e)))?;

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
        use hmac::{Hmac, Mac};
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

/// Retry a specific webhook delivery (replays the stored payload)
///
/// POST /api/v1/outgoing-webhooks/{id}/retry/{delivery_id}
pub async fn retry_delivery(
    State(state): State<AppState>,
    Extension(ctx): Extension<ApiKeyContext>,
    Path((id, delivery_id)): Path<(String, String)>,
) -> AppResult<(StatusCode, Json<serde_json::Value>)> {
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

    // Verify delivery belongs to webhook
    let delivery_exists: Option<(String,)> = sqlx::query_as(
        r#"
        SELECT id FROM outgoing_webhook_deliveries
        WHERE id = $1 AND webhook_id = $2
        "#,
    )
    .bind(&delivery_id)
    .bind(&id)
    .fetch_optional(state.db.pool())
    .await?;

    if delivery_exists.is_none() {
        return Err(AppError::NotFound(format!(
            "Delivery {} not found for webhook {}",
            delivery_id, id
        )));
    }

    let svc = OutgoingWebhookService::new(state.db.pool_arc());
    let webhook_id = id.clone();
    let delivery_id_clone = delivery_id.clone();
    tokio::spawn(async move {
        if let Err(e) = svc.retry_delivery(&webhook_id, &delivery_id_clone).await {
            tracing::error!(
                error = %e,
                webhook_id = %webhook_id,
                delivery_id = %delivery_id_clone,
                "Retry delivery failed"
            );
        }
    });

    Ok((
        StatusCode::OK,
        Json(serde_json::json!({
            "success": true,
            "webhookId": id,
            "deliveryId": delivery_id
        })),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::VALID_WEBHOOK_EVENTS;

    #[test]
    fn test_validate_events_valid() {
        let valid_events = vec!["job.created".to_string(), "job.completed".to_string()];
        assert!(validate_events(&valid_events).is_ok());
    }

    #[test]
    fn test_validate_events_invalid() {
        let invalid_events = vec!["job.created".to_string(), "invalid.event".to_string()];
        let result = validate_events(&invalid_events);
        assert!(result.is_err());
        if let Err(AppError::Validation(msg)) = result {
            assert!(msg.contains("invalid.event"));
        }
    }

    #[test]
    fn test_validate_events_empty() {
        let empty_events: Vec<String> = vec![];
        assert!(validate_events(&empty_events).is_ok());
    }

    #[test]
    fn test_validate_events_all_valid_event_types() {
        let all_events: Vec<String> = VALID_WEBHOOK_EVENTS.iter().map(|s| s.to_string()).collect();
        assert!(validate_events(&all_events).is_ok());
    }

    #[test]
    fn test_outgoing_webhook_summary_serialization() {
        let summary = OutgoingWebhookSummary {
            id: "wh-123".to_string(),
            organization_id: "org-1".to_string(),
            name: "My Webhook".to_string(),
            url: "https://example.com/webhook".to_string(),
            events: vec!["job.completed".to_string()],
            enabled: true,
            failure_count: 0,
            last_triggered_at: None,
            last_status: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };

        let json = serde_json::to_string(&summary).unwrap();
        assert!(json.contains("wh-123"));
        assert!(json.contains("My Webhook"));
        assert!(json.contains("https://example.com/webhook"));
        assert!(json.contains("job.completed"));
    }

    #[test]
    fn test_outgoing_webhook_summary_with_failure() {
        let summary = OutgoingWebhookSummary {
            id: "wh-456".to_string(),
            organization_id: "org-2".to_string(),
            name: "Failed Webhook".to_string(),
            url: "https://example.com/fail".to_string(),
            events: vec!["job.failed".to_string()],
            enabled: false,
            failure_count: 5,
            last_triggered_at: Some(Utc::now()),
            last_status: Some("failed".to_string()),
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };

        let json = serde_json::to_string(&summary).unwrap();
        assert!(json.contains("\"enabled\":false"));
        assert!(json.contains("\"failure_count\":5"));
        assert!(json.contains("failed"));
    }

    #[test]
    fn test_test_webhook_response_serialization() {
        let response = TestWebhookResponse {
            success: true,
            status_code: Some(200),
            response_time_ms: 150,
            error: None,
        };

        let json = serde_json::to_string(&response).unwrap();
        assert!(json.contains("\"success\":true"));
        assert!(json.contains("\"status_code\":200"));
        assert!(json.contains("\"response_time_ms\":150"));
    }

    #[test]
    fn test_test_webhook_response_with_error() {
        let response = TestWebhookResponse {
            success: false,
            status_code: None,
            response_time_ms: 5000,
            error: Some("Connection timeout".to_string()),
        };

        let json = serde_json::to_string(&response).unwrap();
        assert!(json.contains("\"success\":false"));
        assert!(json.contains("Connection timeout"));
    }

    #[test]
    fn test_create_outgoing_webhook_request_validation() {
        use validator::Validate;

        // Valid request
        let valid = CreateOutgoingWebhookRequest {
            name: "My Webhook".to_string(),
            url: "https://example.com/webhook".to_string(),
            secret: Some("my-secret-key-12345".to_string()),
            events: vec!["job.completed".to_string()],
            enabled: true,
        };
        assert!(valid.validate().is_ok());

        // Name too short
        let short_name = CreateOutgoingWebhookRequest {
            name: "".to_string(),
            url: "https://example.com/webhook".to_string(),
            secret: None,
            events: vec!["job.completed".to_string()],
            enabled: true,
        };
        assert!(short_name.validate().is_err());

        // Invalid URL format (validation should handle this at struct level via #[validate(url)])
        let invalid_url = CreateOutgoingWebhookRequest {
            name: "Test".to_string(),
            url: "not-a-url".to_string(),
            secret: None,
            events: vec!["job.completed".to_string()],
            enabled: true,
        };
        // URL validation happens in struct via #[validate(url)]
        assert!(invalid_url.validate().is_err());
    }

    #[test]
    fn test_outgoing_webhook_delivery_serialization() {
        let delivery = OutgoingWebhookDelivery {
            id: "del-123".to_string(),
            webhook_id: "wh-456".to_string(),
            event: "job.completed".to_string(),
            payload: serde_json::json!({"job_id": "job-789"}),
            status: "delivered".to_string(),
            status_code: Some(200),
            response_body: Some("OK".to_string()),
            error: None,
            attempts: 1,
            created_at: Utc::now(),
            delivered_at: Some(Utc::now()),
        };

        let json = serde_json::to_string(&delivery).unwrap();
        assert!(json.contains("del-123"));
        assert!(json.contains("wh-456"));
        assert!(json.contains("job.completed"));
        assert!(json.contains("delivered"));
    }

    #[test]
    fn test_outgoing_webhook_delivery_failed() {
        let delivery = OutgoingWebhookDelivery {
            id: "del-999".to_string(),
            webhook_id: "wh-456".to_string(),
            event: "job.failed".to_string(),
            payload: serde_json::json!({}),
            status: "failed".to_string(),
            status_code: Some(500),
            response_body: None,
            error: Some("Server error".to_string()),
            attempts: 3,
            created_at: Utc::now(),
            delivered_at: None,
        };

        let json = serde_json::to_string(&delivery).unwrap();
        assert!(json.contains("\"status\":\"failed\""));
        assert!(json.contains("\"attempts\":3"));
        assert!(json.contains("Server error"));
    }
}
