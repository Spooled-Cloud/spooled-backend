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
- A webhook that has gone completely silent: 20 consecutive failed deliveries auto-disable
  it (`enabled = FALSE`, `last_status = 'auto_disabled'`)

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

How to read those three columns:

- `enabled` — may already be `FALSE` without anyone touching it. The platform disables a
  webhook automatically after 20 consecutive failed deliveries.
- `last_status` — one of `success`, `failed` or `auto_disabled`. `auto_disabled` means the
  breaker tripped and the webhook is receiving no events at all.
- `failure_count` — consecutive failed **deliveries**, incremented once per delivery rather
  than once per retry attempt, and reset to 0 by any successful delivery (including a
  successful manual retry). The same outage therefore produces a number roughly 5x smaller
  than it used to: `failure_count = 20` is the auto-disable threshold, not a mild warning,
  and any older `failure_count > N` alert threshold needs rescaling.

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

Widen the interval and you will eventually hit the retention floor: delivery rows are
deleted by the per-organization retention sweep at the plan's history retention window
(free 1 day, starter 7, pro 30, enterprise 90). An empty result for a free-tier
organization's "yesterday" is expected, not evidence that nothing was sent.

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

2. **Check the budget you are working with**: the delivery client allows 5s to connect and
   10s for the whole request. Both are compiled in — there is no environment variable to
   raise them, so a customer endpoint that needs longer than 10s will keep timing out and
   must be fixed on their side (respond 2xx immediately, process asynchronously).

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

1. **Adjust the attempt budget**:
   Deliveries already retry with exponential backoff (immediate, 1s, 2s, 4s, 8s). The
   number of attempts per delivery is the only retry knob:
   ```bash
   # Attempts per delivery before it is given up on (requires restart)
   OUTGOING_WEBHOOK_MAX_ATTEMPTS=5
   ```
   Lowering it makes Spooled give up on a rate-limiting endpoint sooner. Note that fewer
   attempts do not mean fewer *deliveries* recorded as failed, so it does not slow down
   the march toward auto-disable.

2. **Reduce pressure on the endpoint**:
   ```bash
   # Hard ceiling on deliveries in flight process-wide (requires restart)
   OUTGOING_WEBHOOK_MAX_CONCURRENT_DELIVERIES=64
   ```
   Lowering this queues deliveries instead of firing them concurrently, which spreads the
   load a rate-limited endpoint sees. It is a process-wide ceiling, so it affects every
   organization on the deployment — do not tune it for one noisy customer.

3. **Batch webhooks**: Contact customer to discuss webhook batching

4. **Temporarily disable a noisy webhook** (if needed):
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
   - Self-signed certificate
   - Certificate chain incomplete

3. **There is no insecure-TLS escape hatch**: certificate validation cannot be turned off
   for webhook delivery. A customer on a self-signed or incomplete chain has to install a
   publicly trusted certificate, or terminate TLS at a proxy that has one.

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

