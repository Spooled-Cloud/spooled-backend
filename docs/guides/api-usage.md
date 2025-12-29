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

When a limit is exceeded, the API returns `403 Forbidden`:

```json
{
  "error": "limit_exceeded",
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

With Redis caching enabled:
- **First request** (cache miss): ~100ms
- **Cached requests**: ~50ms
- **gRPC operations**: ~50ms (batch)

For high-throughput workloads, use the gRPC API which provides ~28x better performance compared to HTTP without caching.

---

## Authentication

### API Key Authentication

Include your API key in the `Authorization` header:

```bash
Authorization: Bearer sp_live_xxxxxxxxxxxxxxxxxxxx
```

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

## Dead Letter Queue

Jobs that have exhausted all retries are moved to the Dead Letter Queue (DLQ).

### List DLQ Jobs

```bash
GET /api/v1/jobs/dlq?queue_name=emails&limit=100
Authorization: Bearer <token>
```

Response:
```json
{
  "jobs": [
    {
      "id": "job_xxxxx",
      "queue_name": "emails",
      "status": "deadletter",
      "error_message": "Failed after 3 retries",
      "last_error_at": "2024-01-01T00:00:00Z"
    }
  ],
  "total": 1
}
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

**Note**: Retry operations respect plan limits for active jobs.

Response:
```json
{
  "retried_count": 25,
  "job_ids": ["job_xxxxx", "job_yyyyy", ...]
}
```

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

### Retry Failed Workflow

Retry all failed jobs in a workflow. This resets failed/deadletter jobs back to pending,
clears retry counts, and resumes the workflow. Only workflows with status `failed` can be retried.

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
| `payload` | object | Yes | Job data (max 5MB) |
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

### Security Best Practices

1. **Always configure a webhook token** for production use
2. **Use HTTPS** - webhooks are rejected over HTTP in production
3. **Use idempotency keys** - external services may retry failed deliveries
4. **Validate on your worker** - the payload comes from external sources

### Billing Webhook (Platform Admin Only)

The `/api/v1/billing/webhook` endpoint handles Stripe subscription events for
the platform billing system. This is configured in Stripe's webhook settings
and secured with `STRIPE_BILLING_WEBHOOK_SECRET`.

---

## Organizations

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
  "suggestion": "my-company-2"
}
```

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

If slug exists, automatically appends number:
```json
{
  "slug": "my-company-name-2"
}
```

---

## Billing

### Get Billing Status

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
| Payload size | 64KB | 256KB | 1MB | 5MB |
| Job retention | 3 days | 14 days | 30 days | 90 days |

---

## Rate Limits

| Tier | Requests/second | Burst |
|------|-----------------|-------|
| Free | 5 | 10 |
| Starter | 25 | 50 |
| Pro | 100 | 200 |
| Enterprise | 500 | 1,000 |
