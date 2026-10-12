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
    "job.deadlettered",
    "job.retried",
    "workflow.completed",
    "workflow.failed",
];

/// Body formats an outgoing webhook can be delivered as.
pub const VALID_WEBHOOK_FORMATS: &[&str] = &["json", "slack", "discord", "teams"];

/// The body an outgoing webhook is delivered as.
///
/// `Json` is the signed envelope (`id`/`event`/`created_at`/`data`), unchanged.
/// The others turn that envelope into the message a chat service's incoming
/// webhook URL accepts, so such a URL can be pasted in as it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum WebhookFormat {
    #[default]
    Json,
    /// `{"text": ...}` (Slack incoming webhooks)
    Slack,
    /// `{"content": ...}` (Discord webhooks)
    Discord,
    /// A `MessageCard` (Microsoft Teams incoming webhooks)
    Teams,
}

impl WebhookFormat {
    /// Parse a requested format, ignoring case and surrounding spaces.
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "json" => Some(Self::Json),
            "slack" => Some(Self::Slack),
            "discord" => Some(Self::Discord),
            "teams" => Some(Self::Teams),
            _ => None,
        }
    }

    /// The stored format. Anything unknown (an empty value from a row read
    /// without the column) is delivered as the JSON envelope.
    pub fn from_stored(value: &str) -> Self {
        Self::parse(value).unwrap_or_default()
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Json => "json",
            Self::Slack => "slack",
            Self::Discord => "discord",
            Self::Teams => "teams",
        }
    }
}

/// Check an optional requested format: `None` when omitted, else the
/// canonical value to store.
pub fn validate_webhook_format(format: Option<&str>) -> Result<Option<&'static str>, String> {
    match format {
        None => Ok(None),
        Some(value) => WebhookFormat::parse(value)
            .map(|f| Some(f.as_str()))
            .ok_or_else(|| {
                format!(
                    "Invalid format '{}'. Valid formats: {}",
                    value,
                    VALID_WEBHOOK_FORMATS.join(", ")
                )
            }),
    }
}

fn default_format() -> String {
    WebhookFormat::Json.as_str().to_string()
}

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
    /// Delivery body: `json` (default), `slack`, `discord` or `teams`
    #[sqlx(default)]
    #[serde(default = "default_format")]
    pub format: String,
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
    pub format: String,
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
            format: w.format,
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
    /// Delivery body: `json` (default), `slack`, `discord` or `teams`.
    /// Checked by [`validate_webhook_format`].
    #[serde(default)]
    pub format: Option<String>,
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
    /// Optional secret for HMAC signing.
    ///
    /// Three-state on purpose: omit the key to keep the current secret, send
    /// `null` to CLEAR it (deliveries then go out unsigned), or send a string to
    /// replace it. A plain `Option<String>` collapsed `null` into "omitted", so
    /// there was no representable request that removed a secret.
    ///
    /// `validator`'s derive cannot see through `Option<Option<T>>`, so the length
    /// bound is enforced by [`UpdateOutgoingWebhookRequest::validate_secret`].
    #[serde(default, deserialize_with = "crate::models::double_option")]
    pub secret: Option<Option<String>>,
    /// Whether the webhook is enabled
    pub enabled: Option<bool>,
    /// Delivery body (`json`, `slack`, `discord`, `teams`); omit to keep it.
    /// Checked by [`validate_webhook_format`].
    #[serde(default)]
    pub format: Option<String>,
}

/// Maximum length of a webhook signing secret.
pub const MAX_WEBHOOK_SECRET_LEN: usize = 255;

impl UpdateOutgoingWebhookRequest {
    /// Length-check the three-state `secret` field, which `#[derive(Validate)]`
    /// cannot reach. Call this alongside `validate()`.
    pub fn validate_secret(&self) -> Result<(), String> {
        if let Some(Some(secret)) = &self.secret {
            if secret.len() > MAX_WEBHOOK_SECRET_LEN {
                return Err(format!(
                    "Secret must be at most {} characters",
                    MAX_WEBHOOK_SECRET_LEN
                ));
            }
        }
        Ok(())
    }
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
    Custom,
}

impl std::fmt::Display for WebhookProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
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

// Note: GitHubWebhookEvent and StripeWebhookEvent removed.
// - GitHub webhook ingestion was removed as it's not needed for this platform.
// - Stripe webhook ingestion (for job creation) was removed because it used a
//   global secret which doesn't work for multi-tenant.
// - The billing webhook in handlers/billing.rs handles platform subscription events
//   and has its own StripeEvent struct.

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

