#![cfg(feature = "docker-tests")]
//! ORG-01, ORG-03, ORG-04, ORG-05. Restore, canceled subscriptions, signup
//! atomicity, and GET /api-keys paging.

mod common;

use std::sync::Arc;

use axum::body::Body;
use axum::http::{HeaderMap, Request, StatusCode};
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

const ADMIN: &str = "r0128-admin";

struct Tenant {
    org_id: String,
    key: String,
    key_id: String,
}

async fn seed_tenant(pool: &sqlx::PgPool, plan: &str) -> Tenant {
    let org_id = uuid::Uuid::new_v4().to_string();
    let slug = format!("og-{}", &org_id[..8]);
    sqlx::query(
        "INSERT INTO organizations (id, name, slug, plan_tier, billing_email, billing_email_verified_at, settings, created_at, updated_at) \
         VALUES ($1, $2, $2, $3, $4, NOW(), '{}'::JSONB, NOW(), NOW())",
    )
    .bind(&org_id)
    .bind(&slug)
    .bind(plan)
    .bind(format!("{slug}@example.com"))
    .execute(pool)
    .await
    .expect("seed org");

    let key = format!("sp_test_{}", uuid::Uuid::new_v4().simple());
    let key_id = uuid::Uuid::new_v4().to_string();
    insert_key(pool, &org_id, &key, &key_id, chrono::Utc::now()).await;
    Tenant {
        org_id,
        key,
        key_id,
    }
}

async fn insert_key(
    pool: &sqlx::PgPool,
    org_id: &str,
    key: &str,
    id: &str,
    created_at: chrono::DateTime<chrono::Utc>,
) {
    let hash = bcrypt::hash(key, 4).expect("hash key");
    sqlx::query(
        "INSERT INTO api_keys (id, organization_id, key_hash, key_prefix, lookup_hash, name, queues, is_active, created_at) \
         VALUES ($1, $2, $3, $4, $5, $1, ARRAY[]::TEXT[], TRUE, $6)",
    )
    .bind(id)
    .bind(org_id)
    .bind(&hash)
    .bind(key.chars().take(8).collect::<String>())
    .bind(spooled_backend::models::api_key_lookup_hash(key))
    .bind(created_at)
    .execute(pool)
    .await
    .expect("seed key");
}

fn settings() -> Settings {
    let mut settings = Settings::load_for_testing();
    settings.registration.admin_api_key = Some(ADMIN.to_string());
    settings.stripe.secret_key = Some("sk_test_r0128".to_string());
    settings.stripe.starter_price_id = Some("price_starter".to_string());
    settings.stripe.pro_price_id = Some("price_pro".to_string());
    settings
}

fn app(db: &TestDatabase) -> Router {
    router(AppState::new(
        Database::from_pool(Arc::clone(&db.pool)),
        None,
        Metrics::new(),
        settings(),
    ))
}

async fn call(
    app: &Router,
    method: &str,
    uri: &str,
    headers: &[(&str, &str)],
    body: Option<Value>,
) -> (StatusCode, HeaderMap, Value) {
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
    let headers = res.headers().clone();
    let bytes = axum::body::to_bytes(res.into_body(), 1 << 20)
        .await
        .unwrap();
    let json = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes)
            .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into()))
    };
    (status, headers, json)
}

async fn org_email(pool: &sqlx::PgPool, id: &str) -> (String, Option<String>, bool) {
    sqlx::query_as(
        "SELECT plan_tier, billing_email, billing_email_verified_at IS NOT NULL \
         FROM organizations WHERE id = $1",
    )
    .bind(id)
    .fetch_one(pool)
    .await
    .unwrap()
}

