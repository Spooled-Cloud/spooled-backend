//! Authentication middleware
//!
//! This module implements dual authentication:
//! - API key authentication with bcrypt verification (for programmatic access)
//! - JWT token authentication (for dashboard/browser access)
//!
//! JWT tokens start with "eyJ" (base64 encoded header).
//! API keys use "sp_" prefix (new standard) or "sk_" prefix (legacy, for backward compatibility).

use axum::{
    extract::{Extension, Request, State},
    http::StatusCode,
    middleware::Next,
    response::{IntoResponse, Response},
    Json,
};
use chrono::Utc;
use jsonwebtoken::{decode, DecodingKey, Validation};
use sqlx::{FromRow, PgPool};

use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex, PoisonError, Weak};
use std::time::Duration;

use crate::api::handlers::auth::Claims;
use crate::api::middleware::bcrypt_gate::{self, ScanLane};
use crate::api::middleware::client_ip::client_ip;
use crate::api::AppState;
use crate::cache::RedisCache;
use crate::error::ErrorResponse;
use crate::models::ApiKeyContext;

type ApiKeyAuthorizationRow = (
    bool,
    Option<i32>,
    Option<chrono::DateTime<Utc>>,
    Vec<String>,
);

/// Map an `(StatusCode, String)` auth failure into a JSON error response so
/// SDKs and the dashboard get the same `{code, message}` shape they get for
/// every other API error. Without this, 401s leaked back as `text/plain`,
/// which forced every client to special-case auth failures.
fn auth_error_response(status: StatusCode, message: String) -> Response {
    let code = match status {
        StatusCode::UNAUTHORIZED => "UNAUTHORIZED",
        StatusCode::FORBIDDEN => "ACCESS_DENIED",
        StatusCode::INTERNAL_SERVER_ERROR => "INTERNAL_ERROR",
        StatusCode::SERVICE_UNAVAILABLE => "SERVICE_UNAVAILABLE",
        _ => "ERROR",
    };
    let body = Json(ErrorResponse {
        code: code.to_string(),
        message,
        details: None,
    });
    let mut response = (status, body).into_response();
    if status == StatusCode::SERVICE_UNAVAILABLE {
        response.headers_mut().insert(
            axum::http::header::RETRY_AFTER,
            axum::http::HeaderValue::from(AUTH_RETRY_AFTER_SECS),
        );
    }
    response
}

/// `Retry-After` of a 503 from authentication: the legacy scan turn stayed
/// busy for its bounded wait, or the database could not be reached. Either
/// way the key was not checked, so the client should retry, not give up.
pub const AUTH_RETRY_AFTER_SECS: u64 = 5;

/// Type alias for organization context extractor
/// Use `Option<Extension<ApiKeyContext>>` in handlers to get the org context
/// from authenticated requests. Returns None if not authenticated.
pub type OrgContext = Extension<ApiKeyContext>;

/// Failed API-key authentications one client address may cause per minute
/// before the legacy bcrypt scan is skipped for it.
///
/// Over the budget only the scan is skipped (401): a key found by
/// `lookup_hash` still authenticates. Without a trusted client-address header
/// every caller lands in the same bucket (the proxy's, or `unknown`), so the
/// budget must never gate the path legitimate modern keys take.
///
/// A request is charged BEFORE it scans and refunded when the scan finds its
/// key or never runs (see [`charge_failed_auth`]).
pub const FAILED_AUTH_PER_IP_LIMIT: u32 = 20;

/// Failed API-key authentications all gRPC callers together may cause per
/// minute before the legacy bcrypt scan is skipped for gRPC.
///
/// gRPC sees the ingress as its peer and has no trusted client-address
/// metadata, so it cannot be budgeted per address. Like the per-IP budget it
/// only skips the scan: keys found by `lookup_hash` always work. gRPC scans
/// also take their own turn ([`bcrypt_gate::ScanLane::Grpc`]), so unknown gRPC
/// tokens under this budget can delay only gRPC legacy keys, never REST ones
/// (W3R-06, W3R-08).
pub const FAILED_AUTH_GRPC_LIMIT: u32 = 60;

/// Redis counter of [`FAILED_AUTH_GRPC_LIMIT`]. Its own prefix, so no client
/// address (`auth_fail:{ip}`) can land in it.
pub const FAILED_AUTH_GRPC_KEY: &str = "auth_fail_grpc";

/// Window of [`FAILED_AUTH_PER_IP_LIMIT`] and [`FAILED_AUTH_GRPC_LIMIT`].
const FAILED_AUTH_WINDOW_SECS: u64 = 60;

/// The failed-auth budget a token that misses `lookup_hash` is charged to.
#[derive(Debug, Clone, Copy)]
pub(crate) enum FailedAuthBudget<'a> {
    /// REST and `/auth/login`: the client address
    /// ([`FAILED_AUTH_PER_IP_LIMIT`]).
    ClientIp(&'a str),
    /// gRPC: one bucket for every caller ([`FAILED_AUTH_GRPC_LIMIT`]).
    Grpc,
}

