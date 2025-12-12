//! Webhook models and related types
//!
//! This module contains:
//! - Incoming webhooks: HTTP requests from external services (GitHub, Stripe, etc.)
//!   that are converted into jobs.
//! - Outgoing webhooks: Notification configurations for sending events to external URLs
//!   when system events occur (job.completed, queue.paused, etc.)

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::FromRow;

// ============================================================================
// OUTGOING WEBHOOK TYPES (for sending notifications)
// ============================================================================

/// Valid event types for outgoing webhooks
pub const VALID_WEBHOOK_EVENTS: &[&str] = &[
    "job.created",
    "job.started",
    "job.completed",
    "job.failed",
    "job.cancelled",
    "queue.paused",
    "queue.resumed",
    "worker.registered",
    "worker.deregistered",
    "schedule.triggered",
];

/// Outgoing webhook configuration
#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct OutgoingWebhook {
    /// Unique identifier
    pub id: String,
    /// Organization that owns this webhook
    pub organization_id: String,
    /// Human-readable name
    pub name: String,
    /// Target URL for webhook delivery
    pub url: String,
    /// Optional secret for HMAC signing
    #[serde(skip_serializing)]
    pub secret: Option<String>,
    /// Event types that trigger this webhook
    pub events: Vec<String>,
    /// Whether the webhook is enabled
    pub enabled: bool,
    /// Number of consecutive failures
    pub failure_count: i32,
    /// Last time webhook was triggered
    pub last_triggered_at: Option<DateTime<Utc>>,
    /// Status of last delivery ('success' or 'failed')
    pub last_status: Option<String>,
    /// Creation timestamp
    pub created_at: DateTime<Utc>,
    /// Last update timestamp
    pub updated_at: DateTime<Utc>,
}

/// Summary of outgoing webhook for list responses
#[derive(Debug, Clone, Serialize)]
pub struct OutgoingWebhookSummary {
    pub id: String,
    pub organization_id: String,
    pub name: String,
    pub url: String,
    pub events: Vec<String>,
    pub enabled: bool,
    pub failure_count: i32,
    pub last_triggered_at: Option<DateTime<Utc>>,
    pub last_status: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl From<OutgoingWebhook> for OutgoingWebhookSummary {
    fn from(w: OutgoingWebhook) -> Self {
        Self {
            id: w.id,
            organization_id: w.organization_id,
            name: w.name,
            url: w.url,
            events: w.events,
            enabled: w.enabled,
            failure_count: w.failure_count,
            last_triggered_at: w.last_triggered_at,
            last_status: w.last_status,
            created_at: w.created_at,
            updated_at: w.updated_at,
        }
    }
}

/// Request to create an outgoing webhook
#[derive(Debug, Clone, Deserialize, validator::Validate)]
pub struct CreateOutgoingWebhookRequest {
    /// Human-readable name
    #[validate(length(min = 1, max = 255, message = "Name must be 1-255 characters"))]
    pub name: String,
    /// Target URL
    #[validate(length(min = 1, max = 2048, message = "URL must be 1-2048 characters"))]
    #[validate(url(message = "Invalid URL format"))]
    pub url: String,
    /// Event types to subscribe to
    #[validate(length(min = 1, message = "At least one event type is required"))]
    pub events: Vec<String>,
    /// Optional secret for HMAC signing
    #[validate(length(max = 255, message = "Secret must be at most 255 characters"))]
    pub secret: Option<String>,
    /// Whether the webhook is enabled (defaults to true)
    #[serde(default = "default_enabled")]
    pub enabled: bool,
}

fn default_enabled() -> bool {
    true
}

/// Request to update an outgoing webhook
#[derive(Debug, Clone, Deserialize, validator::Validate)]
pub struct UpdateOutgoingWebhookRequest {
    /// Human-readable name
    #[validate(length(min = 1, max = 255, message = "Name must be 1-255 characters"))]
    pub name: Option<String>,
    /// Target URL
    #[validate(length(min = 1, max = 2048, message = "URL must be 1-2048 characters"))]
    pub url: Option<String>,
    /// Event types to subscribe to
    pub events: Option<Vec<String>>,
    /// Optional secret for HMAC signing
    #[validate(length(max = 255, message = "Secret must be at most 255 characters"))]
    pub secret: Option<String>,
    /// Whether the webhook is enabled
    pub enabled: Option<bool>,
}

/// Response for webhook test endpoint
#[derive(Debug, Clone, Serialize)]
pub struct TestWebhookResponse {
    /// Whether the test was successful
    pub success: bool,
    /// HTTP status code from the target
    pub status_code: Option<u16>,
    /// Response time in milliseconds
    pub response_time_ms: i64,
    /// Error message if failed
    pub error: Option<String>,
}

/// Outgoing webhook delivery record
#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct OutgoingWebhookDelivery {
    /// Unique identifier
    pub id: String,
    /// Webhook that triggered this delivery
    pub webhook_id: String,
    /// Event type that triggered the delivery
    pub event: String,
    /// Payload sent to the webhook
    pub payload: serde_json::Value,
    /// Delivery status ('pending', 'success', 'failed')
    pub status: String,
    /// HTTP status code from target
    pub status_code: Option<i32>,
    /// Response body from target (truncated)
    pub response_body: Option<String>,
    /// Error message if failed
    pub error: Option<String>,
    /// Number of delivery attempts
    pub attempts: i32,
    /// Creation timestamp
    pub created_at: DateTime<Utc>,
    /// Successful delivery timestamp
    pub delivered_at: Option<DateTime<Utc>>,
}

