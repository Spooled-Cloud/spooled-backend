//! Webhook delivery service for Spooled Backend
//!
//! This module handles outgoing webhook delivery with:
//! - Automatic retries with exponential backoff
//! - Delivery tracking and logging
//! - Timeout handling
//! - Signature generation for authenticity

use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use chrono::Utc;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use sqlx::PgPool;
use tracing::{error, info, warn};

use crate::models::Job;

/// Minimum signing secret length for security
const MIN_SIGNING_SECRET_LENGTH: usize = 32;

/// Maximum response body size to store (for safety)
/// Response bodies could contain sensitive data
const MAX_RESPONSE_BODY_SIZE: usize = 500;

/// Default max delivery attempts
/// Made configurable instead of hardcoded
const DEFAULT_MAX_ATTEMPTS: i32 = 5;

/// Webhook delivery service
pub struct WebhookService {
    client: Client,
    db: Arc<PgPool>,
    /// Secret key for signing webhook payloads
    signing_secret: String,
    /// Maximum delivery attempts (configurable)
    /// Made configurable
    max_attempts: i32,
}

/// Webhook delivery request
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WebhookDeliveryRequest {
    /// Target URL
    pub url: String,
    /// Event type (e.g., "job.completed", "job.failed")
    pub event_type: String,
    /// Payload to send
    pub payload: serde_json::Value,
    /// Organization ID
    pub organization_id: String,
    /// Related job ID (if any)
    pub job_id: Option<String>,
}

/// Result of a webhook delivery attempt
#[derive(Debug, Clone)]
pub struct DeliveryResult {
    /// Whether delivery succeeded
    pub success: bool,
    /// HTTP status code (if request was made)
    pub status_code: Option<u16>,
    /// Response body (truncated)
    pub response_body: Option<String>,
    /// Error message (if failed)
    pub error: Option<String>,
    /// Delivery attempt number
    pub attempt: i32,
    /// Duration of the request
    pub duration_ms: i64,
}

impl WebhookService {
    /// Create a new webhook service
    ///
    /// Validates signing_secret has minimum length
    /// Accepts configurable max_attempts
    pub fn new(db: Arc<PgPool>, signing_secret: String) -> Self {
        Self::new_with_options(db, signing_secret, DEFAULT_MAX_ATTEMPTS)
    }

    /// Create a new webhook service with configurable options
    pub fn new_with_options(db: Arc<PgPool>, signing_secret: String, max_attempts: i32) -> Self {
        // Warn if signing secret is too short
        if signing_secret.len() < MIN_SIGNING_SECRET_LENGTH {
            warn!(
                secret_len = signing_secret.len(),
                min_len = MIN_SIGNING_SECRET_LENGTH,
                "Webhook signing secret is shorter than recommended minimum"
            );
        }

        let client = Client::builder()
            .timeout(Duration::from_secs(30))
            .connect_timeout(Duration::from_secs(10))
            .user_agent("Spooled-Webhook/1.0")
            .build()
            .expect("Failed to create HTTP client");

        // Clamp max_attempts to reasonable range
        let safe_max_attempts = max_attempts.clamp(1, 10);

        Self {
            client,
            db,
            signing_secret,
            max_attempts: safe_max_attempts,
        }
    }

    /// Deliver a webhook with automatic retries
    ///
    /// Uses configurable max_attempts instead of hardcoded 5
    pub async fn deliver(&self, request: WebhookDeliveryRequest) -> Result<DeliveryResult> {
        let max_attempts = self.max_attempts;
        let mut last_result = None;

        for attempt in 1..=max_attempts {
            let result = self.try_deliver(&request, attempt).await;

            // Record the attempt
            self.record_attempt(&request, &result).await?;

            if result.success {
                info!(
                    url = %request.url,
                    event = %request.event_type,
                    attempt = attempt,
                    "Webhook delivered successfully"
                );
                return Ok(result);
            }

            last_result = Some(result.clone());

            if attempt < max_attempts {
                // Exponential backoff: 1s, 2s, 4s, 8s
                let backoff = Duration::from_secs(2_u64.pow(attempt as u32 - 1));
                warn!(
                    url = %request.url,
                    event = %request.event_type,
                    attempt = attempt,
                    backoff_secs = backoff.as_secs(),
                    error = ?result.error,
                    "Webhook delivery failed, retrying"
                );
                tokio::time::sleep(backoff).await;
            }
        }

        let final_result = last_result.unwrap_or(DeliveryResult {
            success: false,
            status_code: None,
            response_body: None,
            error: Some("Max retries exceeded".to_string()),
            attempt: max_attempts,
            duration_ms: 0,
        });

        error!(
            url = %request.url,
            event = %request.event_type,
            "Webhook delivery failed after {} attempts",
            max_attempts
        );

        Ok(final_result)
    }

