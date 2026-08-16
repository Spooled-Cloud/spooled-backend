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

/// Consecutive failed deliveries after which a webhook is auto-disabled.
///
/// Without this, a webhook pointing at a permanently dead host is retried at full
/// rate forever: `failure_count` was recorded but never read by anything.
pub const AUTO_DISABLE_FAILURE_THRESHOLD: i32 = 20;

/// `last_status` written when the breaker trips, so the user can tell an
/// auto-disable apart from a manual one.
pub const AUTO_DISABLED_STATUS: &str = "auto_disabled";

#[derive(Clone)]
pub struct OutgoingWebhookService {
    pool: Pool<Postgres>,
    client: Client,
    max_attempts: i32,
    /// Caps deliveries in flight across the whole process.
    ///
    /// Delivery tasks are spawned from the hot enqueue/complete paths and each
    /// one can live for tens of seconds (request timeout plus retry backoff)
    /// while holding a clone of the job payload. Unbounded, a single tenant
    /// pointing a webhook at a black-holing endpoint grows tasks, sockets and
    /// resident memory without limit until the process dies. This is the
    /// backpressure that turns that into "deliveries queue up" instead.
    delivery_slots: Arc<tokio::sync::Semaphore>,
    /// Total slots, kept so in-flight count can be derived from the semaphore.
    delivery_capacity: usize,
}

