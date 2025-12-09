//! gRPC Service Implementation
//!
//! This module implements a JSON-over-HTTP/2 style gRPC service
//! without requiring protoc compilation.
//!
//! Streaming endpoints:
//! - StreamJobs: Server-Sent Events (SSE) for job streaming
//! - ProcessJobs: Use WebSocket endpoint /api/v1/ws instead

use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use axum::{
    extract::{Query, Request, State},
    http::StatusCode,
    middleware::{self, Next},
    response::{
        sse::{Event, KeepAlive, Sse},
        Response,
    },
    routing::{get, post},
    Json, Router,
};
use chrono::Utc;
use futures::stream::{self, Stream};
use tracing::{debug, error, info, warn};

use crate::db::Database;
use crate::observability::Metrics;

use super::types::*;

/// Shared state for gRPC service
#[derive(Clone)]
pub struct GrpcState {
    pub db: Arc<Database>,
    pub metrics: Arc<Metrics>,
    pub api_keys_enabled: bool,
}

/// Authenticated organization context extracted from API key
#[derive(Clone)]
pub struct GrpcAuthContext {
    pub organization_id: String,
    pub api_key_id: String,
}

/// Main gRPC service
pub struct SpooledGrpcService {
    state: GrpcState,
}

impl SpooledGrpcService {
    pub fn new(db: Arc<Database>, metrics: Arc<Metrics>) -> Self {
        Self {
            state: GrpcState {
                db,
                metrics,
                api_keys_enabled: true, // Enable by default
            },
        }
    }

    /// Create the Axum router for gRPC-style endpoints
    ///
    /// All endpoints now require X-API-Key header authentication
    pub fn router(self) -> Router {
        Router::new()
            // Queue operations
            .route("/spooled.v1.QueueService/Enqueue", post(enqueue_handler))
            .route("/spooled.v1.QueueService/Dequeue", post(dequeue_handler))
            .route("/spooled.v1.QueueService/Complete", post(complete_handler))
            .route("/spooled.v1.QueueService/Fail", post(fail_handler))
            .route(
                "/spooled.v1.QueueService/RenewLease",
                post(renew_lease_handler),
            )
            .route("/spooled.v1.QueueService/GetJob", post(get_job_handler))
            .route(
                "/spooled.v1.QueueService/GetQueueStats",
                post(get_queue_stats_handler),
            )
            // Streaming operations (SSE-based)
            .route(
                "/spooled.v1.QueueService/StreamJobs",
                get(stream_jobs_handler),
            )
            // Worker operations
            .route(
                "/spooled.v1.WorkerService/Register",
                post(register_worker_handler),
            )
            .route(
                "/spooled.v1.WorkerService/Heartbeat",
                post(heartbeat_handler),
            )
            .route(
                "/spooled.v1.WorkerService/Deregister",
                post(deregister_handler),
            )
            // Apply authentication middleware to all routes
            .layer(middleware::from_fn_with_state(
                self.state.clone(),
                grpc_auth_middleware,
            ))
            .with_state(self.state)
    }
}

/// Authentication middleware for gRPC endpoints
///
/// Now properly validates API key using bcrypt and extracts organization context.
/// Previously just checked token length and trusted client-provided org_id.
///
/// Requires X-API-Key header with valid API key or Authorization: Bearer token
async fn grpc_auth_middleware(
    State(state): State<GrpcState>,
    mut request: Request,
    next: Next,
) -> Result<Response, (StatusCode, String)> {
    // Check for API key in headers
    let auth_header = request
        .headers()
        .get("X-API-Key")
        .or_else(|| request.headers().get("Authorization"))
        .and_then(|v| v.to_str().ok());

    let token = match auth_header {
        Some(header) if header.starts_with("Bearer ") => &header[7..],
        Some(header) => header,
        None => {
            warn!("gRPC request without authentication");
            return Err((
                StatusCode::UNAUTHORIZED,
                "Missing X-API-Key or Authorization header".to_string(),
            ));
        }
    };

    // Properly validate API key using bcrypt
    // Use key prefix for efficient lookup (similar to REST auth)
    let key_prefix = token.chars().take(8).collect::<String>();

    // Fetch only keys with matching prefix (much more efficient than ALL keys)
    let api_keys: Vec<(String, String, String)> = sqlx::query_as(
        "SELECT id, organization_id, key_hash FROM api_keys WHERE is_active = TRUE AND (key_prefix = $1 OR key_prefix IS NULL) LIMIT 10"
    )
    .bind(&key_prefix)
    .fetch_all(state.db.pool())
    .await
    // Sanitize error to not leak database details
    .map_err(|e| {
        error!(error = %e, "Database error during gRPC authentication");
        (StatusCode::INTERNAL_SERVER_ERROR, "Authentication failed".to_string())
    })?;

    // Find matching key using bcrypt verification
    let mut auth_context: Option<GrpcAuthContext> = None;
    for (key_id, org_id, key_hash) in &api_keys {
        if bcrypt::verify(token, key_hash).unwrap_or(false) {
            auth_context = Some(GrpcAuthContext {
                organization_id: org_id.clone(),
                api_key_id: key_id.clone(),
            });
            break;
        }
    }

    let auth_context = auth_context.ok_or_else(|| {
        warn!("Invalid API key provided for gRPC request");
        (StatusCode::UNAUTHORIZED, "Invalid API key".to_string())
    })?;

    info!(
        org_id = %auth_context.organization_id,
        api_key_id = %auth_context.api_key_id,
        "gRPC request authenticated"
    );

    // Insert auth context into request extensions so handlers can use it
    request.extensions_mut().insert(auth_context);

    Ok(next.run(request).await)
}

