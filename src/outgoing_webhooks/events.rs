//! Outgoing webhook events added in 0.1.122 (`job.deadlettered`,
//! `job.retried`, `workflow.completed`, `workflow.failed`): their names, how
//! each is derived or built, and the scheduler step that announces workflow
//! outcomes.
//!
//! Every event goes out in the same envelope as the others
//! (`id`/`event`/`created_at`/`data`); only `data` differs.

use anyhow::Result;
use chrono::{DateTime, Utc};
use serde_json::{json, Value};
use sqlx::PgPool;
use tracing::{debug, warn};

use super::service::OutgoingWebhookService;

pub const JOB_FAILED: &str = "job.failed";
pub const JOB_DEADLETTERED: &str = "job.deadlettered";
pub const JOB_RETRIED: &str = "job.retried";
pub const WORKFLOW_COMPLETED: &str = "workflow.completed";
pub const WORKFLOW_FAILED: &str = "workflow.failed";

/// `source` of a `job.retried` sent by `POST /jobs/{id}/retry`.
pub const RETRY_SOURCE_JOB: &str = "job_retry";
/// `source` of a `job.retried` sent by `POST /jobs/dlq/retry`.
pub const RETRY_SOURCE_DLQ: &str = "dlq_retry";
/// `source` of a `job.retried` sent by `POST /workflows/{id}/retry`.
pub const RETRY_SOURCE_WORKFLOW: &str = "workflow_retry";

/// The event another event implies, if any.
///
/// A `job.failed` whose `status` is `deadletter` is also a `job.deadlettered`.
/// Every path that dead-letters a job (REST and gRPC fail, the gRPC stream,
/// the lease-expiry sweeps) already announces the move as `job.failed` with
/// that status, once per transition, so deriving the second event in the
/// dispatcher sends it exactly once from every one of those paths, including
/// any added later. Callers must not send `job.deadlettered` themselves.
pub fn implied_event(event: &str, payload: &Value) -> Option<&'static str> {
    (event == JOB_FAILED && payload.get("status").and_then(Value::as_str) == Some("deadletter"))
        .then_some(JOB_DEADLETTERED)
}

/// `data` of a `job.retried`: the job is back to `pending`.
///
/// `old_status` is the status it left (`failed`, `deadletter`, or `cancelled`
/// for a step a workflow retry re-opens); `source` is one of the
/// `RETRY_SOURCE_*` values.
pub fn job_retried_payload(
    job_id: &str,
    queue_name: &str,
    old_status: &str,
    source: &str,
    workflow_id: Option<&str>,
) -> Value {
    let mut data = json!({
        "job_id": job_id,
        "queue_name": queue_name,
        "status": "pending",
        "old_status": old_status,
        "source": source,
    });
    if let Some(workflow_id) = workflow_id {
        data["workflow_id"] = json!(workflow_id);
    }
    data
}

/// The event announcing a workflow that ended with `status`.
pub fn workflow_outcome_event(status: &str) -> Option<&'static str> {
    match status {
        "completed" => Some(WORKFLOW_COMPLETED),
        "failed" => Some(WORKFLOW_FAILED),
        _ => None,
    }
}

/// A workflow outcome picked for announcement.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct WorkflowOutcome {
    pub id: String,
    pub organization_id: String,
    pub name: String,
    pub status: String,
    pub total_jobs: i32,
    pub completed_jobs: i32,
    pub failed_jobs: i32,
    pub created_at: DateTime<Utc>,
    pub started_at: Option<DateTime<Utc>>,
    pub completed_at: Option<DateTime<Utc>>,
    pub metadata: Option<Value>,
}

impl WorkflowOutcome {
    /// `data` of the `workflow.completed` / `workflow.failed` event.
    pub fn payload(&self) -> Value {
        json!({
            "workflow_id": self.id,
            "name": self.name,
            "status": self.status,
            "total_jobs": self.total_jobs,
            "completed_jobs": self.completed_jobs,
            "failed_jobs": self.failed_jobs,
            "created_at": self.created_at,
            "started_at": self.started_at,
            "completed_at": self.completed_at,
            "metadata": self.metadata,
        })
    }
}

/// Workflow outcomes one announce pass sends at most.
pub const WORKFLOW_OUTCOME_BATCH: i64 = 500;

/// Claim the completed/failed workflows whose outcome has not been announced.
///
/// A workflow is announced when its `completed_at` differs from
/// `announced_completed_at`; the statement copies the one into the other for
/// the rows it returns, so each outcome is returned once, whichever scheduler
/// instance runs it (`SKIP LOCKED`); returned once is not delivered once
/// (see [`announce_workflow_outcomes`]). A retry clears `completed_at`, so the
/// next outcome is announced again. Only the last hour is looked at: older
/// outcomes (from before 0.1.122, or missed while no scheduler ran) are not
/// sent late. The predicate and order are those of the partial index
/// `idx_workflows_unannounced_outcome` (migration 20261014100201): change the
/// two together.
///
/// The picked rows are updated by primary key (`id = ANY(ARRAY(...))`): a join
/// against the pick let the planner hash-join it with a scan of every
/// workflow.
///
/// Binds: `$1` batch size.
pub const ANNOUNCE_WORKFLOW_OUTCOMES_SQL: &str = r#"
    UPDATE workflows w
    SET announced_completed_at = w.completed_at
    WHERE w.id = ANY(ARRAY(
        SELECT id
        FROM workflows
        WHERE status IN ('completed', 'failed')
          AND announced_completed_at IS DISTINCT FROM completed_at
          AND completed_at > NOW() - INTERVAL '1 hour'
        ORDER BY completed_at
        LIMIT $1
        FOR UPDATE SKIP LOCKED
    ))
    RETURNING w.id, w.organization_id, w.name, w.status, w.total_jobs,
              w.completed_jobs, w.failed_jobs, w.created_at, w.started_at,
              w.completed_at, w.metadata
