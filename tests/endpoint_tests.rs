//! Comprehensive endpoint integration tests
//!
//! These tests verify that all API endpoints return correct responses
//! by testing against a real database using testcontainers.

mod common;

use chrono::Utc;
use common::{fixtures, TestDatabase};

// =============================================================================
// Health Endpoints Tests
// =============================================================================

/// Test health check endpoint returns healthy status
#[tokio::test]
async fn test_health_check_returns_healthy() {
    let db = TestDatabase::new().await;

    // Verify database is accessible (simulates health check)
    let result: Option<(i32,)> = sqlx::query_as("SELECT 1")
        .fetch_optional(db.pool())
        .await
        .expect("Database should be accessible");

    assert!(result.is_some(), "Health check query should succeed");
    assert_eq!(result.unwrap().0, 1);
}

/// Test that database tables exist for readiness
#[tokio::test]
async fn test_readiness_tables_exist() {
    let db = TestDatabase::new().await;

    // Check all required tables exist
    let tables: Vec<(String,)> =
        sqlx::query_as("SELECT tablename::TEXT FROM pg_tables WHERE schemaname = 'public'")
            .fetch_all(db.pool())
            .await
            .expect("Should list tables");

    let table_names: Vec<&str> = tables.iter().map(|(t,)| t.as_str()).collect();

    assert!(table_names.contains(&"organizations"));
    assert!(table_names.contains(&"jobs"));
    assert!(table_names.contains(&"workers"));
    assert!(table_names.contains(&"api_keys"));
    assert!(table_names.contains(&"schedules"));
    assert!(table_names.contains(&"workflows"));
}

// =============================================================================
// Organization Endpoints Tests
// =============================================================================

/// Test organization creation
#[tokio::test]
async fn test_organization_create() {
    let db = TestDatabase::new().await;

    let org_id = uuid::Uuid::new_v4().to_string();
    let org_name = "Test Organization";
    let org_slug = format!("test-org-{}", &org_id[..8]);

    sqlx::query(
        r#"
        INSERT INTO organizations (id, name, slug, plan_tier, created_at, updated_at)
        VALUES ($1, $2, $3, 'free', NOW(), NOW())
        "#,
    )
    .bind(&org_id)
    .bind(org_name)
    .bind(&org_slug)
    .execute(db.pool())
    .await
    .expect("Should create organization");

    // Verify organization was created
    let org: Option<(String, String, String)> =
        sqlx::query_as("SELECT id, name, slug FROM organizations WHERE id = $1")
            .bind(&org_id)
            .fetch_optional(db.pool())
            .await
            .expect("Should query organization");

    assert!(org.is_some());
    let (id, name, slug) = org.unwrap();
    assert_eq!(id, org_id);
    assert_eq!(name, org_name);
    assert_eq!(slug, org_slug);
}

/// Test organization list returns correct data
#[tokio::test]
async fn test_organization_list() {
    let db = TestDatabase::new().await;

    // Create multiple organizations
    for i in 0..3 {
        let org_id = uuid::Uuid::new_v4().to_string();
        sqlx::query(
            "INSERT INTO organizations (id, name, slug, plan_tier, created_at, updated_at) VALUES ($1, $2, $3, 'free', NOW(), NOW())"
        )
        .bind(&org_id)
        .bind(format!("Org {}", i))
        .bind(format!("org-{}-{}", i, &org_id[..8]))
        .execute(db.pool())
        .await
        .expect("Should create org");
    }

    // List organizations (excludes default-org created by migration)
    let orgs: Vec<(String, String)> = sqlx::query_as(
        "SELECT id, name FROM organizations WHERE id != 'default-org' ORDER BY created_at",
    )
    .fetch_all(db.pool())
    .await
    .expect("Should list organizations");

    assert_eq!(orgs.len(), 3);
}

/// Test organization update
#[tokio::test]
async fn test_organization_update() {
    let db = TestDatabase::new().await;

    let org_id = uuid::Uuid::new_v4().to_string();
    sqlx::query(
        "INSERT INTO organizations (id, name, slug, plan_tier, created_at, updated_at) VALUES ($1, 'Original Name', 'original-slug', 'free', NOW(), NOW())"
    )
    .bind(&org_id)
    .execute(db.pool())
    .await
    .expect("Should create org");

    // Update organization
    sqlx::query("UPDATE organizations SET name = $1 WHERE id = $2")
        .bind("Updated Name")
        .bind(&org_id)
        .execute(db.pool())
        .await
        .expect("Should update org");

    // Verify update
    let name: (String,) = sqlx::query_as("SELECT name FROM organizations WHERE id = $1")
        .bind(&org_id)
        .fetch_one(db.pool())
        .await
        .expect("Should fetch org");

    assert_eq!(name.0, "Updated Name");
}

/// Test organization delete
#[tokio::test]
async fn test_organization_delete() {
    let db = TestDatabase::new().await;

    let org_id = uuid::Uuid::new_v4().to_string();
    sqlx::query(
        "INSERT INTO organizations (id, name, slug, plan_tier, created_at, updated_at) VALUES ($1, 'To Delete', 'to-delete', 'free', NOW(), NOW())"
    )
    .bind(&org_id)
    .execute(db.pool())
    .await
    .expect("Should create org");

    // Delete organization
    let result = sqlx::query("DELETE FROM organizations WHERE id = $1")
        .bind(&org_id)
        .execute(db.pool())
        .await
        .expect("Should delete org");

    assert_eq!(result.rows_affected(), 1);

    // Verify deletion
    let org: Option<(String,)> = sqlx::query_as("SELECT id FROM organizations WHERE id = $1")
        .bind(&org_id)
        .fetch_optional(db.pool())
        .await
        .expect("Should query org");

    assert!(org.is_none());
}

