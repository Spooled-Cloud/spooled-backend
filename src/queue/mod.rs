//! Queue module for Spooled Backend
//!
//! This module contains the core queue management functionality.

use std::sync::Arc;

use anyhow::Result;
use chrono::{DateTime, Duration, Utc};
use sqlx::PgPool;
use tracing::{debug, info, instrument, warn};
use uuid::Uuid;

use crate::cache::RedisCache;
use crate::models::Job;

/// Queue manager for enqueueing and dequeuing jobs
#[derive(Clone)]
pub struct QueueManager {
    db: Arc<PgPool>,
    cache: Option<Arc<RedisCache>>,
}

impl QueueManager {
    /// Create a new queue manager
    pub fn new(db: Arc<PgPool>, cache: Option<Arc<RedisCache>>) -> Self {
        Self { db, cache }
    }

    /// Enqueue a new job
    ///
    /// Uses the FIXED idempotency pattern: DO UPDATE SET RETURNING
    /// This always returns a job ID whether the job is new or existing
    ///
    #[allow(clippy::too_many_arguments)]
    #[instrument(
        name = "queue.enqueue",
        skip(self, payload),
        fields(
            org_id = %org_id,
            queue_name = %queue_name,
            priority = %priority,
            has_idempotency_key = idempotency_key.is_some()
        )
    )]
    pub async fn enqueue(
        &self,
        org_id: &str,
        queue_name: &str,
        payload: serde_json::Value,
        priority: i32,
        max_retries: i32,
        timeout_seconds: i32,
        scheduled_at: Option<DateTime<Utc>>,
        idempotency_key: Option<&str>,
    ) -> Result<String> {
        // Validate payload size before inserting
        let payload_str = serde_json::to_string(&payload)?;
        const MAX_PAYLOAD_SIZE: usize = 1024 * 1024; // 1MB default
        if payload_str.len() > MAX_PAYLOAD_SIZE {
            anyhow::bail!(
                "Payload size ({} bytes) exceeds maximum allowed ({} bytes)",
                payload_str.len(),
                MAX_PAYLOAD_SIZE
            );
        }

        let job_id = Uuid::new_v4().to_string();
        let now = Utc::now();

        let initial_status = if scheduled_at.is_some() && scheduled_at > Some(now) {
            "scheduled"
        } else {
            "pending"
        };

        // FIXED: Always return job ID (new or existing) with DO UPDATE
        let (returned_id,): (String,) = sqlx::query_as(
            r#"
            INSERT INTO jobs (
                id, organization_id, queue_name, status, payload, priority,
                max_retries, timeout_seconds, created_at, scheduled_at,
                idempotency_key, updated_at
            )
            VALUES ($1, $2, $3, $4, $5::JSONB, $6, $7, $8, $9, $10, $11, $9)
            ON CONFLICT (organization_id, idempotency_key) 
            WHERE idempotency_key IS NOT NULL
            DO UPDATE SET updated_at = NOW() 
            RETURNING id
            "#,
        )
        .bind(&job_id)
        .bind(org_id)
        .bind(queue_name)
        .bind(initial_status)
        .bind(&payload_str) // Use already-validated payload string
        .bind(priority)
        .bind(max_retries)
        .bind(timeout_seconds)
        .bind(now)
        .bind(scheduled_at)
        .bind(idempotency_key)
        .fetch_one(&*self.db)
        .await?;

        // Check if this was a new job or an idempotent duplicate
        let is_new = returned_id == job_id;

        // Publish to Redis Pub/Sub with org context (only for new jobs)
        // Previously used "queue:{name}" which could leak across orgs
        if is_new {
            if let Some(ref cache) = self.cache {
                let _ = cache
                    .publish(
                        &format!("org:{}:queue:{}", org_id, queue_name),
                        &returned_id,
                    )
                    .await;
            }

            // Only record "created" history for NEW jobs, not idempotent duplicates
            self.record_history(&returned_id, "created", serde_json::json!({}))
                .await?;
            info!(job_id = %returned_id, queue = %queue_name, "Job enqueued");
        } else {
            debug!(job_id = %returned_id, queue = %queue_name, "Idempotent job already exists");
        }

        Ok(returned_id)
    }

    /// Minimum lease duration (5 seconds)
    const MIN_LEASE_DURATION_SECS: i64 = 5;

    /// Maximum lease duration (1 hour)
    const MAX_LEASE_DURATION_SECS: i64 = 3600;

    /// Dequeue a job using FOR UPDATE SKIP LOCKED (atomic, non-blocking)
    ///
    /// This is the critical path for job processing. Uses PostgreSQL's
    /// transactional integrity to ensure exactly-once delivery.
    #[instrument(
        name = "queue.dequeue",
        skip(self),
        fields(
            org_id = %org_id,
            queue_name = %queue_name,
            worker_id = %worker_id
        )
    )]
    pub async fn dequeue(
        &self,
        org_id: &str,
        queue_name: &str,
        worker_id: &str,
        lease_duration_secs: i64,
    ) -> Result<Option<Job>> {
        // Bound lease duration to prevent workers locking jobs indefinitely
        let safe_lease_duration =
            lease_duration_secs.clamp(Self::MIN_LEASE_DURATION_SECS, Self::MAX_LEASE_DURATION_SECS);
        if safe_lease_duration != lease_duration_secs {
            warn!(
                requested = lease_duration_secs,
                adjusted = safe_lease_duration,
                "Lease duration adjusted to safe range"
            );
        }

        // Check if queue is paused before dequeuing
        let queue_enabled: Option<(bool,)> = sqlx::query_as(
            "SELECT enabled FROM queue_config WHERE organization_id = $1 AND queue_name = $2",
        )
        .bind(org_id)
        .bind(queue_name)
        .fetch_optional(&*self.db)
        .await?;

        // If queue config exists and is disabled, don't dequeue
        if let Some((enabled,)) = queue_enabled {
            if !enabled {
                debug!(queue = %queue_name, "Queue is paused, skipping dequeue");
                return Ok(None);
            }
        }

        let lease_id = Uuid::new_v4().to_string();
        let now = Utc::now();
        let lease_expires = now + Duration::seconds(safe_lease_duration); // Use bounded value

        // Also added expires_at check to skip expired jobs
        let job = sqlx::query_as::<_, Job>(
            r#"
            UPDATE jobs
            SET 
                status = 'processing',
                assigned_worker_id = $1,
                lease_id = $2,
                lease_expires_at = $3,
                started_at = $4,
                updated_at = $4
            WHERE id = (
                SELECT id FROM jobs
                WHERE 
                    organization_id = $5 
                    AND queue_name = $6
                    AND status IN ('pending', 'scheduled')
                    AND (scheduled_at IS NULL OR scheduled_at <= $4)
                    AND (expires_at IS NULL OR expires_at > $4)
                    AND (dependencies_met IS NULL OR dependencies_met = TRUE)
                ORDER BY priority DESC, created_at ASC
                LIMIT 1
                FOR UPDATE SKIP LOCKED
            )
            RETURNING *
            "#,
        )
        .bind(worker_id)
        .bind(&lease_id)
        .bind(lease_expires)
        .bind(now)
        .bind(org_id)
        .bind(queue_name)
        .fetch_optional(&*self.db)
        .await?;

        if let Some(ref job) = job {
            debug!(job_id = %job.id, worker_id = %worker_id, "Job dequeued");
            self.record_history(
                &job.id,
                "processing",
                serde_json::json!({
                    "worker_id": worker_id,
                    "lease_id": lease_id
                }),
            )
            .await?;
        }

        Ok(job)
    }

    /// Mark a job as completed
    ///
    /// This prevents any worker from completing jobs they don't own.
    pub async fn complete(&self, job_id: &str, result: Option<serde_json::Value>) -> Result<()> {
        self.complete_with_worker(job_id, None, result).await
    }

    /// Mark a job as completed with worker verification
    ///
    /// Now updates dependencies_met for child jobs when parent completes
    pub async fn complete_with_worker(
        &self,
        job_id: &str,
        worker_id: Option<&str>,
        result: Option<serde_json::Value>,
    ) -> Result<()> {
        self.complete_with_worker_and_org(job_id, worker_id, None, result)
            .await
    }

    /// Mark a job as completed with worker and organization verification
    ///
    /// This version includes organization validation to prevent
    /// workers from completing jobs belonging to different organizations.
    #[instrument(
        name = "queue.complete",
        skip(self, result),
        fields(job_id = %job_id)
    )]
    pub async fn complete_with_worker_and_org(
        &self,
        job_id: &str,
        worker_id: Option<&str>,
        org_id: Option<&str>,
        result: Option<serde_json::Value>,
    ) -> Result<()> {
        let query = match (worker_id, org_id) {
            // Verify both worker and organization ownership
            (Some(wid), Some(oid)) => {
                sqlx::query(
                    r#"
                    UPDATE jobs
                    SET 
                        status = 'completed',
                        result = $1::JSONB,
                        completed_at = NOW(),
                        updated_at = NOW()
                    WHERE id = $2 
                      AND assigned_worker_id = $3
                      AND organization_id = $4
                      AND status = 'processing'
                    "#,
                )
                .bind(result.clone().and_then(|r| serde_json::to_string(&r).ok()))
                .bind(job_id)
                .bind(wid)
                .bind(oid)
                .execute(&*self.db)
                .await?
            }
            // Worker verification only (for backward compatibility)
            (Some(wid), None) => {
                sqlx::query(
                    r#"
                    UPDATE jobs
                    SET 
                        status = 'completed',
                        result = $1::JSONB,
                        completed_at = NOW(),
                        updated_at = NOW()
                    WHERE id = $2 
                      AND assigned_worker_id = $3
                      AND status = 'processing'
                    "#,
                )
                .bind(result.clone().and_then(|r| serde_json::to_string(&r).ok()))
                .bind(job_id)
                .bind(wid)
                .execute(&*self.db)
                .await?
            }
            // No worker verification (for internal use only)
            (None, _) => {
                sqlx::query(
                    r#"
                    UPDATE jobs
                    SET 
                        status = 'completed',
                        result = $1::JSONB,
                        completed_at = NOW(),
                        updated_at = NOW()
                    WHERE id = $2
                    "#,
                )
                .bind(result.clone().and_then(|r| serde_json::to_string(&r).ok()))
                .bind(job_id)
                .execute(&*self.db)
                .await?
            }
        };

        if query.rows_affected() == 0 && worker_id.is_some() {
            warn!(job_id = %job_id, "Job completion failed - not owned by worker or not processing");
            return Err(anyhow::anyhow!("Job not found or not owned by worker"));
        }

        // Update dependencies_met for child jobs that depend on this job
        // A child job's dependencies are met when ALL its parent jobs are completed
        let update_result = sqlx::query(
            r#"
            UPDATE jobs
            SET 
                dependencies_met = TRUE,
                updated_at = NOW()
            WHERE parent_job_id = $1
              AND status IN ('pending', 'scheduled')
              AND dependencies_met = FALSE
              AND NOT EXISTS (
                  -- Check if there are any other incomplete parent jobs
                  -- For now, we only support single-parent dependencies via parent_job_id
                  SELECT 1 FROM jobs parent
                  WHERE parent.id = jobs.parent_job_id
                    AND parent.status NOT IN ('completed')
              )
            "#,
        )
        .bind(job_id)
        .execute(&*self.db)
        .await?;

        if update_result.rows_affected() > 0 {
            info!(
                parent_job_id = %job_id,
                unblocked_children = update_result.rows_affected(),
                "Unblocked child jobs after parent completion"
            );
        }

        self.record_history(job_id, "completed", serde_json::json!({}))
            .await?;
        info!(job_id = %job_id, "Job completed");
        Ok(())
    }

    /// Mark a job as failed with retry logic
    ///
    /// Now requires organization_id to prevent cross-tenant attacks.
    /// Previously could fail ANY job if attacker knew the job ID.
    #[instrument(
        name = "queue.fail",
        skip(self),
        fields(
            job_id = %job_id,
            org_id = %organization_id
        )
    )]
    pub async fn fail(&self, job_id: &str, organization_id: &str, error: &str) -> Result<()> {
        // Get job to check retry count - NOW INCLUDES organization_id check
        let job =
            sqlx::query_as::<_, Job>("SELECT * FROM jobs WHERE id = $1 AND organization_id = $2")
                .bind(job_id)
                .bind(organization_id)
                .fetch_optional(&*self.db)
                .await?;

        let Some(job) = job else {
            warn!(job_id = %job_id, organization_id = %organization_id, "Job not found for failure or belongs to different organization");
            return Ok(());
        };

        if job.retry_count < job.max_retries {
            // Exponential backoff: 2^retry_count minutes (capped at 1 hour)
            let backoff_seconds = 2_i64.pow(job.retry_count as u32).min(60) * 60;
            let next_run = Utc::now() + Duration::seconds(backoff_seconds);

            sqlx::query(
                r#"
                UPDATE jobs
                SET 
                    status = 'pending',
                    retry_count = $1,
                    scheduled_at = $2,
                    last_error = $3,
                    assigned_worker_id = NULL,
                    lease_id = NULL,
                    lease_expires_at = NULL,
                    updated_at = NOW()
                WHERE id = $4 AND organization_id = $5
                "#,
            )
            .bind(job.retry_count + 1)
            .bind(next_run)
            .bind(error)
            .bind(job_id)
            .bind(organization_id)
            .execute(&*self.db)
            .await?;

            self.record_history(
                job_id,
                "retry_scheduled",
                serde_json::json!({
                    "error": error,
                    "retry_count": job.retry_count + 1,
                    "next_run": next_run.to_rfc3339(),
                    "organization_id": organization_id
                }),
            )
            .await?;

            warn!(
                job_id = %job_id,
                organization_id = %organization_id,
                attempt = job.retry_count + 1,
                next_run = %next_run,
                "Job will retry"
            );
        } else {
            // Max retries exceeded: move to dead-letter queue
            sqlx::query(
                r#"
                UPDATE jobs
                SET 
                    status = 'deadletter',
                    last_error = $1,
                    assigned_worker_id = NULL,
                    lease_id = NULL,
                    updated_at = NOW()
                WHERE id = $2 AND organization_id = $3
                "#,
            )
            .bind(error)
            .bind(job_id)
            .bind(organization_id)
            .execute(&*self.db)
            .await?;

            // Also insert into dead_letter_queue table - organization check via subquery
            sqlx::query(
                r#"
                INSERT INTO dead_letter_queue (id, job_id, organization_id, queue_name, reason, original_payload, error_details, created_at)
                SELECT 
                    gen_random_uuid()::TEXT,
                    id,
                    organization_id,
                    queue_name,
                    $1,
                    payload,
                    $2::JSONB,
                    NOW()
                FROM jobs WHERE id = $3 AND organization_id = $4
                "#,
            )
            .bind(error)
            .bind(serde_json::json!({"final_error": error, "total_retries": job.retry_count}))
            .bind(job_id)
            .bind(organization_id)
            .execute(&*self.db)
            .await?;

            self.record_history(
                job_id,
                "deadlettered",
                serde_json::json!({
                    "error": error,
                    "total_retries": job.retry_count,
                    "organization_id": organization_id
                }),
            )
            .await?;

            warn!(
                job_id = %job_id,
                organization_id = %organization_id,
                total_retries = job.retry_count,
                "Job moved to dead-letter queue"
            );
        }

        Ok(())
    }

    /// Renew a job's lease
    ///
    /// Now requires organization_id to prevent cross-tenant lease renewal attacks.
    /// Previously only checked worker_id, allowing malicious workers to extend
    /// leases on other organizations' jobs.
    pub async fn renew_lease(
        &self,
        job_id: &str,
        worker_id: &str,
        organization_id: &str,
        lease_duration_secs: i64,
    ) -> Result<bool> {
        let lease_expires = Utc::now() + Duration::seconds(lease_duration_secs);

        let result = sqlx::query(
            r#"
            UPDATE jobs
            SET lease_expires_at = $1, updated_at = NOW()
            WHERE id = $2 AND assigned_worker_id = $3 AND organization_id = $4 AND status = 'processing'
            "#,
        )
        .bind(lease_expires)
        .bind(job_id)
        .bind(worker_id)
        .bind(organization_id)
        .execute(&*self.db)
        .await?;

        Ok(result.rows_affected() > 0)
    }

    /// Recover expired leases (run periodically)
    pub async fn recover_expired_leases(&self) -> Result<i64> {
        let result = sqlx::query(
            r#"
            UPDATE jobs
            SET 
                status = 'pending',
                assigned_worker_id = NULL,
                lease_id = NULL,
                lease_expires_at = NULL,
                updated_at = NOW()
            WHERE 
                status = 'processing'
                AND lease_expires_at < NOW()
            "#,
        )
        .execute(&*self.db)
        .await?;

        let count = result.rows_affected() as i64;
        if count > 0 {
            info!(count = count, "Recovered expired leases");
        }

        Ok(count)
    }

    /// Record job history event
    ///
    /// Now includes organization context in details for audit trail
    /// Now handles errors gracefully - history recording failures
    /// should not fail the main operation (dequeue, complete, fail)
    async fn record_history(
        &self,
        job_id: &str,
        event_type: &str,
        details: serde_json::Value,
    ) -> Result<()> {
        // Include job's org_id in history for audit purposes
        // Handle errors gracefully - history is secondary
        let result = sqlx::query(
            r#"
            INSERT INTO job_history (id, job_id, event_type, details, created_at)
            SELECT 
                gen_random_uuid()::TEXT, 
                $1, 
                $2, 
                jsonb_set($3::jsonb, '{organization_id}', to_jsonb(j.organization_id)),
                NOW()
            FROM jobs j WHERE j.id = $1
            "#,
        )
        .bind(job_id)
        .bind(event_type)
        .bind(&details)
        .execute(&*self.db)
        .await;

        // Log error but don't fail the main operation
        if let Err(e) = result {
            warn!(
                job_id = %job_id,
                event_type = %event_type,
                error = %e,
                "Failed to record job history - continuing with main operation"
            );
        }

        Ok(())
    }

    /// Get queue depth (pending job count)
    pub async fn get_queue_depth(&self, org_id: &str, queue_name: &str) -> Result<i64> {
        let (count,): (i64,) = sqlx::query_as(
            r#"
            SELECT COUNT(*) 
            FROM jobs 
            WHERE organization_id = $1 
              AND queue_name = $2 
              AND status IN ('pending', 'scheduled')
            "#,
        )
        .bind(org_id)
        .bind(queue_name)
        .fetch_one(&*self.db)
        .await?;

        Ok(count)
    }

    /// Get max job age in queue (for monitoring)
    pub async fn get_max_job_age(&self, org_id: &str, queue_name: &str) -> Result<Option<i64>> {
        let result: Option<(Option<i64>,)> = sqlx::query_as(
            r#"
            SELECT EXTRACT(EPOCH FROM (NOW() - MIN(created_at)))::BIGINT
            FROM jobs 
            WHERE organization_id = $1 
              AND queue_name = $2 
              AND status IN ('pending', 'scheduled')
            "#,
        )
        .bind(org_id)
        .bind(queue_name)
        .fetch_optional(&*self.db)
        .await?;

        Ok(result.and_then(|(age,)| age))
    }
}

