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
| Custom | `/api/v1/webhooks/{org_id}/custom` | `X-Webhook-Token` (required) |

Incoming custom webhooks **always require** the `X-Webhook-Token` header.
Tokens are generated automatically when your organization is created.

---

## Custom Webhooks

For any external service, use the custom webhook endpoint.

### Get Your Webhook URL

```bash
curl https://api.spooled.cloud/api/v1/organizations/webhook-token \
  -H "Authorization: Bearer sp_live_YOUR_API_KEY"
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
| `payload` | object | Yes | Job data (request body is capped at 2 MiB; the plan's job payload cap also applies) |
| `event_type` | string | No | Event type for logging |
| `priority` | integer | No | -100 to 100 (default: 0) |
| `idempotency_key` | string | No | Prevent duplicates |

### Managing Webhook Token

```bash
# Regenerate token (invalidates old one)
curl -X POST https://api.spooled.cloud/api/v1/organizations/webhook-token/regenerate \
  -H "Authorization: Bearer sp_live_YOUR_API_KEY"
```

### Inbound Request Log

`GET /api/v1/webhooks/deliveries` lists the requests this endpoint accepted for your
organization, newest first, so you can check that a provider is calling and what became of each
call:

```bash
curl "https://api.spooled.cloud/api/v1/webhooks/deliveries?status=duplicate&limit=20" \
  -H "Authorization: Bearer sp_live_YOUR_API_KEY"
```

```json
[
  {
    "id": "d3a1c0f2-...",
    "provider": "custom",
    "event_type": "order.created",
    "matched_queue": "my-events",
    "job_id": "job_550e8400e29b41d4",
    "status": "created",
    "created_at": "2026-10-12T09:15:00Z"
  }
]
```

`status` is `created` when the request made a new job and `duplicate` when its `idempotency_key`
matched an existing one (`job_id` and `matched_queue` then name that job and its queue). Filters:
`status`, `since` (RFC 3339), `queue_name` (alias `queue`), `limit` (default 50, at most 100) and
`offset`; the `X-Has-More` response header (`true` or `false`, since 0.1.124) says whether another
page follows. Only accepted requests are logged: a wrong token or an over-limit request writes
nothing. The log never returns the payload, a signature or the token, it needs an unrestricted API
key (a queue-scoped key gets `403`), and entries older than 0.1.122 have a `null` `job_id` and
`status`. Entries are removed with the rest of your delivery history (see the retention table
under [Delivery History](#delivery-history)).

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
  -H "Authorization: Bearer sp_live_YOUR_API_KEY" \
  -d '{
    "queue_name": "exports",
    "payload": {"user_id": 123},
    "completion_webhook": "https://api.example.com/webhooks/export-complete",
    "completion_webhook_secret": "whsec_optional_shared_secret"
  }'
```

When job completes, Spooled POSTs to your URL. With `completion_webhook_secret` set, also sends
`X-Spooled-Timestamp` and `X-Spooled-Signature` (`sha256=` HMAC of `{timestamp}.{rawBody}`), same as org outgoing webhooks. Without a secret, the callback is unsigned.

```json
{
  "event": "job.completed",
  "created_at": "2024-01-15T10:30:00Z",
  "data": {
    "job_id": "job_xxx",
    "queue_name": "exports",
    "status": "completed",
    "result": { ... },
    "completed_at": "2024-01-15T10:30:00Z"
  }
}
```

A completion webhook is a single attempt with no delivery history. It shares the outgoing-webhook
HTTP client but has its own limits: an organization gets a quarter of the request slots and of the
pending queue, so one slow or unreachable callback URL does not hold up other callbacks or other
organizations' webhooks. When the queue is full the callback is dropped (and counted in
`spooled_completion_webhooks_dropped_total` on a self-hosted deployment) instead of waiting
without limit. A job that is dead-lettered also sends its completion webhook, with event
`job.failed` and `data.status` `deadletter`, however it got there (a failure the worker reported,
a lease that expired on the last retry, or a worker that went offline).

### Event Subscriptions

Subscribe to events for all jobs:

```bash
curl -X POST https://api.spooled.cloud/api/v1/outgoing-webhooks \
  -H "Authorization: Bearer sp_live_YOUR_API_KEY" \
  -d '{
    "name": "Job Notifications",
    "url": "https://api.example.com/webhooks/spooled",
    "events": ["job.completed", "job.failed", "worker.registered"],
    "secret": "your_webhook_secret"
  }'
```

### Chat Formats (Slack, Discord, Teams)

