//! Schedule handlers for recurring jobs
//!
//! CRUD operations for cron-based job schedules.

use axum::{
    extract::{Extension, Path, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use chrono::Utc;
use serde::Deserialize;
use tracing::{error, info};

use crate::api::handlers::require_queue_access;
use crate::api::middleware::limits::{
    check_job_limits, check_payload_size, check_payload_size_generic, check_resource_limit_conn,
    daily_jobs_limit, lock_resource, refund_daily_jobs, try_increment_daily_jobs, LimitCheckError,
};
use crate::api::middleware::validation::ValidatedJson;
use crate::api::AppState;
use crate::models::{
    ApiKeyContext, CreateScheduleRequest, CreateScheduleResponse, CronSchedule, Schedule,
    UpdateScheduleRequest,
};

async fn require_schedule_access(
    state: &AppState,
    ctx: &ApiKeyContext,
    schedule_id: &str,
) -> Result<(), (StatusCode, String)> {
    let queue_name: Option<String> = sqlx::query_scalar(
        "SELECT queue_name FROM schedules WHERE id = $1 AND organization_id = $2",
    )
    .bind(schedule_id)
    .bind(&ctx.organization_id)
    .fetch_optional(state.db.pool())
    .await
    .map_err(|e| {
        error!(error = %e, "Failed to authorize schedule");
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            "Failed to authorize schedule".to_string(),
        )
    })?;

    match queue_name {
        Some(queue) if ctx.can_access_queue(&queue) => Ok(()),
        _ => Err((StatusCode::NOT_FOUND, "Schedule not found".to_string())),
    }
}

/// 400 for a cron expression with no upcoming fire time.
fn never_runs_response() -> Response {
    (
        StatusCode::BAD_REQUEST,
        "Invalid cron expression: the schedule has no upcoming run time".to_string(),
    )
        .into_response()
}

/// List all schedules
///
/// GET /api/v1/schedules
///
/// Now uses authenticated org context instead of query parameter
pub async fn list(
    State(state): State<AppState>,
    Extension(ctx): Extension<ApiKeyContext>,
    Query(params): Query<ListSchedulesQuery>,
) -> Result<Json<Vec<Schedule>>, (StatusCode, String)> {
    // Use authenticated org context, ignore query param
    let org_id = &ctx.organization_id;
    let is_active = params.is_active_filter()?;

    // Use safe_limit() to enforce maximum. `queue_name`, `is_active` and `offset`
    // are what the Go and Python SDKs send; they used to be ignored, so a filtered
    // call returned every schedule and an offset loop re-read page one forever.
    // `id` breaks created_at ties so offset pages neither repeat nor skip rows.
    let schedules: Vec<Schedule> = sqlx::query_as(
        r#"
        SELECT *
        FROM schedules
        WHERE organization_id = $1
          AND queue_in_key_scope(queue_name, $3::TEXT[])
          AND ($4::TEXT IS NULL OR queue_name = $4)
          AND ($5::BOOLEAN IS NULL OR is_active = $5)
        ORDER BY created_at DESC, id DESC
        LIMIT $2 OFFSET $6
        "#,
    )
    .bind(org_id)
    .bind(params.safe_limit()) // Use bounded limit
    .bind(ctx.queue_scope_filter())
    .bind(params.queue_name.as_deref())
    .bind(is_active)
    .bind(params.offset.unwrap_or(0) as i64)
    .fetch_all(state.db.pool())
    .await
    // Don't leak database error details
    .map_err(|e| {
        error!(error = %e, "Failed to list schedules");
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            "Failed to list schedules".to_string(),
        )
    })?;

    Ok(Json(schedules))
}

/// Get a schedule by ID
///
/// GET /api/v1/schedules/{id}
///
/// Now checks organization isolation
pub async fn get(
    State(state): State<AppState>,
    Extension(ctx): Extension<ApiKeyContext>,
    Path(id): Path<String>,
) -> Result<Json<Schedule>, (StatusCode, String)> {
    require_schedule_access(&state, &ctx, &id).await?;
    let schedule: Option<Schedule> =
        sqlx::query_as("SELECT * FROM schedules WHERE id = $1 AND organization_id = $2")
            .bind(&id)
            .bind(&ctx.organization_id)
            .fetch_optional(state.db.pool())
            .await
            // Don't leak database error details
            .map_err(|e| {
                error!(error = %e, "Failed to get schedule");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "Failed to get schedule".to_string(),
                )
            })?;

    schedule
        .map(Json)
        .ok_or((StatusCode::NOT_FOUND, "Schedule not found".to_string()))
}

