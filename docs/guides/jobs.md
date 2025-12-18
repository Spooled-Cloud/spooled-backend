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

Create up to 100 jobs in a single request:

```bash
curl -X POST https://api.spooled.cloud/api/v1/jobs/bulk \
  -H "Authorization: Bearer sp_live_YOUR_API_KEY" \
  -H "Content-Type: application/json" \
  -d '{
    "jobs": [
      {"queue_name": "emails", "payload": {"user_id": 1}},
      {"queue_name": "emails", "payload": {"user_id": 2}},
      {"queue_name": "emails", "payload": {"user_id": 3}}
    ]
  }'
```

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
      "lease_expires_at": "2024-01-15T10:05:00Z"
    }
  ]
}
```

### Complete Job

Mark a job as successfully completed:

```bash
curl -X POST https://api.spooled.cloud/api/v1/jobs/job_xxx/complete \
  -H "Authorization: Bearer sp_live_YOUR_API_KEY" \
  -H "Content-Type: application/json" \
  -d '{
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
    "reason": "SMTP server unavailable"
  }'
```

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

---

## Job Properties

| Property | Type | Description |
|----------|------|-------------|
| `queue_name` | string | Target queue (required) |
| `payload` | object | Job data as JSON (required, max 1MB) |
| `priority` | integer | -100 to 100, higher = processed first (default: 0) |
| `max_retries` | integer | Max retry attempts (default: 3) |
| `timeout_seconds` | integer | Job timeout in seconds (default: 300) |
| `idempotency_key` | string | Prevent duplicate jobs (max 255 chars) |
| `tags` | array | Searchable tags |
| `scheduled_at` | datetime | Schedule for future execution |
| `completion_webhook` | string | URL to POST on completion |
| `expires_at` | datetime | Job expiration time |

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
    "timeout_seconds": 600,
    "rate_limit": 100
  }'
```

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
  "pending": 150,
  "processing": 25,
  "completed": 10000,
  "failed": 12,
  "deadletter": 3,
  "paused": false,
  "oldest_job_age_seconds": 30
}
```

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
- If a job with the same key exists (any status), the existing job is returned
- Keys are scoped to your organization
- Keys expire after 24 hours (configurable)

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

See [Schedules Guide](./schedules.md) for more details.

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
    job.fail(reason=str(e))  # Will retry
except PermanentError as e:
    job.fail(reason=str(e), no_retry=True)  # Go to DLQ
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
- [Schedules](./schedules.md) — Recurring jobs
- [Workflows](./workflows.md) — Job dependencies
- [API Reference](./api-usage.md) — Complete API documentation
