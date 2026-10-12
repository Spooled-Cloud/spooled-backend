# Jobs & Queues Guide

Complete guide to jobs and queues in Spooled Cloud.

## Table of Contents

1. [Overview](#overview)
2. [Job Lifecycle](#job-lifecycle)
3. [Creating Jobs](#creating-jobs)
4. [Processing Jobs](#processing-jobs)
5. [Job Properties](#job-properties)
6. [Queues](#queues)
7. [Priority](#priority)
8. [Idempotency](#idempotency)
9. [Scheduled Jobs](#scheduled-jobs)
10. [Best Practices](#best-practices)

---

## Overview

Jobs are the fundamental unit of work in Spooled. A job represents an asynchronous task that needs to be processed - sending emails, processing images, delivering webhooks, generating reports, etc.

**Key characteristics:**
- Jobs are JSON payloads stored in PostgreSQL
- Jobs are processed by external workers (your code)
- Jobs have built-in retry logic with exponential backoff
- Jobs support priorities, scheduling, and dependencies

---

## Job Lifecycle

```
┌─────────┐    ┌───────────┐    ┌────────────┐    ┌───────────┐
│ Created │───▶│  Pending  │───▶│ Processing │───▶│ Completed │
└─────────┘    └─────┬─────┘    └─────┬──────┘    └───────────┘
                     │                │
                     │                │ (failure)
                     │                ▼
                     │          ┌──────────┐
                     │          │  Failed  │
                     │          └────┬─────┘
                     │               │
                     │    (retry)    │ (max retries)
                     ◀───────────────┤
                                     ▼
                              ┌────────────┐
                              │ Deadletter │
                              └────────────┘
```

### Job States

| State | Description |
|-------|-------------|
| `pending` | Waiting to be claimed by a worker |
| `processing` | Currently being processed by a worker |
| `completed` | Successfully processed |
| `failed` | Processing failed (will retry if retries remaining) |
| `deadletter` | Failed after exhausting all retries |
| `cancelled` | Manually cancelled |
| `scheduled` | Scheduled for future execution |

---

## Creating Jobs

### Basic Job Creation

```bash
curl -X POST https://api.spooled.cloud/api/v1/jobs \
  -H "Authorization: Bearer sp_live_YOUR_API_KEY" \
  -H "Content-Type: application/json" \
  -d '{
    "queue_name": "emails",
    "payload": {
      "to": "user@example.com",
      "subject": "Welcome!",
      "template": "welcome"
    }
  }'
```

### Full Job Options

```bash
curl -X POST https://api.spooled.cloud/api/v1/jobs \
  -H "Authorization: Bearer sp_live_YOUR_API_KEY" \
  -H "Content-Type: application/json" \
  -d '{
    "queue_name": "emails",
    "payload": {
      "to": "user@example.com",
      "subject": "Welcome!",
      "template": "welcome"
    },
    "priority": 10,
    "max_retries": 5,
    "timeout_seconds": 300,
    "idempotency_key": "welcome-email-user123",
    "tags": ["transactional", "welcome"],
    "scheduled_at": "2024-12-25T09:00:00Z",
    "completion_webhook": "https://api.example.com/webhooks/job-complete"
  }'
```

### Response

```json
{
  "id": "job_550e8400e29b41d4",
  "organization_id": "org_xxxxx",
  "queue_name": "emails",
  "status": "pending",
  "payload": {
    "to": "user@example.com",
    "subject": "Welcome!",
    "template": "welcome"
  },
  "priority": 10,
  "max_retries": 5,
  "retry_count": 0,
  "created_at": "2024-01-15T10:00:00Z"
}
```

### Bulk Enqueue

Create up to 100 jobs in a single request. All jobs go to one `queue_name`; each item carries
its own `payload` and may override `priority`, `idempotency_key` and `scheduled_at`:

```bash
curl -X POST https://api.spooled.cloud/api/v1/jobs/bulk \
  -H "Authorization: Bearer sp_live_YOUR_API_KEY" \
  -H "Content-Type: application/json" \
  -d '{
    "queue_name": "emails",
    "jobs": [
      {"payload": {"user_id": 1}},
      {"payload": {"user_id": 2}, "priority": 10},
      {"payload": {"user_id": 3}, "idempotency_key": "welcome-3"}
    ]
  }'
```

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

Items are validated one by one. An invalid item (a payload over your plan's limit or containing the
NUL character, a `priority` outside -100..100, an `idempotency_key` over 255 characters) is listed
in `failed` with its `index` and the reason, and the valid items are still created. The batch
defaults are range-checked as well: `default_priority` -100..100, `default_max_retries` 0..100,
`default_timeout_seconds` 1..86400. The whole request is a `400` instead when **no** item is
valid (the message lists `jobs[i]: reason` for each one), when a default is out of range, or when
the database refuses a value, so a status check alone is enough to know nothing was created. An
item whose idempotency key already exists comes back with `created: false` and uses no quota.

---

## Processing Jobs

### Claim Jobs

Workers claim jobs to process them:

```bash
curl -X POST https://api.spooled.cloud/api/v1/jobs/claim \
  -H "Authorization: Bearer sp_live_YOUR_API_KEY" \
  -H "Content-Type: application/json" \
  -d '{
    "queue_name": "emails",
    "worker_id": "worker-1",
    "limit": 10
  }'
```

Response:

```json
{
  "jobs": [
    {
      "id": "job_550e8400e29b41d4",
      "queue_name": "emails",
      "status": "processing",
      "payload": {"to": "user@example.com"},
      "lease_expires_at": "2024-01-15T10:00:30Z",
      "lease_id": "lease_7f3c9a"
    }
  ]
}
```

The lease lasts `lease_duration_secs` (default 30 seconds). Keep `lease_id`: complete, fail and
heartbeat requests must send it (a missing `lease_id` is a `422`).

### Complete Job

Mark a job as successfully completed:

```bash
curl -X POST https://api.spooled.cloud/api/v1/jobs/job_xxx/complete \
  -H "Authorization: Bearer sp_live_YOUR_API_KEY" \
  -H "Content-Type: application/json" \
  -d '{
    "worker_id": "worker-1",
    "lease_id": "lease_from_claim",
    "result": {"message_id": "msg_123"}
  }'
```

### Fail Job

Mark a job as failed (will retry if retries remaining):

```bash
curl -X POST https://api.spooled.cloud/api/v1/jobs/job_xxx/fail \
  -H "Authorization: Bearer sp_live_YOUR_API_KEY" \
  -H "Content-Type: application/json" \
  -d '{
    "worker_id": "worker-1",
    "lease_id": "lease_from_claim",
    "error": "SMTP server unavailable"
  }'
```

### Renew a Lease (Heartbeat)

For a job that runs longer than its lease, renew the lease before it expires:

```bash
curl -X POST https://api.spooled.cloud/api/v1/jobs/job_xxx/heartbeat \
  -H "Authorization: Bearer sp_live_YOUR_API_KEY" \
  -H "Content-Type: application/json" \
  -d '{
    "worker_id": "worker-1",
    "lease_id": "lease_from_claim"
  }'
```

Without `lease_duration_secs` the lease is renewed for the length it was claimed with (a job
claimed with `lease_duration_secs: 600` keeps getting 600 seconds); send `lease_duration_secs`
(5-3600) to change it, and later heartbeats keep the new length. `worker_id` and `lease_id` are
required (`422` when missing); a wrong or expired `lease_id` is a `409`.

### Cancel Job

Cancel a pending or scheduled job:

```bash
curl -X DELETE https://api.spooled.cloud/api/v1/jobs/job_xxx \
  -H "Authorization: Bearer sp_live_YOUR_API_KEY"
```

### Retry Job

Manually retry a failed or dead-letter job:

```bash
curl -X POST https://api.spooled.cloud/api/v1/jobs/job_xxx/retry \
  -H "Authorization: Bearer sp_live_YOUR_API_KEY"
```

If the job is a step of a workflow, retrying it also re-opens the steps that were cancelled
because it failed (they wait again until it completes; a step you cancelled yourself stays
cancelled) and puts a `failed` workflow back to `running`. A running workflow counts toward your
plan's workflow limit, so the retry answers `429 QUOTA_EXCEEDED` when no slot is free, and
`403 feature_disabled` on a plan without workflows.

### List Jobs

```bash
curl "https://api.spooled.cloud/api/v1/jobs?queue_name=emails&status=pending&order_by=priority&order_dir=desc&limit=50" \
  -H "Authorization: Bearer sp_live_YOUR_API_KEY"
```

| Parameter | Description |
|-----------|-------------|
| `queue_name` | Only this queue (letters, digits, `-`, `_`, `.`) |
| `status` | `pending`, `scheduled`, `processing`, `completed`, `failed`, `deadletter` or `cancelled` (any case) |
| `tag` | One tag, any characters up to 1024 bytes (`customer:42`) |
| `order_by` | `created_at` (default), `updated_at`, `scheduled_at`, `completed_at`, `priority`, `status`, `queue_name`, `retry_count` |
| `order_dir` | `desc` (default) or `asc` |
| `limit`, `offset` | Page size (default 50, at most 100) and rows to skip. There is no cursor |

An invalid `status`, `queue_name`, `tag`, `order_by` or `order_dir` is a `400`; it is not ignored.
Rows that tie on the sort column are ordered by job id, so `offset` pages do not repeat or skip
jobs. `GET /api/v1/jobs/dlq` takes the same parameters (without `status`).

---

## Job Properties

| Property | Type | Description |
|----------|------|-------------|
| `queue_name` | string | Target queue (required) |
| `payload` | object | Job data as JSON (required, max 1MB) |
| `priority` | integer | -100 to 100, higher = processed first (default: 0) |
| `max_retries` | integer | Max retry attempts (default: 3) |
| `timeout_seconds` | integer | Job timeout in seconds (default: 300) |
| `idempotency_key` | string | Prevent duplicate jobs (max 255 chars; blank = no key) |
| `tags` | array | Searchable tags |
| `scheduled_at` | datetime | Schedule for future execution |
| `completion_webhook` | string | URL to POST on completion |
| `expires_at` | datetime | Job expiration time; must be in the future and later than `scheduled_at` (otherwise `400`) |

---

## Queues

Queues are logical groupings of jobs. Jobs in the same queue are processed in priority/FIFO order.

### Implicit Queue Creation

Queues are created automatically when you enqueue the first job:

```bash
# This creates the "emails" queue if it doesn't exist
curl -X POST https://api.spooled.cloud/api/v1/jobs \
  -d '{"queue_name": "emails", "payload": {...}}'
```

### Queue Configuration

Configure queue-level defaults:

```bash
curl -X PUT https://api.spooled.cloud/api/v1/queues/emails/config \
  -H "Authorization: Bearer sp_live_YOUR_API_KEY" \
  -H "Content-Type: application/json" \
  -d '{
    "max_retries": 5,
    "default_timeout": 600,
    "rate_limit": 100
  }'
```

`max_retries` must be 0-100, `default_timeout` 1-86400 seconds and `rate_limit` 1-100000, otherwise the
request is a `400`; the queue name in the path must be 1-100 letters, digits, `-`, `_` or `.` and may
not start with `.` or `-`. Jobs that omit `max_retries` or `timeout_seconds` take the queue's values.

`rate_limit` is stored and returned, but it is **not enforced yet**: workers are not throttled by it.

### Queue Operations

```bash
# Pause a queue (jobs won't be claimed)
curl -X POST https://api.spooled.cloud/api/v1/queues/emails/pause

# Resume a queue
curl -X POST https://api.spooled.cloud/api/v1/queues/emails/resume

# Get queue stats
curl https://api.spooled.cloud/api/v1/queues/emails/stats
```

### Queue Stats Response

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

`failed_jobs_24h` counts jobs that were dead-lettered in the last 24 hours, and `deadletter_jobs` is
the number of jobs in the queue's dead-letter queue now.

---

## Priority

Jobs with higher priority are processed first. Default is 0.

```bash
# High priority (processed first)
curl -X POST https://api.spooled.cloud/api/v1/jobs \
  -d '{"queue_name": "emails", "payload": {...}, "priority": 100}'

# Normal priority
curl -X POST https://api.spooled.cloud/api/v1/jobs \
  -d '{"queue_name": "emails", "payload": {...}, "priority": 0}'

# Low priority (processed last)
curl -X POST https://api.spooled.cloud/api/v1/jobs \
  -d '{"queue_name": "emails", "payload": {...}, "priority": -100}'
```

### Priority Use Cases

| Priority | Use Case |
|----------|----------|
| 100 | Password resets, security alerts |
| 50 | Real-time notifications |
| 0 | Normal background jobs |
| -50 | Batch processing |
| -100 | Report generation, analytics |

### Boost Priority

Boost an existing job's priority:

```bash
curl -X PUT https://api.spooled.cloud/api/v1/jobs/job_xxx/priority \
  -d '{"priority": 50}'
```

---

## Idempotency

Use `idempotency_key` to prevent duplicate jobs:

```bash
curl -X POST https://api.spooled.cloud/api/v1/jobs \
  -d '{
    "queue_name": "emails",
    "payload": {"user_id": 123, "template": "welcome"},
    "idempotency_key": "welcome-email-user-123"
  }'
```

**Behavior:**
- If a job with the same key exists (any status), the existing job is returned (`200` with
  `"created": false`). The replay is answered before quota checks and uses no quota, so retrying a
  request whose response you lost never fails because you are at a limit
- Keys are scoped to your organization
- A key has no time-to-live: it lives as long as its job. Finished jobs (completed, cancelled,
  failed or dead-lettered) are removed after your plan's job retention (Free 3 days, Starter 14,
  Pro 30, Enterprise 90), and until then they keep answering for their key
- An empty or whitespace-only `idempotency_key` counts as no key

**Best practices:**
- Use deterministic keys based on the operation
- Include relevant IDs: `payment-confirmation-order-{order_id}`
- Include timestamps for time-sensitive operations: `daily-report-2024-01-15`

---

## Scheduled Jobs

### One-time Scheduled Job

Schedule a job for future execution:

```bash
curl -X POST https://api.spooled.cloud/api/v1/jobs \
  -d '{
    "queue_name": "reports",
    "payload": {"type": "daily_report"},
    "scheduled_at": "2024-12-25T09:00:00Z"
  }'
```

### Recurring Jobs (Cron)

Create recurring jobs with schedules:

```bash
curl -X POST https://api.spooled.cloud/api/v1/schedules \
  -d '{
    "name": "Daily Report",
    "cron_expression": "0 9 * * *",
    "timezone": "America/New_York",
    "queue_name": "reports",
    "payload_template": {"type": "daily_report"}
  }'
```

See the [Schedules section of the API Usage Guide](./api-usage.md#schedules) for lifecycle operations.

---

## Best Practices

### 1. Use Idempotency Keys

Always use idempotency keys for critical operations:

```json
{
  "queue_name": "payments",
  "payload": {"order_id": 123},
  "idempotency_key": "charge-order-123"
}
```

### 2. Set Appropriate Timeouts

Match timeout to expected processing time:

```json
{
  "queue_name": "video-processing",
  "payload": {"video_url": "..."},
  "timeout_seconds": 1800
}
```

### 3. Use Tags for Organization

Make jobs searchable:

```json
{
  "queue_name": "emails",
  "payload": {...},
  "tags": ["transactional", "welcome", "user-123"]
}
```

### 4. Handle Failures Gracefully

In your worker, provide meaningful error messages:

```python
try:
    process_job(job)
    job.complete(result={"success": True})
except TemporaryError as e:
    job.fail(error=str(e))  # Will retry
except PermanentError as e:
    job.fail(error=str(e), retry=False)  # gRPC workers can skip retry; REST retries by policy
```

### 5. Monitor Queue Health

Watch these metrics:
- `spooled_jobs_pending` - Jobs waiting
- `spooled_job_max_age_seconds` - Oldest job age (alert if > 300s)
- `spooled_jobs_dead_letter` - Failed jobs

### 6. Use Completion Webhooks

Get notified when jobs complete:

```json
{
  "queue_name": "exports",
  "payload": {"user_id": 123},
  "completion_webhook": "https://api.example.com/webhooks/export-complete"
}
```

---

## Next Steps

- [Retries & DLQ](./retries.md) — Configure retry behavior
- [Workers](./workers.md) — Build production workers
- [Schedules](./api-usage.md#schedules) — Recurring jobs
- [Workflows](./api-usage.md#workflows) — Job dependencies
- [API Reference](./api-usage.md) — Complete API documentation
