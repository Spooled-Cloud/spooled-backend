# Changelog

All notable changes to `spooled-backend` are documented here.

Format follows [Keep a Changelog](https://keepachangelog.com/en/1.0.0/).  
Versions follow [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

---

## [Unreleased]

---

## [0.1.122] - 2026-10-12

Wave 8 of the 2026-10-10 bug hunt, the feature quick wins QW8-QW16 of the
features review (QW13 is dashboard-only): rate-limit headers, server-side job
list filters, the job history route, worker job counters, four new
outgoing-webhook events with Slack, Discord and Teams formats, the inbound
webhook log, and a CI check that the router and `docs/openapi.yaml` list the
same endpoints. Review found three problems before release, all fixed here: the
unit of the rate-limit reset header (W8-R1), the cost of the job-type search
(W8-R2) and a lock-order deadlock in the worker counter trigger (W8-R3).

### Upgrade notes

- **The API change is additive.** Two new routes (`GET /jobs/{id}/history`,
  `GET /webhooks/deliveries`), new optional list parameters, new response
  fields and headers, four new event names. Response bodies keep their shape
  (the job lists are still a bare array) and existing outgoing webhooks keep
  sending the signed JSON envelope byte for byte. Three existing calls behave
  differently:
  - `GET /jobs?status=failed,deadletter` was a `400`; it is a status list now.
  - `q`, `job_type`, `created_after` and `created_before` on `GET /jobs` and
    `GET /jobs/dlq` were ignored, like any unknown parameter; they filter now,
    and an invalid value is a `400` (QW12).
  - A job list the database cannot answer within 5 s is stopped and answers
    `400 QUERY_TIMEOUT`, where it used to run until the 30 s request timeout cut
    it off (the statement kept running in the database). Set
    `DATABASE_LIST_STATEMENT_TIMEOUT_MS` to change the 5000 ms default (W8-R2).
- **`X-RateLimit-*` response headers exist now** (QW9). Authenticated responses
  that went through the plan rate limiter carry `X-RateLimit-Limit`,
  `X-RateLimit-Remaining`, `X-RateLimit-Reset` and `X-RateLimit-Scope`.
  **`X-RateLimit-Reset` is a Unix time in seconds**, the moment
  `X-RateLimit-Remaining` next goes up (the current time while the bucket is
  full); on a `429` it equals the body's `reset_at`, and `Retry-After` is still
  the wait in seconds. The first draft sent the wait in seconds, which all four
  SDKs parse as an epoch and would have shown as January 1970; that was caught
  in review and never released (W8-R1). The headers are left out when no
  rate-limit check ran or it was skipped (public and admin routes, Redis not
  configured, Redis failing with `RATE_LIMIT_FAIL_CLOSED` off, the fail-closed
  `Rate limit unavailable` answer), so clients must treat them as optional.
- **A browser can read them now.** The CORS layers expose
  `X-RateLimit-Limit`, `-Remaining`, `-Reset`, `-Scope`, `Retry-After`,
  `X-Request-Id` and `X-Has-More`; before, no response header was exposed and a
  cross-origin dashboard could not read `Retry-After` either.
- **New webhook events.** `job.deadlettered`, `job.retried`,
  `workflow.completed` and `workflow.failed` can be subscribed to with the
  existing create and update calls (QW8). `job.deadlettered` is sent next to
  every `job.failed` whose `data.status` is `deadletter`; `job.retried` once per
  job that `POST /jobs/{id}/retry`, `POST /jobs/dlq/retry` or `POST
  /workflows/{id}/retry` puts back to `pending`. **`workflow.completed` and
  `workflow.failed` are announced at most once**: a scheduler pass every 10 s
  claims the workflows that ended, marks them announced, commits, and only then
  hands the events to the in-memory delivery queue. No outcome is sent twice,
  whichever instance runs the pass, but an outcome is lost when
  - the process stops between that commit and the delivery being recorded;
  - the delivery queue (or the webhook's share of it) is full, so the event is
    dropped and counted in `spooled_outgoing_webhook_deliveries_dropped_total`,
    or recording the delivery fails;
  - the workflow is retried (`POST /workflows/{id}/retry` clears
    `completed_at`) before the next pass sees it, so a workflow that fails and
    is retried within about 10 s never announces that `workflow.failed`, only
    the outcome it reaches afterwards;
  - the outcome is more than an hour old when a pass first sees it (no
    scheduler ran for that long); everything that ended before this release is
    never announced either.

  A pass announces at most 500 outcomes, the rest follow on the next pass. A
  cancelled workflow sends neither event. Treat the event as a prompt and
  confirm with `GET /workflows/{id}`; making it at-least-once needs a durable
  outbox (W8-R4).
- **Outgoing webhooks take a `format`** (QW14): `json` (the default, and every
  existing webhook), `slack`, `discord` or `teams`. A chat format sends one
  short message built from the event, so the incoming-webhook URL of the chat
  service can be used as it is; `X-Spooled-Signature` signs the bytes actually
  sent. The Teams format is the legacy `MessageCard` of Office 365 connector
  webhooks; Teams Workflows (Power Automate) URLs expect an Adaptive Card and
  reject it.
- **`GET /webhooks/deliveries` is the inbound webhook log** (QW11): the accepted
  requests to `POST /webhooks/{org_id}/custom`, newest first. Unrestricted API
  keys only, a queue-scoped key gets `403 ACCESS_DENIED`. Entries older than
  this release have `null` `job_id` and `status`.
- **Workers report `jobs_processed` and `jobs_failed`** (QW15). They start at 0
  with this release: past failures cannot be attributed. They are kept by a
  database trigger and only count outcomes a worker reported while it held the
  lease; sweep releases and the server-side dead-letter of a job too large for
  a gRPC message are not counted (details under Workers).
- Eight new migrations run at startup, two of them `CREATE INDEX CONCURRENTLY`
  (listed below). The index builds do not block reads or writes but wait for
  transactions that were already open, so a long-lived or idle session delays
  startup; `20261014110000` takes a short lock on `jobs` and `workers`. Run the
  checks in `docs/guides/deployment.md`, "0.1.122 checks", after the rollout.
- No new Prometheus series. The new events share the delivery queue, its bounds
  and its drop counter with the existing ones.

### Rate limiting

- **Headers from the limiter the request was counted against** (QW9). With the
  organization's token bucket, `Limit` is the plan burst (Free 10, Starter 50,
  Pro 200, Enterprise 1000, or `custom_limits.rate_limit_burst`), `Remaining` the
  whole tokens left after this request, `Reset` the time the next whole token
  arrives (now, when the bucket is full), `Scope` `organization`. With an API
  key's own `rate_limit` (requests per minute, a fixed window), `Limit` is that
  value, `Reset` the start of the next window and `Scope` `api_key`. When both
  apply, the one that runs out first is reported: fewer requests remaining, and
  on a tie the later reset.
- **A `429` carries the same headers** with `Remaining` 0, `Retry-After` as
  before, and `X-RateLimit-Reset` and the body's `reset_at` taken from one clock
  reading, so they are equal (W8-R1). The public and admin limiters still answer
  with `Retry-After` only; their `429` bodies are unchanged. All the `429`
  builders in `plan_rate_limit.rs` now share one helper.
- **Token bucket script.** The Lua script behind `check_token_bucket` also
  returns the milliseconds until the next whole token (`TokenBucketResult::next_token_ms`).
  The script has a new SHA (`EVALSHA` falls back to `EVAL` on `NOSCRIPT`), and
  the bucket's hash fields are unchanged, so old and new replicas share buckets
  during a rolling deploy. gRPC callers ignore the new field.
- **CORS exposes the headers** listed in the upgrade notes, on both the
  production and the development layer (`cors_exposed_headers` in
  `src/api/mod.rs`).

### Jobs API

- **Server-side list filters** (QW12). `GET /jobs` takes `status` as one status
  or a comma list (case-insensitive, blanks and repeats ignored, an unknown
  entry a `400` that names it), `q`, `job_type`, `created_after` (inclusive) and
  `created_before` (exclusive), the last two as RFC 3339 (a `+` offset that
  arrived as a space is read as `+`; `created_after` not before `created_before`
  is a `400`). `q` is trimmed, at most 255 bytes, free of NUL, and matches
  case-insensitively a job whose **id starts with it**, whose `queue_name`
  contains it or whose `payload.job_type` contains it; `%` and `_` match
  themselves. `job_type` is an exact match of at most 255 bytes. All filters
  combine with AND and with `queue_name`, `tag` and the key's queue scope.
  `GET /jobs/dlq` takes the same filters except `status`, which it still
  ignores. The order is unchanged (`created_at DESC`, then id).
- **`X-Has-More`** on `GET /jobs`, `GET /jobs/dlq` and `GET /jobs/{id}/history`:
  `true` when another page follows. The list fetches one row past the page, the
  body stays a bare array, and `limit` is still capped at 100.
- **The job-type search is bounded and indexed** (W8-R2). The first version of
  `q` and `job_type` walked `idx_jobs_org_created` through every job of the
  organization and read each payload from TOAST: on 130,000 jobs of ~3 KB
  payloads, `job_type=X` took 381 ms and a `q` search 466 ms, with no timeout.
  - New index `idx_jobs_org_job_type` (organization, `payload->>'job_type'`,
    `created_at DESC`), partial on `octet_length(payload->>'job_type') <= 255`
    so a long free-text type can neither fail the build nor the insert of such
    a job. `job_type=X` reads it: 0.12 ms. A job without a job type is not in
    it.
  - `q` never reads a payload. Its job-type part first resolves, by a skip scan
    of that index, the organization's matching job types, then probes each type
    through the index and unions the result with the id / queue part: 54 ms.
    **That part sees only the first 1,000 distinct job types** of the
    organization in sort order, each at most 255 bytes. A job whose type is
    beyond that, or longer, is still found by `job_type` (exact, up to 255
    bytes), by its id prefix and by its queue.
  - The queries repeat the index predicate verbatim; the planner only uses a
    partial index whose predicate the query implies.
- **List statement timeout** (W8-R2). `GET /jobs` and `GET /jobs/dlq` run in a
  read-only transaction with `SET LOCAL statement_timeout`
  (`DATABASE_LIST_STATEMENT_TIMEOUT_MS`, default 5000; `0` is refused at
  startup because Postgres reads it as no timeout). Postgres cancels the
  statement itself, which also bounds a request whose client went away. A list
  it stopped is `400 QUERY_TIMEOUT` ("narrow it with `queue_name`, `status`,
  `job_type`, `created_after` or `created_before`, or a longer `q`"), not a
  5xx: the same list would only time out again, and the SDKs retry 5xx and count
  them toward their circuit breaker. `LOCAL` ends with the transaction, so the
  setting never reaches the next user of the pooled connection or a
  transaction-mode PgBouncer client. Any other database failure stays a `500`
  without SQL text.
- **`GET /jobs/{id}/history`** (QW10). The recorded `job_history` events of a
  job, oldest first (`order_dir=desc`, alias `order`, for newest first), with
  `limit` (default 100, 1 to 500), `offset` and `X-Has-More`. Another
  organization's job, an unknown id and a queue outside the key's scope are all
  `404`, as for `GET /jobs/{id}`. `worker_id` is the column or else the
  `worker_id` the claim wrote into `details`. Events exist only where code
  writes them: REST claim (`processing`), REST complete (`completed`), REST
  fail (`retry_scheduled`, `deadlettered`) and the scheduler's dead-letter
  sweeps (`deadlettered`). `QueueManager::enqueue` would write `created`, but
  no REST or gRPC handler calls it. **The history is sparse for jobs that go
  through gRPC, and no job has a `created` event** (F12, Phase 2).
- **The status of `job.failed` comes from the transition.** `POST
  /jobs/{id}/fail` used to read the job's status back after the commit; by then
  a retry may have been claimed again or a dead-lettered job retried, and the
  `job.failed` event (and the `job.deadlettered` it implies), the realtime
  events and the metrics were skipped or carried the wrong status. They use the
  status the fail transaction set.
- **`job.retried` on every manual retry.** `POST /jobs/{id}/retry`, `POST
  /jobs/dlq/retry` (one event per job that left the DLQ) and `POST
  /workflows/{id}/retry` send it after the commit, with `old_status` and a
  `source` of `job_retry`, `dlq_retry` or `workflow_retry`. The workflow retry
  statement is now `UPDATE ... FROM` so it can return each job's previous
  status; its predicate is unchanged.

### Workers

- **`jobs_processed` and `jobs_failed`** on `GET /workers` and `GET /workers/{id}`
  (QW15; the backend half of FEAT B7, the dashboard's `Processed 0 / Failed 0`).
  `workers.jobs_processed` and `workers.jobs_failed` are `BIGINT NOT NULL
  DEFAULT 0`, written only by the trigger `trigger_count_worker_job_outcome` on
  `jobs`, never by application code, so REST, gRPC and any path added later
  count alike. A move out of `processing` counts for `OLD.assigned_worker_id`
  when the lease was still live (`OLD.lease_expires_at > now()`): to `completed`
  adds to `jobs_processed`; to `failed`, `deadletter`, or back to `pending` /
  `scheduled` with a raised `retry_count` (a scheduled retry) adds to
  `jobs_failed`. The counters are disjoint: a job that fails twice and then
  completes adds 2 and 1.
- **What is not counted.** Every worker call needs a live lease and every sweep
  an expired one, so lease-expiry recovery and the offline-worker release and
  dead-letter never count as the worker's failure. A release that keeps
  `retry_count` (deregistration, an undelivered gRPC claim) counts nothing, and
  neither does the gRPC dead-letter of a job too large for a gRPC message: that
  is the server's decision, so its transaction first expires the lease
  (`lease_expires_at = now()`) and then dead-letters the job. Counts survive a
  re-registration of the same worker id; workers that existed before the
  release start at 0.
- **Deadlock found before release (W8-R3).** The first version was an ordinary
  AFTER trigger. AFTER triggers run in name order, so it fired before
  `trigger_update_dependent_jobs` and `trigger_update_workflow_progress` and a
  settling transaction locked the job row, then the worker row, then the job's
  dependents. With `dependency_mode` `any`, a worker completing a job at the
  same moment as a dependent it was already running deadlocked: 225 of 300
  rounds of a two-session race. The trigger is now a **constraint trigger,
  `DEFERRABLE INITIALLY DEFERRED`**: its `WHEN` clause is still checked at the
  row update, on that update's old and new rows, but the counting `UPDATE` runs
  at `COMMIT`, so the worker row is the last lock a transaction takes (0 of 300
  rounds). A failure in the counting update now fails the `COMMIT` instead of
  the statement. Never make the trigger immediate again.
- **Cost.** An `UPDATE` that does not set `status` never fires it, and for one
  that does the `WHEN` clause is checked without calling the function (20,000
  lease renewals: 1,048 ms with and without it). A counted transition adds one
  primary-key update of the worker row at commit: one completion per
  transaction on PostgreSQL 16, 8 clients over 200 workers, 8,963 vs 10,094 tps
  (about 11%). Completions of one worker id serialize on its row for the length
  of the commit: 8 clients on one worker, 8,797 vs 10,217 tps.

### Webhooks

- **New events** (QW8). The `outgoing_webhook_events_check` constraint is
  replaced by a superset (a webhook still cannot subscribe to an unknown
  event) and `VALID_WEBHOOK_EVENTS` has the four names; see the upgrade notes
  for what each sends. `data` of `job.deadlettered` is that of the
  `job.failed` it follows (`job_id`, `queue_name`, `status: "deadletter"`,
  `error`); `job.retried` has `job_id`, `queue_name`, `status: "pending"`,
  `old_status` (`failed`, `deadletter` or `cancelled`), `source` and, for a
  workflow job, `workflow_id`; `workflow.completed` / `workflow.failed` have
  `workflow_id`, `name`, `status`, `total_jobs`, `completed_jobs`, `failed_jobs`,
  `created_at`, `started_at`, `completed_at` and `metadata`.
- **`job.deadlettered` is derived, not emitted.** The dispatcher
  (`OutgoingWebhookService::spawn_dispatch` and `dispatch_event`) queues it for
  any `job.failed` whose `data.status` is `deadletter`, so REST fail, gRPC
  `Fail` and `ProcessJobs`, gRPC lease expiry, the oversized-job dead-letter and
  both scheduler sweeps send it once per transition, and so does any path added
  later. **No caller may send `job.deadlettered` itself** (it would arrive
  twice), and a path that retries a job must not use `status: "deadletter"`. It
  queues after the `job.failed` it follows, so with the delivery queue almost
  full the original wins the last slot.
- **Workflow outcomes** are announced by the scheduler at the end of the
  10-second dependency pass (`announce_workflow_outcomes`), one statement that
  claims the `completed` / `failed` workflows whose `completed_at` differs from
  the new `workflows.announced_completed_at`, copies the one into the other and
  returns them, `FOR UPDATE SKIP LOCKED`. It covers both ways a workflow ends,
  the progress trigger on `jobs` (which runs inside the settling transaction,
  where no webhook can be sent) and the terminate sweep, and a retry clears
  `completed_at`, so the next outcome is announced again. The partial index
  `idx_workflows_unannounced_outcome` has the statement's predicate verbatim.
- **Formats** (QW14). `slack` sends `{"text"}` in mrkdwn with `&`, `<` and `>`
  escaped, so a value cannot form a link or an `<!channel>`; `discord` sends
  `{"content"}` of at most 2,000 characters with markdown escaped and
  `allowed_mentions: {"parse": []}`, so an `@everyone` in an error text pings
  nobody; `teams` sends a `MessageCard` with the title, the event as `text` and
  the fields as facts (green, red or blue by event). The message shows these
  `data` fields when present, in this order: `name`, `workflow_id`, `job_id`,
  `queue_name`, `schedule_id`, `worker_id`, `hostname`, `status`, `old_status`,
  `source`, `completed_jobs`, `failed_jobs`, `total_jobs`, `error`, `reason`,
  `message`; a value is cut at 300 characters. The body is rendered at delivery
  from the stored envelope, headers are unchanged, the delivery history keeps the
  envelope, a manual retry renders with the webhook's current format, and
  `POST /outgoing-webhooks/{id}/test` sends the chat format too (Slack and
  Discord answer `400` to raw JSON). `format` is validated case-insensitively on
  create and update (`400` otherwise) and returned by create, get, list and
  update; on update, omitting it keeps the current one.
- **Inbound webhook log** (QW11). `GET /webhooks/deliveries` returns `id`,
  `provider`, `event_type` (`custom` when the request had none), `matched_queue`,
  `job_id`, `status` (`created` or `duplicate`) and `created_at`, newest first
  (`created_at DESC`, id), with `limit` (default 50, 1 to 100) and `offset`
  and the filters `status`, `since` (RFC 3339, inclusive) and `queue_name`
  (alias `queue`, exact match, at most 255 characters). An invalid filter is a
  `400`. It never returns the payload, a signature or the organization id, and
  only accepted requests are logged (a bad token or an over-limit request
  writes nothing, so unauthenticated traffic cannot grow the table).
  `matched_queue` is now the queue of the job the request resolved to, which
  differs from the request's `queue_name` only for an idempotent replay.
  The existing retention sweep applies.

### Tooling and docs

- **Router vs OpenAPI check** (QW16). `scripts/check_openapi_drift.py` (Python 3,
  standard library only) reads every `.route(...)` in `src/api/mod.rs`, with the
  prefix of the `.nest` that mounts its builder, and every operation under
  `paths:` in `docs/openapi.yaml`, and fails when a routed endpoint is
  undocumented, a documented one is not routed or a path parameter is named
  differently. A construct it cannot read (`any()`, `on()`, `route_service`, a
  nest of a non-local router, `$ref` path items) fails the check instead of
  being skipped. Exceptions live in `scripts/openapi-drift-allowlist.json`, each
  with a reason (empty today). CI has a new `openapi-drift` job (the script's
  self-test, then the check) and the Docker build job waits for it. 105 routed
  and 105 documented operations are in sync. **A new route now needs its
  `docs/openapi.yaml` path in the same change.**
- **OpenAPI** documents `GET /organizations/{id}/members` (the one real gap the
  check found), `GET /jobs/{id}/history`, `GET /webhooks/deliveries`, the list
  filters, `X-Has-More`, `QUERY_TIMEOUT`, the rate-limit headers, the worker
  counters, the four events and `format`.
- **Documentation:** the API usage, jobs, workers, webhooks, operations and
  deployment guides and the README describe all of the above; the guides no
  longer say there are no `X-RateLimit-*` headers.

### Migrations

All eight run at startup, in this order:

- `20261014100000_outgoing_webhook_events_and_format.sql` (replaces the events
  CHECK by a superset, adds `outgoing_webhooks.format` `NOT NULL DEFAULT 'json'`
  with its CHECK)
- `20261014100100_inbound_webhook_delivery_outcome.sql` (adds nullable
  `webhook_deliveries.job_id` and `status`, the status CHECK `NOT VALID`)
- `20261014100200_workflow_outcome_announcement.sql` (adds nullable
  `workflows.announced_completed_at`)
- `20261014100201_workflow_outcome_index.sql` (`CREATE INDEX CONCURRENTLY`
  `idx_workflows_unannounced_outcome`; a single statement outside a
  transaction)
- `20261014100202_workflow_outcome_index_repair.sql` (the `INVALID`-index repair
  for the previous one)
- `20261014110000_worker_job_counters.sql` (`count_worker_job_outcome()`, the
  deferred constraint trigger `trigger_count_worker_job_outcome` on `jobs`, and
  `workers.jobs_processed` / `jobs_failed`)
- `20261014120000_job_type_index.sql` (`CREATE INDEX CONCURRENTLY`
  `idx_jobs_org_job_type`)
- `20261014120001_job_type_index_repair.sql` (the `INVALID`-index repair for the
  previous one)

The two `CREATE INDEX CONCURRENTLY` files carry no `SET lock_timeout`, for the
reason given under 0.1.121: sqlx sends a `-- no-transaction` file as one query
string, which Postgres runs as an implicit transaction block that refuses
`CREATE INDEX CONCURRENTLY`. Both need no lock that blocks reads or writes. A
replica that waits on the migrator's advisory lock can be chosen as the deadlock
victim of an index build: startup then fails once and the repair migration
rebuilds the `INVALID` index on the next start, blocking writes to that table
for the build (about half a second at 130,000 `jobs` rows of ~3 KB payloads).
Production is a single container. The other migrations start with `SET LOCAL
lock_timeout = '5s'`; the ones that add a column take `ACCESS EXCLUSIVE` on the
table for an instant (`webhook_deliveries`, `workflows`, `outgoing_webhooks`,
`workers`; catalog-only, no rewrite), and `20261014110000` first creates the
trigger, which takes `SHARE ROW EXCLUSIVE` on `jobs` (it waits for writers in
flight and blocks new ones while held). On a busy `jobs` table a start can fail
on the `lock_timeout` once and succeed on the next. `20261014110000` is
idempotent: a constraint trigger has no `CREATE OR REPLACE`, so a re-run drops
the trigger first. Rolling back to 0.1.121 is safe: the new columns are unused
there and the widened CHECK is a superset.

### Deferred / known

- **QW8 is done** (the 0.1.121 "Deferred / known" entry is superseded), with the
  at-most-once limits of the workflow events above (W8-R4).
- **Job history is incomplete (F12).** gRPC complete, fail and dequeue write no
  `job_history` rows and no handler writes a `created` row, so
  `GET /jobs/{id}/history` covers REST worker calls and scheduler dead-letters.
- **`q` is not index-backed for its id and queue parts.** The job-type part is
  bounded (first 1,000 types) and the statement timeout bounds the whole list,
  but an organization with a very large `jobs` table can still make a broad `q`
  without a created window answer `400 QUERY_TIMEOUT`. A trigram index would
  need `pg_trgm` and a write cost on every insert.
- **Hot worker row (QW15).** Completions of one worker id serialize on its row
  for the commit, about 8,800 completions a second on a local SSD. If that ever
  binds, move the counters to a sharded side table summed on read; the API
  fields would not change.
- **Counting rules are a product choice**: the counters are disjoint, every
  sweep outcome is excluded (including a dead-letter after a worker crash) and
  there is no backfill.
- **Teams** is the legacy `MessageCard` format; an Adaptive Card format for
  Teams Workflows is not offered yet.
- **`src/webhook/mod.rs` is dead code** (an unused public module whose `INSERT`
  into `webhook_deliveries` names columns that do not exist). Delete or fix it.
- **Clients are cross-repo.** This release is the backend half: using the new
  list filters and `X-Has-More`, the history route, the worker counters, the
  event names and `format` in the dashboard and the SDKs is tracked in those
  repositories, and the backend works without them.
- The 0.1.121 open items (BEJOB-16, BEJOB-19, BEJOB-20, BEFLOW-05, BEFLOW-21 and
  the others) are unchanged.

---

## [0.1.121] - 2026-10-11

Fixes from the 2026-10-10 bug hunt (wave 5): the jobs API, workflows and
schedules, realtime streams, outgoing webhooks, API-key queue scopes and
retention.

### Upgrade notes

- **Several requests that used to answer `200` are now refused.** Check clients
  and scripts for these:
  - `GET /jobs`, `GET /jobs/dlq` and `GET /jobs/stats` answer `400` for an
    invalid `status`, `queue_name`, `tag`, `order_by` or `order_dir`. They used
    to log it and return the unfiltered list. `order_by` takes one of
    `created_at`, `updated_at`, `scheduled_at`, `completed_at`, `priority`,
    `status`, `queue_name`, `retry_count`; a client that passes free-form
    values (an SDK list helper, say) gets a `400` (BEJOB-09, BEJOB-10).
  - `PUT /queues/{name}/config` answers `400` for `max_retries` outside 0-100,
    `default_timeout` outside 1-86400, `rate_limit` outside 1-100000 (it was
    clamped silently) and a queue name that is not 1-100 letters, digits, `-`,
    `_` or `.`, or that starts with `.` or `-` (BEJOB-04).
  - `POST /jobs/bulk` answers `400`, not `200` with every item failed, when no
    item is valid, a batch default is out of range or the database refuses a
    value (BEJOB-05).
  - `POST /jobs` answers `400` for an `expires_at` that is not in the future or
    not later than `scheduled_at` (BEJOB-23).
  - `POST /jobs/{id}/complete`, `/fail` and `/heartbeat` without `lease_id`
    answer `422 VALIDATION_ERROR`, not `409 LEASE_EXPIRED` (BEJOB-18).
  - Also `400`: a repeated key in a workflow job's `depends_on` (BEFLOW-16), a
    self or circular edge on `POST /jobs/{id}/dependencies` (BEFLOW-22), a cron
    expression whose date never occurs such as `0 0 30 2 *` (BEFLOW-08) and an
    `is_active` list filter that is not a boolean (BEFLOW-18). `POST
    /workflows/{id}/cancel` on a finished workflow is `409` (BEFLOW-21).
- **Retrying a dead-lettered workflow step can be refused by the workflow
  cap.** A retry re-opens the dependents the scheduler cancelled because of the
  step and puts its `failed` workflow back to `running`, which takes one of the
  plan's `max_workflows` slots. `POST /jobs/{id}/retry` answers `429
  QUOTA_EXCEEDED` (resource `workflows`), or `403 feature_disabled` on a plan
  without workflows, when no slot is free, as `POST /workflows/{id}/retry`
  does. `POST /jobs/dlq/retry` retries everything else and lists the held steps
  in the new `skipped_jobs` field; it answers the cap's error only when nothing
  else could be retried (BEFLOW-04, W5-R3).
- **A workflow with a cancelled step no longer ends `completed`.** It ends
  `failed` when a step failed or was dead-lettered, else `cancelled`, as the
  scheduler sweep already closed such workflows (BEFLOW-04, migration
  `20261013100300`).
- **Cron: `*/N` in a day field now has Vixie AND semantics.** A day-of-month or
  day-of-week field that starts with `*` (`*/2` included) counts as
  unrestricted, so `0 0 */2 * 1` means odd days that are Mondays, not odd days
  or Mondays. Only schedules with `*/N` in one day field and a restricted value
  (or another `*/N`) in the other change; a stored `next_run_at` keeps its old
  value until the schedule fires once. Find them with the query in
  `docs/guides/deployment.md`, "0.1.121 checks" (BEFLOW-30).
- **Retention now deletes four more kinds of data.** The 5-minute sweep also
  removes `schedule_runs` older than the plan's history retention (Free 1 day,
  Starter 7, Pro 30, Enterprise 90, or the `custom_limits` override),
  `dead_letter_queue` rows and `failed` / `deadletter` jobs older than the
  plan's job retention (Free 3 days, Starter 14, Pro 30, Enterprise 90), and
  `offline` workers whose last heartbeat is over 7 days old. A failed,
  dead-lettered or cancelled step of a workflow that still exists is kept until
  the workflow itself is purged, so a failed workflow stays retryable; a failed
  job is also kept while a pending, scheduled or processing job waits on it,
  and a worker while a job is still assigned to it. The first passes after the
  deploy work through the backlog gradually: up to 5,000 jobs per statement and
  10,000 rows per statement of the other kinds, every 5 minutes (BEOPS-11,
  BEFLOW-09, W5-R4, W5-R5).
- **Realtime streams end with their session.** The WebSocket closes with code
  `1008` when the access token expires or the session is logged out or revoked,
  and with `1000` after a maximum age of 1 hour, so a dashboard or SDK client
  reconnects about once an hour. The three SSE streams end at token expiry and
  logout too (BEFLOW-13, BEAUTH-19, BEFLOW-12).
- **Outgoing webhook delivery is bounded.** Past the queue bound a new event or
  delivery is dropped and counted instead of piling up in memory, one webhook
  may use at most a quarter of the request slots and of the queue, and per-job
  completion webhooks have their own slots and a per-organization share. At the
  default `OUTGOING_WEBHOOK_MAX_CONCURRENT_DELIVERIES=64` a single webhook has
  16 requests in flight and 256 queued; raise the setting if a tenant needs
  more. Drops show in four new Prometheus series (see Webhooks) (BEFLOW-10,
  BEFLOW-27, W5-R2).
- **API keys accept `prefix.*` queue patterns.** A `queues` entry can now be
  `orders.*`. Every other use of `*` (`orders*`, `*.eu`, `a.*.b`) is `400
  VALIDATION_ERROR` at create and update; stored keys that hold such an entry
  keep matching nothing, and `docs/guides/deployment.md` has a query that lists
  them (FEAT B2, QW6).
- **A changed billing email waits for its code.** On an organization with a
  verified billing email, `PUT /organizations/{id}` with a new address stores it
  in the new `pending_billing_email`; the verified address keeps working for
  email login until the code sent to the new one is confirmed. The dashboard
  should show `pending_billing_email` and offer the confirmation while it is
  set; until it does, a user who changes the address sees the old one as
  confirmed and has no button for the new one, and nobody is locked out
  (W1-14).
- **gRPC `Register` worker-id conflicts are neutral**, as REST's are: an id held
  by another organization, or by a worker outside a queue-scoped key's queues,
  answers `ALREADY_EXISTS` / `409` with one message that does not say which
  (BEJOB-19). The text "Worker ID already in use" no longer appears.
- Twelve new migrations run at startup, three of them `CREATE INDEX
  CONCURRENTLY` (listed below). The index builds do not block reads or writes
  but wait for transactions that were already open, so a long-lived or idle
  session delays startup. Run the checks in `docs/guides/deployment.md`,
  "0.1.121 checks", after the rollout.

### Jobs API

- **Queue config is validated.** `PUT /queues/{name}/config` checks its body and
  the queue name in the path (see Upgrade notes). The table enforces the same
  ranges with two CHECK constraints, and migration `20261013100100` first
  clamps stored out-of-range rows into range. A stored `default_timeout` of two
  days used to make every enqueue to that queue a `500` (BEJOB-04).
- **Bulk enqueue validates each item.** An invalid item (payload size or NUL
  character, priority outside -100..100, idempotency key over 255 characters)
  is listed in `failed` with its `index` and the reason, and the valid items are
  still created. Batch defaults are range-checked: `default_priority` -100..100,
  `default_max_retries` 0..100, `default_timeout_seconds` 1..86400. A payload or
  tags containing the NUL character is a `400` on single and bulk enqueue
  instead of a `500` (BEJOB-05).
- **A blank `idempotency_key` is no key**, on `POST /jobs` and `POST
  /jobs/bulk`. Such jobs used to collapse onto the organization's first
  empty-key job (BEJOB-06).
- **An idempotent replay is answered first** (`200`, `created: false`), before
  the payload, quota and expiry checks, and charges nothing, for single and bulk
  enqueue; a bulk batch of replays reserves no quota. At the daily cap a job is
  now refused before it is inserted, instead of being inserted, visible to
  workers, and deleted again (BEJOB-07, BEJOB-20 daily-cap part).
- **Queue stats count dead-lettered jobs.** `GET /queues/{name}/stats`
  `failed_jobs_24h` is the number of jobs dead-lettered in the last 24 hours
  (it was always 0) and the new `deadletter_jobs` is the queue's current
  dead-letter count (BEJOB-08, FEAT QW5).
- **List filters and order are honoured.** `status` matches case-insensitively,
  an empty parameter is no filter, and `tag` takes any characters up to 1,024
  bytes (`customer:42`; it allowed only `[A-Za-z0-9_.-]` up to 64). `order_by`
  and `order_dir` work on `GET /jobs` and `GET /jobs/dlq` (`scheduled_at` and
  `completed_at` sort `NULLS LAST`), and every list breaks ties on the job id,
  so `OFFSET` pages no longer repeat or skip rows of a bulk batch (BEJOB-09,
  BEJOB-10, BEJOB-14).
- **Retrying re-opens what the failure cancelled** (BEFLOW-04, W5-R3). A job or
  DLQ retry, in one transaction: resets the job, removes its
  `dead_letter_queue` row, re-opens level by level (up to 100 deep) the
  dependents the dependency sweep cancelled because of it (gated until the job
  completes again; skipped when another dependency would doom them again or
  their workflow is `cancelled`), and recomputes the counters of the affected
  workflows. Only sweep cancellations are re-opened; a job someone cancelled
  stays cancelled. The cancel reasons are now shared constants
  (`DEPENDENCY_CANCEL_REASONS`) used by the sweep and the retry.
- **A heartbeat without `lease_duration_secs` keeps the claimed lease length.**
  It renewed for 30 s, cutting a 600 s claim short. The claim records the
  granted length in the new `jobs.lease_duration_secs` (REST claim, and gRPC
  `Dequeue`, `StreamJobs`, `ProcessJobs` and `RenewLease`), and an explicit
  value becomes the new length. Renewal also uses the database clock now
  (BEJOB-15).
- **`lease_id` is required** on complete, fail and heartbeat (BEJOB-18).
- **DLQ retry and purge.** `POST /jobs/dlq/retry` is no longer refused at the
  daily job cap (it creates no new daily job; the active-jobs cap applies),
  `POST /jobs/dlq/purge` `older_than` compares the time a job was
  dead-lettered, not when it was created, and both statements re-check the
  status on the row they change (BEJOB-22).
- **Documentation:** the guides no longer say idempotency keys expire after 24
  hours (a key lives as long as its job row), no longer document a `cursor`
  parameter that does not exist or a default list limit of 100 (it is 50), and
  show the real queue-stats fields (BEJOB-13).

### Workflows and schedules

- **Workflow steps and the DAG.** `GET /workflows/{id}` jobs carry the payload's
  `job_type` (`"job"` when it has none) and `failed_at` for failed and
  dead-lettered jobs (BEFLOW-19). A workflow turns `running`, with `started_at`
  set, within about 10 s of its first job being claimed (it stayed `pending`
  with no start time until a job finished) (BEFLOW-20).
  `POST /jobs/{id}/dependencies` rejects self and circular edges, rewrites the
  job's `dependency_mode` and `dependencies_met` only when an edge was added,
  and an omitted `dependency_mode` keeps the job's own instead of resetting it
  to `all` (BEFLOW-22).
- **Cancelling a finished workflow is a `409`.** It used to overwrite the
  workflow's `completed` / `failed` outcome and `completed_at` (BEFLOW-21).
- **Cron.** A date that never occurs (`0 0 30 2 *`, `0 0 31 4 *`) is a `400` at
  create and update; such a schedule used to be stored with `next_run_at: null`
  and never fire, after burning 525,600 loop iterations on the request thread.
  A day that cannot match is now skipped whole, so rare but valid schedules
  (Feb 29, Feb 29 on a Sunday) resolve in about one step per day. A cron
  expression with no upcoming run is a `400` on update as well (BEFLOW-08). The
  `*/N` day-field rule is in the upgrade notes (BEFLOW-30).
- **`GET /schedules` honours `queue_name`, `is_active` and `offset`**, which the
  Go and Python SDKs send. A filtered call returned every schedule and an offset
  loop re-read the first page forever. Newest first, ties broken by id
  (BEFLOW-18).
- **Schedule history is purged** after the plan's history retention (BEFLOW-09).

### Realtime

- **WebSocket and SSE end with the session that opened them.** The WebSocket and
  the three SSE streams (`/events`, `/events/jobs/{id}`,
  `/events/queues/{name}`) read `exp`, `jti` and `sid` of the access token they
  were opened with. They end when the API would stop accepting the token (`exp`
  plus the validation leeway) and when a re-check, at most every 2 s, finds the
  session or token revoked (logout), the key revoked or expired, or the
  organization deleted. Before, only the key was re-checked, so a stream opened
  with a dashboard token kept running after logout and past expiry (BEFLOW-13,
  BEAUTH-19).
- **WebSocket close codes and maximum age.** `1008` for an expired or revoked
  token, `1000` after 1 hour, `1011` after an `Error` event when the Redis event
  channel could not be opened or closes (or the authorization lookup failed).
  Before, a socket whose pub/sub link dropped kept answering pings and never
  delivered another event. Clients should reconnect on `1000` and `1011`; with
  Redis down they now reconnect in a loop with backoff instead of holding a
  silent socket (BEFLOW-12).
- **No database query per event.** The WebSocket re-checks authorization at most
  every 2 s before revealing an event, and on every ping (BEFLOW-14).
- **Connection caps no longer leak.** Per-organization counts are Redis sorted
  sets (`realtime_connections:{ws|events|job|queue}:{org}`) scored by a 30 s
  heartbeat; a connection stops counting 90 s after its last heartbeat, so
  replicas killed during a deploy no longer lock an organization out with `429`.
  The old `ws_connections:*` and `sse_connections:*` keys are no longer read and
  expire on their own. During a rolling deploy old replicas still use the
  counters and new ones the sets, so the cap is briefly doubled; the heartbeat
  check uses the application clock, so replica clocks should agree within about
  a minute (BEAUTH-14).
- **`/events/jobs/{id}` and `/events/queues/{name}` are capped** at 100 open
  streams per organization each (`429 RATE_LIMIT_EXCEEDED`). Queue stream
  statistics are computed once per (organization, queue) every 4 s and shared by
  its streams, instead of one full aggregate per stream every 5 s (BEFLOW-15).

### Webhooks

- **Bounded delivery queue.** Outgoing deliveries go through a queue of 16
  deliveries per slot (1,024 at the default) worked by a fixed set of consumers.
  A webhook gets at most a quarter of the request slots and of the queue. Each
  attempt re-reads the webhook, so a delivery stops once the webhook is disabled
  (by the user or by auto-disable), deleted or unsubscribed from the event, and
  uses the current URL and secret. A retry waits without holding a slot
  (BEFLOW-10).
- **New metrics**, sampled on every scrape:
  `spooled_outgoing_webhook_deliveries_dropped_total`,
  `spooled_outgoing_webhook_queue_depth`,
  `spooled_outgoing_webhook_deliveries_in_flight` and
  `spooled_completion_webhooks_dropped_total`. A non-zero drop counter means the
  queue (or one webhook's share) was full (BEFLOW-10, BEFLOW-27).
- **Completion webhooks.** Per-job `completion_webhook` calls share the
  outgoing-webhook HTTP client and connections, but have their own request slots
  and a per-organization lane (a quarter of the slots and of the pending bound),
  so one tenant's backlog to a URL that never answers cannot stall other
  tenants' org webhooks or completion webhooks. They are still a single attempt
  (BEFLOW-27, W5-R2).
- **Delivery history fields.** `error` holds the failure reason (`HTTP 503`, the
  transport error, `Invalid webhook URL: ...`), `response_body` the first 1 KiB
  of the receiver's answer, and `status_code` is `null` instead of `0` when no
  response arrived (BEFLOW-24).
- **`failure_count` resets** when `PUT /outgoing-webhooks/{id}` turns a webhook
  on or changes its URL, so the first failed delivery after a fix no longer
  disables it again. A stored URL that the SSRF policy rejects is a failed
  delivery (no retries) and trips auto-disable, and a manual retry of it returns
  `200 {"success": false}` instead of `500` (BEFLOW-25).
- **`job.failed` also fires for sweep dead-lettering.** A job dead-lettered
  because its lease expired or its worker went offline now writes the
  `deadlettered` job-history row in the same statement and sends what a reported
  failure sends: the `job.failed` outgoing webhook (`data.status: "deadletter"`),
  the job's completion webhook and the realtime `JobStatusChange` and
  `JobFailed` events. It was the one silent failure path (BEFLOW-11, FEAT QW7).

### API keys and scopes

- **`prefix.*` queue patterns.** An entry `orders.*` covers every queue whose
  name starts with `orders.` (`orders.eu`, `orders.eu.west`), not `orders`
  itself or `ordersx`; `*` alone still means every queue and any other use of
  `*` is rejected (see Upgrade notes). One rule, `queue_scope_allows`, serves
  REST and gRPC, and a queue-scoped key can grant a pattern only under one of
  its own patterns that is at least as broad. The admin
  `POST /admin/organizations/{id}/api-keys` route validates `queues` like
  `POST /api-keys` (FEAT B2, QW6).
- **Row filters understand patterns.** Every queue-scope filter now calls the
  new SQL function `queue_in_key_scope(queue_name, scope)` instead of
  `queue_name = ANY(scope)`: REST settle and heartbeat, job, DLQ, workflow and
  schedule lists, dashboard and stats counts, workers lists (a worker is listed
  only when every queue it serves is in scope), gRPC settles, `GetJob`,
  `StreamJobs` and `ProcessJobs` claims, and the gRPC stream and realtime
  authorization. Without this a `prefix.*` key could enqueue and claim but not
  complete, and its jobs were hidden from every list (FEAT B2, QW6).
- **Pending billing email.** The verification code goes to
  `pending_billing_email` when one is set, and confirming it moves the address
  into `billing_email` as verified in one guarded statement. Sending the active
  address again withdraws the change, an organization without a verified address
  is still changed directly (unverified), and an admin write of `billing_email`
  counts as verified and clears a pending change. `GET` and `PUT
  /organizations/{id}` return `pending_billing_email`, and the signup hint
  `unverified_org_exists` also matches a pending address (W1-14).
- **Worker registration conflicts.** `POST /workers/register` (and gRPC
  `Register`) answer an id held by another organization, or by a worker of the
  same organization that serves a queue outside a queue-scoped key's scope,
  with one neutral `409`, and the scoped key can no longer move such a worker
  into its queues (BEJOB-19).

### Retention and performance

- **Claim index.** `idx_jobs_claim` (organization, queue, `priority DESC`,
  `created_at`; partial on `status IN ('pending', 'scheduled')`) has the claim
  query's predicate and order, so a claim reads the first rows of its queue
  instead of sorting its whole backlog: 252 ms became 0.4 ms at 300,000 pending
  jobs (PostgreSQL 16). The gRPC claim paths use the same predicate. The query
  is now the constant `queue::CLAIM_BATCH_SQL` and a test checks its plan
  (BEJOB-03).
- **Retention locks what it deletes.** The workflow, job and worker sweeps pick
  their rows `FOR UPDATE ... SKIP LOCKED`: the subselect re-checks status and
  age against the newest row version, and rows an open transaction holds are
  skipped. A plain `DELETE ... WHERE id IN (SELECT ...)` deleted a job that a
  retry had just moved back to `pending` (W5-R4).
- **Retention keeps a workflow's steps while the workflow exists.** A failed,
  dead-lettered or cancelled step of a workflow that still exists is kept,
  whatever the workflow's status: a deleted step took its dependency edges with
  it, so retrying the workflow ran the step's dependents ungated. Workflow
  retention runs first in the pass, so a purged workflow's steps go in the same
  pass (W5-R5).
- **`schedule_runs` is indexed.** `schedule_runs.job_id` (`ON DELETE SET NULL`)
  had no index, so every deleted job scanned all of `schedule_runs`: at 450,000
  runs and 2,000 dead-lettered jobs the first retention pass took 55 s inside the
  scheduler loop, which also fires cron schedules and recovers leases; now
  0.17 s. The `schedule_runs` sweep ranges over `(schedule_id, started_at)` per
  schedule and deletes by primary key: 84 ms became 50 ms, and 26 ms became
  0.7 ms with nothing to delete. Job retention statements delete 5,000 rows a
  pass (was 10,000) (W5-R6).
- **Scope filters keep their plans.** Migration `20261013130200` moves the
  pattern rewrite of `queue_in_key_scope()` into an `IMMUTABLE` helper, so a
  bound exact-name scope plans as `queue_name = ANY(...)` again and a prefix
  scope as a plain `LIKE ANY`; a scoped key's dashboard counts had dropped to a
  scan of the organization's whole job range (1.5 ms to 43 ms at 130,000 jobs).

### Migrations

All twelve run at startup, in this order:

- `20261013100000_job_claim_index.sql` (`CREATE INDEX CONCURRENTLY`
  `idx_jobs_claim`; a single statement outside a transaction)
- `20261013100001_job_claim_index_repair.sql` (rebuilds `idx_jobs_claim` if an
  interrupted concurrent build left it `INVALID`)
- `20261013100100_queue_config_checks.sql` (clamps stored `queue_config` rows
  into range, then adds the `max_retries` and `default_timeout` CHECKs)
- `20261013100200_job_lease_duration.sql` (adds nullable
  `jobs.lease_duration_secs`)
- `20261013100300_workflow_progress_cancelled_steps.sql` (redefines
  `update_workflow_progress()`: `failed`, else `cancelled`, else `completed`)
- `20261013130000_pending_billing_email.sql` (adds nullable
  `organizations.pending_billing_email`)
- `20261013130100_queue_in_key_scope.sql` (adds `queue_in_key_scope(text,
  text[])`)
- `20261013130200_queue_scope_patterns_fold.sql` (adds the `IMMUTABLE`
  `queue_scope_like_patterns(text[])` and redefines `queue_in_key_scope`)
- `20261013140000_schedule_runs_job_id_index.sql` (`CREATE INDEX CONCURRENTLY`
  `idx_schedule_runs_job_id`)
- `20261013140001_schedule_runs_job_id_index_repair.sql` (the `INVALID`-index
  repair for the previous one)
- `20261013140100_schedule_runs_retention_index.sql` (`CREATE INDEX
  CONCURRENTLY` `idx_schedule_runs_schedule_started`)
- `20261013140101_schedule_runs_retention_index_repair.sql` (the `INVALID`-index
  repair for the previous one)

The three `CREATE INDEX CONCURRENTLY` files carry no `SET lock_timeout`: sqlx
sends a `-- no-transaction` file as one query string, which Postgres runs as an
implicit transaction block that refuses `CREATE INDEX CONCURRENTLY`, so each is
a single statement. They need no lock that blocks reads or writes. With several
replicas starting together, a replica that waits on the migrator's advisory lock
can be chosen as the deadlock victim of the index build: startup then fails once
and the repair migration rebuilds the `INVALID` index on the next start, blocking
writes to that table for the build (under a second at 130,000 `jobs` rows).
Production is a single container. The other migrations start with
`SET LOCAL lock_timeout = '5s'` and change the catalog or define functions only,
apart from `20261013100100`, which first updates the few `queue_config` rows
(one per configured queue) that are out of range.

### Deferred / known

- **QW8 (event types).** `job.deadlettered`, `job.retried`, `workflow.completed`
  and `workflow.failed` are not subscribable yet. They need emit sites in the
  queue and scheduler code plus a CHECK-constraint migration and
  `VALID_WEBHOOK_EVENTS`; offering the names before anything emits them would be
  a silent wrong answer.
- **BEJOB-20 (P3, partly open).** The daily-cap part is fixed. The active-jobs
  cap is still check-then-insert, so concurrent `POST /jobs` can exceed it by a
  few; fixing it needs a per-organization lock or counter on the enqueue hot
  path (owner decision).
- **BEJOB-16 (P3, open).** Mixed clocks: the claim stamps lease expiry from the
  application clock while complete, fail and reclaim compare with the database
  `NOW()`, and the quota windows use `CURRENT_DATE` in the session time zone.
  Only the heartbeat renewal moved to the database clock (BEJOB-15). Keep
  replica clocks within a few seconds (the shortest lease is 5 s).
- **BEFLOW-05 (P2, open).** A dependency that disappears counts as satisfied:
  when a step of a workflow is deleted its dependency edges go with it, and the
  orphan sweep releases the gated dependents one hour later, so they can run
  without the step having run.
- **The expiry sweep still deletes workflow steps.** `expires_at` is accepted on
  a workflow job, and the sweep deletes expired `pending`, `scheduled`, `failed`
  and `deadletter` jobs without looking at their workflow. That is how BEFLOW-05
  is reached, and it also removes failed steps that retention now keeps while
  their workflow exists (W5-R5). Fix direction: cancel the dependents first, or
  skip workflow steps.
- **Realtime fan-out (BEFLOW-14, second half).** Each socket still holds its own
  Redis pub/sub connection (capped at 100 per organization). The queue stream
  statistics cache is per process, so N replicas run the aggregate at most N
  times per 4 s.
- **BEFLOW-21 (P3, partly open).** Only the overwrite of a finished workflow is
  fixed. Cancelling a workflow still sends no `job.cancelled` webhooks for the
  jobs it cancels and does not stop jobs that are already `processing`.
- **BEJOB-19 (P3, partly open).** Worker ids are still one global namespace (a
  per-organization namespace is deferred), and a queue's `active_workers` counts
  `status = 'healthy'` with no heartbeat recency. Offline rows are now purged.
- **W5-R1 (P3, design note).** `POST /jobs/{id}/retry` and `POST /jobs/dlq/retry`
  by a queue-scoped key re-open sweep-cancelled dependents in queues outside the
  key's scope and revive the failed workflow, taking a workflow slot. Reviewed
  and left as is: the key could already cause the opposite cascade by failing or
  cancelling the step, the dependents come back gated and unchanged, and nothing
  of them is returned to the key. An operator who wants a cancelled dependent to
  stay cancelled cannot pin it, short of cancelling the workflow.
- **W5-R3, single-job retry (by design).** `POST /jobs/{id}/retry` still refuses
  a step whose failed workflow does not fit under `max_workflows`; skipping the
  charge would reopen the W4-01 cap bypass.
- Queue `rate_limit` is still stored and returned, not enforced (BEBILL-11,
  BEJOB-12; see 0.1.120).

---

## [0.1.120] - 2026-10-11

Fixes from the 2026-10-10 bug hunt (wave 4): Stripe checkout trust, plan limits,
client-IP handling and the email-login throttles.

### Upgrade notes

- **Stripe: send two more events to the billing webhook.** In the Stripe
  dashboard, the `/api/v1/billing/webhook` endpoint must also send
  `checkout.session.async_payment_succeeded` and
  `checkout.session.async_payment_failed` (next to the six it has). Until then a
  checkout paid by a delayed method (SEPA, ACH) is never linked to its
  organization (BEBILL-15).
- **Payment Link checkouts are no longer trusted.** A completed checkout is
  trusted only when it carries the organization in its metadata, which only the
  new `POST /api/v1/billing/checkout` sets. A legacy Payment Link checkout (the
  organization named by `client_reference_id`, a URL parameter anyone can edit)
  now links only an organization that has no live subscription and whose
  verified billing email the payer used at checkout, and it never cancels
  anything. Any other legacy checkout is acknowledged and not linked, so the
  customer pays and stays on the old plan until you link them by hand. Watch the
  logs for `Not linking a Payment Link checkout` (organization id, customer id,
  subscription id, event id). An upgrade through a Payment Link no longer
  cancels the old subscription either: cancel it in Stripe or the billing portal
  (BEBILL-02).
- **The website and the dashboard must switch to `POST /api/v1/billing/checkout`**
  (body `{"plan": "starter" | "pro"}`, redirect to the returned `url`) together
  with, or right after, this release. Deactivate the public Payment Links in
  Stripe once nothing uses them.
- **`TURNSTILE_SECRET_KEY` stays unset** until both the marketing site and the
  dashboard render the Turnstile widget and send `turnstile_token` to
  `POST /auth/email/start`. Once it is set, a start request without a valid
  token is `400 CAPTCHA_FAILED` (BEAUTH-11, FESF-15).
- **Client IPs: edge headers count only from a private peer.** Without
  `TRUSTED_CLIENT_IP_HEADER`, `CF-Connecting-IP` and `X-Real-IP` are believed
  only when the TCP peer is a loopback, private, link-local or unique-local
  address (a proxy on the same host or network, like production's cloudflared);
  any other caller is rate-limited by its own address. Production needs no
  change and logs one warning at boot while the header is unset. A self-hosted
  proxy that reaches the backend from a public address must set
  `TRUSTED_CLIENT_IP_HEADER`, or every caller shares that proxy's one bucket
  (BEAUTH-10).
- **Throttled `POST /auth/email/verify` answers 429**, with `Retry-After`, not
  401. A client that matched on 401 for "too many attempts" must handle 429
  (BEAUTH-06).
- **`-1` in a `custom_limits` resource cap now means unlimited** (the eight
  caps `max_jobs_per_day` to `max_webhooks`; it used to be ignored, which meant
  the plan default), as `null` does and as `-1` does in the `SPOOLED_PLAN_*`
  variables. The admin API now rejects invalid `custom_limits`
  (see Limits). Run the checks in `docs/guides/deployment.md`, "0.1.120 checks",
  after the rollout (BEBILL-07, BEBILL-26).
- **Admin soft delete now also pauses the organization's schedules, disables its
  outgoing webhooks and removes its incoming-webhook token**, and restoring
  `plan_tier` does not turn any of them back on. Organizations soft-deleted
  before this release keep their active schedules and enabled webhooks in the
  database until you stop them (SQL in the same guide) (BEFLOW-06, BEAUTH-15).
- **`max_workflows` counts pending and running workflows only**, so finished runs
  no longer hold a slot (operator decision). Plan numbers are unchanged
  (BEBILL-10, BEFLOW-17).
- **`GET /billing/status` needs an unrestricted API key** (queue-scoped keys get
  403), like the billing portal (BEBILL-23).
- `FRONTEND_URL` is optional: it is the website base URL Stripe Checkout returns
  to (`<FRONTEND_URL>/account/billing?checkout=success|cancel`), default
  `https://spooled.cloud`. `docker-compose.prod.yml` forwards it, and
  `TURNSTILE_SECRET_KEY`.
- Three new migrations run at startup (listed below). All three are quick: a
  column with a constant default, function definitions, and one small `UPDATE`
  of `api_keys`. During a rolling deploy an old pod's email login can still
  insert an `Email Login` key that is not marked as a system key; the backfill
  in the guide fixes it and is safe to re-run.

### Billing

- **Server-side checkout.** `POST /api/v1/billing/checkout` creates a Stripe
  Checkout Session bound to the authenticated organization: the session and its
  subscription carry `metadata.spooled_org_id`, and `client_reference_id` is
  set too. It reuses the organization's Stripe customer, otherwise Checkout is
  prefilled with the verified billing email. Needs an unrestricted key. `400`
  when `plan` is not `starter` or `pro`, `409` when the organization already has
  that plan with an active or trialing subscription (use the billing portal),
  `503` when Stripe, the plan's price id or the Stripe call is unavailable
  (BEBILL-02, BE-08).
- **Completed checkouts are trusted through metadata only.** Before, anyone who
  knew an organization id could open a Payment Link with that id, pay for a
  plan, take over that organization's billing and have its real subscription
  cancelled. Legacy Payment Link checkouts follow the rule in the upgrade notes
  (BEBILL-02).
- **A replaced subscription is cancelled reliably.** When a server-created
  checkout replaces a live subscription, the old id is recorded in
  `organizations.stripe_superseded_subscription_ids` by the same statement that
  relinks the organization, and removed only after Stripe confirms the cancel
  (a 2xx, a 404, or a rejected cancel of a subscription that has already ended).
  A failed cancel makes the webhook answer 5xx so Stripe redelivers the event,
  instead of leaving the customer billed twice. The cancel pass also runs when
  the relink itself was skipped as stale, and never cancels the organization's
  current subscription. All Stripe API calls now time out after 10 s (5 s to
  connect); the cancel used to wait 20 s and the others did not time out
  (BEBILL-14).
- **A second subscription of the same Stripe customer no longer takes over the
  organization.** Checkout now reuses the customer, so
  `customer.subscription.created` for the new subscription could arrive before
  `checkout.session.completed` and hide the old one, which was then never
  cancelled. `customer.subscription.created/updated` for a different
  subscription are ignored while the organization's current one is live; the
  completed checkout moves the organization over (BEBILL-14).
- **Delayed payments are linked.** `checkout.session.async_payment_succeeded`
  links the organization like `checkout.session.completed` does;
  `checkout.session.completed` with `payment_status=unpaid` stays an
  acknowledged no-op (the money arrives as its own event); and
  `checkout.session.async_payment_failed` is logged at warn level and links
  nothing (BEBILL-15).
- **Events that arrive before the checkout link answer 409, not 500.**
  `customer.subscription.created/updated`, `invoice.paid` and
  `invoice.payment_failed` for a customer that is not linked yet (and younger
  than 24 h) are logged at info level and answered `409 CONFLICT`; Stripe retries
  any non-2xx exactly as before. This was an error-level log and a 500 on every
  new subscription (BEBILL-25).
- `GET /billing/status` requires an unrestricted key (BEBILL-23).

### Limits

- **The email-login key is a system key.** `api_keys.is_system` marks keys
  Spooled creates itself; today that is only the `Email Login` key behind
  dashboard sessions. System keys are excluded from `max_api_keys` (a Free
  organization's second key slot is no longer used up by its first email
  login), are shown as `is_system: true` in `GET /api-keys`,
  `GET /api-keys/{id}` and the `PUT` response, and are found by the flag rather
  than by name. No request can set the flag (BEAUTH-08, BEBILL-09).
- **`max_workflows` counts only pending and running workflows** (BEBILL-10,
  BEFLOW-17). Retrying a failed workflow turns it back into a running one, so
  `POST /workflows/{id}/retry` now takes the workflows lock and pays the cap
  first: `429 QUOTA_EXCEEDED` at the cap, `403 feature_disabled` on a plan
  without workflows (W4-01).
- **Re-activating through `PUT` pays the cap.** `PUT /api-keys/{id}` and
  `PUT /schedules/{id}` with `is_active: true` check the plan cap the way create
  and `/resume` do (`429 QUOTA_EXCEEDED`), under the per-organization lock and
  against the row's current state, so two concurrent re-activations of one row
  are charged once and turning a row off never fails on quota. A re-activated
  schedule's next run is computed from now instead of firing the run it missed
  while paused (BEAUTH-09, BEBILL-06, BEFLOW-07).
- **`custom_limits` is validated.** `PATCH /admin/organizations/{id}` answers
  `400 VALIDATION_ERROR`, listing every problem at once, for a value that is not
  an object, an unknown key, a non-integer, a negative number (except `-1` on a
  resource cap) or an out-of-range number; `null` and `{}` still reset. Retention
  overrides are limited to 0..=36500 days. One table, `OVERRIDE_KEYS` in
  `src/config/plans.rs`, drives both the validation and the runtime reading
  (BEBILL-07, BEBILL-26).
- **A bad stored retention override can no longer stop retention.** The sweep
  cast `custom_limits` to an integer in SQL, so one organization with a string,
  a fraction or an out-of-range number made the retention statements fail for
  every organization, and a stored `-1` deleted all of that organization's
  completed jobs. The sweep now reads overrides through
  `custom_retention_days()`, which returns the plan default for anything that is
  not a whole number of days (BEBILL-07).
- **Admin soft delete stops everything that acts for the organization**, in one
  transaction: keys revoked, schedules paused, outgoing webhooks disabled, the
  incoming-webhook token removed, `plan_tier = 'deleted'`. The cron due-query
  skips deleted organizations, and `POST /webhooks/{org_id}/custom` answers 404
  for one, like for an unknown organization (BEFLOW-06, BEAUTH-15).
- **Documentation:** an API key's `rate_limit` is requests per minute, checked
  after the organization's plan limit, so it can only lower that limit
  (BEBILL-12); a queue's `rate_limit` is stored and returned but not enforced
  (see Deferred / known).

### Auth and rate limits

- **Client IP comes from the TCP peer.** The server now serves with the peer
  address, and every limiter resolves the client the same way: the configured
  `TRUSTED_CLIENT_IP_HEADER` from any peer; otherwise `CF-Connecting-IP` /
  `X-Real-IP` only from a private peer; then `X-Forwarded-For` counted from the
  right past `TRUSTED_PROXY_HOPS`; then the peer itself. This covers the public
  and admin limiters, the failed-auth budget (REST middleware and
  `/auth/login`), `check-email`, `check-slug`, and email-login start and verify.
  A missing configured header falls back to the peer instead of one shared
  `unknown` bucket. Before, a client that reached the backend directly could
  choose its own bucket with `CF-Connecting-IP`. The old two-argument
  `client_ip` helper is removed (BEAUTH-10).
- **An IPv6 /64 is one client.** Rate-limit ids reduce an IPv6 address to its /64
  (an IPv4-mapped address to plain IPv4), so rotating addresses inside a /64 no
  longer gives fresh buckets. Existing IPv6 counters reset at deploy
  (BEBILL-24).
- **A stranger's wrong guesses no longer lock the owner out of email login.**
  The strict verify budget is 5 guesses per 10 minutes per (address, client IP);
  a per-address ceiling of 20 per 10 minutes only makes callers wait for the
  window to end. While these Redis budgets are enforced, a login code tolerates 40
  attempts, so guessing within the budgets never invalidates the owner's code
  (without Redis the old per-code cap of 5 applies). Over a budget,
  `POST /auth/email/verify` answers `429 RATE_LIMIT_EXCEEDED` with `Retry-After`
  and a message naming the wait. The price: a guesser spread over more than four
  client addresses can make about 200 code comparisons per address per hour (it
  was about 75) out of 10^6 codes (BEAUTH-06).
- **`POST /auth/email/start` is throttled per client IP** (3 per minute and 10
  per hour; `429` with `Retry-After`; it fails open on a Redis error unless
  `RATE_LIMIT_FAIL_CLOSED` is set) and accepts an optional `turnstile_token`,
  checked with Cloudflare Turnstile when `TURNSTILE_SECRET_KEY` is set (`400
  CAPTCHA_FAILED` when it is missing, rejected or cannot be verified; it fails
  closed). A code the mail provider failed to send no longer uses up the
  address's 5 codes per hour: the row is written after a successful send, for
  login codes and for billing-email confirmation codes (BEAUTH-11, FESF-15).
- **A login code works once, even when verified twice at the same moment.** The
  code is consumed with `used_at IS NULL` and the row count checked, so parallel
  verifies of one code create one session or one signup token (BEAUTH-24).

### Migrations

- `20261012100000_stripe_superseded_subscriptions.sql` (adds
  `organizations.stripe_superseded_subscription_ids`)
- `20261012110000_system_api_keys_and_active_workflow_count.sql` (adds
  `api_keys.is_system` with a backfill of the `Email Login` keys, and redefines
  `get_org_resource_counts` on top of the draining-workers definition)
- `20261012110100_tolerant_retention_overrides.sql` (adds
  `custom_retention_days(jsonb, text)`)

### Deferred / known

- **Queue `rate_limit` is stored, not enforced** (BEBILL-11, BEJOB-12; operator
  decision). `PUT /queues/{name}/config` still accepts, validates, stores and
  returns it, but no claim path enforces it, and enforcement is not built
  (F02). The OpenAPI text and the doc comments now say so.
- **W4-02 (P3, open).** Two server checkouts paid in two browser tabs: the older
  event is skipped as stale, so its subscription stays billing and is never
  linked. Needs an owner decision, because the obvious fix cancels a
  subscription the customer has paid for.
- **W4-03 (P3, open).** Writing the code row only after the mail is sent widens
  the check-then-insert race on the 5 codes per hour cap: concurrent starts for
  one address can send more than 5 codes.
- **W4-04 (P3, open).** The per-address verify ceiling (20 per 10 minutes) can
  still be burnt by one guesser who rotates through the /64s of an IPv6 /56; the
  owner then waits out the window, and no code is invalidated.
- **W4-05 (P3, open).** No peer-aware regression test covers `/auth/login` and
  the REST failed-auth budget (the peer-aware tests cover the public limiter,
  `check-slug` and email start).
- Known billing limits: a checkout for a different plan cancels the previous
  subscription at once and without proration (the portal is the fairer way to
  change plans); invoice events are not scoped to the organization's current
  subscription, so `invoice.payment_failed` for a replaced subscription of the
  same customer can mark the organization `past_due` (paid entitlements stay);
  deleting an organization does not cancel a superseded subscription whose
  cancel is still pending; a subscription someone creates by hand in the Stripe
  dashboard for an already linked customer is ignored while the organization's
  current one is live, so switch the organization over through checkout or the
  admin API.

---

## [0.1.119] - 2026-10-11

Fixes from the 2026-10-10 bug hunt (wave 3): the gRPC API, authentication cost,
process supervision and the production configuration.

### Upgrade notes

- **Omitted gRPC leases stay 300 s, now configurable.** `Dequeue`,
  `StreamJobs`, `ProcessJobs` and `RenewLease` without a lease (proto3 `0`) get
  `GRPC_DEFAULT_LEASE_DURATION_SECS`, default 300 as before this release (`0` or
  blank also means 300, a value above 3600 is lowered to 3600, and one at or
  below `WORKER_HEARTBEAT_INTERVAL_SECS` or below 5 stops the server at startup:
  such a lease would expire between heartbeats). It is separate from `WORKER_LEASE_DURATION_SECS` (the
  REST default, 30 s on the hosted service) because the Go SDK's `StreamJobs`
  cannot send a lease and the Go and PHP `Dequeue` / `RenewLease` send none
  unless set: on 30 s their longer jobs would be handed to a second worker. An
  explicit lease is honoured as before. `Register` reports this lease as
  `lease_duration_secs`; `heartbeat_interval_secs` is
  `WORKER_HEARTBEAT_INTERVAL_SECS`.
- **`StreamJobs` holds one job per stream.** The next job is claimed only once
  the delivered one is settled (completed or failed), released, or its lease
  has run out. Run one stream per concurrent job slot (up to 100 per
  organization per server), or use `Dequeue` / `ProcessJobs`, if a worker ran
  several jobs from one stream at once.
- **Production refuses `EMAIL_PROVIDER=console`.** `docker-compose.prod.yml`
  still defaults to it, so set `EMAIL_PROVIDER` (and its API key) before
  deploying, or the container exits at startup. An unset or unknown `RUST_ENV`
  counts as production, so a local run needs `RUST_ENV=development`.
- **`GET /health` is 503 while the database is down**, so `curl -f`
  healthchecks and the container `HEALTHCHECK` now fail in that case.
- The new migration `20261011150300_count_draining_workers.sql` runs at startup
  and changes the worker count (see "Draining workers" below).

### gRPC

- **The default gRPC lease is configurable.** An omitted (or `<= 0`) lease on
  `Dequeue`, `StreamJobs`, `ProcessJobs` and `RenewLease` is
  `GRPC_DEFAULT_LEASE_DURATION_SECS` (default 300, the previous fixed value;
  `0` means unset, at most 3600 s, and a value at or below
  `WORKER_HEARTBEAT_INTERVAL_SECS` or below 5 fails startup, so a worker that
  sends no lease never gets one shorter than its heartbeat). It does not follow the REST `WORKER_LEASE_DURATION_SECS`,
  so clients that cannot send a lease keep the lease they have always had.
  `Register` returns that lease and `WORKER_HEARTBEAT_INTERVAL_SECS` instead of a
  fixed 300/10 (BEGRPC-10, W3R-03).
- **Workers can re-register under a stable id.** `RegisterWorkerRequest` has an
  optional `worker_id` (field 7). Send the same id after a restart and the
  server updates that worker instead of adding one, so a restarting worker at the
  plan's `max_workers` is no longer locked out. Ids follow the REST rules, are
  scoped to the organization, and an id another organization uses is
  `ALREADY_EXISTS`. Wire-compatible; SDKs need regenerated stubs to send it
  (BEGRPC-08).
- **`Register` enforces `max_workers` atomically**, under the same advisory lock
  and transaction as REST, so concurrent registrations can no longer overshoot
  the cap (BEGRPC-07).
- **Draining workers count toward `max_workers`** on REST and gRPC, and in
  `GET /organizations/usage`, for as long as they keep heartbeating. A worker
  that heartbeated `draining` used to drop out of the count and let an
  organization run any number of workers. One silent for 2 minutes now goes
  offline like any other stale worker and frees its slot (BEGRPC-15).
- **`Enqueue` validates like REST.** Queue name up to 100 characters, priority
  -100..100, idempotency key up to 255 characters, tags up to 64 KB of JSON, and a
  `scheduled_at` outside years 1-9999 are `INVALID_ARGUMENT`; database CHECK and
  row-size errors are `INVALID_ARGUMENT` instead of `INTERNAL`. An all-zero
  `scheduled_at` means unset (it used to store 1970). `Register` and `Heartbeat`
  apply the REST limits too (metadata 64 KB, `worker_type` and `version` 50
  characters, worker id rules); `max_concurrency` above 100 is lowered to 100
  (BEGRPC-09, BEGRPC-21).
- **Whole numbers in payloads and results stay integers.** `42` round-trips as
  `42`, not `42.0`, for values below 2^53 (BEGRPC-05).
- **Settling a finished job is `FAILED_PRECONDITION`.** `Complete`, `Fail` or
  `RenewLease` on a job that already finished returns the message
  `Job is already <status>` (`CONFLICT` inside `ProcessJobs`), like REST's 409,
  not `NOT_FOUND` (BEGRPC-17).
- **`StreamJobs` claims on demand and is capped.** A stream keeps one job with
  the worker: it claims the next job only once the previous one is settled
  (completed or failed), released, or its lease has run out, so one stream runs
  one job at a time and a worker opens one stream per concurrent job slot. Before, it claimed whenever the client's
  HTTP/2 window had room, so a worker holding one job was leased as many further
  jobs as that window held (about 64 KB of them for grpc-go), which expired
  unread and used up their retries. A job
  claimed but not yet handed over when the client disconnects goes back to
  pending without using a retry. At most 100 streams per organization per server
  (`RESOURCE_EXHAUSTED` beyond that). The rate limit is charged when a stream
  opens and per delivered job, not per idle poll, and a stream over its limit
  waits instead of ending. A change to a key's rate limit applies to open
  `StreamJobs` and `ProcessJobs` streams. A claim reads the organization's plan
  limits from the key row it already holds, so it never waits for a second
  database connection; with the cached limits gone (TTL expiry or a Redis outage)
  and as many busy streams as pool connections, that wait used to stall the
  whole process (BEGRPC-03, BEGRPC-12, BEGRPC-22, W3R-01, W3R-02).
- **Responses stay under the 4 MiB message limit.** A `Dequeue` batch stops at
  about 3 MiB (always at least one job) and the rest stay pending; a job too large
  for any message is dead-lettered with the reason instead of blocking the queue.
  A claim error part-way through a `Dequeue` batch returns the jobs already
  claimed instead of failing the call and leaving them leased (BEGRPC-04,
  BEGRPC-24).
- Dead-lettering over gRPC no longer counts an extra retry (`retry_count` and the
  DLQ `total_retries` match REST). The `job.created` webhook for gRPC jobs
  carries the real initial status (`scheduled` for a future `scheduled_at`) and
  the priority. `ProcessJobs` ends with the request stream's error (for example
  an oversized message) instead of a clean OK (BEGRPC-16, BEGRPC-18, BEGRPC-23).
- Worker RPCs: `Heartbeat` without metadata keeps the stored metadata, and with an
  empty status keeps the current status without a warning (an offline worker
  still comes back as healthy); repeated `Deregister` no longer drives the
  `workers_active` gauge negative. Worker RPCs follow `RATE_LIMIT_FAIL_CLOSED`
  like the queue RPCs (a Redis error no longer fails them with `INTERNAL` in the
  default fail-open mode). gRPC health follows database reachability (checked
  every 10 s) instead of always reporting SERVING (BEGRPC-06, BEGRPC-11,
  BEGRPC-25).

### Authentication

- **bcrypt no longer runs on the async runtime.** Every API-key verification
  (REST, `/auth/login`, gRPC) runs on the blocking pool behind one process-wide
  limit (half the cores, 1-4), and legacy-key scans run one at a time, so a burst
  of bad keys cannot stall requests, the scheduler or the health probes
  (BEOPS-04).
- **Legacy keys are not refused while a scan is busy.** REST (with `/auth/login`)
  and gRPC scan legacy keys on separate turns, so unknown keys on one transport
  never delay legacy keys on the other. Concurrent requests for the same key share
  one scan. A request waits for its turn about as long as a scan takes (measured
  on the host, plus 1 s, at most 10 s), then re-checks whether the scan it waited
  for already found its key. If the turn is still busy the answer is
  `503 SERVICE_UNAVAILABLE` with `Retry-After: 5` (gRPC `UNAVAILABLE`), never 401,
  and the key is not remembered as unknown. Before, a request waited a flat 2 s,
  less than one scan of ten keys at bcrypt cost 12, so concurrent requests for a
  dormant legacy key got 401s that SDKs do not retry (W3R-06, W3R-07, W3R-08).
- **REST, login and gRPC share one key path and one cache.** The Redis record
  cache, then a negative cache (an unknown key is refused for 60 s without
  touching the database), then the indexed `lookup_hash`, then the bounded
  legacy scan. A repeat gRPC call skips bcrypt, a REST revoke ends the gRPC key
  too, and `last_used` is written at most once per 5 minutes instead of on every
  RPC (BEGRPC-01, BEGRPC-25).
- **A per-IP failed-auth budget (20 per minute)** skips only the legacy
  `lookup_hash IS NULL` scan for an address that keeps sending unknown keys. Keys
  that match by `lookup_hash` always authenticate. Legacy keys stay supported and
  nothing is rotated or deactivated (operator decision). gRPC cannot be budgeted
  per address (the peer address it sees is the ingress), so all gRPC callers
  share one budget of 60 failures per minute (Redis key `auth_fail_grpc`) with
  the same effect: over it, gRPC skips the legacy scan (401, not negatively
  cached) until the minute is up. A budget is charged before the scan, so
  concurrent requests and requests the client cancels count too, and the charge
  is taken back when the scan finds the key or never runs. A Redis error leaves
  both budgets open (BEAUTH-05, BEGRPC-13, W3R-06, W3R-08).
- **`/auth/login` resolves keys exactly like the middleware**, so it refuses keys
  of soft-deleted organizations (401) instead of issuing tokens that every later
  JWT check rejected. A key it could not check is a 503 with `Retry-After`, not a
  500.
- **A database failure during authentication no longer leaks database text.** The
  status is `503 SERVICE_UNAVAILABLE` "Authentication is temporarily unavailable"
  (with `Retry-After: 5`) when the database cannot be reached, otherwise `500`
  "Authentication failed";
  gRPC returns `UNAVAILABLE`. It used to be a 500 with the raw sqlx message, or
  "API key is inactive". `GET /schedules/{id}/history` no longer returns database
  error text either (BEOPS-27).

### Runtime and operations

- **The process exits when a part of it dies.** It exits non-zero, so the
  container restart policy brings it back, when the gRPC port is busy or the TLS
  certificate or key is bad, or when the gRPC server, the scheduler or the metrics
  server stops. Before, the process kept running and reported healthy
  (BEGRPC-14, BEOPS-09).
- **Graceful shutdown covers gRPC.** Health turns `NOT_SERVING` first, then
  in-flight calls finish (up to 10 s) while HTTP drains. The whole shutdown, open
  SSE streams included, is bounded by the 30 s shutdown timeout, so the process
  still releases its jobs and closes the database pool (BEOPS-24).
- **`GET /health`** returns 503 with the same body when the database is down (a
  Redis outage stays 200 with `cache: false`, `status: degraded`). The body adds
  `grpc` and `scheduler` booleans; either being false makes `status` degraded
  (BEOPS-21, BEOPS-09).
- **Redis half-open connections are detected.** Every Redis call and the health
  check are capped at 500 ms, including any wait for a reconnect; three timeouts in
  a row (or a failed health ping) replace the connection, at most once per second,
  and Redis sockets use TCP keepalive. A stalled Redis used to hold requests for
  seconds (W1-02).
- **A Redis that is down at startup is picked up when it comes back.** With
  `REDIS_URL` set or `RATE_LIMIT_FAIL_CLOSED` on, the process keeps a
  reconnecting client and runs degraded (`/health`: `cache: false`,
  `status: degraded`) until Redis answers. Before, it ran without Redis until
  restarted: under `RATE_LIMIT_FAIL_CLOSED` every request was refused while
  `/health` reported healthy. Without `REDIS_URL` and fail-closed, a missing Redis
  is still optional (W3R-04).
- **Prometheus metrics are wired up.** `spooled_api_requests_total` and
  `spooled_api_request_duration_seconds` (method, route template, status class),
  `spooled_database_connections_active` and the new
  `spooled_database_connections_max` are recorded; REST failures count
  `spooled_jobs_failed_total` and `spooled_jobs_deadlettered_total`; REST and gRPC
  completions observe `spooled_job_duration_seconds`. `spooled_redis_operations_total`
  and its two alerts are gone (nothing fed them); the latency and database-pool
  alerts and the Grafana panels are corrected. The metrics server returns an error
  instead of panicking when it cannot bind (BEOPS-07).
- `POST /workflows` no longer takes a second database connection while its
  transaction holds the organization lock, so concurrent creates cannot exhaust
  the pool (BEOPS-10).
- **Rolling back to an older image no longer crash-loops** on migrations a newer
  release already applied: unknown applied migrations are ignored, and the
  checksums of the known ones are still checked (BEOPS-08).
- Dashboard `uptime_seconds` and `started_at` count from process start, not from
  the first dashboard request (BEOPS-22).
- `POST /jobs/bulk` accepts bodies up to 100 jobs at the largest plan payload cap,
  capped at 32 MiB (was axum's 2 MiB default, so 100 jobs of 25 KB were refused);
  other routes keep 2 MiB (BEOPS-16).
- Postgres (`sslmode=require` / `verify-*`) and Redis (`rediss://`) connections can
  use TLS, through rustls with the ring provider. Plain URLs behave as before. The
  bundled web PKI roots cover public CAs; a private CA may need a different root
  setup (BEOPS-14).

### Configuration and deployment

- **`docker-compose.prod.yml` forwards the settings the app reads:**
  `RATE_LIMIT_FAIL_CLOSED`, `CORS_ALLOWED_ORIGINS`, `EXTERNAL_URL`, `ENABLE_HSTS`,
  `TRUSTED_CLIENT_IP_HEADER`, `TRUSTED_PROXY_HOPS`, `OUTGOING_WEBHOOK_MAX_ATTEMPTS`,
  `OUTGOING_WEBHOOK_MAX_CONCURRENT_DELIVERIES`, `DATABASE_TEST_BEFORE_ACQUIRE`,
  `DATABASE_DIRECT_URL`, `GRPC_PORT` and the new
  `GRPC_DEFAULT_LEASE_DURATION_SECS` (default 300). A blank value counts as unset for every
  numeric and flag setting (and keeps the default CORS origins), so
  `${NAME:-}` forwarding changes nothing until you set it. Before, setting
  `RATE_LIMIT_FAIL_CLOSED` or `CORS_ALLOWED_ORIGINS` in the stack had no effect
  (BEOPS-13).
- The backend container gets `stop_grace_period: 40s` (the Docker default of 10 s
  would SIGKILL the shutdown drain), and the Kubernetes base deployment
  `terminationGracePeriodSeconds: 40` (was 30): the 30 s shutdown bound plus
  releasing jobs and closing the pool.
- **The bundled Prometheus works with `METRICS_TOKEN`.** It sends the token as a
  bearer credential and loads `docker/prometheus/spooled-rules.yml`. Before, a
  non-empty token made every scrape a 401 and no alert rules were loaded (a single
  downloaded compose file has no rules directory, which means no rules, not an
  error) (BEOPS-06).
- **Production refuses `EMAIL_PROVIDER=console`** (the default when the variable
  is unset or misspelled): login codes would only be written to the log, the API
  would still answer 200, and anyone with log access could sign in as any user.
  Development and staging are unaffected (BEOPS-17).
- Migrations run over `DATABASE_DIRECT_URL` when it is set, bypassing PgBouncer
  (the migrator's session-level lock is not reliable in transaction mode). The build
  re-runs when `migrations/` changes, so a new `.sql` file alone is embedded
  (BEOPS-19, BEOPS-32).
- The Kubernetes ingress no longer pins clients to one pod with
  `upstream-hash-by: $request_uri` (BEOPS-18).
- Migration convention for new migrations: start with `SET LOCAL lock_timeout =
  '5s'` and add foreign keys `NOT VALID`, then `VALIDATE CONSTRAINT` separately
  (W1-10). Documented in `docs/guides/deployment.md`.

### Migrations

- `20261011150300_count_draining_workers.sql` (adds
  `worker_counts_toward_cap(status, last_heartbeat)` and redefines
  `get_org_resource_counts` to use it)

### Deferred

- Requiring the claiming `worker_id` to be a live registered worker of the
  organization on claim and dequeue (BEGRPC-15, owner decision: breaking for REST
  and gRPC consumers that never call Register).
- Pending billing email, so a typo does not drop email login until the new
  address is confirmed (W1-14, moved to a later wave).

---

## [0.1.118] - 2026-10-10

Worker-semantics fixes from the 2026-10-10 bug hunt (wave 2).

### Fixed

- **The stale-worker sweep no longer re-runs jobs that are still running.** It
  released (or dead-lettered) every `processing` job of a worker that missed
  worker heartbeats for two minutes, even when the job's lease was live and
  being renewed. A worker busy in one long job (the PHP SDK sent no worker
  heartbeat while a handler ran) therefore had the job handed to someone else
  mid-run. Jobs with an unexpired lease are left alone; expired leases are
  recovered as before. A successful job heartbeat now also refreshes the
  worker's `last_heartbeat`.
- **`POST /jobs/{id}/fail` records any failure text.** An empty or over-long
  `error` was a 400, and SDK workers that forwarded an exception message
  unmodified left the job `processing` until its lease expired. Empty text is
  stored as "Job failed"; text over 4096 bytes is truncated (same rule as gRPC).
- **gRPC `Fail` without `retry` honours `max_retries`.** `FailRequest.retry` is
  now `optional`: omitted means retry like REST, only an explicit `false`
  dead-letters at once. Before, every client that did not set the field
  dead-lettered jobs on their first failure. Wire-compatible: existing clients
  that never set `retry` get the REST behaviour without upgrading.

---

## [0.1.117] - 2026-10-10

Fixes from the 2026-10-10 code-level bug hunt (wave 1).

### Fixed

- **Schedules no longer go silent for most of a day at the DST fall-back.** In
  the repeated hour, the cron stepper mistook its "already fired" guard for a
  date mismatch and jumped to the next midnight, so every hourly or `*/N`-minute
  schedule in a DST timezone stopped for about 20 hours (Europe on 2026-10-25,
  the US on 2026-11-01). In the repeated hour, a schedule whose minute is `*` or
  `*/N`, or whose hour is `*`, keeps its real-time cadence; every other schedule
  (`30 1 * * *`, `0 */2 * * *`) still fires once. A spring-forward day no longer
  makes 00:00–00:59 schedules skip the next day.
- **Redis reconnects by itself.** The cache held one `MultiplexedConnection`
  that stayed broken after any Redis restart, leaving rate limiting, logout and
  session revocation, the auth cache and email-attempt budgets dead until the
  API restarted. It now uses a `ConnectionManager` with short reconnect
  attempts (callers fall back to their fail-open / fail-closed path within a
  couple of seconds), and a failed health-check PING replaces the connection so
  a half-open socket cannot stick. `/health` reports `degraded` while a
  configured Redis does not answer, and the startup log no longer prints the
  password embedded in `REDIS_URL`.
- **Deleting a parent job no longer deletes its children.** `jobs.parent_job_id`
  was `ON DELETE CASCADE`, so retention, expiry and DLQ purges of a parent
  silently deleted child jobs in any status. It is `ON DELETE SET NULL` now, and
  indexed, so retention no longer scans the whole table per deleted row. A job
  still blocked on a dependency (parent or workflow edge) that is deleted
  before it completed is cancelled by a database trigger, whatever deleted it
  (expiry, queue delete, DLQ purge); before, it was either deleted with the
  parent or released to run an hour later. The expiry sweep cancels those
  children first so it can send `job.cancelled` (reason `parent_expired`).
- **Email login only trusts a verified billing email.** Any key holder could set
  `billing_email` to someone else's address; when that person later signed up
  by email, they were logged into the squatter's organization. Organizations
  now carry `billing_email_verified_at`; changing the address clears it, and
  email login, signup and the uniqueness check only consider verified
  addresses. Addresses are stored lower-cased. New routes confirm an address:
  `POST /api/v1/organizations/billing-email/verification` sends a code and
  `POST /api/v1/organizations/billing-email/verify` checks it (a wrong or
  expired code is a 400). When an email-login code is verified for an address
  that an unconfirmed org holds, the signup response carries
  `unverified_org_exists: true` so clients can point the owner at the
  confirmation step instead of a second signup. Existing addresses that email
  login could already resolve (stored lower-cased; the oldest of duplicates)
  are grandfathered as verified; any admin write of `billing_email` counts as
  verified.
- **Empty secrets no longer count as configured.** `ADMIN_API_KEY`,
  `STRIPE_BILLING_WEBHOOK_SECRET`, `STRIPE_SECRET_KEY`, `METRICS_TOKEN`,
  `EMAIL_API_KEY`, the SMTP settings and `EXTERNAL_URL` arriving as empty
  strings (the compose `${NAME:-}` default) used to be treated as set: an empty
  admin key opened closed registration to an empty `X-Admin-Key` header, and an
  empty webhook secret let forged billing webhooks verify.
- **Deleting an organization cancels its Stripe subscription** (self-serve and
  admin, soft and hard delete). If the cancel fails, nothing is deleted. The
  Stripe call has a 20 s timeout, and the subscription is recorded as canceled
  so a later hard delete does not cancel it twice. Stripe events no longer
  overwrite a soft-deleted organization's `plan_tier` tombstone.
- **DLQ retry and purge accept `queue` as well as `queue_name`.** Unknown fields
  are ignored, so a purge that said `queue` (as the PHP docs did) deleted
  dead-lettered jobs of every queue.
- **SSRF filter:** `::ffff:0.0.0.0` and the rest of `0.0.0.0/8`, multicast and
  reserved ranges, and private addresses embedded in NAT64 (`64:ff9b::/96`) or
  6to4 (`2002::/16`) addresses are rejected as webhook targets.
- Admin `PATCH /admin/organizations/{id}` stores `plan_tier` lower-cased;
  "Pro" used to get free-tier retention.
- Jobs cancelled because their parent or a workflow dependency failed now get
  `completed_at`, so retention eventually removes them.

### Changed

- `:latest` images are published only after CI passes on a push to `main` of
  this repository (the release workflow runs on `workflow_run` of CI instead of
  on every push; fork pull requests can never publish), one at a time, labelled
  with the commit that was tested.
- Clippy 1.99's new `double_must_use` lint is allowed on tonic's generated code.

### Migrations

- `20261010120000_parent_job_fk_set_null.sql`
- `20261010120100_billing_email_verification.sql`

---

## [0.1.116] - 2026-09-26

Dependency and toolchain maintenance (Dependabot #45, #48). No API or behaviour
change; the full suite, including `tests/qa_regression_tests.rs`, passes on the
combined tree.

### Changed

- Build image: `rust:1.98-bookworm` (was 1.96).
- `jsonwebtoken` 11 (HS256 encode/decode unchanged, so tokens issued by earlier
  versions stay valid), `validator` 0.21, `base64` 0.23, OpenTelemetry 0.33 /
  `tracing-opentelemetry` 0.34 (optional `otel` feature), `serial_test` 4 (dev),
  plus 17 semver-compatible updates in `Cargo.lock`.

---

## [0.1.115] - 2026-09-26

Follow-ups found while fixing clients and docs against 0.1.114.

### Fixed

- **`lease_duration_secs` is optional on `POST /jobs/{id}/heartbeat`.** Omitted,
  it renews for the default claim lease (`WORKER_LEASE_DURATION_SECS`, 30 s). It
  was required, and the Go SDK's `Heartbeat` leaves it out unless the caller sets
  it, so those heartbeats were all 422s. Out-of-range values are still rejected.
- **`413 PAYLOAD_TOO_LARGE` names the plan to upgrade to** (`"upgrade_to":
  "starter"`), like every quota error; it was a bare `true`.
- README: the production compose command listed a `pgbouncer` service that
  `docker-compose.prod.yml` does not define, so it failed with "no such service".

---

## [0.1.114] - 2026-09-26

Fixes from a production QA pass (2026-09-26). Every item was re-verified against
the code before changing it; regression tests are in `tests/qa_regression_tests.rs`.

### Security

- **`permissions` on an API key is rejected instead of ignored.** Spooled keys have
  no per-permission scopes, but `POST /api-keys {"permissions": ["jobs:read"]}`
  returned 201 and a key with full write access. The field is now a 400
  `VALIDATION_ERROR` on create and update; `queues` remains the way to narrow a key.
- **Logout ends the whole session.** Access and refresh tokens from one login share
  a session id (`sid` claim, carried through `/auth/refresh`), and `POST /auth/logout`
  revokes it — so the refresh token no longer keeps minting access tokens for ~24
  days when the client does not send it in the logout body. Tokens issued before
  this release keep the previous per-token behaviour.
- **`rustls` 0.23.45** for [RUSTSEC-2026-0285](https://rustsec.org/advisories/RUSTSEC-2026-0285).

### Fixed

- **`GET /api/v1/events` now streams job events.** It only ever emitted
  `system.health`; it now forwards the organization's realtime events (the same
  Redis channel the WebSocket reads) as named SSE events — `job.created`,
  `job.status`, `job.completed`, `job.failed` — with the documented
  `{"type", "data"}` envelope, honouring `queue`, `job_id`, `events` and the key's
  queue scope. The subscription is live before the first `: connected` frame.
  Capped at 100 streams per organization.
- **`job.failed` is published on every reported failure** (docs: "may retry"), and
  `job.completed` carries the real `duration_ms` (claim → completion) instead of 0.
- **The queue quota counts implicit queues.** A queue exists once it has a config
  row, holds jobs, or is targeted by an active schedule. `max_queues` is enforced
  when an enqueue (REST, bulk, gRPC), schedule or workflow would create a new queue,
  and `GET /organizations/usage` reports the real count (a free org showed "1/2"
  while using 4). Jobs into existing queues are never blocked, so an org already
  over its cap keeps working; configuring an implicit queue is not charged twice.
- **Queue config defaults apply to new jobs.** Jobs that omit `max_retries` /
  `timeout_seconds` take them from `PUT /queues/{name}/config` before falling back
  to the server defaults (REST, bulk, gRPC, webhook ingest, schedules, workflows).
- **Worker outcomes say what happened.** A `lease_id` that does not match a still
  live lease is `409 LEASE_EXPIRED` with a "does not match" message (not "expired");
  completing an already-completed job is `409 CONFLICT` "Job is already completed"
  (was 404).
- **Manual `POST /jobs/{id}/retry` resets `retry_count` to 0** and clears the DLQ
  audit row, like `POST /jobs/dlq/retry`. It used to increment, leaving
  `retry_count > max_retries` so the next failure skipped every retry.
- **Every error response is JSON with `code` and `message`.** Extractor rejections
  (e.g. a missing field on `/billing/portal`), plain-text handler errors ("Invalid
  cron expression", "Schedule not found"), unknown routes and 405s now share the
  standard body; legacy fields such as `error` and `details` are kept. Truncated
  JSON is reported as malformed, not "Request body is empty".
- **Schedule history** rows carry `job_status`, the live status of the job the run
  enqueued; `status: completed` only ever meant the enqueue succeeded.
- **`POST /queues/{name}/pause` on a queue that does not exist is 404** (it created
  a config row that bypassed the queue cap). `DELETE /queues/{name}` on a queue with
  no config but finished jobs is a 409 pointing at `?delete_jobs=true`, not a 404.
- **Billing emails are unique among live organizations** on every create/update
  path, including admin; email login resolves an account by that address and used
  to land in an arbitrary org when two shared it. Comparison is case-insensitive.
- **Admin `GET /admin/organizations` rejects an unknown `sort_by` / `sort_order`**
  with 400 instead of silently sorting by `created_at`.
- **Closed-registration signup message.** Completing signup directly against the API
  without the admin key now explains that signup finishes on the website and that
  the verified signup token was not consumed.
- REST claim's default lease reads `WORKER_LEASE_DURATION_SECS` (30 s, unchanged)
  instead of a hard-coded 30.

### Migration

- `20260926120000_count_implicit_queues.sql` adds `get_org_queue_count()` (loose
  index scan over `jobs(organization_id, queue_name, …)`) and redefines
  `get_org_resource_counts()` to use it.

---

## [0.1.113] - 2026-09-10

Toolchain and dependency maintenance. No API or behaviour change.

### Security

- **`h2` bumped to 0.4.19** for [RUSTSEC-2026-0258](https://rustsec.org/advisories/RUSTSEC-2026-0258)
  (unbounded empty `DATA` frames — an HTTP/2 denial-of-service reachable on any
  public listener). `cargo audit` had started failing on the 0.1.112 tree; the
  advisory is newer than the release, not a regression in it.

### Fixed

- **CI clippy failed on Rust 1.98.** `clippy::result_large_err` now fires on
  `Result<Response, Response>` — the signature axum requires for `from_fn`
  middleware and rejections — because `axum::http::Response<Body>` is exactly
  the lint's 128-byte threshold. The return type is fixed by the framework and
  cannot be boxed, so the lint is allowed in `Cargo.toml` (not in the CI
  invocation, so a local `cargo clippy` matches the runner). Two pre-existing
  `useless_borrows_in_formatting` hits in `tests/real_api_tests.rs` are fixed
  properly.

---

## [0.1.112] - 2026-09-10

Contract and correctness pass driven by an SDK/dashboard parity review: every
finding below is a place where a documented or client-consumed response field
disagreed with what the handler actually did. No endpoint was removed or
renamed, and every response change is additive except where noted.

### Fixed

- **Email signup in `REGISTRATION_MODE=closed` rejected every completion, including
  the marketing-site Pages Function that sends `X-Admin-Key`.** `complete_signup`
  was tightened in 0.1.111 to honour registration controls, but it treated
  anything other than `open` as disabled and never looked at the admin key.
  Production runs `REGISTRATION_MODE=closed` with `EMAIL_SIGNUP_ENABLED=true`, so
  from 0.1.111 until this release every email signup returned
  `403 ACCESS_DENIED`. `POST /organizations` already accepts a matching
  `X-Admin-Key` in closed mode; signup complete now does the same.
  `EMAIL_SIGNUP_ENABLED=false` still blocks the email flow entirely, admin key
  or not, and `invite` mode still returns 403.
- **`GET /auth/check-email` never told the signup page that email signup was off.**
  The marketing form already stops when `signup_enabled === false`, but the
  handler only returned `available`/`exists`, so a disabled flow still sent a
  code and failed at `signup/complete`. The response now includes
  `signup_enabled` from `EMAIL_SIGNUP_ENABLED`.
- **`POST /webhooks/{org_id}/custom` discarded the job id.** The handler already
  computed `returned_id` and whether the insert created a row, then answered
  `200` with an empty body. OpenAPI `WebhookResponse` documents `job_id`,
  `queue_name`, and `status`; the response now sends those fields, all three read
  back from the upsert's `RETURNING`. A replayed delivery therefore reports the
  existing job's real queue and current status (`processing`, `completed`, …)
  rather than the request's queue and a hardcoded `pending`.
- **`GET /jobs` and `GET /jobs/dlq` summaries omitted `job_type` and `last_error`.**
  `JobSummary` carried neither, so any client listing jobs showed a blank type
  column and a blank error column on dead-lettered jobs; the error was reachable
  only through a per-job `GET`. `job_type` is read from `payload.job_type` (there
  is no column) and is an empty string when absent; `last_error` is null when the
  job has never failed.
- **`PUT /queues/{name}/config` treated an omitted field as a request to reset it.**
  The upsert filled omitted values from configuration defaults, so an update that
  sent only `max_retries` re-enabled a paused queue (`enabled` defaulted to true)
  and replaced `settings` with `{}`, discarding the pause metadata stored there.
  Omitted `max_retries`, `default_timeout`, `enabled`, `rate_limit`, and
  `settings` now keep the queue's current values, and an incoming `settings`
  object is merged over the stored one rather than replacing it.
- **`DELETE /queues/{name}?delete_jobs=true` ignored the flag.** The parameter is
  in OpenAPI and the dashboard's "also delete all jobs" checkbox sends it, but the
  handler only ever deleted the `queue_config` row and still returned `409` while
  pending or processing jobs existed. It now deletes the queue's jobs, its
  `dead_letter_queue` rows (which have no foreign key to `jobs`), and the config,
  in one transaction. Without the flag the previous config-only, 409-if-busy
  behaviour is unchanged.
- **`POST /jobs/{id}/dependencies` could mark a job runnable when the dependency
  check failed.** The `check_job_dependencies_met` result used
  `unwrap_or(true)`, so a SQL or decode error was read as "dependencies met" and
  the job was released with its dependencies unsatisfied. The error now fails the
  request.
- **`PUT /organizations/{id}` and `PATCH /admin/organizations/{id}` could silently
  destroy the inbound webhook token.** Both replace `settings` wholesale and
  preserved `webhook_token` through `as_object_mut()`, which is `None` for a JSON
  array, string, or number — so a non-object `settings` body dropped the ingest
  secret even though `POST .../webhook-token/clear` deliberately refuses to remove
  it. A non-object `settings` is now rejected with `400` while a token is
  configured; object bodies keep preserving an omitted token.
- **`POST /organizations/webhook-token/regenerate` reported a token it had not
  stored.** The same `as_object_mut()` guard skipped the write when `settings` was
  a non-object, and the handler still returned the new token, so the caller
  configured a secret the server would never accept. Regeneration now always
  persists into an object.
- **`GET /organizations/usage` returned `500` for free-plan organizations.** Free
  caps workflows at `0`, and `current / 0` is `NaN` (or `Inf` above zero), which
  `serde_json` refuses to serialize. A limit of `0` now means "disabled" and
  reports `percentage: null`, which also stops an infinite percentage from
  raising a bogus "limit reached" warning.

### Changed

- `JobSummary` is documented in OpenAPI with the field names it actually
  serializes: `attempt` (not `retry_count`), plus `max_retries`, `job_type`, and
  `last_error`.

---

## [0.1.111] - 2026-08-16

Remediation of a full-backend audit (35 findings; see `AUDIT_2026-08-16_BUG_PLAN.md`).
Four of the 35 were proved invalid during verification and one new finding was
discovered while implementing.

### Breaking

- **API keys are no longer accepted in the query string on REST endpoints.** Use
  `Authorization: Bearer <key>`. The realtime endpoints (`/api/v1/ws`,
  `/api/v1/events`, `/api/v1/events/jobs/{id}`, `/api/v1/events/queues/{name}`)
  still accept `?token=` / `?api_key=`, because browser `EventSource` and
  `WebSocket` cannot set headers. Query strings are retained by proxy and CDN
  access logs, tracing spans, browser history and `Referer`, which is not an
  acceptable resting place for a credential that defaults to never expiring
  (CWE-598). Query-string auth was never part of the OpenAPI contract.
- `PUT /api/v1/outgoing-webhooks/{id}` with `{"secret": null}` now **clears** the
  signing secret instead of being ignored. Omit the field to keep the current
  value. Clients that serialise unchanged fields as explicit `null` will clear
  secrets they meant to keep.

### Security

- Outgoing-webhook DNS is now resolved by a validating resolver that checks every
  address the HTTP client is about to connect to, on every lookup and on every
  redirect hop. Validating the URL and then letting the client resolve the name
  again independently left a DNS-rebinding window that the previous code's own
  doc comment claimed to close.
- Public and admin rate limits no longer key on the leftmost `X-Forwarded-For`
  entry, which the caller supplies: a rotating header value put every request in
  a fresh bucket, removing rather than degrading the limiter in front of
  `/auth/login`, `/auth/email/*`, `/organizations` and the admin routes. Client
  IP now comes from a configured edge header, or from `X-Forwarded-For` counted
  from the right past a configured number of trusted hops. Default is to trust
  nothing, which is correct for self-hosting.
- A queue-scoped API key can no longer read outgoing-webhook delivery history or
  re-fire deliveries; those rows carry job payloads from every queue and the two
  handlers were missing the `require_unrestricted_key` check the rest of the file
  has.
- Anyone who knew an email address could invalidate that account's in-flight
  login code by requesting a new one, and after five requests in an hour lock the
  account out entirely. Verification now accepts any live code for the address.
  The per-code attempt cap is unchanged, so this grants an attacker no additional
  guesses.
- The API-key cache key is now the full SHA-256 rather than a 64-bit truncation.
  A cache hit is trusted without re-running bcrypt, so that digest was the only
  margin on the cached path.
- `/api/v1/auth/check-email` now validates its input and throttles through Redis
  instead of writing a permanent row per request into `email_login_codes` — a
  table it also scanned twice per request, and which the login path scans on
  every attempt.

### Fixed

- **Billing:** a crash, OOM kill or deploy between claiming a Stripe event and
  running its handler permanently discarded that event — every retry was answered
  `200 OK, already processed`. Claims now record completion, and a stale
  in-flight claim is re-claimable.
- **Billing:** the Stripe signature HMAC is computed over the raw request body
  rather than a lossy UTF-8 conversion of it.
- **Quota:** the bulk-enqueue refund called the increment function with a negative
  count. That function assigns rather than adds across a daily reset, so a refund
  landing after midnight set the day's counter negative and granted free quota,
  and it also decremented the lifetime `total_jobs_created`. Refunds now go
  through a dedicated function that floors at zero, never crosses a reset
  boundary, and leaves lifetime totals alone. Existing drift is repaired by the
  migration, and a `CHECK` constraint prevents recurrence.
- **Quota:** the schedule-trigger, custom-webhook, workflow-create and cron paths
  still used the pre-`v0.1.91` read-then-blind-write pattern and could overshoot
  `jobs_per_day` in proportion to concurrency. All four now use the atomic
  reservation, reserving before insert and refunding on failure.
- **Dead letters:** the stale-worker sweep moved jobs to `deadletter` without
  writing the `dead_letter_queue` audit row — the one dead-letter path of five
  that skipped it, and the one covering "the worker holding this job vanished".
  Every dead-letter path is now atomic in a single statement, so status and audit
  row cannot half-apply. Dead-lettered jobs also clear `lease_expires_at`.
- **Workflows:** a job whose dependency was deleted by retention became invisible
  to both the dequeue predicate and the cancellation sweep — permanently pending
  and permanently counted against `active_jobs`. The sweep now releases such jobs
  after a grace period.
- **Workers:** the built-in worker's heartbeat wrote the deprecated
  `current_jobs`/`max_concurrency` columns, so the API always reported zero active
  jobs and any worker above five concurrent jobs marked itself `degraded`.
- `POST /api/v1/workers/register` accepts an optional `worker_id`, so a restarting
  worker reclaims its row instead of leaking one that occupies the plan's worker
  cap until the reaper clears it. Re-registration is not charged against the cap.
- The `workers_active` gauge no longer drifts negative when an already-offline
  worker is deregistered again.

### Changed

- Outgoing-webhook deliveries are bounded by a semaphore
  (`OUTGOING_WEBHOOK_MAX_CONCURRENT_DELIVERIES`, default 64). Delivery tasks were
  spawned without limit from the enqueue and complete hot paths and each held a
  payload clone for up to a minute, so one tenant pointing a webhook at a
  black-holing endpoint could exhaust process memory and sockets.
- **Outgoing webhooks now auto-disable after 20 consecutive failed deliveries**,
  with `last_status = "auto_disabled"`. `failure_count` is also counted once per
  delivery rather than once per attempt — it was previously inflated by the retry
  count and read by nothing.
- `outgoing_webhook_deliveries` and expired `email_login_codes` are covered by the
  per-organization retention sweep. Neither had any retention; delivery rows store
  a copy of the job payload and only the newest 100 per webhook are ever readable.
- `api_keys.last_used` is written at most once per key per five minutes instead of
  once per request. Every authenticated request previously issued an `UPDATE` to
  the same row, serialising requests on that row's lock during authentication.
- Cached per-organization rate limits are invalidated on plan change and on
  organization delete; they were cached for 60 seconds and never invalidated.
- Removed `src/api/middleware/rate_limit.rs`, which was never mounted and failed
  open in every degraded branch while its doc comment claimed the opposite, and
  `EventBroadcaster`, which was never constructed outside its own tests and
  advertised org isolation it did not implement.

### Added

- `OUTGOING_WEBHOOK_MAX_ATTEMPTS` (default 5) and
  `OUTGOING_WEBHOOK_MAX_CONCURRENT_DELIVERIES` (default 64).
- `TRUSTED_CLIENT_IP_HEADER` and `TRUSTED_PROXY_HOPS`. When unset, `CF-Connecting-IP`
  and `X-Real-IP` are consulted by default, so a deployment behind Cloudflare or
  nginx gets per-client rate limiting without configuration; `X-Forwarded-For` is
  used only when `TRUSTED_PROXY_HOPS` says how many entries our own proxies
  appended, and is counted from the right.
- Migration `20260816120000_quota_refund_and_stripe_claim_state.sql`.
- The integration suite can now run without Docker. `TEST_DATABASE_URL` and
  `TEST_REDIS_URL` point the harness at existing services; each test still gets
  its own freshly-migrated database, dropped afterwards. Previously the suite was
  silently unrunnable whenever the Docker engine was unavailable.

### Testing

- Full suite verified on both paths — **1098 passed, 0 failed, 3 ignored** against
  local PostgreSQL 17 + Redis, and identically via testcontainers (PostgreSQL 16,
  the CI path). Clippy and rustfmt clean with `--all-features`.
- Note for future changes: six of the eight files in `tests/` are behind the
  `docker-tests` feature and `default = []`, so **`cargo test` without
  `--all-features` compiles only a fraction of the suite and can report success
  while the integration tests are unbuilt**. Two compile errors in this release
  were only exposed by adding the flag.

## [0.1.110] - 2026-07-19

### Fixed

- Outgoing webhook delivery now performs bounded in-process retries instead of only logging retry intent, and the manual retry endpoint now sends the retry immediately instead of only marking it pending.
- OpenAPI and webhook/gRPC guides now match current request fields, completion webhook envelope shape, gRPC API-key auth, and outgoing webhook headers/events.
- OpenAPI retry/worker schemas now match runtime response bodies and required lease fencing fields.
- REST worker heartbeat and detail responses now use the current worker columns while keeping stable public JSON names (`current_jobs`, `max_concurrency`, `registered_at`).
- Invoice Stripe webhooks no longer advance shared `stripe_last_event_at`, so out-of-order `invoice.*` events cannot suppress a legitimate `subscription.*` plan/status change.

## [0.1.109] - 2026-07-15

### Added

- Optional per-job `completion_webhook_secret` (REST create). When set, completion delivery signs with the same `X-Spooled-Timestamp` / `X-Spooled-Signature` HMAC as org outgoing webhooks. Unset → unsigned (backward compatible).
- `RATE_LIMIT_FAIL_CLOSED` (default `false`). When `true`, Redis missing/errors return HTTP 429 / gRPC `RESOURCE_EXHAUSTED` instead of fail-open. Set `true` on hosted SaaS.

### Fixed

- **BE-15:** `POST /outgoing-webhooks/{id}/retry/{delivery_id}` binds TEXT path ids (was Uuid → Postgres type mismatch / 500).

## [0.1.108] - 2026-07-15

### Fixed

- **Expired-lease deadletter boundary.** Scheduler deadletter sweep and `QueueManager::recover_expired_leases` now use `lease_expires_at <= NOW()`, matching the reclaim path. With `<`, a lease at the exact expiry tick was neither worker-usable nor deadletterable until the next tick.

### Fixed (already on tip / prior)

- `GET /api/v1/jobs` accepts `queue` as a serde alias for `queue_name` (canonical OpenAPI name unchanged).
- `GET /api/v1/jobs/stats` honors optional `queue_name` / `queue` filter (previously org-wide only; query param was ignored).

### Tests

- DST spring-forward gap skip; Europe/Kyiv EET/EEST + fall-back no double-fire (`models/schedule.rs`).

## [0.1.107] - 2026-07-15

### Fixed

- **gRPC/REST/webhook/schedule/workflow default parity.** gRPC Enqueue omit/`<=0` now reads `QUEUE_DEFAULT_MAX_RETRIES` / `QUEUE_DEFAULT_TIMEOUT_SECS` (was hardcoded 3/300). Incoming custom webhooks, schedule create, and workflow job create use the same settings.
- **Admin route rate limit.** Admin API is IP rate-limited via Redis fixed window (60/min) when Redis is configured.
- **Checkout unpaid gate.** `checkout.session.completed` with explicit non-paid `payment_status` is ignored (empty status still allowed for older fixtures).

### Docs

- Agent knowledge base under `docs/ai/knowledge/`; `certs/README.md` clarifies local-dev key material.

## [0.1.106] - 2026-07-15

### Fixed

- **gRPC Enqueue `max_retries` default.** Omitted proto3 `max_retries` was stored as `0`, so the first failure deadlettered immediately. REST omit uses `QUEUE_DEFAULT_MAX_RETRIES` (3). gRPC now treats `max_retries <= 0` as default `3`, matching the existing `timeout_seconds <= 0 → 300` pattern.

## [0.1.105] - 2026-07-14

### Fixed

- Cron schedule fires now emit `schedule.triggered` + `job.created` outgoing webhooks (scheduler previously only published Redis).
- Bulk enqueue, workflow create, incoming custom webhook ingest, and manual schedule trigger now emit `job.created`.
- gRPC `StreamJobs` / `ProcessJobs` now emit `job.started` / `job.completed` / `job.failed` and per-job `completion_webhook` (parity with unary + REST).
- gRPC unary Fail now fires per-job `completion_webhook` on terminal failure.
- Scheduler cascade cancels (parent/dependency failure) emit `job.cancelled`.
- Outgoing delivery rows persist the signed envelope `id` (retry no longer re-wraps stored envelopes).
- gRPC `GetJob` miss returns `NotFound` instead of empty `{}`.
- `POST /auth/logout` soft-parses empty JSON bodies (no more 422 with `Content-Type: application/json`).
- Job retry returns `404` when missing vs `409` when wrong status.

## [0.1.104] - 2026-07-14

### Fixed

- **Outgoing org webhooks now dispatch on real events.** `OutgoingWebhookService::dispatch_event` was never called from REST or gRPC handlers, so CRUD + `/test` worked while `job.created` / `job.started` / `job.completed` / `job.failed` / `job.cancelled` / `queue.paused` / `queue.resumed` / `worker.registered` / `worker.deregistered` / `schedule.triggered` produced zero deliveries.
- **Per-job `completion_webhook` now fires** on complete and terminal fail (REST + gRPC). Previously the URL was stored but never delivered.
- Outgoing delivery signing aligned with `/outgoing-webhooks/{id}/test` (per-webhook secret + `timestamp.payload` HMAC). Delivery attempts now update `last_triggered_at` / `last_status` / `failure_count`.

## [0.1.103] - 2026-07-14

### Fixed

- Compose zero-touch metrics/Grafana bootstrap (no busybox Exited init helpers).
- gRPC TLS helper stays Up/healthy; `GRPC_TLS_INIT_ONCE` escaped for Compose interpolation.
- Drop fixed container name on TLS helper so `compose run --rm` does not clash with the live stack.

### Changed

- Prefer `BACKEND_IMAGE=:latest` for Portainer Pull and redeploy; public Portainer guide scrubbed of private host paths.

## [0.1.102] - 2026-07-14

### Fixed

- rustfmt `require_lease_id` so CI format gate passes.

## [0.1.101] - 2026-07-14

### Fixed

- Restore workflow unit-test fixtures after adding optional `expires_at` to job definitions.

## [0.1.100] - 2026-07-14

### Security

- Block queue-scoped API keys from org control-plane actions: organization update/delete, members/usage, inbound webhook token read/rotate, billing portal, and outgoing webhook CRUD/test.
- Redact `settings.webhook_token` from `GET /organizations/{id}` for queue-scoped keys.
- Scope `GET /jobs/stats` and `GET /dashboard` aggregates to the caller's queue allow-list.
- Require exact `lease_id` on gRPC complete/fail/renew (and ProcessJobs stream) — empty lease no longer bypasses fencing.
- Reject cross-queue `parent_job_id` dependencies (parent must be same queue).

### Fixed

- Prevent transient REST and gRPC authentication failures while legacy API keys are being backfilled with indexed lookup hashes.
- Schedule resume now enforces the same `max_schedules` quota as create (pause→create→resume can no longer overshoot).
- Cancelled jobs (and workflow-cascade cancels) set `completed_at` so retention cleanup can age them out.
- Cancelled workflows stay terminal: progress trigger no longer flips them back to completed/failed/running.
- Workflow job definitions now persist optional `expires_at` / `expiresAt` onto created jobs.

## [0.1.99] - 2026-07-13

Release-validation follow-up for `0.1.98`.

### Tests

- Update integration tests to assert that leased worker operations require the current lease token.

## [0.1.98] - 2026-07-13

Security hardening for worker queue scope, lease fencing, and image secret handling.

### Security

- Require runtime-mounted gRPC TLS secrets instead of baking tracked certificate material into backend images.
- Block queue-scoped API keys from administering API keys.
- Enforce worker queue scope on REST and gRPC worker lifecycle operations.
- Require the current lease token for worker complete, fail, and heartbeat operations.

## [0.1.97] - 2026-07-12

Security authorization and realtime containment fixes.

### Security

- **Queue-scoped authorization is applied consistently across more operations.**
  Restricted API keys can no longer preserve or grant broader key scopes through
  omitted queue updates, and queue scope is enforced for worker mutations plus
  schedule and workflow visibility and lifecycle operations.
- **Realtime connections now track current authorization more closely.** WebSocket
  and SSE paths revalidate key, organization, expiry, and queue scope during
  protected polling or event delivery; subscription filters are enforced for queue
  and job events, and failures close or suppress streams rather than exposing data.
- **gRPC queue operations use current key state and queue scope.** Streaming and
  worker mutation paths re-check authorization and apply queue restrictions before
  returning or changing jobs.

## [0.1.96] - 2026-07-11

Dependency + toolchain catch-up (all open Dependabot PRs landed).

### Changed

- **Rust deps (Dependabot #39):** sqlx 0.8 → 0.9, hmac 0.12 → 0.13, plus the
  rest of the grouped cargo bump. Compatibility fixes:
  - wrap audited dynamic `ORDER BY` admin list queries in `sqlx::AssertSqlSafe`
    (sqlx 0.9 rejects raw `String` SQL);
  - import `hmac::KeyInit` wherever `Hmac::new_from_slice` is used (hmac 0.13).
- **Docker builder (Dependabot #34):** `rust:1.94-bookworm` → `1.96-bookworm`.
- **CI actions (Dependabot #37 / #27):** `actions/checkout` 6 → 7;
  `softprops/action-gh-release` 2 → 3.

## [0.1.95] - 2026-07-11

Security hardening from the post-F9 adversarial pass (billing, soft-delete, gRPC streams).

### Security

- **Canceled Stripe subscriptions can no longer restore a paid `plan_tier`.**
  `customer.subscription.updated` (and admin/checkout reconcile) mapped tier from
  price alone. After `subscription.deleted` set `plan_tier=free`, a same-or-later
  `updated` with `status=canceled` re-applied the price-mapped paid tier via
  `COALESCE`. Terminal statuses (`canceled` / `unpaid` / `incomplete_expired`)
  now force `free`; `past_due` / `active` / `trialing` still use the price map.
- **Billing portal `return_url` is origin-allowlisted.** Any API-key holder could
  mint a Stripe portal session that redirected the victim to an attacker URL after
  exit. `return_url` must match `CORS_ALLOWED_ORIGINS` (https; http localhost only
  outside production).
- **Soft-deleted orgs stay dead.** Three revival paths closed:
  - email `verify` rejected a pre-delete login code that still carried
    `organization_id` and would mint a fresh Email Login key;
  - admin `POST .../api-keys` refused `plan_tier=deleted` orgs;
  - REST + gRPC auth join `organizations` and reject `plan_tier='deleted'`
    (JWT path checks org status explicitly).
- **gRPC stream auth no longer lasts forever.** `StreamJobs` re-checks
  key active/expiry + org not-deleted ~every 30s and rate-limits every poll;
  `ProcessJobs` re-checks every 32 messages. Revoke now ends open streams.
- **`ProcessJobs` matches unary validation** for empty `worker_id` and 1 MiB
  result size (was unbounded / empty-worker-accepting).

### Known follow-ups (deferred)

- `StreamJobs` claim-then-send on disconnect still leaves the job leased until
  expiry (scheduler reclaim); no eager release.
- No per-org gRPC stream cap (only HTTP/2 `MAX_CONCURRENT_STREAMS`).
- Staging still uses permissive CORS (`RUST_ENV=staging`); only Production
  restricts origins.
- `EMAIL_SIGNUP_ENABLED=false` does not block `POST /organizations` when
  `REGISTRATION_MODE=open`.

## [0.1.94] - 2026-07-11

Closes the last open external-audit finding (F9): lease fencing by `lease_id`.

### Added

- **Lease fencing tokens (audit F9).** Claim/dequeue responses now return the job's
  `lease_id` on every path (REST `POST /jobs/claim` → `ClaimedJob.lease_id`; gRPC
  `Job.lease_id` field 19 on `Dequeue`/`StreamJobs`/`ProcessJobs`). Complete, fail,
  and heartbeat/renew-lease accept an optional `lease_id` (REST body field; gRPC
  `CompleteRequest.lease_id = 4`, `FailRequest.lease_id = 5`,
  `RenewLeaseRequest.lease_id = 4`). When the token is provided, the operation
  succeeds only if it matches the job's *current* lease — a stale completion from a
  superseded lease (even by the same worker, e.g. after a reclaim + re-claim) now
  gets `409 LEASE_EXPIRED` / gRPC `FAILED_PRECONDITION` instead of silently winning.
  Omitting the token preserves the legacy `worker_id` + expiry fence, so existing
  SDKs and workers keep working unchanged; SDK releases that echo the token close
  the gap end-to-end.

### Fixed

- **Latent prepared-statement type collision between the REST and gRPC job-complete
  SQL.** The two paths use a byte-identical SQL string but previously bound the
  `result` parameter with different wire types (TEXT JSON string vs binary JSONB).
  sqlx caches prepared statements per connection keyed by the SQL text, so once one
  path prepared the statement on a pooled connection, the other path's completions
  on that connection failed with `invalid input syntax for type json`. Both gRPC
  complete paths now bind the result as a TEXT JSON string, matching REST.
- Integration-test seed helper inserted organizations with nonexistent
  `plan`/`status` columns and swallowed the error, so the lease-enforcement suite
  silently never seeded (FK failures). Fixed the columns and made seeding failures
  panic; the suite (now 12 tests incl. 3 lease_id-fencing tests) runs for real.

## [0.1.93] - 2026-07-11

Remediation of an external audit (all 10 findings verified against the code first).

### Security

- **Queue scope is now enforced on every path, not just enqueue/claim.** An API key's
  `queues` list is an intra-org least-privilege boundary, but it was only checked when
  creating/claiming jobs. A queue-scoped key could still read, cancel, retry, boost,
  and purge-DLQ jobs, list/pause/delete queue config, receive realtime events, and
  (via workflows/schedules) *enqueue* into queues outside its scope. Scope is now
  enforced uniformly across:
  - REST jobs read/control (`get`, `list`, `cancel`, `retry`, `boost_priority`,
    `list_dlq`, `retry_dlq`, `purge_dlq`, `batch_status`) — out-of-scope jobs are
    invisible (same "not found" as a missing job, no existence leak);
  - REST queues (`list`, `get`, `stats`, `update_config`, `pause`, `resume`, `delete`);
  - indirect enqueue (workflow job definitions, schedule create + trigger);
  - realtime — the WebSocket event fan-out now drops events for out-of-scope queues
    even with no client filter (previously a firehose), and the SSE job/queue streams
    enforce scope;
  - gRPC streaming (`StreamJobs`, `ProcessJobs` per message), which bypassed the check
    the unary `Dequeue` already had.
  Unrestricted keys (empty list or `*`) are unaffected. **Behavior change:** existing
  queue-scoped keys that previously saw/controlled all queues are now restricted to
  their listed queues.
- **A queue-scoped key can no longer mint or broaden a key beyond its own scope.**
  `POST/PUT /api-keys` now rejects granting the `*` wildcard or any queue outside the
  caller's scope (previously a leaked worker key could create an org-wide key). New
  `ApiKeyContext::can_grant_queues`.
- **JWT auth now honors current key expiry and queue scope.** The JWT middleware read
  only `is_active` and trusted the token's cached `queues`, so a JWT could outlive its
  key's `expires_at` and retain a scope the key no longer had. It now reads
  `expires_at` and the current `queues` from the database on every request.
- **Self-serve email signup honors registration controls, and can't revive deleted
  orgs.** `complete_signup` now enforces `EMAIL_SIGNUP_ENABLED` + `REGISTRATION_MODE`
  (previously defined but never checked), and the email-login lookups exclude
  `plan_tier = 'deleted'` organizations so a soft-deleted org can't be reactivated with
  a fresh key.

### Fixed

- **gRPC `ProcessJobs` retry backoff was still in minutes.** The v0.1.91 fix corrected
  the unary `Fail` path but missed the `ProcessJobs` stream fail path, which still used
  `2^retry_count` **minutes** (uncapped). Now `2^min(retry,6)` seconds capped at 60s,
  matching the rest.
- **gRPC dequeue paths now respect paused queues.** The unary/stream/process gRPC claim
  SQL ignored `queue_config.enabled = false`, so gRPC workers drained a queue the REST
  path had paused. All three now skip paused queues.

### Known follow-ups (deferred)

- Lease fencing keys on `worker_id` + expiry, not `lease_id`, so a stale completion
  from a re-leased same-worker job can win. A correct fix requires returning `lease_id`
  to workers (proto + SDK changes) and is tracked separately.
- Frontend `security.astro`/`privacy.astro` overstate DB-TLS and RBAC — corrected in
  the frontend repo separately.

## [0.1.92] - 2026-07-10

Fixes from a second independent audit of the running service.

### Security

- **`/auth/login` no longer bcrypt-scans every prefix-matching key.** The v0.1.90
  `lookup_hash` fast-path was applied to the auth middleware and gRPC but missed the
  login handler, so `POST /api/v1/auth/login` still did `WHERE key_prefix = $1 ...
  LIMIT 100` and bcrypt-verified each candidate — an *unauthenticated* endpoint that
  cost ~seconds of CPU per bogus `sp_live_*` key (a cheap amplification/DoS vector,
  and the per-full-key-hash rate limit doesn't stop an attacker rotating keys). Login
  now resolves via the indexed `lookup_hash` (single row + one bcrypt), with the same
  bounded legacy fallback + lazy backfill as the middleware. Bogus login dropped from
  ~3.3s to ~ms.
- **WebSocket `/ws` no longer re-implements auth, closing a refresh-token bypass.**
  The handler decoded `?token=` itself with `Validation::default()` and no
  token-type check, so it accepted **refresh tokens** (which must never grant data
  access) and scoped the event stream by that token's org instead of the
  `authenticate_api_key` middleware's authenticated `ApiKeyContext` — letting a Bearer
  identity and a `?token` identity disagree (a confused-deputy). `/ws` now scopes to
  the middleware's `ApiKeyContext`, exactly like the SSE handlers. The middleware
  already rejects refresh tokens, blacklisted tokens, and inactive keys, so it is the
  single source of truth; `/ws` also now accepts a plain Bearer API key like SSE.

## [0.1.91] - 2026-07-09

Fixes from an independent third-party audit of the running service.

### Fixed

- **Payload-too-large is now HTTP 413, not 200.** The plan payload-size rejection
  built its JSON body with `Json(..).into_response()`, which defaults to HTTP 200,
  so clients received an `error: payload_too_large` body under a *success* status
  and treated the rejected job as accepted. It now returns 413 with a stable
  `code: "PAYLOAD_TOO_LARGE"`. Affects every path that enforces the plan payload
  size (single/bulk enqueue, workflow jobs, webhooks, schedule trigger).
- **Daily job quota is now enforced atomically on bulk and gRPC enqueue.** Only the
  single `POST /jobs` path used the row-locked `try_increment_daily_jobs`; bulk
  enqueue and gRPC `Enqueue` did a non-atomic check-then-increment, so concurrent
  requests could all pass the check and overshoot `jobs_per_day` (observed 30 for a
  cap of 25). Bulk now atomically reserves the whole batch before inserting and
  refunds the credits for any deduplicated/failed rows (net credit == jobs created);
  gRPC reserves-and-rejects like the single path. Workflow-create and the manual
  schedule trigger still credit the daily counter non-atomically — a bounded race
  gated by their own resource caps — and are tracked as a follow-up.
- **gRPC retry backoff was in minutes; now seconds, matching REST.** A gRPC
  `Fail`/`ProcessJobs` retry scheduled the next attempt at `2^retry_count` **minutes**
  (unbounded — 17 hours by the 10th retry), while REST uses `2^min(retry_count,6)`
  **seconds** capped at 60s. Both gRPC retry paths now use the same seconds-based,
  capped formula.
- **Schedule manual trigger returns the structured 429 on quota.** `POST
  /schedules/{id}/trigger` returned a plain-text 403 when the daily job quota was
  exhausted (and a 403 for oversized payloads), which SDKs misclassified as
  `UNKNOWN_ERROR`. It now returns the same `429 QUOTA_EXCEEDED` / `413
  PAYLOAD_TOO_LARGE` bodies as the direct enqueue path.
- **Expired-lease reclaim uses `<=` on the boundary.** Worker operations require
  `lease_expires_at > NOW()`; the reclaim sweep required `lease_expires_at < NOW()`,
  leaving a lease at exactly `NOW()` neither worker-usable nor reclaimable. Reclaim
  now uses `<=` so the expiry instant is owned by reclaim.

### Audit findings intentionally not changed

- The report's "gRPC complete skips `parent_job_id` child unblock" is a false
  positive: `parent_job_id` children get a `job_dependencies` edge at creation, and
  the `AFTER UPDATE OF status` trigger releases dependents on any completion (REST or
  gRPC). The "admin `null` ignored" and "bogus-key ~2.9s" observations were against a
  deployment predating v0.1.90 (both fixed there, pending redeploy).

## [0.1.90] - 2026-07-09

### Security

- **API-key authentication no longer bcrypt-scans every key sharing a prefix.**
  Every live key shares the `sp_live_` prefix (test keys share `sp_test_`), so the
  indexed lookup on `key_prefix` (first 8 chars) matched effectively all keys:
  authentication read up to 100 candidates and bcrypt-verified each — roughly 100
  bcrypt hashes (~7.7s observed) for a single bogus key, a cheap unauthenticated
  CPU-amplification vector, on both the REST and gRPC paths (gRPC has no auth cache,
  so it paid this on every request). A new indexed `api_keys.lookup_hash`
  (`sha256(plaintext)`, migration `20260709120000`) makes auth a single indexed row
  read plus exactly one bcrypt verify. It is not a security boundary — bcrypt still
  verifies the credential after the lookup. Keys created before this release have a
  NULL `lookup_hash` and are backfilled lazily on their next successful auth (until
  then they use a bounded fallback scan restricted to un-backfilled rows), so no
  offline migration of existing keys is required.

### Fixed

- **`custom_limits` and the Stripe id fields honor an explicit `null` again.** In
  the admin org-update endpoint, `custom_limits: null` (documented to reset to plan
  defaults) and `stripe_customer_id`/`stripe_subscription_id: null` (documented to
  clear) were silently ignored: serde collapsed a present `null` into the same
  `None` as an absent key. A `double_option` deserializer now distinguishes absent
  (keep) from `null` (reset/clear) from a value (set). Resetting `custom_limits` via
  an empty object `{}` continues to work.
- **Concurrent workflow retries no longer double-reset jobs.** `retry` read the
  workflow's `failed` status on the pool *before* opening its transaction, so two
  simultaneous retries could both pass the check and both reset the jobs. The
  workflow row is now selected `FOR UPDATE` inside the transaction, serializing
  concurrent retries — the second observes `running` and is rejected cleanly.

## [0.1.89] - 2026-07-09

### Fixed

- **Workflow retry now restores cascade-cancelled dependents.** When a job
  failed, the dependency sweep cancelled its downstream jobs and marked the
  workflow failed; retry only reset failed/deadletter jobs, so the cancelled
  dependents stayed terminal and the retried workflow could never complete. Retry
  now also resets `cancelled` jobs and recomputes `dependencies_met` from current
  dependency state, so downstream jobs run once their parent re-completes.
- **Cron day-of-month + day-of-week follow standard (Vixie) OR semantics.** When
  BOTH fields are restricted (neither is `*`), the schedule fires when EITHER
  matches (e.g. `0 0 0 15 * 1` = the 15th OR any Monday). Previously it required
  BOTH, firing far less often than users expect.
- **Schedule quota errors return the structured 429 body** with
  `code: "QUOTA_EXCEEDED"` like every other resource, instead of a plain-text
  message SDKs misclassified as `UNKNOWN_ERROR`.
- **gRPC rate limiting fails open on a transient Redis error** instead of
  rejecting a legitimate worker operation.

## [0.1.88] - 2026-07-09

### Fixed

- **Worker operations on an expired lease are now rejected.** `complete`, `fail`,
  and `heartbeat` (REST + gRPC + the ProcessJobs stream) previously succeeded even
  after the job's lease had expired, letting a zombie worker act on a job the
  scheduler had already reclaimed. Every worker-scoped update now requires
  `lease_expires_at > NOW()`; an expired lease returns HTTP 409 `LEASE_EXPIRED`
  (gRPC `FAILED_PRECONDITION`), distinct from 404 not-owned. The REST fail path is
  now a single atomic update (no TOCTOU), gRPC fail inserts a `dead_letter_queue`
  row to match REST, and the lease-recovery ticker runs every 10s (was 30s).

## [0.1.87] - 2026-07-08

### Fixed

- **Workflow retry no longer 500s (for real this time).** `POST /workflows/{id}/retry`
  ran `UPDATE workflows SET ... updated_at = NOW()`, but the `workflows` table has
  no `updated_at` column, so every retry hit `42703 undefined_column` -> 500. The
  v0.1.85 fix only corrected the *jobs*-reset query; this second statement was
  still broken. The column reference is removed, and the job-reset + workflow-status
  + count updates now run in ONE transaction so a failure can no longer leave jobs
  reset to `pending` while the workflow stays `failed`.
- **gRPC Enqueue honors the default job timeout.** proto3 `int32` is `0` when
  `timeout_seconds` is omitted, and the handler clamped that to a 1-second budget,
  so gRPC-enqueued jobs without an explicit timeout deadlettered after 1s. Unset
  now uses the 300s default, matching the REST handler.

## [0.1.86] - 2026-07-08

### Fixed

- **Plan resource caps are now enforced atomically.** Creating workflows, queues,
  workers, API keys, schedules, or webhooks checked the count and inserted in two
  separate statements, so concurrent requests could all pass the check and
  overshoot the per-plan cap. Each create now takes a per-`(org, resource)`
  advisory lock (`pg_advisory_xact_lock`) around the check + insert in one
  transaction, so concurrent creates serialize and the cap holds. (The daily-jobs
  cap was already atomic; `active_jobs` remains a best-effort pre-check since it is
  a transient, self-draining count and lives on the hot enqueue path.)
- **Quota/limit-exceeded now returns `429 Too Many Requests`** with a stable
  `code: "QUOTA_EXCEEDED"` (was `403` with no code, which surfaced as
  `UNKNOWN_ERROR` in the SDKs). The concurrency-race path already returned 429;
  both are now consistent.
- **`POST /queues/{name}/pause` accepts an empty body** (the `reason` field is
  optional; a mandatory JSON extractor previously returned `415`).
- **Queue-config `queue_name` in the body is now optional and ignored** (the
  `{name}` path parameter is authoritative), so clients aren't forced to send a
  redundant field.
- **`GET /admin/stats` reports real `uptime_seconds`** (tracked from process
  start) instead of a hardcoded `0`.
- Removed an unreachable priority clamp in the boost-priority handler (validation
  already caps priority at ±100).

## [0.1.85] - 2026-07-08

### Fixed

- **Workflow retry no longer 500s.** `POST /workflows/{id}/retry` referenced a
  non-existent `error_message` column (the column is `last_error`), so every
  retry of a failed workflow returned `500 DATABASE_ERROR`. The retry feature
  now works.
- **Soft-deleting an organization revokes its API keys.** Admin soft-delete
  marked the plan `deleted` but left every API key valid (auth only checks
  `api_keys.is_active`), so a "deleted" org could keep using the API. Soft-delete
  now deactivates the org's keys and evicts them from the auth cache.
- **Admin org list honors sort_by in every filter combination.** `sort_by` of
  slug/plan_tier/updated_at was silently ignored unless BOTH a plan filter and a
  search term were present; the ordering is now applied in all cases.
- **Manual schedule triggers are recorded in history.** `POST /schedules/{id}/trigger`
  bumped `run_count` but wrote no `schedule_runs` row, so triggered runs were
  missing from `/schedules/{id}/history`. They are now recorded.
- **Day-of-week `7` is accepted as Sunday** in cron expressions (both `0` and `7`
  mean Sunday, per standard cron).

## [0.1.84] - 2026-07-08

### Security

- **`RUST_ENV` now fails safe.** An unset or unrecognized `RUST_ENV` resolves to
  `production` (previously `development`), so a deploy that forgets the variable
  no longer silently disables JWT-secret strength validation and permissive-CORS
  protection. Local development opts in explicitly (`.env.example` sets
  `RUST_ENV=development`).
- **Metrics endpoint exposure is now documented accurately and logged.** The doc
  comment falsely claimed localhost-only access; corrected. When `METRICS_TOKEN`
  is unset the endpoint is unauthenticated (bound to `0.0.0.0` for in-cluster
  scraping), and a startup warning is now emitted so this is visible.

### Fixed

- **Daily job quota is now enforced atomically.** Enqueue checked the daily
  count and incremented it in two separate statements, so concurrent enqueues
  could all pass the check and overshoot the per-plan `jobs_per_day` cap. A new
  row-locked `try_increment_daily_jobs` function does the check-and-increment
  atomically; an over-cap enqueue is rejected and the just-created job removed
  (migration `20260708120000_atomic_daily_quota.sql`).
- **Cron expressions now accept the standard 5-field form** (`min hour dom mon
  dow`) in addition to the 6-field form with leading seconds. Seconds default to
  `0`. This matches the SDK README examples and common cron usage.


## [0.1.83] - 2026-07-08

Fixes from a deep multi-agent + live-traffic bug hunt. Multi-tenant isolation was
audited (static + live cross-org probing) and found clean. Two of these were
reproduced against production before fixing (cron `*/N`, `parent_job_id` gating).

### Fixed — correctness
- **Cron `*/N` fired on the wrong dates for day-of-month and month.** Step matching
  used `value % N == 0`, correct only for 0-based fields; day-of-month and month are
  1-based, so `*/2` day-of-month ran on even days (2,4,6…) instead of 1,3,5…, and
  `1 */3` month ran in Mar/Jun/Sep/Dec instead of Jan/Apr/Jul/Oct. Step now counts
  from each field's minimum.
- **Schedules double-fired during the DST fall-back hour.** `next_run` computed from a
  fire instant returned the repeated wall-clock time one hour later. The next run now
  must be strictly after the previous wall clock.
- **Sparse schedules could stall.** When only date fields didn't match, the search
  crawled a minute at a time and could exhaust the iteration cap (e.g. a once-a-year
  schedule); it now jumps a day at a time.
- **`parent_job_id` children ran immediately instead of after the parent.** The child
  was inserted with `dependencies_met = TRUE` (column default) and no dependency edge.
  Children are now gated and released by the parent's completion (and cancelled by the
  scheduler sweep if the parent fails).
- **Bulk enqueue aborted the whole batch** when two items shared an idempotency key
  (`ON CONFLICT DO UPDATE` affecting a row twice). Items are now deduplicated per key.
- **Retry backoff could overflow.** `2^retry_count` overflowed `i64` for high
  `max_retries`; the exponent is now clamped (the backoff was capped at 60s anyway).
- **`/auth/me` accepted a revoked API key's JWT.** It now checks `is_active`.

### Fixed — atomicity / transactions
- **Workflow creation is now a single transaction** (was per-statement on the pool);
  duplicate job keys are rejected instead of silently collapsing and orphaning jobs.
- **`add_dependencies` cycle check + insert** now run under a per-org advisory lock, so
  concurrent calls can't race to create a cycle that deadlocks jobs.
- **Admin `create_organization`** wraps the org + initial API key in one transaction
  (a failed key insert used to orphan the org and its now-taken slug).
- **Admin `update_organization`** re-reads after Stripe reconcile so the response
  reflects the persisted state instead of a stale pre-reconcile snapshot.
- **Stripe webhook idempotency is now atomic**: the event id is claimed up front via
  `INSERT … ON CONFLICT DO NOTHING` (releasing the claim on handler error so retries
  still reprocess), replacing a SELECT-then-INSERT that let concurrent duplicate
  deliveries both process.

### Fixed — security
- **Email login attempt cap is now race-free.** The per-code lockout was a
  read-then-check-then-increment; parallel verifications could all read a low count and
  all get a guess, amplifying brute force. It is now a single atomic conditional
  `UPDATE … WHERE attempts < MAX`.
- **Revoked API keys could linger in cache for up to an hour.** The cache TTL is reduced
  to 60s and `is_active` is re-checked immediately before caching (the DB read runs
  before the slow bcrypt verify, so a revoke racing that window is no longer papered
  over).
- **Signup: the one-time token was consumed before validation**, so a bad slug or a
  taken email/slug left the user unable to retry. The token is now consumed atomically
  only after validation passes.
- **Login codes could be shadowed by a pending signup token** for the same email; the
  login lookup now excludes `signup:` rows.
- **`retry_dlq` accepted an unbounded id array**; it's now capped at the same safe limit
  as the queue path.

### Fixed — reliability / resources
- **Cron scheduler could exhaust the DB connection pool.** It opened a transaction for
  every due schedule (up to 100) before processing any; with a 25-connection pool the
  scheduler stalled and starved the app. Each schedule is now locked, processed, and
  committed one at a time.
- **WebSocket connection counter leaked upward, permanently locking an org out.** The
  per-org counter was incremented before the upgrade (so aborted handshakes leaked it,
  and its TTL kept being bumped). The increment now happens after a successful upgrade,
  paired with the decrement on close; the pre-upgrade limit check is read-only.
- **WebSocket disconnects leaked a Redis pub/sub task + connection.** The subscriber
  task and ping timer are now aborted on disconnect.
- **Org settings update wiped the webhook token.** Replacing the settings object via
  `PUT` silently dropped `webhook_token`; it is now preserved unless explicitly changed.
- **An IPv6 `HOST` crashed gRPC startup** (bare `host:port` concatenation); the address
  is now built with `SocketAddr::new`.

### Known follow-ups (not in this release)
- Daily/active job quota and the API-key / slug-check counters are still
  check-then-act under high concurrency (bounded overshoot); an atomic
  increment-with-limit needs load testing before shipping.
- `RUST_ENV` still defaults to `development` (production sets it explicitly); the metrics
  endpoint binds all interfaces by design for in-cluster scraping (require `METRICS_TOKEN`).
- The Stripe ordering guard is `<=` on second-granularity timestamps (no finer signal
  available).

---

## [0.1.82] - 2026-07-08

### Fixed
- **Admin hard-delete of an organization no longer fails and no longer destroys
  data partially.** `DELETE /api/v1/admin/organizations/{id}?hard_delete=true`
  ran `DELETE FROM queue_configs`, but the table is `queue_config` (singular),
  so every hard delete errored with a database error — *after* it had already
  deleted the org's jobs, workflows, workers, schedules, api_keys and
  outgoing_webhooks on the connection pool (no transaction), leaving the
  organization row present but its data gone. The delete now runs in a single
  transaction (all-or-nothing) against the correct table, and API-key cache
  invalidation happens only after the transaction commits.
- **Schedule timezone validation now matches the scheduler.** `timezone` was
  validated against a hardcoded ~30-zone list, so valid IANA zones the scheduler
  fully supports (e.g. `Europe/Kyiv`) were rejected at create/update time.
  Validation now uses the full IANA database via chrono-tz — the same engine
  `next_run_after_in_timezone` uses — plus fixed offsets like `+05:30`.

---

## [0.1.81] - 2026-07-08

### Fixed
- **CORS default no longer blocks the dashboard.** The allow-list added in
  0.1.80 shipped a default origin list of
  `spooled.cloud, www.spooled.cloud, app.spooled.cloud`. Production serves the
  dashboard at `dashboard.spooled.cloud` (and `app.spooled.cloud` does not
  exist), and no `CORS_ALLOWED_ORIGINS` was set, so the default took effect and
  the browser blocked *every* dashboard→API call — admin login and normal user
  login included. The default now lists the real topology
  (`spooled.cloud, www.spooled.cloud, dashboard.spooled.cloud`), and
  `CORS_ALLOWED_ORIGINS` is set explicitly in the Kubernetes config so the
  running deployment no longer relies on the code default.

### Notes
- No behavioral change to the CORS mechanism itself (still a strict allow-list,
  never origin reflection) — only the set of permitted origins was corrected.

---

## [0.1.80] - 2026-07-07

### Security
- **CORS no longer reflects the request origin.** Production previously used
  `AllowOrigin::mirror_request()`, which echoes back any `Origin` and is
  equivalent to allowing every site. Origins now come from a configured
  allow-list (`CORS_ALLOWED_ORIGINS`, defaulting to the spooled.cloud apps).
- **Security-headers middleware is now actually mounted.** The middleware
  (`X-Frame-Options`, `X-Content-Type-Options`, `Referrer-Policy`,
  `Content-Security-Policy`, `Permissions-Policy`, and HSTS when
  `ENABLE_HSTS=true`) existed but was never added to the router, so the API
  served none of them.
- **Login rate limit no longer buckets on the key prefix.** Brute-force
  throttling hashed the 8-character key prefix, so every `sp_live_` key shared
  one bucket — five failed attempts locked out login for *all* customers (a
  trivial login DoS). It now buckets on a hash of the full submitted key.
- **Logout fails loud and revokes the refresh token.** When Redis is
  unavailable, logout now returns `503` instead of silently leaving the tokens
  usable, and it blacklists the refresh token supplied by the client so a
  session can no longer survive logout via `/auth/refresh`.
- **`/auth/me` rejects non-access tokens**, matching the token-type separation
  enforced elsewhere.

### Fixed
- **Stripe webhooks are now idempotent and order-safe.** Applied event ids are
  recorded in `processed_stripe_events` (only *after* successful processing, so
  a failed handler still returns 5xx and the retry is applied), and a
  `stripe_last_event_at` ordering guard rejects stale re-deliveries that could
  restore paid state on a cancelled organization.
- **Stripe "Basil" API compatibility.** `current_period_end` and the invoice
  subscription id moved to new locations in the 2025-03-31+ API; both are now
  read from the new and legacy locations, so subscription-period storage and
  invoice recovery stop being silent no-ops on current API versions.
- **Webhook signature verification accepts multiple `v1` signatures**, so
  webhooks are not lost during a Stripe signing-secret rotation.
- **Duplicate-checkout double billing is backstopped.** When a checkout
  supersedes an existing subscription, the previous subscription is cancelled
  instead of continuing to bill.
- **Unmapped Stripe price ids are now alerted** (error-logged) instead of
  silently keeping the current plan tier and hiding entitlement drift.
- **Schedules honor their timezone.** Cron expressions are evaluated against
  the schedule's timezone (IANA name or `±HH:MM` offset) with DST-correct
  next-run computation; they previously always ran in UTC.
- **Explicit cron catch-up policy.** Fires missed while the scheduler was down
  are skipped (the schedule runs once now and advances to the next future
  occurrence) and logged, rather than burst-enqueueing an unbounded backlog.
- **Lease-expiration dead-lettering writes the `dead_letter_queue` audit row**,
  matching the explicit-fail path — jobs that died via worker timeout were
  previously missing from the audit table.
- **Stranded workflow dependents are cancelled** and workflows left in a
  non-terminal state are terminated, so a failed or dead-lettered dependency no
  longer strands its dependents in `pending` forever.
- **DLQ audit rows are cleaned up on retry and purge** — there is no foreign
  key, so they previously accumulated indefinitely.

### Changed
- **Enterprise max payload aligned to the enforced 1 MiB ingest cap** (it was
  advertised as 5 MB, which the ingest validators made unenforceable).
- **`bcrypt` bumped to 0.19.2** for an auth-path panic fix.
- **Kubernetes manifests wire the Stripe billing env the backend actually
  reads** (`STRIPE_SECRET_KEY`, `STRIPE_BILLING_WEBHOOK_SECRET`, price ids).
- **Docs describe tenant isolation accurately** — row-level security is not a
  runtime control in this deployment.

---

## [0.1.79] - 2026-06-16

### Security
- **API key queue scoping now enforced on job operations** — A key created with
  a restricted `queues` list (e.g. `["billing"]`) could still enqueue, bulk
  enqueue, and claim jobs on *any* queue via both REST and gRPC; the scope was
  only checked on worker registration. Scope is now enforced consistently on
  `POST /jobs`, `POST /jobs/bulk`, `POST /jobs/claim`, and the gRPC
  `QueueService.Enqueue` / `QueueService.Dequeue` / `WorkerService.Register`
  RPCs. An empty list or a `*` entry still means "all queues". Violations
  return `403`/`PERMISSION_DENIED`. Added shared `can_access_queue` helpers to
  `ApiKeyContext` and `GrpcAuthContext`.

### Fixed
- **`POST /workers/{id}/heartbeat` returned a raw deserialize error** instead of
  the structured `VALIDATION_ERROR` shape used elsewhere. Switched the handler
  to the validated JSON extractor so missing/invalid fields (e.g. `current_jobs`)
  produce consistent `{"code":"VALIDATION_ERROR","details":[...]}` responses.

### Changed
- **`POST /api-keys` response now echoes the `queues` field** so callers can
  confirm the scope a freshly created key was granted (the value was always
  stored, just not returned).

---

## [0.1.78] - 2026-06-15

### Fixed
- **Signup slug validation always shows "Invalid"** — `/organizations/check-slug`
  and `/organizations/generate-slug` were placed behind the API-key auth
  middleware, so the signup page (which has no account yet) always received a
  401 response. The frontend interpreted the `{"code":"UNAUTHORIZED"}` JSON as
  `{valid: undefined}` and displayed "❌ Invalid" for every auto-generated slug.
  Both endpoints moved to public routes; they are rate-limited independently.

---

## [0.1.77] - 2026-05-06

### Fixed
- **SSE streams appear dead at connect** — All three SSE endpoints
  (`/api/v1/events`, `/api/v1/events/queues/{name}`, `/api/v1/events/jobs/{id}`)
  now emit an immediate `:connected` SSE comment so response headers + first
  body chunk flush before the first poll interval (was 10s/5s/2s). Fixes
  proxy-buffered connections (Cloudflare) and SDK reconnect loops caused by
  short read timeouts seeing no data.

---

## [0.1.76] - 2026-05-05

### Fixed
- **`DATABASE_ERROR 500` on `GET /queues/{name}/stats`** — `AVG(EXTRACT(EPOCH
  FROM interval))` returns `NUMERIC` in PostgreSQL 14+, but the sqlx decode
  expected `FLOAT8`. Queues with at least one completed job triggered the
  error. Added explicit `::FLOAT8` cast to align SQL type with `Option<f64>`.

---

## [0.1.75] - 2026-05-04

### Added
- **Implicit queue discovery** — `/queues` and `/queues/{name}` now surface
  queues that have jobs but no explicit `queue_config` row. Default
  configuration (max_retries, timeout) is synthesised from server settings.
- **Structured field-level validation errors** — JSON body parse failures now
  return `VALIDATION_ERROR` (HTTP 422) with `{field, message, code}` details
  instead of a generic "Invalid JSON" string. Codes: `missing_field`,
  `invalid_type`, `empty_body`, `invalid_json`.
- **JSON 401/403 responses** — Authentication errors now return
  `{"code":"UNAUTHORIZED","message":"..."}` (application/json) instead of
  plain text, consistent with the rest of the API.

### Fixed
- **Duplicate cron error prefix** — `CronSchedule::parse` no longer
  double-prefixes the error string ("Invalid cron expression: Invalid cron
  expression: …").

---

## [0.1.74] - 2026-05-03

### Fixed
- **gRPC worker drain state lost on heartbeat** — Heartbeat SQL now preserves
  `draining` status via a `CASE` expression instead of unconditionally
  overwriting it with `active`.

---

## [0.1.73] and earlier

See git log for history prior to this changelog.
