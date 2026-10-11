#![cfg(feature = "docker-tests")]
//! Regressions from the 2026-10-10 code-level bug hunt, exercised through the
//! real router against a real database.
//!
//! Run without Docker by pointing the suite at local services:
//! `TEST_DATABASE_URL=postgres://$USER@localhost:5432/postgres`
//! `TEST_REDIS_URL=redis://127.0.0.1:6379`

mod common;

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::Router;
use serde_json::{json, Value};
use tower::ServiceExt;

use spooled_backend::api::{router, AppState};
use spooled_backend::config::{RegistrationMode, Settings};
use spooled_backend::db::Database;
use spooled_backend::observability::Metrics;

use common::TestDatabase;

struct Tenant {
    org_id: String,
    key: String,
}

/// Seed an org whose billing email is verified, with one unrestricted key.
async fn seed_tenant(pool: &sqlx::PgPool) -> Tenant {
    let org_id = uuid::Uuid::new_v4().to_string();
    let slug = format!("bh-{}", &org_id[..8]);
    sqlx::query(
        "INSERT INTO organizations (id, name, slug, plan_tier, billing_email, billing_email_verified_at, settings, created_at, updated_at) \
         VALUES ($1, $2, $2, 'starter', $3, NOW(), '{}'::JSONB, NOW(), NOW())",
    )
    .bind(&org_id)
    .bind(&slug)
    .bind(format!("{}@example.com", slug))
    .execute(pool)
    .await
    .expect("seed org");

    let key = format!("sp_test_{}", uuid::Uuid::new_v4().simple());
    let hash = bcrypt::hash(&key, 4).expect("hash key");
    sqlx::query(
        "INSERT INTO api_keys (id, organization_id, key_hash, key_prefix, lookup_hash, name, queues, is_active, created_at) \
         VALUES ($1, $2, $3, $4, $5, 'bh', ARRAY[]::TEXT[], TRUE, NOW())",
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

fn app_with(db: &TestDatabase, settings: Settings) -> Router {
    router(AppState::new(
        Database::from_pool(Arc::clone(&db.pool)),
        None,
        Metrics::new(),
        settings,
    ))
}

fn app(db: &TestDatabase) -> Router {
    app_with(db, Settings::load_for_testing())
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

/// Newest stored code row for an address: (code, organization_id).
async fn latest_code(pool: &sqlx::PgPool, email: &str) -> (String, Option<String>) {
    sqlx::query_as(
        "SELECT code, organization_id FROM email_login_codes WHERE email = $1 \
         ORDER BY created_at DESC LIMIT 1",
    )
    .bind(email)
    .fetch_one(pool)
    .await
    .expect("code row")
}

// ---------------------------------------------------------------------------
// BEAUTH-03: an unverified billing email is not an email-login identity.
// ---------------------------------------------------------------------------
#[tokio::test]
async fn changed_billing_email_does_not_resolve_email_login_until_verified() {
    let db = TestDatabase::new().await;
    let squatter = seed_tenant(db.pool()).await;
    let app = app(&db);
    let auth = bearer(&squatter.key);
    let victim = "victim-bh@example.com";

    // Mixed case is normalised; the change clears verification.
    let (status, body) = call(
        &app,
        "PUT",
        &format!("/api/v1/organizations/{}", squatter.org_id),
        &[("Authorization", &auth)],
        Some(json!({"billing_email": "Victim-BH@Example.com"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["billing_email"], victim);
    assert!(body["billing_email_verified_at"].is_null(), "{body}");

    // Email login does not find the squatter's org by the unverified address.
    let (status, body) = call(
        &app,
        "GET",
        &format!("/api/v1/auth/check-email?email={victim}"),
        &[],
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["exists"], false, "{body}");

    let (status, body) = call(
        &app,
        "POST",
        "/api/v1/auth/email/start",
        &[],
        Some(json!({"email": victim})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (code, org) = latest_code(db.pool(), victim).await;
    assert_eq!(
        org, None,
        "login code must not be bound to the squatter's org"
    );

    // The victim verifying the code gets a signup token (a new org), with a
    // hint that an unconfirmed org also uses the address.
    let (status, body) = call(
        &app,
        "POST",
        "/api/v1/auth/email/verify",
        &[],
        Some(json!({"email": victim, "code": code})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["type"], "signup", "{body}");
    assert_eq!(body["unverified_org_exists"], true, "{body}");

    // Another org can still set (but not verify) the same address: an
    // unverified copy blocks nobody.
    let other = seed_tenant(db.pool()).await;
    let (status, body) = call(
        &app,
        "PUT",
        &format!("/api/v1/organizations/{}", other.org_id),
        &[("Authorization", &bearer(&other.key))],
        Some(json!({"billing_email": victim})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
}

#[tokio::test]
async fn billing_email_verification_code_flow() {
    let db = TestDatabase::new().await;
    let tenant = seed_tenant(db.pool()).await;
    let app = app(&db);
    let auth = bearer(&tenant.key);
    let address = "owner-new-bh@example.com";

    let (status, _) = call(
        &app,
        "PUT",
        &format!("/api/v1/organizations/{}", tenant.org_id),
        &[("Authorization", &auth)],
        Some(json!({"billing_email": address})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let (status, body) = call(
        &app,
        "POST",
        "/api/v1/organizations/billing-email/verification",
        &[("Authorization", &auth)],
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["status"], "code_sent");

    let (stored, org) = latest_code(db.pool(), address).await;
    assert_eq!(org.as_deref(), Some(tenant.org_id.as_str()));
    let code = stored
        .strip_prefix("verify:")
        .expect("verify prefix")
        .to_string();

    // The confirmation code is not a login code.
    let (status, body) = call(
        &app,
        "POST",
        "/api/v1/auth/email/verify",
        &[],
        Some(json!({"email": address, "code": code})),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");

    let wrong = if code == "000000" { "111111" } else { "000000" };
    let (status, body) = call(
        &app,
        "POST",
        "/api/v1/organizations/billing-email/verify",
        &[("Authorization", &auth)],
        Some(json!({"code": wrong})),
    )
    .await;
    // 400, not 401: the dashboard reads a 401 as an expired session.
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");

    let (status, body) = call(
        &app,
        "POST",
        "/api/v1/organizations/billing-email/verify",
        &[("Authorization", &auth)],
        Some(json!({"code": code})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["status"], "verified");

    let (_, body) = call(
        &app,
        "GET",
        &format!("/api/v1/auth/check-email?email={address}"),
        &[],
        None,
    )
    .await;
    assert_eq!(body["exists"], true, "{body}");
}

// ---------------------------------------------------------------------------
// BEAUTH-02: an empty ADMIN_API_KEY never matches an empty X-Admin-Key header.
// ---------------------------------------------------------------------------
#[tokio::test]
async fn empty_admin_key_does_not_open_closed_registration() {
    let db = TestDatabase::new().await;
    let mut settings = Settings::load_for_testing();
    settings.registration.mode = RegistrationMode::Closed;
    settings.registration.admin_api_key = Some(String::new());
    let app = app_with(&db, settings);

    let (status, body) = call(
        &app,
        "POST",
        "/api/v1/organizations",
        &[("X-Admin-Key", "")],
        Some(json!({"name": "Empty Key Org", "slug": "empty-key-org"})),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");

    let (count,): (i64,) =
        sqlx::query_as("SELECT COUNT(*) FROM organizations WHERE slug = 'empty-key-org'")
            .fetch_one(db.pool())
            .await
            .unwrap();
    assert_eq!(count, 0);
}

// ---------------------------------------------------------------------------
// FESF-01: a DLQ purge that says `queue` only touches that queue.
// ---------------------------------------------------------------------------
#[tokio::test]
async fn dlq_purge_with_queue_alias_only_purges_that_queue() {
    let db = TestDatabase::new().await;
    let tenant = seed_tenant(db.pool()).await;
    let app = app(&db);

    for queue in ["payments", "emails"] {
        sqlx::query(
            "INSERT INTO jobs (id, organization_id, queue_name, payload, status, created_at, updated_at) \
             VALUES ($1, $2, $3, '{}'::JSONB, 'deadletter', NOW() - INTERVAL '1 hour', NOW())",
        )
        .bind(uuid::Uuid::new_v4().to_string())
        .bind(&tenant.org_id)
        .bind(queue)
        .execute(db.pool())
        .await
        .unwrap();
    }

    let (status, body) = call(
        &app,
        "POST",
        "/api/v1/jobs/dlq/purge",
        &[("Authorization", &bearer(&tenant.key))],
        Some(json!({"queue": "payments", "confirm": true})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let remaining: Vec<(String,)> = sqlx::query_as(
        "SELECT queue_name FROM jobs WHERE organization_id = $1 AND status = 'deadletter'",
    )
    .bind(&tenant.org_id)
    .fetch_all(db.pool())
    .await
    .unwrap();
    assert_eq!(remaining, vec![("emails".to_string(),)]);
}

// ---------------------------------------------------------------------------
// BEJOB-02: deleting a parent job keeps its children.
// ---------------------------------------------------------------------------
#[tokio::test]
async fn deleting_a_parent_job_keeps_its_children() {
    let db = TestDatabase::new().await;
    let tenant = seed_tenant(db.pool()).await;

    let parent = uuid::Uuid::new_v4().to_string();
    let child = uuid::Uuid::new_v4().to_string();
    sqlx::query(
        "INSERT INTO jobs (id, organization_id, queue_name, payload, status, completed_at, created_at, updated_at) \
         VALUES ($1, $2, 'q', '{}'::JSONB, 'completed', NOW() - INTERVAL '40 days', NOW(), NOW())",
    )
    .bind(&parent)
    .bind(&tenant.org_id)
    .execute(db.pool())
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO jobs (id, organization_id, queue_name, payload, status, parent_job_id, scheduled_at, created_at, updated_at) \
         VALUES ($1, $2, 'q', '{}'::JSONB, 'scheduled', $3, NOW() + INTERVAL '1 day', NOW(), NOW())",
    )
    .bind(&child)
    .bind(&tenant.org_id)
    .bind(&parent)
    .execute(db.pool())
    .await
    .unwrap();

    sqlx::query("DELETE FROM jobs WHERE id = $1")
        .bind(&parent)
        .execute(db.pool())
        .await
        .unwrap();

    let row: Option<(String, Option<String>)> =
        sqlx::query_as("SELECT status, parent_job_id FROM jobs WHERE id = $1")
            .bind(&child)
            .fetch_optional(db.pool())
            .await
            .unwrap();
    assert_eq!(row, Some(("scheduled".to_string(), None)));
}

// ---------------------------------------------------------------------------
// Review W1-07: a dependency deleted before it completed cancels its blocked
// dependents (any delete path), instead of releasing them an hour later.
// ---------------------------------------------------------------------------
#[tokio::test]
async fn deleting_an_unfinished_parent_cancels_its_blocked_children() {
    let db = TestDatabase::new().await;
    let tenant = seed_tenant(db.pool()).await;

    let parent = uuid::Uuid::new_v4().to_string();
    let child = uuid::Uuid::new_v4().to_string();
    let dependent = uuid::Uuid::new_v4().to_string();
    sqlx::query(
        "INSERT INTO jobs (id, organization_id, queue_name, payload, status, created_at, updated_at) \
         VALUES ($1, $2, 'q', '{}'::JSONB, 'pending', NOW(), NOW())",
    )
    .bind(&parent)
    .bind(&tenant.org_id)
    .execute(db.pool())
    .await
    .unwrap();
    for (id, parent_ref) in [(&child, Some(&parent)), (&dependent, None)] {
        sqlx::query(
            "INSERT INTO jobs (id, organization_id, queue_name, payload, status, parent_job_id, dependencies_met, created_at, updated_at) \
             VALUES ($1, $2, 'other', '{}'::JSONB, 'pending', $3, FALSE, NOW(), NOW())",
        )
        .bind(id)
        .bind(&tenant.org_id)
        .bind(parent_ref)
        .execute(db.pool())
        .await
        .unwrap();
    }
    sqlx::query("INSERT INTO job_dependencies (job_id, depends_on_job_id) VALUES ($1, $2)")
        .bind(&dependent)
        .bind(&parent)
        .execute(db.pool())
        .await
        .unwrap();

    sqlx::query("DELETE FROM jobs WHERE id = $1")
        .bind(&parent)
        .execute(db.pool())
        .await
        .unwrap();

    for id in [&child, &dependent] {
        let (status, last_error, completed): (String, Option<String>, bool) = sqlx::query_as(
            "SELECT status, last_error, completed_at IS NOT NULL FROM jobs WHERE id = $1",
        )
        .bind(id)
        .fetch_one(db.pool())
        .await
        .unwrap();
        assert_eq!(status, "cancelled");
        assert!(last_error
            .unwrap_or_default()
            .contains("was deleted before it completed"));
        assert!(completed, "cancelled jobs need completed_at for retention");
    }
}

// ---------------------------------------------------------------------------
// BEJOB-01: the stale-worker sweep leaves jobs whose lease is still live.
// ---------------------------------------------------------------------------
#[tokio::test]
async fn stale_worker_sweep_keeps_jobs_with_a_live_lease() {
    use spooled_backend::outgoing_webhooks::service::OutgoingWebhookService;
    use spooled_backend::scheduler::Scheduler;

    let db = TestDatabase::new().await;
    let tenant = seed_tenant(db.pool()).await;
    let worker = uuid::Uuid::new_v4().to_string();
    sqlx::query(
        "INSERT INTO workers (id, organization_id, queue_name, hostname, status, last_heartbeat) \
         VALUES ($1, $2, 'q', 'host', 'healthy', NOW() - INTERVAL '10 minutes')",
    )
    .bind(&worker)
    .bind(&tenant.org_id)
    .execute(db.pool())
    .await
    .unwrap();
    let job = uuid::Uuid::new_v4().to_string();
    sqlx::query(
        "INSERT INTO jobs (id, organization_id, queue_name, payload, status, assigned_worker_id, \
         lease_id, lease_expires_at, retry_count, max_retries, created_at, updated_at) \
         VALUES ($1, $2, 'q', '{}'::JSONB, 'processing', $3, 'lease-1', NOW() + INTERVAL '10 minutes', 0, 3, NOW(), NOW())",
    )
    .bind(&job)
    .bind(&tenant.org_id)
    .bind(&worker)
    .execute(db.pool())
    .await
    .unwrap();

    let scheduler = Scheduler::new(
        Arc::clone(&db.pool),
        None,
        Arc::new(Metrics::new()),
        Arc::new(OutgoingWebhookService::new((*db.pool).clone(), 3, 4)),
    );
    scheduler.cleanup_stale_workers().await.unwrap();

    let (status,): (String,) = sqlx::query_as("SELECT status FROM jobs WHERE id = $1")
        .bind(&job)
        .fetch_one(db.pool())
        .await
        .unwrap();
    assert_eq!(status, "processing", "a renewed lease must not be released");

    // Once the lease has expired the job is recovered as before.
    sqlx::query("UPDATE jobs SET lease_expires_at = NOW() - INTERVAL '1 second' WHERE id = $1")
        .bind(&job)
        .execute(db.pool())
        .await
        .unwrap();
    scheduler.cleanup_stale_workers().await.unwrap();
    let (status, last_error): (String, Option<String>) =
        sqlx::query_as("SELECT status, last_error FROM jobs WHERE id = $1")
            .bind(&job)
            .fetch_one(db.pool())
            .await
            .unwrap();
    assert_eq!(status, "pending");
    assert_eq!(last_error.as_deref(), Some("Worker went offline"));
}

// ---------------------------------------------------------------------------
// BEJOB-11: any failure text is recorded; job heartbeats refresh the worker.
// ---------------------------------------------------------------------------
#[tokio::test]
async fn fail_accepts_empty_and_long_error_text_and_heartbeat_refreshes_worker() {
    let db = TestDatabase::new().await;
    let tenant = seed_tenant(db.pool()).await;
    let app = app(&db);
    let auth = bearer(&tenant.key);

    let (status, body) = call(
        &app,
        "POST",
        "/api/v1/workers/register",
        &[("Authorization", &auth)],
        Some(json!({"queue_name": "fq", "hostname": "bh-host"})),
    )
    .await;
    assert!(status.is_success(), "{body}");
    let worker = body["id"]
        .as_str()
        .or_else(|| body["worker_id"].as_str())
        .unwrap()
        .to_string();

    for (case, error) in [("empty", String::new()), ("long", "x".repeat(10_000))] {
        let (status, body) = call(
            &app,
            "POST",
            "/api/v1/jobs",
            &[("Authorization", &auth)],
            Some(json!({"queue_name": "fq", "payload": {"case": case}, "max_retries": 0})),
        )
        .await;
        assert!(status.is_success(), "{body}");

        // Age the worker's heartbeat; the job heartbeat below must refresh it.
        sqlx::query(
            "UPDATE workers SET last_heartbeat = NOW() - INTERVAL '10 minutes' WHERE id = $1",
        )
        .bind(&worker)
        .execute(db.pool())
        .await
        .unwrap();

        let (status, body) = call(
            &app,
            "POST",
            "/api/v1/jobs/claim",
            &[("Authorization", &auth)],
            Some(json!({"queue_name": "fq", "worker_id": worker, "limit": 1})),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let claimed = &body["jobs"][0];
        let job_id = claimed["id"].as_str().unwrap().to_string();
        let lease_id = claimed["lease_id"].as_str().unwrap().to_string();

        let (status, body) = call(
            &app,
            "POST",
            &format!("/api/v1/jobs/{job_id}/heartbeat"),
            &[("Authorization", &auth)],
            Some(json!({"worker_id": worker, "lease_id": lease_id})),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let (fresh,): (bool,) = sqlx::query_as(
            "SELECT last_heartbeat > NOW() - INTERVAL '1 minute' FROM workers WHERE id = $1",
        )
        .bind(&worker)
        .fetch_one(db.pool())
        .await
        .unwrap();
        assert!(fresh, "a job heartbeat must refresh the worker heartbeat");

        let (status, body) = call(
            &app,
            "POST",
            &format!("/api/v1/jobs/{job_id}/fail"),
            &[("Authorization", &auth)],
            Some(json!({"worker_id": worker, "lease_id": lease_id, "error": error})),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{case}: {body}");

        let (job_status, last_error): (String, String) =
            sqlx::query_as("SELECT status, last_error FROM jobs WHERE id = $1")
                .bind(&job_id)
                .fetch_one(db.pool())
                .await
                .unwrap();
        assert_eq!(job_status, "deadletter", "{case}");
        match case {
            "empty" => assert_eq!(last_error, "Job failed"),
            _ => {
                assert!(last_error.len() <= spooled_backend::models::MAX_FAILURE_MESSAGE_BYTES);
                assert!(last_error.ends_with(spooled_backend::models::FAILURE_TRUNCATION_SUFFIX));
            }
        }
    }
}

// ---- wave 3 / dbwf ----

/// BEOPS-10: POST /workflows used to take a second pool connection for the queue
/// cap while its transaction (holding the per-org advisory lock) held the first, so
/// pool-size concurrent creates for one org starved each other until the acquire
/// timeout. With a tiny pool, more creates than connections must all complete fast.
#[tokio::test]
async fn concurrent_workflow_creates_do_not_deadlock_a_small_pool() {
    let db = TestDatabase::new().await;
    let tenant = seed_tenant(&db.pool).await;

    let small = sqlx::postgres::PgPoolOptions::new()
        .max_connections(3)
        .acquire_timeout(std::time::Duration::from_secs(8))
        .connect_with((*db.pool.connect_options()).clone())
        .await
        .expect("small pool");
    let app = router(AppState::new(
        Database::from_pool(Arc::new(small)),
        None,
        Metrics::new(),
        Settings::load_for_testing(),
    ));
    let auth = bearer(&tenant.key);

    let started = std::time::Instant::now();
    let mut tasks = Vec::new();
    for i in 0..5 {
        let app = app.clone();
        let auth = auth.clone();
        tasks.push(tokio::spawn(async move {
            call(
                &app,
                "POST",
                "/api/v1/workflows",
                &[("Authorization", &auth)],
                Some(json!({
                    "name": format!("wf-{i}"),
                    "jobs": [{"key": "a", "queue_name": "dbwf-new-queue", "payload": {}}],
                })),
            )
            .await
        }));
    }
    for t in tasks {
        let (status, body) = t.await.unwrap();
        assert_eq!(status, StatusCode::OK, "{body}");
    }
    assert!(
        started.elapsed() < std::time::Duration::from_secs(6),
        "creates stalled on pool acquire: {:?}",
        started.elapsed()
    );
}

/// BEOPS-08: an older image must still boot against a database that a newer release
/// already migrated (an applied migration this binary does not know).
#[tokio::test]
async fn migrations_tolerate_applied_migrations_unknown_to_this_binary() {
    let db = TestDatabase::new().await;
    sqlx::query(
        "INSERT INTO _sqlx_migrations (version, description, success, checksum, execution_time) \
         VALUES (29990101000000, 'from_a_newer_release', TRUE, decode('00', 'hex'), 1)",
    )
    .execute(db.pool.as_ref())
    .await
    .expect("insert future migration row");

    Database::from_pool(Arc::clone(&db.pool))
        .run_migrations()
        .await
        .expect("migrations must ignore applied rows missing from this binary");
}
// ---- wave 3 / opscfg ----

/// Router plus a handle on the metrics registry it feeds.
fn app_with_metrics(db: &TestDatabase) -> (Router, Metrics) {
    let metrics = Metrics::new();
    let router = router(AppState::new(
        Database::from_pool(Arc::clone(&db.pool)),
        None,
        metrics.clone(),
        Settings::load_for_testing(),
    ));
    (router, metrics)
}

fn metric_value(output: &str, series: &str) -> Option<f64> {
    output
        .lines()
        .find_map(|line| line.strip_prefix(series)?.trim().parse().ok())
}

// ---------------------------------------------------------------------------
// BEOPS-07: API request metrics use matched route templates, and the pool
// gauges are sampled from the real pool.
// ---------------------------------------------------------------------------
#[tokio::test]
async fn api_request_metrics_use_route_templates_and_pool_gauges_are_sampled() {
    let db = TestDatabase::new().await;
    let tenant = seed_tenant(db.pool()).await;
    let (app, metrics) = app_with_metrics(&db);
    let auth = bearer(&tenant.key);

    let missing_job = uuid::Uuid::new_v4().to_string();
    let (status, _) = call(
        &app,
        "GET",
        &format!("/api/v1/jobs/{missing_job}"),
        &[("Authorization", &auth)],
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let scanner_path = format!("/wp-admin/{}", uuid::Uuid::new_v4());
    let (status, _) = call(&app, "GET", &scanner_path, &[], None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let output = metrics.encode();
    assert_eq!(
        metric_value(
            &output,
            r#"spooled_api_requests_total{method="GET",route="/api/v1/jobs/{id}",status="4xx"}"#
        ),
        Some(1.0),
        "{output}"
    );
    assert_eq!(
        metric_value(
            &output,
            r#"spooled_api_requests_total{method="GET",route="unmatched",status="4xx"}"#
        ),
        Some(1.0),
        "{output}"
    );
    assert!(
        output.contains(
            r#"spooled_api_request_duration_seconds_count{method="GET",route="/api/v1/jobs/{id}"} 1"#
        ),
        "{output}"
    );
    assert!(
        !output.contains(&missing_job) && !output.contains("wp-admin"),
        "raw paths must never become label values"
    );

    let max = metric_value(&output, "spooled_database_connections_max").unwrap_or(0.0);
    assert!(max >= 1.0, "pool max must be sampled: {output}");
    assert!(metric_value(&output, "spooled_database_connections_active").is_some());
}

// ---------------------------------------------------------------------------
// BEOPS-07: the REST fail path counts failed/deadlettered jobs and the REST
// complete path observes the job duration.
// ---------------------------------------------------------------------------
#[tokio::test]
async fn rest_fail_and_complete_update_job_metrics() {
    let db = TestDatabase::new().await;
    let tenant = seed_tenant(db.pool()).await;
    let (app, metrics) = app_with_metrics(&db);
    let auth = bearer(&tenant.key);

    let (status, body) = call(
        &app,
        "POST",
        "/api/v1/workers/register",
        &[("Authorization", &auth)],
        Some(json!({"queue_name": "mq", "hostname": "metrics-host"})),
    )
    .await;
    assert!(status.is_success(), "{body}");
    let worker = body["id"]
        .as_str()
        .or_else(|| body["worker_id"].as_str())
        .unwrap()
        .to_string();

    for (path, retries) in [("fail", 0), ("complete", 3)] {
        let (status, body) = call(
            &app,
            "POST",
            "/api/v1/jobs",
            &[("Authorization", &auth)],
            Some(json!({"queue_name": "mq", "payload": {}, "max_retries": retries})),
        )
        .await;
        assert!(status.is_success(), "{body}");
        let (status, body) = call(
            &app,
            "POST",
            "/api/v1/jobs/claim",
            &[("Authorization", &auth)],
            Some(json!({"queue_name": "mq", "worker_id": worker, "limit": 1})),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let claimed = &body["jobs"][0];
        let job_id = claimed["id"].as_str().unwrap().to_string();
        let lease_id = claimed["lease_id"].as_str().unwrap().to_string();

        let request = if path == "fail" {
            json!({"worker_id": worker, "lease_id": lease_id, "error": "boom"})
        } else {
            json!({"worker_id": worker, "lease_id": lease_id, "result": {"ok": true}})
        };
        let (status, body) = call(
            &app,
            "POST",
            &format!("/api/v1/jobs/{job_id}/{path}"),
            &[("Authorization", &auth)],
            Some(request),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{path}: {body}");
    }

    let output = metrics.encode();
    assert_eq!(
        metric_value(&output, "spooled_jobs_failed_total"),
        Some(1.0),
        "{output}"
    );
    assert_eq!(
        metric_value(&output, "spooled_jobs_deadlettered_total"),
        Some(1.0),
        "{output}"
    );
    assert_eq!(
        metric_value(&output, "spooled_job_duration_seconds_count"),
        Some(1.0),
        "{output}"
    );
}

fn is_body_limit_rejection(status: StatusCode) -> bool {
    status == StatusCode::PAYLOAD_TOO_LARGE || status == StatusCode::BAD_REQUEST
}

// ---------------------------------------------------------------------------
// BEOPS-16: /jobs/bulk accepts 100 jobs of 25 KB (2.5 MB, over axum's 2 MiB
// default) but still has a ceiling; other routes keep the default limit.
// ---------------------------------------------------------------------------
#[tokio::test]
async fn bulk_enqueue_body_limit_fits_100_jobs_and_keeps_a_ceiling() {
    let db = TestDatabase::new().await;
    let tenant = seed_tenant(db.pool()).await;
    let app = app(&db);
    let auth = bearer(&tenant.key);

    let jobs: Vec<Value> = (0..100)
        .map(|_| json!({"payload": {"blob": "x".repeat(25_000)}}))
        .collect();
    let (status, body) = call(
        &app,
        "POST",
        "/api/v1/jobs/bulk",
        &[("Authorization", &auth)],
        Some(json!({"queue_name": "bulkq", "jobs": jobs})),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "2.5 MB bulk must not be 413: {body}"
    );
    assert_eq!(body["success_count"], 100, "{body}");

    // Past the 32 MiB ceiling the body is still refused.
    let huge: Vec<Value> = (0..100)
        .map(|_| json!({"payload": {"blob": "x".repeat(340_000)}}))
        .collect();
    let (status, _) = call(
        &app,
        "POST",
        "/api/v1/jobs/bulk",
        &[("Authorization", &auth)],
        Some(json!({"queue_name": "bulkq", "jobs": huge})),
    )
    .await;
    // ValidatedJson reports the extractor's length-limit error as INVALID_JSON (400).
    assert!(is_body_limit_rejection(status), "{status}");

    // A single-job request keeps the default 2 MiB limit.
    let (status, _) = call(
        &app,
        "POST",
        "/api/v1/jobs",
        &[("Authorization", &auth)],
        Some(json!({"queue_name": "bulkq", "payload": {"blob": "x".repeat(2_200_000)}})),
    )
    .await;
    assert!(is_body_limit_rejection(status), "{status}");
}
// ---- wave 3 / grpcq ----
// gRPC QueueService regressions (BEGRPC-03/04/09/10/12/16/17/18/21/22/23/24),
// driven through `QueueServiceImpl` directly.
mod grpcq {
    use std::sync::Arc;
    use std::time::Duration;

    use prost::Message;
    use prost_types::Timestamp;
    use serde_json::{json, Value};
    use tokio_stream::StreamExt;
    use tonic::{Code, Request as GrpcRequest, Status, Streaming};

    use spooled_backend::cache::RedisCache;
    use spooled_backend::config::{QueueSettings, RedisSettings, WorkerSettings};
    use spooled_backend::grpc::proto::process_request::Request as Op;
    use spooled_backend::grpc::proto::process_response::Response as Reply;
    use spooled_backend::grpc::proto::queue_service_server::QueueService;
    use spooled_backend::grpc::proto::{
        CompleteRequest, DequeueRequest, EnqueueRequest, FailRequest, ProcessRequest,
        ProcessResponse, RenewLeaseRequest, StreamJobsRequest,
    };
    use spooled_backend::grpc::services::QueueServiceImpl;
    use spooled_backend::observability::Metrics;
    use spooled_backend::outgoing_webhooks::service::OutgoingWebhookService;

    use super::common::{TestDatabase, TestRedis};
    use super::seed_tenant;

    /// `WORKER_LEASE_DURATION_SECS` for these services (prod value, REST only).
    const LEASE_SECS: u64 = 30;

    /// `GRPC_DEFAULT_LEASE_DURATION_SECS` for these services (the default).
    const GRPC_LEASE_SECS: u64 = 300;

    fn queue_service(db: &TestDatabase, cache: Option<RedisCache>) -> QueueServiceImpl {
        QueueServiceImpl::new(
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
            worker_settings(GRPC_LEASE_SECS),
        )
    }

    fn worker_settings(grpc_default_lease_secs: u64) -> WorkerSettings {
        WorkerSettings {
            heartbeat_interval_secs: 10,
            lease_duration_secs: LEASE_SECS,
            grpc_default_lease_duration_secs: grpc_default_lease_secs,
            max_concurrency: 10,
            fallback_poll_interval_secs: 5,
        }
    }

    fn grpc<T>(key: &str, body: T) -> GrpcRequest<T> {
        let mut request = GrpcRequest::new(body);
        request
            .metadata_mut()
            .insert("x-api-key", key.parse().unwrap());
        request
    }

    /// A ProcessJobs request stream: `messages` as gRPC frames, then `trailing`
    /// raw bytes, then end of stream.
    fn process_stream(
        key: &str,
        messages: Vec<Op>,
        trailing: &[u8],
    ) -> GrpcRequest<Streaming<ProcessRequest>> {
        let mut bytes = Vec::new();
        for op in messages {
            let body = ProcessRequest { request: Some(op) }.encode_to_vec();
            bytes.push(0u8);
            bytes.extend_from_slice(&(body.len() as u32).to_be_bytes());
            bytes.extend_from_slice(&body);
        }
        bytes.extend_from_slice(trailing);
        let mut codec = tonic_prost::ProstCodec::<ProcessResponse, ProcessRequest>::default();
        let decoder = tonic::codec::Codec::decoder(&mut codec);
        let streaming = Streaming::new_request(
            decoder,
            axum::body::Body::from(bytes),
            None,
            Some(4 * 1024 * 1024),
        );
        grpc(key, streaming)
    }

    async fn process(
        service: &QueueServiceImpl,
        request: GrpcRequest<Streaming<ProcessRequest>>,
    ) -> Vec<Result<ProcessResponse, Status>> {
        let stream = service.process_jobs(request).await.unwrap().into_inner();
        tokio::time::timeout(Duration::from_secs(20), stream.collect::<Vec<_>>())
            .await
            .expect("ProcessJobs response stream ends")
    }

    fn dequeue_req(queue: &str, worker: &str, lease: i32, batch: i32) -> DequeueRequest {
        DequeueRequest {
            queue_name: queue.to_string(),
            worker_id: worker.to_string(),
            lease_duration_secs: lease,
            batch_size: batch,
        }
    }

    fn stream_req(queue: &str, worker: &str, lease: i32) -> StreamJobsRequest {
        StreamJobsRequest {
            queue_name: queue.to_string(),
            worker_id: worker.to_string(),
            lease_duration_secs: lease,
        }
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

    async fn seed_job(
        pool: &sqlx::PgPool,
        org_id: &str,
        queue: &str,
        payload: Value,
        priority: i32,
    ) -> String {
        let id = uuid::Uuid::new_v4().to_string();
        sqlx::query(
            "INSERT INTO jobs (id, organization_id, queue_name, payload, status, priority, max_retries, created_at, updated_at) \
             VALUES ($1, $2, $3, $4, 'pending', $5, 3, clock_timestamp(), NOW())",
        )
        .bind(&id)
        .bind(org_id)
        .bind(queue)
        .bind(payload)
        .bind(priority)
        .execute(pool)
        .await
        .expect("seed job");
        id
    }

    /// A job leased to `worker` under `lease_id`, on its `retry_count`-th attempt.
    async fn seed_processing_job(
        pool: &sqlx::PgPool,
        org_id: &str,
        worker: &str,
        lease_id: &str,
        retry_count: i32,
    ) -> String {
        let id = uuid::Uuid::new_v4().to_string();
        sqlx::query(
            "INSERT INTO jobs (id, organization_id, queue_name, payload, status, assigned_worker_id, \
             lease_id, lease_expires_at, retry_count, max_retries, created_at, updated_at) \
             VALUES ($1, $2, 'fq', '{}'::JSONB, 'processing', $3, $4, NOW() + INTERVAL '5 minutes', $5, 3, NOW(), NOW())",
        )
        .bind(&id)
        .bind(org_id)
        .bind(worker)
        .bind(lease_id)
        .bind(retry_count)
        .execute(pool)
        .await
        .expect("seed processing job");
        id
    }

    /// (status, retry_count) of one job.
    async fn job_state(pool: &sqlx::PgPool, id: &str) -> (String, i32) {
        sqlx::query_as("SELECT status, retry_count FROM jobs WHERE id = $1")
            .bind(id)
            .fetch_one(pool)
            .await
            .unwrap()
    }

    async fn count_status(pool: &sqlx::PgPool, org_id: &str, status: &str) -> i64 {
        let (n,): (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM jobs WHERE organization_id = $1 AND status = $2")
                .bind(org_id)
                .bind(status)
                .fetch_one(pool)
                .await
                .unwrap();
        n
    }

    /// Seconds left on a job's lease.
    async fn lease_left(pool: &sqlx::PgPool, id: &str) -> f64 {
        let (secs,): (f64,) = sqlx::query_as(
            "SELECT EXTRACT(EPOCH FROM (lease_expires_at - NOW()))::FLOAT8 FROM jobs WHERE id = $1",
        )
        .bind(id)
        .fetch_one(pool)
        .await
        .unwrap();
        secs
    }

    fn assert_lease(secs: f64, expected: u64, what: &str) {
        assert!(
            secs > (expected as f64) - 10.0 && secs <= (expected as f64) + 1.0,
            "{what}: lease {secs}s, expected {expected}s"
        );
    }

    fn assert_default_lease(secs: f64, what: &str) {
        assert!(
            secs > (GRPC_LEASE_SECS as f64) - 10.0 && secs <= (GRPC_LEASE_SECS as f64) + 1.0,
            "{what}: lease {secs}s, expected GRPC_DEFAULT_LEASE_DURATION_SECS = {GRPC_LEASE_SECS} (not WORKER_LEASE_DURATION_SECS = {LEASE_SECS})"
        );
    }

    // BEGRPC-03: StreamJobs claims only when the consumer can take a job, and a
    // job claimed but never handed over goes back to pending on disconnect
    // without spending a retry.
    #[tokio::test]
    async fn grpc_stream_jobs_claims_on_demand_and_releases_unsent_jobs() {
        let db = TestDatabase::new().await;
        let t = seed_tenant(db.pool()).await;
        let service = queue_service(&db, None);
        let mut ids = Vec::new();
        for n in 0..6 {
            ids.push(seed_job(db.pool(), &t.org_id, "sq", json!({ "n": n }), 0).await);
        }

        let mut stream = service
            .stream_jobs(grpc(&t.key, stream_req("sq", "w-stream", 60)))
            .await
            .unwrap()
            .into_inner();
        let first = tokio::time::timeout(Duration::from_secs(10), stream.next())
            .await
            .expect("a job within 10s")
            .expect("stream open")
            .expect("a job, not an error");
        assert_eq!(first.id, ids[0]);

        // An eager producer would lease the whole queue to a consumer that took one.
        tokio::time::sleep(Duration::from_millis(1500)).await;
        let processing = count_status(db.pool(), &t.org_id, "processing").await;
        assert!(
            processing <= 2,
            "{processing} jobs leased to a consumer that took one"
        );

        drop(stream);
        let mut processing = -1;
        for _ in 0..50 {
            processing = count_status(db.pool(), &t.org_id, "processing").await;
            if processing == 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        assert_eq!(processing, 1, "only the delivered job stays leased");
        assert_eq!(job_state(db.pool(), &first.id).await.0, "processing");
        let (retries,): (i64,) = sqlx::query_as(
            "SELECT COALESCE(SUM(retry_count), 0)::BIGINT FROM jobs WHERE organization_id = $1",
        )
        .bind(&t.org_id)
        .fetch_one(db.pool())
        .await
        .unwrap();
        assert_eq!(retries, 0, "releasing an unsent job must not burn a retry");
    }

    // BEGRPC-04: a batch never claims past the 4 MiB response limit; what does
    // not fit stays pending with its retries intact.
    #[tokio::test]
    async fn grpc_dequeue_keeps_the_batch_under_the_message_limit() {
        let db = TestDatabase::new().await;
        let t = seed_tenant(db.pool()).await;
        let service = queue_service(&db, None);
        let blob = "x".repeat(1_200_000);
        for n in 0..4 {
            seed_job(
                db.pool(),
                &t.org_id,
                "big",
                json!({ "n": n, "blob": blob }),
                0,
            )
            .await;
        }

        let resp = service
            .dequeue(grpc(&t.key, dequeue_req("big", "w-big", 60, 4)))
            .await
            .unwrap()
            .into_inner();
        assert!(!resp.jobs.is_empty());
        assert!(
            resp.encoded_len() <= 4 * 1024 * 1024,
            "{} jobs, {} bytes: tonic would fail this with OUT_OF_RANGE after the claim",
            resp.jobs.len(),
            resp.encoded_len()
        );
        let processing = count_status(db.pool(), &t.org_id, "processing").await;
        assert_eq!(
            processing as usize,
            resp.jobs.len(),
            "no job leased but unsent"
        );
        assert_eq!(
            count_status(db.pool(), &t.org_id, "pending").await as usize,
            4 - resp.jobs.len()
        );
    }

    // BEGRPC-04: a job that cannot fit in any gRPC message is dead-lettered
    // with a reason instead of being re-claimed forever; the next job flows.
    #[tokio::test]
    async fn grpc_claims_dead_letter_a_job_too_large_for_one_message() {
        let db = TestDatabase::new().await;
        let t = seed_tenant(db.pool()).await;
        let service = queue_service(&db, None);
        let huge = "y".repeat(4_300_000);

        for (path, queue) in [
            ("Dequeue", "h1"),
            ("StreamJobs", "h2"),
            ("ProcessJobs", "h3"),
        ] {
            let big = seed_job(db.pool(), &t.org_id, queue, json!({ "blob": huge }), 10).await;
            let small = seed_job(db.pool(), &t.org_id, queue, json!({ "n": 1 }), 0).await;

            let got = match path {
                "Dequeue" => {
                    let resp = service
                        .dequeue(grpc(&t.key, dequeue_req(queue, "w-huge", 60, 1)))
                        .await
                        .unwrap()
                        .into_inner();
                    resp.jobs.into_iter().map(|j| j.id).collect::<Vec<_>>()
                }
                "StreamJobs" => {
                    let mut stream = service
                        .stream_jobs(grpc(&t.key, stream_req(queue, "w-huge", 60)))
                        .await
                        .unwrap()
                        .into_inner();
                    let job = tokio::time::timeout(Duration::from_secs(10), stream.next())
                        .await
                        .expect("a job within 10s")
                        .expect("stream open")
                        .expect("a job, not an error");
                    vec![job.id]
                }
                _ => {
                    let replies = process(
                        &service,
                        process_stream(
                            &t.key,
                            vec![Op::Dequeue(dequeue_req(queue, "w-huge", 60, 1))],
                            &[],
                        ),
                    )
                    .await;
                    replies
                        .into_iter()
                        .filter_map(|r| match r.ok()?.response? {
                            Reply::Job(job) => Some(job.id),
                            _ => None,
                        })
                        .collect()
                }
            };
            assert_eq!(got, vec![small.clone()], "{path}");

            let (status, last_error): (String, Option<String>) =
                sqlx::query_as("SELECT status, last_error FROM jobs WHERE id = $1")
                    .bind(&big)
                    .fetch_one(db.pool())
                    .await
                    .unwrap();
            assert_eq!(status, "deadletter", "{path}");
            assert!(
                last_error.unwrap_or_default().contains("too large"),
                "{path}: the dead-letter reason names the size"
            );
            let (dlq,): (i64,) =
                sqlx::query_as("SELECT COUNT(*) FROM dead_letter_queue WHERE job_id = $1")
                    .bind(&big)
                    .fetch_one(db.pool())
                    .await
                    .unwrap();
            assert_eq!(dlq, 1, "{path}");
        }
    }

    // BEGRPC-24: a claim error mid-batch returns the jobs already claimed
    // instead of failing the RPC and stranding their leases.
    #[tokio::test]
    async fn grpc_dequeue_returns_jobs_claimed_before_a_mid_batch_error() {
        let db = TestDatabase::new().await;
        let t = seed_tenant(db.pool()).await;
        let service = queue_service(&db, None);
        let first = seed_job(db.pool(), &t.org_id, "mq", json!({ "n": 1 }), 10).await;
        let poisoned = seed_job(
            db.pool(),
            &t.org_id,
            "mq",
            json!({ "bh_fail_claim": true }),
            5,
        )
        .await;
        seed_job(db.pool(), &t.org_id, "mq", json!({ "n": 3 }), 0).await;
        sqlx::query(
            "CREATE FUNCTION bh_fail_claim() RETURNS trigger LANGUAGE plpgsql AS $$ \
             BEGIN \
               IF NEW.status = 'processing' AND NEW.payload ? 'bh_fail_claim' THEN \
                 RAISE EXCEPTION 'bh: claim failure'; \
               END IF; \
               RETURN NEW; \
             END $$",
        )
        .execute(db.pool())
        .await
        .unwrap();
        sqlx::query(
            "CREATE TRIGGER bh_fail_claim BEFORE UPDATE ON jobs \
             FOR EACH ROW EXECUTE FUNCTION bh_fail_claim()",
        )
        .execute(db.pool())
        .await
        .unwrap();

        let resp = service
            .dequeue(grpc(&t.key, dequeue_req("mq", "w-mid", 60, 3)))
            .await
            .expect("jobs claimed before the error are returned")
            .into_inner();
        let ids: Vec<_> = resp.jobs.into_iter().map(|j| j.id).collect();
        assert_eq!(ids, vec![first.clone()]);
        assert_eq!(job_state(db.pool(), &first).await.0, "processing");
        assert_eq!(job_state(db.pool(), &poisoned).await.0, "pending");
    }

    // BEGRPC-09: Enqueue applies the REST `CreateJobRequest` limits, and a
    // database CHECK failure is INVALID_ARGUMENT, not INTERNAL.
    #[tokio::test]
    async fn grpc_enqueue_applies_the_rest_field_limits() {
        let db = TestDatabase::new().await;
        let t = seed_tenant(db.pool()).await;
        let service = queue_service(&db, None);

        let mut big_tags = std::collections::HashMap::new();
        big_tags.insert("note".to_string(), "t".repeat(70_000));
        let cases = vec![
            (
                "priority 101",
                EnqueueRequest {
                    priority: 101,
                    ..enqueue_req("vq")
                },
            ),
            (
                "priority -101",
                EnqueueRequest {
                    priority: -101,
                    ..enqueue_req("vq")
                },
            ),
            (
                "tags over 64 KB",
                EnqueueRequest {
                    tags: big_tags,
                    ..enqueue_req("vq")
                },
            ),
            (
                "idempotency key over 255",
                EnqueueRequest {
                    idempotency_key: "k".repeat(256),
                    ..enqueue_req("vq")
                },
            ),
            ("queue name over 100", enqueue_req(&"q".repeat(101))),
        ];
        for (case, req) in cases {
            let err = service.enqueue(grpc(&t.key, req)).await.expect_err(case);
            assert_eq!(err.code(), Code::InvalidArgument, "{case}: {err:?}");
        }
        let (stored,): (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM jobs WHERE organization_id = $1")
                .bind(&t.org_id)
                .fetch_one(db.pool())
                .await
                .unwrap();
        assert_eq!(stored, 0);

        // The limits themselves are accepted.
        let at_limit = EnqueueRequest {
            priority: 100,
            idempotency_key: "k".repeat(255),
            ..enqueue_req(&"q".repeat(100))
        };
        service
            .enqueue(grpc(&t.key, at_limit))
            .await
            .expect("values at the limits");

        // Whatever the database still rejects as out of range is the caller's.
        sqlx::query(
            "CREATE FUNCTION bh_reject_job() RETURNS trigger LANGUAGE plpgsql AS $$ \
             BEGIN \
               IF NEW.queue_name = 'bh-check' THEN \
                 RAISE EXCEPTION 'bh: rejected' USING ERRCODE = 'check_violation'; \
               END IF; \
               RETURN NEW; \
             END $$",
        )
        .execute(db.pool())
        .await
        .unwrap();
        sqlx::query(
            "CREATE TRIGGER bh_reject_job BEFORE INSERT ON jobs \
             FOR EACH ROW EXECUTE FUNCTION bh_reject_job()",
        )
        .execute(db.pool())
        .await
        .unwrap();
        let err = service
            .enqueue(grpc(&t.key, enqueue_req("bh-check")))
            .await
            .expect_err("check violation");
        assert_eq!(err.code(), Code::InvalidArgument, "{err:?}");
    }

    // BEGRPC-21: a scheduled_at the server cannot represent is rejected instead
    // of silently running the job now; the all-zero Timestamp means unset.
    #[tokio::test]
    async fn grpc_enqueue_rejects_an_invalid_scheduled_at() {
        let db = TestDatabase::new().await;
        let t = seed_tenant(db.pool()).await;
        let service = queue_service(&db, None);
        let in_an_hour = chrono::Utc::now().timestamp() + 3600;

        for (case, ts) in [
            (
                "negative nanos",
                Timestamp {
                    seconds: in_an_hour,
                    nanos: -1,
                },
            ),
            (
                "nanos past 1s",
                Timestamp {
                    seconds: in_an_hour,
                    nanos: 1_000_000_000,
                },
            ),
            (
                "after year 9999",
                Timestamp {
                    seconds: 253_402_300_800,
                    nanos: 0,
                },
            ),
            (
                "before year 1",
                Timestamp {
                    seconds: -62_135_596_801,
                    nanos: 0,
                },
            ),
        ] {
            let req = EnqueueRequest {
                scheduled_at: Some(ts),
                ..enqueue_req("tq")
            };
            let err = service.enqueue(grpc(&t.key, req)).await.expect_err(case);
            assert_eq!(err.code(), Code::InvalidArgument, "{case}: {err:?}");
        }

        let req = EnqueueRequest {
            scheduled_at: Some(Timestamp {
                seconds: 0,
                nanos: 0,
            }),
            ..enqueue_req("tq")
        };
        let id = service
            .enqueue(grpc(&t.key, req))
            .await
            .unwrap()
            .into_inner()
            .job_id;
        let (status, scheduled): (String, Option<chrono::DateTime<chrono::Utc>>) =
            sqlx::query_as("SELECT status, scheduled_at FROM jobs WHERE id = $1")
                .bind(&id)
                .fetch_one(db.pool())
                .await
                .unwrap();
        assert_eq!(status, "pending");
        assert_eq!(scheduled, None);
    }

    // W3R-03 (BEGRPC-10): an omitted lease (0 on the wire) is
    // GRPC_DEFAULT_LEASE_DURATION_SECS (300 s) on every claim path and on
    // renewal, not the 30 s REST default: the Go SDK's StreamJobs cannot send a
    // lease, and on 30 s its longer jobs were handed to a second worker.
    #[tokio::test]
    async fn grpc_omitted_lease_keeps_the_300s_grpc_default() {
        let db = TestDatabase::new().await;
        let t = seed_tenant(db.pool()).await;
        let service = queue_service(&db, None);
        for n in 0..3 {
            seed_job(db.pool(), &t.org_id, "lq", json!({ "n": n }), 0).await;
        }

        let job = service
            .dequeue(grpc(&t.key, dequeue_req("lq", "w-lease", 0, 1)))
            .await
            .unwrap()
            .into_inner()
            .jobs
            .remove(0);
        assert_default_lease(lease_left(db.pool(), &job.id).await, "Dequeue");

        sqlx::query("UPDATE jobs SET lease_expires_at = NOW() + INTERVAL '2 hours' WHERE id = $1")
            .bind(&job.id)
            .execute(db.pool())
            .await
            .unwrap();
        service
            .renew_lease(grpc(
                &t.key,
                RenewLeaseRequest {
                    job_id: job.id.clone(),
                    worker_id: "w-lease".to_string(),
                    extension_secs: 0,
                    lease_id: job.lease_id.clone(),
                },
            ))
            .await
            .unwrap();
        assert_default_lease(lease_left(db.pool(), &job.id).await, "RenewLease");

        let mut stream = service
            .stream_jobs(grpc(&t.key, stream_req("lq", "w-lease", 0)))
            .await
            .unwrap()
            .into_inner();
        let streamed = tokio::time::timeout(Duration::from_secs(10), stream.next())
            .await
            .expect("a job within 10s")
            .expect("stream open")
            .expect("a job, not an error");
        drop(stream);
        assert_default_lease(lease_left(db.pool(), &streamed.id).await, "StreamJobs");

        let replies = process(
            &service,
            process_stream(
                &t.key,
                vec![Op::Dequeue(dequeue_req("lq", "w-lease", 0, 1))],
                &[],
            ),
        )
        .await;
        let processed = match replies[0].as_ref().unwrap().response.as_ref().unwrap() {
            Reply::Job(job) => job.id.clone(),
            other => panic!("expected a job, got {other:?}"),
        };
        assert_default_lease(lease_left(db.pool(), &processed).await, "ProcessJobs");

        // An explicit lease is still honoured.
        seed_job(db.pool(), &t.org_id, "lq", json!({ "n": 3 }), 0).await;
        let job = service
            .dequeue(grpc(&t.key, dequeue_req("lq", "w-lease", 60, 1)))
            .await
            .unwrap()
            .into_inner()
            .jobs
            .remove(0);
        assert_lease(lease_left(db.pool(), &job.id).await, 60, "explicit Dequeue");

        // The default is the operator's GRPC_DEFAULT_LEASE_DURATION_SECS.
        let configured = QueueServiceImpl::new(
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
            worker_settings(900),
        );
        seed_job(db.pool(), &t.org_id, "lq", json!({ "n": 4 }), 0).await;
        let job = configured
            .dequeue(grpc(&t.key, dequeue_req("lq", "w-lease", 0, 1)))
            .await
            .unwrap()
            .into_inner()
            .jobs
            .remove(0);
        assert_lease(
            lease_left(db.pool(), &job.id).await,
            900,
            "configured Dequeue",
        );
    }

    // BEGRPC-12: one org cannot hold an unbounded number of StreamJobs streams.
    #[tokio::test]
    async fn grpc_stream_jobs_caps_open_streams_per_org() {
        let db = TestDatabase::new().await;
        let t = seed_tenant(db.pool()).await;
        let other = seed_tenant(db.pool()).await;
        let service = queue_service(&db, None);

        let mut streams = Vec::new();
        let mut rejected = None;
        for _ in 0..120 {
            match service
                .stream_jobs(grpc(&t.key, stream_req("idle", "w-cap", 0)))
                .await
            {
                Ok(resp) => streams.push(resp.into_inner()),
                Err(status) => {
                    rejected = Some(status);
                    break;
                }
            }
        }
        let status = rejected.expect("120 concurrent streams opened for one org");
        assert_eq!(status.code(), Code::ResourceExhausted, "{status:?}");
        assert_eq!(streams.len(), 100);

        // The cap is per organization.
        let _other = service
            .stream_jobs(grpc(&other.key, stream_req("idle", "w-cap", 0)))
            .await
            .expect("another org opens its own stream");

        // Closing a stream frees its slot.
        drop(streams.pop());
        let mut reopened = false;
        for _ in 0..50 {
            if let Ok(resp) = service
                .stream_jobs(grpc(&t.key, stream_req("idle", "w-cap", 0)))
                .await
            {
                streams.push(resp.into_inner());
                reopened = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        assert!(reopened, "a closed stream gives its slot back");
    }

    // BEGRPC-12 / BEGRPC-22: StreamJobs charges the limiter for delivered jobs,
    // not idle polls; a limit hit backs off without ending the stream; the
    // key's current limit applies, not the one read when the stream opened.
    #[tokio::test]
    async fn grpc_stream_jobs_charges_delivered_jobs_and_backs_off_when_limited() {
        let redis = TestRedis::new().await;
        let cache = RedisCache::connect(&RedisSettings {
            url: redis.url.clone(),
            pool_size: 4,
        })
        .await
        .expect("connect redis");
        let db = TestDatabase::new().await;
        let t = seed_tenant(db.pool()).await;
        // Two requests per minute: one for opening the stream, one for a job.
        sqlx::query("UPDATE api_keys SET rate_limit = 2 WHERE organization_id = $1")
            .bind(&t.org_id)
            .execute(db.pool())
            .await
            .unwrap();
        let service = queue_service(&db, Some(cache));

        let mut stream = service
            .stream_jobs(grpc(&t.key, stream_req("rq", "w-rate", 60)))
            .await
            .expect("stream opens")
            .into_inner();
        let idle = tokio::time::timeout(Duration::from_millis(2500), stream.next()).await;
        assert!(
            idle.is_err(),
            "idle polls must not use up the key's limit: {idle:?}"
        );

        let a = seed_job(db.pool(), &t.org_id, "rq", json!({ "n": "a" }), 0).await;
        let got = tokio::time::timeout(Duration::from_secs(5), stream.next())
            .await
            .expect("a job within 5s")
            .expect("stream open")
            .expect("a job, not an error");
        assert_eq!(got.id, a);
        // The worker finishes `a`, so the stream may hand out another job.
        sqlx::query("UPDATE jobs SET status = 'completed', completed_at = NOW() WHERE id = $1")
            .bind(&a)
            .execute(db.pool())
            .await
            .unwrap();

        // The key is now at its limit: the next job waits, the stream stays open.
        let b = seed_job(db.pool(), &t.org_id, "rq", json!({ "n": "b" }), 0).await;
        let limited = tokio::time::timeout(Duration::from_millis(2500), stream.next()).await;
        assert!(
            limited.is_err(),
            "a rate-limited stream backs off, it does not end: {limited:?}"
        );
        let mut state = job_state(db.pool(), &b).await;
        for _ in 0..20 {
            if state.0 == "pending" {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
            state = job_state(db.pool(), &b).await;
        }
        assert_eq!(
            state,
            ("pending".to_string(), 0),
            "the held-back job keeps its retries"
        );

        // Lifting the key's limit takes effect on the open stream.
        sqlx::query("UPDATE api_keys SET rate_limit = NULL WHERE organization_id = $1")
            .bind(&t.org_id)
            .execute(db.pool())
            .await
            .unwrap();
        let got = tokio::time::timeout(Duration::from_secs(5), stream.next())
            .await
            .expect("delivered once the limit is lifted")
            .expect("stream open")
            .expect("a job, not an error");
        assert_eq!(got.id, b);
    }

    // BEGRPC-01: unary calls through the service use the shared key cache, so a
    // repeat call within the TTL skips bcrypt.
    #[tokio::test]
    async fn grpc_unary_calls_use_the_api_key_cache() {
        let redis = TestRedis::new().await;
        let cache = RedisCache::connect(&RedisSettings {
            url: redis.url.clone(),
            pool_size: 4,
        })
        .await
        .expect("connect redis");
        let db = TestDatabase::new().await;
        let t = seed_tenant(db.pool()).await;
        let service = queue_service(&db, Some(cache));
        let first = seed_processing_job(db.pool(), &t.org_id, "w-cache", "lease-c1", 0).await;
        let second = seed_processing_job(db.pool(), &t.org_id, "w-cache", "lease-c2", 0).await;
        let complete = |id: &str, lease: &str| CompleteRequest {
            job_id: id.to_string(),
            worker_id: "w-cache".to_string(),
            result: None,
            lease_id: lease.to_string(),
        };
        service
            .complete(grpc(&t.key, complete(&first, "lease-c1")))
            .await
            .expect("first call authenticates");

        // A call that ran bcrypt against the broken hash would now fail.
        sqlx::query(
            "UPDATE api_keys SET key_hash = substr(key_hash, 1, 16) || repeat('x', 44) WHERE organization_id = $1",
        )
        .bind(&t.org_id)
        .execute(db.pool())
        .await
        .unwrap();
        service
            .complete(grpc(&t.key, complete(&second, "lease-c2")))
            .await
            .expect("a cached key authenticates without bcrypt");
    }

    // BEGRPC-17: settling a job that already finished says so, like REST's 409.
    #[tokio::test]
    async fn grpc_settling_a_finished_job_is_failed_precondition() {
        let db = TestDatabase::new().await;
        let t = seed_tenant(db.pool()).await;
        let service = queue_service(&db, None);
        let id = seed_processing_job(db.pool(), &t.org_id, "w-done", "lease-done", 0).await;
        let complete = || CompleteRequest {
            job_id: id.clone(),
            worker_id: "w-done".to_string(),
            result: None,
            lease_id: "lease-done".to_string(),
        };
        service.complete(grpc(&t.key, complete())).await.unwrap();

        let again = service
            .complete(grpc(&t.key, complete()))
            .await
            .unwrap_err();
        assert_eq!(again.code(), Code::FailedPrecondition, "{again:?}");
        assert_eq!(again.message(), "Job is already completed");
        let fail = service
            .fail(grpc(
                &t.key,
                FailRequest {
                    job_id: id.clone(),
                    worker_id: "w-done".to_string(),
                    error: "late".to_string(),
                    retry: None,
                    lease_id: "lease-done".to_string(),
                },
            ))
            .await
            .unwrap_err();
        assert_eq!(fail.code(), Code::FailedPrecondition, "{fail:?}");
        let renew = service
            .renew_lease(grpc(
                &t.key,
                RenewLeaseRequest {
                    job_id: id.clone(),
                    worker_id: "w-done".to_string(),
                    extension_secs: 60,
                    lease_id: "lease-done".to_string(),
                },
            ))
            .await
            .unwrap_err();
        assert_eq!(renew.code(), Code::FailedPrecondition, "{renew:?}");

        // Someone else's job is still NOT_FOUND.
        let stranger = service
            .complete(grpc(
                &t.key,
                CompleteRequest {
                    worker_id: "w-other".to_string(),
                    ..complete()
                },
            ))
            .await
            .unwrap_err();
        assert_eq!(stranger.code(), Code::NotFound, "{stranger:?}");

        // ProcessJobs answers in band with the same meaning.
        let replies = process(
            &service,
            process_stream(&t.key, vec![Op::Complete(complete())], &[]),
        )
        .await;
        match replies[0].as_ref().unwrap().response.as_ref().unwrap() {
            Reply::Error(e) => {
                assert_eq!(e.code, "CONFLICT");
                assert_eq!(e.message, "Job is already completed");
            }
            other => panic!("expected an error reply, got {other:?}"),
        }
    }

    // BEGRPC-16: dead-lettering does not count an extra attempt (REST parity).
    #[tokio::test]
    async fn grpc_dead_letter_keeps_retry_count_like_rest() {
        let db = TestDatabase::new().await;
        let t = seed_tenant(db.pool()).await;
        let service = queue_service(&db, None);
        let fail = |id: &str, lease: &str, retry: Option<bool>| FailRequest {
            job_id: id.to_string(),
            worker_id: "w-dlq".to_string(),
            error: "boom".to_string(),
            retry,
            lease_id: lease.to_string(),
        };
        let pool = db.pool().clone();
        let total_retries = |id: String| {
            let pool = pool.clone();
            async move {
                let (n,): (Option<i64>,) = sqlx::query_as(
                    "SELECT (error_details->>'total_retries')::BIGINT FROM dead_letter_queue WHERE job_id = $1",
                )
                .bind(&id)
                .fetch_one(&pool)
                .await
                .unwrap();
                n
            }
        };

        // Last attempt (3 of 3) through the unary RPC.
        let last = seed_processing_job(db.pool(), &t.org_id, "w-dlq", "l-last", 3).await;
        let resp = service
            .fail(grpc(&t.key, fail(&last, "l-last", None)))
            .await
            .unwrap()
            .into_inner();
        assert!(!resp.will_retry);
        assert_eq!(
            job_state(db.pool(), &last).await,
            ("deadletter".to_string(), 3)
        );
        assert_eq!(total_retries(last).await, Some(3));

        // retry = false on a first attempt counts no retry.
        let first = seed_processing_job(db.pool(), &t.org_id, "w-dlq", "l-first", 0).await;
        service
            .fail(grpc(&t.key, fail(&first, "l-first", Some(false))))
            .await
            .unwrap();
        assert_eq!(
            job_state(db.pool(), &first).await,
            ("deadletter".to_string(), 0)
        );
        assert_eq!(total_retries(first).await, Some(0));

        // A retried failure still counts the attempt.
        let retried = seed_processing_job(db.pool(), &t.org_id, "w-dlq", "l-retry", 1).await;
        service
            .fail(grpc(&t.key, fail(&retried, "l-retry", None)))
            .await
            .unwrap();
        assert_eq!(
            job_state(db.pool(), &retried).await,
            ("pending".to_string(), 2)
        );

        // Same through ProcessJobs.
        let streamed = seed_processing_job(db.pool(), &t.org_id, "w-dlq", "l-stream", 3).await;
        let replies = process(
            &service,
            process_stream(
                &t.key,
                vec![Op::Fail(fail(&streamed, "l-stream", None))],
                &[],
            ),
        )
        .await;
        assert!(
            matches!(replies[0].as_ref().unwrap().response, Some(Reply::Fail(_))),
            "{replies:?}"
        );
        assert_eq!(
            job_state(db.pool(), &streamed).await,
            ("deadletter".to_string(), 3)
        );
        assert_eq!(total_retries(streamed).await, Some(3));
    }

    // BEGRPC-18: the gRPC job.created webhook reports the real initial status
    // and the priority, like REST.
    #[tokio::test]
    async fn grpc_job_created_webhook_reports_initial_status_and_priority() {
        let db = TestDatabase::new().await;
        let t = seed_tenant(db.pool()).await;
        let service = queue_service(&db, None);
        sqlx::query(
            "INSERT INTO outgoing_webhooks (organization_id, name, url, events) \
             VALUES ($1, 'bh', 'http://127.0.0.1:9/bh', ARRAY['job.created'])",
        )
        .bind(&t.org_id)
        .execute(db.pool())
        .await
        .unwrap();

        let req = EnqueueRequest {
            priority: 7,
            scheduled_at: Some(Timestamp {
                seconds: chrono::Utc::now().timestamp() + 3600,
                nanos: 0,
            }),
            ..enqueue_req("wq")
        };
        service.enqueue(grpc(&t.key, req)).await.unwrap();

        let mut data = None;
        for _ in 0..100 {
            let row: Option<(Value,)> = sqlx::query_as(
                "SELECT d.payload->'data' FROM outgoing_webhook_deliveries d \
                 JOIN outgoing_webhooks w ON w.id = d.webhook_id \
                 WHERE w.organization_id = $1 AND d.event = 'job.created'",
            )
            .bind(&t.org_id)
            .fetch_optional(db.pool())
            .await
            .unwrap();
            if let Some((v,)) = row {
                data = Some(v);
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        let data = data.expect("a job.created delivery attempt is recorded");
        assert_eq!(data["status"], "scheduled", "{data}");
        assert_eq!(data["priority"], 7, "{data}");
    }

    // BEGRPC-23: a broken ProcessJobs request stream ends the call with its
    // error instead of an OK that looks like a clean close.
    #[tokio::test]
    async fn grpc_process_jobs_ends_with_the_request_stream_error() {
        let db = TestDatabase::new().await;
        let t = seed_tenant(db.pool()).await;
        let service = queue_service(&db, None);

        // One good message, then a frame header announcing 5 MiB (over the cap).
        let mut oversized = vec![0u8];
        oversized.extend_from_slice(&(5u32 * 1024 * 1024).to_be_bytes());
        let replies = process(
            &service,
            process_stream(
                &t.key,
                vec![Op::Dequeue(dequeue_req("pq", "w-p", 0, 1))],
                &oversized,
            ),
        )
        .await;
        assert!(
            matches!(
                replies.first(),
                Some(Ok(ProcessResponse { response: Some(Reply::Error(e)) })) if e.code == "NO_JOBS"
            ),
            "{replies:?}"
        );
        match replies.last() {
            Some(Err(status)) => assert_eq!(status.code(), Code::OutOfRange, "{status:?}"),
            other => panic!("the call must end with the decode error, got {other:?}"),
        }
    }
}
// ---- wave 3 / auth ----

/// Router backed by Redis: the API-key cache, the negative cache and the
/// failed-auth throttle all live there. Keep the `TestRedis` alive.
async fn app_with_redis(
    db: &TestDatabase,
) -> (
    Router,
    spooled_backend::cache::RedisCache,
    common::TestRedis,
) {
    app_with_redis_db(db, None).await
}

/// [`app_with_redis`], on logical Redis database `index` when given. A test
/// that spends the gRPC failed-auth budget (one key for every gRPC caller)
/// uses its own database, so gRPC tests sharing the default one never see it
/// spent.
async fn app_with_redis_db(
    db: &TestDatabase,
    index: Option<u8>,
) -> (
    Router,
    spooled_backend::cache::RedisCache,
    common::TestRedis,
) {
    let redis = common::TestRedis::new().await;
    let url = match index {
        None => redis.url.clone(),
        Some(index) => {
            let base = redis.url.trim_end_matches('/');
            let base = match base.rsplit_once('/') {
                Some((head, tail))
                    if !head.ends_with('/') && tail.chars().all(|c| c.is_ascii_digit()) =>
                {
                    head
                }
                _ => base,
            };
            format!("{base}/{index}")
        }
    };
    let cache =
        spooled_backend::cache::RedisCache::connect(&spooled_backend::config::RedisSettings {
            url,
            pool_size: 4,
        })
        .await
        .expect("connect redis");
    let app = router(AppState::new(
        Database::from_pool(Arc::clone(&db.pool)),
        Some(cache.clone()),
        Metrics::new(),
        Settings::load_for_testing(),
    ));
    (app, cache, redis)
}

/// A client address no other test uses (the throttle buckets live in a
/// shared Redis). IPv4, so the address is its own rate-limit id: an IPv6
/// client is keyed on its /64.
fn unique_client_ip() -> String {
    let [_, a, b, c] = (uuid::Uuid::new_v4().as_u128() as u32).to_be_bytes();
    format!("10.{a}.{b}.{c}")
}

/// Insert an active, unrestricted key for `org_id`. `legacy` leaves
/// `lookup_hash` NULL, like keys created before 0.1.90.
async fn insert_key(pool: &sqlx::PgPool, org_id: &str, key: &str, legacy: bool) -> String {
    insert_key_with_cost(pool, org_id, key, legacy, 4).await
}

/// [`insert_key`] hashed at bcrypt `cost` (production keys use 12).
async fn insert_key_with_cost(
    pool: &sqlx::PgPool,
    org_id: &str,
    key: &str,
    legacy: bool,
    cost: u32,
) -> String {
    let id = uuid::Uuid::new_v4().to_string();
    sqlx::query(
        "INSERT INTO api_keys (id, organization_id, key_hash, key_prefix, lookup_hash, name, queues, is_active, created_at) \
         VALUES ($1, $2, $3, $4, $5, 'w3', ARRAY[]::TEXT[], TRUE, NOW())",
    )
    .bind(&id)
    .bind(org_id)
    .bind(bcrypt::hash(key, cost).expect("hash key"))
    .bind(key.chars().take(8).collect::<String>())
    .bind((!legacy).then(|| spooled_backend::models::api_key_lookup_hash(key)))
    .execute(pool)
    .await
    .expect("insert key");
    id
}

/// Fail auth from `ip` with distinct unknown tokens until it is over budget.
async fn exhaust_failed_auth_budget(app: &Router, ip: &str) {
    for _ in 0..=spooled_backend::api::middleware::auth::FAILED_AUTH_PER_IP_LIMIT {
        let bogus = bearer(&format!("sp_test_{}", uuid::Uuid::new_v4().simple()));
        let (status, body) = call(
            app,
            "GET",
            "/api/v1/jobs",
            &[("Authorization", &bogus), ("CF-Connecting-IP", ip)],
            None,
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");
    }
}

// ---------------------------------------------------------------------------
// BEAUTH-05 / BEGRPC-13: the per-IP failed-auth throttle only gates the
// legacy bcrypt scan. Production sees one proxy address for every client, so
// a key that matches by lookup_hash must keep working when that address is
// over budget.
// ---------------------------------------------------------------------------
#[tokio::test]
async fn w3_auth_fast_path_key_still_authenticates_when_its_ip_is_over_budget() {
    let db = TestDatabase::new().await;
    let tenant = seed_tenant(db.pool()).await;
    let (app, _cache, _redis) = app_with_redis(&db).await;
    let ip = unique_client_ip();

    exhaust_failed_auth_budget(&app, &ip).await;

    let (status, body) = call(
        &app,
        "GET",
        "/api/v1/jobs",
        &[
            ("Authorization", &bearer(&tenant.key)),
            ("CF-Connecting-IP", &ip),
        ],
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
}

#[tokio::test]
async fn w3_auth_legacy_key_is_not_scanned_while_its_ip_is_over_budget() {
    let db = TestDatabase::new().await;
    let tenant = seed_tenant(db.pool()).await;
    let legacy = format!("sp_test_{}", uuid::Uuid::new_v4().simple());
    insert_key(db.pool(), &tenant.org_id, &legacy, true).await;
    let (app, _cache, _redis) = app_with_redis(&db).await;
    let throttled_ip = unique_client_ip();

    exhaust_failed_auth_budget(&app, &throttled_ip).await;

    let (status, body) = call(
        &app,
        "GET",
        "/api/v1/jobs",
        &[
            ("Authorization", &bearer(&legacy)),
            ("CF-Connecting-IP", &throttled_ip),
        ],
        None,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::UNAUTHORIZED,
        "an address over its failed-auth budget must not run the legacy scan: {body}"
    );

    // The skipped scan proved nothing about the token, so it must not have
    // been negatively cached: from another address the key works at once.
    let (status, body) = call(
        &app,
        "GET",
        "/api/v1/jobs",
        &[
            ("Authorization", &bearer(&legacy)),
            ("CF-Connecting-IP", &unique_client_ip()),
        ],
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
}

// ---------------------------------------------------------------------------
// BEAUTH-05: an unknown token is remembered for a minute, so repeating it
// costs one Redis read instead of the database lookups and the legacy scan.
// ---------------------------------------------------------------------------
#[tokio::test]
async fn w3_auth_negative_cache_short_circuits_a_repeated_unknown_token() {
    let db = TestDatabase::new().await;
    let tenant = seed_tenant(db.pool()).await;
    let (app, cache, _redis) = app_with_redis(&db).await;
    let token = format!("sp_test_{}", uuid::Uuid::new_v4().simple());
    let auth = bearer(&token);
    let ip = unique_client_ip();
    let headers = [
        ("Authorization", auth.as_str()),
        ("CF-Connecting-IP", ip.as_str()),
    ];

    let (status, _) = call(&app, "GET", "/api/v1/jobs", &headers, None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    // Make the token valid behind the cache's back: a repeat inside the TTL is
    // answered from the negative cache without reaching the database.
    insert_key(db.pool(), &tenant.org_id, &token, false).await;
    let (status, body) = call(&app, "GET", "/api/v1/jobs", &headers, None).await;
    assert_eq!(
        status,
        StatusCode::UNAUTHORIZED,
        "a repeated unknown token must be answered from the negative cache: {body}"
    );

    cache
        .delete(&format!(
            "auth_miss:{}",
            spooled_backend::models::api_key_lookup_hash(&token)
        ))
        .await
        .unwrap();
    let (status, body) = call(&app, "GET", "/api/v1/jobs", &headers, None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
}

// ---------------------------------------------------------------------------
// BEOPS-27: a database failure during API-key auth is a generic 503, never
// raw sqlx text sent to an unauthenticated caller.
// ---------------------------------------------------------------------------
#[tokio::test]
async fn w3_auth_database_errors_are_not_leaked_to_unauthenticated_callers() {
    let db = TestDatabase::new().await;
    let closed = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .connect_lazy_with((*db.pool.connect_options()).clone());
    closed.close().await;
    let app = router(AppState::new(
        Database::from_pool(Arc::new(closed)),
        None,
        Metrics::new(),
        Settings::load_for_testing(),
    ));

    let (status, body) = call(
        &app,
        "GET",
        "/api/v1/jobs",
        &[("Authorization", &bearer("sp_test_unknown"))],
        None,
    )
    .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    let message = body["message"].as_str().unwrap_or_default().to_lowercase();
    assert!(
        !message.contains("database error") && !message.contains("pool"),
        "sqlx error text must not reach the caller: {body}"
    );
}

// ---------------------------------------------------------------------------
// BEOPS-27: GET /schedules/{id}/history answered a failed schedule lookup with
// the raw sqlx text. It is a generic message now; the error goes to the log.
// ---------------------------------------------------------------------------
#[tokio::test]
async fn w3_schedule_history_does_not_leak_database_errors() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use axum::extract::{Extension, Path, Query, State};
    use spooled_backend::api::handlers::schedules::{history, HistoryQuery};
    use spooled_backend::models::ApiKeyContext;

    let db = TestDatabase::new().await;
    let tenant = seed_tenant(db.pool()).await;
    let schedule_id = uuid::Uuid::new_v4().to_string();
    sqlx::query(
        "INSERT INTO schedules (id, organization_id, name, cron_expression, queue_name, payload_template, timezone, priority, max_retries, timeout_seconds, is_active, created_at, updated_at) \
         VALUES ($1, $2, 'history', '0 * * * * *', 'default', '{}'::JSONB, 'UTC', 0, 3, 300, TRUE, NOW(), NOW())",
    )
    .bind(&schedule_id)
    .bind(&tenant.org_id)
    .execute(db.pool())
    .await
    .expect("seed schedule");

    // A pool that serves the handler's first query (the queue-scope check)
    // and then has no connection to give: every idle connection is refused
    // and a second one cannot be opened, so the history lookup fails with a
    // pool timeout.
    let connects = Arc::new(AtomicUsize::new(0));
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .min_connections(0)
        .acquire_timeout(std::time::Duration::from_secs(1))
        .after_connect(move |_, _| {
            let first = connects.fetch_add(1, Ordering::SeqCst) == 0;
            Box::pin(async move {
                if first {
                    Ok(())
                } else {
                    Err(sqlx::Error::Protocol("no second connection".to_string()))
                }
            })
        })
        .before_acquire(|_, _| Box::pin(async { Ok(false) }))
        .connect_lazy_with((*db.pool.connect_options()).clone());
    let state = AppState::new(
        Database::from_pool(Arc::new(pool)),
        None,
        Metrics::new(),
        Settings::load_for_testing(),
    );
    let ctx = ApiKeyContext {
        api_key_id: "key".to_string(),
        organization_id: tenant.org_id.clone(),
        queues: Vec::new(),
        rate_limit: None,
    };

    let (status, message) = history(
        State(state),
        Extension(ctx),
        Path(schedule_id),
        Query(HistoryQuery { limit: None }),
    )
    .await
    .expect_err("the history lookup has no connection");
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR, "{message}");
    assert_eq!(
        message, "Failed to get schedule history",
        "sqlx error text must not reach the caller"
    );
}

// ---------------------------------------------------------------------------
// BEGRPC-01: gRPC auth shares the REST key cache. A repeat call inside the TTL
// skips bcrypt and the last_used write; a REST revoke ends it.
// ---------------------------------------------------------------------------
fn grpc_request(key: &str) -> tonic::Request<()> {
    let mut request = tonic::Request::new(());
    request
        .metadata_mut()
        .insert("x-api-key", key.parse().unwrap());
    request
}

async fn last_used_is_set(pool: &sqlx::PgPool, key_id: &str) -> bool {
    let (set,): (bool,) =
        sqlx::query_as("SELECT last_used IS NOT NULL FROM api_keys WHERE id = $1")
            .bind(key_id)
            .fetch_one(pool)
            .await
            .unwrap();
    set
}

#[tokio::test]
async fn w3_auth_grpc_reuses_the_rest_key_cache_and_debounces_last_used() {
    use spooled_backend::grpc::auth::authenticate_request_with_cache;

    let db = TestDatabase::new().await;
    let tenant = seed_tenant(db.pool()).await;
    let (app, cache, _redis) = app_with_redis(&db).await;
    let (key_id,): (String,) = sqlx::query_as("SELECT id FROM api_keys WHERE organization_id = $1")
        .bind(&tenant.org_id)
        .fetch_one(db.pool())
        .await
        .unwrap();

    let ctx = authenticate_request_with_cache(db.pool(), Some(&cache), &grpc_request(&tenant.key))
        .await
        .expect("first gRPC call authenticates");
    assert_eq!(ctx.api_key_id, key_id);
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    while !last_used_is_set(db.pool(), &key_id).await {
        assert!(
            tokio::time::Instant::now() < deadline,
            "first call records last_used"
        );
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }

    // Break the stored hash (keeping the prefix the revoke path keys its
    // invalidation on) and clear last_used: a call that ran bcrypt would fail,
    // a call that wrote last_used would set it again.
    sqlx::query(
        "UPDATE api_keys SET key_hash = substr(key_hash, 1, 16) || repeat('x', 44), last_used = NULL WHERE id = $1",
    )
    .bind(&key_id)
    .execute(db.pool())
    .await
    .unwrap();
    for _ in 0..3 {
        authenticate_request_with_cache(db.pool(), Some(&cache), &grpc_request(&tenant.key))
            .await
            .expect("a cached key authenticates without bcrypt");
    }
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    assert!(
        !last_used_is_set(db.pool(), &key_id).await,
        "last_used must not be written on every call"
    );

    // Revoking through REST invalidates the shared cache entry.
    let (status, body) = call(
        &app,
        "DELETE",
        &format!("/api/v1/api-keys/{key_id}"),
        &[("Authorization", &bearer(&tenant.key))],
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    let err = authenticate_request_with_cache(db.pool(), Some(&cache), &grpc_request(&tenant.key))
        .await
        .expect_err("a revoked key must not authenticate over gRPC");
    assert_eq!(err.code(), tonic::Code::Unauthenticated);
}

#[tokio::test]
async fn w3_auth_grpc_negative_cache_short_circuits_a_repeated_unknown_token() {
    use spooled_backend::grpc::auth::authenticate_request_with_cache;

    let db = TestDatabase::new().await;
    let tenant = seed_tenant(db.pool()).await;
    let (_app, cache, _redis) = app_with_redis(&db).await;
    let token = format!("sp_test_{}", uuid::Uuid::new_v4().simple());

    let err = authenticate_request_with_cache(db.pool(), Some(&cache), &grpc_request(&token))
        .await
        .expect_err("unknown token");
    assert_eq!(err.code(), tonic::Code::Unauthenticated);

    insert_key(db.pool(), &tenant.org_id, &token, false).await;
    let err = authenticate_request_with_cache(db.pool(), Some(&cache), &grpc_request(&token))
        .await
        .expect_err("a repeated unknown token is answered from the negative cache");
    assert_eq!(err.code(), tonic::Code::Unauthenticated);
}

// ---------------------------------------------------------------------------
// BEAUTH-05: /auth/login resolves keys through the middleware's path, so it
// shares the failed-auth budget: a legacy key is not scanned for an address
// over budget, while a key found by lookup_hash still logs in from it.
// ---------------------------------------------------------------------------
#[tokio::test]
async fn w3_auth_login_shares_the_failed_auth_budget() {
    let db = TestDatabase::new().await;
    let tenant = seed_tenant(db.pool()).await;
    let legacy = format!("sp_test_{}", uuid::Uuid::new_v4().simple());
    insert_key(db.pool(), &tenant.org_id, &legacy, true).await;
    let (app, _cache, _redis) = app_with_redis(&db).await;
    let ip = unique_client_ip();

    exhaust_failed_auth_budget(&app, &ip).await;

    let login = |key: String| {
        let app = app.clone();
        let ip = ip.clone();
        async move {
            call(
                &app,
                "POST",
                "/api/v1/auth/login",
                &[("CF-Connecting-IP", &ip)],
                Some(json!({ "api_key": key })),
            )
            .await
        }
    };

    let (status, body) = login(legacy).await;
    assert_eq!(
        status,
        StatusCode::UNAUTHORIZED,
        "login must not run the legacy scan for an address over budget: {body}"
    );
    let (status, body) = login(tenant.key.clone()).await;
    assert_eq!(status, StatusCode::OK, "{body}");
}

// ---------------------------------------------------------------------------
// W3R-06: gRPC had no failed-auth budget, so a flood of unknown gRPC tokens
// ran the legacy scan without end. All gRPC callers now share one budget
// (FAILED_AUTH_GRPC_LIMIT per minute) that, like the REST per-IP one, skips
// only the legacy scan and leaves no negative cache behind.
// ---------------------------------------------------------------------------
#[tokio::test]
async fn w3_auth_grpc_failed_auth_budget_skips_the_legacy_scan() {
    use spooled_backend::api::middleware::auth::{FAILED_AUTH_GRPC_KEY, FAILED_AUTH_GRPC_LIMIT};
    use spooled_backend::grpc::auth::authenticate_request_with_cache;

    let db = TestDatabase::new().await;
    let tenant = seed_tenant(db.pool()).await;
    let legacy = format!("sp_test_{}", uuid::Uuid::new_v4().simple());
    insert_key(db.pool(), &tenant.org_id, &legacy, true).await;
    let (app, cache, _redis) = app_with_redis_db(&db, Some(13)).await;
    cache.delete(FAILED_AUTH_GRPC_KEY).await.unwrap();

    for _ in 0..=FAILED_AUTH_GRPC_LIMIT {
        let bogus = format!("sp_test_{}", uuid::Uuid::new_v4().simple());
        let err = authenticate_request_with_cache(db.pool(), Some(&cache), &grpc_request(&bogus))
            .await
            .expect_err("unknown token");
        assert_eq!(err.code(), tonic::Code::Unauthenticated);
    }

    let err = authenticate_request_with_cache(db.pool(), Some(&cache), &grpc_request(&legacy))
        .await
        .expect_err("with the gRPC budget spent the legacy scan must not run");
    assert_eq!(err.code(), tonic::Code::Unauthenticated);
    let miss_key = format!(
        "auth_miss:{}",
        spooled_backend::models::api_key_lookup_hash(&legacy)
    );
    assert!(
        !cache.exists(&miss_key).await.unwrap(),
        "a skipped scan must not negatively cache the token"
    );

    // Keys found by lookup_hash are not gated.
    authenticate_request_with_cache(db.pool(), Some(&cache), &grpc_request(&tenant.key))
        .await
        .expect("a lookup_hash key authenticates while the gRPC budget is spent");

    // The gRPC budget is not REST's: the legacy key works over REST.
    let ip = unique_client_ip();
    let (status, body) = call(
        &app,
        "GET",
        "/api/v1/jobs",
        &[
            ("Authorization", &bearer(&legacy)),
            ("CF-Connecting-IP", &ip),
        ],
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
}

/// [`call`], with the response headers.
async fn call_with_headers(
    app: &Router,
    method: &str,
    uri: &str,
    headers: &[(&str, &str)],
    body: Option<Value>,
) -> (StatusCode, axum::http::HeaderMap, Value) {
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
    let response_headers = res.headers().clone();
    let bytes = axum::body::to_bytes(res.into_body(), 1 << 20)
        .await
        .unwrap();
    let json = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes)
            .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into()))
    };
    (status, response_headers, json)
}

/// Insert legacy (`lookup_hash IS NULL`) keys under `prefix` at the production
/// bcrypt cost, as many as a scan of about `scan` takes on this host (at least
/// `min`). No token matches them. Returns the time one verify took here.
async fn insert_decoys(
    pool: &sqlx::PgPool,
    org_id: &str,
    prefix: &str,
    scan: std::time::Duration,
    min: usize,
) -> std::time::Duration {
    let decoy_hash = bcrypt::hash("sp_decoy_never", 12).expect("hash decoy");
    // The fastest of three: a scan under load only takes longer.
    let mut one_verify = std::time::Duration::MAX;
    for _ in 0..3 {
        let hash = decoy_hash.clone();
        let took = tokio::task::spawn_blocking(move || {
            let started = std::time::Instant::now();
            let _ = bcrypt::verify("sp_decoy_other", &hash);
            started.elapsed()
        })
        .await
        .unwrap();
        one_verify = one_verify.min(took);
    }
    let decoys = ((scan.as_millis() / one_verify.as_millis().max(1)) as usize).clamp(min, 100);
    // `key_hash` is unique: vary the first two digest characters. A verify
    // against each still costs the full bcrypt work and never matches.
    const BCRYPT_B64: &[u8] = b"./ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
    let hashes: Vec<String> = (1..=decoys)
        .map(|i| {
            let mut hash = decoy_hash.clone().into_bytes();
            hash[29] = BCRYPT_B64[i / 64];
            hash[30] = BCRYPT_B64[i % 64];
            String::from_utf8(hash).unwrap()
        })
        .filter(|hash| *hash != decoy_hash)
        .collect();
    sqlx::query(
        "INSERT INTO api_keys (id, organization_id, key_hash, key_prefix, lookup_hash, name, queues, is_active, created_at) \
         SELECT gen_random_uuid()::TEXT, $1, h, $3, NULL, 'decoy', ARRAY[]::TEXT[], TRUE, NOW() \
         FROM UNNEST($2::TEXT[]) AS h",
    )
    .bind(org_id)
    .bind(&hashes)
    .bind(prefix)
    .execute(pool)
    .await
    .expect("insert decoys");
    one_verify
}

/// A REST call with `token` from `ip`, run as its own task.
fn spawn_rest_call(
    app: &Router,
    token: &str,
    ip: &str,
) -> tokio::task::JoinHandle<(StatusCode, axum::http::HeaderMap, Value)> {
    let app = app.clone();
    let auth = bearer(token);
    let ip = ip.to_string();
    tokio::spawn(async move {
        call_with_headers(
            &app,
            "GET",
            "/api/v1/jobs",
            &[("Authorization", &auth), ("CF-Connecting-IP", &ip)],
            None,
        )
        .await
    })
}

fn unknown_token(prefix: &str) -> String {
    format!("{prefix}{}", uuid::Uuid::new_v4().simple())
}

fn miss_key(token: &str) -> String {
    format!(
        "auth_miss:{}",
        spooled_backend::models::api_key_lookup_hash(token)
    )
}

// ---------------------------------------------------------------------------
// W3R-08 (W3R-06): REST and gRPC legacy scans take separate turns. Unknown
// gRPC tokens (one budget for every gRPC caller) held the single process-wide
// scan turn, so REST legacy-key holders waited behind them and were refused
// with a 401; now a transport's scans can delay only its own legacy keys.
// ---------------------------------------------------------------------------
#[tokio::test]
async fn w3_auth_legacy_scans_take_separate_turns_per_transport() {
    use std::time::Duration;

    use spooled_backend::grpc::auth::{authenticate_request, authenticate_request_with_cache};

    let db = TestDatabase::new().await;
    let tenant = seed_tenant(db.pool()).await;
    let rest_legacy = unknown_token("sp_test_");
    let grpc_legacy = unknown_token("sp_test_");
    insert_key(db.pool(), &tenant.org_id, &rest_legacy, true).await;
    insert_key(db.pool(), &tenant.org_id, &grpc_legacy, true).await;
    // Unknown `sp_hold_` tokens scan for about 3 s.
    insert_decoys(
        db.pool(),
        &tenant.org_id,
        "sp_hold_",
        Duration::from_secs(3),
        10,
    )
    .await;
    let (app, cache, _redis) = app_with_redis(&db).await;

    // An unknown gRPC token holds the gRPC turn for its whole scan...
    let grpc_holder = {
        let pool = Arc::clone(&db.pool);
        tokio::spawn(async move {
            authenticate_request(&pool, &grpc_request(&unknown_token("sp_hold_"))).await
        })
    };
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(!grpc_holder.is_finished(), "the decoy scan ended too early");

    // ...and a REST legacy key does not wait for it.
    let started = std::time::Instant::now();
    let (status, body) = call(
        &app,
        "GET",
        "/api/v1/jobs",
        &[
            ("Authorization", &bearer(&rest_legacy)),
            ("CF-Connecting-IP", &unique_client_ip()),
        ],
        None,
    )
    .await;
    let waited = started.elapsed();
    assert_eq!(
        status,
        StatusCode::OK,
        "a REST legacy key must not queue behind a gRPC scan (answered after {waited:?}): {body}"
    );
    assert!(
        !grpc_holder.is_finished(),
        "the REST legacy key waited for the gRPC scan ({waited:?})"
    );
    let err = grpc_holder
        .await
        .unwrap()
        .expect_err("the holder's token is unknown");
    assert_eq!(err.code(), tonic::Code::Unauthenticated);

    // The other way round: an unknown REST token holds the REST turn, and a
    // gRPC legacy key does not wait for it.
    let rest_holder = spawn_rest_call(&app, &unknown_token("sp_hold_"), &unique_client_ip());
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(!rest_holder.is_finished(), "the decoy scan ended too early");
    authenticate_request_with_cache(db.pool(), Some(&cache), &grpc_request(&grpc_legacy))
        .await
        .expect("a gRPC legacy key must not queue behind a REST scan");
    assert!(
        !rest_holder.is_finished(),
        "the gRPC legacy key waited for the REST scan"
    );
    let (status, _, body) = rest_holder.await.unwrap();
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");
}

// ---------------------------------------------------------------------------
// W3R-07: a scan of ten production legacy keys takes longer than the flat 2 s
// a request waited for the scan turn, so concurrent requests for one dormant
// legacy key were refused with 401 (SDKs do not retry it) and charged as
// failed authentications, with no attacker present. They now join the first
// request's scan and share its result: all authenticate, only the scanning
// request is charged, and that charge is refunded because the key was found.
// ---------------------------------------------------------------------------
#[tokio::test]
async fn w3_auth_concurrent_requests_for_a_dormant_legacy_key_share_one_scan() {
    use std::time::Duration;

    let db = TestDatabase::new().await;
    let tenant = seed_tenant(db.pool()).await;
    // The key's own prefix holds about 3 s of production-cost legacy keys,
    // inserted first so the scan reaches the key last.
    insert_decoys(
        db.pool(),
        &tenant.org_id,
        "sp_test_",
        Duration::from_secs(3),
        10,
    )
    .await;
    let legacy = unknown_token("sp_test_");
    let key_id = insert_key(db.pool(), &tenant.org_id, &legacy, true).await;
    let (app, cache, _redis) = app_with_redis(&db).await;
    let ip = unique_client_ip();
    let budget_key = format!("auth_fail:{ip}");

    let requests: Vec<_> = (0..6)
        .map(|_| spawn_rest_call(&app, &legacy, &ip))
        .collect();
    tokio::time::sleep(Duration::from_millis(1000)).await;
    assert!(
        requests.iter().all(|request| !request.is_finished()),
        "the decoy scan ended too early"
    );
    let charged_during_the_scan = cache.get(&budget_key).await.unwrap();

    for request in requests {
        let (status, _, body) = request.await.unwrap();
        assert_eq!(
            status,
            StatusCode::OK,
            "a valid legacy key must authenticate under concurrency: {body}"
        );
    }
    assert_eq!(
        charged_during_the_scan.as_deref(),
        Some("1"),
        "concurrent requests for one token are one scan, charged once"
    );
    assert_eq!(
        cache.get(&budget_key).await.unwrap(),
        None,
        "a scan that found the key is not a failed authentication"
    );
    assert!(!cache.exists(&miss_key(&legacy)).await.unwrap());
    let (backfilled,): (bool,) =
        sqlx::query_as("SELECT lookup_hash IS NOT NULL FROM api_keys WHERE id = $1")
            .bind(&key_id)
            .fetch_one(db.pool())
            .await
            .unwrap();
    assert!(backfilled, "the key takes the fast path from now on");
}

// ---------------------------------------------------------------------------
// W3R-07: a request that cannot get its lane's turn waits about one scan (not
// a flat 2 s) and is then a 503 with Retry-After, on the API and on
// /auth/login: never a 401, never negatively cached, never charged to the
// failed-auth budget. Once the turn is free the key authenticates.
// ---------------------------------------------------------------------------
#[tokio::test]
async fn w3_auth_legacy_key_behind_a_busy_scan_turn_is_503_with_retry_after() {
    use std::time::{Duration, Instant};

    use spooled_backend::api::middleware::auth::AUTH_RETRY_AFTER_SECS;
    use spooled_backend::grpc::auth::authenticate_request;

    let db = TestDatabase::new().await;
    let tenant = seed_tenant(db.pool()).await;
    let legacy = unknown_token("sp_test_");
    insert_key(db.pool(), &tenant.org_id, &legacy, true).await;
    let one_verify = insert_decoys(
        db.pool(),
        &tenant.org_id,
        "sp_test_",
        Duration::from_secs(3),
        10,
    )
    .await;
    let (app, cache, _redis) = app_with_redis(&db).await;

    // The wait is computed from the measured verify speed: measure it first
    // with a few production-cost keys found by lookup_hash (one verify each).
    let modern = unknown_token("sp_warm_");
    insert_key_with_cost(db.pool(), &tenant.org_id, &modern, false, 12).await;
    for _ in 0..4 {
        authenticate_request(db.pool(), &grpc_request(&modern))
            .await
            .expect("a modern key authenticates");
    }

    // Two unknown tokens take the REST turn one after the other, so a third
    // request would wait for two scans.
    let first = spawn_rest_call(&app, &unknown_token("sp_test_"), &unique_client_ip());
    tokio::time::sleep(Duration::from_millis(300)).await;
    let second = spawn_rest_call(&app, &unknown_token("sp_test_"), &unique_client_ip());
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(!first.is_finished(), "the decoy scan ended too early");

    let ip = unique_client_ip();
    let started = Instant::now();
    let request = spawn_rest_call(&app, &legacy, &ip);
    tokio::time::sleep(Duration::from_millis(100)).await;
    // A login for the same key joins that request's scan.
    let login = {
        let app = app.clone();
        let legacy = legacy.clone();
        let ip = ip.clone();
        tokio::spawn(async move {
            call_with_headers(
                &app,
                "POST",
                "/api/v1/auth/login",
                &[("CF-Connecting-IP", &ip)],
                Some(json!({ "api_key": legacy })),
            )
            .await
        })
    };

    let (status, headers, body) = request.await.unwrap();
    let waited = started.elapsed();
    assert_eq!(
        status,
        StatusCode::SERVICE_UNAVAILABLE,
        "a key the scan never checked is not invalid (after {waited:?}): {body}"
    );
    assert_eq!(body["code"], "SERVICE_UNAVAILABLE", "{body}");
    assert_eq!(
        headers
            .get(axum::http::header::RETRY_AFTER)
            .and_then(|v| v.to_str().ok()),
        Some(AUTH_RETRY_AFTER_SECS.to_string().as_str())
    );
    let scan = one_verify * 10;
    assert!(
        waited >= Duration::from_millis(2500) && waited >= scan,
        "waited {waited:?}, less than one scan ({scan:?})"
    );
    assert!(waited < Duration::from_secs(12), "waited {waited:?}");

    let (status, headers, body) = login.await.unwrap();
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    assert!(headers.contains_key(axum::http::header::RETRY_AFTER));

    assert!(
        !cache.exists(&miss_key(&legacy)).await.unwrap(),
        "a token the scan never checked must not be negatively cached"
    );
    assert_eq!(
        cache.get(&format!("auth_fail:{ip}")).await.unwrap(),
        None,
        "a scan that never ran is not a failed authentication"
    );

    for holder in [first, second] {
        let (status, _, body) = holder.await.unwrap();
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");
    }

    // With the turn free again the legacy key authenticates.
    let (status, _, body) = spawn_rest_call(&app, &legacy, &ip).await.unwrap();
    assert_eq!(status, StatusCode::OK, "{body}");
}

// ---------------------------------------------------------------------------
// W3R-08: the failed-auth budgets are charged BEFORE the legacy scan (INCR,
// then compare). Charged after it, concurrent unknown tokens all read the
// counter before any was charged and all scanned, and a request cancelled
// mid-scan was never charged. A scan that finds its key is refunded.
// ---------------------------------------------------------------------------
#[tokio::test]
async fn w3_auth_failed_auth_budget_is_charged_before_the_scan() {
    use std::time::Duration;

    use spooled_backend::api::middleware::auth::{
        FAILED_AUTH_GRPC_KEY, FAILED_AUTH_GRPC_LIMIT, FAILED_AUTH_PER_IP_LIMIT,
    };
    use spooled_backend::grpc::auth::authenticate_request_with_cache;

    let db = TestDatabase::new().await;
    let tenant = seed_tenant(db.pool()).await;
    // Unknown `sp_test_` tokens have a (cheap) legacy key to scan.
    let legacy = unknown_token("sp_test_");
    insert_key(db.pool(), &tenant.org_id, &legacy, true).await;
    // Unknown `sp_slow_` tokens scan for about a second.
    insert_decoys(
        db.pool(),
        &tenant.org_id,
        "sp_slow_",
        Duration::from_secs(1),
        4,
    )
    .await;
    // Its own Redis database: this test spends the gRPC budget.
    let (app, cache, _redis) = app_with_redis_db(&db, Some(12)).await;
    cache.delete(FAILED_AUTH_GRPC_KEY).await.unwrap();

    async fn negatively_cached(
        cache: &spooled_backend::cache::RedisCache,
        tokens: &[String],
    ) -> usize {
        let mut cached = 0;
        for token in tokens {
            if cache.exists(&miss_key(token)).await.unwrap() {
                cached += 1;
            }
        }
        cached
    }

    // Concurrent unknown tokens from one address: exactly the budget scans.
    let ip = unique_client_ip();
    let tokens: Vec<String> = (0..FAILED_AUTH_PER_IP_LIMIT + 5)
        .map(|_| unknown_token("sp_test_"))
        .collect();
    let requests: Vec<_> = tokens
        .iter()
        .map(|token| spawn_rest_call(&app, token, &ip))
        .collect();
    for request in requests {
        let (status, _, body) = request.await.unwrap();
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");
    }
    assert_eq!(
        negatively_cached(&cache, &tokens).await,
        FAILED_AUTH_PER_IP_LIMIT as usize,
        "only the budget's worth of concurrent unknown tokens may scan"
    );
    assert_eq!(
        cache.get(&format!("auth_fail:{ip}")).await.unwrap(),
        Some((FAILED_AUTH_PER_IP_LIMIT + 5).to_string())
    );

    // The same for the one gRPC budget.
    let grpc_tokens: Vec<String> = (0..FAILED_AUTH_GRPC_LIMIT + 5)
        .map(|_| unknown_token("sp_test_"))
        .collect();
    let calls: Vec<_> = grpc_tokens
        .iter()
        .map(|token| {
            let pool = Arc::clone(&db.pool);
            let cache = cache.clone();
            let token = token.clone();
            tokio::spawn(async move {
                authenticate_request_with_cache(&pool, Some(&cache), &grpc_request(&token)).await
            })
        })
        .collect();
    for call in calls {
        let err = call.await.unwrap().expect_err("unknown token");
        assert_eq!(err.code(), tonic::Code::Unauthenticated);
    }
    assert_eq!(
        negatively_cached(&cache, &grpc_tokens).await,
        FAILED_AUTH_GRPC_LIMIT as usize,
        "only the gRPC budget's worth of concurrent unknown tokens may scan"
    );

    // A request cancelled mid-scan has already paid.
    let ip = unique_client_ip();
    let cancelled = spawn_rest_call(&app, &unknown_token("sp_slow_"), &ip);
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(!cancelled.is_finished(), "the decoy scan ended too early");
    cancelled.abort();
    let _ = cancelled.await;
    assert_eq!(
        cache
            .get(&format!("auth_fail:{ip}"))
            .await
            .unwrap()
            .as_deref(),
        Some("1"),
        "a request cancelled mid-scan must stay charged"
    );

    // A scan that finds its key is refunded: the address keeps its count.
    let ip = unique_client_ip();
    for _ in 0..3 {
        let (status, _, body) = spawn_rest_call(&app, &unknown_token("sp_none_"), &ip)
            .await
            .unwrap();
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");
    }
    let (status, _, body) = spawn_rest_call(&app, &legacy, &ip).await.unwrap();
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        cache
            .get(&format!("auth_fail:{ip}"))
            .await
            .unwrap()
            .as_deref(),
        Some("3")
    );
}
// ---- wave 3 / grpcw ----

fn grpcw_request<T>(raw_key: &str, body: T) -> tonic::Request<T> {
    let mut request = tonic::Request::new(body);
    request
        .metadata_mut()
        .insert("x-api-key", raw_key.parse().unwrap());
    request
}

fn grpcw_worker_settings() -> spooled_backend::config::WorkerSettings {
    spooled_backend::config::WorkerSettings {
        heartbeat_interval_secs: 10,
        lease_duration_secs: 30,
        grpc_default_lease_duration_secs: 300,
        max_concurrency: 10,
        fallback_poll_interval_secs: 5,
    }
}

fn grpcw_worker_service_with(
    db: &TestDatabase,
    cache: Option<spooled_backend::cache::RedisCache>,
    rate_limit_fail_closed: bool,
    worker_settings: spooled_backend::config::WorkerSettings,
) -> (
    spooled_backend::grpc::services::WorkerServiceImpl,
    Arc<Metrics>,
) {
    use spooled_backend::outgoing_webhooks::service::OutgoingWebhookService;

    let metrics = Arc::new(Metrics::new());
    let service = spooled_backend::grpc::services::WorkerServiceImpl::new(
        Arc::clone(&db.pool),
        Arc::clone(&metrics),
        cache,
        Arc::new(OutgoingWebhookService::new((*db.pool).clone(), 3, 4)),
        rate_limit_fail_closed,
        worker_settings,
    );
    (service, metrics)
}

fn grpcw_worker_service(
    db: &TestDatabase,
) -> (
    spooled_backend::grpc::services::WorkerServiceImpl,
    Arc<Metrics>,
) {
    grpcw_worker_service_with(db, None, false, grpcw_worker_settings())
}

fn grpcw_register(
    queue: &str,
    worker_id: Option<&str>,
) -> spooled_backend::grpc::proto::RegisterWorkerRequest {
    spooled_backend::grpc::proto::RegisterWorkerRequest {
        queue_name: queue.to_string(),
        hostname: "grpcw-host".to_string(),
        worker_type: String::new(),
        max_concurrency: 0,
        version: String::new(),
        metadata: Default::default(),
        worker_id: worker_id.map(str::to_string),
    }
}

fn grpcw_heartbeat(
    worker_id: &str,
    status: &str,
) -> spooled_backend::grpc::proto::HeartbeatRequest {
    spooled_backend::grpc::proto::HeartbeatRequest {
        worker_id: worker_id.to_string(),
        current_jobs: 0,
        status: status.to_string(),
        metadata: Default::default(),
    }
}

async fn grpcw_set_worker_cap(pool: &sqlx::PgPool, org_id: &str, cap: i32) {
    sqlx::query(
        "UPDATE organizations SET custom_limits = jsonb_build_object('max_workers', $2::INT) WHERE id = $1",
    )
    .bind(org_id)
    .bind(cap)
    .execute(pool)
    .await
    .unwrap();
}

/// Workers the plan cap sees, from the same function the limit check uses.
async fn grpcw_counted_workers(pool: &sqlx::PgPool, org_id: &str) -> i64 {
    let (workers,): (i64,) = sqlx::query_as("SELECT workers FROM get_org_resource_counts($1)")
        .bind(org_id)
        .fetch_one(pool)
        .await
        .unwrap();
    workers
}

// BEGRPC-06: a heartbeat without metadata keeps the stored metadata.
#[tokio::test]
async fn grpcw_heartbeat_without_metadata_keeps_worker_metadata() {
    use spooled_backend::grpc::proto::worker_service_server::WorkerService;

    let db = TestDatabase::new().await;
    let tenant = seed_tenant(db.pool()).await;
    let (service, _) = grpcw_worker_service(&db);

    let mut register = grpcw_register("mq", None);
    register
        .metadata
        .insert("region".to_string(), "eu".to_string());
    let worker_id = service
        .register(grpcw_request(&tenant.key, register))
        .await
        .unwrap()
        .into_inner()
        .worker_id;

    let mut beat = grpcw_heartbeat(&worker_id, "healthy");
    beat.current_jobs = 2;
    let ack = service
        .heartbeat(grpcw_request(&tenant.key, beat))
        .await
        .unwrap()
        .into_inner();
    assert!(ack.acknowledged);
    let (metadata,): (Value,) = sqlx::query_as("SELECT metadata FROM workers WHERE id = $1")
        .bind(&worker_id)
        .fetch_one(db.pool())
        .await
        .unwrap();
    assert_eq!(metadata, json!({"region": "eu"}));

    // Metadata that is sent replaces the stored value, as before.
    let mut beat = grpcw_heartbeat(&worker_id, "healthy");
    beat.metadata.insert("build".to_string(), "abc".to_string());
    service
        .heartbeat(grpcw_request(&tenant.key, beat))
        .await
        .unwrap();
    let (metadata,): (Value,) = sqlx::query_as("SELECT metadata FROM workers WHERE id = $1")
        .bind(&worker_id)
        .fetch_one(db.pool())
        .await
        .unwrap();
    assert_eq!(metadata, json!({"build": "abc"}));
}

// BEGRPC-07: concurrent gRPC registrations cannot overshoot the worker cap.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn grpcw_concurrent_registers_never_exceed_the_worker_cap() {
    use spooled_backend::grpc::proto::worker_service_server::WorkerService;

    use sqlx::Connection;

    let db = TestDatabase::new().await;
    let tenant = seed_tenant(db.pool()).await;
    grpcw_set_worker_cap(db.pool(), &tenant.org_id, 1).await;
    let (service, metrics) = grpcw_worker_service(&db);

    // Hold back every INSERT into workers (reads still pass) until all four
    // registrations are queued, so they really race for the last slot.
    let options = db.pool.connect_options();
    let mut gate_conn = sqlx::PgConnection::connect_with(&options).await.unwrap();
    let mut probe = sqlx::PgConnection::connect_with(&options).await.unwrap();
    let mut gate = gate_conn.begin().await.unwrap();
    sqlx::query("LOCK TABLE workers IN SHARE MODE")
        .execute(&mut *gate)
        .await
        .unwrap();

    let mut tasks = Vec::new();
    for _ in 0..4 {
        let service = service.clone();
        let key = tenant.key.clone();
        tasks.push(tokio::spawn(async move {
            service
                .register(grpcw_request(&key, grpcw_register("cq", None)))
                .await
        }));
    }
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    loop {
        let (waiting,): (i64,) = sqlx::query_as(
            "SELECT COUNT(*) FROM pg_locks WHERE NOT granted \
             AND database = (SELECT oid FROM pg_database WHERE datname = current_database())",
        )
        .fetch_one(&mut probe)
        .await
        .unwrap();
        if waiting >= 4 {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "registrations never queued up ({waiting} waiting)"
        );
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    gate.rollback().await.unwrap();

    let mut registered = 0;
    for task in tasks {
        match task.await.unwrap() {
            Ok(_) => registered += 1,
            Err(status) => assert_eq!(status.code(), tonic::Code::ResourceExhausted, "{status:?}"),
        }
    }
    assert_eq!(registered, 1);
    assert_eq!(grpcw_counted_workers(db.pool(), &tenant.org_id).await, 1);
    assert_eq!(metrics.workers_active.get(), 1);
}

// BEGRPC-08: re-registering a stable worker id at the cap updates that worker.
#[tokio::test]
async fn grpcw_reregistering_a_stable_worker_id_at_the_cap_counts_once() {
    use spooled_backend::grpc::proto::worker_service_server::WorkerService;

    let db = TestDatabase::new().await;
    let tenant = seed_tenant(db.pool()).await;
    grpcw_set_worker_cap(db.pool(), &tenant.org_id, 1).await;
    let (service, metrics) = grpcw_worker_service(&db);

    let first = service
        .register(grpcw_request(
            &tenant.key,
            grpcw_register("sq", Some("stable-worker-1")),
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(first.worker_id, "stable-worker-1");

    // The restarted process comes back with the same id and a new host.
    let mut again = grpcw_register("sq2", Some("stable-worker-1"));
    again.hostname = "grpcw-host-2".to_string();
    let second = service
        .register(grpcw_request(&tenant.key, again))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(second.worker_id, "stable-worker-1");

    let (rows, queue, hostname): (i64, String, String) = sqlx::query_as(
        "SELECT COUNT(*) OVER (), queue_name, hostname FROM workers WHERE organization_id = $1",
    )
    .bind(&tenant.org_id)
    .fetch_one(db.pool())
    .await
    .unwrap();
    assert_eq!(
        (rows, queue.as_str(), hostname.as_str()),
        (1, "sq2", "grpcw-host-2")
    );
    assert_eq!(grpcw_counted_workers(db.pool(), &tenant.org_id).await, 1);
    assert_eq!(metrics.workers_active.get(), 1);

    // The cap still holds for a different worker.
    let err = service
        .register(grpcw_request(&tenant.key, grpcw_register("sq", None)))
        .await
        .unwrap_err();
    assert_eq!(err.code(), tonic::Code::ResourceExhausted, "{err:?}");
}

// BEGRPC-08: a worker id is scoped to its organization.
#[tokio::test]
async fn grpcw_worker_id_of_another_org_cannot_be_taken_over() {
    use spooled_backend::grpc::proto::worker_service_server::WorkerService;
    use spooled_backend::grpc::proto::DeregisterRequest;

    let db = TestDatabase::new().await;
    let owner = seed_tenant(db.pool()).await;
    let other = seed_tenant(db.pool()).await;
    let (service, _) = grpcw_worker_service(&db);

    service
        .register(grpcw_request(
            &owner.key,
            grpcw_register("oq", Some("shared-worker-id")),
        ))
        .await
        .unwrap();

    let mut hijack = grpcw_register("evil", Some("shared-worker-id"));
    hijack.hostname = "evil-host".to_string();
    let err = service
        .register(grpcw_request(&other.key, hijack))
        .await
        .unwrap_err();
    assert_eq!(err.code(), tonic::Code::AlreadyExists, "{err:?}");

    let ack = service
        .heartbeat(grpcw_request(
            &other.key,
            grpcw_heartbeat("shared-worker-id", "draining"),
        ))
        .await
        .unwrap()
        .into_inner();
    assert!(!ack.acknowledged);
    let gone = service
        .deregister(grpcw_request(
            &other.key,
            DeregisterRequest {
                worker_id: "shared-worker-id".to_string(),
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert!(!gone.success);

    let (org, queue, hostname, status): (String, String, String, String) = sqlx::query_as(
        "SELECT organization_id, queue_name, hostname, status FROM workers WHERE id = 'shared-worker-id'",
    )
    .fetch_one(db.pool())
    .await
    .unwrap();
    assert_eq!(org, owner.org_id);
    assert_eq!(
        (queue.as_str(), hostname.as_str(), status.as_str()),
        ("oq", "grpcw-host", "healthy")
    );
    assert_eq!(grpcw_counted_workers(db.pool(), &other.org_id).await, 0);
}

// BEGRPC-09: gRPC Register/Heartbeat apply the REST input limits.
#[tokio::test]
async fn grpcw_register_and_heartbeat_apply_rest_input_limits() {
    use spooled_backend::grpc::proto::worker_service_server::WorkerService;

    let db = TestDatabase::new().await;
    let tenant = seed_tenant(db.pool()).await;
    let (service, _) = grpcw_worker_service(&db);

    let big = "x".repeat(70 * 1024);
    let mut cases = Vec::new();
    let mut req = grpcw_register("lq", None);
    req.metadata.insert("blob".to_string(), big.clone());
    cases.push(("metadata", req));
    let mut req = grpcw_register("lq", None);
    req.worker_type = "t".repeat(51);
    cases.push(("worker_type", req));
    let mut req = grpcw_register("lq", None);
    req.version = "v".repeat(51);
    cases.push(("version", req));
    let mut req = grpcw_register("lq", None);
    req.hostname = "h".repeat(256);
    cases.push(("hostname", req));
    cases.push(("worker_id chars", grpcw_register("lq", Some("bad id!"))));
    cases.push((
        "worker_id length",
        grpcw_register("lq", Some(&"w".repeat(129))),
    ));
    for (case, req) in cases {
        let err = service
            .register(grpcw_request(&tenant.key, req))
            .await
            .unwrap_err();
        assert_eq!(err.code(), tonic::Code::InvalidArgument, "{case}: {err:?}");
    }
    let (rows,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM workers WHERE organization_id = $1")
        .bind(&tenant.org_id)
        .fetch_one(db.pool())
        .await
        .unwrap();
    assert_eq!(rows, 0);

    // max_concurrency is capped at REST's 100 rather than rejected.
    let mut req = grpcw_register("lq", None);
    req.max_concurrency = 500;
    let worker_id = service
        .register(grpcw_request(&tenant.key, req))
        .await
        .unwrap()
        .into_inner()
        .worker_id;
    let (max_jobs,): (i32,) =
        sqlx::query_as("SELECT max_concurrent_jobs FROM workers WHERE id = $1")
            .bind(&worker_id)
            .fetch_one(db.pool())
            .await
            .unwrap();
    assert_eq!(max_jobs, 100);

    let mut beat = grpcw_heartbeat(&worker_id, "healthy");
    beat.metadata.insert("blob".to_string(), big);
    let err = service
        .heartbeat(grpcw_request(&tenant.key, beat))
        .await
        .unwrap_err();
    assert_eq!(err.code(), tonic::Code::InvalidArgument, "{err:?}");
    let (metadata,): (Value,) = sqlx::query_as("SELECT metadata FROM workers WHERE id = $1")
        .bind(&worker_id)
        .fetch_one(db.pool())
        .await
        .unwrap();
    assert_eq!(metadata, json!({}));
}

// BEGRPC-10 / W3R-03: Register reports the lease a gRPC claim without one
// gets (GRPC_DEFAULT_LEASE_DURATION_SECS, not the REST default) and the
// configured heartbeat interval.
#[tokio::test]
async fn grpcw_register_returns_the_grpc_default_lease_and_heartbeat_interval() {
    use spooled_backend::grpc::proto::worker_service_server::WorkerService;

    let db = TestDatabase::new().await;
    let tenant = seed_tenant(db.pool()).await;
    let response = grpcw_worker_service(&db)
        .0
        .register(grpcw_request(&tenant.key, grpcw_register("lease-q", None)))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(
        response.lease_duration_secs, 300,
        "Register must report the 300 s gRPC default, not WORKER_LEASE_DURATION_SECS (30)"
    );

    let mut settings = grpcw_worker_settings();
    settings.lease_duration_secs = 45;
    settings.grpc_default_lease_duration_secs = 600;
    settings.heartbeat_interval_secs = 7;
    let (service, _) = grpcw_worker_service_with(&db, None, false, settings);
    let response = service
        .register(grpcw_request(&tenant.key, grpcw_register("lease-q2", None)))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(response.lease_duration_secs, 600);
    assert_eq!(response.heartbeat_interval_secs, 7);
}

// BEGRPC-11: worker RPCs follow RATE_LIMIT_FAIL_CLOSED like the queue service.
#[tokio::test]
async fn grpcw_worker_rpcs_follow_the_rate_limit_fail_mode() {
    use spooled_backend::cache::RedisCache;
    use spooled_backend::config::RedisSettings;
    use spooled_backend::grpc::proto::worker_service_server::WorkerService;

    let db = TestDatabase::new().await;
    let tenant = seed_tenant(db.pool()).await;

    // No Redis and fail-closed: denied, as QueueService does.
    let (closed_no_redis, _) = grpcw_worker_service_with(&db, None, true, grpcw_worker_settings());
    let err = closed_no_redis
        .register(grpcw_request(&tenant.key, grpcw_register("rq", None)))
        .await
        .unwrap_err();
    assert_eq!(err.code(), tonic::Code::ResourceExhausted, "{err:?}");

    // A string under the org bucket key makes the token-bucket script fail
    // (WRONGTYPE): a Redis error on every worker RPC of this org.
    let redis = common::TestRedis::new().await;
    let cache = RedisCache::connect(&RedisSettings {
        url: redis.url.clone(),
        pool_size: 2,
    })
    .await
    .expect("TEST_REDIS_URL must point at a Redis");
    let bucket = format!("rate_limit:grpc:org:{}", tenant.org_id);
    cache.set(&bucket, "not-a-bucket", 120).await.unwrap();

    let (open, _) =
        grpcw_worker_service_with(&db, Some(cache.clone()), false, grpcw_worker_settings());
    let worker_id = open
        .register(grpcw_request(&tenant.key, grpcw_register("rq", None)))
        .await
        .expect("fail-open: a Redis error must not block Register")
        .into_inner()
        .worker_id;
    let ack = open
        .heartbeat(grpcw_request(
            &tenant.key,
            grpcw_heartbeat(&worker_id, "healthy"),
        ))
        .await
        .expect("fail-open: a Redis error must not block Heartbeat")
        .into_inner();
    assert!(ack.acknowledged);

    let (closed, _) =
        grpcw_worker_service_with(&db, Some(cache.clone()), true, grpcw_worker_settings());
    let err = closed
        .heartbeat(grpcw_request(
            &tenant.key,
            grpcw_heartbeat(&worker_id, "healthy"),
        ))
        .await
        .unwrap_err();
    assert_eq!(err.code(), tonic::Code::ResourceExhausted, "{err:?}");

    cache.delete(&bucket).await.unwrap();
}

// BEGRPC-15: draining workers hold a slot of the worker cap while alive.
#[tokio::test]
async fn grpcw_draining_workers_count_toward_the_worker_cap() {
    use spooled_backend::grpc::proto::worker_service_server::WorkerService;

    let db = TestDatabase::new().await;
    let tenant = seed_tenant(db.pool()).await;
    grpcw_set_worker_cap(db.pool(), &tenant.org_id, 1).await;
    let (service, _) = grpcw_worker_service(&db);
    sqlx::query(
        "INSERT INTO workers (id, organization_id, queue_name, queue_names, hostname, status, last_heartbeat) \
         VALUES ('draining-1', $1, 'dq', ARRAY['dq'], 'host', 'draining', NOW())",
    )
    .bind(&tenant.org_id)
    .execute(db.pool())
    .await
    .unwrap();
    assert_eq!(grpcw_counted_workers(db.pool(), &tenant.org_id).await, 1);

    let err = service
        .register(grpcw_request(&tenant.key, grpcw_register("dq", None)))
        .await
        .unwrap_err();
    assert_eq!(err.code(), tonic::Code::ResourceExhausted, "{err:?}");
    let (status, body) = call(
        &app(&db),
        "POST",
        "/api/v1/workers/register",
        &[("Authorization", &bearer(&tenant.key))],
        Some(json!({"queue_name": "dq", "hostname": "rest-host"})),
    )
    .await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "{body}");

    // The draining worker itself may re-register under its id, on both APIs.
    service
        .register(grpcw_request(
            &tenant.key,
            grpcw_register("dq", Some("draining-1")),
        ))
        .await
        .unwrap();
    sqlx::query("UPDATE workers SET status = 'draining' WHERE id = 'draining-1'")
        .execute(db.pool())
        .await
        .unwrap();
    let (status, body) = call(
        &app(&db),
        "POST",
        "/api/v1/workers/register",
        &[("Authorization", &bearer(&tenant.key))],
        Some(json!({"worker_id": "draining-1", "queue_name": "dq", "hostname": "rest-host"})),
    )
    .await;
    assert!(status.is_success(), "{body}");
    assert_eq!(grpcw_counted_workers(db.pool(), &tenant.org_id).await, 1);

    // A draining worker that stopped heartbeating gives its slot back.
    sqlx::query(
        "UPDATE workers SET status = 'draining', last_heartbeat = NOW() - INTERVAL '10 minutes' \
         WHERE id = 'draining-1'",
    )
    .execute(db.pool())
    .await
    .unwrap();
    assert_eq!(grpcw_counted_workers(db.pool(), &tenant.org_id).await, 0);
    service
        .register(grpcw_request(&tenant.key, grpcw_register("dq", None)))
        .await
        .unwrap();
}

// BEGRPC-25: an empty heartbeat status keeps the stored status, and
// deregistering twice moves the gauges once.
#[tokio::test]
async fn grpcw_heartbeat_without_status_keeps_it_and_deregister_counts_once() {
    use spooled_backend::grpc::proto::worker_service_server::WorkerService;
    use spooled_backend::grpc::proto::DeregisterRequest;

    async fn status_of(pool: &sqlx::PgPool, worker_id: &str) -> String {
        let (status,): (String,) = sqlx::query_as("SELECT status FROM workers WHERE id = $1")
            .bind(worker_id)
            .fetch_one(pool)
            .await
            .unwrap();
        status
    }

    let db = TestDatabase::new().await;
    let tenant = seed_tenant(db.pool()).await;
    let (service, metrics) = grpcw_worker_service(&db);
    let worker_id = service
        .register(grpcw_request(&tenant.key, grpcw_register("hq", None)))
        .await
        .unwrap()
        .into_inner()
        .worker_id;
    assert_eq!(
        (metrics.workers_active.get(), metrics.workers_healthy.get()),
        (1, 1)
    );

    service
        .heartbeat(grpcw_request(
            &tenant.key,
            grpcw_heartbeat(&worker_id, "degraded"),
        ))
        .await
        .unwrap();
    let ack = service
        .heartbeat(grpcw_request(&tenant.key, grpcw_heartbeat(&worker_id, "")))
        .await
        .unwrap()
        .into_inner();
    assert!(ack.acknowledged);
    assert_eq!(status_of(db.pool(), &worker_id).await, "degraded");
    service
        .heartbeat(grpcw_request(
            &tenant.key,
            grpcw_heartbeat(&worker_id, "healthy"),
        ))
        .await
        .unwrap();
    assert_eq!(status_of(db.pool(), &worker_id).await, "healthy");

    for _ in 0..2 {
        let done = service
            .deregister(grpcw_request(
                &tenant.key,
                DeregisterRequest {
                    worker_id: worker_id.clone(),
                },
            ))
            .await
            .unwrap()
            .into_inner();
        assert!(done.success);
        assert_eq!(
            (metrics.workers_active.get(), metrics.workers_healthy.get()),
            (0, 0)
        );
    }

    // A heartbeat proves the worker is alive: an offline worker that beats
    // without a status comes back as healthy (unchanged behaviour).
    service
        .heartbeat(grpcw_request(&tenant.key, grpcw_heartbeat(&worker_id, "")))
        .await
        .unwrap();
    assert_eq!(status_of(db.pool(), &worker_id).await, "healthy");
}
// ---- wave 3 / runtime ----
mod wave3_runtime {
    use super::*;

    use std::io::Read;
    use std::net::SocketAddr;
    use std::process::{Child, Command, ExitStatus, Stdio};
    use std::sync::Mutex;
    use std::time::{Duration, Instant};

    use spooled_backend::api::handlers::health::RuntimeHealth;
    use spooled_backend::grpc::proto::queue_service_client::QueueServiceClient;
    use spooled_backend::grpc::proto::GetQueueStatsRequest;
    use spooled_backend::grpc::server::{GrpcServer, GrpcServerDeps};
    use spooled_backend::outgoing_webhooks::service::OutgoingWebhookService;
    use sqlx::postgres::PgPoolOptions;
    use tokio::sync::watch;
    use tonic_health::pb::health_check_response::ServingStatus;
    use tonic_health::pb::health_client::HealthClient;
    use tonic_health::pb::HealthCheckRequest;

    /// A pool whose server never answers (nothing listens on port 1).
    fn unreachable_pool() -> sqlx::PgPool {
        PgPoolOptions::new()
            .max_connections(1)
            .acquire_timeout(Duration::from_millis(300))
            .connect_lazy("postgres://spooled:unused@127.0.0.1:1/unreachable")
            .unwrap()
    }

    // -----------------------------------------------------------------------
    // BEOPS-21: /health answers 503 (same body) when the database is down.
    // -----------------------------------------------------------------------
    #[tokio::test]
    async fn health_is_503_with_the_same_body_when_the_database_is_down() {
        let app = router(AppState::new(
            Database::from_pool(Arc::new(unreachable_pool())),
            None,
            Metrics::new(),
            Settings::load_for_testing(),
        ));
        let (status, body) = call(&app, "GET", "/health", &[], None).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
        assert_eq!(body["status"], "degraded");
        assert_eq!(body["database"], false);
        assert_eq!(body["cache"], true, "no Redis is configured");
    }

    // -----------------------------------------------------------------------
    // BEOPS-09: /health reports gRPC and scheduler liveness.
    // -----------------------------------------------------------------------
    #[tokio::test]
    async fn health_reports_grpc_and_scheduler_liveness() {
        let db = TestDatabase::new().await;
        let runtime = Arc::new(RuntimeHealth::default());
        let app = app(&db).layer(axum::Extension(Arc::clone(&runtime)));

        let (status, body) = call(&app, "GET", "/health", &[], None).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["grpc"], false, "{body}");
        assert_eq!(body["scheduler"], false, "{body}");
        assert_eq!(body["status"], "degraded");

        runtime.set_grpc_serving(true);
        runtime.scheduler_tick();
        let (status, body) = call(&app, "GET", "/health", &[], None).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["grpc"], true, "{body}");
        assert_eq!(body["scheduler"], true, "{body}");
        assert_eq!(body["database"], true);
        assert_eq!(body["status"], "healthy");

        // A stopped task degrades the report; only a database outage is a 503.
        runtime.scheduler_stopped();
        let (status, body) = call(&app, "GET", "/health", &[], None).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["scheduler"], false, "{body}");
        assert_eq!(body["status"], "degraded");
    }

    // -----------------------------------------------------------------------
    // BEOPS-22: dashboard uptime counts from process start, not first request.
    // -----------------------------------------------------------------------
    #[tokio::test]
    async fn dashboard_uptime_counts_from_process_start() {
        let db = TestDatabase::new().await;
        let tenant = seed_tenant(db.pool()).await;
        let mut state = AppState::new(
            Database::from_pool(Arc::clone(&db.pool)),
            None,
            Metrics::new(),
            Settings::load_for_testing(),
        );
        state.start_time = Instant::now()
            .checked_sub(Duration::from_secs(600))
            .expect("host up for ten minutes");
        let app = router(state);

        let (status, body) = call(
            &app,
            "GET",
            "/api/v1/dashboard",
            &[("Authorization", &bearer(&tenant.key))],
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let uptime = body["system"]["uptime_seconds"].as_u64().unwrap();
        assert!((600..660).contains(&uptime), "uptime_seconds = {uptime}");
        let started: chrono::DateTime<chrono::Utc> = body["system"]["started_at"]
            .as_str()
            .unwrap()
            .parse()
            .unwrap();
        let expected = chrono::Utc::now() - chrono::Duration::seconds(600);
        assert!(
            (started - expected).num_seconds().abs() <= 5,
            "started_at = {started}, expected about {expected}"
        );
    }

    // -----------------------------------------------------------------------
    // BEGRPC-14 / BEOPS-24 / BEGRPC-25: gRPC graceful shutdown and health.
    // -----------------------------------------------------------------------
    pub(super) fn grpc_deps(pool: Arc<sqlx::PgPool>) -> GrpcServerDeps {
        let settings = Settings::load_for_testing();
        GrpcServerDeps {
            db: Arc::new(Database::from_pool(Arc::clone(&pool))),
            metrics: Arc::new(Metrics::new()),
            cache: None,
            outgoing_webhooks: Arc::new(OutgoingWebhookService::new((*pool).clone(), 3, 4)),
            queue_defaults: settings.queue.clone(),
            rate_limit_fail_closed: false,
            worker_settings: settings.worker.clone(),
        }
    }

    pub(super) async fn grpc_channel(addr: SocketAddr) -> tonic::transport::Channel {
        tonic::transport::Endpoint::from_shared(format!("http://{addr}"))
            .unwrap()
            .connect()
            .await
            .unwrap()
    }

    pub(super) fn with_key<T>(raw_key: &str, body: T) -> tonic::Request<T> {
        let mut request = tonic::Request::new(body);
        request
            .metadata_mut()
            .insert("x-api-key", raw_key.parse().unwrap());
        request
    }

    fn health_request(service: &str) -> HealthCheckRequest {
        HealthCheckRequest {
            service: service.to_string(),
        }
    }

    #[tokio::test]
    async fn grpc_shutdown_waits_for_an_in_flight_call() {
        let db = TestDatabase::new().await;
        let tenant = seed_tenant(db.pool()).await;
        let server = GrpcServer::bind(
            "127.0.0.1:0".parse().unwrap(),
            grpc_deps(Arc::clone(&db.pool)),
        )
        .await
        .unwrap();
        let addr = server.local_addr();
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let serving = tokio::spawn(server.serve(shutdown_rx, Duration::from_secs(10), None));
        let mut client = QueueServiceClient::new(grpc_channel(addr).await);

        // Hold the jobs table so the call is still running when shutdown starts.
        let mut lock = db.pool().begin().await.unwrap();
        sqlx::query("LOCK TABLE jobs IN ACCESS EXCLUSIVE MODE")
            .execute(&mut *lock)
            .await
            .unwrap();
        let key = tenant.key.clone();
        let call = tokio::spawn(async move {
            client
                .get_queue_stats(with_key(
                    &key,
                    GetQueueStatsRequest {
                        queue_name: "drain".to_string(),
                    },
                ))
                .await
        });
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let (waiting,): (i64,) = sqlx::query_as(
                "SELECT COUNT(*) FROM pg_stat_activity \
                 WHERE datname = current_database() AND wait_event_type = 'Lock'",
            )
            .fetch_one(db.pool())
            .await
            .unwrap();
            if waiting > 0 {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "the call never reached the database"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }

        shutdown_tx.send(true).unwrap();
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert!(
            !serving.is_finished(),
            "serve must not return while a call is still running"
        );

        lock.rollback().await.unwrap();
        let response = call
            .await
            .unwrap()
            .expect("the in-flight call completes during the drain");
        assert_eq!(response.into_inner().total, 0);
        let result = tokio::time::timeout(Duration::from_secs(5), serving)
            .await
            .expect("serve returns once the call has drained")
            .unwrap();
        assert!(result.is_ok(), "{result:?}");
    }

    #[tokio::test]
    async fn grpc_health_turns_not_serving_first_and_the_drain_is_bounded() {
        let db = TestDatabase::new().await;
        let runtime = Arc::new(RuntimeHealth::default());
        let server = GrpcServer::bind(
            "127.0.0.1:0".parse().unwrap(),
            grpc_deps(Arc::clone(&db.pool)),
        )
        .await
        .unwrap();
        let addr = server.local_addr();
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let drain = Duration::from_secs(2);
        let serving = tokio::spawn(server.serve(shutdown_rx, drain, Some(Arc::clone(&runtime))));
        let mut health = HealthClient::new(grpc_channel(addr).await);

        let check = health
            .check(health_request("spooled.v1.QueueService"))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(check.status, ServingStatus::Serving as i32);
        assert!(runtime.grpc_serving());

        // A watcher (as a load balancer holds one) sees the flip while the
        // connection is still up.
        let mut watch = health.watch(health_request("")).await.unwrap().into_inner();
        let first = watch.message().await.unwrap().unwrap();
        assert_eq!(first.status, ServingStatus::Serving as i32);

        shutdown_tx.send(true).unwrap();
        let next = tokio::time::timeout(Duration::from_secs(2), watch.message())
            .await
            .expect("a health update arrives before the server goes away")
            .expect("the watch stream is still open")
            .expect("a status");
        assert_eq!(next.status, ServingStatus::NotServing as i32);
        assert!(!runtime.grpc_serving());

        // The open watch stream never ends by itself, yet serve returns once
        // the drain bound has passed.
        let result = tokio::time::timeout(drain + Duration::from_secs(3), serving)
            .await
            .expect("the drain is bounded")
            .unwrap();
        assert!(result.is_ok(), "{result:?}");
    }

    #[tokio::test]
    async fn grpc_health_reports_not_serving_while_the_database_is_unreachable() {
        let server = GrpcServer::bind(
            "127.0.0.1:0".parse().unwrap(),
            grpc_deps(Arc::new(unreachable_pool())),
        )
        .await
        .unwrap()
        .with_health_probe_interval(Duration::from_millis(200));
        let addr = server.local_addr();
        let (_shutdown_tx, shutdown_rx) = watch::channel(false);
        let serving = tokio::spawn(server.serve(shutdown_rx, Duration::from_secs(1), None));
        let mut health = HealthClient::new(grpc_channel(addr).await);

        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let queue = health
                .check(health_request("spooled.v1.QueueService"))
                .await
                .unwrap()
                .into_inner()
                .status;
            let overall = health
                .check(health_request(""))
                .await
                .unwrap()
                .into_inner()
                .status;
            if queue == ServingStatus::NotServing as i32
                && overall == ServingStatus::NotServing as i32
            {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "gRPC health must report NOT_SERVING while the database is unreachable"
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        serving.abort();
    }

    // -----------------------------------------------------------------------
    // BEOPS-09: the real binary stops when a server task cannot run.
    // -----------------------------------------------------------------------

    /// URL of this test's own database, for a child process.
    pub(super) async fn child_database_url(db: &TestDatabase) -> String {
        let (name,): (String,) = sqlx::query_as("SELECT current_database()")
            .fetch_one(db.pool())
            .await
            .unwrap();
        match std::env::var("TEST_DATABASE_URL") {
            Ok(admin) => {
                let mut url = url::Url::parse(&admin).unwrap();
                url.set_path(&format!("/{name}"));
                url.to_string()
            }
            Err(_) => {
                // The container fallback uses the image's default credentials.
                let options = db.pool.connect_options();
                format!(
                    "postgres://{}:postgres@{}:{}/{}",
                    options.get_username(),
                    options.get_host(),
                    options.get_port(),
                    name
                )
            }
        }
    }

    pub(super) fn free_port() -> u16 {
        std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port()
    }

    /// The real `spooled-backend` binary, killed when dropped.
    pub(super) struct Backend {
        child: Child,
        output: Arc<Mutex<String>>,
        port: u16,
    }

    impl Backend {
        fn spawn(database_url: &str, port: u16, grpc_port: u16, metrics_port: u16) -> Self {
            Self::spawn_with_env(database_url, port, grpc_port, metrics_port, &[])
        }

        /// `spawn`, with `env` set on top of the defaults.
        pub(super) fn spawn_with_env(
            database_url: &str,
            port: u16,
            grpc_port: u16,
            metrics_port: u16,
            env: &[(&str, &str)],
        ) -> Self {
            let mut command = Command::new(env!("CARGO_BIN_EXE_spooled-backend"));
            command
                .current_dir(std::env::temp_dir())
                .env("DATABASE_URL", database_url)
                .env("DATABASE_MAX_CONNECTIONS", "4")
                .env("DATABASE_MIN_CONNECTIONS", "1")
                .env("HOST", "127.0.0.1")
                .env("PORT", port.to_string())
                .env("GRPC_PORT", grpc_port.to_string())
                .env("METRICS_PORT", metrics_port.to_string())
                .env("RUST_ENV", "development")
                .env("RUST_LOG", "info")
                .env_remove("GRPC_TLS_ENABLED")
                .env_remove("RATE_LIMIT_FAIL_CLOSED");
            // Without a test Redis, leave REDIS_URL unset: a configured Redis
            // that does not answer makes /health report "degraded".
            match std::env::var("TEST_REDIS_URL") {
                Ok(url) => command.env("REDIS_URL", url),
                Err(_) => command.env_remove("REDIS_URL"),
            };
            for (name, value) in env {
                command.env(name, value);
            }
            let mut child = command
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .expect("start spooled-backend");
            let output = Arc::new(Mutex::new(String::new()));
            let pipes: Vec<Box<dyn Read + Send>> = vec![
                Box::new(child.stdout.take().unwrap()),
                Box::new(child.stderr.take().unwrap()),
            ];
            for mut pipe in pipes {
                let output = Arc::clone(&output);
                std::thread::spawn(move || {
                    let mut buf = [0u8; 4096];
                    while let Ok(n) = pipe.read(&mut buf) {
                        if n == 0 {
                            break;
                        }
                        output
                            .lock()
                            .unwrap()
                            .push_str(&String::from_utf8_lossy(&buf[..n]));
                    }
                });
            }
            Self {
                child,
                output,
                port,
            }
        }

        pub(super) fn output(&self) -> String {
            self.output.lock().unwrap().clone()
        }

        /// The exit status, if the process has ended.
        pub(super) fn exited(&mut self) -> Option<ExitStatus> {
            self.child.try_wait().unwrap()
        }

        /// The exit status, if the process ends within `limit`.
        async fn wait_for_exit(&mut self, limit: Duration) -> Option<ExitStatus> {
            let deadline = Instant::now() + limit;
            while Instant::now() < deadline {
                if let Some(status) = self.child.try_wait().unwrap() {
                    return Some(status);
                }
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
            None
        }

        /// `GET /health` over plain HTTP/1.1; None while nothing answers.
        pub(super) async fn health(&self) -> Option<(u16, Value)> {
            let (status, body) = self.get("/health", None).await?;
            Some((status, serde_json::from_str(&body).ok()?))
        }

        /// `GET path` over plain HTTP/1.1, with an `Authorization` header when
        /// given: the status and the raw body. None while nothing answers.
        pub(super) async fn get(
            &self,
            path: &str,
            authorization: Option<&str>,
        ) -> Option<(u16, String)> {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", self.port))
                .await
                .ok()?;
            let auth = authorization
                .map(|value| format!("Authorization: {value}\r\n"))
                .unwrap_or_default();
            let request = format!(
                "GET {path} HTTP/1.1\r\nHost: localhost\r\n{auth}Connection: close\r\n\r\n"
            );
            stream.write_all(request.as_bytes()).await.ok()?;
            let mut raw = Vec::new();
            stream.read_to_end(&mut raw).await.ok()?;
            let text = String::from_utf8_lossy(&raw);
            let status = text.split_whitespace().nth(1)?.parse().ok()?;
            let body = text.split("\r\n\r\n").nth(1)?;
            Some((status, body.to_string()))
        }
    }

    impl Drop for Backend {
        fn drop(&mut self) {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }

    #[tokio::test]
    async fn backend_exits_non_zero_when_grpc_cannot_bind() {
        let db = TestDatabase::new().await;
        let url = child_database_url(&db).await;
        let taken = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let grpc_port = taken.local_addr().unwrap().port();

        let mut backend = Backend::spawn(&url, free_port(), grpc_port, free_port());
        let status = backend.wait_for_exit(Duration::from_secs(45)).await;
        let output = backend.output();
        assert!(
            matches!(status, Some(s) if !s.success()),
            "the process must exit non-zero when gRPC cannot start (status {status:?}); output:\n{output}"
        );
        assert!(output.contains("gRPC"), "{output}");
        drop(taken);
    }

    #[tokio::test]
    async fn backend_exits_non_zero_when_the_metrics_server_dies() {
        let db = TestDatabase::new().await;
        let url = child_database_url(&db).await;
        let taken = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let metrics_port = taken.local_addr().unwrap().port();

        let mut backend = Backend::spawn(&url, free_port(), free_port(), metrics_port);
        let status = backend.wait_for_exit(Duration::from_secs(45)).await;
        let output = backend.output();
        assert!(
            matches!(status, Some(s) if !s.success()),
            "the process must exit non-zero when a server task ends (status {status:?}); output:\n{output}"
        );
        assert!(output.contains("metrics server"), "{output}");
        drop(taken);
    }

    #[tokio::test]
    async fn backend_health_reports_grpc_and_scheduler_running() {
        let db = TestDatabase::new().await;
        let url = child_database_url(&db).await;
        let grpc_port = free_port();
        let mut backend = Backend::spawn(&url, free_port(), grpc_port, free_port());

        let deadline = Instant::now() + Duration::from_secs(45);
        let body = loop {
            if let Some(status) = backend.child.try_wait().unwrap() {
                panic!(
                    "the process exited ({status}); output:\n{}",
                    backend.output()
                );
            }
            if let Some((200, body)) = backend.health().await {
                if body["grpc"] == true && body["scheduler"] == true {
                    break body;
                }
            }
            assert!(
                Instant::now() < deadline,
                "/health never reported gRPC and the scheduler up: {:?}\noutput:\n{}",
                backend.health().await,
                backend.output()
            );
            tokio::time::sleep(Duration::from_millis(250)).await;
        };
        assert_eq!(body["status"], "healthy", "{body}");

        let addr: SocketAddr = format!("127.0.0.1:{grpc_port}").parse().unwrap();
        let mut health = HealthClient::new(grpc_channel(addr).await);
        let status = health
            .check(health_request("spooled.v1.WorkerService"))
            .await
            .unwrap()
            .into_inner()
            .status;
        assert_eq!(status, ServingStatus::Serving as i32);
    }
}

// ---- wave 3 / review fixes ----
// W3R-01 / W3R-02 / W3R-04: StreamJobs pool use and demand over a real HTTP/2
// connection, and a Redis that is down when the process starts.
mod wave3_review {
    use super::*;

    use std::net::SocketAddr;
    use std::time::{Duration, Instant};

    use sqlx::postgres::PgPoolOptions;
    use tokio::sync::watch;
    use tokio_stream::StreamExt;

    use spooled_backend::cache::RedisCache;
    use spooled_backend::config::{QueueSettings, RedisSettings, WorkerSettings};
    use spooled_backend::grpc::proto::queue_service_client::QueueServiceClient;
    use spooled_backend::grpc::proto::queue_service_server::QueueService;
    use spooled_backend::grpc::proto::{CompleteRequest, StreamJobsRequest};
    use spooled_backend::grpc::server::GrpcServer;
    use spooled_backend::grpc::services::QueueServiceImpl;
    use spooled_backend::outgoing_webhooks::service::OutgoingWebhookService;

    use super::common::TestRedis;
    use super::wave3_runtime::{
        child_database_url, free_port, grpc_channel, grpc_deps, with_key, Backend,
    };

    fn stream_req(queue: &str, worker: &str, lease: i32) -> StreamJobsRequest {
        StreamJobsRequest {
            queue_name: queue.to_string(),
            worker_id: worker.to_string(),
            lease_duration_secs: lease,
        }
    }

    /// `count` small pending jobs, oldest first.
    async fn seed_jobs(pool: &sqlx::PgPool, org_id: &str, queue: &str, count: i32) {
        sqlx::query(
            "INSERT INTO jobs (id, organization_id, queue_name, payload, status, priority, max_retries, created_at, updated_at) \
             SELECT gen_random_uuid()::TEXT, $1, $2, jsonb_build_object('n', n), 'pending', 0, 3, \
                    NOW() + n * INTERVAL '1 millisecond', NOW() \
             FROM generate_series(1, $3) AS n",
        )
        .bind(org_id)
        .bind(queue)
        .bind(count)
        .execute(pool)
        .await
        .expect("seed jobs");
    }

    async fn processing(pool: &sqlx::PgPool, org_id: &str) -> i64 {
        let (n,): (i64,) = sqlx::query_as(
            "SELECT COUNT(*) FROM jobs WHERE organization_id = $1 AND status = 'processing'",
        )
        .bind(org_id)
        .fetch_one(pool)
        .await
        .unwrap();
        n
    }

    // W3R-01: a StreamJobs claim must not need a second pool connection while
    // its transaction holds one. With the org's cached plan limits gone (60 s
    // TTL, Redis flush or outage), the claim used to look them up on the pool:
    // with every connection held by a claim waiting for another, the process
    // stalled. A one-connection pool makes that wait certain.
    #[tokio::test]
    async fn grpc_stream_claim_never_waits_for_a_second_pool_connection() {
        let redis = TestRedis::new().await;
        let cache = RedisCache::connect(&RedisSettings {
            url: redis.url.clone(),
            pool_size: 4,
        })
        .await
        .expect("connect redis");
        let db = TestDatabase::new().await;
        let t = seed_tenant(db.pool()).await;
        let pool = PgPoolOptions::new()
            .max_connections(1)
            .acquire_timeout(Duration::from_secs(2))
            .connect_with((*db.pool.connect_options()).clone())
            .await
            .expect("one-connection pool");
        let service = QueueServiceImpl::new(
            Arc::new(pool),
            Arc::new(Metrics::new()),
            Some(cache.clone()),
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
        );

        let mut stream = service
            .stream_jobs(with_key(&t.key, stream_req("pq", "w-pool", 60)))
            .await
            .expect("stream opens")
            .into_inner();
        // Opening the stream cached the org's plan limits; drop the entry.
        cache
            .delete(&format!("org_rate_limits:{}", t.org_id))
            .await
            .unwrap();
        seed_jobs(db.pool(), &t.org_id, "pq", 1).await;

        let job = tokio::time::timeout(Duration::from_secs(8), stream.next())
            .await
            .expect("a job within 8s while the org's cached limits are missing")
            .expect("stream open")
            .expect("a job, not an error");
        assert_eq!(job.queue_name, "pq");
    }

    // W3R-02: over a real HTTP/2 connection the transport takes messages while
    // the client's flow-control window has room, so a stream that claimed
    // whenever its buffer was empty leased the queue to a worker holding one
    // job; those jobs expired unread and spent their retries. A stream keeps
    // one job with the worker until it is settled.
    #[tokio::test]
    async fn grpc_stream_jobs_over_http2_keeps_one_job_with_the_worker() {
        let db = TestDatabase::new().await;
        let t = seed_tenant(db.pool()).await;
        seed_jobs(db.pool(), &t.org_id, "h2q", 200).await;

        let server = GrpcServer::bind(
            "127.0.0.1:0".parse().unwrap(),
            grpc_deps(Arc::clone(&db.pool)),
        )
        .await
        .unwrap();
        let addr: SocketAddr = server.local_addr();
        let (_shutdown_tx, shutdown_rx) = watch::channel(false);
        let serving = tokio::spawn(server.serve(shutdown_rx, Duration::from_secs(1), None));
        let mut client = QueueServiceClient::new(grpc_channel(addr).await);

        let mut stream = client
            .stream_jobs(with_key(&t.key, stream_req("h2q", "w-h2", 0)))
            .await
            .expect("stream opens")
            .into_inner();
        let first = tokio::time::timeout(Duration::from_secs(10), stream.message())
            .await
            .expect("a job within 10s")
            .expect("stream ok")
            .expect("a job");

        // The client's window has room for the whole queue; the worker holds one.
        tokio::time::sleep(Duration::from_millis(1500)).await;
        assert_eq!(
            processing(db.pool(), &t.org_id).await,
            1,
            "jobs were leased into the transport while the worker held one"
        );

        // Completing it through this server lets the next job out.
        client
            .complete(with_key(
                &t.key,
                CompleteRequest {
                    job_id: first.id.clone(),
                    worker_id: "w-h2".to_string(),
                    result: None,
                    lease_id: first.lease_id.clone(),
                },
            ))
            .await
            .expect("complete");
        let second = tokio::time::timeout(Duration::from_secs(5), stream.message())
            .await
            .expect("the next job once the first is settled")
            .expect("stream ok")
            .expect("a job");
        assert_ne!(second.id, first.id);
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert_eq!(processing(db.pool(), &t.org_id).await, 1);

        // A settle the server does not see (REST, another replica) is found
        // by the stream's own check.
        sqlx::query("UPDATE jobs SET status = 'completed', completed_at = NOW() WHERE id = $1")
            .bind(&second.id)
            .execute(db.pool())
            .await
            .unwrap();
        let third = tokio::time::timeout(Duration::from_secs(5), stream.message())
            .await
            .expect("the next job once the second is settled elsewhere")
            .expect("stream ok")
            .expect("a job");
        assert!(third.id != first.id && third.id != second.id);
        assert_eq!(processing(db.pool(), &t.org_id).await, 1);

        let (retries,): (i64,) = sqlx::query_as(
            "SELECT COALESCE(SUM(retry_count), 0)::BIGINT FROM jobs WHERE organization_id = $1",
        )
        .bind(&t.org_id)
        .fetch_one(db.pool())
        .await
        .unwrap();
        assert_eq!(retries, 0);
        drop(stream);
        serving.abort();
    }

    // W3R-04: a backend that starts while its configured Redis is down keeps
    // trying to reach it. It used to run without Redis until restarted: with
    // RATE_LIMIT_FAIL_CLOSED every request was refused for good, while /health
    // reported "healthy".
    #[tokio::test]
    async fn backend_picks_up_a_redis_that_was_down_at_startup() {
        let redis = TestRedis::new().await;
        let db = TestDatabase::new().await;
        let t = seed_tenant(db.pool()).await;
        let database_url = child_database_url(&db).await;

        // REDIS_URL names a port where nothing answers yet.
        let real = url::Url::parse(&redis.url).unwrap();
        let upstream = format!(
            "{}:{}",
            real.host_str().unwrap(),
            real.port().unwrap_or(6379)
        );
        let redis_port = free_port();
        let mut late = real.clone();
        late.set_host(Some("127.0.0.1")).unwrap();
        late.set_port(Some(redis_port)).unwrap();

        let mut backend = Backend::spawn_with_env(
            &database_url,
            free_port(),
            free_port(),
            free_port(),
            &[
                ("REDIS_URL", late.as_str()),
                ("RATE_LIMIT_FAIL_CLOSED", "true"),
            ],
        );
        let auth = bearer(&t.key);

        let deadline = Instant::now() + Duration::from_secs(45);
        let body = loop {
            if let Some(status) = backend.exited() {
                panic!(
                    "the process exited ({status}); output:\n{}",
                    backend.output()
                );
            }
            if let Some((200, body)) = backend.health().await {
                break body;
            }
            assert!(
                Instant::now() < deadline,
                "/health never answered; output:\n{}",
                backend.output()
            );
            tokio::time::sleep(Duration::from_millis(250)).await;
        };
        assert_eq!(body["cache"], false, "{body}");
        assert_eq!(body["status"], "degraded", "{body}");
        let (status, _) = backend
            .get("/api/v1/jobs", Some(&auth))
            .await
            .expect("an answer");
        assert_eq!(status, 429, "fail-closed while Redis is down");

        // Redis comes up.
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", redis_port))
            .await
            .unwrap();
        let proxy = tokio::spawn(async move {
            while let Ok((mut inbound, _)) = listener.accept().await {
                let upstream = upstream.clone();
                tokio::spawn(async move {
                    if let Ok(mut outbound) = tokio::net::TcpStream::connect(&upstream).await {
                        let _ = tokio::io::copy_bidirectional(&mut inbound, &mut outbound).await;
                    }
                });
            }
        });

        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let jobs = backend.get("/api/v1/jobs", Some(&auth)).await;
            let health = backend.health().await;
            if matches!(jobs, Some((200, _)))
                && matches!(&health, Some((200, body)) if body["cache"] == true)
            {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "Redis was never picked up: jobs {jobs:?}, health {health:?}; output:\n{}",
                backend.output()
            );
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
        proxy.abort();
    }
}
// ---- wave 4 / billing ----

const W4B_WEBHOOK_SECRET: &str = "whsec_w4_billing_fixture";
const W4B_STRIPE_KEY: &str = "sk_test_w4_billing_fixture";
const W4B_STARTER_PRICE: &str = "price_w4_starter";
const W4B_PRO_PRICE: &str = "price_w4_pro";

/// Settings with Stripe configured against fixture values (never live Stripe:
/// every test sends the Stripe calls to a local mock via `w4b_stripe`).
fn w4b_settings() -> Settings {
    let mut settings = Settings::load_for_testing();
    settings.stripe.secret_key = Some(W4B_STRIPE_KEY.to_string());
    settings.stripe.webhook_secret = Some(W4B_WEBHOOK_SECRET.to_string());
    settings.stripe.starter_price_id = Some(W4B_STARTER_PRICE.to_string());
    settings.stripe.pro_price_id = Some(W4B_PRO_PRICE.to_string());
    settings
}

/// Call the API with every Stripe API request sent to `stripe`.
async fn w4b_stripe<T>(
    stripe: &wiremock::MockServer,
    fut: impl std::future::Future<Output = T>,
) -> T {
    spooled_backend::api::handlers::billing::with_stripe_api_base(stripe.uri(), fut).await
}

/// Deliver a signed Stripe event to the billing webhook.
async fn w4b_deliver(
    app: &Router,
    stripe: &wiremock::MockServer,
    event: &Value,
) -> (StatusCode, Value) {
    use hmac::{Hmac, KeyInit, Mac};

    let ts = chrono::Utc::now().timestamp();
    let mut mac = Hmac::<sha2::Sha256>::new_from_slice(W4B_WEBHOOK_SECRET.as_bytes()).unwrap();
    mac.update(format!("{ts}.{event}").as_bytes());
    let signature = format!("t={ts},v1={}", hex::encode(mac.finalize().into_bytes()));
    w4b_stripe(
        stripe,
        call(
            app,
            "POST",
            "/api/v1/billing/webhook",
            &[("Stripe-Signature", &signature)],
            Some(event.clone()),
        ),
    )
    .await
}

fn w4b_event(id: &str, event_type: &str, created: i64, object: Value) -> Value {
    json!({ "id": id, "type": event_type, "created": created, "data": { "object": object } })
}

/// A completed Checkout Session. `org_ref` holds `metadata` and/or
/// `client_reference_id`.
fn w4b_session(
    org_ref: Value,
    customer: &str,
    subscription: &str,
    email: &str,
    payment_status: &str,
) -> Value {
    let mut session = json!({
        "id": format!("cs_test_{subscription}"),
        "object": "checkout.session",
        "mode": "subscription",
        "customer": customer,
        "subscription": subscription,
        "customer_details": { "email": email },
        "payment_status": payment_status,
        "metadata": {}
    });
    for (key, value) in org_ref.as_object().unwrap() {
        session[key] = value.clone();
    }
    session
}

fn w4b_subscription(id: &str, customer: &str, price: &str, status: &str) -> Value {
    json!({
        "id": id,
        "object": "subscription",
        "customer": customer,
        "status": status,
        "cancel_at_period_end": false,
        "items": { "data": [{ "price": { "id": price }, "current_period_end": 1_900_000_000 }] }
    })
}

async fn w4b_mock_subscription(
    stripe: &wiremock::MockServer,
    id: &str,
    customer: &str,
    price: &str,
) {
    use wiremock::matchers::{method, path};
    wiremock::Mock::given(method("GET"))
        .and(path(format!("/v1/subscriptions/{id}")))
        .respond_with(
            wiremock::ResponseTemplate::new(200)
                .set_body_json(w4b_subscription(id, customer, price, "active")),
        )
        .mount(stripe)
        .await;
}

async fn w4b_mock_cancel_ok(stripe: &wiremock::MockServer, id: &str) {
    use wiremock::matchers::{method, path};
    wiremock::Mock::given(method("DELETE"))
        .and(path(format!("/v1/subscriptions/{id}")))
        .respond_with(
            wiremock::ResponseTemplate::new(200)
                .set_body_json(json!({ "id": id, "status": "canceled" })),
        )
        .mount(stripe)
        .await;
}

/// Requests the mock Stripe received with this method (and path, if given).
async fn w4b_received(
    stripe: &wiremock::MockServer,
    http_method: &str,
    path: Option<&str>,
) -> Vec<wiremock::Request> {
    stripe
        .received_requests()
        .await
        .unwrap_or_default()
        .into_iter()
        .filter(|r| r.method.as_str() == http_method && path.is_none_or(|p| r.url.path() == p))
        .collect()
}

async fn w4b_set_billing(
    pool: &sqlx::PgPool,
    org_id: &str,
    plan: &str,
    customer: Option<&str>,
    subscription: Option<&str>,
    status: Option<&str>,
) {
    sqlx::query(
        "UPDATE organizations SET plan_tier = $2, stripe_customer_id = $3, \
         stripe_subscription_id = $4, stripe_subscription_status = $5 WHERE id = $1",
    )
    .bind(org_id)
    .bind(plan)
    .bind(customer)
    .bind(subscription)
    .bind(status)
    .execute(pool)
    .await
    .expect("set billing");
}

#[derive(Debug, sqlx::FromRow)]
struct W4bBilling {
    plan_tier: String,
    stripe_customer_id: Option<String>,
    stripe_subscription_id: Option<String>,
    stripe_superseded_subscription_ids: Vec<String>,
}

async fn w4b_billing(pool: &sqlx::PgPool, org_id: &str) -> W4bBilling {
    sqlx::query_as(
        "SELECT plan_tier, stripe_customer_id, stripe_subscription_id, \
         stripe_superseded_subscription_ids FROM organizations WHERE id = $1",
    )
    .bind(org_id)
    .fetch_one(pool)
    .await
    .expect("org billing")
}

async fn w4b_billing_email(pool: &sqlx::PgPool, org_id: &str) -> String {
    let (email,): (String,) =
        sqlx::query_as("SELECT billing_email FROM organizations WHERE id = $1")
            .bind(org_id)
            .fetch_one(pool)
            .await
            .expect("billing email");
    email
}

async fn w4b_scoped_key(pool: &sqlx::PgPool, org_id: &str) -> String {
    let key = format!("sp_test_{}", uuid::Uuid::new_v4().simple());
    sqlx::query(
        "INSERT INTO api_keys (id, organization_id, key_hash, key_prefix, lookup_hash, name, queues, is_active, created_at) \
         VALUES ($1, $2, $3, $4, $5, 'w4b-scoped', ARRAY['w4b-queue']::TEXT[], TRUE, NOW())",
    )
    .bind(uuid::Uuid::new_v4().to_string())
    .bind(org_id)
    .bind(bcrypt::hash(&key, 4).expect("hash key"))
    .bind(key.chars().take(8).collect::<String>())
    .bind(spooled_backend::models::api_key_lookup_hash(&key))
    .execute(pool)
    .await
    .expect("seed scoped key");
    key
}

async fn w4b_checkout(
    app: Router,
    stripe: &wiremock::MockServer,
    auth: String,
    body: Option<Value>,
) -> (StatusCode, Value) {
    w4b_stripe(
        stripe,
        call(
            &app,
            "POST",
            "/api/v1/billing/checkout",
            &[("Authorization", &auth)],
            body,
        ),
    )
    .await
}

// ---------------------------------------------------------------------------
// BEBILL-02 / C1: POST /billing/checkout creates an org-bound Checkout Session.
// ---------------------------------------------------------------------------
#[tokio::test]
async fn w4_billing_checkout_validates_and_binds_the_session_to_the_org() {
    use wiremock::matchers::{header, method, path};

    let db = TestDatabase::new().await;
    let t = seed_tenant(db.pool()).await;
    let auth = bearer(&t.key);
    let stripe = wiremock::MockServer::start().await;
    wiremock::Mock::given(method("POST"))
        .and(path("/v1/checkout/sessions"))
        .and(header(
            "authorization",
            format!("Bearer {W4B_STRIPE_KEY}").as_str(),
        ))
        .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(json!({
            "id": "cs_test_w4b",
            "object": "checkout.session",
            "url": "https://checkout.stripe.com/c/pay/cs_test_w4b"
        })))
        .mount(&stripe)
        .await;
    let checkout =
        |app: Router, auth: String, body: Option<Value>| w4b_checkout(app, &stripe, auth, body);

    // Stripe not configured -> 503.
    let (status, body) = checkout(app(&db), auth.clone(), Some(json!({"plan": "pro"}))).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    assert_eq!(body["code"], "SERVICE_UNAVAILABLE", "{body}");

    // That plan's price not configured -> 503.
    let mut no_pro = w4b_settings();
    no_pro.stripe.pro_price_id = None;
    let (status, body) = checkout(
        app_with(&db, no_pro),
        auth.clone(),
        Some(json!({"plan": "pro"})),
    )
    .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");

    let app = app_with(&db, w4b_settings());

    // Any other plan, or no plan at all -> 400 VALIDATION_ERROR (never 401).
    for bad in [
        Some(json!({"plan": "enterprise"})),
        Some(json!({"plan": "free"})),
        Some(json!({})),
        None,
    ] {
        let (status, body) = checkout(app.clone(), auth.clone(), bad.clone()).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{bad:?}: {body}");
        assert_eq!(body["code"], "VALIDATION_ERROR", "{bad:?}: {body}");
    }

    // A queue-scoped key cannot start a checkout.
    let scoped = bearer(&w4b_scoped_key(db.pool(), &t.org_id).await);
    let (status, body) = checkout(app.clone(), scoped, Some(json!({"plan": "pro"}))).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");

    // Already on that plan with an active subscription -> 409, use the portal.
    w4b_set_billing(
        db.pool(),
        &t.org_id,
        "starter",
        Some("cus_w4b"),
        Some("sub_w4b"),
        Some("active"),
    )
    .await;
    let (status, body) =
        checkout(app.clone(), auth.clone(), Some(json!({"plan": "starter"}))).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert!(
        body["message"].as_str().unwrap().contains("billing portal"),
        "{body}"
    );
    assert!(
        w4b_received(&stripe, "POST", None).await.is_empty(),
        "no session may be created on an error"
    );

    // Upgrade: the session is bound to the org and reuses its customer.
    let (status, body) = checkout(app.clone(), auth.clone(), Some(json!({"plan": "pro"}))).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["session_id"], "cs_test_w4b", "{body}");
    assert_eq!(
        body["url"], "https://checkout.stripe.com/c/pay/cs_test_w4b",
        "{body}"
    );

    let sent = w4b_received(&stripe, "POST", Some("/v1/checkout/sessions")).await;
    assert_eq!(sent.len(), 1);
    let form: std::collections::HashMap<String, String> =
        url::form_urlencoded::parse(&sent[0].body)
            .into_owned()
            .collect();
    assert_eq!(form["mode"], "subscription");
    assert_eq!(form["line_items[0][price]"], W4B_PRO_PRICE);
    assert_eq!(form["client_reference_id"], t.org_id);
    assert_eq!(form["metadata[spooled_org_id]"], t.org_id);
    assert_eq!(
        form["subscription_data[metadata][spooled_org_id]"],
        t.org_id
    );
    assert!(
        form["success_url"].ends_with("/account/billing?checkout=success"),
        "{form:?}"
    );
    assert!(
        form["cancel_url"].ends_with("/account/billing?checkout=cancel"),
        "{form:?}"
    );
    assert_eq!(form["customer"], "cus_w4b");
    assert!(!form.contains_key("customer_email"), "{form:?}");

    // No customer yet: Checkout is prefilled with the verified billing email.
    let fresh = seed_tenant(db.pool()).await;
    let (status, body) = checkout(
        app.clone(),
        bearer(&fresh.key),
        Some(json!({"plan": "starter"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let sent = w4b_received(&stripe, "POST", Some("/v1/checkout/sessions")).await;
    let form: std::collections::HashMap<String, String> =
        url::form_urlencoded::parse(&sent[1].body)
            .into_owned()
            .collect();
    assert_eq!(form["line_items[0][price]"], W4B_STARTER_PRICE);
    assert_eq!(form["metadata[spooled_org_id]"], fresh.org_id);
    assert_eq!(
        form["customer_email"],
        w4b_billing_email(db.pool(), &fresh.org_id).await
    );
    assert!(!form.contains_key("customer"), "{form:?}");
}

// ---------------------------------------------------------------------------
// BEBILL-02: a server-created checkout links its org and cancels the
// subscription it replaces, even when the new subscription's events (same
// customer) arrive before checkout.session.completed.
// ---------------------------------------------------------------------------
#[tokio::test]
async fn w4_billing_server_checkout_links_the_org_and_cancels_the_replaced_subscription() {
    let db = TestDatabase::new().await;
    let t = seed_tenant(db.pool()).await;
    w4b_set_billing(
        db.pool(),
        &t.org_id,
        "starter",
        Some("cus_w4u"),
        Some("sub_w4u_old"),
        Some("active"),
    )
    .await;
    let app = app_with(&db, w4b_settings());
    let stripe = wiremock::MockServer::start().await;
    w4b_mock_subscription(&stripe, "sub_w4u_new", "cus_w4u", W4B_PRO_PRICE).await;
    w4b_mock_cancel_ok(&stripe, "sub_w4u_old").await;
    let t0 = chrono::Utc::now().timestamp() - 60;

    // The new subscription's own event usually comes first. It must not take
    // the org over from the subscription the checkout has to cancel.
    let created = w4b_event(
        "evt_w4u_created",
        "customer.subscription.created",
        t0,
        w4b_subscription("sub_w4u_new", "cus_w4u", W4B_PRO_PRICE, "active"),
    );
    let (status, body) = w4b_deliver(&app, &stripe, &created).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        w4b_billing(db.pool(), &t.org_id)
            .await
            .stripe_subscription_id
            .as_deref(),
        Some("sub_w4u_old")
    );

    let session = w4b_session(
        json!({ "metadata": { "spooled_org_id": t.org_id }, "client_reference_id": t.org_id }),
        "cus_w4u",
        "sub_w4u_new",
        "someone@example.com",
        "paid",
    );
    let completed = w4b_event(
        "evt_w4u_completed",
        "checkout.session.completed",
        t0 + 1,
        session,
    );
    let (status, body) = w4b_deliver(&app, &stripe, &completed).await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let billing = w4b_billing(db.pool(), &t.org_id).await;
    assert_eq!(billing.stripe_customer_id.as_deref(), Some("cus_w4u"));
    assert_eq!(
        billing.stripe_subscription_id.as_deref(),
        Some("sub_w4u_new")
    );
    assert_eq!(billing.plan_tier, "pro");
    assert!(
        billing.stripe_superseded_subscription_ids.is_empty(),
        "{billing:?}"
    );
    assert_eq!(
        w4b_received(&stripe, "DELETE", Some("/v1/subscriptions/sub_w4u_old"))
            .await
            .len(),
        1,
        "the replaced subscription is canceled exactly once"
    );

    // Stripe then reports the old subscription deleted: the org stays on pro.
    let deleted = w4b_event(
        "evt_w4u_deleted",
        "customer.subscription.deleted",
        t0 + 2,
        w4b_subscription("sub_w4u_old", "cus_w4u", W4B_STARTER_PRICE, "canceled"),
    );
    let (status, body) = w4b_deliver(&app, &stripe, &deleted).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let billing = w4b_billing(db.pool(), &t.org_id).await;
    assert_eq!(billing.plan_tier, "pro");
    assert_eq!(
        billing.stripe_subscription_id.as_deref(),
        Some("sub_w4u_new")
    );
}

// ---------------------------------------------------------------------------
// BEBILL-02: a Payment Link checkout naming someone else's org (an editable
// URL parameter) neither relinks nor cancels the victim's subscription, even
// with the victim's billing email typed in at checkout.
// ---------------------------------------------------------------------------
#[tokio::test]
async fn w4_billing_payment_link_checkout_cannot_take_over_a_subscribed_org() {
    let db = TestDatabase::new().await;
    let victim = seed_tenant(db.pool()).await;
    w4b_set_billing(
        db.pool(),
        &victim.org_id,
        "starter",
        Some("cus_victim"),
        Some("sub_victim"),
        Some("active"),
    )
    .await;
    let victim_email = w4b_billing_email(db.pool(), &victim.org_id).await;
    let app = app_with(&db, w4b_settings());
    let stripe = wiremock::MockServer::start().await;
    w4b_mock_subscription(&stripe, "sub_attacker", "cus_attacker", W4B_PRO_PRICE).await;
    w4b_mock_cancel_ok(&stripe, "sub_victim").await;

    let session = w4b_session(
        json!({ "client_reference_id": victim.org_id }),
        "cus_attacker",
        "sub_attacker",
        &victim_email.to_uppercase(),
        "paid",
    );
    let event = w4b_event(
        "evt_w4_takeover",
        "checkout.session.completed",
        chrono::Utc::now().timestamp(),
        session,
    );
    let (status, body) = w4b_deliver(&app, &stripe, &event).await;
    assert_eq!(status, StatusCode::OK, "the event is acknowledged: {body}");

    let billing = w4b_billing(db.pool(), &victim.org_id).await;
    assert_eq!(billing.stripe_customer_id.as_deref(), Some("cus_victim"));
    assert_eq!(
        billing.stripe_subscription_id.as_deref(),
        Some("sub_victim")
    );
    assert_eq!(billing.plan_tier, "starter");
    assert!(
        w4b_received(&stripe, "DELETE", None).await.is_empty(),
        "nothing may be canceled"
    );
}

// ---------------------------------------------------------------------------
// BEBILL-02: a Payment Link checkout links an unsubscribed org only when the
// payer used the org's verified billing email; it never cancels anything.
// ---------------------------------------------------------------------------
#[tokio::test]
async fn w4_billing_payment_link_checkout_needs_the_verified_billing_email() {
    let db = TestDatabase::new().await;
    let app = app_with(&db, w4b_settings());
    let stripe = wiremock::MockServer::start().await;
    let now = chrono::Utc::now().timestamp();

    // Email mismatch -> not linked.
    let other = seed_tenant(db.pool()).await;
    w4b_set_billing(db.pool(), &other.org_id, "free", None, None, None).await;
    let session = w4b_session(
        json!({ "client_reference_id": other.org_id }),
        "cus_w4_mismatch",
        "sub_w4_mismatch",
        "not-the-owner@example.com",
        "paid",
    );
    let (status, body) = w4b_deliver(
        &app,
        &stripe,
        &w4b_event(
            "evt_w4_mismatch",
            "checkout.session.completed",
            now,
            session,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let billing = w4b_billing(db.pool(), &other.org_id).await;
    assert_eq!(billing.stripe_customer_id, None, "{billing:?}");
    assert_eq!(billing.plan_tier, "free");

    // Matching but unverified email -> not linked.
    let unverified = seed_tenant(db.pool()).await;
    w4b_set_billing(db.pool(), &unverified.org_id, "free", None, None, None).await;
    sqlx::query("UPDATE organizations SET billing_email_verified_at = NULL WHERE id = $1")
        .bind(&unverified.org_id)
        .execute(db.pool())
        .await
        .unwrap();
    let session = w4b_session(
        json!({ "client_reference_id": unverified.org_id }),
        "cus_w4_unverified",
        "sub_w4_unverified",
        &w4b_billing_email(db.pool(), &unverified.org_id).await,
        "paid",
    );
    let (status, body) = w4b_deliver(
        &app,
        &stripe,
        &w4b_event(
            "evt_w4_unverified",
            "checkout.session.completed",
            now,
            session,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        w4b_billing(db.pool(), &unverified.org_id)
            .await
            .stripe_customer_id,
        None
    );

    // Verified email (any case), no live subscription -> linked.
    let owner = seed_tenant(db.pool()).await;
    w4b_set_billing(
        db.pool(),
        &owner.org_id,
        "free",
        Some("cus_w4_old"),
        Some("sub_w4_ended"),
        Some("canceled"),
    )
    .await;
    w4b_mock_subscription(&stripe, "sub_w4_owner", "cus_w4_owner", W4B_STARTER_PRICE).await;
    let session = w4b_session(
        json!({ "client_reference_id": owner.org_id }),
        "cus_w4_owner",
        "sub_w4_owner",
        &w4b_billing_email(db.pool(), &owner.org_id)
            .await
            .to_uppercase(),
        "paid",
    );
    let (status, body) = w4b_deliver(
        &app,
        &stripe,
        &w4b_event("evt_w4_owner", "checkout.session.completed", now, session),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let billing = w4b_billing(db.pool(), &owner.org_id).await;
    assert_eq!(billing.stripe_customer_id.as_deref(), Some("cus_w4_owner"));
    assert_eq!(
        billing.stripe_subscription_id.as_deref(),
        Some("sub_w4_owner")
    );
    assert_eq!(billing.plan_tier, "starter");
    assert!(
        billing.stripe_superseded_subscription_ids.is_empty(),
        "{billing:?}"
    );
    assert!(
        w4b_received(&stripe, "DELETE", None).await.is_empty(),
        "nothing may be canceled"
    );
}

// ---------------------------------------------------------------------------
// BEBILL-14: a failed cancel of the replaced subscription fails the event, and
// Stripe's redelivery finishes the cancel.
// ---------------------------------------------------------------------------
#[tokio::test]
async fn w4_billing_failed_superseded_cancel_is_retried_with_the_event() {
    use wiremock::matchers::{method, path};

    let db = TestDatabase::new().await;
    let t = seed_tenant(db.pool()).await;
    w4b_set_billing(
        db.pool(),
        &t.org_id,
        "starter",
        Some("cus_w4r"),
        Some("sub_w4r_old"),
        Some("active"),
    )
    .await;
    let app = app_with(&db, w4b_settings());
    let stripe = wiremock::MockServer::start().await;
    w4b_mock_subscription(&stripe, "sub_w4r_new", "cus_w4r", W4B_PRO_PRICE).await;
    wiremock::Mock::given(method("DELETE"))
        .and(path("/v1/subscriptions/sub_w4r_old"))
        .respond_with(wiremock::ResponseTemplate::new(500))
        .up_to_n_times(1)
        .with_priority(1)
        .mount(&stripe)
        .await;
    w4b_mock_cancel_ok(&stripe, "sub_w4r_old").await;

    let session = w4b_session(
        json!({ "metadata": { "spooled_org_id": t.org_id } }),
        "cus_w4r",
        "sub_w4r_new",
        "someone@example.com",
        "paid",
    );
    let event = w4b_event(
        "evt_w4r",
        "checkout.session.completed",
        chrono::Utc::now().timestamp(),
        session,
    );

    let (status, body) = w4b_deliver(&app, &stripe, &event).await;
    assert!(
        status.is_server_error(),
        "a failed cancel must ask Stripe to retry: {status} {body}"
    );
    let billing = w4b_billing(db.pool(), &t.org_id).await;
    assert_eq!(
        billing.stripe_subscription_id.as_deref(),
        Some("sub_w4r_new"),
        "the customer keeps the plan they paid for"
    );
    assert_eq!(billing.plan_tier, "pro");
    assert_eq!(
        billing.stripe_superseded_subscription_ids,
        vec!["sub_w4r_old".to_string()]
    );
    let (claims,): (i64,) =
        sqlx::query_as("SELECT COUNT(*) FROM processed_stripe_events WHERE event_id = 'evt_w4r'")
            .fetch_one(db.pool())
            .await
            .unwrap();
    assert_eq!(claims, 0, "the claim is released for the retry");

    // Stripe redelivers the same event: the cancel goes through this time.
    let (status, body) = w4b_deliver(&app, &stripe, &event).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let billing = w4b_billing(db.pool(), &t.org_id).await;
    assert!(
        billing.stripe_superseded_subscription_ids.is_empty(),
        "{billing:?}"
    );
    assert_eq!(
        billing.stripe_subscription_id.as_deref(),
        Some("sub_w4r_new")
    );
    assert_eq!(
        w4b_received(&stripe, "DELETE", Some("/v1/subscriptions/sub_w4r_old"))
            .await
            .len(),
        2
    );
}

// ---------------------------------------------------------------------------
// BEBILL-14: Stripe refusing to cancel a subscription that already ended
// counts as done, so the event does not retry forever.
// ---------------------------------------------------------------------------
#[tokio::test]
async fn w4_billing_cancel_of_an_already_ended_subscription_counts_as_done() {
    use wiremock::matchers::{method, path};

    let db = TestDatabase::new().await;
    let t = seed_tenant(db.pool()).await;
    w4b_set_billing(
        db.pool(),
        &t.org_id,
        "starter",
        Some("cus_w4e"),
        Some("sub_w4e_old"),
        Some("past_due"),
    )
    .await;
    let app = app_with(&db, w4b_settings());
    let stripe = wiremock::MockServer::start().await;
    w4b_mock_subscription(&stripe, "sub_w4e_new", "cus_w4e", W4B_STARTER_PRICE).await;
    wiremock::Mock::given(method("DELETE"))
        .and(path("/v1/subscriptions/sub_w4e_old"))
        .respond_with(wiremock::ResponseTemplate::new(400).set_body_json(json!({
            "error": { "type": "invalid_request_error", "message": "subscription already canceled" }
        })))
        .mount(&stripe)
        .await;
    wiremock::Mock::given(method("GET"))
        .and(path("/v1/subscriptions/sub_w4e_old"))
        .respond_with(
            wiremock::ResponseTemplate::new(200).set_body_json(w4b_subscription(
                "sub_w4e_old",
                "cus_w4e",
                W4B_STARTER_PRICE,
                "canceled",
            )),
        )
        .mount(&stripe)
        .await;

    let session = w4b_session(
        json!({ "metadata": { "spooled_org_id": t.org_id } }),
        "cus_w4e",
        "sub_w4e_new",
        "someone@example.com",
        "paid",
    );
    let event = w4b_event(
        "evt_w4e",
        "checkout.session.completed",
        chrono::Utc::now().timestamp(),
        session,
    );
    let (status, body) = w4b_deliver(&app, &stripe, &event).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let billing = w4b_billing(db.pool(), &t.org_id).await;
    assert!(
        billing.stripe_superseded_subscription_ids.is_empty(),
        "{billing:?}"
    );
    assert_eq!(
        billing.stripe_subscription_id.as_deref(),
        Some("sub_w4e_new")
    );
}

// ---------------------------------------------------------------------------
// BEBILL-15: a delayed payment method completes the session unpaid; the
// later checkout.session.async_payment_succeeded links the org.
// ---------------------------------------------------------------------------
#[tokio::test]
async fn w4_billing_async_payment_succeeded_links_the_org() {
    let db = TestDatabase::new().await;
    let t = seed_tenant(db.pool()).await;
    w4b_set_billing(db.pool(), &t.org_id, "free", None, None, None).await;
    let app = app_with(&db, w4b_settings());
    let stripe = wiremock::MockServer::start().await;
    w4b_mock_subscription(&stripe, "sub_w4a", "cus_w4a", W4B_STARTER_PRICE).await;
    let now = chrono::Utc::now().timestamp();
    let org_ref = json!({ "metadata": { "spooled_org_id": t.org_id } });

    let unpaid = w4b_session(
        org_ref.clone(),
        "cus_w4a",
        "sub_w4a",
        "someone@example.com",
        "unpaid",
    );
    let (status, body) = w4b_deliver(
        &app,
        &stripe,
        &w4b_event(
            "evt_w4a_completed",
            "checkout.session.completed",
            now,
            unpaid.clone(),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        w4b_billing(db.pool(), &t.org_id).await.stripe_customer_id,
        None,
        "not paid yet"
    );

    let (status, body) = w4b_deliver(
        &app,
        &stripe,
        &w4b_event(
            "evt_w4a_failed",
            "checkout.session.async_payment_failed",
            now,
            unpaid,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        w4b_billing(db.pool(), &t.org_id).await.stripe_customer_id,
        None,
        "a failed payment links nothing"
    );

    let paid = w4b_session(org_ref, "cus_w4a", "sub_w4a", "someone@example.com", "paid");
    let (status, body) = w4b_deliver(
        &app,
        &stripe,
        &w4b_event(
            "evt_w4a_succeeded",
            "checkout.session.async_payment_succeeded",
            now + 1,
            paid,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let billing = w4b_billing(db.pool(), &t.org_id).await;
    assert_eq!(billing.stripe_customer_id.as_deref(), Some("cus_w4a"));
    assert_eq!(billing.stripe_subscription_id.as_deref(), Some("sub_w4a"));
    assert_eq!(billing.plan_tier, "starter");
}

// ---------------------------------------------------------------------------
// BEBILL-25: subscription/invoice events for a customer that is not linked
// yet ask Stripe to retry without a 500.
// ---------------------------------------------------------------------------
#[tokio::test]
async fn w4_billing_events_before_the_link_ask_for_a_retry_without_a_500() {
    let db = TestDatabase::new().await;
    let app = app_with(&db, w4b_settings());
    let stripe = wiremock::MockServer::start().await;
    let now = chrono::Utc::now().timestamp();

    let created = w4b_event(
        "evt_w4_early_sub",
        "customer.subscription.created",
        now,
        w4b_subscription("sub_w4_early", "cus_w4_early", W4B_STARTER_PRICE, "active"),
    );
    let (status, body) = w4b_deliver(&app, &stripe, &created).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");

    let invoice = w4b_event(
        "evt_w4_early_invoice",
        "invoice.paid",
        now,
        json!({ "id": "in_w4_early", "customer": "cus_w4_early", "subscription": "sub_w4_early" }),
    );
    let (status, body) = w4b_deliver(&app, &stripe, &invoice).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
}

// ---------------------------------------------------------------------------
// BEBILL-23: billing status needs an unrestricted key, like the portal.
// ---------------------------------------------------------------------------
#[tokio::test]
async fn w4_billing_status_requires_an_unrestricted_key() {
    let db = TestDatabase::new().await;
    let t = seed_tenant(db.pool()).await;
    let app = app(&db);

    let scoped = bearer(&w4b_scoped_key(db.pool(), &t.org_id).await);
    let (status, body) = call(
        &app,
        "GET",
        "/api/v1/billing/status",
        &[("Authorization", &scoped)],
        None,
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");

    let (status, body) = call(
        &app,
        "GET",
        "/api/v1/billing/status",
        &[("Authorization", &bearer(&t.key))],
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["plan_tier"], "starter", "{body}");
}
// ---- wave 4 / limits ----
// BEAUTH-08 / BEBILL-09 system API keys, BEAUTH-09 / BEBILL-06 / BEFLOW-07 caps on
// PUT re-activation, BEBILL-07 / BEBILL-26 custom_limits, BEFLOW-06 soft delete,
// BEBILL-10 / BEFLOW-17 active-only workflow count.
mod wave4_limits {
    use super::*;

    use spooled_backend::outgoing_webhooks::service::OutgoingWebhookService;
    use spooled_backend::scheduler::Scheduler;

    const ADMIN_KEY: &str = "w4-limits-admin-key";

    fn admin_app(db: &TestDatabase) -> Router {
        let mut settings = Settings::load_for_testing();
        settings.registration.admin_api_key = Some(ADMIN_KEY.to_string());
        app_with(db, settings)
    }

    fn scheduler(db: &TestDatabase) -> Scheduler {
        Scheduler::new(
            Arc::clone(&db.pool),
            None,
            Arc::new(Metrics::new()),
            Arc::new(OutgoingWebhookService::new((*db.pool).clone(), 3, 4)),
        )
    }

    async fn set_plan(pool: &sqlx::PgPool, org_id: &str, plan: &str, custom: Option<Value>) {
        sqlx::query("UPDATE organizations SET plan_tier = $2, custom_limits = $3 WHERE id = $1")
            .bind(org_id)
            .bind(plan)
            .bind(custom)
            .execute(pool)
            .await
            .expect("set plan");
    }

    /// (api_keys, schedules, workflows) as the plan caps count them.
    async fn counts(pool: &sqlx::PgPool, org_id: &str) -> (i64, i64, i64) {
        sqlx::query_as("SELECT api_keys, schedules, workflows FROM get_org_resource_counts($1)")
            .bind(org_id)
            .fetch_one(pool)
            .await
            .unwrap()
    }

    async fn email_login(app: &Router, pool: &sqlx::PgPool, email: &str) {
        let (status, body) = call(
            app,
            "POST",
            "/api/v1/auth/email/start",
            &[],
            Some(json!({"email": email})),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let (code, _) = latest_code(pool, email).await;
        let (status, body) = call(
            app,
            "POST",
            "/api/v1/auth/email/verify",
            &[],
            Some(json!({"email": email, "code": code})),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["type"], "login", "{body}");
    }

    async fn list_keys(app: &Router, auth: &str) -> Vec<Value> {
        let (status, body) = call(
            app,
            "GET",
            "/api/v1/api-keys",
            &[("Authorization", auth)],
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        body.as_array().expect("key list").clone()
    }

    async fn create_key(app: &Router, auth: &str, body: Value) -> (StatusCode, Value) {
        call(
            app,
            "POST",
            "/api/v1/api-keys",
            &[("Authorization", auth)],
            Some(body),
        )
        .await
    }

    async fn delete(app: &Router, auth: &str, path: &str) -> StatusCode {
        call(app, "DELETE", path, &[("Authorization", auth)], None)
            .await
            .0
    }

    async fn put(app: &Router, auth: &str, path: &str, body: Value) -> (StatusCode, Value) {
        call(app, "PUT", path, &[("Authorization", auth)], Some(body)).await
    }

    // BEAUTH-08 / BEBILL-09 (contract C3): the email-login key is a system key,
    // flagged in the API and outside the max_api_keys count; no request sets the flag.
    #[tokio::test]
    async fn email_login_key_is_a_system_key_outside_the_key_cap() {
        let db = TestDatabase::new().await;
        let t = seed_tenant(db.pool()).await;
        set_plan(db.pool(), &t.org_id, "free", None).await; // max_api_keys 2
        let app = app(&db);
        let auth = bearer(&t.key);
        let email = format!("bh-{}@example.com", &t.org_id[..8]);

        // Two logins share one system key.
        email_login(&app, db.pool(), &email).await;
        email_login(&app, db.pool(), &email).await;
        let keys = list_keys(&app, &auth).await;
        assert!(keys.iter().all(|k| k["is_system"].is_boolean()), "{keys:?}");
        let system: Vec<&Value> = keys.iter().filter(|k| k["is_system"] == true).collect();
        assert_eq!(system.len(), 1, "{keys:?}");
        assert_eq!(system[0]["name"], "Email Login");
        let system_id = system[0]["id"].as_str().unwrap().to_string();
        let (status, body) = call(
            &app,
            "GET",
            &format!("/api/v1/api-keys/{system_id}"),
            &[("Authorization", &auth)],
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["is_system"], true, "{body}");

        // The seed key is 1 of 2: the system key takes no slot, so a second
        // customer key fits, and a third does not.
        let (status, body) =
            create_key(&app, &auth, json!({"name": "second", "is_system": true})).await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        let second = body["id"].as_str().unwrap().to_string();
        assert_eq!(counts(db.pool(), &t.org_id).await.0, 2);
        let (status, body) = create_key(&app, &auth, json!({"name": "third"})).await;
        assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "{body}");

        // Neither create nor update can set the flag.
        let (status, body) = call(
            &app,
            "GET",
            &format!("/api/v1/api-keys/{second}"),
            &[("Authorization", &auth)],
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["is_system"], false, "{body}");
        let (status, body) = put(
            &app,
            &auth,
            &format!("/api/v1/api-keys/{second}"),
            json!({"is_system": true, "name": "second-renamed"}),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["is_system"], false, "{body}");

        // A customer key named like the system key is not mistaken for it: once
        // the user revokes the system key, the next login mints a new system key
        // instead of binding the dashboard session to the customer's key.
        let path = format!("/api/v1/api-keys/{system_id}");
        assert_eq!(delete(&app, &auth, &path).await, StatusCode::NO_CONTENT);
        let path = format!("/api/v1/api-keys/{second}");
        assert_eq!(delete(&app, &auth, &path).await, StatusCode::NO_CONTENT);
        let (status, body) = create_key(&app, &auth, json!({"name": "Email Login"})).await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        let lookalike = body["id"].as_str().unwrap().to_string();

        email_login(&app, db.pool(), &email).await;
        let keys = list_keys(&app, &auth).await;
        let active_system: Vec<&Value> = keys
            .iter()
            .filter(|k| k["is_system"] == true && k["is_active"] == true)
            .collect();
        assert_eq!(active_system.len(), 1, "{keys:?}");
        assert_ne!(active_system[0]["id"], lookalike.as_str());
        assert_ne!(active_system[0]["id"], system_id.as_str());
        let customer = keys.iter().find(|k| k["id"] == lookalike.as_str()).unwrap();
        assert_eq!(customer["is_system"], false, "{customer}");
        assert_eq!(counts(db.pool(), &t.org_id).await.0, 2);
    }

    // BEAUTH-09 / BEBILL-06 / BEFLOW-07: a PUT that re-activates a revoked key or
    // a paused schedule pays the plan cap like create / resume, and a re-activated
    // schedule's next fire is computed from now.
    #[tokio::test]
    async fn put_reactivation_pays_the_cap_and_recomputes_next_run() {
        let db = TestDatabase::new().await;
        let t = seed_tenant(db.pool()).await;
        set_plan(db.pool(), &t.org_id, "free", None).await; // 2 keys, 1 schedule
        let app = app(&db);
        let auth = bearer(&t.key);

        // Keys: seed (1/2) -> A (2/2) -> revoke A -> C (2/2) -> re-enable A.
        let (status, body) = create_key(&app, &auth, json!({"name": "a"})).await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        let a = format!("/api/v1/api-keys/{}", body["id"].as_str().unwrap());
        assert_eq!(delete(&app, &auth, &a).await, StatusCode::NO_CONTENT);
        let (status, body) = create_key(&app, &auth, json!({"name": "c"})).await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        let c = format!("/api/v1/api-keys/{}", body["id"].as_str().unwrap());

        let (status, body) = put(&app, &auth, &a, json!({"is_active": true})).await;
        assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "{body}");
        assert_eq!(body["code"], "QUOTA_EXCEEDED", "{body}");
        assert_eq!(counts(db.pool(), &t.org_id).await.0, 2);
        // Edits that do not re-activate never touch the quota.
        let (status, body) = put(&app, &auth, &a, json!({"name": "a2"})).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["is_active"], false, "{body}");
        let (status, body) = put(&app, &auth, &c, json!({"is_active": false})).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let (status, body) = put(&app, &auth, &a, json!({"is_active": true})).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["is_active"], true, "{body}");

        // Schedules: S1 (1/1) -> pause S1 -> S2 (1/1) -> PUT S1 is_active.
        let schedule = |name: &str| {
            json!({
                "name": name,
                "cron_expression": "0 0 * * *",
                "queue_name": "w4-limits",
                "payload_template": {},
            })
        };
        let (status, body) = call(
            &app,
            "POST",
            "/api/v1/schedules",
            &[("Authorization", &auth)],
            Some(schedule("s1")),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        let s1_id = body["id"].as_str().unwrap().to_string();
        let s1 = format!("/api/v1/schedules/{s1_id}");
        let (status, body) = call(
            &app,
            "POST",
            &format!("{s1}/pause"),
            &[("Authorization", &auth)],
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let (status, body) = call(
            &app,
            "POST",
            "/api/v1/schedules",
            &[("Authorization", &auth)],
            Some(schedule("s2")),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        let s2 = format!("/api/v1/schedules/{}", body["id"].as_str().unwrap());

        let (status, body) = put(&app, &auth, &s1, json!({"is_active": true})).await;
        assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "{body}");
        assert_eq!(body["code"], "QUOTA_EXCEEDED", "{body}");
        assert_eq!(counts(db.pool(), &t.org_id).await.1, 1);

        // With the slot free, re-activation works and does not fire the stale,
        // long-past next_run_at on the next tick.
        assert_eq!(delete(&app, &auth, &s2).await, StatusCode::NO_CONTENT);
        sqlx::query("UPDATE schedules SET next_run_at = NOW() - INTERVAL '3 days' WHERE id = $1")
            .bind(&s1_id)
            .execute(db.pool())
            .await
            .unwrap();
        let (status, body) = put(&app, &auth, &s1, json!({"is_active": true})).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["is_active"], true, "{body}");
        let (future,): (bool,) =
            sqlx::query_as("SELECT next_run_at > NOW() FROM schedules WHERE id = $1")
                .bind(&s1_id)
                .fetch_one(db.pool())
                .await
                .unwrap();
        assert!(
            future,
            "re-activation must recompute next_run_at from now: {body}"
        );
    }

    // BEAUTH-09 / BEFLOW-07: at the cap, parallel re-activations never overshoot.
    #[tokio::test]
    async fn concurrent_put_reactivations_never_overshoot_the_cap() {
        let db = TestDatabase::new().await;
        let t = seed_tenant(db.pool()).await;
        set_plan(
            db.pool(),
            &t.org_id,
            "starter",
            Some(json!({"max_api_keys": 3, "max_schedules": 2})),
        )
        .await;
        let app = app(&db);
        let auth = bearer(&t.key);

        let mut keys = Vec::new();
        let mut schedules = Vec::new();
        for i in 0..6 {
            let key = uuid::Uuid::new_v4().to_string();
            sqlx::query(
                "INSERT INTO api_keys (id, organization_id, key_hash, key_prefix, name, queues, is_active, created_at) \
                 VALUES ($1, $2, 'unused-' || $1, 'sp_test_', $3, ARRAY[]::TEXT[], FALSE, NOW())",
            )
            .bind(&key)
            .bind(&t.org_id)
            .bind(format!("revoked-{i}"))
            .execute(db.pool())
            .await
            .unwrap();
            keys.push(key);
            let schedule = uuid::Uuid::new_v4().to_string();
            sqlx::query(
                "INSERT INTO schedules (id, organization_id, name, cron_expression, queue_name, is_active) \
                 VALUES ($1, $2, $3, '0 0 * * *', 'w4-limits', FALSE)",
            )
            .bind(&schedule)
            .bind(&t.org_id)
            .bind(format!("paused-{i}"))
            .execute(db.pool())
            .await
            .unwrap();
            schedules.push(schedule);
        }

        // The seed key holds 1 of 3 key slots; no schedule is active yet.
        for (resource, ids) in [("api-keys", &keys), ("schedules", &schedules)] {
            let mut tasks = Vec::new();
            for id in ids {
                let app = app.clone();
                let auth = auth.clone();
                let path = format!("/api/v1/{resource}/{id}");
                tasks.push(tokio::spawn(async move {
                    put(&app, &auth, &path, json!({"is_active": true})).await
                }));
            }
            let mut granted = 0;
            for task in tasks {
                let (status, body) = task.await.unwrap();
                match status {
                    StatusCode::OK => granted += 1,
                    StatusCode::TOO_MANY_REQUESTS => {}
                    other => panic!("{resource}: unexpected {other}: {body}"),
                }
            }
            assert_eq!(granted, 2, "{resource}: exactly the free slots are granted");
        }
        let (api_keys, schedules, _) = counts(db.pool(), &t.org_id).await;
        assert_eq!(api_keys, 3, "seed key + 2 re-activated");
        assert_eq!(schedules, 2);
    }

    // BEBILL-07 / BEBILL-26: the admin API rejects custom_limits the limit
    // checks or the retention sweep cannot use; -1 is "unlimited" on caps only.
    #[tokio::test]
    async fn admin_patch_validates_custom_limits() {
        let db = TestDatabase::new().await;
        let t = seed_tenant(db.pool()).await;
        let app = admin_app(&db);
        let path = format!("/api/v1/admin/organizations/{}", t.org_id);
        let admin = [("X-Admin-Key", ADMIN_KEY)];

        for bad in [
            json!({"job_retention_days": "90"}),
            json!({"job_retention_days": 1.5}),
            json!({"job_retention_days": -1}),
            json!({"history_retention_days": 99_999_999_999u64}),
            json!({"max_api_key": 3}),
            json!({"max_api_keys": -2}),
            json!([1]),
        ] {
            let (status, body) = call(
                &app,
                "PATCH",
                &path,
                &admin,
                Some(json!({"custom_limits": bad})),
            )
            .await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{bad}: {body}");
            assert_eq!(body["code"], "VALIDATION_ERROR", "{bad}: {body}");
        }
        let (stored,): (Option<Value>,) =
            sqlx::query_as("SELECT custom_limits FROM organizations WHERE id = $1")
                .bind(&t.org_id)
                .fetch_one(db.pool())
                .await
                .unwrap();
        assert_eq!(stored, None, "nothing invalid was stored");

        let good = json!({"max_api_keys": -1, "max_jobs_per_day": null, "job_retention_days": 45});
        let (status, body) = call(
            &app,
            "PATCH",
            &path,
            &admin,
            Some(json!({"custom_limits": good})),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["custom_limits"], good, "{body}");
        // Reset still works.
        let (status, body) = call(
            &app,
            "PATCH",
            &path,
            &admin,
            Some(json!({"custom_limits": null})),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(body["custom_limits"].is_null(), "{body}");
    }

    async fn completed_job(pool: &sqlx::PgPool, org_id: &str, days_ago: i32) -> String {
        let id = uuid::Uuid::new_v4().to_string();
        sqlx::query(
            "INSERT INTO jobs (id, organization_id, queue_name, payload, status, completed_at, created_at, updated_at) \
             VALUES ($1, $2, 'w4-ret', '{}'::JSONB, 'completed', NOW() - make_interval(days => $3), \
                     NOW() - make_interval(days => $3), NOW())",
        )
        .bind(&id)
        .bind(org_id)
        .bind(days_ago)
        .execute(pool)
        .await
        .expect("seed completed job");
        id
    }

    async fn job_exists(pool: &sqlx::PgPool, id: &str) -> bool {
        let (n,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM jobs WHERE id = $1")
            .bind(id)
            .fetch_one(pool)
            .await
            .unwrap();
        n == 1
    }

    // BEBILL-07: a bad stored retention override falls back to the plan default
    // instead of failing the sweep for every org, and -1 no longer purges.
    #[tokio::test]
    async fn retention_sweep_tolerates_bad_custom_limits() {
        let db = TestDatabase::new().await;
        let pool = db.pool();
        let good = seed_tenant(pool).await;
        let negative = seed_tenant(pool).await;
        let bad = seed_tenant(pool).await;
        set_plan(pool, &good.org_id, "free", None).await;
        set_plan(
            pool,
            &negative.org_id,
            "free",
            Some(json!({"job_retention_days": -1})),
        )
        .await;
        let scheduler = scheduler(&db);

        // -1 used to put the cutoff in the future: a 1-day-old job was purged.
        let old_good = completed_job(pool, &good.org_id, 30).await;
        let recent_negative = completed_job(pool, &negative.org_id, 1).await;
        scheduler.cleanup_stale_workers().await.expect("sweep");
        assert!(
            !job_exists(pool, &old_good).await,
            "free retention is 3 days"
        );
        assert!(
            job_exists(pool, &recent_negative).await,
            "-1 falls back to the plan's 3 days"
        );

        // One org's unusable values used to fail the sweep for everyone.
        for value in [json!("ninety"), json!(1.5), json!(99_999_999_999u64)] {
            set_plan(
                pool,
                &bad.org_id,
                "free",
                Some(json!({"job_retention_days": value, "history_retention_days": value})),
            )
            .await;
            let old_good = completed_job(pool, &good.org_id, 30).await;
            let recent_bad = completed_job(pool, &bad.org_id, 1).await;
            scheduler
                .cleanup_stale_workers()
                .await
                .unwrap_or_else(|e| panic!("sweep with {value}: {e:?}"));
            assert!(!job_exists(pool, &old_good).await, "{value}");
            assert!(job_exists(pool, &recent_bad).await, "{value}");
        }
        assert!(job_exists(pool, &recent_negative).await);
    }

    // BEFLOW-06 / BEAUTH-15: soft delete stops the org's schedules, outgoing
    // webhooks and incoming-webhook token; cron and ingest skip a deleted org
    // even when its rows predate that.
    #[tokio::test]
    async fn soft_deleted_org_stops_cron_webhooks_and_ingest() {
        let db = TestDatabase::new().await;
        let t = seed_tenant(db.pool()).await;
        let app = admin_app(&db);
        let auth = bearer(&t.key);

        let (status, body) = call(
            &app,
            "POST",
            "/api/v1/schedules",
            &[("Authorization", &auth)],
            Some(json!({
                "name": "cron",
                "cron_expression": "0 0 * * *",
                "queue_name": "w4-del",
                "payload_template": {},
            })),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        let schedule = body["id"].as_str().unwrap().to_string();
        sqlx::query(
            "INSERT INTO outgoing_webhooks (organization_id, name, url, events) \
             VALUES ($1, 'hook', 'https://example.com/hook', ARRAY['job.completed'])",
        )
        .bind(&t.org_id)
        .execute(db.pool())
        .await
        .unwrap();
        let set_token = || {
            sqlx::query(
                "UPDATE organizations \
                 SET settings = settings || jsonb_build_object('webhook_token', 'whk_w4_limits') \
                 WHERE id = $1",
            )
            .bind(&t.org_id)
            .execute(db.pool())
        };
        set_token().await.unwrap();
        let ingest_path = format!("/api/v1/webhooks/{}/custom", t.org_id);
        let ingest = || {
            call(
                &app,
                "POST",
                &ingest_path,
                &[("X-Webhook-Token", "whk_w4_limits")],
                Some(json!({"queue_name": "w4-del", "payload": {"n": 1}})),
            )
        };
        let (status, body) = ingest().await;
        assert_eq!(status, StatusCode::OK, "{body}");

        let (status, body) = call(
            &app,
            "DELETE",
            &format!("/api/v1/admin/organizations/{}", t.org_id),
            &[("X-Admin-Key", ADMIN_KEY)],
            None,
        )
        .await;
        assert_eq!(status, StatusCode::NO_CONTENT, "{body}");

        let (schedule_active, hooks_enabled, has_token): (bool, i64, bool) = sqlx::query_as(
            "SELECT \
               (SELECT is_active FROM schedules WHERE id = $2), \
               (SELECT COUNT(*) FROM outgoing_webhooks WHERE organization_id = $1 AND enabled), \
               (SELECT settings ? 'webhook_token' FROM organizations WHERE id = $1)",
        )
        .bind(&t.org_id)
        .bind(&schedule)
        .fetch_one(db.pool())
        .await
        .unwrap();
        assert!(!schedule_active, "soft delete pauses schedules");
        assert_eq!(hooks_enabled, 0, "soft delete disables outgoing webhooks");
        assert!(!has_token, "soft delete clears the incoming-webhook token");

        // An org soft-deleted before this release still has active schedules
        // and its token: neither cron nor ingest may act for it.
        sqlx::query(
            "UPDATE schedules SET is_active = TRUE, next_run_at = NOW() - INTERVAL '1 minute' \
             WHERE id = $1",
        )
        .bind(&schedule)
        .execute(db.pool())
        .await
        .unwrap();
        set_token().await.unwrap();
        let (jobs_before,): (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM jobs WHERE organization_id = $1")
                .bind(&t.org_id)
                .fetch_one(db.pool())
                .await
                .unwrap();

        scheduler(&db)
            .process_cron_schedules()
            .await
            .expect("cron tick");
        let (jobs_after, still_due): (i64, bool) = sqlx::query_as(
            "SELECT (SELECT COUNT(*) FROM jobs WHERE organization_id = $1), \
                    (SELECT next_run_at < NOW() FROM schedules WHERE id = $2)",
        )
        .bind(&t.org_id)
        .bind(&schedule)
        .fetch_one(db.pool())
        .await
        .unwrap();
        assert_eq!(
            jobs_after, jobs_before,
            "cron must not enqueue for a deleted org"
        );
        assert!(still_due, "the deleted org's schedule was not processed");

        let (status, body) = ingest().await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    }

    async fn seed_workflows(pool: &sqlx::PgPool, org_id: &str, status: &str, n: i32) {
        sqlx::query(
            "INSERT INTO workflows (organization_id, name, status, completed_at) \
             SELECT $1, 'wf-' || g, $2, \
                    CASE WHEN $2 IN ('completed', 'failed', 'cancelled') THEN NOW() END \
             FROM generate_series(1, $3) AS g",
        )
        .bind(org_id)
        .bind(status)
        .bind(n)
        .execute(pool)
        .await
        .expect("seed workflows");
    }

    // BEBILL-10 / BEFLOW-17 (contract C5): max_workflows counts pending/running
    // workflows only; finished runs no longer hold a slot until retention.
    #[tokio::test]
    async fn max_workflows_counts_only_unfinished_workflows() {
        let db = TestDatabase::new().await;
        let t = seed_tenant(db.pool()).await; // starter: max_workflows 5
        let app = app(&db);
        let auth = bearer(&t.key);
        for (status, n) in [
            ("completed", 5),
            ("failed", 1),
            ("cancelled", 1),
            ("running", 1),
            ("pending", 1),
        ] {
            seed_workflows(db.pool(), &t.org_id, status, n).await;
        }
        assert_eq!(counts(db.pool(), &t.org_id).await.2, 2);

        let headers = [("Authorization", auth.as_str())];
        let create = || {
            call(
                &app,
                "POST",
                "/api/v1/workflows",
                &headers,
                Some(json!({
                    "name": "w4",
                    "jobs": [{"key": "a", "queue_name": "w4-wf", "payload": {}}],
                })),
            )
        };
        let (status, body) = create().await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(counts(db.pool(), &t.org_id).await.2, 3);

        // The cap still holds for unfinished workflows.
        seed_workflows(db.pool(), &t.org_id, "running", 2).await;
        let (status, body) = create().await;
        assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "{body}");
    }
}
// ---- wave 4 / authrl ----
// BEAUTH-10 / BEBILL-24 (client IP from the TCP peer, IPv6 per /64),
// BEAUTH-06 (verify budgets per client), BEAUTH-11 / FESF-15 (start throttle,
// Turnstile, undelivered codes), BEAUTH-24 (single-use codes).
mod wave4_authrl {
    use super::*;

    use std::net::SocketAddr;

    use axum::extract::ConnectInfo;
    use wiremock::matchers::{body_string_contains, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use spooled_backend::cache::RedisCache;
    use spooled_backend::config::{EmailProvider, RedisSettings};

    /// Router on Redis with `settings`. Keep the `TestRedis` alive.
    async fn redis_app(
        db: &TestDatabase,
        settings: Settings,
    ) -> (Router, RedisCache, super::common::TestRedis) {
        let redis = super::common::TestRedis::new().await;
        let cache = RedisCache::connect(&RedisSettings {
            url: redis.url.clone(),
            pool_size: 4,
        })
        .await
        .expect("connect redis");
        let app = router(AppState::new(
            Database::from_pool(Arc::clone(&db.pool)),
            Some(cache.clone()),
            Metrics::new(),
            settings,
        ));
        (app, cache, redis)
    }

    /// A public IPv6 /64 no other test uses; `host` picks an address in it.
    fn unique_prefix() -> String {
        let id = uuid::Uuid::new_v4().as_u128();
        format!("2001:db8:{:x}:{:x}", (id >> 16) as u16, id as u16)
    }

    fn in_prefix(prefix: &str, host: u16) -> String {
        format!("{prefix}::{host:x}")
    }

    /// [`call`] over a connection from `peer`, as `axum::serve` with
    /// `ConnectInfo` delivers it (main.rs). Returns the response headers too.
    async fn call_from(
        app: &Router,
        peer: &str,
        method: &str,
        uri: &str,
        headers: &[(&str, &str)],
        body: Option<Value>,
    ) -> (StatusCode, axum::http::HeaderMap, Value) {
        let mut req = Request::builder().method(method).uri(uri);
        for (name, value) in headers {
            req = req.header(*name, *value);
        }
        let mut req = match body {
            Some(b) => req
                .header("Content-Type", "application/json")
                .body(Body::from(b.to_string()))
                .unwrap(),
            None => req.body(Body::empty()).unwrap(),
        };
        let ip: std::net::IpAddr = peer.parse().expect("peer address");
        req.extensions_mut()
            .insert(ConnectInfo(SocketAddr::new(ip, 41000)));
        let res = app.clone().oneshot(req).await.expect("request");
        let status = res.status();
        let response_headers = res.headers().clone();
        let bytes = axum::body::to_bytes(res.into_body(), 1 << 20)
            .await
            .unwrap();
        let json = if bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&bytes)
                .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into()))
        };
        (status, response_headers, json)
    }

    fn unique_address(tag: &str) -> String {
        format!("{tag}-{}@example.com", uuid::Uuid::new_v4().simple())
    }

    fn retry_after(headers: &axum::http::HeaderMap) -> u64 {
        headers
            .get("Retry-After")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse().ok())
            .expect("Retry-After header")
    }

    async fn code_rows(pool: &sqlx::PgPool, email: &str) -> i64 {
        sqlx::query_scalar("SELECT COUNT(*) FROM email_login_codes WHERE email = $1")
            .bind(email)
            .fetch_one(pool)
            .await
            .expect("count codes")
    }

    // -----------------------------------------------------------------------
    // BEAUTH-10: without TRUSTED_CLIENT_IP_HEADER, CF-Connecting-IP picked the
    // bucket for any caller, so a client reaching the backend directly got a
    // fresh public-limiter bucket per request. Production's cloudflared talks
    // to the backend from the docker network and keeps per-client buckets.
    // -----------------------------------------------------------------------
    #[tokio::test]
    async fn w4_authrl_public_peer_cannot_choose_its_bucket_with_edge_headers() {
        let db = TestDatabase::new().await;
        let mut settings = Settings::load_for_testing();
        settings.rate_limit.requests_per_second = 1;
        settings.rate_limit.burst_size = 2;
        let (app, _cache, _redis) = redis_app(&db, settings).await;
        let uri = "/api/v1/auth/check-email?email=nobody-w4@example.com";

        // A public peer (203.0.113.9 in spirit; unique per run in a shared
        // Redis) naming a different client every time: one bucket, its own.
        let public_peer = in_prefix(&unique_prefix(), 9);
        let mut statuses = Vec::new();
        for _ in 0..4 {
            let spoofed = unique_client_ip();
            let (status, _, _) = call_from(
                &app,
                &public_peer,
                "GET",
                uri,
                &[("CF-Connecting-IP", &spoofed), ("X-Real-IP", &spoofed)],
                None,
            )
            .await;
            statuses.push(status);
        }
        assert!(
            statuses.contains(&StatusCode::TOO_MANY_REQUESTS),
            "a public peer must be limited on its own address: {statuses:?}"
        );

        // The tunnel container's address (docker bridge): the header is the
        // client, so distinct clients behind one proxy keep separate buckets.
        for _ in 0..4 {
            let client = unique_client_ip();
            let (status, _, body) = call_from(
                &app,
                "172.18.0.5",
                "GET",
                uri,
                &[("CF-Connecting-IP", &client)],
                None,
            )
            .await;
            assert_eq!(status, StatusCode::OK, "{body}");
        }
    }

    // BEAUTH-10: check-slug keys its limits on the TCP peer
    // (as do /auth/login and the failed-auth budget). A public peer naming another client in CF-Connecting-IP is still
    // counted on its own address; a docker-network peer is counted on the header.
    #[tokio::test]
    async fn w4_authrl_check_slug_budget_follows_the_peer() {
        use spooled_backend::api::middleware::client_ip::rate_limit_id;

        let db = TestDatabase::new().await;
        let (app, cache, _redis) = redis_app(&db, Settings::load_for_testing()).await;

        let public_peer = in_prefix(&unique_prefix(), 9);
        let spoofed = unique_client_ip();
        let (status, _, body) = call_from(
            &app,
            &public_peer,
            "GET",
            "/api/v1/organizations/check-slug?slug=w4-peer-slug",
            &[("CF-Connecting-IP", &spoofed)],
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let own = format!("check_slug:minute:{}", rate_limit_id(&public_peer));
        let named = format!("check_slug:minute:{}", rate_limit_id(&spoofed));
        assert!(
            cache.get(&own).await.unwrap().is_some(),
            "the public peer is counted on its own address"
        );
        assert!(
            cache.get(&named).await.unwrap().is_none(),
            "a public peer cannot move its count to the address it names"
        );

        let client = unique_client_ip();
        let (status, _, body) = call_from(
            &app,
            "172.18.0.5",
            "GET",
            "/api/v1/organizations/check-slug?slug=w4-peer-slug",
            &[("CF-Connecting-IP", &client)],
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let behind_proxy = format!("check_slug:minute:{}", rate_limit_id(&client));
        assert!(
            cache.get(&behind_proxy).await.unwrap().is_some(),
            "behind the tunnel the header names the client"
        );
    }

    // The real binary serves with ConnectInfo: a caller that sends no edge
    // header is keyed on its TCP address, not on the shared "unknown" bucket.
    #[tokio::test]
    async fn w4_authrl_the_server_keys_a_bare_client_on_its_tcp_address() {
        use std::time::{Duration, Instant};

        use super::wave3_runtime::{child_database_url, free_port, Backend};

        let db = TestDatabase::new().await;
        let url = child_database_url(&db).await;
        let redis = super::common::TestRedis::new().await;
        let cache = RedisCache::connect(&RedisSettings {
            url: redis.url.clone(),
            pool_size: 2,
        })
        .await
        .expect("connect redis");
        let bucket = "check_email:minute:127.0.0.1";
        cache.delete(bucket).await.unwrap();

        let backend = Backend::spawn_with_env(
            &url,
            free_port(),
            free_port(),
            free_port(),
            &[("REDIS_URL", redis.url.as_str())],
        );
        let deadline = Instant::now() + Duration::from_secs(45);
        while !matches!(backend.get("/health/live", None).await, Some((200, _))) {
            assert!(
                Instant::now() < deadline,
                "the backend never came up; output:\n{}",
                backend.output()
            );
            tokio::time::sleep(Duration::from_millis(250)).await;
        }

        let (status, body) = backend
            .get("/api/v1/auth/check-email?email=w4-peer@example.com", None)
            .await
            .expect("an answer");
        assert_eq!(status, 200, "{body}");
        assert!(
            cache.exists(bucket).await.unwrap(),
            "keyed on the TCP peer; output:\n{}",
            backend.output()
        );
        cache.delete(bucket).await.unwrap();
    }

    // -----------------------------------------------------------------------
    // BEAUTH-11 / FESF-15 / contract C2: POST /auth/email/start is limited per
    // client IP (429 RATE_LIMIT_EXCEEDED + Retry-After). BEBILL-24: an IPv6
    // client is one client per /64.
    // -----------------------------------------------------------------------
    #[tokio::test]
    async fn w4_authrl_email_start_is_limited_per_client_ip() {
        let db = TestDatabase::new().await;
        let (app, cache, _redis) = redis_app(&db, Settings::load_for_testing()).await;

        // Behind the tunnel, one client rotating addresses inside its /64
        // while asking for codes to different addresses.
        let prefix = unique_prefix();
        let mut statuses = Vec::new();
        let mut throttled = None;
        for host in 1..=4u16 {
            let client = in_prefix(&prefix, host);
            let (status, headers, body) = call_from(
                &app,
                "172.18.0.5",
                "POST",
                "/api/v1/auth/email/start",
                &[("CF-Connecting-IP", &client)],
                Some(json!({"email": unique_address("w4-start")})),
            )
            .await;
            statuses.push(status);
            if status == StatusCode::TOO_MANY_REQUESTS {
                throttled = Some((headers, body));
            }
        }
        assert_eq!(
            statuses,
            vec![
                StatusCode::OK,
                StatusCode::OK,
                StatusCode::OK,
                StatusCode::TOO_MANY_REQUESTS
            ],
            "burst of 3 per minute per /64"
        );
        let (headers, body) = throttled.unwrap();
        assert_eq!(body["code"], "RATE_LIMIT_EXCEEDED", "{body}");
        let wait = retry_after(&headers);
        assert!((1..=60).contains(&wait), "Retry-After {wait}");

        // Another client is unaffected.
        let (status, _, body) = call_from(
            &app,
            "172.18.0.5",
            "POST",
            "/api/v1/auth/email/start",
            &[("CF-Connecting-IP", &unique_client_ip())],
            Some(json!({"email": unique_address("w4-start")})),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");

        // The hourly budget: a client that already asked 10 times this hour.
        let hourly = in_prefix(&unique_prefix(), 1);
        cache
            .set(
                &format!("email_start:hour:{}", rate_limit_id(&hourly)),
                "10",
                3600,
            )
            .await
            .unwrap();
        let (status, headers, body) = call_from(
            &app,
            &hourly,
            "POST",
            "/api/v1/auth/email/start",
            &[],
            Some(json!({"email": unique_address("w4-start")})),
        )
        .await;
        assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "{body}");
        assert!(retry_after(&headers) > 60, "{headers:?}");
        assert!(
            body["message"].as_str().unwrap().contains("minutes"),
            "{body}"
        );
    }

    fn rate_limit_id(addr: &str) -> String {
        spooled_backend::api::middleware::client_ip::rate_limit_id(addr)
    }

    // -----------------------------------------------------------------------
    // Contract C2: with TURNSTILE_SECRET_KEY set, start needs a token that
    // Cloudflare accepts (400 CAPTCHA_FAILED otherwise); unset, nothing changes.
    // -----------------------------------------------------------------------
    #[tokio::test]
    async fn w4_authrl_turnstile_is_checked_only_when_configured() {
        let db = TestDatabase::new().await;
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/turnstile/v0/siteverify"))
            .and(body_string_contains("secret=turnstile-test-secret"))
            .and(body_string_contains("response=good-token"))
            .and(body_string_contains("remoteip=198.51.100.23"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"success": true})))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/turnstile/v0/siteverify"))
            .and(body_string_contains("response=bad-token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(
                json!({"success": false, "error-codes": ["invalid-input-response"]}),
            ))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/turnstile/v0/siteverify"))
            .and(body_string_contains("response=outage-token"))
            .respond_with(ResponseTemplate::new(503))
            .expect(1)
            .mount(&server)
            .await;

        // Off (production today): no token needed, Cloudflare never called.
        let off = app(&db);
        let (status, body) = call(
            &off,
            "POST",
            "/api/v1/auth/email/start",
            &[],
            Some(json!({"email": unique_address("w4-ts-off")})),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");

        let mut settings = Settings::load_for_testing();
        settings.turnstile.secret_key = Some("turnstile-test-secret".to_string());
        settings.turnstile.verify_url = format!("{}/turnstile/v0/siteverify", server.uri());
        let on = app_with(&db, settings);
        let start = |token: Option<&str>, email: &str| {
            let mut body = json!({"email": email});
            if let Some(token) = token {
                body["turnstile_token"] = json!(token);
            }
            let on = on.clone();
            async move {
                call(
                    &on,
                    "POST",
                    "/api/v1/auth/email/start",
                    &[("CF-Connecting-IP", "198.51.100.23")],
                    Some(body),
                )
                .await
            }
        };

        for token in [None, Some(""), Some("bad-token"), Some("outage-token")] {
            let email = unique_address("w4-ts-on");
            let (status, body) = start(token, &email).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{token:?}: {body}");
            assert_eq!(body["code"], "CAPTCHA_FAILED", "{token:?}: {body}");
            assert_eq!(
                code_rows(db.pool(), &email).await,
                0,
                "no code for {token:?}"
            );
        }

        let email = unique_address("w4-ts-on");
        let (status, body) = start(Some("good-token"), &email).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(code_rows(db.pool(), &email).await, 1);
        // Mock expectations (one call each) are checked when `server` drops.
    }

    // -----------------------------------------------------------------------
    // BEAUTH-11: the code row was stored before the mail went out, so a mail
    // provider outage spent the address's 5 codes per hour on codes nobody got.
    // -----------------------------------------------------------------------
    #[tokio::test]
    async fn w4_authrl_undelivered_codes_do_not_use_the_hourly_allowance() {
        let db = TestDatabase::new().await;
        let tenant = seed_tenant(db.pool()).await;
        // Resend without EMAIL_API_KEY: every send fails before any network call.
        let mut settings = Settings::load_for_testing();
        settings.email.provider = EmailProvider::Resend;
        settings.email.api_key = None;
        let broken = app_with(&db, settings);

        let email = unique_address("w4-outage");
        for _ in 0..6 {
            let (status, body) = call(
                &broken,
                "POST",
                "/api/v1/auth/email/start",
                &[],
                Some(json!({"email": email})),
            )
            .await;
            assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR, "{body}");
        }
        assert_eq!(code_rows(db.pool(), &email).await, 0);

        // The provider is back: the address still gets its code.
        let (status, body) = call(
            &app(&db),
            "POST",
            "/api/v1/auth/email/start",
            &[],
            Some(json!({"email": email})),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(code_rows(db.pool(), &email).await, 1);

        // Same for the billing-email confirmation code.
        let address = unique_address("w4-billing");
        let (status, body) = call(
            &app(&db),
            "PUT",
            &format!("/api/v1/organizations/{}", tenant.org_id),
            &[("Authorization", &bearer(&tenant.key))],
            Some(json!({"billing_email": address})),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let (status, body) = call(
            &broken,
            "POST",
            "/api/v1/organizations/billing-email/verification",
            &[("Authorization", &bearer(&tenant.key))],
            None,
        )
        .await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR, "{body}");
        assert_eq!(code_rows(db.pool(), &address).await, 0);
    }

    /// Org email of a seeded tenant, and a fresh login code for it.
    async fn tenant_with_code(db: &TestDatabase, app: &Router) -> (String, String) {
        let tenant = seed_tenant(db.pool()).await;
        let (email,): (String,) =
            sqlx::query_as("SELECT billing_email FROM organizations WHERE id = $1")
                .bind(&tenant.org_id)
                .fetch_one(db.pool())
                .await
                .unwrap();
        let (status, body) = call(
            app,
            "POST",
            "/api/v1/auth/email/start",
            &[("CF-Connecting-IP", &unique_client_ip())],
            Some(json!({"email": email})),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let (code, org) = latest_code(db.pool(), &email).await;
        assert_eq!(org.as_deref(), Some(tenant.org_id.as_str()));
        (email, code)
    }

    fn wrong_code(code: &str) -> &'static str {
        if code == "000000" {
            "111111"
        } else {
            "000000"
        }
    }

    async fn verify_from(app: &Router, ip: &str, email: &str, code: &str) -> (StatusCode, Value) {
        call(
            app,
            "POST",
            "/api/v1/auth/email/verify",
            &[("CF-Connecting-IP", ip)],
            Some(json!({"email": email, "code": code})),
        )
        .await
    }

    // -----------------------------------------------------------------------
    // BEAUTH-06: five wrong guesses by anyone exhausted the address's budget
    // and the owner's correct code was refused for the rest of the window.
    // -----------------------------------------------------------------------
    #[tokio::test]
    async fn w4_authrl_a_strangers_wrong_guesses_do_not_lock_the_owner_out() {
        let db = TestDatabase::new().await;
        let (app, _cache, _redis) = redis_app(&db, Settings::load_for_testing()).await;
        let (email, code) = tenant_with_code(&db, &app).await;
        let wrong = wrong_code(&code);

        let stranger = unique_client_ip();
        for _ in 0..5 {
            let (status, body) = verify_from(&app, &stranger, &email, wrong).await;
            assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");
            assert_eq!(body["message"], "Invalid code", "{body}");
        }
        // The stranger is out of guesses, and is told for how long.
        let (status, body) = verify_from(&app, &stranger, &email, wrong).await;
        assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "{body}");
        let message = body["message"].as_str().unwrap();
        assert!(message.contains("try again in"), "{message}");
        assert!(message.contains("minute"), "{message}");

        // The owner, from their own network, signs in with the code they got.
        let (status, body) = verify_from(&app, &unique_client_ip(), &email, &code).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["type"], "login", "{body}");
    }

    // The per-address ceiling makes a guesser spread over many clients (and
    // everyone else) wait out the window; it never invalidates the codes.
    #[tokio::test]
    async fn w4_authrl_the_per_address_ceiling_slows_without_burning_codes() {
        let db = TestDatabase::new().await;
        let (app, _cache, _redis) = redis_app(&db, Settings::load_for_testing()).await;
        let (email, code) = tenant_with_code(&db, &app).await;
        let wrong = wrong_code(&code);

        for _ in 0..4 {
            let client = unique_client_ip();
            for _ in 0..5 {
                let (status, body) = verify_from(&app, &client, &email, wrong).await;
                assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");
            }
        }
        let (status, body) = verify_from(&app, &unique_client_ip(), &email, &code).await;
        assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "{body}");
        let message = body["message"].as_str().unwrap();
        assert!(message.contains("for this address"), "{message}");
        assert!(message.contains("minute"), "{message}");

        let (used, attempts): (Option<chrono::DateTime<chrono::Utc>>, i32) = sqlx::query_as(
            "SELECT used_at, attempts FROM email_login_codes WHERE email = $1 AND code = $2",
        )
        .bind(&email)
        .bind(&code)
        .fetch_one(db.pool())
        .await
        .unwrap();
        assert!(used.is_none(), "the code must survive the ceiling");
        assert_eq!(attempts, 20);
    }

    // -----------------------------------------------------------------------
    // BEAUTH-24: marking a code used was not conditional, so parallel verifies
    // of one code each minted a session.
    // -----------------------------------------------------------------------
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn w4_authrl_parallel_verifies_of_one_code_mint_one_session() {
        let db = TestDatabase::new().await;
        let app = app(&db);
        for round in 0..3 {
            let (email, code) = tenant_with_code(&db, &app).await;
            let barrier = Arc::new(tokio::sync::Barrier::new(5));
            let mut tasks = Vec::new();
            for _ in 0..5 {
                let (app, email, code, barrier) = (
                    app.clone(),
                    email.clone(),
                    code.clone(),
                    Arc::clone(&barrier),
                );
                tasks.push(tokio::spawn(async move {
                    barrier.wait().await;
                    verify_from(&app, &unique_client_ip(), &email, &code).await
                }));
            }
            let mut sessions = 0;
            for task in tasks {
                let (status, body) = task.await.unwrap();
                if status == StatusCode::OK {
                    assert_eq!(body["type"], "login", "{body}");
                    sessions += 1;
                } else {
                    assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");
                }
            }
            assert_eq!(sessions, 1, "round {round}: one code, one session");
        }
    }
}
// ---- wave 4 / review fixes ----
// W4-01: workflow retry pays the max_workflows cap, now that only pending/running
// workflows count against it.
mod wave4_review {
    use super::*;

    /// Create a one-job workflow through the API; returns its id.
    async fn create_workflow(app: &Router, auth: &str, name: &str) -> String {
        let (status, body) = call(
            app,
            "POST",
            "/api/v1/workflows",
            &[("Authorization", auth)],
            Some(json!({
                "name": name,
                "jobs": [{"key": "a", "queue_name": "w4r-wf", "payload": {}}],
            })),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        body["workflow_id"]
            .as_str()
            .expect("workflow_id")
            .to_string()
    }

    /// Fail the workflow's job and the workflow, as a failed run leaves them.
    async fn fail_workflow(pool: &sqlx::PgPool, workflow_id: &str) {
        sqlx::query(
            "UPDATE jobs SET status = 'failed', last_error = 'boom', completed_at = NOW(), \
             updated_at = NOW() WHERE workflow_id = $1",
        )
        .bind(workflow_id)
        .execute(pool)
        .await
        .expect("fail jobs");
        sqlx::query(
            "UPDATE workflows SET status = 'failed', completed_at = COALESCE(completed_at, NOW()) \
             WHERE id = $1",
        )
        .bind(workflow_id)
        .execute(pool)
        .await
        .expect("fail workflow");
    }

    /// (workflow status, its job statuses).
    async fn workflow_state(pool: &sqlx::PgPool, workflow_id: &str) -> (String, Vec<String>) {
        let status: String = sqlx::query_scalar("SELECT status FROM workflows WHERE id = $1")
            .bind(workflow_id)
            .fetch_one(pool)
            .await
            .unwrap();
        let jobs: Vec<String> =
            sqlx::query_scalar("SELECT status FROM jobs WHERE workflow_id = $1 ORDER BY id")
                .bind(workflow_id)
                .fetch_all(pool)
                .await
                .unwrap();
        (status, jobs)
    }

    fn state(workflow: &str, job: &str) -> (String, Vec<String>) {
        (workflow.to_string(), vec![job.to_string()])
    }

    async fn active_workflows(pool: &sqlx::PgPool, org_id: &str) -> i64 {
        sqlx::query_scalar("SELECT workflows FROM get_org_resource_counts($1)")
            .bind(org_id)
            .fetch_one(pool)
            .await
            .unwrap()
    }

    async fn retry(app: &Router, auth: &str, workflow_id: &str) -> (StatusCode, Value) {
        call(
            app,
            "POST",
            &format!("/api/v1/workflows/{workflow_id}/retry"),
            &[("Authorization", auth)],
            None,
        )
        .await
    }

    // W4-01: retry flipped a failed workflow back to running with no cap check, so
    // fail 5 -> create 5 -> retry 5 left 10 active workflows on a cap of 5.
    #[tokio::test]
    async fn w4_review_workflow_retry_pays_the_workflows_cap() {
        let db = TestDatabase::new().await;
        let t = seed_tenant(db.pool()).await; // starter: max_workflows 5
        let app = app(&db);
        let auth = bearer(&t.key);

        let mut failed = Vec::new();
        for i in 0..5 {
            let id = create_workflow(&app, &auth, &format!("first-{i}")).await;
            fail_workflow(db.pool(), &id).await;
            failed.push(id);
        }
        assert_eq!(active_workflows(db.pool(), &t.org_id).await, 0);
        let mut running = Vec::new();
        for i in 0..5 {
            running.push(create_workflow(&app, &auth, &format!("second-{i}")).await);
        }
        assert_eq!(active_workflows(db.pool(), &t.org_id).await, 5);

        // At the cap: every retry is refused and nothing is reset.
        for id in &failed {
            let (status, body) = retry(&app, &auth, id).await;
            assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "{body}");
            assert_eq!(
                workflow_state(db.pool(), id).await,
                state("failed", "failed"),
                "a refused retry must not reset the workflow or its jobs"
            );
        }
        assert_eq!(active_workflows(db.pool(), &t.org_id).await, 5);

        // One run finishes, freeing a slot: exactly one retry fits.
        sqlx::query(
            "UPDATE workflows SET status = 'completed', completed_at = NOW() WHERE id = $1",
        )
        .bind(&running[0])
        .execute(db.pool())
        .await
        .unwrap();
        let (status, body) = retry(&app, &auth, &failed[0]).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(
            workflow_state(db.pool(), &failed[0]).await,
            state("running", "pending")
        );
        let (status, body) = retry(&app, &auth, &failed[1]).await;
        assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "{body}");
        assert_eq!(active_workflows(db.pool(), &t.org_id).await, 5);

        // A plan without workflows cannot restart one left over from a paid plan.
        sqlx::query(
            "UPDATE workflows SET status = 'cancelled', completed_at = NOW() \
             WHERE organization_id = $1 AND status IN ('pending', 'running')",
        )
        .bind(&t.org_id)
        .execute(db.pool())
        .await
        .unwrap();
        sqlx::query("UPDATE organizations SET plan_tier = 'free' WHERE id = $1")
            .bind(&t.org_id)
            .execute(db.pool())
            .await
            .unwrap();
        let (status, body) = retry(&app, &auth, &failed[1]).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
        assert_eq!(body["error"], "feature_disabled", "{body}");
        assert_eq!(
            workflow_state(db.pool(), &failed[1]).await,
            state("failed", "failed")
        );
        assert_eq!(active_workflows(db.pool(), &t.org_id).await, 0);
    }

    // W4-01: parallel retries of different failed workflows never overshoot the cap.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn w4_review_parallel_workflow_retries_never_overshoot_the_cap() {
        let db = TestDatabase::new().await;
        let t = seed_tenant(db.pool()).await;
        sqlx::query("UPDATE organizations SET custom_limits = $2 WHERE id = $1")
            .bind(&t.org_id)
            .bind(json!({"max_workflows": 2}))
            .execute(db.pool())
            .await
            .unwrap();
        let app = app(&db);
        let auth = bearer(&t.key);

        let mut failed = Vec::new();
        for i in 0..5 {
            let id = create_workflow(&app, &auth, &format!("wf-{i}")).await;
            fail_workflow(db.pool(), &id).await;
            failed.push(id);
        }
        assert_eq!(active_workflows(db.pool(), &t.org_id).await, 0);

        let barrier = Arc::new(tokio::sync::Barrier::new(failed.len()));
        let mut tasks = Vec::new();
        for id in &failed {
            let (app, auth, id, barrier) =
                (app.clone(), auth.clone(), id.clone(), Arc::clone(&barrier));
            tasks.push(tokio::spawn(async move {
                barrier.wait().await;
                retry(&app, &auth, &id).await
            }));
        }
        let mut granted = 0;
        for task in tasks {
            let (status, body) = task.await.unwrap();
            match status {
                StatusCode::OK => granted += 1,
                StatusCode::TOO_MANY_REQUESTS => {}
                other => panic!("unexpected {other}: {body}"),
            }
        }
        assert_eq!(granted, 2, "two free slots, two retries");
        assert_eq!(active_workflows(db.pool(), &t.org_id).await, 2);
    }
}