By default a webhook receives the signed JSON event envelope (see
[Outgoing Webhook Format](#outgoing-webhook-format)). Set `format` to send a short chat message
instead, so the incoming-webhook URL of the chat service can be used as it is:

```bash
curl -X POST https://api.spooled.cloud/api/v1/outgoing-webhooks \
  -H "Authorization: Bearer sp_live_YOUR_API_KEY" \
  -d '{
    "name": "Slack alerts",
    "url": "https://hooks.slack.com/services/T000/B000/XXXX",
    "events": ["job.failed", "workflow.failed"],
    "format": "slack"
  }'
```

| `format` | Body sent | Notes |
|----------|-----------|-------|
| `json` (default) | `{id, event, created_at, data}` | The signed envelope, unchanged. Every webhook created before 0.1.122 is `json` |
| `slack` | `{"text": "..."}` | mrkdwn; `&`, `<` and `>` in values are escaped, so a value cannot become a link or an `<!channel>` |
| `discord` | `{"content": "...", "allowed_mentions": {"parse": []}}` | At most 2,000 characters, markdown escaped; `allowed_mentions` is empty, so an `@everyone` in an error text pings nobody |
| `teams` | A `MessageCard` (`title`, `summary`, `themeColor`, `text`, one section of `facts`) | The legacy Office 365 connector card. **Teams Workflows (Power Automate) webhooks expect an Adaptive Card and reject it** |

The message has a title for the event ("Spooled: job failed") and these fields of the event's
`data`, in this order, when present: name, workflow, job, queue, schedule, worker, host, status,
previous status, source, completed / failed / total jobs, error, reason and message. A value is
cut at 300 characters.

- `format` is case-insensitive when you send it; anything but the four values is a `400`. On
  `PUT /api/v1/outgoing-webhooks/{id}`, omitting it keeps the current format.
- The headers are the same for every format and `X-Spooled-Signature` (when a secret is set)
  signs the bytes actually sent. Chat services ignore it, so a `secret` is rarely worth setting
  for them.
- The message is built when the delivery is sent. The delivery history keeps the JSON envelope in
  `payload`, and a manual retry sends with the webhook's current format.
- `POST /api/v1/outgoing-webhooks/{id}/test` sends the test in the chat format as well; Slack and
  Discord answer `400` to raw JSON.

### Updating a Webhook

`PUT /api/v1/outgoing-webhooks/{id}` is a partial update: fields you omit keep their
current value.

```bash
# Rotate the signing secret
curl -X PUT https://api.spooled.cloud/api/v1/outgoing-webhooks/{id} \
  -H "Authorization: Bearer sp_live_YOUR_API_KEY" \
  -d '{"secret": "your_new_webhook_secret"}'
```

The `secret` field has three states:

| You send | Result |
|----------|--------|
| Field omitted | Current secret is kept |
| `"secret": "new_value"` | Secret is replaced |
| `"secret": null` | Secret is **removed** |

> **⚠️ Sending `"secret": null` deletes the secret.** Deliveries then go out unsigned,
> with no `X-Spooled-Signature` header at all, and any receiver that enforces signature
> verification will reject every one of them. This is how you deliberately remove a
> secret. It is also a trap for clients that round-trip the whole object and serialise
> unchanged fields as explicit `null` — send only the fields you mean to change, or set
> the secret to a string rather than `null`.

### Delivery History

You can inspect deliveries and retry individual delivery attempts:

```bash
# List delivery history
curl https://api.spooled.cloud/api/v1/outgoing-webhooks/{id}/deliveries \
  -H "Authorization: Bearer sp_live_YOUR_API_KEY"

# Retry a specific delivery (max 10 total attempts per delivery)
curl -X POST https://api.spooled.cloud/api/v1/outgoing-webhooks/{id}/retry/{delivery_id} \
  -H "Authorization: Bearer sp_live_YOUR_API_KEY"
```

> **Note:** You cannot retry successful deliveries, and each delivery has a maximum of 10 total delivery attempts across automatic and manual retries.

Each delivery record explains a failure: `error` holds the reason (`HTTP 503`, a transport error such
as a refused connection or a timeout, or `Invalid webhook URL: ...`), `response_body` the first 1 KiB
of your endpoint's answer (for any status), and `status_code` is `null` when no response arrived at
all. Manually retrying a delivery whose stored URL is rejected by the SSRF check answers
`{"success": false}` and counts as a failed delivery.

Delivery history is kept for your plan's history retention window, then removed:

| Plan | Delivery history kept |
|------|----------------------|
| Free | 1 day |
| Starter | 7 days |
| Pro | 30 days |
| Enterprise | 90 days |

Once a delivery row is swept it disappears from the list and can no longer be retried,
so on Free you are debugging today's failures, not last week's. Only the newest 100
deliveries per webhook are returned in any case — export anything you need to keep.

### Event Types

| Event | Description |
|-------|-------------|
| `job.created` | New job enqueued |
| `job.started` | Job claimed by worker |
| `job.completed` | Job completed successfully |
| `job.failed` | Job failed; `data.status` may be `failed` or `deadletter`. A job dead-lettered because its lease expired or its worker went offline sends it too (`data.status` `deadletter`, `data.error` the reason) |
| `job.cancelled` | Job cancelled |
| `job.deadlettered` | Job moved to the dead-letter queue. Sent next to every `job.failed` whose `data.status` is `deadletter`, with the same `data` |
| `job.retried` | A `failed`, `deadletter` or `cancelled` job put back to `pending` by `POST /api/v1/jobs/{id}/retry`, `POST /api/v1/jobs/dlq/retry` or `POST /api/v1/workflows/{id}/retry`; `data.old_status` is the status it left, `data.source` the endpoint (`job_retry`, `dlq_retry`, `workflow_retry`) |
| `workflow.completed` | Workflow finished with no failed or dead-lettered job. Sent **at most once**, see below |
| `workflow.failed` | Workflow finished with at least one failed or dead-lettered job (a cancelled workflow sends neither). Sent **at most once**, see below |
| `queue.paused` | Queue paused |
| `queue.resumed` | Queue resumed |
| `worker.registered` | New worker connected |
| `worker.deregistered` | Worker disconnected |
| `schedule.triggered` | Schedule created jobs |

#### Workflow outcomes are announced at most once

`workflow.completed` and `workflow.failed` are not sent by the request that settles the
workflow's last job: that happens inside a database trigger, where no webhook can be sent.
Instead a scheduler pass, every 10 seconds, claims the workflows that ended since the last
pass. The claim marks each outcome as announced and commits, and only then are the events
handed to the in-memory delivery queue. So:

- **No outcome is announced twice**, even with several Spooled instances running the pass.
- **An outcome can be lost.** If the process stops between that commit and the delivery being
  recorded (a restart or deploy at that instant), if the delivery queue is full at that moment
  (the drop is counted in `spooled_outgoing_webhook_deliveries_dropped_total`), or if recording
  the delivery fails, the outcome is marked announced but never sent, and no later pass sends
  it. Once recorded, the delivery is retried like any other event (see
  [Retry Behavior](#retry-behavior)).
- **A pass sees only the workflow's state at that moment.** A failed workflow that is retried
  with `POST /api/v1/workflows/{id}/retry` before the next pass, so within about 10 seconds of
  failing, never announces that `workflow.failed`; the outcome it reaches after the retry is
  announced.
- **Only outcomes less than an hour old are sent.** An outcome reached while no scheduler ran
  for over an hour, or before 0.1.122, is never sent.

If you act on workflow outcomes, treat the event as a prompt and confirm with
`GET /api/v1/workflows/{id}`, and reconcile periodically with `GET /api/v1/workflows` instead of
relying on the event alone.

#### Data of the newer events

Every event uses the envelope below; only `data` differs.

```json
// job.deadlettered: the data of the job.failed it follows
{"job_id": "job_xxx", "queue_name": "emails", "status": "deadletter", "error": "Failed after 3 retries"}

// job.retried
{"job_id": "job_xxx", "queue_name": "emails", "status": "pending", "old_status": "deadletter",
 "source": "dlq_retry", "workflow_id": "wf_xxx"}

// workflow.completed / workflow.failed
{"workflow_id": "wf_xxx", "name": "nightly-export", "status": "failed", "total_jobs": 5,
 "completed_jobs": 3, "failed_jobs": 2, "created_at": "2026-10-12T00:00:00Z",
 "started_at": "2026-10-12T00:00:12Z", "completed_at": "2026-10-12T00:03:40Z",
 "metadata": null}
```

- `job.deadlettered` is sent once per dead-lettering, whichever path did it (a failure the worker
  reported over REST or gRPC, a lease that expired on the last retry, a worker that went offline,
  a job too large for a gRPC message), right after the matching `job.failed`. Subscribe to only
  one of the two unless you want both. Under heavy overload the second can be dropped while the
  first is delivered.
- `job.retried`: `old_status` is `failed`, `deadletter` or `cancelled` (a step a workflow retry
  re-opens); `source` is `job_retry`, `dlq_retry` or `workflow_retry`; `workflow_id` is present
  only for a workflow job. One event per job, sent after the retry committed.
- `workflow_id` in `workflow.completed` / `workflow.failed` is the id to pass to
  `GET /api/v1/workflows/{id}`.

### Outgoing Webhook Format

```json
{
  "id": "evt_xxx",
  "event": "job.completed",
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
X-Spooled-Delivery-Attempt: 1
```

The delivery ID is the top-level `id` field in the JSON body. With a chat `format` the body is
the chat message, which has no `id`; read the delivery ID from the delivery history instead.

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

Outgoing webhooks use best-effort in-process retry with short exponential backoff:

| Attempt | Delay |
|---------|-------|
| 1 | Immediate |
| 2 | 1 second |
| 3 | 2 seconds |
| 4 | 4 seconds |
| 5 | 8 seconds |

Five attempts is the default; on a self-hosted deployment the limit is
`OUTGOING_WEBHOOK_MAX_ATTEMPTS`. After the attempt limit, the delivery remains failed.
You can also use the manual retry endpoint above; manual retry sends immediately and
records the new result.

Deliveries go through a bounded queue worked by a fixed number of senders
(`OUTGOING_WEBHOOK_MAX_CONCURRENT_DELIVERIES`, default 64, which also sets the queue size:
16 deliveries per sender). One webhook may have at most a quarter of the senders in flight (16 at
the default) and a quarter of the queue (256), so a slow endpoint cannot hold up other webhooks.
A delivery that is waiting for a retry holds no sender. Before every attempt Spooled re-reads the
webhook: if it was disabled, deleted or unsubscribed from the event in the meantime, the queued
delivery is dropped, and the current URL and secret are used.

Under sustained overload new events are **dropped** rather than kept in memory without limit; on a
self-hosted deployment `spooled_outgoing_webhook_deliveries_dropped_total` counts them (see the
operations guide). Raise `OUTGOING_WEBHOOK_MAX_CONCURRENT_DELIVERIES` if a busy, healthy endpoint
needs more than a quarter of the senders. Delivery order is not guaranteed — use the event
`created_at` if you need ordering.

### Automatic Disabling

**After 20 consecutive failed deliveries, Spooled disables the webhook.** It is set to
`enabled: false` with `last_status: "auto_disabled"` and stops receiving events entirely
— no further deliveries, no further retries, and nothing queued up for later. An endpoint
that stays down through a long outage will therefore go quiet rather than being retried
forever.

`last_status` is one of `success`, `failed` or `auto_disabled`. Poll the webhook, or watch
for `failure_count` climbing, to notice before the threshold is reached:

```bash
curl https://api.spooled.cloud/api/v1/outgoing-webhooks/{id} \
  -H "Authorization: Bearer sp_live_YOUR_API_KEY"
```

`failure_count` counts **consecutive failed deliveries**, one per delivery rather than one
per retry attempt, and any successful delivery — including a successful manual retry —
resets it to 0.

A webhook whose stored URL is rejected by the SSRF check (for example it now resolves to a private
address) counts each event as a failed delivery, without retries, and is disabled after 20 of them
like any other.

To turn the webhook back on once your endpoint is healthy:

```bash
curl -X PUT https://api.spooled.cloud/api/v1/outgoing-webhooks/{id} \
  -H "Authorization: Bearer sp_live_YOUR_API_KEY" \
  -d '{"enabled": true}'
```

Turning a webhook back on, or changing its `url`, starts a fresh failure count (`failure_count`
back to 0), so the first failed delivery after you fix the endpoint does not disable it again.

> **Note:** Re-enabling counts against your plan's webhook limit. If you created other
> webhooks while this one was disabled you may already be at the cap, and the request
> returns `429` with `QUOTA_EXCEEDED` — delete or disable another webhook first.

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

### 7. SSRF Protection

In production, Spooled validates webhook URLs to prevent Server-Side Request Forgery (SSRF) attacks:

- ❌ Private IP ranges (10.x.x.x, 172.16-31.x.x, 192.168.x.x)
- ❌ Loopback addresses (127.x.x.x, localhost, ::1)
- ❌ Link-local addresses (169.254.x.x, fe80::)
- ❌ Cloud metadata endpoints (169.254.169.254)
- ❌ Internal hostnames (.local, .internal, .corp)
- ❌ HTTP URLs (must use HTTPS)

This ensures webhooks can only target legitimate external services.

---

## Troubleshooting

### Webhook Not Received

1. Check the webhook is still enabled — a run of 20 consecutive failed deliveries sets
   `enabled: false` and `last_status: "auto_disabled"` (see [Automatic Disabling](#automatic-disabling))
2. Check endpoint is publicly accessible
3. Verify HTTPS certificate is valid
4. Check firewall allows incoming connections
5. Verify webhook token is correct

### Signature Verification Failing

1. Ensure you're using the raw request body (not parsed JSON)
2. Check secret hasn't been rotated — or cleared, if something sent `"secret": null` on an update
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
