use anyhow::{Context, Result};
use chrono::Utc;
use hmac::{Hmac, KeyInit, Mac};
use reqwest::Client;
use sha2::Sha256;
use sqlx::{Pool, Postgres};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;
use tokio::sync::{mpsc, OwnedSemaphorePermit, Semaphore};
use tracing::{debug, error, info, warn};

use super::events;
use super::format::body_for;
use crate::models::{OutgoingWebhook, WebhookFormat};
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

/// Bytes of the receiver's answer kept in a delivery's `response_body`.
pub const MAX_STORED_RESPONSE_BODY_BYTES: usize = 1024;

/// Characters of error text kept in a delivery's `error`.
const MAX_STORED_ERROR_CHARS: usize = 1024;

/// Upper bound on consumers, whatever the configuration says: each one is a
/// task, and the setting used to size a semaphore, where a huge value was free.
const MAX_CONSUMERS: usize = 1024;

/// Upper bound on the queue (one channel slot and one semaphore permit each).
const MAX_QUEUE_CAPACITY: usize = 1 << 20;

/// Lane-map size past which lanes with nothing admitted are forgotten.
const LANE_PRUNE_THRESHOLD: usize = 1024;

/// User agents, kept distinct now that both kinds share one client.
const ORG_WEBHOOK_USER_AGENT: &str = "Spooled-OutgoingWebhook/1.0";
const COMPLETION_WEBHOOK_USER_AGENT: &str = "Spooled-CompletionWebhook/1.0";

/// Manual retries stop here. The HTTP handler reports the same cap.
pub const MAX_MANUAL_RETRY_ATTEMPTS: i32 = 10;

/// A manual retry lost the claim to another retry of the same delivery.
/// The loser does not send and does not write the delivery row.
#[derive(Debug)]
pub struct RetryInProgress;

impl std::fmt::Display for RetryInProgress {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("A retry of this delivery is already in progress")
    }
}

impl std::error::Error for RetryInProgress {}

/// Claim held across one outbound request. The outcome write must present the
/// same token, so a retry that lost the race cannot overwrite the row.
#[derive(Clone, Copy)]
struct DeliveryClaim {
    attempts: i32,
    token: i64,
}

/// Bounds on outgoing deliveries.
///
/// Every delivery (one event for one webhook) is admitted into a bounded queue
/// and worked by a fixed set of consumers. The queue bound keeps a failing or
/// black-holing endpoint from turning into unbounded tasks and payload copies;
/// the per-webhook bounds keep one such endpoint from taking every consumer, or
/// every queue slot, from the other tenants.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeliveryLimits {
    /// Consumers, and outbound requests in flight process-wide.
    pub max_concurrent: usize,
    /// Events and deliveries admitted but not resolved yet: queued, waiting for
    /// their webhook's turn, or sleeping before a retry. Past this, new ones are
    /// dropped and counted ([`OutgoingWebhookService::deliveries_dropped`]).
    pub queue_capacity: usize,
    /// Requests in flight to one webhook.
    pub per_webhook_concurrency: usize,
    /// Deliveries one webhook may have admitted at once.
    pub per_webhook_queue: usize,
}

impl DeliveryLimits {
    /// Limits derived from `OUTGOING_WEBHOOK_MAX_CONCURRENT_DELIVERIES`: a queue
    /// of 16 deliveries per slot, and one webhook gets at most a quarter of the
    /// slots and a quarter of the queue.
    pub fn from_concurrency(max_concurrent: usize) -> Self {
        let max_concurrent = max_concurrent.clamp(1, MAX_CONSUMERS);
        let queue_capacity = max_concurrent * 16;
        Self {
            max_concurrent,
            queue_capacity,
            per_webhook_concurrency: (max_concurrent / 4).max(1),
            per_webhook_queue: (queue_capacity / 4).max(1),
        }
    }

    fn normalized(self) -> Self {
        let max_concurrent = self.max_concurrent.clamp(1, MAX_CONSUMERS);
        let queue_capacity = self.queue_capacity.clamp(1, MAX_QUEUE_CAPACITY);
        Self {
            max_concurrent,
            queue_capacity,
            per_webhook_concurrency: self.per_webhook_concurrency.clamp(1, max_concurrent),
            per_webhook_queue: self.per_webhook_queue.clamp(1, queue_capacity),
        }
    }
}

/// Org-configured outgoing webhooks: a bounded delivery queue worked by a fixed
/// set of consumers (see [`DeliveryLimits`]).
///
/// It replaces one spawned task per event, each parked on a global semaphore
/// with its own payload copy and a webhook list read before the wait: a failing
/// endpoint grew that pile without bound, kept being called after the breaker
/// disabled it, and held the global slots through its backoff sleeps, so every
/// other tenant's deliveries queued behind it.
#[derive(Clone)]
pub struct OutgoingWebhookService {
    inner: Arc<Inner>,
}

struct Inner {
    pool: Pool<Postgres>,
    client: Client,
    max_attempts: i32,
    limits: DeliveryLimits,
    /// Outbound requests in flight. Consumers and manual retries hold a slot
    /// for the request itself only, never across a backoff sleep. Per-job
    /// completion webhooks have their own pool (see [`CompletionGate`]).
    delivery_slots: Arc<Semaphore>,
    /// One permit per admitted event or delivery: the queue bound. A permit
    /// travels with its item until the delivery resolves.
    admission: Arc<Semaphore>,
    /// As large as `admission`, and every item in it holds a permit, so a send
    /// never finds it full.
    queue_tx: mpsc::Sender<Work>,
    queue_rx: tokio::sync::Mutex<mpsc::Receiver<Work>>,
    /// Consumers start on first use (a runtime is guaranteed there, not in `new`).
    consumers: OnceLock<()>,
    lanes: Mutex<HashMap<String, Arc<Lane>>>,
    dropped: AtomicU64,
}