/// POST /webhooks/{org_id}/custom — OpenAPI `WebhookResponse`.
#[derive(Debug, Serialize)]
pub struct CustomWebhookResponse {
    /// ID of the created (or idempotent existing) job
    pub job_id: String,
    /// Queue the job is in. Read from the stored row, so an idempotent replay
    /// reports the original job's queue rather than the queue in the request.
    pub queue_name: String,
    /// Job status read back from the row: `pending` for a newly inserted job,
    /// the current status (`processing`, `completed`, ...) for an idempotent replay.
    pub status: String,
}

/// One entry of the inbound webhook log (`GET /webhooks/deliveries`).
///
/// Deliberately without the stored `payload` (up to 5 MB each) and `signature`.
#[derive(Debug, Serialize, FromRow)]
pub struct WebhookDeliverySummary {
    pub id: String,
    pub provider: String,
    pub event_type: String,
    /// Queue the job was created in
    pub matched_queue: Option<String>,
    /// Job the request created or matched (`null` for entries before 0.1.122)
    pub job_id: Option<String>,
    /// `created` or `duplicate` (`null` for entries before 0.1.122)
    pub status: Option<String>,
    pub created_at: DateTime<Utc>,
}

impl From<WebhookDelivery> for WebhookDeliverySummary {
    fn from(delivery: WebhookDelivery) -> Self {
        Self {
            id: delivery.id,
            provider: delivery.provider,
            event_type: delivery.event_type,
            matched_queue: delivery.matched_queue,
            job_id: None,
            status: None,
            created_at: delivery.created_at,
        }
    }
}

/// `status` of an inbound request that created a new job.
pub const INBOUND_DELIVERY_CREATED: &str = "created";
/// `status` of an inbound request whose idempotency key matched an existing job.
pub const INBOUND_DELIVERY_DUPLICATE: &str = "duplicate";

/// Default and largest page of `GET /webhooks/deliveries`.
pub const INBOUND_DELIVERIES_DEFAULT_LIMIT: i64 = 50;
pub const INBOUND_DELIVERIES_MAX_LIMIT: i64 = 100;

/// Query of `GET /webhooks/deliveries`.
#[derive(Debug, Default, Deserialize)]
pub struct ListInboundDeliveriesQuery {
    /// Page size, 1-100 (default 50; out-of-range values are clamped)
    pub limit: Option<i64>,
    /// Entries to skip (default 0)
    pub offset: Option<i64>,
    /// `created` or `duplicate`
    pub status: Option<String>,
    /// RFC 3339 timestamp; only entries received at or after it
    pub since: Option<String>,
    /// Only entries whose job went to this queue (`queue` accepted as alias)
    #[serde(alias = "queue")]
    pub queue_name: Option<String>,
}

/// The checked filters of a [`ListInboundDeliveriesQuery`].
#[derive(Debug, Clone, PartialEq)]
pub struct InboundDeliveriesFilter {
    pub limit: i64,
    pub offset: i64,
    pub status: Option<&'static str>,
    pub since: Option<DateTime<Utc>>,
    pub queue_name: Option<String>,
}