// ============================================================================
// INCOMING WEBHOOK TYPES (for receiving events from external services)
// ============================================================================

/// Webhook provider enumeration
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum WebhookProvider {
    GitHub,
    Stripe,
    Custom,
}

impl std::fmt::Display for WebhookProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WebhookProvider::GitHub => write!(f, "github"),
            WebhookProvider::Stripe => write!(f, "stripe"),
            WebhookProvider::Custom => write!(f, "custom"),
        }
    }
}

/// Webhook delivery record
#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct WebhookDelivery {
    /// Unique identifier
    pub id: String,
    /// Organization that owns this delivery
    pub organization_id: String,
    /// Provider (github, stripe, custom)
    pub provider: String,
    /// Event type (e.g., "push", "invoice.paid")
    pub event_type: String,
    /// Raw payload
    pub payload: serde_json::Value,
    /// Signature from provider
    pub signature: Option<String>,
    /// Queue where job was created (if any)
    pub matched_queue: Option<String>,
    /// Delivery timestamp
    pub created_at: DateTime<Utc>,
}

/// GitHub webhook event
#[derive(Debug, Deserialize)]
pub struct GitHubWebhookEvent {
    /// Event action (e.g., "opened", "closed")
    pub action: Option<String>,
    /// Repository info
    pub repository: Option<GitHubRepository>,
    /// Sender info
    pub sender: Option<GitHubUser>,
    /// Full payload for passthrough
    #[serde(flatten)]
    pub extra: serde_json::Value,
}

/// GitHub repository info
#[derive(Debug, Deserialize, Serialize)]
pub struct GitHubRepository {
    pub id: i64,
    pub name: String,
    pub full_name: String,
    pub private: bool,
}

/// GitHub user info
#[derive(Debug, Deserialize, Serialize)]
pub struct GitHubUser {
    pub id: i64,
    pub login: String,
}

/// Stripe webhook event
#[derive(Debug, Deserialize)]
pub struct StripeWebhookEvent {
    /// Event ID
    pub id: String,
    /// Event type (e.g., "invoice.paid")
    #[serde(rename = "type")]
    pub event_type: String,
    /// Event data
    pub data: StripeEventData,
    /// API version
    pub api_version: Option<String>,
    /// Creation timestamp
    pub created: i64,
}

/// Stripe event data
#[derive(Debug, Deserialize)]
pub struct StripeEventData {
    /// The object that triggered the event
    pub object: serde_json::Value,
}

