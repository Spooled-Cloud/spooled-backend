#![cfg(feature = "docker-tests")]
//! SITE-01. Checkout refuses an operator-set paid plan that has no Stripe
//! subscription.

mod common;

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::Router;
use serde_json::{json, Value};
use tower::ServiceExt;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use spooled_backend::api::{router, AppState};
use spooled_backend::config::Settings;
use spooled_backend::db::Database;
use spooled_backend::observability::Metrics;

use common::TestDatabase;

struct Tenant {
    org_id: String,
    key: String,
}

async fn seed_tenant(pool: &sqlx::PgPool, plan: &str, subscription: Option<&str>) -> Tenant {
    let org_id = uuid::Uuid::new_v4().to_string();
    let slug = format!("bl-{}", &org_id[..8]);
    sqlx::query(
        "INSERT INTO organizations (id, name, slug, plan_tier, billing_email, billing_email_verified_at, \
         stripe_subscription_id, settings, created_at, updated_at) \
         VALUES ($1, $2, $2, $3, $4, NOW(), $5, '{}'::JSONB, NOW(), NOW())",
    )
    .bind(&org_id)
    .bind(&slug)
    .bind(plan)
    .bind(format!("{slug}@example.com"))
    .bind(subscription)
    .execute(pool)
    .await
    .expect("seed org");

    let key = format!("sp_test_{}", uuid::Uuid::new_v4().simple());
    let hash = bcrypt::hash(&key, 4).expect("hash key");
    sqlx::query(
        "INSERT INTO api_keys (id, organization_id, key_hash, key_prefix, lookup_hash, name, queues, is_active, created_at) \
         VALUES ($1, $2, $3, $4, $5, 'bl', ARRAY[]::TEXT[], TRUE, NOW())",
    )
    .bind(uuid::Uuid::new_v4().to_string())
    .bind(&org_id)
    .bind(&hash)
    .bind(key.chars().take(8).collect::<String>())
    .bind(spooled_backend::models::api_key_lookup_hash(&key))
    .execute(pool)
    .await
    .expect("seed key");

    Tenant { org_id, key }
}

fn app(db: &TestDatabase) -> Router {
    let mut settings = Settings::load_for_testing();
    settings.stripe.secret_key = Some("sk_test_r0128".to_string());
    settings.stripe.starter_price_id = Some("price_starter".to_string());
    settings.stripe.pro_price_id = Some("price_pro".to_string());
    router(AppState::new(
        Database::from_pool(Arc::clone(&db.pool)),
        None,
        Metrics::new(),
        settings,
    ))
}

async fn call(
    app: &Router,
    method: &str,
    uri: &str,
    headers: &[(&str, &str)],
    body: Option<Value>,
) -> (StatusCode, Value) {
    let mut req = Request::builder().method(method).uri(uri);
    for (name, value) in headers {
        req = req.header(*name, *value);
    }
    let req = match body {
        Some(b) => req
            .header("Content-Type", "application/json")
            .body(Body::from(b.to_string()))
            .unwrap(),
        None => req.body(Body::empty()).unwrap(),
    };
    let res = app.clone().oneshot(req).await.expect("request");
    let status = res.status();
    let bytes = axum::body::to_bytes(res.into_body(), 1 << 20)
        .await
        .unwrap();
    let json = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes)
            .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into()))
    };
    (status, json)
}

fn bearer(key: &str) -> String {
    format!("Bearer {key}")
}

/// SITE-01: a paid tier with no Stripe subscription cannot start checkout.
/// A free org still can.
#[tokio::test]
async fn site01_checkout_refuses_an_operator_set_paid_plan() {
    let db = TestDatabase::new().await;
    let app = app(&db);
    let stripe = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/checkout/sessions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "cs_r0128",
            "url": "https://checkout.example/cs_r0128"
        })))
        .mount(&stripe)
        .await;

    let paid = seed_tenant(db.pool(), "starter", None).await;
    let blank = seed_tenant(db.pool(), "pro", Some("   ")).await;
    let free = seed_tenant(db.pool(), "free", None).await;

    for (label, tenant, plan) in [("starter", &paid, "pro"), ("blank", &blank, "starter")] {
        let (status, body) = spooled_backend::api::handlers::billing::with_stripe_api_base(
            stripe.uri(),
            call(
                &app,
                "POST",
                "/api/v1/billing/checkout",
                &[("Authorization", &bearer(&tenant.key))],
                Some(json!({"plan": plan})),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT, "{label}: {body}");
        assert_eq!(body["code"], "CONFLICT");
        assert!(
            body["message"]
                .as_str()
                .unwrap_or_default()
                .contains("Contact support"),
            "{label}: {body}"
        );
    }
    assert!(
        stripe.received_requests().await.unwrap().is_empty(),
        "the refusal happens before Stripe"
    );

    let (status, body) = spooled_backend::api::handlers::billing::with_stripe_api_base(
        stripe.uri(),
        call(
            &app,
            "POST",
            "/api/v1/billing/checkout",
            &[("Authorization", &bearer(&free.key))],
            Some(json!({"plan": "starter"})),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["url"], "https://checkout.example/cs_r0128");
}
