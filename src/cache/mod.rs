//! Cache module for Spooled Backend
//!
//! This module provides Redis-based caching and pub/sub functionality.
//! It gracefully degrades when Redis is unavailable.

use anyhow::{Context, Result};
use redis::aio::{ConnectionManager, ConnectionManagerConfig};
use redis::io::tcp::{socket2, TcpSettings};
use redis::{AsyncCommands, Client, IntoConnectionInfo};
use std::future::Future;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};
use tracing::{debug, info, warn};

use crate::config::RedisSettings;

/// Redis cache wrapper
///
/// Commands go through a [`ConnectionManager`], which re-establishes the
/// connection after an I/O error. A bare `MultiplexedConnection` stays broken
/// forever once its socket drops (for example when Redis restarts), which left
/// rate limiting, session revocation and the auth cache dead until the API
/// process itself was restarted.
///
/// Every command is bounded by [`COMMAND_TIMEOUT`], including any wait for a
/// reconnect in progress. The manager does not count a response timeout as a
/// dropped connection, so a half-open socket (Redis host gone without a TCP
/// reset) would time out forever; after [`TIMEOUTS_BEFORE_RECONNECT`]
/// timeouts in a row the connection is replaced.
#[derive(Clone)]
pub struct RedisCache {
    client: Client,
    config: ConnectionManagerConfig,
    connection: Arc<RwLock<ConnectionManager>>,
    timeouts: Arc<TimeoutTracker>,
}

/// Upper bound for one Redis call (command, pipeline or script) and for the
/// health-check PING, including any wait for a reconnect in progress, so
/// callers reach their fail-open / fail-closed handling promptly and
/// `/health` answers while Redis is unreachable.
const COMMAND_TIMEOUT: Duration = Duration::from_millis(500);

/// Upper bound for opening a pub/sub connection: a connect (1 s timeout)
/// plus the SUBSCRIBE round trip.
const SUBSCRIBE_TIMEOUT: Duration = Duration::from_secs(2);

/// Timed-out calls in a row after which the connection counts as dead.
const TIMEOUTS_BEFORE_RECONNECT: u32 = 3;

/// Minimum time between two forced reconnects, so an outage under load does
/// not turn into a connection storm against Redis.
const MIN_RECONNECT_INTERVAL: Duration = Duration::from_secs(1);

/// TCP keepalive on Redis sockets, so the kernel notices an idle connection
/// whose peer has gone away (and NAT / Docker network state is kept warm).
const TCP_KEEPALIVE_TIME: Duration = Duration::from_secs(30);
const TCP_KEEPALIVE_INTERVAL: Duration = Duration::from_secs(10);

/// Decides when timed-out calls mean the connection is dead.
#[derive(Debug, Default)]
struct TimeoutTracker {
    /// Timed-out calls since Redis last answered.
    in_a_row: AtomicU32,
    /// When the connection was last replaced.
    last_reconnect: Mutex<Option<Instant>>,
}

impl TimeoutTracker {
    /// Redis answered (a reply or an error reply): the connection works.
    fn answered(&self) {
        if self.in_a_row.load(Ordering::Relaxed) != 0 {
            self.in_a_row.store(0, Ordering::Relaxed);
        }
    }

    /// A call timed out. True when the connection should be replaced now.
    fn timed_out(&self, now: Instant) -> bool {
        let in_a_row = self.in_a_row.fetch_add(1, Ordering::Relaxed) + 1;
        in_a_row >= TIMEOUTS_BEFORE_RECONNECT && self.reconnect_due(now)
    }

    /// Claim a reconnect unless one happened within [`MIN_RECONNECT_INTERVAL`].
    /// A claimed reconnect starts the timeout count again.
    fn reconnect_due(&self, now: Instant) -> bool {
        let mut last = self
            .last_reconnect
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if last.is_some_and(|at| now.saturating_duration_since(at) < MIN_RECONNECT_INTERVAL) {
            return false;
        }
        *last = Some(now);
        self.in_a_row.store(0, Ordering::Relaxed);
        true
    }
}

/// Strip any `user:password@` part from a Redis URL before it is logged.
fn redact_redis_url(raw: &str) -> String {
    match url::Url::parse(raw) {
        Ok(mut parsed) => {
            if !parsed.username().is_empty() || parsed.password().is_some() {
                let _ = parsed.set_username("");
                let _ = parsed.set_password(None);
            }
            parsed.to_string()
        }
        Err(_) => "<unparseable redis url>".to_string(),
    }
}

