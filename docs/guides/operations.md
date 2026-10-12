# Spooled Backend Operations Guide

This guide covers operational configuration, tuning, and best practices for running Spooled Backend in production.

## Table of Contents

1. [Connection Pooling (PgBouncer)](#connection-pooling-pgbouncer)
2. [Rate Limiting & Backpressure](#rate-limiting-backpressure)
3. [Redis Configuration](#redis-configuration)
4. [Queue Partitioning](#queue-partitioning)
5. [Autovacuum Tuning](#autovacuum-tuning)
6. [Monitoring & Alerting](#monitoring-alerting)
7. [Security Configuration](#security-configuration)
8. [Backup & Disaster Recovery](#backup-disaster-recovery)

---

## Connection Pooling (PgBouncer)

### Why PgBouncer?

PostgreSQL connections are expensive (each uses ~5-10MB RAM). PgBouncer provides:
- Connection multiplexing (1000+ app connections → 50 DB connections)
- Transparent failover
- Connection reuse

### Configuration

The docker-compose stack includes PgBouncer with these defaults:

| Setting | Default | Description |
|---------|---------|-------------|
| `PGBOUNCER_POOL_MODE` | `transaction` | Pool connections per transaction (recommended) |
| `PGBOUNCER_MAX_CLIENT_CONN` | `1000` | Max client connections to PgBouncer |
| `PGBOUNCER_DEFAULT_POOL_SIZE` | `50` | Connections per database/user |
| `PGBOUNCER_MIN_POOL_SIZE` | `10` | Minimum idle connections |
| `PGBOUNCER_RESERVE_POOL_SIZE` | `10` | Extra connections for burst |

### Application Configuration

```bash
# Use PgBouncer for application connections
DATABASE_URL=postgres://user:pass@pgbouncer:6432/spooled

# Use direct connection for migrations (prepared statements)
DATABASE_DIRECT_URL=postgres://user:pass@postgres:5432/spooled
```

---

## Rate Limiting & Backpressure

### Global Rate Limits

Configure via environment variables:

| Variable | Default | Description |
|----------|---------|-------------|
| `RATE_LIMIT_REQUESTS_PER_SECOND` | `100` | Global requests per second per client |
| `RATE_LIMIT_BURST_SIZE` | `200` | Burst capacity for sudden traffic |
| `RATE_LIMIT_FAIL_CLOSED` | `false` | `true` rejects requests (and gRPC calls) while Redis cannot answer, instead of letting them through. Set it on a hosted service |

### Per-Client Rate Limiting

Rate limits are tracked per client:
- **Organization**: authenticated routes share one token bucket per organization, sized by the plan
  (requests per second and burst; see [API Reference > Rate Limits](./api-usage.md#rate-limits))
- **API Key**: an optional per-key `rate_limit` in requests per **minute**, checked after the
  organization bucket (so it can only lower it)
- **IP Address**: See [Client IP Resolution](#client-ip-resolution) below. It also keys the
  failed-auth budget, `check-email`, `check-slug` and the email-login throttles
  (`POST /auth/email/start`: 3 per minute and 10 per hour per client; `POST /auth/email/verify`:
  5 guesses per 10 minutes per address and client, plus 20 per address from all clients).
  An IPv6 client counts as its whole /64.
- There is no User-Agent fallback: an unidentified caller is keyed on its TCP peer address.

Throttled email-login routes answer `429 RATE_LIMIT_EXCEEDED` with a `Retry-After` header.

Authenticated REST responses that went through the organization or key limiter also carry
`X-RateLimit-Limit`, `X-RateLimit-Remaining`, `X-RateLimit-Reset` (a Unix time in seconds) and
`X-RateLimit-Scope` (`organization` or `api_key`); a `429` from that limiter has them too, with
`Remaining` 0. They are left out on public and admin routes, without Redis, and when a check was
skipped because Redis failed while `RATE_LIMIT_FAIL_CLOSED` is off. If a proxy or CDN sits in front,
let them through: dashboards and SDKs read them. The CORS layer exposes them to browsers (see
[CORS Configuration](#cors-configuration)). They add no metrics; the limiter's cost is the same
Redis calls as before (the token-bucket script now also returns the time to the next token).

### Client IP Resolution

For unauthenticated traffic the limiter buckets by client IP, so where that IP comes from
decides whether the limiter works at all. The server sees the TCP peer of every request and
resolves the client in this order:

1. `TRUSTED_CLIENT_IP_HEADER`, if you set it (e.g. `CF-Connecting-IP`). It is believed from
   **any** peer: you are telling Spooled that a proxy of yours always sets it. If the header
   is missing on a request, the steps below still apply (and the built-in edge headers of step
   2 are not consulted), ending at the peer address.
2. Otherwise the built-in edge headers, `CF-Connecting-IP` then `X-Real-IP`, but **only when the
   TCP peer is a loopback, private (RFC 1918), link-local or IPv6 unique-local address**, i.e.
   a proxy on the same host or network. Production's Cloudflare Tunnel connects from the docker
   network, so it needs no configuration. A caller that reaches the backend from any other
   address cannot choose its bucket with these headers.
3. `X-Forwarded-For`, read from the **right**, skipping `TRUSTED_PROXY_HOPS` entries.
4. The socket peer address.

An IPv6 address is reduced to its /64 (an IPv4-mapped address to plain IPv4) before it is used
as a rate-limit key, because one subscriber is usually handed a whole /64.

Production logs one warning at boot while `TRUSTED_CLIENT_IP_HEADER` is unset. If your proxy
reaches the backend from a **public** address, step 2 does not apply and every caller would share
that proxy's one bucket: set `TRUSTED_CLIENT_IP_HEADER` (`CF-Connecting-IP` behind Cloudflare,
`X-Real-IP` behind nginx). Setting `TRUSTED_CLIENT_IP_HEADER=CF-Connecting-IP` is also safe on the
production stack, where the backend port is not published.

`X-Forwarded-For` is **not** consulted unless you set `TRUSTED_PROXY_HOPS`, and the default
is `0`. Every proxy appends to that header, so its leftmost entry is whatever the caller
sent: trusting it lets a caller mint a fresh rate-limit bucket per request, which removes
the limiter rather than degrading it. Count only the hops **your own** proxies add — `1`
behind a single reverse proxy, `2` behind e.g. Cloudflare → nginx.

If you run behind a proxy that sets neither `CF-Connecting-IP` nor `X-Real-IP` and you
leave `TRUSTED_PROXY_HOPS` at `0`, every request arrives with the proxy's address and all
unauthenticated traffic shares one bucket. Set one of the two variables.

### Request Size Limits

| Limit | Value | Description |
|-------|-------|-------------|
| Request body | `2 MiB` | Default body limit of every route (axum) |
| `POST /api/v1/jobs/bulk` body | up to `32 MiB` | 100 jobs x the largest plan payload cap (+ 4 KiB each), clamped between 2 and 32 MiB; split bigger batches |
| `MAX_PAYLOAD_SIZE` | `1 MB` | Maximum job payload size |
| `MAX_WEBHOOK_PAYLOAD_SIZE` | `5 MB` | Webhook payload check in the handler; the 2 MiB request body limit above is reached first, so 5 MB is not reachable |

### Queue-Level Controls

```bash
# Pause a queue to stop processing
curl -X POST /api/v1/queues/emails/pause

# Resume processing
curl -X POST /api/v1/queues/emails/resume

# Configure queue limits
curl -X PUT /api/v1/queues/emails/config \
  -d '{"max_retries": 5, "default_timeout": 600, "rate_limit": 50}'
```

`rate_limit` is accepted, validated, stored and returned, but it is **not enforced yet**: no claim
path throttles a queue by it. The only enforced rate limits are the plan's request limit and an
API key's `rate_limit` (requests per minute, checked after the plan limit, so it can only lower it).

### Backpressure Indicators

Monitor these metrics for backpressure:

| Metric | Warning | Critical | Action |
|--------|---------|----------|--------|
| `spooled_job_max_age_seconds` | > 300s | > 900s | Add workers or scale |
| `spooled_jobs_pending` | Growing | > 10,000 | Check worker capacity |
| `spooled_database_connections_active / spooled_database_connections_max` | > 80% | > 95% | Increase pool size |

#### Job list timeouts

`GET /jobs` and `GET /jobs/dlq` run in a read-only transaction under a statement timeout,
`DATABASE_LIST_STATEMENT_TIMEOUT_MS` (default 5000; `0` is refused at startup because Postgres reads
it as no timeout). Postgres cancels the statement itself, so a client that disconnected does not leave
a long scan running. A list it stopped answers `400` with code `QUERY_TIMEOUT` and logs a warning,
`Job list stopped by the list statement timeout` (with `timeout_ms`); it shows in
`spooled_api_requests_total{status="4xx"}` and not as a 5xx. A steady trickle usually means one
organization lists a very large `jobs` table with a broad `q` and no created window or queue; the
fix is on the caller's side. Raise the setting only if the database can take the load: a list holds a
pool connection for as long as it runs. The job-type search (`job_type`, `q`) reads
`idx_jobs_org_job_type`; check it is valid after the upgrade (see [Deployment](./deployment.md),
"0.1.122 checks").

---

## Redis Configuration

### Fallback Polling

When Redis is unavailable, workers use fallback polling:

| Setting | Default | Description |
|---------|---------|-------------|
| `WORKER_FALLBACK_POLL_INTERVAL_SECS` | `5` | Database poll interval when Redis is down |

**How it works:**
1. Workers subscribe to Redis pub/sub channel `org:{org_id}:queue:{name}`
2. When jobs are enqueued, a notification is published
3. If Redis is unavailable, workers poll the database directly
4. Fallback polling ensures no jobs are missed

### Redis Tuning

```bash
# In docker-compose or redis.conf
maxmemory 256mb
maxmemory-policy allkeys-lru
appendonly yes
appendfsync everysec
```

---

## Queue Partitioning

> **Warning: the live `jobs` table is not partitioned, and nothing in Spooled reads or writes
> `jobs_partitioned`.** Migration `20241209000009_partitioned_jobs.sql` creates that second,
> empty table as scaffolding, and the helpers below manage only it. Do **not** follow the
> "swap tables" notes at the end of that migration (`ALTER TABLE jobs RENAME TO jobs_legacy;
> ALTER TABLE jobs_partitioned RENAME TO jobs;`) on a deployment you care about:
>
> - **Idempotency breaks.** PostgreSQL requires a unique index on a partitioned table to
>   contain the partition key, so the index on `jobs_partitioned` is
>   `(organization_id, idempotency_key, created_at)`: unique per key *and* creation time, which
>   no two jobs share, so a repeated key is no longer caught. Spooled's enqueue statements use
>   `ON CONFLICT (organization_id, idempotency_key) WHERE idempotency_key IS NOT NULL`, which
>   needs a unique index on exactly those columns (`idx_jobs_idempotency` on the real `jobs`
>   table); on the swapped table there is none to infer, and enqueueing fails.
> - **The copy fails.** `jobs_partitioned` lacks columns that `jobs` has gained
>   (`completion_webhook`, `workflow_id`, `workflow_step`, `dependency_mode`,
>   `completion_webhook_secret`, `lease_duration_secs`), so `INSERT INTO jobs_partitioned
>   SELECT * FROM jobs` does not run.
> - **Integrity is lost.** It has none of the foreign keys to `jobs(id)` (job history,
>   dependencies, schedule runs, `parent_job_id`; a primary key of `(id, created_at)` cannot be
>   referenced by `id` alone), none of the CHECK constraints and none of the triggers that
>   workflow progress, dependent-job release and the worker counters rely on.
>
> Partitioning the live table would need a purpose-built migration, planned and tested against a
> copy of production data; it is deferred (BEJOB-17 in `docs/ai/knowledge/bugs`). The functions
> below are safe to leave alone: `maintain_jobs_partitions()` only drops partitions of the
> unused table.

### Monthly Partitioning

These helpers create and maintain monthly partitions of `jobs_partitioned` (by `created_at`),
the unused table described above:

```sql
-- View partition health
SELECT * FROM check_partition_health();

-- Manually create partition for next month
SELECT create_jobs_partition_for_month(CURRENT_DATE + INTERVAL '1 month');

-- Run full maintenance
CALL maintain_jobs_partitions();
```

### Partition Maintenance

Run daily via cron or pg_cron:

```bash
# Using external script
./scripts/partition_maintenance.sh

# Using pg_cron (in PostgreSQL)
SELECT cron.schedule('maintain-partitions', '0 2 * * *', 'CALL maintain_jobs_partitions()');
```

### Retention Policy

Default retention: **6 months**

To change retention, modify `maintain_jobs_partitions()`:

```sql
-- Change from 6 to 12 months
retention_months INT := 12;
```

### Data Retention Sweeps

The scheduler removes old data every 5 minutes, per organization, using the plan's retention
(`custom_limits.job_retention_days` / `history_retention_days` override it):

| Data | Retention | Notes |
|------|-----------|-------|
| Completed and cancelled jobs | Job retention (Free 3 days, Starter 14, Pro 30, Enterprise 90) | A cancelled step of a workflow that still exists is kept |
| Failed and dead-lettered jobs, and their `dead_letter_queue` rows | Job retention | Kept while a pending, scheduled or processing job waits on them, and while their workflow exists (a failed workflow stays retryable until it is purged) |
| Workflows | Job retention, after they finish | Their steps are removed in the same pass |
| `schedule_runs`, job history, delivery history | History retention (Free 1 day, Starter 7, Pro 30, Enterprise 90) | |
| Workers | 7 days after going `offline` | Kept while a job is still assigned to them |

Each statement is a bounded batch (5,000 jobs, 10,000 rows of the other kinds), so a backlog (a
plan downgrade, a lowered override, the first passes after upgrading to 0.1.121) is worked off over
several passes rather than in one long statement; the scheduler loop also fires cron schedules
and recovers leases, so no statement may hold it for long. Rows another transaction holds, or that
a retry just changed, are skipped and picked up on a later pass. Deleted rows leave free space
for reuse; the table files shrink only after `VACUUM FULL` or `pg_repack`.

---

## Autovacuum Tuning

High-churn tables have aggressive autovacuum settings:

| Table | `vacuum_scale_factor` | Description |
|-------|----------------------|-------------|
| `jobs` | `0.02` (2%) | Very high update rate |
| `workers` | `0.02` (2%) | Frequent heartbeats |
| `job_history` | `0.05` (5%) | Insert-heavy |
| `schedules` | `0.05` (5%) | Moderate updates |
| `queue_config` | `0.20` (20%) | Rarely changes |

### Monitoring Autovacuum

```sql
-- Check dead tuple count
SELECT schemaname, relname, n_dead_tup, last_autovacuum
FROM pg_stat_user_tables
WHERE n_dead_tup > 10000
ORDER BY n_dead_tup DESC;

-- Check autovacuum activity
SELECT * FROM pg_stat_progress_vacuum;
```

### Manual Vacuum

If dead tuples accumulate:

```sql
-- Analyze and vacuum specific table
VACUUM ANALYZE jobs;

-- Full vacuum (requires exclusive lock - use during maintenance)
VACUUM FULL jobs;
```

---

## Monitoring & Alerting

### Key Metrics

| Metric | Description | Alert Threshold |
|--------|-------------|-----------------|
| `spooled_job_max_age_seconds` | Oldest pending job | > 300s warning, > 900s critical |
| `spooled_jobs_pending` | Queue depth | Growing trend |
| `spooled_workers_healthy` | Healthy workers | == 0 critical |
| `spooled_jobs_deadlettered_total` | DLQ rate | > 5 in 15 min |
| `spooled_api_request_duration_seconds` | API latency | p99 > 1s |
| `spooled_database_connections_active` / `spooled_database_connections_max` | Pool usage | > 80% warning, > 95% critical |
| `spooled_outgoing_webhook_deliveries_dropped_total` | Org webhook events dropped because the delivery queue (or one webhook's share) was full | Any increase |
| `spooled_outgoing_webhook_queue_depth` | Org webhook events and deliveries queued and not resolved yet | Close to 16 x `OUTGOING_WEBHOOK_MAX_CONCURRENT_DELIVERIES` (1,024 by default) |
| `spooled_outgoing_webhook_deliveries_in_flight` | Org webhook requests in flight | At `OUTGOING_WEBHOOK_MAX_CONCURRENT_DELIVERIES` for long |
| `spooled_completion_webhooks_dropped_total` | Per-job completion webhooks dropped because their queue was full | Any increase |

`spooled_api_requests_total{method,route,status}` and `spooled_api_request_duration_seconds{method,route}` use the matched route template (`unmatched` for 404s) and a status class (`2xx`, `4xx`, ...). There is no Redis metric.

### Health, Supervision and Shutdown

- `GET /health` answers `503` (same JSON body) while the database is down, so `curl -f` healthchecks fail. A Redis outage, a stopped gRPC server or a stalled scheduler keep `200` and show as `"cache": false`, `"grpc": false` or `"scheduler": false` with `"status": "degraded"`. `/health/live` and `/health/ready` are unchanged.
- The process exits non-zero (so `restart: unless-stopped` or the orchestrator restarts it) when the gRPC port is busy or the gRPC TLS certificate or key is bad, and when the gRPC server, the scheduler or the metrics server stops. A `METRICS_PORT` that cannot be bound therefore causes a restart loop.
- On SIGTERM the gRPC health status turns `NOT_SERVING`, in-flight gRPC calls get up to 10 s while HTTP drains, and the whole shutdown is bounded by 30 s before jobs held by the instance are released and the database pool is closed. Give the container at least 40 s: `docker-compose.prod.yml` sets `stop_grace_period: 40s`; the Kubernetes base manifest sets `terminationGracePeriodSeconds: 40` (raise both together if you lengthen the shutdown).
- gRPC health (`grpc.health.v1`, whole server and each service) follows database reachability, checked every 10 s. Redis state is deliberately not part of it.
- Every Redis call is capped at 500 ms, so a stalled Redis degrades features (rate limiting, session revocation, API-key cache) instead of stalling requests. See the [Redis runbook](./runbooks/redis-unavailable.md).

### Prometheus Scrape Config

```yaml
- job_name: 'spooled-backend'
  static_configs:
    - targets: ['spooled-backend:9090']
  metrics_path: '/metrics'
  scrape_interval: 10s
  # Needed when the backend has METRICS_TOKEN set
  # authorization: { type: Bearer, credentials: 'your-metrics-token' }
```

The bundled Prometheus in `docker-compose.prod.yml` does this itself: it reads `METRICS_TOKEN`, sends it as a bearer credential, and loads the alert rules from `docker/prometheus/spooled-rules.yml` (mounted from the checkout; a single downloaded compose file has no rules directory and therefore loads no rules).

### Grafana Dashboard

Pre-built dashboard at: `docker/grafana/dashboards/spooled-overview.json`

Panels include:
- Job throughput (enqueue/complete/fail rates)
- Queue depth (pending/processing)
- Latency percentiles (p50/p95/p99)
- Worker health
- Database connections
- Error rates

### Alertmanager Configuration

Alertmanager handles alert routing and notifications. Configuration: `docker/alertmanager/alertmanager.yml`

#### Alert Severity Levels

| Severity | Response Time | Notification Channel | Repeat Interval |
|----------|---------------|---------------------|-----------------|
| Critical | Immediate (10s group wait) | PagerDuty/SMS/Phone | 1 hour |
| Warning | 30 seconds | Slack/Email | 4 hours |

#### Critical Alerts (Page-Worthy)

These alerts require immediate attention:

| Alert | Condition | Action |
|-------|-----------|--------|
| `NoHealthyWorkers` | No workers reporting healthy | Check worker processes, network, deployments |
| `DatabaseConnectionsCritical` | > 95% of the pool in use | Scale pool, investigate connection leaks |
| `CriticalJobFailureRate` | > 25% job failure rate | Check job handlers, payloads, downstream services |
| `JobQueueBacklogCritical` | Oldest job > 15 min | Scale workers, check processing bottlenecks |
| `CriticalAPILatency` | p99 > 5 seconds | Check database, reduce load, scale |
| `AllWorkersDegraded` | Workers active but unhealthy | Check network, heartbeat issues |

#### Warning Alerts (Non-Urgent)

These alerts indicate issues that need attention but aren't emergencies:

| Alert | Condition | Action |
|-------|-----------|--------|
| `JobQueueBacklog` | Oldest job > 5 min | Monitor, consider scaling |
| `HighJobFailureRate` | > 10% job failure rate | Investigate failing jobs |
| `JobsDeadlettered` | > 5 DLQ jobs in 15 min | Check DLQ, investigate patterns |
| `HighAPILatency` | p99 > 1 second | Monitor, optimize queries |
| `WorkerCountDrop` | > 50% workers lost | Check deployments, scaling events |
| `JobAccumulationRate` | Enqueue > completion rate | Plan capacity increase |

#### Setting Up Notifications

1. **Email**: Uncomment `email_configs` and configure SMTP in `alertmanager.yml`
2. **Slack**: Add Slack webhook URL to `slack_api_url` and configure `slack_configs`
3. **PagerDuty**: Add service key to `pagerduty_configs` for critical alerts

Example Slack configuration:

```yaml
receivers:
  - name: 'critical-alerts'
    slack_configs:
      - channel: '#alerts-critical'
        api_url: 'https://hooks.slack.com/services/YOUR/WEBHOOK/URL'
        send_resolved: true
        title: '{{ if eq .Status "firing" }}🚨{{ else }}✅{{ end }} {{ .GroupLabels.alertname }}'
```

#### Testing Alerts

Run the alert testing script to verify alerts fire correctly:

```bash
./loadtest/test-alerts.sh
```

---

## Security Configuration

### TLS/HTTPS

In production, webhooks require HTTPS:

```bash
# Enable HSTS header
ENABLE_HSTS=true

# Webhooks check X-Forwarded-Proto header
# Configure your reverse proxy to set this header
```

### CORS Configuration

Development mode: Allow any origin
Production mode: Restricted to configured origins

```bash
# Set allowed origins in production
CORS_ALLOWED_ORIGINS=https://app.yourdomain.com,https://admin.yourdomain.com
```

Both the production and the development CORS layer expose these response headers to browsers:
`X-RateLimit-Limit`, `X-RateLimit-Remaining`, `X-RateLimit-Reset`, `X-RateLimit-Scope`,
`Retry-After`, `X-Request-Id` and `X-Has-More`. Without that a cross-origin dashboard could not read
any of them. A proxy in front that rewrites CORS headers must keep `Access-Control-Expose-Headers`.

### JWT Security

```bash
# Production requires minimum 32 character secret
JWT_SECRET=your-very-long-and-random-secret-at-least-32-chars
JWT_EXPIRATION_HOURS=24
```

### Billing Webhook Security

```bash
# Required for Stripe billing webhooks (subscription management)
STRIPE_BILLING_WEBHOOK_SECRET=whsec_your-stripe-secret-min-16-chars
```

The billing endpoint `/api/v1/billing/webhook` must be subscribed to these Stripe events:
`checkout.session.completed`, `checkout.session.async_payment_succeeded`,
`checkout.session.async_payment_failed`, `customer.subscription.created`,
`customer.subscription.updated`, `customer.subscription.deleted`, `invoice.paid` and
`invoice.payment_failed`. Without the two `async_payment_*` events a checkout paid by a delayed
method (SEPA, ACH) is never linked to its organization.

A paid plan is bought through `POST /api/v1/billing/checkout`, which creates a Checkout Session
bound to the authenticated organization. Only such a session is trusted to say which
organization paid. A legacy Payment Link checkout (organization named by a `client_reference_id`
anyone can edit) links only an organization with no live subscription whose **verified** billing
email the payer used, and never cancels a subscription; otherwise the log shows
`Not linking a Payment Link checkout` and the customer has to be linked by hand. Variables:
`STRIPE_SECRET_KEY`, `STRIPE_STARTER_PRICE_ID`, `STRIPE_PRO_PRICE_ID` (checkout answers 503
without them) and the optional `FRONTEND_URL` (where Checkout returns the customer,
default `https://spooled.cloud`).

### Login Codes: Turnstile

`TURNSTILE_SECRET_KEY` turns on a Cloudflare Turnstile check for `POST /api/v1/auth/email/start`:
the request must carry a valid `turnstile_token`, else `400 CAPTCHA_FAILED` (an unreachable
Turnstile counts as failed). Keep it unset until the website and the dashboard both render the
widget and send the token. The per-client throttle on that route applies either way.

### Custom Webhook Security

Organizations can configure a webhook token in their settings. When set, incoming
custom webhooks must include the `X-Webhook-Token` header with the correct value.

---

## Environment Variable Reference

### Server

| Variable | Default | Description |
|----------|---------|-------------|
| `HOST` | `0.0.0.0` | Bind address |
| `PORT` | `8080` | REST API port |
| `GRPC_PORT` | `50051` | gRPC API port (HTTP/2 + Protobuf) |
| `METRICS_PORT` | `9090` | Prometheus metrics port |
| `RUST_ENV` | `production` | Environment (development/staging/production); unset or unknown means production |
| `EXTERNAL_URL` | Unset | Public base URL of the API |
| `CORS_ALLOWED_ORIGINS` | spooled.cloud origins | Comma-separated allowed origins; blank keeps the default list, so self-hosters on their own domain must set it for the dashboard |
| `ENABLE_HSTS` | `false` | Send `Strict-Transport-Security` (only behind HTTPS) |
| `METRICS_TOKEN` | Unset | Bearer token required on `/metrics` (and sent by the bundled Prometheus) |

`docker-compose.prod.yml` forwards the settings in this table as `${NAME:-default}`. A blank value counts as unset for numeric, flag and optional-string settings, so forwarding an empty variable changes nothing.

### Database

| Variable | Default | Description |
|----------|---------|-------------|
| `DATABASE_URL` | Required | PostgreSQL URL (via PgBouncer) |
| `DATABASE_DIRECT_URL` | Optional | Direct PostgreSQL URL, bypassing PgBouncer. When set, migrations run over it (one connection) instead of `DATABASE_URL`; blank = unset |
| `DATABASE_MAX_CONNECTIONS` | `25` | Connection pool max |
| `DATABASE_MIN_CONNECTIONS` | `5` | Connection pool min |
| `DATABASE_ACQUIRE_TIMEOUT_SECS` | `30` | How long a request waits for a pooled connection |
| `DATABASE_LIST_STATEMENT_TIMEOUT_MS` | `5000` | Statement timeout of the job list reads (`GET /jobs`, `GET /jobs/dlq`), set with `SET LOCAL` in each list's own read-only transaction. Postgres stops a list that runs longer (also one whose client went away) and the API answers `400 QUERY_TIMEOUT`; `0` is refused because Postgres reads it as no timeout |
| `DATABASE_TEST_BEFORE_ACQUIRE` | `false` | Validate each connection on checkout (adds about 5 ms) |

Postgres connections use TLS when the URL asks for it (`sslmode=require` / `verify-*`), and Redis when the URL is `rediss://` (rustls with the ring provider and the bundled public web roots; a private CA may need a different root setup). Plain `postgres://` and `redis://` URLs behave as before.

### Redis

| Variable | Default | Description |
|----------|---------|-------------|
| `REDIS_URL` | `redis://localhost:6379` | Redis connection URL |
| `REDIS_POOL_SIZE` | `10` | Connection pool size |

### Workers

| Variable | Default | Description |
|----------|---------|-------------|
| `WORKER_HEARTBEAT_INTERVAL_SECS` | `10` | Heartbeat frequency |
| `WORKER_LEASE_DURATION_SECS` | `30` | Default job lease for a REST claim or heartbeat that sets none |
| `GRPC_DEFAULT_LEASE_DURATION_SECS` | `300` | Lease for gRPC `Dequeue` / `StreamJobs` / `ProcessJobs` / `RenewLease` when the request sets none (`0` or blank = 300; at most 3600 s; at or below `WORKER_HEARTBEAT_INTERVAL_SECS` the server refuses to start). Returned by gRPC `Register`. Kept at 300 by default because the Go SDK's `StreamJobs` cannot send a lease |
| `WORKER_MAX_CONCURRENCY` | `5` | Max jobs per worker |
| `WORKER_FALLBACK_POLL_INTERVAL_SECS` | `5` | Poll interval when Redis unavailable |

### Outgoing Webhooks

| Variable | Default | Description |
|----------|---------|-------------|
| `OUTGOING_WEBHOOK_MAX_ATTEMPTS` | `5` | Delivery attempts per event before giving up (exponential backoff between them) |
| `OUTGOING_WEBHOOK_MAX_CONCURRENT_DELIVERIES` | `64` | Delivery senders and request slots process-wide. Also sizes the bounded queue (16 per sender; events past it are dropped and counted) and one webhook's share (a quarter of the senders and of the queue). Per-job completion webhooks get their own slots of the same size |

Each in-flight delivery holds a copy of the job payload for up to the request timeout plus
retry backoff, so raise the concurrency ceiling only with memory and socket headroom to
match. Without it, one webhook pointed at a black-holing endpoint grows tasks and sockets
until the process dies.

A webhook is disabled automatically after 20 consecutive failed deliveries
(`enabled = false`, `last_status = "auto_disabled"`); re-enable it with
`PUT /api/v1/outgoing-webhooks/{id}` and `{"enabled": true}`.

`workflow.completed` and `workflow.failed` are sent by the scheduler, not by the request that
finished the workflow: a pass at the end of the 10 second dependency tick claims up to 500 workflows
that ended in the last hour (`workflows.announced_completed_at` records what was sent), commits, and
then queues the events. They are **at most once**: a restart between the claim and the recorded
delivery, a full delivery queue (`spooled_outgoing_webhook_deliveries_dropped_total` moves) or a
scheduler that did not run for over an hour loses them, and a workflow retried within one tick never
announces its failure. If the scheduler is stalled (`/health` shows `"scheduler": false`) nothing is
announced and outcomes older than an hour are skipped when it recovers. `job.deadlettered` is derived
from `job.failed` at dispatch and goes through the same bounded queue. The `format` of a webhook
(`json`, `slack`, `discord`, `teams`) is rendered at delivery; a chat service that answers `400`
to a delivery usually means the webhook still has the default `json` format.

### Trusted Proxy / Client IP

| Variable | Default | Description |
|----------|---------|-------------|
| `TRUSTED_CLIENT_IP_HEADER` | Unset | Edge header carrying the real client IP, believed from any peer once set. Unset: `CF-Connecting-IP`, then `X-Real-IP`, believed only from a loopback/private/link-local peer |
| `TRUSTED_PROXY_HOPS` | `0` | Entries **your own** proxies append to `X-Forwarded-For`, counted from the right. `0` means that header is never consulted |

See [Client IP Resolution](#client-ip-resolution) for why the leftmost `X-Forwarded-For`
entry is not trusted.

### gRPC

| Variable | Default | Description |
|----------|---------|-------------|
| `GRPC_PORT` | `50051` | gRPC server port |
| `GRPC_MAX_MESSAGE_SIZE` | `4194304` | Max message size (4MB) |
| `GRPC_HEALTH_ENABLED` | `true` | Enable gRPC health service |
| `GRPC_REFLECTION_ENABLED` | `true` | Enable gRPC reflection (disable in prod if needed) |

### Email

| Variable | Default | Description |
|----------|---------|-------------|
| `EMAIL_PROVIDER` | `console` | `resend`, `sendgrid`, `postmark` or `smtp`. **Required when `RUST_ENV=production`**: the backend refuses to start with `console` (login codes would only be logged), and an unset or misspelled value resolves to `console` |
| `EMAIL_API_KEY` | Unset | API key of the provider (SMTP uses `SMTP_*`) |
| `TURNSTILE_SECRET_KEY` | Unset | Cloudflare Turnstile secret; when set, `POST /auth/email/start` requires a valid `turnstile_token` |

### Billing

| Variable | Default | Description |
|----------|---------|-------------|
| `STRIPE_SECRET_KEY` | Unset | Stripe API key; without it checkout answers 503 |
| `STRIPE_BILLING_WEBHOOK_SECRET` | Unset | Signing secret of the billing webhook endpoint |
| `STRIPE_STARTER_PRICE_ID`, `STRIPE_PRO_PRICE_ID` | Unset | Price ids `POST /billing/checkout` sells and the webhook maps back to a plan |
| `FRONTEND_URL` | `https://spooled.cloud` | Website base URL Checkout returns to (`/account/billing?checkout=success\|cancel`) |

### Tracing

| Variable | Default | Description |
|----------|---------|-------------|
| `OTEL_EXPORTER_OTLP_ENDPOINT` | Optional | Jaeger/OTLP endpoint |
| `OTEL_SERVICE_NAME` | `spooled-backend` | Service name in traces |
| `JSON_LOGS` | `false` | Enable JSON structured logging |

---

## Backup & Disaster Recovery

### Backup Strategy Overview

| Data | Backup Method | Frequency | Retention |
|------|---------------|-----------|-----------|
| PostgreSQL | Logical + PITR | Daily + continuous WAL | 30 days |
| Redis | RDB + AOF | Hourly RDB, continuous AOF | 7 days |
| Configuration | Git + secrets manager | On change | Indefinite |

### PostgreSQL Backup

#### Logical Backup (pg_dump)

```bash
#!/bin/bash
# scripts/backup_database.sh

BACKUP_DIR="/var/backups/spooled"
TIMESTAMP=$(date +%Y%m%d_%H%M%S)
DB_URL="${DATABASE_DIRECT_URL:-postgres://spooled:password@localhost:5432/spooled}"

# Create backup directory
mkdir -p "$BACKUP_DIR"

# Full logical backup (compressed)
pg_dump "$DB_URL" \
  --format=custom \
  --compress=9 \
  --file="$BACKUP_DIR/spooled_$TIMESTAMP.dump"

# Keep only last 30 days
find "$BACKUP_DIR" -name "spooled_*.dump" -mtime +30 -delete

echo "Backup completed: spooled_$TIMESTAMP.dump"
```

Schedule with cron:

```bash
# Daily at 2 AM
0 2 * * * /usr/local/bin/spooled-backup-database.sh >> /var/log/spooled/backup.log 2>&1
```

#### Point-in-Time Recovery (PITR)

For managed PostgreSQL (AWS RDS, Cloud SQL, etc.), enable PITR:

```bash
# AWS RDS - Enable automated backups
aws rds modify-db-instance \
  --db-instance-identifier spooled-db \
  --backup-retention-period 30 \
  --preferred-backup-window "02:00-03:00"
```

For self-hosted PostgreSQL, configure WAL archiving:

```bash
# postgresql.conf
archive_mode = on
archive_command = 'cp %p /var/lib/postgresql/wal_archive/%f'
wal_level = replica
```

#### Restore from Backup

```bash
# Restore from logical backup
pg_restore \
  --dbname="postgres://spooled:password@localhost:5432/spooled" \
  --clean \
  --if-exists \
  /var/backups/spooled/spooled_20241209_020000.dump

# Verify restoration
psql "$DATABASE_URL" -c "SELECT COUNT(*) FROM jobs;"
psql "$DATABASE_URL" -c "SELECT COUNT(*) FROM organizations;"
```

### Redis Backup

#### RDB Snapshots

Redis is configured for persistence in docker-compose:

```yaml
redis:
  command:
    - "redis-server"
    - "--appendonly"
    - "yes"
    - "--appendfsync"
    - "everysec"
```

Manual backup:

```bash
# Trigger RDB snapshot
redis-cli -h localhost BGSAVE

# Copy RDB file
cp /var/lib/redis/dump.rdb /var/backups/redis/dump_$(date +%Y%m%d).rdb
```

#### Restore Redis

```bash
# Stop Redis
docker-compose stop redis

# Replace RDB file
cp /var/backups/redis/dump_20241209.rdb /var/lib/redis/dump.rdb

# Start Redis
docker-compose start redis
```

### Disaster Recovery Procedures

#### Scenario 1: Database Corruption

1. **Stop the application** to prevent further corruption:
   ```bash
   docker-compose stop spooled-backend
   ```

2. **Assess the damage**:
   ```bash
   psql "$DATABASE_URL" -c "SELECT * FROM pg_stat_user_tables;"
   ```

3. **Restore from backup**:
   ```bash
   pg_restore --dbname="$DATABASE_URL" --clean /var/backups/spooled/latest.dump
   ```

4. **Run migrations** to ensure schema is current:
   ```bash
   sqlx migrate run
   ```

5. **Restart application**:
   ```bash
   docker-compose start spooled-backend
   ```

#### Scenario 2: Complete Infrastructure Loss

1. **Provision new infrastructure** (VMs, containers, etc.)

2. **Restore database** from off-site backup

3. **Restore Redis** (or start fresh - cache will rebuild)

4. **Deploy application**:
   ```bash
   docker-compose up -d
   ```

5. **Verify health**:
   ```bash
   curl http://localhost:8080/health
   ```

#### Scenario 3: Redis Failure

Redis is optional - the system degrades gracefully:

1. **Workers switch to fallback polling** (every 5 seconds)
2. **Real-time features disabled** (WebSocket notifications)
3. **Rate limiting uses strict fallback** (10 req/min)

To recover:

```bash
# Restart Redis
docker-compose restart redis

# Workers will automatically reconnect
```

### Recovery Time Objectives (RTO)

| Scenario | Target RTO | Notes |
|----------|------------|-------|
| Redis failure | 0 min | Automatic fallback |
| Single pod failure | 30 sec | Kubernetes restarts |
| Database failover | 5 min | Managed DB automatic |
| Full restore from backup | 30 min | Depends on data size |
| Complete rebuild | 2 hours | Full infrastructure |

### Recovery Point Objectives (RPO)

| Data | RPO | Method |
|------|-----|--------|
| Jobs | 0 | PostgreSQL ACID |
| Job history | 5 min | WAL archiving |
| Configuration | 0 | Git versioned |
| Redis cache | 1 sec | AOF persistence |

### Backup Verification

Test backups monthly:

```bash
#!/bin/bash
# scripts/verify_backup.sh

# Create test database
createdb spooled_test

# Restore latest backup
pg_restore --dbname=spooled_test /var/backups/spooled/latest.dump

# Verify data integrity
psql spooled_test -c "
  SELECT 
    (SELECT COUNT(*) FROM jobs) as jobs,
    (SELECT COUNT(*) FROM organizations) as orgs,
    (SELECT COUNT(*) FROM workers) as workers;
"

# Cleanup
dropdb spooled_test

echo "Backup verification complete"
```

### Alerting for Backup Failures

Add to your monitoring:

```yaml
# prometheus rules
- alert: BackupJobFailed
  expr: time() - backup_last_success_timestamp > 86400 * 2
  for: 1h
  labels:
    severity: critical
  annotations:
    summary: "Database backup has not succeeded in 2 days"
```
