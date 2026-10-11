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
- `429 RATE_LIMIT_EXCEEDED` with `Retry-After` from `POST /auth/email/start` or `POST /auth/email/verify`, or `400 CAPTCHA_FAILED` from `/auth/email/start` (see [Scenario G](#scenario-g-email-login-answers-429-or-captcha_failed))
- After upgrading to 0.1.120, unrelated users throttled together because every request seems to come from one address (see [Scenario H](#scenario-h-every-caller-shares-one-rate-limit-bucket))
- `503 SERVICE_UNAVAILABLE` "Authentication is temporarily unavailable" with `Retry-After: 5` (gRPC `UNAVAILABLE`, `/auth/login` `service_unavailable`): the credential could not be checked. Either the database could not be reached (see the [Postgres saturation runbook](./postgres-saturation.md)), or a legacy key (created before 0.1.90, no `lookup_hash`) waited for the legacy-key scan longer than about one scan, at most 10 s, while other unknown keys on the same transport held it (see [Scenario F](#scenario-f-503--unavailable-for-a-legacy-key)). It is not a bad key; a retry authenticates it. Any other database failure during authentication is `500` "Authentication failed"; the database error text is only in the server log.

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
-- Note: last_used is coarse — written at most once per key per 5 minutes, not per request.
--       A key in active use can show a timestamp several minutes old, so never use it to
--       decide whether a specific request arrived.
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

2. **If legitimate traffic**, note that the failed-auth budgets are fixed in the code (20 per
   minute per client address on REST and `/auth/login`, 60 per minute shared by gRPC) and only
   skip the legacy-key scan. A caller that is rate limited by the plan (`429`) is handled by
   the plan limits, not by an auth setting. To release one address early, delete its counter
   (the address is the rate-limit id: IPv4 as is, IPv6 as its /64, e.g. `2001:db8:1:2::/64`):
   ```bash
   redis-cli DEL "auth_fail:203.0.113.9"
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

### Scenario F: 503 / UNAVAILABLE for a legacy key

**Cause**: the key has no `lookup_hash` yet (created before 0.1.90 and not used since), so
it is found by a bcrypt scan of all such keys, about 250 ms per key. REST (with
`/auth/login`) and gRPC each run one scan at a time; concurrent requests for the same key
share one scan. A request waits for its transport's turn about as long as one scan takes
(plus 1 s, at most 10 s) and is then answered 503 / `UNAVAILABLE`. That happens when unknown
keys on the same transport keep the turn busy; unknown gRPC keys never delay REST and the
other way round.

#### Actions:

1. **Retry.** SDKs retry 503 and `UNAVAILABLE`. The first successful authentication stores
   the key's `lookup_hash`; from then on it never scans again.

2. **Count the legacy keys** (each one adds about 250 ms to every scan; around 40 the 10 s
   wait cap is reached, around 100 a scan approaches the 30 s request timeout):
   ```sql
   SELECT id, organization_id, name, key_prefix, last_used
   FROM api_keys
   WHERE is_active = TRUE AND lookup_hash IS NULL
   ORDER BY last_used DESC NULLS LAST;
   ```
   Do not deactivate or rotate them to fix this: legacy keys stay supported (operator
   decision).

3. **Look for a flood of unknown keys** on the affected transport: the `auth_fail:*`
   counters and `auth_fail_grpc` in Redis, 401 rates in the access logs. Block the source at
   the load balancer if it is an attack.

### Scenario G: Email login answers 429 or CAPTCHA_FAILED

**Cause**: the email-login throttles (0.1.120), or Cloudflare Turnstile.

All counters are fixed windows in Redis. `{ip}` is the client's rate-limit id (an IPv4 address,
or an IPv6 /64 written `2001:db8:1:2::/64`):

| Redis key | Limit | Meaning |
|-----------|-------|---------|
| `email_start:minute:{ip}` | 3 per 60 s | login codes requested by one client |
| `email_start:hour:{ip}` | 10 per 3600 s | same, per hour |
| `email_verify_attempts:{email}:{ip}` | 5 per 10 min | code guesses by one client for one address |
| `email_verify_ceiling:{email}` | 20 per 10 min | code guesses for one address from all clients |

Over a limit the answer is `429 RATE_LIMIT_EXCEEDED` with a `Retry-After` header and
`details.retry_after_seconds`. Separately, an address gets at most 5 login codes per hour
(a `429` without `Retry-After`). The counters only make callers wait: a stranger's wrong
guesses never invalidate the owner's code (a code tolerates 40 attempts while the Redis
budgets are enforced), and a code the mail provider failed to send is not counted.

#### Actions:

1. **Wait for `Retry-After`.** Every window ends by itself.

2. **Release a legitimate user early** (for example many users behind one NAT address):
   ```bash
   redis-cli DEL "email_start:minute:203.0.113.9" "email_start:hour:203.0.113.9"
   redis-cli DEL "email_verify_attempts:user@example.com:203.0.113.9" "email_verify_ceiling:user@example.com"
   ```

3. **Look for a guesser** if the per-address ceiling keeps tripping: the log line
   `Login verify attempt budget exhausted` names the masked address and the client.
   A guesser who rotates through many addresses (for example the /64s of an IPv6 /56) can keep
   the ceiling spent, so the owner has to wait out each window; no code is invalidated.

4. **`400 CAPTCHA_FAILED`** comes only from `POST /auth/email/start` and only while
   `TURNSTILE_SECRET_KEY` is set. If the website or the dashboard does not send a
   `turnstile_token` yet, unset the variable and redeploy. If they do, check the log for
   `Turnstile siteverify` errors (an unreachable or erroring Turnstile counts as failed) and that
   the widget's site key belongs to the configured secret.

5. **Without Redis** the start throttle fails open (with `RATE_LIMIT_FAIL_CLOSED` it answers
   429 instead), and the verify budgets are skipped, so each code allows 5 attempts and a
   stranger can spend them, as before 0.1.120.

### Scenario H: Every caller shares one rate-limit bucket

**Cause**: client addresses come from the TCP peer. `CF-Connecting-IP` and `X-Real-IP` are
believed only from a loopback, private, link-local or unique-local peer unless
`TRUSTED_CLIENT_IP_HEADER` names the header. A proxy that reaches the backend from a **public**
address, with `TRUSTED_CLIENT_IP_HEADER` unset, puts every caller in the proxy's one bucket.
Production's cloudflared connects over the docker network and is not affected.

#### Actions:

1. **Confirm**: production logs `TRUSTED_CLIENT_IP_HEADER is not set` at boot, and the
   `auth_fail:*` / `email_start:*` keys in Redis show the proxy's address for everybody.

2. **Set the header** your proxy writes and redeploy: `TRUSTED_CLIENT_IP_HEADER=CF-Connecting-IP`
   behind Cloudflare, `X-Real-IP` behind nginx. It is then believed from any peer, so the backend
   port must be reachable only through that proxy.

---

## API Key Cache Issues

The backend caches API key lookups in Redis, shared by REST, `/auth/login` and gRPC:

| Redis key | Meaning | TTL |
|-----------|---------|-----|
| `api_key:{sha256(token)}` | Verified key record (bcrypt skipped on a hit) | 60 s |
| `auth_miss:{sha256(token)}` | The token matched no key (refused without touching the database) | 60 s |
| `auth_fail:{client_ip}` | Failed-auth count (20 per minute budget) per client address (resolved from the TCP peer and the trusted edge header, an IPv6 caller as its /64), charged before the legacy bcrypt scan of keys without a `lookup_hash` and taken back when the scan finds the key or never runs; over it, only that scan is skipped (401) | 60 s |
| `auth_fail_grpc` | Failed-auth count shared by every gRPC caller (60 per minute), charged the same way; over it, gRPC skips the same legacy scan | 60 s |

If a key appears invalid right after creation or reactivation (a token that was refused is remembered for up to 60 s):

### Clear API Key Cache

```bash
# If using Redis cache
redis-cli KEYS "api_key:*" | xargs redis-cli DEL
redis-cli KEYS "auth_miss:*" | xargs redis-cli DEL

# Or wait up to 60 seconds for the entries to expire
```

The TTLs are fixed in the code (no environment variable). Without Redis every request is a cache miss and pays one bcrypt verification (about 250 ms of CPU, run on a bounded blocking pool of half the cores, 1 to 4), so throughput per process is limited to roughly that many verifications per 0.25 s.

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
