//! Workflow API handlers
//!
//! Handlers for managing job workflows and dependencies.

use axum::{
    extract::{Extension, Path, Query, State},
    Json,
};
use chrono::Utc;
use serde::Deserialize;
use std::collections::HashMap;
use tracing::{info, warn};

use crate::{
    api::{middleware::ValidatedJson, AppState},
    error::{AppError, AppResult},
    models::{
        AddDependenciesRequest, AddDependenciesResponse, ApiKeyContext, CreateWorkflowRequest,
        CreateWorkflowResponse, DependencyInfo, JobWithDependencies, Workflow, WorkflowJobMapping,
        WorkflowResponse,
    },
};

/// List workflows for organization
///
pub async fn list(
    State(state): State<AppState>,
    Extension(ctx): Extension<ApiKeyContext>,
    Query(query): Query<ListWorkflowsQuery>,
) -> AppResult<Json<Vec<WorkflowResponse>>> {
    // Use bounded limit
    let limit = query.safe_limit();
    let offset = query.offset.unwrap_or(0).max(0);

    // Use validated status
    let workflows: Vec<Workflow> = if let Some(status) = query.safe_status() {
        sqlx::query_as(
            r#"
            SELECT * FROM workflows
            WHERE organization_id = $1 AND status = $2
            ORDER BY created_at DESC
            LIMIT $3 OFFSET $4
            "#,
        )
        .bind(&ctx.organization_id)
        .bind(status)
        .bind(limit)
        .bind(offset)
        .fetch_all(state.db.pool())
        .await?
    } else {
        sqlx::query_as(
            r#"
            SELECT * FROM workflows
            WHERE organization_id = $1
            ORDER BY created_at DESC
            LIMIT $2 OFFSET $3
            "#,
        )
        .bind(&ctx.organization_id)
        .bind(limit)
        .bind(offset)
        .fetch_all(state.db.pool())
        .await?
    };

    Ok(Json(workflows.into_iter().map(|w| w.into()).collect()))
}

/// Get a single workflow
///
pub async fn get(
    State(state): State<AppState>,
    Extension(ctx): Extension<ApiKeyContext>,
    Path(workflow_id): Path<String>,
) -> AppResult<Json<WorkflowResponse>> {
    let workflow: Workflow =
        sqlx::query_as("SELECT * FROM workflows WHERE id = $1 AND organization_id = $2")
            .bind(&workflow_id)
            .bind(&ctx.organization_id)
            .fetch_optional(state.db.pool())
            .await?
            .ok_or_else(|| AppError::NotFound(format!("Workflow {} not found", workflow_id)))?;

    Ok(Json(workflow.into()))
}

