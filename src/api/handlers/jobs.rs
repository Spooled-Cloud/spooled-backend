//! Job handlers

use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::IntoResponse,
    Json,
};
use chrono::Utc;
use sqlx::{Postgres, QueryBuilder};
use std::sync::Arc;
use tracing::{info, warn};
use uuid::Uuid;

use crate::api::handlers::require_queue_access;
use crate::api::middleware::limits::{
    check_job_limits, check_payload_size, daily_jobs_limit, increment_daily_jobs,
    refund_daily_jobs, try_increment_daily_jobs,
};
use crate::api::middleware::ValidatedJson;
use crate::api::AppState;
use crate::error::{AppError, AppResult};
use crate::models::ApiKeyContext;
use crate::models::{
    ClaimJobsRequest, ClaimJobsResponse, ClaimedJob, CompleteJobRequest, CreateJobRequest,
    CreateJobResponse, FailJobRequest, HeartbeatJobRequest, Job, JobStats, JobStatsQuery,
    JobSummary, ListJobsQuery,
};
use crate::queue::{QueueManager, WorkerOpOutcome};
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

/// Validate a single tag filter value (simple, predictable syntax).
fn validate_tag_filter(tag: &str) -> bool {
    !tag.is_empty()
        && tag.len() <= 64
        && tag
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
}