impl FailedAuthBudget<'_> {
    fn key(&self) -> String {
        match self {
            Self::ClientIp(ip) => format!("auth_fail:{}", ip),
            Self::Grpc => FAILED_AUTH_GRPC_KEY.to_string(),
        }
    }

    fn limit(&self) -> u32 {
        match self {
            Self::ClientIp(_) => FAILED_AUTH_PER_IP_LIMIT,
            Self::Grpc => FAILED_AUTH_GRPC_LIMIT,
        }
    }

    /// The scan turn requests under this budget take.
    fn lane(&self) -> ScanLane {
        match self {
            Self::ClientIp(_) => ScanLane::Rest,
            Self::Grpc => ScanLane::Grpc,
        }
    }
}

/// How long a token that matched no key is answered from Redis without
/// touching the database.
const AUTH_MISS_TTL_SECS: u64 = 60;

/// API key record from database, and the value cached under
/// `api_key:{sha256(token)}`. Shared by the REST middleware, `/auth/login` and
/// gRPC, so a revoke that clears the cache entry ends all three.
#[derive(Debug, Clone, FromRow)]
pub(crate) struct ApiKeyRecord {
    pub(crate) id: String,
    pub(crate) organization_id: String,
    key_hash: String,
    pub(crate) queues: Vec<String>,
    pub(crate) rate_limit: Option<i32>,
    pub(crate) is_active: bool,
    pub(crate) expires_at: Option<chrono::DateTime<Utc>>,
}

/// Authenticate API key or JWT token middleware
///
/// This middleware accepts both:
/// - API keys (sp_ or legacy sk_ prefix) - for programmatic access
/// - JWT tokens (start with "eyJ") - for dashboard/browser access
///
/// For API keys:
/// 1. Looks up the API key record by a fast hash lookup
/// 2. Verifies the bcrypt hash in Rust (NOT database) for horizontal scaling
/// 3. Checks expiration and active status
/// 4. Updates last_used timestamp asynchronously
///
/// For JWT tokens:
/// 1. Validates the JWT signature and expiration
/// 2. Extracts claims to build the API key context
/// 3. Checks token blacklist (for logged out tokens)
pub async fn authenticate_api_key(
    State(state): State<AppState>,
    request: Request,
    next: Next,
) -> Result<Response, Response> {
    authenticate_inner(state, request, next, false).await
}

/// Same as [`authenticate_api_key`], but ALSO accepts the credential in the
/// query string.
///
/// Mount this **only** on the realtime routes (`/ws`, `/events*`). Browser
/// `EventSource` and `WebSocket` cannot set an `Authorization` header, so those
/// endpoints have no alternative; every other endpoint can and must use the
/// header.
///
/// The distinction matters because query strings are retained by things headers
/// are not: reverse-proxy and CDN access logs, APM spans that record
/// `http.url`, browser history, and `Referer`. A long-lived API key (they
/// default to no expiry) must not travel there. CWE-598.
pub async fn authenticate_api_key_allow_query(
    State(state): State<AppState>,
    request: Request,
    next: Next,
) -> Result<Response, Response> {
    authenticate_inner(state, request, next, true).await
}

