//! QueueService gRPC implementation
//!
//! Implements all queue operations: Enqueue, Dequeue, Complete, Fail,
//! RenewLease, GetJob, GetQueueStats, StreamJobs, and ProcessJobs.

// tonic::Status is a standard gRPC error type, size is acceptable
#![allow(clippy::result_large_err)]

use std::collections::HashMap;
use std::ops::RangeInclusive;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::Utc;
use futures::{Stream, StreamExt};
use prost::Message;
use sqlx::{PgPool, Postgres, Transaction};
use tokio::sync::{mpsc, Notify};
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status, Streaming};
use tracing::{debug, error, info, warn};

use crate::api::middleware::limits::{
    check_job_limits_generic, check_new_queue_limit_generic, check_new_queue_limit_generic_conn,
    check_payload_size_generic, daily_jobs_limit, lock_resource, queue_job_defaults,
    queue_name_exists, refund_daily_jobs, try_increment_daily_jobs, LimitCheckError,
};
use crate::cache::RedisCache;
use crate::config::{PlanLimits, QueueSettings, WorkerSettings};
use crate::grpc::auth::{
    authenticate_from_metadata_with_cache, authenticate_request_with_cache, reload_api_key_context,
    GrpcAuthContext,
};
use crate::grpc::constants::{
    MAX_BATCH_SIZE, MAX_GRPC_MESSAGE_SIZE, MAX_GRPC_PAYLOAD_SIZE as MAX_PAYLOAD_SIZE,
    MAX_RESULT_SIZE,
};
use crate::grpc::convert::{
    datetime_to_timestamp, job_to_proto, struct_to_json, struct_to_json_opt,
    timestamp_to_datetime_opt,
};
use crate::grpc::proto::{
    queue_service_server::QueueService, CompleteRequest, CompleteResponse, DequeueRequest,
    DequeueResponse, EnqueueRequest, EnqueueResponse, FailRequest, FailResponse, GetJobRequest,
    GetJobResponse, GetQueueStatsRequest, GetQueueStatsResponse, Job, ProcessRequest,
    ProcessResponse, RenewLeaseRequest, RenewLeaseResponse, StreamJobsRequest,
};
use crate::grpc::validation::lease_duration_or_default;
use crate::models::Job as DbJob;
use crate::observability::Metrics;
use crate::outgoing_webhooks::service::OutgoingWebhookService;

/// Maximum error message length
#[cfg(test)]
const MAX_ERROR_MESSAGE_LENGTH: usize = crate::models::MAX_FAILURE_MESSAGE_BYTES;
#[cfg(test)]
const TRUNCATION_SUFFIX: &str = crate::models::FAILURE_TRUNCATION_SUFFIX;

/// Poll interval for streaming jobs (milliseconds). Also the back-off while a
/// stream is over its rate limit, and the longest wait between two checks on
/// the job a stream's worker holds.
const STREAM_POLL_INTERVAL_MS: u64 = 1000;

/// First wait (milliseconds) between checks on the job a stream's worker
/// holds; it doubles up to `STREAM_POLL_INTERVAL_MS`. A settle in this process
/// wakes the stream at once (`SettleSignals`).
const STREAM_SETTLE_CHECK_MIN_MS: u64 = 10;

/// Open StreamJobs streams per organization, counted in this process. Same
/// number as the REST per-org SSE and WebSocket caps.
const MAX_STREAMS_PER_ORG: usize = 100;

/// A Dequeue batch stops growing past this many encoded bytes, leaving room
/// under `MAX_GRPC_MESSAGE_SIZE`. A single job may use the whole message.
const MAX_BATCH_RESPONSE_BYTES: usize = 3 * 1024 * 1024;

/// REST `CreateJobRequest` limits (models/job.rs) that Enqueue applies too.
const MAX_ENQUEUE_QUEUE_NAME_CHARS: usize = 100;
const MAX_IDEMPOTENCY_KEY_CHARS: usize = 255;
const MAX_TAGS_SIZE: usize = 64 * 1024;
const PRIORITY_RANGE: RangeInclusive<i32> = -100..=100;

/// QueueService implementation
#[derive(Clone)]
pub struct QueueServiceImpl {
    pool: Arc<PgPool>,
    metrics: Arc<Metrics>,
    cache: Option<RedisCache>,
    outgoing_webhooks: Arc<OutgoingWebhookService>,
    /// Shared with REST (`QUEUE_DEFAULT_*`) so omit/`<=0` proto3 zeros match.
    queue_defaults: QueueSettings,
    /// Mirror REST `RATE_LIMIT_FAIL_CLOSED`.
    rate_limit_fail_closed: bool,
    /// `GRPC_DEFAULT_LEASE_DURATION_SECS` for omitted leases (see [`Self::lease_secs`]).
    worker_settings: WorkerSettings,
    /// Open StreamJobs streams per organization (`MAX_STREAMS_PER_ORG`).
    stream_slots: Arc<StreamSlots>,
    /// Wakes a StreamJobs stream when this process settles the job it waits on.
    settle_signals: Arc<SettleSignals>,
}

/// An org's plan rate limits, as REST's `plan_rate_limit` computes and caches
/// them under `org_rate_limits:{org}`.
#[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize)]
struct OrgRateLimits {
    rps: u32,
    burst: u32,
}

impl OrgRateLimits {
    fn from_plan(plan_tier: &str, custom_limits: Option<&serde_json::Value>) -> Self {
        let limits = PlanLimits::for_tier_with_overrides(plan_tier, custom_limits);
        // 0 is a real "blocked" override. Clamping it to 1 cached a request per
        // second under the shared org_rate_limits key (see check_token_bucket).
        Self {
            rps: limits.rate_limit_requests_per_second,
            burst: limits.rate_limit_burst,
        }
    }
}

/// Settles seen by this process, for StreamJobs streams waiting on the job
/// their worker holds. A settle elsewhere (REST, another replica) is noticed
/// by the stream's own checks instead.
#[derive(Default)]
struct SettleSignals(Mutex<HashMap<String, Arc<Notify>>>);

impl SettleSignals {
    /// Start waiting for `job_id` to be settled here.
    fn watch(self: &Arc<Self>, job_id: &str) -> SettleWatch {
        let notify = Arc::new(Notify::new());
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(job_id.to_string(), Arc::clone(&notify));
        SettleWatch {
            signals: Arc::clone(self),
            job_id: job_id.to_string(),
            notify,
        }
    }

    /// `job_id` was just completed or failed through this service.
    fn settled(&self, job_id: &str) {
        let watchers = self.0.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(notify) = watchers.get(job_id) {
            notify.notify_one();
        }
    }
}

/// One stream's wait on a job; unregistered when dropped.
struct SettleWatch {
    signals: Arc<SettleSignals>,
    job_id: String,
    notify: Arc<Notify>,
}

impl Drop for SettleWatch {
    fn drop(&mut self) {
        let mut watchers = self.signals.0.lock().unwrap_or_else(|e| e.into_inner());
        if watchers
            .get(&self.job_id)
            .is_some_and(|notify| Arc::ptr_eq(notify, &self.notify))
        {
            watchers.remove(&self.job_id);
        }
    }
}

/// Count of open StreamJobs streams per organization.
#[derive(Default)]
struct StreamSlots(Mutex<HashMap<String, usize>>);

impl StreamSlots {
    /// Take one of the org's stream slots, or `None` when all are in use.
    fn acquire(self: &Arc<Self>, org_id: &str) -> Option<StreamSlot> {
        let mut open = self.0.lock().unwrap_or_else(|e| e.into_inner());
        let count = open.entry(org_id.to_string()).or_insert(0);
        if *count >= MAX_STREAMS_PER_ORG {
            return None;
        }
        *count += 1;
        Some(StreamSlot {
            slots: Arc::clone(self),
            org_id: org_id.to_string(),
        })
    }
}

/// One open stream's slot; freed when the stream's producer task ends.
struct StreamSlot {
    slots: Arc<StreamSlots>,
    org_id: String,
}

impl Drop for StreamSlot {
    fn drop(&mut self) {
        let mut open = self.slots.0.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(count) = open.get_mut(&self.org_id) {
            *count = count.saturating_sub(1);
            if *count == 0 {
                open.remove(&self.org_id);
            }
        }
    }
}

/// A job claimed for a StreamJobs consumer and waiting in the stream's
/// one-slot buffer. If the stream is dropped before the consumer takes it
/// (client gone), the claim is undone: the job goes back to `pending` without
/// spending a retry, instead of sitting leased until the lease runs out.
struct BufferedJob {
    job: Option<Job>,
    pool: Arc<PgPool>,
    metrics: Arc<Metrics>,
    org_id: String,
}

impl BufferedJob {
    /// Hand the job to the transport; from here on it counts as delivered.
    fn deliver(mut self) -> Job {
        self.job.take().unwrap_or_default()
    }
}

impl Drop for BufferedJob {
    fn drop(&mut self) {
        let Some(job) = self.job.take() else {
            return;
        };
        let pool = Arc::clone(&self.pool);
        let metrics = Arc::clone(&self.metrics);
        let org_id = std::mem::take(&mut self.org_id);
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                if release_unsent_claim(&pool, &org_id, &job.id, &job.lease_id).await {
                    metrics.jobs_processing.dec();
                }
            });
        }
    }
}

