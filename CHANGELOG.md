# Changelog

All notable changes to `spooled-backend` are documented here.

Format follows [Keep a Changelog](https://keepachangelog.com/en/1.0.0/).  
Versions follow [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

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