/// Start the gRPC server on the specified address
pub async fn start_grpc_server(
    addr: SocketAddr,
    db: Arc<Database>,
    metrics: Arc<Metrics>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let service = SpooledGrpcService::new(db, metrics);
    let app = service.router();

    info!(%addr, "Starting gRPC server");

    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;

    Ok(())
}

// Handler implementations

/// Now uses authenticated organization context instead of trusting client-provided org_id.
/// Previously a malicious user could create jobs for ANY organization by setting organization_id in request.
/// Now calls validate() to check payload size and queue name
async fn enqueue_handler(
    State(state): State<GrpcState>,
    axum::Extension(auth): axum::Extension<GrpcAuthContext>,
    Json(req): Json<EnqueueJobRequest>,
) -> Result<Json<EnqueueJobResponse>, (StatusCode, String)> {
    // Validate request before processing
    req.validate().map_err(|e| {
        warn!(error = %e, "gRPC enqueue request validation failed");
        (StatusCode::BAD_REQUEST, "Invalid request".to_string())
    })?;

    let job_id = uuid::Uuid::new_v4().to_string();
    let now = Utc::now();

    // Warn if client-provided org_id doesn't match authenticated org
    if !req.organization_id.is_empty() && req.organization_id != auth.organization_id {
        warn!(
            requested_org = %req.organization_id,
            authenticated_org = %auth.organization_id,
            "Client tried to enqueue job for different organization - using authenticated org instead"
        );
    }

    let status = if req.scheduled_at.is_some_and(|t| t > now) {
        "scheduled"
    } else {
        "pending"
    };

    // Use authenticated organization_id, not client-provided
    let result = sqlx::query(
        r#"
        INSERT INTO jobs (
            id, organization_id, queue_name, status, payload, priority,
            max_retries, timeout_seconds, scheduled_at, idempotency_key,
            tags, completion_webhook, parent_job_id, expires_at,
            created_at, updated_at
        )
        VALUES ($1, $2, $3, $4, $5::JSONB, $6, $7, $8, $9, $10, $11::JSONB, $12, $13, $14, $15, $15)
        ON CONFLICT (idempotency_key) WHERE idempotency_key IS NOT NULL
        DO UPDATE SET updated_at = EXCLUDED.updated_at
        RETURNING id, (xmax = 0) as created
        "#,
    )
    .bind(&job_id)
    .bind(&auth.organization_id) // Use authenticated org, not req.organization_id
    .bind(&req.queue_name)
    .bind(status)
    .bind(&req.payload)
    .bind(req.priority)
    .bind(req.max_retries)
    .bind(req.timeout_seconds)
    .bind(req.scheduled_at)
    .bind(&req.idempotency_key)
    .bind(&req.tags)
    .bind(&req.completion_webhook)
    .bind(&req.parent_job_id)
    .bind(req.expires_at)
    .bind(now)
    .fetch_one(state.db.pool())
    .await
    // Sanitize error to not leak database details
    .map_err(|e| {
        error!(error = %e, "Failed to enqueue job");
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            "Failed to enqueue job".to_string(),
        )
    })?;

    use sqlx::Row;
    let returned_id: String = result.get("id");
    let created: bool = result.get("created");

    state.metrics.jobs_enqueued.inc();

    Ok(Json(EnqueueJobResponse {
        job_id: returned_id,
        created,
    }))
}

