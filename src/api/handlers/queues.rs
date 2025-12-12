//! Queue handlers

use axum::{
    extract::{Extension, Path, State},
    http::StatusCode,
    Json,
};
use chrono::Utc;

use crate::api::AppState;
use crate::error::{AppError, AppResult};
use crate::models::{
    ApiKeyContext, PauseQueueRequest, PauseQueueResponse, QueueConfig, QueueConfigSummary,
    QueueStats, ResumeQueueResponse, UpsertQueueConfigRequest,
};

/// Maximum queues per page
const MAX_QUEUES_PER_PAGE: i64 = 100;

/// Maximum allowed rate limit value
const MAX_RATE_LIMIT: i32 = 100000;

/// List all queues with their configurations
///
/// Now filters by authenticated organization
/// Now uses configurable limit constant
pub async fn list(
    State(state): State<AppState>,
    Extension(ctx): Extension<ApiKeyContext>,
) -> AppResult<Json<Vec<QueueConfigSummary>>> {
    // Use constant instead of hardcoded value
    let configs = sqlx::query_as::<_, QueueConfig>(
        "SELECT * FROM queue_config WHERE organization_id = $1 ORDER BY queue_name ASC LIMIT $2",
    )
    .bind(&ctx.organization_id)
    .bind(MAX_QUEUES_PER_PAGE)
    .fetch_all(state.db.pool())
    .await?;

    let summaries: Vec<QueueConfigSummary> = configs.into_iter().map(Into::into).collect();
    Ok(Json(summaries))
}

/// Validate queue name for safe characters
fn validate_queue_name_param(name: &str) -> Result<(), AppError> {
    if name.is_empty() || name.len() > 100 {
        return Err(AppError::BadRequest(
            "Invalid queue name length".to_string(),
        ));
    }
    if !name
        .chars()
        .all(|c| c.is_alphanumeric() || c == '-' || c == '_' || c == '.')
    {
        return Err(AppError::BadRequest(
            "Queue name contains invalid characters".to_string(),
        ));
    }
    Ok(())
}

/// Get queue configuration by name
///
/// Now checks organization isolation
/// Now returns generic "Queue not found" without exposing name
/// Now validates queue name parameter
pub async fn get(
    State(state): State<AppState>,
    Extension(ctx): Extension<ApiKeyContext>,
    Path(name): Path<String>,
) -> AppResult<Json<QueueConfig>> {
    // Validate queue name
    validate_queue_name_param(&name)?;

    let config = sqlx::query_as::<_, QueueConfig>(
        "SELECT * FROM queue_config WHERE queue_name = $1 AND organization_id = $2",
    )
    .bind(&name)
    .bind(&ctx.organization_id)
    .fetch_optional(state.db.pool())
    .await?
    // Don't expose queue name in error
    .ok_or_else(|| AppError::NotFound("Queue not found".to_string()))?;

    Ok(Json(config))
}

/// Update or create queue configuration
///
/// Now uses authenticated organization context
/// Now validates rate_limit bounds
pub async fn update_config(
    State(state): State<AppState>,
    Extension(ctx): Extension<ApiKeyContext>,
    Path(name): Path<String>,
    Json(request): Json<UpsertQueueConfigRequest>,
) -> AppResult<Json<QueueConfig>> {
    let max_retries = request
        .max_retries
        .unwrap_or(state.settings.queue.default_max_retries);
    let default_timeout = request
        .default_timeout
        .unwrap_or(state.settings.queue.default_timeout_secs);
    let enabled = request.enabled.unwrap_or(true);

    // Validate rate_limit if provided
    let rate_limit = request.rate_limit.map(|rl| {
        if rl < 0 {
            tracing::warn!(rate_limit = rl, queue = %name, "Invalid negative rate limit, using 0");
            0
        } else if rl > MAX_RATE_LIMIT {
            tracing::warn!(rate_limit = rl, max = MAX_RATE_LIMIT, queue = %name, "Rate limit exceeds maximum, capping");
            MAX_RATE_LIMIT
        } else {
            rl
        }
    });

    let config = sqlx::query_as::<_, QueueConfig>(
        r#"
        INSERT INTO queue_config (
            id, organization_id, queue_name, max_retries, default_timeout,
            rate_limit, enabled, settings, created_at, updated_at
        )
        VALUES (gen_random_uuid()::TEXT, $1, $2, $3, $4, $5, $6, $7, NOW(), NOW())
        ON CONFLICT (organization_id, queue_name)
        DO UPDATE SET
            max_retries = EXCLUDED.max_retries,
            default_timeout = EXCLUDED.default_timeout,
            rate_limit = EXCLUDED.rate_limit,
            enabled = EXCLUDED.enabled,
            settings = COALESCE(EXCLUDED.settings, queue_config.settings),
            updated_at = NOW()
        RETURNING *
        "#,
    )
    .bind(&ctx.organization_id) // Use authenticated org context
    .bind(&name)
    .bind(max_retries)
    .bind(default_timeout)
    .bind(rate_limit) // Use validated rate_limit
    .bind(enabled)
    .bind(request.settings.unwrap_or(serde_json::json!({})))
    .fetch_one(state.db.pool())
    .await?;

    Ok(Json(config))
}

