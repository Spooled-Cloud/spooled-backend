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
use validator::Validate;

use crate::api::handlers::require_queue_access;
use crate::api::middleware::limits::{
    check_job_limits, check_new_queue_limit, check_payload_size, check_resource_limit,
    check_resource_limit_conn, daily_jobs_limit, increment_daily_jobs, lock_resource,
    queue_job_defaults, refund_daily_jobs, resource_room_conn, try_increment_daily_jobs,
};
use crate::api::middleware::ValidatedJson;
use crate::api::AppState;
use crate::error::{AppError, AppResult};
use crate::models::ApiKeyContext;
use crate::models::{
    normalize_idempotency_key, ClaimJobsRequest, ClaimJobsResponse, ClaimedJob, CompleteJobRequest,
    CreateJobRequest, CreateJobResponse, FailJobRequest, HeartbeatJobRequest, Job, JobHistory,
    JobHistoryQuery, JobStats, JobStatsQuery, JobSummary, ListJobsQuery, DEPENDENCY_CANCEL_REASONS,
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

/// Longest tag a list filter accepts, in bytes.
const MAX_TAG_FILTER_LEN: usize = 1024;

/// A filter sent empty (`?status=`) is no filter.
fn filter_value(value: &Option<String>) -> Option<&str> {
    value.as_deref().filter(|v| !v.is_empty())
}

// Invalid list filters are a 400. They used to be logged and dropped, which
// answered the UNFILTERED list with 200: `?tag=customer:42` returned every
// customer's jobs, and a script retrying "the failed jobs" acted on all of them.

fn queue_filter(value: &Option<String>) -> AppResult<Option<&str>> {
    match filter_value(value) {
        Some(queue) if !validate_queue_name_filter(queue) => Err(AppError::Validation(
            "Invalid queue_name filter: use 1-255 letters, digits, '-', '_' or '.'".to_string(),
        )),
        queue => Ok(queue),
    }
}

/// Status filters match case-insensitively (`Pending` = `pending`). One status
/// or a comma-separated list (`failed,deadletter`): empty entries are skipped
/// and repeats dropped, so `?status=` is still no filter.
fn status_filter(value: &Option<String>) -> AppResult<Vec<&'static str>> {
    let Some(raw) = filter_value(value) else {
        return Ok(Vec::new());
    };
    let mut statuses: Vec<&'static str> = Vec::new();
    for entry in raw.split(',').map(str::trim).filter(|e| !e.is_empty()) {
        let status = VALID_JOB_STATUSES
            .iter()
            .copied()
            .find(|valid| valid.eq_ignore_ascii_case(entry))
            .ok_or_else(|| {
                AppError::Validation(format!(
                    "Invalid status filter '{}': expected one or more of {} (comma-separated)",
                    entry,
                    VALID_JOB_STATUSES.join(", ")
                ))
            })?;
        if !statuses.contains(&status) {
            statuses.push(status);
        }
    }
    Ok(statuses)
}

/// Longest `q` search text and `job_type` filter, in bytes.
const MAX_SEARCH_FILTER_LEN: usize = 255;

/// Escape `\`, `%` and `_` so the text matches itself in a LIKE pattern (whose
/// escape character is the default backslash).
fn escape_like(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len());
    for c in text.chars() {
        if matches!(c, '\\' | '%' | '_') {
            escaped.push('\\');
        }
        escaped.push(c);
    }
    escaped
}

/// Free text for `q` / `job_type`: at most 255 bytes, no NUL (Postgres text
/// cannot hold it).
fn text_filter<'a>(name: &str, value: Option<&'a str>) -> AppResult<Option<&'a str>> {
    match value {
        Some(text) if text.len() > MAX_SEARCH_FILTER_LEN || text.contains('\0') => {
            Err(AppError::Validation(format!(
                "Invalid {} filter: at most {} bytes and no NUL character",
                name, MAX_SEARCH_FILTER_LEN
            )))
        }
        text => Ok(text),
    }
}

/// An RFC 3339 instant. A `+` offset sent unencoded in a query string arrives
/// as a space (`...T10:00:00 02:00`); that one spelling is read as `+` too.
fn parse_rfc3339(raw: &str) -> Option<chrono::DateTime<Utc>> {
    if let Ok(t) = chrono::DateTime::parse_from_rfc3339(raw) {
        return Some(t.with_timezone(&Utc));
    }
    let sign = raw.len().checked_sub(6)?;
    if raw.as_bytes()[sign] != b' ' {
        return None;
    }
    chrono::DateTime::parse_from_rfc3339(&format!("{}+{}", &raw[..sign], &raw[sign + 1..]))
        .ok()
        .map(|t| t.with_timezone(&Utc))
}

fn time_filter(name: &str, value: &Option<String>) -> AppResult<Option<chrono::DateTime<Utc>>> {
    let Some(raw) = value.as_deref().map(str::trim).filter(|v| !v.is_empty()) else {
        return Ok(None);
    };
    parse_rfc3339(raw).map(Some).ok_or_else(|| {
        AppError::Validation(format!(
            "Invalid {} '{}': expected an RFC 3339 timestamp such as 2026-10-11T08:00:00Z",
            name, raw
        ))
    })
}

/// The validated filters of `GET /jobs` and `GET /jobs/dlq`. Every value is a
/// bound parameter; only fixed SQL text is pushed.
struct JobListFilters<'a> {
    queue: Option<&'a str>,
    statuses: Vec<&'static str>,
    tag: Option<&'a str>,
    /// `q`, LIKE-escaped.
    search: Option<String>,
    job_type: Option<&'a str>,
    created_after: Option<chrono::DateTime<Utc>>,
    created_before: Option<chrono::DateTime<Utc>>,
}

impl<'a> JobListFilters<'a> {
    /// Validate the query's filters; any invalid one is a 400. `with_status`
    /// is false for `/jobs/dlq`, whose status is fixed (its `status` parameter
    /// was always ignored, and still is).
    fn parse(query: &'a ListJobsQuery, with_status: bool) -> AppResult<Self> {
        let created_after = time_filter("created_after", &query.created_after)?;
        let created_before = time_filter("created_before", &query.created_before)?;
        if let (Some(after), Some(before)) = (created_after, created_before) {
            if after >= before {
                return Err(AppError::Validation(
                    "created_after must be earlier than created_before".to_string(),
                ));
            }
        }
        Ok(Self {
            queue: queue_filter(&query.queue_name)?,
            statuses: if with_status {
                status_filter(&query.status)?
            } else {
                Vec::new()
            },
            tag: tag_filter(&query.tag)?,
            search: text_filter(
                "q",
                query.q.as_deref().map(str::trim).filter(|q| !q.is_empty()),
            )?
            .map(escape_like),
            job_type: text_filter("job_type", filter_value(&query.job_type))?,
            created_after,
            created_before,
        })
    }

    /// ` AND ...` for each filter set except `q` (see [`job_list_query`]).
    /// `with_job_type: false` leaves out the `job_type` filter, for the part
    /// of a search that already matches one job type at a time.
    fn push(&self, qb: &mut QueryBuilder<Postgres>, with_job_type: bool) {
        if let Some(queue) = self.queue {
            qb.push(" AND queue_name = ");
            qb.push_bind(queue.to_string());
        }
        match self.statuses.as_slice() {
            [] => {}
            // One status keeps the plain equality (and its plans).
            [status] => {
                qb.push(" AND status = ");
                qb.push_bind(*status);
            }
            statuses => {
                qb.push(" AND status = ANY(");
                qb.push_bind(
                    statuses
                        .iter()
                        .map(|s| s.to_string())
                        .collect::<Vec<String>>(),
                );
                qb.push("::TEXT[])");
            }
        }
        if let Some(tag) = self.tag {
            qb.push(" AND tags ? ");
            qb.push_bind(tag.to_string());
        }
        if let Some(job_type) = self.job_type.filter(|_| with_job_type) {
            // Answered by idx_jobs_org_job_type, whose partial predicate
            // this repeats verbatim (the planner only uses a partial index
            // whose predicate the query implies).
            qb.push(" AND payload->>'job_type' = ");
            qb.push_bind(job_type.to_string());
            qb.push(JOB_TYPE_INDEXED);
        }
        if let Some(after) = self.created_after {
            qb.push(" AND created_at >= ");
            qb.push_bind(after);
        }
        if let Some(before) = self.created_before {
            qb.push(" AND created_at < ");
            qb.push_bind(before);
        }
    }
}

/// The predicate of idx_jobs_org_job_type (migration 20261014120000). Every
/// query that should use the index repeats it verbatim.
const JOB_TYPE_INDEXED: &str = " AND octet_length(payload->>'job_type') <= 255";

/// Most distinct job types of an organization that `q` is compared with (in
/// their sort order); the cap bounds the skip scan of [`SEARCH_JOB_TYPES_SQL`].
pub const MAX_SEARCH_JOB_TYPES: i64 = 1000;

