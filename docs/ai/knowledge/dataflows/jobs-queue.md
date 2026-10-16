# Dataflow: Jobs / Queue

`GET /jobs` and `GET /jobs/dlq` summaries (`JobSummary`, `models/job.rs`) include `job_type` from `payload.job_type` (no column) and `last_error` (null when none). `job_type` is `""` when the payload names no type; the published SDKs rely on that (QAV2-L3, kept in 0.1.125). `GET /workflows/{id}` still falls back to `"job"`.

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

REST `fail` (`handlers/jobs.rs:1162`) takes the status of its `job.failed` event (and so of the implied `job.deadlettered`), of the realtime events and of the metrics from the transition itself (`outcome.new_status`, `:1211`), not from a SELECT after the commit (0.1.122): by then a scheduled retry may have been claimed again or a dead-lettered job retried, and the event was skipped or carried the wrong status.

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
- The unique index is `idx_jobs_idempotency` on the unpartitioned `jobs` table
  (`migrations/20241209000001_initial_schema.sql:86`). The unused `jobs_partitioned` table of migration
  `20241209000009` has `(organization_id, idempotency_key, created_at)`, which is not unique per key (a
  unique index on a partitioned table must contain the partition key). Swapping that table in as `jobs`, as
  the migration's own notes suggest, leaves the `ON CONFLICT` statements above without an index to infer
  and stops duplicate keys from being detected; it also lacks columns, foreign keys, CHECKs and triggers.
  Deferred (BEJOB-17); see chesterton fence #33 and the warning in `docs/guides/operations.md`.

## Scheduler loops (in-process)

Activated scheduled jobs (~5s), cron (~10s), lease recovery (~10s), metrics (~15s), stale workers (~300s), dependency/workflow progress (~10s). See `scheduler/mod.rs`. Since 0.1.121 the dependency pass (`update_job_dependencies` `:942`) also moves a `pending` workflow to `running` and fills `started_at` once one of its jobs has started (`:1155`, BEFLOW-20; no trigger change, it never overwrites an outcome the progress trigger wrote), and both dead-lettering sweeps (`recover_expired_leases` `:258`, `cleanup_stale_workers` `:1251`) write the `deadlettered` job_history row and the DLQ row in one CTE and announce the failure through `announce_deadlettered` (`:382`): realtime `JobStatusChange` + `JobFailed`, the `job.failed` outgoing webhook (`data.status` `deadletter`) and the job's completion webhook, like the REST fail path (BEFLOW-11, QW7). Realtime publishing stops after its first failure in a batch so a hung Redis cannot stall the one scheduler loop. Since 0.1.122 the dependency pass ends with `announce_workflow_outcomes` (`scheduler/mod.rs:1249`, QW8): one claiming `UPDATE` announces the `completed`/`failed` workflows whose `completed_at` it has not sent yet, **at most once** (see `dataflows/webhooks.md`, invariants #41). Every loop pass reports to `RuntimeHealth::scheduler_tick` (`scheduler/mod.rs:95`) and a drop guard marks it stopped however `run` ends (`:42`, `:76`); `/health` reports the scheduler down after 120 s without a pass.

Cron catch-up (`scheduler/mod.rs` ~395–418): if `next_run_at` is far in the past after downtime, **skip missed fires, run once now**, advance `next_run_at` via `next_run_after_in_timezone` (DST-aware). Live re-verified 2026-07-15 on `0.1.107`.