/// Create a new workflow
///
/// Now uses organization context from authentication
pub async fn create(
    State(state): State<AppState>,
    Extension(ctx): Extension<ApiKeyContext>,
    ValidatedJson(request): ValidatedJson<CreateWorkflowRequest>,
) -> AppResult<Json<CreateWorkflowResponse>> {
    let workflow_id = uuid::Uuid::new_v4().to_string();
    let now = Utc::now();

    // Validate dependencies (check for cycles and missing references)
    let job_keys: std::collections::HashSet<&str> =
        request.jobs.iter().map(|j| j.key.as_str()).collect();
    for job_def in &request.jobs {
        for dep_key in &job_def.depends_on {
            if !job_keys.contains(dep_key.as_str()) {
                return Err(AppError::BadRequest(format!(
                    "Job '{}' depends on unknown job '{}'",
                    job_def.key, dep_key
                )));
            }
        }
    }

    // Check for circular dependencies (simple DFS)
    if has_circular_dependency(&request.jobs) {
        return Err(AppError::BadRequest(
            "Circular dependency detected in workflow".to_string(),
        ));
    }

    // Create the workflow
    sqlx::query(
        r#"
        INSERT INTO workflows (id, organization_id, name, description, status, total_jobs, metadata, created_at)
        VALUES ($1, $2, $3, $4, 'pending', $5, $6, $7)
        "#,
    )
    .bind(&workflow_id)
    .bind(&ctx.organization_id) // Use authenticated org context
    .bind(&request.name)
    .bind(&request.description)
    .bind(request.jobs.len() as i32)
    .bind(&request.metadata)
    .bind(now)
    .execute(state.db.pool())
    .await?;

    // Create jobs and track their IDs
    let mut job_mappings: HashMap<String, String> = HashMap::new();
    let mut response_mappings = Vec::new();

    for (step, job_def) in request.jobs.iter().enumerate() {
        let job_id = uuid::Uuid::new_v4().to_string();
        let has_dependencies = !job_def.depends_on.is_empty();

        // Create the job
        sqlx::query(
            r#"
            INSERT INTO jobs (
                id, organization_id, queue_name, status, payload, priority,
                max_retries, timeout_seconds, workflow_id, workflow_step,
                dependency_mode, dependencies_met, created_at, updated_at
            )
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $13)
            "#,
        )
        .bind(&job_id)
        .bind(&ctx.organization_id) // Use authenticated org context
        .bind(&job_def.queue_name)
        .bind("pending") // All jobs start as pending, dependencies_met controls processing
        .bind(&job_def.payload)
        .bind(job_def.priority)
        .bind(job_def.max_retries.unwrap_or(3))
        .bind(job_def.timeout_seconds.unwrap_or(300))
        .bind(&workflow_id)
        .bind(step as i32)
        .bind(job_def.dependency_mode.to_string())
        .bind(!has_dependencies) // dependencies_met = true if no dependencies
        .bind(now)
        .execute(state.db.pool())
        .await?;

        job_mappings.insert(job_def.key.clone(), job_id.clone());
        response_mappings.push(WorkflowJobMapping {
            key: job_def.key.clone(),
            job_id,
        });
    }

    // Create job dependencies
    for job_def in &request.jobs {
        let job_id = &job_mappings[&job_def.key];
        for dep_key in &job_def.depends_on {
            let dep_job_id = &job_mappings[dep_key];
            sqlx::query(
                r#"
                INSERT INTO job_dependencies (job_id, depends_on_job_id, dependency_type)
                VALUES ($1, $2, $3)
                "#,
            )
            .bind(job_id)
            .bind(dep_job_id)
            .bind(job_def.dependency_mode.to_string())
            .execute(state.db.pool())
            .await?;
        }
    }

    info!(
        workflow_id = %workflow_id,
        name = %request.name,
        jobs_count = request.jobs.len(),
        "Created workflow"
    );

    state
        .metrics
        .jobs_enqueued
        .inc_by(request.jobs.len() as u64);

    // Notify Redis about new workflow and pending jobs
    if let Some(ref cache) = state.cache {
        // Notify workflow channel
        let _ = cache
            .publish(
                &format!("org:{}:workflow:{}", ctx.organization_id, workflow_id),
                "created",
            )
            .await;

        // Notify queue channels for jobs without dependencies (ready to run)
        let queues_with_ready_jobs: std::collections::HashSet<&str> = request
            .jobs
            .iter()
            .filter(|j| j.depends_on.is_empty())
            .map(|j| j.queue_name.as_str())
            .collect();

        for queue_name in queues_with_ready_jobs {
            let _ = cache
                .publish(
                    &format!("org:{}:queue:{}", ctx.organization_id, queue_name),
                    "workflow_job_ready",
                )
                .await;
        }
    }

    Ok(Json(CreateWorkflowResponse {
        workflow_id,
        job_ids: response_mappings,
        status: "pending".to_string(),
    }))
}

/// Cancel a workflow
///
pub async fn cancel(
    State(state): State<AppState>,
    Extension(ctx): Extension<ApiKeyContext>,
    Path(workflow_id): Path<String>,
) -> AppResult<Json<WorkflowResponse>> {
    // First verify workflow belongs to this organization
    let workflow_exists: Option<(String,)> =
        sqlx::query_as("SELECT id FROM workflows WHERE id = $1 AND organization_id = $2")
            .bind(&workflow_id)
            .bind(&ctx.organization_id)
            .fetch_optional(state.db.pool())
            .await?;

    if workflow_exists.is_none() {
        return Err(AppError::NotFound(format!(
            "Workflow {} not found",
            workflow_id
        )));
    }

    // Update workflow status
    sqlx::query(
        "UPDATE workflows SET status = 'cancelled', completed_at = NOW() WHERE id = $1 AND organization_id = $2",
    )
    .bind(&workflow_id)
    .bind(&ctx.organization_id)
    .execute(state.db.pool())
    .await?;

    // Cancel only jobs belonging to this org's workflow
    // Cancel all pending/scheduled jobs in the workflow
    sqlx::query(
        r#"
        UPDATE jobs
        SET status = 'cancelled', updated_at = NOW()
        WHERE workflow_id = $1 AND organization_id = $2 AND status IN ('pending', 'scheduled')
        "#,
    )
    .bind(&workflow_id)
    .bind(&ctx.organization_id)
    .execute(state.db.pool())
    .await?;

    // Fetch and return the updated workflow
    let workflow: Workflow =
        sqlx::query_as("SELECT * FROM workflows WHERE id = $1 AND organization_id = $2")
            .bind(&workflow_id)
            .bind(&ctx.organization_id)
            .fetch_optional(state.db.pool())
            .await?
            .ok_or_else(|| AppError::NotFound(format!("Workflow {} not found", workflow_id)))?;

    info!(workflow_id = %workflow_id, org_id = %ctx.organization_id, "Cancelled workflow");

    // Notify Redis that workflow was cancelled
    if let Some(ref cache) = state.cache {
        let _ = cache
            .publish(
                &format!("org:{}:workflow:{}", ctx.organization_id, workflow_id),
                "cancelled",
            )
            .await;
    }

    Ok(Json(workflow.into()))
}

