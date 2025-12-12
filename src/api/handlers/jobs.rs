//! Job handlers

use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    Json,
};
use chrono::Utc;
use tracing::info;
use uuid::Uuid;

use crate::api::middleware::limits::{check_job_limits, check_payload_size, increment_daily_jobs};
use crate::api::middleware::ValidatedJson;
use crate::api::AppState;
use crate::error::{AppError, AppResult};
use crate::models::ApiKeyContext;
use crate::models::{
    CreateJobRequest, CreateJobResponse, Job, JobStats, JobSummary, ListJobsQuery,
};
use axum::extract::Extension;

/// Maximum jobs per list request
/// Reduced from 1000 to prevent memory exhaustion
const MAX_JOBS_PER_PAGE: i64 = 100;

/// Valid job statuses for filtering
const VALID_JOB_STATUSES: &[&str] = &[
    "pending",
    "scheduled",
    "processing",
    "completed",
    "failed",
    "deadletter",
    "cancelled",
];

/// Validate queue name for safe characters
/// Prevents SQL injection via queue_name filter
fn validate_queue_name_filter(name: &str) -> bool {
    name.len() <= 255
        && name
            .chars()
            .all(|c| c.is_alphanumeric() || c == '-' || c == '_' || c == '.')
}

/// List jobs with optional filtering
///
/// Reduced max limit from 1000 to 100
pub async fn list(
    State(state): State<AppState>,
    Extension(ctx): Extension<ApiKeyContext>,
    Query(query): Query<ListJobsQuery>,
) -> AppResult<Json<Vec<JobSummary>>> {
    // Cap limit at 100 to prevent memory exhaustion
    let limit = query.limit.unwrap_or(50).clamp(1, MAX_JOBS_PER_PAGE);
    let offset = query.offset.unwrap_or(0).max(0);

    // Validate queue_name to prevent injection
    let validated_queue = query.queue_name.as_ref().and_then(|q| {
        if validate_queue_name_filter(q) {
            Some(q.clone())
        } else {
            tracing::warn!(queue_name = %q, "Invalid queue_name filter ignored");
            None
        }
    });

    // Validate status parameter
    let validated_status = query.status.as_ref().and_then(|s| {
        if VALID_JOB_STATUSES.contains(&s.as_str()) {
            Some(s.clone())
        } else {
            tracing::warn!(status = %s, "Invalid job status filter ignored");
            None
        }
    });
    let org_id = &ctx.organization_id;

    // All queries now include organization_id filter
    // Use validated_status instead of raw query.status
    // Use validated_queue instead of raw query.queue_name
    let jobs = match (&validated_queue, &validated_status) {
        (Some(queue), Some(status)) => {
            sqlx::query_as::<_, Job>(
                "SELECT * FROM jobs WHERE organization_id = $1 AND queue_name = $2 AND status = $3 ORDER BY created_at DESC LIMIT $4 OFFSET $5",
            )
            .bind(org_id)
            .bind(queue)
            .bind(status)
            .bind(limit)
            .bind(offset)
            .fetch_all(state.db.pool())
            .await?
        }
        (Some(queue), None) => {
            sqlx::query_as::<_, Job>(
                "SELECT * FROM jobs WHERE organization_id = $1 AND queue_name = $2 ORDER BY created_at DESC LIMIT $3 OFFSET $4",
            )
            .bind(org_id)
            .bind(queue)
            .bind(limit)
            .bind(offset)
            .fetch_all(state.db.pool())
            .await?
        }
        (None, Some(status)) => {
            sqlx::query_as::<_, Job>(
                "SELECT * FROM jobs WHERE organization_id = $1 AND status = $2 ORDER BY created_at DESC LIMIT $3 OFFSET $4",
            )
            .bind(org_id)
            .bind(status)
            .bind(limit)
            .bind(offset)
            .fetch_all(state.db.pool())
            .await?
        }
        (None, None) => {
            sqlx::query_as::<_, Job>(
                "SELECT * FROM jobs WHERE organization_id = $1 ORDER BY created_at DESC LIMIT $2 OFFSET $3",
            )
            .bind(org_id)
            .bind(limit)
            .bind(offset)
            .fetch_all(state.db.pool())
            .await?
        }
    };

    let summaries: Vec<JobSummary> = jobs.into_iter().map(Into::into).collect();
    Ok(Json(summaries))
}

