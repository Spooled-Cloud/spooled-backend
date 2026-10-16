#![cfg(feature = "docker-tests")]
//! Workflow retry, dependency mode, expiry and progress (FLOW-01..04, FLOW-07, DASH-31).

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

async fn seed_tenant(pool: &sqlx::PgPool) -> Tenant {
    let org_id = uuid::Uuid::new_v4().to_string();
    let slug = format!("r28f-{}", &org_id[..8]);
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
         VALUES ($1, $2, $3, $4, $5, 'r28f', ARRAY[]::TEXT[], TRUE, NOW())",
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

fn job(key: &str, extra: Value) -> Value {
    let mut body = json!({
        "key": key,
        "queue_name": "default",
        "payload": {"n": 1},
    });
    if let (Value::Object(dst), Value::Object(src)) = (&mut body, extra) {
        for (k, v) in src {
            dst.insert(k, v);
        }
    }
    body
}

async fn insert_workflow(pool: &sqlx::PgPool, org_id: &str, status: &str, total: i32) -> String {
    let id = uuid::Uuid::new_v4().to_string();
    sqlx::query(
        "INSERT INTO workflows (id, organization_id, name, status, total_jobs, created_at) \
         VALUES ($1, $2, 'wf', $3, $4, NOW())",
    )
    .bind(&id)
    .bind(org_id)
    .bind(status)
    .bind(total)
    .execute(pool)
    .await
    .expect("seed workflow");
    id
}

async fn job_state(pool: &sqlx::PgPool, id: &str) -> (String, Option<String>, Option<bool>) {
    sqlx::query_as("SELECT status, last_error, dependencies_met FROM jobs WHERE id = $1")
        .bind(id)
        .fetch_one(pool)
        .await
        .expect("job row")
}

/// FLOW-01: a past expires_at is a validation error, not a workflow that the
/// expiry sweep then hollows out.
#[tokio::test]
async fn workflow_create_rejects_a_past_expires_at() {
    let db = TestDatabase::new().await;
    let t = seed_tenant(db.pool()).await;
    let app = app(&db);
    let (status, body) = call(
        &app,
        "POST",
        "/api/v1/workflows",
        &t.key,
        Some(json!({
            "name": "expired",
            "jobs": [job("step", json!({"expires_at": "2020-01-01T00:00:00Z"}))]
        })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["code"], "VALIDATION_ERROR");
    assert!(
        body["message"]
            .as_str()
            .unwrap_or("")
            .contains("expires_at must be in the future"),
        "{body}"
    );
    let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM workflows WHERE organization_id = $1")
        .bind(&t.org_id)
        .fetch_one(db.pool())
        .await
        .unwrap();
    assert_eq!(n, 0);
}

/// FLOW-01: deleting the last expired job used to leave the workflow pending,
/// still holding a max_workflows slot.
#[tokio::test]
async fn expiry_of_the_only_job_cancels_the_workflow() {
    let db = TestDatabase::new().await;
    let t = seed_tenant(db.pool()).await;
    let wf = insert_workflow(db.pool(), &t.org_id, "pending", 1).await;
    let job_id = uuid::Uuid::new_v4().to_string();
    sqlx::query(
        "INSERT INTO jobs (id, organization_id, queue_name, payload, status, workflow_id, expires_at, created_at, updated_at) \
         VALUES ($1, $2, 'default', '{}'::JSONB, 'pending', $3, NOW() - INTERVAL '1 minute', NOW(), NOW())",
    )
    .bind(&job_id)
    .bind(&t.org_id)
    .bind(&wf)
    .execute(db.pool())
    .await
    .unwrap();

    scheduler(&db)
        .cleanup_stale_workers()
        .await
        .expect("expiry sweep");

    let left: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM jobs WHERE id = $1")
        .bind(&job_id)
        .fetch_one(db.pool())
        .await
        .unwrap();
    assert_eq!(left, 0, "the expired job is deleted");
    let status: String = sqlx::query_scalar("SELECT status FROM workflows WHERE id = $1")
        .bind(&wf)
        .fetch_one(db.pool())
        .await
        .unwrap();
    assert_eq!(status, "cancelled");
}

/// FLOW-02: an any-mode child with a live alternative is not cancelled when
/// its parent_job_id fails. An all-mode child of that parent is.
#[tokio::test]
async fn any_mode_child_survives_a_failed_parent() {
    let db = TestDatabase::new().await;
    let t = seed_tenant(db.pool()).await;
    let parent = uuid::Uuid::new_v4().to_string();
    let alt = uuid::Uuid::new_v4().to_string();
    let any_child = uuid::Uuid::new_v4().to_string();
    let all_child = uuid::Uuid::new_v4().to_string();
    for (id, status) in [(&parent, "failed"), (&alt, "pending")] {
        sqlx::query(
            "INSERT INTO jobs (id, organization_id, queue_name, payload, status, created_at, updated_at) \
             VALUES ($1, $2, 'default', '{}'::JSONB, $3, NOW(), NOW())",
        )
        .bind(id)
        .bind(&t.org_id)
        .bind(status)
        .execute(db.pool())
        .await
        .unwrap();
    }
    for (id, mode) in [(&any_child, "any"), (&all_child, "all")] {
        sqlx::query(
            "INSERT INTO jobs (id, organization_id, queue_name, payload, status, parent_job_id, dependency_mode, dependencies_met, created_at, updated_at) \
             VALUES ($1, $2, 'default', '{}'::JSONB, 'pending', $3, $4, FALSE, NOW(), NOW())",
        )
        .bind(id)
        .bind(&t.org_id)
        .bind(&parent)
        .bind(mode)
        .execute(db.pool())
        .await
        .unwrap();
    }
    sqlx::query("INSERT INTO job_dependencies (job_id, depends_on_job_id) VALUES ($1, $2)")
        .bind(&any_child)
        .bind(&alt)
        .execute(db.pool())
        .await
        .unwrap();

    scheduler(&db)
        .update_job_dependencies()
        .await
        .expect("dependency sweep");

    let (any_status, _, _) = job_state(db.pool(), &any_child).await;
    let (all_status, all_error, _) = job_state(db.pool(), &all_child).await;
    assert_eq!(any_status, "pending");
    assert_eq!(all_status, "cancelled");
    assert_eq!(
        all_error.as_deref(),
        Some("Parent job failed or was deadlettered")
    );
}

/// FLOW-02: DLQ retry must not clear dependencies_met on an any-mode child
/// that another dependency already satisfied. An all-mode sibling is re-gated.
#[tokio::test]
async fn dlq_retry_keeps_a_satisfied_any_child_runnable() {
    let db = TestDatabase::new().await;
    let t = seed_tenant(db.pool()).await;
    let app = app(&db);
    let parent = uuid::Uuid::new_v4().to_string();
    let done = uuid::Uuid::new_v4().to_string();
    let any_child = uuid::Uuid::new_v4().to_string();
    let all_child = uuid::Uuid::new_v4().to_string();
    for (id, status) in [(&parent, "deadletter"), (&done, "completed")] {
        sqlx::query(
            "INSERT INTO jobs (id, organization_id, queue_name, payload, status, created_at, updated_at) \
             VALUES ($1, $2, 'default', '{}'::JSONB, $3, NOW(), NOW())",
        )
        .bind(id)
        .bind(&t.org_id)
        .bind(status)
        .execute(db.pool())
        .await
        .unwrap();
    }
    sqlx::query(
        "INSERT INTO jobs (id, organization_id, queue_name, payload, status, parent_job_id, dependency_mode, dependencies_met, created_at, updated_at) \
         VALUES ($1, $2, 'default', '{}'::JSONB, 'pending', $3, 'any', TRUE, NOW(), NOW())",
    )
    .bind(&any_child)
    .bind(&t.org_id)
    .bind(&parent)
    .execute(db.pool())
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO jobs (id, organization_id, queue_name, payload, status, parent_job_id, dependency_mode, dependencies_met, created_at, updated_at) \
         VALUES ($1, $2, 'default', '{}'::JSONB, 'pending', $3, 'all', TRUE, NOW(), NOW())",
    )
    .bind(&all_child)
    .bind(&t.org_id)
    .bind(&parent)
    .execute(db.pool())
    .await
    .unwrap();
    sqlx::query("INSERT INTO job_dependencies (job_id, depends_on_job_id) VALUES ($1, $2)")
        .bind(&any_child)
        .bind(&done)
        .execute(db.pool())
        .await
        .unwrap();

    let (status, body) = call(
        &app,
        "POST",
        "/api/v1/jobs/dlq/retry",
        &t.key,
        Some(json!({"job_ids": [parent]})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["retried_count"].as_i64(), Some(1));

    let (_, _, any_met) = job_state(db.pool(), &any_child).await;
    let (_, _, all_met) = job_state(db.pool(), &all_child).await;
    assert_eq!(any_met, Some(true));
    assert_eq!(all_met, Some(false));
}

/// FLOW-03: retry reruns failed, dead-lettered and dependency-cancelled steps.
/// A step a user cancelled, and one cancelled because a dependency was deleted,
/// stay cancelled.
#[tokio::test]
async fn workflow_retry_reopens_dependency_cancels_only() {
    let db = TestDatabase::new().await;
    let t = seed_tenant(db.pool()).await;
    let app = app(&db);
    let wf = insert_workflow(db.pool(), &t.org_id, "failed", 4).await;
    let dead = uuid::Uuid::new_v4().to_string();
    let dep_cancel = uuid::Uuid::new_v4().to_string();
    let user_cancel = uuid::Uuid::new_v4().to_string();
    let deleted_dep = uuid::Uuid::new_v4().to_string();
    let rows = [
        (dead.as_str(), "deadletter", None),
        (
            dep_cancel.as_str(),
            "cancelled",
            Some("Dependency job failed or was deadlettered"),
        ),
        (user_cancel.as_str(), "cancelled", None),
        (
            deleted_dep.as_str(),
            "cancelled",
            Some("Dependency job x was deleted before it completed"),
        ),
    ];
    for (id, status, err) in rows {
        sqlx::query(
            "INSERT INTO jobs (id, organization_id, queue_name, payload, status, workflow_id, last_error, created_at, updated_at) \
             VALUES ($1, $2, 'default', '{}'::JSONB, $3, $4, $5, NOW(), NOW())",
        )
        .bind(id)
        .bind(&t.org_id)
        .bind(status)
        .bind(&wf)
        .bind(err)
        .execute(db.pool())
        .await
        .unwrap();
    }

    let (status, body) = call(
        &app,
        "POST",
        &format!("/api/v1/workflows/{wf}/retry"),
        &t.key,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    assert_eq!(job_state(db.pool(), &dead).await.0, "pending");
    assert_eq!(job_state(db.pool(), &dep_cancel).await.0, "pending");
    assert_eq!(job_state(db.pool(), &user_cancel).await.0, "cancelled");
    assert_eq!(job_state(db.pool(), &deleted_dep).await.0, "cancelled");
}

/// FLOW-04: a dead-lettered step of a cancelled workflow is not retried.
/// One job answers 409. A bulk retry skips it and still retries a job that
/// belongs to no workflow.
#[tokio::test]
async fn cancelled_workflow_deadletter_is_not_retried() {
    let db = TestDatabase::new().await;
    let t = seed_tenant(db.pool()).await;
    let app = app(&db);
    let wf = insert_workflow(db.pool(), &t.org_id, "cancelled", 1).await;
    let held = uuid::Uuid::new_v4().to_string();
    let free = uuid::Uuid::new_v4().to_string();
    for (id, workflow) in [(&held, Some(wf.as_str())), (&free, None)] {
        sqlx::query(
            "INSERT INTO jobs (id, organization_id, queue_name, payload, status, workflow_id, created_at, updated_at) \
             VALUES ($1, $2, 'default', '{}'::JSONB, 'deadletter', $3, NOW(), NOW())",
        )
        .bind(id)
        .bind(&t.org_id)
        .bind(workflow)
        .execute(db.pool())
        .await
        .unwrap();
    }

    let (status, body) = call(
        &app,
        "POST",
        &format!("/api/v1/jobs/{held}/retry"),
        &t.key,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["code"], "CONFLICT");
    assert!(
        body["message"]
            .as_str()
            .unwrap_or("")
            .contains("cancelled workflow"),
        "{body}"
    );
    assert_eq!(job_state(db.pool(), &held).await.0, "deadletter");

    let (status, body) = call(
        &app,
        "POST",
        "/api/v1/jobs/dlq/retry",
        &t.key,
        Some(json!({"job_ids": [held, free]})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["retried_count"].as_i64(), Some(1));
    let retried = body["retried_jobs"].as_array().cloned().unwrap_or_default();
    assert!(retried.iter().all(|id| id.as_str() != Some(held.as_str())));
    assert!(retried.iter().any(|id| id.as_str() == Some(free.as_str())));
    assert_eq!(job_state(db.pool(), &held).await.0, "deadletter");
    assert_eq!(job_state(db.pool(), &free).await.0, "pending");
}

/// FLOW-07 / DASH-31: detail progress keeps a cancelled step out of `failed`
/// and reports it as `cancelled`. The list's stored failed_jobs stays 0.
#[tokio::test]
async fn workflow_detail_counts_cancelled_apart_from_failed() {
    let db = TestDatabase::new().await;
    let t = seed_tenant(db.pool()).await;
    let app = app(&db);
    let wf = insert_workflow(db.pool(), &t.org_id, "running", 1).await;
    sqlx::query(
        "INSERT INTO jobs (id, organization_id, queue_name, payload, status, workflow_id, created_at, updated_at) \
         VALUES ($1, $2, 'default', '{}'::JSONB, 'cancelled', $3, NOW(), NOW())",
    )
    .bind(uuid::Uuid::new_v4().to_string())
    .bind(&t.org_id)
    .bind(&wf)
    .execute(db.pool())
    .await
    .unwrap();

    let (status, body) = call(
        &app,
        "GET",
        &format!("/api/v1/workflows/{wf}"),
        &t.key,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["progress"]["failed"].as_i64(), Some(0));
    assert_eq!(body["progress"]["cancelled"].as_i64(), Some(1));

    let (status, list) = call(&app, "GET", "/api/v1/workflows", &t.key, None).await;
    assert_eq!(status, StatusCode::OK, "{list}");
    let row = list
        .as_array()
        .and_then(|rows| rows.iter().find(|row| row["id"] == wf))
        .expect("workflow in the list");
    assert_eq!(row["failed_jobs"].as_i64(), Some(0));
}

/// GET /workflows/{id} returns each job's step key, so a client can match the
/// jobs it reads back to the steps it defined. A job created before the key
/// was stored has `key: null`.
#[tokio::test]
async fn workflow_detail_returns_each_job_key() {
    let db = TestDatabase::new().await;
    let t = seed_tenant(db.pool()).await;
    let app = app(&db);

    let (status, created) = call(
        &app,
        "POST",
        "/api/v1/workflows",
        &t.key,
        Some(json!({
            "name": "keys",
            "jobs": [
                job("extract", json!({})),
                job("load", json!({"depends_on": ["extract"]})),
            ],
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{created}");
    let workflow_id = created["workflow_id"].as_str().unwrap().to_string();
    let id_of = |key: &str| {
        created["job_ids"]
            .as_array()
            .unwrap()
            .iter()
            .find(|m| m["key"] == key)
            .map(|m| m["job_id"].as_str().unwrap().to_string())
            .unwrap()
    };
    let (extract, load) = (id_of("extract"), id_of("load"));

    // A job written before the column existed has no key.
    sqlx::query("UPDATE jobs SET workflow_job_key = NULL WHERE id = $1")
        .bind(&load)
        .execute(db.pool())
        .await
        .unwrap();

    let (status, detail) = call(
        &app,
        "GET",
        &format!("/api/v1/workflows/{workflow_id}"),
        &t.key,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{detail}");
    let key_of = |id: &str| {
        detail["jobs"]
            .as_array()
            .unwrap()
            .iter()
            .find(|j| j["id"] == id)
            .map(|j| j["key"].clone())
            .unwrap()
    };
    assert_eq!(key_of(&extract), json!("extract"), "{detail}");
    assert_eq!(key_of(&load), Value::Null, "{detail}");
}
