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
- **Observable**: operator-level Prometheus metrics and Grafana dashboards for self-hosters, optional OpenTelemetry export (`--features otel`). There is no tenant-facing Prometheus endpoint or alerting feature
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

## 🚀 Try it in 2 minutes

Needs only Docker (with Compose v2), `curl` and `jq`. This runs the published backend and dashboard images with PostgreSQL and Redis on your machine, with no configuration:

```bash
curl -O https://raw.githubusercontent.com/spooled-cloud/spooled-backend/main/docker-compose.quickstart.yml
docker compose -f docker-compose.quickstart.yml up -d --wait

curl http://localhost:8080/health
# {"status":"healthy","database":true,"cache":true,"grpc":true,"scheduler":true}
```

Create an organization. The response holds your first API key in `api_key.key`; it is shown only once:

```bash
RESPONSE=$(curl -s -X POST http://localhost:8080/api/v1/organizations \
  -H "Content-Type: application/json" \
  -d '{"name": "My Company", "slug": "my-company"}')
API_KEY=$(echo "$RESPONSE" | jq -r '.api_key.key')
echo "$API_KEY"   # sp_test_... (sp_live_... when RUST_ENV=production)
```

Enqueue a job, claim it as a worker, and complete it:

```bash
# Enqueue -> {"id":"...","created":true}
curl -s -X POST http://localhost:8080/api/v1/jobs \
  -H "Authorization: Bearer $API_KEY" -H "Content-Type: application/json" \
  -d '{"queue_name": "emails", "payload": {"to": "user@example.com"}}'

# Claim -> {"jobs":[{"id":"...","lease_id":"...","lease_expires_at":"...", ...}]}
CLAIM=$(curl -s -X POST http://localhost:8080/api/v1/jobs/claim \
  -H "Authorization: Bearer $API_KEY" -H "Content-Type: application/json" \
  -d '{"queue_name": "emails", "worker_id": "worker-1", "limit": 1}')
JOB_ID=$(echo "$CLAIM" | jq -r '.jobs[0].id')
LEASE_ID=$(echo "$CLAIM" | jq -r '.jobs[0].lease_id')

# Complete -> {"success":true}
curl -s -X POST "http://localhost:8080/api/v1/jobs/$JOB_ID/complete" \
  -H "Authorization: Bearer $API_KEY" -H "Content-Type: application/json" \
  -d "{\"worker_id\": \"worker-1\", \"lease_id\": \"$LEASE_ID\", \"result\": {\"sent\": true}}"
```

Open the dashboard at **http://localhost:4321** and sign in with the API key.

| Port | Service | Change it with |
|------|---------|----------------|
| `8080` | REST API | `SPOOLED_HTTP_PORT` |
| `50051` | gRPC API (no TLS: `grpcurl -plaintext`) | `SPOOLED_GRPC_PORT` |
| `4321` | Dashboard | `SPOOLED_DASHBOARD_PORT` |

Ports bind to `127.0.0.1`. If one is taken, override it, for example `SPOOLED_HTTP_PORT=18080 docker compose -f docker-compose.quickstart.yml up -d --wait` (the dashboard follows the API port automatically).

