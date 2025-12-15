use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use chrono::Utc;
use reqwest::Client;
use sqlx::PgPool;
use tracing::{debug, error, info, warn};

use crate::models::{OutgoingWebhook, OutgoingWebhookDelivery};

const MAX_RESPONSE_BODY_SIZE: usize = 500;
const DEFAULT_HTTP_TIMEOUT_SECS: u64 = 10;

#[derive(Clone)]
pub struct OutgoingWebhookService {
    db: Arc<PgPool>,
    http: Client,
}

impl OutgoingWebhookService {
    pub fn new(db: Arc<PgPool>) -> Self {
        let http = Client::builder()
            .timeout(Duration::from_secs(DEFAULT_HTTP_TIMEOUT_SECS))
            .connect_timeout(Duration::from_secs(5))
            .user_agent("Spooled-OutgoingWebhook/1.0")
            .build()
            .expect("Failed to create reqwest client");

        Self { db, http }
    }

    /// Fire-and-forget dispatch for a given org/event.
    ///
    /// This records a delivery row for each matching webhook and spawns the actual HTTP delivery.
    pub async fn dispatch_event(
        &self,
        organization_id: &str,
        event: &str,
        payload: serde_json::Value,
    ) -> Result<()> {
        // Find enabled webhooks that subscribe to this event
        let webhooks: Vec<OutgoingWebhook> = sqlx::query_as(
            r#"
            SELECT id, organization_id, name, url, secret, events, enabled,
                   failure_count, last_triggered_at, last_status, created_at, updated_at
            FROM outgoing_webhooks
            WHERE organization_id = $1
              AND enabled = TRUE
              AND $2 = ANY(events)
            "#,
        )
        .bind(organization_id)
        .bind(event)
        .fetch_all(&*self.db)
        .await?;

        if webhooks.is_empty() {
            debug!(org_id = %organization_id, event = %event, "No outgoing webhooks subscribed");
            return Ok(());
        }

        for webhook in webhooks {
            let delivery_id = self
                .create_delivery(&webhook.id, event, payload.clone())
                .await?;

            let svc = self.clone();
            tokio::spawn(async move {
                if let Err(e) = svc.deliver_by_ids(&webhook.id, &delivery_id).await {
                    error!(
                        error = %e,
                        webhook_id = %webhook.id,
                        delivery_id = %delivery_id,
                        "Outgoing webhook delivery task failed"
                    );
                }
            });
        }

        Ok(())
    }

    pub async fn create_delivery(
        &self,
        webhook_id: &str,
        event: &str,
        payload: serde_json::Value,
    ) -> Result<String> {
        let id: (String,) = sqlx::query_as(
            r#"
            INSERT INTO outgoing_webhook_deliveries (
                id, webhook_id, event, payload, status, attempts, created_at
            )
            VALUES (gen_random_uuid()::TEXT, $1, $2, $3::JSONB, 'pending', 0, NOW())
            RETURNING id
            "#,
        )
        .bind(webhook_id)
        .bind(event)
        .bind(payload)
        .fetch_one(&*self.db)
        .await?;

        Ok(id.0)
    }

    pub async fn deliver_by_ids(&self, webhook_id: &str, delivery_id: &str) -> Result<()> {
        let webhook: OutgoingWebhook = sqlx::query_as(
            r#"
            SELECT id, organization_id, name, url, secret, events, enabled,
                   failure_count, last_triggered_at, last_status, created_at, updated_at
            FROM outgoing_webhooks
            WHERE id = $1
            "#,
        )
        .bind(webhook_id)
        .fetch_one(&*self.db)
        .await?;

        let delivery: OutgoingWebhookDelivery = sqlx::query_as(
            r#"
            SELECT id, webhook_id, event, payload, status, status_code,
                   response_body, error, attempts, created_at, delivered_at
            FROM outgoing_webhook_deliveries
            WHERE id = $1 AND webhook_id = $2
            "#,
        )
        .bind(delivery_id)
        .bind(webhook_id)
        .fetch_one(&*self.db)
        .await?;

        self.deliver(&webhook, &delivery).await
    }

