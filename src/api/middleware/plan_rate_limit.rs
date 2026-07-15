//! Plan-aware rate limiting middleware
//!
//! Enforces API rate limits based on:
//! - Organization plan tier limits (tokens/sec + burst)
//! - Optional per-API-key override (requests per minute)
//!
//! Uses Redis token bucket for plan limits and Redis fixed-window counter for per-key override.

use axum::{
    body::Body,
    extract::State,
    http::{Request, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
};
use serde::Serialize;
use tracing::{debug, warn};

use crate::api::AppState;
use crate::config::PlanLimits;
use crate::models::ApiKeyContext;

/// Rate limit exceeded response (shared shape with existing middleware)
#[derive(Debug, Serialize)]
struct RateLimitExceededResponse {
    pub error: String,
    pub code: String,
    pub retry_after_secs: u64,
    pub limit: u32,
    pub remaining: u32,
    pub reset_at: u64,
}

#[derive(Debug, Clone, Serialize, serde::Deserialize)]
struct OrgRateLimits {
    rps: u32,
    burst: u32,
}

fn current_timestamp() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn rate_limit_unavailable_response(retry_after_secs: u64) -> Response {
    let body = RateLimitExceededResponse {
        error: "Rate limit unavailable".to_string(),
        code: "RATE_LIMIT_EXCEEDED".to_string(),
        retry_after_secs,
        limit: 0,
        remaining: 0,
        reset_at: current_timestamp() + retry_after_secs,
    };
    let mut res = (StatusCode::TOO_MANY_REQUESTS, axum::Json(body)).into_response();
    if let Ok(v) = retry_after_secs.to_string().parse() {
        res.headers_mut().insert("Retry-After", v);
    }
    res
}

/// Extract a best-effort client id for unauthenticated routes (IP-based).
fn extract_client_id(request: &Request<Body>) -> String {
    // X-Forwarded-For (first IP) if present
    if let Some(forwarded) = request
        .headers()
        .get("X-Forwarded-For")
        .and_then(|v| v.to_str().ok())
    {
        if let Some(ip) = forwarded.split(',').next() {
            let trimmed = ip.trim();
            if !trimmed.is_empty() && trimmed.len() < 46 {
                return format!("ip:{}", trimmed);
            }
        }
    }

    // Fall back to user agent hash (reduces global grouping)
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

    // Worst-case fallback: group unknown clients together (don't generate a random key,
    // otherwise callers can bypass rate limiting by omitting identifying headers).
    "unknown".to_string()
}

async fn get_org_rate_limits(state: &AppState, org_id: &str) -> Result<OrgRateLimits, ()> {
    // Cache in Redis for 60s to avoid DB hit per request.
    let cache_key = format!("org_rate_limits:{}", org_id);

    if let Some(ref cache) = state.cache {
        if let Ok(Some(cached)) = cache.get_json::<OrgRateLimits>(&cache_key).await {
            return Ok(cached);
        }
    }

    let row: (String, Option<serde_json::Value>) = sqlx::query_as(
        "SELECT plan_tier, custom_limits FROM organizations WHERE id = $1",
    )
    .bind(org_id)
    .fetch_one(state.db.pool())
    .await
    .map_err(|e| {
        warn!(error = %e, org_id = %org_id, "Failed to fetch org plan limits for rate limiting");
    })?;

    let limits = PlanLimits::for_tier_with_overrides(&row.0, row.1.as_ref());
    let out = OrgRateLimits {
        rps: limits.rate_limit_requests_per_second.max(1),
        burst: limits.rate_limit_burst.max(1),
    };

    if let Some(ref cache) = state.cache {
        let _ = cache.set_json(&cache_key, &out, 60).await;
    }

    Ok(out)
}

/// Plan-aware rate limiting middleware.
///
/// Apply this **after auth** on protected routes so we can use ApiKeyContext.
pub async fn plan_rate_limit_middleware(
    State(state): State<AppState>,
    request: Request<Body>,
    next: Next,
) -> Response {
    let fail_closed = state.settings.rate_limit.fail_closed_on_redis_error;

    // If Redis isn't configured: self-host default allows; SaaS fail-closed denies.
    let Some(ref cache) = state.cache else {
        if fail_closed {
            warn!("Redis not configured; denying request (RATE_LIMIT_FAIL_CLOSED)");
            return rate_limit_unavailable_response(5);
        }
        return next.run(request).await;
    };

    // If we have org context (protected routes), apply plan-based org limit + optional per-key override.
    if let Some(ctx) = request.extensions().get::<ApiKeyContext>().cloned() {
        let org_limits = match get_org_rate_limits(&state, &ctx.organization_id).await {
            Ok(v) => v,
            Err(_) => {
                // If we can't load limits, fail closed with a small fallback.
                OrgRateLimits { rps: 5, burst: 10 }
            }
        };

        // 1) Org-level token bucket (plan rps/burst)
        let org_key = format!("rate_limit:org:{}", ctx.organization_id);
        match cache
            .check_token_bucket(&org_key, org_limits.rps, org_limits.burst)
            .await
        {
            Ok(result) => {
                if !result.allowed {
                    let retry_after_secs = result.retry_after_ms.div_ceil(1000);
                    let reset_at = current_timestamp() + retry_after_secs;
                    let body = RateLimitExceededResponse {
                        error: "Rate limit exceeded".to_string(),
                        code: "RATE_LIMIT_EXCEEDED".to_string(),
                        retry_after_secs,
                        limit: org_limits.burst,
                        remaining: 0,
                        reset_at,
                    };
                    let mut res = (StatusCode::TOO_MANY_REQUESTS, axum::Json(body)).into_response();
                    if let Ok(v) = retry_after_secs.to_string().parse() {
                        res.headers_mut().insert("Retry-After", v);
                    }
                    return res;
                }
            }
            Err(e) => {
                if fail_closed {
                    warn!(
                        error = %e,
                        org_id = %ctx.organization_id,
                        "Redis error during org rate limit check; denying (fail-closed)"
                    );
                    return rate_limit_unavailable_response(5);
                }
                warn!(error = %e, org_id = %ctx.organization_id, "Redis error during org rate limit check; allowing");
            }
        }

        // 2) Optional per-API-key override (requests per minute)
        if let Some(per_minute) = ctx.rate_limit {
            let key_limit = per_minute.max(1) as u32;
            let key = format!("rate_limit:api_key:{}", ctx.api_key_id);
            match cache.check_rate_limit(&key, key_limit, 60).await {
                Ok(result) => {
                    if !result.allowed {
                        let retry_after_secs =
                            (result.reset_at - chrono::Utc::now()).num_seconds().max(1) as u64;
                        let reset_at = current_timestamp() + retry_after_secs;
                        let body = RateLimitExceededResponse {
                            error: "Rate limit exceeded".to_string(),
                            code: "RATE_LIMIT_EXCEEDED".to_string(),
                            retry_after_secs,
                            limit: key_limit,
                            remaining: 0,
                            reset_at,
                        };
                        let mut res =
                            (StatusCode::TOO_MANY_REQUESTS, axum::Json(body)).into_response();
                        if let Ok(v) = retry_after_secs.to_string().parse() {
                            res.headers_mut().insert("Retry-After", v);
                        }
                        return res;
                    }
                }
                Err(e) => {
                    if fail_closed {
                        warn!(
                            error = %e,
                            api_key_id = %ctx.api_key_id,
                            "Redis error during api-key rate limit check; denying (fail-closed)"
                        );
                        return rate_limit_unavailable_response(5);
                    }
                    warn!(
                        error = %e,
                        api_key_id = %ctx.api_key_id,
                        "Redis error during api-key rate limit check; allowing"
                    );
                }
            }
        }

        // Allowed -> proceed
        return next.run(request).await;
    }

    // Public routes: apply a simple global rate limit (settings-based) keyed by IP/UA.
    let client_id = extract_client_id(&request);
    let rps = state.settings.rate_limit.requests_per_second.max(1);
    let burst = state.settings.rate_limit.burst_size.max(1);
    let key = format!("rate_limit:public:{}", client_id);
    match cache.check_token_bucket(&key, rps, burst).await {
        Ok(result) => {
            if !result.allowed {
                let retry_after_secs = result.retry_after_ms.div_ceil(1000);
                let reset_at = current_timestamp() + retry_after_secs;
                let body = RateLimitExceededResponse {
                    error: "Rate limit exceeded".to_string(),
                    code: "RATE_LIMIT_EXCEEDED".to_string(),
                    retry_after_secs,
                    limit: burst,
                    remaining: 0,
                    reset_at,
                };
                let mut res = (StatusCode::TOO_MANY_REQUESTS, axum::Json(body)).into_response();
                if let Ok(v) = retry_after_secs.to_string().parse() {
                    res.headers_mut().insert("Retry-After", v);
                }
                return res;
            }
        }
        Err(e) => {
            if fail_closed {
                warn!(error = %e, "Redis error during public rate limit check; denying (fail-closed)");
                return rate_limit_unavailable_response(5);
            }
            debug!(error = %e, "Redis error during public rate limit check; allowing request");
        }
    }

    next.run(request).await
}

/// Rate-limit admin routes by client IP (brute-force cushion).
/// Uses Redis fixed-window when available; fail-open by default if Redis is
/// missing/errors (self-host). Set `RATE_LIMIT_FAIL_CLOSED=true` for SaaS.
pub async fn admin_rate_limit_middleware(
    State(state): State<AppState>,
    request: Request<Body>,
    next: Next,
) -> Response {
    let fail_closed = state.settings.rate_limit.fail_closed_on_redis_error;
    let Some(ref cache) = state.cache else {
        if fail_closed {
            warn!("Redis not configured; denying admin request (RATE_LIMIT_FAIL_CLOSED)");
            return rate_limit_unavailable_response(5);
        }
        return next.run(request).await;
    };

    const WINDOW_SECS: u64 = 60;
    const MAX_REQUESTS: u32 = 60;

    let client_id = extract_client_id(&request);
    let key = format!("rate_limit:admin:{}", client_id);
    match cache
        .check_rate_limit(&key, MAX_REQUESTS, WINDOW_SECS)
        .await
    {
        Ok(result) => {
            if !result.allowed {
                let retry_after_secs =
                    (result.reset_at - chrono::Utc::now()).num_seconds().max(1) as u64;
                let body = RateLimitExceededResponse {
                    error: "Rate limit exceeded".to_string(),
                    code: "RATE_LIMIT_EXCEEDED".to_string(),
                    retry_after_secs,
                    limit: MAX_REQUESTS,
                    remaining: 0,
                    reset_at: current_timestamp() + retry_after_secs,
                };
                let mut res = (StatusCode::TOO_MANY_REQUESTS, axum::Json(body)).into_response();
                if let Ok(v) = retry_after_secs.to_string().parse() {
                    res.headers_mut().insert("Retry-After", v);
                }
                return res;
            }
        }
        Err(e) => {
            if fail_closed {
                warn!(error = %e, "Redis error during admin rate limit check; denying (fail-closed)");
                return rate_limit_unavailable_response(5);
            }
            debug!(error = %e, "Redis error during admin rate limit check; allowing request");
        }
    }

    next.run(request).await
}
