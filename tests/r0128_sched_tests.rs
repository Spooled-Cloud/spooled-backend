#![cfg(feature = "docker-tests")]
//! Schedule payload cap, cron slot, delete trigger and expiry dependents.

mod common;

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::Router;
use serde_json::{json, Value};
use tower::ServiceExt;

use spooled_backend::api::{router, AppState};
use spooled_backend::config::Settings;
use spooled_backend::db::Database;
use spooled_backend::observability::Metrics;
use spooled_backend::outgoing_webhooks::service::OutgoingWebhookService;
use spooled_backend::scheduler::Scheduler;

use common::TestDatabase;

struct Tenant {
    org_id: String,
    key: String,
}

async fn seed_tenant(pool: &sqlx::PgPool, plan: &str) -> Tenant {
    let org_id = uuid::Uuid::new_v4().to_string();
    let slug = format!("r28s-{}", &org_id[..8]);
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
    let hash = bcrypt::hash(&key, 4).expect("hash key");
    sqlx::query(
        "INSERT INTO api_keys (id, organization_id, key_hash, key_prefix, lookup_hash, name, queues, is_active, created_at) \
         VALUES ($1, $2, $3, $4, $5, 'r28s', ARRAY[]::TEXT[], TRUE, NOW())",
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
    router(AppState::new(
        Database::from_pool(Arc::clone(&db.pool)),
        None,
        Metrics::new(),
        Settings::load_for_testing(),
    ))
}

fn scheduler(db: &TestDatabase) -> Scheduler {
    Scheduler::new(
        Arc::clone(&db.pool),
        None,
        Arc::new(Metrics::new()),
        Arc::new(OutgoingWebhookService::new((*db.pool).clone(), 1, 4)),
    )
}

async fn call(
    app: &Router,
    method: &str,
    uri: &str,
    key: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let req = Request::builder()
        .method(method)
        .uri(uri)
        .header("Authorization", format!("Bearer {key}"));
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

async fn insert_job(pool: &sqlx::PgPool, org_id: &str, status: &str) -> String {
    let id = uuid::Uuid::new_v4().to_string();
    sqlx::query(
        "INSERT INTO jobs (id, organization_id, queue_name, payload, status, created_at, updated_at) \
         VALUES ($1, $2, 'default', '{}'::JSONB, $3, NOW(), NOW())",
    )
    .bind(&id)
    .bind(org_id)
    .bind(status)
    .execute(pool)
    .await
    .expect("seed job");
    id
}

async fn insert_waiting(pool: &sqlx::PgPool, org_id: &str, mode: &str) -> String {
    let id = uuid::Uuid::new_v4().to_string();
    sqlx::query(
        "INSERT INTO jobs (id, organization_id, queue_name, payload, status, dependency_mode, dependencies_met, created_at, updated_at) \
         VALUES ($1, $2, 'default', '{}'::JSONB, 'pending', $3, FALSE, NOW(), NOW())",
    )
    .bind(&id)
    .bind(org_id)
    .bind(mode)
    .execute(pool)
    .await
    .expect("seed child");
    id
}

/// SCHED-01: a free-plan template between 64KB and 1MB is rejected on update.
/// The stored template and next fire stay put. A small template still updates.
#[tokio::test]
async fn schedule_update_rejects_a_payload_over_the_plan() {
    let db = TestDatabase::new().await;
    let t = seed_tenant(db.pool(), "free").await;
    let app = app(&db);
    let id = uuid::Uuid::new_v4().to_string();
    sqlx::query(
        "INSERT INTO schedules (id, organization_id, name, cron_expression, timezone, queue_name, payload_template, is_active, next_run_at) \
         VALUES ($1, $2, 'daily', '0 0 * * *', 'UTC', 'schedq', '{\"keep\":true}'::JSONB, TRUE, TIMESTAMPTZ '2030-01-01 00:00:00+00')",
    )
    .bind(&id)
    .bind(&t.org_id)
    .execute(db.pool())
    .await
    .unwrap();

    let big = "x".repeat(70_000);
    let (status, body) = call(
        &app,
        "PUT",
        &format!("/api/v1/schedules/{id}"),
        &t.key,
        Some(json!({"payload_template": {"p": big}})),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert_eq!(body["code"], "ACCESS_DENIED");
    let message = body["message"].as_str().unwrap_or("");
    assert!(
        message.contains("payload size limit"),
        "expected the plan payload error, got {body}"
    );

    let (payload, next_run): (Value, Option<chrono::DateTime<chrono::Utc>>) =
        sqlx::query_as("SELECT payload_template, next_run_at FROM schedules WHERE id = $1")
            .bind(&id)
            .fetch_one(db.pool())
            .await
            .unwrap();
    assert_eq!(payload, json!({"keep": true}));
    assert_eq!(
        next_run,
        Some(
            chrono::DateTime::parse_from_rfc3339("2030-01-01T00:00:00Z")
                .unwrap()
                .with_timezone(&chrono::Utc)
        )
    );

    let (status, body) = call(
        &app,
        "PUT",
        &format!("/api/v1/schedules/{id}"),
        &t.key,
        Some(json!({"payload_template": {"ok": true}})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let stored: Value = sqlx::query_scalar("SELECT payload_template FROM schedules WHERE id = $1")
        .bind(&id)
        .fetch_one(db.pool())
        .await
        .unwrap();
    assert_eq!(stored, json!({"ok": true}));
}

/// SCHED-02: a pass that outlasts the precomputed slot must store a next fire
/// taken after the insert, or the next pass enqueues the same occurrence again.
#[tokio::test]
async fn cron_success_stores_a_next_run_after_a_slow_insert() {
    let db = TestDatabase::new().await;
    let t = seed_tenant(db.pool(), "starter").await;
    let id = uuid::Uuid::new_v4().to_string();
    sqlx::query(
        "INSERT INTO schedules (id, organization_id, name, cron_expression, timezone, queue_name, payload_template, is_active, next_run_at) \
         VALUES ($1, $2, 'each-second', '* * * * * *', 'UTC', 'cronq', '{}'::JSONB, TRUE, NOW())",
    )
    .bind(&id)
    .bind(&t.org_id)
    .execute(db.pool())
    .await
    .unwrap();

    // The sleep is inside the insert, so a next_run taken from the batch-start
    // clock is already behind the timestamp the trigger records.
    sqlx::query(
        r#"
        CREATE FUNCTION r0128_slow_cron_insert() RETURNS trigger LANGUAGE plpgsql AS $fn$
        BEGIN
            PERFORM pg_sleep(2);
            NEW.payload = jsonb_set(
                COALESCE(NEW.payload, '{}'::jsonb),
                '{slept_at}',
                to_jsonb(EXTRACT(EPOCH FROM clock_timestamp()))
            );
            RETURN NEW;
        END
        $fn$
        "#,
    )
    .execute(db.pool())
    .await
    .unwrap();
    sqlx::query(
        "CREATE TRIGGER r0128_slow_cron_insert BEFORE INSERT ON jobs \
         FOR EACH ROW EXECUTE FUNCTION r0128_slow_cron_insert()",
    )
    .execute(db.pool())
    .await
    .unwrap();

    scheduler(&db).process_cron_schedules().await.expect("cron");

    let jobs: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM jobs WHERE organization_id = $1")
        .bind(&t.org_id)
        .fetch_one(db.pool())
        .await
        .unwrap();
    assert_eq!(jobs, 1);
    let ahead: Option<bool> = sqlx::query_scalar(
        "SELECT s.next_run_at > to_timestamp((j.payload->>'slept_at')::double precision) \
         FROM schedules s \
         JOIN jobs j ON j.organization_id = s.organization_id \
         WHERE s.id = $1",
    )
    .bind(&id)
    .fetch_one(db.pool())
    .await
    .unwrap();
    assert_eq!(ahead, Some(true), "next_run_at must be after the insert");
}

/// SCHED-03: deleting one branch of an any-join leaves the child pending.
/// An all-mode child of that same job is cancelled.
#[tokio::test]
async fn deleting_one_any_mode_alternative_leaves_the_child() {
    let db = TestDatabase::new().await;
    let t = seed_tenant(db.pool(), "starter").await;
    let going = insert_job(db.pool(), &t.org_id, "pending").await;
    let staying = insert_job(db.pool(), &t.org_id, "pending").await;
    let any_child = insert_waiting(db.pool(), &t.org_id, "any").await;
    let all_child = insert_waiting(db.pool(), &t.org_id, "all").await;
    sqlx::query(
        "INSERT INTO job_dependencies (job_id, depends_on_job_id) VALUES ($1, $2), ($1, $3), ($4, $2)",
    )
    .bind(&any_child)
    .bind(&going)
    .bind(&staying)
    .bind(&all_child)
    .execute(db.pool())
    .await
    .unwrap();

    sqlx::query("DELETE FROM jobs WHERE id = $1")
        .bind(&going)
        .execute(db.pool())
        .await
        .unwrap();

    let any_status: String = sqlx::query_scalar("SELECT status FROM jobs WHERE id = $1")
        .bind(&any_child)
        .fetch_one(db.pool())
        .await
        .unwrap();
    let (all_status, all_error): (String, Option<String>) =
        sqlx::query_as("SELECT status, last_error FROM jobs WHERE id = $1")
            .bind(&all_child)
            .fetch_one(db.pool())
            .await
            .unwrap();
    assert_eq!(any_status, "pending");
    assert_eq!(all_status, "cancelled");
    assert!(
        all_error.unwrap_or_default().contains("deleted"),
        "all-mode child records the delete"
    );
}

/// SCHED-04: an expired dependency cancels an all-mode child, with a distinct
/// last_error. An any-mode child that still has a live alternative stays pending.
#[tokio::test]
async fn expiry_cancels_a_blocked_dependency_child() {
    let db = TestDatabase::new().await;
    let t = seed_tenant(db.pool(), "starter").await;
    let expired = uuid::Uuid::new_v4().to_string();
    sqlx::query(
        "INSERT INTO jobs (id, organization_id, queue_name, payload, status, expires_at, created_at, updated_at) \
         VALUES ($1, $2, 'default', '{}'::JSONB, 'pending', NOW() - INTERVAL '1 minute', NOW(), NOW())",
    )
    .bind(&expired)
    .bind(&t.org_id)
    .execute(db.pool())
    .await
    .unwrap();
    let live = insert_job(db.pool(), &t.org_id, "pending").await;
    let all_child = insert_waiting(db.pool(), &t.org_id, "all").await;
    let any_child = insert_waiting(db.pool(), &t.org_id, "any").await;
    sqlx::query(
        "INSERT INTO job_dependencies (job_id, depends_on_job_id) VALUES ($1, $2), ($3, $2), ($3, $4)",
    )
    .bind(&all_child)
    .bind(&expired)
    .bind(&any_child)
    .bind(&live)
    .execute(db.pool())
    .await
    .unwrap();

    scheduler(&db)
        .cleanup_stale_workers()
        .await
        .expect("expiry sweep");

    let gone: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM jobs WHERE id = $1")
        .bind(&expired)
        .fetch_one(db.pool())
        .await
        .unwrap();
    assert_eq!(gone, 0);
    let (all_status, all_error): (String, Option<String>) =
        sqlx::query_as("SELECT status, last_error FROM jobs WHERE id = $1")
            .bind(&all_child)
            .fetch_one(db.pool())
            .await
            .unwrap();
    let any_status: String = sqlx::query_scalar("SELECT status FROM jobs WHERE id = $1")
        .bind(&any_child)
        .fetch_one(db.pool())
        .await
        .unwrap();
    assert_eq!(all_status, "cancelled");
    assert!(
        all_error.unwrap_or_default().contains("expired"),
        "the expiry reason is not the delete-trigger text"
    );
    assert_eq!(any_status, "pending");
}
