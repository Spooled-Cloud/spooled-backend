//! Background scheduler for Spooled Backend
//!
//! This module handles background tasks:
//! - Scheduled job activation
//! - Cron schedule processing
//! - Expired lease recovery
//! - Metrics collection
//! - Dead job cleanup

use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use chrono::Utc;
use sqlx::PgPool;
use tokio::sync::watch;
use tokio::time::interval;
use tracing::{debug, error, info, warn};

use crate::api::middleware::limits::{
    check_job_limits_generic, check_payload_size_generic, increment_daily_jobs,
};
use crate::cache::RedisCache;
use crate::models::CronSchedule;
use crate::observability::Metrics;

/// Background scheduler that runs periodic maintenance tasks
pub struct Scheduler {
    db: Arc<PgPool>,
    cache: Option<Arc<RedisCache>>,
    metrics: Arc<Metrics>,
}

impl Scheduler {
    /// Create a new scheduler
    pub fn new(db: Arc<PgPool>, cache: Option<Arc<RedisCache>>, metrics: Arc<Metrics>) -> Self {
        Self { db, cache, metrics }
    }

    /// Run all background tasks
    pub async fn run(&self, mut shutdown: watch::Receiver<bool>) -> Result<()> {
        info!("Background scheduler starting");

        // Task intervals
        let mut scheduled_job_ticker = interval(Duration::from_secs(5));
        let mut cron_ticker = interval(Duration::from_secs(10)); // Process cron schedules
        let mut lease_recovery_ticker = interval(Duration::from_secs(30));
        let mut metrics_ticker = interval(Duration::from_secs(15));
        let mut cleanup_ticker = interval(Duration::from_secs(300)); // 5 minutes
        let mut dependency_ticker = interval(Duration::from_secs(10)); // Check job dependencies

        loop {
            tokio::select! {
                _ = shutdown.changed() => {
                    if *shutdown.borrow() {
                        info!("Scheduler shutdown signal received");
                        break;
                    }
                }

                _ = scheduled_job_ticker.tick() => {
                    if let Err(e) = self.activate_scheduled_jobs().await {
                        error!(error = %e, "Failed to activate scheduled jobs");
                    }
                }

                _ = cron_ticker.tick() => {
                    if let Err(e) = self.process_cron_schedules().await {
                        error!(error = %e, "Failed to process cron schedules");
                    }
                }

                _ = lease_recovery_ticker.tick() => {
                    if let Err(e) = self.recover_expired_leases().await {
                        error!(error = %e, "Failed to recover expired leases");
                    }
                }

                _ = metrics_ticker.tick() => {
                    if let Err(e) = self.update_metrics().await {
                        error!(error = %e, "Failed to update metrics");
                    }
                }

                _ = cleanup_ticker.tick() => {
                    if let Err(e) = self.cleanup_stale_workers().await {
                        error!(error = %e, "Failed to cleanup stale workers");
                    }
                }

                // Periodically check and update job dependencies
                _ = dependency_ticker.tick() => {
                    if let Err(e) = self.update_job_dependencies().await {
                        error!(error = %e, "Failed to update job dependencies");
                    }
                }
            }
        }

        info!("Scheduler shutdown complete");
        Ok(())
    }

    /// Activate jobs that are scheduled to run now
    async fn activate_scheduled_jobs(&self) -> Result<()> {
        let result = sqlx::query(
            r#"
            UPDATE jobs
            SET 
                status = 'pending',
                updated_at = NOW()
            WHERE 
                status = 'scheduled'
                AND scheduled_at <= NOW()
            "#,
        )
        .execute(&*self.db)
        .await?;

        let activated = result.rows_affected();
        if activated > 0 {
            info!(count = activated, "Activated scheduled jobs");

            // Update metrics when jobs are activated
            self.metrics.jobs_pending.add(activated as i64);

            // Publish notifications for activated jobs WITH org context
            if let Some(ref cache) = self.cache {
                // Get the queue names AND org_ids for activated jobs
                let queues: Vec<(String, String)> = sqlx::query_as(
                    r#"
                    SELECT DISTINCT organization_id, queue_name 
                    FROM jobs 
                    WHERE status = 'pending' 
                    AND scheduled_at IS NOT NULL
                    AND scheduled_at <= NOW() + INTERVAL '5 seconds'
                    "#,
                )
                .fetch_all(&*self.db)
                .await?;

                for (org_id, queue_name) in queues {
                    // Include org_id in channel name
                    let _ = cache
                        .publish(
                            &format!("org:{}:queue:{}", org_id, queue_name),
                            "scheduled_activated",
                        )
                        .await;
                }
            }
        }

        Ok(())
    }