    /// Validate webhook URL to prevent SSRF attacks
    ///
    /// Prevents requests to internal/private networks
    /// Now blocks localhost HTTP in production mode
    /// Returns generic error to avoid URL leakage in response
    fn validate_url(&self, url: &str) -> Result<(), String> {
        let parsed = url::Url::parse(url).map_err(|e| format!("Invalid URL: {}", e))?;

        // Check if we're in production mode (via environment variable)
        let is_production = std::env::var("RUST_ENV")
            .map(|v| v == "production")
            .unwrap_or(false);

        // Only allow HTTPS (or HTTP for localhost in dev ONLY)
        match parsed.scheme() {
            "https" => {}
            "http" => {
                // Block HTTP entirely in production
                if is_production {
                    return Err("HTTP webhooks not allowed in production - use HTTPS".to_string());
                }

                // Allow HTTP only for localhost in development
                if let Some(host) = parsed.host_str() {
                    if host != "localhost" && host != "127.0.0.1" {
                        return Err("HTTP only allowed for localhost in development".to_string());
                    }
                }
            }
            _ => return Err("Only HTTPS URLs are allowed".to_string()),
        }

        // Block internal/private IPs
        if let Some(host) = parsed.host_str() {
            // Block common internal hostnames
            let blocked_hosts = [
                "localhost",
                "127.0.0.1",
                "::1",
                "0.0.0.0",
                "metadata",
                "metadata.google",
                "169.254.169.254", // AWS/GCP metadata
                "metadata.google.internal",
            ];

            // Allow localhost only in non-production (checked above for HTTP)
            if parsed.scheme() == "https" && blocked_hosts.contains(&host) {
                return Err(format!("Blocked host: {}", host));
            }

            // Block private IP ranges
            if let Ok(ip) = host.parse::<std::net::IpAddr>() {
                if ip.is_loopback() || ip.is_unspecified() {
                    return Err("Loopback addresses not allowed".to_string());
                }
                // Check for private ranges
                match ip {
                    std::net::IpAddr::V4(ipv4) => {
                        if ipv4.is_private() || ipv4.is_link_local() {
                            return Err("Private IP addresses not allowed".to_string());
                        }
                    }
                    std::net::IpAddr::V6(ipv6) => {
                        // IPv6 loopback already checked above
                        // Check for link-local, unique local, etc.
                        let segments = ipv6.segments();
                        if segments[0] == 0xfe80 || segments[0] == 0xfc00 || segments[0] == 0xfd00 {
                            return Err("Private IPv6 addresses not allowed".to_string());
                        }
                    }
                }
            }
        }

        Ok(())
    }

    /// Try to deliver a webhook once
    async fn try_deliver(&self, request: &WebhookDeliveryRequest, attempt: i32) -> DeliveryResult {
        // Validate URL before making request
        // Don't expose URL validation details in error message
        if let Err(e) = self.validate_url(&request.url) {
            warn!(error = %e, "Webhook URL validation failed");
            return DeliveryResult {
                success: false,
                status_code: None,
                response_body: None,
                error: Some("Invalid webhook URL".to_string()), // Generic error
                attempt,
                duration_ms: 0,
            };
        }

        let start = std::time::Instant::now();
        let payload_json = serde_json::to_string(&request.payload).unwrap_or_default();

        // Generate signature
        let timestamp = Utc::now().timestamp();
        let signature = self.generate_signature(&payload_json, timestamp);

        let result = self
            .client
            .post(&request.url)
            .header("Content-Type", "application/json")
            .header("X-Spooled-Event", &request.event_type)
            .header("X-Spooled-Timestamp", timestamp.to_string())
            .header("X-Spooled-Signature", &signature)
            .header("X-Spooled-Delivery-Attempt", attempt.to_string())
            .body(payload_json)
            .send()
            .await;

        let duration_ms = start.elapsed().as_millis() as i64;

        match result {
            Ok(response) => {
                let status = response.status();
                let success = status.is_success();
                // Sanitize response body - remove potential sensitive data
                let body = response.text().await.ok().map(|b| {
                    // Truncate and sanitize - remove control characters
                    b.chars()
                        .take(MAX_RESPONSE_BODY_SIZE)
                        .filter(|c| !c.is_control() || c.is_whitespace())
                        .collect()
                });

                DeliveryResult {
                    success,
                    status_code: Some(status.as_u16()),
                    response_body: body,
                    error: if success {
                        None
                    } else {
                        Some(format!("HTTP {}", status))
                    },
                    attempt,
                    duration_ms,
                }
            }
            Err(e) => {
                // Don't expose raw reqwest error (may contain URLs, IPs, etc.)
                tracing::warn!(error = %e, "Webhook delivery request failed");
                DeliveryResult {
                    success: false,
                    status_code: None,
                    response_body: None,
                    // Use generic error message
                    error: Some("Request failed".to_string()),
                    attempt,
                    duration_ms,
                }
            }
        }
    }

