//! Worker handlers

use axum::{
    extract::{Extension, Path, State},
    http::StatusCode,
    Json,
};
use chrono::Utc;
use uuid::Uuid;

use crate::api::middleware::ValidatedJson;
use crate::api::AppState;
use crate::error::{AppError, AppResult};
use crate::models::{
    ApiKeyContext, RegisterWorkerRequest, RegisterWorkerResponse, Worker, WorkerHeartbeatRequest,
    WorkerSummary,
};

/// Maximum workers per page
const MAX_WORKERS_PER_PAGE: i64 = 100;

/// List all workers
///
/// Now filters by authenticated organization
/// Now uses configurable limit constant
pub async fn list(
    State(state): State<AppState>,
    Extension(ctx): Extension<ApiKeyContext>,
) -> AppResult<Json<Vec<WorkerSummary>>> {
    // Use constant instead of hardcoded value
    let workers = sqlx::query_as::<_, Worker>(
        "SELECT * FROM workers WHERE organization_id = $1 ORDER BY last_heartbeat DESC LIMIT $2",
    )
    .bind(&ctx.organization_id)
    .bind(MAX_WORKERS_PER_PAGE)
    .fetch_all(state.db.pool())
    .await?;

    let summaries: Vec<WorkerSummary> = workers.into_iter().map(Into::into).collect();
    Ok(Json(summaries))
}

/// Register a new worker
///
/// Now uses authenticated organization context
/// Now validates queue exists and API key has permission for queue
pub async fn register(
    State(state): State<AppState>,
    Extension(ctx): Extension<ApiKeyContext>,
    ValidatedJson(request): ValidatedJson<RegisterWorkerRequest>,
) -> AppResult<(StatusCode, Json<RegisterWorkerResponse>)> {
    // Validate queue name characters
    if !request
        .queue_name
        .chars()
        .all(|c| c.is_alphanumeric() || c == '-' || c == '_' || c == '.')
    {
        return Err(AppError::Validation(
            "Queue name contains invalid characters".to_string(),
        ));
    }

    // Check if API key has permission for this queue
    // Empty queues list means all queues allowed, otherwise must be in list
    if !ctx.queues.is_empty()
        && !ctx.queues.contains(&request.queue_name)
        && !ctx.queues.contains(&"*".to_string())
    {
        return Err(AppError::Authorization(format!(
            "API key does not have permission for queue '{}'",
            request.queue_name
        )));
    }

    // Check if queue config exists for this org (optional - queue can be auto-created)
    // FIX: Table name was wrong (queue_configs -> queue_config)
    let queue_exists: Option<(i64,)> = sqlx::query_as(
        "SELECT COUNT(*) FROM queue_config WHERE organization_id = $1 AND queue_name = $2",
    )
    .bind(&ctx.organization_id)
    .bind(&request.queue_name)
    .fetch_optional(state.db.pool())
    .await?;

    // Log warning if queue doesn't exist (but allow registration - queue will be auto-created)
    if queue_exists.map(|(c,)| c).unwrap_or(0) == 0 {
        tracing::warn!(
            queue_name = %request.queue_name,
            org_id = %ctx.organization_id,
            "Worker registering for queue that doesn't have explicit config - will use defaults"
        );
    }

    let worker_id = Uuid::new_v4().to_string();
    let now = Utc::now();
    let max_concurrency = request.max_concurrency.unwrap_or(5);

    // Use queue_names (array) column instead of queue_name (string)
    // to match the database schema used by the worker module
    //
    // Previously, an attacker could update another org's worker status by using their ID.
    let result = sqlx::query(
        r#"
        INSERT INTO workers (
            id, organization_id, queue_names, hostname, worker_type,
            max_concurrent_jobs, current_job_count, status, last_heartbeat,
            metadata, version, created_at, updated_at
        )
        VALUES ($1, $2, $3, $4, $5, $6, 0, 'healthy', $7, $8, $9, $7, $7)
        ON CONFLICT (id) DO UPDATE SET
            status = 'healthy',
            last_heartbeat = NOW(),
            updated_at = NOW()
        WHERE workers.organization_id = EXCLUDED.organization_id
        "#,
    )
    .bind(&worker_id)
    .bind(&ctx.organization_id)
    .bind(vec![&request.queue_name]) // Convert to array
    .bind(&request.hostname)
    .bind(&request.worker_type)
    .bind(max_concurrency) // Use max_concurrent_jobs
    .bind(now)
    .bind(request.metadata.unwrap_or(serde_json::json!({})))
    .bind(&request.version)
    .execute(state.db.pool())
    .await?;

    // Check if registration succeeded - if rows_affected is 0, worker ID exists for different org
    if result.rows_affected() == 0 {
        return Err(AppError::Conflict("Worker ID already in use".to_string()));
    }

    state.metrics.workers_active.inc();
    state.metrics.workers_healthy.inc();

    Ok((
        StatusCode::CREATED,
        Json(RegisterWorkerResponse {
            id: worker_id,
            queue_name: request.queue_name,
            lease_duration_secs: state.settings.worker.lease_duration_secs,
            heartbeat_interval_secs: state.settings.worker.heartbeat_interval_secs,
        }),
    ))
}