/// ORG-01: restoring a soft-deleted org drops a verified billing email that a
/// live org already holds, and keeps one that nobody else has.
#[tokio::test]
async fn org01_restore_drops_a_verified_email_held_by_a_live_org() {
    let db = TestDatabase::new().await;
    let app = app(&db);
    let live = seed_tenant(db.pool(), "starter").await;
    let taken = seed_tenant(db.pool(), "deleted").await;
    let unique = seed_tenant(db.pool(), "deleted").await;
    let shared = format!("shared-{}@example.com", &live.org_id[..8]);
    sqlx::query(
        "UPDATE organizations SET billing_email = $2, billing_email_verified_at = NOW() WHERE id = $1",
    )
    .bind(&live.org_id)
    .bind(&shared)
    .execute(db.pool())
    .await
    .unwrap();
    sqlx::query(
        "UPDATE organizations SET billing_email = $2, billing_email_verified_at = NOW() WHERE id = $1",
    )
    .bind(&taken.org_id)
    .bind(&shared)
    .execute(db.pool())
    .await
    .unwrap();

    let (status, _, body) = call(
        &app,
        "PATCH",
        &format!("/api/v1/admin/organizations/{}", taken.org_id),
        &[("X-Admin-Key", ADMIN)],
        Some(json!({"plan_tier": "starter"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (plan, email, verified) = org_email(db.pool(), &taken.org_id).await;
    assert_eq!(plan, "starter");
    assert_eq!(email, None, "the live org already verified this address");
    assert!(!verified);
    let (_, live_email, live_verified) = org_email(db.pool(), &live.org_id).await;
    assert_eq!(live_email.as_deref(), Some(shared.as_str()));
    assert!(live_verified);

    let (status, _, body) = call(
        &app,
        "PATCH",
        &format!("/api/v1/admin/organizations/{}", unique.org_id),
        &[("X-Admin-Key", ADMIN)],
        Some(json!({"plan_tier": "pro"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (plan, email, verified) = org_email(db.pool(), &unique.org_id).await;
    assert_eq!(plan, "pro");
    assert!(email.is_some(), "an unclaimed address stays on restore");
    assert!(verified);
}

/// ORG-03: a plan-only patch does not apply a canceled subscription's tier,
/// and deleting the org clears the subscription id after Stripe cancels it.
#[tokio::test]
async fn org03_canceled_subscription_does_not_own_the_plan() {
    let db = TestDatabase::new().await;
    let app = app(&db);
    let stripe = MockServer::start().await;
    let plan_org = seed_tenant(db.pool(), "free").await;
    sqlx::query(
        "UPDATE organizations SET stripe_subscription_id = 'sub_plan', stripe_subscription_status = 'canceled' WHERE id = $1",
    )
    .bind(&plan_org.org_id)
    .execute(db.pool())
    .await
    .unwrap();
    Mock::given(method("GET"))
        .and(path("/v1/subscriptions/sub_plan"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "sub_plan",
            "status": "canceled",
            "customer": "cus_plan"
        })))
        .expect(0)
        .mount(&stripe)
        .await;

    let (status, _, body) = spooled_backend::api::handlers::billing::with_stripe_api_base(
        stripe.uri(),
        call(
            &app,
            "PATCH",
            &format!("/api/v1/admin/organizations/{}", plan_org.org_id),
            &[("X-Admin-Key", ADMIN)],
            Some(json!({"plan_tier": "pro"})),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (plan, sub, sub_status): (String, Option<String>, Option<String>) = sqlx::query_as(
        "SELECT plan_tier, stripe_subscription_id, stripe_subscription_status FROM organizations WHERE id = $1",
    )
    .bind(&plan_org.org_id)
    .fetch_one(db.pool())
    .await
    .unwrap();
    assert_eq!(
        plan, "pro",
        "a plan-only patch must not reconcile the canceled subscription"
    );
    assert_eq!(sub.as_deref(), Some("sub_plan"));
    assert_eq!(sub_status.as_deref(), Some("canceled"));
    stripe.verify().await;

    let delete_org = seed_tenant(db.pool(), "starter").await;
    sqlx::query(
        "UPDATE organizations SET stripe_subscription_id = 'sub_del', stripe_subscription_status = 'active' WHERE id = $1",
    )
    .bind(&delete_org.org_id)
    .execute(db.pool())
    .await
    .unwrap();
    Mock::given(method("DELETE"))
        .and(path("/v1/subscriptions/sub_del"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "sub_del",
            "status": "canceled"
        })))
        .mount(&stripe)
        .await;
    let (status, _, body) = spooled_backend::api::handlers::billing::with_stripe_api_base(
        stripe.uri(),
        call(
            &app,
            "DELETE",
            &format!("/api/v1/admin/organizations/{}", delete_org.org_id),
            &[("X-Admin-Key", ADMIN)],
            None,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    let (plan, sub, sub_status): (String, Option<String>, Option<String>) = sqlx::query_as(
        "SELECT plan_tier, stripe_subscription_id, stripe_subscription_status FROM organizations WHERE id = $1",
    )
    .bind(&delete_org.org_id)
    .fetch_one(db.pool())
    .await
    .unwrap();
    assert_eq!(plan, "deleted");
    assert_eq!(sub, None, "the canceled subscription id must not remain");
    assert_eq!(sub_status.as_deref(), Some("canceled"));
}

async fn signup_token(pool: &sqlx::PgPool, email: &str) -> String {
    let token = uuid::Uuid::new_v4().simple().to_string();
    sqlx::query(
        "INSERT INTO email_login_codes (email, code, expires_at) VALUES ($1, $2, NOW() + INTERVAL '1 hour')",
    )
    .bind(email)
    .bind(format!("signup:{token}"))
    .execute(pool)
    .await
    .unwrap();
    token
}

async fn token_used(pool: &sqlx::PgPool, token: &str) -> bool {
    let used: Option<chrono::DateTime<chrono::Utc>> =
        sqlx::query_scalar("SELECT used_at FROM email_login_codes WHERE code = $1")
            .bind(format!("signup:{token}"))
            .fetch_one(pool)
            .await
            .unwrap();
    used.is_some()
}

/// ORG-04: the org and its first key commit together, and a signup token is
/// consumed only once that org exists.
#[tokio::test]
async fn org04_org_and_first_key_and_signup_token_commit_together() {
    let db = TestDatabase::new().await;
    let app = app(&db);

    sqlx::query(
        "CREATE OR REPLACE FUNCTION r0128_reject_key() RETURNS trigger LANGUAGE plpgsql AS $$ \
         BEGIN RAISE EXCEPTION 'r0128 key insert failed'; END; $$",
    )
    .execute(db.pool())
    .await
    .unwrap();
    sqlx::query(
        "CREATE TRIGGER r0128_reject_key BEFORE INSERT ON api_keys \
         FOR EACH ROW EXECUTE FUNCTION r0128_reject_key()",
    )
    .execute(db.pool())
    .await
    .unwrap();
    let slug = format!("r{}", &uuid::Uuid::new_v4().simple().to_string()[..10]);
    let (status, _, body) = call(
        &app,
        "POST",
        "/api/v1/organizations",
        &[],
        Some(json!({"name": "R0128 Atomic", "slug": slug})),
    )
    .await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR, "{body}");
    let orgs: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM organizations WHERE slug = $1")
        .bind(&slug)
        .fetch_one(db.pool())
        .await
        .unwrap();
    assert_eq!(
        orgs, 0,
        "a failed key insert must not leave the organization"
    );
    sqlx::query("DROP TRIGGER r0128_reject_key ON api_keys")
        .execute(db.pool())
        .await
        .unwrap();

    // Hold the insert open so both signups pass the slug pre-check and meet
    // on the unique index. The loser must still have an unused token.
    sqlx::query(
        "CREATE OR REPLACE FUNCTION r0128_slow_org() RETURNS trigger LANGUAGE plpgsql AS $$ \
         BEGIN PERFORM pg_sleep(0.8); RETURN NEW; END; $$",
    )
    .execute(db.pool())
    .await
    .unwrap();
    sqlx::query(
        "CREATE TRIGGER r0128_slow_org BEFORE INSERT ON organizations \
         FOR EACH ROW EXECUTE FUNCTION r0128_slow_org()",
    )
    .execute(db.pool())
    .await
    .unwrap();

    let race_slug = format!("r{}", &uuid::Uuid::new_v4().simple().to_string()[..10]);
    let token_a = signup_token(db.pool(), "a-r0128@example.com").await;
    let token_b = signup_token(db.pool(), "b-r0128@example.com").await;
    let body_a = json!({"signup_token": token_a, "name": "R0128 Race", "slug": race_slug});
    let body_b = json!({"signup_token": token_b, "name": "R0128 Race", "slug": race_slug});
    let (a, b) = tokio::join!(
        call(
            &app,
            "POST",
            "/api/v1/auth/signup/complete",
            &[],
            Some(body_a)
        ),
        call(
            &app,
            "POST",
            "/api/v1/auth/signup/complete",
            &[],
            Some(body_b)
        ),
    );
    let winners = [&a, &b]
        .iter()
        .filter(|(status, _, _)| *status == StatusCode::CREATED)
        .count();
    assert_eq!(winners, 1, "one signup wins: {a:?} {b:?}");
    let loser = if a.0 == StatusCode::CREATED { &b } else { &a };
    assert_eq!(loser.0, StatusCode::CONFLICT, "loser: {loser:?}");
    assert_eq!(
        loser.2["message"], "Organization creation failed due to conflict",
        "the loser must lose on the insert, not the pre-check: {loser:?}"
    );
    let orgs: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM organizations WHERE slug = $1")
        .bind(&race_slug)
        .fetch_one(db.pool())
        .await
        .unwrap();
    assert_eq!(orgs, 1);
    let used = u8::from(token_used(db.pool(), &token_a).await)
        + u8::from(token_used(db.pool(), &token_b).await);
    assert_eq!(used, 1, "the losing token must stay unused");
}

/// ORG-05 / DASH-30: GET /api-keys pages like GET /jobs. The default body is
/// still a bare array of the 100 newest keys.
#[tokio::test]
async fn org05_api_key_list_is_paged_newest_first() {
    let db = TestDatabase::new().await;
    let app = app(&db);
    let t = seed_tenant(db.pool(), "starter").await;
    let auth = format!("Bearer {}", t.key);
    // The seed key is older than this page, so the default 100 are these rows.
    sqlx::query("UPDATE api_keys SET created_at = '2020-01-01T00:00:00Z' WHERE id = $1")
        .bind(&t.key_id)
        .execute(db.pool())
        .await
        .unwrap();

    let stamp = chrono::DateTime::parse_from_rfc3339("2026-06-01T00:00:00Z")
        .unwrap()
        .with_timezone(&chrono::Utc);
    for i in 0..100 {
        let id = format!("k{i:03}");
        let when = stamp + chrono::Duration::seconds(i);
        // Dummy hash: these keys are never presented. The unique index is on key_hash.
        sqlx::query(
            "INSERT INTO api_keys (id, organization_id, key_hash, key_prefix, name, queues, is_active, created_at) \
             VALUES ($1, $2, $3, 'sp_test_', $1, ARRAY[]::TEXT[], TRUE, $4)",
        )
        .bind(&id)
        .bind(&t.org_id)
        .bind(format!("hash-{id}"))
        .bind(when)
        .execute(db.pool())
        .await
        .unwrap();
    }

    let (status, headers, body) = call(
        &app,
        "GET",
        "/api/v1/api-keys",
        &[("Authorization", &auth)],
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let page = body.as_array().expect("bare array");
    assert_eq!(page.len(), 100);
    assert_eq!(
        headers.get("x-has-more").and_then(|v| v.to_str().ok()),
        Some("true")
    );
    assert_eq!(page[0]["id"], "k099", "newest created_at first");
    assert_eq!(page[99]["id"], "k000");

    let (status, headers, body) = call(
        &app,
        "GET",
        "/api/v1/api-keys?offset=100",
        &[("Authorization", &auth)],
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let rest = body.as_array().unwrap();
    assert_eq!(rest.len(), 1, "the seed key is the oldest");
    assert_eq!(
        headers.get("x-has-more").and_then(|v| v.to_str().ok()),
        Some("false")
    );

    // Same created_at: id DESC is the tie break. limit=0 clamps to 1.
    let tie = seed_tenant(db.pool(), "starter").await;
    let when = chrono::Utc::now();
    sqlx::query("UPDATE api_keys SET id = 'id-a', created_at = $2 WHERE id = $1")
        .bind(&tie.key_id)
        .bind(when)
        .execute(db.pool())
        .await
        .unwrap();
    for id in ["id-b", "id-c"] {
        let key = format!("sp_test_{}", uuid::Uuid::new_v4().simple());
        insert_key(db.pool(), &tie.org_id, &key, id, when).await;
    }
    // id-a is the renamed seed key; its lookup hash is unchanged.
    let tie_auth = format!("Bearer {}", tie.key);
    let (status, headers, body) = call(
        &app,
        "GET",
        "/api/v1/api-keys?limit=2",
        &[("Authorization", &tie_auth)],
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let ids: Vec<&str> = body
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, ["id-c", "id-b"]);
    assert_eq!(
        headers.get("x-has-more").and_then(|v| v.to_str().ok()),
        Some("true")
    );

    let (status, headers, body) = call(
        &app,
        "GET",
        "/api/v1/api-keys?limit=0&offset=2",
        &[("Authorization", &tie_auth)],
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body.as_array().unwrap().len(), 1);
    assert_eq!(body[0]["id"], "id-a");
    assert_eq!(
        headers.get("x-has-more").and_then(|v| v.to_str().ok()),
        Some("false")
    );
}
