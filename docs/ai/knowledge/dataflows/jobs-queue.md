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

REST claim default lease = `WORKER_LEASE_DURATION_SECS` (30); since 0.1.115 a
heartbeat that omits `lease_duration_secs` renews for that same default (it was a 422). gRPC has its
own default: an omitted/`<= 0` lease on `Dequeue`, `StreamJobs`, `ProcessJobs` and `RenewLease` is
`GRPC_DEFAULT_LEASE_DURATION_SECS` (default 300; `0` = unset, at most 3600, must exceed `WORKER_HEARTBEAT_INTERVAL_SECS`; `grpc/services/queue_service.rs:273`
`lease_secs`, `grpc/mod.rs:91` `lease_duration_or_default`). Since **0.1.119** it is a setting instead of
the hard-coded `DEFAULT_LEASE_DURATION_SECS`, and `Register` reports it (BEGRPC-10). It must not follow
the 30 s REST default: the Go SDK `StreamJobs` cannot send a lease (W3R-03).

REST validation allows `max_retries` range `min = 0` (`models/job.rs` ~214).

`PUT /queues/{name}/config` is an upsert. Omitted `max_retries`/`default_timeout`/`enabled`/`rate_limit`/`settings` keep the existing row. Incoming `settings` objects are merged into the current JSON (pause metadata lives there). `rate_limit` (1-100000) is accepted, stored and returned but **not enforced by any claim path** (BEBILL-11 / BEJOB-12, operator decision; enforcement is F02): say "stored, not enforced yet" in docs, never that workers are throttled by it.

`DELETE /queues/{name}` without `delete_jobs` removes the `queue_config` row only and 409s while pending/processing jobs exist; with no config row but finished jobs it is a 409 that points at `?delete_jobs=true` (was a bare 404). `POST /queues/{name}/pause` on a queue that does not exist is 404 (it used to create a config row outside the queue cap); create-and-pause is `PUT .../config {"enabled": false}`. `?delete_jobs=true` deletes every job in that org+queue, matching `dead_letter_queue` rows, and the config (OpenAPI + dashboard checkbox).

`POST /jobs/{id}/dependencies` sets `dependencies_met` from `check_job_dependencies_met`. A SQL/decode error fails the request; it must not default to met.

`POST /jobs/bulk` has its own body limit (`api/mod.rs:194-213` `bulk_body_limit`): 100 jobs x
(largest plan payload cap + 4 KiB), clamped between axum's 2 MiB default and a 32 MiB ceiling
(`BULK_BODY_LIMIT_CEILING`); today that resolves to the 32 MiB ceiling. A body over it is refused
as 413 or `400 INVALID_JSON` (the extractor's length-limit error). Every other route keeps 2 MiB.
Before 0.1.119 a bulk of 100 jobs of 25 KB was refused (BEOPS-16).

## Idempotency

`ON CONFLICT (organization_id, idempotency_key) WHERE idempotency_key IS NOT NULL` in queue enqueue path.

## Scheduler loops (in-process)

Activated scheduled jobs (~5s), cron (~10s), lease recovery (~10s), metrics (~15s), stale workers (~300s), dependency/workflow progress (~10s). See `scheduler/mod.rs`. Every loop pass reports to `RuntimeHealth::scheduler_tick` (`scheduler/mod.rs:95`) and a drop guard marks it stopped however `run` ends (`:42`, `:76`); `/health` reports the scheduler down after 120 s without a pass.

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
