# Runbook: Schedule Drift and Missed Runs

**Severity**: Warning  
**Related Issues**: Scheduled jobs not running, jobs running late, duplicate scheduled jobs

---

## Symptoms

- Scheduled jobs not running at expected times
- Jobs running significantly later than scheduled
- Cron jobs skipping runs entirely
- Customer reports of "schedule stopped working"
- Multiple instances of the same scheduled job running

---

## Diagnosis

### 1. Check Schedule Status

```sql
-- View all active schedules
SELECT 
  id,
  name,
  cron_expression,
  timezone,
  next_run_at,
  last_run_at,
  is_active,
  run_count
FROM schedules
WHERE is_active = true
ORDER BY next_run_at NULLS LAST;
```

### 2. Calculate Schedule Drift

```sql
-- Check for significant drift (next_run in the past)
SELECT 
  id,
  name,
  cron_expression,
  next_run_at,
  NOW() - next_run_at as drift,
  last_run_at
FROM schedules
WHERE is_active = true 
  AND next_run_at < NOW() - INTERVAL '5 minutes';
```

### 3. Check Recent Schedule Runs

```sql
-- Schedule executions in last hour
SELECT 
  sr.id AS run_id,
  sr.schedule_id,
  s.name AS schedule_name,
  sr.job_id,
  sr.status,
  sr.error_message,
  sr.started_at,
  sr.completed_at
FROM schedule_runs sr
JOIN schedules s ON s.id = sr.schedule_id
WHERE sr.started_at > NOW() - INTERVAL '1 hour'
ORDER BY sr.started_at DESC;
```

### 4. Check for schedules missing `next_run_at`

In this system, `next_run_at` is calculated by the application. If it is `NULL`, schedules won't trigger until it’s recalculated.

```sql
SELECT id, name, cron_expression, timezone, last_run_at, next_run_at
FROM schedules
WHERE is_active = true
  AND next_run_at IS NULL
ORDER BY last_run_at DESC NULLS LAST
LIMIT 50;
```

### 5. Check System Clock

```bash
# Server time
date
timedatectl

# Compare to NTP
ntpdate -q pool.ntp.org
```

### 6. Check Schedule Processor Health

```bash
# Look for schedule processing logs
docker logs spooled-backend --tail 200 | grep -i "schedule"

# If you're using a separate scheduler process/service, ensure it's running
# and has access to the database.
```

---

## Resolution

### Scenario A: Schedules Stuck (next_run_at in past)

**Cause**: Schedule processor crashed or schedule never updated after last run

#### Actions:

1. **Manually advance stuck schedules**:
   ```sql
   -- For a specific schedule, calculate next run
   UPDATE schedules
   SET next_run_at = (
     -- This should use your cron library to calculate
     -- Placeholder: set to next hour
     date_trunc('hour', NOW()) + INTERVAL '1 hour'
   )
   WHERE id = 'sch_XXX'
     AND next_run_at < NOW() - INTERVAL '5 minutes';
   ```

2. **Restart backend to reset schedule processor**:
   ```bash
   docker-compose restart spooled-backend
   ```

3. **Verify schedules are advancing**:
   ```sql
   SELECT id, name, next_run_at 
   FROM schedules 
   WHERE is_active = true
   ORDER BY next_run_at;
   ```

### Scenario B: Clock Skew

**Cause**: Server clock is wrong, causing schedule calculations to be off

#### Actions:

1. **Sync system clock**:
   ```bash
   sudo systemctl restart systemd-timesyncd
   # or
   sudo ntpdate -s pool.ntp.org
   ```

2. **Verify clock sync**:
   ```bash
   timedatectl
   ```

3. **After clock fix**, recalculate all schedules:
   ```bash
   # Restart backend to recalculate
   docker-compose restart spooled-backend
   ```

### Scenario C: Timezone Issues

**Cause**: Schedule timezone doesn't match expectations

#### Actions:

1. **Check schedule timezone**:
   ```sql
   SELECT id, name, timezone, cron_expression, next_run_at
   FROM schedules
   WHERE id = 'sch_XXX';
   ```

2. **Update timezone if wrong**:
   ```sql
   UPDATE schedules
   SET timezone = 'America/New_York'  -- or correct timezone
   WHERE id = 'sch_XXX';
   ```

3. **List valid timezones**:
   ```sql
   SELECT name FROM pg_timezone_names ORDER BY name;
   ```

### Scenario D: Duplicate Scheduled Jobs

**Cause**: Multiple backend instances each creating scheduled jobs

#### Actions:

1. **Check for duplicates**:
   ```sql
   SELECT 
     schedule_id,
     DATE_TRUNC('minute', started_at) as minute,
     COUNT(*) as count
   FROM schedule_runs
   WHERE started_at > NOW() - INTERVAL '1 hour'
   GROUP BY schedule_id, DATE_TRUNC('minute', started_at)
   HAVING COUNT(*) > 1;
   ```

2. **Note on concurrency**:
   - Schedule processing uses `FOR UPDATE SKIP LOCKED` at the database level, which should prevent double-processing across multiple schedulers.
   - Duplicates typically indicate `next_run_at` being recalculated incorrectly, or schedule processor being invoked concurrently with overlapping logic.

### Scenario E: Schedule Processor Not Running

**Cause**: Background job for schedule processing crashed

#### Actions:

1. **Check for schedule processor logs**:
   ```bash
   docker logs spooled-backend --tail 500 | grep -i "schedule processor"
   ```

2. **Restart the backend**:
   ```bash
   docker-compose restart spooled-backend
   ```

3. **Verify it's running**:
   ```bash
   # Should see periodic log entries
   docker logs spooled-backend -f | grep -i schedule
   ```

---

## Manual Recovery

### Trigger Missed Scheduled Job

```sql
-- Create a job from a schedule (mirrors `process_due_schedules()` logic)
WITH s AS (
  SELECT * FROM schedules WHERE id = 'sch_XXX'
),
new_job AS (
  INSERT INTO jobs (
    id,
    organization_id,
    queue_name,
    status,
    payload,
    priority,
    max_retries,
    timeout_seconds,
    tags,
    created_at,
    updated_at
  )
  SELECT
    gen_random_uuid()::TEXT,
    organization_id,
    queue_name,
    'pending',
    payload_template,
    priority,
    max_retries,
    timeout_seconds,
    tags,
    NOW(),
    NOW()
  FROM s
  RETURNING id
)
INSERT INTO schedule_runs (id, schedule_id, job_id, status, started_at, completed_at)
SELECT gen_random_uuid()::TEXT, 'sch_XXX', id, 'completed', NOW(), NOW()
FROM new_job;
```

### Pause Problematic Schedule

```sql
UPDATE schedules
SET is_active = false
WHERE id = 'sch_XXX';
```

### Resume Schedule

```sql
UPDATE schedules
SET 
  is_active = true,
  next_run_at = NOW() + INTERVAL '1 minute'  -- Start soon (app should later compute proper next run)
WHERE id = 'sch_XXX';
```

---

## Prevention

### Configuration Best Practices

1. **Use explicit timezones**:
   ```sql
   -- Always specify timezone when creating schedules
   INSERT INTO schedules (timezone) VALUES ('UTC');
   ```

2. **Monitor schedule lag**:
   If you do not export schedule metrics, monitor via a periodic DB check:

   ```sql
   -- Count schedules that are active but overdue
   SELECT COUNT(*) AS overdue_schedules
   FROM schedules
   WHERE is_active = true
     AND next_run_at IS NOT NULL
     AND next_run_at < NOW() - INTERVAL '5 minutes';
   ```

   If you *do* export a schedule lag metric, alert when lag exceeds 5 minutes for > 5m.

3. **Set up NTP sync**:
   ```bash
   # Ensure NTP is always enabled
   timedatectl set-ntp true
   ```

### Monitoring

Add to your monitoring checklist:
- [ ] Alert when any schedule's next_run_at is > 5 min in past
- [ ] Dashboard panel showing schedule execution times
- [ ] Track schedule drift histogram
- [ ] Monitor for duplicate scheduled jobs

### Schedule Health Query

Run periodically to catch issues early:

```sql
SELECT 
  CASE 
    WHEN next_run_at < NOW() - INTERVAL '1 hour' THEN 'critical_drift'
    WHEN next_run_at < NOW() - INTERVAL '5 minutes' THEN 'drift'
    WHEN last_run_at IS NULL AND created_at < NOW() - INTERVAL '1 day' THEN 'never_run'
    ELSE 'healthy'
  END as health,
  COUNT(*) as count
FROM schedules
WHERE is_active = true
GROUP BY health;
```

---

## Cron Expression Reference

Common cron expressions and their meanings:

| Expression | Meaning |
|------------|---------|
| `0 * * * *` | Every hour at minute 0 |
| `*/15 * * * *` | Every 15 minutes |
| `0 0 * * *` | Daily at midnight |
| `0 9 * * 1-5` | Weekdays at 9 AM |
| `0 0 1 * *` | First day of month at midnight |

**Note**: All times are interpreted in the schedule's configured timezone.

---

## Related Documentation

- [Schedules Guide](../../../spooled-frontend/src/pages/docs/jobs.astro) (scheduled jobs section)
- [Operations Guide](../operations.md)