/// The organization's (`$1`) job types that contain the `q` text (`$3`, an
/// ILIKE pattern), among its first `$2` distinct job types in sort order.
///
/// A skip scan of idx_jobs_org_job_type: each step is one index descent to the
/// next distinct type, so the cost follows the number of types, not of jobs,
/// and no payload but one per type is read. A substring cannot be looked up in
/// a btree, which is why `q` first resolves to whole job types here and then
/// matches jobs of those types through the same index ([`job_list_query`]).
/// It used to compare `payload->>'job_type'` with the text on every job of the
/// organization, reading each payload from TOAST: 466 ms for a search on 130k
/// jobs of ~3 KB payloads; this step takes about 1 ms there (41 types).
pub const SEARCH_JOB_TYPES_SQL: &str = "\
WITH RECURSIVE job_types(job_type) AS ( \
    (SELECT payload->>'job_type' FROM jobs \
      WHERE organization_id = $1 AND octet_length(payload->>'job_type') <= 255 \
      ORDER BY payload->>'job_type' LIMIT 1) \
  UNION ALL \
    SELECT (SELECT payload->>'job_type' FROM jobs \
             WHERE organization_id = $1 AND octet_length(payload->>'job_type') <= 255 \
               AND payload->>'job_type' > job_types.job_type \
             ORDER BY payload->>'job_type' LIMIT 1) \
    FROM job_types WHERE job_types.job_type IS NOT NULL \
) \
SELECT job_type FROM (SELECT job_type FROM job_types WHERE job_type IS NOT NULL LIMIT $2) AS seen \
WHERE job_type ILIKE $3";

/// Where a job list reads from: the organization's jobs, only the
/// dead-lettered ones for `/jobs/dlq`, only the key's queues for a scoped key.
struct JobListScope<'a> {
    org_id: &'a str,
    dead_letter_only: bool,
    queues: Option<&'a [String]>,
}

impl JobListScope<'_> {
    /// ` FROM jobs WHERE ...` for this scope and `filters` (except `q`).
    fn push_from(
        &self,
        qb: &mut QueryBuilder<Postgres>,
        filters: &JobListFilters<'_>,
        with_job_type: bool,
    ) {
        qb.push(" FROM jobs WHERE organization_id = ");
        qb.push_bind(self.org_id.to_string());
        if self.dead_letter_only {
            qb.push(" AND status = 'deadletter'");
        }
        // Queue scope: a restricted key only ever lists jobs in its own queues.
        // This must RESTRICT the result set (not merely validate an optional
        // filter), otherwise a scoped key with no `queue_name` filter would
        // see every queue's jobs.
        if let Some(allowed) = self.queues {
            qb.push(" AND queue_in_key_scope(queue_name, ");
            qb.push_bind(allowed.to_vec());
            qb.push("::TEXT[])");
        }
        filters.push(qb, with_job_type);
    }
}

/// ` AND (...)` matching `q` on the columns stored in the row: an id prefix or
/// a queue name substring (`NOT (...)` when `negate`). `search` is
/// LIKE-escaped, so `%` and `_` in it match themselves; ILIKE ignores case.
fn push_search_columns(qb: &mut QueryBuilder<Postgres>, search: &str, negate: bool) {
    qb.push(if negate {
        " AND NOT (id ILIKE "
    } else {
        " AND (id ILIKE "
    });
    qb.push_bind(format!("{search}%"));
    qb.push(" OR queue_name ILIKE ");
    qb.push_bind(format!("%{search}%"));
    qb.push(")");
}

/// The SQL of one job list page: `limit + 1` rows from `offset`, so the
/// caller can tell whether another page follows.
///
/// Without `q`, or when `q` names no job type (`search_types` empty), it is a
/// single SELECT. Otherwise the jobs matching `q` are the union of two
/// disjoint parts, each cut to the `offset + limit + 1` rows that could reach
/// the page:
/// - an id prefix or queue name match, read from the row's own columns;
/// - for each job type of `search_types`, its jobs that the first part does
///   not have, read through idx_jobs_org_job_type with the type as the index
///   key (a LATERAL probe per type).
///
/// So no part of a search compares the text with a payload. The planner
/// cannot tell that reading a payload is expensive (a large one is in TOAST),
/// and given a single `... OR payload->>'job_type' = ANY(...)` condition it
/// walked the organization's jobs and read every payload.
fn job_list_query(
    scope: &JobListScope<'_>,
    filters: &JobListFilters<'_>,
    search_types: &[String],
    (column, direction): (&'static str, &'static str),
    limit: i64,
    offset: i64,
) -> QueryBuilder<Postgres> {
    let mut qb: QueryBuilder<Postgres> = QueryBuilder::new("SELECT *");
    match filters.search.as_deref() {
        Some(search) if !search_types.is_empty() => {
            let part_rows = offset.saturating_add(limit).saturating_add(1);
            qb.push(" FROM ((SELECT *");
            scope.push_from(&mut qb, filters, true);
            push_search_columns(&mut qb, search, false);
            push_job_order(&mut qb, column, direction);
            qb.push(" LIMIT ");
            qb.push_bind(part_rows);
            qb.push(") UNION ALL (SELECT j.* FROM UNNEST(");
            qb.push_bind(search_types.to_vec());
            // `search_types` already honours a `job_type` filter.
            qb.push("::TEXT[]) AS t(job_type) CROSS JOIN LATERAL (SELECT *");
            scope.push_from(&mut qb, filters, false);
            qb.push(" AND payload->>'job_type' = t.job_type");
            qb.push(JOB_TYPE_INDEXED);
            push_search_columns(&mut qb, search, true);
            push_job_order(&mut qb, column, direction);
            qb.push(" LIMIT ");
            qb.push_bind(part_rows);
            qb.push(") AS j)) AS matched");
        }
        search => {
            scope.push_from(&mut qb, filters, true);
            if let Some(search) = search {
                push_search_columns(&mut qb, search, false);
            }
        }
    }
    push_job_order(&mut qb, column, direction);
    qb.push(" LIMIT ");
    qb.push_bind(limit + 1);
    qb.push(" OFFSET ");
    qb.push_bind(offset);
    qb
}

/// One page of `GET /jobs` or `GET /jobs/dlq` and whether another follows.
///
/// The statements run in a short read-only transaction with a `SET LOCAL
/// statement_timeout` (`DATABASE_LIST_STATEMENT_TIMEOUT_MS`, 5 s by default).
/// Postgres cancels a statement that runs longer itself, which also bounds the
/// work of a request whose client went away: dropping the request future does
/// not cancel a running statement. LOCAL ends with the transaction, so the
/// setting never reaches the next user of the pooled connection (nor another
/// client behind a transaction-mode PgBouncer).
async fn list_job_page(
    state: &AppState,
    scope: &JobListScope<'_>,
    filters: &JobListFilters<'_>,
    order: (&'static str, &'static str),
    limit: i64,
    offset: i64,
) -> AppResult<(Vec<Job>, bool)> {
    let timeout_ms = state.settings.database.list_statement_timeout_ms;
    let run = async {
        // AssertSqlSafe: the only value written into the text is a u64 from
        // the configuration; SET takes no bind parameters.
        let mut tx = state
            .db
            .pool()
            .begin_with(sqlx::AssertSqlSafe(format!(
                "BEGIN READ ONLY; SET LOCAL statement_timeout = {timeout_ms}"
            )))
            .await?;
        let mut search_types: Vec<String> = match filters.search.as_deref() {
            Some(search) => {
                sqlx::query_scalar(SEARCH_JOB_TYPES_SQL)
                    .bind(scope.org_id)
                    .bind(MAX_SEARCH_JOB_TYPES)
                    .bind(format!("%{search}%"))
                    .fetch_all(&mut *tx)
                    .await?
            }
            None => Vec::new(),
        };
        if let Some(job_type) = filters.job_type {
            search_types.retain(|t| t == job_type);
        }
        let mut qb = job_list_query(scope, filters, &search_types, order, limit, offset);
        let jobs: Vec<Job> = qb.build_query_as().fetch_all(&mut *tx).await?;
        tx.commit().await?;
        Ok::<_, sqlx::Error>(jobs)
    };
    let jobs = run.await.map_err(|e| list_query_error(e, timeout_ms))?;
    Ok(split_page(jobs, limit))
}

/// A list statement that Postgres cancelled (SQLSTATE 57014: the statement
/// timeout, or an operator's pg_cancel_backend) is a `400 QUERY_TIMEOUT`
/// asking for a narrower list. Not a 5xx:
/// the SDKs retry those and count them toward their circuit breaker, and the
/// same list would only time out again. Any other failure stays a database
/// error, a 500 whose body carries no SQL.
fn list_query_error(e: sqlx::Error, timeout_ms: u64) -> AppError {
    let cancelled = e
        .as_database_error()
        .and_then(|db| db.code())
        .is_some_and(|code| code == "57014");
    if !cancelled {
        return AppError::Database(e);
    }
    warn!(timeout_ms, "Job list stopped by the list statement timeout");
    AppError::QueryTimeout(format!(
        "Listing these jobs took longer than {timeout_ms} ms and was stopped. Narrow it with \
         queue_name, status, job_type, created_after or created_before, or a longer q"
    ))
}

/// Response header saying whether another page follows: `true` when the
/// page was full and at least one more row matched. The list bodies are bare
/// arrays, so the hint rides in a header rather than a new body field.
pub const HAS_MORE_HEADER: &str = "x-has-more";