/// Now uses authenticated organization context instead of trusting client-provided org_id.
/// Previously a malicious user could dequeue jobs from ANY organization by setting organization_id in request.
/// Now validates queue_name format
async fn dequeue_handler(
    State(state): State<GrpcState>,
    axum::Extension(auth): axum::Extension<GrpcAuthContext>,
    Json(req): Json<DequeueJobRequest>,
) -> Result<Json<DequeueJobResponse>, (StatusCode, String)> {
    // Validate queue name
    if req.queue_name.is_empty() || req.queue_name.len() > 255 {
        return Err((
            StatusCode::BAD_REQUEST,
            "Invalid queue name length".to_string(),
        ));
    }
    if !req
        .queue_name
        .chars()
        .all(|c| c.is_alphanumeric() || c == '-' || c == '_' || c == '.')
    {
        return Err((
            StatusCode::BAD_REQUEST,
            "Invalid queue name characters".to_string(),
        ));
    }

    // Use safe_lease_duration to clamp to valid range
    let safe_lease_secs = req.safe_lease_duration();
    let lease_duration = chrono::Duration::seconds(safe_lease_secs as i64);
    let lease_expires_at = Utc::now() + lease_duration;
    let lease_id = uuid::Uuid::new_v4().to_string();

    // Warn if client-provided org_id doesn't match authenticated org
    if !req.organization_id.is_empty() && req.organization_id != auth.organization_id {
        warn!(
            requested_org = %req.organization_id,
            authenticated_org = %auth.organization_id,
            "Client tried to dequeue job from different organization - using authenticated org instead"
        );
    }

    // Use authenticated organization_id, not client-provided
    // Query matches QueueManager.dequeue - includes scheduled status and dependencies_met check
    let job: Option<crate::models::Job> = sqlx::query_as(
        r#"
        UPDATE jobs
        SET 
            status = 'processing',
            started_at = NOW(),
            assigned_worker_id = $1,
            lease_id = $2,
            lease_expires_at = $3,
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
    .bind(lease_expires_at)
    .bind(&auth.organization_id) // Use authenticated org, not req.organization_id
    .bind(&req.queue_name)
    .fetch_optional(state.db.pool())
    .await
    // Sanitize error to not leak database details
    .map_err(|e| {
        error!(error = %e, "Failed to dequeue job");
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            "Failed to dequeue job".to_string(),
        )
    })?;

    let grpc_job = job.map(|j| GrpcJob {
        id: j.id,
        organization_id: j.organization_id,
        queue_name: j.queue_name,
        status: GrpcJobStatus::from(j.status.as_str()),
        payload: j.payload.to_string(),
        result: j.result.map(|r| r.to_string()),
        retry_count: j.retry_count,
        max_retries: j.max_retries,
        last_error: j.last_error,
        created_at: Some(j.created_at),
        scheduled_at: j.scheduled_at,
        started_at: j.started_at,
        completed_at: j.completed_at,
        expires_at: j.expires_at,
        priority: j.priority,
        tags: j.tags.map(|t| t.to_string()),
        timeout_seconds: j.timeout_seconds,
        parent_job_id: j.parent_job_id,
        completion_webhook: j.completion_webhook,
        assigned_worker_id: j.assigned_worker_id,
        lease_id: j.lease_id,
        lease_expires_at: j.lease_expires_at,
        idempotency_key: j.idempotency_key,
    });

    if grpc_job.is_some() {
        state.metrics.jobs_processing.inc();
    }

    Ok(Json(DequeueJobResponse { job: grpc_job }))
}

/// Maximum result payload size (1MB)
const MAX_RESULT_SIZE: usize = 1024 * 1024;

