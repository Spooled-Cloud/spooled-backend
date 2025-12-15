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

use crate::grpc::auth::authenticate_request;
use crate::grpc::proto::{
    worker_service_server::WorkerService, DeregisterRequest, DeregisterResponse,
    HeartbeatRequest, HeartbeatResponse, RegisterWorkerRequest, RegisterWorkerResponse,
};
use crate::observability::Metrics;

/// Maximum workers per organization
const MAX_WORKERS_PER_ORG: i64 = 100;

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
}

impl WorkerServiceImpl {
    pub fn new(pool: Arc<PgPool>, metrics: Arc<Metrics>) -> Self {
        Self { pool, metrics }
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
            return Err(Status::invalid_argument("Queue name must be 1-255 characters"));
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

        // Check worker count limit per organization
        let (worker_count,): (i64,) = sqlx::query_as(
            "SELECT COUNT(*) FROM workers WHERE organization_id = $1 AND status != 'offline'",
        )
        .bind(&auth.organization_id)
        .fetch_one(self.pool.as_ref())
        .await
        .map_err(|e| {
            tracing::error!(error = %e, "Database error during worker registration");
            Status::internal("Registration failed")
        })?;

        if worker_count >= MAX_WORKERS_PER_ORG {
            warn!(
                org_id = %auth.organization_id,
                worker_count = worker_count,
                max_workers = MAX_WORKERS_PER_ORG,
                "Organization exceeded max worker limit"
            );
            return Err(Status::resource_exhausted(format!(
                "Organization has reached maximum worker limit ({})",
                MAX_WORKERS_PER_ORG
            )));
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
    async fn deregister(
        &self,
        request: Request<DeregisterRequest>,
    ) -> Result<Response<DeregisterResponse>, Status> {
        let auth = authenticate_request(self.pool.as_ref(), &request).await?;
        let req = request.into_inner();

        if req.worker_id.is_empty() {
            return Err(Status::invalid_argument("Worker ID is required"));
        }

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