/// Create a new job
///
/// Now requires authenticated context - previously fell back to "default-org"
/// SECURITY: Enforces plan limits before creating jobs
pub async fn create(
    State(state): State<AppState>,
    Extension(ctx): Extension<ApiKeyContext>,
    ValidatedJson(request): ValidatedJson<CreateJobRequest>,
) -> AppResult<(StatusCode, Json<CreateJobResponse>)> {
    let org_id = ctx.organization_id;

    // Check payload size against plan limits
    let payload_json = serde_json::to_string(&request.payload).unwrap_or_default();
    if let Err(response) = check_payload_size(state.db.pool(), &org_id, payload_json.len()).await {
        return Err(AppError::LimitExceeded(Box::new(response)));
    }

    // Check job limits (daily + active) before creating
    if let Err(response) = check_job_limits(state.db.pool(), &org_id, 1).await {
        return Err(AppError::LimitExceeded(Box::new(response)));
    }

    let job_id = Uuid::new_v4().to_string();
    let now = Utc::now();

    let priority = request.priority.unwrap_or(0);
    let max_retries = request
        .max_retries
        .unwrap_or(state.settings.queue.default_max_retries);
    let timeout_seconds = request
        .timeout_seconds
        .unwrap_or(state.settings.queue.default_timeout_secs);

    // Determine initial status
    let initial_status = if request.scheduled_at.is_some() && request.scheduled_at > Some(now) {
        "scheduled"
    } else {
        "pending"
    };

    // Use FIXED idempotency pattern: DO UPDATE SET RETURNING
    // This always returns an ID whether the job is new or existing
    let (returned_id,): (String,) = sqlx::query_as(
        r#"
        INSERT INTO jobs (
            id, organization_id, queue_name, status, payload, priority,
            max_retries, timeout_seconds, created_at, scheduled_at,
            idempotency_key, updated_at, tags, parent_job_id, 
            completion_webhook, expires_at
        )
        VALUES ($1, $2, $3, $4, $5::JSONB, $6, $7, $8, $9, $10, $11, $9, $12, $13, $14, $15)
        ON CONFLICT (organization_id, idempotency_key) 
        WHERE idempotency_key IS NOT NULL
        DO UPDATE SET updated_at = NOW() 
        RETURNING id
        "#,
    )
    .bind(&job_id)
    .bind(&org_id)
    .bind(&request.queue_name)
    .bind(initial_status)
    .bind(serde_json::to_string(&request.payload).unwrap_or_default())
    .bind(priority)
    .bind(max_retries)
    .bind(timeout_seconds)
    .bind(now)
    .bind(request.scheduled_at)
    .bind(&request.idempotency_key)
    .bind(
        request
            .tags
            .as_ref()
            .and_then(|t| serde_json::to_string(t).ok()),
    )
    .bind(&request.parent_job_id)
    .bind(&request.completion_webhook)
    .bind(request.expires_at)
    .fetch_one(state.db.pool())
    .await
    // Sanitize SQL errors
    .map_err(|e| {
        tracing::error!(error = %e, "Failed to create job");
        AppError::Database(sqlx::Error::Protocol("Failed to create job".to_string()))
    })?;

    // Check if this was a new job or existing (idempotent)
    let created = returned_id == job_id;

    // Update metrics and usage counters
    if created {
        state.metrics.jobs_enqueued.inc();
        state.metrics.jobs_pending.inc();

        // Increment daily job counter for plan limit tracking
        if let Err(e) = increment_daily_jobs(state.db.pool(), &org_id, 1).await {
            tracing::warn!(error = %e, org_id = %org_id, "Failed to increment daily job counter");
        }
    }

    // Publish to Redis with org context for real-time subscribers
    if let Some(ref cache) = state.cache {
        let _ = cache
            .publish(
                &format!("org:{}:queue:{}", org_id, request.queue_name),
                &returned_id,
            )
            .await;
    }

    Ok((
        if created {
            StatusCode::CREATED
        } else {
            StatusCode::OK
        },
        Json(CreateJobResponse {
            id: returned_id,
            created,
        }),
    ))
}