/// Now uses authenticated organization context to validate job ownership.
/// Previously only checked worker_id, allowing cross-tenant job completion attacks.
/// Validates result size
async fn complete_handler(
    State(state): State<GrpcState>,
    axum::Extension(auth): axum::Extension<GrpcAuthContext>,
    Json(req): Json<CompleteJobRequest>,
) -> Result<Json<()>, (StatusCode, String)> {
    // Validate result size
    if let Some(ref result) = req.result {
        if result.len() > MAX_RESULT_SIZE {
            return Err((
                StatusCode::BAD_REQUEST,
                format!("Result too large: max {} bytes", MAX_RESULT_SIZE),
            ));
        }
    }

    let result = sqlx::query(
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
    .bind(&req.result)
    .bind(&req.job_id)
    .bind(&req.worker_id)
    .bind(&auth.organization_id) // Validate organization ownership
    .execute(state.db.pool())
    .await
    // Sanitize error
    .map_err(|e| {
        error!(error = %e, "Failed to complete job");
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            "Failed to complete job".to_string(),
        )
    })?;

    if result.rows_affected() == 0 {
        return Err((
            StatusCode::NOT_FOUND,
            "Job not found or not authorized".to_string(),
        ));
    }

    state.metrics.jobs_completed.inc();
    state.metrics.jobs_processing.dec();

    Ok(Json(()))
}

/// Maximum error message length
const MAX_ERROR_MESSAGE_LENGTH: usize = 4096;

/// Now uses authenticated organization context to validate job ownership.
/// Previously only checked worker_id, allowing cross-tenant job failure attacks.
/// Validates error message length
async fn fail_handler(
    State(state): State<GrpcState>,
    axum::Extension(auth): axum::Extension<GrpcAuthContext>,
    Json(req): Json<FailJobRequest>,
) -> Result<Json<()>, (StatusCode, String)> {
    // Truncate error message if too long
    let error_message = if req.error_message.len() > MAX_ERROR_MESSAGE_LENGTH {
        format!(
            "{}... [truncated]",
            &req.error_message[..MAX_ERROR_MESSAGE_LENGTH - 15]
        )
    } else {
        req.error_message.clone()
    };

    // Atomic check-and-update to prevent race condition
    // Previously: read retry_count, then update (race between read and write)
    // Now: single atomic UPDATE with CASE expression

    #[derive(sqlx::FromRow)]
    struct UpdateResult {
        old_status: String,
        new_status: String,
    }

    let result: Option<UpdateResult> = sqlx::query_as(
        r#"
        UPDATE jobs
        SET 
            status = CASE 
                WHEN retry_count >= max_retries THEN 'deadletter'
                ELSE 'pending'
            END,
            last_error = $1,
            retry_count = retry_count + 1,
            scheduled_at = CASE 
                WHEN retry_count < max_retries THEN NOW() + (POWER(2, retry_count) || ' minutes')::INTERVAL
                ELSE scheduled_at
            END,
            assigned_worker_id = NULL,
            lease_id = NULL,
            lease_expires_at = NULL,
            updated_at = NOW()
        WHERE id = $2 
          AND assigned_worker_id = $3
          AND organization_id = $4
          AND status = 'processing'
        RETURNING 
            'processing' as old_status,
            CASE 
                WHEN retry_count >= max_retries THEN 'deadletter'
                ELSE 'pending'
            END as new_status
        "#
    )
    .bind(&error_message) // Use sanitized error message
    .bind(&req.job_id)
    .bind(&req.worker_id)
    .bind(&auth.organization_id) // Validate organization ownership
    .fetch_optional(state.db.pool())
    .await
    // Sanitize error
    .map_err(|e| {
        error!(error = %e, "Failed to fail job");
        (StatusCode::INTERNAL_SERVER_ERROR, "Failed to update job".to_string())
    })?;

    let result = result.ok_or((
        StatusCode::NOT_FOUND,
        "Job not found or not authorized".to_string(),
    ))?;

    state.metrics.jobs_failed.inc();
    state.metrics.jobs_processing.dec();

    if result.new_status == "deadletter" {
        state.metrics.jobs_deadlettered.inc();
    }

    Ok(Json(()))
}

async fn renew_lease_handler(
    State(state): State<GrpcState>,
    axum::Extension(auth): axum::Extension<GrpcAuthContext>,
    Json(req): Json<RenewLeaseRequest>,
) -> Result<Json<RenewLeaseResponse>, (StatusCode, String)> {
    // Use safe_lease_duration to clamp to valid range
    let safe_lease_secs = req.safe_lease_duration();
    let lease_duration = chrono::Duration::seconds(safe_lease_secs as i64);
    let new_lease_expires_at = Utc::now() + lease_duration;

    let result = sqlx::query(
        r#"
        UPDATE jobs
        SET lease_expires_at = $1, updated_at = NOW()
        WHERE id = $2 AND assigned_worker_id = $3 AND organization_id = $4 AND status = 'processing'
        "#,
    )
    .bind(new_lease_expires_at)
    .bind(&req.job_id)
    .bind(&req.worker_id)
    .bind(&auth.organization_id)
    .execute(state.db.pool())
    .await
    // Sanitize error
    .map_err(|e| {
        error!(error = %e, "Failed to renew lease");
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            "Failed to renew lease".to_string(),
        )
    })?;

    Ok(Json(RenewLeaseResponse {
        renewed: result.rows_affected() > 0,
        new_lease_expires_at: if result.rows_affected() > 0 {
            Some(new_lease_expires_at)
        } else {
            None
        },
    }))
}