/// Get job dependencies
///
pub async fn get_job_dependencies(
    State(state): State<AppState>,
    Extension(ctx): Extension<ApiKeyContext>,
    Path(job_id): Path<String>,
) -> AppResult<Json<JobWithDependencies>> {
    // First verify job belongs to this organization
    let job_exists: Option<(String,)> =
        sqlx::query_as("SELECT id FROM jobs WHERE id = $1 AND organization_id = $2")
            .bind(&job_id)
            .bind(&ctx.organization_id)
            .fetch_optional(state.db.pool())
            .await?;

    if job_exists.is_none() {
        return Err(AppError::NotFound(format!("Job {} not found", job_id)));
    }

    // Get job's dependencies - filter by org to prevent cross-tenant leakage
    let dependencies: Vec<DependencyInfo> = sqlx::query_as(
        r#"
        SELECT j.id as job_id, j.queue_name, j.status
        FROM job_dependencies jd
        JOIN jobs j ON j.id = jd.depends_on_job_id
        WHERE jd.job_id = $1 AND j.organization_id = $2
        "#,
    )
    .bind(&job_id)
    .bind(&ctx.organization_id)
    .fetch_all(state.db.pool())
    .await?;

    // Get job's dependents - filter by org
    let dependents: Vec<DependencyInfo> = sqlx::query_as(
        r#"
        SELECT j.id as job_id, j.queue_name, j.status
        FROM job_dependencies jd
        JOIN jobs j ON j.id = jd.job_id
        WHERE jd.depends_on_job_id = $1 AND j.organization_id = $2
        "#,
    )
    .bind(&job_id)
    .bind(&ctx.organization_id)
    .fetch_all(state.db.pool())
    .await?;

    // Get dependencies_met status - already verified org ownership above
    let (dependencies_met,): (bool,) = sqlx::query_as(
        "SELECT COALESCE(dependencies_met, TRUE) FROM jobs WHERE id = $1 AND organization_id = $2",
    )
    .bind(&job_id)
    .bind(&ctx.organization_id)
    .fetch_optional(state.db.pool())
    .await?
    .unwrap_or((true,));

    Ok(Json(JobWithDependencies {
        job_id,
        dependencies,
        dependents,
        dependencies_met,
    }))
}

/// Add dependencies to an existing job
///
pub async fn add_dependencies(
    State(state): State<AppState>,
    Extension(ctx): Extension<ApiKeyContext>,
    Path(job_id): Path<String>,
    ValidatedJson(request): ValidatedJson<AddDependenciesRequest>,
) -> AppResult<Json<AddDependenciesResponse>> {
    // Verify job exists AND belongs to this organization
    let exists: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM jobs WHERE id = $1 AND organization_id = $2)",
    )
    .bind(&job_id)
    .bind(&ctx.organization_id)
    .fetch_one(state.db.pool())
    .await?;

    if !exists {
        return Err(AppError::NotFound(format!("Job {} not found", job_id)));
    }

    // Verify all dependency jobs belong to the same organization
    for dep_job_id in &request.depends_on {
        let dep_exists: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM jobs WHERE id = $1 AND organization_id = $2)",
        )
        .bind(dep_job_id)
        .bind(&ctx.organization_id)
        .fetch_one(state.db.pool())
        .await?;

        if !dep_exists {
            return Err(AppError::NotFound(format!(
                "Dependency job {} not found in your organization",
                dep_job_id
            )));
        }
    }

    // Update dependency mode
    sqlx::query("UPDATE jobs SET dependency_mode = $1, dependencies_met = FALSE WHERE id = $2")
        .bind(request.dependency_mode.to_string())
        .bind(&job_id)
        .execute(state.db.pool())
        .await?;

    // Add dependencies
    let mut added = 0;
    for dep_job_id in &request.depends_on {
        // Check for self-dependency
        if dep_job_id == &job_id {
            warn!(job_id = %job_id, "Attempted self-dependency");
            continue;
        }

        // Check for circular dependency
        if would_create_cycle(state.db.pool(), &job_id, dep_job_id).await? {
            warn!(
                job_id = %job_id,
                depends_on = %dep_job_id,
                "Circular dependency detected, skipping"
            );
            continue;
        }

        let result = sqlx::query(
            r#"
            INSERT INTO job_dependencies (job_id, depends_on_job_id, dependency_type)
            VALUES ($1, $2, $3)
            ON CONFLICT (job_id, depends_on_job_id) DO NOTHING
            "#,
        )
        .bind(&job_id)
        .bind(dep_job_id)
        .bind(request.dependency_mode.to_string())
        .execute(state.db.pool())
        .await?;

        added += result.rows_affected() as i32;
    }

    // Check if dependencies are met
    let dependencies_met: bool = sqlx::query_scalar("SELECT check_job_dependencies_met($1)")
        .bind(&job_id)
        .fetch_one(state.db.pool())
        .await
        .unwrap_or(true);

    // Update job's dependencies_met flag
    sqlx::query("UPDATE jobs SET dependencies_met = $1 WHERE id = $2")
        .bind(dependencies_met)
        .bind(&job_id)
        .execute(state.db.pool())
        .await?;

    Ok(Json(AddDependenciesResponse {
        dependencies_added: added,
        dependencies_met,
    }))
}