/// Get a job by ID
///
/// Now requires organization context to prevent cross-tenant access
pub async fn get(
    State(state): State<AppState>,
    Extension(ctx): Extension<ApiKeyContext>,
    Path(id): Path<String>,
) -> AppResult<Json<Job>> {
    let job = sqlx::query_as::<_, Job>("SELECT * FROM jobs WHERE id = $1 AND organization_id = $2")
        .bind(&id)
        .bind(&ctx.organization_id)
        .fetch_optional(state.db.pool())
        .await?
        // Don't expose job ID in error message
        .ok_or_else(|| AppError::NotFound("Job not found".to_string()))?;

    Ok(Json(job))
}

/// Cancel a job
///
/// Now requires organization context to prevent cross-tenant access
pub async fn cancel(
    State(state): State<AppState>,
    Extension(ctx): Extension<ApiKeyContext>,
    Path(id): Path<String>,
) -> AppResult<StatusCode> {
    let result = sqlx::query(
        r#"
        UPDATE jobs 
        SET status = 'cancelled', updated_at = NOW()
        WHERE id = $1 AND organization_id = $2 AND status IN ('pending', 'scheduled')
        "#,
    )
    .bind(&id)
    .bind(&ctx.organization_id)
    .execute(state.db.pool())
    .await?;

    if result.rows_affected() == 0 {
        // Check if job exists and get its status (only within org)
        let job_status: Option<(String,)> =
            sqlx::query_as("SELECT status FROM jobs WHERE id = $1 AND organization_id = $2")
                .bind(&id)
                .bind(&ctx.organization_id)
                .fetch_optional(state.db.pool())
                .await?;

        match job_status {
            // Don't expose job ID in error
            None => return Err(AppError::NotFound("Job not found".to_string())),
            Some(_) => {
                return Err(AppError::Conflict(
                    "Job cannot be cancelled (not in pending or scheduled state)".to_string(),
                ))
            }
        }
    }

    // Decrement jobs_pending only - the metric will be corrected by scheduler
    // The cancelled job was either pending or scheduled, both contribute to "pending" queue depth
    state.metrics.jobs_pending.dec();

    // Notify Redis about job cancellation
    if let Some(ref cache) = state.cache {
        let _ = cache
            .publish(
                &format!("org:{}:job:{}", ctx.organization_id, id),
                "cancelled",
            )
            .await;
    }

    Ok(StatusCode::NO_CONTENT)
}

/// Retry a failed job
///
pub async fn retry(
    State(state): State<AppState>,
    Extension(ctx): Extension<ApiKeyContext>,
    Path(id): Path<String>,
) -> AppResult<Json<Job>> {
    let job = sqlx::query_as::<_, Job>(
        r#"
        UPDATE jobs 
        SET 
            status = 'pending',
            retry_count = retry_count + 1,
            last_error = NULL,
            updated_at = NOW()
        WHERE id = $1 AND organization_id = $2 AND status IN ('failed', 'deadletter')
        RETURNING *
        "#,
    )
    .bind(&id)
    .bind(&ctx.organization_id)
    .fetch_optional(state.db.pool())
    .await?
    .ok_or_else(|| {
        AppError::Conflict(
            "Job cannot be retried (not found or not in failed/deadletter state)".to_string(),
        )
    })?;

    state.metrics.jobs_retried.inc();
    state.metrics.jobs_pending.inc();

    // Notify Redis about job being available for workers
    if let Some(ref cache) = state.cache {
        let _ = cache
            .publish(
                &format!("org:{}:queue:{}", ctx.organization_id, job.queue_name),
                &job.id,
            )
            .await;
    }

    Ok(Json(job))
}

