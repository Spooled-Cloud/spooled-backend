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
| Setup time | 2 minutes | 2 minutes to try (`docker-compose.quickstart.yml`), longer for production |
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
# Claim up to 5 jobs -> {"jobs":[{"id":"...","lease_id":"...","lease_expires_at":"...","payload":{...},...}]}
CLAIM=$(curl -s -X POST https://api.spooled.cloud/api/v1/jobs/claim \
  -H "Authorization: Bearer sp_live_YOUR_API_KEY" \
  -H "Content-Type: application/json" \
  -d '{"queue_name": "my-queue", "worker_id": "worker-1", "limit": 5}')
JOB_ID=$(echo "$CLAIM" | jq -r '.jobs[0].id')
LEASE_ID=$(echo "$CLAIM" | jq -r '.jobs[0].lease_id')

# Complete the job (lease_id from the claim is required) -> {"success":true}
curl -s -X POST "https://api.spooled.cloud/api/v1/jobs/$JOB_ID/complete" \
  -H "Authorization: Bearer sp_live_YOUR_API_KEY" \
  -H "Content-Type: application/json" \
  -d "{\"worker_id\": \"worker-1\", \"lease_id\": \"$LEASE_ID\"}"

# Or fail it (will retry)
curl -s -X POST "https://api.spooled.cloud/api/v1/jobs/$JOB_ID/fail" \
  -H "Authorization: Bearer sp_live_YOUR_API_KEY" \
  -H "Content-Type: application/json" \
  -d "{\"worker_id\": \"worker-1\", \"lease_id\": \"$LEASE_ID\", \"error\": \"Connection timeout\"}"
```

A claimed job must be completed or failed before its lease expires (`lease_expires_at`, 30 seconds unless the claim sends `lease_duration_secs`); otherwise it goes back to the queue for another worker.

---

## Self-Hosted Quickstart

### Prerequisites

- Docker with Compose v2
- `curl` and `jq`
- Rust 1.96+ only for option B (building from source)

### Step 1: Start Spooled

**Option A - published images (no build, about 2 minutes).** From a clone of this repository, or after downloading `docker-compose.quickstart.yml`:

```bash
docker compose -f docker-compose.quickstart.yml up -d --wait
```

This starts the backend (REST on `localhost:8080`, gRPC on `localhost:50051` without TLS), the dashboard (`localhost:4321`), PostgreSQL and Redis. Ports already in use? Set `SPOOLED_HTTP_PORT`, `SPOOLED_GRPC_PORT` or `SPOOLED_DASHBOARD_PORT`, for example `SPOOLED_HTTP_PORT=18080 docker compose -f docker-compose.quickstart.yml up -d --wait`.

**Option B - build from source (local development).** The same steps as README "Local Development":

```bash
cp .env.example .env                                     # local settings, URLs match docker-compose.yml
docker compose up -d --wait postgres redis pgbouncer     # Postgres :5433, PgBouncer :6432, Redis :6379
cargo run                                                # migrations run automatically on first start
```

`cargo run` logs, among other lines, `Server listening addr=0.0.0.0:8080`, the gRPC server on `0.0.0.0:50051` and `Metrics server listening metrics_addr=0.0.0.0:9090`.

**Ports:**
- `:8080` - REST API (HTTP/1.1)
- `:50051` - gRPC API (HTTP/2 + Protobuf) (self-hosted)
- `:9090` - Prometheus metrics (option B; not published by the Compose stacks)

**Spooled Cloud gRPC endpoint:**
- `grpc.spooled.cloud:443` (TLS)

### Step 2: Verify Installation

```bash
curl http://localhost:8080/health
```

Expected response:
```json
{"status":"healthy","database":true,"cache":true,"grpc":true,"scheduler":true}
```

The public health endpoint intentionally omits build version information. Operators should use authenticated `GET /api/v1/dashboard` and inspect `system.version` plus `system.uptime_seconds` when verifying a rollout.

### Step 3: Create Your Organization

Every tenant in Spooled has an organization. When you create one, you get an initial API key:

```bash
RESPONSE=$(curl -s -X POST http://localhost:8080/api/v1/organizations \
  -H "Content-Type: application/json" \
  -d '{
    "name": "My Company",
    "slug": "my-company"
  }')
