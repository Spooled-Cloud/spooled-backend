//! QueueService gRPC implementation
//!
//! Implements all queue operations: Enqueue, Dequeue, Complete, Fail,
//! RenewLease, GetJob, GetQueueStats, StreamJobs, and ProcessJobs.

// tonic::Status is a standard gRPC error type, size is acceptable
#![allow(clippy::result_large_err)]

use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use futures::Stream;
use sqlx::PgPool;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status, Streaming};
use tracing::{debug, error, info};

use crate::api::middleware::limits::{
    check_job_limits_generic, check_payload_size_generic, increment_daily_jobs, LimitCheckError,
};
use crate::cache::RedisCache;
use crate::config::PlanLimits;
use crate::grpc::auth::{authenticate_from_metadata, authenticate_request};
use crate::grpc::convert::{
    datetime_to_timestamp, job_to_proto, jobs_to_proto, struct_to_json_opt,
    timestamp_to_datetime_opt,
};
use crate::grpc::proto::{
    queue_service_server::QueueService, CompleteRequest, CompleteResponse, DequeueRequest,
    DequeueResponse, EnqueueRequest, EnqueueResponse, FailRequest, FailResponse, GetJobRequest,
    GetJobResponse, GetQueueStatsRequest, GetQueueStatsResponse, Job, ProcessRequest,
    ProcessResponse, RenewLeaseRequest, RenewLeaseResponse, StreamJobsRequest,
};
use crate::models::Job as DbJob;
use crate::observability::Metrics;

/// Maximum payload size in bytes (1MB)
const MAX_PAYLOAD_SIZE: usize = 1024 * 1024;

/// Maximum result size in bytes (1MB)
const MAX_RESULT_SIZE: usize = 1024 * 1024;

/// Maximum error message length
const MAX_ERROR_MESSAGE_LENGTH: usize = 4096;

/// Minimum lease duration in seconds
const MIN_LEASE_DURATION_SECS: i32 = 5;

/// Maximum lease duration in seconds (1 hour)
const MAX_LEASE_DURATION_SECS: i32 = 3600;

/// Default lease duration in seconds (5 minutes)
const DEFAULT_LEASE_DURATION_SECS: i32 = 300;

/// Maximum batch size for dequeue
const MAX_BATCH_SIZE: i32 = 100;

/// Poll interval for streaming jobs (milliseconds)
const STREAM_POLL_INTERVAL_MS: u64 = 1000;

/// QueueService implementation
#[derive(Clone)]
pub struct QueueServiceImpl {
    pool: Arc<PgPool>,
    metrics: Arc<Metrics>,
    cache: Option<RedisCache>,
}

impl QueueServiceImpl {
    pub fn new(pool: Arc<PgPool>, metrics: Arc<Metrics>, cache: Option<RedisCache>) -> Self {
        Self { pool, metrics, cache }
    }

    async fn enforce_rate_limits(
        &self,
        org_id: &str,
        api_key_id: &str,
        api_key_per_minute: Option<i32>,
    ) -> Result<(), Status> {
        let Some(ref cache) = self.cache else {
            // Redis is optional; if absent we can't reliably enforce distributed rate limits.
            return Ok(());
        };

        #[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
        struct OrgRateLimits {
            rps: u32,
            burst: u32,
        }

        let cache_key = format!("org_rate_limits:{}", org_id);
        let org_limits: OrgRateLimits = match cache.get_json(&cache_key).await {
            Ok(Some(v)) => v,
            _ => {
                let (plan_tier, custom_limits): (String, Option<serde_json::Value>) = sqlx::query_as(
                    "SELECT plan_tier, custom_limits FROM organizations WHERE id = $1",
                )
                .bind(org_id)
                .fetch_one(self.pool.as_ref())
                .await
                .map_err(|e| {
                    error!(
                        error = %e,
                        org_id = %org_id,
                        "Failed to fetch org plan for gRPC rate limiting"
                    );
                    Status::internal("Rate limiting unavailable")
                })?;

                let limits = PlanLimits::for_tier_with_overrides(&plan_tier, custom_limits.as_ref());
                let v = OrgRateLimits {
                    rps: limits.rate_limit_requests_per_second.max(1),
                    burst: limits.rate_limit_burst.max(1),
                };
                let _ = cache.set_json(&cache_key, &v, 60).await;
                v
            }
        };

        // Org token bucket (plan rps/burst)
        let bucket_key = format!("rate_limit:grpc:org:{}", org_id);
        let result = cache
            .check_token_bucket(&bucket_key, org_limits.rps, org_limits.burst)
            .await
            .map_err(|e| {
                error!(error = %e, org_id = %org_id, "Redis error during gRPC org rate limit");
                Status::internal("Rate limiting unavailable")
            })?;

        if !result.allowed {
            return Err(Status::resource_exhausted("Rate limit exceeded"));
        }

        // Optional per-key override (requests per minute)
        if let Some(per_min) = api_key_per_minute {
            let per_min = per_min.max(1) as u32;
            let key = format!("rate_limit:grpc:api_key:{}", api_key_id);
            let rl = cache.check_rate_limit(&key, per_min, 60).await.map_err(|e| {
                error!(
                    error = %e,
                    api_key_id = %api_key_id,
                    "Redis error during gRPC api-key rate limit"
                );
                Status::internal("Rate limiting unavailable")
            })?;
            if !rl.allowed {
                return Err(Status::resource_exhausted("Rate limit exceeded"));
            }
        }

        Ok(())
    }