/// Get job statistics
///
pub async fn stats(
    State(state): State<AppState>,
    Extension(ctx): Extension<ApiKeyContext>,
) -> AppResult<Json<JobStats>> {
    let stats = sqlx::query_as::<_, (i64, i64, i64, i64, i64, i64, i64)>(
        r#"
        SELECT 
            COUNT(*) FILTER (WHERE status = 'pending') as pending,
            COUNT(*) FILTER (WHERE status = 'scheduled') as scheduled,
            COUNT(*) FILTER (WHERE status = 'processing') as processing,
            COUNT(*) FILTER (WHERE status = 'completed') as completed,
            COUNT(*) FILTER (WHERE status = 'failed') as failed,
            COUNT(*) FILTER (WHERE status = 'deadletter') as deadletter,
            COUNT(*) FILTER (WHERE status = 'cancelled') as cancelled
        FROM jobs
        WHERE organization_id = $1
        "#,
    )
    .bind(&ctx.organization_id)
    .fetch_one(state.db.pool())
    .await?;

    Ok(Json(JobStats {
        pending: stats.0,
        scheduled: stats.1,
        processing: stats.2,
        completed: stats.3,
        failed: stats.4,
        deadletter: stats.5,
        cancelled: stats.6,
        total: stats.0 + stats.1 + stats.2 + stats.3 + stats.4 + stats.5 + stats.6,
    }))
}

/// Bulk enqueue multiple jobs
///
/// Now requires authenticated context - previously fell back to "default-org"
/// SECURITY: Enforces plan limits before creating jobs
pub async fn bulk_enqueue(
    State(state): State<AppState>,
    Extension(ctx): Extension<ApiKeyContext>,
    ValidatedJson(request): ValidatedJson<crate::models::BulkEnqueueRequest>,
) -> AppResult<Json<crate::models::BulkEnqueueResponse>> {
    let org_id = ctx.organization_id;
    let job_count = request.jobs.len() as u64;

    // Check job limits (daily + active) before creating
    if let Err(response) = check_job_limits(state.db.pool(), &org_id, job_count).await {
        return Err(AppError::LimitExceeded(Box::new(response)));
    }

    let now = Utc::now();
    let default_priority = request.default_priority.unwrap_or(0);
    let default_max_retries = request
        .default_max_retries
        .unwrap_or(state.settings.queue.default_max_retries);
    let default_timeout = request
        .default_timeout_seconds
        .unwrap_or(state.settings.queue.default_timeout_secs);

    let mut succeeded = Vec::new();
    let mut failed = Vec::new();

    // Track successful enqueues to correctly update metrics on partial failure
    let mut successful_count: u64 = 0;

    for (index, job_item) in request.jobs.into_iter().enumerate() {
        let job_id = Uuid::new_v4().to_string();
        let priority = job_item.priority.unwrap_or(default_priority);

        let initial_status = if job_item.scheduled_at.is_some() && job_item.scheduled_at > Some(now)
        {
            "scheduled"
        } else {
            "pending"
        };

        let result = sqlx::query_as::<_, (String,)>(
            r#"
            INSERT INTO jobs (
                id, organization_id, queue_name, status, payload, priority,
                max_retries, timeout_seconds, created_at, scheduled_at,
                idempotency_key, updated_at
            )
            VALUES ($1, $2, $3, $4, $5::JSONB, $6, $7, $8, $9, $10, $11, $9)
            ON CONFLICT (organization_id, idempotency_key) 
            WHERE idempotency_key IS NOT NULL
            DO UPDATE SET updated_at = NOW() 
            RETURNING id
            "#,
        )
        .bind(&job_id)
        .bind(&org_id)
        .bind(&request.queue_name)
        .bind(initial_status)
        .bind(serde_json::to_string(&job_item.payload).unwrap_or_default())
        .bind(priority)
        .bind(default_max_retries)
        .bind(default_timeout)
        .bind(now)
        .bind(job_item.scheduled_at)
        .bind(&job_item.idempotency_key)
        .fetch_one(state.db.pool())
        .await;

        match result {
            Ok((returned_id,)) => {
                let created = returned_id == job_id;
                if created {
                    // Track count locally, update metrics at end
                    successful_count += 1;
                }
                succeeded.push(crate::models::BulkJobResult {
                    index,
                    job_id: returned_id,
                    created,
                });
            }
            Err(e) => {
                // Sanitize error message to prevent SQL injection details leaking
                tracing::debug!(error = %e, index = index, "Bulk enqueue job failed");
                failed.push(crate::models::BulkJobError {
                    index,
                    error: "Failed to create job".to_string(), // Generic error
                });
            }
        }
    }

    let success_count = succeeded.len();
    let failure_count = failed.len();
    let total = success_count + failure_count;

    // Update metrics and usage counters only after all jobs processed
    // Previously metrics were updated per-job, which could leave inconsistent state on partial failure
    if successful_count > 0 {
        state.metrics.jobs_enqueued.inc_by(successful_count);

        // Increment daily job counter for plan limit tracking
        if let Err(e) =
            increment_daily_jobs(state.db.pool(), &org_id, successful_count as i32).await
        {
            tracing::warn!(
                error = %e,
                org_id = %org_id,
                count = successful_count,
                "Failed to increment daily job counter"
            );
        }
    }

    // Publish notification for new jobs with org context
    if success_count > 0 {
        if let Some(ref cache) = state.cache {
            let _ = cache
                .publish(
                    &format!("org:{}:queue:{}", org_id, request.queue_name),
                    "bulk_enqueue",
                )
                .await;
        }
    }

    Ok(Json(crate::models::BulkEnqueueResponse {
        succeeded,
        failed,
        total,
        success_count,
        failure_count,
    }))
}