/// Maximum concurrent jobs a worker can request
const MAX_CONCURRENT_JOBS_LIMIT: i32 = 1000;
const MIN_CONCURRENT_JOBS: i32 = 1;

/// Valid worker status values
const VALID_WORKER_STATUSES: &[&str] = &["healthy", "degraded", "draining", "offline"];

/// Maximum hostname length
const MAX_HOSTNAME_LENGTH: usize = 255;

/// Previously a malicious user could register workers for ANY organization.
async fn register_worker_handler(
    State(state): State<GrpcState>,
    axum::Extension(auth): axum::Extension<GrpcAuthContext>,
    Json(req): Json<RegisterWorkerRequest>,
) -> Result<Json<RegisterWorkerResponse>, (StatusCode, String)> {
    // Validate hostname if provided
    if let Some(ref hostname) = req.hostname {
        if hostname.len() > MAX_HOSTNAME_LENGTH {
            return Err((StatusCode::BAD_REQUEST, "Hostname too long".to_string()));
        }
        if !hostname
            .chars()
            .all(|c| c.is_alphanumeric() || c == '-' || c == '.' || c == '_')
        {
            return Err((
                StatusCode::BAD_REQUEST,
                "Invalid characters in hostname".to_string(),
            ));
        }
    }

    // Validate queue names
    for queue_name in &req.queue_names {
        if queue_name.is_empty() || queue_name.len() > 255 {
            return Err((
                StatusCode::BAD_REQUEST,
                "Invalid queue name length".to_string(),
            ));
        }
        if !queue_name
            .chars()
            .all(|c| c.is_alphanumeric() || c == '-' || c == '_' || c == '.')
        {
            return Err((
                StatusCode::BAD_REQUEST,
                "Invalid characters in queue name".to_string(),
            )); // Don't expose queue name in error
        }
    }

    // Validate max_concurrent_jobs
    let safe_max_concurrent = req
        .max_concurrent_jobs
        .clamp(MIN_CONCURRENT_JOBS, MAX_CONCURRENT_JOBS_LIMIT);
    if req.max_concurrent_jobs != safe_max_concurrent {
        warn!(
            requested = req.max_concurrent_jobs,
            adjusted = safe_max_concurrent,
            "Adjusted max_concurrent_jobs to safe range"
        );
    }

    let now = Utc::now();

    // Check worker count limit per organization to prevent DoS
    const MAX_WORKERS_PER_ORG: i64 = 100;
    let (worker_count,): (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM workers WHERE organization_id = $1 AND status != 'offline'",
    )
    .bind(&auth.organization_id)
    .fetch_one(state.db.pool())
    .await
    // Sanitize error
    .map_err(|e| {
        error!(error = %e, "Database error during worker registration");
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            "Registration failed".to_string(),
        )
    })?;

    if worker_count >= MAX_WORKERS_PER_ORG {
        warn!(
            org_id = %auth.organization_id,
            worker_count = worker_count,
            max_workers = MAX_WORKERS_PER_ORG,
            "Organization exceeded max worker limit"
        );
        return Err((
            StatusCode::TOO_MANY_REQUESTS,
            format!(
                "Organization has reached maximum worker limit ({})",
                MAX_WORKERS_PER_ORG
            ),
        ));
    }

    // Warn if client-provided org_id doesn't match authenticated org
    if !req.organization_id.is_empty() && req.organization_id != auth.organization_id {
        warn!(
            requested_org = %req.organization_id,
            authenticated_org = %auth.organization_id,
            "Client tried to register worker for different organization - using authenticated org instead"
        );
    }

    // Use authenticated organization_id, not client-provided
    // Use safe_max_concurrent instead of raw value
    //
    // Previously, an attacker could register a worker with an existing ID from another org
    // and overwrite that worker's configuration (queue_names, hostname, etc.).
    // Now the update only happens if the worker belongs to the same organization.
    let result = sqlx::query(
        r#"
        INSERT INTO workers (
            id, organization_id, queue_names, max_concurrent_jobs,
            hostname, metadata, status, last_heartbeat, created_at, updated_at
        )
        VALUES ($1, $2, $3, $4, $5, $6::JSONB, 'healthy', $7, $7, $7)
        ON CONFLICT (id) DO UPDATE SET
            queue_names = EXCLUDED.queue_names,
            max_concurrent_jobs = EXCLUDED.max_concurrent_jobs,
            hostname = EXCLUDED.hostname,
            metadata = EXCLUDED.metadata,
            status = 'healthy',
            last_heartbeat = EXCLUDED.last_heartbeat,
            updated_at = EXCLUDED.updated_at
        WHERE workers.organization_id = EXCLUDED.organization_id
        "#,
    )
    .bind(&req.worker_id)
    .bind(&auth.organization_id) // Use authenticated org, not req.organization_id
    .bind(&req.queue_names)
    .bind(safe_max_concurrent) // Use validated value
    .bind(&req.hostname)
    .bind(&req.metadata)
    .bind(now)
    .execute(state.db.pool())
    .await
    // Sanitize error
    .map_err(|e| {
        error!(error = %e, "Failed to register worker");
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            "Failed to register worker".to_string(),
        )
    })?;

    // Check if the registration was successful
    // If rows_affected is 0, it means the worker ID exists but belongs to another org
    if result.rows_affected() == 0 {
        warn!(
            worker_id = %req.worker_id,
            org_id = %auth.organization_id,
            "Worker ID already exists for a different organization"
        );
        return Err((
            StatusCode::CONFLICT,
            "Worker ID already in use by another organization".to_string(),
        ));
    }

    state.metrics.workers_active.inc();
    state.metrics.workers_healthy.inc();

    Ok(Json(RegisterWorkerResponse {
        registered: true,
        heartbeat_interval_seconds: 10, // Default heartbeat interval
    }))
}

