//! Real-time API Handlers (WebSocket and SSE)
//!
//! This module provides real-time communication channels for:
//! - Job status updates
//! - Queue activity
//! - Worker heartbeats
//! - System events

use axum::{
    extract::{
        ws::{close_code, CloseFrame, Message, WebSocket, WebSocketUpgrade},
        Extension, Path, Query, RawQuery, State,
    },
    http::{header::AUTHORIZATION, HeaderMap, StatusCode},
    response::{sse::Event, IntoResponse, Sse},
};
use futures::{
    stream::{SplitSink, Stream},
    SinkExt, StreamExt,
};
use jsonwebtoken::{decode, DecodingKey, Validation};
use redis::AsyncCommands;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::future::Future;
use std::sync::{Arc, LazyLock, Mutex, PoisonError};
use std::time::Duration;
use tokio::time::Instant;
use tracing::{debug, error, info, warn};

use crate::api::handlers::auth::{is_token_revoked, Claims};
use crate::api::handlers::require_queue_access;
use crate::api::AppState;
use crate::cache::RedisCache;
use crate::error::{AppError, AppResult};
use crate::models::{queue_scope_allows, queue_scope_is_unrestricted, ApiKeyContext};

/// Maximum SSE connection duration (30 minutes)
const MAX_SSE_DURATION_SECS: u64 = 1800;

/// Maximum WebSocket connection duration (1 hour). The key and the session are
/// re-checked while the socket is open, but a socket must not live forever on a
/// single upgrade; clients reconnect after the normal close (1000).
const MAX_WEBSOCKET_DURATION_SECS: u64 = 3600;

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

/// Maximum concurrent global SSE streams (`/events`) per organization. Each one
/// holds a Redis pub/sub connection, so it is capped like WebSocket connections.
const MAX_SSE_CONNECTIONS_PER_ORG: usize = 100;

/// Maximum concurrent `/events/jobs/{id}` streams per organization. Each one
/// polls the database every [`SSE_JOB_POLL_INTERVAL_SECS`].
const MAX_SSE_JOB_STREAMS_PER_ORG: usize = 100;

/// Maximum concurrent `/events/queues/{name}` streams per organization.
const MAX_SSE_QUEUE_STREAMS_PER_ORG: usize = 100;

type CurrentJobAuthRow = (String, String, Vec<String>);

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

// ---------------------------------------------------------------------------
// Per-org connection caps
// ---------------------------------------------------------------------------

/// How often an open stream refreshes its heartbeat in the org's connection set.
const CONNECTION_HEARTBEAT_SECS: u64 = 30;

/// A connection whose heartbeat is older than this no longer counts toward the
/// cap: its process was killed (deploy SIGKILL, OOM, crash) before it could
/// remove itself. Three heartbeat intervals.
const CONNECTION_STALE_SECS: i64 = 90;

/// TTL of an org's connection set, refreshed on every admission and heartbeat,
/// so a set whose connections all died ungracefully disappears on its own.
const CONNECTION_SET_TTL_SECS: i64 = 2 * CONNECTION_STALE_SECS;

/// Upper bound for one connection-set call. On timeout or a Redis error the cap
/// fails open, as the counter it replaced did.
const CONNECTION_SET_TIMEOUT: Duration = Duration::from_millis(500);

/// The realtime stream kinds, each capped per organization on its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RealtimeStream {
    /// `GET /ws`
    WebSocket,
    /// `GET /events`
    Events,
    /// `GET /events/jobs/{id}`
    Job,
    /// `GET /events/queues/{name}`
    Queue,
}

impl RealtimeStream {
    fn label(self) -> &'static str {
        match self {
            Self::WebSocket => "ws",
            Self::Events => "events",
            Self::Job => "job",
            Self::Queue => "queue",
        }
    }

    fn limit(self) -> usize {
        match self {
            Self::WebSocket => MAX_WEBSOCKET_CONNECTIONS_PER_ORG,
            Self::Events => MAX_SSE_CONNECTIONS_PER_ORG,
            Self::Job => MAX_SSE_JOB_STREAMS_PER_ORG,
            Self::Queue => MAX_SSE_QUEUE_STREAMS_PER_ORG,
        }
    }
}

/// Redis sorted set of an org's open connections of one kind: one member per
/// connection, scored with its last heartbeat (unix seconds). The cap counts
/// members with a recent heartbeat only.
///
/// Before 0.1.121 this was a plain counter (`ws_connections:{org}` /
/// `sse_connections:{org}`) decremented on close. A replica killed before its
/// streams closed never decremented, and because every new connection refreshed
/// the counter's TTL, the phantom count of an active org never decayed: after
/// enough deploys the org was refused every realtime connection (BEAUTH-14).
pub fn realtime_connections_key(stream: RealtimeStream, org_id: &str) -> String {
    format!("realtime_connections:{}:{}", stream.label(), org_id)
}

/// Prune members whose heartbeat stopped, then admit the connection if the
/// org is under the cap. Returns `{admitted, live members}`.
static ADMIT_CONNECTION: LazyLock<redis::Script> = LazyLock::new(|| {
    redis::Script::new(
        r#"
        redis.call('ZREMRANGEBYSCORE', KEYS[1], '-inf', ARGV[2])
        local live = redis.call('ZCARD', KEYS[1])
        if live >= tonumber(ARGV[3]) then
            return {0, live}
        end
        redis.call('ZADD', KEYS[1], ARGV[1], ARGV[4])
        redis.call('EXPIRE', KEYS[1], ARGV[5])
        return {1, live + 1}
        "#,
    )
});

/// One admitted realtime connection: its member in the org's connection set.
///
/// Dropping the slot removes the member. A process that dies before that leaves
/// a member whose heartbeat stops, which stops counting after
/// [`CONNECTION_STALE_SECS`].
struct ConnectionSlot {
    /// `None` without Redis (nothing to count) or once released.
    cache: Option<RedisCache>,
    key: String,
    id: String,
    last_beat: Instant,
}

