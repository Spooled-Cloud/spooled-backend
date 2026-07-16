# Dataflow: Webhooks

## Incoming (job ingest)

`POST /api/v1/webhooks/{org_id}/custom` — requires `X-Webhook-Token`; production HTTPS via `X-Forwarded-Proto`. Inserts jobs using configured queue defaults (`QUEUE_DEFAULT_MAX_RETRIES` / `QUEUE_DEFAULT_TIMEOUT_SECS` via settings).

## Outgoing org webhooks (`outgoing_webhooks/service.rs`)

Events in `VALID_WEBHOOK_EVENTS` (`models/webhook.rs`):  
`job.created|started|completed|failed|cancelled`, `queue.paused|resumed`, `worker.registered|deregistered`, `schedule.triggered`.

Delivery body: `{ id, event, created_at, data }`. Headers: `X-Spooled-Event`, `X-Spooled-Timestamp`, `X-Spooled-Delivery-Attempt`, optional `X-Spooled-Signature` (`sha256=` HMAC of `{ts}.{body}`) when secret set. URL SSRF-validated. Delivery uses bounded in-process retries, plus a manual retry endpoint.

Note: `job.deadlettered` appears in legacy `src/webhook/` but is **not** in `VALID_WEBHOOK_EVENTS` for the live outgoing service.

## Per-job `completion_webhook`

`spawn_completion_webhook` / `deliver_completion_webhook` — SSRF check + POST JSON with `X-Spooled-Event`. When `completion_webhook_secret` is set on the job (REST create), also sends `X-Spooled-Timestamp` + `X-Spooled-Signature` (`sha256=` HMAC of `{ts}.{body}`) — same scheme as org outgoing webhooks. Absent secret → unsigned (backward compatible).
