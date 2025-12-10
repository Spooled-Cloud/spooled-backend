# Spooled Backend Operations Guide

This guide covers operational configuration, tuning, and best practices for running Spooled Backend in production.

## Table of Contents

1. [Connection Pooling (PgBouncer)](#connection-pooling-pgbouncer)
2. [Rate Limiting & Backpressure](#rate-limiting--backpressure)
3. [Redis Configuration](#redis-configuration)
4. [Queue Partitioning](#queue-partitioning)
5. [Autovacuum Tuning](#autovacuum-tuning)
6. [Monitoring & Alerting](#monitoring--alerting)
7. [Security Configuration](#security-configuration)
8. [Backup & Disaster Recovery](#backup--disaster-recovery)

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

### Per-Client Rate Limiting

Rate limits are tracked per client:
- **API Key**: Hash of the API key
- **IP Address**: From `X-Forwarded-For` header
- **User-Agent**: Fallback for unidentified clients

### Request Size Limits

| Limit | Value | Description |
|-------|-------|-------------|
| `MAX_CONTENT_LENGTH` | `5 MB` | Maximum request body size |
| `MAX_PAYLOAD_SIZE` | `1 MB` | Maximum job payload size |
| `MAX_WEBHOOK_PAYLOAD_SIZE` | `5 MB` | Maximum webhook payload |

### Queue-Level Controls

```bash
# Pause a queue to stop processing
curl -X POST /api/v1/queues/emails/pause

# Resume processing
curl -X POST /api/v1/queues/emails/resume

# Configure queue limits
curl -X PUT /api/v1/queues/emails/config \
  -d '{"max_concurrent_jobs": 100, "rate_limit_per_second": 50}'
```

### Backpressure Indicators

Monitor these metrics for backpressure:

| Metric | Warning | Critical | Action |
|--------|---------|----------|--------|
| `spooled_job_max_age_seconds` | > 300s | > 900s | Add workers or scale |
| `spooled_jobs_pending` | Growing | > 10,000 | Check worker capacity |
| `spooled_database_connections_active` | > 80 | > 95 | Increase pool size |

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

### Monthly Partitioning

The jobs table can be partitioned by `created_at` for better performance:

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

### Prometheus Scrape Config

```yaml
- job_name: 'spooled-backend'
  static_configs:
    - targets: ['spooled-backend:9090']
  metrics_path: '/metrics'
  scrape_interval: 10s
  # Optional: add auth token
  # bearer_token: 'your-metrics-token'
```

### Grafana Dashboard

Pre-built dashboard at: `docker/grafana/dashboards/spooled-overview.json`

Panels include:
- Job throughput (enqueue/complete/fail rates)
- Queue depth (pending/processing)
- Latency percentiles (p50/p95/p99)
- Worker health
- Database connections
- Error rates

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

### JWT Security

```bash
# Production requires minimum 32 character secret
JWT_SECRET=your-very-long-and-random-secret-at-least-32-chars
JWT_EXPIRATION_HOURS=24
```

### Webhook Security

```bash
# Required for GitHub webhooks
GITHUB_WEBHOOK_SECRET=your-github-secret-min-16-chars

# Required for Stripe webhooks
STRIPE_WEBHOOK_SECRET=whsec_your-stripe-secret-min-16-chars
```

---

## Environment Variable Reference

### Server

| Variable | Default | Description |
|----------|---------|-------------|
| `HOST` | `0.0.0.0` | Bind address |
| `PORT` | `8080` | API port |
| `METRICS_PORT` | `9090` | Prometheus metrics port |
| `RUST_ENV` | `development` | Environment (development/staging/production) |

### Database

| Variable | Default | Description |
|----------|---------|-------------|
| `DATABASE_URL` | Required | PostgreSQL URL (via PgBouncer) |
| `DATABASE_DIRECT_URL` | Optional | Direct PostgreSQL URL (for migrations) |
| `DATABASE_MAX_CONNECTIONS` | `25` | Connection pool max |
| `DATABASE_MIN_CONNECTIONS` | `5` | Connection pool min |

### Redis

| Variable | Default | Description |
|----------|---------|-------------|
| `REDIS_URL` | `redis://localhost:6379` | Redis connection URL |
| `REDIS_POOL_SIZE` | `10` | Connection pool size |

### Workers

| Variable | Default | Description |
|----------|---------|-------------|
| `WORKER_HEARTBEAT_INTERVAL_SECS` | `10` | Heartbeat frequency |
| `WORKER_LEASE_DURATION_SECS` | `30` | Job lock timeout |
| `WORKER_MAX_CONCURRENCY` | `5` | Max jobs per worker |
| `WORKER_FALLBACK_POLL_INTERVAL_SECS` | `5` | Poll interval when Redis unavailable |

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
0 2 * * * /opt/spooled/scripts/backup_database.sh >> /var/log/spooled/backup.log 2>&1
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

