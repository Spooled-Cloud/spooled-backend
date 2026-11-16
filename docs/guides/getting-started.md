# Getting Started with Spooled

> A beginner-friendly guide for developers who have used Laravel Queues or similar systems.

## What is Spooled?

**Spooled** is a job queue system - just like Laravel Queues, but as a standalone service. Instead of being tied to one framework, Spooled works with any language or platform via HTTP/gRPC API.

### Laravel Queues vs Spooled

| Laravel Queues | Spooled |
|----------------|---------|
| Built into Laravel | Standalone service (works with anything) |
| `dispatch(new SendEmail($user))` | HTTP POST to `/api/v1/jobs` or gRPC `Enqueue` |
| `php artisan queue:work` | Workers connect via REST or gRPC |
| Redis/Database driver | PostgreSQL + Redis |
| One app only | Multiple apps, any language |

**Think of Spooled as "Laravel Queues as a Service"** - you get all the familiar concepts (jobs, queues, workers, retries, delays) but accessible from anywhere.

### API Options

Spooled offers two API protocols:

| Protocol | Port | Best For |
|----------|------|----------|
| **REST** | 8080 | Web apps, simple integrations, admin dashboards |
| **gRPC** | 50051 (self-hosted) / 443 (Spooled Cloud via `grpc.spooled.cloud`) | High-throughput workers, streaming, type-safe SDKs |