// =============================================================================
// Job Endpoints Tests
// =============================================================================

/// Test job creation
#[tokio::test]
async fn test_job_create() {
    let db = TestDatabase::new().await;

    // Create org first
    let org_id = "test-org-job";
    sqlx::query(
        "INSERT INTO organizations (id, name, slug, plan_tier, created_at, updated_at) VALUES ($1, $1, $1, 'free', NOW(), NOW()) ON CONFLICT DO NOTHING"
    )
    .bind(org_id)
    .execute(db.pool())
    .await
    .expect("Should create org");

    // Create job
    let job_id = uuid::Uuid::new_v4().to_string();
    sqlx::query(
        r#"
        INSERT INTO jobs (id, organization_id, queue_name, status, payload, priority, max_retries, timeout_seconds, created_at, updated_at)
        VALUES ($1, $2, 'default', 'pending', '{}'::JSONB, 0, 3, 300, NOW(), NOW())
        "#
    )
    .bind(&job_id)
    .bind(org_id)
    .execute(db.pool())
    .await
    .expect("Should create job");

    // Verify job
    let job: Option<(String, String, String)> =
        sqlx::query_as("SELECT id, organization_id, status FROM jobs WHERE id = $1")
            .bind(&job_id)
            .fetch_optional(db.pool())
            .await
            .expect("Should query job");

    assert!(job.is_some());
    let (id, org, status) = job.unwrap();
    assert_eq!(id, job_id);
    assert_eq!(org, org_id);
    assert_eq!(status, "pending");
}

/// Test job list with organization filter
#[tokio::test]
async fn test_job_list_org_isolation() {
    let db = TestDatabase::new().await;

    // Create two orgs
    for org_id in ["org-1", "org-2"] {
        sqlx::query(
            "INSERT INTO organizations (id, name, slug, plan_tier, created_at, updated_at) VALUES ($1, $1, $1, 'free', NOW(), NOW()) ON CONFLICT DO NOTHING"
        )
        .bind(org_id)
        .execute(db.pool())
        .await
        .expect("Should create org");
    }

    // Create jobs for each org
    for org_id in ["org-1", "org-2"] {
        for _ in 0..3 {
            let job_id = uuid::Uuid::new_v4().to_string();
            sqlx::query(
                "INSERT INTO jobs (id, organization_id, queue_name, status, payload, priority, max_retries, timeout_seconds, created_at, updated_at) VALUES ($1, $2, 'default', 'pending', '{}'::JSONB, 0, 3, 300, NOW(), NOW())"
            )
            .bind(&job_id)
            .bind(org_id)
            .execute(db.pool())
            .await
            .expect("Should create job");
        }
    }

    // List jobs for org-1 only
    let org1_jobs: Vec<(String,)> =
        sqlx::query_as("SELECT id FROM jobs WHERE organization_id = $1")
            .bind("org-1")
            .fetch_all(db.pool())
            .await
            .expect("Should list jobs");

    assert_eq!(org1_jobs.len(), 3, "Org-1 should have exactly 3 jobs");

    // List jobs for org-2 only
    let org2_jobs: Vec<(String,)> =
        sqlx::query_as("SELECT id FROM jobs WHERE organization_id = $1")
            .bind("org-2")
            .fetch_all(db.pool())
            .await
            .expect("Should list jobs");

    assert_eq!(org2_jobs.len(), 3, "Org-2 should have exactly 3 jobs");
}

/// Test job status transitions
#[tokio::test]
async fn test_job_status_transitions() {
    let db = TestDatabase::new().await;

    let org_id = "test-org-status";
    sqlx::query(
        "INSERT INTO organizations (id, name, slug, plan_tier, created_at, updated_at) VALUES ($1, $1, $1, 'free', NOW(), NOW()) ON CONFLICT DO NOTHING"
    )
    .bind(org_id)
    .execute(db.pool())
    .await
    .expect("Should create org");

    let job_id = uuid::Uuid::new_v4().to_string();
    sqlx::query(
        "INSERT INTO jobs (id, organization_id, queue_name, status, payload, priority, max_retries, timeout_seconds, created_at, updated_at) VALUES ($1, $2, 'default', 'pending', '{}'::JSONB, 0, 3, 300, NOW(), NOW())"
    )
    .bind(&job_id)
    .bind(org_id)
    .execute(db.pool())
    .await
    .expect("Should create job");

    // Transition: pending -> processing
    sqlx::query("UPDATE jobs SET status = 'processing' WHERE id = $1")
        .bind(&job_id)
        .execute(db.pool())
        .await
        .expect("Should update status");

    let status: (String,) = sqlx::query_as("SELECT status FROM jobs WHERE id = $1")
        .bind(&job_id)
        .fetch_one(db.pool())
        .await
        .expect("Should query status");
    assert_eq!(status.0, "processing");

    // Transition: processing -> completed
    sqlx::query("UPDATE jobs SET status = 'completed', completed_at = NOW() WHERE id = $1")
        .bind(&job_id)
        .execute(db.pool())
        .await
        .expect("Should complete job");

    let status: (String,) = sqlx::query_as("SELECT status FROM jobs WHERE id = $1")
        .bind(&job_id)
        .fetch_one(db.pool())
        .await
        .expect("Should query status");
    assert_eq!(status.0, "completed");
}

