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
    let org: Option<(
        String,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<DateTime<Utc>>,
        Option<bool>,
    )> = sqlx::query_as(
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
    let stripe_secret = state.stripe.secret_key.as_ref().ok_or_else(|| {
        AppError::Internal("Stripe is not configured".to_string())
    })?;

    // Get customer ID
    let customer_id: Option<(Option<String>,)> = sqlx::query_as(
        "SELECT stripe_customer_id FROM organizations WHERE id = $1",
    )
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
    if let Some(ref config_id) = state.stripe.billing_portal_config_id {
        form_params.push(("configuration", config_id.as_str()));
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

    let portal: StripePortalSession = response.json().await.map_err(|e| {
        AppError::Internal(format!("Failed to parse Stripe response: {}", e))
    })?;

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
        error!("Stripe billing webhook received but STRIPE_BILLING_WEBHOOK_SECRET is not configured");
        AppError::Internal("Webhook signature verification not configured".to_string())
    })?;

    verify_stripe_signature(webhook_secret, &body, signature)?;

    // Parse the event
    let event: StripeEvent = serde_json::from_slice(&body)
        .map_err(|e| AppError::BadRequest(format!("Invalid JSON: {}", e)))?;

    info!(event_type = %event.event_type, event_id = %event.id, "Processing Stripe billing webhook");

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

    Ok(StatusCode::OK)
}

#[derive(Debug, Deserialize)]
struct StripeEvent {
    id: String,
    #[serde(rename = "type")]
    event_type: String,
    data: StripeEventData,
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
        // Update organization with Stripe customer ID
        sqlx::query(
            r#"
            UPDATE organizations 
            SET stripe_customer_id = $1, 
                stripe_subscription_id = $2,
                updated_at = NOW()
            WHERE id = $3
            "#,
        )
        .bind(customer_id)
        .bind(subscription_id)
        .bind(org_id)
        .execute(state.db.pool())
        .await?;

        info!(org_id = %org_id, customer_id = %customer_id, "Linked Stripe customer to organization");
    }

    Ok(())
}

/// Handle subscription created/updated events
async fn handle_subscription_updated(state: &AppState, event: &StripeEvent) -> AppResult<()> {
    let subscription = &event.data.object;
    
    let subscription_id = subscription["id"].as_str();
    let customer_id = subscription["customer"].as_str();
    let status = subscription["status"].as_str();
    let current_period_end = subscription["current_period_end"].as_i64();
    let cancel_at_period_end = subscription["cancel_at_period_end"].as_bool();
    
    // Determine plan tier from price
    let plan_tier = determine_plan_tier(subscription, state);
    
    if let (Some(customer_id), Some(status)) = (customer_id, status) {
        let period_end = current_period_end
            .and_then(|ts| DateTime::from_timestamp(ts, 0));

        // Update organization
        sqlx::query(
            r#"
            UPDATE organizations 
            SET 
                stripe_subscription_id = $1,
                stripe_subscription_status = $2,
                stripe_current_period_end = $3,
                stripe_cancel_at_period_end = $4,
                plan_tier = COALESCE($5, plan_tier),
                updated_at = NOW()
            WHERE stripe_customer_id = $6
            "#,
        )
        .bind(subscription_id)
        .bind(status)
        .bind(period_end)
        .bind(cancel_at_period_end)
        .bind(&plan_tier)
        .bind(customer_id)
        .execute(state.db.pool())
        .await?;

        info!(
            customer_id = %customer_id,
            status = %status,
            plan_tier = ?plan_tier,
            "Updated subscription status"
        );
    }

    Ok(())
}

/// Handle subscription deleted event
async fn handle_subscription_deleted(state: &AppState, event: &StripeEvent) -> AppResult<()> {
    let subscription = &event.data.object;
    
    let customer_id = subscription["customer"].as_str();
    
    if let Some(customer_id) = customer_id {
        // Downgrade to free tier
        sqlx::query(
            r#"
            UPDATE organizations 
            SET 
                stripe_subscription_id = NULL,
                stripe_subscription_status = 'canceled',
                stripe_cancel_at_period_end = FALSE,
                plan_tier = 'free',
                updated_at = NOW()
            WHERE stripe_customer_id = $1
            "#,
        )
        .bind(customer_id)
        .execute(state.db.pool())
        .await?;

        info!(customer_id = %customer_id, "Subscription canceled, downgraded to free tier");
    }

    Ok(())
}

/// Handle invoice.paid event
async fn handle_invoice_paid(state: &AppState, event: &StripeEvent) -> AppResult<()> {
    let invoice = &event.data.object;
    
    let customer_id = invoice["customer"].as_str();
    let subscription_id = invoice["subscription"].as_str();
    
    if let (Some(customer_id), Some(_sub_id)) = (customer_id, subscription_id) {
        // Ensure subscription is active
        sqlx::query(
            r#"
            UPDATE organizations 
            SET 
                stripe_subscription_status = 'active',
                updated_at = NOW()
            WHERE stripe_customer_id = $1
            "#,
        )
        .bind(customer_id)
        .execute(state.db.pool())
        .await?;

        info!(customer_id = %customer_id, "Invoice paid, subscription active");
    }

    Ok(())
}

/// Handle invoice.payment_failed event
async fn handle_payment_failed(state: &AppState, event: &StripeEvent) -> AppResult<()> {
    let invoice = &event.data.object;
    
    let customer_id = invoice["customer"].as_str();
    
    if let Some(customer_id) = customer_id {
        // Mark as past_due
        sqlx::query(
            r#"
            UPDATE organizations 
            SET 
                stripe_subscription_status = 'past_due',
                updated_at = NOW()
            WHERE stripe_customer_id = $1
            "#,
        )
        .bind(customer_id)
        .execute(state.db.pool())
        .await?;

        warn!(customer_id = %customer_id, "Payment failed, subscription past_due");
    }

    Ok(())
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
    
    // Default: keep current plan
    None
}

/// Verify Stripe webhook signature
fn verify_stripe_signature(secret: &str, body: &[u8], signature: &str) -> AppResult<()> {
    // Parse Stripe signature format: t=timestamp,v1=signature
    let mut timestamp = None;
    let mut v1_signature = None;

    for part in signature.split(',') {
        if let Some((key, value)) = part.split_once('=') {
            match key {
                "t" => timestamp = Some(value),
                "v1" => v1_signature = Some(value),
                _ => {}
            }
        }
    }

    let timestamp_str = timestamp
        .ok_or_else(|| AppError::Authentication("Missing timestamp in signature".to_string()))?;
    let expected =
        v1_signature.ok_or_else(|| AppError::Authentication("Missing v1 signature".to_string()))?;

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

    if !constant_time_compare(&computed, expected) {
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
    
    let a_bytes: Vec<u8> = a.bytes().chain(std::iter::repeat(0u8)).take(max_len).collect();
    let b_bytes: Vec<u8> = b.bytes().chain(std::iter::repeat(0u8)).take(max_len).collect();

    let bytes_eq = a_bytes.ct_eq(&b_bytes).into();
    len_eq && bytes_eq
}