/// Now uses authenticated organization context to validate worker ownership.
/// Previously any client could send heartbeats for ANY worker, potentially keeping
/// malicious workers alive or disrupting legitimate workers.
/// Validates status against allowed enum values
async fn heartbeat_handler(
    State(state): State<GrpcState>,
    axum::Extension(auth): axum::Extension<GrpcAuthContext>,
    Json(req): Json<HeartbeatRequest>,
) -> Result<Json<HeartbeatResponse>, (StatusCode, String)> {
    // Validate status against allowed values
    let safe_status = if VALID_WORKER_STATUSES.contains(&req.status.as_str()) {
        req.status.clone()
    } else {
        warn!(
            status = %req.status,
            "Invalid worker status, defaulting to healthy"
        );
        "healthy".to_string()
    };

    let result = sqlx::query(
        r#"
        UPDATE workers
        SET 
            last_heartbeat = NOW(),
            status = $1,
            current_job_count = $2,
            updated_at = NOW()
        WHERE id = $3 AND organization_id = $4
        "#,
    )
    .bind(&safe_status) // Use validated status
    .bind(req.current_job_ids.len() as i32)
    .bind(&req.worker_id)
    .bind(&auth.organization_id) // Validate organization ownership
    .execute(state.db.pool())
    .await
    // Sanitize error
    .map_err(|e| {
        error!(error = %e, "Failed to process heartbeat");
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            "Heartbeat failed".to_string(),
        )
    })?;

    Ok(Json(HeartbeatResponse {
        acknowledged: result.rows_affected() > 0,
    }))
}