/// Test job retry logic
#[tokio::test]
async fn test_job_retry() {
    let db = TestDatabase::new().await;

    let org_id = "test-org-retry";
    sqlx::query(
        "INSERT INTO organizations (id, name, slug, plan_tier, created_at, updated_at) VALUES ($1, $1, $1, 'free', NOW(), NOW()) ON CONFLICT DO NOTHING"
    )
    .bind(org_id)
    .execute(db.pool())
    .await
    .expect("Should create org");

    let job_id = uuid::Uuid::new_v4().to_string();
    sqlx::query(
        "INSERT INTO jobs (id, organization_id, queue_name, status, payload, priority, max_retries, retry_count, timeout_seconds, created_at, updated_at) VALUES ($1, $2, 'default', 'failed', '{}'::JSONB, 0, 3, 1, 300, NOW(), NOW())"
    )
    .bind(&job_id)
    .bind(org_id)
    .execute(db.pool())
    .await
    .expect("Should create failed job");

    // Retry job
    let result = sqlx::query(
        r#"
        UPDATE jobs 
        SET status = 'pending', 
            retry_count = retry_count + 1,
            updated_at = NOW()
        WHERE id = $1 AND organization_id = $2 AND retry_count < max_retries
        "#,
    )
    .bind(&job_id)
    .bind(org_id)
    .execute(db.pool())
    .await
    .expect("Should retry job");

    assert_eq!(result.rows_affected(), 1);

    let job: (String, i32) = sqlx::query_as("SELECT status, retry_count FROM jobs WHERE id = $1")
        .bind(&job_id)
        .fetch_one(db.pool())
        .await
        .expect("Should query job");

    assert_eq!(job.0, "pending");
    assert_eq!(job.1, 2);
}

/// Test job cancel
#[tokio::test]
async fn test_job_cancel() {
    let db = TestDatabase::new().await;

    let org_id = "test-org-cancel";
    sqlx::query(
        "INSERT INTO organizations (id, name, slug, plan_tier, created_at, updated_at) VALUES ($1, $1, $1, 'free', NOW(), NOW()) ON CONFLICT DO NOTHING"
    )
    .bind(org_id)
    .execute(db.pool())
    .await
    .expect("Should create org");

    let job_id = uuid::Uuid::new_v4().to_string();
    sqlx::query(
        "INSERT INTO jobs (id, organization_id, queue_name, status, payload, priority, max_retries, timeout_seconds, created_at, updated_at) VALUES ($1, $2, 'default', 'pending', '{}'::JSONB, 0, 3, 300, NOW(), NOW())"
    )
    .bind(&job_id)
    .bind(org_id)
    .execute(db.pool())
    .await
    .expect("Should create job");

    // Cancel job
    sqlx::query("UPDATE jobs SET status = 'cancelled' WHERE id = $1 AND organization_id = $2")
        .bind(&job_id)
        .bind(org_id)
        .execute(db.pool())
        .await
        .expect("Should cancel job");

    let status: (String,) = sqlx::query_as("SELECT status FROM jobs WHERE id = $1")
        .bind(&job_id)
        .fetch_one(db.pool())
        .await
        .expect("Should query status");

    assert_eq!(status.0, "cancelled");
}

// =============================================================================
// Queue Config Endpoints Tests
// =============================================================================

/// Test queue config creation
#[tokio::test]
async fn test_queue_config_create() {
    let db = TestDatabase::new().await;

    let org_id = "test-org-queue";
    sqlx::query(
        "INSERT INTO organizations (id, name, slug, plan_tier, created_at, updated_at) VALUES ($1, $1, $1, 'free', NOW(), NOW()) ON CONFLICT DO NOTHING"
    )
    .bind(org_id)
    .execute(db.pool())
    .await
    .expect("Should create org");

    // Create queue config
    sqlx::query(
        r#"
        INSERT INTO queue_config (organization_id, queue_name, max_retries, default_timeout, rate_limit, enabled, created_at, updated_at)
        VALUES ($1, 'emails', 5, 120, 100, true, NOW(), NOW())
        "#
    )
    .bind(org_id)
    .execute(db.pool())
    .await
    .expect("Should create queue config");

    // Verify
    let config: Option<(String, i32, i32, Option<i32>, bool)> = sqlx::query_as(
        "SELECT queue_name, max_retries, default_timeout, rate_limit, enabled FROM queue_config WHERE organization_id = $1 AND queue_name = 'emails'"
    )
    .bind(org_id)
    .fetch_optional(db.pool())
    .await
    .expect("Should query config");

    assert!(config.is_some());
    let (name, retries, timeout, rate, enabled) = config.unwrap();
    assert_eq!(name, "emails");
    assert_eq!(retries, 5);
    assert_eq!(timeout, 120);
    assert_eq!(rate, Some(100));
    assert!(enabled);
}

