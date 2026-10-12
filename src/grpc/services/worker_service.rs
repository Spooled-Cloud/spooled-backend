//! WorkerService gRPC implementation
//!
//! Implements worker lifecycle management: Register, Heartbeat, Deregister.

// tonic::Status is a standard gRPC error type, size is acceptable
#![allow(clippy::result_large_err)]

use std::collections::HashMap;
use std::sync::Arc;

use chrono::Utc;
use sqlx::PgPool;
use tonic::{Request, Response, Status};
use tracing::{info, warn};

use crate::api::middleware::limits::{check_resource_limit_conn, lock_resource};
use crate::cache::RedisCache;
use crate::config::PlanLimits;
use crate::grpc::auth::authenticate_request_with_cache;
use crate::grpc::proto::{
    worker_service_server::WorkerService, DeregisterRequest, DeregisterResponse, HeartbeatRequest,
    HeartbeatResponse, RegisterWorkerRequest, RegisterWorkerResponse,
};
use crate::models::WORKER_ID_UNAVAILABLE;
use crate::observability::Metrics;

/// Maximum concurrent jobs a worker can declare (REST's upper bound).
///
/// Larger values are clamped rather than rejected: gRPC used to accept up to
/// 1000, and failing Register would take down fleets that send such values.
const MAX_CONCURRENT_JOBS: i32 = 100;

/// Minimum concurrent jobs
const MIN_CONCURRENT_JOBS: i32 = 1;

/// Concurrency used when the request leaves `max_concurrency` at 0 (REST default)
const DEFAULT_CONCURRENT_JOBS: i32 = 5;

/// Highest `current_jobs` a heartbeat can report (REST's upper bound)
const MAX_CURRENT_JOBS: i32 = 1000;

/// Maximum hostname length
const MAX_HOSTNAME_LENGTH: usize = 255;

/// Maximum `worker_type` and `version` length, in characters (as REST)
const MAX_WORKER_TEXT_LENGTH: usize = 50;

/// Valid worker statuses
const VALID_WORKER_STATUSES: &[&str] = &["healthy", "degraded", "draining", "offline"];

/// WorkerService implementation
#[derive(Clone)]
pub struct WorkerServiceImpl {
    pool: Arc<PgPool>,
    metrics: Arc<Metrics>,
    cache: Option<RedisCache>,
    outgoing_webhooks: Arc<crate::outgoing_webhooks::service::OutgoingWebhookService>,
    /// Mirror REST `RATE_LIMIT_FAIL_CLOSED`.
    rate_limit_fail_closed: bool,
    /// `Register` reports `GRPC_DEFAULT_LEASE_DURATION_SECS` (the lease an
    /// omitted gRPC lease gets) and `WORKER_HEARTBEAT_INTERVAL_SECS`.
    worker_settings: crate::config::WorkerSettings,
}

impl WorkerServiceImpl {
    pub fn new(
        pool: Arc<PgPool>,
        metrics: Arc<Metrics>,
        cache: Option<RedisCache>,
        outgoing_webhooks: Arc<crate::outgoing_webhooks::service::OutgoingWebhookService>,
        rate_limit_fail_closed: bool,
        worker_settings: crate::config::WorkerSettings,
    ) -> Self {
        Self {
            pool,
            metrics,
            cache,
            outgoing_webhooks,
            rate_limit_fail_closed,
            worker_settings,
        }
    }