Known costs at scale (BEJOB-21, **deferred**, measured at 320k `jobs` rows; production held about 130k on 2026-10-12): the dependency pass `update_job_dependencies` (`scheduler/mod.rs:942`) runs three `UPDATE jobs child ... AND (child.dependencies_met = FALSE OR child.dependencies_met IS NULL)` statements (`:954`, `:988`, `:1049`) every 10 s, about 47 ms each, because the `IS NULL` arm cannot use the partial index `idx_jobs_dependencies_met` (`WHERE dependencies_met = FALSE`); `dequeue_batch` (`queue/mod.rs:277`) awaits one `record_history` INSERT per claimed job, 100 for a full batch; the implicit-queue backfill of `GET /queues` (`handlers/queues.rs:50`) and `GET /jobs/stats` (`handlers/jobs.rs:1845`) read the whole job range of the organization. Nothing is user-visible at today's size; the fix is scheduled after the SDK wave (findings entry has the directions). Do not "optimise" these statements casually: the workflow sweep's loop invariants (stated in the code comments of `update_job_dependencies`) and the claim order are load-bearing.

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
can dispatch `job.cancelled` (reason `parent_expired`). Since 0.1.128 the trigger and the sweeps honor `dependency_mode = 'any'` (see "Round-2 fixes").

## Cron and DST (0.1.117)

`CronSchedule::next_run_after_tz` (`models/schedule.rs`): spring-forward wall
times never match. In a repeated fall-back hour, a schedule whose minute is
`*` / `*/N`, or whose hour is `*`, keeps real-time cadence; every other schedule
(including `0 */2 * * *`) fires on the first occurrence only. The repeat guard jumps straight past the reference wall clock.
It used to be read as a date mismatch and jump to the next midnight, silencing
hourly / `*/N` schedules for ~20 h every fall-back (BE-45). The date-mismatch
jump goes to the real next local midnight, so 23-hour days no longer skip
00:00–00:59 matches.

## Schedule history follows the job (0.1.125, QAV2-M2)

`GET /schedules/{id}/history` (`schedules.rs` `history`, `effective_run_outcome`) joins each `schedule_runs` row to the job it
enqueued (`LEFT JOIN jobs ... AND organization_id`) and answers `status` / `completed_at` from the job: `pending|scheduled` ->
`pending` (null), `processing` -> `running` (null), `completed` -> `completed`, `failed|deadletter|cancelled` -> `failed`; the
terminal ones carry the job's `completed_at` (the run's when the job has none). A run without a job (`job_id` NULL: nothing was
enqueued, or retention deleted the job, `ON DELETE SET NULL`) keeps the stored status and time. The stored `schedule_runs.status`
is unchanged: the scheduler and the manual trigger still insert `completed` with `completed_at = started_at` the moment the job
exists (it means "enqueued"), `failed` when the enqueue was refused. The response uses only `pending|running|completed|failed`
(SDK enums); `job_status` stays the raw job status. Nothing reads the stored status except this handler.

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

## Job lifecycle fixes (0.1.126)

- **A retry waits as `scheduled`** (R0126-JOBS-05). `fail_by_worker`, the legacy `fail`, gRPC `Fail` and ProcessJobs fail store
  `status = 'scheduled'` with `scheduled_at = now + backoff` (`FailByWorkerOutcome.new_status`, `will_retry` = status `scheduled`); the
  scheduler's activation (`status = 'scheduled' AND scheduled_at <= NOW()` -> `pending`) and claim (`status IN ('pending','scheduled')`) are
  unchanged. List filters and stats read the status, so a waiting retry is `scheduled` everywhere. Do not store `pending` with a future
  `scheduled_at` again.
- **Deregister keeps a live lease** (R0126-JOBS-01). `queue::release_jobs_for_deregistered_worker` (REST and gRPC) releases only
  `lease_expires_at IS NULL OR <= NOW()` jobs with `retry_count < max_retries`, as `retry_count + 1`, `last_error = 'Worker deregistered'`
  and a `retry_scheduled` history row; a live lease stays `processing` for the worker or the expiry sweep. Same rule as
  `cleanup_stale_workers` above.
- **Requeues write history** (R0126-JOBS-04). `recover_expired_leases` and `cleanup_stale_workers` are CTEs that insert a
  `retry_scheduled` `job_history` row (`error`, `retry_count`, `organization_id`) for each requeued job, like their dead-letter halves.