/// Valid workflow statuses for filtering
const VALID_WORKFLOW_STATUSES: &[&str] =
    &["pending", "running", "completed", "failed", "cancelled"];

/// Maximum workflows per page
const MAX_WORKFLOWS_PER_PAGE: i64 = 100;

/// Query parameters for listing workflows
///
#[derive(Debug, Deserialize)]
pub struct ListWorkflowsQuery {
    pub status: Option<String>,
    pub limit: Option<i64>,
    pub offset: Option<i64>,
}

impl ListWorkflowsQuery {
    /// Validate and return safe status
    pub fn safe_status(&self) -> Option<&str> {
        self.status.as_ref().and_then(|s| {
            if VALID_WORKFLOW_STATUSES.contains(&s.as_str()) {
                Some(s.as_str())
            } else {
                tracing::warn!(status = %s, "Invalid workflow status filter ignored");
                None
            }
        })
    }

    /// Return bounded limit
    pub fn safe_limit(&self) -> i64 {
        self.limit.unwrap_or(50).clamp(1, MAX_WORKFLOWS_PER_PAGE)
    }
}

/// Check if adding a dependency would create a cycle
async fn would_create_cycle(
    pool: &sqlx::PgPool,
    job_id: &str,
    depends_on_job_id: &str,
) -> Result<bool, AppError> {
    // Check if depends_on_job_id transitively depends on job_id
    let has_cycle: bool = sqlx::query_scalar(
        r#"
        WITH RECURSIVE dep_chain AS (
            SELECT depends_on_job_id
            FROM job_dependencies
            WHERE job_id = $1
            
            UNION
            
            SELECT jd.depends_on_job_id
            FROM job_dependencies jd
            JOIN dep_chain dc ON dc.depends_on_job_id = jd.job_id
        )
        SELECT EXISTS(SELECT 1 FROM dep_chain WHERE depends_on_job_id = $2)
        "#,
    )
    .bind(depends_on_job_id)
    .bind(job_id)
    .fetch_one(pool)
    .await?;

    Ok(has_cycle)
}

/// Check for circular dependencies in workflow definition
fn has_circular_dependency(jobs: &[crate::models::WorkflowJobDefinition]) -> bool {
    use std::collections::HashSet;

    // Build adjacency list
    let job_deps: HashMap<String, Vec<String>> = jobs
        .iter()
        .map(|j| (j.key.clone(), j.depends_on.clone()))
        .collect();

    fn visit(
        key: &str,
        job_deps: &HashMap<String, Vec<String>>,
        visiting: &mut HashSet<String>,
        visited: &mut HashSet<String>,
    ) -> bool {
        if visited.contains(key) {
            return false;
        }
        if visiting.contains(key) {
            return true; // Cycle detected
        }

        visiting.insert(key.to_string());

        if let Some(deps) = job_deps.get(key) {
            for dep in deps {
                if visit(dep, job_deps, visiting, visited) {
                    return true;
                }
            }
        }

        visiting.remove(key);
        visited.insert(key.to_string());
        false
    }

    let mut visiting = HashSet::new();
    let mut visited = HashSet::new();

    for key in job_deps.keys() {
        if visit(key, &job_deps, &mut visiting, &mut visited) {
            return true;
        }
    }

    false
}

