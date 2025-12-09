//! Health check handlers

use axum::{
    extract::{Extension, State},
    http::StatusCode,
    Json,
};
use chrono::{DateTime, Utc};
use serde::Serialize;
use std::time::Instant;

use crate::api::AppState;
use crate::models::ApiKeyContext;

// Track server start time
lazy_static::lazy_static! {
    static ref SERVER_START_TIME: Instant = Instant::now();
    static ref SERVER_START_UTC: DateTime<Utc> = Utc::now();
}

/// Health check response
///
/// Version field removed from unauthenticated response
/// to prevent information disclosure for reconnaissance
#[derive(Serialize)]
pub struct HealthResponse {
    pub status: String,
    /// Version only shown in authenticated contexts
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    pub database: bool,
    pub cache: bool,
}

/// Basic health check endpoint
///
/// No longer exposes server version to unauthenticated users.
/// Version info could be used by attackers to find known vulnerabilities.
pub async fn health_check(State(state): State<AppState>) -> Json<HealthResponse> {
    let db_healthy = state.db.health_check().await.unwrap_or(false);
    let cache_healthy = if let Some(ref cache) = state.cache {
        cache.health_check().await.unwrap_or(false)
    } else {
        true // Cache is optional
    };

    // Don't expose version to unauthenticated users
    // Version is only shown in dashboard_data which requires authentication
    Json(HealthResponse {
        status: if db_healthy { "healthy" } else { "degraded" }.to_string(),
        version: None, // Hidden from unauthenticated endpoint
        database: db_healthy,
        cache: cache_healthy,
    })
}

/// Kubernetes liveness probe
pub async fn liveness() -> StatusCode {
    StatusCode::OK
}

/// Kubernetes readiness probe
pub async fn readiness(State(state): State<AppState>) -> StatusCode {
    match state.db.health_check().await {
        Ok(true) => StatusCode::OK,
        _ => StatusCode::SERVICE_UNAVAILABLE,
    }
}

/// Dashboard data response
#[derive(Serialize)]
pub struct DashboardData {
    /// System information
    pub system: SystemInfo,
    /// Job statistics
    pub jobs: JobSummaryStats,
    /// Queue summary
    pub queues: Vec<QueueSummary>,
    /// Worker summary
    pub workers: WorkerSummary,
    /// Recent activity
    pub recent_activity: RecentActivity,
}

/// System information
#[derive(Serialize)]
pub struct SystemInfo {
    pub version: String,
    pub uptime_seconds: u64,
    pub started_at: DateTime<Utc>,
    pub database_status: String,
    pub cache_status: String,
    pub environment: String,
}

/// Job summary statistics
#[derive(Serialize)]
pub struct JobSummaryStats {
    pub total: i64,
    pub pending: i64,
    pub processing: i64,
    pub completed_24h: i64,
    pub failed_24h: i64,
    pub deadletter: i64,
    pub avg_wait_time_ms: Option<f64>,
    pub avg_processing_time_ms: Option<f64>,
}

/// Queue summary
#[derive(Serialize)]
pub struct QueueSummary {
    pub name: String,
    pub pending: i64,
    pub processing: i64,
    pub paused: bool,
}

/// Worker summary
#[derive(Serialize)]
pub struct WorkerSummary {
    pub total: i64,
    pub healthy: i64,
    pub unhealthy: i64,
}

/// Recent activity stats
#[derive(Serialize)]
pub struct RecentActivity {
    pub jobs_created_1h: i64,
    pub jobs_completed_1h: i64,
    pub jobs_failed_1h: i64,
}