/// Fetch one row past `limit` to learn whether more follow, then drop it.
pub(crate) fn split_page<T>(mut rows: Vec<T>, limit: i64) -> (Vec<T>, bool) {
    let limit = usize::try_from(limit).unwrap_or(0);
    let has_more = rows.len() > limit;
    rows.truncate(limit);
    (rows, has_more)
}

/// `body` with the [`HAS_MORE_HEADER`] paging hint.
pub(crate) fn paged<T: serde::Serialize>(body: T, has_more: bool) -> impl IntoResponse {
    (
        [(
            axum::http::HeaderName::from_static(HAS_MORE_HEADER),
            axum::http::HeaderValue::from_static(if has_more { "true" } else { "false" }),
        )],
        Json(body),
    )
}

/// Tags are stored as free-form JSON, so the filter takes any characters it
/// could match (it is a bound parameter). It used to allow only
/// `[A-Za-z0-9_.-]`, so `customer:42` or `env prod` could never be filtered.
/// NUL is refused because Postgres text cannot hold it.
fn tag_filter(value: &Option<String>) -> AppResult<Option<&str>> {
    match filter_value(value) {
        Some(tag) if tag.len() > MAX_TAG_FILTER_LEN || tag.contains('\0') => {
            Err(AppError::Validation(format!(
                "Invalid tag filter: at most {} bytes and no NUL character",
                MAX_TAG_FILTER_LEN
            )))
        }
        tag => Ok(tag),
    }
}

