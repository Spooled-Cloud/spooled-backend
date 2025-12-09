//! Worker module for Spooled Backend
//!
//! Workers are job processors that dequeue and execute jobs from queues.
//! This module implements the worker loop with Redis pub/sub + fallback polling.

use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use chrono::Utc;
use futures::StreamExt;
use sqlx::PgPool;
use tokio::sync::watch;
use tokio::time::interval;
use tracing::{debug, error, info, instrument, warn};

use crate::cache::RedisCache;
use crate::models::Job;
use crate::queue::QueueManager;

/// Worker loop configuration
#[derive(Debug, Clone)]
pub struct WorkerConfig {
    /// Worker ID
    pub worker_id: String,
    /// Organization ID
    pub organization_id: String,
    /// Queue to process
    pub queue_name: String,
    /// Maximum concurrent jobs
    pub max_concurrency: usize,
    /// Heartbeat interval in seconds
    pub heartbeat_interval_secs: u64,
    /// Lease duration in seconds
    pub lease_duration_secs: i64,
    /// Fallback poll interval in seconds (when Redis unavailable)
    pub fallback_poll_interval_secs: u64,
}

/// Worker loop that processes jobs
pub struct WorkerLoop {
    config: WorkerConfig,
    queue_manager: QueueManager,
    db: Arc<PgPool>,
    cache: Option<Arc<RedisCache>>,
}

impl WorkerLoop {
    /// Create a new worker loop
    pub fn new(config: WorkerConfig, db: Arc<PgPool>, cache: Option<Arc<RedisCache>>) -> Self {
        let queue_manager = QueueManager::new(db.clone(), cache.clone());

        Self {
            config,
            queue_manager,
            db,
            cache,
        }
    }

    /// Run the worker loop with graceful shutdown support
    ///
    /// This implements the CRITICAL pattern: Redis + Fallback Polling
    /// Workers listen to Redis BUT also poll database every 5-10 seconds.
    /// This guarantees job delivery even if Redis fails temporarily.
    pub async fn run(&self, mut shutdown: watch::Receiver<bool>) -> Result<()> {
        let mut active_jobs: Vec<String> = Vec::new();
        let mut fallback_poll_ticker =
            interval(Duration::from_secs(self.config.fallback_poll_interval_secs));
        let mut heartbeat_ticker =
            interval(Duration::from_secs(self.config.heartbeat_interval_secs));

        info!(
            worker_id = %self.config.worker_id,
            queue = %self.config.queue_name,
            max_concurrency = self.config.max_concurrency,
            "Worker started"
        );

        // Register worker
        self.register_worker().await?;

        // Subscribe to Redis channel with org context
        // Previously subscribed to "queue:{name}" but we publish to "org:{org}:queue:{name}"
        let mut redis_rx = if let Some(ref cache) = self.cache {
            let channel = format!(
                "org:{}:queue:{}",
                self.config.organization_id, self.config.queue_name
            );
            match cache.subscribe(&channel).await {
                Ok(pubsub) => Some(pubsub),
                Err(e) => {
                    warn!(error = %e, channel = %channel, "Failed to subscribe to Redis, using polling only");
                    None
                }
            }
        } else {
            None
        };

        loop {
            tokio::select! {
                // Graceful shutdown signal
                _ = shutdown.changed() => {
                    if *shutdown.borrow() {
                        info!("Worker shutdown signal received");
                        self.shutdown_gracefully(&active_jobs).await?;
                        break;
                    }
                }

                // Redis notification (new job available)
                msg = async {
                    if let Some(ref mut pubsub) = redis_rx {
                        pubsub.on_message().next().await
                    } else {
                        std::future::pending::<Option<redis::Msg>>().await
                    }
                } => {
                    if msg.is_some() {
                        debug!("Redis notification: new job available");

                        // Clean up completed jobs before checking capacity
                        // Previously, active_jobs could fill with completed jobs, blocking new work
                        self.cleanup_completed_jobs(&mut active_jobs).await;

                        // Try to dequeue if capacity available
                        if active_jobs.len() < self.config.max_concurrency {
                            if let Ok(Some(job)) = self.dequeue_and_process().await {
                                active_jobs.push(job.id.clone());
                            }
                        }
                    }
                }

                // Fallback polling (Redis reliable delivery mechanism)
                _ = fallback_poll_ticker.tick() => {
                    debug!("Fallback poll: checking for pending jobs");

                    // Clean up completed jobs from active list
                    self.cleanup_completed_jobs(&mut active_jobs).await;

                    // Dequeue up to capacity
                    while active_jobs.len() < self.config.max_concurrency {
                        match self.dequeue_and_process().await {
                            Ok(Some(job)) => {
                                active_jobs.push(job.id.clone());
                            }
                            Ok(None) => {
                                // No more jobs available
                                break;
                            }
                            Err(e) => {
                                warn!(error = %e, "Failed to dequeue job");
                                break;
                            }
                        }
                    }
                }

                // Send heartbeat
                _ = heartbeat_ticker.tick() => {
                    if let Err(e) = self.send_heartbeat(active_jobs.len()).await {
                        error!(error = %e, "Failed to send heartbeat");
                    }
                }
            }
        }

        Ok(())
    }