impl RedisCache {
    /// Connect to Redis with the given settings
    pub async fn connect(settings: &RedisSettings) -> Result<Self> {
        info!(url = %redact_redis_url(&settings.url), "Connecting to Redis");

        let (client, config) = Self::client_and_config(settings)?;
        let mut connection = ConnectionManager::new_with_config(client.clone(), config.clone())
            .await
            .context("Failed to connect to Redis")?;

        // Test connection
        let _: String = redis::cmd("PING")
            .query_async(&mut connection)
            .await
            .context("Failed to ping Redis")?;

        info!("Redis connection established");

        Ok(Self::with_connection(client, config, connection))
    }

    /// A client that connects on first use and keeps reconnecting while Redis
    /// does not answer. Calls fail (within [`COMMAND_TIMEOUT`]) until it does.
    pub fn connect_lazy(settings: &RedisSettings) -> Result<Self> {
        let (client, config) = Self::client_and_config(settings)?;
        let connection = ConnectionManager::new_lazy_with_config(client.clone(), config.clone())
            .context("Failed to create Redis client")?;
        Ok(Self::with_connection(client, config, connection))
    }

    /// The process's Redis at startup.
    ///
    /// When Redis does not answer yet and `required` (the operator set
    /// `REDIS_URL`, or `RATE_LIMIT_FAIL_CLOSED` cannot work without Redis),
    /// the process keeps a reconnecting client: it runs degraded, `/health`
    /// says so, and it picks Redis up once Redis answers. Giving up here
    /// instead left the process without Redis until it was restarted, which
    /// under `RATE_LIMIT_FAIL_CLOSED` refused every request while `/health`
    /// reported healthy. Otherwise (self-host without Redis) it runs without.
    pub async fn connect_at_startup(settings: &RedisSettings, required: bool) -> Option<Self> {
        let error = match Self::connect(settings).await {
            Ok(cache) => return Some(cache),
            Err(e) => e,
        };
        if !required {
            warn!(error = %error, "Redis unavailable, running without cache");
            return None;
        }
        match Self::connect_lazy(settings) {
            Ok(cache) => {
                warn!(
                    error = %error,
                    "Redis unavailable at startup; running degraded and reconnecting until it answers"
                );
                Some(cache)
            }
            Err(e) => {
                warn!(error = %e, "Redis client could not be created, running without cache");
                None
            }
        }
    }

    fn client_and_config(settings: &RedisSettings) -> Result<(Client, ConnectionManagerConfig)> {
        let keepalive = socket2::TcpKeepalive::new()
            .with_time(TCP_KEEPALIVE_TIME)
            .with_interval(TCP_KEEPALIVE_INTERVAL);
        let connection_info = settings
            .url
            .as_str()
            .into_connection_info()
            .context("Failed to create Redis client")?
            .set_tcp_settings(TcpSettings::default().set_keepalive(keepalive));
        let client = Client::open(connection_info).context("Failed to create Redis client")?;

        // Keep the manager's own reconnect attempt short (two retries, 1 s per
        // connect, at most 1 s between tries) so a fresh connection is ready
        // soon after Redis answers again. Callers never wait on it for longer
        // than COMMAND_TIMEOUT.
        let config = ConnectionManagerConfig::new()
            .set_number_of_retries(2)
            .set_max_delay(Duration::from_secs(1))
            .set_connection_timeout(Some(Duration::from_secs(1)))
            .set_response_timeout(Some(COMMAND_TIMEOUT));
        Ok((client, config))
    }

    fn with_connection(
        client: Client,
        config: ConnectionManagerConfig,
        connection: ConnectionManager,
    ) -> Self {
        Self {
            client,
            config,
            connection: Arc::new(RwLock::new(connection)),
            timeouts: Arc::new(TimeoutTracker::default()),
        }
    }

    /// Get a connection handle.
    ///
    /// The handle is cheap to clone and reconnects on its own after the
    /// underlying connection drops.
    pub async fn get_connection(&self) -> Result<ConnectionManager> {
        Ok(self
            .connection
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone())
    }

    /// Replace the connection manager with a fresh, lazily connecting one.
    ///
    /// The manager reconnects on I/O errors but not on response timeouts, so a
    /// half-open socket (Redis host gone without a TCP reset) would keep timing
    /// out forever. Called after repeated command timeouts and when the
    /// health-check PING fails, at most once per [`MIN_RECONNECT_INTERVAL`].
    fn reset_connection(&self, reason: &str) {
        match ConnectionManager::new_lazy_with_config(self.client.clone(), self.config.clone()) {
            Ok(fresh) => {
                *self
                    .connection
                    .write()
                    .unwrap_or_else(|poisoned| poisoned.into_inner()) = fresh;
                info!(reason, "Replaced the Redis connection");
            }
            Err(e) => warn!(error = %e, "Could not create a new Redis connection"),
        }
    }