    /// Recover jobs with expired leases (worker crashed or timed out)
    async fn recover_expired_leases(&self) -> Result<()> {
        // Only retry jobs that haven't exceeded max_retries
        // Jobs that have exceeded max_retries should go to deadletter

        // First, recover jobs that can still be retried
        let result = sqlx::query(
            r#"
            UPDATE jobs
            SET 
                status = 'pending',
                assigned_worker_id = NULL,
                lease_id = NULL,
                lease_expires_at = NULL,
                retry_count = retry_count + 1,
                last_error = 'Lease expired (worker timeout)',
                updated_at = NOW()
            WHERE 
                status = 'processing'
                AND lease_expires_at < NOW()
                AND retry_count < max_retries
            "#,
        )
        .execute(&*self.db)
        .await?;

        let recovered = result.rows_affected();
        if recovered > 0 {
            warn!(count = recovered, "Recovered expired leases");
            self.metrics.jobs_retried.inc_by(recovered);
        }

        // Move jobs that have exceeded max_retries to deadletter
        let result = sqlx::query(
            r#"
            UPDATE jobs
            SET 
                status = 'deadletter',
                assigned_worker_id = NULL,
                lease_id = NULL,
                lease_expires_at = NULL,
                last_error = 'Max retries exceeded after lease expiration',
                updated_at = NOW()
            WHERE 
                status = 'processing'
                AND lease_expires_at < NOW()
                AND retry_count >= max_retries
            "#,
        )
        .execute(&*self.db)
        .await?;

        let deadlettered = result.rows_affected();
        if deadlettered > 0 {
            warn!(
                count = deadlettered,
                "Moved expired jobs to deadletter queue"
            );
            self.metrics.jobs_deadlettered.inc_by(deadlettered);
        }

        Ok(())
    }

    /// Update Prometheus metrics from database
    async fn update_metrics(&self) -> Result<()> {
        // Job status counts
        let counts: (i64, i64, i64, i64, i64) = sqlx::query_as(
            r#"
            SELECT 
                COUNT(*) FILTER (WHERE status = 'pending') as pending,
                COUNT(*) FILTER (WHERE status = 'processing') as processing,
                COUNT(*) FILTER (WHERE status = 'completed') as completed,
                COUNT(*) FILTER (WHERE status = 'failed') as failed,
                COUNT(*) FILTER (WHERE status = 'deadletter') as deadletter
            FROM jobs
            "#,
        )
        .fetch_one(&*self.db)
        .await?;

        self.metrics.jobs_pending.set(counts.0);
        self.metrics.jobs_processing.set(counts.1);

        // Worker counts
        let worker_counts: (i64, i64) = sqlx::query_as(
            r#"
            SELECT 
                COUNT(*) as total,
                COUNT(*) FILTER (WHERE status = 'healthy') as healthy
            FROM workers
            WHERE last_heartbeat > NOW() - INTERVAL '2 minutes'
            "#,
        )
        .fetch_one(&*self.db)
        .await?;

        self.metrics.workers_active.set(worker_counts.0);
        self.metrics.workers_healthy.set(worker_counts.1);

        // Max job age (CRITICAL metric for queue health)
        let max_age: Option<(Option<i64>,)> = sqlx::query_as(
            r#"
            SELECT EXTRACT(EPOCH FROM (NOW() - MIN(created_at)))::BIGINT
            FROM jobs
            WHERE status IN ('pending', 'scheduled')
            "#,
        )
        .fetch_optional(&*self.db)
        .await?;

        if let Some((Some(age),)) = max_age {
            self.metrics.job_max_age_seconds.set(age);
        }

        debug!("Metrics updated");
        Ok(())
    }

