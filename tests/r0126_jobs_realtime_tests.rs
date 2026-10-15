#![cfg(feature = "docker-tests")]
//! Regressions for the 0.1.126 batch (jobs, realtime, queue names, webhooks).
//!
//! `TEST_DATABASE_URL=postgres://$USER@localhost:5432/postgres`
//! `TEST_REDIS_URL=redis://127.0.0.1:6379`

mod common;

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::Router;
use futures::StreamExt;
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
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

async fn seed_tenant(pool: &sqlx::PgPool) -> Tenant {
    let org_id = uuid::Uuid::new_v4().to_string();
    let slug = format!("gb2-{}", &org_id[..8]);
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
         VALUES ($1, $2, $3, $4, $5, 'gb2', ARRAY[]::TEXT[], TRUE, NOW())",
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

async fn app_redis(db: &TestDatabase, redis_url: &str) -> (Router, RedisCache) {
    let cache = RedisCache::connect(&RedisSettings {
        url: redis_url.to_string(),
        pool_size: 4,
    })
    .await
    .expect("redis");
    let app = router(AppState::new(
        Database::from_pool(Arc::clone(&db.pool)),
        Some(cache.clone()),
        Metrics::new(),
        Settings::load_for_testing(),
    ));
    (app, cache)
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

async fn authed(
    app: &Router,
    method: &str,
    uri: &str,
    key: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let auth = bearer(key);
    call(app, method, uri, &[("Authorization", &auth)], body).await
}

fn queue_service(
    db: &TestDatabase,
    cache: Option<RedisCache>,
) -> spooled_backend::grpc::services::QueueServiceImpl {
    use spooled_backend::config::{QueueSettings, WorkerSettings};
    use spooled_backend::outgoing_webhooks::service::OutgoingWebhookService;
    spooled_backend::grpc::services::QueueServiceImpl::new(
        Arc::clone(&db.pool),
        Arc::new(Metrics::new()),
        cache,
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

fn worker_service(db: &TestDatabase) -> spooled_backend::grpc::services::WorkerServiceImpl {
    use spooled_backend::config::WorkerSettings;
    use spooled_backend::outgoing_webhooks::service::OutgoingWebhookService;
    spooled_backend::grpc::services::WorkerServiceImpl::new(
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

fn grpc_req<T>(key: &str, body: T) -> tonic::Request<T> {
    let mut request = tonic::Request::new(body);
    request
        .metadata_mut()
        .insert("x-api-key", key.parse().unwrap());
    request
}

fn scheduler(db: &TestDatabase) -> spooled_backend::scheduler::Scheduler {
    use spooled_backend::outgoing_webhooks::service::OutgoingWebhookService;
    spooled_backend::scheduler::Scheduler::new(
        Arc::clone(&db.pool),
        None,
        Arc::new(Metrics::new()),
        Arc::new(OutgoingWebhookService::new((*db.pool).clone(), 1, 4)),
    )
}

struct HeldLease {
    worker: String,
    job: String,
    lease_id: String,
    retry_count: i32,
}

/// Register one worker and lease both of two fresh jobs to it. The second
/// lease is then expired in SQL.
async fn leased_pair(
    app: &Router,
    pool: &sqlx::PgPool,
    key: &str,
    queue: &str,
) -> (HeldLease, HeldLease) {
    let worker = uuid::Uuid::new_v4().to_string();
    let (status, body) = authed(
        app,
        "POST",
        "/api/v1/workers/register",
        key,
        Some(json!({"worker_id": worker, "queue_name": queue, "hostname": "host"})),
    )
    .await;
    assert!(status.is_success(), "register {status} {body}");

    for _ in 0..2 {
        let (status, body) = authed(
            app,
            "POST",
            "/api/v1/jobs",
            key,
            Some(json!({"queue_name": queue, "payload": {}})),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
    }

    let (status, body) = authed(
        app,
        "POST",
        "/api/v1/jobs/claim",
        key,
        Some(
            json!({"queue_name": queue, "worker_id": worker, "limit": 2, "lease_duration_secs": 3600}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let claimed = body["jobs"].as_array().expect("jobs");
    assert_eq!(claimed.len(), 2, "{body}");

    let mut held = Vec::new();
    for job in claimed {
        held.push(HeldLease {
            worker: worker.clone(),
            job: job["id"].as_str().unwrap().to_string(),
            lease_id: job["lease_id"].as_str().unwrap().to_string(),
            retry_count: job["retry_count"].as_i64().unwrap() as i32,
        });
    }
    sqlx::query("UPDATE jobs SET lease_expires_at = NOW() - INTERVAL '1 second' WHERE id = $1")
        .bind(&held[1].job)
        .execute(pool)
        .await
        .unwrap();
    let expired = held.pop().unwrap();
    let live = held.pop().unwrap();
    (live, expired)
}

async fn assert_lease_kept(pool: &sqlx::PgPool, held: &HeldLease) {
    let (status, lease_id, retry_count): (String, Option<String>, i32) =
        sqlx::query_as("SELECT status, lease_id, retry_count FROM jobs WHERE id = $1")
            .bind(&held.job)
            .fetch_one(pool)
            .await
            .unwrap();
    assert_eq!(status, "processing", "live lease was released");
    assert_eq!(lease_id.as_deref(), Some(held.lease_id.as_str()));
    assert_eq!(retry_count, held.retry_count);
}

async fn assert_lease_requeued(pool: &sqlx::PgPool, held: &HeldLease) {
    let (status, retry_count): (String, i32) =
        sqlx::query_as("SELECT status, retry_count FROM jobs WHERE id = $1")
            .bind(&held.job)
            .fetch_one(pool)
            .await
            .unwrap();
    assert_eq!(status, "pending", "expired lease was not requeued");
    assert_eq!(retry_count, held.retry_count + 1);
    let history: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM job_history WHERE job_id = $1 AND event_type = 'retry_scheduled'",
    )
    .bind(&held.job)
    .fetch_one(pool)
    .await
    .unwrap();
    assert_eq!(history, 1, "requeue wrote no job_history row");
}

/// JOBS-01: deregister requeues an expired lease and leaves a live one running.
#[tokio::test]
async fn jobs01_deregister_keeps_live_lease_and_requeues_expired() {
    let db = TestDatabase::new().await;
    let app = app(&db);
    let t = seed_tenant(db.pool()).await;

    let (rest_live, rest_expired) = leased_pair(&app, db.pool(), &t.key, "jobs01-rest").await;
    let (status, body) = authed(
        &app,
        "POST",
        &format!("/api/v1/workers/{}/deregister", rest_live.worker),
        &t.key,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    assert_lease_kept(db.pool(), &rest_live).await;
    assert_lease_requeued(db.pool(), &rest_expired).await;

    let (grpc_live, grpc_expired) = leased_pair(&app, db.pool(), &t.key, "jobs01-grpc").await;
    use spooled_backend::grpc::proto::worker_service_server::WorkerService;
    let response = worker_service(&db)
        .deregister(grpc_req(
            &t.key,
            spooled_backend::grpc::proto::DeregisterRequest {
                worker_id: grpc_live.worker.clone(),
            },
        ))
        .await
        .expect("grpc deregister")
        .into_inner();
    assert!(response.success);
    assert_lease_kept(db.pool(), &grpc_live).await;
    assert_lease_requeued(db.pool(), &grpc_expired).await;
}

async fn create_job(app: &Router, key: &str, queue: &str, parent: Option<&str>) -> String {
    let mut body = json!({"queue_name": queue, "payload": {}});
    if let Some(parent) = parent {
        body["parent_job_id"] = json!(parent);
    }
    let (status, body) = authed(app, "POST", "/api/v1/jobs", key, Some(body)).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    body["id"].as_str().unwrap().to_string()
}

async fn deps_met(pool: &sqlx::PgPool, id: &str) -> Option<bool> {
    sqlx::query_scalar("SELECT dependencies_met FROM jobs WHERE id = $1")
        .bind(id)
        .fetch_one(pool)
        .await
        .unwrap()
}

/// JOBS-02: completing a parent must not mark a child runnable while another
/// `job_dependencies` edge is still open. A parent-only child still unblocks.
#[tokio::test]
async fn jobs02_parent_completion_does_not_clear_unmet_dependency() {
    let db = TestDatabase::new().await;
    let app = app(&db);
    let t = seed_tenant(db.pool()).await;
    let queue = "jobs02";

    let parent = create_job(&app, &t.key, queue, None).await;
    let other = create_job(&app, &t.key, queue, None).await;
    let blocked = create_job(&app, &t.key, queue, Some(&parent)).await;
    let parent_only = create_job(&app, &t.key, queue, Some(&parent)).await;

    let (status, body) = authed(
        &app,
        "POST",
        &format!("/api/v1/jobs/{blocked}/dependencies"),
        &t.key,
        Some(json!({"depends_on": [other], "dependency_mode": "all"})),
    )
    .await;
    assert!(status.is_success(), "add dep {status} {body}");

    let worker = uuid::Uuid::new_v4().to_string();
    let (status, body) = authed(
        &app,
        "POST",
        "/api/v1/workers/register",
        &t.key,
        Some(json!({"worker_id": worker, "queue_name": queue, "hostname": "host"})),
    )
    .await;
    assert!(status.is_success(), "{body}");

    let (status, body) = authed(
        &app,
        "POST",
        "/api/v1/jobs/claim",
        &t.key,
        Some(json!({"queue_name": queue, "worker_id": worker, "limit": 10, "lease_duration_secs": 60})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let mut leases = std::collections::HashMap::new();
    for job in body["jobs"].as_array().expect("jobs") {
        leases.insert(
            job["id"].as_str().unwrap().to_string(),
            job["lease_id"].as_str().unwrap().to_string(),
        );
    }
    assert!(leases.contains_key(&parent), "{body}");
    assert!(leases.contains_key(&other), "{body}");
    assert!(
        !leases.contains_key(&blocked),
        "child with an open parent was claimable: {body}"
    );

    let (status, body) = authed(
        &app,
        "POST",
        &format!("/api/v1/jobs/{parent}/complete"),
        &t.key,
        Some(json!({"worker_id": worker, "lease_id": leases[&parent]})),
    )
    .await;
    assert!(status.is_success(), "complete parent {status} {body}");

    assert_eq!(
        deps_met(db.pool(), &blocked).await,
        Some(false),
        "unmet extra dependency was cleared"
    );
    assert_eq!(
        deps_met(db.pool(), &parent_only).await,
        Some(true),
        "parent-only child stayed blocked"
    );

    scheduler(&db).update_job_dependencies().await.unwrap();
    assert_eq!(
        deps_met(db.pool(), &blocked).await,
        Some(false),
        "dependency sweep forced the flag true"
    );

    let (status, body) = authed(
        &app,
        "POST",
        &format!("/api/v1/jobs/{other}/complete"),
        &t.key,
        Some(json!({"worker_id": worker, "lease_id": leases[&other]})),
    )
    .await;
    assert!(status.is_success(), "complete other {status} {body}");
    assert_eq!(deps_met(db.pool(), &blocked).await, Some(true));
}

async fn workflow_row(pool: &sqlx::PgPool, id: &str) -> (String, i32) {
    sqlx::query_as("SELECT status, failed_jobs FROM workflows WHERE id = $1")
        .bind(id)
        .fetch_one(pool)
        .await
        .unwrap()
}

async fn seed_workflow(pool: &sqlx::PgPool, org_id: &str) -> String {
    let id = uuid::Uuid::new_v4().to_string();
    sqlx::query(
        "INSERT INTO workflows (id, organization_id, name, status, total_jobs) \
         VALUES ($1, $2, 'wf', 'running', 1)",
    )
    .bind(&id)
    .bind(org_id)
    .execute(pool)
    .await
    .unwrap();
    id
}

/// JOBS-03: deadletter and cancelled steps update the stored workflow row.
#[tokio::test]
async fn jobs03_deadletter_and_cancelled_update_workflow_progress() {
    let db = TestDatabase::new().await;
    let t = seed_tenant(db.pool()).await;

    let dead = seed_workflow(db.pool(), &t.org_id).await;
    let job = insert_pending_workflow_job(db.pool(), &t.org_id, &dead).await;
    sqlx::query("UPDATE jobs SET status = 'deadletter' WHERE id = $1")
        .bind(&job)
        .execute(db.pool())
        .await
        .unwrap();
    let (status, failed) = workflow_row(db.pool(), &dead).await;
    assert_eq!(failed, 1, "deadletter did not count");
    assert_eq!(status, "failed");

    let mixed = seed_workflow(db.pool(), &t.org_id).await;
    sqlx::query("UPDATE workflows SET total_jobs = 2 WHERE id = $1")
        .bind(&mixed)
        .execute(db.pool())
        .await
        .unwrap();
    let staying = insert_pending_workflow_job(db.pool(), &t.org_id, &mixed).await;
    let dying = insert_pending_workflow_job(db.pool(), &t.org_id, &mixed).await;
    sqlx::query("UPDATE jobs SET status = 'deadletter' WHERE id = $1")
        .bind(&dying)
        .execute(db.pool())
        .await
        .unwrap();
    let (status, failed) = workflow_row(db.pool(), &mixed).await;
    assert_eq!(failed, 1);
    assert_eq!(
        status, "running",
        "a still-pending step must keep the workflow running"
    );
    let staying_status: String = sqlx::query_scalar("SELECT status FROM jobs WHERE id = $1")
        .bind(&staying)
        .fetch_one(db.pool())
        .await
        .unwrap();
    assert_eq!(staying_status, "pending");

    let cancelled = seed_workflow(db.pool(), &t.org_id).await;
    let job = insert_pending_workflow_job(db.pool(), &t.org_id, &cancelled).await;
    sqlx::query("UPDATE jobs SET status = 'cancelled' WHERE id = $1")
        .bind(&job)
        .execute(db.pool())
        .await
        .unwrap();
    let (status, failed) = workflow_row(db.pool(), &cancelled).await;
    assert_eq!(failed, 0);
    assert_eq!(status, "cancelled");
}

async fn insert_pending_workflow_job(
    pool: &sqlx::PgPool,
    org_id: &str,
    workflow_id: &str,
) -> String {
    let id = uuid::Uuid::new_v4().to_string();
    sqlx::query(
        "INSERT INTO jobs (id, organization_id, queue_name, status, payload, workflow_id) \
         VALUES ($1, $2, 'wfq', 'pending', '{}'::JSONB, $3)",
    )
    .bind(&id)
    .bind(org_id)
    .bind(workflow_id)
    .execute(pool)
    .await
    .unwrap();
    id
}

async fn serve(app: Router) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await
        .unwrap();
    });
    addr
}

struct RawWs {
    tcp: TcpStream,
    buf: Vec<u8>,
}

async fn ws_open(addr: SocketAddr, target: &str, headers: &[(&str, &str)]) -> Result<RawWs, u16> {
    let tcp = TcpStream::connect(addr).await.unwrap();
    let mut request = format!(
        "GET {target} HTTP/1.1\r\nHost: {addr}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\
         Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n"
    );
    for (name, value) in headers {
        request.push_str(&format!("{name}: {value}\r\n"));
    }
    request.push_str("\r\n");
    let mut ws = RawWs {
        tcp,
        buf: Vec::new(),
    };
    ws.tcp.write_all(request.as_bytes()).await.unwrap();
    let head_end = loop {
        if let Some(pos) = ws.buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break pos + 4;
        }
        assert!(
            tokio::time::timeout(Duration::from_secs(10), ws.read_more())
                .await
                .expect("handshake"),
            "handshake closed"
        );
    };
    let status: u16 = String::from_utf8_lossy(&ws.buf[..head_end])
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap();
    if status != 101 {
        return Err(status);
    }
    ws.buf.drain(..head_end);
    Ok(ws)
}

impl RawWs {
    async fn read_more(&mut self) -> bool {
        let mut chunk = [0u8; 8192];
        match self.tcp.read(&mut chunk).await {
            Ok(n) if n > 0 => {
                self.buf.extend_from_slice(&chunk[..n]);
                true
            }
            _ => false,
        }
    }

    async fn fill(&mut self, n: usize) -> bool {
        while self.buf.len() < n {
            if !self.read_more().await {
                return false;
            }
        }
        true
    }

    async fn frame(&mut self) -> Option<(u8, Vec<u8>)> {
        if !self.fill(2).await {
            return None;
        }
        let opcode = self.buf[0] & 0x0f;
        let (mut len, mut header) = ((self.buf[1] & 0x7f) as usize, 2);
        if len == 126 {
            if !self.fill(4).await {
                return None;
            }
            len = u16::from_be_bytes([self.buf[2], self.buf[3]]) as usize;
            header = 4;
        }
        if !self.fill(header + len).await {
            return None;
        }
        let payload: Vec<u8> = self.buf.drain(..header + len).skip(header).collect();
        Some((opcode, payload))
    }

    /// Next text frame, skipping the server ping and other control frames.
    async fn text(&mut self, limit: Duration) -> Option<String> {
        let deadline = tokio::time::Instant::now() + limit;
        while let Ok(Some((opcode, payload))) =
            tokio::time::timeout_at(deadline, self.frame()).await
        {
            if opcode == 1 {
                return Some(String::from_utf8(payload).unwrap());
            }
            if opcode == 8 {
                return None;
            }
        }
        None
    }

    async fn send_text(&mut self, text: &str) {
        let payload = text.as_bytes();
        let mut frame = vec![0x81, 0x80 | payload.len() as u8];
        let mask = [0x11u8, 0x22, 0x33, 0x44];
        frame.extend_from_slice(&mask);
        for (i, byte) in payload.iter().enumerate() {
            frame.push(byte ^ mask[i % 4]);
        }
        self.tcp.write_all(&frame).await.unwrap();
    }

    async fn texts(&mut self, limit: Duration) -> String {
        let deadline = tokio::time::Instant::now() + limit;
        let mut out = String::new();
        while let Ok(Some(text)) = tokio::time::timeout_at(deadline, self.text(limit)).await {
            out.push_str(&text);
            out.push('\n');
        }
        out
    }
}

async fn open_ws(addr: SocketAddr, key: &str) -> RawWs {
    let auth = bearer(key);
    let mut ws = ws_open(addr, "/api/v1/ws", &[("Authorization", &auth)])
        .await
        .expect("upgrade");
    let hello = ws.text(Duration::from_secs(3)).await.expect("hello");
    assert!(hello.contains("\"Ping\""), "{hello}");
    ws
}

/// Send `command`, then a ping, and wait until the pong. The server handles
/// the socket one frame at a time, so the pong means `command` was applied.
async fn command_then_pong(ws: &mut RawWs, command: &str) -> String {
    ws.send_text(command).await;
    ws.send_text(r#"{"cmd":"ping"}"#).await;
    let mut seen = String::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    while tokio::time::Instant::now() < deadline {
        let Some(text) = ws.text(Duration::from_secs(3)).await else {
            break;
        };
        seen.push_str(&text);
        if text.contains("\"Ping\"") {
            return seen;
        }
    }
    panic!("no pong after {command}: {seen}");
}

async fn publish(cache: &RedisCache, org_id: &str, job_id: &str, queue: &str) {
    let event = json!({
        "type": "JobCreated",
        "data": {
            "job_id": job_id,
            "queue_name": queue,
            "priority": 0,
            "timestamp": chrono::Utc::now(),
        },
    });
    cache
        .publish(&format!("org:{org_id}:events"), &event.to_string())
        .await
        .expect("publish");
}

/// RT-01: lowercase `cmd` and `{type, queues}` subscribe, and the PascalCase
/// shape still does. A command the server does not understand is an error
/// frame, not a silent no-op, and the socket stays open.
#[tokio::test]
async fn rt01_websocket_accepts_sdk_and_quickstart_subscribe() {
    let db = TestDatabase::new().await;
    let redis = TestRedis::new().await;
    let (app, cache) = app_redis(&db, &redis.url).await;
    let t = seed_tenant(db.pool()).await;
    let addr = serve(app).await;

    for command in [
        r#"{"cmd":"subscribe","queue":"emails"}"#,
        r#"{"type":"subscribe","queues":["emails"]}"#,
        r#"{"cmd":"Subscribe","queue":"emails","job_id":null}"#,
    ] {
        let mut ws = open_ws(addr, &t.key).await;
        let during = command_then_pong(&mut ws, command).await;
        assert!(
            !during.contains("Unrecognized"),
            "{command} was rejected: {during}"
        );
        publish(&cache, &t.org_id, "job-emails", "emails").await;
        publish(&cache, &t.org_id, "job-billing", "billing").await;
        let seen = ws.texts(Duration::from_secs(2)).await;
        assert!(
            seen.contains("job-emails"),
            "{command} did not deliver the subscribed queue: {seen}"
        );
        assert!(
            !seen.contains("job-billing"),
            "{command} delivered an unsubscribed queue: {seen}"
        );
    }

    let mut ws = open_ws(addr, &t.key).await;
    let seen = command_then_pong(&mut ws, "{not-json").await;
    assert!(
        seen.contains("Unrecognized realtime command"),
        "bad text produced no error: {seen}"
    );
    assert!(
        seen.contains("\"Ping\""),
        "socket closed after a bad command"
    );
}

/// RT-02: filters from separate commands are OR. Unsubscribing one dimension,
/// a PHP null for the other, or an empty unsubscribe does not drop the rest.
#[tokio::test]
async fn rt02_subscriptions_are_or_and_unsubscribe_is_partial() {
    let db = TestDatabase::new().await;
    let redis = TestRedis::new().await;
    let (app, cache) = app_redis(&db, &redis.url).await;
    let t = seed_tenant(db.pool()).await;
    let addr = serve(app).await;

    let mut ws = open_ws(addr, &t.key).await;
    command_then_pong(&mut ws, r#"{"cmd":"subscribe","queue":"emails"}"#).await;
    command_then_pong(&mut ws, r#"{"cmd":"subscribe","job_id":"job-1"}"#).await;
    publish(&cache, &t.org_id, "job-2", "emails").await;
    let seen = ws.texts(Duration::from_secs(2)).await;
    assert!(
        seen.contains("job-2"),
        "a queue subscribe was ANDed with a later job subscribe: {seen}"
    );

    command_then_pong(&mut ws, r#"{"cmd":"unsubscribe","job_id":"job-1"}"#).await;
    publish(&cache, &t.org_id, "job-3", "emails").await;
    publish(&cache, &t.org_id, "job-bill", "billing").await;
    let seen = ws.texts(Duration::from_secs(2)).await;
    assert!(
        seen.contains("job-3"),
        "unsubscribe job cleared the queue: {seen}"
    );
    assert!(
        !seen.contains("job-bill"),
        "unsubscribe job cleared every filter: {seen}"
    );

    let mut ws = open_ws(addr, &t.key).await;
    command_then_pong(&mut ws, r#"{"cmd":"subscribe","queue":"emails"}"#).await;
    command_then_pong(
        &mut ws,
        r#"{"cmd":"unsubscribe","queue":null,"job_id":"job-1"}"#,
    )
    .await;
    publish(&cache, &t.org_id, "job-php", "emails").await;
    publish(&cache, &t.org_id, "job-php-bill", "billing").await;
    let seen = ws.texts(Duration::from_secs(2)).await;
    assert!(
        seen.contains("job-php"),
        "PHP null queue cleared the queue filter: {seen}"
    );
    assert!(!seen.contains("job-php-bill"), "{seen}");

    let mut ws = open_ws(addr, &t.key).await;
    command_then_pong(&mut ws, r#"{"cmd":"subscribe","queue":"emails"}"#).await;
    command_then_pong(&mut ws, r#"{"cmd":"unsubscribe"}"#).await;
    publish(&cache, &t.org_id, "job-empty", "billing").await;
    publish(&cache, &t.org_id, "job-keep", "emails").await;
    let seen = ws.texts(Duration::from_secs(2)).await;
    assert!(
        !seen.contains("job-empty"),
        "empty unsubscribe cleared every filter: {seen}"
    );
    assert!(seen.contains("job-keep"), "{seen}");
}

type SseBody = axum::body::BodyDataStream;

async fn sse_open(app: &Router, uri: &str, key: &str) -> (StatusCode, SseBody) {
    let auth = bearer(key);
    let res = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(uri)
                .header("Authorization", &auth)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    (res.status(), res.into_body().into_data_stream())
}

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

struct RedisProxy {
    url: String,
    links: Arc<std::sync::Mutex<Vec<tokio::task::JoinHandle<()>>>>,
    refuse: Arc<AtomicBool>,
}

async fn redis_proxy(upstream: &str) -> RedisProxy {
    let parsed = url::Url::parse(upstream).expect("redis url");
    let target = format!(
        "{}:{}",
        parsed.host_str().unwrap(),
        parsed.port().unwrap_or(6379)
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mut proxied = parsed.clone();
    proxied.set_host(Some("127.0.0.1")).unwrap();
    proxied
        .set_port(Some(listener.local_addr().unwrap().port()))
        .unwrap();
    let links = Arc::new(std::sync::Mutex::new(Vec::new()));
    let accepted = Arc::clone(&links);
    let refuse = Arc::new(AtomicBool::new(false));
    let refuse_flag = Arc::clone(&refuse);
    tokio::spawn(async move {
        while let Ok((mut inbound, _)) = listener.accept().await {
            if refuse_flag.load(Ordering::Relaxed) {
                drop(inbound);
                continue;
            }
            let target = target.clone();
            let link = tokio::spawn(async move {
                if let Ok(mut outbound) = TcpStream::connect(&target).await {
                    let _ = tokio::io::copy_bidirectional(&mut inbound, &mut outbound).await;
                }
            });
            accepted.lock().unwrap().push(link);
        }
    });
    RedisProxy {
        url: proxied.to_string(),
        links,
        refuse,
    }
}

impl RedisProxy {
    fn cut(&self) {
        for link in self.links.lock().unwrap().drain(..) {
            link.abort();
        }
    }
}

/// RT-03: a pub/sub channel that closes, or that never opens, ends the SSE
/// stream with an error instead of health-only until the 30 minute cap.
#[tokio::test]
async fn rt03_sse_ends_when_the_event_channel_closes() {
    let db = TestDatabase::new().await;
    let redis = TestRedis::new().await;
    let proxy = redis_proxy(&redis.url).await;
    let (app, _cache) = app_redis(&db, &proxy.url).await;
    let t = seed_tenant(db.pool()).await;

    // Refuse new sockets while the cache's existing connection still works, so
    // auth and the connection slot succeed and only the pub/sub subscribe fails.
    proxy.refuse.store(true, Ordering::Relaxed);
    let (status, mut body) = sse_open(&app, "/api/v1/events", &t.key).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "subscribe failure must still be a 200 stream"
    );
    let (seen, ended) = sse_read(&mut body, Duration::from_secs(5)).await;
    assert!(
        seen.contains("Realtime event channel closed"),
        "failed subscribe produced no error: {seen}"
    );
    assert!(
        !seen.contains(": connected"),
        "failed subscribe still said connected: {seen}"
    );
    assert!(ended, "failed subscribe left the stream open: {seen}");

    proxy.refuse.store(false, Ordering::Relaxed);
    let (status, mut body) = sse_open(&app, "/api/v1/events", &t.key).await;
    assert_eq!(status, StatusCode::OK);
    let (seen, ended) = sse_read(&mut body, Duration::from_secs(3)).await;
    assert!(seen.contains(": connected"), "{seen}");
    assert!(!ended, "stream ended before the channel was cut");
    proxy.cut();
    let (seen, ended) = sse_read(&mut body, Duration::from_secs(5)).await;
    assert!(
        ended,
        "channel close left the stream open (health only): {seen}"
    );
    assert!(seen.contains("Realtime event channel closed"), "{seen}");
}

/// RT-04: RenewLease stores `NOW() + extension` from the statement that
/// commits, not a Rust timestamp taken while the pool acquire is waiting.
#[tokio::test]
async fn rt04_renew_lease_expiry_survives_pool_acquire_wait() {
    use spooled_backend::grpc::proto::queue_service_server::QueueService;
    use spooled_backend::grpc::proto::{DequeueRequest, EnqueueRequest, RenewLeaseRequest};

    let db = TestDatabase::new().await;
    let redis = TestRedis::new().await;
    let cache = RedisCache::connect(&RedisSettings {
        url: redis.url.clone(),
        pool_size: 4,
    })
    .await
    .unwrap();
    let service = queue_service(&db, Some(cache));
    let t = seed_tenant(db.pool()).await;
    let worker = "rt04-worker";

    let job_id = service
        .enqueue(grpc_req(
            &t.key,
            EnqueueRequest {
                queue_name: "rt04".to_string(),
                payload: None,
                priority: 0,
                max_retries: 0,
                timeout_seconds: 0,
                scheduled_at: None,
                idempotency_key: String::new(),
                tags: Default::default(),
            },
        ))
        .await
        .expect("enqueue")
        .into_inner()
        .job_id;
    let leased = service
        .dequeue(grpc_req(
            &t.key,
            DequeueRequest {
                queue_name: "rt04".to_string(),
                worker_id: worker.to_string(),
                lease_duration_secs: 60,
                batch_size: 1,
            },
        ))
        .await
        .expect("dequeue")
        .into_inner()
        .jobs;
    assert_eq!(leased.len(), 1);
    let lease_id = leased[0].lease_id.clone();

    // The enqueue/dequeue warmed the key cache and the last_used guard, so the
    // renew below does not touch Postgres until the UPDATE. That UPDATE is what
    // has to wait.
    tokio::time::sleep(Duration::from_millis(200)).await;
    let mut held = Vec::new();
    for _ in 0..5 {
        held.push(
            tokio::time::timeout(Duration::from_secs(8), db.pool().acquire())
                .await
                .expect("pool acquire")
                .expect("connection"),
        );
    }

    // `renew_lease` borrows `&self`, so the clone has to outlive the spawned
    // future. Building the request first keeps the API key out of the task.
    let request = grpc_req(
        &t.key,
        RenewLeaseRequest {
            job_id: job_id.clone(),
            worker_id: worker.to_string(),
            extension_secs: 30,
            lease_id,
        },
    );
    let service = service.clone();
    let renew = tokio::spawn(async move { service.renew_lease(request).await });
    tokio::time::sleep(Duration::from_secs(6)).await;
    drop(held);

    let response = renew.await.unwrap().expect("renew").into_inner();
    assert!(response.success);
    let remaining: f64 = sqlx::query_scalar(
        "SELECT EXTRACT(EPOCH FROM (lease_expires_at - clock_timestamp()))::float8 FROM jobs WHERE id = $1",
    )
    .bind(&job_id)
    .fetch_one(db.pool())
    .await
    .unwrap();
    assert!(
        remaining >= 28.0,
        "30s extension was measured before the acquire wait: {remaining:.2}s left"
    );
    assert!(remaining < 40.0, "extension was not 30s: {remaining:.2}");
}

/// RT-05: a failed manual retry must not replace `last_status = auto_disabled`.
#[tokio::test]
async fn rt05_retry_keeps_auto_disabled_status() {
    use spooled_backend::outgoing_webhooks::service::OutgoingWebhookService;

    let db = TestDatabase::new().await;
    let t = seed_tenant(db.pool()).await;
    let webhook_id = uuid::Uuid::new_v4().to_string();
    let delivery_id = uuid::Uuid::new_v4();
    sqlx::query(
        "INSERT INTO outgoing_webhooks (id, organization_id, name, url, secret, events, enabled, failure_count, last_status) \
         VALUES ($1, $2, 'hook', 'http://127.0.0.1:9/hook', 'sek', ARRAY['job.completed']::TEXT[], FALSE, 0, 'auto_disabled')",
    )
    .bind(&webhook_id)
    .bind(&t.org_id)
    .execute(db.pool())
    .await
    .unwrap();
    let payload = json!({
        "id": delivery_id.to_string(),
        "event": "job.completed",
        "data": {"job_id": "j"},
    });
    sqlx::query(
        "INSERT INTO outgoing_webhook_deliveries (id, webhook_id, event, payload, status, attempts) \
         VALUES ($1, $2, 'job.completed', $3, 'failed', 1)",
    )
    .bind(delivery_id.to_string())
    .bind(&webhook_id)
    .bind(payload)
    .execute(db.pool())
    .await
    .unwrap();

    let service = OutgoingWebhookService::new((*db.pool).clone(), 1, 4);
    let delivered =
        tokio::time::timeout(Duration::from_secs(15), service.retry_delivery(delivery_id))
            .await
            .expect("retry timed out")
            .expect("retry");
    assert!(!delivered);

    let (last_status, enabled): (Option<String>, bool) =
        sqlx::query_as("SELECT last_status, enabled FROM outgoing_webhooks WHERE id = $1")
            .bind(&webhook_id)
            .fetch_one(db.pool())
            .await
            .unwrap();
    assert_eq!(last_status.as_deref(), Some("auto_disabled"));
    assert!(!enabled);
}

/// A webhook re-enabled by PUT keeps the auto_disabled marker until its next
/// delivery; that delivery must replace it. Keeping it on every delivery left
/// a re-enabled webhook showing "Re-enabled" forever.
#[tokio::test]
async fn rt05_reenabled_webhook_delivery_replaces_auto_disabled_status() {
    use spooled_backend::outgoing_webhooks::service::OutgoingWebhookService;

    let db = TestDatabase::new().await;
    let t = seed_tenant(db.pool()).await;
    let webhook_id = uuid::Uuid::new_v4().to_string();
    let delivery_id = uuid::Uuid::new_v4();
    sqlx::query(
        "INSERT INTO outgoing_webhooks (id, organization_id, name, url, secret, events, enabled, failure_count, last_status) \
         VALUES ($1, $2, 'hook', 'http://127.0.0.1:9/hook', 'sek', ARRAY['job.completed']::TEXT[], TRUE, 0, 'auto_disabled')",
    )
    .bind(&webhook_id)
    .bind(&t.org_id)
    .execute(db.pool())
    .await
    .unwrap();
    let payload = json!({
        "id": delivery_id.to_string(),
        "event": "job.completed",
        "data": {"job_id": "j"},
    });
    sqlx::query(
        "INSERT INTO outgoing_webhook_deliveries (id, webhook_id, event, payload, status, attempts) \
         VALUES ($1, $2, 'job.completed', $3, 'failed', 1)",
    )
    .bind(delivery_id.to_string())
    .bind(&webhook_id)
    .bind(payload)
    .execute(db.pool())
    .await
    .unwrap();

    let service = OutgoingWebhookService::new((*db.pool).clone(), 1, 4);
    let delivered =
        tokio::time::timeout(Duration::from_secs(15), service.retry_delivery(delivery_id))
            .await
            .expect("retry timed out")
            .expect("retry");
    assert!(!delivered);

    let last_status: Option<String> =
        sqlx::query_scalar("SELECT last_status FROM outgoing_webhooks WHERE id = $1")
            .bind(&webhook_id)
            .fetch_one(db.pool())
            .await
            .unwrap();
    assert_eq!(last_status.as_deref(), Some("failed"));
}

fn workflow_body(queue: &str) -> Value {
    json!({
        "name": "wf",
        "jobs": [{"key": "step", "queue_name": queue, "payload": {}}]
    })
}

fn schedule_body(queue: &str) -> Value {
    json!({
        "name": "sched",
        "cron_expression": "0 0 * * *",
        "queue_name": queue,
        "payload_template": {}
    })
}

/// LIVE-01: workflow, schedule and inbound webhook queue names use the same
/// 1-100 character set as POST /jobs. 65-100 stays valid.
#[tokio::test]
async fn live01_queue_names_match_claim_rules() {
    let db = TestDatabase::new().await;
    let app = app(&db);
    let t = seed_tenant(db.pool()).await;
    let long = "q".repeat(101);
    let ok = "q".repeat(65);
    let hundred = "a".repeat(100);

    for (uri, body) in [
        ("/api/v1/workflows", workflow_body(&long)),
        ("/api/v1/workflows", workflow_body("bad queue")),
        ("/api/v1/schedules", schedule_body(&long)),
        ("/api/v1/schedules", schedule_body("bad queue")),
    ] {
        let (status, resp) = authed(&app, "POST", uri, &t.key, Some(body)).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{uri} {resp}");
    }

    let (status, resp) = authed(
        &app,
        "POST",
        "/api/v1/workflows",
        &t.key,
        Some(workflow_body(&ok)),
    )
    .await;
    assert!(
        status.is_success(),
        "65-char workflow queue rejected: {status} {resp}"
    );
    let (status, resp) = authed(
        &app,
        "POST",
        "/api/v1/schedules",
        &t.key,
        Some(schedule_body(&hundred)),
    )
    .await;
    assert!(
        status.is_success(),
        "100-char schedule queue rejected: {status} {resp}"
    );

    let token = "whk_live01";
    sqlx::query(
        "UPDATE organizations SET settings = settings || jsonb_build_object('webhook_token', $2) WHERE id = $1",
    )
    .bind(&t.org_id)
    .bind(token)
    .execute(db.pool())
    .await
    .unwrap();
    let hook = format!("/api/v1/webhooks/{}/custom", t.org_id);
    let (status, resp) = call(
        &app,
        "POST",
        &hook,
        &[("X-Webhook-Token", token)],
        Some(json!({"queue_name": long, "payload": {}})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{resp}");
    let (status, resp) = call(
        &app,
        "POST",
        &hook,
        &[("X-Webhook-Token", token)],
        Some(json!({"queue_name": "bad queue", "payload": {}})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{resp}");
    let (status, resp) = call(
        &app,
        "POST",
        &hook,
        &[("X-Webhook-Token", token)],
        Some(json!({"queue_name": ok, "payload": {}})),
    )
    .await;
    assert!(
        status.is_success(),
        "65-char webhook queue rejected: {status} {resp}"
    );
}

/// LIVE-02: a schedule row stored before the tighter rule must not enqueue.
#[tokio::test]
async fn live02_trigger_and_cron_skip_unclaimable_queue() {
    let db = TestDatabase::new().await;
    let app = app(&db);
    let t = seed_tenant(db.pool()).await;
    let trigger_id = uuid::Uuid::new_v4().to_string();
    sqlx::query(
        "INSERT INTO schedules (id, organization_id, name, cron_expression, timezone, queue_name, payload_template, is_active, next_run_at) \
         VALUES ($1, $2, 'legacy', '0 0 * * *', 'UTC', 'bad queue', '{}'::JSONB, TRUE, NOW() + INTERVAL '1 day')",
    )
    .bind(&trigger_id)
    .bind(&t.org_id)
    .execute(db.pool())
    .await
    .unwrap();

    let (status, body) = authed(
        &app,
        "POST",
        &format!("/api/v1/schedules/{trigger_id}/trigger"),
        &t.key,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    let jobs: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM jobs WHERE organization_id = $1 AND queue_name = 'bad queue'",
    )
    .bind(&t.org_id)
    .fetch_one(db.pool())
    .await
    .unwrap();
    assert_eq!(jobs, 0, "trigger enqueued an unclaimable queue");

    let cron_id = uuid::Uuid::new_v4().to_string();
    sqlx::query(
        "INSERT INTO schedules (id, organization_id, name, cron_expression, timezone, queue_name, payload_template, is_active, next_run_at) \
         VALUES ($1, $2, 'cron', '* * * * *', 'UTC', 'also bad', '{}'::JSONB, TRUE, NOW() - INTERVAL '1 minute')",
    )
    .bind(&cron_id)
    .bind(&t.org_id)
    .execute(db.pool())
    .await
    .unwrap();

    scheduler(&db).process_cron_schedules().await.unwrap();

    let jobs: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM jobs WHERE organization_id = $1 AND queue_name = 'also bad'",
    )
    .bind(&t.org_id)
    .fetch_one(db.pool())
    .await
    .unwrap();
    assert_eq!(jobs, 0, "cron enqueued an unclaimable queue");
    let (run_status, error): (String, Option<String>) =
        sqlx::query_as("SELECT status, error_message FROM schedule_runs WHERE schedule_id = $1")
            .bind(&cron_id)
            .fetch_one(db.pool())
            .await
            .expect("failed schedule run");
    assert_eq!(run_status, "failed");
    assert!(
        error.as_deref().unwrap_or("").contains("not claimable"),
        "{error:?}"
    );
    let still_due: bool = sqlx::query_scalar(
        "SELECT next_run_at IS NOT NULL AND next_run_at <= NOW() FROM schedules WHERE id = $1",
    )
    .bind(&cron_id)
    .fetch_one(db.pool())
    .await
    .unwrap();
    assert!(!still_due, "bad schedule stayed due");
}

/// LIVE-03: a blank or whitespace idempotency key is stored as NULL, so two
/// deliveries of the same blank key create two jobs.
#[tokio::test]
async fn live03_blank_webhook_idempotency_key_is_null() {
    let db = TestDatabase::new().await;
    let app = app(&db);
    let t = seed_tenant(db.pool()).await;
    let token = "whk_live03";
    sqlx::query(
        "UPDATE organizations SET settings = settings || jsonb_build_object('webhook_token', $2) WHERE id = $1",
    )
    .bind(&t.org_id)
    .bind(token)
    .execute(db.pool())
    .await
    .unwrap();

    let hook = format!("/api/v1/webhooks/{}/custom", t.org_id);
    let mut ids = Vec::new();
    for _ in 0..2 {
        let (status, body) = call(
            &app,
            "POST",
            &hook,
            &[("X-Webhook-Token", token)],
            Some(json!({"queue_name": "inbox", "payload": {}, "idempotency_key": "   "})),
        )
        .await;
        assert!(status.is_success(), "{status} {body}");
        ids.push(body["job_id"].as_str().unwrap().to_string());
    }
    assert_ne!(
        ids[0], ids[1],
        "whitespace idempotency key deduped the second job"
    );
    let stored: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM jobs WHERE organization_id = $1 AND idempotency_key IS NOT NULL",
    )
    .bind(&t.org_id)
    .fetch_one(db.pool())
    .await
    .unwrap();
    assert_eq!(stored, 0, "blank idempotency key was stored");
}

async fn retry_history_error(pool: &sqlx::PgPool, job_id: &str) -> Option<String> {
    sqlx::query_scalar(
        "SELECT details->>'error' FROM job_history WHERE job_id = $1 AND event_type = 'retry_scheduled'",
    )
    .bind(job_id)
    .fetch_optional(pool)
    .await
    .unwrap()
}

/// JOBS-04: a lease timeout and an offline-worker requeue both leave a
/// `retry_scheduled` history row. A live lease is not part of either sweep.
#[tokio::test]
async fn jobs04_lease_and_offline_requeues_write_history() {
    let db = TestDatabase::new().await;
    let app = app(&db);
    let t = seed_tenant(db.pool()).await;
    let sched = scheduler(&db);

    let (live, expired) = leased_pair(&app, db.pool(), &t.key, "jobs04-lease").await;
    sched.recover_expired_leases().await.unwrap();
    assert_lease_kept(db.pool(), &live).await;
    assert_lease_requeued(db.pool(), &expired).await;
    assert_eq!(
        retry_history_error(db.pool(), &expired.job)
            .await
            .as_deref(),
        Some("Lease expired (worker timeout)")
    );
    assert!(retry_history_error(db.pool(), &live.job).await.is_none());

    let (live, expired) = leased_pair(&app, db.pool(), &t.key, "jobs04-offline").await;
    sqlx::query("UPDATE workers SET status = 'offline' WHERE id = $1")
        .bind(&live.worker)
        .execute(db.pool())
        .await
        .unwrap();
    sched.cleanup_stale_workers().await.unwrap();
    assert_lease_kept(db.pool(), &live).await;
    assert_lease_requeued(db.pool(), &expired).await;
    assert_eq!(
        retry_history_error(db.pool(), &expired.job)
            .await
            .as_deref(),
        Some("Worker went offline")
    );
}

/// JOBS-05: a retry with backoff is `scheduled`, so the scheduled list and
/// stats include it and the pending ones do not. Claim waits for scheduled_at.
#[tokio::test]
async fn jobs05_retry_is_scheduled_until_it_is_due() {
    let db = TestDatabase::new().await;
    let app = app(&db);
    let t = seed_tenant(db.pool()).await;
    let queue = "jobs05";
    let worker = uuid::Uuid::new_v4().to_string();
    let (status, body) = authed(
        &app,
        "POST",
        "/api/v1/workers/register",
        &t.key,
        Some(json!({"worker_id": worker, "queue_name": queue, "hostname": "host"})),
    )
    .await;
    assert!(status.is_success(), "register {status} {body}");
    let (status, body) = authed(
        &app,
        "POST",
        "/api/v1/jobs",
        &t.key,
        Some(json!({"queue_name": queue, "payload": {}, "max_retries": 3})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let job = body["id"].as_str().unwrap().to_string();

    let (status, body) = authed(
        &app,
        "POST",
        "/api/v1/jobs/claim",
        &t.key,
        Some(json!({"queue_name": queue, "worker_id": worker, "limit": 1, "lease_duration_secs": 60})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let lease = body["jobs"][0]["lease_id"].as_str().unwrap().to_string();
    let (status, body) = authed(
        &app,
        "POST",
        &format!("/api/v1/jobs/{job}/fail"),
        &t.key,
        Some(json!({"worker_id": worker, "lease_id": lease, "error": "boom"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let (job_status, scheduled_at): (String, Option<chrono::DateTime<chrono::Utc>>) =
        sqlx::query_as("SELECT status, scheduled_at FROM jobs WHERE id = $1")
            .bind(&job)
            .fetch_one(db.pool())
            .await
            .unwrap();
    assert_eq!(job_status, "scheduled");
    assert!(
        scheduled_at.is_some_and(|at| at > chrono::Utc::now()),
        "retry was stored without a future scheduled_at: {scheduled_at:?}"
    );

    // Pin the backoff so the list and claim checks do not race the 1s delay.
    sqlx::query("UPDATE jobs SET scheduled_at = NOW() + INTERVAL '1 hour' WHERE id = $1")
        .bind(&job)
        .execute(db.pool())
        .await
        .unwrap();

    let (status, listed) = authed(
        &app,
        "GET",
        "/api/v1/jobs?status=scheduled&queue_name=jobs05",
        &t.key,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{listed}");
    assert!(
        listed
            .as_array()
            .unwrap()
            .iter()
            .any(|row| row["id"] == job),
        "{listed}"
    );
    let (status, listed) = authed(
        &app,
        "GET",
        "/api/v1/jobs?status=pending&queue_name=jobs05",
        &t.key,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{listed}");
    assert!(
        listed
            .as_array()
            .unwrap()
            .iter()
            .all(|row| row["id"] != job),
        "{listed}"
    );
    let (status, stats) = authed(
        &app,
        "GET",
        "/api/v1/jobs/stats?queue_name=jobs05",
        &t.key,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{stats}");
    assert_eq!(stats["scheduled"], 1, "{stats}");
    assert_eq!(stats["pending"], 0, "{stats}");

    let (status, body) = authed(
        &app,
        "POST",
        "/api/v1/jobs/claim",
        &t.key,
        Some(json!({"queue_name": queue, "worker_id": worker, "limit": 1})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(
        body["jobs"].as_array().unwrap().is_empty(),
        "future retry was claimed: {body}"
    );

    sqlx::query("UPDATE jobs SET scheduled_at = NOW() - INTERVAL '1 second' WHERE id = $1")
        .bind(&job)
        .execute(db.pool())
        .await
        .unwrap();
    let (status, body) = authed(
        &app,
        "POST",
        "/api/v1/jobs/claim",
        &t.key,
        Some(json!({"queue_name": queue, "worker_id": worker, "limit": 1})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["jobs"][0]["id"], job, "{body}");
}
