#![cfg(feature = "docker-tests")]
//! Stripe billing webhook: signed replay, event-id claim, and ordering guard.
//!
//! Uses a fixture webhook secret only — never hits live Stripe.

mod common;

use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use chrono::{DateTime, Utc};
use hmac::{Hmac, KeyInit, Mac};
use sha2::Sha256;
use spooled_backend::api::handlers::billing::webhook;
use spooled_backend::api::AppState;
use spooled_backend::config::Settings;
use spooled_backend::db::Database;
use spooled_backend::observability::Metrics;

use common::TestDatabase;

const WEBHOOK_SECRET: &str = "whsec_test_fixture_secret";

type HmacSha256 = Hmac<Sha256>;

fn sign_header(body: &[u8]) -> String {
    let ts = Utc::now().timestamp();
    let mut mac = HmacSha256::new_from_slice(WEBHOOK_SECRET.as_bytes()).unwrap();
    mac.update(format!("{}.{}", ts, String::from_utf8_lossy(body)).as_bytes());
    let v1 = hex::encode(mac.finalize().into_bytes());
    format!("t={},v1={}", ts, v1)
}

fn signed_headers(body: &[u8]) -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert(
        "Stripe-Signature",
        HeaderValue::from_str(&sign_header(body)).unwrap(),
    );
    headers
}

fn billing_state(db: &TestDatabase) -> AppState {
    let mut settings = Settings::load_for_testing();
    settings.stripe.webhook_secret = Some(WEBHOOK_SECRET.to_string());
    AppState::new(
        Database::from_pool(Arc::clone(&db.pool)),
        None,
        Metrics::new(),
        settings,
    )
}

async fn seed_linked_org(pool: &sqlx::PgPool, org_id: &str, customer_id: &str) {
    sqlx::query(
        r#"
        INSERT INTO organizations (
            id, name, slug, plan_tier, billing_email,
            stripe_customer_id, stripe_subscription_id, stripe_subscription_status,
            created_at, updated_at
        ) VALUES (
            $1, $1, $1, 'starter', 'stripe-test@example.com',
            $2, 'sub_seed', 'active',
            NOW(), NOW()
        )
        "#,
    )
    .bind(org_id)
    .bind(customer_id)
    .execute(pool)
    .await
    .expect("seed org");
}

fn subscription_event(event_id: &str, created: i64, customer_id: &str, status: &str) -> Vec<u8> {
    serde_json::json!({
        "id": event_id,
        "type": "customer.subscription.updated",
        "created": created,
        "data": {
            "object": {
                "id": "sub_seed",
                "customer": customer_id,
                "status": status,
                "cancel_at_period_end": false,
                "items": { "data": [] }
            }
        }
    })
    .to_string()
    .into_bytes()
}

#[tokio::test]
async fn test_stripe_signed_replay_same_event_id_is_noop() {
    let db = TestDatabase::new().await;
    let state = billing_state(&db);
    let org_id = "org-stripe-replay";
    let customer_id = "cus_replay";
    seed_linked_org(db.pool(), org_id, customer_id).await;

    let body = subscription_event("evt_replay_1", 1_700_000_200, customer_id, "active");
    let headers = signed_headers(&body);

    let first = webhook(
        State(state.clone()),
        headers.clone(),
        Bytes::from(body.clone()),
    )
    .await
    .expect("first delivery");
    assert_eq!(first, StatusCode::OK);

    let claimed: (i64,) =
        sqlx::query_as("SELECT COUNT(*) FROM processed_stripe_events WHERE event_id = $1")
            .bind("evt_replay_1")
            .fetch_one(db.pool())
            .await
            .unwrap();
    assert_eq!(claimed.0, 1);

    let before: (Option<String>, Option<DateTime<Utc>>) = sqlx::query_as(
        "SELECT stripe_subscription_status, stripe_last_event_at FROM organizations WHERE id = $1",
    )
    .bind(org_id)
    .fetch_one(db.pool())
    .await
    .unwrap();

    // Valid signature + identical event id must ACK without a second claim row.
    let second = webhook(State(state), headers, Bytes::from(body))
        .await
        .expect("replay delivery");
    assert_eq!(second, StatusCode::OK);

    let claimed_again: (i64,) =
        sqlx::query_as("SELECT COUNT(*) FROM processed_stripe_events WHERE event_id = $1")
            .bind("evt_replay_1")
            .fetch_one(db.pool())
            .await
            .unwrap();
    assert_eq!(claimed_again.0, 1);

    let after: (Option<String>, Option<DateTime<Utc>>) = sqlx::query_as(
        "SELECT stripe_subscription_status, stripe_last_event_at FROM organizations WHERE id = $1",
    )
    .bind(org_id)
    .fetch_one(db.pool())
    .await
    .unwrap();
    assert_eq!(after, before);
}