    /// Run one Redis call with [`COMMAND_TIMEOUT`], replacing the connection
    /// once timeouts in a row show that it is dead.
    async fn bounded<T>(&self, call: impl Future<Output = redis::RedisResult<T>>) -> Result<T> {
        match tokio::time::timeout(COMMAND_TIMEOUT, call).await {
            Ok(Ok(value)) => {
                self.timeouts.answered();
                Ok(value)
            }
            Ok(Err(e)) => {
                if e.is_timeout() {
                    self.command_timed_out();
                } else if !e.is_io_error() {
                    // An error reply still shows the connection works; the
                    // manager reconnects on I/O errors by itself.
                    self.timeouts.answered();
                }
                Err(e.into())
            }
            Err(_) => {
                self.command_timed_out();
                Err(anyhow::anyhow!(
                    "Redis did not answer within {:?}",
                    COMMAND_TIMEOUT
                ))
            }
        }
    }

    fn command_timed_out(&self) {
        if self.timeouts.timed_out(Instant::now()) {
            self.reset_connection("repeated command timeouts");
        }
    }

    /// Get a value from cache
    pub async fn get(&self, key: &str) -> Result<Option<String>> {
        let mut conn = self.get_connection().await?;
        let result: Option<String> = self.bounded(conn.get(key)).await?;
        debug!(key = %key, hit = result.is_some(), "Cache get");
        Ok(result)
    }

    /// Set a value in cache with TTL
    pub async fn set(&self, key: &str, value: &str, ttl_secs: u64) -> Result<()> {
        let mut conn = self.get_connection().await?;
        let _: () = self.bounded(conn.set_ex(key, value, ttl_secs)).await?;
        debug!(key = %key, ttl = ttl_secs, "Cache set");
        Ok(())
    }

    /// Set a key only if it does not already exist, with a TTL (`SET NX EX`).
    ///
    /// Returns `true` when this caller won the race and the key was written,
    /// `false` when it already existed. This is the primitive for
    /// "do this work at most once per interval, across all replicas" — used by
    /// the `last_used` write guard, and safe to reuse for other coalesced
    /// writes.
    ///
    /// Note the key must not collide with a token-bucket key: those are Redis
    /// HASHes and `SET` against them fails with WRONGTYPE.
    pub async fn set_if_absent(&self, key: &str, value: &str, ttl_secs: u64) -> Result<bool> {
        let mut conn = self.get_connection().await?;
        let written: Option<String> = self
            .bounded(
                redis::cmd("SET")
                    .arg(key)
                    .arg(value)
                    .arg("NX")
                    .arg("EX")
                    .arg(ttl_secs)
                    .query_async(&mut conn),
            )
            .await?;
        // Redis replies OK on success and nil when NX prevented the write.
        Ok(written.is_some())
    }

    /// Delete a value from cache
    pub async fn delete(&self, key: &str) -> Result<()> {
        let mut conn = self.get_connection().await?;
        let _: () = self.bounded(conn.del(key)).await?;
        debug!(key = %key, "Cache delete");
        Ok(())
    }

    /// Maximum keys to delete in a single pattern operation
    /// Limit total deletions to prevent runaway operations
    const MAX_PATTERN_DELETE_KEYS: u64 = 10000;

    /// SCAN batch size - configurable based on dataset size
    /// Made more reasonable for typical workloads
    const SCAN_BATCH_SIZE: u64 = 500;

    /// Delete all keys matching a pattern (use with caution!)
    ///
    /// This uses SCAN to find keys and DEL to delete them.
    /// For large datasets, consider using UNLINK for async deletion.
    ///
    /// The pattern MUST include organization context to prevent cache pollution attacks.
    /// Uses configurable SCAN COUNT and limits total deletions
    pub async fn delete_pattern(&self, pattern: &str) -> Result<u64> {
        // Validate pattern has organization context
        // Pattern should be like "org:ORG_ID:*" or similar to prevent cross-tenant deletion
        if !pattern.contains(':') {
            warn!(pattern = %pattern, "delete_pattern called without namespace - blocking to prevent cross-tenant deletion");
            return Err(anyhow::anyhow!("Pattern must include namespace (e.g., 'org:xxx:*') to prevent cross-tenant cache deletion"));
        }

        let mut conn = self.get_connection().await?;
        let mut deleted: u64 = 0;
        let mut cursor: u64 = 0;

        loop {
            // Use larger SCAN COUNT for efficiency
            let (new_cursor, keys): (u64, Vec<String>) = self
                .bounded(
                    redis::cmd("SCAN")
                        .arg(cursor)
                        .arg("MATCH")
                        .arg(pattern)
                        .arg("COUNT")
                        .arg(Self::SCAN_BATCH_SIZE)
                        .query_async(&mut conn),
                )
                .await?;

            // Delete found keys using UNLINK for non-blocking deletion
            if !keys.is_empty() {
                // Use UNLINK for async deletion (Redis 4.0+)
                // Falls back to DEL if UNLINK fails (older Redis)
                let count: u64 = self
                    .bounded(redis::cmd("UNLINK").arg(&keys).query_async(&mut conn))
                    .await
                    .unwrap_or(0);

                // If UNLINK failed, try DEL
                let count = if count == 0 && !keys.is_empty() {
                    self.bounded(conn.del::<_, u64>(&keys)).await.unwrap_or(0)
                } else {
                    count
                };

                deleted += count;
                debug!(pattern = %pattern, count = count, "Deleted cached keys");
            }

            cursor = new_cursor;

            // Limit total deletions to prevent runaway operations
            if cursor == 0 || deleted >= Self::MAX_PATTERN_DELETE_KEYS {
                if deleted >= Self::MAX_PATTERN_DELETE_KEYS {
                    warn!(
                        pattern = %pattern,
                        deleted = deleted,
                        limit = Self::MAX_PATTERN_DELETE_KEYS,
                        "Pattern delete reached limit - some keys may remain"
                    );
                }
                break;
            }
        }

        info!(pattern = %pattern, total = deleted, "Cache pattern delete complete");
        Ok(deleted)
    }