impl ConnectionSlot {
    /// Admit a connection of `stream` for `org_id`, or return the number of live
    /// connections when the org is at its cap.
    async fn admit(state: &AppState, stream: RealtimeStream, org_id: &str) -> Result<Self, usize> {
        let mut slot = Self {
            cache: state.cache.clone(),
            key: realtime_connections_key(stream, org_id),
            id: uuid::Uuid::new_v4().to_string(),
            last_beat: Instant::now(),
        };
        let Some(cache) = slot.cache.clone() else {
            return Ok(slot);
        };
        let now = chrono::Utc::now().timestamp();
        let admit = async {
            let mut conn = cache.get_connection().await?;
            let reply: (i64, i64) = ADMIT_CONNECTION
                .key(&slot.key)
                .arg(now)
                .arg(now - CONNECTION_STALE_SECS)
                .arg(stream.limit())
                .arg(&slot.id)
                .arg(CONNECTION_SET_TTL_SECS)
                .invoke_async(&mut conn)
                .await?;
            Ok::<_, anyhow::Error>(reply)
        };
        match tokio::time::timeout(CONNECTION_SET_TIMEOUT, admit).await {
            Ok(Ok((1, _))) => Ok(slot),
            Ok(Ok((_, live))) => {
                warn!(
                    org_id = %org_id,
                    stream = stream.label(),
                    live,
                    limit = stream.limit(),
                    "Realtime connection limit exceeded"
                );
                slot.cache = None;
                Err(live.max(0) as usize)
            }
            Ok(Err(e)) => {
                warn!(error = %e, org_id = %org_id, "Realtime connection cap unavailable; admitting");
                Ok(slot)
            }
            Err(_) => {
                warn!(org_id = %org_id, "Realtime connection cap timed out; admitting");
                Ok(slot)
            }
        }
    }

    /// Refresh this connection's heartbeat (re-adding it if it was pruned).
    async fn heartbeat(&mut self) {
        let Some(cache) = self.cache.clone() else {
            return;
        };
        self.last_beat = Instant::now();
        let now = chrono::Utc::now().timestamp();
        let beat = async {
            let mut conn = cache.get_connection().await?;
            redis::pipe()
                .zadd(&self.key, &self.id, now)
                .ignore()
                .expire(&self.key, CONNECTION_SET_TTL_SECS)
                .ignore()
                .query_async::<()>(&mut conn)
                .await?;
            Ok::<_, anyhow::Error>(())
        };
        if !matches!(
            tokio::time::timeout(CONNECTION_SET_TIMEOUT, beat).await,
            Ok(Ok(()))
        ) {
            debug!(key = %self.key, "Realtime connection heartbeat failed");
        }
    }

    /// [`Self::heartbeat`] once [`CONNECTION_HEARTBEAT_SECS`] have passed.
    async fn heartbeat_if_due(&mut self) {
        if self.last_beat.elapsed() >= Duration::from_secs(CONNECTION_HEARTBEAT_SECS) {
            self.heartbeat().await;
        }
    }
}

impl Drop for ConnectionSlot {
    fn drop(&mut self) {
        let Some(cache) = self.cache.take() else {
            return;
        };
        let key = std::mem::take(&mut self.key);
        let id = std::mem::take(&mut self.id);
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                let remove = async {
                    let mut conn = cache.get_connection().await?;
                    let _: i64 = conn.zrem(&key, &id).await?;
                    Ok::<_, anyhow::Error>(())
                };
                if !matches!(
                    tokio::time::timeout(CONNECTION_SET_TIMEOUT, remove).await,
                    Ok(Ok(()))
                ) {
                    // Not fatal: the member stops counting once its heartbeat is stale.
                    warn!(key = %key, "Failed to release realtime connection slot");
                }
            });
        }
    }
}

// ---------------------------------------------------------------------------
// Stream authorization
// ---------------------------------------------------------------------------

/// How long a stream trusts its last authorization check before re-reading the
/// key from the database and the session from Redis. Bounds revocation latency
/// without one query per forwarded event.
const SSE_AUTH_RECHECK_SECS: u64 = 2;

/// Expiry and revocation handles of the access JWT a realtime stream was opened
/// with. Empty for an API-key stream: the key itself is re-read from the
/// database, and it has no session.
#[derive(Debug, Clone, Default)]
struct StreamSession {
    jti: Option<String>,
    sid: Option<String>,
    /// Unix second after which the auth middleware no longer accepts the token
    /// (`exp` plus the validation leeway). Closing earlier would only make the
    /// client reconnect with a token that still works.
    valid_until: Option<i64>,
}

/// Read the session of the credential `authenticate_api_key_allow_query`
/// accepted for this request: the `Authorization: Bearer` value when present,
/// otherwise the first non-empty `token` / `api_key` query value, the order the
/// middleware uses. Only a JWT has a session.
///
/// The token is decoded again only to learn `exp` / `jti` / `sid`, which the
/// middleware does not pass on: the org and the queue scope still come from
/// `ctx`, and a token that does not belong to `ctx` is refused.
fn stream_session(
    state: &AppState,
    headers: &HeaderMap,
    raw_query: Option<&str>,
    ctx: &ApiKeyContext,
) -> Result<StreamSession, &'static str> {
    let from_header = headers
        .get(AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|header| header.strip_prefix("Bearer "))
        .map(str::to_string);
    let token = from_header.or_else(|| {
        url::form_urlencoded::parse(raw_query.unwrap_or("").as_bytes())
            .find(|(key, value)| (key == "token" || key == "api_key") && !value.is_empty())
            .map(|(_, value)| value.into_owned())
    });
    let Some(token) = token.filter(|t| t.starts_with("eyJ")) else {
        return Ok(StreamSession::default());
    };
    let validation = Validation::default();
    let claims = decode::<Claims>(
        &token,
        &DecodingKey::from_secret(state.settings.jwt.secret.as_bytes()),
        &validation,
    )
    .map(|data| data.claims)
    .ok()
    .filter(|claims| {
        claims.token_type == "access"
            && claims.api_key_id == ctx.api_key_id
            && claims.org_id == ctx.organization_id
    })
    .ok_or("Invalid token")?;
    Ok(StreamSession {
        valid_until: Some(claims.exp.saturating_add(validation.leeway as i64)),
        jti: Some(claims.jti),
        sid: claims.sid,
    })
}

