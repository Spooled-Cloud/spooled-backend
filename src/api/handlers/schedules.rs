//! Schedule handlers for recurring jobs
//!
//! CRUD operations for cron-based job schedules.

use axum::{
    extract::{Extension, Path, Query, State},
    http::StatusCode,
    Json,
};
use chrono::Utc;
use serde::Deserialize;
use tracing::{error, info};

use crate::api::middleware::validation::ValidatedJson;
use crate::api::AppState;
use crate::models::{
    ApiKeyContext, CreateScheduleRequest, CreateScheduleResponse, CronSchedule, Schedule,
    UpdateScheduleRequest,
};

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

    // Use safe_limit() to enforce maximum
    let schedules: Vec<Schedule> = sqlx::query_as(
        r#"
        SELECT *
        FROM schedules
        WHERE organization_id = $1
        ORDER BY created_at DESC
        LIMIT $2
        "#,
    )
    .bind(org_id)
    .bind(params.safe_limit()) // Use bounded limit
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
/// Now uses authenticated org context
pub async fn create(
    State(state): State<AppState>,
    Extension(ctx): Extension<ApiKeyContext>,
    ValidatedJson(req): ValidatedJson<CreateScheduleRequest>,
) -> Result<(StatusCode, Json<CreateScheduleResponse>), (StatusCode, String)> {
    let org_id = &ctx.organization_id;

    // Validate cron expression
    let cron = CronSchedule::parse(&req.cron_expression).map_err(|e| {
        (
            StatusCode::BAD_REQUEST,
            format!("Invalid cron expression: {}", e),
        )
    })?;

    // Calculate next run time
    let next_run = cron.next_run_after(Utc::now());

    let id = uuid::Uuid::new_v4().to_string();
    let now = Utc::now();

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
    .bind(req.max_retries.unwrap_or(3))
    .bind(req.timeout_seconds.unwrap_or(300))
    .bind(next_run)
    .bind(&req.tags)
    .bind(&req.metadata)
    .bind(now)
    .execute(state.db.pool())
    .await
    // Don't leak database error details
    .map_err(|e| {
        error!(error = %e, "Failed to create schedule");
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            "Failed to create schedule".to_string(),
        )
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
) -> Result<Json<Schedule>, (StatusCode, String)> {
    let org_id = &ctx.organization_id;

    // Validate cron expression if provided
    let next_run = if let Some(ref cron_expr) = req.cron_expression {
        let cron = CronSchedule::parse(cron_expr).map_err(|e| {
            (
                StatusCode::BAD_REQUEST,
                format!("Invalid cron expression: {}", e),
            )
        })?;
        Some(cron.next_run_after(Utc::now()))
    } else {
        None
    };

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
    .bind(next_run.flatten())
    .bind(&req.tags)
    .bind(&req.metadata)
    .fetch_optional(state.db.pool())
    .await
    // Don't leak database error details
    .map_err(|e| {
        error!(error = %e, "Failed to update schedule");
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            "Failed to update schedule".to_string(),
        )
    })?;

    schedule
        .map(Json)
        .ok_or((StatusCode::NOT_FOUND, "Schedule not found".to_string()))
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
) -> Result<Json<Schedule>, (StatusCode, String)> {
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
            })?;

    let current = current.ok_or((StatusCode::NOT_FOUND, "Schedule not found".to_string()))?;

    let next_run = CronSchedule::parse(&current.cron_expression)
        .ok()
        .and_then(|c| c.next_run_after(Utc::now()));

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
    .fetch_optional(state.db.pool())
    .await
    // Don't leak database error details
    .map_err(|e| {
        error!(error = %e, "Failed to resume schedule");
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            "Failed to resume schedule".to_string(),
        )
    })?;

    schedule
        .map(Json)
        .ok_or((StatusCode::NOT_FOUND, "Schedule not found".to_string()))
}

/// Trigger a schedule immediately
///
/// POST /api/v1/schedules/{id}/trigger
///
pub async fn trigger(
    State(state): State<AppState>,
    Extension(ctx): Extension<ApiKeyContext>,
    Path(id): Path<String>,
) -> Result<Json<TriggerResponse>, (StatusCode, String)> {
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
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "Failed to get schedule".to_string(),
                )
            })?;

    let schedule = schedule.ok_or((StatusCode::NOT_FOUND, "Schedule not found".to_string()))?;

    // Create job immediately
    let job_id = uuid::Uuid::new_v4().to_string();
    let now = Utc::now();

    sqlx::query(
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
    .await
    // Don't expose database error details
    .map_err(|e| {
        error!(error = %e, "Failed to create job from schedule trigger");
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            "Failed to create job".to_string(),
        )
    })?;

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

    info!(schedule_id = %id, job_id = %job_id, org_id = %ctx.organization_id, "Schedule triggered manually");

    // Publish to Redis to notify workers of new job
    if let Some(ref cache) = state.cache {
        let _ = cache
            .publish(
                &format!("org:{}:queue:{}", ctx.organization_id, schedule.queue_name),
                &job_id,
            )
            .await;
    }

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
    // First verify the schedule belongs to this organization
    let schedule_exists: Option<(String,)> =
        sqlx::query_as("SELECT id FROM schedules WHERE id = $1 AND organization_id = $2")
            .bind(&id)
            .bind(&ctx.organization_id)
            .fetch_optional(state.db.pool())
            .await
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    if schedule_exists.is_none() {
        return Err((StatusCode::NOT_FOUND, "Schedule not found".to_string()));
    }

    // Use safe_limit() to enforce maximum
    let runs: Vec<ScheduleRunRecord> = sqlx::query_as(
        r#"
        SELECT sr.*
        FROM schedule_runs sr
        WHERE sr.schedule_id = $1
        ORDER BY sr.started_at DESC
        LIMIT $2
        "#,
    )
    .bind(&id)
    .bind(params.safe_limit()) // Use bounded limit
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
}

impl ListSchedulesQuery {
    /// Get limit with enforced maximum to prevent memory exhaustion
    pub fn safe_limit(&self) -> i64 {
        const MAX_SCHEDULE_LIMIT: u32 = 500;
        self.limit.unwrap_or(100).min(MAX_SCHEDULE_LIMIT) as i64
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
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_list_schedules_query_defaults() {
        let query = ListSchedulesQuery {
            _organization_id: None,
            limit: None,
        };
        assert!(query._organization_id.is_none());
        assert!(query.limit.is_none());
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