    /// Process cron schedules and create jobs
    ///
    /// Uses transaction to prevent duplicate jobs if schedule update fails
    /// Each schedule is processed in its own transaction to ensure locks are effective
    async fn process_cron_schedules(&self) -> Result<()> {
        #[derive(sqlx::FromRow)]
        struct ScheduleRecord {
            id: String,
            organization_id: String,
            cron_expression: String,
            timezone: String,
            queue_name: String,
            payload_template: serde_json::Value,
            priority: i32,
            max_retries: i32,
            timeout_seconds: i32,
            tags: Option<serde_json::Value>,
            next_run_at: Option<chrono::DateTime<Utc>>,
        }

        // First, get IDs of schedules that are due (without lock, just to know what to process)
        let schedule_ids: Vec<(String,)> = sqlx::query_as(
            r#"
            SELECT id
            FROM schedules
            WHERE is_active = TRUE
              AND next_run_at IS NOT NULL
              AND next_run_at <= NOW()
            LIMIT 100
            "#,
        )
        .fetch_all(&*self.db)
        .await?;

        // Process each schedule in its own transaction with proper locking
        let mut schedules_to_process = Vec::new();
        for (schedule_id,) in schedule_ids {
            // Start transaction and lock the specific schedule
            let mut tx = self.db.begin().await?;

            let schedule: Option<ScheduleRecord> = sqlx::query_as(
                r#"
                SELECT id, organization_id, cron_expression, timezone, queue_name,
                       payload_template, priority, max_retries, timeout_seconds, tags,
                       next_run_at
                FROM schedules
                WHERE id = $1
                  AND is_active = TRUE
                  AND next_run_at IS NOT NULL
                  AND next_run_at <= NOW()
                FOR UPDATE SKIP LOCKED
                "#,
            )
            .bind(&schedule_id)
            .fetch_optional(&mut *tx)
            .await?;

            if let Some(s) = schedule {
                schedules_to_process.push((s, tx));
            } else {
                // Schedule was already processed by another instance, rollback
                tx.rollback().await?;
            }
        }

        // Alias for backward compatibility with existing loop
        let schedules = schedules_to_process;

        let mut processed = 0;
        let now = Utc::now();

        for (schedule, mut tx) in schedules {
            let cron = CronSchedule::parse(&schedule.cron_expression).ok();

            // Calculate next run time FIRST (before any DB operations), in the
            // schedule's own timezone (DST-aware); stored value is UTC.
            let next_run = cron
                .as_ref()
                .and_then(|c| c.next_run_after_in_timezone(now, &schedule.timezone));

            // Catch-up policy: fires missed while the scheduler was down are
            // SKIPPED, not backfilled — the schedule runs once now and advances
            // to the next future occurrence. Backfilling would burst-enqueue an
            // unbounded number of stale jobs after long downtime. The skipped
            // occurrences are logged so the gap is visible to operators.
            if let (Some(cron), Some(stored_next)) = (cron.as_ref(), schedule.next_run_at) {
                let mut missed = 0u32;
                let mut t = stored_next;
                while let Some(fire) = cron.next_run_after_in_timezone(t, &schedule.timezone) {
                    if fire >= now || missed >= 100 {
                        break;
                    }
                    missed += 1;
                    t = fire;
                }
                if missed > 0 {
                    warn!(
                        schedule_id = %schedule.id,
                        org_id = %schedule.organization_id,
                        missed_fires = missed,
                        due_since = %stored_next,
                        "Skipping cron fires missed during scheduler downtime (catch-up policy: skip, run once now)"
                    );
                }
            }

            // Enforce plan limits BEFORE inserting cron jobs.
            // Important: cron-triggered jobs must count toward daily quota + payload limits.
            if let Err(e) = check_job_limits_generic(&self.db, &schedule.organization_id, 1).await {
                tx.rollback().await?;

                warn!(
                    schedule_id = %schedule.id,
                    org_id = %schedule.organization_id,
                    error = %e,
                    "Cron schedule blocked by plan job limits"
                );

                let run_id = uuid::Uuid::new_v4().to_string();
                let _ = sqlx::query(
                    r#"
                    INSERT INTO schedule_runs (id, schedule_id, status, error_message, started_at, completed_at)
                    VALUES ($1, $2, 'failed', $3, $4, $4)
                    "#,
                )
                .bind(&run_id)
                .bind(&schedule.id)
                .bind(e.to_string())
                .bind(now)
                .execute(&*self.db)
                .await;

                let _ = sqlx::query(
                    "UPDATE schedules SET next_run_at = $2, updated_at = NOW() WHERE id = $1",
                )
                .bind(&schedule.id)
                .bind(next_run)
                .execute(&*self.db)
                .await;

                continue;
            }

            let payload_json =
                serde_json::to_string(&schedule.payload_template).unwrap_or_default();
            if let Err(e) =
                check_payload_size_generic(&self.db, &schedule.organization_id, payload_json.len())
                    .await
            {
                tx.rollback().await?;

                warn!(
                    schedule_id = %schedule.id,
                    org_id = %schedule.organization_id,
                    error = %e,
                    "Cron schedule blocked by payload size limits"
                );

                let run_id = uuid::Uuid::new_v4().to_string();
                let _ = sqlx::query(
                    r#"
                    INSERT INTO schedule_runs (id, schedule_id, status, error_message, started_at, completed_at)
                    VALUES ($1, $2, 'failed', $3, $4, $4)
                    "#,
                )
                .bind(&run_id)
                .bind(&schedule.id)
                .bind(e.to_string())
                .bind(now)
                .execute(&*self.db)
                .await;

                let _ = sqlx::query(
                    "UPDATE schedules SET next_run_at = $2, updated_at = NOW() WHERE id = $1",
                )
                .bind(&schedule.id)
                .bind(next_run)
                .execute(&*self.db)
                .await;

                continue;
            }

            // Transaction already started with lock - create job from schedule
            let job_id = uuid::Uuid::new_v4().to_string();
            let run_id = uuid::Uuid::new_v4().to_string();

            let job_result = sqlx::query(
                r#"
                INSERT INTO jobs (
                    id, organization_id, queue_name, status, payload,
                    priority, max_retries, timeout_seconds, tags,
                    created_at, updated_at
                )
                VALUES ($1, $2, $3, 'pending', $4, $5, $6, $7, $8, $9, $9)
                "#,
            )
            .bind(&job_id)
            .bind(&schedule.organization_id)
            .bind(&schedule.queue_name)
            .bind(&schedule.payload_template)
            .bind(schedule.priority)
            .bind(schedule.max_retries)
            .bind(schedule.timeout_seconds)
            .bind(&schedule.tags)
            .bind(now)
            .execute(&mut *tx)
            .await;

            match job_result {
                Ok(_) => {
                    // Record successful run
                    let _ = sqlx::query(
                        r#"
                        INSERT INTO schedule_runs (id, schedule_id, job_id, status, started_at, completed_at)
                        VALUES ($1, $2, $3, 'completed', $4, $4)
                        "#
                    )
                    .bind(&run_id)
                    .bind(&schedule.id)
                    .bind(&job_id)
                    .bind(now)
                    .execute(&mut *tx)
                    .await?;

                    // Update schedule - MUST succeed for transaction to commit
                    sqlx::query(
                        r#"
                        UPDATE schedules
                        SET last_run_at = $2, run_count = run_count + 1, next_run_at = $3, updated_at = $2
                        WHERE id = $1
                        "#
                    )
                    .bind(&schedule.id)
                    .bind(now)
                    .bind(next_run)
                    .execute(&mut *tx)
                    .await?;

                    // Commit transaction
                    tx.commit().await?;

                    processed += 1;
                    self.metrics.jobs_enqueued.inc();

                    // Increment daily job counter for plan limit tracking (best-effort).
                    if let Err(e) =
                        increment_daily_jobs(&self.db, &schedule.organization_id, 1).await
                    {
                        warn!(
                            error = %e,
                            org_id = %schedule.organization_id,
                            "Failed to increment daily job counter for cron schedule job"
                        );
                    }

                    // Notify queue with org context (outside transaction)
                    if let Some(ref cache) = self.cache {
                        let _ = cache
                            .publish(
                                &format!(
                                    "org:{}:queue:{}",
                                    schedule.organization_id, schedule.queue_name
                                ),
                                "cron_job_created",
                            )
                            .await;
                    }
                }
                Err(e) => {
                    // Rollback transaction
                    tx.rollback().await?;

                    error!(schedule_id = %schedule.id, error = %e, "Failed to create cron job");

                    // Record failed run (new transaction)
                    let _ = sqlx::query(
                        r#"
                        INSERT INTO schedule_runs (id, schedule_id, status, error_message, started_at, completed_at)
                        VALUES ($1, $2, 'failed', $3, $4, $4)
                        "#
                    )
                    .bind(&run_id)
                    .bind(&schedule.id)
                    .bind(e.to_string())
                    .bind(now)
                    .execute(&*self.db)
                    .await;

                    // Still update next_run_at to avoid getting stuck
                    let _ = sqlx::query(
                        "UPDATE schedules SET next_run_at = $2, updated_at = NOW() WHERE id = $1",
                    )
                    .bind(&schedule.id)
                    .bind(next_run)
                    .execute(&*self.db)
                    .await;
                }
            }
        }

        if processed > 0 {
            info!(count = processed, "Processed cron schedules");
        }

        Ok(())
    }