3. **Update the webhook secret if needed** (via API, on the customer's key):
   ```bash
   curl -X PUT \
     -H "Authorization: Bearer CUSTOMER_API_KEY" \
     -H "Content-Type: application/json" \
     -d '{"secret": "new_secret"}' \
     "https://api.spooled.cloud/api/v1/outgoing-webhooks/whk_XXX"
   ```
   Omitted fields are left alone. Sending `"secret": null` **clears** the secret and every
   later delivery goes out unsigned with no `X-Spooled-Signature` header — only do that
   when the receiver has genuinely stopped verifying signatures.

---

## Manual Retry

### Retry a failed delivery (via API — the only way to actually resend)

```bash
# POST /api/v1/outgoing-webhooks/{id}/retry/{delivery_id}
curl -X POST \
  -H "Authorization: Bearer YOUR_API_KEY" \
  "https://api.spooled.cloud/api/v1/outgoing-webhooks/whk_XXX/retry/dlv_XXX"
```

The endpoint sends immediately and records the new result. If it succeeds, the webhook's
`failure_count` resets to 0, which also walks it back from the auto-disable threshold.

> **Do not "retry" by rewriting delivery rows in SQL.** Nothing sweeps
> `outgoing_webhook_deliveries` looking for `status = 'pending'` — deliveries are sent
> in-process — so flipping rows back to pending sends nothing and destroys the `error`,
> `status_code` and `response_body` evidence you need for diagnosis.

### Pick the deliveries to retry

```sql
SELECT d.id AS delivery_id, d.webhook_id, d.event, d.status_code, d.error, d.attempts
FROM outgoing_webhook_deliveries d
WHERE d.status = 'failed'
  AND d.attempts < 10
  AND d.created_at > NOW() - INTERVAL '1 hour'
ORDER BY d.created_at DESC
LIMIT 50;
```

Feed those ids to the retry endpoint. Deliveries at 10 total attempts cannot be retried,
and rows already removed by the plan retention sweep are gone for good.

---

## Disabling and Re-enabling

### Automatic disable

The platform stops a hopeless webhook on its own: after **20 consecutive failed
deliveries** it sets `enabled = FALSE` and `last_status = 'auto_disabled'`, and the webhook
receives no further events. In most incidents you do not need to disable anything by hand
— check whether the breaker has already tripped:

```sql
SELECT id, name, url, enabled, failure_count, last_status, last_triggered_at
FROM outgoing_webhooks
WHERE last_status = 'auto_disabled'
ORDER BY last_triggered_at DESC NULLS LAST
LIMIT 50;
```

### Disable a failing webhook (stop deliveries)

```sql
UPDATE outgoing_webhooks
SET enabled = FALSE, updated_at = NOW()
WHERE id = 'whk_XXX';
```

### Re-enable a webhook — use the API, not SQL

```bash
curl -X PUT \
  -H "Authorization: Bearer CUSTOMER_API_KEY" \
  -H "Content-Type: application/json" \
  -d '{"enabled": true}' \
  "https://api.spooled.cloud/api/v1/outgoing-webhooks/whk_XXX"
```

> **Do not re-enable with `UPDATE outgoing_webhooks SET enabled = TRUE`.** Turning a
> webhook back on is charged against the organization's plan webhook cap, and that check
> only runs on the API path. The customer may have created replacement webhooks while this
> one was disabled, so a direct SQL update silently puts them over their plan limit; the
> API returns `429 QUOTA_EXCEEDED` instead, which is the answer you want. Fix the endpoint
> first — re-enabling into a still-broken endpoint just burns another 20 deliveries.

---

## Prevention

### Configuration Best Practices

1. **Know the fixed timeouts**: 5s to connect, 10s per request. They are compiled in, so
   tell customers their endpoint must acknowledge within 10 seconds and do the real work
   asynchronously.

2. **Configure the retry policy** — the two knobs that exist:
   ```bash
   # Attempts per delivery, with exponential backoff between them
   OUTGOING_WEBHOOK_MAX_ATTEMPTS=5

   # Hard ceiling on deliveries in flight process-wide; over this, deliveries queue
   OUTGOING_WEBHOOK_MAX_CONCURRENT_DELIVERIES=64
   ```
   Leave the concurrency ceiling alone unless you have the memory and socket headroom for
   a higher one: every in-flight delivery holds a copy of the payload for up to the request
   timeout plus backoff.

3. **Run production as production**: HTTPS-only webhook targets and the SSRF blocklist are
   enforced when `RUST_ENV=production`. There is no per-webhook override.

### Monitoring

Add to your monitoring checklist:
- [ ] Alert on webhook failure rate > 10%
- [ ] Alert on specific endpoint consistently failing
- [ ] Alert on `last_status = 'auto_disabled'` — that webhook is now dark until someone
      re-enables it through the API
- [ ] Alert on `failure_count` approaching 20 (consecutive failed deliveries), which is
      the auto-disable threshold
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

Each event is retried with exponential backoff. If 20 deliveries fail in a row, the
webhook is disabled automatically and stops receiving events until it is re-enabled.

If you need assistance, contact support@spooled.cloud
```

---

## Related Documentation

- [Webhooks Guide](../webhooks.md)
- [Operations Guide](../operations.md)