/// Why a realtime stream ended on the server's side.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StreamEnd {
    /// The access token the stream was opened with expired.
    TokenExpired,
    /// Logout, token or session revocation, a revoked or expired key, or a
    /// deleted organization.
    Revoked,
    /// The connection reached its maximum age.
    MaxAge,
    /// The authorization lookup failed; the stream fails closed.
    AuthUnavailable,
    /// The Redis pub/sub channel could not be opened or closed.
    ChannelClosed,
}

impl StreamEnd {
    /// The `Error` event a WebSocket client receives before the close frame.
    fn message(self) -> Option<&'static str> {
        match self {
            Self::TokenExpired => Some("Access token expired"),
            Self::Revoked => Some("Authorization revoked"),
            Self::MaxAge => None,
            Self::AuthUnavailable => Some("Authorization check failed"),
            Self::ChannelClosed => {
                Some("Realtime event channel closed; reconnect to resume events")
            }
        }
    }

    /// The WebSocket close frame. 1008 tells the dashboard not to reconnect with
    /// the same token; every other code makes clients reconnect.
    fn close_frame(self) -> CloseFrame {
        let (code, reason) = match self {
            Self::TokenExpired => (close_code::POLICY, "token expired"),
            Self::Revoked => (close_code::POLICY, "authorization revoked"),
            Self::MaxAge => (close_code::NORMAL, "maximum connection age reached"),
            Self::AuthUnavailable => (close_code::ERROR, "authorization check failed"),
            Self::ChannelClosed => (close_code::ERROR, "event channel closed"),
        };
        CloseFrame {
            code,
            reason: reason.into(),
        }
    }
}

/// Keeps a long-lived stream's authorization current. The stream ends at its
/// deadline (token expiry or maximum age, whichever is first) and when a
/// re-check finds the session revoked (logout) or the key revoked, expired or
/// in a deleted org. The re-check also picks up a narrowed queue scope.
struct StreamAuth {
    api_key_id: String,
    org_id: String,
    session: StreamSession,
    deadline: Instant,
    deadline_reason: StreamEnd,
    /// The key's queue scope as of `checked_at`.
    scope: Vec<String>,
    checked_at: Option<Instant>,
}

impl StreamAuth {
    fn new(ctx: &ApiKeyContext, session: StreamSession, max_age: Duration) -> Self {
        let now = Instant::now();
        let (mut deadline, mut deadline_reason) = (now + max_age, StreamEnd::MaxAge);
        if let Some(valid_until) = session.valid_until {
            let left = valid_until.saturating_sub(chrono::Utc::now().timestamp());
            let token_deadline = now + Duration::from_secs(left.max(0) as u64);
            if token_deadline <= deadline {
                (deadline, deadline_reason) = (token_deadline, StreamEnd::TokenExpired);
            }
        }
        Self {
            api_key_id: ctx.api_key_id.clone(),
            org_id: ctx.organization_id.clone(),
            session,
            deadline,
            deadline_reason,
            scope: ctx.queues.clone(),
            checked_at: None,
        }
    }

    fn scope(&self) -> &[String] {
        &self.scope
    }

    fn allows_queue(&self, queue: &str) -> bool {
        queue_scope_allows(&self.scope, queue)
    }

    /// The deadline and the session. Redis is a cache of `revoked_sessions`;
    /// a missing Redis key still has to be checked in Postgres.
    async fn check_session(&self, state: &AppState) -> Result<(), StreamEnd> {
        if Instant::now() >= self.deadline {
            return Err(self.deadline_reason);
        }
        if self.session.jti.is_some() || self.session.sid.is_some() {
            let jti = self.session.jti.as_deref().unwrap_or("");
            // A database error is not a revocation: keep the stream and check
            // again on the next turn.
            match is_token_revoked(
                state.cache.as_ref(),
                state.db.pool(),
                jti,
                self.session.sid.as_deref(),
            )
            .await
            {
                Ok(true) => return Err(StreamEnd::Revoked),
                Ok(false) => {}
                Err(e) => warn!(error = %e, "Stream session revocation check failed"),
            }
        }
        Ok(())
    }

    /// Re-authorize now: the session, then the key and its current scope.
    async fn recheck(&mut self, state: &AppState) -> Result<(), StreamEnd> {
        self.check_session(state).await?;
        match load_current_realtime_scope(state.db.pool(), &self.api_key_id, &self.org_id).await {
            Ok(Some(scope)) => {
                self.scope = scope;
                self.checked_at = Some(Instant::now());
                Ok(())
            }
            Ok(None) => Err(StreamEnd::Revoked),
            Err(e) => {
                error!(error = %e, org_id = %self.org_id, "Realtime authorization lookup failed");
                Err(StreamEnd::AuthUnavailable)
            }
        }
    }

