#![cfg(feature = "docker-tests")]
//! Follow-ups on the round-2 fixes: a failed delivery-row write must not drop
//! the retry chain, expiry must finish a workflow that still has completed
//! jobs, overlapping any-mode deletes must cancel the child, a new schedule
//! or workflow queue shares the POST /jobs queue cap, and gRPC RegisterWorker
//! rejects a queue name claim would reject.

mod common;

use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::Router;
use serde_json::{json, Value};
use tonic::Code;
use tower::ServiceExt;
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

use spooled_backend::api::{router, AppState};
use spooled_backend::config::{Settings, WorkerSettings};
use spooled_backend::db::Database;
use spooled_backend::grpc::proto::worker_service_server::WorkerService;
use spooled_backend::grpc::proto::RegisterWorkerRequest;
use spooled_backend::grpc::services::WorkerServiceImpl;
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
    let slug = format!("fu-{}", &org_id[..8]);
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
         VALUES ($1, $2, $3, $4, $5, 'fu', ARRAY[]::TEXT[], TRUE, NOW())",
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

fn worker_service(db: &TestDatabase) -> WorkerServiceImpl {
    WorkerServiceImpl::new(
        Arc::clone(&db.pool),
        Arc::new(Metrics::new()),
        None,
        Arc::new(OutgoingWebhookService::new((*db.pool).clone(), 1, 4)),
        false,
        WorkerSettings {
            heartbeat_interval_secs: 10,
            lease_duration_secs: 30,
            grpc_default_lease_duration_secs: 300,
            max_concurrency: 10,
            fallback_poll_interval_secs: 5,
        },
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

async fn cap_queues(pool: &sqlx::PgPool, org_id: &str) {
    sqlx::query(
        "UPDATE organizations SET custom_limits = '{\"max_queues\": 1}'::JSONB WHERE id = $1",
    )
    .bind(org_id)
    .execute(pool)
    .await
    .unwrap();
}

async fn queue_count(pool: &sqlx::PgPool, org_id: &str) -> i64 {
    sqlx::query_scalar("SELECT get_org_queue_count($1)")
        .bind(org_id)
        .fetch_one(pool)
        .await
        .unwrap()
}

fn accepted(status: StatusCode) -> bool {
    status == StatusCode::OK || status == StatusCode::CREATED
}

async fn insert_job(pool: &sqlx::PgPool, org_id: &str, queue: &str, status: &str) -> String {
    let id = uuid::Uuid::new_v4().to_string();
    sqlx::query(
        "INSERT INTO jobs (id, organization_id, queue_name, payload, status, created_at, updated_at) \
         VALUES ($1, $2, $3, '{}'::JSONB, $4, NOW(), NOW())",
    )
    .bind(&id)
    .bind(org_id)
    .bind(queue)
    .bind(status)
    .execute(pool)
    .await
    .expect("seed job");
    id
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

/// A failed claimed delivery write used to leave retry_claimed_at set and
/// return success, so the scheduled continuation could not claim the row and
/// the rest of the attempt budget never ran.
#[tokio::test]
async fn failed_delivery_write_releases_the_claim_and_retries() {
    let db = TestDatabase::new().await;
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&server)
        .await;

    sqlx::query("CREATE SEQUENCE r0128_fail_delivery_seq")
        .execute(db.pool())
        .await
        .unwrap();
    sqlx::query(
        "CREATE FUNCTION r0128_fail_delivery_write() RETURNS trigger LANGUAGE plpgsql AS $$ \
         BEGIN \
           IF NEW.retry_claimed_at IS NULL \
              AND OLD.retry_claimed_at IS NOT NULL \
              AND NEW.delivered_at IS DISTINCT FROM OLD.delivered_at \
              AND nextval('r0128_fail_delivery_seq') = 1 \
           THEN \
             RAISE EXCEPTION 'delivery write failed'; \
           END IF; \
           RETURN NEW; \
         END $$",
    )
    .execute(db.pool())
    .await
    .unwrap();
    sqlx::query(
        "CREATE TRIGGER r0128_fail_delivery_write BEFORE UPDATE ON outgoing_webhook_deliveries \
         FOR EACH ROW EXECUTE FUNCTION r0128_fail_delivery_write()",
    )
    .execute(db.pool())
    .await
    .unwrap();

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

    let svc = Arc::new(OutgoingWebhookService::new((*db.pool).clone(), 5, 4));
    svc.spawn_dispatch(
        t.org_id.clone(),
        "job.created".to_string(),
        "job-1".to_string(),
        json!({"job_id": "job-1", "queue_name": "default"}),
    );

    let mut saw_retry = false;
    for _ in 0..80 {
        let row: Option<(i32, Option<chrono::DateTime<chrono::Utc>>)> = sqlx::query_as(
            "SELECT attempts, retry_claimed_at FROM outgoing_webhook_deliveries WHERE webhook_id = $1",
        )
        .bind(&webhook_id)
        .fetch_optional(db.pool())
        .await
        .unwrap();
        let requests = server.received_requests().await.unwrap().len();
        if let Some((attempts, claimed)) = row {
            if attempts >= 3 && claimed.is_none() && requests >= 3 {
                saw_retry = true;
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    let requests = server.received_requests().await.unwrap().len();
    let row: Option<(i32, Option<chrono::DateTime<chrono::Utc>>, String)> = sqlx::query_as(
        "SELECT attempts, retry_claimed_at, status FROM outgoing_webhook_deliveries WHERE webhook_id = $1",
    )
    .bind(&webhook_id)
    .fetch_optional(db.pool())
    .await
    .unwrap();
    assert!(
        saw_retry,
        "the attempt after the failed write must run; requests={requests} row={row:?}"
    );
}

/// Expiry of the last open job finishes the workflow from the jobs that remain.
#[tokio::test]
async fn expiry_finishes_a_workflow_with_no_active_job() {
    let db = TestDatabase::new().await;
    let t = seed_tenant(db.pool()).await;

    let completed_wf = insert_workflow(db.pool(), &t.org_id, "running", 2).await;
    let failed_wf = insert_workflow(db.pool(), &t.org_id, "running", 2).await;
    let active_wf = insert_workflow(db.pool(), &t.org_id, "running", 2).await;

    for (wf, status, expires) in [
        (&completed_wf, "completed", false),
        (&completed_wf, "pending", true),
        (&failed_wf, "failed", false),
        (&failed_wf, "pending", true),
        (&active_wf, "processing", false),
        (&active_wf, "pending", true),
    ] {
        let id = uuid::Uuid::new_v4().to_string();
        sqlx::query(
            "INSERT INTO jobs (id, organization_id, queue_name, payload, status, workflow_id, expires_at, created_at, updated_at) \
             VALUES ($1, $2, 'default', '{}'::JSONB, $3, $4, CASE WHEN $5 THEN NOW() - INTERVAL '1 minute' ELSE NULL END, NOW(), NOW())",
        )
        .bind(&id)
        .bind(&t.org_id)
        .bind(status)
        .bind(wf)
        .bind(expires)
        .execute(db.pool())
        .await
        .unwrap();
    }

    scheduler(&db)
        .cleanup_stale_workers()
        .await
        .expect("expiry sweep");

    let status_of = |pool: Arc<sqlx::PgPool>, id: String| async move {
        let status: String = sqlx::query_scalar("SELECT status FROM workflows WHERE id = $1")
            .bind(id)
            .fetch_one(&*pool)
            .await
            .unwrap();
        status
    };
    let pool = Arc::clone(&db.pool);
    // One step completed and the other expired before it ran: the workflow
    // ends, but it did not complete.
    assert_eq!(
        status_of(Arc::clone(&pool), completed_wf).await,
        "cancelled"
    );
    assert_eq!(status_of(Arc::clone(&pool), failed_wf).await, "failed");
    assert_eq!(
        status_of(pool, active_wf).await,
        "running",
        "a workflow with a job still processing stays running"
    );
}

/// Two overlapping deletes of the last any-mode parents must not both decide
/// the other parent is still alive. The second waits, then cancels the child.
#[tokio::test]
async fn overlapping_any_mode_deletes_cancel_the_child() {
    let db = TestDatabase::new().await;
    let t = seed_tenant(db.pool()).await;
    let parent_a = insert_job(db.pool(), &t.org_id, "default", "pending").await;
    let parent_b = insert_job(db.pool(), &t.org_id, "default", "pending").await;
    let child = uuid::Uuid::new_v4().to_string();
    sqlx::query(
        "INSERT INTO jobs (id, organization_id, queue_name, payload, status, dependency_mode, dependencies_met, created_at, updated_at) \
         VALUES ($1, $2, 'default', '{}'::JSONB, 'pending', 'any', FALSE, NOW(), NOW())",
    )
    .bind(&child)
    .bind(&t.org_id)
    .execute(db.pool())
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO job_dependencies (job_id, depends_on_job_id) VALUES ($1, $2), ($1, $3)",
    )
    .bind(&child)
    .bind(&parent_a)
    .bind(&parent_b)
    .execute(db.pool())
    .await
    .unwrap();

    let mut conn_a = db.pool.acquire().await.unwrap();
    sqlx::query("BEGIN").execute(&mut *conn_a).await.unwrap();
    sqlx::query("DELETE FROM jobs WHERE id = $1")
        .bind(&parent_a)
        .execute(&mut *conn_a)
        .await
        .unwrap();

    let pool = Arc::clone(&db.pool);
    let parent_b2 = parent_b.clone();
    let started = Arc::new(tokio::sync::Notify::new());
    let notify = Arc::clone(&started);
    let delete_b = tokio::spawn(async move {
        let mut conn_b = pool.acquire().await.unwrap();
        sqlx::query("BEGIN").execute(&mut *conn_b).await.unwrap();
        sqlx::query("SET lock_timeout = '15s'")
            .execute(&mut *conn_b)
            .await
            .unwrap();
        notify.notify_one();
        sqlx::query("DELETE FROM jobs WHERE id = $1")
            .bind(&parent_b2)
            .execute(&mut *conn_b)
            .await
            .unwrap();
        sqlx::query("COMMIT").execute(&mut *conn_b).await.unwrap();
    });

    started.notified().await;
    let mut blocked = false;
    for _ in 0..100 {
        if delete_b.is_finished() {
            break;
        }
        let waiting: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM pg_stat_activity \
             WHERE wait_event_type = 'Lock' AND query ILIKE '%DELETE FROM jobs%'",
        )
        .fetch_one(db.pool())
        .await
        .unwrap();
        if waiting > 0 {
            blocked = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        blocked,
        "the second delete must wait on the child row before the first commits"
    );

    sqlx::query("COMMIT").execute(&mut *conn_a).await.unwrap();
    delete_b.await.unwrap();

    let (status, met): (String, Option<bool>) =
        sqlx::query_as("SELECT status, dependencies_met FROM jobs WHERE id = $1")
            .bind(&child)
            .fetch_one(db.pool())
            .await
            .unwrap();
    assert_eq!(status, "cancelled");
    assert_ne!(met, Some(true), "a cancelled child must not be released");
}

/// Hold the queues lock across the insert so a concurrent creator is still in
/// its check when the first row commits.
async fn slow_inserts(pool: &sqlx::PgPool, schedules: bool) {
    sqlx::query(
        "CREATE FUNCTION r0128_slow_insert() RETURNS trigger LANGUAGE plpgsql AS $$ \
         BEGIN PERFORM pg_sleep(0.8); RETURN NEW; END $$",
    )
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        "CREATE TRIGGER r0128_slow_jobs BEFORE INSERT ON jobs \
         FOR EACH ROW EXECUTE FUNCTION r0128_slow_insert()",
    )
    .execute(pool)
    .await
    .unwrap();
    if schedules {
        sqlx::query(
            "CREATE TRIGGER r0128_slow_schedules BEFORE INSERT ON schedules \
             FOR EACH ROW EXECUTE FUNCTION r0128_slow_insert()",
        )
        .execute(pool)
        .await
        .unwrap();
    }
}

/// A first schedule and a first job must not both pass max_queues.
#[tokio::test]
async fn schedule_create_shares_the_job_queue_lock() {
    let db = TestDatabase::new().await;
    let t = seed_tenant(db.pool()).await;
    cap_queues(db.pool(), &t.org_id).await;
    slow_inserts(db.pool(), true).await;
    let app = app(&db);

    let (job, schedule) = tokio::join!(
        call(
            &app,
            "POST",
            "/api/v1/jobs",
            &t.key,
            Some(json!({"queue_name": "from-job", "payload": {}})),
        ),
        call(
            &app,
            "POST",
            "/api/v1/schedules",
            &t.key,
            Some(json!({
                "name": "nightly",
                "cron_expression": "* * * * *",
                "queue_name": "from-sched",
                "payload_template": {"n": 1}
            })),
        ),
    );
    let queues = queue_count(db.pool(), &t.org_id).await;
    assert_eq!(queues, 1, "job={job:?} schedule={schedule:?}");
    assert!(
        accepted(job.0) ^ accepted(schedule.0),
        "exactly one create is allowed: job={job:?} schedule={schedule:?}"
    );
    assert!(
        job.0 == StatusCode::TOO_MANY_REQUESTS || schedule.0 == StatusCode::TOO_MANY_REQUESTS,
        "the loser is the queue cap: job={job:?} schedule={schedule:?}"
    );
}

/// A workflow that opens a queue takes the same queues lock as POST /jobs.
#[tokio::test]
async fn workflow_create_shares_the_job_queue_lock() {
    let db = TestDatabase::new().await;
    let t = seed_tenant(db.pool()).await;
    cap_queues(db.pool(), &t.org_id).await;
    slow_inserts(db.pool(), false).await;
    let app = app(&db);

    let (job, workflow) = tokio::join!(
        call(
            &app,
            "POST",
            "/api/v1/jobs",
            &t.key,
            Some(json!({"queue_name": "from-job", "payload": {}})),
        ),
        call(
            &app,
            "POST",
            "/api/v1/workflows",
            &t.key,
            Some(json!({
                "name": "pipe",
                "jobs": [{"key": "step", "queue_name": "from-wf", "payload": {"n": 1}}]
            })),
        ),
    );
    let queues = queue_count(db.pool(), &t.org_id).await;
    assert_eq!(queues, 1, "job={job:?} workflow={workflow:?}");
    assert!(
        accepted(job.0) ^ accepted(workflow.0),
        "exactly one create is allowed: job={job:?} workflow={workflow:?}"
    );
    assert!(
        job.0 == StatusCode::TOO_MANY_REQUESTS || workflow.0 == StatusCode::TOO_MANY_REQUESTS,
        "the loser is the queue cap: job={job:?} workflow={workflow:?}"
    );
}

/// RegisterWorker rejects a queue name that starts with '.' or '-'.
#[tokio::test]
async fn grpc_register_rejects_a_queue_name_starting_with_dot_or_dash() {
    let db = TestDatabase::new().await;
    let t = seed_tenant(db.pool()).await;
    let svc = worker_service(&db);

    for queue in [".hidden", "-hidden"] {
        let mut request = tonic::Request::new(RegisterWorkerRequest {
            queue_name: queue.to_string(),
            hostname: "h".to_string(),
            worker_type: String::new(),
            max_concurrency: 0,
            version: String::new(),
            metadata: Default::default(),
            worker_id: Some(format!("w-{queue}")),
        });
        request
            .metadata_mut()
            .insert("x-api-key", t.key.parse().unwrap());
        let err = svc.register(request).await.expect_err(queue);
        assert_eq!(err.code(), Code::InvalidArgument, "{queue}: {err}");
        assert!(err.message().contains("dots or dashes"), "{queue}: {err}");
    }

    let workers: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM workers WHERE organization_id = $1")
            .bind(&t.org_id)
            .fetch_one(db.pool())
            .await
            .unwrap();
    assert_eq!(workers, 0, "a rejected name must not register a worker");

    let mut request = tonic::Request::new(RegisterWorkerRequest {
        queue_name: "emails".to_string(),
        hostname: "h".to_string(),
        worker_type: String::new(),
        max_concurrency: 0,
        version: String::new(),
        metadata: Default::default(),
        worker_id: Some("w-ok".to_string()),
    });
    request
        .metadata_mut()
        .insert("x-api-key", t.key.parse().unwrap());
    svc.register(request)
        .await
        .expect("a normal queue registers");
}