- **`dependencies_met` is recomputed, not forced** (R0126-JOBS-02). `complete_with_worker_and_org`, `complete_by_worker` and
  `update_job_dependencies` set `dependencies_met = check_job_dependencies_met(id)` (only `IS DISTINCT FROM`), never TRUE from
  `parent_job_id` alone; a blocked child becomes FALSE because NULL means claimable.
- **Workflow progress sees dead-letter and cancel** (R0126-JOBS-03). Migration `20261015120100` replaces `update_workflow_progress`
  (guard `NEW.status IN ('completed','failed','deadletter','cancelled')`); terminal workflows stay terminal; no backfill.
- **Queue names are claimable everywhere a job is inserted** (R0126-LIVE-01, -02). `models::is_claimable_queue_name` (1-100 characters,
  `validate_queue_name_chars`) guards schedule trigger (400) and the cron tick (a `failed` `schedule_runs` row, `next_run_at` advanced);
  `CreateScheduleRequest`, `WorkflowJobDefinition` and `CustomWebhookRequest` are 1-100 characters, and
  `CreateWorkflowRequest.jobs` runs `validate_workflow_job_queue_names` (queue rules only; deliberately not `nested`, which would newly enforce key/priority/retry rules that never ran). Any new insert path must use these.
- Tests: `r0126_jobs_realtime_tests::jobs01..05_*`, `live01..03_*`.

## Round-2 fixes (0.1.128)

- **Idempotency is org-wide, the answer is scoped** (R0128-FLOW-06, RT-01). `existing_idempotent_job` returns id and queue; a hit outside the
  key's queue scope is `409` ("Idempotency key is already in use", `models::IDEMPOTENCY_KEY_IN_USE`), never the id. `POST /jobs/bulk`
  refuses the whole request. The insert's `ON CONFLICT ... DO UPDATE` carries `WHERE queue_in_key_scope(jobs.queue_name, $n)`, so a race
  returns no row and writes nothing; the caller refunds its quota reservation. The unique index is unchanged.
- **One lock opens a queue** (R0128-ORG-02). Every path that can make a queue new takes `lock_resource(..., "queues")` and re-reads
  `queue_name_exists` (`queue_config`, any job, an active schedule) before `check_new_queue_limit_*_conn`: `POST /jobs`, bulk, gRPC
  `Enqueue`, schedule create, workflow create, and `cron_new_queue_refusal` in the cron tick. A queue that already exists takes no lock.
  A new path that inserts jobs for an unseen queue name must do the same.