    /// [`Self::recheck`] when the last one is older than [`SSE_AUTH_RECHECK_SECS`].
    async fn recheck_if_due(&mut self, state: &AppState) -> Result<(), StreamEnd> {
        let fresh = self
            .checked_at
            .is_some_and(|at| at.elapsed() < Duration::from_secs(SSE_AUTH_RECHECK_SECS));
        if fresh {
            if Instant::now() >= self.deadline {
                return Err(self.deadline_reason);
            }
            return Ok(());
        }
        self.recheck(state).await
    }
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
    if !queue_scope_is_unrestricted(key_queues) && event.unverifiable_for_scoped_key() {
        return false;
    }
    if let Some(queue) = event.queue_name() {
        if !queue_scope_allows(key_queues, queue) {
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
    Extension(ctx): Extension<ApiKeyContext>,
    headers: HeaderMap,
    RawQuery(raw_query): RawQuery,
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
    // single source of truth. (`stream_session` reads only the expiry and revocation
    // handles of that same credential.)
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

    let session = stream_session(&state, &headers, raw_query.as_deref(), &ctx)
        .map_err(|e| (StatusCode::UNAUTHORIZED, e))?;

    // Take a slot in the org's connection set before the upgrade. The slot moves
    // into the upgrade callback, so a handshake that never completes drops it and
    // releases the slot again.
    let slot = ConnectionSlot::admit(&state, RealtimeStream::WebSocket, &org_id)
        .await
        .map_err(|_| {
            (
                StatusCode::TOO_MANY_REQUESTS,
                "Too many WebSocket connections",
            )
        })?;

    info!(org_id = %org_id, "WebSocket connection requested");
    // The key's queue scope (re-read while the socket is open) is enforced on the
    // event fan-out: a scoped key must never receive events for other queues.
    let auth = StreamAuth::new(
        &ctx,
        session,
        Duration::from_secs(MAX_WEBSOCKET_DURATION_SECS),
    );
    Ok(ws.on_upgrade(move |socket| handle_websocket(socket, query, auth, slot, state)))
}

/// Send the reason a WebSocket ends (an `Error` event, then the close frame).
async fn close_websocket(sender: &mut SplitSink<WebSocket, Message>, end: StreamEnd) {
    if let Some(message) = end.message() {
        let event = RealtimeEvent::Error {
            message: message.to_string(),
            timestamp: chrono::Utc::now(),
        };
        let json = serde_json::to_string(&event).unwrap_or_default();
        let _ = sender.send(Message::Text(json.into())).await;
    }
    let _ = sender.send(Message::Close(Some(end.close_frame()))).await;
}

/// Handle WebSocket connection
///
/// Events come from the org's Redis pub/sub channel and are filtered by the
/// subscription and the key's current scope. Authorization is re-checked at
/// most every [`SSE_AUTH_RECHECK_SECS`] before an event is revealed and on every
/// ping, not with a database query per event (BEFLOW-14).
async fn handle_websocket(
    socket: WebSocket,
    query: SubscribeQuery,
    mut auth: StreamAuth,
    mut slot: ConnectionSlot,
    state: AppState,
) {
    let (mut sender, mut receiver) = socket.split();
    let org_id = auth.org_id.clone();

    // Parse event filter
    let event_filter: Option<Vec<String>> = query
        .events
        .map(|e| e.split(',').map(|s| s.trim().to_lowercase()).collect());

    let mut subscription = SubscriptionFilter {
        queue: query.queue,
        job_id: query.job_id,
    };

    info!(
        org_id = %org_id,
        queue = ?subscription.queue,
        job_id = ?subscription.job_id,
        events = ?event_filter,
        "WebSocket connection established"
    );

    // Subscribe to the org's events. A socket whose channel could not be opened,
    // or closes later (Redis restart, failover, network blip), used to keep
    // answering pings while never delivering another event; the client was never
    // told (BEFLOW-12). Either case now sends an `Error` event and closes (1011),
    // and clients reconnect and resubscribe. Without Redis there is no channel.
    let mut messages = match state.cache {
        Some(ref cache) => match cache.subscribe(&format!("org:{}:events", org_id)).await {
            Ok(pubsub) => Some(pubsub.into_on_message()),
            Err(e) => {
                warn!(error = %e, org_id = %org_id, "WebSocket could not subscribe to org events; closing");
                close_websocket(&mut sender, StreamEnd::ChannelClosed).await;
                return;
            }
        },
        None => None,
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
        return;
    }

    let ping_every = Duration::from_secs(WEBSOCKET_PING_INTERVAL_SECS);
    let mut pings = tokio::time::interval_at(Instant::now() + ping_every, ping_every);
    let deadline = auth.deadline;

    // Main event loop. `Some(end)` = the server ends the socket.
    let end: Option<StreamEnd> = loop {
        tokio::select! {
            // Token expiry or maximum connection age.
            _ = tokio::time::sleep_until(deadline) => break Some(auth.deadline_reason),
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
                                        .is_some_and(|requested| !auth.allows_queue(requested))
                                    {
                                        continue;
                                    }
                                    if queue.is_some() {
                                        subscription.queue = queue;
                                    }
                                    if job_id.is_some() {
                                        subscription.job_id = job_id;
                                    }
                                }
                                ClientCommand::Unsubscribe { queue, job_id } => {
                                    if queue.is_none() || subscription.queue == queue {
                                        subscription.queue = None;
                                    }
                                    if job_id.is_none() || subscription.job_id == job_id {
                                        subscription.job_id = None;
                                    }
                                }
                                ClientCommand::Ping => {
                                    let pong = RealtimeEvent::Ping {
                                        timestamp: chrono::Utc::now(),
                                    };
                                    if sender.send(Message::Text(serde_json::to_string(&pong).unwrap().into())).await.is_err() {
                                        break None;
                                    }
                                }
                            }
                        }
                    }
                    Some(Ok(Message::Ping(data)))
                        if sender.send(Message::Pong(data.clone())).await.is_err() =>
                    {
                        break None;
                    }
                    Some(Ok(Message::Close(_))) => {
                        info!("Client closed WebSocket connection");
                        break None;
                    }
                    Some(Err(e)) => {
                        warn!("WebSocket error: {}", e);
                        break None;
                    }
                    None => {
                        break None;
                    }
                    _ => {}
                }
            }
            // Handle Redis pub/sub events
            payload = next_pubsub_payload(&mut messages) => {
                let Some(payload) = payload else {
                    warn!(org_id = %org_id, "WebSocket org event channel closed; closing");
                    break Some(StreamEnd::ChannelClosed);
                };
                let Ok(event) = serde_json::from_str::<RealtimeEvent>(&payload) else {
                    continue;
                };
                // Re-authorize (expired token, logout, revoked/expired key, deleted
                // org, narrowed scope) before revealing anything.
                if let Err(end) = auth.recheck_if_due(&state).await {
                    break Some(end);
                }
                if !event_matches_subscription(&event, &subscription, auth.scope()) {
                    continue;
                }

                // Apply event type filter
                let event_type = event.event_type();
                let should_send = event_filter.as_ref().is_none_or(|filter| {
                    filter.iter().any(|f| f == event_type || f == "*")
                });

                if should_send {
                    let json = serde_json::to_string(&event).unwrap_or_default();
                    if sender.send(Message::Text(json.into())).await.is_err() {
                        break None;
                    }
                }
            }
            // Periodic pings: re-check authorization so an idle socket also ends
            // after logout, and refresh the connection heartbeat.
            _ = pings.tick() => {
                if let Err(end) = auth.recheck(&state).await {
                    break Some(end);
                }
                slot.heartbeat().await;
                let ping = RealtimeEvent::Ping {
                    timestamp: chrono::Utc::now(),
                };
                if sender.send(Message::Text(serde_json::to_string(&ping).unwrap().into())).await.is_err() {
                    break None;
                }
            }
        }
    };

    if let Some(end) = end {
        info!(org_id = %org_id, reason = ?end, "Closing WebSocket connection");
        close_websocket(&mut sender, end).await;
    }
    // The pub/sub connection and the connection slot are released on drop.
    drop(messages);
    drop(slot);
    info!(org_id = %org_id, "WebSocket connection closed");
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
/// Closes the stream when the job reaches a terminal status. A job of another
/// organization, an unknown id and a job in a queue outside the key's scope all
/// answer the 404 `GET /jobs/{id}` gives, before the response becomes a stream.
pub async fn sse_job_handler(
    Path(job_id): Path<String>,
    Extension(ctx): Extension<ApiKeyContext>,
    headers: HeaderMap,
    RawQuery(raw_query): RawQuery,
    State(state): State<AppState>,
) -> AppResult<Sse<impl Stream<Item = Result<Event, std::io::Error>>>> {
    let org_id = ctx.organization_id.clone();
    info!(job_id = %job_id, org_id = %org_id, "SSE connection for job");

    let session = stream_session(&state, &headers, raw_query.as_deref(), &ctx)
        .map_err(|e| AppError::Unauthorized(e.to_string()))?;

    // Same lookup and answer as REST `GET /jobs/{id}`, before a connection slot is
    // taken and before the response upgrades to a stream: a missing id, another
    // organization's job and an out-of-scope queue are one indistinguishable 404, so
    // the stream is no oracle for job ids. A failed lookup is a 500 (fail closed).
    let queue_name: Option<String> =
        sqlx::query_scalar("SELECT queue_name FROM jobs WHERE id = $1 AND organization_id = $2")
            .bind(&job_id)
            .bind(&org_id)
            .fetch_optional(state.db.pool())
            .await?;
    if queue_name
        .as_deref()
        .is_none_or(|queue| !ctx.can_access_queue(queue))
    {
        return Err(AppError::NotFound("Job not found".to_string()));
    }

    let slot = ConnectionSlot::admit(&state, RealtimeStream::Job, &org_id)
        .await
        .map_err(|_| AppError::RateLimit("Too many SSE connections".to_string()))?;
    let auth = StreamAuth::new(&ctx, session, Duration::from_secs(MAX_SSE_DURATION_SECS));
    let api_key_id = ctx.api_key_id.clone();

    // Poll for job status changes (filtered by org), until the job is terminal,
    // the key or session is revoked, or the deadline (token expiry or the maximum
    // stream duration) passes.
    let stream = async_stream::stream! {
        let mut slot = slot;

        // Emit an SSE comment immediately so the response headers + first body
        // chunk flush right away. Without this, proxies (Cloudflare in particular)
        // buffer the response until the first poll cycle (2s), which makes the
        // connection look dead to clients with short timeouts.
        yield Ok(Event::default().comment("connected"));

        let mut last_status: Option<String> = None;
        loop {
            tokio::time::sleep(Duration::from_secs(SSE_JOB_POLL_INTERVAL_SECS)).await;
            if let Err(end) = auth.check_session(&state).await {
                debug!(job_id = %job_id, reason = ?end, "SSE job stream closing");
                break;
            }

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
                    if !queue_scope_allows(&queues, &queue_name) {
                        break;
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
                        yield Ok(Event::default().event("job.status").data(json));
                        last_status = Some(status);
                        if is_terminal {
                            break;
                        }
                    } else if is_terminal {
                        // Job is terminal but status didn't change - close stream
                        break;
                    } else {
                        // No change, send keepalive comment
                        yield Ok(Event::default().comment("keepalive"));
                    }
                }
                Ok(None) => {
                    // Job not found (or doesn't belong to this org) - close stream
                    let event = RealtimeEvent::Error {
                        message: "Job not found".to_string(), // Don't leak job_id
                        timestamp: chrono::Utc::now(),
                    };
                    let json = serde_json::to_string(&event).unwrap_or_default();
                    yield Ok(Event::default().event("error").data(json));
                    break;
                }
                Err(e) => {
                    error!(error = %e, job_id = %job_id, "Database error in SSE authorization poll");
                    yield Err(std::io::Error::other("SSE authorization check failed"));
                    break;
                }
            }
            slot.heartbeat_if_due().await;
        }
    };

    Ok(Sse::new(stream).keep_alive(
        axum::response::sse::KeepAlive::new()
            .interval(Duration::from_secs(15))
            .text("ping"),
    ))
}