async fn authenticate_inner(
    state: AppState,
    mut request: Request,
    next: Next,
    allow_query_credential: bool,
) -> Result<Response, Response> {
    // Check Authorization header first
    let token_from_header = request
        .headers()
        .get("Authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|header| header.strip_prefix("Bearer ").map(|s| s.to_string()));

    let token = match token_from_header {
        Some(t) => t,
        None if allow_query_credential => {
            match extract_query_credential(request.uri().query().unwrap_or("")) {
                Some(t) => t,
                None => {
                    return Err(auth_error_response(
                        StatusCode::UNAUTHORIZED,
                        "Missing or invalid Authorization header or token/api_key query parameter"
                            .to_string(),
                    ));
                }
            }
        }
        None => {
            return Err(auth_error_response(
                StatusCode::UNAUTHORIZED,
                "Missing or invalid Authorization header".to_string(),
            ));
        }
    };

    // Detect token type: JWT tokens start with "eyJ" (base64 encoded JSON header)
    let context = if token.starts_with("eyJ") {
        authenticate_jwt_token(&state, &token)
            .await
            .map_err(|(s, m)| auth_error_response(s, m))?
    } else {
        let client_ip = client_ip(request.headers(), &state.settings.trusted_proxy);
        authenticate_api_key_token(
            state.db.pool(),
            state.cache.as_ref(),
            &token,
            FailedAuthBudget::ClientIp(&client_ip),
        )
        .await
        .map_err(|(s, m)| auth_error_response(s, m))?
    };

    // Store context for downstream handlers
    request.extensions_mut().insert(context);

    Ok(next.run(request).await)
}

/// Pull a `token=` / `api_key=` value out of a raw query string.
///
/// Percent-decodes the value: credentials are base64url/alphanumeric today, but
/// a raw pass-through would silently break the moment one is not.
fn extract_query_credential(query: &str) -> Option<String> {
    for pair in query.split('&') {
        let mut parts = pair.splitn(2, '=');
        if let (Some(key), Some(value)) = (parts.next(), parts.next()) {
            if key == "token" || key == "api_key" {
                let decoded = percent_decode(value);
                if !decoded.is_empty() {
                    return Some(decoded);
                }
            }
        }
    }
    None
}

/// Minimal percent-decoder for query values (`%XX` and `+` for space).
fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).ok();
                match hex.and_then(|h| u8::from_str_radix(h, 16).ok()) {
                    Some(byte) => {
                        out.push(byte);
                        i += 3;
                    }
                    None => {
                        out.push(bytes[i]);
                        i += 1;
                    }
                }
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Authenticate JWT token and return API key context
async fn authenticate_jwt_token(
    state: &AppState,
    token: &str,
) -> Result<ApiKeyContext, (StatusCode, String)> {
    // Decode and validate JWT
    let token_data = decode::<Claims>(
        token,
        &DecodingKey::from_secret(state.settings.jwt.secret.as_bytes()),
        &Validation::default(),
    )
    .map_err(|e| {
        tracing::debug!(error = %e, "JWT validation failed");
        (StatusCode::UNAUTHORIZED, "Invalid token".to_string())
    })?;

    // Verify it's an access token (not a refresh token)
    if token_data.claims.token_type != "access" {
        return Err((StatusCode::UNAUTHORIZED, "Invalid token type".to_string()));
    }

    // Check if token is blacklisted (logged out)
    if let Some(ref cache) = state.cache {
        if crate::api::handlers::auth::is_token_revoked(
            cache,
            &token_data.claims.jti,
            token_data.claims.sid.as_deref(),
        )
        .await
        {
            tracing::warn!(jti = %token_data.claims.jti, "Attempted to use revoked token");
            return Err((
                StatusCode::UNAUTHORIZED,
                "Token has been revoked".to_string(),
            ));
        }
    }

    // Verify the API key is still active AND not expired, and read its CURRENT queue
    // scope from the DB. Trusting the token's `claims.queues` let a key that was later
    // narrowed (e.g. `["*"]` -> `["emails"]`) keep its old wildcard scope for the JWT's
    // lifetime; and omitting `expires_at` let a JWT outlive the key's expiry (the direct
    // API-key path already rejects expired keys).
    let api_key_row: Option<ApiKeyAuthorizationRow> = sqlx::query_as(
        "SELECT is_active, rate_limit, expires_at, queues FROM api_keys WHERE id = $1",
    )
    .bind(&token_data.claims.api_key_id)
    .fetch_optional(state.db.pool())
    .await
    .map_err(auth_db_error)?;

    match api_key_row {
        Some((true, rate_limit, expires_at, queues)) => {
            if let Some(expires) = expires_at {
                if expires < Utc::now() {
                    return Err((StatusCode::UNAUTHORIZED, "API key has expired".to_string()));
                }
            }
            // Soft-deleted orgs must not authenticate via a leftover JWT, even if the
            // key row was somehow reactivated (email-verify / admin mint revival paths).
            let org_tier: Option<(String,)> =
                sqlx::query_as("SELECT plan_tier FROM organizations WHERE id = $1")
                    .bind(&token_data.claims.org_id)
                    .fetch_optional(state.db.pool())
                    .await
                    .map_err(auth_db_error)?;
            if !matches!(org_tier.as_ref().map(|(t,)| t.as_str()), Some(t) if t != "deleted") {
                return Err((
                    StatusCode::UNAUTHORIZED,
                    "Organization is not available".to_string(),
                ));
            }
            // Build context from the CURRENT DB scope, not the (possibly stale) token.
            Ok(ApiKeyContext {
                api_key_id: token_data.claims.api_key_id,
                organization_id: token_data.claims.org_id,
                queues,
                rate_limit,
            })
        }
        Some((false, _, _, _)) => {
            tracing::warn!(
                api_key_id = %token_data.claims.api_key_id,
                "JWT token references revoked API key"
            );
            Err((
                StatusCode::UNAUTHORIZED,
                "API key has been revoked".to_string(),
            ))
        }
        None => {
            tracing::warn!(
                api_key_id = %token_data.claims.api_key_id,
                "JWT token references deleted API key"
            );
            Err((StatusCode::UNAUTHORIZED, "API key not found".to_string()))
        }
    }
}

/// Authenticate a raw API key and return its context.
///
/// Shared by the REST middleware and gRPC. `budget` is the failed-auth budget
/// the legacy scan is charged to (see [`resolve_api_key`]).
pub(crate) async fn authenticate_api_key_token(
    pool: &PgPool,
    cache: Option<&RedisCache>,
    token: &str,
    budget: FailedAuthBudget<'_>,
) -> Result<ApiKeyContext, (StatusCode, String)> {
    let api_key = resolve_api_key(pool, cache, token, budget).await?;

    // Verify active status
    if !api_key.is_active {
        return Err((StatusCode::UNAUTHORIZED, "API key is inactive".to_string()));
    }

    // Verify expiration
    if let Some(expires) = api_key.expires_at {
        if expires < Utc::now() {
            return Err((StatusCode::UNAUTHORIZED, "API key has expired".to_string()));
        }
    }

    touch_last_used(pool, cache, &api_key.id).await;

    Ok(ApiKeyContext {
        api_key_id: api_key.id,
        organization_id: api_key.organization_id,
        queues: api_key.queues,
        rate_limit: api_key.rate_limit,
    })
}

