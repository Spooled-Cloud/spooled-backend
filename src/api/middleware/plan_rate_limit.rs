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
    http::{header, HeaderMap, HeaderValue, Request, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
};
use serde::Serialize;
use tracing::{debug, warn};

use crate::api::middleware::client_ip::{client_ip_from_peer, peer_addr};
use crate::api::AppState;
use crate::config::{PlanLimits, TrustedProxySettings};
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

/// `X-RateLimit-Limit`: the size of the limit that answered (see [`RateLimitHeaders`]).
pub const HEADER_LIMIT: &str = "x-ratelimit-limit";
/// `X-RateLimit-Remaining`: requests that may still be made right now.
pub const HEADER_REMAINING: &str = "x-ratelimit-remaining";
/// `X-RateLimit-Reset`: Unix time in seconds at which `X-RateLimit-Remaining`
/// next goes up (the current time when nothing is spent). It is the same
/// instant as `reset_at` in a `429` body, and the SDKs read it as an epoch.
pub const HEADER_RESET: &str = "x-ratelimit-reset";
/// `X-RateLimit-Scope`: which limit the other three describe.
pub const HEADER_SCOPE: &str = "x-ratelimit-scope";

/// Which limiter the `X-RateLimit-*` headers describe.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RateLimitScope {
    /// The organization's plan token bucket: `limit` is the burst (bucket
    /// size), `reset_secs` the time until the next whole token.
    Organization,
    /// The API key's own `rate_limit` (requests per minute, fixed one-minute
    /// window): `reset_secs` is the time until the window starts again.
    ApiKey,
}

impl RateLimitScope {
    fn as_str(self) -> &'static str {
        match self {
            RateLimitScope::Organization => "organization",
            RateLimitScope::ApiKey => "api_key",
        }
    }
}

/// `X-RateLimit-*` values for one authenticated request, taken from the
/// limiter result that request was counted against.
///
/// `reset_secs` is the wait until `X-RateLimit-Remaining` next goes up, rounded
/// up: for the organization bucket that is the next whole token (`0` when the
/// bucket is full), for an API key's per-minute limit it is the start of the
/// next window. On a `429` it equals `Retry-After`.
///
/// `X-RateLimit-Reset` carries that moment as a Unix time in seconds (the
/// current time plus `reset_secs`), not the wait itself: every Spooled SDK
/// parses the header as an epoch, and a `429` body's `reset_at` is the same
/// value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RateLimitHeaders {
    pub scope: RateLimitScope,
    pub limit: u32,
    pub remaining: u32,
    pub reset_secs: u64,
}

impl RateLimitHeaders {
    /// From the organization token bucket of size `capacity`.
    pub fn from_token_bucket(capacity: u32, result: &crate::cache::TokenBucketResult) -> Self {
        Self {
            scope: RateLimitScope::Organization,
            limit: capacity,
            remaining: result.remaining.min(capacity),
            reset_secs: result.next_token_ms.div_ceil(1000),
        }
    }

    /// From an API key's fixed-window counter of `limit` requests, at `now`.
    pub fn from_window(
        limit: u32,
        result: &crate::cache::RateLimitResult,
        now: chrono::DateTime<chrono::Utc>,
    ) -> Self {
        let until_reset_ms = (result.reset_at - now).num_milliseconds().max(0) as u64;
        Self {
            scope: RateLimitScope::ApiKey,
            limit,
            remaining: result.remaining.min(limit),
            reset_secs: until_reset_ms.div_ceil(1000),
        }
    }

    /// The values sent with a refused request: nothing remains, and the reset
    /// is the same wait as `Retry-After`.
    pub fn refused(self, retry_after_secs: u64) -> Self {
        Self {
            remaining: 0,
            reset_secs: retry_after_secs,
            ..self
        }
    }