/// Status counts reported by the queue stream.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct QueueStreamCounts {
    pending: i64,
    processing: i64,
    completed: i64,
    failed: i64,
}

/// How long one queue aggregate is shared by every queue stream of the same
/// (org, queue) in this process.
const QUEUE_STATS_CACHE_SECS: u64 = 4;

/// Above this many cached (org, queue) entries, idle ones are pruned.
const QUEUE_STATS_CACHE_PRUNE_AT: usize = 1024;

type QueueStatsCell = Arc<tokio::sync::Mutex<Option<(Instant, QueueStreamCounts)>>>;

/// Per-(org, queue) cache of the queue stream's aggregate. The aggregate counts
/// every job row of the queue (completed ones included, within retention), and
/// each stream used to run it every 5 s on its own (BEFLOW-15). Concurrent
/// streams wait for one load instead of each running it.
#[derive(Default)]
struct QueueStatsCache {
    cells: Mutex<HashMap<(String, String), QueueStatsCell>>,
}

static QUEUE_STATS_CACHE: LazyLock<QueueStatsCache> = LazyLock::new(QueueStatsCache::default);

impl QueueStatsCache {
    async fn get_or_load<F, Fut, E>(
        &self,
        org_id: &str,
        queue_name: &str,
        ttl: Duration,
        load: F,
    ) -> Result<QueueStreamCounts, E>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<QueueStreamCounts, E>>,
    {
        let cell = {
            let mut cells = self.cells.lock().unwrap_or_else(PoisonError::into_inner);
            if cells.len() >= QUEUE_STATS_CACHE_PRUNE_AT {
                // Keep entries in use (locked) or refreshed recently.
                cells.retain(|_, cell| {
                    cell.try_lock().map_or(true, |cached| {
                        cached
                            .as_ref()
                            .is_some_and(|(at, _)| at.elapsed() < ttl * 4)
                    })
                });
            }
            cells
                .entry((org_id.to_string(), queue_name.to_string()))
                .or_default()
                .clone()
        };
        let mut cached = cell.lock().await;
        if let Some((at, counts)) = *cached {
            if at.elapsed() < ttl {
                return Ok(counts);
            }
        }
        let counts = load().await?;
        *cached = Some((Instant::now(), counts));
        Ok(counts)
    }
}

