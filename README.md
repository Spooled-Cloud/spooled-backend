# Spooled Backend

[![License](https://img.shields.io/badge/License-Apache%202.0-blue.svg)](https://opensource.org/licenses/Apache-2.0)
[![Rust](https://img.shields.io/badge/rust-1.85%2B-orange.svg)](https://www.rust-lang.org/)
[![Docker](https://img.shields.io/badge/docker-ghcr.io-blue.svg)](https://ghcr.io)

**Production-ready job queue and webhook processing service built with Rust**

Spooled is a high-performance, multi-tenant job queue system designed for reliability, observability, and horizontal scalability.

## ✨ Features

- 🚀 **High Performance**: 10,000+ jobs/sec throughput with Rust + Tokio
- 🔒 **Multi-Tenant**: PostgreSQL Row-Level Security (RLS) for data isolation
- 📊 **Observable**: Prometheus metrics, Grafana dashboards, OpenTelemetry tracing
- 🔄 **Reliable**: Exactly-once delivery with `FOR UPDATE SKIP LOCKED`
- ⚡ **Real-Time**: WebSocket/SSE for live job updates
- 🔐 **Secure**: Bcrypt API keys, JWT auth, HMAC webhook verification
- 📈 **Scalable**: Kubernetes-ready with HPA, PDB, and health probes
- 🗓️ **Scheduling**: Cron-based recurring jobs with timezone support
- 🔗 **Workflows**: Job dependencies with DAG execution

## 🐳 Quick Start with Docker

### Pull and Run

```bash
# Pull the multi-arch image (supports amd64 and arm64)
docker pull ghcr.io/spooled-cloud/spooled-backend:latest

# Run with Docker Compose
curl -O https://raw.githubusercontent.com/spooled-cloud/spooled-backend/main/docker-compose.prod.yml
curl -O https://raw.githubusercontent.com/spooled-cloud/spooled-backend/main/.env.example
cp .env.example .env

# Generate secure secrets
export JWT_SECRET=$(openssl rand -base64 32)
export POSTGRES_PASSWORD=$(openssl rand -base64 16)
sed -i "s/your-jwt-secret-minimum-32-characters-long/$JWT_SECRET/" .env
sed -i "s/your_secure_password/$POSTGRES_PASSWORD/g" .env

# Start services
docker compose -f docker-compose.prod.yml up -d

# Verify
curl http://localhost:8080/health
```

### Environment Variables

| Variable | Required | Default | Description |
|----------|----------|---------|-------------|
| `DATABASE_URL` | ✅ | - | PostgreSQL connection string |
| `JWT_SECRET` | ✅ | - | 32+ char secret for JWT signing |
| `ADMIN_API_KEY` | ❌ | - | Key for admin portal access |
| `REDIS_URL` | ❌ | `redis://localhost:6379` | Redis for pub/sub & caching |
| `RUST_ENV` | ❌ | `development` | `development`/`staging`/`production` |
| `REGISTRATION_MODE` | ❌ | `open` | `open`/`closed` - controls public registration |
| `PORT` | ❌ | `8080` | API server port |
| `METRICS_PORT` | ❌ | `9090` | Prometheus metrics port |
| `GRPC_PORT` | ❌ | `50051` | gRPC port for workers |

## 🔧 Local Development

### Prerequisites

- Rust 1.85+
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

| Method | Endpoint | Description |
|--------|----------|-------------|
| `GET` | `/health` | Health check |
| `POST` | `/api/v1/organizations` | Create organization (returns initial API key) |
| `GET` | `/api/v1/organizations/usage` | Get plan usage info |
| `POST` | `/api/v1/jobs` | Create a job |
| `GET` | `/api/v1/jobs` | List jobs |
| `POST` | `/api/v1/jobs/bulk` | Bulk enqueue jobs |
| `POST` | `/api/v1/schedules` | Create cron schedule |
| `POST` | `/api/v1/workflows` | Create workflow (DAG) |
| `POST` | `/api/v1/webhooks/{org}/github` | GitHub webhook |
| `POST` | `/api/v1/webhooks/{org}/stripe` | Stripe webhook |
| `GET` | `/api/v1/ws` | WebSocket for real-time |

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

### Create Your First Job

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

# Deploy
docker compose -f docker-compose.prod.yml up -d

# Enable monitoring (optional)
docker compose -f docker-compose.prod.yml --profile monitoring up -d
```

### Kubernetes

```bash
# Create namespace and secrets
kubectl create namespace spooled
kubectl create secret generic spooled-secrets \
  --namespace spooled \
  --from-literal=database-url='postgres://user:pass@postgres:5432/spooled' \
  --from-literal=jwt-secret="$(openssl rand -base64 32)"

# Deploy with Kustomize
kubectl apply -k k8s/overlays/production

# Or with Helm (coming soon)
# helm install spooled ./charts/spooled -n spooled
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
curl http://localhost:9090/metrics

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
- **Multi-tenancy**: PostgreSQL Row-Level Security (RLS)
- **Rate Limiting**: Per-key limits with Redis (fails closed)
- **Webhooks**: HMAC-SHA256 signature verification
- **Input Validation**: All inputs sanitized and size-limited
- **SSRF Protection**: Webhook URLs validated in production

## 📚 Documentation

- [Quick Start Guide](docs/guides/quickstart.md)
- [API Usage Guide](docs/guides/api-usage.md)
- [Architecture](docs/guides/architecture.md)
- [Deployment Guide](docs/guides/deployment.md)
- [Operations Guide](docs/guides/operations.md)
- [OpenAPI Spec](docs/openapi.yaml)

## 🏗️ Architecture

```
┌─────────────────────────────────────────────────────────────┐
│                     SPOOLED BACKEND                         │
├─────────────────────────────────────────────────────────────┤
│  REST API (Axum)  │  gRPC (Tonic)  │  WebSocket/SSE        │
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

## 📄 License

Apache License 2.0 - see [LICENSE](LICENSE) for details.

---

**Built with ❤️ in Rust**
