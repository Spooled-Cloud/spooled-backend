//! Real-time API Handlers (WebSocket and SSE)
//!
//! This module provides real-time communication channels for:
//! - Job status updates
//! - Queue activity
//! - Worker heartbeats
//! - System events

use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        Extension, Path, Query, State,
    },
    http::StatusCode,
    response::{sse::Event, IntoResponse, Sse},
};
use futures::{
    stream::{self, Stream},
    SinkExt, StreamExt,
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tracing::{debug, error, info, warn};

use crate::api::AppState;

/// Maximum SSE connection duration (30 minutes)
const MAX_SSE_DURATION_SECS: u64 = 1800;

/// SSE poll interval for jobs (2 seconds)
const SSE_JOB_POLL_INTERVAL_SECS: u64 = 2;

/// SSE poll interval for queues (5 seconds)
const SSE_QUEUE_POLL_INTERVAL_SECS: u64 = 5;

/// WebSocket ping interval
/// Made configurable via constant
const WEBSOCKET_PING_INTERVAL_SECS: u64 = 30;

/// Event types for real-time updates
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", content = "data")]
pub enum RealtimeEvent {
    /// Job status changed
    JobStatusChange {
        job_id: String,
        queue_name: String,
        old_status: String,
        new_status: String,
        timestamp: chrono::DateTime<chrono::Utc>,
    },
    /// Job created
    JobCreated {
        job_id: String,
        queue_name: String,
        priority: i32,
        timestamp: chrono::DateTime<chrono::Utc>,
    },
    /// Job completed
    JobCompleted {
        job_id: String,
        queue_name: String,
        duration_ms: i64,
        timestamp: chrono::DateTime<chrono::Utc>,
        /// Optional job result (included if available at completion time)
        #[serde(skip_serializing_if = "Option::is_none")]
        result: Option<serde_json::Value>,
    },
    /// Job failed
    JobFailed {
        job_id: String,
        queue_name: String,
        error: String,
        retry_count: i32,
        timestamp: chrono::DateTime<chrono::Utc>,
    },
    /// Queue stats update
    QueueStats {
        queue_name: String,
        pending: i64,
        processing: i64,
        completed: i64,
        failed: i64,
        timestamp: chrono::DateTime<chrono::Utc>,
    },
    /// Worker heartbeat
    WorkerHeartbeat {
        worker_id: String,
        status: String,
        current_job_id: Option<String>,
        timestamp: chrono::DateTime<chrono::Utc>,
    },
    /// Worker registered
    WorkerRegistered {
        worker_id: String,
        queue_name: String,
        timestamp: chrono::DateTime<chrono::Utc>,
    },
    /// Worker deregistered
    WorkerDeregistered {
        worker_id: String,
        timestamp: chrono::DateTime<chrono::Utc>,
    },
    /// System health update
    SystemHealth {
        database: bool,
        redis: bool,
        timestamp: chrono::DateTime<chrono::Utc>,
    },
    /// Ping/Pong for keepalive
    Ping {
        timestamp: chrono::DateTime<chrono::Utc>,
    },
    /// Error event
    Error {
        message: String,
        timestamp: chrono::DateTime<chrono::Utc>,
    },
}

impl RealtimeEvent {
    fn queue_name(&self) -> Option<&str> {
        match self {
            Self::JobStatusChange { queue_name, .. }
            | Self::JobCreated { queue_name, .. }
            | Self::JobCompleted { queue_name, .. }
            | Self::JobFailed { queue_name, .. }
            | Self::QueueStats { queue_name, .. }
            | Self::WorkerRegistered { queue_name, .. } => Some(queue_name),
            _ => None,
        }
    }

    fn job_id(&self) -> Option<&str> {
        match self {
            Self::JobStatusChange { job_id, .. }
            | Self::JobCreated { job_id, .. }
            | Self::JobCompleted { job_id, .. }
            | Self::JobFailed { job_id, .. } => Some(job_id),
            Self::WorkerHeartbeat {
                current_job_id: Some(job_id),
                ..
            } => Some(job_id),
            _ => None,
        }
    }

    fn unverifiable_for_scoped_key(&self) -> bool {
        matches!(
            self,
            Self::WorkerHeartbeat { .. } | Self::WorkerDeregistered { .. }
        )
    }

    /// Get the event type as a string (for SSE event type)
    pub fn event_type(&self) -> &'static str {
        match self {
            RealtimeEvent::JobStatusChange { .. } => "job.status",
            RealtimeEvent::JobCreated { .. } => "job.created",
            RealtimeEvent::JobCompleted { .. } => "job.completed",
            RealtimeEvent::JobFailed { .. } => "job.failed",
            RealtimeEvent::QueueStats { .. } => "queue.stats",
            RealtimeEvent::WorkerHeartbeat { .. } => "worker.heartbeat",
            RealtimeEvent::WorkerRegistered { .. } => "worker.registered",
            RealtimeEvent::WorkerDeregistered { .. } => "worker.deregistered",
            RealtimeEvent::SystemHealth { .. } => "system.health",
            RealtimeEvent::Ping { .. } => "ping",
            RealtimeEvent::Error { .. } => "error",
        }
    }
}