/// Per-webhook bounds (per-org ones for completion webhooks).
struct Lane {
    /// Requests in flight to this webhook (or for this org).
    slots: Arc<Semaphore>,
    /// This webhook's (or org's) share of the queue.
    share: Arc<Semaphore>,
}

enum Work {
    /// An event not matched to webhooks yet (from `spawn_dispatch`).
    Event {
        organization_id: String,
        event: String,
        resource_id: String,
        payload: serde_json::Value,
        admission: OwnedSemaphorePermit,
    },
    /// One event for one webhook.
    Delivery(Box<Delivery>),
}

struct Delivery {
    /// The webhook as of fan-out; re-read before every attempt.
    webhook: OutgoingWebhook,
    /// The signed envelope (`id`/`event`/`created_at`/`data`), same on every attempt.
    body: serde_json::Value,
    attempt: i32,
    lane: Arc<Lane>,
    /// The webhook's request slot, once granted.
    lane_slot: Option<OwnedSemaphorePermit>,
    _share: OwnedSemaphorePermit,
    _admission: OwnedSemaphorePermit,
}

/// Result of one delivery attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Attempt {
    Delivered,
    /// Transport error or non-2xx answer: worth retrying.
    Failed,
    /// The URL fails the SSRF policy, so retrying cannot help. Still a failed
    /// delivery for the breaker, or such a webhook would never auto-disable.
    Rejected,
    /// This attempt held the claim, the POST failed, and the row write failed.
    /// The claim was released. Retry; do not count the attempt as recorded.
    Released,
    /// Another retry owns this delivery. Do not send, write, or count it.
    Superseded,
}

impl OutgoingWebhookService {
    pub fn new(pool: Pool<Postgres>, max_attempts: i32, max_concurrent_deliveries: usize) -> Self {
        Self::with_limits(
            pool,
            max_attempts,
            DeliveryLimits::from_concurrency(max_concurrent_deliveries),
        )
    }

    pub fn with_limits(pool: Pool<Postgres>, max_attempts: i32, limits: DeliveryLimits) -> Self {
        let limits = limits.normalized();
        // SECURITY: Never fall back to a default client. A default client may have no timeouts
        // and will follow redirects, increasing SSRF/DoS risk.
        let client = build_outbound_http_client(
            Duration::from_secs(10),
            Duration::from_secs(5),
            ORG_WEBHOOK_USER_AGENT,
        );
        let delivery_slots = Arc::new(Semaphore::new(limits.max_concurrent));
        // Per-job completion webhooks have no handle on this service, so the
        // first one built (AppState's, in production) lends them its client
        // (one connection pool) and its limits. Not its slots: consumers wait
        // on those inline, so a tenant's completion backlog queued there would
        // stall every tenant's org-webhook deliveries.
        let _ = COMPLETION_GATE.set(CompletionGate::new(client.clone(), limits));
        let (queue_tx, queue_rx) = mpsc::channel(limits.queue_capacity);
        Self {
            inner: Arc::new(Inner {
                pool,
                client,
                max_attempts: max_attempts.max(1),
                limits,
                delivery_slots,
                admission: Arc::new(Semaphore::new(limits.queue_capacity)),
                queue_tx,
                queue_rx: tokio::sync::Mutex::new(queue_rx),
                consumers: OnceLock::new(),
                lanes: Mutex::new(HashMap::new()),
                dropped: AtomicU64::new(0),
            }),
        }
    }

    pub fn limits(&self) -> DeliveryLimits {
        self.inner.limits
    }

    /// Requests holding a slot. Exposed for metrics/diagnostics.
    pub fn deliveries_in_flight(&self) -> usize {
        self.inner
            .limits
            .max_concurrent
            .saturating_sub(self.inner.delivery_slots.available_permits())
    }

    /// Events and deliveries admitted and not resolved yet.
    pub fn deliveries_queued(&self) -> usize {
        self.inner
            .limits
            .queue_capacity
            .saturating_sub(self.inner.admission.available_permits())
    }

    /// Events and deliveries dropped because the queue (or the webhook's share
    /// of it) was full, since start.
    pub fn deliveries_dropped(&self) -> u64 {
        self.inner.dropped.load(Ordering::Relaxed)
    }

    /// Best-effort fire-and-forget dispatch used by REST/gRPC handlers.
    ///
    /// Never blocks and never spawns: the event is queued for the consumers,
    /// or dropped (and counted) when the queue is full. An event that implies
    /// another ([`events::implied_event`]: a dead-lettering `job.failed` is
    /// also a `job.deadlettered`) queues that one too, with the same data.
    pub fn spawn_dispatch(
        self: &Arc<Self>,
        organization_id: String,
        event: String,
        resource_id: String,
        payload: serde_json::Value,
    ) {
        // The event itself first: with the queue nearly full, the one callers
        // always had wins the last slot.
        let implied = events::implied_event(&event, &payload).map(|implied| {
            (
                implied.to_string(),
                organization_id.clone(),
                resource_id.clone(),
                payload.clone(),
            )
        });
        self.queue_event(organization_id, event, resource_id, payload);
        if let Some((implied, organization_id, resource_id, payload)) = implied {
            self.queue_event(organization_id, implied, resource_id, payload);
        }
    }

    fn queue_event(
        self: &Arc<Self>,
        organization_id: String,
        event: String,
        resource_id: String,
        payload: serde_json::Value,
    ) {
        let Ok(admission) = Arc::clone(&self.inner.admission).try_acquire_owned() else {
            self.inner
                .note_drop("delivery queue full", &organization_id, &event, None);
            return;
        };
        self.inner.enqueue(Work::Event {
            organization_id,
            event,
            resource_id,
            payload,
            admission,
        });
    }