- **`any` is honored on every cancel path** (R0128-FLOW-02, SCHED-03). A child with `dependency_mode = 'any'` is cancelled only when no
  other dependency is outside failed/deadletter/cancelled: the `parent_job_id` failure sweep, the expiry sweep, and the delete trigger
  (migration `20261016120000` replaces `cancel_blocked_dependents_of_deleted_job`; it locks the dependents first, found through the two indexed lookups as a UNION (an OR would scan the organization's pending backlog per deleted row), `FOR NO KEY UPDATE` in id order).
  DLQ retry resets a child's `dependencies_met` with `check_job_dependencies_met` for `any` children, not FALSE.
- **Expiry sweep announces and finishes** (R0128-SCHED-04, FLOW-01). Before the delete it cancels `job_dependencies` children
  (`last_error` "Dependency job <id> expired before it completed") and dispatches `job.cancelled` (`dependency_expired`; `parent_expired`
  for `parent_job_id` children). After the delete it finishes each affected `pending`/`running` workflow with no active job left
  (failed if a step failed, cancelled if a step was cancelled or removed or none is left). Workflow create rejects a past `expires_at`.
- **Retries of cancelled work** (R0128-FLOW-03, -04). Workflow retry reopens failed/deadletter steps and cancelled steps whose `last_error`
  is in `DEPENDENCY_CANCEL_REASONS`. `POST /jobs/{id}/retry` is 409 for a job of a `cancelled` workflow; DLQ retry skips those jobs.
- **Workflow create limits** (R0128-LIVE3-01, -04). `validate_workflow_job_limits` on `jobs`: priority -100..100, `max_retries` 0..100,
  `timeout_seconds` 1..86400, `expires_at` in the future (400). Not `nested`: job key rules still do not run. The handler's clamps now
  bound only defaults. `progress.cancelled` is separate from `progress.failed` (R0128-FLOW-07).
- **Cron next run** (R0128-SCHED-02): after the insert, `next_run_after_clock` from `clock_timestamp()`; failure paths other than "queue not
  claimable" still advance from the batch clock. `PUT /schedules/{id}` checks the plan payload cap for a supplied template (SCHED-01).
- **`GET /dashboard`** (R0128-DASH-29, -32, -35): `jobs.scheduled`; workers total = all, healthy = `healthy` with a heartbeat under 2 minutes;
  queues from `queue_config` plus implicit queues, paused first, LIMIT 20.
- Tests: `r0128_flow_tests`, `r0128_sched_tests`, `r0128_jobs_tests`, `r0128_followup_tests`, `r0128_live3_tests`, `r0128_dash_tests`.

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

- **List filters are strict** (`handlers/jobs.rs`): `queue_filter` `:70`, `status_filter` `:82` (case-insensitive),
  `tag_filter` `:503` (any characters, at most 1,024 bytes, no NUL; it used to be `[A-Za-z0-9_.-]` up to 64)
  turn an invalid value into a 400 `VALIDATION_ERROR` for `GET /jobs`, `GET /jobs/dlq` and `GET /jobs/stats` (0.1.122: `status` is a comma list now, `status_filter` `:82` returns `Vec<&str>`; see "Job lists" below)
  (`queue_name`); they used to log and return the unfiltered list. An empty parameter is no filter (BEJOB-09).
- **Order** is `ListJobsQuery::order()` (`models/job.rs:585`): `order_by` from `ALLOWED_ORDER_BY_FIELDS`
  (`created_at`, `updated_at`, `scheduled_at`, `completed_at`, `priority`, `status`, `queue_name`,
  `retry_count`), `order_dir` `asc|desc`, both case-insensitive, an unknown value a 400; `push_job_order`
  (`jobs.rs:520`) appends `id` in the same direction so OFFSET pages are stable, `NULLS LAST` for the two
  nullable timestamps (BEJOB-10, BEJOB-14). Pagination is `limit` (default 50, max 100) + `offset`; there is no
  cursor (BEJOB-13).
- **Bulk** (`bulk_enqueue` `:1904`): each item is validated with `BulkJobItem::validate()` (not `nested`); invalid
  items go to `failed[]` as `{index, error}` and the valid ones are created. A 400 (not 200) when no item is
  valid (message lists `jobs[i]: reason`), a batch default is out of range (`default_priority` -100..100,
  `default_max_retries` 0..100, `default_timeout_seconds` 1..86400) or the database refuses a value
  (`job_insert_error`: SQLSTATE class 22 or 23514); other insert errors are 500 after the quota reservation is
  refunded. A payload or tags containing U+0000 is refused up front (`contains_nul`), because `jsonb` rejects it
  (BEJOB-05).
- **`expires_at`** must be in the future and later than `scheduled_at` (`validate_expiry` `:579`); such a job used
  to be accepted, charged and never claimable (BEJOB-23).
- **Queue stats** (`queues.rs:298`): `failed_jobs_24h` counts `failed|deadletter` rows by `updated_at` (dead-lettering
  sets no `completed_at`; no job is ever `failed`), `deadletter_jobs` is the current DLQ count (BEJOB-08, QW5).
- **DLQ** (`retry_dlq` `:2445`, `purge_dlq` `:2670`): retry checks only the `active_jobs` cap (a retry creates no
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

## Job lists, search, history and timeouts (0.1.122, QW10, QW12, W8-R2)

- **Filters** (`JobListFilters` `handlers/jobs.rs:165`, built by `JobListFilters::parse`; all values bound, only fixed SQL pushed):
  `queue_name`, `status` (comma list, case-insensitive, blanks and repeats dropped, an unknown entry a 400 naming it; one
  status keeps `status = $n`, several `status = ANY($n::TEXT[])`), `tag`, `q` (trimmed, at most 255 bytes, no NUL,
  `escape_like` `:111` so `\ % _` match themselves), `job_type` (exact, at most 255 bytes), `created_after` (`>=`) and
  `created_before` (`<`) as RFC 3339 (`parse_rfc3339` `:138` also accepts the `+` offset that arrives as a space;
  `after >= before` is a 400). `GET /jobs/dlq` uses the same parser with `with_status = false` (its `status` stays
  ignored). The default order is `created_at DESC, id DESC` and `idx_jobs_org_created` still answers it.
- **`q` = id prefix OR queue_name substring OR payload job_type substring.** The job-type part never compares text with a
  payload (W8-R2: on 130k jobs of ~3 KB payloads the first version read every payload from TOAST, 466 ms; `job_type=X`
  381 ms). `list_job_page` (`:412`) first runs `SEARCH_JOB_TYPES_SQL` (`:277`), a recursive skip scan of
  `idx_jobs_org_job_type` that returns the organization's job types matching `ILIKE %q%` among its **first
  `MAX_SEARCH_JOB_TYPES` = 1000 distinct types** (sort order, types of at most 255 bytes). `job_list_query` (`:357`) then
  builds a single SELECT (no `q`, or no matching type) or a UNION ALL of two disjoint parts, each cut to
  `offset + limit + 1` rows: the id/queue match from the row's own columns, and a LATERAL probe per matching type through
  the index. 54 ms; `job_type=X` 0.12 ms. A type beyond the 1000th or longer than 255 bytes is therefore invisible to `q`'s
  job-type part and still found by `job_type` (up to 255 bytes), id prefix and queue.
- **Index `idx_jobs_org_job_type`** (`20261014120000`, CIC; repair `20261014120001`): `(organization_id,
  (payload->>'job_type'), created_at DESC) WHERE octet_length(payload->>'job_type') <= 255`. The partial predicate bounds
  the btree entry size (2704 bytes) so a long free-text type cannot fail the build or an insert. **Every query that should use
  it repeats the predicate verbatim** (`JOB_TYPE_INDEXED` `:260`, appended next to every `payload->>'job_type' =`),
  or the planner cannot imply it (invariants #39). Jobs without a job type are not indexed.
- **Paging hint.** `list_job_page` fetches `limit + 1` rows; `split_page` (`:481`) drops the extra and `paged` (`:489`) sets
  `X-Has-More: true|false` (`HAS_MORE_HEADER`). The body stays a bare `JobSummary[]`; `limit` is still capped at 100. The
  header is invisible to browsers without CORS `expose_headers` (`api/mod.rs:85`). Both helpers are `pub(crate)`;
  `GET /webhooks/deliveries` uses them too (0.1.124, `dataflows/webhooks.md`).
- **Statement timeout.** Each list runs `BEGIN READ ONLY; SET LOCAL statement_timeout = <ms>` (`begin_with` + `AssertSqlSafe`:
  the only text written into the SQL is a `u64` from `DATABASE_LIST_STATEMENT_TIMEOUT_MS`, default 5000; `0` is refused by
  `Settings::validate`, `config/mod.rs:699`, because Postgres reads 0 as no timeout). Postgres cancels the statement itself,
  which also stops the work of a request whose client left (dropping the future does not cancel a running statement); `LOCAL`
  ends with the transaction so it never reaches the next pooled user or a transaction-mode PgBouncer client.
  `list_query_error` (`:460`) maps SQLSTATE 57014 to `AppError::QueryTimeout` (`error/mod.rs:78`): **400 `QUERY_TIMEOUT`**,
  not a 5xx, because the SDKs retry 5xx and count them toward their circuit breaker and the same list would time out again.
  Any other database error stays a 500 without SQL text (invariants #38). The settle/claim paths have no such timeout.
- **History** (`history` `:1389`): `GET /jobs/{id}/history` reads `job_history` through `idx_job_history_job_created`
  in either direction (`order_dir` asc default, alias `order`; `limit` 1-500, default 100; `offset`; `limit + 1` for
  `X-Has-More`). Access is `GET /jobs/{id}`'s: unknown id, other organization and a queue outside the key's scope are all 404.
  `worker_id = COALESCE(worker_id, details->>'worker_id')`, because `record_history` (`queue/mod.rs:1303`) never fills the
  column. Writers today: REST claim (`processing`), complete (`completed`), fail (`retry_scheduled`, `deadlettered`) and the
  scheduler's dead-letter CTEs (`deadlettered`); `QueueManager::enqueue` would write `created` but no handler calls it, and
  gRPC settlements write nothing (F12, open).

## Worker counters (0.1.122, QW15, W8-R3)

`workers.jobs_processed` / `jobs_failed` (`BIGINT NOT NULL DEFAULT 0`, `models/worker.rs:167,172`, returned in
`WorkerSummary` and `WorkerResponse`) are written **only** by the trigger `trigger_count_worker_job_outcome` on `jobs`
(migration `20261014110000`); never increment them in Rust, that would double count.
- **Rule.** `AFTER UPDATE OF status ON jobs ... WHEN (OLD.status = 'processing' AND OLD.assigned_worker_id IS NOT NULL AND
  OLD.lease_expires_at > now() AND (NEW.status IN ('completed','failed','deadletter') OR (NEW.status IN ('pending',
  'scheduled') AND NEW.retry_count > OLD.retry_count)))`. `completed` adds to `jobs_processed`, everything else matched to
  `jobs_failed`, for `OLD.assigned_worker_id` in `OLD.organization_id`. The counters are disjoint (fail twice then complete
  = 2 and 1).
- **Why the live-lease test.** Every worker call requires `lease_expires_at > NOW()` and every sweep an expired or NULL
  lease, and `now()` is the transaction start for both, so the lease-expiry sweep and the offline-worker release /
  dead-letter never count; a release that keeps `retry_count` (deregistration, an undelivered gRPC claim) counts nothing.
  A new path that settles a job for a worker must keep that convention; a server-side settle that is not the worker's
  outcome must expire the lease first, as `dead_letter_oversized` does (`grpc/services/queue_service.rs:2049`:
  `UPDATE ... SET lease_expires_at = NOW()` fenced on `lease_id`, then the dead-letter, in one transaction), and a release
  must not raise `retry_count` unless it is a failure.
- **The trigger is a DEFERRED constraint trigger** (`CREATE CONSTRAINT TRIGGER ... DEFERRABLE INITIALLY DEFERRED`): the
  `WHEN` clause is evaluated at the row update on that update's OLD/NEW rows, the counting `UPDATE workers` runs at COMMIT.
  As an ordinary AFTER trigger it fired (name order) before `trigger_update_dependent_jobs` and
  `trigger_update_workflow_progress`, so a settling transaction locked the job row, then the worker row, then the
  dependents; with `dependency_mode = 'any'` a worker completing a job and a dependent it was already running at the same
  instant deadlocked (225 of 300 rounds of a two-session race; 0 deferred). **Never make it immediate** (invariants #40). A
  failure in the counting UPDATE fails the COMMIT, not the statement. A constraint trigger has no `CREATE OR REPLACE`, so the
  migration drops and recreates it.
- **Cost.** `UPDATE`s that do not set `status` never fire it; others evaluate `WHEN` without calling the function (20k lease
  renewals: 1048 ms with and without). A counted transition adds one primary-key UPDATE of the worker row at commit: about
  11% on completions (PG16, 8 clients over 200 workers 8963 vs 10094 tps); completions of ONE worker id serialize on its row
  for the commit only (8797 vs 10217 tps). If that ever binds, move to a sharded side table summed on read (the API fields
  would not change). Counters start at 0 at deploy (no backfill) and survive a re-registration of the same id.

## Retry re-opens what the failure cancelled (0.1.121)

`POST /jobs/{id}/retry` (`:1719`) and `POST /jobs/dlq/retry` (`:2445`) run in one transaction (BEFLOW-04):
1. `lock_resource(org, 'workflows')` first (single retry: only for a job that belongs to a workflow; DLQ retry: always), then row locks, the order `POST /workflows` and `/workflows/{id}/retry` use;
2. reset the job(s) to `pending`, `retry_count = 0`, delete their DLQ rows;
3. `reopen_cancelled_dependents` (`:1560`) re-opens, level by level (`MAX_REOPEN_DEPTH` 100), children cancelled with
   `last_error` in `DEPENDENCY_CANCEL_REASONS` (`models/job.rs:141`: `DEPENDENCY_FAILED_CANCEL_REASON` and
   `PARENT_FAILED_CANCEL_REASON`, the constants the scheduler sweep writes - one definition for both sides, never
   a duplicated string), gated (`dependencies_met` recomputed), skipping a cancelled workflow's steps and any
   dependent another dependency still dooms; a job someone cancelled is not touched;
4. `reopen_workflows` (`:1655`) puts a `failed` workflow back to `running` and recomputes its counters; reviving pays a
   `max_workflows` slot (`check_resource_limit_conn`), so a single-job retry answers 429 `QUOTA_EXCEEDED` /
   403 `feature_disabled` when no slot is free (by design, W5-R3: skipping the charge reopens the W4-01 bypass).
   `retry_dlq` instead reads the room (`resource_room_conn`), revives the oldest failed workflows that fit, leaves the
   other steps in the DLQ and lists them in `skipped_jobs`; it answers the cap's error only when nothing else could be
   retried.

After the commit all three retry endpoints send `job.retried` per job through `spawn_dispatch` (single `jobs.rs:1797` with `source` `job_retry`, DLQ `:2646` `dlq_retry`, `workflows.rs:793` `workflow_retry`; the workflow retry statement is `UPDATE ... FROM` so `RETURNING` carries each job's previous status) (0.1.122, QW8). The single retry reads `old_status` before its transaction.

Design note (W5-R1, P3, reviewed, no change): a queue-scoped key's retry re-opens sweep-cancelled dependents in queues
outside its scope and revives the workflow; the key could already trigger the opposite cascade, the dependents return
gated and unchanged, and none of their data is returned.

## Workflows (0.1.121)

- Progress trigger `update_workflow_progress()` (migration `20261013100300`, runs when a job becomes `completed|failed`,
  only on `pending|running` workflows): `running` while any job is `pending|scheduled|processing`, else `failed` if any
  is `failed|deadletter`, else `cancelled` if any is `cancelled`, else `completed`. A cancelled step no longer lets a
  workflow end `completed` (BEFLOW-04). BEFLOW-03 (concurrent last completions can leave the workflow `running`) is
  untouched.
- `POST /workflows/{id}/cancel` (`workflows.rs:851`) reads the status under the row lock and answers 409 for a
  `completed|failed|cancelled` workflow (BEFLOW-21; still open: no `job.cancelled` webhooks for the jobs it cancels, and
  `processing` jobs keep running). `POST /workflows` rejects a repeated key in one job's `depends_on` with 400 before the quota
  reservation (BEFLOW-16). `GET /workflows/{id}` jobs come from `workflow_job_response` (`:238`): `job_type` is
  `payload.job_type` else `"job"`, `failed_at` is `updated_at` for `failed|deadletter` jobs, `backoff_type` is always
  `exponential` (BEFLOW-19).
- `POST /jobs/{id}/dependencies` (`:1015`) checks every edge before writing (self edge and cycles are 400), writes
  `dependency_mode` / `dependencies_met` only when an edge was inserted, and an omitted mode keeps the job's own
  (`AddDependenciesRequest.dependency_mode` is an `Option`) (BEFLOW-22).
