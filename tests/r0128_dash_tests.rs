#![cfg(feature = "docker-tests")]
//! Dashboard overview: scheduled jobs, worker totals, empty queue config.

mod common;

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::Router;
use serde_json::Value;
use tower::ServiceExt;

use spooled_backend::api::{router, AppState};
use spooled_backend::config::Settings;
use spooled_backend::db::Database;
use spooled_backend::observability::Metrics;

use common::TestDatabase;

struct Tenant {
    org_id: String,
    key: String,
}

async fn seed_tenant(pool: &sqlx::PgPool) -> Tenant {
    let org_id = uuid::Uuid::new_v4().to_string();
    let slug = format!("r28d-{}", &org_id[..8]);
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
         VALUES ($1, $2, $3, $4, $5, 'r28d', ARRAY[]::TEXT[], TRUE, NOW())",
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

async fn dashboard(db: &TestDatabase, key: &str) -> Value {
    let app = app(db);
    let req = Request::builder()
        .method("GET")
        .uri("/api/v1/dashboard")
        .header("Authorization", format!("Bearer {key}"))
        .body(Body::empty())
        .unwrap();
    let res = app.oneshot(req).await.expect("request");
    let status = res.status();
    let bytes = axum::body::to_bytes(res.into_body(), 1 << 20)
        .await
        .unwrap();
    let body: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    assert_eq!(status, StatusCode::OK, "{body}");
    body
}

fn queue_named<'a>(body: &'a Value, name: &str) -> &'a Value {
    body["queues"]
        .as_array()
        .and_then(|rows| rows.iter().find(|row| row["name"] == name))
        .unwrap_or_else(|| panic!("queue {name} missing from {}", body["queues"]))
}

/// DASH-29: a job waiting out backoff is `scheduled`, not added to `pending`.
/// The other job counters stay.
#[tokio::test]
async fn dashboard_job_counts_keep_scheduled_apart_from_pending() {
    let db = TestDatabase::new().await;
    let t = seed_tenant(db.pool()).await;
    for status in ["pending", "scheduled"] {
        sqlx::query(
            "INSERT INTO jobs (id, organization_id, queue_name, payload, status, created_at, updated_at) \
             VALUES ($1, $2, 'implicit-q', '{}'::JSONB, $3, NOW(), NOW())",
        )
        .bind(uuid::Uuid::new_v4().to_string())
        .bind(&t.org_id)
        .bind(status)
        .execute(db.pool())
        .await
        .unwrap();
    }

    let body = dashboard(&db, &t.key).await;
    assert_eq!(body["jobs"]["pending"].as_i64(), Some(1));
    assert_eq!(body["jobs"]["scheduled"].as_i64(), Some(1));
    assert_eq!(body["jobs"]["processing"].as_i64(), Some(0));
    assert_eq!(body["jobs"]["completed_24h"].as_i64(), Some(0));
    assert_eq!(body["jobs"]["failed_24h"].as_i64(), Some(0));
    assert_eq!(body["jobs"]["deadletter"].as_i64(), Some(0));
    assert_eq!(body["jobs"]["total"].as_i64(), Some(2));
}

/// DASH-32: worker totals count a heartbeat from the last two minutes (the
/// reaper's window) and an offline worker. The old one-minute filter dropped both.
#[tokio::test]
async fn dashboard_worker_totals_include_recent_and_offline() {
    let db = TestDatabase::new().await;
    let t = seed_tenant(db.pool()).await;
    // Static SQL: sqlx::query refuses a formatted string, and the dashboard
    // compares last_heartbeat to the database clock.
    let heartbeats = [
        (
            "fresh",
            "healthy",
            "INSERT INTO workers (id, organization_id, queue_name, hostname, status, last_heartbeat) \
             VALUES ($1, $2, 'dash', $3, $4, NOW())",
        ),
        (
            "recent",
            "healthy",
            "INSERT INTO workers (id, organization_id, queue_name, hostname, status, last_heartbeat) \
             VALUES ($1, $2, 'dash', $3, $4, NOW() - INTERVAL '90 seconds')",
        ),
        (
            "gone",
            "offline",
            "INSERT INTO workers (id, organization_id, queue_name, hostname, status, last_heartbeat) \
             VALUES ($1, $2, 'dash', $3, $4, NOW() - INTERVAL '3 minutes')",
        ),
    ];
    for (name, status, sql) in heartbeats {
        sqlx::query(sql)
            .bind(uuid::Uuid::new_v4().to_string())
            .bind(&t.org_id)
            .bind(name)
            .bind(status)
            .execute(db.pool())
            .await
            .unwrap();
    }

    let body = dashboard(&db, &t.key).await;
    assert_eq!(body["workers"]["total"].as_i64(), Some(3));
    assert_eq!(body["workers"]["healthy"].as_i64(), Some(2));
    assert_eq!(body["workers"]["unhealthy"].as_i64(), Some(1));
}

/// DASH-35: a queue_config row with no jobs, including a paused one, is in the
/// overview. A queue that exists only as job rows is still listed.
#[tokio::test]
async fn dashboard_queues_include_config_rows_with_no_jobs() {
    let db = TestDatabase::new().await;
    let t = seed_tenant(db.pool()).await;
    for (name, enabled) in [("paused-empty", false), ("enabled-empty", true)] {
        sqlx::query(
            "INSERT INTO queue_config (organization_id, queue_name, enabled) VALUES ($1, $2, $3)",
        )
        .bind(&t.org_id)
        .bind(name)
        .bind(enabled)
        .execute(db.pool())
        .await
        .unwrap();
    }
    sqlx::query(
        "INSERT INTO jobs (id, organization_id, queue_name, payload, status, created_at, updated_at) \
         VALUES ($1, $2, 'implicit-q', '{}'::JSONB, 'pending', NOW(), NOW())",
    )
    .bind(uuid::Uuid::new_v4().to_string())
    .bind(&t.org_id)
    .execute(db.pool())
    .await
    .unwrap();

    let body = dashboard(&db, &t.key).await;
    let paused = queue_named(&body, "paused-empty");
    assert_eq!(paused["paused"].as_bool(), Some(true));
    assert_eq!(paused["pending"].as_i64(), Some(0));
    let enabled = queue_named(&body, "enabled-empty");
    assert_eq!(enabled["paused"].as_bool(), Some(false));
    assert_eq!(enabled["pending"].as_i64(), Some(0));
    let implicit = queue_named(&body, "implicit-q");
    assert_eq!(implicit["paused"].as_bool(), Some(false));
    assert_eq!(implicit["pending"].as_i64(), Some(1));
}
