# Spooled Cloud API Usage Guide

Complete guide to using the Spooled Cloud REST API.

Spooled is a Postgres-backed job queue: you enqueue jobs, workers lease and process them, and you get retries, DLQ, schedules, and workflows. Webhooks are supported as an optional ingestion mechanism.

## Table of Contents

1. [Plan Limits](#plan-limits)
2. [Authentication](#authentication)
3. [Jobs](#jobs)
4. [Dead Letter Queue](#dead-letter-queue)
5. [Queues](#queues)
6. [Workers](#workers)
7. [Schedules](#schedules)
8. [Workflows](#workflows)
9. [Webhooks](#webhooks)
10. [Organizations](#organizations)
11. [Billing](#billing)
12. [Real-time Events](#real-time-events)
13. [gRPC API](#grpc-api)
14. [Error Handling](#error-handling)

---

## Plan Limits

All API operations automatically enforce tier-based limits to ensure fair multi-tenancy.

### Available Tiers

| Tier | Active Jobs | Daily Jobs | Queues | Workers | Webhooks |
|------|-------------|------------|--------|---------|----------|
| **Free** | 10 | 1,000 | 5 | 3 | 2 |
| **Starter** | 100 | 100,000 | 25 | 25 | 10 |
| **Enterprise** | Unlimited | Unlimited | Unlimited | Unlimited | Unlimited |

### Operations with Limit Enforcement

Limits are **automatically checked** before:
- ✅ Creating jobs (`POST /jobs`, `POST /jobs/bulk`)
- ✅ gRPC job enqueue
- ✅ Creating workflows (counts all jobs)
- ✅ Triggering schedules
- ✅ Retrying DLQ jobs
- ✅ Registering workers
- ✅ Creating queues
- ✅ Creating webhooks

### Limit Exceeded Response

When a limit is exceeded, the API returns `429 Too Many Requests`:

```json
{
  "error": "limit_exceeded",
  "code": "QUOTA_EXCEEDED",
  "message": "active jobs limit reached (10/10). Upgrade to starter for higher limits.",
  "resource": "active_jobs",
  "current": 10,
  "limit": 10,
  "plan": "free",
  "upgrade_to": "starter"
}
```

### Checking Current Usage

```bash
GET /api/v1/organizations/usage
Authorization: Bearer <token>
```

Response:
```json
{
  "plan": {
    "tier": "free",
    "display_name": "Free"
  },
  "usage": {
    "jobs_today": { "current": 45, "limit": 1000, "percentage": 4.5 },
    "active_jobs": { "current": 8, "limit": 10, "percentage": 80.0 },
    "queues": { "current": 3, "limit": 5 },
    "workers": { "current": 2, "limit": 3 }
  }
}
```

### Performance

Latency and throughput depend on payload size, database capacity, cache state, and
network path. Use the checked-in `loadtest/` scenarios to benchmark your deployment.
For long-lived, high-throughput workers, prefer the gRPC streaming APIs.

---

## Authentication

### API Key Authentication

Include your API key in the `Authorization` header:

```bash
Authorization: Bearer sp_live_xxxxxxxxxxxxxxxxxxxx
```

The `Authorization: Bearer` header accepts **either** an API key or a JWT — the
API auto-detects a JWT by its `eyJ` prefix.

> **Credentials in the query string are only accepted on the realtime endpoints**
> (`/api/v1/ws`, `/api/v1/events`, `/api/v1/events/jobs/{id}`,
> `/api/v1/events/queues/{name}`), because browser `EventSource` and `WebSocket`
> cannot set request headers. Since **0.1.111**, every other endpoint requires the
> header and returns `401` for `?api_key=` / `?token=`.
>
> Query strings are recorded by reverse-proxy and CDN access logs, tracing spans,
> browser history and the `Referer` header. An API key does not expire by default,
> so one leaked log line is a lasting compromise.

> The `X-API-Key` header is **gRPC-only** and returns `401` on REST endpoints.
> Use `Authorization: Bearer` for the REST API.

API keys may be restricted to specific queues. Restricted keys can only grant a
subset of their own queues when creating or updating keys; omitting or emptying the
queue list would mean unrestricted access and is rejected. Queue scope is enforced
across jobs, workers, schedules, workflows, realtime subscriptions, and gRPC calls.
Active realtime and gRPC streams periodically revalidate revocation, expiry,
organization status, and current queue scope.

A `queues` entry is an exact queue name, `*` (every queue), or a prefix pattern
`prefix.*` that covers every queue whose name starts with `prefix.`: `orders.*`
covers `orders.eu` and `orders.eu.west`, but not `orders` itself or `ordersx`
(list `orders` as well to reach it). Names are case-sensitive. Any other use of
`*` (`orders*`, `*.eu`, `a.*.b`) is rejected with `400 VALIDATION_ERROR` when the
key is created or updated. A restricted key can grant a pattern only under one of
its own patterns that is at least as broad.

### JWT Authentication

For enhanced security, exchange your API key for JWT tokens:

#### Login

```bash
POST /api/v1/auth/login
Content-Type: application/json

{
  "api_key": "sp_live_xxxxxxxxxxxxxxxxxxxx"
}
```

Response:
```json
{
  "access_token": "eyJhbGciOiJIUzI1NiIs...",
  "refresh_token": "eyJhbGciOiJIUzI1NiIs...",
  "token_type": "Bearer",
  "expires_in": 86400,
  "refresh_expires_in": 2592000
}
```

#### Refresh Token

```bash
POST /api/v1/auth/refresh
Content-Type: application/json

{
  "refresh_token": "eyJhbGciOiJIUzI1NiIs..."
}
```

#### Get Current User

```bash
GET /api/v1/auth/me
Authorization: Bearer <access_token>
```

Response:
```json
{
  "organization_id": "org_xxxxx",
  "api_key_id": "key_xxxxx",
  "queues": ["emails", "notifications"],
  "issued_at": "2024-01-01T00:00:00Z",
  "expires_at": "2024-01-02T00:00:00Z"
}
```

---

## Jobs

### Create a Job

```bash
POST /api/v1/jobs
Content-Type: application/json
Authorization: Bearer <token>

{
  "queue_name": "emails",
  "payload": {
    "to": "user@example.com",
    "subject": "Welcome!",
    "template": "welcome_email"
  },
  "priority": 0,
  "max_retries": 3,
  "timeout_seconds": 300,
  "idempotency_key": "email-welcome-user123",
  "tags": ["transactional", "welcome"],
  "completion_webhook": "https://api.example.com/webhooks/job-complete"
}
```

**Parameters:**

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `queue_name` | string | Yes | Target queue |
| `payload` | object | Yes | Job data (JSON) |
| `priority` | integer | No | -100 to 100 (default: 0) |
| `max_retries` | integer | No | Maximum retry attempts |
| `timeout_seconds` | integer | No | Job timeout |
| `scheduled_at` | datetime | No | Future execution time |
| `idempotency_key` | string | No | Prevent duplicate jobs |
| `tags` | array | No | Searchable tags |
| `completion_webhook` | string | No | URL to notify on completion |
| `expires_at` | datetime | No | Job expiration time |

Response:
```json
{
  "id": "job_550e8400e29b41d4",
  "organization_id": "org_xxxxx",
  "queue_name": "emails",
  "status": "pending",
  "payload": {"type": "send_welcome_email", "user_id": 123},
  "priority": 0,
  "max_retries": 3,
  "retry_count": 0,
  "created_at": "2024-01-01T00:00:00Z"
}
```

### List Jobs

```bash
GET /api/v1/jobs?queue_name=emails&status=failed,deadletter&created_after=2026-10-11T00:00:00Z&limit=50
Authorization: Bearer <token>
```

**Query Parameters:**

| Parameter | Type | Description |
|-----------|------|-------------|
| `queue_name` | string | Filter by queue (letters, digits, `-`, `_`, `.`) |
| `status` | string | One status or a comma-separated list (`failed,deadletter`): pending, scheduled, processing, completed, failed, deadletter, cancelled (any case) |
| `q` | string | Search text, at most 255 bytes, case-insensitive: matches a job whose id **starts with** it, or whose `queue_name` or `payload.job_type` contains it. `%` and `_` match themselves |
| `job_type` | string | Exact `payload.job_type`, at most 255 bytes |
| `created_after` | RFC 3339 | Only jobs created at or after this instant |
| `created_before` | RFC 3339 | Only jobs created before this instant (must be later than `created_after`) |
| `tag` | string | One tag, any characters up to 1024 bytes |
| `limit` | integer | Max results (default: 50, at most 100) |
| `offset` | integer | Rows to skip (default: 0). There is no cursor |
| `order_by` | string | created_at (default), updated_at, scheduled_at, completed_at, priority, status, queue_name, retry_count |
| `order_dir` | string | desc (default), asc |

The filters combine with AND. An invalid `status` (any entry of the list), `queue_name`, `tag`,
`q`, `job_type`, `created_after`, `created_before`, `order_by` or `order_dir` is a `400`; it is not
ignored (before 0.1.121 it was, and the unfiltered list came back). An empty parameter is no
filter. Percent-encode a `+` offset in a timestamp as `%2B`. Rows that tie on the sort column are
ordered by job id, so `offset` pages do not repeat or skip jobs.

The body is a plain JSON array of job summaries. The `X-Has-More` response header says whether
another page follows (`true` or `false`), so a client can page until it reads `false` without
asking for one row too many; the dashboard can read it cross-origin.

Two limits on `q`: its job-type part compares the text with the organization's distinct job
types, not with every job, and looks at the first 1,000 of them (in sort order, each at most 255
bytes). A job whose type is beyond that is still found by `job_type`, by its id prefix or by its
queue. And the list runs under a statement timeout (5 seconds, `DATABASE_LIST_STATEMENT_TIMEOUT_MS`
on a self-hosted deployment): a list the database cannot answer in time is stopped and answers
`400` with code `QUERY_TIMEOUT`. Retrying it would time out again; narrow it with `queue_name`,
`status`, `job_type`, a created window or a longer `q`.

### Get Job

```bash
GET /api/v1/jobs/{job_id}
Authorization: Bearer <token>
```

### Job History

```bash
GET /api/v1/jobs/{job_id}/history?order_dir=desc&limit=100
Authorization: Bearer <token>
```

The lifecycle events recorded for the job, oldest first (`order_dir=desc` for newest first; the
alias `order` works too). `limit` is 1 to 500 (default 100), `offset` skips events, and the
`X-Has-More` header says whether more follow.

```json
[
  {
    "id": "6f0c1a52-...",
    "job_id": "job_550e8400e29b41d4",
    "event_type": "processing",
    "worker_id": "emails-worker-1",
    "details": {"worker_id": "emails-worker-1", "lease_id": "...", "organization_id": "org_xxxxx"},
    "created_at": "2026-10-12T09:00:01Z"
  }
]
```

`event_type` is `processing`, `completed`, `retry_scheduled` or `deadlettered` (`created` exists,
but only the internal queue manager's enqueue writes it and no REST or gRPC handler calls that).
Events exist only where the server records them: REST claim, complete and fail, and the scheduler's
dead-letter sweeps. **The history is sparse for jobs handled over gRPC, and a job has no `created`
event.** Another organization's job, an unknown id and a job in a queue outside the key's scope all
answer `404`.

### Cancel Job

```bash
DELETE /api/v1/jobs/{job_id}
Authorization: Bearer <token>
```

### Retry Job

```bash
POST /api/v1/jobs/{job_id}/retry
Authorization: Bearer <token>
```

Retrying a step of a workflow also re-opens the steps that were cancelled because it failed and puts
a `failed` workflow back to `running`. That takes a slot of the plan's workflow limit: `429
QUOTA_EXCEEDED` when none is free, `403 feature_disabled` on a plan without workflows.

### Boost Priority

```bash
PUT /api/v1/jobs/{job_id}/priority
Content-Type: application/json
Authorization: Bearer <token>

{
  "priority": 10
}
```

### Bulk Enqueue

```bash
POST /api/v1/jobs/bulk
Content-Type: application/json
Authorization: Bearer <token>

{
  "queue_name": "emails",
  "jobs": [
    {"payload": {"id": 1}},
    {"payload": {"id": 2}, "priority": 10},
    {"payload": {"id": 3}, "idempotency_key": "welcome-3"}
  ]
}
```

Up to 100 jobs, all for one `queue_name`. Optional batch defaults: `default_priority` (-100..100),
`default_max_retries` (0..100), `default_timeout_seconds` (1..86400).

Response:
```json
{
  "succeeded": [
    {"index": 0, "job_id": "job_xxx1", "created": true},
    {"index": 1, "job_id": "job_xxx2", "created": true},
    {"index": 2, "job_id": "job_xxx3", "created": true}
  ],
  "failed": [],
  "total": 3,
  "success_count": 3,
  "failure_count": 0
}
```

Items are validated one by one: an invalid item is listed in `failed` as `{"index": 1, "error": "..."}`
and the valid ones are still created. The request is a `400` (not a `200` with everything failed)
when no item is valid, a batch default is out of range, or the database refuses a value. A blank
`idempotency_key` is no key; an item whose key already exists is returned with `created: false`
and uses no quota.

### Batch Status

```bash
GET /api/v1/jobs/status?ids=job_xxx1,job_xxx2,job_xxx3
Authorization: Bearer <token>
```

### Job Statistics

```bash
GET /api/v1/jobs/stats
Authorization: Bearer <token>
```

Response:
```json
{
  "pending": 150,
  "processing": 25,
  "completed": 10000,
  "failed": 12,
  "deadletter": 3,
  "scheduled": 50
}
```

---

## Dead Letter Queue

Jobs that have exhausted all retries are moved to the Dead Letter Queue (DLQ).

### List DLQ Jobs

```bash
GET /api/v1/jobs/dlq?queue_name=emails&q=invoice&created_after=2026-10-01T00:00:00Z&limit=100
Authorization: Bearer <token>
```

Takes the filters, ordering, paging, `X-Has-More` header and statement timeout of
[List Jobs](#list-jobs), without `status` (the list is always `deadletter`).

Response, a plain array of job summaries:
```json
[
  {
    "id": "job_xxxxx",
    "queue_name": "emails",
    "status": "deadletter",
    "priority": 0,
    "attempt": 3,
    "max_retries": 3,
    "job_type": "send_welcome_email",
    "last_error": "Failed after 3 retries",
    "created_at": "2026-10-11T00:00:00Z",
    "scheduled_at": null,
    "started_at": "2026-10-11T00:00:05Z",
    "completed_at": null
  }
]
```

### Retry DLQ Jobs

Retry specific jobs by ID:

```bash
POST /api/v1/jobs/dlq/retry
Content-Type: application/json
Authorization: Bearer <token>

{
  "job_ids": ["job_xxxxx", "job_yyyyy"]
}
```

Retry all jobs in a queue:

```bash
POST /api/v1/jobs/dlq/retry
Content-Type: application/json
Authorization: Bearer <token>

{
  "queue_name": "emails",
  "limit": 100
}
```

**Note**: Retry operations respect the plan limit for active jobs. They do not count against the
daily job limit, because a retry creates no new job.

Response:
```json
{
  "retried_count": 25,
  "retried_jobs": ["job_xxxxx", "job_yyyyy", ...],
  "skipped_jobs": []
}
```

Retrying a dead-lettered step of a failed workflow restarts that workflow, which takes a slot of the
plan's workflow limit and re-opens the steps that were cancelled because of it. When no slot is
free those steps stay in the DLQ and are listed in `skipped_jobs`; everything else is still
retried. If only such steps are left, the request answers the limit's error (`429`, or `403` on a
plan without workflows).

### Purge DLQ

Permanently delete jobs from DLQ:

```bash
POST /api/v1/jobs/dlq/purge
Content-Type: application/json
Authorization: Bearer <token>

{
  "queue_name": "emails",
  "confirm": true,
  "older_than": "2024-01-01T00:00:00Z"
}
```

`older_than` selects jobs that were dead-lettered before that time, not jobs created before it.

**Warning**: This action is irreversible!

---

## Queues

### List Queues

```bash
GET /api/v1/queues
Authorization: Bearer <token>
```

### Get Queue

```bash
GET /api/v1/queues/{queue_name}
Authorization: Bearer <token>
```

### Queue Statistics

```bash
GET /api/v1/queues/{queue_name}/stats
Authorization: Bearer <token>
```

Response:
```json
{
  "queue_name": "emails",
  "pending_jobs": 150,
  "processing_jobs": 25,
  "completed_jobs_24h": 10000,
  "failed_jobs_24h": 12,
  "deadletter_jobs": 3,
  "avg_processing_time_ms": 250.0,
  "max_job_age_seconds": 30,
  "active_workers": 4
}
```

`failed_jobs_24h` is the number of jobs dead-lettered in the last 24 hours; `deadletter_jobs` is the
queue's current dead-letter count.

### Update Queue Config

```bash
PUT /api/v1/queues/{queue_name}/config
Content-Type: application/json
Authorization: Bearer <token>

{
  "max_retries": 5,
  "default_timeout": 600,
  "rate_limit": 100
}
```

`max_retries` must be 0-100, `default_timeout` 1-86400 seconds, and `rate_limit` 1-100000; a value
outside its range is a `400` (it is not clamped), and so is a queue name in the path that is not 1-100
letters, digits, `-`, `_` or `.` or that starts with `.` or `-`.

`rate_limit` is accepted, validated, stored and returned, but it is **not enforced
yet**: no claim path throttles the queue by it.

### Pause Queue

```bash
POST /api/v1/queues/{queue_name}/pause
Authorization: Bearer <token>
```

### Resume Queue

```bash
POST /api/v1/queues/{queue_name}/resume
Authorization: Bearer <token>
```

### Delete Queue

Removes the queue configuration. Fails with 409 if the queue still has pending or processing jobs.

To also delete every job in the queue (including pending, processing, completed, and dead-lettered):

```bash
DELETE /api/v1/queues/{queue_name}?delete_jobs=true
Authorization: Bearer <token>
```

---

## Workers

### Register Worker

```bash
POST /api/v1/workers/register
Content-Type: application/json
Authorization: Bearer <token>

{
  "worker_id": "emails-worker-1",
  "queue_name": "emails",
  "hostname": "worker-1.example.com",
  "max_concurrency": 5,
  "metadata": {
    "region": "us-east-1"
  }
}
```

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `queue_name` | string | Yes | Queue this worker processes (1-100 chars) |
| `hostname` | string | Yes | Host the worker runs on (1-255 chars) |
| `worker_id` | string | No | Stable worker id, 1-128 chars from `A-Z a-z 0-9 . _ -` |
| `worker_type` | string | No | Free-form label (max 50 chars) |
| `max_concurrency` | integer | No | 1-100 (default: 5) |
| `metadata` | object | No | Arbitrary JSON (max 64KB) |
| `version` | string | No | Worker build version (max 50 chars) |

`worker_id` is optional but worth setting. Send the same id across restarts and
registration upserts a single row, so the worker reclaims its identity. Omit it and the
server mints a UUID — the previous behaviour — but each restart then leaves the old row
occupying a slot in your plan's worker limit until the stale-worker reaper clears it
(about two minutes), which is enough for a crash-looping worker on a small plan to `429`
itself out of registering. Re-registering an id you already own is not charged against the
cap. An id owned by a different organization returns `409`, and so does, for a queue-scoped key, the
id of a worker of your organization that serves a queue outside the key's scope. Both answer with
the same text ("This worker_id cannot be used: register with a different worker_id, or omit it to
get a generated one"), which does not say which case it was. Workers that stay `offline` for more
than 7 days are removed.

### Worker Heartbeat

Send every 10 seconds to stay healthy:

```bash
POST /api/v1/workers/{worker_id}/heartbeat
Content-Type: application/json
Authorization: Bearer <token>

{
  "status": "healthy",
  "current_jobs": ["job_xxx1", "job_xxx2"]
}
```

### List Workers

```bash
GET /api/v1/workers
Authorization: Bearer <token>
```

### Get Worker

```bash
GET /api/v1/workers/{worker_id}
Authorization: Bearer <token>
```

Both return `jobs_processed` and `jobs_failed`, counted by the server since 0.1.122 (they start
at 0 and survive a re-registration of the same `worker_id`). `jobs_processed` is the jobs the
worker completed, `jobs_failed` the failures it reported (each retry scheduled and each
dead-letter); a job that fails twice and then completes adds 2 and 1. Only outcomes reported while
the worker held the job's lease count: a job released because its lease expired or the worker
went offline, a release on deregistration and a job the server dead-letters because it is too
large for a gRPC message do not. Success rate is `jobs_processed / (jobs_processed + jobs_failed)`.

### Deregister Worker

```bash
POST /api/v1/workers/{worker_id}/deregister
Authorization: Bearer <token>
```

---

## Schedules

### Create Schedule

```bash
POST /api/v1/schedules
Content-Type: application/json
Authorization: Bearer <token>

{
  "name": "Daily Report",
  "description": "Generate daily sales report",
  "cron_expression": "0 0 9 * * *",
  "timezone": "America/New_York",
  "queue_name": "reports",
  "payload_template": {
    "type": "daily_sales",
    "format": "pdf"
  },
  "priority": 5,
  "max_retries": 3
}
```

**Cron Expression Format:**

Both the standard 5-field form (`min hour dom mon dow`) and the 6-field form
with a leading seconds field are accepted. When 5 fields are given, the seconds
default to `0`.

```
┌───────────── second (0 - 59)   ← optional; defaults to 0 when omitted
│ ┌───────────── minute (0 - 59)
│ │ ┌───────────── hour (0 - 23)
│ │ │ ┌───────────── day of month (1 - 31)
│ │ │ │ ┌───────────── month (1 - 12)
│ │ │ │ │ ┌───────────── day of week (0 - 6, Sun=0)
│ │ │ │ │ │
* * * * * *
```

**Examples:**
- `* * * * *` - Every minute (5-field)
- `*/5 * * * *` - Every 5 minutes (5-field)
- `0 9 * * *` - Daily at 9 AM (5-field)
- `0 0 9 * * *` - Daily at 9 AM (6-field, seconds explicit)
- `0 0 * * 1` - Every Monday at midnight
- `0 0 1 * *` - First of every month

**Step values (`*/N`)** count from each field's minimum, so on day-of-month
`*/2` matches the 1st, 3rd, 5th, … and on month `*/3` matches January, April,
July, October.

**Day-of-month and day-of-week.** When both are restricted (a number, list or
range) a day matches if **either** matches: `0 0 15 * 1` runs on the 15th and on
every Monday. A field that starts with `*` (`*` or `*/N`) is unrestricted, so the
two fields then combine with **and**, as in Vixie cron: `0 0 */2 * 1` runs on odd
days that are also Mondays.

**Impossible dates** (`0 0 30 2 *`, `0 0 31 4 *`) and expressions with no upcoming
run are rejected with `400` when the schedule is created or updated.

**Timezone:** cron fields are evaluated in the schedule's `timezone` (IANA
name or `+HH:MM` offset, default `UTC`), DST-aware. Times skipped by a
spring-forward transition are not fired that day; times repeated by a
fall-back transition fire once, at the first occurrence.

**Catch-up policy:** fires missed while the platform is down are skipped, not
backfilled — the schedule runs once when service resumes and then advances to
the next future occurrence. Skipped fires are logged server-side. Design jobs
to be idempotent and, where gaps matter, derive work from state (e.g. "process
everything since last success") rather than from fire count.

### List Schedules

```bash
GET /api/v1/schedules?queue_name=reports&is_active=true&limit=50&offset=0
Authorization: Bearer <token>
```

Optional filters: `queue_name` (only schedules for that queue), `is_active` (`true`, `false`, `1` or
`0`; anything else is a `400`), `limit` (default 100, at most 500) and `offset`. Newest first.

### Get Schedule

```bash
GET /api/v1/schedules/{schedule_id}
Authorization: Bearer <token>
```

### Update Schedule

```bash
PUT /api/v1/schedules/{schedule_id}
Content-Type: application/json
Authorization: Bearer <token>

{
  "cron_expression": "0 0 10 * * *",
  "is_active": true
}
```

### Pause Schedule

```bash
POST /api/v1/schedules/{schedule_id}/pause
Authorization: Bearer <token>
```

### Resume Schedule

```bash
POST /api/v1/schedules/{schedule_id}/resume
Authorization: Bearer <token>
```

### Trigger Now

Manually trigger a schedule:

```bash
POST /api/v1/schedules/{schedule_id}/trigger
Authorization: Bearer <token>
```

Response:
```json
{
  "job_id": "job_xxxxx",
  "triggered_at": "2024-01-01T00:00:00Z"
}
```

### Schedule History

```bash
GET /api/v1/schedules/{schedule_id}/history?limit=50
Authorization: Bearer <token>
```

### Delete Schedule

```bash
DELETE /api/v1/schedules/{schedule_id}
Authorization: Bearer <token>
```

---

## Workflows

### Create Workflow

```bash
POST /api/v1/workflows
Content-Type: application/json
Authorization: Bearer <token>

{
  "name": "Order Processing",
  "jobs": [
    {
      "key": "validate",
      "queue_name": "orders",
      "payload": {"action": "validate"}
    },
    {
      "key": "process_payment",
      "queue_name": "payments",
      "payload": {"action": "charge"},
      "depends_on": ["validate"]
    },
    {
      "key": "ship",
      "queue_name": "shipping",
      "payload": {"action": "ship"},
      "depends_on": ["process_payment"]
    },
    {
      "key": "notify",
      "queue_name": "notifications",
      "payload": {"action": "notify"},
      "depends_on": ["ship"]
    }
  ]
}
```

`depends_on` lists the `key`s of other jobs in the workflow. Listing the same key twice, an unknown
key or a cycle is a `400`.

### List Workflows

```bash
GET /api/v1/workflows
Authorization: Bearer <token>
```

### Get Workflow

```bash
GET /api/v1/workflows/{workflow_id}
Authorization: Bearer <token>
```

Each job in `jobs` has a `job_type` (the `job_type` of its payload, `"job"` when the payload has
none) and, for a failed or dead-lettered job, a `failed_at`. A workflow turns `running`, with
`started_at` set, within about 10 seconds of its first job being claimed. When a step failed the
workflow ends `failed`; when a step was cancelled and none failed it ends `cancelled`; it is
`completed` only when every step completed.

### Cancel Workflow

```bash
POST /api/v1/workflows/{workflow_id}/cancel
Authorization: Bearer <token>
```

Only a `pending` or `running` workflow can be cancelled. A `completed`, `failed` or `cancelled`
workflow answers `409` and keeps its outcome.

### Retry Failed Workflow

Retry all failed jobs in a workflow. This resets failed/deadletter jobs back to pending,
clears retry counts, and resumes the workflow. Only workflows with status `failed` can be retried.
A running workflow counts toward the plan's `max_workflows` again, so the retry answers
`429 QUOTA_EXCEEDED` when the organization is at its limit, and `403 feature_disabled` on a plan
without workflows.

```bash
POST /api/v1/workflows/{workflow_id}/retry
Authorization: Bearer <token>
```

Response:
```json
{
  "id": "wf_abc123",
  "name": "Data Processing Pipeline",
  "status": "running",
  "total_jobs": 5,
  "completed_jobs": 3,
  "failed_jobs": 0
}
```

### Job Dependencies

Get a job's dependencies:

```bash
GET /api/v1/jobs/{job_id}/dependencies
Authorization: Bearer <token>
```

Add dependencies to a job:

```bash
POST /api/v1/jobs/{job_id}/dependencies
Content-Type: application/json
Authorization: Bearer <token>

{
  "depends_on": ["job_xxx1", "job_xxx2"],
  "dependency_mode": "all"
}
```

A dependency on the job itself, or one that would close a cycle, is a `400`. `dependency_mode` is
optional; omitted, the job keeps its current mode. When every listed dependency already existed the
job is left unchanged and `dependencies_added` is 0.

---

## Webhooks

Spooled supports custom webhooks that allow you to receive events from any source
and automatically create jobs in your queues.

### Custom Webhook Overview

Each organization gets a unique webhook URL based on their organization ID:

```
POST https://api.spooled.cloud/api/v1/webhooks/{org_id}/custom
```

Where `{org_id}` is your organization's UUID (e.g., `org_a1b2c3d4e5f6`).

### Step 1: Get Your Webhook Token

A webhook token is automatically generated when your organization is created.
You can retrieve it via the API or from the dashboard under Organization Settings.

```bash
# Get your current webhook token and URL
curl https://api.spooled.cloud/api/v1/organizations/webhook-token \
  -H "Authorization: Bearer sp_live_your_api_key"
```

Response:
```json
{
  "webhook_token": "whk_abc123...",
  "webhook_url": "https://api.spooled.cloud/api/v1/webhooks/{org_id}/custom"
}
```

### Managing Your Webhook Token

```bash
# Regenerate the token (invalidates the old one)
curl -X POST https://api.spooled.cloud/api/v1/organizations/webhook-token/regenerate \
  -H "Authorization: Bearer sp_live_your_api_key"
```

**Important:** 
- Store your webhook token securely - you'll configure it in your external service
- Regenerating the token invalidates the old one; update all external services
- The `X-Webhook-Token` header is required for incoming webhooks

### Step 2: Send Webhooks to Spooled

Configure your external service to send webhooks to your organization's endpoint:

```bash
POST /api/v1/webhooks/{org_id}/custom
X-Webhook-Token: your-secret-webhook-token-min-16-chars
Content-Type: application/json

{
  "queue_name": "my-events",
  "event_type": "order.created",
  "payload": {
    "order_id": 12345,
    "customer": "John Doe",
    "items": ["item1", "item2"],
    "total": 99.99
  },
  "priority": 0,
  "idempotency_key": "order-12345"
}
```

**Request Fields:**

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `queue_name` | string | Yes | Target queue for the job (1-255 chars) |
| `payload` | object | Yes | Job data (max 1MB) |
| `event_type` | string | No | Event type label (for logging/tracking) |
| `priority` | integer | No | -100 to 100 (default: 0) |
| `idempotency_key` | string | No | Prevent duplicate jobs (max 255 chars) |

**Response (200 OK):**
```json
{
  "job_id": "job_550e8400e29b41d4",
  "queue_name": "my-events",
  "status": "pending"
}
```

### Example: Receiving Webhooks from Different Services

#### From a Payment Provider

```bash
# Configure your payment provider to POST to:
# https://api.spooled.cloud/api/v1/webhooks/org_abc123/custom

curl -X POST https://api.spooled.cloud/api/v1/webhooks/org_abc123/custom \
  -H "X-Webhook-Token: your-secret-token" \
  -H "Content-Type: application/json" \
  -d '{
    "queue_name": "payments",
    "event_type": "payment.completed",
    "payload": {
      "payment_id": "pay_xyz789",
      "amount": 5000,
      "currency": "usd",
      "customer_email": "user@example.com"
    },
    "idempotency_key": "payment-pay_xyz789"
  }'
```

#### From a Form Submission

```bash
curl -X POST https://api.spooled.cloud/api/v1/webhooks/org_abc123/custom \
  -H "X-Webhook-Token: your-secret-token" \
  -H "Content-Type: application/json" \
  -d '{
    "queue_name": "form-submissions",
    "event_type": "contact.form",
    "payload": {
      "name": "Jane Doe",
      "email": "jane@example.com",
      "message": "I have a question about..."
    }
  }'
```

#### From a CI/CD Pipeline

```bash
curl -X POST https://api.spooled.cloud/api/v1/webhooks/org_abc123/custom \
  -H "X-Webhook-Token: $SPOOLED_WEBHOOK_TOKEN" \
  -H "Content-Type: application/json" \
  -d '{
    "queue_name": "deployments",
    "event_type": "deploy.triggered",
    "priority": 10,
    "payload": {
      "repo": "my-app",
      "branch": "main",
      "commit": "abc123def",
      "environment": "production"
    },
    "idempotency_key": "deploy-abc123def-production"
  }'
```

### Error Responses

| Status | Description |
|--------|-------------|
| 200 | Job created successfully |
| 400 | Invalid request (missing queue_name, invalid JSON, etc.) |
| 401 | Invalid or missing `X-Webhook-Token` |
| 404 | Organization not found |

### Inbound Webhook Log

```bash
GET /api/v1/webhooks/deliveries?status=duplicate&since=2026-10-12T00:00:00Z&limit=50
Authorization: Bearer <token>
```

The requests `POST /api/v1/webhooks/{org_id}/custom` accepted for your organization, newest first.
Use it to answer "did the provider call us, and what did we do with it". A request with a wrong
token, or one refused by a limit, is not logged.

| Parameter | Description |
|-----------|-------------|
| `status` | `created` (the request made a new job) or `duplicate` (its idempotency key matched an existing job) |
| `since` | RFC 3339 instant; only requests received at or after it |
| `queue_name` | Only requests whose job is in this queue (exact match, at most 255 characters; alias `queue`) |
| `limit`, `offset` | Page size (default 50, 1 to 100) and entries to skip |

```json
[
  {
    "id": "d3a1c0f2-...",
    "provider": "custom",
    "event_type": "order.created",
    "matched_queue": "my-events",
    "job_id": "job_550e8400e29b41d4",
    "status": "created",
    "created_at": "2026-10-12T09:15:00Z"
  }
]
```

`event_type` is `custom` when the request sent none. `matched_queue` is the queue of the job the
request resolved to, which is not the request's `queue_name` when an idempotent replay matched a job
in another queue. `job_id` and `status` are `null` for entries recorded before 0.1.122. The log
never returns the payload, a signature or the webhook token. It needs an unrestricted API key: a
queue-scoped key gets `403 ACCESS_DENIED`. An unknown `status`, a `since` that is not RFC 3339 or a
`queue_name` over 255 characters is a `400`. Entries are removed by the same retention sweep as the
other delivery history. As on `GET /jobs`, the body is a bare array and the `X-Has-More` response
header (`true` or `false`, since 0.1.124) says whether another page follows.

### Security Best Practices

1. **Always configure a webhook token** for production use
2. **Use HTTPS** - webhooks are rejected over HTTP in production
3. **Use idempotency keys** - external services may retry failed deliveries
4. **Validate on your worker** - the payload comes from external sources

### Billing Webhook (Platform Admin Only)

The `/api/v1/billing/webhook` endpoint handles Stripe subscription events for
the platform billing system. This is configured in Stripe's webhook settings
and secured with `STRIPE_BILLING_WEBHOOK_SECRET`. The endpoint must be subscribed to
`checkout.session.completed`, `checkout.session.async_payment_succeeded`,
`checkout.session.async_payment_failed`, `customer.subscription.created`,
`customer.subscription.updated`, `customer.subscription.deleted`, `invoice.paid` and
`invoice.payment_failed`. An event for a customer that is not linked to an organization yet
(they usually arrive before the checkout does) is answered `409` and Stripe retries it; a
processing failure is `500` and is retried as well.

---

## Organizations

### Change the Billing Email

`PUT /api/v1/organizations/{id}` with a new `billing_email` on an organization whose billing email
is already confirmed does not replace it yet: the new address is returned as
`pending_billing_email`, the confirmed address keeps working for email login, and the confirmation
code is sent to the new one with `POST /api/v1/organizations/billing-email/verification`.
`POST /api/v1/organizations/billing-email/verify` with that code makes it the billing email.
Sending the current address again withdraws a pending change. An organization without a confirmed
billing email is changed directly (the new address stays unconfirmed until verified).

### Get Organization Usage

Get current resource usage and plan limits:

```bash
GET /api/v1/organizations/usage
Authorization: Bearer <token>
```

Response:
```json
{
  "plan": {
    "tier": "starter",
    "display_name": "Starter"
  },
  "usage": {
    "jobs_today": {
      "current": 1250,
      "limit": 100000,
      "percentage": 1.25
    },
    "active_jobs": {
      "current": 45,
      "limit": 100,
      "percentage": 45.0
    },
    "queues": { "current": 8, "limit": 25 },
    "workers": { "current": 12, "limit": 25 },
    "webhooks": { "current": 3, "limit": 10 },
    "schedules": { "current": 5, "limit": 25 }
  }
}
```

### Check Slug Availability

Check if an organization slug is available:

```bash
GET /api/v1/organizations/check-slug?slug=my-company
```

Response:
```json
{
  "available": true,
  "valid": true,
  "suggestion": null
}
```

If not available:
```json
{
  "available": false,
  "valid": true,
  "suggestion": "my-company-4f9a1c"
}
```

The slug is trimmed and lower-cased first. It must be 3-50 characters (what signup accepts) of
lowercase letters, digits and hyphens, not at either end; otherwise `valid` is `false` and `error`
says why, for example `"Validation failed: Slug must be 3-50 characters"`. A reserved name such as `admin` is `valid`
but not `available`, with `"error": "This slug is reserved"` and a suggestion.

### Generate Unique Slug

Generate a unique slug from an organization name:

```bash
POST /api/v1/organizations/generate-slug
Content-Type: application/json

{
  "name": "My Company Name"
}
```

Response:
```json
{
  "slug": "my-company-name"
}
```

If the slug is taken, is a reserved name (`Admin` gives `admin`) or is shorter than 3 characters,
six random characters are appended:
```json
{
  "slug": "my-company-name-4f9a1c"
}
```

The slug is at most 50 characters. A name with no ASCII letter or digit gives `org-` and the random
characters (`org-4f9a1c`); a name that starts with a digit gets `org-` in front (`3M` gives `org-3m`).

---

## Billing

### Get Billing Status

Needs an unrestricted API key (or a JWT from one): a queue-scoped key gets `403 ACCESS_DENIED`.

```bash
GET /api/v1/billing/status
Authorization: Bearer <token>
```

Response:
```json
{
  "plan_tier": "starter",
  "has_stripe_customer": true,
  "stripe_customer_id": "cus_xxxxx",
  "subscription_status": "active",
  "current_period_end": "2024-02-01T00:00:00Z",
  "cancel_at_period_end": false
}
```

### Start a Checkout

Create a Stripe Checkout session that buys a plan for your organization. Needs an unrestricted
API key (or a JWT from one).

```bash
POST /api/v1/billing/checkout
Content-Type: application/json
Authorization: Bearer <token>

{
  "plan": "pro"
}
```

`plan` is `starter` or `pro`. Response:
```json
{
  "url": "https://checkout.stripe.com/c/pay/cs_live_xxxxx",
  "session_id": "cs_live_xxxxx"
}
```

Redirect the user to `url`. Checkout returns them to
`<FRONTEND_URL>/account/billing?checkout=success` (or `=cancel`), where `FRONTEND_URL` defaults to
`https://spooled.cloud`. The session is bound to the authenticated organization; if the
organization already has a Stripe customer it is reused, otherwise Checkout is prefilled with the
organization's verified billing email.

| Status | When |
|--------|------|
| 400 `VALIDATION_ERROR` | `plan` is missing, not `starter` or `pro`, or the body is not JSON |
| 403 `ACCESS_DENIED` | The API key is restricted to queues |
| 409 `CONFLICT` | The organization already has that plan with an active or trialing subscription; use the billing portal |
| 503 `SERVICE_UNAVAILABLE` | Billing or that plan's price is not configured, or Stripe is unavailable |

Buying a different plan than the current one replaces the current subscription once the payment
succeeds (the old subscription is cancelled at once, without proration); use the billing portal to
change plans with proration.

### Create Customer Portal Session

Create a Stripe Customer Portal session for managing subscriptions:

```bash
POST /api/v1/billing/portal
Content-Type: application/json
Authorization: Bearer <token>

{
  "return_url": "https://yourapp.com/settings/billing"
}
```

Response:
```json
{
  "url": "https://billing.stripe.com/session/xxxxx"
}
```

Redirect the user to this URL to manage their subscription, payment methods, and invoices.

---

## Real-time Events

### WebSocket

Connect to receive real-time updates. WebSocket auth requires a JWT in the
`?token=` query parameter (headers aren't available on the handshake). Get a JWT
from `POST /api/v1/auth/login` using your API key, then pass it as `token`. API
keys are **not** accepted here. Requested subscriptions are intersected with the
JWT's current queue scope, and authorization is revalidated while connected:

```javascript
// token is a JWT from POST /api/v1/auth/login (starts with "eyJ..."), not an API key
const ws = new WebSocket('wss://api.example.com/api/v1/ws?token=eyJ...&queues=emails,notifications');

ws.onopen = () => {
  console.log('Connected');
};

ws.onmessage = (event) => {
  const data = JSON.parse(event.data);
  switch (data.type) {
    case 'job.created':
      console.log('New job:', data.payload.id);
      break;
    case 'job.completed':
      console.log('Job completed:', data.payload.id);
      break;
    case 'job.failed':
      console.log('Job failed:', data.payload.id, data.payload.error);
      break;
  }
};
```

**Lifetime and close codes.** A realtime connection lasts as long as the token it was opened
with: the server re-checks the session (logout), the key and its queue scope at most every 2
seconds, and closes the socket when the token expires or the session is revoked. Close codes:

| Code | Meaning | What to do |
|------|---------|------------|
| `1008` | The access token expired, or the token, session or key was revoked (an `Error` event comes first) | Get a new token, then reconnect |
| `1000` | Maximum age of 1 hour reached | Reconnect |
| `1011` | The server lost its event channel (after an `Error` event), or the authorization lookup failed | Reconnect with backoff |

An organization can have 100 open WebSockets; the next connection answers `429`.

### Server-Sent Events (SSE)

#### All Events

```bash
curl -N http://localhost:8080/api/v1/events \
  -H "Authorization: Bearer <token>"
```

#### Job Events

```bash
curl -N http://localhost:8080/api/v1/events/jobs/{job_id} \
  -H "Authorization: Bearer <token>"
```

#### Queue Events

```bash
curl -N http://localhost:8080/api/v1/events/queues/{queue_name} \
  -H "Authorization: Bearer <token>"
```

The SSE streams end when the access token expires or the session is logged out, and after 30
minutes; reconnect to continue. An organization can have 100 open streams of each kind (`/events`,
`/events/jobs/{id}`, `/events/queues/{name}`); the next one answers `429`. A job stream also ends
when the job reaches a final status.

**Event Format:**
```
event: job.completed
data: {"id":"job_xxx","status":"completed","result":{...}}

event: queue.stats
data: {"queue_name":"emails","pending":150,"processing":25}
```

---

## gRPC API

Spooled provides a **real gRPC API** for high-performance worker communication. The gRPC API uses HTTP/2 + Protobuf and supports streaming.

### Connection

```bash
# Local / self-hosted (default)
localhost:50051

# Spooled Cloud (TLS over 443)
grpc.spooled.cloud:443
```

### Authentication

Include your API key in gRPC metadata:

```bash
# Using x-api-key header
grpcurl -H "x-api-key: sp_live_xxxx" ...

# Using authorization header
grpcurl -H "authorization: Bearer sp_live_xxxx" ...
```

### QueueService RPCs

#### Enqueue

```bash
grpcurl -plaintext \
  -H "x-api-key: sp_live_xxxx" \
  -d '{
    "queue_name": "emails",
    "payload": {"to": "user@example.com", "subject": "Hello"},
    "priority": 0,
    "max_retries": 3,
    "timeout_seconds": 300
  }' \
  localhost:50051 spooled.v1.QueueService/Enqueue
```

If `timeout_seconds` is omitted (or `0`), it defaults to `300` seconds.

Response:
```json
{
  "job_id": "job_550e8400e29b41d4",
  "created": true
}
```

#### Dequeue (Batch)

```bash
grpcurl -plaintext \
  -H "x-api-key: sp_live_xxxx" \
  -d '{
    "queue_name": "emails",
    "worker_id": "worker-1",
    "lease_duration_secs": 300,
    "batch_size": 10
  }' \
  localhost:50051 spooled.v1.QueueService/Dequeue
```

#### Complete

```bash
grpcurl -plaintext \
  -H "x-api-key: sp_live_xxxx" \
  -d '{
    "job_id": "job_550e8400e29b41d4",
    "worker_id": "worker-1",
    "result": {"email_sent": true}
  }' \
  localhost:50051 spooled.v1.QueueService/Complete
```

#### Fail

```bash
grpcurl -plaintext \
  -H "x-api-key: sp_live_xxxx" \
  -d '{
    "job_id": "job_550e8400e29b41d4",
    "worker_id": "worker-1",
    "error": "SMTP connection failed",
    "retry": true
  }' \
  localhost:50051 spooled.v1.QueueService/Fail
```

#### RenewLease

```bash
grpcurl -plaintext \
  -H "x-api-key: sp_live_xxxx" \
  -d '{
    "job_id": "job_550e8400e29b41d4",
    "worker_id": "worker-1",
    "extension_secs": 300
  }' \
  localhost:50051 spooled.v1.QueueService/RenewLease
```

#### GetJob

```bash
grpcurl -plaintext \
  -H "x-api-key: sp_live_xxxx" \
  -d '{"job_id": "job_550e8400e29b41d4"}' \
  localhost:50051 spooled.v1.QueueService/GetJob
```

#### GetQueueStats

```bash
grpcurl -plaintext \
  -H "x-api-key: sp_live_xxxx" \
  -d '{"queue_name": "emails"}' \
  localhost:50051 spooled.v1.QueueService/GetQueueStats
```

### Streaming RPCs

#### StreamJobs (Server Streaming)

Continuously receive jobs as they become available:

```bash
grpcurl -plaintext \
  -H "x-api-key: sp_live_xxxx" \
  -d '{
    "queue_name": "emails",
    "worker_id": "worker-1",
    "lease_duration_secs": 300
  }' \
  localhost:50051 spooled.v1.QueueService/StreamJobs
```

Jobs are streamed as they're claimed:
```json
{"id": "job_1", "queue_name": "emails", "payload": {...}}
{"id": "job_2", "queue_name": "emails", "payload": {...}}
...
```

#### ProcessJobs (Bidirectional Streaming)

Interactive job processing - send commands, receive responses:

```bash
# Interactive mode with grpcurl
grpcurl -plaintext \
  -H "x-api-key: sp_live_xxxx" \
  localhost:50051 spooled.v1.QueueService/ProcessJobs
```

Send requests:
```json
{"dequeue": {"queue_name": "emails", "worker_id": "w1", "batch_size": 1}}
{"complete": {"job_id": "job_1", "worker_id": "w1"}}
{"fail": {"job_id": "job_2", "worker_id": "w1", "error": "failed"}}
{"renew_lease": {"job_id": "job_3", "worker_id": "w1", "extension_secs": 60}}
```

### WorkerService RPCs

#### Register

```bash
grpcurl -plaintext \
  -H "x-api-key: sp_live_xxxx" \
  -d '{
    "queue_name": "emails",
    "hostname": "worker-host-1",
    "worker_type": "email-sender",
    "max_concurrency": 10,
    "version": "1.0.0"
  }' \
  localhost:50051 spooled.v1.WorkerService/Register
```

Response:
```json
{
  "worker_id": "worker_abc123",
  "lease_duration_secs": 300,
  "heartbeat_interval_secs": 10
}
```

#### Heartbeat

```bash
grpcurl -plaintext \
  -H "x-api-key: sp_live_xxxx" \
  -d '{
    "worker_id": "worker_abc123",
    "current_jobs": 5,
    "status": "healthy"
  }' \
  localhost:50051 spooled.v1.WorkerService/Heartbeat
```

#### Deregister

```bash
grpcurl -plaintext \
  -H "x-api-key: sp_live_xxxx" \
  -d '{"worker_id": "worker_abc123"}' \
  localhost:50051 spooled.v1.WorkerService/Deregister
```

### Health Check

```bash
grpcurl -plaintext localhost:50051 grpc.health.v1.Health/Check
```

### Service Discovery (Reflection)

```bash
# List all services
grpcurl -plaintext localhost:50051 list

# Describe a service
grpcurl -plaintext localhost:50051 describe spooled.v1.QueueService

# Describe a message
grpcurl -plaintext localhost:50051 describe spooled.v1.EnqueueRequest
```

### gRPC Error Codes

| gRPC Code | HTTP Equiv | Description |
|-----------|------------|-------------|
| `UNAUTHENTICATED` | 401 | Invalid or missing API key |
| `PERMISSION_DENIED` | 403 | Not authorized for this resource |
| `NOT_FOUND` | 404 | Job/queue/worker not found |
| `INVALID_ARGUMENT` | 400 | Validation error |
| `RESOURCE_EXHAUSTED` | 429 | Rate limited or quota exceeded |
| `INTERNAL` | 500 | Server error |

---

## Error Handling

### Error Response Format

```json
{
  "code": "validation_error",
  "message": "Invalid request body",
  "details": {
    "queue_name": "is required",
    "payload": "must be an object"
  }
}
```

### Error Codes

| HTTP Status | Code | Description |
|-------------|------|-------------|
| 400 | `bad_request` | Invalid request format |
| 400 | `validation_error` | Request validation failed |
| 401 | `unauthorized` | Invalid or missing credentials |
| 403 | `forbidden` | Insufficient permissions |
| 404 | `not_found` | Resource not found |
| 409 | `conflict` | Resource conflict (e.g., duplicate) |
| 400 | `QUERY_TIMEOUT` | A job list ran into its statement timeout; narrow the filters (retrying the same request times out again) |
| 429 | `RATE_LIMIT_EXCEEDED` | Too many requests (also `QUOTA_EXCEEDED` for plan caps) |
| 500 | `internal_error` | Server error |

### Rate Limiting

A rate-limited request answers `429` with a `Retry-After` header (the wait in whole seconds) and
a JSON body with the same information:

```
Retry-After: 60
X-RateLimit-Limit: 100
X-RateLimit-Remaining: 0
X-RateLimit-Reset: 1704067200
X-RateLimit-Scope: api_key

{
  "error": "Rate limit exceeded",
  "code": "RATE_LIMIT_EXCEEDED",
  "retry_after_secs": 60,
  "limit": 100,
  "remaining": 0,
  "reset_at": 1704067200
}
```

Authenticated responses, allowed or refused, carry the state of the limit that answered:

| Header | Meaning |
|--------|---------|
| `X-RateLimit-Limit` | The plan burst (the bucket size: Free 10, Starter 50, Pro 200, Enterprise 1000, or the organization's override), or the key's own per-minute limit |
| `X-RateLimit-Remaining` | Requests that may still be made right now, after this one |
| `X-RateLimit-Reset` | The **Unix time** in seconds at which `X-RateLimit-Remaining` next goes up (the next whole token of the bucket, or the start of the key's next minute); the current time while the bucket is full. On a `429` it equals the body's `reset_at` |
| `X-RateLimit-Scope` | `organization` or `api_key`: which limit the other three describe |

`X-RateLimit-Reset` is an absolute time, not a wait; `Retry-After` is the wait. When an API key has its
own `rate_limit` as well, the numbers are those of whichever limit runs out first (fewer requests
remaining, and on a tie the later reset).

```
X-RateLimit-Limit: 200
X-RateLimit-Remaining: 187
X-RateLimit-Reset: 1704067201
X-RateLimit-Scope: organization
```

Unauthenticated routes, admin routes, deployments without Redis and requests whose limit check could
not run (Redis failing while `RATE_LIMIT_FAIL_CLOSED` is off) carry no `X-RateLimit-*` headers, so
treat them as optional; a refusal there still has `Retry-After`. Browsers can read these headers,
`Retry-After`, `X-Request-Id` and `X-Has-More` on cross-origin calls (`Access-Control-Expose-Headers`).

The main limit is a token bucket **per organization** (requests per second and burst, by plan; see
[Rate Limits](#rate-limits)). An API key can also carry its own `rate_limit` in **requests per
minute**; it is checked after the organization limit, so it can only lower what that key may do,
and it is counted per key. Unauthenticated routes are limited per client IP.

---

## Best Practices

1. **Use Idempotency Keys** - Prevent duplicate jobs on retries
2. **Set Appropriate Timeouts** - Match job duration expectations
3. **Use Tags** - Make jobs searchable and filterable
4. **Monitor Queue Age** - Alert on `spooled_job_max_age_seconds`
5. **Handle Webhooks** - Use completion webhooks for async flows
6. **Implement Circuit Breakers** - Handle downstream failures gracefully

## Usage & Plan Limits

### Get Current Usage

```bash
GET /api/v1/organizations/usage
Authorization: Bearer <token>
```

Response:
```json
{
  "plan": "free",
  "limits": {
    "tier": "free",
    "display_name": "Free",
    "max_jobs_per_day": 1000,
    "max_active_jobs": 10,
    "max_queues": 2,
    "max_workers": 1,
    "max_api_keys": 2,
    "max_schedules": 1,
    "max_workflows": 0,
    "max_webhooks": 1
  },
  "usage": {
    "jobs_today": {"current": 150, "limit": 1000, "percentage": 15.0},
    "active_jobs": {"current": 5, "limit": 10, "percentage": 50.0},
    "queues": {"current": 1, "limit": 2, "percentage": 50.0}
  },
  "warnings": []
}
```

### Plan Limits

| Resource | Free | Starter | Pro | Enterprise |
|----------|------|---------|-----|------------|
| Jobs/day | 1,000 | 10,000 | 100,000 | Unlimited |
| Active jobs | 10 | 500 | 5,000 | Unlimited |
| Queues | 2 | 10 | 50 | Unlimited |
| Workers | 1 | 5 | 25 | Unlimited |
| API keys | 2 | 10 | 50 | Unlimited |
| Schedules | 1 | 25 | 100 | Unlimited |
| Workflows | ❌ | 5 | 50 | Unlimited |
| Webhooks | 1 | 10 | 50 | Unlimited |
| Payload size | 64KB | 256KB | 1MB | 1MB |
| Job retention | 3 days | 14 days | 30 days | 90 days |
| History retention | 1 day | 7 days | 30 days | 90 days |

`Workflows` counts workflows that are pending or running; finished runs do not hold a slot.
`API keys` counts the keys you created: the key Spooled makes for your dashboard session when you
sign in by email (`is_system: true` in `GET /api-keys`) is not counted. Re-activating a revoked
key or a paused schedule with `PUT` ... `{"is_active": true}` is checked against the limit like a
new one (`429 QUOTA_EXCEEDED`).

History retention covers the delivery history behind
`GET /api/v1/outgoing-webhooks/{id}/deliveries`, the run history of schedules
(`GET /api/v1/schedules/{id}/history`) and job history. Older records are removed, so on
Free a failure from yesterday is no longer listed and can no longer be retried, and a schedule's
runs from before yesterday are gone.

Job retention applies to every finished job: completed and cancelled jobs, and also failed and
dead-lettered jobs together with their dead-letter entries, are removed once they are older than
your plan's job retention. A failed step of a workflow that still exists is kept until the
workflow itself is removed, so a failed workflow stays retryable. Workers that have been offline
for more than 7 days are removed too.

---

## Rate Limits

| Tier | Requests/second | Burst |
|------|-----------------|-------|
| Free | 5 | 10 |
| Starter | 25 | 50 |
| Pro | 100 | 200 |
| Enterprise | 500 | 1,000 |
