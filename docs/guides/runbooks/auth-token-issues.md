# Runbook: Authentication and Token Issues

**Severity**: Warning to Critical (depending on scope)  
**Related Errors**: `401 Unauthorized`, `403 Forbidden`, `invalid_token`

---

## Symptoms

- Users/workers receiving `401 Unauthorized` errors
- API requests failing with `invalid API key` or `token expired`
- Dashboard showing "Session expired" frequently
- Workers unable to authenticate
- Sudden spike in authentication failures

---

## Diagnosis

### 1. Check Authentication Metrics

```bash
curl -s http://localhost:9090/metrics | grep -E "auth|token"
```

### 2. Identify Error Patterns

Check application logs:

```bash
# Recent auth failures
docker logs spooled-backend --tail 500 | grep -i "auth\|unauthorized\|forbidden\|token"

# Count by error type
docker logs spooled-backend --tail 5000 | grep -i "auth" | sort | uniq -c | sort -rn
```

### 3. Test API Key Validation

```bash
# Test a known good API key
curl -H "Authorization: Bearer sp_live_XXXXX" \
  https://api.spooled.cloud/api/v1/health
```

### 4. Check JWT Configuration

```bash
# Verify JWT secret is set
echo $JWT_SECRET | wc -c  # Should be > 32

# Check JWT expiration setting
echo $JWT_EXPIRATION_HOURS  # Default: 24
```

### 5. Database API Key Check

```sql
-- List recent API keys for an organization (recommended)
-- Note: API keys are stored as bcrypt hashes; you generally cannot search by the raw key value.
SELECT
  id,
  organization_id,
  name,
  key_prefix,
  is_active,
  created_at,
  last_used,
  expires_at
FROM api_keys
ORDER BY created_at DESC
LIMIT 50;

-- Find inactive/expired keys
SELECT
  id,
  name,
  key_prefix,
  is_active,
  last_used,
  expires_at
FROM api_keys
WHERE is_active = FALSE
   OR (expires_at IS NOT NULL AND expires_at < NOW())
ORDER BY created_at DESC
LIMIT 50;
```

---

## Resolution

### Scenario A: Invalid API Key Errors

**Cause**: Key doesn't exist, is inactive/expired, or malformed

#### Actions:

1. **Verify key format**:
   - Live keys: `sp_live_...` (32+ characters)
   - Test keys: `sp_test_...` (32+ characters)

2. **Check whether the key is active / expired** (by key `id` or `name` from the dashboard):
   ```sql
   SELECT id, name, key_prefix, is_active, last_used, expires_at
   FROM api_keys
   WHERE id = 'key_XXX';
   ```

3. **Generate new key** (via Dashboard or API):
   ```bash
   # Via API (requires admin token)
   curl -X POST https://api.spooled.cloud/api/v1/organizations/ORG_ID/api-keys \
     -H "Authorization: Bearer ADMIN_TOKEN" \
     -H "Content-Type: application/json" \
     -d '{"name": "New Key"}'
   ```

### Scenario B: JWT Token Expired

**Cause**: Dashboard session or worker token expired

#### Actions:

1. **For Dashboard users**: Clear cookies and re-login

2. **For workers**: Refresh the token:
   ```bash
   # Workers should handle token refresh automatically
   # If not, restart the worker to get fresh token
   ```

3. **If widespread JWT issues**, check system clock:
   ```bash
   # Check server time
   date
   timedatectl
   
   # If clock is wrong, sync it
   sudo ntpdate -s time.nist.gov
   ```

### Scenario C: JWT Secret Mismatch

**Cause**: JWT_SECRET changed or different across instances

#### Actions:

1. **Verify all instances use same secret**:
   ```bash
   # Check each instance
   kubectl exec -it spooled-backend-xxx -- printenv JWT_SECRET | sha256sum
   ```

2. **If secret was changed**, all existing tokens are invalid:
   - Users must re-login
   - Workers must re-register

3. **Rotate secret safely**:
   ```bash
   # 1. Set new secret alongside old (if supported)
   # 2. Deploy with new secret
   # 3. All sessions will expire (users re-login)
   ```

### Scenario D: Rate Limited (Too Many Auth Failures)

**Cause**: Security rate limiting kicked in

#### Actions:

1. **Check rate limit status**:
   ```bash
   curl -s http://localhost:9090/metrics | grep rate_limit
   ```

2. **If legitimate traffic**, temporarily increase limits:
   ```bash
   RATE_LIMIT_AUTH_PER_MINUTE=100  # Default: 20
   ```

3. **If attack**, block the source IP at load balancer level

### Scenario E: Clock Skew Issues

**Cause**: Server clock out of sync affects JWT validation

#### Actions:

1. **Check clock**:
   ```bash
   date
   timedatectl status
   ```

2. **Sync clock**:
   ```bash
   # Using systemd-timesyncd
   sudo systemctl restart systemd-timesyncd
   
   # Using ntpd
   sudo ntpdate -s pool.ntp.org
   ```

3. **Verify NTP is enabled**:
   ```bash
   timedatectl set-ntp true
   ```

---

## API Key Cache Issues

The backend caches API key lookups. If keys appear invalid immediately after creation:

### Clear API Key Cache

```bash
# If using Redis cache
redis-cli KEYS "api_key:*" | xargs redis-cli DEL

# Or wait for cache expiration (typically 5 minutes)
```

### Verify Cache Settings

```bash
# Check cache TTL
echo $API_KEY_CACHE_TTL_SECONDS  # Default: 300 (5 min)
```

---

## Worker Authentication Issues

### Check Worker Registration

```sql
-- Find worker by ID
SELECT * FROM workers WHERE id = 'wrk_XXX';

-- Check worker's organization
SELECT w.*, o.name as org_name 
FROM workers w 
JOIN organizations o ON w.organization_id = o.id
WHERE w.id = 'wrk_XXX';
```

### Re-register Worker

If worker can't authenticate:

1. Stop the worker
2. Delete worker registration (if exists):
   ```sql
   DELETE FROM workers WHERE id = 'wrk_XXX';
   ```
3. Start worker with valid API key

---

## Prevention

### Configuration Best Practices

1. **Use strong JWT secret** (minimum 32 characters):
   ```bash
   JWT_SECRET=$(openssl rand -hex 32)
   ```

2. **Set appropriate token expiration**:
   ```bash
   JWT_EXPIRATION_HOURS=24  # Shorter for higher security
   ```

3. **Enable API key rotation reminders**:
   - Track `last_rotated_at` for API keys
   - Alert when keys are > 90 days old

### Monitoring

Add to your monitoring checklist:
- [ ] Alert on authentication failure rate > 5%
- [ ] Alert on unusual geographic access patterns
- [ ] Dashboard panel for auth success/failure ratio
- [ ] Track API key age distribution

### Security Audit

Regularly review:

```sql
-- API keys not used in 90 days
SELECT * FROM api_keys 
WHERE is_active = TRUE
  AND (last_used IS NULL OR last_used < NOW() - INTERVAL '90 days');

-- Organizations with many API keys
SELECT organization_id, COUNT(*) as key_count
FROM api_keys
WHERE is_active = TRUE
GROUP BY organization_id
HAVING COUNT(*) > 10;
```

---

## Related Documentation

- [Authentication and API Keys](../api-usage.md#authentication)
- [API Usage Guide](../api-usage.md)
- [Operations Guide - JWT Security](../operations.md#jwt-security)