    /// Register worker in database
    ///
    /// Changed queue_name to queue_names (array) to match schema
    async fn register_worker(&self) -> Result<()> {
        // Check if queue is enabled before registration
        let queue_enabled: Option<(bool,)> = sqlx::query_as(
            "SELECT enabled FROM queue_config WHERE organization_id = $1 AND queue_name = $2",
        )
        .bind(&self.config.organization_id)
        .bind(&self.config.queue_name)
        .fetch_optional(&*self.db)
        .await?;

        if let Some((enabled,)) = queue_enabled {
            if !enabled {
                warn!(
                    queue = %self.config.queue_name,
                    org_id = %self.config.organization_id,
                    "Registering worker for paused queue"
                );
            }
        }

        // Use queue_names (array) instead of queue_name (string)
        let result = sqlx::query(
            r#"
            INSERT INTO workers (
                id, organization_id, queue_names, hostname, max_concurrent_jobs,
                current_job_count, status, last_heartbeat, metadata, created_at, updated_at
            )
            VALUES ($1, $2, $3, $4, $5, 0, 'healthy', NOW(), '{}', NOW(), NOW())
            ON CONFLICT (id) DO UPDATE SET
                status = 'healthy',
                last_heartbeat = NOW(),
                updated_at = NOW()
            WHERE workers.organization_id = EXCLUDED.organization_id
            "#,
        )
        .bind(&self.config.worker_id)
        .bind(&self.config.organization_id)
        .bind(vec![&self.config.queue_name]) // Convert to array
        .bind(hostname::get()?.to_string_lossy().to_string())
        .bind(self.config.max_concurrency as i32)
        .execute(&*self.db)
        .await?;

        // Check if registration succeeded
        if result.rows_affected() == 0 {
            return Err(anyhow::anyhow!(
                "Worker ID already exists for a different organization"
            ));
        }

        Ok(())
    }

