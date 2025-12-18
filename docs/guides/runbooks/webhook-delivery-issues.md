# Runbook: Webhook Delivery Issues

**Severity**: Warning  
**Related Tables**: `outgoing_webhooks`, `outgoing_webhook_deliveries`  
**Related APIs**: `GET /api/v1/outgoing-webhooks/{id}/deliveries`, `POST /api/v1/outgoing-webhooks/{id}/retry/{delivery_id}`

---

## Symptoms

- Webhook deliveries failing consistently
- Jobs completing but webhooks not received by customer endpoints
- Increasing retry attempts for webhook deliveries
- Customer complaints about missing notifications
- Logs showing `webhook delivery failed: connection refused/timeout`

---

## Diagnosis

### 1. Identify affected outgoing webhooks

List webhook configs and recent health indicators:

```sql
SELECT
  id,
  organization_id,
  name,
  url,
  enabled,
  failure_count,
  last_status,
  last_triggered_at,
  created_at,
  updated_at
FROM outgoing_webhooks
ORDER BY failure_count DESC, last_triggered_at DESC NULLS LAST
LIMIT 50;
```

### 2. Check recent delivery failures

```sql
SELECT
  d.id AS delivery_id,
  d.webhook_id,
  w.name AS webhook_name,
  w.url AS webhook_url,
  d.event,
  d.status,
  d.status_code,
  d.error,
  d.attempts,
  d.created_at,
  d.delivered_at
FROM outgoing_webhook_deliveries d
JOIN outgoing_webhooks w ON w.id = d.webhook_id
WHERE d.status = 'failed'
  AND d.created_at > NOW() - INTERVAL '1 hour'
ORDER BY created_at DESC
LIMIT 50;
```

### 3. Identify failure patterns (status codes + error messages)

```sql
SELECT
  COALESCE(status_code::TEXT, 'no_status_code') AS status_code,
  COALESCE(NULLIF(error, ''), 'no_error') AS error,
  COUNT(*) AS count
FROM outgoing_webhook_deliveries
WHERE status = 'failed'
  AND created_at > NOW() - INTERVAL '1 hour'
GROUP BY 1, 2
ORDER BY count DESC
LIMIT 50;
```

### 4. Check specific endpoint health (from the backend environment)

```bash
# Test webhook endpoint manually
curl -v -X POST "https://customer-endpoint.com/webhook" \
  -H "Content-Type: application/json" \
  -d '{"test": true}'
```

### 5. Check Outbound Network

```bash
# From backend container
docker exec spooled-backend curl -v https://httpbin.org/post

# Check DNS resolution
docker exec spooled-backend nslookup customer-endpoint.com
```

---

## Resolution

### Scenario A: Timeout Errors

**Cause**: Customer endpoint is slow or unresponsive

#### Actions:

1. **Check if endpoint is actually down**:
   ```bash
   curl -v -o /dev/null -s "https://customer-endpoint.com/health"
   ```

2. **Increase timeout temporarily** (if endpoint is slow but responsive):
   ```bash
   # Environment variable (requires restart)
   WEBHOOK_TIMEOUT_SECONDS=30  # Default is 10
   ```

3. **Contact customer**: Their endpoint may be overloaded

### Scenario B: Connection Refused

**Cause**: Endpoint not listening, firewall blocking, or DNS issues

#### Actions:

1. **Verify endpoint URL is correct**:
   ```sql
   SELECT id, name, url, enabled FROM outgoing_webhooks ORDER BY created_at DESC LIMIT 50;
   ```

2. **Check if endpoint requires IP allowlisting**: Provide our outbound IPs

3. **Test from different network**: Rule out local network issues

### Scenario C: Rate Limited (429 errors)

**Cause**: Customer endpoint is rate limiting our requests

#### Actions:

1. **Implement backoff**:
   The system automatically implements exponential backoff, but you can adjust:
   ```bash
   WEBHOOK_RETRY_DELAY_SECONDS=60  # Initial delay
   WEBHOOK_MAX_RETRIES=5
   ```

