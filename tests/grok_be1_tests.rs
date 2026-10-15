#![cfg(feature = "docker-tests")]
//! Regressions for the verified BILL-01..06 and AUTH-01/02/04/05 findings.
//! Stripe is only ever a local wiremock.

mod common;

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::Router;
use serde_json::{json, Value};
use tower::ServiceExt;

use spooled_backend::api::handlers::auth::Claims;
use spooled_backend::api::{router, AppState};
use spooled_backend::cache::RedisCache;
use spooled_backend::config::{QueueSettings, RedisSettings, Settings, WorkerSettings};
use spooled_backend::db::Database;
use spooled_backend::observability::Metrics;

use common::{TestDatabase, TestRedis};

struct Tenant {
    org_id: String,
    key: String,
}

async fn seed_tenant(pool: &sqlx::PgPool) -> Tenant {
    let org_id = uuid::Uuid::new_v4().to_string();
    let slug = format!("g1-{}", &org_id[..8]);
    sqlx::query(
        "INSERT INTO organizations (id, name, slug, plan_tier, billing_email, billing_email_verified_at, settings, created_at, updated_at) \
         VALUES ($1, $2, $2, 'starter', $3, NOW(), '{}'::JSONB, NOW(), NOW())",
    )
    .bind(&org_id)
    .bind(&slug)
    .bind(format!("{slug}@example.com"))
    .execute(pool)
    .await
    .expect("seed org");

    let key = format!("sp_test_{}", uuid::Uuid::new_v4().simple());
    let hash = bcrypt::hash(&key, 4).expect("hash key");
    sqlx::query(
        "INSERT INTO api_keys (id, organization_id, key_hash, key_prefix, lookup_hash, name, queues, is_active, created_at) \
         VALUES ($1, $2, $3, $4, $5, 'g1', ARRAY[]::TEXT[], TRUE, NOW())",
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

fn app_with(db: &TestDatabase, cache: Option<RedisCache>, settings: Settings) -> Router {
    router(AppState::new(
        Database::from_pool(Arc::clone(&db.pool)),
        cache,
        Metrics::new(),
        settings,
    ))
}

fn app(db: &TestDatabase) -> Router {
    app_with(db, None, Settings::load_for_testing())
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

const WEBHOOK_SECRET: &str = "whsec_grok_be1_fixture";
const STRIPE_KEY: &str = "sk_test_grok_be1_fixture";
const PRO_PRICE: &str = "price_grok_be1_pro";

fn stripe_settings() -> Settings {
    let mut settings = Settings::load_for_testing();
    settings.stripe.secret_key = Some(STRIPE_KEY.to_string());
    settings.stripe.webhook_secret = Some(WEBHOOK_SECRET.to_string());
    settings.stripe.pro_price_id = Some(PRO_PRICE.to_string());
    settings
}

async fn deliver(
    app: &Router,
    stripe: &wiremock::MockServer,
    event: &Value,
) -> (StatusCode, Value) {
    use hmac::{Hmac, KeyInit, Mac};

    let ts = chrono::Utc::now().timestamp();
    let mut mac = Hmac::<sha2::Sha256>::new_from_slice(WEBHOOK_SECRET.as_bytes()).unwrap();
    mac.update(format!("{ts}.{event}").as_bytes());
    let signature = format!("t={ts},v1={}", hex::encode(mac.finalize().into_bytes()));
    spooled_backend::api::handlers::billing::with_stripe_api_base(
        stripe.uri(),
        call(
            app,
            "POST",
            "/api/v1/billing/webhook",
            &[("Stripe-Signature", &signature)],
            Some(event.clone()),
        ),
    )
    .await
}

fn stripe_event(id: &str, event_type: &str, created: i64, object: Value) -> Value {
    json!({ "id": id, "type": event_type, "created": created, "data": { "object": object } })
}

fn subscription(id: &str, customer: &str, price: &str, status: &str) -> Value {
    json!({
        "id": id,
        "object": "subscription",
        "customer": customer,
        "status": status,
        "cancel_at_period_end": false,
        "items": { "data": [{ "price": { "id": price } }] }
    })
}

#[derive(Debug, sqlx::FromRow)]
struct BillingRow {
    plan_tier: String,
    stripe_subscription_id: Option<String>,
    stripe_subscription_status: Option<String>,
    stripe_last_event_at: Option<chrono::DateTime<chrono::Utc>>,
}

async fn billing_row(pool: &sqlx::PgPool, org_id: &str) -> BillingRow {
    sqlx::query_as(
        "SELECT plan_tier, stripe_subscription_id, stripe_subscription_status, stripe_last_event_at \
         FROM organizations WHERE id = $1",
    )
    .bind(org_id)
    .fetch_one(pool)
    .await
    .expect("org billing")
}

async fn redis_cache() -> (TestRedis, RedisCache) {
    let redis = TestRedis::new().await;
    let cache = RedisCache::connect(&RedisSettings {
        url: redis.url.clone(),
        pool_size: 4,
    })
    .await
    .expect("redis");
    (redis, cache)
}

fn mint_token(claims: &Claims) -> String {
    jsonwebtoken::encode(
        &jsonwebtoken::Header::default(),
        claims,
        &jsonwebtoken::EncodingKey::from_secret(b"test-secret"),
    )
    .expect("mint jwt")
}

fn access_claims(org_id: &str, api_key_id: &str, token_type: &str) -> Claims {
    let now = chrono::Utc::now();
    Claims {
        sub: org_id.to_string(),
        api_key_id: api_key_id.to_string(),
        org_id: org_id.to_string(),
        iat: now.timestamp(),
        exp: (now + chrono::Duration::hours(1)).timestamp(),
        nbf: now.timestamp(),
        jti: uuid::Uuid::new_v4().to_string(),
        queues: vec!["*".to_string()],
        token_type: token_type.to_string(),
        sid: Some(uuid::Uuid::new_v4().to_string()),
    }
}

// BILL-01: a same-second canceled update must not restore the subscription id,
// an unpaid incomplete resubscribe must not grant the paid tier, and a failed
// delayed payment clears that incomplete link.
#[tokio::test]
async fn bill01_canceled_subscription_update_does_not_restore_id_or_grant_unpaid_plan() {
    let db = TestDatabase::new().await;
    let tenant = seed_tenant(db.pool()).await;
    let customer = "cus_bill01";
    let old_sub = "sub_bill01_old";
    let new_sub = "sub_bill01_new";
    sqlx::query(
        "UPDATE organizations SET plan_tier = 'pro', stripe_customer_id = $2, \
         stripe_subscription_id = $3, stripe_subscription_status = 'active' WHERE id = $1",
    )
    .bind(&tenant.org_id)
    .bind(customer)
    .bind(old_sub)
    .execute(db.pool())
    .await
    .unwrap();

    let app = app_with(&db, None, stripe_settings());
    let stripe = wiremock::MockServer::start().await;
    let t = 1_700_000_000_i64;

    let (status, body) = deliver(
        &app,
        &stripe,
        &stripe_event(
            "evt_bill01_deleted",
            "customer.subscription.deleted",
            t,
            subscription(old_sub, customer, PRO_PRICE, "canceled"),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let row = billing_row(db.pool(), &tenant.org_id).await;
    assert!(row.stripe_subscription_id.is_none(), "{row:?}");
    assert_eq!(row.plan_tier, "free");

    let (status, body) = deliver(
        &app,
        &stripe,
        &stripe_event(
            "evt_bill01_canceled",
            "customer.subscription.updated",
            t,
            subscription(old_sub, customer, PRO_PRICE, "canceled"),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let row = billing_row(db.pool(), &tenant.org_id).await;
    assert!(
        row.stripe_subscription_id.is_none(),
        "canceled update restored the subscription id: {row:?}"
    );
    assert_eq!(row.plan_tier, "free", "{row:?}");

    let (status, body) = deliver(
        &app,
        &stripe,
        &stripe_event(
            "evt_bill01_incomplete",
            "customer.subscription.updated",
            t + 1,
            subscription(new_sub, customer, PRO_PRICE, "incomplete"),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let row = billing_row(db.pool(), &tenant.org_id).await;
    assert_eq!(
        row.stripe_subscription_id.as_deref(),
        Some(new_sub),
        "incomplete event was not applied: {row:?}"
    );
    assert_eq!(
        row.stripe_subscription_status.as_deref(),
        Some("incomplete")
    );
    assert_eq!(
        row.plan_tier, "free",
        "unpaid subscription granted a plan: {row:?}"
    );

    let (status, body) = deliver(
        &app,
        &stripe,
        &stripe_event(
            "evt_bill01_failed",
            "checkout.session.async_payment_failed",
            t + 2,
            json!({ "id": "cs_bill01", "subscription": new_sub, "customer": customer }),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let row = billing_row(db.pool(), &tenant.org_id).await;
    assert!(
        row.stripe_subscription_id.is_none(),
        "failed payment left the unpaid subscription linked: {row:?}"
    );
    assert_eq!(row.plan_tier, "free");
}

// BILL-02: deleting an org cancels superseded subscriptions too. A failed
// cancel leaves the org in place.
#[tokio::test]
async fn bill02_org_delete_cancels_superseded_subscriptions() {
    use wiremock::matchers::{method, path};

    let db = TestDatabase::new().await;
    let ok = seed_tenant(db.pool()).await;
    let bad = seed_tenant(db.pool()).await;
    sqlx::query(
        "UPDATE organizations SET stripe_customer_id = $2, stripe_subscription_id = $3, \
         stripe_subscription_status = 'active', \
         stripe_superseded_subscription_ids = ARRAY[$4]::TEXT[] WHERE id = $1",
    )
    .bind(&ok.org_id)
    .bind("cus_bill02_ok")
    .bind("sub_bill02_new_ok")
    .bind("sub_bill02_old_ok")
    .execute(db.pool())
    .await
    .unwrap();
    sqlx::query(
        "UPDATE organizations SET stripe_customer_id = $2, stripe_subscription_id = $3, \
         stripe_subscription_status = 'active', \
         stripe_superseded_subscription_ids = ARRAY[$4]::TEXT[] WHERE id = $1",
    )
    .bind(&bad.org_id)
    .bind("cus_bill02_bad")
    .bind("sub_bill02_new_bad")
    .bind("sub_bill02_old_bad")
    .execute(db.pool())
    .await
    .unwrap();

    let stripe = wiremock::MockServer::start().await;
    for id in [
        "sub_bill02_new_ok",
        "sub_bill02_old_ok",
        "sub_bill02_new_bad",
    ] {
        wiremock::Mock::given(method("DELETE"))
            .and(path(format!("/v1/subscriptions/{id}")))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(json!({ "id": id, "status": "canceled" })),
            )
            .mount(&stripe)
            .await;
    }
    wiremock::Mock::given(method("DELETE"))
        .and(path("/v1/subscriptions/sub_bill02_old_bad"))
        .respond_with(wiremock::ResponseTemplate::new(500).set_body_string("stripe down"))
        .mount(&stripe)
        .await;

    let app = app_with(&db, None, stripe_settings());
    let bad_auth = bearer(&bad.key);
    let (status, body) = spooled_backend::api::handlers::billing::with_stripe_api_base(
        stripe.uri(),
        call(
            &app,
            "DELETE",
            &format!("/api/v1/organizations/{}", bad.org_id),
            &[("Authorization", &bad_auth)],
            None,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    let still: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM organizations WHERE id = $1")
        .bind(&bad.org_id)
        .fetch_one(db.pool())
        .await
        .unwrap();
    assert_eq!(
        still.0, 1,
        "org was deleted although a superseded cancel failed"
    );

    let ok_auth = bearer(&ok.key);
    let (status, body) = spooled_backend::api::handlers::billing::with_stripe_api_base(
        stripe.uri(),
        call(
            &app,
            "DELETE",
            &format!("/api/v1/organizations/{}", ok.org_id),
            &[("Authorization", &ok_auth)],
            None,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    let gone: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM organizations WHERE id = $1")
        .bind(&ok.org_id)
        .fetch_one(db.pool())
        .await
        .unwrap();
    assert_eq!(gone.0, 0);

    let requests = stripe.received_requests().await.unwrap_or_default();
    for id in [
        "sub_bill02_new_ok",
        "sub_bill02_old_ok",
        "sub_bill02_old_bad",
    ] {
        assert!(
            requests
                .iter()
                .any(|r| r.method == "DELETE" && r.url.path().ends_with(id)),
            "missing DELETE for {id}: {:?}",
            requests
                .iter()
                .map(|r| r.url.path().to_string())
                .collect::<Vec<_>>()
        );
    }
}

// BILL-03: a gRPC replay at the daily cap returns the existing job.
#[tokio::test]
async fn bill03_grpc_replay_at_daily_cap_returns_existing_job() {
    use spooled_backend::grpc::proto::queue_service_server::QueueService;
    use spooled_backend::grpc::proto::EnqueueRequest;
    use spooled_backend::grpc::services::QueueServiceImpl;
    use spooled_backend::outgoing_webhooks::service::OutgoingWebhookService;

    let db = TestDatabase::new().await;
    let tenant = seed_tenant(db.pool()).await;
    sqlx::query(
        "UPDATE organizations SET custom_limits = '{\"max_jobs_per_day\": 1}'::JSONB WHERE id = $1",
    )
    .bind(&tenant.org_id)
    .execute(db.pool())
    .await
    .unwrap();

    let service = QueueServiceImpl::new(
        Arc::clone(&db.pool),
        Arc::new(Metrics::new()),
        None,
        Arc::new(OutgoingWebhookService::new((*db.pool).clone(), 1, 4)),
        QueueSettings {
            default_max_retries: 3,
            default_timeout_secs: 300,
            max_payload_size_bytes: 1024 * 1024,
        },
        false,
        WorkerSettings {
            heartbeat_interval_secs: 10,
            lease_duration_secs: 30,
            grpc_default_lease_duration_secs: 300,
            max_concurrency: 10,
            fallback_poll_interval_secs: 5,
        },
    );

    let enqueue = |key: &str, idem: &str| {
        let mut request = tonic::Request::new(EnqueueRequest {
            queue_name: "jobs".to_string(),
            payload: None,
            priority: 0,
            max_retries: 0,
            timeout_seconds: 0,
            scheduled_at: None,
            idempotency_key: idem.to_string(),
            tags: Default::default(),
        });
        request
            .metadata_mut()
            .insert("x-api-key", key.parse().unwrap());
        request
    };

    let first = service
        .enqueue(enqueue(&tenant.key, "bill03-key"))
        .await
        .expect("first enqueue")
        .into_inner();
    assert!(first.created, "{first:?}");

    let rejected = service
        .enqueue(enqueue(&tenant.key, "bill03-other"))
        .await
        .expect_err("second job at the cap");
    assert_eq!(
        rejected.code(),
        tonic::Code::ResourceExhausted,
        "{rejected:?}"
    );

    let replay = service
        .enqueue(enqueue(&tenant.key, "bill03-key"))
        .await
        .expect("replay at the cap")
        .into_inner();
    assert!(!replay.created, "{replay:?}");
    assert_eq!(replay.job_id, first.job_id);

    let (count,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM jobs WHERE organization_id = $1")
        .bind(&tenant.org_id)
        .fetch_one(db.pool())
        .await
        .unwrap();
    assert_eq!(count, 1);
}

// BILL-04: a key 429 does not spend an org token.
#[tokio::test]
async fn bill04_key_limit_does_not_spend_org_token() {
    use redis::AsyncCommands;

    let db = TestDatabase::new().await;
    let tenant = seed_tenant(db.pool()).await;
    sqlx::query(
        "UPDATE organizations SET custom_limits = \
         '{\"rate_limit_requests_per_second\": 1, \"rate_limit_burst\": 5}'::JSONB WHERE id = $1",
    )
    .bind(&tenant.org_id)
    .execute(db.pool())
    .await
    .unwrap();
    sqlx::query("UPDATE api_keys SET rate_limit = 1 WHERE organization_id = $1")
        .bind(&tenant.org_id)
        .execute(db.pool())
        .await
        .unwrap();

    let (_redis, cache) = redis_cache().await;
    let app = app_with(&db, Some(cache.clone()), Settings::load_for_testing());
    let auth = bearer(&tenant.key);
    let (status, body) = call(
        &app,
        "GET",
        "/api/v1/jobs",
        &[("Authorization", &auth)],
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    for _ in 0..2 {
        let (status, body) = call(
            &app,
            "GET",
            "/api/v1/jobs",
            &[("Authorization", &auth)],
            None,
        )
        .await;
        assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "{body}");
    }

    let mut conn = cache.get_connection().await.expect("redis conn");
    let tokens: Option<String> = conn
        .hget(format!("rate_limit:org:{}", tenant.org_id), "tokens")
        .await
        .expect("hget");
    let tokens: f64 = tokens.expect("org bucket missing").parse().expect("tokens");
    assert!(
        tokens > 3.5,
        "org bucket spent tokens on key 429s: {tokens}"
    );
}

// BILL-05: a zero rps/burst override rejects the request.
#[tokio::test]
async fn bill05_zero_rate_limit_override_rejects() {
    let db = TestDatabase::new().await;
    let tenant = seed_tenant(db.pool()).await;
    sqlx::query(
        "UPDATE organizations SET custom_limits = \
         '{\"rate_limit_requests_per_second\": 0, \"rate_limit_burst\": 0}'::JSONB WHERE id = $1",
    )
    .bind(&tenant.org_id)
    .execute(db.pool())
    .await
    .unwrap();

    let (_redis, cache) = redis_cache().await;
    let app = app_with(&db, Some(cache), Settings::load_for_testing());
    let auth = bearer(&tenant.key);
    let (status, body) = call(
        &app,
        "GET",
        "/api/v1/jobs",
        &[("Authorization", &auth)],
        None,
    )
    .await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "{body}");
}

// BILL-06: an invoice updates only its own subscription, and only when it is
// not older than the last subscription event. The subscription clock stays.
#[tokio::test]
async fn bill06_stale_invoice_event_does_not_overwrite_subscription_status() {
    let db = TestDatabase::new().await;
    let tenant = seed_tenant(db.pool()).await;
    let other = seed_tenant(db.pool()).await;
    let customer = "cus_bill06";
    let live = "sub_bill06_live";
    let other_sub = "sub_bill06_other";
    sqlx::query(
        "UPDATE organizations SET plan_tier = 'pro', stripe_customer_id = $2, \
         stripe_subscription_id = $3, stripe_subscription_status = 'active' WHERE id = $1",
    )
    .bind(&tenant.org_id)
    .bind(customer)
    .bind(live)
    .execute(db.pool())
    .await
    .unwrap();
    sqlx::query(
        "UPDATE organizations SET plan_tier = 'pro', stripe_customer_id = $2, \
         stripe_subscription_id = $3, stripe_subscription_status = 'active' WHERE id = $1",
    )
    .bind(&other.org_id)
    .bind(customer)
    .bind(other_sub)
    .execute(db.pool())
    .await
    .unwrap();

    let app = app_with(&db, None, stripe_settings());
    let stripe = wiremock::MockServer::start().await;
    let t = 1_700_000_500_i64;

    let (status, body) = deliver(
        &app,
        &stripe,
        &stripe_event(
            "evt_bill06_sub",
            "customer.subscription.updated",
            t,
            subscription(live, customer, PRO_PRICE, "active"),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let invoice =
        |id: &str, sub: &str| json!({ "id": id, "customer": customer, "subscription": sub });

    let (status, body) = deliver(
        &app,
        &stripe,
        &stripe_event(
            "evt_bill06_old_fail",
            "invoice.payment_failed",
            t - 100,
            invoice("in_old", live),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let row = billing_row(db.pool(), &tenant.org_id).await;
    assert_eq!(
        row.stripe_subscription_status.as_deref(),
        Some("active"),
        "{row:?}"
    );

    let (status, body) = deliver(
        &app,
        &stripe,
        &stripe_event(
            "evt_bill06_other_fail",
            "invoice.payment_failed",
            t + 100,
            invoice("in_other", "sub_bill06_unknown"),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let row = billing_row(db.pool(), &tenant.org_id).await;
    assert_eq!(
        row.stripe_subscription_status.as_deref(),
        Some("active"),
        "{row:?}"
    );
    let other_row = billing_row(db.pool(), &other.org_id).await;
    assert_eq!(
        other_row.stripe_subscription_status.as_deref(),
        Some("active"),
        "invoice for one subscription changed another org: {other_row:?}"
    );

    let (status, body) = deliver(
        &app,
        &stripe,
        &stripe_event(
            "evt_bill06_fail",
            "invoice.payment_failed",
            t + 100,
            invoice("in_fail", live),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let row = billing_row(db.pool(), &tenant.org_id).await;
    assert_eq!(
        row.stripe_subscription_status.as_deref(),
        Some("past_due"),
        "{row:?}"
    );
    assert_eq!(row.stripe_last_event_at.unwrap().timestamp(), t);

    let (status, body) = deliver(
        &app,
        &stripe,
        &stripe_event(
            "evt_bill06_paid",
            "invoice.paid",
            t + 200,
            invoice("in_paid", live),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let row = billing_row(db.pool(), &tenant.org_id).await;
    assert_eq!(
        row.stripe_subscription_status.as_deref(),
        Some("active"),
        "{row:?}"
    );
    assert_eq!(
        row.stripe_last_event_at.unwrap().timestamp(),
        t,
        "invoice event advanced stripe_last_event_at"
    );
}

// AUTH-01: changing the verified billing email burns the old login code,
// rejects a later code for the old address, and kills the email-login session.
#[tokio::test]
async fn auth01_billing_email_change_invalidates_old_login() {
    let db = TestDatabase::new().await;
    let tenant = seed_tenant(db.pool()).await;
    let alice = "alice-auth01@example.com";
    sqlx::query(
        "UPDATE organizations SET billing_email = $2, billing_email_verified_at = NOW() WHERE id = $1",
    )
    .bind(&tenant.org_id)
    .bind(alice)
    .execute(db.pool())
    .await
    .unwrap();

    let raw = format!("sp_test_{}", uuid::Uuid::new_v4().simple());
    let hash = bcrypt::hash(&raw, 4).expect("hash");
    sqlx::query(
        "INSERT INTO api_keys (id, organization_id, key_hash, key_prefix, lookup_hash, name, queues, is_active, created_at, is_system) \
         VALUES ($1, $2, $3, $4, $5, 'Email Login', ARRAY['*']::TEXT[], TRUE, NOW(), TRUE)",
    )
    .bind(uuid::Uuid::new_v4().to_string())
    .bind(&tenant.org_id)
    .bind(&hash)
    .bind(raw.chars().take(8).collect::<String>())
    .bind(spooled_backend::models::api_key_lookup_hash(&raw))
    .execute(db.pool())
    .await
    .unwrap();

    let mut settings = Settings::load_for_testing();
    settings.registration.admin_api_key = Some("grok-be1-admin".to_string());
    let app = app_with(&db, None, settings);

    let login_auth = bearer(&raw);
    let (status, login) = call(
        &app,
        "POST",
        "/api/v1/auth/login",
        &[("Authorization", &login_auth)],
        Some(json!({ "api_key": raw })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{login}");
    let access = login["access_token"].as_str().unwrap().to_string();
    let refresh = login["refresh_token"].as_str().unwrap().to_string();
    let access_header = bearer(&access);

    sqlx::query(
        "INSERT INTO email_login_codes (email, code, organization_id, expires_at) \
         VALUES ($1, '111111', $2, NOW() + INTERVAL '10 minutes')",
    )
    .bind(alice)
    .bind(&tenant.org_id)
    .execute(db.pool())
    .await
    .unwrap();

    let (status, body) = call(
        &app,
        "PATCH",
        &format!("/api/v1/admin/organizations/{}", tenant.org_id),
        &[("X-Admin-Key", "grok-be1-admin")],
        Some(json!({ "billing_email": "bob-auth01@example.com" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let (used,): (bool,) = sqlx::query_as(
        "SELECT used_at IS NOT NULL FROM email_login_codes WHERE email = $1 AND code = '111111'",
    )
    .bind(alice)
    .fetch_one(db.pool())
    .await
    .unwrap();
    assert!(used, "login code for the old address was not burned");

    sqlx::query(
        "INSERT INTO email_login_codes (email, code, organization_id, expires_at) \
         VALUES ($1, '222222', $2, NOW() + INTERVAL '10 minutes')",
    )
    .bind(alice)
    .bind(&tenant.org_id)
    .execute(db.pool())
    .await
    .unwrap();
    let (status, body) = call(
        &app,
        "POST",
        "/api/v1/auth/email/verify",
        &[],
        Some(json!({ "email": alice, "code": "222222" })),
    )
    .await;
    assert_ne!(body["type"], "login", "{status} {body}");

    let (status, body) = call(
        &app,
        "GET",
        "/api/v1/jobs",
        &[("Authorization", &access_header)],
        None,
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");
    let (status, body) = call(
        &app,
        "POST",
        "/api/v1/auth/refresh",
        &[],
        Some(json!({ "refresh_token": refresh })),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");
}

// AUTH-02: logout survives Redis dropping the session key.
#[tokio::test]
async fn auth02_logout_survives_redis_eviction() {
    let db = TestDatabase::new().await;
    let tenant = seed_tenant(db.pool()).await;
    let (_redis, cache) = redis_cache().await;
    let app = app_with(&db, Some(cache.clone()), Settings::load_for_testing());
    let (status, login) = call(
        &app,
        "POST",
        "/api/v1/auth/login",
        &[],
        Some(json!({ "api_key": tenant.key })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{login}");
    let access = login["access_token"].as_str().unwrap().to_string();
    let refresh = login["refresh_token"].as_str().unwrap().to_string();
    let access_header = bearer(&access);

    let (status, body) = call(
        &app,
        "POST",
        "/api/v1/auth/logout",
        &[("Authorization", &access_header)],
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");

    let data = jsonwebtoken::decode::<Claims>(
        &access,
        &jsonwebtoken::DecodingKey::from_secret(b"test-secret"),
        &jsonwebtoken::Validation::default(),
    )
    .expect("decode access token");
    let sid = data.claims.sid.expect("sid");
    cache
        .delete(&format!("session_revoked:{sid}"))
        .await
        .expect("drop redis session key");

    let (status, body) = call(
        &app,
        "POST",
        "/api/v1/auth/refresh",
        &[],
        Some(json!({ "refresh_token": refresh })),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");
}

// AUTH-02: during a Redis outage a live session keeps working (Postgres is the
// source of truth), and a logged-out session is still rejected.
#[tokio::test]
async fn auth02_redis_outage_checks_revocation_in_postgres() {
    let db = TestDatabase::new().await;
    let tenant = seed_tenant(db.pool()).await;
    let (_redis, cache) = redis_cache().await;
    let live = app_with(&db, Some(cache), Settings::load_for_testing());
    let (status, login) = call(
        &live,
        "POST",
        "/api/v1/auth/login",
        &[],
        Some(json!({ "api_key": tenant.key })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{login}");
    let access = login["access_token"].as_str().unwrap().to_string();
    let access_header = bearer(&access);

    let (status, body) = call(
        &live,
        "GET",
        "/api/v1/auth/me",
        &[("Authorization", &access_header)],
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let dead = RedisCache::connect_lazy(&RedisSettings {
        url: "redis://127.0.0.1:1".to_string(),
        pool_size: 1,
    })
    .expect("lazy redis");
    let down = app_with(&db, Some(dead), Settings::load_for_testing());
    let (status, body) = call(
        &down,
        "GET",
        "/api/v1/auth/me",
        &[("Authorization", &access_header)],
        None,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "a Redis outage must not log users out: {body}"
    );

    let (status, body) = call(
        &live,
        "POST",
        "/api/v1/auth/logout",
        &[("Authorization", &access_header)],
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    let (status, body) = call(
        &down,
        "GET",
        "/api/v1/auth/me",
        &[("Authorization", &access_header)],
        None,
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");
}

// AUTH-04: /auth/me and /auth/validate reject what the middleware rejects.
#[tokio::test]
async fn auth04_validate_and_me_reject_tokens_middleware_rejects() {
    let db = TestDatabase::new().await;
    let tenant = seed_tenant(db.pool()).await;
    let app = app(&db);
    let expired_id = uuid::Uuid::new_v4().to_string();
    let expired_raw = format!("sp_test_{}", uuid::Uuid::new_v4().simple());
    sqlx::query(
        "INSERT INTO api_keys (id, organization_id, key_hash, key_prefix, lookup_hash, name, queues, is_active, expires_at, created_at) \
         VALUES ($1, $2, $3, $4, $5, 'expired', ARRAY['*']::TEXT[], TRUE, NOW() - INTERVAL '1 hour', NOW())",
    )
    .bind(&expired_id)
    .bind(&tenant.org_id)
    .bind(bcrypt::hash(&expired_raw, 4).unwrap())
    .bind(expired_raw.chars().take(8).collect::<String>())
    .bind(spooled_backend::models::api_key_lookup_hash(&expired_raw))
    .execute(db.pool())
    .await
    .unwrap();
    let expired_jwt = mint_token(&access_claims(&tenant.org_id, &expired_id, "access"));
    let expired_header = bearer(&expired_jwt);

    let (status, body) = call(
        &app,
        "GET",
        "/api/v1/auth/me",
        &[("Authorization", &expired_header)],
        None,
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");
    assert!(body.get("billing_email").is_none(), "{body}");

    let revoked_id = uuid::Uuid::new_v4().to_string();
    let revoked_raw = format!("sp_test_{}", uuid::Uuid::new_v4().simple());
    sqlx::query(
        "INSERT INTO api_keys (id, organization_id, key_hash, key_prefix, lookup_hash, name, queues, is_active, created_at) \
         VALUES ($1, $2, $3, $4, $5, 'revoked', ARRAY['*']::TEXT[], FALSE, NOW())",
    )
    .bind(&revoked_id)
    .bind(&tenant.org_id)
    .bind(bcrypt::hash(&revoked_raw, 4).unwrap())
    .bind(revoked_raw.chars().take(8).collect::<String>())
    .bind(spooled_backend::models::api_key_lookup_hash(&revoked_raw))
    .execute(db.pool())
    .await
    .unwrap();

    let cases = [
        (
            "refresh",
            mint_token(&access_claims(&tenant.org_id, &expired_id, "refresh")),
        ),
        (
            "revoked",
            mint_token(&access_claims(&tenant.org_id, &revoked_id, "access")),
        ),
        ("expired", expired_jwt.clone()),
    ];
    for (label, token) in cases {
        let (status, body) = call(
            &app,
            "POST",
            "/api/v1/auth/validate",
            &[],
            Some(json!({ "token": token })),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{label} {body}");
        assert_eq!(body["valid"], false, "{label} {body}");
    }

    let (status, login) = call(
        &app,
        "POST",
        "/api/v1/auth/login",
        &[],
        Some(json!({ "api_key": tenant.key })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{login}");
    let good = login["access_token"].as_str().unwrap();
    let (status, body) = call(
        &app,
        "POST",
        "/api/v1/auth/validate",
        &[],
        Some(json!({ "token": good })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["valid"], true, "{body}");
    let good_header = bearer(good);
    let (status, body) = call(
        &app,
        "GET",
        "/api/v1/auth/me",
        &[("Authorization", &good_header)],
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
}

// AUTH-05: a queues update that commits during bcrypt is what the cache stores.
#[tokio::test]
async fn auth05_api_key_cache_sees_queue_update_during_bcrypt() {
    let db = TestDatabase::new().await;
    let tenant = seed_tenant(db.pool()).await;
    let raw = format!("sp_test_{}", uuid::Uuid::new_v4().simple());
    let key_id = uuid::Uuid::new_v4().to_string();
    // Cost 12 is slow enough that the update lands during verify, before the
    // cache write. Cost 4 finishes first and the race does not exist.
    sqlx::query(
        "INSERT INTO api_keys (id, organization_id, key_hash, key_prefix, lookup_hash, name, queues, is_active, created_at) \
         VALUES ($1, $2, $3, $4, $5, 'slow', ARRAY['*']::TEXT[], TRUE, NOW())",
    )
    .bind(&key_id)
    .bind(&tenant.org_id)
    .bind(bcrypt::hash(&raw, 12).expect("hash"))
    .bind(raw.chars().take(8).collect::<String>())
    .bind(spooled_backend::models::api_key_lookup_hash(&raw))
    .execute(db.pool())
    .await
    .unwrap();

    let (_redis, cache) = redis_cache().await;
    let app = app_with(&db, Some(cache), Settings::load_for_testing());
    let auth = bearer(&raw);
    let app2 = app.clone();
    let auth2 = auth.clone();
    let first = tokio::spawn(async move {
        call(
            &app2,
            "GET",
            "/api/v1/jobs",
            &[("Authorization", &auth2)],
            None,
        )
        .await
    });
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    sqlx::query("UPDATE api_keys SET queues = ARRAY['orders']::TEXT[] WHERE id = $1")
        .bind(&key_id)
        .execute(db.pool())
        .await
        .unwrap();
    let (status, body) = first.await.expect("request task");
    assert_eq!(status, StatusCode::OK, "{body}");

    let (status, body) = call(
        &app,
        "POST",
        "/api/v1/jobs",
        &[("Authorization", &auth)],
        Some(json!({ "queue_name": "other", "payload": {} })),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
}
