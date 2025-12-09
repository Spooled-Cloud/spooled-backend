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
use tokio::sync::broadcast;
use tracing::{debug, error, info, warn};

use crate::api::AppState;

/// Maximum number of events to buffer in broadcast channel
const BROADCAST_CAPACITY: usize = 1000;

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

/// Event with organization context
#[derive(Debug, Clone)]
pub struct OrgScopedEvent {
    /// Organization ID for filtering
    pub organization_id: String,
    /// The actual event
    pub event: RealtimeEvent,
}

/// Event broadcaster for real-time updates
///
/// Now supports organization-scoped event broadcasting
#[derive(Clone)]
pub struct EventBroadcaster {
    /// Sender for broadcasting events (now org-scoped)
    sender: broadcast::Sender<OrgScopedEvent>,
}

impl EventBroadcaster {
    /// Create a new event broadcaster
    pub fn new() -> Self {
        let (sender, _) = broadcast::channel(BROADCAST_CAPACITY);
        Self { sender }
    }

    /// Broadcast an event to all subscribers (with organization context)
    ///
    /// Now requires organization_id to prevent cross-tenant leakage
    pub fn broadcast(&self, organization_id: &str, event: RealtimeEvent) {
        let scoped_event = OrgScopedEvent {
            organization_id: organization_id.to_string(),
            event,
        };
        // Ignore errors if there are no receivers
        let _ = self.sender.send(scoped_event);
    }

    /// Subscribe to events (returns org-scoped events)
    ///
    /// Subscribers must filter by their organization_id
    pub fn subscribe(&self) -> broadcast::Receiver<OrgScopedEvent> {
        self.sender.subscribe()
    }

    /// Subscribe to events for a specific organization
    ///
    /// Convenience method that filters events by org
    pub fn subscribe_for_org(&self, _org_id: &str) -> broadcast::Receiver<OrgScopedEvent> {
        // Note: Actual filtering happens on receive side
        // In production, consider using separate channels per org for better performance
        self.sender.subscribe()
    }
}

impl Default for EventBroadcaster {
    fn default() -> Self {
        Self::new()
    }
}

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

/// Query parameters for WebSocket subscription
///
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
    Query(query): Query<SubscribeQuery>,
    State(state): State<AppState>,
) -> Result<impl IntoResponse, (StatusCode, &'static str)> {
    // Validate authentication token
    let org_id = match &query.token {
        Some(token) => {
            // Decode and validate JWT token
            use crate::api::handlers::auth::Claims;
            use jsonwebtoken::{decode, DecodingKey, Validation};

            let token_data = decode::<Claims>(
                token,
                &DecodingKey::from_secret(state.settings.jwt.secret.as_bytes()),
                &Validation::default(),
            )
            .map_err(|_| (StatusCode::UNAUTHORIZED, "Invalid or expired token"))?;

            // Check blacklist
            if let Some(ref cache) = state.cache {
                let blacklist_key = format!("token_blacklist:{}", token_data.claims.jti);
                if let Ok(Some(_)) = cache.get(&blacklist_key).await {
                    return Err((StatusCode::UNAUTHORIZED, "Token has been revoked"));
                }
            }

            Some(token_data.claims.org_id)
        }
        None => {
            return Err((StatusCode::UNAUTHORIZED, "Missing authentication token"));
        }
    };

    // Org_id is now guaranteed to be Some after successful auth
    let org_id = org_id.expect("org_id must be set after authentication");

    // Validate queue filter
    if let Err(e) = validate_queue_filter(&query.queue) {
        return Err((StatusCode::BAD_REQUEST, e));
    }

    // Implement connection counting per org using Redis
    if let Some(ref cache) = state.cache {
        let conn_key = format!("ws_connections:{}", org_id);
        match cache.incr_with_ttl(&conn_key, 3600).await {
            Ok(count) if count as usize > MAX_WEBSOCKET_CONNECTIONS_PER_ORG => {
                // Decrement since we're rejecting
                let _ = cache.decr(&conn_key).await;
                warn!(
                    org_id = %org_id,
                    count = count,
                    limit = MAX_WEBSOCKET_CONNECTIONS_PER_ORG,
                    "WebSocket connection limit exceeded"
                );
                return Err((
                    StatusCode::TOO_MANY_REQUESTS,
                    "Too many WebSocket connections",
                ));
            }
            Ok(count) => {
                debug!(org_id = %org_id, count = count, "WebSocket connection count incremented");
            }
            Err(e) => {
                // Log but allow connection if Redis is down
                warn!(org_id = %org_id, error = %e, "Failed to check WebSocket connection count");
            }
        }
    }

    info!(org_id = %org_id, "WebSocket connection requested");
    Ok(ws.on_upgrade(move |socket| handle_websocket(socket, query, org_id, state)))
}