This guide uses REST examples. See [API Usage](./api-usage.md#grpc-api) for gRPC documentation.

---

## Quick Start (5 Minutes)

The examples use two shell variables, so they work the same against your own server and Spooled Cloud:

```bash
# Self-hosted: start Spooled locally first (README "Try it in 2 minutes"):
#   docker compose -f docker-compose.quickstart.yml up -d --wait
export SPOOLED_URL=http://localhost:8080
# Spooled Cloud instead: export SPOOLED_URL=https://api.spooled.cloud
```

Every request with a body needs `Content-Type: application/json`; without it the API answers `400 INVALID_JSON`.

### Step 1: Create an Organization

An organization is like a "tenant" - it groups your queues, jobs, and API keys. On your own server, create one with the API (on Spooled Cloud, sign up at [spooled.cloud](https://spooled.cloud/signup) and create an API key in the dashboard instead, then skip to Step 3):

```bash
curl -s -X POST "$SPOOLED_URL/api/v1/organizations" \
  -H "Content-Type: application/json" \
  -d '{
    "name": "My Company",
    "slug": "my-company"
  }'
```

**Response:**
```json
{
  "organization": {
    "id": "5917d88d-5b3b-41a1-b598-89a444585845",
    "name": "My Company",
    "slug": "my-company",
    "plan_tier": "free",
    "created_at": "2026-11-16T17:05:43.704066Z",
    ...
  },
  "api_key": {
    "id": "6e6749b3-a6c5-4270-9976-1eae8dcd83a0",
    "key": "sp_live_xxxxxxxxxxxxxxxxxxxx",
    "name": "Initial Admin Key",
    "created_at": "2026-11-16T17:05:43.704066Z"
  }
}
```

> ⚠️ **Save `api_key.key` now** - it is only shown once (like Stripe API keys). Keys start with `sp_live_` when the server runs with `RUST_ENV=production` and `sp_test_` otherwise.

```bash
export SPOOLED_API_KEY=sp_live_xxxxxxxxxxxxxxxxxxxx   # the api_key.key value
```

Or create the organization and keep the key in one go:

```bash
export SPOOLED_API_KEY=$(curl -s -X POST "$SPOOLED_URL/api/v1/organizations" \
  -H "Content-Type: application/json" \
  -d '{"name": "My Company", "slug": "my-company"}' | jq -r '.api_key.key')
```

> A self-hosted organization starts on the server's `DEFAULT_PLAN_TIER` (default `free`: 2 queues, 1 worker, no workflows, 5 requests/second). See [Lifting plan limits](../../README.md#lifting-plan-limits-self-hosted).

---

### Step 2: Create More API Keys (Optional)

The organization comes with one key. To add a key, for example one limited to some queues:

```bash
curl -s -X POST "$SPOOLED_URL/api/v1/api-keys" \
  -H "Authorization: Bearer $SPOOLED_API_KEY" \
  -H "Content-Type: application/json" \
  -d '{
    "name": "Production App",
    "queues": ["emails", "notifications"]
  }'
```

**Response:**
```json
{
  "id": "09529172-0cdd-4dc0-85c3-b0d4db92f480",
  "key": "sp_live_xxxxxxxxxxxxxxxxxxxx",
  "name": "Production App",
  "queues": ["emails", "notifications"],
  "created_at": "2026-11-16T17:23:23.219675Z",
  "expires_at": null
}
```

> ⚠️ **Save the `key` immediately!** It's only shown once.

---

### Step 3: Configure a Queue (Optional)

Queues are created implicitly when you enqueue your first job. You can optionally configure queue settings:

```bash
curl -s -X PUT "$SPOOLED_URL/api/v1/queues/emails/config" \
  -H "Authorization: Bearer $SPOOLED_API_KEY" \
  -H "Content-Type: application/json" \
  -d '{
    "max_retries": 3,
    "default_timeout": 300,
    "rate_limit": 100
  }'
```

`rate_limit` is stored and returned but not enforced: workers are not throttled by it.

**Response:**
```json
{
  "id": "01dfa461-5db4-41fe-8b59-75f8945d5112",
  "organization_id": "5917d88d-5b3b-41a1-b598-89a444585845",
  "queue_name": "emails",
  "max_retries": 3,
  "default_timeout": 300,
  "rate_limit": 100,
  "enabled": true,
  "settings": {},
  "created_at": "2026-11-16T17:23:23.160559Z",
  "updated_at": "2026-11-16T17:23:23.160559Z"
}
```

---

### Step 4: Push a Job (Like Laravel's `dispatch()`)

This is equivalent to Laravel's `dispatch(new SendEmail($user))`:

```bash
curl -s -X POST "$SPOOLED_URL/api/v1/jobs" \
  -H "Authorization: Bearer $SPOOLED_API_KEY" \
  -H "Content-Type: application/json" \
  -d '{
    "queue_name": "emails",
    "payload": {
      "type": "send_welcome_email",
      "user_id": 123,
      "email": "user@example.com",
      "template": "welcome"
    }
  }'
```

**Response:**
```json
{
  "id": "a2a10d19-aa88-4b43-ab8a-564938edee8f",
  "created": true
}
```

`created` is `false` when an `idempotency_key` matched an existing job (the `id` is then that job's). Fetch the full job with `GET /api/v1/jobs/{id}`.

---

### Step 5: Process Jobs (Like `php artisan queue:work`)

A worker claims jobs, does the work, and reports the result. A claim gives each job a lease: `lease_id` must be sent back with `complete` or `fail`, before `lease_expires_at` (30 seconds by default; send `lease_duration_secs`, 5-3600, to change it, or extend it with `POST /api/v1/jobs/{id}/heartbeat`).

```bash
# 1. Claim jobs (get the next job(s) to process)
CLAIM=$(curl -s -X POST "$SPOOLED_URL/api/v1/jobs/claim" \
  -H "Authorization: Bearer $SPOOLED_API_KEY" \
  -H "Content-Type: application/json" \
  -d '{
    "queue_name": "emails",
    "worker_id": "worker-1",
    "limit": 10
  }')
echo "$CLAIM"
```

**Response:**
```json
{
  "jobs": [
    {
      "id": "a2a10d19-aa88-4b43-ab8a-564938edee8f",
      "queue_name": "emails",
      "payload": {
        "type": "send_welcome_email",
        "user_id": 123,
        "email": "user@example.com",
        "template": "welcome"
      },
      "retry_count": 0,
      "max_retries": 3,
      "timeout_seconds": 300,
      "lease_expires_at": "2026-11-16T17:06:25.082319Z",
      "lease_id": "8cbe9c5a-c9c6-4481-9188-dce50dc4b41c-a2a10d19-aa88-4b43-ab8a-564938edee8f"
    }
  ]
}
```

An empty queue answers `{"jobs": []}`.

```bash
JOB_ID=$(echo "$CLAIM" | jq -r '.jobs[0].id')
LEASE_ID=$(echo "$CLAIM" | jq -r '.jobs[0].lease_id')

# 2. Do your work (send the email, process data, etc.)
# ... your code here ...

# 3. Mark the job as completed -> {"success":true}
curl -s -X POST "$SPOOLED_URL/api/v1/jobs/$JOB_ID/complete" \
  -H "Authorization: Bearer $SPOOLED_API_KEY" \
  -H "Content-Type: application/json" \
  -d "{
    \"worker_id\": \"worker-1\",
    \"lease_id\": \"$LEASE_ID\",
    \"result\": {\"email_sent\": true, \"message_id\": \"msg_123\"}
  }"

# Or, if the work failed, mark it failed (retried until max_retries, then dead-lettered):
# curl -s -X POST "$SPOOLED_URL/api/v1/jobs/$JOB_ID/fail" \
#   -H "Authorization: Bearer $SPOOLED_API_KEY" \
#   -H "Content-Type: application/json" \
#   -d "{\"worker_id\": \"worker-1\", \"lease_id\": \"$LEASE_ID\", \"error\": \"SMTP timeout\"}"
```

---

## Code Examples

Prefer an official SDK (Node.js, Python, Go, PHP) for real projects: they handle leases, heartbeats and retries for you. See [SDKs](./sdks.md) and the [Quick Start Guide](./quickstart.md#sdk-examples). The examples below call the REST API directly, so you can see exactly what goes over the wire. They read `SPOOLED_URL` and `SPOOLED_API_KEY` from the environment.

### PHP (Laravel App)

```php
<?php

namespace App\Services;

use Illuminate\Support\Facades\Http;

class SpooledQueue
{
    private string $apiKey;
    private string $baseUrl;

    public function __construct()
    {
        $this->apiKey = config('services.spooled.api_key');
        $this->baseUrl = config('services.spooled.url', 'http://localhost:8080');
    }

    /**
     * Push a job to the queue (like dispatch()). Returns ['id' => ..., 'created' => true].
     */
    public function dispatch(string $queue, array $payload, array $options = []): array
    {
        $job = [
            'queue_name' => $queue,
            'payload' => $payload,
            'priority' => $options['priority'] ?? 0,
        ];
        if (isset($options['delay'])) {
            $job['scheduled_at'] = now()->addSeconds($options['delay'])->toIso8601ZuluString();
        }

        // Http::post() sends JSON with Content-Type: application/json
        return Http::withToken($this->apiKey)
            ->post("{$this->baseUrl}/api/v1/jobs", $job)
            ->throw()
            ->json();
    }

    /**
     * Push a delayed job (like dispatch()->delay())
     */
    public function dispatchLater(string $queue, array $payload, int $delaySeconds): array
    {
        return $this->dispatch($queue, $payload, ['delay' => $delaySeconds]);
    }
}

// Usage in your Laravel app:
$spooled = app(SpooledQueue::class);

// Simple job
$spooled->dispatch('emails', [
    'type' => 'send_welcome_email',
    'user_id' => $user->id,
    'email' => $user->email,
]);

// Delayed job (send in 1 hour)
$spooled->dispatchLater('emails', [
    'type' => 'send_reminder',
    'user_id' => $user->id,
], 3600);
```

### PHP Worker (Standalone Script)

```php
<?php
// worker.php - run as a separate process: SPOOLED_API_KEY=... php worker.php
// Needs PHP 8.1+ with allow_url_fopen (the default). No Composer packages.

$apiKey   = getenv('SPOOLED_API_KEY');
$baseUrl  = getenv('SPOOLED_URL') ?: 'http://localhost:8080';
$workerId = gethostname() . '-' . getmypid();
$queue    = 'emails';

function spooled(string $method, string $path, array $body): array
{
    global $apiKey, $baseUrl;
    $response = file_get_contents($baseUrl . $path, false, stream_context_create([
        'http' => [
            'method' => $method,
            'header' => ["Authorization: Bearer {$apiKey}", 'Content-Type: application/json'],
            'content' => json_encode($body),
            'ignore_errors' => true, // return 4xx/5xx bodies instead of false
        ],
    ]));
    return json_decode($response ?: '{}', true) ?? [];
}

// Your job handlers (replace with real work). Throw to fail the job.
function handle(array $payload): array
{
    switch ($payload['type'] ?? null) {
        case 'send_welcome_email':
            // sendWelcomeEmail($payload['email']);
            return ['email_sent' => true];
        default:
            throw new Exception('Unknown job type: ' . json_encode($payload['type'] ?? null));
    }
}

echo "Worker {$workerId} starting...\n";

while (true) {
    // Claim up to 10 jobs; the response is {"jobs": [...]}
    $claim = spooled('POST', '/api/v1/jobs/claim', [
        'queue_name' => $queue,
        'worker_id' => $workerId,
        'limit' => 10,
    ]);
    $jobs = $claim['jobs'] ?? [];

    if (empty($jobs)) {
        sleep(1); // No jobs available, wait and retry
        continue;
    }

    foreach ($jobs as $job) {
        echo "Processing job {$job['id']}\n";
        try {
            $result = handle($job['payload']);
            spooled('POST', "/api/v1/jobs/{$job['id']}/complete", [
                'worker_id' => $workerId,
                'lease_id' => $job['lease_id'],
                'result' => $result,
            ]);
            echo "✓ Job {$job['id']} completed\n";
        } catch (Throwable $e) {
            spooled('POST', "/api/v1/jobs/{$job['id']}/fail", [
                'worker_id' => $workerId,
                'lease_id' => $job['lease_id'],
                'error' => $e->getMessage(),
            ]);
            echo "✗ Job {$job['id']} failed: {$e->getMessage()}\n";
        }
    }
}
```

### Node.js

Node.js 18+ (built-in `fetch`), no dependencies:

```javascript
// spooled.js - a tiny REST client using the built-in fetch (Node 18+), no dependencies
class Spooled {
  constructor(apiKey, baseUrl = process.env.SPOOLED_URL || 'http://localhost:8080') {
    this.apiKey = apiKey;
    this.baseUrl = baseUrl;
  }

  async request(path, body) {
    const res = await fetch(`${this.baseUrl}${path}`, {
      method: 'POST',
      headers: {
        Authorization: `Bearer ${this.apiKey}`,
        'Content-Type': 'application/json',
      },
      body: JSON.stringify(body),
    });
    const data = await res.json();
    if (!res.ok) throw new Error(`${res.status} ${data.code}: ${data.message}`);
    return data;
  }

  // Push a job (like Laravel's dispatch). Returns {id, created}.
  dispatch(queue_name, payload, options = {}) {
    return this.request('/api/v1/jobs', {
      queue_name,
      payload,
      scheduled_at: options.delay ? new Date(Date.now() + options.delay * 1000).toISOString() : undefined,
      priority: options.priority,
    });
  }

  // Claim jobs for processing. Returns an array of {id, payload, lease_id, ...}.
  async claim(queue_name, workerId, limit = 10) {
    const { jobs } = await this.request('/api/v1/jobs/claim', { queue_name, worker_id: workerId, limit });
    return jobs;
  }

  // Mark a claimed job as completed
  complete(job, workerId, result = {}) {
    return this.request(`/api/v1/jobs/${job.id}/complete`, { worker_id: workerId, lease_id: job.lease_id, result });
  }

  // Mark a claimed job as failed (retried until max_retries, then dead-lettered)
  fail(job, workerId, error) {
    return this.request(`/api/v1/jobs/${job.id}/fail`, { worker_id: workerId, lease_id: job.lease_id, error });
  }
}

module.exports = Spooled;
```

```javascript
// dispatch.js - node dispatch.js
const Spooled = require('./spooled');

const spooled = new Spooled(process.env.SPOOLED_API_KEY);

async function main() {
  // Dispatch a job
  const job = await spooled.dispatch('emails', {
    type: 'send_welcome_email',
    userId: 123,
    email: 'user@example.com',
  });
  console.log('Queued', job.id);

  // Dispatch with delay (5 minutes)
  await spooled.dispatch('emails', { type: 'send_reminder', userId: 123 }, { delay: 300 });
}

main();
```

### Node.js Worker

```javascript
// worker.js - node worker.js
const Spooled = require('./spooled');

const spooled = new Spooled(process.env.SPOOLED_API_KEY);
const workerId = `worker-${process.pid}`;
const queue = 'emails';
const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

// Your job handlers (replace with real work). Throw to fail the job.
async function handle(payload) {
  switch (payload.type) {
    case 'send_welcome_email':
      // await sendWelcomeEmail(payload.email);
      return { emailSent: true };
    default:
      throw new Error(`Unknown job type: ${payload.type}`);
  }
}

async function processJobs() {
  console.log(`Worker ${workerId} starting...`);
  while (true) {
    try {
      const jobs = await spooled.claim(queue, workerId, 10);
      if (jobs.length === 0) {
        await sleep(1000);
        continue;
      }
      for (const job of jobs) {
        try {
          const result = await handle(job.payload);
          await spooled.complete(job, workerId, result);
          console.log(`✓ Job ${job.id} completed`);
        } catch (err) {
          await spooled.fail(job, workerId, err.message);
          console.log(`✗ Job ${job.id} failed: ${err.message}`);
        }
      }
    } catch (err) {
      console.error('Worker error:', err.message);
      await sleep(5000);
    }
  }
}

processJobs();
```

### Python

Python 3.8+ with `requests` (`pip install requests`). Saved as `spooled_client.py` (a file named `spooled.py` would shadow the official `spooled` SDK package):

```python
# spooled_client.py - a tiny REST client (pip install requests)
import os
import socket
import time
from datetime import datetime, timedelta, timezone

import requests


class Spooled:
    def __init__(self, api_key, base_url=os.getenv("SPOOLED_URL", "http://localhost:8080")):
        self.base_url = base_url
        self.session = requests.Session()
        # requests sets Content-Type: application/json for json= bodies
        self.session.headers["Authorization"] = f"Bearer {api_key}"

    def _post(self, path, body):
        response = self.session.post(f"{self.base_url}{path}", json=body, timeout=30)
        response.raise_for_status()
        return response.json()

    def dispatch(self, queue_name, payload, delay=None, priority=0):
        """Push a job to the queue (like Laravel's dispatch). Returns {"id", "created"}."""
        job = {"queue_name": queue_name, "payload": payload, "priority": priority}
        if delay:
            job["scheduled_at"] = (datetime.now(timezone.utc) + timedelta(seconds=delay)).isoformat()
        return self._post("/api/v1/jobs", job)

    def claim(self, queue_name, worker_id, limit=10):
        """Claim jobs for processing. Returns a list of jobs with id, payload and lease_id."""
        body = {"queue_name": queue_name, "worker_id": worker_id, "limit": limit}
        return self._post("/api/v1/jobs/claim", body)["jobs"]

    def complete(self, job, worker_id, result=None):
        """Mark a claimed job as completed"""
        body = {"worker_id": worker_id, "lease_id": job["lease_id"], "result": result or {}}
        self._post(f"/api/v1/jobs/{job['id']}/complete", body)

    def fail(self, job, worker_id, error):
        """Mark a claimed job as failed (retried until max_retries, then dead-lettered)"""
        body = {"worker_id": worker_id, "lease_id": job["lease_id"], "error": error}
        self._post(f"/api/v1/jobs/{job['id']}/fail", body)


if __name__ == "__main__":
    spooled = Spooled(os.environ["SPOOLED_API_KEY"])

    # Dispatch a job, and one with a 5 minute delay
    print(spooled.dispatch("emails", {"type": "send_welcome_email", "user_id": 123, "email": "user@example.com"}))
    spooled.dispatch("emails", {"type": "send_reminder", "user_id": 123}, delay=300)

    # A minimal worker loop (Ctrl+C to stop)
    worker_id = f"{socket.gethostname()}-{os.getpid()}"
    while True:
        jobs = spooled.claim("emails", worker_id)
        if not jobs:
            time.sleep(1)
            continue
        for job in jobs:
            try:
                # ... your work here, e.g. send_welcome_email(job["payload"]) ...
                spooled.complete(job, worker_id, {"email_sent": True})
                print("completed", job["id"])
            except Exception as exc:
                spooled.fail(job, worker_id, str(exc))
```

---

## Common Patterns

### Delayed Jobs

Send a job that will only be processed after a delay:

```bash
curl -s -X POST "$SPOOLED_URL/api/v1/jobs" \
  -H "Authorization: Bearer $SPOOLED_API_KEY" \
  -H "Content-Type: application/json" \
  -d '{
    "queue_name": "emails",
    "payload": {"type": "send_reminder", "user_id": 123},
    "scheduled_at": "2030-01-15T11:05:00Z"
  }'
```

> **Note:** `scheduled_at` is an ISO 8601 timestamp. For relative delays, calculate the future timestamp in your code.

This is like Laravel's:
```php
dispatch(new SendReminder($user))->delay(now()->addHour());
```

---

### Priority Jobs

Higher priority jobs are processed first:

Priority ranges from -100 to 100 (default 0):

```bash
# High priority (processed first)
curl -s -X POST "$SPOOLED_URL/api/v1/jobs" \
  -H "Authorization: Bearer $SPOOLED_API_KEY" \
  -H "Content-Type: application/json" \
  -d '{"queue_name": "emails", "payload": {"type": "password_reset"}, "priority": 10}'

# Normal priority
curl -s -X POST "$SPOOLED_URL/api/v1/jobs" \
  -H "Authorization: Bearer $SPOOLED_API_KEY" \
  -H "Content-Type: application/json" \
  -d '{"queue_name": "emails", "payload": {"type": "newsletter"}, "priority": 0}'

# Low priority (processed last)
curl -s -X POST "$SPOOLED_URL/api/v1/jobs" \
  -H "Authorization: Bearer $SPOOLED_API_KEY" \
  -H "Content-Type: application/json" \
  -d '{"queue_name": "emails", "payload": {"type": "digest"}, "priority": -10}'
```

---

### Scheduled Jobs (Cron)

Create recurring jobs that run on a schedule:

```bash
curl -s -X POST "$SPOOLED_URL/api/v1/schedules" \
  -H "Authorization: Bearer $SPOOLED_API_KEY" \
  -H "Content-Type: application/json" \
  -d '{
    "name": "daily-report",
    "queue_name": "reports",
    "cron_expression": "0 9 * * *",
    "payload_template": {"type": "generate_daily_report"},
    "timezone": "America/New_York"
  }'
```

The Free plan allows one schedule.

This creates a job every day at 9 AM New York time - like Laravel's:
```php
$schedule->job(new GenerateDailyReport)->dailyAt('09:00');
```

---

### Job Dependencies (Workflows)

Run jobs in sequence - job B only runs after job A completes. Each job has a `key`, and `depends_on` lists keys. Workflows are not available on the Free plan (`403 FEATURE_DISABLED`); see [Lifting plan limits](../../README.md#lifting-plan-limits-self-hosted).

```bash
# Create a workflow -> {"workflow_id":"...","job_ids":[{"key":"create-account","job_id":"..."},...],"status":"pending"}
curl -s -X POST "$SPOOLED_URL/api/v1/workflows" \
  -H "Authorization: Bearer $SPOOLED_API_KEY" \
  -H "Content-Type: application/json" \
  -d '{
    "name": "user-onboarding",
    "jobs": [
      {"key": "create-account", "queue_name": "users", "payload": {"step": 1}},
      {"key": "send-welcome", "queue_name": "emails", "payload": {"step": 2}, "depends_on": ["create-account"]},
      {"key": "setup-defaults", "queue_name": "users", "payload": {"step": 3}, "depends_on": ["create-account"]}
    ]
  }'
```

This is like Laravel's job chaining:
```php
Bus::chain([
    new CreateAccount($user),
    new SendWelcomeEmail($user),
    new SetupDefaults($user),
])->dispatch();
```

---

## Monitoring

### Check Job Status

```bash
curl -s "$SPOOLED_URL/api/v1/jobs/$JOB_ID" \
  -H "Authorization: Bearer $SPOOLED_API_KEY"
```

### List Pending Jobs

```bash
curl -s "$SPOOLED_URL/api/v1/jobs?queue_name=emails&status=pending" \
  -H "Authorization: Bearer $SPOOLED_API_KEY"
```

### View Failed Jobs (Dead Letter Queue)

```bash
curl -s "$SPOOLED_URL/api/v1/jobs/dlq?queue_name=emails" \
  -H "Authorization: Bearer $SPOOLED_API_KEY"
```

### Retry a Failed Job

```bash
curl -s -X POST "$SPOOLED_URL/api/v1/jobs/$JOB_ID/retry" \
  -H "Authorization: Bearer $SPOOLED_API_KEY"
```

---

## Comparison Cheat Sheet

| Laravel | Spooled |
|---------|---------|
| `dispatch(new Job($data))` | `POST /api/v1/jobs` with `queue_name` + `payload` |
| `dispatch()->delay(60)` | `scheduled_at` with future timestamp |
| `dispatch()->onQueue('emails')` | `queue_name: "emails"` in request |
| `php artisan queue:work` | Worker loop calling `POST /api/v1/jobs/claim` |
| `php artisan queue:retry` | `POST /api/v1/jobs/{id}/retry` |
| `failed_jobs` table | `GET /api/v1/jobs/dlq` |
| `$schedule->job()->daily()` | `POST /api/v1/schedules` with cron |
| `Bus::chain([...])` | `POST /api/v1/workflows` with dependencies |

---

## Next Steps

1. **Read the [API Reference](../openapi.yaml)** for all endpoints
2. **Check the [Architecture Guide](./architecture.md)** to understand the system
3. **Set up Grafana** to monitor your queues (the local development `docker-compose.yml` includes Prometheus and Grafana with a dashboard)
4. **Use the SDKs** (Node.js, Python, Go, PHP) for the best developer experience

---

## Getting Help

- **API Issues**: Check the response `error` field for details
- **Job not processing?** Verify workers are connected and the queue name matches
- **Grafana Dashboard**: with the local development stack, http://localhost:3000 (admin/admin)

Happy queuing! 🚀