/// Create a new schedule
///
/// POST /api/v1/schedules
///
/// Now uses authenticated org context.
/// SECURITY: Enforces plan limits before creating schedules.
pub async fn create(
    State(state): State<AppState>,
    Extension(ctx): Extension<ApiKeyContext>,
    ValidatedJson(req): ValidatedJson<CreateScheduleRequest>,
) -> Result<(StatusCode, Json<CreateScheduleResponse>), axum::response::Response> {
    use axum::response::IntoResponse;
    let org_id = &ctx.organization_id;

    // Queue scope: a schedule is an indirect enqueue — a restricted key must not
    // schedule jobs into a queue outside its scope.
    crate::api::handlers::require_queue_access(&ctx, &req.queue_name)
        .map_err(IntoResponse::into_response)?;

    // Enforce plan payload size for schedule payload_template at creation time too.
    let payload_json = serde_json::to_string(&req.payload_template).unwrap_or_default();
    check_payload_size_generic(state.db.pool(), org_id, payload_json.len())
        .await
        .map_err(|e| match e {
            LimitCheckError::Database(msg) => {
                error!(error = %msg, "Failed to check payload size for schedule creation");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "Failed to check limits".to_string(),
                )
                    .into_response()
            }
            // Return 403 to match other plan-limit errors in this handler module.
            _ => (StatusCode::FORBIDDEN, e.to_string()).into_response(),
        })?;

    // Validate cron expression
    let cron = CronSchedule::parse(&req.cron_expression).map_err(|e| {
        (
            StatusCode::BAD_REQUEST,
            format!("Invalid cron expression: {}", e),
        )
            .into_response()
    })?;

    // Omitted retry/timeout come from the target queue's config. A schedule into a
    // queue that does not exist yet creates it (active schedules count as queues),
    // so it is charged against the plan's queue cap like a first enqueue.
    let queue_defaults = crate::api::middleware::limits::queue_job_defaults(
        state.db.pool(),
        &state.settings.queue,
        org_id,
        &req.queue_name,
    )
    .await
    .map_err(|e| {
        error!(error = %e, "Failed to read queue defaults for schedule creation");
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            "Failed to check limits".to_string(),
        )
            .into_response()
    })?;
    if !queue_defaults.exists {
        crate::api::middleware::limits::check_new_queue_limit(state.db.pool(), org_id, 1).await?;
    }

    // Calculate next run time in the schedule's timezone (stored as UTC). A
    // schedule with no next run would be stored with `next_run_at: null` and never
    // fire, so it is refused rather than reported as created.
    let next_run =
        cron.next_run_after_in_timezone(Utc::now(), req.timezone.as_deref().unwrap_or("UTC"));
    if next_run.is_none() {
        return Err(never_runs_response());
    }

    let id = uuid::Uuid::new_v4().to_string();
    let now = Utc::now();

    // Enforce the schedules cap atomically under an advisory lock so concurrent
    // creates for this org cannot overshoot the plan limit.
    let tx_err = || {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            "Failed to create schedule".to_string(),
        )
            .into_response()
    };
    let mut tx = state.db.pool().begin().await.map_err(|e| {
        error!(error = %e, "Failed to begin transaction for schedule create");
        tx_err()
    })?;
    lock_resource(&mut tx, org_id, "schedules")
        .await
        .map_err(|e| {
            error!(error = %e, "Failed to acquire schedule limit lock");
            tx_err()
        })?;
    // Propagate the STRUCTURED limit response (JSON with code "QUOTA_EXCEEDED"),
    // matching every other create handler — previously this returned a plain-text
    // 429 so SDKs classified schedule-quota errors as UNKNOWN_ERROR.
    check_resource_limit_conn(&mut tx, org_id, "schedules", 1).await?;

    sqlx::query(
        r#"
        INSERT INTO schedules (
            id, organization_id, name, description, cron_expression, timezone,
            queue_name, payload_template, priority, max_retries, timeout_seconds,
            is_active, next_run_at, tags, metadata, created_at, updated_at
        )
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, TRUE, $12, $13, $14, $15, $15)
        "#,
    )
    .bind(&id)
    .bind(org_id)
    .bind(&req.name)
    .bind(&req.description)
    .bind(&req.cron_expression)
    .bind(req.timezone.as_deref().unwrap_or("UTC"))
    .bind(&req.queue_name)
    .bind(&req.payload_template)
    .bind(req.priority.unwrap_or(0))
    .bind(
        req.max_retries
            .unwrap_or(queue_defaults.max_retries)
            .clamp(0, 100),
    )
    .bind(
        req.timeout_seconds
            .unwrap_or(queue_defaults.timeout_seconds)
            .clamp(1, 86400),
    )
    .bind(next_run)
    .bind(&req.tags)
    .bind(&req.metadata)
    .bind(now)
    .execute(&mut *tx)
    .await
    // Don't leak database error details
    .map_err(|e| {
        error!(error = %e, "Failed to create schedule");
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            "Failed to create schedule".to_string(),
        )
            .into_response()
    })?;

    tx.commit().await.map_err(|e| {
        error!(error = %e, "Failed to commit schedule create");
        tx_err()
    })?;

    info!(schedule_id = %id, name = %req.name, "Schedule created");

    Ok((
        StatusCode::CREATED,
        Json(CreateScheduleResponse {
            id,
            name: req.name,
            cron_expression: req.cron_expression,
            next_run_at: next_run,
        }),
    ))
}

