# Workers Guide

Complete guide to building and running workers with Spooled Cloud.

## Table of Contents

1. [Overview](#overview)
2. [Worker Lifecycle](#worker-lifecycle)
3. [Building Workers](#building-workers)
4. [Worker Registration](#worker-registration)
5. [Processing Jobs](#processing-jobs)
6. [Concurrency](#concurrency)
7. [Health & Heartbeats](#health-heartbeats)
8. [Graceful Shutdown](#graceful-shutdown)
9. [gRPC Workers](#grpc-workers)
10. [Best Practices](#best-practices)

---

## Overview

Workers are your applications that process jobs from Spooled queues. Spooled itself doesn't execute your business logic - it manages the queue, and your workers do the actual work.

**Key concepts:**
- Workers claim jobs via REST or gRPC API
- Jobs have leases (default 5 minutes) - complete before expiry
- Workers send heartbeats to stay registered
- Multiple workers can process the same queue concurrently

---

## Worker Lifecycle

```
┌──────────┐     ┌──────────┐     ┌──────────┐
│ Register │────▶│ Healthy  │────▶│ Degraded │
└──────────┘     └────┬─────┘     └────┬─────┘
                      │                │
                      │ heartbeat      │ missed heartbeats
                      │                │
                      ▼                ▼
                 ┌──────────┐    ┌──────────┐
                 │ Healthy  │    │ Offline  │
                 └──────────┘    └──────────┘
```

### Worker States

| State | Description |
|-------|-------------|
| `healthy` | Sending heartbeats, ready for jobs |
| `degraded` | Missed 2+ heartbeats, may be struggling |
| `offline` | Missed 5+ heartbeats, jobs will be recovered |

---

## Building Workers

### Simple Polling Worker (REST)

```python
import requests
import time
import os

API_KEY = os.environ["SPOOLED_API_KEY"]
BASE_URL = "https://api.spooled.cloud"
WORKER_ID = f"worker-{os.getpid()}"
QUEUE = "emails"

headers = {
    "Authorization": f"Bearer {API_KEY}",
    "Content-Type": "application/json"
}

def main():
    print(f"Worker {WORKER_ID} starting...")
    
    while True:
        try:
            # Claim jobs
            response = requests.post(
                f"{BASE_URL}/api/v1/jobs/claim",
                headers=headers,
                json={
                    "queue_name": QUEUE,
                    "worker_id": WORKER_ID,
                    "limit": 10
                }
            )
            jobs = response.json().get("jobs", [])
            
            if not jobs:
                time.sleep(1)  # No jobs, wait before polling
                continue
            
            for job in jobs:
                process_job(job)
                
        except Exception as e:
            print(f"Error: {e}")
            time.sleep(5)

def process_job(job):
    job_id = job["id"]
    print(f"Processing {job_id}")
    
    try:
        # Your business logic here
        result = do_work(job["payload"])
        
        # Mark complete
        requests.post(
            f"{BASE_URL}/api/v1/jobs/{job_id}/complete",
            headers=headers,
            json={"worker_id": WORKER_ID, "lease_id": job.get("lease_id"), "result": result}
        )
        print(f"✓ Completed {job_id}")
        
    except Exception as e:
        # Mark failed (will retry)
        requests.post(
            f"{BASE_URL}/api/v1/jobs/{job_id}/fail",
            headers=headers,
            json={"worker_id": WORKER_ID, "lease_id": job.get("lease_id"), "error": str(e)}
        )
        print(f"✗ Failed {job_id}: {e}")

def do_work(payload):
    # Your actual processing logic
    return {"success": True}

if __name__ == "__main__":
    main()
```

### Node.js Worker

```javascript
const axios = require('axios');

const API_KEY = process.env.SPOOLED_API_KEY;
const BASE_URL = 'https://api.spooled.cloud';
const WORKER_ID = `worker-${process.pid}`;
const QUEUE = 'emails';

const client = axios.create({
  baseURL: BASE_URL,
  headers: { 'Authorization': `Bearer ${API_KEY}` }
});

async function main() {
  console.log(`Worker ${WORKER_ID} starting...`);
  
  while (true) {
    try {
      const { data } = await client.post('/api/v1/jobs/claim', {
        queue_name: QUEUE,
        worker_id: WORKER_ID,
        limit: 10
      });
      
      const jobs = data.jobs || [];
      
      if (jobs.length === 0) {
        await sleep(1000);
        continue;
      }
      
      for (const job of jobs) {
        await processJob(job);
      }
      
    } catch (err) {
      console.error('Error:', err.message);
      await sleep(5000);
    }
  }
}

async function processJob(job) {
  console.log(`Processing ${job.id}`);
  
  try {
    const result = await doWork(job.payload);
    
    await client.post(`/api/v1/jobs/${job.id}/complete`, { result });
    console.log(`✓ Completed ${job.id}`);
    
  } catch (err) {
    await client.post(`/api/v1/jobs/${job.id}/fail`, { reason: err.message });
    console.log(`✗ Failed ${job.id}: ${err.message}`);
  }
}

async function doWork(payload) {
  // Your actual processing logic
  return { success: true };
}

const sleep = (ms) => new Promise(r => setTimeout(r, ms));

main();
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
    queueName: 'emails',
    concurrency: 10,
    pollIntervalMs: 1000,
));

$worker->process(function (JobContext $ctx): array {
    echo "Processing {$ctx->job->id}\n";
    
    $result = doWork($ctx->payload);
    
    echo "✓ Completed {$ctx->job->id}\n";
    return $result;
});

$worker->on('error', function (\Throwable $e, ?JobContext $ctx): void {
    if ($ctx) {
        echo "✗ Failed {$ctx->job->id}: {$e->getMessage()}\n";
    } else {
        echo "Worker error: {$e->getMessage()}\n";
    }
});

// Graceful shutdown
pcntl_signal(SIGTERM, fn() => $worker->stop());
pcntl_signal(SIGINT, fn() => $worker->stop());

$worker->start();
```

---

## Worker Registration

For advanced use cases, register your worker to enable monitoring and job routing.

### Register Worker

```bash
curl -X POST https://api.spooled.cloud/api/v1/workers/register \
  -H "Authorization: Bearer sp_live_YOUR_API_KEY" \
  -H "Content-Type: application/json" \
  -d '{
    "worker_id": "emails-worker-1",
    "queue_name": "emails",
    "hostname": "worker-1.example.com",
    "max_concurrency": 10,
    "metadata": {
      "region": "us-east-1"
    }
  }'
```

Response:

```json
{
  "id": "emails-worker-1",
  "queue_name": "emails",
  "lease_duration_secs": 30,
  "heartbeat_interval_secs": 10
}
```

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `queue_name` | string | Yes | Queue this worker processes (1-100 chars) |
| `hostname` | string | Yes | Host the worker runs on (1-255 chars) |
| `worker_id` | string | No | Stable worker id, 1-128 chars from `A-Z a-z 0-9 . _ -` |
| `worker_type` | string | No | Free-form label (max 50 chars) |
| `max_concurrency` | integer | No | 1-100 (default: 5) |
| `metadata` | object | No | Arbitrary JSON (max 64KB) |
| `version` | string | No | Worker build version (max 50 chars) |

### Pick a Stable `worker_id`

Supplying `worker_id` makes registration an **upsert**: a worker that restarts reuses its
existing row instead of creating another one. Re-registering an id you already own is not
charged against your plan's worker limit.

Omit it and the server mints a fresh UUID, which is the older behaviour — but then every
restart leaves the previous row behind, counting against your worker cap until the
stale-worker reaper clears it about two minutes later. On a tight plan a crash-looping
worker can therefore quota itself out of registering and start getting `429` responses.

A `worker_id` already registered by a **different** organization is rejected with `409`.

### Deregister Worker

```bash
curl -X POST https://api.spooled.cloud/api/v1/workers/emails-worker-1/deregister \
  -H "Authorization: Bearer sp_live_YOUR_API_KEY"
```

---

## Processing Jobs

### Claim Jobs

```bash
curl -X POST https://api.spooled.cloud/api/v1/jobs/claim \
  -H "Authorization: Bearer sp_live_YOUR_API_KEY" \
  -H "Content-Type: application/json" \
  -d '{
    "queue_name": "emails",
    "worker_id": "emails-worker-1",
    "limit": 10
  }'
```

### Complete Job

```bash
curl -X POST https://api.spooled.cloud/api/v1/jobs/job_xxx/complete \
  -H "Authorization: Bearer sp_live_YOUR_API_KEY" \
  -H "Content-Type: application/json" \
  -d '{
    "result": {"message_id": "msg_123", "delivered": true}
  }'
```

### Fail Job

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

### Renew Lease

For long-running jobs, renew the lease before it expires:

```bash
curl -X POST https://api.spooled.cloud/api/v1/jobs/job_xxx/renew-lease \
  -H "Authorization: Bearer sp_live_YOUR_API_KEY" \
  -H "Content-Type: application/json" \
  -d '{
    "extension_seconds": 300
  }'
```

---

## Concurrency

### Configuring Concurrency

Control how many jobs a worker processes simultaneously:

```python
import asyncio
from concurrent.futures import ThreadPoolExecutor

CONCURRENCY = 10
executor = ThreadPoolExecutor(max_workers=CONCURRENCY)

async def worker_loop():
    while True:
        jobs = await claim_jobs(limit=CONCURRENCY)
        
        if jobs:
            # Process concurrently
            futures = [
                executor.submit(process_job, job) 
                for job in jobs
            ]
            # Wait for all to complete
            for future in futures:
                future.result()
        else:
            await asyncio.sleep(1)
```

### Concurrency Best Practices

| Resource Type | Suggested Concurrency |
|---------------|----------------------|
| CPU-bound | 1-2 per CPU core |
| I/O-bound (API calls) | 10-50 |
| Memory-heavy | Based on available RAM |
| External rate limits | Match external limit |

---

## Health & Heartbeats

### Send Heartbeat

Registered workers should send heartbeats every 10 seconds:

```bash
curl -X POST https://api.spooled.cloud/api/v1/workers/emails-worker-1/heartbeat \
  -H "Authorization: Bearer sp_live_YOUR_API_KEY" \
  -H "Content-Type: application/json" \
  -d '{
    "status": "healthy",
    "current_jobs": 5,
    "cpu_percent": 45,
    "memory_percent": 60
  }'
```

### Heartbeat Response

```json
{
  "acknowledged": true,
  "next_heartbeat_seconds": 10,
  "commands": []
}
```

### Worker Health Check

```bash
curl https://api.spooled.cloud/api/v1/workers/emails-worker-1 \
  -H "Authorization: Bearer sp_live_YOUR_API_KEY"
```

Response:

```json
{
  "id": "emails-worker-1",
  "status": "healthy",
  "queue_names": ["emails"],
  "current_jobs": 5,
  "last_heartbeat": "2024-01-15T10:00:00Z",
  "registered_at": "2024-01-15T09:00:00Z"
}
```

---

## Graceful Shutdown

Handle shutdown signals properly to avoid job loss:

```python
import signal
import sys

shutdown_requested = False

def signal_handler(signum, frame):
    global shutdown_requested
    print("Shutdown signal received, finishing current jobs...")
    shutdown_requested = True

signal.signal(signal.SIGTERM, signal_handler)
signal.signal(signal.SIGINT, signal_handler)

def main():
    while not shutdown_requested:
        jobs = claim_jobs(limit=1)  # Claim fewer jobs when approaching shutdown
        
        for job in jobs:
            if shutdown_requested:
                # Don't start new jobs, let this one finish
                break
            process_job(job)
    
    # Deregister worker
    deregister_worker()
    print("Worker shutdown complete")
    sys.exit(0)
```

### Kubernetes Graceful Shutdown

```yaml
spec:
  containers:
    - name: worker
      lifecycle:
        preStop:
          exec:
            command: ["/bin/sh", "-c", "sleep 30"]
  terminationGracePeriodSeconds: 60
```

---

## gRPC Workers

For high-throughput scenarios, use gRPC streaming:

### Stream Jobs (Server Streaming)

Jobs are pushed to your worker as they become available:

```python
import grpc
from spooled_pb2 import StreamJobsRequest
from spooled_pb2_grpc import QueueServiceStub

# Spooled Cloud (TLS over 443)
channel = grpc.secure_channel('grpc.spooled.cloud:443', grpc.ssl_channel_credentials())

# Self-hosted / local dev (no TLS by default)
# channel = grpc.insecure_channel('localhost:50051')
stub = QueueServiceStub(channel)

request = StreamJobsRequest(
    queue_name="emails",
    worker_id="worker-1",
    lease_duration_secs=300
)

metadata = [('x-api-key', 'sp_live_YOUR_API_KEY')]

for job in stub.StreamJobs(request, metadata=metadata):
    print(f"Received job: {job.id}")
    process_job(job)
    stub.Complete(CompleteRequest(job_id=job.id, worker_id="worker-1"), metadata=metadata)
```

### Bidirectional Streaming

For maximum control, use bidirectional streaming:

```python
def process_jobs():
    while True:
        # Request jobs
        yield ProcessRequest(
            dequeue=DequeueRequest(
                queue_name="emails",
                worker_id="worker-1",
                batch_size=10
            )
        )
        
        # Receive and process
        # ...
        
        # Send completion
        yield ProcessRequest(
            complete=CompleteRequest(
                job_id=job.id,
                worker_id="worker-1"
            )
        )

for response in stub.ProcessJobs(process_jobs(), metadata=metadata):
    handle_response(response)
```

---

## Best Practices

### 1. Use Stable Worker IDs

Always include a worker ID, and make it survive restarts — derive it from something fixed
about the deployment, such as the hostname or the ordinal your orchestrator assigns:

```python
WORKER_ID = f"emails-{hostname}"          # bare metal / VM
WORKER_ID = os.environ["POD_NAME"]        # Kubernetes StatefulSet
```

Do **not** mix in the pid or a random suffix:

```python
WORKER_ID = f"{hostname}-{pid}-{uuid.uuid4().hex[:8]}"  # new identity every restart
```

If you call `/workers/register`, a fresh id per restart registers a fresh worker row each
time, and the old one keeps a slot in your plan's worker limit until the stale-worker
reaper clears it about two minutes later. A worker in a crash loop can pile up enough of
those rows to hit the cap and get `429` on the very registration it needs. A stable id
upserts one row instead, and re-registering an id you already own is free.

### 2. Handle Lease Expiry

Monitor job processing time and renew leases:

```python
import threading

def process_with_lease_renewal(job):
    # Start lease renewal thread
    stop_renewal = threading.Event()
    renewal_thread = threading.Thread(
        target=renew_lease_periodically,
        args=(job["id"], stop_renewal)
    )
    renewal_thread.start()
    
    try:
        result = do_work(job["payload"])
        complete_job(job["id"], result)
    finally:
        stop_renewal.set()
        renewal_thread.join()
```

### 3. Implement Backpressure

Don't claim more jobs than you can handle:

```python
current_jobs = 0
MAX_CONCURRENT = 10

while True:
    available_slots = MAX_CONCURRENT - current_jobs
    if available_slots > 0:
        jobs = claim_jobs(limit=available_slots)
        current_jobs += len(jobs)
        for job in jobs:
            process_async(job, on_complete=lambda: current_jobs -= 1)
    else:
        time.sleep(0.1)  # Wait for slots to free up
```

### 4. Log Everything

```python
import structlog

log = structlog.get_logger()

def process_job(job):
    log.info("processing_job", job_id=job["id"], queue=job["queue_name"])
    
    try:
        result = do_work(job["payload"])
        log.info("job_completed", job_id=job["id"], result=result)
    except Exception as e:
        log.error("job_failed", job_id=job["id"], error=str(e))
        raise
```

### 5. Use Health Checks

Implement a `/health` endpoint for your worker:

```python
from flask import Flask
app = Flask(__name__)

@app.route('/health')
def health():
    return {
        "status": "healthy",
        "worker_id": WORKER_ID,
        "current_jobs": len(active_jobs),
        "uptime_seconds": time.time() - start_time
    }
```

### 6. Monitor Worker Metrics

Expose Prometheus metrics:

```python
from prometheus_client import Counter, Gauge, Histogram

jobs_processed = Counter('worker_jobs_processed_total', 'Jobs processed', ['queue', 'status'])
jobs_in_progress = Gauge('worker_jobs_in_progress', 'Jobs currently processing')
processing_time = Histogram('worker_job_processing_seconds', 'Job processing time')

@processing_time.time()
def process_job(job):
    jobs_in_progress.inc()
    try:
        do_work(job)
        jobs_processed.labels(queue=job['queue_name'], status='completed').inc()
    except:
        jobs_processed.labels(queue=job['queue_name'], status='failed').inc()
        raise
    finally:
        jobs_in_progress.dec()
```

---

## Next Steps

- [Jobs Guide](./jobs.md) — Job creation and properties
- [Retries Guide](./retries.md) — Retry configuration
- [gRPC Guide](./grpc-server.md) — High-performance gRPC API
- [API Reference](./api-usage.md) — Complete API documentation