/// Find the key a raw token belongs to.
///
/// In order:
/// 1. The Redis record cache (`api_key:{sha256}`): no database, no bcrypt.
///    Bcrypt was verified when the record was cached.
/// 2. The negative cache (`auth_miss:{sha256}`): a token that matched nothing
///    in the last [`AUTH_MISS_TTL_SECS`] is refused without the database.
/// 3. Fast path: `lookup_hash = sha256(token)` is unique, so a single indexed row
///    read yields the one candidate, which is then bcrypt-verified. This replaced
///    the old prefix scan, where every `sp_live_` key shared the same
///    `key_prefix` so a bogus key cost up to 100 bcrypt verifications.
/// 4. Legacy fallback ([`resolve_legacy`]): rows created before `lookup_hash`
///    existed have it NULL and can't be backfilled offline (the plaintext isn't
///    recoverable from bcrypt), so they get the bounded prefix scan and are
///    backfilled on a match. Any unknown token can trigger it, so it is charged
///    to `budget` first and skipped once the budget is spent
///    ([`FAILED_AUTH_PER_IP_LIMIT`] per client address on REST,
///    [`FAILED_AUTH_GRPC_LIMIT`] for all of gRPC); REST and gRPC scans take
///    separate turns, a request waits for its turn about one scan at most, and
///    concurrent requests for one token share one scan. A miss after a scan is
///    negatively cached. A spent budget is a 401 and a turn still busy after
///    the wait a 503; neither is negatively cached: they proved nothing about
///    the token.
///
/// Steps 1-3 never touch the budget: a key found there is never gated.
///
/// Every bcrypt verify runs through [`bcrypt_gate`], off the async workers.
/// Redis errors fail open for the caches and the budget: the request takes the
/// database path as if Redis were not configured.
pub(crate) async fn resolve_api_key(
    pool: &PgPool,
    cache: Option<&RedisCache>,
    token: &str,
    budget: FailedAuthBudget<'_>,
) -> Result<ApiKeyRecord, (StatusCode, String)> {
    let lookup_hash = hash_for_lookup(token);

    if let Some(cache) = cache {
        // Cache hit - trust it! The cache key is the full SHA-256 of the token.
        if let Ok(Some(cached)) = cache
            .get_json::<ApiKeyRecord>(&api_key_cache_key(&lookup_hash))
            .await
        {
            return Ok(cached);
        }
        if let Ok(true) = cache.exists(&auth_miss_key(&lookup_hash)).await {
            return Err(invalid_api_key());
        }
    }

    if let Some(record) = find_by_lookup_hash(pool, token, &lookup_hash).await? {
        return cache_api_key(pool, cache, &lookup_hash, record).await;
    }

    match resolve_legacy(pool, cache, token, &lookup_hash, budget).await? {
        Legacy::Found(record) => cache_api_key(pool, cache, &lookup_hash, record).await,
        Legacy::OverBudget => {
            // The scan never looked at this token, so the miss proves nothing
            // and is not negatively cached.
            tracing::debug!(
                ?budget,
                "Failed-auth budget spent; skipping the legacy key scan"
            );
            Err(invalid_api_key())
        }
        Legacy::Busy => {
            // Not checked either: no negative cache, or a legacy key refused
            // during a flood would stay refused for the cache TTL; a 503 so the
            // client retries instead of treating its key as invalid.
            tracing::debug!(?budget, "Legacy API key scan busy; token not checked");
            Err(auth_unavailable())
        }
        Legacy::Missed { charge } => {
            // A concurrent legacy-key backfill can commit after the fast lookup
            // but before the fallback query, moving the row between both
            // snapshots. Retry the indexed lookup once before rejecting the key.
            let retried = find_by_lookup_hash(pool, token, &lookup_hash).await;
            if !matches!(retried, Ok(None)) {
                refund_failed_auth(cache, charge).await;
            }
            if let Some(record) = retried? {
                return cache_api_key(pool, cache, &lookup_hash, record).await;
            }
            if let Some(cache) = cache {
                let _ = cache
                    .set(&auth_miss_key(&lookup_hash), "1", AUTH_MISS_TTL_SECS)
                    .await;
            }
            Err(invalid_api_key())
        }
    }
}

/// Outcome of [`scan_legacy_keys`], shared by the requests of one flight
/// ([`resolve_legacy`]).
#[derive(Clone)]
enum LegacyScan {
    Found(ApiKeyRecord),
    /// Every candidate was checked (or there were none): the token is unknown.
    Missed,
    /// The lane's scan turn stayed busy for the bounded wait; the token was not
    /// checked.
    Busy,
}

/// Outcome of [`resolve_legacy`] for one request.
enum Legacy {
    /// The scan, or the re-check after waiting for it, found the key. Any charge
    /// was refunded.
    Found(ApiKeyRecord),
    /// No legacy key matched. `charge` is this request's failed-auth charge
    /// (only the request that ran the scan has one), refunded if the caller's
    /// final `lookup_hash` retry still finds the key.
    Missed { charge: Option<BudgetCharge> },
    /// The scan turn, or the request already scanning this token, stayed busy
    /// past the bounded wait. Not charged.
    Busy,
    /// The budget was spent: no scan. Charged.
    OverBudget,
}

/// Longest a request waits for a concurrent request already resolving the same
/// token on the same lane: that request's wait for the turn (at most
/// [`bcrypt_gate::LEGACY_SCAN_MAX_WAIT`]) plus its scan. Past it the request
/// is a 503 like a busy turn; under the 30 s request timeout.
const LEGACY_FLIGHT_MAX_WAIT: Duration = Duration::from_secs(20);