    /// Queue one delivery per matching webhook (and per webhook matching the
    /// event it implies, as in [`Self::spawn_dispatch`]). Returns once they are
    /// queued, not delivered.
    pub async fn dispatch_event(
        &self,
        event: String,
        resource_id: &str,
        payload: serde_json::Value,
        organization_id: &str,
    ) -> Result<()> {
        let implied = events::implied_event(&event, &payload).map(|i| (i, payload.clone()));
        self.inner
            .fan_out(&event, resource_id, payload, organization_id, None)
            .await?;
        if let Some((implied, payload)) = implied {
            self.inner
                .fan_out(implied, resource_id, payload, organization_id, None)
                .await?;
        }
        Ok(())
    }

    pub async fn retry_delivery(&self, delivery_id: uuid::Uuid) -> Result<bool> {
        // Fetch delivery details using runtime query (no compile-time check)
        let row = sqlx::query(
            r#"
            SELECT d.webhook_id, d.event, d.status, d.payload, d.attempts, d.created_at,
                   w.url, w.secret, w.format
            FROM outgoing_webhook_deliveries d
            JOIN outgoing_webhooks w ON d.webhook_id = w.id
            WHERE d.id = $1
            "#,
        )
        .bind(delivery_id.to_string())
        .fetch_optional(&self.inner.pool)
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
            // The webhook's current format: the stored payload is the
            // envelope, rendered again for this attempt.
            format: row.get("format"),
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

        // Claim before sending. The automatic retry of this row may already be
        // in its backoff sleep; without the claim both POSTs run and both write
        // the delivery. A miss means that other retry owns it (or the attempt
        // cap was crossed). A success that landed since the read is not an error.
        let claim = match self
            .inner
            .claim_retry(
                &delivery_id.to_string(),
                attempts,
                Some(MAX_MANUAL_RETRY_ATTEMPTS),
            )
            .await?
        {
            Some(claim) => claim,
            None => {
                let status: Option<String> = sqlx::query_scalar(
                    "SELECT status FROM outgoing_webhook_deliveries WHERE id = $1",
                )
                .bind(delivery_id.to_string())
                .fetch_optional(&self.inner.pool)
                .await?;
                if status.as_deref() == Some("success") {
                    return Ok(true);
                }
                return Err(RetryInProgress.into());
            }
        };

        // A manual retry is a real outbound request, so it must occupy a
        // delivery slot like any other — otherwise the cap is not process-wide
        // and the retry endpoint becomes an unmetered path to the same
        // exhaustion the slots exist to prevent.
        let slot = self
            .inner
            .delivery_slots
            .acquire()
            .await
            .context("Outgoing webhook delivery semaphore closed")?;
        let outcome = self
            .inner
            .try_deliver(&webhook, &body, claim.attempts, Some(claim))
            .await;
        drop(slot);
        // Superseded: another retry owns the row. Released: this retry's row
        // write failed and the claim was cleared, so the caller may try again
        // but this call did not record an outcome.
        if matches!(outcome, Attempt::Superseded | Attempt::Released) {
            return Err(RetryInProgress.into());
        }

        let delivered = outcome == Attempt::Delivered;
        // A manual retry is a delivery too: a success here must clear the
        // failure streak, otherwise a webhook the user just fixed stays one
        // failure away from the auto-disable breaker.
        self.inner
            .record_delivery_outcome(&webhook.id, delivered)
            .await;
        Ok(delivered)
    }
}

impl Inner {
    fn enqueue(self: &Arc<Self>, work: Work) {
        self.ensure_consumers();
        // Cannot fail while the invariant on `queue_tx` holds; never block a
        // caller over it if it somehow does.
        if let Err(e) = self.queue_tx.try_send(work) {
            let (organization_id, event, webhook_id) = match e.into_inner() {
                Work::Event {
                    organization_id,
                    event,
                    ..
                } => (organization_id, event, None),
                Work::Delivery(d) => (
                    d.webhook.organization_id,
                    d.body["event"].as_str().unwrap_or_default().to_string(),
                    Some(d.webhook.id),
                ),
            };
            self.note_drop(
                "delivery queue closed or full",
                &organization_id,
                &event,
                webhook_id.as_deref(),
            );
        }
    }

    fn note_drop(
        &self,
        reason: &str,
        organization_id: &str,
        event: &str,
        webhook_id: Option<&str>,
    ) {
        let dropped = self.dropped.fetch_add(1, Ordering::Relaxed) + 1;
        // Under overload this runs per event: warn on the first and every 100th.
        if dropped == 1 || dropped.is_multiple_of(100) {
            warn!(
                reason,
                org_id = organization_id,
                event,
                webhook_id,
                dropped_total = dropped,
                "Outgoing webhook delivery dropped"
            );
        } else {
            debug!(
                reason,
                org_id = organization_id,
                event,
                webhook_id,
                "Outgoing webhook delivery dropped"
            );
        }
    }

    fn ensure_consumers(self: &Arc<Self>) {
        self.consumers.get_or_init(|| {
            for _ in 0..self.limits.max_concurrent {
                let inner = Arc::clone(self);
                tokio::spawn(async move {
                    // A panic loses the item in hand, not the consumer, so the
                    // pool never silently shrinks.
                    loop {
                        match tokio::spawn(Arc::clone(&inner).consume()).await {
                            Err(e) if e.is_panic() => {
                                error!("Outgoing webhook consumer panicked; restarting it")
                            }
                            _ => return,
                        }
                    }
                });
            }
        });
    }

    async fn consume(self: Arc<Self>) {
        loop {
            let work = self.queue_rx.lock().await.recv().await;
            let Some(work) = work else { return };
            match work {
                Work::Event {
                    organization_id,
                    event,
                    resource_id,
                    payload,
                    admission,
                } => {
                    if let Err(e) = self
                        .fan_out(
                            &event,
                            &resource_id,
                            payload,
                            &organization_id,
                            Some(admission),
                        )
                        .await
                    {
                        warn!(error = %e, org_id = %organization_id, "Outgoing webhook dispatch failed");
                    }
                }
                Work::Delivery(delivery) => self.work(delivery).await,
            }
        }
    }

