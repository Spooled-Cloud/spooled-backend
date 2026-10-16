#![cfg(feature = "docker-tests")]
//! Workflow job limits, ProcessJobs dequeue names, REST worker register, OpenAPI.

mod common;

use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::Router;
use prost::Message;
use serde_json::{json, Value};
use tokio_stream::StreamExt;
use tonic::Request as GrpcRequest;
use tower::ServiceExt;

use spooled_backend::api::{router, AppState};
use spooled_backend::config::{QueueSettings, Settings, WorkerSettings};
use spooled_backend::db::Database;
use spooled_backend::grpc::proto::process_request::Request as Op;
use spooled_backend::grpc::proto::process_response::Response as Reply;
use spooled_backend::grpc::proto::queue_service_server::QueueService;
use spooled_backend::grpc::proto::{DequeueRequest, ProcessRequest, ProcessResponse};
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
    let slug = format!("r28l-{}", &org_id[..8]);
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
         VALUES ($1, $2, $3, $4, $5, 'r28l', ARRAY[]::TEXT[], TRUE, NOW())",
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

fn workflow(name: &str, job: Value) -> Value {
    json!({"name": name, "jobs": [job]})
}

fn step(extra: Value) -> Value {
    let mut job = json!({
        "key": "step",
        "queue_name": "default",
        "payload": {"n": 1},
    });
    if let (Value::Object(dst), Value::Object(src)) = (&mut job, extra) {
        for (k, v) in src {
            dst.insert(k, v);
        }
    }
    job
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

fn error_code(response: &ProcessResponse) -> String {
    match &response.response {
        Some(Reply::Error(err)) => err.code.clone(),
        other => format!("not an error: {other:?}"),
    }
}

/// LIVE3-01: priority outside -100..100 is 400 VALIDATION_ERROR, not a 500
/// from the jobs check. The ends of the range are stored.
#[tokio::test]
async fn workflow_job_priority_out_of_range_is_validation_error() {
    let db = TestDatabase::new().await;
    let t = seed_tenant(db.pool()).await;
    let app = app(&db);

    for priority in [250, -101] {
        let (status, body) = call(
            &app,
            "POST",
            "/api/v1/workflows",
            &t.key,
            Some(workflow("bad-prio", step(json!({"priority": priority})))),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{priority}: {body}");
        assert_eq!(body["code"], "VALIDATION_ERROR", "{body}");
        assert!(
            body["message"]
                .as_str()
                .unwrap_or("")
                .contains("Priority must be between -100 and 100"),
            "{body}"
        );
    }
    let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM workflows WHERE organization_id = $1")
        .bind(&t.org_id)
        .fetch_one(db.pool())
        .await
        .unwrap();
    assert_eq!(n, 0);

    for priority in [100, -100] {
        let (status, body) = call(
            &app,
            "POST",
            "/api/v1/workflows",
            &t.key,
            Some(workflow(
                &format!("ok-{priority}"),
                step(json!({"priority": priority, "key": format!("k{priority}")})),
            )),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{priority}: {body}");
    }
    let stored: Vec<i32> = sqlx::query_scalar(
        "SELECT priority FROM jobs WHERE organization_id = $1 ORDER BY priority",
    )
    .bind(&t.org_id)
    .fetch_all(db.pool())
    .await
    .unwrap();
    assert_eq!(stored, vec![-100, 100]);
}

/// LIVE3-04: max_retries outside 0..100 and timeout_seconds outside 1..86400
/// are 400, not stored clamped. A job key outside the per-job key rules still
/// creates the workflow.
#[tokio::test]
async fn workflow_job_retries_and_timeout_out_of_range_are_validation_errors() {
    let db = TestDatabase::new().await;
    let t = seed_tenant(db.pool()).await;
    let app = app(&db);

    let cases = [
        (
            json!({"max_retries": 500}),
            "Max retries must be between 0 and 100",
        ),
        (
            json!({"timeout_seconds": 0}),
            "Timeout must be between 1 and 86400 seconds",
        ),
        (
            json!({"timeout_seconds": 86401}),
            "Timeout must be between 1 and 86400 seconds",
        ),
    ];
    for (extra, needle) in cases {
        let (status, body) = call(
            &app,
            "POST",
            "/api/v1/workflows",
            &t.key,
            Some(workflow("bad-limit", step(extra))),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert_eq!(body["code"], "VALIDATION_ERROR", "{body}");
        assert!(
            body["message"].as_str().unwrap_or("").contains(needle),
            "{body}"
        );
    }
    let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM jobs WHERE organization_id = $1")
        .bind(&t.org_id)
        .fetch_one(db.pool())
        .await
        .unwrap();
    assert_eq!(n, 0, "an out-of-range value must not be stored clamped");

    let (status, body) = call(
        &app,
        "POST",
        "/api/v1/workflows",
        &t.key,
        Some(workflow(
            "keys-off",
            step(json!({
                "key": "spaced key",
                "max_retries": 0,
                "timeout_seconds": 1
            })),
        )),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (retries, timeout): (i32, i32) =
        sqlx::query_as("SELECT max_retries, timeout_seconds FROM jobs WHERE organization_id = $1")
            .bind(&t.org_id)
            .fetch_one(db.pool())
            .await
            .unwrap();
    assert_eq!((retries, timeout), (0, 1));
}

/// LIVE3-02: ProcessJobs dequeue validates the queue name the way unary Dequeue
/// does. An illegal name is INVALID_ARGUMENT, not NO_JOBS.
#[tokio::test]
async fn process_jobs_dequeue_rejects_an_illegal_queue_name() {
    let db = TestDatabase::new().await;
    let t = seed_tenant(db.pool()).await;
    let service = queue_service(&db);
    let names = [
        "q".repeat(101),
        "bad queue".to_string(),
        String::new(),
        "-aaaaaaaaaa".to_string(),
        ".dots....".to_string(),
    ];
    let ops: Vec<Op> = names
        .iter()
        .map(|queue| {
            Op::Dequeue(DequeueRequest {
                queue_name: queue.clone(),
                worker_id: "w1".to_string(),
                lease_duration_secs: 30,
                batch_size: 1,
            })
        })
        .collect();

    let mut bytes = Vec::new();
    for op in ops {
        let body = ProcessRequest { request: Some(op) }.encode_to_vec();
        bytes.push(0u8);
        bytes.extend_from_slice(&(body.len() as u32).to_be_bytes());
        bytes.extend_from_slice(&body);
    }
    let mut codec = tonic_prost::ProstCodec::<ProcessResponse, ProcessRequest>::default();
    let decoder = tonic::codec::Codec::decoder(&mut codec);
    let streaming = tonic::Streaming::new_request(
        decoder,
        axum::body::Body::from(bytes),
        None,
        Some(4 * 1024 * 1024),
    );
    let mut request = GrpcRequest::new(streaming);
    request
        .metadata_mut()
        .insert("x-api-key", t.key.parse().unwrap());

    let stream = service
        .process_jobs(request)
        .await
        .expect("stream opens")
        .into_inner();
    let replies = tokio::time::timeout(Duration::from_secs(20), stream.collect::<Vec<_>>())
        .await
        .expect("stream ends");
    assert_eq!(replies.len(), names.len());
    for (name, reply) in names.iter().zip(replies) {
        let response = reply.expect("response");
        assert_eq!(
            error_code(&response),
            "INVALID_ARGUMENT",
            "queue {name:?} must not be reported as an empty queue"
        );
    }
}

/// LIVE3-03: REST register rejects a queue name claim will not lease (a leading
/// `.` or `-`, or more than 100 characters). A 100-character name still registers.
#[tokio::test]
async fn rest_worker_register_rejects_a_queue_name_claim_will_not() {
    let db = TestDatabase::new().await;
    let t = seed_tenant(db.pool()).await;
    let app = app(&db);

    for queue in ["-lead", ".", &"a".repeat(101)] {
        let (status, body) = call(
            &app,
            "POST",
            "/api/v1/workers/register",
            &t.key,
            Some(json!({"queue_name": queue, "hostname": "host-1"})),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{queue}: {body}");
        assert_eq!(body["code"], "VALIDATION_ERROR", "{body}");
    }
    let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM workers WHERE organization_id = $1")
        .bind(&t.org_id)
        .fetch_one(db.pool())
        .await
        .unwrap();
    assert_eq!(n, 0);

    let ok_name = "b".repeat(100);
    let (status, body) = call(
        &app,
        "POST",
        "/api/v1/workers/register",
        &t.key,
        Some(json!({"queue_name": ok_name, "hostname": "host-1"})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let stored: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM workers WHERE organization_id = $1 AND queue_name = $2",
    )
    .bind(&t.org_id)
    .bind(&ok_name)
    .fetch_one(db.pool())
    .await
    .unwrap();
    assert_eq!(stored, 1);
}

fn section<'a>(yaml: &'a str, start: &str, end: &str) -> &'a str {
    let at = yaml
        .find(start)
        .unwrap_or_else(|| panic!("missing {start}"));
    let rest = &yaml[at + start.len()..];
    let stop = rest.find(end).unwrap_or_else(|| panic!("missing {end}"));
    &rest[..stop]
}

/// LIVE3-05: workflow jobs are capped at 100 in the OpenAPI document, and the
/// job fields that now answer 400 are described with those ranges.
#[test]
fn openapi_workflow_jobs_are_capped_at_100() {
    let yaml = include_str!("../docs/openapi.yaml");
    let jobs = section(yaml, "CreateWorkflowRequest:", "WorkflowJobDefinition:");
    assert!(
        jobs.contains("maxItems: 100\n"),
        "CreateWorkflowRequest jobs must be maxItems 100"
    );
    assert!(
        !jobs.contains("maxItems: 1000"),
        "CreateWorkflowRequest jobs must not still advertise maxItems 1000"
    );

    let def = section(yaml, "WorkflowJobDefinition:", "CreateWorkflowResponse:");
    for needle in [
        "minimum: -100",
        "maximum: 100",
        "minimum: 0",
        "maximum: 86400",
        "expires_at:",
    ] {
        assert!(
            def.contains(needle),
            "WorkflowJobDefinition missing {needle}"
        );
    }

    let stats = section(yaml, "JobSummaryStats:", "QueueSummary:");
    assert!(
        stats.contains("scheduled:"),
        "JobSummaryStats must expose scheduled"
    );
    assert!(stats.contains(
        "required: [total, pending, scheduled, processing, completed_24h, failed_24h, deadletter]"
    ));
}