/// Requests resolving one token on one lane at the same time: the first runs
/// the scan, the rest wait for it and take its outcome (W3R-07). Before, each
/// queued for the scan turn on its own, and those that got it re-scanned the
/// candidate list the first had already settled.
struct LegacyFlight {
    /// Set by the request that scanned, for the others. Holding the lock is
    /// leading the flight; a leader that leaves without an outcome (cancelled,
    /// over its budget, a database error) hands the lead to the next waiter.
    outcome: tokio::sync::Mutex<Option<LegacyScan>>,
}

type FlightKey = (ScanLane, String);

/// Open flights by lane and `lookup_hash`. An entry lives while a request is
/// in its flight: every [`FlightTicket`] is created and dropped under this lock.
static FLIGHTS: LazyLock<Mutex<HashMap<FlightKey, Weak<LegacyFlight>>>> =
    LazyLock::new(Default::default);

/// One request's place in a [`LegacyFlight`]; the last one out removes the
/// flight.
struct FlightTicket {
    key: FlightKey,
    flight: Option<Arc<LegacyFlight>>,
}

impl FlightTicket {
    fn join(lane: ScanLane, lookup_hash: &str) -> Self {
        let key = (lane, lookup_hash.to_string());
        let mut flights = FLIGHTS.lock().unwrap_or_else(PoisonError::into_inner);
        let flight = match flights.get(&key).and_then(Weak::upgrade) {
            Some(flight) => flight,
            None => {
                let flight = Arc::new(LegacyFlight {
                    outcome: tokio::sync::Mutex::new(None),
                });
                flights.insert(key.clone(), Arc::downgrade(&flight));
                flight
            }
        };
        Self {
            key,
            flight: Some(flight),
        }
    }

    fn flight(&self) -> &LegacyFlight {
        self.flight
            .as_deref()
            .expect("a ticket holds its flight until dropped")
    }
}

impl Drop for FlightTicket {
    fn drop(&mut self) {
        let mut flights = FLIGHTS.lock().unwrap_or_else(PoisonError::into_inner);
        // Dropped under the lock, like every join, so a dead entry is exact.
        drop(self.flight.take());
        if flights
            .get(&self.key)
            .is_some_and(|flight| flight.strong_count() == 0)
        {
            flights.remove(&self.key);
        }
    }
}

/// Resolve a token that missed `lookup_hash` through the legacy scan of its
/// lane ([`FailedAuthBudget::lane`]).
///
/// Concurrent requests for the token join one [`LegacyFlight`]: the leader is
/// charged to its budget and scans, the others wait at most
/// [`LEGACY_FLIGHT_MAX_WAIT`] and take its outcome without a charge or a scan
/// of their own. A shared outcome is the leader's scan of this exact token
/// (the flight key is its full SHA-256, as for the record cache), and a found
/// record is still re-checked for `is_active` by [`cache_api_key`].
async fn resolve_legacy(
    pool: &PgPool,
    cache: Option<&RedisCache>,
    token: &str,
    lookup_hash: &str,
    budget: FailedAuthBudget<'_>,
) -> Result<Legacy, (StatusCode, String)> {
    let ticket = FlightTicket::join(budget.lane(), lookup_hash);
    let Ok(mut outcome) =
        tokio::time::timeout(LEGACY_FLIGHT_MAX_WAIT, ticket.flight().outcome.lock()).await
    else {
        tracing::debug!("Legacy API key scan of this token still running; not waiting longer");
        return Ok(Legacy::Busy);
    };
    match outcome.as_ref() {
        Some(LegacyScan::Found(record)) => return Ok(Legacy::Found(record.clone())),
        Some(LegacyScan::Missed) => return Ok(Legacy::Missed { charge: None }),
        Some(LegacyScan::Busy) => return Ok(Legacy::Busy),
        None => {}
    }

    // This request leads: charge first, so neither concurrency nor a cancelled
    // request gets a scan for free.
    let Ok(charge) = charge_failed_auth(cache, budget).await else {
        return Ok(Legacy::OverBudget);
    };
    let scanned = scan_legacy_keys(pool, cache, token, lookup_hash, budget.lane()).await;
    if let Ok(scan) = &scanned {
        *outcome = Some(scan.clone());
    }
    drop(outcome);

    match scanned {
        Ok(LegacyScan::Missed) => Ok(Legacy::Missed { charge }),
        Ok(LegacyScan::Found(record)) => {
            refund_failed_auth(cache, charge).await;
            Ok(Legacy::Found(record))
        }
        Ok(LegacyScan::Busy) => {
            refund_failed_auth(cache, charge).await;
            Ok(Legacy::Busy)
        }
        Err(e) => {
            refund_failed_auth(cache, charge).await;
            Err(e)
        }
    }
}

fn invalid_api_key() -> (StatusCode, String) {
    (StatusCode::UNAUTHORIZED, "Invalid API key".to_string())
}

fn auth_unavailable() -> (StatusCode, String) {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        "Authentication is temporarily unavailable".to_string(),
    )
}

/// Map a database failure during authentication to a response for a caller
/// who is not authenticated yet: never the sqlx text (pool state, server error
/// detail), and 503 when the database cannot be reached so clients retry
/// instead of treating it as a server bug.
fn auth_db_error(e: sqlx::Error) -> (StatusCode, String) {
    tracing::error!(error = %e, "Database error during authentication");
    match e {
        sqlx::Error::PoolTimedOut
        | sqlx::Error::PoolClosed
        | sqlx::Error::WorkerCrashed
        | sqlx::Error::Io(_)
        | sqlx::Error::Tls(_) => auth_unavailable(),
        _ => (
            StatusCode::INTERNAL_SERVER_ERROR,
            "Authentication failed".to_string(),
        ),
    }
}

