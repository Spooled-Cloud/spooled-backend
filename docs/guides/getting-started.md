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

### Step 1: Create an Organization

An organization is like a "tenant" - it groups your queues, jobs, and API keys.

```bash
curl -X POST https://api.spooled.cloud/api/v1/organizations \
  -H "Content-Type: application/json" \
  -d '{
    "name": "My Company",
    "slug": "my-company"
  }'
```

**Response:**
```json
{
  "id": "org_abc123",
  "name": "My Company",
  "slug": "my-company",
  "created_at": "2024-01-15T10:00:00Z"
}
```

> 💡 Save the `id` - you'll need it!

---

### Step 2: Get Your API Key

When you create an organization (Step 1), you automatically receive an initial API key in the response. If you need additional API keys:

```bash
curl -X POST https://api.spooled.cloud/api/v1/api-keys \
  -H "Authorization: Bearer sp_live_xxxxxxxxxxxxxxxxxxxx" \
  -H "Content-Type: application/json" \
  -d '{
    "name": "Production App",
    "queues": ["emails", "notifications"]
  }'
```

**Response:**
```json
{
  "id": "key_xyz789",
  "name": "Production App",
  "key": "sp_live_xxxxxxxxxxxxxxxxxxxx",
  "queues": ["emails", "notifications"],
  "created_at": "2024-01-15T10:00:00Z"
}
```

> ⚠️ **Save the `key` immediately!** It's only shown once (like Stripe API keys).

---

### Step 3: Configure a Queue (Optional)

Queues are created implicitly when you enqueue your first job. You can optionally configure queue settings:

```bash
curl -X PUT https://api.spooled.cloud/api/v1/queues/emails/config \
  -H "Authorization: Bearer sp_live_xxxxxxxxxxxxxxxxxxxx" \
  -H "Content-Type: application/json" \
  -d '{
    "max_retries": 3,
    "default_timeout": 300,
    "rate_limit": {"requests": 100, "period": 60}
  }'
```

**Response:**
```json
{
  "name": "emails",
  "max_retries": 3,
  "default_timeout": 300,
  "rate_limit": {"requests": 100, "period": 60}
}
```

---

### Step 4: Push a Job (Like Laravel's `dispatch()`)

This is equivalent to Laravel's `dispatch(new SendEmail($user))`:

```bash
curl -X POST https://api.spooled.cloud/api/v1/jobs \
  -H "Authorization: Bearer sp_live_xxxxxxxxxxxxxxxxxxxx" \
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
  "id": "job_ghi789",
  "queue_name": "emails",
  "status": "pending",
  "payload": {
    "type": "send_welcome_email",
    "user_id": 123,
    "email": "user@example.com",
    "template": "welcome"
  },
  "created_at": "2024-01-15T10:05:00Z"
}
```

---

### Step 5: Process Jobs (Like `php artisan queue:work`)

Your worker fetches jobs and processes them:

```bash
# 1. Claim jobs (get the next job(s) to process)
curl -X POST https://api.spooled.cloud/api/v1/jobs/claim \
  -H "Authorization: Bearer sp_live_xxxxxxxxxxxxxxxxxxxx" \
  -H "Content-Type: application/json" \
  -d '{
    "queue_name": "emails",
    "worker_id": "worker-1",
    "limit": 10
  }'
```

**Response:**
```json
[
  {
    "id": "job_ghi789",
    "queue_name": "emails",
    "status": "processing",
    "payload": {
      "type": "send_welcome_email",
      "user_id": 123,
      "email": "user@example.com",
      "template": "welcome"
    },
    "attempts": 1,
    "leased_until": "2024-01-15T10:10:00Z"
  }
]
```

```bash
# 2. Do your work (send the email, process data, etc.)
# ... your code here ...

# 3. Mark the job as completed
curl -X POST https://api.spooled.cloud/api/v1/jobs/job_ghi789/complete \
  -H "Authorization: Bearer sp_live_xxxxxxxxxxxxxxxxxxxx" \
  -H "Content-Type: application/json" \
  -d '{
    "worker_id": "worker-1",
    "result": {"email_sent": true, "message_id": "msg_123"}
  }'
```