/// Dashboard data endpoint for UI
///
/// Previously leaked data from ALL organizations - critical privacy violation
pub async fn dashboard_data(
    State(state): State<AppState>,
    Extension(ctx): Extension<ApiKeyContext>,
) -> Json<DashboardData> {
    let org_id = &ctx.organization_id;

    // Get system info
    let db_healthy = state.db.health_check().await.unwrap_or(false);
    let cache_healthy = if let Some(ref cache) = state.cache {
        cache.health_check().await.unwrap_or(false)
    } else {
        true
    };

    let system = SystemInfo {
        version: env!("CARGO_PKG_VERSION").to_string(),
        uptime_seconds: SERVER_START_TIME.elapsed().as_secs(),
        started_at: *SERVER_START_UTC,
        database_status: if db_healthy { "healthy" } else { "unhealthy" }.to_string(),
        cache_status: if cache_healthy {
            "healthy"
        } else {
            "unhealthy"
        }
        .to_string(),
        environment: state.settings.server.environment.to_string(),
    };

    // Get job stats filtered by organization
    let job_stats: (i64, i64, i64, i64) = sqlx::query_as(
        r#"
        SELECT 
            COUNT(*) FILTER (WHERE status = 'pending') as pending,
            COUNT(*) FILTER (WHERE status = 'processing') as processing,
            COUNT(*) FILTER (WHERE status = 'completed' AND completed_at > NOW() - INTERVAL '24 hours') as completed_24h,
            COUNT(*) FILTER (WHERE status IN ('failed', 'deadletter') AND updated_at > NOW() - INTERVAL '24 hours') as failed_24h
        FROM jobs
        WHERE organization_id = $1
        "#
    )
    .bind(org_id)
    .fetch_one(state.db.pool())
    .await
    .unwrap_or((0, 0, 0, 0));

    let (total_jobs,): (i64,) =
        sqlx::query_as("SELECT COUNT(*) FROM jobs WHERE organization_id = $1")
            .bind(org_id)
            .fetch_one(state.db.pool())
            .await
            .unwrap_or((0,));

    let (deadletter_count,): (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM jobs WHERE status = 'deadletter' AND organization_id = $1",
    )
    .bind(org_id)
    .fetch_one(state.db.pool())
    .await
    .unwrap_or((0,));

    // Calculate average wait time (time from created to started) and processing time (started to completed)
    // Only consider jobs completed in the last 24 hours for relevant metrics
    let timing_stats: (Option<f64>, Option<f64>) = sqlx::query_as(
        r#"
        SELECT 
            AVG(EXTRACT(EPOCH FROM (started_at - created_at)) * 1000)::FLOAT8 as avg_wait_ms,
            AVG(EXTRACT(EPOCH FROM (completed_at - started_at)) * 1000)::FLOAT8 as avg_processing_ms
        FROM jobs
        WHERE organization_id = $1
          AND status = 'completed'
          AND completed_at > NOW() - INTERVAL '24 hours'
          AND started_at IS NOT NULL
          AND completed_at IS NOT NULL
        "#,
    )
    .bind(org_id)
    .fetch_one(state.db.pool())
    .await
    .unwrap_or((None, None));

    let jobs = JobSummaryStats {
        total: total_jobs,
        pending: job_stats.0,
        processing: job_stats.1,
        completed_24h: job_stats.2,
        failed_24h: job_stats.3,
        deadletter: deadletter_count,
        avg_wait_time_ms: timing_stats.0,
        avg_processing_time_ms: timing_stats.1,
    };

    // Get queue summary filtered by organization
    // Now includes pause status from queue_config
    let queues: Vec<QueueSummary> = sqlx::query_as(
        r#"
        SELECT 
            j.queue_name as name,
            COUNT(*) FILTER (WHERE j.status = 'pending') as pending,
            COUNT(*) FILTER (WHERE j.status = 'processing') as processing,
            COALESCE(qc.enabled, TRUE) as enabled
        FROM jobs j
        LEFT JOIN queue_config qc ON qc.queue_name = j.queue_name AND qc.organization_id = j.organization_id
        WHERE j.organization_id = $1
        GROUP BY j.queue_name, qc.enabled
        ORDER BY j.queue_name
        LIMIT 20
        "#
    )
    .bind(org_id)
    .fetch_all(state.db.pool())
    .await
    .unwrap_or_default()
    .into_iter()
    .map(|(name, pending, processing, enabled): (String, i64, i64, bool)| QueueSummary {
        name,
        pending,
        processing,
        paused: !enabled, // Paused = not enabled
    })
    .collect();

    // Get worker summary filtered by organization
    let worker_stats: (i64, i64) = sqlx::query_as(
        r#"
        SELECT 
            COUNT(*) as total,
            COUNT(*) FILTER (WHERE status = 'healthy') as healthy
        FROM workers
        WHERE last_heartbeat > NOW() - INTERVAL '1 minute'
          AND organization_id = $1
        "#,
    )
    .bind(org_id)
    .fetch_one(state.db.pool())
    .await
    .unwrap_or((0, 0));

    let workers = WorkerSummary {
        total: worker_stats.0,
        healthy: worker_stats.1,
        unhealthy: worker_stats.0 - worker_stats.1,
    };

    // Get recent activity filtered by organization
    let recent: (i64, i64, i64) = sqlx::query_as(
        r#"
        SELECT 
            COUNT(*) FILTER (WHERE created_at > NOW() - INTERVAL '1 hour') as created,
            COUNT(*) FILTER (WHERE completed_at > NOW() - INTERVAL '1 hour' AND status = 'completed') as completed,
            COUNT(*) FILTER (WHERE updated_at > NOW() - INTERVAL '1 hour' AND status IN ('failed', 'deadletter')) as failed
        FROM jobs
        WHERE organization_id = $1
        "#
    )
    .bind(org_id)
    .fetch_one(state.db.pool())
    .await
    .unwrap_or((0, 0, 0));

    let recent_activity = RecentActivity {
        jobs_created_1h: recent.0,
        jobs_completed_1h: recent.1,
        jobs_failed_1h: recent.2,
    };

    Json(DashboardData {
        system,
        jobs,
        queues,
        workers,
        recent_activity,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Updated test to use correct struct with Option<String> version
    #[test]
    fn test_health_response_serialization() {
        let response = HealthResponse {
            status: "healthy".to_string(),
            version: Some("0.1.0".to_string()), // Now Option<String>
            database: true,
            cache: true,
        };

        let json = serde_json::to_string(&response).unwrap();
        assert!(json.contains("healthy"));
        assert!(json.contains("database"));
    }

    /// Test that version is omitted when None
    #[test]
    fn test_health_response_without_version() {
        let response = HealthResponse {
            status: "healthy".to_string(),
            version: None,
            database: true,
            cache: true,
        };

        let json = serde_json::to_string(&response).unwrap();
        assert!(json.contains("healthy"));
        assert!(!json.contains("version")); // Should be omitted
    }
}
