# Spooled Backend

[![License](https://img.shields.io/badge/License-Apache%202.0-blue.svg)](https://opensource.org/licenses/Apache-2.0)
[![Rust](https://img.shields.io/badge/rust-1.96%2B-orange.svg)](https://www.rust-lang.org/)
[![Docker](https://img.shields.io/badge/docker-ghcr.io-blue.svg)](https://ghcr.io)

**Job queue + worker coordination service built with Rust**

[**Live Demo (SpriteForge)**](https://example.spooled.cloud) • [Documentation](https://spooled.cloud/docs) • [Website](https://spooled.cloud)

Spooled is a high-performance, multi-tenant job queue system designed for reliability, observability, and horizontal scalability.

## ✨ Features

- **High Performance**: Built on Rust + Tokio + PostgreSQL with Redis-backed caching
- **Optimized gRPC**: HTTP/2 keepalive, TCP optimizations, connection pooling, and streaming
- **Multi-Tenant**: explicit per-organization scoping on every query
- **Observable**: Prometheus metrics, Grafana dashboards, optional OpenTelemetry export (`--features otel`)
- **Reliable**: At-least-once processing with leases + retries (use idempotency keys for exactly-once effects)
- **Real-Time**: WebSocket + SSE for live job/queue updates
- **Secure**: Bcrypt API keys with Redis caching, JWT auth, HMAC webhook verification
- **Scalable**: Stateless API nodes (Kubernetes-friendly) + DB-backed locking (`FOR UPDATE SKIP LOCKED`)
- **Scheduling**: Cron-based recurring jobs with timezone support
- **Workflows**: Job dependencies with DAG execution
- **Dual Protocol**: REST API (`:8080`) + real gRPC (`GRPC_PORT`, default `:50051`) with streaming support
- **Tier-Based Limits**: Automatic enforcement across all endpoints (HTTP, gRPC, workflows, schedules)
- **Dead Letter Queue**: Automatic retry and purge operations for failed jobs
- **Webhooks**: Outgoing webhook delivery with automatic retries and status tracking
- **Billing**: Stripe integration for subscriptions and usage tracking

## 🐳 Quick Start with Docker

Zero-touch init: production Compose generates gRPC TLS into a Docker volume, and
Prometheus/Grafana write their own scrape/datasource config on start. No busybox
one-shots, no host PEM files, no extra clone of `deploy/` paths.

### Pull and Run

> **Production gRPC TLS:** images from `v0.1.99` onward do not bake a historical `certs/grpc-key.pem` into the image. The production Compose stack automatically generates a private self-signed origin certificate in a Docker volume and renews it before expiration. No certificate setup is required. See [SECURITY.md](SECURITY.md#unresolved-operator-actions).

```bash
# Pull the multi-arch image (supports amd64 and arm64)
docker pull ghcr.io/spooled-cloud/spooled-backend:latest

# Minimal path: one compose file + .env (configs are embedded / self-bootstrapped)
curl -O https://raw.githubusercontent.com/spooled-cloud/spooled-backend/main/docker-compose.prod.yml
cat > .env << EOF
POSTGRES_PASSWORD=$(openssl rand -base64 16)
JWT_SECRET=$(openssl rand -base64 32)
RUST_ENV=production
# Required because docker-compose.prod.yml starts the cloudflared service.
# Set this to a real tunnel token from your secret manager; do not commit it.
CLOUDFLARE_TUNNEL_TOKEN=replace-with-cloudflare-tunnel-token
EOF

# Without Cloudflare Tunnel, start only the application services and expose
# REST/gRPC through your own ingress or compose override:
docker compose -f docker-compose.prod.yml up -d db redis backend prometheus grafana

# To use Cloudflare Tunnel, first replace the token placeholder and configure the
# routes documented in docker-compose.prod.yml, then start the full stack instead:
# docker compose -f docker-compose.prod.yml up -d

# Verify from inside the backend container (the production stack exposes REST via Cloudflare)
docker compose -f docker-compose.prod.yml exec backend curl -fsS http://localhost:8080/health
```

Prefer a full Git clone (or Portainer Git stack) when you also want local/dev
assets under `docker/prometheus` and `docker/grafana` (richer rules/dashboards).
Those paths are optional for production — `docker-compose.prod.yml` is self-contained.

### Portainer

Create a **Git** stack from this repository (compose file `docker-compose.prod.yml` at repo root) and set the required environment variables listed in that file. Then use **Pull and redeploy** like any other stack.

The stack:
- generates and stores its gRPC origin certificate automatically (helper stays **Up / healthy**, not Exited 0)
- self-bootstraps Prometheus scrape config + Grafana Prometheus datasource on start
- needs no host shell access, PEM upload, Docker Swarm secret, or busybox init containers

By default the backend follows `ghcr.io/spooled-cloud/spooled-backend:latest` and pulls on each deployment. **Set `BACKEND_IMAGE` to an immutable tag/digest** for controlled rollouts (required on shared production hosts).

On the Spooled production host, prefer durable WD `/opt/spooled/backend`. After Pull and redeploy, Portainer may delete `/data/compose/71/<sha>/` — run `scripts/heal-portainer-stack-files.sh` (installed as `/opt/spooled/backend/bin/heal-portainer-stack-files`). Full operator guide: [docs/guides/production-host-portainer.md](docs/guides/production-host-portainer.md).

### Environment Variables

| Variable | Required | Default | Description |
|----------|----------|---------|-------------|
| `DATABASE_URL` | ✅ | - | PostgreSQL connection string |
| `JWT_SECRET` | ✅ | - | 32+ char secret for JWT signing |
| `ADMIN_API_KEY` | ❌ | - | Key for admin portal access |
| `REDIS_URL` | ❌ | `redis://localhost:6379` | Redis for pub/sub & caching |
| `RUST_ENV` | ❌ | `production` | `development`/`staging`/`production` (unset/unknown values fail safe to production) |
| `REGISTRATION_MODE` | ❌ | `open` | `open`/`closed` - controls public registration |
| `PORT` | ❌ | `8080` | REST API server port |
| `GRPC_PORT` | ❌ | `50051` | gRPC API server port |
| `GRPC_TLS_ENABLED` | ❌ | `true` (prod) | Enable TLS for gRPC (required for Cloudflare Tunnel) |
| `GRPC_TLS_CERT_PATH` | ❌ | (Compose sets `/run/secrets/grpc-cert.pem`) | Path to TLS certificate (PEM); production Compose mounts the generated volume automatically |
| `GRPC_TLS_KEY_PATH` | ❌ | (Compose sets `/run/secrets/grpc-key.pem`) | Path to TLS private key (PEM); production Compose mounts the generated volume read-only |
| `METRICS_PORT` | ❌ | `9090` | Prometheus metrics port |
| `METRICS_TOKEN` | ❌ | - | If set, requires `Authorization: Bearer <token>` for `/metrics` |

### Plan Limits via Environment Variables (Self-Hosted)

Spooled ships with sensible built-in plan defaults (Free/Starter/Pro/Enterprise), but **you can override every plan limit via env vars**.

Limits resolution order (lowest → highest precedence):

- Built-in defaults
- `SPOOLED_PLAN_LIMITS_JSON` (global per-tier JSON map)
- `SPOOLED_PLAN_<TIER>_LIMITS_JSON` (tier-specific JSON)
- `SPOOLED_PLAN_<TIER>_<FIELD>` (tier-specific individual fields)
- Organization `custom_limits` (DB, per-org override)

#### JSON overrides

- **`SPOOLED_PLAN_LIMITS_JSON`**: JSON object mapping tier → overrides (same keys as `organizations.custom_limits`)
- **`SPOOLED_PLAN_FREE_LIMITS_JSON`**, **`SPOOLED_PLAN_STARTER_LIMITS_JSON`**, **`SPOOLED_PLAN_PRO_LIMITS_JSON`**, **`SPOOLED_PLAN_ENTERPRISE_LIMITS_JSON`**

Example:

```json
{
  "free": { "max_jobs_per_day": 5000, "max_payload_size_bytes": 131072 },
  "starter": { "max_active_jobs": 2000 },
  "enterprise": { "max_jobs_per_day": null }
}
```

Notes:
- For **optional** limits (like `max_jobs_per_day`), `null` means **unlimited**.

#### Per-field overrides

You can override individual fields per tier with env vars:

- **Limits (support `unlimited` / `none` / `null` / `-1`)**:
  - `SPOOLED_PLAN_<TIER>_MAX_JOBS_PER_DAY`
  - `SPOOLED_PLAN_<TIER>_MAX_ACTIVE_JOBS`
  - `SPOOLED_PLAN_<TIER>_MAX_QUEUES`
  - `SPOOLED_PLAN_<TIER>_MAX_WORKERS`
  - `SPOOLED_PLAN_<TIER>_MAX_API_KEYS`
  - `SPOOLED_PLAN_<TIER>_MAX_SCHEDULES`
  - `SPOOLED_PLAN_<TIER>_MAX_WORKFLOWS`
  - `SPOOLED_PLAN_<TIER>_MAX_WEBHOOKS`
- **Sizes / rates / retention**:
  - `SPOOLED_PLAN_<TIER>_MAX_PAYLOAD_SIZE_BYTES`
  - `SPOOLED_PLAN_<TIER>_RATE_LIMIT_RPS`
  - `SPOOLED_PLAN_<TIER>_RATE_LIMIT_BURST`
  - `SPOOLED_PLAN_<TIER>_JOB_RETENTION_DAYS`
  - `SPOOLED_PLAN_<TIER>_HISTORY_RETENTION_DAYS`

Where `<TIER>` is one of: `FREE`, `STARTER`, `PRO`, `ENTERPRISE`.

## 🔧 Local Development

### Prerequisites

- Rust 1.96+
- Docker & Docker Compose
- PostgreSQL 16+ (or use Docker)
- Redis 7+ (optional, for pub/sub)

### Setup

```bash
# Clone repository
git clone https://github.com/spooled-cloud/spooled-backend.git
cd spooled-backend

# Start dependencies
docker compose up -d postgres redis

# Run migrations and start server
cargo run

# Run tests
cargo test
```

### API Endpoints

#### Core Job Management

| Method | Endpoint | Description |
|--------|----------|-------------|
| `GET` | `/health` | Health check |
| `POST` | `/api/v1/jobs` | Create a job (enforces plan limits) |
| `GET` | `/api/v1/jobs` | List jobs |
| `POST` | `/api/v1/jobs/bulk` | Bulk enqueue jobs (enforces plan limits) |
| `POST` | `/api/v1/jobs/claim` | Claim (lease) jobs for worker processing |
| `POST` | `/api/v1/jobs/{id}/complete` | Mark a job completed (worker ack) |
| `POST` | `/api/v1/jobs/{id}/fail` | Mark a job failed (worker nack) |
| `POST` | `/api/v1/jobs/{id}/heartbeat` | Extend a job lease (long-running jobs) |
| `GET` | `/api/v1/jobs/stats` | Get job statistics |

#### Dead Letter Queue (DLQ)

| Method | Endpoint | Description |
|--------|----------|-------------|
| `GET` | `/api/v1/jobs/dlq` | List jobs in dead letter queue |
| `POST` | `/api/v1/jobs/dlq/retry` | Retry jobs from DLQ (enforces plan limits) |
| `POST` | `/api/v1/jobs/dlq/purge` | Purge jobs from DLQ |

#### Organizations

| Method | Endpoint | Description |
|--------|----------|-------------|
| `POST` | `/api/v1/organizations` | Create organization (returns initial API key) |
| `GET` | `/api/v1/organizations/usage` | Get plan usage and limits |
| `GET` | `/api/v1/organizations/check-slug` | Check if slug is available |
| `POST` | `/api/v1/organizations/generate-slug` | Generate unique slug from name |

#### Schedules & Workflows

| Method | Endpoint | Description |
|--------|----------|-------------|
| `POST` | `/api/v1/schedules` | Create cron schedule |
| `POST` | `/api/v1/schedules/{id}/trigger` | Manually trigger schedule (enforces plan limits) |
| `POST` | `/api/v1/workflows` | Create workflow/DAG (enforces plan limits) |

#### Webhooks

| Method | Endpoint | Description |
|--------|----------|-------------|
| `POST` | `/api/v1/outgoing-webhooks` | Configure outgoing notifications |
| `GET` | `/api/v1/outgoing-webhooks/{id}/deliveries` | Get delivery history |
| `POST` | `/api/v1/outgoing-webhooks/{id}/retry/{delivery_id}` | Retry failed delivery |
| `POST` | `/api/v1/webhooks/{org_id}/custom` | Incoming webhook (ingestion → creates jobs) |

#### Real-Time

| Method | Endpoint | Description |
|--------|----------|-------------|
| `GET` | `/api/v1/ws` | WebSocket for real-time |
| `GET` | `/api/v1/events` | SSE stream of all events |
| `GET` | `/api/v1/events/queues/{name}` | SSE stream of queue updates |
| `GET` | `/api/v1/events/jobs/{id}` | SSE stream of job updates |

#### Authentication

| Method | Endpoint | Description |
|--------|----------|-------------|
| `POST` | `/api/v1/auth/login` | Exchange API key for JWT |
| `POST` | `/api/v1/auth/refresh` | Refresh JWT token |
| `POST` | `/api/v1/auth/email/start` | Start email-based login |
| `POST` | `/api/v1/auth/email/verify` | Verify email login code |

#### Billing

| Method | Endpoint | Description |
|--------|----------|-------------|
| `GET` | `/api/v1/billing/status` | Get billing status |
| `POST` | `/api/v1/billing/portal` | Create Stripe customer portal session |

#### Admin API (requires `X-Admin-Key` header)

| Method | Endpoint | Description |
|--------|----------|-------------|
| `GET` | `/api/v1/admin/organizations` | List all organizations |
| `POST` | `/api/v1/admin/organizations` | Create organization with plan tier |
| `GET` | `/api/v1/admin/organizations/{id}` | Get organization details |
| `PATCH` | `/api/v1/admin/organizations/{id}` | Update organization (plan, status) |
| `DELETE` | `/api/v1/admin/organizations/{id}` | Delete organization (soft or hard) |
| `POST` | `/api/v1/admin/organizations/{id}/api-keys` | Create API key for organization |
| `POST` | `/api/v1/admin/organizations/{id}/reset-usage` | Reset daily usage counters |
| `GET` | `/api/v1/admin/stats` | Platform-wide statistics |
| `GET` | `/api/v1/admin/plans` | List available plans with limits |

### Quick Examples

#### Create Your First Job

```bash
# 1. Create an organization (returns initial API key - save it!)
RESPONSE=$(curl -s -X POST http://localhost:8080/api/v1/organizations \
  -H "Content-Type: application/json" \
  -d '{"name": "My Company", "slug": "my-company"}')
echo "$RESPONSE"
# Save the api_key from the response - it's only shown once!
API_KEY=$(echo "$RESPONSE" | jq -r '.api_key')

# 2. Create a job using the API key
curl -X POST http://localhost:8080/api/v1/jobs \
  -H "Content-Type: application/json" \
  -H "Authorization: Bearer $API_KEY" \
  -d '{
    "queue_name": "emails",
    "payload": {"to": "user@example.com", "subject": "Hello!"},
    "priority": 0,
    "max_retries": 3
  }'
```

#### Create a Cron Schedule (Recurring Jobs)

```bash
# Run daily sales report every day at 9 AM
curl -X POST http://localhost:8080/api/v1/schedules \
  -H "Authorization: Bearer $API_KEY" \
  -H "Content-Type: application/json" \
  -d '{
    "name": "daily-sales-report",
    "cron_expression": "0 0 9 * * *",
    "timezone": "America/New_York",
    "queue_name": "reports",
    "payload_template": {"report_type": "daily_sales"}
  }'
```

#### Create a Workflow (Job Dependencies)

```bash
# User onboarding: create account → send email → setup defaults
curl -X POST http://localhost:8080/api/v1/workflows \
  -H "Authorization: Bearer $API_KEY" \
  -H "Content-Type: application/json" \
  -d '{
    "name": "user-onboarding",
    "jobs": [
      {
        "name": "create-account",
        "queue_name": "users",
        "payload": {"email": "user@example.com"}
      },
      {
        "name": "send-welcome",
        "queue_name": "emails",
        "depends_on": ["create-account"],
        "payload": {"template": "welcome"}
      },
      {
        "name": "setup-defaults",
        "queue_name": "users",
        "depends_on": ["create-account"],
        "payload": {"settings": {}}
      }
    ]
  }'
```

#### Configure Outgoing Webhooks (Notifications)

```bash
# Get notified in Slack when jobs fail
curl -X POST http://localhost:8080/api/v1/outgoing-webhooks \
  -H "Authorization: Bearer $API_KEY" \
  -H "Content-Type: application/json" \
  -d '{
    "name": "Slack Alerts",
    "url": "https://hooks.slack.com/services/YOUR/WEBHOOK/URL",
    "events": ["job.failed", "queue.paused"],
    "secret": "your-hmac-secret"
  }'
```

## 🔌 gRPC API

Spooled provides a **real gRPC API** using HTTP/2 + Protobuf for high-performance worker communication.

### Endpoints

- **Spooled Cloud (TLS)**: `grpc.spooled.cloud:443`
- **Self-hosted / local**: `localhost:50051` (or whatever `GRPC_PORT` is set to)

### gRPC TLS (Cloudflare Tunnel)

When using Cloudflare Tunnel with HTTPS origin, gRPC TLS is **required** because HTTP/2 needs TLS at the origin.

The production docker-compose includes:
- **TLS enabled by default** (`GRPC_TLS_ENABLED=true`)
- **Automatic private self-signed origin cert** in Docker volume `grpc_tls` (generated/renewed by `grpc-tls-init`; not `./certs/` in Git or the image)
- **Performance Optimized**: HTTP/2 keepalives, TCP_NODELAY, and tuned connection windows

**Cloudflare Tunnel Configuration:**
- Service Type: `HTTPS`
- URL: `backend:50051`
- HTTP2 Connection: `ON`
- No TLS Verify: `ON` (required for self-signed certs)

> **Note**: Cloudflare Tunnel requires HTTPS for HTTP/2 (gRPC). You cannot use plaintext HTTP with gRPC through Cloudflare.

To disable TLS for local development (without Cloudflare):
```bash
GRPC_TLS_ENABLED=false cargo run
```

### Proto Definition

The service definitions are in [`proto/spooled.proto`](proto/spooled.proto):

```protobuf
service QueueService {
  rpc Enqueue(EnqueueRequest) returns (EnqueueResponse);
  rpc Dequeue(DequeueRequest) returns (DequeueResponse);
  rpc Complete(CompleteRequest) returns (CompleteResponse);
  rpc Fail(FailRequest) returns (FailResponse);
  rpc RenewLease(RenewLeaseRequest) returns (RenewLeaseResponse);
  rpc GetJob(GetJobRequest) returns (GetJobResponse);
  rpc GetQueueStats(GetQueueStatsRequest) returns (GetQueueStatsResponse);
  
  // Server-side streaming for continuous job delivery
  rpc StreamJobs(StreamJobsRequest) returns (stream Job);
  
  // Bidirectional streaming for real-time job processing
  rpc ProcessJobs(stream ProcessRequest) returns (stream ProcessResponse);
}

service WorkerService {
  rpc Register(RegisterWorkerRequest) returns (RegisterWorkerResponse);
  rpc Heartbeat(HeartbeatRequest) returns (HeartbeatResponse);
  rpc Deregister(DeregisterRequest) returns (DeregisterResponse);
}
```

### gRPC Features

- ⚡ **Efficient transport** with Protobuf, HTTP/2 multiplexing, and streaming
- 🛡️ **Automatic plan limit enforcement** on enqueue operations
- 📦 **Batch operations** for higher throughput
- 🔄 **Streaming support** for real-time job processing
- 🔐 **Secure authentication** via API key metadata (x-api-key header)

> **Note**: The default gRPC port is `50051`. If this port is in use (e.g., by Multipass on macOS), 
> set `GRPC_PORT=50052` or another available port. See [gRPC Server Guide](docs/guides/grpc-server.md) for details.

### gRPC Quick Start

```bash
# Test with grpcurl (install: brew install grpcurl)
# List services (reflection enabled)
grpcurl -plaintext localhost:50051 list

# Enqueue a job
grpcurl -plaintext \
  -H "x-api-key: sp_live_your_key" \
  -d '{
    "queue_name": "emails",
    "payload": {"to": "user@example.com"},
    "priority": 0,
    "max_retries": 3
  }' \
  localhost:50051 spooled.v1.QueueService/Enqueue

# Dequeue jobs
grpcurl -plaintext \
  -H "x-api-key: sp_live_your_key" \
  -d '{"queue_name": "emails", "worker_id": "worker-1", "batch_size": 10}' \
  localhost:50051 spooled.v1.QueueService/Dequeue

# Stream jobs (server streaming)
grpcurl -plaintext \
  -H "x-api-key: sp_live_your_key" \
  -d '{"queue_name": "emails", "worker_id": "worker-1", "lease_duration_secs": 300}' \
  localhost:50051 spooled.v1.QueueService/StreamJobs
```

### gRPC Features

| Feature | Description |
|---------|-------------|
| **Health Check** | Standard gRPC health protocol (`grpc.health.v1.Health`) |
| **Reflection** | Service discovery for debugging tools |
| **Streaming** | Server + bidirectional streaming for efficient workers |
| **Compression** | gzip compression supported |
| **Auth** | `x-api-key` or `authorization: Bearer` metadata |

### When to Use gRPC vs REST

| Use Case | Recommended |
|----------|-------------|
| Web/mobile apps | REST API |
| Dashboard/admin | REST API |
| High-throughput workers | gRPC |
| Streaming job delivery | gRPC StreamJobs |
| Language with gRPC SDK | gRPC |

## 💎 Plan Limits & Tiers

Spooled enforces tier-based limits automatically across all endpoints to prevent abuse and enable fair multi-tenancy.

### Available Tiers

| Tier | Active Jobs | Daily Jobs | Queues | Workers | Webhooks | Schedules | Workflows |
|------|-------------|------------|--------|---------|----------|-----------|-----------|
| **Free** | 10 | 1,000 | 5 | 3 | 2 | 5 | 2 |
| **Starter** | 100 | 100,000 | 25 | 25 | 10 | 25 | 10 |
| **Enterprise** | Unlimited | Unlimited | Unlimited | Unlimited | Unlimited | Unlimited | Unlimited |

### Limit Enforcement

Limits are **automatically enforced** on:

- ✅ **HTTP API**: `POST /jobs`, `POST /jobs/bulk`
- ✅ **gRPC API**: `Enqueue` operation
- ✅ **Workflows**: Counts all jobs in the workflow
- ✅ **Schedules**: When triggered (manual or automatic)
- ✅ **DLQ Retry**: When retrying jobs from dead letter queue
- ✅ **Workers**: Registration and concurrent operations
- ✅ **Queues**: Creation and configuration
- ✅ **Webhooks**: Creation and updates

### Limit Exceeded Response

When a limit is exceeded, the API returns `429 Too Many Requests`:

```json
{
  "error": "limit_exceeded",
  "code": "QUOTA_EXCEEDED",
  "message": "active jobs limit reached (10/10). Upgrade to starter for higher limits.",
  "resource": "active_jobs",
  "current": 10,
  "limit": 10,
  "plan": "free",
  "upgrade_to": "starter"
}
```

For gRPC, the status code is `RESOURCE_EXHAUSTED` with a descriptive message.

### Performance Characteristics

Actual latency and throughput depend on payload size, database capacity, network path,
and worker behavior. Use the checked-in `loadtest/` scenarios to establish a baseline
for your deployment. Prefer gRPC streaming for long-lived, high-throughput workers.

### Custom Limits

Enterprise customers can request custom limits via `custom_limits` in the database:

```sql
UPDATE organizations 
SET custom_limits = '{"max_active_jobs": 10000, "max_jobs_per_day": 1000000}'::jsonb
WHERE id = 'org-id';
```

## 🚀 Production Deployment

### Docker Compose (Recommended for Single Server)

```bash
# Download production compose file
curl -O https://raw.githubusercontent.com/spooled-cloud/spooled-backend/main/docker-compose.prod.yml

# Configure environment
cat > .env << EOF
POSTGRES_PASSWORD=$(openssl rand -base64 16)
JWT_SECRET=$(openssl rand -base64 32)
RUST_ENV=production
JSON_LOGS=true
EOF

# Deploy without the optional Cloudflare Tunnel sidecar
docker compose -f docker-compose.prod.yml up -d db pgbouncer redis backend prometheus grafana

# To start cloudflared too, add CLOUDFLARE_TUNNEL_TOKEN from your secret manager,
# configure the documented routes, and run the full-stack command instead.
# Prometheus and Grafana are included in the production stack
```

### Kubernetes

```bash
# Create the namespace and secret expected by the manifests
kubectl create namespace spooled --dry-run=client -o yaml | kubectl apply -f -
kubectl create secret generic spooled-secrets \
  --namespace spooled \
  --from-literal=database-url='postgres://user:pass@postgres:5432/spooled' \
  --from-literal=jwt-secret="$(openssl rand -base64 32)"

# Review the image tag and environment-specific values in the overlay, then deploy
kubectl apply -k k8s/overlays/production

# This repository currently ships Kustomize manifests; no Helm chart is included.
```

### ARM64 / Raspberry Pi / AWS Graviton

Images are automatically built for both `amd64` and `arm64`:

```bash
# Explicit platform selection
docker pull --platform linux/arm64 ghcr.io/spooled-cloud/spooled-backend:latest
```

## 📊 Monitoring

### Prometheus Metrics

```bash
curl -H "Authorization: Bearer $METRICS_TOKEN" http://localhost:9090/metrics

# Key metrics:
# spooled_jobs_pending       - Jobs waiting
# spooled_jobs_processing    - Jobs in progress
# spooled_job_duration_seconds - Processing time histogram
# spooled_workers_healthy    - Healthy worker count
```

### Grafana Dashboard

Access at http://localhost:3000 (admin/admin) when using `--profile monitoring`.

Pre-configured dashboards:
- **Spooled Overview**: Job throughput, queue depth, latency
- **Worker Status**: Health, capacity, distribution

### Distributed Tracing (Jaeger)

```bash
# Build with OpenTelemetry support
cargo build --features otel

# Run with tracing
OTEL_EXPORTER_OTLP_ENDPOINT=http://jaeger:4317 ./target/release/spooled-backend
```

## 🔒 Security

- **Authentication**: API keys (bcrypt hashed) or JWT tokens
- **Least privilege**: queue-scoped keys are enforced across REST, realtime, and gRPC operations; active streams revalidate current key state
- **Multi-tenancy**: explicit per-organization scoping on every query
- **Rate Limiting**: Per-key limits with Redis (fails closed when configured)
- **Webhooks**: HMAC-SHA256 signature verification
- **Input Validation**: All inputs sanitized and size-limited
- **SSRF Protection**: Webhook URLs validated in production

## 📚 Documentation

### Getting Started
- [Quick Start Guide](docs/guides/quickstart.md) — Get running in 5 minutes
- [Getting Started (Laravel users)](docs/guides/getting-started.md) — Familiar concepts for Laravel developers
- [Real-world examples](docs/guides/real-world-examples.md) — 5 beginner-friendly examples you can copy/paste

### Core Concepts
- [Jobs & Queues](docs/guides/jobs.md) — Job lifecycle, creation, and processing
- [Workers](docs/guides/workers.md) — Building production workers
- [Retries & DLQ](docs/guides/retries.md) — Retry configuration and dead letter queue
- [Webhooks](docs/guides/webhooks.md) — Incoming and outgoing webhooks

### Reference
- [API Usage Guide](docs/guides/api-usage.md) — Complete REST API reference
- [gRPC Server Guide](docs/guides/grpc-server.md) — High-performance gRPC API
- [SDKs](docs/guides/sdks.md) — Node.js, Python, Go, PHP SDKs
- [OpenAPI Spec](docs/openapi.yaml) — OpenAPI 3.1 specification

### Operations
- [Architecture](docs/guides/architecture.md) — System design and data flow
- [Deployment Guide](docs/guides/deployment.md) — Docker, Kubernetes, production checklist
- [Operations Guide](docs/guides/operations.md) — Monitoring, maintenance, troubleshooting

## 🏗️ Architecture

```
┌─────────────────────────────────────────────────────────────┐
│                     SPOOLED BACKEND                         │
├─────────────────────────────────────────────────────────────┤
│  REST API (Axum)  │  gRPC (Tonic)  │  WebSocket/SSE         │
├─────────────────────────────────────────────────────────────┤
│              Queue Manager (FOR UPDATE SKIP LOCKED)         │
│              Worker Coordination & Heartbeat                │
│              Scheduler (Cron, Dependencies, Retries)        │
├─────────────────────────────────────────────────────────────┤
│  PostgreSQL 16+   │   Redis 7+     │   Prometheus           │
│  (+ PgBouncer)    │   (Pub/Sub)    │   (Metrics)            │
└─────────────────────────────────────────────────────────────┘
```

## 🤝 Contributing

1. Fork the repository
2. Create a feature branch (`git checkout -b feature/amazing`)
3. Commit changes (`git commit -m 'Add amazing feature'`)
4. Push to branch (`git push origin feature/amazing`)
5. Open a Pull Request

Release contributors must keep the version in `Cargo.toml`, the root `spooled-backend` entry in `Cargo.lock`, and the matching `CHANGELOG.md` entry synchronized. Before tagging or deploying, follow the advisory [release and deployment evidence checklist](docs/guides/deployment.md#release-and-deployment-evidence-checklist). Exceptions may be recorded with an owner and rationale, but a Git tag, manifest, or lockfile version mismatch for the same artifact is a release error and must not be published.

## 📄 License

Apache License 2.0 - see [LICENSE](LICENSE) for details.

---

**Built with ❤️ in Rust**
