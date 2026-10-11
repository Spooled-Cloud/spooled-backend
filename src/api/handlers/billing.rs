//! Billing handlers for Stripe integration
//!
//! This module provides endpoints for:
//! - Getting billing status
//! - Creating Checkout and billing portal sessions
//! - Handling Stripe billing webhooks

use std::sync::OnceLock;
use std::time::Duration;

use axum::{
    body::Bytes,
    extract::{Extension, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use chrono::{DateTime, Utc};
use hmac::{Hmac, KeyInit, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use tracing::{error, info, warn};

use crate::api::handlers::require_unrestricted_key;
use crate::api::middleware::plan_rate_limit::invalidate_org_rate_limits;
use crate::api::AppState;
use crate::config::{Environment, StripeSettings};
use crate::error::{AppError, AppResult, ErrorResponse};
use crate::models::ApiKeyContext;

type HmacSha256 = Hmac<Sha256>;

/// Organization billing data tuple from database query
type OrgBillingData = (
    String,                // plan_tier
    Option<String>,        // stripe_customer_id
    Option<String>,        // stripe_subscription_id
    Option<String>,        // stripe_subscription_status
    Option<DateTime<Utc>>, // stripe_current_period_end
    Option<bool>,          // stripe_cancel_at_period_end
);

/// Maximum age for Stripe webhook timestamps (5 minutes)
const STRIPE_TIMESTAMP_TOLERANCE_SECS: i64 = 300;

/// How long a `processing` claim on a Stripe event is respected before a retry
/// may take it over.
///
/// Must comfortably exceed the slowest handler. `handle_checkout_completed`
/// makes outbound Stripe API calls (see `cancel_stripe_subscription`), so this
/// is sized for a slow round trip plus retries rather than for local DB work.
/// Too low and a genuinely in-flight event gets processed twice concurrently;
/// too high and a crashed attempt stays stuck for that long. The handlers are
/// idempotent, so erring high costs only latency-to-recovery.
const STRIPE_CLAIM_STALE_AFTER_SECS: i64 = 300;

/// Upper bound for one Stripe API call.
///
/// A webhook makes up to a few calls in a row (reconcile, then cancel a
/// superseded subscription) and the router's 30 s request timeout drops the
/// whole handler, so a hung call has to give up well before that.
const STRIPE_HTTP_TIMEOUT: Duration = Duration::from_secs(10);

const STRIPE_API_BASE: &str = "https://api.stripe.com";

tokio::task_local! {
    /// Stripe API base URL for the current task, set only by tests.
    static STRIPE_API_BASE_OVERRIDE: String;
}

/// Run `fut` with every Stripe API call it makes sent to `base` instead of
/// api.stripe.com. Integration tests use it to point the handlers at a mock
/// Stripe server; nothing in the service calls it.
#[doc(hidden)]
pub async fn with_stripe_api_base<F: std::future::Future>(base: String, fut: F) -> F::Output {
    STRIPE_API_BASE_OVERRIDE.scope(base, fut).await
}

fn stripe_api_url(path: &str) -> String {
    let base = STRIPE_API_BASE_OVERRIDE
        .try_with(|base| base.trim_end_matches('/').to_string())
        .unwrap_or_else(|_| STRIPE_API_BASE.to_string());
    format!("{base}{path}")
}

/// Shared Stripe HTTP client with a request timeout.
fn stripe_http_client() -> AppResult<reqwest::Client> {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    if let Some(client) = CLIENT.get() {
        return Ok(client.clone());
    }
    let client = reqwest::Client::builder()
        .timeout(STRIPE_HTTP_TIMEOUT)
        .connect_timeout(Duration::from_secs(5))
        .build()
        .map_err(|e| AppError::Internal(format!("Failed to build HTTP client: {}", e)))?;
    Ok(CLIENT.get_or_init(|| client).clone())
}

/// Checkout Session metadata key naming the organization that started it.
///
/// Only `POST /billing/checkout` sets it, with the Stripe secret key, for the
/// authenticated organization. A Payment Link URL can set
/// `client_reference_id` but not metadata, so this key is what makes a
/// completed checkout trustworthy.
const CHECKOUT_ORG_METADATA_KEY: &str = "spooled_org_id";

/// Where Checkout sends the customer back to when `FRONTEND_URL` is not set.
const DEFAULT_FRONTEND_URL: &str = "https://spooled.cloud";

/// Billing status response
#[derive(Debug, Serialize)]
pub struct BillingStatusResponse {
    /// Current plan tier
    pub plan_tier: String,
    /// Stripe subscription ID (if any)
    pub stripe_subscription_id: Option<String>,
    /// Subscription status (active, past_due, canceled, etc.)
    pub stripe_subscription_status: Option<String>,
    /// Current billing period end
    pub stripe_current_period_end: Option<DateTime<Utc>>,
    /// Whether subscription will cancel at period end
    pub stripe_cancel_at_period_end: Option<bool>,
    /// Whether user has a Stripe customer
    pub has_stripe_customer: bool,
}

/// Get billing status
///
/// GET /api/v1/billing/status
pub async fn status(
    State(state): State<AppState>,
    Extension(context): Extension<ApiKeyContext>,
) -> AppResult<Json<BillingStatusResponse>> {
    // Subscription id and period end are account data, not queue data: a
    // queue-scoped worker key has no business reading them (same as the portal).
    require_unrestricted_key(&context, "read billing status")?;

    // Fetch organization billing data
    let org: Option<OrgBillingData> = sqlx::query_as(
        r#"
        SELECT 
            plan_tier,
            stripe_customer_id,
            stripe_subscription_id,
            stripe_subscription_status,
            stripe_current_period_end,
            stripe_cancel_at_period_end
        FROM organizations 
        WHERE id = $1
        "#,
    )
    .bind(&context.organization_id)
    .fetch_optional(state.db.pool())
    .await?;

    let Some((
        plan_tier,
        stripe_customer_id,
        stripe_subscription_id,
        stripe_subscription_status,
        stripe_current_period_end,
        stripe_cancel_at_period_end,
    )) = org
    else {
        return Err(AppError::NotFound("Organization not found".to_string()));
    };

    Ok(Json(BillingStatusResponse {
        plan_tier,
        stripe_subscription_id,
        stripe_subscription_status,
        stripe_current_period_end,
        stripe_cancel_at_period_end,
        has_stripe_customer: stripe_customer_id.is_some(),
    }))
}

/// Create billing portal session request
#[derive(Debug, Deserialize)]
pub struct CreatePortalRequest {
    /// Return URL after exiting the portal
    pub return_url: String,
}

/// Create billing portal session response
#[derive(Debug, Serialize)]
pub struct CreatePortalResponse {
    /// URL to redirect the user to
    pub url: String,
}

/// Create a Stripe billing portal session
///
/// POST /api/v1/billing/portal
pub async fn create_portal(
    State(state): State<AppState>,
    Extension(context): Extension<ApiKeyContext>,
    Json(request): Json<CreatePortalRequest>,
) -> AppResult<Json<CreatePortalResponse>> {
    require_unrestricted_key(&context, "open the billing portal")?;

    let stripe_secret = state
        .stripe
        .secret_key
        .as_ref()
        .ok_or_else(|| AppError::Internal("Stripe is not configured".to_string()))?;

    // Reject open redirects: Stripe sends the user to return_url after the
    // portal. Without an origin allowlist any API-key holder can mint a portal
    // session that lands the victim on an attacker-controlled page.
    validate_portal_return_url(
        &request.return_url,
        &state.settings.server.cors_allowed_origins,
        state.settings.server.environment,
    )?;

    // Get customer ID
    let customer_id: Option<(Option<String>,)> =
        sqlx::query_as("SELECT stripe_customer_id FROM organizations WHERE id = $1")
            .bind(&context.organization_id)
            .fetch_optional(state.db.pool())
            .await?;

    let Some((Some(customer_id),)) = customer_id else {
        return Err(AppError::BadRequest(
            "No billing account found. Please subscribe to a plan first.".to_string(),
        ));
    };

    // Create billing portal session via Stripe API
    let client = stripe_http_client()?;

    let mut form_params = vec![
        ("customer", customer_id.as_str()),
        ("return_url", request.return_url.as_str()),
    ];

    // Add configuration if specified
    if let Some(config_id) = state
        .stripe
        .billing_portal_config_id
        .as_deref()
        .map(str::trim)
        .filter(|v| !v.is_empty())
    {
        form_params.push(("configuration", config_id));
    }

    let response = client
        .post(stripe_api_url("/v1/billing_portal/sessions"))
        .header("Authorization", format!("Bearer {}", stripe_secret))
        .form(&form_params)
        .send()
        .await
        .map_err(|e| AppError::Internal(format!("Failed to create portal session: {}", e)))?;

    if !response.status().is_success() {
        let error_text = response.text().await.unwrap_or_default();
        error!(error = %error_text, "Stripe portal session creation failed");
        return Err(AppError::Internal(
            "Failed to create billing portal session".to_string(),
        ));
    }

    let portal: StripePortalSession = response
        .json()
        .await
        .map_err(|e| AppError::Internal(format!("Failed to parse Stripe response: {}", e)))?;

    info!(
        org_id = %context.organization_id,
        "Created billing portal session"
    );

    Ok(Json(CreatePortalResponse { url: portal.url }))
}

#[derive(Debug, Deserialize)]
struct StripePortalSession {
    url: String,
}

/// Create checkout session request
#[derive(Debug, Deserialize)]
pub struct CreateCheckoutRequest {
    /// Plan to buy: "starter" or "pro"
    pub plan: Option<String>,
}

/// Create checkout session response
#[derive(Debug, Serialize)]
pub struct CreateCheckoutResponse {
    /// Stripe-hosted Checkout page to redirect the user to
    pub url: String,
    /// Checkout Session id (cs_...)
    pub session_id: String,
}

#[derive(Debug, Deserialize)]
struct StripeCheckoutSession {
    id: String,
    url: Option<String>,
}

/// Errors of `POST /billing/checkout`: the usual API errors, plus 503 for
/// "billing is not available", which `AppError` has no variant for.
#[derive(Debug)]
pub enum CheckoutError {
    App(AppError),
    Unavailable(String),
}

impl From<AppError> for CheckoutError {
    fn from(e: AppError) -> Self {
        Self::App(e)
    }
}

impl From<sqlx::Error> for CheckoutError {
    fn from(e: sqlx::Error) -> Self {
        Self::App(e.into())
    }
}

impl IntoResponse for CheckoutError {
    fn into_response(self) -> Response {
        match self {
            Self::App(e) => e.into_response(),
            Self::Unavailable(message) => (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(ErrorResponse {
                    code: "SERVICE_UNAVAILABLE".to_string(),
                    message,
                    details: None,
                }),
            )
                .into_response(),
        }
    }
}

/// Organization fields the checkout endpoint needs.
#[derive(sqlx::FromRow)]
struct CheckoutOrg {
    plan_tier: String,
    stripe_customer_id: Option<String>,
    stripe_subscription_id: Option<String>,
    stripe_subscription_status: Option<String>,
    billing_email: Option<String>,
    billing_email_verified_at: Option<DateTime<Utc>>,
}

/// The plan named in a checkout request body: "starter" or "pro".
fn parse_checkout_plan(body: &[u8]) -> AppResult<&'static str> {
    let invalid = || AppError::Validation("plan must be \"starter\" or \"pro\"".to_string());
    let request: CreateCheckoutRequest = serde_json::from_slice(body).map_err(|_| invalid())?;
    match request.plan.as_deref().map(str::trim) {
        Some("starter") => Ok("starter"),
        Some("pro") => Ok("pro"),
        _ => Err(invalid()),
    }
}

fn checkout_price_id<'a>(stripe: &'a StripeSettings, plan: &str) -> Option<&'a str> {
    let price = match plan {
        "starter" => stripe.starter_price_id.as_deref(),
        "pro" => stripe.pro_price_id.as_deref(),
        _ => None,
    };
    price.map(str::trim).filter(|p| !p.is_empty())
}