// `EventBroadcaster` / `OrgScopedEvent` were removed here. They were never
// constructed outside their own unit tests — the live realtime path is Redis
// pub/sub plus DB polling in the handlers below — while their doc comments
// advertised org-scoped isolation ("Now requires organization_id to prevent
// cross-tenant leakage") that the code did not implement: `subscribe_for_org`
// ignored its argument and handed back a receiver on ONE global
// `broadcast::channel` shared by every tenant.
//
// If a broadcaster is wanted later, two things must be solved first, neither of
// which the deleted version did:
//   1. Per-org channels (or a per-org filter applied before the channel), so one
//      tenant's burst cannot evict another tenant's events from the ring buffer.
//   2. Explicit `RecvError::Lagged` handling, so a slow subscriber is told it
//      missed events instead of having its stream silently terminated.

/// Validate queue filter name for safe characters
fn validate_queue_filter(queue: &Option<String>) -> Result<(), &'static str> {
    if let Some(q) = queue {
        if q.len() > 100 {
            return Err("Queue filter too long");
        }
        if !q
            .chars()
            .all(|c| c.is_alphanumeric() || c == '-' || c == '_' || c == '.' || c == '*')
        {
            return Err("Queue filter contains invalid characters");
        }
    }
    Ok(())
}

/// Maximum WebSocket connections per organization
const MAX_WEBSOCKET_CONNECTIONS_PER_ORG: usize = 100;

type CurrentJobAuthRow = (String, String, Vec<String>);
type QueueAuthStatRow = (Vec<String>, Option<String>, i64);

/// Reload current realtime authorization, including organization lifecycle state.
/// Missing rows represent revoked/expired keys or soft-deleted organizations.
pub async fn load_current_realtime_scope(
    pool: &sqlx::PgPool,
    api_key_id: &str,
    organization_id: &str,
) -> Result<Option<Vec<String>>, sqlx::Error> {
    sqlx::query_scalar(
        r#"
        SELECT k.queues
        FROM api_keys k
        INNER JOIN organizations o ON o.id = k.organization_id
        WHERE k.id = $1
          AND k.organization_id = $2
          AND k.is_active = TRUE
          AND (k.expires_at IS NULL OR k.expires_at > NOW())
          AND o.plan_tier <> 'deleted'
        "#,
    )
    .bind(api_key_id)
    .bind(organization_id)
    .fetch_optional(pool)
    .await
}

/// Query parameters for WebSocket subscription
///
#[derive(Debug, Clone)]
struct SubscriptionFilter {
    queue: Option<String>,
    job_id: Option<String>,
}

fn event_matches_subscription(
    event: &RealtimeEvent,
    filter: &SubscriptionFilter,
    key_queues: &[String],
) -> bool {
    let unrestricted = key_queues.is_empty() || key_queues.iter().any(|queue| queue == "*");
    if !unrestricted && event.unverifiable_for_scoped_key() {
        return false;
    }
    if let Some(queue) = event.queue_name() {
        if !unrestricted && !key_queues.iter().any(|allowed| allowed == queue) {
            return false;
        }
        if filter
            .queue
            .as_deref()
            .is_some_and(|wanted| wanted != queue)
        {
            return false;
        }
    } else if filter.queue.is_some() {
        return false;
    }

    match (filter.job_id.as_deref(), event.job_id()) {
        (Some(wanted), Some(actual)) => wanted == actual,
        (Some(_), None) => false,
        _ => true,
    }
}

#[derive(Debug, Deserialize)]
pub struct SubscribeQuery {
    /// Filter by queue name (optional, validated for safe chars)
    /// Now validated for safe characters
    pub queue: Option<String>,
    /// Filter by job ID (optional)
    pub job_id: Option<String>,
    /// Filter by event types (comma-separated)
    pub events: Option<String>,
    /// Authentication token (required)
    /// WebSocket now requires authentication
    pub token: Option<String>,
}