/// Publish a realtime event to the organization-scoped events channel (best-effort).
///
/// The WebSocket handler subscribes to `org:{org_id}:events` and expects JSON in the
/// `RealtimeEvent` enum shape: `{ "type": "...", "data": { ... } }`.
async fn publish_realtime_event(
    cache: &crate::cache::RedisCache,
    org_id: &str,
    event: serde_json::Value,
) {
    let channel = format!("org:{}:events", org_id);
    if let Ok(payload) = serde_json::to_string(&event) {
        let _ = cache.publish(&channel, &payload).await;
    }
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

    // Validate tag filter (optional)
    let validated_tag = query.tag.as_ref().and_then(|t| {
        if validate_tag_filter(t) {
            Some(t.clone())
        } else {
            tracing::warn!(tag = %t, "Invalid tag filter ignored");
            None
        }
    });
    let org_id = &ctx.organization_id;

    // Build query dynamically to support optional filters without a combinatorial explosion.
    // All filters are parameterized (no string interpolation of user input).
    let mut qb: QueryBuilder<Postgres> =
        QueryBuilder::new("SELECT * FROM jobs WHERE organization_id = ");
    qb.push_bind(org_id);

    // Queue scope: a restricted key only ever lists jobs in its own queues. This must
    // RESTRICT the result set (not merely validate an optional filter), otherwise a
    // scoped key with no `queue_name` filter would see every queue's jobs.
    if let Some(allowed) = ctx.queue_scope_filter() {
        qb.push(" AND queue_name = ANY(");
        qb.push_bind(allowed.to_vec());
        qb.push(")");
    }

    if let Some(queue) = &validated_queue {
        qb.push(" AND queue_name = ");
        qb.push_bind(queue);
    }

    if let Some(status) = &validated_status {
        qb.push(" AND status = ");
        qb.push_bind(status);
    }

    if let Some(tag) = &validated_tag {
        qb.push(" AND tags ? ");
        qb.push_bind(tag);
    }

    qb.push(" ORDER BY created_at DESC LIMIT ");
    qb.push_bind(limit);
    qb.push(" OFFSET ");
    qb.push_bind(offset);

    let jobs: Vec<Job> = qb.build_query_as().fetch_all(state.db.pool()).await?;

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
    // Enforce API key queue scope (empty/`*` = all queues allowed)
    if !ctx.can_access_queue(&request.queue_name) {
        return Err(AppError::Authorization(format!(
            "API key does not have permission for queue '{}'",
            request.queue_name
        )));
    }

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

    // If a parent job is declared, the child must not run until the parent completes.
    // Validate the parent belongs to THIS org, then gate the child
    // (dependencies_met = FALSE) unless the parent has already completed. A
    // job_dependencies edge is recorded after insert so the existing completion
    // trigger releases the child (and the scheduler cancel-sweep terminates it if the
    // parent fails). Previously parent_job_id was stored but the child ran immediately.
    let parent_already_completed = if let Some(ref parent_id) = request.parent_job_id {
        let parent: Option<(String, String)> = sqlx::query_as(
            "SELECT status, queue_name FROM jobs WHERE id = $1 AND organization_id = $2",
        )
        .bind(parent_id)
        .bind(&org_id)
        .fetch_optional(state.db.pool())
        .await?;
        match parent {
            Some((status, parent_queue)) => {
                if parent_queue != request.queue_name {
                    return Err(AppError::Validation(
                        "parent_job_id must reference a job in the same queue".to_string(),
                    ));
                }
                status == "completed"
            }
            None => {
                return Err(AppError::Validation(
                    "parent_job_id does not reference a job in this organization".to_string(),
                ));
            }
        }
    } else {
        false
    };
    // Gated only when a parent exists and has not completed yet.
    let dependencies_met = request.parent_job_id.is_none() || parent_already_completed;

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
            completion_webhook, completion_webhook_secret, expires_at, dependencies_met
        )
        VALUES ($1, $2, $3, $4, $5::JSONB, $6, $7, $8, $9, $10, $11, $9, $12::JSONB, $13, $14, $15, $16, $17)
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
    .bind(&request.completion_webhook_secret)
    .bind(request.expires_at)
    .bind(dependencies_met)
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
        // Atomically enforce + increment the daily quota. The pre-insert
        // check_job_limits above rejects the common over-limit case; this closes
        // the concurrency window where N simultaneous enqueues each passed that
        // check and then all incremented, overshooting the cap.
        let limit = daily_jobs_limit(state.db.pool(), &org_id)
            .await
            .unwrap_or(-1);
        match try_increment_daily_jobs(state.db.pool(), &org_id, 1, limit).await {
            Ok(count) if count >= 0 => {
                state.metrics.jobs_enqueued.inc();
                state.metrics.jobs_pending.inc();
            }
            Ok(_) => {
                // Over the daily cap. The job we just created is brand new with no
                // dependents yet, so remove it and reject with the standard limit
                // error rather than leaving an uncounted job that exceeds quota.
                let _ = sqlx::query("DELETE FROM jobs WHERE id = $1 AND organization_id = $2")
                    .bind(&job_id)
                    .bind(&org_id)
                    .execute(state.db.pool())
                    .await;
                let response = check_job_limits(state.db.pool(), &org_id, 1)
                    .await
                    .err()
                    .unwrap_or_else(|| {
                        (StatusCode::TOO_MANY_REQUESTS, "Daily job limit reached").into_response()
                    });
                return Err(AppError::LimitExceeded(Box::new(response)));
            }
            Err(e) => {
                // Counter DB error: keep the job (best-effort, as before) rather
                // than failing an accepted enqueue.
                tracing::warn!(error = %e, org_id = %org_id, "Failed to increment daily job counter");
                state.metrics.jobs_enqueued.inc();
                state.metrics.jobs_pending.inc();
            }
        }
    }

    // Record the parent dependency edge so the parent's completion trigger releases
    // this gated child. Recompute dependencies_met afterward to close the race where
    // the parent completed between the status check above and this insert (its trigger
    // would have fired before the edge existed).
    if created && !dependencies_met {
        if let Some(ref parent_id) = request.parent_job_id {
            match sqlx::query(
                "INSERT INTO job_dependencies (id, job_id, depends_on_job_id, dependency_type) \
                 VALUES ($1, $2, $3, 'all') ON CONFLICT (job_id, depends_on_job_id) DO NOTHING",
            )
            .bind(Uuid::new_v4().to_string())
            .bind(&returned_id)
            .bind(parent_id)
            .execute(state.db.pool())
            .await
            {
                Ok(_) => {
                    let _ = sqlx::query(
                        "UPDATE jobs SET dependencies_met = check_job_dependencies_met(id) \
                         WHERE id = $1 AND organization_id = $2",
                    )
                    .bind(&returned_id)
                    .bind(&org_id)
                    .execute(state.db.pool())
                    .await;
                }
                Err(e) => {
                    tracing::error!(error = %e, job_id = %returned_id, parent_job_id = %parent_id,
                        "Failed to record parent job dependency edge; child remains gated");
                }
            }
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

        // Also publish a structured realtime event for WebSocket consumers.
        // Only publish for newly created jobs, not idempotent duplicates.
        if created {
            publish_realtime_event(
                cache,
                &org_id,
                serde_json::json!({
                    "type": "JobCreated",
                    "data": {
                        "job_id": returned_id,
                        "queue_name": request.queue_name,
                        "priority": priority,
                        "timestamp": Utc::now().to_rfc3339()
                    }
                }),
            )
            .await;
        }
    }

    if created {
        state.outgoing_webhooks.spawn_dispatch(
            org_id.clone(),
            "job.created".to_string(),
            returned_id.clone(),
            serde_json::json!({
                "job_id": returned_id,
                "queue_name": request.queue_name,
                "priority": priority,
                "status": initial_status,
            }),
        );
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

/// Claim jobs for processing (worker polling)
///
/// POST /api/v1/jobs/claim
///
/// This endpoint is designed for worker processes. It atomically leases jobs
/// using PostgreSQL FOR UPDATE SKIP LOCKED. Uses batch claiming for efficiency.
pub async fn claim(
    State(state): State<AppState>,
    Extension(ctx): Extension<ApiKeyContext>,
    ValidatedJson(request): ValidatedJson<ClaimJobsRequest>,
) -> AppResult<Json<ClaimJobsResponse>> {
    // Enforce API key queue scope (empty/`*` = all queues allowed)
    if !ctx.can_access_queue(&request.queue_name) {
        return Err(AppError::Authorization(format!(
            "API key does not have permission for queue '{}'",
            request.queue_name
        )));
    }

    // Defaults and safety bounds
    let limit = request
        .limit
        .unwrap_or(1)
        .clamp(1, MAX_JOBS_PER_PAGE as i32);
    let lease_duration_secs = request.lease_duration_secs.unwrap_or(30);

    let queue = QueueManager::new(
        state.db.pool_arc(),
        state.cache.as_ref().map(|c| Arc::new(c.clone())),
    );

    // Use batch dequeue for efficiency (single DB round-trip)
    let claimed_jobs = queue
        .dequeue_batch(
            &ctx.organization_id,
            &request.queue_name,
            &request.worker_id,
            lease_duration_secs,
            limit,
        )
        .await?;

    // Update metrics for all claimed jobs
    let claimed_count = claimed_jobs.len() as i64;
    if claimed_count > 0 {
        state.metrics.jobs_processing.add(claimed_count);
        state.metrics.jobs_pending.sub(claimed_count);
    }

    for job in &claimed_jobs {
        state.outgoing_webhooks.spawn_dispatch(
            ctx.organization_id.clone(),
            "job.started".to_string(),
            job.id.clone(),
            serde_json::json!({
                "job_id": job.id,
                "queue_name": job.queue_name,
                "worker_id": request.worker_id,
            }),
        );
    }

    let jobs: Vec<ClaimedJob> = claimed_jobs.into_iter().map(Into::into).collect();

    Ok(Json(ClaimJobsResponse { jobs }))
}

/// Complete (ack) a job as the worker that leased it
///
/// POST /api/v1/jobs/{id}/complete
pub async fn complete(
    State(state): State<AppState>,
    Extension(ctx): Extension<ApiKeyContext>,
    Path(id): Path<String>,
    ValidatedJson(request): ValidatedJson<CompleteJobRequest>,
) -> AppResult<(StatusCode, Json<serde_json::Value>)> {
    let queue = QueueManager::new(
        state.db.pool_arc(),
        state.cache.as_ref().map(|c| Arc::new(c.clone())),
    );

    let outcome = queue
        .complete_by_worker(
            &id,
            &request.worker_id,
            &ctx.organization_id,
            request.lease_id.as_deref(),
            ctx.queue_scope_filter(),
            request.result,
        )
        .await
        .map_err(|e| AppError::Internal(e.to_string()))?;

    match outcome {
        WorkerOpOutcome::Ok => {}
        WorkerOpOutcome::LeaseExpired => {
            return Err(AppError::LeaseExpired(
                "Job lease expired before completion".to_string(),
            ));
        }
        WorkerOpOutcome::NotOwned => {
            return Err(AppError::NotFound(
                "Job not found or not owned by worker".to_string(),
            ));
        }
    }

    // Metrics: processing -> completed
    state.metrics.jobs_processing.dec();
    state.metrics.jobs_completed.inc();

    // Fetch job fields once for realtime + org webhooks + per-job completion_webhook.
    type CompletionJobRow = (
        String,
        Option<serde_json::Value>,
        Option<String>,
        Option<String>,
    );
    let job_data: Option<CompletionJobRow> = sqlx::query_as(
        "SELECT queue_name, result, completion_webhook, completion_webhook_secret FROM jobs WHERE id = $1 AND organization_id = $2",
    )
    .bind(&id)
    .bind(&ctx.organization_id)
    .fetch_optional(state.db.pool())
    .await
    .unwrap_or(None);

    let (queue_name, result, completion_webhook, completion_webhook_secret) =
        job_data.unwrap_or_else(|| ("unknown".to_string(), None, None, None));

    // Best-effort realtime publish
    if let Some(ref cache) = state.cache {
        publish_realtime_event(
            cache,
            &ctx.organization_id,
            serde_json::json!({
                "type": "JobCompleted",
                "data": {
                    "job_id": id,
                    "queue_name": queue_name,
                    "duration_ms": 0,
                    "result": result,
                    "timestamp": Utc::now().to_rfc3339()
                }
            }),
        )
        .await;
    }

    state.outgoing_webhooks.spawn_dispatch(
        ctx.organization_id.clone(),
        "job.completed".to_string(),
        id.clone(),
        serde_json::json!({
            "job_id": id,
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
                "job_id": id,
                "queue_name": queue_name,
                "status": "completed",
                "result": result,
            }),
            completion_webhook_secret.filter(|s| !s.is_empty()),
        );
    }

    Ok((StatusCode::OK, Json(serde_json::json!({ "success": true }))))
}

/// Fail a job as the worker that leased it (triggers retry or DLQ)
///
/// POST /api/v1/jobs/{id}/fail
pub async fn fail(
    State(state): State<AppState>,
    Extension(ctx): Extension<ApiKeyContext>,
    Path(id): Path<String>,
    ValidatedJson(request): ValidatedJson<FailJobRequest>,
) -> AppResult<(StatusCode, Json<serde_json::Value>)> {
    // `fail_by_worker` folds the previous SELECT-then-UPDATE pre-check into
    // a single atomic UPDATE guarded by the lease predicate, so a lease that
    // expires between check and write can no longer sneak past as success.
    let queue = QueueManager::new(
        state.db.pool_arc(),
        state.cache.as_ref().map(|c| Arc::new(c.clone())),
    );

    let outcome = queue
        .fail_by_worker(
            &id,
            &request.worker_id,
            &ctx.organization_id,
            request.lease_id.as_deref(),
            ctx.queue_scope_filter(),
            &request.error,
        )
        .await
        .map_err(|e| AppError::Internal(e.to_string()))?;

    match outcome.outcome {
        WorkerOpOutcome::Ok => {}
        WorkerOpOutcome::LeaseExpired => {
            return Err(AppError::LeaseExpired(
                "Job lease expired before failure was reported".to_string(),
            ));
        }
        WorkerOpOutcome::NotOwned => {
            return Err(AppError::NotFound(
                "Job not found or not owned by worker".to_string(),
            ));
        }
    }

    // Metrics: processing -> pending (retry) OR processing -> deadletter.
    state.metrics.jobs_processing.dec();

    // Best-effort realtime publish (status after fail may be pending (retry), failed, or deadletter)
    type FailJobRow = (String, String, Option<String>, Option<String>);
    let updated: Option<FailJobRow> = sqlx::query_as(
        "SELECT status, queue_name, completion_webhook, completion_webhook_secret FROM jobs WHERE id = $1 AND organization_id = $2",
    )
    .bind(&id)
    .bind(&ctx.organization_id)
    .fetch_optional(state.db.pool())
    .await
    .unwrap_or(None);

    if let Some((new_status, queue_name, completion_webhook, completion_webhook_secret)) = updated {
        if let Some(ref cache) = state.cache {
            publish_realtime_event(
                cache,
                &ctx.organization_id,
                serde_json::json!({
                    "type": "JobStatusChange",
                    "data": {
                        "job_id": id,
                        "queue_name": queue_name,
                        "old_status": "processing",
                        "new_status": new_status,
                        "timestamp": Utc::now().to_rfc3339()
                    }
                }),
            )
            .await;
        }

        // Fire job.failed for terminal failure / DLQ; retries stay pending.
        if new_status == "failed" || new_status == "deadletter" {
            state.outgoing_webhooks.spawn_dispatch(
                ctx.organization_id.clone(),
                "job.failed".to_string(),
                id.clone(),
                serde_json::json!({
                    "job_id": id,
                    "queue_name": queue_name,
                    "status": new_status,
                    "error": request.error,
                }),
            );

            if let Some(url) = completion_webhook.filter(|u| !u.is_empty()) {
                crate::outgoing_webhooks::service::spawn_completion_webhook(
                    url,
                    "job.failed".to_string(),
                    serde_json::json!({
                        "job_id": id,
                        "queue_name": queue_name,
                        "status": new_status,
                        "error": request.error,
                    }),
                    completion_webhook_secret.filter(|s| !s.is_empty()),
                );
            }
        }
    }

    Ok((StatusCode::OK, Json(serde_json::json!({ "success": true }))))
}

/// Extend a lease on an in-flight job (heartbeat)
///
/// POST /api/v1/jobs/{id}/heartbeat
pub async fn heartbeat(
    State(state): State<AppState>,
    Extension(ctx): Extension<ApiKeyContext>,
    Path(id): Path<String>,
    ValidatedJson(request): ValidatedJson<HeartbeatJobRequest>,
) -> AppResult<(StatusCode, Json<serde_json::Value>)> {
    let queue = QueueManager::new(
        state.db.pool_arc(),
        state.cache.as_ref().map(|c| Arc::new(c.clone())),
    );

    let outcome = queue
        .renew_lease(
            &id,
            &request.worker_id,
            &ctx.organization_id,
            request.lease_id.as_deref(),
            ctx.queue_scope_filter(),
            request.lease_duration_secs,
        )
        .await
        .map_err(|e| AppError::Internal(e.to_string()))?;

    match outcome {
        WorkerOpOutcome::Ok => Ok((StatusCode::OK, Json(serde_json::json!({ "success": true })))),
        WorkerOpOutcome::LeaseExpired => Err(AppError::LeaseExpired(
            "Job lease expired and cannot be extended".to_string(),
        )),
        WorkerOpOutcome::NotOwned => Err(AppError::NotFound(
            "Job not found or not owned by worker".to_string(),
        )),
    }
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

    // Queue-scope: a key restricted to certain queues must not read jobs in other
    // queues. Return the same not-found so out-of-scope job existence isn't revealed.
    if !ctx.can_access_queue(&job.queue_name) {
        return Err(AppError::NotFound("Job not found".to_string()));
    }

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
    // Capture current status/queue for accurate realtime event, AND authoritatively
    // enforce queue scope: a restricted key cannot cancel jobs in queues outside its
    // scope (returns not-found so existence isn't revealed).
    let before: Option<(String, String)> = sqlx::query_as(
        "SELECT status, queue_name FROM jobs WHERE id = $1 AND organization_id = $2",
    )
    .bind(&id)
    .bind(&ctx.organization_id)
    .fetch_optional(state.db.pool())
    .await?;
    if let Some((_, ref queue_name)) = before {
        if !ctx.can_access_queue(queue_name) {
            return Err(AppError::NotFound("Job not found".to_string()));
        }
    }

    let result = sqlx::query(
        r#"
        UPDATE jobs
        SET status = 'cancelled', completed_at = NOW(), updated_at = NOW()
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

        let (old_status, queue_name) =
            before.unwrap_or_else(|| ("pending".to_string(), "unknown".to_string()));
        publish_realtime_event(
            cache,
            &ctx.organization_id,
            serde_json::json!({
                "type": "JobStatusChange",
                "data": {
                    "job_id": id,
                    "queue_name": queue_name,
                    "old_status": old_status,
                    "new_status": "cancelled",
                    "timestamp": Utc::now().to_rfc3339()
                }
            }),
        )
        .await;

        state.outgoing_webhooks.spawn_dispatch(
            ctx.organization_id.clone(),
            "job.cancelled".to_string(),
            id.clone(),
            serde_json::json!({
                "job_id": id,
                "queue_name": queue_name,
                "status": "cancelled",
                "old_status": old_status,
            }),
        );
    } else {
        let (_, queue_name) =
            before.unwrap_or_else(|| ("pending".to_string(), "unknown".to_string()));
        state.outgoing_webhooks.spawn_dispatch(
            ctx.organization_id.clone(),
            "job.cancelled".to_string(),
            id.clone(),
            serde_json::json!({
                "job_id": id,
                "queue_name": queue_name,
                "status": "cancelled",
            }),
        );
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
    // Capture previous status for realtime event (failed vs deadletter) AND enforce
    // queue scope: a restricted key cannot retry jobs in queues outside its scope.
    let before: Option<(String, String)> = sqlx::query_as(
        "SELECT status, queue_name FROM jobs WHERE id = $1 AND organization_id = $2",
    )
    .bind(&id)
    .bind(&ctx.organization_id)
    .fetch_optional(state.db.pool())
    .await?;
    if let Some((_, ref queue_name)) = before {
        if !ctx.can_access_queue(queue_name) {
            return Err(AppError::NotFound("Job not found".to_string()));
        }
    }
    let before_status: Option<(String,)> = before.as_ref().map(|(s, _)| (s.clone(),));

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
        if before.is_none() {
            AppError::NotFound("Job not found".to_string())
        } else {
            AppError::Conflict("Job cannot be retried (not in failed/deadletter state)".to_string())
        }
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

        // Best-effort realtime publish
        publish_realtime_event(
            cache,
            &ctx.organization_id,
            serde_json::json!({
                "type": "JobStatusChange",
                "data": {
                    "job_id": job.id,
                    "queue_name": job.queue_name,
                    "old_status": before_status.map(|(s,)| s).unwrap_or_else(|| "failed".to_string()),
                    "new_status": "pending",
                    "timestamp": Utc::now().to_rfc3339()
                }
            }),
        )
        .await;
    }

    Ok(Json(job))
}

/// Get job statistics
///
/// Optional `queue_name` (alias `queue`) scopes counts to one queue. Without it,
/// returns org-wide totals (still restricted by API-key queue scope).
pub async fn stats(
    State(state): State<AppState>,
    Extension(ctx): Extension<ApiKeyContext>,
    Query(query): Query<JobStatsQuery>,
) -> AppResult<Json<JobStats>> {
    let validated_queue = query.queue_name.as_ref().and_then(|q| {
        if validate_queue_name_filter(q) {
            Some(q.as_str())
        } else {
            tracing::warn!(queue_name = %q, "Invalid queue_name stats filter ignored");
            None
        }
    });

    if let Some(queue) = validated_queue {
        if !ctx.can_access_queue(queue) {
            return Err(AppError::NotFound(format!("Queue '{}' not found", queue)));
        }
    }

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
          AND ($2::TEXT[] IS NULL OR queue_name = ANY($2))
          AND ($3::TEXT IS NULL OR queue_name = $3)
        "#,
    )
    .bind(&ctx.organization_id)
    .bind(ctx.queue_scope_filter())
    .bind(validated_queue)
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
/// Uses batch INSERT with UNNEST for better performance (single round-trip).
/// SECURITY: Enforces plan limits before creating jobs
pub async fn bulk_enqueue(
    State(state): State<AppState>,
    Extension(ctx): Extension<ApiKeyContext>,
    ValidatedJson(request): ValidatedJson<crate::models::BulkEnqueueRequest>,
) -> AppResult<Json<crate::models::BulkEnqueueResponse>> {
    // Enforce API key queue scope (empty/`*` = all queues allowed)
    if !ctx.can_access_queue(&request.queue_name) {
        return Err(AppError::Authorization(format!(
            "API key does not have permission for queue '{}'",
            request.queue_name
        )));
    }

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

    // Prepare arrays for batch INSERT using UNNEST
    let mut job_ids: Vec<String> = Vec::with_capacity(request.jobs.len());
    let mut statuses: Vec<String> = Vec::with_capacity(request.jobs.len());
    let mut payloads: Vec<String> = Vec::with_capacity(request.jobs.len());
    let mut priorities: Vec<i32> = Vec::with_capacity(request.jobs.len());
    let mut scheduled_ats: Vec<Option<chrono::DateTime<Utc>>> =
        Vec::with_capacity(request.jobs.len());
    let mut idempotency_keys: Vec<Option<String>> = Vec::with_capacity(request.jobs.len());
    let mut indexes: Vec<i32> = Vec::with_capacity(request.jobs.len());
    let mut max_payload_size: usize = 0;

    for (index, job_item) in request.jobs.iter().enumerate() {
        let job_id = Uuid::new_v4().to_string();
        let priority = job_item.priority.unwrap_or(default_priority);
        let initial_status = if job_item.scheduled_at.is_some() && job_item.scheduled_at > Some(now)
        {
            "scheduled"
        } else {
            "pending"
        };

        job_ids.push(job_id);
        statuses.push(initial_status.to_string());
        let payload_json = serde_json::to_string(&job_item.payload).unwrap_or_default();
        max_payload_size = max_payload_size.max(payload_json.len());
        payloads.push(payload_json);
        priorities.push(priority);
        scheduled_ats.push(job_item.scheduled_at);
        idempotency_keys.push(job_item.idempotency_key.clone());
        indexes.push(index as i32);
    }

    // SECURITY: Enforce per-plan payload size limit for bulk enqueue too.
    // Previously, /jobs created enforced plan payload limits, but /jobs/bulk only validated 1MB.
    if let Err(response) = check_payload_size(state.db.pool(), &org_id, max_payload_size).await {
        return Err(AppError::LimitExceeded(Box::new(response)));
    }

    // Atomically reserve the daily-job quota for the WHOLE batch before inserting.
    // check_job_limits above is a fast non-atomic pre-check (and still guards the soft
    // active-jobs limit); this row-locked reservation is the authoritative daily cap,
    // closing the window where N concurrent bulk requests each passed the pre-check and
    // then all inserted, overshooting jobs_per_day (the single-enqueue path already does
    // this via try_increment_daily_jobs). All-or-nothing: a batch that would exceed the
    // cap is rejected before any job is created. A batch containing in-batch idempotency
    // duplicates reserves its request size, i.e. counts conservatively (never overshoots).
    let daily_limit = daily_jobs_limit(state.db.pool(), &org_id)
        .await
        .unwrap_or(-1);
    let quota_reserved = match try_increment_daily_jobs(
        state.db.pool(),
        &org_id,
        job_count as i32,
        daily_limit,
    )
    .await
    {
        Ok(count) if count >= 0 => true,
        Ok(_) => {
            let response = check_job_limits(state.db.pool(), &org_id, job_count)
                .await
                .err()
                .unwrap_or_else(|| {
                    (StatusCode::TOO_MANY_REQUESTS, "Daily job limit reached").into_response()
                });
            return Err(AppError::LimitExceeded(Box::new(response)));
        }
        Err(e) => {
            // Counter DB error: proceed best-effort (matching the single-enqueue path)
            // rather than failing an otherwise-valid batch. The post-insert block below
            // then credits the actual creations.
            tracing::warn!(error = %e, org_id = %org_id, "Failed to reserve daily job quota for bulk enqueue");
            false
        }
    };

    // Batch INSERT using UNNEST - single database round-trip
    // Returns: (index, returned_job_id, input_job_id) to determine which were created vs deduplicated
    let results: Result<Vec<(i32, String, String)>, sqlx::Error> = sqlx::query_as(
        r#"
        WITH input_jobs AS (
            SELECT
                idx,
                jid,
                stat,
                pay,
                pri,
                sch,
                idem
            FROM UNNEST(
                $1::INT[],
                $2::TEXT[],
                $3::TEXT[],
                $4::TEXT[],
                $5::INT[],
                $6::TIMESTAMPTZ[],
                $7::TEXT[]
            ) AS t(idx, jid, stat, pay, pri, sch, idem)
        ),
        -- Deduplicate within the batch: two items sharing one idempotency_key would
        -- make `ON CONFLICT DO UPDATE` "affect a row a second time" and abort the whole
        -- INSERT. Keep only the first row per key (null keys stay distinct via their jid);
        -- the final SELECT below still maps every input row to the resulting job id.
        dedup_input AS (
            SELECT DISTINCT ON (COALESCE(idem, jid)) idx, jid, stat, pay, pri, sch, idem
            FROM input_jobs
            ORDER BY COALESCE(idem, jid), idx
        ),
        inserted AS (
            INSERT INTO jobs (
                id, organization_id, queue_name, status, payload, priority,
                max_retries, timeout_seconds, created_at, scheduled_at,
                idempotency_key, updated_at
            )
            SELECT
                ij.jid,
                $8,
                $9,
                ij.stat,
                ij.pay::JSONB,
                ij.pri,
                $10,
                $11,
                $12,
                ij.sch,
                ij.idem,
                $12
            FROM dedup_input ij
            ON CONFLICT (organization_id, idempotency_key)
            WHERE idempotency_key IS NOT NULL
            DO UPDATE SET updated_at = NOW()
            RETURNING id, idempotency_key
        )
        SELECT
            ij.idx,
            COALESCE(ins.id, ij.jid) as returned_id,
            ij.jid as input_id
        FROM input_jobs ij
        LEFT JOIN inserted ins ON (
            ins.idempotency_key IS NOT NULL AND ins.idempotency_key = ij.idem
        ) OR (
            ij.idem IS NULL AND ins.id = ij.jid
        )
        ORDER BY ij.idx
        "#,
    )
    .bind(&indexes)
    .bind(&job_ids)
    .bind(&statuses)
    .bind(&payloads)
    .bind(&priorities)
    .bind(&scheduled_ats)
    .bind(&idempotency_keys)
    .bind(&org_id)
    .bind(&request.queue_name)
    .bind(default_max_retries)
    .bind(default_timeout)
    .bind(now)
    .fetch_all(state.db.pool())
    .await;

    let mut succeeded = Vec::new();
    let mut failed = Vec::new();
    let mut successful_count: u64 = 0;
    let total_jobs = indexes.len(); // Save count before potential move

    match results {
        Ok(rows) => {
            for (index, returned_id, input_id) in rows {
                let created = returned_id == input_id;
                if created {
                    successful_count += 1;
                }
                succeeded.push(crate::models::BulkJobResult {
                    index: index as usize,
                    job_id: returned_id,
                    created,
                });
            }
        }
        Err(e) => {
            // If batch insert fails entirely, report all as failed
            tracing::error!(error = %e, "Bulk enqueue batch insert failed");
            for index in 0..total_jobs {
                failed.push(crate::models::BulkJobError {
                    index,
                    error: "Failed to create job".to_string(),
                });
            }
        }
    }

    let success_count = succeeded.len();
    let failure_count = failed.len();
    let total = success_count + failure_count;

    // Update metrics and usage counters
    if successful_count > 0 {
        state.metrics.jobs_enqueued.inc_by(successful_count);
    }

    // Reconcile the daily counter so it reflects the jobs actually created.
    if quota_reserved {
        // The whole batch (job_count) was reserved up-front to prevent overshoot; refund
        // the credits for rows NOT created — in-batch/idempotency duplicates, or a fully
        // failed insert (successful_count == 0) — so the net credit equals the creations.
        let unused = job_count as i64 - successful_count as i64;
        if unused > 0 {
            // Refund via the dedicated sign-safe function. Calling
            // increment_daily_jobs with a negative count assigned instead of
            // adding across a reset boundary (negative daily counter = free
            // quota) and decremented the lifetime total.
            if let Err(e) = refund_daily_jobs(state.db.pool(), &org_id, unused as i32).await {
                tracing::warn!(
                    error = %e,
                    org_id = %org_id,
                    refund = unused,
                    "Failed to refund unused daily job reservation after bulk enqueue"
                );
            }
        }
    } else if successful_count > 0 {
        // Reservation errored above: fall back to crediting the actual creations.
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

            // Also publish realtime job.created events for newly-created jobs (best-effort).
            // This allows WebSocket consumers to show new jobs immediately.
            for item in &succeeded {
                if item.created {
                    let priority = priorities
                        .get(item.index)
                        .copied()
                        .unwrap_or(default_priority);
                    publish_realtime_event(
                        cache,
                        &org_id,
                        serde_json::json!({
                            "type": "JobCreated",
                            "data": {
                                "job_id": item.job_id,
                                "queue_name": request.queue_name,
                                "priority": priority,
                                "timestamp": Utc::now().to_rfc3339()
                            }
                        }),
                    )
                    .await;
                }
            }
        }

        for item in &succeeded {
            if item.created {
                let priority = priorities
                    .get(item.index)
                    .copied()
                    .unwrap_or(default_priority);
                state.outgoing_webhooks.spawn_dispatch(
                    org_id.clone(),
                    "job.created".to_string(),
                    item.job_id.clone(),
                    serde_json::json!({
                        "job_id": item.job_id,
                        "queue_name": request.queue_name,
                        "priority": priority,
                        "status": "pending",
                    }),
                );
            }
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

/// Boost job priority
///
/// Now validates priority bounds
pub async fn boost_priority(
    State(state): State<AppState>,
    Extension(ctx): Extension<ApiKeyContext>,
    Path(id): Path<String>,
    ValidatedJson(request): ValidatedJson<crate::models::BoostPriorityRequest>,
) -> AppResult<Json<crate::models::BoostPriorityResponse>> {
    // `priority` is already validated to [-100, 100] by ValidatedJson, so no
    // additional clamp is needed (the previous ±1000 clamp was unreachable).
    let new_priority = request.priority;

    // Get current priority + queue - with organization check
    let job: Option<(i32, String)> = sqlx::query_as(
        "SELECT priority, queue_name FROM jobs WHERE id = $1 AND organization_id = $2 AND status IN ('pending', 'scheduled')"
    )
    .bind(&id)
    .bind(&ctx.organization_id)
    .fetch_optional(state.db.pool())
    .await?;

    let Some((old_priority, queue_name)) = job else {
        return Err(AppError::NotFound(format!(
            "Job {} not found or not in pending/scheduled state",
            id
        )));
    };

    // Queue scope: a restricted key cannot boost jobs in queues outside its scope.
    if !ctx.can_access_queue(&queue_name) {
        return Err(AppError::NotFound(format!(
            "Job {} not found or not in pending/scheduled state",
            id
        )));
    }

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

    // Validate queue_name to prevent injection (same as list())
    let validated_queue = query.queue_name.as_ref().and_then(|q| {
        if validate_queue_name_filter(q) {
            Some(q.clone())
        } else {
            tracing::warn!(queue_name = %q, "Invalid queue_name filter ignored in list_dlq");
            None
        }
    });

    let validated_tag = query.tag.as_ref().and_then(|t| {
        if validate_tag_filter(t) {
            Some(t.clone())
        } else {
            tracing::warn!(tag = %t, "Invalid tag filter ignored in list_dlq");
            None
        }
    });

    let mut qb: QueryBuilder<Postgres> =
        QueryBuilder::new("SELECT * FROM jobs WHERE status = 'deadletter' AND organization_id = ");
    qb.push_bind(&ctx.organization_id);

    // Queue scope: restrict a scoped key's DLQ listing to its own queues.
    if let Some(allowed) = ctx.queue_scope_filter() {
        qb.push(" AND queue_name = ANY(");
        qb.push_bind(allowed.to_vec());
        qb.push(")");
    }

    if let Some(queue) = &validated_queue {
        qb.push(" AND queue_name = ");
        qb.push_bind(queue);
    }

    if let Some(tag) = &validated_tag {
        qb.push(" AND tags ? ");
        qb.push_bind(tag);
    }

    qb.push(" ORDER BY created_at DESC LIMIT ");
    qb.push_bind(limit);
    qb.push(" OFFSET ");
    qb.push_bind(offset);

    let jobs: Vec<Job> = qb.build_query_as().fetch_all(state.db.pool()).await?;

    let summaries: Vec<JobSummary> = jobs.into_iter().map(Into::into).collect();
    Ok(Json(summaries))
}

/// Retry jobs from dead-letter queue
///
/// Now uses safe_limit method
/// SECURITY: Enforces plan limits before retrying jobs
pub async fn retry_dlq(
    State(state): State<AppState>,
    Extension(ctx): Extension<ApiKeyContext>,
    ValidatedJson(request): ValidatedJson<crate::models::RetryDlqRequest>,
) -> AppResult<Json<crate::models::RetryDlqResponse>> {
    // Use safe_limit method to enforce bounds
    let limit = request.safe_limit();

    // Queue scope: a restricted key may only retry DLQ jobs in a queue it can access.
    // The `job_ids` and all-queue branches can span queues, so they are denied for
    // scoped keys — such a key must name an accessible queue_name.
    if ctx.queue_scope_filter().is_some() {
        if request.job_ids.is_some() {
            return Err(AppError::Authorization(
                "Queue-scoped API keys must retry DLQ by queue_name, not job_ids".to_string(),
            ));
        }
        match &request.queue_name {
            Some(q) => require_queue_access(&ctx, q)?,
            None => {
                return Err(AppError::Authorization(
                    "Queue-scoped API keys must specify a queue_name for DLQ retry".to_string(),
                ))
            }
        }
    }

    // Bound the client-supplied id list to the same safe limit as the queue path.
    // The job_ids branch uses `id = ANY($1)` with no cap, so without this a caller
    // could submit an arbitrarily large array (unbounded query + retry).
    if let Some(ref job_ids) = request.job_ids {
        if job_ids.len() as i64 > limit {
            return Err(AppError::Validation(format!(
                "job_ids exceeds the maximum of {} ids per retry request",
                limit
            )));
        }
    }

    // First, count how many jobs would be retried (to check limits)
    // Note: Using subquery with LIMIT since COUNT(*) ignores LIMIT clause
    let job_count: i64 = if let Some(ref job_ids) = request.job_ids {
        sqlx::query_scalar(
            "SELECT COUNT(*) FROM jobs WHERE id = ANY($1) AND organization_id = $2 AND status = 'deadletter'"
        )
        .bind(job_ids)
        .bind(&ctx.organization_id)
        .fetch_one(state.db.pool())
        .await?
    } else if let Some(ref queue_name) = request.queue_name {
        sqlx::query_scalar(
            "SELECT COUNT(*) FROM (SELECT 1 FROM jobs WHERE organization_id = $1 AND queue_name = $2 AND status = 'deadletter' LIMIT $3) sub"
        )
        .bind(&ctx.organization_id)
        .bind(queue_name)
        .bind(limit)
        .fetch_one(state.db.pool())
        .await?
    } else {
        sqlx::query_scalar(
            "SELECT COUNT(*) FROM (SELECT 1 FROM jobs WHERE organization_id = $1 AND status = 'deadletter' LIMIT $2) sub"
        )
        .bind(&ctx.organization_id)
        .bind(limit)
        .fetch_one(state.db.pool())
        .await?
    };

    // Check job limits before retrying (DLQ retry adds to active jobs count)
    if job_count > 0 {
        if let Err(response) =
            check_job_limits(state.db.pool(), &ctx.organization_id, job_count as u64).await
        {
            return Err(AppError::LimitExceeded(Box::new(response)));
        }
    }

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

    // Remove the corresponding dead_letter_queue audit rows — the job is live
    // again, and nothing else ever deletes them (no FK), so they would dangle.
    if !retried_job_ids.is_empty() {
        if let Err(e) = sqlx::query(
            "DELETE FROM dead_letter_queue WHERE job_id = ANY($1) AND organization_id = $2",
        )
        .bind(&retried_job_ids)
        .bind(&ctx.organization_id)
        .execute(state.db.pool())
        .await
        {
            warn!(error = %e, "Failed to clean dead_letter_queue rows after DLQ retry");
        }
    }

    // Reset child jobs that were blocked due to parent failure
    // When a parent job is retried from DLQ, its child jobs (with dependencies_met = false)
    // should have their dependencies recalculated
    // SECURITY: Must include organization_id to prevent cross-tenant modification
    if !retried_job_ids.is_empty() {
        let reset_children = sqlx::query(
            r#"
            UPDATE jobs
            SET dependencies_met = FALSE, updated_at = NOW()
            WHERE parent_job_id = ANY($1)
              AND organization_id = $2
              AND status IN ('pending', 'scheduled')
            "#,
        )
        .bind(&retried_job_ids)
        .bind(&ctx.organization_id)
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

    // Queue scope: a restricted key may only purge within a queue it can access. The
    // `queue_name = None` branches below delete across ALL queues, so they are denied
    // for scoped keys — such a key must name an accessible queue_name.
    if ctx.queue_scope_filter().is_some() {
        match &request.queue_name {
            Some(q) => require_queue_access(&ctx, q)?,
            None => {
                return Err(AppError::Authorization(
                    "Queue-scoped API keys must specify a queue_name to purge".to_string(),
                ))
            }
        }
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

    // Clean up dead_letter_queue rows whose jobs no longer exist (this purge
    // and any earlier ones — there is no FK, so they accumulate forever).
    if let Err(e) = sqlx::query(
        r#"
        DELETE FROM dead_letter_queue dlq
        WHERE dlq.organization_id = $1
          AND NOT EXISTS (SELECT 1 FROM jobs j WHERE j.id = dlq.job_id)
        "#,
    )
    .bind(&ctx.organization_id)
    .execute(state.db.pool())
    .await
    {
        warn!(error = %e, "Failed to clean orphaned dead_letter_queue rows after purge");
    }

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

    // Queue scope: drop jobs in queues a restricted key can't access (per-job, since
    // the id list can span queues). Unrestricted keys keep everything.
    let jobs: Vec<BatchJobStatus> = jobs
        .into_iter()
        .filter(|j| ctx.can_access_queue(&j.queue_name))
        .collect();

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
            tag: None,
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
            tag: None,
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
            tag: None,
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