async fn load_queue_stream_counts(
    pool: &sqlx::PgPool,
    org_id: &str,
    queue_name: &str,
) -> Result<QueueStreamCounts, sqlx::Error> {
    let rows: Vec<(String, i64)> = sqlx::query_as(
        r#"
        SELECT status, COUNT(*)
        FROM jobs
        WHERE organization_id = $1 AND queue_name = $2
        GROUP BY status
        "#,
    )
    .bind(org_id)
    .bind(queue_name)
    .fetch_all(pool)
    .await?;
    let mut counts = QueueStreamCounts::default();
    for (status, count) in rows {
        add_queue_stream_count(&mut counts, &status, count);
    }
    Ok(counts)
}

/// Fold one status bucket into the `queue.stats` payload.
///
/// `scheduled` is not pending. REST `pending_jobs` and gRPC `pending` count
/// only `status = 'pending'`; a future job must not inflate this event.
fn add_queue_stream_count(counts: &mut QueueStreamCounts, status: &str, count: i64) {
    match status {
        "pending" => counts.pending += count,
        "processing" => counts.processing += count,
        "completed" => counts.completed += count,
        "failed" | "deadletter" => counts.failed += count,
        _ => {}
    }
}

/// SSE stream for queue updates
///
/// Now requires authentication and filters by organization.
/// Previously returned stats for any queue without checking org isolation.
/// A queue outside the key's scope is a 403 before the stream starts.
pub async fn sse_queue_handler(
    Path(queue_name): Path<String>,
    Extension(ctx): Extension<ApiKeyContext>,
    headers: HeaderMap,
    RawQuery(raw_query): RawQuery,
    State(state): State<AppState>,
) -> AppResult<Sse<impl Stream<Item = Result<Event, std::io::Error>>>> {
    let org_id = ctx.organization_id.clone();
    info!(queue_name = %queue_name, org_id = %org_id, "SSE connection for queue");

    let session = stream_session(&state, &headers, raw_query.as_deref(), &ctx)
        .map_err(|e| AppError::Unauthorized(e.to_string()))?;

    // Queue-scope guard: a restricted key must not stream stats for a queue outside
    // its scope. It gets the 403 the REST queue routes and the `queue` filter of
    // `/events` and `/ws` give, before the response becomes a stream. (A queue
    // inside the scope that has no jobs yet is not a 404: queues are implicit and
    // the stats are the caller's own organization's, so there is nothing to probe.)
    require_queue_access(&ctx, &queue_name)?;

    let slot = ConnectionSlot::admit(&state, RealtimeStream::Queue, &org_id)
        .await
        .map_err(|_| AppError::RateLimit("Too many SSE connections".to_string()))?;
    let mut auth = StreamAuth::new(&ctx, session, Duration::from_secs(MAX_SSE_DURATION_SECS));

    let stream = async_stream::stream! {
        let mut slot = slot;

        // Flush an initial SSE comment so headers/first chunk make it past
        // proxies before the first 5s poll cycle. See sse_job_handler for context.
        yield Ok(Event::default().comment("connected"));

        loop {
            tokio::time::sleep(Duration::from_secs(SSE_QUEUE_POLL_INTERVAL_SECS)).await;

            // Re-authorize (expired token, logout, revoked/expired key, deleted org,
            // narrowed scope) before this protected poll.
            match auth.recheck_if_due(&state).await {
                Ok(()) => {}
                Err(StreamEnd::AuthUnavailable) => {
                    yield Err(std::io::Error::other("SSE authorization check failed"));
                    break;
                }
                Err(end) => {
                    debug!(queue = %queue_name, reason = ?end, "SSE queue stream closing");
                    break;
                }
            }
            if !auth.allows_queue(&queue_name) {
                break;
            }

            let counts = QUEUE_STATS_CACHE
                .get_or_load(
                    &org_id,
                    &queue_name,
                    Duration::from_secs(QUEUE_STATS_CACHE_SECS),
                    || load_queue_stream_counts(state.db.pool(), &org_id, &queue_name),
                )
                .await;
            match counts {
                Ok(counts) => {
                    let event = RealtimeEvent::QueueStats {
                        queue_name: queue_name.clone(),
                        pending: counts.pending,
                        processing: counts.processing,
                        completed: counts.completed,
                        failed: counts.failed,
                        timestamp: chrono::Utc::now(),
                    };
                    let json = serde_json::to_string(&event).unwrap_or_default();
                    yield Ok(Event::default().event("queue.stats").data(json));
                }
                Err(e) => {
                    error!(error = %e, queue = %queue_name, "Database error in SSE queue stats poll");
                    yield Err(std::io::Error::other("SSE queue stats unavailable"));
                    break;
                }
            }
            slot.heartbeat_if_due().await;
        }
    };

    Ok(Sse::new(stream).keep_alive(
        axum::response::sse::KeepAlive::new()
            .interval(Duration::from_secs(15))
            .text("ping"),
    ))
}