/// Maximum priority value
const MAX_PRIORITY: i32 = 1000;

/// Minimum priority value
const MIN_PRIORITY: i32 = -1000;

/// Boost job priority
///
/// Now validates priority bounds
pub async fn boost_priority(
    State(state): State<AppState>,
    Extension(ctx): Extension<ApiKeyContext>,
    Path(id): Path<String>,
    ValidatedJson(request): ValidatedJson<crate::models::BoostPriorityRequest>,
) -> AppResult<Json<crate::models::BoostPriorityResponse>> {
    // Validate priority bounds
    let new_priority = request.priority.clamp(MIN_PRIORITY, MAX_PRIORITY);
    if new_priority != request.priority {
        tracing::warn!(
            requested = request.priority,
            clamped = new_priority,
            "Priority clamped to valid range"
        );
    }

    // Get current priority - with organization check
    let job: Option<(i32,)> = sqlx::query_as(
        "SELECT priority FROM jobs WHERE id = $1 AND organization_id = $2 AND status IN ('pending', 'scheduled')"
    )
    .bind(&id)
    .bind(&ctx.organization_id)
    .fetch_optional(state.db.pool())
    .await?;

    let Some((old_priority,)) = job else {
        return Err(AppError::NotFound(format!(
            "Job {} not found or not in pending/scheduled state",
            id
        )));
    };

    // Update priority - with organization check
    // Use clamped priority value
    sqlx::query(
        "UPDATE jobs SET priority = $1, updated_at = NOW() WHERE id = $2 AND organization_id = $3",
    )
    .bind(new_priority)
    .bind(&id)
    .bind(&ctx.organization_id)
    .execute(state.db.pool())
    .await?;

    Ok(Json(crate::models::BoostPriorityResponse {
        job_id: id,
        old_priority,
        new_priority, // Return clamped value
    }))
}

/// List jobs in dead-letter queue
///
/// Reduced max limit from 1000 to 100
pub async fn list_dlq(
    State(state): State<AppState>,
    Extension(ctx): Extension<ApiKeyContext>,
    Query(query): Query<ListJobsQuery>,
) -> AppResult<Json<Vec<JobSummary>>> {
    // Cap limit at 100 to prevent memory exhaustion
    let limit = query.limit.unwrap_or(50).clamp(1, MAX_JOBS_PER_PAGE);
    let offset = query.offset.unwrap_or(0).max(0);

    let jobs = match &query.queue_name {
        Some(queue) => {
            sqlx::query_as::<_, Job>(
                "SELECT * FROM jobs WHERE status = 'deadletter' AND organization_id = $1 AND queue_name = $2 ORDER BY created_at DESC LIMIT $3 OFFSET $4",
            )
            .bind(&ctx.organization_id)
            .bind(queue)
            .bind(limit)
            .bind(offset)
            .fetch_all(state.db.pool())
            .await?
        }
        None => {
            sqlx::query_as::<_, Job>(
                "SELECT * FROM jobs WHERE status = 'deadletter' AND organization_id = $1 ORDER BY created_at DESC LIMIT $2 OFFSET $3",
            )
            .bind(&ctx.organization_id)
            .bind(limit)
            .bind(offset)
            .fetch_all(state.db.pool())
            .await?
        }
    };

    let summaries: Vec<JobSummary> = jobs.into_iter().map(Into::into).collect();
    Ok(Json(summaries))
}

