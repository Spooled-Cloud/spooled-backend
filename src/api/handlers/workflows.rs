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
    api::{
        middleware::{
            limits::{
                check_job_limits, check_payload_size, check_resource_limit_conn, lock_resource,
            },
            ValidatedJson,
        },
        AppState,
    },
    error::{AppError, AppResult},
    models::{
        AddDependenciesRequest, AddDependenciesResponse, ApiKeyContext, CreateWorkflowRequest,
        CreateWorkflowResponse, DependencyInfo, Job, JobErrorResponse, JobWithDependencies,
        Workflow, WorkflowDependencyResponse, WorkflowDetailResponse, WorkflowJobMapping,
        WorkflowJobResponse, WorkflowProgress, WorkflowResponse,
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

/// Get a single workflow with jobs and dependencies
///
pub async fn get(
    State(state): State<AppState>,
    Extension(ctx): Extension<ApiKeyContext>,
    Path(workflow_id): Path<String>,
) -> AppResult<Json<WorkflowDetailResponse>> {
    // Fetch the workflow
    let workflow: Workflow =
        sqlx::query_as("SELECT * FROM workflows WHERE id = $1 AND organization_id = $2")
            .bind(&workflow_id)
            .bind(&ctx.organization_id)
            .fetch_optional(state.db.pool())
            .await?
            .ok_or_else(|| AppError::NotFound(format!("Workflow {} not found", workflow_id)))?;

    // Fetch all jobs belonging to this workflow
    let jobs: Vec<Job> = sqlx::query_as(
        r#"
        SELECT * FROM jobs 
        WHERE workflow_id = $1 AND organization_id = $2
        ORDER BY workflow_step ASC, created_at ASC
        "#,
    )
    .bind(&workflow_id)
    .bind(&ctx.organization_id)
    .fetch_all(state.db.pool())
    .await?;

    // Fetch all dependencies for jobs in this workflow
    let job_ids: Vec<String> = jobs.iter().map(|j| j.id.clone()).collect();
    let dependencies: Vec<WorkflowDependencyRow> = if !job_ids.is_empty() {
        sqlx::query_as(
            r#"
            SELECT jd.depends_on_job_id as parent_job_id, jd.job_id as child_job_id, jd.dependency_type
            FROM job_dependencies jd
            WHERE jd.job_id = ANY($1)
            "#,
        )
        .bind(&job_ids)
        .fetch_all(state.db.pool())
        .await?
    } else {
        vec![]
    };

    // Calculate progress
    let mut pending = 0;
    let mut processing = 0;
    let mut completed = 0;
    let mut failed = 0;
    for job in &jobs {
        match job.status.as_str() {
            "pending" | "scheduled" => pending += 1,
            "processing" => processing += 1,
            "completed" => completed += 1,
            "failed" | "deadletter" | "cancelled" => failed += 1,
            _ => pending += 1,
        }
    }

    // Convert jobs to response format
    let job_responses: Vec<WorkflowJobResponse> = jobs
        .into_iter()
        .map(|job| {
            let error = job.last_error.as_ref().map(|msg| JobErrorResponse {
                error_type: "JobError".to_string(),
                message: msg.clone(),
                stack: None,
            });

            WorkflowJobResponse {
                id: job.id,
                organization_id: job.organization_id,
                queue: job.queue_name,
                job_type: "job".to_string(), // jobs table doesn't have job_type, use default
                payload: job.payload,
                status: job.status,
                priority: job.priority,
                attempt: job.retry_count,
                max_retries: job.max_retries,
                backoff_type: "exponential".to_string(), // default
                timeout_ms: Some((job.timeout_seconds * 1000) as i64),
                created_at: job.created_at,
                scheduled_at: job.scheduled_at,
                started_at: job.started_at,
                completed_at: job.completed_at,
                failed_at: None,     // jobs table doesn't track this separately
                next_retry_at: None, // would need to calculate from retry logic
                result: job.result,
                error,
                metadata: job.tags,
                workflow_id: job.workflow_id,
                parent_job_id: job.parent_job_id,
            }
        })
        .collect();

    // Convert dependencies to response format
    let dependency_responses: Vec<WorkflowDependencyResponse> = dependencies
        .into_iter()
        .map(|d| WorkflowDependencyResponse {
            parent_job_id: d.parent_job_id,
            child_job_id: d.child_job_id,
            dependency_type: d.dependency_type,
        })
        .collect();

    let response = WorkflowDetailResponse {
        id: workflow.id,
        organization_id: workflow.organization_id,
        name: workflow.name,
        description: workflow.description,
        status: workflow.status,
        jobs: job_responses,
        dependencies: dependency_responses,
        progress: WorkflowProgress {
            total: workflow.total_jobs,
            completed,
            failed,
            pending,
            processing,
        },
        created_at: workflow.created_at,
        started_at: workflow.started_at,
        completed_at: workflow.completed_at,
        metadata: workflow.metadata,
    };

    Ok(Json(response))
}

/// Helper struct for fetching dependencies
#[derive(Debug)]
struct WorkflowDependencyRow {
    parent_job_id: String,
    child_job_id: String,
    dependency_type: String,
}

impl<'r> sqlx::FromRow<'r, sqlx::postgres::PgRow> for WorkflowDependencyRow {
    fn from_row(row: &'r sqlx::postgres::PgRow) -> Result<Self, sqlx::Error> {
        use sqlx::Row;
        Ok(WorkflowDependencyRow {
            parent_job_id: row.try_get("parent_job_id")?,
            child_job_id: row.try_get("child_job_id")?,
            dependency_type: row.try_get("dependency_type")?,
        })
    }
}

/// Create a new workflow
///
/// Now uses organization context from authentication
/// SECURITY: Enforces plan limits before creating workflow jobs
pub async fn create(
    State(state): State<AppState>,
    Extension(ctx): Extension<ApiKeyContext>,
    ValidatedJson(request): ValidatedJson<CreateWorkflowRequest>,
) -> AppResult<Json<CreateWorkflowResponse>> {
    let workflow_id = uuid::Uuid::new_v4().to_string();
    let now = Utc::now();

    // (The authoritative workflows-count check runs inside the transaction below
    // under an advisory lock so concurrent creates can't overshoot the cap.)

    // Check job limits (daily + active) before creating workflow jobs
    let job_count = request.jobs.len() as u64;
    if let Err(response) = check_job_limits(state.db.pool(), &ctx.organization_id, job_count).await
    {
        return Err(AppError::LimitExceeded(Box::new(response)));
    }

    // Enforce plan payload size for workflow jobs too (plan overrides + custom_limits).
    let mut max_payload_size: usize = 0;
    for job_def in &request.jobs {
        let payload_json = serde_json::to_string(&job_def.payload).unwrap_or_default();
        max_payload_size = max_payload_size.max(payload_json.len());
    }
    if let Err(response) =
        check_payload_size(state.db.pool(), &ctx.organization_id, max_payload_size).await
    {
        return Err(AppError::LimitExceeded(Box::new(response)));
    }

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

    // Reject duplicate job keys. Keys index job_mappings and the key set; duplicates
    // silently collapse, mis-wiring dependencies and leaving orphan jobs.
    if job_keys.len() != request.jobs.len() {
        return Err(AppError::BadRequest(
            "Workflow job keys must be unique".to_string(),
        ));
    }

    // Build the whole workflow (workflow row + jobs + dependency edges) in ONE
    // transaction. Previously each INSERT ran on the pool independently, so a
    // mid-sequence failure (e.g. a duplicate dependency edge) left a half-built
    // workflow committed — root jobs already runnable, others missing. All-or-nothing.
    let mut tx = state.db.pool().begin().await?;

    // Enforce workflow count / feature availability (Free disables workflows)
    // atomically: the advisory lock serializes concurrent creates for this org so
    // the count + inserts cannot overshoot the plan cap.
    lock_resource(&mut tx, &ctx.organization_id, "workflows").await?;
    if let Err(response) =
        check_resource_limit_conn(&mut tx, &ctx.organization_id, "workflows", 1).await
    {
        return Err(AppError::LimitExceeded(Box::new(response)));
    }

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
    .execute(&mut *tx)
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
        .execute(&mut *tx)
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
            .execute(&mut *tx)
            .await?;
        }
    }

    tx.commit().await?;

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

    // Increment daily job counter for plan limit tracking
    if let Err(e) = crate::api::middleware::limits::increment_daily_jobs(
        state.db.pool(),
        &ctx.organization_id,
        job_count as i32,
    )
    .await
    {
        warn!(error = %e, org_id = %ctx.organization_id, "Failed to increment daily job counter");
    }

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