/// [`bcrypt_gate::verify`], with a failure to run the check reported as an
/// error rather than read as "no match" (which could be negatively cached).
async fn verify_key(token: &str, key_hash: &str) -> Result<bool, (StatusCode, String)> {
    bcrypt_gate::verify(token, key_hash).await.map_err(|e| {
        tracing::error!(error = %e, "API key verification could not run");
        auth_unavailable()
    })
}

/// The active key whose `lookup_hash` matches, if bcrypt agrees.
///
/// Mirrors the old scan's `is_active = TRUE` filter; expiry is checked by the
/// caller from the returned `expires_at`. Soft-deleted orgs (`plan_tier =
/// 'deleted'`) are excluded so a leftover or revived key cannot authenticate
/// after soft-delete.
async fn find_by_lookup_hash(
    pool: &PgPool,
    token: &str,
    lookup_hash: &str,
) -> Result<Option<ApiKeyRecord>, (StatusCode, String)> {
    let record: Option<ApiKeyRecord> = sqlx::query_as(
        "SELECT k.id, k.organization_id, k.key_hash, k.queues, k.rate_limit, k.is_active, k.expires_at
         FROM api_keys k
         INNER JOIN organizations o ON o.id = k.organization_id
         WHERE k.lookup_hash = $1 AND k.is_active = TRUE AND o.plan_tier <> 'deleted'
         LIMIT 1",
    )
    .bind(lookup_hash)
    .fetch_optional(pool)
    .await
    .map_err(auth_db_error)?;

    match record {
        // Defense in depth: verify bcrypt even though lookup_hash matched, so a
        // (practically impossible) SHA-256 collision can't authenticate. A
        // collision without a bcrypt match falls through to the legacy scan.
        Some(record) if verify_key(token, &record.key_hash).await? => Ok(Some(record)),
        _ => Ok(None),
    }
}

/// Bounded scan of the keys that carry no `lookup_hash` yet, on `lane`'s turn;
/// backfills it on a match so the key uses the fast path from then on. Only
/// `lookup_hash IS NULL` rows are scanned, so the set shrinks as keys
/// re-authenticate.
///
/// A request that had to wait for the turn re-checks the token first (the
/// record cache, the negative cache, `lookup_hash`): the request on the other
/// lane or another replica it waited behind may have resolved it. It then reads
/// the candidates again, without the rows backfilled meanwhile.
async fn scan_legacy_keys(
    pool: &PgPool,
    cache: Option<&RedisCache>,
    token: &str,
    lookup_hash: &str,
    lane: ScanLane,
) -> Result<LegacyScan, (StatusCode, String)> {
    let candidates = legacy_candidates(pool, token).await?;
    if candidates.is_empty() {
        return Ok(LegacyScan::Missed);
    }
    let units = candidates
        .iter()
        .map(|record| bcrypt_gate::verify_units(&record.key_hash))
        .fold(0u64, u64::saturating_add);
    let turn = bcrypt_gate::legacy_scan_turn(lane, units)
        .await
        .map_err(|e| {
            tracing::error!(error = %e, "Legacy API key scan could not run");
            auth_unavailable()
        })?;
    let Some(turn) = turn else {
        return Ok(LegacyScan::Busy);
    };

    let candidates = if turn.waited() {
        if let Some(cache) = cache {
            if let Ok(Some(record)) = cache
                .get_json::<ApiKeyRecord>(&api_key_cache_key(lookup_hash))
                .await
            {
                return Ok(LegacyScan::Found(record));
            }
            if let Ok(true) = cache.exists(&auth_miss_key(lookup_hash)).await {
                return Ok(LegacyScan::Missed);
            }
        }
        if let Some(record) = find_by_lookup_hash(pool, token, lookup_hash).await? {
            return Ok(LegacyScan::Found(record));
        }
        legacy_candidates(pool, token).await?
    } else {
        candidates
    };

    for record in candidates {
        if verify_key(token, &record.key_hash).await? {
            backfill_lookup_hash(pool, &record.id, lookup_hash).await;
            return Ok(LegacyScan::Found(record));
        }
    }
    Ok(LegacyScan::Missed)
}

/// The active legacy keys a token may be: no `lookup_hash`, and its prefix or
/// none, at most 100.
async fn legacy_candidates(
    pool: &PgPool,
    token: &str,
) -> Result<Vec<ApiKeyRecord>, (StatusCode, String)> {
    let key_prefix: String = token.chars().take(8).collect();
    sqlx::query_as(
        "SELECT k.id, k.organization_id, k.key_hash, k.queues, k.rate_limit, k.is_active, k.expires_at
         FROM api_keys k
         INNER JOIN organizations o ON o.id = k.organization_id
         WHERE k.is_active = TRUE
           AND k.lookup_hash IS NULL
           AND (k.key_prefix = $1 OR k.key_prefix IS NULL)
           AND o.plan_tier <> 'deleted'
         LIMIT 100",
    )
    .bind(&key_prefix)
    .fetch_all(pool)
    .await
    .map_err(auth_db_error)
}