/// Get queue statistics
///
pub async fn stats(
    State(state): State<AppState>,
    Extension(ctx): Extension<ApiKeyContext>,
    Path(name): Path<String>,
) -> AppResult<Json<QueueStats>> {
    // Validate queue name
    validate_queue_name_param(&name)?;

    // Check both legacy queue_name AND queue_names array for accurate worker count
    let stats = sqlx::query_as::<_, (i64, i64, i64, i64, Option<f64>, Option<i64>, i64)>(
        r#"
        SELECT 
            COUNT(*) FILTER (WHERE status = 'pending') as pending_jobs,
            COUNT(*) FILTER (WHERE status = 'processing') as processing_jobs,
            COUNT(*) FILTER (WHERE status = 'completed' AND completed_at > NOW() - INTERVAL '24 hours') as completed_24h,
            COUNT(*) FILTER (WHERE status = 'failed' AND completed_at > NOW() - INTERVAL '24 hours') as failed_24h,
            AVG(EXTRACT(EPOCH FROM (completed_at - started_at)) * 1000) FILTER (WHERE status = 'completed' AND completed_at IS NOT NULL AND started_at IS NOT NULL) as avg_processing_time_ms,
            EXTRACT(EPOCH FROM (NOW() - MIN(created_at)))::BIGINT FILTER (WHERE status IN ('pending', 'scheduled')) as max_job_age_seconds,
            (SELECT COUNT(*) FROM workers WHERE (queue_name = $1 OR $1 = ANY(queue_names)) AND organization_id = $2 AND status = 'healthy') as active_workers
        FROM jobs
        WHERE queue_name = $1 AND organization_id = $2
        "#,
    )
    .bind(&name)
    .bind(&ctx.organization_id)
    .fetch_one(state.db.pool())
    .await?;

    Ok(Json(QueueStats {
        queue_name: name,
        pending_jobs: stats.0,
        processing_jobs: stats.1,
        completed_jobs_24h: stats.2,
        failed_jobs_24h: stats.3,
        avg_processing_time_ms: stats.4,
        max_job_age_seconds: stats.5,
        active_workers: stats.6,
    }))
}

/// Pause a queue (stops job processing)
///
pub async fn pause(
    State(state): State<AppState>,
    Extension(ctx): Extension<ApiKeyContext>,
    Path(name): Path<String>,
    Json(request): Json<PauseQueueRequest>,
) -> AppResult<Json<PauseQueueResponse>> {
    let now = Utc::now();

    // Upsert queue config with enabled=false and paused metadata
    let paused_settings = serde_json::json!({
        "paused": true,
        "paused_at": now.to_rfc3339(),
        "paused_reason": request.reason
    });

    let result = sqlx::query(
        r#"
        INSERT INTO queue_config (
            id, organization_id, queue_name, max_retries, default_timeout,
            enabled, settings, created_at, updated_at
        )
        VALUES (gen_random_uuid()::TEXT, $1, $2, 3, 300, false, $3, NOW(), NOW())
        ON CONFLICT (organization_id, queue_name)
        DO UPDATE SET
            enabled = false,
            settings = queue_config.settings || $3,
            updated_at = NOW()
        "#,
    )
    .bind(&ctx.organization_id)
    .bind(&name)
    .bind(&paused_settings)
    .execute(state.db.pool())
    .await?;

    if result.rows_affected() == 0 {
        return Err(AppError::Internal("Failed to pause queue".to_string()));
    }

    // Publish pause notification with org context to prevent cross-tenant leakage
    // Previously published to queue:<name>:control which could be read by any org with same queue name
    if let Some(ref cache) = state.cache {
        let _ = cache
            .publish(
                &format!("org:{}:queue:{}:control", ctx.organization_id, name),
                "paused",
            )
            .await;
    }

    Ok(Json(PauseQueueResponse {
        queue_name: name,
        paused: true,
        paused_at: now,
        reason: request.reason,
    }))
}