#[tokio::test]
async fn test_stripe_older_event_rejected_by_ordering_guard() {
    let db = TestDatabase::new().await;
    let state = billing_state(&db);
    let org_id = "org-stripe-order";
    let customer_id = "cus_order";
    seed_linked_org(db.pool(), org_id, customer_id).await;

    let newer = subscription_event("evt_order_new", 1_700_000_500, customer_id, "active");
    let newer_headers = signed_headers(&newer);
    let newer_status = webhook(State(state.clone()), newer_headers, Bytes::from(newer))
        .await
        .expect("newer event");
    assert_eq!(newer_status, StatusCode::OK);

    let mid: (Option<String>, Option<DateTime<Utc>>) = sqlx::query_as(
        "SELECT stripe_subscription_status, stripe_last_event_at FROM organizations WHERE id = $1",
    )
    .bind(org_id)
    .fetch_one(db.pool())
    .await
    .unwrap();
    assert_eq!(mid.0.as_deref(), Some("active"));
    assert!(mid.1.is_some());

    // Older canceled event arrives after newer active — must not regress status.
    let older = subscription_event("evt_order_old", 1_700_000_100, customer_id, "canceled");
    let older_headers = signed_headers(&older);
    let older_status = webhook(State(state), older_headers, Bytes::from(older))
        .await
        .expect("older event ACK");
    assert_eq!(older_status, StatusCode::OK);

    let after: (Option<String>, String) = sqlx::query_as(
        "SELECT stripe_subscription_status, plan_tier FROM organizations WHERE id = $1",
    )
    .bind(org_id)
    .fetch_one(db.pool())
    .await
    .unwrap();
    assert_eq!(
        after.0.as_deref(),
        Some("active"),
        "stale canceled must not overwrite newer active"
    );
    assert_eq!(
        after.1, "starter",
        "plan_tier must not drop to free from stale cancel"
    );

    // Older event was still claimed (handled + success no-op) so Stripe won't retry forever.
    let claimed: (i64,) =
        sqlx::query_as("SELECT COUNT(*) FROM processed_stripe_events WHERE event_id = $1")
            .bind("evt_order_old")
            .fetch_one(db.pool())
            .await
            .unwrap();
    assert_eq!(claimed.0, 1);
}

#[tokio::test]
async fn test_stripe_handler_error_releases_event_claim() {
    let db = TestDatabase::new().await;
    let state = billing_state(&db);

    // No org linked to this customer; recent created → 5xx retry request + claim release.
    let body = subscription_event(
        "evt_release_1",
        Utc::now().timestamp(),
        "cus_unlinked_recent",
        "active",
    );
    let headers = signed_headers(&body);
    let err = webhook(
        State(state.clone()),
        headers.clone(),
        Bytes::from(body.clone()),
    )
    .await
    .expect_err("unlinked recent subscription should 5xx");
    let _ = err;

    let claimed: (i64,) =
        sqlx::query_as("SELECT COUNT(*) FROM processed_stripe_events WHERE event_id = $1")
            .bind("evt_release_1")
            .fetch_one(db.pool())
            .await
            .unwrap();
    assert_eq!(
        claimed.0, 0,
        "failed handler must release claim so Stripe can retry"
    );

    // After linking the org, the same event id must be claimable again.
    seed_linked_org(db.pool(), "org-stripe-release", "cus_unlinked_recent").await;
    let retry = webhook(State(state), headers, Bytes::from(body))
        .await
        .expect("retry after link");
    assert_eq!(retry, StatusCode::OK);
    let claimed_after: (i64,) =
        sqlx::query_as("SELECT COUNT(*) FROM processed_stripe_events WHERE event_id = $1")
            .bind("evt_release_1")
            .fetch_one(db.pool())
            .await
            .unwrap();
    assert_eq!(claimed_after.0, 1);
}

#[tokio::test]
async fn test_stripe_multi_v1_header_accepted_end_to_end() {
    let db = TestDatabase::new().await;
    let state = billing_state(&db);
    seed_linked_org(db.pool(), "org-stripe-multiv1", "cus_multiv1").await;

    let body = subscription_event("evt_multiv1", 1_700_000_300, "cus_multiv1", "active");
    let ts = Utc::now().timestamp();
    let mut mac = HmacSha256::new_from_slice(WEBHOOK_SECRET.as_bytes()).unwrap();
    mac.update(format!("{}.{}", ts, String::from_utf8_lossy(&body)).as_bytes());
    let valid = hex::encode(mac.finalize().into_bytes());
    // Stale rotation sibling first — must still verify (any matching v1).
    let header = format!("t={},v1={},v1={}", ts, "0".repeat(64), valid);

    let mut headers = HeaderMap::new();
    headers.insert("Stripe-Signature", HeaderValue::from_str(&header).unwrap());

    let status = webhook(State(state), headers, Bytes::from(body))
        .await
        .expect("multi-v1");
    assert_eq!(status, StatusCode::OK);
}