/// WebSocket upgrade handler
///
/// Now requires authentication token in query parameter.
/// WebSocket cannot use headers for auth, so token must be in query string.
pub async fn websocket_handler(
    ws: WebSocketUpgrade,
    Extension(ctx): Extension<crate::models::ApiKeyContext>,
    Query(query): Query<SubscribeQuery>,
    State(state): State<AppState>,
) -> Result<impl IntoResponse, (StatusCode, &'static str)> {
    // Scope the stream to the organization the `authenticate_api_key` middleware already
    // authenticated (Bearer API key, or an ACCESS JWT / api_key supplied via query). Do
    // NOT re-decode `?token=` here: the previous in-handler decode used
    // `Validation::default()` with no token-type check, so it accepted REFRESH tokens
    // (which must never grant data access), and it scoped by the token's org instead of
    // the authenticated principal — letting a Bearer identity and a `?token` identity
    // disagree. The middleware already rejects refresh tokens, revoked/blacklisted
    // tokens, and inactive keys, so trusting its context (as the SSE handlers do) is the
    // single source of truth.
    let org_id = ctx.organization_id.clone();

    if query
        .queue
        .as_deref()
        .is_some_and(|queue| !ctx.can_access_queue(queue))
    {
        return Err((StatusCode::FORBIDDEN, "Queue is outside API key scope"));
    }

    // Validate queue filter
    if let Err(e) = validate_queue_filter(&query.queue) {
        return Err((StatusCode::BAD_REQUEST, e));
    }

    // Enforce the per-org connection limit with a READ-ONLY check here. The increment
    // used to happen at this point, before the upgrade — but the on_upgrade callback only
    // fires on a *successful* upgrade, so a client that aborted the handshake incremented
    // the counter with no matching decrement, and incr_with_ttl kept bumping the TTL so the
    // leaked count never expired and permanently locked the org out. The increment now
    // happens inside handle_websocket after the upgrade, paired with the decrement on close.
    if let Some(ref cache) = state.cache {
        let conn_key = format!("ws_connections:{}", org_id);
        if let Ok(Some(v)) = cache.get(&conn_key).await {
            if v.parse::<usize>()
                .map(|c| c >= MAX_WEBSOCKET_CONNECTIONS_PER_ORG)
                .unwrap_or(false)
            {
                warn!(
                    org_id = %org_id,
                    count = %v,
                    limit = MAX_WEBSOCKET_CONNECTIONS_PER_ORG,
                    "WebSocket connection limit exceeded"
                );
                return Err((
                    StatusCode::TOO_MANY_REQUESTS,
                    "Too many WebSocket connections",
                ));
            }
        }
    }

    info!(org_id = %org_id, "WebSocket connection requested");
    // Pass the key's queue scope so the event fan-out can enforce it (a scoped key
    // must never receive events for queues outside its scope).
    let key_queues = ctx.queues.clone();
    let api_key_id = ctx.api_key_id.clone();
    Ok(ws.on_upgrade(move |socket| {
        handle_websocket(socket, query, org_id, api_key_id, key_queues, state)
    }))
}

