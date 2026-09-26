#![cfg(feature = "docker-tests")]
//! Regressions from the 2026-09-26 production QA pass, exercised through the real
//! router (auth, rate limiting, error normalization) against a real database.
//!
//! Run without Docker by pointing the suite at local services:
//! `TEST_DATABASE_URL=postgres://$USER@localhost:5432/postgres`
//! `TEST_REDIS_URL=redis://127.0.0.1:6379`

mod common;

use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::Router;
use futures::StreamExt;
use serde_json::{json, Value};
use tower::ServiceExt;

use spooled_backend::api::{router, AppState};
use spooled_backend::cache::RedisCache;
use spooled_backend::config::{RedisSettings, Settings};
use spooled_backend::db::Database;
use spooled_backend::observability::Metrics;

use common::{TestDatabase, TestRedis};

struct Tenant {
    org_id: String,
    key: String,
}

async fn seed_tenant(pool: &sqlx::PgPool, plan: &str) -> Tenant {
    let org_id = uuid::Uuid::new_v4().to_string();
    let slug = format!("qa-{}", &org_id[..8]);
    sqlx::query(
        "INSERT INTO organizations (id, name, slug, plan_tier, billing_email, settings, created_at, updated_at) \
         VALUES ($1, $2, $2, $3, $4, '{}'::JSONB, NOW(), NOW())",
    )
    .bind(&org_id)
    .bind(&slug)
    .bind(plan)
    .bind(format!("{}@example.com", slug))
    .execute(pool)
    .await
    .expect("seed org");

    let key = format!("sp_test_{}", uuid::Uuid::new_v4().simple());
    let hash = bcrypt::hash(&key, 4).expect("hash key");
    sqlx::query(
        "INSERT INTO api_keys (id, organization_id, key_hash, key_prefix, lookup_hash, name, queues, is_active, created_at) \
         VALUES ($1, $2, $3, $4, $5, 'qa', ARRAY[]::TEXT[], TRUE, NOW())",
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

async fn app(db: &TestDatabase, redis: Option<&TestRedis>) -> Router {
    let settings = Settings::load_for_testing();
    let cache = match redis {
        Some(r) => Some(
            RedisCache::connect(&RedisSettings {
                url: r.url.clone(),
                pool_size: 4,
            })
            .await
            .expect("connect redis"),
        ),
        None => None,
    };
    router(AppState::new(
        Database::from_pool(Arc::clone(&db.pool)),
        cache,
        Metrics::new(),
        settings,
    ))
}

async fn call(
    app: &Router,
    method: &str,
    uri: &str,
    bearer: Option<&str>,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let mut req = Request::builder().method(method).uri(uri);
    if let Some(token) = bearer {
        req = req.header("Authorization", format!("Bearer {}", token));
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

// ---------------------------------------------------------------------------
// H1: `permissions` is rejected, not silently dropped.
// ---------------------------------------------------------------------------
#[tokio::test]
async fn api_key_permissions_are_rejected() {
    let db = TestDatabase::new().await;
    let t = seed_tenant(db.pool(), "starter").await;
    let app = app(&db, None).await;

    let (status, body) = call(
        &app,
        "POST",
        "/api/v1/api-keys",
        Some(&t.key),
        Some(json!({"name": "ro", "permissions": ["jobs:read"]})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["code"], "VALIDATION_ERROR");
    assert!(
        body["message"].as_str().unwrap().contains("permissions"),
        "{body}"
    );

    let (status, _) = call(
        &app,
        "POST",
        "/api/v1/api-keys",
        Some(&t.key),
        Some(json!({"name": "plain"})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
}

// ---------------------------------------------------------------------------
// M2 + M3: implicit queues count against the plan cap; queue config defaults
// apply to new jobs.
// ---------------------------------------------------------------------------
#[tokio::test]
async fn implicit_queues_are_capped_and_counted() {
    let db = TestDatabase::new().await;
    let t = seed_tenant(db.pool(), "free").await; // max_queues = 2
    let app = app(&db, None).await;

    for q in ["qa-a", "qa-b"] {
        let (status, body) = call(
            &app,
            "POST",
            "/api/v1/jobs",
            Some(&t.key),
            Some(json!({"queue_name": q, "payload": {}})),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{q}: {body}");
    }

    // Third distinct queue is over the free cap.
    let (status, body) = call(
        &app,
        "POST",
        "/api/v1/jobs",
        Some(&t.key),
        Some(json!({"queue_name": "qa-c", "payload": {}})),
    )
    .await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "{body}");
    assert_eq!(body["resource"], "queues", "{body}");

    // Existing queues keep working.
    let (status, _) = call(
        &app,
        "POST",
        "/api/v1/jobs",
        Some(&t.key),
        Some(json!({"queue_name": "qa-a", "payload": {}})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    // Configuring an implicit queue is not charged a second slot.
    let (status, body) = call(
        &app,
        "PUT",
        "/api/v1/queues/qa-a/config",
        Some(&t.key),
        Some(json!({"max_retries": 2, "default_timeout": 120})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    // Bulk and schedules into a new queue are capped too.
    let (status, _) = call(
        &app,
        "POST",
        "/api/v1/jobs/bulk",
        Some(&t.key),
        Some(json!({"queue_name": "qa-d", "jobs": [{"payload": {}}]})),
    )
    .await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);

    // The meter counts every queue, not just configured ones.
    let (status, usage) = call(
        &app,
        "GET",
        "/api/v1/organizations/usage",
        Some(&t.key),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{usage}");
    assert_eq!(usage["usage"]["queues"]["current"], 2, "{usage}");

    // M3: the config written above now applies to jobs that omit the fields.
    let (status, created) = call(
        &app,
        "POST",
        "/api/v1/jobs",
        Some(&t.key),
        Some(json!({"queue_name": "qa-a", "payload": {}})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let id = created["id"].as_str().unwrap();
    let (_, job) = call(
        &app,
        "GET",
        &format!("/api/v1/jobs/{id}"),
        Some(&t.key),
        None,
    )
    .await;
    assert_eq!(job["max_retries"], 2, "{job}");
    assert_eq!(job["timeout_seconds"], 120, "{job}");

    // Explicit values still win.
    let (_, created) = call(
        &app,
        "POST",
        "/api/v1/jobs",
        Some(&t.key),
        Some(json!({"queue_name": "qa-a", "payload": {}, "max_retries": 7})),
    )
    .await;
    let id = created["id"].as_str().unwrap();
    let (_, job) = call(
        &app,
        "GET",
        &format!("/api/v1/jobs/{id}"),
        Some(&t.key),
        None,
    )
    .await;
    assert_eq!(job["max_retries"], 7);
    assert_eq!(job["timeout_seconds"], 120);
}

#[tokio::test]
async fn queue_count_function_counts_jobs_configs_and_schedules() {
    let db = TestDatabase::new().await;
    let t = seed_tenant(db.pool(), "enterprise").await;
    let pool = db.pool();
    for (i, q) in ["b", "a", "b", "c", "a"].iter().enumerate() {
        sqlx::query("INSERT INTO jobs (id, organization_id, queue_name, status, payload, created_at, updated_at) VALUES ($1, $2, $3, 'completed', '{}'::JSONB, NOW(), NOW())")
            .bind(format!("{}-{}", t.org_id, i))
            .bind(&t.org_id)
            .bind(q)
            .execute(pool)
            .await
            .unwrap();
    }
    sqlx::query("INSERT INTO queue_config (id, organization_id, queue_name, max_retries, default_timeout, enabled, settings, created_at, updated_at) VALUES (gen_random_uuid()::TEXT, $1, 'c', 3, 300, TRUE, '{}'::JSONB, NOW(), NOW())")
        .bind(&t.org_id)
        .execute(pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO queue_config (id, organization_id, queue_name, max_retries, default_timeout, enabled, settings, created_at, updated_at) VALUES (gen_random_uuid()::TEXT, $1, 'd', 3, 300, TRUE, '{}'::JSONB, NOW(), NOW())")
        .bind(&t.org_id)
        .execute(pool)
        .await
        .unwrap();
    let (count,): (i64,) = sqlx::query_as("SELECT get_org_queue_count($1)")
        .bind(&t.org_id)
        .fetch_one(pool)
        .await
        .unwrap();
    assert_eq!(count, 4, "a, b, c (jobs) + c, d (config) = 4 distinct");

    let other = seed_tenant(pool, "free").await;
    let (count,): (i64,) = sqlx::query_as("SELECT get_org_queue_count($1)")
        .bind(&other.org_id)
        .fetch_one(pool)
        .await
        .unwrap();
    assert_eq!(count, 0);
}

// ---------------------------------------------------------------------------
// L9 + L3: queue endpoints on unknown queues; JSON error bodies everywhere.
// ---------------------------------------------------------------------------
#[tokio::test]
async fn queue_endpoints_and_error_bodies() {
    let db = TestDatabase::new().await;
    let t = seed_tenant(db.pool(), "starter").await;
    let app = app(&db, None).await;

    let (status, body) = call(
        &app,
        "POST",
        "/api/v1/queues/qa-nope/pause",
        Some(&t.key),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert_eq!(body["code"], "NOT_FOUND");

    // Implicit queue with finished jobs: config-only delete explains itself.
    let (_, created) = call(
        &app,
        "POST",
        "/api/v1/jobs",
        Some(&t.key),
        Some(json!({"queue_name": "qa-imp", "payload": {}})),
    )
    .await;
    sqlx::query("UPDATE jobs SET status = 'completed' WHERE id = $1")
        .bind(created["id"].as_str().unwrap())
        .execute(db.pool())
        .await
        .unwrap();
    let (status, body) = call(&app, "DELETE", "/api/v1/queues/qa-imp", Some(&t.key), None).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert!(body["message"]
        .as_str()
        .unwrap()
        .contains("delete_jobs=true"));
    // Pausing an implicit queue still works.
    let (status, _) = call(
        &app,
        "POST",
        "/api/v1/queues/qa-imp/pause",
        Some(&t.key),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // Unknown route and wrong method: JSON, not an empty body.
    let (status, body) = call(
        &app,
        "GET",
        "/api/v1/definitely-not-a-route",
        Some(&t.key),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["code"], "NOT_FOUND");
    let (status, body) = call(&app, "DELETE", "/health", None, None).await;
    assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED);
    assert_eq!(body["code"], "METHOD_NOT_ALLOWED");

    // Plain-text handler errors are wrapped.
    let (status, body) = call(
        &app,
        "GET",
        "/api/v1/schedules/00000000-0000-0000-0000-000000000000/history",
        Some(&t.key),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["code"], "NOT_FOUND", "{body}");
    let (status, body) = call(&app, "POST", "/api/v1/schedules", Some(&t.key), Some(json!({"name": "x", "cron_expression": "61 * * * *", "queue_name": "qa-imp", "payload_template": {}}))).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["code"], "BAD_REQUEST", "{body}");
    assert!(body["message"].as_str().unwrap().contains("cron"));

    // axum extractor rejection (missing field on a plain Json extractor route).
    let (status, body) = call(
        &app,
        "POST",
        "/api/v1/billing/portal",
        Some(&t.key),
        Some(json!({})),
    )
    .await;
    assert!(status.is_client_error());
    assert!(
        body["code"].is_string() && body["message"].is_string(),
        "{body}"
    );

    // Missing auth: JSON with code + message.
    let (status, body) = call(&app, "GET", "/api/v1/jobs", None, None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert!(
        body["code"].is_string() && body["message"].is_string(),
        "{body}"
    );
}

// ---------------------------------------------------------------------------
// L4 + L5 + L6: worker outcomes, retry budget, schedule history job status.
// ---------------------------------------------------------------------------
#[tokio::test]
async fn worker_outcomes_retry_budget_and_schedule_history() {
    let db = TestDatabase::new().await;
    let t = seed_tenant(db.pool(), "starter").await;
    let app = app(&db, None).await;

    let (_, created) = call(
        &app,
        "POST",
        "/api/v1/jobs",
        Some(&t.key),
        Some(json!({"queue_name": "qa-w", "payload": {}})),
    )
    .await;
    let job_id = created["id"].as_str().unwrap().to_string();
    let (status, claimed) = call(
        &app,
        "POST",
        "/api/v1/jobs/claim",
        Some(&t.key),
        Some(json!({"queue_name": "qa-w", "worker_id": "w1"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{claimed}");
    let lease_id = claimed["jobs"][0]["lease_id"].as_str().unwrap().to_string();

    // Wrong lease while the lease is live: LEASE_EXPIRED code, honest message.
    let (status, body) = call(
        &app,
        "POST",
        &format!("/api/v1/jobs/{job_id}/complete"),
        Some(&t.key),
        Some(json!({"worker_id": "w1", "lease_id": "not-the-lease"})),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(body["code"], "LEASE_EXPIRED");
    assert!(
        body["message"].as_str().unwrap().contains("does not match"),
        "{body}"
    );

    let (status, _) = call(
        &app,
        "POST",
        &format!("/api/v1/jobs/{job_id}/complete"),
        Some(&t.key),
        Some(json!({"worker_id": "w1", "lease_id": lease_id})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    // Second complete: 409 "already completed", not 404.
    let (status, body) = call(
        &app,
        "POST",
        &format!("/api/v1/jobs/{job_id}/complete"),
        Some(&t.key),
        Some(json!({"worker_id": "w1", "lease_id": lease_id})),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["code"], "CONFLICT");
    assert!(body["message"].as_str().unwrap().contains("completed"));

    // Manual retry of a dead-lettered job restores the retry budget.
    let (_, created) = call(
        &app,
        "POST",
        "/api/v1/jobs",
        Some(&t.key),
        Some(json!({"queue_name": "qa-w", "payload": {}})),
    )
    .await;
    let dlq_id = created["id"].as_str().unwrap().to_string();
    sqlx::query("UPDATE jobs SET status = 'deadletter', retry_count = 3 WHERE id = $1")
        .bind(&dlq_id)
        .execute(db.pool())
        .await
        .unwrap();
    let (status, job) = call(
        &app,
        "POST",
        &format!("/api/v1/jobs/{dlq_id}/retry"),
        Some(&t.key),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{job}");
    assert_eq!(job["retry_count"], 0);
    assert_eq!(job["status"], "pending");

    // Schedule history reports the enqueued job's live status.
    let (status, sched) = call(&app, "POST", "/api/v1/schedules", Some(&t.key), Some(json!({"name": "qa", "cron_expression": "0 0 * * *", "queue_name": "qa-w", "payload_template": {}}))).await;
    assert_eq!(status, StatusCode::CREATED, "{sched}");
    let sched_id = sched["id"].as_str().unwrap();
    let (status, _) = call(
        &app,
        "POST",
        &format!("/api/v1/schedules/{sched_id}/trigger"),
        Some(&t.key),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, history) = call(
        &app,
        "GET",
        &format!("/api/v1/schedules/{sched_id}/history"),
        Some(&t.key),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{history}");
    assert_eq!(history[0]["status"], "completed");
    assert_eq!(history[0]["job_status"], "pending", "{history}");
}

// ---------------------------------------------------------------------------
// M1: logout ends the session even when the refresh token is not sent.
// ---------------------------------------------------------------------------
#[tokio::test]
async fn logout_revokes_the_refresh_token_of_the_session() {
    let db = TestDatabase::new().await;
    let redis = TestRedis::new().await;
    let t = seed_tenant(db.pool(), "enterprise").await;
    let app = app(&db, Some(&redis)).await;

    let (status, login) = call(
        &app,
        "POST",
        "/api/v1/auth/login",
        None,
        Some(json!({"api_key": t.key})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{login}");
    let access = login["access_token"].as_str().unwrap().to_string();
    let refresh = login["refresh_token"].as_str().unwrap().to_string();

    // A second access token from the same session (via refresh) before logout.
    let (status, refreshed) = call(
        &app,
        "POST",
        "/api/v1/auth/refresh",
        None,
        Some(json!({"refresh_token": refresh})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let sibling = refreshed["access_token"].as_str().unwrap().to_string();

    // Logout with NO body.
    let (status, _) = call(&app, "POST", "/api/v1/auth/logout", Some(&access), None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    let (status, _) = call(
        &app,
        "POST",
        "/api/v1/auth/refresh",
        None,
        Some(json!({"refresh_token": refresh})),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::UNAUTHORIZED,
        "refresh token must die with the session"
    );
    let (status, _) = call(&app, "GET", "/api/v1/jobs", Some(&sibling), None).await;
    assert_eq!(
        status,
        StatusCode::UNAUTHORIZED,
        "sibling access token must die too"
    );

    // A fresh login is unaffected.
    let (status, again) = call(
        &app,
        "POST",
        "/api/v1/auth/login",
        None,
        Some(json!({"api_key": t.key})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = call(
        &app,
        "GET",
        "/api/v1/jobs",
        Some(again["access_token"].as_str().unwrap()),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
}

// ---------------------------------------------------------------------------
// H2: the global SSE stream forwards job events as named SSE events.
// ---------------------------------------------------------------------------
#[tokio::test]
async fn global_sse_stream_emits_job_events() {
    let db = TestDatabase::new().await;
    let redis = TestRedis::new().await;
    let t = seed_tenant(db.pool(), "enterprise").await;
    let app = app(&db, Some(&redis)).await;

    let req = Request::builder()
        .uri("/api/v1/events")
        .header("Authorization", format!("Bearer {}", t.key))
        .body(Body::empty())
        .unwrap();
    let res = app.clone().oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let mut stream = res.into_body().into_data_stream();

    // First chunk is the ": connected" comment; give the stream a moment to
    // subscribe to the org channel before producing events.
    let first = tokio::time::timeout(Duration::from_secs(5), stream.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(String::from_utf8_lossy(&first).contains("connected"));
    tokio::time::sleep(Duration::from_millis(500)).await;

    let (status, created) = call(
        &app,
        "POST",
        "/api/v1/jobs",
        Some(&t.key),
        Some(json!({"queue_name": "qa-sse", "payload": {}})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let job_id = created["id"].as_str().unwrap().to_string();

    let mut seen = String::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(8);
    while !seen.contains("event: job.created") {
        let chunk = tokio::time::timeout_at(deadline, stream.next())
            .await
            .expect("job.created within 8s")
            .unwrap()
            .unwrap();
        seen.push_str(&String::from_utf8_lossy(&chunk));
    }
    assert!(seen.contains(&job_id), "{seen}");
    assert!(seen.contains("\"type\":\"JobCreated\""), "{seen}");

    // Complete it and expect job.completed with a real duration field.
    let (_, claimed) = call(
        &app,
        "POST",
        "/api/v1/jobs/claim",
        Some(&t.key),
        Some(json!({"queue_name": "qa-sse", "worker_id": "w"})),
    )
    .await;
    let lease = claimed["jobs"][0]["lease_id"].as_str().unwrap().to_string();
    let (status, _) = call(
        &app,
        "POST",
        &format!("/api/v1/jobs/{job_id}/complete"),
        Some(&t.key),
        Some(json!({"worker_id": "w", "lease_id": lease})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    while !seen.contains("event: job.completed") {
        let chunk = tokio::time::timeout_at(deadline, stream.next())
            .await
            .expect("job.completed within 8s")
            .unwrap()
            .unwrap();
        seen.push_str(&String::from_utf8_lossy(&chunk));
    }
    assert!(seen.contains("duration_ms"));

    // Out-of-scope queue filter is refused.
    let (status, body) = call(
        &app,
        "GET",
        "/api/v1/events?queue=bad%20name",
        Some(&t.key),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
}

// ---------------------------------------------------------------------------
// L16: admin API refuses duplicate billing emails and unknown sort fields.
// ---------------------------------------------------------------------------
#[tokio::test]
async fn admin_rejects_duplicate_billing_email_and_bad_sort() {
    let db = TestDatabase::new().await;
    let mut settings = Settings::load_for_testing();
    settings.registration.admin_api_key = Some("qa-admin-key-for-tests".to_string());
    let app = router(AppState::new(
        Database::from_pool(Arc::clone(&db.pool)),
        None,
        Metrics::new(),
        settings,
    ));

    let admin = |method: &str, uri: &str, body: Option<Value>| {
        let mut req = Request::builder()
            .method(method)
            .uri(uri)
            .header("X-Admin-Key", "qa-admin-key-for-tests");
        let req = match body {
            Some(b) => {
                req = req.header("Content-Type", "application/json");
                req.body(Body::from(b.to_string())).unwrap()
            }
            None => req.body(Body::empty()).unwrap(),
        };
        let app = app.clone();
        async move {
            let res = app.oneshot(req).await.unwrap();
            let status = res.status();
            let bytes = axum::body::to_bytes(res.into_body(), 1 << 20)
                .await
                .unwrap();
            (
                status,
                serde_json::from_slice::<Value>(&bytes).unwrap_or(Value::Null),
            )
        }
    };

    let (status, first) = admin(
        "POST",
        "/api/v1/admin/organizations",
        Some(json!({"name": "QA One", "slug": "qa-dup-one", "billing_email": "dup@example.com"})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{first}");

    // Same email, different case: still the same inbox, so still a duplicate.
    let (status, body) = admin(
        "POST",
        "/api/v1/admin/organizations",
        Some(json!({"name": "QA Two", "slug": "qa-dup-two", "billing_email": "Dup@Example.com"})),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");

    let (status, body) = admin("GET", "/api/v1/admin/organizations?sort_by=bogus", None).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["code"], "VALIDATION_ERROR");
    let (status, _) = admin(
        "GET",
        "/api/v1/admin/organizations?sort_by=name&sort_order=asc",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
}

// ---------------------------------------------------------------------------
// Heartbeat without lease_duration_secs uses the default lease instead of 422;
// a payload-too-large error names the plan to upgrade to.
// ---------------------------------------------------------------------------
#[tokio::test]
async fn heartbeat_default_lease_and_payload_upgrade_hint() {
    let db = TestDatabase::new().await;
    let t = seed_tenant(db.pool(), "free").await;
    let app = app(&db, None).await;

    let (_, created) = call(
        &app,
        "POST",
        "/api/v1/jobs",
        Some(&t.key),
        Some(json!({"queue_name": "qa-hb", "payload": {}})),
    )
    .await;
    let job_id = created["id"].as_str().unwrap().to_string();
    let (_, claimed) = call(
        &app,
        "POST",
        "/api/v1/jobs/claim",
        Some(&t.key),
        Some(json!({"queue_name": "qa-hb", "worker_id": "w1"})),
    )
    .await;
    let lease_id = claimed["jobs"][0]["lease_id"].as_str().unwrap().to_string();

    let (status, body) = call(
        &app,
        "POST",
        &format!("/api/v1/jobs/{job_id}/heartbeat"),
        Some(&t.key),
        Some(json!({"worker_id": "w1", "lease_id": lease_id})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (expires,): (chrono::DateTime<chrono::Utc>,) =
        sqlx::query_as("SELECT lease_expires_at FROM jobs WHERE id = $1")
            .bind(&job_id)
            .fetch_one(db.pool())
            .await
            .unwrap();
    let secs = (expires - chrono::Utc::now()).num_seconds();
    assert!(
        (20..=31).contains(&secs),
        "default lease should be ~30s, got {secs}"
    );

    // Explicit out-of-range values are still rejected.
    let (status, _) = call(
        &app,
        "POST",
        &format!("/api/v1/jobs/{job_id}/heartbeat"),
        Some(&t.key),
        Some(json!({"worker_id": "w1", "lease_id": lease_id, "lease_duration_secs": 1})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // Free plan payload cap is 64 KiB.
    let big = "x".repeat(70_000);
    let (status, body) = call(
        &app,
        "POST",
        "/api/v1/jobs",
        Some(&t.key),
        Some(json!({"queue_name": "qa-hb", "payload": {"blob": big}})),
    )
    .await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE, "{body}");
    assert_eq!(body["code"], "PAYLOAD_TOO_LARGE");
    assert_eq!(body["upgrade_to"], "starter", "{body}");
}
