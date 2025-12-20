//! WorkerService gRPC implementation
//!
//! Implements worker lifecycle management: Register, Heartbeat, Deregister.

// tonic::Status is a standard gRPC error type, size is acceptable
#![allow(clippy::result_large_err)]

use std::sync::Arc;

use chrono::Utc;
use sqlx::PgPool;
use tonic::{Request, Response, Status};
use tracing::{info, warn};

use crate::cache::RedisCache;
use crate::config::PlanLimits;
use crate::grpc::auth::authenticate_request;
use crate::grpc::proto::{
    worker_service_server::WorkerService, DeregisterRequest, DeregisterResponse, HeartbeatRequest,
    HeartbeatResponse, RegisterWorkerRequest, RegisterWorkerResponse,
};
use crate::observability::Metrics;

/// Maximum concurrent jobs a worker can handle
const MAX_CONCURRENT_JOBS: i32 = 1000;

/// Minimum concurrent jobs
const MIN_CONCURRENT_JOBS: i32 = 1;

/// Maximum hostname length
const MAX_HOSTNAME_LENGTH: usize = 255;

/// Default lease duration for workers (seconds)
const DEFAULT_LEASE_DURATION_SECS: i32 = 300;

/// Default heartbeat interval (seconds)
const DEFAULT_HEARTBEAT_INTERVAL_SECS: i32 = 10;

/// Valid worker statuses
const VALID_WORKER_STATUSES: &[&str] = &["healthy", "degraded", "draining", "offline"];

/// WorkerService implementation
#[derive(Clone)]
pub struct WorkerServiceImpl {
    pool: Arc<PgPool>,
    metrics: Arc<Metrics>,
    cache: Option<RedisCache>,
}