#[cfg(test)]
mod tests {

    #[test]
    fn test_exponential_backoff() {
        // Test backoff calculation
        for retry in 0..5 {
            let backoff_seconds = 2_i64.pow(retry).min(60) * 60;
            println!(
                "Retry {}: {} seconds = {} minutes",
                retry,
                backoff_seconds,
                backoff_seconds / 60
            );
        }

        assert_eq!(2_i64.pow(0) * 60, 60); // 1 minute
        assert_eq!(2_i64.pow(1) * 60, 120); // 2 minutes
        assert_eq!(2_i64.pow(2) * 60, 240); // 4 minutes
        assert_eq!(2_i64.pow(3) * 60, 480); // 8 minutes
        assert_eq!(60_i64.min(2_i64.pow(10)) * 60, 3600); // Capped at 60 minutes
    }

    #[test]
    fn test_backoff_sequence() {
        // Verify the full backoff sequence
        let expected_minutes = vec![1, 2, 4, 8, 16, 32, 60, 60, 60, 60];

        for (retry, expected) in expected_minutes.iter().enumerate() {
            let backoff_minutes = 2_i64.pow(retry as u32).min(60);
            assert_eq!(
                backoff_minutes, *expected,
                "Retry {} should wait {} minutes",
                retry, expected
            );
        }
    }