use validator::Validate;

/// Validate queue name to prevent injection attacks
fn validate_queue_name(queue_name: &str) -> Result<(), validator::ValidationError> {
    // Queue names should only contain safe characters
    let is_valid = queue_name
        .chars()
        .all(|c| c.is_alphanumeric() || c == '-' || c == '_' || c == '.');

    if !is_valid {
        let mut err = validator::ValidationError::new("invalid_queue_name");
        err.message = Some(std::borrow::Cow::Borrowed(
            "Queue name can only contain alphanumeric characters, dashes, underscores, and dots",
        ));
        return Err(err);
    }

    // Prevent path traversal attempts
    if queue_name.contains("..") || queue_name.starts_with('.') || queue_name.starts_with('-') {
        let mut err = validator::ValidationError::new("invalid_queue_name");
        err.message = Some(std::borrow::Cow::Borrowed(
            "Queue name cannot start with dots/dashes or contain path traversal patterns",
        ));
        return Err(err);
    }

    // Prevent reserved/dangerous names
    let reserved_names = ["admin", "system", "internal", "root", "default"];
    if reserved_names.contains(&queue_name.to_lowercase().as_str()) {
        let mut err = validator::ValidationError::new("reserved_queue_name");
        err.message = Some(std::borrow::Cow::Borrowed(
            "Queue name is reserved and cannot be used",
        ));
        return Err(err);
    }

    Ok(())
}

/// Maximum custom webhook payload size (5MB)
const MAX_CUSTOM_WEBHOOK_PAYLOAD_SIZE: usize = 5 * 1024 * 1024;

/// Validate payload size
fn validate_custom_payload_size(
    payload: &serde_json::Value,
) -> Result<(), validator::ValidationError> {
    let json_str = serde_json::to_string(payload).unwrap_or_default();
    if json_str.len() > MAX_CUSTOM_WEBHOOK_PAYLOAD_SIZE {
        let mut err = validator::ValidationError::new("payload_too_large");
        err.message = Some(std::borrow::Cow::Owned(format!(
            "Payload too large: {} bytes (max: {} bytes)",
            json_str.len(),
            MAX_CUSTOM_WEBHOOK_PAYLOAD_SIZE
        )));
        return Err(err);
    }
    Ok(())
}

/// Validate event type for safe characters
fn validate_event_type(event_type: &str) -> Result<(), validator::ValidationError> {
    if !event_type
        .chars()
        .all(|c| c.is_alphanumeric() || c == '-' || c == '_' || c == '.')
    {
        let mut err = validator::ValidationError::new("invalid_event_type");
        err.message = Some(std::borrow::Cow::Borrowed(
            "Event type can only contain alphanumeric characters, dashes, underscores, and dots",
        ));
        return Err(err);
    }
    Ok(())
}

/// Custom webhook request
///
#[derive(Debug, Deserialize, Validate)]
pub struct CustomWebhookRequest {
    /// Target queue
    #[validate(length(min = 1, max = 255, message = "Queue name must be 1-255 characters"))]
    #[validate(custom(function = "validate_queue_name"))]
    pub queue_name: String,
    /// Event type
    #[validate(length(max = 100, message = "Event type must be at most 100 characters"))]
    #[validate(custom(function = "validate_event_type"))]
    pub event_type: Option<String>,
    /// Payload to process (max 5MB)
    /// Now validated for size
    #[validate(custom(function = "validate_custom_payload_size"))]
    pub payload: serde_json::Value,
    /// Optional idempotency key
    #[validate(length(max = 255, message = "Idempotency key must be at most 255 characters"))]
    pub idempotency_key: Option<String>,
    /// Optional priority
    #[validate(range(min = -100, max = 100, message = "Priority must be between -100 and 100"))]
    pub priority: Option<i32>,
}

/// Webhook delivery summary for list responses
#[derive(Debug, Serialize)]
pub struct WebhookDeliverySummary {
    pub id: String,
    pub provider: String,
    pub event_type: String,
    pub matched_queue: Option<String>,
    pub created_at: DateTime<Utc>,
}