/// Handle WebSocket connection
///
/// Org_id is now String instead of Option<String> since auth is required.
/// `key_queues` is the authenticated key's queue scope (empty or `*` = all).
async fn handle_websocket(
    socket: WebSocket,
    query: SubscribeQuery,
    org_id: String,
    api_key_id: String,
    key_queues: Vec<String>,
    state: AppState,
) {
    let (mut sender, mut receiver) = socket.split();
    // Increment the per-org connection counter now that the upgrade succeeded (the limit
    // was checked read-only before the upgrade). Paired with the decrement in cleanup.
    if let Some(ref cache) = state.cache {
        let conn_key = format!("ws_connections:{}", org_id);
        let _ = cache.incr_with_ttl(&conn_key, 3600).await;
    }

    // Parse event filter
    let event_filter: Option<Vec<String>> = query
        .events
        .map(|e| e.split(',').map(|s| s.trim().to_lowercase()).collect());

    let subscription = Arc::new(tokio::sync::RwLock::new(SubscriptionFilter {
        queue: query.queue.clone(),
        job_id: query.job_id.clone(),
    }));
    let queue_filter = query.queue.clone();
    let job_filter = query.job_id.clone();

    info!(
        org_id = %org_id,
        queue = ?queue_filter,
        job_id = ?job_filter,
        events = ?event_filter,
        "WebSocket connection established"
    );

    // Set up Redis pub/sub subscription for real-time events
    let (event_tx, mut event_rx) = tokio::sync::mpsc::channel::<RealtimeEvent>(100);

    // Subscribe to Redis pub/sub for this organization's events
    let pubsub_task = if let Some(ref cache) = state.cache {
        let cache_clone = cache.clone();
        let org_id_clone = org_id.clone();
        let event_tx_clone = event_tx.clone();
        let api_key_id_clone = api_key_id.clone();
        let subscription_clone = subscription.clone();
        let auth_pool = state.db.pool().clone();

        // Spawn task to listen for Redis pub/sub messages
        Some(tokio::spawn(async move {
            // Subscribe to org-specific channel
            let channel = format!("org:{}:events", org_id_clone);
            match cache_clone.subscribe(&channel).await {
                Ok(mut pubsub) => {
                    use futures::StreamExt;

                    while let Some(msg) = pubsub.on_message().next().await {
                        if let Ok(payload) = msg.get_payload::<String>() {
                            // Try to parse as RealtimeEvent
                            if let Ok(event) = serde_json::from_str::<RealtimeEvent>(&payload) {
                                let current_scope = load_current_realtime_scope(
                                    &auth_pool,
                                    &api_key_id_clone,
                                    &org_id_clone,
                                )
                                .await
                                .unwrap_or(None);
                                let Some(scope) = current_scope.as_deref() else {
                                    break;
                                };
                                let filter = subscription_clone.read().await.clone();

                                if event_matches_subscription(&event, &filter, scope)
                                    && event_tx_clone.send(event).await.is_err()
                                {
                                    break; // Channel closed
                                }
                            }
                        }
                    }
                }
                Err(e) => {
                    warn!(error = %e, "Failed to subscribe to Redis pub/sub");
                }
            }
        }))
    } else {
        None
    };

    // Send initial ping
    let ping = RealtimeEvent::Ping {
        timestamp: chrono::Utc::now(),
    };
    if let Err(e) = sender
        .send(Message::Text(serde_json::to_string(&ping).unwrap().into()))
        .await
    {
        error!("Failed to send initial ping: {}", e);
        if let Some(t) = &pubsub_task {
            t.abort();
        }
        cleanup_websocket_connection(&state, &org_id).await;
        return;
    }

    // Spawn task to send periodic pings
    let ping_interval = std::time::Duration::from_secs(WEBSOCKET_PING_INTERVAL_SECS);
    let (ping_tx, mut ping_rx) = tokio::sync::mpsc::channel::<()>(1);

    let ping_task = tokio::spawn(async move {
        let mut interval = tokio::time::interval(ping_interval);
        loop {
            interval.tick().await;
            if ping_tx.send(()).await.is_err() {
                break;
            }
        }
    });

    // Main event loop
    loop {
        tokio::select! {
            // Handle incoming messages from client
            msg = receiver.next() => {
                match msg {
                    Some(Ok(Message::Text(text))) => {
                        debug!("Received text message: {}", text);
                        if let Ok(cmd) = serde_json::from_str::<ClientCommand>(&text) {
                            match cmd {
                                ClientCommand::Subscribe { queue, job_id } => {
                                    if queue
                                        .as_deref()
                                        .is_some_and(|requested| !key_queues.is_empty()
                                            && !key_queues.iter().any(|allowed| allowed == "*" || allowed == requested))
                                    {
                                        continue;
                                    }
                                    let mut filter = subscription.write().await;
                                    if queue.is_some() {
                                        filter.queue = queue;
                                    }
                                    if job_id.is_some() {
                                        filter.job_id = job_id;
                                    }
                                }
                                ClientCommand::Unsubscribe { queue, job_id } => {
                                    let mut filter = subscription.write().await;
                                    if queue.is_none() || filter.queue == queue {
                                        filter.queue = None;
                                    }
                                    if job_id.is_none() || filter.job_id == job_id {
                                        filter.job_id = None;
                                    }
                                }
                                ClientCommand::Ping => {
                                    let pong = RealtimeEvent::Ping {
                                        timestamp: chrono::Utc::now(),
                                    };
                                    if sender.send(Message::Text(serde_json::to_string(&pong).unwrap().into())).await.is_err() {
                                        break;
                                    }
                                }
                            }
                        }
                    }
                    Some(Ok(Message::Ping(data)))
                        if sender.send(Message::Pong(data.clone())).await.is_err() =>
                    {
                        break;
                    }
                    Some(Ok(Message::Close(_))) => {
                        info!("Client closed WebSocket connection");
                        break;
                    }
                    Some(Err(e)) => {
                        warn!("WebSocket error: {}", e);
                        break;
                    }
                    None => {
                        break;
                    }
                    _ => {}
                }
            }
            // Handle Redis pub/sub events
            Some(event) = event_rx.recv() => {
                // Apply event type filter
                let event_type = event.event_type();
                let should_send = event_filter.as_ref().is_none_or(|filter| {
                    filter.iter().any(|f| f == event_type || f == "*")
                });

                if should_send {
                    let json = serde_json::to_string(&event).unwrap_or_default();
                    if sender.send(Message::Text(json.into())).await.is_err() {
                        break;
                    }
                }
            }
            // Handle periodic pings
            _ = ping_rx.recv() => {
                let ping = RealtimeEvent::Ping {
                    timestamp: chrono::Utc::now(),
                };
                if sender.send(Message::Text(serde_json::to_string(&ping).unwrap().into())).await.is_err() {
                    break;
                }
            }
        }
    }

    // Abort the background tasks so their Redis pub/sub connection and ping timer are
    // released immediately on disconnect. The pub/sub task blocks on on_message().next(),
    // so for an idle org (no events) it would otherwise never observe the dropped receiver
    // and would leak its Redis connection indefinitely.
    if let Some(t) = &pubsub_task {
        t.abort();
    }
    ping_task.abort();

    // Cleanup: decrement connection count
    cleanup_websocket_connection(&state, &org_id).await;
    info!(org_id = %org_id, "WebSocket connection closed");
}

/// Cleanup WebSocket connection - decrement connection count
async fn cleanup_websocket_connection(state: &AppState, org_id: &str) {
    if let Some(ref cache) = state.cache {
        let conn_key = format!("ws_connections:{}", org_id);
        if let Err(e) = cache.decr(&conn_key).await {
            warn!(org_id = %org_id, error = %e, "Failed to decrement WebSocket connection count");
        }
    }
}