    /// Same policy as QueueService: a Redis error allows the call unless
    /// `RATE_LIMIT_FAIL_CLOSED` is set, and fail-closed without Redis denies.
    async fn enforce_rate_limits(
        &self,
        org_id: &str,
        api_key_id: &str,
        api_key_per_minute: Option<i32>,
    ) -> Result<(), Status> {
        let Some(ref cache) = self.cache else {
            if self.rate_limit_fail_closed {
                return Err(Status::resource_exhausted(
                    "Rate limit unavailable (Redis not configured)",
                ));
            }
            // Redis optional for self-host; can't enforce distributed rate limits.
            return Ok(());
        };

        #[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
        struct OrgRateLimits {
            rps: u32,
            burst: u32,
        }

        let cache_key = crate::api::middleware::plan_rate_limit::org_rate_limits_key(org_id);
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
                        tracing::error!(
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

        Ok(())
    }

    /// Validate hostname format
    fn validate_hostname(hostname: &str) -> Result<(), Status> {
        if hostname.len() > MAX_HOSTNAME_LENGTH {
            return Err(Status::invalid_argument("Hostname too long"));
        }
        if !hostname
            .chars()
            .all(|c| c.is_alphanumeric() || c == '-' || c == '.' || c == '_')
        {
            return Err(Status::invalid_argument(
                "Hostname can only contain alphanumeric characters, dots, dashes, and underscores",
            ));
        }
        if hostname.starts_with('.') || hostname.starts_with('-') {
            return Err(Status::invalid_argument(
                "Hostname cannot start with dots or dashes",
            ));
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
        Ok(())
    }

    /// `worker_type` / `version` are capped like REST (50 characters).
    fn validate_worker_text(field: &str, value: &str) -> Result<(), Status> {
        if value.chars().count() > MAX_WORKER_TEXT_LENGTH {
            return Err(Status::invalid_argument(format!(
                "{} must be at most {} characters",
                field, MAX_WORKER_TEXT_LENGTH
            )));
        }
        Ok(())
    }

    /// The caller's stable worker id, or `None` to mint one. Empty counts as
    /// omitted; anything else follows the REST rules (1-128 safe characters).
    fn requested_worker_id(worker_id: Option<&str>) -> Result<Option<String>, Status> {
        match worker_id {
            None | Some("") => Ok(None),
            Some(id) => {
                if id.chars().count() > 128 {
                    return Err(Status::invalid_argument(
                        "Worker ID must be 1-128 characters",
                    ));
                }
                crate::models::validate_worker_id(id).map_err(|e| {
                    Status::invalid_argument(
                        e.message
                            .map(|m| m.to_string())
                            .unwrap_or_else(|| "Invalid worker ID".to_string()),
                    )
                })?;
                Ok(Some(id.to_string()))
            }
        }
    }

    /// Clamp `max_concurrency` into REST's range; 0 (unset in proto3) means
    /// the REST default.
    fn resolve_max_concurrency(requested: i32) -> i32 {
        if requested == 0 {
            return DEFAULT_CONCURRENT_JOBS;
        }
        let clamped = requested.clamp(MIN_CONCURRENT_JOBS, MAX_CONCURRENT_JOBS);
        if clamped != requested {
            warn!(
                requested,
                adjusted = clamped,
                "Adjusted max_concurrency to safe range"
            );
        }
        clamped
    }

    /// Worker metadata as JSON, `None` for an empty map (proto3 cannot tell an
    /// empty map from an omitted one). Capped at 64 KB like REST.
    fn metadata_json(
        metadata: &HashMap<String, String>,
    ) -> Result<Option<serde_json::Value>, Status> {
        if metadata.is_empty() {
            return Ok(None);
        }
        let json: serde_json::Value = metadata
            .iter()
            .map(|(k, v)| (k.clone(), serde_json::Value::String(v.clone())))
            .collect();
        crate::models::validate_metadata_size(&json).map_err(|e| {
            Status::invalid_argument(
                e.message
                    .map(|m| m.to_string())
                    .unwrap_or_else(|| "Metadata too large".to_string()),
            )
        })?;
        Ok(Some(json))
    }

    /// The status a heartbeat asks for. `None` (empty in the request) keeps the
    /// stored one; an unknown value falls back to healthy as before.
    fn requested_heartbeat_status(status: &str) -> Option<&str> {
        if status.is_empty() {
            None
        } else if VALID_WORKER_STATUSES.contains(&status) {
            Some(status)
        } else {
            warn!(status = %status, "Invalid worker status, defaulting to healthy");
            Some("healthy")
        }
    }

    /// Mirrors the heartbeat UPDATE's CASE: a drain request is sticky, and a
    /// heartbeat without a status keeps the stored one, except that a worker
    /// marked offline is alive again and comes back as healthy.
    #[cfg(test)]
    fn resolve_heartbeat_status<'a>(
        current_status: &'a str,
        requested_status: Option<&'a str>,
    ) -> &'a str {
        match requested_status {
            _ if current_status == "draining" => "draining",
            Some(requested) => requested,
            None if current_status == "offline" => "healthy",
            None => current_status,
        }
    }