/// Resume a paused queue
///
pub async fn resume(
    State(state): State<AppState>,
    Extension(ctx): Extension<ApiKeyContext>,
    Path(name): Path<String>,
) -> AppResult<Json<ResumeQueueResponse>> {
    // Get current pause state
    let config: Option<(serde_json::Value,)> = sqlx::query_as(
        "SELECT settings FROM queue_config WHERE queue_name = $1 AND organization_id = $2",
    )
    .bind(&name)
    .bind(&ctx.organization_id)
    .fetch_optional(state.db.pool())
    .await?;

    let paused_at = config
        .as_ref()
        .and_then(|(settings,)| settings.get("paused_at"))
        .and_then(|v| v.as_str())
        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
        .map(|dt| dt.with_timezone(&Utc));

    let paused_duration_secs = paused_at
        .map(|at| (Utc::now() - at).num_seconds())
        .unwrap_or(0);

    // Resume queue
    let result = sqlx::query(
        r#"
        UPDATE queue_config
        SET 
            enabled = true,
            settings = settings - 'paused' - 'paused_at' - 'paused_reason',
            updated_at = NOW()
        WHERE queue_name = $1 AND organization_id = $2
        "#,
    )
    .bind(&name)
    .bind(&ctx.organization_id)
    .execute(state.db.pool())
    .await?;

    if result.rows_affected() == 0 {
        // Don't expose queue name in error
        return Err(AppError::NotFound("Queue not found".to_string()));
    }

    // Publish resume notification with org context to prevent cross-tenant leakage
    if let Some(ref cache) = state.cache {
        let _ = cache
            .publish(
                &format!("org:{}:queue:{}:control", ctx.organization_id, name),
                "resumed",
            )
            .await;
    }

    Ok(Json(ResumeQueueResponse {
        queue_name: name,
        resumed: true,
        paused_duration_secs,
    }))
}

/// Delete queue configuration
///
pub async fn delete(
    State(state): State<AppState>,
    Extension(ctx): Extension<ApiKeyContext>,
    Path(name): Path<String>,
) -> AppResult<StatusCode> {
    // Check if queue has pending jobs
    let (pending_count,): (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM jobs WHERE queue_name = $1 AND organization_id = $2 AND status IN ('pending', 'processing')"
    )
    .bind(&name)
    .bind(&ctx.organization_id)
    .fetch_one(state.db.pool())
    .await?;

    if pending_count > 0 {
        return Err(AppError::Conflict(format!(
            "Cannot delete queue {} with {} pending/processing jobs",
            name, pending_count
        )));
    }

    let result =
        sqlx::query("DELETE FROM queue_config WHERE queue_name = $1 AND organization_id = $2")
            .bind(&name)
            .bind(&ctx.organization_id)
            .execute(state.db.pool())
            .await?;

    if result.rows_affected() == 0 {
        // Don't expose queue name in error
        return Err(AppError::NotFound("Queue not found".to_string()));
    }

    // Invalidate cache for deleted queue
    if let Some(ref cache) = state.cache {
        let cache_key = format!("org:{}:queue_config:{}", ctx.organization_id, name);
        if let Err(e) = cache.delete(&cache_key).await {
            tracing::warn!(error = %e, "Failed to invalidate queue config cache");
        }
    }

    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_queue_stats_serialization() {
        let stats = QueueStats {
            queue_name: "default".to_string(),
            pending_jobs: 10,
            processing_jobs: 5,
            completed_jobs_24h: 100,
            failed_jobs_24h: 2,
            avg_processing_time_ms: Some(150.5),
            max_job_age_seconds: Some(30),
            active_workers: 3,
        };

        let json = serde_json::to_string(&stats).unwrap();
        assert!(json.contains("default"));
        assert!(json.contains("pending_jobs"));
    }
}