    /// Delete all keys matching a pattern for a specific organization
    ///
    /// Safe version that enforces organization prefix
    pub async fn delete_org_pattern(
        &self,
        organization_id: &str,
        suffix_pattern: &str,
    ) -> Result<u64> {
        // Construct pattern with org prefix to ensure tenant isolation
        let safe_pattern = format!("org:{}:{}", organization_id, suffix_pattern);

        // Now call the internal method (pattern has org context, so it's safe)
        self.delete_pattern_internal(&safe_pattern).await
    }

    /// Internal delete pattern without validation (called after org prefix is added)
    async fn delete_pattern_internal(&self, pattern: &str) -> Result<u64> {
        let mut conn = self.get_connection().await?;
        let mut deleted: u64 = 0;
        let mut cursor: u64 = 0;

        loop {
            let (new_cursor, keys): (u64, Vec<String>) = self
                .bounded(
                    redis::cmd("SCAN")
                        .arg(cursor)
                        .arg("MATCH")
                        .arg(pattern)
                        .arg("COUNT")
                        .arg(100)
                        .query_async(&mut conn),
                )
                .await?;

            if !keys.is_empty() {
                let count: u64 = self.bounded(conn.del(&keys)).await?;
                deleted += count;
                debug!(pattern = %pattern, count = count, "Deleted cached keys");
            }

            cursor = new_cursor;
            if cursor == 0 {
                break;
            }
        }

        info!(pattern = %pattern, total = deleted, "Cache pattern delete complete");
        Ok(deleted)
    }

    /// Check if a key exists
    pub async fn exists(&self, key: &str) -> Result<bool> {
        let mut conn = self.get_connection().await?;
        let exists: bool = self.bounded(conn.exists(key)).await?;
        Ok(exists)
    }

    /// Increment a counter
    pub async fn incr(&self, key: &str) -> Result<i64> {
        let mut conn = self.get_connection().await?;
        let value: i64 = self.bounded(conn.incr(key, 1)).await?;
        Ok(value)
    }

    /// Decrement a counter (minimum 0)
    ///
    /// If value goes to 0 or negative, deletes the key to prevent memory leaks.
    /// Previous implementation used SET without TTL which removed expiration,
    /// causing keys to become permanent.
    pub async fn decr(&self, key: &str) -> Result<i64> {
        let mut conn = self.get_connection().await?;
        let value: i64 = self.bounded(conn.decr(key, 1)).await?;
        // If value is 0 or negative, delete the key entirely
        // This prevents memory leaks from orphaned keys with no TTL
        // and is semantically correct (0 connections = no entry needed)
        if value <= 0 {
            let _: () = self.bounded(conn.del(key)).await?;
            return Ok(0);
        }
        Ok(value)
    }

    /// Increment a counter with TTL (for connection counting)
    pub async fn incr_with_ttl(&self, key: &str, ttl_secs: u64) -> Result<i64> {
        let mut conn = self.get_connection().await?;
        // Use pipeline for atomicity
        let (value, _): (i64, ()) = self
            .bounded(
                redis::pipe()
                    .atomic()
                    .incr(key, 1)
                    .expire(key, ttl_secs as i64)
                    .query_async(&mut conn),
            )
            .await?;
        Ok(value)
    }

    /// Increment a counter and set expiration (atomic for rate limiting)
    ///
    pub async fn increment(&self, key: &str, ttl_secs: u64) -> Result<i64> {
        let mut conn = self.get_connection().await?;
        // Use pipeline for atomicity
        let (value, _): (i64, ()) = self
            .bounded(
                redis::pipe()
                    .atomic()
                    .incr(key, 1)
                    .expire(key, ttl_secs as i64)
                    .query_async(&mut conn),
            )
            .await?;
        Ok(value)
    }

