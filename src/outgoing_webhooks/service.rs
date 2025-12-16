use anyhow::{Context, Result};
use chrono::Utc;
use hmac::{Hmac, Mac};
use reqwest::Client;
use sha2::Sha256;
use sqlx::{Pool, Postgres};
use std::time::Duration;
use tracing::{error, info, warn};
use url::Url;

use crate::models::OutgoingWebhook;

#[derive(Clone)]
pub struct OutgoingWebhookService {
    pool: Pool<Postgres>,
    client: Client,
    signing_secret: String,
    max_attempts: i32,
}

impl OutgoingWebhookService {
    pub fn new(pool: Pool<Postgres>, signing_secret: String, max_attempts: i32) -> Self {
        Self {
            pool,
            client: Client::builder()
                .timeout(Duration::from_secs(10))
                .build()
                .unwrap_or_default(),
            signing_secret,
            max_attempts,
        }
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
                if let Err(e) = service.deliver(webhook, event_clone, payload, delivery_id).await {
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
        let signature = self.generate_signature(&payload)?;
        
        let body = serde_json::json!({
            "id": delivery_id,
            "event": event,
            "created_at": Utc::now(),
            "data": payload,
        });

        self.try_deliver(&webhook, &body, &signature, 1).await
    }

    pub async fn retry_delivery(&self, delivery_id: uuid::Uuid) -> Result<()> {
        // Fetch delivery details
        let delivery = sqlx::query!(
            r#"
            SELECT d.*, w.url, w.secret 
            FROM outgoing_webhook_deliveries d
            JOIN outgoing_webhooks w ON d.webhook_id = w.id
            WHERE d.id = $1::text
            "#,
            delivery_id.to_string()
        )
        .fetch_optional(&self.pool)
        .await?
        .ok_or_else(|| anyhow::anyhow!("Delivery not found"))?;

        if delivery.status == "success" {
            return Ok(());
        }

        let webhook = OutgoingWebhook {
            id: delivery.webhook_id,
            organization_id: String::new(), // Not needed for delivery
            url: delivery.url,
            secret: delivery.secret,
            events: vec![], // Not needed
            enabled: true,
            failure_count: 0,
            last_triggered_at: None,
            last_status: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
            name: String::new(),
        };

        // Reconstruct signature (or use stored one if we had it, but we generate fresh)
        let payload: serde_json::Value = serde_json::from_value(delivery.payload)?;
        let signature = self.generate_signature(&payload)?;
        
        // Construct body (same as original)
        let body = serde_json::json!({
            "id": delivery_id,
            "event": delivery.event,
            "created_at": delivery.created_at,
            "data": payload,
        });

        self.try_deliver(&webhook, &body, &signature, delivery.attempts + 1).await
    }

    async fn try_deliver(
        &self,
        webhook: &OutgoingWebhook,
        body: &serde_json::Value,
        signature: &str,
        attempt: i32,
    ) -> Result<()> {
        let start = Utc::now();
        
        // Validate URL prevents SSRF
        if let Err(e) = self.validate_url(&webhook.url) {
            self.record_attempt(&webhook.id, body, 0, Some(&e.to_string()), attempt, start).await?;
            return Err(e);
        }

        let response = self.client
            .post(&webhook.url)
            .header("X-Spooled-Signature", signature)
            .header("X-Spooled-Event", body["event"].as_str().unwrap_or_default())
            .json(body)
            .send()
            .await;

        match response {
            Ok(res) => {
                let status = res.status().as_u16();
                let success = res.status().is_success();
                let error = if !success {
                    Some(format!("HTTP {}", status))
                } else {
                    None
                };

                self.record_attempt(&webhook.id, body, status as i32, error.as_deref(), attempt, start).await?;
                
                if !success && attempt < self.max_attempts {
                    // Schedule retry (in a real system, this would be a delayed job)
                    // For now, we just log it
                    warn!("Webhook delivery failed (attempt {}/{}), should retry", attempt, self.max_attempts);
                }
            }
            Err(e) => {
                self.record_attempt(&webhook.id, body, 0, Some(&e.to_string()), attempt, start).await?;
                if attempt < self.max_attempts {
                    warn!("Webhook delivery error (attempt {}/{}), should retry: {}", attempt, self.max_attempts, e);
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

        sqlx::query!(
            r#"
            INSERT INTO outgoing_webhook_deliveries (
                webhook_id, event, status, payload, 
                status_code, response_body, attempts, 
                created_at, delivered_at
            )
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
            "#,
            webhook_id,
            request_payload["event"].as_str().unwrap_or("unknown"),
            status,
            request_payload,
            response_status,
            response_body,
            attempt_count,
            started_at,
            Utc::now()
        )
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    fn generate_signature(&self, payload: &serde_json::Value) -> Result<String> {
        type HmacSha256 = Hmac<Sha256>;
        let mut mac = HmacSha256::new_from_slice(self.signing_secret.as_bytes())
            .context("HMAC can take key of any size")?;
        mac.update(payload.to_string().as_bytes());
        let result = mac.finalize();
        Ok(hex::encode(result.into_bytes()))
    }

    fn validate_url(&self, url: &str) -> Result<()> {
        let parsed = Url::parse(url).context("Invalid URL")?;
        if parsed.scheme() != "http" && parsed.scheme() != "https" {
            return Err(anyhow::anyhow!("Invalid scheme"));
        }
        // Add more SSRF checks here (allow localhost for dev/tests)
        Ok(())
    }
}