    /// Queue one delivery per enabled webhook subscribed to `event`.
    ///
    /// `carried` is the event's own queue permit: it passes to the first
    /// delivery, so fanning out never needs more room than the event held.
    async fn fan_out(
        self: &Arc<Self>,
        event: &str,
        resource_id: &str,
        payload: serde_json::Value,
        organization_id: &str,
        mut carried: Option<OwnedSemaphorePermit>,
    ) -> Result<()> {
        let webhooks = sqlx::query_as::<_, OutgoingWebhook>(
            r#"
            SELECT * FROM outgoing_webhooks
            WHERE organization_id = $1
            AND enabled = true
            AND $2 = ANY(events)
            "#,
        )
        .bind(organization_id)
        .bind(event)
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
            let Some((lane, share)) = self.lane_share(&webhook.id) else {
                self.note_drop(
                    "webhook backlog full",
                    organization_id,
                    event,
                    Some(&webhook.id),
                );
                continue;
            };
            let admission = match carried.take() {
                Some(permit) => permit,
                None => match Arc::clone(&self.admission).try_acquire_owned() {
                    Ok(permit) => permit,
                    Err(_) => {
                        self.note_drop(
                            "delivery queue full",
                            organization_id,
                            event,
                            Some(&webhook.id),
                        );
                        continue;
                    }
                },
            };
            let body = serde_json::json!({
                "id": uuid::Uuid::new_v4(),
                "event": event,
                "created_at": Utc::now(),
                "data": payload.clone(),
            });
            self.enqueue(Work::Delivery(Box::new(Delivery {
                webhook,
                body,
                attempt: 1,
                lane,
                lane_slot: None,
                _share: share,
                _admission: admission,
            })));
        }

