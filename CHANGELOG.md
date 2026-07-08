# Changelog

All notable changes to `spooled-backend` are documented here.

Format follows [Keep a Changelog](https://keepachangelog.com/en/1.0.0/).  
Versions follow [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

---

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