impl QueueServiceImpl {
    pub fn new(
        pool: Arc<PgPool>,
        metrics: Arc<Metrics>,
        cache: Option<RedisCache>,
        outgoing_webhooks: Arc<OutgoingWebhookService>,
        queue_defaults: QueueSettings,
        rate_limit_fail_closed: bool,
        worker_settings: WorkerSettings,
    ) -> Self {
        Self {
            worker_settings,
            pool,
            metrics,
            cache,
            outgoing_webhooks,
            queue_defaults,
            rate_limit_fail_closed,
            stream_slots: Arc::default(),
            settle_signals: Arc::default(),
        }
    }

    /// Lease for a claim or renewal: `<= 0` (omitted) is
    /// `GRPC_DEFAULT_LEASE_DURATION_SECS` (300 s unless configured), an explicit
    /// value is clamped to 5-3600 s.
    ///
    /// Not REST's `WORKER_LEASE_DURATION_SECS` (30 s): the Go SDK's `StreamJobs`
    /// has no lease parameter and the Go/PHP `Dequeue` and `RenewLease` send 0
    /// unless set, so those workers would lose any job running past 30 s to a
    /// second worker (W3R-03).
    fn lease_secs(&self, requested: i32) -> i32 {
        lease_duration_or_default(
            requested,
            self.worker_settings.grpc_default_lease_duration_secs,
        )
    }

    /// Proto3 omitted int32 is 0 — treat as "use configured default", not zero retries.
    fn resolve_max_retries(&self, value: i32) -> i32 {
        if value <= 0 {
            self.queue_defaults.default_max_retries.clamp(0, 100)
        } else {
            value.clamp(1, 100)
        }
    }

    fn resolve_timeout_seconds(&self, value: i32) -> i32 {
        if value <= 0 {
            self.queue_defaults.default_timeout_secs.clamp(1, 86400)
        } else {
            value.clamp(1, 86400)
        }
    }

    async fn enforce_rate_limits(
        &self,
        org_id: &str,
        api_key_id: &str,
        api_key_per_minute: Option<i32>,
    ) -> Result<(), Status> {
        let Some(ref cache) = self.cache else {
            return self.without_rate_limiter();
        };
        let org_limits = self.org_rate_limits(cache, org_id).await?;
        self.charge_rate_limits(cache, org_id, org_limits, api_key_id, api_key_per_minute)
            .await
    }

    /// No Redis configured: distributed limits cannot be enforced. Self-host
    /// allows; `RATE_LIMIT_FAIL_CLOSED` denies.
    fn without_rate_limiter(&self) -> Result<(), Status> {
        if self.rate_limit_fail_closed {
            return Err(Status::resource_exhausted(
                "Rate limit unavailable (Redis not configured)",
            ));
        }
        Ok(())
    }

    /// The org's plan limits: cached in Redis, read from the database on a
    /// miss or a Redis error. That read takes a pool connection, so a caller
    /// holding one must use `charge_rate_limits` instead.
    async fn org_rate_limits(
        &self,
        cache: &RedisCache,
        org_id: &str,
    ) -> Result<OrgRateLimits, Status> {
        let cache_key = crate::api::middleware::plan_rate_limit::org_rate_limits_key(org_id);
        if let Ok(Some(limits)) = cache.get_json(&cache_key).await {
            return Ok(limits);
        }
        let (plan_tier, custom_limits): (String, Option<serde_json::Value>) =
            sqlx::query_as("SELECT plan_tier, custom_limits FROM organizations WHERE id = $1")
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
        let limits = OrgRateLimits::from_plan(&plan_tier, custom_limits.as_ref());
        let _ = cache.set_json(&cache_key, &limits, 60).await;
        Ok(limits)
    }