/// Update a schedule
///
/// PUT /api/v1/schedules/{id}
///
pub async fn update(
    State(state): State<AppState>,
    Extension(ctx): Extension<ApiKeyContext>,
    Path(id): Path<String>,
    ValidatedJson(req): ValidatedJson<UpdateScheduleRequest>,
) -> Result<Json<Schedule>, Response> {
    let org_id = &ctx.organization_id;
    let internal = |msg: &str| (StatusCode::INTERNAL_SERVER_ERROR, msg.to_string()).into_response();
    require_schedule_access(&state, &ctx, &id)
        .await
        .map_err(IntoResponse::into_response)?;

    let current: Option<Schedule> =
        sqlx::query_as("SELECT * FROM schedules WHERE id = $1 AND organization_id = $2")
            .bind(&id)
            .bind(org_id)
            .fetch_optional(state.db.pool())
            .await
            .map_err(|e| {
                error!(error = %e, "Failed to load schedule for update");
                internal("Failed to update schedule")
            })?;
    let current = current
        .ok_or_else(|| (StatusCode::NOT_FOUND, "Schedule not found".to_string()).into_response())?;

    // Recompute next_run_at when the cron expression OR the timezone changes —
    // both alter the next fire time. The effective values are the incoming ones
    // falling back to what is currently stored. Re-activating a paused schedule
    // recomputes it from now as /resume does; the stored value is the fire it
    // missed while paused, so keeping it fired the schedule on the next tick.
    let reactivating = req.is_active == Some(true) && !current.is_active;
    let next_run = if req.cron_expression.is_some() || req.timezone.is_some() {
        let cron_expr = req
            .cron_expression
            .as_deref()
            .unwrap_or(&current.cron_expression);
        let timezone = req.timezone.as_deref().unwrap_or(&current.timezone);

        let cron = CronSchedule::parse(cron_expr).map_err(|e| {
            (
                StatusCode::BAD_REQUEST,
                format!("Invalid cron expression: {}", e),
            )
                .into_response()
        })?;
        // Same rule as create: an update must not leave a schedule that never fires
        // (and `COALESCE` below would otherwise keep the old next_run_at).
        Some(
            cron.next_run_after_in_timezone(Utc::now(), timezone)
                .ok_or_else(never_runs_response)?,
        )
    } else if reactivating {
        CronSchedule::parse(&current.cron_expression)
            .ok()
            .and_then(|c| c.next_run_after_in_timezone(Utc::now(), &current.timezone))
    } else {
        None
    };

    // `is_active: true` on a paused schedule takes a schedules slot, so it pays
    // the same advisory-locked cap as create and /resume; without this,
    // pause -> create -> PUT is_active overshot the plan. The flag is re-read
    // under the lock so concurrent re-activations of one schedule charge once.
    let mut tx = state.db.pool().begin().await.map_err(|e| {
        error!(error = %e, "Failed to begin transaction for schedule update");
        internal("Failed to update schedule")
    })?;
    if req.is_active == Some(true) {
        lock_resource(&mut tx, org_id, "schedules")
            .await
            .map_err(|e| {
                error!(error = %e, "Failed to acquire schedule limit lock on update");
                internal("Failed to update schedule")
            })?;
        let active_now: Option<bool> = sqlx::query_scalar(
            "SELECT is_active FROM schedules WHERE id = $1 AND organization_id = $2 FOR UPDATE",
        )
        .bind(&id)
        .bind(org_id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(|e| {
            error!(error = %e, "Failed to lock schedule for update");
            internal("Failed to update schedule")
        })?;
        if active_now == Some(false) {
            check_resource_limit_conn(&mut tx, org_id, "schedules", 1).await?;
        }
    }

    let schedule: Option<Schedule> = sqlx::query_as(
        r#"
        UPDATE schedules
        SET
            name = COALESCE($3, name),
            description = COALESCE($4, description),
            cron_expression = COALESCE($5, cron_expression),
            timezone = COALESCE($6, timezone),
            payload_template = COALESCE($7, payload_template),
            priority = COALESCE($8, priority),
            max_retries = COALESCE($9, max_retries),
            timeout_seconds = COALESCE($10, timeout_seconds),
            is_active = COALESCE($11, is_active),
            next_run_at = COALESCE($12, next_run_at),
            tags = COALESCE($13, tags),
            metadata = COALESCE($14, metadata),
            updated_at = NOW()
        WHERE id = $1 AND organization_id = $2
        RETURNING *
        "#,
    )
    .bind(&id)
    .bind(org_id)
    .bind(&req.name)
    .bind(&req.description)
    .bind(&req.cron_expression)
    .bind(&req.timezone)
    .bind(&req.payload_template)
    .bind(req.priority)
    .bind(req.max_retries)
    .bind(req.timeout_seconds)
    .bind(req.is_active)
    .bind(next_run)
    .bind(&req.tags)
    .bind(&req.metadata)
    .fetch_optional(&mut *tx)
    .await
    // Don't leak database error details
    .map_err(|e| {
        error!(error = %e, "Failed to update schedule");
        internal("Failed to update schedule")
    })?;

    tx.commit().await.map_err(|e| {
        error!(error = %e, "Failed to commit schedule update");
        internal("Failed to update schedule")
    })?;

    schedule
        .map(Json)
        .ok_or_else(|| (StatusCode::NOT_FOUND, "Schedule not found".to_string()).into_response())
}

/// Delete a schedule
///
/// DELETE /api/v1/schedules/{id}
///
pub async fn delete(
    State(state): State<AppState>,
    Extension(ctx): Extension<ApiKeyContext>,
    Path(id): Path<String>,
) -> Result<StatusCode, (StatusCode, String)> {
    require_schedule_access(&state, &ctx, &id).await?;
    let result = sqlx::query("DELETE FROM schedules WHERE id = $1 AND organization_id = $2")
        .bind(&id)
        .bind(&ctx.organization_id)
        .execute(state.db.pool())
        .await
        // Don't leak database error details
        .map_err(|e| {
            error!(error = %e, "Failed to delete schedule");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to delete schedule".to_string(),
            )
        })?;

    if result.rows_affected() == 0 {
        return Err((StatusCode::NOT_FOUND, "Schedule not found".to_string()));
    }

    info!(schedule_id = %id, org_id = %ctx.organization_id, "Schedule deleted");
    Ok(StatusCode::NO_CONTENT)
}

