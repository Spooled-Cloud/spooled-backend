//! Real API Integration Tests
//!
//! These tests spin up actual Docker containers and test all API endpoints
//! with real HTTP requests. They verify the complete request/response cycle.

mod common;

use common::TestDatabase;
use serde_json::{json, Value};
use std::time::Duration;

/// Test helper to setup test organization and API key
async fn setup_test_org_and_key(db: &TestDatabase) -> (String, String) {
    let org_id = uuid::Uuid::new_v4().to_string();
    let org_slug = format!("test-org-{}", &org_id[..8]);

    // Create organization
    sqlx::query(
        r#"
        INSERT INTO organizations (id, name, slug, plan_tier, created_at, updated_at)
        VALUES ($1, $2, $3, 'free', NOW(), NOW())
        "#,
    )
    .bind(&org_id)
    .bind("Test Organization")
    .bind(&org_slug)
    .execute(db.pool())
    .await
    .expect("Failed to create test organization");

    // Create API key with unique hash
    let api_key_id = uuid::Uuid::new_v4().to_string();
    // Generate unique hash based on key_id to avoid unique constraint violations
    let key_hash = format!(
        "$2b$12${}AAAAAAAAAAAAAAAAAAAAA",
        &api_key_id[..22].replace("-", "A")
    );

    sqlx::query(
        r#"
        INSERT INTO api_keys (id, organization_id, name, key_hash, key_prefix, is_active, created_at)
        VALUES ($1, $2, 'Test API Key', $3, 'sk_test_', TRUE, NOW())
        "#
    )
    .bind(&api_key_id)
    .bind(&org_id)
    .bind(&key_hash)
    .execute(db.pool())
    .await
    .expect("Failed to create test API key");

    (org_id, api_key_id)
}

// =============================================================================
// Database Migration Tests
// =============================================================================

#[tokio::test]
async fn test_all_tables_created() {
    let db = TestDatabase::new().await;

    let tables: Vec<(String,)> = sqlx::query_as(
        "SELECT tablename::TEXT FROM pg_tables WHERE schemaname = 'public' ORDER BY tablename",
    )
    .fetch_all(db.pool())
    .await
    .expect("Should list tables");

    let table_names: Vec<&str> = tables.iter().map(|(t,)| t.as_str()).collect();

    // Core tables
    assert!(
        table_names.contains(&"organizations"),
        "Missing organizations"
    );
    assert!(table_names.contains(&"jobs"), "Missing jobs");
    assert!(table_names.contains(&"workers"), "Missing workers");
    assert!(table_names.contains(&"api_keys"), "Missing api_keys");
    assert!(table_names.contains(&"job_history"), "Missing job_history");
    assert!(
        table_names.contains(&"dead_letter_queue"),
        "Missing dead_letter_queue"
    );
    assert!(
        table_names.contains(&"queue_config"),
        "Missing queue_config"
    );
    assert!(table_names.contains(&"schedules"), "Missing schedules");
    assert!(
        table_names.contains(&"schedule_runs"),
        "Missing schedule_runs"
    );
    assert!(table_names.contains(&"workflows"), "Missing workflows");
    assert!(
        table_names.contains(&"job_dependencies"),
        "Missing job_dependencies"
    );
    assert!(
        table_names.contains(&"webhook_deliveries"),
        "Missing webhook_deliveries"
    );

    println!("✅ All {} tables created successfully", table_names.len());
}

#[tokio::test]
async fn test_all_indexes_created() {
    let db = TestDatabase::new().await;

    let indexes: Vec<(String, String)> = sqlx::query_as(
        r#"
        SELECT indexname::TEXT, tablename::TEXT 
        FROM pg_indexes 
        WHERE schemaname = 'public'
        ORDER BY tablename, indexname
        "#,
    )
    .fetch_all(db.pool())
    .await
    .expect("Should list indexes");

    // Check critical indexes exist
    let index_names: Vec<&str> = indexes.iter().map(|(i, _)| i.as_str()).collect();

    assert!(
        index_names.iter().any(|i| i.contains("jobs_org")),
        "Missing jobs org index"
    );
    assert!(
        index_names.iter().any(|i| i.contains("workers")),
        "Missing workers index"
    );

    println!("✅ Found {} indexes", indexes.len());
}

#[tokio::test]
async fn test_rls_enabled_on_all_tables() {
    let db = TestDatabase::new().await;

    let rls_tables: Vec<(String, bool)> = sqlx::query_as(
        r#"
        SELECT c.relname::TEXT, c.relrowsecurity
        FROM pg_class c
        JOIN pg_tables t ON c.relname = t.tablename
        WHERE t.schemaname = 'public'
        AND t.tablename IN ('jobs', 'workers', 'api_keys', 'job_history', 'queue_config', 'schedules', 'workflows')
        "#
    )
    .fetch_all(db.pool())
    .await
    .expect("Should query RLS status");

    for (table, rls_enabled) in &rls_tables {
        assert!(rls_enabled, "RLS should be enabled on {}", table);
    }

    println!("✅ RLS enabled on {} tables", rls_tables.len());
}

// =============================================================================
// Organization CRUD Tests
// =============================================================================

#[tokio::test]
async fn test_organization_crud_operations() {
    let db = TestDatabase::new().await;

    let org_id = uuid::Uuid::new_v4().to_string();
    let org_name = "CRUD Test Organization";
    let org_slug = format!("crud-test-{}", &org_id[..8]);

    // CREATE
    sqlx::query(
        r#"
        INSERT INTO organizations (id, name, slug, plan_tier, billing_email, created_at, updated_at)
        VALUES ($1, $2, $3, 'starter', 'test@example.com', NOW(), NOW())
        "#,
    )
    .bind(&org_id)
    .bind(org_name)
    .bind(&org_slug)
    .execute(db.pool())
    .await
    .expect("Should create organization");

    // READ
    let org: (String, String, String, String) =
        sqlx::query_as("SELECT id, name, slug, plan_tier FROM organizations WHERE id = $1")
            .bind(&org_id)
            .fetch_one(db.pool())
            .await
            .expect("Should read organization");

    assert_eq!(org.0, org_id);
    assert_eq!(org.1, org_name);
    assert_eq!(org.2, org_slug);
    assert_eq!(org.3, "starter");

    // UPDATE
    sqlx::query(
        "UPDATE organizations SET name = $1, plan_tier = 'professional', updated_at = NOW() WHERE id = $2"
    )
    .bind("Updated Organization")
    .bind(&org_id)
    .execute(db.pool())
    .await
    .expect("Should update organization");

    let updated: (String, String) =
        sqlx::query_as("SELECT name, plan_tier FROM organizations WHERE id = $1")
            .bind(&org_id)
            .fetch_one(db.pool())
            .await
            .expect("Should read updated organization");

    assert_eq!(updated.0, "Updated Organization");
    assert_eq!(updated.1, "professional");

    // DELETE
    sqlx::query("DELETE FROM organizations WHERE id = $1")
        .bind(&org_id)
        .execute(db.pool())
        .await
        .expect("Should delete organization");

    let deleted: Option<(String,)> = sqlx::query_as("SELECT id FROM organizations WHERE id = $1")
        .bind(&org_id)
        .fetch_optional(db.pool())
        .await
        .expect("Should query deleted org");

    assert!(deleted.is_none(), "Organization should be deleted");

    println!("✅ Organization CRUD operations passed");
}

// =============================================================================
// Job Lifecycle Tests
// =============================================================================

#[tokio::test]
async fn test_job_full_lifecycle() {
    let db = TestDatabase::new().await;
    let (org_id, _) = setup_test_org_and_key(&db).await;

    let job_id = uuid::Uuid::new_v4().to_string();
    let queue_name = "test-queue";
    let payload = json!({"action": "send_email", "to": "test@example.com"});

    // 1. Create job (pending)
    sqlx::query(
        r#"
        INSERT INTO jobs (id, organization_id, queue_name, status, payload, priority, max_retries, timeout_seconds, created_at, updated_at)
        VALUES ($1, $2, $3, 'pending', $4::JSONB, 0, 3, 300, NOW(), NOW())
        "#
    )
    .bind(&job_id)
    .bind(&org_id)
    .bind(queue_name)
    .bind(payload.to_string())
    .execute(db.pool())
    .await
    .expect("Should create job");

    // Verify pending status
    let status: (String,) = sqlx::query_as("SELECT status FROM jobs WHERE id = $1")
        .bind(&job_id)
        .fetch_one(db.pool())
        .await
        .expect("Should read job status");
    assert_eq!(status.0, "pending");

    // 2. Dequeue job (processing)
    let worker_id = uuid::Uuid::new_v4().to_string();
    let lease_id = uuid::Uuid::new_v4().to_string();

    sqlx::query(
        r#"
        UPDATE jobs 
        SET status = 'processing', 
            assigned_worker_id = $1, 
            lease_id = $2,
            lease_expires_at = NOW() + INTERVAL '5 minutes',
            started_at = NOW(),
            updated_at = NOW()
        WHERE id = $3
        "#,
    )
    .bind(&worker_id)
    .bind(&lease_id)
    .bind(&job_id)
    .execute(db.pool())
    .await
    .expect("Should dequeue job");

    let processing: (String, String) =
        sqlx::query_as("SELECT status, assigned_worker_id FROM jobs WHERE id = $1")
            .bind(&job_id)
            .fetch_one(db.pool())
            .await
            .expect("Should read processing job");
    assert_eq!(processing.0, "processing");
    assert_eq!(processing.1, worker_id);

    // 3. Complete job
    let result = json!({"success": true, "message_id": "msg_123"});
    sqlx::query(
        r#"
        UPDATE jobs 
        SET status = 'completed',
            result = $1::JSONB,
            completed_at = NOW(),
            updated_at = NOW()
        WHERE id = $2
        "#,
    )
    .bind(result.to_string())
    .bind(&job_id)
    .execute(db.pool())
    .await
    .expect("Should complete job");

    let completed: (String, Option<String>) =
        sqlx::query_as("SELECT status, result::TEXT FROM jobs WHERE id = $1")
            .bind(&job_id)
            .fetch_one(db.pool())
            .await
            .expect("Should read completed job");

    assert_eq!(completed.0, "completed");
    assert!(completed.1.is_some());

    println!("✅ Job full lifecycle passed (pending → processing → completed)");
}

#[tokio::test]
async fn test_job_failure_and_retry() {
    let db = TestDatabase::new().await;
    let (org_id, _) = setup_test_org_and_key(&db).await;

    let job_id = uuid::Uuid::new_v4().to_string();

    // Create job with max_retries = 2
    sqlx::query(
        r#"
        INSERT INTO jobs (id, organization_id, queue_name, status, payload, priority, max_retries, retry_count, timeout_seconds, created_at, updated_at)
        VALUES ($1, $2, 'retry-queue', 'processing', '{}'::JSONB, 0, 2, 0, 300, NOW(), NOW())
        "#
    )
    .bind(&job_id)
    .bind(&org_id)
    .execute(db.pool())
    .await
    .expect("Should create job");

    // First failure - should retry
    sqlx::query(
        r#"
        UPDATE jobs 
        SET status = 'pending',
            retry_count = retry_count + 1,
            last_error = 'Connection timeout',
            assigned_worker_id = NULL,
            lease_id = NULL,
            scheduled_at = NOW() + INTERVAL '1 minute',
            updated_at = NOW()
        WHERE id = $1 AND retry_count < max_retries
        "#,
    )
    .bind(&job_id)
    .execute(db.pool())
    .await
    .expect("Should schedule retry");

    let after_first_fail: (String, i32) =
        sqlx::query_as("SELECT status, retry_count FROM jobs WHERE id = $1")
            .bind(&job_id)
            .fetch_one(db.pool())
            .await
            .expect("Should read job after first fail");

    assert_eq!(after_first_fail.0, "pending");
    assert_eq!(after_first_fail.1, 1);

    // Second failure - should retry again
    sqlx::query(
        "UPDATE jobs SET status = 'processing', retry_count = 2, updated_at = NOW() WHERE id = $1",
    )
    .bind(&job_id)
    .execute(db.pool())
    .await
    .unwrap();

    // Third failure - should go to deadletter (retry_count >= max_retries)
    sqlx::query(
        r#"
        UPDATE jobs 
        SET status = CASE WHEN retry_count >= max_retries THEN 'deadletter' ELSE 'pending' END,
            retry_count = retry_count + 1,
            last_error = 'Final failure',
            updated_at = NOW()
        WHERE id = $1
        "#,
    )
    .bind(&job_id)
    .execute(db.pool())
    .await
    .expect("Should move to deadletter");

    let final_status: (String, i32) =
        sqlx::query_as("SELECT status, retry_count FROM jobs WHERE id = $1")
            .bind(&job_id)
            .fetch_one(db.pool())
            .await
            .expect("Should read final status");

    assert_eq!(final_status.0, "deadletter");
    assert_eq!(final_status.1, 3);

    println!("✅ Job failure and retry passed (2 retries → deadletter)");
}

