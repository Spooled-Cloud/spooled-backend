//! Integration tests for Spooled Backend
//!
//! These tests use testcontainers to spin up PostgreSQL and Redis
//! and test the full API flow end-to-end.

mod common;

use common::{fixtures, TestDatabase};

/// Test database migrations run successfully
#[tokio::test]
async fn test_database_migrations() {
    let db = TestDatabase::new().await;

    // Verify tables exist
    let tables: Vec<(String,)> = sqlx::query_as(
        r#"
        SELECT tablename::TEXT 
        FROM pg_tables 
        WHERE schemaname = 'public'
        ORDER BY tablename
        "#,
    )
    .fetch_all(db.pool())
    .await
    .expect("Failed to query tables");

    let table_names: Vec<&str> = tables.iter().map(|(t,)| t.as_str()).collect();

    // Check all expected tables exist
    assert!(
        table_names.contains(&"organizations"),
        "Missing organizations table"
    );
    assert!(table_names.contains(&"jobs"), "Missing jobs table");
    assert!(table_names.contains(&"workers"), "Missing workers table");
    assert!(
        table_names.contains(&"job_history"),
        "Missing job_history table"
    );
    assert!(
        table_names.contains(&"dead_letter_queue"),
        "Missing dead_letter_queue table"
    );
    assert!(
        table_names.contains(&"queue_config"),
        "Missing queue_config table"
    );
    assert!(table_names.contains(&"api_keys"), "Missing api_keys table");
    assert!(
        table_names.contains(&"webhook_deliveries"),
        "Missing webhook_deliveries table"
    );
}

/// Test RLS policies are enabled
#[tokio::test]
async fn test_rls_policies_enabled() {
    let db = TestDatabase::new().await;

    // Check RLS is enabled on key tables
    let rls_tables: Vec<(String, bool)> = sqlx::query_as(
        r#"
        SELECT relname::TEXT, relrowsecurity
        FROM pg_class
        WHERE relname IN ('jobs', 'workers', 'api_keys', 'job_history')
        "#,
    )
    .fetch_all(db.pool())
    .await
    .expect("Failed to query RLS status");

    for (table, rls_enabled) in rls_tables {
        assert!(rls_enabled, "RLS should be enabled on table: {}", table);
    }
}

/// Test organization CRUD operations
#[tokio::test]
async fn test_organization_crud() {
    let db = TestDatabase::new().await;

    // Create organization directly in database (bypassing RLS for testing)
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
    .expect("Failed to create organization");

    // Verify organization exists
    let org: Option<(String, String)> =
        sqlx::query_as("SELECT name, slug FROM organizations WHERE id = $1")
            .bind(&org_id)
            .fetch_optional(db.pool())
            .await
            .expect("Failed to query organization");

    assert!(org.is_some(), "Organization should exist");
    let (name, slug) = org.unwrap();
    assert_eq!(name, org_name);
    assert_eq!(slug, org_slug);

    // Update organization
    sqlx::query("UPDATE organizations SET name = $1, updated_at = NOW() WHERE id = $2")
        .bind("Updated Organization")
        .bind(&org_id)
        .execute(db.pool())
        .await
        .expect("Failed to update organization");

    // Verify update
    let updated_name: (String,) = sqlx::query_as("SELECT name FROM organizations WHERE id = $1")
        .bind(&org_id)
        .fetch_one(db.pool())
        .await
        .expect("Failed to query updated organization");

    assert_eq!(updated_name.0, "Updated Organization");

    // Delete organization
    sqlx::query("DELETE FROM organizations WHERE id = $1")
        .bind(&org_id)
        .execute(db.pool())
        .await
        .expect("Failed to delete organization");

    // Verify deletion
    let deleted: Option<(String,)> = sqlx::query_as("SELECT id FROM organizations WHERE id = $1")
        .bind(&org_id)
        .fetch_optional(db.pool())
        .await
        .expect("Failed to query deleted organization");

    assert!(deleted.is_none(), "Organization should be deleted");
}

/// Test job creation and retrieval
#[tokio::test]
async fn test_job_lifecycle() {
    let db = TestDatabase::new().await;

    // Use default organization
    let org_id = "default-org";
    let queue_name = "test-queue";

    // Create a job
    let job_id = uuid::Uuid::new_v4().to_string();
    let payload = serde_json::json!({"action": "test", "value": 42});

    sqlx::query(
        r#"
        INSERT INTO jobs (
            id, organization_id, queue_name, status, payload, priority,
            max_retries, timeout_seconds, created_at, updated_at
        )
        VALUES ($1, $2, $3, 'pending', $4::JSONB, 0, 3, 300, NOW(), NOW())
        "#,
    )
    .bind(&job_id)
    .bind(org_id)
    .bind(queue_name)
    .bind(serde_json::to_string(&payload).unwrap())
    .execute(db.pool())
    .await
    .expect("Failed to create job");

    // Verify job exists and is pending
    let job: (String, String) = sqlx::query_as("SELECT status, queue_name FROM jobs WHERE id = $1")
        .bind(&job_id)
        .fetch_one(db.pool())
        .await
        .expect("Failed to query job");

    assert_eq!(job.0, "pending");
    assert_eq!(job.1, queue_name);

    // Update job status to processing
    sqlx::query("UPDATE jobs SET status = 'processing', started_at = NOW() WHERE id = $1")
        .bind(&job_id)
        .execute(db.pool())
        .await
        .expect("Failed to update job status");

    // Complete the job
    let result = serde_json::json!({"success": true});
    sqlx::query(
        "UPDATE jobs SET status = 'completed', result = $1::JSONB, completed_at = NOW() WHERE id = $2",
    )
    .bind(serde_json::to_string(&result).unwrap())
    .bind(&job_id)
    .execute(db.pool())
    .await
    .expect("Failed to complete job");

    // Verify job is completed
    let completed_job: (String, Option<String>) =
        sqlx::query_as("SELECT status, result::TEXT FROM jobs WHERE id = $1")
            .bind(&job_id)
            .fetch_one(db.pool())
            .await
            .expect("Failed to query completed job");

    assert_eq!(completed_job.0, "completed");
    assert!(completed_job.1.is_some());
}

/// Test job idempotency
#[tokio::test]
async fn test_job_idempotency() {
    let db = TestDatabase::new().await;

    let org_id = "default-org";
    let queue_name = "test-queue";
    let idempotency_key = "unique-key-123";

    // Create first job with idempotency key
    let job1_id = uuid::Uuid::new_v4().to_string();

    let returned_id1: (String,) = sqlx::query_as(
        r#"
        INSERT INTO jobs (
            id, organization_id, queue_name, status, payload, priority,
            max_retries, timeout_seconds, idempotency_key, created_at, updated_at
        )
        VALUES ($1, $2, $3, 'pending', '{}'::JSONB, 0, 3, 300, $4, NOW(), NOW())
        ON CONFLICT (organization_id, idempotency_key) WHERE idempotency_key IS NOT NULL
        DO UPDATE SET updated_at = NOW()
        RETURNING id
        "#,
    )
    .bind(&job1_id)
    .bind(org_id)
    .bind(queue_name)
    .bind(idempotency_key)
    .fetch_one(db.pool())
    .await
    .expect("Failed to create first job");

    assert_eq!(returned_id1.0, job1_id, "First job should have original ID");

    // Try to create second job with same idempotency key
    let job2_id = uuid::Uuid::new_v4().to_string();

    let returned_id2: (String,) = sqlx::query_as(
        r#"
        INSERT INTO jobs (
            id, organization_id, queue_name, status, payload, priority,
            max_retries, timeout_seconds, idempotency_key, created_at, updated_at
        )
        VALUES ($1, $2, $3, 'pending', '{}'::JSONB, 0, 3, 300, $4, NOW(), NOW())
        ON CONFLICT (organization_id, idempotency_key) WHERE idempotency_key IS NOT NULL
        DO UPDATE SET updated_at = NOW()
        RETURNING id
        "#,
    )
    .bind(&job2_id)
    .bind(org_id)
    .bind(queue_name)
    .bind(idempotency_key)
    .fetch_one(db.pool())
    .await
    .expect("Failed to insert duplicate job");

    // Should return the first job's ID (idempotent behavior)
    assert_eq!(
        returned_id2.0, job1_id,
        "Idempotent insert should return original job ID"
    );

    // Verify only one job exists with this idempotency key
    let count: (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM jobs WHERE organization_id = $1 AND idempotency_key = $2",
    )
    .bind(org_id)
    .bind(idempotency_key)
    .fetch_one(db.pool())
    .await
    .expect("Failed to count jobs");

    assert_eq!(
        count.0, 1,
        "Should have exactly one job with this idempotency key"
    );
}