impl OutgoingWebhookService {
    pub fn new(pool: Pool<Postgres>, max_attempts: i32, max_concurrent_deliveries: usize) -> Self {
        let delivery_capacity = max_concurrent_deliveries.max(1);
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
            delivery_slots: Arc::new(tokio::sync::Semaphore::new(delivery_capacity)),
            delivery_capacity,
        }
    }

    /// Deliveries currently holding a slot. Exposed for metrics/diagnostics.
    pub fn deliveries_in_flight(&self) -> usize {
        self.delivery_capacity
            .saturating_sub(self.delivery_slots.available_permits())
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

            // Acquire the slot BEFORE spawning and hand the permit to the task.
            //
            // Acquiring inside the spawned task would bound concurrent HTTP but
            // not the number of tasks: they would pile up parked on the
            // semaphore, each retaining its own clone of the job payload, and
            // the memory exhaustion this is meant to prevent would happen
            // anyway. Awaiting here is safe — `dispatch_event` is itself already
            // running on a detached task (see `spawn_dispatch`), so the caller's
            // request is never blocked; backpressure lands on the dispatcher,
            // which is exactly where it belongs.
            let permit = match Arc::clone(&self.delivery_slots).acquire_owned().await {
                Ok(p) => p,
                Err(e) => {
                    error!(error = %e, "Outgoing webhook delivery semaphore closed");
                    break;
                }
            };

            let payload = payload.clone();
            let service = self.clone();
            let event_clone = event.clone();
            tokio::spawn(async move {
                if let Err(e) = service
                    .deliver_with_permit(webhook, event_clone, payload, delivery_id, permit)
                    .await
                {
                    error!(error = %e, "Failed to deliver webhook");
                }
            });
        }

        Ok(())
    }

    /// Deliver one event, acquiring a delivery slot first.
    ///
    /// Use this from paths that are NOT already holding a permit.
    pub async fn deliver(
        &self,
        webhook: OutgoingWebhook,
        event: String,
        payload: serde_json::Value,
        delivery_id: uuid::Uuid,
    ) -> Result<()> {
        let permit = Arc::clone(&self.delivery_slots)
            .acquire_owned()
            .await
            .context("Outgoing webhook delivery semaphore closed")?;
        self.deliver_with_permit(webhook, event, payload, delivery_id, permit)
            .await
    }

    /// Deliver one event using an already-acquired slot.
    ///
    /// The permit is held for the WHOLE delivery — retries and backoff sleeps
    /// included, since those sleeps are the window that made unbounded spawning
    /// dangerous — and released when this future completes.
    pub async fn deliver_with_permit(
        &self,
        webhook: OutgoingWebhook,
        event: String,
        payload: serde_json::Value,
        delivery_id: uuid::Uuid,
        permit: tokio::sync::OwnedSemaphorePermit,
    ) -> Result<()> {
        let _permit = permit;

        let body = serde_json::json!({
            "id": delivery_id,
            "event": event,
            "created_at": Utc::now(),
            "data": payload,
        });

        let max_attempts = self.max_attempts.max(1);
        let mut delivered = false;
        for attempt in 1..=max_attempts {
            delivered = self.try_deliver(&webhook, &body, attempt).await?;
            if delivered || attempt >= max_attempts {
                break;
            }

            let backoff = Duration::from_secs(2_u64.pow((attempt - 1).min(5) as u32));
            warn!(
                "Webhook delivery failed (attempt {}/{}), retrying in {}s",
                attempt,
                max_attempts,
                backoff.as_secs()
            );
            tokio::time::sleep(backoff).await;
        }

        // Account the DELIVERY, not each attempt. Bumping failure_count inside
        // try_deliver multiplied every failed event by the retry count, so the
        // number the API reports was never "how many deliveries failed".
        self.record_delivery_outcome(&webhook.id, delivered).await;

        Ok(())
    }

    /// Update per-webhook health after a delivery resolves, and trip the breaker.
    ///
    /// Success resets the counter; failure increments it once. Crossing
    /// [`AUTO_DISABLE_FAILURE_THRESHOLD`] disables the webhook so a permanently
    /// dead endpoint stops consuming delivery slots — `dispatch_event` only
    /// selects `enabled = true` rows, so this is self-enforcing.
    async fn record_delivery_outcome(&self, webhook_id: &str, delivered: bool) {
        let result = sqlx::query(
            r#"
            UPDATE outgoing_webhooks
            SET failure_count = CASE WHEN $2 THEN 0 ELSE failure_count + 1 END,
                enabled = CASE
                    WHEN NOT $2 AND failure_count + 1 >= $3 THEN FALSE
                    ELSE enabled
                END,
                last_status = CASE
                    WHEN NOT $2 AND failure_count + 1 >= $3 THEN $4
                    ELSE last_status
                END,
                updated_at = NOW()
            WHERE id = $1
            RETURNING failure_count, enabled
            "#,
        )
        .bind(webhook_id)
        .bind(delivered)
        .bind(AUTO_DISABLE_FAILURE_THRESHOLD)
        .bind(AUTO_DISABLED_STATUS)
        .fetch_optional(&self.pool)
        .await;

        match result {
            Ok(Some(row)) => {
                use sqlx::Row;
                let failure_count: i32 = row.get("failure_count");
                let enabled: bool = row.get("enabled");
                if !delivered && !enabled {
                    error!(
                        webhook_id,
                        failure_count,
                        threshold = AUTO_DISABLE_FAILURE_THRESHOLD,
                        "Outgoing webhook auto-disabled after consecutive delivery failures"
                    );
                }
            }
            Ok(None) => {
                // Webhook deleted mid-delivery; nothing to record.
            }
            Err(e) => {
                warn!(error = %e, webhook_id, "Failed to record webhook delivery outcome");
            }
        }
    }

    pub async fn retry_delivery(&self, delivery_id: uuid::Uuid) -> Result<bool> {
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
            return Ok(true);
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

        // A manual retry is a real outbound request, so it must occupy a
        // delivery slot like any other — otherwise the cap is not process-wide
        // and the retry endpoint becomes an unmetered path to the same
        // exhaustion the semaphore exists to prevent.
        let _permit = Arc::clone(&self.delivery_slots)
            .acquire_owned()
            .await
            .context("Outgoing webhook delivery semaphore closed")?;

        let delivered = self.try_deliver(&webhook, &body, attempts + 1).await?;
        // A manual retry is a delivery too: a success here must clear the
        // failure streak, otherwise a webhook the user just fixed stays one
        // failure away from the auto-disable breaker.
        self.record_delivery_outcome(&webhook.id, delivered).await;
        Ok(delivered)
    }

    async fn try_deliver(
        &self,
        webhook: &OutgoingWebhook,
        body: &serde_json::Value,
        attempt: i32,
    ) -> Result<bool> {
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

                Ok(success)
            }
            Err(e) => {
                self.record_attempt(&webhook.id, body, 0, Some(&e.to_string()), attempt, start)
                    .await?;
                Ok(false)
            }
        }
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
        // `failure_count` is deliberately NOT touched here: this runs once per
        // ATTEMPT, and the counter must move once per DELIVERY. It is owned by
        // `record_delivery_outcome`, which also owns the auto-disable breaker.
        sqlx::query(
            r#"
            UPDATE outgoing_webhooks
            SET last_triggered_at = NOW(),
                last_status = $2,
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

    /// Validate webhook URL to prevent SSRF attacks.
    ///
    /// Scheme/hostname/literal-IP checks only: `check_dns_resolution` is OFF
    /// here deliberately. That check calls the blocking `to_socket_addrs()`, and
    /// this runs on every delivery ATTEMPT on the async runtime — an
    /// attacker-supplied hostname served by a nameserver that never answers
    /// would park a runtime worker thread for the resolver's full timeout.
    ///
    /// The IP policy is still enforced for every connection, by
    /// `SsrfGuardedResolver` inside the HTTP client (`security::http_client`),
    /// which is async, covers redirect hops, and — unlike a check performed here
    /// — validates the address actually connected to rather than one resolved
    /// moments earlier. DNS validation still runs at configure time
    /// (create/update/test), where the user gets an immediate, clear error.
    fn validate_url(&self, url: &str) -> Result<()> {
        let options = UrlValidationOptions {
            check_dns_resolution: false,
            ..UrlValidationOptions::default()
        };
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
///
/// When `secret` is `Some` and non-empty, signs with the same HMAC scheme as org
/// outgoing webhooks (`X-Spooled-Timestamp` + `X-Spooled-Signature`). Unsigned
/// when secret is absent — backward compatible with existing receivers.
///
/// The envelope matches org webhooks exactly (`id`/`event`/`created_at`/`data`)
/// so a receiver can dedupe on `body.id` for BOTH kinds. It previously omitted
/// `id`, which silently degraded id-keyed dedupe to a constant key.
pub fn spawn_completion_webhook(
    url: String,
    event: String,
    payload: serde_json::Value,
    secret: Option<String>,
) {
    tokio::spawn(async move {
        if let Err(e) = deliver_completion_webhook(&url, &event, &payload, secret.as_deref()).await
        {
            warn!(error = %e, url = %url, event = %event, "completion_webhook delivery failed");
        }
    });
}

async fn deliver_completion_webhook(
    url: &str,
    event: &str,
    payload: &serde_json::Value,
    secret: Option<&str>,
) -> Result<()> {
    // No blocking DNS on the delivery path — the client's SsrfGuardedResolver
    // enforces the IP policy at connect time. See `validate_url` above.
    let options = UrlValidationOptions {
        check_dns_resolution: false,
        ..UrlValidationOptions::default()
    };
    validate_url_ssrf(url, &options).map_err(|e| anyhow::anyhow!("Invalid webhook URL: {}", e))?;

    let client = build_outbound_http_client(
        Duration::from_secs(10),
        Duration::from_secs(5),
        "Spooled-CompletionWebhook/1.0",
    );
    let delivery_id = uuid::Uuid::new_v4();
    let body = serde_json::json!({
        "id": delivery_id,
        "event": event,
        "created_at": Utc::now(),
        "data": payload,
    });
    let payload_json = serde_json::to_string(&body).unwrap_or_default();

    let mut request_builder = client
        .post(url)
        .header("Content-Type", "application/json")
        .header("X-Spooled-Event", event)
        .header("X-Spooled-Delivery-Attempt", "1");

    if let Some(secret) = secret.filter(|s| !s.is_empty()) {
        let timestamp = Utc::now().timestamp();
        let signature = sign_payload(secret, timestamp, &payload_json)?;
        request_builder = request_builder
            .header("X-Spooled-Timestamp", timestamp.to_string())
            .header("X-Spooled-Signature", signature);
    }

    let resp = request_builder.body(payload_json).send().await?;
    if !resp.status().is_success() {
        anyhow::bail!("HTTP {}", resp.status());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sign_payload_matches_org_webhook_format() {
        let sig = sign_payload("whsec_test", 1_700_000_000, r#"{"event":"job.completed"}"#)
            .expect("sign");
        assert!(sig.starts_with("sha256="));
        assert_eq!(sig.len(), "sha256=".len() + 64);
        let again = sign_payload("whsec_test", 1_700_000_000, r#"{"event":"job.completed"}"#)
            .expect("sign again");
        assert_eq!(sig, again);
    }

    #[test]
    fn sign_payload_changes_with_secret_or_body() {
        let a = sign_payload("a", 1, "{}").unwrap();
        let b = sign_payload("b", 1, "{}").unwrap();
        let c = sign_payload("a", 1, "{\"x\":1}").unwrap();
        assert_ne!(a, b);
        assert_ne!(a, c);
    }
}
