# Dataflow: Webhooks

## Incoming (job ingest)

`POST /api/v1/webhooks/{org_id}/custom` — requires `X-Webhook-Token`; production HTTPS via `X-Forwarded-Proto`. Inserts job with hardcoded retries/timeout `3`/`300`.

## Outgoing org webhooks (`outgoing_webhooks/service.rs`)

Events in `VALID_WEBHOOK_EVENTS` (`models/webhook.rs`):  
`job.created|started|completed|failed|cancelled`, `queue.paused|resumed`, `worker.registered|deregistered`, `schedule.triggered`.

Delivery headers: `X-Spooled-Event`, `X-Spooled-Timestamp`, `X-Spooled-Delivery-Attempt`, optional `X-Spooled-Signature` (`sha256=` HMAC of `{ts}.{body}`) when secret set. URL SSRF-validated.

Note: `job.deadlettered` appears in legacy `src/webhook/` but is **not** in `VALID_WEBHOOK_EVENTS` for the live outgoing service.

## Per-job `completion_webhook`

`spawn_completion_webhook` / `deliver_completion_webhook` — SSRF check + POST JSON with `X-Spooled-Event` only; **no HMAC signature** (`service.rs` ~372–400).