echo "$RESPONSE"
API_KEY=$(echo "$RESPONSE" | jq -r '.api_key.key')
```

Response:
```json
{
  "organization": {
    "id": "e116712a-d5e3-491d-ad7b-b8460a5eb370",
    "name": "My Company",
    "slug": "my-company",
    "plan_tier": "free",
    "billing_email": null,
    "settings": {"webhook_token": "whk_..."},
    "custom_limits": null,
    "created_at": "2026-11-16T17:05:43.704066Z",
    "updated_at": "2026-11-16T17:05:43.704066Z",
    ...
  },
  "api_key": {
    "id": "6e6749b3-a6c5-4270-9976-1eae8dcd83a0",
    "key": "sp_test_vMWwzKDw7US3zK1s26S7Fc...",
    "name": "Initial Admin Key",
    "created_at": "2026-11-16T17:05:43.704066Z"
  }
}
```

⚠️ **IMPORTANT**: Save `api_key.key` immediately! It's only shown once and cannot be retrieved later. Keys start with `sp_test_` in development and `sp_live_` when the server runs with `RUST_ENV=production`.

The organization starts on the server's `DEFAULT_PLAN_TIER` (default `free`: 2 queues, 1 worker, no workflows, 5 requests/second; the quickstart Compose file sets `enterprise`, which backend images up to 0.1.130 ignore). To lift the limits, see [Lifting plan limits](../../README.md#lifting-plan-limits-self-hosted).

### Step 4: Create, Claim and Complete Your First Job

```bash
curl -s -X POST http://localhost:8080/api/v1/jobs \
  -H "Content-Type: application/json" \
  -H "Authorization: Bearer $API_KEY" \
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

Response (`created` is `false` when an `idempotency_key` matched an existing job):
```json
{"id": "a2a10d19-aa88-4b43-ab8a-564938edee8f", "created": true}
```

Process it the way a worker does:

```bash
CLAIM=$(curl -s -X POST http://localhost:8080/api/v1/jobs/claim \
  -H "Content-Type: application/json" \
  -H "Authorization: Bearer $API_KEY" \
  -d '{"queue_name": "emails", "worker_id": "worker-1", "limit": 1}')
JOB_ID=$(echo "$CLAIM" | jq -r '.jobs[0].id')
LEASE_ID=$(echo "$CLAIM" | jq -r '.jobs[0].lease_id')

curl -s -X POST "http://localhost:8080/api/v1/jobs/$JOB_ID/complete" \
  -H "Content-Type: application/json" \
  -H "Authorization: Bearer $API_KEY" \
  -d "{\"worker_id\": \"worker-1\", \"lease_id\": \"$LEASE_ID\", \"result\": {\"sent\": true}}"

curl -s "http://localhost:8080/api/v1/jobs/$JOB_ID" -H "Authorization: Bearer $API_KEY" | jq '{status, result}'
# {"status": "completed", "result": {"sent": true}}
```

With option A, open the dashboard at http://localhost:4321 and sign in with the API key.

---

## SDK Examples

All four SDKs talk REST by default (gRPC is opt-in) and accept a self-hosted base URL such as `http://localhost:8080`; they add `/api/v1` themselves. Leave the base URL out to use Spooled Cloud. Each example reads the API key from `SPOOLED_API_KEY`. More in [SDKs](./sdks.md).

### Node.js

Node.js 18+. Save as `create-job.mjs` (top-level `await` needs ES modules).

```bash
npm install @spooled/sdk
```

```javascript
import { SpooledClient } from '@spooled/sdk';

const client = new SpooledClient({
  apiKey: process.env.SPOOLED_API_KEY,
  baseUrl: 'http://localhost:8080', // omit for Spooled Cloud
});

const { id, created } = await client.jobs.create({
  queueName: 'emails',
  payload: { to: 'user@example.com', subject: 'Welcome!' },
  maxRetries: 5,
  idempotencyKey: 'welcome-user-123',
});
console.log(`job ${id} (new: ${created})`);
```