    pub async fn deliver(&self, webhook: &OutgoingWebhook, delivery: &OutgoingWebhookDelivery) -> Result<()> {
        let timestamp = Utc::now().timestamp();
        let payload_json = serde_json::to_string(&delivery.payload).unwrap_or_else(|_| "{}".to_string());

        let attempt = delivery.attempts + 1;

        let mut req = self
            .http
            .post(&webhook.url)
            .header("Content-Type", "application/json")
            .header("X-Spooled-Event", &delivery.event)
            .header("X-Spooled-Timestamp", timestamp.to_string())
            .header("X-Spooled-Delivery-Attempt", attempt.to_string());

        if let Some(ref secret) = webhook.secret {
            let signature = sign_payload(secret, timestamp, &payload_json)?;
            req = req.header("X-Spooled-Signature", signature);
        }

        let start = std::time::Instant::now();
        let resp = req.body(payload_json).send().await;
        let duration_ms = start.elapsed().as_millis() as i64;

        match resp {
            Ok(r) => {
                let status = r.status();
                let success = status.is_success();
                let body = r.text().await.ok().map(|b| truncate_sanitize(&b));

                self.update_delivery_attempt(
                    &delivery.id,
                    attempt,
                    Some(status.as_u16() as i32),
                    body.clone(),
                    if success { None } else { Some(format!("HTTP {}", status.as_u16())) },
                    success,
                )
                .await?;

                self.update_webhook_status(&webhook.id, success).await?;

                if success {
                    info!(
                        webhook_id = %webhook.id,
                        delivery_id = %delivery.id,
                        status = %status.as_u16(),
                        duration_ms = duration_ms,
                        "Outgoing webhook delivered"
                    );
                } else {
                    warn!(
                        webhook_id = %webhook.id,
                        delivery_id = %delivery.id,
                        status = %status.as_u16(),
                        duration_ms = duration_ms,
                        "Outgoing webhook delivery failed"
                    );
                }
            }
            Err(e) => {
                warn!(
                    webhook_id = %webhook.id,
                    delivery_id = %delivery.id,
                    error = %e,
                    duration_ms = duration_ms,
                    "Outgoing webhook request error"
                );

                self.update_delivery_attempt(
                    &delivery.id,
                    attempt,
                    None,
                    None,
                    Some("Request failed".to_string()),
                    false,
                )
                .await?;

                self.update_webhook_status(&webhook.id, false).await?;
            }
        }

        Ok(())
    }

    async fn update_delivery_attempt(
        &self,
        delivery_id: &str,
        attempt: i32,
        status_code: Option<i32>,
        response_body: Option<String>,
        error_msg: Option<String>,
        success: bool,
    ) -> Result<()> {
        sqlx::query(
            r#"
            UPDATE outgoing_webhook_deliveries
            SET
                status = $2,
                status_code = $3,
                response_body = $4,
                error = $5,
                attempts = $6,
                delivered_at = CASE WHEN $2 = 'success' THEN NOW() ELSE delivered_at END
            WHERE id = $1
            "#,
        )
        .bind(delivery_id)
        .bind(if success { "success" } else { "failed" })
        .bind(status_code)
        .bind(response_body)
        .bind(error_msg)
        .bind(attempt)
        .execute(&*self.db)
        .await?;

        Ok(())
    }

    async fn update_webhook_status(&self, webhook_id: &str, success: bool) -> Result<()> {
        if success {
            sqlx::query(
                r#"
                UPDATE outgoing_webhooks
                SET last_triggered_at = NOW(),
                    last_status = 'success',
                    failure_count = 0,
                    updated_at = NOW()
                WHERE id = $1
                "#,
            )
            .bind(webhook_id)
            .execute(&*self.db)
            .await?;
        } else {
            sqlx::query(
                r#"
                UPDATE outgoing_webhooks
                SET last_triggered_at = NOW(),
                    last_status = 'failed',
                    failure_count = failure_count + 1,
                    updated_at = NOW()
                WHERE id = $1
                "#,
            )
            .bind(webhook_id)
            .execute(&*self.db)
            .await?;
        }
        Ok(())
    }

    /// Retry an existing delivery (re-sends the stored payload).
    pub async fn retry_delivery(&self, webhook_id: &str, delivery_id: &str) -> Result<()> {
        // Mark as pending (for visibility) then deliver
        sqlx::query(
            r#"
            UPDATE outgoing_webhook_deliveries
            SET status = 'pending', error = NULL
            WHERE id = $1 AND webhook_id = $2
            "#,
        )
        .bind(delivery_id)
        .bind(webhook_id)
        .execute(&*self.db)
        .await?;

        self.deliver_by_ids(webhook_id, delivery_id).await
    }
}

fn truncate_sanitize(body: &str) -> String {
    body.chars()
        .take(MAX_RESPONSE_BODY_SIZE)
        .filter(|c| !c.is_control() || c.is_whitespace())
        .collect()
}

fn sign_payload(secret: &str, timestamp: i64, payload_json: &str) -> Result<String> {
    use hmac::{Hmac, Mac};
    use sha2::Sha256;
    type HmacSha256 = Hmac<Sha256>;

    let message = format!("{}.{}", timestamp, payload_json);
    let mut mac = HmacSha256::new_from_slice(secret.as_bytes())
        .map_err(|_| anyhow::anyhow!("Invalid HMAC key"))?;
    mac.update(message.as_bytes());
    Ok(format!("sha256={}", hex::encode(mac.finalize().into_bytes())))
}


