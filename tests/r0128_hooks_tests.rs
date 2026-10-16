#![cfg(feature = "docker-tests")]
//! HOOK-02 .. HOOK-05. Outbound retry overlap, inbound custom-webhook validation,
//! and the completion-webhook body the guides actually sign.

mod common;

use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::Router;
use serde_json::{json, Value};
use tower::ServiceExt;
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

use spooled_backend::api::{router, AppState};
use spooled_backend::config::Settings;
use spooled_backend::db::Database;
use spooled_backend::observability::Metrics;
use spooled_backend::outgoing_webhooks::service::OutgoingWebhookService;

use common::TestDatabase;

struct Tenant {
    org_id: String,
    key: String,
}

async fn seed_tenant(pool: &sqlx::PgPool) -> Tenant {
    let org_id = uuid::Uuid::new_v4().to_string();
    let slug = format!("hk-{}", &org_id[..8]);
    sqlx::query(
        "INSERT INTO organizations (id, name, slug, plan_tier, billing_email, billing_email_verified_at, settings, created_at, updated_at) \
         VALUES ($1, $2, $2, 'starter', $3, NOW(), '{\"webhook_token\":\"whk_r0128\"}'::JSONB, NOW(), NOW())",
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
         VALUES ($1, $2, $3, $4, $5, 'hk', ARRAY[]::TEXT[], TRUE, NOW())",
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

fn app_with(db: &TestDatabase, webhooks: Arc<OutgoingWebhookService>) -> Router {
    let mut state = AppState::new(
        Database::from_pool(Arc::clone(&db.pool)),
        None,
        Metrics::new(),
        Settings::load_for_testing(),
    );
    state.outgoing_webhooks = webhooks;
    router(state)
}

fn app(db: &TestDatabase) -> Router {
    router(AppState::new(
        Database::from_pool(Arc::clone(&db.pool)),
        None,
        Metrics::new(),
        Settings::load_for_testing(),
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

/// HOOK-02: a manual retry and the automatic retry of the same delivery must
/// not both POST, and the loser must not write the row.
#[tokio::test]
async fn hook02_manual_retry_does_not_overlap_the_automatic_retry() {
    let db = TestDatabase::new().await;
    let server = MockServer::start().await;
    // Priority 1 is checked first. One fast 500, then a slow 200, so the
    // winner is still on the wire when the other retry and the backoff arrive.
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(500))
        .with_priority(1)
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_secs(2)))
        .with_priority(2)
        .mount(&server)
        .await;

    let t = seed_tenant(db.pool()).await;
    let webhook_id = uuid::Uuid::new_v4().to_string();
    sqlx::query(
        "INSERT INTO outgoing_webhooks (id, organization_id, name, url, secret, events, enabled) \
         VALUES ($1, $2, 'hook', $3, 'sec', ARRAY['job.created']::TEXT[], TRUE)",
    )
    .bind(&webhook_id)
    .bind(&t.org_id)
    .bind(server.uri())
    .execute(db.pool())
    .await
    .unwrap();

    // More than one attempt, so the first 500 schedules the automatic retry.
    let svc = Arc::new(OutgoingWebhookService::new((*db.pool).clone(), 4, 4));
    let app = app_with(&db, Arc::clone(&svc));
    svc.spawn_dispatch(
        t.org_id.clone(),
        "job.created".to_string(),
        "job-1".to_string(),
        json!({"job_id": "job-1", "queue_name": "default"}),
    );

    let mut delivery_id = None;
    for _ in 0..100 {
        let row: Option<(String, String, i32)> = sqlx::query_as(
            "SELECT id, status, attempts FROM outgoing_webhook_deliveries WHERE webhook_id = $1",
        )
        .bind(&webhook_id)
        .fetch_optional(db.pool())
        .await
        .unwrap();
        if let Some((id, status, attempts)) = row {
            if status == "failed" && attempts >= 1 {
                delivery_id = Some(id);
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let delivery_id = match delivery_id {
        Some(id) => id,
        None => {
            let rows: Vec<(String, String, i32, Option<String>)> = sqlx::query_as(
                "SELECT id, status, attempts, error FROM outgoing_webhook_deliveries WHERE webhook_id = $1",
            )
            .bind(&webhook_id)
            .fetch_all(db.pool())
            .await
            .unwrap();
            let requests = server.received_requests().await.unwrap().len();
            panic!("first attempt never failed; rows={rows:?} requests={requests}");
        }
    };

    let path = format!("/api/v1/outgoing-webhooks/{webhook_id}/retry/{delivery_id}");
    let auth = bearer(&t.key);
    let headers = [("Authorization", auth.as_str())];
    let (first, second) = tokio::join!(
        call(&app, "POST", &path, &headers, None),
        call(&app, "POST", &path, &headers, None),
    );
    let statuses = [first.0, second.0];
    assert!(
        statuses.contains(&StatusCode::OK),
        "one retry should deliver: {first:?} {second:?}"
    );
    assert!(
        statuses.contains(&StatusCode::CONFLICT),
        "the other retry must lose the claim: {first:?} {second:?}"
    );
    let loser = if first.0 == StatusCode::CONFLICT {
        &first.1
    } else {
        &second.1
    };
    assert_eq!(loser["code"], "CONFLICT");
    assert!(
        loser["message"]
            .as_str()
            .unwrap_or_default()
            .contains("already in progress"),
        "{loser}"
    );

    let requests = server.received_requests().await.unwrap().len();
    assert_eq!(requests, 2, "the 500 and exactly one retry POST");

    let (status, attempts, status_code): (String, i32, Option<i32>) = sqlx::query_as(
        "SELECT status, attempts, status_code FROM outgoing_webhook_deliveries WHERE id = $1",
    )
    .bind(&delivery_id)
    .fetch_one(db.pool())
    .await
    .unwrap();
    assert_eq!(status, "success");
    assert_eq!(attempts, 2, "only the claim that sent incremented attempts");
    assert_eq!(status_code, Some(200));
}

async fn custom(app: &Router, org_id: &str, body: Value) -> (StatusCode, Value) {
    call(
        app,
        "POST",
        &format!("/api/v1/webhooks/{org_id}/custom"),
        &[("X-Webhook-Token", "whk_r0128")],
        Some(body),
    )
    .await
}

/// HOOK-03: U+0000 in an inbound custom payload is a validation 400, not a 500
/// from the jsonb insert.
#[tokio::test]
async fn hook03_nul_in_custom_webhook_payload_is_400() {
    let db = TestDatabase::new().await;
    let app = app(&db);
    let t = seed_tenant(db.pool()).await;

    let (status, body) = custom(
        &app,
        &t.org_id,
        json!({"queue_name": "inbox", "payload": {"note": "a\u{0000}b"}}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    let text = body.to_string();
    assert!(
        text.contains("payload_contains_nul"),
        "expected the jobs NUL code, got {body}"
    );
    let jobs: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM jobs WHERE organization_id = $1")
        .bind(&t.org_id)
        .fetch_one(db.pool())
        .await
        .unwrap();
    assert_eq!(jobs, 0, "a rejected payload must not insert a job");
}

/// HOOK-04: the inbound webhook accepts the queue names POST /jobs accepts,
/// including `default` and a name containing `..`. A leading dot is still invalid.
#[tokio::test]
async fn hook04_custom_webhook_queue_names_match_post_jobs() {
    let db = TestDatabase::new().await;
    let app = app(&db);
    let t = seed_tenant(db.pool()).await;

    for queue in ["default", "a..b"] {
        let (status, body) = custom(
            &app,
            &t.org_id,
            json!({"queue_name": queue, "payload": {"n": queue}}),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{queue}: {body}");
        assert_eq!(body["queue_name"], queue);
        assert!(body["job_id"].as_str().is_some(), "{body}");
    }

    let (status, body) = custom(
        &app,
        &t.org_id,
        json!({"queue_name": ".hidden", "payload": {}}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_ne!(status, StatusCode::INTERNAL_SERVER_ERROR);
}

/// HOOK-05: the guide sample and OpenAPI describe the body that is signed.
#[test]
fn hook05_completion_webhook_docs_match_the_signed_body() {
    let guide = include_str!("../docs/guides/webhooks.md");
    let marker = guide
        .find("```json\n{\n  \"id\": \"dlv_xxx\"")
        .expect("completion sample");
    let block = &guide[marker..];
    let end = block.find("\n```").expect("end of sample");
    let sample = &block[..end];
    assert!(sample.contains("\"event\": \"job.completed\""));
    assert!(sample.contains("\"id\""));
    assert!(
        !sample.contains("completed_at"),
        "the signed body has no data.completed_at: {sample}"
    );

    let openapi = include_str!("../docs/openapi.yaml");
    assert!(openapi.contains("{id, event, created_at, data}"));
    assert!(openapi
        .contains("{id, event, created_at, data: {job_id, queue_name, status, result | error}}"));
}