/// Interval between `system.health` events on the global SSE stream.
const SSE_HEALTH_INTERVAL_SECS: u64 = 10;

/// Wait for the next payload on the org pub/sub channel. Resolves to `None` once
/// the channel closes; pends forever when there is no channel (no Redis).
async fn next_pubsub_payload(messages: &mut Option<redis::aio::PubSubStream>) -> Option<String> {
    match messages {
        Some(stream) => {
            while let Some(msg) = stream.next().await {
                if let Ok(payload) = msg.get_payload::<String>() {
                    return Some(payload);
                }
            }
            None
        }
        None => std::future::pending().await,
    }
}

/// SSE stream for all events of the authenticated organization
///
/// Forwards the same Redis pub/sub events the WebSocket receives (`job.created`,
/// `job.status`, `job.completed`, `job.failed`, ...). Each is sent as a named SSE
/// event whose `data:` is the `{"type": ..., "data": {...}}` envelope documented on
/// /docs/realtime. A `system.health` event follows every 10 seconds. The optional
/// `queue`, `job_id` and `events` (comma-separated names) query parameters narrow
/// the stream, and the key's queue scope always applies.
///
/// Before 0.1.114 this stream only ever emitted `system.health`, so SSE clients
/// following the docs never saw a job event.
pub async fn sse_events_handler(
    Extension(ctx): Extension<ApiKeyContext>,
    headers: HeaderMap,
    RawQuery(raw_query): RawQuery,
    Query(query): Query<SubscribeQuery>,
    State(state): State<AppState>,
) -> Result<
    Sse<impl Stream<Item = Result<Event, std::convert::Infallible>>>,
    (StatusCode, &'static str),
> {
    if let Err(e) = validate_queue_filter(&query.queue) {
        return Err((StatusCode::BAD_REQUEST, e));
    }
    if query
        .queue
        .as_deref()
        .is_some_and(|queue| !ctx.can_access_queue(queue))
    {
        return Err((StatusCode::FORBIDDEN, "Queue is outside API key scope"));
    }

    let org_id = ctx.organization_id.clone();
    let session = stream_session(&state, &headers, raw_query.as_deref(), &ctx)
        .map_err(|e| (StatusCode::UNAUTHORIZED, e))?;

    // Per-org cap: the slot is released when the stream is dropped.
    let slot = ConnectionSlot::admit(&state, RealtimeStream::Events, &org_id)
        .await
        .map_err(|_| (StatusCode::TOO_MANY_REQUESTS, "Too many SSE connections"))?;
    let mut auth = StreamAuth::new(&ctx, session, Duration::from_secs(MAX_SSE_DURATION_SECS));

    info!(org_id = %org_id, "SSE connection for all events (authenticated)");

    let event_filter: Option<Vec<String>> = query.events.map(|e| {
        e.split(',')
            .map(|s| s.trim().to_lowercase())
            .filter(|s| !s.is_empty())
            .collect()
    });
    let subscription = SubscriptionFilter {
        queue: query.queue,
        job_id: query.job_id,
    };

    let stream = async_stream::stream! {
        let mut slot = slot;

        // Subscribe before the first frame, so a client that has seen `: connected`
        // does not miss an event published right after.
        let mut messages = match state.cache {
            Some(ref cache) => match cache.subscribe(&format!("org:{}:events", org_id)).await {
                Ok(pubsub) => Some(pubsub.into_on_message()),
                Err(e) => {
                    warn!(error = %e, org_id = %org_id, "SSE could not subscribe to org events; health only");
                    None
                }
            },
            None => None,
        };

        // Flush headers and a first chunk right away so proxies (Cloudflare) do not
        // buffer the response. See sse_job_handler.
        yield Ok(Event::default().comment("connected"));

        let health_every = Duration::from_secs(SSE_HEALTH_INTERVAL_SECS);
        let mut health = tokio::time::interval_at(Instant::now() + health_every, health_every);
        // Token expiry or the maximum stream duration, whichever is first.
        let deadline = auth.deadline;

        loop {
            // `None` = health tick, `Some(payload)` = pub/sub event.
            let payload: Option<String> = tokio::select! {
                _ = tokio::time::sleep_until(deadline) => {
                    debug!(org_id = %org_id, reason = ?auth.deadline_reason, "SSE events stream deadline - closing");
                    break;
                }
                _ = health.tick() => None,
                msg = next_pubsub_payload(&mut messages) => match msg {
                    Some(payload) => Some(payload),
                    None => {
                        warn!(org_id = %org_id, "SSE org event channel closed; health only");
                        messages = None;
                        continue;
                    }
                },
            };

            // Re-authorize (expired token, logout, revoked/expired key, deleted org,
            // narrowed scope) before revealing anything, at most once per
            // SSE_AUTH_RECHECK_SECS.
            if let Err(end) = auth.recheck_if_due(&state).await {
                debug!(org_id = %org_id, reason = ?end, "SSE events stream closing");
                break;
            }

            let event = match payload {
                Some(payload) => match serde_json::from_str::<RealtimeEvent>(&payload) {
                    Ok(event) if event_matches_subscription(&event, &subscription, auth.scope()) => event,
                    _ => continue,
                },
                None => {
                    slot.heartbeat_if_due().await;
                    let db_ok = sqlx::query("SELECT 1").execute(state.db.pool()).await.is_ok();
                    let redis_ok = match state.cache {
                        Some(ref cache) => cache.ping().await.is_ok(),
                        None => false,
                    };
                    RealtimeEvent::SystemHealth {
                        database: db_ok,
                        redis: redis_ok,
                        timestamp: chrono::Utc::now(),
                    }
                }
            };

            let event_type = event.event_type();
            if !event_filter
                .as_ref()
                .is_none_or(|filter| filter.iter().any(|f| f == event_type || f == "*"))
            {
                continue;
            }

            let json = serde_json::to_string(&event).unwrap_or_default();
            yield Ok(Event::default().event(event_type).data(json));
        }
    };

    Ok(Sse::new(stream).keep_alive(
        axum::response::sse::KeepAlive::new()
            .interval(Duration::from_secs(15))
            .text("ping"),
    ))
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

    fn ctx(queues: &[&str]) -> ApiKeyContext {
        ApiKeyContext {
            api_key_id: "key".to_string(),
            organization_id: "org".to_string(),
            queues: queues.iter().map(|q| q.to_string()).collect(),
            rate_limit: None,
        }
    }

    // BEFLOW-13: a stream ends at the earlier of token expiry and its maximum age.
    #[tokio::test]
    async fn test_stream_deadline_is_token_expiry_or_max_age() {
        let max_age = Duration::from_secs(3600);
        let now = chrono::Utc::now().timestamp();

        let key_stream = StreamAuth::new(&ctx(&[]), StreamSession::default(), max_age);
        assert_eq!(key_stream.deadline_reason, StreamEnd::MaxAge);
        assert!(key_stream.deadline > Instant::now() + Duration::from_secs(3590));

        let short_token = StreamSession {
            valid_until: Some(now + 60),
            ..StreamSession::default()
        };
        let auth = StreamAuth::new(&ctx(&[]), short_token, max_age);
        assert_eq!(auth.deadline_reason, StreamEnd::TokenExpired);
        assert!(auth.deadline <= Instant::now() + Duration::from_secs(61));

        let long_token = StreamSession {
            valid_until: Some(now + 86_400),
            ..StreamSession::default()
        };
        let auth = StreamAuth::new(&ctx(&[]), long_token, max_age);
        assert_eq!(auth.deadline_reason, StreamEnd::MaxAge);

        let expired = StreamSession {
            valid_until: Some(now - 5),
            ..StreamSession::default()
        };
        let auth = StreamAuth::new(&ctx(&[]), expired, max_age);
        assert_eq!(auth.deadline_reason, StreamEnd::TokenExpired);
        assert!(auth.deadline <= Instant::now());

        let scoped = StreamAuth::new(&ctx(&["emails"]), StreamSession::default(), max_age);
        assert!(scoped.allows_queue("emails"));
        assert!(!scoped.allows_queue("billing"));
    }

    // BEFLOW-15: queue streams of one (org, queue) share one aggregate per TTL
    // instead of each running it every poll.
    #[tokio::test]
    async fn test_queue_stats_cache_shares_one_load_per_queue_and_ttl() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let cache = QueueStatsCache::default();
        let loads = AtomicUsize::new(0);
        let load = || async {
            let n = loads.fetch_add(1, Ordering::SeqCst) as i64 + 1;
            tokio::time::sleep(Duration::from_millis(20)).await;
            Ok::<_, ()>(QueueStreamCounts {
                pending: n,
                ..QueueStreamCounts::default()
            })
        };
        let ttl = Duration::from_millis(300);

        // Ten streams polling at once wait for one load.
        let results = futures::future::join_all(
            (0..10).map(|_| cache.get_or_load("org", "emails", ttl, load)),
        )
        .await;
        assert_eq!(loads.load(Ordering::SeqCst), 1);
        assert!(results.iter().all(|r| r
            == &Ok(QueueStreamCounts {
                pending: 1,
                ..QueueStreamCounts::default()
            })));

        // Within the TTL the cached counts are served.
        cache.get_or_load("org", "emails", ttl, load).await.unwrap();
        assert_eq!(loads.load(Ordering::SeqCst), 1);

        // Other queues and other orgs are separate entries.
        cache
            .get_or_load("org", "billing", ttl, load)
            .await
            .unwrap();
        cache
            .get_or_load("other-org", "emails", ttl, load)
            .await
            .unwrap();
        assert_eq!(loads.load(Ordering::SeqCst), 3);

        // After the TTL the aggregate is loaded again.
        tokio::time::sleep(ttl).await;
        let fresh = cache.get_or_load("org", "emails", ttl, load).await.unwrap();
        assert_eq!(loads.load(Ordering::SeqCst), 4);
        assert_eq!(fresh.pending, 4);

        // A failed load is not cached.
        let failing = || async { Err::<QueueStreamCounts, ()>(()) };
        tokio::time::sleep(ttl).await;
        assert!(cache
            .get_or_load("org", "emails", ttl, failing)
            .await
            .is_err());
        cache.get_or_load("org", "emails", ttl, load).await.unwrap();
        assert_eq!(loads.load(Ordering::SeqCst), 5);
    }

    #[test]
    fn queue_stream_counts_do_not_treat_scheduled_as_pending() {
        let mut counts = QueueStreamCounts::default();
        add_queue_stream_count(&mut counts, "scheduled", 4);
        add_queue_stream_count(&mut counts, "pending", 1);
        add_queue_stream_count(&mut counts, "processing", 2);
        add_queue_stream_count(&mut counts, "completed", 3);
        add_queue_stream_count(&mut counts, "deadletter", 5);
        add_queue_stream_count(&mut counts, "failed", 6);
        assert_eq!(
            counts,
            QueueStreamCounts {
                pending: 1,
                processing: 2,
                completed: 3,
                failed: 11,
            }
        );
    }
}