    /// The limit that runs out first: fewer requests remaining, and on a tie
    /// the one that takes longer to come back.
    pub fn tighter(self, other: Self) -> Self {
        if (other.remaining, std::cmp::Reverse(other.reset_secs))
            < (self.remaining, std::cmp::Reverse(self.reset_secs))
        {
            other
        } else {
            self
        }
    }

    /// The `X-RateLimit-Reset` value at Unix time `now` (seconds): when
    /// `X-RateLimit-Remaining` next goes up, `now` itself for a full bucket.
    pub fn reset_at(&self, now: u64) -> u64 {
        now.saturating_add(self.reset_secs)
    }

    /// Set the four headers on `headers`, as of the current time.
    pub fn apply(&self, headers: &mut HeaderMap) {
        self.apply_at(headers, current_timestamp());
    }

    /// Set the four headers on `headers`, as of Unix time `now` (seconds).
    pub fn apply_at(&self, headers: &mut HeaderMap, now: u64) {
        headers.insert(HEADER_LIMIT, HeaderValue::from(self.limit));
        headers.insert(HEADER_REMAINING, HeaderValue::from(self.remaining));
        headers.insert(HEADER_RESET, HeaderValue::from(self.reset_at(now)));
        headers.insert(HEADER_SCOPE, HeaderValue::from_static(self.scope.as_str()));
    }
}