impl WorkerServiceImpl {
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
                    tracing::error!(
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

        let bucket_key = format!("rate_limit:grpc:org:{}", org_id);
        let result = cache
            .check_token_bucket(&bucket_key, org_limits.rps, org_limits.burst)
            .await
            .map_err(|e| {
                tracing::error!(
                    error = %e,
                    org_id = %org_id,
                    "Redis error during gRPC org rate limit"
                );
                Status::internal("Rate limiting unavailable")
            })?;
        if !result.allowed {
            return Err(Status::resource_exhausted("Rate limit exceeded"));
        }

        if let Some(per_min) = api_key_per_minute {
            let per_min = per_min.max(1) as u32;
            let key = format!("rate_limit:grpc:api_key:{}", api_key_id);
            let rl = cache.check_rate_limit(&key, per_min, 60).await.map_err(|e| {
                tracing::error!(
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

    /// Validate worker status
    fn validate_status(status: &str) -> &str {
        if VALID_WORKER_STATUSES.contains(&status) {
            status
        } else {
            warn!(status = %status, "Invalid worker status, defaulting to healthy");
            "healthy"
        }
    }
}

#[tonic::async_trait]
impl WorkerService for WorkerServiceImpl {
    /// Register a new worker
    async fn register(
        &self,
        request: Request<RegisterWorkerRequest>,
    ) -> Result<Response<RegisterWorkerResponse>, Status> {
        let auth = authenticate_request(self.pool.as_ref(), &request).await?;
        self.enforce_rate_limits(&auth.organization_id, &auth.api_key_id, auth.rate_limit)
            .await?;
        let req = request.into_inner();

        // Validate queue name
        Self::validate_queue_name(&req.queue_name)?;

        // Validate hostname
        Self::validate_hostname(&req.hostname)?;

        // Validate max_concurrency
        let safe_max_concurrent = req
            .max_concurrency
            .clamp(MIN_CONCURRENT_JOBS, MAX_CONCURRENT_JOBS);
        if req.max_concurrency != safe_max_concurrent && req.max_concurrency != 0 {
            warn!(
                requested = req.max_concurrency,
                adjusted = safe_max_concurrent,
                "Adjusted max_concurrency to safe range"
            );
        }
        let max_concurrent = if req.max_concurrency == 0 {
            5 // Default
        } else {
            safe_max_concurrent
        };

        // Enforce plan-based worker limits (supports env overrides + org custom_limits).
        let (plan_tier, custom_limits): (String, Option<serde_json::Value>) = sqlx::query_as(
            "SELECT plan_tier, custom_limits FROM organizations WHERE id = $1",
        )
        .bind(&auth.organization_id)
        .fetch_one(self.pool.as_ref())
        .await
        .map_err(|e| {
            tracing::error!(error = %e, "Failed to fetch org plan limits");
            Status::internal("Registration failed")
        })?;

        let limits = PlanLimits::for_tier_with_overrides(&plan_tier, custom_limits.as_ref());

        // Read current worker count using the same source as HTTP limit checks.
        let counts: (i64, i64, i64, i64, i64, i64, i64, i64) =
            sqlx::query_as("SELECT * FROM get_org_resource_counts($1)")
                .bind(&auth.organization_id)
                .fetch_one(self.pool.as_ref())
                .await
                .map_err(|e| {
                    tracing::error!(error = %e, "Failed to fetch org resource counts");
                    Status::internal("Registration failed")
                })?;

        let current_workers = counts.2.max(0) as u64;
        if let Err(err) = limits.check_limit("workers", current_workers, 1) {
            warn!(
                org_id = %auth.organization_id,
                current = current_workers,
                limit = err.limit,
                "Organization exceeded worker limit"
            );
            return Err(Status::resource_exhausted(err.to_string()));
        }

        let now = Utc::now();
        let worker_id = uuid::Uuid::new_v4().to_string();

        // Convert metadata to JSON
        let metadata_json: serde_json::Value = req
            .metadata
            .iter()
            .map(|(k, v)| (k.clone(), serde_json::Value::String(v.clone())))
            .collect();

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
        .fetch_optional(self.pool.as_ref())
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
            return Err(Status::already_exists(
                "Worker ID already in use by another organization",
            ));
        }

        self.metrics.workers_active.inc();
        self.metrics.workers_healthy.inc();

        info!(
            worker_id = %worker_id,
            queue = %req.queue_name,
            hostname = %req.hostname,
            org_id = %auth.organization_id,
            "Worker registered via gRPC"
        );

        Ok(Response::new(RegisterWorkerResponse {
            worker_id,
            lease_duration_secs: DEFAULT_LEASE_DURATION_SECS,
            heartbeat_interval_secs: DEFAULT_HEARTBEAT_INTERVAL_SECS,
        }))
    }

    /// Send worker heartbeat
    async fn heartbeat(
        &self,
        request: Request<HeartbeatRequest>,
    ) -> Result<Response<HeartbeatResponse>, Status> {
        let auth = authenticate_request(self.pool.as_ref(), &request).await?;
        self.enforce_rate_limits(&auth.organization_id, &auth.api_key_id, auth.rate_limit)
            .await?;
        let req = request.into_inner();

        if req.worker_id.is_empty() {
            return Err(Status::invalid_argument("Worker ID is required"));
        }

        let safe_status = Self::validate_status(&req.status);

        // Convert metadata to JSON
        let metadata_json: serde_json::Value = req
            .metadata
            .iter()
            .map(|(k, v)| (k.clone(), serde_json::Value::String(v.clone())))
            .collect();

        // Check if we should tell the worker to drain
        // This could be based on configuration or admin action
        let should_drain = false; // TODO: Implement drain logic if needed

        let result = sqlx::query(
            r#"
            UPDATE workers
            SET 
                last_heartbeat = NOW(),
                status = $1,
                current_job_count = $2,
                metadata = COALESCE($3::JSONB, metadata),
                updated_at = NOW()
            WHERE id = $4 AND organization_id = $5
            "#,
        )
        .bind(safe_status)
        .bind(req.current_jobs)
        .bind(&metadata_json)
        .bind(&req.worker_id)
        .bind(&auth.organization_id)
        .execute(self.pool.as_ref())
        .await
        .map_err(|e| {
            tracing::error!(error = %e, "Failed to process heartbeat");
            Status::internal("Heartbeat failed")
        })?;

        let acknowledged = result.rows_affected() > 0;

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
        let auth = authenticate_request(self.pool.as_ref(), &request).await?;
        self.enforce_rate_limits(&auth.organization_id, &auth.api_key_id, auth.rate_limit)
            .await?;
        let req = request.into_inner();

        if req.worker_id.is_empty() {
            return Err(Status::invalid_argument("Worker ID is required"));
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

        // Then mark worker as offline
        let result = sqlx::query(
            "UPDATE workers SET status = 'offline', updated_at = NOW() WHERE id = $1 AND organization_id = $2",
        )
        .bind(&req.worker_id)
        .bind(&auth.organization_id)
        .execute(self.pool.as_ref())
        .await
        .map_err(|e| {
            tracing::error!(error = %e, "Failed to deregister worker");
            Status::internal("Failed to deregister worker")
        })?;

        let success = result.rows_affected() > 0;

        if success {
            self.metrics.workers_active.dec();
            info!(
                worker_id = %req.worker_id,
                org_id = %auth.organization_id,
                "Worker deregistered via gRPC"
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
    fn test_validate_status() {
        assert_eq!(WorkerServiceImpl::validate_status("healthy"), "healthy");
        assert_eq!(WorkerServiceImpl::validate_status("degraded"), "degraded");
        assert_eq!(WorkerServiceImpl::validate_status("draining"), "draining");
        assert_eq!(WorkerServiceImpl::validate_status("offline"), "offline");
        assert_eq!(WorkerServiceImpl::validate_status("invalid"), "healthy");
    }
}
