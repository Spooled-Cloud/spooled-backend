# Spooled Cloud Deployment Guide

This guide covers deploying Spooled Cloud in various environments.

## Table of Contents

1. [Prerequisites](#prerequisites)
2. [Local Development](#local-development)
3. [Docker Deployment](#docker-deployment)
4. [Kubernetes Deployment](#kubernetes-deployment)
5. [Production Checklist](#production-checklist)
6. [Monitoring & Observability](#monitoring--observability)
7. [Scaling Guidelines](#scaling-guidelines)
8. [Troubleshooting](#troubleshooting)

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
# Start the full stack
cd spooled-backend/docker
docker-compose up -d

# Verify services
docker-compose ps
```

This starts:
- **PostgreSQL 16** - Primary database (port 5432)
- **PgBouncer** - Connection pooler (port 6432)
- **Redis 7** - Cache and Pub/Sub (port 6379)
- **Prometheus** - Metrics collection (port 9091)
- **Grafana** - Dashboards (port 3000)
- **Jaeger** - Distributed tracing (port 16686)

### 2. Configure Environment

```bash
# Copy example env file
cp .env.example .env

# Edit configuration
vim .env
```

Required environment variables:

```env
# Database (use PgBouncer for app, direct for migrations)
DATABASE_URL=postgres://spooled:spooled_dev_password@localhost:6432/spooled
DATABASE_DIRECT_URL=postgres://spooled:spooled_dev_password@localhost:5432/spooled

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
# {"status":"healthy","version":"1.0.0","database":true,"cache":true}
```

---

## Docker Deployment

### Build the Image

```bash
cd spooled-backend

# Build production image
docker build -t spooled-backend:latest .

# Or with specific tag
docker build -t spooled-backend:v1.0.0 .
```

### Run with Docker

```bash
docker run -d \
  --name spooled-backend \
  -p 8080:8080 \
  -p 9090:9090 \
  -p 50051:50051 \
  -e DATABASE_URL="postgres://user:pass@host:5432/spooled" \
  -e REDIS_URL="redis://redis:6379" \
  -e JWT_SECRET="your-production-secret" \
  -e RUST_ENV="production" \
  spooled-backend:latest
```

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

## Kubernetes Deployment

### Prerequisites

- Kubernetes cluster (1.25+)
- kubectl configured
- Helm 3 (optional, for dependencies)

### 1. Create Namespace

```bash
kubectl create namespace spooled-cloud
```

### 2. Create Secrets

```bash
kubectl create secret generic spooled-secrets \
  --namespace spooled-cloud \
  --from-literal=database-url='postgres://user:pass@postgres:5432/spooled' \
  --from-literal=redis-url='redis://redis:6379' \
  --from-literal=jwt-secret='your-production-secret'
```

### 3. Deploy with Kustomize

```bash
# Development
kubectl apply -k k8s/overlays/development

# Production
kubectl apply -k k8s/overlays/production
```

### 4. Verify Deployment

```bash
# Check pods
kubectl get pods -n spooled-cloud

# Check services
kubectl get svc -n spooled-cloud

# View logs
kubectl logs -f deployment/spooled-backend -n spooled-cloud

# Port forward for testing
kubectl port-forward svc/spooled-backend 8080:80 -n spooled-cloud
```

### Helm Chart (External Dependencies)

```bash
# PostgreSQL
helm install postgres bitnami/postgresql \
  --namespace spooled-cloud \
  --set auth.postgresPassword=your-password \
  --set auth.database=spooled

# Redis
helm install redis bitnami/redis \
  --namespace spooled-cloud \
  --set auth.enabled=false
```

---

## Production Checklist

### Security

- [ ] Change `JWT_SECRET` from default
- [ ] Use strong database passwords
- [ ] Enable TLS/HTTPS (via ingress or load balancer)
- [ ] Configure network policies
- [ ] Set up proper RBAC
- [ ] Enable audit logging
- [ ] Review and restrict CORS settings

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

Dashboards are auto-provisioned when using docker-compose:

```bash
# Dashboard location:
docker/grafana/dashboards/spooled-overview.json
```

Access Grafana at http://localhost:3000 (admin/admin)

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
kubectl describe pod <pod-name> -n spooled-cloud

# View recent logs
kubectl logs --tail=100 -f deployment/spooled-backend -n spooled-cloud

# Execute into container
kubectl exec -it deployment/spooled-backend -n spooled-cloud -- /bin/sh

# Check database connectivity
kubectl run pg-test --rm -it --image=postgres:16 -- psql $DATABASE_URL

# Check Redis connectivity
kubectl run redis-test --rm -it --image=redis:7 -- redis-cli -u $REDIS_URL ping
```

---

## Support

- GitHub Issues: https://github.com/spooled-cloud/spooled-backend/issues
- Documentation: https://docs.spooled.cloud
- Email: support@spooled.cloud