/// `ORDER BY` for job lists: the requested column, then `id`, so rows that tie
/// on it (a bulk enqueue or a cron batch shares one `created_at`) keep one
/// order across OFFSET pages. Without the tiebreak a claim moving a row could
/// show it on two pages and skip another. `column` and `direction` come from
/// [`ListJobsQuery::order`], i.e. from fixed strings.
fn push_job_order(qb: &mut QueryBuilder<Postgres>, column: &'static str, direction: &'static str) {
    // NULL timestamps (not yet scheduled / completed) go last either way.
    let nulls = match column {
        "scheduled_at" | "completed_at" => " NULLS LAST",
        _ => "",
    };
    qb.push(format_args!(
        " ORDER BY {column} {direction}{nulls}, id {direction}"
    ));
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

/// A worker presented a `lease_id` that is not the job's current lease while that
/// lease is still live. Same `LEASE_EXPIRED` code as a timed-out lease — SDK worker
/// runtimes treat both as "this worker no longer holds the job" — but a message
/// that says what actually happened.
fn lease_mismatch_error() -> AppError {
    AppError::LeaseExpired(
        "lease_id does not match the job's current lease; this worker no longer holds it"
            .to_string(),
    )
}

/// A worker acted on a job it owned that already reached a terminal status
/// (a duplicate `complete`, for example): 409 rather than a misleading 404.
fn already_finished_error(status: &str) -> AppError {
    AppError::Conflict(format!("Job is already {}", status))
}

/// The job an idempotency key already names in this org, if any.
async fn existing_idempotent_job(
    pool: &sqlx::PgPool,
    org_id: &str,
    key: &str,
) -> Result<Option<String>, sqlx::Error> {
    sqlx::query_scalar("SELECT id FROM jobs WHERE organization_id = $1 AND idempotency_key = $2")
        .bind(org_id)
        .bind(key)
        .fetch_optional(pool)
        .await
}

/// `expires_at` must lie in the future and after `scheduled_at`. Such a job
/// was accepted (201, quota charged) but could never be claimed, and the
/// expiry sweep deleted it a few minutes later.
fn validate_expiry(
    expires_at: Option<chrono::DateTime<Utc>>,
    scheduled_at: Option<chrono::DateTime<Utc>>,
    now: chrono::DateTime<Utc>,
) -> AppResult<()> {
    let Some(expires_at) = expires_at else {
        return Ok(());
    };
    if expires_at <= now {
        return Err(AppError::Validation(
            "expires_at must be in the future".to_string(),
        ));
    }
    if scheduled_at.is_some_and(|scheduled_at| expires_at <= scheduled_at) {
        return Err(AppError::Validation(
            "expires_at must be later than scheduled_at".to_string(),
        ));
    }
    Ok(())
}

/// Map a failed job INSERT. The database refusing a value (a data exception,
/// SQLSTATE class 22, or a CHECK violation) is bad input and a 400; anything
/// else is a 500 with the SQL error kept out of the response.
fn job_insert_error(e: sqlx::Error) -> AppError {
    tracing::error!(error = %e, "Failed to create job");
    let rejected_value = e
        .as_database_error()
        .and_then(|db| db.code())
        .is_some_and(|code| code.starts_with("22") || code == "23514");
    if rejected_value {
        AppError::Validation("The database rejected a value of the job".to_string())
    } else {
        AppError::Database(sqlx::Error::Protocol("Failed to create job".to_string()))
    }
}

/// Give back daily-quota credits reserved for jobs that were not created.
async fn refund_reserved_quota(state: &AppState, org_id: &str, reserved: bool, count: i64) {
    if !reserved || count <= 0 {
        return;
    }
    // The dedicated sign-safe function: incrementing by a negative count
    // assigned instead of adding across a reset boundary.
    if let Err(e) = refund_daily_jobs(state.db.pool(), org_id, count as i32).await {
        tracing::warn!(
            error = %e,
            org_id = %org_id,
            refund = count,
            "Failed to refund an unused daily job reservation"
        );
    }
}

/// List jobs with optional filtering
///
/// Reduced max limit from 1000 to 100. Filters: `queue_name`, `status` (one
/// or a comma list), `tag`, `q`, `job_type`, `created_after`,
/// `created_before`; the `X-Has-More` header says whether another page
/// follows.
pub async fn list(
    State(state): State<AppState>,
    Extension(ctx): Extension<ApiKeyContext>,
    Query(query): Query<ListJobsQuery>,
) -> AppResult<impl IntoResponse> {
    // Cap limit at 100 to prevent memory exhaustion
    let limit = query.limit.unwrap_or(50).clamp(1, MAX_JOBS_PER_PAGE);
    let offset = query.offset.unwrap_or(0).max(0);

    let filters = JobListFilters::parse(&query, true)?;
    let order = query.order().map_err(AppError::Validation)?;
    let scope = JobListScope {
        org_id: &ctx.organization_id,
        dead_letter_only: false,
        queues: ctx.queue_scope_filter(),
    };

    // All filters are bound parameters (no string interpolation of user input).
    let (jobs, has_more) = list_job_page(&state, &scope, &filters, order, limit, offset).await?;

    let summaries: Vec<JobSummary> = jobs.into_iter().map(Into::into).collect();
    Ok(paged(summaries, has_more))
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
    let idempotency_key = normalize_idempotency_key(request.idempotency_key.clone());

    // A replay of a job that already exists is answered before any quota check.
    // The retry of a request whose response was lost used to be evaluated as a
    // new job: at a full active-jobs or daily cap it got 429 although its job
    // existed, and the producer took it for not enqueued.
    if let Some(ref key) = idempotency_key {
        if let Some(existing_id) = existing_idempotent_job(state.db.pool(), &org_id, key).await? {
            return Ok((
                StatusCode::OK,
                Json(CreateJobResponse {
                    id: existing_id,
                    created: false,
                }),
            ));
        }
    }

    let now = Utc::now();
    validate_expiry(request.expires_at, request.scheduled_at, now)?;

    // Check payload size against plan limits
    let payload_json = serde_json::to_string(&request.payload).unwrap_or_default();
    if let Err(response) = check_payload_size(state.db.pool(), &org_id, payload_json.len()).await {
        return Err(AppError::LimitExceeded(Box::new(response)));
    }

    // Check job limits (daily + active) before creating
    if let Err(response) = check_job_limits(state.db.pool(), &org_id, 1).await {
        return Err(AppError::LimitExceeded(Box::new(response)));
    }

    // Omitted retry/timeout come from the queue's config; a job into a queue that
    // does not exist yet creates it, which is charged against the plan's queue cap.
    let queue_defaults = queue_job_defaults(
        state.db.pool(),
        &state.settings.queue,
        &org_id,
        &request.queue_name,
    )
    .await?;
    if !queue_defaults.exists {
        if let Err(response) = check_new_queue_limit(state.db.pool(), &org_id, 1).await {
            return Err(AppError::LimitExceeded(Box::new(response)));
        }
    }

    let job_id = Uuid::new_v4().to_string();

    let priority = request.priority.unwrap_or(0);
    let max_retries = request.max_retries.unwrap_or(queue_defaults.max_retries);
    let timeout_seconds = request
        .timeout_seconds
        .unwrap_or(queue_defaults.timeout_seconds);

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

    // Reserve the daily quota BEFORE inserting. Charging it after the insert
    // meant that, at the cap, the job was briefly visible (a worker could claim
    // it) and then deleted, and a concurrent replay of the same key could be
    // answered 200 for a job that was about to disappear. The reservation is
    // refunded when no row is created (insert error, or a replay that raced in).
    let daily_limit = daily_jobs_limit(state.db.pool(), &org_id)
        .await
        .unwrap_or(-1);
    let quota_reserved = match try_increment_daily_jobs(state.db.pool(), &org_id, 1, daily_limit)
        .await
    {
        Ok(count) if count >= 0 => true,
        Ok(_) => {
            let response = check_job_limits(state.db.pool(), &org_id, 1)
                .await
                .err()
                .unwrap_or_else(|| {
                    (StatusCode::TOO_MANY_REQUESTS, "Daily job limit reached").into_response()
                });
            return Err(AppError::LimitExceeded(Box::new(response)));
        }
        Err(e) => {
            // Counter DB error: accept the job uncounted (best-effort, as before)
            // rather than failing an otherwise valid enqueue.
            tracing::warn!(error = %e, org_id = %org_id, "Failed to increment daily job counter");
            false
        }
    };

    // Use FIXED idempotency pattern: DO UPDATE SET RETURNING
    // This always returns an ID whether the job is new or existing
    let inserted: Result<(String,), sqlx::Error> = sqlx::query_as(
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
    .bind(&idempotency_key)
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
    .await;

    // Check if this was a new job or existing (idempotent)
    let returned_id = match inserted {
        Ok((id,)) => id,
        Err(e) => {
            refund_reserved_quota(&state, &org_id, quota_reserved, 1).await;
            return Err(job_insert_error(e));
        }
    };
    let created = returned_id == job_id;

    // Update metrics and usage counters
    if created {
        state.metrics.jobs_enqueued.inc();
        state.metrics.jobs_pending.inc();
    } else {
        refund_reserved_quota(&state, &org_id, quota_reserved, 1).await;
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
    // Default lease: WORKER_LEASE_DURATION_SECS (30 s unless configured). Workers
    // that run longer pass `lease_duration_secs` (5-3600) or heartbeat.
    let lease_duration_secs = request
        .lease_duration_secs
        .unwrap_or(state.settings.worker.lease_duration_secs as i64);

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
            Some(request.lease_id.as_str()),
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
        WorkerOpOutcome::LeaseMismatch => return Err(lease_mismatch_error()),
        WorkerOpOutcome::AlreadyFinished(status) => {
            return Err(already_finished_error(&status));
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
        Option<i64>,
    );
    let job_data: Option<CompletionJobRow> = sqlx::query_as(
        r#"
        SELECT queue_name, result, completion_webhook, completion_webhook_secret,
               (EXTRACT(EPOCH FROM (completed_at - started_at)) * 1000)::BIGINT
        FROM jobs WHERE id = $1 AND organization_id = $2
        "#,
    )
    .bind(&id)
    .bind(&ctx.organization_id)
    .fetch_optional(state.db.pool())
    .await
    .unwrap_or(None);

    let (queue_name, result, completion_webhook, completion_webhook_secret, duration_ms) =
        job_data.unwrap_or_else(|| ("unknown".to_string(), None, None, None, None));
    // Processing time (claim -> completion). Was hard-coded to 0 before 0.1.114.
    // Feeds spooled_job_duration_seconds (SlowJobProcessing alerts); skipped when
    // the timing columns were unavailable rather than observing a fake 0.
    if let Some(ms) = duration_ms {
        state
            .metrics
            .job_duration
            .observe(ms.max(0) as f64 / 1000.0);
    }
    let duration_ms = duration_ms.unwrap_or(0).max(0);

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
                    "duration_ms": duration_ms,
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
            &ctx.organization_id,
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
            Some(request.lease_id.as_str()),
            ctx.queue_scope_filter(),
            &crate::models::normalize_failure_message(&request.error),
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
        WorkerOpOutcome::LeaseMismatch => return Err(lease_mismatch_error()),
        WorkerOpOutcome::AlreadyFinished(status) => {
            return Err(already_finished_error(&status));
        }
        WorkerOpOutcome::NotOwned => {
            return Err(AppError::NotFound(
                "Job not found or not owned by worker".to_string(),
            ));
        }
    }

    // The status this transition set (`pending` for a scheduled retry,
    // `deadletter`). It is taken from the transition, not read back after the
    // commit: by then the retry may be claimed again or the dead-lettered job
    // retried, and job.failed (with the job.deadlettered it implies) would be
    // skipped or carry the wrong status.
    let new_status = outcome.new_status.clone().unwrap_or_else(|| {
        if outcome.will_retry {
            "pending"
        } else {
            "deadletter"
        }
        .to_string()
    });

    // Metrics: processing -> pending (retry) OR processing -> deadletter. The gRPC
    // fail path counts the same series; without this REST workers' failures never
    // reached spooled_jobs_failed_total / spooled_jobs_deadlettered_total.
    state.metrics.jobs_processing.dec();
    state.metrics.jobs_failed.inc();
    if new_status == "deadletter" {
        state.metrics.jobs_deadlettered.inc();
    }

    // Best-effort realtime publish and webhooks; the row supplies the other
    // fields.
    type FailJobRow = (String, i32, Option<String>, Option<String>);
    let updated: Option<FailJobRow> = sqlx::query_as(
        "SELECT queue_name, retry_count, completion_webhook, completion_webhook_secret FROM jobs WHERE id = $1 AND organization_id = $2",
    )
    .bind(&id)
    .bind(&ctx.organization_id)
    .fetch_optional(state.db.pool())
    .await
    .unwrap_or(None);

    if let Some((queue_name, retry_count, completion_webhook, completion_webhook_secret)) = updated
    {
        if let Some(ref cache) = state.cache {
            let now = Utc::now().to_rfc3339();
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
                        "timestamp": now
                    }
                }),
            )
            .await;
            // `job.failed` is documented as "job failed (may retry)", so it fires on
            // every reported failure — `retry_count` and the status event above tell
            // a scheduled retry apart from a dead-lettered job.
            publish_realtime_event(
                cache,
                &ctx.organization_id,
                serde_json::json!({
                    "type": "JobFailed",
                    "data": {
                        "job_id": id,
                        "queue_name": queue_name,
                        "error": request.error,
                        "retry_count": retry_count,
                        "timestamp": now
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
                    &ctx.organization_id,
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

    // An omitted lease_duration_secs renews for the length the lease was
    // granted with, not the 30 s default (that cut a 600 s claim to 30 s).
    let outcome = queue
        .renew_lease_for(
            &id,
            &request.worker_id,
            &ctx.organization_id,
            Some(request.lease_id.as_str()),
            ctx.queue_scope_filter(),
            request.lease_duration_secs,
            state.settings.worker.lease_duration_secs as i64,
        )
        .await
        .map_err(|e| AppError::Internal(e.to_string()))?;

    match outcome {
        WorkerOpOutcome::Ok => Ok((StatusCode::OK, Json(serde_json::json!({ "success": true })))),
        WorkerOpOutcome::LeaseExpired => Err(AppError::LeaseExpired(
            "Job lease expired and cannot be extended".to_string(),
        )),
        WorkerOpOutcome::LeaseMismatch => Err(lease_mismatch_error()),
        WorkerOpOutcome::AlreadyFinished(status) => Err(already_finished_error(&status)),
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

/// A job's recorded lifecycle events (`job_history`)
///
/// GET /api/v1/jobs/{id}/history?order_dir=asc|desc&limit=&offset=
///
/// Oldest first unless `order_dir=desc`. Same access rule as `GET /jobs/{id}`:
/// another organization's job, or one in a queue outside the key's scope, is
/// a 404. `worker_id` is the column, or else the `worker_id` the claim wrote
/// into `details`. The `X-Has-More` header says whether more events follow.
pub async fn history(
    State(state): State<AppState>,
    Extension(ctx): Extension<ApiKeyContext>,
    Path(id): Path<String>,
    Query(query): Query<JobHistoryQuery>,
) -> AppResult<impl IntoResponse> {
    let direction = query.direction().map_err(AppError::Validation)?;
    let (limit, offset) = query.page();

    let queue_name: Option<String> =
        sqlx::query_scalar("SELECT queue_name FROM jobs WHERE id = $1 AND organization_id = $2")
            .bind(&id)
            .bind(&ctx.organization_id)
            .fetch_optional(state.db.pool())
            .await?;
    match queue_name {
        Some(queue) if ctx.can_access_queue(&queue) => {}
        // Out-of-scope existence is not revealed, as in get().
        _ => return Err(AppError::NotFound("Job not found".to_string())),
    }

    // `direction` is one of two fixed strings. Walks
    // idx_job_history_job_created (job_id, created_at DESC) either way.
    let events: Vec<JobHistory> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT id, job_id, event_type, \
                COALESCE(worker_id, details->>'worker_id') AS worker_id, \
                details, created_at \
         FROM job_history \
         WHERE job_id = $1 \
         ORDER BY created_at {direction}, id {direction} \
         LIMIT $2 OFFSET $3"
    )))
    .bind(&id)
    .bind(limit + 1)
    .bind(offset)
    .fetch_all(state.db.pool())
    .await?;
    let (events, has_more) = split_page(events, limit);

    Ok(paged(events, has_more))
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

/// Deepest dependency chain re-opened by one retry.
const MAX_REOPEN_DEPTH: usize = 100;

/// After the `retried` jobs went back to pending, re-open the dependents the
/// sweep cancelled because of them, level by level (a cancelled step's own
/// dependents were cancelled for the same reason). Returns the re-opened ids.
///
/// Before, a retried step ran alone: its dependents stayed cancelled, and a
/// workflow with another branch still running then ended 'completed' with a
/// step that never ran. A dependent is re-opened gated
/// (`dependencies_met = FALSE`) until the retried job completes, and only when
/// none of its other dependencies still dooms it (the sweep would cancel it
/// again at once). Steps of a cancelled workflow stay cancelled.
async fn reopen_cancelled_dependents(
    conn: &mut sqlx::PgConnection,
    org_id: &str,
    retried: &[String],
) -> Result<Vec<String>, sqlx::Error> {
    let mut reopened = Vec::new();
    let mut frontier = retried.to_vec();
    for _ in 0..MAX_REOPEN_DEPTH {
        if frontier.is_empty() {
            break;
        }
        let level: Vec<String> = sqlx::query_scalar(
            r#"
            UPDATE jobs child
            SET
                status = CASE WHEN child.scheduled_at > NOW() THEN 'scheduled' ELSE 'pending' END,
                last_error = NULL,
                completed_at = NULL,
                dependencies_met = check_job_dependencies_met(child.id)
                    AND NOT EXISTS (
                        SELECT 1 FROM jobs parent
                        WHERE parent.id = child.parent_job_id AND parent.status <> 'completed'
                    ),
                updated_at = NOW()
            WHERE child.organization_id = $1
              AND child.status = 'cancelled'
              AND child.last_error = ANY($3)
              AND (
                  child.parent_job_id = ANY($2)
                  OR EXISTS (
                      SELECT 1 FROM job_dependencies jd
                      WHERE jd.job_id = child.id AND jd.depends_on_job_id = ANY($2)
                  )
              )
              AND NOT EXISTS (
                  SELECT 1 FROM workflows w
                  WHERE w.id = child.workflow_id AND w.status = 'cancelled'
              )
              AND NOT EXISTS (
                  SELECT 1 FROM jobs parent
                  WHERE parent.id = child.parent_job_id
                    AND parent.status IN ('failed', 'deadletter', 'cancelled')
              )
              AND (
                  (
                      COALESCE(child.dependency_mode, 'all') <> 'any'
                      AND NOT EXISTS (
                          SELECT 1 FROM job_dependencies jd
                          JOIN jobs dep ON dep.id = jd.depends_on_job_id
                          WHERE jd.job_id = child.id
                            AND dep.status IN ('failed', 'deadletter', 'cancelled')
                      )
                  )
                  OR (
                      child.dependency_mode = 'any'
                      AND EXISTS (
                          SELECT 1 FROM job_dependencies jd
                          JOIN jobs dep ON dep.id = jd.depends_on_job_id
                          WHERE jd.job_id = child.id
                            AND dep.status NOT IN ('failed', 'deadletter', 'cancelled')
                      )
                  )
              )
            RETURNING child.id
            "#,
        )
        .bind(org_id)
        .bind(&frontier)
        // Only dependents the scheduler's sweep cancelled (shared reasons).
        .bind(DEPENDENCY_CANCEL_REASONS)
        .fetch_all(&mut *conn)
        .await?;
        reopened.extend(level.iter().cloned());
        frontier = level;
    }
    if !reopened.is_empty() {
        info!(
            org_id = %org_id,
            retried = ?retried,
            reopened = reopened.len(),
            "Re-opened dependents cancelled by a retried job's failure"
        );
    }
    Ok(reopened)
}

/// Bring the workflows of retried and re-opened jobs back in line: a 'failed'
/// workflow runs again (it would otherwise ignore its jobs settling, and keep
/// stale counters) and pays its max_workflows slot as
/// `POST /workflows/{id}/retry` does; the caller holds that cap's lock. Every
/// affected workflow gets its counters recomputed.
///
/// `POST /jobs/{id}/retry` names one job, so a refusal here refuses it. A DLQ
/// retry picks only the steps whose workflows fit under the cap (see
/// [`DlqScope`]); for it this check is a backstop.
async fn reopen_workflows(
    conn: &mut sqlx::PgConnection,
    org_id: &str,
    retried: &[String],
    reopened: &[String],
) -> AppResult<()> {
    let job_ids: Vec<&String> = retried.iter().chain(reopened).collect();
    let workflows: Vec<(String, String)> = sqlx::query_as(
        r#"
        SELECT w.id, w.status
        FROM workflows w
        WHERE w.organization_id = $1
          AND w.id IN (
              SELECT workflow_id FROM jobs
              WHERE organization_id = $1 AND id = ANY($2) AND workflow_id IS NOT NULL
          )
        FOR UPDATE OF w
        "#,
    )
    .bind(org_id)
    .bind(&job_ids)
    .fetch_all(&mut *conn)
    .await?;
    if workflows.is_empty() {
        return Ok(());
    }

    let reviving = workflows.iter().filter(|(_, s)| s == "failed").count() as u64;
    if reviving > 0 {
        if let Err(response) =
            check_resource_limit_conn(&mut *conn, org_id, "workflows", reviving).await
        {
            return Err(AppError::LimitExceeded(Box::new(response)));
        }
    }

    let ids: Vec<String> = workflows.into_iter().map(|(id, _)| id).collect();
    sqlx::query(
        r#"
        UPDATE workflows w
        SET
            status = CASE WHEN w.status = 'failed' THEN 'running' ELSE w.status END,
            completed_at = CASE WHEN w.status = 'failed' THEN NULL ELSE w.completed_at END,
            completed_jobs = (
                SELECT COUNT(*) FROM jobs j WHERE j.workflow_id = w.id AND j.status = 'completed'
            ),
            failed_jobs = (
                SELECT COUNT(*) FROM jobs j
                WHERE j.workflow_id = w.id AND j.status IN ('failed', 'deadletter')
            )
        WHERE w.organization_id = $1
          AND w.id = ANY($2)
          AND w.status IN ('pending', 'running', 'failed')
        "#,
    )
    .bind(org_id)
    .bind(&ids)
    .execute(&mut *conn)
    .await?;
    Ok(())
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
    let before: Option<(String, String, Option<String>)> = sqlx::query_as(
        "SELECT status, queue_name, workflow_id FROM jobs WHERE id = $1 AND organization_id = $2",
    )
    .bind(&id)
    .bind(&ctx.organization_id)
    .fetch_optional(state.db.pool())
    .await?;
    if let Some((_, ref queue_name, _)) = before {
        if !ctx.can_access_queue(queue_name) {
            return Err(AppError::NotFound("Job not found".to_string()));
        }
    }
    let before_status: Option<(String,)> = before.as_ref().map(|(s, _, _)| (s.clone(),));

    // One transaction: the retry, re-opening the dependents its failure
    // cancelled, and putting its workflow back to running apply together.
    let mut tx = state.db.pool().begin().await?;
    if before
        .as_ref()
        .is_some_and(|(_, _, workflow)| workflow.is_some())
    {
        // Reviving a failed workflow pays a max_workflows slot; take the cap's
        // lock before any row lock, in the order workflow create/retry use.
        lock_resource(&mut tx, &ctx.organization_id, "workflows").await?;
    }

    let job = sqlx::query_as::<_, Job>(
        r#"
        UPDATE jobs
        SET
            status = 'pending',
            -- A manual retry grants a fresh retry budget, exactly like
            -- POST /jobs/dlq/retry. Incrementing here left dead-lettered jobs with
            -- retry_count > max_retries, so one more failure skipped every retry.
            retry_count = 0,
            last_error = NULL,
            updated_at = NOW()
        WHERE id = $1 AND organization_id = $2 AND status IN ('failed', 'deadletter')
        RETURNING *
        "#,
    )
    .bind(&id)
    .bind(&ctx.organization_id)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or_else(|| {
        if before.is_none() {
            AppError::NotFound("Job not found".to_string())
        } else {
            AppError::Conflict("Job cannot be retried (not in failed/deadletter state)".to_string())
        }
    })?;

    // The job is live again: drop its dead_letter_queue audit row, as the DLQ
    // retry path does, so GET /jobs/dlq stops listing it.
    sqlx::query("DELETE FROM dead_letter_queue WHERE job_id = $1 AND organization_id = $2")
        .bind(&id)
        .bind(&ctx.organization_id)
        .execute(&mut *tx)
        .await?;

    let retried = [id.clone()];
    let reopened = reopen_cancelled_dependents(&mut tx, &ctx.organization_id, &retried).await?;
    reopen_workflows(&mut tx, &ctx.organization_id, &retried, &reopened).await?;
    tx.commit().await?;

    state.metrics.jobs_retried.inc();
    state.metrics.jobs_pending.add(1 + reopened.len() as i64);

    state.outgoing_webhooks.spawn_dispatch(
        ctx.organization_id.clone(),
        crate::outgoing_webhooks::events::JOB_RETRIED.to_string(),
        job.id.clone(),
        crate::outgoing_webhooks::events::job_retried_payload(
            &job.id,
            &job.queue_name,
            before_status
                .as_ref()
                .map(|(s,)| s.as_str())
                .unwrap_or("failed"),
            crate::outgoing_webhooks::events::RETRY_SOURCE_JOB,
            job.workflow_id.as_deref(),
        ),
    );

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
    // An invalid queue filter is a 400 here too, not org-wide totals.
    let validated_queue = queue_filter(&query.queue_name)?;

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
          AND queue_in_key_scope(queue_name, $2::TEXT[])
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
///
/// Items are validated one by one: an invalid item is reported in `failed`
/// under its index with the reason, and the valid items are still created.
/// The item validators never ran before, so one out-of-range value failed the
/// whole INSERT and the API still answered 200 with every item in `failed` as
/// "Failed to create job". A batch with no valid item, invalid batch defaults,
/// or a batch the database refuses is now an error status, not a 200.
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
    let total = request.jobs.len();

    let mut failed: Vec<crate::models::BulkJobError> = Vec::new();
    // (request index, item, idempotency key with a blank key read as none)
    let mut valid: Vec<(usize, &crate::models::BulkJobItem, Option<String>)> = Vec::new();
    for (index, item) in request.jobs.iter().enumerate() {
        match item.validate() {
            Ok(()) => valid.push((
                index,
                item,
                normalize_idempotency_key(item.idempotency_key.clone()),
            )),
            Err(errors) => failed.push(crate::models::BulkJobError {
                index,
                error: match AppError::from(errors) {
                    AppError::Validation(message) => message,
                    other => other.to_string(),
                },
            }),
        }
    }
    if valid.is_empty() {
        let reasons: Vec<String> = failed
            .iter()
            .map(|f| format!("jobs[{}]: {}", f.index, f.error))
            .collect();
        return Err(AppError::Validation(format!(
            "No job in the batch is valid; {}",
            reasons.join("; ")
        )));
    }

    // Replays are answered from the jobs that already exist and charge no quota:
    // only items that will create a row count against the caps (the first item
    // of the batch carrying a key not stored yet, and every keyless item). The
    // whole request used to be charged, so a replayed batch at a full cap was
    // refused although every job existed.
    let keys: Vec<&str> = valid
        .iter()
        .filter_map(|(_, _, key)| key.as_deref())
        .collect();
    let existing_keys: std::collections::HashSet<String> = if keys.is_empty() {
        std::collections::HashSet::new()
    } else {
        sqlx::query_scalar::<_, String>(
            "SELECT idempotency_key FROM jobs WHERE organization_id = $1 AND idempotency_key = ANY($2)",
        )
        .bind(&org_id)
        .bind(&keys)
        .fetch_all(state.db.pool())
        .await?
        .into_iter()
        .collect()
    };
    let mut batch_keys = std::collections::HashSet::new();
    let creates: Vec<bool> = valid
        .iter()
        .map(|(_, _, key)| match key {
            None => true,
            Some(key) => !existing_keys.contains(key) && batch_keys.insert(key.as_str()),
        })
        .collect();
    let job_count = creates.iter().filter(|c| **c).count() as u64;

    let now = Utc::now();
    let mut job_ids: Vec<String> = Vec::with_capacity(valid.len());
    let mut statuses: Vec<String> = Vec::with_capacity(valid.len());
    let mut payloads: Vec<String> = Vec::with_capacity(valid.len());
    let mut priorities: Vec<i32> = Vec::with_capacity(valid.len());
    let mut scheduled_ats: Vec<Option<chrono::DateTime<Utc>>> = Vec::with_capacity(valid.len());
    let mut idempotency_keys: Vec<Option<String>> = Vec::with_capacity(valid.len());
    let mut indexes: Vec<i32> = Vec::with_capacity(valid.len());
    let mut max_payload_size: usize = 0;
    let default_priority = request.default_priority.unwrap_or(0);

    for ((index, job_item, key), creates) in valid.iter().zip(&creates) {
        let initial_status = if job_item.scheduled_at.is_some() && job_item.scheduled_at > Some(now)
        {
            "scheduled"
        } else {
            "pending"
        };
        let payload_json = serde_json::to_string(&job_item.payload).unwrap_or_default();
        if *creates {
            max_payload_size = max_payload_size.max(payload_json.len());
        }

        job_ids.push(Uuid::new_v4().to_string());
        statuses.push(initial_status.to_string());
        payloads.push(payload_json);
        priorities.push(job_item.priority.unwrap_or(default_priority));
        scheduled_ats.push(job_item.scheduled_at);
        idempotency_keys.push(key.clone());
        indexes.push(*index as i32);
    }

    // Plan checks, for the jobs this batch creates.
    if job_count > 0 {
        // SECURITY: Enforce per-plan payload size limit for bulk enqueue too.
        if let Err(response) = check_payload_size(state.db.pool(), &org_id, max_payload_size).await
        {
            return Err(AppError::LimitExceeded(Box::new(response)));
        }
        // Check job limits (daily + active) before creating
        if let Err(response) = check_job_limits(state.db.pool(), &org_id, job_count).await {
            return Err(AppError::LimitExceeded(Box::new(response)));
        }
    }

    let queue_defaults = queue_job_defaults(
        state.db.pool(),
        &state.settings.queue,
        &org_id,
        &request.queue_name,
    )
    .await?;
    if job_count > 0 && !queue_defaults.exists {
        if let Err(response) = check_new_queue_limit(state.db.pool(), &org_id, 1).await {
            return Err(AppError::LimitExceeded(Box::new(response)));
        }
    }
    let default_max_retries = request
        .default_max_retries
        .unwrap_or(queue_defaults.max_retries);
    let default_timeout = request
        .default_timeout_seconds
        .unwrap_or(queue_defaults.timeout_seconds);

    // Atomically reserve the daily-job quota for the WHOLE batch before inserting.
    // check_job_limits above is a fast non-atomic pre-check (and still guards the soft
    // active-jobs limit); this row-locked reservation is the authoritative daily cap,
    // closing the window where N concurrent bulk requests each passed the pre-check and
    // then all inserted, overshooting jobs_per_day (the single-enqueue path already does
    // this via try_increment_daily_jobs). All-or-nothing: a batch that would exceed the
    // cap is rejected before any job is created. A batch of replays only reserves nothing.
    let quota_reserved = if job_count == 0 {
        false
    } else {
        let daily_limit = daily_jobs_limit(state.db.pool(), &org_id)
            .await
            .unwrap_or(-1);
        match try_increment_daily_jobs(state.db.pool(), &org_id, job_count as i32, daily_limit)
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

    // A failed batch INSERT created nothing: give the reservation back and say so
    // with an error status (it was a 200 listing every item as failed).
    let rows = match results {
        Ok(rows) => rows,
        Err(e) => {
            tracing::error!(error = %e, "Bulk enqueue batch insert failed");
            refund_reserved_quota(&state, &org_id, quota_reserved, job_count as i64).await;
            return Err(job_insert_error(e));
        }
    };

    let mut succeeded = Vec::with_capacity(rows.len());
    let mut successful_count: u64 = 0;
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

    let success_count = succeeded.len();
    let failure_count = failed.len();

    // Update metrics and usage counters
    if successful_count > 0 {
        state.metrics.jobs_enqueued.inc_by(successful_count);
    }

    // Reconcile the daily counter so it reflects the jobs actually created: the
    // reservation covered the items expected to create a row; refund what a racing
    // replay took, and credit creations nothing was reserved for (the reservation
    // errored, or a key that existed a moment ago was gone by the INSERT).
    let reserved = if quota_reserved { job_count as i64 } else { 0 };
    let unused = reserved - successful_count as i64;
    if unused > 0 {
        refund_reserved_quota(&state, &org_id, true, unused).await;
    } else if unused < 0 {
        if let Err(e) = increment_daily_jobs(state.db.pool(), &org_id, (-unused) as i32).await {
            tracing::warn!(
                error = %e,
                org_id = %org_id,
                count = -unused,
                "Failed to increment daily job counter"
            );
        }
    }

    // Priority of a request item, for the job.created events below.
    let priority_of = |index: usize| {
        request
            .jobs
            .get(index)
            .and_then(|item| item.priority)
            .unwrap_or(default_priority)
    };

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
                    publish_realtime_event(
                        cache,
                        &org_id,
                        serde_json::json!({
                            "type": "JobCreated",
                            "data": {
                                "job_id": item.job_id,
                                "queue_name": request.queue_name,
                                "priority": priority_of(item.index),
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
                state.outgoing_webhooks.spawn_dispatch(
                    org_id.clone(),
                    "job.created".to_string(),
                    item.job_id.clone(),
                    serde_json::json!({
                        "job_id": item.job_id,
                        "queue_name": request.queue_name,
                        "priority": priority_of(item.index),
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
) -> AppResult<impl IntoResponse> {
    // Cap limit at 100 to prevent memory exhaustion
    let limit = query.limit.unwrap_or(50).clamp(1, MAX_JOBS_PER_PAGE);
    let offset = query.offset.unwrap_or(0).max(0);

    // Same filter, order and timeout rules as list(); `status` is fixed to
    // deadletter here.
    let filters = JobListFilters::parse(&query, false)?;
    let order = query.order().map_err(AppError::Validation)?;
    let scope = JobListScope {
        org_id: &ctx.organization_id,
        dead_letter_only: true,
        queues: ctx.queue_scope_filter(),
    };

    let (jobs, has_more) = list_job_page(&state, &scope, &filters, order, limit, offset).await?;

    let summaries: Vec<JobSummary> = jobs.into_iter().map(Into::into).collect();
    Ok(paged(summaries, has_more))
}

/// The dead-lettered jobs a DLQ retry looks at: the named ids, else one
/// queue's, else all of the org's.
#[derive(Clone, Copy)]
enum DlqScope<'a> {
    Ids(&'a [String]),
    Queue(&'a str),
    All,
}

impl DlqScope<'_> {
    /// ` FROM jobs j LEFT JOIN workflows w ... WHERE ...`: the org's
    /// dead-lettered jobs in this scope, each with its workflow (`w`, all NULL
    /// for a job outside any).
    fn push_from(self, qb: &mut QueryBuilder<Postgres>, org_id: &str) {
        qb.push(
            " FROM jobs j \
             LEFT JOIN workflows w \
               ON w.id = j.workflow_id AND w.organization_id = j.organization_id \
             WHERE j.status = 'deadletter' AND j.organization_id = ",
        );
        qb.push_bind(org_id);
        match self {
            DlqScope::Ids(ids) => {
                qb.push(" AND j.id = ANY(");
                qb.push_bind(ids);
                qb.push(")");
            }
            DlqScope::Queue(queue) => {
                qb.push(" AND j.queue_name = ");
                qb.push_bind(queue);
            }
            DlqScope::All => {}
        }
    }
}

/// The 'failed' workflows with dead-lettered steps in `scope` that a retry
/// may revive: at most `room`, those with the oldest step first.
async fn revivable_dlq_workflows(
    conn: &mut sqlx::PgConnection,
    org_id: &str,
    scope: DlqScope<'_>,
    room: u64,
) -> AppResult<Vec<String>> {
    if room == 0 {
        return Ok(Vec::new());
    }
    let mut qb: QueryBuilder<Postgres> = QueryBuilder::new("SELECT w.id");
    scope.push_from(&mut qb, org_id);
    qb.push(" AND w.status = 'failed' GROUP BY w.id ORDER BY MIN(j.created_at), w.id LIMIT ");
    qb.push_bind(i64::try_from(room).unwrap_or(i64::MAX));
    Ok(qb.build_query_scalar().fetch_all(&mut *conn).await?)
}

/// Dead-lettered steps in `scope` of 'failed' workflows outside `revivable`:
/// the jobs a retry leaves in the DLQ because of the max_workflows cap.
async fn dlq_jobs_held_by_the_workflow_cap(
    conn: &mut sqlx::PgConnection,
    org_id: &str,
    scope: DlqScope<'_>,
    revivable: &[String],
    limit: i64,
) -> AppResult<Vec<String>> {
    let mut qb: QueryBuilder<Postgres> = QueryBuilder::new("SELECT j.id");
    scope.push_from(&mut qb, org_id);
    qb.push(" AND w.status = 'failed' AND NOT (w.id = ANY(");
    qb.push_bind(revivable);
    qb.push(")) ORDER BY j.created_at ASC, j.id LIMIT ");
    qb.push_bind(limit);
    Ok(qb.build_query_scalar().fetch_all(&mut *conn).await?)
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

    // A retry adds to the active jobs, so that cap applies. It creates no new
    // job, so the daily cap does not: an org at its daily cap could not retry
    // its DLQ at all.
    if job_count > 0 {
        if let Err(response) = check_resource_limit(
            state.db.pool(),
            &ctx.organization_id,
            "active_jobs",
            job_count as u64,
        )
        .await
        {
            return Err(AppError::LimitExceeded(Box::new(response)));
        }
    }

    // One transaction for the retry and everything it re-opens (see retry()).
    // The workflows cap lock comes first, before any row lock.
    let mut tx = state.db.pool().begin().await?;
    lock_resource(&mut tx, &ctx.organization_id, "workflows").await?;

    // Explicit ids win over a queue name. Every pick includes organization_id.
    let scope = match (&request.job_ids, &request.queue_name) {
        (Some(ids), _) => DlqScope::Ids(ids),
        (None, Some(queue)) => DlqScope::Queue(queue),
        (None, None) => DlqScope::All,
    };
    let org_id = ctx.organization_id.as_str();

    // A dead-lettered step of a 'failed' workflow revives that workflow, and
    // reviving pays a max_workflows slot (see reopen_workflows). Pick only the
    // steps of as many such workflows as fit, oldest first, and leave the rest
    // in the DLQ, reported as skipped. Refusing the whole request instead let
    // one such step among the oldest jobs block every bulk retry until a slot
    // freed up. `None`: the cap is unlimited, every step can go.
    let revivable = match resource_room_conn(&mut tx, org_id, "workflows").await {
        Ok(None) => None,
        Ok(Some(room)) => Some(revivable_dlq_workflows(&mut tx, org_id, scope, room).await?),
        Err(response) => return Err(AppError::LimitExceeded(Box::new(response))),
    };

    // The ids are picked in a subquery, so the outer UPDATE checks the status
    // again: a job retried (or purged) by a concurrent request after the pick
    // must not be reset a second time.
    let mut qb: QueryBuilder<Postgres> = QueryBuilder::new(
        "UPDATE jobs SET status = 'pending', retry_count = 0, last_error = NULL, \
         updated_at = NOW() WHERE id IN (SELECT j.id",
    );
    scope.push_from(&mut qb, org_id);
    if let Some(revivable) = &revivable {
        qb.push(" AND (w.status IS DISTINCT FROM 'failed' OR w.id = ANY(");
        qb.push_bind(revivable.as_slice());
        qb.push("))");
    }
    qb.push(" ORDER BY j.created_at ASC, j.id LIMIT ");
    qb.push_bind(limit);
    qb.push(") AND organization_id = ");
    qb.push_bind(org_id);
    qb.push(" AND status = 'deadletter' RETURNING id, queue_name, workflow_id");
    let retried_rows: Vec<(String, String, Option<String>)> =
        qb.build_query_as().fetch_all(&mut *tx).await?;
    let retried_job_ids: Vec<String> = retried_rows.iter().map(|(id, _, _)| id.clone()).collect();
    let retried_count = retried_job_ids.len() as i64;

    let skipped_jobs = match &revivable {
        Some(revivable) => {
            dlq_jobs_held_by_the_workflow_cap(&mut tx, org_id, scope, revivable, limit).await?
        }
        None => Vec::new(),
    };
    if !skipped_jobs.is_empty() {
        if retried_job_ids.is_empty() {
            // Nothing else to retry: answer with the cap that holds them, as
            // POST /jobs/{id}/retry does for one such step.
            check_resource_limit_conn(&mut tx, org_id, "workflows", 1)
                .await
                .map_err(|response| AppError::LimitExceeded(Box::new(response)))?;
        }
        info!(
            org_id = %org_id,
            skipped = skipped_jobs.len(),
            "DLQ retry left steps of failed workflows the max_workflows cap cannot revive"
        );
    }

    if !retried_job_ids.is_empty() {
        // Remove the corresponding dead_letter_queue audit rows — the job is live
        // again, and nothing else ever deletes them (no FK), so they would dangle.
        sqlx::query(
            "DELETE FROM dead_letter_queue WHERE job_id = ANY($1) AND organization_id = $2",
        )
        .bind(&retried_job_ids)
        .bind(&ctx.organization_id)
        .execute(&mut *tx)
        .await?;

        // Reset child jobs that were blocked due to parent failure
        // When a parent job is retried from DLQ, its child jobs (with dependencies_met = false)
        // should have their dependencies recalculated
        // SECURITY: Must include organization_id to prevent cross-tenant modification
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
        .execute(&mut *tx)
        .await?;
        if reset_children.rows_affected() > 0 {
            info!(
                parent_jobs = ?retried_job_ids,
                reset_children = reset_children.rows_affected(),
                "Reset child jobs for retried DLQ jobs"
            );
        }
    }

    let reopened =
        reopen_cancelled_dependents(&mut tx, &ctx.organization_id, &retried_job_ids).await?;
    reopen_workflows(&mut tx, &ctx.organization_id, &retried_job_ids, &reopened).await?;
    tx.commit().await?;

    state.metrics.jobs_retried.inc_by(retried_count as u64);

    // One job.retried per job that left the DLQ, after the commit.
    for (job_id, queue_name, workflow_id) in &retried_rows {
        state.outgoing_webhooks.spawn_dispatch(
            ctx.organization_id.clone(),
            crate::outgoing_webhooks::events::JOB_RETRIED.to_string(),
            job_id.clone(),
            crate::outgoing_webhooks::events::job_retried_payload(
                job_id,
                queue_name,
                "deadletter",
                crate::outgoing_webhooks::events::RETRY_SOURCE_DLQ,
                workflow_id.as_deref(),
            ),
        );
    }

    Ok(Json(crate::models::RetryDlqResponse {
        retried_count,
        retried_jobs: retried_job_ids,
        skipped_jobs,
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

    // ALL queries now include organization_id for tenant isolation.
    // `older_than` compares the time the job was dead-lettered: `updated_at` of
    // a dead-lettered row (any later touch only moves it forward, so a purge never
    // removes a job dead-lettered after `older_than`). It compared `created_at`,
    // so an old job that had just been dead-lettered was purged at once. The outer
    // DELETE checks the status again, so a job retried after the pick survives.
    let result = match (&request.queue_name, &request.older_than) {
        (Some(queue), Some(older_than)) => {
            sqlx::query(
                r#"DELETE FROM jobs WHERE id IN (
                    SELECT id FROM jobs
                    WHERE status = 'deadletter' AND organization_id = $1 AND queue_name = $2 AND updated_at < $3
                    LIMIT $4
                ) AND organization_id = $1 AND status = 'deadletter'"#
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
                ) AND organization_id = $1 AND status = 'deadletter'"#
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
                    WHERE status = 'deadletter' AND organization_id = $1 AND updated_at < $2
                    LIMIT $3
                ) AND organization_id = $1 AND status = 'deadletter'"#
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
                ) AND organization_id = $1 AND status = 'deadletter'"#
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
            ..Default::default()
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
            ..Default::default()
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
            ..Default::default()
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

    fn some(value: &str) -> Option<String> {
        Some(value.to_string())
    }

    #[test]
    fn status_filter_takes_a_comma_list() {
        assert_eq!(status_filter(&None).unwrap(), Vec::<&str>::new());
        assert_eq!(status_filter(&some("")).unwrap(), Vec::<&str>::new());
        assert_eq!(status_filter(&some(" , ")).unwrap(), Vec::<&str>::new());
        assert_eq!(status_filter(&some("Pending")).unwrap(), vec!["pending"]);
        assert_eq!(
            status_filter(&some("failed, DEADLETTER,,failed")).unwrap(),
            vec!["failed", "deadletter"]
        );
        let err = status_filter(&some("failed,dead")).unwrap_err();
        assert!(
            matches!(&err, AppError::Validation(m) if m.contains("'dead'")),
            "{err:?}"
        );
    }

    #[test]
    fn escape_like_makes_wildcards_literal() {
        assert_eq!(escape_like("abc-123"), "abc-123");
        assert_eq!(escape_like("100%_off"), "100\\%\\_off");
        assert_eq!(escape_like("a\\b"), "a\\\\b");
    }

    #[test]
    fn rfc3339_filters_parse_and_reject() {
        let utc = |s: &str| {
            chrono::DateTime::parse_from_rfc3339(s)
                .unwrap()
                .with_timezone(&Utc)
        };
        assert_eq!(
            parse_rfc3339("2026-10-11T08:00:00Z"),
            Some(utc("2026-10-11T08:00:00Z"))
        );
        assert_eq!(
            parse_rfc3339("2026-10-11T10:00:00.5+02:00"),
            Some(utc("2026-10-11T08:00:00.5Z"))
        );
        // An unencoded `+` decoded to a space.
        assert_eq!(
            parse_rfc3339("2026-10-11T10:00:00 02:00"),
            Some(utc("2026-10-11T08:00:00Z"))
        );
        for bad in [
            "yesterday",
            "2026-10-11",
            "1728633600",
            "2026-10-11T10:00:00 0200",
            "é",
        ] {
            assert_eq!(parse_rfc3339(bad), None, "{bad}");
        }
        assert_eq!(time_filter("created_after", &some("  ")).unwrap(), None);
        assert!(matches!(
            time_filter("created_after", &some("soon")),
            Err(AppError::Validation(m)) if m.contains("created_after")
        ));
    }

    #[test]
    fn list_filters_validate_every_new_parameter() {
        let parse = |query: ListJobsQuery| JobListFilters::parse(&query, true).map(|_| ());
        assert!(parse(ListJobsQuery::default()).is_ok());
        assert!(parse(ListJobsQuery {
            q: some("  "),
            job_type: some(""),
            ..Default::default()
        })
        .is_ok());
        for bad in [
            ListJobsQuery {
                q: Some("x".repeat(256)),
                ..Default::default()
            },
            ListJobsQuery {
                q: some("a\0b"),
                ..Default::default()
            },
            ListJobsQuery {
                job_type: some("a\0b"),
                ..Default::default()
            },
            ListJobsQuery {
                created_before: some("not a time"),
                ..Default::default()
            },
            // An empty window.
            ListJobsQuery {
                created_after: some("2026-10-11T08:00:00Z"),
                created_before: some("2026-10-11T08:00:00Z"),
                ..Default::default()
            },
            ListJobsQuery {
                status: some("done"),
                ..Default::default()
            },
        ] {
            let shown = format!("{bad:?}");
            assert!(
                matches!(parse(bad), Err(AppError::Validation(_))),
                "{shown}"
            );
        }

        let query = ListJobsQuery {
            q: some(" 50%_Off "),
            status: some("failed,deadletter"),
            ..Default::default()
        };
        let filters = JobListFilters::parse(&query, true).unwrap();
        assert_eq!(filters.search.as_deref(), Some("50\\%\\_Off"));
        assert_eq!(filters.statuses, vec!["failed", "deadletter"]);
        // `/jobs/dlq` ignores `status`, as it always did.
        let query = ListJobsQuery {
            status: some("anything"),
            ..Default::default()
        };
        assert!(JobListFilters::parse(&query, false)
            .unwrap()
            .statuses
            .is_empty());
    }

    /// The SQL text `job_list_query` builds for `query`, with these job types
    /// resolved for `q`.
    fn list_sql(query: &ListJobsQuery, dead_letter_only: bool, types: &[&str]) -> String {
        let filters = JobListFilters::parse(query, !dead_letter_only).unwrap();
        let queues = vec!["billing-*".to_string()];
        let scope = JobListScope {
            org_id: "org",
            dead_letter_only,
            queues: Some(&queues),
        };
        let types: Vec<String> = types.iter().map(|t| t.to_string()).collect();
        let order = query.order().unwrap();
        job_list_query(&scope, &filters, &types, order, 50, 0)
            .sql()
            .as_str()
            .to_string()
    }

    // W8-R2: `q` never compares its text with a payload. Job types matching
    // it are probed through idx_jobs_org_job_type, one type at a time, and a
    // `job_type` filter is spelled so that the same index answers it.
    #[test]
    fn job_list_sql_reads_job_types_only_through_the_index() {
        let search = ListJobsQuery {
            q: some("Mail"),
            ..Default::default()
        };
        // `q` naming no job type: id prefix and queue name only.
        let sql = list_sql(&search, false, &[]);
        assert!(!sql.contains("payload"), "{sql}");
        assert!(sql.contains("(id ILIKE $"), "{sql}");
        assert!(!sql.contains("UNION"), "{sql}");

        // `q` naming job types: the two disjoint parts, each cut to the rows
        // that can reach the page, and the payload only as the index key.
        let sql = list_sql(&search, false, &["send_mail", "mailer"]);
        assert!(sql.contains(" UNION ALL "), "{sql}");
        assert!(sql.contains("CROSS JOIN LATERAL"), "{sql}");
        assert!(
            sql.contains(
                "payload->>'job_type' = t.job_type AND octet_length(payload->>'job_type') <= 255"
            ),
            "{sql}"
        );
        assert_eq!(sql.matches("payload").count(), 2, "{sql}");
        assert!(sql.contains("AND NOT (id ILIKE $"), "{sql}");
        // The scope and the order apply to both parts and to the page.
        assert_eq!(sql.matches("queue_in_key_scope").count(), 2, "{sql}");
        assert_eq!(
            sql.matches("ORDER BY created_at DESC, id DESC").count(),
            3,
            "{sql}"
        );
        // The DLQ keeps its fixed status in both parts.
        let sql = list_sql(&search, true, &["send_mail"]);
        assert_eq!(sql.matches("status = 'deadletter'").count(), 2, "{sql}");

        // `job_type` with `q`: the probe part is already that one type and
        // does not repeat the filter.
        let both = ListJobsQuery {
            q: some("mail"),
            job_type: some("send_mail"),
            ..Default::default()
        };
        let sql = list_sql(&both, false, &["send_mail"]);
        assert_eq!(sql.matches("payload->>'job_type' = $").count(), 1, "{sql}");

        // The exact filter repeats the index predicate.
        let exact = ListJobsQuery {
            job_type: some("send_mail"),
            ..Default::default()
        };
        let sql = list_sql(&exact, false, &[]);
        let filter = "AND payload->>'job_type' = $";
        let at = sql.find(filter).expect(&sql) + filter.len();
        let after_bind = sql[at..].trim_start_matches(|c: char| c.is_ascii_digit());
        assert!(after_bind.starts_with(JOB_TYPE_INDEXED), "{sql}");
    }

    #[test]
    fn a_cancelled_list_statement_is_a_clean_400() {
        // Any non-database error stays a database error.
        assert!(matches!(
            list_query_error(sqlx::Error::PoolTimedOut, 5000),
            AppError::Database(_)
        ));
        let response = AppError::QueryTimeout("x".to_string()).into_response();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[test]
    fn split_page_reports_a_following_row() {
        assert_eq!(split_page(vec![1, 2, 3], 2), (vec![1, 2], true));
        assert_eq!(split_page(vec![1, 2], 2), (vec![1, 2], false));
        assert_eq!(split_page(Vec::<i32>::new(), 2), (vec![], false));
    }
}