    /// Record a delivery attempt in the database
    async fn record_attempt(
        &self,
        request: &WebhookDeliveryRequest,
        result: &DeliveryResult,
    ) -> Result<()> {
        sqlx::query(
            r#"
            INSERT INTO webhook_deliveries (
                id, organization_id, provider, event_type, signature,
                queue_name, payload, response_code, response_body,
                created_at
            )
            VALUES (
                gen_random_uuid()::TEXT, $1, 'outgoing', $2, $3,
                $4, $5, $6, $7, NOW()
            )
            "#,
        )
        .bind(&request.organization_id)
        .bind(&request.event_type)
        .bind(format!("attempt:{}", result.attempt))
        .bind(request.job_id.as_ref().map(|_| "completion"))
        .bind(&request.payload)
        .bind(result.status_code.map(|s| s as i32))
        .bind(&result.response_body)
        .execute(&*self.db)
        .await?;

        Ok(())
    }

    /// Generate HMAC-SHA256 signature for webhook payload
    fn generate_signature(&self, payload: &str, timestamp: i64) -> String {
        use hmac::{Hmac, Mac};
        type HmacSha256 = Hmac<Sha256>;

        let message = format!("{}.{}", timestamp, payload);
        let mut mac = HmacSha256::new_from_slice(self.signing_secret.as_bytes()).expect("HMAC key");
        mac.update(message.as_bytes());
        let result = mac.finalize();
        format!("sha256={}", hex::encode(result.into_bytes()))
    }

    /// Send job completion webhook
    pub async fn send_job_completion(&self, job: &Job) -> Result<Option<DeliveryResult>> {
        let Some(ref webhook_url) = job.completion_webhook else {
            return Ok(None);
        };

        let event_type = match job.status.as_str() {
            "completed" => "job.completed",
            "failed" => "job.failed",
            "deadletter" => "job.deadlettered",
            _ => "job.status_changed",
        };

        let payload = serde_json::json!({
            "event": event_type,
            "job_id": job.id,
            "queue_name": job.queue_name,
            "status": job.status,
            "result": job.result,
            "error": job.last_error,
            "retry_count": job.retry_count,
            "created_at": job.created_at,
            "completed_at": job.completed_at,
        });

        let request = WebhookDeliveryRequest {
            url: webhook_url.clone(),
            event_type: event_type.to_string(),
            payload,
            organization_id: job.organization_id.clone(),
            job_id: Some(job.id.clone()),
        };

        let result = self.deliver(request).await?;
        Ok(Some(result))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_signature_generation() {
        // Create a mock service (we can't actually create one without a DB pool)
        // Instead, test the signature algorithm directly

        use hmac::{Hmac, Mac};
        type HmacSha256 = Hmac<Sha256>;

        let secret = "test-secret";
        let payload = r#"{"test": "data"}"#;
        let timestamp = 1234567890_i64;

        let message = format!("{}.{}", timestamp, payload);
        let mut mac = HmacSha256::new_from_slice(secret.as_bytes()).expect("HMAC key");
        mac.update(message.as_bytes());
        let result = mac.finalize();
        let signature = format!("sha256={}", hex::encode(result.into_bytes()));

        assert!(signature.starts_with("sha256="));
        assert_eq!(signature.len(), 71); // "sha256=" + 64 hex chars
    }

    #[test]
    fn test_delivery_result_serialization() {
        let result = DeliveryResult {
            success: true,
            status_code: Some(200),
            response_body: Some("OK".to_string()),
            error: None,
            attempt: 1,
            duration_ms: 150,
        };

        assert!(result.success);
        assert_eq!(result.status_code, Some(200));
    }
}
