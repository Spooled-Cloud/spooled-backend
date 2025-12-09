//! Cache module for Spooled Backend
//!
//! This module provides Redis-based caching and pub/sub functionality.
//! It gracefully degrades when Redis is unavailable.

use anyhow::{Context, Result};
use redis::aio::MultiplexedConnection;
use redis::{AsyncCommands, Client};
use std::sync::Arc;
use tracing::{debug, info, warn};

use crate::config::RedisSettings;

/// Redis cache wrapper
#[derive(Clone)]
pub struct RedisCache {
    client: Client,
    connection: Arc<tokio::sync::RwLock<Option<MultiplexedConnection>>>,
}

impl RedisCache {
    /// Connect to Redis with the given settings
    pub async fn connect(settings: &RedisSettings) -> Result<Self> {
        info!(url = %settings.url, "Connecting to Redis");

        let client =
            Client::open(settings.url.as_str()).context("Failed to create Redis client")?;

        let connection = client
            .get_multiplexed_async_connection()
            .await
            .context("Failed to connect to Redis")?;

        // Test connection
        let mut conn = connection.clone();
        let _: String = redis::cmd("PING")
            .query_async(&mut conn)
            .await
            .context("Failed to ping Redis")?;

        info!("Redis connection established");

        Ok(Self {
            client,
            connection: Arc::new(tokio::sync::RwLock::new(Some(connection))),
        })
    }

    /// Get a connection, reconnecting if necessary
    ///
    pub async fn get_connection(&self) -> Result<MultiplexedConnection> {
        {
            let guard = self.connection.read().await;
            if let Some(conn) = guard.as_ref() {
                return Ok(conn.clone());
            }
        }

        // Reconnect with timeout
        let mut guard = self.connection.write().await;
        if guard.is_none() {
            info!("Reconnecting to Redis");
            // Add timeout to prevent indefinite hang
            let conn_future = self.client.get_multiplexed_async_connection();
            let conn = tokio::time::timeout(std::time::Duration::from_secs(10), conn_future)
                .await
                .context("Redis connection timeout")?
                .context("Failed to reconnect to Redis")?;
            *guard = Some(conn);
        }

        Ok(guard.as_ref().unwrap().clone())
    }

    /// Get a value from cache
    pub async fn get(&self, key: &str) -> Result<Option<String>> {
        let mut conn = self.get_connection().await?;
        let result: Option<String> = conn.get(key).await?;
        debug!(key = %key, hit = result.is_some(), "Cache get");
        Ok(result)
    }

    /// Set a value in cache with TTL
    pub async fn set(&self, key: &str, value: &str, ttl_secs: u64) -> Result<()> {
        let mut conn = self.get_connection().await?;
        let _: () = conn.set_ex(key, value, ttl_secs).await?;
        debug!(key = %key, ttl = ttl_secs, "Cache set");
        Ok(())
    }