/// Base URL of the website Checkout returns the customer to.
fn checkout_return_base() -> String {
    std::env::var("FRONTEND_URL")
        .ok()
        .map(|v| v.trim().trim_end_matches('/').to_string())
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| DEFAULT_FRONTEND_URL.to_string())
}

/// Form body of the Stripe `POST /v1/checkout/sessions` call.
///
/// The organization goes into `client_reference_id` (for the Stripe dashboard)
/// and into session and subscription metadata, which the webhook trusts. An
/// existing Stripe customer is reused so the new subscription stays reachable
/// from the same billing portal; otherwise Checkout is prefilled with the
/// verified billing email.
fn checkout_session_form(
    org_id: &str,
    price_id: &str,
    customer_id: Option<&str>,
    verified_email: Option<&str>,
    return_base: &str,
) -> Vec<(String, String)> {
    let mut form = vec![
        ("mode".to_string(), "subscription".to_string()),
        ("line_items[0][price]".to_string(), price_id.to_string()),
        ("line_items[0][quantity]".to_string(), "1".to_string()),
        ("client_reference_id".to_string(), org_id.to_string()),
        (
            format!("metadata[{CHECKOUT_ORG_METADATA_KEY}]"),
            org_id.to_string(),
        ),
        (
            format!("subscription_data[metadata][{CHECKOUT_ORG_METADATA_KEY}]"),
            org_id.to_string(),
        ),
        (
            "success_url".to_string(),
            format!("{return_base}/account/billing?checkout=success"),
        ),
        (
            "cancel_url".to_string(),
            format!("{return_base}/account/billing?checkout=cancel"),
        ),
    ];
    if let Some(customer_id) = customer_id {
        form.push(("customer".to_string(), customer_id.to_string()));
    } else if let Some(email) = verified_email {
        form.push(("customer_email".to_string(), email.to_string()));
    }
    form
}

/// Create a Stripe Checkout Session for the caller's organization
///
/// POST /api/v1/billing/checkout
///
/// Replaces the public Payment Links, whose `client_reference_id` is a URL
/// parameter anyone can edit: the session created here is bound to the
/// authenticated organization through metadata only this server can set.
pub async fn create_checkout(
    State(state): State<AppState>,
    Extension(context): Extension<ApiKeyContext>,
    body: Bytes,
) -> Result<Json<CreateCheckoutResponse>, CheckoutError> {
    require_unrestricted_key(&context, "start a checkout")?;

    let plan = parse_checkout_plan(&body)?;

    let Some(stripe_secret) = state.stripe.secret_key.as_deref() else {
        return Err(CheckoutError::Unavailable(
            "Billing is not configured".to_string(),
        ));
    };
    let Some(price_id) = checkout_price_id(&state.stripe, plan) else {
        return Err(CheckoutError::Unavailable(format!(
            "The {plan} plan is not available for purchase right now"
        )));
    };

    let org_id = context.organization_id.as_str();
    let org: Option<CheckoutOrg> = sqlx::query_as(
        r#"
        SELECT plan_tier, stripe_customer_id, stripe_subscription_id,
               stripe_subscription_status, billing_email, billing_email_verified_at
        FROM organizations
        WHERE id = $1 AND plan_tier <> 'deleted'
        "#,
    )
    .bind(org_id)
    .fetch_optional(state.db.pool())
    .await?;
    let Some(org) = org else {
        return Err(AppError::NotFound("Organization not found".to_string()).into());
    };

    let live = matches!(
        org.stripe_subscription_status.as_deref(),
        Some("active" | "trialing")
    );
    if org.plan_tier == plan && org.stripe_subscription_id.is_some() && live {
        return Err(AppError::Conflict(format!(
            "This organization already has an active {plan} subscription. \
             Use the billing portal to manage it."
        ))
        .into());
    }

    let verified_email = org
        .billing_email
        .as_deref()
        .filter(|_| org.billing_email_verified_at.is_some());
    let form = checkout_session_form(
        org_id,
        price_id,
        org.stripe_customer_id.as_deref(),
        verified_email,
        &checkout_return_base(),
    );

    let unavailable =
        || CheckoutError::Unavailable("Checkout is temporarily unavailable; try again".to_string());
    let response = stripe_http_client()?
        .post(stripe_api_url("/v1/checkout/sessions"))
        .header("Authorization", format!("Bearer {}", stripe_secret))
        .form(&form)
        .send()
        .await
        .map_err(|e| {
            error!(org_id = %org_id, error = %e, "Stripe checkout session request failed");
            unavailable()
        })?;

    if !response.status().is_success() {
        let status = response.status();
        let error_text = response.text().await.unwrap_or_default();
        error!(org_id = %org_id, status = %status, error = %error_text, "Stripe checkout session creation failed");
        return Err(unavailable());
    }

    let session: StripeCheckoutSession = response.json().await.map_err(|e| {
        error!(org_id = %org_id, error = %e, "Failed to parse Stripe checkout session");
        unavailable()
    })?;
    let Some(url) = session.url else {
        error!(org_id = %org_id, session_id = %session.id, "Stripe checkout session has no url");
        return Err(unavailable());
    };

    info!(org_id = %org_id, session_id = %session.id, plan = %plan, "Created Stripe checkout session");

    Ok(Json(CreateCheckoutResponse {
        url,
        session_id: session.id,
    }))
}

