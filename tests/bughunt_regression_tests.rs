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
