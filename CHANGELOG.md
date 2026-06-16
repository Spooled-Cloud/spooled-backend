# Changelog

All notable changes to `spooled-backend` are documented here.

Format follows [Keep a Changelog](https://keepachangelog.com/en/1.0.0/).  
Versions follow [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

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