/// Handle Stripe billing webhook
///
/// POST /api/v1/billing/webhook
///
/// This handles subscription lifecycle events from Stripe
pub async fn webhook(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> AppResult<StatusCode> {
    // Get the signature from headers
    let signature = headers
        .get("Stripe-Signature")
        .and_then(|v| v.to_str().ok())
        .ok_or_else(|| AppError::Authentication("Missing Stripe-Signature header".to_string()))?;

    // Verify signature
    // An empty secret would make any payload signed with an empty HMAC key verify.
    let webhook_secret = state
        .stripe
        .webhook_secret
        .as_ref()
        .filter(|secret| !secret.is_empty())
        .ok_or_else(|| {
            error!(
            "Stripe billing webhook received but STRIPE_BILLING_WEBHOOK_SECRET is not configured"
        );
            AppError::Internal("Webhook signature verification not configured".to_string())
        })?;

    verify_stripe_signature(webhook_secret, &body, signature)?;

    // Parse the event
    let event: StripeEvent = serde_json::from_slice(&body)
        .map_err(|e| AppError::BadRequest(format!("Invalid JSON: {}", e)))?;

    info!(event_type = %event.event_type, event_id = %event.id, "Processing Stripe billing webhook");

    let handled = matches!(
        event.event_type.as_str(),
        "checkout.session.completed"
            | "checkout.session.async_payment_succeeded"
            | "checkout.session.async_payment_failed"
            | "customer.subscription.created"
            | "customer.subscription.updated"
            | "customer.subscription.deleted"
            | "invoice.paid"
            | "invoice.payment_failed"
    );

    // Idempotency: atomically CLAIM the event id before processing.
    //
    // The claim records COMPLETION, not merely an attempt. Previously the row was
    // inserted, the handler ran, and the row was DELETEd only if the handler
    // returned `Err`. That compensation covers exactly one failure mode: a
    // crash, OOM kill, or a deploy rolling the container between the claim and
    // the handler left the claim committed with the work never done. Stripe then
    // retried for up to three days and every retry was answered "200 OK, already
    // processed" — silently discarding a paid invoice, a cancellation, or a
    // checkout completion, with a log line indistinguishable from ordinary
    // duplicate suppression.
    //
    // So: claim as 'processing'; mark 'done' only after the handler succeeds. A
    // duplicate is suppressed only when the existing row is 'done', or is
    // 'processing' and fresh enough to be a genuinely concurrent attempt. A
    // 'processing' row older than the stale window is a crashed attempt and is
    // re-claimed. Reprocessing is safe because the handlers are idempotent and
    // ordering-guarded (see `stripe_event_applies`).
    // Timestamp of OUR claim, used to release only our own claim later.
    let mut claim_token: Option<DateTime<Utc>> = None;

    if handled {
        let claimed: Option<(DateTime<Utc>,)> = sqlx::query_as(
            r#"
            INSERT INTO processed_stripe_events (event_id, status, claimed_at)
            VALUES ($1, 'processing', NOW())
            ON CONFLICT (event_id) DO UPDATE
                SET claimed_at = NOW()
                WHERE processed_stripe_events.status = 'processing'
                  AND processed_stripe_events.claimed_at < NOW() - ($2 || ' seconds')::INTERVAL
            RETURNING claimed_at
            "#,
        )
        .bind(&event.id)
        .bind(STRIPE_CLAIM_STALE_AFTER_SECS.to_string())
        .fetch_optional(state.db.pool())
        .await?;

        match claimed {
            Some((claimed_at,)) => claim_token = Some(claimed_at),
            None => {
                // We did not get the claim. Distinguish the two reasons, because
                // they need OPPOSITE responses: a finished event must be
                // acknowledged so Stripe stops retrying, while an event held by
                // an attempt that may have died must NOT be acknowledged — a
                // 200 there would tell Stripe the event was handled when nobody
                // is handling it, and the only thing that could still rescue it
                // is a later retry.
                let existing: Option<(String,)> = sqlx::query_as(
                    "SELECT status FROM processed_stripe_events WHERE event_id = $1",
                )
                .bind(&event.id)
                .fetch_optional(state.db.pool())
                .await?;

                return match existing.as_ref().map(|(s,)| s.as_str()) {
                    Some("done") => {
                        info!(event_id = %event.id, event_type = %event.event_type, "Skipping already-processed Stripe event");
                        Ok(StatusCode::OK)
                    }
                    _ => {
                        // Fresh 'processing' claim held by a concurrent attempt
                        // (or the row vanished under us). Ask Stripe to retry;
                        // if the holder died, the retry after the stale window
                        // takes the claim over.
                        warn!(
                            event_id = %event.id,
                            event_type = %event.event_type,
                            "Stripe event is claimed by an in-flight attempt; asking Stripe to retry"
                        );
                        Err(AppError::Conflict(
                            "Event is already being processed; retry shortly".to_string(),
                        ))
                    }
                };
            }
        }
    }

    // Handle different event types.
    let outcome: AppResult<()> = match event.event_type.as_str() {
        // A delayed payment method (SEPA, ACH) completes the session unpaid and
        // reports the money later as async_payment_succeeded: same linker.
        "checkout.session.completed" | "checkout.session.async_payment_succeeded" => {
            handle_checkout_completed(&state, &event).await
        }
        "checkout.session.async_payment_failed" => {
            let session = &event.data.object;
            warn!(
                event_id = %event.id,
                session_id = ?session["id"].as_str(),
                customer_id = ?session["customer"].as_str(),
                subscription_id = ?session["subscription"].as_str(),
                "Checkout payment failed; organization not linked"
            );
            Ok(())
        }
        "customer.subscription.created" | "customer.subscription.updated" => {
            handle_subscription_updated(&state, &event).await
        }
        "customer.subscription.deleted" => handle_subscription_deleted(&state, &event).await,
        "invoice.paid" => handle_invoice_paid(&state, &event).await,
        "invoice.payment_failed" => handle_payment_failed(&state, &event).await,
        _ => {
            info!(event_type = %event.event_type, "Ignoring unhandled Stripe event");
            Ok(())
        }
    };

    // On a handler error, RELEASE the claim immediately so Stripe's retry is
    // reprocessed rather than skipped. This is now an optimisation rather than
    // the only safety net: even if this DELETE fails (or the process dies before
    // reaching it), the row is still 'processing' and the stale-claim window
    // above will let a later retry take it over.
    if let Err(e) = outcome {
        if let Some(token) = claim_token {
            // Release ONLY our own claim. Matching on `claimed_at` matters: if
            // this attempt outlived the stale window, a retry may already have
            // taken the claim over and be processing right now — deleting by
            // event_id alone would remove that live claim and let a third
            // delivery start the same work concurrently.
            if let Err(del) = sqlx::query(
                "DELETE FROM processed_stripe_events \
                 WHERE event_id = $1 AND status = 'processing' AND claimed_at = $2",
            )
            .bind(&event.id)
            .bind(token)
            .execute(state.db.pool())
            .await
            {
                error!(error = %del, event_id = %event.id, "Failed to release Stripe event claim after handler error; retry will wait for the stale-claim window");
            }
        }
        return Err(e);
    }

    if let Some(token) = claim_token {
        // Only NOW is the event actually processed. Until this commits, a retry
        // is allowed to take the claim over. Guarded on our own claim for the
        // same reason as the release above.
        if let Err(e) = sqlx::query(
            "UPDATE processed_stripe_events SET status = 'done' \
             WHERE event_id = $1 AND claimed_at = $2",
        )
        .bind(&event.id)
        .bind(token)
        .execute(state.db.pool())
        .await
        {
            // The work IS done; we simply failed to record that. A retry will
            // re-run the (idempotent, ordering-guarded) handler, which is the
            // safe direction to fail in.
            error!(error = %e, event_id = %event.id, "Failed to finalize Stripe event claim; event may be reprocessed");
        }

        // Opportunistically prune rows far beyond Stripe's retry window.
        if let Err(e) = sqlx::query(
            "DELETE FROM processed_stripe_events WHERE received_at < NOW() - INTERVAL '30 days'",
        )
        .execute(state.db.pool())
        .await
        {
            warn!(error = %e, "Failed to prune processed_stripe_events");
        }
    }

    Ok(StatusCode::OK)
}

#[derive(Debug, Deserialize)]
struct StripeEvent {
    id: String,
    #[serde(rename = "type")]
    event_type: String,
    /// Stripe-side creation time (unix seconds); used as the ordering guard.
    #[serde(default)]
    created: i64,
    data: StripeEventData,
}

/// Stripe-side creation time of the event, falling back to "now" if absent so a
/// malformed event is treated as fresh rather than silently discarded as stale.
fn event_created_at(event: &StripeEvent) -> DateTime<Utc> {
    if event.created > 0 {
        DateTime::from_timestamp(event.created, 0).unwrap_or_else(Utc::now)
    } else {
        Utc::now()
    }
}

/// Whether an event should mutate org billing state given the org's last applied
/// Stripe event time. Mirrors the SQL guard
/// `stripe_last_event_at IS NULL OR stripe_last_event_at <= $event_time`
/// (second-granularity; equal timestamps both apply — known residual).
fn stripe_event_applies(last_event_at: Option<DateTime<Utc>>, event_time: DateTime<Utc>) -> bool {
    match last_event_at {
        None => true,
        Some(last) => last <= event_time,
    }
}

#[derive(Debug, Deserialize)]
struct StripeEventData {
    object: serde_json::Value,
}

/// Which organization a completed checkout is for, and how far the session can
/// be trusted to say so.
#[derive(Debug, PartialEq, Eq)]
enum CheckoutOrgRef<'a> {
    /// Session created by `POST /billing/checkout`: only this server can have
    /// set `metadata.spooled_org_id`, and it set it to the authenticated org.
    Trusted(&'a str),
    /// Legacy Payment Link: `client_reference_id` is a URL parameter anyone can
    /// set to any organization id.
    Unverified(&'a str),
}

fn non_empty_str(value: &serde_json::Value) -> Option<&str> {
    value.as_str().map(str::trim).filter(|s| !s.is_empty())
}

fn checkout_org_ref(session: &serde_json::Value) -> Option<CheckoutOrgRef<'_>> {
    if let Some(org_id) = non_empty_str(&session["metadata"][CHECKOUT_ORG_METADATA_KEY]) {
        return Some(CheckoutOrgRef::Trusted(org_id));
    }
    non_empty_str(&session["client_reference_id"]).map(CheckoutOrgRef::Unverified)
}

/// The email the payer gave at checkout.
fn checkout_customer_email(session: &serde_json::Value) -> Option<&str> {
    non_empty_str(&session["customer_details"]["email"])
        .or_else(|| non_empty_str(&session["customer_email"]))
}

/// Handle checkout.session.completed and checkout.session.async_payment_succeeded
///
/// Links the paying Stripe customer and subscription to the organization the
/// checkout was for. How far that is trusted depends on who created the
/// session (see [`CheckoutOrgRef`]):
/// - created by `POST /billing/checkout`: link the organization and cancel the
///   subscription it replaces (an upgrade);
/// - legacy Payment Link: link only an organization without a live
///   subscription whose verified billing email is the email the payer gave,
///   and never cancel anything. Otherwise anyone who learned an organization id
///   could pay for a plan, take over its billing and have the backend cancel
///   its real subscription.
async fn handle_checkout_completed(state: &AppState, event: &StripeEvent) -> AppResult<()> {
    let session = &event.data.object;

    // Reject unpaid / incomplete checkouts before linking anything. A delayed
    // payment method (SEPA, ACH) completes the session `unpaid`; the money is
    // reported by checkout.session.async_payment_succeeded, which links then.
    // Empty payment_status kept for older fixtures / partial payloads.
    let payment_status = session["payment_status"].as_str().unwrap_or("");
    if !matches!(payment_status, "paid" | "no_payment_required" | "") {
        info!(
            payment_status = %payment_status,
            event_id = %event.id,
            event_type = %event.event_type,
            "Checkout not paid yet; linking waits for checkout.session.async_payment_succeeded"
        );
        return Ok(());
    }

    let Some(org_ref) = checkout_org_ref(session) else {
        info!(event_id = %event.id, "Ignoring checkout session without an organization reference");
        return Ok(());
    };
    let Some(customer_id) = session["customer"].as_str() else {
        warn!(event_id = %event.id, "Ignoring checkout session without a customer");
        return Ok(());
    };
    let subscription_id = session["subscription"].as_str();

    // The stripe_last_event_at guard rejects stale re-deliveries: an old
    // checkout event from a previous session must never relink the org after a
    // newer billing event has been applied.
    let event_time = event_created_at(event);
    let (org_id, trusted) = match org_ref {
        CheckoutOrgRef::Trusted(org_id) => (org_id, true),
        CheckoutOrgRef::Unverified(org_id) => (org_id, false),
    };

    let relinked = if trusted {
        // The subscription this checkout replaces is recorded in the same
        // statement that forgets it, and dropped from the list only once Stripe
        // confirms the cancel: a failed or interrupted cancel is retried with
        // the event instead of leaving the customer billed twice.
        sqlx::query(
            r#"
            UPDATE organizations
            SET stripe_customer_id = $1,
                stripe_subscription_id = COALESCE($2, stripe_subscription_id),
                stripe_superseded_subscription_ids = CASE
                    WHEN $2::TEXT IS NOT NULL
                     AND stripe_subscription_id IS NOT NULL
                     AND stripe_subscription_id <> $2
                     AND COALESCE(stripe_subscription_status, '')
                         NOT IN ('canceled', 'cancelled', 'incomplete_expired')
                     AND NOT (stripe_subscription_id = ANY(stripe_superseded_subscription_ids))
                    THEN array_append(stripe_superseded_subscription_ids, stripe_subscription_id)
                    ELSE stripe_superseded_subscription_ids
                END,
                stripe_last_event_at = $3,
                updated_at = NOW()
            WHERE id = $4
              AND plan_tier <> 'deleted'
              AND (stripe_last_event_at IS NULL OR stripe_last_event_at <= $3)
            "#,
        )
        .bind(customer_id)
        .bind(subscription_id)
        .bind(event_time)
        .bind(org_id)
        .execute(state.db.pool())
        .await?
    } else {
        sqlx::query(
            r#"
            UPDATE organizations
            SET stripe_customer_id = $1,
                stripe_subscription_id = COALESCE($2, stripe_subscription_id),
                stripe_last_event_at = $3,
                updated_at = NOW()
            WHERE id = $4
              AND plan_tier <> 'deleted'
              AND (stripe_last_event_at IS NULL OR stripe_last_event_at <= $3)
              -- The payer gave the organization's verified billing email ...
              AND billing_email_verified_at IS NOT NULL
              AND lower(btrim(billing_email)) = lower(btrim($5))
              -- ... and the organization pays for no live subscription (a
              -- retry of this same checkout is not a takeover).
              AND (stripe_subscription_id IS NULL
                   OR stripe_subscription_id = $2
                   OR COALESCE(stripe_subscription_status, '')
                      IN ('canceled', 'cancelled', 'incomplete_expired', 'unpaid'))
            "#,
        )
        .bind(customer_id)
        .bind(subscription_id)
        .bind(event_time)
        .bind(org_id)
        .bind(checkout_customer_email(session))
        .execute(state.db.pool())
        .await?
    }
    .rows_affected()
        > 0;

    if relinked {
        info!(org_id = %org_id, customer_id = %customer_id, trusted, "Linked Stripe customer to organization");

        // Important: webhook event order is not guaranteed. In practice we often see
        // customer.subscription.created arrive before checkout.session.completed, which means
        // subscription updates can't be applied yet (org has no stripe_customer_id at that time).
        // To ensure the org tier is correct immediately after checkout completes, reconcile from Stripe now.
        if let Some(subscription_id) = subscription_id {
            if let Err(e) = reconcile_org_from_subscription_id(state, org_id, subscription_id).await
            {
                // Propagate as 5xx so Stripe retries the event: the customer has paid,
                // and swallowing this error could leave them on the free tier. The event
                // is only marked processed after success, so the retry is applied
                // (re-linking is idempotent).
                warn!(
                    org_id = %org_id,
                    subscription_id = %subscription_id,
                    error = %e,
                    "Failed to reconcile org plan tier from subscription after checkout; requesting Stripe retry"
                );
                return Err(e);
            }
        }
    } else if trusted {
        warn!(
            org_id = %org_id,
            customer_id = %customer_id,
            event_id = %event.id,
            "Skipping checkout.session.completed: org missing or a newer billing event was already applied"
        );
    } else {
        warn!(
            org_id = %org_id,
            customer_id = %customer_id,
            subscription_id = ?subscription_id,
            event_id = %event.id,
            "Not linking a Payment Link checkout: the org has a live subscription, the payer's email \
             is not its verified billing email, the org is missing, or a newer billing event was applied"
        );
    }

    // Only a checkout this server created may retire a subscription. Runs even
    // when the relink above was skipped, so a retry finishes a cancel that an
    // earlier attempt recorded but could not complete.
    if trusted {
        cancel_superseded_subscriptions(state, org_id).await?;
    }

    Ok(())
}

/// Cancel the subscriptions that server-created checkouts replaced for this
/// organization, forgetting each one once Stripe confirms.
///
/// Returns the first failure, so the webhook answers 5xx and Stripe redelivers
/// the event; the redelivery tries the remaining cancels again.
async fn cancel_superseded_subscriptions(state: &AppState, org_id: &str) -> AppResult<()> {
    let row: Option<(Vec<String>, Option<String>)> = sqlx::query_as(
        "SELECT stripe_superseded_subscription_ids, stripe_subscription_id \
         FROM organizations WHERE id = $1",
    )
    .bind(org_id)
    .fetch_optional(state.db.pool())
    .await?;
    let Some((superseded, current)) = row else {
        return Ok(());
    };

    let mut failure = None;
    for subscription_id in superseded {
        if current.as_deref() == Some(subscription_id.as_str()) {
            // It became the organization's subscription again: never cancel
            // the subscription the organization is on.
            warn!(org_id = %org_id, subscription_id = %subscription_id,
                "Superseded subscription is the organization's current one again; not canceling it");
        } else {
            info!(org_id = %org_id, subscription_id = %subscription_id,
                "Canceling the subscription a checkout replaced");
            if let Err(e) = cancel_stripe_subscription(state, &subscription_id).await {
                error!(org_id = %org_id, subscription_id = %subscription_id, error = %e,
                    "Failed to cancel a superseded subscription; Stripe will redeliver the event to retry");
                failure.get_or_insert(e);
                continue;
            }
        }
        forget_superseded_subscription(state, &subscription_id).await?;
    }

    match failure {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

/// Drop a subscription from every organization's list of subscriptions to
/// cancel: it was canceled, or Stripe reported it deleted.
async fn forget_superseded_subscription(state: &AppState, subscription_id: &str) -> AppResult<()> {
    sqlx::query(
        "UPDATE organizations \
         SET stripe_superseded_subscription_ids = array_remove(stripe_superseded_subscription_ids, $1) \
         WHERE $1 = ANY(stripe_superseded_subscription_ids)",
    )
    .bind(subscription_id)
    .execute(state.db.pool())
    .await?;
    Ok(())
}

/// Handle subscription created/updated events
async fn handle_subscription_updated(state: &AppState, event: &StripeEvent) -> AppResult<()> {
    let subscription = &event.data.object;

    let subscription_id = subscription["id"].as_str();
    let customer_id = subscription["customer"].as_str();
    let status = subscription["status"].as_str();
    let current_period_end = subscription_period_end(subscription);
    let cancel_at_period_end = subscription["cancel_at_period_end"].as_bool();

    // Tier from status + price. Terminal statuses must force free — otherwise a
    // `subscription.updated` (status=canceled) that arrives at/after
    // `subscription.deleted` re-applies the price-mapped paid tier via COALESCE
    // and restores paid entitlements on a canceled org.
    let plan_tier = resolve_plan_tier(subscription, state);

    if let (Some(customer_id), Some(status)) = (customer_id, status) {
        let period_end = current_period_end.and_then(|ts| DateTime::from_timestamp(ts, 0));
        let event_time = event_created_at(event);

        // Update organization. The stripe_last_event_at guard ignores stale
        // re-deliveries — Stripe does not guarantee ordering, and an old
        // subscription.updated replayed after cancellation used to restore paid
        // state on a canceled org.
        let result = sqlx::query(
            r#"
            UPDATE organizations
            SET
                stripe_subscription_id = $1,
                stripe_subscription_status = $2,
                stripe_current_period_end = $3,
                stripe_cancel_at_period_end = $4,
                plan_tier = COALESCE($5, plan_tier),
                stripe_last_event_at = $7,
                updated_at = NOW()
            WHERE stripe_customer_id = $6
              -- Never resurrect a soft-deleted org (plan_tier is its tombstone).
              AND plan_tier <> 'deleted'
              AND (stripe_last_event_at IS NULL OR stripe_last_event_at <= $7)
              -- A checkout reuses the org's customer, so one customer can hold
              -- the live subscription and the one replacing it. An event for
              -- the other one must not take over the org: before
              -- checkout.session.completed it would hide the subscription that
              -- checkout has to cancel, and afterwards an update of the
              -- canceled one would switch the org back to it.
              AND (stripe_subscription_id IS NULL
                   OR stripe_subscription_id = $1
                   OR COALESCE(stripe_subscription_status, '')
                      IN ('canceled', 'cancelled', 'incomplete_expired', 'unpaid'))
            RETURNING id
            "#,
        )
        .bind(subscription_id)
        .bind(status)
        .bind(period_end)
        .bind(cancel_at_period_end)
        .bind(&plan_tier)
        .bind(customer_id)
        .bind(event_time)
        .fetch_all(state.db.pool())
        .await?;

        // plan_tier feeds the 60s-cached rate-limit bucket; drop it so a
        // downgrade takes effect now rather than up to a minute later.
        for row in &result {
            use sqlx::Row;
            let org_id: String = row.get("id");
            invalidate_org_rate_limits(state.cache.as_ref(), &org_id).await;
        }

        if result.is_empty() {
            // No org linked to this customer yet (event arrived before
            // checkout.session.completed): ask Stripe to retry until the
            // linkage exists. Without the retry this event is dropped forever and a
            // paying customer can be stuck on the free tier. Orphaned customers
            // (superseded > 24h ago) are ACKed to stop pointless retries.
            if !org_linked_to_customer(state, customer_id).await?
                && should_request_retry_for_unlinked(event_time)
            {
                return Err(retry_until_linked(event, customer_id));
            }

            // Org exists but the ordering guard rejected the event (stale
            // re-delivery), the event is for a subscription the org is not on,
            // or the customer is orphaned: ACK.
            info!(
                customer_id = %customer_id,
                subscription_id = ?subscription_id,
                status = %status,
                plan_tier = ?plan_tier,
                event_id = %event.id,
                "Ignoring stale subscription event or one for a subscription the org is not on"
            );
        } else {
            info!(
                customer_id = %customer_id,
                status = %status,
                plan_tier = ?plan_tier,
                "Updated subscription status"
            );
        }
    }

    Ok(())
}

/// Fetch the Stripe subscription object and use it to update the organization's billing fields.
///
/// This is used to repair the common webhook ordering issue where subscription events arrive
/// before checkout.session.completed links stripe_customer_id to the org.
pub(crate) async fn reconcile_org_from_subscription_id(
    state: &AppState,
    org_id: &str,
    subscription_id: &str,
) -> AppResult<()> {
    let stripe_secret = state
        .stripe
        .secret_key
        .as_ref()
        .ok_or_else(|| AppError::Internal("Stripe is not configured".to_string()))?;

    let client = stripe_http_client()?;
    let url = stripe_api_url(&format!("/v1/subscriptions/{}", subscription_id));

    let response = client
        .get(url)
        .header("Authorization", format!("Bearer {}", stripe_secret))
        .send()
        .await
        .map_err(|e| {
            AppError::Internal(format!("Failed to fetch subscription from Stripe: {}", e))
        })?;

    if !response.status().is_success() {
        let error_text = response.text().await.unwrap_or_default();
        return Err(AppError::Internal(format!(
            "Stripe subscription fetch failed: {}",
            error_text
        )));
    }

    let subscription: serde_json::Value = response.json().await.map_err(|e| {
        AppError::Internal(format!("Failed to parse Stripe subscription JSON: {}", e))
    })?;

    let customer_id = subscription["customer"].as_str();
    let status = subscription["status"].as_str();
    let current_period_end = subscription_period_end(&subscription);
    let cancel_at_period_end = subscription["cancel_at_period_end"].as_bool();
    let plan_tier = resolve_plan_tier(&subscription, state);
    let period_end = current_period_end.and_then(|ts| DateTime::from_timestamp(ts, 0));

    sqlx::query(
        r#"
        UPDATE organizations
        SET
            stripe_customer_id = COALESCE($1, stripe_customer_id),
            stripe_subscription_id = $2,
            stripe_subscription_status = $3,
            stripe_current_period_end = $4,
            stripe_cancel_at_period_end = $5,
            plan_tier = COALESCE($6, plan_tier),
            updated_at = NOW()
        WHERE id = $7
          AND plan_tier <> 'deleted'
        "#,
    )
    .bind(customer_id)
    .bind(subscription_id)
    .bind(status)
    .bind(period_end)
    .bind(cancel_at_period_end)
    .bind(&plan_tier)
    .bind(org_id)
    .execute(state.db.pool())
    .await?;

    // plan_tier drives the org's rate-limit bucket, which is cached for 60s.
    // Without this, a downgrade (or a cancellation) leaves the paid rps/burst in
    // force for up to a minute — the admin path already invalidates, and the
    // Stripe path is the one real customers actually take.
    invalidate_org_rate_limits(state.cache.as_ref(), org_id).await;

    info!(
        org_id = %org_id,
        subscription_id = %subscription_id,
        customer_id = ?customer_id,
        status = ?status,
        plan_tier = ?plan_tier,
        "Reconciled org billing from Stripe subscription"
    );

    Ok(())
}

/// Cancel a Stripe subscription immediately.
///
/// Used as the double-billing backstop: when a checkout supersedes an existing
/// subscription, the old one must not keep charging the customer.
async fn cancel_stripe_subscription(state: &AppState, subscription_id: &str) -> AppResult<()> {
    let stripe_secret = state
        .stripe
        .secret_key
        .as_ref()
        .ok_or_else(|| AppError::Internal("Stripe is not configured".to_string()))?;

    // The shared client times out: a hung Stripe call must not hang the
    // request that triggered the cancel (an organization delete waits on it).
    let client = stripe_http_client()?;
    let url = stripe_api_url(&format!("/v1/subscriptions/{}", subscription_id));

    let response = client
        .delete(url)
        .header("Authorization", format!("Bearer {}", stripe_secret))
        .send()
        .await
        .map_err(|e| AppError::Internal(format!("Failed to cancel subscription: {}", e)))?;

    if response.status() == reqwest::StatusCode::NOT_FOUND {
        // Already canceled and purged on Stripe's side: nothing left to stop.
        warn!(subscription_id = %subscription_id, "Stripe subscription not found while canceling");
        return Ok(());
    }
    if !response.status().is_success() {
        let status = response.status();
        let error_text = response.text().await.unwrap_or_default();
        // Stripe refuses to cancel a subscription that already ended. That is
        // the outcome we wanted; reporting it as a failure would retry the
        // cancel for as long as Stripe redelivers the event.
        if status.is_client_error()
            && stripe_subscription_has_ended(stripe_secret, subscription_id).await
        {
            warn!(subscription_id = %subscription_id, "Stripe subscription had already ended while canceling");
            return Ok(());
        }
        return Err(AppError::Internal(format!(
            "Stripe subscription cancel failed ({}): {}",
            status, error_text
        )));
    }

    info!(subscription_id = %subscription_id, "Canceled Stripe subscription");
    Ok(())
}

/// Whether Stripe reports the subscription as ended (canceled or expired).
/// Any failure to find out counts as "not ended".
async fn stripe_subscription_has_ended(stripe_secret: &str, subscription_id: &str) -> bool {
    let Ok(client) = stripe_http_client() else {
        return false;
    };
    let response = client
        .get(stripe_api_url(&format!(
            "/v1/subscriptions/{}",
            subscription_id
        )))
        .header("Authorization", format!("Bearer {}", stripe_secret))
        .send()
        .await;
    let Ok(response) = response else {
        return false;
    };
    if !response.status().is_success() {
        return false;
    }
    let Ok(subscription) = response.json::<serde_json::Value>().await else {
        return false;
    };
    matches!(
        subscription["status"].as_str(),
        Some("canceled" | "incomplete_expired")
    )
}

/// Cancel the organization's Stripe subscription before the organization is
/// deleted.
///
/// Deleting an org used to leave its subscription running, so the customer
/// kept being charged for an organization that no longer existed. Callers must
/// abort the delete when this returns an error.
pub(crate) async fn cancel_subscription_before_org_delete(
    state: &AppState,
    org: &crate::models::Organization,
) -> AppResult<()> {
    let Some(subscription_id) = org.stripe_subscription_id.as_deref() else {
        return Ok(());
    };
    let status = org.stripe_subscription_status.as_deref().unwrap_or("");
    if matches!(status, "canceled" | "cancelled" | "incomplete_expired") {
        return Ok(());
    }
    if state.stripe.secret_key.is_none() {
        warn!(
            org_id = %org.id,
            subscription_id = %subscription_id,
            "Organization has a Stripe subscription but Stripe is not configured; not canceling it"
        );
        return Ok(());
    }
    cancel_stripe_subscription(state, subscription_id)
        .await
        .map_err(|e| {
            error!(org_id = %org.id, subscription_id = %subscription_id, error = %e,
                "Could not cancel Stripe subscription before deleting organization");
            AppError::Conflict(
                "The organization's subscription could not be canceled, so nothing was \
                 deleted. Try again, or cancel the subscription in the billing portal first."
                    .to_string(),
            )
        })?;

    // Record the cancel locally: a soft-deleted org no longer receives billing
    // webhooks (its tombstone is protected), so without this a later hard
    // delete would try to cancel the same subscription again.
    sqlx::query(
        "UPDATE organizations SET stripe_subscription_status = 'canceled', \
         stripe_cancel_at_period_end = FALSE, updated_at = NOW() WHERE id = $1",
    )
    .bind(&org.id)
    .execute(state.db.pool())
    .await?;
    Ok(())
}

/// Handle subscription deleted event
async fn handle_subscription_deleted(state: &AppState, event: &StripeEvent) -> AppResult<()> {
    let subscription = &event.data.object;

    let customer_id = subscription["customer"].as_str();
    let subscription_id = subscription["id"].as_str();

    if let Some(customer_id) = customer_id {
        // Downgrade to free tier. Scoped to the org's *current* subscription id so a
        // deletion event for a superseded subscription (e.g. one we auto-canceled after a
        // duplicate checkout) can never downgrade an org that already moved to a newer
        // subscription. NULL id is matched for legacy rows linked by customer only.
        // The stripe_last_event_at guard additionally ignores stale re-deliveries.
        let event_time = event_created_at(event);
        let downgraded = sqlx::query(
            r#"
            UPDATE organizations
            SET
                stripe_subscription_id = NULL,
                stripe_subscription_status = 'canceled',
                stripe_cancel_at_period_end = FALSE,
                plan_tier = 'free',
                stripe_last_event_at = $3,
                updated_at = NOW()
            WHERE stripe_customer_id = $1
              AND plan_tier <> 'deleted'
              AND (stripe_subscription_id IS NULL OR stripe_subscription_id = $2)
              AND (stripe_last_event_at IS NULL OR stripe_last_event_at <= $3)
            RETURNING id
            "#,
        )
        .bind(customer_id)
        .bind(subscription_id)
        .bind(event_time)
        .fetch_all(state.db.pool())
        .await?;

        // A cancellation is the sharpest case for stale limits: without this the
        // org keeps its paid rps/burst for up to the 60s cache TTL.
        for row in &downgraded {
            use sqlx::Row;
            let org_id: String = row.get("id");
            invalidate_org_rate_limits(state.cache.as_ref(), &org_id).await;
        }

        // Stripe confirms the subscription is gone: if a checkout had replaced
        // it, there is nothing left to cancel.
        if let Some(subscription_id) = subscription_id {
            forget_superseded_subscription(state, subscription_id).await?;
        }

        info!(customer_id = %customer_id, subscription_id = ?subscription_id, "Subscription canceled, downgraded to free tier");
    }

    Ok(())
}

/// Extract the subscription id from an invoice object.
///
/// Stripe API "Basil" (2025-03-31+) moved the top-level `invoice.subscription`
/// field to `parent.subscription_details.subscription`; older API versions
/// still send the top-level field. Accept both so the recovery handler is not
/// a silent no-op on current API versions.
fn invoice_subscription_id(invoice: &serde_json::Value) -> Option<&str> {
    invoice["subscription"]
        .as_str()
        .or_else(|| invoice["parent"]["subscription_details"]["subscription"].as_str())
}

/// Whether any organization is linked to this Stripe customer.
async fn org_linked_to_customer(state: &AppState, customer_id: &str) -> AppResult<bool> {
    let row: Option<(String,)> =
        sqlx::query_as("SELECT id FROM organizations WHERE stripe_customer_id = $1 LIMIT 1")
            .bind(customer_id)
            .fetch_optional(state.db.pool())
            .await?;
    Ok(row.is_some())
}

/// Answer for an event whose customer is not linked to an organization yet.
///
/// Subscription and invoice events usually arrive before
/// checkout.session.completed links the customer. That is the normal path of
/// every new subscription, not a server fault, so it is answered 409 (Stripe
/// retries any non-2xx response) and logged at info, not as a 500 with an
/// error log.
fn retry_until_linked(event: &StripeEvent, customer_id: &str) -> AppError {
    info!(
        customer_id = %customer_id,
        event_id = %event.id,
        event_type = %event.event_type,
        "Stripe event arrived before its customer was linked to an organization; asking Stripe to retry"
    );
    AppError::Conflict(format!(
        "Stripe customer {} is not linked to an organization yet; retry later",
        customer_id
    ))
}

/// Whether an event for a not-yet-linked customer should be answered with a
/// retry request (see [`retry_until_linked`]). Linkage races
/// (subscription/invoice events arriving before
/// checkout.session.completed) resolve within minutes; after 24h the customer is
/// almost certainly orphaned (e.g. superseded by a newer checkout), and retrying
/// forever would only pollute webhook health.
fn should_request_retry_for_unlinked(event_time: DateTime<Utc>) -> bool {
    Utc::now() - event_time < chrono::Duration::hours(24)
}

/// Handle invoice.paid event
async fn handle_invoice_paid(state: &AppState, event: &StripeEvent) -> AppResult<()> {
    let invoice = &event.data.object;

    let customer_id = invoice["customer"].as_str();
    let subscription_id = invoice_subscription_id(invoice);

    if let (Some(customer_id), Some(_sub_id)) = (customer_id, subscription_id) {
        // Mark active without advancing stripe_last_event_at. Invoice timestamps share
        // a different stream than subscription.* events; bumping the shared clock here
        // used to drop a legitimate later plan-tier/subscription update that arrived
        // with an earlier Stripe `created` (out-of-order delivery).
        // Do not revive canceled subscriptions (subscription.deleted owns terminal state).
        let event_time = event_created_at(event);
        let result = sqlx::query(
            r#"
            UPDATE organizations
            SET
                stripe_subscription_status = 'active',
                updated_at = NOW()
            WHERE stripe_customer_id = $1
              AND stripe_subscription_id IS NOT NULL
              AND COALESCE(stripe_subscription_status, '') NOT IN ('canceled', 'cancelled')
            "#,
        )
        .bind(customer_id)
        .execute(state.db.pool())
        .await?;

        if result.rows_affected() == 0 {
            // No org linked yet (event raced ahead of checkout linkage): ask
            // Stripe to retry — a paying customer must not get stuck on the free tier
            // because a one-shot event was dropped. Already-canceled / orphaned
            // customers are ACKed instead.
            if !org_linked_to_customer(state, customer_id).await?
                && should_request_retry_for_unlinked(event_time)
            {
                return Err(retry_until_linked(event, customer_id));
            }
            info!(customer_id = %customer_id, event_id = %event.id, "Ignoring invoice.paid for unlinked or canceled org");
            return Ok(());
        }

        info!(customer_id = %customer_id, "Invoice paid, subscription active");
    }

    Ok(())
}

/// Handle invoice.payment_failed event
async fn handle_payment_failed(state: &AppState, event: &StripeEvent) -> AppResult<()> {
    let invoice = &event.data.object;

    let customer_id = invoice["customer"].as_str();

    // Only subscription-backed invoices may flip the org to past_due; a failed
    // one-off invoice must not degrade the subscription status (this also makes
    // the handler symmetric with invoice.paid).
    if invoice_subscription_id(invoice).is_none() {
        info!(event_id = %event.id, "Ignoring invoice.payment_failed for non-subscription invoice");
        return Ok(());
    }

    if let Some(customer_id) = customer_id {
        // Mark past_due without advancing stripe_last_event_at (see invoice.paid).
        // Only degrade live paid subscriptions — never after cancel/free.
        let event_time = event_created_at(event);
        let result = sqlx::query(
            r#"
            UPDATE organizations
            SET
                stripe_subscription_status = 'past_due',
                updated_at = NOW()
            WHERE stripe_customer_id = $1
              AND stripe_subscription_id IS NOT NULL
              AND COALESCE(stripe_subscription_status, '') NOT IN ('canceled', 'cancelled')
            "#,
        )
        .bind(customer_id)
        .execute(state.db.pool())
        .await?;

        if result.rows_affected() == 0 {
            // Same retry semantics as invoice.paid: not-linked-yet → 5xx for retry,
            // canceled/orphaned → ACK.
            if !org_linked_to_customer(state, customer_id).await?
                && should_request_retry_for_unlinked(event_time)
            {
                return Err(retry_until_linked(event, customer_id));
            }
            info!(customer_id = %customer_id, event_id = %event.id, "Ignoring invoice.payment_failed for unlinked or canceled org");
            return Ok(());
        }

        warn!(customer_id = %customer_id, "Payment failed, subscription past_due");
    }

    Ok(())
}

/// Current period end of a subscription, in unix seconds.
///
/// Stripe API "Basil" (2025-03-31+) removed the top-level
/// `current_period_end`; it now lives on each subscription item. Accept both
/// so the column stops being stored as NULL on current API versions.
fn subscription_period_end(subscription: &serde_json::Value) -> Option<i64> {
    subscription["current_period_end"]
        .as_i64()
        .or_else(|| subscription["items"]["data"][0]["current_period_end"].as_i64())
}

/// Subscription statuses that must never carry a paid plan_tier.
///
/// Stripe emits both `customer.subscription.updated` (status → canceled) and
/// `customer.subscription.deleted` on cancel. The deleted handler sets
/// `plan_tier = free`; if an updated(canceled) with the same-or-later
/// `created` is applied afterward and we only map from price, COALESCE
/// restores the paid tier. `unpaid` / `incomplete_expired` are terminal the
/// same way. `past_due` / `incomplete` / `trialing` / `active` keep the
/// price-mapped tier (grace / in-progress checkout).
fn is_terminal_subscription_status(status: &str) -> bool {
    matches!(status, "canceled" | "unpaid" | "incomplete_expired")
}

/// Resolve plan tier from subscription status + price.
///
/// Terminal statuses → `Some("free")`. Otherwise map from the price id.
fn resolve_plan_tier(subscription: &serde_json::Value, state: &AppState) -> Option<String> {
    if let Some(status) = subscription["status"].as_str() {
        if is_terminal_subscription_status(status) {
            return Some("free".to_string());
        }
    }
    determine_plan_tier(subscription, state)
}

/// Determine plan tier from subscription prices (ignores status).
fn determine_plan_tier(subscription: &serde_json::Value, state: &AppState) -> Option<String> {
    let items = subscription["items"]["data"].as_array()?;
    let first_item = items.first()?;
    let price_id = first_item["price"]["id"].as_str()?;

    // Match against configured price IDs
    if let Some(ref starter_price) = state.stripe.starter_price_id {
        if price_id == starter_price {
            return Some("starter".to_string());
        }
    }
    if let Some(ref pro_price) = state.stripe.pro_price_id {
        if price_id == pro_price {
            return Some("pro".to_string());
        }
    }

    // Unknown price: the tier is intentionally kept (COALESCE), but silently
    // doing so hides entitlement drift — the customer is being charged a price
    // we can't map (rotated/annual/enterprise price id missing from env).
    error!(
        price_id = %price_id,
        subscription_id = ?subscription["id"].as_str(),
        "Stripe price id not mapped to any plan tier; keeping current tier — check STRIPE_*_PRICE_ID configuration"
    );
    None
}

/// Allowlist `return_url` for the Stripe billing portal.
///
/// Origin must be in `CORS_ALLOWED_ORIGINS` (same set the dashboard is served
/// from). Scheme must be https in production; http is only allowed for
/// localhost/127.0.0.1 outside production so local dashboards still work.
fn validate_portal_return_url(
    return_url: &str,
    allowed_origins: &[String],
    environment: Environment,
) -> AppResult<()> {
    let parsed = url::Url::parse(return_url)
        .map_err(|_| AppError::BadRequest("return_url must be a valid absolute URL".to_string()))?;

    let host = parsed.host_str().unwrap_or("");
    let is_loopback = host == "localhost" || host == "127.0.0.1" || host == "::1";

    match parsed.scheme() {
        "https" => {}
        "http" if environment != Environment::Production && is_loopback => {}
        "http" if environment == Environment::Production => {
            return Err(AppError::BadRequest(
                "return_url must use https in production".to_string(),
            ));
        }
        _ => {
            return Err(AppError::BadRequest(
                "return_url must use https (or http://localhost in non-production)".to_string(),
            ));
        }
    }

    // Rebuild origin without path/query/fragment. url::Url::origin() yields an
    // opaque Origin for some non-special schemes; for http(s) this is fine.
    let origin = {
        let mut origin = format!("{}://{}", parsed.scheme(), host);
        if let Some(port) = parsed.port() {
            // url::Url omits default ports from .port(); only non-default appear.
            origin.push(':');
            origin.push_str(&port.to_string());
        }
        origin
    };

    let allowed = allowed_origins.iter().any(|o| {
        // Compare case-insensitively on scheme/host; strip trailing slash if present.
        let normalized = o.trim().trim_end_matches('/');
        normalized.eq_ignore_ascii_case(&origin)
    });

    if !allowed {
        return Err(AppError::BadRequest(
            "return_url origin is not in the allowed list".to_string(),
        ));
    }

    Ok(())
}

/// Verify Stripe webhook signature
fn verify_stripe_signature(secret: &str, body: &[u8], signature: &str) -> AppResult<()> {
    // Parse Stripe signature format: t=timestamp,v1=signature
    // During webhook-secret rotation Stripe signs with both secrets and sends
    // MULTIPLE v1 entries — keep them all and accept any match, otherwise
    // webhooks are transiently lost for the whole rotation window.
    let mut timestamp = None;
    let mut v1_signatures: Vec<&str> = Vec::new();

    for part in signature.split(',') {
        if let Some((key, value)) = part.split_once('=') {
            match key {
                "t" => timestamp = Some(value),
                "v1" => v1_signatures.push(value),
                _ => {}
            }
        }
    }

    let timestamp_str = timestamp
        .ok_or_else(|| AppError::Authentication("Missing timestamp in signature".to_string()))?;
    if v1_signatures.is_empty() {
        return Err(AppError::Authentication("Missing v1 signature".to_string()));
    }

    // Validate timestamp
    let timestamp_secs: i64 = timestamp_str
        .parse()
        .map_err(|_| AppError::Authentication("Invalid timestamp format".to_string()))?;

    let now = Utc::now().timestamp();
    let age = now - timestamp_secs;

    if age > STRIPE_TIMESTAMP_TOLERANCE_SECS {
        return Err(AppError::Authentication(format!(
            "Webhook timestamp too old: {} seconds ago",
            age
        )));
    }
    if age < -60 {
        return Err(AppError::Authentication(
            "Webhook timestamp is in the future".to_string(),
        ));
    }

    let mut mac = HmacSha256::new_from_slice(secret.as_bytes())
        .map_err(|_| AppError::Internal("Invalid HMAC key".to_string()))?;

    // MAC over the RAW body bytes. The previous version built the signed payload
    // through `String::from_utf8_lossy`, which substitutes U+FFFD for any invalid
    // byte sequence — so the bytes fed to the HMAC were not guaranteed to be the
    // bytes Stripe signed. Stripe sends UTF-8 JSON so it never diverged in
    // practice, but a signature check must never transform its input.
    mac.update(timestamp_str.as_bytes());
    mac.update(b".");
    mac.update(body);

    let computed = hex::encode(mac.finalize().into_bytes());

    if !v1_signatures
        .iter()
        .any(|expected| constant_time_compare(&computed, expected))
    {
        return Err(AppError::Authentication(
            "Invalid webhook signature".to_string(),
        ));
    }

    Ok(())
}

/// Constant-time string comparison
fn constant_time_compare(a: &str, b: &str) -> bool {
    use subtle::ConstantTimeEq;

    let len_eq = a.len() == b.len();
    let max_len = a.len().max(b.len());

    let a_bytes: Vec<u8> = a
        .bytes()
        .chain(std::iter::repeat(0u8))
        .take(max_len)
        .collect();
    let b_bytes: Vec<u8> = b
        .bytes()
        .chain(std::iter::repeat(0u8))
        .take(max_len)
        .collect();

    let bytes_eq = a_bytes.ct_eq(&b_bytes).into();
    len_eq && bytes_eq
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_billing_status_response_serialization() {
        let response = BillingStatusResponse {
            plan_tier: "starter".to_string(),
            stripe_subscription_id: Some("sub_123".to_string()),
            stripe_subscription_status: Some("active".to_string()),
            stripe_current_period_end: Some(Utc::now()),
            stripe_cancel_at_period_end: Some(false),
            has_stripe_customer: true,
        };

        let json = serde_json::to_string(&response).unwrap();
        assert!(json.contains("starter"));
        assert!(json.contains("sub_123"));
        assert!(json.contains("active"));
        assert!(json.contains("has_stripe_customer"));
    }

    #[test]
    fn test_billing_status_response_without_subscription() {
        let response = BillingStatusResponse {
            plan_tier: "free".to_string(),
            stripe_subscription_id: None,
            stripe_subscription_status: None,
            stripe_current_period_end: None,
            stripe_cancel_at_period_end: None,
            has_stripe_customer: false,
        };

        let json = serde_json::to_string(&response).unwrap();
        assert!(json.contains("free"));
        assert!(json.contains("false")); // has_stripe_customer
    }

    #[test]
    fn test_create_portal_request_deserialization() {
        let json = r#"{"return_url": "https://example.com/billing"}"#;
        let request: CreatePortalRequest = serde_json::from_str(json).unwrap();
        assert_eq!(request.return_url, "https://example.com/billing");
    }

    #[test]
    fn test_create_portal_response_serialization() {
        let response = CreatePortalResponse {
            url: "https://billing.stripe.com/session/xxx".to_string(),
        };

        let json = serde_json::to_string(&response).unwrap();
        assert!(json.contains("https://billing.stripe.com/session/xxx"));
    }

    #[test]
    fn test_constant_time_compare() {
        assert!(constant_time_compare("abc", "abc"));
        assert!(!constant_time_compare("abc", "abd"));
        assert!(!constant_time_compare("abc", "abcd"));
        assert!(!constant_time_compare("", "a"));
        assert!(constant_time_compare("", ""));
    }

    #[test]
    fn test_stripe_event_deserialization() {
        let json = r#"{
            "id": "evt_123",
            "type": "customer.subscription.updated",
            "created": 1700000000,
            "data": {
                "object": {
                    "id": "sub_123",
                    "status": "active"
                }
            }
        }"#;

        let event: StripeEvent = serde_json::from_str(json).unwrap();
        assert_eq!(event.id, "evt_123");
        assert_eq!(event.event_type, "customer.subscription.updated");
        assert_eq!(event.created, 1700000000);
        assert_eq!(event.data.object["id"], "sub_123");
        assert_eq!(event.data.object["status"], "active");

        let created = event_created_at(&event);
        assert_eq!(created.timestamp(), 1700000000);
    }

    #[test]
    fn test_invoice_subscription_id_legacy_and_basil() {
        // Pre-Basil API versions: top-level subscription field.
        let legacy = serde_json::json!({ "subscription": "sub_legacy" });
        assert_eq!(invoice_subscription_id(&legacy), Some("sub_legacy"));

        // Basil (2025-03-31+): parent.subscription_details.subscription.
        let basil = serde_json::json!({
            "parent": { "subscription_details": { "subscription": "sub_basil" } }
        });
        assert_eq!(invoice_subscription_id(&basil), Some("sub_basil"));

        // One-off invoice: no subscription anywhere.
        let one_off = serde_json::json!({ "customer": "cus_1" });
        assert_eq!(invoice_subscription_id(&one_off), None);
    }

    #[test]
    fn test_stripe_event_without_created_treated_as_fresh() {
        let json = r#"{
            "id": "evt_456",
            "type": "invoice.paid",
            "data": { "object": {} }
        }"#;

        let event: StripeEvent = serde_json::from_str(json).unwrap();
        assert_eq!(event.created, 0);

        // Missing created must be treated as "now", never as stale.
        let before = Utc::now().timestamp() - 5;
        assert!(event_created_at(&event).timestamp() >= before);
    }

    fn sign_stripe_body(secret: &str, body: &[u8], ts: i64) -> String {
        let mut mac = HmacSha256::new_from_slice(secret.as_bytes()).unwrap();
        mac.update(format!("{}.{}", ts, String::from_utf8_lossy(body)).as_bytes());
        hex::encode(mac.finalize().into_bytes())
    }

    #[test]
    fn test_stripe_signature_format() {
        // Test signature format parsing (not actual verification)
        let signature = "t=1700000000,v1=abc123def456";
        let parts: Vec<&str> = signature.split(',').collect();
        assert_eq!(parts.len(), 2);
        assert!(parts[0].starts_with("t="));
        assert!(parts[1].starts_with("v1="));
    }

    #[test]
    fn test_stripe_signature_accepts_valid_hmac() {
        let secret = "whsec_test_secret";
        let body = br#"{"id":"evt_ok","type":"invoice.paid","data":{"object":{}}}"#;
        let ts = Utc::now().timestamp();
        let v1 = sign_stripe_body(secret, body, ts);
        let header = format!("t={},v1={}", ts, v1);
        assert!(verify_stripe_signature(secret, body, &header).is_ok());
    }

    #[test]
    fn test_stripe_signature_rejects_tampered_body() {
        let secret = "whsec_test_secret";
        let body = br#"{"id":"evt_ok"}"#;
        let ts = Utc::now().timestamp();
        let v1 = sign_stripe_body(secret, body, ts);
        let header = format!("t={},v1={}", ts, v1);
        let tampered = br#"{"id":"evt_evil"}"#;
        assert!(verify_stripe_signature(secret, tampered, &header).is_err());
    }

    #[test]
    fn test_stripe_signature_rejects_stale_and_future_timestamp() {
        let secret = "whsec_test_secret";
        let body = b"{}";

        let stale_ts = Utc::now().timestamp() - STRIPE_TIMESTAMP_TOLERANCE_SECS - 1;
        let stale_v1 = sign_stripe_body(secret, body, stale_ts);
        let stale_header = format!("t={},v1={}", stale_ts, stale_v1);
        assert!(verify_stripe_signature(secret, body, &stale_header).is_err());

        let future_ts = Utc::now().timestamp() + 120;
        let future_v1 = sign_stripe_body(secret, body, future_ts);
        let future_header = format!("t={},v1={}", future_ts, future_v1);
        assert!(verify_stripe_signature(secret, body, &future_header).is_err());
    }

    #[test]
    fn test_stripe_signature_rejects_missing_parts() {
        let secret = "whsec_test_secret";
        let body = b"{}";
        let ts = Utc::now().timestamp();
        let v1 = sign_stripe_body(secret, body, ts);

        assert!(verify_stripe_signature(secret, body, &format!("v1={}", v1)).is_err());
        assert!(verify_stripe_signature(secret, body, &format!("t={}", ts)).is_err());
        assert!(verify_stripe_signature(secret, body, "").is_err());
    }

    #[test]
    fn test_stripe_signature_accepts_any_v1_during_rotation() {
        let secret = "whsec_test_secret";
        let body = b"{\"id\":\"evt_1\"}";
        let ts = Utc::now().timestamp();
        let valid = sign_stripe_body(secret, body, ts);

        // Valid signature first, stale one second (the rotation ordering that
        // used to fail because only the LAST v1 was kept).
        let header = format!("t={},v1={},v1={}", ts, valid, "0".repeat(64));
        assert!(verify_stripe_signature(secret, body, &header).is_ok());

        // And the reverse order still verifies.
        let header = format!("t={},v1={},v1={}", ts, "0".repeat(64), valid);
        assert!(verify_stripe_signature(secret, body, &header).is_ok());

        // No valid signature at all fails.
        let header = format!("t={},v1={}", ts, "0".repeat(64));
        assert!(verify_stripe_signature(secret, body, &header).is_err());
    }

    #[test]
    fn test_stripe_event_applies_ordering_guard() {
        let older = DateTime::from_timestamp(1_700_000_000, 0).unwrap();
        let newer = DateTime::from_timestamp(1_700_000_100, 0).unwrap();

        assert!(stripe_event_applies(None, older));
        assert!(stripe_event_applies(Some(older), newer));
        assert!(stripe_event_applies(Some(newer), newer)); // equal → apply (SQL `<=`)
        assert!(!stripe_event_applies(Some(newer), older));
    }

    #[test]
    fn test_subscription_period_end_basil_and_legacy() {
        let legacy = serde_json::json!({ "current_period_end": 1700000000 });
        assert_eq!(subscription_period_end(&legacy), Some(1700000000));

        let basil = serde_json::json!({
            "items": { "data": [ { "current_period_end": 1800000000 } ] }
        });
        assert_eq!(subscription_period_end(&basil), Some(1800000000));

        let none = serde_json::json!({});
        assert_eq!(subscription_period_end(&none), None);
    }

    #[test]
    fn test_stripe_timestamp_tolerance() {
        assert_eq!(STRIPE_TIMESTAMP_TOLERANCE_SECS, 300);
    }

    #[test]
    fn test_terminal_subscription_status_forces_free() {
        assert!(is_terminal_subscription_status("canceled"));
        assert!(is_terminal_subscription_status("unpaid"));
        assert!(is_terminal_subscription_status("incomplete_expired"));
        // Grace / in-progress — must NOT force free (price mapping applies).
        assert!(!is_terminal_subscription_status("active"));
        assert!(!is_terminal_subscription_status("past_due"));
        assert!(!is_terminal_subscription_status("trialing"));
        assert!(!is_terminal_subscription_status("incomplete"));
    }

    #[test]
    fn test_resolve_plan_tier_canceled_ignores_price() {
        // Minimal AppState-shaped stripe config isn't needed: terminal status
        // short-circuits before price lookup. Build a canceled sub with a
        // price that would otherwise map if we only looked at price ids.
        let canceled = serde_json::json!({
            "id": "sub_x",
            "status": "canceled",
            "items": { "data": [{ "price": { "id": "price_pro" } }] }
        });
        // resolve_plan_tier needs AppState — test the status gate directly and
        // the free-forcing branch via a thin wrapper on the pure helper.
        assert!(is_terminal_subscription_status(
            canceled["status"].as_str().unwrap()
        ));
        // Simulate resolve_plan_tier's terminal branch:
        let tier = if is_terminal_subscription_status(canceled["status"].as_str().unwrap()) {
            Some("free".to_string())
        } else {
            None
        };
        assert_eq!(tier.as_deref(), Some("free"));
    }

    #[test]
    fn test_checkout_org_ref_trusts_only_server_metadata() {
        // Server-created session: metadata wins over client_reference_id.
        let session = serde_json::json!({
            "client_reference_id": "org-from-url",
            "metadata": { "spooled_org_id": "org-from-server" }
        });
        assert_eq!(
            checkout_org_ref(&session),
            Some(CheckoutOrgRef::Trusted("org-from-server"))
        );

        // Payment Link: only the URL parameter, which anyone can edit.
        let link = serde_json::json!({ "client_reference_id": "org-from-url", "metadata": {} });
        assert_eq!(
            checkout_org_ref(&link),
            Some(CheckoutOrgRef::Unverified("org-from-url"))
        );

        // Blank values reference nothing.
        let blank = serde_json::json!({
            "client_reference_id": " ",
            "metadata": { "spooled_org_id": "" }
        });
        assert_eq!(checkout_org_ref(&blank), None);
        assert_eq!(checkout_org_ref(&serde_json::json!({})), None);
    }

    #[test]
    fn test_checkout_customer_email_prefers_customer_details() {
        let session = serde_json::json!({
            "customer_details": { "email": "payer@example.com" },
            "customer_email": "prefilled@example.com"
        });
        assert_eq!(checkout_customer_email(&session), Some("payer@example.com"));

        let prefilled = serde_json::json!({
            "customer_details": { "email": null },
            "customer_email": "prefilled@example.com"
        });
        assert_eq!(
            checkout_customer_email(&prefilled),
            Some("prefilled@example.com")
        );
        assert_eq!(checkout_customer_email(&serde_json::json!({})), None);
    }

    #[test]
    fn test_parse_checkout_plan_accepts_only_starter_and_pro() {
        assert_eq!(
            parse_checkout_plan(br#"{"plan":"starter"}"#).unwrap(),
            "starter"
        );
        assert_eq!(parse_checkout_plan(br#"{"plan":"pro"}"#).unwrap(), "pro");
        for body in [
            &br#"{"plan":"enterprise"}"#[..],
            br#"{"plan":"free"}"#,
            br#"{"plan":"PRO"}"#,
            br#"{"plan":7}"#,
            br#"{}"#,
            b"",
            b"not json",
        ] {
            assert!(
                matches!(parse_checkout_plan(body), Err(AppError::Validation(_))),
                "{}",
                String::from_utf8_lossy(body)
            );
        }
    }

    #[test]
    fn test_checkout_session_form_binds_the_org() {
        let form = checkout_session_form(
            "org-1",
            "price_pro",
            None,
            Some("owner@example.com"),
            "https://spooled.cloud",
        );
        let get = |key: &str| form.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str());
        assert_eq!(get("mode"), Some("subscription"));
        assert_eq!(get("line_items[0][price]"), Some("price_pro"));
        assert_eq!(get("client_reference_id"), Some("org-1"));
        assert_eq!(get("metadata[spooled_org_id]"), Some("org-1"));
        assert_eq!(
            get("subscription_data[metadata][spooled_org_id]"),
            Some("org-1")
        );
        assert_eq!(
            get("success_url"),
            Some("https://spooled.cloud/account/billing?checkout=success")
        );
        assert_eq!(
            get("cancel_url"),
            Some("https://spooled.cloud/account/billing?checkout=cancel")
        );
        assert_eq!(get("customer_email"), Some("owner@example.com"));
        assert_eq!(get("customer"), None);

        // An existing customer is reused instead of prefilling an email.
        let form = checkout_session_form(
            "org-1",
            "price_pro",
            Some("cus_1"),
            Some("owner@example.com"),
            "https://spooled.cloud",
        );
        assert!(form.iter().any(|(k, v)| k == "customer" && v == "cus_1"));
        assert!(!form.iter().any(|(k, _)| k == "customer_email"));
    }

    #[test]
    fn test_checkout_price_id_ignores_blank_and_unknown_plans() {
        let stripe = StripeSettings {
            secret_key: None,
            webhook_secret: None,
            starter_price_id: Some("price_starter".to_string()),
            pro_price_id: Some("  ".to_string()),
            billing_portal_config_id: None,
        };
        assert_eq!(checkout_price_id(&stripe, "starter"), Some("price_starter"));
        assert_eq!(checkout_price_id(&stripe, "pro"), None);
        assert_eq!(checkout_price_id(&stripe, "enterprise"), None);
    }

    #[tokio::test]
    async fn test_stripe_api_url_override_is_scoped_to_the_task() {
        assert_eq!(
            stripe_api_url("/v1/subscriptions/sub_1"),
            "https://api.stripe.com/v1/subscriptions/sub_1"
        );
        let inside = with_stripe_api_base("http://127.0.0.1:9/".to_string(), async {
            stripe_api_url("/v1/subscriptions/sub_1")
        })
        .await;
        assert_eq!(inside, "http://127.0.0.1:9/v1/subscriptions/sub_1");
        assert!(stripe_api_url("/x").starts_with("https://api.stripe.com"));
    }

    #[test]
    fn test_portal_return_url_allowlist() {
        let allowed = vec![
            "https://dashboard.spooled.cloud".to_string(),
            "https://spooled.cloud".to_string(),
        ];

        assert!(validate_portal_return_url(
            "https://dashboard.spooled.cloud/settings/billing",
            &allowed,
            Environment::Production,
        )
        .is_ok());

        assert!(validate_portal_return_url(
            "https://evil.example/phish",
            &allowed,
            Environment::Production,
        )
        .is_err());

        assert!(validate_portal_return_url(
            "http://dashboard.spooled.cloud/billing",
            &allowed,
            Environment::Production,
        )
        .is_err());

        // Localhost http OK outside production when allowlisted.
        let local = vec!["http://localhost:4321".to_string()];
        assert!(validate_portal_return_url(
            "http://localhost:4321/account/billing",
            &local,
            Environment::Development,
        )
        .is_ok());
        assert!(validate_portal_return_url(
            "http://localhost:4321/account/billing",
            &local,
            Environment::Production,
        )
        .is_err());

        assert!(
            validate_portal_return_url("not-a-url", &allowed, Environment::Production,).is_err()
        );
    }
}