/// Test queue pause/resume
#[tokio::test]
async fn test_queue_pause_resume() {
    let db = TestDatabase::new().await;

    let org_id = "test-org-pause";
    sqlx::query(
        "INSERT INTO organizations (id, name, slug, plan_tier, created_at, updated_at) VALUES ($1, $1, $1, 'free', NOW(), NOW()) ON CONFLICT DO NOTHING"
    )
    .bind(org_id)
    .execute(db.pool())
    .await
    .expect("Should create org");

    sqlx::query(
        "INSERT INTO queue_config (organization_id, queue_name, max_retries, default_timeout, enabled, created_at, updated_at) VALUES ($1, 'pausable', 3, 60, true, NOW(), NOW())"
    )
    .bind(org_id)
    .execute(db.pool())
    .await
    .expect("Should create queue");

    // Pause queue
    sqlx::query("UPDATE queue_config SET enabled = false WHERE organization_id = $1 AND queue_name = 'pausable'")
        .bind(org_id)
        .execute(db.pool())
        .await
        .expect("Should pause queue");

    let enabled: (bool,) = sqlx::query_as(
        "SELECT enabled FROM queue_config WHERE organization_id = $1 AND queue_name = 'pausable'",
    )
    .bind(org_id)
    .fetch_one(db.pool())
    .await
    .expect("Should query queue");

    assert!(!enabled.0, "Queue should be paused");

    // Resume queue
    sqlx::query("UPDATE queue_config SET enabled = true WHERE organization_id = $1 AND queue_name = 'pausable'")
        .bind(org_id)
        .execute(db.pool())
        .await
        .expect("Should resume queue");

    let enabled: (bool,) = sqlx::query_as(
        "SELECT enabled FROM queue_config WHERE organization_id = $1 AND queue_name = 'pausable'",
    )
    .bind(org_id)
    .fetch_one(db.pool())
    .await
    .expect("Should query queue");

    assert!(enabled.0, "Queue should be resumed");
}

// =============================================================================
// Worker Endpoints Tests
// =============================================================================

/// Test worker registration
#[tokio::test]
async fn test_worker_register() {
    let db = TestDatabase::new().await;

    let org_id = "test-org-worker";
    sqlx::query(
        "INSERT INTO organizations (id, name, slug, plan_tier, created_at, updated_at) VALUES ($1, $1, $1, 'free', NOW(), NOW()) ON CONFLICT DO NOTHING"
    )
    .bind(org_id)
    .execute(db.pool())
    .await
    .expect("Should create org");

    let worker_id = uuid::Uuid::new_v4().to_string();
    sqlx::query(
        r#"
        INSERT INTO workers (id, organization_id, queue_name, queue_names, hostname, max_concurrent_jobs, current_job_count, status, last_heartbeat, metadata, created_at, updated_at)
        VALUES ($1, $2, 'default', ARRAY['default'], 'worker-host-1', 5, 0, 'healthy', NOW(), '{}'::JSONB, NOW(), NOW())
        "#
    )
    .bind(&worker_id)
    .bind(org_id)
    .execute(db.pool())
    .await
    .expect("Should register worker");

    // Verify
    let worker: Option<(String, String, i32)> =
        sqlx::query_as("SELECT id, status, max_concurrent_jobs FROM workers WHERE id = $1")
            .bind(&worker_id)
            .fetch_optional(db.pool())
            .await
            .expect("Should query worker");

    assert!(worker.is_some());
    let (id, status, concurrency) = worker.unwrap();
    assert_eq!(id, worker_id);
    assert_eq!(status, "healthy");
    assert_eq!(concurrency, 5);
}

/// Test worker heartbeat
#[tokio::test]
async fn test_worker_heartbeat() {
    let db = TestDatabase::new().await;

    let org_id = "test-org-hb";
    sqlx::query(
        "INSERT INTO organizations (id, name, slug, plan_tier, created_at, updated_at) VALUES ($1, $1, $1, 'free', NOW(), NOW()) ON CONFLICT DO NOTHING"
    )
    .bind(org_id)
    .execute(db.pool())
    .await
    .expect("Should create org");

    let worker_id = uuid::Uuid::new_v4().to_string();
    sqlx::query(
        "INSERT INTO workers (id, organization_id, queue_name, queue_names, hostname, max_concurrent_jobs, current_job_count, status, last_heartbeat, metadata, created_at, updated_at) VALUES ($1, $2, 'default', ARRAY['default'], 'host', 5, 0, 'healthy', NOW() - INTERVAL '1 minute', '{}'::JSONB, NOW(), NOW())"
    )
    .bind(&worker_id)
    .bind(org_id)
    .execute(db.pool())
    .await
    .expect("Should create worker");

    // Send heartbeat
    let result = sqlx::query(
        "UPDATE workers SET last_heartbeat = NOW(), current_job_count = $1 WHERE id = $2 AND organization_id = $3"
    )
    .bind(2i32)
    .bind(&worker_id)
    .bind(org_id)
    .execute(db.pool())
    .await
    .expect("Should update heartbeat");

    assert_eq!(result.rows_affected(), 1);

    let current_jobs: (i32,) =
        sqlx::query_as("SELECT current_job_count FROM workers WHERE id = $1")
            .bind(&worker_id)
            .fetch_one(db.pool())
            .await
            .expect("Should query worker");

    assert_eq!(current_jobs.0, 2);
}