        Ok(())
    }

    /// The webhook's lane and one unit of its queue share, if it has any left.
    fn lane_share(&self, webhook_id: &str) -> Option<(Arc<Lane>, OwnedSemaphorePermit)> {
        let mut lanes = self.lanes.lock().unwrap_or_else(|e| e.into_inner());
        if lanes.len() >= LANE_PRUNE_THRESHOLD && !lanes.contains_key(webhook_id) {
            let full_share = self.limits.per_webhook_queue;
            lanes.retain(|_, lane| lane.share.available_permits() < full_share);
        }
        let lane = lanes.entry(webhook_id.to_string()).or_insert_with(|| {
            Arc::new(Lane {
                slots: Arc::new(Semaphore::new(self.limits.per_webhook_concurrency)),
                share: Arc::new(Semaphore::new(self.limits.per_webhook_queue)),
            })
        });
        let share = Arc::clone(&lane.share).try_acquire_owned().ok()?;
        Some((Arc::clone(lane), share))
    }

    /// Run one attempt of `delivery`, then retry it, resolve it or drop it.
    async fn work(self: &Arc<Self>, mut delivery: Box<Delivery>) {
        if delivery.lane_slot.is_none() {
            match Arc::clone(&delivery.lane.slots).try_acquire_owned() {
                Ok(slot) => delivery.lane_slot = Some(slot),
                Err(_) => {
                    // The webhook is at its concurrency cap. Wait for its turn
                    // off the consumer, so a slow endpoint occupies at most
                    // `per_webhook_concurrency` consumers.
                    let inner = Arc::clone(self);
                    tokio::spawn(async move {
                        if let Ok(slot) = Arc::clone(&delivery.lane.slots).acquire_owned().await {
                            delivery.lane_slot = Some(slot);
                            inner.enqueue(Work::Delivery(delivery));
                        }
                    });
                    return;
                }
            }
        }

        let outcome = self.attempt(&mut delivery).await;
        // Free the webhook's slot before any backoff sleep.
        delivery.lane_slot = None;

        match outcome {
            None => {}
            Some(Attempt::Delivered) => {
                self.record_delivery_outcome(&delivery.webhook.id, true)
                    .await
            }
            Some(Attempt::Failed | Attempt::Released) if delivery.attempt < self.max_attempts => {
                self.schedule_retry(delivery)
            }
            // Another sender owns the retry, or this attempt's row write failed
            // after the budget was used. Counting it or scheduling again would
            // double the POST and the row write. A released claim is what lets
            // an earlier continuation take the row; this branch does not start one.
            Some(Attempt::Superseded | Attempt::Released) => {}
            // Account the DELIVERY, not each attempt. Bumping failure_count per
            // attempt multiplied every failed event by the retry count, so the
            // number the API reports was never "how many deliveries failed".
            Some(Attempt::Failed | Attempt::Rejected) => {
                self.record_delivery_outcome(&delivery.webhook.id, false)
                    .await
            }
        }
    }

    /// One attempt, or `None` when the webhook was disabled, deleted or
    /// unsubscribed from the event while the delivery waited (it is then
    /// abandoned, not counted as a failure).
    async fn attempt(&self, delivery: &mut Delivery) -> Option<Attempt> {
        // Re-read the webhook right before calling it. The breaker (or the user)
        // may have disabled it since the event was queued, and a new URL or
        // secret applies to the next attempt.
        let event = delivery.body["event"].as_str().unwrap_or_default();
        match sqlx::query_as::<_, OutgoingWebhook>("SELECT * FROM outgoing_webhooks WHERE id = $1")
            .bind(&delivery.webhook.id)
            .fetch_optional(&self.pool)
            .await
        {
            Ok(Some(current)) if current.enabled && current.events.iter().any(|e| e == event) => {
                delivery.webhook = current
            }
            Ok(_) => {
                debug!(
                    webhook_id = %delivery.webhook.id,
                    attempt = delivery.attempt,
                    "Outgoing webhook disabled, deleted or unsubscribed; delivery abandoned"
                );
                return None;
            }
            Err(e) => warn!(
                error = %e,
                webhook_id = %delivery.webhook.id,
                "Could not re-read webhook before delivery; using the queued copy"
            ),
        }

        // Retries claim the row first. The manual endpoint claims the same way,
        // so only one of them sends. Attempt 1 has no row yet (or is the insert).
        let claim = if delivery.attempt > 1 {
            let delivery_id = delivery.body["id"].as_str().unwrap_or_default();
            if delivery_id.is_empty() {
                None
            } else {
                match self
                    .claim_retry(delivery_id, delivery.attempt - 1, None)
                    .await
                {
                    Ok(Some(claim)) => {
                        delivery.attempt = claim.attempts;
                        Some(claim)
                    }
                    Ok(None) => {
                        let exists: Option<i32> = sqlx::query_scalar(
                            "SELECT 1 FROM outgoing_webhook_deliveries WHERE id = $1",
                        )
                        .bind(delivery_id)
                        .fetch_optional(&self.pool)
                        .await
                        .ok()
                        .flatten();
                        if exists.is_some() {
                            debug!(
                                webhook_id = %delivery.webhook.id,
                                delivery_id,
                                "Webhook retry already claimed; not sending"
                            );
                            return None;
                        }
                        // The first attempt never managed to insert the row.
                        // Send this one unclaimed, as that attempt did.
                        None
                    }
                    Err(e) => {
                        warn!(
                            error = %e,
                            webhook_id = %delivery.webhook.id,
                            "Could not claim webhook retry; not sending"
                        );
                        return None;
                    }
                }
            }
        } else {
            None
        };

        let _slot = self.delivery_slots.acquire().await.ok()?;
        Some(
            self.try_deliver(&delivery.webhook, &delivery.body, delivery.attempt, claim)
                .await,
        )
    }

    /// Take the right to send one retry. `observed_attempts` is the attempts
    /// value the caller saw; the update matches it, so two callers cannot both
    /// proceed. `max_attempts` caps a manual retry. A claim older than two
    /// minutes can be taken again: that is past the outbound timeout, so the
    /// previous sender is not still in flight.
    async fn claim_retry(
        &self,
        delivery_id: &str,
        observed_attempts: i32,
        max_attempts: Option<i32>,
    ) -> Result<Option<DeliveryClaim>, sqlx::Error> {
        let row: Option<(i32, i64)> = sqlx::query_as(
            r#"
            UPDATE outgoing_webhook_deliveries
            SET attempts = attempts + 1,
                retry_claim_token = retry_claim_token + 1,
                retry_claimed_at = NOW()
            WHERE id = $1
              AND status = 'failed'
              AND attempts = $2
              AND ($3::INT IS NULL OR attempts < $3)
              AND (
                    retry_claimed_at IS NULL
                    OR retry_claimed_at < NOW() - INTERVAL '2 minutes'
                  )
            RETURNING attempts, retry_claim_token
            "#,
        )
        .bind(delivery_id)
        .bind(observed_attempts)
        .bind(max_attempts)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|(attempts, token)| DeliveryClaim { attempts, token }))
    }

    /// Re-queue `delivery` after its backoff. While it sleeps it keeps its
    /// queue permit (memory stays bounded) but holds no request slot.
    fn schedule_retry(self: &Arc<Self>, mut delivery: Box<Delivery>) {
        let backoff = Duration::from_secs(2_u64.pow((delivery.attempt - 1).clamp(0, 5) as u32));
        warn!(
            webhook_id = %delivery.webhook.id,
            "Webhook delivery failed (attempt {}/{}), retrying in {}s",
            delivery.attempt,
            self.max_attempts,
            backoff.as_secs()
        );
        delivery.attempt += 1;
        let inner = Arc::clone(self);
        tokio::spawn(async move {
            tokio::time::sleep(backoff).await;
            inner.enqueue(Work::Delivery(delivery));
        });
    }

    /// Update per-webhook health after a delivery resolves, and trip the breaker.
    ///
    /// Success resets the counter; failure increments it once. Crossing
    /// [`AUTO_DISABLE_FAILURE_THRESHOLD`] disables the webhook so a permanently
    /// dead endpoint stops consuming delivery slots — `fan_out` only selects
    /// `enabled = true` rows and every attempt re-checks `enabled`, so this is
    /// self-enforcing, including for deliveries already queued.
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

    /// Send one attempt and log it. The caller holds a delivery slot.
    ///
    /// `claim` is set for a retry. The row write has to match that token; if it
    /// does not, the attempt was superseded and the caller must not count it.
    async fn try_deliver(
        &self,
        webhook: &OutgoingWebhook,
        body: &serde_json::Value,
        attempt: i32,
        claim: Option<DeliveryClaim>,
    ) -> Attempt {
        let start = Utc::now();

        // Validate URL prevents SSRF
        if let Err(e) = self.validate_url(&webhook.url) {
            let recorded = self
                .record_attempt(
                    &webhook.id,
                    body,
                    AttemptLog::error(e.to_string()),
                    attempt,
                    start,
                    claim,
                )
                .await;
            return settle(recorded, Attempt::Rejected);
        }

        // `body` is the envelope, and it is what the delivery log keeps; the
        // webhook's format decides what goes on the wire (the envelope itself
        // for `json`). The signature covers the bytes sent.
        let payload_json = body_for(WebhookFormat::from_stored(&webhook.format), body);
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
                match sign_payload(secret, timestamp, &payload_json) {
                    Ok(signature) => {
                        request_builder = request_builder.header("X-Spooled-Signature", signature)
                    }
                    Err(e) => {
                        let log = AttemptLog::error(e.to_string());
                        let recorded = self
                            .record_attempt(&webhook.id, body, log, attempt, start, claim)
                            .await;
                        return settle(recorded, Attempt::Rejected);
                    }
                }
            }
        }

        match request_builder.body(payload_json).send().await {
            Ok(res) => {
                let status = res.status();
                let log = AttemptLog {
                    status_code: Some(i32::from(status.as_u16())),
                    response_body: read_capped_body(res, MAX_STORED_RESPONSE_BODY_BYTES).await,
                    error: (!status.is_success()).then(|| format!("HTTP {}", status.as_u16())),
                };
                let recorded = self
                    .record_attempt(&webhook.id, body, log, attempt, start, claim)
                    .await;
                settle(
                    recorded,
                    if status.is_success() {
                        Attempt::Delivered
                    } else {
                        Attempt::Failed
                    },
                )
            }
            Err(e) => {
                let log = AttemptLog::error(describe_send_error(e));
                let recorded = self
                    .record_attempt(&webhook.id, body, log, attempt, start, claim)
                    .await;
                settle(recorded, Attempt::Failed)
            }
        }
    }

    /// Write the delivery row for one attempt.
    ///
    /// A database error is logged, not returned: the request has already gone
    /// out, and failing here must neither re-send a delivery the receiver
    /// accepted nor abandon one that still has retries left.
    ///
    /// [`Recorded::Superseded`] means a claimed retry no longer owns the row.
    /// The caller must not schedule another attempt or bump `failure_count`.
    ///
    /// [`Recorded::Released`] means this attempt held the claim but the row
    /// write failed. `retry_claimed_at` is cleared first: leaving the stamp
    /// set made the continuation miss the claim for two minutes and drop the
    /// rest of the budget. The caller retries a failed POST and does not
    /// count the attempt as recorded.
    async fn record_attempt(
        &self,
        webhook_id: &str,
        request_payload: &serde_json::Value,
        log: AttemptLog,
        attempt_count: i32,
        started_at: chrono::DateTime<Utc>,
        claim: Option<DeliveryClaim>,
    ) -> Recorded {
        let status = match log.status_code {
            Some(code) if (200..300).contains(&code) => "success",
            _ => "failed",
        };

        let delivery_id = request_payload
            .get("id")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());

        // A claimed retry updates only its own token. The unconditional upsert
        // is the first attempt, which inserts the row.
        let recorded = if let Some(claim) = claim {
            match sqlx::query(
                r#"
                UPDATE outgoing_webhook_deliveries
                SET status = $3,
                    status_code = $4,
                    response_body = $5,
                    error = $6,
                    attempts = $7,
                    delivered_at = $8,
                    retry_claimed_at = NULL
                WHERE id = $1
                  AND retry_claim_token = $2
                  AND status = 'failed'
                "#,
            )
            .bind(&delivery_id)
            .bind(claim.token)
            .bind(status)
            .bind(log.status_code)
            .bind(&log.response_body)
            .bind(&log.error)
            .bind(attempt_count)
            .bind(Utc::now())
            .execute(&self.pool)
            .await
            {
                Ok(result) if result.rows_affected() > 0 => Recorded::Applied,
                Ok(_) => Recorded::Superseded,
                Err(e) => {
                    warn!(error = %e, webhook_id, delivery_id, "Failed to record webhook delivery attempt");
                    // claim_retry already stamped retry_claimed_at. The
                    // continuation cannot take a claim newer than two minutes,
                    // then gives up. Clear the stamp so that continuation runs.
                    if let Err(release_err) = sqlx::query(
                        r#"
                        UPDATE outgoing_webhook_deliveries
                        SET retry_claimed_at = NULL
                        WHERE id = $1
                          AND retry_claim_token = $2
                          AND status = 'failed'
                        "#,
                    )
                    .bind(&delivery_id)
                    .bind(claim.token)
                    .execute(&self.pool)
                    .await
                    {
                        warn!(
                            error = %release_err,
                            webhook_id,
                            delivery_id,
                            "Failed to release webhook retry claim"
                        );
                    }
                    Recorded::Released
                }
            }
        } else {
            let inserted = sqlx::query(
                r#"
                INSERT INTO outgoing_webhook_deliveries (
                    id, webhook_id, event, status, payload,
                    status_code, response_body, error, attempts,
                    created_at, delivered_at
                )
                VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)
                ON CONFLICT (id) DO UPDATE SET
                    status = EXCLUDED.status,
                    status_code = EXCLUDED.status_code,
                    response_body = EXCLUDED.response_body,
                    error = EXCLUDED.error,
                    attempts = EXCLUDED.attempts,
                    delivered_at = EXCLUDED.delivered_at
                "#,
            )
            .bind(&delivery_id)
            .bind(webhook_id)
            .bind(request_payload["event"].as_str().unwrap_or("unknown"))
            .bind(status)
            .bind(request_payload)
            .bind(log.status_code)
            .bind(&log.response_body)
            .bind(&log.error)
            .bind(attempt_count)
            .bind(started_at)
            .bind(Utc::now())
            .execute(&self.pool)
            .await;
            if let Err(e) = inserted {
                warn!(error = %e, webhook_id, delivery_id, "Failed to record webhook delivery attempt");
            }
            Recorded::Applied
        };
        if !matches!(recorded, Recorded::Applied) {
            return recorded;
        }

        // Keep webhook summary fields current for dashboard / list views.
        // `failure_count` is deliberately NOT touched here: this runs once per
        // ATTEMPT, and the counter must move once per DELIVERY. It is owned by
        // `record_delivery_outcome`, which also owns the auto-disable breaker.
        //
        // A manual retry forces `enabled` only on the in-memory copy it sends
        // with. Writing `success` or `failed` over `auto_disabled` made the
        // dashboard drop the re-enable banner (it keys off that marker) while
        // `fan_out` still requires `enabled = true`, so later events were not
        // delivered. The marker stays only while the webhook is disabled: the
        // PUT that re-enables leaves it for the next delivery to overwrite, and
        // nothing else writes last_status.
        let summary = sqlx::query(
            r#"
            UPDATE outgoing_webhooks
            SET last_triggered_at = NOW(),
                last_status = CASE
                    WHEN last_status = $3 AND NOT enabled THEN last_status
                    ELSE $2
                END,
                updated_at = NOW()
            WHERE id = $1
            "#,
        )
        .bind(webhook_id)
        .bind(status)
        .bind(AUTO_DISABLED_STATUS)
        .execute(&self.pool)
        .await;
        if let Err(e) = summary {
            warn!(error = %e, webhook_id, "Failed to update webhook last status");
        }
        Recorded::Applied
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
    /// which is async, does not follow redirects, and — unlike a check performed here
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

