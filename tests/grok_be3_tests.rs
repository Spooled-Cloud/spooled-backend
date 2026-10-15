#![cfg(feature = "docker-tests")]
//! LIVE2-03 .. LIVE2-08. gRPC and SSE regressions from the live probe.
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
use tonic::Code;
use tower::ServiceExt;

use spooled_backend::api::{router, AppState};
use spooled_backend::config::{QueueSettings, Settings, WorkerSettings};
use spooled_backend::db::Database;
use spooled_backend::grpc::convert::json_to_struct;
use spooled_backend::grpc::proto::queue_service_server::QueueService;
use spooled_backend::grpc::proto::worker_service_server::WorkerService;
use spooled_backend::grpc::proto::{
    CompleteRequest, DequeueRequest, EnqueueRequest, GetQueueStatsRequest, HeartbeatRequest,
    RegisterWorkerRequest,
};
use spooled_backend::grpc::services::{QueueServiceImpl, WorkerServiceImpl};
use spooled_backend::observability::Metrics;
use spooled_backend::outgoing_webhooks::service::OutgoingWebhookService;

use common::TestDatabase;

struct Tenant {
    org_id: String,
    key: String,
}

/// Seed an org whose billing email is verified, with one unrestricted key.
async fn seed_tenant(pool: &sqlx::PgPool) -> Tenant {
    let org_id = uuid::Uuid::new_v4().to_string();
    let slug = format!("g3-{}", &org_id[..8]);
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
         VALUES ($1, $2, $3, $4, $5, 'g3', ARRAY[]::TEXT[], TRUE, NOW())",
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

fn queue_settings() -> QueueSettings {
    QueueSettings {
        default_max_retries: 3,
        default_timeout_secs: 300,
        max_payload_size_bytes: 1024 * 1024,
    }
}

fn worker_settings() -> WorkerSettings {
    WorkerSettings {
        heartbeat_interval_secs: 10,
        lease_duration_secs: 30,
        grpc_default_lease_duration_secs: 300,
        max_concurrency: 10,
        fallback_poll_interval_secs: 5,
    }
}

fn queue_service(db: &TestDatabase) -> QueueServiceImpl {
    QueueServiceImpl::new(
        Arc::clone(&db.pool),
        Arc::new(Metrics::new()),
        None,
        Arc::new(OutgoingWebhookService::new((*db.pool).clone(), 1, 4)),
        queue_settings(),
        false,
        worker_settings(),
    )
}

fn worker_service(db: &TestDatabase) -> WorkerServiceImpl {
    WorkerServiceImpl::new(
        Arc::clone(&db.pool),
        Arc::new(Metrics::new()),
        None,
        Arc::new(OutgoingWebhookService::new((*db.pool).clone(), 1, 4)),
        false,
        worker_settings(),
    )
}

fn grpc<T>(key: &str, body: T) -> tonic::Request<T> {
    let mut request = tonic::Request::new(body);
    request
        .metadata_mut()
        .insert("x-api-key", key.parse().unwrap());
    request
}

fn enqueue_req(queue: &str) -> EnqueueRequest {
    EnqueueRequest {
        queue_name: queue.to_string(),
        payload: None,
        priority: 0,
        max_retries: 0,
        timeout_seconds: 0,
        scheduled_at: None,
        idempotency_key: String::new(),
        tags: Default::default(),
    }
}

fn dequeue_req(queue: &str, worker: &str) -> DequeueRequest {
    DequeueRequest {
        queue_name: queue.to_string(),
        worker_id: worker.to_string(),
        lease_duration_secs: 60,
        batch_size: 1,
    }
}

fn register_req(queue: &str, worker_id: &str) -> RegisterWorkerRequest {
    RegisterWorkerRequest {
        queue_name: queue.to_string(),
        hostname: "h".to_string(),
        worker_type: String::new(),
        max_concurrency: 0,
        version: String::new(),
        metadata: Default::default(),
        worker_id: Some(worker_id.to_string()),
    }
}

type SseBody = axum::body::BodyDataStream;

async fn sse_open(app: &Router, uri: &str, headers: &[(&str, &str)]) -> (StatusCode, SseBody) {
    let mut req = Request::builder().uri(uri);
    for (name, value) in headers {
        req = req.header(*name, *value);
    }
    let res = app
        .clone()
        .oneshot(req.body(Body::empty()).unwrap())
        .await
        .unwrap();
    (res.status(), res.into_body().into_data_stream())
}

/// Stream text read within `limit`, and whether the stream ended.
async fn sse_read(stream: &mut SseBody, limit: Duration) -> (String, bool) {
    let deadline = tokio::time::Instant::now() + limit;
    let mut seen = String::new();
    loop {
        match tokio::time::timeout_at(deadline, stream.next()).await {
            Ok(Some(Ok(chunk))) => seen.push_str(&String::from_utf8_lossy(&chunk)),
            Ok(_) => return (seen, true),
            Err(_) => return (seen, false),
        }
    }
}

fn queue_stats_pending(sse: &str) -> i64 {
    let data = sse
        .lines()
        .find_map(|line| line.strip_prefix("data:"))
        .unwrap_or_else(|| panic!("queue.stats data line missing: {sse}"));
    let json: Value = serde_json::from_str(data.trim()).unwrap_or_else(|e| panic!("{e}: {sse}"));
    json["data"]["pending"]
        .as_i64()
        .unwrap_or_else(|| panic!("pending missing: {sse}"))
}

async fn job_result(pool: &sqlx::PgPool, id: &str) -> Option<Value> {
    let (result,): (Option<Value>,) = sqlx::query_as("SELECT result FROM jobs WHERE id = $1")
        .bind(id)
        .fetch_one(pool)
        .await
        .unwrap();
    result
}

async fn job_max_retries(pool: &sqlx::PgPool, id: &str) -> i32 {
    let (max_retries,): (i32,) = sqlx::query_as("SELECT max_retries FROM jobs WHERE id = $1")
        .bind(id)
        .fetch_one(pool)
        .await
        .unwrap();
    max_retries
}

async fn job_status(pool: &sqlx::PgPool, id: &str) -> String {
    let (status,): (String,) = sqlx::query_as("SELECT status FROM jobs WHERE id = $1")
        .bind(id)
        .fetch_one(pool)
        .await
        .unwrap();
    status
}

// LIVE2-03: SSE queue.stats must not count a scheduled job as pending.
#[tokio::test]
async fn live2_03_sse_queue_stats_does_not_count_scheduled_as_pending() {
    let db = TestDatabase::new().await;
    let t = seed_tenant(db.pool()).await;
    let app = app(&db);
    let queue = format!("sched-{}", &t.org_id[..8]);

    sqlx::query(
        "INSERT INTO jobs (id, organization_id, queue_name, payload, status, priority, max_retries, scheduled_at, created_at, updated_at) \
         VALUES ($1, $2, $3, '{}'::JSONB, 'scheduled', 0, 3, NOW() + INTERVAL '3 hours', NOW(), NOW())",
    )
    .bind(uuid::Uuid::new_v4().to_string())
    .bind(&t.org_id)
    .bind(&queue)
    .execute(db.pool())
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO jobs (id, organization_id, queue_name, payload, status, priority, max_retries, created_at, updated_at) \
         VALUES ($1, $2, $3, '{}'::JSONB, 'pending', 0, 3, NOW(), NOW())",
    )
    .bind(uuid::Uuid::new_v4().to_string())
    .bind(&t.org_id)
    .bind(&queue)
    .execute(db.pool())
    .await
    .unwrap();

    let auth = bearer(&t.key);
    let (status, body) = call(
        &app,
        "GET",
        &format!("/api/v1/queues/{queue}/stats"),
        &[("Authorization", &auth)],
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["pending_jobs"], 1, "{body}");

    let service = queue_service(&db);
    let stats = service
        .get_queue_stats(grpc(
            &t.key,
            GetQueueStatsRequest {
                queue_name: queue.clone(),
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(stats.pending, 1);
    assert_eq!(stats.scheduled, 1);

    let (status, mut stream) = sse_open(
        &app,
        &format!("/api/v1/events/queues/{queue}"),
        &[("Authorization", &auth)],
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    // The queue stream sleeps 5s before the first `queue.stats`.
    let (seen, _) = sse_read(&mut stream, Duration::from_secs(12)).await;
    assert!(seen.contains("queue.stats"), "{seen}");
    assert_eq!(queue_stats_pending(&seen), 1, "{seen}");
}

// LIVE2-04: RegisterWorker queue names are 1-100 characters, like REST.
#[tokio::test]
async fn live2_04_grpc_register_worker_rejects_queue_names_over_100() {
    let db = TestDatabase::new().await;
    let t = seed_tenant(db.pool()).await;
    let service = worker_service(&db);

    let err = service
        .register(grpc(&t.key, register_req(&"q".repeat(101), "long-101")))
        .await
        .unwrap_err();
    assert_eq!(err.code(), Code::InvalidArgument);
    assert_eq!(err.message(), "Queue name must be 1-100 characters");

    let err = service
        .register(grpc(&t.key, register_req(&"q".repeat(120), "long-120")))
        .await
        .unwrap_err();
    assert_eq!(err.code(), Code::InvalidArgument);

    let (workers,): (i64,) =
        sqlx::query_as("SELECT COUNT(*) FROM workers WHERE organization_id = $1")
            .bind(&t.org_id)
            .fetch_one(db.pool())
            .await
            .unwrap();
    assert_eq!(workers, 0, "a rejected name must not insert a worker");

    let worker_id = service
        .register(grpc(&t.key, register_req(&"q".repeat(100), "long-100")))
        .await
        .unwrap()
        .into_inner()
        .worker_id;
    assert_eq!(worker_id, "long-100");
}

// LIVE2-05: Complete with no result stores NULL, like REST. A present
// result, including `{}`, is stored as that JSON.
#[tokio::test]
async fn live2_05_grpc_complete_without_result_stores_null() {
    let db = TestDatabase::new().await;
    let t = seed_tenant(db.pool()).await;
    let service = queue_service(&db);
    let queue = "cresult";
    let worker = "w-cresult";

    async fn claim_and_complete(
        service: &QueueServiceImpl,
        pool: &sqlx::PgPool,
        key: &str,
        queue: &str,
        worker: &str,
        result: Option<prost_types::Struct>,
    ) -> Option<Value> {
        service
            .enqueue(grpc(key, enqueue_req(queue)))
            .await
            .unwrap();
        let job = service
            .dequeue(grpc(key, dequeue_req(queue, worker)))
            .await
            .unwrap()
            .into_inner()
            .jobs
            .pop()
            .expect("dequeued job");
        let done = service
            .complete(grpc(
                key,
                CompleteRequest {
                    job_id: job.id.clone(),
                    worker_id: worker.to_string(),
                    result,
                    lease_id: job.lease_id,
                },
            ))
            .await
            .unwrap()
            .into_inner();
        assert!(done.success);
        job_result(pool, &job.id).await
    }

    let omitted = claim_and_complete(&service, db.pool(), &t.key, queue, worker, None).await;
    assert!(
        omitted.is_none(),
        "omitted result must be SQL NULL, got {omitted:?}"
    );

    let empty = json_to_struct(&json!({})).unwrap();
    let stored_empty =
        claim_and_complete(&service, db.pool(), &t.key, queue, worker, Some(empty)).await;
    assert_eq!(stored_empty, Some(json!({})));

    let value = json_to_struct(&json!({"ok": true})).unwrap();
    let stored = claim_and_complete(&service, db.pool(), &t.key, queue, worker, Some(value)).await;
    assert_eq!(stored, Some(json!({"ok": true})));
}

// LIVE2-06: a negative max_retries is INVALID_ARGUMENT. 0 (proto3 omit)
// still takes the queue default.
#[tokio::test]
async fn live2_06_grpc_enqueue_rejects_negative_max_retries() {
    let db = TestDatabase::new().await;
    let t = seed_tenant(db.pool()).await;
    let service = queue_service(&db);
    let queue = "retries";

    sqlx::query(
        "INSERT INTO queue_config (id, organization_id, queue_name, max_retries, default_timeout, enabled, created_at, updated_at) \
         VALUES ($1, $2, $3, 5, 300, TRUE, NOW(), NOW())",
    )
    .bind(uuid::Uuid::new_v4().to_string())
    .bind(&t.org_id)
    .bind(queue)
    .execute(db.pool())
    .await
    .unwrap();

    let mut negative = enqueue_req(queue);
    negative.max_retries = -5;
    let err = service.enqueue(grpc(&t.key, negative)).await.unwrap_err();
    assert_eq!(err.code(), Code::InvalidArgument);
    assert_eq!(err.message(), "Max retries must be between 0 and 100");

    let (rows,): (i64,) =
        sqlx::query_as("SELECT COUNT(*) FROM jobs WHERE organization_id = $1 AND queue_name = $2")
            .bind(&t.org_id)
            .bind(queue)
            .fetch_one(db.pool())
            .await
            .unwrap();
    assert_eq!(rows, 0, "a rejected enqueue must not insert a job");

    // 0 is what proto3 sends when the field is omitted.
    let omitted = service
        .enqueue(grpc(&t.key, enqueue_req(queue)))
        .await
        .unwrap()
        .into_inner();
    assert!(omitted.created);
    assert_eq!(job_max_retries(db.pool(), &omitted.job_id).await, 5);

    let mut explicit = enqueue_req(queue);
    explicit.max_retries = 7;
    let created = service
        .enqueue(grpc(&t.key, explicit))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(job_max_retries(db.pool(), &created.job_id).await, 7);
}

// LIVE2-07: a live lease with the wrong token is the REST mismatch text,
// not "Lease expired". A lease that has actually ended still says expired.
#[tokio::test]
async fn live2_07_grpc_lease_mismatch_message_matches_rest() {
    let db = TestDatabase::new().await;
    let t = seed_tenant(db.pool()).await;
    let service = queue_service(&db);
    let worker = "w-lease";
    let lease = "real-lease";
    let job_id = uuid::Uuid::new_v4().to_string();
    sqlx::query(
        "INSERT INTO jobs (id, organization_id, queue_name, payload, status, assigned_worker_id, \
         lease_id, lease_expires_at, max_retries, created_at, updated_at) \
         VALUES ($1, $2, 'lm', '{}'::JSONB, 'processing', $3, $4, NOW() + INTERVAL '5 minutes', 3, NOW(), NOW())",
    )
    .bind(&job_id)
    .bind(&t.org_id)
    .bind(worker)
    .bind(lease)
    .execute(db.pool())
    .await
    .unwrap();

    let mismatch = service
        .complete(grpc(
            &t.key,
            CompleteRequest {
                job_id: job_id.clone(),
                worker_id: worker.to_string(),
                result: None,
                lease_id: "deadbeef-not-the-lease".to_string(),
            },
        ))
        .await
        .unwrap_err();
    assert_eq!(mismatch.code(), Code::FailedPrecondition);
    assert_eq!(
        mismatch.message(),
        "lease_id does not match the job's current lease; this worker no longer holds it"
    );
    assert_eq!(job_status(db.pool(), &job_id).await, "processing");

    sqlx::query("UPDATE jobs SET lease_expires_at = NOW() - INTERVAL '1 minute' WHERE id = $1")
        .bind(&job_id)
        .execute(db.pool())
        .await
        .unwrap();
    let expired = service
        .complete(grpc(
            &t.key,
            CompleteRequest {
                job_id: job_id.clone(),
                worker_id: worker.to_string(),
                result: None,
                lease_id: lease.to_string(),
            },
        ))
        .await
        .unwrap_err();
    assert_eq!(expired.code(), Code::FailedPrecondition);
    assert_eq!(expired.message(), "Lease expired");
    assert_eq!(job_status(db.pool(), &job_id).await, "processing");
}

// LIVE2-08: an unknown heartbeat status is INVALID_ARGUMENT and does not
// rewrite a degraded worker to healthy. Empty still keeps the stored status.
#[tokio::test]
async fn live2_08_grpc_heartbeat_rejects_unknown_status() {
    let db = TestDatabase::new().await;
    let t = seed_tenant(db.pool()).await;
    let service = worker_service(&db);
    let worker_id = service
        .register(grpc(&t.key, register_req("hb", "w-hb")))
        .await
        .unwrap()
        .into_inner()
        .worker_id;

    let ack = service
        .heartbeat(grpc(
            &t.key,
            HeartbeatRequest {
                worker_id: worker_id.clone(),
                current_jobs: 0,
                status: "degraded".to_string(),
                metadata: Default::default(),
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert!(ack.acknowledged);

    let err = service
        .heartbeat(grpc(
            &t.key,
            HeartbeatRequest {
                worker_id: worker_id.clone(),
                current_jobs: 0,
                status: "not-a-status".to_string(),
                metadata: Default::default(),
            },
        ))
        .await
        .unwrap_err();
    assert_eq!(err.code(), Code::InvalidArgument);
    assert_eq!(
        err.message(),
        "Status must be one of: healthy, degraded, draining, offline"
    );

    let (status,): (String,) = sqlx::query_as("SELECT status FROM workers WHERE id = $1")
        .bind(&worker_id)
        .fetch_one(db.pool())
        .await
        .unwrap();
    assert_eq!(status, "degraded");

    let ack = service
        .heartbeat(grpc(
            &t.key,
            HeartbeatRequest {
                worker_id: worker_id.clone(),
                current_jobs: 0,
                status: String::new(),
                metadata: Default::default(),
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert!(ack.acknowledged);
    let (status,): (String,) = sqlx::query_as("SELECT status FROM workers WHERE id = $1")
        .bind(&worker_id)
        .fetch_one(db.pool())
        .await
        .unwrap();
    assert_eq!(status, "degraded");
}

// A queue name RegisterWorker accepts (100 characters of 3-byte letters) must
// also pass the shared validator Dequeue, Enqueue and GetQueueStats use.
#[test]
fn grpc_queue_name_validator_counts_characters_not_bytes() {
    use spooled_backend::grpc::validation::validate_queue_name;

    let name = "\u{6f22}".repeat(100);
    assert_eq!(name.len(), 300);
    assert!(validate_queue_name(&name).is_ok());
    assert!(validate_queue_name(&"a".repeat(256)).is_err());
}
