# SDKs Guide

Official SDKs for integrating with Spooled Cloud.

## Table of Contents

1. [Overview](#overview)
2. [Node.js SDK](#nodejs-sdk)
3. [Python SDK](#python-sdk)
4. [Go SDK](#go-sdk)
5. [REST API](#rest-api)
6. [gRPC API](#grpc-api)
7. [Community SDKs](#community-sdks)

---

## Overview

Spooled provides official SDKs for popular languages, plus REST and gRPC APIs for any platform.

| SDK | Status | Package |
|-----|--------|---------|
| **Node.js** | ✅ Available | `@spooled/sdk` |
| **Python** | ✅ Available | `spooled` |
| **Go** | 🚧 Coming Soon | `github.com/spooled-cloud/spooled-go` |
| **REST API** | ✅ Stable | Any HTTP client |
| **gRPC API** | ✅ Stable | Any gRPC client |

---

## Node.js SDK

### Installation

```bash
npm install @spooled/sdk
```

### Quick Start

```typescript
import { SpooledClient, SpooledWorker } from '@spooled/sdk';

// Initialize client
const client = new SpooledClient({
  apiKey: process.env.SPOOLED_API_KEY!,
  // baseUrl: 'http://localhost:8080'  // For self-hosted
});

// Create a job
const job = await client.jobs.create({
  queueName: 'emails',
  payload: {
    to: 'user@example.com',
    subject: 'Welcome!',
    template: 'welcome'
  },
  idempotencyKey: `welcome-${userId}`,
  maxRetries: 5
});

console.log(`Queued job: ${job.id}`);
```

### Client API

```typescript
// Jobs
const job = await client.jobs.create({ queueName, payload });
const job = await client.jobs.get(jobId);
const jobs = await client.jobs.list({ queueName, status, limit });
await client.jobs.cancel(jobId);
await client.jobs.retry(jobId);

// Bulk operations
const result = await client.jobs.bulkEnqueue([
  { queueName: 'emails', payload: { id: 1 } },
  { queueName: 'emails', payload: { id: 2 } }
]);

// Queues
const queues = await client.queues.list();
const stats = await client.queues.getStats(queueName);
await client.queues.pause(queueName);
await client.queues.resume(queueName);

// Schedules
const schedule = await client.schedules.create({
  name: 'Daily Report',
  cronExpression: '0 9 * * *',
  timezone: 'America/New_York',
  queue: 'reports',
  payloadTemplate: { type: 'daily' }
});
await client.schedules.trigger(scheduleId);
```

### Worker

```typescript
const worker = new SpooledWorker({
  apiKey: process.env.SPOOLED_API_KEY!,
  queueName: 'emails',
  concurrency: 10
});

worker.on('job', async (job) => {
  const { to, subject, template } = job.payload;
  
  try {
    await sendEmail({ to, subject, template });
    await job.complete({ sent: true });
  } catch (error) {
    await job.fail({ reason: error.message });
  }
});

worker.on('error', (error) => {
  console.error('Worker error:', error);
});

await worker.start();
```

### Worker with gRPC Streaming

```typescript
const worker = new SpooledWorker({
  apiKey: process.env.SPOOLED_API_KEY!,
  queue: 'emails',
  transport: 'grpc',  // Use gRPC streaming
  // Spooled Cloud (TLS)
  grpcHost: 'grpc.spooled.cloud:443'
  // For self-hosted local dev (no TLS): grpcHost: 'localhost:50051'
});
```

---

## Python SDK

### Installation

```bash
pip install spooled

# With all extras (gRPC, realtime)
pip install spooled[all]
```

### Quick Start

```python
from spooled import SpooledClient, SpooledWorker
import os

# Initialize client
client = SpooledClient(
    api_key=os.environ["SPOOLED_API_KEY"],
    # base_url="http://localhost:8080"  # For self-hosted
)

# Create a job
job = client.jobs.create({
    "queue_name": "image-processing",
    "payload": {
        "image_url": "https://example.com/image.jpg",
        "operations": ["resize", "compress"]
    },
    "idempotency_key": f"process-image-{image_id}",
    "max_retries": 3
})

print(f"Queued job: {job.id}")
```

### Client API

```python
# Jobs
job = client.jobs.create({"queue_name": queue_name, "payload": payload})
job = client.jobs.get(job_id)
jobs = client.jobs.list(queue_name=queue_name, status="pending", limit=50)
client.jobs.cancel(job_id)
client.jobs.retry(job_id)

# Bulk operations
result = client.jobs.bulk_enqueue({
    "jobs": [
        {"queue_name": "emails", "payload": {"id": 1}},
        {"queue_name": "emails", "payload": {"id": 2}}
    ]
})

# Queues
queues = client.queues.list()
stats = client.queues.get_stats(queue_name)
client.queues.pause(queue_name)
client.queues.resume(queue_name)

# Schedules
schedule = client.schedules.create(
    name="Daily Report",
    cron_expression="0 9 * * *",
    timezone="America/New_York",
    queue="reports",
    payload_template={"type": "daily"}
)
client.schedules.trigger(schedule_id)
```

### Worker

```python
worker = SpooledWorker(
    api_key=os.environ["SPOOLED_API_KEY"],
    queue="image-processing",
    concurrency=10
)

@worker.job_handler
async def process_image(job):
    image_url = job.payload["image_url"]
    operations = job.payload["operations"]
    
    try:
        result = await run_image_pipeline(image_url, operations)
        await job.complete(result={"output_url": result.url})
    except Exception as e:
        await job.fail(reason=str(e))

# Run worker
worker.run()
```

### Sync Worker (No Async)

```python
worker = SpooledWorker(
    api_key=os.environ["SPOOLED_API_KEY"],
    queue="reports",
    async_mode=False
)

@worker.job_handler
def generate_report(job):
    report_type = job.payload["type"]
    report = create_report(report_type)
    job.complete(result={"report_id": report.id})

worker.run()
```

---

## Go SDK

### Installation

```bash
go get github.com/spooled-cloud/spooled-go
```

### Quick Start

```go
package main

import (
    "context"
    "log"
    "os"
    
    spooled "github.com/spooled-cloud/spooled-go"
)

func main() {
    client := spooled.NewClient(os.Getenv("SPOOLED_API_KEY"))
    // client.SetBaseURL("http://localhost:8080")  // For self-hosted
    
    job, err := client.Jobs.Enqueue(context.Background(), &spooled.EnqueueParams{
        Queue: "webhook-delivery",
        Payload: map[string]interface{}{
            "url":     "https://customer.example.com/webhook",
            "event":   "order.completed",
            "payload": orderData,
        },
        IdempotencyKey: "order-webhook-" + orderId,
        MaxRetries:     5,
    })
    
    if err != nil {
        log.Fatal(err)
    }
    
    log.Printf("Queued job: %s", job.ID)
}
```

### Client API

```go
// Jobs
job, err := client.Jobs.Enqueue(ctx, params)
job, err := client.Jobs.Get(ctx, jobID)
jobs, err := client.Jobs.List(ctx, &spooled.ListParams{Queue: "emails", Status: "pending"})
err := client.Jobs.Cancel(ctx, jobID)
err := client.Jobs.Retry(ctx, jobID)

// Bulk operations
result, err := client.Jobs.BulkEnqueue(ctx, []spooled.EnqueueParams{...})

// Queues
queues, err := client.Queues.List(ctx)
stats, err := client.Queues.GetStats(ctx, queueName)
err := client.Queues.Pause(ctx, queueName)
err := client.Queues.Resume(ctx, queueName)

// Schedules
schedule, err := client.Schedules.Create(ctx, &spooled.CreateScheduleParams{...})
err := client.Schedules.Trigger(ctx, scheduleID)
```

### Worker

```go
package main

import (
    "context"
    "log"
    "os"
    
    spooled "github.com/spooled-cloud/spooled-go"
)

func main() {
    worker := spooled.NewWorker(&spooled.WorkerConfig{
        APIKey:      os.Getenv("SPOOLED_API_KEY"),
        Queue:       "emails",
        Concurrency: 10,
    })
    
    worker.HandleFunc(func(ctx context.Context, job *spooled.Job) error {
        email := job.Payload["to"].(string)
        subject := job.Payload["subject"].(string)
        
        if err := sendEmail(email, subject); err != nil {
            return err  // Will retry
        }
        
        return nil  // Success
    })
    
    if err := worker.Run(context.Background()); err != nil {
        log.Fatal(err)
    }
}
```

---

## REST API

For languages without an official SDK, use the REST API directly.

### Base URL

```
https://api.spooled.cloud
```

### Authentication

```bash
Authorization: Bearer sk_live_YOUR_API_KEY
```

### Example: Enqueue Job

```bash
curl -X POST https://api.spooled.cloud/api/v1/jobs \
  -H "Authorization: Bearer sk_live_YOUR_API_KEY" \
  -H "Content-Type: application/json" \
  -d '{
    "queue_name": "emails",
    "payload": {"to": "user@example.com"},
    "max_retries": 3
  }'
```

### Example: Claim Jobs

```bash
curl -X POST https://api.spooled.cloud/api/v1/jobs/claim \
  -H "Authorization: Bearer sk_live_YOUR_API_KEY" \
  -d '{"queue_name": "emails", "limit": 10}'
```

### Example: Complete Job

```bash
curl -X POST https://api.spooled.cloud/api/v1/jobs/job_xxx/complete \
  -H "Authorization: Bearer sk_live_YOUR_API_KEY" \
  -d '{"result": {"sent": true}}'
```

See [API Usage Guide](./api-usage.md) for complete documentation.

---

## gRPC API

For high-throughput workers, use the gRPC API.

### Connection

```
# Spooled Cloud (TLS over 443)
grpc.spooled.cloud:443

# Self-hosted / local (default)
localhost:50051
```

### Proto File

Download from: `https://github.com/spooled-cloud/spooled-backend/blob/main/proto/spooled.proto`

### Services

```protobuf
service QueueService {
  rpc Enqueue(EnqueueRequest) returns (EnqueueResponse);
  rpc Dequeue(DequeueRequest) returns (DequeueResponse);
  rpc Complete(CompleteRequest) returns (CompleteResponse);
  rpc Fail(FailRequest) returns (FailResponse);
  rpc RenewLease(RenewLeaseRequest) returns (RenewLeaseResponse);
  rpc GetJob(GetJobRequest) returns (GetJobResponse);
  rpc GetQueueStats(GetQueueStatsRequest) returns (GetQueueStatsResponse);
  
  // Streaming
  rpc StreamJobs(StreamJobsRequest) returns (stream Job);
  rpc ProcessJobs(stream ProcessRequest) returns (stream ProcessResponse);
}

service WorkerService {
  rpc Register(RegisterWorkerRequest) returns (RegisterWorkerResponse);
  rpc Heartbeat(HeartbeatRequest) returns (HeartbeatResponse);
  rpc Deregister(DeregisterRequest) returns (DeregisterResponse);
}
```

### Example with grpcurl

```bash
# List services
grpcurl -plaintext localhost:50051 list

# (Spooled Cloud) List services
grpcurl grpc.spooled.cloud:443 list

# Enqueue job
grpcurl -plaintext \
  -H "x-api-key: sk_live_xxx" \
  -d '{"queue_name": "emails", "payload": {"to": "user@example.com"}}' \
  localhost:50051 spooled.v1.QueueService/Enqueue

# (Spooled Cloud) Enqueue job
grpcurl \
  -H "x-api-key: sk_live_xxx" \
  -d '{"queue_name": "emails", "payload": {"to": "user@example.com"}}' \
  grpc.spooled.cloud:443 spooled.v1.QueueService/Enqueue

# Stream jobs
grpcurl -plaintext \
  -H "x-api-key: sk_live_xxx" \
  -d '{"queue_name": "emails", "worker_id": "w1"}' \
  localhost:50051 spooled.v1.QueueService/StreamJobs

# (Spooled Cloud) Stream jobs
grpcurl \
  -H "x-api-key: sk_live_xxx" \
  -d '{"queue_name": "emails", "worker_id": "w1"}' \
  grpc.spooled.cloud:443 spooled.v1.QueueService/StreamJobs
```

See [gRPC Server Guide](./grpc-server.md) for complete documentation.

---

## Community SDKs

Community-maintained SDKs (not officially supported):

| Language | Package | Maintainer |
|----------|---------|------------|
| Ruby | `spooled-ruby` | Community |
| PHP | `spooled-php` | Community |
| Java | `spooled-java` | Community |
| Rust | `spooled-rs` | Community |

---

## SDK Features Comparison

| Feature | Node.js | Python | Go | REST | gRPC |
|---------|---------|--------|-----|------|------|
| Enqueue jobs | ✅ | ✅ | ✅ | ✅ | ✅ |
| Bulk enqueue | ✅ | ✅ | ✅ | ✅ | ❌ |
| Worker class | ✅ | ✅ | ✅ | DIY | DIY |
| gRPC streaming | ✅ | ✅ | ✅ | ❌ | ✅ |
| Auto retry | ✅ | ✅ | ✅ | DIY | DIY |
| Type safety | ✅ | ✅ | ✅ | ❌ | ✅ |

---

## Best Practices

### 1. Use Environment Variables

```bash
# .env
SPOOLED_API_KEY=sk_live_xxxxx
SPOOLED_BASE_URL=https://api.spooled.cloud
```

### 2. Handle Errors Gracefully

```typescript
try {
  const job = await client.jobs.create({ ... });
} catch (error) {
  if (error.code === 'rate_limited') {
    await delay(error.retryAfter);
    // Retry
  } else if (error.code === 'validation_error') {
    console.error('Invalid request:', error.details);
  } else {
    throw error;
  }
}
```

### 3. Use Typed Payloads

```typescript
interface EmailPayload {
  to: string;
  subject: string;
  template: string;
}

const job = await client.jobs.create<EmailPayload>({
  queueName: 'emails',
  payload: {
    to: 'user@example.com',
    subject: 'Welcome!',
    template: 'welcome'
  }
});
```

### 4. Configure Timeouts

```typescript
const client = new SpooledClient({
  apiKey: process.env.SPOOLED_API_KEY!,
  timeout: 30000,  // 30 seconds
  retries: 3
});
```

---

## Next Steps

- [Quickstart](./quickstart.md) — Get started in 5 minutes
- [Jobs Guide](./jobs.md) — Job creation and processing
- [Workers Guide](./workers.md) — Building production workers
- [API Reference](./api-usage.md) — Complete API documentation