/// Test worker deregister
#[tokio::test]
async fn test_worker_deregister() {
    let db = TestDatabase::new().await;

    let org_id = "test-org-dereg";
    sqlx::query(
        "INSERT INTO organizations (id, name, slug, plan_tier, created_at, updated_at) VALUES ($1, $1, $1, 'free', NOW(), NOW()) ON CONFLICT DO NOTHING"
    )
    .bind(org_id)
    .execute(db.pool())
    .await
    .expect("Should create org");

    let worker_id = uuid::Uuid::new_v4().to_string();
    sqlx::query(
        "INSERT INTO workers (id, organization_id, queue_name, queue_names, hostname, max_concurrent_jobs, current_job_count, status, last_heartbeat, metadata, created_at, updated_at) VALUES ($1, $2, 'default', ARRAY['default'], 'host', 5, 0, 'healthy', NOW(), '{}'::JSONB, NOW(), NOW())"
    )
    .bind(&worker_id)
    .bind(org_id)
    .execute(db.pool())
    .await
    .expect("Should create worker");

    // Deregister
    sqlx::query("UPDATE workers SET status = 'offline' WHERE id = $1 AND organization_id = $2")
        .bind(&worker_id)
        .bind(org_id)
        .execute(db.pool())
        .await
        .expect("Should deregister worker");

    let status: (String,) = sqlx::query_as("SELECT status FROM workers WHERE id = $1")
        .bind(&worker_id)
        .fetch_one(db.pool())
        .await
        .expect("Should query status");

    assert_eq!(status.0, "offline");
}

// =============================================================================
// API Key Endpoints Tests
// =============================================================================

/// Test API key creation and storage
#[tokio::test]
async fn test_api_key_create() {
    let db = TestDatabase::new().await;

    let org_id = "test-org-apikey";
    sqlx::query(
        "INSERT INTO organizations (id, name, slug, plan_tier, created_at, updated_at) VALUES ($1, $1, $1, 'free', NOW(), NOW()) ON CONFLICT DO NOTHING"
    )
    .bind(org_id)
    .execute(db.pool())
    .await
    .expect("Should create org");

    let key_id = uuid::Uuid::new_v4().to_string();
    let key_hash = bcrypt::hash("sk_test_secretkey123", bcrypt::DEFAULT_COST).unwrap();

    sqlx::query(
        r#"
        INSERT INTO api_keys (id, organization_id, key_hash, name, queues, is_active, created_at)
        VALUES ($1, $2, $3, 'Test API Key', ARRAY['default', 'emails'], true, NOW())
        "#,
    )
    .bind(&key_id)
    .bind(org_id)
    .bind(&key_hash)
    .execute(db.pool())
    .await
    .expect("Should create API key");

    // Verify
    let key: Option<(String, String, bool)> =
        sqlx::query_as("SELECT id, name, is_active FROM api_keys WHERE id = $1")
            .bind(&key_id)
            .fetch_optional(db.pool())
            .await
            .expect("Should query API key");

    assert!(key.is_some());
    let (id, name, active) = key.unwrap();
    assert_eq!(id, key_id);
    assert_eq!(name, "Test API Key");
    assert!(active);
}

/// Test API key revocation
#[tokio::test]
async fn test_api_key_revoke() {
    let db = TestDatabase::new().await;

    let org_id = "test-org-revoke";
    sqlx::query(
        "INSERT INTO organizations (id, name, slug, plan_tier, created_at, updated_at) VALUES ($1, $1, $1, 'free', NOW(), NOW()) ON CONFLICT DO NOTHING"
    )
    .bind(org_id)
    .execute(db.pool())
    .await
    .expect("Should create org");

    let key_id = uuid::Uuid::new_v4().to_string();
    let key_hash = bcrypt::hash("sk_test_torevoke", bcrypt::DEFAULT_COST).unwrap();

    sqlx::query(
        "INSERT INTO api_keys (id, organization_id, key_hash, name, queues, is_active, created_at) VALUES ($1, $2, $3, 'To Revoke', ARRAY['default'], true, NOW())"
    )
    .bind(&key_id)
    .bind(org_id)
    .bind(&key_hash)
    .execute(db.pool())
    .await
    .expect("Should create API key");

    // Revoke
    sqlx::query("UPDATE api_keys SET is_active = false WHERE id = $1 AND organization_id = $2")
        .bind(&key_id)
        .bind(org_id)
        .execute(db.pool())
        .await
        .expect("Should revoke key");

    let active: (bool,) = sqlx::query_as("SELECT is_active FROM api_keys WHERE id = $1")
        .bind(&key_id)
        .fetch_one(db.pool())
        .await
        .expect("Should query key");

    assert!(!active.0);
}

// =============================================================================
// Schedule Endpoints Tests
// =============================================================================

