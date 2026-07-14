use anyhow::{Context, Result};
use chrono::Utc;
use hmac::{Hmac, KeyInit, Mac};
use reqwest::Client;
use sha2::Sha256;
use sqlx::{Pool, Postgres};
use std::sync::Arc;
use std::time::Duration;
use tracing::{error, info, warn};

use crate::models::OutgoingWebhook;
use crate::security::{
    build_outbound_http_client, validate_webhook_url as validate_url_ssrf, UrlValidationOptions,
};

#[derive(Clone)]
pub struct OutgoingWebhookService {
    pool: Pool<Postgres>,
    client: Client,
    max_attempts: i32,
}

impl OutgoingWebhookService {
    pub fn new(pool: Pool<Postgres>, max_attempts: i32) -> Self {
        Self {
            pool,
            // SECURITY: Never fall back to a default client. A default client may have no timeouts
            // and will follow redirects, increasing SSRF/DoS risk.
            client: build_outbound_http_client(
                Duration::from_secs(10),
                Duration::from_secs(5),
                "Spooled-OutgoingWebhook/1.0",
            ),
            max_attempts,
        }
    }

    /// Best-effort fire-and-forget dispatch used by REST/gRPC handlers.
    pub fn spawn_dispatch(
        self: &Arc<Self>,
        organization_id: String,
        event: String,
        resource_id: String,
        payload: serde_json::Value,
    ) {
        let service = Arc::clone(self);
        tokio::spawn(async move {
            if let Err(e) = service
                .dispatch_event(event, &resource_id, payload, &organization_id)
                .await
            {
                warn!(error = %e, org_id = %organization_id, "Outgoing webhook dispatch failed");
            }
        });
    }

    pub async fn dispatch_event(
        &self,
        event: String,
        resource_id: &str,
        payload: serde_json::Value,
        organization_id: &str,
    ) -> Result<()> {
        // Find matching webhooks
        let webhooks = sqlx::query_as::<_, OutgoingWebhook>(
            r#"
            SELECT * FROM outgoing_webhooks
            WHERE organization_id = $1
            AND enabled = true
            AND $2 = ANY(events)
            "#,
        )
        .bind(organization_id)
        .bind(event.clone())
        .fetch_all(&self.pool)
        .await
        .context("Failed to fetch matching webhooks")?;

        if webhooks.is_empty() {
            return Ok(());
        }

        info!(
            event = %event,
            resource_id,
            org_id = organization_id,
            count = webhooks.len(),
            "Dispatching webhook event"
        );

        for webhook in webhooks {
            let delivery_id = uuid::Uuid::new_v4();
            let payload = payload.clone();

            // Spawn a task for each delivery to not block the main request
            let service = self.clone();
            let event_clone = event.clone();
            tokio::spawn(async move {
                if let Err(e) = service
                    .deliver(webhook, event_clone, payload, delivery_id)
                    .await
                {
                    error!(error = %e, "Failed to deliver webhook");
                }
            });
        }

        Ok(())
    }

    pub async fn deliver(
        &self,
        webhook: OutgoingWebhook,
        event: String,
        payload: serde_json::Value,
        delivery_id: uuid::Uuid,
    ) -> Result<()> {
        let body = serde_json::json!({
            "id": delivery_id,
            "event": event,
            "created_at": Utc::now(),
            "data": payload,
        });

        self.try_deliver(&webhook, &body, 1).await
    }

    pub async fn retry_delivery(&self, delivery_id: uuid::Uuid) -> Result<()> {
        // Fetch delivery details using runtime query (no compile-time check)
        let row = sqlx::query(
            r#"
            SELECT d.webhook_id, d.event, d.status, d.payload, d.attempts, d.created_at,
                   w.url, w.secret 
            FROM outgoing_webhook_deliveries d
            JOIN outgoing_webhooks w ON d.webhook_id = w.id
            WHERE d.id = $1
            "#,
        )
        .bind(delivery_id.to_string())
        .fetch_optional(&self.pool)
        .await?
        .ok_or_else(|| anyhow::anyhow!("Delivery not found"))?;

        use sqlx::Row;
        let status: String = row.get("status");
        if status == "success" {
            return Ok(());
        }

        let webhook = OutgoingWebhook {
            id: row.get("webhook_id"),
            organization_id: String::new(), // Not needed for delivery
            url: row.get("url"),
            secret: row.get("secret"),
            events: vec![], // Not needed
            enabled: true,
            failure_count: 0,
            last_triggered_at: None,
            last_status: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
            name: String::new(),
        };

        let payload: serde_json::Value = row.get("payload");
        let event: String = row.get("event");
        let created_at: chrono::DateTime<Utc> = row.get("created_at");
        let attempts: i32 = row.get("attempts");

        // Deliveries store the full signed envelope (`id`/`event`/`data`). Older
        // rows may only have the inner `data` object — wrap those for retry.
        let body = if payload.get("event").is_some() && payload.get("data").is_some() {
            let mut envelope = payload;
            envelope["id"] = serde_json::json!(delivery_id);
            envelope
        } else {
            serde_json::json!({
                "id": delivery_id,
                "event": event,
                "created_at": created_at,
                "data": payload,
            })
        };

        self.try_deliver(&webhook, &body, attempts + 1).await
    }

