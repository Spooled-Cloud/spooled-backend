//! Billing handlers for Stripe integration
//!
//! This module provides endpoints for:
//! - Getting billing status
//! - Creating billing portal sessions
//! - Handling Stripe billing webhooks

use axum::{
    body::Bytes,
    extract::{Extension, State},
    http::{HeaderMap, StatusCode},
    Json,
};
use chrono::{DateTime, Utc};
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use tracing::{error, info, warn};

use crate::api::AppState;
use crate::error::{AppError, AppResult};
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
    let stripe_secret = state
        .stripe
        .secret_key
        .as_ref()
        .ok_or_else(|| AppError::Internal("Stripe is not configured".to_string()))?;

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
    let client = reqwest::Client::new();

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
        .post("https://api.stripe.com/v1/billing_portal/sessions")
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
    let webhook_secret = state.stripe.webhook_secret.as_ref().ok_or_else(|| {
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
            | "customer.subscription.created"
            | "customer.subscription.updated"
            | "customer.subscription.deleted"
            | "invoice.paid"
            | "invoice.payment_failed"
    );

    // Idempotency: Stripe re-delivers events (retries for up to ~3 days) with the
    // same event id. Skip anything we've already applied successfully.
    if handled {
        let already_processed: Option<(String,)> =
            sqlx::query_as("SELECT event_id FROM processed_stripe_events WHERE event_id = $1")
                .bind(&event.id)
                .fetch_optional(state.db.pool())
                .await?;

        if already_processed.is_some() {
            info!(event_id = %event.id, event_type = %event.event_type, "Skipping already-processed Stripe event");
            return Ok(StatusCode::OK);
        }
    }

    // Handle different event types
    match event.event_type.as_str() {
        "checkout.session.completed" => {
            handle_checkout_completed(&state, &event).await?;
        }
        "customer.subscription.created" | "customer.subscription.updated" => {
            handle_subscription_updated(&state, &event).await?;
        }
        "customer.subscription.deleted" => {
            handle_subscription_deleted(&state, &event).await?;
        }
        "invoice.paid" => {
            handle_invoice_paid(&state, &event).await?;
        }
        "invoice.payment_failed" => {
            handle_payment_failed(&state, &event).await?;
        }
        _ => {
            info!(event_type = %event.event_type, "Ignoring unhandled Stripe event");
        }
    }

    // Record the event only after successful processing: a handler error must
    // return 5xx *without* marking the event processed, so Stripe's retry of the
    // same event id is applied rather than skipped as a duplicate.
    if handled {
        sqlx::query(
            "INSERT INTO processed_stripe_events (event_id) VALUES ($1) ON CONFLICT (event_id) DO NOTHING",
        )
        .bind(&event.id)
        .execute(state.db.pool())
        .await?;

        // Opportunistic pruning far beyond Stripe's retry window.
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

#[derive(Debug, Deserialize)]
struct StripeEventData {
    object: serde_json::Value,
}

/// Handle checkout.session.completed event
async fn handle_checkout_completed(state: &AppState, event: &StripeEvent) -> AppResult<()> {
    let session = &event.data.object;

    let customer_id = session["customer"].as_str();
    let subscription_id = session["subscription"].as_str();
    let client_reference_id = session["client_reference_id"].as_str(); // org_id

    if let (Some(customer_id), Some(org_id)) = (customer_id, client_reference_id) {
        // Capture the current linkage before overwriting it: a Payment Link checkout for an
        // already-subscribed org creates a brand-new customer + second subscription, and the
        // old subscription would keep billing while becoming unreachable from the portal.
        let previous: Option<(Option<String>, Option<String>)> = sqlx::query_as(
            "SELECT stripe_customer_id, stripe_subscription_id FROM organizations WHERE id = $1",
        )
        .bind(org_id)
        .fetch_optional(state.db.pool())
        .await?;

        // Update organization with Stripe customer ID. The stripe_last_event_at
        // guard rejects stale re-deliveries: an old checkout event from a previous
        // session must never relink the org (and trigger the cancel backstop below)
        // after a newer billing event has been applied.
        let event_time = event_created_at(event);
        let relinked = sqlx::query(
            r#"
            UPDATE organizations
            SET stripe_customer_id = $1,
                stripe_subscription_id = $2,
                stripe_last_event_at = $3,
                updated_at = NOW()
            WHERE id = $4
              AND (stripe_last_event_at IS NULL OR stripe_last_event_at <= $3)
            "#,
        )
        .bind(customer_id)
        .bind(subscription_id)
        .bind(event_time)
        .bind(org_id)
        .execute(state.db.pool())
        .await?;

        if relinked.rows_affected() == 0 {
            warn!(
                org_id = %org_id,
                customer_id = %customer_id,
                event_id = %event.id,
                "Skipping checkout.session.completed: org missing or a newer billing event was already applied"
            );
            return Ok(());
        }

        // Backstop against double billing: cancel the superseded subscription, if any.
        if let Some((prev_customer_id, Some(prev_subscription_id))) = previous {
            let is_superseded =
                subscription_id.is_some() && subscription_id != Some(prev_subscription_id.as_str());
            if is_superseded {
                error!(
                    org_id = %org_id,
                    prev_customer_id = ?prev_customer_id,
                    prev_subscription_id = %prev_subscription_id,
                    new_customer_id = %customer_id,
                    new_subscription_id = ?subscription_id,
                    "Checkout completed for an org that already had a subscription; canceling the superseded subscription"
                );
                if let Err(e) = cancel_stripe_subscription(state, &prev_subscription_id).await {
                    error!(
                        org_id = %org_id,
                        prev_subscription_id = %prev_subscription_id,
                        error = %e,
                        "Failed to cancel superseded subscription — manual cleanup required to stop double billing"
                    );
                }
            }
        }

        info!(org_id = %org_id, customer_id = %customer_id, "Linked Stripe customer to organization");

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
                // (re-linking is idempotent and the cancel backstop only fires when the
                // subscription id actually changes).
                warn!(
                    org_id = %org_id,
                    subscription_id = %subscription_id,
                    error = %e,
                    "Failed to reconcile org plan tier from subscription after checkout; requesting Stripe retry"
                );
                return Err(e);
            }
        }
    }

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

    // Determine plan tier from price
    let plan_tier = determine_plan_tier(subscription, state);

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
              AND (stripe_last_event_at IS NULL OR stripe_last_event_at <= $7)
            "#,
        )
        .bind(subscription_id)
        .bind(status)
        .bind(period_end)
        .bind(cancel_at_period_end)
        .bind(&plan_tier)
        .bind(customer_id)
        .bind(event_time)
        .execute(state.db.pool())
        .await?;

        if result.rows_affected() == 0 {
            // No org linked to this customer yet (event arrived before
            // checkout.session.completed): return 5xx so Stripe retries until the
            // linkage exists. Without the retry this event is dropped forever and a
            // paying customer can be stuck on the free tier. Orphaned customers
            // (superseded > 24h ago) are ACKed to stop pointless retries.
            if !org_linked_to_customer(state, customer_id).await?
                && should_request_retry_for_unlinked(event_time)
            {
                warn!(
                    customer_id = %customer_id,
                    subscription_id = ?subscription_id,
                    event_id = %event.id,
                    "Subscription event received before org linkage; requesting Stripe retry"
                );
                return Err(AppError::Internal(format!(
                    "subscription event received before org was linked to customer {}; requesting Stripe retry",
                    customer_id
                )));
            }

            // Org exists but the ordering guard rejected the event (stale re-delivery),
            // or the customer is orphaned: ACK.
            info!(
                customer_id = %customer_id,
                subscription_id = ?subscription_id,
                status = %status,
                plan_tier = ?plan_tier,
                event_id = %event.id,
                "Ignoring stale subscription event"
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

    let client = reqwest::Client::new();
    let url = format!(
        "https://api.stripe.com/v1/subscriptions/{}",
        subscription_id
    );

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
    let plan_tier = determine_plan_tier(&subscription, state);
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

    let client = reqwest::Client::new();
    let url = format!(
        "https://api.stripe.com/v1/subscriptions/{}",
        subscription_id
    );

    let response = client
        .delete(url)
        .header("Authorization", format!("Bearer {}", stripe_secret))
        .send()
        .await
        .map_err(|e| AppError::Internal(format!("Failed to cancel subscription: {}", e)))?;

    if !response.status().is_success() {
        let error_text = response.text().await.unwrap_or_default();
        return Err(AppError::Internal(format!(
            "Stripe subscription cancel failed: {}",
            error_text
        )));
    }

    info!(subscription_id = %subscription_id, "Canceled superseded Stripe subscription");
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
        sqlx::query(
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
              AND (stripe_subscription_id IS NULL OR stripe_subscription_id = $2)
              AND (stripe_last_event_at IS NULL OR stripe_last_event_at <= $3)
            "#,
        )
        .bind(customer_id)
        .bind(subscription_id)
        .bind(event_time)
        .execute(state.db.pool())
        .await?;

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

/// Whether an event for a not-yet-linked customer should be answered with 5xx so
/// Stripe retries it. Linkage races (subscription/invoice events arriving before
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
        // Ensure subscription is active. Stale re-deliveries are ignored via the
        // stripe_last_event_at guard.
        let event_time = event_created_at(event);
        let result = sqlx::query(
            r#"
            UPDATE organizations
            SET
                stripe_subscription_status = 'active',
                stripe_last_event_at = $2,
                updated_at = NOW()
            WHERE stripe_customer_id = $1
              AND (stripe_last_event_at IS NULL OR stripe_last_event_at <= $2)
            "#,
        )
        .bind(customer_id)
        .bind(event_time)
        .execute(state.db.pool())
        .await?;

        if result.rows_affected() == 0 {
            // No org linked yet (event raced ahead of checkout linkage): return 5xx so
            // Stripe retries — a paying customer must not get stuck on the free tier
            // because a one-shot event was dropped. A stale event (org linked but the
            // ordering guard rejected it) or an orphaned-customer event is ACKed instead.
            if !org_linked_to_customer(state, customer_id).await?
                && should_request_retry_for_unlinked(event_time)
            {
                return Err(AppError::Internal(format!(
                    "invoice.paid received before org was linked to customer {}; requesting Stripe retry",
                    customer_id
                )));
            }
            info!(customer_id = %customer_id, event_id = %event.id, "Ignoring stale or unlinked invoice.paid event");
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
        // Mark as past_due. Stale re-deliveries are ignored via the
        // stripe_last_event_at guard.
        let event_time = event_created_at(event);
        let result = sqlx::query(
            r#"
            UPDATE organizations
            SET
                stripe_subscription_status = 'past_due',
                stripe_last_event_at = $2,
                updated_at = NOW()
            WHERE stripe_customer_id = $1
              AND (stripe_last_event_at IS NULL OR stripe_last_event_at <= $2)
            "#,
        )
        .bind(customer_id)
        .bind(event_time)
        .execute(state.db.pool())
        .await?;

        if result.rows_affected() == 0 {
            // Same retry semantics as invoice.paid: not-linked-yet → 5xx for retry,
            // stale/orphaned → ACK.
            if !org_linked_to_customer(state, customer_id).await?
                && should_request_retry_for_unlinked(event_time)
            {
                return Err(AppError::Internal(format!(
                    "invoice.payment_failed received before org was linked to customer {}; requesting Stripe retry",
                    customer_id
                )));
            }
            info!(customer_id = %customer_id, event_id = %event.id, "Ignoring stale or unlinked invoice.payment_failed event");
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

/// Determine plan tier from subscription prices
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

    // Create signed payload
    let signed_payload = format!("{}.{}", timestamp_str, String::from_utf8_lossy(body));

    let mut mac = HmacSha256::new_from_slice(secret.as_bytes())
        .map_err(|_| AppError::Internal("Invalid HMAC key".to_string()))?;
    mac.update(signed_payload.as_bytes());

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
    fn test_stripe_signature_accepts_any_v1_during_rotation() {
        use hmac::Mac;

        let secret = "whsec_test_secret";
        let body = b"{\"id\":\"evt_1\"}";
        let ts = Utc::now().timestamp().to_string();

        let mut mac = HmacSha256::new_from_slice(secret.as_bytes()).unwrap();
        mac.update(format!("{}.{}", ts, String::from_utf8_lossy(body)).as_bytes());
        let valid = hex::encode(mac.finalize().into_bytes());

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
}
