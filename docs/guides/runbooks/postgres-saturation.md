# Runbook: PostgreSQL Connection Saturation

**Severity**: Critical  
**Alert**: `DatabaseConnectionsCritical` (> 95 connections)  
**Related Alert**: `DatabaseConnectionsHigh` (> 80 connections)

---

## Symptoms

- API requests failing with `connection pool exhausted` errors
- Slow response times across all endpoints
- Alert firing: `DatabaseConnectionsCritical` or `DatabaseConnectionsHigh`
- Grafana shows connection count near or at maximum
- Application logs show `FATAL: too many connections for role`

---

## Diagnosis

### 1. Check Current Connection Count

```bash
# Via metrics endpoint
curl -s http://localhost:9090/metrics | grep spooled_database_connections

# Via PostgreSQL directly
psql "$DATABASE_DIRECT_URL" -c "
SELECT 
  count(*) as total,
  count(*) FILTER (WHERE state = 'active') as active,
  count(*) FILTER (WHERE state = 'idle') as idle,
  count(*) FILTER (WHERE state = 'idle in transaction') as idle_in_tx
FROM pg_stat_activity 
WHERE datname = 'spooled';
"
```

### 2. Identify Connection Sources

```bash
# Connections by application/client
psql "$DATABASE_DIRECT_URL" -c "
SELECT 
  application_name,
  client_addr,
  count(*) as connections,
  count(*) FILTER (WHERE state = 'active') as active
FROM pg_stat_activity 
WHERE datname = 'spooled'
GROUP BY application_name, client_addr
ORDER BY connections DESC;
"
```

### 3. Check for Long-Running Queries

```bash
psql "$DATABASE_DIRECT_URL" -c "
SELECT 
  pid,
  now() - pg_stat_activity.query_start AS duration,
  query,
  state
FROM pg_stat_activity
WHERE (now() - pg_stat_activity.query_start) > interval '1 minute'
  AND state != 'idle'
ORDER BY duration DESC;
"
```

### 4. Check PgBouncer Status

```bash
# PgBouncer stats
psql -h localhost -p 6432 -U spooled pgbouncer -c "SHOW POOLS;"
psql -h localhost -p 6432 -U spooled pgbouncer -c "SHOW STATS;"
psql -h localhost -p 6432 -U spooled pgbouncer -c "SHOW CLIENTS;"
```

---

## Resolution

### Immediate Actions

#### 1. Kill Idle-in-Transaction Connections

```bash
# Find and terminate idle-in-transaction connections older than 5 minutes
psql "$DATABASE_DIRECT_URL" -c "
SELECT pg_terminate_backend(pid)
FROM pg_stat_activity
WHERE state = 'idle in transaction'
  AND query_start < now() - interval '5 minutes'
  AND pid != pg_backend_pid();
"
```

#### 2. Kill Long-Running Queries

```bash
# Terminate queries running longer than 10 minutes
psql "$DATABASE_DIRECT_URL" -c "
SELECT pg_terminate_backend(pid)
FROM pg_stat_activity
WHERE state = 'active'
  AND query_start < now() - interval '10 minutes'
  AND pid != pg_backend_pid();
"
```

#### 3. Temporarily Increase Pool Size

If using PgBouncer, temporarily increase limits:

```bash
# Edit PgBouncer config (hot reload)
docker exec spooled-pgbouncer sh -c 'echo "default_pool_size = 75" >> /bitnami/pgbouncer/conf/pgbouncer.ini'
docker exec spooled-pgbouncer pkill -HUP pgbouncer
```

Or restart with new environment variable:

```bash
docker-compose stop pgbouncer
PGBOUNCER_DEFAULT_POOL_SIZE=75 docker-compose up -d pgbouncer
```

### If Problem Persists

#### 4. Identify and Fix Application Issues

Common causes:
- **Connection leak**: Application not releasing connections after use
- **Missing pool exhaustion handling**: No timeout on connection acquire
- **Runaway process**: Single process opening many connections

Check application logs for connection-related errors:
```bash
grep -i "connection\|pool\|database" /var/log/spooled/*.log | tail -100
```

#### 5. Scale Down Non-Critical Workers

Temporarily reduce worker count to free connections:

```bash
# If using Kubernetes
kubectl scale deployment spooled-workers --replicas=2
```

#### 6. Restart Application Pods

As a last resort, rolling restart releases all connections:

```bash
# Kubernetes
kubectl rollout restart deployment spooled-backend

# Docker Compose
docker-compose restart spooled-backend
```

---

## Prevention

### Configuration Best Practices

1. **Set connection timeouts in application**:
   ```bash
   DATABASE_POOL_TIMEOUT=30  # Wait max 30s for connection
   DATABASE_MAX_CONNECTIONS=25  # Per process
   ```

2. **Use PgBouncer for connection pooling**:
   ```bash
   # Application connects to PgBouncer (port 6432)
   DATABASE_URL=postgres://user:pass@pgbouncer:6432/spooled
   
   # Migrations use direct connection (port 5432)
   DATABASE_DIRECT_URL=postgres://user:pass@postgres:5432/spooled
   ```

3. **Enable connection recycling**:
   ```bash
   PGBOUNCER_SERVER_IDLE_TIMEOUT=300  # Recycle idle connections after 5 min
   ```

### Monitoring

Add to your monitoring checklist:
- [ ] Alert on > 80% connection usage (warning)
- [ ] Alert on > 95% connection usage (critical)
- [ ] Dashboard panel showing connection pool usage
- [ ] Log slow queries > 1 second

### Capacity Planning

| Workers | Recommended Pool Size | Max DB Connections |
|---------|----------------------|-------------------|
| 1-5 | 25 | 50 |
| 5-10 | 50 | 100 |
| 10-20 | 75 | 150 |
| 20+ | 100+ | 200+ |

---

## Related Documentation

- [Operations Guide - Connection Pooling](../operations.md#connection-pooling-pgbouncer)
- [PgBouncer Documentation](https://www.pgbouncer.org/config.html)

