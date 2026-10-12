# Dataflow: gRPC ↔ REST

## Shared domain

Both paths mutate the same Postgres job/worker tables and honor org scoping from API-key auth.

## Auth difference

| | REST | gRPC |
|--|------|------|
| API key | Bearer | `x-api-key` (or Bearer treated as key) |
| JWT | yes | no |
| Admin | `X-Admin-Key` on admin REST only | n/a |

Since 0.1.119 gRPC authenticates through the same function as REST
(`authenticate_api_key_token`, `src/grpc/auth.rs:180` `validate_api_key`): shared Redis
record cache, negative cache, bounded bcrypt, debounced `last_used`. Every service call
site uses `authenticate_request_with_cache` / `authenticate_from_metadata_with_cache`
(`src/grpc/services/queue_service.rs:439`, `:1392`; `worker_service.rs:359`). The
no-cache variants (`src/grpc/auth.rs:53`, `:79`) remain for callers without Redis; a new
service method must use the `_with_cache` form. A database failure during auth is
`UNAVAILABLE` (REST 503), anything else unexpected `INTERNAL`. gRPC has no client
address to budget failed auths on, so all gRPC callers share one failed-auth budget
(`FAILED_AUTH_GRPC_LIMIT` = 60/min, Redis `auth_fail_grpc`, `src/grpc/auth.rs:187`
`FailedAuthBudget::Grpc`, charged before the scan) that skips only the legacy scan (W3R-06,
W3R-08). gRPC legacy scans take their own turn (`bcrypt_gate::ScanLane::Grpc`), so unknown
gRPC tokens never delay REST legacy keys and the other way round; a gRPC legacy key whose
turn stays busy past the bounded wait is `UNAVAILABLE`, not `UNAUTHENTICATED` (W3R-07). See
`dataflows/auth.md`.

## Default difference (resolved in 0.1.107)

REST and gRPC both read `QUEUE_DEFAULT_*` from settings when the client omits values (gRPC via proto3 `<=0`). Explicit REST `max_retries: 0` remains allowed; gRPC still cannot express zero retries without a proto change.

Lease default (0.1.119, BEGRPC-10, W3R-03): an omitted/`<=0` lease on `Dequeue`,
`StreamJobs`, `ProcessJobs` Dequeue/RenewLease and unary `RenewLease` is
`GRPC_DEFAULT_LEASE_DURATION_SECS` (default 300; `0` = unset, at most 3600, and a value at
or below `WORKER_HEARTBEAT_INTERVAL_SECS` or below 5 refused by `Settings::validate`;
`src/config/mod.rs:378`, `:648`), resolved by `lease_secs` (`src/grpc/services/queue_service.rs:273`)
through `lease_duration_or_default` (`src/grpc/mod.rs:91`). Explicit leases are clamped to
5..3600 as before. This is intentionally **not** the REST default
`WORKER_LEASE_DURATION_SECS` (30): the Go SDK `StreamJobs` has no lease parameter and the
Go/PHP `Dequeue`/`RenewLease` send 0 unless set, so a 30 s default (briefly on the 0.1.119
branch) re-delivered their jobs after 30 s while the first worker still ran them. The value
was a hard-coded 300 before 0.1.119 and stays 300 unless the operator changes it.
`WorkerService.Register` returns this gRPC default as `lease_duration_secs` and
`WORKER_HEARTBEAT_INTERVAL_SECS` as `heartbeat_interval_secs` (`worker_service.rs:529-534`).

## Lease length is remembered (0.1.121, BEJOB-15)

Every gRPC claim (`Dequeue` `queue_service.rs:515`, `StreamJobs` `:1739`, `ProcessJobs` `:2203`) stores the granted
lease in `jobs.lease_duration_secs` (migration `20261013100200`), and `RenewLease` stores the renewed one
(`:1287`, `:2844`), so a REST `POST /jobs/{id}/heartbeat` without `lease_duration_secs` after a gRPC claim keeps
the 300 s (or whatever was granted) instead of dropping to 30 s. REST claims record theirs through
`CLAIM_BATCH_SQL`. The gRPC claim queries share the REST claim predicate and therefore `idx_jobs_claim`; edit them
together (see invariants).

## Queue scope: patterns (0.1.121, FEAT B2 / QW6)