### Python

Python 3.10+.

```bash
pip install spooled
```

```python
import os
from spooled import SpooledClient

client = SpooledClient(
    api_key=os.environ["SPOOLED_API_KEY"],
    base_url="http://localhost:8080",  # omit for Spooled Cloud
)
result = client.jobs.create({
    "queue_name": "emails",
    "payload": {"to": "user@example.com", "subject": "Welcome!"},
    "max_retries": 5,
    "idempotency_key": "welcome-user-123",
})
print(f"job {result.id} (new: {result.created})")
client.close()
```

### Go

```bash
go get github.com/spooled-cloud/spooled-sdk-go@latest
```

```go
package main

import (
	"context"
	"log"
	"os"

	"github.com/spooled-cloud/spooled-sdk-go/spooled"
	"github.com/spooled-cloud/spooled-sdk-go/spooled/resources"
	"github.com/spooled-cloud/spooled-sdk-go/spooled/types"
)

func main() {
	client, err := spooled.NewClient(
		spooled.WithAPIKey(os.Getenv("SPOOLED_API_KEY")),
		spooled.WithBaseURL("http://localhost:8080"), // omit for Spooled Cloud
	)
	if err != nil {
		log.Fatal(err)
	}
	defer client.Close()

	res, err := client.Jobs().Create(context.Background(), &resources.CreateJobRequest{
		QueueName:      "emails",
		Payload:        map[string]any{"to": "user@example.com", "subject": "Welcome!"},
		MaxRetries:     types.Int(5),
		IdempotencyKey: types.String("welcome-user-123"),
	})
	if err != nil {
		log.Fatal(err)
	}
	log.Printf("job %s (new: %v)", res.ID, res.Created)
}
```

### PHP

PHP 8.2+.

```bash
composer require spooled-cloud/spooled
```

```php
<?php
require __DIR__ . '/vendor/autoload.php';

use Spooled\SpooledClient;
use Spooled\Config\ClientOptions;

$client = new SpooledClient(new ClientOptions(
    apiKey: getenv('SPOOLED_API_KEY') ?: null,
    baseUrl: 'http://localhost:8080', // omit for Spooled Cloud
));

$result = $client->jobs->create([
    'queueName' => 'emails',
    'payload' => ['to' => 'user@example.com', 'subject' => 'Welcome!'],
    'maxRetries' => 5,
    'idempotencyKey' => 'welcome-user-123',
]);
echo "job {$result->id}\n";
```

---

## Worker Patterns

SDK workers claim jobs, keep their leases alive, and complete or fail each job from your handler's result: return to complete, throw (or return an error) to fail. They also register a worker with the server, and the Free plan allows one worker per organization.

### Node.js Worker

```javascript
// worker.mjs
import { SpooledClient, SpooledWorker } from '@spooled/sdk';

async function sendEmail(to, subject) { /* your code */ }

const client = new SpooledClient({ apiKey: process.env.SPOOLED_API_KEY, baseUrl: 'http://localhost:8080' });
const worker = new SpooledWorker(client, { queueName: 'emails', concurrency: 5, workerId: 'emails-worker-1' });

worker.process(async (ctx) => {
  const { to, subject } = ctx.payload;
  await sendEmail(to, subject); // throw = fail (retried while retries remain)
  return { sent: true };        // return = complete; value stored as the job result
});

await worker.start();
for (const sig of ['SIGINT', 'SIGTERM']) {
  process.on(sig, () => void worker.stop().then(() => process.exit(0)));
}
```

### Python Worker

```python
import os, signal
from spooled import SpooledClient
from spooled.worker import SpooledWorker

def send_email(to: str, subject: str) -> None:
    ...  # your code

client = SpooledClient(api_key=os.environ["SPOOLED_API_KEY"], base_url="http://localhost:8080")
worker = SpooledWorker(client, queue_name="emails", concurrency=5, worker_id="emails-worker-1")

@worker.process
def handle(ctx):
    send_email(ctx.payload["to"], ctx.payload["subject"])  # raise = fail (retried)
    return {"sent": True}                                  # return = complete

signal.signal(signal.SIGINT, lambda *_: worker.stop())
signal.signal(signal.SIGTERM, lambda *_: worker.stop())
worker.start()  # blocks until stop() has finished running jobs and deregistered
client.close()
```