/// Store `lookup_hash` on a legacy key that just verified, so every lane and
/// replica finds it on the fast path from now on.
///
/// Awaited, so the key's next request (and one that waited for this scan)
/// finds it by `lookup_hash`; run as its own task, so a cancelled request still
/// completes it. Guarded by `lookup_hash IS NULL`, so concurrent backfills are
/// idempotent. A failure only means the next request scans again.
async fn backfill_lookup_hash(pool: &PgPool, key_id: &str, lookup_hash: &str) {
    let db = pool.clone();
    let key_id = key_id.to_string();
    let lookup_hash = lookup_hash.to_string();
    let backfill = tokio::spawn(async move {
        sqlx::query("UPDATE api_keys SET lookup_hash = $1 WHERE id = $2 AND lookup_hash IS NULL")
            .bind(&lookup_hash)
            .bind(&key_id)
            .execute(&db)
            .await
    });
    match backfill.await {
        Ok(Ok(_)) => {}
        Ok(Err(e)) => tracing::warn!(error = %e, "Legacy API key lookup_hash backfill failed"),
        Err(e) => tracing::warn!(error = %e, "Legacy API key lookup_hash backfill did not run"),
    }
}

/// One failed authentication charged to a budget before the legacy scan.
struct BudgetCharge {
    key: String,
}

/// The budget was spent before this request: it must not scan.
struct OverBudget;

/// Charge `budget` one failed authentication BEFORE the legacy scan: INCR, then
/// compare the new count to the limit.
///
/// Charging after the scan (and reading the counter before it) let concurrent
/// requests all read the counter before any was charged, and a request
/// cancelled mid-scan was never charged at all (W3R-08). The charge is taken
/// back ([`refund_failed_auth`]) when the scan finds the key or never runs
/// (turn busy, database error): those are not failed authentications, so a
/// burst of requests for a dormant legacy key does not use up the budget its
/// address shares. Over the limit the request is charged and skips the scan.
///
/// `Ok(None)`: nothing to refund (no Redis, or a Redis error: the budget fails
/// open).
async fn charge_failed_auth(
    cache: Option<&RedisCache>,
    budget: FailedAuthBudget<'_>,
) -> Result<Option<BudgetCharge>, OverBudget> {
    let Some(cache) = cache else {
        return Ok(None);
    };
    let key = budget.key();
    // Fixed window: the first failure starts it and later ones do not extend
    // it, so a slow trickle of failures behind one shared proxy address cannot
    // keep the scan switched off.
    match cache
        .check_rate_limit(&key, budget.limit(), FAILED_AUTH_WINDOW_SECS)
        .await
    {
        Ok(counted) if counted.allowed => Ok(Some(BudgetCharge { key })),
        Ok(_) => Err(OverBudget),
        Err(e) => {
            tracing::debug!(error = %e, "Failed-auth counter unavailable");
            Ok(None)
        }
    }
}

/// Take back a [`charge_failed_auth`] charge.
async fn refund_failed_auth(cache: Option<&RedisCache>, charge: Option<BudgetCharge>) {
    if let (Some(cache), Some(charge)) = (cache, charge) {
        if let Err(e) = cache.decr(&charge.key).await {
            tracing::debug!(error = %e, "Failed-auth refund unavailable");
        }
    }
}

fn api_key_cache_key(lookup_hash: &str) -> String {
    format!("api_key:{}", lookup_hash)
}

fn auth_miss_key(lookup_hash: &str) -> String {
    format!("auth_miss:{}", lookup_hash)
}

/// TTL for cached API-key records. Kept short so that if a key is revoked in the
/// narrow window that races the cache write (revoke invalidates the entry before
/// this write lands), the stale-but-active record self-expires quickly instead of
/// staying valid for an hour.
const API_KEY_CACHE_TTL_SECS: u64 = 60;

/// Cache a freshly verified key record (a no-op without Redis).
async fn cache_api_key(
    pool: &PgPool,
    cache: Option<&RedisCache>,
    lookup_hash: &str,
    record: ApiKeyRecord,
) -> Result<ApiKeyRecord, (StatusCode, String)> {
    let Some(cache) = cache else {
        return Ok(record);
    };

    // Re-check is_active immediately before caching. The SELECT that found the
    // record ran BEFORE the (slow) bcrypt verify, so a revoke that commits during
    // bcrypt would otherwise be papered over by caching a stale active record for
    // the full TTL. If the key was revoked in that window, refuse it and do not cache.
    let still_active: Option<(bool,)> =
        sqlx::query_as("SELECT is_active FROM api_keys WHERE id = $1")
            .bind(&record.id)
            .fetch_optional(pool)
            .await
            .map_err(auth_db_error)?;
    if !matches!(still_active, Some((true,))) {
        return Err((StatusCode::UNAUTHORIZED, "API key is inactive".to_string()));
    }

    let _ = cache
        .set_json(
            &api_key_cache_key(lookup_hash),
            &record,
            API_KEY_CACHE_TTL_SECS,
        )
        .await;

    // CRITICAL: Also set reverse mapping (bcrypt hash prefix -> lookup hash)
    // This allows cache invalidation when the API key is revoked.
    let hash_prefix = if record.key_hash.len() >= 16 {
        &record.key_hash[..16]
    } else {
        &record.key_hash
    };
    let reverse_key = format!("api_key_reverse:{}", hash_prefix);
    let _ = cache
        .set(&reverse_key, lookup_hash, API_KEY_CACHE_TTL_SECS)
        .await;

    Ok(record)
}