"#;

/// Send `workflow.completed` / `workflow.failed` for every workflow that ended
/// since the last pass, whichever path ended it: the progress trigger on jobs
/// (run inside the transaction that finished the last job, where no webhook
/// can be sent) or the scheduler's terminate sweep. Returns how many were
/// announced.
///
/// At most once, by design: the claim (`ANNOUNCE_WORKFLOW_OUTCOMES_SQL`)
/// commits `announced_completed_at` before the events reach the in-memory
/// delivery queue (`spawn_dispatch`), and nothing re-sends a claimed outcome.
/// So no outcome is sent twice, whichever and however many instances run
/// the pass, but one is lost when:
/// - the process stops between that commit and the consumer recording the
///   delivery rows;
/// - the delivery queue (or the webhook's share of it) is full, so the event
///   is dropped and counted, or recording the deliveries fails;
/// - the workflow is reopened before the pass sees the outcome: a
///   `POST /workflows/{id}/retry` clears `completed_at`, so a workflow that
///   fails and is retried within one pass interval (the scheduler's 10 s
///   dependency tick) never announces that `workflow.failed`, only the
///   outcome it reaches afterwards;
/// - the outcome is more than an hour old when a pass first sees it
///   (no scheduler ran, or a pass failed before this step, for that long).
///
/// Documented in docs/guides/webhooks.md and docs/openapi.yaml; making it
/// at-least-once needs a durable outbox, not this claim.
pub async fn announce_workflow_outcomes(
    pool: &PgPool,
    webhooks: &std::sync::Arc<OutgoingWebhookService>,
) -> Result<usize> {
    let outcomes: Vec<WorkflowOutcome> = sqlx::query_as(ANNOUNCE_WORKFLOW_OUTCOMES_SQL)
        .bind(WORKFLOW_OUTCOME_BATCH)
        .fetch_all(pool)
        .await?;
    for outcome in &outcomes {
        let Some(event) = workflow_outcome_event(&outcome.status) else {
            // Cannot happen (the statement selects completed/failed only).
            warn!(workflow_id = %outcome.id, status = %outcome.status, "Unexpected workflow outcome");
            continue;
        };
        webhooks.spawn_dispatch(
            outcome.organization_id.clone(),
            event.to_string(),
            outcome.id.clone(),
            outcome.payload(),
        );
    }
    if !outcomes.is_empty() {
        debug!(count = outcomes.len(), "Announced workflow outcomes");
    }
    Ok(outcomes.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_dead_lettering_job_failed_implies_job_deadlettered() {
        let dead = json!({"job_id": "j", "status": "deadletter", "error": "boom"});
        assert_eq!(implied_event(JOB_FAILED, &dead), Some(JOB_DEADLETTERED));
        // A terminal failure that is not a dead-lettering implies nothing.
        let failed = json!({"job_id": "j", "status": "failed"});
        assert_eq!(implied_event(JOB_FAILED, &failed), None);
        // Nor does any other event carrying that status.
        assert_eq!(implied_event("job.cancelled", &dead), None);
        assert_eq!(implied_event(JOB_DEADLETTERED, &dead), None);
        assert_eq!(implied_event(JOB_FAILED, &json!(null)), None);
    }

    #[test]
    fn job_retried_payload_names_the_previous_status_and_source() {
        let p = job_retried_payload("j1", "q", "deadletter", RETRY_SOURCE_DLQ, None);
        assert_eq!(
            p,
            json!({"job_id": "j1", "queue_name": "q", "status": "pending",
                   "old_status": "deadletter", "source": "dlq_retry"})
        );
        let p = job_retried_payload("j2", "q", "cancelled", RETRY_SOURCE_WORKFLOW, Some("wf"));
        assert_eq!(p["workflow_id"], "wf");
        assert_eq!(p["source"], "workflow_retry");
    }

    #[test]
    fn only_completed_and_failed_workflows_are_announced() {
        assert_eq!(
            workflow_outcome_event("completed"),
            Some(WORKFLOW_COMPLETED)
        );
        assert_eq!(workflow_outcome_event("failed"), Some(WORKFLOW_FAILED));
        for status in ["cancelled", "running", "pending", ""] {
            assert_eq!(workflow_outcome_event(status), None, "{status}");
        }
    }

    #[test]
    fn new_event_names_are_subscribable() {
        for event in [
            JOB_DEADLETTERED,
            JOB_RETRIED,
            WORKFLOW_COMPLETED,
            WORKFLOW_FAILED,
        ] {
            assert!(crate::models::VALID_WEBHOOK_EVENTS.contains(&event));
        }
    }

    #[test]
    fn workflow_outcome_payload_shape() {
        let now = Utc::now();
        let outcome = WorkflowOutcome {
            id: "wf".into(),
            organization_id: "org".into(),
            name: "nightly".into(),
            status: "failed".into(),
            total_jobs: 3,
            completed_jobs: 2,
            failed_jobs: 1,
            created_at: now,
            started_at: Some(now),
            completed_at: Some(now),
            metadata: Some(json!({"k": "v"})),
        };
        let p = outcome.payload();
        assert_eq!(p["workflow_id"], "wf");
        assert_eq!(p["status"], "failed");
        assert_eq!(p["failed_jobs"], 1);
        assert_eq!(p["metadata"]["k"], "v");
        assert!(p.get("organization_id").is_none());
    }
}