    /// Set expiration on a key
    pub async fn expire(&self, key: &str, ttl_secs: u64) -> Result<()> {
        let mut conn = self.get_connection().await?;
        let _: () = self.bounded(conn.expire(key, ttl_secs as i64)).await?;
        Ok(())
    }

    /// Publish a message to a channel
    pub async fn publish(&self, channel: &str, message: &str) -> Result<()> {
        let mut conn = self.get_connection().await?;
        let _: () = self.bounded(conn.publish(channel, message)).await?;
        debug!(channel = %channel, "Published message");
        Ok(())
    }

    /// Subscribe to a channel (returns a PubSub handle)
    pub async fn subscribe(&self, channel: &str) -> Result<redis::aio::PubSub> {
        let open = async {
            let mut pubsub = self
                .client
                .get_async_pubsub()
                .await
                .context("Failed to create pubsub connection")?;
            pubsub.subscribe(channel).await?;
            Ok::<_, anyhow::Error>(pubsub)
        };
        let pubsub = tokio::time::timeout(SUBSCRIBE_TIMEOUT, open)
            .await
            .map_err(|_| {
                anyhow::anyhow!("Redis subscribe did not complete within {SUBSCRIBE_TIMEOUT:?}")
            })??;
        info!(channel = %channel, "Subscribed to channel");

        Ok(pubsub)
    }

    /// Get JSON value from cache
    pub async fn get_json<T: serde::de::DeserializeOwned>(&self, key: &str) -> Result<Option<T>> {
        match self.get(key).await? {
            Some(data) => {
                let value = serde_json::from_str(&data)?;
                Ok(Some(value))
            }
            None => Ok(None),
        }
    }

    /// Set JSON value in cache
    pub async fn set_json<T: serde::Serialize>(
        &self,
        key: &str,
        value: &T,
        ttl_secs: u64,
    ) -> Result<()> {
        let json = serde_json::to_string(value)?;
        self.set(key, &json, ttl_secs).await
    }

    /// Health check
    ///
    /// A failed PING replaces the connection right away (at most once per
    /// [`MIN_RECONNECT_INTERVAL`]), so the container healthcheck also heals a
    /// half-open connection that sees no other traffic.
    pub async fn health_check(&self) -> Result<bool> {
        let mut conn = self.get_connection().await?;
        let ping_cmd = redis::cmd("PING");
        let ping = ping_cmd.query_async::<String>(&mut conn);
        let failure = match tokio::time::timeout(COMMAND_TIMEOUT, ping).await {
            Ok(Ok(_)) => {
                self.timeouts.answered();
                return Ok(true);
            }
            Ok(Err(e)) => {
                warn!(error = %e, "Redis health check failed");
                "failed health check"
            }
            Err(_) => {
                warn!("Redis health check timed out");
                "timed-out health check"
            }
        };
        if self.timeouts.reconnect_due(Instant::now()) {
            self.reset_connection(failure);
        }
        Ok(false)
    }

    /// Simple ping method
    pub async fn ping(&self) -> Result<()> {
        let mut conn = self.get_connection().await?;
        let _: String = self
            .bounded(redis::cmd("PING").query_async(&mut conn))
            .await?;
        Ok(())
    }

    /// Rate limiting: check if request is allowed
    ///
    /// Use atomic INCR + EXPIRE in Lua script to prevent race condition
    /// where key could expire between INCR and EXPIRE for existing keys
    pub async fn check_rate_limit(
        &self,
        key: &str,
        limit: u32,
        window_secs: u64,
    ) -> Result<RateLimitResult> {
        let mut conn = self.get_connection().await?;

        // Use Lua script for atomic operation
        // This prevents race condition where:
        // 1. Thread A: INCR (key doesn't exist, creates with value 1)
        // 2. Thread B: INCR (increments to 2)
        // 3. Key expires
        // 4. Thread A: EXPIRE (fails because key expired)
        let script = redis::Script::new(
            r#"
            local count = redis.call('INCR', KEYS[1])
            if count == 1 then
                redis.call('EXPIRE', KEYS[1], ARGV[1])
            end
            local ttl = redis.call('TTL', KEYS[1])
            if ttl < 0 then
                ttl = tonumber(ARGV[1])
            end
            return {count, ttl}
            "#,
        );

        let (count, ttl): (i64, i64) = self
            .bounded(
                script
                    .key(key)
                    .arg(window_secs as i64)
                    .invoke_async(&mut conn),
            )
            .await?;

        Ok(RateLimitResult {
            allowed: count <= limit as i64,
            remaining: (limit as i64 - count).max(0) as u32,
            reset_at: chrono::Utc::now() + chrono::Duration::seconds(ttl),
        })
    }