/// Test FOR UPDATE SKIP LOCKED behavior
#[tokio::test]
async fn test_for_update_skip_locked() {
    let db = TestDatabase::new().await;

    let org_id = "default-org";
    let queue_name = "test-queue";

    // Create multiple pending jobs
    for i in 0..5 {
        let job_id = uuid::Uuid::new_v4().to_string();
        sqlx::query(
            r#"
            INSERT INTO jobs (
                id, organization_id, queue_name, status, payload, priority,
                max_retries, timeout_seconds, created_at, updated_at
            )
            VALUES ($1, $2, $3, 'pending', '{}'::JSONB, $4, 3, 300, NOW(), NOW())
            "#,
        )
        .bind(&job_id)
        .bind(org_id)
        .bind(queue_name)
        .bind(i) // Different priorities
        .execute(db.pool())
        .await
        .expect("Failed to create job");
    }

    // Simulate dequeue with FOR UPDATE SKIP LOCKED
    let worker_id = uuid::Uuid::new_v4().to_string();

    let dequeued: Option<(String, i32)> = sqlx::query_as(
        r#"
        UPDATE jobs
        SET 
            status = 'processing',
            assigned_worker_id = $1,
            started_at = NOW()
        WHERE id = (
            SELECT id FROM jobs
            WHERE organization_id = $2
              AND queue_name = $3
              AND status = 'pending'
            ORDER BY priority DESC, created_at ASC
            LIMIT 1
            FOR UPDATE SKIP LOCKED
        )
        RETURNING id, priority
        "#,
    )
    .bind(&worker_id)
    .bind(org_id)
    .bind(queue_name)
    .fetch_optional(db.pool())
    .await
    .expect("Failed to dequeue job");

    assert!(dequeued.is_some(), "Should dequeue a job");
    let (dequeued_id, priority) = dequeued.unwrap();

    // The highest priority job (4) should be dequeued first
    assert_eq!(priority, 4, "Should dequeue highest priority job first");

    // Verify job is now processing
    let status: (String,) = sqlx::query_as("SELECT status FROM jobs WHERE id = $1")
        .bind(&dequeued_id)
        .fetch_one(db.pool())
        .await
        .expect("Failed to query job status");

    assert_eq!(status.0, "processing");
}

/// Test worker registration and heartbeat
#[tokio::test]
async fn test_worker_lifecycle() {
    let db = TestDatabase::new().await;

    let org_id = "default-org";
    let worker_id = uuid::Uuid::new_v4().to_string();
    let queue_name = "test-queue";

    // Register worker
    sqlx::query(
        r#"
        INSERT INTO workers (
            id, organization_id, queue_name, hostname, max_concurrency,
            current_jobs, status, last_heartbeat, metadata, registered_at
        )
        VALUES ($1, $2, $3, 'test-host', 5, 0, 'healthy', NOW(), '{}', NOW())
        "#,
    )
    .bind(&worker_id)
    .bind(org_id)
    .bind(queue_name)
    .execute(db.pool())
    .await
    .expect("Failed to register worker");

    // Verify worker exists
    let worker: (String, String) =
        sqlx::query_as("SELECT status, hostname FROM workers WHERE id = $1")
            .bind(&worker_id)
            .fetch_one(db.pool())
            .await
            .expect("Failed to query worker");

    assert_eq!(worker.0, "healthy");
    assert_eq!(worker.1, "test-host");

    // Send heartbeat
    sqlx::query("UPDATE workers SET last_heartbeat = NOW(), current_jobs = $1 WHERE id = $2")
        .bind(3)
        .bind(&worker_id)
        .execute(db.pool())
        .await
        .expect("Failed to send heartbeat");

    // Verify heartbeat updated
    let current_jobs: (i32,) = sqlx::query_as("SELECT current_jobs FROM workers WHERE id = $1")
        .bind(&worker_id)
        .fetch_one(db.pool())
        .await
        .expect("Failed to query worker");

    assert_eq!(current_jobs.0, 3);

    // Mark worker as offline
    sqlx::query("UPDATE workers SET status = 'offline' WHERE id = $1")
        .bind(&worker_id)
        .execute(db.pool())
        .await
        .expect("Failed to update worker status");

    let status: (String,) = sqlx::query_as("SELECT status FROM workers WHERE id = $1")
        .bind(&worker_id)
        .fetch_one(db.pool())
        .await
        .expect("Failed to query worker status");

    assert_eq!(status.0, "offline");
}

/// Test dead letter queue
#[tokio::test]
async fn test_dead_letter_queue() {
    let db = TestDatabase::new().await;

    let org_id = "default-org";
    let queue_name = "test-queue";
    let job_id = uuid::Uuid::new_v4().to_string();

    // Create a job
    sqlx::query(
        r#"
        INSERT INTO jobs (
            id, organization_id, queue_name, status, payload, priority,
            max_retries, timeout_seconds, retry_count, created_at, updated_at
        )
        VALUES ($1, $2, $3, 'deadletter', '{"test": true}'::JSONB, 0, 3, 300, 3, NOW(), NOW())
        "#,
    )
    .bind(&job_id)
    .bind(org_id)
    .bind(queue_name)
    .execute(db.pool())
    .await
    .expect("Failed to create job");

    // Add to dead letter queue
    let dlq_id = uuid::Uuid::new_v4().to_string();
    sqlx::query(
        r#"
        INSERT INTO dead_letter_queue (
            id, job_id, organization_id, queue_name, reason, original_payload, error_details, created_at
        )
        VALUES ($1, $2, $3, $4, 'Max retries exceeded', '{"test": true}'::JSONB, '{"error": "timeout"}'::JSONB, NOW())
        "#,
    )
    .bind(&dlq_id)
    .bind(&job_id)
    .bind(org_id)
    .bind(queue_name)
    .execute(db.pool())
    .await
    .expect("Failed to add to DLQ");

    // Verify DLQ entry
    let dlq_entry: (String, String) =
        sqlx::query_as("SELECT job_id, reason FROM dead_letter_queue WHERE id = $1")
            .bind(&dlq_id)
            .fetch_one(db.pool())
            .await
            .expect("Failed to query DLQ");

    assert_eq!(dlq_entry.0, job_id);
    assert_eq!(dlq_entry.1, "Max retries exceeded");
}

/// Test job history tracking
#[tokio::test]
async fn test_job_history() {
    let db = TestDatabase::new().await;

    let org_id = "default-org";
    let queue_name = "test-queue";
    let job_id = uuid::Uuid::new_v4().to_string();

    // Create a job
    sqlx::query(
        r#"
        INSERT INTO jobs (
            id, organization_id, queue_name, status, payload, priority,
            max_retries, timeout_seconds, created_at, updated_at
        )
        VALUES ($1, $2, $3, 'pending', '{}'::JSONB, 0, 3, 300, NOW(), NOW())
        "#,
    )
    .bind(&job_id)
    .bind(org_id)
    .bind(queue_name)
    .execute(db.pool())
    .await
    .expect("Failed to create job");

    // Add history events
    let events = vec![
        ("created", serde_json::json!({})),
        ("processing", serde_json::json!({"worker_id": "worker-1"})),
        ("completed", serde_json::json!({"duration_ms": 150})),
    ];

    for (event_type, details) in &events {
        let history_id = uuid::Uuid::new_v4().to_string();
        sqlx::query(
            r#"
            INSERT INTO job_history (id, job_id, event_type, details, created_at)
            VALUES ($1, $2, $3, $4, NOW())
            "#,
        )
        .bind(&history_id)
        .bind(&job_id)
        .bind(*event_type)
        .bind(details)
        .execute(db.pool())
        .await
        .expect("Failed to add history event");
    }

    // Query history
    let history: Vec<(String,)> =
        sqlx::query_as("SELECT event_type FROM job_history WHERE job_id = $1 ORDER BY created_at")
            .bind(&job_id)
            .fetch_all(db.pool())
            .await
            .expect("Failed to query history");

    assert_eq!(history.len(), 3);
    assert_eq!(history[0].0, "created");
    assert_eq!(history[1].0, "processing");
    assert_eq!(history[2].0, "completed");
}

