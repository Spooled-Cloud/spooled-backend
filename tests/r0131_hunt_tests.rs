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
