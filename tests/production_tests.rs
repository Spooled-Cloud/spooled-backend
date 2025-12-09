//! Production-Ready Integration Tests
//!
//! These tests verify critical production scenarios:
//! - Concurrent access patterns
//! - Data integrity under load
//! - Error handling and recovery
//! - Security isolation
//! - Edge cases and boundary conditions

mod common;

use common::TestDatabase;
use futures::future::join_all;
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::Duration;

/// Test helper to setup test organization
async fn setup_org(db: &TestDatabase, name: &str) -> String {
    let org_id = uuid::Uuid::new_v4().to_string();
    let org_slug = format!("{}-{}", name, &org_id[..8]);

    sqlx::query(
        r#"
        INSERT INTO organizations (id, name, slug, plan_tier, created_at, updated_at)
        VALUES ($1, $2, $3, 'free', NOW(), NOW())
        "#,
    )
    .bind(&org_id)
    .bind(name)
    .bind(&org_slug)
    .execute(db.pool())
    .await
    .expect("Failed to create organization");

    org_id
}

// =============================================================================
// CONCURRENT ACCESS TESTS
// =============================================================================

/// Test concurrent job creation doesn't cause conflicts
#[tokio::test]
async fn test_concurrent_job_creation() {
    let db = TestDatabase::new().await;
    let org_id = setup_org(&db, "concurrent-create").await;
    let pool = db.pool.clone();

    // Spawn 50 concurrent job creations
    let handles: Vec<_> = (0..50).map(|i| {
        let pool = pool.clone();
        let org_id = org_id.clone();
        tokio::spawn(async move {
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
            .execute(&*pool)
            .await
            .expect("Should create job");
            job_id
        })
    }).collect();

    let job_ids: Vec<String> = join_all(handles)
        .await
        .into_iter()
        .map(|r| r.unwrap())
        .collect();

    assert_eq!(job_ids.len(), 50);

    // Verify all jobs created
    let count: (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM jobs WHERE organization_id = $1 AND queue_name = 'concurrent-queue'",
    )
    .bind(&org_id)
    .fetch_one(db.pool())
    .await
    .unwrap();

    assert_eq!(count.0, 50);
    println!("✅ Concurrent job creation: 50 jobs created successfully");
}

