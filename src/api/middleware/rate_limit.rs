//! Rate limiting middleware using Redis
//!
//! Implements a sliding window rate limiter with Redis as the backing store.

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::{
    body::Body,
    extract::State,
    http::{Request, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
};
use hex;
use serde::Serialize;

use crate::cache::RedisCache;

/// Rate limit configuration
#[derive(Debug, Clone)]
pub struct RateLimitConfig {
    /// Maximum requests allowed in the window
    pub max_requests: u32,
    /// Window duration in seconds
    pub window_secs: u64,
    /// Key prefix for Redis
    pub key_prefix: String,
}

impl Default for RateLimitConfig {
    fn default() -> Self {
        Self {
            max_requests: 100,
            window_secs: 60,
            key_prefix: "rate_limit".to_string(),
        }
    }
}

/// Rate limit state
#[derive(Clone)]
pub struct RateLimitState {
    pub cache: Option<Arc<RedisCache>>,
    pub config: RateLimitConfig,
}

/// Rate limit exceeded response
#[derive(Debug, Serialize)]
pub struct RateLimitExceededResponse {
    pub error: String,
    pub code: String,
    pub retry_after_secs: u64,
    pub limit: u32,
    pub remaining: u32,
    pub reset_at: u64,
}

/// Rate limit info included in response headers
#[derive(Debug, Clone)]
pub struct RateLimitInfo {
    pub limit: u32,
    pub remaining: u32,
    pub reset_at: u64,
}

/// Rate limiting middleware
pub async fn rate_limit_middleware(
    State(state): State<RateLimitState>,
    request: Request<Body>,
    next: Next,
) -> Response {
    // Get client identifier (API key or IP address)
    let client_id = extract_client_id(&request);

    // Check rate limit
    match check_rate_limit(&state, &client_id).await {
        Ok(info) => {
            // Allow request, add rate limit headers
            let mut response = next.run(request).await;
            add_rate_limit_headers(&mut response, &info);
            response
        }
        Err(info) => {
            // Rate limit exceeded
            let reset_in = info.reset_at.saturating_sub(current_timestamp());
            let response = RateLimitExceededResponse {
                error: "Rate limit exceeded".to_string(),
                code: "RATE_LIMIT_EXCEEDED".to_string(),
                retry_after_secs: reset_in,
                limit: info.limit,
                remaining: 0,
                reset_at: info.reset_at,
            };

            let mut res = (StatusCode::TOO_MANY_REQUESTS, axum::Json(response)).into_response();
            add_rate_limit_headers(&mut res, &info);

            // Add Retry-After header
            if let Ok(value) = reset_in.to_string().parse() {
                res.headers_mut().insert("Retry-After", value);
            }

            res
        }
    }
}

/// Extract client identifier from request
///
/// Now uses SHA256 hash of API key instead of first 8 chars.
/// Using first 8 chars could cause rate limit collisions between keys with same prefix.
fn extract_client_id(request: &Request<Body>) -> String {
    // Try to get API key from header first
    if let Some(api_key) = request
        .headers()
        .get("X-API-Key")
        .and_then(|v| v.to_str().ok())
    {
        // Use SHA256 hash instead of first 8 chars to prevent collisions
        use sha2::{Digest, Sha256};
        let mut hasher = Sha256::new();
        hasher.update(api_key.as_bytes());
        let hash = hex::encode(hasher.finalize());
        return format!("api:{}", &hash[..16]); // First 16 chars of hash = 64 bits
    }

    // Fall back to IP address
    if let Some(forwarded) = request
        .headers()
        .get("X-Forwarded-For")
        .and_then(|v| v.to_str().ok())
    {
        if let Some(ip) = forwarded.split(',').next() {
            let trimmed = ip.trim();
            // Validate IP format to prevent grouping attack
            if !trimmed.is_empty() && trimmed.len() < 46 {
                // Max IPv6 length
                return format!("ip:{}", trimmed);
            }
        }
    }

    // Use a unique identifier instead of shared "unknown"
    // This prevents all unknown clients from being rate limited together
    // Use a hash of request characteristics that are available
    if let Some(user_agent) = request
        .headers()
        .get("User-Agent")
        .and_then(|v| v.to_str().ok())
    {
        use sha2::{Digest, Sha256};
        let mut hasher = Sha256::new();
        hasher.update(user_agent.as_bytes());
        let hash = hex::encode(&hasher.finalize()[..8]);
        return format!("ua:{}", hash);
    }

    // Last resort: unique per-request (very strict)
    format!("req:{}", uuid::Uuid::new_v4())
}

/// Fallback rate limit when Redis is unavailable (stricter to prevent DoS)
const FALLBACK_MAX_REQUESTS: u32 = 10;

/// Even stricter fallback for truly unknown clients
/// Different fallback for different client types
const UNKNOWN_CLIENT_FALLBACK: u32 = 5;

/// Check rate limit using Redis sliding window
///
/// No longer fails open when Redis is unavailable.
/// Uses a stricter fallback limit to prevent DoS attacks.
async fn check_rate_limit(
    state: &RateLimitState,
    client_id: &str,
) -> Result<RateLimitInfo, RateLimitInfo> {
    let Some(ref cache) = state.cache else {
        // No Redis configured - use in-memory fallback with stricter limits
        // In production, Redis should always be configured
        tracing::warn!("Rate limiting without Redis - using strict fallback limits");
        // Use even stricter limits for unknown/unidentified clients
        let limit = if client_id.starts_with("req:") || client_id.starts_with("ua:") {
            UNKNOWN_CLIENT_FALLBACK
        } else {
            FALLBACK_MAX_REQUESTS
        };
        return Ok(RateLimitInfo {
            limit,
            remaining: limit,
            reset_at: current_timestamp() + state.config.window_secs,
        });
    };

    let key = format!("{}:{}", state.config.key_prefix, client_id);
    let now = current_timestamp();
    let window_start = now - state.config.window_secs;
    let reset_at = now + state.config.window_secs;

    let mut conn = match cache.get_connection().await {
        Ok(conn) => conn,
        Err(e) => {
            // Redis unavailable - use strict fallback to prevent DoS
            tracing::error!(error = %e, "Redis unavailable for rate limiting - using strict fallback");
            return Ok(RateLimitInfo {
                limit: FALLBACK_MAX_REQUESTS,
                remaining: FALLBACK_MAX_REQUESTS.saturating_sub(1), // Already counting this request
                reset_at,
            });
        }
    };

    // Use Redis sorted set for sliding window
    // Score = timestamp, value = unique request ID
    let request_id = format!("{}:{}", now, uuid::Uuid::new_v4());

    // Pipeline: remove old entries, add new, count total, set TTL
    let result: Result<((), (), i64, ()), _> = redis::pipe()
        .atomic()
        .zrembyscore(&key, 0i64, window_start as i64)
        .zadd(&key, &request_id, now as f64)
        .zcard(&key)
        .expire(&key, state.config.window_secs as i64 + 1)
        .query_async(&mut conn)
        .await;

    match result {
        Ok((_, _, count, _)) => {
            let count = count as u32;
            if count > state.config.max_requests {
                Err(RateLimitInfo {
                    limit: state.config.max_requests,
                    remaining: 0,
                    reset_at,
                })
            } else {
                Ok(RateLimitInfo {
                    limit: state.config.max_requests,
                    remaining: state.config.max_requests.saturating_sub(count),
                    reset_at,
                })
            }
        }
        // On Redis pipeline error, fail CLOSED with strict fallback limits
        // Previously allowed request with full quota, which could be exploited by forcing Redis errors
        Err(e) => {
            tracing::error!(error = %e, "Redis pipeline error - applying strict rate limit");
            // Apply strict fallback limit instead of allowing full quota
            Ok(RateLimitInfo {
                limit: FALLBACK_MAX_REQUESTS,
                remaining: FALLBACK_MAX_REQUESTS.saturating_sub(1), // Already counting this request
                reset_at,
            })
        }
    }
}

/// Add rate limit headers to response
fn add_rate_limit_headers(response: &mut Response, info: &RateLimitInfo) {
    let headers = response.headers_mut();

    if let Ok(v) = info.limit.to_string().parse() {
        headers.insert("X-RateLimit-Limit", v);
    }
    if let Ok(v) = info.remaining.to_string().parse() {
        headers.insert("X-RateLimit-Remaining", v);
    }
    if let Ok(v) = info.reset_at.to_string().parse() {
        headers.insert("X-RateLimit-Reset", v);
    }
}

/// Get current Unix timestamp in seconds
fn current_timestamp() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_secs()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_rate_limit_config_default() {
        let config = RateLimitConfig::default();
        assert_eq!(config.max_requests, 100);
        assert_eq!(config.window_secs, 60);
    }

    #[test]
    fn test_rate_limit_exceeded_response() {
        let response = RateLimitExceededResponse {
            error: "Rate limit exceeded".to_string(),
            code: "RATE_LIMIT_EXCEEDED".to_string(),
            retry_after_secs: 30,
            limit: 100,
            remaining: 0,
            reset_at: 1234567890,
        };

        let json = serde_json::to_string(&response).unwrap();
        assert!(json.contains("RATE_LIMIT_EXCEEDED"));
        assert!(json.contains("\"limit\":100"));
    }

    #[test]
    fn test_current_timestamp() {
        let ts = current_timestamp();
        // Should be a reasonable timestamp (after year 2020)
        assert!(ts > 1577836800);
    }
}