    /// Token bucket rate limiting: check if request is allowed
    ///
    /// Enforces an average refill rate (tokens/sec) with a burst capacity.
    /// Uses a Redis Lua script to ensure atomic update of (tokens, last_ts).
    ///
    /// - `key`: Redis key (hash) used to store bucket state
    /// - `refill_rate_per_sec`: tokens refilled per second (RPS)
    /// - `capacity`: maximum tokens (burst)
    pub async fn check_token_bucket(
        &self,
        key: &str,
        refill_rate_per_sec: u32,
        capacity: u32,
    ) -> Result<TokenBucketResult> {
        // Avoid division by zero; treat 0 as "blocked"
        let refill_rate_per_sec = refill_rate_per_sec.max(1);
        let capacity = capacity.max(1);

        let now_ms = (std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis()) as i64;

        // TTL: keep bucket state around for a bit (at least 60s),
        // or ~2x the time needed to refill from 0 to full.
        let refill_time_secs = (capacity as f64 / refill_rate_per_sec as f64).ceil() as i64;
        let ttl_ms = std::cmp::max(60, (refill_time_secs * 2).max(1)) * 1000;

        let mut conn = self.get_connection().await?;

        let script = redis::Script::new(
            r#"
            local key = KEYS[1]
            local now_ms = tonumber(ARGV[1])
            local rate = tonumber(ARGV[2]) -- tokens per second
            local capacity = tonumber(ARGV[3])
            local ttl_ms = tonumber(ARGV[4])

            local data = redis.call('HMGET', key, 'tokens', 'ts')
            local tokens = tonumber(data[1])
            local ts = tonumber(data[2])

            if tokens == nil then tokens = capacity end
            if ts == nil then ts = now_ms end

            local delta = now_ms - ts
            if delta < 0 then delta = 0 end

            local refill = (delta / 1000.0) * rate
            tokens = math.min(capacity, tokens + refill)

            local allowed = 0
            local retry_ms = 0
            if tokens >= 1.0 then
              allowed = 1
              tokens = tokens - 1.0
            else
              allowed = 0
              retry_ms = math.ceil((1.0 - tokens) * 1000.0 / rate)
            end

            -- Time until the next whole token, i.e. until the integer
            -- `remaining` goes up by one; 0 when the bucket is already full.
            local whole = math.floor(tokens)
            local next_ms = 0
            if whole < capacity then
              next_ms = math.ceil((whole + 1.0 - tokens) * 1000.0 / rate)
            end

            redis.call('HMSET', key, 'tokens', tokens, 'ts', now_ms)
            redis.call('PEXPIRE', key, ttl_ms)

            return {allowed, whole, retry_ms, next_ms}
            "#,
        );

        let (allowed, remaining, retry_ms, next_ms): (i64, i64, i64, i64) = self
            .bounded(
                script
                    .key(key)
                    .arg(now_ms)
                    .arg(refill_rate_per_sec as i64)
                    .arg(capacity as i64)
                    .arg(ttl_ms)
                    .invoke_async(&mut conn),
            )
            .await?;

        Ok(TokenBucketResult {
            allowed: allowed == 1,
            remaining: remaining.max(0) as u32,
            retry_after_ms: retry_ms.max(0) as u64,
            next_token_ms: next_ms.max(0) as u64,
        })
    }
}

/// Rate limit check result
#[derive(Debug, Clone)]
pub struct RateLimitResult {
    /// Whether the request is allowed
    pub allowed: bool,
    /// Remaining requests in window
    pub remaining: u32,
    /// When the rate limit resets
    pub reset_at: chrono::DateTime<chrono::Utc>,
}

/// Token bucket rate limit check result
#[derive(Debug, Clone)]
pub struct TokenBucketResult {
    /// Whether the request is allowed
    pub allowed: bool,
    /// Remaining tokens (approximate integer)
    pub remaining: u32,
    /// Suggested retry-after in milliseconds (0 if allowed)
    pub retry_after_ms: u64,
    /// Milliseconds until the next whole token, i.e. until `remaining` goes
    /// up by one (0 when the bucket is full). Equals `retry_after_ms` when the
    /// request was refused.
    pub next_token_ms: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redis_url_is_redacted_before_logging() {
        assert_eq!(
            redact_redis_url("redis://:s3cret@redis:6379/0"),
            "redis://redis:6379/0"
        );
        assert_eq!(
            redact_redis_url("redis://user:s3cret@redis:6379"),
            "redis://redis:6379"
        );
        assert_eq!(redact_redis_url("redis://redis:6379"), "redis://redis:6379");
        assert!(!redact_redis_url("not a url :s3cret@").contains("s3cret"));
    }