/// Pause a schedule
///
/// POST /api/v1/schedules/{id}/pause
///
pub async fn pause(
    State(state): State<AppState>,
    Extension(ctx): Extension<ApiKeyContext>,
    Path(id): Path<String>,
) -> Result<Json<Schedule>, (StatusCode, String)> {
    require_schedule_access(&state, &ctx, &id).await?;
    let schedule: Option<Schedule> = sqlx::query_as(
        r#"
        UPDATE schedules
        SET is_active = FALSE, updated_at = NOW()
        WHERE id = $1 AND organization_id = $2
        RETURNING *
        "#,
    )
    .bind(&id)
    .bind(&ctx.organization_id)
    .fetch_optional(state.db.pool())
    .await
    // Don't leak database error details
    .map_err(|e| {
        error!(error = %e, "Failed to pause schedule");
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            "Failed to pause schedule".to_string(),
        )
    })?;

    schedule
        .map(Json)
        .ok_or((StatusCode::NOT_FOUND, "Schedule not found".to_string()))
}

/// Resume a schedule
///
/// POST /api/v1/schedules/{id}/resume
///
pub async fn resume(
    State(state): State<AppState>,
    Extension(ctx): Extension<ApiKeyContext>,
    Path(id): Path<String>,
) -> Result<Json<Schedule>, Response> {
    require_schedule_access(&state, &ctx, &id)
        .await
        .map_err(|(status, msg)| (status, msg).into_response())?;
    // Get current cron expression to calculate next run
    let current: Option<Schedule> =
        sqlx::query_as("SELECT * FROM schedules WHERE id = $1 AND organization_id = $2")
            .bind(&id)
            .bind(&ctx.organization_id)
            .fetch_optional(state.db.pool())
            .await
            // Don't expose database error details
            .map_err(|e| {
                error!(error = %e, "Failed to get schedule for resume");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "Failed to get schedule".to_string(),
                )
                    .into_response()
            })?;

    let current = current
        .ok_or_else(|| (StatusCode::NOT_FOUND, "Schedule not found".to_string()).into_response())?;

    let next_run = CronSchedule::parse(&current.cron_expression)
        .ok()
        .and_then(|c| c.next_run_after_in_timezone(Utc::now(), &current.timezone));

    // Resuming a paused schedule re-consumes a schedules slot. Enforce the same
    // advisory-locked cap as create so pause→create→resume cannot overshoot.
    let mut tx = state.db.pool().begin().await.map_err(|e| {
        error!(error = %e, "Failed to begin transaction for schedule resume");
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            "Failed to resume schedule".to_string(),
        )
            .into_response()
    })?;
    if !current.is_active {
        lock_resource(&mut tx, &ctx.organization_id, "schedules")
            .await
            .map_err(|e| {
                error!(error = %e, "Failed to acquire schedule limit lock on resume");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "Failed to resume schedule".to_string(),
                )
                    .into_response()
            })?;
        check_resource_limit_conn(&mut tx, &ctx.organization_id, "schedules", 1).await?;
    }

    let schedule: Option<Schedule> = sqlx::query_as(
        r#"
        UPDATE schedules
        SET is_active = TRUE, next_run_at = $2, updated_at = NOW()
        WHERE id = $1 AND organization_id = $3
        RETURNING *
        "#,
    )
    .bind(&id)
    .bind(next_run)
    .bind(&ctx.organization_id)
    .fetch_optional(&mut *tx)
    .await
    // Don't leak database error details
    .map_err(|e| {
        error!(error = %e, "Failed to resume schedule");
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            "Failed to resume schedule".to_string(),
        )
            .into_response()
    })?;

    tx.commit().await.map_err(|e| {
        error!(error = %e, "Failed to commit schedule resume");
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            "Failed to resume schedule".to_string(),
        )
            .into_response()
    })?;

    schedule
        .map(Json)
        .ok_or_else(|| (StatusCode::NOT_FOUND, "Schedule not found".to_string()).into_response())
}