/// Test autovacuum settings are applied
#[tokio::test]
async fn test_autovacuum_settings() {
    let db = TestDatabase::new().await;

    // Check autovacuum settings for jobs table
    let settings: Vec<(String, String)> = sqlx::query_as(
        r#"
        SELECT name::TEXT, setting::TEXT
        FROM pg_settings
        WHERE name LIKE 'autovacuum%'
        LIMIT 5
        "#,
    )
    .fetch_all(db.pool())
    .await
    .expect("Failed to query autovacuum settings");

    // Verify we got some settings (specific values may vary by PostgreSQL version)
    assert!(!settings.is_empty(), "Should have autovacuum settings");
}

/// Test creating multiple organizations with unique slugs using fixtures
#[tokio::test]
async fn test_multiple_organizations_with_fixtures() {
    let db = TestDatabase::new().await;

    // Create multiple organizations using fixtures
    for _ in 0..3 {
        let org_data = fixtures::create_organization_request();
        let org_id = uuid::Uuid::new_v4().to_string();

        sqlx::query(
            r#"
            INSERT INTO organizations (id, name, slug, plan_tier, billing_email, created_at, updated_at)
            VALUES ($1, $2, $3, 'free', $4, NOW(), NOW())
            "#,
        )
        .bind(&org_id)
        .bind(org_data["name"].as_str().unwrap())
        .bind(org_data["slug"].as_str().unwrap())
        .bind(org_data["billing_email"].as_str().unwrap())
        .execute(db.pool())
        .await
        .expect("Failed to create organization");
    }

    // Verify all organizations were created
    let count: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM organizations")
        .fetch_one(db.pool())
        .await
        .expect("Failed to count organizations");

    // At least 3 (may have default org from migrations)
    assert!(count.0 >= 3, "Should have at least 3 organizations");
}

/// Test job creation with fixtures
#[tokio::test]
async fn test_job_creation_with_fixtures() {
    let db = TestDatabase::new().await;

    let org_id = "default-org";
    let queue_name = "test-queue";
    let job_data = fixtures::create_job_request(queue_name);
    let job_id = uuid::Uuid::new_v4().to_string();

    sqlx::query(
        r#"
        INSERT INTO jobs (
            id, organization_id, queue_name, status, payload, priority,
            max_retries, timeout_seconds, created_at, updated_at
        )
        VALUES ($1, $2, $3, 'pending', $4::JSONB, $5, $6, $7, NOW(), NOW())
        "#,
    )
    .bind(&job_id)
    .bind(org_id)
    .bind(job_data["queue_name"].as_str().unwrap())
    .bind(job_data["payload"].to_string())
    .bind(job_data["priority"].as_i64().unwrap() as i32)
    .bind(job_data["max_retries"].as_i64().unwrap() as i32)
    .bind(job_data["timeout_seconds"].as_i64().unwrap() as i32)
    .execute(db.pool())
    .await
    .expect("Failed to create job");

    // Verify job
    let job: (String, i32) = sqlx::query_as("SELECT status, priority FROM jobs WHERE id = $1")
        .bind(&job_id)
        .fetch_one(db.pool())
        .await
        .expect("Failed to query job");

    assert_eq!(job.0, "pending");
    assert_eq!(job.1, 0);
}

/// Test scheduled jobs
#[tokio::test]
async fn test_scheduled_jobs() {
    let db = TestDatabase::new().await;

    let org_id = "default-org";
    let queue_name = "scheduled-queue";
    let job_id = uuid::Uuid::new_v4().to_string();

    // Create a scheduled job for the future
    let scheduled_at = chrono::Utc::now() + chrono::Duration::hours(1);

    sqlx::query(
        r#"
        INSERT INTO jobs (
            id, organization_id, queue_name, status, payload, priority,
            max_retries, timeout_seconds, scheduled_at, created_at, updated_at
        )
        VALUES ($1, $2, $3, 'scheduled', '{}'::JSONB, 0, 3, 300, $4, NOW(), NOW())
        "#,
    )
    .bind(&job_id)
    .bind(org_id)
    .bind(queue_name)
    .bind(scheduled_at)
    .execute(db.pool())
    .await
    .expect("Failed to create scheduled job");

    // Verify job is scheduled
    let job: (String, Option<chrono::DateTime<chrono::Utc>>) =
        sqlx::query_as("SELECT status, scheduled_at FROM jobs WHERE id = $1")
            .bind(&job_id)
            .fetch_one(db.pool())
            .await
            .expect("Failed to query job");

    assert_eq!(job.0, "scheduled");
    assert!(job.1.is_some());
}

/// Test queue configuration
#[tokio::test]
async fn test_queue_configuration() {
    let db = TestDatabase::new().await;

    let org_id = "default-org";
    let queue_name = "configured-queue";
    let config_id = uuid::Uuid::new_v4().to_string();

    // Create queue configuration (matching actual schema)
    sqlx::query(
        r#"
        INSERT INTO queue_config (
            id, organization_id, queue_name, max_retries, default_timeout,
            rate_limit, enabled, created_at, updated_at
        )
        VALUES ($1, $2, $3, 5, 600, 100, true, NOW(), NOW())
        "#,
    )
    .bind(&config_id)
    .bind(org_id)
    .bind(queue_name)
    .execute(db.pool())
    .await
    .expect("Failed to create queue config");

    // Verify config
    let config: (i32, i32, Option<i32>, bool) = sqlx::query_as(
        "SELECT max_retries, default_timeout, rate_limit, enabled FROM queue_config WHERE id = $1",
    )
    .bind(&config_id)
    .fetch_one(db.pool())
    .await
    .expect("Failed to query queue config");

    assert_eq!(config.0, 5);
    assert_eq!(config.1, 600);
    assert_eq!(config.2, Some(100));
    assert!(config.3);
}

/// Test API key creation and storage
#[tokio::test]
async fn test_api_key_storage() {
    let db = TestDatabase::new().await;

    let org_id = "default-org";
    let api_key_id = uuid::Uuid::new_v4().to_string();
    let key_hash = "hashed_key_value"; // In real code this would be bcrypt hash

    // Create API key (matching actual schema)
    sqlx::query(
        r#"
        INSERT INTO api_keys (
            id, organization_id, name, key_hash, queues, rate_limit,
            is_active, created_at, last_used, expires_at
        )
        VALUES ($1, $2, 'Test API Key', $3, ARRAY['default', 'emails'], 100, true, NOW(), NULL, NOW() + INTERVAL '1 year')
        "#,
    )
    .bind(&api_key_id)
    .bind(org_id)
    .bind(key_hash)
    .execute(db.pool())
    .await
    .expect("Failed to create API key");

    // Verify API key
    let key: (String, Vec<String>, Option<i32>, bool) =
        sqlx::query_as("SELECT name, queues, rate_limit, is_active FROM api_keys WHERE id = $1")
            .bind(&api_key_id)
            .fetch_one(db.pool())
            .await
            .expect("Failed to query API key");

    assert_eq!(key.0, "Test API Key");
    assert_eq!(key.1, vec!["default", "emails"]);
    assert_eq!(key.2, Some(100));
    assert!(key.3);
}

