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
use sqlx::{PgPool, Postgres, Transaction};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status, Streaming};
use tracing::{debug, error, info, warn};

use crate::api::middleware::limits::{
    check_job_limits_generic, check_payload_size_generic, daily_jobs_limit,
    try_increment_daily_jobs, LimitCheckError,
};
use crate::cache::RedisCache;
use crate::config::PlanLimits;
use crate::grpc::auth::{
    authenticate_from_metadata, authenticate_request, reload_api_key_context, GrpcAuthContext,
};
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
use crate::outgoing_webhooks::service::OutgoingWebhookService;

/// Maximum payload size in bytes (1MB)
const MAX_PAYLOAD_SIZE: usize = 1024 * 1024;

/// Maximum result size in bytes (1MB)
const MAX_RESULT_SIZE: usize = 1024 * 1024;

/// Maximum error message length
const MAX_ERROR_MESSAGE_LENGTH: usize = 4096;
const TRUNCATION_SUFFIX: &str = "... [truncated]";

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
    outgoing_webhooks: Arc<OutgoingWebhookService>,
}

impl QueueServiceImpl {
    pub fn new(
        pool: Arc<PgPool>,
        metrics: Arc<Metrics>,
        cache: Option<RedisCache>,
        outgoing_webhooks: Arc<OutgoingWebhookService>,
    ) -> Self {
        Self {
            pool,
            metrics,
            cache,
            outgoing_webhooks,
        }
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
                let (plan_tier, custom_limits): (String, Option<serde_json::Value>) =
                    sqlx::query_as(
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

                let limits =
                    PlanLimits::for_tier_with_overrides(&plan_tier, custom_limits.as_ref());
                let v = OrgRateLimits {
                    rps: limits.rate_limit_requests_per_second.max(1),
                    burst: limits.rate_limit_burst.max(1),
                };
                let _ = cache.set_json(&cache_key, &v, 60).await;
                v
            }
        };

        // Org token bucket (plan rps/burst). Fail OPEN on a transient Redis error:
        // a rate-limit backend blip must not reject a legitimate worker op
        // (rate limiting is best-effort, not a correctness gate).
        let bucket_key = format!("rate_limit:grpc:org:{}", org_id);
        match cache
            .check_token_bucket(&bucket_key, org_limits.rps, org_limits.burst)
            .await
        {
            Ok(result) if !result.allowed => {
                return Err(Status::resource_exhausted("Rate limit exceeded"));
            }
            Ok(_) => {}
            Err(e) => {
                warn!(error = %e, org_id = %org_id, "Redis error during gRPC org rate limit — allowing (fail-open)");
            }
        }

        // Optional per-key override (requests per minute). Also fail-open on a
        // transient Redis error rather than rejecting a legitimate worker op.
        if let Some(per_min) = api_key_per_minute {
            let per_min = per_min.max(1) as u32;
            let key = format!("rate_limit:grpc:api_key:{}", api_key_id);
            match cache.check_rate_limit(&key, per_min, 60).await {
                Ok(rl) if !rl.allowed => {
                    return Err(Status::resource_exhausted("Rate limit exceeded"));
                }
                Ok(_) => {}
                Err(e) => {
                    warn!(error = %e, api_key_id = %api_key_id, "Redis error during gRPC api-key rate limit — allowing (fail-open)");
                }
            }
        }