/// `429 RATE_LIMIT_EXCEEDED` with `Retry-After`, plus the `X-RateLimit-*`
/// headers when the refusing limiter is known. The body's `reset_at` and
/// `X-RateLimit-Reset` are taken from one clock reading, so they match.
fn rate_limit_exceeded_response(
    retry_after_secs: u64,
    limit: u32,
    headers: Option<RateLimitHeaders>,
) -> Response {
    let now = current_timestamp();
    let body = RateLimitExceededResponse {
        error: "Rate limit exceeded".to_string(),
        code: "RATE_LIMIT_EXCEEDED".to_string(),
        retry_after_secs,
        limit,
        remaining: 0,
        reset_at: now + retry_after_secs,
    };
    let mut res = (StatusCode::TOO_MANY_REQUESTS, axum::Json(body)).into_response();
    res.headers_mut()
        .insert(header::RETRY_AFTER, HeaderValue::from(retry_after_secs));
    if let Some(headers) = headers {
        headers.apply_at(res.headers_mut(), now);
    }
    res
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

/// Client id for unauthenticated routes, derived from a TRUSTED address only.
///
/// Delegates to [`crate::api::middleware::client_ip`], which reads the
/// configured edge header and counts `X-Forwarded-For` from the right. The
/// previous implementation took the LEFTMOST `X-Forwarded-For` entry — the one
/// the caller supplies — so any client could land in a fresh bucket per request
/// and the limiter guarding `/auth/login`, `/auth/email/*`, `/organizations`
/// and the admin routes was effectively absent.
///
/// The User-Agent fallback was also removed: it is client-controlled too, so it
/// provided the same bypass one step down.
///
/// The TCP peer decides whether the default edge headers are believed at all
/// (a client reaching the backend directly could otherwise name its own bucket
/// with `CF-Connecting-IP`), and an IPv6 caller is keyed on its /64.
fn extract_client_id(request: &Request<Body>, settings: &TrustedProxySettings) -> String {
    let peer = peer_addr(request.extensions());
    format!(
        "ip:{}",
        client_ip_from_peer(request.headers(), peer, settings)
    )
}

/// Redis key holding an org's cached plan rate limits.
///
/// Single source of truth: the key was previously formatted independently in
/// three modules (here, and both gRPC services), so an invalidation added in one
/// place would silently miss the others.
pub fn org_rate_limits_key(org_id: &str) -> String {
    format!("org_rate_limits:{}", org_id)
}

/// Drop an org's cached plan limits.
///
/// Must be called whenever `plan_tier` or `custom_limits` changes — including
/// soft/hard delete — otherwise the org keeps its previous rps/burst for up to
/// the 60 s TTL. Best-effort: a cache miss is harmless (next request re-reads
/// from the DB), so failures are logged, not propagated.
pub async fn invalidate_org_rate_limits(cache: Option<&crate::cache::RedisCache>, org_id: &str) {
    if let Some(cache) = cache {
        if let Err(e) = cache.delete(&org_rate_limits_key(org_id)).await {
            debug!(error = %e, org_id = %org_id, "Failed to invalidate cached org rate limits");
        }
    }
}

async fn get_org_rate_limits(state: &AppState, org_id: &str) -> Result<OrgRateLimits, ()> {
    // Cache in Redis for 60s to avoid DB hit per request.
    let cache_key = org_rate_limits_key(org_id);

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

        // What the X-RateLimit-* headers report: the limit that runs out
        // first. Nothing is sent once a check was skipped on a Redis error
        // (fail-open): the numbers would be wrong.
        let mut reported: Option<RateLimitHeaders> = None;
        let mut bypassed = false;

        // 1) Org-level token bucket (plan rps/burst)
        let org_key = format!("rate_limit:org:{}", ctx.organization_id);
        match cache
            .check_token_bucket(&org_key, org_limits.rps, org_limits.burst)
            .await
        {
            Ok(result) => {
                let headers = RateLimitHeaders::from_token_bucket(org_limits.burst, &result);
                if !result.allowed {
                    let retry_after_secs = result.retry_after_ms.div_ceil(1000);
                    return rate_limit_exceeded_response(
                        retry_after_secs,
                        org_limits.burst,
                        Some(headers.refused(retry_after_secs)),
                    );
                }
                reported = Some(headers);
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
                bypassed = true;
            }
        }

        // 2) Optional per-API-key override (requests per minute)
        if let Some(per_minute) = ctx.rate_limit {
            let key_limit = per_minute.max(1) as u32;
            let key = format!("rate_limit:api_key:{}", ctx.api_key_id);
            match cache.check_rate_limit(&key, key_limit, 60).await {
                Ok(result) => {
                    let headers =
                        RateLimitHeaders::from_window(key_limit, &result, chrono::Utc::now());
                    if !result.allowed {
                        let retry_after_secs =
                            (result.reset_at - chrono::Utc::now()).num_seconds().max(1) as u64;
                        return rate_limit_exceeded_response(
                            retry_after_secs,
                            key_limit,
                            Some(headers.refused(retry_after_secs)),
                        );
                    }
                    reported = Some(match reported {
                        Some(org) => org.tighter(headers),
                        None => headers,
                    });
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
                    bypassed = true;
                }
            }
        }

        // Allowed -> proceed
        let mut response = next.run(request).await;
        if let Some(headers) = reported.filter(|_| !bypassed) {
            headers.apply(response.headers_mut());
        }
        return response;
    }

    // Public routes: apply a simple global rate limit (settings-based) keyed by IP/UA.
    let client_id = extract_client_id(&request, &state.settings.trusted_proxy);
    let rps = state.settings.rate_limit.requests_per_second.max(1);
    let burst = state.settings.rate_limit.burst_size.max(1);
    let key = format!("rate_limit:public:{}", client_id);
    match cache.check_token_bucket(&key, rps, burst).await {
        Ok(result) => {
            if !result.allowed {
                let retry_after_secs = result.retry_after_ms.div_ceil(1000);
                return rate_limit_exceeded_response(retry_after_secs, burst, None);
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

    let client_id = extract_client_id(&request, &state.settings.trusted_proxy);
    let key = format!("rate_limit:admin:{}", client_id);
    match cache
        .check_rate_limit(&key, MAX_REQUESTS, WINDOW_SECS)
        .await
    {
        Ok(result) => {
            if !result.allowed {
                let retry_after_secs =
                    (result.reset_at - chrono::Utc::now()).num_seconds().max(1) as u64;
                return rate_limit_exceeded_response(retry_after_secs, MAX_REQUESTS, None);
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cache::{RateLimitResult, TokenBucketResult};

    fn bucket(allowed: bool, remaining: u32, retry_ms: u64, next_ms: u64) -> TokenBucketResult {
        TokenBucketResult {
            allowed,
            remaining,
            retry_after_ms: retry_ms,
            next_token_ms: next_ms,
        }
    }

    fn header(headers: &HeaderMap, name: &str) -> Option<String> {
        headers
            .get(name)
            .map(|v| v.to_str().expect("ascii header").to_string())
    }

    #[test]
    fn token_bucket_headers_report_burst_whole_tokens_and_seconds_to_the_next_token() {
        // Free plan: 5 rps, burst 10; 9.4 tokens left -> 9, next token in 120 ms.
        let h = RateLimitHeaders::from_token_bucket(10, &bucket(true, 9, 0, 120));
        assert_eq!(h.scope, RateLimitScope::Organization);
        assert_eq!(h.limit, 10);
        assert_eq!(h.remaining, 9);
        assert_eq!(h.reset_secs, 1, "rounded up, never 0 while refilling");

        // A full bucket has nothing to wait for.
        assert_eq!(
            RateLimitHeaders::from_token_bucket(10, &bucket(true, 10, 0, 0)).reset_secs,
            0
        );
        // 1.5 s to the next token at a slow refill rounds up to 2.
        assert_eq!(
            RateLimitHeaders::from_token_bucket(10, &bucket(true, 3, 0, 1_500)).reset_secs,
            2
        );
        // Remaining never exceeds the advertised limit.
        assert_eq!(
            RateLimitHeaders::from_token_bucket(10, &bucket(true, 50, 0, 0)).remaining,
            10
        );
    }

    #[test]
    fn window_headers_report_seconds_until_the_window_starts_again() {
        let now = chrono::Utc::now();
        let result = RateLimitResult {
            allowed: true,
            remaining: 41,
            reset_at: now + chrono::Duration::milliseconds(59_500),
        };
        let h = RateLimitHeaders::from_window(60, &result, now);
        assert_eq!(h.scope, RateLimitScope::ApiKey);
        assert_eq!((h.limit, h.remaining, h.reset_secs), (60, 41, 60));

        // A window that already ended (clock skew) reports 0, not a wrap-around.
        let past = RateLimitResult {
            allowed: true,
            remaining: 0,
            reset_at: now - chrono::Duration::seconds(3),
        };
        assert_eq!(RateLimitHeaders::from_window(60, &past, now).reset_secs, 0);
    }

    #[tokio::test]
    async fn refused_headers_match_retry_after() {
        let h = RateLimitHeaders::from_token_bucket(10, &bucket(false, 0, 1_200, 1_200));
        let refused = h.refused(2);
        assert_eq!(
            (refused.limit, refused.remaining, refused.reset_secs),
            (10, 0, 2)
        );

        let before = current_timestamp();
        let res = rate_limit_exceeded_response(2, 10, Some(refused));
        let after = current_timestamp();
        assert_eq!(res.status(), StatusCode::TOO_MANY_REQUESTS);
        let headers = res.headers().clone();
        assert_eq!(header(&headers, "retry-after").as_deref(), Some("2"));
        assert_eq!(header(&headers, HEADER_LIMIT).as_deref(), Some("10"));
        assert_eq!(header(&headers, HEADER_REMAINING).as_deref(), Some("0"));
        assert_eq!(
            header(&headers, HEADER_SCOPE).as_deref(),
            Some("organization")
        );
        // X-RateLimit-Reset is the Unix time of the reset (what the SDKs
        // parse), the very value of the body's reset_at; Retry-After is the wait.
        let reset: u64 = header(&headers, HEADER_RESET)
            .expect("reset header")
            .parse()
            .expect("numeric reset");
        assert!(
            (before + 2..=after + 2).contains(&reset),
            "{reset} is not now + Retry-After ({before}..={after} + 2)"
        );
        let body = axum::body::to_bytes(res.into_body(), usize::MAX)
            .await
            .expect("body");
        let body: serde_json::Value = serde_json::from_slice(&body).expect("json body");
        assert_eq!(body["reset_at"].as_u64(), Some(reset), "{body}");
        assert_eq!(body["retry_after_secs"], 2);

        // Public and admin refusals keep Retry-After only.
        let res = rate_limit_exceeded_response(3, 100, None);
        assert_eq!(header(res.headers(), "retry-after").as_deref(), Some("3"));
        assert!(res.headers().get(HEADER_LIMIT).is_none());
        assert!(res.headers().get(HEADER_REMAINING).is_none());
        assert!(res.headers().get(HEADER_RESET).is_none());
    }

    #[test]
    fn the_limit_that_runs_out_first_is_reported() {
        let org = RateLimitHeaders {
            scope: RateLimitScope::Organization,
            limit: 200,
            remaining: 150,
            reset_secs: 1,
        };
        let key = RateLimitHeaders {
            scope: RateLimitScope::ApiKey,
            limit: 30,
            remaining: 4,
            reset_secs: 40,
        };
        assert_eq!(org.tighter(key), key);
        assert_eq!(key.tighter(org), key);

        // A per-key limit with more room than the org bucket: the org bucket.
        let roomy = RateLimitHeaders {
            remaining: 170,
            ..key
        };
        assert_eq!(org.tighter(roomy), org);

        // Tie on remaining: the one that takes longer to come back.
        let tie = RateLimitHeaders {
            remaining: 150,
            ..key
        };
        assert_eq!(org.tighter(tie), tie);
        assert_eq!(tie.tighter(org), tie);
    }

    #[test]
    fn apply_sets_the_four_headers() {
        let mut headers = HeaderMap::new();
        RateLimitHeaders {
            scope: RateLimitScope::ApiKey,
            limit: 60,
            remaining: 12,
            reset_secs: 33,
        }
        .apply_at(&mut headers, 1_700_000_000);
        assert_eq!(header(&headers, "X-RateLimit-Limit").as_deref(), Some("60"));
        assert_eq!(
            header(&headers, "X-RateLimit-Remaining").as_deref(),
            Some("12")
        );
        assert_eq!(
            header(&headers, "X-RateLimit-Reset").as_deref(),
            Some("1700000033"),
            "a Unix time, not the 33 s wait"
        );
        assert_eq!(
            header(&headers, "X-RateLimit-Scope").as_deref(),
            Some("api_key")
        );
    }

    #[test]
    fn reset_header_is_a_unix_time_the_sdks_can_parse() {
        // Every SDK turns X-RateLimit-Reset into a date with the value as Unix
        // seconds (Go time.Unix, Node new Date(v * 1000), Python fromtimestamp,
        // PHP $reset - time()). A wait of a few seconds there reads as 1970.
        let now = 1_760_000_000;
        let spent = RateLimitHeaders::from_token_bucket(10, &bucket(true, 9, 0, 120));
        assert_eq!(spent.reset_at(now), now + 1);

        // A full bucket has nothing to wait for: the reset is now, not 0.
        let full = RateLimitHeaders::from_token_bucket(10, &bucket(true, 10, 0, 0));
        assert_eq!(full.reset_at(now), now);

        // An API key window ending in 59.5 s.
        let wall = chrono::Utc::now();
        let window = RateLimitHeaders::from_window(
            60,
            &RateLimitResult {
                allowed: true,
                remaining: 41,
                reset_at: wall + chrono::Duration::milliseconds(59_500),
            },
            wall,
        );
        assert_eq!(window.reset_at(now), now + 60);

        // apply() stamps the current time.
        let before = current_timestamp();
        let mut headers = HeaderMap::new();
        spent.apply(&mut headers);
        let after = current_timestamp();
        let reset: u64 = header(&headers, HEADER_RESET).unwrap().parse().unwrap();
        assert!(
            (before + 1..=after + 1).contains(&reset),
            "{reset} not in {before}..={after} + 1"
        );
    }
}