    async fn try_deliver(
        &self,
        webhook: &OutgoingWebhook,
        body: &serde_json::Value,
        attempt: i32,
    ) -> Result<()> {
        let start = Utc::now();

        // Validate URL prevents SSRF
        if let Err(e) = self.validate_url(&webhook.url) {
            self.record_attempt(&webhook.id, body, 0, Some(&e.to_string()), attempt, start)
                .await?;
            return Err(e);
        }

        let payload_json = serde_json::to_string(body).unwrap_or_default();
        let timestamp = Utc::now().timestamp();
        let event = body["event"].as_str().unwrap_or_default();

        let mut request_builder = self
            .client
            .post(&webhook.url)
            .header("Content-Type", "application/json")
            .header("X-Spooled-Event", event)
            .header("X-Spooled-Timestamp", timestamp.to_string())
            .header("X-Spooled-Delivery-Attempt", attempt.to_string());

        // Prefer the per-webhook secret (matches /outgoing-webhooks/{id}/test).
        if let Some(ref secret) = webhook.secret {
            if !secret.is_empty() {
                let signature = sign_payload(secret, timestamp, &payload_json)?;
                request_builder = request_builder.header("X-Spooled-Signature", signature);
            }
        }

        let response = request_builder.body(payload_json).send().await;

        match response {
            Ok(res) => {
                let status = res.status().as_u16();
                let success = res.status().is_success();
                let error = if !success {
                    Some(format!("HTTP {}", status))
                } else {
                    None
                };

                self.record_attempt(
                    &webhook.id,
                    body,
                    status as i32,
                    error.as_deref(),
                    attempt,
                    start,
                )
                .await?;

                if !success && attempt < self.max_attempts {
                    warn!(
                        "Webhook delivery failed (attempt {}/{}), should retry",
                        attempt, self.max_attempts
                    );
                }
            }
            Err(e) => {
                self.record_attempt(&webhook.id, body, 0, Some(&e.to_string()), attempt, start)
                    .await?;
                if attempt < self.max_attempts {
                    warn!(
                        "Webhook delivery error (attempt {}/{}), should retry: {}",
                        attempt, self.max_attempts, e
                    );
                }
            }
        }

        Ok(())
    }

    async fn record_attempt(
        &self,
        webhook_id: &str,
        request_payload: &serde_json::Value,
        response_status: i32,
        response_body: Option<&str>,
        attempt_count: i32,
        started_at: chrono::DateTime<Utc>,
    ) -> Result<()> {
        let status = if (200..300).contains(&response_status) {
            "success"
        } else {
            "failed"
        };

        let delivery_id = request_payload
            .get("id")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());

        sqlx::query(
            r#"
            INSERT INTO outgoing_webhook_deliveries (
                id, webhook_id, event, status, payload,
                status_code, response_body, attempts,
                created_at, delivered_at
            )
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)
            ON CONFLICT (id) DO UPDATE SET
                status = EXCLUDED.status,
                status_code = EXCLUDED.status_code,
                response_body = EXCLUDED.response_body,
                attempts = EXCLUDED.attempts,
                delivered_at = EXCLUDED.delivered_at
            "#,
        )
        .bind(&delivery_id)
        .bind(webhook_id)
        .bind(request_payload["event"].as_str().unwrap_or("unknown"))
        .bind(status)
        .bind(request_payload)
        .bind(response_status)
        .bind(response_body)
        .bind(attempt_count)
        .bind(started_at)
        .bind(Utc::now())
        .execute(&self.pool)
        .await?;

        // Keep webhook summary fields current for dashboard / list views.
        sqlx::query(
            r#"
            UPDATE outgoing_webhooks
            SET last_triggered_at = NOW(),
                last_status = $2,
                failure_count = CASE
                    WHEN $2 = 'success' THEN 0
                    ELSE failure_count + 1
                END,
                updated_at = NOW()
            WHERE id = $1
            "#,
        )
        .bind(webhook_id)
        .bind(status)
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    /// Validate webhook URL to prevent SSRF attacks
    ///
    /// Uses the centralized URL validator from the security module.
    fn validate_url(&self, url: &str) -> Result<()> {
        let options = UrlValidationOptions::default();
        validate_url_ssrf(url, &options).map_err(|e| {
            warn!(url = %url, error = %e, "Webhook URL validation failed (SSRF protection)");
            anyhow::anyhow!("Invalid webhook URL: {}", e)
        })
    }
}

fn sign_payload(secret: &str, timestamp: i64, payload_json: &str) -> Result<String> {
    type HmacSha256 = Hmac<Sha256>;
    let message = format!("{}.{}", timestamp, payload_json);
    let mut mac =
        HmacSha256::new_from_slice(secret.as_bytes()).context("HMAC can take key of any size")?;
    mac.update(message.as_bytes());
    Ok(format!(
        "sha256={}",
        hex::encode(mac.finalize().into_bytes())
    ))
}

/// Deliver a per-job `completion_webhook` URL (best-effort, fire-and-forget).
pub fn spawn_completion_webhook(url: String, event: String, payload: serde_json::Value) {
    tokio::spawn(async move {
        if let Err(e) = deliver_completion_webhook(&url, &event, &payload).await {
            warn!(error = %e, url = %url, event = %event, "completion_webhook delivery failed");
        }
    });
}

async fn deliver_completion_webhook(
    url: &str,
    event: &str,
    payload: &serde_json::Value,
) -> Result<()> {
    let options = UrlValidationOptions::default();
    validate_url_ssrf(url, &options).map_err(|e| anyhow::anyhow!("Invalid webhook URL: {}", e))?;

    let client = build_outbound_http_client(
        Duration::from_secs(10),
        Duration::from_secs(5),
        "Spooled-CompletionWebhook/1.0",
    );
    let body = serde_json::json!({
        "event": event,
        "created_at": Utc::now(),
        "data": payload,
    });
    let resp = client
        .post(url)
        .header("Content-Type", "application/json")
        .header("X-Spooled-Event", event)
        .json(&body)
        .send()
        .await?;
    if !resp.status().is_success() {
        anyhow::bail!("HTTP {}", resp.status());
    }
    Ok(())
}