The handler is a plain function; for `async` code use `AsyncSpooledClient` with `AsyncSpooledWorker` and `await worker.start()`.

### Go Worker

```go
package main

import (
	"context"
	"log"
	"os"
	"os/signal"
	"syscall"

	"github.com/spooled-cloud/spooled-sdk-go/spooled"
	"github.com/spooled-cloud/spooled-sdk-go/spooled/worker"
)

func sendEmail(to, subject string) error { return nil } // your code

func main() {
	client, err := spooled.NewClient(spooled.WithAPIKey(os.Getenv("SPOOLED_API_KEY")),
		spooled.WithBaseURL("http://localhost:8080"))
	if err != nil {
		log.Fatal(err)
	}
	defer client.Close()

	w := worker.NewWorker(client.Jobs(), client.Workers(),
		worker.Options{QueueName: "emails", Concurrency: 5, WorkerID: "emails-worker-1"})
	w.Process(func(ctx *worker.JobContext) (map[string]any, error) {
		to, _ := ctx.Payload["to"].(string)
		subject, _ := ctx.Payload["subject"].(string)
		if err := sendEmail(to, subject); err != nil {
			return nil, err // error = fail (retried while retries remain)
		}
		return map[string]any{"sent": true}, nil // nil error = complete
	})

	ctx, stop := signal.NotifyContext(context.Background(), syscall.SIGINT, syscall.SIGTERM)
	defer stop()
	if err := w.Start(ctx); err != nil { // returns once registered; polls in background
		log.Fatal(err)
	}
	<-ctx.Done()
	_ = w.Stop() // lets running jobs finish, then deregisters
}
```

### PHP Worker

Needs the `pcntl` and `posix` extensions (CLI on Linux or macOS). It runs one job at a time per process; run several processes for parallel work.

```php
<?php
require __DIR__ . '/vendor/autoload.php';

use Spooled\SpooledClient;
use Spooled\Config\ClientOptions;
use Spooled\Worker\{SpooledWorker, WorkerConfig, JobContext};

function sendEmail(string $to, string $subject): void { /* your code */ }

$client = new SpooledClient(new ClientOptions(apiKey: getenv('SPOOLED_API_KEY') ?: null, baseUrl: 'http://localhost:8080'));
$worker = new SpooledWorker($client, new WorkerConfig(queueName: 'emails', workerId: 'emails-worker-1'));

$worker->process(function (JobContext $ctx): array {
    sendEmail($ctx->get('to'), $ctx->get('subject')); // throw = fail (retried)
    return ['sent' => true];                          // return = complete
});

pcntl_async_signals(true);
pcntl_signal(SIGTERM, fn () => $worker->stop());
pcntl_signal(SIGINT, fn () => $worker->stop());
$worker->start(); // blocks until stop()
```

### Simple Polling Worker (cURL/Bash)

Needs `curl` and `jq`. Run with `SPOOLED_API_KEY=sp_... ./worker.sh`.