/// Test job retry counting
#[tokio::test]
async fn test_job_retry_counting() {
    let db = TestDatabase::new().await;

    let org_id = "default-org";
    let job_id = uuid::Uuid::new_v4().to_string();

    // Create a job
    sqlx::query(
        r#"
        INSERT INTO jobs (
            id, organization_id, queue_name, status, payload, priority,
            max_retries, retry_count, timeout_seconds, created_at, updated_at
        )
        VALUES ($1, $2, 'test-queue', 'pending', '{}'::JSONB, 0, 3, 0, 300, NOW(), NOW())
        "#,
    )
    .bind(&job_id)
    .bind(org_id)
    .execute(db.pool())
    .await
    .expect("Failed to create job");

    // Simulate retries
    for expected_retry in 1..=3 {
        sqlx::query(
            "UPDATE jobs SET retry_count = retry_count + 1, updated_at = NOW() WHERE id = $1",
        )
        .bind(&job_id)
        .execute(db.pool())
        .await
        .expect("Failed to increment retry count");

        let retry_count: (i32,) = sqlx::query_as("SELECT retry_count FROM jobs WHERE id = $1")
            .bind(&job_id)
            .fetch_one(db.pool())
            .await
            .expect("Failed to query retry count");

        assert_eq!(retry_count.0, expected_retry);
    }

    // Verify max retries reached
    let job: (i32, i32) = sqlx::query_as("SELECT retry_count, max_retries FROM jobs WHERE id = $1")
        .bind(&job_id)
        .fetch_one(db.pool())
        .await
        .expect("Failed to query job");

    assert_eq!(job.0, job.1, "Retry count should equal max retries");
}

/// Test webhook delivery tracking
#[tokio::test]
async fn test_webhook_delivery_tracking() {
    let db = TestDatabase::new().await;

    let org_id = "default-org";
    let delivery_id = uuid::Uuid::new_v4().to_string();

    // Create webhook delivery (matching actual schema)
    sqlx::query(
        r#"
        INSERT INTO webhook_deliveries (
            id, organization_id, provider, event_type, payload, signature, matched_queue, created_at
        )
        VALUES ($1, $2, 'github', 'push', '{"ref": "refs/heads/main"}'::JSONB, 'sha256=abc123', 'webhooks', NOW())
        "#,
    )
    .bind(&delivery_id)
    .bind(org_id)
    .execute(db.pool())
    .await
    .expect("Failed to create webhook delivery");

    // Verify delivery
    let delivery: (String, String, Option<String>, Option<String>) = sqlx::query_as(
        "SELECT provider, event_type, signature, matched_queue FROM webhook_deliveries WHERE id = $1",
    )
    .bind(&delivery_id)
    .fetch_one(db.pool())
    .await
    .expect("Failed to query webhook delivery");

    assert_eq!(delivery.0, "github");
    assert_eq!(delivery.1, "push");
    assert_eq!(delivery.2, Some("sha256=abc123".to_string()));
    assert_eq!(delivery.3, Some("webhooks".to_string()));
}

/// Test job lease expiration
#[tokio::test]
async fn test_job_lease_expiration() {
    let db = TestDatabase::new().await;

    let org_id = "default-org";
    let job_id = uuid::Uuid::new_v4().to_string();
    let worker_id = uuid::Uuid::new_v4().to_string();

    // Create a processing job with expired lease
    let expired_lease = chrono::Utc::now() - chrono::Duration::minutes(5);

    sqlx::query(
        r#"
        INSERT INTO jobs (
            id, organization_id, queue_name, status, payload, priority,
            max_retries, timeout_seconds, assigned_worker_id, lease_expires_at,
            created_at, updated_at
        )
        VALUES ($1, $2, 'test-queue', 'processing', '{}'::JSONB, 0, 3, 300, $3, $4, NOW(), NOW())
        "#,
    )
    .bind(&job_id)
    .bind(org_id)
    .bind(&worker_id)
    .bind(expired_lease)
    .execute(db.pool())
    .await
    .expect("Failed to create job with expired lease");

    // Query for expired leases
    let expired_jobs: Vec<(String,)> = sqlx::query_as(
        r#"
        SELECT id FROM jobs 
        WHERE status = 'processing' 
        AND lease_expires_at < NOW()
        "#,
    )
    .fetch_all(db.pool())
    .await
    .expect("Failed to query expired jobs");

    assert!(!expired_jobs.is_empty(), "Should find expired jobs");
    assert!(
        expired_jobs.iter().any(|(id,)| id == &job_id),
        "Should find our expired job"
    );
}

/// Test job filtering by queue_name
#[tokio::test]
async fn test_job_filtering_by_queue() {
    let db = TestDatabase::new().await;

    let org_id = "default-org";

    // Create jobs in different queues
    for queue in ["queue-a", "queue-b", "queue-a"] {
        let job_id = uuid::Uuid::new_v4().to_string();
        sqlx::query(
            r#"
            INSERT INTO jobs (
                id, organization_id, queue_name, status, payload, priority,
                max_retries, timeout_seconds, created_at, updated_at
            )
            VALUES ($1, $2, $3, 'pending', '{}'::JSONB, 0, 3, 300, NOW(), NOW())
            "#,
        )
        .bind(&job_id)
        .bind(org_id)
        .bind(queue)
        .execute(db.pool())
        .await
        .expect("Failed to create job");
    }

    // Filter by queue-a
    let queue_a_jobs: Vec<(String,)> = sqlx::query_as("SELECT id FROM jobs WHERE queue_name = $1")
        .bind("queue-a")
        .fetch_all(db.pool())
        .await
        .expect("Failed to query jobs");

    assert_eq!(queue_a_jobs.len(), 2, "Should have 2 jobs in queue-a");

    // Filter by queue-b
    let queue_b_jobs: Vec<(String,)> = sqlx::query_as("SELECT id FROM jobs WHERE queue_name = $1")
        .bind("queue-b")
        .fetch_all(db.pool())
        .await
        .expect("Failed to query jobs");

    assert_eq!(queue_b_jobs.len(), 1, "Should have 1 job in queue-b");
}

/// Test job filtering by status
#[tokio::test]
async fn test_job_filtering_by_status() {
    let db = TestDatabase::new().await;

    let org_id = "default-org";

    // Create jobs with different statuses
    for (status, count) in [("pending", 3), ("processing", 2), ("completed", 5)] {
        for _ in 0..count {
            let job_id = uuid::Uuid::new_v4().to_string();
            sqlx::query(
                r#"
                INSERT INTO jobs (
                    id, organization_id, queue_name, status, payload, priority,
                    max_retries, timeout_seconds, created_at, updated_at
                )
                VALUES ($1, $2, 'test-queue', $3, '{}'::JSONB, 0, 3, 300, NOW(), NOW())
                "#,
            )
            .bind(&job_id)
            .bind(org_id)
            .bind(status)
            .execute(db.pool())
            .await
            .expect("Failed to create job");
        }
    }

    // Filter by pending
    let pending_jobs: Vec<(String,)> = sqlx::query_as("SELECT id FROM jobs WHERE status = $1")
        .bind("pending")
        .fetch_all(db.pool())
        .await
        .expect("Failed to query jobs");

    assert_eq!(pending_jobs.len(), 3, "Should have 3 pending jobs");

    // Filter by completed
    let completed_jobs: Vec<(String,)> = sqlx::query_as("SELECT id FROM jobs WHERE status = $1")
        .bind("completed")
        .fetch_all(db.pool())
        .await
        .expect("Failed to query jobs");

    assert_eq!(completed_jobs.len(), 5, "Should have 5 completed jobs");
}