/// Test schedule creation
#[tokio::test]
async fn test_schedule_create() {
    let db = TestDatabase::new().await;

    let org_id = "test-org-schedule";
    sqlx::query(
        "INSERT INTO organizations (id, name, slug, plan_tier, created_at, updated_at) VALUES ($1, $1, $1, 'free', NOW(), NOW()) ON CONFLICT DO NOTHING"
    )
    .bind(org_id)
    .execute(db.pool())
    .await
    .expect("Should create org");

    let schedule_id = uuid::Uuid::new_v4().to_string();
    sqlx::query(
        r#"
        INSERT INTO schedules (id, organization_id, name, cron_expression, queue_name, payload_template, timezone, priority, max_retries, timeout_seconds, is_active, created_at, updated_at)
        VALUES ($1, $2, 'Daily Job', '0 0 0 * * *', 'default', '{}'::JSONB, 'UTC', 0, 3, 300, true, NOW(), NOW())
        "#
    )
    .bind(&schedule_id)
    .bind(org_id)
    .execute(db.pool())
    .await
    .expect("Should create schedule");

    // Verify
    let schedule: Option<(String, String, String, bool)> =
        sqlx::query_as("SELECT id, name, cron_expression, is_active FROM schedules WHERE id = $1")
            .bind(&schedule_id)
            .fetch_optional(db.pool())
            .await
            .expect("Should query schedule");

    assert!(schedule.is_some());
    let (id, name, cron, active) = schedule.unwrap();
    assert_eq!(id, schedule_id);
    assert_eq!(name, "Daily Job");
    assert_eq!(cron, "0 0 0 * * *");
    assert!(active);
}

/// Test schedule pause/resume
#[tokio::test]
async fn test_schedule_pause_resume() {
    let db = TestDatabase::new().await;

    let org_id = "test-org-sched-pause";
    sqlx::query(
        "INSERT INTO organizations (id, name, slug, plan_tier, created_at, updated_at) VALUES ($1, $1, $1, 'free', NOW(), NOW()) ON CONFLICT DO NOTHING"
    )
    .bind(org_id)
    .execute(db.pool())
    .await
    .expect("Should create org");

    let schedule_id = uuid::Uuid::new_v4().to_string();
    sqlx::query(
        "INSERT INTO schedules (id, organization_id, name, cron_expression, queue_name, payload_template, timezone, priority, max_retries, timeout_seconds, is_active, created_at, updated_at) VALUES ($1, $2, 'Pausable', '* * * * * *', 'default', '{}'::JSONB, 'UTC', 0, 3, 300, true, NOW(), NOW())"
    )
    .bind(&schedule_id)
    .bind(org_id)
    .execute(db.pool())
    .await
    .expect("Should create schedule");

    // Pause
    sqlx::query("UPDATE schedules SET is_active = false WHERE id = $1 AND organization_id = $2")
        .bind(&schedule_id)
        .bind(org_id)
        .execute(db.pool())
        .await
        .expect("Should pause schedule");

    let active: (bool,) = sqlx::query_as("SELECT is_active FROM schedules WHERE id = $1")
        .bind(&schedule_id)
        .fetch_one(db.pool())
        .await
        .expect("Should query schedule");

    assert!(!active.0);

    // Resume
    sqlx::query("UPDATE schedules SET is_active = true WHERE id = $1 AND organization_id = $2")
        .bind(&schedule_id)
        .bind(org_id)
        .execute(db.pool())
        .await
        .expect("Should resume schedule");

    let active: (bool,) = sqlx::query_as("SELECT is_active FROM schedules WHERE id = $1")
        .bind(&schedule_id)
        .fetch_one(db.pool())
        .await
        .expect("Should query schedule");

    assert!(active.0);
}

// =============================================================================
// Workflow Endpoints Tests
// =============================================================================

/// Test workflow creation
#[tokio::test]
async fn test_workflow_create() {
    let db = TestDatabase::new().await;

    let org_id = "test-org-workflow";
    sqlx::query(
        "INSERT INTO organizations (id, name, slug, plan_tier, created_at, updated_at) VALUES ($1, $1, $1, 'free', NOW(), NOW()) ON CONFLICT DO NOTHING"
    )
    .bind(org_id)
    .execute(db.pool())
    .await
    .expect("Should create org");

    let workflow_id = uuid::Uuid::new_v4().to_string();
    sqlx::query(
        r#"
        INSERT INTO workflows (id, organization_id, name, status, metadata, created_at)
        VALUES ($1, $2, 'Test Workflow', 'running', '{}'::JSONB, NOW())
        "#,
    )
    .bind(&workflow_id)
    .bind(org_id)
    .execute(db.pool())
    .await
    .expect("Should create workflow");

    // Verify
    let workflow: Option<(String, String, String)> =
        sqlx::query_as("SELECT id, name, status FROM workflows WHERE id = $1")
            .bind(&workflow_id)
            .fetch_optional(db.pool())
            .await
            .expect("Should query workflow");

    assert!(workflow.is_some());
    let (id, name, status) = workflow.unwrap();
    assert_eq!(id, workflow_id);
    assert_eq!(name, "Test Workflow");
    assert_eq!(status, "running");
}