    /// Dequeue next job and spawn processing task
    #[instrument(
        name = "worker.dequeue_and_process",
        skip(self),
        fields(
            worker_id = %self.config.worker_id,
            queue_name = %self.config.queue_name
        )
    )]
    async fn dequeue_and_process(&self) -> Result<Option<Job>> {
        let job = self
            .queue_manager
            .dequeue(
                &self.config.organization_id,
                &self.config.queue_name,
                &self.config.worker_id,
                self.config.lease_duration_secs,
            )
            .await?;

        if let Some(job) = job {
            info!(job_id = %job.id, queue = %self.config.queue_name, "Job dequeued");

            // Spawn async task to process job (non-blocking)
            let queue_manager = self.queue_manager.clone();
            let job_id = job.id.clone();
            let organization_id = job.organization_id.clone();
            let timeout_seconds = job.timeout_seconds as u64;

            tokio::spawn(async move {
                if let Err(e) =
                    process_job(&queue_manager, &job_id, &organization_id, timeout_seconds).await
                {
                    error!(job_id = %job_id, organization_id = %organization_id, error = %e, "Job processing failed");
                }
            });

            Ok(Some(job))
        } else {
            Ok(None)
        }
    }

    /// Check if job is still processing
    ///
    /// Previously this could check job status for ANY organization's jobs.
    async fn is_job_still_processing(&self, job_id: &str) -> Result<bool> {
        // Include organization_id in query to prevent cross-tenant access
        let result: Option<(String,)> =
            sqlx::query_as("SELECT status FROM jobs WHERE id = $1 AND organization_id = $2")
                .bind(job_id)
                .bind(&self.config.organization_id)
                .fetch_optional(&*self.db)
                .await?;

        Ok(result.map(|(s,)| s == "processing").unwrap_or(false))
    }

    /// Clean up completed jobs from active list
    async fn cleanup_completed_jobs(&self, active_jobs: &mut Vec<String>) {
        let mut completed = Vec::new();

        for job_id in active_jobs.iter() {
            match self.is_job_still_processing(job_id).await {
                Ok(true) => {} // Still processing
                _ => completed.push(job_id.clone()),
            }
        }

        for job_id in completed {
            active_jobs.retain(|id| id != &job_id);
        }
    }

    /// Send heartbeat to renew lease and report status
    ///
    async fn send_heartbeat(&self, active_job_count: usize) -> Result<()> {
        let result = sqlx::query(
            r#"
            UPDATE workers 
            SET 
                last_heartbeat = NOW(),
                current_jobs = $1,
                status = CASE 
                    WHEN $1 > max_concurrency THEN 'degraded'
                    ELSE 'healthy'
                END
            WHERE id = $2 AND organization_id = $3
            "#,
        )
        .bind(active_job_count as i32)
        .bind(&self.config.worker_id)
        .bind(&self.config.organization_id) // Validate org ownership
        .execute(&*self.db)
        .await?;

        if result.rows_affected() == 0 {
            warn!(
                worker_id = %self.config.worker_id,
                org_id = %self.config.organization_id,
                "Heartbeat failed - worker not found or org mismatch"
            );
        } else {
            debug!(
                worker_id = %self.config.worker_id,
                active_jobs = active_job_count,
                "Heartbeat sent"
            );
        }

        Ok(())
    }

    /// Graceful shutdown: wait for active jobs to complete
    ///
    /// All queries now include organization_id to prevent cross-tenant updates
    async fn shutdown_gracefully(&self, active_jobs: &[String]) -> Result<()> {
        info!("Initiating graceful shutdown");

        // Mark worker as draining (with org check)
        sqlx::query(
            "UPDATE workers SET status = 'draining' WHERE id = $1 AND organization_id = $2",
        )
        .bind(&self.config.worker_id)
        .bind(&self.config.organization_id)
        .execute(&*self.db)
        .await?;

        // Wait for active jobs with timeout (max 2 minutes)
        let shutdown_deadline = Utc::now() + chrono::Duration::seconds(120);

        for job_id in active_jobs {
            while Utc::now() < shutdown_deadline {
                if !self.is_job_still_processing(job_id).await.unwrap_or(false) {
                    break;
                }
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        }

        // Release any remaining leases (with org check to prevent cross-tenant release)
        sqlx::query(
            r#"
            UPDATE jobs 
            SET 
                status = 'pending',
                assigned_worker_id = NULL,
                lease_id = NULL,
                lease_expires_at = NULL
            WHERE assigned_worker_id = $1 
              AND organization_id = $2
              AND status = 'processing'
            "#,
        )
        .bind(&self.config.worker_id)
        .bind(&self.config.organization_id)
        .execute(&*self.db)
        .await?;

        // Mark worker as offline (with org check)
        sqlx::query("UPDATE workers SET status = 'offline' WHERE id = $1 AND organization_id = $2")
            .bind(&self.config.worker_id)
            .bind(&self.config.organization_id)
            .execute(&*self.db)
            .await?;

        info!("Worker shutdown complete");
        Ok(())
    }
}