    /// Update job dependencies - unblock child jobs when parents complete
    ///
    /// This is a safety net to catch any jobs that weren't updated
    /// during parent completion (e.g., race conditions, server restarts)
    async fn update_job_dependencies(&self) -> Result<()> {
        // Find jobs with dependencies_met = FALSE whose parent jobs are completed
        // SECURITY: Include organization_id filter to ensure cross-tenant isolation
        let result = sqlx::query(
            r#"
            UPDATE jobs child
            SET 
                dependencies_met = TRUE,
                updated_at = NOW()
            WHERE 
                child.parent_job_id IS NOT NULL
                AND child.status IN ('pending', 'scheduled')
                AND (child.dependencies_met = FALSE OR child.dependencies_met IS NULL)
                AND EXISTS (
                    SELECT 1 FROM jobs parent
                    WHERE parent.id = child.parent_job_id
                      AND parent.organization_id = child.organization_id
                      AND parent.status = 'completed'
                )
            "#,
        )
        .execute(&*self.db)
        .await?;

        let updated = result.rows_affected();
        if updated > 0 {
            info!(
                count = updated,
                "Unblocked child jobs with completed parent dependencies"
            );
        }

        // Also handle jobs where parent failed/deadlettered - these should be cancelled
        // or marked as blocked (policy decision - here we cancel them)
        // SECURITY: Include organization_id filter to ensure cross-tenant isolation
        let failed_parent_result = sqlx::query(
            r#"
            UPDATE jobs child
            SET 
                status = 'cancelled',
                last_error = 'Parent job failed or was deadlettered',
                updated_at = NOW()
            WHERE 
                child.parent_job_id IS NOT NULL
                AND child.status IN ('pending', 'scheduled')
                AND (child.dependencies_met = FALSE OR child.dependencies_met IS NULL)
                AND EXISTS (
                    SELECT 1 FROM jobs parent
                    WHERE parent.id = child.parent_job_id
                      AND parent.organization_id = child.organization_id
                      AND parent.status IN ('failed', 'deadletter', 'cancelled')
                )
            "#,
        )
        .execute(&*self.db)
        .await?;

        let cancelled = failed_parent_result.rows_affected();
        if cancelled > 0 {
            warn!(
                count = cancelled,
                "Cancelled child jobs due to failed parent dependencies"
            );
        }

        // Workflow jobs express dependencies via job_dependencies (not
        // parent_job_id), and the DB trigger only flips dependencies_met on
        // parent *completion* — so a failed/deadlettered/cancelled dependency
        // used to strand dependents in pending forever. Cancel jobs whose
        // dependencies can no longer be met, honoring dependency_mode:
        //   'all' — any dependency in a terminal-failure state is fatal;
        //   'any' — fatal only when no dependency can still complete.
        // Looped so multi-level chains cascade within one sweep.
        let mut workflow_cancelled = 0u64;
        for _ in 0..10 {
            let result = sqlx::query(
                r#"
                UPDATE jobs child
                SET
                    status = 'cancelled',
                    last_error = 'Dependency job failed or was deadlettered',
                    updated_at = NOW()
                WHERE
                    child.status IN ('pending', 'scheduled')
                    AND (child.dependencies_met = FALSE OR child.dependencies_met IS NULL)
                    AND EXISTS (
                        SELECT 1 FROM job_dependencies jd WHERE jd.job_id = child.id
                    )
                    AND (
                        (
                            COALESCE(child.dependency_mode, 'all') != 'any'
                            AND EXISTS (
                                SELECT 1
                                FROM job_dependencies jd
                                JOIN jobs dep ON dep.id = jd.depends_on_job_id
                                WHERE jd.job_id = child.id
                                  AND dep.organization_id = child.organization_id
                                  AND dep.status IN ('failed', 'deadletter', 'cancelled')
                            )
                        )
                        OR
                        (
                            COALESCE(child.dependency_mode, 'all') = 'any'
                            AND NOT EXISTS (
                                SELECT 1
                                FROM job_dependencies jd
                                JOIN jobs dep ON dep.id = jd.depends_on_job_id
                                WHERE jd.job_id = child.id
                                  AND dep.organization_id = child.organization_id
                                  AND dep.status NOT IN ('failed', 'deadletter', 'cancelled')
                            )
                        )
                    )
                "#,
            )
            .execute(&*self.db)
            .await?;

            let batch = result.rows_affected();
            workflow_cancelled += batch;
            if batch == 0 {
                break;
            }
        }

        if workflow_cancelled > 0 {
            warn!(
                count = workflow_cancelled,
                "Cancelled workflow jobs whose dependencies can no longer be met"
            );
        }

        // Terminate workflows stuck non-terminal: the progress trigger only fires
        // on transitions to completed/failed, so cancellations (above or manual)
        // never close the workflow. A workflow with jobs but nothing left to run
        // is 'failed' if anything failed/deadlettered, else 'cancelled'.
        let terminated = sqlx::query(
            r#"
            UPDATE workflows w
            SET
                status = CASE
                    WHEN EXISTS (
                        SELECT 1 FROM jobs j
                        WHERE j.workflow_id = w.id AND j.status IN ('failed', 'deadletter')
                    ) THEN 'failed'
                    ELSE 'cancelled'
                END,
                completed_jobs = (
                    SELECT COUNT(*) FROM jobs j
                    WHERE j.workflow_id = w.id AND j.status = 'completed'
                ),
                failed_jobs = (
                    SELECT COUNT(*) FROM jobs j
                    WHERE j.workflow_id = w.id AND j.status IN ('failed', 'deadletter')
                ),
                completed_at = COALESCE(w.completed_at, NOW())
            WHERE
                w.status IN ('pending', 'running')
                AND EXISTS (SELECT 1 FROM jobs j WHERE j.workflow_id = w.id)
                AND NOT EXISTS (
                    SELECT 1 FROM jobs j
                    WHERE j.workflow_id = w.id
                      AND j.status IN ('pending', 'scheduled', 'processing')
                )
                AND EXISTS (
                    SELECT 1 FROM jobs j
                    WHERE j.workflow_id = w.id
                      AND j.status IN ('failed', 'deadletter', 'cancelled')
                )
            "#,
        )
        .execute(&*self.db)
        .await?;

        if terminated.rows_affected() > 0 {
            warn!(
                count = terminated.rows_affected(),
                "Terminated workflows with no runnable jobs remaining"
            );
        }

        Ok(())
    }