This stack is for trying Spooled locally, not for production: it runs with `RUST_ENV=development`, fixed local credentials, and the `console` email provider (email login codes are printed by `docker compose -f docker-compose.quickstart.yml logs backend`). New organizations start on the `enterprise` plan (`DEFAULT_PLAN_TIER`) so plan limits do not get in the way. Backend images up to `0.1.130` ignore `DEFAULT_PLAN_TIER` and put new organizations on the Free plan (2 queues, 1 worker, no workflows); see [Lifting plan limits](#lifting-plan-limits-self-hosted). Stop and delete everything with `docker compose -f docker-compose.quickstart.yml down -v`.

## 🐳 Self-Hosting with Docker Compose (production)

`docker-compose.prod.yml` runs the backend with PostgreSQL, Redis, an auto-generated gRPC TLS certificate, and optional Prometheus, Grafana and Cloudflare Tunnel. It is self-contained: a single downloaded file works (no `docker/` or `certs/` paths needed).

> **Production gRPC TLS:** images from `v0.1.99` onward do not bake a historical `certs/grpc-key.pem` into the image. The production Compose stack automatically generates a private self-signed origin certificate in a Docker volume and renews it before expiration. No certificate setup is required. See [SECURITY.md](SECURITY.md#unresolved-operator-actions).

```bash
mkdir spooled && cd spooled
curl -O https://raw.githubusercontent.com/spooled-cloud/spooled-backend/main/docker-compose.prod.yml

cat > .env << EOF
# --- Required ---
POSTGRES_PASSWORD=$(openssl rand -hex 16)
JWT_SECRET=$(openssl rand -hex 32)
RUST_ENV=production
# How login codes are emailed: smtp, resend, sendgrid or postmark
# (the backend refuses to start with the default "console" in production)
EMAIL_PROVIDER=smtp
SMTP_HOST=smtp.example.com
SMTP_PORT=587
SMTP_USERNAME=replace-with-smtp-user
SMTP_PASSWORD=replace-with-smtp-password
EMAIL_FROM_ADDRESS=noreply@example.com
# Or instead of SMTP_*: EMAIL_PROVIDER=resend and EMAIL_API_KEY=re_...

# --- Recommended for a private install ---
# New organizations start on this plan (default free: 2 queues, 1 worker, no workflows)
DEFAULT_PLAN_TIER=enterprise
# Enables /api/v1/admin/* (sent as the X-Admin-Key header; not an API key)
ADMIN_API_KEY=$(openssl rand -hex 32)
EOF

# Backend + PostgreSQL + Redis (+ the gRPC certificate helper it depends on)
docker compose -f docker-compose.prod.yml up -d --wait db redis backend

curl http://localhost:8080/health
```

Then create your organization and first API key exactly as in [Try it in 2 minutes](#-try-it-in-2-minutes) (the key starts with `sp_live_`).

- **Secrets:** use `openssl rand -hex`, not `-base64`: a base64 value can contain `/` or `+`, which breaks the `DATABASE_URL` that Compose builds from `POSTGRES_PASSWORD`. `JWT_SECRET` must be at least 32 characters and must not contain words such as `secret`, `test` or `password`, or the backend refuses to start. Compose stops with an error if `POSTGRES_PASSWORD` or `JWT_SECRET` is missing. Do not copy `.env.example` here: it is the local development file (`RUST_ENV=development`).
- **Email:** only checked at startup for "not `console`". Wrong SMTP or API-key values show up when someone asks for a login code (`docker compose -f docker-compose.prod.yml logs backend`). API keys work without any email setup.
- **Host ports:** REST `127.0.0.1:8080` and gRPC `127.0.0.1:50051` (TLS). PostgreSQL is also published on `127.0.0.1:5432` for SSH tunnels. Change them with `SPOOLED_HTTP_PORT`, `SPOOLED_GRPC_PORT` and `POSTGRES_HOST_PORT`; set `SPOOLED_BIND_ADDRESS=0.0.0.0` to expose the API ports on every interface instead of putting your own reverse proxy in front. Redis is never published.
- **Monitoring:** add `prometheus grafana` to the `up` command. They are not published on the host; reach Grafana through a tunnel or proxy (`grafana:3000` on the compose network).
- **Cloudflare Tunnel:** set `CLOUDFLARE_TUNNEL_TOKEN`, configure the routes listed at the top of `docker-compose.prod.yml`, and run `docker compose -f docker-compose.prod.yml up -d` (all services). Without a token, leave `cloudflared` out as above.
- **CORS:** if you serve the dashboard or your own browser app from another origin, list it in `CORS_ALLOWED_ORIGINS` (production only allows the listed origins).

### gRPC on the production stack

The production stack always serves gRPC over TLS with a self-signed certificate (`CN=backend`), so `grpcurl -plaintext` cannot connect. Either skip verification:

```bash
grpcurl -insecure localhost:50051 list
```

or verify against the generated certificate:

```bash
docker compose -f docker-compose.prod.yml cp backend:/run/secrets/grpc-cert.pem ./grpc-cert.pem
grpcurl -cacert grpc-cert.pem -servername backend localhost:50051 list
```

### Portainer

Create a **Git** stack from this repository (compose file `docker-compose.prod.yml` at repo root) and set the environment variables from the `.env` above in the stack environment. Then use **Pull and redeploy** like any other stack.

The stack:
- generates and stores its gRPC origin certificate automatically (helper stays **Up / healthy**, not Exited 0)
- self-bootstraps Prometheus scrape config + Grafana Prometheus datasource on start
- needs no host shell access, PEM upload, Docker Swarm secret, or busybox init containers

By default the backend follows `ghcr.io/spooled-cloud/spooled-backend:latest` and pulls on each deployment (`pull_policy: always`). **That is the intentional path** for Portainer **Pull and redeploy**. Set `BACKEND_IMAGE` to a version tag or digest only for a temporary rollback.

Portainer details (Git stacks, ephemeral checkouts, recreate): [docs/guides/production-host-portainer.md](docs/guides/production-host-portainer.md).

### Environment Variables

| Variable | Required | Default | Description |
|----------|----------|---------|-------------|
| `DATABASE_URL` | ✅ | - | PostgreSQL connection string |
| `JWT_SECRET` | ✅ | - | 32+ char secret for JWT signing |
| `ADMIN_API_KEY` | ❌ | - | Value of the `X-Admin-Key` header for `/api/v1/admin/*` and for creating organizations when `REGISTRATION_MODE=closed`. Not an API key; unset = admin API off |
| `DEFAULT_PLAN_TIER` | ❌ | `free` | Plan of every new organization (`free`/`starter`/`pro`/`enterprise`; an unknown value stops startup). Self-hosted organizations are **not** limit-free by default: set `enterprise` for a private install. Images up to `0.1.130` ignore it, see [Lifting plan limits](#lifting-plan-limits-self-hosted) |
| `REDIS_URL` | ❌ | `redis://localhost:6379` | Redis for pub/sub & caching |
| `RUST_ENV` | ❌ | `production` | `development`/`staging`/`production` (unset/unknown values fail safe to production) |
| `REGISTRATION_MODE` | ❌ | `open` | `open`/`closed` - controls public registration |
| `PORT` | ❌ | `8080` | REST API server port |
| `GRPC_PORT` | ❌ | `50051` | gRPC API server port |
| `GRPC_TLS_ENABLED` | ❌ | `false` | Enable TLS for gRPC (required for Cloudflare Tunnel). The default is `false` in every `RUST_ENV`; `docker-compose.prod.yml` sets it to `true` |
| `GRPC_TLS_CERT_PATH` | ❌ | (Compose sets `/run/secrets/grpc-cert.pem`) | Path to TLS certificate (PEM); production Compose mounts the generated volume automatically |
| `GRPC_TLS_KEY_PATH` | ❌ | (Compose sets `/run/secrets/grpc-key.pem`) | Path to TLS private key (PEM); production Compose mounts the generated volume read-only |
| `METRICS_PORT` | ❌ | `9090` | Prometheus metrics port |
| `METRICS_TOKEN` | ❌ | - | If set, requires `Authorization: Bearer <token>` for `/metrics` |
| `TRUSTED_CLIENT_IP_HEADER` | ❌ | - | Edge header carrying the real client IP, believed from any caller once set. Unset: `CF-Connecting-IP`, then `X-Real-IP`, believed **only** when the TCP peer is a loopback/private/link-local address (a proxy of yours on the same host or network); any other caller is keyed on its own address. Production warns at boot while unset |
| `TRUSTED_PROXY_HOPS` | ❌ | `0` | Entries **your own** proxies append to `X-Forwarded-For`, counted from the right. `0` = never consult that header |
| `OUTGOING_WEBHOOK_MAX_ATTEMPTS` | ❌ | `5` | Delivery attempts per outgoing webhook event |
| `OUTGOING_WEBHOOK_MAX_CONCURRENT_DELIVERIES` | ❌ | `64` | Webhook delivery senders and request slots, process-wide; also sizes the bounded delivery queue (16 per sender, drops are counted) |
| `TURNSTILE_SECRET_KEY` | ❌ | - | Cloudflare Turnstile secret. When set, `POST /auth/email/start` requires a valid `turnstile_token` (`400 CAPTCHA_FAILED` otherwise). Leave unset until the site and the dashboard both send the token |
| `FRONTEND_URL` | ❌ | `https://spooled.cloud` | Website base URL Stripe Checkout returns to (`<FRONTEND_URL>/account/billing?checkout=success\|cancel`) |
| `EMAIL_PROVIDER` | ✅ in production | `console` | `resend`/`sendgrid`/`postmark`/`smtp`. The backend refuses to start with `console` when `RUST_ENV=production` (login codes would only be logged) |
| `EMAIL_API_KEY` | with `resend`/`sendgrid`/`postmark` | - | Provider API key |
| `SMTP_HOST` / `SMTP_PORT` / `SMTP_USERNAME` / `SMTP_PASSWORD` | with `smtp` | - / `587` / - / - | SMTP relay (STARTTLS on `SMTP_PORT`) |
| `EMAIL_FROM_ADDRESS` / `EMAIL_FROM_NAME` | ❌ | `noreply@spooled.cloud` / `Spooled Cloud` | Sender of login-code emails |
| `RATE_LIMIT_FAIL_CLOSED` | ❌ | `false` | `true` rejects requests while Redis cannot answer instead of letting them through |
| `CORS_ALLOWED_ORIGINS` | ❌ | spooled.cloud origins | Comma-separated allowed origins; blank keeps the default list |
| `EXTERNAL_URL` | ❌ | - | Public base URL of the API |
| `ENABLE_HSTS` | ❌ | `false` | Send `Strict-Transport-Security` (only behind HTTPS) |
| `DATABASE_DIRECT_URL` | ❌ | - | Direct (non-PgBouncer) Postgres URL; migrations run over it when set |
| `DATABASE_LIST_STATEMENT_TIMEOUT_MS` | ❌ | `5000` | Statement timeout of `GET /jobs` and `GET /jobs/dlq` in milliseconds; a list it stops answers `400 QUERY_TIMEOUT`. `0` is refused (Postgres would read it as no timeout) |
| `WORKER_LEASE_DURATION_SECS` | ❌ | `30` | Default job lease for a REST claim that sends none |
| `GRPC_DEFAULT_LEASE_DURATION_SECS` | ❌ | `300` | Lease for gRPC claims and renewals that send none (`0` = 300, at most 3600, must be greater than `WORKER_HEARTBEAT_INTERVAL_SECS`); reported by gRPC `Register` |

A blank value counts as unset, so compose `${NAME:-}` forwarding is safe. `docker-compose.prod.yml` forwards all of the above (it builds `DATABASE_URL` and `REDIS_URL` itself) plus the `SPOOLED_PLAN_LIMITS_JSON` / `SPOOLED_PLAN_<TIER>_LIMITS_JSON` overrides below.

Host ports of the Compose files (they never change the ports inside the containers):

| Variable | Default | Used by |
|----------|---------|---------|
| `SPOOLED_BIND_ADDRESS` | `127.0.0.1` | `docker-compose.quickstart.yml`, `docker-compose.prod.yml` (API ports) |
| `SPOOLED_HTTP_PORT` | `8080` | quickstart, prod |
| `SPOOLED_GRPC_PORT` | `50051` | quickstart, prod |
| `SPOOLED_DASHBOARD_PORT` | `4321` | quickstart |
| `POSTGRES_HOST_PORT` | `5432` (prod, on `127.0.0.1`), `5433` (dev) | prod, `docker-compose.yml` |
| `PGBOUNCER_HOST_PORT` / `REDIS_HOST_PORT` | `6432` / `6379` | `docker-compose.yml` |
| `PROMETHEUS_HOST_PORT` / `GRAFANA_HOST_PORT` | `9091` / `3000` | `docker-compose.yml` |

The leftmost `X-Forwarded-For` entry is whatever the caller sent, so it is never trusted for rate limiting: set `TRUSTED_CLIENT_IP_HEADER` or `TRUSTED_PROXY_HOPS` if you terminate behind a proxy that sets neither `CF-Connecting-IP` nor `X-Real-IP`. `CF-Connecting-IP` and `X-Real-IP` are likewise believed only from a private TCP peer unless `TRUSTED_CLIENT_IP_HEADER` names the header, so a proxy that reaches the backend from a public address must set it. An IPv6 client is rate-limited per /64. Details in [docs/guides/operations.md](docs/guides/operations.md).

### Lifting plan limits (self-hosted)

Every organization has a plan (`free`, `starter`, `pro` or `enterprise`, limits in [Plan Limits & Tiers](#-plan-limits--tiers)). A self-hosted organization is **not** limit-free by default: it starts on `DEFAULT_PLAN_TIER`, which defaults to `free` (2 queues, 1 worker, no workflows, 5 requests/second). For a private install:

- **New organizations:** set `DEFAULT_PLAN_TIER=enterprise` (unlimited counts, 1 MB payloads, 500 requests/second). Backend images up to `0.1.130` ignore this setting.
- **An existing organization (any version):** set `ADMIN_API_KEY`, then move it to another plan:

  ```bash
  curl -X PATCH "http://localhost:8080/api/v1/admin/organizations/$ORG_ID" \
    -H "X-Admin-Key: $ADMIN_API_KEY" \
    -H "Content-Type: application/json" \
    -d '{"plan_tier": "enterprise"}'
  ```

  `ORG_ID` is `organization.id` from the `POST /api/v1/organizations` response (or `GET /api/v1/admin/organizations`).
- **Change what a tier allows:** the environment variables below.

### Plan Limits via Environment Variables (Self-Hosted)

Spooled ships with built-in plan defaults (Free/Starter/Pro/Enterprise), but **you can override every plan limit via env vars**. `docker-compose.prod.yml` and `docker-compose.quickstart.yml` forward the JSON variables (`SPOOLED_PLAN_LIMITS_JSON`, `SPOOLED_PLAN_<TIER>_LIMITS_JSON`); the per-field variables need your own `environment:` entries (or `cargo run` with them in `.env`).

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

Build and run the backend from source against PostgreSQL, PgBouncer and Redis in Docker. This is the one local development path; [CONTRIBUTING.md](CONTRIBUTING.md) and the [Quick Start Guide](docs/guides/quickstart.md) use the same steps.

### Prerequisites

- Rust 1.96+
- Docker with Compose v2

### Setup

```bash
git clone https://github.com/spooled-cloud/spooled-backend.git
cd spooled-backend

# Local settings: RUST_ENV=development, console email, URLs that match docker-compose.yml
cp .env.example .env

# PostgreSQL on localhost:5433, PgBouncer on localhost:6432, Redis on localhost:6379
docker compose up -d --wait postgres redis pgbouncer

# Runs migrations, then serves REST on :8080, gRPC on :50051 (no TLS), metrics on :9090
cargo run

curl http://localhost:8080/health
```

`cargo run` reads `.env` (`DATABASE_URL` through PgBouncer, `DATABASE_DIRECT_URL` straight to Postgres for migrations). If a host port is taken, start Compose with `POSTGRES_HOST_PORT`, `PGBOUNCER_HOST_PORT` or `REDIS_HOST_PORT` and change the matching URL in `.env`. If port 8080 is taken, set `PORT` in `.env`.

`docker compose up -d --wait` without service names also starts Prometheus (http://localhost:9091, scraping the backend on the host's `:9090`) and Grafana (http://localhost:3000, admin/admin, with the provisioned "Spooled Backend Overview" dashboard).

Then create an organization and run jobs as in [Try it in 2 minutes](#-try-it-in-2-minutes). New organizations start on the Free plan; see [Lifting plan limits](#lifting-plan-limits-self-hosted).

### Tests

```bash
cargo test
```

Integration tests start their own PostgreSQL and Redis containers (testcontainers), so Docker must be running; see [CONTRIBUTING.md](CONTRIBUTING.md#testing).

### API Endpoints

#### Core Job Management

| Method | Endpoint | Description |
|--------|----------|-------------|
| `GET` | `/health` | Health check |
| `POST` | `/api/v1/jobs` | Create a job (enforces plan limits) |
| `GET` | `/api/v1/jobs` | List jobs (filters: `status` list, `q`, `job_type`, `created_after`, `created_before`; `X-Has-More` header) |
| `GET` | `/api/v1/jobs/{id}/history` | A job's recorded lifecycle events |
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
| `GET` | `/api/v1/webhooks/deliveries` | Inbound webhook log (accepted requests, newest first) |

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
| `POST` | `/api/v1/auth/email/start` | Start email-based login (3/min and 10/hour per client IP; optional `turnstile_token`) |
| `POST` | `/api/v1/auth/email/verify` | Verify email login code (429 with `Retry-After` when throttled) |

#### Billing

| Method | Endpoint | Description |
|--------|----------|-------------|
| `GET` | `/api/v1/billing/status` | Get billing status (unrestricted API key) |
| `POST` | `/api/v1/billing/checkout` | Create a Stripe Checkout session for `starter` or `pro`, bound to the organization (unrestricted API key) |
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
# {"organization":{"id":"...","plan_tier":"free",...},"api_key":{"id":"...","key":"sp_live_...","name":"Initial Admin Key","created_at":"..."}}
# The key is only shown once!
API_KEY=$(echo "$RESPONSE" | jq -r '.api_key.key')
ORG_ID=$(echo "$RESPONSE" | jq -r '.organization.id')

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

Each job has a `key` that `depends_on` refers to. Workflows are not available on the Free plan (`403 FEATURE_DISABLED`); see [Lifting plan limits](#lifting-plan-limits-self-hosted).

```bash
# User onboarding: create account → send email → setup defaults
curl -X POST http://localhost:8080/api/v1/workflows \
  -H "Authorization: Bearer $API_KEY" \
  -H "Content-Type: application/json" \
  -d '{
    "name": "user-onboarding",
    "jobs": [
      {
        "key": "create-account",
        "queue_name": "users",
        "payload": {"email": "user@example.com"}
      },
      {
        "key": "send-welcome",
        "queue_name": "emails",
        "depends_on": ["create-account"],
        "payload": {"template": "welcome"}
      },
      {
        "key": "setup-defaults",
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
    "format": "slack"
  }'
```

`format` is `json` (the default: the signed event envelope), `slack`, `discord` or `teams`; a chat format sends one short message so the chat service's incoming-webhook URL works as it is. Events: `job.created`, `job.started`, `job.completed`, `job.failed`, `job.cancelled`, `job.deadlettered`, `job.retried`, `queue.paused`, `queue.resumed`, `worker.registered`, `worker.deregistered`, `schedule.triggered`, `workflow.completed` and `workflow.failed` (the two workflow events are announced at most once, see [docs/guides/webhooks.md](docs/guides/webhooks.md)).

Deliveries retry with exponential backoff (`OUTGOING_WEBHOOK_MAX_ATTEMPTS`, default 5). After 20 consecutive failed deliveries the webhook is disabled automatically (`enabled: false`, `last_status: "auto_disabled"`) and receives no further events until you re-enable it with `PUT /api/v1/outgoing-webhooks/{id}` and `{"enabled": true}`. See [docs/guides/webhooks.md](docs/guides/webhooks.md).

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

Outside the production Compose stack gRPC TLS is off by default (`GRPC_TLS_ENABLED=false`), which is what `cargo run` and `docker-compose.quickstart.yml` use.

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

Use `-plaintext` against `cargo run` and `docker-compose.quickstart.yml` (gRPC without TLS). The production Compose stack serves gRPC over TLS with a self-signed certificate: replace `-plaintext` with `-insecure` (or `-cacert grpc-cert.pem -servername backend`, see [gRPC on the production stack](#grpc-on-the-production-stack)).

```bash
# Test with grpcurl (install: brew install grpcurl)
# List services (reflection enabled)
grpcurl -plaintext localhost:50051 list

# Enqueue a job
grpcurl -plaintext \
  -H "x-api-key: $API_KEY" \
  -d '{
    "queue_name": "emails",
    "payload": {"to": "user@example.com"},
    "priority": 0,
    "max_retries": 3
  }' \
  localhost:50051 spooled.v1.QueueService/Enqueue

# Dequeue jobs
grpcurl -plaintext \
  -H "x-api-key: $API_KEY" \
  -d '{"queue_name": "emails", "worker_id": "worker-1", "batch_size": 10}' \
  localhost:50051 spooled.v1.QueueService/Dequeue

# Stream jobs (server streaming)
grpcurl -plaintext \
  -H "x-api-key: $API_KEY" \
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

Built-in defaults from [`src/config/plans.rs`](src/config/plans.rs) (self-hosters can change them, see [Lifting plan limits](#lifting-plan-limits-self-hosted)):

| Tier | Active Jobs | Daily Jobs | Queues | Workers | API Keys | Webhooks | Schedules | Workflows | Max Payload | Rate Limit (req/s, burst) | Job / History Retention |
|------|-------------|------------|--------|---------|----------|----------|-----------|-----------|-------------|---------------------------|-------------------------|
| **Free** | 10 | 1,000 | 2 | 1 | 2 | 1 | 1 | 0 (disabled) | 64 KB | 5, 10 | 3 / 1 days |
| **Starter** | 500 | 10,000 | 10 | 5 | 5 | 5 | 10 | 5 | 256 KB | 25, 50 | 14 / 7 days |
| **Pro** | 5,000 | 100,000 | 50 | 25 | 25 | 20 | 50 | 25 | 1 MB | 100, 200 | 30 / 30 days |
| **Enterprise** | Unlimited | Unlimited | Unlimited | Unlimited | Unlimited | Unlimited | Unlimited | Unlimited | 1 MB | 500, 1000 | 90 / 90 days |

A new organization gets `DEFAULT_PLAN_TIER` (default `free`).

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

Prefer `PATCH /api/v1/admin/organizations/{id}`, which validates the object. The resource caps are `max_jobs_per_day`, `max_active_jobs`, `max_queues`, `max_workers`, `max_api_keys`, `max_schedules`, `max_workflows` and `max_webhooks`: a whole number, or `null` / `-1` for unlimited. The settings are `max_payload_size_bytes`, `rate_limit_requests_per_second`, `rate_limit_burst`, `job_retention_days` and `history_retention_days`: a whole number, or `null` for the plan default (retention is 0-36500 days). Unknown keys and any other value are a `400 VALIDATION_ERROR`. A value written directly in SQL that does not fit is ignored (the plan default applies).

## 🚀 Production Deployment

### Docker Compose (Recommended for Single Server)

See [Self-Hosting with Docker Compose](#-self-hosting-with-docker-compose-production) for the complete `.env` (required: `POSTGRES_PASSWORD`, `JWT_SECRET`, `RUST_ENV=production`, `EMAIL_PROVIDER` with its SMTP or API-key settings; recommended: `DEFAULT_PLAN_TIER=enterprise` for a private install), host ports, gRPC TLS and Cloudflare Tunnel.

### Kubernetes

The manifests run the backend, PgBouncer and Redis; bring your own PostgreSQL. The production overlay deploys into namespace `spooled` and prefixes every name with `prod-` (services `prod-pgbouncer:5432`, `prod-redis:6379`).

```bash
kubectl create namespace spooled --dry-run=client -o yaml | kubectl apply -f -

# Secrets: copy the template and fill in spooled-secrets (database-url via
# prod-pgbouncer:5432, database-direct-url, redis-url redis://prod-redis:6379,
# jwt-secret from `openssl rand -hex 32`, email-api-key) and pgbouncer-secrets
# (your PostgreSQL host, database, user, password)
cp k8s/base/secrets.yaml.template secrets.yaml
"${EDITOR:-vi}" secrets.yaml
kubectl apply -n spooled -f secrets.yaml

# Review k8s/overlays/production/kustomization.yaml first: image tag, ingress host,
# EMAIL_PROVIDER / EMAIL_FROM_ADDRESS (required with RUST_ENV=production) and
# DEFAULT_PLAN_TIER. Render it without a cluster: kubectl kustomize k8s/overlays/production
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

The backend serves Prometheus metrics on `METRICS_PORT` (default `9090`). With `cargo run` that is `http://localhost:9090/metrics`; the Compose stacks do not publish it, so scrape it from the compose network (`backend:9090`) or read it inside the container:

```bash
# cargo run (add -H "Authorization: Bearer $METRICS_TOKEN" when METRICS_TOKEN is set)
curl http://localhost:9090/metrics

# docker-compose.prod.yml
docker compose -f docker-compose.prod.yml exec backend curl -s http://localhost:9090/metrics

# Key metrics:
# spooled_jobs_pending             - Jobs waiting
# spooled_jobs_processing          - Jobs in progress
# spooled_jobs_deadlettered_total  - Jobs moved to the dead-letter queue (counter)
# spooled_job_duration_seconds     - Processing time histogram
# spooled_workers_healthy          - Healthy worker count
```

### Grafana Dashboard

- **Local development** (`docker compose up -d` with `docker-compose.yml`): http://localhost:3000 (admin/admin), Prometheus at http://localhost:9091. The provisioned **Spooled Backend Overview** dashboard shows job throughput, queue depth, latency and workers.
- **Production Compose:** start `prometheus grafana` too; Grafana is not published on the host (route `grafana:3000` through your tunnel or proxy) and set `GRAFANA_PASSWORD`. It comes with the Prometheus datasource but no dashboards; import `docker/grafana/dashboards/spooled-overview.json` from this repository.

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
- **Rate Limiting**: Per-organization token bucket by plan, plus an optional per-key limit in requests per minute, with Redis (fails closed when configured). Rejected requests get `429` with `Retry-After` (seconds); authenticated responses carry `X-RateLimit-Limit`, `X-RateLimit-Remaining`, `X-RateLimit-Reset` (Unix time) and `X-RateLimit-Scope`
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