/// Trigger a schedule immediately
///
/// POST /api/v1/schedules/{id}/trigger
///
pub async fn trigger(
    State(state): State<AppState>,
    Extension(ctx): Extension<ApiKeyContext>,
    Path(id): Path<String>,
) -> Result<Json<TriggerResponse>, Response> {
    // Get schedule with org check
    let schedule: Option<Schedule> =
        sqlx::query_as("SELECT * FROM schedules WHERE id = $1 AND organization_id = $2")
            .bind(&id)
            .bind(&ctx.organization_id)
            .fetch_optional(state.db.pool())
            .await
            // Don't expose database error details
            .map_err(|e| {
                error!(error = %e, "Failed to get schedule for trigger");
                (StatusCode::INTERNAL_SERVER_ERROR, "Failed to get schedule").into_response()
            })?;

    let schedule =
        schedule.ok_or_else(|| (StatusCode::NOT_FOUND, "Schedule not found").into_response())?;

    // Queue scope: firing a schedule is an indirect enqueue into schedule.queue_name;
    // a restricted key must not trigger a schedule targeting a queue outside its scope.
    require_queue_access(&ctx, &schedule.queue_name).map_err(IntoResponse::into_response)?;

    // Check job limits before creating a job from the schedule trigger. Use the same
    // check_job_limits/check_payload_size helpers the direct enqueue path uses so a
    // triggered job over quota returns the structured 429 `QUOTA_EXCEEDED` (and an
    // oversized template returns 413), instead of the plain-text 403 SDKs
    // misclassified as UNKNOWN_ERROR.
    check_job_limits(state.db.pool(), &ctx.organization_id, 1).await?;

    // Also enforce plan payload size for the schedule's payload template.
    let payload_json = serde_json::to_string(&schedule.payload_template).unwrap_or_default();
    check_payload_size(state.db.pool(), &ctx.organization_id, payload_json.len()).await?;

    // Create job immediately
    let job_id = uuid::Uuid::new_v4().to_string();
    let now = Utc::now();

    // Reserve the daily quota slot BEFORE inserting. check_job_limits above only reads a
    // snapshot of today's count: N concurrent triggers can each observe a count under the
    // cap, all insert, and all increment afterwards, so the cap was only ever enforced
    // against stale data and got overshot in proportion to concurrency. This atomic
    // reserve-then-insert is the real gate; the check above stays because it still guards
    // the soft active_jobs limit and produces the structured error body.
    let daily_limit = daily_jobs_limit(state.db.pool(), &ctx.organization_id)
        .await
        .unwrap_or(-1);
    let quota_reserved = match try_increment_daily_jobs(
        state.db.pool(),
        &ctx.organization_id,
        1,
        daily_limit,
    )
    .await
    {
        Ok(count) if count >= 0 => true,
        Ok(_) => {
            // Over the daily cap. Rebuild the body via check_job_limits so this keeps
            // answering 429 `QUOTA_EXCEEDED` as of v0.1.91 — SDKs classify a bare
            // plain-text rejection as UNKNOWN_ERROR and do not surface it as a quota
            // problem. Nothing was inserted yet, so there is nothing to unwind.
            let response = check_job_limits(state.db.pool(), &ctx.organization_id, 1)
                .await
                .err()
                .unwrap_or_else(|| {
                    (StatusCode::TOO_MANY_REQUESTS, "Daily job limit reached").into_response()
                });
            return Err(response);
        }
        Err(e) => {
            // Counter DB error: fall through and trigger anyway, as this endpoint always
            // has. A counter outage must not turn into a refused trigger. No reservation
            // was taken, so a later insert failure must not refund one.
            tracing::warn!(error = %e, org_id = %ctx.organization_id, "Failed to reserve daily job quota for schedule trigger");
            false
        }
    };

    let insert_result = sqlx::query(
        r#"
        INSERT INTO jobs (
            id, organization_id, queue_name, status, payload, priority,
            max_retries, timeout_seconds, tags, created_at, updated_at
        )
        VALUES ($1, $2, $3, 'pending', $4, $5, $6, $7, $8, $9, $9)
        "#,
    )
    .bind(&job_id)
    .bind(&schedule.organization_id) // Uses org from schedule (already verified)
    .bind(&schedule.queue_name)
    .bind(&schedule.payload_template)
    .bind(schedule.priority)
    .bind(schedule.max_retries)
    .bind(schedule.timeout_seconds)
    .bind(&schedule.tags)
    .bind(now)
    .execute(state.db.pool())
    .await;

    if let Err(e) = insert_result {
        // Hand the reservation back: no job exists, so keeping the slot would shrink the
        // org's day for every failed insert. Must go through refund_daily_jobs — a negative
        // increment_daily_jobs assigns rather than adds across the daily reset, so a refund
        // landing after midnight would drive the counter negative and hand out free quota.
        if quota_reserved {
            if let Err(refund_err) =
                refund_daily_jobs(state.db.pool(), &ctx.organization_id, 1).await
            {
                tracing::warn!(error = %refund_err, org_id = %ctx.organization_id, "Failed to refund reserved daily job quota after schedule trigger insert failed");
            }
        }
        // Don't expose database error details
        error!(error = %e, "Failed to create job from schedule trigger");
        return Err((StatusCode::INTERNAL_SERVER_ERROR, "Failed to create job").into_response());
    }

    // Update schedule's last_run_at and run_count
    sqlx::query(
        r#"
        UPDATE schedules
        SET last_run_at = $2, run_count = run_count + 1, updated_at = $2
        WHERE id = $1 AND organization_id = $3
        "#,
    )
    .bind(&id)
    .bind(now)
    .bind(&ctx.organization_id)
    .execute(state.db.pool())
    .await
    .ok();

    // Record the run in schedule_runs so manual triggers appear in history (the
    // cron worker records its runs; a manual trigger previously bumped run_count
    // but wrote no history row, so run_count and history diverged).
    let run_id = uuid::Uuid::new_v4().to_string();
    let _ = sqlx::query(
        r#"
        INSERT INTO schedule_runs (id, schedule_id, job_id, status, started_at, completed_at)
        VALUES ($1, $2, $3, 'completed', $4, $4)
        "#,
    )
    .bind(&run_id)
    .bind(&id)
    .bind(&job_id)
    .bind(now)
    .execute(state.db.pool())
    .await;

    info!(schedule_id = %id, job_id = %job_id, org_id = %ctx.organization_id, "Schedule triggered manually");

    // Daily counter already reserved before the insert above — no post-hoc increment here.

    // Publish to Redis to notify workers of new job
    if let Some(ref cache) = state.cache {
        let _ = cache
            .publish(
                &format!("org:{}:queue:{}", ctx.organization_id, schedule.queue_name),
                &job_id,
            )
            .await;
    }

    state.outgoing_webhooks.spawn_dispatch(
        ctx.organization_id.clone(),
        "schedule.triggered".to_string(),
        id.clone(),
        serde_json::json!({
            "schedule_id": id,
            "job_id": job_id,
            "queue_name": schedule.queue_name,
        }),
    );
    state.outgoing_webhooks.spawn_dispatch(
        ctx.organization_id.clone(),
        "job.created".to_string(),
        job_id.clone(),
        serde_json::json!({
            "job_id": job_id,
            "queue_name": schedule.queue_name,
            "priority": schedule.priority,
            "status": "pending",
            "source": "schedule_trigger",
            "schedule_id": id,
        }),
    );

    Ok(Json(TriggerResponse {
        job_id,
        triggered_at: now,
    }))
}

