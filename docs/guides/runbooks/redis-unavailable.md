# Runbook: Redis Unavailable

**Severity**: Warning (degraded) / Critical (if persistent)  
**Detection**: `GET /health` returns `"cache": false` and `"status": "degraded"` (still HTTP 200). No Redis alert rule ships with the backend, because it exports no Redis metric; alert on the `/health` body or on a Redis exporter (`redis_up == 0`).

---

## Symptoms

- Workers logging `Redis connection failed, using fallback polling`
- Real-time features (WebSocket, SSE) not working
- Rate limiting using conservative fallback (10 req/min)
- `GET /health` reports `"cache": false` and `"status": "degraded"`
- Calls that use Redis (rate limiting, logout and session revocation, the API-key cache) take their fail-open / fail-closed path (`RATE_LIMIT_FAIL_CLOSED`) after at most half a second each: every Redis call is capped at 500 ms

---

## Impact Assessment

Redis is **not critical** - the system degrades gracefully:

| Feature | Without Redis | Impact |
|---------|--------------|--------|
| Job notifications | Fallback to DB polling (5s interval) | Slight delay in job pickup |
| WebSocket updates | Disabled: a socket whose Redis event channel cannot be opened or drops gets an `Error` event and closes with `1011` (since 0.1.121; it used to stay open and silent). Clients reconnect with backoff | Dashboard not real-time |
| Realtime connection caps | Fail open (a Redis error or the 500 ms timeout admits the connection) | Per-organization caps are not enforced meanwhile |
| Rate limiting | 10 req/min fallback | More restrictive |
| Worker coordination | Direct DB queries | Higher DB load |
| Email-login throttles (`email_start:*`, `email_verify_*`) | Start throttle fails open (429 with `RATE_LIMIT_FAIL_CLOSED`); the verify budgets are skipped and each code allows only 5 attempts | A stranger can burn an address's code again until Redis is back |

**Bottom line**: Jobs continue processing, but with higher latency and DB load.

---

## Diagnosis

### 1. Check Redis Container Status

```bash
# Docker status
docker ps | grep redis
docker logs spooled-redis --tail 50

# Container health
docker inspect spooled-redis | jq '.[0].State'
```

### 2. Test Redis Connectivity

```bash
# From host
redis-cli -h localhost -p 6379 ping

# From application container
docker exec spooled-backend redis-cli -h redis -p 6379 ping
```

### 3. Check Redis Memory

```bash
redis-cli INFO memory | grep -E "used_memory_human|maxmemory_human|used_memory_peak_human"
```

### 4. Check Redis Logs

```bash
docker logs spooled-redis --tail 100 | grep -i "error\|warning\|oom"
```

### 5. Check Network Connectivity

```bash
# From backend container, try to reach Redis
docker exec spooled-backend nc -zv redis 6379

# Check DNS resolution
docker exec spooled-backend nslookup redis
```

---

## Resolution

### Immediate Actions

#### 1. Restart Redis (Quick Fix)

```bash
# Docker Compose
docker-compose restart redis

# Verify it's up
docker-compose ps redis
redis-cli ping
```

#### 2. Check and Free Memory

If Redis is out of memory:

```bash
# Check memory status
redis-cli INFO memory

# Clear all keys (CAUTION: only if cache data is expendable)
redis-cli FLUSHALL

# Or selectively clear old data
redis-cli KEYS "queue:*" | xargs redis-cli DEL
```

#### 3. Fix Disk Space Issues

If AOF/RDB persistence is failing:

```bash
# Check disk space
df -h /var/lib/docker/volumes/

# If disk is full, clear old data
docker system prune -f
```

### If Redis Won't Start

#### 4. Check AOF File Corruption

```bash
# Check AOF file
docker exec spooled-redis redis-check-aof --fix /data/appendonly.aof

# If severely corrupted, remove and restart (loses cached data)
docker-compose stop redis
docker volume rm spooled-backend_redis_data
docker-compose up -d redis
```

#### 5. Check for Port Conflicts

```bash
# Check if port 6379 is in use
lsof -i :6379
netstat -tlnp | grep 6379

# Kill conflicting process if needed
```

### Recovery Verification

#### 6. Verify Workers Reconnected

```bash
# Check application logs for reconnection
docker logs spooled-backend --tail 50 | grep -i redis

# Should see: "Redis connection restored" or similar
```

#### 7. Verify Health

```bash
# "cache" should be true again and "status" healthy
curl -s http://localhost:8080/health
# {"status":"healthy","database":true,"cache":true,"grpc":true,"scheduler":true}
```

No backend restart is needed: the cache reconnects by itself, and a half-open connection (Redis host crashed or network path changed, so calls only time out) is replaced after three timeouts in a row. That includes a Redis that was down when the backend started, as long as `REDIS_URL` is set or `RATE_LIMIT_FAIL_CLOSED` is on (from 0.1.119; before, the backend ran without Redis until restarted). Without either, a Redis missing at startup counts as not configured and `/health` stays `healthy`. The `spooled_redis_operations_total` metric and its alerts were removed in 0.1.119 because nothing ever updated them.

---

## Prevention

### Configuration Best Practices

1. **Memory limits** (prevent OOM):
   ```yaml
   redis:
     command:
       - "--maxmemory"
       - "256mb"
       - "--maxmemory-policy"
       - "allkeys-lru"
   ```

2. **Persistence settings** (balance durability vs. performance):
   ```yaml
   redis:
     command:
       - "--appendonly"
       - "yes"
       - "--appendfsync"
       - "everysec"
   ```

3. **Connection pooling in application**:
   ```bash
   REDIS_POOL_SIZE=10
   REDIS_CONNECTION_TIMEOUT=5
   ```

### Monitoring

Add to your monitoring checklist:
- [ ] Alert when `/health` reports `"cache": false` (watch the JSON body; the status code stays 200) or when a Redis exporter reports `redis_up == 0`
- [ ] Alert when memory > 80%
- [ ] Dashboard panel for Redis memory and connections
- [ ] Track fallback polling activations

### Graceful Degradation Verification

Periodically test that fallback works:

1. Stop Redis: `docker-compose stop redis`
2. Verify workers continue processing (with fallback logs)
3. Restart Redis: `docker-compose start redis`
4. Verify normal operation resumes

---

## Expected Fallback Behavior

When Redis is unavailable:

```
[INFO] Redis connection failed, switching to fallback polling mode
[INFO] Workers will poll database every 5 seconds for new jobs
[WARN] Real-time features disabled until Redis reconnects
[INFO] Rate limiting using conservative fallback (10 req/min per client)
```

When Redis reconnects:

```
[INFO] Redis connection restored
[INFO] Resuming pub/sub notifications
[INFO] Real-time features re-enabled
```

---

## Related Documentation

- [Operations Guide - Redis Configuration](../operations.md#redis-configuration)
- [Redis Documentation](https://redis.io/docs/)
