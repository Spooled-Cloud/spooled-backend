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
heartbeat that omits `lease_duration_secs` renews for that same default (it was a 422). gRPC `Dequeue` default
is 300 (`grpc/services/queue_service.rs` `DEFAULT_LEASE_DURATION_SECS`).

REST validation allows `max_retries` range `min = 0` (`models/job.rs` ~214).

`PUT /queues/{name}/config` is an upsert. Omitted `max_retries`/`default_timeout`/`enabled`/`rate_limit`/`settings` keep the existing row. Incoming `settings` objects are merged into the current JSON (pause metadata lives there).

`DELETE /queues/{name}` without `delete_jobs` removes the `queue_config` row only and 409s while pending/processing jobs exist; with no config row but finished jobs it is a 409 that points at `?delete_jobs=true` (was a bare 404). `POST /queues/{name}/pause` on a queue that does not exist is 404 (it used to create a config row outside the queue cap); create-and-pause is `PUT .../config {"enabled": false}`. `?delete_jobs=true` deletes every job in that org+queue, matching `dead_letter_queue` rows, and the config (OpenAPI + dashboard checkbox).

`POST /jobs/{id}/dependencies` sets `dependencies_met` from `check_job_dependencies_met`. A SQL/decode error fails the request; it must not default to met.

## Idempotency

`ON CONFLICT (organization_id, idempotency_key) WHERE idempotency_key IS NOT NULL` in queue enqueue path.

## Scheduler loops (in-process)

Activated scheduled jobs (~5s), cron (~10s), lease recovery (~10s), metrics (~15s), stale workers (~300s), dependency/workflow progress (~10s). See `scheduler/mod.rs`.

Cron catch-up (`scheduler/mod.rs` ~395–418): if `next_run_at` is far in the past after downtime, **skip missed fires, run once now**, advance `next_run_at` via `next_run_after_in_timezone` (DST-aware). Live re-verified 2026-07-15 on `0.1.107`.

## Chesterton fence

gRPC maps proto3 zero to “use default 3” intentionally — explicit zero retries are **not** expressible on gRPC without a proto change or sentinel. Do not “fix” by treating 0 as zero retries without updating all SDKs and docs.
