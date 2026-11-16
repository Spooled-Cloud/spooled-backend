#![cfg(feature = "docker-tests")]
//! ENUM-01..03: no route tells a caller without the admin key, or without the
//! emailed code, whether an address has an account (email enumeration).

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

const ADMIN: &str = "r0130-admin";

struct Tenant {
    org_id: String,
    key: String,
    email: String,
}

/// An organization whose billing email is verified, so email login resolves it.
async fn seed_tenant(pool: &sqlx::PgPool) -> Tenant {
    let org_id = uuid::Uuid::new_v4().to_string();
    let slug = format!("en-{}", &org_id[..8]);
    let email = format!("{slug}@example.com");
    sqlx::query(
        "INSERT INTO organizations (id, name, slug, plan_tier, billing_email, billing_email_verified_at, settings, created_at, updated_at) \
         VALUES ($1, $2, $2, 'free', $3, NOW(), '{}'::JSONB, NOW(), NOW())",
    )
    .bind(&org_id)
    .bind(&slug)
    .bind(&email)
    .execute(pool)
    .await
    .expect("seed org");

    let key = format!("sp_test_{}", uuid::Uuid::new_v4().simple());
    let hash = bcrypt::hash(&key, 4).expect("hash key");
    sqlx::query(
        "INSERT INTO api_keys (id, organization_id, key_hash, key_prefix, lookup_hash, name, queues, is_active, created_at) \
         VALUES ($1, $2, $3, $4, $5, 'en', ARRAY[]::TEXT[], TRUE, NOW())",
    )
    .bind(uuid::Uuid::new_v4().to_string())
    .bind(&org_id)
    .bind(&hash)
    .bind(key.chars().take(8).collect::<String>())
    .bind(spooled_backend::models::api_key_lookup_hash(&key))
    .execute(pool)
    .await
    .expect("seed key");

    Tenant { org_id, key, email }
}

fn app_with_mode(db: &TestDatabase, mode: RegistrationMode) -> Router {
    let mut settings = Settings::load_for_testing();
    settings.registration.admin_api_key = Some(ADMIN.to_string());
    settings.registration.mode = mode;
    router(AppState::new(
        Database::from_pool(Arc::clone(&db.pool)),
        None,
        Metrics::new(),
        settings,
    ))
}