    /// Delete a value from cache
    pub async fn delete(&self, key: &str) -> Result<()> {
        let mut conn = self.get_connection().await?;
        let _: () = conn.del(key).await?;
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
            let (new_cursor, keys): (u64, Vec<String>) = redis::cmd("SCAN")
                .arg(cursor)
                .arg("MATCH")
                .arg(pattern)
                .arg("COUNT")
                .arg(Self::SCAN_BATCH_SIZE)
                .query_async(&mut conn)
                .await?;

            // Delete found keys using UNLINK for non-blocking deletion
            if !keys.is_empty() {
                // Use UNLINK for async deletion (Redis 4.0+)
                // Falls back to DEL if UNLINK fails (older Redis)
                let count: u64 = redis::cmd("UNLINK")
                    .arg(&keys)
                    .query_async(&mut conn)
                    .await
                    .unwrap_or(0);

                // If UNLINK failed, try DEL
                let count = if count == 0 && !keys.is_empty() {
                    conn.del::<_, u64>(&keys).await.unwrap_or(0)
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
            let (new_cursor, keys): (u64, Vec<String>) = redis::cmd("SCAN")
                .arg(cursor)
                .arg("MATCH")
                .arg(pattern)
                .arg("COUNT")
                .arg(100)
                .query_async(&mut conn)
                .await?;

            if !keys.is_empty() {
                let count: u64 = conn.del(&keys).await?;
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
        let exists: bool = conn.exists(key).await?;
        Ok(exists)
    }

    /// Increment a counter
    pub async fn incr(&self, key: &str) -> Result<i64> {
        let mut conn = self.get_connection().await?;
        let value: i64 = conn.incr(key, 1).await?;
        Ok(value)
    }

    /// Decrement a counter (minimum 0)
    pub async fn decr(&self, key: &str) -> Result<i64> {
        let mut conn = self.get_connection().await?;
        let value: i64 = conn.decr(key, 1).await?;
        // Ensure value doesn't go negative
        if value < 0 {
            let _: () = conn.set(key, 0i64).await?;
            return Ok(0);
        }
        Ok(value)
    }

    /// Increment a counter with TTL (for connection counting)
    pub async fn incr_with_ttl(&self, key: &str, ttl_secs: u64) -> Result<i64> {
        let mut conn = self.get_connection().await?;
        // Use pipeline for atomicity
        let (value, _): (i64, ()) = redis::pipe()
            .atomic()
            .incr(key, 1)
            .expire(key, ttl_secs as i64)
            .query_async(&mut conn)
            .await?;
        Ok(value)
    }

    /// Increment a counter and set expiration (atomic for rate limiting)
    ///
    pub async fn increment(&self, key: &str, ttl_secs: u64) -> Result<i64> {
        let mut conn = self.get_connection().await?;
        // Use pipeline for atomicity
        let (value, _): (i64, ()) = redis::pipe()
            .atomic()
            .incr(key, 1)
            .expire(key, ttl_secs as i64)
            .query_async(&mut conn)
            .await?;
        Ok(value)
    }

    /// Set expiration on a key
    pub async fn expire(&self, key: &str, ttl_secs: u64) -> Result<()> {
        let mut conn = self.get_connection().await?;
        let _: () = conn.expire(key, ttl_secs as i64).await?;
        Ok(())
    }

    /// Publish a message to a channel
    pub async fn publish(&self, channel: &str, message: &str) -> Result<()> {
        let mut conn = self.get_connection().await?;
        let _: () = conn.publish(channel, message).await?;
        debug!(channel = %channel, "Published message");
        Ok(())
    }

    /// Subscribe to a channel (returns a PubSub handle)
    pub async fn subscribe(&self, channel: &str) -> Result<redis::aio::PubSub> {
        let pubsub = self
            .client
            .get_async_pubsub()
            .await
            .context("Failed to create pubsub connection")?;

        let mut pubsub = pubsub;
        pubsub.subscribe(channel).await?;
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
    pub async fn health_check(&self) -> Result<bool> {
        match self.get_connection().await {
            Ok(mut conn) => match redis::cmd("PING").query_async::<String>(&mut conn).await {
                Ok(_) => Ok(true),
                Err(e) => {
                    warn!(error = %e, "Redis health check failed");
                    Ok(false)
                }
            },
            Err(e) => {
                warn!(error = %e, "Redis connection failed");
                Ok(false)
            }
        }
    }

    /// Simple ping method
    pub async fn ping(&self) -> Result<()> {
        let mut conn = self.get_connection().await?;
        let _: String = redis::cmd("PING").query_async(&mut conn).await?;
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

        let (count, ttl): (i64, i64) = script
            .key(key)
            .arg(window_secs as i64)
            .invoke_async(&mut conn)
            .await?;

        Ok(RateLimitResult {
            allowed: count <= limit as i64,
            remaining: (limit as i64 - count).max(0) as u32,
            reset_at: chrono::Utc::now() + chrono::Duration::seconds(ttl),
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

#[cfg(test)]
mod tests {
    use super::*;

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
}