/// Retry jobs from dead-letter queue
///
/// Now uses safe_limit method
pub async fn retry_dlq(
    State(state): State<AppState>,
    Extension(ctx): Extension<ApiKeyContext>,
    ValidatedJson(request): ValidatedJson<crate::models::RetryDlqRequest>,
) -> AppResult<Json<crate::models::RetryDlqResponse>> {
    // Use safe_limit method to enforce bounds
    let limit = request.safe_limit();

    // Build query based on filters - ALL queries now include organization_id
    let retried_jobs: Vec<(String,)> = if let Some(ref job_ids) = request.job_ids {
        // Retry specific jobs - with org check
        sqlx::query_as(
            r#"
            UPDATE jobs
            SET 
                status = 'pending',
                retry_count = 0,
                last_error = NULL,
                updated_at = NOW()
            WHERE id = ANY($1) AND organization_id = $2 AND status = 'deadletter'
            RETURNING id
            "#,
        )
        .bind(job_ids)
        .bind(&ctx.organization_id)
        .fetch_all(state.db.pool())
        .await?
    } else if let Some(ref queue_name) = request.queue_name {
        // Retry jobs from specific queue - with org check
        sqlx::query_as(
            r#"
            UPDATE jobs
            SET 
                status = 'pending',
                retry_count = 0,
                last_error = NULL,
                updated_at = NOW()
            WHERE id IN (
                SELECT id FROM jobs 
                WHERE status = 'deadletter' AND organization_id = $1 AND queue_name = $2
                ORDER BY created_at ASC
                LIMIT $3
            )
            RETURNING id
            "#,
        )
        .bind(&ctx.organization_id)
        .bind(queue_name)
        .bind(limit)
        .fetch_all(state.db.pool())
        .await?
    } else {
        // Retry all DLQ jobs up to limit - with org check
        sqlx::query_as(
            r#"
            UPDATE jobs
            SET 
                status = 'pending',
                retry_count = 0,
                last_error = NULL,
                updated_at = NOW()
            WHERE id IN (
                SELECT id FROM jobs 
                WHERE status = 'deadletter' AND organization_id = $1
                ORDER BY created_at ASC
                LIMIT $2
            )
            RETURNING id
            "#,
        )
        .bind(&ctx.organization_id)
        .bind(limit)
        .fetch_all(state.db.pool())
        .await?
    };

    let retried_job_ids: Vec<String> = retried_jobs.into_iter().map(|(id,)| id).collect();
    let retried_count = retried_job_ids.len() as i64;

    // Reset child jobs that were blocked due to parent failure
    // When a parent job is retried from DLQ, its child jobs (with dependencies_met = false)
    // should have their dependencies recalculated
    if !retried_job_ids.is_empty() {
        let reset_children = sqlx::query(
            r#"
            UPDATE jobs
            SET dependencies_met = FALSE, updated_at = NOW()
            WHERE parent_job_id = ANY($1)
              AND status IN ('pending', 'scheduled')
            "#,
        )
        .bind(&retried_job_ids)
        .execute(state.db.pool())
        .await;

        if let Ok(result) = reset_children {
            if result.rows_affected() > 0 {
                info!(
                    parent_jobs = ?retried_job_ids,
                    reset_children = result.rows_affected(),
                    "Reset child jobs for retried DLQ jobs"
                );
            }
        }
    }

    state.metrics.jobs_retried.inc_by(retried_count as u64);

    Ok(Json(crate::models::RetryDlqResponse {
        retried_count,
        retried_jobs: retried_job_ids,
    }))
}

/// Maximum jobs to purge in a single request
const MAX_PURGE_LIMIT: i64 = 10000;