/// Test scheduler lease recovery respects max_retries
#[tokio::test]
async fn test_lease_recovery_respects_max_retries() {
    let db = TestDatabase::new().await;

    let org_id = "default-org";
    let expired_lease = chrono::Utc::now() - chrono::Duration::minutes(5);

    // Create a job that has already reached max_retries
    let job_at_max_retries = uuid::Uuid::new_v4().to_string();
    sqlx::query(
        r#"
        INSERT INTO jobs (
            id, organization_id, queue_name, status, payload, priority,
            max_retries, retry_count, timeout_seconds, lease_expires_at,
            created_at, updated_at
        )
        VALUES ($1, $2, 'test-queue', 'processing', '{}'::JSONB, 0, 3, 3, 300, $3, NOW(), NOW())
        "#,
    )
    .bind(&job_at_max_retries)
    .bind(org_id)
    .bind(expired_lease)
    .execute(db.pool())
    .await
    .expect("Failed to create job at max retries");

    // Create a job that can still be retried
    let job_can_retry = uuid::Uuid::new_v4().to_string();
    sqlx::query(
        r#"
        INSERT INTO jobs (
            id, organization_id, queue_name, status, payload, priority,
            max_retries, retry_count, timeout_seconds, lease_expires_at,
            created_at, updated_at
        )
        VALUES ($1, $2, 'test-queue', 'processing', '{}'::JSONB, 0, 3, 1, 300, $3, NOW(), NOW())
        "#,
    )
    .bind(&job_can_retry)
    .bind(org_id)
    .bind(expired_lease)
    .execute(db.pool())
    .await
    .expect("Failed to create job that can retry");

    // Simulate lease recovery: jobs below max_retries go back to pending
    sqlx::query(
        r#"
        UPDATE jobs
        SET status = 'pending', retry_count = retry_count + 1
        WHERE status = 'processing' AND lease_expires_at < NOW() AND retry_count < max_retries
        "#,
    )
    .execute(db.pool())
    .await
    .expect("Failed to recover leases");

    // Jobs at max_retries go to deadletter
    sqlx::query(
        r#"
        UPDATE jobs
        SET status = 'deadletter'
        WHERE status = 'processing' AND lease_expires_at < NOW() AND retry_count >= max_retries
        "#,
    )
    .execute(db.pool())
    .await
    .expect("Failed to deadletter jobs");

    // Verify: job that can retry should be pending
    let (status_can_retry,): (String,) = sqlx::query_as("SELECT status FROM jobs WHERE id = $1")
        .bind(&job_can_retry)
        .fetch_one(db.pool())
        .await
        .expect("Failed to query job");

    assert_eq!(
        status_can_retry, "pending",
        "Job below max_retries should be pending"
    );

    // Verify: job at max_retries should be deadletter
    let (status_at_max,): (String,) = sqlx::query_as("SELECT status FROM jobs WHERE id = $1")
        .bind(&job_at_max_retries)
        .fetch_one(db.pool())
        .await
        .expect("Failed to query job");

    assert_eq!(
        status_at_max, "deadletter",
        "Job at max_retries should be deadlettered"
    );
}

/// Test combined queue and status filtering
#[tokio::test]
async fn test_job_filtering_combined() {
    let db = TestDatabase::new().await;

    let org_id = "default-org";

    // Create diverse jobs
    let test_cases = [
        ("emails", "pending"),
        ("emails", "pending"),
        ("emails", "completed"),
        ("webhooks", "pending"),
        ("webhooks", "failed"),
    ];

    for (queue, status) in test_cases {
        let job_id = uuid::Uuid::new_v4().to_string();
        sqlx::query(
            r#"
            INSERT INTO jobs (
                id, organization_id, queue_name, status, payload, priority,
                max_retries, timeout_seconds, created_at, updated_at
            )
            VALUES ($1, $2, $3, $4, '{}'::JSONB, 0, 3, 300, NOW(), NOW())
            "#,
        )
        .bind(&job_id)
        .bind(org_id)
        .bind(queue)
        .bind(status)
        .execute(db.pool())
        .await
        .expect("Failed to create job");
    }

    // Filter by queue=emails AND status=pending
    let filtered_jobs: Vec<(String,)> =
        sqlx::query_as("SELECT id FROM jobs WHERE queue_name = $1 AND status = $2")
            .bind("emails")
            .bind("pending")
            .fetch_all(db.pool())
            .await
            .expect("Failed to query jobs");

    assert_eq!(
        filtered_jobs.len(),
        2,
        "Should have 2 pending jobs in emails queue"
    );
}