fn app(db: &TestDatabase) -> Router {
    app_with_mode(db, RegistrationMode::Open)
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

async fn check_email(app: &Router, email: &str, headers: &[(&str, &str)]) -> Value {
    let (status, body) = call(
        app,
        "GET",
        &format!("/api/v1/auth/check-email?email={email}"),
        headers,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    body
}

/// Newest stored code row for an address.
async fn latest_code(pool: &sqlx::PgPool, email: &str) -> String {
    let (code,): (String,) = sqlx::query_as(
        "SELECT code FROM email_login_codes WHERE email = $1 ORDER BY created_at DESC LIMIT 1",
    )
    .bind(email)
    .fetch_one(pool)
    .await
    .expect("code row");
    code
}

// ENUM-01: anonymous check-email answers every address the same way; the
// operator (valid X-Admin-Key) still gets the real answer.
#[tokio::test]
async fn enum01_check_email_tells_only_the_admin_whether_an_address_is_registered() {
    let db = TestDatabase::new().await;
    let owner = seed_tenant(db.pool()).await;
    let app = app(&db);
    let unknown = "nobody-r0130@example.com";

    let registered = check_email(&app, &owner.email, &[]).await;
    let fresh = check_email(&app, unknown, &[]).await;
    assert_eq!(registered, fresh, "anonymous answers must not differ");
    assert_eq!(registered["available"], true, "{registered}");
    assert_eq!(registered["exists"], false, "{registered}");
    assert_eq!(registered["signup_enabled"], true, "{registered}");

    // Upper case, and a wrong or empty admin key, change nothing.
    let upper = check_email(&app, &owner.email.to_uppercase(), &[]).await;
    assert_eq!(upper, fresh);
    let wrong = check_email(&app, &owner.email, &[("X-Admin-Key", "not-the-key")]).await;
    assert_eq!(wrong, fresh);
    let empty = check_email(&app, &owner.email, &[("X-Admin-Key", "")]).await;
    assert_eq!(empty, fresh);

    let admin = [("X-Admin-Key", ADMIN)];
    let body = check_email(&app, &owner.email, &admin).await;
    assert_eq!(body["exists"], true, "{body}");
    assert_eq!(body["available"], false, "{body}");
    let body = check_email(&app, unknown, &admin).await;
    assert_eq!(body["exists"], false, "{body}");
    assert_eq!(body["available"], true, "{body}");

    // A malformed address is still a 400, for everyone alike.
    let (status, _) = call(
        &app,
        "GET",
        "/api/v1/auth/check-email?email=not-an-address",
        &[],
        None,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

// ENUM-02: an API key holder cannot test addresses through the billing email.
// Setting another account's verified address and asking for the confirmation
// code answer exactly as for a fresh address; only the emailed code, which the
// mailbox owner receives, reaches the 409.
#[tokio::test]
async fn enum02_billing_email_routes_do_not_reveal_a_registered_address() {
    let db = TestDatabase::new().await;
    let owner = seed_tenant(db.pool()).await;
    let prober = seed_tenant(db.pool()).await;
    let app = app(&db);
    let auth = format!("Bearer {}", prober.key);
    let org_path = format!("/api/v1/organizations/{}", prober.org_id);

    for address in [owner.email.as_str(), "fresh-r0130@example.com"] {
        let (status, body) = call(
            &app,
            "PUT",
            &org_path,
            &[("Authorization", &auth)],
            Some(json!({ "billing_email": address })),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{address}: {body}");
        // The prober's own verified address stays; the new one waits.
        assert_eq!(body["billing_email"], prober.email.as_str(), "{body}");
        assert_eq!(body["pending_billing_email"], address, "{body}");

        let (status, body) = call(
            &app,
            "POST",
            "/api/v1/organizations/billing-email/verification",
            &[("Authorization", &auth)],
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{address}: {body}");
        assert_eq!(body["status"], "code_sent", "{address}: {body}");
    }

    // Back to the owner's address: whoever holds its code (the owner) learns
    // that it is taken; the prober's verified address is untouched.
    let (status, body) = call(
        &app,
        "PUT",
        &org_path,
        &[("Authorization", &auth)],
        Some(json!({ "billing_email": owner.email })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, body) = call(
        &app,
        "POST",
        "/api/v1/organizations/billing-email/verification",
        &[("Authorization", &auth)],
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let code = latest_code(db.pool(), &owner.email).await;
    let code = code.strip_prefix("verify:").expect("verify prefix");
    let (status, body) = call(
        &app,
        "POST",
        "/api/v1/organizations/billing-email/verify",
        &[("Authorization", &auth)],
        Some(json!({ "code": code })),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");

    let (billing_email, verified): (Option<String>, Option<chrono::DateTime<chrono::Utc>>) =
        sqlx::query_as(
            "SELECT billing_email, billing_email_verified_at FROM organizations WHERE id = $1",
        )
        .bind(&prober.org_id)
        .fetch_one(db.pool())
        .await
        .unwrap();
    assert_eq!(billing_email.as_deref(), Some(prober.email.as_str()));
    assert!(verified.is_some());

    // Email login for the owner's address still resolves the owner only.
    let body = check_email(&app, &owner.email, &[("X-Admin-Key", ADMIN)]).await;
    assert_eq!(body["exists"], true, "{body}");
    let (owner_id,): (String,) = sqlx::query_as(
        "SELECT id FROM organizations \
         WHERE lower(billing_email) = $1 AND billing_email_verified_at IS NOT NULL",
    )
    .bind(&owner.email)
    .fetch_one(db.pool())
    .await
    .unwrap();
    assert_eq!(owner_id, owner.org_id);
}

// ENUM-03: public organization create (open registration) does not answer 409
// for a registered address; the copy is stored unverified. With the admin key
// the operator still gets the 409.
#[tokio::test]
async fn enum03_public_org_create_tells_only_the_admin_about_a_registered_address() {
    let db = TestDatabase::new().await;
    let owner = seed_tenant(db.pool()).await;
    let app = app(&db);

    let (status, body) = call(
        &app,
        "POST",
        "/api/v1/organizations",
        &[],
        Some(json!({
            "name": "Enum Probe",
            "slug": "en-probe-r0130",
            "billing_email": owner.email,
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["organization"]["billing_email"], owner.email.as_str());
    assert!(
        body["organization"]["billing_email_verified_at"].is_null(),
        "{body}"
    );

    let (status, body) = call(
        &app,
        "POST",
        "/api/v1/organizations",
        &[("X-Admin-Key", ADMIN)],
        Some(json!({
            "name": "Enum Admin",
            "slug": "en-admin-r0130",
            "billing_email": owner.email,
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");

    // Closed registration: the admin key is required, and the 409 stays.
    let closed = app_with_mode(&db, RegistrationMode::Closed);
    let (status, _) = call(
        &closed,
        "POST",
        "/api/v1/organizations",
        &[],
        Some(json!({
            "name": "Enum Closed",
            "slug": "en-closed-r0130",
            "billing_email": owner.email,
        })),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, body) = call(
        &closed,
        "POST",
        "/api/v1/organizations",
        &[("X-Admin-Key", ADMIN)],
        Some(json!({
            "name": "Enum Closed",
            "slug": "en-closed-r0130",
            "billing_email": owner.email,
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
}
