# Runbook: Redis Unavailable

**Severity**: Warning (degraded) / Critical (if persistent)  
**Alert**: `RedisUnavailable` (no operations for 2+ minutes)  
**Related Alert**: `RedisOperationsLow` (significant drop in operations)

---

## Symptoms

- Workers logging `Redis connection failed, using fallback polling`
- Real-time features (WebSocket, SSE) not working
- Rate limiting using conservative fallback (10 req/min)
- Alert firing: `RedisUnavailable` or `RedisOperationsLow`
- Grafana shows Redis operations dropped to zero

---

## Impact Assessment

Redis is **not critical** - the system degrades gracefully:

| Feature | Without Redis | Impact |
|---------|--------------|--------|
| Job notifications | Fallback to DB polling (5s interval) | Slight delay in job pickup |
| WebSocket updates | Disabled | Dashboard not real-time |
| Rate limiting | 10 req/min fallback | More restrictive |
| Worker coordination | Direct DB queries | Higher DB load |

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

#### 7. Verify Metrics Flowing

```bash
# Redis operations should resume
curl -s http://localhost:9090/metrics | grep spooled_redis_operations
```

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
- [ ] Alert when Redis ping fails
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