/// Whether a claimed delivery-row write landed.
enum Recorded {
    /// The row now stores this attempt. The caller owns the result.
    Applied,
    /// Another retry owns the row.
    Superseded,
    /// This attempt held the claim, the write failed, and the claim was cleared.
    Released,
}

/// Map a row-write result onto the attempt the caller should act on.
///
/// A failed POST whose write did not land is retried (`Released`): the claim
/// is already clear, so the next claim can succeed. A success is not sent
/// again. A rejected URL stays rejected so the breaker still sees it.
fn settle(recorded: Recorded, applied: Attempt) -> Attempt {
    match recorded {
        Recorded::Applied => applied,
        Recorded::Superseded => Attempt::Superseded,
        Recorded::Released => match applied {
            Attempt::Failed => Attempt::Released,
            Attempt::Rejected => Attempt::Rejected,
            Attempt::Delivered | Attempt::Superseded | Attempt::Released => Attempt::Superseded,
        },
    }
}

/// What one attempt leaves in the delivery log: the HTTP status (`None` when
/// no response arrived), the start of the receiver's answer, and the reason it
/// failed.
struct AttemptLog {
    status_code: Option<i32>,
    response_body: Option<String>,
    error: Option<String>,
}

impl AttemptLog {
    fn error(text: String) -> Self {
        Self {
            status_code: None,
            response_body: None,
            error: Some(clean_text(text, MAX_STORED_ERROR_CHARS)),
        }
    }
}