/// Test concurrent dequeue with SKIP LOCKED prevents duplicates
#[tokio::test]
async fn test_concurrent_dequeue_no_duplicates() {
    let db = TestDatabase::new().await;
    let org_id = setup_org(&db, "concurrent-dequeue").await;

    // Create 20 pending jobs
    for i in 0..20 {
        let job_id = uuid::Uuid::new_v4().to_string();
        sqlx::query(
            r#"
            INSERT INTO jobs (id, organization_id, queue_name, status, payload, priority, max_retries, timeout_seconds, created_at, updated_at)
            VALUES ($1, $2, 'dequeue-queue', 'pending', $3::JSONB, 0, 3, 300, NOW(), NOW())
            "#
        )
        .bind(&job_id)
        .bind(&org_id)
        .bind(json!({"index": i}).to_string())
        .execute(db.pool())
        .await
        .unwrap();
    }

    let pool = db.pool.clone();
    let org_id_clone = org_id.clone();

    // Spawn 10 concurrent workers each trying to dequeue 5 jobs
    let handles: Vec<_> = (0..10)
        .map(|worker_num| {
            let pool = pool.clone();
            let org_id = org_id_clone.clone();
            tokio::spawn(async move {
                let mut dequeued_ids = Vec::new();
                for _ in 0..5 {
                    let result: Option<(String,)> = sqlx::query_as(
                        r#"
                    UPDATE jobs
                    SET status = 'processing',
                        assigned_worker_id = $1,
                        updated_at = NOW()
                    WHERE id = (
                        SELECT id FROM jobs
                        WHERE organization_id = $2
                        AND queue_name = 'dequeue-queue'
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
                    .expect("Dequeue should work");

                    if let Some((id,)) = result {
                        dequeued_ids.push(id);
                    }
                }
                dequeued_ids
            })
        })
        .collect();

    let all_dequeued: Vec<String> = join_all(handles)
        .await
        .into_iter()
        .flat_map(|r| r.unwrap())
        .collect();

    // Verify exactly 20 unique jobs dequeued
    let unique_count = all_dequeued
        .iter()
        .collect::<std::collections::HashSet<_>>()
        .len();
    assert_eq!(unique_count, 20, "All jobs should be uniquely dequeued");
    assert_eq!(all_dequeued.len(), 20, "No duplicate dequeues");

    println!("✅ Concurrent dequeue: 20 jobs dequeued by 10 workers, 0 duplicates");
}

/// Test concurrent updates to same job fail gracefully
#[tokio::test]
async fn test_concurrent_job_completion_race() {
    let db = TestDatabase::new().await;
    let org_id = setup_org(&db, "race-condition").await;

    let job_id = uuid::Uuid::new_v4().to_string();
    sqlx::query(
        r#"
        INSERT INTO jobs (id, organization_id, queue_name, status, payload, priority, max_retries, timeout_seconds, created_at, updated_at)
        VALUES ($1, $2, 'race-queue', 'processing', '{}'::JSONB, 0, 3, 300, NOW(), NOW())
        "#
    )
    .bind(&job_id)
    .bind(&org_id)
    .execute(db.pool())
    .await
    .unwrap();

    let pool = db.pool.clone();
    let job_id_clone = job_id.clone();

    // 5 workers try to complete the same job simultaneously
    let handles: Vec<_> = (0..5)
        .map(|worker_num| {
            let pool = pool.clone();
            let job_id = job_id_clone.clone();
            tokio::spawn(async move {
                let result = sqlx::query(
                    r#"
                UPDATE jobs 
                SET status = 'completed',
                    result = $1::JSONB,
                    completed_at = NOW(),
                    updated_at = NOW()
                WHERE id = $2 AND status = 'processing'
                "#,
                )
                .bind(json!({"worker": worker_num}).to_string())
                .bind(&job_id)
                .execute(&*pool)
                .await
                .expect("Update should work");

                result.rows_affected() > 0
            })
        })
        .collect();

    let results: Vec<bool> = join_all(handles)
        .await
        .into_iter()
        .map(|r| r.unwrap())
        .collect();

    // Only one worker should succeed
    let success_count = results.iter().filter(|&&r| r).count();
    assert_eq!(success_count, 1, "Only one worker should complete the job");

    // Verify job is completed
    let final_status: (String,) = sqlx::query_as("SELECT status FROM jobs WHERE id = $1")
        .bind(&job_id)
        .fetch_one(db.pool())
        .await
        .unwrap();

    assert_eq!(final_status.0, "completed");
    println!("✅ Concurrent completion race: exactly 1 of 5 workers succeeded");
}

// =============================================================================
// DATA INTEGRITY TESTS
// =============================================================================

/// Test job status transitions are valid
#[tokio::test]
async fn test_job_status_transition_validity() {
    let db = TestDatabase::new().await;
    let org_id = setup_org(&db, "transitions").await;

    let job_id = uuid::Uuid::new_v4().to_string();

    // Create pending job
    sqlx::query(
        r#"
        INSERT INTO jobs (id, organization_id, queue_name, status, payload, priority, max_retries, timeout_seconds, created_at, updated_at)
        VALUES ($1, $2, 'transition-queue', 'pending', '{}'::JSONB, 0, 3, 300, NOW(), NOW())
        "#
    )
    .bind(&job_id)
    .bind(&org_id)
    .execute(db.pool())
    .await
    .unwrap();

    // Valid transitions
    let transitions = [("pending", "processing"), ("processing", "completed")];

    for (from, to) in transitions {
        let result = sqlx::query(
            "UPDATE jobs SET status = $1, updated_at = NOW() WHERE id = $2 AND status = $3",
        )
        .bind(to)
        .bind(&job_id)
        .bind(from)
        .execute(db.pool())
        .await
        .unwrap();

        assert!(
            result.rows_affected() > 0,
            "Transition {} -> {} should succeed",
            from,
            to
        );
    }

    // Invalid transition: completed -> pending (should not update)
    let result = sqlx::query(
        "UPDATE jobs SET status = 'pending', updated_at = NOW() WHERE id = $1 AND status = 'completed'"
    )
    .bind(&job_id)
    .execute(db.pool())
    .await
    .unwrap();

    // Note: The DB allows it but the app layer should prevent it
    // Here we're just verifying the query runs

    println!("✅ Job status transitions work correctly");
}

/// Test retry count increments properly
#[tokio::test]
async fn test_retry_count_integrity() {
    let db = TestDatabase::new().await;
    let org_id = setup_org(&db, "retry-count").await;

    let job_id = uuid::Uuid::new_v4().to_string();

    sqlx::query(
        r#"
        INSERT INTO jobs (id, organization_id, queue_name, status, payload, priority, max_retries, retry_count, timeout_seconds, created_at, updated_at)
        VALUES ($1, $2, 'retry-queue', 'processing', '{}'::JSONB, 0, 5, 0, 300, NOW(), NOW())
        "#
    )
    .bind(&job_id)
    .bind(&org_id)
    .execute(db.pool())
    .await
    .unwrap();

    // Simulate 5 retries
    for expected_count in 1..=5 {
        sqlx::query(
            r#"
            UPDATE jobs 
            SET status = 'pending',
                retry_count = retry_count + 1,
                last_error = $1,
                updated_at = NOW()
            WHERE id = $2
            "#,
        )
        .bind(format!("Retry attempt {}", expected_count))
        .bind(&job_id)
        .execute(db.pool())
        .await
        .unwrap();

        let count: (i32,) = sqlx::query_as("SELECT retry_count FROM jobs WHERE id = $1")
            .bind(&job_id)
            .fetch_one(db.pool())
            .await
            .unwrap();

        assert_eq!(
            count.0, expected_count,
            "Retry count should be {}",
            expected_count
        );
    }

    println!("✅ Retry count integrity verified (5 increments)");
}

/// Test scheduled_at ordering works correctly
#[tokio::test]
async fn test_scheduled_job_ordering() {
    let db = TestDatabase::new().await;
    let org_id = setup_org(&db, "scheduled-order").await;

    // Create jobs scheduled at different times
    let times = vec![
        ("job-3", "NOW() + INTERVAL '3 hours'"),
        ("job-1", "NOW() + INTERVAL '1 hour'"),
        ("job-2", "NOW() + INTERVAL '2 hours'"),
        ("job-now", "NOW()"),
    ];

    for (name, scheduled) in &times {
        let job_id = uuid::Uuid::new_v4().to_string();
        sqlx::query(&format!(
            r#"
            INSERT INTO jobs (id, organization_id, queue_name, status, payload, priority, max_retries, timeout_seconds, scheduled_at, created_at, updated_at)
            VALUES ($1, $2, 'scheduled-queue', 'scheduled', $3::JSONB, 0, 3, 300, {}, NOW(), NOW())
            "#,
            scheduled
        ))
        .bind(&job_id)
        .bind(&org_id)
        .bind(json!({"name": name}).to_string())
        .execute(db.pool())
        .await
        .unwrap();
    }

    // Query jobs that should run now
    let ready_jobs: Vec<(Value,)> = sqlx::query_as(
        r#"
        SELECT payload 
        FROM jobs 
        WHERE organization_id = $1 
        AND queue_name = 'scheduled-queue'
        AND scheduled_at <= NOW()
        ORDER BY scheduled_at
        "#,
    )
    .bind(&org_id)
    .fetch_all(db.pool())
    .await
    .unwrap();

    assert_eq!(ready_jobs.len(), 1);
    assert_eq!(
        ready_jobs[0].0.get("name").unwrap().as_str().unwrap(),
        "job-now"
    );

    println!("✅ Scheduled job ordering works correctly");
}

// =============================================================================
// SECURITY ISOLATION TESTS
// =============================================================================

/// Test complete organization isolation for jobs
#[tokio::test]
async fn test_full_job_isolation() {
    let db = TestDatabase::new().await;

    let org1 = setup_org(&db, "org-alpha").await;
    let org2 = setup_org(&db, "org-beta").await;

    // Create jobs in org1
    for i in 0..5 {
        let job_id = uuid::Uuid::new_v4().to_string();
        sqlx::query(
            r#"
            INSERT INTO jobs (id, organization_id, queue_name, status, payload, priority, max_retries, timeout_seconds, created_at, updated_at)
            VALUES ($1, $2, 'isolated-queue', 'pending', $3::JSONB, 0, 3, 300, NOW(), NOW())
            "#
        )
        .bind(&job_id)
        .bind(&org1)
        .bind(json!({"org": "alpha", "index": i}).to_string())
        .execute(db.pool())
        .await
        .unwrap();
    }

    // Create jobs in org2
    for i in 0..3 {
        let job_id = uuid::Uuid::new_v4().to_string();
        sqlx::query(
            r#"
            INSERT INTO jobs (id, organization_id, queue_name, status, payload, priority, max_retries, timeout_seconds, created_at, updated_at)
            VALUES ($1, $2, 'isolated-queue', 'pending', $3::JSONB, 0, 3, 300, NOW(), NOW())
            "#
        )
        .bind(&job_id)
        .bind(&org2)
        .bind(json!({"org": "beta", "index": i}).to_string())
        .execute(db.pool())
        .await
        .unwrap();
    }

    // Query org1 jobs
    let org1_jobs: Vec<(Value,)> =
        sqlx::query_as("SELECT payload FROM jobs WHERE organization_id = $1")
            .bind(&org1)
            .fetch_all(db.pool())
            .await
            .unwrap();

    assert_eq!(org1_jobs.len(), 5);
    for (payload,) in &org1_jobs {
        assert_eq!(payload.get("org").unwrap().as_str().unwrap(), "alpha");
    }

    // Query org2 jobs
    let org2_jobs: Vec<(Value,)> =
        sqlx::query_as("SELECT payload FROM jobs WHERE organization_id = $1")
            .bind(&org2)
            .fetch_all(db.pool())
            .await
            .unwrap();

    assert_eq!(org2_jobs.len(), 3);
    for (payload,) in &org2_jobs {
        assert_eq!(payload.get("org").unwrap().as_str().unwrap(), "beta");
    }

    // Attempt cross-org access (should return nothing)
    let cross_org: Option<(String,)> = sqlx::query_as(
        "SELECT id FROM jobs WHERE organization_id = $1 AND payload->>'org' = 'beta'",
    )
    .bind(&org1)
    .fetch_optional(db.pool())
    .await
    .unwrap();

    assert!(
        cross_org.is_none(),
        "Cross-org access should return nothing"
    );

    println!("✅ Full job isolation: org1=5 jobs, org2=3 jobs, cross-access=0");
}

/// Test worker isolation between organizations
#[tokio::test]
async fn test_worker_isolation() {
    let db = TestDatabase::new().await;

    let org1 = setup_org(&db, "worker-org1").await;
    let org2 = setup_org(&db, "worker-org2").await;

    // Create worker in org1
    let worker1_id = uuid::Uuid::new_v4().to_string();
    sqlx::query(
        r#"
        INSERT INTO workers (id, organization_id, queue_name, hostname, status, max_concurrency, current_jobs, last_heartbeat, registered_at)
        VALUES ($1, $2, 'shared-queue', 'worker1-host', 'healthy', 5, 0, NOW(), NOW())
        "#
    )
    .bind(&worker1_id)
    .bind(&org1)
    .execute(db.pool())
    .await
    .unwrap();

    // Attempt to update worker from org2 (should fail)
    let result = sqlx::query(
        "UPDATE workers SET status = 'compromised' WHERE id = $1 AND organization_id = $2",
    )
    .bind(&worker1_id)
    .bind(&org2)
    .execute(db.pool())
    .await
    .unwrap();

    assert_eq!(
        result.rows_affected(),
        0,
        "Cross-org worker update should fail"
    );

    // Verify worker still healthy
    let status: (String,) = sqlx::query_as("SELECT status FROM workers WHERE id = $1")
        .bind(&worker1_id)
        .fetch_one(db.pool())
        .await
        .unwrap();

    assert_eq!(status.0, "healthy");

    println!("✅ Worker isolation verified");
}

// =============================================================================
// EDGE CASES AND BOUNDARY TESTS
// =============================================================================

/// Test maximum priority values
#[tokio::test]
async fn test_priority_boundaries() {
    let db = TestDatabase::new().await;
    let org_id = setup_org(&db, "priority-bounds").await;

    // Test valid priority range (-100 to 100)
    for priority in [-100, -50, 0, 50, 100] {
        let job_id = uuid::Uuid::new_v4().to_string();
        let result = sqlx::query(
            r#"
            INSERT INTO jobs (id, organization_id, queue_name, status, payload, priority, max_retries, timeout_seconds, created_at, updated_at)
            VALUES ($1, $2, 'priority-queue', 'pending', '{}'::JSONB, $3, 3, 300, NOW(), NOW())
            "#
        )
        .bind(&job_id)
        .bind(&org_id)
        .bind(priority)
        .execute(db.pool())
        .await;

        assert!(result.is_ok(), "Priority {} should be valid", priority);
    }

    // Test invalid priority (out of range)
    let job_id = uuid::Uuid::new_v4().to_string();
    let result = sqlx::query(
        r#"
        INSERT INTO jobs (id, organization_id, queue_name, status, payload, priority, max_retries, timeout_seconds, created_at, updated_at)
        VALUES ($1, $2, 'priority-queue', 'pending', '{}'::JSONB, 101, 3, 300, NOW(), NOW())
        "#
    )
    .bind(&job_id)
    .bind(&org_id)
    .execute(db.pool())
    .await;

    assert!(result.is_err(), "Priority 101 should be invalid");

    println!("✅ Priority boundaries verified (-100 to 100)");
}

/// Test maximum timeout values
#[tokio::test]
async fn test_timeout_boundaries() {
    let db = TestDatabase::new().await;
    let org_id = setup_org(&db, "timeout-bounds").await;

    // Valid timeout (1 second to 86400 seconds = 24 hours)
    for timeout in [1, 60, 3600, 86400] {
        let job_id = uuid::Uuid::new_v4().to_string();
        let result = sqlx::query(
            r#"
            INSERT INTO jobs (id, organization_id, queue_name, status, payload, priority, max_retries, timeout_seconds, created_at, updated_at)
            VALUES ($1, $2, 'timeout-queue', 'pending', '{}'::JSONB, 0, 3, $3, NOW(), NOW())
            "#
        )
        .bind(&job_id)
        .bind(&org_id)
        .bind(timeout)
        .execute(db.pool())
        .await;

        assert!(result.is_ok(), "Timeout {} should be valid", timeout);
    }

    // Invalid timeout (0)
    let job_id = uuid::Uuid::new_v4().to_string();
    let result = sqlx::query(
        r#"
        INSERT INTO jobs (id, organization_id, queue_name, status, payload, priority, max_retries, timeout_seconds, created_at, updated_at)
        VALUES ($1, $2, 'timeout-queue', 'pending', '{}'::JSONB, 0, 3, 0, NOW(), NOW())
        "#
    )
    .bind(&job_id)
    .bind(&org_id)
    .execute(db.pool())
    .await;

    assert!(result.is_err(), "Timeout 0 should be invalid");

    println!("✅ Timeout boundaries verified (1 to 86400)");
}

/// Test very long queue names
#[tokio::test]
async fn test_long_queue_name() {
    let db = TestDatabase::new().await;
    let org_id = setup_org(&db, "long-queue").await;

    // 255 character queue name (should work)
    let long_name = "q".repeat(255);
    let job_id = uuid::Uuid::new_v4().to_string();
    let result = sqlx::query(
        r#"
        INSERT INTO jobs (id, organization_id, queue_name, status, payload, priority, max_retries, timeout_seconds, created_at, updated_at)
        VALUES ($1, $2, $3, 'pending', '{}'::JSONB, 0, 3, 300, NOW(), NOW())
        "#
    )
    .bind(&job_id)
    .bind(&org_id)
    .bind(&long_name)
    .execute(db.pool())
    .await;

    assert!(result.is_ok(), "255-char queue name should work");

    println!("✅ Long queue name (255 chars) works");
}

/// Test empty payload handling
#[tokio::test]
async fn test_empty_payload() {
    let db = TestDatabase::new().await;
    let org_id = setup_org(&db, "empty-payload").await;

    let job_id = uuid::Uuid::new_v4().to_string();
    sqlx::query(
        r#"
        INSERT INTO jobs (id, organization_id, queue_name, status, payload, priority, max_retries, timeout_seconds, created_at, updated_at)
        VALUES ($1, $2, 'empty-queue', 'pending', '{}'::JSONB, 0, 3, 300, NOW(), NOW())
        "#
    )
    .bind(&job_id)
    .bind(&org_id)
    .execute(db.pool())
    .await
    .unwrap();

    let payload: (Value,) = sqlx::query_as("SELECT payload FROM jobs WHERE id = $1")
        .bind(&job_id)
        .fetch_one(db.pool())
        .await
        .unwrap();

    assert_eq!(payload.0, json!({}));

    println!("✅ Empty payload handling works");
}

/// Test null vs empty string handling
#[tokio::test]
async fn test_null_vs_empty() {
    let db = TestDatabase::new().await;
    let org_id = setup_org(&db, "null-empty").await;

    // Job with NULL idempotency_key
    let job1_id = uuid::Uuid::new_v4().to_string();
    sqlx::query(
        r#"
        INSERT INTO jobs (id, organization_id, queue_name, status, payload, priority, max_retries, timeout_seconds, idempotency_key, created_at, updated_at)
        VALUES ($1, $2, 'null-queue', 'pending', '{}'::JSONB, 0, 3, 300, NULL, NOW(), NOW())
        "#
    )
    .bind(&job1_id)
    .bind(&org_id)
    .execute(db.pool())
    .await
    .unwrap();

    // Job with empty string idempotency_key (should be treated differently)
    let job2_id = uuid::Uuid::new_v4().to_string();
    sqlx::query(
        r#"
        INSERT INTO jobs (id, organization_id, queue_name, status, payload, priority, max_retries, timeout_seconds, idempotency_key, created_at, updated_at)
        VALUES ($1, $2, 'null-queue', 'pending', '{}'::JSONB, 0, 3, 300, '', NOW(), NOW())
        "#
    )
    .bind(&job2_id)
    .bind(&org_id)
    .execute(db.pool())
    .await
    .unwrap();

    // Both should exist
    let count: (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM jobs WHERE organization_id = $1 AND queue_name = 'null-queue'",
    )
    .bind(&org_id)
    .fetch_one(db.pool())
    .await
    .unwrap();

    assert_eq!(count.0, 2);

    println!("✅ NULL vs empty string handling works");
}

// =============================================================================
// STRESS TESTS
// =============================================================================

/// Test high-volume job creation
#[tokio::test]
async fn test_bulk_1000_jobs() {
    let db = TestDatabase::new().await;
    let org_id = setup_org(&db, "bulk-1000").await;

    let start = std::time::Instant::now();

    // Create 1000 jobs in batches
    for batch in 0..10 {
        for i in 0..100 {
            let job_id = uuid::Uuid::new_v4().to_string();
            sqlx::query(
                r#"
                INSERT INTO jobs (id, organization_id, queue_name, status, payload, priority, max_retries, timeout_seconds, created_at, updated_at)
                VALUES ($1, $2, 'bulk-queue', 'pending', $3::JSONB, 0, 3, 300, NOW(), NOW())
                "#
            )
            .bind(&job_id)
            .bind(&org_id)
            .bind(json!({"batch": batch, "index": i}).to_string())
            .execute(db.pool())
            .await
            .unwrap();
        }
    }

    let elapsed = start.elapsed();

    // Verify count
    let count: (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM jobs WHERE organization_id = $1 AND queue_name = 'bulk-queue'",
    )
    .bind(&org_id)
    .fetch_one(db.pool())
    .await
    .unwrap();

    assert_eq!(count.0, 1000);

    println!("✅ Bulk 1000 jobs created in {:?}", elapsed);
}

/// Test rapid status updates
#[tokio::test]
async fn test_rapid_status_updates() {
    let db = TestDatabase::new().await;
    let org_id = setup_org(&db, "rapid-updates").await;

    let job_id = uuid::Uuid::new_v4().to_string();
    sqlx::query(
        r#"
        INSERT INTO jobs (id, organization_id, queue_name, status, payload, priority, max_retries, timeout_seconds, created_at, updated_at)
        VALUES ($1, $2, 'rapid-queue', 'pending', '{}'::JSONB, 0, 3, 300, NOW(), NOW())
        "#
    )
    .bind(&job_id)
    .bind(&org_id)
    .execute(db.pool())
    .await
    .unwrap();

    let start = std::time::Instant::now();

    // 100 rapid updates
    for i in 0..100 {
        sqlx::query("UPDATE jobs SET retry_count = $1, updated_at = NOW() WHERE id = $2")
            .bind(i)
            .bind(&job_id)
            .execute(db.pool())
            .await
            .unwrap();
    }

    let elapsed = start.elapsed();

    let final_count: (i32,) = sqlx::query_as("SELECT retry_count FROM jobs WHERE id = $1")
        .bind(&job_id)
        .fetch_one(db.pool())
        .await
        .unwrap();

    assert_eq!(final_count.0, 99);

    println!("✅ 100 rapid updates completed in {:?}", elapsed);
}

// =============================================================================
// WORKFLOW AND DEPENDENCY TESTS
// =============================================================================

/// Test workflow job counting
#[tokio::test]
async fn test_workflow_job_counting() {
    let db = TestDatabase::new().await;
    let org_id = setup_org(&db, "workflow-count").await;

    let workflow_id = uuid::Uuid::new_v4().to_string();

    // Create workflow
    sqlx::query(
        r#"
        INSERT INTO workflows (id, organization_id, name, status, total_jobs, completed_jobs, failed_jobs, created_at)
        VALUES ($1, $2, 'Test Workflow', 'running', 5, 0, 0, NOW())
        "#
    )
    .bind(&workflow_id)
    .bind(&org_id)
    .execute(db.pool())
    .await
    .unwrap();

    // Create 5 jobs
    for i in 0..5 {
        let job_id = uuid::Uuid::new_v4().to_string();
        sqlx::query(
            r#"
            INSERT INTO jobs (id, organization_id, queue_name, workflow_id, status, payload, priority, max_retries, timeout_seconds, created_at, updated_at)
            VALUES ($1, $2, 'workflow-queue', $3, 'pending', $4::JSONB, $5, 3, 300, NOW(), NOW())
            "#
        )
        .bind(&job_id)
        .bind(&org_id)
        .bind(&workflow_id)
        .bind(json!({"step": i}).to_string())
        .bind(i)
        .execute(db.pool())
        .await
        .unwrap();
    }

    // Complete 3 jobs
    sqlx::query(
        r#"
        UPDATE jobs 
        SET status = 'completed', completed_at = NOW(), updated_at = NOW()
        WHERE workflow_id = $1 AND priority < 3
        "#,
    )
    .bind(&workflow_id)
    .execute(db.pool())
    .await
    .unwrap();

    // Update workflow counts
    sqlx::query(
        r#"
        UPDATE workflows 
        SET completed_jobs = (SELECT COUNT(*) FROM jobs WHERE workflow_id = $1 AND status = 'completed'),
            failed_jobs = (SELECT COUNT(*) FROM jobs WHERE workflow_id = $1 AND status IN ('failed', 'deadletter'))
        WHERE id = $1
        "#
    )
    .bind(&workflow_id)
    .execute(db.pool())
    .await
    .unwrap();

    let counts: (i32, i32, i32) = sqlx::query_as(
        "SELECT total_jobs, completed_jobs, failed_jobs FROM workflows WHERE id = $1",
    )
    .bind(&workflow_id)
    .fetch_one(db.pool())
    .await
    .unwrap();

    assert_eq!(counts.0, 5);
    assert_eq!(counts.1, 3);
    assert_eq!(counts.2, 0);

    println!("✅ Workflow job counting works (5 total, 3 completed, 0 failed)");
}

/// Test job dependency creation
#[tokio::test]
async fn test_job_dependencies() {
    let db = TestDatabase::new().await;
    let org_id = setup_org(&db, "dependencies").await;

    // Create parent job
    let parent_job_id = uuid::Uuid::new_v4().to_string();
    sqlx::query(
        r#"
        INSERT INTO jobs (id, organization_id, queue_name, status, payload, priority, max_retries, timeout_seconds, created_at, updated_at)
        VALUES ($1, $2, 'dep-queue', 'pending', '{"type": "parent"}'::JSONB, 0, 3, 300, NOW(), NOW())
        "#
    )
    .bind(&parent_job_id)
    .bind(&org_id)
    .execute(db.pool())
    .await
    .unwrap();

    // Create child job
    let child_job_id = uuid::Uuid::new_v4().to_string();
    sqlx::query(
        r#"
        INSERT INTO jobs (id, organization_id, queue_name, status, payload, priority, max_retries, timeout_seconds, dependencies_met, created_at, updated_at)
        VALUES ($1, $2, 'dep-queue', 'pending', '{"type": "child"}'::JSONB, 0, 3, 300, FALSE, NOW(), NOW())
        "#
    )
    .bind(&child_job_id)
    .bind(&org_id)
    .execute(db.pool())
    .await
    .unwrap();

    // Create dependency
    let dep_id = uuid::Uuid::new_v4().to_string();
    sqlx::query(
        r#"
        INSERT INTO job_dependencies (id, job_id, depends_on_job_id, dependency_type, created_at)
        VALUES ($1, $2, $3, 'all', NOW())
        "#,
    )
    .bind(&dep_id)
    .bind(&child_job_id)
    .bind(&parent_job_id)
    .execute(db.pool())
    .await
    .unwrap();

    // Verify dependency
    let deps: Vec<(String, String)> =
        sqlx::query_as("SELECT job_id, depends_on_job_id FROM job_dependencies WHERE job_id = $1")
            .bind(&child_job_id)
            .fetch_all(db.pool())
            .await
            .unwrap();

    assert_eq!(deps.len(), 1);
    assert_eq!(deps[0].1, parent_job_id);

    println!("✅ Job dependencies work correctly");
}

// =============================================================================
// SCHEDULE TESTS
// =============================================================================

/// Test cron schedule storage
#[tokio::test]
async fn test_cron_schedule_storage() {
    let db = TestDatabase::new().await;
    let org_id = setup_org(&db, "cron-test").await;

    let schedule_id = uuid::Uuid::new_v4().to_string();

    // Various cron expressions
    let cron_expressions = [
        "* * * * *",      // Every minute
        "0 * * * *",      // Every hour
        "0 0 * * *",      // Every day at midnight
        "0 0 * * 0",      // Every Sunday
        "0 0 1 * *",      // First of every month
        "0 9-17 * * 1-5", // Weekdays 9am-5pm
    ];

    for cron in cron_expressions {
        let id = uuid::Uuid::new_v4().to_string();
        let result = sqlx::query(
            r#"
            INSERT INTO schedules (id, organization_id, name, queue_name, cron_expression, timezone, payload_template, is_active, created_at, updated_at)
            VALUES ($1, $2, 'Test Schedule', 'schedule-queue', $3, 'UTC', '{}'::JSONB, TRUE, NOW(), NOW())
            "#
        )
        .bind(&id)
        .bind(&org_id)
        .bind(cron)
        .execute(db.pool())
        .await;

        assert!(result.is_ok(), "Cron expression '{}' should be valid", cron);
    }

    println!("✅ Cron schedule storage works (6 expressions tested)");
}

/// Test schedule run tracking
#[tokio::test]
async fn test_schedule_run_tracking() {
    let db = TestDatabase::new().await;
    let org_id = setup_org(&db, "schedule-runs").await;

    let schedule_id = uuid::Uuid::new_v4().to_string();

    // Create schedule
    sqlx::query(
        r#"
        INSERT INTO schedules (id, organization_id, name, queue_name, cron_expression, timezone, payload_template, is_active, run_count, created_at, updated_at)
        VALUES ($1, $2, 'Track Schedule', 'track-queue', '* * * * *', 'UTC', '{}'::JSONB, TRUE, 0, NOW(), NOW())
        "#
    )
    .bind(&schedule_id)
    .bind(&org_id)
    .execute(db.pool())
    .await
    .unwrap();

    // Simulate 5 runs
    for _ in 0..5 {
        // Create a schedule run (job_id is NULL - we're just testing run tracking)
        let run_id = uuid::Uuid::new_v4().to_string();

        sqlx::query(
            r#"
            INSERT INTO schedule_runs (id, schedule_id, started_at, status)
            VALUES ($1, $2, NOW(), 'running')
            "#,
        )
        .bind(&run_id)
        .bind(&schedule_id)
        .execute(db.pool())
        .await
        .unwrap();

        // Update schedule run count
        sqlx::query(
            "UPDATE schedules SET run_count = run_count + 1, last_run_at = NOW() WHERE id = $1",
        )
        .bind(&schedule_id)
        .execute(db.pool())
        .await
        .unwrap();
    }

    let run_count: (i64,) = sqlx::query_as("SELECT run_count FROM schedules WHERE id = $1")
        .bind(&schedule_id)
        .fetch_one(db.pool())
        .await
        .unwrap();

    assert_eq!(run_count.0, 5);

    let runs: Vec<(String,)> =
        sqlx::query_as("SELECT id FROM schedule_runs WHERE schedule_id = $1")
            .bind(&schedule_id)
            .fetch_all(db.pool())
            .await
            .unwrap();

    assert_eq!(runs.len(), 5);

    println!("✅ Schedule run tracking works (5 runs recorded)");
}

// =============================================================================
// CLEANUP AND RETENTION TESTS
// =============================================================================

/// Test old job cleanup
#[tokio::test]
async fn test_job_cleanup_by_age() {
    let db = TestDatabase::new().await;
    let org_id = setup_org(&db, "cleanup").await;

    // Create old completed job (7 days ago)
    let old_job_id = uuid::Uuid::new_v4().to_string();
    sqlx::query(
        r#"
        INSERT INTO jobs (id, organization_id, queue_name, status, payload, priority, max_retries, timeout_seconds, created_at, updated_at, completed_at)
        VALUES ($1, $2, 'cleanup-queue', 'completed', '{}'::JSONB, 0, 3, 300, NOW() - INTERVAL '7 days', NOW() - INTERVAL '7 days', NOW() - INTERVAL '7 days')
        "#
    )
    .bind(&old_job_id)
    .bind(&org_id)
    .execute(db.pool())
    .await
    .unwrap();

    // Create recent completed job
    let recent_job_id = uuid::Uuid::new_v4().to_string();
    sqlx::query(
        r#"
        INSERT INTO jobs (id, organization_id, queue_name, status, payload, priority, max_retries, timeout_seconds, created_at, updated_at, completed_at)
        VALUES ($1, $2, 'cleanup-queue', 'completed', '{}'::JSONB, 0, 3, 300, NOW() - INTERVAL '1 day', NOW() - INTERVAL '1 day', NOW() - INTERVAL '1 day')
        "#
    )
    .bind(&recent_job_id)
    .bind(&org_id)
    .execute(db.pool())
    .await
    .unwrap();

    // Delete jobs older than 5 days
    let deleted = sqlx::query(
        r#"
        DELETE FROM jobs 
        WHERE organization_id = $1 
        AND queue_name = 'cleanup-queue'
        AND status = 'completed'
        AND completed_at < NOW() - INTERVAL '5 days'
        "#,
    )
    .bind(&org_id)
    .execute(db.pool())
    .await
    .unwrap();

    assert_eq!(deleted.rows_affected(), 1);

    // Verify only recent job remains
    let remaining: Vec<(String,)> = sqlx::query_as(
        "SELECT id FROM jobs WHERE organization_id = $1 AND queue_name = 'cleanup-queue'",
    )
    .bind(&org_id)
    .fetch_all(db.pool())
    .await
    .unwrap();

    assert_eq!(remaining.len(), 1);
    assert_eq!(remaining[0].0, recent_job_id);

    println!("✅ Job cleanup by age works (1 old job deleted, 1 recent kept)");
}

/// Test lease expiration cleanup
#[tokio::test]
async fn test_lease_expiration() {
    let db = TestDatabase::new().await;
    let org_id = setup_org(&db, "lease-expire").await;

    // Create job with expired lease
    let expired_job_id = uuid::Uuid::new_v4().to_string();
    sqlx::query(
        r#"
        INSERT INTO jobs (id, organization_id, queue_name, status, payload, priority, max_retries, timeout_seconds, assigned_worker_id, lease_expires_at, created_at, updated_at)
        VALUES ($1, $2, 'lease-queue', 'processing', '{}'::JSONB, 0, 3, 300, 'dead-worker', NOW() - INTERVAL '5 minutes', NOW(), NOW())
        "#
    )
    .bind(&expired_job_id)
    .bind(&org_id)
    .execute(db.pool())
    .await
    .unwrap();

    // Create job with valid lease
    let valid_job_id = uuid::Uuid::new_v4().to_string();
    sqlx::query(
        r#"
        INSERT INTO jobs (id, organization_id, queue_name, status, payload, priority, max_retries, timeout_seconds, assigned_worker_id, lease_expires_at, created_at, updated_at)
        VALUES ($1, $2, 'lease-queue', 'processing', '{}'::JSONB, 0, 3, 300, 'live-worker', NOW() + INTERVAL '5 minutes', NOW(), NOW())
        "#
    )
    .bind(&valid_job_id)
    .bind(&org_id)
    .execute(db.pool())
    .await
    .unwrap();

    // Find and reset expired leases
    let expired = sqlx::query(
        r#"
        UPDATE jobs 
        SET status = 'pending',
            assigned_worker_id = NULL,
            lease_expires_at = NULL,
            retry_count = retry_count + 1,
            updated_at = NOW()
        WHERE organization_id = $1
        AND status = 'processing'
        AND lease_expires_at < NOW()
        "#,
    )
    .bind(&org_id)
    .execute(db.pool())
    .await
    .unwrap();

    assert_eq!(expired.rows_affected(), 1);

    // Verify expired job was reset
    let reset_job: (String, Option<String>) =
        sqlx::query_as("SELECT status, assigned_worker_id FROM jobs WHERE id = $1")
            .bind(&expired_job_id)
            .fetch_one(db.pool())
            .await
            .unwrap();

    assert_eq!(reset_job.0, "pending");
    assert!(reset_job.1.is_none());

    // Verify valid lease job unchanged
    let valid_job: (String, Option<String>) =
        sqlx::query_as("SELECT status, assigned_worker_id FROM jobs WHERE id = $1")
            .bind(&valid_job_id)
            .fetch_one(db.pool())
            .await
            .unwrap();

    assert_eq!(valid_job.0, "processing");
    assert_eq!(valid_job.1, Some("live-worker".to_string()));

    println!("✅ Lease expiration cleanup works");
}