    /// Convert the returned worker status into the gRPC drain hint.
    fn should_drain_from_status(status: Option<&str>) -> bool {
        matches!(status, Some("draining"))
    }

    fn effective_worker_queues(queue_name: String, queue_names: Vec<String>) -> Vec<String> {
        if queue_names.is_empty() {
            vec![queue_name]
        } else {
            queue_names
        }
    }

    /// Turn the REST limit check's rejection into a gRPC status, keeping its
    /// message (`check_resource_limit_conn` answers with an HTTP response).
    async fn limit_rejection_status(response: axum::response::Response) -> Status {
        let code = response.status();
        let message = axum::body::to_bytes(response.into_body(), 64 * 1024)
            .await
            .ok()
            .and_then(|body| serde_json::from_slice::<serde_json::Value>(&body).ok())
            .and_then(|body| body.get("message")?.as_str().map(str::to_string));
        match code {
            axum::http::StatusCode::TOO_MANY_REQUESTS => Status::resource_exhausted(
                message.unwrap_or_else(|| "Worker limit reached".to_string()),
            ),
            axum::http::StatusCode::FORBIDDEN => Status::permission_denied(
                message.unwrap_or_else(|| "Workers are not available on this plan".to_string()),
            ),
            _ => Status::internal("Registration failed"),
        }
    }
}