/// Minimum timeout to prevent immediate job failures
const MIN_TIMEOUT_SECONDS: u64 = 1;
/// Maximum timeout (24 hours)
const MAX_TIMEOUT_SECONDS: u64 = 86400;

/// Process a single job with timeout and error handling
///
/// Ensures timeout is at least 1 second to prevent immediate failures.
/// Now requires organization_id to properly validate job ownership on failure.
#[instrument(
    name = "worker.process_job",
    skip(queue_manager),
    fields(
        job_id = %job_id,
        org_id = %organization_id,
        timeout_secs = %timeout_seconds
    )
)]
async fn process_job(
    queue_manager: &QueueManager,
    job_id: &str,
    organization_id: &str,
    timeout_seconds: u64,
) -> Result<()> {
    // Clamp timeout to valid range
    let safe_timeout = timeout_seconds.clamp(MIN_TIMEOUT_SECONDS, MAX_TIMEOUT_SECONDS);

    if timeout_seconds != safe_timeout {
        warn!(
            job_id = %job_id,
            original = timeout_seconds,
            adjusted = safe_timeout,
            "Adjusted timeout to safe range"
        );
    }

    let timeout_duration = Duration::from_secs(safe_timeout);

    let result = tokio::time::timeout(timeout_duration, execute_job_handler(job_id)).await;

    match result {
        Ok(Ok(job_result)) => {
            // Success: mark job as completed
            queue_manager.complete(job_id, Some(job_result)).await?;
        }
        Ok(Err(e)) => {
            // Handler error: retry or dead-letter (with org isolation)
            queue_manager
                .fail(job_id, organization_id, &e.to_string())
                .await?;
        }
        Err(_) => {
            // Timeout: retry or dead-letter (with org isolation)
            queue_manager
                .fail(
                    job_id,
                    organization_id,
                    &format!("Job timeout after {}s", timeout_seconds),
                )
                .await?;
        }
    }

    Ok(())
}

/// Built-in job handler for the embedded worker.
///
/// **IMPORTANT**: This built-in worker is primarily for development and testing.
/// In production deployments, jobs should be processed by external workers that:
/// 1. Connect via the gRPC API (`RegisterWorker`, `DequeueJobs`, `CompleteJob`)
/// 2. Use one of the official SDKs (Node.js, Python, Go)
/// 3. Implement their own business logic for each job type
///
/// The Spooled backend is a **job queue**, not a job executor. It manages:
/// - Job enqueueing, prioritization, and scheduling
/// - Worker registration and health monitoring
/// - Retry logic, dead-letter queues, and job history
///
/// Your application's workers dequeue jobs and execute the actual business logic
/// (HTTP calls, database updates, email sending, etc.) based on the job payload.
///
/// This built-in handler simply marks jobs as completed for testing purposes.
async fn execute_job_handler(job_id: &str) -> Result<serde_json::Value> {
    info!(job_id = %job_id, "Built-in worker executing job (for testing only)");

    // Simulate processing time
    tokio::time::sleep(Duration::from_millis(100)).await;

    Ok(serde_json::json!({
        "status": "success",
        "message": "Processed by built-in worker (use external workers in production)",
        "completed_at": Utc::now().to_rfc3339()
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_worker_config() {
        let config = WorkerConfig {
            worker_id: "test-worker".to_string(),
            organization_id: "test-org".to_string(),
            queue_name: "default".to_string(),
            max_concurrency: 5,
            heartbeat_interval_secs: 10,
            lease_duration_secs: 30,
            fallback_poll_interval_secs: 5,
        };

        assert_eq!(config.max_concurrency, 5);
        assert_eq!(config.queue_name, "default");
    }
}