    /// Validate queue name format
    fn validate_queue_name(name: &str) -> Result<(), Status> {
        if name.is_empty() || name.len() > 255 {
            return Err(Status::invalid_argument(
                "Queue name must be 1-255 characters",
            ));
        }
        if !name
            .chars()
            .all(|c| c.is_alphanumeric() || c == '-' || c == '_' || c == '.')
        {
            return Err(Status::invalid_argument(
                "Queue name can only contain alphanumeric characters, dashes, underscores, and dots",
            ));
        }
        if name.starts_with('.') || name.starts_with('-') {
            return Err(Status::invalid_argument(
                "Queue name cannot start with dots or dashes",
            ));
        }
        Ok(())
    }

    /// Validate and clamp lease duration
    fn safe_lease_duration(secs: i32) -> i32 {
        if secs <= 0 {
            DEFAULT_LEASE_DURATION_SECS
        } else {
            secs.clamp(MIN_LEASE_DURATION_SECS, MAX_LEASE_DURATION_SECS)
        }
    }

    /// Claim a single job from the queue
    async fn claim_job(
        &self,
        org_id: &str,
        queue_name: &str,
        worker_id: &str,
        lease_duration_secs: i32,
    ) -> Result<Option<DbJob>, Status> {
        let lease_id = uuid::Uuid::new_v4().to_string();
        let lease_duration = Self::safe_lease_duration(lease_duration_secs);

        let job: Option<DbJob> = sqlx::query_as(
            r#"
            UPDATE jobs
            SET 
                status = 'processing',
                started_at = COALESCE(started_at, NOW()),
                assigned_worker_id = $1,
                lease_id = $2,
                lease_expires_at = NOW() + ($3 || ' seconds')::INTERVAL,
                updated_at = NOW()
            WHERE id = (
                SELECT id FROM jobs
                WHERE organization_id = $4
                  AND queue_name = $5
                  AND status IN ('pending', 'scheduled')
                  AND (scheduled_at IS NULL OR scheduled_at <= NOW())
                  AND (expires_at IS NULL OR expires_at > NOW())
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
        .bind(lease_duration)
        .bind(org_id)
        .bind(queue_name)
        .fetch_optional(self.pool.as_ref())
        .await
        .map_err(|e| {
            error!(error = %e, "Failed to claim job");
            Status::internal("Failed to claim job")
        })?;

        Ok(job)
    }
}

#[tonic::async_trait]
impl QueueService for QueueServiceImpl {
    /// Enqueue a new job
    async fn enqueue(
        &self,
        request: Request<EnqueueRequest>,
    ) -> Result<Response<EnqueueResponse>, Status> {
        // Authenticate
        let auth = authenticate_request(self.pool.as_ref(), &request).await?;
        // Enforce plan-based rate limits (org + optional per-key override)
        self.enforce_rate_limits(&auth.organization_id, &auth.api_key_id, auth.rate_limit)
            .await?;
        let req = request.into_inner();

        // Validate queue name
        Self::validate_queue_name(&req.queue_name)?;

        // Validate payload size
        let payload = struct_to_json_opt(req.payload.as_ref());
        let payload_str = serde_json::to_string(&payload).unwrap_or_default();
        if payload_str.len() > MAX_PAYLOAD_SIZE {
            return Err(Status::invalid_argument(format!(
                "Payload too large: {} bytes (max: {} bytes)",
                payload_str.len(),
                MAX_PAYLOAD_SIZE
            )));
        }

        // Enforce plan-specific payload limits (e.g. Free: 64KB, Starter: 256KB)
        check_payload_size_generic(self.pool.as_ref(), &auth.organization_id, payload_str.len())
            .await
            .map_err(|e| match e {
                LimitCheckError::Database(msg) => {
                    error!(error = %msg, "Failed to check payload size");
                    Status::internal("Failed to check payload size")
                }
                LimitCheckError::PayloadTooLarge { .. } => Status::resource_exhausted(e.to_string()),
                // Not expected for payload checks, but map defensively
                LimitCheckError::LimitExceeded(err) => Status::resource_exhausted(err.to_string()),
            })?;

        // Check job limits (daily + active) before creating
        check_job_limits_generic(self.pool.as_ref(), &auth.organization_id, 1)
            .await
            .map_err(|e| match e {
                LimitCheckError::Database(msg) => {
                    error!(error = %msg, "Failed to check job limits");
                    Status::internal("Failed to check job limits")
                }
                LimitCheckError::LimitExceeded(err) => Status::resource_exhausted(err.to_string()),
                LimitCheckError::PayloadTooLarge { .. } => {
                    // Not expected for job count checks
                    Status::internal("Limit check error")
                }
            })?;

        let job_id = uuid::Uuid::new_v4().to_string();
        let now = Utc::now();
        let scheduled_at = timestamp_to_datetime_opt(req.scheduled_at);

        let status = if scheduled_at.is_some_and(|t| t > now) {
            "scheduled"
        } else {
            "pending"
        };

        // Convert tags from map to JSON
        let tags_json: serde_json::Value = req
            .tags
            .iter()
            .map(|(k, v)| (k.clone(), serde_json::Value::String(v.clone())))
            .collect();

        // Insert job with idempotency support
        let result = sqlx::query(
            r#"
            INSERT INTO jobs (
                id, organization_id, queue_name, status, payload, priority,
                max_retries, timeout_seconds, scheduled_at, idempotency_key,
                tags, created_at, updated_at
            )
            VALUES ($1, $2, $3, $4, $5::JSONB, $6, $7, $8, $9, $10, $11::JSONB, $12, $12)
            ON CONFLICT (organization_id, idempotency_key) WHERE idempotency_key IS NOT NULL
            DO UPDATE SET updated_at = EXCLUDED.updated_at
            RETURNING id, (xmax = 0) as created
            "#,
        )
        .bind(&job_id)
        .bind(&auth.organization_id)
        .bind(&req.queue_name)
        .bind(status)
        .bind(&payload)
        .bind(req.priority)
        .bind(req.max_retries.max(0))
        .bind(req.timeout_seconds.clamp(1, 86400))
        .bind(scheduled_at)
        .bind(if req.idempotency_key.is_empty() {
            None
        } else {
            Some(&req.idempotency_key)
        })
        .bind(&tags_json)
        .bind(now)
        .fetch_one(self.pool.as_ref())
        .await
        .map_err(|e| {
            error!(error = %e, "Failed to enqueue job");
            Status::internal("Failed to enqueue job")
        })?;

        use sqlx::Row;
        let returned_id: String = result.get("id");
        let created: bool = result.get("created");

        // Update metrics and usage counters
        if created {
            self.metrics.jobs_enqueued.inc();

            // Increment daily job counter for plan limit tracking
            if let Err(e) = increment_daily_jobs(self.pool.as_ref(), &auth.organization_id, 1).await
            {
                tracing::warn!(error = %e, org_id = %auth.organization_id, "Failed to increment daily job counter");
            }
        }

        info!(
            job_id = %returned_id,
            queue = %req.queue_name,
            org_id = %auth.organization_id,
            created = created,
            "Job enqueued via gRPC"
        );

        Ok(Response::new(EnqueueResponse {
            job_id: returned_id,
            created,
        }))
    }

    /// Dequeue jobs for processing
    async fn dequeue(
        &self,
        request: Request<DequeueRequest>,
    ) -> Result<Response<DequeueResponse>, Status> {
        let auth = authenticate_request(self.pool.as_ref(), &request).await?;
        self.enforce_rate_limits(&auth.organization_id, &auth.api_key_id, auth.rate_limit)
            .await?;
        let req = request.into_inner();

        Self::validate_queue_name(&req.queue_name)?;

        if req.worker_id.is_empty() {
            return Err(Status::invalid_argument("Worker ID is required"));
        }

        let batch_size = req.batch_size.clamp(1, MAX_BATCH_SIZE);
        let lease_duration = Self::safe_lease_duration(req.lease_duration_secs);

        let mut jobs = Vec::with_capacity(batch_size as usize);

        // Claim jobs one by one (atomic operations)
        for _ in 0..batch_size {
            match self
                .claim_job(
                    &auth.organization_id,
                    &req.queue_name,
                    &req.worker_id,
                    lease_duration,
                )
                .await?
            {
                Some(job) => {
                    self.metrics.jobs_processing.inc();
                    jobs.push(job);
                }
                None => break, // No more jobs available
            }
        }

        debug!(
            count = jobs.len(),
            queue = %req.queue_name,
            worker = %req.worker_id,
            "Dequeued jobs via gRPC"
        );

        Ok(Response::new(DequeueResponse {
            jobs: jobs_to_proto(&jobs),
        }))
    }

    /// Complete a job successfully
    async fn complete(
        &self,
        request: Request<CompleteRequest>,
    ) -> Result<Response<CompleteResponse>, Status> {
        let auth = authenticate_request(self.pool.as_ref(), &request).await?;
        self.enforce_rate_limits(&auth.organization_id, &auth.api_key_id, auth.rate_limit)
            .await?;
        let req = request.into_inner();

        if req.job_id.is_empty() {
            return Err(Status::invalid_argument("Job ID is required"));
        }
        if req.worker_id.is_empty() {
            return Err(Status::invalid_argument("Worker ID is required"));
        }

        // Validate result size
        let result_json = struct_to_json_opt(req.result.as_ref());
        let result_str = serde_json::to_string(&result_json).unwrap_or_default();
        if result_str.len() > MAX_RESULT_SIZE {
            return Err(Status::invalid_argument(format!(
                "Result too large: {} bytes (max: {} bytes)",
                result_str.len(),
                MAX_RESULT_SIZE
            )));
        }

        let db_result = sqlx::query(
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
        .bind(&result_json)
        .bind(&req.job_id)
        .bind(&req.worker_id)
        .bind(&auth.organization_id)
        .execute(self.pool.as_ref())
        .await
        .map_err(|e| {
            error!(error = %e, "Failed to complete job");
            Status::internal("Failed to complete job")
        })?;

        if db_result.rows_affected() == 0 {
            return Err(Status::not_found("Job not found or not authorized"));
        }

        self.metrics.jobs_completed.inc();
        self.metrics.jobs_processing.dec();

        info!(job_id = %req.job_id, "Job completed via gRPC");

        Ok(Response::new(CompleteResponse { success: true }))
    }

    /// Fail a job (triggers retry or DLQ)
    async fn fail(&self, request: Request<FailRequest>) -> Result<Response<FailResponse>, Status> {
        let auth = authenticate_request(self.pool.as_ref(), &request).await?;
        self.enforce_rate_limits(&auth.organization_id, &auth.api_key_id, auth.rate_limit)
            .await?;
        let req = request.into_inner();

        if req.job_id.is_empty() {
            return Err(Status::invalid_argument("Job ID is required"));
        }
        if req.worker_id.is_empty() {
            return Err(Status::invalid_argument("Worker ID is required"));
        }

        // Truncate error message if too long
        let error_message = if req.error.len() > MAX_ERROR_MESSAGE_LENGTH {
            format!(
                "{}... [truncated]",
                &req.error[..MAX_ERROR_MESSAGE_LENGTH - 15]
            )
        } else {
            req.error.clone()
        };

        #[derive(sqlx::FromRow)]
        struct UpdateResult {
            new_status: String,
            new_retry_count: i32,
            retry_delay_secs: Option<i64>,
        }

        let result: Option<UpdateResult> = sqlx::query_as(
            r#"
            UPDATE jobs
            SET 
                status = CASE 
                    WHEN $1 = FALSE OR retry_count >= max_retries THEN 'deadletter'
                    ELSE 'pending'
                END,
                last_error = $2,
                retry_count = retry_count + 1,
                scheduled_at = CASE 
                    WHEN $1 = TRUE AND retry_count < max_retries 
                    THEN NOW() + (POWER(2, retry_count) || ' minutes')::INTERVAL
                    ELSE scheduled_at
                END,
                assigned_worker_id = NULL,
                lease_id = NULL,
                lease_expires_at = NULL,
                updated_at = NOW()
            WHERE id = $3 
              AND assigned_worker_id = $4
              AND organization_id = $5
              AND status = 'processing'
            RETURNING 
                status as new_status,
                retry_count as new_retry_count,
                CASE 
                    WHEN status = 'pending' 
                    THEN EXTRACT(EPOCH FROM (scheduled_at - NOW()))::BIGINT
                    ELSE NULL
                END as retry_delay_secs
            "#,
        )
        .bind(req.retry)
        .bind(&error_message)
        .bind(&req.job_id)
        .bind(&req.worker_id)
        .bind(&auth.organization_id)
        .fetch_optional(self.pool.as_ref())
        .await
        .map_err(|e| {
            error!(error = %e, "Failed to fail job");
            Status::internal("Failed to update job")
        })?;

        let result = result.ok_or_else(|| Status::not_found("Job not found or not authorized"))?;

        self.metrics.jobs_failed.inc();
        self.metrics.jobs_processing.dec();

        let will_retry = result.new_status == "pending";
        if result.new_status == "deadletter" {
            self.metrics.jobs_deadlettered.inc();
        }

        info!(
            job_id = %req.job_id,
            will_retry = will_retry,
            new_status = %result.new_status,
            "Job failed via gRPC"
        );

        Ok(Response::new(FailResponse {
            success: true,
            will_retry,
            next_retry_delay_secs: result.retry_delay_secs.unwrap_or(0) as i32,
        }))
    }

    /// Renew job lease
    async fn renew_lease(
        &self,
        request: Request<RenewLeaseRequest>,
    ) -> Result<Response<RenewLeaseResponse>, Status> {
        let auth = authenticate_request(self.pool.as_ref(), &request).await?;
        self.enforce_rate_limits(&auth.organization_id, &auth.api_key_id, auth.rate_limit)
            .await?;
        let req = request.into_inner();

        if req.job_id.is_empty() {
            return Err(Status::invalid_argument("Job ID is required"));
        }
        if req.worker_id.is_empty() {
            return Err(Status::invalid_argument("Worker ID is required"));
        }

        let extension_secs = Self::safe_lease_duration(req.extension_secs);
        let new_expires_at = Utc::now() + chrono::Duration::seconds(extension_secs as i64);

        let result = sqlx::query(
            r#"
            UPDATE jobs
            SET lease_expires_at = $1, updated_at = NOW()
            WHERE id = $2 
              AND assigned_worker_id = $3 
              AND organization_id = $4 
              AND status = 'processing'
            "#,
        )
        .bind(new_expires_at)
        .bind(&req.job_id)
        .bind(&req.worker_id)
        .bind(&auth.organization_id)
        .execute(self.pool.as_ref())
        .await
        .map_err(|e| {
            error!(error = %e, "Failed to renew lease");
            Status::internal("Failed to renew lease")
        })?;

        let success = result.rows_affected() > 0;

        debug!(
            job_id = %req.job_id,
            success = success,
            "Lease renewed via gRPC"
        );

        Ok(Response::new(RenewLeaseResponse {
            success,
            new_expires_at: if success {
                Some(datetime_to_timestamp(new_expires_at))
            } else {
                None
            },
        }))
    }

    /// Get job by ID
    async fn get_job(
        &self,
        request: Request<GetJobRequest>,
    ) -> Result<Response<GetJobResponse>, Status> {
        let auth = authenticate_request(self.pool.as_ref(), &request).await?;
        self.enforce_rate_limits(&auth.organization_id, &auth.api_key_id, auth.rate_limit)
            .await?;
        let req = request.into_inner();

        if req.job_id.is_empty() {
            return Err(Status::invalid_argument("Job ID is required"));
        }

        let job: Option<DbJob> =
            sqlx::query_as("SELECT * FROM jobs WHERE id = $1 AND organization_id = $2")
                .bind(&req.job_id)
                .bind(&auth.organization_id)
                .fetch_optional(self.pool.as_ref())
                .await
                .map_err(|e| {
                    error!(error = %e, "Failed to get job");
                    Status::internal("Failed to get job")
                })?;

        Ok(Response::new(GetJobResponse {
            job: job.as_ref().map(job_to_proto),
        }))
    }

    /// Get queue statistics
    async fn get_queue_stats(
        &self,
        request: Request<GetQueueStatsRequest>,
    ) -> Result<Response<GetQueueStatsResponse>, Status> {
        let auth = authenticate_request(self.pool.as_ref(), &request).await?;
        self.enforce_rate_limits(&auth.organization_id, &auth.api_key_id, auth.rate_limit)
            .await?;
        let req = request.into_inner();

        Self::validate_queue_name(&req.queue_name)?;

        let stats: (i64, i64, i64, i64, i64, i64, i64, Option<i64>) = sqlx::query_as(
            r#"
            SELECT 
                COUNT(*) FILTER (WHERE status = 'pending') as pending,
                COUNT(*) FILTER (WHERE status = 'scheduled') as scheduled,
                COUNT(*) FILTER (WHERE status = 'processing') as processing,
                COUNT(*) FILTER (WHERE status = 'completed') as completed,
                COUNT(*) FILTER (WHERE status = 'failed') as failed,
                COUNT(*) FILTER (WHERE status = 'deadletter') as deadletter,
                COUNT(*) as total,
                EXTRACT(EPOCH FROM (NOW() - MIN(created_at) FILTER (WHERE status = 'pending')))::BIGINT * 1000 as max_age_ms
            FROM jobs
            WHERE queue_name = $1 AND organization_id = $2
            "#,
        )
        .bind(&req.queue_name)
        .bind(&auth.organization_id)
        .fetch_one(self.pool.as_ref())
        .await
        .map_err(|e| {
            error!(error = %e, "Failed to get queue stats");
            Status::internal("Failed to get queue stats")
        })?;

        Ok(Response::new(GetQueueStatsResponse {
            queue_name: req.queue_name,
            pending: stats.0,
            scheduled: stats.1,
            processing: stats.2,
            completed: stats.3,
            failed: stats.4,
            deadletter: stats.5,
            total: stats.6,
            max_age_ms: stats.7.unwrap_or(0),
        }))
    }

    /// Stream jobs to worker (server-side streaming)
    type StreamJobsStream = Pin<Box<dyn Stream<Item = Result<Job, Status>> + Send>>;

    async fn stream_jobs(
        &self,
        request: Request<StreamJobsRequest>,
    ) -> Result<Response<Self::StreamJobsStream>, Status> {
        let auth = authenticate_request(self.pool.as_ref(), &request).await?;
        self.enforce_rate_limits(&auth.organization_id, &auth.api_key_id, auth.rate_limit)
            .await?;
        let req = request.into_inner();

        Self::validate_queue_name(&req.queue_name)?;

        if req.worker_id.is_empty() {
            return Err(Status::invalid_argument("Worker ID is required"));
        }

        let lease_duration = Self::safe_lease_duration(req.lease_duration_secs);
        let pool = self.pool.clone();
        let metrics = self.metrics.clone();
        let org_id = auth.organization_id.clone();
        let queue_name = req.queue_name.clone();
        let worker_id = req.worker_id.clone();

        let (tx, rx) = mpsc::channel(16);

        // Spawn background task to poll for jobs
        tokio::spawn(async move {
            loop {
                // Check if receiver is closed
                if tx.is_closed() {
                    debug!(worker = %worker_id, "Stream closed by client");
                    break;
                }

                // Try to claim a job
                let lease_id = uuid::Uuid::new_v4().to_string();
                let job_result: Result<Option<DbJob>, _> = sqlx::query_as(
                    r#"
                    UPDATE jobs
                    SET 
                        status = 'processing',
                        assigned_worker_id = $1,
                        lease_id = $2,
                        lease_expires_at = NOW() + ($3 || ' seconds')::INTERVAL,
                        started_at = COALESCE(started_at, NOW()),
                        updated_at = NOW()
                    WHERE id = (
                        SELECT id FROM jobs
                        WHERE organization_id = $4
                          AND queue_name = $5
                          AND status IN ('pending', 'scheduled')
                          AND (scheduled_at IS NULL OR scheduled_at <= NOW())
                          AND (expires_at IS NULL OR expires_at > NOW())
                          AND (dependencies_met IS NULL OR dependencies_met = TRUE)
                        ORDER BY priority DESC, created_at ASC
                        LIMIT 1
                        FOR UPDATE SKIP LOCKED
                    )
                    RETURNING *
                    "#,
                )
                .bind(&worker_id)
                .bind(&lease_id)
                .bind(lease_duration)
                .bind(&org_id)
                .bind(&queue_name)
                .fetch_optional(pool.as_ref())
                .await;

                match job_result {
                    Ok(Some(job)) => {
                        metrics.jobs_processing.inc();
                        debug!(job_id = %job.id, queue = %queue_name, "Streaming job to worker");

                        if tx.send(Ok(job_to_proto(&job))).await.is_err() {
                            break;
                        }
                    }
                    Ok(None) => {
                        // No jobs available, wait before polling again
                        tokio::time::sleep(Duration::from_millis(STREAM_POLL_INTERVAL_MS)).await;
                    }
                    Err(e) => {
                        error!(error = %e, "Error polling for jobs in stream");
                        if tx
                            .send(Err(Status::internal("Error polling for jobs")))
                            .await
                            .is_err()
                        {
                            break;
                        }
                        tokio::time::sleep(Duration::from_millis(STREAM_POLL_INTERVAL_MS)).await;
                    }
                }
            }
        });

        let stream = ReceiverStream::new(rx);
        Ok(Response::new(Box::pin(stream)))
    }

    /// Bidirectional streaming for job processing
    type ProcessJobsStream = Pin<Box<dyn Stream<Item = Result<ProcessResponse, Status>> + Send>>;

    async fn process_jobs(
        &self,
        request: Request<Streaming<ProcessRequest>>,
    ) -> Result<Response<Self::ProcessJobsStream>, Status> {
        // Extract metadata before consuming request (Streaming is not Sync)
        let metadata = request.metadata().clone();
        let auth = authenticate_from_metadata(self.pool.as_ref(), &metadata).await?;
        let mut stream = request.into_inner();

        let pool = self.pool.clone();
        let metrics = self.metrics.clone();
        let org_id = auth.organization_id.clone();
        let api_key_id = auth.api_key_id.clone();
        let api_key_rate_limit = auth.rate_limit;
        let this = self.clone();

        let (tx, rx) = mpsc::channel(16);

        // Spawn background task to handle incoming requests
        tokio::spawn(async move {
            while let Ok(Some(req)) = stream.message().await {
                // IMPORTANT: enforce rate limits per message, otherwise the stream can bypass limits.
                if let Err(status) =
                    this.enforce_rate_limits(&org_id, &api_key_id, api_key_rate_limit).await
                {
                    let response = ProcessResponse {
                        response: Some(crate::grpc::proto::process_response::Response::Error(
                            crate::grpc::proto::ErrorResponse {
                                code: "RATE_LIMIT_EXCEEDED".to_string(),
                                message: status.to_string(),
                            },
                        )),
                    };
                    if tx.send(Ok(response)).await.is_err() {
                        break;
                    }
                    continue;
                }

                let response = match req.request {
                    Some(crate::grpc::proto::process_request::Request::Dequeue(dequeue_req)) => {
                        handle_process_dequeue(&pool, &metrics, &org_id, dequeue_req).await
                    }
                    Some(crate::grpc::proto::process_request::Request::Complete(complete_req)) => {
                        handle_process_complete(&pool, &metrics, &org_id, complete_req).await
                    }
                    Some(crate::grpc::proto::process_request::Request::Fail(fail_req)) => {
                        handle_process_fail(&pool, &metrics, &org_id, fail_req).await
                    }
                    Some(crate::grpc::proto::process_request::Request::RenewLease(renew_req)) => {
                        handle_process_renew(&pool, &org_id, renew_req).await
                    }
                    None => ProcessResponse {
                        response: Some(crate::grpc::proto::process_response::Response::Error(
                            crate::grpc::proto::ErrorResponse {
                                code: "INVALID_REQUEST".to_string(),
                                message: "Empty request".to_string(),
                            },
                        )),
                    },
                };

                if tx.send(Ok(response)).await.is_err() {
                    break;
                }
            }
        });

        let stream = ReceiverStream::new(rx);
        Ok(Response::new(Box::pin(stream)))
    }
}

/// Handle dequeue request in ProcessJobs stream
async fn handle_process_dequeue(
    pool: &PgPool,
    metrics: &Metrics,
    org_id: &str,
    req: DequeueRequest,
) -> ProcessResponse {
    let lease_id = uuid::Uuid::new_v4().to_string();
    let lease_duration = QueueServiceImpl::safe_lease_duration(req.lease_duration_secs);

    let job: Result<Option<DbJob>, _> = sqlx::query_as(
        r#"
        UPDATE jobs
        SET 
            status = 'processing',
            assigned_worker_id = $1,
            lease_id = $2,
            lease_expires_at = NOW() + ($3 || ' seconds')::INTERVAL,
            started_at = COALESCE(started_at, NOW()),
            updated_at = NOW()
        WHERE id = (
            SELECT id FROM jobs
            WHERE organization_id = $4
              AND queue_name = $5
              AND status IN ('pending', 'scheduled')
              AND (scheduled_at IS NULL OR scheduled_at <= NOW())
              AND (expires_at IS NULL OR expires_at > NOW())
              AND (dependencies_met IS NULL OR dependencies_met = TRUE)
            ORDER BY priority DESC, created_at ASC
            LIMIT 1
            FOR UPDATE SKIP LOCKED
        )
        RETURNING *
        "#,
    )
    .bind(&req.worker_id)
    .bind(&lease_id)
    .bind(lease_duration)
    .bind(org_id)
    .bind(&req.queue_name)
    .fetch_optional(pool)
    .await;

    match job {
        Ok(Some(job)) => {
            metrics.jobs_processing.inc();
            ProcessResponse {
                response: Some(crate::grpc::proto::process_response::Response::Job(
                    job_to_proto(&job),
                )),
            }
        }
        Ok(None) => ProcessResponse {
            response: Some(crate::grpc::proto::process_response::Response::Error(
                crate::grpc::proto::ErrorResponse {
                    code: "NO_JOBS".to_string(),
                    message: "No jobs available".to_string(),
                },
            )),
        },
        Err(e) => {
            error!(error = %e, "Failed to dequeue in ProcessJobs");
            ProcessResponse {
                response: Some(crate::grpc::proto::process_response::Response::Error(
                    crate::grpc::proto::ErrorResponse {
                        code: "INTERNAL".to_string(),
                        message: "Failed to dequeue job".to_string(),
                    },
                )),
            }
        }
    }
}

/// Handle complete request in ProcessJobs stream
async fn handle_process_complete(
    pool: &PgPool,
    metrics: &Metrics,
    org_id: &str,
    req: CompleteRequest,
) -> ProcessResponse {
    let result_json = struct_to_json_opt(req.result.as_ref());

    let db_result = sqlx::query(
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
    .bind(&result_json)
    .bind(&req.job_id)
    .bind(&req.worker_id)
    .bind(org_id)
    .execute(pool)
    .await;

    match db_result {
        Ok(result) if result.rows_affected() > 0 => {
            metrics.jobs_completed.inc();
            metrics.jobs_processing.dec();
            ProcessResponse {
                response: Some(crate::grpc::proto::process_response::Response::Complete(
                    CompleteResponse { success: true },
                )),
            }
        }
        Ok(_) => ProcessResponse {
            response: Some(crate::grpc::proto::process_response::Response::Error(
                crate::grpc::proto::ErrorResponse {
                    code: "NOT_FOUND".to_string(),
                    message: "Job not found or not authorized".to_string(),
                },
            )),
        },
        Err(e) => {
            error!(error = %e, "Failed to complete in ProcessJobs");
            ProcessResponse {
                response: Some(crate::grpc::proto::process_response::Response::Error(
                    crate::grpc::proto::ErrorResponse {
                        code: "INTERNAL".to_string(),
                        message: "Failed to complete job".to_string(),
                    },
                )),
            }
        }
    }
}

/// Handle fail request in ProcessJobs stream
async fn handle_process_fail(
    pool: &PgPool,
    metrics: &Metrics,
    org_id: &str,
    req: FailRequest,
) -> ProcessResponse {
    let error_message = if req.error.len() > MAX_ERROR_MESSAGE_LENGTH {
        format!(
            "{}... [truncated]",
            &req.error[..MAX_ERROR_MESSAGE_LENGTH - 15]
        )
    } else {
        req.error.clone()
    };

    #[derive(sqlx::FromRow)]
    struct UpdateResult {
        new_status: String,
        retry_delay_secs: Option<i64>,
    }

    let result: Result<Option<UpdateResult>, _> = sqlx::query_as(
        r#"
        UPDATE jobs
        SET 
            status = CASE 
                WHEN $1 = FALSE OR retry_count >= max_retries THEN 'deadletter'
                ELSE 'pending'
            END,
            last_error = $2,
            retry_count = retry_count + 1,
            scheduled_at = CASE 
                WHEN $1 = TRUE AND retry_count < max_retries 
                THEN NOW() + (POWER(2, retry_count) || ' minutes')::INTERVAL
                ELSE scheduled_at
            END,
            assigned_worker_id = NULL,
            lease_id = NULL,
            lease_expires_at = NULL,
            updated_at = NOW()
        WHERE id = $3 
          AND assigned_worker_id = $4
          AND organization_id = $5
          AND status = 'processing'
        RETURNING 
            status as new_status,
            CASE 
                WHEN status = 'pending' 
                THEN EXTRACT(EPOCH FROM (scheduled_at - NOW()))::BIGINT
                ELSE NULL
            END as retry_delay_secs
        "#,
    )
    .bind(req.retry)
    .bind(&error_message)
    .bind(&req.job_id)
    .bind(&req.worker_id)
    .bind(org_id)
    .fetch_optional(pool)
    .await;

    match result {
        Ok(Some(update_result)) => {
            metrics.jobs_failed.inc();
            metrics.jobs_processing.dec();
            let will_retry = update_result.new_status == "pending";
            if update_result.new_status == "deadletter" {
                metrics.jobs_deadlettered.inc();
            }
            ProcessResponse {
                response: Some(crate::grpc::proto::process_response::Response::Fail(
                    FailResponse {
                        success: true,
                        will_retry,
                        next_retry_delay_secs: update_result.retry_delay_secs.unwrap_or(0) as i32,
                    },
                )),
            }
        }
        Ok(None) => ProcessResponse {
            response: Some(crate::grpc::proto::process_response::Response::Error(
                crate::grpc::proto::ErrorResponse {
                    code: "NOT_FOUND".to_string(),
                    message: "Job not found or not authorized".to_string(),
                },
            )),
        },
        Err(e) => {
            error!(error = %e, "Failed to fail in ProcessJobs");
            ProcessResponse {
                response: Some(crate::grpc::proto::process_response::Response::Error(
                    crate::grpc::proto::ErrorResponse {
                        code: "INTERNAL".to_string(),
                        message: "Failed to update job".to_string(),
                    },
                )),
            }
        }
    }
}

/// Handle renew lease request in ProcessJobs stream
async fn handle_process_renew(
    pool: &PgPool,
    org_id: &str,
    req: RenewLeaseRequest,
) -> ProcessResponse {
    let extension_secs = QueueServiceImpl::safe_lease_duration(req.extension_secs);
    let new_expires_at = Utc::now() + chrono::Duration::seconds(extension_secs as i64);

    let result = sqlx::query(
        r#"
        UPDATE jobs
        SET lease_expires_at = $1, updated_at = NOW()
        WHERE id = $2 
          AND assigned_worker_id = $3 
          AND organization_id = $4 
          AND status = 'processing'
        "#,
    )
    .bind(new_expires_at)
    .bind(&req.job_id)
    .bind(&req.worker_id)
    .bind(org_id)
    .execute(pool)
    .await;

    match result {
        Ok(res) => {
            let success = res.rows_affected() > 0;
            ProcessResponse {
                response: Some(crate::grpc::proto::process_response::Response::RenewLease(
                    RenewLeaseResponse {
                        success,
                        new_expires_at: if success {
                            Some(datetime_to_timestamp(new_expires_at))
                        } else {
                            None
                        },
                    },
                )),
            }
        }
        Err(e) => {
            error!(error = %e, "Failed to renew lease in ProcessJobs");
            ProcessResponse {
                response: Some(crate::grpc::proto::process_response::Response::Error(
                    crate::grpc::proto::ErrorResponse {
                        code: "INTERNAL".to_string(),
                        message: "Failed to renew lease".to_string(),
                    },
                )),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_validate_queue_name() {
        assert!(QueueServiceImpl::validate_queue_name("default").is_ok());
        assert!(QueueServiceImpl::validate_queue_name("my-queue").is_ok());
        assert!(QueueServiceImpl::validate_queue_name("queue_123").is_ok());
        assert!(QueueServiceImpl::validate_queue_name("prod.emails").is_ok());

        assert!(QueueServiceImpl::validate_queue_name("").is_err());
        assert!(QueueServiceImpl::validate_queue_name("-invalid").is_err());
        assert!(QueueServiceImpl::validate_queue_name(".invalid").is_err());
        assert!(QueueServiceImpl::validate_queue_name("invalid@chars").is_err());
    }

    #[test]
    fn test_safe_lease_duration() {
        assert_eq!(
            QueueServiceImpl::safe_lease_duration(0),
            DEFAULT_LEASE_DURATION_SECS
        );
        assert_eq!(
            QueueServiceImpl::safe_lease_duration(-10),
            DEFAULT_LEASE_DURATION_SECS
        );
        assert_eq!(
            QueueServiceImpl::safe_lease_duration(1),
            MIN_LEASE_DURATION_SECS
        );
        assert_eq!(QueueServiceImpl::safe_lease_duration(60), 60);
        assert_eq!(
            QueueServiceImpl::safe_lease_duration(10000),
            MAX_LEASE_DURATION_SECS
        );
    }
}