        Ok(())
    }

    fn require_queue_access(auth: &GrpcAuthContext, queue_name: &str) -> Result<(), Status> {
        if auth.can_access_queue(queue_name) {
            Ok(())
        } else {
            Err(Status::permission_denied(format!(
                "API key does not have permission for queue '{}'",
                queue_name
            )))
        }
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

    /// Proto3 strings default to "" — empty means missing. Settlement SQL
    /// requires an exact `lease_id` match (same as REST); omitting the token
    /// must not bypass fencing via `IS NULL OR`.
    fn opt_lease_id(lease_id: &str) -> Option<&str> {
        if lease_id.is_empty() {
            None
        } else {
            Some(lease_id)
        }
    }

    fn require_lease_id(lease_id: &str) -> Result<&str, Status> {
        Self::opt_lease_id(lease_id)
            .ok_or_else(|| Status::invalid_argument("lease_id is required for job settlement"))
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
                  -- Respect paused queues, matching the REST dequeue path.
                  AND NOT EXISTS (
                      SELECT 1 FROM queue_config qc
                      WHERE qc.organization_id = $4
                        AND qc.queue_name = $5
                        AND qc.enabled = false
                  )
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

        // Enforce API key queue scope (empty/`*` = all queues allowed)
        if !auth.can_access_queue(&req.queue_name) {
            return Err(Status::permission_denied(format!(
                "API key does not have permission for queue '{}'",
                req.queue_name
            )));
        }

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
                LimitCheckError::PayloadTooLarge { .. } => {
                    Status::resource_exhausted(e.to_string())
                }
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
        // proto3 int32 is 0 when the field is omitted; treat 0 as "use the
        // default" (300s, matching the REST handler) instead of clamping to a
        // 1-second budget that deadletters any job taking longer than a second.
        .bind(if req.timeout_seconds <= 0 {
            300
        } else {
            req.timeout_seconds.clamp(1, 86400)
        })
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
            // Atomically enforce + increment the daily quota, closing the check-then-
            // increment race the generic pre-check above leaves open (mirrors the REST
            // enqueue path). If over the cap, remove the job we just created and reject.
            // The enqueued metric is only bumped for jobs that are kept, so an over-cap
            // rejection doesn't inflate it.
            let limit = daily_jobs_limit(self.pool.as_ref(), &auth.organization_id)
                .await
                .unwrap_or(-1);
            match try_increment_daily_jobs(self.pool.as_ref(), &auth.organization_id, 1, limit)
                .await
            {
                Ok(count) if count >= 0 => {
                    self.metrics.jobs_enqueued.inc();
                }
                Ok(_) => {
                    let _ = sqlx::query("DELETE FROM jobs WHERE id = $1 AND organization_id = $2")
                        .bind(&returned_id)
                        .bind(&auth.organization_id)
                        .execute(self.pool.as_ref())
                        .await;
                    return Err(Status::resource_exhausted("Daily job limit reached"));
                }
                Err(e) => {
                    // Counter DB error: keep the job (best-effort) rather than failing an
                    // accepted enqueue, and still count it as enqueued.
                    self.metrics.jobs_enqueued.inc();
                    tracing::warn!(error = %e, org_id = %auth.organization_id, "Failed to increment daily job counter");
                }
            }
        }

        info!(
            job_id = %returned_id,
            queue = %req.queue_name,
            org_id = %auth.organization_id,
            created = created,
            "Job enqueued via gRPC"
        );

        if created {
            self.outgoing_webhooks.spawn_dispatch(
                auth.organization_id.clone(),
                "job.created".to_string(),
                returned_id.clone(),
                serde_json::json!({
                    "job_id": returned_id,
                    "queue_name": req.queue_name,
                    "status": "pending",
                }),
            );
        }

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

        // Enforce API key queue scope (empty/`*` = all queues allowed)
        if !auth.can_access_queue(&req.queue_name) {
            return Err(Status::permission_denied(format!(
                "API key does not have permission for queue '{}'",
                req.queue_name
            )));
        }

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

        for job in &jobs {
            self.outgoing_webhooks.spawn_dispatch(
                auth.organization_id.clone(),
                "job.started".to_string(),
                job.id.clone(),
                serde_json::json!({
                    "job_id": job.id,
                    "queue_name": job.queue_name,
                    "worker_id": req.worker_id,
                }),
            );
        }

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

        let lease_id = Self::require_lease_id(&req.lease_id)?;
        let allowed_queues = queue_scope_filter(&auth);

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

        // NOTE: this SQL is byte-identical to QueueManager::complete_by_worker's.
        // sqlx caches prepared statements per connection KEYED BY THE SQL STRING,
        // so $1 must be bound with the same wire type as the REST path (a TEXT
        // JSON string cast by ::JSONB) — binding serde_json::Value (JSONB oid)
        // here poisons whichever path prepares second on a shared pool
        // connection ("invalid input syntax for type json").
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
              AND lease_expires_at IS NOT NULL
              AND lease_expires_at > NOW()
              AND lease_id = $5
              AND ($6::TEXT[] IS NULL OR queue_name = ANY($6))
            "#,
        )
        .bind(&result_str)
        .bind(&req.job_id)
        .bind(&req.worker_id)
        .bind(&auth.organization_id)
        .bind(lease_id)
        .bind(allowed_queues)
        .execute(self.pool.as_ref())
        .await
        .map_err(|e| {
            error!(error = %e, "Failed to complete job");
            Status::internal("Failed to complete job")
        })?;

        if db_result.rows_affected() == 0 {
            return Err(classify_worker_miss_status(
                self.pool.as_ref(),
                &req.job_id,
                &req.worker_id,
                &auth.organization_id,
                Some(lease_id),
                allowed_queues,
            )
            .await);
        }

        self.metrics.jobs_completed.inc();
        self.metrics.jobs_processing.dec();

        info!(job_id = %req.job_id, "Job completed via gRPC");

        let job_data: Option<(String, Option<serde_json::Value>, Option<String>)> =
            sqlx::query_as(
                "SELECT queue_name, result, completion_webhook FROM jobs WHERE id = $1 AND organization_id = $2",
            )
            .bind(&req.job_id)
            .bind(&auth.organization_id)
            .fetch_optional(self.pool.as_ref())
            .await
            .unwrap_or(None);
        let (queue_name, result, completion_webhook) =
            job_data.unwrap_or_else(|| ("unknown".to_string(), None, None));

        self.outgoing_webhooks.spawn_dispatch(
            auth.organization_id.clone(),
            "job.completed".to_string(),
            req.job_id.clone(),
            serde_json::json!({
                "job_id": req.job_id,
                "queue_name": queue_name,
                "status": "completed",
                "result": result,
            }),
        );
        if let Some(url) = completion_webhook.filter(|u| !u.is_empty()) {
            crate::outgoing_webhooks::service::spawn_completion_webhook(
                url,
                "job.completed".to_string(),
                serde_json::json!({
                    "job_id": req.job_id,
                    "queue_name": queue_name,
                    "status": "completed",
                    "result": result,
                }),
            );
        }

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

        let lease_id = Self::require_lease_id(&req.lease_id)?;
        let allowed_queues = queue_scope_filter(&auth);
        let error_message = truncate_error_message(&req.error);

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
                    THEN NOW() + (LEAST(POWER(2, LEAST(retry_count, 6)), 60) || ' seconds')::INTERVAL
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
              AND lease_expires_at IS NOT NULL
              AND lease_expires_at > NOW()
              AND lease_id = $6
              AND ($7::TEXT[] IS NULL OR queue_name = ANY($7))
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
        .bind(lease_id)
        .bind(allowed_queues)
        .fetch_optional(self.pool.as_ref())
        .await
        .map_err(|e| {
            error!(error = %e, "Failed to fail job");
            Status::internal("Failed to update job")
        })?;

        let result = match result {
            Some(r) => r,
            None => {
                return Err(classify_worker_miss_status(
                    self.pool.as_ref(),
                    &req.job_id,
                    &req.worker_id,
                    &auth.organization_id,
                    Some(lease_id),
                    allowed_queues,
                )
                .await);
            }
        };

        self.metrics.jobs_failed.inc();
        self.metrics.jobs_processing.dec();

        let will_retry = result.new_status == "pending";
        if result.new_status == "deadletter" {
            self.metrics.jobs_deadlettered.inc();

            // Parity with REST: also insert a dead_letter_queue row so DLQ
            // listings surface failures made via gRPC. Best-effort — errors
            // here are logged but do not fail the RPC (the job status has
            // already been moved to deadletter above).
            if let Err(e) = sqlx::query(
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
            .bind(&error_message)
            .bind(serde_json::json!({
                "final_error": error_message,
                "total_retries": result.new_retry_count,
            }))
            .bind(&req.job_id)
            .bind(&auth.organization_id)
            .execute(self.pool.as_ref())
            .await
            {
                error!(error = %e, job_id = %req.job_id, "Failed to insert dead_letter_queue row via gRPC");
            }
        }

        info!(
            job_id = %req.job_id,
            will_retry = will_retry,
            new_status = %result.new_status,
            "Job failed via gRPC"
        );

        if result.new_status == "deadletter" || result.new_status == "failed" {
            let queue_name: String = sqlx::query_scalar(
                "SELECT queue_name FROM jobs WHERE id = $1 AND organization_id = $2",
            )
            .bind(&req.job_id)
            .bind(&auth.organization_id)
            .fetch_optional(self.pool.as_ref())
            .await
            .ok()
            .flatten()
            .unwrap_or_else(|| "unknown".to_string());

            self.outgoing_webhooks.spawn_dispatch(
                auth.organization_id.clone(),
                "job.failed".to_string(),
                req.job_id.clone(),
                serde_json::json!({
                    "job_id": req.job_id,
                    "queue_name": queue_name,
                    "status": result.new_status,
                    "error": error_message,
                }),
            );
        }

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

        let lease_id = Self::require_lease_id(&req.lease_id)?;
        let allowed_queues = queue_scope_filter(&auth);
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
              AND lease_expires_at IS NOT NULL
              AND lease_expires_at > NOW()
              AND lease_id = $5
              AND ($6::TEXT[] IS NULL OR queue_name = ANY($6))
            "#,
        )
        .bind(new_expires_at)
        .bind(&req.job_id)
        .bind(&req.worker_id)
        .bind(&auth.organization_id)
        .bind(lease_id)
        .bind(allowed_queues)
        .execute(self.pool.as_ref())
        .await
        .map_err(|e| {
            error!(error = %e, "Failed to renew lease");
            Status::internal("Failed to renew lease")
        })?;

        if result.rows_affected() == 0 {
            return Err(classify_worker_miss_status(
                self.pool.as_ref(),
                &req.job_id,
                &req.worker_id,
                &auth.organization_id,
                Some(lease_id),
                allowed_queues,
            )
            .await);
        }

        debug!(
            job_id = %req.job_id,
            "Lease renewed via gRPC"
        );

        Ok(Response::new(RenewLeaseResponse {
            success: true,
            new_expires_at: Some(datetime_to_timestamp(new_expires_at)),
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

        let allowed_queues = queue_scope_filter(&auth);
        let job: Option<DbJob> = sqlx::query_as(
            "SELECT * FROM jobs WHERE id = $1 AND organization_id = $2 AND ($3::TEXT[] IS NULL OR queue_name = ANY($3))",
        )
                .bind(&req.job_id)
                .bind(&auth.organization_id)
                .bind(allowed_queues)
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
        Self::require_queue_access(&auth, &req.queue_name)?;

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

        // Queue scope: a restricted key cannot stream-claim from a queue outside its
        // scope (mirrors the unary Dequeue check; the streaming path had none).
        if !auth.can_access_queue(&req.queue_name) {
            return Err(Status::permission_denied(format!(
                "API key does not have permission for queue '{}'",
                req.queue_name
            )));
        }

        if req.worker_id.is_empty() {
            return Err(Status::invalid_argument("Worker ID is required"));
        }

        let lease_duration = Self::safe_lease_duration(req.lease_duration_secs);
        let pool = self.pool.clone();
        let metrics = self.metrics.clone();
        let this = self.clone();
        let org_id = auth.organization_id.clone();
        let api_key_id = auth.api_key_id.clone();
        let api_key_rate_limit = auth.rate_limit;
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

                let current_auth = match reload_api_key_context(pool.as_ref(), &api_key_id).await {
                    Ok(auth) if auth.can_access_queue(&queue_name) => auth,
                    Ok(_) => {
                        let _ = tx
                            .send(Err(Status::permission_denied(format!(
                                "API key does not have permission for queue '{}'",
                                queue_name
                            ))))
                            .await;
                        break;
                    }
                    Err(status) => {
                        let _ = tx.send(Err(status)).await;
                        break;
                    }
                };

                if let Err(status) = this
                    .enforce_rate_limits(
                        &org_id,
                        &api_key_id,
                        current_auth.rate_limit.or(api_key_rate_limit),
                    )
                    .await
                {
                    if tx.send(Err(status)).await.is_err() {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(STREAM_POLL_INTERVAL_MS)).await;
                    continue;
                }

                // Lock the current key authorization through claim commit so a concurrent
                // revocation or scope narrowing cannot race this protected operation.
                let mut operation_tx = match begin_stream_operation(
                    pool.as_ref(),
                    &api_key_id,
                    &org_id,
                    Some(&queue_name),
                )
                .await
                {
                    Ok(tx) => tx,
                    Err(status) => {
                        let _ = tx.send(Err(status)).await;
                        break;
                    }
                };

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
                          AND EXISTS (
                              SELECT 1
                              FROM api_keys k
                              INNER JOIN organizations o ON o.id = k.organization_id
                              WHERE k.id = $6
                                AND k.organization_id = jobs.organization_id
                                AND k.is_active = TRUE
                                AND (k.expires_at IS NULL OR k.expires_at > NOW())
                                AND o.plan_tier <> 'deleted'
                                AND (
                                    cardinality(k.queues) = 0
                                    OR '*' = ANY(k.queues)
                                    OR jobs.queue_name = ANY(k.queues)
                                )
                          )
                          AND status IN ('pending', 'scheduled')
                          AND (scheduled_at IS NULL OR scheduled_at <= NOW())
                          AND (expires_at IS NULL OR expires_at > NOW())
                          AND (dependencies_met IS NULL OR dependencies_met = TRUE)
                          -- Respect paused queues (queue_config.enabled = false), matching
                          -- the REST QueueManager::dequeue_batch path.
                          AND NOT EXISTS (
                              SELECT 1 FROM queue_config qc
                              WHERE qc.organization_id = $4
                                AND qc.queue_name = $5
                                AND qc.enabled = false
                          )
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
                .bind(&api_key_id)
                .fetch_optional(&mut *operation_tx)
                .await;

                match job_result {
                    Ok(Some(job)) => {
                        if let Err(e) = operation_tx.commit().await {
                            error!(error = %e, "Failed to commit streamed job claim");
                            let _ = tx
                                .send(Err(Status::internal("Error polling for jobs")))
                                .await;
                            break;
                        }
                        metrics.jobs_processing.inc();
                        debug!(job_id = %job.id, queue = %queue_name, "Streaming job to worker");

                        if tx.send(Ok(job_to_proto(&job))).await.is_err() {
                            break;
                        }
                    }
                    Ok(None) => {
                        if let Err(e) = operation_tx.commit().await {
                            error!(error = %e, "Failed to commit empty streamed job claim");
                            let _ = tx
                                .send(Err(Status::internal("Error polling for jobs")))
                                .await;
                            break;
                        }
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
                let current_auth = match reload_api_key_context(pool.as_ref(), &api_key_id).await {
                    Ok(auth) => auth,
                    Err(status) => {
                        let response = process_error("UNAUTHENTICATED", status.message());
                        let _ = tx.send(Ok(response)).await;
                        break;
                    }
                };

                // IMPORTANT: enforce rate limits per message, otherwise the stream can bypass limits.
                if let Err(status) = this
                    .enforce_rate_limits(
                        &org_id,
                        &api_key_id,
                        current_auth.rate_limit.or(api_key_rate_limit),
                    )
                    .await
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
                        handle_process_dequeue(&pool, &metrics, &org_id, &api_key_id, dequeue_req)
                            .await
                    }
                    Some(crate::grpc::proto::process_request::Request::Complete(complete_req)) => {
                        handle_process_complete(&pool, &metrics, &org_id, &api_key_id, complete_req)
                            .await
                    }
                    Some(crate::grpc::proto::process_request::Request::Fail(fail_req)) => {
                        handle_process_fail(&pool, &metrics, &org_id, &api_key_id, fail_req).await
                    }
                    Some(crate::grpc::proto::process_request::Request::RenewLease(renew_req)) => {
                        handle_process_renew(&pool, &org_id, &api_key_id, renew_req).await
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

fn queue_scope_filter(auth: &GrpcAuthContext) -> Option<&[String]> {
    queue_scope_filter_from_queues(&auth.queues)
}

fn queue_scope_filter_from_queues(queues: &[String]) -> Option<&[String]> {
    if queues.is_empty() || queues.iter().any(|queue| queue == "*") {
        None
    } else {
        Some(queues)
    }
}

async fn begin_stream_operation<'a>(
    pool: &'a PgPool,
    api_key_id: &str,
    org_id: &str,
    queue_name: Option<&str>,
) -> Result<Transaction<'a, Postgres>, Status> {
    let mut tx = pool.begin().await.map_err(|e| {
        error!(error = %e, "Failed to begin stream operation");
        Status::internal("Failed to begin stream operation")
    })?;

    let queues: Option<Vec<String>> = sqlx::query_scalar(
        r#"
        SELECT k.queues
        FROM api_keys k
        INNER JOIN organizations o ON o.id = k.organization_id
        WHERE k.id = $1
          AND k.organization_id = $2
          AND k.is_active = TRUE
          AND (k.expires_at IS NULL OR k.expires_at > NOW())
          AND o.plan_tier <> 'deleted'
        FOR SHARE OF k
        "#,
    )
    .bind(api_key_id)
    .bind(org_id)
    .fetch_optional(&mut *tx)
    .await
    .map_err(|e| {
        error!(error = %e, api_key_id = %api_key_id, "Failed to lock stream authorization");
        Status::internal("Authentication revalidation failed")
    })?;

    let queues = queues.ok_or_else(|| Status::unauthenticated("API key revoked or expired"))?;
    if let Some(queue_name) = queue_name {
        let allowed = queues.is_empty()
            || queues
                .iter()
                .any(|allowed| allowed == "*" || allowed == queue_name);
        if !allowed {
            return Err(Status::permission_denied(format!(
                "API key does not have permission for queue '{}'",
                queue_name
            )));
        }
    }

    Ok(tx)
}

fn process_status_error(status: &Status) -> ProcessResponse {
    let code = match status.code() {
        tonic::Code::Unauthenticated => "UNAUTHENTICATED",
        tonic::Code::PermissionDenied => "PERMISSION_DENIED",
        tonic::Code::InvalidArgument => "INVALID_ARGUMENT",
        _ => "INTERNAL",
    };
    process_error(code, status.message())
}

fn process_error(code: &str, message: &str) -> ProcessResponse {
    ProcessResponse {
        response: Some(crate::grpc::proto::process_response::Response::Error(
            crate::grpc::proto::ErrorResponse {
                code: code.to_string(),
                message: message.to_string(),
            },
        )),
    }
}

fn truncate_error_message(message: &str) -> String {
    if message.len() <= MAX_ERROR_MESSAGE_LENGTH {
        return message.to_string();
    }

    let max_prefix_len = MAX_ERROR_MESSAGE_LENGTH - TRUNCATION_SUFFIX.len();
    let boundary = message
        .char_indices()
        .map(|(index, _)| index)
        .take_while(|index| *index <= max_prefix_len)
        .last()
        .unwrap_or(0);
    format!("{}{}", &message[..boundary], TRUNCATION_SUFFIX)
}

/// Handle dequeue request in ProcessJobs stream
async fn handle_process_dequeue(
    pool: &PgPool,
    metrics: &Metrics,
    org_id: &str,
    api_key_id: &str,
    req: DequeueRequest,
) -> ProcessResponse {
    if req.worker_id.is_empty() {
        return ProcessResponse {
            response: Some(crate::grpc::proto::process_response::Response::Error(
                crate::grpc::proto::ErrorResponse {
                    code: "INVALID_ARGUMENT".to_string(),
                    message: "Worker ID is required".to_string(),
                },
            )),
        };
    }

    let mut tx = match begin_stream_operation(pool, api_key_id, org_id, Some(&req.queue_name)).await
    {
        Ok(tx) => tx,
        Err(status) => return process_status_error(&status),
    };
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
              -- Respect paused queues, matching the REST dequeue path.
              AND NOT EXISTS (
                  SELECT 1 FROM queue_config qc
                  WHERE qc.organization_id = $4
                    AND qc.queue_name = $5
                    AND qc.enabled = false
              )
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
    .fetch_optional(&mut *tx)
    .await;

    match job {
        Ok(Some(job)) => {
            if let Err(e) = tx.commit().await {
                error!(error = %e, "Failed to commit ProcessJobs dequeue");
                return process_error("INTERNAL", "Failed to dequeue job");
            }
            metrics.jobs_processing.inc();
            ProcessResponse {
                response: Some(crate::grpc::proto::process_response::Response::Job(
                    job_to_proto(&job),
                )),
            }
        }
        Ok(None) => {
            if let Err(e) = tx.commit().await {
                error!(error = %e, "Failed to commit empty ProcessJobs dequeue");
                return process_error("INTERNAL", "Failed to dequeue job");
            }
            ProcessResponse {
                response: Some(crate::grpc::proto::process_response::Response::Error(
                    crate::grpc::proto::ErrorResponse {
                        code: "NO_JOBS".to_string(),
                        message: "No jobs available".to_string(),
                    },
                )),
            }
        }
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

/// Classify why a worker-scoped UPDATE affected zero rows into a
/// `tonic::Status` for unary RPCs. Returns `FailedPrecondition`
/// (LEASE_EXPIRED) when a matching row exists but its lease is
/// null/past, and `NotFound` otherwise.
async fn classify_worker_miss_status(
    pool: &PgPool,
    job_id: &str,
    worker_id: &str,
    org_id: &str,
    lease_id: Option<&str>,
    allowed_queues: Option<&[String]>,
) -> Status {
    /// Same shape as the ProcessJobs helper below — factored to a type alias
    /// to appease clippy's `type_complexity` lint.
    type WorkerMissRow = (String, Option<chrono::DateTime<Utc>>, Option<String>);

    let row: Result<Option<WorkerMissRow>, _> = sqlx::query_as(
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
    .fetch_optional(pool)
    .await;

    match row {
        Ok(Some((status, lease, current_lease_id))) if status == "processing" => {
            let expired = lease.map(|t| t <= Utc::now()).unwrap_or(true);
            // A presented fencing token that no longer matches means the
            // caller's lease was superseded — report it as expired.
            let fenced_out = match lease_id {
                Some(presented) => current_lease_id.as_deref() != Some(presented),
                None => false,
            };
            if expired || fenced_out {
                Status::failed_precondition("Lease expired")
            } else {
                Status::not_found("Job not found or not authorized")
            }
        }
        Ok(_) => Status::not_found("Job not found or not authorized"),
        Err(e) => {
            error!(error = %e, job_id = %job_id, "Failed to classify worker miss");
            Status::internal("Failed to check job state")
        }
    }
}

/// Same as [`classify_worker_miss_status`] but returning an in-band
/// `ErrorResponse` for ProcessJobs streaming replies. Uses code
/// `LEASE_EXPIRED` (mirrors REST) so clients can distinguish it from
/// generic `NOT_FOUND`.
async fn classify_worker_miss_process_response(
    tx: &mut Transaction<'_, Postgres>,
    job_id: &str,
    worker_id: &str,
    org_id: &str,
    api_key_id: &str,
    lease_id: Option<&str>,
) -> ProcessResponse {
    /// Row returned by the worker-miss classifier: current `status`, the
    /// job's `lease_expires_at` (nullable), and its current `lease_id`.
    /// Factored out to keep clippy's `type_complexity` lint happy.
    type WorkerMissRow = (String, Option<chrono::DateTime<Utc>>, Option<String>);

    let (code, message) = match sqlx::query_as::<_, WorkerMissRow>(
        r#"
        SELECT status, lease_expires_at, lease_id
        FROM jobs
        WHERE id = $1
          AND organization_id = $2
          AND assigned_worker_id = $3
          AND EXISTS (
              SELECT 1 FROM api_keys k
              WHERE k.id = $4
                AND k.organization_id = jobs.organization_id
                AND (
                    cardinality(k.queues) = 0
                    OR '*' = ANY(k.queues)
                    OR jobs.queue_name = ANY(k.queues)
                )
          )
        "#,
    )
    .bind(job_id)
    .bind(org_id)
    .bind(worker_id)
    .bind(api_key_id)
    .fetch_optional(&mut **tx)
    .await
    {
        Ok(Some((status, lease, current_lease_id))) if status == "processing" => {
            let expired = lease.map(|t| t <= Utc::now()).unwrap_or(true);
            // A presented fencing token that no longer matches means the
            // caller's lease was superseded — report it as expired.
            let fenced_out = match lease_id {
                Some(presented) => current_lease_id.as_deref() != Some(presented),
                None => false,
            };
            if expired || fenced_out {
                ("LEASE_EXPIRED", "Lease expired")
            } else {
                ("NOT_FOUND", "Job not found or not authorized")
            }
        }
        Ok(_) => ("NOT_FOUND", "Job not found or not authorized"),
        Err(e) => {
            error!(error = %e, job_id = %job_id, "Failed to classify worker miss in ProcessJobs");
            ("INTERNAL", "Failed to check job state")
        }
    };

    ProcessResponse {
        response: Some(crate::grpc::proto::process_response::Response::Error(
            crate::grpc::proto::ErrorResponse {
                code: code.to_string(),
                message: message.to_string(),
            },
        )),
    }
}

/// Handle complete request in ProcessJobs stream
async fn handle_process_complete(
    pool: &PgPool,
    metrics: &Metrics,
    org_id: &str,
    api_key_id: &str,
    req: CompleteRequest,
) -> ProcessResponse {
    // Bind the result as a TEXT JSON string (matching the REST/unary paths)
    // so an identical SQL string can never be prepared with conflicting
    // parameter types on a shared pool connection (see unary `complete`).
    if req.lease_id.is_empty() {
        return ProcessResponse {
            response: Some(crate::grpc::proto::process_response::Response::Error(
                crate::grpc::proto::ErrorResponse {
                    code: "INVALID_ARGUMENT".to_string(),
                    message: "lease_id is required for job settlement".to_string(),
                },
            )),
        };
    }
    let result_json = struct_to_json_opt(req.result.as_ref());
    let result_str = serde_json::to_string(&result_json).unwrap_or_default();
    if result_str.len() > MAX_RESULT_SIZE {
        return ProcessResponse {
            response: Some(crate::grpc::proto::process_response::Response::Error(
                crate::grpc::proto::ErrorResponse {
                    code: "INVALID_ARGUMENT".to_string(),
                    message: format!(
                        "Result too large: {} bytes (max: {} bytes)",
                        result_str.len(),
                        MAX_RESULT_SIZE
                    ),
                },
            )),
        };
    }

    let mut tx = match begin_stream_operation(pool, api_key_id, org_id, None).await {
        Ok(tx) => tx,
        Err(status) => return process_status_error(&status),
    };

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
          AND lease_expires_at IS NOT NULL
          AND lease_expires_at > NOW()
          AND lease_id = $5
          AND EXISTS (
              SELECT 1 FROM api_keys k
              WHERE k.id = $6
                AND k.organization_id = jobs.organization_id
                AND (
                    cardinality(k.queues) = 0
                    OR '*' = ANY(k.queues)
                    OR jobs.queue_name = ANY(k.queues)
                )
          )
        "#,
    )
    .bind(&result_str)
    .bind(&req.job_id)
    .bind(&req.worker_id)
    .bind(org_id)
    .bind(QueueServiceImpl::opt_lease_id(&req.lease_id))
    .bind(api_key_id)
    .execute(&mut *tx)
    .await;

    match db_result {
        Ok(result) if result.rows_affected() > 0 => {
            if let Err(e) = tx.commit().await {
                error!(error = %e, "Failed to commit ProcessJobs completion");
                return process_error("INTERNAL", "Failed to complete job");
            }
            metrics.jobs_completed.inc();
            metrics.jobs_processing.dec();
            ProcessResponse {
                response: Some(crate::grpc::proto::process_response::Response::Complete(
                    CompleteResponse { success: true },
                )),
            }
        }
        Ok(_) => {
            let response = classify_worker_miss_process_response(
                &mut tx,
                &req.job_id,
                &req.worker_id,
                org_id,
                api_key_id,
                QueueServiceImpl::opt_lease_id(&req.lease_id),
            )
            .await;
            if let Err(e) = tx.commit().await {
                error!(error = %e, "Failed to commit ProcessJobs completion miss");
                return process_error("INTERNAL", "Failed to complete job");
            }
            response
        }
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
    api_key_id: &str,
    req: FailRequest,
) -> ProcessResponse {
    if req.lease_id.is_empty() {
        return ProcessResponse {
            response: Some(crate::grpc::proto::process_response::Response::Error(
                crate::grpc::proto::ErrorResponse {
                    code: "INVALID_ARGUMENT".to_string(),
                    message: "lease_id is required for job settlement".to_string(),
                },
            )),
        };
    }
    let error_message = truncate_error_message(&req.error);
    let mut tx = match begin_stream_operation(pool, api_key_id, org_id, None).await {
        Ok(tx) => tx,
        Err(status) => return process_status_error(&status),
    };

    #[derive(sqlx::FromRow)]
    struct UpdateResult {
        new_status: String,
        new_retry_count: i32,
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
                THEN NOW() + (LEAST(POWER(2, LEAST(retry_count, 6)), 60) || ' seconds')::INTERVAL
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
          AND lease_expires_at IS NOT NULL
          AND lease_expires_at > NOW()
          AND lease_id = $6
          AND EXISTS (
              SELECT 1 FROM api_keys k
              WHERE k.id = $7
                AND k.organization_id = jobs.organization_id
                AND (
                    cardinality(k.queues) = 0
                    OR '*' = ANY(k.queues)
                    OR jobs.queue_name = ANY(k.queues)
                )
          )
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
    .bind(org_id)
    .bind(QueueServiceImpl::opt_lease_id(&req.lease_id))
    .bind(api_key_id)
    .fetch_optional(&mut *tx)
    .await;

    match result {
        Ok(Some(update_result)) => {
            let will_retry = update_result.new_status == "pending";
            if update_result.new_status == "deadletter" {
                // Parity with REST: insert into dead_letter_queue.
                if let Err(e) = sqlx::query(
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
                .bind(&error_message)
                .bind(serde_json::json!({
                    "final_error": error_message,
                    "total_retries": update_result.new_retry_count,
                }))
                .bind(&req.job_id)
                .bind(org_id)
                .execute(&mut *tx)
                .await
                {
                    error!(error = %e, job_id = %req.job_id, "Failed to insert dead_letter_queue row in ProcessJobs");
                }
            }
            if let Err(e) = tx.commit().await {
                error!(error = %e, "Failed to commit ProcessJobs failure");
                return process_error("INTERNAL", "Failed to update job");
            }
            metrics.jobs_failed.inc();
            metrics.jobs_processing.dec();
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
        Ok(None) => {
            let response = classify_worker_miss_process_response(
                &mut tx,
                &req.job_id,
                &req.worker_id,
                org_id,
                api_key_id,
                QueueServiceImpl::opt_lease_id(&req.lease_id),
            )
            .await;
            if let Err(e) = tx.commit().await {
                error!(error = %e, "Failed to commit ProcessJobs failure miss");
                return process_error("INTERNAL", "Failed to update job");
            }
            response
        }
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
    api_key_id: &str,
    req: RenewLeaseRequest,
) -> ProcessResponse {
    if req.lease_id.is_empty() {
        return ProcessResponse {
            response: Some(crate::grpc::proto::process_response::Response::Error(
                crate::grpc::proto::ErrorResponse {
                    code: "INVALID_ARGUMENT".to_string(),
                    message: "lease_id is required for job settlement".to_string(),
                },
            )),
        };
    }
    let extension_secs = QueueServiceImpl::safe_lease_duration(req.extension_secs);
    let new_expires_at = Utc::now() + chrono::Duration::seconds(extension_secs as i64);
    let mut tx = match begin_stream_operation(pool, api_key_id, org_id, None).await {
        Ok(tx) => tx,
        Err(status) => return process_status_error(&status),
    };

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
          AND EXISTS (
              SELECT 1 FROM api_keys k
              WHERE k.id = $6
                AND k.organization_id = jobs.organization_id
                AND (
                    cardinality(k.queues) = 0
                    OR '*' = ANY(k.queues)
                    OR jobs.queue_name = ANY(k.queues)
                )
          )
        "#,
    )
    .bind(new_expires_at)
    .bind(&req.job_id)
    .bind(&req.worker_id)
    .bind(org_id)
    .bind(QueueServiceImpl::opt_lease_id(&req.lease_id))
    .bind(api_key_id)
    .execute(&mut *tx)
    .await;

    match result {
        Ok(res) if res.rows_affected() > 0 => {
            if let Err(e) = tx.commit().await {
                error!(error = %e, "Failed to commit ProcessJobs lease renewal");
                return process_error("INTERNAL", "Failed to renew lease");
            }
            ProcessResponse {
                response: Some(crate::grpc::proto::process_response::Response::RenewLease(
                    RenewLeaseResponse {
                        success: true,
                        new_expires_at: Some(datetime_to_timestamp(new_expires_at)),
                    },
                )),
            }
        }
        Ok(_) => {
            let response = classify_worker_miss_process_response(
                &mut tx,
                &req.job_id,
                &req.worker_id,
                org_id,
                api_key_id,
                QueueServiceImpl::opt_lease_id(&req.lease_id),
            )
            .await;
            if let Err(e) = tx.commit().await {
                error!(error = %e, "Failed to commit ProcessJobs lease renewal miss");
                return process_error("INTERNAL", "Failed to renew lease");
            }
            response
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
    use prost::Message;

    fn round_trip<T>(message: &T) -> T
    where
        T: Message + Default,
    {
        T::decode(message.encode_to_vec().as_slice()).expect("protobuf round trip")
    }

    #[test]
    fn test_public_settlement_requests_preserve_lease_ids_on_wire() {
        let complete = round_trip(&CompleteRequest {
            job_id: "job".to_string(),
            worker_id: "worker".to_string(),
            result: None,
            lease_id: "complete-lease".to_string(),
        });
        assert_eq!(complete.lease_id, "complete-lease");

        let fail = round_trip(&FailRequest {
            job_id: "job".to_string(),
            worker_id: "worker".to_string(),
            error: "boom".to_string(),
            retry: true,
            lease_id: "fail-lease".to_string(),
        });
        assert_eq!(fail.lease_id, "fail-lease");

        let renew = round_trip(&RenewLeaseRequest {
            job_id: "job".to_string(),
            worker_id: "worker".to_string(),
            extension_secs: 120,
            lease_id: "renew-lease".to_string(),
        });
        assert_eq!(renew.extension_secs, 120);
        assert_eq!(renew.lease_id, "renew-lease");
    }

    #[test]
    fn test_process_requests_preserve_settlement_variants_on_wire() {
        use crate::grpc::proto::process_request::Request as Operation;

        let operations = [
            Operation::Complete(CompleteRequest {
                job_id: "complete".to_string(),
                worker_id: "worker".to_string(),
                result: None,
                lease_id: "lease-c".to_string(),
            }),
            Operation::Fail(FailRequest {
                job_id: "fail".to_string(),
                worker_id: "worker".to_string(),
                error: "boom".to_string(),
                retry: false,
                lease_id: "lease-f".to_string(),
            }),
            Operation::RenewLease(RenewLeaseRequest {
                job_id: "renew".to_string(),
                worker_id: "worker".to_string(),
                extension_secs: 60,
                lease_id: "lease-r".to_string(),
            }),
        ];

        for operation in operations {
            let decoded = round_trip(&ProcessRequest {
                request: Some(operation),
            });
            match decoded.request.expect("operation") {
                Operation::Complete(req) => assert_eq!(req.lease_id, "lease-c"),
                Operation::Fail(req) => assert_eq!(req.lease_id, "lease-f"),
                Operation::RenewLease(req) => assert_eq!(req.lease_id, "lease-r"),
                Operation::Dequeue(_) => panic!("unexpected dequeue"),
            }
        }
    }

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
    fn test_truncate_error_message_preserves_utf8_boundaries() {
        let message = format!("{}é", "a".repeat(MAX_ERROR_MESSAGE_LENGTH - 1));
        let truncated = truncate_error_message(&message);

        assert!(truncated.is_char_boundary(truncated.len()));
        assert!(truncated.ends_with(TRUNCATION_SUFFIX));
        assert!(truncated.len() <= MAX_ERROR_MESSAGE_LENGTH);
        assert_eq!(
            truncated,
            format!(
                "{}{}",
                "a".repeat(MAX_ERROR_MESSAGE_LENGTH - TRUNCATION_SUFFIX.len()),
                TRUNCATION_SUFFIX
            )
        );
    }

    #[test]
    fn test_truncate_error_message_leaves_short_multibyte_text_unchanged() {
        let message = "помилка 🚨";
        assert_eq!(truncate_error_message(message), message);
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