// Manual implementation for DependencyInfo
impl<'r> sqlx::FromRow<'r, sqlx::postgres::PgRow> for DependencyInfo {
    fn from_row(row: &'r sqlx::postgres::PgRow) -> Result<Self, sqlx::Error> {
        use sqlx::Row;
        Ok(DependencyInfo {
            job_id: row.try_get("job_id")?,
            queue_name: row.try_get("queue_name")?,
            status: row.try_get("status")?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{DependencyMode, WorkflowJobDefinition};

    #[test]
    fn test_circular_dependency_detection_none() {
        let jobs = vec![
            WorkflowJobDefinition {
                key: "job1".to_string(),
                queue_name: "q".to_string(),
                payload: serde_json::json!({}),
                depends_on: vec![],
                dependency_mode: DependencyMode::All,
                priority: 0,
                max_retries: None,
                timeout_seconds: None,
            },
            WorkflowJobDefinition {
                key: "job2".to_string(),
                queue_name: "q".to_string(),
                payload: serde_json::json!({}),
                depends_on: vec!["job1".to_string()],
                dependency_mode: DependencyMode::All,
                priority: 0,
                max_retries: None,
                timeout_seconds: None,
            },
        ];

        assert!(!has_circular_dependency(&jobs));
    }

    #[test]
    fn test_circular_dependency_detection_simple() {
        let jobs = vec![
            WorkflowJobDefinition {
                key: "job1".to_string(),
                queue_name: "q".to_string(),
                payload: serde_json::json!({}),
                depends_on: vec!["job2".to_string()],
                dependency_mode: DependencyMode::All,
                priority: 0,
                max_retries: None,
                timeout_seconds: None,
            },
            WorkflowJobDefinition {
                key: "job2".to_string(),
                queue_name: "q".to_string(),
                payload: serde_json::json!({}),
                depends_on: vec!["job1".to_string()],
                dependency_mode: DependencyMode::All,
                priority: 0,
                max_retries: None,
                timeout_seconds: None,
            },
        ];

        assert!(has_circular_dependency(&jobs));
    }

    #[test]
    fn test_circular_dependency_detection_transitive() {
        let jobs = vec![
            WorkflowJobDefinition {
                key: "job1".to_string(),
                queue_name: "q".to_string(),
                payload: serde_json::json!({}),
                depends_on: vec!["job3".to_string()],
                dependency_mode: DependencyMode::All,
                priority: 0,
                max_retries: None,
                timeout_seconds: None,
            },
            WorkflowJobDefinition {
                key: "job2".to_string(),
                queue_name: "q".to_string(),
                payload: serde_json::json!({}),
                depends_on: vec!["job1".to_string()],
                dependency_mode: DependencyMode::All,
                priority: 0,
                max_retries: None,
                timeout_seconds: None,
            },
            WorkflowJobDefinition {
                key: "job3".to_string(),
                queue_name: "q".to_string(),
                payload: serde_json::json!({}),
                depends_on: vec!["job2".to_string()],
                dependency_mode: DependencyMode::All,
                priority: 0,
                max_retries: None,
                timeout_seconds: None,
            },
        ];

        assert!(has_circular_dependency(&jobs));
    }

    #[test]
    fn test_circular_dependency_detection_diamond() {
        // Diamond dependency (not circular):
        // A
        // / \
        // B   C
        // \ /
        // D
        let jobs = vec![
            WorkflowJobDefinition {
                key: "A".to_string(),
                queue_name: "q".to_string(),
                payload: serde_json::json!({}),
                depends_on: vec![],
                dependency_mode: DependencyMode::All,
                priority: 0,
                max_retries: None,
                timeout_seconds: None,
            },
            WorkflowJobDefinition {
                key: "B".to_string(),
                queue_name: "q".to_string(),
                payload: serde_json::json!({}),
                depends_on: vec!["A".to_string()],
                dependency_mode: DependencyMode::All,
                priority: 0,
                max_retries: None,
                timeout_seconds: None,
            },
            WorkflowJobDefinition {
                key: "C".to_string(),
                queue_name: "q".to_string(),
                payload: serde_json::json!({}),
                depends_on: vec!["A".to_string()],
                dependency_mode: DependencyMode::All,
                priority: 0,
                max_retries: None,
                timeout_seconds: None,
            },
            WorkflowJobDefinition {
                key: "D".to_string(),
                queue_name: "q".to_string(),
                payload: serde_json::json!({}),
                depends_on: vec!["B".to_string(), "C".to_string()],
                dependency_mode: DependencyMode::All,
                priority: 0,
                max_retries: None,
                timeout_seconds: None,
            },
        ];

        assert!(!has_circular_dependency(&jobs));
    }
}