```bash
#!/usr/bin/env bash
API_KEY="${SPOOLED_API_KEY:?set SPOOLED_API_KEY}"
BASE_URL="${SPOOLED_URL:-http://localhost:8080}"  # or https://api.spooled.cloud
WORKER_ID="bash-worker-$$"

while true; do
  # Claim a job -> {"jobs":[{"id":"...","lease_id":"...","payload":{...},...}]}
  CLAIM=$(curl -s -X POST "$BASE_URL/api/v1/jobs/claim" \
    -H "Authorization: Bearer $API_KEY" \
    -H "Content-Type: application/json" \
    -d "{\"queue_name\": \"my-queue\", \"worker_id\": \"$WORKER_ID\", \"limit\": 1}")

  JOB_ID=$(echo "$CLAIM" | jq -r '.jobs[0].id // empty')

  if [ -n "$JOB_ID" ]; then
    LEASE_ID=$(echo "$CLAIM" | jq -r '.jobs[0].lease_id')
    echo "Processing job: $JOB_ID $(echo "$CLAIM" | jq -c '.jobs[0].payload')"

    # Your processing logic here (finish before the 30 s lease expires)...

    # Complete the job
    curl -s -X POST "$BASE_URL/api/v1/jobs/$JOB_ID/complete" \
      -H "Authorization: Bearer $API_KEY" \
      -H "Content-Type: application/json" \
      -d "{\"worker_id\": \"$WORKER_ID\", \"lease_id\": \"$LEASE_ID\"}"
    echo
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

One request enqueues up to 100 jobs into one queue (`queue_name` is set once, at the top level):

```bash
curl -X POST http://localhost:8080/api/v1/jobs/bulk \
  -H "Content-Type: application/json" \
  -H "Authorization: Bearer sp_live_..." \
  -d '{
    "queue_name": "emails",
    "jobs": [
      {"payload": {"id": 1}},
      {"payload": {"id": 2}, "priority": 5},
      {"payload": {"id": 3}, "idempotency_key": "email-3"}
    ]
  }'
```

Response:
```json
{
  "succeeded": [
    {"index": 0, "job_id": "888cfba6-dfbb-499f-b1de-7aa02a151863", "created": true},
    {"index": 1, "job_id": "1a60abe5-d5b7-4bfe-b966-98e0ad30041c", "created": true},
    {"index": 2, "job_id": "0b3f6a0e-5f1e-4a52-9c39-1f0e7f3c2d11", "created": true}
  ],
  "failed": [],
  "total": 3,
  "success_count": 3,
  "failure_count": 0
}
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
curl -X DELETE http://localhost:8080/api/v1/jobs/job_m1n2o3p4 \
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
  "pending_jobs": 1,
  "processing_jobs": 0,
  "completed_jobs_24h": 1,
  "failed_jobs_24h": 1,
  "deadletter_jobs": 1,
  "avg_processing_time_ms": 117.7,
  "max_job_age_seconds": null,
  "active_workers": 0
}
```

Counts for all queues together: `GET /api/v1/jobs/stats` (`pending`, `scheduled`, `processing`, `completed`, `failed`, `deadletter`, `cancelled`, `total`).

---

## Real-time Updates

### WebSocket Connection

WebSocket auth uses a JWT in the `?token=` query parameter (headers aren't
available on the WebSocket handshake). Exchange your API key for a JWT via
`POST /api/v1/auth/login` first, then pass the returned token:

```javascript
// token is a JWT from POST /api/v1/auth/login (starts with "eyJ..."), not an API key
const ws = new WebSocket('ws://localhost:8080/api/v1/ws?token=eyJ...');

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

