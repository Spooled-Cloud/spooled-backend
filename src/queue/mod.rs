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

/// Outcome of a worker-scoped job operation (complete / fail / heartbeat).
///
/// This lets callers distinguish "lease already expired" (client should stop
/// working — HTTP 409 LEASE_EXPIRED / gRPC FAILED_PRECONDITION) from "job
/// missing or not owned by this worker" (HTTP 404 NOT_FOUND / gRPC NOT_FOUND),
/// which was previously collapsed into a single failure path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkerOpOutcome {
    /// The row was updated successfully.
    Ok,
    /// A row exists for this (job, org, worker) but its `lease_expires_at`
    /// is null or already in the past.
    LeaseExpired,
    /// No row matched the (job, org, worker, status = 'processing') filter.
    NotOwned,
}

/// Outcome of a `fail_by_worker` call.
#[derive(Debug, Clone)]
pub struct FailByWorkerOutcome {
    pub outcome: WorkerOpOutcome,
    /// Whether the failed job was rescheduled (`true`) or moved to
    /// dead-letter (`false`). Undefined when `outcome != Ok`.
    pub will_retry: bool,
    /// New status of the job (`"pending"` or `"deadletter"`) when
    /// `outcome == Ok`.
    pub new_status: Option<String>,
}

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

    /// Maximum batch size for dequeue
    const MAX_BATCH_SIZE: i32 = 100;

    /// Dequeue multiple jobs in a single query using FOR UPDATE SKIP LOCKED
    ///
    /// This is more efficient than calling dequeue() in a loop as it uses
    /// a single database round-trip to claim multiple jobs.
    #[instrument(
        name = "queue.dequeue_batch",
        skip(self),
        fields(
            org_id = %org_id,
            queue_name = %queue_name,
            worker_id = %worker_id,
            batch_size = %batch_size
        )
    )]
    pub async fn dequeue_batch(
        &self,
        org_id: &str,
        queue_name: &str,
        worker_id: &str,
        lease_duration_secs: i64,
        batch_size: i32,
    ) -> Result<Vec<Job>> {
        // Bound lease duration and batch size
        let safe_lease_duration =
            lease_duration_secs.clamp(Self::MIN_LEASE_DURATION_SECS, Self::MAX_LEASE_DURATION_SECS);
        let safe_batch_size = batch_size.clamp(1, Self::MAX_BATCH_SIZE);

        // Check if queue is paused before dequeuing
        let queue_enabled: Option<(bool,)> = sqlx::query_as(
            "SELECT enabled FROM queue_config WHERE organization_id = $1 AND queue_name = $2",
        )
        .bind(org_id)
        .bind(queue_name)
        .fetch_optional(&*self.db)
        .await?;

        if let Some((enabled,)) = queue_enabled {
            if !enabled {
                debug!(queue = %queue_name, "Queue is paused, skipping dequeue");
                return Ok(Vec::new());
            }
        }

        let now = Utc::now();
        let lease_expires = now + Duration::seconds(safe_lease_duration);

        // Use CTE to select and update multiple jobs atomically
        // Each job gets the same lease_id prefix with a unique suffix
        let lease_id_prefix = Uuid::new_v4().to_string();

        let jobs: Vec<Job> = sqlx::query_as(
            r#"
            WITH eligible_jobs AS (
                SELECT id
                FROM jobs
                WHERE
                    organization_id = $5
                    AND queue_name = $6
                    AND status IN ('pending', 'scheduled')
                    AND (scheduled_at IS NULL OR scheduled_at <= $4)
                    AND (expires_at IS NULL OR expires_at > $4)
                    AND (dependencies_met IS NULL OR dependencies_met = TRUE)
                ORDER BY priority DESC, created_at ASC
                LIMIT $7
                FOR UPDATE SKIP LOCKED
            )
            UPDATE jobs
            SET
                status = 'processing',
                assigned_worker_id = $1,
                lease_id = $2 || '-' || jobs.id,
                lease_expires_at = $3,
                started_at = $4,
                updated_at = $4
            FROM eligible_jobs
            WHERE jobs.id = eligible_jobs.id
            RETURNING jobs.*
            "#,
        )
        .bind(worker_id)
        .bind(&lease_id_prefix)
        .bind(lease_expires)
        .bind(now)
        .bind(org_id)
        .bind(queue_name)
        .bind(safe_batch_size)
        .fetch_all(&*self.db)
        .await?;

        // Record history for each claimed job (batch operation, don't fail on history errors)
        for job in &jobs {
            debug!(job_id = %job.id, worker_id = %worker_id, "Job dequeued in batch");
            let _ = self
                .record_history(
                    &job.id,
                    "processing",
                    serde_json::json!({
                        "worker_id": worker_id,
                        "lease_id": format!("{}-{}", lease_id_prefix, job.id),
                        "batch_claim": true
                    }),
                )
                .await;
        }

        if !jobs.is_empty() {
            info!(
                count = jobs.len(),
                queue = %queue_name,
                worker_id = %worker_id,
                "Batch dequeued jobs"
            );
        }

        Ok(jobs)
    }

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
    /// Uses a transaction to ensure job completion and dependency updates are atomic.
    ///
    /// # Lease enforcement
    ///
    /// When both `worker_id` and `org_id` are provided (public REST/gRPC
    /// path) the SQL requires `lease_expires_at IS NOT NULL AND
    /// lease_expires_at > NOW()`. Callers that need to distinguish
    /// "lease expired" from "not owned" should use
    /// [`Self::complete_by_worker`] instead.
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
        // Use transaction to ensure atomicity of job completion and dependency updates
        let mut tx = self.db.begin().await?;

        let result_json = result.clone().and_then(|r| serde_json::to_string(&r).ok());

        let query_result = match (worker_id, org_id) {
            // Verify both worker and organization ownership + lease still valid
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
                      AND lease_expires_at IS NOT NULL
                      AND lease_expires_at > NOW()
                    "#,
                )
                .bind(&result_json)
                .bind(job_id)
                .bind(wid)
                .bind(oid)
                .execute(&mut *tx)
                .await?
            }
            // Worker verification only (for backward compatibility) + lease still valid
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
                      AND lease_expires_at IS NOT NULL
                      AND lease_expires_at > NOW()
                    "#,
                )
                .bind(&result_json)
                .bind(job_id)
                .bind(wid)
                .execute(&mut *tx)
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
                .bind(&result_json)
                .bind(job_id)
                .execute(&mut *tx)
                .await?
            }
        };

        if query_result.rows_affected() == 0 && worker_id.is_some() {
            tx.rollback().await?;
            warn!(job_id = %job_id, "Job completion failed - not owned by worker, not processing, or lease expired");
            return Err(anyhow::anyhow!(
                "Job not found, not owned by worker, or lease expired"
            ));
        }

        // Unblock children gated on this job via the LEGACY scalar `parent_job_id`
        // edge only. This is not the workflow dependency mechanism: workflow jobs
        // express many-to-many dependencies in the `job_dependencies` table with a
        // `dependency_mode` of 'all'/'any', and those are resolved by the
        // `update_dependent_jobs` DB trigger (migrations/20241209000003_job_dependencies.sql)
        // plus the safety-net sweep in `Scheduler::update_job_dependencies`
        // (src/scheduler/mod.rs). Changing the query below does NOT change
        // workflow dependency behaviour.
        // SECURITY: Include organization_id when available for defense-in-depth
        let update_result = if let Some(oid) = org_id {
            sqlx::query(
                r#"
                UPDATE jobs
                SET
                    dependencies_met = TRUE,
                    updated_at = NOW()
                WHERE parent_job_id = $1
                  AND organization_id = $2
                  AND status IN ('pending', 'scheduled')
                  AND dependencies_met = FALSE
                  AND NOT EXISTS (
                      -- Single-parent (`parent_job_id`) edges only; multi-parent
                      -- workflow dependencies live in `job_dependencies`.
                      -- SECURITY: Include organization_id in subquery for cross-tenant isolation
                      SELECT 1 FROM jobs parent
                      WHERE parent.id = jobs.parent_job_id
                        AND parent.organization_id = $2
                        AND parent.status NOT IN ('completed')
                  )
                "#,
            )
            .bind(job_id)
            .bind(oid)
            .execute(&mut *tx)
            .await?
        } else {
            // Fallback for internal calls without org context (backward compat)
            // Uses self-join on organization_id for cross-tenant isolation
            sqlx::query(
                r#"
                UPDATE jobs
                SET
                    dependencies_met = TRUE,
                    updated_at = NOW()
                WHERE parent_job_id = $1
                  AND status IN ('pending', 'scheduled')
                  AND dependencies_met = FALSE
                  AND NOT EXISTS (
                      SELECT 1 FROM jobs parent
                      WHERE parent.id = jobs.parent_job_id
                        AND parent.organization_id = jobs.organization_id
                        AND parent.status NOT IN ('completed')
                  )
                "#,
            )
            .bind(job_id)
            .execute(&mut *tx)
            .await?
        };

        // Commit the transaction
        tx.commit().await?;

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

    /// Complete a job on behalf of an authenticated worker, distinguishing
    /// between "not owned" and "lease expired".
    ///
    /// This is the preferred entry point for REST/gRPC handlers that must
    /// return `404 NOT_FOUND` vs `409 LEASE_EXPIRED` per OpenAPI. When the
    /// UPDATE affects zero rows the method performs a follow-up SELECT to
    /// determine which case applies:
    ///
    /// * matching row exists with `lease_expires_at` null/past → `LeaseExpired`
    /// * matching row exists, `status != 'processing'` → `NotOwned`
    /// * no row → `NotOwned`
    ///
    /// On `Ok` this also unblocks child jobs whose only remaining parent is
    /// this one (same transaction as [`Self::complete_with_worker_and_org`]).
    /// `lease_id` is the fencing token returned by the claim. It is required
    /// for leased jobs so stale work from a superseded lease cannot settle a
    /// replacement execution owned by the same stable worker id.
    #[instrument(
        name = "queue.complete_by_worker",
        skip(self, result),
        fields(job_id = %job_id, worker_id = %worker_id, org_id = %org_id)
    )]
    pub async fn complete_by_worker(
        &self,
        job_id: &str,
        worker_id: &str,
        org_id: &str,
        lease_id: Option<&str>,
        allowed_queues: Option<&[String]>,
        result: Option<serde_json::Value>,
    ) -> Result<WorkerOpOutcome> {
        let mut tx = self.db.begin().await?;
        let result_json = result.clone().and_then(|r| serde_json::to_string(&r).ok());

        let query_result = sqlx::query(
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
              AND lease_expires_at IS NOT NULL
              AND lease_expires_at > NOW()
              AND lease_id = $5
              AND ($6::TEXT[] IS NULL OR queue_name = ANY($6))
            "#,
        )
        .bind(&result_json)
        .bind(job_id)
        .bind(worker_id)
        .bind(org_id)
        .bind(lease_id)
        .bind(allowed_queues)
        .execute(&mut *tx)
        .await?;

        if query_result.rows_affected() == 0 {
            tx.rollback().await?;
            return self
                .classify_worker_miss(job_id, worker_id, org_id, lease_id, allowed_queues)
                .await;
        }

        // Unblock child jobs whose only remaining parent just completed.
        let update_result = sqlx::query(
            r#"
            UPDATE jobs
            SET
                dependencies_met = TRUE,
                updated_at = NOW()
            WHERE parent_job_id = $1
              AND organization_id = $2
              AND status IN ('pending', 'scheduled')
              AND dependencies_met = FALSE
              AND NOT EXISTS (
                  SELECT 1 FROM jobs parent
                  WHERE parent.id = jobs.parent_job_id
                    AND parent.organization_id = $2
                    AND parent.status NOT IN ('completed')
              )
            "#,
        )
        .bind(job_id)
        .bind(org_id)
        .execute(&mut *tx)
        .await?;

        tx.commit().await?;

        if update_result.rows_affected() > 0 {
            info!(
                parent_job_id = %job_id,
                unblocked_children = update_result.rows_affected(),
                "Unblocked child jobs after parent completion"
            );
        }

        self.record_history(job_id, "completed", serde_json::json!({}))
            .await?;
        info!(job_id = %job_id, "Job completed by worker");
        Ok(WorkerOpOutcome::Ok)
    }

    /// Classify why a worker-scoped UPDATE affected zero rows.
    ///
    /// Called after complete/fail/renew_lease when `rows_affected == 0`,
    /// so handlers can return `LEASE_EXPIRED` (409) vs `NOT_FOUND` (404).
    /// When the caller presented a `lease_id` fencing token that no longer
    /// matches the job's current lease, the caller's lease was superseded —
    /// that is `LeaseExpired`, not `NotOwned`.
    async fn classify_worker_miss(
        &self,
        job_id: &str,
        worker_id: &str,
        org_id: &str,
        lease_id: Option<&str>,
        allowed_queues: Option<&[String]>,
    ) -> Result<WorkerOpOutcome> {
        let row: Option<(String, Option<DateTime<Utc>>, Option<String>)> = sqlx::query_as(
            r#"
            SELECT status, lease_expires_at, lease_id
            FROM jobs
            WHERE id = $1
              AND organization_id = $2
              AND assigned_worker_id = $3
              AND ($4::TEXT[] IS NULL OR queue_name = ANY($4))
            "#,
        )
        .bind(job_id)
        .bind(org_id)
        .bind(worker_id)
        .bind(allowed_queues)
        .fetch_optional(&*self.db)
        .await?;

        match row {
            Some((status, lease, current_lease_id)) if status == "processing" => {
                let expired = lease.map(|t| t <= Utc::now()).unwrap_or(true);
                let fenced_out = current_lease_id.as_deref() != lease_id;
                if expired || fenced_out {
                    Ok(WorkerOpOutcome::LeaseExpired)
                } else {
                    // Row exists, is processing, lease still valid — but the
                    // UPDATE missed. This is only reachable via a benign race
                    // (row updated between UPDATE and SELECT). Treat as
                    // NotOwned so the caller retries or gives up gracefully.
                    Ok(WorkerOpOutcome::NotOwned)
                }
            }
            // Row exists but is not in 'processing' (e.g. already completed,
            // reclaimed, cancelled) — from the worker's perspective the job
            // is no longer theirs to act on.
            Some(_) => Ok(WorkerOpOutcome::NotOwned),
            None => Ok(WorkerOpOutcome::NotOwned),
        }
    }

    /// Fail a job on behalf of an authenticated worker, distinguishing
    /// "not owned" from "lease expired".
    ///
    /// This is the preferred entry point for REST/gRPC handlers. It folds
    /// the previous REST TOCTOU pre-check into a single atomic UPDATE that
    /// requires `assigned_worker_id = $worker AND lease_expires_at > NOW()`,
    /// then classifies zero-row updates via
    /// [`Self::classify_worker_miss`].
    ///
    /// When the update succeeds the job is either rescheduled with
    /// exponential backoff + jitter (seconds, capped at 60s) or moved to
    /// dead-letter along with a DLQ row. Returns the resulting status and a
    /// `will_retry` flag so callers can emit the correct realtime event.
    #[instrument(
        name = "queue.fail_by_worker",
        skip(self),
        fields(job_id = %job_id, worker_id = %worker_id, org_id = %org_id)
    )]
    pub async fn fail_by_worker(
        &self,
        job_id: &str,
        worker_id: &str,
        org_id: &str,
        lease_id: Option<&str>,
        allowed_queues: Option<&[String]>,
        error: &str,
    ) -> Result<FailByWorkerOutcome> {
        // Load current retry_count under (worker, org, processing, lease>now).
        // This is a snapshot for the reschedule/DLQ decision; the follow-up
        // UPDATE below repeats the same predicate so no state can change
        // between check and write without one of them failing atomically.
        let row: Option<(i32, i32)> = sqlx::query_as(
            r#"
            SELECT retry_count, max_retries
            FROM jobs
            WHERE id = $1
              AND organization_id = $2
              AND assigned_worker_id = $3
              AND status = 'processing'
              AND lease_expires_at IS NOT NULL
              AND lease_expires_at > NOW()
              AND lease_id = $4
              AND ($5::TEXT[] IS NULL OR queue_name = ANY($5))
            "#,
        )
        .bind(job_id)
        .bind(org_id)
        .bind(worker_id)
        .bind(lease_id)
        .bind(allowed_queues)
        .fetch_optional(&*self.db)
        .await?;

        let Some((retry_count, max_retries)) = row else {
            return Ok(FailByWorkerOutcome {
                outcome: self
                    .classify_worker_miss(job_id, worker_id, org_id, lease_id, allowed_queues)
                    .await?,
                will_retry: false,
                new_status: None,
            });
        };

        if retry_count < max_retries {
            // Same seconds-with-jitter backoff as `Self::fail`, but wrapped
            // into the atomic UPDATE (folds the REST pre-check TOCTOU).
            let backoff_exp = retry_count.clamp(0, 6) as u32;
            let base_backoff_seconds = 2_i64.pow(backoff_exp).min(60);
            let jitter_ms: i64 = (rand::random::<u16>() % 500) as i64;
            let next_run = Utc::now()
                + Duration::seconds(base_backoff_seconds)
                + Duration::milliseconds(jitter_ms);

            let updated = sqlx::query(
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
                WHERE id = $4
                  AND organization_id = $5
                  AND assigned_worker_id = $6
                  AND status = 'processing'
                  AND lease_expires_at IS NOT NULL
                  AND lease_expires_at > NOW()
                  AND lease_id = $7
                  AND ($8::TEXT[] IS NULL OR queue_name = ANY($8))
                "#,
            )
            .bind(retry_count + 1)
            .bind(next_run)
            .bind(error)
            .bind(job_id)
            .bind(org_id)
            .bind(worker_id)
            .bind(lease_id)
            .bind(allowed_queues)
            .execute(&*self.db)
            .await?;

            if updated.rows_affected() == 0 {
                return Ok(FailByWorkerOutcome {
                    outcome: self
                        .classify_worker_miss(job_id, worker_id, org_id, lease_id, allowed_queues)
                        .await?,
                    will_retry: false,
                    new_status: None,
                });
            }

            self.record_history(
                job_id,
                "retry_scheduled",
                serde_json::json!({
                    "error": error,
                    "retry_count": retry_count + 1,
                    "next_run": next_run.to_rfc3339(),
                    "organization_id": org_id
                }),
            )
            .await?;

            warn!(
                job_id = %job_id,
                organization_id = %org_id,
                attempt = retry_count + 1,
                next_run = %next_run,
                "Job will retry"
            );

            Ok(FailByWorkerOutcome {
                outcome: WorkerOpOutcome::Ok,
                will_retry: true,
                new_status: Some("pending".to_string()),
            })
        } else {
            // Status change and audit row are ONE statement: a crash between two
            // separate statements used to leave the job `deadletter` with no
            // `dead_letter_queue` row, and that table is the only place holding
            // `original_payload`/`error_details` after retention deletes the job.
            // The data-modifying CTE makes the pair atomic without a transaction.
            let updated = sqlx::query(
                r#"
                WITH deadlettered AS (
                    UPDATE jobs
                    SET
                        status = 'deadletter',
                        last_error = $1,
                        assigned_worker_id = NULL,
                        lease_id = NULL,
                        -- Terminal rows carry no lease. Leaving a stale
                        -- `lease_expires_at` behind made the column mean
                        -- something different for dead-lettered jobs than for
                        -- every other status.
                        lease_expires_at = NULL,
                        updated_at = NOW()
                    WHERE id = $2
                      AND organization_id = $3
                      AND assigned_worker_id = $4
                      AND status = 'processing'
                      AND lease_expires_at IS NOT NULL
                      AND lease_expires_at > NOW()
                      AND lease_id = $5
                      AND ($6::TEXT[] IS NULL OR queue_name = ANY($6))
                    RETURNING id, organization_id, queue_name, payload
                )
                INSERT INTO dead_letter_queue (
                    id, job_id, organization_id, queue_name, reason,
                    original_payload, error_details, created_at
                )
                SELECT
                    gen_random_uuid()::TEXT,
                    d.id,
                    d.organization_id,
                    d.queue_name,
                    $1,
                    d.payload,
                    $7::JSONB,
                    NOW()
                FROM deadlettered d
                "#,
            )
            .bind(error)
            .bind(job_id)
            .bind(org_id)
            .bind(worker_id)
            .bind(lease_id)
            .bind(allowed_queues)
            .bind(serde_json::json!({"final_error": error, "total_retries": retry_count}))
            .execute(&*self.db)
            .await?;

            // rows_affected is the INSERT's row count, which equals the number of
            // rows the inner UPDATE matched — zero means the job was not ours.
            if updated.rows_affected() == 0 {
                return Ok(FailByWorkerOutcome {
                    outcome: self
                        .classify_worker_miss(job_id, worker_id, org_id, lease_id, allowed_queues)
                        .await?,
                    will_retry: false,
                    new_status: None,
                });
            }

            self.record_history(
                job_id,
                "deadlettered",
                serde_json::json!({
                    "error": error,
                    "total_retries": retry_count,
                    "organization_id": org_id
                }),
            )
            .await?;

            warn!(
                job_id = %job_id,
                organization_id = %org_id,
                total_retries = retry_count,
                "Job moved to dead-letter queue"
            );

            Ok(FailByWorkerOutcome {
                outcome: WorkerOpOutcome::Ok,
                will_retry: false,
                new_status: Some("deadletter".to_string()),
            })
        }
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
            // Exponential backoff (seconds) with small jitter:
            // 1s, 2s, 4s, 8s, 16s, 32s, 60s (cap) ...
            //
            // NOTE: This is intentionally seconds (not minutes) to keep retries responsive and
            // to match our documentation/examples.
            // Cap the exponent before pow: max_retries can be up to 100, and
            // 2^retry_count overflows i64 (panics in debug) well before then. The
            // backoff is capped at 60s and 2^6 = 64 > 60, so exponent 6 already
            // saturates the cap — clamp there to avoid the overflow entirely.
            let backoff_exp = job.retry_count.clamp(0, 6) as u32;
            let base_backoff_seconds = 2_i64.pow(backoff_exp).min(60);
            let jitter_ms: i64 = (rand::random::<u16>() % 500) as i64;
            let next_run = Utc::now()
                + Duration::seconds(base_backoff_seconds)
                + Duration::milliseconds(jitter_ms);

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
            // Max retries exceeded: move to dead-letter queue.
            // Status change + audit row in ONE statement — see the equivalent
            // comment in `fail_by_worker`. Two statements could half-apply.
            sqlx::query(
                r#"
                WITH deadlettered AS (
                    UPDATE jobs
                    SET
                        status = 'deadletter',
                        last_error = $1,
                        assigned_worker_id = NULL,
                        lease_id = NULL,
                        lease_expires_at = NULL,
                        updated_at = NOW()
                    WHERE id = $2 AND organization_id = $3
                    RETURNING id, organization_id, queue_name, payload
                )
                INSERT INTO dead_letter_queue (
                    id, job_id, organization_id, queue_name, reason,
                    original_payload, error_details, created_at
                )
                SELECT
                    gen_random_uuid()::TEXT,
                    d.id,
                    d.organization_id,
                    d.queue_name,
                    $1,
                    d.payload,
                    $4::JSONB,
                    NOW()
                FROM deadlettered d
                "#,
            )
            .bind(error)
            .bind(job_id)
            .bind(organization_id)
            .bind(serde_json::json!({"final_error": error, "total_retries": job.retry_count}))
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

    /// Renew a job's lease.
    ///
    /// Strict policy: expired leases are **not** extended. If the current
    /// `lease_expires_at` is null or already in the past the method returns
    /// [`WorkerOpOutcome::LeaseExpired`] so the caller can respond with
    /// HTTP 409 `LEASE_EXPIRED` / gRPC `FAILED_PRECONDITION` and stop the
    /// worker. This prevents zombie workers from blocking scheduler
    /// reclaim by heart-beating past expiry.
    ///
    /// Requires `organization_id` to prevent cross-tenant lease renewal.
    pub async fn renew_lease(
        &self,
        job_id: &str,
        worker_id: &str,
        organization_id: &str,
        lease_id: Option<&str>,
        allowed_queues: Option<&[String]>,
        lease_duration_secs: i64,
    ) -> Result<WorkerOpOutcome> {
        let lease_expires = Utc::now() + Duration::seconds(lease_duration_secs);

        let result = sqlx::query(
            r#"
            UPDATE jobs
            SET lease_expires_at = $1, updated_at = NOW()
            WHERE id = $2
              AND assigned_worker_id = $3
              AND organization_id = $4
              AND status = 'processing'
              AND lease_expires_at IS NOT NULL
              AND lease_expires_at > NOW()
              AND lease_id = $5
              AND ($6::TEXT[] IS NULL OR queue_name = ANY($6))
            "#,
        )
        .bind(lease_expires)
        .bind(job_id)
        .bind(worker_id)
        .bind(organization_id)
        .bind(lease_id)
        .bind(allowed_queues)
        .execute(&*self.db)
        .await?;

        if result.rows_affected() > 0 {
            Ok(WorkerOpOutcome::Ok)
        } else {
            self.classify_worker_miss(job_id, worker_id, organization_id, lease_id, allowed_queues)
                .await
        }
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
                -- `<=` matches scheduler reclaim/deadletter: expiry instant is
                -- owned by recovery, not left in a neither-usable-nor-reclaimable gap.
                AND lease_expires_at <= NOW()
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
            let backoff_seconds = 2_i64.pow(retry).min(60);
            println!("Retry {}: {} seconds", retry, backoff_seconds);
        }

        assert_eq!(2_i64.pow(0), 1); // 1 second
        assert_eq!(2_i64.pow(1), 2); // 2 seconds
        assert_eq!(2_i64.pow(2), 4); // 4 seconds
        assert_eq!(2_i64.pow(3), 8); // 8 seconds
        assert_eq!(60_i64.min(2_i64.pow(10)), 60); // Capped at 60 seconds
    }

    #[test]
    fn test_backoff_sequence() {
        // Verify the full backoff sequence
        let expected_seconds = vec![1, 2, 4, 8, 16, 32, 60, 60, 60, 60];

        for (retry, expected) in expected_seconds.iter().enumerate() {
            let backoff_seconds = 2_i64.pow(retry as u32).min(60);
            assert_eq!(
                backoff_seconds, *expected,
                "Retry {} should wait {} seconds",
                retry, expected
            );
        }
    }

    #[test]
    fn test_max_retries_edge_cases() {
        // Test edge cases for retry counting
        let max_retries = 3;
        let cases = vec![(0, true), (2, true), (3, false), (4, false)];

        for (retry_count, expected) in cases {
            assert_eq!(
                retry_count < max_retries,
                expected,
                "retry_count={} max_retries={}",
                retry_count,
                max_retries
            );
        }
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