/// Get job by ID
///
/// Requires organization context to prevent cross-tenant access
async fn get_job_handler(
    State(state): State<GrpcState>,
    axum::Extension(auth): axum::Extension<GrpcAuthContext>,
    Json(req): Json<GetJobRequest>,
) -> Result<Json<GetJobResponse>, (StatusCode, String)> {
    let job: Option<crate::models::Job> =
        sqlx::query_as("SELECT * FROM jobs WHERE id = $1 AND organization_id = $2")
            .bind(&req.job_id)
            .bind(&auth.organization_id)
            .fetch_optional(state.db.pool())
            .await
            .map_err(|e| {
                error!(error = %e, "Failed to get job");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "Failed to get job".to_string(),
                )
            })?;

    let grpc_job = job.map(|j| GrpcJob {
        id: j.id,
        organization_id: j.organization_id,
        queue_name: j.queue_name,
        status: GrpcJobStatus::from(j.status.as_str()),
        payload: j.payload.to_string(),
        result: j.result.map(|r| r.to_string()),
        retry_count: j.retry_count,
        max_retries: j.max_retries,
        last_error: j.last_error,
        created_at: Some(j.created_at),
        scheduled_at: j.scheduled_at,
        started_at: j.started_at,
        completed_at: j.completed_at,
        expires_at: j.expires_at,
        priority: j.priority,
        tags: j.tags.map(|t| t.to_string()),
        timeout_seconds: j.timeout_seconds,
        parent_job_id: j.parent_job_id,
        completion_webhook: j.completion_webhook,
        assigned_worker_id: j.assigned_worker_id,
        lease_id: j.lease_id,
        lease_expires_at: j.lease_expires_at,
        idempotency_key: j.idempotency_key,
    });

    Ok(Json(GetJobResponse { job: grpc_job }))
}

/// Get queue statistics
///
/// Requires organization context for tenant isolation
async fn get_queue_stats_handler(
    State(state): State<GrpcState>,
    axum::Extension(auth): axum::Extension<GrpcAuthContext>,
    Json(req): Json<GetQueueStatsRequest>,
) -> Result<Json<GetQueueStatsResponse>, (StatusCode, String)> {
    // Validate queue name
    if req.queue_name.is_empty() || req.queue_name.len() > 255 {
        return Err((StatusCode::BAD_REQUEST, "Invalid queue name".to_string()));
    }

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
            EXTRACT(EPOCH FROM (NOW() - MIN(created_at)))::BIGINT * 1000 FILTER (WHERE status = 'pending') as max_age_ms
        FROM jobs
        WHERE queue_name = $1 AND organization_id = $2
        "#
    )
    .bind(&req.queue_name)
    .bind(&auth.organization_id)
    .fetch_one(state.db.pool())
    .await
    .map_err(|e| {
        error!(error = %e, "Failed to get queue stats");
        (StatusCode::INTERNAL_SERVER_ERROR, "Failed to get queue stats".to_string())
    })?;

    Ok(Json(GetQueueStatsResponse {
        queue_name: req.queue_name,
        pending: stats.0,
        scheduled: stats.1,
        processing: stats.2,
        completed: stats.3,
        failed: stats.4,
        deadletter: stats.5,
        total: stats.6,
        max_age_ms: stats.7,
    }))
}

/// Deregister a worker
///
/// Requires organization context to prevent cross-tenant worker manipulation
async fn deregister_handler(
    State(state): State<GrpcState>,
    axum::Extension(auth): axum::Extension<GrpcAuthContext>,
    Json(req): Json<DeregisterRequest>,
) -> Result<Json<DeregisterResponse>, (StatusCode, String)> {
    // Mark worker as offline (with organization check)
    let result = sqlx::query(
        "UPDATE workers SET status = 'offline', updated_at = NOW() WHERE id = $1 AND organization_id = $2"
    )
    .bind(&req.worker_id)
    .bind(&auth.organization_id)
    .execute(state.db.pool())
    .await
    .map_err(|e| {
        error!(error = %e, "Failed to deregister worker");
        (StatusCode::INTERNAL_SERVER_ERROR, "Failed to deregister worker".to_string())
    })?;

    if result.rows_affected() > 0 {
        state.metrics.workers_active.dec();
        info!(worker_id = %req.worker_id, "Worker deregistered via gRPC");
    }

    Ok(Json(DeregisterResponse {
        success: result.rows_affected() > 0,
    }))
}

/// Query parameters for StreamJobs SSE endpoint
#[derive(Debug, Clone, serde::Deserialize)]
pub struct StreamJobsQuery {
    pub queue_name: String,
    pub worker_id: String,
    #[serde(default = "default_lease_duration")]
    pub lease_duration_secs: i32,
}

fn default_lease_duration() -> i32 {
    300 // 5 minutes default
}

