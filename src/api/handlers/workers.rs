//! Worker handlers

use axum::{
    extract::{Extension, Path, State},
    http::StatusCode,
    Json,
};
use chrono::Utc;
use uuid::Uuid;

use crate::api::middleware::limits::{check_resource_limit_conn, lock_resource};
use crate::api::middleware::ValidatedJson;
use crate::api::AppState;
use crate::error::{AppError, AppResult};
use crate::models::{
    ApiKeyContext, RegisterWorkerRequest, RegisterWorkerResponse, Worker, WorkerHeartbeatRequest,
    WorkerResponse, WorkerSummary,
};

/// Maximum workers per page
const MAX_WORKERS_PER_PAGE: i64 = 100;

fn effective_worker_queues(worker: &Worker) -> Vec<String> {
    if worker.queue_names.is_empty() {
        vec![worker.queue_name.clone()]
    } else {
        worker.queue_names.clone()
    }
}

fn ensure_worker_in_scope(ctx: &ApiKeyContext, worker: &Worker) -> AppResult<()> {
    let queues = effective_worker_queues(worker);
    if ctx.can_access_all_queues(&queues) {
        Ok(())
    } else {
        Err(AppError::NotFound("Worker not found".to_string()))
    }
}

/// List all workers
///
/// Now filters by authenticated organization
/// Now uses configurable limit constant
pub async fn list(
    State(state): State<AppState>,
    Extension(ctx): Extension<ApiKeyContext>,
) -> AppResult<Json<Vec<WorkerSummary>>> {
    // Use constant instead of hardcoded value
    let workers = if ctx.is_unrestricted() {
        sqlx::query_as::<_, Worker>(
            "SELECT * FROM workers WHERE organization_id = $1 ORDER BY last_heartbeat DESC LIMIT $2",
        )
        .bind(&ctx.organization_id)
        .bind(MAX_WORKERS_PER_PAGE)
        .fetch_all(state.db.pool())
        .await?
    } else {
        sqlx::query_as::<_, Worker>(
            r#"
            SELECT * FROM workers
            WHERE organization_id = $1
              AND COALESCE(queue_names, ARRAY[]::TEXT[]) <@ $2::TEXT[]
              AND (COALESCE(cardinality(queue_names), 0) > 0 OR queue_name = ANY($2::TEXT[]))
            ORDER BY last_heartbeat DESC
            LIMIT $3
            "#,
        )
        .bind(&ctx.organization_id)
        .bind(&ctx.queues)
        .bind(MAX_WORKERS_PER_PAGE)
        .fetch_all(state.db.pool())
        .await?
    };

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
    if !ctx.can_access_queue(&request.queue_name) {
        return Err(AppError::Authorization(format!(
            "API key does not have permission for queue '{}'",
            request.queue_name
        )));
    }

    // Check if queue config exists for this org (optional - queue can be auto-created)
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

    // A caller-supplied id makes this a true upsert, so a restarting worker
    // reclaims its row instead of leaking a new one that occupies the plan's
    // worker cap until the stale-worker reaper catches it. When omitted we mint
    // a fresh UUID, which cannot collide — the `ON CONFLICT` guard below is then
    // unreachable, and that is fine: it exists for the supplied-id case.
    let worker_id = request
        .worker_id
        .clone()
        .unwrap_or_else(|| Uuid::new_v4().to_string());
    let now = Utc::now();
    let max_concurrency = request.max_concurrency.unwrap_or(5);

    // Enforce the workers cap atomically under an advisory lock so concurrent
    // registrations for this org cannot overshoot the plan limit.
    let mut tx = state.db.pool().begin().await?;
    lock_resource(&mut tx, &ctx.organization_id, "workers").await?;

    // Re-registering an id this org already owns adds no worker, so it must not
    // be charged against the cap — otherwise a worker at the limit could not
    // restart. Counted inside the advisory lock so the answer cannot change
    // between here and the upsert.
    let already_registered: Option<(i64,)> = sqlx::query_as(
        "SELECT COUNT(*) FROM workers WHERE id = $1 AND organization_id = $2 AND status IN ('healthy', 'degraded')",
    )
    .bind(&worker_id)
    .bind(&ctx.organization_id)
    .fetch_optional(&mut *tx)
    .await?;
    let adding = if already_registered.map(|(c,)| c).unwrap_or(0) > 0 {
        0
    } else {
        1
    };

    if let Err(response) =
        check_resource_limit_conn(&mut tx, &ctx.organization_id, "workers", adding).await
    {
        return Err(AppError::LimitExceeded(Box::new(response)));
    }

    // Set both queue_name (legacy, NOT NULL) and queue_names (array, new)
    // The queue_name column has a NOT NULL constraint from initial migration.
    //
    // Previously, an attacker could update another org's worker status by using their ID.
    let result = sqlx::query(
        r#"
        INSERT INTO workers (
            id, organization_id, queue_name, queue_names, hostname, worker_type,
            max_concurrent_jobs, current_job_count, status, last_heartbeat,
            metadata, version, created_at, updated_at
        )
        VALUES ($1, $2, $3, $4, $5, $6, $7, 0, 'healthy', $8, $9, $10, $8, $8)
        ON CONFLICT (id) DO UPDATE SET
            status = 'healthy',
            last_heartbeat = NOW(),
            -- Re-registration refreshes the worker's declared capabilities: a
            -- restarted process may target a different queue or concurrency,
            -- and silently keeping the old row's values would misreport it.
            queue_name = EXCLUDED.queue_name,
            queue_names = EXCLUDED.queue_names,
            hostname = EXCLUDED.hostname,
            worker_type = EXCLUDED.worker_type,
            max_concurrent_jobs = EXCLUDED.max_concurrent_jobs,
            metadata = EXCLUDED.metadata,
            version = EXCLUDED.version,
            updated_at = NOW()
        -- Guards a caller-supplied id from hijacking another org's worker row.
        WHERE workers.organization_id = EXCLUDED.organization_id
        "#,
    )
    .bind(&worker_id)
    .bind(&ctx.organization_id)
    .bind(&request.queue_name) // Legacy queue_name (NOT NULL)
    .bind(vec![&request.queue_name]) // New queue_names array
    .bind(&request.hostname)
    .bind(&request.worker_type)
    .bind(max_concurrency) // Use max_concurrent_jobs
    .bind(now)
    .bind(request.metadata.unwrap_or(serde_json::json!({})))
    .bind(&request.version)
    .execute(&mut *tx)
    .await?;

    // Check if registration succeeded - if rows_affected is 0, worker ID exists for different org
    if result.rows_affected() == 0 {
        return Err(AppError::Conflict("Worker ID already in use".to_string()));
    }

    tx.commit().await?;

    // Only a NEW worker moves the gauges. Registration is an upsert now, so a
    // restart re-registering its stable id would otherwise increment on every
    // restart while `deregister` decrements once — the exact drift B-09 fixed
    // from the other direction. `adding` already encodes "did this add a row".
    if adding > 0 {
        state.metrics.workers_active.inc();
        state.metrics.workers_healthy.inc();
    }

    state.outgoing_webhooks.spawn_dispatch(
        ctx.organization_id.clone(),
        "worker.registered".to_string(),
        worker_id.clone(),
        serde_json::json!({
            "worker_id": worker_id,
            "queue_name": request.queue_name,
            "hostname": request.hostname,
        }),
    );

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
) -> AppResult<Json<WorkerResponse>> {
    let worker =
        sqlx::query_as::<_, Worker>("SELECT * FROM workers WHERE id = $1 AND organization_id = $2")
            .bind(&id)
            .bind(&ctx.organization_id)
            .fetch_optional(state.db.pool())
            .await?
            // Don't expose worker ID in error message
            .ok_or_else(|| AppError::NotFound("Worker not found".to_string()))?;

    ensure_worker_in_scope(&ctx, &worker)?;

    Ok(Json(worker.into()))
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
    ValidatedJson(request): ValidatedJson<WorkerHeartbeatRequest>,
) -> AppResult<StatusCode> {
    // Validate and sanitize status
    let status = request.status.as_deref().unwrap_or("healthy");
    let validated_status = if VALID_WORKER_STATUSES.contains(&status) {
        status.to_string()
    } else {
        tracing::warn!(status = %status, worker_id = %id, "Invalid worker status, defaulting to healthy");
        "healthy".to_string()
    };

    let worker =
        sqlx::query_as::<_, Worker>("SELECT * FROM workers WHERE id = $1 AND organization_id = $2")
            .bind(&id)
            .bind(&ctx.organization_id)
            .fetch_optional(state.db.pool())
            .await?
            .ok_or_else(|| AppError::NotFound("Worker not found".to_string()))?;
    ensure_worker_in_scope(&ctx, &worker)?;

    // Use validated status
    let result = sqlx::query(
        r#"
        UPDATE workers 
        SET 
            last_heartbeat = NOW(),
            current_job_count = $1,
            status = $2,
            metadata = COALESCE($3, metadata),
            updated_at = NOW()
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
    let worker =
        sqlx::query_as::<_, Worker>("SELECT * FROM workers WHERE id = $1 AND organization_id = $2")
            .bind(&id)
            .bind(&ctx.organization_id)
            .fetch_optional(state.db.pool())
            .await?
            .ok_or_else(|| AppError::NotFound("Worker not found".to_string()))?;
    ensure_worker_in_scope(&ctx, &worker)?;
    let previous_status = worker.status;

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

    // Deregistering an already-offline worker is a no-op for the gauges: this
    // endpoint is idempotent from a client's perspective (a second DELETE also
    // returns 204), so an unguarded decrement drifted `workers_active` negative
    // and it never recovered until process restart.
    if previous_status != "offline" {
        state.metrics.workers_active.dec();
    }
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

    state.outgoing_webhooks.spawn_dispatch(
        ctx.organization_id.clone(),
        "worker.deregistered".to_string(),
        id.clone(),
        serde_json::json!({
            "worker_id": id,
            "queue_name": worker.queue_name,
        }),
    );

    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    use super::*;
    use validator::Validate;

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
        assert!(json.contains("\"lease_duration_secs\":30"));
        assert!(json.contains("\"heartbeat_interval_secs\":10"));
    }

    #[test]
    fn test_register_worker_request_validation() {
        // Valid request
        let valid = RegisterWorkerRequest {
            worker_id: None,
            queue_name: "emails".to_string(),
            hostname: "worker-host-1".to_string(),
            worker_type: Some("http".to_string()),
            max_concurrency: Some(10),
            metadata: None,
            version: Some("1.0.0".to_string()),
        };
        assert!(valid.validate().is_ok());

        // Queue name too short
        let short_queue = RegisterWorkerRequest {
            worker_id: None,
            queue_name: "".to_string(),
            hostname: "host".to_string(),
            worker_type: Some("http".to_string()),
            max_concurrency: Some(10),
            metadata: None,
            version: None,
        };
        assert!(short_queue.validate().is_err());

        // Hostname too short
        let short_host = RegisterWorkerRequest {
            worker_id: None,
            queue_name: "default".to_string(),
            hostname: "".to_string(),
            worker_type: Some("http".to_string()),
            max_concurrency: Some(10),
            metadata: None,
            version: None,
        };
        assert!(short_host.validate().is_err());
    }

    #[test]
    fn test_worker_heartbeat_request_deserialization() {
        let json = r#"{"status":"healthy","current_jobs":5}"#;
        let request: WorkerHeartbeatRequest = serde_json::from_str(json).unwrap();
        assert_eq!(request.status.as_deref(), Some("healthy"));
        assert_eq!(request.current_jobs, 5);
    }

    #[test]
    fn test_worker_heartbeat_request_defaults() {
        let json = r#"{"current_jobs":0}"#;
        let request: WorkerHeartbeatRequest = serde_json::from_str(json).unwrap();
        assert_eq!(request.status, None);
        assert_eq!(request.current_jobs, 0);
    }

    #[test]
    fn test_worker_summary_serialization() {
        let summary = WorkerSummary {
            id: "worker-abc123".to_string(),
            queue_name: "default".to_string(),
            hostname: "worker-host".to_string(),
            status: "healthy".to_string(),
            current_jobs: 3,
            max_concurrency: 10,
            last_heartbeat: Utc::now(),
        };

        let json = serde_json::to_string(&summary).unwrap();
        assert!(json.contains("worker-abc123"));
        assert!(json.contains("\"status\":\"healthy\""));
        assert!(json.contains("\"current_jobs\":3"));
    }

    #[test]
    fn test_max_workers_per_page_constant() {
        assert_eq!(MAX_WORKERS_PER_PAGE, 100);
    }

    #[test]
    fn test_queue_name_validation_in_register() {
        // Valid queue names
        assert!("default"
            .chars()
            .all(|c| c.is_alphanumeric() || c == '-' || c == '_' || c == '.'));
        assert!("my-queue"
            .chars()
            .all(|c| c.is_alphanumeric() || c == '-' || c == '_' || c == '.'));
        assert!("my_queue"
            .chars()
            .all(|c| c.is_alphanumeric() || c == '-' || c == '_' || c == '.'));
        assert!("queue.v2"
            .chars()
            .all(|c| c.is_alphanumeric() || c == '-' || c == '_' || c == '.'));

        // Invalid queue names
        assert!(!"my queue"
            .chars()
            .all(|c| c.is_alphanumeric() || c == '-' || c == '_' || c == '.'));
        assert!(!"my@queue"
            .chars()
            .all(|c| c.is_alphanumeric() || c == '-' || c == '_' || c == '.'));
    }
}
