#![cfg(feature = "docker-tests")]
//! 0.1.131 hunt: job-engine races and predicates (CORE-*), API limits and
//! config (API-*), worker cap and gRPC history (NP-*).
//!
//! Each test asserts the correct behaviour; each failed before its fix.

mod common;

use std::sync::Arc;
use std::time::Duration;

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
    let slug = format!("hcore-{}", &org_id[..8]);
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
         VALUES ($1, $2, $3, $4, $5, 'hcore', ARRAY[]::TEXT[], TRUE, NOW())",
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

async fn insert_job(
    pool: &sqlx::PgPool,
    org_id: &str,
    queue: &str,
    status: &str,
    workflow_id: Option<&str>,
) -> String {
    let id = uuid::Uuid::new_v4().to_string();
    sqlx::query(
        "INSERT INTO jobs (id, organization_id, queue_name, payload, status, workflow_id, \
                           assigned_worker_id, lease_id, lease_expires_at, created_at, updated_at) \
         VALUES ($1, $2, $3, '{}'::JSONB, $4, $5, \
                 CASE WHEN $4 = 'processing' THEN 'w1' END, \
                 CASE WHEN $4 = 'processing' THEN 'lease-' || $1 END, \
                 CASE WHEN $4 = 'processing' THEN NOW() + INTERVAL '5 minutes' END, \
                 NOW(), NOW())",
    )
    .bind(&id)
    .bind(org_id)
    .bind(queue)
    .bind(status)
    .bind(workflow_id)
    .execute(pool)
    .await
    .expect("seed job");
    id
}

/// Sessions of this test database blocked on a lock.
async fn lock_waiters(pool: &sqlx::PgPool) -> i64 {
    sqlx::query_scalar(
        "SELECT COUNT(*) FROM pg_stat_activity \
         WHERE datname = current_database() AND wait_event_type = 'Lock'",
    )
    .fetch_one(pool)
    .await
    .unwrap()
}