/// Commands that clients can send via WebSocket
#[derive(Debug, Deserialize)]
#[serde(tag = "cmd")]
pub enum ClientCommand {
    /// Subscribe to specific queue/job updates
    Subscribe {
        queue: Option<String>,
        job_id: Option<String>,
    },
    /// Unsubscribe from specific queue/job updates
    Unsubscribe {
        queue: Option<String>,
        job_id: Option<String>,
    },
    /// Ping request
    Ping,
}

/// Terminal job statuses (stream should close when job reaches these)
const TERMINAL_STATUSES: &[&str] = &["completed", "failed", "deadletter", "cancelled"];

/// SSE stream for job updates
///
/// Now closes stream when job reaches terminal status
pub async fn sse_job_handler(
    Path(job_id): Path<String>,
    Extension(ctx): Extension<crate::models::ApiKeyContext>,
    State(state): State<AppState>,
) -> crate::error::AppResult<Sse<impl Stream<Item = Result<Event, std::io::Error>>>> {
    let org_id = ctx.organization_id.clone();
    info!(job_id = %job_id, org_id = %org_id, "SSE connection for job");

    // Track connection start time to close idle connections
    let start_time = std::time::Instant::now();

    // Emit an SSE comment immediately so the response headers + first body
    // chunk flush right away. Without this, proxies (Cloudflare in particular)
    // buffer the response until the first poll cycle (2s), which makes the
    // connection look dead to clients with short timeouts.
    let initial =
        stream::once(async { Ok::<_, std::io::Error>(Event::default().comment("connected")) });

    // Fail closed if the authorization lookup fails. A missing/out-of-scope job uses
    // the same closed stream behavior so its existence is not revealed.
    let queue_name: Option<String> =
        sqlx::query_scalar("SELECT queue_name FROM jobs WHERE id = $1 AND organization_id = $2")
            .bind(&job_id)
            .bind(&org_id)
            .fetch_optional(state.db.pool())
            .await?;
    let deny = queue_name
        .as_deref()
        .is_none_or(|queue| !ctx.can_access_queue(queue));

    // Create a stream that polls for job status changes (filtered by org)
    let body = stream::unfold(
        (
            job_id.clone(),
            org_id.clone(),
            state.clone(),
            ctx.api_key_id.clone(),
            None::<String>,
            deny,
            start_time,
        ),
        |(job_id, org_id, state, api_key_id, last_status, should_close, start_time)| async move {
            // Stop polling if we should close
            if should_close {
                return None;
            }

            // Close connection after max duration to prevent resource waste
            if start_time.elapsed().as_secs() > MAX_SSE_DURATION_SECS {
                tracing::debug!(job_id = %job_id, "SSE job stream timeout - closing");
                return None;
            }

            // Poll interval
            tokio::time::sleep(std::time::Duration::from_secs(SSE_JOB_POLL_INTERVAL_SECS)).await;

            // Query current job status with organization filter
            let result: Result<Option<CurrentJobAuthRow>, sqlx::Error> = sqlx::query_as(
                r#"
                    SELECT j.status, j.queue_name, k.queues
                    FROM jobs j
                    INNER JOIN api_keys k ON k.id = $3 AND k.organization_id = j.organization_id
                    INNER JOIN organizations o ON o.id = k.organization_id
                    WHERE j.id = $1
                      AND j.organization_id = $2
                      AND k.is_active = TRUE
                      AND (k.expires_at IS NULL OR k.expires_at > NOW())
                      AND o.plan_tier <> 'deleted'
                    "#,
            )
            .bind(&job_id)
            .bind(&org_id)
            .bind(&api_key_id)
            .fetch_optional(state.db.pool())
            .await;

            match result {
                Ok(Some((status, queue_name, queues))) => {
                    let unrestricted = queues.is_empty() || queues.iter().any(|q| q == "*");
                    if !unrestricted && !queues.iter().any(|q| q == &queue_name) {
                        return None;
                    }
                    // Check if job reached terminal status
                    let is_terminal = TERMINAL_STATUSES.contains(&status.as_str());

                    if last_status.as_deref() != Some(&status) {
                        // Status changed, emit event
                        let event = RealtimeEvent::JobStatusChange {
                            job_id: job_id.clone(),
                            queue_name,
                            old_status: last_status.clone().unwrap_or_default(),
                            new_status: status.clone(),
                            timestamp: chrono::Utc::now(),
                        };
                        let json = serde_json::to_string(&event).unwrap_or_default();
                        Some((
                            Ok(Event::default().event("job.status").data(json)),
                            (
                                job_id,
                                org_id,
                                state,
                                api_key_id,
                                Some(status),
                                is_terminal,
                                start_time,
                            ),
                        ))
                    } else if is_terminal {
                        // Job is terminal but status didn't change - close stream
                        None
                    } else {
                        // No change, send keepalive comment
                        Some((
                            Ok(Event::default().comment("keepalive")),
                            (
                                job_id,
                                org_id,
                                state,
                                api_key_id,
                                last_status,
                                false,
                                start_time,
                            ),
                        ))
                    }
                }
                Ok(None) => {
                    // Job not found (or doesn't belong to this org) - close stream
                    let event = RealtimeEvent::Error {
                        message: "Job not found".to_string(), // Don't leak job_id
                        timestamp: chrono::Utc::now(),
                    };
                    let json = serde_json::to_string(&event).unwrap_or_default();
                    Some((
                        Ok(Event::default().event("error").data(json)),
                        (
                            job_id,
                            org_id,
                            state,
                            api_key_id,
                            last_status,
                            true,
                            start_time,
                        ), // Close on not found
                    ))
                }
                Err(e) => {
                    error!(error = %e, job_id = %job_id, "Database error in SSE authorization poll");
                    Some((
                        Err(std::io::Error::other("SSE authorization check failed")),
                        (
                            job_id,
                            org_id,
                            state,
                            api_key_id,
                            last_status,
                            true,
                            start_time,
                        ),
                    ))
                }
            }
        },
    );

    Ok(Sse::new(initial.chain(body)).keep_alive(
        axum::response::sse::KeepAlive::new()
            .interval(std::time::Duration::from_secs(15))
            .text("ping"),
    ))
}

