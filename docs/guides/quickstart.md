# Spooled Cloud Quick Start Guide

Get up and running with Spooled Cloud in 5 minutes.

## Prerequisites

- Docker & Docker Compose
- Rust 1.85+ (for local development)
- curl or httpie

## Step 1: Start the Services

```bash
cd spooled-backend

# Start PostgreSQL, Redis, PgBouncer, and monitoring
docker-compose up -d

# Verify services are running
docker-compose ps
```

Expected output:
```
NAME                 STATUS              PORTS
spooled-postgres     running (healthy)   5432/tcp
spooled-pgbouncer    running (healthy)   6432/tcp
spooled-redis        running (healthy)   6379/tcp
spooled-prometheus   running             9091/tcp
spooled-grafana      running             3000/tcp
```

## Step 2: Configure Environment

```bash
# Copy the example environment file
cp .env.example .env

# The defaults work for local development
# For production, you MUST change JWT_SECRET and passwords
```

## Step 3: Run the Backend

```bash
# Run the server (migrations run automatically on first start)
cargo run
```

You should see:
```
INFO spooled_backend: Starting Spooled Backend v1.0.0
INFO spooled_backend: Database connection pool established
INFO spooled_backend: Database migrations completed
INFO spooled_backend: Redis cache connected
INFO spooled_backend: Server listening on 0.0.0.0:8080
INFO spooled_backend: Metrics server listening on 0.0.0.0:9090
```

## Step 4: Verify Installation

```bash
curl http://localhost:8080/health
```

Expected response:
```json
{"status":"healthy","version":"1.0.0","database":true,"cache":true}
```

## Step 5: Create Your Organization

Every tenant in Spooled has an organization. Create one first:

```bash
curl -X POST http://localhost:8080/api/v1/organizations \
  -H "Content-Type: application/json" \
  -d '{
    "name": "My Company",
    "slug": "my-company"
  }'
```

Response:
```json
{
  "id": "org_a1b2c3d4e5f6",
  "name": "My Company",
  "slug": "my-company",
  "created_at": "2024-12-09T10:00:00Z"
}
```

**Save the `id` value** - you'll need it for the next step.

## Step 6: Create Your API Key

API keys authenticate all requests to the API:

```bash
curl -X POST http://localhost:8080/api/v1/organizations/org_a1b2c3d4e5f6/api-keys \
  -H "Content-Type: application/json" \
  -d '{
    "name": "Development Key",
    "permissions": ["jobs:read", "jobs:write", "queues:read"]
  }'
```

Response:
```json
{
  "id": "key_x1y2z3",
  "key": "sk_live_abc123xyz789...",
  "name": "Development Key",
  "permissions": ["jobs:read", "jobs:write", "queues:read"],
  "created_at": "2024-12-09T10:01:00Z"
}
```

⚠️ **IMPORTANT**: Save the `key` value immediately! It's only shown once and cannot be retrieved later.

## Step 7: Create Your First Job

Now use your API key to create a job:

```bash
curl -X POST http://localhost:8080/api/v1/jobs \
  -H "Content-Type: application/json" \
  -H "Authorization: Bearer sk_live_abc123xyz789..." \
  -d '{
    "queue_name": "emails",
    "payload": {
      "to": "user@example.com",
      "subject": "Welcome!",
      "body": "This is my first job!"
    },
    "priority": 0,
    "max_retries": 3
  }'
```

Response:
```json
{
  "id": "job_m1n2o3p4",
  "organization_id": "org_a1b2c3d4e5f6",
  "queue_name": "emails",
  "status": "pending",
  "payload": {"to": "user@example.com", "subject": "Welcome!", "body": "This is my first job!"},
  "priority": 0,
  "max_retries": 3,
  "created_at": "2024-12-09T10:02:00Z"
}
```

## Step 8: List Your Jobs

```bash
curl http://localhost:8080/api/v1/jobs \
  -H "Authorization: Bearer sk_live_abc123xyz789..."
```

Response:
```json
{
  "jobs": [
    {
      "id": "job_m1n2o3p4",
      "queue_name": "emails",
      "status": "pending",
      ...
    }
  ],
  "total": 1,
  "page": 1,
  "per_page": 50
}
```

## Step 9: Get Queue Statistics

```bash
curl http://localhost:8080/api/v1/queues/emails/stats \
  -H "Authorization: Bearer sk_live_abc123xyz789..."
```

Response:
```json
{
  "queue_name": "emails",
  "pending": 1,
  "processing": 0,
  "completed": 0,
  "failed": 0,
  "dead_letter": 0
}
```

---

## Common Operations

### Create a Scheduled Job (Run Later)