/// Purge jobs from dead-letter queue
///
pub async fn purge_dlq(
    State(state): State<AppState>,
    Extension(ctx): Extension<ApiKeyContext>,
    ValidatedJson(request): ValidatedJson<crate::models::PurgeDlqRequest>,
) -> AppResult<Json<crate::models::PurgeDlqResponse>> {
    if !request.confirm {
        return Err(AppError::BadRequest(
            "Must set 'confirm: true' to purge dead-letter queue".to_string(),
        ));
    }

    // Get limit with max bound
    let limit = request
        .limit
        .unwrap_or(MAX_PURGE_LIMIT)
        .clamp(1, MAX_PURGE_LIMIT);

    // ALL queries now include organization_id for tenant isolation
    let result = match (&request.queue_name, &request.older_than) {
        (Some(queue), Some(older_than)) => {
            sqlx::query(
                r#"DELETE FROM jobs WHERE id IN (
                    SELECT id FROM jobs 
                    WHERE status = 'deadletter' AND organization_id = $1 AND queue_name = $2 AND created_at < $3
                    LIMIT $4
                )"#
            )
            .bind(&ctx.organization_id)
            .bind(queue)
            .bind(older_than)
            .bind(limit)
            .execute(state.db.pool())
            .await?
        }
        (Some(queue), None) => {
            sqlx::query(
                r#"DELETE FROM jobs WHERE id IN (
                    SELECT id FROM jobs 
                    WHERE status = 'deadletter' AND organization_id = $1 AND queue_name = $2
                    LIMIT $3
                )"#
            )
                .bind(&ctx.organization_id)
                .bind(queue)
                .bind(limit)
                .execute(state.db.pool())
                .await?
        }
        (None, Some(older_than)) => {
            sqlx::query(
                r#"DELETE FROM jobs WHERE id IN (
                    SELECT id FROM jobs 
                    WHERE status = 'deadletter' AND organization_id = $1 AND created_at < $2
                    LIMIT $3
                )"#
            )
                .bind(&ctx.organization_id)
                .bind(older_than)
                .bind(limit)
                .execute(state.db.pool())
                .await?
        }
        (None, None) => {
            sqlx::query(
                r#"DELETE FROM jobs WHERE id IN (
                    SELECT id FROM jobs 
                    WHERE status = 'deadletter' AND organization_id = $1
                    LIMIT $2
                )"#
            )
                .bind(&ctx.organization_id)
                .bind(limit)
                .execute(state.db.pool())
                .await?
        }
    };

    Ok(Json(crate::models::PurgeDlqResponse {
        purged_count: result.rows_affected() as i64,
    }))
}

/// Request for batch status lookup
#[derive(Debug, serde::Deserialize)]
pub struct BatchStatusQuery {
    /// Comma-separated job IDs
    pub ids: String,
}

/// Batch job status response
#[derive(Debug, serde::Serialize)]
pub struct BatchJobStatus {
    pub id: String,
    pub status: String,
    pub queue_name: String,
    pub retry_count: i32,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub completed_at: Option<chrono::DateTime<chrono::Utc>>,
}

/// Get status of multiple jobs at once
///
/// Now requires organization context to prevent cross-tenant access.
pub async fn batch_status(
    State(state): State<AppState>,
    Extension(ctx): Extension<ApiKeyContext>,
    Query(query): Query<BatchStatusQuery>,
) -> AppResult<Json<Vec<BatchJobStatus>>> {
    // Parse comma-separated IDs
    let ids: Vec<&str> = query
        .ids
        .split(',')
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .collect();

    if ids.is_empty() {
        return Ok(Json(vec![]));
    }

    if ids.len() > 100 {
        return Err(AppError::BadRequest(
            "Maximum 100 job IDs allowed per request".to_string(),
        ));
    }

    // Build query with ANY array - filtered by organization
    let jobs: Vec<BatchJobStatus> = sqlx::query_as(
        r#"
        SELECT id, status, queue_name, retry_count, created_at, completed_at
        FROM jobs
        WHERE id = ANY($1) AND organization_id = $2
        "#,
    )
    .bind(&ids)
    .bind(&ctx.organization_id)
    .fetch_all(state.db.pool())
    .await?;

    Ok(Json(jobs))
}

