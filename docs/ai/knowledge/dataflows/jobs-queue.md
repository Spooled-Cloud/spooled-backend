# Dataflow: Jobs / Queue

`GET /jobs` and `GET /jobs/dlq` summaries include `job_type` from `payload.job_type` (no column; empty string when absent) and `last_error` (null when none).

## Happy path

1. **Enqueue** — REST `POST /api/v1/jobs` (`handlers/jobs.rs`) or gRPC `Enqueue` (`grpc/services/queue_service.rs`) or `QueueManager::enqueue` (`queue/mod.rs`).
2. Job row `pending` with priority, payload, `max_retries`, `timeout_seconds`, optional `idempotency_key`.
3. **Claim / Dequeue** — worker claims via REST claim or gRPC `Dequeue` → `running` + lease.
4. **Complete** or **Fail** — fail increments `retry_count`; if `retry_count >= max_retries` → `deadletter` (+ DLQ audit insert on relevant paths).
5. **Lease recovery** — Scheduler `recover_expired_leases` (~10s) requeues (`retry_count < max_retries`) or deadletters exhausted leases. Both sweeps use `lease_expires_at <= NOW()` so the expiry instant is owned by recovery (worker ops require `>`).

## Defaults (verified)

Since **0.1.114** an omitted value comes from the target queue's `queue_config`
row first (`limits::queue_job_defaults`), then the server default. Before that,
`PUT /queues/{name}/config` `max_retries` / `default_timeout` never reached a job.

| Source | max_retries | timeout_seconds |
|--------|-------------|-----------------|
| REST omit (`POST /jobs`, bulk `default_*` omitted) | `queue_config.max_retries`, else `settings.queue.default_max_retries` ← env `QUEUE_DEFAULT_MAX_RETRIES` default `"3"` | `queue_config.default_timeout`, else `QUEUE_DEFAULT_TIMEOUT_SECS` default `"300"` |
| gRPC `<=0` / omit | same resolution | same |
| Custom webhook ingest | same resolution (falls back to settings if the lookup fails) | same |
| Schedules (resolved at create) / workflows omit | same resolution | same |
| DB column default | `DEFAULT 3` (initial schema) | — |

## Queue cap (implicit queues)

Queues are implicit: the first job into a name creates it. A queue *exists* when it
has a `queue_config` row, holds jobs, or is the target of an active schedule
(`get_org_queue_count`, migration `20260926120000`). Since **0.1.114** the plan's
`max_queues` is checked when REST create/bulk, gRPC Enqueue, schedule create or
workflow create would create a new queue (`limits::check_new_queue_limit*`), and
`PUT /queues/{name}/config` no longer charges a slot for a queue that already exists
implicitly. Webhook ingest and schedule fires are not capped (bouncing them only
causes sender retries). An org already over its cap keeps using its existing queues.
The check is not serialized under a lock, so concurrent first-enqueues into different
new names can overshoot by a few.

## Worker outcomes (REST complete / fail / heartbeat)

`classify_worker_miss` (`queue/mod.rs`): lease past expiry, or job requeued to
`pending`/`scheduled` → `LeaseExpired`; lease live but `lease_id` differs →
`LeaseMismatch` (still HTTP 409 `LEASE_EXPIRED` — the Go worker keys on that code —
with a "does not match" message); job already terminal → `AlreadyFinished` → 409
`CONFLICT` "Job is already completed" (was 404); not this worker's → 404.

Manual `POST /jobs/{id}/retry` resets `retry_count` to 0 and removes the job's
`dead_letter_queue` row, like `POST /jobs/dlq/retry` (it used to increment, leaving
`retry_count > max_retries`).