impl ListInboundDeliveriesQuery {
    /// Clamp the page and check the filters. An unknown `status`, a `since`
    /// that is not RFC 3339 or an over-long queue name is an error (400), not
    /// a silently unfiltered page.
    pub fn filter(&self) -> Result<InboundDeliveriesFilter, String> {
        let status = match self.status.as_deref().map(str::trim) {
            None | Some("") => None,
            Some(s) if s.eq_ignore_ascii_case(INBOUND_DELIVERY_CREATED) => {
                Some(INBOUND_DELIVERY_CREATED)
            }
            Some(s) if s.eq_ignore_ascii_case(INBOUND_DELIVERY_DUPLICATE) => {
                Some(INBOUND_DELIVERY_DUPLICATE)
            }
            Some(other) => {
                return Err(format!(
                    "Invalid status '{}': expected {} or {}",
                    other, INBOUND_DELIVERY_CREATED, INBOUND_DELIVERY_DUPLICATE
                ))
            }
        };
        let since = match self.since.as_deref().map(str::trim) {
            None | Some("") => None,
            Some(s) => Some(
                DateTime::parse_from_rfc3339(s)
                    .map_err(|_| {
                        format!(
                            "Invalid since '{}': expected an RFC 3339 timestamp such as 2026-10-14T12:00:00Z",
                            s
                        )
                    })?
                    .with_timezone(&Utc),
            ),
        };
        let queue_name = match self.queue_name.as_deref().map(str::trim) {
            None | Some("") => None,
            Some(q) if q.len() > 255 => {
                return Err("queue_name must be at most 255 characters".to_string())
            }
            Some(q) => Some(q.to_string()),
        };
        Ok(InboundDeliveriesFilter {
            limit: self
                .limit
                .unwrap_or(INBOUND_DELIVERIES_DEFAULT_LIMIT)
                .clamp(1, INBOUND_DELIVERIES_MAX_LIMIT),
            offset: self.offset.unwrap_or(0).max(0),
            status,
            since,
            queue_name,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_webhook_provider_display() {
        assert_eq!(WebhookProvider::Custom.to_string(), "custom");
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
            format: "slack".to_string(),
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
        assert_eq!(summary.format, "slack");
        // Note: secret is not included in summary (skip_serializing)
    }

    #[test]
    fn new_event_types_are_valid() {
        for event in [
            "job.deadlettered",
            "job.retried",
            "workflow.completed",
            "workflow.failed",
        ] {
            assert!(VALID_WEBHOOK_EVENTS.contains(&event), "{event}");
        }
    }

    #[test]
    fn webhook_format_parses_and_validates() {
        assert_eq!(WebhookFormat::parse(" Slack "), Some(WebhookFormat::Slack));
        assert_eq!(WebhookFormat::parse("teams"), Some(WebhookFormat::Teams));
        assert_eq!(WebhookFormat::parse("xml"), None);
        assert_eq!(WebhookFormat::from_stored(""), WebhookFormat::Json);
        assert_eq!(
            WebhookFormat::from_stored("discord"),
            WebhookFormat::Discord
        );
        for f in VALID_WEBHOOK_FORMATS {
            assert_eq!(WebhookFormat::parse(f).unwrap().as_str(), *f);
        }
        assert_eq!(validate_webhook_format(None), Ok(None));
        assert_eq!(
            validate_webhook_format(Some("DISCORD")),
            Ok(Some("discord"))
        );
        let err = validate_webhook_format(Some("xml")).unwrap_err();
        assert!(err.contains("json, slack, discord, teams"), "{err}");
    }

    #[test]
    fn outgoing_webhook_requests_take_an_optional_format() {
        let create: CreateOutgoingWebhookRequest = serde_json::from_str(
            r#"{"name":"n","url":"https://example.com","events":["job.failed"],"format":"slack"}"#,
        )
        .unwrap();
        assert_eq!(create.format.as_deref(), Some("slack"));
        let create: CreateOutgoingWebhookRequest = serde_json::from_str(
            r#"{"name":"n","url":"https://example.com","events":["job.failed"]}"#,
        )
        .unwrap();
        assert_eq!(create.format, None);
        let update: UpdateOutgoingWebhookRequest =
            serde_json::from_str(r#"{"format":"teams"}"#).unwrap();
        assert_eq!(update.format.as_deref(), Some("teams"));
    }

    #[test]
    fn inbound_deliveries_query_clamps_and_rejects_bad_filters() {
        let f = ListInboundDeliveriesQuery::default().filter().unwrap();
        assert_eq!((f.limit, f.offset, f.status, f.since), (50, 0, None, None));

        let f = ListInboundDeliveriesQuery {
            limit: Some(5_000),
            offset: Some(-3),
            status: Some("Duplicate".into()),
            since: Some("2026-10-14T12:00:00+02:00".into()),
            queue_name: Some(" orders ".into()),
        }
        .filter()
        .unwrap();
        assert_eq!(f.limit, 100);
        assert_eq!(f.offset, 0);
        assert_eq!(f.status, Some("duplicate"));
        assert_eq!(f.since.unwrap().to_rfc3339(), "2026-10-14T10:00:00+00:00");
        assert_eq!(f.queue_name.as_deref(), Some("orders"));

        let bad_status = ListInboundDeliveriesQuery {
            status: Some("failed".into()),
            ..Default::default()
        };
        assert!(bad_status
            .filter()
            .unwrap_err()
            .contains("created or duplicate"));
        let bad_since = ListInboundDeliveriesQuery {
            since: Some("yesterday".into()),
            ..Default::default()
        };
        assert!(bad_since.filter().unwrap_err().contains("RFC 3339"));
        let zero = ListInboundDeliveriesQuery {
            limit: Some(0),
            ..Default::default()
        };
        assert_eq!(zero.filter().unwrap().limit, 1);
    }

    #[test]
    fn inbound_delivery_summary_never_carries_payload_or_signature() {
        let summary: WebhookDeliverySummary = WebhookDelivery {
            id: "d1".into(),
            organization_id: "o1".into(),
            provider: "custom".into(),
            event_type: "order.paid".into(),
            payload: serde_json::json!({"secret": "x"}),
            signature: Some("sig".into()),
            matched_queue: Some("orders".into()),
            created_at: Utc::now(),
        }
        .into();
        let json = serde_json::to_value(&summary).unwrap();
        let keys: Vec<&str> = json
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        for hidden in ["payload", "signature", "organization_id"] {
            assert!(!keys.contains(&hidden), "{keys:?}");
        }
        assert!(
            keys.contains(&"job_id") && keys.contains(&"status"),
            "{keys:?}"
        );
    }
}