```bash
curl -X POST http://localhost:8080/api/v1/jobs \
  -H "Content-Type: application/json" \
  -H "Authorization: Bearer sk_live_..." \
  -d '{
    "queue_name": "reports",
    "payload": {"type": "daily_report"},
    "scheduled_at": "2024-12-10T09:00:00Z"
  }'
```

### Create a Cron Schedule (Recurring)

```bash
curl -X POST http://localhost:8080/api/v1/schedules \
  -H "Content-Type: application/json" \
  -H "Authorization: Bearer sk_live_..." \
  -d '{
    "name": "Daily Report",
    "cron_expression": "0 9 * * *",
    "timezone": "America/New_York",
    "queue_name": "reports",
    "payload_template": {"type": "daily_report"}
  }'
```

### Bulk Enqueue Jobs

```bash
curl -X POST http://localhost:8080/api/v1/jobs/bulk \
  -H "Content-Type: application/json" \
  -H "Authorization: Bearer sk_live_..." \
  -d '{
    "jobs": [
      {"queue_name": "emails", "payload": {"id": 1}},
      {"queue_name": "emails", "payload": {"id": 2}},
      {"queue_name": "emails", "payload": {"id": 3}}
    ]
  }'
```

### Pause a Queue

```bash
curl -X POST http://localhost:8080/api/v1/queues/emails/pause \
  -H "Authorization: Bearer sk_live_..."
```

### Resume a Queue

```bash
curl -X POST http://localhost:8080/api/v1/queues/emails/resume \
  -H "Authorization: Bearer sk_live_..."
```

### Retry a Failed Job

```bash
curl -X POST http://localhost:8080/api/v1/jobs/job_m1n2o3p4/retry \
  -H "Authorization: Bearer sk_live_..."
```

### Cancel a Job

```bash
curl -X POST http://localhost:8080/api/v1/jobs/job_m1n2o3p4/cancel \
  -H "Authorization: Bearer sk_live_..."
```

---

## Real-time Updates

### WebSocket Connection

```javascript
const ws = new WebSocket('ws://localhost:8080/api/v1/ws?token=sk_live_...');

ws.onopen = () => {
  // Subscribe to queue events
  ws.send(JSON.stringify({ type: 'subscribe', queues: ['emails'] }));
};

ws.onmessage = (event) => {
  const data = JSON.parse(event.data);
  console.log('Event:', data.type, data.payload);
  // { type: 'job.completed', payload: { id: 'job_...', status: 'completed' } }
};
```

### Server-Sent Events (SSE)

```bash
# Subscribe to all events for your organization
curl -N http://localhost:8080/api/v1/events \
  -H "Authorization: Bearer sk_live_..."

# Subscribe to specific job updates
curl -N http://localhost:8080/api/v1/events/jobs/job_m1n2o3p4 \
  -H "Authorization: Bearer sk_live_..."

# Subscribe to queue events
curl -N http://localhost:8080/api/v1/events/queues/emails \
  -H "Authorization: Bearer sk_live_..."
```

---

## Monitoring

### Prometheus Metrics

```bash
curl http://localhost:9090/metrics
```

Key metrics to watch:
- `spooled_jobs_pending` - Jobs waiting to be processed
- `spooled_jobs_processing` - Jobs currently in progress
- `spooled_job_max_age_seconds` - Age of oldest pending job (alert if > 300s!)

### Grafana Dashboard

Open http://localhost:3000 (login: admin/admin)

---

## Troubleshooting

### "Database Connection Error"

```bash
# Check PostgreSQL is running
docker-compose ps postgres

# Check logs
docker-compose logs postgres

# Verify connection
docker-compose exec postgres psql -U spooled -d spooled -c "SELECT 1"
```

### "Redis Connection Error"

The application will continue to work without Redis (degraded mode - no real-time updates).

```bash
# Check Redis is running
docker-compose ps redis

# Test connection
docker-compose exec redis redis-cli ping
```

### "Migration Failed"

```bash
# Reset database and start fresh
docker-compose down -v
docker-compose up -d postgres pgbouncer redis
sleep 5
cargo run  # Migrations run automatically
```

### "Port Already in Use"

```bash
# Find what's using port 8080
lsof -i :8080

# Kill the process
kill -9 <PID>
```

### "Invalid API Key"

- Verify the key starts with `sk_live_` or `sk_test_`
- Check the key hasn't been revoked
- Ensure you're using the correct organization's key

---

## Next Steps

1. **Read the full API documentation**: `docs/guides/api-usage.md`
2. **Set up webhooks**: Configure GitHub/Stripe webhooks to create jobs automatically
3. **Deploy to production**: See `docs/guides/deployment.md`
4. **Understand the architecture**: See `docs/guides/architecture.md`
5. **Run load tests**: See `loadtest/README.md`

---

## Need Help?

- GitHub Issues: https://github.com/spooled-cloud/spooled-backend/issues
- Documentation: https://docs.spooled.cloud