/// Get schedule history
///
/// GET /api/v1/schedules/{id}/history
///
pub async fn history(
    State(state): State<AppState>,
    Extension(ctx): Extension<ApiKeyContext>,
    Path(id): Path<String>,
    Query(params): Query<HistoryQuery>,
) -> Result<Json<Vec<ScheduleRunRecord>>, (StatusCode, String)> {
    require_schedule_access(&state, &ctx, &id).await?;
    // First verify the schedule belongs to this organization
    let schedule_exists: Option<(String,)> =
        sqlx::query_as("SELECT id FROM schedules WHERE id = $1 AND organization_id = $2")
            .bind(&id)
            .bind(&ctx.organization_id)
            .fetch_optional(state.db.pool())
            .await
            // Don't leak database error details (BEOPS-27)
            .map_err(|e| {
                error!(error = %e, schedule_id = %id, "Failed to look up schedule for history");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "Failed to get schedule history".to_string(),
                )
            })?;

    if schedule_exists.is_none() {
        return Err((StatusCode::NOT_FOUND, "Schedule not found".to_string()));
    }

    // Use safe_limit() to enforce maximum
    // `status` is the outcome of the run itself (was the job enqueued?), so a
    // manual trigger reads `completed` as soon as the job exists. `job_status`
    // carries what the enqueued job is doing now, so a still-pending job is not
    // mistaken for a finished one.
    let runs: Vec<ScheduleRunRecord> = sqlx::query_as(
        r#"
        SELECT sr.id, sr.schedule_id, sr.job_id, sr.status, sr.error_message,
               sr.started_at, sr.completed_at, j.status AS job_status
        FROM schedule_runs sr
        LEFT JOIN jobs j ON j.id = sr.job_id AND j.organization_id = $3
        WHERE sr.schedule_id = $1
        ORDER BY sr.started_at DESC
        LIMIT $2
        "#,
    )
    .bind(&id)
    .bind(params.safe_limit()) // Use bounded limit
    .bind(&ctx.organization_id)
    .fetch_all(state.db.pool())
    .await
    // Don't leak database error details
    .map_err(|e| {
        error!(error = %e, "Failed to get schedule history");
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            "Failed to get schedule history".to_string(),
        )
    })?;

    Ok(Json(runs))
}

