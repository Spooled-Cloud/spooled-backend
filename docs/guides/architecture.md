# Spooled Cloud Architecture Guide

This document provides a comprehensive overview of the Spooled Cloud backend architecture.

## Table of Contents

1. [System Overview](#system-overview)
2. [Core Components](#core-components)
3. [Data Flow](#data-flow)
4. [Database Design](#database-design)
5. [API Design](#api-design)
6. [Security Model](#security-model)
7. [Scalability Patterns](#scalability-patterns)

---

## System Overview

```
┌─────────────────────────────────────────────────────────────────────────┐
│                           Spooled Cloud                                  │
├─────────────────────────────────────────────────────────────────────────┤
│                                                                          │
│  ┌─────────────┐    ┌─────────────┐    ┌─────────────┐                  │
│  │   Clients   │    │  Webhooks   │    │   Workers   │                  │
│  │  (REST/WS)  │    │(GitHub/etc) │    │  (gRPC/REST)│                  │
│  └──────┬──────┘    └──────┬──────┘    └──────┬──────┘                  │
│         │                  │                  │                          │
│         ▼                  ▼                  ▼                          │
│  ┌─────────────────────────────────────────────────────┐                │
│  │                    API Gateway                       │                │
│  │  (Axum - Rate Limit, Auth, Validation, Versioning)  │                │
│  └───────────────────────┬─────────────────────────────┘                │
│                          │                                               │
│         ┌────────────────┼────────────────┐                             │
│         ▼                ▼                ▼                             │
│  ┌────────────┐   ┌────────────┐   ┌────────────┐                       │
│  │   Queue    │   │  Worker    │   │  Webhook   │                       │
│  │  Manager   │   │  Manager   │   │  Handler   │                       │
│  └─────┬──────┘   └─────┬──────┘   └─────┬──────┘                       │
│        │                │                │                               │
│        └────────────────┴────────────────┘                              │
│                         │                                                │
│  ┌──────────────────────┴──────────────────────┐                        │
│  │              Background Scheduler            │                        │
│  │  (Lease Recovery, Cron, Cleanup, Metrics)   │                        │
│  └──────────────────────┬──────────────────────┘                        │
│                         │                                                │
│         ┌───────────────┴───────────────┐                               │
│         ▼                               ▼                               │
│  ┌─────────────┐                 ┌─────────────┐                        │
│  │ PostgreSQL  │                 │    Redis    │                        │
│  │  (Primary)  │                 │   (Cache)   │                        │
│  └─────────────┘                 └─────────────┘                        │
│                                                                          │
└─────────────────────────────────────────────────────────────────────────┘
```

---

## Core Components

### 1. API Layer (`src/api/`)

The API layer handles all incoming HTTP/WebSocket requests.

```
src/api/
├── mod.rs           # Router setup, middleware chain
├── pagination.rs    # Cursor-based pagination
├── handlers/
│   ├── auth.rs      # JWT authentication
│   ├── jobs.rs      # Job CRUD, bulk ops
│   ├── queues.rs    # Queue configuration
│   ├── workers.rs   # Worker management
│   ├── webhooks.rs  # Webhook ingestion
│   ├── workflows.rs # DAG workflows
│   ├── schedules.rs # Cron scheduling
│   ├── realtime.rs  # WebSocket/SSE
│   └── health.rs    # Health checks
└── middleware/
    ├── auth.rs      # API key validation
    ├── rate_limit.rs# Redis-based rate limiting
    ├── validation.rs# Request validation
    ├── versioning.rs# API version headers
    └── security.rs  # Security headers
```

#### Request Flow

```
Request → Rate Limit → Auth → Validation → Handler → Response
                                              │
                                              ▼
                              ┌───────────────────────┐
                              │ Database/Cache Layer  │
                              └───────────────────────┘
```

### 2. Queue Manager (`src/queue/`)

Core job queue logic with guaranteed delivery.

**Key Features:**
- `FOR UPDATE SKIP LOCKED` for safe concurrent dequeue
- Exponential backoff retry logic
- Dead-letter queue for failed jobs
- Lease-based job ownership

```rust
// Dequeue Algorithm
1. BEGIN TRANSACTION
2. SELECT job FROM jobs 
   WHERE status = 'pending' 
   AND (scheduled_at IS NULL OR scheduled_at <= NOW())
   ORDER BY priority DESC, created_at ASC
   LIMIT 1
   FOR UPDATE SKIP LOCKED
3. UPDATE job SET status = 'processing', lease_expires_at = NOW() + lease_duration
4. COMMIT
5. Return job to worker
```

### 3. Worker Manager (`src/worker/`)

Manages worker lifecycle and job distribution.

**Worker States:**
```
┌──────────┐     ┌──────────┐     ┌──────────┐
│ Offline  │────▶│ Healthy  │────▶│ Degraded │
└──────────┘     └────┬─────┘     └────┬─────┘
      ▲               │                │
      │               ▼                │
      │         ┌──────────┐          │
      └─────────│ Offline  │◀─────────┘
                └──────────┘
```

**Heartbeat Protocol:**
- Workers send heartbeat every 10 seconds
- Miss 2+ heartbeats → Degraded
- Miss 5+ heartbeats → Offline
- Jobs from offline workers are recovered

### 4. Scheduler (`src/scheduler/`)

Background task runner for maintenance operations.

| Task | Interval | Purpose |
|------|----------|---------|
| Activate scheduled jobs | 5s | Move scheduled→pending |
| Process cron schedules | 10s | Create jobs from cron |
| Recover expired leases | 30s | Handle worker failures |
| Update metrics | 15s | Refresh Prometheus gauges |
| Update job dependencies | 10s | Unblock child jobs when parents complete |
| Cleanup stale workers | 5m | Mark offline workers |
| Data retention | 5m | Delete old data |

### 5. Webhook Handler (`src/webhook/`)

Handles incoming and outgoing webhooks.

**Incoming (Ingestion):**
```
GitHub/Stripe/Custom → Signature Verify → Create Job → Queue
```

**Outgoing (Delivery):**
```
Job Complete → Webhook URL → Retry (exp backoff) → Record Result
```

### 6. gRPC Service (`src/grpc/`)

High-performance worker communication.

```protobuf
service QueueService {
  rpc Enqueue(EnqueueRequest) returns (EnqueueResponse);
  rpc Dequeue(DequeueRequest) returns (DequeueResponse);
  rpc Complete(CompleteRequest) returns (Empty);
  rpc Fail(FailRequest) returns (Empty);
  rpc RenewLease(RenewRequest) returns (RenewResponse);
}

service WorkerService {
  rpc Register(RegisterRequest) returns (RegisterResponse);
  rpc Heartbeat(HeartbeatRequest) returns (HeartbeatResponse);
}
```

---

## Data Flow

### Job Lifecycle

```
┌─────────┐    ┌───────────┐    ┌────────────┐    ┌───────────┐
│ Created │───▶│  Pending  │───▶│ Processing │───▶│ Completed │
└─────────┘    └─────┬─────┘    └─────┬──────┘    └─────┬─────┘
                     │                │                  │
                     │                │ (failure)        │ (unblock children)
                     │                ▼                  ▼
                     │          ┌──────────┐      ┌─────────────┐
                     │          │  Failed  │      │ Child Jobs  │
                     │          └────┬─────┘      │dependencies │
                     │               │            │    met      │
                     │    (retry)    │ (max retry)└─────────────┘
                     ◀───────────────┤
                                     ▼
                              ┌────────────┐
                              │ Deadletter │───▶ Child jobs cancelled
                              └────────────┘
```

### Job Dependencies (DAG Workflows)

Jobs can have parent-child dependencies. Child jobs are blocked until their parent completes:

1. When a parent job **completes**: Child jobs have `dependencies_met` set to `TRUE`
2. When a parent job **fails/deadletters/cancels**: Child jobs are automatically cancelled
3. The scheduler periodically checks for orphaned dependencies (safety net)

### Webhook Flow

```
┌──────────┐     ┌───────────────┐     ┌─────────┐     ┌──────────┐
│ External │────▶│ /webhooks/xxx │────▶│ Verify  │────▶│ Enqueue  │
│  Source  │     │    Endpoint   │     │Signature│     │   Job    │
└──────────┘     └───────────────┘     └─────────┘     └──────────┘
```

### Real-time Updates

```
┌────────────┐           ┌─────────────┐           ┌────────────┐
│   Client   │◀─────────▶│  WebSocket  │◀──────────│   Redis    │
│  (Browser) │   JSON    │   Handler   │   Pub/Sub │   Cache    │
└────────────┘           └─────────────┘           └────────────┘
                                                         ▲
                                                         │
┌────────────┐                                    ┌──────┴─────┐
│ Job Status │────────────────────────────────────▶│  Publish   │
│   Change   │                                    │   Event    │
└────────────┘                                    └────────────┘
```

---

## Database Design

### Entity Relationship

```
┌─────────────────┐     ┌─────────────────┐
│  organizations  │────▶│      jobs       │
└─────────────────┘     └────────┬────────┘
        │                        │
        │                        ▼
        │               ┌─────────────────┐
        │               │   job_history   │
        │               └─────────────────┘
        │
        ▼               ┌─────────────────┐
┌─────────────────┐     │job_dependencies │
│    api_keys     │     └─────────────────┘
└─────────────────┘              ▲
        │                        │
        ▼               ┌────────┴────────┐
┌─────────────────┐     │    workflows    │
│    workers      │     └─────────────────┘
└─────────────────┘
        │               ┌─────────────────┐
        ▼               │    schedules    │
┌─────────────────┐     └─────────────────┘
│  queue_config   │              │
└─────────────────┘              ▼
                        ┌─────────────────┐
                        │  schedule_runs  │
                        └─────────────────┘
```

### Key Tables

| Table | Purpose | Key Indexes |
|-------|---------|-------------|
| `jobs` | Main job storage | status, queue_name, scheduled_at, priority |
| `workers` | Worker registry | status, last_heartbeat |
| `organizations` | Multi-tenancy | slug (unique) |
| `api_keys` | Authentication | key_hash |
| `workflows` | DAG orchestration | status |
| `schedules` | Cron definitions | next_run_at, is_active |

### Row-Level Security

All tables use PostgreSQL RLS for tenant isolation:

```sql
-- Every table has this pattern
ALTER TABLE jobs ENABLE ROW LEVEL SECURITY;

CREATE POLICY jobs_org_isolation ON jobs
    FOR ALL
    USING (organization_id = current_setting('app.current_org_id', true));
```

---

## API Design

### REST Conventions

| Method | Path Pattern | Purpose |
|--------|--------------|---------|
| GET | /resources | List resources |
| POST | /resources | Create resource |
| GET | /resources/{id} | Get single resource |
| PUT | /resources/{id} | Update resource |
| DELETE | /resources/{id} | Delete resource |
| POST | /resources/{id}/action | Custom action |

### Authentication

```
Authorization: Bearer <api_key>
   or
Authorization: Bearer <jwt_token>
```

### Response Format

**Success:**
```json
{
  "id": "job-123",
  "status": "pending",
  "created_at": "2024-01-01T00:00:00Z"
}
```

**Error:**
```json
{
  "code": "validation_error",
  "message": "Invalid request",
  "details": {
    "queue_name": "required"
  }
}
```

### Pagination

Cursor-based pagination for large datasets:

```
GET /api/v1/jobs?cursor=eyJpZCI6Ijk5OSJ9&limit=50
```

Response includes next cursor:
```json
{
  "data": [...],
  "next_cursor": "eyJpZCI6IjEwNDkifQ==",
  "has_more": true
}
```

---

## Security Model

### Defense in Depth

```
┌─────────────────────────────────────────┐
│        1. Network Security              │
│   (TLS, Firewall, Network Policies)     │
├─────────────────────────────────────────┤
│        2. Rate Limiting                 │
│   (Per-IP, Per-API-Key, Per-Queue)     │
├─────────────────────────────────────────┤
│        3. Authentication                │
│   (API Keys, JWT, bcrypt hashing)      │
├─────────────────────────────────────────┤
│        4. Authorization                 │
│   (Row-Level Security, Queue ACLs)     │
├─────────────────────────────────────────┤
│        5. Input Validation              │
│   (Schema validation, Sanitization)    │
├─────────────────────────────────────────┤
│        6. Security Headers              │
│   (CSP, HSTS, X-Frame-Options, etc)    │
└─────────────────────────────────────────┘
```

### API Key Security

- Keys are hashed with bcrypt (cost=12)
- Prefix-based lookup for efficient validation
- Keys can have expiration dates, queue restrictions, and rate limit overrides
- Key cache invalidated immediately on revoke

### JWT Tokens

- Access tokens: 24 hour expiry
- Refresh tokens: 30 day expiry
- Blacklist stored in Redis
- Claims include org_id, queues, permissions

### Multi-Tenant Isolation

All operations are scoped to the authenticated organization via PostgreSQL Row-Level Security:

```sql
CREATE POLICY org_isolation ON jobs
    FOR ALL USING (organization_id = current_setting('app.current_org_id'));
```

**Isolation enforced at:**
- Database level (RLS policies on all tables)
- API handlers (org context required)
- Cache keys (org-prefixed namespaces)
- Redis pub/sub (org-scoped channels)
- gRPC operations (uses auth context, ignores client-provided org_id)

### Request Limits

| Resource | Limit |
|----------|-------|
| Request body | 5MB |
| Job payload | 1MB |
| gRPC payload | 1MB |
| Webhook payload | 5MB |
| List page size | 100 |
| Bulk enqueue | 100 jobs |

### Webhook Security

- GitHub: HMAC-SHA256 signature verification
- Stripe: Signature + timestamp validation (5 min max age)
- Custom: Optional X-Webhook-Token header
- Outbound: HTTPS required in production (SSRF protection)

---

## Scalability Patterns

### Horizontal Scaling

```
                    ┌─────────────────┐
                    │  Load Balancer  │
                    └────────┬────────┘
           ┌─────────────────┼─────────────────┐
           ▼                 ▼                 ▼
    ┌─────────────┐   ┌─────────────┐   ┌─────────────┐
    │  Backend 1  │   │  Backend 2  │   │  Backend N  │
    └──────┬──────┘   └──────┬──────┘   └──────┬──────┘
           │                 │                 │
           └─────────────────┴─────────────────┘
                             │
              ┌──────────────┼──────────────┐
              ▼              ▼              ▼
       ┌──────────┐   ┌──────────┐   ┌──────────┐
       │PostgreSQL│   │PgBouncer │   │  Redis   │
       │ Primary  │   │ (Pool)   │   │ Cluster  │
       └──────────┘   └──────────┘   └──────────┘
```

### Connection Pooling

- PgBouncer in transaction mode
- Each backend: 25 connections to bouncer
- Bouncer: 100 connections to PostgreSQL
- Result: 10 backends × 25 = 250 virtual connections

### Caching Strategy

| Data | Cache Location | TTL |
|------|----------------|-----|
| API Keys | Redis | 1 hour |
| Queue Config | Redis | 5 minutes |
| Job Status (hot) | Redis | 30 seconds |
| Rate Limit Counters | Redis | Per window |

### Queue Partitioning

For high-volume queues, consider partitioning by:
- Time (daily/weekly partitions)
- Organization (separate tables)
- Queue name (sharding)

---

## Performance Characteristics

### Benchmarks (Reference)

| Operation | P50 | P95 | P99 |
|-----------|-----|-----|-----|
| Health Check | 1ms | 5ms | 10ms |
| Job Enqueue | 5ms | 20ms | 50ms |
| Job Dequeue | 10ms | 30ms | 100ms |
| Job Complete | 5ms | 15ms | 30ms |
| List Jobs (100) | 20ms | 50ms | 100ms |

### Capacity Planning

| Component | 1K req/s | 10K req/s | 100K req/s |
|-----------|----------|-----------|------------|
| Backend Pods | 2 | 10 | 50 |
| PostgreSQL | 1 primary | 1 primary + 2 read | Cluster |
| Redis | 1 node | Sentinel | Cluster |
| PgBouncer | 1 | 2 | 4 |

---

## Further Reading

- [Deployment Guide](./deployment.md)
- [API Reference](../openapi.yaml)
- [Load Testing](../../loadtest/README.md)