/// Read at most `limit` bytes of a response as text. Reading to the end of a
/// short body also lets the connection go back to the pool. `None` if empty.
async fn read_capped_body(mut response: reqwest::Response, limit: usize) -> Option<String> {
    let mut buf = Vec::new();
    while buf.len() < limit {
        match response.chunk().await {
            Ok(Some(chunk)) => {
                let take = chunk.len().min(limit - buf.len());
                buf.extend_from_slice(&chunk[..take]);
            }
            Ok(None) | Err(_) => break,
        }
    }
    let mut text = String::from_utf8_lossy(&buf).replace('\0', "");
    // A character cut at the limit decodes to U+FFFD, which is wider.
    while text.len() > limit {
        text.pop();
    }
    (!text.is_empty()).then_some(text)
}

/// A send error with its causes: reqwest's own message ("error sending
/// request") says little, the chain has the useful part ("connection
/// refused", "operation timed out"). The URL is left out; the user has it.
fn describe_send_error(e: reqwest::Error) -> String {
    let e = e.without_url();
    let mut text = e.to_string();
    let mut source = std::error::Error::source(&e);
    while let Some(cause) = source {
        let cause_text = cause.to_string();
        if !text.ends_with(&cause_text) {
            text.push_str(": ");
            text.push_str(&cause_text);
        }
        source = cause.source();
    }
    text
}

/// Postgres TEXT cannot hold NUL; keep at most `max_chars` characters.
fn clean_text(text: String, max_chars: usize) -> String {
    let mut text = text.replace('\0', "");
    if let Some((cut, _)) = text.char_indices().nth(max_chars) {
        text.truncate(cut);
    }
    text
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

/// Bounds on per-job `completion_webhook` deliveries.
///
/// They share the org-webhook client but have their own request slots and a
/// per-org share of each bound, the same shape as org webhooks' per-webhook
/// lanes. One tenant whose completion URL never answers then holds at most a
/// quarter of the completion slots and of the pending bound, and nothing an
/// org-webhook consumer waits on.
struct CompletionGate {
    /// One client, so one connection pool and resolver, instead of a new
    /// client (and a cold connection) per delivery.
    client: Client,
    limits: DeliveryLimits,
    /// Completion requests in flight process-wide.
    slots: Arc<Semaphore>,
    /// Completion deliveries admitted and not finished; past this they are
    /// dropped instead of parking a task each on `slots`.
    pending: Arc<Semaphore>,
    lanes: Mutex<HashMap<String, Arc<Lane>>>,
    dropped: AtomicU64,
}

impl CompletionGate {
    fn new(client: Client, limits: DeliveryLimits) -> Self {
        let limits = limits.normalized();
        Self {
            client,
            limits,
            slots: Arc::new(Semaphore::new(limits.max_concurrent)),
            pending: Arc::new(Semaphore::new(limits.queue_capacity)),
            lanes: Mutex::new(HashMap::new()),
            dropped: AtomicU64::new(0),
        }
    }

    /// The org's lane and one unit of its pending share, if it has any left.
    fn lane_share(&self, organization_id: &str) -> Option<(Arc<Lane>, OwnedSemaphorePermit)> {
        let mut lanes = self.lanes.lock().unwrap_or_else(|e| e.into_inner());
        if lanes.len() >= LANE_PRUNE_THRESHOLD && !lanes.contains_key(organization_id) {
            let full_share = self.limits.per_webhook_queue;
            lanes.retain(|_, lane| lane.share.available_permits() < full_share);
        }
        let lane = lanes.entry(organization_id.to_string()).or_insert_with(|| {
            Arc::new(Lane {
                slots: Arc::new(Semaphore::new(self.limits.per_webhook_concurrency)),
                share: Arc::new(Semaphore::new(self.limits.per_webhook_queue)),
            })
        });
        let share = Arc::clone(&lane.share).try_acquire_owned().ok()?;
        Some((Arc::clone(lane), share))
    }

    fn note_drop(&self, reason: &str, organization_id: &str, event: &str) {
        let dropped = self.dropped.fetch_add(1, Ordering::Relaxed) + 1;
        if dropped == 1 || dropped.is_multiple_of(100) {
            warn!(
                reason,
                org_id = organization_id,
                event,
                dropped_total = dropped,
                "completion_webhook dropped"
            );
        } else {
            debug!(
                reason,
                org_id = organization_id,
                event,
                "completion_webhook dropped"
            );
        }
    }
}

static COMPLETION_GATE: OnceLock<CompletionGate> = OnceLock::new();

/// The gate built by the first [`OutgoingWebhookService`], or a default one if
/// a completion webhook fires before any service exists.
fn completion_gate() -> &'static CompletionGate {
    COMPLETION_GATE.get_or_init(|| {
        CompletionGate::new(
            build_outbound_http_client(
                Duration::from_secs(10),
                Duration::from_secs(5),
                COMPLETION_WEBHOOK_USER_AGENT,
            ),
            DeliveryLimits::from_concurrency(
                crate::config::OutgoingWebhookSettings::default().max_concurrent_deliveries,
            ),
        )
    })
}