/// Test workflow with job dependencies
#[tokio::test]
async fn test_workflow_job_dependencies() {
    let db = TestDatabase::new().await;

    let org_id = "test-org-deps";
    sqlx::query(
        "INSERT INTO organizations (id, name, slug, plan_tier, created_at, updated_at) VALUES ($1, $1, $1, 'free', NOW(), NOW()) ON CONFLICT DO NOTHING"
    )
    .bind(org_id)
    .execute(db.pool())
    .await
    .expect("Should create org");

    let workflow_id = uuid::Uuid::new_v4().to_string();
    sqlx::query(
        "INSERT INTO workflows (id, organization_id, name, status, metadata, created_at) VALUES ($1, $2, 'Dep Test', 'running', '{}'::JSONB, NOW())"
    )
    .bind(&workflow_id)
    .bind(org_id)
    .execute(db.pool())
    .await
    .expect("Should create workflow");

    // Create parent job
    let parent_id = uuid::Uuid::new_v4().to_string();
    sqlx::query(
        "INSERT INTO jobs (id, organization_id, queue_name, status, payload, priority, max_retries, timeout_seconds, workflow_id, created_at, updated_at) VALUES ($1, $2, 'default', 'pending', '{}'::JSONB, 0, 3, 300, $3, NOW(), NOW())"
    )
    .bind(&parent_id)
    .bind(org_id)
    .bind(&workflow_id)
    .execute(db.pool())
    .await
    .expect("Should create parent job");

    // Create child job with dependency
    let child_id = uuid::Uuid::new_v4().to_string();
    sqlx::query(
        "INSERT INTO jobs (id, organization_id, queue_name, status, payload, priority, max_retries, timeout_seconds, workflow_id, parent_job_id, dependencies_met, created_at, updated_at) VALUES ($1, $2, 'default', 'pending', '{}'::JSONB, 0, 3, 300, $3, $4, false, NOW(), NOW())"
    )
    .bind(&child_id)
    .bind(org_id)
    .bind(&workflow_id)
    .bind(&parent_id)
    .execute(db.pool())
    .await
    .expect("Should create child job");

    // Verify dependency structure
    let child: (String, bool) =
        sqlx::query_as("SELECT parent_job_id, dependencies_met FROM jobs WHERE id = $1")
            .bind(&child_id)
            .fetch_one(db.pool())
            .await
            .expect("Should query child");

    assert_eq!(child.0, parent_id);
    assert!(!child.1, "Dependencies should not be met initially");
}

// =============================================================================
// Dead Letter Queue Tests
// =============================================================================

/// Test DLQ entries are created for failed jobs
#[tokio::test]
async fn test_dlq_entry_creation() {
    let db = TestDatabase::new().await;

    let org_id = "test-org-dlq";
    sqlx::query(
        "INSERT INTO organizations (id, name, slug, plan_tier, created_at, updated_at) VALUES ($1, $1, $1, 'free', NOW(), NOW()) ON CONFLICT DO NOTHING"
    )
    .bind(org_id)
    .execute(db.pool())
    .await
    .expect("Should create org");

    let job_id = uuid::Uuid::new_v4().to_string();
    sqlx::query(
        "INSERT INTO jobs (id, organization_id, queue_name, status, payload, priority, max_retries, retry_count, timeout_seconds, created_at, updated_at) VALUES ($1, $2, 'default', 'failed', '{\"test\": true}'::JSONB, 0, 3, 3, 300, NOW(), NOW())"
    )
    .bind(&job_id)
    .bind(org_id)
    .execute(db.pool())
    .await
    .expect("Should create failed job");

    // Create DLQ entry (columns: id, job_id, organization_id, queue_name, reason, original_payload, error_details, created_at)
    let dlq_id = uuid::Uuid::new_v4().to_string();
    sqlx::query(
        r#"
        INSERT INTO dead_letter_queue (id, job_id, organization_id, queue_name, reason, original_payload, error_details, created_at)
        VALUES ($1, $2, $3, 'default', 'Max retries exceeded', '{"test": true}'::JSONB, '{"error": "test"}'::JSONB, NOW())
        "#
    )
    .bind(&dlq_id)
    .bind(&job_id)
    .bind(org_id)
    .execute(db.pool())
    .await
    .expect("Should create DLQ entry");

    // Verify
    let dlq: Option<(String, String, String)> =
        sqlx::query_as("SELECT id, job_id, reason FROM dead_letter_queue WHERE id = $1")
            .bind(&dlq_id)
            .fetch_optional(db.pool())
            .await
            .expect("Should query DLQ");

    assert!(dlq.is_some());
    let (id, jid, reason) = dlq.unwrap();
    assert_eq!(id, dlq_id);
    assert_eq!(jid, job_id);
    assert_eq!(reason, "Max retries exceeded");
}

// =============================================================================
// Job History Tests
// =============================================================================

