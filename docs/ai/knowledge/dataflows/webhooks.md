# Dataflow: Webhooks

## Incoming (job ingest)

`POST /api/v1/webhooks/{org_id}/custom` — requires `X-Webhook-Token`; production HTTPS via `X-Forwarded-Proto`. Inserts jobs using configured queue defaults (`QUEUE_DEFAULT_MAX_RETRIES` / `QUEUE_DEFAULT_TIMEOUT_SECS` via settings).

## Outgoing org webhooks (`outgoing_webhooks/service.rs`)

Events in `VALID_WEBHOOK_EVENTS` (`models/webhook.rs`):  
`job.created|started|completed|failed|cancelled`, `queue.paused|resumed`, `worker.registered|deregistered`, `schedule.triggered`.

Delivery body: `{ id, event, created_at, data }`. Headers: `X-Spooled-Event`, `X-Spooled-Timestamp`, `X-Spooled-Delivery-Attempt`, optional `X-Spooled-Signature` (`sha256=` HMAC of `{ts}.{body}`) when secret set. URL SSRF-validated. Delivery uses bounded in-process retries (`OUTGOING_WEBHOOK_MAX_ATTEMPTS`, default 5, exponential backoff), plus a manual retry endpoint. In-flight deliveries are capped process-wide by `OUTGOING_WEBHOOK_MAX_CONCURRENT_DELIVERIES` (default 64) — over that, deliveries queue. Ordering is not guaranteed and never was.

Note: `job.deadlettered` appears in legacy `src/webhook/` but is **not** in `VALID_WEBHOOK_EVENTS` for the live outgoing service.

### Secret update is three-state (`PUT /api/v1/outgoing-webhooks/{id}`)

`double_option` on the request `secret` field: field omitted → keep stored secret; `null` → **clear** it (deliveries then go out unsigned, no `X-Spooled-Signature`); string → replace. Before 0.1.111 `null` was silently ignored, so a secret could never be removed. Do **not** "simplify" this back to `Option<String>` — that reintroduces the un-removable-secret bug, and note that a client which serialises unchanged fields as explicit `null` now wipes the secret.

### Auto-disable + `failure_count` (0.1.111)

`AUTO_DISABLE_FAILURE_THRESHOLD = 20` / `AUTO_DISABLED_STATUS = "auto_disabled"` (`outgoing_webhooks/service.rs` ~20–24). After 20 **consecutive** failed deliveries the row flips to `enabled = false`, `last_status = "auto_disabled"` and stops receiving events. Re-enable is `PUT /api/v1/outgoing-webhooks/{id}` `{"enabled": true}`, which is charged against the plan webhook cap — so re-enabling can fail `429 QUOTA_EXCEEDED`. `last_status` domain is now `success | failed | auto_disabled`.

`failure_count` is incremented once per **delivery**, not once per retry attempt, so for the same real-world failures it is ~5x smaller than pre-0.1.111 numbers; any inherited `failure_count > N` threshold is wrong. A successful delivery resets it to 0, including a successful manual retry.

### Delivery history retention

`outgoing_webhook_deliveries` rows are deleted by the per-organization retention sweep at the plan's `history_retention_days` (free 1, starter 7, pro 30, enterprise 90). The API only ever exposed the newest 100 deliveries per webhook.

## Per-job `completion_webhook`

`spawn_completion_webhook` / `deliver_completion_webhook` — SSRF check + POST JSON with `X-Spooled-Event`. When `completion_webhook_secret` is set on the job (REST create), also sends `X-Spooled-Timestamp` + `X-Spooled-Signature` (`sha256=` HMAC of `{ts}.{body}`) — same scheme as org outgoing webhooks. Absent secret → unsigned (backward compatible).