/// Completion webhooks dropped because too many were pending, since start.
pub fn completion_webhooks_dropped() -> u64 {
    // Read without building the gate: a metrics scrape before the first service
    // must not install the default gate in place of the service's.
    COMPLETION_GATE
        .get()
        .map_or(0, |gate| gate.dropped.load(Ordering::Relaxed))
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
///
/// Bounded by [`CompletionGate`]: when the org's share, or the process-wide
/// bound, of pending deliveries is used up it is dropped and counted
/// ([`completion_webhooks_dropped`]) rather than queued without bound.
pub fn spawn_completion_webhook(
    organization_id: &str,
    url: String,
    event: String,
    payload: serde_json::Value,
    secret: Option<String>,
) {
    let gate = completion_gate();
    let Some((lane, share)) = gate.lane_share(organization_id) else {
        gate.note_drop("org backlog full", organization_id, &event);
        return;
    };
    let Ok(pending) = Arc::clone(&gate.pending).try_acquire_owned() else {
        gate.note_drop("too many pending", organization_id, &event);
        return;
    };
    tokio::spawn(async move {
        let _admitted = (share, pending);
        if let Err(e) =
            deliver_completion_webhook(gate, &lane, &url, &event, &payload, secret.as_deref()).await
        {
            warn!(error = %e, url = %url, event = %event, "completion_webhook delivery failed");
        }
    });
}

async fn deliver_completion_webhook(
    gate: &CompletionGate,
    lane: &Lane,
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

    let delivery_id = uuid::Uuid::new_v4();
    let body = serde_json::json!({
        "id": delivery_id,
        "event": event,
        "created_at": Utc::now(),
        "data": payload,
    });
    let payload_json = serde_json::to_string(&body).unwrap_or_default();

    let mut request_builder = gate
        .client
        .post(url)
        .header(reqwest::header::USER_AGENT, COMPLETION_WEBHOOK_USER_AGENT)
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

    // The org's slot first: a backlog waits on its own org's lane, so at most
    // `per_webhook_concurrency` of one org's deliveries are ever ahead of
    // another org's in the line for `gate.slots`.
    let _org_slot = lane
        .slots
        .acquire()
        .await
        .context("Completion webhook semaphore closed")?;
    let _slot = gate
        .slots
        .acquire()
        .await
        .context("Completion webhook semaphore closed")?;
    let resp = request_builder.body(payload_json).send().await?;
    let status = resp.status();
    // Drain a short answer so the connection is reused.
    read_capped_body(resp, MAX_STORED_RESPONSE_BODY_BYTES).await;
    if !status.is_success() {
        anyhow::bail!("HTTP {}", status);
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
    fn delivery_limits_give_one_webhook_a_quarter() {
        assert_eq!(
            DeliveryLimits::from_concurrency(64),
            DeliveryLimits {
                max_concurrent: 64,
                queue_capacity: 1024,
                per_webhook_concurrency: 16,
                per_webhook_queue: 256,
            }
        );
        assert_eq!(
            DeliveryLimits::from_concurrency(0),
            DeliveryLimits {
                max_concurrent: 1,
                queue_capacity: 16,
                per_webhook_concurrency: 1,
                per_webhook_queue: 4,
            }
        );
        assert_eq!(
            DeliveryLimits::from_concurrency(usize::MAX).max_concurrent,
            MAX_CONSUMERS
        );
        let odd = DeliveryLimits {
            max_concurrent: 2,
            queue_capacity: 0,
            per_webhook_concurrency: 9,
            per_webhook_queue: 9,
        }
        .normalized();
        assert_eq!(
            (
                odd.queue_capacity,
                odd.per_webhook_concurrency,
                odd.per_webhook_queue
            ),
            (1, 2, 1)
        );
    }

    #[test]
    fn completion_gate_has_its_own_slots_and_a_per_org_share() {
        let limits = DeliveryLimits::from_concurrency(4);
        let client = build_outbound_http_client(
            Duration::from_secs(1),
            Duration::from_secs(1),
            COMPLETION_WEBHOOK_USER_AGENT,
        );
        let gate = CompletionGate::new(client, limits);
        assert_eq!(gate.slots.available_permits(), 4);
        assert_eq!(gate.pending.available_permits(), 64);

        let busy: Vec<_> = (0..limits.per_webhook_queue)
            .map(|_| gate.lane_share("busy-org").expect("within the org's share"))
            .collect();
        assert!(
            gate.lane_share("busy-org").is_none(),
            "an org gets a quarter of the pending bound"
        );
        let (lane, _share) = gate
            .lane_share("other-org")
            .expect("another org is admitted");
        assert_eq!(lane.slots.available_permits(), 1, "one request per org");

        drop(busy);
        assert!(
            gate.lane_share("busy-org").is_some(),
            "the share comes back"
        );
    }

    #[test]
    fn clean_text_strips_nul_and_cuts_on_a_char_boundary() {
        assert_eq!(clean_text("a\0b".to_string(), 10), "ab");
        assert_eq!(clean_text("żółw".to_string(), 2), "żó");
        assert_eq!(clean_text("ok".to_string(), 10), "ok");
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