/// Handle WebSocket connection
///
/// Org_id is now String instead of Option<String> since auth is required
async fn handle_websocket(
    socket: WebSocket,
    query: SubscribeQuery,
    org_id: String,
    state: AppState,
) {
    let (mut sender, mut receiver) = socket.split();

    // Parse event filter
    let event_filter: Option<Vec<String>> = query
        .events
        .map(|e| e.split(',').map(|s| s.trim().to_lowercase()).collect());

    // Clone queue filter for use in event matching
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
    if let Some(ref cache) = state.cache {
        let cache_clone = cache.clone();
        let org_id_clone = org_id.clone();
        let queue_filter_clone = queue_filter.clone();
        let event_tx_clone = event_tx.clone();

        // Spawn task to listen for Redis pub/sub messages
        tokio::spawn(async move {
            // Subscribe to org-specific channel
            let channel = format!("org:{}:events", org_id_clone);
            match cache_clone.subscribe(&channel).await {
                Ok(mut pubsub) => {
                    use futures::StreamExt;

                    while let Some(msg) = pubsub.on_message().next().await {
                        if let Ok(payload) = msg.get_payload::<String>() {
                            // Try to parse as RealtimeEvent
                            if let Ok(event) = serde_json::from_str::<RealtimeEvent>(&payload) {
                                // Filter by queue if specified
                                let should_send = match (&queue_filter_clone, &event) {
                                    (Some(q), RealtimeEvent::JobCreated { queue_name, .. }) => {
                                        queue_name == q
                                    }
                                    (Some(q), RealtimeEvent::JobCompleted { queue_name, .. }) => {
                                        queue_name == q
                                    }
                                    (Some(q), RealtimeEvent::JobFailed { queue_name, .. }) => {
                                        queue_name == q
                                    }
                                    (
                                        Some(q),
                                        RealtimeEvent::JobStatusChange { queue_name, .. },
                                    ) => queue_name == q,
                                    (Some(q), RealtimeEvent::QueueStats { queue_name, .. }) => {
                                        queue_name == q
                                    }
                                    (None, _) => true,
                                    _ => true,
                                };

                                if should_send && event_tx_clone.send(event).await.is_err() {
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
        });
    }

    // Send initial ping
    let ping = RealtimeEvent::Ping {
        timestamp: chrono::Utc::now(),
    };
    if let Err(e) = sender
        .send(Message::Text(serde_json::to_string(&ping).unwrap().into()))
        .await
    {
        error!("Failed to send initial ping: {}", e);
        cleanup_websocket_connection(&state, &org_id).await;
        return;
    }

    // Spawn task to send periodic pings
    let ping_interval = std::time::Duration::from_secs(WEBSOCKET_PING_INTERVAL_SECS);
    let (ping_tx, mut ping_rx) = tokio::sync::mpsc::channel::<()>(1);

    tokio::spawn(async move {
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
                                    info!("Client subscribing to queue: {:?}, job: {:?}", queue, job_id);
                                }
                                ClientCommand::Unsubscribe { queue, job_id } => {
                                    info!("Client unsubscribing from queue: {:?}, job: {:?}", queue, job_id);
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
                    Some(Ok(Message::Ping(data))) => {
                        if sender.send(Message::Pong(data)).await.is_err() {
                            break;
                        }
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
) -> Sse<impl Stream<Item = Result<Event, std::convert::Infallible>>> {
    let org_id = ctx.organization_id.clone();
    info!(job_id = %job_id, org_id = %org_id, "SSE connection for job");

    // Track connection start time to close idle connections
    let start_time = std::time::Instant::now();

    // Create a stream that polls for job status changes (filtered by org)
    let stream = stream::unfold(
        (
            job_id.clone(),
            org_id.clone(),
            state.clone(),
            None::<String>,
            false,
            start_time,
        ),
        |(job_id, org_id, state, last_status, should_close, start_time)| async move {
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
            let result: Result<Option<(String, String)>, sqlx::Error> = sqlx::query_as(
                "SELECT status, queue_name FROM jobs WHERE id = $1 AND organization_id = $2",
            )
            .bind(&job_id)
            .bind(&org_id)
            .fetch_optional(state.db.pool())
            .await;

            match result {
                Ok(Some((status, queue_name))) => {
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
                            (job_id, org_id, state, Some(status), is_terminal, start_time),
                        ))
                    } else if is_terminal {
                        // Job is terminal but status didn't change - close stream
                        None
                    } else {
                        // No change, send keepalive comment
                        Some((
                            Ok(Event::default().comment("keepalive")),
                            (job_id, org_id, state, last_status, false, start_time),
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
                        (job_id, org_id, state, last_status, true, start_time), // Close on not found
                    ))
                }
                Err(e) => {
                    error!("Database error in SSE stream: {}", e);
                    let event = RealtimeEvent::Error {
                        message: "Database error".to_string(),
                        timestamp: chrono::Utc::now(),
                    };
                    let json = serde_json::to_string(&event).unwrap_or_default();
                    Some((
                        Ok(Event::default().event("error").data(json)),
                        (job_id, org_id, state, last_status, false, start_time), // Keep trying on transient errors
                    ))
                }
            }
        },
    );

    Sse::new(stream).keep_alive(
        axum::response::sse::KeepAlive::new()
            .interval(std::time::Duration::from_secs(15))
            .text("ping"),
    )
}

/// SSE stream for queue updates
///
/// Now requires authentication and filters by organization.
/// Previously returned stats for any queue without checking org isolation.
pub async fn sse_queue_handler(
    Path(queue_name): Path<String>,
    Extension(ctx): Extension<crate::models::ApiKeyContext>,
    State(state): State<AppState>,
) -> Sse<impl Stream<Item = Result<Event, std::convert::Infallible>>> {
    let org_id = ctx.organization_id.clone();
    info!(queue_name = %queue_name, org_id = %org_id, "SSE connection for queue");

    // Track connection start time for timeout
    let start_time = std::time::Instant::now();

    // Create a stream that polls for queue stats (filtered by org)
    let stream = stream::unfold(
        (
            queue_name.clone(),
            org_id.clone(),
            state.clone(),
            start_time,
        ),
        |(queue_name, org_id, state, start_time)| async move {
            // Close connection after max duration
            if start_time.elapsed().as_secs() > MAX_SSE_DURATION_SECS {
                tracing::debug!(queue = %queue_name, "SSE queue stream timeout - closing");
                return None;
            }

            // Poll interval
            tokio::time::sleep(std::time::Duration::from_secs(SSE_QUEUE_POLL_INTERVAL_SECS)).await;

            // Query queue stats - now filtered by organization
            let stats: Result<Vec<(String, i64)>, sqlx::Error> = sqlx::query_as(
                r#"
                SELECT status, COUNT(*) as count
                FROM jobs
                WHERE queue_name = $1 AND organization_id = $2
                GROUP BY status
                "#,
            )
            .bind(&queue_name)
            .bind(&org_id)
            .fetch_all(state.db.pool())
            .await;

            match stats {
                Ok(rows) => {
                    let mut pending = 0;
                    let mut processing = 0;
                    let mut completed = 0;
                    let mut failed = 0;

                    for (status, count) in rows {
                        match status.as_str() {
                            "pending" | "scheduled" => pending += count,
                            "processing" => processing += count,
                            "completed" => completed += count,
                            "failed" | "deadletter" => failed += count,
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
                        (queue_name, org_id, state, start_time),
                    ))
                }
                Err(e) => {
                    error!("Database error in SSE stream: {}", e);
                    let event = RealtimeEvent::Error {
                        message: "Database error".to_string(),
                        timestamp: chrono::Utc::now(),
                    };
                    let json = serde_json::to_string(&event).unwrap_or_default();
                    Some((
                        Ok(Event::default().event("error").data(json)),
                        (queue_name, org_id, state, start_time),
                    ))
                }
            }
        },
    );

    Sse::new(stream).keep_alive(
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
    info!(org_id = %org_id, "SSE connection for all events (authenticated)");

    // Parse event filter
    let event_filter: Option<Vec<String>> = query
        .events
        .map(|e| e.split(',').map(|s| s.trim().to_lowercase()).collect());

    // Track connection start time for timeout
    let start_time = std::time::Instant::now();

    // Create a stream that emits periodic health checks and simulated events
    let stream = stream::unfold(
        (
            state.clone(),
            event_filter,
            org_id.clone(),
            0u64,
            start_time,
        ),
        |(state, event_filter, org_id, counter, start_time)| async move {
            // Close connection after max duration
            if start_time.elapsed().as_secs() > MAX_SSE_DURATION_SECS {
                tracing::debug!(org_id = %org_id, "SSE events stream timeout - closing");
                return None;
            }
            // Poll interval
            tokio::time::sleep(std::time::Duration::from_secs(10)).await;

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
                    (state, event_filter, org_id, counter + 1, start_time),
                ))
            } else {
                // Send keepalive
                Some((
                    Ok(Event::default().comment("keepalive")),
                    (state, event_filter, org_id, counter + 1, start_time),
                ))
            }
        },
    );

    Sse::new(stream).keep_alive(
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

    /// Updated test to use org-scoped broadcast
    #[test]
    fn test_event_broadcaster() {
        let broadcaster = EventBroadcaster::new();
        let mut rx = broadcaster.subscribe();

        let event = RealtimeEvent::Ping {
            timestamp: chrono::Utc::now(),
        };
        // Use org-scoped broadcast
        broadcaster.broadcast("test-org", event.clone());

        // Should receive the event
        let received = rx.try_recv();
        assert!(received.is_ok());
        // Verify org_id is set
        let scoped_event = received.unwrap();
        assert_eq!(scoped_event.organization_id, "test-org");
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
