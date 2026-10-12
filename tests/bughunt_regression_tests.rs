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
    // Without a verified address of its own, the squatter's change replaces the
    // billing email directly. (A verified one now stays, with the new address
    // waiting in `pending_billing_email`: see wave 5 / keys, W1-14.)
    sqlx::query("UPDATE organizations SET billing_email_verified_at = NULL WHERE id = $1")
        .bind(&squatter.org_id)
        .execute(db.pool())
        .await
        .unwrap();

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
    // The dead-letter is the server's decision, not a failure the worker
    // reported, so the claiming worker's jobs_failed stays 0 (wave 8 review).
    #[tokio::test]
    async fn grpc_claims_dead_letter_a_job_too_large_for_one_message() {
        let db = TestDatabase::new().await;
        let t = seed_tenant(db.pool()).await;
        let service = queue_service(&db, None);
        let huge = "y".repeat(4_300_000);
        sqlx::query(
            "INSERT INTO workers (id, organization_id, queue_name, hostname) \
             VALUES ('w-huge', $1, 'h1', 'w-huge-host')",
        )
        .bind(&t.org_id)
        .execute(db.pool())
        .await
        .unwrap();

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
            let counters: (i64, i64) = sqlx::query_as(
                "SELECT jobs_processed, jobs_failed FROM workers WHERE id = 'w-huge'",
            )
            .fetch_one(db.pool())
            .await
            .unwrap();
            assert_eq!(counters, (0, 0), "{path}: not a failure of the worker");
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
// ---- wave 5 / jobsapi ----
// Job and queue REST semantics: claim index, queue config and bulk validation,
// idempotency, queue stats, list filters/order, worker calls, DLQ, and
// retrying a workflow step.
mod w5_jobsapi {
    use super::*;

    async fn post(app: &Router, auth: &str, uri: &str, body: Value) -> (StatusCode, Value) {
        call(app, "POST", uri, &[("Authorization", auth)], Some(body)).await
    }

    async fn get(app: &Router, auth: &str, uri: &str) -> (StatusCode, Value) {
        call(app, "GET", uri, &[("Authorization", auth)], None).await
    }

    async fn register_worker(app: &Router, auth: &str, queue: &str) -> String {
        let (status, body) = post(
            app,
            auth,
            "/api/v1/workers/register",
            json!({"queue_name": queue, "hostname": "w5-host"}),
        )
        .await;
        assert!(status.is_success(), "{body}");
        body["id"]
            .as_str()
            .or_else(|| body["worker_id"].as_str())
            .expect("worker id")
            .to_string()
    }

    /// Claim one job; returns (job id, lease id).
    async fn claim_one(app: &Router, auth: &str, queue: &str, worker: &str) -> (String, String) {
        let (status, body) = post(
            app,
            auth,
            "/api/v1/jobs/claim",
            json!({"queue_name": queue, "worker_id": worker, "limit": 1}),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let job = &body["jobs"][0];
        (
            job["id"].as_str().expect("a claimable job").to_string(),
            job["lease_id"].as_str().unwrap().to_string(),
        )
    }

    async fn complete(app: &Router, auth: &str, job: &str, worker: &str, lease: &str) {
        let (status, body) = post(
            app,
            auth,
            &format!("/api/v1/jobs/{job}/complete"),
            json!({"worker_id": worker, "lease_id": lease}),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
    }

    async fn job_status(pool: &sqlx::PgPool, job: &str) -> String {
        sqlx::query_scalar("SELECT status FROM jobs WHERE id = $1")
            .bind(job)
            .fetch_one(pool)
            .await
            .unwrap()
    }

    async fn workflow_status(pool: &sqlx::PgPool, workflow: &str) -> String {
        sqlx::query_scalar("SELECT status FROM workflows WHERE id = $1")
            .bind(workflow)
            .fetch_one(pool)
            .await
            .unwrap()
    }

    /// Move a job's `updated_at` (for a dead-lettered job: when it was
    /// dead-lettered) `days` into the past. The `jobs_updated_at` trigger stamps
    /// every UPDATE with the current time, so it is off for this one statement.
    async fn backdate_updated_at(pool: &sqlx::PgPool, job: &str, days: i32) {
        let mut tx = pool.begin().await.unwrap();
        sqlx::query("ALTER TABLE jobs DISABLE TRIGGER jobs_updated_at")
            .execute(&mut *tx)
            .await
            .unwrap();
        sqlx::query("UPDATE jobs SET updated_at = NOW() - make_interval(days => $2) WHERE id = $1")
            .bind(job)
            .bind(days)
            .execute(&mut *tx)
            .await
            .unwrap();
        sqlx::query("ALTER TABLE jobs ENABLE TRIGGER jobs_updated_at")
            .execute(&mut *tx)
            .await
            .unwrap();
        tx.commit().await.unwrap();
    }

    async fn set_custom_limits(pool: &sqlx::PgPool, org_id: &str, limits: Value) {
        sqlx::query("UPDATE organizations SET custom_limits = $2 WHERE id = $1")
            .bind(org_id)
            .bind(limits)
            .execute(pool)
            .await
            .unwrap();
    }

    // BEJOB-03: the claim query walks idx_jobs_claim instead of sorting the queue.
    #[tokio::test]
    async fn w5_claim_query_uses_the_claim_index() {
        let db = TestDatabase::new().await;
        let t = seed_tenant(db.pool()).await;
        sqlx::query(
            "INSERT INTO jobs (id, organization_id, queue_name, status, payload, priority, created_at, updated_at) \
             SELECT gen_random_uuid()::TEXT, $1, CASE WHEN g % 4 = 0 THEN 'other' ELSE 'big' END, \
                    CASE WHEN g % 10 = 0 THEN 'completed' ELSE 'pending' END, '{}'::JSONB, \
                    (g % 7) - 3, NOW() - make_interval(secs => g), NOW() \
             FROM generate_series(1, 20000) AS g",
        )
        .bind(&t.org_id)
        .execute(db.pool())
        .await
        .unwrap();
        sqlx::query("ANALYZE jobs")
            .execute(db.pool())
            .await
            .unwrap();

        let now = chrono::Utc::now();
        let plan: Vec<String> = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
            "EXPLAIN {}",
            spooled_backend::queue::CLAIM_BATCH_SQL
        )))
        .bind("w5-worker")
        .bind("w5-lease")
        .bind(now + chrono::Duration::seconds(30))
        .bind(now)
        .bind(&t.org_id)
        .bind("big")
        .bind(10_i32)
        .bind(30_i32)
        .fetch_all(db.pool())
        .await
        .unwrap();
        let plan = plan.join("\n");
        assert!(plan.contains("idx_jobs_claim"), "claim plan:\n{plan}");
    }

    // BEJOB-03: an interrupted concurrent build leaves an INVALID index that
    // IF NOT EXISTS skips; the repair migration rebuilds it.
    #[tokio::test]
    async fn w5_claim_index_repair_rebuilds_an_invalid_index() {
        let db = TestDatabase::new().await;
        let valid = |pool: sqlx::PgPool| async move {
            let valid: bool = sqlx::query_scalar(
                "SELECT indisvalid FROM pg_index WHERE indexrelid = 'idx_jobs_claim'::regclass",
            )
            .fetch_one(&pool)
            .await
            .unwrap();
            valid
        };
        assert!(valid(db.pool().clone()).await);
        sqlx::query(
            "UPDATE pg_index SET indisvalid = FALSE WHERE indexrelid = 'idx_jobs_claim'::regclass",
        )
        .execute(db.pool())
        .await
        .unwrap();
        assert!(!valid(db.pool().clone()).await);

        let mut tx = db.pool().begin().await.unwrap();
        sqlx::raw_sql(include_str!(
            "../migrations/20261013100001_job_claim_index_repair.sql"
        ))
        .execute(&mut *tx)
        .await
        .unwrap();
        tx.commit().await.unwrap();
        assert!(valid(db.pool().clone()).await);
    }

    // BEJOB-04: queue defaults are validated, and so is the path's queue name.
    #[tokio::test]
    async fn w5_queue_config_rejects_out_of_range_defaults_and_bad_names() {
        let db = TestDatabase::new().await;
        let t = seed_tenant(db.pool()).await;
        let app = app(&db);
        let auth = bearer(&t.key);

        for body in [
            json!({"default_timeout": 172800}),
            json!({"default_timeout": 0}),
            json!({"max_retries": -5}),
            json!({"max_retries": 101}),
            json!({"rate_limit": 0}),
        ] {
            let (status, resp) = call(
                &app,
                "PUT",
                "/api/v1/queues/reports/config",
                &[("Authorization", &auth)],
                Some(body.clone()),
            )
            .await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{body} -> {resp}");
        }
        for name in ["bad%20name", "-dash", &"q".repeat(101)] {
            let (status, resp) = call(
                &app,
                "PUT",
                &format!("/api/v1/queues/{name}/config"),
                &[("Authorization", &auth)],
                Some(json!({"max_retries": 2})),
            )
            .await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{name} -> {resp}");
        }
        let (configured,): (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM queue_config WHERE organization_id = $1")
                .bind(&t.org_id)
                .fetch_one(db.pool())
                .await
                .unwrap();
        assert_eq!(configured, 0, "rejected configs must not be stored");

        // An in-range config is stored and new jobs take it.
        let (status, resp) = call(
            &app,
            "PUT",
            "/api/v1/queues/reports/config",
            &[("Authorization", &auth)],
            Some(json!({"default_timeout": 600, "max_retries": 7})),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{resp}");
        let (status, resp) = post(
            &app,
            &auth,
            "/api/v1/jobs",
            json!({"queue_name": "reports", "payload": {}}),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{resp}");
        let (timeout, retries): (i32, i32) =
            sqlx::query_as("SELECT timeout_seconds, max_retries FROM jobs WHERE id = $1")
                .bind(resp["id"].as_str().unwrap())
                .fetch_one(db.pool())
                .await
                .unwrap();
        assert_eq!((timeout, retries), (600, 7));

        // The table holds the same ranges.
        let err = sqlx::query(
            "UPDATE queue_config SET default_timeout = 172800 WHERE organization_id = $1",
        )
        .bind(&t.org_id)
        .execute(db.pool())
        .await
        .expect_err("out-of-range default_timeout must violate a CHECK");
        assert!(
            err.to_string()
                .contains("queue_config_default_timeout_check"),
            "{err}"
        );
    }

    // BEJOB-05: bulk items are validated one by one.
    #[tokio::test]
    async fn w5_bulk_reports_invalid_items_per_index_and_creates_the_rest() {
        let db = TestDatabase::new().await;
        let t = seed_tenant(db.pool()).await;
        let app = app(&db);
        let auth = bearer(&t.key);

        let (status, body) = post(
            &app,
            &auth,
            "/api/v1/jobs/bulk",
            json!({"queue_name": "bq", "jobs": [
                {"payload": {"n": 0}},
                {"payload": {"n": 1}, "priority": 500},
                {"payload": {"n": 2}, "idempotency_key": "k".repeat(300)},
                {"payload": {"n": 3, "bad": "nul\u{0}byte"}},
                {"payload": {"n": 4}, "priority": -100},
            ]}),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let ok: Vec<u64> = body["succeeded"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["index"].as_u64().unwrap())
            .collect();
        assert_eq!(ok, vec![0, 4], "{body}");
        let failed: Vec<(u64, String)> = body["failed"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| {
                (
                    r["index"].as_u64().unwrap(),
                    r["error"].as_str().unwrap().to_string(),
                )
            })
            .collect();
        assert_eq!(
            failed.iter().map(|(i, _)| *i).collect::<Vec<_>>(),
            vec![1, 2, 3],
            "{body}"
        );
        assert!(failed[0].1.contains("Priority"), "{body}");
        assert!(failed[1].1.contains("Idempotency key"), "{body}");
        assert!(failed[2].1.contains("NUL"), "{body}");
        assert_eq!(body["success_count"], 2, "{body}");
        assert_eq!(body["failure_count"], 3, "{body}");
        let (created,): (i64,) = sqlx::query_as(
            "SELECT COUNT(*) FROM jobs WHERE organization_id = $1 AND queue_name = 'bq'",
        )
        .bind(&t.org_id)
        .fetch_one(db.pool())
        .await
        .unwrap();
        assert_eq!(created, 2);

        // Nothing valid in the batch, or invalid batch defaults: a 4xx, not 200.
        let (status, body) = post(
            &app,
            &auth,
            "/api/v1/jobs/bulk",
            json!({"queue_name": "bq", "jobs": [{"payload": {}, "priority": 101}]}),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert!(
            body["message"].as_str().unwrap_or("").contains("jobs[0]"),
            "{body}"
        );
        for defaults in [
            json!({"default_timeout_seconds": 0}),
            json!({"default_priority": 500}),
            json!({"default_max_retries": -1}),
        ] {
            let mut request = json!({"queue_name": "bq", "jobs": [{"payload": {}}]});
            request
                .as_object_mut()
                .unwrap()
                .extend(defaults.as_object().unwrap().clone());
            let (status, body) = post(&app, &auth, "/api/v1/jobs/bulk", request).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{defaults} -> {body}");
        }
    }

    // BEJOB-06: a blank idempotency key is no key at all.
    #[tokio::test]
    async fn w5_blank_idempotency_key_is_no_key() {
        let db = TestDatabase::new().await;
        let t = seed_tenant(db.pool()).await;
        let app = app(&db);
        let auth = bearer(&t.key);

        let mut ids = std::collections::HashSet::new();
        for key in ["", "", "   "] {
            let (status, body) = post(
                &app,
                &auth,
                "/api/v1/jobs",
                json!({"queue_name": "iq", "payload": {}, "idempotency_key": key}),
            )
            .await;
            assert_eq!(status, StatusCode::CREATED, "{body}");
            assert_eq!(body["created"], true, "{body}");
            ids.insert(body["id"].as_str().unwrap().to_string());
        }
        assert_eq!(ids.len(), 3, "every blank-key job is its own job");

        let (status, body) = post(
            &app,
            &auth,
            "/api/v1/jobs/bulk",
            json!({"queue_name": "iq", "jobs": [
                {"payload": {"n": 1}, "idempotency_key": ""},
                {"payload": {"n": 2}, "idempotency_key": ""},
                {"payload": {"n": 3}, "idempotency_key": " "},
            ]}),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["success_count"], 3, "{body}");
        let created: Vec<bool> = body["succeeded"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["created"].as_bool().unwrap())
            .collect();
        assert_eq!(created, vec![true, true, true], "{body}");
        let (null_keys,): (i64,) = sqlx::query_as(
            "SELECT COUNT(*) FROM jobs WHERE organization_id = $1 AND idempotency_key IS NULL",
        )
        .bind(&t.org_id)
        .fetch_one(db.pool())
        .await
        .unwrap();
        assert_eq!(null_keys, 6);
    }

    // BEJOB-07: a replay is answered before the quota checks and charges nothing.
    #[tokio::test]
    async fn w5_idempotent_replay_is_answered_before_quota_checks() {
        let db = TestDatabase::new().await;
        let t = seed_tenant(db.pool()).await;
        let app = app(&db);
        let auth = bearer(&t.key);
        set_custom_limits(
            db.pool(),
            &t.org_id,
            json!({"max_active_jobs": 2, "max_jobs_per_day": 2}),
        )
        .await;

        let (status, first) = post(
            &app,
            &auth,
            "/api/v1/jobs",
            json!({"queue_name": "rq", "payload": {}, "idempotency_key": "order-1"}),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{first}");
        let (status, body) = post(
            &app,
            &auth,
            "/api/v1/jobs/bulk",
            json!({"queue_name": "rq", "jobs": [{"payload": {}, "idempotency_key": "order-2"}]}),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["success_count"], 1, "{body}");
        let second = body["succeeded"][0]["job_id"].as_str().unwrap().to_string();

        // Both caps are now full; replays still answer with the existing jobs.
        for _ in 0..2 {
            let (status, body) = post(
                &app,
                &auth,
                "/api/v1/jobs",
                json!({"queue_name": "rq", "payload": {}, "idempotency_key": "order-1"}),
            )
            .await;
            assert_eq!(status, StatusCode::OK, "{body}");
            assert_eq!(body["id"], first["id"], "{body}");
            assert_eq!(body["created"], false, "{body}");

            let (status, body) = post(
                &app,
                &auth,
                "/api/v1/jobs/bulk",
                json!({"queue_name": "rq", "jobs": [
                    {"payload": {}, "idempotency_key": "order-1"},
                    {"payload": {}, "idempotency_key": "order-2"},
                ]}),
            )
            .await;
            assert_eq!(status, StatusCode::OK, "{body}");
            assert_eq!(body["succeeded"][0]["job_id"], first["id"], "{body}");
            assert_eq!(body["succeeded"][1]["job_id"], second.as_str(), "{body}");
            assert_eq!(body["succeeded"][0]["created"], false, "{body}");
            assert_eq!(body["succeeded"][1]["created"], false, "{body}");
        }

        // A genuinely new job is still refused at the cap, and replays charged nothing.
        let (status, body) = post(
            &app,
            &auth,
            "/api/v1/jobs",
            json!({"queue_name": "rq", "payload": {}, "idempotency_key": "order-3"}),
        )
        .await;
        assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "{body}");
        let (jobs_today,): (i64,) =
            sqlx::query_as("SELECT jobs_today FROM get_org_resource_counts($1)")
                .bind(&t.org_id)
                .fetch_one(db.pool())
                .await
                .unwrap();
        assert_eq!(jobs_today, 2);
    }

    // BEJOB-08 / QW5: queue stats count dead-lettered jobs.
    #[tokio::test]
    async fn w5_queue_stats_count_dead_lettered_jobs() {
        let db = TestDatabase::new().await;
        let t = seed_tenant(db.pool()).await;
        let app = app(&db);
        let auth = bearer(&t.key);
        let worker = register_worker(&app, &auth, "sq").await;

        for n in 0..2 {
            let (status, body) = post(
                &app,
                &auth,
                "/api/v1/jobs",
                json!({"queue_name": "sq", "payload": {"n": n}, "max_retries": 0}),
            )
            .await;
            assert_eq!(status, StatusCode::CREATED, "{body}");
        }
        let (job, lease) = claim_one(&app, &auth, "sq", &worker).await;
        let (status, body) = post(
            &app,
            &auth,
            &format!("/api/v1/jobs/{job}/fail"),
            json!({"worker_id": worker, "lease_id": lease, "error": "boom"}),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(job_status(db.pool(), &job).await, "deadletter");

        let (status, stats) = get(&app, &auth, "/api/v1/queues/sq/stats").await;
        assert_eq!(status, StatusCode::OK, "{stats}");
        assert_eq!(stats["deadletter_jobs"], 1, "{stats}");
        assert_eq!(stats["failed_jobs_24h"], 1, "{stats}");
        assert_eq!(stats["pending_jobs"], 1, "{stats}");

        // A job dead-lettered two days ago is still in the DLQ but not in the 24h count.
        backdate_updated_at(db.pool(), &job, 2).await;
        let (_, stats) = get(&app, &auth, "/api/v1/queues/sq/stats").await;
        assert_eq!(stats["deadletter_jobs"], 1, "{stats}");
        assert_eq!(stats["failed_jobs_24h"], 0, "{stats}");
    }

    // BEJOB-09: an invalid filter is a 400, not an unfiltered list.
    #[tokio::test]
    async fn w5_list_filters_reject_invalid_values() {
        let db = TestDatabase::new().await;
        let t = seed_tenant(db.pool()).await;
        let app = app(&db);
        let auth = bearer(&t.key);
        for (n, tag) in ["customer:42", "customer:7"].iter().enumerate() {
            let (status, body) = post(
                &app,
                &auth,
                "/api/v1/jobs",
                json!({"queue_name": "lq", "payload": {"n": n}, "tags": [tag, "env prod"]}),
            )
            .await;
            assert_eq!(status, StatusCode::CREATED, "{body}");
        }
        sqlx::query(
            "UPDATE jobs SET status = 'deadletter' WHERE organization_id = $1 AND tags ? 'customer:42'",
        )
        .bind(&t.org_id)
        .execute(db.pool())
        .await
        .unwrap();

        for uri in [
            "/api/v1/jobs?status=dead_letter",
            "/api/v1/jobs?status=bogus",
            "/api/v1/jobs?queue_name=bad%20name",
            "/api/v1/jobs/dlq?queue_name=bad%20name",
            "/api/v1/jobs/stats?queue_name=bad%20name",
        ] {
            let (status, body) = get(&app, &auth, uri).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{uri} -> {body}");
        }

        // Tags are matched with the characters they are stored with.
        let (status, body) = get(&app, &auth, "/api/v1/jobs?tag=customer%3A7").await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body.as_array().unwrap().len(), 1, "{body}");
        let (status, body) = get(&app, &auth, "/api/v1/jobs/dlq?tag=customer%3A42").await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body.as_array().unwrap().len(), 1, "{body}");
        let (status, body) = get(&app, &auth, "/api/v1/jobs?tag=env%20prod").await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body.as_array().unwrap().len(), 2, "{body}");
        let (status, body) = get(&app, &auth, "/api/v1/jobs?tag=customer%3A99").await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body.as_array().unwrap().len(), 0, "{body}");

        // Status is matched case-insensitively; an empty filter is no filter.
        let (status, body) = get(&app, &auth, "/api/v1/jobs?status=DeadLetter").await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body.as_array().unwrap().len(), 1, "{body}");
        let (status, body) = get(&app, &auth, "/api/v1/jobs?status=&tag=&queue_name=").await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body.as_array().unwrap().len(), 2, "{body}");
    }

    // BEJOB-10 / BEJOB-14: order_by/order_dir are honoured, ties break on id.
    #[tokio::test]
    async fn w5_list_honours_order_by_with_an_id_tiebreak() {
        let db = TestDatabase::new().await;
        let t = seed_tenant(db.pool()).await;
        let app = app(&db);
        let auth = bearer(&t.key);
        for priority in [5, -3, 10] {
            let (status, body) = post(
                &app,
                &auth,
                "/api/v1/jobs",
                json!({"queue_name": "oq", "payload": {}, "priority": priority}),
            )
            .await;
            assert_eq!(status, StatusCode::CREATED, "{body}");
        }
        let priorities = |body: &Value| -> Vec<i64> {
            body.as_array()
                .unwrap()
                .iter()
                .map(|j| j["priority"].as_i64().unwrap())
                .collect()
        };
        let (status, body) = get(
            &app,
            &auth,
            "/api/v1/jobs?queue_name=oq&order_by=priority&order_dir=asc",
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(priorities(&body), vec![-3, 5, 10], "{body}");
        let (status, body) = get(&app, &auth, "/api/v1/jobs?queue_name=oq&order_by=priority").await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(priorities(&body), vec![10, 5, -3], "{body}");

        for uri in [
            "/api/v1/jobs?order_by=payload",
            "/api/v1/jobs?order_dir=sideways",
            "/api/v1/jobs/dlq?order_by=created_at;DROP",
        ] {
            let (status, body) = get(&app, &auth, uri).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{uri} -> {body}");
        }

        // Rows sharing one created_at (bulk and cron batches do) page stably.
        sqlx::query(
            "INSERT INTO jobs (id, organization_id, queue_name, payload, created_at, updated_at) \
             SELECT 'w5-tie-' || g, $1, 'ties', '{}'::JSONB, '2026-01-01T00:00:00Z', NOW() \
             FROM generate_series(1, 7) AS g",
        )
        .bind(&t.org_id)
        .execute(db.pool())
        .await
        .unwrap();
        let mut seen = Vec::new();
        for offset in [0, 3, 6] {
            // Move a row between pages, as a claim does: the order must not change.
            sqlx::query("UPDATE jobs SET updated_at = NOW() WHERE id = 'w5-tie-2'")
                .execute(db.pool())
                .await
                .unwrap();
            let (status, body) = get(
                &app,
                &auth,
                &format!("/api/v1/jobs?queue_name=ties&limit=3&offset={offset}"),
            )
            .await;
            assert_eq!(status, StatusCode::OK, "{body}");
            seen.extend(
                body.as_array()
                    .unwrap()
                    .iter()
                    .map(|j| j["id"].as_str().unwrap().to_string()),
            );
        }
        let expected: Vec<String> = (1..=7).rev().map(|g| format!("w5-tie-{g}")).collect();
        assert_eq!(seen, expected);
    }

    // BEFLOW-04: retrying a dead-lettered workflow step re-opens the steps the
    // sweep cancelled because of it, and the workflow runs to completion.
    #[tokio::test]
    async fn w5_retrying_a_dead_lettered_workflow_step_reopens_its_cancelled_dependents() {
        let db = TestDatabase::new().await;
        let t = seed_tenant(db.pool()).await;
        let app = app(&db);
        let auth = bearer(&t.key);
        let worker = register_worker(&app, &auth, "wq").await;
        let side_worker = register_worker(&app, &auth, "wx").await;

        for via_dlq in [false, true] {
            let (status, body) = post(
                &app,
                &auth,
                "/api/v1/workflows",
                json!({
                    "name": format!("w5-wf-{via_dlq}"),
                    "jobs": [
                        {"key": "a", "queue_name": "wq", "payload": {}},
                        {"key": "b", "queue_name": "wq", "payload": {}, "depends_on": ["a"], "max_retries": 0},
                        {"key": "c", "queue_name": "wq", "payload": {}, "depends_on": ["b"]},
                        {"key": "d", "queue_name": "wq", "payload": {}, "depends_on": ["c"]},
                        {"key": "x", "queue_name": "wx", "payload": {}},
                    ],
                }),
            )
            .await;
            assert_eq!(status, StatusCode::OK, "{body}");
            let workflow = body["workflow_id"].as_str().unwrap().to_string();
            let id = |key: &str| -> String {
                body["job_ids"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|m| m["key"] == key)
                    .unwrap()["job_id"]
                    .as_str()
                    .unwrap()
                    .to_string()
            };
            let (a, b, c, d) = (id("a"), id("b"), id("c"), id("d"));

            let (job, lease) = claim_one(&app, &auth, "wq", &worker).await;
            assert_eq!(job, a);
            complete(&app, &auth, &a, &worker, &lease).await;
            let (job, lease) = claim_one(&app, &auth, "wq", &worker).await;
            assert_eq!(job, b);
            let (status, body) = post(
                &app,
                &auth,
                &format!("/api/v1/jobs/{b}/fail"),
                json!({"worker_id": worker, "lease_id": lease, "error": "boom"}),
            )
            .await;
            assert_eq!(status, StatusCode::OK, "{body}");
            assert_eq!(job_status(db.pool(), &b).await, "deadletter");

            // What the scheduler's dependency sweep does within 10 s.
            sqlx::query(
                "UPDATE jobs SET status = 'cancelled', \
                 last_error = 'Dependency job failed or was deadlettered', \
                 completed_at = NOW(), updated_at = NOW() WHERE id = ANY($1)",
            )
            .bind(vec![c.clone(), d.clone()])
            .execute(db.pool())
            .await
            .unwrap();

            if via_dlq {
                // The side branch finished first, so the workflow was closed as failed.
                let (job, lease) = claim_one(&app, &auth, "wx", &side_worker).await;
                complete(&app, &auth, &job, &side_worker, &lease).await;
                sqlx::query(
                    "UPDATE workflows SET status = 'failed', completed_at = NOW() WHERE id = $1",
                )
                .bind(&workflow)
                .execute(db.pool())
                .await
                .unwrap();
                let (status, body) = post(
                    &app,
                    &auth,
                    "/api/v1/jobs/dlq/retry",
                    json!({"job_ids": [b]}),
                )
                .await;
                assert_eq!(status, StatusCode::OK, "{body}");
                assert_eq!(body["retried_count"], 1, "{body}");
            } else {
                let (status, body) =
                    post(&app, &auth, &format!("/api/v1/jobs/{b}/retry"), json!({})).await;
                assert_eq!(status, StatusCode::OK, "{body}");
            }

            for dependent in [&c, &d] {
                let (status, met, error): (String, Option<bool>, Option<String>) = sqlx::query_as(
                    "SELECT status, dependencies_met, last_error FROM jobs WHERE id = $1",
                )
                .bind(dependent)
                .fetch_one(db.pool())
                .await
                .unwrap();
                assert_eq!(status, "pending", "via_dlq={via_dlq}");
                assert_eq!(met, Some(false), "via_dlq={via_dlq}");
                assert_eq!(error, None, "via_dlq={via_dlq}");
            }
            assert_eq!(workflow_status(db.pool(), &workflow).await, "running");

            let (job, lease) = claim_one(&app, &auth, "wq", &worker).await;
            assert_eq!(job, b);
            complete(&app, &auth, &b, &worker, &lease).await;
            if !via_dlq {
                let (job, lease) = claim_one(&app, &auth, "wx", &side_worker).await;
                complete(&app, &auth, &job, &side_worker, &lease).await;
                assert_eq!(workflow_status(db.pool(), &workflow).await, "running");
            }
            let (job, lease) = claim_one(&app, &auth, "wq", &worker).await;
            assert_eq!(job, c);
            complete(&app, &auth, &c, &worker, &lease).await;
            let (job, lease) = claim_one(&app, &auth, "wq", &worker).await;
            assert_eq!(job, d);
            complete(&app, &auth, &d, &worker, &lease).await;

            let (status, done, total): (String, i32, i32) = sqlx::query_as(
                "SELECT status, completed_jobs, total_jobs FROM workflows WHERE id = $1",
            )
            .bind(&workflow)
            .fetch_one(db.pool())
            .await
            .unwrap();
            assert_eq!(
                (status.as_str(), done, total),
                ("completed", 5, 5),
                "via_dlq={via_dlq}"
            );
        }
    }

    // BEFLOW-04: a workflow whose step was cancelled does not end 'completed'.
    #[tokio::test]
    async fn w5_workflow_with_a_cancelled_step_does_not_complete() {
        let db = TestDatabase::new().await;
        let t = seed_tenant(db.pool()).await;
        let app = app(&db);
        let auth = bearer(&t.key);
        let worker = register_worker(&app, &auth, "cq").await;
        let (status, body) = post(
            &app,
            &auth,
            "/api/v1/workflows",
            json!({
                "name": "w5-cancelled-step",
                "jobs": [
                    {"key": "a", "queue_name": "cq", "payload": {}, "priority": 10},
                    {"key": "b", "queue_name": "cq", "payload": {}},
                ],
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let workflow = body["workflow_id"].as_str().unwrap().to_string();
        let b = body["job_ids"]
            .as_array()
            .unwrap()
            .iter()
            .find(|m| m["key"] == "b")
            .unwrap()["job_id"]
            .as_str()
            .unwrap()
            .to_string();
        let (status, body) = call(
            &app,
            "DELETE",
            &format!("/api/v1/jobs/{b}"),
            &[("Authorization", &auth)],
            None,
        )
        .await;
        assert!(status.is_success(), "{body}");
        let (job, lease) = claim_one(&app, &auth, "cq", &worker).await;
        complete(&app, &auth, &job, &worker, &lease).await;
        assert_eq!(workflow_status(db.pool(), &workflow).await, "cancelled");
    }

    // BEJOB-15 / BEJOB-18: heartbeat keeps the claimed lease length; a worker
    // call without lease_id is a 422.
    #[tokio::test]
    async fn w5_heartbeat_keeps_the_claimed_lease_and_lease_id_is_required() {
        let db = TestDatabase::new().await;
        let t = seed_tenant(db.pool()).await;
        let app = app(&db);
        let auth = bearer(&t.key);
        let worker = register_worker(&app, &auth, "hq").await;
        let (status, body) = post(
            &app,
            &auth,
            "/api/v1/jobs",
            json!({"queue_name": "hq", "payload": {}}),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        let (status, body) = post(
            &app,
            &auth,
            "/api/v1/jobs/claim",
            json!({"queue_name": "hq", "worker_id": worker, "lease_duration_secs": 600}),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let job = body["jobs"][0]["id"].as_str().unwrap().to_string();
        let lease = body["jobs"][0]["lease_id"].as_str().unwrap().to_string();

        let remaining = |pool: sqlx::PgPool, job: String| async move {
            let secs: f64 = sqlx::query_scalar(
                "SELECT EXTRACT(EPOCH FROM lease_expires_at - NOW())::FLOAT8 FROM jobs WHERE id = $1",
            )
            .bind(job)
            .fetch_one(&pool)
            .await
            .unwrap();
            secs
        };
        let (status, body) = post(
            &app,
            &auth,
            &format!("/api/v1/jobs/{job}/heartbeat"),
            json!({"worker_id": worker, "lease_id": lease}),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let secs = remaining(db.pool().clone(), job.clone()).await;
        assert!(secs > 590.0, "lease shortened to {secs}s");

        // A heartbeat naming a length uses it, and later omitted ones keep it.
        let (status, body) = post(
            &app,
            &auth,
            &format!("/api/v1/jobs/{job}/heartbeat"),
            json!({"worker_id": worker, "lease_id": lease, "lease_duration_secs": 120}),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let (status, body) = post(
            &app,
            &auth,
            &format!("/api/v1/jobs/{job}/heartbeat"),
            json!({"worker_id": worker, "lease_id": lease}),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let secs = remaining(db.pool().clone(), job.clone()).await;
        assert!((110.0..=121.0).contains(&secs), "lease is {secs}s");

        for (action, extra) in [
            ("heartbeat", json!({})),
            ("fail", json!({"error": "boom"})),
            ("complete", json!({})),
        ] {
            let mut request = json!({"worker_id": worker});
            request
                .as_object_mut()
                .unwrap()
                .extend(extra.as_object().unwrap().clone());
            let (status, body) = post(
                &app,
                &auth,
                &format!("/api/v1/jobs/{job}/{action}"),
                request,
            )
            .await;
            assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{action}: {body}");
            assert_eq!(body["details"][0]["field"], "lease_id", "{action}: {body}");
        }
        assert_eq!(job_status(db.pool(), &job).await, "processing");
    }

    // BEJOB-22: DLQ retry is not charged against the daily cap; purge's
    // older_than means "dead-lettered before".
    #[tokio::test]
    async fn w5_dlq_retry_ignores_the_daily_cap_and_purge_uses_dead_letter_time() {
        let db = TestDatabase::new().await;
        let t = seed_tenant(db.pool()).await;
        let app = app(&db);
        let auth = bearer(&t.key);
        set_custom_limits(db.pool(), &t.org_id, json!({"max_jobs_per_day": 2})).await;
        let mut ids = Vec::new();
        for n in 0..2 {
            let (status, body) = post(
                &app,
                &auth,
                "/api/v1/jobs",
                json!({"queue_name": "dq", "payload": {"n": n}}),
            )
            .await;
            assert_eq!(status, StatusCode::CREATED, "{body}");
            ids.push(body["id"].as_str().unwrap().to_string());
        }
        sqlx::query("UPDATE jobs SET status = 'deadletter', updated_at = NOW() WHERE id = ANY($1)")
            .bind(&ids)
            .execute(db.pool())
            .await
            .unwrap();

        let (status, body) = post(
            &app,
            &auth,
            "/api/v1/jobs/dlq/retry",
            json!({"queue_name": "dq", "limit": 1}),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["retried_count"], 1, "{body}");

        // Created long ago, dead-lettered now: not older than a day as a dead letter.
        let remaining = if body["retried_jobs"][0] == ids[0].as_str() {
            &ids[1]
        } else {
            &ids[0]
        };
        sqlx::query("UPDATE jobs SET created_at = NOW() - INTERVAL '10 days' WHERE id = $1")
            .bind(remaining)
            .execute(db.pool())
            .await
            .unwrap();
        let older_than = (chrono::Utc::now() - chrono::Duration::days(1)).to_rfc3339();
        let (status, body) = post(
            &app,
            &auth,
            "/api/v1/jobs/dlq/purge",
            json!({"confirm": true, "older_than": older_than}),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["purged_count"], 0, "{body}");
        backdate_updated_at(db.pool(), remaining, 2).await;
        let (status, body) = post(
            &app,
            &auth,
            "/api/v1/jobs/dlq/purge",
            json!({"confirm": true, "queue_name": "dq", "older_than": older_than}),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["purged_count"], 1, "{body}");
    }

    // BEJOB-20: at the daily cap a job is refused before it is inserted. It was
    // inserted first (visible to workers) and deleted again when the counter
    // said the cap was full.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn w5_daily_cap_refuses_a_job_before_inserting_it() {
        let db = TestDatabase::new().await;
        let t = seed_tenant(db.pool()).await;
        set_custom_limits(db.pool(), &t.org_id, json!({"max_jobs_per_day": 3})).await;
        sqlx::raw_sql(
            "CREATE TABLE w5_deleted_jobs (id TEXT); \
             CREATE FUNCTION w5_note_delete() RETURNS TRIGGER AS $$ \
             BEGIN INSERT INTO w5_deleted_jobs VALUES (OLD.id); RETURN OLD; END $$ LANGUAGE plpgsql; \
             CREATE TRIGGER w5_note_delete AFTER DELETE ON jobs \
             FOR EACH ROW EXECUTE FUNCTION w5_note_delete();",
        )
        .execute(db.pool())
        .await
        .unwrap();
        let app = app(&db);
        let auth = bearer(&t.key);
        for n in 0..2 {
            let (status, body) = post(
                &app,
                &auth,
                "/api/v1/jobs",
                json!({"queue_name": "cap", "payload": {"n": n}}),
            )
            .await;
            assert_eq!(status, StatusCode::CREATED, "{body}");
        }

        // One slot left; concurrent enqueues all pass the non-atomic pre-check.
        let barrier = Arc::new(tokio::sync::Barrier::new(10));
        let mut tasks = Vec::new();
        for n in 0..10 {
            let (app, auth, barrier) = (app.clone(), auth.clone(), Arc::clone(&barrier));
            tasks.push(tokio::spawn(async move {
                barrier.wait().await;
                post(
                    &app,
                    &auth,
                    "/api/v1/jobs",
                    json!({"queue_name": "cap", "payload": {"racer": n}}),
                )
                .await
            }));
        }
        let mut created = 0;
        for task in tasks {
            let (status, body) = task.await.unwrap();
            match status {
                StatusCode::CREATED => created += 1,
                StatusCode::TOO_MANY_REQUESTS => {}
                other => panic!("unexpected {other}: {body}"),
            }
        }
        assert_eq!(created, 1);
        let (deleted,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM w5_deleted_jobs")
            .fetch_one(db.pool())
            .await
            .unwrap();
        assert_eq!(deleted, 0, "a refused job must never have been inserted");
        let (jobs,): (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM jobs WHERE organization_id = $1")
                .bind(&t.org_id)
                .fetch_one(db.pool())
                .await
                .unwrap();
        assert_eq!(jobs, 3);
    }

    // BEJOB-23: an expiry that has passed, or comes before the schedule, is a 400.
    #[tokio::test]
    async fn w5_expires_at_in_the_past_or_before_scheduled_at_is_rejected() {
        let db = TestDatabase::new().await;
        let t = seed_tenant(db.pool()).await;
        let app = app(&db);
        let auth = bearer(&t.key);
        let now = chrono::Utc::now();
        let at = |minutes: i64| (now + chrono::Duration::minutes(minutes)).to_rfc3339();

        for body in [
            json!({"queue_name": "eq", "payload": {}, "expires_at": at(-1)}),
            json!({"queue_name": "eq", "payload": {}, "expires_at": at(30), "scheduled_at": at(60)}),
        ] {
            let (status, resp) = post(&app, &auth, "/api/v1/jobs", body.clone()).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{body} -> {resp}");
        }
        let (jobs_today,): (i64,) =
            sqlx::query_as("SELECT jobs_today FROM get_org_resource_counts($1)")
                .bind(&t.org_id)
                .fetch_one(db.pool())
                .await
                .unwrap();
        assert_eq!(jobs_today, 0, "a rejected job consumes no quota");

        let (status, resp) = post(
            &app,
            &auth,
            "/api/v1/jobs",
            json!({"queue_name": "eq", "payload": {}, "expires_at": at(90), "scheduled_at": at(60)}),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{resp}");
    }
}
// ---- wave 5 / flow ----
// Schedules, workflows, dependency edges and the scheduler's failure and
// retention sweeps: BEFLOW-08/-09/-11/-16/-18/-19/-20/-21/-22 (BEOPS-11, QW7).
mod wave5_flow {
    use super::*;

    use std::time::Duration;

    use futures::StreamExt;
    use spooled_backend::cache::RedisCache;
    use spooled_backend::config::RedisSettings;
    use spooled_backend::outgoing_webhooks::service::OutgoingWebhookService;
    use spooled_backend::scheduler::Scheduler;

    use super::common::TestRedis;

    fn scheduler(db: &TestDatabase, cache: Option<Arc<RedisCache>>) -> Scheduler {
        Scheduler::new(
            Arc::clone(&db.pool),
            cache,
            Arc::new(Metrics::new()),
            Arc::new(OutgoingWebhookService::new((*db.pool).clone(), 3, 4)),
        )
    }

    async fn create_schedule(
        app: &Router,
        auth: &str,
        name: &str,
        queue: &str,
        cron: &str,
    ) -> (StatusCode, Value) {
        call(
            app,
            "POST",
            "/api/v1/schedules",
            &[("Authorization", auth)],
            Some(json!({
                "name": name,
                "cron_expression": cron,
                "queue_name": queue,
                "payload_template": {},
            })),
        )
        .await
    }

    /// Create a workflow through the API; returns (workflow id, key -> job id).
    async fn create_workflow(
        app: &Router,
        auth: &str,
        jobs: Value,
    ) -> (String, std::collections::HashMap<String, String>) {
        let (status, body) = call(
            app,
            "POST",
            "/api/v1/workflows",
            &[("Authorization", auth)],
            Some(json!({"name": "w5-flow", "jobs": jobs})),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let ids = body["job_ids"]
            .as_array()
            .expect("job_ids")
            .iter()
            .map(|m| {
                (
                    m["key"].as_str().unwrap().to_string(),
                    m["job_id"].as_str().unwrap().to_string(),
                )
            })
            .collect();
        (body["workflow_id"].as_str().unwrap().to_string(), ids)
    }

    async fn get_workflow(app: &Router, auth: &str, id: &str) -> Value {
        let (status, body) = call(
            app,
            "GET",
            &format!("/api/v1/workflows/{id}"),
            &[("Authorization", auth)],
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        body
    }

    async fn exists(pool: &sqlx::PgPool, table: &str, id: &str) -> bool {
        // `table` is always a literal from this module.
        sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
            "SELECT EXISTS(SELECT 1 FROM {table} WHERE id = $1)"
        )))
        .bind(id)
        .fetch_one(pool)
        .await
        .unwrap()
    }

    async fn seed_job(pool: &sqlx::PgPool, org_id: &str, status: &str, age_days: i32) -> String {
        let id = uuid::Uuid::new_v4().to_string();
        sqlx::query(
            "INSERT INTO jobs (id, organization_id, queue_name, payload, status, created_at, updated_at) \
             VALUES ($1, $2, 'w5-ret', '{}'::JSONB, $3, NOW() - make_interval(days => $4), \
             NOW() - make_interval(days => $4))",
        )
        .bind(&id)
        .bind(org_id)
        .bind(status)
        .bind(age_days)
        .execute(pool)
        .await
        .unwrap();
        id
    }

    /// Wait for the outgoing `event` delivery attempt about `job_id`; returns its data.
    async fn delivered(pool: &sqlx::PgPool, org_id: &str, event: &str, job_id: &str) -> Value {
        for _ in 0..100 {
            let row: Option<Value> = sqlx::query_scalar(
                "SELECT d.payload->'data' FROM outgoing_webhook_deliveries d \
                 JOIN outgoing_webhooks w ON w.id = d.webhook_id \
                 WHERE w.organization_id = $1 AND d.event = $2 \
                   AND d.payload->'data'->>'job_id' = $3",
            )
            .bind(org_id)
            .bind(event)
            .bind(job_id)
            .fetch_optional(pool)
            .await
            .unwrap();
            if let Some(data) = row {
                return data;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        panic!("no {event} delivery for {job_id}");
    }

    async fn subscribe_events(cache: &RedisCache, org_id: &str) -> redis::aio::PubSub {
        cache
            .subscribe(&format!("org:{org_id}:events"))
            .await
            .expect("subscribe")
    }

    /// Realtime event types published for `job_id` within a few seconds.
    async fn realtime_types(pubsub: &mut redis::aio::PubSub, job_id: &str) -> Vec<String> {
        let mut seen = Vec::new();
        let mut stream = pubsub.on_message();
        while let Ok(Some(msg)) = tokio::time::timeout(Duration::from_secs(3), stream.next()).await
        {
            let payload: String = msg.get_payload().unwrap();
            let event: Value = serde_json::from_str(&payload).unwrap();
            if event["data"]["job_id"] == job_id {
                seen.push(event["type"].as_str().unwrap_or_default().to_string());
                if seen.iter().any(|t| t == "JobFailed") {
                    break;
                }
            }
        }
        seen
    }

    async fn deadletter_history(pool: &sqlx::PgPool, job_id: &str) -> i64 {
        sqlx::query_scalar(
            "SELECT COUNT(*) FROM job_history WHERE job_id = $1 AND event_type = 'deadlettered'",
        )
        .bind(job_id)
        .fetch_one(pool)
        .await
        .unwrap()
    }

    async fn subscribe_job_failed(pool: &sqlx::PgPool, org_id: &str) {
        sqlx::query(
            "INSERT INTO outgoing_webhooks (organization_id, name, url, events) \
             VALUES ($1, 'w5', 'http://127.0.0.1:9/w5', ARRAY['job.failed'])",
        )
        .bind(org_id)
        .execute(pool)
        .await
        .unwrap();
    }

    // BEFLOW-08: Feb 30 / Apr 31 were accepted with next_run_at null and never
    // fired; create and update now refuse them.
    #[tokio::test]
    async fn w5_flow_impossible_cron_dates_are_rejected() {
        let db = TestDatabase::new().await;
        let t = seed_tenant(db.pool()).await;
        let app = app(&db);
        let auth = bearer(&t.key);

        for cron in ["0 0 0 30 2 *", "0 0 31 4 *"] {
            let (status, body) = create_schedule(&app, &auth, "never", "w5-cron", cron).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{cron}: {body}");
        }
        let stored: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM schedules WHERE organization_id = $1")
                .bind(&t.org_id)
                .fetch_one(db.pool())
                .await
                .unwrap();
        assert_eq!(stored, 0, "a refused schedule is not stored");

        let (status, body) = create_schedule(&app, &auth, "leap", "w5-cron", "0 0 29 2 *").await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        assert!(body["next_run_at"].is_string(), "{body}");
        let id = body["id"].as_str().unwrap().to_string();

        let (status, body) = call(
            &app,
            "PUT",
            &format!("/api/v1/schedules/{id}"),
            &[("Authorization", &auth)],
            Some(json!({"cron_expression": "0 0 31 4 *"})),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        let (cron, next): (String, Option<chrono::DateTime<chrono::Utc>>) =
            sqlx::query_as("SELECT cron_expression, next_run_at FROM schedules WHERE id = $1")
                .bind(&id)
                .fetch_one(db.pool())
                .await
                .unwrap();
        assert_eq!(cron, "0 0 29 2 *");
        assert!(next.is_some());
    }

    // BEFLOW-18: GET /schedules ignored queue_name, is_active and offset, which
    // the Go and Python SDKs send.
    #[tokio::test]
    async fn w5_flow_schedule_list_honours_queue_active_and_offset() {
        let db = TestDatabase::new().await;
        let t = seed_tenant(db.pool()).await;
        let app = app(&db);
        let auth = bearer(&t.key);

        let mut ids = Vec::new();
        for (name, queue) in [("s1", "w5-a"), ("s2", "w5-b"), ("s3", "w5-a")] {
            let (status, body) = create_schedule(&app, &auth, name, queue, "0 0 * * *").await;
            assert_eq!(status, StatusCode::CREATED, "{body}");
            ids.push(body["id"].as_str().unwrap().to_string());
        }
        // Newest first is s3, s2, s1.
        for (age, id) in ids.iter().rev().enumerate() {
            sqlx::query(
                "UPDATE schedules SET created_at = NOW() - make_interval(mins => $2) WHERE id = $1",
            )
            .bind(id)
            .bind(age as i32 + 1)
            .execute(db.pool())
            .await
            .unwrap();
        }
        let (status, body) = call(
            &app,
            "POST",
            &format!("/api/v1/schedules/{}/pause", ids[0]),
            &[("Authorization", &auth)],
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");

        let names = |body: &Value| -> Vec<String> {
            body.as_array()
                .expect("array")
                .iter()
                .map(|s| s["name"].as_str().unwrap().to_string())
                .collect()
        };
        for (query, expected) in [
            ("queue_name=w5-a", vec!["s3", "s1"]),
            ("is_active=false", vec!["s1"]),
            ("is_active=true&queue_name=w5-a", vec!["s3"]),
            ("limit=1&offset=1", vec!["s2"]),
            ("limit=2&offset=2", vec!["s1"]),
            ("offset=0", vec!["s3", "s2", "s1"]),
        ] {
            let (status, body) = call(
                &app,
                "GET",
                &format!("/api/v1/schedules?{query}"),
                &[("Authorization", &auth)],
                None,
            )
            .await;
            assert_eq!(status, StatusCode::OK, "{query}: {body}");
            assert_eq!(names(&body), expected, "{query}");
        }

        let (status, body) = call(
            &app,
            "GET",
            "/api/v1/schedules?is_active=maybe",
            &[("Authorization", &auth)],
            None,
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    }

    // BEFLOW-16: a duplicate depends_on key hit the unique edge constraint
    // inside the transaction and answered 500.
    #[tokio::test]
    async fn w5_flow_duplicate_workflow_dependency_is_a_bad_request() {
        let db = TestDatabase::new().await;
        let t = seed_tenant(db.pool()).await;
        let app = app(&db);
        let auth = bearer(&t.key);

        let (status, body) = call(
            &app,
            "POST",
            "/api/v1/workflows",
            &[("Authorization", &auth)],
            Some(json!({
                "name": "dup",
                "jobs": [
                    {"key": "a", "queue_name": "w5-wf", "payload": {}},
                    {"key": "c", "queue_name": "w5-wf", "payload": {}, "depends_on": ["a", "a"]},
                ],
            })),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert!(body.to_string().contains("more than once"), "{body}");
        let jobs: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM jobs WHERE organization_id = $1")
            .bind(&t.org_id)
            .fetch_one(db.pool())
            .await
            .unwrap();
        assert_eq!(jobs, 0);
    }

    // BEFLOW-19: GET /workflows/{id} hardcoded job_type "job" and failed_at null.
    #[tokio::test]
    async fn w5_flow_workflow_jobs_carry_job_type_and_failed_at() {
        let db = TestDatabase::new().await;
        let t = seed_tenant(db.pool()).await;
        let app = app(&db);
        let auth = bearer(&t.key);

        let (wf, ids) = create_workflow(
            &app,
            &auth,
            json!([
                {"key": "a", "queue_name": "w5-wf", "payload": {"job_type": "send_email"}},
                {"key": "b", "queue_name": "w5-wf", "payload": {}, "depends_on": ["a"]},
            ]),
        )
        .await;
        sqlx::query(
            "UPDATE jobs SET status = 'deadletter', last_error = 'boom', updated_at = NOW() WHERE id = $1",
        )
        .bind(&ids["a"])
        .execute(db.pool())
        .await
        .unwrap();

        let body = get_workflow(&app, &auth, &wf).await;
        let job = |id: &str| -> Value {
            body["jobs"]
                .as_array()
                .unwrap()
                .iter()
                .find(|j| j["id"] == id)
                .cloned()
                .expect("job in workflow")
        };
        let a = job(&ids["a"]);
        assert_eq!(a["job_type"], "send_email", "{a}");
        assert!(a["failed_at"].is_string(), "{a}");
        assert_eq!(a["backoff_type"], "exponential", "{a}");
        let b = job(&ids["b"]);
        assert_eq!(b["job_type"], "job", "{b}");
        assert!(b["failed_at"].is_null(), "{b}");
    }

    // BEFLOW-20: started_at was never set and a workflow read `pending` until
    // its first job finished.
    #[tokio::test]
    async fn w5_flow_workflow_is_running_once_a_job_starts() {
        let db = TestDatabase::new().await;
        let t = seed_tenant(db.pool()).await;
        let app = app(&db);
        let auth = bearer(&t.key);

        let (wf, ids) = create_workflow(
            &app,
            &auth,
            json!([
                {"key": "a", "queue_name": "w5-wf", "payload": {}},
                {"key": "b", "queue_name": "w5-wf", "payload": {}, "depends_on": ["a"]},
            ]),
        )
        .await;
        // A worker claims the root job.
        sqlx::query(
            "UPDATE jobs SET status = 'processing', started_at = NOW(), assigned_worker_id = 'w5', \
             lease_id = 'l5', lease_expires_at = NOW() + INTERVAL '5 minutes' WHERE id = $1",
        )
        .bind(&ids["a"])
        .execute(db.pool())
        .await
        .unwrap();
        // A one-job workflow that started and finished between two sweeps.
        let (quick, quick_ids) = create_workflow(
            &app,
            &auth,
            json!([{"key": "only", "queue_name": "w5-wf", "payload": {}}]),
        )
        .await;
        sqlx::query(
            "UPDATE jobs SET status = 'completed', started_at = NOW() - INTERVAL '2 seconds', \
             completed_at = NOW() WHERE id = $1",
        )
        .bind(&quick_ids["only"])
        .execute(db.pool())
        .await
        .unwrap();

        scheduler(&db, None)
            .update_job_dependencies()
            .await
            .unwrap();

        let body = get_workflow(&app, &auth, &wf).await;
        assert_eq!(body["status"], "running", "{body}");
        assert!(body["started_at"].is_string(), "{body}");
        let body = get_workflow(&app, &auth, &quick).await;
        assert_eq!(body["status"], "completed", "{body}");
        assert!(body["started_at"].is_string(), "{body}");
    }

    // BEFLOW-21: cancel overwrote a finished workflow's status and completed_at.
    #[tokio::test]
    async fn w5_flow_cancel_refuses_a_finished_workflow() {
        let db = TestDatabase::new().await;
        let t = seed_tenant(db.pool()).await;
        let app = app(&db);
        let auth = bearer(&t.key);

        let (done, ids) = create_workflow(
            &app,
            &auth,
            json!([{"key": "a", "queue_name": "w5-wf", "payload": {}}]),
        )
        .await;
        sqlx::query("UPDATE jobs SET status = 'completed', completed_at = NOW() WHERE id = $1")
            .bind(&ids["a"])
            .execute(db.pool())
            .await
            .unwrap();
        sqlx::query(
            "UPDATE workflows SET status = 'completed', completed_at = '2026-01-01T00:00:00Z' WHERE id = $1",
        )
        .bind(&done)
        .execute(db.pool())
        .await
        .unwrap();

        let (status, body) = call(
            &app,
            "POST",
            &format!("/api/v1/workflows/{done}/cancel"),
            &[("Authorization", &auth)],
            None,
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT, "{body}");
        let (wf_status, completed_at): (String, chrono::DateTime<chrono::Utc>) =
            sqlx::query_as("SELECT status, completed_at FROM workflows WHERE id = $1")
                .bind(&done)
                .fetch_one(db.pool())
                .await
                .unwrap();
        assert_eq!(wf_status, "completed");
        assert_eq!(completed_at.to_rfc3339(), "2026-01-01T00:00:00+00:00");

        // A workflow still in flight is cancelled as before.
        let (live, _) = create_workflow(
            &app,
            &auth,
            json!([{"key": "a", "queue_name": "w5-wf", "payload": {}}]),
        )
        .await;
        let (status, body) = call(
            &app,
            "POST",
            &format!("/api/v1/workflows/{live}/cancel"),
            &[("Authorization", &auth)],
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["status"], "cancelled", "{body}");
    }

    // BEFLOW-22: self and cyclic edges were skipped with a 200, after the call
    // had already rewritten dependency_mode (default `all`).
    #[tokio::test]
    async fn w5_flow_add_dependencies_rejects_self_and_cycles_and_keeps_mode() {
        let db = TestDatabase::new().await;
        let t = seed_tenant(db.pool()).await;
        let app = app(&db);
        let auth = bearer(&t.key);

        let mut ids = Vec::new();
        for _ in 0..2 {
            let (status, body) = call(
                &app,
                "POST",
                "/api/v1/jobs",
                &[("Authorization", &auth)],
                Some(json!({"queue_name": "w5-deps", "payload": {}})),
            )
            .await;
            assert!(status.is_success(), "{body}");
            ids.push(body["id"].as_str().unwrap().to_string());
        }
        let (a, b) = (&ids[0], &ids[1]);
        let add = |job: String, body: Value| {
            let app = app.clone();
            let auth = auth.clone();
            async move {
                call(
                    &app,
                    "POST",
                    &format!("/api/v1/jobs/{job}/dependencies"),
                    &[("Authorization", &auth)],
                    Some(body),
                )
                .await
            }
        };
        let mode_and_met = |id: String| {
            let pool = db.pool().clone();
            async move {
                sqlx::query_as::<_, (Option<String>, Option<bool>)>(
                    "SELECT dependency_mode, dependencies_met FROM jobs WHERE id = $1",
                )
                .bind(id)
                .fetch_one(&pool)
                .await
                .unwrap()
            }
        };

        let (status, body) = add(a.clone(), json!({"depends_on": [a]})).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert_eq!(
            mode_and_met(a.clone()).await.1,
            Some(true),
            "a stays runnable"
        );

        let (status, body) = add(
            a.clone(),
            json!({"depends_on": [b], "dependency_mode": "any"}),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["dependencies_added"], 1, "{body}");

        // b -> a would close the loop a -> b -> a.
        let before = mode_and_met(b.clone()).await;
        let (status, body) = add(b.clone(), json!({"depends_on": [a]})).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert_eq!(mode_and_met(b.clone()).await, before, "b is untouched");

        // Re-adding an existing edge adds nothing and keeps a's `any` mode.
        let (status, body) = add(a.clone(), json!({"depends_on": [b]})).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["dependencies_added"], 0, "{body}");
        assert_eq!(
            mode_and_met(a.clone()).await,
            (Some("any".to_string()), Some(false))
        );
    }

    // BEFLOW-09 / BEOPS-11: schedule_runs, dead_letter_queue, failed/deadletter
    // jobs and offline workers were never garbage-collected.
    #[tokio::test]
    async fn w5_flow_retention_sweeps_runs_dlq_failed_jobs_and_offline_workers() {
        let db = TestDatabase::new().await;
        let t = seed_tenant(db.pool()).await; // starter: jobs 14 days, history 7 days
        let pool = db.pool();
        let org = t.org_id.as_str();

        // schedule_runs follow history retention.
        let schedule = uuid::Uuid::new_v4().to_string();
        sqlx::query(
            "INSERT INTO schedules (id, organization_id, name, cron_expression, queue_name, payload_template) \
             VALUES ($1, $2, 'ret', '0 0 * * *', 'w5-ret', '{}'::JSONB)",
        )
        .bind(&schedule)
        .bind(org)
        .execute(pool)
        .await
        .unwrap();
        let mut runs = Vec::new();
        for age_days in [10, 2] {
            let id = uuid::Uuid::new_v4().to_string();
            sqlx::query(
                "INSERT INTO schedule_runs (id, schedule_id, status, started_at, completed_at) \
                 VALUES ($1, $2, 'completed', NOW() - make_interval(days => $3), NOW() - make_interval(days => $3))",
            )
            .bind(&id)
            .bind(&schedule)
            .bind(age_days)
            .execute(pool)
            .await
            .unwrap();
            runs.push(id);
        }

        // dead_letter_queue rows follow job retention.
        let mut dlq = Vec::new();
        for age_days in [20, 2] {
            let id = uuid::Uuid::new_v4().to_string();
            sqlx::query(
                "INSERT INTO dead_letter_queue (id, job_id, organization_id, queue_name, reason, original_payload, created_at) \
                 VALUES ($1, 'gone', $2, 'w5-ret', 'boom', '{}'::JSONB, NOW() - make_interval(days => $3))",
            )
            .bind(&id)
            .bind(org)
            .bind(age_days)
            .execute(pool)
            .await
            .unwrap();
            dlq.push(id);
        }

        // Failed and dead-lettered jobs follow job retention...
        let old_dead = seed_job(pool, org, "deadletter", 20).await;
        let old_failed = seed_job(pool, org, "failed", 20).await;
        let recent_dead = seed_job(pool, org, "deadletter", 2).await;
        // ...but never while something still waits on them.
        let gating_edge = seed_job(pool, org, "deadletter", 20).await;
        let edge_child = seed_job(pool, org, "pending", 0).await;
        sqlx::query("INSERT INTO job_dependencies (job_id, depends_on_job_id) VALUES ($1, $2)")
            .bind(&edge_child)
            .bind(&gating_edge)
            .execute(pool)
            .await
            .unwrap();
        let gating_parent = seed_job(pool, org, "failed", 20).await;
        let parent_child = seed_job(pool, org, "pending", 0).await;
        sqlx::query("UPDATE jobs SET parent_job_id = $2 WHERE id = $1")
            .bind(&parent_child)
            .bind(&gating_parent)
            .execute(pool)
            .await
            .unwrap();
        let in_running_workflow = seed_job(pool, org, "deadletter", 20).await;
        let workflow = uuid::Uuid::new_v4().to_string();
        sqlx::query(
            "INSERT INTO workflows (id, organization_id, name, status, total_jobs) VALUES ($1, $2, 'ret', 'running', 1)",
        )
        .bind(&workflow)
        .bind(org)
        .execute(pool)
        .await
        .unwrap();
        sqlx::query("UPDATE jobs SET workflow_id = $2 WHERE id = $1")
            .bind(&in_running_workflow)
            .bind(&workflow)
            .execute(pool)
            .await
            .unwrap();

        // Offline workers go after 7 days, unless a job is still leased to them.
        let mut workers = Vec::new();
        for age_days in [8, 1, 8] {
            let id = uuid::Uuid::new_v4().to_string();
            sqlx::query(
                "INSERT INTO workers (id, organization_id, queue_name, hostname, status, last_heartbeat) \
                 VALUES ($1, $2, 'w5-ret', 'host', 'offline', NOW() - make_interval(days => $3))",
            )
            .bind(&id)
            .bind(org)
            .bind(age_days)
            .execute(pool)
            .await
            .unwrap();
            workers.push(id);
        }
        let leased = seed_job(pool, org, "processing", 0).await;
        sqlx::query(
            "UPDATE jobs SET assigned_worker_id = $2, lease_id = 'l', \
             lease_expires_at = NOW() + INTERVAL '10 minutes' WHERE id = $1",
        )
        .bind(&leased)
        .bind(&workers[2])
        .execute(pool)
        .await
        .unwrap();

        scheduler(&db, None).cleanup_stale_workers().await.unwrap();

        assert!(!exists(pool, "schedule_runs", &runs[0]).await, "old run");
        assert!(exists(pool, "schedule_runs", &runs[1]).await, "recent run");
        assert!(
            !exists(pool, "dead_letter_queue", &dlq[0]).await,
            "old DLQ row"
        );
        assert!(
            exists(pool, "dead_letter_queue", &dlq[1]).await,
            "recent DLQ row"
        );
        assert!(!exists(pool, "jobs", &old_dead).await, "old deadletter job");
        assert!(!exists(pool, "jobs", &old_failed).await, "old failed job");
        assert!(
            exists(pool, "jobs", &recent_dead).await,
            "recent deadletter job"
        );
        assert!(
            exists(pool, "jobs", &gating_edge).await,
            "has a pending dependent"
        );
        assert!(
            exists(pool, "jobs", &gating_parent).await,
            "has a pending child"
        );
        assert!(
            exists(pool, "jobs", &in_running_workflow).await,
            "its workflow is still running"
        );
        assert!(
            !exists(pool, "workers", &workers[0]).await,
            "offline 8 days"
        );
        assert!(exists(pool, "workers", &workers[1]).await, "offline 1 day");
        assert!(
            exists(pool, "workers", &workers[2]).await,
            "still holds a lease"
        );
    }

    // BEFLOW-11 / QW7: dead-lettering by lease expiry sent no job.failed webhook,
    // no realtime event and wrote no job_history row.
    #[tokio::test]
    async fn w5_flow_lease_expiry_dead_letter_emits_failure_events() {
        let redis = TestRedis::new().await;
        let cache = RedisCache::connect(&RedisSettings {
            url: redis.url.clone(),
            pool_size: 4,
        })
        .await
        .expect("connect redis");
        let db = TestDatabase::new().await;
        let t = seed_tenant(db.pool()).await;
        subscribe_job_failed(db.pool(), &t.org_id).await;
        let job = seed_job(db.pool(), &t.org_id, "processing", 0).await;
        sqlx::query(
            "UPDATE jobs SET assigned_worker_id = 'gone', lease_id = 'l', retry_count = 3, max_retries = 3, \
             lease_expires_at = NOW() - INTERVAL '1 second' WHERE id = $1",
        )
        .bind(&job)
        .execute(db.pool())
        .await
        .unwrap();
        let mut pubsub = subscribe_events(&cache, &t.org_id).await;

        scheduler(&db, Some(Arc::new(cache)))
            .recover_expired_leases()
            .await
            .unwrap();

        let status: String = sqlx::query_scalar("SELECT status FROM jobs WHERE id = $1")
            .bind(&job)
            .fetch_one(db.pool())
            .await
            .unwrap();
        assert_eq!(status, "deadletter");
        assert_eq!(deadletter_history(db.pool(), &job).await, 1);
        let types = realtime_types(&mut pubsub, &job).await;
        assert!(types.iter().any(|t| t == "JobStatusChange"), "{types:?}");
        assert!(types.iter().any(|t| t == "JobFailed"), "{types:?}");
        let data = delivered(db.pool(), &t.org_id, "job.failed", &job).await;
        assert_eq!(data["status"], "deadletter", "{data}");
    }

    // BEFLOW-11 / QW7: the same for a job dead-lettered because its worker went
    // offline.
    #[tokio::test]
    async fn w5_flow_offline_worker_dead_letter_emits_failure_events() {
        let redis = TestRedis::new().await;
        let cache = RedisCache::connect(&RedisSettings {
            url: redis.url.clone(),
            pool_size: 4,
        })
        .await
        .expect("connect redis");
        let db = TestDatabase::new().await;
        let t = seed_tenant(db.pool()).await;
        subscribe_job_failed(db.pool(), &t.org_id).await;
        let worker = uuid::Uuid::new_v4().to_string();
        sqlx::query(
            "INSERT INTO workers (id, organization_id, queue_name, hostname, status, last_heartbeat) \
             VALUES ($1, $2, 'w5-ret', 'host', 'healthy', NOW() - INTERVAL '10 minutes')",
        )
        .bind(&worker)
        .bind(&t.org_id)
        .execute(db.pool())
        .await
        .unwrap();
        let job = seed_job(db.pool(), &t.org_id, "processing", 0).await;
        sqlx::query(
            "UPDATE jobs SET assigned_worker_id = $2, lease_id = 'l', retry_count = 2, max_retries = 2, \
             lease_expires_at = NULL WHERE id = $1",
        )
        .bind(&job)
        .bind(&worker)
        .execute(db.pool())
        .await
        .unwrap();
        let mut pubsub = subscribe_events(&cache, &t.org_id).await;

        scheduler(&db, Some(Arc::new(cache)))
            .cleanup_stale_workers()
            .await
            .unwrap();

        let status: String = sqlx::query_scalar("SELECT status FROM jobs WHERE id = $1")
            .bind(&job)
            .fetch_one(db.pool())
            .await
            .unwrap();
        assert_eq!(status, "deadletter");
        assert_eq!(deadletter_history(db.pool(), &job).await, 1);
        let types = realtime_types(&mut pubsub, &job).await;
        assert!(types.iter().any(|t| t == "JobFailed"), "{types:?}");
        let data = delivered(db.pool(), &t.org_id, "job.failed", &job).await;
        assert_eq!(data["status"], "deadletter", "{data}");
    }
}
// ---- wave 5 / realtime ----
mod w5_realtime {
    use super::*;

    use std::net::SocketAddr;
    use std::time::Duration;

    use futures::StreamExt;
    use spooled_backend::api::handlers::auth::Claims;
    use spooled_backend::cache::RedisCache;
    use spooled_backend::config::RedisSettings;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};

    /// Router on `redis_url` (and on `pool` when given, else the test database).
    async fn realtime_app(
        db: &TestDatabase,
        redis_url: &str,
        pool: Option<sqlx::PgPool>,
    ) -> (Router, RedisCache) {
        let cache = RedisCache::connect(&RedisSettings {
            url: redis_url.to_string(),
            pool_size: 4,
        })
        .await
        .expect("connect redis");
        let pool = pool.map(Arc::new).unwrap_or_else(|| Arc::clone(&db.pool));
        let app = router(AppState::new(
            Database::from_pool(pool),
            Some(cache.clone()),
            Metrics::new(),
            Settings::load_for_testing(),
        ));
        (app, cache)
    }

    /// Serve `app` on a loopback port: a WebSocket upgrade needs a real connection.
    pub(super) async fn serve(app: Router) -> SocketAddr {
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

    #[derive(Debug)]
    pub(super) enum Frame {
        Text(String),
        Close(u16),
        Other,
    }

    /// A WebSocket client written by hand: the dev-dependencies carry none.
    pub(super) struct RawWs {
        tcp: TcpStream,
        buf: Vec<u8>,
    }

    /// Open `/api/v1/ws...`; `Err` is the HTTP status of a refused upgrade.
    pub(super) async fn ws_open(
        addr: SocketAddr,
        target: &str,
        headers: &[(&str, &str)],
    ) -> Result<RawWs, u16> {
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
                    .expect("handshake answer"),
                "connection closed during the handshake"
            );
        };
        let status: u16 = String::from_utf8_lossy(&ws.buf[..head_end])
            .split_whitespace()
            .nth(1)
            .and_then(|s| s.parse().ok())
            .expect("status line");
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

        /// The next server frame (servers do not mask), `None` once the connection ends.
        pub(super) async fn frame(&mut self) -> Option<Frame> {
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
            } else if len == 127 {
                if !self.fill(10).await {
                    return None;
                }
                len = u64::from_be_bytes(self.buf[2..10].try_into().unwrap()) as usize;
                header = 10;
            }
            if !self.fill(header + len).await {
                return None;
            }
            let payload: Vec<u8> = self.buf.drain(..header + len).skip(header).collect();
            Some(match opcode {
                1 => Frame::Text(String::from_utf8(payload).unwrap()),
                8 if payload.len() >= 2 => {
                    Frame::Close(u16::from_be_bytes([payload[0], payload[1]]))
                }
                8 => Frame::Close(1005),
                _ => Frame::Other,
            })
        }

        /// Frames that arrive within `limit`, up to a close frame or the end of
        /// the connection.
        pub(super) async fn frames_within(&mut self, limit: Duration) -> Vec<Frame> {
            let deadline = tokio::time::Instant::now() + limit;
            let mut frames = Vec::new();
            while let Ok(Some(frame)) = tokio::time::timeout_at(deadline, self.frame()).await {
                let closed = matches!(frame, Frame::Close(_));
                frames.push(frame);
                if closed {
                    break;
                }
            }
            frames
        }

        async fn text_containing(&mut self, needle: &str, limit: Duration) -> bool {
            let deadline = tokio::time::Instant::now() + limit;
            while let Ok(Some(frame)) = tokio::time::timeout_at(deadline, self.frame()).await {
                match frame {
                    Frame::Text(text) if text.contains(needle) => return true,
                    Frame::Close(_) => return false,
                    _ => {}
                }
            }
            false
        }
    }

    /// TCP proxy in front of Redis. `cut` drops every proxied connection, as a
    /// Redis restart or a network blip does.
    struct RedisProxy {
        url: String,
        links: Arc<std::sync::Mutex<Vec<tokio::task::JoinHandle<()>>>>,
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
        tokio::spawn(async move {
            while let Ok((mut inbound, _)) = listener.accept().await {
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
        }
    }

    impl RedisProxy {
        fn cut(&self) {
            for link in self.links.lock().unwrap().drain(..) {
                link.abort();
            }
        }
    }

    async fn publish_job_created(cache: &RedisCache, org_id: &str, job_id: &str) {
        let event = json!({
            "type": "JobCreated",
            "data": {
                "job_id": job_id,
                "queue_name": "w5-rt",
                "priority": 0,
                "timestamp": chrono::Utc::now(),
            },
        });
        cache
            .publish(&format!("org:{org_id}:events"), &event.to_string())
            .await
            .expect("publish");
    }

    async fn api_key_id(pool: &sqlx::PgPool, org_id: &str) -> String {
        sqlx::query_scalar("SELECT id FROM api_keys WHERE organization_id = $1")
            .bind(org_id)
            .fetch_one(pool)
            .await
            .expect("key id")
    }

    /// An access JWT for the org's key whose acceptance (`exp` plus the 60 s
    /// validation leeway) ends `secs` from now.
    async fn expiring_token(pool: &sqlx::PgPool, t: &Tenant, secs: i64) -> String {
        let api_key_id = api_key_id(pool, &t.org_id).await;
        let now = chrono::Utc::now().timestamp();
        let claims = Claims {
            sub: t.org_id.clone(),
            api_key_id,
            org_id: t.org_id.clone(),
            iat: now - 120,
            exp: now - 60 + secs,
            nbf: now - 120,
            jti: uuid::Uuid::new_v4().to_string(),
            queues: vec![],
            token_type: "access".to_string(),
            sid: Some(uuid::Uuid::new_v4().to_string()),
        };
        jsonwebtoken::encode(
            &jsonwebtoken::Header::default(),
            &claims,
            &jsonwebtoken::EncodingKey::from_secret(
                Settings::load_for_testing().jwt.secret.as_bytes(),
            ),
        )
        .unwrap()
    }

    async fn login(app: &Router, key: &str) -> String {
        let (status, body) = call(
            app,
            "POST",
            "/api/v1/auth/login",
            &[],
            Some(json!({"api_key": key})),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        body["access_token"].as_str().unwrap().to_string()
    }

    async fn logout(app: &Router, token: &str) {
        let (status, body) = call(
            app,
            "POST",
            "/api/v1/auth/logout",
            &[("Authorization", &bearer(token))],
            None,
        )
        .await;
        assert!(status.is_success(), "{status}: {body}");
    }

    async fn create_job(app: &Router, key: &str) -> String {
        let (status, body) = call(
            app,
            "POST",
            "/api/v1/jobs",
            &[("Authorization", &bearer(key))],
            Some(json!({"queue_name": "w5-rt", "payload": {}})),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        body["id"].as_str().unwrap().to_string()
    }

    pub(super) type SseBody = axum::body::BodyDataStream;

    pub(super) async fn sse_open(
        app: &Router,
        uri: &str,
        headers: &[(&str, &str)],
    ) -> (StatusCode, SseBody) {
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
    pub(super) async fn sse_read(stream: &mut SseBody, limit: Duration) -> (String, bool) {
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

    fn conn_key(kind: &str, org_id: &str) -> String {
        format!("realtime_connections:{kind}:{org_id}")
    }

    /// Put `n` connections with heartbeat `score` into an org's connection set.
    async fn seed_connections(cache: &RedisCache, key: &str, n: usize, score: i64) {
        let mut conn = cache.get_connection().await.unwrap();
        let mut pipe = redis::pipe();
        for _ in 0..n {
            pipe.zadd(key, uuid::Uuid::new_v4().to_string(), score)
                .ignore();
        }
        pipe.expire(key, 600).ignore();
        let _: () = pipe.query_async(&mut conn).await.unwrap();
    }

    async fn live_connections(cache: &RedisCache, key: &str) -> i64 {
        let mut conn = cache.get_connection().await.unwrap();
        redis::cmd("ZCARD")
            .arg(key)
            .query_async(&mut conn)
            .await
            .unwrap()
    }

    // BEFLOW-13 / BEAUTH-19: a WebSocket ends when its access token stops being
    // accepted, instead of streaming for as long as the key stays active.
    #[tokio::test]
    async fn websocket_closes_when_its_access_token_expires() {
        let db = TestDatabase::new().await;
        let t = seed_tenant(db.pool()).await;
        let (app, _cache, _redis) = app_with_redis(&db).await;
        let addr = serve(app).await;

        let token = expiring_token(db.pool(), &t, 3).await;
        let mut ws = ws_open(addr, &format!("/api/v1/ws?token={token}"), &[])
            .await
            .expect("upgrade");
        let frames = ws.frames_within(Duration::from_secs(10)).await;
        assert!(
            matches!(frames.last(), Some(Frame::Close(1008))),
            "the socket must close (1008) once the token expires: {frames:?}"
        );
    }

    // BEFLOW-13 / BEAUTH-19: logout (session revocation) ends an open WebSocket
    // before it reveals another event.
    #[tokio::test]
    async fn websocket_stops_after_logout() {
        let db = TestDatabase::new().await;
        let t = seed_tenant(db.pool()).await;
        let (app, cache, _redis) = app_with_redis(&db).await;
        let addr = serve(app.clone()).await;

        let token = login(&app, &t.key).await;
        let mut ws = ws_open(addr, &format!("/api/v1/ws?token={token}"), &[])
            .await
            .expect("upgrade");
        assert!(
            matches!(ws.frame().await, Some(Frame::Text(_))),
            "initial ping"
        );
        tokio::time::sleep(Duration::from_millis(300)).await;

        logout(&app, &token).await;
        publish_job_created(&cache, &t.org_id, "w5-after-logout").await;
        let frames = ws.frames_within(Duration::from_secs(6)).await;
        assert!(
            !frames
                .iter()
                .any(|f| matches!(f, Frame::Text(text) if text.contains("w5-after-logout"))),
            "no event after logout: {frames:?}"
        );
        assert!(
            matches!(frames.last(), Some(Frame::Close(1008))),
            "{frames:?}"
        );
    }

    // BEAUTH-19: the three SSE streams end when their access token expires.
    #[tokio::test]
    async fn sse_streams_end_when_their_access_token_expires() {
        let db = TestDatabase::new().await;
        let t = seed_tenant(db.pool()).await;
        let (app, _cache, _redis) = app_with_redis(&db).await;
        let job_id = create_job(&app, &t.key).await;

        let token = expiring_token(db.pool(), &t, 4).await;
        let (s1, mut events) = sse_open(&app, &format!("/api/v1/events?token={token}"), &[]).await;
        let (s2, mut job) = sse_open(
            &app,
            &format!("/api/v1/events/jobs/{job_id}?token={token}"),
            &[],
        )
        .await;
        let (s3, mut queue) = sse_open(
            &app,
            &format!("/api/v1/events/queues/w5-rt?token={token}"),
            &[],
        )
        .await;
        assert_eq!(
            (s1, s2, s3),
            (StatusCode::OK, StatusCode::OK, StatusCode::OK)
        );

        let limit = Duration::from_secs(12);
        let ((_, e1), (_, e2), (_, e3)) = tokio::join!(
            sse_read(&mut events, limit),
            sse_read(&mut job, limit),
            sse_read(&mut queue, limit)
        );
        assert!(e1, "/events must end at token expiry");
        assert!(e2, "/events/jobs/{{id}} must end at token expiry");
        assert!(e3, "/events/queues/{{name}} must end at token expiry");
    }

    // BEAUTH-19: logout ends the global SSE stream before it reveals another event.
    #[tokio::test]
    async fn sse_events_stop_after_logout() {
        let db = TestDatabase::new().await;
        let t = seed_tenant(db.pool()).await;
        let (app, cache, _redis) = app_with_redis(&db).await;

        let token = login(&app, &t.key).await;
        let (status, mut stream) =
            sse_open(&app, &format!("/api/v1/events?token={token}"), &[]).await;
        assert_eq!(status, StatusCode::OK);
        let (first, _) = sse_read(&mut stream, Duration::from_millis(500)).await;
        assert!(first.contains("connected"), "{first}");

        logout(&app, &token).await;
        publish_job_created(&cache, &t.org_id, "w5-sse-after-logout").await;
        let (seen, ended) = sse_read(&mut stream, Duration::from_secs(6)).await;
        assert!(
            !seen.contains("w5-sse-after-logout"),
            "no event after logout: {seen}"
        );
        assert!(ended, "the stream must end after logout: {seen}");
    }

    // BEFLOW-12: when the Redis pub/sub link drops, the socket says so and
    // closes, instead of answering pings while never delivering an event.
    #[tokio::test]
    async fn websocket_closes_when_its_event_channel_drops() {
        let db = TestDatabase::new().await;
        let t = seed_tenant(db.pool()).await;
        let redis = common::TestRedis::new().await;
        let proxy = redis_proxy(&redis.url).await;
        let (app, _proxied) = realtime_app(&db, &proxy.url, None).await;
        let (_direct_app, direct) = realtime_app(&db, &redis.url, None).await;
        let addr = serve(app).await;

        let auth = bearer(&t.key);
        let mut ws = ws_open(addr, "/api/v1/ws", &[("Authorization", &auth)])
            .await
            .expect("upgrade");
        assert!(
            matches!(ws.frame().await, Some(Frame::Text(_))),
            "initial ping"
        );
        tokio::time::sleep(Duration::from_millis(500)).await;
        publish_job_created(&direct, &t.org_id, "w5-before-cut").await;
        assert!(
            ws.text_containing("w5-before-cut", Duration::from_secs(5))
                .await,
            "events flow before the cut"
        );

        proxy.cut();
        let frames = ws.frames_within(Duration::from_secs(8)).await;
        assert!(
            frames
                .iter()
                .any(|f| matches!(f, Frame::Text(text) if text.contains("\"type\":\"Error\""))),
            "the client is told why: {frames:?}"
        );
        assert!(
            matches!(frames.last(), Some(Frame::Close(1011))),
            "{frames:?}"
        );
    }

    // BEFLOW-14: the socket re-checks its key on a timer, not with one database
    // query per forwarded event.
    #[tokio::test]
    async fn websocket_does_not_query_the_database_per_event() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let db = TestDatabase::new().await;
        let t = seed_tenant(db.pool()).await;
        let redis = common::TestRedis::new().await;
        // Every query on the pool hands its connection back once: count those.
        let released = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&released);
        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(5)
            .after_release(move |_, _| {
                counter.fetch_add(1, Ordering::SeqCst);
                Box::pin(async { Ok(true) })
            })
            .connect_with((*db.pool.connect_options()).clone())
            .await
            .unwrap();
        let (app, cache) = realtime_app(&db, &redis.url, Some(pool)).await;
        let addr = serve(app).await;

        let auth = bearer(&t.key);
        let mut ws = ws_open(addr, "/api/v1/ws", &[("Authorization", &auth)])
            .await
            .expect("upgrade");
        assert!(
            matches!(ws.frame().await, Some(Frame::Text(_))),
            "initial ping"
        );
        tokio::time::sleep(Duration::from_millis(300)).await;
        publish_job_created(&cache, &t.org_id, "w5-warmup").await;
        assert!(
            ws.text_containing("w5-warmup", Duration::from_secs(5))
                .await
        );
        tokio::time::sleep(Duration::from_millis(300)).await;

        let before = released.load(Ordering::SeqCst);
        for i in 0..20 {
            publish_job_created(&cache, &t.org_id, &format!("w5-burst-{i}")).await;
        }
        assert!(
            ws.text_containing("w5-burst-19", Duration::from_secs(5))
                .await
        );
        tokio::time::sleep(Duration::from_millis(300)).await;
        let queries = released.load(Ordering::SeqCst) - before;
        assert!(queries <= 2, "20 events cost {queries} database queries");
    }

    // BEAUTH-14: a counter leaked by a replica that died without cleanup (the
    // pre-0.1.121 `ws_connections` / `sse_connections` keys) no longer locks the
    // org out.
    #[tokio::test]
    async fn a_leaked_legacy_connection_counter_does_not_lock_the_org_out() {
        let db = TestDatabase::new().await;
        let t = seed_tenant(db.pool()).await;
        let (app, cache, _redis) = app_with_redis(&db).await;
        cache
            .set(&format!("ws_connections:{}", t.org_id), "100", 600)
            .await
            .unwrap();
        cache
            .set(&format!("sse_connections:{}", t.org_id), "100", 600)
            .await
            .unwrap();
        let addr = serve(app.clone()).await;

        let auth = bearer(&t.key);
        assert!(ws_open(addr, "/api/v1/ws", &[("Authorization", &auth)])
            .await
            .is_ok());
        let (status, _) = sse_open(&app, "/api/v1/events", &[("Authorization", &auth)]).await;
        assert_eq!(status, StatusCode::OK);
    }

    // BEAUTH-14 / BEFLOW-15: every realtime stream kind is capped per org, and
    // only connections with a recent heartbeat count toward the cap.
    #[tokio::test]
    async fn realtime_caps_count_only_live_connections() {
        let db = TestDatabase::new().await;
        let t = seed_tenant(db.pool()).await;
        let (app, cache, _redis) = app_with_redis(&db).await;
        let addr = serve(app.clone()).await;
        let job_id = create_job(&app, &t.key).await;
        let auth = bearer(&t.key);
        let headers = [("Authorization", auth.as_str())];
        let job_uri = format!("/api/v1/events/jobs/{job_id}");
        let streams = [
            "/api/v1/events",
            job_uri.as_str(),
            "/api/v1/events/queues/w5-rt",
        ];
        let now = chrono::Utc::now().timestamp();

        // A replica that was killed leaves members whose heartbeat stopped.
        for kind in ["ws", "events", "job", "queue"] {
            seed_connections(&cache, &conn_key(kind, &t.org_id), 100, now - 3600).await;
        }
        assert!(
            ws_open(addr, "/api/v1/ws", &headers).await.is_ok(),
            "stale members do not count"
        );
        for uri in streams {
            let (status, _) = sse_open(&app, uri, &headers).await;
            assert_eq!(status, StatusCode::OK, "{uri}: stale members do not count");
        }

        for kind in ["ws", "events", "job", "queue"] {
            seed_connections(&cache, &conn_key(kind, &t.org_id), 100, now).await;
        }
        assert_eq!(ws_open(addr, "/api/v1/ws", &headers).await.err(), Some(429));
        for uri in streams {
            let (status, _) = sse_open(&app, uri, &headers).await;
            assert_eq!(
                status,
                StatusCode::TOO_MANY_REQUESTS,
                "{uri}: 100 live connections"
            );
        }
    }

    // BEAUTH-14: a closed stream leaves the live-connection set at once.
    #[tokio::test]
    async fn realtime_streams_leave_the_connection_set_on_disconnect() {
        let db = TestDatabase::new().await;
        let t = seed_tenant(db.pool()).await;
        let (app, cache, _redis) = app_with_redis(&db).await;
        let addr = serve(app.clone()).await;
        let job_id = create_job(&app, &t.key).await;
        let auth = bearer(&t.key);

        async fn drains(cache: &RedisCache, key: &str) -> bool {
            for _ in 0..30 {
                if live_connections(cache, key).await == 0 {
                    return true;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            false
        }

        let job_key = conn_key("job", &t.org_id);
        let (status, stream) = sse_open(
            &app,
            &format!("/api/v1/events/jobs/{job_id}"),
            &[("Authorization", &auth)],
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(live_connections(&cache, &job_key).await, 1);
        drop(stream);
        assert!(drains(&cache, &job_key).await, "job stream slot released");

        let ws_key = conn_key("ws", &t.org_id);
        let ws = ws_open(addr, "/api/v1/ws", &[("Authorization", &auth)])
            .await
            .expect("upgrade");
        assert_eq!(live_connections(&cache, &ws_key).await, 1);
        drop(ws);
        assert!(drains(&cache, &ws_key).await, "WebSocket slot released");
    }
}
// ---- wave 5 / hooks ----

mod wave5_hooks {
    use super::*;

    use std::collections::HashSet;
    use std::net::SocketAddr;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;
    use std::time::{Duration, Instant};

    use axum::extract::ConnectInfo;

    use spooled_backend::outgoing_webhooks::service::{
        spawn_completion_webhook, OutgoingWebhookService,
    };

    /// A local receiver: answers every request with `status` and `body` after
    /// `delay`, and counts requests, peak concurrency and TCP connections.
    pub(super) struct Endpoint {
        pub(super) url: String,
        stats: Arc<EndpointStats>,
    }

    #[derive(Default)]
    struct EndpointStats {
        hits: AtomicUsize,
        in_flight: AtomicUsize,
        peak: AtomicUsize,
        peers: Mutex<HashSet<SocketAddr>>,
    }

    /// Leaves `in_flight` even when the client gives up mid-request.
    struct InFlight(Arc<EndpointStats>);

    impl Drop for InFlight {
        fn drop(&mut self) {
            self.0.in_flight.fetch_sub(1, Ordering::SeqCst);
        }
    }

    impl Endpoint {
        pub(super) async fn start(status: u16, delay: Duration, body: String) -> Self {
            let stats = Arc::new(EndpointStats::default());
            let shared = Arc::clone(&stats);
            let app = Router::new().fallback(move |ConnectInfo(peer): ConnectInfo<SocketAddr>| {
                let (stats, body) = (Arc::clone(&shared), body.clone());
                async move {
                    stats.hits.fetch_add(1, Ordering::SeqCst);
                    stats.peers.lock().unwrap().insert(peer);
                    let now = stats.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
                    stats.peak.fetch_max(now, Ordering::SeqCst);
                    let _guard = InFlight(Arc::clone(&stats));
                    tokio::time::sleep(delay).await;
                    (StatusCode::from_u16(status).unwrap(), body)
                }
            });
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            tokio::spawn(async move {
                axum::serve(
                    listener,
                    app.into_make_service_with_connect_info::<SocketAddr>(),
                )
                .await
                .unwrap();
            });
            Self {
                url: format!("http://{addr}/hook"),
                stats,
            }
        }

        pub(super) fn hits(&self) -> usize {
            self.stats.hits.load(Ordering::SeqCst)
        }

        pub(super) fn peak(&self) -> usize {
            self.stats.peak.load(Ordering::SeqCst)
        }

        fn connections(&self) -> usize {
            self.stats.peers.lock().unwrap().len()
        }
    }

    pub(super) async fn insert_hook(pool: &sqlx::PgPool, org_id: &str, url: &str) -> String {
        sqlx::query_scalar(
            "INSERT INTO outgoing_webhooks (organization_id, name, url, events) \
             VALUES ($1, 'w5', $2, ARRAY['job.completed']) RETURNING id",
        )
        .bind(org_id)
        .bind(url)
        .fetch_one(pool)
        .await
        .unwrap()
    }

    pub(super) fn fire(service: &Arc<OutgoingWebhookService>, org_id: &str) {
        let job_id = uuid::Uuid::new_v4().to_string();
        service.spawn_dispatch(
            org_id.to_string(),
            "job.completed".to_string(),
            job_id.clone(),
            json!({ "job_id": job_id, "status": "completed" }),
        );
    }

    pub(super) async fn eventually(within: Duration, mut check: impl FnMut() -> bool) -> bool {
        let deadline = Instant::now() + within;
        loop {
            if check() {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// Poll a webhook's newest delivery row until it exists.
    async fn await_delivery(pool: &sqlx::PgPool, hook_id: &str, within: Duration) -> DeliveryRow {
        let deadline = Instant::now() + within;
        loop {
            if let Some(row) = last_delivery(pool, hook_id).await {
                return row;
            }
            assert!(
                Instant::now() < deadline,
                "no delivery recorded for {hook_id}"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// (status, status_code, response_body, error, attempts) of a webhook's
    /// newest delivery row.
    type DeliveryRow = (String, Option<i32>, Option<String>, Option<String>, i32);

    async fn last_delivery(pool: &sqlx::PgPool, hook_id: &str) -> Option<DeliveryRow> {
        sqlx::query_as(
            "SELECT status, status_code, response_body, error, attempts \
             FROM outgoing_webhook_deliveries WHERE webhook_id = $1 \
             ORDER BY created_at DESC LIMIT 1",
        )
        .bind(hook_id)
        .fetch_optional(pool)
        .await
        .unwrap()
    }

    // BEFLOW-10: a slow endpoint gets at most a quarter of the delivery slots,
    // so another tenant's delivery goes straight out instead of queueing
    // behind its backlog.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn w5_hooks_slow_endpoint_does_not_hold_every_delivery_slot() {
        let db = TestDatabase::new().await;
        let slow_org = seed_tenant(db.pool()).await;
        let fast_org = seed_tenant(db.pool()).await;
        let slow = Endpoint::start(200, Duration::from_secs(3), "ok".into()).await;
        let fast = Endpoint::start(200, Duration::ZERO, "ok".into()).await;
        insert_hook(db.pool(), &slow_org.org_id, &slow.url).await;
        insert_hook(db.pool(), &fast_org.org_id, &fast.url).await;
        let service = Arc::new(OutgoingWebhookService::new((*db.pool).clone(), 1, 4));

        for _ in 0..8 {
            fire(&service, &slow_org.org_id);
        }
        assert!(eventually(Duration::from_secs(5), || slow.hits() >= 1).await);
        tokio::time::sleep(Duration::from_millis(200)).await;

        fire(&service, &fast_org.org_id);
        assert!(
            eventually(Duration::from_millis(1500), || fast.hits() == 1).await,
            "the fast tenant waited behind the slow endpoint"
        );
        assert_eq!(slow.peak(), 1, "4 slots / 4 = one request per webhook");
    }

    // BEFLOW-10: a delivery sleeping before its retry holds no slot.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn w5_hooks_retry_backoff_does_not_hold_a_delivery_slot() {
        let db = TestDatabase::new().await;
        let down_org = seed_tenant(db.pool()).await;
        let up_org = seed_tenant(db.pool()).await;
        let down = Endpoint::start(500, Duration::ZERO, "down".into()).await;
        let up = Endpoint::start(200, Duration::ZERO, "ok".into()).await;
        insert_hook(db.pool(), &down_org.org_id, &down.url).await;
        insert_hook(db.pool(), &up_org.org_id, &up.url).await;
        // One slot, three attempts: 1 s then 2 s of backoff.
        let service = Arc::new(OutgoingWebhookService::new((*db.pool).clone(), 3, 1));

        fire(&service, &down_org.org_id);
        assert!(eventually(Duration::from_secs(5), || down.hits() == 1).await);
        fire(&service, &up_org.org_id);
        assert!(
            eventually(Duration::from_millis(800), || up.hits() == 1).await,
            "the only slot was held through the other delivery's backoff"
        );
        assert!(
            eventually(Duration::from_secs(6), || down.hits() == 3).await,
            "the failing delivery still retries"
        );
    }

    // BEFLOW-10: deliveries already queued stop as soon as the webhook is
    // disabled (by the user or the breaker), instead of draining for minutes.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn w5_hooks_disabling_a_webhook_stops_its_queued_retries() {
        let db = TestDatabase::new().await;
        let t = seed_tenant(db.pool()).await;
        let down = Endpoint::start(500, Duration::ZERO, "down".into()).await;
        let hook = insert_hook(db.pool(), &t.org_id, &down.url).await;
        let service = Arc::new(OutgoingWebhookService::new((*db.pool).clone(), 3, 4));

        fire(&service, &t.org_id);
        assert!(eventually(Duration::from_secs(5), || down.hits() == 1).await);
        sqlx::query("UPDATE outgoing_webhooks SET enabled = FALSE WHERE id = $1")
            .bind(&hook)
            .execute(db.pool())
            .await
            .unwrap();

        // Attempt 2 was due ~1 s after attempt 1, attempt 3 ~2 s after that.
        tokio::time::sleep(Duration::from_millis(3500)).await;
        assert_eq!(down.hits(), 1, "a disabled webhook was still called");
        let (status, _, _, _, attempts) = last_delivery(db.pool(), &hook).await.unwrap();
        assert_eq!((status.as_str(), attempts), ("failed", 1));
    }

    // BEFLOW-10: past the queue bound deliveries are dropped (and counted)
    // instead of parking one task and one payload copy each.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn w5_hooks_a_full_delivery_queue_drops_instead_of_parking() {
        let db = TestDatabase::new().await;
        let t = seed_tenant(db.pool()).await;
        let slow = Endpoint::start(200, Duration::from_millis(200), "ok".into()).await;
        insert_hook(db.pool(), &t.org_id, &slow.url).await;
        // One slot: a queue of 16, of which one webhook may hold 4.
        let service = Arc::new(OutgoingWebhookService::new((*db.pool).clone(), 1, 1));

        for _ in 0..20 {
            fire(&service, &t.org_id);
        }
        assert!(eventually(Duration::from_secs(5), || slow.hits() >= 4).await);
        tokio::time::sleep(Duration::from_millis(1200)).await;
        assert_eq!(
            slow.hits(),
            4,
            "only the webhook's queue share is delivered"
        );
        assert_eq!(
            service.deliveries_dropped(),
            16,
            "4 events past the queue, then 12 deliveries past the webhook's share"
        );
        assert_eq!(
            service.deliveries_queued(),
            0,
            "every queue slot is released"
        );
    }

    // BEFLOW-24: the delivery log keeps the error text in `error` and the
    // receiver's (truncated) answer in `response_body`; no response means a
    // null status code, not 0.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn w5_hooks_delivery_log_keeps_the_error_and_the_response_body() {
        let db = TestDatabase::new().await;
        let t = seed_tenant(db.pool()).await;
        let body = format!("maintenance until 10:00 {}", "x".repeat(5000));
        let busy = Endpoint::start(503, Duration::ZERO, body).await;
        let busy_hook = insert_hook(db.pool(), &t.org_id, &busy.url).await;
        let closed_port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let dead_hook = insert_hook(
            db.pool(),
            &t.org_id,
            &format!("http://127.0.0.1:{closed_port}/hook"),
        )
        .await;
        let service = Arc::new(OutgoingWebhookService::new((*db.pool).clone(), 1, 4));

        fire(&service, &t.org_id);

        let (status, code, response_body, error, _) =
            await_delivery(db.pool(), &busy_hook, Duration::from_secs(15)).await;
        assert_eq!((status.as_str(), code), ("failed", Some(503)));
        assert_eq!(error.as_deref(), Some("HTTP 503"));
        let response_body = response_body.expect("the receiver's answer is stored");
        assert!(
            response_body.starts_with("maintenance until 10:00 x"),
            "{response_body}"
        );
        assert!(
            response_body.len() <= 1024,
            "truncated: {}",
            response_body.len()
        );

        let (status, code, response_body, error, _) =
            await_delivery(db.pool(), &dead_hook, Duration::from_secs(15)).await;
        assert_eq!((status.as_str(), code), ("failed", None));
        assert_eq!(response_body, None);
        assert!(
            error.is_some_and(|e| !e.is_empty()),
            "the transport error is stored"
        );
    }

    // BEFLOW-25: re-enabling a webhook or pointing it at a new URL starts a
    // fresh failure streak; other edits keep it.
    #[tokio::test]
    async fn w5_hooks_reenabling_or_repointing_a_webhook_resets_its_failure_count() {
        let db = TestDatabase::new().await;
        let t = seed_tenant(db.pool()).await;
        let app = app(&db);
        let auth = bearer(&t.key);
        let hook: String = sqlx::query_scalar(
            "INSERT INTO outgoing_webhooks \
               (organization_id, name, url, events, enabled, failure_count, last_status) \
             VALUES ($1, 'w5', 'http://127.0.0.1:9/a', ARRAY['job.completed'], FALSE, 20, 'auto_disabled') \
             RETURNING id",
        )
        .bind(&t.org_id)
        .fetch_one(db.pool())
        .await
        .unwrap();
        let uri = format!("/api/v1/outgoing-webhooks/{hook}");
        let headers = [("Authorization", auth.as_str())];
        let set_failures = |n: i32| {
            sqlx::query("UPDATE outgoing_webhooks SET failure_count = $2 WHERE id = $1")
                .bind(&hook)
                .bind(n)
                .execute(db.pool())
        };

        let (status, body) = call(
            &app,
            "PUT",
            &uri,
            &headers,
            Some(json!({ "enabled": true })),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["failure_count"], 0, "{body}");
        // The dashboard's "Re-enabled" badge reads this until the next delivery.
        assert_eq!(body["last_status"], "auto_disabled", "{body}");

        set_failures(7).await.unwrap();
        let (_, body) = call(
            &app,
            "PUT",
            &uri,
            &headers,
            Some(json!({ "name": "renamed" })),
        )
        .await;
        assert_eq!(body["failure_count"], 7, "{body}");
        let same_url = json!({ "url": "http://127.0.0.1:9/a" });
        let (_, body) = call(&app, "PUT", &uri, &headers, Some(same_url)).await;
        assert_eq!(body["failure_count"], 7, "same URL: {body}");
        let new_url = json!({ "url": "http://127.0.0.1:9/b" });
        let (status, body) = call(&app, "PUT", &uri, &headers, Some(new_url)).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["failure_count"], 0, "new URL: {body}");
    }

    // BEFLOW-25: a stored URL that fails the SSRF check is a failed delivery,
    // so it trips the auto-disable breaker like any other dead endpoint.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn w5_hooks_a_blocked_url_counts_toward_auto_disable() {
        let db = TestDatabase::new().await;
        let t = seed_tenant(db.pool()).await;
        let hook: String = sqlx::query_scalar(
            "INSERT INTO outgoing_webhooks (organization_id, name, url, events, failure_count) \
             VALUES ($1, 'w5', 'http://169.254.169.254/latest/meta-data', ARRAY['job.completed'], 19) \
             RETURNING id",
        )
        .bind(&t.org_id)
        .fetch_one(db.pool())
        .await
        .unwrap();
        let service = Arc::new(OutgoingWebhookService::new((*db.pool).clone(), 3, 4));

        fire(&service, &t.org_id);
        await_delivery(db.pool(), &hook, Duration::from_secs(5)).await;
        // A retry would have landed 1 s after the first attempt.
        tokio::time::sleep(Duration::from_millis(1500)).await;
        let (status, _, _, error, attempts) = last_delivery(db.pool(), &hook).await.unwrap();
        let state: (bool, i32, Option<String>) = sqlx::query_as(
            "SELECT enabled, failure_count, last_status FROM outgoing_webhooks WHERE id = $1",
        )
        .bind(&hook)
        .fetch_one(db.pool())
        .await
        .unwrap();
        assert_eq!(state, (false, 20, Some("auto_disabled".to_string())));
        assert_eq!((status.as_str(), attempts), ("failed", 1), "no retries");
        assert!(
            error
                .as_deref()
                .is_some_and(|e| e.contains("Invalid webhook URL")),
            "{error:?}"
        );
    }

    // BEFLOW-27: completion webhooks go through the org-webhook service's
    // client (connections are reused) and a bounded slot pool of their own
    // (W5-R2: not the org webhooks' slots).
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn w5_hooks_completion_webhooks_share_the_client_and_are_bounded() {
        let db = TestDatabase::new().await;
        let slow = Endpoint::start(200, Duration::from_millis(400), "ok".into()).await;
        // The first service in the process (AppState's, in production) lends
        // completion deliveries its client and its limits: two slots.
        let _service = OutgoingWebhookService::new((*db.pool).clone(), 1, 2);

        for i in 0..6 {
            spawn_completion_webhook(
                "w5-hooks-org",
                slow.url.clone(),
                "job.completed".to_string(),
                json!({ "job_id": i }),
                None,
            );
        }
        assert!(eventually(Duration::from_secs(10), || slow.hits() == 6).await);
        assert!(slow.peak() <= 2, "peak concurrency {}", slow.peak());
        assert!(
            slow.connections() <= 2,
            "{} connections for 6 deliveries",
            slow.connections()
        );
    }
}
// ---- wave 5 / keys ----
// FEAT B2 / QW6 queue-prefix API keys, W1-14 pending billing email, BEJOB-19
// worker registration conflicts.
mod wave5_keys {
    use super::*;

    use tonic::{Code, Request as GrpcRequest};

    use spooled_backend::config::{QueueSettings, WorkerSettings};
    use spooled_backend::grpc::proto::queue_service_server::QueueService;
    use spooled_backend::grpc::proto::{DequeueRequest, EnqueueRequest};
    use spooled_backend::grpc::services::QueueServiceImpl;
    use spooled_backend::outgoing_webhooks::service::OutgoingWebhookService;

    const ADMIN_KEY: &str = "w5-keys-admin-key";

    fn admin_app(db: &TestDatabase) -> Router {
        let mut settings = Settings::load_for_testing();
        settings.registration.admin_api_key = Some(ADMIN_KEY.to_string());
        app_with(db, settings)
    }

    /// An API key of `org_id` limited to `queues`, inserted directly.
    async fn seed_scoped_key(pool: &sqlx::PgPool, org_id: &str, queues: &[&str]) -> String {
        let key = format!("sp_test_{}", uuid::Uuid::new_v4().simple());
        sqlx::query(
            "INSERT INTO api_keys (id, organization_id, key_hash, key_prefix, lookup_hash, name, queues, is_active, created_at) \
             VALUES ($1, $2, $3, $4, $5, 'w5 scoped', $6, TRUE, NOW())",
        )
        .bind(uuid::Uuid::new_v4().to_string())
        .bind(org_id)
        .bind(bcrypt::hash(&key, 4).expect("hash key"))
        .bind(key.chars().take(8).collect::<String>())
        .bind(spooled_backend::models::api_key_lookup_hash(&key))
        .bind(queues.iter().map(|q| q.to_string()).collect::<Vec<_>>())
        .execute(pool)
        .await
        .expect("seed scoped key");
        key
    }

    async fn enqueue(app: &Router, auth: &str, queue: &str) -> (StatusCode, Value) {
        call(
            app,
            "POST",
            "/api/v1/jobs",
            &[("Authorization", auth)],
            Some(json!({"queue_name": queue, "payload": {"q": queue}})),
        )
        .await
    }

    async fn claim(app: &Router, auth: &str, queue: &str) -> (StatusCode, Value) {
        call(
            app,
            "POST",
            "/api/v1/jobs/claim",
            &[("Authorization", auth)],
            Some(json!({"queue_name": queue, "worker_id": "w5-prefix-worker", "limit": 1})),
        )
        .await
    }

    /// Id of the key named `name`.
    async fn key_id(app: &Router, auth: &str, name: &str) -> String {
        let (status, body) = call(
            app,
            "GET",
            "/api/v1/api-keys",
            &[("Authorization", auth)],
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        body.as_array()
            .expect("key list")
            .iter()
            .find(|k| k["name"] == name)
            .and_then(|k| k["id"].as_str())
            .expect("named key")
            .to_string()
    }

    // FEAT B2 / QW6: `orders.*` was accepted at create and then matched no queue,
    // and `*` anywhere else in an entry was accepted too.
    #[tokio::test]
    async fn w5_keys_prefix_key_reaches_matching_queues_only() {
        let db = TestDatabase::new().await;
        let t = seed_tenant(db.pool()).await;
        let app = app(&db);
        let admin = bearer(&t.key);

        let (status, body) = call(
            &app,
            "POST",
            "/api/v1/api-keys",
            &[("Authorization", &admin)],
            Some(json!({"name": "orders prefix", "queues": ["orders.*"]})),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        let prefix = bearer(body["key"].as_str().expect("key"));

        for queue in ["orders.eu", "orders.eu.west"] {
            let (status, body) = enqueue(&app, &prefix, queue).await;
            assert_eq!(status, StatusCode::CREATED, "{queue}: {body}");
            let (status, body) = claim(&app, &prefix, queue).await;
            assert_eq!(status, StatusCode::OK, "{queue}: {body}");
            assert_eq!(body["jobs"][0]["queue_name"], queue, "{body}");
        }
        for queue in ["orders", "ordersx", "billing"] {
            let (status, body) = enqueue(&app, &prefix, queue).await;
            assert_eq!(status, StatusCode::FORBIDDEN, "{queue}: {body}");
            let (status, body) = claim(&app, &prefix, queue).await;
            assert_eq!(status, StatusCode::FORBIDDEN, "{queue}: {body}");
        }

        // Any other use of `*` is refused instead of minting a key for no queue.
        for queues in [json!(["orders*"]), json!(["*.eu"]), json!(["orders.*.eu"])] {
            let (status, body) = call(
                &app,
                "POST",
                "/api/v1/api-keys",
                &[("Authorization", &admin)],
                Some(json!({"name": "bad pattern", "queues": queues})),
            )
            .await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{queues}: {body}");
        }
        let id = key_id(&app, &admin, "orders prefix").await;
        let (status, body) = call(
            &app,
            "PUT",
            &format!("/api/v1/api-keys/{id}"),
            &[("Authorization", &admin)],
            Some(json!({"queues": ["restaurant.prints*"]})),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
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

    fn grpc<T>(key: &str, body: T) -> GrpcRequest<T> {
        let mut request = GrpcRequest::new(body);
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

    fn dequeue_req(queue: &str) -> DequeueRequest {
        DequeueRequest {
            queue_name: queue.to_string(),
            worker_id: "w5-grpc-prefix-worker".to_string(),
            lease_duration_secs: 60,
            batch_size: 1,
        }
    }

    // FEAT B2 / QW6, gRPC side: the same scope rule as REST.
    #[tokio::test]
    async fn w5_keys_grpc_prefix_key_reaches_matching_queues_only() {
        let db = TestDatabase::new().await;
        let t = seed_tenant(db.pool()).await;
        let key = seed_scoped_key(db.pool(), &t.org_id, &["restaurant.prints.*"]).await;
        let service = queue_service(&db);

        let queue = "restaurant.prints.store-12";
        let enqueued = service
            .enqueue(grpc(&key, enqueue_req(queue)))
            .await
            .expect("enqueue in a covered queue")
            .into_inner();
        let claimed = service
            .dequeue(grpc(&key, dequeue_req(queue)))
            .await
            .expect("dequeue from a covered queue")
            .into_inner();
        assert_eq!(claimed.jobs.len(), 1);
        assert_eq!(claimed.jobs[0].id, enqueued.job_id);

        for queue in [
            "restaurant.prints",
            "restaurant.printsx",
            "restaurant.kitchen",
        ] {
            let err = service
                .enqueue(grpc(&key, enqueue_req(queue)))
                .await
                .expect_err(queue);
            assert_eq!(err.code(), Code::PermissionDenied, "{queue}");
            let err = service
                .dequeue(grpc(&key, dequeue_req(queue)))
                .await
                .expect_err(queue);
            assert_eq!(err.code(), Code::PermissionDenied, "{queue}");
        }
    }

    // The SQL twin of the scope rule agrees with the Rust one, so the queries
    // that filter rows by a key's scope can move to it.
    #[tokio::test]
    async fn w5_keys_queue_in_key_scope_sql_matches_the_rust_rule() {
        let db = TestDatabase::new().await;
        let scopes: &[&[&str]] = &[
            &[],
            &["*"],
            &["orders.*"],
            &["orders.*", "billing"],
            &["restaurant.prints.*"],
            &["a_b.*"],
            &["a_b"],
            &["orders*"],
            &[".*"],
            &["a.*.*", "x.*"],
            &["billing", "emails"],
        ];
        let queues = [
            "orders",
            "orders.eu",
            "orders.eu.west",
            "ordersx",
            "ordersx.eu",
            "billing",
            "billing.eu",
            "emails",
            "restaurant.prints",
            "restaurant.prints.s1",
            "a_b",
            "axb",
            "a_b.x",
            "axb.x",
            "x.foo",
            "a.b.c",
            "foo",
        ];
        for scope in scopes {
            let scope: Vec<String> = scope.iter().map(|s| s.to_string()).collect();
            for queue in queues {
                let (in_sql,): (bool,) = sqlx::query_as("SELECT queue_in_key_scope($1, $2)")
                    .bind(queue)
                    .bind(&scope)
                    .fetch_one(db.pool())
                    .await
                    .unwrap();
                assert_eq!(
                    in_sql,
                    spooled_backend::models::queue_scope_allows(&scope, queue),
                    "scope {scope:?}, queue {queue}"
                );
            }
        }
        // NULL is what `queue_scope_filter()` binds for an unrestricted key.
        let (all,): (bool,) = sqlx::query_as("SELECT queue_in_key_scope('anything', NULL)")
            .fetch_one(db.pool())
            .await
            .unwrap();
        assert!(all);
    }

    async fn org_row(app: &Router, auth: &str, org_id: &str) -> Value {
        let (status, body) = call(
            app,
            "GET",
            &format!("/api/v1/organizations/{org_id}"),
            &[("Authorization", auth)],
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        body
    }

    async fn email_exists(app: &Router, email: &str) -> bool {
        let (status, body) = call(
            app,
            "GET",
            &format!("/api/v1/auth/check-email?email={email}"),
            &[],
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        body["exists"].as_bool().expect("exists")
    }

    // W1-14: changing a verified billing email dropped email login at once; the
    // new address now waits in `pending_billing_email` until it is verified.
    #[tokio::test]
    async fn w5_keys_billing_email_change_waits_until_verified() {
        let db = TestDatabase::new().await;
        let t = seed_tenant(db.pool()).await;
        let app = admin_app(&db);
        let auth = bearer(&t.key);
        let org_path = format!("/api/v1/organizations/{}", t.org_id);
        let old = org_row(&app, &auth, &t.org_id).await["billing_email"]
            .as_str()
            .expect("seeded billing email")
            .to_string();
        let new = format!("w5-new-{}@example.com", &t.org_id[..8]);

        let (status, body) = call(
            &app,
            "PUT",
            &org_path,
            &[("Authorization", &auth)],
            Some(json!({"billing_email": new.to_uppercase()})),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["billing_email"], old, "{body}");
        assert!(!body["billing_email_verified_at"].is_null(), "{body}");
        assert_eq!(body["pending_billing_email"], new, "{body}");
        let row = org_row(&app, &auth, &t.org_id).await;
        assert_eq!(row["billing_email"], old, "{row}");
        assert_eq!(row["pending_billing_email"], new, "{row}");

        // The verified address keeps working for email login; the pending one
        // is no login identity yet.
        assert!(email_exists(&app, &old).await);
        assert!(!email_exists(&app, &new).await);
        let (status, body) = call(
            &app,
            "POST",
            "/api/v1/auth/email/start",
            &[],
            Some(json!({"email": old})),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let (_, bound) = latest_code(db.pool(), &old).await;
        assert_eq!(bound.as_deref(), Some(t.org_id.as_str()));
        let (status, body) = call(
            &app,
            "POST",
            "/api/v1/auth/email/start",
            &[],
            Some(json!({"email": new})),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let (_, bound) = latest_code(db.pool(), &new).await;
        assert_eq!(bound, None, "a pending address must not bind a login code");

        // The confirmation code goes to the pending address.
        let (status, body) = call(
            &app,
            "POST",
            "/api/v1/organizations/billing-email/verification",
            &[("Authorization", &auth)],
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["status"], "code_sent", "{body}");
        let (stored, bound) = latest_code(db.pool(), &new).await;
        assert_eq!(bound.as_deref(), Some(t.org_id.as_str()));
        let code = stored
            .strip_prefix("verify:")
            .expect("verify prefix")
            .to_string();

        let (status, body) = call(
            &app,
            "POST",
            "/api/v1/organizations/billing-email/verify",
            &[("Authorization", &auth)],
            Some(json!({"code": code})),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["status"], "verified", "{body}");
        let row = org_row(&app, &auth, &t.org_id).await;
        assert_eq!(row["billing_email"], new, "{row}");
        assert!(!row["billing_email_verified_at"].is_null(), "{row}");
        assert!(row["pending_billing_email"].is_null(), "{row}");
        assert!(email_exists(&app, &new).await);
        assert!(!email_exists(&app, &old).await);

        // Sending the active address again withdraws a pending change.
        let typo = format!("w5-typo-{}@example.con", &t.org_id[..8]);
        for (address, pending) in [(&typo, json!(typo)), (&new, Value::Null)] {
            let (status, body) = call(
                &app,
                "PUT",
                &org_path,
                &[("Authorization", &auth)],
                Some(json!({"billing_email": address})),
            )
            .await;
            assert_eq!(status, StatusCode::OK, "{body}");
            assert_eq!(body["billing_email"], new, "{body}");
            assert_eq!(body["pending_billing_email"], pending, "{body}");
        }

        // An operator's address counts as verified at once and replaces any
        // pending one.
        let (status, body) = call(
            &app,
            "PUT",
            &org_path,
            &[("Authorization", &auth)],
            Some(json!({"billing_email": typo})),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let ops = format!("w5-ops-{}@example.com", &t.org_id[..8]);
        let (status, body) = call(
            &app,
            "PATCH",
            &format!("/api/v1/admin/organizations/{}", t.org_id),
            &[("X-Admin-Key", ADMIN_KEY)],
            Some(json!({"billing_email": ops})),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let row = org_row(&app, &auth, &t.org_id).await;
        assert_eq!(row["billing_email"], ops, "{row}");
        assert!(!row["billing_email_verified_at"].is_null(), "{row}");
        assert!(row["pending_billing_email"].is_null(), "{row}");
        assert!(email_exists(&app, &ops).await);

        // Without a verified address there is nothing to keep: the change
        // replaces it directly, unverified, as before.
        sqlx::query("UPDATE organizations SET billing_email_verified_at = NULL WHERE id = $1")
            .bind(&t.org_id)
            .execute(db.pool())
            .await
            .unwrap();
        let (status, body) = call(
            &app,
            "PUT",
            &org_path,
            &[("Authorization", &auth)],
            Some(json!({"billing_email": new})),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["billing_email"], new, "{body}");
        assert!(body["billing_email_verified_at"].is_null(), "{body}");
        assert!(body["pending_billing_email"].is_null(), "{body}");
    }

    async fn register_worker(
        app: &Router,
        auth: &str,
        worker_id: &str,
        queue: &str,
    ) -> (StatusCode, Value) {
        call(
            app,
            "POST",
            "/api/v1/workers/register",
            &[("Authorization", auth)],
            Some(json!({"worker_id": worker_id, "queue_name": queue, "hostname": "w5-host"})),
        )
        .await
    }

    /// (organization_id, queue_name) of a worker row.
    async fn worker_row(pool: &sqlx::PgPool, worker_id: &str) -> (String, String) {
        sqlx::query_as("SELECT organization_id, queue_name FROM workers WHERE id = $1")
            .bind(worker_id)
            .fetch_one(pool)
            .await
            .unwrap()
    }

    // BEJOB-19: another org's worker id answered "Worker ID already in use"
    // (which only ever meant another org), and a queue-scoped key could
    // re-register a worker row of its org serving a queue outside its scope.
    #[tokio::test]
    async fn w5_keys_worker_register_conflicts_look_alike_and_respect_scope() {
        let db = TestDatabase::new().await;
        let a = seed_tenant(db.pool()).await;
        let b = seed_tenant(db.pool()).await;
        let app = app(&db);
        let worker_id = format!("w5-worker-{}", &a.org_id[..8]);

        let (status, body) = register_worker(&app, &bearer(&a.key), &worker_id, "payroll").await;
        assert_eq!(status, StatusCode::CREATED, "{body}");

        // Org B cannot take the id.
        let (status, cross_org) =
            register_worker(&app, &bearer(&b.key), &worker_id, "payroll").await;
        assert_eq!(status, StatusCode::CONFLICT, "{cross_org}");

        // A key of org A scoped to `orders.*` cannot move A's payroll worker into
        // its own queues, and gets the very same answer.
        let scoped = seed_scoped_key(db.pool(), &a.org_id, &["orders.*"]).await;
        let (status, same_org) =
            register_worker(&app, &bearer(&scoped), &worker_id, "orders.eu").await;
        assert_eq!(status, StatusCode::CONFLICT, "{same_org}");
        assert_eq!(
            cross_org, same_org,
            "the two conflicts must be indistinguishable"
        );
        assert_eq!(
            worker_row(db.pool(), &worker_id).await,
            (a.org_id.clone(), "payroll".to_string())
        );

        // Within its scope the scoped key still re-registers (upserts) its worker.
        let own = format!("w5-own-{}", &a.org_id[..8]);
        for queue in ["orders.eu", "orders.eu", "orders.us"] {
            let (status, body) = register_worker(&app, &bearer(&scoped), &own, queue).await;
            assert_eq!(status, StatusCode::CREATED, "{queue}: {body}");
        }
        assert_eq!(worker_row(db.pool(), &own).await.1, "orders.us");
        // And an unrestricted key may re-register any worker of its org.
        let (status, body) = register_worker(&app, &bearer(&a.key), &worker_id, "emails").await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        assert_eq!(worker_row(db.pool(), &worker_id).await.1, "emails");
    }
}

// ---- wave 5 / integration ----
// Wave 5 integration: queue-prefix API keys on every path (KEYS-1/2), gRPC
// Register parity (KEYS-3), the pending-billing-email login hint (KEYS-4), the
// lease length gRPC claims record, the shared dependency-cancel reasons and the
// webhook delivery metrics.
mod w5_integration {
    use super::*;

    use std::collections::HashSet;
    use std::time::Duration;

    use futures::StreamExt;
    use prost::Message;
    use tonic::{Code, Request as GrpcRequest, Streaming};

    use spooled_backend::config::{QueueSettings, WorkerSettings};
    use spooled_backend::grpc::proto::process_request::Request as Op;
    use spooled_backend::grpc::proto::process_response::Response as Reply;
    use spooled_backend::grpc::proto::queue_service_server::QueueService;
    use spooled_backend::grpc::proto::worker_service_server::WorkerService;
    use spooled_backend::grpc::proto::{
        CompleteRequest, DequeueRequest, EnqueueRequest, FailRequest, GetJobRequest,
        ProcessRequest, ProcessResponse, RenewLeaseRequest, StreamJobsRequest,
    };
    use spooled_backend::grpc::services::QueueServiceImpl;
    use spooled_backend::outgoing_webhooks::service::OutgoingWebhookService;
    use spooled_backend::scheduler::Scheduler;

    use super::w5_realtime::{serve, sse_open, sse_read, ws_open, Frame};

    /// An API key of `org_id` limited to `queues`, inserted directly.
    async fn seed_key(pool: &sqlx::PgPool, org_id: &str, queues: &[&str]) -> String {
        let key = format!("sp_test_{}", uuid::Uuid::new_v4().simple());
        sqlx::query(
            "INSERT INTO api_keys (id, organization_id, key_hash, key_prefix, lookup_hash, name, queues, is_active, created_at) \
             VALUES ($1, $2, $3, $4, $5, 'w5 integration', $6, TRUE, NOW())",
        )
        .bind(uuid::Uuid::new_v4().to_string())
        .bind(org_id)
        .bind(bcrypt::hash(&key, 4).expect("hash key"))
        .bind(key.chars().take(8).collect::<String>())
        .bind(spooled_backend::models::api_key_lookup_hash(&key))
        .bind(queues.iter().map(|q| q.to_string()).collect::<Vec<_>>())
        .execute(pool)
        .await
        .expect("seed scoped key");
        key
    }

    async fn api(
        app: &Router,
        method: &str,
        uri: &str,
        auth: &str,
        body: Option<Value>,
    ) -> (StatusCode, Value) {
        call(app, method, uri, &[("Authorization", auth)], body).await
    }

    /// Enqueue a job that dead-letters on its first failure; its id.
    async fn enqueue(app: &Router, auth: &str, queue: &str) -> String {
        let (status, body) = api(
            app,
            "POST",
            "/api/v1/jobs",
            auth,
            Some(json!({"queue_name": queue, "payload": {"q": queue}, "max_retries": 0})),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{queue}: {body}");
        body["id"].as_str().expect("job id").to_string()
    }

    /// Claim the next job of `queue` for `worker`: (job id, lease id).
    async fn claim(app: &Router, auth: &str, queue: &str, worker: &str) -> (String, String) {
        let (status, body) = api(
            app,
            "POST",
            "/api/v1/jobs/claim",
            auth,
            Some(json!({
                "queue_name": queue,
                "worker_id": worker,
                "limit": 1,
                "lease_duration_secs": 120,
            })),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{queue}: {body}");
        let job = &body["jobs"][0];
        (
            job["id"].as_str().expect("claimed job").to_string(),
            job["lease_id"].as_str().expect("lease id").to_string(),
        )
    }

    /// POST /jobs/{id}/{action} as `worker` holding `lease`.
    async fn settle(
        app: &Router,
        auth: &str,
        action: &str,
        job: &str,
        worker: &str,
        lease: &str,
    ) -> (StatusCode, Value) {
        let body = match action {
            "fail" => json!({"worker_id": worker, "lease_id": lease, "error": "boom"}),
            "complete" => json!({"worker_id": worker, "lease_id": lease, "result": {"ok": true}}),
            _ => json!({"worker_id": worker, "lease_id": lease}),
        };
        api(
            app,
            "POST",
            &format!("/api/v1/jobs/{job}/{action}"),
            auth,
            Some(body),
        )
        .await
    }

    async fn get_ok(app: &Router, auth: &str, uri: &str) -> Value {
        let (status, body) = api(app, "GET", uri, auth, None).await;
        assert_eq!(status, StatusCode::OK, "{uri}: {body}");
        body
    }

    /// The `field` values of a JSON array of objects.
    fn ids(body: &Value, field: &str) -> HashSet<String> {
        body.as_array()
            .unwrap_or_else(|| panic!("array: {body}"))
            .iter()
            .map(|row| row[field].as_str().expect("id field").to_string())
            .collect()
    }

    async fn job_status(pool: &sqlx::PgPool, id: &str) -> String {
        sqlx::query_scalar("SELECT status FROM jobs WHERE id = $1")
            .bind(id)
            .fetch_one(pool)
            .await
            .unwrap()
    }

    async fn create_schedule(app: &Router, auth: &str, queue: &str) -> String {
        let (status, body) = api(
            app,
            "POST",
            "/api/v1/schedules",
            auth,
            Some(json!({
                "name": format!("w5i-{queue}"),
                "cron_expression": "0 0 * * *",
                "queue_name": queue,
                "payload_template": {},
            })),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{queue}: {body}");
        body["id"].as_str().expect("schedule id").to_string()
    }

    async fn create_workflow(app: &Router, auth: &str, queue: &str) -> String {
        let (status, body) = api(
            app,
            "POST",
            "/api/v1/workflows",
            auth,
            Some(json!({
                "name": format!("w5i-{queue}"),
                "jobs": [
                    {"key": "a", "queue_name": queue, "payload": {}},
                    {"key": "b", "queue_name": queue, "payload": {}, "depends_on": ["a"]},
                ],
            })),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{queue}: {body}");
        body["workflow_id"]
            .as_str()
            .expect("workflow id")
            .to_string()
    }

    /// One REST pass for a key scoped to `scope`, working in `queue` (covered),
    /// against rows in `outside` (not covered) that the org's unrestricted key
    /// made, the first one leased to the scoped key's own worker id and the
    /// second one dead-lettered.
    async fn rest_scope_pass(db: &TestDatabase, scope: &[&str], queue: &str, outside: &[&str]) {
        let t = seed_tenant(db.pool()).await;
        let app = app(db);
        let admin = bearer(&t.key);
        let key = bearer(&seed_key(db.pool(), &t.org_id, scope).await);
        let what = format!("scope {scope:?}");
        let worker = format!("w5i-worker-{}", &t.org_id[..8]);

        let mut hidden = HashSet::new();
        let mut leased = None;
        let mut dead_outside = None;
        for (i, q) in outside.iter().enumerate() {
            let id = enqueue(&app, &admin, q).await;
            let (claimed, lease) = claim(&app, &admin, q, &worker).await;
            assert_eq!(claimed, id);
            match i {
                0 => leased = Some((id.clone(), lease)),
                1 => {
                    let (status, body) = settle(&app, &admin, "fail", &id, &worker, &lease).await;
                    assert_eq!(status, StatusCode::OK, "{body}");
                    dead_outside = Some(id.clone());
                }
                _ => {}
            }
            hidden.insert(id);
        }
        let (leased, leased_lease) = leased.expect("a leased outside job");
        let dead_outside = dead_outside.expect("a dead-lettered outside job");
        let (status, body) = api(
            &app,
            "POST",
            "/api/v1/workers/register",
            &admin,
            Some(json!({"worker_id": format!("{worker}-out"), "queue_name": outside[0], "hostname": "w5i"})),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        let outside_schedule = create_schedule(&app, &admin, outside[0]).await;
        let outside_workflow = create_workflow(&app, &admin, outside[0]).await;

        // The whole worker lifecycle inside the scope.
        let (status, body) = api(
            &app,
            "POST",
            "/api/v1/workers/register",
            &key,
            Some(json!({"worker_id": worker, "queue_name": queue, "hostname": "w5i"})),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{what}: {body}");
        let done = enqueue(&app, &key, queue).await;
        let (claimed, lease) = claim(&app, &key, queue, &worker).await;
        assert_eq!(claimed, done, "{what}");
        for action in ["heartbeat", "complete"] {
            let (status, body) = settle(&app, &key, action, &done, &worker, &lease).await;
            assert_eq!(status, StatusCode::OK, "{what} {action}: {body}");
        }
        let dead = enqueue(&app, &key, queue).await;
        let (claimed, lease) = claim(&app, &key, queue, &worker).await;
        assert_eq!(claimed, dead, "{what}");
        let (status, body) = settle(&app, &key, "fail", &dead, &worker, &lease).await;
        assert_eq!(status, StatusCode::OK, "{what} fail: {body}");
        assert_eq!(job_status(db.pool(), &done).await, "completed");
        assert_eq!(job_status(db.pool(), &dead).await, "deadletter");

        let body = get_ok(&app, &key, &format!("/api/v1/jobs/{done}")).await;
        assert_eq!(body["status"], "completed", "{what}: {body}");
        let listed = ids(&get_ok(&app, &key, "/api/v1/jobs?limit=100").await, "id");
        assert_eq!(
            listed,
            HashSet::from([done.clone(), dead.clone()]),
            "{what}: job list"
        );
        let dlq = ids(
            &get_ok(&app, &key, "/api/v1/jobs/dlq?limit=100").await,
            "id",
        );
        assert_eq!(dlq, HashSet::from([dead.clone()]), "{what}: DLQ list");
        let stats = get_ok(&app, &key, "/api/v1/jobs/stats").await;
        assert_eq!(
            (stats["total"].as_i64(), stats["completed"].as_i64()),
            (Some(2), Some(1)),
            "{what}: {stats}"
        );
        assert_eq!(stats["deadletter"].as_i64(), Some(1), "{what}: {stats}");
        assert_eq!(stats["processing"].as_i64(), Some(0), "{what}: {stats}");
        let dashboard = get_ok(&app, &key, "/api/v1/dashboard").await;
        assert_eq!(dashboard["jobs"]["total"], 2, "{what}: {dashboard}");
        assert_eq!(dashboard["jobs"]["deadletter"], 1, "{what}: {dashboard}");
        assert_eq!(dashboard["workers"]["total"], 1, "{what}: {dashboard}");
        assert_eq!(
            ids(&dashboard["queues"], "name"),
            HashSet::from([queue.to_string()]),
            "{what}: {dashboard}"
        );
        let workers = ids(&get_ok(&app, &key, "/api/v1/workers").await, "id");
        assert_eq!(workers, HashSet::from([worker.clone()]), "{what}: workers");

        let schedule = create_schedule(&app, &key, queue).await;
        let schedules = ids(&get_ok(&app, &key, "/api/v1/schedules").await, "id");
        assert_eq!(schedules, HashSet::from([schedule]), "{what}: schedules");
        let workflow = create_workflow(&app, &key, queue).await;
        let workflows = ids(&get_ok(&app, &key, "/api/v1/workflows").await, "id");
        assert_eq!(
            workflows,
            HashSet::from([workflow.clone()]),
            "{what}: workflows"
        );
        get_ok(&app, &key, &format!("/api/v1/workflows/{workflow}")).await;

        // Everything outside the scope answers as it always did.
        for q in outside {
            let (status, body) = api(
                &app,
                "POST",
                "/api/v1/jobs",
                &key,
                Some(json!({"queue_name": q, "payload": {}})),
            )
            .await;
            assert_eq!(status, StatusCode::FORBIDDEN, "{what} enqueue {q}: {body}");
            let (status, body) = api(
                &app,
                "POST",
                "/api/v1/jobs/claim",
                &key,
                Some(json!({"queue_name": q, "worker_id": worker, "limit": 1})),
            )
            .await;
            assert_eq!(status, StatusCode::FORBIDDEN, "{what} claim {q}: {body}");
        }
        for id in &hidden {
            let (status, body) = api(&app, "GET", &format!("/api/v1/jobs/{id}"), &key, None).await;
            assert_eq!(status, StatusCode::NOT_FOUND, "{what} get: {body}");
        }
        for action in ["heartbeat", "complete", "fail"] {
            let (status, body) = settle(&app, &key, action, &leased, &worker, &leased_lease).await;
            assert_eq!(status, StatusCode::NOT_FOUND, "{what} {action}: {body}");
        }
        assert_eq!(job_status(db.pool(), &leased).await, "processing");
        assert!(!dlq.contains(&dead_outside));
        for uri in [
            format!("/api/v1/workers/{worker}-out"),
            format!("/api/v1/workflows/{outside_workflow}"),
            format!("/api/v1/schedules/{outside_schedule}"),
        ] {
            let (status, body) = api(&app, "GET", &uri, &key, None).await;
            assert_eq!(status, StatusCode::NOT_FOUND, "{what} {uri}: {body}");
        }
    }

    // KEYS-1: a `prefix.*` key could enqueue and claim in its queues, then got
    // 404 on heartbeat/complete/fail and saw none of its jobs, DLQ rows,
    // workers, schedules, workflows or dashboard counts, because those queries
    // compared the queue to the key's entries by exact name. Exact-name keys
    // keep their old answers, and none of them reaches a covered child queue.
    #[tokio::test]
    async fn w5_int_rest_paths_follow_prefix_and_exact_key_scopes() {
        let db = TestDatabase::new().await;
        rest_scope_pass(
            &db,
            &["orders.*"],
            "orders.eu",
            &["ordersx", "payments", "orders"],
        )
        .await;
        rest_scope_pass(
            &db,
            &["orders.eu"],
            "orders.eu",
            &["orders.us", "payments", "orders"],
        )
        .await;
        rest_scope_pass(&db, &["orders"], "orders", &["orders.eu", "ordersx"]).await;
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

    fn grpc<T>(key: &str, body: T) -> GrpcRequest<T> {
        let mut request = GrpcRequest::new(body);
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

    fn dequeue_req(queue: &str, worker: &str, lease: i32) -> DequeueRequest {
        DequeueRequest {
            queue_name: queue.to_string(),
            worker_id: worker.to_string(),
            lease_duration_secs: lease,
            batch_size: 1,
        }
    }

    fn complete_req(job: &str, worker: &str, lease: &str) -> CompleteRequest {
        CompleteRequest {
            job_id: job.to_string(),
            worker_id: worker.to_string(),
            result: None,
            lease_id: lease.to_string(),
        }
    }

    fn fail_req(job: &str, worker: &str, lease: &str) -> FailRequest {
        FailRequest {
            job_id: job.to_string(),
            worker_id: worker.to_string(),
            error: "boom".to_string(),
            retry: Some(false),
            lease_id: lease.to_string(),
        }
    }

    fn renew_req(job: &str, worker: &str, lease: &str, secs: i32) -> RenewLeaseRequest {
        RenewLeaseRequest {
            job_id: job.to_string(),
            worker_id: worker.to_string(),
            extension_secs: secs,
            lease_id: lease.to_string(),
        }
    }

    /// Enqueue in `queue` and lease the job to `worker` over gRPC: (id, lease).
    async fn grpc_leased(
        service: &QueueServiceImpl,
        key: &str,
        queue: &str,
        worker: &str,
        lease: i32,
    ) -> (String, String) {
        let id = service
            .enqueue(grpc(key, enqueue_req(queue)))
            .await
            .unwrap_or_else(|e| panic!("enqueue {queue}: {e}"))
            .into_inner()
            .job_id;
        let jobs = service
            .dequeue(grpc(key, dequeue_req(queue, worker, lease)))
            .await
            .unwrap_or_else(|e| panic!("dequeue {queue}: {e}"))
            .into_inner()
            .jobs;
        assert_eq!(jobs.len(), 1, "{queue}");
        assert_eq!(jobs[0].id, id, "{queue}");
        (id, jobs[0].lease_id.clone())
    }

    /// A ProcessJobs request stream carrying `ops`.
    fn process_stream(key: &str, ops: Vec<Op>) -> GrpcRequest<Streaming<ProcessRequest>> {
        let mut bytes = Vec::new();
        for op in ops {
            let body = ProcessRequest { request: Some(op) }.encode_to_vec();
            bytes.push(0u8);
            bytes.extend_from_slice(&(body.len() as u32).to_be_bytes());
            bytes.extend_from_slice(&body);
        }
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

    async fn process(service: &QueueServiceImpl, key: &str, ops: Vec<Op>) -> Vec<Reply> {
        let stream = service
            .process_jobs(process_stream(key, ops))
            .await
            .expect("ProcessJobs")
            .into_inner();
        tokio::time::timeout(Duration::from_secs(20), stream.collect::<Vec<_>>())
            .await
            .expect("ProcessJobs ends")
            .into_iter()
            .map(|reply| reply.expect("reply").response.expect("response"))
            .collect()
    }

    fn error_code(reply: &Reply) -> Option<&str> {
        match reply {
            Reply::Error(e) => Some(e.code.as_str()),
            _ => None,
        }
    }

    /// The gRPC counterpart of [`rest_scope_pass`]: unary calls, StreamJobs and
    /// ProcessJobs inside the scope, and the same denials outside it.
    async fn grpc_scope_pass(db: &TestDatabase, scope: &[&str], queue: &str, outside: &[&str]) {
        let t = seed_tenant(db.pool()).await;
        let key = seed_key(db.pool(), &t.org_id, scope).await;
        let service = queue_service(db);
        let what = format!("scope {scope:?}");
        let worker = format!("w5i-grpc-{}", &t.org_id[..8]);
        let (leased, leased_lease) = grpc_leased(&service, &t.key, outside[0], &worker, 300).await;

        // Unary lifecycle.
        let (done, lease) = grpc_leased(&service, &key, queue, &worker, 300).await;
        service
            .renew_lease(grpc(&key, renew_req(&done, &worker, &lease, 60)))
            .await
            .unwrap_or_else(|e| panic!("{what} renew: {e}"));
        let got = service
            .get_job(grpc(
                &key,
                GetJobRequest {
                    job_id: done.clone(),
                },
            ))
            .await
            .unwrap_or_else(|e| panic!("{what} get: {e}"))
            .into_inner();
        assert_eq!(got.job.expect("job").id, done);
        service
            .complete(grpc(&key, complete_req(&done, &worker, &lease)))
            .await
            .unwrap_or_else(|e| panic!("{what} complete: {e}"));
        let (dead, lease) = grpc_leased(&service, &key, queue, &worker, 300).await;
        service
            .fail(grpc(&key, fail_req(&dead, &worker, &lease)))
            .await
            .unwrap_or_else(|e| panic!("{what} fail: {e}"));
        assert_eq!(job_status(db.pool(), &done).await, "completed", "{what}");
        assert_eq!(job_status(db.pool(), &dead).await, "deadletter", "{what}");

        // ProcessJobs.
        let (p_done, p_done_lease) = grpc_leased(&service, &key, queue, &worker, 300).await;
        let (p_dead, p_dead_lease) = grpc_leased(&service, &key, queue, &worker, 300).await;
        let p_next = service
            .enqueue(grpc(&key, enqueue_req(queue)))
            .await
            .expect("enqueue")
            .into_inner()
            .job_id;
        let replies = process(
            &service,
            &key,
            vec![
                Op::Dequeue(dequeue_req(queue, &worker, 0)),
                Op::RenewLease(renew_req(&p_dead, &worker, &p_dead_lease, 60)),
                Op::Complete(complete_req(&p_done, &worker, &p_done_lease)),
                Op::Fail(fail_req(&p_dead, &worker, &p_dead_lease)),
                Op::Complete(complete_req(&leased, &worker, &leased_lease)),
                Op::Dequeue(dequeue_req(outside[0], &worker, 0)),
            ],
        )
        .await;
        assert_eq!(replies.len(), 6, "{what}: {replies:?}");
        assert!(
            matches!(&replies[0], Reply::Job(job) if job.id == p_next),
            "{what}: {:?}",
            replies[0]
        );
        assert!(
            matches!(&replies[1], Reply::RenewLease(r) if r.success),
            "{what}: {:?}",
            replies[1]
        );
        assert!(
            matches!(&replies[2], Reply::Complete(r) if r.success),
            "{what}: {:?}",
            replies[2]
        );
        assert!(
            matches!(&replies[3], Reply::Fail(r) if r.success),
            "{what}: {:?}",
            replies[3]
        );
        assert_eq!(error_code(&replies[4]), Some("NOT_FOUND"), "{what}");
        assert_eq!(error_code(&replies[5]), Some("PERMISSION_DENIED"), "{what}");
        assert_eq!(job_status(db.pool(), &p_done).await, "completed", "{what}");
        assert_eq!(job_status(db.pool(), &p_dead).await, "deadletter", "{what}");

        // StreamJobs.
        let streamed = service
            .enqueue(grpc(&key, enqueue_req(queue)))
            .await
            .expect("enqueue")
            .into_inner()
            .job_id;
        let mut stream = service
            .stream_jobs(grpc(
                &key,
                StreamJobsRequest {
                    queue_name: queue.to_string(),
                    worker_id: format!("{worker}-s"),
                    lease_duration_secs: 60,
                },
            ))
            .await
            .unwrap_or_else(|e| panic!("{what} StreamJobs: {e}"))
            .into_inner();
        let job = tokio::time::timeout(Duration::from_secs(10), stream.next())
            .await
            .expect("a streamed job within 10s")
            .expect("stream open")
            .unwrap_or_else(|e| panic!("{what} StreamJobs: {e}"));
        assert_eq!(job.id, streamed, "{what}");
        service
            .complete(grpc(
                &key,
                complete_req(&streamed, &format!("{worker}-s"), &job.lease_id),
            ))
            .await
            .unwrap_or_else(|e| panic!("{what} complete streamed: {e}"));
        drop(stream);

        // Outside the scope: the answers from before.
        let err = service
            .get_job(grpc(
                &key,
                GetJobRequest {
                    job_id: leased.clone(),
                },
            ))
            .await
            .expect_err("get outside");
        assert_eq!(err.code(), Code::NotFound, "{what}");
        let err = service
            .renew_lease(grpc(&key, renew_req(&leased, &worker, &leased_lease, 60)))
            .await
            .expect_err("renew outside");
        assert_eq!(err.code(), Code::NotFound, "{what}");
        let err = service
            .complete(grpc(&key, complete_req(&leased, &worker, &leased_lease)))
            .await
            .expect_err("complete outside");
        assert_eq!(err.code(), Code::NotFound, "{what}");
        let err = service
            .fail(grpc(&key, fail_req(&leased, &worker, &leased_lease)))
            .await
            .expect_err("fail outside");
        assert_eq!(err.code(), Code::NotFound, "{what}");
        assert_eq!(job_status(db.pool(), &leased).await, "processing", "{what}");
        for q in outside {
            let err = service
                .dequeue(grpc(&key, dequeue_req(q, &worker, 0)))
                .await
                .expect_err("dequeue outside");
            assert_eq!(err.code(), Code::PermissionDenied, "{what} {q}");
            let err = service
                .stream_jobs(grpc(
                    &key,
                    StreamJobsRequest {
                        queue_name: q.to_string(),
                        worker_id: worker.clone(),
                        lease_duration_secs: 60,
                    },
                ))
                .await
                .err()
                .unwrap_or_else(|| panic!("{what}: StreamJobs on {q} opened"));
            assert_eq!(err.code(), Code::PermissionDenied, "{what} {q}");
        }
    }

    // KEYS-1 / KEYS-2, gRPC: a prefix key could Enqueue and Dequeue, then got
    // NOT_FOUND on RenewLease/Complete/Fail/GetJob, PERMISSION_DENIED from every
    // StreamJobs claim and ProcessJobs dequeue (the inline exact rule) and
    // NOT_FOUND from ProcessJobs settles (the SQL exact rule).
    #[tokio::test]
    async fn w5_int_grpc_paths_follow_prefix_and_exact_key_scopes() {
        let db = TestDatabase::new().await;
        grpc_scope_pass(
            &db,
            &["orders.*"],
            "orders.eu",
            &["ordersx", "payments", "orders"],
        )
        .await;
        grpc_scope_pass(&db, &["orders.eu"], "orders.eu", &["orders.us", "payments"]).await;
        grpc_scope_pass(&db, &["orders"], "orders", &["orders.eu", "ordersx"]).await;
    }

    /// The plan of a scoped count with `scope` bound, as text.
    async fn scoped_count_plan(conn: &mut sqlx::PgConnection, scope: &[&str]) -> String {
        let lines: Vec<String> = sqlx::query_scalar(
            "EXPLAIN SELECT COUNT(*) FROM jobs \
             WHERE organization_id = $1 AND queue_in_key_scope(queue_name, $2::TEXT[])",
        )
        .bind("w5i-org")
        .bind(scope.iter().map(|q| q.to_string()).collect::<Vec<_>>())
        .fetch_all(conn)
        .await
        .expect("explain");
        lines.join("\n")
    }

    // KEYS-1, plans: with a bound scope an exact-name scope must still plan as
    // `queue_name = ANY(...)` (an index condition on idx_jobs_org_queue_status),
    // and a prefix scope as a plain LIKE. The first queue_in_key_scope() kept a
    // STABLE pattern rewrite in every plan, so a scoped key's counts scanned the
    // org's whole job range and re-ran a regexp on each row.
    #[tokio::test]
    async fn w5_int_scoped_counts_keep_index_friendly_plans() {
        let db = TestDatabase::new().await;
        let mut conn = db.pool().acquire().await.unwrap();

        let exact = scoped_count_plan(&mut conn, &["orders.eu", "billing"]).await;
        assert!(
            exact.contains("queue_name = ANY ('{orders.eu,billing}'::text[])"),
            "{exact}"
        );
        for leftover in ["array_to_string", "queue_scope_like_patterns", "~~"] {
            assert!(!exact.contains(leftover), "{leftover} left in: {exact}");
        }

        let prefix = scoped_count_plan(&mut conn, &["orders.*", "billing"]).await;
        assert!(
            prefix.contains("~~ ANY ('{orders.%,billing}'::text[])"),
            "{prefix}"
        );
        for leftover in [
            "array_to_string",
            "regexp_replace",
            "queue_scope_like_patterns",
        ] {
            assert!(!prefix.contains(leftover), "{leftover} left in: {prefix}");
        }
    }

    async fn publish(cache: &spooled_backend::cache::RedisCache, org_id: &str, queue: &str) {
        let event = json!({
            "type": "JobCreated",
            "data": {
                "job_id": format!("w5i-rt-{queue}"),
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

    // KEYS-2, realtime: the WebSocket and SSE fan-out, the job stream and the
    // queue stream compared a prefix key's entries by exact name, so its
    // streams stayed silent or closed at the first re-check.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn w5_int_prefix_key_realtime_streams_follow_the_scope() {
        let db = TestDatabase::new().await;
        let t = seed_tenant(db.pool()).await;
        let (app, cache, _redis) = app_with_redis(&db).await;
        let addr = serve(app.clone()).await;
        let key = bearer(&seed_key(db.pool(), &t.org_id, &["orders.*"]).await);
        let headers = [("Authorization", key.as_str())];

        // Streams filtered on an uncovered queue are refused, as before.
        let (status, _) = sse_open(&app, "/api/v1/events?queue=ordersx", &headers).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(
            ws_open(addr, "/api/v1/ws?queue=payments", &headers)
                .await
                .err(),
            Some(403)
        );

        let mut ws = ws_open(addr, "/api/v1/ws", &headers)
            .await
            .expect("upgrade");
        assert!(
            matches!(ws.frame().await, Some(Frame::Text(_))),
            "initial ping"
        );
        let (status, mut events) = sse_open(&app, "/api/v1/events", &headers).await;
        assert_eq!(status, StatusCode::OK);
        let (first, _) = sse_read(&mut events, Duration::from_millis(500)).await;
        assert!(first.contains("connected"), "{first}");
        tokio::time::sleep(Duration::from_millis(500)).await;

        for queue in ["ordersx", "payments", "orders", "orders.eu"] {
            publish(&cache, &t.org_id, queue).await;
        }
        let mut seen = Vec::new();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(6);
        while let Ok(Some(frame)) = tokio::time::timeout_at(deadline, ws.frame()).await {
            match frame {
                Frame::Text(text) => {
                    let last = text.contains("w5i-rt-orders.eu");
                    seen.push(text);
                    if last {
                        break;
                    }
                }
                Frame::Close(code) => panic!("WebSocket closed ({code}): {seen:?}"),
                Frame::Other => {}
            }
        }
        assert!(
            seen.iter().any(|text| text.contains("w5i-rt-orders.eu")),
            "the covered queue's event reaches the WebSocket: {seen:?}"
        );
        for queue in ["ordersx", "payments", "orders"] {
            let needle = format!("\"w5i-rt-{queue}\"");
            assert!(
                !seen.iter().any(|text| text.contains(&needle)),
                "{queue} leaked to the WebSocket: {seen:?}"
            );
        }
        let mut sse = String::new();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(6);
        while !sse.contains("w5i-rt-orders.eu") && tokio::time::Instant::now() < deadline {
            let (more, ended) = sse_read(&mut events, Duration::from_millis(500)).await;
            sse.push_str(&more);
            if ended {
                break;
            }
        }
        assert!(sse.contains("w5i-rt-orders.eu"), "SSE /events: {sse}");
        for queue in ["ordersx", "payments", "orders"] {
            assert!(
                !sse.contains(&format!("\"w5i-rt-{queue}\"")),
                "{queue} leaked to SSE: {sse}"
            );
        }

        // The job and queue streams.
        let job = enqueue(&app, &key, "orders.eu").await;
        let hidden_job = enqueue(&app, &bearer(&t.key), "payments").await;
        let (s1, mut job_stream) =
            sse_open(&app, &format!("/api/v1/events/jobs/{job}"), &headers).await;
        let (s2, mut queue_stream) =
            sse_open(&app, "/api/v1/events/queues/orders.eu", &headers).await;
        let (s3, mut hidden_job_stream) =
            sse_open(&app, &format!("/api/v1/events/jobs/{hidden_job}"), &headers).await;
        let (s4, mut hidden_queue_stream) =
            sse_open(&app, "/api/v1/events/queues/ordersx", &headers).await;
        assert_eq!(
            (s1, s2, s3, s4),
            (
                StatusCode::OK,
                StatusCode::OK,
                StatusCode::OK,
                StatusCode::OK
            )
        );
        let limit = Duration::from_secs(8);
        let ((job_seen, _), (queue_seen, _), (hidden_seen, hidden_ended), (hq_seen, hq_ended)) = tokio::join!(
            sse_read(&mut job_stream, limit),
            sse_read(&mut queue_stream, limit),
            sse_read(&mut hidden_job_stream, limit),
            sse_read(&mut hidden_queue_stream, limit)
        );
        assert!(job_seen.contains("job.status"), "job stream: {job_seen}");
        assert!(
            queue_seen.contains("queue.stats"),
            "queue stream: {queue_seen}"
        );
        assert!(
            hidden_ended && !hidden_seen.contains("job.status"),
            "uncovered job stream: {hidden_seen}"
        );
        assert!(
            hq_ended && !hq_seen.contains("queue.stats"),
            "uncovered queue stream: {hq_seen}"
        );
    }

    // KEYS-2: the admin key route stored any `queues` it was given, so
    // `orders*` or `*.eu` made a key that could reach no queue.
    #[tokio::test]
    async fn w5_int_admin_created_keys_validate_their_queues() {
        const ADMIN_KEY: &str = "w5-integration-admin-key";
        let db = TestDatabase::new().await;
        let t = seed_tenant(db.pool()).await;
        let mut settings = Settings::load_for_testing();
        settings.registration.admin_api_key = Some(ADMIN_KEY.to_string());
        let app = app_with(&db, settings);
        let uri = format!("/api/v1/admin/organizations/{}/api-keys", t.org_id);

        for queues in [
            json!(["orders*"]),
            json!(["*.eu"]),
            json!(["orders.*.eu"]),
            json!(["bad queue"]),
            json!([""]),
        ] {
            let (status, body) = call(
                &app,
                "POST",
                &uri,
                &[("X-Admin-Key", ADMIN_KEY)],
                Some(json!({"name": "w5i", "queues": queues})),
            )
            .await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{queues}: {body}");
        }
        for queues in [Some(json!(["orders.*", "billing"])), None] {
            let mut request = json!({"name": "w5i"});
            if let Some(queues) = &queues {
                request["queues"] = queues.clone();
            }
            let (status, body) = call(
                &app,
                "POST",
                &uri,
                &[("X-Admin-Key", ADMIN_KEY)],
                Some(request),
            )
            .await;
            assert_eq!(status, StatusCode::CREATED, "{queues:?}: {body}");
            let key = bearer(body["key"].as_str().expect("key"));
            enqueue(&app, &key, "orders.eu").await;
        }
    }

    /// (organization_id, queue_name) of a worker row.
    async fn worker_row(pool: &sqlx::PgPool, id: &str) -> (String, String) {
        sqlx::query_as("SELECT organization_id, queue_name FROM workers WHERE id = $1")
            .bind(id)
            .fetch_one(pool)
            .await
            .unwrap()
    }

    // KEYS-3: gRPC Register said "Worker ID already in use by another
    // organization" (REST answers a neutral 409 since BEJOB-19), and a
    // queue-scoped key could move a worker row of its org that serves a queue
    // outside its scope.
    #[tokio::test]
    async fn w5_int_grpc_register_conflicts_match_rest() {
        let db = TestDatabase::new().await;
        let a = seed_tenant(db.pool()).await;
        let b = seed_tenant(db.pool()).await;
        let app = app(&db);
        let (service, _) = grpcw_worker_service(&db);
        let worker_id = format!("w5i-worker-{}", &a.org_id[..8]);
        let (status, body) = api(
            &app,
            "POST",
            "/api/v1/workers/register",
            &bearer(&a.key),
            Some(json!({"worker_id": worker_id, "queue_name": "payroll", "hostname": "w5i"})),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        let (status, rest) = api(
            &app,
            "POST",
            "/api/v1/workers/register",
            &bearer(&b.key),
            Some(json!({"worker_id": worker_id, "queue_name": "payroll", "hostname": "w5i"})),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT, "{rest}");
        let rest_message = rest["message"].as_str().expect("message");

        let err = service
            .register(grpcw_request(
                &b.key,
                grpcw_register("payroll", Some(&worker_id)),
            ))
            .await
            .expect_err("another org's worker id");
        assert_eq!(err.code(), Code::AlreadyExists);
        assert_eq!(err.message(), rest_message, "same answer as REST");

        let scoped = seed_key(db.pool(), &a.org_id, &["orders.*"]).await;
        let err = service
            .register(grpcw_request(
                &scoped,
                grpcw_register("orders.eu", Some(&worker_id)),
            ))
            .await
            .expect_err("an out-of-scope worker of the same org");
        assert_eq!(err.code(), Code::AlreadyExists);
        assert_eq!(err.message(), rest_message, "indistinguishable conflicts");
        assert_eq!(
            worker_row(db.pool(), &worker_id).await,
            (a.org_id.clone(), "payroll".to_string())
        );

        // Within its scope the key still re-registers its own worker.
        let own = format!("w5i-own-{}", &a.org_id[..8]);
        for queue in ["orders.eu", "orders.eu", "orders.us"] {
            service
                .register(grpcw_request(&scoped, grpcw_register(queue, Some(&own))))
                .await
                .unwrap_or_else(|e| panic!("{queue}: {e}"));
        }
        assert_eq!(worker_row(db.pool(), &own).await.1, "orders.us");
        // And an unrestricted key re-registers any worker of its org.
        service
            .register(grpcw_request(
                &a.key,
                grpcw_register("emails", Some(&worker_id)),
            ))
            .await
            .expect("unrestricted re-register");
        assert_eq!(worker_row(db.pool(), &worker_id).await.1, "emails");
    }

    // KEYS-4: the signup hint `unverified_org_exists` ignored a pending billing
    // email (W1-14), the other unconfirmed address an org can hold.
    #[tokio::test]
    async fn w5_int_login_hint_counts_a_pending_billing_email() {
        let db = TestDatabase::new().await;
        let t = seed_tenant(db.pool()).await;
        let app = app(&db);
        let pending = format!("w5i-pending-{}@example.com", &t.org_id[..8]);
        let (status, body) = api(
            &app,
            "PUT",
            &format!("/api/v1/organizations/{}", t.org_id),
            &bearer(&t.key),
            Some(json!({"billing_email": pending})),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["pending_billing_email"], pending, "{body}");

        let (status, body) = call(
            &app,
            "POST",
            "/api/v1/auth/email/start",
            &[],
            Some(json!({"email": pending})),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let (code, bound) = latest_code(db.pool(), &pending).await;
        assert_eq!(bound, None, "a pending address is no login identity");
        let (status, body) = call(
            &app,
            "POST",
            "/api/v1/auth/email/verify",
            &[],
            Some(json!({"email": pending, "code": code})),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["type"], "signup", "{body}");
        assert_eq!(body["unverified_org_exists"], true, "{body}");
    }

    async fn lease_column(pool: &sqlx::PgPool, id: &str) -> Option<i32> {
        sqlx::query_scalar("SELECT lease_duration_secs FROM jobs WHERE id = $1")
            .bind(id)
            .fetch_one(pool)
            .await
            .unwrap()
    }

    async fn lease_left(pool: &sqlx::PgPool, id: &str) -> f64 {
        sqlx::query_scalar(
            "SELECT EXTRACT(EPOCH FROM (lease_expires_at - NOW()))::FLOAT8 FROM jobs WHERE id = $1",
        )
        .bind(id)
        .fetch_one(pool)
        .await
        .unwrap()
    }

    /// A REST heartbeat that names no length, then the seconds left on the lease.
    async fn rest_heartbeat(
        app: &Router,
        db: &TestDatabase,
        auth: &str,
        job: &str,
        worker: &str,
        lease: &str,
    ) -> f64 {
        let (status, body) = settle(app, auth, "heartbeat", job, worker, lease).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        lease_left(db.pool(), job).await
    }

    fn assert_about(left: f64, secs: f64, what: &str) {
        assert!(
            left > secs - 10.0 && left <= secs + 1.0,
            "{what}: {left}s left, expected {secs}s"
        );
    }

    // jobsapi cross-request: gRPC claims did not record the granted lease in
    // jobs.lease_duration_secs, so a REST heartbeat without a length cut a
    // 600 s gRPC lease to the 30 s default.
    #[tokio::test]
    async fn w5_int_grpc_claims_record_the_lease_rest_heartbeats_keep() {
        let db = TestDatabase::new().await;
        let t = seed_tenant(db.pool()).await;
        let app = app(&db);
        let auth = bearer(&t.key);
        let service = queue_service(&db);

        let (unary, lease) = grpc_leased(&service, &t.key, "w5i-lease", "w5i-lw", 600).await;
        assert_eq!(lease_column(db.pool(), &unary).await, Some(600));
        let left = rest_heartbeat(&app, &db, &auth, &unary, "w5i-lw", &lease).await;
        assert_about(left, 600.0, "Dequeue then REST heartbeat");
        service
            .renew_lease(grpc(&t.key, renew_req(&unary, "w5i-lw", &lease, 90)))
            .await
            .expect("renew");
        assert_eq!(lease_column(db.pool(), &unary).await, Some(90));
        let left = rest_heartbeat(&app, &db, &auth, &unary, "w5i-lw", &lease).await;
        assert_about(left, 90.0, "RenewLease then REST heartbeat");

        let streamed = enqueue(&app, &auth, "w5i-lease-s").await;
        let mut stream = service
            .stream_jobs(grpc(
                &t.key,
                StreamJobsRequest {
                    queue_name: "w5i-lease-s".to_string(),
                    worker_id: "w5i-ls".to_string(),
                    lease_duration_secs: 450,
                },
            ))
            .await
            .expect("StreamJobs")
            .into_inner();
        let job = tokio::time::timeout(Duration::from_secs(10), stream.next())
            .await
            .expect("a streamed job within 10s")
            .expect("stream open")
            .expect("a job");
        assert_eq!(job.id, streamed);
        assert_eq!(lease_column(db.pool(), &streamed).await, Some(450));
        let left = rest_heartbeat(&app, &db, &auth, &streamed, "w5i-ls", &job.lease_id).await;
        assert_about(left, 450.0, "StreamJobs then REST heartbeat");

        let processed = enqueue(&app, &auth, "w5i-lease-p").await;
        let replies = process(
            &service,
            &t.key,
            vec![Op::Dequeue(dequeue_req("w5i-lease-p", "w5i-lp", 500))],
        )
        .await;
        let lease = match &replies[..] {
            [Reply::Job(job)] if job.id == processed => job.lease_id.clone(),
            other => panic!("ProcessJobs dequeue: {other:?}"),
        };
        assert_eq!(lease_column(db.pool(), &processed).await, Some(500));
        let left = rest_heartbeat(&app, &db, &auth, &processed, "w5i-lp", &lease).await;
        assert_about(left, 500.0, "ProcessJobs then REST heartbeat");
        drop(stream);
    }

    // jobsapi/flow cross-request: the scheduler's dependency sweep and the
    // retry re-open matched the cancel reasons through two copies of the same
    // strings; both now use the shared consts.
    #[tokio::test]
    async fn w5_int_sweep_cancel_reasons_are_the_ones_retry_reopens() {
        use spooled_backend::models::{
            DEPENDENCY_CANCEL_REASONS, DEPENDENCY_FAILED_CANCEL_REASON, PARENT_FAILED_CANCEL_REASON,
        };

        let db = TestDatabase::new().await;
        let t = seed_tenant(db.pool()).await;
        let app = app(&db);
        let auth = bearer(&t.key);
        let parent = enqueue(&app, &auth, "w5i-dep").await;
        let child = uuid::Uuid::new_v4().to_string();
        let dependent = uuid::Uuid::new_v4().to_string();
        for (id, parent_job_id) in [(&child, Some(&parent)), (&dependent, None)] {
            sqlx::query(
                "INSERT INTO jobs (id, organization_id, queue_name, payload, status, max_retries, \
                 parent_job_id, dependencies_met, created_at, updated_at) \
                 VALUES ($1, $2, 'w5i-dep-kids', '{}'::JSONB, 'pending', 3, $3, FALSE, NOW(), NOW())",
            )
            .bind(id)
            .bind(&t.org_id)
            .bind(parent_job_id)
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
        let (claimed, lease) = claim(&app, &auth, "w5i-dep", "w5i-dw").await;
        assert_eq!(claimed, parent);
        let (status, body) = settle(&app, &auth, "fail", &parent, "w5i-dw", &lease).await;
        assert_eq!(status, StatusCode::OK, "{body}");

        Scheduler::new(
            Arc::clone(&db.pool),
            None,
            Arc::new(Metrics::new()),
            Arc::new(OutgoingWebhookService::new((*db.pool).clone(), 3, 4)),
        )
        .update_job_dependencies()
        .await
        .expect("dependency sweep");
        for (id, reason) in [
            (&child, PARENT_FAILED_CANCEL_REASON),
            (&dependent, DEPENDENCY_FAILED_CANCEL_REASON),
        ] {
            let (status, last_error): (String, Option<String>) =
                sqlx::query_as("SELECT status, last_error FROM jobs WHERE id = $1")
                    .bind(id)
                    .fetch_one(db.pool())
                    .await
                    .unwrap();
            assert_eq!(status, "cancelled");
            assert_eq!(last_error.as_deref(), Some(reason));
            assert!(DEPENDENCY_CANCEL_REASONS.contains(&reason));
        }

        let (status, body) = api(
            &app,
            "POST",
            &format!("/api/v1/jobs/{parent}/retry"),
            &auth,
            None,
        )
        .await;
        assert!(status.is_success(), "{status}: {body}");
        assert_eq!(job_status(db.pool(), &child).await, "pending");
        assert_eq!(job_status(db.pool(), &dependent).await, "pending");
    }

    /// The value of the sample line `name` in a Prometheus text exposition.
    fn sample(text: &str, name: &str) -> Option<f64> {
        text.lines()
            .find_map(|line| line.strip_prefix(name)?.strip_prefix(' ')?.parse().ok())
    }

    // hooks cross-request: delivery drops were only visible in logs. The
    // outgoing-webhook counters are now sampled on every scrape, like the pool
    // gauges.
    #[tokio::test]
    async fn w5_int_webhook_delivery_counters_reach_prometheus() {
        let db = TestDatabase::new().await;
        let mut settings = Settings::load_for_testing();
        // One delivery slot: a queue of 16.
        settings.outgoing_webhooks.max_concurrent_deliveries = 1;
        let state = AppState::new(
            Database::from_pool(Arc::clone(&db.pool)),
            None,
            Metrics::new(),
            settings,
        );

        // Nothing yields here, so no consumer takes an event off the queue.
        for _ in 0..20 {
            state.outgoing_webhooks.spawn_dispatch(
                "w5i-org".to_string(),
                "job.completed".to_string(),
                "w5i-job".to_string(),
                json!({"job_id": "w5i-job"}),
            );
        }
        for _ in 0..2 {
            let text = state.metrics.encode();
            assert_eq!(
                sample(&text, "spooled_outgoing_webhook_deliveries_dropped_total"),
                Some(4.0),
                "{text}"
            );
            assert_eq!(
                sample(&text, "spooled_outgoing_webhook_queue_depth"),
                Some(16.0),
                "{text}"
            );
            assert_eq!(
                sample(&text, "spooled_outgoing_webhook_deliveries_in_flight"),
                Some(0.0),
                "{text}"
            );
            assert_eq!(
                sample(&text, "spooled_completion_webhooks_dropped_total"),
                Some(0.0),
                "{text}"
            );
        }
    }
}
// ---- wave 5 / review fixes ----
// W5-R2: completion webhooks get their own request slots and per-org bounds.
// W5-R3: a DLQ retry skips the steps of failed workflows the max_workflows cap
// cannot revive, instead of failing whole.
mod wave5_review {
    use super::*;

    use std::time::Duration;

    use spooled_backend::outgoing_webhooks::service::{
        completion_webhooks_dropped, spawn_completion_webhook, OutgoingWebhookService,
    };

    use super::wave5_hooks::{eventually, fire, insert_hook, Endpoint};

    fn complete_to(org_id: &str, url: &str, count: usize) {
        for i in 0..count {
            spawn_completion_webhook(
                org_id,
                url.to_string(),
                "job.completed".to_string(),
                json!({ "job_id": i }),
                None,
            );
        }
    }

    // W5-R2: completion webhooks waited on the org webhooks' request slots,
    // and consumers wait on those inline. One tenant's backlog of completions
    // to a URL that never answers stalled every tenant's org webhooks and
    // every other tenant's completion webhooks.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn w5_review_a_completion_backlog_does_not_stall_other_tenants_webhooks() {
        let db = TestDatabase::new().await;
        let busy = seed_tenant(db.pool()).await;
        let other = seed_tenant(db.pool()).await;
        let black_hole = Endpoint::start(200, Duration::from_secs(3), "ok".into()).await;
        let org_hook = Endpoint::start(200, Duration::ZERO, "ok".into()).await;
        let completion = Endpoint::start(200, Duration::ZERO, "ok".into()).await;
        insert_hook(db.pool(), &other.org_id, &org_hook.url).await;
        // Four slots: one org (or webhook) gets one at a time.
        let service = Arc::new(OutgoingWebhookService::new((*db.pool).clone(), 1, 4));

        complete_to(&busy.org_id, &black_hole.url, 12);
        assert!(eventually(Duration::from_secs(5), || black_hole.hits() >= 1).await);
        tokio::time::sleep(Duration::from_millis(200)).await;

        fire(&service, &other.org_id);
        complete_to(&other.org_id, &completion.url, 1);
        assert!(
            eventually(Duration::from_millis(1500), || org_hook.hits() == 1).await,
            "an org webhook waited behind another tenant's completion backlog"
        );
        assert!(
            eventually(Duration::from_millis(1500), || completion.hits() == 1).await,
            "a completion webhook waited behind another tenant's backlog"
        );
        assert_eq!(black_hole.peak(), 1, "4 slots / 4 = one request per org");
        assert_eq!(completion_webhooks_dropped(), 0);
    }

    // W5-R2: one org's completion backlog takes its share of the pending
    // bound, not all of it, so another org's completion webhook still goes out.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn w5_review_one_org_cannot_take_every_pending_completion_webhook() {
        let db = TestDatabase::new().await;
        let slow = Endpoint::start(200, Duration::from_secs(1), "ok".into()).await;
        let fast = Endpoint::start(200, Duration::ZERO, "ok".into()).await;
        // One slot: 16 pending, of which one org may hold 4.
        let _service = OutgoingWebhookService::new((*db.pool).clone(), 1, 1);

        complete_to("w5r-busy-org", &slow.url, 20);
        assert_eq!(
            completion_webhooks_dropped(),
            16,
            "the busy org keeps its share of 4"
        );
        complete_to("w5r-other-org", &fast.url, 1);
        assert_eq!(
            completion_webhooks_dropped(),
            16,
            "the other org's delivery is admitted"
        );
        assert!(
            eventually(Duration::from_secs(3), || fast.hits() == 1).await,
            "the other org's delivery waited for the busy org's backlog"
        );
        assert!(eventually(Duration::from_secs(6), || slow.hits() == 4).await);
    }

    const DLQ_QUEUE: &str = "w5r-dlq";

    async fn post(app: &Router, auth: &str, uri: &str, body: Value) -> (StatusCode, Value) {
        call(app, "POST", uri, &[("Authorization", auth)], Some(body)).await
    }

    /// Dead-letter `job`, dated `age_minutes` back so the DLQ order is fixed.
    async fn dead_letter(pool: &sqlx::PgPool, job: &str, age_minutes: i32) {
        sqlx::query(
            "UPDATE jobs SET status = 'deadletter', last_error = 'boom', \
             created_at = NOW() - make_interval(mins => $2), updated_at = NOW() WHERE id = $1",
        )
        .bind(job)
        .bind(age_minutes)
        .execute(pool)
        .await
        .expect("dead-letter job");
    }

    /// A one-step workflow left 'failed' with its step dead-lettered.
    /// Returns (workflow id, step id).
    async fn failed_workflow(
        app: &Router,
        auth: &str,
        pool: &sqlx::PgPool,
        name: &str,
        age_minutes: i32,
    ) -> (String, String) {
        let (status, body) = post(
            app,
            auth,
            "/api/v1/workflows",
            json!({"name": name, "jobs": [{"key": "a", "queue_name": DLQ_QUEUE, "payload": {}}]}),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let workflow = body["workflow_id"].as_str().unwrap().to_string();
        let step = body["job_ids"][0]["job_id"].as_str().unwrap().to_string();
        dead_letter(pool, &step, age_minutes).await;
        sqlx::query("UPDATE workflows SET status = 'failed', completed_at = NOW() WHERE id = $1")
            .bind(&workflow)
            .execute(pool)
            .await
            .expect("fail workflow");
        (workflow, step)
    }

    /// A plain job, dead-lettered.
    async fn dead_job(app: &Router, auth: &str, pool: &sqlx::PgPool, age_minutes: i32) -> String {
        let (status, body) = post(
            app,
            auth,
            "/api/v1/jobs",
            json!({"queue_name": DLQ_QUEUE, "payload": {}}),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        let id = body["id"].as_str().unwrap().to_string();
        dead_letter(pool, &id, age_minutes).await;
        id
    }

    async fn status_of(pool: &sqlx::PgPool, table: &str, id: &str) -> String {
        let sql = match table {
            "jobs" => "SELECT status FROM jobs WHERE id = $1",
            "workflows" => "SELECT status FROM workflows WHERE id = $1",
            other => panic!("no status query for {other}"),
        };
        sqlx::query_scalar(sql)
            .bind(id)
            .fetch_one(pool)
            .await
            .unwrap()
    }

    fn ids(body: &Value, field: &str) -> Vec<String> {
        let mut ids: Vec<String> = body[field]
            .as_array()
            .unwrap_or_else(|| panic!("{field} in {body}"))
            .iter()
            .map(|v| v.as_str().unwrap().to_string())
            .collect();
        ids.sort();
        ids
    }

    fn sorted(ids: &[&String]) -> Vec<String> {
        let mut ids: Vec<String> = ids.iter().map(|s| s.to_string()).collect();
        ids.sort();
        ids
    }

    // W5-R3: a bulk DLQ retry failed whole (429, or 403 on Free) whenever the
    // jobs it picked included a step of a failed workflow the max_workflows cap
    // could not revive, so no dead-lettered job could be retried in bulk until
    // a workflow slot freed up.
    #[tokio::test]
    async fn w5_review_dlq_retry_skips_steps_the_workflow_cap_cannot_revive() {
        let db = TestDatabase::new().await;
        let pool = db.pool();
        let t = seed_tenant(pool).await;
        sqlx::query("UPDATE organizations SET custom_limits = $2 WHERE id = $1")
            .bind(&t.org_id)
            .bind(json!({"max_workflows": 1}))
            .execute(pool)
            .await
            .unwrap();
        let app = app(&db);
        let auth = bearer(&t.key);

        let (failed, step) = failed_workflow(&app, &auth, pool, "w5r-failed", 60).await;
        let (failed_2, step_2) = failed_workflow(&app, &auth, pool, "w5r-failed-2", 50).await;
        let (status, body) = post(
            &app,
            &auth,
            "/api/v1/workflows",
            json!({"name": "w5r-running", "jobs": [{"key": "a", "queue_name": "w5r-run", "payload": {}}]}),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let running = body["workflow_id"].as_str().unwrap().to_string();
        let plain = [
            dead_job(&app, &auth, pool, 30).await,
            dead_job(&app, &auth, pool, 20).await,
        ];

        // At the cap, by queue and across queues: the plain jobs are retried,
        // the steps stay in the DLQ and are reported.
        for request in [json!({}), json!({"queue_name": DLQ_QUEUE})] {
            for (job, age) in plain.iter().zip([30, 20]) {
                dead_letter(pool, job, age).await;
            }
            let (status, body) = post(&app, &auth, "/api/v1/jobs/dlq/retry", request.clone()).await;
            assert_eq!(status, StatusCode::OK, "{request}: {body}");
            assert_eq!(body["retried_count"], 2, "{body}");
            assert_eq!(ids(&body, "retried_jobs"), sorted(&[&plain[0], &plain[1]]));
            assert_eq!(
                body["skipped_jobs"],
                json!([step, step_2]),
                "oldest first: {body}"
            );
            for job in &plain {
                assert_eq!(status_of(pool, "jobs", job).await, "pending");
            }
            for (workflow, job) in [(&failed, &step), (&failed_2, &step_2)] {
                assert_eq!(status_of(pool, "workflows", workflow).await, "failed");
                assert_eq!(status_of(pool, "jobs", job).await, "deadletter");
            }
        }

        // Only held steps left: the request names the cap, as a single retry does.
        let (status, body) = post(&app, &auth, "/api/v1/jobs/dlq/retry", json!({})).await;
        assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "{body}");
        assert_eq!(body["error"], "limit_exceeded", "{body}");
        assert_eq!(status_of(pool, "jobs", &step).await, "deadletter");

        // Named ids follow the same rule.
        dead_letter(pool, &plain[0], 30).await;
        let (status, body) = post(
            &app,
            &auth,
            "/api/v1/jobs/dlq/retry",
            json!({"job_ids": [step, plain[0]]}),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["retried_jobs"], json!([plain[0]]), "{body}");
        assert_eq!(body["skipped_jobs"], json!([step]), "{body}");

        // One slot frees: the workflow with the oldest step is revived, the
        // other stays held.
        sqlx::query(
            "UPDATE workflows SET status = 'completed', completed_at = NOW() WHERE id = $1",
        )
        .bind(&running)
        .execute(pool)
        .await
        .unwrap();
        let (status, body) = post(&app, &auth, "/api/v1/jobs/dlq/retry", json!({})).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["retried_jobs"], json!([step]), "{body}");
        assert_eq!(body["skipped_jobs"], json!([step_2]), "{body}");
        assert_eq!(status_of(pool, "workflows", &failed).await, "running");
        assert_eq!(status_of(pool, "workflows", &failed_2).await, "failed");

        // A plan without workflows still retries everything else.
        sqlx::query(
            "UPDATE workflows SET status = 'cancelled', completed_at = NOW() \
             WHERE organization_id = $1 AND status IN ('pending', 'running')",
        )
        .bind(&t.org_id)
        .execute(pool)
        .await
        .unwrap();
        sqlx::query(
            "UPDATE organizations SET plan_tier = 'free', custom_limits = NULL WHERE id = $1",
        )
        .bind(&t.org_id)
        .execute(pool)
        .await
        .unwrap();
        dead_letter(pool, &plain[1], 20).await;
        let (status, body) = post(&app, &auth, "/api/v1/jobs/dlq/retry", json!({})).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["retried_jobs"], json!([plain[1]]), "{body}");
        assert_eq!(body["skipped_jobs"], json!([step_2]), "{body}");
        let (status, body) = post(&app, &auth, "/api/v1/jobs/dlq/retry", json!({})).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
        assert_eq!(body["error"], "feature_disabled", "{body}");
        assert_eq!(status_of(pool, "jobs", &step_2).await, "deadletter");
        assert_eq!(status_of(pool, "workflows", &failed_2).await, "failed");
    }
}

// ---- wave 5 / retention ----
// W5-R4: the failed/dead-letter retention deleted a job that a retry had moved
// back to pending (or that had just failed again) after the sweep picked it.
// W5-R5: it deleted the failed step of a failed workflow that could still be
// retried, so the retry ran the step's dependents ungated.
// W5-R6: schedule_runs.job_id had no index, so every deleted job scanned all of
// schedule_runs, and the schedule_runs sweep read the whole table twice.
mod wave5_retention {
    use super::*;

    use std::collections::HashMap;
    use std::time::Duration;

    use spooled_backend::outgoing_webhooks::service::OutgoingWebhookService;
    use spooled_backend::scheduler::{Scheduler, SCHEDULE_RUNS_RETENTION_SQL};

    const QUEUE: &str = "w5-ret";

    async fn sweep(pool: Arc<sqlx::PgPool>) -> anyhow::Result<()> {
        Scheduler::new(
            Arc::clone(&pool),
            None,
            Arc::new(Metrics::new()),
            Arc::new(OutgoingWebhookService::new((*pool).clone(), 3, 4)),
        )
        .cleanup_stale_workers()
        .await
    }

    /// A job in `status`, created and last changed `age_days` ago.
    async fn old_job(
        pool: &sqlx::PgPool,
        org_id: &str,
        status: &str,
        age_days: i32,
        workflow_id: Option<&str>,
    ) -> String {
        let id = uuid::Uuid::new_v4().to_string();
        sqlx::query(
            "INSERT INTO jobs (id, organization_id, queue_name, payload, status, last_error, workflow_id, \
             created_at, updated_at, completed_at) \
             VALUES ($1, $2, $3, '{}'::JSONB, $4, 'boom', $5, NOW() - make_interval(days => $6), \
             NOW() - make_interval(days => $6), NOW() - make_interval(days => $6))",
        )
        .bind(&id)
        .bind(org_id)
        .bind(QUEUE)
        .bind(status)
        .bind(workflow_id)
        .bind(age_days)
        .execute(pool)
        .await
        .unwrap();
        id
    }

    /// Put a job in `status`, finished `age_days` ago. The `jobs_updated_at`
    /// trigger stamps every UPDATE with the current time, so it is off for
    /// this one statement.
    async fn finish_job(pool: &sqlx::PgPool, job: &str, status: &str, age_days: i32) {
        let mut tx = pool.begin().await.unwrap();
        sqlx::query("ALTER TABLE jobs DISABLE TRIGGER jobs_updated_at")
            .execute(&mut *tx)
            .await
            .unwrap();
        sqlx::query(
            "UPDATE jobs SET status = $2, updated_at = NOW() - make_interval(days => $3), \
             completed_at = NOW() - make_interval(days => $3) WHERE id = $1",
        )
        .bind(job)
        .bind(status)
        .bind(age_days)
        .execute(&mut *tx)
        .await
        .unwrap();
        sqlx::query("ALTER TABLE jobs ENABLE TRIGGER jobs_updated_at")
            .execute(&mut *tx)
            .await
            .unwrap();
        tx.commit().await.unwrap();
    }

    /// Close a workflow as `status`, `age_days` ago.
    async fn finish_workflow(pool: &sqlx::PgPool, workflow: &str, status: &str, age_days: i32) {
        sqlx::query(
            "UPDATE workflows SET status = $2, completed_at = NOW() - make_interval(days => $3) \
             WHERE id = $1",
        )
        .bind(workflow)
        .bind(status)
        .bind(age_days)
        .execute(pool)
        .await
        .unwrap();
    }

    /// Create a workflow through the API; returns (workflow id, key -> job id).
    async fn create_workflow(
        app: &Router,
        auth: &str,
        jobs: Value,
    ) -> (String, HashMap<String, String>) {
        let (status, body) = call(
            app,
            "POST",
            "/api/v1/workflows",
            &[("Authorization", auth)],
            Some(json!({"name": "w5-ret", "jobs": jobs})),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let ids = body["job_ids"]
            .as_array()
            .expect("job_ids")
            .iter()
            .map(|m| {
                (
                    m["key"].as_str().unwrap().to_string(),
                    m["job_id"].as_str().unwrap().to_string(),
                )
            })
            .collect();
        (body["workflow_id"].as_str().unwrap().to_string(), ids)
    }

    /// A job's status, or None once it is deleted.
    async fn job_status(pool: &sqlx::PgPool, id: &str) -> Option<String> {
        sqlx::query_scalar("SELECT status FROM jobs WHERE id = $1")
            .bind(id)
            .fetch_optional(pool)
            .await
            .unwrap()
    }

    async fn workflow_exists(pool: &sqlx::PgPool, id: &str) -> bool {
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM workflows WHERE id = $1)")
            .bind(id)
            .fetch_one(pool)
            .await
            .unwrap()
    }

    // W5-R4: the sweep picked the old failed rows in a subselect and deleted
    // `WHERE id IN (...)` without looking at them again. A retry that moved a
    // picked job back to pending, or a job that failed again just now, was
    // deleted as soon as the retry's transaction let go of the row. The retry
    // here is held open until the sweep has either finished or is waiting on
    // its row locks, so the race is hit every run.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn w5_retention_leaves_a_job_retried_while_the_sweep_runs() {
        let db = TestDatabase::new().await;
        let t = seed_tenant(db.pool()).await; // starter: jobs 14 days
        let pool = db.pool();
        let retried = old_job(pool, &t.org_id, "deadletter", 20, None).await;
        let failed_again = old_job(pool, &t.org_id, "failed", 20, None).await;
        let untouched = old_job(pool, &t.org_id, "deadletter", 20, None).await;

        let mut retry = pool.begin().await.unwrap();
        sqlx::query(
            "UPDATE jobs SET status = 'pending', retry_count = 0, last_error = NULL WHERE id = $1",
        )
        .bind(&retried)
        .execute(&mut *retry)
        .await
        .unwrap();
        sqlx::query("UPDATE jobs SET status = 'failed', last_error = 'again' WHERE id = $1")
            .bind(&failed_again)
            .execute(&mut *retry)
            .await
            .unwrap();

        let task = tokio::spawn(sweep(Arc::clone(&db.pool)));
        let mut blocked = false;
        for _ in 0..600 {
            if task.is_finished() {
                break;
            }
            blocked = sqlx::query_scalar(
                "SELECT EXISTS (SELECT 1 FROM pg_stat_activity \
                 WHERE datname = current_database() AND pid <> pg_backend_pid() \
                   AND wait_event_type = 'Lock')",
            )
            .fetch_one(pool)
            .await
            .unwrap();
            if blocked {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(
            blocked || task.is_finished(),
            "the sweep neither finished nor waited on the retry"
        );
        retry.commit().await.unwrap();
        tokio::time::timeout(Duration::from_secs(30), task)
            .await
            .expect("sweep finishes")
            .unwrap()
            .unwrap();

        assert_eq!(
            job_status(pool, &retried).await.as_deref(),
            Some("pending"),
            "a job retried while the sweep ran was deleted"
        );
        assert_eq!(
            job_status(pool, &failed_again).await.as_deref(),
            Some("failed"),
            "a job that had just failed again was deleted"
        );
        assert_eq!(job_status(pool, &untouched).await, None, "old dead-letter");
    }

    // W5-R5: a failed workflow can be retried until workflow retention removes
    // it. The sweep deleted its dead-lettered step once the step itself was
    // old, which also dropped the step's dependency edges, and the retry then
    // re-opened the cancelled dependent as runnable before the step ran again.
    #[tokio::test]
    async fn w5_retention_keeps_the_failed_step_of_a_retryable_workflow() {
        let db = TestDatabase::new().await;
        let t = seed_tenant(db.pool()).await; // starter: jobs 14 days
        let pool = db.pool();
        let app = app(&db);
        let auth = bearer(&t.key);

        // a dead-lettered 20 days ago, b (waits on a) was cancelled for it, and
        // c ran until yesterday, when the workflow ended failed.
        let (workflow, steps) = create_workflow(
            &app,
            &auth,
            json!([
                {"key": "a", "queue_name": QUEUE, "payload": {}},
                {"key": "b", "queue_name": QUEUE, "payload": {}, "depends_on": ["a"]},
                {"key": "c", "queue_name": QUEUE, "payload": {}},
            ]),
        )
        .await;
        finish_job(pool, &steps["a"], "deadletter", 20).await;
        finish_job(pool, &steps["b"], "cancelled", 1).await;
        finish_job(pool, &steps["c"], "completed", 1).await;
        finish_workflow(pool, &workflow, "failed", 1).await;

        sweep(Arc::clone(&db.pool)).await.unwrap();

        let (status, body) = call(
            &app,
            "POST",
            &format!("/api/v1/workflows/{workflow}/retry"),
            &[("Authorization", &auth)],
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let (b_status, b_ready): (String, Option<bool>) =
            sqlx::query_as("SELECT status, dependencies_met FROM jobs WHERE id = $1")
                .bind(&steps["b"])
                .fetch_one(pool)
                .await
                .unwrap();
        assert_eq!(b_status, "pending");
        assert_eq!(
            b_ready,
            Some(false),
            "the retried workflow released b before a ran again"
        );
        assert_eq!(
            job_status(pool, &steps["a"]).await.as_deref(),
            Some("pending"),
            "the dead-lettered step of a retryable workflow was deleted"
        );
    }

    // W5-R5: a workflow's failed and cancelled steps go once the workflow
    // itself is past retention, in the same pass; a step whose workflow is
    // already gone goes as before.
    #[tokio::test]
    async fn w5_retention_purges_workflow_steps_with_their_workflow() {
        let db = TestDatabase::new().await;
        let t = seed_tenant(db.pool()).await; // starter: jobs 14 days
        let pool = db.pool();
        let app = app(&db);
        let auth = bearer(&t.key);
        let jobs = json!([
            {"key": "a", "queue_name": QUEUE, "payload": {}},
            {"key": "b", "queue_name": QUEUE, "payload": {}, "depends_on": ["a"]},
        ]);

        let (expired, expired_steps) = create_workflow(&app, &auth, jobs.clone()).await;
        finish_job(pool, &expired_steps["a"], "deadletter", 20).await;
        finish_job(pool, &expired_steps["b"], "cancelled", 20).await;
        finish_workflow(pool, &expired, "failed", 20).await;

        let (retryable, retryable_steps) = create_workflow(&app, &auth, jobs).await;
        finish_job(pool, &retryable_steps["a"], "deadletter", 20).await;
        finish_job(pool, &retryable_steps["b"], "cancelled", 20).await;
        finish_workflow(pool, &retryable, "failed", 1).await;

        let orphan = old_job(pool, &t.org_id, "deadletter", 20, Some("w5-ret-gone")).await;

        sweep(Arc::clone(&db.pool)).await.unwrap();

        assert!(!workflow_exists(pool, &expired).await, "expired workflow");
        assert_eq!(job_status(pool, &expired_steps["a"]).await, None);
        assert_eq!(job_status(pool, &expired_steps["b"]).await, None);
        assert!(workflow_exists(pool, &retryable).await);
        assert_eq!(
            job_status(pool, &retryable_steps["a"]).await.as_deref(),
            Some("deadletter"),
            "failed step of a workflow that can still be retried"
        );
        assert_eq!(
            job_status(pool, &retryable_steps["b"]).await.as_deref(),
            Some("cancelled"),
            "cancelled step of a workflow that can still be retried"
        );
        assert_eq!(
            job_status(pool, &orphan).await,
            None,
            "its workflow is gone"
        );
    }

    /// The generic plan (the one a foreign-key action's cached query uses) of
    /// the ON DELETE SET NULL action of schedule_runs_job_id_fkey.
    async fn fk_action_plan(pool: &sqlx::PgPool) -> String {
        let mut conn = pool.acquire().await.unwrap();
        sqlx::raw_sql(
            "SET plan_cache_mode = force_generic_plan; \
             PREPARE w5_ret_fk(TEXT) AS \
             UPDATE ONLY schedule_runs SET job_id = NULL WHERE $1 OPERATOR(pg_catalog.=) job_id",
        )
        .execute(&mut *conn)
        .await
        .unwrap();
        let plan: Vec<String> = sqlx::query_scalar("EXPLAIN EXECUTE w5_ret_fk('w5-ret-job-1')")
            .fetch_all(&mut *conn)
            .await
            .unwrap();
        sqlx::raw_sql("DEALLOCATE w5_ret_fk; RESET plan_cache_mode")
            .execute(&mut *conn)
            .await
            .unwrap();
        plan.join("\n")
    }

    async fn index_valid(pool: &sqlx::PgPool, index: &str) -> bool {
        sqlx::query_scalar("SELECT indisvalid FROM pg_index WHERE indexrelid = to_regclass($1)")
            .bind(index)
            .fetch_one(pool)
            .await
            .unwrap_or_else(|e| panic!("{index}: {e}"))
    }

    // W5-R6: deleting a job sets schedule_runs.job_id to NULL for its runs; with
    // no index on the column that was a scan of all of schedule_runs per job
    // (27 ms a job at 450k runs, so 55 s for 2,000 dead-lettered jobs). The
    // schedule_runs sweep now ranges over (schedule_id, started_at) per
    // schedule instead of reading the table twice.
    #[tokio::test]
    async fn w5_retention_schedule_run_lookups_use_indexes() {
        let db = TestDatabase::new().await;
        let t = seed_tenant(db.pool()).await;
        let pool = db.pool();
        sqlx::query(
            "INSERT INTO schedules (id, organization_id, name, cron_expression, queue_name, payload_template) \
             SELECT 'w5-ret-' || g, $1, 'ret ' || g, '* * * * *', $2, '{}'::JSONB \
             FROM generate_series(1, 20) AS g",
        )
        .bind(&t.org_id)
        .bind(QUEUE)
        .execute(pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO jobs (id, organization_id, queue_name, payload, status) \
             SELECT 'w5-ret-job-' || g, $1, $2, '{}'::JSONB, 'completed' \
             FROM generate_series(1, 1000) AS g",
        )
        .bind(&t.org_id)
        .bind(QUEUE)
        .execute(pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO schedule_runs (id, schedule_id, job_id, status, started_at) \
             SELECT 'w5-ret-run-' || g, 'w5-ret-' || (1 + g % 20), \
                    CASE WHEN g <= 1000 THEN 'w5-ret-job-' || g END, 'completed', \
                    NOW() - make_interval(mins => g) \
             FROM generate_series(1, 18000) AS g",
        )
        .execute(pool)
        .await
        .unwrap();
        sqlx::query("ANALYZE schedule_runs")
            .execute(pool)
            .await
            .unwrap();

        // The ON DELETE SET NULL action of schedule_runs_job_id_fkey.
        let plan = fk_action_plan(pool).await;
        assert!(
            plan.contains("idx_schedule_runs_job_id"),
            "foreign-key action plan:\n{plan}"
        );

        let plan: Vec<String> = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
            "EXPLAIN {SCHEDULE_RUNS_RETENTION_SQL}"
        )))
        .fetch_all(pool)
        .await
        .unwrap();
        let plan = plan.join("\n");
        assert!(
            plan.contains("idx_schedule_runs_schedule_started"),
            "retention plan:\n{plan}"
        );
        assert!(
            !plan.contains("Seq Scan on schedule_runs"),
            "retention plan:\n{plan}"
        );

        // And the sweep still removes exactly the runs past retention
        // (starter: 7 days of history); 18,000 minutes is 12.5 days.
        sweep(Arc::clone(&db.pool)).await.unwrap();
        let (left, oldest_ok): (i64, bool) = sqlx::query_as(
            "SELECT COUNT(*), COALESCE(MIN(started_at) > NOW() - INTERVAL '7 days', TRUE) \
             FROM schedule_runs",
        )
        .fetch_one(pool)
        .await
        .unwrap();
        assert!(oldest_ok, "a run past retention is left");
        assert!(
            (10_070..=10_080).contains(&left),
            "{left} runs left of 18,000; 7 days is 10,080 minutes"
        );
    }

    // W5-R6: the two indexes are built CONCURRENTLY; an interrupted build
    // leaves an INVALID index that IF NOT EXISTS skips, and the repair
    // migrations rebuild it.
    #[tokio::test]
    async fn w5_retention_index_repairs_rebuild_an_invalid_index() {
        let db = TestDatabase::new().await;
        for (index, repair) in [
            (
                "idx_schedule_runs_job_id",
                include_str!("../migrations/20261013140001_schedule_runs_job_id_index_repair.sql"),
            ),
            (
                "idx_schedule_runs_schedule_started",
                include_str!(
                    "../migrations/20261013140101_schedule_runs_retention_index_repair.sql"
                ),
            ),
        ] {
            assert!(
                index_valid(db.pool(), index).await,
                "{index} after migrations"
            );
            sqlx::query(
                "UPDATE pg_index SET indisvalid = FALSE WHERE indexrelid = to_regclass($1)",
            )
            .bind(index)
            .execute(db.pool())
            .await
            .unwrap();
            assert!(!index_valid(db.pool(), index).await);

            let mut tx = db.pool().begin().await.unwrap();
            sqlx::raw_sql(repair).execute(&mut *tx).await.unwrap();
            tx.commit().await.unwrap();
            assert!(
                index_valid(db.pool(), index).await,
                "{index} after its repair migration"
            );
        }
    }
}
// ---- wave 8 / ratelimit ----
// QW9 (`X-RateLimit-*` headers from the plan limiter) and QW16 (router vs
// docs/openapi.yaml).
mod wave8_ratelimit {
    use super::*;

    use spooled_backend::api::middleware::plan_rate_limit::{
        HEADER_LIMIT, HEADER_REMAINING, HEADER_RESET, HEADER_SCOPE,
    };
    use spooled_backend::cache::RedisCache;
    use spooled_backend::config::RedisSettings;

    /// Router on Redis. Keep the `TestRedis` alive.
    pub(super) async fn redis_app(
        db: &TestDatabase,
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
            Settings::load_for_testing(),
        ));
        (app, cache, redis)
    }

    pub(super) fn number(headers: &axum::http::HeaderMap, name: &str) -> Option<u64> {
        headers
            .get(name)
            .map(|v| v.to_str().unwrap().parse().expect("numeric header"))
    }

    pub(super) fn text<'a>(headers: &'a axum::http::HeaderMap, name: &str) -> Option<&'a str> {
        headers.get(name).map(|v| v.to_str().unwrap())
    }

    /// Unix time in whole seconds, the unit of `X-RateLimit-Reset`.
    pub(super) fn unix_now() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
    }

    fn has_no_rate_limit_headers(headers: &axum::http::HeaderMap) -> bool {
        [HEADER_LIMIT, HEADER_REMAINING, HEADER_RESET, HEADER_SCOPE]
            .iter()
            .all(|name| headers.get(*name).is_none())
    }

    pub(super) async fn set_org_rate_limit(
        pool: &sqlx::PgPool,
        org_id: &str,
        rps: i32,
        burst: i32,
    ) {
        sqlx::query(
            "UPDATE organizations SET custom_limits = jsonb_build_object( \
             'rate_limit_requests_per_second', $2::INT, 'rate_limit_burst', $3::INT) WHERE id = $1",
        )
        .bind(org_id)
        .bind(rps)
        .bind(burst)
        .execute(pool)
        .await
        .expect("set custom rate limit");
    }

    /// Give the tenant's (only) key its own per-minute limit; returns its id.
    pub(super) async fn set_key_rate_limit(
        pool: &sqlx::PgPool,
        org_id: &str,
        per_minute: i32,
    ) -> String {
        sqlx::query_scalar(
            "UPDATE api_keys SET rate_limit = $2 WHERE organization_id = $1 RETURNING id",
        )
        .bind(org_id)
        .bind(per_minute)
        .fetch_one(pool)
        .await
        .expect("set key rate limit")
    }

    pub(super) async fn get_jobs(
        app: &Router,
        auth: &str,
    ) -> (StatusCode, axum::http::HeaderMap, Value) {
        call_with_headers(app, "GET", "/api/v1/jobs", &[("Authorization", auth)], None).await
    }

    // -----------------------------------------------------------------------
    // QW9: the organization bucket's numbers ride on every authenticated
    // response. Limit is the burst, Remaining the whole tokens left after this
    // request, Reset the Unix time of the next whole token; the 429 that
    // follows carries the same headers with Remaining 0 and Reset == now +
    // Retry-After == the body's reset_at.
    // -----------------------------------------------------------------------
    #[tokio::test]
    async fn w8_ratelimit_org_bucket_headers_on_success_and_on_429() {
        let db = TestDatabase::new().await;
        let (app, _cache, _redis) = redis_app(&db).await;
        let tenant = seed_tenant(db.pool()).await;
        set_org_rate_limit(db.pool(), &tenant.org_id, 1, 3).await;
        let auth = bearer(&tenant.key);

        let before = unix_now();
        let (status, headers, body) = get_jobs(&app, &auth).await;
        let after = unix_now();
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(number(&headers, HEADER_LIMIT), Some(3));
        assert_eq!(
            number(&headers, HEADER_REMAINING),
            Some(2),
            "fresh bucket of 3, one spent"
        );
        let reset = number(&headers, HEADER_RESET).unwrap();
        assert!(
            (before + 1..=after + 1).contains(&reset),
            "the spent token comes back within a second at 1 rps: {reset} vs {before}..={after}"
        );
        assert_eq!(text(&headers, HEADER_SCOPE), Some("organization"));
        assert!(headers.get("retry-after").is_none());

        // Spend the rest of the bucket. Remaining never rises faster than the
        // 1 rps refill, and the refusal reports what Retry-After says.
        let mut last_remaining = 2;
        let mut refused = None;
        for _ in 0..10 {
            let before = unix_now();
            let (status, headers, body) = get_jobs(&app, &auth).await;
            let after = unix_now();
            assert_eq!(number(&headers, HEADER_LIMIT), Some(3), "{body}");
            if status == StatusCode::TOO_MANY_REQUESTS {
                refused = Some((headers, body, before, after));
                break;
            }
            assert_eq!(status, StatusCode::OK, "{body}");
            let remaining = number(&headers, HEADER_REMAINING).unwrap();
            assert!(
                remaining <= last_remaining + 1,
                "{remaining} after {last_remaining}"
            );
            last_remaining = remaining;
        }
        let (headers, body, before, after) = refused.expect("a bucket of 3 at 1 rps runs out");
        assert_eq!(body["code"], "RATE_LIMIT_EXCEEDED");
        let retry_after = number(&headers, "retry-after").expect("Retry-After on 429");
        assert!(retry_after >= 1);
        assert_eq!(number(&headers, HEADER_REMAINING), Some(0));
        let reset = number(&headers, HEADER_RESET).unwrap();
        assert!(
            (before + retry_after..=after + retry_after).contains(&reset),
            "{reset} is not now + Retry-After {retry_after}"
        );
        assert_eq!(body["reset_at"].as_u64(), Some(reset), "{body}");
        assert_eq!(text(&headers, HEADER_SCOPE), Some("organization"));
        assert_eq!(body["limit"], 3);
    }

    // -----------------------------------------------------------------------
    // QW9: a key with its own per-minute rate_limit tighter than the org bucket
    // reports that limit (scope api_key; Reset = Unix time the window ends),
    // and its 429 carries the same numbers, Reset equal to now + Retry-After.
    // -----------------------------------------------------------------------
    #[tokio::test]
    async fn w8_ratelimit_tighter_api_key_limit_is_reported() {
        let db = TestDatabase::new().await;
        let (app, _cache, _redis) = redis_app(&db).await;
        let tenant = seed_tenant(db.pool()).await;
        set_key_rate_limit(db.pool(), &tenant.org_id, 2).await;
        let auth = bearer(&tenant.key);

        for expected_remaining in [1, 0] {
            let before = unix_now();
            let (status, headers, body) = get_jobs(&app, &auth).await;
            let after = unix_now();
            assert_eq!(status, StatusCode::OK, "{body}");
            assert_eq!(text(&headers, HEADER_SCOPE), Some("api_key"));
            assert_eq!(number(&headers, HEADER_LIMIT), Some(2));
            assert_eq!(number(&headers, HEADER_REMAINING), Some(expected_remaining));
            let reset = number(&headers, HEADER_RESET).unwrap();
            assert!(
                (before + 1..=after + 60).contains(&reset),
                "one-minute window ends at a Unix time: {reset} vs {before}..={after}"
            );
        }

        let before = unix_now();
        let (status, headers, body) = get_jobs(&app, &auth).await;
        let after = unix_now();
        assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "{body}");
        let retry_after = number(&headers, "retry-after").expect("Retry-After on 429");
        assert!((1..=60).contains(&retry_after));
        assert_eq!(text(&headers, HEADER_SCOPE), Some("api_key"));
        assert_eq!(number(&headers, HEADER_LIMIT), Some(2));
        assert_eq!(number(&headers, HEADER_REMAINING), Some(0));
        let reset = number(&headers, HEADER_RESET).unwrap();
        assert!(
            (before + retry_after..=after + retry_after).contains(&reset),
            "{reset} is not now + Retry-After {retry_after}"
        );
        assert_eq!(body["reset_at"].as_u64(), Some(reset), "{body}");
    }

    // -----------------------------------------------------------------------
    // QW9: when a check is skipped on a Redis error (fail-open, the self-host
    // default) or there is no Redis at all, the request goes through with no
    // X-RateLimit-* headers rather than made-up numbers. Public routes are
    // limited per client address and report nothing either.
    // -----------------------------------------------------------------------
    #[tokio::test]
    async fn w8_ratelimit_no_headers_when_the_limiter_is_bypassed() {
        let db = TestDatabase::new().await;
        let (app, cache, _redis) = redis_app(&db).await;

        // A string where the bucket hash belongs: the token-bucket script
        // fails with WRONGTYPE, which is how a Redis error reaches the limiter.
        let tenant = seed_tenant(db.pool()).await;
        let auth = bearer(&tenant.key);
        cache
            .set(&format!("rate_limit:org:{}", tenant.org_id), "broken", 60)
            .await
            .unwrap();
        let (status, headers, body) = get_jobs(&app, &auth).await;
        assert_eq!(status, StatusCode::OK, "fail-open lets it through: {body}");
        assert!(has_no_rate_limit_headers(&headers), "{headers:?}");

        // The org bucket answers but the key's own counter fails (INCR on a
        // non-number): the binding limit is unknown, so still no headers.
        let other = seed_tenant(db.pool()).await;
        let key_id = set_key_rate_limit(db.pool(), &other.org_id, 5).await;
        cache
            .set(&format!("rate_limit:api_key:{key_id}"), "broken", 60)
            .await
            .unwrap();
        let other_auth = bearer(&other.key);
        let (status, headers, body) = get_jobs(&app, &other_auth).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(has_no_rate_limit_headers(&headers), "{headers:?}");
        // Once the counter works again the key's numbers come back.
        cache
            .delete(&format!("rate_limit:api_key:{key_id}"))
            .await
            .unwrap();
        let (status, headers, body) = get_jobs(&app, &other_auth).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(text(&headers, HEADER_SCOPE), Some("api_key"));
        assert_eq!(number(&headers, HEADER_REMAINING), Some(4));

        // No Redis configured (self-host without RATE_LIMIT_FAIL_CLOSED).
        let no_redis = app_with(&db, Settings::load_for_testing());
        let (status, headers, body) = get_jobs(&no_redis, &auth).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(has_no_rate_limit_headers(&headers), "{headers:?}");

        // Public routes.
        let (status, headers, body) = call_with_headers(
            &app,
            "GET",
            "/api/v1/auth/check-email?email=nobody-w8@example.com",
            &[],
            None,
        )
        .await;
        assert!(
            status == StatusCode::OK || status == StatusCode::TOO_MANY_REQUESTS,
            "{status} {body}"
        );
        assert!(has_no_rate_limit_headers(&headers), "{headers:?}");
    }

    // QW9: the token-bucket result carries the time to the next whole token,
    // which equals the retry wait once the bucket is empty.
    #[tokio::test]
    async fn w8_ratelimit_token_bucket_reports_time_to_the_next_token() {
        let db = TestDatabase::new().await;
        let (_app, cache, _redis) = redis_app(&db).await;
        let key = format!("rate_limit:w8:{}", uuid::Uuid::new_v4());

        let first = cache.check_token_bucket(&key, 5, 10).await.unwrap();
        assert!(first.allowed);
        assert_eq!(first.remaining, 9);
        assert!(
            (1..=200).contains(&first.next_token_ms),
            "5 rps: a token every 200 ms, got {}",
            first.next_token_ms
        );

        let mut last = first;
        for _ in 0..20 {
            last = cache.check_token_bucket(&key, 5, 10).await.unwrap();
            if !last.allowed {
                break;
            }
        }
        assert!(!last.allowed, "10 tokens at 5 rps run out within 20 calls");
        assert_eq!(last.remaining, 0);
        assert!(last.retry_after_ms > 0);
        assert_eq!(last.next_token_ms, last.retry_after_ms);
        cache.delete(&key).await.unwrap();
    }

    /// Every (METHOD, path) under `paths:` of docs/openapi.yaml (block style,
    /// two-space indentation, as scripts/check_openapi_drift.py reads it).
    fn documented_operations() -> Vec<(String, String)> {
        let doc = include_str!("../docs/openapi.yaml");
        let mut ops = Vec::new();
        let mut in_paths = false;
        let mut current: Option<String> = None;
        for line in doc.lines() {
            if line == "paths:" {
                in_paths = true;
                continue;
            }
            if !in_paths || line.trim().is_empty() || line.trim_start().starts_with('#') {
                continue;
            }
            if !line.starts_with(' ') {
                break;
            }
            if let Some(key) = line.strip_prefix("  ").filter(|l| !l.starts_with(' ')) {
                let key = key.trim_end().trim_end_matches(':');
                current = Some(key.trim_matches(|c| c == '\'' || c == '"').to_string());
                continue;
            }
            if let Some(key) = line.strip_prefix("    ").filter(|l| !l.starts_with(' ')) {
                let key = key.split(':').next().unwrap_or_default();
                if ["get", "post", "put", "delete", "patch"].contains(&key) {
                    ops.push((key.to_uppercase(), current.clone().expect("path")));
                }
            }
        }
        ops
    }

    /// The `pending` operations of scripts/openapi-drift-allowlist.json:
    /// documented ahead of a route that lands in another branch.
    fn pending_operations() -> Vec<(String, String)> {
        let list: Value =
            serde_json::from_str(include_str!("../scripts/openapi-drift-allowlist.json"))
                .expect("allowlist JSON");
        list["pending"]
            .as_array()
            .expect("pending list")
            .iter()
            .map(|e| {
                (
                    e["method"].as_str().unwrap().to_uppercase(),
                    e["path"].as_str().unwrap().to_string(),
                )
            })
            .collect()
    }

    // -----------------------------------------------------------------------
    // QW16: every operation docs/openapi.yaml documents reaches a route of the
    // real router: an unauthenticated probe gets a handler's or a
    // middleware's answer (401, 400, ...), never the router's own "No route
    // matches this path" 404 or a 405. This checks the document against
    // axum's matcher rather than against the drift script's reading of
    // src/api/mod.rs.
    // -----------------------------------------------------------------------
    #[tokio::test]
    async fn w8_openapi_documented_operations_reach_a_route() {
        let db = TestDatabase::new().await;
        let app = app(&db);
        let pending = pending_operations();
        let ops = documented_operations();
        assert!(
            ops.len() >= 100,
            "read {} operations from openapi.yaml",
            ops.len()
        );
        assert!(ops.contains(&("GET".into(), "/api/v1/organizations/{id}/members".into())));

        let mut unrouted = Vec::new();
        for (method, path) in &ops {
            if pending.contains(&(method.clone(), path.clone())) {
                continue;
            }
            let uri = path
                .split('/')
                .map(|seg| {
                    if seg.starts_with('{') {
                        "w8-drift-probe"
                    } else {
                        seg
                    }
                })
                .collect::<Vec<_>>()
                .join("/");
            let (status, body) = call(&app, method, &uri, &[], None).await;
            let router_404 =
                status == StatusCode::NOT_FOUND && body["message"] == "No route matches this path";
            if router_404 || status == StatusCode::METHOD_NOT_ALLOWED {
                unrouted.push(format!("{method} {path} -> {status}"));
            }
        }
        assert!(
            unrouted.is_empty(),
            "documented but not routed: {unrouted:#?}"
        );

        // The probe does tell the two apart.
        let (status, body) = call(&app, "GET", "/api/v1/w8-not-a-route", &[], None).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(body["message"], "No route matches this path");
        let (status, _) = call(&app, "PATCH", "/api/v1/jobs", &[], None).await;
        assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED);
    }

    // QW16: the CI drift check (scripts/check_openapi_drift.py, both
    // directions) passes on this tree, so `cargo test` catches drift too.
    // Skipped, loudly, where no Python 3 is installed; CI always has one.
    #[test]
    fn w8_openapi_drift_script_passes_on_this_tree() {
        let script = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/scripts/check_openapi_drift.py"
        );
        let Some(output) = ["python3", "python"].iter().find_map(|python| {
            std::process::Command::new(python)
                .arg(script)
                .output()
                .ok()
                .filter(|out| {
                    // The Windows Store `python3` alias exists but is not Python.
                    String::from_utf8_lossy(&out.stdout).contains("routed operations")
                        || String::from_utf8_lossy(&out.stderr).contains("drift")
                })
        }) else {
            eprintln!("SKIPPED: no Python 3 to run {script}");
            return;
        };
        assert!(
            output.status.success(),
            "drift check failed:\n{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
}
// ---- wave 8 / jobsapi ----
// Phase-1 quick wins: a job's history (QW10), server-side job list filters
// with a paging hint (QW12) and per-worker job counters (QW15).
mod w8_jobsapi {
    use super::*;

    use chrono::{DateTime, Duration, Utc};

    /// GET returning (status, body, `X-Has-More` header).
    async fn get_paged(app: &Router, auth: &str, uri: &str) -> (StatusCode, Value, Option<String>) {
        let req = Request::builder()
            .method("GET")
            .uri(uri)
            .header("Authorization", auth)
            .body(Body::empty())
            .unwrap();
        let res = app.clone().oneshot(req).await.expect("request");
        let status = res.status();
        let has_more = res
            .headers()
            .get("x-has-more")
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        let bytes = axum::body::to_bytes(res.into_body(), 1 << 20)
            .await
            .unwrap();
        let body = if bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&bytes)
                .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into()))
        };
        (status, body, has_more)
    }

    async fn post(app: &Router, auth: &str, uri: &str, body: Value) -> (StatusCode, Value) {
        call(app, "POST", uri, &[("Authorization", auth)], Some(body)).await
    }

    /// An active key of `org_id` limited to `queues`.
    async fn scoped_key(pool: &sqlx::PgPool, org_id: &str, queues: &[&str]) -> String {
        let key = format!("sp_test_{}", uuid::Uuid::new_v4().simple());
        let hash = bcrypt::hash(&key, 4).expect("hash key");
        let queues: Vec<String> = queues.iter().map(|q| q.to_string()).collect();
        sqlx::query(
            "INSERT INTO api_keys (id, organization_id, key_hash, key_prefix, lookup_hash, name, queues, is_active, created_at) \
             VALUES ($1, $2, $3, $4, $5, 'w8-scoped', $6, TRUE, NOW())",
        )
        .bind(uuid::Uuid::new_v4().to_string())
        .bind(org_id)
        .bind(&hash)
        .bind(key.chars().take(8).collect::<String>())
        .bind(spooled_backend::models::api_key_lookup_hash(&key))
        .bind(&queues)
        .execute(pool)
        .await
        .expect("seed scoped key");
        key
    }

    fn ids(body: &Value) -> Vec<String> {
        body.as_array()
            .unwrap_or_else(|| panic!("an array: {body}"))
            .iter()
            .map(|j| j["id"].as_str().unwrap().to_string())
            .collect()
    }

    fn sorted(mut values: Vec<String>) -> Vec<String> {
        values.sort();
        values
    }

    async fn insert_job(
        pool: &sqlx::PgPool,
        org_id: &str,
        id: &str,
        queue: &str,
        status: &str,
        payload: Value,
        created_at: DateTime<Utc>,
    ) {
        sqlx::query(
            "INSERT INTO jobs (id, organization_id, queue_name, status, payload, created_at, updated_at) \
             VALUES ($1, $2, $3, $4, $5, $6, NOW())",
        )
        .bind(id)
        .bind(org_id)
        .bind(queue)
        .bind(status)
        .bind(payload)
        .bind(created_at)
        .execute(pool)
        .await
        .expect("insert job");
    }

    async fn register_worker(app: &Router, auth: &str, queue: &str) -> String {
        let (status, body) = post(
            app,
            auth,
            "/api/v1/workers/register",
            json!({"queue_name": queue, "hostname": "w8-host"}),
        )
        .await;
        assert!(status.is_success(), "{body}");
        body["id"].as_str().expect("worker id").to_string()
    }

    async fn create_job(app: &Router, auth: &str, queue: &str, max_retries: i32) -> String {
        let (status, body) = post(
            app,
            auth,
            "/api/v1/jobs",
            json!({"queue_name": queue, "payload": {"job_type": "w8"}, "max_retries": max_retries}),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        body["id"].as_str().unwrap().to_string()
    }

    /// Claim the one job of `queue`; returns its lease id.
    async fn claim(app: &Router, auth: &str, queue: &str, worker: &str, job: &str) -> String {
        let (status, body) = post(
            app,
            auth,
            "/api/v1/jobs/claim",
            json!({"queue_name": queue, "worker_id": worker, "limit": 1}),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["jobs"][0]["id"], json!(job), "{body}");
        body["jobs"][0]["lease_id"].as_str().unwrap().to_string()
    }

    async fn worker_counters(app: &Router, auth: &str, worker: &str) -> (i64, i64) {
        let (status, body) = call(
            app,
            "GET",
            &format!("/api/v1/workers/{worker}"),
            &[("Authorization", auth)],
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        (
            body["jobs_processed"].as_i64().expect("jobs_processed"),
            body["jobs_failed"].as_i64().expect("jobs_failed"),
        )
    }

    async fn job_status(pool: &sqlx::PgPool, job: &str) -> String {
        sqlx::query_scalar("SELECT status FROM jobs WHERE id = $1")
            .bind(job)
            .fetch_one(pool)
            .await
            .unwrap()
    }

    // QW10: GET /jobs/{id}/history returns the rows the worker calls wrote,
    // oldest first or newest first, paged, behind GET /jobs/{id}'s access rule.
    #[tokio::test]
    async fn w8_job_history_lists_events_in_order_behind_the_job_access_rule() {
        let db = TestDatabase::new().await;
        let t = seed_tenant(db.pool()).await;
        let other = seed_tenant(db.pool()).await;
        let app = app(&db);
        let auth = bearer(&t.key);

        let worker = register_worker(&app, &auth, "w8-hist").await;
        let job = create_job(&app, &auth, "w8-hist", 3).await;
        let lease = claim(&app, &auth, "w8-hist", &worker, &job).await;
        let (status, body) = post(
            &app,
            &auth,
            &format!("/api/v1/jobs/{job}/complete"),
            json!({"worker_id": worker, "lease_id": lease}),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");

        let uri = format!("/api/v1/jobs/{job}/history");
        let events = |body: &Value| -> Vec<String> {
            body.as_array()
                .unwrap_or_else(|| panic!("an array: {body}"))
                .iter()
                .map(|e| e["event_type"].as_str().unwrap().to_string())
                .collect()
        };

        let (status, body, has_more) = get_paged(&app, &auth, &uri).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(events(&body), vec!["processing", "completed"]);
        assert_eq!(has_more.as_deref(), Some("false"));
        let first = &body[0];
        assert_eq!(first["job_id"], json!(job));
        // The claim wrote the worker into `details`; the response surfaces it.
        assert_eq!(first["worker_id"], json!(worker), "{first}");
        assert!(first["id"].is_string() && first["created_at"].is_string());
        assert!(first["details"].is_object());

        let (status, body, _) = get_paged(&app, &auth, &format!("{uri}?order_dir=desc")).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(events(&body), vec!["completed", "processing"]);
        let (_, body, _) = get_paged(&app, &auth, &format!("{uri}?order=DESC")).await;
        assert_eq!(events(&body), vec!["completed", "processing"]);

        let (_, body, has_more) = get_paged(&app, &auth, &format!("{uri}?limit=1")).await;
        assert_eq!(events(&body), vec!["processing"]);
        assert_eq!(has_more.as_deref(), Some("true"));
        let (_, body, has_more) = get_paged(&app, &auth, &format!("{uri}?limit=1&offset=1")).await;
        assert_eq!(events(&body), vec!["completed"]);
        assert_eq!(has_more.as_deref(), Some("false"));

        let (status, body, _) = get_paged(&app, &auth, &format!("{uri}?order_dir=sideways")).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert_eq!(body["code"], "VALIDATION_ERROR");

        // Another organization's key: the job does not exist for it.
        let (status, _, _) = get_paged(&app, &bearer(&other.key), &uri).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        // A key of this org scoped to other queues: the same 404 as GET /jobs/{id}.
        let outside = scoped_key(db.pool(), &t.org_id, &["w8-elsewhere"]).await;
        let (status, _, _) = get_paged(&app, &bearer(&outside), &uri).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        let (status, _) = call(
            &app,
            "GET",
            &format!("/api/v1/jobs/{job}"),
            &[("Authorization", &bearer(&outside))],
            None,
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        // A key whose scope covers the queue reads it.
        let inside = scoped_key(db.pool(), &t.org_id, &["w8-hist"]).await;
        let (status, body, _) = get_paged(&app, &bearer(&inside), &uri).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(events(&body).len(), 2);
        // An unknown id.
        let (status, _, _) = get_paged(&app, &auth, "/api/v1/jobs/no-such-job/history").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    // QW12: status lists, q, job_type and a created window filter on the
    // server, invalid values are a 400, and X-Has-More tells whether another
    // page follows.
    #[tokio::test]
    async fn w8_job_list_filters_on_the_server_with_a_paging_hint() {
        let db = TestDatabase::new().await;
        let t = seed_tenant(db.pool()).await;
        let other = seed_tenant(db.pool()).await;
        let app = app(&db);
        let auth = bearer(&t.key);
        let pool = db.pool();
        // Whole seconds: the window bounds below compare equal to stored values.
        let base = DateTime::from_timestamp(Utc::now().timestamp() - 36_000, 0).unwrap();
        let at = |hours: i64| base + Duration::hours(hours);

        // (id, queue, status, job_type, created hour offset)
        for (id, queue, status, job_type, hour) in [
            ("w8a-failed", "billing-EU", "failed", "send_Email", 1),
            ("w8a-dead", "billing-us", "deadletter", "charge", 2),
            ("w8a-pending", "reports", "pending", "send_email", 3),
            ("w8b-done", "reports", "completed", "render", 4),
            ("x-w8a-dead2", "reports", "deadletter", "render", 5),
            ("w8b-pct", "reports", "completed", "50% off", 6),
        ] {
            insert_job(
                pool,
                &t.org_id,
                id,
                queue,
                status,
                json!({"job_type": job_type}),
                at(hour),
            )
            .await;
        }
        // Another tenant's job that every filter below would match.
        insert_job(
            pool,
            &other.org_id,
            "w8a-foreign",
            "billing-EU",
            "deadletter",
            json!({"job_type": "send_email"}),
            at(2),
        )
        .await;

        let list = |query: String| {
            let app = app.clone();
            let auth = auth.clone();
            async move { get_paged(&app, &auth, &format!("/api/v1/jobs?{query}")).await }
        };

        // A status list (any case, repeats and blanks tolerated), newest first.
        for query in [
            "status=failed,deadletter",
            "status=Failed,%20DEADLETTER,,failed",
        ] {
            let (status, body, _) = list(query.to_string()).await;
            assert_eq!(status, StatusCode::OK, "{query}: {body}");
            assert_eq!(
                ids(&body),
                vec!["x-w8a-dead2", "w8a-dead", "w8a-failed"],
                "{query}"
            );
        }
        let (status, body, _) = list("status=failed,dead".to_string()).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert_eq!(body["code"], "VALIDATION_ERROR");
        assert!(
            body["message"].as_str().unwrap().contains("'dead'"),
            "{body}"
        );

        // q: id prefix (not infix), queue_name or job_type substring, any case.
        let (_, body, _) = list("q=W8A".to_string()).await;
        assert_eq!(
            sorted(ids(&body)),
            vec!["w8a-dead", "w8a-failed", "w8a-pending"]
        );
        let (_, body, _) = list("q=eu".to_string()).await;
        assert_eq!(ids(&body), vec!["w8a-failed"]);
        let (_, body, _) = list("q=EMAIL".to_string()).await;
        assert_eq!(sorted(ids(&body)), vec!["w8a-failed", "w8a-pending"]);
        // `%` and `_` are literal characters, not wildcards.
        let (_, body, _) = list("q=%25".to_string()).await;
        assert_eq!(ids(&body), vec!["w8b-pct"]);
        let (_, body, _) = list("q=w8_".to_string()).await;
        assert_eq!(ids(&body), Vec::<String>::new());
        let (status, body, _) = list(format!("q={}", "x".repeat(256))).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        // Combined with a status list.
        let (_, body, _) = list("q=w8a&status=deadletter,failed".to_string()).await;
        assert_eq!(ids(&body), vec!["w8a-dead", "w8a-failed"]);

        // job_type is exact.
        let (_, body, _) = list("job_type=send_email".to_string()).await;
        assert_eq!(ids(&body), vec!["w8a-pending"]);

        // A created window: created_after inclusive, created_before exclusive.
        let rfc = |hours: i64| {
            at(hours)
                .to_rfc3339_opts(chrono::SecondsFormat::Micros, true)
                .replace('+', "%2B")
        };
        let (status, body, _) = list(format!(
            "created_after={}&created_before={}",
            rfc(2),
            rfc(4)
        ))
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(ids(&body), vec!["w8a-pending", "w8a-dead"]);
        // An offset whose `+` was not encoded arrives as a space.
        let plus_two = at(5)
            .with_timezone(&chrono::FixedOffset::east_opt(2 * 3600).unwrap())
            .to_rfc3339_opts(chrono::SecondsFormat::Micros, false)
            .replace('+', "%20");
        let (status, body, _) = list(format!("created_after={plus_two}")).await;
        assert_eq!(status, StatusCode::OK, "{plus_two}: {body}");
        assert_eq!(ids(&body), vec!["w8b-pct", "x-w8a-dead2"], "{plus_two}");
        for query in [
            "created_after=yesterday".to_string(),
            "created_before=2026-10-11".to_string(),
            format!("created_after={}&created_before={}", rfc(4), rfc(2)),
        ] {
            let (status, body, _) = list(query.clone()).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{query}: {body}");
            assert_eq!(body["code"], "VALIDATION_ERROR", "{query}");
        }

        // The paging hint: one more matching row exists, or not.
        let (_, body, has_more) = list("limit=2".to_string()).await;
        assert_eq!(ids(&body), vec!["w8b-pct", "x-w8a-dead2"]);
        assert_eq!(has_more.as_deref(), Some("true"));
        let (_, body, has_more) = list("limit=2&offset=4".to_string()).await;
        assert_eq!(ids(&body), vec!["w8a-dead", "w8a-failed"]);
        assert_eq!(has_more.as_deref(), Some("false"));
        let (_, body, has_more) = list("status=failed,deadletter&limit=3".to_string()).await;
        assert_eq!(ids(&body).len(), 3);
        assert_eq!(has_more.as_deref(), Some("false"));

        // The DLQ list takes the same search filters (its status stays fixed).
        let (status, body, has_more) = get_paged(
            &app,
            &auth,
            &format!(
                "/api/v1/jobs/dlq?q=w8a&created_before={}&status=pending",
                rfc(5)
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(ids(&body), vec!["w8a-dead"]);
        assert_eq!(has_more.as_deref(), Some("false"));
        let (status, _, _) = get_paged(&app, &auth, "/api/v1/jobs/dlq?created_after=never").await;
        assert_eq!(status, StatusCode::BAD_REQUEST);

        // A queue-scoped key still sees only its queues under the new filters.
        let scoped = scoped_key(pool, &t.org_id, &["billing-us"]).await;
        let (_, body, _) = get_paged(
            &app,
            &bearer(&scoped),
            "/api/v1/jobs?status=failed,deadletter&q=w8a",
        )
        .await;
        assert_eq!(ids(&body), vec!["w8a-dead"]);
    }

    // QW12: the default order, a status list and a created window still walk
    // idx_jobs_org_created on a large org instead of sorting all its rows.
    #[tokio::test]
    async fn w8_job_list_filters_keep_the_org_created_index() {
        let db = TestDatabase::new().await;
        let t = seed_tenant(db.pool()).await;
        sqlx::query(
            "INSERT INTO jobs (id, organization_id, queue_name, status, payload, created_at, updated_at) \
             SELECT gen_random_uuid()::TEXT, $1, 'q' || (g % 20), \
                    (ARRAY['completed','completed','completed','completed','completed', \
                           'completed','pending','deadletter','cancelled','processing'])[1 + g % 10], \
                    jsonb_build_object('job_type', 'type-' || (g % 50)), \
                    NOW() - make_interval(secs => g), NOW() \
             FROM generate_series(1, 30000) AS g",
        )
        .bind(&t.org_id)
        .execute(db.pool())
        .await
        .unwrap();
        sqlx::query("ANALYZE jobs")
            .execute(db.pool())
            .await
            .unwrap();

        // The handler's SQL for these filters (JobListFilters::push), with the
        // page size the default limit asks for (50 + 1).
        for sql in [
            "SELECT * FROM jobs WHERE organization_id = $1 \
             ORDER BY created_at DESC, id DESC LIMIT 51 OFFSET 0",
            "SELECT * FROM jobs WHERE organization_id = $1 \
             AND status = ANY(ARRAY['failed','deadletter']::TEXT[]) \
             ORDER BY created_at DESC, id DESC LIMIT 51 OFFSET 0",
            "SELECT * FROM jobs WHERE organization_id = $1 \
             AND created_at >= NOW() - INTERVAL '2 hours' AND created_at < NOW() - INTERVAL '1 hour' \
             ORDER BY created_at DESC, id DESC LIMIT 51 OFFSET 0",
        ] {
            let plan: Vec<String> = sqlx::query_scalar(sqlx::AssertSqlSafe(format!("EXPLAIN {sql}")))
                .bind(&t.org_id)
                .fetch_all(db.pool())
                .await
                .unwrap();
            let plan = plan.join("\n");
            assert!(plan.contains("idx_jobs_org_created"), "{sql}\n{plan}");
            assert!(!plan.contains("Seq Scan"), "{sql}\n{plan}");
        }
    }

    /// `EXPLAIN` of `sql` with `binds` (all text) as one string.
    async fn explain(pool: &sqlx::PgPool, sql: &str, binds: &[&str]) -> String {
        let explain = format!("EXPLAIN {sql}");
        let mut query = sqlx::query_scalar::<_, String>(sqlx::AssertSqlSafe(explain));
        for bind in binds {
            query = query.bind(*bind);
        }
        query.fetch_all(pool).await.unwrap().join("\n")
    }

    // W8-R2: `job_type` reads idx_jobs_org_job_type, and `q` reaches job types
    // only through that index (a skip scan over the distinct types, then one
    // probe per matching type): no plan compares a payload in a filter, which
    // used to read every payload of the organization from TOAST.
    #[tokio::test]
    async fn w8_job_type_filter_and_search_use_the_job_type_index() {
        let db = TestDatabase::new().await;
        let t = seed_tenant(db.pool()).await;
        let pool = db.pool();
        sqlx::query(
            "INSERT INTO jobs (id, organization_id, queue_name, status, payload, created_at, updated_at) \
             SELECT gen_random_uuid()::TEXT, $1, 'q' || (g % 20), \
                    (ARRAY['completed','completed','completed','completed','completed', \
                           'completed','pending','deadletter','cancelled','processing'])[1 + g % 10], \
                    jsonb_build_object('job_type', \
                        CASE WHEN g % 1000 = 0 THEN 'rare_export' ELSE 'type_' || (g % 40) END, \
                        'data', repeat('x', 200)), \
                    NOW() - make_interval(secs => g), NOW() \
             FROM generate_series(1, 30000) AS g",
        )
        .bind(&t.org_id)
        .execute(pool)
        .await
        .unwrap();
        sqlx::query("ANALYZE jobs").execute(pool).await.unwrap();

        // No plan may evaluate the payload outside an index condition.
        let reads_no_payload = |plan: &str| {
            !plan
                .lines()
                .any(|line| line.contains("Filter:") && line.contains("payload"))
        };

        // The handler's SQL (job_list_query) for an unrestricted key and the
        // default order and page.
        let exact = "SELECT * FROM jobs WHERE organization_id = $1 \
                     AND payload->>'job_type' = $2 AND octet_length(payload->>'job_type') <= 255 \
                     ORDER BY created_at DESC, id DESC LIMIT 51 OFFSET 0";
        let exact_dlq = "SELECT * FROM jobs WHERE organization_id = $1 AND status = 'deadletter' \
                         AND payload->>'job_type' = $2 AND octet_length(payload->>'job_type') <= 255 \
                         ORDER BY created_at DESC, id DESC LIMIT 51 OFFSET 0";
        for (sql, job_type) in [
            (exact, "rare_export"),
            (exact, "type_7"),
            (exact_dlq, "rare_export"),
        ] {
            let plan = explain(pool, sql, &[&t.org_id, job_type]).await;
            assert!(plan.contains("idx_jobs_org_job_type"), "{sql}\n{plan}");
            assert!(!plan.contains("Seq Scan"), "{sql}\n{plan}");
            assert!(reads_no_payload(&plan), "{sql}\n{plan}");
        }

        // q, step one: the organization's job types containing the text.
        let plan = sqlx::query_scalar::<_, String>(sqlx::AssertSqlSafe(format!(
            "EXPLAIN {}",
            spooled_backend::api::handlers::jobs::SEARCH_JOB_TYPES_SQL
        )))
        .bind(&t.org_id)
        .bind(spooled_backend::api::handlers::jobs::MAX_SEARCH_JOB_TYPES)
        .bind("%type\\_1%")
        .fetch_all(pool)
        .await
        .unwrap()
        .join("\n");
        assert!(plan.contains("idx_jobs_org_job_type"), "{plan}");
        assert!(!plan.contains("Seq Scan"), "{plan}");
        assert!(reads_no_payload(&plan), "{plan}");

        // q, step two: id prefix / queue name on the row's own columns, and
        // the jobs of the matching types through the index.
        let search = "SELECT * FROM ((SELECT * FROM jobs WHERE organization_id = $1 \
             AND (id ILIKE $2 OR queue_name ILIKE $3) ORDER BY created_at DESC, id DESC LIMIT 51) \
             UNION ALL (SELECT j.* FROM UNNEST($4::TEXT[]) AS t(job_type) CROSS JOIN LATERAL ( \
             SELECT * FROM jobs WHERE organization_id = $1 AND payload->>'job_type' = t.job_type \
             AND octet_length(payload->>'job_type') <= 255 AND NOT (id ILIKE $2 OR queue_name ILIKE $3) \
             ORDER BY created_at DESC, id DESC LIMIT 51) AS j)) AS matched \
             ORDER BY created_at DESC, id DESC LIMIT 51 OFFSET 0";
        for types in [
            "{rare_export}",
            "{type_1,type_10,type_11,type_12,type_13,type_14,type_15,type_16,type_17,type_18,type_19}",
        ] {
            let plan = explain(pool, search, &[&t.org_id, "type\\_1%", "%type\\_1%", types]).await;
            assert!(plan.contains("idx_jobs_org_job_type"), "{types}\n{plan}");
            assert!(reads_no_payload(&plan), "{types}\n{plan}");
        }
    }

    // W8-R2: an interrupted concurrent build of idx_jobs_org_job_type is
    // repaired.
    #[tokio::test]
    async fn w8_job_type_index_repair_rebuilds_an_invalid_index() {
        let db = TestDatabase::new().await;
        let index = "idx_jobs_org_job_type";
        let valid = |pool: sqlx::PgPool| async move {
            sqlx::query_scalar::<_, bool>(
                "SELECT indisvalid FROM pg_index WHERE indexrelid = to_regclass($1)",
            )
            .bind(index)
            .fetch_one(&pool)
            .await
            .unwrap()
        };
        assert!(valid(db.pool().clone()).await);
        sqlx::query("UPDATE pg_index SET indisvalid = FALSE WHERE indexrelid = to_regclass($1)")
            .bind(index)
            .execute(db.pool())
            .await
            .unwrap();
        assert!(!valid(db.pool().clone()).await);
        let mut tx = db.pool().begin().await.unwrap();
        sqlx::raw_sql(include_str!(
            "../migrations/20261014120001_job_type_index_repair.sql"
        ))
        .execute(&mut *tx)
        .await
        .unwrap();
        tx.commit().await.unwrap();
        assert!(valid(db.pool().clone()).await);
    }

    // W8-R2: the bounded search keeps q's results: job types matched as
    // substrings, together with `job_type`, under any order, and within the
    // organization's first MAX_SEARCH_JOB_TYPES distinct types. A job type
    // too long for the index is stored and listed, just not searched.
    #[tokio::test]
    async fn w8_job_search_matches_job_types_through_the_index() {
        let db = TestDatabase::new().await;
        let t = seed_tenant(db.pool()).await;
        let app = app(&db);
        let auth = bearer(&t.key);
        let pool = db.pool();
        let base = Utc::now() - Duration::hours(10);
        for (i, (id, queue, status, job_type)) in [
            ("s-mail-1", "reports", "failed", "send_Email"),
            ("s-mail-2", "mailers", "deadletter", "send_email"),
            ("s-charge", "billing", "deadletter", "charge"),
            ("s-render", "reports", "completed", "render"),
        ]
        .into_iter()
        .enumerate()
        {
            insert_job(
                pool,
                &t.org_id,
                id,
                queue,
                status,
                json!({"job_type": job_type}),
                base + Duration::minutes(i as i64),
            )
            .await;
        }
        let list = |query: &str| {
            let (app, auth) = (app.clone(), auth.clone());
            let uri = format!("/api/v1/jobs{query}");
            async move { get_paged(&app, &auth, &uri).await }
        };

        // A job type substring, and a queue name match, each listed once.
        let (status, body, has_more) = list("?q=MAIL").await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(ids(&body), vec!["s-mail-2", "s-mail-1"]);
        assert_eq!(has_more.as_deref(), Some("false"));
        // With the exact filter, only that type (or an id / queue match of it).
        let (_, body, _) = list("?q=mail&job_type=send_email").await;
        assert_eq!(ids(&body), vec!["s-mail-2"]);
        let (_, body, _) = list("?q=s-&job_type=charge").await;
        assert_eq!(ids(&body), vec!["s-charge"]);
        let (_, body, _) = list("?q=render&job_type=charge").await;
        assert_eq!(ids(&body), Vec::<String>::new());
        // Another order, and paging across both parts of the search.
        let (_, body, _) = list("?q=e&order_by=queue_name&order_dir=asc").await;
        assert_eq!(
            ids(&body),
            vec!["s-charge", "s-mail-2", "s-mail-1", "s-render"]
        );
        let (_, body, has_more) =
            list("?q=e&order_by=queue_name&order_dir=asc&limit=2&offset=1").await;
        assert_eq!(ids(&body), vec!["s-mail-2", "s-mail-1"]);
        assert_eq!(has_more.as_deref(), Some("true"));
        // The DLQ list searches the same way.
        let (status, body, _) = get_paged(&app, &auth, "/api/v1/jobs/dlq?q=EMAIL").await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(ids(&body), vec!["s-mail-2"]);

        // A job type longer than the index takes is stored and listed.
        let long_type = "L".repeat(3000);
        insert_job(
            pool,
            &t.org_id,
            "s-long",
            "reports",
            "completed",
            json!({"job_type": long_type}),
            base + Duration::minutes(30),
        )
        .await;
        let (status, body, _) = list("?q=s-long").await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(ids(&body), vec!["s-long"]);
        let (_, body, _) = list("?q=LLLL").await;
        assert_eq!(ids(&body), Vec::<String>::new());

        // The first MAX_SEARCH_JOB_TYPES distinct types (in sort order) are
        // searched; a type sorting after them is not.
        let cap = spooled_backend::api::handlers::jobs::MAX_SEARCH_JOB_TYPES;
        sqlx::query(
            "INSERT INTO jobs (id, organization_id, queue_name, status, payload, created_at, updated_at) \
             SELECT 'cap-' || g, $1, 'capq', 'completed', \
                    jsonb_build_object('job_type', 'a-cap-' || lpad(g::TEXT, 5, '0')), \
                    NOW() - INTERVAL '1 day', NOW() \
             FROM generate_series(1, $2::INT) AS g",
        )
        .bind(&t.org_id)
        .bind(cap as i32)
        .execute(pool)
        .await
        .unwrap();
        let (_, body, _) = list(&format!("?q=a-cap-{cap:05}")).await;
        assert_eq!(ids(&body), vec![format!("cap-{cap}")]);
        // `render` now sorts after a thousand `a-cap-*` types and the 4 above.
        let (_, body, _) = list("?q=ende").await;
        assert_eq!(ids(&body), Vec::<String>::new());
        // The exact filter is not bounded.
        let (_, body, _) = list("?job_type=render").await;
        assert_eq!(ids(&body), vec!["s-render"]);
    }

    // W8-R2: GET /jobs and /jobs/dlq run under a per-statement timeout. A list
    // the database cannot answer in time (here: waiting on a lock) is stopped
    // by Postgres and answered 400 QUERY_TIMEOUT, without SQL text, and the
    // timeout does not stay on the pooled connection.
    #[tokio::test]
    async fn w8_job_list_statement_timeout_is_a_clean_400() {
        let db = TestDatabase::new().await;
        let t = seed_tenant(db.pool()).await;
        let mut settings = Settings::load_for_testing();
        settings.database.list_statement_timeout_ms = 300;
        let app = app_with(&db, settings);
        let auth = bearer(&t.key);
        let pool = db.pool();
        insert_job(
            pool,
            &t.org_id,
            "slow-1",
            "reports",
            "deadletter",
            json!({"job_type": "render"}),
            Utc::now(),
        )
        .await;

        // Another session holds the jobs table; every list read waits on it.
        let mut blocker = pool.begin().await.unwrap();
        sqlx::query("LOCK TABLE jobs IN ACCESS EXCLUSIVE MODE")
            .execute(&mut *blocker)
            .await
            .unwrap();
        for uri in [
            "/api/v1/jobs",
            "/api/v1/jobs?q=slow",
            "/api/v1/jobs?job_type=render&status=deadletter",
            "/api/v1/jobs/dlq?q=render",
        ] {
            let started = std::time::Instant::now();
            let (status, body, _) = get_paged(&app, &auth, uri).await;
            let took = started.elapsed();
            assert_eq!(status, StatusCode::BAD_REQUEST, "{uri}: {body}");
            assert_eq!(body["code"], "QUERY_TIMEOUT", "{uri}: {body}");
            let message = body["message"].as_str().unwrap();
            assert!(message.contains("300 ms"), "{message}");
            for leak in ["SELECT", "statement", "jobs WHERE", "canceling"] {
                assert!(!message.contains(leak), "{uri}: {message}");
            }
            assert!(
                took < std::time::Duration::from_secs(5),
                "{uri} took {took:?}"
            );
        }
        blocker.rollback().await.unwrap();

        // The timeout was LOCAL to each list's transaction: the lists answer
        // again, and no pooled connection kept it.
        let (status, body, _) = get_paged(&app, &auth, "/api/v1/jobs?q=slow").await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(ids(&body), vec!["slow-1"]);
        let mut held = Vec::new();
        for _ in 0..pool.num_idle() {
            let mut conn = pool.acquire().await.unwrap();
            let timeout: String = sqlx::query_scalar("SHOW statement_timeout")
                .fetch_one(&mut *conn)
                .await
                .unwrap();
            assert_eq!(timeout, "0", "a pooled connection kept the list timeout");
            held.push(conn);
        }
    }

    // QW15: workers.jobs_processed / jobs_failed count what the worker reported
    // on any path (REST, or the raw SQL the gRPC service runs), and never a
    // sweep release, a release that keeps retry_count or another org's job.
    #[tokio::test]
    async fn w8_worker_counters_count_reported_outcomes_only() {
        let db = TestDatabase::new().await;
        let t = seed_tenant(db.pool()).await;
        let other = seed_tenant(db.pool()).await;
        let app = app(&db);
        let auth = bearer(&t.key);
        let pool = db.pool();

        let worker = register_worker(&app, &auth, "w8-cnt").await;
        assert_eq!(worker_counters(&app, &auth, &worker).await, (0, 0));

        // REST complete.
        let job = create_job(&app, &auth, "w8-cnt-a", 3).await;
        let lease = claim(&app, &auth, "w8-cnt-a", &worker, &job).await;
        let (status, body) = post(
            &app,
            &auth,
            &format!("/api/v1/jobs/{job}/complete"),
            json!({"worker_id": worker, "lease_id": lease}),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(worker_counters(&app, &auth, &worker).await, (1, 0));

        // REST fail with retries left: a retry is scheduled, one failure.
        let job = create_job(&app, &auth, "w8-cnt-b", 3).await;
        let lease = claim(&app, &auth, "w8-cnt-b", &worker, &job).await;
        let (status, body) = post(
            &app,
            &auth,
            &format!("/api/v1/jobs/{job}/fail"),
            json!({"worker_id": worker, "lease_id": lease, "error": "boom"}),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(job_status(pool, &job).await, "pending");
        assert_eq!(worker_counters(&app, &auth, &worker).await, (1, 1));

        // REST fail without retries: dead-lettered, one more failure.
        let job = create_job(&app, &auth, "w8-cnt-c", 0).await;
        let lease = claim(&app, &auth, "w8-cnt-c", &worker, &job).await;
        let (status, body) = post(
            &app,
            &auth,
            &format!("/api/v1/jobs/{job}/fail"),
            json!({"worker_id": worker, "lease_id": lease, "error": "fatal"}),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(job_status(pool, &job).await, "deadletter");
        assert_eq!(worker_counters(&app, &auth, &worker).await, (1, 2));

        // The gRPC fail statement (its shape in queue_service.rs, retry=false):
        // counted although no REST code ran.
        let job = create_job(&app, &auth, "w8-cnt-d", 3).await;
        let lease = claim(&app, &auth, "w8-cnt-d", &worker, &job).await;
        let moved = sqlx::query(
            "UPDATE jobs SET \
                 status = CASE WHEN $1 = FALSE OR retry_count >= max_retries THEN 'deadletter' ELSE 'pending' END, \
                 last_error = 'grpc', assigned_worker_id = NULL, lease_id = NULL, \
                 lease_expires_at = NULL, updated_at = NOW() \
             WHERE id = $2 AND assigned_worker_id = $3 AND organization_id = $4 \
               AND status = 'processing' AND lease_expires_at > NOW() AND lease_id = $5",
        )
        .bind(false)
        .bind(&job)
        .bind(&worker)
        .bind(&t.org_id)
        .bind(&lease)
        .execute(pool)
        .await
        .unwrap();
        assert_eq!(moved.rows_affected(), 1);
        assert_eq!(worker_counters(&app, &auth, &worker).await, (1, 3));

        // The lease-expiry sweep retries an abandoned job: not the worker's failure.
        let job = create_job(&app, &auth, "w8-cnt-e", 3).await;
        claim(&app, &auth, "w8-cnt-e", &worker, &job).await;
        sqlx::query("UPDATE jobs SET lease_expires_at = NOW() - INTERVAL '1 second' WHERE id = $1")
            .bind(&job)
            .execute(pool)
            .await
            .unwrap();
        let scheduler = spooled_backend::scheduler::Scheduler::new(
            Arc::clone(&db.pool),
            None,
            Arc::new(Metrics::new()),
            Arc::new(
                spooled_backend::outgoing_webhooks::service::OutgoingWebhookService::new(
                    (*db.pool).clone(),
                    3,
                    4,
                ),
            ),
        );
        scheduler.recover_expired_leases().await.unwrap();
        let (status, retries): (String, i32) =
            sqlx::query_as("SELECT status, retry_count FROM jobs WHERE id = $1")
                .bind(&job)
                .fetch_one(pool)
                .await
                .unwrap();
        assert_eq!((status.as_str(), retries), ("pending", 1), "the sweep ran");
        assert_eq!(worker_counters(&app, &auth, &worker).await, (1, 3));

        // Another org's job naming this worker id does not touch its counters.
        insert_job(
            pool,
            &other.org_id,
            "w8-foreign-claim",
            "w8-cnt",
            "pending",
            json!({}),
            Utc::now(),
        )
        .await;
        sqlx::query(
            "UPDATE jobs SET status = 'processing', assigned_worker_id = $2, lease_id = 'l', \
                 lease_expires_at = NOW() + INTERVAL '1 minute' WHERE id = $1",
        )
        .bind("w8-foreign-claim")
        .bind(&worker)
        .execute(pool)
        .await
        .unwrap();
        sqlx::query("UPDATE jobs SET status = 'completed', completed_at = NOW() WHERE id = $1")
            .bind("w8-foreign-claim")
            .execute(pool)
            .await
            .unwrap();
        assert_eq!(worker_counters(&app, &auth, &worker).await, (1, 3));

        // Deregistering releases a held job without a retry: nothing counted.
        let job = create_job(&app, &auth, "w8-cnt-f", 3).await;
        claim(&app, &auth, "w8-cnt-f", &worker, &job).await;
        let (status, body) = post(
            &app,
            &auth,
            &format!("/api/v1/workers/{worker}/deregister"),
            json!({}),
        )
        .await;
        assert!(status.is_success(), "{body}");
        assert_eq!(job_status(pool, &job).await, "pending");
        assert_eq!(worker_counters(&app, &auth, &worker).await, (1, 3));

        // The list carries the counters too.
        let (status, body) = call(
            &app,
            "GET",
            "/api/v1/workers",
            &[("Authorization", &auth)],
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let row = body
            .as_array()
            .unwrap()
            .iter()
            .find(|w| w["id"] == json!(worker))
            .expect("worker listed");
        assert_eq!(row["jobs_processed"], 1, "{row}");
        assert_eq!(row["jobs_failed"], 3, "{row}");
    }

    // QW15: the counter migration is idempotent: a re-run keeps the counts and
    // leaves one trigger.
    #[tokio::test]
    async fn w8_worker_counter_migration_reruns_cleanly() {
        let db = TestDatabase::new().await;
        let t = seed_tenant(db.pool()).await;
        let app = app(&db);
        let auth = bearer(&t.key);
        let worker = register_worker(&app, &auth, "w8-rerun").await;
        sqlx::query("UPDATE workers SET jobs_processed = 7, jobs_failed = 2 WHERE id = $1")
            .bind(&worker)
            .execute(db.pool())
            .await
            .unwrap();

        let mut tx = db.pool().begin().await.unwrap();
        sqlx::raw_sql(include_str!(
            "../migrations/20261014110000_worker_job_counters.sql"
        ))
        .execute(&mut *tx)
        .await
        .unwrap();
        tx.commit().await.unwrap();

        assert_eq!(worker_counters(&app, &auth, &worker).await, (7, 2));
        // One trigger, still a constraint trigger deferred to commit.
        let triggers: Vec<(bool, bool, bool)> = sqlx::query_as(
            "SELECT tgconstraint <> 0, tgdeferrable, tginitdeferred FROM pg_trigger \
             WHERE tgrelid = 'jobs'::regclass AND tgname = 'trigger_count_worker_job_outcome'",
        )
        .fetch_all(db.pool())
        .await
        .unwrap();
        assert_eq!(triggers, vec![(true, true, true)]);
    }

    // QW15 review: the counter trigger is deferred to commit. As an ordinary
    // AFTER trigger it ran before trigger_update_dependent_jobs (name order),
    // so completing a job J locked J, then the worker row, then J's
    // dependents. One worker completing J and an 'any'-mode dependent D it was
    // already running at the same instant deadlocked: J's transaction held
    // the worker row and waited for D, D's held D and waited for the worker
    // row. Both complete now, and both are counted.
    #[tokio::test]
    async fn w8_worker_counter_does_not_deadlock_concurrent_any_mode_completions() {
        let db = TestDatabase::new().await;
        let t = seed_tenant(db.pool()).await;
        let app = app(&db);
        let auth = bearer(&t.key);
        let pool = db.pool();
        let worker = register_worker(&app, &auth, "w8-dl").await;

        // K done; J running; D ('any' of J and K) already running; one worker.
        for (id, status, mode, lease) in [
            ("w8-dl-k", "completed", "all", None),
            ("w8-dl-j", "processing", "all", Some("lease-j")),
            ("w8-dl-d", "processing", "any", Some("lease-d")),
        ] {
            sqlx::query(
                "INSERT INTO jobs (id, organization_id, queue_name, status, payload, \
                     assigned_worker_id, lease_id, lease_expires_at, dependency_mode, \
                     dependencies_met, started_at, created_at, updated_at) \
                 VALUES ($1, $2, 'w8-dl', $3, '{}'::JSONB, $4, $5, \
                     CASE WHEN $5::TEXT IS NULL THEN NULL ELSE NOW() + INTERVAL '5 minutes' END, \
                     $6, TRUE, NOW(), NOW(), NOW())",
            )
            .bind(id)
            .bind(&t.org_id)
            .bind(status)
            .bind(lease.map(|_| worker.clone()))
            .bind(lease)
            .bind(mode)
            .execute(pool)
            .await
            .unwrap();
        }
        sqlx::query(
            "INSERT INTO job_dependencies (job_id, depends_on_job_id) \
             VALUES ('w8-dl-d', 'w8-dl-j'), ('w8-dl-d', 'w8-dl-k')",
        )
        .execute(pool)
        .await
        .unwrap();

        // D's settling transaction holds D ...
        let mut d_tx = pool.begin().await.unwrap();
        sqlx::query("SELECT 1 FROM jobs WHERE id = 'w8-dl-d' FOR UPDATE")
            .execute(&mut *d_tx)
            .await
            .unwrap();

        // ... while J completes over REST, on a second connection: the
        // dependents trigger waits for D.
        let j_done = tokio::spawn({
            let (app, auth, worker) = (app.clone(), auth.clone(), worker.clone());
            async move {
                post(
                    &app,
                    &auth,
                    "/api/v1/jobs/w8-dl-j/complete",
                    json!({"worker_id": worker, "lease_id": "lease-j"}),
                )
                .await
            }
        });
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            let waiting: i64 = sqlx::query_scalar(
                "SELECT COUNT(*) FROM pg_stat_activity \
                 WHERE datname = current_database() AND wait_event_type = 'Lock'",
            )
            .fetch_one(pool)
            .await
            .unwrap();
            if waiting > 0 {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "J's completion never waited for D"
            );
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }

        // D completes in its transaction (the worker complete statement).
        let moved = sqlx::query(
            "UPDATE jobs SET status = 'completed', completed_at = NOW(), updated_at = NOW() \
             WHERE id = 'w8-dl-d' AND assigned_worker_id = $1 AND status = 'processing' \
               AND lease_expires_at > NOW() AND lease_id = 'lease-d'",
        )
        .bind(&worker)
        .execute(&mut *d_tx)
        .await
        .expect("D completes without a deadlock");
        assert_eq!(moved.rows_affected(), 1);
        d_tx.commit().await.expect("D commits");

        let (status, body) = tokio::time::timeout(std::time::Duration::from_secs(10), j_done)
            .await
            .expect("J's completion returns")
            .unwrap();
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(job_status(pool, "w8-dl-j").await, "completed");
        assert_eq!(job_status(pool, "w8-dl-d").await, "completed");
        assert_eq!(worker_counters(&app, &auth, &worker).await, (2, 0));
    }
}
// ---- wave 8 / events ----
mod w8_events {
    use super::*;

    use std::sync::Mutex;
    use std::time::{Duration, Instant};

    use axum::http::HeaderMap;
    use spooled_backend::outgoing_webhooks::events::{
        announce_workflow_outcomes, ANNOUNCE_WORKFLOW_OUTCOMES_SQL, WORKFLOW_OUTCOME_BATCH,
    };
    use spooled_backend::outgoing_webhooks::service::OutgoingWebhookService;
    use spooled_backend::scheduler::Scheduler;

    /// Requests a local receiver got: (headers, body).
    #[derive(Clone, Default)]
    struct Received(Arc<Mutex<Vec<(HeaderMap, String)>>>);

    impl Received {
        fn all(&self) -> Vec<(HeaderMap, String)> {
            self.0.lock().unwrap().clone()
        }
    }

    /// A webhook receiver answering 200 to everything, and its URL.
    async fn receiver() -> (String, Received) {
        let received = Received::default();
        let shared = received.clone();
        let app = Router::new().fallback(move |headers: HeaderMap, body: String| {
            let shared = shared.clone();
            async move {
                shared.0.lock().unwrap().push((headers, body));
                StatusCode::OK
            }
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (format!("http://{addr}/hook"), received)
    }

    async fn insert_hook(pool: &sqlx::PgPool, org_id: &str, url: &str, events: &[&str]) -> String {
        let events: Vec<String> = events.iter().map(|e| e.to_string()).collect();
        sqlx::query_scalar(
            "INSERT INTO outgoing_webhooks (organization_id, name, url, events) \
             VALUES ($1, 'w8', $2, $3) RETURNING id",
        )
        .bind(org_id)
        .bind(url)
        .bind(&events)
        .fetch_one(pool)
        .await
        .unwrap()
    }

    /// Delivery rows of an org's webhooks: (event, data), oldest first.
    async fn deliveries(pool: &sqlx::PgPool, org_id: &str) -> Vec<(String, Value)> {
        sqlx::query_as(
            "SELECT d.event, d.payload->'data' FROM outgoing_webhook_deliveries d \
             JOIN outgoing_webhooks w ON w.id = d.webhook_id \
             WHERE w.organization_id = $1 ORDER BY d.created_at, d.event",
        )
        .bind(org_id)
        .fetch_all(pool)
        .await
        .unwrap()
    }

    /// Wait until the org has `count` delivery rows, then a little longer to
    /// catch any extra one, and return them.
    async fn settle(pool: &sqlx::PgPool, org_id: &str, count: usize) -> Vec<(String, Value)> {
        let deadline = Instant::now() + Duration::from_secs(10);
        while deliveries(pool, org_id).await.len() < count && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        tokio::time::sleep(Duration::from_millis(400)).await;
        deliveries(pool, org_id).await
    }

    fn events_of(rows: &[(String, Value)]) -> Vec<&str> {
        let mut events: Vec<&str> = rows.iter().map(|(e, _)| e.as_str()).collect();
        events.sort_unstable();
        events
    }

    fn service(db: &TestDatabase) -> Arc<OutgoingWebhookService> {
        Arc::new(OutgoingWebhookService::new((*db.pool).clone(), 3, 4))
    }

    fn scheduler(db: &TestDatabase) -> Scheduler {
        Scheduler::new(
            Arc::clone(&db.pool),
            None,
            Arc::new(Metrics::new()),
            service(db),
        )
    }

    async fn scoped_key(pool: &sqlx::PgPool, org_id: &str) -> String {
        let key = format!("sp_test_{}", uuid::Uuid::new_v4().simple());
        sqlx::query(
            "INSERT INTO api_keys (id, organization_id, key_hash, key_prefix, lookup_hash, name, queues, is_active, created_at) \
             VALUES ($1, $2, $3, $4, $5, 'w8-scoped', ARRAY['w8-in']::TEXT[], TRUE, NOW())",
        )
        .bind(uuid::Uuid::new_v4().to_string())
        .bind(org_id)
        .bind(bcrypt::hash(&key, 4).unwrap())
        .bind(key.chars().take(8).collect::<String>())
        .bind(spooled_backend::models::api_key_lookup_hash(&key))
        .execute(pool)
        .await
        .unwrap();
        key
    }

    // QW8: the four new event types can be subscribed to, through the API and
    // in the table; an unknown event is still refused by both.
    #[tokio::test]
    async fn w8_new_event_types_are_subscribable() {
        let db = TestDatabase::new().await;
        let t = seed_tenant(db.pool()).await;
        let app = app(&db);
        let auth = bearer(&t.key);
        let new_events = json!([
            "job.deadlettered",
            "job.retried",
            "workflow.completed",
            "workflow.failed"
        ]);

        let (status, body) = call(
            &app,
            "POST",
            "/api/v1/outgoing-webhooks",
            &[("Authorization", &auth)],
            Some(json!({"name": "w8", "url": "https://example.com/w8", "events": new_events})),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        assert_eq!(body["events"], new_events);
        let id = body["id"].as_str().unwrap().to_string();

        let (status, body) = call(
            &app,
            "PUT",
            &format!("/api/v1/outgoing-webhooks/{id}"),
            &[("Authorization", &auth)],
            Some(json!({"events": ["job.retried", "job.failed"]})),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["events"], json!(["job.retried", "job.failed"]));

        let (status, body) = call(
            &app,
            "POST",
            "/api/v1/outgoing-webhooks",
            &[("Authorization", &auth)],
            Some(json!({"name": "w8", "url": "https://example.com/w8", "events": ["workflow.cancelled"]})),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");

        let refused = sqlx::query(
            "INSERT INTO outgoing_webhooks (organization_id, name, url, events) \
             VALUES ($1, 'w8', 'https://example.com', ARRAY['job.deadlettered', 'nope'])",
        )
        .bind(&t.org_id)
        .execute(db.pool())
        .await
        .unwrap_err();
        assert!(
            refused
                .to_string()
                .contains("outgoing_webhook_events_check"),
            "{refused}"
        );
    }

    async fn seed_leased_job(
        pool: &sqlx::PgPool,
        org_id: &str,
        lease_expires_in_secs: i64,
        retry_count: i32,
    ) -> String {
        let id = uuid::Uuid::new_v4().to_string();
        sqlx::query(
            "INSERT INTO jobs (id, organization_id, queue_name, payload, status, assigned_worker_id, \
             lease_id, lease_expires_at, retry_count, max_retries, started_at, created_at, updated_at) \
             VALUES ($1, $2, 'w8q', '{}'::JSONB, 'processing', 'w8-worker', 'w8-lease', \
                     NOW() + make_interval(secs => $3), $4, 3, NOW(), NOW(), NOW())",
        )
        .bind(&id)
        .bind(org_id)
        .bind(lease_expires_in_secs as f64)
        .bind(retry_count)
        .execute(pool)
        .await
        .unwrap();
        id
    }

    // QW8: a job moved to the dead-letter queue sends job.deadlettered once,
    // next to the job.failed it already sent: from the REST fail path and from
    // the lease-expiry sweep. A failure that is retried sends neither.
    #[tokio::test]
    async fn w8_dead_lettering_sends_job_deadlettered_once() {
        let db = TestDatabase::new().await;
        let t = seed_tenant(db.pool()).await;
        let app = app(&db);
        let auth = bearer(&t.key);
        let (url, received) = receiver().await;
        insert_hook(
            db.pool(),
            &t.org_id,
            &url,
            &["job.failed", "job.deadlettered"],
        )
        .await;

        // A failure with retries left: pending again, no event.
        let retried = seed_leased_job(db.pool(), &t.org_id, 600, 0).await;
        let (status, body) = call(
            &app,
            "POST",
            &format!("/api/v1/jobs/{retried}/fail"),
            &[("Authorization", &auth)],
            Some(json!({"worker_id": "w8-worker", "lease_id": "w8-lease", "error": "flaky"})),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");

        // The last attempt: dead-lettered.
        let dead = seed_leased_job(db.pool(), &t.org_id, 600, 3).await;
        let (status, body) = call(
            &app,
            "POST",
            &format!("/api/v1/jobs/{dead}/fail"),
            &[("Authorization", &auth)],
            Some(json!({"worker_id": "w8-worker", "lease_id": "w8-lease", "error": "boom"})),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");

        let rows = settle(db.pool(), &t.org_id, 2).await;
        assert_eq!(
            events_of(&rows),
            ["job.deadlettered", "job.failed"],
            "{rows:?}"
        );
        let (_, data) = rows.iter().find(|(e, _)| e == "job.deadlettered").unwrap();
        assert_eq!(data["job_id"], dead.as_str());
        assert_eq!(data["queue_name"], "w8q");
        assert_eq!(data["status"], "deadletter");
        assert_eq!(data["error"], "boom");
        assert!(rows.iter().all(|(_, d)| d["job_id"] != retried.as_str()));

        // The lease-expiry sweep dead-letters an abandoned last attempt.
        let abandoned = seed_leased_job(db.pool(), &t.org_id, -5, 3).await;
        scheduler(&db).recover_expired_leases().await.unwrap();
        let rows = settle(db.pool(), &t.org_id, 4).await;
        let for_abandoned: Vec<&str> = {
            let mut v: Vec<&str> = rows
                .iter()
                .filter(|(_, d)| d["job_id"] == abandoned.as_str())
                .map(|(e, _)| e.as_str())
                .collect();
            v.sort_unstable();
            v
        };
        assert_eq!(
            for_abandoned,
            ["job.deadlettered", "job.failed"],
            "{rows:?}"
        );

        // The receiver got the envelope for each, X-Spooled-Event set.
        let got: Vec<String> = received
            .all()
            .iter()
            .map(|(h, _)| h["x-spooled-event"].to_str().unwrap().to_string())
            .collect();
        assert_eq!(
            got.iter().filter(|e| *e == "job.deadlettered").count(),
            2,
            "{got:?}"
        );
    }

    // Wave 8 review: REST fail takes the status of job.failed (and of the
    // job.deadlettered it implies) from the transition, not from a read after
    // the commit. Here the dead-lettered job is put back to `pending` before
    // the handler could read it (a trigger standing in for an immediate DLQ
    // retry); the read said `pending` and no event went out.
    #[tokio::test]
    async fn w8_rest_fail_event_status_comes_from_the_transition() {
        let db = TestDatabase::new().await;
        let t = seed_tenant(db.pool()).await;
        let app = app(&db);
        let auth = bearer(&t.key);
        let (url, _received) = receiver().await;
        insert_hook(
            db.pool(),
            &t.org_id,
            &url,
            &["job.failed", "job.deadlettered"],
        )
        .await;
        sqlx::raw_sql(
            "CREATE FUNCTION w8_test_retry_dead_letter() RETURNS TRIGGER AS $$ \
             BEGIN \
                 UPDATE jobs SET status = 'pending' WHERE id = NEW.id; \
                 RETURN NULL; \
             END; $$ LANGUAGE plpgsql; \
             CREATE TRIGGER w8_test_retry_dead_letter AFTER UPDATE OF status ON jobs \
                 FOR EACH ROW WHEN (NEW.status = 'deadletter') \
                 EXECUTE FUNCTION w8_test_retry_dead_letter();",
        )
        .execute(db.pool())
        .await
        .unwrap();

        let dead = seed_leased_job(db.pool(), &t.org_id, 600, 3).await;
        let (status, body) = call(
            &app,
            "POST",
            &format!("/api/v1/jobs/{dead}/fail"),
            &[("Authorization", &auth)],
            Some(json!({"worker_id": "w8-worker", "lease_id": "w8-lease", "error": "boom"})),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let now: String = sqlx::query_scalar("SELECT status FROM jobs WHERE id = $1")
            .bind(&dead)
            .fetch_one(db.pool())
            .await
            .unwrap();
        assert_eq!(now, "pending", "the stand-in retry ran");

        let rows = settle(db.pool(), &t.org_id, 2).await;
        assert_eq!(
            events_of(&rows),
            ["job.deadlettered", "job.failed"],
            "{rows:?}"
        );
        for (event, data) in &rows {
            assert_eq!(data["job_id"], dead.as_str(), "{event}");
            assert_eq!(data["status"], "deadletter", "{event}");
            assert_eq!(data["queue_name"], "w8q", "{event}");
        }
    }

    // QW8: a workflow retry sends job.retried for every job it puts back,
    // with the status it left.
    #[tokio::test]
    async fn w8_workflow_retry_sends_job_retried_per_job() {
        let db = TestDatabase::new().await;
        let t = seed_tenant(db.pool()).await;
        let app = app(&db);
        let (url, _received) = receiver().await;
        insert_hook(db.pool(), &t.org_id, &url, &["job.retried"]).await;

        let wf = uuid::Uuid::new_v4().to_string();
        sqlx::query(
            "INSERT INTO workflows (id, organization_id, name, status, total_jobs, failed_jobs, completed_at) \
             VALUES ($1, $2, 'w8', 'failed', 3, 1, NOW())",
        )
        .bind(&wf)
        .bind(&t.org_id)
        .execute(db.pool())
        .await
        .unwrap();
        let (dead, cancelled, done) = (
            uuid::Uuid::new_v4().to_string(),
            uuid::Uuid::new_v4().to_string(),
            uuid::Uuid::new_v4().to_string(),
        );
        for (id, status) in [
            (&dead, "deadletter"),
            (&cancelled, "cancelled"),
            (&done, "completed"),
        ] {
            sqlx::query(
                "INSERT INTO jobs (id, organization_id, queue_name, payload, status, workflow_id, \
                 retry_count, max_retries, completed_at, created_at, updated_at) \
                 VALUES ($1, $2, 'w8q', '{}'::JSONB, $3, $4, 0, 3, NOW(), NOW(), NOW())",
            )
            .bind(id)
            .bind(&t.org_id)
            .bind(status)
            .bind(&wf)
            .execute(db.pool())
            .await
            .unwrap();
        }

        let (status, body) = call(
            &app,
            "POST",
            &format!("/api/v1/workflows/{wf}/retry"),
            &[("Authorization", &bearer(&t.key))],
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");

        let rows = settle(db.pool(), &t.org_id, 2).await;
        assert_eq!(events_of(&rows), ["job.retried", "job.retried"], "{rows:?}");
        let old_status = |job: &str| {
            rows.iter()
                .find(|(_, d)| d["job_id"] == job)
                .map(|(_, d)| d.clone())
                .unwrap_or_else(|| panic!("no job.retried for {job}: {rows:?}"))
        };
        let d = old_status(&dead);
        assert_eq!(d["old_status"], "deadletter");
        assert_eq!(d["status"], "pending");
        assert_eq!(d["source"], "workflow_retry");
        assert_eq!(d["workflow_id"], wf.as_str());
        assert_eq!(d["queue_name"], "w8q");
        assert_eq!(old_status(&cancelled)["old_status"], "cancelled");
        // The completed job was not retried.
        assert!(rows.iter().all(|(_, d)| d["job_id"] != done.as_str()));
    }

    async fn seed_workflow(
        pool: &sqlx::PgPool,
        org_id: &str,
        status: &str,
        ended_ago: Option<&str>,
    ) -> String {
        let id = uuid::Uuid::new_v4().to_string();
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "INSERT INTO workflows (id, organization_id, name, status, total_jobs, completed_at, metadata) \
             VALUES ($1, $2, 'w8 flow', $3, 1, {}, '{{\"team\": \"ops\"}}'::JSONB)",
            match ended_ago {
                Some(ago) => format!("NOW() - INTERVAL '{ago}'"),
                None => "NULL".to_string(),
            }
        )))
        .bind(&id)
        .bind(org_id)
        .bind(status)
        .execute(pool)
        .await
        .unwrap();
        id
    }

    async fn seed_workflow_job(
        pool: &sqlx::PgPool,
        org_id: &str,
        workflow: &str,
        status: &str,
    ) -> String {
        let id = uuid::Uuid::new_v4().to_string();
        sqlx::query(
            "INSERT INTO jobs (id, organization_id, queue_name, payload, status, workflow_id, \
             retry_count, max_retries, created_at, updated_at) \
             VALUES ($1, $2, 'w8q', '{}'::JSONB, $3, $4, 0, 3, NOW(), NOW())",
        )
        .bind(&id)
        .bind(org_id)
        .bind(status)
        .bind(workflow)
        .execute(pool)
        .await
        .unwrap();
        id
    }

    async fn set_job_status(pool: &sqlx::PgPool, job: &str, status: &str) {
        sqlx::query("UPDATE jobs SET status = $2, updated_at = NOW() WHERE id = $1")
            .bind(job)
            .bind(status)
            .execute(pool)
            .await
            .unwrap();
    }

    // QW8: a workflow that ends completed (progress trigger) or failed
    // (scheduler terminate sweep) is announced once; ended again after a
    // retry, it is announced again. Old or cancelled outcomes are not.
    #[tokio::test]
    async fn w8_workflow_outcomes_are_announced_once_per_transition() {
        let db = TestDatabase::new().await;
        let t = seed_tenant(db.pool()).await;
        let pool = db.pool();
        let (url, _received) = receiver().await;
        insert_hook(
            pool,
            &t.org_id,
            &url,
            &["workflow.completed", "workflow.failed"],
        )
        .await;
        let sched = scheduler(&db);
        let webhooks = service(&db);

        // Ends 'completed' in the progress trigger, when its job completes.
        let done = seed_workflow(pool, &t.org_id, "running", None).await;
        let done_job = seed_workflow_job(pool, &t.org_id, &done, "processing").await;
        set_job_status(pool, &done_job, "completed").await;
        // Ends 'failed' in the terminate sweep (a dead-lettered job does not
        // fire the trigger).
        let failed = seed_workflow(pool, &t.org_id, "running", None).await;
        let failed_job = seed_workflow_job(pool, &t.org_id, &failed, "deadletter").await;
        // Never announced: ended before the last hour, or cancelled.
        let old = seed_workflow(pool, &t.org_id, "completed", Some("2 hours")).await;
        let cancelled = seed_workflow(pool, &t.org_id, "cancelled", Some("1 minute")).await;

        sched.update_job_dependencies().await.unwrap();
        let status: String = sqlx::query_scalar("SELECT status FROM workflows WHERE id = $1")
            .bind(&failed)
            .fetch_one(pool)
            .await
            .unwrap();
        assert_eq!(status, "failed");
        // Everything was claimed by that pass.
        assert_eq!(
            announce_workflow_outcomes(pool, &webhooks).await.unwrap(),
            0
        );
        sched.update_job_dependencies().await.unwrap();

        let rows = settle(pool, &t.org_id, 2).await;
        assert_eq!(
            events_of(&rows),
            ["workflow.completed", "workflow.failed"],
            "{rows:?}"
        );
        let data = |event: &str| rows.iter().find(|(e, _)| e == event).unwrap().1.clone();
        let completed = data("workflow.completed");
        assert_eq!(completed["workflow_id"], done.as_str());
        assert_eq!(completed["status"], "completed");
        assert_eq!(completed["name"], "w8 flow");
        assert_eq!(completed["completed_jobs"], 1);
        assert_eq!(completed["metadata"]["team"], "ops");
        assert!(completed["completed_at"].is_string(), "{completed}");
        let failed_data = data("workflow.failed");
        assert_eq!(failed_data["workflow_id"], failed.as_str());
        assert_eq!(failed_data["failed_jobs"], 1);
        for skipped in [&old, &cancelled] {
            assert!(rows
                .iter()
                .all(|(_, d)| d["workflow_id"] != skipped.as_str()));
        }

        // Retried (as POST /workflows/{id}/retry does) and completed this
        // time: one more announcement, workflow.completed.
        sqlx::query("UPDATE workflows SET status = 'running', completed_at = NULL WHERE id = $1")
            .bind(&failed)
            .execute(pool)
            .await
            .unwrap();
        set_job_status(pool, &failed_job, "processing").await;
        set_job_status(pool, &failed_job, "completed").await;
        sched.update_job_dependencies().await.unwrap();
        let rows = settle(pool, &t.org_id, 3).await;
        let for_failed: Vec<&str> = rows
            .iter()
            .filter(|(_, d)| d["workflow_id"] == failed.as_str())
            .map(|(e, _)| e.as_str())
            .collect();
        assert_eq!(
            for_failed,
            ["workflow.failed", "workflow.completed"],
            "{rows:?}"
        );
        assert_eq!(rows.len(), 3, "{rows:?}");
    }

    // QW8: the announce sweep reads the partial index, not every finished
    // workflow.
    #[tokio::test]
    async fn w8_workflow_outcome_sweep_uses_its_index() {
        let db = TestDatabase::new().await;
        let t = seed_tenant(db.pool()).await;
        let pool = db.pool();
        sqlx::query(
            "INSERT INTO workflows (id, organization_id, name, status, total_jobs, completed_at, announced_completed_at) \
             SELECT 'w8-wf-' || g, $1, 'wf', CASE WHEN g % 2 = 0 THEN 'completed' ELSE 'failed' END, 1, \
                    NOW() - make_interval(mins => g), NOW() - make_interval(mins => g) \
             FROM generate_series(1, 5000) AS g",
        )
        .bind(&t.org_id)
        .execute(pool)
        .await
        .unwrap();
        seed_workflow(pool, &t.org_id, "completed", Some("1 minute")).await;
        sqlx::query("ANALYZE workflows")
            .execute(pool)
            .await
            .unwrap();

        let plan: Vec<String> = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
            "EXPLAIN {ANNOUNCE_WORKFLOW_OUTCOMES_SQL}"
        )))
        .bind(WORKFLOW_OUTCOME_BATCH)
        .fetch_all(pool)
        .await
        .unwrap();
        let plan = plan.join("\n");
        assert!(plan.contains("idx_workflows_unannounced_outcome"), "{plan}");
        assert!(!plan.contains("Seq Scan on workflows"), "{plan}");

        // And only the one unannounced outcome is claimed.
        let webhooks = service(&db);
        assert_eq!(
            announce_workflow_outcomes(pool, &webhooks).await.unwrap(),
            1
        );
    }

    // QW8: an interrupted concurrent build of the index is repaired.
    #[tokio::test]
    async fn w8_workflow_outcome_index_repair_rebuilds_an_invalid_index() {
        let db = TestDatabase::new().await;
        let index = "idx_workflows_unannounced_outcome";
        let valid = |pool: sqlx::PgPool| async move {
            sqlx::query_scalar::<_, bool>(
                "SELECT indisvalid FROM pg_index WHERE indexrelid = to_regclass($1)",
            )
            .bind(index)
            .fetch_one(&pool)
            .await
            .unwrap()
        };
        assert!(valid(db.pool().clone()).await);
        sqlx::query("UPDATE pg_index SET indisvalid = FALSE WHERE indexrelid = to_regclass($1)")
            .bind(index)
            .execute(db.pool())
            .await
            .unwrap();
        assert!(!valid(db.pool().clone()).await);
        let mut tx = db.pool().begin().await.unwrap();
        sqlx::raw_sql(include_str!(
            "../migrations/20261014100202_workflow_outcome_index_repair.sql"
        ))
        .execute(&mut *tx)
        .await
        .unwrap();
        tx.commit().await.unwrap();
        assert!(valid(db.pool().clone()).await);
    }

    fn verify_signature(secret: &str, headers: &HeaderMap, body: &str) -> bool {
        use hmac::{Hmac, KeyInit, Mac};
        let ts = headers["x-spooled-timestamp"].to_str().unwrap();
        let mut mac = Hmac::<sha2::Sha256>::new_from_slice(secret.as_bytes()).unwrap();
        mac.update(format!("{ts}.{body}").as_bytes());
        let expected = format!("sha256={}", hex::encode(mac.finalize().into_bytes()));
        headers["x-spooled-signature"].to_str().unwrap() == expected
    }

    async fn wait_for(received: &Received, count: usize) -> Vec<(HeaderMap, String)> {
        let deadline = Instant::now() + Duration::from_secs(10);
        while received.all().len() < count && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        received.all()
    }

    // QW14: `format` is stored and validated, and a chat-format webhook gets
    // the event as that service's message, signed like the envelope, which a
    // `json` webhook still gets unchanged.
    #[tokio::test]
    async fn w8_webhook_format_maps_the_body() {
        let db = TestDatabase::new().await;
        let t = seed_tenant(db.pool()).await;
        let app = app(&db);
        let auth = bearer(&t.key);
        let (chat_url, chat) = receiver().await;
        let (json_url, plain) = receiver().await;

        let create = |url: String, extra: Value| {
            let mut body = json!({"name": "w8", "url": url, "events": ["job.created"]});
            for (k, v) in extra.as_object().unwrap() {
                body[k] = v.clone();
            }
            body
        };
        let (status, body) = call(
            &app,
            "POST",
            "/api/v1/outgoing-webhooks",
            &[("Authorization", &auth)],
            Some(create(chat_url.clone(), json!({"format": "xml"}))),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert!(body.to_string().contains("slack"), "{body}");

        let (status, slack) = call(
            &app,
            "POST",
            "/api/v1/outgoing-webhooks",
            &[("Authorization", &auth)],
            Some(create(
                chat_url,
                json!({"format": "Slack", "secret": "chat-secret"}),
            )),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{slack}");
        assert_eq!(slack["format"], "slack");
        let (status, plain_hook) = call(
            &app,
            "POST",
            "/api/v1/outgoing-webhooks",
            &[("Authorization", &auth)],
            Some(create(json_url, json!({"secret": "json-secret"}))),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{plain_hook}");
        assert_eq!(plain_hook["format"], "json");

        let (status, job) = call(
            &app,
            "POST",
            "/api/v1/jobs",
            &[("Authorization", &auth)],
            Some(json!({"queue_name": "w8-format", "payload": {"n": 1}})),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{job}");

        let got = wait_for(&chat, 1).await;
        assert_eq!(got.len(), 1, "{got:?}");
        let (headers, body) = &got[0];
        let message: Value = serde_json::from_str(body).unwrap();
        assert_eq!(message.as_object().unwrap().len(), 1, "{message}");
        let text = message["text"].as_str().unwrap();
        assert!(text.contains("Spooled: job created"), "{text}");
        assert!(text.contains("Queue: w8-format"), "{text}");
        assert_eq!(headers["x-spooled-event"], "job.created");
        assert!(verify_signature("chat-secret", headers, body));

        let got = wait_for(&plain, 1).await;
        let (headers, body) = &got[0];
        let envelope: Value = serde_json::from_str(body).unwrap();
        assert_eq!(envelope["event"], "job.created");
        assert_eq!(envelope["data"]["queue_name"], "w8-format");
        assert!(envelope["id"].is_string());
        assert!(verify_signature("json-secret", headers, body));

        // The delivery log keeps the envelope whatever the format.
        let logged: Vec<Value> = sqlx::query_scalar(
            "SELECT d.payload FROM outgoing_webhook_deliveries d WHERE d.webhook_id = $1",
        )
        .bind(slack["id"].as_str().unwrap())
        .fetch_all(db.pool())
        .await
        .unwrap();
        assert_eq!(logged.len(), 1);
        assert_eq!(logged[0]["event"], "job.created");

        // Switched to Discord; the test endpoint sends a Discord message.
        let hook = slack["id"].as_str().unwrap();
        let (status, body) = call(
            &app,
            "PUT",
            &format!("/api/v1/outgoing-webhooks/{hook}"),
            &[("Authorization", &auth)],
            Some(json!({"format": "discord"})),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["format"], "discord");
        let (status, body) = call(
            &app,
            "PUT",
            &format!("/api/v1/outgoing-webhooks/{hook}"),
            &[("Authorization", &auth)],
            Some(json!({"format": "pager"})),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");

        let (status, body) = call(
            &app,
            "POST",
            &format!("/api/v1/outgoing-webhooks/{hook}/test"),
            &[("Authorization", &auth)],
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["success"], true, "{body}");
        let got = wait_for(&chat, 2).await;
        let message: Value = serde_json::from_str(&got[1].1).unwrap();
        assert!(
            message["content"]
                .as_str()
                .unwrap()
                .contains("Spooled: test webhook"),
            "{message}"
        );
        assert_eq!(message["allowed_mentions"], json!({"parse": []}));

        let (_, list) = call(
            &app,
            "GET",
            "/api/v1/outgoing-webhooks",
            &[("Authorization", &auth)],
            None,
        )
        .await;
        let formats: Vec<&str> = list
            .as_array()
            .unwrap()
            .iter()
            .map(|w| w["format"].as_str().unwrap())
            .collect();
        assert!(
            formats.contains(&"discord") && formats.contains(&"json"),
            "{list}"
        );
    }

    // QW11: GET /webhooks/deliveries lists the org's accepted inbound
    // requests, newest first, with the job each resolved to; filters and
    // paging work, bad filters are a 400, and nothing stored that could be a
    // secret comes back.
    #[tokio::test]
    async fn w8_inbound_webhook_log_lists_requests() {
        let db = TestDatabase::new().await;
        let t = seed_tenant(db.pool()).await;
        let other = seed_tenant(db.pool()).await;
        let app = app(&db);
        let auth = bearer(&t.key);
        let token = "whk_w8_inbound_secret";
        sqlx::query(
            "UPDATE organizations SET settings = settings || jsonb_build_object('webhook_token', $2::TEXT) \
             WHERE id = $1",
        )
        .bind(&t.org_id)
        .bind(token)
        .execute(db.pool())
        .await
        .unwrap();

        let ingest = format!("/api/v1/webhooks/{}/custom", t.org_id);
        let mut job_ids = Vec::new();
        for body in [
            json!({"queue_name": "w8-in", "event_type": "order.paid", "payload": {"card": "4242"}, "idempotency_key": "k1"}),
            json!({"queue_name": "w8-in", "event_type": "order.paid", "payload": {"card": "4242"}, "idempotency_key": "k1"}),
            json!({"queue_name": "w8-other", "payload": {"n": 2}}),
        ] {
            let (status, res) = call(
                &app,
                "POST",
                &ingest,
                &[("X-Webhook-Token", token)],
                Some(body),
            )
            .await;
            assert_eq!(status, StatusCode::OK, "{res}");
            job_ids.push(res["job_id"].as_str().unwrap().to_string());
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert_eq!(job_ids[0], job_ids[1], "the replay matched the first job");

        let list = |query: &'static str| {
            let app = app.clone();
            let auth = auth.clone();
            async move {
                call(
                    &app,
                    "GET",
                    &format!("/api/v1/webhooks/deliveries{query}"),
                    &[("Authorization", &auth)],
                    None,
                )
                .await
            }
        };

        let (status, all) = list("").await;
        assert_eq!(status, StatusCode::OK, "{all}");
        let entries = all.as_array().unwrap();
        assert_eq!(entries.len(), 3, "{all}");
        // Newest first.
        assert_eq!(entries[0]["status"], "created");
        assert_eq!(entries[0]["job_id"], job_ids[2].as_str());
        assert_eq!(entries[0]["matched_queue"], "w8-other");
        assert_eq!(entries[0]["event_type"], "custom");
        assert_eq!(entries[1]["status"], "duplicate");
        assert_eq!(entries[1]["job_id"], job_ids[0].as_str());
        assert_eq!(entries[2]["status"], "created");
        assert_eq!(entries[2]["event_type"], "order.paid");
        assert_eq!(entries[2]["provider"], "custom");
        for entry in entries {
            let keys: Vec<&String> = entry.as_object().unwrap().keys().collect();
            assert!(
                keys.iter()
                    .all(|k| !["payload", "signature", "organization_id"].contains(&k.as_str())),
                "{entry}"
            );
            assert!(entry["id"].is_string() && entry["created_at"].is_string());
        }
        let raw = all.to_string();
        assert!(!raw.contains(token) && !raw.contains("4242"), "{raw}");

        let (_, dup) = list("?status=duplicate").await;
        assert_eq!(dup.as_array().unwrap().len(), 1, "{dup}");
        let (_, created) = list("?status=created&queue_name=w8-in").await;
        assert_eq!(created.as_array().unwrap().len(), 1, "{created}");
        let (_, page) = list("?limit=1&offset=1").await;
        assert_eq!(page.as_array().unwrap().len(), 1, "{page}");
        assert_eq!(page[0]["status"], "duplicate");
        let (_, future) = list("?since=2999-01-01T00:00:00Z").await;
        assert_eq!(future, json!([]));
        let (_, past) = list("?since=2000-01-01T00:00:00Z&limit=2").await;
        assert_eq!(past.as_array().unwrap().len(), 2, "{past}");

        for bad in ["?status=failed", "?since=yesterday"] {
            let (status, body) = list(bad).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{bad}: {body}");
        }

        // Another org sees its own (empty) log; a queue-scoped key is refused
        // with a 403, not a 401 (the dashboard reads 401 as a lost session).
        let (status, theirs) = call(
            &app,
            "GET",
            "/api/v1/webhooks/deliveries",
            &[("Authorization", &bearer(&other.key))],
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(theirs, json!([]));
        let scoped = scoped_key(db.pool(), &t.org_id).await;
        let (status, body) = call(
            &app,
            "GET",
            "/api/v1/webhooks/deliveries",
            &[("Authorization", &bearer(&scoped))],
            None,
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    }
}

/// Wave 8 integration: CORS exposes the new response headers, and the manual
/// retry endpoints announce `job.retried`.
mod w8_integration {
    use super::*;

    use std::time::{Duration, Instant};

    use spooled_backend::config::Environment;

    const EXPOSED: [&str; 7] = [
        "x-ratelimit-limit",
        "x-ratelimit-remaining",
        "x-ratelimit-reset",
        "x-ratelimit-scope",
        "retry-after",
        "x-request-id",
        "x-has-more",
    ];

    async fn exposed_by(app: &Router, origin: &str) -> Vec<String> {
        let req = Request::builder()
            .method("GET")
            .uri("/health")
            .header("Origin", origin)
            .body(Body::empty())
            .unwrap();
        let res = app.clone().oneshot(req).await.expect("request");
        res.headers()
            .get("access-control-expose-headers")
            .expect("Access-Control-Expose-Headers on a CORS response")
            .to_str()
            .unwrap()
            .split(',')
            .map(|h| h.trim().to_ascii_lowercase())
            .collect()
    }

    #[tokio::test]
    async fn w8_cors_exposes_rate_limit_and_pagination_headers() {
        let db = TestDatabase::new().await;

        let dev = exposed_by(&app(&db), "https://dashboard.example").await;
        for name in EXPOSED {
            assert!(dev.iter().any(|h| h == name), "dev missing {name}: {dev:?}");
        }

        let mut settings = Settings::load_for_testing();
        settings.server.environment = Environment::Production;
        settings.server.cors_allowed_origins = vec!["https://dashboard.example".to_string()];
        let prod = exposed_by(&app_with(&db, settings), "https://dashboard.example").await;
        for name in EXPOSED {
            assert!(
                prod.iter().any(|h| h == name),
                "prod missing {name}: {prod:?}"
            );
        }
    }

    async fn receiver() -> String {
        let app = Router::new().fallback(|| async { StatusCode::OK });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        format!("http://{addr}/hook")
    }

    async fn insert_hook(pool: &sqlx::PgPool, org_id: &str, url: &str) {
        sqlx::query(
            "INSERT INTO outgoing_webhooks (organization_id, name, url, events) \
             VALUES ($1, 'w8', $2, ARRAY['job.retried'])",
        )
        .bind(org_id)
        .bind(url)
        .execute(pool)
        .await
        .unwrap();
    }

    async fn insert_job(pool: &sqlx::PgPool, org_id: &str, status: &str) -> String {
        let id = uuid::Uuid::new_v4().to_string();
        sqlx::query(
            "INSERT INTO jobs (id, organization_id, queue_name, payload, status, retry_count, \
             max_retries, completed_at, created_at, updated_at) \
             VALUES ($1, $2, 'w8q', '{}'::JSONB, $3, 3, 3, NOW(), NOW(), NOW())",
        )
        .bind(&id)
        .bind(org_id)
        .bind(status)
        .execute(pool)
        .await
        .unwrap();
        id
    }

    /// job.retried delivery payloads of an org, once `count` exist (plus a
    /// short grace period to catch extras).
    async fn retried(pool: &sqlx::PgPool, org_id: &str, count: usize) -> Vec<Value> {
        let fetch = || async {
            sqlx::query_scalar::<_, Value>(
                "SELECT d.payload->'data' FROM outgoing_webhook_deliveries d \
                 JOIN outgoing_webhooks w ON w.id = d.webhook_id \
                 WHERE w.organization_id = $1 AND d.event = 'job.retried'",
            )
            .bind(org_id)
            .fetch_all(pool)
            .await
            .unwrap()
        };
        let deadline = Instant::now() + Duration::from_secs(10);
        while fetch().await.len() < count && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        tokio::time::sleep(Duration::from_millis(400)).await;
        fetch().await
    }

    #[tokio::test]
    async fn w8_job_retry_sends_job_retried() {
        let db = TestDatabase::new().await;
        let t = seed_tenant(db.pool()).await;
        let app = app(&db);
        insert_hook(db.pool(), &t.org_id, &receiver().await).await;
        let failed = insert_job(db.pool(), &t.org_id, "failed").await;
        let dead = insert_job(db.pool(), &t.org_id, "deadletter").await;

        for id in [&failed, &dead] {
            let (status, body) = call(
                &app,
                "POST",
                &format!("/api/v1/jobs/{id}/retry"),
                &[("Authorization", &bearer(&t.key))],
                None,
            )
            .await;
            assert_eq!(status, StatusCode::OK, "{body}");
        }
        // A retry that is refused announces nothing.
        let (status, _) = call(
            &app,
            "POST",
            &format!("/api/v1/jobs/{failed}/retry"),
            &[("Authorization", &bearer(&t.key))],
            None,
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT);

        let rows = retried(db.pool(), &t.org_id, 2).await;
        assert_eq!(rows.len(), 2, "{rows:?}");
        let of = |id: &str| rows.iter().find(|d| d["job_id"] == id).unwrap().clone();
        assert_eq!(of(&failed)["old_status"], "failed");
        assert_eq!(of(&dead)["old_status"], "deadletter");
        for d in &rows {
            assert_eq!(d["source"], "job_retry");
            assert_eq!(d["status"], "pending");
            assert_eq!(d["queue_name"], "w8q");
        }
    }

    #[tokio::test]
    async fn w8_dlq_retry_sends_one_job_retried_per_job() {
        let db = TestDatabase::new().await;
        let t = seed_tenant(db.pool()).await;
        let app = app(&db);
        insert_hook(db.pool(), &t.org_id, &receiver().await).await;
        let a = insert_job(db.pool(), &t.org_id, "deadletter").await;
        let b = insert_job(db.pool(), &t.org_id, "deadletter").await;
        let untouched = insert_job(db.pool(), &t.org_id, "completed").await;

        let (status, body) = call(
            &app,
            "POST",
            "/api/v1/jobs/dlq/retry",
            &[("Authorization", &bearer(&t.key))],
            Some(json!({ "queue_name": "w8q" })),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["retried_count"], 2, "{body}");

        let rows = retried(db.pool(), &t.org_id, 2).await;
        assert_eq!(rows.len(), 2, "{rows:?}");
        for id in [&a, &b] {
            let d = rows.iter().find(|d| d["job_id"] == id.as_str()).unwrap();
            assert_eq!(d["source"], "dlq_retry");
            assert_eq!(d["old_status"], "deadletter");
        }
        assert!(rows.iter().all(|d| d["job_id"] != untouched.as_str()));
    }
}

// ---- wave 8 / review fixes ----
/// W8-R1: `X-RateLimit-Reset` is a Unix time in seconds, which is how all four
/// SDKs parse it (Go `time.Unix`, Node `new Date(v * 1000)`, Python
/// `datetime.fromtimestamp`, PHP `$reset - time()`), and on a `429` it is the
/// body's `reset_at`. It used to carry the wait in seconds, so every SDK
/// reported the reset as a moment in January 1970.
mod w8_review_ratelimit_reset {
    use super::*;

    use super::wave8_ratelimit::{
        get_jobs, number, redis_app, set_key_rate_limit, set_org_rate_limit, text, unix_now,
    };
    use spooled_backend::api::middleware::plan_rate_limit::{HEADER_RESET, HEADER_SCOPE};

    /// `reset` must be a Unix time no earlier than `not_before` and no later
    /// than `wait_upto` seconds from now. The old seconds-until value (a
    /// handful of seconds since 1970) fails the first bound.
    fn assert_epoch(reset: u64, not_before: u64, wait_upto: u64, what: &str) {
        assert!(
            reset >= not_before,
            "{what}: X-RateLimit-Reset {reset} is not a Unix time at or after {not_before}"
        );
        assert!(
            reset <= unix_now() + wait_upto,
            "{what}: X-RateLimit-Reset {reset} is past now + {wait_upto} s"
        );
    }

    #[tokio::test]
    async fn w8r1_ratelimit_reset_header_is_a_unix_time_equal_to_body_reset_at() {
        let db = TestDatabase::new().await;
        let (app, _cache, _redis) = redis_app(&db).await;

        // Organization bucket: one token, refilled at 1 rps.
        let org = seed_tenant(db.pool()).await;
        set_org_rate_limit(db.pool(), &org.org_id, 1, 1).await;
        let auth = bearer(&org.key);
        let before = unix_now();
        let (status, headers, body) = get_jobs(&app, &auth).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(text(&headers, HEADER_SCOPE), Some("organization"));
        assert_epoch(
            number(&headers, HEADER_RESET).unwrap(),
            before + 1,
            1,
            "organization, allowed",
        );

        let before = unix_now();
        let (status, headers, body) = get_jobs(&app, &auth).await;
        assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "{body}");
        let reset = number(&headers, HEADER_RESET).unwrap();
        let retry_after = number(&headers, "retry-after").unwrap();
        assert_epoch(
            reset,
            before + retry_after,
            retry_after,
            "organization, 429",
        );
        assert_eq!(
            body["reset_at"].as_u64(),
            Some(reset),
            "header and body name the same instant: {body}"
        );

        // API key per-minute window.
        let keyed = seed_tenant(db.pool()).await;
        set_key_rate_limit(db.pool(), &keyed.org_id, 1).await;
        let auth = bearer(&keyed.key);
        let before = unix_now();
        let (status, headers, body) = get_jobs(&app, &auth).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(text(&headers, HEADER_SCOPE), Some("api_key"));
        assert_epoch(
            number(&headers, HEADER_RESET).unwrap(),
            before + 1,
            60,
            "api key, allowed",
        );

        let before = unix_now();
        let (status, headers, body) = get_jobs(&app, &auth).await;
        assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "{body}");
        let reset = number(&headers, HEADER_RESET).unwrap();
        let retry_after = number(&headers, "retry-after").unwrap();
        assert_epoch(reset, before + retry_after, retry_after, "api key, 429");
        assert_eq!(body["reset_at"].as_u64(), Some(reset), "{body}");
    }
}
