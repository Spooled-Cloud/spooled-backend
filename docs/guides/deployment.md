# Spooled Cloud Deployment Guide

This guide covers deploying Spooled Cloud in various environments.

## Table of Contents

1. [Deployment Options](#deployment-options)
2. [Managed Cloud](#managed-cloud)
3. [Prerequisites](#prerequisites)
4. [Local Development](#local-development)
5. [Docker Deployment](#docker-deployment)
6. [Production host + Portainer](#production-host--portainer)
7. [Kubernetes Deployment](#kubernetes-deployment)
8. [Release and Deployment Evidence Checklist](#release-and-deployment-evidence-checklist)
9. [Production Checklist](#production-checklist)
10. [Monitoring & Observability](#monitoring--observability)
11. [Scaling Guidelines](#scaling-guidelines)
12. [Troubleshooting](#troubleshooting)

---

## Deployment Options

| Option | Best For | Maintenance |
|--------|----------|-------------|
| **Managed Cloud** | Most teams | Zero maintenance |
| **Docker Compose** | Development, small deployments | Basic ops |
| **Kubernetes** | Large scale, air-gapped | Full ops team |
| **Bare Metal** | Maximum control | Expert ops |

---

## Managed Cloud

The easiest way to get started is with the managed service at [spooled.cloud](https://spooled.cloud).

**Benefits:**
- ✓ No infrastructure to manage
- ✓ Automatic scaling and updates
- ✓ Built-in monitoring and alerting
- ✓ 99.9% SLA (Pro and Enterprise plans)
- ✓ Security-focused architecture and operational monitoring

**Getting Started:**
1. Sign up at https://spooled.cloud/signup
2. Create an organization and get your API key
3. Start queuing jobs immediately

For teams that need self-hosting (air-gapped, data residency, etc.), continue to the sections below.

---

## Prerequisites

### Required Services

| Service | Version | Purpose |
|---------|---------|---------|
| PostgreSQL | 16+ | Primary database |
| Redis | 7+ | Caching, Pub/Sub, rate limiting |

### Recommended

- PgBouncer (connection pooling for high load)
- Prometheus + Grafana (monitoring)
- OpenTelemetry Collector (tracing)

---

## Local Development

### 1. Start Dependencies

```bash
# From the spooled-backend repository root, start the local stack
docker compose up -d

# Verify services
docker compose ps
```

This starts:
- **PostgreSQL 16** - Primary database (host port 5433)
- **PgBouncer** - Connection pooler (host port 6432)
- **Redis 7** - Cache and Pub/Sub (port 6379)
- **Prometheus** - Metrics collection (port 9091)
- **Grafana** - Dashboards (port 3000)


### 2. Configure Environment

```bash
# Configure the local backend process
cat > .env <<'EOF'
DATABASE_URL=postgres://spooled:spooled_password@localhost:5433/spooled
DATABASE_DIRECT_URL=postgres://spooled:spooled_password@localhost:5433/spooled
REDIS_URL=redis://localhost:6379
RUST_ENV=development
JWT_SECRET=development-secret-change-in-production
EOF
```

Required environment variables:

```env
# Database (the local backend process connects directly to the mapped PostgreSQL port)
DATABASE_URL=postgres://spooled:spooled_password@localhost:5433/spooled
DATABASE_DIRECT_URL=postgres://spooled:spooled_password@localhost:5433/spooled

# Redis
REDIS_URL=redis://localhost:6379

# Server
HOST=0.0.0.0
PORT=8080
METRICS_PORT=9090
RUST_ENV=development

# Security (CHANGE IN PRODUCTION!)
JWT_SECRET=your-secret-key-at-least-32-characters-long

# Tracing (optional)
OTEL_EXPORTER_OTLP_ENDPOINT=http://localhost:4317
OTEL_SERVICE_NAME=spooled-backend
JSON_LOGS=false
```

### 3. Run Migrations

```bash
# Install sqlx-cli if not present
cargo install sqlx-cli --no-default-features --features postgres

# Run migrations
sqlx migrate run
```

### 4. Start the Server

```bash
# Development mode with hot reload
cargo watch -x run

# Or standard run
cargo run
```

### 5. Verify

```bash
curl http://localhost:8080/health
# {"status":"healthy","database":true,"cache":true}

# Public /health intentionally omits the version. Verify a rollout through the
# authenticated GET /api/v1/dashboard response (system.version and uptime_seconds).
```

---

## Docker Deployment

### Build the Image

```bash
cd spooled-backend

# Build production image
docker build -t spooled-backend:latest .

# Or with the selected release tag
docker build -t spooled-backend:${RELEASE_TAG} .
```

### Run with Docker

```bash
docker run -d \
  --name spooled-backend \
  -p 8080:8080 \    # REST API
  -p 9090:9090 \    # Prometheus metrics
  -p 50051:50051 \  # gRPC API (HTTP/2 + Protobuf)
  -e DATABASE_URL="postgres://user:pass@host:5432/spooled" \
  -e REDIS_URL="redis://redis:6379" \
  -e JWT_SECRET="your-production-secret" \
  -e RUST_ENV="production" \
  spooled-backend:latest
```

**Ports:**

| Port | Protocol | Description |
|------|----------|-------------|
| 8080 | HTTP/1.1 | REST API, WebSocket, SSE |
| 9090 | HTTP/1.1 | Prometheus metrics |
| 50051 | HTTP/2 | gRPC API (Protobuf) |

### Docker Compose (Full Stack)

```yaml
# docker-compose.prod.yml
version: '3.8'

services:
  backend:
    image: spooled-backend:latest
    ports:
      - "8080:8080"
      - "9090:9090"
    environment:
      DATABASE_URL: postgres://postgres:${DB_PASSWORD}@db:5432/spooled
      REDIS_URL: redis://redis:6379
      JWT_SECRET: ${JWT_SECRET}
      RUST_ENV: production
    depends_on:
      - db
      - redis
    restart: unless-stopped
    healthcheck:
      test: ["CMD", "curl", "-f", "http://localhost:8080/health"]
      interval: 30s
      timeout: 10s
      retries: 3

  db:
    image: postgres:16-alpine
    volumes:
      - postgres_data:/var/lib/postgresql/data
    environment:
      POSTGRES_PASSWORD: ${DB_PASSWORD}
      POSTGRES_DB: spooled
    restart: unless-stopped

  redis:
    image: redis:7-alpine
    volumes:
      - redis_data:/data
    restart: unless-stopped

volumes:
  postgres_data:
  redis_data:
```

---

## Production host + Portainer

Preferred prod rollout: Portainer Git **Pull and redeploy** for stack `spooled-backend` (compose project name must match). Keep live WD on durable `/opt/spooled/backend`. Image policy is **`:latest`** (compose default + host `.env`). Portainer often deletes `/data/compose/71/<sha>/` after Pull — heal with `bin/heal-portainer-stack-files` (repo: `scripts/heal-portainer-stack-files.sh`) and re-adopt from durable if labels point at a missing path.

Canonical operator guide (isolation, Pull path, heal explanation, recreate, CLI):

→ **[production-host-portainer.md](./production-host-portainer.md)**

Summary:

| Item | Value |
|------|--------|
| Compose project / stack name | `spooled-backend` |
| Preferred UI action | Portainer **Pull and redeploy** |
| Image | `ghcr.io/spooled-cloud/spooled-backend:latest` |
| Live / durable path | `/opt/spooled/backend` |
| Post-Pull heal | `sudo /opt/spooled/backend/bin/heal-portainer-stack-files` |
| CLI fallback | `sudo /opt/spooled/backend/bin/deploy-backend` |

**Isolation:** never run compose/`docker rm` against other host projects (`authentik`, `outlinewiki`, dashboard, SpriteForge, etc.). Never remove volumes when deleting/recreating the Portainer stack.

### Zero-touch init (self-host / Portainer / single-file)

`docker-compose.prod.yml` is designed so newcomers do not run manual init containers:

| Concern | What happens on `up` |
|---------|----------------------|
| gRPC origin TLS | `grpc-tls-init` writes/renews certs into volume `grpc_tls`, then stays **healthy** (not Exited 0). CI uses `GRPC_TLS_INIT_ONCE=1` for a one-shot exit. |
| Prometheus | Service writes scrape config to `/tmp/prometheus.yml` then starts Prometheus. |
| Grafana | Service writes Prometheus datasource provisioning under its data volume then starts Grafana. |

No bind-mount of repo `deploy/` paths is required. A raw `curl` of the compose file + `.env` is enough. Full Git clones still get optional richer assets under `docker/prometheus` and `docker/grafana` for local/dev compose.

---

## Kubernetes Deployment

### Prerequisites

- Kubernetes cluster (1.25+)
- kubectl configured
- Helm 3 (optional, for dependencies)

### 1. Create Namespace

```bash
kubectl create namespace spooled
```

### 2. Create Secrets

```bash
kubectl create secret generic spooled-secrets \
  --namespace spooled \
  --from-literal=database-url='postgres://user:pass@postgres:5432/spooled' \
  --from-literal=redis-url='redis://redis:6379' \
  --from-literal=jwt-secret='your-production-secret'
```

### 3. Deploy with Kustomize

The current manifests keep `app.kubernetes.io/version` out of workload selectors so future version changes can roll out in place. Releases before `v0.1.97` used `commonLabels`, which placed version `1.0.0` in immutable Deployment selectors. Before upgrading a cluster originally created from those manifests, inspect the live selector:

```bash
kubectl get deployment prod-spooled-backend \
  --namespace spooled \
  --output jsonpath='{.spec.selector.matchLabels.app\.kubernetes\.io/version}{"\n"}'
```

If this prints `1.0.0`, a normal apply will fail because Kubernetes cannot remove an immutable selector. Record the current rollback manifest/image, schedule the brief backend interruption, then recreate only that Deployment before applying the complete overlay:

```bash
kubectl delete deployment prod-spooled-backend --namespace spooled
kubectl apply -k k8s/overlays/production
kubectl rollout status deployment/prod-spooled-backend --namespace spooled
```

Do not delete PostgreSQL, Redis, Services, Secrets, ConfigMaps, or persistent volumes for this migration. Clusters whose live Deployment selector has no version label can use an ordinary apply:

```bash
# Development
kubectl apply -k k8s/overlays/development

# Production
kubectl apply -k k8s/overlays/production
```

### 4. Verify Deployment

```bash
# Check pods
kubectl get pods -n spooled

# Check services
kubectl get svc -n spooled

# View logs
kubectl logs -f deployment/prod-spooled-backend -n spooled

# Port forward for testing
kubectl port-forward svc/prod-spooled-backend 8080:80 -n spooled
```

### Helm Chart (External Dependencies)

```bash
# PostgreSQL
helm install postgres bitnami/postgresql \
  --namespace spooled \
  --set auth.postgresPassword=your-password \
  --set auth.database=spooled

# Redis
helm install redis bitnami/redis \
  --namespace spooled \
  --set auth.enabled=false
```

---

## Release and Deployment Evidence Checklist

This checklist is advisory evidence tracking: an operator may mark an item `N/A` or record an exception, owner, and reason without treating the checklist itself as release authorization. A mismatch among version values that describe the **same artifact**, however, is a release error and must be corrected before publishing that artifact.

### Select and synchronize the release

- [ ] Record `RELEASE_VERSION`, `RELEASE_TAG=v${RELEASE_VERSION}`, the intended branch, and the release commit.
- [ ] Confirm the tag is unused locally and on `origin`, and that the release commit is clean and synced.
- [ ] Set `[package].version` in `Cargo.toml`; this is the authoritative source version and supplies Rust's compile-time `CARGO_PKG_VERSION`.
- [ ] Regenerate or update the root `spooled-backend` package entry in `Cargo.lock` so its version exactly matches `Cargo.toml`.
- [ ] Add the matching release entry to `CHANGELOG.md`.
- [ ] Update the version-bearing `app.kubernetes.io/version` label in `k8s/base/kustomization.yaml` when it is intended to identify this release.
- [ ] Search deployment and user documentation for stale hard-coded release/image examples.
- [ ] Confirm the tag without its `v` prefix, `Cargo.toml`, and the root `Cargo.lock` package version are identical. The tag workflow enforces this for tag builds before publishing images.

### Validate the release commit

- [ ] Run the repository's format, build, test, Clippy, and security-audit checks, recording commands and CI run URLs.
- [ ] Render the production Kustomize overlay and confirm it selects `ghcr.io/spooled-cloud/spooled-backend` with the intended immutable release tag or digest.
- [ ] Inspect the live Deployment selector before applying. If it still contains legacy `app.kubernetes.io/version=1.0.0`, use the documented one-time controlled Deployment recreation; an ordinary apply cannot mutate that selector.
- [ ] Confirm no secrets, populated `.env` files, or local generated artifacts are staged.
- [ ] Record any advisory-check exception with its owner and rationale. Do not waive a same-artifact version mismatch.

### Publish and identify the artifact

- [ ] Create the tag on the validated release commit and confirm `git rev-parse "${RELEASE_TAG}^{}"` equals that commit.
- [ ] Record the tag workflow run and confirm the expected amd64/arm64 images and multi-architecture manifest exist.
- [ ] Record the immutable GHCR digest for `ghcr.io/spooled-cloud/spooled-backend:${RELEASE_TAG}`. Do not use `latest` as release identity.
- [ ] Confirm the GitHub Release, Git tag, image tag, image digest, and source commit describe the same artifact.

### Deploy separately

- [ ] Record the environment, operator/provider deployment ID, start time, selected immutable tag or digest, and previous rollback digest.
- [ ] Confirm publication did not get mistaken for deployment: a Git tag, GitHub Release, or GHCR image does not prove a host was updated.
- [ ] Verify rollout status and deployment history in the actual target environment.

### Verify production

- [ ] Confirm deployment/provider provenance resolves to the intended GHCR digest and source commit.
- [ ] Check public `/health`, `/health/live`, and `/health/ready` for service health only; these endpoints do not prove the running release version.
- [ ] Using an isolated authorized API key, call authenticated `GET /api/v1/dashboard` and require `system.version == RELEASE_VERSION`.
- [ ] Require fresh/small `system.uptime_seconds` after a rollout; mature uptime can indicate that the old process is still serving.
- [ ] Record the observation time and keep source, tag, artifact, deployment, and live-production evidence as separate facts.
- [ ] Recheck REST, realtime, and gRPC paths relevant to the release; record any externally blocked path rather than claiming it passed.

### Close out

- [ ] Exercise or verify the rollback target and instructions.
- [ ] Update dated operational-boundary documentation only from recorded evidence.
- [ ] Preserve unresolved checks as explicit exceptions; never convert an unverified item into a production claim.

## Production Checklist

### Security

- [ ] Change `JWT_SECRET` from default
- [ ] Use strong database passwords
- [ ] Enable TLS/HTTPS (via ingress or load balancer)
- [ ] Enable gRPC TLS (required for Cloudflare Tunnel HTTPS origin)
- [ ] Configure network policies
- [ ] Set up proper RBAC
- [ ] Enable audit logging
- [ ] Review and restrict CORS settings

### gRPC TLS (Cloudflare Tunnel)

Cloudflare Tunnel requires HTTPS for HTTP/2 connections (which gRPC uses). You **cannot** use plaintext HTTP for gRPC through Cloudflare Tunnel.

#### Required Configuration

1. **Docker Compose**: Enable TLS (default in `docker-compose.prod.yml`):
   ```yaml
   environment:
     GRPC_TLS_ENABLED: "true"
   ```

2. **Cloudflare Tunnel Config**:
   - **Service Type**: `HTTPS`
   - **URL**: `backend:50051`
   - **Settings**:
     - [x] HTTP2 Connection
     - [x] No TLS Verify (required for self-signed certs)

3. **Certificates**: Do **not** upload PEMs. Production Compose generates a private self-signed origin cert into the `grpc_tls` Docker volume on first start and renews it when fewer than 30 days remain. The helper container stays Up/healthy afterward (set `GRPC_TLS_INIT_ONCE=1` only for CI one-shot checks).

#### Local Development (without Cloudflare)

For local development without a tunnel, you can disable TLS:
- `GRPC_TLS_ENABLED=false` 
- Connect directly to `localhost:50051` with insecure credentials

### Database

- [ ] Enable connection pooling (PgBouncer) - included in docker-compose
- [ ] Configure appropriate `max_connections` (200 default)
- [ ] Set up automated backups
- [ ] Enable SSL connections
- [ ] Configure appropriate timeouts
- [ ] Review autovacuum settings (migration 20241209000005 tunes high-churn tables)

### Redis

- [ ] Enable persistence (AOF/RDB)
- [ ] Configure maxmemory policy
- [ ] Set up Redis Sentinel or Cluster for HA

### Application

- [ ] Set `RUST_ENV=production`
- [ ] Configure appropriate rate limits
- [ ] Set reasonable timeout values
- [ ] Enable compression
- [ ] Configure log level (info or warn)

### Infrastructure

- [ ] Set up horizontal pod autoscaling
- [ ] Configure pod disruption budgets
- [ ] Set resource limits and requests
- [ ] Configure health check endpoints
- [ ] Set up monitoring and alerting

---

## Monitoring & Observability

### Prometheus Metrics

Metrics are exposed at `/metrics` (default port 9090).

Key metrics to monitor:

```
# Queue Health
spooled_jobs_pending          # Jobs waiting to be processed
spooled_jobs_processing       # Jobs currently being processed
spooled_job_max_age_seconds   # Oldest pending job age (CRITICAL!)

# Throughput
spooled_jobs_enqueued_total   # Total jobs enqueued
spooled_jobs_completed_total  # Total jobs completed
spooled_jobs_failed_total     # Total jobs failed

# Workers
spooled_workers_active        # Active workers
spooled_workers_healthy       # Healthy workers

# API
spooled_api_requests_total    # Total API requests
spooled_api_request_duration  # Request latency histogram
```

### Grafana Dashboard

**Production Compose** (`docker-compose.prod.yml`) auto-provisions a Prometheus datasource on Grafana start (self-bootstrap; no init container).

**Local/dev Compose** can mount richer dashboards from the repo:

```bash
# Dashboard JSON (local/dev assets):
docker/grafana/dashboards/spooled-overview.json
```

Access Grafana at http://localhost:3000 (admin/admin by default — change in production).

### Alerting Rules

Alert rules are pre-configured in `docker/prometheus/spooled-rules.yml`:

| Alert | Condition | Severity |
|-------|-----------|----------|
| `JobQueueBacklog` | Oldest job > 5 minutes | Warning |
| `JobQueueBacklogCritical` | Oldest job > 15 minutes | Critical |
| `HighJobFailureRate` | Failure rate > 10% | Warning |
| `CriticalJobFailureRate` | Failure rate > 25% | Critical |
| `NoHealthyWorkers` | No healthy workers | Critical |
| `HighAPILatency` | p99 > 1 second | Warning |
| `JobsDeadlettered` | > 5 DLQ jobs in 15 min | Warning |

To enable alerting, configure Alertmanager in `docker/prometheus/prometheus.yml`.

### Distributed Tracing (Jaeger)

Jaeger provides distributed tracing for request flow analysis.

**Enable tracing:**

```bash
# Build with OpenTelemetry support
cargo build --features otel

# Run with tracing
OTEL_EXPORTER_OTLP_ENDPOINT=http://localhost:4317 \
OTEL_SERVICE_NAME=spooled-backend \
cargo run --features otel
```

Access Jaeger UI at http://localhost:16686

---

## Scaling Guidelines

### Horizontal Scaling

The backend is stateless and scales horizontally:

```yaml
# Kubernetes HPA
apiVersion: autoscaling/v2
kind: HorizontalPodAutoscaler
spec:
  minReplicas: 2
  maxReplicas: 20
  metrics:
    - type: Resource
      resource:
        name: cpu
        target:
          type: Utilization
          averageUtilization: 70
    - type: Resource
      resource:
        name: memory
        target:
          type: Utilization
          averageUtilization: 80
```

### Database Scaling

| Load Level | Recommendation |
|------------|----------------|
| < 1K req/s | Single PostgreSQL instance |
| 1K-10K req/s | PostgreSQL with PgBouncer |
| > 10K req/s | Read replicas + connection pooling |

### Redis Scaling

| Load Level | Recommendation |
|------------|----------------|
| < 10K ops/s | Single Redis instance |
| 10K-100K ops/s | Redis Sentinel (HA) |
| > 100K ops/s | Redis Cluster |

### Resource Guidelines

```yaml
# Starter (< 100 req/s)
resources:
  requests:
    memory: "256Mi"
    cpu: "250m"
  limits:
    memory: "512Mi"
    cpu: "500m"

# Medium (100-1000 req/s)
resources:
  requests:
    memory: "512Mi"
    cpu: "500m"
  limits:
    memory: "1Gi"
    cpu: "1000m"

# Large (> 1000 req/s)
resources:
  requests:
    memory: "1Gi"
    cpu: "1000m"
  limits:
    memory: "2Gi"
    cpu: "2000m"
```

---

## Troubleshooting

### Common Issues

#### 1. Database Connection Errors

```
Error: Failed to connect to database
```

**Solutions:**
- Verify `DATABASE_URL` is correct
- Check network connectivity to database
- Ensure database is accepting connections
- Check connection pool settings

#### 2. Redis Connection Errors

```
Error: Redis unavailable, running without cache
```

**Solutions:**
- Verify `REDIS_URL` is correct
- Check Redis is running
- Note: Application will run without Redis (degraded mode)

#### 3. High Memory Usage

**Solutions:**
- Check for job payload size limits
- Review connection pool settings
- Enable memory limits in Kubernetes

#### 4. Slow Job Processing

**Indicators:**
- High `spooled_job_max_age_seconds`
- Growing `spooled_jobs_pending`

**Solutions:**
- Add more workers
- Check worker health
- Review job timeout settings
- Check for database bottlenecks

#### 5. Authentication Failures

```
Error: Invalid API key
```

**Solutions:**
- Verify API key format
- Check key hasn't expired
- Ensure key is active in database

### Useful Commands

```bash
# Check pod status
kubectl describe pod <pod-name> -n spooled

# View recent production logs
kubectl logs --tail=100 -f deployment/prod-spooled-backend -n spooled

# Execute into a production backend container
kubectl exec -it deployment/prod-spooled-backend -n spooled -- /bin/sh

# Check database connectivity
kubectl run pg-test --rm -it --image=postgres:16 -- psql $DATABASE_URL

# Check Redis connectivity
kubectl run redis-test --rm -it --image=redis:7 -- redis-cli -u $REDIS_URL ping
```

---

## Support

- GitHub Issues: https://github.com/spooled-cloud/spooled-backend/issues
- Documentation: https://spooled.cloud/docs/
- Email: support@spooled.cloud