/// Stream jobs to a worker using Server-Sent Events
///
/// This implements the StreamJobs gRPC method using SSE for HTTP compatibility.
/// Jobs are polled from the queue and streamed to the client as they become available.
///
/// The worker should:
/// 1. Connect to this endpoint with valid authentication
/// 2. Receive job events as SSE messages
/// 3. Process each job and call Complete or Fail endpoints
/// 4. Reconnect if the connection drops
async fn stream_jobs_handler(
    State(state): State<GrpcState>,
    axum::Extension(auth): axum::Extension<GrpcAuthContext>,
    Query(params): Query<StreamJobsQuery>,
) -> Result<Sse<impl Stream<Item = Result<Event, Infallible>>>, (StatusCode, String)> {
    // Validate queue name
    if params.queue_name.is_empty() || params.queue_name.len() > 255 {
        return Err((StatusCode::BAD_REQUEST, "Invalid queue name".to_string()));
    }

    // Validate worker_id
    if params.worker_id.is_empty() || params.worker_id.len() > 255 {
        return Err((StatusCode::BAD_REQUEST, "Invalid worker_id".to_string()));
    }

    // Clamp lease duration to safe range
    let lease_duration = params
        .lease_duration_secs
        .clamp(MIN_LEASE_DURATION_SECS, MAX_LEASE_DURATION_SECS);

    let org_id = auth.organization_id.clone();
    let queue_name = params.queue_name.clone();
    let worker_id = params.worker_id.clone();
    let db = state.db.clone();
    let metrics = state.metrics.clone();

    // Create a stream that polls for jobs
    let stream = stream::unfold(
        (db, org_id, queue_name, worker_id, lease_duration, metrics),
        |(db, org_id, queue_name, worker_id, lease_duration, metrics)| async move {
            // Poll interval - check for new jobs every 1 second
            tokio::time::sleep(Duration::from_secs(1)).await;

            // Try to dequeue a job
            let lease_id = uuid::Uuid::new_v4().to_string();
            let job_result: Result<Option<crate::models::Job>, _> = sqlx::query_as(
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
                    AND status = 'pending'
                    AND (scheduled_at IS NULL OR scheduled_at <= NOW())
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
            .fetch_optional(db.pool())
            .await;

            let event = match job_result {
                Ok(Some(job)) => {
                    metrics.jobs_processing.inc();
                    debug!(job_id = %job.id, queue = %queue_name, "Streamed job to worker");

                    // Convert to GrpcJob
                    let grpc_job = GrpcJob {
                        id: job.id,
                        organization_id: job.organization_id,
                        queue_name: job.queue_name,
                        status: GrpcJobStatus::from(job.status.as_str()),
                        payload: job.payload.to_string(),
                        result: job.result.map(|r| r.to_string()),
                        retry_count: job.retry_count,
                        max_retries: job.max_retries,
                        last_error: job.last_error,
                        created_at: Some(job.created_at),
                        scheduled_at: job.scheduled_at,
                        started_at: job.started_at,
                        completed_at: job.completed_at,
                        expires_at: job.expires_at,
                        priority: job.priority,
                        tags: job.tags.map(|t| t.to_string()),
                        timeout_seconds: job.timeout_seconds,
                        parent_job_id: job.parent_job_id,
                        completion_webhook: job.completion_webhook,
                        assigned_worker_id: job.assigned_worker_id,
                        lease_id: job.lease_id,
                        lease_expires_at: job.lease_expires_at,
                        idempotency_key: job.idempotency_key,
                    };

                    Event::default()
                        .event("job")
                        .json_data(&grpc_job)
                        .unwrap_or_else(|_| Event::default().data("error"))
                }
                Ok(None) => {
                    // No job available, send keepalive
                    Event::default().event("keepalive").data("")
                }
                Err(e) => {
                    error!(error = %e, "Error polling for jobs");
                    Event::default().event("error").data("Internal error")
                }
            };

            Some((
                Ok(event),
                (db, org_id, queue_name, worker_id, lease_duration, metrics),
            ))
        },
    );

    Ok(Sse::new(stream).keep_alive(KeepAlive::default()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_grpc_state_clone() {
        // Just ensure the state is Clone
        fn assert_clone<T: Clone>() {}
        assert_clone::<GrpcState>();
    }

    #[test]
    fn test_enqueue_request_defaults() {
        let req = EnqueueJobRequest {
            organization_id: "org-1".to_string(),
            queue_name: "test".to_string(),
            payload: "{}".to_string(),
            priority: 0,
            max_retries: 3,
            timeout_seconds: 300,
            scheduled_at: None,
            idempotency_key: None,
            tags: None,
            completion_webhook: None,
            parent_job_id: None,
            expires_at: None,
        };

        assert_eq!(req.priority, 0);
        assert_eq!(req.max_retries, 3);
    }
}