#[tonic::async_trait]
impl WorkerService for WorkerServiceImpl {
    /// Register a worker, or re-register one under its stable id
    async fn register(
        &self,
        request: Request<RegisterWorkerRequest>,
    ) -> Result<Response<RegisterWorkerResponse>, Status> {
        let auth =
            authenticate_request_with_cache(self.pool.as_ref(), self.cache.as_ref(), &request)
                .await?;
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

        // Same input limits as REST, so bad input is INVALID_ARGUMENT here
        // rather than a DB error surfacing as INTERNAL.
        Self::validate_hostname(&req.hostname)?;
        Self::validate_worker_text("Worker type", &req.worker_type)?;
        Self::validate_worker_text("Version", &req.version)?;
        let requested_worker_id = Self::requested_worker_id(req.worker_id.as_deref())?;
        let metadata_json =
            Self::metadata_json(&req.metadata)?.unwrap_or_else(|| serde_json::json!({}));
        let max_concurrent = Self::resolve_max_concurrency(req.max_concurrency);

        // A caller-supplied id makes this an upsert of one row, as on REST, so
        // a restarting worker takes its row back instead of adding one that
        // holds a slot of the plan's worker cap until the reaper runs.
        let worker_id = requested_worker_id.unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        let now = Utc::now();

        // Check the cap and insert in one transaction under the advisory lock
        // REST takes, so concurrent registrations cannot overshoot it.
        let mut tx = self.pool.begin().await.map_err(|e| {
            tracing::error!(error = %e, "Failed to start worker registration");
            Status::internal("Registration failed")
        })?;
        lock_resource(&mut tx, &auth.organization_id, "workers")
            .await
            .map_err(|e| {
                tracing::error!(error = %e, "Failed to lock worker limit");
                Status::internal("Registration failed")
            })?;

        // A queue-scoped key must not take over a worker of its org that serves a
        // queue outside its scope: the upsert would move that row into the key's
        // queues. Same rule and answer as REST. Locked, and read under the
        // advisory lock every registration of this org takes, so the row cannot
        // change before the upsert.
        if !crate::models::queue_scope_is_unrestricted(&auth.queues) {
            let existing: Option<(String, Vec<String>)> = sqlx::query_as(
                "SELECT queue_name, queue_names FROM workers \
                 WHERE id = $1 AND organization_id = $2 FOR UPDATE",
            )
            .bind(&worker_id)
            .bind(&auth.organization_id)
            .fetch_optional(&mut *tx)
            .await
            .map_err(|e| {
                tracing::error!(error = %e, "Failed to look up worker registration");
                Status::internal("Registration failed")
            })?;
            if existing.is_some_and(|(queue_name, queue_names)| {
                !auth.can_access_all_queues(&Self::effective_worker_queues(queue_name, queue_names))
            }) {
                return Err(Status::already_exists(WORKER_ID_UNAVAILABLE));
            }
        }

        // An id this org already holds a slot for adds no worker, so it is not
        // charged against the cap; otherwise a worker at the limit could not
        // restart. Uses the same rule as the count itself.
        let (already_registered,): (bool,) = sqlx::query_as(
            "SELECT EXISTS (SELECT 1 FROM workers WHERE id = $1 AND organization_id = $2 \
             AND worker_counts_toward_cap(status, last_heartbeat))",
        )
        .bind(&worker_id)
        .bind(&auth.organization_id)
        .fetch_one(&mut *tx)
        .await
        .map_err(|e| {
            tracing::error!(error = %e, "Failed to look up worker registration");
            Status::internal("Registration failed")
        })?;
        let adding = if already_registered { 0 } else { 1 };

        if let Err(response) =
            check_resource_limit_conn(&mut tx, &auth.organization_id, "workers", adding).await
        {
            let status = Self::limit_rejection_status(response).await;
            warn!(
                org_id = %auth.organization_id,
                code = ?status.code(),
                message = %status.message(),
                "Worker registration rejected by plan limit"
            );
            return Err(status);
        }

        // Insert or update worker
        let result = sqlx::query(
            r#"
            INSERT INTO workers (
                id, organization_id, queue_name, queue_names, max_concurrent_jobs,
                hostname, worker_type, version, metadata, status,
                last_heartbeat, current_job_count, created_at, updated_at
            )
            VALUES ($1, $2, $3, ARRAY[$3], $4, $5, $6, $7, $8::JSONB, 'healthy', $9, 0, $9, $9)
            ON CONFLICT (id) DO UPDATE SET
                queue_name = EXCLUDED.queue_name,
                queue_names = EXCLUDED.queue_names,
                max_concurrent_jobs = EXCLUDED.max_concurrent_jobs,
                hostname = EXCLUDED.hostname,
                worker_type = EXCLUDED.worker_type,
                version = EXCLUDED.version,
                metadata = EXCLUDED.metadata,
                status = 'healthy',
                last_heartbeat = EXCLUDED.last_heartbeat,
                updated_at = EXCLUDED.updated_at
            -- Guards a caller-supplied id from taking over another org's worker.
            WHERE workers.organization_id = EXCLUDED.organization_id
            RETURNING id
            "#,
        )
        .bind(&worker_id)
        .bind(&auth.organization_id)
        .bind(&req.queue_name)
        .bind(max_concurrent)
        .bind(&req.hostname)
        .bind(if req.worker_type.is_empty() {
            None
        } else {
            Some(&req.worker_type)
        })
        .bind(if req.version.is_empty() {
            None
        } else {
            Some(&req.version)
        })
        .bind(&metadata_json)
        .bind(now)
        .fetch_optional(&mut *tx)
        .await
        .map_err(|e| {
            tracing::error!(error = %e, "Failed to register worker");
            Status::internal("Failed to register worker")
        })?;

        if result.is_none() {
            warn!(
                org_id = %auth.organization_id,
                "Worker ID conflict - worker belongs to different organization"
            );
            return Err(Status::already_exists(WORKER_ID_UNAVAILABLE));
        }

        tx.commit().await.map_err(|e| {
            tracing::error!(error = %e, "Failed to commit worker registration");
            Status::internal("Failed to register worker")
        })?;

        // Only a new worker moves the gauges; a re-registration already counted.
        if adding > 0 {
            self.metrics.workers_active.inc();
            self.metrics.workers_healthy.inc();
        }

        info!(
            worker_id = %worker_id,
            queue = %req.queue_name,
            hostname = %req.hostname,
            org_id = %auth.organization_id,
            reregistered = adding == 0,
            "Worker registered via gRPC"
        );

        self.outgoing_webhooks.spawn_dispatch(
            auth.organization_id.clone(),
            "worker.registered".to_string(),
            worker_id.clone(),
            serde_json::json!({
                "worker_id": worker_id,
                "queue_name": req.queue_name,
                "hostname": req.hostname,
            }),
        );

        Ok(Response::new(RegisterWorkerResponse {
            worker_id,
            // The lease this worker's claims get when they send none.
            lease_duration_secs: i32::try_from(
                self.worker_settings.grpc_default_lease_duration_secs,
            )
            .unwrap_or(i32::MAX),
            heartbeat_interval_secs: i32::try_from(self.worker_settings.heartbeat_interval_secs)
                .unwrap_or(i32::MAX),
        }))
    }