    #[test]
    fn test_rate_limit_result() {
        let result = RateLimitResult {
            allowed: true,
            remaining: 99,
            reset_at: chrono::Utc::now(),
        };
        assert!(result.allowed);
        assert_eq!(result.remaining, 99);
    }

    #[test]
    fn timeouts_in_a_row_replace_the_connection_and_an_answer_resets_the_count() {
        let tracker = TimeoutTracker::default();
        let start = Instant::now();
        for _ in 1..TIMEOUTS_BEFORE_RECONNECT {
            assert!(!tracker.timed_out(start));
        }
        assert!(
            tracker.timed_out(start),
            "the last timeout in a row replaces the connection"
        );

        // The count starts again after a reconnect, and an answer in between
        // starts it again too.
        let later = start + MIN_RECONNECT_INTERVAL;
        for _ in 1..TIMEOUTS_BEFORE_RECONNECT {
            assert!(!tracker.timed_out(later));
        }
        tracker.answered();
        for _ in 1..TIMEOUTS_BEFORE_RECONNECT {
            assert!(!tracker.timed_out(later));
        }
        assert!(tracker.timed_out(later));
    }

    #[test]
    fn forced_reconnects_are_spaced_by_the_minimum_interval() {
        let tracker = TimeoutTracker::default();
        let start = Instant::now();
        assert!(
            tracker.reconnect_due(start),
            "the first failed health check"
        );
        let soon = start + MIN_RECONNECT_INTERVAL / 2;
        assert!(!tracker.reconnect_due(soon));
        for _ in 0..TIMEOUTS_BEFORE_RECONNECT * 2 {
            assert!(
                !tracker.timed_out(soon),
                "too soon after the last reconnect"
            );
        }
        // Once the interval has passed, the timeouts already counted do.
        assert!(tracker.timed_out(start + MIN_RECONNECT_INTERVAL));
        assert!(!tracker.reconnect_due(start + MIN_RECONNECT_INTERVAL));
    }

    use std::sync::atomic::{AtomicBool, AtomicUsize};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// A minimal RESP server: PING gets PONG, GET gets nil, everything else
    /// OK. It can stop answering while keeping its sockets open, which is what
    /// a half-open TCP connection (Redis host gone without a reset) looks like
    /// to the client.
    struct FakeRedis {
        url: String,
        accepted: Arc<AtomicUsize>,
        /// Connections with an index below this never answer again.
        stalled_below: Arc<AtomicUsize>,
        /// When set, no connection answers, including new ones.
        stall_all: Arc<AtomicBool>,
    }

    impl FakeRedis {
        async fn start() -> Self {
            Self::start_on("127.0.0.1:0").await
        }

        async fn start_on(addr: &str) -> Self {
            let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
            let url = format!("redis://{}", listener.local_addr().unwrap());
            let accepted = Arc::new(AtomicUsize::new(0));
            let stalled_below = Arc::new(AtomicUsize::new(0));
            let stall_all = Arc::new(AtomicBool::new(false));
            let (count, below, all) = (accepted.clone(), stalled_below.clone(), stall_all.clone());
            tokio::spawn(async move {
                while let Ok((socket, _)) = listener.accept().await {
                    let index = count.fetch_add(1, Ordering::SeqCst);
                    tokio::spawn(serve_fake_connection(
                        socket,
                        index,
                        below.clone(),
                        all.clone(),
                    ));
                }
            });
            Self {
                url,
                accepted,
                stalled_below,
                stall_all,
            }
        }

        fn settings(&self) -> RedisSettings {
            RedisSettings {
                url: self.url.clone(),
                pool_size: 1,
            }
        }

        /// Every connection accepted so far goes silent; new ones still answer.
        fn stall_existing(&self) {
            self.stalled_below
                .store(self.accepted.load(Ordering::SeqCst), Ordering::SeqCst);
        }

        fn connections(&self) -> usize {
            self.accepted.load(Ordering::SeqCst)
        }
    }

    async fn serve_fake_connection(
        mut socket: tokio::net::TcpStream,
        index: usize,
        stalled_below: Arc<AtomicUsize>,
        stall_all: Arc<AtomicBool>,
    ) {
        let mut buf = Vec::new();
        let mut chunk = [0u8; 4096];
        loop {
            let n = match socket.read(&mut chunk).await {
                Ok(0) | Err(_) => return,
                Ok(n) => n,
            };
            buf.extend_from_slice(&chunk[..n]);
            while let Some((command, used)) = parse_resp_command(&buf) {
                buf.drain(..used);
                if stall_all.load(Ordering::SeqCst) || index < stalled_below.load(Ordering::SeqCst)
                {
                    continue;
                }
                let reply: &[u8] = match command.as_str() {
                    "PING" => b"+PONG\r\n",
                    "GET" => b"$-1\r\n",
                    _ => b"+OK\r\n",
                };
                if socket.write_all(reply).await.is_err() {
                    return;
                }
            }
        }
    }