/// SSE stream for queue updates
///
/// Now requires authentication and filters by organization.
/// Previously returned stats for any queue without checking org isolation.
pub async fn sse_queue_handler(
    Path(queue_name): Path<String>,
    Extension(ctx): Extension<crate::models::ApiKeyContext>,
    State(state): State<AppState>,
) -> Sse<impl Stream<Item = Result<Event, std::io::Error>>> {
    let org_id = ctx.organization_id.clone();
    let api_key_id = ctx.api_key_id.clone();
    info!(queue_name = %queue_name, org_id = %org_id, "SSE connection for queue");

    // Track connection start time for timeout
    let start_time = std::time::Instant::now();

    // Flush an initial SSE comment so headers/first chunk make it past
    // proxies before the first 5s poll cycle. See sse_job_handler for context.
    let initial =
        stream::once(async { Ok::<_, std::io::Error>(Event::default().comment("connected")) });

    // Queue-scope guard: a restricted key must not stream stats for a queue outside
    // its scope. If denied, the stream sends only the initial comment and closes.
    let deny = !ctx.is_unrestricted() && !ctx.can_access_queue(&queue_name);

    // Create a stream that polls for queue stats (filtered by org)
    let body = stream::unfold(
        (
            queue_name.clone(),
            org_id.clone(),
            state.clone(),
            api_key_id,
            start_time,
            deny,
        ),
        |(queue_name, org_id, state, api_key_id, start_time, deny)| async move {
            // Deny out-of-scope queues (close immediately after the initial comment).
            if deny {
                return None;
            }
            // Close connection after max duration
            if start_time.elapsed().as_secs() > MAX_SSE_DURATION_SECS {
                tracing::debug!(queue = %queue_name, "SSE queue stream timeout - closing");
                return None;
            }

            // Poll interval
            tokio::time::sleep(std::time::Duration::from_secs(SSE_QUEUE_POLL_INTERVAL_SECS)).await;

            // Reload the key and aggregate stats in one statement so revocation,
            // expiry, and scope narrowing take effect before this protected poll.
            let stats: Result<Vec<QueueAuthStatRow>, sqlx::Error> = sqlx::query_as(
                r#"
                    SELECT k.queues, j.status, COUNT(j.id)
                    FROM api_keys k
                    INNER JOIN organizations o ON o.id = k.organization_id
                    LEFT JOIN jobs j
                      ON j.organization_id = k.organization_id
                     AND j.queue_name = $1
                    WHERE k.id = $2
                      AND k.organization_id = $3
                      AND k.is_active = TRUE
                      AND (k.expires_at IS NULL OR k.expires_at > NOW())
                      AND o.plan_tier <> 'deleted'
                    GROUP BY k.queues, j.status
                    "#,
            )
            .bind(&queue_name)
            .bind(&api_key_id)
            .bind(&org_id)
            .fetch_all(state.db.pool())
            .await;

            match stats {
                Ok(rows) if !rows.is_empty() => {
                    let current_queues = &rows[0].0;
                    let unrestricted = current_queues.is_empty()
                        || current_queues.iter().any(|allowed| allowed == "*");
                    if !unrestricted && !current_queues.iter().any(|allowed| allowed == &queue_name)
                    {
                        return None;
                    }

                    let mut pending = 0;
                    let mut processing = 0;
                    let mut completed = 0;
                    let mut failed = 0;

                    for (_, status, count) in rows {
                        match status.as_deref() {
                            Some("pending" | "scheduled") => pending += count,
                            Some("processing") => processing += count,
                            Some("completed") => completed += count,
                            Some("failed" | "deadletter") => failed += count,
                            _ => {}
                        }
                    }

                    let event = RealtimeEvent::QueueStats {
                        queue_name: queue_name.clone(),
                        pending,
                        processing,
                        completed,
                        failed,
                        timestamp: chrono::Utc::now(),
                    };
                    let json = serde_json::to_string(&event).unwrap_or_default();
                    Some((
                        Ok(Event::default().event("queue.stats").data(json)),
                        (queue_name, org_id, state, api_key_id, start_time, deny),
                    ))
                }
                Ok(_) => None,
                Err(e) => {
                    error!(error = %e, queue = %queue_name, "Database error in SSE authorization poll");
                    Some((
                        Err(std::io::Error::other("SSE authorization check failed")),
                        (queue_name, org_id, state, api_key_id, start_time, true),
                    ))
                }
            }
        },
    );

    Sse::new(initial.chain(body)).keep_alive(
        axum::response::sse::KeepAlive::new()
            .interval(std::time::Duration::from_secs(15))
            .text("ping"),
    )
}