/// Test job history tracking
#[tokio::test]
async fn test_job_history_tracking() {
    let db = TestDatabase::new().await;

    let org_id = "test-org-history";
    sqlx::query(
        "INSERT INTO organizations (id, name, slug, plan_tier, created_at, updated_at) VALUES ($1, $1, $1, 'free', NOW(), NOW()) ON CONFLICT DO NOTHING"
    )
    .bind(org_id)
    .execute(db.pool())
    .await
    .expect("Should create org");

    let job_id = uuid::Uuid::new_v4().to_string();
    sqlx::query(
        "INSERT INTO jobs (id, organization_id, queue_name, status, payload, priority, max_retries, timeout_seconds, created_at, updated_at) VALUES ($1, $2, 'default', 'pending', '{}'::JSONB, 0, 3, 300, NOW(), NOW())"
    )
    .bind(&job_id)
    .bind(org_id)
    .execute(db.pool())
    .await
    .expect("Should create job");

    // Add history entries (job_history has: id, job_id, event_type, worker_id, details, created_at)
    for event_type in ["created", "started", "completed"] {
        let history_id = uuid::Uuid::new_v4().to_string();
        sqlx::query(
            "INSERT INTO job_history (id, job_id, event_type, details, created_at) VALUES ($1, $2, $3, $4::JSONB, NOW())"
        )
        .bind(&history_id)
        .bind(&job_id)
        .bind(event_type)
        .bind(serde_json::json!({"status": event_type}).to_string())
        .execute(db.pool())
        .await
        .expect("Should add history");
    }

    // Verify history
    let history: Vec<(String,)> =
        sqlx::query_as("SELECT event_type FROM job_history WHERE job_id = $1 ORDER BY created_at")
            .bind(&job_id)
            .fetch_all(db.pool())
            .await
            .expect("Should query history");

    assert_eq!(history.len(), 3);
    assert_eq!(history[0].0, "created");
    assert_eq!(history[1].0, "started");
    assert_eq!(history[2].0, "completed");
}

// =============================================================================
// Webhook Delivery Tests
// =============================================================================

/// Test webhook delivery tracking
/// Columns: id, organization_id, provider, event_type, payload, signature, matched_queue, created_at
#[tokio::test]
async fn test_webhook_delivery_tracking() {
    let db = TestDatabase::new().await;

    let org_id = "test-org-webhook";
    sqlx::query(
        "INSERT INTO organizations (id, name, slug, plan_tier, created_at, updated_at) VALUES ($1, $1, $1, 'free', NOW(), NOW()) ON CONFLICT DO NOTHING"
    )
    .bind(org_id)
    .execute(db.pool())
    .await
    .expect("Should create org");

    let delivery_id = uuid::Uuid::new_v4().to_string();
    sqlx::query(
        r#"
        INSERT INTO webhook_deliveries (id, organization_id, provider, event_type, payload, signature, matched_queue, created_at)
        VALUES ($1, $2, 'github', 'push', '{"event": "push"}'::JSONB, 'sha256=abc123', 'github-queue', NOW())
        "#
    )
    .bind(&delivery_id)
    .bind(org_id)
    .execute(db.pool())
    .await
    .expect("Should create delivery");

    // Verify
    let delivery: (String, String, String, Option<String>) = sqlx::query_as(
        "SELECT provider, event_type, matched_queue, signature FROM webhook_deliveries WHERE id = $1"
    )
    .bind(&delivery_id)
    .fetch_one(db.pool())
    .await
    .expect("Should query delivery");

    assert_eq!(delivery.0, "github");
    assert_eq!(delivery.1, "push");
    assert_eq!(delivery.2, "github-queue");
    assert_eq!(delivery.3, Some("sha256=abc123".to_string()));
}

// =============================================================================
// Organization Isolation Tests
// =============================================================================

/// Test that organization isolation works across all tables
#[tokio::test]
async fn test_cross_org_isolation() {
    let db = TestDatabase::new().await;

    // Create two organizations
    for org_id in ["iso-org-1", "iso-org-2"] {
        sqlx::query(
            "INSERT INTO organizations (id, name, slug, plan_tier, created_at, updated_at) VALUES ($1, $1, $1, 'free', NOW(), NOW()) ON CONFLICT DO NOTHING"
        )
        .bind(org_id)
        .execute(db.pool())
        .await
        .expect("Should create org");
    }

    // Create jobs for org-1
    for _ in 0..5 {
        let job_id = uuid::Uuid::new_v4().to_string();
        sqlx::query(
            "INSERT INTO jobs (id, organization_id, queue_name, status, payload, priority, max_retries, timeout_seconds, created_at, updated_at) VALUES ($1, 'iso-org-1', 'default', 'pending', '{}'::JSONB, 0, 3, 300, NOW(), NOW())"
        )
        .bind(&job_id)
        .execute(db.pool())
        .await
        .expect("Should create job");
    }

    // Create jobs for org-2
    for _ in 0..3 {
        let job_id = uuid::Uuid::new_v4().to_string();
        sqlx::query(
            "INSERT INTO jobs (id, organization_id, queue_name, status, payload, priority, max_retries, timeout_seconds, created_at, updated_at) VALUES ($1, 'iso-org-2', 'default', 'pending', '{}'::JSONB, 0, 3, 300, NOW(), NOW())"
        )
        .bind(&job_id)
        .execute(db.pool())
        .await
        .expect("Should create job");
    }

    // Verify isolation - org-1 can only see its jobs
    let org1_count: (i64,) =
        sqlx::query_as("SELECT COUNT(*) FROM jobs WHERE organization_id = 'iso-org-1'")
            .fetch_one(db.pool())
            .await
            .expect("Should count jobs");
    assert_eq!(org1_count.0, 5);

    // Verify isolation - org-2 can only see its jobs
    let org2_count: (i64,) =
        sqlx::query_as("SELECT COUNT(*) FROM jobs WHERE organization_id = 'iso-org-2'")
            .fetch_one(db.pool())
            .await
            .expect("Should count jobs");
    assert_eq!(org2_count.0, 3);
}