An API key's `queues` entry may be `*`, an exact name or `prefix.*`. `GrpcAuthContext::can_access_queue` and
`can_access_all_queues` (`grpc/auth.rs:37`) delegate to `models::queue_scope_allows`, the rule REST's
`ApiKeyContext` uses, so the two cannot drift. Row filters never write `queue_name = ANY(scope)`: unary
`Complete`/`Fail`/`RenewLease`, `GetJob` and the `StreamJobs`/`ProcessJobs` claims and settles use the SQL function
`queue_in_key_scope(queue_name, scope)` (for the stream queries `queue_in_key_scope(jobs.queue_name, k.queues)` on the
locked key row), and stream authorization uses `queue_scope_allows` / `queue_scope_is_unrestricted`
(`begin_stream_operation` `queue_service.rs:1919`, `queue_scope_filter_from_queues` `:1899`). See `dataflows/auth.md`.

## Worker counters are database-side (0.1.122, QW15)

`workers.jobs_processed` / `jobs_failed` come from the deferred trigger on `jobs`, so gRPC `Complete`, `Fail`, `ProcessJobs` and
`StreamJobs` settlements count with no code of their own, exactly like REST. The proto `Worker` / `ListWorkers` messages do not
carry the counters (REST only; adding them is an additive proto change, not done). Keep the lease convention when adding a gRPC
settle path (`dataflows/jobs-queue.md`, invariants #40).

## Worker field names

The database uses `max_concurrent_jobs`, `current_job_count`, and `created_at`; REST public JSON keeps `max_concurrency`, `current_jobs`, and `registered_at`. Keep handlers and OpenAPI mapped at the boundary so SDKs do not see DB-only column names.

## Worker registration (WorkerService, 0.1.119)

`Register` (`src/grpc/services/worker_service.rs:354`) mirrors REST `workers.rs`:

- One transaction: `lock_resource(org, "workers")` (`:398`), then the already-registered
  lookup using the SQL function `worker_counts_toward_cap` (`:406-411`), then
  `check_resource_limit_conn`, then the upsert, then commit. Concurrent registrations
  cannot overshoot `max_workers` (BEGRPC-07). A limit rejection is mapped from the HTTP
  response and keeps the REST message (`limit_rejection_status` `:332`: 429 →
  `RESOURCE_EXHAUSTED`, 403 → `PERMISSION_DENIED`).
- `RegisterWorkerRequest.worker_id` (optional, field 7, `proto/spooled.proto`): empty or
  omitted mints a UUID; a supplied id follows `models::validate_worker_id` (1-128 chars,
  ASCII letters, digits, `-_.`). Re-registering an id this org already holds is an upsert
  that does not count twice; `ON CONFLICT (id) ... WHERE workers.organization_id =
  EXCLUDED.organization_id` (`:444`) means another org's id is `ALREADY_EXISTS` and its
  row is untouched (BEGRPC-08). Since 0.1.121 (BEJOB-19, KEYS-3) every conflict carries the shared
  `models::WORKER_ID_UNAVAILABLE` text ("This worker_id cannot be used: register with a different
  worker_id, or omit it to get a generated one", `models/worker.rs:21`), the same words as REST's 409, which
  no longer reveals that another organization holds the id (the old "Worker ID already in use by another
  organization" is gone; no SDK or dashboard code matched it). A queue-scoped key
  (`!queue_scope_is_unrestricted(&auth.queues)`) that names a same-org worker serving a queue outside its
  scope gets the identical `ALREADY_EXISTS`: the row is read `FOR UPDATE` under the registration advisory
  lock, before the upsert, and checked with `can_access_all_queues` (`worker_service.rs:407-430`; REST
  `workers.rs:160-176`). Worker ids stay one global namespace (a per-org namespace is deferred).
- Input limits are the REST ones and fail with `INVALID_ARGUMENT` before any write:
  metadata ≤ 64 KB, `worker_type`/`version` ≤ 50, hostname ≤ 255. `max_concurrency` is
  clamped to 1-100 (0 = 5) instead of rejected (existing fleets must keep registering);
  `current_jobs` on Heartbeat is clamped to 0-1000 (BEGRPC-09).
- Slot rule: `worker_counts_toward_cap(status, last_heartbeat)` (migration
  `20261011150300`) is true for healthy/degraded, and for draining only with a heartbeat in
  the last 2 minutes. `get_org_resource_counts` (newest definition: migration
  `20261012110000`, which keeps this rule), REST register and gRPC register all use
  it. The stale sweep also takes silent draining workers offline now
  (`src/scheduler/mod.rs:1049`).
- `Heartbeat` (`:535`): no metadata keeps the stored metadata (BEGRPC-06); an empty status
  keeps the stored status (offline → healthy; draining is sticky; an unknown non-empty
  status → healthy with a WARN). `Deregister` (`:643`) moves `workers_active` only when the
  worker was not already offline (BEGRPC-25).
- Worker RPC rate limiting honours `RATE_LIMIT_FAIL_CLOSED` like `QueueService`
  (`worker_service.rs:84` vs `queue_service.rs:212`; still two copies of
  `enforce_rate_limits`) (BEGRPC-11).
- **Not enforced:** claim/dequeue never checks that `worker_id` is a registered worker, so
  `max_workers` still only bounds `Register`. Deferred by owner decision (BEGRPC-15):
  requiring it breaks REST and gRPC consumers that never call Register.

## Enqueue validation (0.1.119)

`validate_enqueue_fields` (`queue_service.rs:326`): queue name ≤ 100 characters (counted
as characters), priority -100..100, idempotency key ≤ 255 characters, tags ≤ 64 KB JSON;
all `INVALID_ARGUMENT` with REST wording. Claim paths still accept queue names up to 255
so older long-named queues drain. INSERT errors with SQLSTATE 23514/54000 map to
`INVALID_ARGUMENT` (`enqueue_db_error_status` `:1766`). `scheduled_at` outside years 1-9999
or nanos outside 0..1e9 is `INVALID_ARGUMENT`; an all-zero Timestamp means unset
(`src/grpc/convert.rs:38`, `:50`). The `job.created` webhook carries `priority` and the
real initial status (`scheduled` for a future `scheduled_at`) (`:643-650`).

## Streaming

`StreamJobs` / `ProcessJobs` exist on QueueService. Live consume re-verified 2026-07-15 on `0.1.107` (bounded): ProcessJobs dequeue→complete ×5 + stale double-complete rejected; StreamJobs drained ×2. Long-lived streaming under load remains residual operational risk. Prefer bounded streams in tests.

StreamJobs (0.1.119, BEGRPC-03/12/22, W3R-01/02):

- One job with the worker per stream (`run_job_stream` `:1645`). After handing out a job
  the stream claims nothing until that job is no longer `processing` under the delivered
  `lease_id` with time left (`delivery_still_held` `:1854`): completed, failed, released,
  or its lease ran out. Checks back off 10 ms → 1 s; a `Complete`/`Fail` (unary or
  ProcessJobs) through the same process wakes the stream at once (`SettleSignals` `:123`).
  The one-slot channel (`stream_jobs` `:1415`) is not demand control on its own: tonic and
  hyper pull messages while the client's HTTP/2 window has room (64 KB for grpc-go), so
  claiming whenever the slot was free leased that many jobs to a worker holding one, and
  they expired unread with `retry_count + 1` (W3R-02). Consequence: one stream runs one job
  at a time; concurrency means one stream per job slot.
- A claimed job still buffered is a `BufferedJob` (`:210`); if the stream is dropped it
  goes back to `pending` with the lease cleared and `retry_count` unchanged
  (`release_unsent_claim` `:1987`). `job.started` fires when the job is handed to the
  transport, not at claim.
- At most `MAX_STREAMS_PER_ORG` = 100 open streams per org **per process**
  (`:70`, `StreamSlots` `:170`); the next gets `RESOURCE_EXHAUSTED`. Not cluster-wide:
  N replicas allow 100×N.
- The rate limiter is charged once on open and once per delivered job, never per idle poll.
  Over the limit the claim is rolled back and the stream waits 1 s instead of ending. The
  key's current `rate_limit` and the org's plan limits come from the `FOR SHARE` key
  lookup in `begin_stream_operation` (`:1904`, `LockedStreamKey`), so lifting or changing a
  limit reaches open streams, and the charge inside the claim transaction is Redis only
  (`charge_stream_claim` `:399`). Never touch the pool while a stream's claim transaction is
  open: the old `org_rate_limits` cache-miss lookup there needed a second connection, and
  with as many busy streams as pool connections the process stalled (W3R-01).
- StreamJobs and ProcessJobs do not end on process shutdown; an open worker stream holds
  the 10 s gRPC drain until it elapses (open item, `bugs/findings.md` BEOPS-24).

ProcessJobs: an error on the request stream (an oversized or malformed frame, a reset) ends
the call with that status, for example `OUT_OF_RANGE` (`:1386`, BEGRPC-23).

Message size: `MAX_GRPC_MESSAGE_SIZE` = 4 MiB (`src/grpc/mod.rs:50`, also the tonic
encode/decode limit in `server.rs`). A `Dequeue` batch stops at 3 MiB encoded
(`MAX_BATCH_RESPONSE_BYTES` `:68`, checked at `:735`) but always returns one job; the job
that does not fit is released to pending, no retry spent. A job over 4 MiB is
dead-lettered by `dead_letter_oversized` (`:2049`; called at `:850`, `:1816`, `:2279`) with "Job too large for a gRPC
message", a `job.failed` webhook (which also sends the implied `job.deadlettered`, 0.1.122) and its completion webhook, on Dequeue, StreamJobs and
ProcessJobs alike. Since 0.1.122 that is one transaction: it first expires the lease (`lease_expires_at = NOW()`, fenced on
`lease_id` and `status = 'processing'`; a miss rolls back) and then dead-letters the job and writes the DLQ row, because the
dead-letter is the server's decision and must not count as the claiming worker's failure on `workers.jobs_failed`: the counter
trigger only counts a move out of `processing` under a live lease (`dataflows/jobs-queue.md`, "Worker counters"). This includes a REST-created job whose Struct form is over 4 MiB: any
gRPC consumer on that queue dead-letters it, though a REST worker could process it (product
policy). A claim error part-way through a unary Dequeue batch returns the jobs claimed so
far (`:711-720`, BEGRPC-24).

## Settlement outcomes (0.1.119, BEGRPC-17)

Complete/Fail/RenewLease on a job this worker owned that has already finished:
`FAILED_PRECONDITION "Job is already <status>"` on unary calls (`classify_worker_miss_status`
`queue_service.rs:2079`), `CONFLICT` with the same text inside ProcessJobs (`:2144`),
matching REST's 409. A requeued job is "Lease expired". A job owned by another worker or
outside the key's scope is still `NOT_FOUND`. The 2026-07-15 live check
("double-complete → `NOT_FOUND`") is superseded.

Dead-letter keeps `retry_count` (the statements at `:958-966` and `:2417-2424`), so the DLQ
`total_retries` equals the retries run, as on REST; a retried failure still adds one.

## Struct numbers

`google.protobuf.Struct` numbers are doubles. Whole values with |n| < 2^53 come back as JSON
integers (`src/grpc/convert.rs:144-149` `number_to_json`): `42` stays `42`, `1.5` stays a
float, `2^53` and above stay floats, NaN/infinity become null. Non-object payloads are
still dropped; fixing that needs a proto field (`bytes payload_json`).

## Health and shutdown (0.1.119)

`main.rs:121` binds the gRPC port (and loads TLS) before anything is spawned, so a busy port
or bad PEM exits the process non-zero. gRPC serves with a shutdown watch
(`src/grpc/server.rs:288` `serve`): on shutdown tonic-health turns `''`, `QueueService`
and `WorkerService` to NOT_SERVING, then in-flight calls drain for `GRPC_DRAIN_TIMEOUT`
(10 s, `:61`). While serving, `probe_database` (`:375`) checks the DB every 10 s (3 s
timeout) and flips health on change; Redis state is deliberately not reflected. REST
`/health` reports `grpc` and `scheduler` (see "Process supervision, shutdown and `/health`" in
`dataflows/jobs-queue.md`).

## Proto JSON

camelCase: `jobId`, `leaseId`, etc.

`FailRequest.retry` is proto3 `optional` (0.1.118): omitted = retry per
`max_retries` (REST parity), explicit `false` = dead-letter now (BEGRPC-02).

`RegisterWorkerRequest.worker_id` is proto3 `optional` (field 7, 0.1.119). The SDK proto
copies need it plus regenerated stubs before SDK workers can send a stable id.