The server closes the socket with code `1008` when the token expires or the session is logged out
(log in again, then reconnect), and with `1000` after 1 hour or `1011` when its event channel was
lost (reconnect). See the [API usage guide](./api-usage.md#real-time-events).

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
| `job.started` | Job claimed by worker |
| `job.completed` | Job finished successfully |
| `job.failed` | Job failed; `data.status` may be `failed` or `deadletter` |
| `job.cancelled` | Job was cancelled |
| `queue.paused` | Queue processing paused |
| `queue.resumed` | Queue processing resumed |

---

## Monitoring

### Prometheus Metrics

With `cargo run` (option B) the metrics port is `9090` on the host. The Compose stacks do not publish it; read it inside the container instead, e.g. `docker compose -f docker-compose.quickstart.yml exec backend curl -s http://localhost:9090/metrics`.

```bash
curl http://localhost:9090/metrics
```

Key metrics to watch:

| Metric | Description | Alert Threshold |
|--------|-------------|-----------------|
| `spooled_jobs_pending` | Jobs waiting to be processed | > 1000 |
| `spooled_jobs_processing` | Jobs currently in progress | - |
| `spooled_job_max_age_seconds` | Age of oldest pending job | > 300s |
| `spooled_jobs_deadlettered_total` | Jobs moved to the dead-letter queue (counter) | `increase(...[5m]) > 0` |
| `spooled_workers_active` | Active workers | < expected |
| `spooled_api_requests_total` | Total API requests | - |
| `spooled_api_request_duration_seconds` | Request latency | P99 > 1s |

### Grafana Dashboard

With the local development stack (`docker compose up -d` using `docker-compose.yml`), open http://localhost:3000 (login: admin/admin). The quickstart and production stacks do not publish Grafana.

The provisioned "Spooled Backend Overview" dashboard includes:
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
  "queue_name": "emails",
  "payload": {"to": "user@example.com"},
  "idempotency_key": "welcome-email-user123"
}
```

If a job with the same key exists, the request returns that job's `id` with `"created": false` (HTTP 200) instead of creating a duplicate.

### Automatic Retries

Failed jobs retry automatically with exponential backoff: `2^n` seconds (n = retries already used),
capped at 60 seconds, plus up to 0.5 s of jitter. The curve is fixed and cannot be configured.

```
1st failure: retried after ~1 second
2nd failure: retried after ~2 seconds
3rd failure: retried after ~4 seconds
...
7th failure and later: retried after ~60 seconds (cap)
```

Configure the number of retries per job (or as a queue default):
```json
{
  "max_retries": 5
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
  "queue_name": "emails",
  "payload": {"type": "password_reset"},
  "priority": 10
}
```

Priority scale: -100 (lowest) to 100 (highest). Default is 0.

### Job Leases

Jobs are claimed with a lease: 30 seconds for a REST claim that sends no `lease_duration_secs` (server setting `WORKER_LEASE_DURATION_SECS`), 300 seconds for gRPC (`GRPC_DEFAULT_LEASE_DURATION_SECS`). Complete or fail the job with the claim's `lease_id` before the lease expires, or extend it with `POST /api/v1/jobs/{id}/heartbeat`; otherwise the job is released for another worker.

Workers should:
1. Renew lease for long-running jobs
2. Complete/fail jobs promptly
3. Handle graceful shutdown

---

## Troubleshooting

### "Database Connection Error"

```bash
# Check PostgreSQL is running
docker compose ps postgres

# Check logs
docker compose logs postgres

# Verify connection
docker compose exec postgres psql -U spooled -d spooled -c "SELECT 1"
```

### "Redis Connection Error"

The application will continue to work without Redis (degraded mode - no real-time updates).

```bash
# Check Redis is running
docker compose ps redis

# Test connection
docker compose exec redis redis-cli ping
```

### "Migration Failed"

```bash
# Reset database and start fresh
docker compose down -v
docker compose up -d --wait postgres pgbouncer redis
cargo run  # Migrations run automatically
```

### "Port Already in Use"

```bash
# Find what's using port 8080
lsof -i :8080
```

Stop that process, or move Spooled to another host port: `SPOOLED_HTTP_PORT` (and `SPOOLED_GRPC_PORT`, `SPOOLED_DASHBOARD_PORT`) for the quickstart and production Compose files; `POSTGRES_HOST_PORT`, `PGBOUNCER_HOST_PORT`, `REDIS_HOST_PORT` for `docker-compose.yml` (then update the URLs in `.env`); `PORT` in `.env` for `cargo run`.

### "Invalid API Key"

- Verify the key starts with `sp_live_` or `sp_test_` (new keys) or `sk_live_`/`sk_test_` (legacy keys)
- Check the key hasn't been revoked
- Ensure you're using the correct organization's key
- Check key hasn't expired (if expiration was set)

### "Rate Limited"

Per-second rate limiting returns HTTP `429 Too Many Requests`:

```json
{
  "error": "Rate limit exceeded",
  "code": "RATE_LIMIT_EXCEEDED",
  "retry_after_secs": 60,
  "limit": 100,
  "remaining": 0,
  "reset_at": 1704067200
}
```

- The response carries a `Retry-After` header (seconds); the same wait is in `retry_after_secs`. Authenticated requests also get `X-RateLimit-Limit`, `X-RateLimit-Remaining`, `X-RateLimit-Reset` (a Unix time, equal to `reset_at`) and `X-RateLimit-Scope`
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
  -H "Content-Type: application/json" \
  -d '{"worker_id": "worker-1", "lease_id": "lease_from_claim", "error": "Manual intervention - stuck job"}'
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