2. **Batch webhooks**: Contact customer to discuss webhook batching

3. **Temporarily disable a noisy webhook** (if needed):
   ```sql
   UPDATE outgoing_webhooks
   SET enabled = FALSE
   WHERE id = 'whk_XXX';
   ```

### Scenario D: SSL/TLS Errors

**Cause**: Certificate issues on customer endpoint

#### Actions:

1. **Check certificate**:
   ```bash
   openssl s_client -connect customer-endpoint.com:443 -servername customer-endpoint.com
   ```

2. **Common issues**:
   - Expired certificate
   - Self-signed certificate (not supported by default)
   - Certificate chain incomplete

3. **If self-signed is required** (not recommended):
   ```bash
   WEBHOOK_ALLOW_INSECURE=true  # Security risk!
   ```

### Scenario E: Authentication Failures (401/403)

**Cause**: Customer endpoint rejects requests (auth header expectations, signature validation, etc.)

#### Actions:

1. **Verify webhook secret is correct**:
   ```sql
   SELECT id, name, url, secret, enabled
   FROM outgoing_webhooks
   WHERE id = 'whk_XXX';
   ```

2. **Test signature generation**:
   ```bash
   # Generate test signature
   echo -n '{"test":true}' | openssl dgst -sha256 -hmac "webhook_secret"
   ```

3. **Update webhook token if needed**:
   ```sql
   UPDATE outgoing_webhooks
   SET secret = 'new_secret', updated_at = NOW()
   WHERE id = 'whk_XXX';
   ```

---

## Manual Retry

### Retry a failed delivery (recommended: via API)

```bash
# POST /api/v1/outgoing-webhooks/{id}/retry/{delivery_id}
curl -X POST \
  -H "Authorization: Bearer YOUR_API_KEY" \
  "https://api.spooled.cloud/api/v1/outgoing-webhooks/whk_XXX/retry/dlv_XXX"
```

### Retry failed deliveries (direct SQL; use with care)

```sql
UPDATE outgoing_webhook_deliveries
SET
  status = 'pending',
  attempts = attempts + 1,
  error = NULL,
  status_code = NULL,
  response_body = NULL,
  delivered_at = NULL
WHERE status = 'failed'
  AND created_at > NOW() - INTERVAL '1 hour';
```

### Disable a failing webhook (stop deliveries)

```sql
UPDATE outgoing_webhooks
SET enabled = FALSE, updated_at = NOW()
WHERE id = 'whk_XXX';
```

---

## Prevention

### Configuration Best Practices

1. **Set reasonable timeouts**:
   ```bash
   WEBHOOK_TIMEOUT_SECONDS=10
   WEBHOOK_CONNECT_TIMEOUT_SECONDS=5
   ```

2. **Configure retry policy**:
   ```bash
   WEBHOOK_MAX_RETRIES=5
   WEBHOOK_RETRY_BACKOFF_MULTIPLIER=2
   ```

3. **Require HTTPS in production**:
   ```bash
   WEBHOOK_REQUIRE_HTTPS=true
   ```

### Monitoring

Add to your monitoring checklist:
- [ ] Alert on webhook failure rate > 10%
- [ ] Alert on specific endpoint consistently failing
- [ ] Dashboard panel for webhook delivery success rate
- [ ] Track average webhook delivery latency

### Customer Communication

Template for webhook issues:

```
Subject: Webhook Delivery Issue Detected

We've detected that webhooks to your endpoint [URL] are failing.

Error: [error_message]
Failure rate: [X]% over the last hour

Please verify:
1. Your endpoint is accessible from the internet
2. Your endpoint responds within 10 seconds
3. Your SSL certificate is valid

Failed webhooks will be retried [X] more times with exponential backoff.

If you need assistance, contact support@spooled.cloud
```

---

## Related Documentation

- [Webhooks Guide](../webhooks.md)
- [Operations Guide](../operations.md)