    /// Send worker heartbeat
    async fn heartbeat(
        &self,
        request: Request<HeartbeatRequest>,
    ) -> Result<Response<HeartbeatResponse>, Status> {
        let auth =
            authenticate_request_with_cache(self.pool.as_ref(), self.cache.as_ref(), &request)
                .await?;
        self.enforce_rate_limits(&auth.organization_id, &auth.api_key_id, auth.rate_limit)
            .await?;
        let req = request.into_inner();

        if req.worker_id.is_empty() {
            return Err(Status::invalid_argument("Worker ID is required"));
        }

        let requested_status = Self::requested_heartbeat_status(&req.status);
        // Empty metadata binds NULL so the stored metadata is kept.
        let metadata_json = Self::metadata_json(&req.metadata)?;

        let worker_queues: Option<(String, Vec<String>)> = sqlx::query_as(
            "SELECT queue_name, queue_names FROM workers WHERE id = $1 AND organization_id = $2",
        )
        .bind(&req.worker_id)
        .bind(&auth.organization_id)
        .fetch_optional(self.pool.as_ref())
        .await
        .map_err(|e| {
            tracing::error!(error = %e, "Failed to authorize worker heartbeat");
            Status::internal("Heartbeat failed")
        })?;

        let Some((queue_name, queue_names)) = worker_queues else {
            warn!(
                worker_id = %req.worker_id,
                org_id = %auth.organization_id,
                "Heartbeat for unknown or unauthorized worker"
            );
            return Ok(Response::new(HeartbeatResponse {
                acknowledged: false,
                should_drain: false,
            }));
        };

        let queues = Self::effective_worker_queues(queue_name, queue_names);
        if !auth.can_access_all_queues(&queues) {
            warn!(
                worker_id = %req.worker_id,
                org_id = %auth.organization_id,
                "Heartbeat for unknown or unauthorized worker"
            );
            return Ok(Response::new(HeartbeatResponse {
                acknowledged: false,
                should_drain: false,
            }));
        }

        let result = sqlx::query_as::<_, (String,)>(
            r#"
            UPDATE workers
            SET
                last_heartbeat = NOW(),
                status = CASE
                    WHEN status = 'draining' OR $1 = 'draining' THEN 'draining'
                    WHEN $1 IS NOT NULL THEN $1
                    WHEN status = 'offline' THEN 'healthy'
                    ELSE status
                END,
                current_job_count = $2,
                metadata = COALESCE($3::JSONB, metadata),
                updated_at = NOW()
            WHERE id = $4 AND organization_id = $5
            RETURNING status
            "#,
        )
        .bind(requested_status)
        .bind(req.current_jobs.clamp(0, MAX_CURRENT_JOBS))
        .bind(&metadata_json)
        .bind(&req.worker_id)
        .bind(&auth.organization_id)
        .fetch_optional(self.pool.as_ref())
        .await
        .map_err(|e| {
            tracing::error!(error = %e, "Failed to process heartbeat");
            Status::internal("Heartbeat failed")
        })?;

        let acknowledged = result.is_some();
        let should_drain =
            Self::should_drain_from_status(result.as_ref().map(|(status,)| status.as_str()));

        if !acknowledged {
            warn!(
                worker_id = %req.worker_id,
                org_id = %auth.organization_id,
                "Heartbeat for unknown or unauthorized worker"
            );
        }

        Ok(Response::new(HeartbeatResponse {
            acknowledged,
            should_drain,
        }))
    }

