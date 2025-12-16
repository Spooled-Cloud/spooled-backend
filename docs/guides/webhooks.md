# Webhooks Guide

Complete guide to receiving and sending webhooks with Spooled Cloud.

## Table of Contents

1. [Overview](#overview)
2. [Incoming Webhooks](#incoming-webhooks)
3. [Custom Webhooks](#custom-webhooks)
4. [Signature Verification](#signature-verification)
5. [Outgoing Webhooks](#outgoing-webhooks)
6. [Best Practices](#best-practices)

---

## Overview

Spooled supports webhooks in two directions:

1. **Incoming webhooks** — Receive events from any external service and automatically create jobs
2. **Outgoing webhooks** — Send notifications when jobs complete, fail, or hit DLQ

---

## Incoming Webhooks

### How It Works

```
┌──────────┐     ┌─────────────┐     ┌─────────┐     ┌──────────┐
│ External │────▶│   Spooled   │────▶│ Verify  │────▶│  Create  │
│  Source  │     │  Endpoint   │     │Signature│     │   Job    │
└──────────┘     └─────────────┘     └─────────┘     └──────────┘
```

### Supported Incoming Webhooks

Spooled currently supports **one** incoming webhook endpoint per organization:

| Type | Endpoint | Authentication |
|------|----------|----------------|
| Custom | `/api/v1/webhooks/{org_id}/custom` | `X-Webhook-Token` (optional, recommended) |

If a webhook token is configured for the organization, the request must include the
`X-Webhook-Token` header. If no token is configured, the endpoint is open (not
recommended for production).

---

## Custom Webhooks

For any external service, use the custom webhook endpoint.

### Get Your Webhook URL

```bash
curl https://api.spooled.cloud/api/v1/organizations/webhook-token \
  -H "Authorization: Bearer sk_live_YOUR_API_KEY"
```

Response:

```json
{
  "webhook_url": "https://api.spooled.cloud/api/v1/webhooks/org_abc123/custom",
  "webhook_token": "whk_xxxxxxxxxxxx"
}
```

### Send Webhooks to Spooled

```bash
curl -X POST https://api.spooled.cloud/api/v1/webhooks/org_abc123/custom \
  -H "X-Webhook-Token: whk_xxxxxxxxxxxx" \
  -H "Content-Type: application/json" \
  -d '{
    "queue_name": "my-events",
    "event_type": "order.created",
    "payload": {
      "order_id": 12345,
      "customer": "John Doe",
      "total": 99.99
    },
    "priority": 0,
    "idempotency_key": "order-12345"
  }'
```

Response:

```json
{
  "job_id": "job_550e8400e29b41d4",
  "queue_name": "my-events",
  "status": "pending"
}
```

### Request Fields

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `queue_name` | string | Yes | Target queue (1-255 chars) |
| `payload` | object | Yes | Job data (max 5MB) |
| `event_type` | string | No | Event type for logging |
| `priority` | integer | No | -100 to 100 (default: 0) |
| `idempotency_key` | string | No | Prevent duplicates |

### Managing Webhook Token

```bash
# Regenerate token (invalidates old one)
curl -X POST https://api.spooled.cloud/api/v1/organizations/webhook-token/regenerate \
  -H "Authorization: Bearer sk_live_YOUR_API_KEY"

# Clear token (NOT recommended for production)
curl -X POST https://api.spooled.cloud/api/v1/organizations/webhook-token/clear \
  -H "Authorization: Bearer sk_live_YOUR_API_KEY" \
  -d '{"confirm": true}'
```

---

## Signature Verification

### Custom Webhook Token

For custom webhooks, we use a simple token-based authentication:

```python
def verify_webhook_token(request) -> bool:
    token = request.headers.get('X-Webhook-Token')
    return token == os.environ['WEBHOOK_TOKEN']
```

---

## Outgoing Webhooks

### Completion Webhooks

Get notified when a specific job completes:

```bash
curl -X POST https://api.spooled.cloud/api/v1/jobs \
  -H "Authorization: Bearer sk_live_YOUR_API_KEY" \
  -d '{
    "queue_name": "exports",
    "payload": {"user_id": 123},
    "completion_webhook": "https://api.example.com/webhooks/export-complete"
  }'
```

When job completes, Spooled POSTs to your URL:

```json
{
  "event": "job.completed",
  "job_id": "job_xxx",
  "queue_name": "exports",
  "status": "completed",
  "result": { ... },
  "completed_at": "2024-01-15T10:30:00Z"
}
```

### Event Subscriptions

Subscribe to events for all jobs:

```bash
curl -X POST https://api.spooled.cloud/api/v1/outgoing-webhooks \
  -H "Authorization: Bearer sk_live_YOUR_API_KEY" \
  -d '{
    "name": "Job Notifications",
    "url": "https://api.example.com/webhooks/spooled",
    "events": ["job.completed", "job.failed", "job.dead_letter"],
    "secret": "your_webhook_secret"
  }'
```

You can inspect deliveries and retry individual delivery attempts:

```bash
# List delivery history
curl https://api.spooled.cloud/api/v1/outgoing-webhooks/{id}/deliveries \
  -H "Authorization: Bearer sk_live_YOUR_API_KEY"

# Retry a specific delivery
curl -X POST https://api.spooled.cloud/api/v1/outgoing-webhooks/{id}/retry/{delivery_id} \
  -H "Authorization: Bearer sk_live_YOUR_API_KEY"
```

### Event Types

| Event | Description |
|-------|-------------|
| `job.created` | New job enqueued |
| `job.processing` | Job claimed by worker |
| `job.completed` | Job completed successfully |
| `job.failed` | Job failed (may retry) |
| `job.dead_letter` | Job moved to DLQ |
| `queue.paused` | Queue paused |
| `queue.resumed` | Queue resumed |
| `worker.registered` | New worker connected |
| `worker.offline` | Worker went offline |

### Outgoing Webhook Format

```json
{
  "id": "evt_xxx",
  "type": "job.completed",
  "created_at": "2024-01-15T10:30:00Z",
  "data": {
    "job_id": "job_xxx",
    "queue_name": "emails",
    "status": "completed",
    "result": { ... }
  }
}
```

### Webhook Headers

Outgoing webhooks include:

```
Content-Type: application/json
X-Spooled-Event: job.completed
X-Spooled-Signature: sha256=xxxxx
X-Spooled-Timestamp: 1705315800
X-Spooled-Delivery-ID: del_xxx
```

### Verify Outgoing Webhook Signature

```python
import hmac
import hashlib

def verify_spooled_signature(payload: bytes, signature: str, timestamp: str, secret: str) -> bool:
    signed_payload = f"{timestamp}.{payload.decode()}"
    expected = hmac.new(
        secret.encode(),
        signed_payload.encode(),
        hashlib.sha256
    ).hexdigest()
    
    return hmac.compare_digest(f"sha256={expected}", signature)
```

### Retry Behavior

Outgoing webhooks retry with exponential backoff:

| Attempt | Delay |
|---------|-------|
| 1 | Immediate |
| 2 | 1 minute |
| 3 | 5 minutes |
| 4 | 30 minutes |
| 5 | 2 hours |

After 5 failed attempts, the webhook is marked as failed.

---

## Best Practices

### 1. Always Verify Signatures

Never process webhooks without verifying the signature:

```python
@app.post("/webhooks/spooled")
def handle_webhook(request):
    if not verify_signature(request):
        return {"error": "Invalid signature"}, 401
    
    event = request.json
    process_event(event)
    return {"received": True}
```

### 2. Respond Quickly

Return a 2xx response immediately, then process asynchronously:

```python
@app.post("/webhooks/stripe")
def handle_stripe_webhook(request):
    # Verify signature
    verify_stripe_signature(request)
    
    # Return immediately
    event = request.json
    background_task.delay(process_stripe_event, event)
    
    return {"received": True}, 200
```

### 3. Use Idempotency Keys

External services may retry webhooks. Use idempotency:

```python
def process_event(event):
    event_id = event.get("id") or event.get("idempotency_key")
    
    # Check if already processed
    if redis.get(f"webhook:{event_id}"):
        return {"status": "already_processed"}
    
    # Process event
    handle_event(event)
    
    # Mark as processed
    redis.setex(f"webhook:{event_id}", 86400, "1")
```

### 4. Store Raw Payloads

Log incoming webhooks for debugging:

```python
def handle_webhook(request):
    # Store raw payload
    db.webhook_logs.insert({
        "received_at": datetime.utcnow(),
        "headers": dict(request.headers),
        "body": request.get_data(as_text=True)
    })
    
    # Then process
    process_webhook(request.json)
```

### 5. Handle Missing Events

Your webhook endpoint might miss events. Implement reconciliation:

```python
# Daily reconciliation job
def reconcile_stripe_events():
    # Fetch recent events from Stripe
    events = stripe.Event.list(created={"gte": yesterday})
    
    for event in events:
        if not already_processed(event.id):
            process_event(event)
```

### 6. Use HTTPS Only

Webhook endpoints must use HTTPS in production. Spooled will refuse to send to HTTP endpoints.

---

## Troubleshooting

### Webhook Not Received

1. Check endpoint is publicly accessible
2. Verify HTTPS certificate is valid
3. Check firewall allows incoming connections
4. Verify webhook token is correct

### Signature Verification Failing

1. Ensure you're using the raw request body (not parsed JSON)
2. Check secret hasn't been rotated
3. Verify timestamp is within acceptable range (5 minutes)

### Duplicate Events

1. Implement idempotency using event IDs
2. Use database unique constraints
3. Check for retry scenarios

---

## Next Steps

- [Jobs Guide](./jobs.md) — Processing webhook-created jobs
- [Workers Guide](./workers.md) — Building job processors
- [API Reference](./api-usage.md) — Complete API documentation