REST claim default lease = `WORKER_LEASE_DURATION_SECS` (30). A heartbeat that omits
`lease_duration_secs` renews for the length the lease was granted with (0.1.121, BEJOB-15): every claim
path (REST `CLAIM_BATCH_SQL` `queue/mod.rs:62`, gRPC `Dequeue`/`StreamJobs`/`ProcessJobs`) records the
granted length in `jobs.lease_duration_secs` (migration `20261013100200`), gRPC `RenewLease` records the
renewed one, and an explicit heartbeat value becomes the new length; NULL (claimed before the column, or
by a path that records nothing) falls back to the 30 s default. Renewal is `QueueManager::renew_lease_for`
(`queue/mod.rs:1205`) and uses the database clock. Before 0.1.121 the omitted value was always 30 s, which
cut a 600 s claim short; before 0.1.115 it was a 422. `lease_id` is a required `String` on complete, fail
and heartbeat (0.1.121, BEJOB-18): a missing one is a 422 `VALIDATION_ERROR`, not a 409 `LEASE_EXPIRED`.
gRPC has its
own default: an omitted/`<= 0` lease on `Dequeue`, `StreamJobs`, `ProcessJobs` and `RenewLease` is
`GRPC_DEFAULT_LEASE_DURATION_SECS` (default 300; `0` = unset, at most 3600, must exceed `WORKER_HEARTBEAT_INTERVAL_SECS`; `grpc/services/queue_service.rs:273`
`lease_secs`, `grpc/mod.rs:91` `lease_duration_or_default`). Since **0.1.119** it is a setting instead of
the hard-coded `DEFAULT_LEASE_DURATION_SECS`, and `Register` reports it (BEGRPC-10). It must not follow
the 30 s REST default: the Go SDK `StreamJobs` cannot send a lease (W3R-03).

REST validation allows `max_retries` range `min = 0` (`models/job.rs` ~214).

`PUT /queues/{name}/config` is an upsert, validated since 0.1.121 (BEJOB-04): the body through
`ValidatedJson` (`max_retries` 0-100, `default_timeout` 1-86400, `rate_limit` 1-100000, each a 400 outside
the range; `rate_limit` used to be clamped) and the path name through `validate_configurable_queue_name`
(`queues.rs:109`: 1-100 letters, digits, `-`, `_`, `.`, not starting with `.` or `-`; a legacy queue named
otherwise can still be read and deleted, not configured). Migration `20261013100100` clamps stored
out-of-range rows, then adds `queue_config_max_retries_check` / `queue_config_default_timeout_check`: a
stored `default_timeout` of 172800 used to make every enqueue to the queue a 500 on `jobs_timeout_check`.
Omitted `max_retries`/`default_timeout`/`enabled`/`rate_limit`/`settings` keep the existing row. Incoming `settings` objects are merged into the current JSON (pause metadata lives there). `rate_limit` (1-100000) is accepted, stored and returned but **not enforced by any claim path** (BEBILL-11 / BEJOB-12, operator decision; enforcement is F02): say "stored, not enforced yet" in docs, never that workers are throttled by it.

`DELETE /queues/{name}` without `delete_jobs` removes the `queue_config` row only and 409s while pending/processing jobs exist; with no config row but finished jobs it is a 409 that points at `?delete_jobs=true` (was a bare 404). `POST /queues/{name}/pause` on a queue that does not exist is 404 (it used to create a config row outside the queue cap); create-and-pause is `PUT .../config {"enabled": false}`. `?delete_jobs=true` deletes every job in that org+queue, matching `dead_letter_queue` rows, and the config (OpenAPI + dashboard checkbox).

`POST /jobs/{id}/dependencies` sets `dependencies_met` from `check_job_dependencies_met`. A SQL/decode error fails the request; it must not default to met.

