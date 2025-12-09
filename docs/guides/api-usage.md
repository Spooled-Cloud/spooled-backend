# Spooled Cloud API Usage Guide

Complete guide to using the Spooled Cloud REST API.

## Table of Contents

1. [Authentication](#authentication)
2. [Jobs](#jobs)
3. [Queues](#queues)
4. [Workers](#workers)
5. [Schedules](#schedules)
6. [Workflows](#workflows)
7. [Webhooks](#webhooks)
8. [Real-time Events](#real-time-events)
9. [Error Handling](#error-handling)

---

## Authentication

### API Key Authentication

Include your API key in the `Authorization` header:

```bash
Authorization: Bearer sk_live_xxxxxxxxxxxxxxxxxxxx
```

### JWT Authentication

For enhanced security, exchange your API key for JWT tokens:

#### Login

```bash
POST /api/v1/auth/login
Content-Type: application/json

{
  "api_key": "sk_live_xxxxxxxxxxxxxxxxxxxx"
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
  "payload": {...},
  "priority": 0,
  "max_retries": 3,
  "retry_count": 0,
  "created_at": "2024-01-01T00:00:00Z"
}
```

### List Jobs

```bash
GET /api/v1/jobs?queue_name=emails&status=pending&limit=50
Authorization: Bearer <token>
```

**Query Parameters:**

| Parameter | Type | Description |
|-----------|------|-------------|
| `queue_name` | string | Filter by queue |
| `status` | string | pending, processing, completed, failed, deadletter |
| `limit` | integer | Max results (default: 100) |
| `cursor` | string | Pagination cursor |
| `order_by` | string | created_at, priority |
| `order_dir` | string | asc, desc |

### Get Job

```bash
GET /api/v1/jobs/{job_id}
Authorization: Bearer <token>
```

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
  "jobs": [
    {"queue_name": "emails", "payload": {"id": 1}},
    {"queue_name": "emails", "payload": {"id": 2}},
    {"queue_name": "emails", "payload": {"id": 3}}
  ]
}
```

Response:
```json
{
  "created": 3,
  "failed": 0,
  "jobs": [
    {"id": "job_xxx1", "status": "pending"},
    {"id": "job_xxx2", "status": "pending"},
    {"id": "job_xxx3", "status": "pending"}
  ]
}
```

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
  "pending_count": 150,
  "processing_count": 25,
  "completed_count": 10000,
  "failed_count": 12,
  "avg_processing_time_ms": 250,
  "oldest_job_age_seconds": 30
}
```

### Update Queue Config

```bash
PUT /api/v1/queues/{queue_name}/config
Content-Type: application/json
Authorization: Bearer <token>

{
  "max_retries": 5,
  "timeout_seconds": 600,
  "rate_limit": 100
}
```

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

Deletes all jobs in the queue:

```bash
DELETE /api/v1/queues/{queue_name}
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
  "id": "worker-abc123",
  "queue_names": ["emails", "notifications"],
  "max_concurrent_jobs": 5,
  "hostname": "worker-1.example.com",
  "metadata": {
    "version": "1.0.0",
    "region": "us-east-1"
  }
}
```

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
```
┌───────────── second (0 - 59)
│ ┌───────────── minute (0 - 59)
│ │ ┌───────────── hour (0 - 23)
│ │ │ ┌───────────── day of month (1 - 31)
│ │ │ │ ┌───────────── month (1 - 12)
│ │ │ │ │ ┌───────────── day of week (0 - 6, Sun=0)
│ │ │ │ │ │
* * * * * *
```

**Examples:**
- `0 * * * * *` - Every minute
- `0 */5 * * * *` - Every 5 minutes
- `0 0 9 * * *` - Daily at 9 AM
- `0 0 0 * * 1` - Every Monday at midnight
- `0 0 0 1 * *` - First of every month

### List Schedules

```bash
GET /api/v1/schedules
Authorization: Bearer <token>
```

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
      "step_name": "validate",
      "queue_name": "orders",
      "payload": {"action": "validate"}
    },
    {
      "step_name": "process_payment",
      "queue_name": "payments",
      "payload": {"action": "charge"},
      "depends_on": ["validate"]
    },
    {
      "step_name": "ship",
      "queue_name": "shipping",
      "payload": {"action": "ship"},
      "depends_on": ["process_payment"]
    },
    {
      "step_name": "notify",
      "queue_name": "notifications",
      "payload": {"action": "notify"},
      "depends_on": ["ship"]
    }
  ]
}
```

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

### Cancel Workflow

```bash
POST /api/v1/workflows/{workflow_id}/cancel
Authorization: Bearer <token>
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

---

## Webhooks

### GitHub Webhook

```bash
POST /api/v1/webhooks/github
X-Hub-Signature-256: sha256=...
Content-Type: application/json

{
  "action": "opened",
  "pull_request": {...}
}
```

### Stripe Webhook

```bash
POST /api/v1/webhooks/stripe
Stripe-Signature: t=...,v1=...
Content-Type: application/json

{
  "type": "payment_intent.succeeded",
  "data": {...}
}
```

### Custom Webhook

```bash
POST /api/v1/webhooks/custom
X-Webhook-Signature: sha256=...
Content-Type: application/json

{
  "queue_name": "custom_events",
  "payload": {...}
}
```

---

## Real-time Events

### WebSocket

Connect to receive real-time updates:

```javascript
const ws = new WebSocket('wss://api.example.com/api/v1/ws?queues=emails,notifications');

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

**Event Format:**
```
event: job.completed
data: {"id":"job_xxx","status":"completed","result":{...}}

event: queue.stats
data: {"queue_name":"emails","pending":150,"processing":25}
```

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
| 429 | `rate_limited` | Too many requests |
| 500 | `internal_error` | Server error |

### Rate Limiting

When rate limited, check these headers:

```
X-RateLimit-Limit: 1000
X-RateLimit-Remaining: 0
X-RateLimit-Reset: 1704067200
Retry-After: 60
```

---

## Best Practices

1. **Use Idempotency Keys** - Prevent duplicate jobs on retries
2. **Set Appropriate Timeouts** - Match job duration expectations
3. **Use Tags** - Make jobs searchable and filterable
4. **Monitor Queue Age** - Alert on `spooled_job_max_age_seconds`
5. **Handle Webhooks** - Use completion webhooks for async flows
6. **Implement Circuit Breakers** - Handle downstream failures gracefully

## Rate Limits

| Tier | Requests/second | Burst |
|------|-----------------|-------|
| Free | 100 | 200 |
| Pro | 1,000 | 2,000 |
| Enterprise | Custom | Custom |

