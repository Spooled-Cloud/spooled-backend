# gRPC Server Guide

This guide covers the gRPC server implementation in Spooled Backend, including configuration, available services, testing, and troubleshooting.

## Overview

Spooled Backend provides a high-performance gRPC API alongside the REST API for job queue operations. The gRPC server uses [tonic](https://github.com/hyperium/tonic) (Rust's async gRPC implementation) with Protocol Buffers.

### Features

- **QueueService**: Job queue operations (enqueue, dequeue, complete, fail, etc.)
- **WorkerService**: Worker lifecycle management (register, heartbeat, deregister)
- **Health Service**: Standard gRPC health checks (`grpc.health.v1.Health`)
- **Reflection Service**: Service discovery for debugging tools like `grpcurl`
- **Optional TLS**: For secure connections (required for Cloudflare Tunnel)
- **Server-side streaming**: `StreamJobs` for continuous job delivery
- **Bidirectional streaming**: `ProcessJobs` for real-time job processing

## Configuration

### Environment Variables

| Variable | Default | Description |
|----------|---------|-------------|
| `GRPC_PORT` | `50051` | Port for the gRPC server |
| `HOST` | `0.0.0.0` | Host address to bind to |
| `GRPC_TLS_ENABLED` | `false` | Enable TLS for gRPC connections |
| `GRPC_TLS_CERT_PATH` | - | Path to TLS certificate (PEM format) |
| `GRPC_TLS_KEY_PATH` | - | Path to TLS private key (PEM format) |

### Example Startup

```bash
# Development (no TLS)
DATABASE_URL="postgres://spooled:spooled_password@localhost:5432/spooled" \
REDIS_URL="redis://localhost:6379" \
JWT_SECRET="local-dev-secret" \
REGISTRATION_MODE="open" \
RUST_ENV="development" \
GRPC_PORT=50052 \
cargo run --bin spooled-backend

# Production (with TLS)
DATABASE_URL="$DATABASE_URL" \
REDIS_URL="$REDIS_URL" \
JWT_SECRET="$JWT_SECRET" \
GRPC_PORT=50051 \
GRPC_TLS_ENABLED=true \
GRPC_TLS_CERT_PATH=/etc/ssl/certs/grpc.crt \
GRPC_TLS_KEY_PATH=/etc/ssl/private/grpc.key \
cargo run --release --bin spooled-backend
```

### TLS Configuration

gRPC TLS is required when an HTTPS ingress connects to the gRPC origin. The production Cloudflare Tunnel route uses `https://backend:50051` with HTTP/2 origin enabled and TLS verification disabled for the private self-signed origin certificate. Direct and public Health, reflection, and an authenticated application unary RPC were verified after deployment on 2026-07-13.

#### Automatic Self-Signed Certificate (Production Compose)

The production Compose stack generates a private self-signed origin certificate on first deployment. The certificate and key persist in the `grpc_tls` Docker volume, and the init service renews them during a deployment when less than 30 days remain. The backend mounts the volume read-only. No PEM files, host paths, or Docker Swarm secrets are required.

**Default setup (docker-compose.prod.yml):**
- TLS is enabled by default
- Certificate generation and renewal are automatic (`grpc-tls-init` stays Up/healthy after ensure; CI uses `GRPC_TLS_INIT_ONCE=1`)
- The private key is stored outside Git and container images (volume `grpc_tls`)
- Pin `BACKEND_IMAGE` to an immutable tag/digest on production hosts; default `latest` is only for casual/dev deploys

#### Supplying Your Own Certificate

Deployments that require a CA-issued certificate can replace the `grpc_tls` volume through a platform-specific Compose override or orchestrator manifest. Mount the certificate and key read-only, keep them outside Git and container images, and set `GRPC_TLS_CERT_PATH` and `GRPC_TLS_KEY_PATH` to their in-container paths.

#### Disabling TLS

For local development or when TLS is terminated at a load balancer:

```bash
# Via environment variable
GRPC_TLS_ENABLED=false cargo run

# In docker-compose, override the environment:
services:
  backend:
    environment:
      GRPC_TLS_ENABLED: "false"
```

**Warning:** Without origin TLS, use an ingress that explicitly supports cleartext HTTP/2 (`h2c`) to the backend. Do not assume an HTTP/1 reverse proxy can carry gRPC.

## Available Services

### 1. QueueService (`spooled.v1.QueueService`)

Job queue operations for enqueueing, processing, and managing jobs.

| Method | Request | Response | Description |
|--------|---------|----------|-------------|
| `Enqueue` | `EnqueueRequest` | `EnqueueResponse` | Add a new job to the queue |
| `Dequeue` | `DequeueRequest` | `DequeueResponse` | Fetch jobs for processing (batch supported) |
| `Complete` | `CompleteRequest` | `CompleteResponse` | Mark a job as completed |
| `Fail` | `FailRequest` | `FailResponse` | Mark a job as failed (with retry option) |
| `RenewLease` | `RenewLeaseRequest` | `RenewLeaseResponse` | Extend job lease duration |
| `GetJob` | `GetJobRequest` | `GetJobResponse` | Get job details by ID |
| `GetQueueStats` | `GetQueueStatsRequest` | `GetQueueStatsResponse` | Get queue statistics |
| `StreamJobs` | `StreamJobsRequest` | `stream Job` | Server-side streaming of jobs |
| `ProcessJobs` | `stream ProcessRequest` | `stream ProcessResponse` | Bidirectional streaming |

> **`Enqueue` timeouts**: if `timeout_seconds` is omitted (or `0`), the job
> timeout defaults to `300` seconds (matching the REST API).

### 2. WorkerService (`spooled.v1.WorkerService`)

Worker lifecycle management for registering and monitoring workers.

| Method | Request | Response | Description |
|--------|---------|----------|-------------|
| `Register` | `RegisterWorkerRequest` | `RegisterWorkerResponse` | Register a new worker |
| `Heartbeat` | `HeartbeatRequest` | `HeartbeatResponse` | Send worker heartbeat |
| `Deregister` | `DeregisterRequest` | `DeregisterResponse` | Deregister a worker |

### 3. Health Service (`grpc.health.v1.Health`)

Standard gRPC health checking protocol.

| Method | Request | Response | Description |
|--------|---------|----------|-------------|
| `Check` | `HealthCheckRequest` | `HealthCheckResponse` | Check service health |
| `Watch` | `HealthCheckRequest` | `stream HealthCheckResponse` | Watch health status |

## Authentication

All `QueueService` and `WorkerService` methods require authentication via one of:

- **API key credential**: send the API key value in the `x-api-key` gRPC metadata field, for example `x-api-key: sp_live_...`. The metadata field name is not the credential.
- **JWT credential**: send the JWT in the `authorization` gRPC metadata field as `authorization: Bearer <jwt-token>`.

Although tools often call metadata entries “headers,” these names are HTTP/2/gRPC metadata. The health service does NOT require authentication.

### Example with API Key

```bash
grpcurl -plaintext \
  -H "x-api-key: sp_live_abc123..." \
  -d '{"queue_name":"my-queue"}' \
  localhost:50052 spooled.v1.QueueService/GetQueueStats
```

### Spooled Cloud Endpoint

For Spooled Cloud, gRPC is exposed as **TLS on 443**:

- `grpc.spooled.cloud:443` (TLS)

Example:

```bash
grpcurl \
  -H "x-api-key: sp_live_xxxx" \
  -d '{"queue_name":"my-queue"}' \
  grpc.spooled.cloud:443 spooled.v1.QueueService/GetQueueStats
```

## Testing with grpcurl

[grpcurl](https://github.com/fullstorydev/grpcurl) is a command-line tool for interacting with gRPC servers.

### Installation

```bash
# macOS
brew install grpcurl

# Linux
go install github.com/fullstorydev/grpcurl/cmd/grpcurl@latest
```

### List Available Services

```bash
grpcurl -plaintext localhost:50052 list
```

Output:
```
grpc.health.v1.Health
grpc.reflection.v1.ServerReflection
spooled.v1.QueueService
spooled.v1.WorkerService
```

### Describe a Service

```bash
grpcurl -plaintext localhost:50052 describe spooled.v1.QueueService
```

### Health Check

```bash
grpcurl -plaintext -d '{}' localhost:50052 grpc.health.v1.Health/Check
```

Output:
```json
{
  "status": "SERVING"
}
```

### Check Specific Service Health

```bash
grpcurl -plaintext \
  -d '{"service": "spooled.v1.QueueService"}' \
  localhost:50052 grpc.health.v1.Health/Check
```

## Proto File Location

The Protocol Buffer definitions are located at:

```
spooled-backend/proto/spooled.proto
```

The proto file is compiled during the build process via `build.rs` using `tonic-prost-build`.

## Architecture

```
┌─────────────────────────────────────────────────────────────┐
│                    Spooled Backend                          │
├─────────────────────────────────────────────────────────────┤
│  HTTP API (axum)          │  gRPC API (tonic)               │
│  Port: 8080               │  Port: GRPC_PORT (default 50051)│
├───────────────────────────┼─────────────────────────────────┤
│  REST endpoints           │  QueueService                   │
│  WebSocket                │  WorkerService                  │
│  SSE streaming            │  HealthService                  │
│                           │  ReflectionService              │
├───────────────────────────┴─────────────────────────────────┤
│                    Shared Services                          │
│  - Database (PostgreSQL)                                    │
│  - Cache (Redis)                                            │
│  - Metrics (Prometheus)                                     │
│  - Authentication                                           │
└─────────────────────────────────────────────────────────────┘
```

## Troubleshooting

### Port Already in Use

**Symptom**: Server logs show `Address already in use` error for gRPC port.

**Common Causes**:
1. Another instance of spooled-backend is running
2. Another service is using the port (e.g., Multipass uses 50051)
3. Previous process didn't shut down cleanly

**Solutions**:

```bash
# Check what's using the port
lsof -i :50051

# Or with netstat
netstat -vanp tcp | grep 50051

# Use a different port
GRPC_PORT=50052 cargo run --bin spooled-backend

# Kill conflicting process (if safe)
pkill -f spooled-backend
```

**Note**: On macOS, port 50051 is commonly used by:
- **Multipass** (Ubuntu VM manager) - uses `launchd` to reserve the port
- **Docker Desktop** - some configurations use this port

If you see `launchd` using the port, use `GRPC_PORT=50052` instead.

### Connection Refused

**Symptom**: Client gets "connection refused" when connecting.

**Checklist**:
1. Verify the server is running: `lsof -i :50052`
2. Check server logs for startup errors
3. Verify the correct port is being used
4. Check firewall rules

### TLS Certificate Issues

**Symptom**: TLS handshake failures or certificate errors.

**Solutions**:
1. Verify certificate paths are correct
2. Ensure certificate is in PEM format
3. Check certificate validity: `openssl x509 -in cert.pem -text -noout`
4. Verify certificate matches the hostname being used

### Authentication Errors

**Symptom**: `Unauthenticated: Missing x-api-key or authorization header`

Here `x-api-key` and `authorization` are metadata field names. The credential is the API key value placed in `x-api-key`, or the JWT placed after `Bearer` in `authorization`.

**Solutions**:
1. Include the API key header: `-H "x-api-key: sp_live_..."` (or legacy `sk_live_...`)
2. Or use JWT token: `-H "Authorization: Bearer <token>"`
3. Verify the API key is valid and not expired
4. Check the organization has the correct permissions

### Reflection Not Working

**Symptom**: `grpcurl list` shows empty or incomplete results.

**Solutions**:
1. Ensure reflection service is enabled (it is by default)
2. Try with `-plaintext` flag for non-TLS connections
3. For TLS, use appropriate flags: `-insecure` or `-cacert`

## Performance Tuning

The gRPC server is tuned for high concurrency and low latency.

### Default Optimizations

The server automatically applies:
- **HTTP/2 Keepalive**: Pings every 10s to keep connections alive through load balancers
- **TCP Keepalive**: Prevents silent connection drops
- **TCP_NODELAY**: Disables Nagle's algorithm for lower latency
- **Concurrency**: Supports up to 200 concurrent streams per connection
- **Window Sizes**: 1MB connection window, 512KB stream window

### Throughput vs Latency

Performance depends on payload size, database capacity, TLS termination, and network
path. Benchmark the target deployment with the checked-in `loadtest/` tools rather
than relying on environment-independent throughput or latency figures.

### Cloudflare Tunnel Configuration

Cloudflare Tunnel **requires HTTPS** for HTTP/2 (gRPC). You cannot use plaintext HTTP.

**Required settings:**
1. `GRPC_TLS_ENABLED=true` in docker-compose (default)
2. Service Type: `HTTPS`, URL: `backend:50051`
3. HTTP2 Connection: `ON`
4. No TLS Verify: `ON` (for self-signed certificates)

### Connection Pooling

gRPC uses HTTP/2 which supports multiplexing multiple requests over a single connection. This is more efficient than REST for high-throughput scenarios. Clients should reuse the `SpooledGrpcClient` instance to benefit from connection pooling.

For continuous job processing, prefer:
- `StreamJobs` (server-side streaming) for simple job delivery
- `ProcessJobs` (bidirectional streaming) for real-time processing with acknowledgments
- WebSocket endpoint (`/api/v1/ws`) for REST API clients

## SDK Support

The gRPC API is supported by the official SDKs:

- **Node.js SDK** (`@spooled/sdk`): Full gRPC support with `@grpc/grpc-js`
- **Python SDK** (`spooled`): Full gRPC support with `grpcio`
- **Go SDK** (`spooled-sdk-go`): Production ready - [Documentation](https://pkg.go.dev/github.com/spooled-cloud/spooled-sdk-go)
- **PHP SDK** (`spooled-cloud/spooled`): Full gRPC support with `ext-grpc`

### Node.js SDK Example

```typescript
import { SpooledClient } from '@spooled/sdk';

const client = new SpooledClient({
  apiKey: 'sp_live_...',
  grpc: {
    enabled: true,
    host: 'localhost',
    port: 50052,
  },
});

// Use gRPC for job operations
const { jobId } = await grpcClient.queue.enqueue({
  queueName: 'my-queue',
  payload: { key: 'value' },
});
```

## Related Documentation

- [API Usage Guide](./api-usage.md) - REST API documentation
- [Quickstart](./quickstart.md) - Cloud and self-hosted setup
- [Deployment Guide](./deployment.md) - Production deployment
- [Security Policy](../../SECURITY.md) - Reporting and deployment best practices