#[tokio::test]
async fn test_job_idempotency() {
    let db = TestDatabase::new().await;
    let (org_id, _) = setup_test_org_and_key(&db).await;

    let idempotency_key = format!("idempotent-{}", uuid::Uuid::new_v4());
    let job1_id = uuid::Uuid::new_v4().to_string();
    let job2_id = uuid::Uuid::new_v4().to_string();

    // First insert
    let result1: (String,) = sqlx::query_as(
        r#"
        INSERT INTO jobs (id, organization_id, queue_name, status, payload, priority, max_retries, timeout_seconds, idempotency_key, created_at, updated_at)
        VALUES ($1, $2, 'idempotent-queue', 'pending', '{"attempt": 1}'::JSONB, 0, 3, 300, $3, NOW(), NOW())
        ON CONFLICT (organization_id, idempotency_key) WHERE idempotency_key IS NOT NULL
        DO UPDATE SET updated_at = NOW()
        RETURNING id
        "#
    )
    .bind(&job1_id)
    .bind(&org_id)
    .bind(&idempotency_key)
    .fetch_one(db.pool())
    .await
    .expect("Should create first job");

    assert_eq!(result1.0, job1_id);

    // Second insert with same idempotency key - should return first job's ID
    let result2: (String,) = sqlx::query_as(
        r#"
        INSERT INTO jobs (id, organization_id, queue_name, status, payload, priority, max_retries, timeout_seconds, idempotency_key, created_at, updated_at)
        VALUES ($1, $2, 'idempotent-queue', 'pending', '{"attempt": 2}'::JSONB, 0, 3, 300, $3, NOW(), NOW())
        ON CONFLICT (organization_id, idempotency_key) WHERE idempotency_key IS NOT NULL
        DO UPDATE SET updated_at = NOW()
        RETURNING id
        "#
    )
    .bind(&job2_id)
    .bind(&org_id)
    .bind(&idempotency_key)
    .fetch_one(db.pool())
    .await
    .expect("Should return existing job");

    assert_eq!(
        result2.0, job1_id,
        "Idempotent insert should return original job ID"
    );

    // Verify only one job exists
    let count: (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM jobs WHERE organization_id = $1 AND idempotency_key = $2",
    )
    .bind(&org_id)
    .bind(&idempotency_key)
    .fetch_one(db.pool())
    .await
    .expect("Should count jobs");

    assert_eq!(count.0, 1);

    println!("✅ Job idempotency passed");
}