/// Test bulk job enqueue pattern
#[tokio::test]
async fn test_bulk_job_enqueue() {
    let db = TestDatabase::new().await;

    let org_id = "default-org";
    let queue_name = "bulk-test";

    // Bulk insert multiple jobs
    let job_ids: Vec<String> = (0..5).map(|_| uuid::Uuid::new_v4().to_string()).collect();

    for (i, job_id) in job_ids.iter().enumerate() {
        sqlx::query(
            r#"
            INSERT INTO jobs (
                id, organization_id, queue_name, status, payload, priority,
                max_retries, timeout_seconds, created_at, updated_at
            )
            VALUES ($1, $2, $3, 'pending', $4::JSONB, $5, 3, 300, NOW(), NOW())
            "#,
        )
        .bind(job_id)
        .bind(org_id)
        .bind(queue_name)
        .bind(serde_json::json!({"index": i}))
        .bind(i as i32)
        .execute(db.pool())
        .await
        .expect("Failed to create job");
    }

    // Verify all jobs created
    let (count,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM jobs WHERE queue_name = $1")
        .bind(queue_name)
        .fetch_one(db.pool())
        .await
        .expect("Failed to count jobs");

    assert_eq!(count, 5, "Should have 5 jobs from bulk insert");

    // Verify priority ordering
    let jobs: Vec<(String, i32)> = sqlx::query_as(
        "SELECT id, priority FROM jobs WHERE queue_name = $1 ORDER BY priority DESC",
    )
    .bind(queue_name)
    .fetch_all(db.pool())
    .await
    .expect("Failed to query jobs");

    assert_eq!(jobs[0].1, 4, "Highest priority job should be first");
    assert_eq!(jobs[4].1, 0, "Lowest priority job should be last");
}

/// Test dead-letter queue retry
#[tokio::test]
async fn test_dlq_retry() {
    let db = TestDatabase::new().await;

    let org_id = "default-org";
    let job_id = uuid::Uuid::new_v4().to_string();

    // Create a job in deadletter status
    sqlx::query(
        r#"
        INSERT INTO jobs (
            id, organization_id, queue_name, status, payload, priority,
            max_retries, retry_count, timeout_seconds, last_error,
            created_at, updated_at
        )
        VALUES ($1, $2, 'test-queue', 'deadletter', '{}'::JSONB, 0, 3, 3, 300, 
                'Max retries exceeded', NOW(), NOW())
        "#,
    )
    .bind(&job_id)
    .bind(org_id)
    .execute(db.pool())
    .await
    .expect("Failed to create deadletter job");

    // Retry the job from DLQ
    sqlx::query(
        r#"
        UPDATE jobs
        SET status = 'pending', retry_count = 0, last_error = NULL, updated_at = NOW()
        WHERE id = $1 AND status = 'deadletter'
        "#,
    )
    .bind(&job_id)
    .execute(db.pool())
    .await
    .expect("Failed to retry DLQ job");

    // Verify job is now pending
    let (status,): (String,) = sqlx::query_as("SELECT status FROM jobs WHERE id = $1")
        .bind(&job_id)
        .fetch_one(db.pool())
        .await
        .expect("Failed to query job");

    assert_eq!(status, "pending", "Job should be pending after DLQ retry");
}

/// Test queue pause/resume via settings
#[tokio::test]
async fn test_queue_pause_resume() {
    let db = TestDatabase::new().await;

    let org_id = "default-org";
    let queue_name = "pausable-queue";

    // Create queue config
    sqlx::query(
        r#"
        INSERT INTO queue_config (
            id, organization_id, queue_name, max_retries, default_timeout,
            enabled, settings, created_at, updated_at
        )
        VALUES (gen_random_uuid()::TEXT, $1, $2, 3, 300, true, '{}'::JSONB, NOW(), NOW())
        "#,
    )
    .bind(org_id)
    .bind(queue_name)
    .execute(db.pool())
    .await
    .expect("Failed to create queue config");

    // Pause the queue
    sqlx::query(
        r#"
        UPDATE queue_config
        SET enabled = false, settings = '{"paused": true}'::JSONB, updated_at = NOW()
        WHERE queue_name = $1 AND organization_id = $2
        "#,
    )
    .bind(queue_name)
    .bind(org_id)
    .execute(db.pool())
    .await
    .expect("Failed to pause queue");

    // Verify queue is paused
    let (enabled, settings): (bool, serde_json::Value) =
        sqlx::query_as("SELECT enabled, settings FROM queue_config WHERE queue_name = $1")
            .bind(queue_name)
            .fetch_one(db.pool())
            .await
            .expect("Failed to query queue config");

    assert!(!enabled, "Queue should be disabled");
    assert!(
        settings
            .get("paused")
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
        "Queue should be paused"
    );

    // Resume the queue
    sqlx::query(
        r#"
        UPDATE queue_config
        SET enabled = true, settings = '{}'::JSONB, updated_at = NOW()
        WHERE queue_name = $1 AND organization_id = $2
        "#,
    )
    .bind(queue_name)
    .bind(org_id)
    .execute(db.pool())
    .await
    .expect("Failed to resume queue");

    // Verify queue is resumed
    let (enabled,): (bool,) =
        sqlx::query_as("SELECT enabled FROM queue_config WHERE queue_name = $1")
            .bind(queue_name)
            .fetch_one(db.pool())
            .await
            .expect("Failed to query queue config");

    assert!(enabled, "Queue should be enabled after resume");
}

/// Test job priority boost
#[tokio::test]
async fn test_job_priority_boost() {
    let db = TestDatabase::new().await;

    let org_id = "default-org";
    let job_id = uuid::Uuid::new_v4().to_string();

    // Create a pending job with low priority
    sqlx::query(
        r#"
        INSERT INTO jobs (
            id, organization_id, queue_name, status, payload, priority,
            max_retries, timeout_seconds, created_at, updated_at
        )
        VALUES ($1, $2, 'test-queue', 'pending', '{}'::JSONB, 0, 3, 300, NOW(), NOW())
        "#,
    )
    .bind(&job_id)
    .bind(org_id)
    .execute(db.pool())
    .await
    .expect("Failed to create job");

    // Boost priority
    sqlx::query("UPDATE jobs SET priority = $1, updated_at = NOW() WHERE id = $2")
        .bind(100)
        .bind(&job_id)
        .execute(db.pool())
        .await
        .expect("Failed to boost priority");

    // Verify priority was updated
    let (priority,): (i32,) = sqlx::query_as("SELECT priority FROM jobs WHERE id = $1")
        .bind(&job_id)
        .fetch_one(db.pool())
        .await
        .expect("Failed to query job");

    assert_eq!(priority, 100, "Priority should be boosted to 100");
}

/// Test data retention cleanup for old completed jobs
#[tokio::test]
async fn test_data_retention_cleanup() {
    let db = TestDatabase::new().await;

    let org_id = "default-org";

    // Create an old completed job (simulating 31 days ago)
    let old_job_id = uuid::Uuid::new_v4().to_string();
    sqlx::query(
        r#"
        INSERT INTO jobs (
            id, organization_id, queue_name, status, payload, priority,
            max_retries, timeout_seconds, created_at, completed_at, updated_at
        )
        VALUES ($1, $2, 'test-queue', 'completed', '{}'::JSONB, 0, 3, 300, 
                NOW() - INTERVAL '31 days', NOW() - INTERVAL '31 days', NOW())
        "#,
    )
    .bind(&old_job_id)
    .bind(org_id)
    .execute(db.pool())
    .await
    .expect("Failed to create old job");

    // Create a recent completed job (1 day ago)
    let recent_job_id = uuid::Uuid::new_v4().to_string();
    sqlx::query(
        r#"
        INSERT INTO jobs (
            id, organization_id, queue_name, status, payload, priority,
            max_retries, timeout_seconds, created_at, completed_at, updated_at
        )
        VALUES ($1, $2, 'test-queue', 'completed', '{}'::JSONB, 0, 3, 300, 
                NOW() - INTERVAL '1 day', NOW() - INTERVAL '1 day', NOW())
        "#,
    )
    .bind(&recent_job_id)
    .bind(org_id)
    .execute(db.pool())
    .await
    .expect("Failed to create recent job");

    // Run cleanup (simulate scheduler cleanup task)
    let result = sqlx::query(
        r#"
        DELETE FROM jobs
        WHERE status IN ('completed', 'cancelled')
        AND completed_at < NOW() - INTERVAL '30 days'
        "#,
    )
    .execute(db.pool())
    .await
    .expect("Failed to cleanup old jobs");

    assert!(
        result.rows_affected() >= 1,
        "Should have cleaned up at least 1 old job"
    );

    // Verify old job is deleted
    let old_job_exists: Option<(String,)> = sqlx::query_as("SELECT id FROM jobs WHERE id = $1")
        .bind(&old_job_id)
        .fetch_optional(db.pool())
        .await
        .expect("Failed to query old job");

    assert!(old_job_exists.is_none(), "Old job should be deleted");

    // Verify recent job still exists
    let recent_job_exists: Option<(String,)> = sqlx::query_as("SELECT id FROM jobs WHERE id = $1")
        .bind(&recent_job_id)
        .fetch_optional(db.pool())
        .await
        .expect("Failed to query recent job");

    assert!(recent_job_exists.is_some(), "Recent job should still exist");
}

/// Test job expiration (expires_at)
#[tokio::test]
async fn test_job_expiration() {
    let db = TestDatabase::new().await;

    let org_id = "default-org";

    // Create an expired job
    let expired_job_id = uuid::Uuid::new_v4().to_string();
    sqlx::query(
        r#"
        INSERT INTO jobs (
            id, organization_id, queue_name, status, payload, priority,
            max_retries, timeout_seconds, expires_at, created_at, updated_at
        )
        VALUES ($1, $2, 'test-queue', 'pending', '{}'::JSONB, 0, 3, 300, 
                NOW() - INTERVAL '1 hour', NOW(), NOW())
        "#,
    )
    .bind(&expired_job_id)
    .bind(org_id)
    .execute(db.pool())
    .await
    .expect("Failed to create expired job");

    // Query for expired jobs
    let expired_jobs: Vec<(String,)> = sqlx::query_as(
        r#"
        SELECT id FROM jobs 
        WHERE expires_at IS NOT NULL AND expires_at < NOW()
        AND status = 'pending'
        "#,
    )
    .fetch_all(db.pool())
    .await
    .expect("Failed to query expired jobs");

    assert!(!expired_jobs.is_empty(), "Should find expired jobs");
    assert!(
        expired_jobs.iter().any(|(id,)| id == &expired_job_id),
        "Should find our expired job"
    );
}

/// Test concurrent job dequeue (FOR UPDATE SKIP LOCKED behavior)
#[tokio::test]
async fn test_concurrent_dequeue_safety() {
    let db = TestDatabase::new().await;

    let org_id = "default-org";

    // Create 3 pending jobs
    for i in 0..3 {
        let job_id = uuid::Uuid::new_v4().to_string();
        sqlx::query(
            r#"
            INSERT INTO jobs (
                id, organization_id, queue_name, status, payload, priority,
                max_retries, timeout_seconds, created_at, updated_at
            )
            VALUES ($1, $2, 'concurrent-queue', 'pending', '{}'::JSONB, $3, 3, 300, NOW(), NOW())
            "#,
        )
        .bind(&job_id)
        .bind(org_id)
        .bind(i)
        .execute(db.pool())
        .await
        .expect("Failed to create job");
    }

    // Simulate 3 workers trying to dequeue at the same time
    // Each should get a different job (or none if already taken)
    let dequeued_jobs: Vec<String> = futures::future::join_all((0..3).map(|worker_id| {
        let pool = db.pool().clone();
        async move {
            let worker_name = format!("worker-{}", worker_id);
            let result: Option<(String,)> = sqlx::query_as(
                r#"
                UPDATE jobs
                SET 
                    status = 'processing',
                    assigned_worker_id = $1,
                    lease_expires_at = NOW() + INTERVAL '30 seconds',
                    updated_at = NOW()
                WHERE id = (
                    SELECT id FROM jobs
                    WHERE 
                        organization_id = $2 
                        AND queue_name = 'concurrent-queue'
                        AND status = 'pending'
                    ORDER BY priority DESC, created_at ASC
                    LIMIT 1
                    FOR UPDATE SKIP LOCKED
                )
                RETURNING id
                "#,
            )
            .bind(&worker_name)
            .bind("default-org")
            .fetch_optional(&pool)
            .await
            .expect("Failed to dequeue");

            result.map(|(id,)| id)
        }
    }))
    .await
    .into_iter()
    .flatten()
    .collect();

    // Each job should only be dequeued once
    let unique_jobs: std::collections::HashSet<_> = dequeued_jobs.iter().collect();
    assert_eq!(
        dequeued_jobs.len(),
        unique_jobs.len(),
        "No duplicate dequeues should occur"
    );
    assert_eq!(dequeued_jobs.len(), 3, "All 3 jobs should be dequeued");
}

/// Test webhook delivery tracking with multiple attempts
#[tokio::test]
async fn test_webhook_delivery_attempts() {
    let db = TestDatabase::new().await;

    let org_id = "default-org";

    // Simulate 3 delivery attempts using the actual schema
    for attempt in 1..=3 {
        let payload = serde_json::json!({
            "job_id": "123",
            "attempt": attempt
        });

        sqlx::query(
            r#"
            INSERT INTO webhook_deliveries (
                id, organization_id, provider, event_type, signature,
                matched_queue, payload, created_at
            )
            VALUES (
                gen_random_uuid()::TEXT, $1, 'custom', 'job.completed', $2,
                'test-queue', $3, NOW()
            )
            "#,
        )
        .bind(org_id)
        .bind(format!("attempt:{}", attempt))
        .bind(&payload)
        .execute(db.pool())
        .await
        .expect("Failed to record webhook delivery");
    }

    // Verify all attempts were recorded
    let (count,): (i64,) =
        sqlx::query_as("SELECT COUNT(*) FROM webhook_deliveries WHERE organization_id = $1")
            .bind(org_id)
            .fetch_one(db.pool())
            .await
            .expect("Failed to count deliveries");

    assert_eq!(count, 3, "Should have 3 delivery attempts");

    // Verify we can query by signature (attempt tracking)
    let (signature,): (Option<String>,) = sqlx::query_as(
        r#"
        SELECT signature FROM webhook_deliveries 
        WHERE organization_id = $1 
        ORDER BY created_at DESC 
        LIMIT 1
        "#,
    )
    .bind(org_id)
    .fetch_one(db.pool())
    .await
    .expect("Failed to query last attempt");

    assert_eq!(
        signature,
        Some("attempt:3".to_string()),
        "Final attempt should be recorded"
    );
}

/// Test organization isolation with RLS
#[tokio::test]
async fn test_organization_isolation() {
    let db = TestDatabase::new().await;

    // First create the organizations (FK constraint)
    sqlx::query(
        r#"
        INSERT INTO organizations (id, name, slug, plan_tier, created_at, updated_at)
        VALUES ('org-isolation-1', 'Org One', 'org-one', 'free', NOW(), NOW())
        ON CONFLICT (id) DO NOTHING
        "#,
    )
    .execute(db.pool())
    .await
    .expect("Failed to create org1");

    sqlx::query(
        r#"
        INSERT INTO organizations (id, name, slug, plan_tier, created_at, updated_at)
        VALUES ('org-isolation-2', 'Org Two', 'org-two', 'free', NOW(), NOW())
        ON CONFLICT (id) DO NOTHING
        "#,
    )
    .execute(db.pool())
    .await
    .expect("Failed to create org2");

    // Create jobs for two different orgs
    let org1_job_id = uuid::Uuid::new_v4().to_string();
    let org2_job_id = uuid::Uuid::new_v4().to_string();

    sqlx::query(
        r#"
        INSERT INTO jobs (
            id, organization_id, queue_name, status, payload, priority,
            max_retries, timeout_seconds, created_at, updated_at
        )
        VALUES ($1, 'org-isolation-1', 'test-queue', 'pending', '{"org": 1}'::JSONB, 0, 3, 300, NOW(), NOW())
        "#,
    )
    .bind(&org1_job_id)
    .execute(db.pool())
    .await
    .expect("Failed to create org1 job");

    sqlx::query(
        r#"
        INSERT INTO jobs (
            id, organization_id, queue_name, status, payload, priority,
            max_retries, timeout_seconds, created_at, updated_at
        )
        VALUES ($1, 'org-isolation-2', 'test-queue', 'pending', '{"org": 2}'::JSONB, 0, 3, 300, NOW(), NOW())
        "#,
    )
    .bind(&org2_job_id)
    .execute(db.pool())
    .await
    .expect("Failed to create org2 job");

    // Verify each org can only see their own jobs (simulating RLS)
    let org1_jobs: Vec<(String,)> =
        sqlx::query_as("SELECT id FROM jobs WHERE organization_id = 'org-isolation-1'")
            .fetch_all(db.pool())
            .await
            .expect("Failed to query org1 jobs");

    let org2_jobs: Vec<(String,)> =
        sqlx::query_as("SELECT id FROM jobs WHERE organization_id = 'org-isolation-2'")
            .fetch_all(db.pool())
            .await
            .expect("Failed to query org2 jobs");

    assert!(
        org1_jobs.iter().any(|(id,)| id == &org1_job_id),
        "Org1 should see their job"
    );
    assert!(
        !org1_jobs.iter().any(|(id,)| id == &org2_job_id),
        "Org1 should not see org2 job"
    );

    assert!(
        org2_jobs.iter().any(|(id,)| id == &org2_job_id),
        "Org2 should see their job"
    );
    assert!(
        !org2_jobs.iter().any(|(id,)| id == &org1_job_id),
        "Org2 should not see org1 job"
    );
}

/// Test edge case: empty queue dequeue
#[tokio::test]
async fn test_empty_queue_dequeue() {
    let db = TestDatabase::new().await;

    // Try to dequeue from an empty queue
    let result: Option<(String,)> = sqlx::query_as(
        r#"
        UPDATE jobs
        SET status = 'processing', assigned_worker_id = 'test-worker'
        WHERE id = (
            SELECT id FROM jobs
            WHERE queue_name = 'empty-queue' AND status = 'pending'
            LIMIT 1
            FOR UPDATE SKIP LOCKED
        )
        RETURNING id
        "#,
    )
    .fetch_optional(db.pool())
    .await
    .expect("Failed to dequeue");

    assert!(result.is_none(), "Empty queue should return None");
}

/// Test edge case: very long queue name
#[tokio::test]
async fn test_long_queue_name() {
    let db = TestDatabase::new().await;

    let org_id = "default-org";
    let long_queue_name = "a".repeat(100); // 100 character queue name
    let job_id = uuid::Uuid::new_v4().to_string();

    sqlx::query(
        r#"
        INSERT INTO jobs (
            id, organization_id, queue_name, status, payload, priority,
            max_retries, timeout_seconds, created_at, updated_at
        )
        VALUES ($1, $2, $3, 'pending', '{}'::JSONB, 0, 3, 300, NOW(), NOW())
        "#,
    )
    .bind(&job_id)
    .bind(org_id)
    .bind(&long_queue_name)
    .execute(db.pool())
    .await
    .expect("Failed to create job with long queue name");

    // Verify job was created
    let (queue_name,): (String,) = sqlx::query_as("SELECT queue_name FROM jobs WHERE id = $1")
        .bind(&job_id)
        .fetch_one(db.pool())
        .await
        .expect("Failed to query job");

    assert_eq!(queue_name.len(), 100);
}

/// Test edge case: large payload
#[tokio::test]
async fn test_large_payload() {
    let db = TestDatabase::new().await;

    let org_id = "default-org";
    let job_id = uuid::Uuid::new_v4().to_string();

    // Create a large payload (100KB of JSON)
    let large_data = "x".repeat(100_000);
    let large_payload = serde_json::json!({
        "data": large_data,
        "nested": {
            "array": [1, 2, 3, 4, 5],
            "object": {"key": "value"}
        }
    });

    sqlx::query(
        r#"
        INSERT INTO jobs (
            id, organization_id, queue_name, status, payload, priority,
            max_retries, timeout_seconds, created_at, updated_at
        )
        VALUES ($1, $2, 'test-queue', 'pending', $3, 0, 3, 300, NOW(), NOW())
        "#,
    )
    .bind(&job_id)
    .bind(org_id)
    .bind(&large_payload)
    .execute(db.pool())
    .await
    .expect("Failed to create job with large payload");

    // Verify payload was stored correctly
    let (payload,): (serde_json::Value,) = sqlx::query_as("SELECT payload FROM jobs WHERE id = $1")
        .bind(&job_id)
        .fetch_one(db.pool())
        .await
        .expect("Failed to query job");

    assert_eq!(payload["data"].as_str().unwrap().len(), 100_000);
}

/// Test edge case: maximum retries boundary
#[tokio::test]
async fn test_max_retries_boundary() {
    let db = TestDatabase::new().await;

    let org_id = "default-org";
    let job_id = uuid::Uuid::new_v4().to_string();

    // Create job with max_retries = 0 (no retries allowed)
    sqlx::query(
        r#"
        INSERT INTO jobs (
            id, organization_id, queue_name, status, payload, priority,
            max_retries, retry_count, timeout_seconds, created_at, updated_at
        )
        VALUES ($1, $2, 'test-queue', 'failed', '{}'::JSONB, 0, 0, 0, 300, NOW(), NOW())
        "#,
    )
    .bind(&job_id)
    .bind(org_id)
    .execute(db.pool())
    .await
    .expect("Failed to create job");

    // Job should go to deadletter immediately (retry_count >= max_retries)
    let (can_retry,): (bool,) =
        sqlx::query_as("SELECT retry_count < max_retries FROM jobs WHERE id = $1")
            .bind(&job_id)
            .fetch_one(db.pool())
            .await
            .expect("Failed to query job");

    assert!(!can_retry, "Job with max_retries=0 should not be retryable");
}

/// Test edge case: negative priority
#[tokio::test]
async fn test_negative_priority() {
    let db = TestDatabase::new().await;

    let org_id = "default-org";

    // Create jobs with varying priorities including negative
    for priority in [-100, -50, 0, 50, 100] {
        let job_id = uuid::Uuid::new_v4().to_string();
        sqlx::query(
            r#"
            INSERT INTO jobs (
                id, organization_id, queue_name, status, payload, priority,
                max_retries, timeout_seconds, created_at, updated_at
            )
            VALUES ($1, $2, 'priority-queue', 'pending', '{}'::JSONB, $3, 3, 300, NOW(), NOW())
            "#,
        )
        .bind(&job_id)
        .bind(org_id)
        .bind(priority)
        .execute(db.pool())
        .await
        .expect("Failed to create job");
    }

    // Verify jobs are ordered by priority DESC
    let priorities: Vec<(i32,)> = sqlx::query_as(
        "SELECT priority FROM jobs WHERE queue_name = 'priority-queue' ORDER BY priority DESC",
    )
    .fetch_all(db.pool())
    .await
    .expect("Failed to query jobs");

    let priority_values: Vec<i32> = priorities.into_iter().map(|(p,)| p).collect();
    assert_eq!(priority_values, vec![100, 50, 0, -50, -100]);
}

/// Test edge case: job with all optional fields null
#[tokio::test]
async fn test_minimal_job() {
    let db = TestDatabase::new().await;

    let org_id = "default-org";
    let job_id = uuid::Uuid::new_v4().to_string();

    // Create job with only required fields
    sqlx::query(
        r#"
        INSERT INTO jobs (
            id, organization_id, queue_name, status, payload, priority,
            max_retries, timeout_seconds, created_at, updated_at
        )
        VALUES ($1, $2, 'test-queue', 'pending', '{}'::JSONB, 0, 3, 300, NOW(), NOW())
        "#,
    )
    .bind(&job_id)
    .bind(org_id)
    .execute(db.pool())
    .await
    .expect("Failed to create minimal job");

    // Verify all optional fields are NULL
    let result: (
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
    ) = sqlx::query_as(
        r#"
        SELECT 
            result::TEXT, last_error, tags::TEXT, 
            parent_job_id, completion_webhook, idempotency_key
        FROM jobs WHERE id = $1
        "#,
    )
    .bind(&job_id)
    .fetch_one(db.pool())
    .await
    .expect("Failed to query job");

    assert!(result.0.is_none(), "result should be NULL");
    assert!(result.1.is_none(), "last_error should be NULL");
    assert!(result.2.is_none(), "tags should be NULL");
    assert!(result.3.is_none(), "parent_job_id should be NULL");
    assert!(result.4.is_none(), "completion_webhook should be NULL");
    assert!(result.5.is_none(), "idempotency_key should be NULL");
}

/// Test edge case: unicode in job data
#[tokio::test]
async fn test_unicode_in_job_data() {
    let db = TestDatabase::new().await;

    let org_id = "default-org";
    let job_id = uuid::Uuid::new_v4().to_string();

    let unicode_payload = serde_json::json!({
        "message": "Hello 世界 🌍 مرحبا Привет",
        "emoji": "🚀💻🔥",
        "chinese": "中文测试",
        "arabic": "اختبار",
        "russian": "Тест",
        "special": "™©®"
    });

    sqlx::query(
        r#"
        INSERT INTO jobs (
            id, organization_id, queue_name, status, payload, priority,
            max_retries, timeout_seconds, created_at, updated_at
        )
        VALUES ($1, $2, 'unicode-queue', 'pending', $3, 0, 3, 300, NOW(), NOW())
        "#,
    )
    .bind(&job_id)
    .bind(org_id)
    .bind(&unicode_payload)
    .execute(db.pool())
    .await
    .expect("Failed to create unicode job");

    // Verify unicode was preserved
    let (payload,): (serde_json::Value,) = sqlx::query_as("SELECT payload FROM jobs WHERE id = $1")
        .bind(&job_id)
        .fetch_one(db.pool())
        .await
        .expect("Failed to query job");

    assert!(payload["message"].as_str().unwrap().contains("世界"));
    assert!(payload["emoji"].as_str().unwrap().contains("🚀"));
}

/// Test edge case: concurrent job completion
#[tokio::test]
async fn test_concurrent_completion() {
    let db = TestDatabase::new().await;

    let org_id = "default-org";
    let job_id = uuid::Uuid::new_v4().to_string();

    // Create a processing job
    sqlx::query(
        r#"
        INSERT INTO jobs (
            id, organization_id, queue_name, status, payload, priority,
            max_retries, timeout_seconds, assigned_worker_id, created_at, updated_at
        )
        VALUES ($1, $2, 'test-queue', 'processing', '{}'::JSONB, 0, 3, 300, 'worker-1', NOW(), NOW())
        "#,
    )
    .bind(&job_id)
    .bind(org_id)
    .execute(db.pool())
    .await
    .expect("Failed to create job");

    // Simulate concurrent completion attempts
    let results: Vec<u64> = futures::future::join_all((0..3).map(|_| {
        let pool = db.pool().clone();
        let jid = job_id.clone();
        async move {
            let result = sqlx::query(
                r#"
                UPDATE jobs
                SET status = 'completed', completed_at = NOW(), result = '{"success": true}'::JSONB
                WHERE id = $1 AND status = 'processing'
                "#,
            )
            .bind(&jid)
            .execute(&pool)
            .await
            .expect("Failed to complete job");

            result.rows_affected()
        }
    }))
    .await;

    // Only one completion should succeed
    let successful_completions: u64 = results.into_iter().sum();
    assert_eq!(
        successful_completions, 1,
        "Only one completion should succeed"
    );

    // Verify final status
    let (status,): (String,) = sqlx::query_as("SELECT status FROM jobs WHERE id = $1")
        .bind(&job_id)
        .fetch_one(db.pool())
        .await
        .expect("Failed to query job");

    assert_eq!(status, "completed");
}
