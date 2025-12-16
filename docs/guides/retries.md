# Retries & Dead Letter Queue Guide

Complete guide to retry configuration and dead letter queue management in Spooled Cloud.

## Table of Contents

1. [Overview](#overview)
2. [Automatic Retries](#automatic-retries)
3. [Backoff Strategies](#backoff-strategies)
4. [Dead Letter Queue](#dead-letter-queue)
5. [DLQ Management](#dlq-management)
6. [Best Practices](#best-practices)

---

## Overview

Spooled provides robust retry handling to ensure reliable job processing:

- **Automatic retries** with configurable limits
- **Exponential backoff** with jitter to prevent thundering herd
- **Dead letter queue (DLQ)** for jobs that exhaust retries
- **Manual retry** capability for DLQ jobs

---

## Automatic Retries

When a job fails, Spooled automatically retries it based on the configuration.

### Retry Flow

```
┌─────────────┐     ┌─────────────┐     ┌─────────────┐
│ Processing  │────▶│   Failed    │────▶│   Pending   │
│   (fails)   │     │             │     │  (delayed)  │
└─────────────┘     └──────┬──────┘     └─────────────┘
                          │
                          │ (max retries exceeded)
                          ▼
                   ┌─────────────┐
                   │ Dead Letter │
                   └─────────────┘
```

### Configure Retries

**Per job:**

```bash
curl -X POST https://api.spooled.cloud/api/v1/jobs \
  -H "Authorization: Bearer sk_live_YOUR_API_KEY" \
  -H "Content-Type: application/json" \
  -d '{
    "queue_name": "emails",
    "payload": {"to": "user@example.com"},
    "max_retries": 5
  }'
```

**Per queue (default for all jobs):**

```bash
curl -X PUT https://api.spooled.cloud/api/v1/queues/emails/config \
  -H "Authorization: Bearer sk_live_YOUR_API_KEY" \
  -H "Content-Type: application/json" \
  -d '{
    "max_retries": 3
  }'
```

### Fail a Job

When your worker fails a job, it will be retried:

```bash
curl -X POST https://api.spooled.cloud/api/v1/jobs/job_xxx/fail \
  -H "Authorization: Bearer sk_live_YOUR_API_KEY" \
  -H "Content-Type: application/json" \
  -d '{
    "reason": "Connection timeout to SMTP server"
  }'
```

### Skip Retries

For permanent failures, skip retries and send directly to DLQ:

```bash
curl -X POST https://api.spooled.cloud/api/v1/jobs/job_xxx/fail \
  -H "Authorization: Bearer sk_live_YOUR_API_KEY" \
  -H "Content-Type: application/json" \
  -d '{
    "reason": "Invalid email address format",
    "no_retry": true
  }'
```

---

## Backoff Strategies

Spooled uses exponential backoff with jitter to prevent thundering herd problems.

### Default Backoff

```
delay = min(base_delay * 2^attempt + random_jitter, max_delay)
```

| Attempt | Base Delay | With Jitter (approx) |
|---------|------------|---------------------|
| 1 | 1s | 1-1.5s |
| 2 | 2s | 2-2.5s |
| 3 | 4s | 4-4.5s |
| 4 | 8s | 8-8.5s |
| 5 | 16s | 16-16.5s |
| 6 | 32s | 32-32.5s |
| 7 | 64s | 64-64.5s |
| 8 | 128s | 128-128.5s |

**Parameters:**
- `base_delay` = 1 second
- `max_delay` = 1 hour (3600 seconds)
- `random_jitter` = 0-500ms

### Custom Backoff (Per Queue)

```bash
curl -X PUT https://api.spooled.cloud/api/v1/queues/emails/config \
  -H "Authorization: Bearer sk_live_YOUR_API_KEY" \
  -H "Content-Type: application/json" \
  -d '{
    "backoff_base_seconds": 5,
    "backoff_multiplier": 2.0,
    "backoff_max_seconds": 3600
  }'
```

### Backoff Examples

| Use Case | Base | Max | Result |
|----------|------|-----|--------|
| Fast retry (API calls) | 1s | 60s | 1s, 2s, 4s, 8s, 16s, 32s, 60s |
| Slow retry (external services) | 30s | 3600s | 30s, 60s, 120s, 240s... |
| Aggressive retry (transient errors) | 0.5s | 30s | 0.5s, 1s, 2s, 4s, 8s, 16s, 30s |

---

## Dead Letter Queue

Jobs that fail after exhausting all retries are moved to the Dead Letter Queue (DLQ).

### Why Jobs Go to DLQ

1. **Max retries exceeded** - Failed `max_retries` times
2. **Manual rejection** - Worker called fail with `no_retry: true`
3. **Timeout exceeded** - Job lease expired without completion
4. **Poison pill** - Repeated crashes processing the job

### View DLQ Jobs

```bash
curl "https://api.spooled.cloud/api/v1/jobs?status=dead_letter" \
  -H "Authorization: Bearer sk_live_YOUR_API_KEY"
```

Response:

```json
{
  "jobs": [
    {
      "id": "job_xxx",
      "queue_name": "emails",
      "status": "dead_letter",
      "payload": {"to": "user@example.com"},
      "error_message": "SMTP server unavailable after 5 retries",
      "retry_count": 5,
      "max_retries": 5,
      "last_failed_at": "2024-01-15T10:30:00Z",
      "created_at": "2024-01-15T10:00:00Z"
    }
  ],
  "total": 1
}
```

### Filter DLQ by Queue

```bash
curl "https://api.spooled.cloud/api/v1/jobs?status=dead_letter&queue_name=emails" \
  -H "Authorization: Bearer sk_live_YOUR_API_KEY"
```

---

## DLQ Management

### Retry a Single Job

```bash
curl -X POST https://api.spooled.cloud/api/v1/jobs/job_xxx/retry \
  -H "Authorization: Bearer sk_live_YOUR_API_KEY"
```

This:
- Resets retry count to 0
- Moves job back to `pending` status
- Job will be processed by next available worker

### Bulk Retry

Retry multiple jobs by ID:

```bash
curl -X POST https://api.spooled.cloud/api/v1/jobs/dlq/retry \
  -H "Authorization: Bearer sk_live_YOUR_API_KEY" \
  -H "Content-Type: application/json" \
  -d '{
    "job_ids": ["job_xxx", "job_yyy", "job_zzz"]
  }'
```

Retry all DLQ jobs for a queue:

```bash
curl -X POST https://api.spooled.cloud/api/v1/queues/emails/dlq/retry-all \
  -H "Authorization: Bearer sk_live_YOUR_API_KEY"
```

### Purge DLQ

Permanently delete DLQ jobs (irreversible):

```bash
curl -X POST https://api.spooled.cloud/api/v1/jobs/dlq/purge \
  -H "Authorization: Bearer sk_live_YOUR_API_KEY" \
  -H "Content-Type: application/json" \
  -d '{
    "queue_name": "emails",
    "older_than": "2024-01-01T00:00:00Z",
    "confirm": true
  }'
```

### Export DLQ for Analysis

```bash
curl "https://api.spooled.cloud/api/v1/jobs?status=dead_letter&limit=1000" \
  -H "Authorization: Bearer sk_live_YOUR_API_KEY" \
  -H "Accept: application/json" \
  > dlq_export.json
```

---

## Best Practices

### 1. Set Appropriate Retry Counts

| Job Type | Suggested Retries |
|----------|-------------------|
| Email sending | 3-5 |
| Payment processing | 5-10 |
| External API calls | 3 |
| File processing | 2-3 |
| Webhooks | 5-10 |

### 2. Distinguish Temporary vs Permanent Failures

```python
# In your worker
try:
    result = process_job(job)
    job.complete(result)
except ConnectionError as e:
    # Temporary - will retry
    job.fail(reason=str(e))
except ValidationError as e:
    # Permanent - skip to DLQ
    job.fail(reason=str(e), no_retry=True)
```

### 3. Monitor DLQ Growth

Set up alerts for:

```promql
# Alert if DLQ has jobs
spooled_jobs_dead_letter > 0

# Alert if DLQ growing fast
rate(spooled_jobs_dead_letter_total[5m]) > 1
```

### 4. Review DLQ Regularly

- Set up a daily/weekly DLQ review process
- Look for patterns in failure reasons
- Fix root causes before bulk retrying

### 5. Use Completion Webhooks for Failures

Get notified when jobs fail completely:

```bash
curl -X POST https://api.spooled.cloud/api/v1/webhooks/subscriptions \
  -d '{
    "events": ["job.dead_letter"],
    "url": "https://api.example.com/webhooks/job-failures"
  }'
```

### 6. Implement Circuit Breakers

For external service calls, implement circuit breakers:

```python
from circuitbreaker import circuit

@circuit(failure_threshold=5, recovery_timeout=60)
def call_external_api(data):
    return requests.post("https://api.external.com", json=data)

try:
    call_external_api(job.payload)
    job.complete()
except CircuitBreakerError:
    # Circuit open - fail without retry, try again later
    job.fail(reason="Circuit breaker open", no_retry=True)
```

---

## Retry State Inspection

### Get Job with Retry History

```bash
curl https://api.spooled.cloud/api/v1/jobs/job_xxx?include=history \
  -H "Authorization: Bearer sk_live_YOUR_API_KEY"
```

Response:

```json
{
  "id": "job_xxx",
  "status": "dead_letter",
  "retry_count": 5,
  "max_retries": 5,
  "history": [
    {"status": "pending", "at": "2024-01-15T10:00:00Z"},
    {"status": "processing", "at": "2024-01-15T10:00:01Z"},
    {"status": "failed", "at": "2024-01-15T10:00:05Z", "error": "Connection timeout"},
    {"status": "pending", "at": "2024-01-15T10:00:06Z"},
    {"status": "processing", "at": "2024-01-15T10:00:08Z"},
    {"status": "failed", "at": "2024-01-15T10:00:12Z", "error": "Connection timeout"},
    ...
    {"status": "dead_letter", "at": "2024-01-15T10:05:00Z"}
  ]
}
```

---

## Next Steps

- [Jobs Guide](./jobs.md) — Job creation and processing
- [Workers Guide](./workers.md) — Build production workers
- [Monitoring](./operations.md) — Monitor retry metrics
- [API Reference](./api-usage.md) — Complete API documentation