`POST /jobs/bulk` has its own body limit (`api/mod.rs:194-213` `bulk_body_limit`): 100 jobs x
(largest plan payload cap + 4 KiB), clamped between axum's 2 MiB default and a 32 MiB ceiling
(`BULK_BODY_LIMIT_CEILING`); today that resolves to the 32 MiB ceiling. A body over it is refused
as 413 or `400 INVALID_JSON` (the extractor's length-limit error). Every other route keeps 2 MiB.
Before 0.1.119 a bulk of 100 jobs of 25 KB was refused (BEOPS-16).

## Idempotency

`ON CONFLICT (organization_id, idempotency_key) WHERE idempotency_key IS NOT NULL` in queue enqueue path.

0.1.121 (BEJOB-06, BEJOB-07, BEJOB-20 daily part, BEJOB-13):
- A blank or whitespace-only key is no key (`models::normalize_idempotency_key`, `job.rs:198`; single and
  bulk). `''` is a value to the unique index, so empty-key jobs used to collapse onto the first one. gRPC
  already treated an empty key as absent.
- A replay is answered before any payload, quota or expiry check: `POST /jobs` looks the key up first
  (`existing_idempotent_job`, `handlers/jobs.rs:163`) and answers `200 {id, created: false}`; bulk looks up
  the existing keys first and only the items that will create a row count toward the payload, active-job,
  daily and new-queue checks (a batch of replays reserves nothing). Keys are not tied to time: a key lives
  as long as its job row (a finished job is removed by the plan's job retention, failed and dead-lettered
  ones too since 0.1.121, and answers for its key until then). There is no 24-hour TTL, whatever older
  guides said.
- The daily quota is reserved (`try_increment_daily_jobs`) BEFORE the insert and refunded when no row is
  created, so a job over the cap is never inserted, claimable and deleted again. The active-jobs cap is
  still check-then-insert (BEJOB-20 remainder, open).

## Scheduler loops (in-process)

Activated scheduled jobs (~5s), cron (~10s), lease recovery (~10s), metrics (~15s), stale workers (~300s), dependency/workflow progress (~10s). See `scheduler/mod.rs`. Since 0.1.121 the dependency pass (`update_job_dependencies` `:942`) also moves a `pending` workflow to `running` and fills `started_at` once one of its jobs has started (`:1155`, BEFLOW-20; no trigger change, it never overwrites an outcome the progress trigger wrote), and both dead-lettering sweeps (`recover_expired_leases` `:258`, `cleanup_stale_workers` `:1251`) write the `deadlettered` job_history row and the DLQ row in one CTE and announce the failure through `announce_deadlettered` (`:382`): realtime `JobStatusChange` + `JobFailed`, the `job.failed` outgoing webhook (`data.status` `deadletter`) and the job's completion webhook, like the REST fail path (BEFLOW-11, QW7). Realtime publishing stops after its first failure in a batch so a hung Redis cannot stall the one scheduler loop. Every loop pass reports to `RuntimeHealth::scheduler_tick` (`scheduler/mod.rs:95`) and a drop guard marks it stopped however `run` ends (`:42`, `:76`); `/health` reports the scheduler down after 120 s without a pass.

Cron catch-up (`scheduler/mod.rs` ~395–418): if `next_run_at` is far in the past after downtime, **skip missed fires, run once now**, advance `next_run_at` via `next_run_after_in_timezone` (DST-aware). Live re-verified 2026-07-15 on `0.1.107`.

## Chesterton fence

gRPC maps proto3 zero to “use default 3” intentionally — explicit zero retries are **not** expressible on gRPC without a proto change or sentinel. Do not “fix” by treating 0 as zero retries without updating all SDKs and docs.

## Parent/child jobs and deletion (0.1.117)

`jobs.parent_job_id` is `ON DELETE SET NULL` (migration `20261010120000`, with a
partial index). Retention, expiry and DLQ purge used to cascade-delete children
in any status (BE-46). The AFTER DELETE trigger `"A_cancel_blocked_dependents_on_delete"`
cancels jobs still blocked on a deleted job that had not completed (scalar
`parent_job_id` or `job_dependencies` edge), for every delete path; its name
sorts before the `RI_ConstraintTrigger_*` FK actions so it still sees the links.
Without it the orphan sweep would release those jobs after 1 h. The expiry sweep
(`scheduler/mod.rs` cleanup) cancels children of an expiring parent first so it
can dispatch `job.cancelled` (reason `parent_expired`).

## Cron and DST (0.1.117)

`CronSchedule::next_run_after_tz` (`models/schedule.rs`): spring-forward wall
times never match. In a repeated fall-back hour, a schedule whose minute is
`*` / `*/N`, or whose hour is `*`, keeps real-time cadence; every other schedule
(including `0 */2 * * *`) fires on the first occurrence only. The repeat guard jumps straight past the reference wall clock.
It used to be read as a date mismatch and jump to the next midnight, silencing
hourly / `*/N` schedules for ~20 h every fall-back (BE-45). The date-mismatch
jump goes to the real next local midnight, so 23-hour days no longer skip
00:00–00:59 matches.

## Cron day fields and impossible dates (0.1.121)

`models/schedule.rs`. A day field is **restricted** unless it starts with `*`: `*` and `*/N` are both
unrestricted (Vixie cron; `day_fields_restricted` `:335`, BEFLOW-30). Day-of-month and day-of-week combine
with OR only when both are restricted, otherwise with AND, so `0 0 */2 * 1` is odd days that are Mondays.
`CronSchedule::parse` rejects a date that can never occur (`0 0 30 2 *`, `0 0 31 4 *`) through
`has_reachable_date` (`:352`; only a restricted day-of-month ANDed with an unrestricted day-of-week can be
impossible; February counts 29 days), `next_run_after_tz` answers `None` at once for one, and
`POST/PUT /schedules` answer 400 when no next run exists (`never_runs_response`, `handlers/schedules.rs:54`),
so a schedule is never stored with `next_run_at` NULL (BEFLOW-08). When a date field does not match, the loop
jumps to the next local midnight (DST-aware `until_next_local_midnight`) instead of stepping the time, so rare
schedules (Feb 29 on a Sunday) cost about one iteration per day. `GET /schedules` honours `queue_name`,
`is_active` (`true|false|1|0` any case, else 400) and `offset`, newest first with an `id DESC` tiebreak
(BEFLOW-18).

## Stale workers vs live leases (0.1.118)

`Scheduler::cleanup_stale_workers` (`scheduler/mod.rs:1049`) marks workers offline after 2 min without a
worker heartbeat (since 0.1.119 `healthy`, `degraded` and `draining` alike, `:1056`; a crashed
draining worker used to stay `draining` for ever), but only releases or dead-letters their jobs once the job
lease has expired (`lease_expires_at IS NULL OR <= NOW()`). A live lease means a
worker is still renewing it (BEJOB-01: the PHP SDK sent no worker heartbeat
during a long handler and its jobs were re-run). `QueueManager::renew_lease`
also refreshes `workers.last_heartbeat` on success.

## Failure text (0.1.118)

`models::normalize_failure_message` (REST `/fail` and gRPC `Fail`): empty →
"Job failed", over `MAX_FAILURE_MESSAGE_BYTES` (4096) → truncated with
`FAILURE_TRUNCATION_SUFFIX`. Never a 400 (BEJOB-11).

## Draining workers and the worker cap (0.1.119)

`max_workers` counts a worker via the SQL function `worker_counts_toward_cap(status, last_heartbeat)`
(migration `20261011150300_count_draining_workers.sql`): healthy, degraded, or draining with a heartbeat
in the last 2 minutes. Before, a worker that heartbeated `status = 'draining'` dropped out of
`get_org_resource_counts` and an org could run any number of workers (BEGRPC-15). Visible effects:
a new REST or gRPC registration is refused (429 / `RESOURCE_EXHAUSTED`) when the cap is full of live
draining workers; `GET /organizations/usage` includes them; a live draining worker can still re-register
its own id at the cap. Claim/dequeue still does not require a registered worker (deferred, owner decision).

## Process supervision, shutdown and `/health` (0.1.119)

- Startup: `GrpcServer::bind` runs before anything is spawned (`main.rs:121`), so a busy port or a
  bad/missing PEM exits the process with `gRPC server failed to start on <addr>`. The metrics server, the
  gRPC server and the scheduler each run under `supervise` (`main.rs:331`); if any ends or panics before
  shutdown the process logs it, runs the normal bounded shutdown and exits 1, so the restart policy
  brings it back (BEGRPC-14, BEOPS-09). A `METRICS_PORT` clash is therefore a restart loop, by design.
- Shutdown: SIGTERM/Ctrl+C or a task failure sets the shutdown watch. gRPC health turns NOT_SERVING,
  then tonic drains alongside axum for `GRPC_DRAIN_TIMEOUT` = 10 s (`grpc/server.rs:61`); the scheduler
  finishes its pass; the metrics server is aborted. Everything together, HTTP drain included, is bounded
  by `SHUTDOWN_TIMEOUT` = 30 s (`main.rs:38`), then `release_local_jobs` and `db.close()` run. Compose sets
  `stop_grace_period: 40s` and `k8s/base/deployment.yaml:332` `terminationGracePeriodSeconds: 40`
  (was 30, shorter than the bound); `main.rs` `tests::orchestrators_wait_past_the_shutdown_timeout` keeps
  both at least `SHUTDOWN_TIMEOUT` + 10 s. Open StreamJobs/ProcessJobs streams and SSE streams have no
  shutdown hook; they hold the drain until their timeout (open, BEOPS-24).
- `GET /health` (`api/handlers/health.rs:93`): 503 with the same body when the database check fails
  (`:113`); Redis down is 200 with `cache:false`, `status:"degraded"`. `grpc` and `scheduler` are optional
  booleans from the `RuntimeHealth` extension (`:26`; omitted in router-only tests); either false makes
  `status` degraded; scheduler is false after `SCHEDULER_STALL_AFTER_SECS` = 120 (`:20`). `/health/live`
  and `/health/ready` are unchanged. The compose/Docker `HEALTHCHECK curl -f /health` therefore fails while
  the DB is down (intended).
- Dashboard `system.uptime_seconds` / `started_at` come from `state.start_time` (set when `AppState` is
  built), not from the first dashboard request (BEOPS-22).
- Redis (`cache/mod.rs`): every call, pipeline, Lua script and the health PING goes through
  `RedisCache::bounded` (`:192`), capped at `COMMAND_TIMEOUT` 500 ms (`:43`) including any reconnect
  wait. `TimeoutTracker` (`:63`) replaces the `ConnectionManager` after
  `TIMEOUTS_BEFORE_RECONNECT` = 3 timeouts in a row (`:50`) or a failed health PING, at most once per
  `MIN_RECONNECT_INTERVAL` 1 s (`:54`), because redis' `ConnectionManager` only reconnects on I/O errors and a
  response timeout is `TimedOut` (a half-open socket was never replaced). Sockets use TCP keepalive
  30 s / 10 s (`:58`); `subscribe()` is capped at 2 s (`:47`). Any call slower than 500 ms now fails into the
  caller's fail-open/fail-closed path (a slow Lua script, a SCAN round trip on a huge keyspace).
- Metrics: `track_http_metrics` (`observability/mod.rs:293`, outermost layer in `api::router`) feeds
  `spooled_api_requests_total{method,route,status}` and `spooled_api_request_duration_seconds{method,route}`
  with the matched route template (`unmatched` for 404s); the pool is attached in `AppState::new` and
  sampled into `spooled_database_connections_active` / `_max` on each scrape. REST fail increments
  `jobs_failed` (and `jobs_deadlettered` on dead-letter, `handlers/jobs.rs:710`); REST complete
  (`handlers/jobs.rs:601`) and gRPC complete (`queue_service.rs:879`) observe `job_duration`.
  `spooled_redis_operations_total` no longer exists. gRPC Enqueue/Dequeue still do not update the
  `jobs_pending` gauge (the scheduler corrects it every 15 s; open, BEGRPC-25 residual).
- `POST /workflows` checks the queue cap on the transaction's own connection
  (`limits::check_new_queue_limit_conn`, `limits.rs:436`; call site `handlers/workflows.rs:432`). A second
  pool connection there deadlocked the pool when pool-size creates waited on the advisory lock (BEOPS-10).
- Migrator (`db/mod.rs` `run_migrator`): `set_ignore_missing(true)`, so an older image boots after a
  rollback; migrations run over a one-connection pool on `DATABASE_DIRECT_URL` when set. `build.rs` emits
  `rerun-if-changed=migrations`. Residual: an accidentally deleted local migration file is no longer
  noticed either.

## Retention sweeps (0.1.121)

`Scheduler::cleanup_old_data_by_org` (`scheduler/mod.rs:1453`) runs every 5 min inside the scheduler loop,
so every statement is a bounded batch (`RETENTION_JOB_BATCH` = 5,000 jobs per statement, `:37`).
- Order: old workflows first, then completed/cancelled jobs, then failed/dead-lettered jobs, so a purged
  workflow's steps go in the same pass.
- Workflow, job and worker deletes pick their rows `FOR UPDATE ... SKIP LOCKED`: the subselect re-checks
  status and age against the newest row version and skips rows an open transaction holds. A plain
  `DELETE ... WHERE id IN (SELECT ...)` deleted a job a retry had just moved back to pending (W5-R4).
- A failed, dead-lettered or cancelled job that belongs to a workflow that still exists is kept,
  whatever the workflow's status: a failed workflow can be retried until workflow retention removes it,
  and a deleted step takes its dependency edges along, so the retry ran its dependents ungated (W5-R5).
- `schedule_runs.job_id` (ON DELETE SET NULL) is indexed (`idx_schedule_runs_job_id`, migration
  `20261013140000`); without it every deleted job scanned all of schedule_runs (27 ms a job at 450k runs:
  55 s for 2,000 dead-lettered jobs, now 0.18 s). The schedule_runs sweep (`SCHEDULE_RUNS_RETENTION_SQL`,
  `:48`) ranges over `idx_schedule_runs_schedule_started` (`20261013140100`) per schedule (W5-R6).
- What each statement removes (BEOPS-11, BEFLOW-09; before 0.1.121 only completed/cancelled jobs, job_history,
  deliveries, email codes and workflows were swept):
  - workflows `completed|failed|cancelled` past `job_retention_days` (`LIMIT 5000`);
  - jobs `completed|cancelled` past `job_retention_days`, 5,000 per pass; a `cancelled` step of a workflow
    that still exists stays (a workflow retry resets it);
  - jobs `failed|deadletter` past `job_retention_days` by `updated_at` (the failure time), 5,000 per pass,
    never while a `pending|scheduled|processing` job waits on them (a `job_dependencies` edge or
    `parent_job_id` child) and never while their workflow row exists, whatever its status;
  - `dead_letter_queue` rows past `job_retention_days` by `created_at` (10,000 per pass; they hold a payload
    copy and were only ever emptied by an explicit purge);
  - `schedule_runs` past `history_retention_days` by `started_at` (10,000 per pass, per schedule);
  - workers `offline` with `last_heartbeat` over 7 days old and no `processing` job assigned (10,000 per
    pass, not plan-dependent);
  - then the existing history, delivery, email-code and `expires_at` sweeps.
  - Retention days are Free 3 / Starter 14 / Pro 30 / Enterprise 90 for jobs and 1 / 7 / 30 / 90 for history,
    overridable through `custom_limits` read by `custom_retention_days()` only.
- The first passes after a deploy work off the backlog gradually, one batch per statement every 5 minutes
  (about 1.4M jobs a day per statement). Counts are logged at `debug`.
- Still open: the `expires_at` sweep (`cleanup_old_data_by_org`, last statement) deletes `pending`,
  `scheduled`, `failed` and `deadletter` jobs past `expires_at` without looking at their workflow, so an
  expiring workflow step still takes its dependency edges with it and the 1 h orphan sweep then releases its
  dependents (BEFLOW-05; the cancel-on-expire path covers `parent_job_id` children only).

## Jobs API semantics (0.1.121)

- **List filters are strict** (`handlers/jobs.rs`): `queue_filter` `:70`, `status_filter` `:80` (case-insensitive),
  `tag_filter` `:102` (any characters, at most 1,024 bytes, no NUL; it used to be `[A-Za-z0-9_.-]` up to 64)
  turn an invalid value into a 400 `VALIDATION_ERROR` for `GET /jobs`, `GET /jobs/dlq` and `GET /jobs/stats`
  (`queue_name`); they used to log and return the unfiltered list. An empty parameter is no filter (BEJOB-09).
- **Order** is `ListJobsQuery::order()` (`models/job.rs:571`): `order_by` from `ALLOWED_ORDER_BY_FIELDS`
  (`created_at`, `updated_at`, `scheduled_at`, `completed_at`, `priority`, `status`, `queue_name`,
  `retry_count`), `order_dir` `asc|desc`, both case-insensitive, an unknown value a 400; `push_job_order`
  (`jobs.rs:119`) appends `id` in the same direction so OFFSET pages are stable, `NULLS LAST` for the two
  nullable timestamps (BEJOB-10, BEJOB-14). Pagination is `limit` (default 50, max 100) + `offset`; there is no
  cursor (BEJOB-13).
- **Bulk** (`bulk_enqueue` `:1458`): each item is validated with `BulkJobItem::validate()` (not `nested`); invalid
  items go to `failed[]` as `{index, error}` and the valid ones are created. A 400 (not 200) when no item is
  valid (message lists `jobs[i]: reason`), a batch default is out of range (`default_priority` -100..100,
  `default_max_retries` 0..100, `default_timeout_seconds` 1..86400) or the database refuses a value
  (`job_insert_error`: SQLSTATE class 22 or 23514); other insert errors are 500 after the quota reservation is
  refunded. A payload or tags containing U+0000 is refused up front (`contains_nul`), because `jsonb` rejects it
  (BEJOB-05).
- **`expires_at`** must be in the future and later than `scheduled_at` (`validate_expiry` `:178`); such a job used
  to be accepted, charged and never claimable (BEJOB-23).
- **Queue stats** (`queues.rs:298`): `failed_jobs_24h` counts `failed|deadletter` rows by `updated_at` (dead-lettering
  sets no `completed_at`; no job is ever `failed`), `deadletter_jobs` is the current DLQ count (BEJOB-08, QW5).
- **DLQ** (`retry_dlq` `:2021`, `purge_dlq` `:2228`): retry checks only the `active_jobs` cap (a retry creates no
  daily job); purge `older_than` compares `updated_at`, the dead-letter time; both outer statements re-check
  `status = 'deadletter'` and the organization (BEJOB-22).
- **Claim predicate vs `idx_jobs_claim`** (migration `20261013100000`): `(organization_id, queue_name, priority DESC,
  created_at ASC) WHERE status IN ('pending', 'scheduled')`. The planner only uses a partial index whose predicate
  the query implies, so `CLAIM_BATCH_SQL` and the gRPC claim queries must keep equalities on organization and queue, a
  status list within `('pending', 'scheduled')` and that sort order (extra filters such as `scheduled_at`,
  `expires_at`, `dependencies_met` are fine); a claim test runs `EXPLAIN` on the exact constant (BEJOB-03). The migration is a `-- no-transaction`
  file with ONE statement and no `SET lock_timeout` (sqlx sends the file as one simple query, which Postgres runs as
  an implicit transaction block that refuses `CREATE INDEX CONCURRENTLY`); `20261013100001` rebuilds the index if an
  interrupted build left it `INVALID` (`IF NOT EXISTS` would skip it for ever). Same pair for `schedule_runs`.
- `jobs_updated_at` is a BEFORE UPDATE trigger that stamps `updated_at` on every UPDATE: tests that backdate rows
  must disable it, and anything that reads `updated_at` as "when it failed" (stats, purge, retention) relies on a
  terminal row not being touched again.

## Retry re-opens what the failure cancelled (0.1.121)

`POST /jobs/{id}/retry` (`:1289`) and `POST /jobs/dlq/retry` (`:2021`) run in one transaction (BEFLOW-04):
1. `lock_resource(org, 'workflows')` first (single retry: only for a job that belongs to a workflow; DLQ retry: always), then row locks, the order `POST /workflows` and `/workflows/{id}/retry` use;
2. reset the job(s) to `pending`, `retry_count = 0`, delete their DLQ rows;
3. `reopen_cancelled_dependents` (`:1130`) re-opens, level by level (`MAX_REOPEN_DEPTH` 100), children cancelled with
   `last_error` in `DEPENDENCY_CANCEL_REASONS` (`models/job.rs:141`: `DEPENDENCY_FAILED_CANCEL_REASON` and
   `PARENT_FAILED_CANCEL_REASON`, the constants the scheduler sweep writes - one definition for both sides, never
   a duplicated string), gated (`dependencies_met` recomputed), skipping a cancelled workflow's steps and any
   dependent another dependency still dooms; a job someone cancelled is not touched;
4. `reopen_workflows` (`:1225`) puts a `failed` workflow back to `running` and recomputes its counters; reviving pays a
   `max_workflows` slot (`check_resource_limit_conn`), so a single-job retry answers 429 `QUOTA_EXCEEDED` /
   403 `feature_disabled` when no slot is free (by design, W5-R3: skipping the charge reopens the W4-01 bypass).
   `retry_dlq` instead reads the room (`resource_room_conn`), revives the oldest failed workflows that fit, leaves the
   other steps in the DLQ and lists them in `skipped_jobs`; it answers the cap's error only when nothing else could be
   retried.

Design note (W5-R1, P3, reviewed, no change): a queue-scoped key's retry re-opens sweep-cancelled dependents in queues
outside its scope and revives the workflow; the key could already trigger the opposite cascade, the dependents return
gated and unchanged, and none of their data is returned.

## Workflows (0.1.121)

- Progress trigger `update_workflow_progress()` (migration `20261013100300`, runs when a job becomes `completed|failed`,
  only on `pending|running` workflows): `running` while any job is `pending|scheduled|processing`, else `failed` if any
  is `failed|deadletter`, else `cancelled` if any is `cancelled`, else `completed`. A cancelled step no longer lets a
  workflow end `completed` (BEFLOW-04). BEFLOW-03 (concurrent last completions can leave the workflow `running`) is
  untouched.
- `POST /workflows/{id}/cancel` (`workflows.rs:825`) reads the status under the row lock and answers 409 for a
  `completed|failed|cancelled` workflow (BEFLOW-21; still open: no `job.cancelled` webhooks for the jobs it cancels, and
  `processing` jobs keep running). `POST /workflows` rejects a repeated key in one job's `depends_on` with 400 before the quota
  reservation (BEFLOW-16). `GET /workflows/{id}` jobs come from `workflow_job_response` (`:237`): `job_type` is
  `payload.job_type` else `"job"`, `failed_at` is `updated_at` for `failed|deadletter` jobs, `backoff_type` is always
  `exponential` (BEFLOW-19).
- `POST /jobs/{id}/dependencies` (`:989`) checks every edge before writing (self edge and cycles are 400), writes
  `dependency_mode` / `dependencies_met` only when an edge was inserted, and an omitted mode keeps the job's own
  (`AddDependenciesRequest.dependency_mode` is an `Option`) (BEFLOW-22).