/// How coarse `api_keys.last_used` is allowed to be.
///
/// The field answers "has this key been used recently", and nothing needs it to
/// be exact — so it is written at most once per key per interval instead of once
/// per request.
const LAST_USED_WRITE_INTERVAL_SECS: u64 = 300;

/// Record that a key was used, at most once per [`LAST_USED_WRITE_INTERVAL_SECS`].
///
/// Previously this fired an `UPDATE api_keys SET last_used = NOW()` on EVERY
/// authenticated request, including cache hits. In Postgres an UPDATE writes a
/// new row version, so a key doing 500 req/s produced 500 versions/s of one row:
/// every request serialised on that row's lock (adding latency to the very first
/// thing each request does), the table bloated, and the unbounded spawned tasks
/// competed for pool connections with the handlers.
///
/// The Redis `SET NX EX` acts as the once-per-interval gate. With no Redis
/// configured we fall back to writing every time — self-hosted deployments do
/// not have the request volume this protects against, and silently dropping
/// `last_used` entirely would be worse.
async fn touch_last_used(pool: &PgPool, cache: Option<&RedisCache>, key_id: &str) {
    if let Some(cache) = cache {
        let guard_key = format!("api_key_last_used:{}", key_id);
        match cache
            .set_if_absent(&guard_key, "1", LAST_USED_WRITE_INTERVAL_SECS)
            .await
        {
            // Guard already held: another request wrote within the interval.
            Ok(false) => return,
            Ok(true) => {}
            Err(e) => {
                // Redis unavailable: fall through and write, matching the
                // no-cache path rather than losing the field entirely.
                tracing::debug!(error = %e, "last_used write guard unavailable; writing through");
            }
        }
    }

    let db = pool.clone();
    let key_id = key_id.to_string();
    tokio::spawn(async move {
        let _ = sqlx::query("UPDATE api_keys SET last_used = NOW() WHERE id = $1")
            .bind(&key_id)
            .execute(&db)
            .await;
    });
}

/// Cache-key digest for an API key.
///
/// Uses the FULL SHA-256, same as `api_keys.lookup_hash`. It used to be
/// truncated to 16 hex chars (64 bits) — and because a cache hit is trusted
/// without re-running bcrypt, that truncated digest was the only thing between a
/// presented token and an authenticated context. The DB path deliberately keeps
/// bcrypt as defence-in-depth behind its full-length hash; there is no reason
/// for the cache path to hold a weaker margin, and the extra characters cost
/// nothing.
fn hash_for_lookup(token: &str) -> String {
    crate::models::api_key_lookup_hash(token)
}

// Implement Serialize/Deserialize for ApiKeyRecord to cache it
impl serde::Serialize for ApiKeyRecord {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        use serde::ser::SerializeStruct;
        let mut state = serializer.serialize_struct("ApiKeyRecord", 7)?;
        state.serialize_field("id", &self.id)?;
        state.serialize_field("organization_id", &self.organization_id)?;
        state.serialize_field("key_hash", &self.key_hash)?;
        state.serialize_field("queues", &self.queues)?;
        state.serialize_field("rate_limit", &self.rate_limit)?;
        state.serialize_field("is_active", &self.is_active)?;
        state.serialize_field("expires_at", &self.expires_at)?;
        state.end()
    }
}

impl<'de> serde::Deserialize<'de> for ApiKeyRecord {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(serde::Deserialize)]
        struct Helper {
            id: String,
            organization_id: String,
            key_hash: String,
            queues: Vec<String>,
            rate_limit: Option<i32>,
            is_active: bool,
            expires_at: Option<chrono::DateTime<Utc>>,
        }

        let helper = Helper::deserialize(deserializer)?;
        Ok(ApiKeyRecord {
            id: helper.id,
            organization_id: helper.organization_id,
            key_hash: helper.key_hash,
            queues: helper.queues,
            rate_limit: helper.rate_limit,
            is_active: helper.is_active,
            expires_at: helper.expires_at,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_hash_for_lookup() {
        let hash1 = hash_for_lookup("sp_test_abc123");
        let hash2 = hash_for_lookup("sp_test_abc123");
        let hash3 = hash_for_lookup("sp_test_xyz789");

        assert_eq!(hash1, hash2);
        assert_ne!(hash1, hash3);
        // Full SHA-256, not a 16-char truncation: a cache hit skips bcrypt, so
        // this digest is the whole margin on that path.
        assert_eq!(hash1.len(), 64);
        assert_eq!(hash1, crate::models::api_key_lookup_hash("sp_test_abc123"));
    }

    #[test]
    fn query_credential_is_extracted_and_percent_decoded() {
        assert_eq!(
            extract_query_credential("api_key=sp_live_abc"),
            Some("sp_live_abc".to_string())
        );
        assert_eq!(
            extract_query_credential("foo=1&token=eyJhbGc%3D&bar=2"),
            Some("eyJhbGc=".to_string())
        );
        assert_eq!(extract_query_credential("foo=1&bar=2"), None);
        assert_eq!(extract_query_credential(""), None);
        // Empty value must not authenticate as an empty token.
        assert_eq!(extract_query_credential("api_key="), None);
    }
}