// Manual FromRow implementation for BatchJobStatus
impl<'r> sqlx::FromRow<'r, sqlx::postgres::PgRow> for BatchJobStatus {
    fn from_row(row: &'r sqlx::postgres::PgRow) -> Result<Self, sqlx::Error> {
        use sqlx::Row;
        Ok(BatchJobStatus {
            id: row.try_get("id")?,
            status: row.try_get("status")?,
            queue_name: row.try_get("queue_name")?,
            retry_count: row.try_get("retry_count")?,
            created_at: row.try_get("created_at")?,
            completed_at: row.try_get("completed_at")?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_job_stats_total() {
        let stats = JobStats {
            pending: 10,
            scheduled: 5,
            processing: 3,
            completed: 100,
            failed: 2,
            deadletter: 1,
            cancelled: 0,
            total: 121,
        };

        assert_eq!(
            stats.pending
                + stats.scheduled
                + stats.processing
                + stats.completed
                + stats.failed
                + stats.deadletter
                + stats.cancelled,
            stats.total
        );
    }

    #[test]
    fn test_job_stats_edge_cases() {
        // All zeros
        let empty_stats = JobStats {
            pending: 0,
            scheduled: 0,
            processing: 0,
            completed: 0,
            failed: 0,
            deadletter: 0,
            cancelled: 0,
            total: 0,
        };
        assert_eq!(empty_stats.total, 0);

        // Large numbers
        let large_stats = JobStats {
            pending: 1_000_000,
            scheduled: 500_000,
            processing: 100_000,
            completed: 10_000_000,
            failed: 50_000,
            deadletter: 10_000,
            cancelled: 5_000,
            total: 11_665_000,
        };
        assert_eq!(
            large_stats.pending
                + large_stats.scheduled
                + large_stats.processing
                + large_stats.completed
                + large_stats.failed
                + large_stats.deadletter
                + large_stats.cancelled,
            large_stats.total
        );
    }

    #[test]
    fn test_list_jobs_query_defaults() {
        let query = ListJobsQuery {
            queue_name: None,
            status: None,
            limit: None,
            offset: None,
            order_by: None,
            order_dir: None,
        };

        // Default limit should be 50, capped at 1000
        let limit = query.limit.unwrap_or(50).min(1000);
        assert_eq!(limit, 50);

        // Default offset should be 0
        let offset = query.offset.unwrap_or(0);
        assert_eq!(offset, 0);
    }

    #[test]
    fn test_list_jobs_query_limit_capping() {
        let query_large = ListJobsQuery {
            queue_name: None,
            status: None,
            limit: Some(5000), // Over max
            offset: None,
            order_by: None,
            order_dir: None,
        };

        let limit = query_large.limit.unwrap_or(50).min(1000);
        assert_eq!(limit, 1000); // Should be capped

        let query_small = ListJobsQuery {
            queue_name: None,
            status: None,
            limit: Some(10),
            offset: None,
            order_by: None,
            order_dir: None,
        };

        let limit = query_small.limit.unwrap_or(50).min(1000);
        assert_eq!(limit, 10);
    }

    #[test]
    fn test_initial_status_determination() {
        use chrono::Utc;

        let now = Utc::now();

        // No scheduled_at -> pending
        let scheduled_at: Option<chrono::DateTime<Utc>> = None;
        let status = if scheduled_at.is_some() && scheduled_at > Some(now) {
            "scheduled"
        } else {
            "pending"
        };
        assert_eq!(status, "pending");

        // scheduled_at in the past -> pending
        let scheduled_at = Some(now - chrono::Duration::hours(1));
        let status = if scheduled_at.is_some() && scheduled_at > Some(now) {
            "scheduled"
        } else {
            "pending"
        };
        assert_eq!(status, "pending");

        // scheduled_at in the future -> scheduled
        let scheduled_at = Some(now + chrono::Duration::hours(1));
        let status = if scheduled_at.is_some() && scheduled_at > Some(now) {
            "scheduled"
        } else {
            "pending"
        };
        assert_eq!(status, "scheduled");
    }
}