---

## Code Examples

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
        $this->baseUrl = config('services.spooled.url', 'https://api.spooled.cloud');
    }

    /**
     * Push a job to the queue (like dispatch())
     */
    public function dispatch(string $queue, array $payload, array $options = []): array
    {
        $response = Http::withToken($this->apiKey)
            ->post("{$this->baseUrl}/api/v1/jobs", [
                'queue' => $queue,
                'payload' => $payload,
                'delay_seconds' => $options['delay'] ?? null,
                'priority' => $options['priority'] ?? 0,
            ]);

        return $response->json();
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
// worker.php - Run this as a separate process

require 'vendor/autoload.php';

$apiKey = getenv('SPOOLED_API_KEY');
$baseUrl = getenv('SPOOLED_URL') ?: 'https://api.spooled.cloud';
$workerId = gethostname() . '-' . getmypid();
$queue = 'emails';

echo "Worker {$workerId} starting...\n";

while (true) {
    // Claim jobs
    $response = file_get_contents("{$baseUrl}/api/v1/jobs/claim", false, stream_context_create([
        'http' => [
            'method' => 'POST',
            'header' => [
                "Authorization: Bearer {$apiKey}",
                "Content-Type: application/json",
            ],
            'content' => json_encode([
                'queue_name' => $queue,
                'worker_id' => $workerId,
                'limit' => 10,
            ]),
        ],
    ]));

    $jobs = json_decode($response, true);

    if (empty($jobs)) {
        // No jobs available, wait and retry
        sleep(1);
        continue;
    }

    foreach ($jobs as $job) {

        echo "Processing job {$job['id']}: {$job['payload']['type']}\n";

        try {
            // Process the job based on type
            switch ($job['payload']['type']) {
                case 'send_welcome_email':
                    sendWelcomeEmail($job['payload']);
                    break;
                case 'send_reminder':
                    sendReminderEmail($job['payload']);
                    break;
                default:
                    throw new Exception("Unknown job type: {$job['payload']['type']}");
            }

            // Mark as completed
            completeJob($baseUrl, $apiKey, $workerId, $job['id']);
            echo "✓ Job {$job['id']} completed\n";

        } catch (Exception $e) {
            // Mark as failed
            failJob($baseUrl, $apiKey, $workerId, $job['id'], $e->getMessage());
            echo "✗ Job {$job['id']} failed: {$e->getMessage()}\n";
        }
    }
}

function completeJob($baseUrl, $apiKey, $workerId, $jobId) {
    file_get_contents("{$baseUrl}/api/v1/jobs/{$jobId}/complete", false, stream_context_create([
        'http' => [
            'method' => 'POST',
            'header' => ["Authorization: Bearer {$apiKey}", "Content-Type: application/json"],
            'content' => json_encode(['worker_id' => $workerId]),
        ],
    ]));
}

function failJob($baseUrl, $apiKey, $workerId, $jobId, $error) {
    file_get_contents("{$baseUrl}/api/v1/jobs/{$jobId}/fail", false, stream_context_create([
        'http' => [
            'method' => 'POST',
            'header' => ["Authorization: Bearer {$apiKey}", "Content-Type: application/json"],
            'content' => json_encode(['worker_id' => $workerId, 'error' => $error]),
        ],
    ]));
}
```

### Node.js

```javascript
// spooled.js
const axios = require('axios');

class Spooled {
  constructor(apiKey, baseUrl = 'https://api.spooled.cloud') {
    this.client = axios.create({
      baseURL: baseUrl,
      headers: { 'Authorization': `Bearer ${apiKey}` }
    });
  }

  // Push a job (like Laravel's dispatch)
  async dispatch(queue_name, payload, options = {}) {
    const { data } = await this.client.post('/api/v1/jobs', {
      queue_name,
      payload,
      scheduled_at: options.delay ? new Date(Date.now() + options.delay * 1000).toISOString() : undefined,
      priority: options.priority
    });
    return data;
  }

  // Claim jobs for processing
  async claim(queue_name, workerId, limit = 10) {
    const { data } = await this.client.post('/api/v1/jobs/claim', {
      queue_name,
      worker_id: workerId,
      limit
    });
    return data;
  }

  // Mark job as completed
  async complete(jobId, workerId, result = {}) {
    await this.client.post(`/api/v1/jobs/${jobId}/complete`, { 
      worker_id: workerId,
      result 
    });
  }

  // Mark job as failed
  async fail(jobId, workerId, error) {
    await this.client.post(`/api/v1/jobs/${jobId}/fail`, { 
      worker_id: workerId,
      error 
    });
  }
}

// Usage:
const spooled = new Spooled(process.env.SPOOLED_API_KEY);

// Dispatch a job
await spooled.dispatch('emails', {
  type: 'send_welcome_email',
  userId: 123,
  email: 'user@example.com'
});

// Dispatch with delay (5 minutes)
await spooled.dispatch('emails', {
  type: 'send_reminder',
  userId: 123
}, { delay: 300 });
```

### Node.js Worker

```javascript
// worker.js
const Spooled = require('./spooled');

const spooled = new Spooled(process.env.SPOOLED_API_KEY);
const workerId = `worker-${process.pid}`;
const queue = 'emails';

console.log(`Worker ${workerId} starting...`);

async function processJobs() {
  while (true) {
    try {
      const jobs = await spooled.claim(queue, workerId, 10);
      
      if (!jobs || jobs.length === 0) {
        await sleep(1000);
        continue;
      }

      for (const job of jobs) {
        console.log(`Processing job ${job.id}: ${job.payload.type}`);

        try {
          // Process based on job type
          switch (job.payload.type) {
            case 'send_welcome_email':
              await sendWelcomeEmail(job.payload);
              break;
            case 'send_reminder':
              await sendReminderEmail(job.payload);
              break;
            default:
              throw new Error(`Unknown job type: ${job.payload.type}`);
          }

          await spooled.complete(job.id, workerId);
          console.log(`✓ Job ${job.id} completed`);

        } catch (err) {
          await spooled.fail(job.id, workerId, err.message);
          console.log(`✗ Job ${job.id} failed: ${err.message}`);
        }
      }

    } catch (err) {
      console.error('Worker error:', err.message);
      await sleep(5000);
    }
  }
}

const sleep = (ms) => new Promise(resolve => setTimeout(resolve, ms));

processJobs();
```

### Python

```python
# spooled.py
import requests
import os
import time
import socket

class Spooled:
    def __init__(self, api_key, base_url='https://api.spooled.cloud'):
        self.base_url = base_url
        self.headers = {
            'Authorization': f'Bearer {api_key}',
            'Content-Type': 'application/json'
        }

    def dispatch(self, queue_name, payload, delay=None, priority=0):
        """Push a job to the queue (like Laravel's dispatch)"""
        job_data = {
            'queue_name': queue_name,
            'payload': payload,
            'priority': priority
        }
        if delay:
            from datetime import datetime, timedelta
            job_data['scheduled_at'] = (datetime.utcnow() + timedelta(seconds=delay)).isoformat() + 'Z'
        
        response = requests.post(
            f'{self.base_url}/api/v1/jobs',
            headers=self.headers,
            json=job_data
        )
        return response.json()

    def claim(self, queue_name, worker_id, limit=10):
        """Claim jobs for processing"""
        response = requests.post(
            f'{self.base_url}/api/v1/jobs/claim',
            headers=self.headers,
            json={'queue_name': queue_name, 'worker_id': worker_id, 'limit': limit}
        )
        return response.json()

    def complete(self, job_id, worker_id, result=None):
        """Mark a job as completed"""
        requests.post(
            f'{self.base_url}/api/v1/jobs/{job_id}/complete',
            headers=self.headers,
            json={'worker_id': worker_id, 'result': result or {}}
        )

    def fail(self, job_id, worker_id, error):
        """Mark a job as failed"""
        requests.post(
            f'{self.base_url}/api/v1/jobs/{job_id}/fail',
            headers=self.headers,
            json={'worker_id': worker_id, 'error': error}
        )


# Usage:
spooled = Spooled(os.getenv('SPOOLED_API_KEY'))

# Dispatch a job
spooled.dispatch('emails', {
    'type': 'send_welcome_email',
    'user_id': 123,
    'email': 'user@example.com'
})

# Dispatch with 5 minute delay
spooled.dispatch('emails', {
    'type': 'send_reminder',
    'user_id': 123
}, delay=300)
```

---

## Common Patterns

### Delayed Jobs

Send a job that will only be processed after a delay:

```bash
curl -X POST https://api.spooled.cloud/api/v1/jobs \
  -H "Authorization: Bearer sp_live_xxx" \
  -H "Content-Type: application/json" \
  -d '{
    "queue_name": "emails",
    "payload": {"type": "send_reminder", "user_id": 123},
    "scheduled_at": "2024-01-15T11:05:00Z"
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

```bash
# High priority (processed first)
curl -X POST https://api.spooled.cloud/api/v1/jobs \
  -H "Authorization: Bearer sp_live_xxx" \
  -d '{"queue_name": "emails", "payload": {...}, "priority": 10}'

# Normal priority
curl -X POST https://api.spooled.cloud/api/v1/jobs \
  -H "Authorization: Bearer sp_live_xxx" \
  -d '{"queue_name": "emails", "payload": {...}, "priority": 0}'

# Low priority (processed last)
curl -X POST https://api.spooled.cloud/api/v1/jobs \
  -H "Authorization: Bearer sp_live_xxx" \
  -d '{"queue_name": "emails", "payload": {...}, "priority": -10}'
```

---

### Scheduled Jobs (Cron)

Create recurring jobs that run on a schedule:

```bash
curl -X POST https://api.spooled.cloud/api/v1/schedules \
  -H "Authorization: Bearer sp_live_xxx" \
  -H "Content-Type: application/json" \
  -d '{
    "name": "daily-report",
    "queue_name": "reports",
    "cron_expression": "0 9 * * *",
    "payload": {"type": "generate_daily_report"},
    "timezone": "America/New_York"
  }'
```

This creates a job every day at 9 AM New York time - like Laravel's:
```php
$schedule->job(new GenerateDailyReport)->dailyAt('09:00');
```

---

### Job Dependencies (Workflows)

Run jobs in sequence - job B only runs after job A completes:

```bash
# Create a workflow
curl -X POST https://api.spooled.cloud/api/v1/workflows \
  -H "Authorization: Bearer sp_live_xxx" \
  -H "Content-Type: application/json" \
  -d '{
    "name": "user-onboarding",
    "jobs": [
      {"name": "create-account", "queue_name": "users", "payload": {"step": 1}},
      {"name": "send-welcome", "queue_name": "emails", "payload": {"step": 2}, "depends_on": ["create-account"]},
      {"name": "setup-defaults", "queue_name": "users", "payload": {"step": 3}, "depends_on": ["create-account"]}
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
curl https://api.spooled.cloud/api/v1/jobs/job_xxx \
  -H "Authorization: Bearer sp_live_xxx"
```

### List Pending Jobs

```bash
curl "https://api.spooled.cloud/api/v1/jobs?queue_name=emails&status=pending" \
  -H "Authorization: Bearer sp_live_xxx"
```

### View Failed Jobs (Dead Letter Queue)

```bash
curl "https://api.spooled.cloud/api/v1/jobs/dlq?queue_name=emails" \
  -H "Authorization: Bearer sp_live_xxx"
```

### Retry a Failed Job

```bash
curl -X POST https://api.spooled.cloud/api/v1/jobs/job_xxx/retry \
  -H "Authorization: Bearer sp_live_xxx"
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
3. **Set up Grafana** to monitor your queues (included in docker-compose)
4. **Use the SDKs** (Node.js, Python, Go, PHP) for the best developer experience

---

## Getting Help

- **API Issues**: Check the response `error` field for details
- **Job not processing?** Verify workers are connected and the queue name matches
- **Grafana Dashboard**: Monitor job throughput and latency at `/grafana`

Happy queuing! 🚀
