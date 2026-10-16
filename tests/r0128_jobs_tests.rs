#![cfg(feature = "docker-tests")]
//! FLOW-06, RT-01, RT-02, ORG-02. Idempotency scope on REST and gRPC, and the
//! new-queue cap serialized the way schedule creation already is.

mod common;

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::Router;
use serde_json::{json, Value};
use tonic::Code;
use tower::ServiceExt;

use spooled_backend::api::{router, AppState};
use spooled_backend::config::{QueueSettings, Settings, WorkerSettings};
use spooled_backend::db::Database;
use spooled_backend::grpc::proto::queue_service_server::QueueService;
use spooled_backend::grpc::proto::EnqueueRequest;
use spooled_backend::grpc::services::QueueServiceImpl;
use spooled_backend::observability::Metrics;
use spooled_backend::outgoing_webhooks::service::OutgoingWebhookService;

use common::TestDatabase;

struct Tenant {
    org_id: String,
    key: String,
}

async fn seed_tenant(pool: &sqlx::PgPool) -> Tenant {
    let org_id = uuid::Uuid::new_v4().to_string();
    let slug = format!("jb-{}", &org_id[..8]);
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
    insert_key(pool, &org_id, &key, &[]).await;
    Tenant { org_id, key }
}

async fn insert_key(pool: &sqlx::PgPool, org_id: &str, key: &str, queues: &[&str]) {
    let hash = bcrypt::hash(key, 4).expect("hash key");
    let queues: Vec<String> = queues.iter().map(|q| (*q).to_string()).collect();
    sqlx::query(
        "INSERT INTO api_keys (id, organization_id, key_hash, key_prefix, lookup_hash, name, queues, is_active, created_at) \
         VALUES ($1, $2, $3, $4, $5, 'jb', $6, TRUE, NOW())",
    )
    .bind(uuid::Uuid::new_v4().to_string())
    .bind(org_id)
    .bind(&hash)
    .bind(key.chars().take(8).collect::<String>())
    .bind(spooled_backend::models::api_key_lookup_hash(key))
    .bind(&queues)
    .execute(pool)
    .await
    .expect("seed key");
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

fn queue_service(db: &TestDatabase) -> QueueServiceImpl {
    QueueServiceImpl::new(
        Arc::clone(&db.pool),
        Arc::new(Metrics::new()),
        None,
        Arc::new(OutgoingWebhookService::new((*db.pool).clone(), 1, 4)),
        QueueSettings {
            default_max_retries: 3,
            default_timeout_secs: 300,
            max_payload_size_bytes: 1024 * 1024,
        },
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

fn grpc<T>(key: &str, body: T) -> tonic::Request<T> {
    let mut request = tonic::Request::new(body);
    request
        .metadata_mut()
        .insert("x-api-key", key.parse().unwrap());
    request
}

fn enqueue_req(queue: &str, idempotency_key: &str) -> EnqueueRequest {
    EnqueueRequest {
        queue_name: queue.to_string(),
        payload: None,
        priority: 0,
        max_retries: 0,
        timeout_seconds: 0,
        scheduled_at: None,
        idempotency_key: idempotency_key.to_string(),
        tags: Default::default(),
    }
}

async fn updated_at(pool: &sqlx::PgPool, id: &str) -> chrono::DateTime<chrono::Utc> {
    sqlx::query_scalar("SELECT updated_at FROM jobs WHERE id = $1")
        .bind(id)
        .fetch_one(pool)
        .await
        .unwrap()
}

fn assert_conflict_hides_job(status: StatusCode, body: &Value, job_id: &str) {
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["code"], "CONFLICT");
    assert_eq!(
        body["message"],
        spooled_backend::models::IDEMPOTENCY_KEY_IN_USE
    );
    assert!(
        !body.to_string().contains(job_id),
        "out-of-scope replay leaked {job_id}: {body}"
    );
}

/// FLOW-06: a REST create or bulk replay that hits a job outside the key's
/// queue scope is 409 and does not return or touch that job.
#[tokio::test]
async fn flow06_rest_idempotency_replay_outside_queue_scope_is_409() {
    let db = TestDatabase::new().await;
    let app = app(&db);
    let t = seed_tenant(db.pool()).await;
    let auth = bearer(&t.key);

    let (status, body) = call(
        &app,
        "POST",
        "/api/v1/jobs",
        &[("Authorization", &auth)],
        Some(json!({"queue_name": "hidden", "payload": {}, "idempotency_key": "idem-scope"})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let hidden_id = body["id"].as_str().unwrap().to_string();
    let before = updated_at(db.pool(), &hidden_id).await;

    let scoped = format!("sp_test_{}", uuid::Uuid::new_v4().simple());
    insert_key(db.pool(), &t.org_id, &scoped, &["visible"]).await;
    let scoped_auth = bearer(&scoped);

    let (status, body) = call(
        &app,
        "POST",
        "/api/v1/jobs",
        &[("Authorization", &scoped_auth)],
        Some(json!({"queue_name": "visible", "payload": {}, "idempotency_key": "idem-scope"})),
    )
    .await;
    assert_conflict_hides_job(status, &body, &hidden_id);
    assert_eq!(updated_at(db.pool(), &hidden_id).await, before);

    let (status, body) = call(
        &app,
        "POST",
        "/api/v1/jobs/bulk",
        &[("Authorization", &scoped_auth)],
        Some(json!({
            "queue_name": "visible",
            "jobs": [{"payload": {"n": 1}, "idempotency_key": "idem-scope"}]
        })),
    )
    .await;
    assert_conflict_hides_job(status, &body, &hidden_id);
    assert_eq!(updated_at(db.pool(), &hidden_id).await, before);
    let visible: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM jobs WHERE organization_id = $1 AND queue_name = 'visible'",
    )
    .bind(&t.org_id)
    .fetch_one(db.pool())
    .await
    .unwrap();
    assert_eq!(visible, 0, "the rejected batch must not insert");

    // A key that can see the stored queue still replays it.
    let (status, body) = call(
        &app,
        "POST",
        "/api/v1/jobs",
        &[("Authorization", &auth)],
        Some(json!({"queue_name": "hidden", "payload": {}, "idempotency_key": "idem-scope"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["id"], hidden_id);
    assert_eq!(body["created"], false);
}

/// RT-01: gRPC answers an out-of-scope idempotency replay like REST, without
/// the other queue's job id.
#[tokio::test]
async fn rt01_grpc_idempotency_replay_outside_queue_scope_hides_the_job() {
    let db = TestDatabase::new().await;
    let app = app(&db);
    let t = seed_tenant(db.pool()).await;
    let auth = bearer(&t.key);
    let (status, body) = call(
        &app,
        "POST",
        "/api/v1/jobs",
        &[("Authorization", &auth)],
        Some(json!({"queue_name": "hidden", "payload": {}, "idempotency_key": "idem-grpc"})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let hidden_id = body["id"].as_str().unwrap().to_string();
    let before = updated_at(db.pool(), &hidden_id).await;

    let scoped = format!("sp_test_{}", uuid::Uuid::new_v4().simple());
    insert_key(db.pool(), &t.org_id, &scoped, &["visible"]).await;

    let err = queue_service(&db)
        .enqueue(grpc(&scoped, enqueue_req("visible", "idem-grpc")))
        .await
        .expect_err("out-of-scope replay");
    assert_eq!(err.code(), Code::FailedPrecondition);
    assert_eq!(
        err.message(),
        spooled_backend::models::IDEMPOTENCY_KEY_IN_USE
    );
    assert!(!err.message().contains(&hidden_id));
    assert_eq!(updated_at(db.pool(), &hidden_id).await, before);

    let again = queue_service(&db)
        .enqueue(grpc(&t.key, enqueue_req("hidden", "idem-grpc")))
        .await
        .expect("in-scope replay")
        .into_inner();
    assert!(!again.created);
    assert_eq!(again.job_id, hidden_id);
}

/// RT-02: a whitespace-only gRPC idempotency key is no key, same as REST.
#[tokio::test]
async fn rt02_grpc_whitespace_idempotency_key_is_not_stored() {
    let db = TestDatabase::new().await;
    let t = seed_tenant(db.pool()).await;
    let svc = queue_service(&db);

    for key in [" ", "\n", " \t "] {
        let res = svc
            .enqueue(grpc(&t.key, enqueue_req("ws", key)))
            .await
            .expect("enqueue")
            .into_inner();
        assert!(res.created, "{key:?} must not replay a previous blank key");
    }

    let (jobs, stored): (i64, i64) = sqlx::query_as(
        "SELECT COUNT(*), COUNT(idempotency_key) FROM jobs WHERE organization_id = $1 AND queue_name = 'ws'",
    )
    .bind(&t.org_id)
    .fetch_one(db.pool())
    .await
    .unwrap();
    assert_eq!(jobs, 3);
    assert_eq!(stored, 0, "whitespace-only keys must be SQL NULL");
}

/// ORG-02: the first use of a new queue name is serialized against max_queues
/// for POST /jobs and gRPC Enqueue. An existing queue stays usable.
#[tokio::test]
async fn org02_new_queue_names_respect_max_queues() {
    let db = TestDatabase::new().await;
    let app = app(&db);
    let t = seed_tenant(db.pool()).await;
    sqlx::query(
        "UPDATE organizations SET custom_limits = '{\"max_queues\": 1}'::JSONB WHERE id = $1",
    )
    .bind(&t.org_id)
    .execute(db.pool())
    .await
    .unwrap();
    let auth = bearer(&t.key);

    let posts: Vec<_> = (0..8)
        .map(|i| {
            let app = app.clone();
            let auth = auth.clone();
            let body = json!({"queue_name": format!("nq{i}"), "payload": {}});
            async move {
                let headers = [("Authorization", auth.as_str())];
                call(&app, "POST", "/api/v1/jobs", &headers, Some(body)).await
            }
        })
        .collect();
    let results = futures::future::join_all(posts).await;
    let mut created = 0;
    let mut limited = 0;
    for (status, body) in &results {
        match *status {
            StatusCode::CREATED => created += 1,
            StatusCode::TOO_MANY_REQUESTS => limited += 1,
            _ => panic!("unexpected {status}: {body}"),
        }
    }
    assert_eq!(created, 1, "{results:?}");
    assert_eq!(limited, 7, "{results:?}");

    let queue: String =
        sqlx::query_scalar("SELECT queue_name FROM jobs WHERE organization_id = $1 LIMIT 1")
            .bind(&t.org_id)
            .fetch_one(db.pool())
            .await
            .unwrap();
    let (status, body) = call(
        &app,
        "POST",
        "/api/v1/jobs",
        &[("Authorization", &auth)],
        Some(json!({"queue_name": &queue, "payload": {"again": true}})),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::CREATED,
        "existing queue stays usable: {body}"
    );

    let (status, body) = call(
        &app,
        "POST",
        "/api/v1/jobs/bulk",
        &[("Authorization", &auth)],
        Some(json!({"queue_name": "bulk-new", "jobs": [{"payload": {}}]})),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::TOO_MANY_REQUESTS,
        "bulk new queue: {body}"
    );
    let (status, body) = call(
        &app,
        "POST",
        "/api/v1/jobs/bulk",
        &[("Authorization", &auth)],
        Some(json!({"queue_name": &queue, "jobs": [{"payload": {"bulk": true}}]})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "bulk existing queue: {body}");
    assert_eq!(body["success_count"], 1);

    let svc = queue_service(&db);
    let grpc_posts: Vec<_> = (0..8)
        .map(|i| svc.enqueue(grpc(&t.key, enqueue_req(&format!("gq{i}"), ""))))
        .collect();
    let grpc_results = futures::future::join_all(grpc_posts).await;
    let mut grpc_ok = 0;
    for res in grpc_results {
        match res {
            Ok(ok) => panic!("new queue slipped past max_queues: {:?}", ok.into_inner()),
            Err(status) => {
                assert_eq!(status.code(), Code::ResourceExhausted, "{status:?}");
                grpc_ok += 1;
            }
        }
    }
    assert_eq!(grpc_ok, 8);

    let again = svc
        .enqueue(grpc(&t.key, enqueue_req(&queue, "")))
        .await
        .expect("existing queue")
        .into_inner();
    assert!(again.created);
}