/// Get a worker by ID
///
/// Now returns generic error without exposing worker ID
pub async fn get(
    State(state): State<AppState>,
    Extension(ctx): Extension<ApiKeyContext>,
    Path(id): Path<String>,
) -> AppResult<Json<Worker>> {
    let worker =
        sqlx::query_as::<_, Worker>("SELECT * FROM workers WHERE id = $1 AND organization_id = $2")
            .bind(&id)
            .bind(&ctx.organization_id)
            .fetch_optional(state.db.pool())
            .await?
            // Don't expose worker ID in error message
            .ok_or_else(|| AppError::NotFound("Worker not found".to_string()))?;

    Ok(Json(worker))
}

/// Valid worker statuses
const VALID_WORKER_STATUSES: &[&str] = &["healthy", "degraded", "draining", "offline"];

/// Send worker heartbeat
///
/// Now validates status against allowed values
pub async fn heartbeat(
    State(state): State<AppState>,
    Extension(ctx): Extension<ApiKeyContext>,
    Path(id): Path<String>,
    Json(request): Json<WorkerHeartbeatRequest>,
) -> AppResult<StatusCode> {
    // Validate and sanitize status
    let status = request.status.as_deref().unwrap_or("healthy");
    let validated_status = if VALID_WORKER_STATUSES.contains(&status) {
        status.to_string()
    } else {
        tracing::warn!(status = %status, worker_id = %id, "Invalid worker status, defaulting to healthy");
        "healthy".to_string()
    };

    // Use validated status
    let result = sqlx::query(
        r#"
        UPDATE workers 
        SET 
            last_heartbeat = NOW(),
            current_jobs = $1,
            status = $2,
            metadata = COALESCE($3, metadata)
        WHERE id = $4 AND organization_id = $5
        "#,
    )
    .bind(request.current_jobs)
    .bind(&validated_status)
    .bind(request.metadata)
    .bind(&id)
    .bind(&ctx.organization_id)
    .execute(state.db.pool())
    .await?;

    if result.rows_affected() == 0 {
        // Don't expose worker ID in error message
        return Err(AppError::NotFound("Worker not found".to_string()));
    }

    Ok(StatusCode::OK)
}

/// Deregister a worker
///
pub async fn deregister(
    State(state): State<AppState>,
    Extension(ctx): Extension<ApiKeyContext>,
    Path(id): Path<String>,
) -> AppResult<StatusCode> {
    // Get worker's current status before updating (with org check)
    let worker_status: Option<(String,)> =
        sqlx::query_as("SELECT status FROM workers WHERE id = $1 AND organization_id = $2")
            .bind(&id)
            .bind(&ctx.organization_id)
            .fetch_optional(state.db.pool())
            .await?;

    let Some((previous_status,)) = worker_status else {
        // Don't expose worker ID in error message
        return Err(AppError::NotFound("Worker not found".to_string()));
    };

    // Release any jobs assigned to this worker (with org isolation)
    sqlx::query(
        r#"
        UPDATE jobs 
        SET 
            status = 'pending',
            assigned_worker_id = NULL,
            lease_id = NULL,
            lease_expires_at = NULL
        WHERE assigned_worker_id = $1 AND status = 'processing' AND organization_id = $2
        "#,
    )
    .bind(&id)
    .bind(&ctx.organization_id)
    .execute(state.db.pool())
    .await?;

    // Mark worker as offline (with org isolation)
    sqlx::query("UPDATE workers SET status = 'offline' WHERE id = $1 AND organization_id = $2")
        .bind(&id)
        .bind(&ctx.organization_id)
        .execute(state.db.pool())
        .await?;

    // Only decrement healthy metric if worker was actually healthy
    state.metrics.workers_active.dec();
    if previous_status == "healthy" {
        state.metrics.workers_healthy.dec();
    }

    // Notify Redis that worker deregistered
    if let Some(ref cache) = state.cache {
        // Notify organization-scoped channel for any listeners
        let _ = cache
            .publish(
                &format!("org:{}:workers", ctx.organization_id),
                &format!("deregistered:{}", id),
            )
            .await;
    }

    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_register_response_serialization() {
        let response = RegisterWorkerResponse {
            id: "worker-1".to_string(),
            queue_name: "default".to_string(),
            lease_duration_secs: 30,
            heartbeat_interval_secs: 10,
        };

        let json = serde_json::to_string(&response).unwrap();
        assert!(json.contains("worker-1"));
        assert!(json.contains("default"));
    }
}