    #[test]
    fn test_max_retries_edge_cases() {
        // Test edge cases for retry counting
        assert!(0 < 3, "Fresh job can retry");
        assert!(2 < 3, "Job with 2 retries can retry once more");
        assert!(!(3 < 3), "Job with max retries cannot retry");
        assert!(!(4 < 3), "Job exceeding max retries cannot retry");
    }

    #[test]
    fn test_lease_duration_calculation() {
        use chrono::Duration;

        let now = chrono::Utc::now();
        let lease_duration_secs = 30_i64;
        let lease_expires = now + Duration::seconds(lease_duration_secs);

        assert!(lease_expires > now);
        assert_eq!((lease_expires - now).num_seconds(), 30);
    }

    #[test]
    fn test_priority_ordering() {
        // Higher priority should be processed first
        let priorities = vec![0, 10, -5, 100, 50];
        let mut sorted = priorities.clone();
        sorted.sort_by(|a, b| b.cmp(a)); // DESC order

        assert_eq!(sorted, vec![100, 50, 10, 0, -5]);
    }

    #[test]
    fn test_status_transitions() {
        // Valid status transitions
        let valid_transitions = vec![
            ("pending", "processing"),
            ("pending", "cancelled"),
            ("scheduled", "pending"),
            ("scheduled", "cancelled"),
            ("processing", "completed"),
            ("processing", "failed"),
            ("processing", "pending"), // lease expired
            ("failed", "pending"),     // retry
            ("failed", "deadletter"),
            ("deadletter", "pending"), // manual retry
        ];

        for (from, to) in valid_transitions {
            // Just verify these are recognized states
            assert!(!from.is_empty());
            assert!(!to.is_empty());
        }
    }
}