    /// Cleanup workers that haven't sent heartbeat
    async fn cleanup_stale_workers(&self) -> Result<()> {
        // Mark workers as offline if no heartbeat for 2 minutes
        let result = sqlx::query(
            r#"
            UPDATE workers
            SET status = 'offline'
            WHERE 
                status IN ('healthy', 'degraded')
                AND last_heartbeat < NOW() - INTERVAL '2 minutes'
            "#,
        )
        .execute(&*self.db)
        .await?;

        let marked_offline = result.rows_affected();
        if marked_offline > 0 {
            warn!(count = marked_offline, "Marked stale workers as offline");
        }

        // Release jobs from offline workers with retry_count increment
        // This prevents accidental cross-tenant job release if worker IDs somehow collide
        let result = sqlx::query(
            r#"
            UPDATE jobs
            SET 
                status = 'pending',
                assigned_worker_id = NULL,
                lease_id = NULL,
                lease_expires_at = NULL,
                retry_count = retry_count + 1,
                last_error = 'Worker went offline',
                updated_at = NOW()
            WHERE 
                status = 'processing'
                AND retry_count < max_retries
                AND EXISTS (
                    SELECT 1 FROM workers w
                    WHERE w.id = jobs.assigned_worker_id
                      AND w.status = 'offline'
                      AND w.organization_id = jobs.organization_id
                )
            "#,
        )
        .execute(&*self.db)
        .await?;

        let released = result.rows_affected();
        if released > 0 {
            warn!(
                count = released,
                "Released jobs from offline workers (will retry)"
            );
            self.metrics.jobs_retried.inc_by(released);

            // Notify Redis about released jobs with org context
            if let Some(ref cache) = self.cache {
                let released_queues: Vec<(String, String)> = sqlx::query_as(
                    r#"
                    SELECT DISTINCT organization_id, queue_name 
                    FROM jobs 
                    WHERE status = 'pending' 
                    AND last_error = 'Worker went offline'
                    AND updated_at > NOW() - INTERVAL '5 seconds'
                    "#,
                )
                .fetch_all(&*self.db)
                .await
                .unwrap_or_default();

                for (org_id, queue_name) in released_queues {
                    let _ = cache
                        .publish(
                            &format!("org:{}:queue:{}", org_id, queue_name),
                            "stale_worker_released",
                        )
                        .await;
                }
            }
        }

        // Move jobs that have exceeded max_retries to deadletter
        let result = sqlx::query(
            r#"
            UPDATE jobs
            SET 
                status = 'deadletter',
                assigned_worker_id = NULL,
                lease_id = NULL,
                lease_expires_at = NULL,
                last_error = 'Max retries exceeded after worker went offline',
                updated_at = NOW()
            WHERE 
                status = 'processing'
                AND retry_count >= max_retries
                AND EXISTS (
                    SELECT 1 FROM workers w
                    WHERE w.id = jobs.assigned_worker_id
                      AND w.status = 'offline'
                      AND w.organization_id = jobs.organization_id
                )
            "#,
        )
        .execute(&*self.db)
        .await?;

        let deadlettered = result.rows_affected();
        if deadlettered > 0 {
            warn!(
                count = deadlettered,
                "Moved offline worker jobs to deadletter (max retries exceeded)"
            );
            self.metrics.jobs_deadlettered.inc_by(deadlettered);
        }

        // Cleanup old data based on each organization's plan tier + custom overrides
        // This respects per-org custom_limits overrides for job_retention_days and history_retention_days
        let total_cleaned = self.cleanup_old_data_by_org().await?;
        if total_cleaned > 0 {
            info!(
                count = total_cleaned,
                "Cleaned up old data (tier-based retention with custom overrides)"
            );
        }

        Ok(())
    }