    /// Deregister a worker
    ///
    /// SECURITY: Now releases any jobs assigned to this worker before marking offline.
    /// Previously, jobs would be stuck in 'processing' until background cleanup ran.
    async fn deregister(
        &self,
        request: Request<DeregisterRequest>,
    ) -> Result<Response<DeregisterResponse>, Status> {
        let auth =
            authenticate_request_with_cache(self.pool.as_ref(), self.cache.as_ref(), &request)
                .await?;
        self.enforce_rate_limits(&auth.organization_id, &auth.api_key_id, auth.rate_limit)
            .await?;
        let req = request.into_inner();

        if req.worker_id.is_empty() {
            return Err(Status::invalid_argument("Worker ID is required"));
        }

        let worker_queues: Option<(String, Vec<String>)> = sqlx::query_as(
            "SELECT queue_name, queue_names FROM workers WHERE id = $1 AND organization_id = $2",
        )
        .bind(&req.worker_id)
        .bind(&auth.organization_id)
        .fetch_optional(self.pool.as_ref())
        .await
        .map_err(|e| {
            tracing::error!(error = %e, "Failed to authorize worker deregistration");
            Status::internal("Failed to deregister worker")
        })?;

        let Some((queue_name, queue_names)) = worker_queues else {
            warn!(
                worker_id = %req.worker_id,
                org_id = %auth.organization_id,
                "Deregister for unknown or unauthorized worker"
            );
            return Ok(Response::new(DeregisterResponse { success: false }));
        };

        let queues = Self::effective_worker_queues(queue_name, queue_names);
        if !auth.can_access_all_queues(&queues) {
            warn!(
                worker_id = %req.worker_id,
                org_id = %auth.organization_id,
                "Deregister for unknown or unauthorized worker"
            );
            return Ok(Response::new(DeregisterResponse { success: false }));
        }

        // First, release any jobs assigned to this worker (with org isolation)
        // This prevents jobs from being stuck in 'processing' state
        let released = sqlx::query(
            r#"
            UPDATE jobs
            SET
                status = 'pending',
                assigned_worker_id = NULL,
                lease_id = NULL,
                lease_expires_at = NULL,
                updated_at = NOW()
            WHERE assigned_worker_id = $1 AND status = 'processing' AND organization_id = $2
            "#,
        )
        .bind(&req.worker_id)
        .bind(&auth.organization_id)
        .execute(self.pool.as_ref())
        .await
        .map_err(|e| {
            tracing::error!(error = %e, "Failed to release worker jobs");
            Status::internal("Failed to release worker jobs")
        })?;

        if released.rows_affected() > 0 {
            info!(
                worker_id = %req.worker_id,
                released_jobs = released.rows_affected(),
                "Released jobs from deregistering worker"
            );
        }

        // Then mark worker as offline, reading the status it had under the row
        // lock: a repeated (or concurrent) Deregister must not move the gauges
        // again.
        let previous_status: Option<(String,)> = sqlx::query_as(
            r#"
            WITH previous AS (
                SELECT id, status FROM workers
                WHERE id = $1 AND organization_id = $2
                FOR UPDATE
            )
            UPDATE workers w
            SET status = 'offline', updated_at = NOW()
            FROM previous
            WHERE w.id = previous.id AND w.organization_id = $2
            RETURNING previous.status
            "#,
        )
        .bind(&req.worker_id)
        .bind(&auth.organization_id)
        .fetch_optional(self.pool.as_ref())
        .await
        .map_err(|e| {
            tracing::error!(error = %e, "Failed to deregister worker");
            Status::internal("Failed to deregister worker")
        })?;

        let success = previous_status.is_some();

        if let Some((previous_status,)) = previous_status {
            if previous_status != "offline" {
                self.metrics.workers_active.dec();
            }
            if previous_status == "healthy" {
                self.metrics.workers_healthy.dec();
            }
            info!(
                worker_id = %req.worker_id,
                org_id = %auth.organization_id,
                "Worker deregistered via gRPC"
            );
            self.outgoing_webhooks.spawn_dispatch(
                auth.organization_id.clone(),
                "worker.deregistered".to_string(),
                req.worker_id.clone(),
                serde_json::json!({
                    "worker_id": req.worker_id,
                }),
            );
        }

        Ok(Response::new(DeregisterResponse { success }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_validate_hostname() {
        assert!(WorkerServiceImpl::validate_hostname("worker-1").is_ok());
        assert!(WorkerServiceImpl::validate_hostname("worker.local").is_ok());
        assert!(WorkerServiceImpl::validate_hostname("worker_host").is_ok());

        assert!(WorkerServiceImpl::validate_hostname("-invalid").is_err());
        assert!(WorkerServiceImpl::validate_hostname(".invalid").is_err());
        assert!(WorkerServiceImpl::validate_hostname("invalid@host").is_err());
    }

    #[test]
    fn test_validate_queue_name() {
        assert!(WorkerServiceImpl::validate_queue_name("default").is_ok());
        assert!(WorkerServiceImpl::validate_queue_name("my-queue").is_ok());
        assert!(WorkerServiceImpl::validate_queue_name("queue.name").is_ok());

        assert!(WorkerServiceImpl::validate_queue_name("").is_err());
        assert!(WorkerServiceImpl::validate_queue_name("bad@queue").is_err());
    }

    #[test]
    fn test_requested_heartbeat_status() {
        assert_eq!(
            WorkerServiceImpl::requested_heartbeat_status("healthy"),
            Some("healthy")
        );
        assert_eq!(
            WorkerServiceImpl::requested_heartbeat_status("degraded"),
            Some("degraded")
        );
        assert_eq!(
            WorkerServiceImpl::requested_heartbeat_status("draining"),
            Some("draining")
        );
        assert_eq!(
            WorkerServiceImpl::requested_heartbeat_status("offline"),
            Some("offline")
        );
        assert_eq!(
            WorkerServiceImpl::requested_heartbeat_status("invalid"),
            Some("healthy")
        );
        assert_eq!(WorkerServiceImpl::requested_heartbeat_status(""), None);
    }

    #[test]
    fn test_resolve_heartbeat_status_preserves_existing_draining() {
        assert_eq!(
            WorkerServiceImpl::resolve_heartbeat_status("draining", Some("healthy")),
            "draining"
        );
        assert_eq!(
            WorkerServiceImpl::resolve_heartbeat_status("draining", None),
            "draining"
        );
    }

    #[test]
    fn test_resolve_heartbeat_status_accepts_requested_draining() {
        assert_eq!(
            WorkerServiceImpl::resolve_heartbeat_status("healthy", Some("draining")),
            "draining"
        );
    }

    #[test]
    fn test_resolve_heartbeat_status_keeps_normal_statuses() {
        assert_eq!(
            WorkerServiceImpl::resolve_heartbeat_status("healthy", Some("healthy")),
            "healthy"
        );
        assert_eq!(
            WorkerServiceImpl::resolve_heartbeat_status("healthy", Some("degraded")),
            "degraded"
        );
    }

    #[test]
    fn test_resolve_heartbeat_status_without_status_keeps_the_stored_one() {
        assert_eq!(
            WorkerServiceImpl::resolve_heartbeat_status("degraded", None),
            "degraded"
        );
        assert_eq!(
            WorkerServiceImpl::resolve_heartbeat_status("healthy", None),
            "healthy"
        );
        assert_eq!(
            WorkerServiceImpl::resolve_heartbeat_status("offline", None),
            "healthy"
        );
    }

    #[test]
    fn test_should_drain_from_status_handles_unknown_worker() {
        assert!(!WorkerServiceImpl::should_drain_from_status(None));
        assert!(!WorkerServiceImpl::should_drain_from_status(Some(
            "healthy"
        )));
        assert!(WorkerServiceImpl::should_drain_from_status(Some(
            "draining"
        )));
    }

    #[test]
    fn test_requested_worker_id() {
        assert_eq!(WorkerServiceImpl::requested_worker_id(None).unwrap(), None);
        assert_eq!(
            WorkerServiceImpl::requested_worker_id(Some("")).unwrap(),
            None
        );
        assert_eq!(
            WorkerServiceImpl::requested_worker_id(Some("pod-7.worker_a")).unwrap(),
            Some("pod-7.worker_a".to_string())
        );
        let max = "w".repeat(128);
        assert!(WorkerServiceImpl::requested_worker_id(Some(&max)).is_ok());

        for bad in ["bad id", "a/b", "ü", &"w".repeat(129)] {
            let err = WorkerServiceImpl::requested_worker_id(Some(bad)).unwrap_err();
            assert_eq!(err.code(), tonic::Code::InvalidArgument, "{bad}");
        }
    }

    #[test]
    fn test_resolve_max_concurrency_clamps_to_rest_range() {
        assert_eq!(WorkerServiceImpl::resolve_max_concurrency(0), 5);
        assert_eq!(WorkerServiceImpl::resolve_max_concurrency(-3), 1);
        assert_eq!(WorkerServiceImpl::resolve_max_concurrency(40), 40);
        assert_eq!(WorkerServiceImpl::resolve_max_concurrency(100), 100);
        assert_eq!(WorkerServiceImpl::resolve_max_concurrency(1000), 100);
    }

    #[test]
    fn test_validate_worker_text() {
        assert!(WorkerServiceImpl::validate_worker_text("Version", "").is_ok());
        assert!(WorkerServiceImpl::validate_worker_text("Version", &"v".repeat(50)).is_ok());
        let err = WorkerServiceImpl::validate_worker_text("Version", &"v".repeat(51)).unwrap_err();
        assert_eq!(err.code(), tonic::Code::InvalidArgument);
    }

    #[test]
    fn test_metadata_json() {
        assert_eq!(
            WorkerServiceImpl::metadata_json(&HashMap::new()).unwrap(),
            None
        );
        let small = HashMap::from([("region".to_string(), "eu".to_string())]);
        assert_eq!(
            WorkerServiceImpl::metadata_json(&small).unwrap(),
            Some(serde_json::json!({"region": "eu"}))
        );
        let big = HashMap::from([("blob".to_string(), "x".repeat(64 * 1024))]);
        let err = WorkerServiceImpl::metadata_json(&big).unwrap_err();
        assert_eq!(err.code(), tonic::Code::InvalidArgument);
    }
}
