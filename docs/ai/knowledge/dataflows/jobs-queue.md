# Dataflow: Jobs / Queue

## Happy path

1. **Enqueue** — REST `POST /api/v1/jobs` (`handlers/jobs.rs`) or gRPC `Enqueue` (`grpc/services/queue_service.rs`) or `QueueManager::enqueue` (`queue/mod.rs`).
2. Job row `pending` with priority, payload, `max_retries`, `timeout_seconds`, optional `idempotency_key`.
3. **Claim / Dequeue** — worker claims via REST claim or gRPC `Dequeue` → `running` + lease.
4. **Complete** or **Fail** — fail increments `retry_count`; if `retry_count >= max_retries` → `deadletter` (+ DLQ audit insert on relevant paths).
5. **Lease recovery** — Scheduler `recover_expired_leases` (~10s) requeues (`retry_count < max_retries`) or deadletters exhausted leases. Both sweeps use `lease_expires_at <= NOW()` so the expiry instant is owned by recovery (worker ops require `>`).

## Defaults (verified)

| Source | max_retries | timeout_seconds |
|--------|-------------|-----------------|
| REST omit | `settings.queue.default_max_retries` ← env `QUEUE_DEFAULT_MAX_RETRIES` default `"3"` (`config/mod.rs`) | `QUEUE_DEFAULT_TIMEOUT_SECS` default `"300"` |
| gRPC `<=0` / omit | `settings.queue` ← `QUEUE_DEFAULT_*` | same |
| Custom webhook ingest | settings defaults | settings defaults |
| Schedules / workflows omit | settings defaults | settings defaults |
| DB column default | `DEFAULT 3` (initial schema) | — |

REST validation allows `max_retries` range `min = 0` (`models/job.rs` ~214).

## Idempotency

`ON CONFLICT (organization_id, idempotency_key) WHERE idempotency_key IS NOT NULL` in queue enqueue path.

## Scheduler loops (in-process)

Activated scheduled jobs (~5s), cron (~10s), lease recovery (~10s), metrics (~15s), stale workers (~300s), dependency/workflow progress (~10s). See `scheduler/mod.rs`.

Cron catch-up (`scheduler/mod.rs` ~395–418): if `next_run_at` is far in the past after downtime, **skip missed fires, run once now**, advance `next_run_at` via `next_run_after_in_timezone` (DST-aware). Live re-verified 2026-07-15 on `0.1.107`.

## Chesterton fence

gRPC maps proto3 zero to “use default 3” intentionally — explicit zero retries are **not** expressible on gRPC without a proto change or sentinel. Do not “fix” by treating 0 as zero retries without updating all SDKs and docs.