    /// Parse one complete RESP array command from the front of `buf`: its
    /// upper-cased name and the number of bytes it used.
    fn parse_resp_command(buf: &[u8]) -> Option<(String, usize)> {
        fn line(buf: &[u8], from: usize) -> Option<(&[u8], usize)> {
            let end = from + buf.get(from..)?.windows(2).position(|w| w == b"\r\n")?;
            Some((&buf[from..end], end + 2))
        }
        fn number(raw: &[u8], prefix: u8) -> Option<usize> {
            std::str::from_utf8(raw.strip_prefix(&[prefix])?)
                .ok()?
                .parse()
                .ok()
        }
        let (header, mut pos) = line(buf, 0)?;
        let count = number(header, b'*')?;
        let mut name = String::new();
        for i in 0..count {
            let (len_line, start) = line(buf, pos)?;
            let len = number(len_line, b'$')?;
            if buf.len() < start + len + 2 {
                return None;
            }
            if i == 0 {
                name = String::from_utf8_lossy(&buf[start..start + len]).to_uppercase();
            }
            pos = start + len + 2;
        }
        Some((name, pos))
    }

    /// Generous upper bound for one cache call in the tests below; the
    /// configured bound is ~500 ms.
    const TEST_CALL_BOUND: Duration = Duration::from_millis(900);

    #[tokio::test]
    async fn half_open_connection_is_replaced_after_repeated_timeouts() {
        let fake = FakeRedis::start().await;
        let cache = RedisCache::connect(&fake.settings()).await.unwrap();
        assert_eq!(cache.get("k").await.unwrap(), None);
        assert_eq!(fake.connections(), 1);

        // The socket stays open but nothing comes back: every call times out
        // and the connection manager never reconnects on its own.
        fake.stall_existing();
        let mut recovered = false;
        for _ in 0..6 {
            let started = Instant::now();
            let result = cache.get("k").await;
            assert!(
                started.elapsed() < TEST_CALL_BOUND,
                "a cache call took {:?}",
                started.elapsed()
            );
            if result.is_ok() {
                recovered = true;
                break;
            }
        }
        assert!(
            recovered,
            "repeated timeouts must replace the half-open connection"
        );
        assert!(fake.connections() >= 2, "a new connection was opened");
    }

    #[tokio::test]
    async fn cache_calls_and_health_check_stay_bounded_while_redis_never_answers() {
        let fake = FakeRedis::start().await;
        let cache = RedisCache::connect(&fake.settings()).await.unwrap();

        // Redis accepts connections but answers nothing, also during the
        // reconnect handshake.
        fake.stall_all.store(true, Ordering::SeqCst);
        for round in 0..4 {
            let started = Instant::now();
            assert!(!cache.health_check().await.unwrap());
            assert!(
                started.elapsed() < TEST_CALL_BOUND,
                "health check {round} took {:?}",
                started.elapsed()
            );
            for _ in 0..2 {
                let started = Instant::now();
                assert!(cache.get("k").await.is_err());
                assert!(
                    started.elapsed() < TEST_CALL_BOUND,
                    "cache get in round {round} took {:?}",
                    started.elapsed()
                );
            }
        }

        // Once Redis answers again (on new connections; the old sockets stay
        // dead) the cache recovers by itself.
        fake.stall_existing();
        fake.stall_all.store(false, Ordering::SeqCst);
        let mut recovered = false;
        for _ in 0..10 {
            if cache.get("k").await.is_ok() {
                recovered = true;
                break;
            }
        }
        assert!(recovered, "the cache reconnects once Redis answers again");
    }

    // W3R-04: a Redis that is down when the process starts is kept as a
    // reconnecting client when it is required, and picked up once it answers;
    // giving up left the process without Redis until a restart.
    #[tokio::test]
    async fn startup_keeps_reconnecting_to_a_required_redis_that_is_down() {
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let settings = RedisSettings {
            url: format!("redis://127.0.0.1:{port}"),
            pool_size: 1,
        };

        assert!(
            RedisCache::connect_at_startup(&settings, false)
                .await
                .is_none(),
            "an optional Redis that is down is left out"
        );
        let cache = RedisCache::connect_at_startup(&settings, true)
            .await
            .expect("a required Redis is kept while it is down");
        assert!(!cache.health_check().await.unwrap_or(false));
        assert!(cache.get("k").await.is_err());

        let _redis = FakeRedis::start_on(&format!("127.0.0.1:{port}")).await;
        let mut recovered = false;
        for _ in 0..40 {
            if cache.health_check().await.unwrap_or(false) && cache.get("k").await.is_ok() {
                recovered = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
        assert!(recovered, "the cache connects once Redis answers");
    }
}