/// Query parameters for listing schedules
///
/// Removed organization_id query param - now uses auth context only
#[derive(Debug, Deserialize)]
pub struct ListSchedulesQuery {
    /// Organization_id in query is IGNORED - use auth context instead
    /// Kept for backwards compatibility but not used
    #[serde(skip)]
    pub _organization_id: Option<String>,
    pub limit: Option<u32>,
    /// Only schedules targeting this queue.
    pub queue_name: Option<String>,
    /// `true` / `false` (also `1` / `0`, any case); see `is_active_filter`.
    pub is_active: Option<String>,
    /// Rows to skip, newest first.
    pub offset: Option<u32>,
}

impl ListSchedulesQuery {
    /// Get limit with enforced maximum to prevent memory exhaustion
    pub fn safe_limit(&self) -> i64 {
        const MAX_SCHEDULE_LIMIT: u32 = 500;
        self.limit.unwrap_or(100).min(MAX_SCHEDULE_LIMIT) as i64
    }

    /// The `is_active` filter. Anything but a boolean is a 400: silently
    /// ignoring an unreadable filter is how every schedule came back before.
    pub fn is_active_filter(&self) -> Result<Option<bool>, (StatusCode, String)> {
        match self.is_active.as_deref().map(str::to_ascii_lowercase) {
            None => Ok(None),
            Some(v) if v == "true" || v == "1" => Ok(Some(true)),
            Some(v) if v == "false" || v == "0" => Ok(Some(false)),
            Some(_) => Err((
                StatusCode::BAD_REQUEST,
                "is_active must be true or false".to_string(),
            )),
        }
    }
}