/// Wait until some session of this test database is blocked on a lock.
async fn wait_for_lock_waiter(pool: &sqlx::PgPool) {
    for _ in 0..100 {
        if lock_waiters(pool).await > 0 {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("the second transaction never blocked");
}

/// CORE: two workflow steps that finish at the same time leave the workflow
/// `running` for ever. Each progress-trigger run counts the steps from a
/// snapshot taken before it waits for the other's lock on the workflow row,
/// so both see the other step still `processing`. The terminate sweep only
/// looks at workflows that have a failed/cancelled step, so nothing repairs
/// it, and the workflow keeps a max_workflows slot.
#[tokio::test]
async fn core1_concurrent_last_step_completions_finish_the_workflow() {
    let db = TestDatabase::new().await;
    let t = seed_tenant(db.pool()).await;
    let wf = uuid::Uuid::new_v4().to_string();
    sqlx::query(
        "INSERT INTO workflows (id, organization_id, name, status, total_jobs, created_at, started_at) \
         VALUES ($1, $2, 'fan-out', 'running', 2, NOW(), NOW())",
    )
    .bind(&wf)
    .bind(&t.org_id)
    .execute(db.pool())
    .await
    .unwrap();
    let a = insert_job(db.pool(), &t.org_id, "default", "processing", Some(&wf)).await;
    let b = insert_job(db.pool(), &t.org_id, "default", "processing", Some(&wf)).await;

    // Worker 1 completes step A (its trigger sees B processing: 'running').
    let mut tx1 = db.pool().begin().await.unwrap();
    sqlx::query("UPDATE jobs SET status = 'completed', completed_at = NOW() WHERE id = $1")
        .bind(&a)
        .execute(&mut *tx1)
        .await
        .unwrap();

    // Worker 2 completes step B at the same instant (its trigger sees A still
    // processing, then waits for tx1's lock on the workflow row).
    let pool = Arc::clone(&db.pool);
    let b2 = b.clone();
    let second = tokio::spawn(async move {
        let mut tx2 = pool.begin().await.unwrap();
        sqlx::query("UPDATE jobs SET status = 'completed', completed_at = NOW() WHERE id = $1")
            .bind(&b2)
            .execute(&mut *tx2)
            .await
            .unwrap();
        tx2.commit().await.unwrap();
    });
    wait_for_lock_waiter(db.pool()).await;
    tx1.commit().await.unwrap();
    second.await.unwrap();

    // The scheduler's safety nets run.
    scheduler(&db).update_job_dependencies().await.unwrap();

    let open: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM jobs WHERE workflow_id = $1 AND status <> 'completed'",
    )
    .bind(&wf)
    .fetch_one(db.pool())
    .await
    .unwrap();
    assert_eq!(open, 0, "both steps completed");

    let (status, completed_at, completed_jobs): (
        String,
        Option<chrono::DateTime<chrono::Utc>>,
        i32,
    ) = sqlx::query_as("SELECT status, completed_at, completed_jobs FROM workflows WHERE id = $1")
        .bind(&wf)
        .fetch_one(db.pool())
        .await
        .unwrap();
    let (active_workflows,): (i64,) =
        sqlx::query_as("SELECT workflows FROM get_org_resource_counts($1)")
            .bind(&t.org_id)
            .fetch_one(db.pool())
            .await
            .unwrap();
    println!(
        "workflow status={status} completed_at={completed_at:?} completed_jobs={completed_jobs} \
         max_workflows slots held={active_workflows}"
    );
    assert_eq!(
        status, "completed",
        "every step completed, yet the workflow is stuck (slots held: {active_workflows})"
    );
}

/// CORE: POST /jobs/{id}/dependencies racing the dependency's completion
/// leaves the job gated on a completed job for ever. The completion trigger
/// cannot see the uncommitted edge, the handler's recompute cannot see the
/// uncommitted completion, and no sweep re-checks a job that has edges but no
/// parent_job_id.
#[tokio::test]
async fn core3_dependency_added_while_it_completes_is_not_lost() {
    let db = TestDatabase::new().await;
    let t = seed_tenant(db.pool()).await;
    let app = app(&db);
    let dep = insert_job(db.pool(), &t.org_id, "default", "processing", None).await;
    let target = insert_job(db.pool(), &t.org_id, "default", "pending", None).await;

    // The dependency's worker is completing it (trigger runs, finds no edges).
    let mut tx1 = db.pool().begin().await.unwrap();
    sqlx::query("UPDATE jobs SET status = 'completed', completed_at = NOW() WHERE id = $1")
        .bind(&dep)
        .execute(&mut *tx1)
        .await
        .unwrap();

    // Meanwhile the client gates the target on it. With the fix the handler
    // waits for the completion to commit; without it, it answers at once.
    let app2 = app.clone();
    let key = t.key.clone();
    let uri = format!("/api/v1/jobs/{target}/dependencies");
    let dep2 = dep.clone();
    let handler = tokio::spawn(async move {
        call(
            &app2,
            "POST",
            &uri,
            &key,
            Some(json!({ "depends_on": [dep2] })),
        )
        .await
    });
    for _ in 0..100 {
        if handler.is_finished() || lock_waiters(db.pool()).await > 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    tx1.commit().await.unwrap();
    let (status, body) = handler.await.unwrap();
    assert_eq!(status, StatusCode::OK, "{body}");
    println!("add_dependencies answered {body}");

    // Every safety net the scheduler has.
    let s = scheduler(&db);
    s.update_job_dependencies().await.unwrap();

    let (dep_status,): (String,) = sqlx::query_as("SELECT status FROM jobs WHERE id = $1")
        .bind(&dep)
        .fetch_one(db.pool())
        .await
        .unwrap();
    assert_eq!(dep_status, "completed");

    let (claimed_status, claimed) = call(
        &app,
        "POST",
        "/api/v1/jobs/claim",
        &t.key,
        Some(json!({ "queue_name": "default", "worker_id": "w2", "limit": 10 })),
    )
    .await;
    let (met,): (Option<bool>,) = sqlx::query_as("SELECT dependencies_met FROM jobs WHERE id = $1")
        .bind(&target)
        .fetch_one(db.pool())
        .await
        .unwrap();
    println!("target dependencies_met={met:?}; claim {claimed_status} -> {claimed}");
    assert_eq!(
        met,
        Some(true),
        "the only dependency is completed, but the target stays gated"
    );
}

/// CORE: DELETE /queues/{name} (no delete_jobs) only refuses while
/// pending/processing jobs exist. A retry waiting out its backoff has been
/// `scheduled` since 0.1.126, so a paused queue whose jobs are all waiting
/// retries loses its config row, and with it the pause.
#[tokio::test]
async fn core6_queue_delete_refuses_while_a_retry_is_waiting() {
    let db = TestDatabase::new().await;
    let t = seed_tenant(db.pool()).await;
    let app = app(&db);

    let (status, body) = call(
        &app,
        "PUT",
        "/api/v1/queues/held/config",
        &t.key,
        Some(json!({ "enabled": false, "max_retries": 7 })),
    )
    .await;
    assert!(status.is_success(), "{status} {body}");

    // A job of the paused queue failed once and waits for its retry.
    let job = insert_job(db.pool(), &t.org_id, "held", "scheduled", None).await;
    sqlx::query(
        "UPDATE jobs SET scheduled_at = NOW() - INTERVAL '1 second', retry_count = 1 WHERE id = $1",
    )
    .bind(&job)
    .execute(db.pool())
    .await
    .unwrap();

    let (del_status, del_body) = call(&app, "DELETE", "/api/v1/queues/held", &t.key, None).await;
    let (claim_status, claimed) = call(
        &app,
        "POST",
        "/api/v1/jobs/claim",
        &t.key,
        Some(json!({ "queue_name": "held", "worker_id": "w9" })),
    )
    .await;
    println!(
        "delete -> {del_status} {del_body}; claim on the paused queue -> {claim_status} {claimed}"
    );
    assert_eq!(
        del_status,
        StatusCode::CONFLICT,
        "a queue with a job waiting for its retry must not lose its config"
    );
}

/// CORE: W5-R5 kept retention from deleting a failed workflow's dead-lettered
/// step, but POST /jobs/dlq/purge still deletes it. Its dependency edges
/// cascade away, so POST /workflows/{id}/retry re-opens the sweep-cancelled
/// dependent with `dependencies_met = TRUE`: it runs although its dependency
/// never succeeded, and the workflow ends `completed`.
#[tokio::test]
async fn core2_workflow_retry_after_dlq_purge_does_not_run_ungated_steps() {
    let db = TestDatabase::new().await;
    let t = seed_tenant(db.pool()).await;
    let app = app(&db);

    let (status, wf) = call(
        &app,
        "POST",
        "/api/v1/workflows",
        &t.key,
        Some(json!({
            "name": "etl",
            "jobs": [
                {"key": "extract", "queue_name": "etl", "payload": {"step": "extract"}, "max_retries": 0},
                {"key": "load", "queue_name": "etl", "payload": {"step": "load"}, "depends_on": ["extract"]}
            ]
        })),
    )
    .await;
    assert!(status.is_success(), "{status} {wf}");
    let wf_id = wf["workflow_id"].as_str().unwrap().to_string();
    let load_id = wf["job_ids"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["key"] == "load")
        .unwrap()["job_id"]
        .as_str()
        .unwrap()
        .to_string();

    // extract runs and fails for good (max_retries 0).
    let (_, claimed) = call(
        &app,
        "POST",
        "/api/v1/jobs/claim",
        &t.key,
        Some(json!({"queue_name": "etl", "worker_id": "w1"})),
    )
    .await;
    let extract = &claimed["jobs"][0];
    let (fs, fb) = call(
        &app,
        "POST",
        &format!("/api/v1/jobs/{}/fail", extract["id"].as_str().unwrap()),
        &t.key,
        Some(json!({"worker_id": "w1", "lease_id": extract["lease_id"], "error": "source down"})),
    )
    .await;
    assert_eq!(fs, StatusCode::OK, "{fb}");

    // The dependency sweep cancels load; the workflow is failed.
    scheduler(&db).update_job_dependencies().await.unwrap();
    let (wf_status,): (String,) = sqlx::query_as("SELECT status FROM workflows WHERE id = $1")
        .bind(&wf_id)
        .fetch_one(db.pool())
        .await
        .unwrap();
    assert_eq!(wf_status, "failed");

    // Routine DLQ clean-up, then the workflow is retried.
    let (ps, pb) = call(
        &app,
        "POST",
        "/api/v1/jobs/dlq/purge",
        &t.key,
        Some(json!({"confirm": true})),
    )
    .await;
    assert_eq!(ps, StatusCode::OK, "{pb}");
    let (rs, rb) = call(
        &app,
        "POST",
        &format!("/api/v1/workflows/{wf_id}/retry"),
        &t.key,
        None,
    )
    .await;
    println!("purge -> {pb}; workflow retry -> {rs} {rb}");
    assert_eq!(rs, StatusCode::CONFLICT, "nothing is left to retry: {rb}");
    let (wf_status,): (String,) = sqlx::query_as("SELECT status FROM workflows WHERE id = $1")
        .bind(&wf_id)
        .fetch_one(db.pool())
        .await
        .unwrap();
    assert_eq!(
        wf_status, "failed",
        "a refused retry leaves the workflow failed"
    );

    let (load_status, load_met): (String, Option<bool>) =
        sqlx::query_as("SELECT status, dependencies_met FROM jobs WHERE id = $1")
            .bind(&load_id)
            .fetch_one(db.pool())
            .await
            .unwrap();
    let (_, claimed_again) = call(
        &app,
        "POST",
        "/api/v1/jobs/claim",
        &t.key,
        Some(json!({"queue_name": "etl", "worker_id": "w2"})),
    )
    .await;
    println!("load: status={load_status} dependencies_met={load_met:?}; claim -> {claimed_again}");
    assert_eq!(load_status, "cancelled");
    let (load_error,): (Option<String>,) =
        sqlx::query_as("SELECT last_error FROM jobs WHERE id = $1")
            .bind(&load_id)
            .fetch_one(db.pool())
            .await
            .unwrap();
    assert!(
        load_error
            .as_deref()
            .is_some_and(|e| e.contains("was deleted before it completed")),
        "the dependent records that its dependency was deleted: {load_error:?}"
    );
    assert!(
        claimed_again["jobs"]
            .as_array()
            .map_or(true, |j| j.is_empty()),
        "load runs although extract never succeeded: {claimed_again}"
    );
}

/// CORE-1: workflows already stuck `running` by the race (every step
/// completed) are closed by the scheduler sweep, which used to close only
/// workflows with a failed or cancelled step.
#[tokio::test]
async fn core1_sweep_closes_a_running_workflow_whose_steps_all_completed() {
    let db = TestDatabase::new().await;
    let t = seed_tenant(db.pool()).await;
    let wf = uuid::Uuid::new_v4().to_string();
    sqlx::query(
        "INSERT INTO workflows (id, organization_id, name, status, total_jobs, completed_jobs, created_at, started_at) \
         VALUES ($1, $2, 'stuck', 'running', 2, 1, NOW(), NOW())",
    )
    .bind(&wf)
    .bind(&t.org_id)
    .execute(db.pool())
    .await
    .unwrap();
    insert_job(db.pool(), &t.org_id, "default", "completed", Some(&wf)).await;
    insert_job(db.pool(), &t.org_id, "default", "completed", Some(&wf)).await;

    scheduler(&db).update_job_dependencies().await.unwrap();

    let (status, completed_jobs, completed_at): (
        String,
        i32,
        Option<chrono::DateTime<chrono::Utc>>,
    ) = sqlx::query_as("SELECT status, completed_jobs, completed_at FROM workflows WHERE id = $1")
        .bind(&wf)
        .fetch_one(db.pool())
        .await
        .unwrap();
    assert_eq!(status, "completed");
    assert_eq!(completed_jobs, 2);
    assert!(completed_at.is_some());
}

/// CORE-5: `59 1 * * *` in New York on the 2026 fall-back day (1 November).
/// The 01:59 EDT occurrence (05:59Z) fires; the tick commits late, at
/// 01:01 EST (06:01Z). Counting from that clock gave the repeated 01:59 (EST)
/// an hour later, a second run the same day. The next run is the next day.
#[test]
fn core5_late_fire_in_repeated_hour_does_not_fire_again_the_same_day() {
    use chrono::TimeZone;
    use spooled_backend::models::CronSchedule;
    use spooled_backend::scheduler::next_run_after_occurrence;

    let cron = CronSchedule::parse("59 1 * * *").expect("parse");
    let fired = chrono::Utc.with_ymd_and_hms(2026, 11, 1, 5, 59, 0).unwrap();
    let late_commit = chrono::Utc.with_ymd_and_hms(2026, 11, 1, 6, 1, 0).unwrap();
    let next_day = chrono::Utc.with_ymd_and_hms(2026, 11, 2, 6, 59, 0).unwrap();

    assert_eq!(
        next_run_after_occurrence(Some(&cron), "America/New_York", Some(fired), late_commit),
        Some(next_day),
        "a late tick must not schedule the repeated 01:59 (EST) the same day"
    );
    // On time, and with no stored occurrence, the answer is the clock's.
    assert_eq!(
        next_run_after_occurrence(Some(&cron), "America/New_York", Some(fired), fired),
        Some(next_day)
    );
    let ordinary = chrono::Utc.with_ymd_and_hms(2026, 6, 1, 12, 0, 0).unwrap();
    assert_eq!(
        next_run_after_occurrence(Some(&cron), "UTC", None, ordinary),
        Some(chrono::Utc.with_ymd_and_hms(2026, 6, 2, 1, 59, 0).unwrap())
    );
    // Long downtime: the next run is the next one after the clock.
    let long_ago = chrono::Utc.with_ymd_and_hms(2025, 1, 1, 1, 59, 0).unwrap();
    assert_eq!(
        next_run_after_occurrence(Some(&cron), "UTC", Some(long_ago), ordinary),
        Some(chrono::Utc.with_ymd_and_hms(2026, 6, 2, 1, 59, 0).unwrap())
    );
}

/// CORE-8: a database error in the cron limit check is not a limit refusal.
/// The fire used to be recorded as a failed run and next_run_at advanced, so
/// the occurrence was lost; now the next tick fires it.
#[tokio::test]
async fn core8_cron_limit_check_db_error_retries_on_the_next_tick() {
    let db = TestDatabase::new().await;
    let t = seed_tenant(db.pool()).await;
    let id = uuid::Uuid::new_v4().to_string();
    sqlx::query(
        "INSERT INTO schedules (id, organization_id, name, cron_expression, timezone, queue_name, payload_template, is_active, next_run_at) \
         VALUES ($1, $2, 'hourly', '0 * * * *', 'UTC', 'cronq', '{}'::JSONB, TRUE, NOW() - INTERVAL '1 second')",
    )
    .bind(&id)
    .bind(&t.org_id)
    .execute(db.pool())
    .await
    .unwrap();
    let (due,): (chrono::DateTime<chrono::Utc>,) =
        sqlx::query_as("SELECT next_run_at FROM schedules WHERE id = $1")
            .bind(&id)
            .fetch_one(db.pool())
            .await
            .unwrap();

    // The limit check's count function is unavailable (this test's database only).
    sqlx::query(
        "ALTER FUNCTION get_org_resource_counts(TEXT) RENAME TO get_org_resource_counts_off",
    )
    .execute(db.pool())
    .await
    .unwrap();
    scheduler(&db).process_cron_schedules().await.unwrap();

    let (next,): (chrono::DateTime<chrono::Utc>,) =
        sqlx::query_as("SELECT next_run_at FROM schedules WHERE id = $1")
            .bind(&id)
            .fetch_one(db.pool())
            .await
            .unwrap();
    let runs: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM schedule_runs WHERE schedule_id = $1")
        .bind(&id)
        .fetch_one(db.pool())
        .await
        .unwrap();
    assert_eq!(next, due, "the occurrence stays due");
    assert_eq!(
        runs, 0,
        "no failed run is recorded for a check that could not run"
    );

    sqlx::query(
        "ALTER FUNCTION get_org_resource_counts_off(TEXT) RENAME TO get_org_resource_counts",
    )
    .execute(db.pool())
    .await
    .unwrap();
    scheduler(&db).process_cron_schedules().await.unwrap();
    let (status, job_id): (String, Option<String>) =
        sqlx::query_as("SELECT status, job_id FROM schedule_runs WHERE schedule_id = $1")
            .bind(&id)
            .fetch_one(db.pool())
            .await
            .unwrap();
    assert_eq!(status, "completed");
    assert!(job_id.is_some());
}

/// CORE-4: the API process put every `processing` job of a worker that shared
/// its hostname back to pending on shutdown, although it runs no worker of its
/// own, so a job an external worker on the same host was still running ran
/// twice. Nothing in the server binary releases jobs any more.
#[test]
fn core4_api_shutdown_does_not_release_jobs() {
    let main_rs = include_str!("../src/main.rs");
    assert!(
        !main_rs.contains("UPDATE jobs"),
        "src/main.rs must not change jobs on shutdown"
    );
}

fn app_with(db: &TestDatabase, settings: Settings) -> Router {
    router(AppState::new(
        Database::from_pool(Arc::clone(&db.pool)),
        None,
        Metrics::new(),
        settings,
    ))
}

/// Unauthenticated call (org create, signup).
async fn call_anon(app: &Router, method: &str, uri: &str, body: Value) -> (StatusCode, Value) {
    let req = Request::builder()
        .method(method)
        .uri(uri)
        .header("Content-Type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    let res = app.clone().oneshot(req).await.expect("request");
    let status = res.status();
    let bytes = axum::body::to_bytes(res.into_body(), 1 << 20)
        .await
        .unwrap();
    let json = serde_json::from_slice(&bytes)
        .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into()));
    (status, json)
}

/// An organization from public `POST /organizations` and one from email
/// signup completion: (org id, API key) each.
async fn create_public_and_signup_orgs(db: &TestDatabase, app: &Router) -> [(String, String); 2] {
    let slug = format!("pt{}", &uuid::Uuid::new_v4().simple().to_string()[..10]);
    let (status, body) = call_anon(
        app,
        "POST",
        "/api/v1/organizations",
        json!({"name": "Plan Default", "slug": slug}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let public = (
        body["organization"]["id"].as_str().unwrap().to_string(),
        body["api_key"]["key"].as_str().unwrap().to_string(),
    );

    let token = uuid::Uuid::new_v4().simple().to_string();
    let email = format!("{}@example.com", &token[..12]);
    sqlx::query(
        "INSERT INTO email_login_codes (email, code, expires_at) VALUES ($1, $2, NOW() + INTERVAL '1 hour')",
    )
    .bind(&email)
    .bind(format!("signup:{token}"))
    .execute(db.pool())
    .await
    .unwrap();
    let slug = format!("ps{}", &uuid::Uuid::new_v4().simple().to_string()[..10]);
    let (status, body) = call_anon(
        app,
        "POST",
        "/api/v1/auth/signup/complete",
        json!({"signup_token": token, "name": "Plan Signup", "slug": slug}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let signup = (
        body["organization"]["id"].as_str().unwrap().to_string(),
        body["api_key"].as_str().unwrap().to_string(),
    );
    [public, signup]
}

async fn plan_tier(pool: &sqlx::PgPool, org_id: &str) -> String {
    sqlx::query_scalar("SELECT plan_tier FROM organizations WHERE id = $1")
        .bind(org_id)
        .fetch_one(pool)
        .await
        .unwrap()
}

async fn register_worker(app: &Router, key: &str, n: usize) -> (StatusCode, Value) {
    call(
        app,
        "POST",
        "/api/v1/workers/register",
        key,
        Some(json!({"queue_name": "plan-q", "hostname": format!("plan-host-{n}")})),
    )
    .await
}

/// DEFAULT_PLAN_TIER (owner decision): a self-hosted install is not stuck on
/// the hosted Free plan. With `enterprise`, organizations from public create
/// and from email signup get that plan and can register three workers and
/// create a workflow.
#[tokio::test]
async fn default_plan_tier_applies_to_public_create_and_signup() {
    let db = TestDatabase::new().await;
    let mut settings = Settings::load_for_testing();
    settings.registration.default_plan_tier = "enterprise".to_string();
    let app = app_with(&db, settings);

    for (org_id, key) in create_public_and_signup_orgs(&db, &app).await {
        assert_eq!(plan_tier(db.pool(), &org_id).await, "enterprise");
        for n in 1..=3 {
            let (status, body) = register_worker(&app, &key, n).await;
            assert!(status.is_success(), "worker {n}: {status} {body}");
        }
        let (status, body) = call(
            &app,
            "POST",
            "/api/v1/workflows",
            &key,
            Some(json!({"name": "wf", "jobs": [{"key": "a", "queue_name": "plan-q", "payload": {}}]})),
        )
        .await;
        assert!(status.is_success(), "workflow: {status} {body}");
    }
}

/// With the default setting new organizations stay on Free (spooled.cloud is
/// unchanged): one worker, the second is refused.
#[tokio::test]
async fn default_plan_tier_unset_keeps_new_orgs_on_free() {
    let db = TestDatabase::new().await;
    let app = app(&db);

    for (org_id, key) in create_public_and_signup_orgs(&db, &app).await {
        assert_eq!(plan_tier(db.pool(), &org_id).await, "free");
        let (status, body) = register_worker(&app, &key, 1).await;
        assert!(status.is_success(), "{status} {body}");
        let (status, body) = register_worker(&app, &key, 2).await;
        assert!(
            !status.is_success(),
            "Free allows one worker: {status} {body}"
        );
    }
}

/// DEFAULT_PLAN_TIER parsing: trimmed, case-insensitive, unset or blank is
/// free, an unknown plan stops startup with a message naming the setting.
#[test]
fn default_plan_tier_parsing() {
    use spooled_backend::config::default_plan_tier_from;
    assert_eq!(default_plan_tier_from(None).unwrap(), "free");
    assert_eq!(default_plan_tier_from(Some("  ")).unwrap(), "free");
    assert_eq!(
        default_plan_tier_from(Some(" Enterprise ")).unwrap(),
        "enterprise"
    );
    assert_eq!(default_plan_tier_from(Some("STARTER")).unwrap(), "starter");
    let err = default_plan_tier_from(Some("gold"))
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("DEFAULT_PLAN_TIER") && err.contains("gold"),
        "{err}"
    );
}

/// NP-1: a worker stopped without deregistering (Ctrl+C) held the Free plan's
/// only worker slot until the stale-worker sweep (every 5 minutes, 2-minute
/// threshold) marked it offline, so restarting the worker failed with
/// "workers limit reached (1/1)" for up to seven minutes. A worker silent past
/// the sweep's threshold no longer counts.
#[tokio::test]
async fn np1_silent_worker_does_not_hold_the_free_worker_slot() {
    let db = TestDatabase::new().await;
    let t = seed_tenant(db.pool()).await;
    sqlx::query("UPDATE organizations SET plan_tier = 'free' WHERE id = $1")
        .bind(&t.org_id)
        .execute(db.pool())
        .await
        .unwrap();
    let app = app(&db);

    let (status, body) = register_worker(&app, &t.key, 1).await;
    assert!(status.is_success(), "{status} {body}");
    // A second live worker is over Free's cap of one.
    let (status, body) = register_worker(&app, &t.key, 2).await;
    assert!(
        !status.is_success(),
        "Free allows one live worker: {status} {body}"
    );

    // The first worker was killed: no heartbeat for longer than the threshold.
    sqlx::query("UPDATE workers SET last_heartbeat = NOW() - INTERVAL '3 minutes' WHERE organization_id = $1")
        .bind(&t.org_id)
        .execute(db.pool())
        .await
        .unwrap();
    let (status, body) = register_worker(&app, &t.key, 3).await;
    assert!(
        status.is_success(),
        "the restarted worker must get the dead one's slot: {status} {body}"
    );
    let (workers,): (i64,) = sqlx::query_as("SELECT workers FROM get_org_resource_counts($1)")
        .bind(&t.org_id)
        .fetch_one(db.pool())
        .await
        .unwrap();
    assert_eq!(workers, 1);
}

async fn custom_webhook(
    app: &Router,
    org_id: &str,
    token: &str,
    body: Value,
) -> (StatusCode, Value) {
    let req = Request::builder()
        .method("POST")
        .uri(format!("/api/v1/webhooks/{org_id}/custom"))
        .header("X-Webhook-Token", token)
        .header("Content-Type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    let res = app.clone().oneshot(req).await.expect("request");
    let status = res.status();
    let bytes = axum::body::to_bytes(res.into_body(), 1 << 20)
        .await
        .unwrap();
    let json = serde_json::from_slice(&bytes)
        .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into()));
    (status, json)
}

/// API-1: inbound custom webhooks opened any number of queues (a Free org
/// reached 5 of 2). A delivery to a queue name the org does not have is now
/// charged against max_queues like `POST /jobs`; existing queues still work.
#[tokio::test]
async fn api1_custom_webhook_respects_the_queue_cap() {
    let db = TestDatabase::new().await;
    let t = seed_tenant(db.pool()).await;
    sqlx::query(
        "UPDATE organizations SET plan_tier = 'free', settings = '{\"webhook_token\":\"whk_r0131\"}'::JSONB WHERE id = $1",
    )
    .bind(&t.org_id)
    .execute(db.pool())
    .await
    .unwrap();
    let app = app(&db);
    let max_queues = i64::from(
        spooled_backend::config::PlanLimits::for_tier("free")
            .max_queues
            .expect("Free caps queues"),
    );

    for n in 0..max_queues {
        let (status, body) = custom_webhook(
            &app,
            &t.org_id,
            "whk_r0131",
            json!({"queue_name": format!("hook-q{n}"), "payload": {"n": n}}),
        )
        .await;
        assert!(status.is_success(), "queue {n}: {status} {body}");
    }
    let (status, body) = custom_webhook(
        &app,
        &t.org_id,
        "whk_r0131",
        json!({"queue_name": "hook-over", "payload": {}}),
    )
    .await;
    assert!(
        !status.is_success(),
        "a third queue on Free: {status} {body}"
    );
    let queues: i64 = sqlx::query_scalar(
        "SELECT COUNT(DISTINCT queue_name) FROM jobs WHERE organization_id = $1",
    )
    .bind(&t.org_id)
    .fetch_one(db.pool())
    .await
    .unwrap();
    assert_eq!(queues, max_queues);
    // An existing queue still takes deliveries.
    let (status, body) = custom_webhook(
        &app,
        &t.org_id,
        "whk_r0131",
        json!({"queue_name": "hook-q0", "payload": {"again": true}}),
    )
    .await;
    assert!(status.is_success(), "{status} {body}");
}

fn grpc_queue_service(db: &TestDatabase) -> spooled_backend::grpc::services::QueueServiceImpl {
    spooled_backend::grpc::services::QueueServiceImpl::new(
        Arc::clone(&db.pool),
        Arc::new(Metrics::new()),
        None,
        Arc::new(OutgoingWebhookService::new((*db.pool).clone(), 1, 4)),
        spooled_backend::config::QueueSettings {
            default_max_retries: 3,
            default_timeout_secs: 300,
            max_payload_size_bytes: 1024 * 1024,
        },
        false,
        spooled_backend::config::WorkerSettings {
            heartbeat_interval_secs: 10,
            lease_duration_secs: 30,
            grpc_default_lease_duration_secs: 300,
            max_concurrency: 10,
            fallback_poll_interval_secs: 5,
        },
    )
}

fn grpc<T>(key: &str, body: T) -> tonic::Request<T> {
    let mut request = tonic::Request::new(body);
    request
        .metadata_mut()
        .insert("x-api-key", key.parse().unwrap());
    request
}

async fn history_events(pool: &sqlx::PgPool, job_id: &str) -> Vec<String> {
    sqlx::query_scalar("SELECT event_type FROM job_history WHERE job_id = $1 ORDER BY event_type")
        .bind(job_id)
        .fetch_all(pool)
        .await
        .unwrap()
}

/// NP-5: jobs claimed, completed and failed over gRPC had an empty
/// `GET /jobs/{id}/history`; only the REST paths wrote job_history. gRPC
/// Dequeue, Complete and Fail now write the same events.
#[tokio::test]
async fn np5_grpc_claim_complete_and_fail_write_job_history() {
    use spooled_backend::grpc::proto::queue_service_server::QueueService;
    use spooled_backend::grpc::proto::{CompleteRequest, DequeueRequest, FailRequest};

    let db = TestDatabase::new().await;
    let t = seed_tenant(db.pool()).await;
    let svc = grpc_queue_service(&db);
    insert_job(db.pool(), &t.org_id, "gq", "pending", None).await;
    insert_job(db.pool(), &t.org_id, "gq", "pending", None).await;

    let jobs = svc
        .dequeue(grpc(
            &t.key,
            DequeueRequest {
                queue_name: "gq".to_string(),
                worker_id: "gw".to_string(),
                lease_duration_secs: 60,
                batch_size: 2,
            },
        ))
        .await
        .expect("dequeue")
        .into_inner()
        .jobs;
    assert_eq!(jobs.len(), 2);
    let (done, failed) = (&jobs[0], &jobs[1]);

    svc.complete(grpc(
        &t.key,
        CompleteRequest {
            job_id: done.id.clone(),
            worker_id: "gw".to_string(),
            lease_id: done.lease_id.clone(),
            ..Default::default()
        },
    ))
    .await
    .expect("complete");
    svc.fail(grpc(
        &t.key,
        FailRequest {
            job_id: failed.id.clone(),
            worker_id: "gw".to_string(),
            error: "boom".to_string(),
            lease_id: failed.lease_id.clone(),
            ..Default::default()
        },
    ))
    .await
    .expect("fail");

    assert_eq!(
        history_events(db.pool(), &done.id).await,
        ["completed", "processing"]
    );
    assert_eq!(
        history_events(db.pool(), &failed.id).await,
        ["processing", "retry_scheduled"]
    );
    let (org,): (String,) = sqlx::query_as(
        "SELECT details->>'organization_id' FROM job_history WHERE job_id = $1 AND event_type = 'retry_scheduled'",
    )
    .bind(&failed.id)
    .fetch_one(db.pool())
    .await
    .unwrap();
    assert_eq!(org, t.org_id);
}

/// API-3: an expired API key authenticates nothing but still held a
/// max_api_keys slot, so an organization at its limit could not create a key.
#[tokio::test]
async fn api3_expired_api_keys_do_not_count_toward_the_cap() {
    let db = TestDatabase::new().await;
    let t = seed_tenant(db.pool()).await;
    sqlx::query("UPDATE organizations SET plan_tier = 'free' WHERE id = $1")
        .bind(&t.org_id)
        .execute(db.pool())
        .await
        .unwrap();
    let cap = i64::from(
        spooled_backend::config::PlanLimits::for_tier("free")
            .max_api_keys
            .expect("Free caps API keys"),
    );
    // Fill the cap with keys that expired yesterday (the seeded key is the 1st).
    for n in 1..cap {
        sqlx::query(
            "INSERT INTO api_keys (id, organization_id, key_hash, key_prefix, name, queues, is_active, created_at, expires_at) \
             VALUES ($1, $2, 'x', 'sp_test_', $3, ARRAY[]::TEXT[], TRUE, NOW(), NOW() - INTERVAL '1 day')",
        )
        .bind(uuid::Uuid::new_v4().to_string())
        .bind(&t.org_id)
        .bind(format!("expired-{n}"))
        .execute(db.pool())
        .await
        .unwrap();
    }
    let (keys,): (i64,) = sqlx::query_as("SELECT api_keys FROM get_org_resource_counts($1)")
        .bind(&t.org_id)
        .fetch_one(db.pool())
        .await
        .unwrap();
    assert_eq!(keys, 1, "only the live key counts");

    let app = app(&db);
    let (status, body) = call(
        &app,
        "POST",
        "/api/v1/api-keys",
        &t.key,
        Some(json!({"name": "fresh"})),
    )
    .await;
    assert!(status.is_success(), "{status} {body}");
}

/// API-2: request validation resolved webhook hostnames with blocking DNS on
/// the async runtime (a slow name server stalled the worker thread). The
/// synchronous check no longer resolves; the async one resolves with a
/// timeout; IP literals and known-internal names are refused by both.
#[tokio::test]
async fn api2_url_validation_does_not_block_on_dns() {
    use spooled_backend::security::{
        validate_webhook_url, validate_webhook_url_resolved, UrlValidationOptions,
    };
    let options = UrlValidationOptions {
        is_production: true,
        allow_localhost_in_dev: false,
        check_dns_resolution: true,
    };
    let unresolvable = "https://no-such-host.invalid/hook";
    assert!(
        validate_webhook_url(unresolvable, &options).is_ok(),
        "the synchronous check must not resolve"
    );
    let started = std::time::Instant::now();
    assert!(validate_webhook_url_resolved(unresolvable, &options)
        .await
        .is_err());
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "bounded by the 3 s timeout"
    );
    for blocked in [
        "https://10.0.0.1/hook",
        "https://127.0.0.1/hook",
        "https://[::1]/hook",
        "https://169.254.169.254/latest",
        "https://metadata.google.internal/x",
        "https://redis/x",
    ] {
        assert!(
            validate_webhook_url(blocked, &options).is_err(),
            "{blocked}"
        );
        assert!(
            validate_webhook_url_resolved(blocked, &options)
                .await
                .is_err(),
            "{blocked}"
        );
    }
}

/// API-4: the webhook URL policy treated only `RUST_ENV=production` as
/// production, while the server treats unset or unknown RUST_ENV as
/// production (fail-safe); with RUST_ENV unset or `staging`, loopback and
/// plain-HTTP webhook targets were accepted. The policy now follows the
/// server's environment. (nextest runs each test in its own process, so the
/// process-wide policy set here does not leak into other tests.)
#[test]
fn api4_url_policy_follows_the_server_environment() {
    use spooled_backend::config::Environment;
    use spooled_backend::security::{
        set_url_policy_environment, validate_webhook_url, UrlValidationOptions,
    };
    // Before any settings are applied: RUST_ENV with the server's own parse.
    assert_eq!(Environment::from_rust_env(None), Environment::Production);
    assert_eq!(
        Environment::from_rust_env(Some("staging")),
        Environment::Staging
    );
    assert_eq!(
        Environment::from_rust_env(Some("prod")),
        Environment::Production
    );
    assert_eq!(
        Environment::from_rust_env(Some("dev")),
        Environment::Development
    );

    set_url_policy_environment(&Environment::Staging);
    let options = UrlValidationOptions::default();
    assert!(options.is_production);
    assert!(validate_webhook_url("http://127.0.0.1:9/hook", &options).is_err());
    assert!(validate_webhook_url("http://example.com/hook", &options).is_err());

    set_url_policy_environment(&Environment::Development);
    let options = UrlValidationOptions::default();
    assert!(!options.is_production);
    assert!(validate_webhook_url("http://127.0.0.1:9/hook", &options).is_ok());
}

/// API-5 and API-6: settings that used to fail open or fail late.
#[test]
fn api5_api6_config_parsing() {
    use spooled_backend::config::{
        db_min_connections_from, registration_mode_from, server_host_from, RegistrationMode,
    };
    // REGISTRATION_MODE: trimmed, case-insensitive; a typo is an error, not `open`.
    assert_eq!(
        registration_mode_from(None).unwrap(),
        RegistrationMode::Open
    );
    assert_eq!(
        registration_mode_from(Some(" Closed ")).unwrap(),
        RegistrationMode::Closed
    );
    assert_eq!(
        registration_mode_from(Some("INVITE")).unwrap(),
        RegistrationMode::Invite
    );
    assert_eq!(
        registration_mode_from(Some("open")).unwrap(),
        RegistrationMode::Open
    );
    let err = registration_mode_from(Some("close"))
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("REGISTRATION_MODE") && err.contains("close"),
        "{err}"
    );

    // HOST: localhost binds 127.0.0.1; a name that is not an IP is refused at load.
    assert_eq!(server_host_from(None).unwrap(), "0.0.0.0");
    assert_eq!(server_host_from(Some("localhost")).unwrap(), "127.0.0.1");
    assert_eq!(server_host_from(Some(" :: ")).unwrap(), "::");
    assert_eq!(server_host_from(Some("[::1]")).unwrap(), "::1");
    let err = server_host_from(Some("api.example.com"))
        .unwrap_err()
        .to_string();
    assert!(err.contains("HOST"), "{err}");

    // DATABASE_MIN_CONNECTIONS defaults to min(5, max); an explicit value stays.
    assert_eq!(db_min_connections_from(None, 25).unwrap(), 5);
    assert_eq!(db_min_connections_from(None, 3).unwrap(), 3);
    assert_eq!(db_min_connections_from(Some("2"), 3).unwrap(), 2);
    assert_eq!(db_min_connections_from(Some("9"), 3).unwrap(), 9);
}

/// API-7 and API-8, pinned in the source: key creation hashes through the
/// bounded blocking-pool gate (bcrypt cost 12 inline parked an async worker
/// ~250 ms per call, reachable without a key via public org create), and the
/// login rate-limit warning logs a masked address.
#[test]
fn api7_api8_key_hashing_off_the_runtime_and_masked_email_log() {
    for (file, src) in [
        (
            "organizations.rs",
            include_str!("../src/api/handlers/organizations.rs"),
        ),
        (
            "api_keys.rs",
            include_str!("../src/api/handlers/api_keys.rs"),
        ),
        (
            "email_login.rs",
            include_str!("../src/api/handlers/email_login.rs"),
        ),
        ("admin.rs", include_str!("../src/api/handlers/admin.rs")),
    ] {
        assert!(
            !src.contains("bcrypt::hash("),
            "{file} must hash through bcrypt_gate::hash"
        );
    }
    let email_login = include_str!("../src/api/handlers/email_login.rs");
    assert!(!email_login.contains("warn!(email = %email,"));
}

async fn updated_at(pool: &sqlx::PgPool, job_id: &str) -> chrono::DateTime<chrono::Utc> {
    sqlx::query_scalar("SELECT updated_at FROM jobs WHERE id = $1")
        .bind(job_id)
        .fetch_one(pool)
        .await
        .unwrap()
}

/// Make a job look finished long ago.
async fn age_finished(
    pool: &sqlx::PgPool,
    job_id: &str,
    status: &str,
) -> chrono::DateTime<chrono::Utc> {
    sqlx::query(
        "UPDATE jobs SET status = $2, completed_at = NOW() - INTERVAL '2 days', \
         assigned_worker_id = NULL, lease_id = NULL, lease_expires_at = NULL WHERE id = $1",
    )
    .bind(job_id)
    .bind(status)
    .execute(pool)
    .await
    .unwrap();
    // The jobs_updated_at trigger stamps every UPDATE; disable it for the backdate.
    sqlx::query("ALTER TABLE jobs DISABLE TRIGGER jobs_updated_at")
        .execute(pool)
        .await
        .unwrap();
    sqlx::query("UPDATE jobs SET updated_at = NOW() - INTERVAL '2 days' WHERE id = $1")
        .bind(job_id)
        .execute(pool)
        .await
        .unwrap();
    sqlx::query("ALTER TABLE jobs ENABLE TRIGGER jobs_updated_at")
        .execute(pool)
        .await
        .unwrap();
    updated_at(pool, job_id).await
}

/// CORE-7: an idempotent replay (REST create and bulk, gRPC Enqueue, inbound
/// webhook) rewrote `updated_at` of the existing job, however long ago it
/// finished, and retention, `failed_24h` and DLQ purge `older_than` age
/// finished jobs by it. A replay now leaves the row as it is and still
/// answers with its id.
#[tokio::test]
async fn core7_idempotent_replays_leave_a_finished_job_untouched() {
    use spooled_backend::grpc::proto::queue_service_server::QueueService;
    use spooled_backend::grpc::proto::EnqueueRequest;

    let db = TestDatabase::new().await;
    let t = seed_tenant(db.pool()).await;
    sqlx::query(
        "UPDATE organizations SET settings = '{\"webhook_token\":\"whk_c7\"}'::JSONB WHERE id = $1",
    )
    .bind(&t.org_id)
    .execute(db.pool())
    .await
    .unwrap();
    let app = app(&db);

    let (status, body) = call(
        &app,
        "POST",
        "/api/v1/jobs",
        &t.key,
        Some(json!({"queue_name": "c7", "payload": {"n": 1}, "idempotency_key": "c7-key"})),
    )
    .await;
    assert!(status.is_success(), "{status} {body}");
    let job_id = body["id"].as_str().unwrap().to_string();
    let aged = age_finished(db.pool(), &job_id, "deadletter").await;

    let (status, body) = call(
        &app,
        "POST",
        "/api/v1/jobs",
        &t.key,
        Some(json!({"queue_name": "c7", "payload": {"n": 1}, "idempotency_key": "c7-key"})),
    )
    .await;
    assert!(status.is_success(), "{status} {body}");
    assert_eq!(body["id"], job_id.as_str());
    assert_eq!(body["created"], false);

    let (status, body) = call(
        &app,
        "POST",
        "/api/v1/jobs/bulk",
        &t.key,
        Some(json!({"queue_name": "c7", "jobs": [
            {"payload": {"n": 1}, "idempotency_key": "c7-key"},
            {"payload": {"n": 2}, "idempotency_key": "c7-new"}
        ]})),
    )
    .await;
    assert!(status.is_success(), "{status} {body}");
    assert_eq!(body["succeeded"][0]["job_id"], job_id.as_str(), "{body}");
    assert_eq!(body["succeeded"][0]["created"], false, "{body}");
    assert_eq!(body["succeeded"][1]["created"], true, "{body}");

    let (status, body) = custom_webhook(
        &app,
        &t.org_id,
        "whk_c7",
        json!({"queue_name": "c7", "payload": {}, "idempotency_key": "c7-key"}),
    )
    .await;
    assert!(status.is_success(), "{status} {body}");
    assert_eq!(body["job_id"], job_id.as_str());
    assert_eq!(body["status"], "deadletter");

    let svc = grpc_queue_service(&db);
    let res = svc
        .enqueue(grpc(
            &t.key,
            EnqueueRequest {
                queue_name: "c7".to_string(),
                idempotency_key: "c7-key".to_string(),
                ..Default::default()
            },
        ))
        .await
        .expect("enqueue")
        .into_inner();
    assert_eq!(res.job_id, job_id);
    assert!(!res.created);

    assert_eq!(
        updated_at(db.pool(), &job_id).await,
        aged,
        "no replay touched the job"
    );
}

/// CORE-7: a dependency completing recomputed `dependencies_met` of every
/// dependent, so a dependent that had long finished got a fresh `updated_at`.
/// Only waiting (pending or scheduled) dependents are touched now.
#[tokio::test]
async fn core7_dependency_completion_leaves_finished_dependents_untouched() {
    let db = TestDatabase::new().await;
    let t = seed_tenant(db.pool()).await;
    let dep = insert_job(db.pool(), &t.org_id, "default", "processing", None).await;
    let finished = insert_job(db.pool(), &t.org_id, "default", "pending", None).await;
    let waiting = insert_job(db.pool(), &t.org_id, "default", "pending", None).await;
    for child in [&finished, &waiting] {
        sqlx::query(
            "INSERT INTO job_dependencies (job_id, depends_on_job_id, dependency_type) VALUES ($1, $2, 'all')",
        )
        .bind(child)
        .bind(&dep)
        .execute(db.pool())
        .await
        .unwrap();
        sqlx::query("UPDATE jobs SET dependencies_met = FALSE WHERE id = $1")
            .bind(child)
            .execute(db.pool())
            .await
            .unwrap();
    }
    let aged = age_finished(db.pool(), &finished, "failed").await;

    sqlx::query("UPDATE jobs SET status = 'completed', completed_at = NOW() WHERE id = $1")
        .bind(&dep)
        .execute(db.pool())
        .await
        .unwrap();

    assert_eq!(updated_at(db.pool(), &finished).await, aged);
    let (met,): (Option<bool>,) = sqlx::query_as("SELECT dependencies_met FROM jobs WHERE id = $1")
        .bind(&waiting)
        .fetch_one(db.pool())
        .await
        .unwrap();
    assert_eq!(met, Some(true), "a waiting dependent is still released");
}

/// CORE-2: the delete trigger repeats the sweep's cancel reasons as SQL
/// strings; they must stay the ones the retries re-open.
#[test]
fn core2_delete_trigger_names_the_sweep_reasons() {
    let migration = include_str!(
        "../migrations/20261116120100_deleted_dependency_closes_cancelled_dependents.sql"
    );
    for reason in spooled_backend::models::DEPENDENCY_CANCEL_REASONS {
        assert!(
            migration.contains(&format!("'{reason}'")),
            "the delete trigger must name the sweep reason {reason:?}"
        );
    }
}