/// Retry failed jobs in a workflow
///
/// Resets all failed/deadletter jobs back to pending and resumes the workflow.
pub async fn retry(
    State(state): State<AppState>,
    Extension(ctx): Extension<ApiKeyContext>,
    Path(workflow_id): Path<String>,
) -> AppResult<Json<WorkflowResponse>> {
    // Reset the jobs AND flip the workflow back to running in ONE transaction:
    // previously these were separate statements, so when the (buggy) workflow
    // UPDATE failed the jobs were already reset to pending while the workflow
    // stayed 'failed' — a divergent state the reset jobs could never recover from.
    let mut tx = state.db.pool().begin().await?;

    // Verify + lock the workflow row inside the transaction. `FOR UPDATE` serializes
    // concurrent retries of the same workflow: the second caller blocks here until the
    // first commits, then observes status='running' and is rejected — instead of both
    // reading 'failed' outside the tx (TOCTOU) and double-resetting the jobs. An early
    // Err return below drops the tx, rolling back with no changes applied.
    let workflow: Workflow =
        sqlx::query_as("SELECT * FROM workflows WHERE id = $1 AND organization_id = $2 FOR UPDATE")
            .bind(&workflow_id)
            .bind(&ctx.organization_id)
            .fetch_optional(&mut *tx)
            .await?
            .ok_or_else(|| AppError::NotFound(format!("Workflow {} not found", workflow_id)))?;

    // Only allow retry on failed workflows
    if workflow.status_enum() != crate::models::WorkflowStatus::Failed {
        return Err(AppError::BadRequest(format!(
            "Cannot retry workflow with status '{}'. Only failed workflows can be retried.",
            workflow.status
        )));
    }

    // Reset every non-completed job back to pending — including jobs the failure
    // cascade CANCELLED (dependents of the failed job). Previously only
    // failed/deadletter were reset, so cancelled dependents stayed terminal and
    // the retried workflow could never complete. `dependencies_met` is recomputed
    // from the current dependency states via check_job_dependencies_met(id): a
    // reset dependent whose parent is also being reset re-gates (FALSE) and is
    // released by the completion trigger when the parent re-completes; one whose
    // parent already completed is runnable immediately (TRUE).
    let updated = sqlx::query(
        r#"
        UPDATE jobs
        SET status = 'pending',
            retry_count = 0,
            last_error = NULL,
            started_at = NULL,
            completed_at = NULL,
            dependencies_met = check_job_dependencies_met(id),
            updated_at = NOW()
        WHERE workflow_id = $1
          AND organization_id = $2
          AND status IN ('failed', 'deadletter', 'cancelled')
        "#,
    )
    .bind(&workflow_id)
    .bind(&ctx.organization_id)
    .execute(&mut *tx)
    .await?;

    let jobs_retried = updated.rows_affected();

    // Update workflow status back to running. NOTE: the workflows table has no
    // `updated_at` column — referencing it here raised 42703 (undefined_column)
    // and 500'd every retry. Only touch columns that exist (cancel() is the model).
    sqlx::query(
        r#"
        UPDATE workflows
        SET status = 'running',
            completed_at = NULL
        WHERE id = $1 AND organization_id = $2
        "#,
    )
    .bind(&workflow_id)
    .bind(&ctx.organization_id)
    .execute(&mut *tx)
    .await?;

    // Recalculate completed/failed job counts
    let (completed_count,): (i64,) =
        sqlx::query_as("SELECT COUNT(*) FROM jobs WHERE workflow_id = $1 AND status = 'completed'")
            .bind(&workflow_id)
            .fetch_one(&mut *tx)
            .await?;

    let (failed_count,): (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM jobs WHERE workflow_id = $1 AND status IN ('failed', 'deadletter')",
    )
    .bind(&workflow_id)
    .fetch_one(&mut *tx)
    .await?;

    sqlx::query("UPDATE workflows SET completed_jobs = $1, failed_jobs = $2 WHERE id = $3")
        .bind(completed_count as i32)
        .bind(failed_count as i32)
        .bind(&workflow_id)
        .execute(&mut *tx)
        .await?;

    tx.commit().await?;

    info!(
        workflow_id = %workflow_id,
        org_id = %ctx.organization_id,
        jobs_retried = jobs_retried,
        "Retried workflow"
    );

    // Notify Redis that workflow was retried
    if let Some(ref cache) = state.cache {
        let _ = cache
            .publish(
                &format!("org:{}:workflow:{}", ctx.organization_id, workflow_id),
                "retried",
            )
            .await;

        // Also notify queue channels that jobs are ready
        let queues: Vec<(String,)> = sqlx::query_as(
            r#"
            SELECT DISTINCT queue_name 
            FROM jobs 
            WHERE workflow_id = $1 AND status = 'pending' AND dependencies_met = TRUE
            "#,
        )
        .bind(&workflow_id)
        .fetch_all(state.db.pool())
        .await
        .unwrap_or_default();

        for (queue_name,) in queues {
            let _ = cache
                .publish(
                    &format!("org:{}:queue:{}", ctx.organization_id, queue_name),
                    "workflow_job_ready",
                )
                .await;
        }
    }

    // Fetch and return the updated workflow
    let workflow: Workflow =
        sqlx::query_as("SELECT * FROM workflows WHERE id = $1 AND organization_id = $2")
            .bind(&workflow_id)
            .bind(&ctx.organization_id)
            .fetch_optional(state.db.pool())
            .await?
            .ok_or_else(|| AppError::NotFound(format!("Workflow {} not found", workflow_id)))?;

    Ok(Json(workflow.into()))
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

    // Serialize dependency-graph mutations per org in a transaction guarded by an
    // advisory lock. would_create_cycle + the edge INSERT were separate statements, so
    // two concurrent calls (A->B and B->A) could each pass the cycle check before either
    // edge existed and both insert, forming a cycle that deadlocks the jobs forever. The
    // lock forces the second caller to wait until the first commits, so its cycle check
    // observes the first edge.
    let mut tx = state.db.pool().begin().await?;
    sqlx::query("SELECT pg_advisory_xact_lock(hashtext($1))")
        .bind(format!("job_deps:{}", ctx.organization_id))
        .execute(&mut *tx)
        .await?;

    // Update dependency mode
    // SECURITY: Include organization_id for defense-in-depth
    sqlx::query("UPDATE jobs SET dependency_mode = $1, dependencies_met = FALSE WHERE id = $2 AND organization_id = $3")
        .bind(request.dependency_mode.to_string())
        .bind(&job_id)
        .bind(&ctx.organization_id)
        .execute(&mut *tx)
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
        .execute(&mut *tx)
        .await?;

        added += result.rows_affected() as i32;
    }

    // Check if dependencies are met
    let dependencies_met: bool = sqlx::query_scalar("SELECT check_job_dependencies_met($1)")
        .bind(&job_id)
        .fetch_one(&mut *tx)
        .await
        .unwrap_or(true);

    // Update job's dependencies_met flag
    // SECURITY: Include organization_id for defense-in-depth
    sqlx::query("UPDATE jobs SET dependencies_met = $1 WHERE id = $2 AND organization_id = $3")
        .bind(dependencies_met)
        .bind(&job_id)
        .bind(&ctx.organization_id)
        .execute(&mut *tx)
        .await?;

    tx.commit().await?;

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