/// SSE stream for all system events
///
/// to system health information which could be used for reconnaissance
pub async fn sse_events_handler(
    Extension(ctx): Extension<crate::models::ApiKeyContext>,
    Query(query): Query<SubscribeQuery>,
    State(state): State<AppState>,
) -> Sse<impl Stream<Item = Result<Event, std::convert::Infallible>>> {
    let org_id = ctx.organization_id.clone();
    let api_key_id = ctx.api_key_id.clone();
    info!(org_id = %org_id, "SSE connection for all events (authenticated)");

    // Parse event filter
    let event_filter: Option<Vec<String>> = query
        .events
        .map(|e| e.split(',').map(|s| s.trim().to_lowercase()).collect());

    // Track connection start time for timeout
    let start_time = std::time::Instant::now();

    // Flush an initial SSE comment so headers/first chunk make it past
    // proxies before the first 10s poll cycle. See sse_job_handler for context.
    let initial = stream::once(async {
        Ok::<_, std::convert::Infallible>(Event::default().comment("connected"))
    });

    // Create a stream that emits periodic health checks and simulated events
    let body = stream::unfold(
        (
            state.clone(),
            event_filter,
            org_id.clone(),
            api_key_id,
            0u64,
            start_time,
        ),
        |(state, event_filter, org_id, api_key_id, counter, start_time)| async move {
            // Close connection after max duration
            if start_time.elapsed().as_secs() > MAX_SSE_DURATION_SECS {
                tracing::debug!(org_id = %org_id, "SSE events stream timeout - closing");
                return None;
            }
            // Poll interval
            tokio::time::sleep(std::time::Duration::from_secs(10)).await;

            // Revalidate the key and organization before revealing system health.
            match load_current_realtime_scope(state.db.pool(), &api_key_id, &org_id).await {
                Ok(Some(_)) => {}
                Ok(None) => return None,
                Err(e) => {
                    error!(error = %e, org_id = %org_id, "SSE events authorization lookup failed");
                    return None;
                }
            }

            // Query system health
            let db_ok = sqlx::query("SELECT 1")
                .execute(state.db.pool())
                .await
                .is_ok();

            let redis_ok = if let Some(ref cache) = state.cache {
                cache.ping().await.is_ok()
            } else {
                false
            };

            let event = RealtimeEvent::SystemHealth {
                database: db_ok,
                redis: redis_ok,
                timestamp: chrono::Utc::now(),
            };

            // Check if event passes filter
            let should_emit = event_filter
                .as_ref()
                .is_none_or(|filter| filter.iter().any(|f| f == "system.health" || f == "*"));

            if should_emit {
                let json = serde_json::to_string(&event).unwrap_or_default();
                Some((
                    Ok(Event::default().event("system.health").data(json)),
                    (
                        state,
                        event_filter,
                        org_id,
                        api_key_id,
                        counter + 1,
                        start_time,
                    ),
                ))
            } else {
                // Send keepalive
                Some((
                    Ok(Event::default().comment("keepalive")),
                    (
                        state,
                        event_filter,
                        org_id,
                        api_key_id,
                        counter + 1,
                        start_time,
                    ),
                ))
            }
        },
    );

    Sse::new(initial.chain(body)).keep_alive(
        axum::response::sse::KeepAlive::new()
            .interval(std::time::Duration::from_secs(15))
            .text("ping"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_realtime_event_types() {
        let event = RealtimeEvent::JobCreated {
            job_id: "test".to_string(),
            queue_name: "queue".to_string(),
            priority: 0,
            timestamp: chrono::Utc::now(),
        };
        assert_eq!(event.event_type(), "job.created");

        let event = RealtimeEvent::JobCompleted {
            job_id: "test".to_string(),
            queue_name: "queue".to_string(),
            duration_ms: 100,
            timestamp: chrono::Utc::now(),
            result: None,
        };
        assert_eq!(event.event_type(), "job.completed");

        let event = RealtimeEvent::QueueStats {
            queue_name: "queue".to_string(),
            pending: 10,
            processing: 5,
            completed: 100,
            failed: 2,
            timestamp: chrono::Utc::now(),
        };
        assert_eq!(event.event_type(), "queue.stats");
    }

    #[test]
    fn test_event_serialization() {
        let event = RealtimeEvent::JobStatusChange {
            job_id: "job-123".to_string(),
            queue_name: "emails".to_string(),
            old_status: "pending".to_string(),
            new_status: "processing".to_string(),
            timestamp: chrono::Utc::now(),
        };

        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("\"type\":\"JobStatusChange\""));
        assert!(json.contains("\"job_id\":\"job-123\""));

        // Deserialize back
        let deserialized: RealtimeEvent = serde_json::from_str(&json).unwrap();
        match deserialized {
            RealtimeEvent::JobStatusChange { job_id, .. } => {
                assert_eq!(job_id, "job-123");
            }
            _ => panic!("Wrong event type"),
        }
    }

    #[test]
    fn test_scoped_subscription_filters_queue_job_and_worker_events() {
        let scope = vec!["allowed".to_string()];
        let filter = SubscriptionFilter {
            queue: Some("allowed".to_string()),
            job_id: Some("job-1".to_string()),
        };
        let allowed = RealtimeEvent::JobCreated {
            job_id: "job-1".to_string(),
            queue_name: "allowed".to_string(),
            priority: 0,
            timestamp: chrono::Utc::now(),
        };
        let wrong_job = RealtimeEvent::JobCreated {
            job_id: "job-2".to_string(),
            queue_name: "allowed".to_string(),
            priority: 0,
            timestamp: chrono::Utc::now(),
        };
        let heartbeat = RealtimeEvent::WorkerHeartbeat {
            worker_id: "worker".to_string(),
            status: "healthy".to_string(),
            current_job_id: Some("job-1".to_string()),
            timestamp: chrono::Utc::now(),
        };

        assert!(event_matches_subscription(&allowed, &filter, &scope));
        assert!(!event_matches_subscription(&wrong_job, &filter, &scope));
        assert!(!event_matches_subscription(&heartbeat, &filter, &scope));
        let job_only_filter = SubscriptionFilter {
            queue: None,
            job_id: Some("job-1".to_string()),
        };
        assert!(event_matches_subscription(
            &heartbeat,
            &job_only_filter,
            &[]
        ));
    }

    #[test]
    fn test_client_command_deserialization() {
        let json = r#"{"cmd":"Subscribe","queue":"emails","job_id":null}"#;
        let cmd: ClientCommand = serde_json::from_str(json).unwrap();
        match cmd {
            ClientCommand::Subscribe { queue, job_id } => {
                assert_eq!(queue, Some("emails".to_string()));
                assert!(job_id.is_none());
            }
            _ => panic!("Wrong command type"),
        }

        let json = r#"{"cmd":"Ping"}"#;
        let cmd: ClientCommand = serde_json::from_str(json).unwrap();
        assert!(matches!(cmd, ClientCommand::Ping));
    }

    #[test]
    fn test_all_event_types() {
        let events = vec![
            RealtimeEvent::JobStatusChange {
                job_id: "1".to_string(),
                queue_name: "q".to_string(),
                old_status: "a".to_string(),
                new_status: "b".to_string(),
                timestamp: chrono::Utc::now(),
            },
            RealtimeEvent::JobCreated {
                job_id: "1".to_string(),
                queue_name: "q".to_string(),
                priority: 0,
                timestamp: chrono::Utc::now(),
            },
            RealtimeEvent::JobCompleted {
                job_id: "1".to_string(),
                queue_name: "q".to_string(),
                duration_ms: 100,
                timestamp: chrono::Utc::now(),
                result: None,
            },
            RealtimeEvent::JobFailed {
                job_id: "1".to_string(),
                queue_name: "q".to_string(),
                error: "err".to_string(),
                retry_count: 1,
                timestamp: chrono::Utc::now(),
            },
            RealtimeEvent::QueueStats {
                queue_name: "q".to_string(),
                pending: 0,
                processing: 0,
                completed: 0,
                failed: 0,
                timestamp: chrono::Utc::now(),
            },
            RealtimeEvent::WorkerHeartbeat {
                worker_id: "w".to_string(),
                status: "healthy".to_string(),
                current_job_id: None,
                timestamp: chrono::Utc::now(),
            },
            RealtimeEvent::WorkerRegistered {
                worker_id: "w".to_string(),
                queue_name: "q".to_string(),
                timestamp: chrono::Utc::now(),
            },
            RealtimeEvent::WorkerDeregistered {
                worker_id: "w".to_string(),
                timestamp: chrono::Utc::now(),
            },
            RealtimeEvent::SystemHealth {
                database: true,
                redis: true,
                timestamp: chrono::Utc::now(),
            },
            RealtimeEvent::Ping {
                timestamp: chrono::Utc::now(),
            },
            RealtimeEvent::Error {
                message: "error".to_string(),
                timestamp: chrono::Utc::now(),
            },
        ];

        for event in events {
            // Ensure all events can be serialized
            let json = serde_json::to_string(&event).unwrap();
            assert!(!json.is_empty());

            // Ensure event_type returns a valid string
            let event_type = event.event_type();
            assert!(!event_type.is_empty());
        }
    }
}
