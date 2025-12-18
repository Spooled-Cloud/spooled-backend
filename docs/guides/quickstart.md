# Spooled Cloud Quick Start Guide

Get up and running with Spooled Cloud in 5 minutes.

## Table of Contents

1. [Cloud vs Self-Hosted](#cloud-vs-self-hosted)
2. [Cloud Quickstart](#cloud-quickstart)
3. [Self-Hosted Quickstart](#self-hosted-quickstart)
4. [SDK Examples](#sdk-examples)
5. [Worker Patterns](#worker-patterns)
6. [Common Operations](#common-operations)
7. [Real-time Updates](#real-time-updates)
8. [Monitoring](#monitoring)
9. [Key Concepts](#key-concepts)
10. [Troubleshooting](#troubleshooting)

---

## Cloud vs Self-Hosted

| Feature | Cloud (api.spooled.cloud) | Self-Hosted |
|---------|---------------------------|-------------|
| Setup time | 2 minutes | 10 minutes |
| Maintenance | Zero | You manage |
| Scaling | Automatic | Manual |
| Best for | Production, teams | Development, air-gapped |

---

## Cloud Quickstart

### Prerequisites

- A Spooled Cloud account — [Sign up for free](https://spooled.cloud/signup)
- An API key — Available in your [dashboard](https://spooled.cloud/account/api-keys)

### Step 1: Queue Your First Job

```bash
curl -X POST https://api.spooled.cloud/api/v1/jobs \
  -H "Authorization: Bearer sp_live_YOUR_API_KEY" \
  -H "Content-Type: application/json" \
  -d '{
    "queue_name": "my-queue",
    "payload": {
      "event": "user.created",
      "user_id": "usr_123",
      "email": "alice@example.com"
    },
    "idempotency_key": "user-created-usr_123"
  }'
```

### Step 2: Process Jobs

```bash
# Claim up to 5 jobs
curl -X POST https://api.spooled.cloud/api/v1/jobs/claim \
  -H "Authorization: Bearer sp_live_YOUR_API_KEY" \
  -d '{"queue_name": "my-queue", "limit": 5}'

# Complete a job
curl -X POST https://api.spooled.cloud/api/v1/jobs/job_xyz123/complete \
  -H "Authorization: Bearer sp_live_YOUR_API_KEY"

# Or fail it (will retry)
curl -X POST https://api.spooled.cloud/api/v1/jobs/job_xyz123/fail \
  -H "Authorization: Bearer sp_live_YOUR_API_KEY" \
  -d '{"reason": "Connection timeout"}'
```

---

## Self-Hosted Quickstart

### Prerequisites

- Docker & Docker Compose
- Rust 1.85+ (for local development)
- curl or httpie

### Step 1: Start the Services

```bash
cd spooled-backend

# Start PostgreSQL, Redis, PgBouncer, and monitoring
docker-compose up -d

# Verify services are running
docker-compose ps
```

Expected output:
```
NAME                 STATUS              PORTS
spooled-postgres     running (healthy)   5432/tcp
spooled-pgbouncer    running (healthy)   6432/tcp
spooled-redis        running (healthy)   6379/tcp
spooled-prometheus   running             9091/tcp
spooled-grafana      running             3000/tcp
```

### Step 2: Configure Environment

```bash
# Copy the example environment file
cp .env.example .env

# The defaults work for local development
# For production, you MUST change JWT_SECRET and passwords
```

### Step 3: Run the Backend

```bash
# Run the server (migrations run automatically on first start)
cargo run
```

You should see:
```
INFO spooled_backend: Starting Spooled Backend v1.0.0
INFO spooled_backend: Database connection pool established
INFO spooled_backend: Database migrations completed
INFO spooled_backend: Redis cache connected
INFO spooled_backend: REST API listening on 0.0.0.0:8080
INFO spooled_backend: gRPC API listening on 0.0.0.0:50051
INFO spooled_backend: Metrics server listening on 0.0.0.0:9090
```

**Ports:**
- `:8080` - REST API (HTTP/1.1)
- `:50051` - gRPC API (HTTP/2 + Protobuf) (self-hosted)
- `:9090` - Prometheus metrics

**Spooled Cloud gRPC endpoint:**
- `grpc.spooled.cloud:443` (TLS)

### Step 4: Verify Installation

```bash
curl http://localhost:8080/health
```

Expected response:
```json
{"status":"healthy","version":"1.0.0","database":true,"cache":true}
```

### Step 5: Create Your Organization

Every tenant in Spooled has an organization. When you create one, you get an initial API key:

```bash
curl -X POST http://localhost:8080/api/v1/organizations \
  -H "Content-Type: application/json" \
  -d '{
    "name": "My Company",
    "slug": "my-company"
  }'
```

Response:
```json
{
  "id": "org_a1b2c3d4e5f6",
  "name": "My Company",
  "slug": "my-company",
  "plan_tier": "free",
  "api_key": "sk_a1b2c3d4e5f6g7h8i9j0...",
  "api_key_id": "key_x1y2z3",
  "created_at": "2024-12-09T10:00:00Z"
}
```

⚠️ **IMPORTANT**: Save the `api_key` value immediately! It's only shown once and cannot be retrieved later.

### Step 6: Create Your First Job

```bash
curl -X POST http://localhost:8080/api/v1/jobs \
  -H "Content-Type: application/json" \
  -H "Authorization: Bearer sp_live_abc123xyz789..." \
  -d '{
    "queue_name": "emails",
    "payload": {
      "to": "user@example.com",
      "subject": "Welcome!",
      "body": "This is my first job!"
    },
    "priority": 0,
    "max_retries": 3
  }'
```

Response:
```json
{
  "id": "job_m1n2o3p4",
  "organization_id": "org_a1b2c3d4e5f6",
  "queue_name": "emails",
  "status": "pending",
  "payload": {"to": "user@example.com", "subject": "Welcome!", "body": "This is my first job!"},
  "priority": 0,
  "max_retries": 3,
  "created_at": "2024-12-09T10:02:00Z"
}
```

---

## SDK Examples

> **All SDKs are production-ready!** Node.js, Python, Go, and PHP SDKs are fully featured with comprehensive test coverage.

### Node.js

```bash
npm install @spooled/sdk
```

```typescript
import { SpooledClient } from '@spooled/sdk';

const client = new SpooledClient({
  apiKey: process.env.SPOOLED_API_KEY!,
  // For self-hosted: baseUrl: 'http://localhost:8080'
});

// Create a job
const job = await client.jobs.create({
  queueName: 'email-notifications',
  payload: {
    to: 'user@example.com',
    subject: 'Welcome!',
    template: 'welcome',
  },
  idempotencyKey: `welcome-${userId}`,
  maxRetries: 5,
});

console.log(`Queued job: ${job.id}`);
```

### Python

```bash
pip install spooled-sdk
```

```python
from spooled import SpooledClient
import os

client = SpooledClient(
    api_key=os.environ["SPOOLED_API_KEY"],
    # For self-hosted: base_url="http://localhost:8080"
)

# Create a background job
job = client.jobs.create({
    "queue_name": "image-processing",
    "payload": {
        "image_url": "https://example.com/image.jpg",
        "operations": ["resize", "compress"],
        "output_format": "webp"
    },
    "idempotency_key": f"process-image-{image_id}",
    "max_retries": 3
})

print(f"Queued job: {job.id}")
```

### Go

```bash
go get github.com/spooled-cloud/spooled-go
```

```go
package main

import (
    "context"
    "log"
    "os"
    
    "github.com/spooled-cloud/spooled-sdk-go/spooled"
    "github.com/spooled-cloud/spooled-sdk-go/spooled/resources"
)

func main() {
    client, _ := spooled.NewClient(spooled.WithAPIKey(os.Getenv("SPOOLED_API_KEY")))
    // For self-hosted: spooled.WithBaseURL("http://localhost:8080")
    defer client.Close()
    
    job, err := client.Jobs().Create(context.Background(), &resources.CreateJobRequest{
        QueueName: "webhook-delivery",
        Payload: map[string]interface{}{
            "url":     "https://customer.example.com/webhook",
            "event":   "order.completed",
            "payload": orderData,
        },
        IdempotencyKey: stringPtr("order-webhook-" + orderId),
        MaxRetries:     intPtr(5),
    })
    
    if err != nil {
        log.Fatal(err)
    }
    
    log.Printf("Queued job: %s", job.ID)
}
```

### PHP

```bash
composer require spooled-cloud/spooled
```

```php
<?php

use Spooled\SpooledClient;
use Spooled\Config\ClientOptions;

$client = new SpooledClient(new ClientOptions(
    apiKey: getenv('SPOOLED_API_KEY'),
    // For self-hosted: baseUrl: 'http://localhost:8080'
));

// Create a job
$job = $client->jobs->create([
    'queue' => 'email-notifications',
    'payload' => [
        'to' => 'user@example.com',
        'subject' => 'Welcome!',
        'template' => 'welcome',
    ],
    'idempotencyKey' => "welcome-{$userId}",
    'maxRetries' => 5,
]);

echo "Queued job: {$job->id}\n";
```

---

## Worker Patterns

### Node.js Worker

```typescript
import { SpooledWorker } from '@spooled/sdk';

const worker = new SpooledWorker({
  apiKey: process.env.SPOOLED_API_KEY!,
  queueName: 'email-notifications',
  concurrency: 10,
});

worker.on('job', async (job) => {
  const { to, subject, template } = job.payload;
  
  try {
    await sendEmail({ to, subject, template });
    await job.complete();
    console.log(`Sent email to ${to}`);
  } catch (error) {
    await job.fail({ reason: error.message });
  }
});

worker.start();
console.log('Worker started, listening for jobs...');
```

### Python Worker

```python
from spooled import SpooledWorker
import os

worker = SpooledWorker(
    api_key=os.environ["SPOOLED_API_KEY"],
    queue="image-processing"
)

@worker.job_handler
async def process_image(job):
    image_url = job.payload["image_url"]
    operations = job.payload["operations"]
    
    # Process the image
    result = await run_image_pipeline(image_url, operations)
    
    # Store the result
    await job.complete(result={"output_url": result.url})
    
worker.run()
```

### PHP Worker

```php
<?php

use Spooled\SpooledClient;
use Spooled\Config\ClientOptions;
use Spooled\Worker\SpooledWorker;
use Spooled\Worker\WorkerConfig;
use Spooled\Worker\JobContext;

$client = new SpooledClient(new ClientOptions(
    apiKey: getenv('SPOOLED_API_KEY'),
));

$worker = new SpooledWorker($client, new WorkerConfig(
    queueName: 'image-processing',
    concurrency: 10,
));

$worker->process(function (JobContext $ctx): array {
    $imageUrl = $ctx->get('image_url');
    $operations = $ctx->get('operations');
    
    // Process the image
    $result = runImagePipeline($imageUrl, $operations);
    
    return ['output_url' => $result->url];
});

// Graceful shutdown
pcntl_signal(SIGTERM, fn() => $worker->stop());
pcntl_signal(SIGINT, fn() => $worker->stop());

$worker->start();
```

### Simple Polling Worker (cURL/Bash)

```bash
#!/bin/bash
API_KEY="sp_live_..."
BASE_URL="http://localhost:8080"  # or https://api.spooled.cloud

while true; do
  # Claim a job
  JOB=$(curl -s -X POST "$BASE_URL/api/v1/jobs/claim" \
    -H "Authorization: Bearer $API_KEY" \
    -H "Content-Type: application/json" \
    -d '{"queue": "my-queue", "limit": 1}')
  
  JOB_ID=$(echo $JOB | jq -r '.jobs[0].id // empty')
  
  if [ -n "$JOB_ID" ]; then
    echo "Processing job: $JOB_ID"
    
    # Your processing logic here...
    
    # Complete the job
    curl -s -X POST "$BASE_URL/api/v1/jobs/$JOB_ID/complete" \
      -H "Authorization: Bearer $API_KEY"
  else
    sleep 1  # No jobs, wait before polling again
  fi
done
```

---

## Common Operations

### Create a Scheduled Job (Run Later)

```bash
curl -X POST http://localhost:8080/api/v1/jobs \
  -H "Content-Type: application/json" \
  -H "Authorization: Bearer sp_live_..." \
  -d '{
    "queue_name": "reports",
    "payload": {"type": "daily_report"},
    "scheduled_at": "2024-12-10T09:00:00Z"
  }'
```

### Create a Cron Schedule (Recurring)

```bash
curl -X POST http://localhost:8080/api/v1/schedules \
  -H "Content-Type: application/json" \
  -H "Authorization: Bearer sp_live_..." \
  -d '{
    "name": "Daily Report",
    "cron_expression": "0 9 * * *",
    "timezone": "America/New_York",
    "queue_name": "reports",
    "payload_template": {"type": "daily_report"}
  }'
```

### Bulk Enqueue Jobs

```bash
curl -X POST http://localhost:8080/api/v1/jobs/bulk \
  -H "Content-Type: application/json" \
  -H "Authorization: Bearer sp_live_..." \
  -d '{
    "jobs": [
      {"queue_name": "emails", "payload": {"id": 1}},
      {"queue_name": "emails", "payload": {"id": 2}},
      {"queue_name": "emails", "payload": {"id": 3}}
    ]
  }'
```

### Pause a Queue

```bash
curl -X POST http://localhost:8080/api/v1/queues/emails/pause \
  -H "Authorization: Bearer sp_live_..."
```

### Resume a Queue

```bash
curl -X POST http://localhost:8080/api/v1/queues/emails/resume \
  -H "Authorization: Bearer sp_live_..."
```

### Retry a Failed Job

```bash
curl -X POST http://localhost:8080/api/v1/jobs/job_m1n2o3p4/retry \
  -H "Authorization: Bearer sp_live_..."
```

### Cancel a Job

```bash
curl -X POST http://localhost:8080/api/v1/jobs/job_m1n2o3p4/cancel \
  -H "Authorization: Bearer sp_live_..."
```

### List Jobs with Filtering

```bash
# Filter by status
curl "http://localhost:8080/api/v1/jobs?status=pending" \
  -H "Authorization: Bearer sp_live_..."

# Filter by queue
curl "http://localhost:8080/api/v1/jobs?queue_name=emails" \
  -H "Authorization: Bearer sp_live_..."

# Combine filters with pagination
curl "http://localhost:8080/api/v1/jobs?status=failed&queue_name=emails&limit=50" \
  -H "Authorization: Bearer sp_live_..."
```

### Get Queue Statistics

```bash
curl http://localhost:8080/api/v1/queues/emails/stats \
  -H "Authorization: Bearer sp_live_..."
```

Response:
```json
{
  "queue_name": "emails",
  "pending": 1,
  "processing": 0,
  "completed": 0,
  "failed": 0,
  "deadletter": 0
}
```

---

## Real-time Updates

### WebSocket Connection

```javascript
const ws = new WebSocket('ws://localhost:8080/api/v1/ws?token=sp_live_...');

ws.onopen = () => {
  // Subscribe to queue events
  ws.send(JSON.stringify({ type: 'subscribe', queues: ['emails'] }));
};

ws.onmessage = (event) => {
  const data = JSON.parse(event.data);
  console.log('Event:', data.type, data.payload);
  // { type: 'job.completed', payload: { id: 'job_...', status: 'completed' } }
};
```

### Server-Sent Events (SSE)

```bash
# Subscribe to all events for your organization
curl -N http://localhost:8080/api/v1/events \
  -H "Authorization: Bearer sp_live_..."

# Subscribe to specific job updates
curl -N http://localhost:8080/api/v1/events/jobs/job_m1n2o3p4 \
  -H "Authorization: Bearer sp_live_..."

# Subscribe to queue events
curl -N http://localhost:8080/api/v1/events/queues/emails \
  -H "Authorization: Bearer sp_live_..."
```

### Event Types

| Event | Description |
|-------|-------------|
| `job.created` | New job enqueued |
| `job.processing` | Job claimed by worker |
| `job.completed` | Job finished successfully |
| `job.failed` | Job failed (will retry if retries remaining) |
| `job.dead_letter` | Job moved to DLQ after exhausting retries |
| `job.cancelled` | Job was cancelled |
| `queue.paused` | Queue processing paused |
| `queue.resumed` | Queue processing resumed |

---

## Monitoring

### Prometheus Metrics

```bash
curl http://localhost:9090/metrics
```

Key metrics to watch:

| Metric | Description | Alert Threshold |
|--------|-------------|-----------------|
| `spooled_jobs_pending` | Jobs waiting to be processed | > 1000 |
| `spooled_jobs_processing` | Jobs currently in progress | - |
| `spooled_job_max_age_seconds` | Age of oldest pending job | > 300s |
| `spooled_jobs_dead_letter` | Jobs in dead-letter queue | > 0 |
| `spooled_workers_active` | Active workers | < expected |
| `spooled_api_requests_total` | Total API requests | - |
| `spooled_api_request_duration_seconds` | Request latency | P99 > 1s |

### Grafana Dashboard

Open http://localhost:3000 (login: admin/admin)

Pre-built dashboard includes:
- Job throughput (enqueued/completed/failed per minute)
- Queue depth by queue name
- Worker health and activity
- API latency percentiles
- Error rates

---

## Key Concepts

### Idempotency

Use `idempotency_key` to prevent duplicate processing:

```json
{
  "queue": "emails",
  "payload": {"to": "user@example.com"},
  "idempotency_key": "welcome-email-user123"
}
```

If a job with the same key exists, the request returns the existing job instead of creating a duplicate.

### Automatic Retries

Failed jobs retry automatically with exponential backoff:

```
Attempt 1: immediate
Attempt 2: 1 second delay
Attempt 3: 2 seconds delay
Attempt 4: 4 seconds delay
...
```

Configure per job:
```json
{
  "max_retries": 5,
  "backoff_multiplier": 2.0,
  "backoff_max_delay": 3600
}
```

### Dead-Letter Queue (DLQ)

Jobs that exhaust all retries are moved to the DLQ:

```bash
# List DLQ jobs
curl "http://localhost:8080/api/v1/jobs/dlq?queue_name=emails&limit=100" \
  -H "Authorization: Bearer sp_live_..."

# Retry a DLQ job
curl -X POST http://localhost:8080/api/v1/jobs/job_xyz/retry \
  -H "Authorization: Bearer sp_live_..."

# Bulk retry all DLQ jobs for a queue
curl -X POST http://localhost:8080/api/v1/jobs/dlq/retry \
  -H "Authorization: Bearer sp_live_..." \
  -H "Content-Type: application/json" \
  -d '{"queue_name":"emails","limit":100}'
```

### Priority Queues

Higher priority jobs are processed first:

```json
{
  "queue": "emails",
  "payload": {"type": "password_reset"},
  "priority": 10
}
```

Priority scale: 0 (lowest) to 100 (highest). Default is 0.

### Job Leases

Jobs are claimed with a lease (default 5 minutes). If not completed/failed before lease expires, the job is released for another worker.

Workers should:
1. Renew lease for long-running jobs
2. Complete/fail jobs promptly
3. Handle graceful shutdown

---

## Troubleshooting

### "Database Connection Error"

```bash
# Check PostgreSQL is running
docker-compose ps postgres

# Check logs
docker-compose logs postgres

# Verify connection
docker-compose exec postgres psql -U spooled -d spooled -c "SELECT 1"
```

### "Redis Connection Error"

The application will continue to work without Redis (degraded mode - no real-time updates).

```bash
# Check Redis is running
docker-compose ps redis

# Test connection
docker-compose exec redis redis-cli ping
```

### "Migration Failed"

```bash
# Reset database and start fresh
docker-compose down -v
docker-compose up -d postgres pgbouncer redis
sleep 5
cargo run  # Migrations run automatically
```

### "Port Already in Use"

```bash
# Find what's using port 8080
lsof -i :8080

# Kill the process
kill -9 <PID>
```

### "Invalid API Key"

- Verify the key starts with `sp_live_` or `sp_test_`
- Check the key hasn't been revoked
- Ensure you're using the correct organization's key
- Check key hasn't expired (if expiration was set)

### "Rate Limited"

```json
{
  "error": "rate_limit_exceeded",
  "message": "Too many requests",
  "retry_after": 60
}
```

- Check your plan's rate limits
- Implement exponential backoff in clients
- Use bulk operations where possible

### "Job Stuck in Processing"

```bash
# Check for expired leases (will be auto-recovered)
curl "http://localhost:8080/api/v1/jobs?status=processing" \
  -H "Authorization: Bearer sp_live_..."

# Force-fail a stuck job
curl -X POST http://localhost:8080/api/v1/jobs/job_xyz/fail \
  -H "Authorization: Bearer sp_live_..." \
  -d '{"reason": "Manual intervention - stuck job"}'
```

---

## Next Steps

1. **Read the full API documentation**: [API Usage Guide](api-usage.md)
2. **Try gRPC for high-throughput workers**: [gRPC Server Guide](grpc-server.md)
3. **Set up custom webhooks**: [Webhooks Guide](webhooks.md)
4. **Deploy to production**: [Deployment Guide](deployment.md)
5. **Understand the architecture**: [Architecture Guide](architecture.md)
6. **Run load tests**: See `loadtest/README.md`

---

## Need Help?

- GitHub Issues: https://github.com/spooled-cloud/spooled-backend/issues
- Documentation: https://spooled.cloud/docs/
- Email: support@spooled.cloud
