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

gRPC TLS is **required** when using Cloudflare Tunnel (or similar proxies) with HTTPS origin, because HTTP/2 over HTTPS requires TLS at the origin.

#### Using Self-Signed Certificates (Recommended for Cloudflare Tunnel)

The production docker-compose includes pre-generated self-signed certificates in the `certs/` directory. These work with Cloudflare Tunnel when "No TLS Verify" is enabled.

**Default setup (docker-compose.prod.yml):**
- TLS is enabled by default
- Certificates are mounted from `./certs/grpc-cert.pem` and `./certs/grpc-key.pem`
- Valid for 10 years

**Cloudflare Tunnel configuration:**
- Service Type: `HTTPS`
- URL: `backend:50051`
- HTTP2 connection: `ON`
- No TLS Verify: `ON` (required for self-signed certs)

#### Generating New Certificates

To generate fresh self-signed certificates:

```bash
# Generate 10-year self-signed certificate
openssl req -x509 -newkey rsa:2048 \
  -keyout certs/grpc-key.pem \
  -out certs/grpc-cert.pem \
  -days 3650 -nodes \
  -subj "/CN=backend"
```

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

**Warning:** Without TLS, gRPC cannot be used through Cloudflare Tunnel with HTTPS origin type. Use HTTP origin type instead (which may not support HTTP/2 properly).

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

- **API Key Header**: `x-api-key: <your-api-key>`
- **Authorization Header**: `Authorization: Bearer <jwt-token>`

The health service does NOT require authentication.

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

| Deployment | Throughput | Latency (p99) | Notes |
|------------|------------|---------------|-------|
| Local (no TLS) | ~10k req/s | < 5ms | Fastest for development |
| Cloudflare (TLS) | ~2k req/s | ~50ms | Production through Cloudflare Tunnel |
| Direct (with TLS) | ~5k req/s | ~10ms | Direct connection, no proxy |

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

- [API Integration Guide](./API_INTEGRATION.md) - REST API documentation
- [Getting Started](./GETTING_STARTED.md) - Quick start guide
- [Deployment Guide](./DEPLOYMENT.md) - Production deployment
- [Security Guide](./SECURITY.md) - Security best practices