impl From<WebhookDelivery> for WebhookDeliverySummary {
    fn from(delivery: WebhookDelivery) -> Self {
        Self {
            id: delivery.id,
            provider: delivery.provider,
            event_type: delivery.event_type,
            matched_queue: delivery.matched_queue,
            created_at: delivery.created_at,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_webhook_provider_display() {
        assert_eq!(WebhookProvider::GitHub.to_string(), "github");
        assert_eq!(WebhookProvider::Stripe.to_string(), "stripe");
        assert_eq!(WebhookProvider::Custom.to_string(), "custom");
    }

    #[test]
    fn test_github_webhook_deserialization() {
        let json = r#"{
            "action": "opened",
            "repository": {
                "id": 123,
                "name": "test-repo",
                "full_name": "owner/test-repo",
                "private": false
            },
            "sender": {
                "id": 456,
                "login": "testuser"
            }
        }"#;

        let event: GitHubWebhookEvent = serde_json::from_str(json).unwrap();
        assert_eq!(event.action, Some("opened".to_string()));
        assert_eq!(event.repository.unwrap().name, "test-repo");
    }

    #[test]
    fn test_stripe_webhook_deserialization() {
        let json = r#"{
            "id": "evt_123",
            "type": "invoice.paid",
            "data": {
                "object": {"id": "inv_123"}
            },
            "api_version": "2023-10-16",
            "created": 1234567890
        }"#;

        let event: StripeWebhookEvent = serde_json::from_str(json).unwrap();
        assert_eq!(event.id, "evt_123");
        assert_eq!(event.event_type, "invoice.paid");
    }

    #[test]
    fn test_valid_webhook_events() {
        assert!(VALID_WEBHOOK_EVENTS.contains(&"job.created"));
        assert!(VALID_WEBHOOK_EVENTS.contains(&"job.completed"));
        assert!(VALID_WEBHOOK_EVENTS.contains(&"queue.paused"));
        assert!(VALID_WEBHOOK_EVENTS.contains(&"worker.registered"));
        assert!(!VALID_WEBHOOK_EVENTS.contains(&"invalid.event"));
    }

    #[test]
    fn test_create_outgoing_webhook_request_deserialization() {
        let json = r#"{
            "name": "My Webhook",
            "url": "https://example.com/webhook",
            "events": ["job.completed", "job.failed"],
            "secret": "my-secret"
        }"#;

        let request: CreateOutgoingWebhookRequest = serde_json::from_str(json).unwrap();
        assert_eq!(request.name, "My Webhook");
        assert_eq!(request.url, "https://example.com/webhook");
        assert_eq!(request.events.len(), 2);
        assert_eq!(request.secret, Some("my-secret".to_string()));
        assert!(request.enabled); // default value
    }

    #[test]
    fn test_update_outgoing_webhook_request_partial() {
        let json = r#"{"name": "Updated Name"}"#;

        let request: UpdateOutgoingWebhookRequest = serde_json::from_str(json).unwrap();
        assert_eq!(request.name, Some("Updated Name".to_string()));
        assert!(request.url.is_none());
        assert!(request.events.is_none());
        assert!(request.enabled.is_none());
    }

    #[test]
    fn test_outgoing_webhook_summary_from() {
        let webhook = OutgoingWebhook {
            id: "wh_123".to_string(),
            organization_id: "org_123".to_string(),
            name: "Test Webhook".to_string(),
            url: "https://example.com".to_string(),
            secret: Some("secret".to_string()),
            events: vec!["job.completed".to_string()],
            enabled: true,
            failure_count: 0,
            last_triggered_at: None,
            last_status: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };

        let summary: OutgoingWebhookSummary = webhook.into();
        assert_eq!(summary.id, "wh_123");
        assert_eq!(summary.name, "Test Webhook");
        // Note: secret is not included in summary (skip_serializing)
    }
}