    /// Charge one request to the org's token bucket (plan rps/burst) and to
    /// the key's per-minute window. Redis only, never the database pool.
    /// Default fail-open on Redis errors (self-host); `RATE_LIMIT_FAIL_CLOSED`
    /// denies for hosted SaaS.
    async fn charge_rate_limits(
        &self,
        cache: &RedisCache,
        org_id: &str,
        org_limits: OrgRateLimits,
        api_key_id: &str,
        api_key_per_minute: Option<i32>,
    ) -> Result<(), Status> {
        // Key window first: a key 429 must not spend an org token.
        if let Some(per_min) = api_key_per_minute {
            let per_min = per_min.max(1) as u32;
            let key = format!("rate_limit:grpc:api_key:{}", api_key_id);
            match cache.check_rate_limit(&key, per_min, 60).await {
                Ok(rl) if !rl.allowed => {
                    return Err(Status::resource_exhausted("Rate limit exceeded"));
                }
                Ok(_) => {}
                Err(e) => {
                    if self.rate_limit_fail_closed {
                        warn!(error = %e, api_key_id = %api_key_id, "Redis error during gRPC api-key rate limit — denying (fail-closed)");
                        return Err(Status::resource_exhausted("Rate limit unavailable"));
                    }
                    warn!(error = %e, api_key_id = %api_key_id, "Redis error during gRPC api-key rate limit — allowing (fail-open)");
                }
            }
        }

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
                if self.rate_limit_fail_closed {
                    warn!(error = %e, org_id = %org_id, "Redis error during gRPC org rate limit — denying (fail-closed)");
                    return Err(Status::resource_exhausted("Rate limit unavailable"));
                }
                warn!(error = %e, org_id = %org_id, "Redis error during gRPC org rate limit — allowing (fail-open)");
            }
        }

        Ok(())
    }

    /// Charge one StreamJobs claim while its transaction is open, against the
    /// limits read from the locked key row. Asking the pool for the org's plan
    /// here, as `enforce_rate_limits` does on a cache miss, left every claim in
    /// flight waiting for a second connection: with as many busy streams as
    /// pool connections, none was ever given back.
    async fn charge_stream_claim(
        &self,
        org_id: &str,
        api_key_id: &str,
        locked: &LockedStreamKey,
    ) -> Result<(), Status> {
        match self.cache.as_ref() {
            Some(cache) => {
                self.charge_rate_limits(
                    cache,
                    org_id,
                    locked.org_limits,
                    api_key_id,
                    locked.key_rate_limit,
                )
                .await
            }
            None => self.without_rate_limiter(),
        }
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
        crate::grpc::validation::validate_queue_name(name)
    }

    /// The REST `CreateJobRequest` limits Enqueue used to skip. Past them a
    /// request either tripped a database constraint (reported as INTERNAL,
    /// which SDKs retry) or stored far more than the plan's payload cap in
    /// `tags`.
    fn validate_enqueue_fields(
        req: &EnqueueRequest,
        tags_json: &serde_json::Value,
    ) -> Result<(), Status> {
        if req.queue_name.chars().count() > MAX_ENQUEUE_QUEUE_NAME_CHARS {
            return Err(Status::invalid_argument(
                "Queue name must be 1-100 characters",
            ));
        }
        if !PRIORITY_RANGE.contains(&req.priority) {
            return Err(Status::invalid_argument(
                "Priority must be between -100 and 100",
            ));
        }
        if req.idempotency_key.chars().count() > MAX_IDEMPOTENCY_KEY_CHARS {
            return Err(Status::invalid_argument(
                "Idempotency key must be at most 255 characters",
            ));
        }
        let tags_size = serde_json::to_string(tags_json).map_or(0, |s| s.len());
        if tags_size > MAX_TAGS_SIZE {
            return Err(Status::invalid_argument(format!(
                "Tags too large: {} bytes (max: {} bytes)",
                tags_size, MAX_TAGS_SIZE
            )));
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

    /// Claim a single job from the queue. `lease_duration` is already
    /// resolved by [`Self::lease_secs`].
    async fn claim_job(
        &self,
        org_id: &str,
        queue_name: &str,
        worker_id: &str,
        lease_duration: i32,
    ) -> Result<Option<DbJob>, Status> {
        let lease_id = uuid::Uuid::new_v4().to_string();

        let job: Option<DbJob> = sqlx::query_as(
            r#"
            UPDATE jobs
            SET
                status = 'processing',
                started_at = COALESCE(started_at, NOW()),
                assigned_worker_id = $1,
                lease_id = $2,
                lease_expires_at = NOW() + ($3 || ' seconds')::INTERVAL,
                -- The granted length, so a REST heartbeat that names none keeps it.
                lease_duration_secs = $3,
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
        let auth =
            authenticate_request_with_cache(self.pool.as_ref(), self.cache.as_ref(), &request)
                .await?;
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

        // Convert tags from map to JSON
        let tags_json: serde_json::Value = req
            .tags
            .iter()
            .map(|(k, v)| (k.clone(), serde_json::Value::String(v.clone())))
            .collect();
        Self::validate_enqueue_fields(&req, &tags_json)?;

        // A Timestamp outside the proto spec used to be dropped, so the job ran
        // at once instead of when asked.
        let scheduled_at = timestamp_to_datetime_opt(req.scheduled_at)
            .map_err(|msg| Status::invalid_argument(format!("scheduled_at: {msg}")))?;

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

        // A replay is answered before the quota check. At a full daily cap the
        // limit check used to reject it with RESOURCE_EXHAUSTED even though the
        // job already existed (REST looks the key up first). Whitespace-only is
        // no key, same as REST: storing " " made every such enqueue one job.
        let idempotency_key =
            crate::models::normalize_idempotency_key(Some(req.idempotency_key.clone()));
        if let Some(ref key) = idempotency_key {
            match sqlx::query_as::<_, (String, String)>(
                "SELECT id, queue_name FROM jobs WHERE organization_id = $1 AND idempotency_key = $2",
            )
            .bind(&auth.organization_id)
            .bind(key)
            .fetch_optional(self.pool.as_ref())
            .await
            {
                Ok(Some((existing_id, existing_queue))) => {
                    if !auth.can_access_queue(&existing_queue) {
                        return Err(Status::failed_precondition(
                            crate::models::IDEMPOTENCY_KEY_IN_USE,
                        ));
                    }
                    return Ok(Response::new(EnqueueResponse {
                        job_id: existing_id,
                        created: false,
                    }));
                }
                Ok(None) => {}
                Err(e) => return Err(enqueue_db_error_status(&e)),
            }
        }

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

        // Queue config supplies omitted retry/timeout (0 on the wire), and a job into
        // a queue that does not exist yet is charged against the plan's queue cap —
        // same as REST `POST /jobs`.
        let queue_defaults = queue_job_defaults(
            self.pool.as_ref(),
            &self.queue_defaults,
            &auth.organization_id,
            &req.queue_name,
        )
        .await
        .map_err(|e| {
            error!(error = %e, "Failed to read queue defaults");
            Status::internal("Failed to check limits")
        })?;
        if !queue_defaults.exists {
            check_new_queue_limit_generic(self.pool.as_ref(), &auth.organization_id, 1)
                .await
                .map_err(|e| match e {
                    LimitCheckError::LimitExceeded(err) => {
                        Status::resource_exhausted(err.to_string())
                    }
                    other => {
                        error!(error = %other, "Failed to check queue limit");
                        Status::internal("Failed to check limits")
                    }
                })?;
        }
        // 0 is proto3's omitted int32: the queue default, as when REST omits
        // the field (explicit zero retries are not expressible here). A
        // negative is present on the wire and is rejected like REST's 0-100
        // check, instead of being stored as that default.
        if req.max_retries < 0 {
            return Err(Status::invalid_argument(
                "Max retries must be between 0 and 100",
            ));
        }
        let max_retries = if req.max_retries == 0 {
            queue_defaults.max_retries.clamp(0, 100)
        } else {
            self.resolve_max_retries(req.max_retries)
        };
        let timeout_seconds = if req.timeout_seconds <= 0 {
            queue_defaults.timeout_seconds.clamp(1, 86400)
        } else {
            self.resolve_timeout_seconds(req.timeout_seconds)
        };

        let job_id = uuid::Uuid::new_v4().to_string();
        let now = Utc::now();

        let status = if scheduled_at.is_some_and(|t| t > now) {
            "scheduled"
        } else {
            "pending"
        };

        // Reserve the daily quota BEFORE inserting. Charging after the insert
        // left a pending row that a worker could claim until a best-effort
        // DELETE, and a replay at the cap was RESOURCE_EXHAUSTED. Refund when
        // the insert fails or the row already existed.
        let daily_limit = daily_jobs_limit(self.pool.as_ref(), &auth.organization_id)
            .await
            .unwrap_or(-1);
        let quota_reserved = match try_increment_daily_jobs(
            self.pool.as_ref(),
            &auth.organization_id,
            1,
            daily_limit,
        )
        .await
        {
            Ok(count) if count >= 0 => true,
            Ok(_) => {
                return Err(Status::resource_exhausted("Daily job limit reached"));
            }
            Err(e) => {
                // Counter DB error: accept the job uncounted (best-effort, as before).
                tracing::warn!(error = %e, org_id = %auth.organization_id, "Failed to increment daily job counter");
                false
            }
        };

        // Same transaction as REST: lock max_queues only when the queue is new,
        // and do not return (or touch) a job the key cannot see on conflict.
        let mut tx = match self.pool.begin().await {
            Ok(tx) => tx,
            Err(e) => {
                refund_enqueue_quota(self.pool.as_ref(), &auth.organization_id, quota_reserved)
                    .await;
                return Err(enqueue_db_error_status(&e));
            }
        };
        if !queue_defaults.exists {
            if let Err(e) = lock_resource(&mut tx, &auth.organization_id, "queues").await {
                drop(tx);
                refund_enqueue_quota(self.pool.as_ref(), &auth.organization_id, quota_reserved)
                    .await;
                error!(error = %e, "Failed to lock queue limit");
                return Err(Status::internal("Failed to check limits"));
            }
            let still_new = match queue_name_exists(&mut tx, &auth.organization_id, &req.queue_name)
                .await
            {
                Ok(exists) => !exists,
                Err(e) => {
                    drop(tx);
                    refund_enqueue_quota(self.pool.as_ref(), &auth.organization_id, quota_reserved)
                        .await;
                    error!(error = %e, "Failed to re-check queue existence");
                    return Err(Status::internal("Failed to check limits"));
                }
            };
            if still_new {
                if let Err(e) =
                    check_new_queue_limit_generic_conn(&mut tx, &auth.organization_id, 1).await
                {
                    drop(tx);
                    refund_enqueue_quota(self.pool.as_ref(), &auth.organization_id, quota_reserved)
                        .await;
                    return Err(match e {
                        LimitCheckError::LimitExceeded(err) => {
                            Status::resource_exhausted(err.to_string())
                        }
                        other => {
                            error!(error = %other, "Failed to check queue limit");
                            Status::internal("Failed to check limits")
                        }
                    });
                }
            }
        }

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
            WHERE queue_in_key_scope(jobs.queue_name, $13::TEXT[])
            RETURNING id, (xmax = 0) as created
            "#,
        )
        .bind(&job_id)
        .bind(&auth.organization_id)
        .bind(&req.queue_name)
        .bind(status)
        .bind(&payload)
        .bind(req.priority)
        // proto3 int32 is 0 when the field is omitted; treat 0 as "use the
        // queue's default" (same as REST omit) instead of "no retries".
        // Explicit zero retries are not expressible on gRPC.
        .bind(max_retries)
        .bind(timeout_seconds)
        .bind(scheduled_at)
        .bind(&idempotency_key)
        .bind(&tags_json)
        .bind(now)
        .bind(queue_scope_filter(&auth))
        .fetch_optional(&mut *tx)
        .await;

        let result = match result {
            Ok(Some(row)) => row,
            Ok(None) => {
                drop(tx);
                refund_enqueue_quota(self.pool.as_ref(), &auth.organization_id, quota_reserved)
                    .await;
                return Err(Status::failed_precondition(
                    crate::models::IDEMPOTENCY_KEY_IN_USE,
                ));
            }
            Err(e) => {
                drop(tx);
                refund_enqueue_quota(self.pool.as_ref(), &auth.organization_id, quota_reserved)
                    .await;
                return Err(enqueue_db_error_status(&e));
            }
        };
        if let Err(e) = tx.commit().await {
            refund_enqueue_quota(self.pool.as_ref(), &auth.organization_id, quota_reserved).await;
            return Err(enqueue_db_error_status(&e));
        }

        use sqlx::Row;
        let returned_id: String = result.get("id");
        let created: bool = result.get("created");

        // Update metrics and usage counters
        if created {
            self.metrics.jobs_enqueued.inc();
        } else if quota_reserved {
            if let Err(refund) =
                refund_daily_jobs(self.pool.as_ref(), &auth.organization_id, 1).await
            {
                tracing::warn!(error = %refund, org_id = %auth.organization_id, "Failed to refund an unused daily job reservation");
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
                // Same shape as REST `POST /jobs`: the real initial status (a
                // future `scheduled_at` is `scheduled`) and the priority.
                serde_json::json!({
                    "job_id": returned_id,
                    "queue_name": req.queue_name,
                    "priority": req.priority,
                    "status": status,
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
        let auth =
            authenticate_request_with_cache(self.pool.as_ref(), self.cache.as_ref(), &request)
                .await?;
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

        let batch_size = req.batch_size.clamp(1, MAX_BATCH_SIZE) as usize;
        let lease_duration = self.lease_secs(req.lease_duration_secs);

        let mut jobs: Vec<Job> = Vec::with_capacity(batch_size);
        let mut response_bytes = 0;

        // Claim jobs one by one (atomic operations). tonic checks the 4 MiB
        // response limit only after this handler returns, by which point every
        // job in the batch is leased, so the batch is sized as it grows.
        while jobs.len() < batch_size {
            let job = match self
                .claim_job(
                    &auth.organization_id,
                    &req.queue_name,
                    &req.worker_id,
                    lease_duration,
                )
                .await
            {
                Ok(Some(job)) => job,
                Ok(None) => break, // No more jobs available
                // The jobs claimed so far are leased to this worker already:
                // hand them over instead of stranding them until lease expiry.
                Err(status) if !jobs.is_empty() => {
                    warn!(
                        claimed = jobs.len(),
                        error = %status.message(),
                        "Dequeue batch cut short by a claim error"
                    );
                    break;
                }
                Err(status) => return Err(status),
            };

            let job_proto = job_to_proto(&job);
            let size = prost::encoding::message::encoded_len(1, &job_proto);
            if size > MAX_GRPC_MESSAGE_SIZE {
                dead_letter_oversized(
                    self.pool.as_ref(),
                    &self.metrics,
                    &self.outgoing_webhooks,
                    &job,
                    size,
                )
                .await;
                continue;
            }
            if !jobs.is_empty() && response_bytes + size > MAX_BATCH_RESPONSE_BYTES {
                // Does not fit this response: back to pending, no retry spent.
                release_unsent_claim(
                    self.pool.as_ref(),
                    &auth.organization_id,
                    &job.id,
                    &job_proto.lease_id,
                )
                .await;
                break;
            }

            self.metrics.jobs_processing.inc();
            response_bytes += size;
            jobs.push(job_proto);
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

        Ok(Response::new(DequeueResponse { jobs }))
    }

    /// Complete a job successfully
    async fn complete(
        &self,
        request: Request<CompleteRequest>,
    ) -> Result<Response<CompleteResponse>, Status> {
        let auth =
            authenticate_request_with_cache(self.pool.as_ref(), self.cache.as_ref(), &request)
                .await?;
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

        // Omitted `result` is SQL NULL, matching REST. A present Struct,
        // including `{}`, is stored as that JSON.
        let result_json =
            complete_result_text(req.result.as_ref()).map_err(Status::invalid_argument)?;

        // NOTE: this SQL is byte-identical to QueueManager::complete_by_worker's.
        // sqlx caches prepared statements per connection KEYED BY THE SQL STRING,
        // so $1 must be bound with the same wire type as the REST path (a TEXT
        // JSON string, NULL when omitted, cast by ::JSONB) — binding
        // serde_json::Value (JSONB oid) here poisons whichever path prepares
        // second on a shared pool connection ("invalid input syntax for type json").
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
              AND queue_in_key_scope(queue_name, $6::TEXT[])
            "#,
        )
        .bind(&result_json)
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
        self.settle_signals.settled(&req.job_id);

        info!(job_id = %req.job_id, "Job completed via gRPC");

        type CompletionJobRow = (
            String,
            Option<serde_json::Value>,
            Option<String>,
            Option<String>,
            Option<i64>,
        );
        let job_data: Option<CompletionJobRow> = sqlx::query_as(
            "SELECT queue_name, result, completion_webhook, completion_webhook_secret,              (EXTRACT(EPOCH FROM (completed_at - started_at)) * 1000)::BIGINT              FROM jobs WHERE id = $1 AND organization_id = $2",
        )
        .bind(&req.job_id)
        .bind(&auth.organization_id)
        .fetch_optional(self.pool.as_ref())
        .await
        .unwrap_or(None);
        let (queue_name, result, completion_webhook, completion_webhook_secret, duration_ms) =
            job_data.unwrap_or_else(|| ("unknown".to_string(), None, None, None, None));
        // Same observation as REST complete (spooled_job_duration_seconds).
        if let Some(ms) = duration_ms {
            self.metrics.job_duration.observe(ms.max(0) as f64 / 1000.0);
        }

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
                &auth.organization_id,
                url,
                "job.completed".to_string(),
                serde_json::json!({
                    "job_id": req.job_id,
                    "queue_name": queue_name,
                    "status": "completed",
                    "result": result,
                }),
                completion_webhook_secret.filter(|s| !s.is_empty()),
            );
        }

        Ok(Response::new(CompleteResponse { success: true }))
    }

    /// Fail a job (triggers retry or DLQ)
    async fn fail(&self, request: Request<FailRequest>) -> Result<Response<FailResponse>, Status> {
        let auth =
            authenticate_request_with_cache(self.pool.as_ref(), self.cache.as_ref(), &request)
                .await?;
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

        // Settle the job AND write its dead-letter audit row in ONE statement.
        //
        // These used to be two independent statements on the pool, so a crash or
        // a DB error between them left the job `deadletter` with no
        // `dead_letter_queue` row — and that table is the only place holding
        // `original_payload`/`error_details` once retention deletes the job. The
        // INSERT's error was merely logged, so the RPC reported success while
        // the audit trail silently lost the most interesting failures.
        //
        // The data-modifying CTE also removes a second latent bug: the old
        // INSERT re-read the job with `SELECT ... FROM jobs WHERE id = ...`
        // AFTER the UPDATE committed, so a concurrent purge in that window
        // inserted zero rows and reported no error at all.
        let result: Option<UpdateResult> = sqlx::query_as(
            r#"
            WITH updated AS (
                UPDATE jobs
                SET
                    status = CASE
                        WHEN $1 = FALSE OR retry_count >= max_retries THEN 'deadletter'
                        ELSE 'scheduled'
                    END,
                    last_error = $2,
                    -- Like REST, dead-lettering counts no further retry: the
                    -- DLQ row's total_retries is the retries actually run.
                    retry_count = CASE
                        WHEN $1 = FALSE OR retry_count >= max_retries THEN retry_count
                        ELSE retry_count + 1
                    END,
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
                  AND queue_in_key_scope(queue_name, $7::TEXT[])
                RETURNING
                    id,
                    organization_id,
                    queue_name,
                    payload,
                    status,
                    retry_count,
                    scheduled_at
            ),
            dlq AS (
                -- Parity with REST: DLQ listings must surface gRPC failures too.
                INSERT INTO dead_letter_queue (
                    id, job_id, organization_id, queue_name, reason,
                    original_payload, error_details, created_at
                )
                SELECT
                    gen_random_uuid()::TEXT,
                    u.id,
                    u.organization_id,
                    u.queue_name,
                    $2,
                    u.payload,
                    jsonb_build_object(
                        'final_error', $2::TEXT,
                        'total_retries', u.retry_count
                    ),
                    NOW()
                FROM updated u
                WHERE u.status = 'deadletter'
            )
            SELECT
                u.status as new_status,
                u.retry_count as new_retry_count,
                CASE
                    WHEN u.status = 'scheduled'
                    THEN EXTRACT(EPOCH FROM (u.scheduled_at - NOW()))::BIGINT
                    ELSE NULL
                END as retry_delay_secs
            FROM updated u
            "#,
        )
        // Omitted `retry` (proto3 optional) means "retry per max_retries", like
        // REST; only an explicit false dead-letters immediately.
        .bind(req.retry.unwrap_or(true))
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
        self.settle_signals.settled(&req.job_id);

        let will_retry = result.new_status == "scheduled";
        if result.new_status == "deadletter" {
            // The dead_letter_queue row was written by the same statement that
            // set this status — see the CTE above.
            self.metrics.jobs_deadlettered.inc();
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

            let completion_meta: Option<(Option<String>, Option<String>)> = sqlx::query_as(
                "SELECT completion_webhook, completion_webhook_secret FROM jobs WHERE id = $1 AND organization_id = $2",
            )
            .bind(&req.job_id)
            .bind(&auth.organization_id)
            .fetch_optional(self.pool.as_ref())
            .await
            .ok()
            .flatten();
            if let Some((url, secret)) = completion_meta {
                if let Some(url) = url.filter(|u| !u.is_empty()) {
                    crate::outgoing_webhooks::service::spawn_completion_webhook(
                        &auth.organization_id,
                        url,
                        "job.failed".to_string(),
                        serde_json::json!({
                            "job_id": req.job_id,
                            "queue_name": queue_name,
                            "status": result.new_status,
                            "error": error_message,
                        }),
                        secret.filter(|s| !s.is_empty()),
                    );
                }
            }
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
        let auth =
            authenticate_request_with_cache(self.pool.as_ref(), self.cache.as_ref(), &request)
                .await?;
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
        let extension_secs = self.lease_secs(req.extension_secs);

        // `NOW()` is the transaction clock, after this statement has a
        // connection. A timestamp taken in Rust first goes stale while the
        // pool acquire waits, and a 30s extension can be stored already due.
        let new_expires_at: Option<chrono::DateTime<chrono::Utc>> = sqlx::query_scalar(
            r#"
            UPDATE jobs
            SET lease_expires_at = NOW() + make_interval(secs => $1::int),
                lease_duration_secs = $1,
                updated_at = NOW()
            WHERE id = $2
              AND assigned_worker_id = $3
              AND organization_id = $4
              AND status = 'processing'
              AND lease_expires_at IS NOT NULL
              AND lease_expires_at > NOW()
              AND lease_id = $5
              AND queue_in_key_scope(queue_name, $6::TEXT[])
            RETURNING lease_expires_at
            "#,
        )
        .bind(extension_secs)
        .bind(&req.job_id)
        .bind(&req.worker_id)
        .bind(&auth.organization_id)
        .bind(lease_id)
        .bind(allowed_queues)
        .fetch_optional(self.pool.as_ref())
        .await
        .map_err(|e| {
            error!(error = %e, "Failed to renew lease");
            Status::internal("Failed to renew lease")
        })?;

        let Some(new_expires_at) = new_expires_at else {
            return Err(classify_worker_miss_status(
                self.pool.as_ref(),
                &req.job_id,
                &req.worker_id,
                &auth.organization_id,
                Some(lease_id),
                allowed_queues,
            )
            .await);
        };

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
        let auth =
            authenticate_request_with_cache(self.pool.as_ref(), self.cache.as_ref(), &request)
                .await?;
        self.enforce_rate_limits(&auth.organization_id, &auth.api_key_id, auth.rate_limit)
            .await?;
        let req = request.into_inner();

        if req.job_id.is_empty() {
            return Err(Status::invalid_argument("Job ID is required"));
        }

        let allowed_queues = queue_scope_filter(&auth);
        let job: Option<DbJob> = sqlx::query_as(
            "SELECT * FROM jobs WHERE id = $1 AND organization_id = $2 AND queue_in_key_scope(queue_name, $3::TEXT[])",
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

        let Some(job) = job else {
            return Err(Status::not_found("Job not found"));
        };

        Ok(Response::new(GetJobResponse {
            job: Some(job_to_proto(&job)),
        }))
    }

    /// Get queue statistics
    async fn get_queue_stats(
        &self,
        request: Request<GetQueueStatsRequest>,
    ) -> Result<Response<GetQueueStatsResponse>, Status> {
        let auth =
            authenticate_request_with_cache(self.pool.as_ref(), self.cache.as_ref(), &request)
                .await?;
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
        let auth =
            authenticate_request_with_cache(self.pool.as_ref(), self.cache.as_ref(), &request)
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

        // Every open stream polls the database, so an org gets a bounded number of
        // them. The slot is freed when the stream's task ends.
        let slot = self
            .stream_slots
            .acquire(&auth.organization_id)
            .ok_or_else(|| {
                Status::resource_exhausted(format!(
                    "Too many open StreamJobs streams for this organization (max {})",
                    MAX_STREAMS_PER_ORG
                ))
            })?;

        // Opening the stream counts as one request; after that only delivered
        // jobs are charged (see `run_job_stream`).
        self.enforce_rate_limits(&auth.organization_id, &auth.api_key_id, auth.rate_limit)
            .await?;

        let lease_duration = self.lease_secs(req.lease_duration_secs);
        let org_id = auth.organization_id.clone();
        let worker_id = req.worker_id.clone();

        // One slot. Demand itself is decided in `run_job_stream`: the transport
        // takes messages while the client's HTTP/2 window has room, so an empty
        // slot does not mean the worker wants another job.
        let (tx, rx) = mpsc::channel(1);

        let this = self.clone();
        tokio::spawn(async move {
            let _slot = slot;
            this.run_job_stream(
                tx,
                auth.organization_id,
                auth.api_key_id,
                req.queue_name,
                req.worker_id,
                lease_duration,
            )
            .await;
        });

        let outgoing_webhooks = self.outgoing_webhooks.clone();
        let stream = ReceiverStream::new(rx).map(move |item: Result<BufferedJob, Status>| {
            item.map(|buffered| {
                let job = buffered.deliver();
                outgoing_webhooks.spawn_dispatch(
                    org_id.clone(),
                    "job.started".to_string(),
                    job.id.clone(),
                    serde_json::json!({
                        "job_id": job.id,
                        "queue_name": job.queue_name,
                        "worker_id": worker_id,
                    }),
                );
                job
            })
        });
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
        let auth = authenticate_from_metadata_with_cache(
            self.pool.as_ref(),
            self.cache.as_ref(),
            &metadata,
        )
        .await?;
        let mut stream = request.into_inner();

        let pool = self.pool.clone();
        let metrics = self.metrics.clone();
        let org_id = auth.organization_id.clone();
        let api_key_id = auth.api_key_id.clone();
        let this = self.clone();

        let (tx, rx) = mpsc::channel(16);

        // Spawn background task to handle incoming requests
        tokio::spawn(async move {
            loop {
                let req = match stream.message().await {
                    Ok(Some(req)) => req,
                    Ok(None) => break,
                    // A broken request stream (oversized or malformed message, a
                    // reset) ends the call with its error; ending with OK reads as
                    // a clean close to the client.
                    Err(status) => {
                        let _ = tx.send(Err(status)).await;
                        break;
                    }
                };

                let current_auth = match reload_api_key_context(pool.as_ref(), &api_key_id).await {
                    Ok(auth) => auth,
                    Err(status) => {
                        let response = process_error("UNAUTHENTICATED", status.message());
                        let _ = tx.send(Ok(response)).await;
                        break;
                    }
                };

                // IMPORTANT: enforce rate limits per message, otherwise the stream can bypass limits.
                // The key's limit as it is now: a limit removed mid-stream is gone.
                if let Err(status) = this
                    .enforce_rate_limits(&org_id, &api_key_id, current_auth.rate_limit)
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
                        handle_process_dequeue(
                            &pool,
                            &metrics,
                            &this.outgoing_webhooks,
                            &org_id,
                            &api_key_id,
                            this.lease_secs(dequeue_req.lease_duration_secs),
                            dequeue_req,
                        )
                        .await
                    }
                    Some(crate::grpc::proto::process_request::Request::Complete(complete_req)) => {
                        let job_id = complete_req.job_id.clone();
                        let response = handle_process_complete(
                            &pool,
                            &metrics,
                            &this.outgoing_webhooks,
                            &org_id,
                            &api_key_id,
                            complete_req,
                        )
                        .await;
                        this.settle_signals.settled(&job_id);
                        response
                    }
                    Some(crate::grpc::proto::process_request::Request::Fail(fail_req)) => {
                        let job_id = fail_req.job_id.clone();
                        let response = handle_process_fail(
                            &pool,
                            &metrics,
                            &this.outgoing_webhooks,
                            &org_id,
                            &api_key_id,
                            fail_req,
                        )
                        .await;
                        this.settle_signals.settled(&job_id);
                        response
                    }
                    Some(crate::grpc::proto::process_request::Request::RenewLease(renew_req)) => {
                        let extension_secs = this.lease_secs(renew_req.extension_secs);
                        handle_process_renew(&pool, &org_id, &api_key_id, extension_secs, renew_req)
                            .await
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

impl QueueServiceImpl {
    /// Producer behind one StreamJobs stream. It keeps one job out with the
    /// worker at a time: the next job is claimed only once the worker no longer
    /// holds the previous one (completed, failed, released, or its lease ran
    /// out). Waiting for the consumer to take a message is not enough, since
    /// the HTTP/2 transport takes messages while the client's flow-control
    /// window has room, whether or not the worker asked for them; jobs leased
    /// that way expired unread and spent their retries. The rate limiter is
    /// charged per delivered job rather than per idle poll, and a stream over
    /// its limit backs off instead of ending.
    async fn run_job_stream(
        self,
        tx: mpsc::Sender<Result<BufferedJob, Status>>,
        org_id: String,
        api_key_id: String,
        queue_name: String,
        worker_id: String,
        lease_duration: i32,
    ) {
        let poll_interval = Duration::from_millis(STREAM_POLL_INTERVAL_MS);
        let first_settle_check = Duration::from_millis(STREAM_SETTLE_CHECK_MIN_MS);
        // The last job handed to the worker: (job id, lease id, settle wake-up).
        let mut held: Option<(String, String, SettleWatch)> = None;
        let mut settle_check = first_settle_check;
        loop {
            if let Some((job_id, lease_id, watch)) = held.as_ref() {
                match delivery_still_held(self.pool.as_ref(), &org_id, job_id, lease_id).await {
                    Ok(true) => {
                        tokio::select! {
                            _ = tx.closed() => {
                                debug!(worker = %worker_id, "Stream closed by client");
                                break;
                            }
                            _ = watch.notify.notified() => {}
                            _ = tokio::time::sleep(settle_check) => {
                                settle_check = (settle_check * 2).min(poll_interval);
                            }
                        }
                        continue;
                    }
                    Ok(false) => {
                        held = None;
                        settle_check = first_settle_check;
                    }
                    Err(e) => {
                        error!(error = %e, "Error checking the streamed job");
                        let _ = tx
                            .send(Err(Status::internal("Error polling for jobs")))
                            .await;
                        break;
                    }
                }
            }

            // Wait until the transport has taken the previous message. Fails once
            // the client has gone.
            let Ok(permit) = tx.reserve().await else {
                debug!(worker = %worker_id, "Stream closed by client");
                break;
            };

            // Lock the current key authorization through claim commit so a concurrent
            // revocation or scope narrowing cannot race this protected operation. The
            // locked row also gives the key's and the org's current rate limits.
            let (mut operation_tx, locked_key) = match begin_stream_operation(
                self.pool.as_ref(),
                &api_key_id,
                &org_id,
                Some(&queue_name),
            )
            .await
            {
                Ok(locked) => locked,
                Err(status) => {
                    permit.send(Err(status));
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
                    -- The granted length, so a REST heartbeat that names none keeps it.
                    lease_duration_secs = $3,
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
                            AND queue_in_key_scope(jobs.queue_name, k.queues)
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

            let job = match job_result {
                Ok(Some(job)) => job,
                Ok(None) => {
                    if let Err(e) = operation_tx.commit().await {
                        error!(error = %e, "Failed to commit empty streamed job claim");
                        permit.send(Err(Status::internal("Error polling for jobs")));
                        break;
                    }
                    drop(permit);
                    // No jobs available, wait before polling again
                    if sleep_unless_closed(&tx, poll_interval).await {
                        break;
                    }
                    continue;
                }
                Err(e) => {
                    // tonic ends the response at its first error item.
                    error!(error = %e, "Error polling for jobs in stream");
                    permit.send(Err(Status::internal("Error polling for jobs")));
                    break;
                }
            };

            let job_proto = job_to_proto(&job);
            let size = job_proto.encoded_len();
            if size > MAX_GRPC_MESSAGE_SIZE {
                if let Err(e) = operation_tx.commit().await {
                    error!(error = %e, "Failed to commit streamed job claim");
                    permit.send(Err(Status::internal("Error polling for jobs")));
                    break;
                }
                dead_letter_oversized(
                    self.pool.as_ref(),
                    &self.metrics,
                    &self.outgoing_webhooks,
                    &job,
                    size,
                )
                .await;
                continue;
            }

            // Charged per delivered job, against the key's current limit. Over the
            // limit, the claim is rolled back (the job stays pending, retries
            // intact) and the stream waits instead of ending. Redis only: the
            // transaction holds a pool connection.
            if let Err(status) = self
                .charge_stream_claim(&org_id, &api_key_id, &locked_key)
                .await
            {
                let _ = operation_tx.rollback().await;
                drop(permit);
                debug!(worker = %worker_id, reason = %status.message(), "StreamJobs held back by the rate limit");
                if sleep_unless_closed(&tx, poll_interval).await {
                    break;
                }
                continue;
            }

            if let Err(e) = operation_tx.commit().await {
                error!(error = %e, "Failed to commit streamed job claim");
                permit.send(Err(Status::internal("Error polling for jobs")));
                break;
            }
            self.metrics.jobs_processing.inc();
            debug!(job_id = %job.id, queue = %queue_name, "Streaming job to worker");

            let watch = self.settle_signals.watch(&job.id);
            held = Some((job.id.clone(), lease_id, watch));
            permit.send(Ok(BufferedJob {
                job: Some(job_proto),
                pool: Arc::clone(&self.pool),
                metrics: Arc::clone(&self.metrics),
                org_id: org_id.clone(),
            }));
        }
    }
}

/// Whether the job a stream handed out is still with its worker: `processing`
/// under the lease it was delivered with, and the lease has not run out.
async fn delivery_still_held(
    pool: &PgPool,
    org_id: &str,
    job_id: &str,
    lease_id: &str,
) -> Result<bool, sqlx::Error> {
    let (held,): (bool,) = sqlx::query_as(
        r#"
        SELECT EXISTS (
            SELECT 1 FROM jobs
            WHERE id = $1
              AND organization_id = $2
              AND lease_id = $3
              AND status = 'processing'
              AND lease_expires_at > NOW()
        )
        "#,
    )
    .bind(job_id)
    .bind(org_id)
    .bind(lease_id)
    .fetch_one(pool)
    .await?;
    Ok(held)
}

fn queue_scope_filter(auth: &GrpcAuthContext) -> Option<&[String]> {
    queue_scope_filter_from_queues(&auth.queues)
}

/// The scope to bind for `queue_in_key_scope(queue_name, $n::TEXT[])`: `None`
/// (all queues) for an unrestricted key, else the key's list, `prefix.*`
/// entries included.
fn queue_scope_filter_from_queues(queues: &[String]) -> Option<&[String]> {
    if crate::models::queue_scope_is_unrestricted(queues) {
        None
    } else {
        Some(queues)
    }
}

/// What a stream operation read from its locked key row.
struct LockedStreamKey {
    /// The key's current per-minute rate limit.
    key_rate_limit: Option<i32>,
    /// The org's current plan rate limits.
    org_limits: OrgRateLimits,
}

/// Open a stream operation's transaction with the key's row locked `FOR SHARE`
/// (still active, not expired, org not deleted, queue in scope). Returns the
/// key's and the org's current rate limits along with the transaction, so
/// nothing under the transaction needs a second pool connection to find them.
async fn begin_stream_operation<'a>(
    pool: &'a PgPool,
    api_key_id: &str,
    org_id: &str,
    queue_name: Option<&str>,
) -> Result<(Transaction<'a, Postgres>, LockedStreamKey), Status> {
    let mut tx = pool.begin().await.map_err(|e| {
        error!(error = %e, "Failed to begin stream operation");
        Status::internal("Failed to begin stream operation")
    })?;

    type KeyRow = (Vec<String>, Option<i32>, String, Option<serde_json::Value>);
    let key: Option<KeyRow> = sqlx::query_as(
        r#"
        SELECT k.queues, k.rate_limit, o.plan_tier, o.custom_limits
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

    let (queues, key_rate_limit, plan_tier, custom_limits) =
        key.ok_or_else(|| Status::unauthenticated("API key revoked or expired"))?;
    if let Some(queue_name) = queue_name {
        if !crate::models::queue_scope_allows(&queues, queue_name) {
            return Err(Status::permission_denied(format!(
                "API key does not have permission for queue '{}'",
                queue_name
            )));
        }
    }

    Ok((
        tx,
        LockedStreamKey {
            key_rate_limit,
            org_limits: OrgRateLimits::from_plan(&plan_tier, custom_limits.as_ref()),
        },
    ))
}

/// Wait `delay`, or less if the stream's consumer goes away; `true` when it did.
async fn sleep_unless_closed<T>(tx: &mpsc::Sender<T>, delay: Duration) -> bool {
    tokio::select! {
        _ = tx.closed() => true,
        _ = tokio::time::sleep(delay) => false,
    }
}

async fn refund_enqueue_quota(pool: &PgPool, org_id: &str, reserved: bool) {
    if !reserved {
        return;
    }
    if let Err(refund) = refund_daily_jobs(pool, org_id, 1).await {
        tracing::warn!(error = %refund, org_id, "Failed to refund an unused daily job reservation");
    }
}

/// Map an Enqueue INSERT error. A CHECK constraint (23514) or a value too big
/// for an index row (54000) is the request's fault: INVALID_ARGUMENT, which
/// SDKs do not retry, rather than INTERNAL.
fn enqueue_db_error_status(e: &sqlx::Error) -> Status {
    if let Some(db) = e.as_database_error() {
        if matches!(db.code().as_deref(), Some("23514") | Some("54000")) {
            return Status::invalid_argument(format!("Job rejected: {}", db.message()));
        }
    }
    error!(error = %e, "Failed to enqueue job");
    Status::internal("Failed to enqueue job")
}

/// Undo a claim whose job never reached the worker: back to `pending` with the
/// lease cleared and `retry_count` untouched. Fenced on the claim's `lease_id`,
/// so a job settled or claimed again since is left alone. Returns whether the
/// job was released.
async fn release_unsent_claim(pool: &PgPool, org_id: &str, job_id: &str, lease_id: &str) -> bool {
    let released = sqlx::query(
        r#"
        UPDATE jobs
        SET
            status = 'pending',
            assigned_worker_id = NULL,
            lease_id = NULL,
            lease_expires_at = NULL,
            updated_at = NOW()
        WHERE id = $1
          AND organization_id = $2
          AND lease_id = $3
          AND status = 'processing'
        "#,
    )
    .bind(job_id)
    .bind(org_id)
    .bind(lease_id)
    .execute(pool)
    .await;

    match released {
        Ok(done) => {
            let released = done.rows_affected() > 0;
            if released {
                debug!(job_id = %job_id, "Released an undelivered job claim");
            }
            released
        }
        Err(e) => {
            warn!(error = %e, job_id = %job_id, "Failed to release an undelivered job claim; it returns when its lease expires");
            false
        }
    }
}

/// Dead-letter a job this call just claimed whose encoded form (`size` bytes)
/// cannot fit in one gRPC message. Handing it out would fail in tonic's encoder
/// after the claim, and since it heads the queue every later claim would pick
/// it again, so no gRPC consumer could get past it. Fenced on the claim's
/// lease; `retry_count` is kept, as no attempt ran. The job stays readable
/// over REST.
///
/// This is the server's decision, not an outcome the worker reported, so it
/// must not count on `workers.jobs_failed`: the counter trigger (migration
/// 20261014110000) counts a move out of `processing` only while the lease is
/// live. One transaction therefore first expires the lease
/// (`lease_expires_at = NOW()`, the transaction's start, fenced on the
/// `lease_id`) and then dead-letters the job, whose old row then carries no
/// live lease.
async fn dead_letter_oversized(
    pool: &PgPool,
    metrics: &Metrics,
    outgoing_webhooks: &Arc<OutgoingWebhookService>,
    job: &DbJob,
    size: usize,
) {
    let reason = format!(
        "Job too large for a gRPC message: {} bytes encoded (max {} bytes)",
        size, MAX_GRPC_MESSAGE_SIZE
    );
    let moved = async {
        let mut tx = pool.begin().await?;
        let expired = sqlx::query(
            r#"
            UPDATE jobs
            SET lease_expires_at = NOW()
            WHERE id = $1
              AND organization_id = $2
              AND lease_id = $3
              AND status = 'processing'
            "#,
        )
        .bind(&job.id)
        .bind(&job.organization_id)
        .bind(&job.lease_id)
        .execute(&mut *tx)
        .await?;
        if expired.rows_affected() == 0 {
            // Settled or claimed again in the meantime; dropping the
            // transaction rolls it back.
            return Ok::<u64, sqlx::Error>(0);
        }
        let moved = sqlx::query(
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
                WHERE id = $2
                  AND organization_id = $3
                  AND lease_id = $4
                  AND status = 'processing'
                RETURNING id, organization_id, queue_name, payload, retry_count
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
                jsonb_build_object('final_error', $1::TEXT, 'total_retries', d.retry_count),
                NOW()
            FROM deadlettered d
            "#,
        )
        .bind(&reason)
        .bind(&job.id)
        .bind(&job.organization_id)
        .bind(&job.lease_id)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(moved.rows_affected())
    }
    .await;

    match moved {
        Ok(moved) if moved > 0 => {}
        // Settled or claimed again in the meantime.
        Ok(_) => return,
        Err(e) => {
            error!(error = %e, job_id = %job.id, "Failed to dead-letter a job too large for gRPC; it returns when its lease expires");
            return;
        }
    }

    metrics.jobs_failed.inc();
    metrics.jobs_deadlettered.inc();
    warn!(
        job_id = %job.id,
        queue = %job.queue_name,
        encoded_bytes = size,
        "Dead-lettered a job too large for a gRPC message"
    );

    let event = serde_json::json!({
        "job_id": job.id,
        "queue_name": job.queue_name,
        "status": "deadletter",
        "error": reason,
    });
    outgoing_webhooks.spawn_dispatch(
        job.organization_id.clone(),
        "job.failed".to_string(),
        job.id.clone(),
        event.clone(),
    );
    if let Some(url) = job.completion_webhook.clone().filter(|u| !u.is_empty()) {
        crate::outgoing_webhooks::service::spawn_completion_webhook(
            &job.organization_id,
            url,
            "job.failed".to_string(),
            event,
            job.completion_webhook_secret
                .clone()
                .filter(|s| !s.is_empty()),
        );
    }
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
    // Same rules as REST: empty becomes "Job failed", long text is cut.
    crate::models::normalize_failure_message(message)
}

/// Handle dequeue request in ProcessJobs stream. `lease_duration` is already
/// resolved by [`QueueServiceImpl::lease_secs`].
async fn handle_process_dequeue(
    pool: &PgPool,
    metrics: &Metrics,
    outgoing_webhooks: &Arc<OutgoingWebhookService>,
    org_id: &str,
    api_key_id: &str,
    lease_duration: i32,
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

    // A job too large for one response message is dead-lettered and the next
    // one tried. Each pass takes a job out of the queue, so this ends.
    loop {
        let (mut tx, _) =
            match begin_stream_operation(pool, api_key_id, org_id, Some(&req.queue_name)).await {
                Ok(locked) => locked,
                Err(status) => return process_status_error(&status),
            };
        let lease_id = uuid::Uuid::new_v4().to_string();

        let job: Result<Option<DbJob>, _> = sqlx::query_as(
            r#"
            UPDATE jobs
            SET
                status = 'processing',
                assigned_worker_id = $1,
                lease_id = $2,
                lease_expires_at = NOW() + ($3 || ' seconds')::INTERVAL,
                -- The granted length, so a REST heartbeat that names none keeps it.
                lease_duration_secs = $3,
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
                let job_proto = job_to_proto(&job);
                let size = prost::encoding::message::encoded_len(1, &job_proto);
                if size > MAX_GRPC_MESSAGE_SIZE {
                    dead_letter_oversized(pool, metrics, outgoing_webhooks, &job, size).await;
                    continue;
                }
                metrics.jobs_processing.inc();
                outgoing_webhooks.spawn_dispatch(
                    org_id.to_string(),
                    "job.started".to_string(),
                    job.id.clone(),
                    serde_json::json!({
                        "job_id": job.id,
                        "queue_name": job.queue_name,
                        "worker_id": req.worker_id,
                    }),
                );
                return ProcessResponse {
                    response: Some(crate::grpc::proto::process_response::Response::Job(
                        job_proto,
                    )),
                };
            }
            Ok(None) => {
                if let Err(e) = tx.commit().await {
                    error!(error = %e, "Failed to commit empty ProcessJobs dequeue");
                    return process_error("INTERNAL", "Failed to dequeue job");
                }
                return ProcessResponse {
                    response: Some(crate::grpc::proto::process_response::Response::Error(
                        crate::grpc::proto::ErrorResponse {
                            code: "NO_JOBS".to_string(),
                            message: "No jobs available".to_string(),
                        },
                    )),
                };
            }
            Err(e) => {
                error!(error = %e, "Failed to dequeue in ProcessJobs");
                return ProcessResponse {
                    response: Some(crate::grpc::proto::process_response::Response::Error(
                        crate::grpc::proto::ErrorResponse {
                            code: "INTERNAL".to_string(),
                            message: "Failed to dequeue job".to_string(),
                        },
                    )),
                };
            }
        }
    }
}

/// REST `lease_mismatch_error` (`jobs.rs`). Same failed-precondition class
/// as a timeout, but the text says the token was wrong while the lease was
/// still live.
const LEASE_MISMATCH_MESSAGE: &str =
    "lease_id does not match the job's current lease; this worker no longer holds it";

/// Why a still-`processing` row's worker UPDATE missed, when it is a lease
/// problem. An expired (or missing) lease is "Lease expired", even if the
/// token also differs — REST checks expiry first. A live lease whose token
/// is not the one presented is [`LEASE_MISMATCH_MESSAGE`]. `None` means the
/// row looked eligible, so the miss is reported as not found.
fn processing_lease_miss(
    lease: Option<chrono::DateTime<Utc>>,
    current_lease_id: Option<&str>,
    presented: Option<&str>,
) -> Option<&'static str> {
    let expired = lease.map(|t| t <= Utc::now()).unwrap_or(true);
    if expired {
        return Some("Lease expired");
    }
    let fenced_out = match presented {
        Some(token) => current_lease_id != Some(token),
        None => false,
    };
    if fenced_out {
        Some(LEASE_MISMATCH_MESSAGE)
    } else {
        None
    }
}

/// JSON text for `jobs.result` (`$1::JSONB`). An omitted Complete `result`
/// is SQL NULL, matching REST `complete_by_worker`. A present Struct,
/// including an empty one, is that JSON. The value stays a TEXT string so
/// this SQL can share a prepared statement with the REST path (a JSONB bind
/// poisons whichever path prepares second).
fn complete_result_text(result: Option<&prost_types::Struct>) -> Result<Option<String>, String> {
    let Some(result) = result else {
        return Ok(None);
    };
    let result_str = serde_json::to_string(&struct_to_json(result)).unwrap_or_default();
    if result_str.len() > MAX_RESULT_SIZE {
        return Err(format!(
            "Result too large: {} bytes (max: {} bytes)",
            result_str.len(),
            MAX_RESULT_SIZE
        ));
    }
    Ok(Some(result_str))
}

/// Classify why a worker-scoped UPDATE affected zero rows into a
/// `tonic::Status` for unary RPCs. Returns `FailedPrecondition`
/// (LEASE_EXPIRED) when a matching row exists but its lease is
/// null/past or was requeued, `FailedPrecondition` with the REST mismatch
/// text when the presented token is not the live lease, `FailedPrecondition`
/// "Job is already <status>" when it already finished (REST 409), and
/// `NotFound` otherwise.
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
          AND queue_in_key_scope(queue_name, $4::TEXT[])
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
            match processing_lease_miss(lease, current_lease_id.as_deref(), lease_id) {
                Some(message) => Status::failed_precondition(message),
                None => Status::not_found("Job not found or not authorized"),
            }
        }
        // Requeued after its lease ran out: the worker lost the job.
        Ok(Some((status, _, _))) if status == "pending" || status == "scheduled" => {
            Status::failed_precondition("Lease expired")
        }
        // Completed, failed, dead-lettered or cancelled, e.g. a retried Complete
        // whose first response was lost. REST answers 409 with the same text.
        Ok(Some((status, _, _))) => {
            Status::failed_precondition(format!("Job is already {}", status))
        }
        Ok(None) => Status::not_found("Job not found or not authorized"),
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
                AND queue_in_key_scope(jobs.queue_name, k.queues)
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
            match processing_lease_miss(lease, current_lease_id.as_deref(), lease_id) {
                Some(message) => ("LEASE_EXPIRED", message.to_string()),
                None => ("NOT_FOUND", "Job not found or not authorized".to_string()),
            }
        }
        // Requeued after its lease ran out: the worker lost the job.
        Ok(Some((status, _, _))) if status == "pending" || status == "scheduled" => {
            ("LEASE_EXPIRED", "Lease expired".to_string())
        }
        // Already finished: same code and text as REST's 409.
        Ok(Some((status, _, _))) => ("CONFLICT", format!("Job is already {}", status)),
        Ok(None) => ("NOT_FOUND", "Job not found or not authorized".to_string()),
        Err(e) => {
            error!(error = %e, job_id = %job_id, "Failed to classify worker miss in ProcessJobs");
            ("INTERNAL", "Failed to check job state".to_string())
        }
    };

    process_error(code, &message)
}

/// Handle complete request in ProcessJobs stream
async fn handle_process_complete(
    pool: &PgPool,
    metrics: &Metrics,
    outgoing_webhooks: &Arc<OutgoingWebhookService>,
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
    // Same NULL-vs-JSON rule as the unary Complete (and REST).
    let result_json = match complete_result_text(req.result.as_ref()) {
        Ok(text) => text,
        Err(message) => {
            return ProcessResponse {
                response: Some(crate::grpc::proto::process_response::Response::Error(
                    crate::grpc::proto::ErrorResponse {
                        code: "INVALID_ARGUMENT".to_string(),
                        message,
                    },
                )),
            };
        }
    };

    let (mut tx, _) = match begin_stream_operation(pool, api_key_id, org_id, None).await {
        Ok(locked) => locked,
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
                AND queue_in_key_scope(jobs.queue_name, k.queues)
          )
        "#,
    )
    .bind(&result_json)
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

            type CompletionJobRow = (
                String,
                Option<serde_json::Value>,
                Option<String>,
                Option<String>,
            );
            let job_data: Option<CompletionJobRow> = sqlx::query_as(
                "SELECT queue_name, result, completion_webhook, completion_webhook_secret FROM jobs WHERE id = $1 AND organization_id = $2",
            )
            .bind(&req.job_id)
            .bind(org_id)
            .fetch_optional(pool)
            .await
            .unwrap_or(None);
            let (queue_name, result_payload, completion_webhook, completion_webhook_secret) =
                job_data.unwrap_or_else(|| ("unknown".to_string(), None, None, None));

            outgoing_webhooks.spawn_dispatch(
                org_id.to_string(),
                "job.completed".to_string(),
                req.job_id.clone(),
                serde_json::json!({
                    "job_id": req.job_id,
                    "queue_name": queue_name,
                    "status": "completed",
                    "result": result_payload,
                }),
            );
            if let Some(url) = completion_webhook.filter(|u| !u.is_empty()) {
                crate::outgoing_webhooks::service::spawn_completion_webhook(
                    org_id,
                    url,
                    "job.completed".to_string(),
                    serde_json::json!({
                        "job_id": req.job_id,
                        "queue_name": queue_name,
                        "status": "completed",
                        "result": result_payload,
                    }),
                    completion_webhook_secret.filter(|s| !s.is_empty()),
                );
            }

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
    outgoing_webhooks: &Arc<OutgoingWebhookService>,
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
    let (mut tx, _) = match begin_stream_operation(pool, api_key_id, org_id, None).await {
        Ok(locked) => locked,
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
                ELSE 'scheduled'
            END,
            last_error = $2,
            -- Like REST, dead-lettering counts no further retry.
            retry_count = CASE
                WHEN $1 = FALSE OR retry_count >= max_retries THEN retry_count
                ELSE retry_count + 1
            END,
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
                AND queue_in_key_scope(jobs.queue_name, k.queues)
          )
        RETURNING
            status as new_status,
            retry_count as new_retry_count,
            CASE
                WHEN status = 'scheduled'
                THEN EXTRACT(EPOCH FROM (scheduled_at - NOW()))::BIGINT
                ELSE NULL
            END as retry_delay_secs
        "#,
    )
    .bind(req.retry.unwrap_or(true))
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
            let will_retry = update_result.new_status == "scheduled";
            if update_result.new_status == "deadletter" {
                // Parity with REST: insert into dead_letter_queue.
                //
                // This runs INSIDE the transaction, so a failure here must abort
                // the settlement rather than be logged and stepped over: in
                // Postgres a failed statement poisons the surrounding
                // transaction, and the previous log-and-continue therefore
                // guaranteed that the `tx.commit()` below would fail anyway —
                // rolling back the status update too and returning INTERNAL,
                // while the log line claimed only the audit row was lost.
                // Returning here makes that outcome explicit and releases the
                // transaction immediately.
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
                    error!(error = %e, job_id = %req.job_id, "Failed to insert dead_letter_queue row in ProcessJobs; rolling back settlement");
                    let _ = tx.rollback().await;
                    return process_error("INTERNAL", "Failed to update job");
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

            if update_result.new_status == "deadletter" || update_result.new_status == "failed" {
                let job_data: Option<(String, Option<String>, Option<String>)> = sqlx::query_as(
                    "SELECT queue_name, completion_webhook, completion_webhook_secret FROM jobs WHERE id = $1 AND organization_id = $2",
                )
                .bind(&req.job_id)
                .bind(org_id)
                .fetch_optional(pool)
                .await
                .unwrap_or(None);
                let (queue_name, completion_webhook, completion_webhook_secret) =
                    job_data.unwrap_or_else(|| ("unknown".to_string(), None, None));

                outgoing_webhooks.spawn_dispatch(
                    org_id.to_string(),
                    "job.failed".to_string(),
                    req.job_id.clone(),
                    serde_json::json!({
                        "job_id": req.job_id,
                        "queue_name": queue_name,
                        "status": update_result.new_status,
                        "error": error_message,
                    }),
                );
                if let Some(url) = completion_webhook.filter(|u| !u.is_empty()) {
                    crate::outgoing_webhooks::service::spawn_completion_webhook(
                        org_id,
                        url,
                        "job.failed".to_string(),
                        serde_json::json!({
                            "job_id": req.job_id,
                            "queue_name": queue_name,
                            "status": update_result.new_status,
                            "error": error_message,
                        }),
                        completion_webhook_secret.filter(|s| !s.is_empty()),
                    );
                }
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

/// Handle renew lease request in ProcessJobs stream. `extension_secs` is
/// already resolved by [`QueueServiceImpl::lease_secs`].
async fn handle_process_renew(
    pool: &PgPool,
    org_id: &str,
    api_key_id: &str,
    extension_secs: i32,
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
    // Same clock as unary RenewLease: the expiry is Postgres `NOW()` once
    // `begin_stream_operation` has a connection, not a Rust timestamp taken
    // while that acquire is still waiting.
    let (mut tx, _) = match begin_stream_operation(pool, api_key_id, org_id, None).await {
        Ok(locked) => locked,
        Err(status) => return process_status_error(&status),
    };

    let result: Result<Option<chrono::DateTime<chrono::Utc>>, sqlx::Error> = sqlx::query_scalar(
        r#"
        UPDATE jobs
        SET lease_expires_at = NOW() + make_interval(secs => $1::int),
            lease_duration_secs = $1,
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
                AND queue_in_key_scope(jobs.queue_name, k.queues)
          )
        RETURNING lease_expires_at
        "#,
    )
    .bind(extension_secs)
    .bind(&req.job_id)
    .bind(&req.worker_id)
    .bind(org_id)
    .bind(QueueServiceImpl::opt_lease_id(&req.lease_id))
    .bind(api_key_id)
    .fetch_optional(&mut *tx)
    .await;

    match result {
        Ok(Some(new_expires_at)) => {
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
            retry: Some(true),
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
                retry: Some(false),
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
    fn processing_lease_miss_distinguishes_a_live_mismatch_from_expiry() {
        let future = Utc::now() + chrono::Duration::minutes(5);
        let past = Utc::now() - chrono::Duration::minutes(5);
        assert_eq!(
            processing_lease_miss(Some(future), Some("real"), Some("other")),
            Some(LEASE_MISMATCH_MESSAGE)
        );
        assert_eq!(
            processing_lease_miss(Some(past), Some("real"), Some("real")),
            Some("Lease expired")
        );
        // Expiry wins when the token is also wrong, same as REST.
        assert_eq!(
            processing_lease_miss(Some(past), Some("real"), Some("other")),
            Some("Lease expired")
        );
        assert_eq!(
            processing_lease_miss(None, Some("real"), Some("other")),
            Some("Lease expired")
        );
        assert_eq!(
            processing_lease_miss(Some(future), Some("real"), Some("real")),
            None
        );
        assert_eq!(
            processing_lease_miss(Some(future), Some("real"), None),
            None
        );
    }

    #[test]
    fn complete_without_result_is_sql_null_not_empty_object() {
        assert_eq!(complete_result_text(None).unwrap(), None);
        let empty = crate::grpc::convert::json_to_struct(&serde_json::json!({})).unwrap();
        assert_eq!(
            complete_result_text(Some(&empty)).unwrap().as_deref(),
            Some("{}")
        );
        let value = crate::grpc::convert::json_to_struct(&serde_json::json!({"ok": true})).unwrap();
        assert_eq!(
            complete_result_text(Some(&value)).unwrap().as_deref(),
            Some("{\"ok\":true}")
        );
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

    // BEGRPC-12: the per-org stream cap counts open streams and frees a slot
    // when its holder goes away.
    #[test]
    fn test_stream_slots_cap_open_streams_per_org() {
        let slots = Arc::new(StreamSlots::default());
        let mut held: Vec<_> = (0..MAX_STREAMS_PER_ORG)
            .map(|_| slots.acquire("org-a").expect("under the cap"))
            .collect();
        assert!(slots.acquire("org-a").is_none());
        let _other = slots.acquire("org-b").expect("the cap is per org");

        held.pop();
        assert!(slots.acquire("org-a").is_some());
        held.clear();
        assert!(!slots.0.lock().unwrap().contains_key("org-a"));
    }

    // BEGRPC-09: the REST CreateJobRequest limits, at and past each edge.
    #[test]
    fn test_validate_enqueue_fields() {
        let req = |queue: &str, priority: i32, key: &str| EnqueueRequest {
            queue_name: queue.to_string(),
            payload: None,
            priority,
            max_retries: 0,
            timeout_seconds: 0,
            scheduled_at: None,
            idempotency_key: key.to_string(),
            tags: Default::default(),
        };
        let no_tags = serde_json::json!({});
        let check = |r: &EnqueueRequest, tags: &serde_json::Value| {
            QueueServiceImpl::validate_enqueue_fields(r, tags).map_err(|s| s.code())
        };
        let invalid = Err(tonic::Code::InvalidArgument);

        assert_eq!(
            check(&req(&"q".repeat(100), 100, &"k".repeat(255)), &no_tags),
            Ok(())
        );
        assert_eq!(check(&req("q", -100, ""), &no_tags), Ok(()));
        assert_eq!(check(&req(&"q".repeat(101), 0, ""), &no_tags), invalid);
        assert_eq!(check(&req("q", 101, ""), &no_tags), invalid);
        assert_eq!(check(&req("q", -101, ""), &no_tags), invalid);
        assert_eq!(check(&req("q", 0, &"k".repeat(256)), &no_tags), invalid);
        // Characters, not bytes, like the REST validator.
        assert_eq!(
            check(&req(&"é".repeat(100), 0, &"é".repeat(255)), &no_tags),
            Ok(())
        );

        let tags = |len: usize| serde_json::json!({ "t": "x".repeat(len) });
        // {"t":"..."} is 8 bytes of JSON around the value.
        assert_eq!(check(&req("q", 0, ""), &tags(MAX_TAGS_SIZE - 8)), Ok(()));
        assert_eq!(check(&req("q", 0, ""), &tags(MAX_TAGS_SIZE - 7)), invalid);
    }

    // W3R-02: a settle in this process wakes the stream waiting on that job,
    // even when it lands before the stream starts waiting; a watch replaced
    // by a newer one for the same job does not unregister it.
    #[tokio::test]
    async fn settle_signals_wake_the_stream_waiting_on_the_job() {
        let signals = Arc::new(SettleSignals::default());
        let watch = signals.watch("job-1");
        signals.settled("job-2");
        signals.settled("job-1");
        tokio::time::timeout(Duration::from_secs(1), watch.notify.notified())
            .await
            .expect("woken by its own job's settle");

        let older = signals.watch("job-3");
        let newer = signals.watch("job-3");
        drop(older);
        signals.settled("job-3");
        tokio::time::timeout(Duration::from_secs(1), newer.notify.notified())
            .await
            .expect("the newer watch is still registered");
        drop(newer);
        drop(watch);
        assert!(
            signals.0.lock().unwrap().is_empty(),
            "dropped watches unregister"
        );
    }
}