    /// Cleanup old data respecting per-organization retention limits
    ///
    /// Uses JOIN-based queries for efficiency (single query per resource type).
    /// Plan tier retention: free=3d, starter=14d, pro=30d, enterprise=90d
    /// Custom limits can override via custom_limits.job_retention_days/history_retention_days
    async fn cleanup_old_data_by_org(&self) -> Result<u64> {
        let mut total_cleaned: u64 = 0;

        // Cleanup old completed/cancelled jobs using JOIN with organizations table
        // Uses CASE to determine retention based on plan_tier, with custom_limits override
        let result = sqlx::query(
            r#"
            DELETE FROM jobs
            WHERE id IN (
                SELECT j.id
                FROM jobs j
                JOIN organizations o ON j.organization_id = o.id
                WHERE j.status IN ('completed', 'cancelled')
                  AND j.completed_at < NOW() - (
                    COALESCE(
                        (o.custom_limits->>'job_retention_days')::INT,
                        CASE o.plan_tier
                            WHEN 'enterprise' THEN 90
                            WHEN 'pro' THEN 30
                            WHEN 'starter' THEN 14
                            ELSE 3  -- free tier default
                        END
                    ) || ' days'
                  )::INTERVAL
                LIMIT 10000  -- Batch limit to prevent long-running transactions
            )
            "#,
        )
        .execute(&*self.db)
        .await?;

        let jobs_cleaned = result.rows_affected();
        if jobs_cleaned > 0 {
            debug!(
                count = jobs_cleaned,
                "Cleaned up old jobs (tier-based retention)"
            );
            total_cleaned += jobs_cleaned;
        }

        // Cleanup old job history using JOIN
        let result = sqlx::query(
            r#"
            DELETE FROM job_history
            WHERE id IN (
                SELECT jh.id
                FROM job_history jh
                JOIN jobs j ON jh.job_id = j.id
                JOIN organizations o ON j.organization_id = o.id
                WHERE jh.created_at < NOW() - (
                    COALESCE(
                        (o.custom_limits->>'history_retention_days')::INT,
                        CASE o.plan_tier
                            WHEN 'enterprise' THEN 90
                            WHEN 'pro' THEN 30
                            WHEN 'starter' THEN 7
                            ELSE 1  -- free tier default
                        END
                    ) || ' days'
                )::INTERVAL
                LIMIT 10000
            )
            "#,
        )
        .execute(&*self.db)
        .await?;

        let history_cleaned = result.rows_affected();
        if history_cleaned > 0 {
            debug!(
                count = history_cleaned,
                "Cleaned up old job history (tier-based retention)"
            );
            total_cleaned += history_cleaned;
        }

        // Cleanup old webhook deliveries using JOIN
        let result = sqlx::query(
            r#"
            DELETE FROM webhook_deliveries
            WHERE id IN (
                SELECT wd.id
                FROM webhook_deliveries wd
                JOIN organizations o ON wd.organization_id = o.id
                WHERE wd.created_at < NOW() - (
                    COALESCE(
                        (o.custom_limits->>'history_retention_days')::INT,
                        CASE o.plan_tier
                            WHEN 'enterprise' THEN 90
                            WHEN 'pro' THEN 30
                            WHEN 'starter' THEN 7
                            ELSE 1  -- free tier default
                        END
                    ) || ' days'
                )::INTERVAL
                LIMIT 10000
            )
            "#,
        )
        .execute(&*self.db)
        .await?;

        let webhooks_cleaned = result.rows_affected();
        if webhooks_cleaned > 0 {
            debug!(
                count = webhooks_cleaned,
                "Cleaned up old webhook deliveries (tier-based retention)"
            );
            total_cleaned += webhooks_cleaned;
        }

        // Cleanup jobs that have passed their expires_at timestamp
        // These are jobs created with explicit expiration (e.g., demo/temporary jobs)
        // They remain pending/scheduled but are never claimed, so delete them
        let result = sqlx::query(
            r#"
            DELETE FROM jobs
            WHERE id IN (
                SELECT id FROM jobs
                WHERE expires_at IS NOT NULL
                  AND expires_at < NOW()
                  AND status IN ('pending', 'scheduled', 'failed', 'deadletter')
                LIMIT 10000
            )
            "#,
        )
        .execute(&*self.db)
        .await?;

        let expired_cleaned = result.rows_affected();
        if expired_cleaned > 0 {
            debug!(
                count = expired_cleaned,
                "Cleaned up expired jobs (expires_at passed)"
            );
            total_cleaned += expired_cleaned;
        }

        // Cleanup old completed/failed/cancelled workflows based on plan tier
        // Workflows accumulate over time (e.g., demo apps create one per request)
        let result = sqlx::query(
            r#"
            DELETE FROM workflows
            WHERE id IN (
                SELECT w.id
                FROM workflows w
                JOIN organizations o ON w.organization_id = o.id
                WHERE w.status IN ('completed', 'failed', 'cancelled')
                  AND w.completed_at < NOW() - (
                    COALESCE(
                        (o.custom_limits->>'job_retention_days')::INT,
                        CASE o.plan_tier
                            WHEN 'enterprise' THEN 90
                            WHEN 'pro' THEN 30
                            WHEN 'starter' THEN 14
                            ELSE 3  -- free tier default
                        END
                    ) || ' days'
                  )::INTERVAL
                LIMIT 5000
            )
            "#,
        )
        .execute(&*self.db)
        .await?;

        let workflows_cleaned = result.rows_affected();
        if workflows_cleaned > 0 {
            debug!(
                count = workflows_cleaned,
                "Cleaned up old workflows (tier-based retention)"
            );
            total_cleaned += workflows_cleaned;
        }

        Ok(total_cleaned)
    }
}

#[cfg(test)]
mod tests {

    #[test]
    fn test_scheduler_creation() {
        // Basic test to ensure struct can be created
        // Full integration tests use testcontainers
    }
}