/// Query parameters for org context
#[derive(Debug, Deserialize)]
pub struct OrgQuery {
    pub organization_id: Option<String>,
}

/// Query parameters for history
///
#[derive(Debug, Deserialize)]
pub struct HistoryQuery {
    #[serde(default)]
    pub limit: Option<u32>,
}

impl HistoryQuery {
    /// Get limit with enforced maximum to prevent DoS
    pub fn safe_limit(&self) -> i64 {
        const MAX_HISTORY_LIMIT: u32 = 500;
        self.limit.unwrap_or(50).min(MAX_HISTORY_LIMIT) as i64
    }
}

/// Response for manual trigger
#[derive(Debug, serde::Serialize)]
pub struct TriggerResponse {
    pub job_id: String,
    pub triggered_at: chrono::DateTime<Utc>,
}

/// Schedule run record
#[derive(Debug, serde::Serialize, sqlx::FromRow)]
pub struct ScheduleRunRecord {
    pub id: String,
    pub schedule_id: String,
    pub job_id: Option<String>,
    pub status: String,
    pub error_message: Option<String>,
    pub started_at: chrono::DateTime<Utc>,
    pub completed_at: Option<chrono::DateTime<Utc>>,
    /// Current status of the job this run enqueued (`pending`, `processing`,
    /// `completed`, ...). `None` when the run created no job or the job has
    /// since been removed by retention.
    pub job_status: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_list_schedules_query_defaults() {
        let query = ListSchedulesQuery {
            _organization_id: None,
            limit: None,
            queue_name: None,
            is_active: None,
            offset: None,
        };
        assert!(query._organization_id.is_none());
        assert!(query.limit.is_none());
        assert_eq!(query.is_active_filter(), Ok(None));
    }

    #[test]
    fn test_trigger_response_serialization() {
        let response = TriggerResponse {
            job_id: "job-123".to_string(),
            triggered_at: Utc::now(),
        };
        let json = serde_json::to_string(&response).unwrap();
        assert!(json.contains("job-123"));
    }
}