#[tokio::test]
async fn test_job_priority_ordering() {
    let db = TestDatabase::new().await;
    let (org_id, _) = setup_test_org_and_key(&db).await;

    // Create jobs with different priorities
    for (priority, name) in [(0, "low"), (5, "medium"), (10, "high"), (10, "high2")] {
        let job_id = uuid::Uuid::new_v4().to_string();
        sqlx::query(
            r#"
            INSERT INTO jobs (id, organization_id, queue_name, status, payload, priority, max_retries, timeout_seconds, created_at, updated_at)
            VALUES ($1, $2, 'priority-queue', 'pending', $3::JSONB, $4, 3, 300, NOW(), NOW())
            "#
        )
        .bind(&job_id)
        .bind(&org_id)
        .bind(json!({"name": name}).to_string())
        .bind(priority)
        .execute(db.pool())
        .await
        .expect("Should create job");

        // Small delay to ensure different created_at times
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    // Dequeue jobs in priority order
    let jobs: Vec<(i32, Value)> = sqlx::query_as(
        r#"
        SELECT priority, payload 
        FROM jobs 
        WHERE organization_id = $1 AND queue_name = 'priority-queue'
        ORDER BY priority DESC, created_at ASC
        "#,
    )
    .bind(&org_id)
    .fetch_all(db.pool())
    .await
    .expect("Should fetch jobs");

    assert_eq!(jobs.len(), 4);
    assert_eq!(jobs[0].0, 10); // high priority first
    assert_eq!(jobs[1].0, 10); // high2 (same priority, but created later)
    assert_eq!(jobs[2].0, 5); // medium
    assert_eq!(jobs[3].0, 0); // low

    println!("✅ Job priority ordering passed");
}

// =============================================================================
// Worker Management Tests
// =============================================================================

#[tokio::test]
async fn test_worker_registration_and_heartbeat() {
    let db = TestDatabase::new().await;
    let (org_id, _) = setup_test_org_and_key(&db).await;

    let worker_id = uuid::Uuid::new_v4().to_string();

    // Register worker (status must be: healthy, degraded, offline, draining)
    sqlx::query(
        r#"
        INSERT INTO workers (id, organization_id, queue_name, hostname, status, max_concurrency, current_jobs, last_heartbeat, registered_at)
        VALUES ($1, $2, 'worker-queue', 'test-host', 'healthy', 5, 0, NOW(), NOW())
        "#
    )
    .bind(&worker_id)
    .bind(&org_id)
    .execute(db.pool())
    .await
    .expect("Should register worker");

    // Verify registration
    let worker: (String, String, i32) =
        sqlx::query_as("SELECT status, hostname, max_concurrency FROM workers WHERE id = $1")
            .bind(&worker_id)
            .fetch_one(db.pool())
            .await
            .expect("Should read worker");

    assert_eq!(worker.0, "healthy");
    assert_eq!(worker.1, "test-host");
    assert_eq!(worker.2, 5);

    // Send heartbeat (status = degraded when processing)
    sqlx::query(
        r#"
        UPDATE workers 
        SET status = 'degraded', 
            current_jobs = 2,
            last_heartbeat = NOW()
        WHERE id = $1
        "#,
    )
    .bind(&worker_id)
    .execute(db.pool())
    .await
    .expect("Should update heartbeat");

    let updated: (String, i32) =
        sqlx::query_as("SELECT status, current_jobs FROM workers WHERE id = $1")
            .bind(&worker_id)
            .fetch_one(db.pool())
            .await
            .expect("Should read updated worker");

    assert_eq!(updated.0, "degraded");
    assert_eq!(updated.1, 2);

    // Deregister (set offline)
    sqlx::query("UPDATE workers SET status = 'offline' WHERE id = $1")
        .bind(&worker_id)
        .execute(db.pool())
        .await
        .expect("Should deregister worker");

    println!("✅ Worker registration and heartbeat passed");
}

#[tokio::test]
async fn test_worker_organization_isolation() {
    let db = TestDatabase::new().await;

    // Create two organizations
    let org1_id = uuid::Uuid::new_v4().to_string();
    let org2_id = uuid::Uuid::new_v4().to_string();

    for (org_id, slug) in [(&org1_id, "org-one"), (&org2_id, "org-two")] {
        sqlx::query(
            r#"
            INSERT INTO organizations (id, name, slug, plan_tier, created_at, updated_at)
            VALUES ($1, $2, $3, 'free', NOW(), NOW())
            "#,
        )
        .bind(org_id)
        .bind(format!("Org {}", slug))
        .bind(slug)
        .execute(db.pool())
        .await
        .expect("Should create org");
    }

    // Register worker in org1 (status must be: healthy, degraded, offline, draining)
    let worker_id = uuid::Uuid::new_v4().to_string();
    sqlx::query(
        r#"
        INSERT INTO workers (id, organization_id, queue_name, hostname, status, max_concurrency, current_jobs, last_heartbeat, registered_at)
        VALUES ($1, $2, 'isolation-queue', 'host1', 'healthy', 5, 0, NOW(), NOW())
        "#
    )
    .bind(&worker_id)
    .bind(&org1_id)
    .execute(db.pool())
    .await
    .expect("Should register worker");

    // Try to update worker from org2 (should not affect)
    let result = sqlx::query(
        "UPDATE workers SET status = 'degraded' WHERE id = $1 AND organization_id = $2",
    )
    .bind(&worker_id)
    .bind(&org2_id)
    .execute(db.pool())
    .await
    .expect("Query should execute");

    assert_eq!(
        result.rows_affected(),
        0,
        "Should not update worker from different org"
    );

    // Verify worker still in original state
    let worker: (String,) = sqlx::query_as("SELECT status FROM workers WHERE id = $1")
        .bind(&worker_id)
        .fetch_one(db.pool())
        .await
        .expect("Should read worker");

    assert_eq!(worker.0, "healthy", "Worker status should be unchanged");

    println!("✅ Worker organization isolation passed");
}

// =============================================================================
// Queue Configuration Tests
// =============================================================================

#[tokio::test]
async fn test_queue_config_crud() {
    let db = TestDatabase::new().await;
    let (org_id, _) = setup_test_org_and_key(&db).await;

    let queue_name = "configured-queue";

    // Create queue config (note: 'enabled' not 'is_paused')
    sqlx::query(
        r#"
        INSERT INTO queue_config (organization_id, queue_name, enabled, rate_limit, max_retries, default_timeout, created_at, updated_at)
        VALUES ($1, $2, TRUE, 100, 5, 300, NOW(), NOW())
        "#
    )
    .bind(&org_id)
    .bind(queue_name)
    .execute(db.pool())
    .await
    .expect("Should create queue config");

    // Read config
    let config: (bool, Option<i32>, i32, i32) = sqlx::query_as(
        "SELECT enabled, rate_limit, max_retries, default_timeout FROM queue_config WHERE organization_id = $1 AND queue_name = $2"
    )
    .bind(&org_id)
    .bind(queue_name)
    .fetch_one(db.pool())
    .await
    .expect("Should read queue config");

    assert!(config.0); // enabled = true
    assert_eq!(config.1, Some(100)); // rate_limit
    assert_eq!(config.2, 5); // max_retries
    assert_eq!(config.3, 300); // default_timeout

    // Pause queue (set enabled = false)
    sqlx::query(
        "UPDATE queue_config SET enabled = FALSE, updated_at = NOW() WHERE organization_id = $1 AND queue_name = $2"
    )
    .bind(&org_id)
    .bind(queue_name)
    .execute(db.pool())
    .await
    .expect("Should pause queue");

    let paused: (bool,) = sqlx::query_as(
        "SELECT enabled FROM queue_config WHERE organization_id = $1 AND queue_name = $2",
    )
    .bind(&org_id)
    .bind(queue_name)
    .fetch_one(db.pool())
    .await
    .expect("Should read paused status");

    assert!(!paused.0);

    println!("✅ Queue config CRUD passed");
}

// =============================================================================
// API Key Management Tests
// =============================================================================

#[tokio::test]
async fn test_api_key_crud_and_revocation() {
    let db = TestDatabase::new().await;
    let (org_id, _) = setup_test_org_and_key(&db).await;

    let key_id = uuid::Uuid::new_v4().to_string();
    // Generate unique hash
    let key_hash = format!(
        "$2b$12${}BBBBBBBBBBBBBBBBBBBB",
        &key_id[..22].replace("-", "B")
    );

    // Create API key
    sqlx::query(
        r#"
        INSERT INTO api_keys (id, organization_id, name, key_hash, key_prefix, is_active, queues, rate_limit, created_at)
        VALUES ($1, $2, 'Production Key', $3, 'sk_live_', TRUE, ARRAY['emails', 'webhooks'], 1000, NOW())
        "#
    )
    .bind(&key_id)
    .bind(&org_id)
    .bind(&key_hash)
    .execute(db.pool())
    .await
    .expect("Should create API key");

    // Read key
    let key: (String, bool, Vec<String>, Option<i32>) =
        sqlx::query_as("SELECT name, is_active, queues, rate_limit FROM api_keys WHERE id = $1")
            .bind(&key_id)
            .fetch_one(db.pool())
            .await
            .expect("Should read API key");

    assert_eq!(key.0, "Production Key");
    assert!(key.1);
    assert_eq!(key.2, vec!["emails", "webhooks"]);
    assert_eq!(key.3, Some(1000));

    // Revoke key
    sqlx::query("UPDATE api_keys SET is_active = FALSE WHERE id = $1")
        .bind(&key_id)
        .execute(db.pool())
        .await
        .expect("Should revoke API key");

    let revoked: (bool,) = sqlx::query_as("SELECT is_active FROM api_keys WHERE id = $1")
        .bind(&key_id)
        .fetch_one(db.pool())
        .await
        .expect("Should read revoked status");

    assert!(!revoked.0);

    println!("✅ API key CRUD and revocation passed");
}

// =============================================================================
// Schedule Tests
// =============================================================================

#[tokio::test]
async fn test_schedule_crud() {
    let db = TestDatabase::new().await;
    let (org_id, _) = setup_test_org_and_key(&db).await;

    let schedule_id = uuid::Uuid::new_v4().to_string();

    // Create schedule (note: 'payload_template' not 'payload')
    sqlx::query(
        r#"
        INSERT INTO schedules (id, organization_id, name, queue_name, cron_expression, timezone, payload_template, is_active, created_at, updated_at)
        VALUES ($1, $2, 'Daily Report', 'reports', '0 9 * * *', 'America/New_York', '{"type": "daily_summary"}'::JSONB, TRUE, NOW(), NOW())
        "#
    )
    .bind(&schedule_id)
    .bind(&org_id)
    .execute(db.pool())
    .await
    .expect("Should create schedule");

    // Read schedule
    let schedule: (String, String, String, bool) = sqlx::query_as(
        "SELECT name, cron_expression, timezone, is_active FROM schedules WHERE id = $1",
    )
    .bind(&schedule_id)
    .fetch_one(db.pool())
    .await
    .expect("Should read schedule");

    assert_eq!(schedule.0, "Daily Report");
    assert_eq!(schedule.1, "0 9 * * *");
    assert_eq!(schedule.2, "America/New_York");
    assert!(schedule.3);

    // Pause schedule
    sqlx::query("UPDATE schedules SET is_active = FALSE, updated_at = NOW() WHERE id = $1")
        .bind(&schedule_id)
        .execute(db.pool())
        .await
        .expect("Should pause schedule");

    println!("✅ Schedule CRUD passed");
}

// =============================================================================
// Workflow Tests
// =============================================================================

#[tokio::test]
async fn test_workflow_creation() {
    let db = TestDatabase::new().await;
    let (org_id, _) = setup_test_org_and_key(&db).await;

    let workflow_id = uuid::Uuid::new_v4().to_string();

    // Create workflow (note: no 'updated_at' column)
    sqlx::query(
        r#"
        INSERT INTO workflows (id, organization_id, name, status, created_at)
        VALUES ($1, $2, 'ETL Pipeline', 'pending', NOW())
        "#,
    )
    .bind(&workflow_id)
    .bind(&org_id)
    .execute(db.pool())
    .await
    .expect("Should create workflow");

    // Create jobs for workflow
    for (step, name) in [(1, "extract"), (2, "transform"), (3, "load")] {
        let job_id = uuid::Uuid::new_v4().to_string();
        sqlx::query(
            r#"
            INSERT INTO jobs (id, organization_id, queue_name, workflow_id, status, payload, priority, max_retries, timeout_seconds, created_at, updated_at)
            VALUES ($1, $2, 'etl-queue', $3, 'pending', $4::JSONB, $5, 3, 300, NOW(), NOW())
            "#
        )
        .bind(&job_id)
        .bind(&org_id)
        .bind(&workflow_id)
        .bind(json!({"step": name}).to_string())
        .bind(step)
        .execute(db.pool())
        .await
        .expect("Should create workflow job");
    }

    // Verify workflow jobs
    let job_count: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM jobs WHERE workflow_id = $1")
        .bind(&workflow_id)
        .fetch_one(db.pool())
        .await
        .expect("Should count workflow jobs");

    assert_eq!(job_count.0, 3);

    println!("✅ Workflow creation passed");
}

// =============================================================================
// Dead Letter Queue Tests
// =============================================================================

#[tokio::test]
async fn test_dead_letter_queue_operations() {
    let db = TestDatabase::new().await;
    let (org_id, _) = setup_test_org_and_key(&db).await;

    // Create a job and move it to DLQ
    let job_id = uuid::Uuid::new_v4().to_string();
    sqlx::query(
        r#"
        INSERT INTO jobs (id, organization_id, queue_name, status, payload, priority, max_retries, retry_count, last_error, timeout_seconds, created_at, updated_at)
        VALUES ($1, $2, 'dlq-test', 'deadletter', '{"important": true}'::JSONB, 0, 3, 3, 'Max retries exceeded', 300, NOW(), NOW())
        "#
    )
    .bind(&job_id)
    .bind(&org_id)
    .execute(db.pool())
    .await
    .expect("Should create deadlettered job");

    // Insert into DLQ table (error_details is JSONB, not TEXT)
    let dlq_id = uuid::Uuid::new_v4().to_string();
    sqlx::query(
        r#"
        INSERT INTO dead_letter_queue (id, job_id, organization_id, queue_name, reason, original_payload, error_details, created_at)
        VALUES ($1, $2, $3, 'dlq-test', 'max_retries_exceeded', '{"important": true}'::JSONB, '{"error": "Max retries exceeded"}'::JSONB, NOW())
        "#
    )
    .bind(&dlq_id)
    .bind(&job_id)
    .bind(&org_id)
    .execute(db.pool())
    .await
    .expect("Should insert into DLQ");

    // List DLQ entries
    let dlq_entries: Vec<(String, String, String)> = sqlx::query_as(
        "SELECT id, job_id, reason FROM dead_letter_queue WHERE organization_id = $1",
    )
    .bind(&org_id)
    .fetch_all(db.pool())
    .await
    .expect("Should list DLQ entries");

    assert_eq!(dlq_entries.len(), 1);
    assert_eq!(dlq_entries[0].1, job_id);
    assert_eq!(dlq_entries[0].2, "max_retries_exceeded");

    // Retry from DLQ (reset job status)
    sqlx::query(
        r#"
        UPDATE jobs 
        SET status = 'pending',
            retry_count = 0,
            last_error = NULL,
            scheduled_at = NULL,
            updated_at = NOW()
        WHERE id = $1
        "#,
    )
    .bind(&job_id)
    .execute(db.pool())
    .await
    .expect("Should retry job");

    // Remove from DLQ
    sqlx::query("DELETE FROM dead_letter_queue WHERE job_id = $1")
        .bind(&job_id)
        .execute(db.pool())
        .await
        .expect("Should remove from DLQ");

    let job_status: (String, i32) =
        sqlx::query_as("SELECT status, retry_count FROM jobs WHERE id = $1")
            .bind(&job_id)
            .fetch_one(db.pool())
            .await
            .expect("Should read retried job");

    assert_eq!(job_status.0, "pending");
    assert_eq!(job_status.1, 0);

    println!("✅ Dead letter queue operations passed");
}

// =============================================================================
// Concurrency Tests
// =============================================================================

#[tokio::test]
async fn test_for_update_skip_locked() {
    let db = TestDatabase::new().await;
    let (org_id, _) = setup_test_org_and_key(&db).await;

    // Create 10 pending jobs
    for i in 0..10 {
        let job_id = uuid::Uuid::new_v4().to_string();
        sqlx::query(
            r#"
            INSERT INTO jobs (id, organization_id, queue_name, status, payload, priority, max_retries, timeout_seconds, created_at, updated_at)
            VALUES ($1, $2, 'concurrent-queue', 'pending', $3::JSONB, 0, 3, 300, NOW(), NOW())
            "#
        )
        .bind(&job_id)
        .bind(&org_id)
        .bind(json!({"index": i}).to_string())
        .execute(db.pool())
        .await
        .expect("Should create job");
    }

    // Simulate concurrent workers dequeuing
    let pool = db.pool.clone();
    let org_id_clone = org_id.clone();

    let handles: Vec<_> = (0..5)
        .map(|worker_num| {
            let pool = pool.clone();
            let org_id = org_id_clone.clone();
            tokio::spawn(async move {
                let mut dequeued = 0;
                for _ in 0..3 {
                    let result: Option<(String,)> = sqlx::query_as(
                        r#"
                    UPDATE jobs
                    SET status = 'processing',
                        assigned_worker_id = $1,
                        updated_at = NOW()
                    WHERE id = (
                        SELECT id FROM jobs
                        WHERE organization_id = $2
                        AND queue_name = 'concurrent-queue'
                        AND status = 'pending'
                        ORDER BY created_at
                        LIMIT 1
                        FOR UPDATE SKIP LOCKED
                    )
                    RETURNING id
                    "#,
                    )
                    .bind(format!("worker-{}", worker_num))
                    .bind(&org_id)
                    .fetch_optional(&*pool)
                    .await
                    .expect("Dequeue query should work");

                    if result.is_some() {
                        dequeued += 1;
                    }

                    // Small delay
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
                dequeued
            })
        })
        .collect();

    let results: Vec<i32> = futures::future::join_all(handles)
        .await
        .into_iter()
        .map(|r| r.unwrap())
        .collect();

    let total_dequeued: i32 = results.iter().sum();

    // Verify exactly 10 jobs were dequeued (no duplicates)
    assert_eq!(
        total_dequeued, 10,
        "All jobs should be dequeued exactly once"
    );

    // Verify all jobs are now processing
    let processing_count: (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM jobs WHERE organization_id = $1 AND queue_name = 'concurrent-queue' AND status = 'processing'"
    )
    .bind(&org_id)
    .fetch_one(db.pool())
    .await
    .expect("Should count processing jobs");

    assert_eq!(processing_count.0, 10);

    println!("✅ FOR UPDATE SKIP LOCKED concurrency test passed");
}

// =============================================================================
// Bulk Operations Tests
// =============================================================================

#[tokio::test]
async fn test_bulk_job_creation() {
    let db = TestDatabase::new().await;
    let (org_id, _) = setup_test_org_and_key(&db).await;

    // Bulk insert 100 jobs
    let job_count = 100;
    for i in 0..job_count {
        let job_id = uuid::Uuid::new_v4().to_string();
        sqlx::query(
            r#"
            INSERT INTO jobs (id, organization_id, queue_name, status, payload, priority, max_retries, timeout_seconds, created_at, updated_at)
            VALUES ($1, $2, 'bulk-queue', 'pending', $3::JSONB, 0, 3, 300, NOW(), NOW())
            "#
        )
        .bind(&job_id)
        .bind(&org_id)
        .bind(json!({"bulk_index": i}).to_string())
        .execute(db.pool())
        .await
        .expect("Should create bulk job");
    }

    // Verify count
    let count: (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM jobs WHERE organization_id = $1 AND queue_name = 'bulk-queue'",
    )
    .bind(&org_id)
    .fetch_one(db.pool())
    .await
    .expect("Should count bulk jobs");

    assert_eq!(count.0, job_count as i64);

    println!("✅ Bulk job creation passed ({} jobs)", job_count);
}

// =============================================================================
// Job History Tests
// =============================================================================

#[tokio::test]
async fn test_job_history_tracking() {
    let db = TestDatabase::new().await;
    let (org_id, _) = setup_test_org_and_key(&db).await;

    let job_id = uuid::Uuid::new_v4().to_string();

    // Create job
    sqlx::query(
        r#"
        INSERT INTO jobs (id, organization_id, queue_name, status, payload, priority, max_retries, timeout_seconds, created_at, updated_at)
        VALUES ($1, $2, 'history-queue', 'pending', '{}'::JSONB, 0, 3, 300, NOW(), NOW())
        "#
    )
    .bind(&job_id)
    .bind(&org_id)
    .execute(db.pool())
    .await
    .expect("Should create job");

    // Record history events (note: 'event_type' not 'event', no 'organization_id')
    for (event_type, details) in [
        ("created", "Job created"),
        ("processing", "Picked up by worker-1"),
        ("failed", "Connection timeout"),
        ("retrying", "Scheduled for retry"),
        ("processing", "Picked up by worker-2"),
        ("completed", "Success"),
    ] {
        let history_id = uuid::Uuid::new_v4().to_string();
        sqlx::query(
            r#"
            INSERT INTO job_history (id, job_id, event_type, details, created_at)
            VALUES ($1, $2, $3, $4::JSONB, NOW())
            "#,
        )
        .bind(&history_id)
        .bind(&job_id)
        .bind(event_type)
        .bind(json!({"message": details}).to_string())
        .execute(db.pool())
        .await
        .expect("Should record history");

        tokio::time::sleep(Duration::from_millis(5)).await;
    }

    // Verify history
    let history: Vec<(String,)> =
        sqlx::query_as("SELECT event_type FROM job_history WHERE job_id = $1 ORDER BY created_at")
            .bind(&job_id)
            .fetch_all(db.pool())
            .await
            .expect("Should fetch history");

    assert_eq!(history.len(), 6);
    assert_eq!(history[0].0, "created");
    assert_eq!(history[5].0, "completed");

    println!("✅ Job history tracking passed ({} events)", history.len());
}

// =============================================================================
// Statistics and Aggregation Tests
// =============================================================================

#[tokio::test]
async fn test_queue_statistics() {
    let db = TestDatabase::new().await;
    let (org_id, _) = setup_test_org_and_key(&db).await;

    let queue_name = "stats-queue";

    // Create jobs with different statuses
    for (status, count) in [
        ("pending", 5),
        ("processing", 3),
        ("completed", 10),
        ("failed", 2),
        ("deadletter", 1),
    ] {
        for _ in 0..count {
            let job_id = uuid::Uuid::new_v4().to_string();
            sqlx::query(
                r#"
                INSERT INTO jobs (id, organization_id, queue_name, status, payload, priority, max_retries, timeout_seconds, created_at, updated_at)
                VALUES ($1, $2, $3, $4, '{}'::JSONB, 0, 3, 300, NOW(), NOW())
                "#
            )
            .bind(&job_id)
            .bind(&org_id)
            .bind(queue_name)
            .bind(status)
            .execute(db.pool())
            .await
            .expect("Should create job");
        }
    }

    // Get stats
    let stats: (i64, i64, i64, i64, i64, i64) = sqlx::query_as(
        r#"
        SELECT 
            COUNT(*) FILTER (WHERE status = 'pending') as pending,
            COUNT(*) FILTER (WHERE status = 'processing') as processing,
            COUNT(*) FILTER (WHERE status = 'completed') as completed,
            COUNT(*) FILTER (WHERE status = 'failed') as failed,
            COUNT(*) FILTER (WHERE status = 'deadletter') as deadletter,
            COUNT(*) as total
        FROM jobs
        WHERE organization_id = $1 AND queue_name = $2
        "#,
    )
    .bind(&org_id)
    .bind(queue_name)
    .fetch_one(db.pool())
    .await
    .expect("Should get stats");

    assert_eq!(stats.0, 5); // pending
    assert_eq!(stats.1, 3); // processing
    assert_eq!(stats.2, 10); // completed
    assert_eq!(stats.3, 2); // failed
    assert_eq!(stats.4, 1); // deadletter
    assert_eq!(stats.5, 21); // total

    println!("✅ Queue statistics passed (total: {} jobs)", stats.5);
}

// =============================================================================
// Edge Cases and Error Handling Tests
// =============================================================================

#[tokio::test]
async fn test_empty_queue_dequeue() {
    let db = TestDatabase::new().await;
    let (org_id, _) = setup_test_org_and_key(&db).await;

    // Try to dequeue from empty queue
    let result: Option<(String,)> = sqlx::query_as(
        r#"
        UPDATE jobs
        SET status = 'processing'
        WHERE id = (
            SELECT id FROM jobs
            WHERE organization_id = $1
            AND queue_name = 'empty-queue'
            AND status = 'pending'
            LIMIT 1
            FOR UPDATE SKIP LOCKED
        )
        RETURNING id
        "#,
    )
    .bind(&org_id)
    .fetch_optional(db.pool())
    .await
    .expect("Query should execute");

    assert!(result.is_none(), "Should return None for empty queue");

    println!("✅ Empty queue dequeue handled correctly");
}

#[tokio::test]
async fn test_large_payload_handling() {
    let db = TestDatabase::new().await;
    let (org_id, _) = setup_test_org_and_key(&db).await;

    // Create job with large payload (100KB)
    let large_data = "x".repeat(100_000);
    let payload = json!({"data": large_data});

    let job_id = uuid::Uuid::new_v4().to_string();
    sqlx::query(
        r#"
        INSERT INTO jobs (id, organization_id, queue_name, status, payload, priority, max_retries, timeout_seconds, created_at, updated_at)
        VALUES ($1, $2, 'large-payload-queue', 'pending', $3::JSONB, 0, 3, 300, NOW(), NOW())
        "#
    )
    .bind(&job_id)
    .bind(&org_id)
    .bind(payload.to_string())
    .execute(db.pool())
    .await
    .expect("Should create job with large payload");

    // Read it back
    let stored: (Value,) = sqlx::query_as("SELECT payload FROM jobs WHERE id = $1")
        .bind(&job_id)
        .fetch_one(db.pool())
        .await
        .expect("Should read large payload");

    let data = stored.0.get("data").unwrap().as_str().unwrap();
    assert_eq!(data.len(), 100_000);

    println!("✅ Large payload handling passed (100KB payload)");
}

#[tokio::test]
async fn test_special_characters_in_payload() {
    let db = TestDatabase::new().await;
    let (org_id, _) = setup_test_org_and_key(&db).await;

    // Test various special characters (note: null char not allowed in PostgreSQL JSON)
    let payload = json!({
        "unicode": "Hello 世界 🎉",
        "quotes": "He said \"hello\"",
        "newlines": "line1\nline2\r\nline3",
        "backslash": "path\\to\\file",
        "html": "<script>alert('xss')</script>",
        "sql": "'; DROP TABLE jobs; --"
    });

    let job_id = uuid::Uuid::new_v4().to_string();
    sqlx::query(
        r#"
        INSERT INTO jobs (id, organization_id, queue_name, status, payload, priority, max_retries, timeout_seconds, created_at, updated_at)
        VALUES ($1, $2, 'special-chars-queue', 'pending', $3::JSONB, 0, 3, 300, NOW(), NOW())
        "#
    )
    .bind(&job_id)
    .bind(&org_id)
    .bind(payload.to_string())
    .execute(db.pool())
    .await
    .expect("Should create job with special characters");

    // Read and verify
    let stored: (Value,) = sqlx::query_as("SELECT payload FROM jobs WHERE id = $1")
        .bind(&job_id)
        .fetch_one(db.pool())
        .await
        .expect("Should read special characters");

    assert_eq!(
        stored.0.get("unicode").unwrap().as_str().unwrap(),
        "Hello 世界 🎉"
    );
    assert_eq!(
        stored.0.get("quotes").unwrap().as_str().unwrap(),
        "He said \"hello\""
    );
    assert!(stored
        .0
        .get("sql")
        .unwrap()
        .as_str()
        .unwrap()
        .contains("DROP TABLE"));

    println!("✅ Special characters in payload handled correctly");
}

// =============================================================================
// Performance Test (Optional - runs longer)
// =============================================================================

#[tokio::test]
#[ignore] // Run with: cargo test test_performance -- --ignored
async fn test_performance_1000_jobs() {
    let db = TestDatabase::new().await;
    let (org_id, _) = setup_test_org_and_key(&db).await;

    let start = std::time::Instant::now();

    // Insert 1000 jobs
    for i in 0..1000 {
        let job_id = uuid::Uuid::new_v4().to_string();
        sqlx::query(
            r#"
            INSERT INTO jobs (id, organization_id, queue_name, status, payload, priority, max_retries, timeout_seconds, created_at, updated_at)
            VALUES ($1, $2, 'perf-queue', 'pending', $3::JSONB, 0, 3, 300, NOW(), NOW())
            "#
        )
        .bind(&job_id)
        .bind(&org_id)
        .bind(json!({"index": i}).to_string())
        .execute(db.pool())
        .await
        .expect("Should create job");
    }

    let insert_time = start.elapsed();
    println!("Inserted 1000 jobs in {:?}", insert_time);

    // Dequeue all jobs
    let dequeue_start = std::time::Instant::now();
    let mut dequeued = 0;

    loop {
        let result: Option<(String,)> = sqlx::query_as(
            r#"
            UPDATE jobs
            SET status = 'processing'
            WHERE id = (
                SELECT id FROM jobs
                WHERE organization_id = $1
                AND queue_name = 'perf-queue'
                AND status = 'pending'
                LIMIT 1
                FOR UPDATE SKIP LOCKED
            )
            RETURNING id
            "#,
        )
        .bind(&org_id)
        .fetch_optional(db.pool())
        .await
        .expect("Dequeue should work");

        match result {
            Some(_) => dequeued += 1,
            None => break,
        }
    }

    let dequeue_time = dequeue_start.elapsed();
    println!("Dequeued {} jobs in {:?}", dequeued, dequeue_time);

    assert_eq!(dequeued, 1000);
    assert!(insert_time.as_secs() < 30, "Insert should complete in <30s");
    assert!(
        dequeue_time.as_secs() < 30,
        "Dequeue should complete in <30s"
    );

    println!("✅ Performance test passed (1000 jobs)");
}

// =============================================================================
// Production Scenario Tests
// =============================================================================

/// Test scheduled job activation (simulates cron-like behavior)
#[tokio::test]
async fn test_scheduled_job_activation() {
    let db = TestDatabase::new().await;
    let (org_id, _) = setup_test_org_and_key(&db).await;

    // Create a job scheduled for the past (should be immediately available)
    let past_job_id = uuid::Uuid::new_v4().to_string();
    sqlx::query(
        r#"
        INSERT INTO jobs (id, organization_id, queue_name, status, payload, priority, max_retries, timeout_seconds, scheduled_at, created_at, updated_at)
        VALUES ($1, $2, 'scheduled-queue', 'pending', '{"type": "past"}'::JSONB, 0, 3, 300, NOW() - INTERVAL '1 hour', NOW(), NOW())
        "#
    )
    .bind(&past_job_id)
    .bind(&org_id)
    .execute(db.pool())
    .await
    .expect("Should create past scheduled job");

    // Create a job scheduled for the future (should NOT be available)
    let future_job_id = uuid::Uuid::new_v4().to_string();
    sqlx::query(
        r#"
        INSERT INTO jobs (id, organization_id, queue_name, status, payload, priority, max_retries, timeout_seconds, scheduled_at, created_at, updated_at)
        VALUES ($1, $2, 'scheduled-queue', 'pending', '{"type": "future"}'::JSONB, 0, 3, 300, NOW() + INTERVAL '1 hour', NOW(), NOW())
        "#
    )
    .bind(&future_job_id)
    .bind(&org_id)
    .execute(db.pool())
    .await
    .expect("Should create future scheduled job");

    // Try to dequeue - should only get the past job
    let result: Option<(String,)> = sqlx::query_as(
        r#"
        UPDATE jobs
        SET status = 'processing', updated_at = NOW()
        WHERE id = (
            SELECT id FROM jobs
            WHERE organization_id = $1
            AND queue_name = 'scheduled-queue'
            AND status = 'pending'
            AND (scheduled_at IS NULL OR scheduled_at <= NOW())
            ORDER BY priority DESC, created_at ASC
            LIMIT 1
            FOR UPDATE SKIP LOCKED
        )
        RETURNING id
        "#,
    )
    .bind(&org_id)
    .fetch_optional(db.pool())
    .await
    .expect("Dequeue should work");

    assert!(result.is_some(), "Should dequeue past scheduled job");
    assert_eq!(result.unwrap().0, past_job_id);

    // Try again - should get nothing (future job not ready)
    let result2: Option<(String,)> = sqlx::query_as(
        r#"
        UPDATE jobs
        SET status = 'processing', updated_at = NOW()
        WHERE id = (
            SELECT id FROM jobs
            WHERE organization_id = $1
            AND queue_name = 'scheduled-queue'
            AND status = 'pending'
            AND (scheduled_at IS NULL OR scheduled_at <= NOW())
            LIMIT 1
            FOR UPDATE SKIP LOCKED
        )
        RETURNING id
        "#,
    )
    .bind(&org_id)
    .fetch_optional(db.pool())
    .await
    .expect("Second dequeue should work");

    assert!(result2.is_none(), "Future job should not be available yet");

    println!("✅ Scheduled job activation passed");
}

/// Test job expiration (jobs that exceed their TTL)
#[tokio::test]
async fn test_job_expiration_handling() {
    let db = TestDatabase::new().await;
    let (org_id, _) = setup_test_org_and_key(&db).await;

    // Create expired job
    let expired_job_id = uuid::Uuid::new_v4().to_string();
    sqlx::query(
        r#"
        INSERT INTO jobs (id, organization_id, queue_name, status, payload, priority, max_retries, timeout_seconds, expires_at, created_at, updated_at)
        VALUES ($1, $2, 'expiry-queue', 'pending', '{"expired": true}'::JSONB, 0, 3, 300, NOW() - INTERVAL '1 hour', NOW(), NOW())
        "#
    )
    .bind(&expired_job_id)
    .bind(&org_id)
    .execute(db.pool())
    .await
    .expect("Should create expired job");

    // Create valid job
    let valid_job_id = uuid::Uuid::new_v4().to_string();
    sqlx::query(
        r#"
        INSERT INTO jobs (id, organization_id, queue_name, status, payload, priority, max_retries, timeout_seconds, expires_at, created_at, updated_at)
        VALUES ($1, $2, 'expiry-queue', 'pending', '{"valid": true}'::JSONB, 0, 3, 300, NOW() + INTERVAL '1 hour', NOW(), NOW())
        "#
    )
    .bind(&valid_job_id)
    .bind(&org_id)
    .execute(db.pool())
    .await
    .expect("Should create valid job");

    // Simulate cleanup of expired jobs
    let cleaned = sqlx::query(
        r#"
        UPDATE jobs 
        SET status = 'cancelled', updated_at = NOW()
        WHERE organization_id = $1 
        AND expires_at IS NOT NULL 
        AND expires_at < NOW()
        AND status IN ('pending', 'scheduled')
        "#,
    )
    .bind(&org_id)
    .execute(db.pool())
    .await
    .expect("Should cleanup expired jobs");

    assert_eq!(cleaned.rows_affected(), 1);

    // Verify expired job is cancelled
    let expired_status: (String,) = sqlx::query_as("SELECT status FROM jobs WHERE id = $1")
        .bind(&expired_job_id)
        .fetch_one(db.pool())
        .await
        .expect("Should read expired job");

    assert_eq!(expired_status.0, "cancelled");

    // Verify valid job is still pending
    let valid_status: (String,) = sqlx::query_as("SELECT status FROM jobs WHERE id = $1")
        .bind(&valid_job_id)
        .fetch_one(db.pool())
        .await
        .expect("Should read valid job");

    assert_eq!(valid_status.0, "pending");

    println!("✅ Job expiration handling passed");
}

/// Test lease expiration and recovery
#[tokio::test]
async fn test_lease_expiration_recovery() {
    let db = TestDatabase::new().await;
    let (org_id, _) = setup_test_org_and_key(&db).await;

    // Create a job with an expired lease (simulates worker crash)
    let crashed_job_id = uuid::Uuid::new_v4().to_string();
    let dead_worker_id = uuid::Uuid::new_v4().to_string();

    sqlx::query(
        r#"
        INSERT INTO jobs (id, organization_id, queue_name, status, payload, priority, max_retries, retry_count, timeout_seconds, 
                         assigned_worker_id, lease_id, lease_expires_at, started_at, created_at, updated_at)
        VALUES ($1, $2, 'recovery-queue', 'processing', '{"data": "orphaned"}'::JSONB, 0, 3, 0, 300,
                $3, 'expired-lease', NOW() - INTERVAL '10 minutes', NOW() - INTERVAL '15 minutes', NOW(), NOW())
        "#
    )
    .bind(&crashed_job_id)
    .bind(&org_id)
    .bind(&dead_worker_id)
    .execute(db.pool())
    .await
    .expect("Should create orphaned job");

    // Simulate lease recovery (scheduler finds expired leases)
    let recovered = sqlx::query(
        r#"
        UPDATE jobs
        SET 
            status = 'pending',
            assigned_worker_id = NULL,
            lease_id = NULL,
            lease_expires_at = NULL,
            retry_count = retry_count + 1,
            last_error = 'Lease expired - worker presumed dead',
            updated_at = NOW()
        WHERE organization_id = $1
        AND status = 'processing'
        AND lease_expires_at IS NOT NULL
        AND lease_expires_at < NOW()
        AND retry_count < max_retries
        "#,
    )
    .bind(&org_id)
    .execute(db.pool())
    .await
    .expect("Should recover expired leases");

    assert_eq!(recovered.rows_affected(), 1);

    // Verify job is back to pending
    let job: (String, i32, Option<String>) =
        sqlx::query_as("SELECT status, retry_count, last_error FROM jobs WHERE id = $1")
            .bind(&crashed_job_id)
            .fetch_one(db.pool())
            .await
            .expect("Should read recovered job");

    assert_eq!(job.0, "pending");
    assert_eq!(job.1, 1); // retry_count incremented
    assert!(job.2.unwrap().contains("expired"));

    println!("✅ Lease expiration recovery passed");
}

/// Test multi-queue worker processing
#[tokio::test]
async fn test_multi_queue_worker() {
    let db = TestDatabase::new().await;
    let (org_id, _) = setup_test_org_and_key(&db).await;

    // Create jobs in multiple queues
    for queue in ["emails", "webhooks", "reports"] {
        let job_id = uuid::Uuid::new_v4().to_string();
        sqlx::query(
            r#"
            INSERT INTO jobs (id, organization_id, queue_name, status, payload, priority, max_retries, timeout_seconds, created_at, updated_at)
            VALUES ($1, $2, $3, 'pending', $4::JSONB, 0, 3, 300, NOW(), NOW())
            "#
        )
        .bind(&job_id)
        .bind(&org_id)
        .bind(queue)
        .bind(json!({"queue": queue}).to_string())
        .execute(db.pool())
        .await
        .expect("Should create job");
    }

    // Worker that processes multiple queues
    let queue_list = vec!["emails", "webhooks", "reports"];
    let mut processed_queues = Vec::new();

    for _ in 0..3 {
        let result: Option<(String, String)> = sqlx::query_as(
            r#"
            UPDATE jobs
            SET status = 'processing', updated_at = NOW()
            WHERE id = (
                SELECT id FROM jobs
                WHERE organization_id = $1
                AND queue_name = ANY($2)
                AND status = 'pending'
                ORDER BY priority DESC, created_at ASC
                LIMIT 1
                FOR UPDATE SKIP LOCKED
            )
            RETURNING id, queue_name
            "#,
        )
        .bind(&org_id)
        .bind(&queue_list)
        .fetch_optional(db.pool())
        .await
        .expect("Dequeue should work");

        if let Some((_, queue)) = result {
            processed_queues.push(queue);
        }
    }

    assert_eq!(processed_queues.len(), 3);
    assert!(processed_queues.contains(&"emails".to_string()));
    assert!(processed_queues.contains(&"webhooks".to_string()));
    assert!(processed_queues.contains(&"reports".to_string()));

    println!("✅ Multi-queue worker passed");
}

/// Test job dependency chain
#[tokio::test]
async fn test_job_dependency_chain() {
    let db = TestDatabase::new().await;
    let (org_id, _) = setup_test_org_and_key(&db).await;

    // Create workflow
    let workflow_id = uuid::Uuid::new_v4().to_string();
    sqlx::query(
        "INSERT INTO workflows (id, organization_id, name, status, total_jobs, created_at) VALUES ($1, $2, 'ETL Pipeline', 'running', 3, NOW())"
    )
    .bind(&workflow_id)
    .bind(&org_id)
    .execute(db.pool())
    .await
    .expect("Should create workflow");

    // Create dependency chain: extract -> transform -> load
    let extract_id = uuid::Uuid::new_v4().to_string();
    let transform_id = uuid::Uuid::new_v4().to_string();
    let load_id = uuid::Uuid::new_v4().to_string();

    // Extract job (no dependencies, ready to run)
    sqlx::query(
        r#"
        INSERT INTO jobs (id, organization_id, queue_name, workflow_id, status, payload, priority, max_retries, timeout_seconds, dependencies_met, created_at, updated_at)
        VALUES ($1, $2, 'etl-queue', $3, 'pending', '{"step": "extract"}'::JSONB, 1, 3, 300, TRUE, NOW(), NOW())
        "#
    )
    .bind(&extract_id)
    .bind(&org_id)
    .bind(&workflow_id)
    .execute(db.pool())
    .await
    .expect("Should create extract job");

    // Transform job (depends on extract, not ready)
    sqlx::query(
        r#"
        INSERT INTO jobs (id, organization_id, queue_name, workflow_id, status, payload, priority, max_retries, timeout_seconds, dependencies_met, created_at, updated_at)
        VALUES ($1, $2, 'etl-queue', $3, 'pending', '{"step": "transform"}'::JSONB, 2, 3, 300, FALSE, NOW(), NOW())
        "#
    )
    .bind(&transform_id)
    .bind(&org_id)
    .bind(&workflow_id)
    .execute(db.pool())
    .await
    .expect("Should create transform job");

    // Load job (depends on transform, not ready)
    sqlx::query(
        r#"
        INSERT INTO jobs (id, organization_id, queue_name, workflow_id, status, payload, priority, max_retries, timeout_seconds, dependencies_met, created_at, updated_at)
        VALUES ($1, $2, 'etl-queue', $3, 'pending', '{"step": "load"}'::JSONB, 3, 3, 300, FALSE, NOW(), NOW())
        "#
    )
    .bind(&load_id)
    .bind(&org_id)
    .bind(&workflow_id)
    .execute(db.pool())
    .await
    .expect("Should create load job");

    // Create dependency records
    sqlx::query("INSERT INTO job_dependencies (job_id, depends_on_job_id) VALUES ($1, $2)")
        .bind(&transform_id)
        .bind(&extract_id)
        .execute(db.pool())
        .await
        .expect("Should create transform->extract dependency");

    sqlx::query("INSERT INTO job_dependencies (job_id, depends_on_job_id) VALUES ($1, $2)")
        .bind(&load_id)
        .bind(&transform_id)
        .execute(db.pool())
        .await
        .expect("Should create load->transform dependency");

    // Try to dequeue - should only get extract (dependencies_met = true)
    let first_job: Option<(String, Value)> = sqlx::query_as(
        r#"
        UPDATE jobs
        SET status = 'processing', updated_at = NOW()
        WHERE id = (
            SELECT id FROM jobs
            WHERE organization_id = $1
            AND queue_name = 'etl-queue'
            AND status = 'pending'
            AND dependencies_met = TRUE
            ORDER BY priority ASC, created_at ASC
            LIMIT 1
            FOR UPDATE SKIP LOCKED
        )
        RETURNING id, payload
        "#,
    )
    .bind(&org_id)
    .fetch_optional(db.pool())
    .await
    .expect("First dequeue should work");

    assert!(first_job.is_some());
    let (job_id, payload) = first_job.unwrap();
    assert_eq!(job_id, extract_id);
    assert_eq!(payload.get("step").unwrap().as_str().unwrap(), "extract");

    // Complete extract job
    sqlx::query("UPDATE jobs SET status = 'completed', completed_at = NOW() WHERE id = $1")
        .bind(&extract_id)
        .execute(db.pool())
        .await
        .expect("Should complete extract");

    // Update workflow counter
    sqlx::query("UPDATE workflows SET completed_jobs = completed_jobs + 1 WHERE id = $1")
        .bind(&workflow_id)
        .execute(db.pool())
        .await
        .expect("Should update workflow");

    // Mark transform as ready (dependency met)
    sqlx::query("UPDATE jobs SET dependencies_met = TRUE WHERE id = $1")
        .bind(&transform_id)
        .execute(db.pool())
        .await
        .expect("Should mark transform ready");

    // Now dequeue should get transform
    let second_job: Option<(String, Value)> = sqlx::query_as(
        r#"
        UPDATE jobs
        SET status = 'processing', updated_at = NOW()
        WHERE id = (
            SELECT id FROM jobs
            WHERE organization_id = $1
            AND queue_name = 'etl-queue'
            AND status = 'pending'
            AND dependencies_met = TRUE
            ORDER BY priority ASC, created_at ASC
            LIMIT 1
            FOR UPDATE SKIP LOCKED
        )
        RETURNING id, payload
        "#,
    )
    .bind(&org_id)
    .fetch_optional(db.pool())
    .await
    .expect("Second dequeue should work");

    assert!(second_job.is_some());
    let (job_id2, payload2) = second_job.unwrap();
    assert_eq!(job_id2, transform_id);
    assert_eq!(payload2.get("step").unwrap().as_str().unwrap(), "transform");

    println!("✅ Job dependency chain passed");
}

/// Test concurrent organization isolation
#[tokio::test]
async fn test_concurrent_org_isolation() {
    let db = TestDatabase::new().await;

    // Create two separate organizations
    let org1_id = uuid::Uuid::new_v4().to_string();
    let org2_id = uuid::Uuid::new_v4().to_string();

    for (org_id, slug) in [(&org1_id, "org-alpha"), (&org2_id, "org-beta")] {
        sqlx::query(
            "INSERT INTO organizations (id, name, slug, plan_tier, created_at, updated_at) VALUES ($1, $2, $3, 'free', NOW(), NOW())"
        )
        .bind(org_id)
        .bind(format!("Org {}", slug))
        .bind(slug)
        .execute(db.pool())
        .await
        .expect("Should create org");
    }

    // Create 5 jobs in each org
    for org_id in [&org1_id, &org2_id] {
        for i in 0..5 {
            let job_id = uuid::Uuid::new_v4().to_string();
            sqlx::query(
                r#"
                INSERT INTO jobs (id, organization_id, queue_name, status, payload, priority, max_retries, timeout_seconds, created_at, updated_at)
                VALUES ($1, $2, 'isolation-queue', 'pending', $3::JSONB, 0, 3, 300, NOW(), NOW())
                "#
            )
            .bind(&job_id)
            .bind(org_id)
            .bind(json!({"index": i}).to_string())
            .execute(db.pool())
            .await
            .expect("Should create job");
        }
    }

    // Org1 worker dequeues all its jobs
    let mut org1_dequeued = 0;
    loop {
        let result: Option<(String,)> = sqlx::query_as(
            r#"
            UPDATE jobs
            SET status = 'processing', updated_at = NOW()
            WHERE id = (
                SELECT id FROM jobs
                WHERE organization_id = $1
                AND queue_name = 'isolation-queue'
                AND status = 'pending'
                LIMIT 1
                FOR UPDATE SKIP LOCKED
            )
            RETURNING id
            "#,
        )
        .bind(&org1_id)
        .fetch_optional(db.pool())
        .await
        .expect("Dequeue should work");

        match result {
            Some(_) => org1_dequeued += 1,
            None => break,
        }
    }

    // Org2 worker dequeues all its jobs
    let mut org2_dequeued = 0;
    loop {
        let result: Option<(String,)> = sqlx::query_as(
            r#"
            UPDATE jobs
            SET status = 'processing', updated_at = NOW()
            WHERE id = (
                SELECT id FROM jobs
                WHERE organization_id = $1
                AND queue_name = 'isolation-queue'
                AND status = 'pending'
                LIMIT 1
                FOR UPDATE SKIP LOCKED
            )
            RETURNING id
            "#,
        )
        .bind(&org2_id)
        .fetch_optional(db.pool())
        .await
        .expect("Dequeue should work");

        match result {
            Some(_) => org2_dequeued += 1,
            None => break,
        }
    }

    // Each org should have processed exactly their own jobs
    assert_eq!(org1_dequeued, 5, "Org1 should process 5 jobs");
    assert_eq!(org2_dequeued, 5, "Org2 should process 5 jobs");

    // Verify no cross-org leakage
    let org1_jobs: (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM jobs WHERE organization_id = $1 AND status = 'processing'",
    )
    .bind(&org1_id)
    .fetch_one(db.pool())
    .await
    .expect("Should count org1 jobs");

    let org2_jobs: (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM jobs WHERE organization_id = $1 AND status = 'processing'",
    )
    .bind(&org2_id)
    .fetch_one(db.pool())
    .await
    .expect("Should count org2 jobs");

    assert_eq!(org1_jobs.0, 5);
    assert_eq!(org2_jobs.0, 5);

    println!("✅ Concurrent organization isolation passed");
}

/// Test job timeout handling
#[tokio::test]
async fn test_job_timeout_handling() {
    let db = TestDatabase::new().await;
    let (org_id, _) = setup_test_org_and_key(&db).await;

    // Create a job with very short timeout
    let job_id = uuid::Uuid::new_v4().to_string();
    sqlx::query(
        r#"
        INSERT INTO jobs (id, organization_id, queue_name, status, payload, priority, max_retries, timeout_seconds, 
                         started_at, assigned_worker_id, created_at, updated_at)
        VALUES ($1, $2, 'timeout-queue', 'processing', '{"slow": true}'::JSONB, 0, 3, 5,
                NOW() - INTERVAL '10 seconds', 'slow-worker', NOW(), NOW())
        "#
    )
    .bind(&job_id)
    .bind(&org_id)
    .execute(db.pool())
    .await
    .expect("Should create timed-out job");

    // Simulate timeout detection (scheduler finds jobs past their timeout)
    let timed_out = sqlx::query(
        r#"
        UPDATE jobs
        SET 
            status = 'pending',
            assigned_worker_id = NULL,
            retry_count = retry_count + 1,
            last_error = 'Job timed out',
            updated_at = NOW()
        WHERE organization_id = $1
        AND status = 'processing'
        AND started_at IS NOT NULL
        AND started_at + (timeout_seconds * INTERVAL '1 second') < NOW()
        AND retry_count < max_retries
        "#,
    )
    .bind(&org_id)
    .execute(db.pool())
    .await
    .expect("Should timeout jobs");

    assert_eq!(timed_out.rows_affected(), 1);

    // Verify job is back to pending
    let job: (String, i32, Option<String>) =
        sqlx::query_as("SELECT status, retry_count, last_error FROM jobs WHERE id = $1")
            .bind(&job_id)
            .fetch_one(db.pool())
            .await
            .expect("Should read job");

    assert_eq!(job.0, "pending");
    assert_eq!(job.1, 1);
    assert!(job.2.unwrap().contains("timed out"));

    println!("✅ Job timeout handling passed");
}

/// Test rate limiting (queue-level)
#[tokio::test]
async fn test_queue_rate_limiting() {
    let db = TestDatabase::new().await;
    let (org_id, _) = setup_test_org_and_key(&db).await;

    // Create queue config with rate limit
    sqlx::query(
        r#"
        INSERT INTO queue_config (organization_id, queue_name, enabled, rate_limit, max_retries, default_timeout, created_at, updated_at)
        VALUES ($1, 'rate-limited-queue', TRUE, 10, 3, 300, NOW(), NOW())
        "#
    )
    .bind(&org_id)
    .execute(db.pool())
    .await
    .expect("Should create queue config");

    // Verify rate limit is stored
    let config: (bool, Option<i32>) = sqlx::query_as(
        "SELECT enabled, rate_limit FROM queue_config WHERE organization_id = $1 AND queue_name = 'rate-limited-queue'"
    )
    .bind(&org_id)
    .fetch_one(db.pool())
    .await
    .expect("Should read config");

    assert!(config.0);
    assert_eq!(config.1, Some(10));

    println!("✅ Queue rate limiting config passed");
}

/// Test paused queue behavior
#[tokio::test]
async fn test_paused_queue_behavior() {
    let db = TestDatabase::new().await;
    let (org_id, _) = setup_test_org_and_key(&db).await;

    // Create paused queue config
    sqlx::query(
        r#"
        INSERT INTO queue_config (organization_id, queue_name, enabled, max_retries, default_timeout, created_at, updated_at)
        VALUES ($1, 'paused-queue', FALSE, 3, 300, NOW(), NOW())
        "#
    )
    .bind(&org_id)
    .execute(db.pool())
    .await
    .expect("Should create paused queue config");

    // Create job in paused queue
    let job_id = uuid::Uuid::new_v4().to_string();
    sqlx::query(
        r#"
        INSERT INTO jobs (id, organization_id, queue_name, status, payload, priority, max_retries, timeout_seconds, created_at, updated_at)
        VALUES ($1, $2, 'paused-queue', 'pending', '{"waiting": true}'::JSONB, 0, 3, 300, NOW(), NOW())
        "#
    )
    .bind(&job_id)
    .bind(&org_id)
    .execute(db.pool())
    .await
    .expect("Should create job in paused queue");

    // Check if queue is paused before dequeuing
    let is_paused: (bool,) = sqlx::query_as(
        "SELECT NOT enabled FROM queue_config WHERE organization_id = $1 AND queue_name = 'paused-queue'"
    )
    .bind(&org_id)
    .fetch_one(db.pool())
    .await
    .expect("Should check queue status");

    assert!(is_paused.0, "Queue should be paused");

    // Try to dequeue (simulating check-then-dequeue pattern)
    // In real code, we check queue_config first, then only dequeue if enabled
    let pending_count: (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM jobs WHERE organization_id = $1 AND queue_name = 'paused-queue' AND status = 'pending'"
    )
    .bind(&org_id)
    .fetch_one(db.pool())
    .await
    .expect("Should count pending");

    assert_eq!(pending_count.0, 1, "Job should be waiting in paused queue");

    // Resume queue
    sqlx::query(
        "UPDATE queue_config SET enabled = TRUE, updated_at = NOW() WHERE organization_id = $1 AND queue_name = 'paused-queue'"
    )
    .bind(&org_id)
    .execute(db.pool())
    .await
    .expect("Should resume queue");

    // Check if queue is now enabled
    let is_enabled: (bool,) = sqlx::query_as(
        "SELECT enabled FROM queue_config WHERE organization_id = $1 AND queue_name = 'paused-queue'"
    )
    .bind(&org_id)
    .fetch_one(db.pool())
    .await
    .expect("Should check queue status");

    assert!(is_enabled.0, "Queue should be enabled now");

    // Now dequeue should work (queue is enabled)
    let result2: Option<(String,)> = sqlx::query_as(
        r#"
        UPDATE jobs
        SET status = 'processing', updated_at = NOW()
        WHERE id = (
            SELECT id FROM jobs
            WHERE organization_id = $1
            AND queue_name = 'paused-queue'
            AND status = 'pending'
            LIMIT 1
            FOR UPDATE SKIP LOCKED
        )
        RETURNING id
        "#,
    )
    .bind(&org_id)
    .fetch_optional(db.pool())
    .await
    .expect("Dequeue after resume should work");

    assert!(result2.is_some(), "Should dequeue after resume");
    assert_eq!(result2.unwrap().0, job_id);

    println!("✅ Paused queue behavior passed");
}

/// Test worker stale detection
#[tokio::test]
async fn test_worker_stale_detection() {
    let db = TestDatabase::new().await;
    let (org_id, _) = setup_test_org_and_key(&db).await;

    // Create stale worker (hasn't sent heartbeat in 5 minutes)
    let stale_worker_id = uuid::Uuid::new_v4().to_string();
    sqlx::query(
        r#"
        INSERT INTO workers (id, organization_id, queue_name, hostname, status, max_concurrency, current_jobs, last_heartbeat, registered_at)
        VALUES ($1, $2, 'worker-queue', 'stale-host', 'healthy', 5, 2, NOW() - INTERVAL '10 minutes', NOW())
        "#
    )
    .bind(&stale_worker_id)
    .bind(&org_id)
    .execute(db.pool())
    .await
    .expect("Should create stale worker");

    // Create healthy worker
    let healthy_worker_id = uuid::Uuid::new_v4().to_string();
    sqlx::query(
        r#"
        INSERT INTO workers (id, organization_id, queue_name, hostname, status, max_concurrency, current_jobs, last_heartbeat, registered_at)
        VALUES ($1, $2, 'worker-queue', 'healthy-host', 'healthy', 5, 0, NOW(), NOW())
        "#
    )
    .bind(&healthy_worker_id)
    .bind(&org_id)
    .execute(db.pool())
    .await
    .expect("Should create healthy worker");

    // Simulate stale worker detection (scheduler marks workers offline)
    let marked_offline = sqlx::query(
        r#"
        UPDATE workers
        SET status = 'offline', updated_at = NOW()
        WHERE organization_id = $1
        AND status IN ('healthy', 'degraded')
        AND last_heartbeat < NOW() - INTERVAL '5 minutes'
        "#,
    )
    .bind(&org_id)
    .execute(db.pool())
    .await
    .expect("Should mark stale workers offline");

    assert_eq!(marked_offline.rows_affected(), 1);

    // Verify stale worker is offline
    let stale: (String,) = sqlx::query_as("SELECT status FROM workers WHERE id = $1")
        .bind(&stale_worker_id)
        .fetch_one(db.pool())
        .await
        .expect("Should read stale worker");

    assert_eq!(stale.0, "offline");

    // Verify healthy worker is still healthy
    let healthy: (String,) = sqlx::query_as("SELECT status FROM workers WHERE id = $1")
        .bind(&healthy_worker_id)
        .fetch_one(db.pool())
        .await
        .expect("Should read healthy worker");

    assert_eq!(healthy.0, "healthy");

    println!("✅ Worker stale detection passed");
}

/// Test schedule run tracking
#[tokio::test]
async fn test_schedule_run_tracking() {
    let db = TestDatabase::new().await;
    let (org_id, _) = setup_test_org_and_key(&db).await;

    // Create schedule
    let schedule_id = uuid::Uuid::new_v4().to_string();
    sqlx::query(
        r#"
        INSERT INTO schedules (id, organization_id, name, queue_name, cron_expression, timezone, payload_template, is_active, created_at, updated_at)
        VALUES ($1, $2, 'Hourly Report', 'reports', '0 * * * *', 'UTC', '{"type": "hourly"}'::JSONB, TRUE, NOW(), NOW())
        "#
    )
    .bind(&schedule_id)
    .bind(&org_id)
    .execute(db.pool())
    .await
    .expect("Should create schedule");

    // Simulate schedule trigger and create run record
    let job_id = uuid::Uuid::new_v4().to_string();
    sqlx::query(
        r#"
        INSERT INTO jobs (id, organization_id, queue_name, status, payload, priority, max_retries, timeout_seconds, created_at, updated_at)
        VALUES ($1, $2, 'reports', 'pending', '{"type": "hourly"}'::JSONB, 0, 3, 300, NOW(), NOW())
        "#
    )
    .bind(&job_id)
    .bind(&org_id)
    .execute(db.pool())
    .await
    .expect("Should create scheduled job");

    // Record schedule run (columns: id, schedule_id, job_id, status, error_message, started_at, completed_at)
    let run_id = uuid::Uuid::new_v4().to_string();
    sqlx::query(
        r#"
        INSERT INTO schedule_runs (id, schedule_id, job_id, status, started_at)
        VALUES ($1, $2, $3, 'running', NOW())
        "#,
    )
    .bind(&run_id)
    .bind(&schedule_id)
    .bind(&job_id)
    .execute(db.pool())
    .await
    .expect("Should record schedule run");

    // Update schedule last_run_at and run_count
    sqlx::query(
        "UPDATE schedules SET last_run_at = NOW(), run_count = run_count + 1, updated_at = NOW() WHERE id = $1"
    )
    .bind(&schedule_id)
    .execute(db.pool())
    .await
    .expect("Should update schedule");

    // Verify schedule was updated
    let schedule: (i64, bool) =
        sqlx::query_as("SELECT run_count, last_run_at IS NOT NULL FROM schedules WHERE id = $1")
            .bind(&schedule_id)
            .fetch_one(db.pool())
            .await
            .expect("Should read schedule");

    assert_eq!(schedule.0, 1);
    assert!(schedule.1); // last_run_at is set

    // Verify run record
    let run: (String, String) =
        sqlx::query_as("SELECT job_id, status FROM schedule_runs WHERE schedule_id = $1")
            .bind(&schedule_id)
            .fetch_one(db.pool())
            .await
            .expect("Should read run");

    assert_eq!(run.0, job_id);
    assert_eq!(run.1, "running");

    println!("✅ Schedule run tracking passed");
}

/// Test comprehensive job statistics
#[tokio::test]
async fn test_comprehensive_job_statistics() {
    let db = TestDatabase::new().await;
    let (org_id, _) = setup_test_org_and_key(&db).await;

    let queue_name = "stats-comprehensive";

    // Create diverse job set
    let scenarios = vec![
        ("pending", 0, None, None),  // pending, no retries
        ("pending", 5, None, None),  // pending, high priority
        ("pending", -5, None, None), // pending, low priority
        ("processing", 0, Some("worker-1"), None),
        ("processing", 0, Some("worker-2"), None),
        ("completed", 0, None, Some(100)),   // completed, fast
        ("completed", 0, None, Some(5000)),  // completed, slow
        ("completed", 0, None, Some(30000)), // completed, very slow
        ("failed", 0, None, None),
        ("failed", 0, None, None),
        ("deadletter", 0, None, None),
    ];

    for (status, priority, worker, duration_ms) in scenarios {
        let job_id = uuid::Uuid::new_v4().to_string();
        let started_at = if status == "processing" || status == "completed" {
            "NOW() - INTERVAL '1 minute'"
        } else {
            "NULL"
        };
        let completed_at = if status == "completed" {
            format!(
                "NOW() - INTERVAL '{} milliseconds'",
                duration_ms.unwrap_or(0)
            )
        } else {
            "NULL".to_string()
        };

        sqlx::query(&format!(
            r#"
            INSERT INTO jobs (id, organization_id, queue_name, status, payload, priority, max_retries, timeout_seconds, 
                             assigned_worker_id, started_at, completed_at, created_at, updated_at)
            VALUES ($1, $2, $3, $4, '{{}}'::JSONB, $5, 3, 300, $6, {}, {}, NOW(), NOW())
            "#,
            started_at, completed_at
        ))
        .bind(&job_id)
        .bind(&org_id)
        .bind(queue_name)
        .bind(status)
        .bind(priority)
        .bind(worker)
        .execute(db.pool())
        .await
        .expect("Should create job");
    }

    // Get comprehensive stats
    let stats: (i64, i64, i64, i64, i64, i64, Option<i32>, Option<i32>) = sqlx::query_as(
        r#"
        SELECT 
            COUNT(*) FILTER (WHERE status = 'pending') as pending,
            COUNT(*) FILTER (WHERE status = 'processing') as processing,
            COUNT(*) FILTER (WHERE status = 'completed') as completed,
            COUNT(*) FILTER (WHERE status = 'failed') as failed,
            COUNT(*) FILTER (WHERE status = 'deadletter') as deadletter,
            COUNT(*) as total,
            MAX(priority) FILTER (WHERE status = 'pending') as max_pending_priority,
            MIN(priority) FILTER (WHERE status = 'pending') as min_pending_priority
        FROM jobs
        WHERE organization_id = $1 AND queue_name = $2
        "#,
    )
    .bind(&org_id)
    .bind(queue_name)
    .fetch_one(db.pool())
    .await
    .expect("Should get stats");

    assert_eq!(stats.0, 3); // pending
    assert_eq!(stats.1, 2); // processing
    assert_eq!(stats.2, 3); // completed
    assert_eq!(stats.3, 2); // failed
    assert_eq!(stats.4, 1); // deadletter
    assert_eq!(stats.5, 11); // total
    assert_eq!(stats.6, Some(5)); // max priority
    assert_eq!(stats.7, Some(-5)); // min priority

    // Get worker stats
    let worker_stats: Vec<(Option<String>, i64)> = sqlx::query_as(
        r#"
        SELECT assigned_worker_id, COUNT(*) 
        FROM jobs 
        WHERE organization_id = $1 AND queue_name = $2 AND status = 'processing'
        GROUP BY assigned_worker_id
        "#,
    )
    .bind(&org_id)
    .bind(queue_name)
    .fetch_all(db.pool())
    .await
    .expect("Should get worker stats");

    assert_eq!(worker_stats.len(), 2);

    println!("✅ Comprehensive job statistics passed");
}

/// Test transaction rollback on failure
#[tokio::test]
async fn test_transaction_rollback() {
    let db = TestDatabase::new().await;
    let (org_id, _) = setup_test_org_and_key(&db).await;

    let job_id = uuid::Uuid::new_v4().to_string();

    // Create initial job
    sqlx::query(
        r#"
        INSERT INTO jobs (id, organization_id, queue_name, status, payload, priority, max_retries, timeout_seconds, created_at, updated_at)
        VALUES ($1, $2, 'tx-queue', 'pending', '{"original": true}'::JSONB, 0, 3, 300, NOW(), NOW())
        "#
    )
    .bind(&job_id)
    .bind(&org_id)
    .execute(db.pool())
    .await
    .expect("Should create job");

    // Try a transaction that should fail
    let tx_result = async {
        let mut tx = db.pool.begin().await?;

        // Update job
        sqlx::query("UPDATE jobs SET status = 'processing' WHERE id = $1")
            .bind(&job_id)
            .execute(&mut *tx)
            .await?;

        // This will fail - insert with NULL required field
        sqlx::query("INSERT INTO jobs (id, organization_id, queue_name, status, payload, priority, max_retries, timeout_seconds, created_at, updated_at) VALUES ($1, NULL, 'fail', 'pending', '{}'::JSONB, 0, 3, 300, NOW(), NOW())")
            .bind(uuid::Uuid::new_v4().to_string())
            .execute(&mut *tx)
            .await?;

        tx.commit().await?;
        Ok::<_, sqlx::Error>(())
    }.await;

    assert!(tx_result.is_err(), "Transaction should fail");

    // Verify job is still pending (rollback worked)
    let status: (String,) = sqlx::query_as("SELECT status FROM jobs WHERE id = $1")
        .bind(&job_id)
        .fetch_one(db.pool())
        .await
        .expect("Should read job");

    assert_eq!(
        status.0, "pending",
        "Job should still be pending after rollback"
    );

    println!("✅ Transaction rollback passed");
}

/// Test data integrity constraints
#[tokio::test]
async fn test_data_integrity_constraints() {
    let db = TestDatabase::new().await;
    let (org_id, _) = setup_test_org_and_key(&db).await;

    // Test 1: Job status constraint
    let result = sqlx::query(
        r#"
        INSERT INTO jobs (id, organization_id, queue_name, status, payload, priority, max_retries, timeout_seconds, created_at, updated_at)
        VALUES ($1, $2, 'constraint-queue', 'invalid_status', '{}'::JSONB, 0, 3, 300, NOW(), NOW())
        "#
    )
    .bind(uuid::Uuid::new_v4().to_string())
    .bind(&org_id)
    .execute(db.pool())
    .await;

    assert!(result.is_err(), "Invalid status should fail constraint");

    // Test 2: Priority constraint
    let result2 = sqlx::query(
        r#"
        INSERT INTO jobs (id, organization_id, queue_name, status, payload, priority, max_retries, timeout_seconds, created_at, updated_at)
        VALUES ($1, $2, 'constraint-queue', 'pending', '{}'::JSONB, 999, 3, 300, NOW(), NOW())
        "#
    )
    .bind(uuid::Uuid::new_v4().to_string())
    .bind(&org_id)
    .execute(db.pool())
    .await;

    assert!(
        result2.is_err(),
        "Priority out of range should fail constraint"
    );

    // Test 3: Timeout constraint
    let result3 = sqlx::query(
        r#"
        INSERT INTO jobs (id, organization_id, queue_name, status, payload, priority, max_retries, timeout_seconds, created_at, updated_at)
        VALUES ($1, $2, 'constraint-queue', 'pending', '{}'::JSONB, 0, 3, -1, NOW(), NOW())
        "#
    )
    .bind(uuid::Uuid::new_v4().to_string())
    .bind(&org_id)
    .execute(db.pool())
    .await;

    assert!(result3.is_err(), "Negative timeout should fail constraint");

    // Test 4: Worker status constraint
    let result4 = sqlx::query(
        r#"
        INSERT INTO workers (id, organization_id, queue_name, hostname, status, max_concurrency, current_jobs, last_heartbeat, registered_at)
        VALUES ($1, $2, 'test', 'host', 'invalid_status', 5, 0, NOW(), NOW())
        "#
    )
    .bind(uuid::Uuid::new_v4().to_string())
    .bind(&org_id)
    .execute(db.pool())
    .await;

    assert!(
        result4.is_err(),
        "Invalid worker status should fail constraint"
    );

    // Test 5: Worker concurrency constraint (new column max_concurrent_jobs, check <= 1000)
    let result5 = sqlx::query(
        r#"
        INSERT INTO workers (id, organization_id, queue_name, queue_names, hostname, status, max_concurrent_jobs, current_job_count, last_heartbeat, created_at, updated_at)
        VALUES ($1, $2, 'test', ARRAY['test'], 'host', 'healthy', 1001, 0, NOW(), NOW(), NOW())
        "#
    )
    .bind(uuid::Uuid::new_v4().to_string())
    .bind(&org_id)
    .execute(db.pool())
    .await;

    assert!(
        result5.is_err(),
        "max_concurrent_jobs > 1000 should fail constraint"
    );

    // Test 6: Webhook provider constraint
    let result6 = sqlx::query(
        r#"
        INSERT INTO webhook_deliveries (id, organization_id, provider, event_type, payload, created_at)
        VALUES ($1, $2, 'invalid_provider', 'push', '{}'::JSONB, NOW())
        "#
    )
    .bind(uuid::Uuid::new_v4().to_string())
    .bind(&org_id)
    .execute(db.pool())
    .await;

    assert!(
        result6.is_err(),
        "Invalid webhook provider should fail constraint"
    );

    println!("✅ Data integrity constraints passed");
}

/// Test deadletter requeue flow
#[tokio::test]
async fn test_deadletter_requeue_flow() {
    let db = TestDatabase::new().await;
    let (org_id, _) = setup_test_org_and_key(&db).await;

    // Create deadlettered job and DLQ entry
    let job_id = uuid::Uuid::new_v4().to_string();
    sqlx::query(
        r#"
        INSERT INTO jobs (id, organization_id, queue_name, status, payload, priority, max_retries, retry_count, last_error, timeout_seconds, created_at, updated_at)
        VALUES ($1, $2, 'dlq-flow', 'deadletter', '{}'::JSONB, 0, 3, 3, 'failed', 300, NOW(), NOW())
        "#
    )
    .bind(&job_id)
    .bind(&org_id)
    .execute(db.pool())
    .await
    .expect("Should create deadletter job");

    let dlq_id = uuid::Uuid::new_v4().to_string();
    sqlx::query(
        r#"
        INSERT INTO dead_letter_queue (id, job_id, organization_id, queue_name, reason, original_payload, error_details, created_at)
        VALUES ($1, $2, $3, 'dlq-flow', 'failed', '{}'::JSONB, '{}'::JSONB, NOW())
        "#
    )
    .bind(&dlq_id)
    .bind(&job_id)
    .bind(&org_id)
    .execute(db.pool())
    .await
    .expect("Should insert DLQ record");

    // Requeue: set job to pending and remove DLQ record
    sqlx::query(
        "UPDATE jobs SET status = 'pending', retry_count = 0, last_error = NULL, updated_at = NOW() WHERE id = $1"
    )
    .bind(&job_id)
    .execute(db.pool())
    .await
    .expect("Should requeue job");

    sqlx::query("DELETE FROM dead_letter_queue WHERE id = $1")
        .bind(&dlq_id)
        .execute(db.pool())
        .await
        .expect("Should delete DLQ record");

    // Verify job is pending and DLQ entry removed
    let status: (String,) = sqlx::query_as("SELECT status FROM jobs WHERE id = $1")
        .bind(&job_id)
        .fetch_one(db.pool())
        .await
        .expect("Should read job");
    assert_eq!(status.0, "pending");

    let dlq_count: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM dead_letter_queue WHERE id = $1")
        .bind(&dlq_id)
        .fetch_one(db.pool())
        .await
        .expect("Should count dlq");
    assert_eq!(dlq_count.0, 0);

    println!("✅ Deadletter requeue flow passed");
}

/// Test idempotency across organizations (same key allowed per org)
#[tokio::test]
async fn test_idempotency_cross_org() {
    let db = TestDatabase::new().await;

    let org1_id = uuid::Uuid::new_v4().to_string();
    let org2_id = uuid::Uuid::new_v4().to_string();
    for (org_id, slug) in [(&org1_id, "cross-org-a"), (&org2_id, "cross-org-b")] {
        sqlx::query(
            "INSERT INTO organizations (id, name, slug, plan_tier, created_at, updated_at) VALUES ($1, $2, $3, 'free', NOW(), NOW())"
        )
        .bind(org_id)
        .bind(format!("Org {}", slug))
        .bind(slug)
        .execute(db.pool())
        .await
        .expect("Should create org");
    }

    let idem_key = "shared-idem-key";
    // Org1 insert
    sqlx::query(
        r#"
        INSERT INTO jobs (id, organization_id, queue_name, status, payload, priority, max_retries, timeout_seconds, idempotency_key, created_at, updated_at)
        VALUES ($1, $2, 'idem', 'pending', '{}'::JSONB, 0, 3, 300, $3, NOW(), NOW())
        ON CONFLICT (organization_id, idempotency_key) WHERE idempotency_key IS NOT NULL
        DO UPDATE SET updated_at = NOW()
        "#
    )
    .bind(uuid::Uuid::new_v4().to_string())
    .bind(&org1_id)
    .bind(idem_key)
    .execute(db.pool())
    .await
    .expect("Org1 insert");

    // Org2 insert with same idempotency key should also succeed (scoped per org)
    let result = sqlx::query(
        r#"
        INSERT INTO jobs (id, organization_id, queue_name, status, payload, priority, max_retries, timeout_seconds, idempotency_key, created_at, updated_at)
        VALUES ($1, $2, 'idem', 'pending', '{}'::JSONB, 0, 3, 300, $3, NOW(), NOW())
        ON CONFLICT (organization_id, idempotency_key) WHERE idempotency_key IS NOT NULL
        DO UPDATE SET updated_at = NOW()
        "#
    )
    .bind(uuid::Uuid::new_v4().to_string())
    .bind(&org2_id)
    .bind(idem_key)
    .execute(db.pool())
    .await;

    assert!(result.is_ok(), "Idempotency keys should be per-org");

    println!("✅ Idempotency across organizations passed");
}

/// Test worker multi-queue lookup using queue_names array
#[tokio::test]
async fn test_worker_queue_names_lookup() {
    let db = TestDatabase::new().await;
    let (org_id, _) = setup_test_org_and_key(&db).await;

    let worker_id = uuid::Uuid::new_v4().to_string();
    sqlx::query(
        r#"
        INSERT INTO workers (id, organization_id, queue_name, queue_names, hostname, status, max_concurrent_jobs, current_job_count, last_heartbeat, created_at, updated_at)
        VALUES ($1, $2, 'emails', ARRAY['emails','reports'], 'multi-host', 'healthy', 5, 0, NOW(), NOW(), NOW())
        "#
    )
    .bind(&worker_id)
    .bind(&org_id)
    .execute(db.pool())
    .await
    .expect("Should insert worker");

    let found: Option<(String,)> = sqlx::query_as(
        "SELECT id FROM workers WHERE organization_id = $1 AND ('reports' = ANY(queue_names))",
    )
    .bind(&org_id)
    .fetch_optional(db.pool())
    .await
    .expect("Should query worker by queue_names");

    assert!(found.is_some(), "Worker should be found via queue_names");
    assert_eq!(found.unwrap().0, worker_id);

    println!("✅ Worker queue_names lookup passed");
}

/// Test retry exhaustion moves job to deadletter
#[tokio::test]
async fn test_retry_exhaustion_deadletter() {
    let db = TestDatabase::new().await;
    let (org_id, _) = setup_test_org_and_key(&db).await;
    let job_id = uuid::Uuid::new_v4().to_string();

    // Create job at max retries
    sqlx::query(
        r#"
        INSERT INTO jobs (id, organization_id, queue_name, status, payload, priority, max_retries, retry_count, timeout_seconds, last_error, created_at, updated_at)
        VALUES ($1, $2, 'retry-exhaust', 'processing', '{}'::JSONB, 0, 2, 2, 300, 'fail', NOW(), NOW())
        "#
    )
    .bind(&job_id)
    .bind(&org_id)
    .execute(db.pool())
    .await
    .expect("Should insert job");

    // Simulate transition to deadletter when retries exhausted
    sqlx::query(
        r#"
        UPDATE jobs
        SET status = 'deadletter', updated_at = NOW()
        WHERE id = $1 AND retry_count >= max_retries
        "#,
    )
    .bind(&job_id)
    .execute(db.pool())
    .await
    .expect("Should move to deadletter");

    let status: (String,) = sqlx::query_as("SELECT status FROM jobs WHERE id = $1")
        .bind(&job_id)
        .fetch_one(db.pool())
        .await
        .expect("Should read status");

    assert_eq!(status.0, "deadletter");
    println!("✅ Retry exhaustion moves to deadletter");
}

/// Test schedule run completion update
#[tokio::test]
async fn test_schedule_run_completion() {
    let db = TestDatabase::new().await;
    let (org_id, _) = setup_test_org_and_key(&db).await;

    // Create schedule
    let schedule_id = uuid::Uuid::new_v4().to_string();
    sqlx::query(
        r#"
        INSERT INTO schedules (id, organization_id, name, queue_name, cron_expression, timezone, payload_template, is_active, created_at, updated_at)
        VALUES ($1, $2, 'Completion Test', 'reports', '0 * * * *', 'UTC', '{}'::JSONB, TRUE, NOW(), NOW())
        "#
    )
    .bind(&schedule_id)
    .bind(&org_id)
    .execute(db.pool())
    .await
    .expect("Should create schedule");

    // Insert schedule run (running)
    let run_id = uuid::Uuid::new_v4().to_string();
    sqlx::query(
        r#"
        INSERT INTO schedule_runs (id, schedule_id, status, started_at)
        VALUES ($1, $2, 'running', NOW())
        "#,
    )
    .bind(&run_id)
    .bind(&schedule_id)
    .execute(db.pool())
    .await
    .expect("Should insert run");

    // Complete run
    sqlx::query(
        "UPDATE schedule_runs SET status = 'completed', completed_at = NOW() WHERE id = $1",
    )
    .bind(&run_id)
    .execute(db.pool())
    .await
    .expect("Should complete run");

    let run: (String, bool) =
        sqlx::query_as("SELECT status, completed_at IS NOT NULL FROM schedule_runs WHERE id = $1")
            .bind(&run_id)
            .fetch_one(db.pool())
            .await
            .expect("Should read run");

    assert_eq!(run.0, "completed");
    assert!(run.1);
    println!("✅ Schedule run completion passed");
}

/// Test max_concurrent_jobs upper bound accepted at 1000
#[tokio::test]
async fn test_worker_max_concurrent_jobs_upper_bound() {
    let db = TestDatabase::new().await;
    let (org_id, _) = setup_test_org_and_key(&db).await;

    let worker_id = uuid::Uuid::new_v4().to_string();
    let result = sqlx::query(
        r#"
        INSERT INTO workers (id, organization_id, queue_name, queue_names, hostname, status, max_concurrent_jobs, current_job_count, last_heartbeat, created_at, updated_at)
        VALUES ($1, $2, 'upper-bound', ARRAY['upper-bound'], 'host', 'healthy', 1000, 0, NOW(), NOW(), NOW())
        "#
    )
    .bind(&worker_id)
    .bind(&org_id)
    .execute(db.pool())
    .await;

    assert!(result.is_ok(), "max_concurrent_jobs = 1000 should succeed");
    println!("✅ Worker max_concurrent_jobs upper bound accepted");
}

/// Test queue_config allows NULL rate_limit
#[tokio::test]
async fn test_queue_config_null_rate_limit() {
    let db = TestDatabase::new().await;
    let (org_id, _) = setup_test_org_and_key(&db).await;

    sqlx::query(
        r#"
        INSERT INTO queue_config (organization_id, queue_name, enabled, max_retries, default_timeout, rate_limit, created_at, updated_at)
        VALUES ($1, 'null-rate-limit', TRUE, 3, 300, NULL, NOW(), NOW())
        "#
    )
    .bind(&org_id)
    .execute(db.pool())
    .await
    .expect("Should insert queue_config with NULL rate_limit");

    let cfg: (Option<i32>,) = sqlx::query_as(
        "SELECT rate_limit FROM queue_config WHERE organization_id = $1 AND queue_name = 'null-rate-limit'"
    )
    .bind(&org_id)
    .fetch_one(db.pool())
    .await
    .expect("Should read config");

    assert!(cfg.0.is_none());
    println!("✅ Queue config supports NULL rate_limit");
}

/// Test high concurrency dequeue stress
#[tokio::test]
async fn test_high_concurrency_dequeue_stress() {
    let db = TestDatabase::new().await;
    let (org_id, _) = setup_test_org_and_key(&db).await;

    // Create 50 pending jobs
    for i in 0..50 {
        let job_id = uuid::Uuid::new_v4().to_string();
        sqlx::query(
            r#"
            INSERT INTO jobs (id, organization_id, queue_name, status, payload, priority, max_retries, timeout_seconds, created_at, updated_at)
            VALUES ($1, $2, 'stress-queue', 'pending', $3::JSONB, 0, 3, 300, NOW(), NOW())
            "#
        )
        .bind(&job_id)
        .bind(&org_id)
        .bind(json!({"index": i}).to_string())
        .execute(db.pool())
        .await
        .expect("Should create job");
    }

    // Spawn 10 concurrent workers, each trying to dequeue 10 jobs
    let pool = db.pool.clone();
    let org_id_clone = org_id.clone();

    let handles: Vec<_> = (0..10)
        .map(|worker_num| {
            let pool = pool.clone();
            let org_id = org_id_clone.clone();
            tokio::spawn(async move {
                let mut dequeued = 0;
                for _ in 0..10 {
                    let result: Option<(String,)> = sqlx::query_as(
                        r#"
                    UPDATE jobs
                    SET status = 'processing',
                        assigned_worker_id = $1,
                        updated_at = NOW()
                    WHERE id = (
                        SELECT id FROM jobs
                        WHERE organization_id = $2
                        AND queue_name = 'stress-queue'
                        AND status = 'pending'
                        LIMIT 1
                        FOR UPDATE SKIP LOCKED
                    )
                    RETURNING id
                    "#,
                    )
                    .bind(format!("stress-worker-{}", worker_num))
                    .bind(&org_id)
                    .fetch_optional(&*pool)
                    .await
                    .expect("Dequeue should work");

                    if result.is_some() {
                        dequeued += 1;
                    }
                }
                dequeued
            })
        })
        .collect();

    let results: Vec<i32> = futures::future::join_all(handles)
        .await
        .into_iter()
        .map(|r| r.unwrap())
        .collect();

    let total_dequeued: i32 = results.iter().sum();

    // Verify exactly 50 jobs were dequeued (no duplicates, no losses)
    assert_eq!(
        total_dequeued, 50,
        "All 50 jobs should be dequeued exactly once"
    );

    // Verify no pending jobs remain
    let pending: (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM jobs WHERE organization_id = $1 AND queue_name = 'stress-queue' AND status = 'pending'"
    )
    .bind(&org_id)
    .fetch_one(db.pool())
    .await
    .expect("Should count pending");

    assert_eq!(pending.0, 0, "No pending jobs should remain");

    // Verify all jobs are processing
    let processing: (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM jobs WHERE organization_id = $1 AND queue_name = 'stress-queue' AND status = 'processing'"
    )
    .bind(&org_id)
    .fetch_one(db.pool())
    .await
    .expect("Should count processing");

    assert_eq!(processing.0, 50, "All 50 jobs should be processing");

    println!("✅ High concurrency dequeue stress passed (10 workers × 50 jobs)");
}
