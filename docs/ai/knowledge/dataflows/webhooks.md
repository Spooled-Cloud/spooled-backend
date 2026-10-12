# Dataflow: Webhooks

## Incoming (job ingest)

`POST /api/v1/webhooks/{org_id}/custom` — requires `X-Webhook-Token`; a soft-deleted org (`plan_tier = 'deleted'`) answers 404 like an unknown one, and admin soft delete also removes the org's webhook token, pauses its schedules and disables its outgoing webhooks in the same transaction (0.1.120, BEFLOW-06); production HTTPS via `X-Forwarded-Proto`. Inserts jobs using configured queue defaults (`QUEUE_DEFAULT_MAX_RETRIES` / `QUEUE_DEFAULT_TIMEOUT_SECS` via settings). Response is `{ job_id, queue_name, status }` (OpenAPI `WebhookResponse`), not an empty 200. All three come from the upsert's `RETURNING`, so an idempotent replay reports the **existing** job's queue and current status (`processing`/`completed`/...), not a hardcoded `pending`. The token lives in `organizations.settings.webhook_token`. `PUT /organizations/{id}` and `PATCH /admin/organizations/{id}` preserve it on object settings (omit keeps it); a non-object `settings` body is 400 while a token is configured, because that replace would drop it. `POST .../webhook-token/clear` is not allowed (rotate instead). `POST .../webhook-token/regenerate` always writes the new token into an object (non-object settings become `{webhook_token}`).

Since 0.1.122 (QW11) the `webhook_deliveries` row written for each accepted request (`handlers/webhooks.rs:93` `custom`) also records `job_id` and `status` (`created`, or `duplicate` when the idempotency key matched an existing job; NULL before 0.1.122), and `matched_queue` is the queue of the job the request resolved to (the request's `queue_name` only differs on an idempotent replay). See "Inbound webhook log" below.

## Outgoing org webhooks (`outgoing_webhooks/service.rs`)

Events in `VALID_WEBHOOK_EVENTS` (`models/webhook.rs:18`):  
`job.created|started|completed|failed|cancelled`, `job.deadlettered`, `job.retried`, `queue.paused|resumed`, `worker.registered|deregistered`, `schedule.triggered`, `workflow.completed`, `workflow.failed` (`job.deadlettered`, `job.retried`, `workflow.completed` and `workflow.failed` were added in 0.1.122, QW8; the table CHECK `outgoing_webhook_events_check` was replaced by a superset in migration `20261014100000`, so a webhook still cannot subscribe to an unknown event).

Delivery body: `{ id, event, created_at, data }` (for `format = json`; see "Format" below). Headers: `X-Spooled-Event`, `X-Spooled-Timestamp`, `X-Spooled-Delivery-Attempt`, optional `X-Spooled-Signature` (`sha256=` HMAC of `{ts}.{body}`) when secret set. URL SSRF-validated. Delivery uses bounded in-process retries (`OUTGOING_WEBHOOK_MAX_ATTEMPTS`, default 5, exponential backoff), plus a manual retry endpoint. Deliveries go through a bounded queue worked by a fixed set of consumers, sized by `OUTGOING_WEBHOOK_MAX_CONCURRENT_DELIVERIES` (default 64): past the bound an event is dropped and counted, not queued (see "Bounded delivery queue" below). Ordering is not guaranteed and never was.

`job.failed` fires from every dead-letter path since 0.1.121 (BEFLOW-11, QW7): REST fail, gRPC fail and too-large, and the scheduler's lease-expiry and offline-worker sweeps (`Scheduler::announce_deadlettered`, `scheduler/mod.rs:382`), all with `data.status` `deadletter`; the sweeps also send the job's completion webhook and the realtime `JobStatusChange` / `JobFailed` events and write the `deadlettered` job_history row in the same CTE.

The REST `fail` handler takes the status it puts in `job.failed` from the transition (`handlers/jobs.rs:1211`), not from a SELECT after the commit (0.1.122): by then a scheduled retry may be claimed again or a dead-lettered job retried, and the event (and the `job.deadlettered` it implies) was skipped or wrong. `src/webhook/mod.rs` is an unused legacy module whose `INSERT INTO webhook_deliveries` names columns that do not exist; do not revive it.

### Events added in 0.1.122 (QW8)

- **`job.deadlettered` is derived, never emitted by hand.** `events::implied_event` (`outgoing_webhooks/events.rs:38`) maps a
  `job.failed` whose `data.status == "deadletter"` to `job.deadlettered`; `OutgoingWebhookService::spawn_dispatch` (`service.rs:254`)
  and `dispatch_event` (`:301`) queue it with the same data **after** the original (`queue_event` `:277`: with the queue nearly full the
  `job.failed` wins the last slot, and the derived one is dropped and counted). Every dead-letter path already sends exactly one
  `job.failed` with that status (REST fail, gRPC `Fail` / `ProcessJobs` / too-large `dead_letter_oversized`, the lease-expiry and
  offline-worker sweeps), so each transition sends it once. A caller that also sends `job.deadlettered` doubles it; a path that
  re-queues a job for retry must not use `status: "deadletter"` (invariants #42).
- **`job.retried`** (`events::job_retried_payload` `:48`): `{job_id, queue_name, status: "pending", old_status, source, workflow_id?}`,
  one `spawn_dispatch` per job after the commit of `POST /jobs/{id}/retry`, `POST /jobs/dlq/retry` and `POST /workflows/{id}/retry`
  (`source` `job_retry` / `dlq_retry` / `workflow_retry`; `old_status` `failed`, `deadletter` or `cancelled`). Call sites:
  `handlers/jobs.rs:1797,2646`, `handlers/workflows.rs:793`.
- **`workflow.completed` / `workflow.failed` are announced by the scheduler, not at the transition.** A workflow ends in the progress
  trigger `update_workflow_progress` (inside whatever transaction settled the last job, where no webhook can be sent) or in the
  terminate sweep. The end of `update_job_dependencies` (the 10 s dependency pass, `scheduler/mod.rs:1249`) calls
  `announce_workflow_outcomes` (`events.rs:175`), which runs `ANNOUNCE_WORKFLOW_OUTCOMES_SQL` (`:132`):
  `UPDATE workflows SET announced_completed_at = completed_at WHERE id = ANY(ARRAY(SELECT id ... WHERE status IN
  ('completed','failed') AND announced_completed_at IS DISTINCT FROM completed_at AND completed_at > NOW() - INTERVAL '1 hour'
  ORDER BY completed_at LIMIT 500 FOR UPDATE SKIP LOCKED)) RETURNING ...` (migration `20261014100200` adds the column) and one
  `spawn_dispatch` per returned row. A retry nulls `completed_at` (`POST /workflows/{id}/retry`, `reopen_workflows` in `jobs.rs`),
  so the next outcome has a new `completed_at` and is announced again; a future reopen path that keeps `completed_at` would
  suppress the next announcement. A cancelled workflow sends neither event.
  - **The form `id = ANY(ARRAY(subselect))` is deliberate**: a CTE or join made the planner hash-join the pick with a seq scan of
    `workflows` (test `w8_workflow_outcome_sweep_uses_its_index` checks the plan). The statement's predicate and order are those of
    the partial index `idx_workflows_unannounced_outcome` (`20261014100201`, CIC; repair `...100202`): change the two together.
  - **At most once, by design (W8-R4, invariants #41).** The claim commits `announced_completed_at` before the events reach the
    in-memory delivery queue and nothing re-sends a claimed outcome. So no outcome is sent twice (any number of instances), but it
    is lost when: the process stops between that commit and the consumer recording the delivery rows; the delivery queue (or the
    webhook's share) is full (dropped and counted in `spooled_outgoing_webhook_deliveries_dropped_total`) or recording fails; the
    workflow is retried before the pass sees it (a workflow that fails and is retried within one 10 s tick never announces that
    failure); the outcome is over an hour old when a pass first sees it (no scheduler ran; also what keeps the first pass after the
    deploy from announcing history). More than 500 outcomes per pass wait for the next one. At-least-once needs a durable outbox.
    Documented in `docs/guides/webhooks.md` and `docs/openapi.yaml`; consumers confirm with `GET /workflows/{id}`.

### Format: json, slack, discord, teams (0.1.122, QW14)

`outgoing_webhooks.format` (`TEXT NOT NULL DEFAULT 'json'` + CHECK, migration `20261014100000`), `WebhookFormat` and
`validate_webhook_format` in `models/webhook.rs:44,85` (case-insensitive and trimmed on input, stored lowercase, an unknown value
a 400 `Invalid format '<x>'. Valid formats: json, slack, discord, teams`; omitted = `json` on create and "keep" on update).
`OutgoingWebhook` has `#[sqlx(default)] format`, so an explicit column list without it still loads (as `json`). Rendering is
`outgoing_webhooks/format.rs::body_for(format, envelope)` (`:42`) at delivery time (`service.rs:791`) and in `POST
/outgoing-webhooks/{id}/test`; `json` returns the envelope serialization exactly as before.
- The delivery log (`outgoing_webhook_deliveries.payload`) stores the **envelope**, and `retry_delivery` re-renders with the
  webhook's current format (the row it joins carries `w.format`). Headers are the same for every format and
  `X-Spooled-Signature` signs the bytes actually sent.
- `slack` `{"text"}` (mrkdwn, `& < >` escaped so values cannot form links or `<!channel>`); `discord` `{"content"}` at most 2000
  characters, markdown escaped, `allowed_mentions: {"parse": []}` so an `@everyone` in an error text pings nobody; `teams` a
  legacy `MessageCard` (`themeColor` 2EB67D / D9302F / 4A6CF7 by event, facts for the fields). Teams Workflows (Power Automate)
  webhooks expect an Adaptive Card and are not covered (a possible future `format` value). Fields shown, in order, when present
  in `data`: name, workflow_id, job_id, queue_name, schedule_id, worker_id, hostname, status, old_status, source, completed_jobs,
  failed_jobs, total_jobs, error, reason, message; values are cut at 300 characters. A body without `data` (the test endpoint's) is
  read as the data itself.
- `Work::Delivery` is boxed (`Box<Delivery>`, clippy `large_enum_variant`).

### Inbound webhook log (0.1.122, QW11)

`GET /api/v1/webhooks/deliveries` (`handlers/webhooks.rs:326` `list_deliveries`, `ListInboundDeliveriesQuery` `models/webhook.rs:492`):
organization-wide, so **unrestricted keys only** (`require_unrestricted_key`, a queue-scoped key gets 403 `ACCESS_DENIED`, never 401).
`limit` default 50 clamped to 1..100, `offset`, `status` (`created` | `duplicate`), `since` (RFC 3339, `>=`), `queue_name` (alias
`queue`, exact on `matched_queue`, at most 255 characters); an invalid filter is a 400. Newest first (`created_at DESC, id DESC`).
Columns returned: `id, provider, event_type, matched_queue, job_id, status, created_at`, never the payload, a signature or the
organization id (the webhook token is never stored). Only **accepted** requests are logged (a rejected one writes nothing, so
unauthenticated traffic cannot amplify writes). `job_id`/`status` are nullable (`20261014100100`; the `status` CHECK is `NOT VALID`
because validating would scan the log under `ACCESS EXCLUSIVE`; every existing row is NULL). Retention is the existing sweep.

### Bounded delivery queue (0.1.121, BEFLOW-10, W5-R2)

`OutgoingWebhookService` (`outgoing_webhooks/service.rs`) replaced one spawned task per event, each parked on a global semaphore with a payload copy, with a bounded queue and a fixed set of consumers.
- Limits come from `DeliveryLimits::from_concurrency(N)` (`:76`), N = `OUTGOING_WEBHOOK_MAX_CONCURRENT_DELIVERIES` clamped to 1..1024: N consumers and N request slots, a queue of 16N items (events plus deliveries), and per webhook N/4 requests in flight and 4N queued. At the default: 64 consumers, queue 1024, 16 requests and 256 queued per webhook. `with_limits` takes explicit values.
- `spawn_dispatch` (`:254`) admits an `Event` item or drops and counts it (`note_drop`, warn on the first drop and every 100th); it never spawns and never blocks. A consumer runs the webhook SELECT and fans the event out into one `Delivery` per matching webhook (`fan_out` `:514`); `dispatch_event` does the same inline and returns once the deliveries are queued. Each delivery holds an admission permit plus a share of its webhook's queue until it resolves; one waiting for its webhook's request slot waits off the consumer and is re-queued.
- `attempt` (`:657`) re-reads the webhook before EVERY attempt: disabled, deleted or no longer subscribed means the delivery is abandoned (not counted as a failure); the current URL and secret are used. Backoff is 2^(attempt-1) s capped at 32 s and holds no request slot. Outcomes: `Delivered`, `Failed` (retry), `Rejected` (SSRF-invalid stored URL: no retry, but it counts as a failed delivery so the breaker still trips). `failure_count` moves once per delivery.
- Delivery rows (`record_attempt` `:850`, BEFLOW-24): `error` holds the reason (`HTTP 503`, transport error without the URL, `Invalid webhook URL: ...`), `response_body` the first 1 KiB (`MAX_STORED_RESPONSE_BODY_BYTES`, NULs stripped), `status_code` is NULL when no response arrived (it was 0). A database error while recording an attempt is logged and does not abort the delivery.
- `PUT /outgoing-webhooks/{id}` resets `failure_count` to 0 when it turns the webhook on or changes its URL; `last_status` is kept (the dashboard reads `auto_disabled` there as "re-enabled"). A manual retry of a delivery whose stored URL is SSRF-rejected answers 200 `{success: false}` instead of 500 (BEFLOW-25).
- Metrics, sampled on every scrape by `Metrics::attach_outgoing_webhooks` (attached in `AppState::new`): `spooled_outgoing_webhook_deliveries_dropped_total`, `spooled_outgoing_webhook_queue_depth`, `spooled_outgoing_webhook_deliveries_in_flight`, `spooled_completion_webhooks_dropped_total`.
- Consequences: a single healthy endpoint is capped at N/4 concurrent requests (about 80 requests/s at 200 ms latency by default) and 4N queued; raise the setting for a tenant that needs more. Memory is bounded but not small: queue capacity times payload size (about 1 GiB at the defaults with 1 MiB payloads). Each attempt does one extra primary-key SELECT. Consumers start on first dispatch; queued and backing-off deliveries are lost at shutdown, as detached tasks always were.

### Secret update is three-state (`PUT /api/v1/outgoing-webhooks/{id}`)

`double_option` on the request `secret` field: field omitted → keep stored secret; `null` → **clear** it (deliveries then go out unsigned, no `X-Spooled-Signature`); string → replace. Before 0.1.111 `null` was silently ignored, so a secret could never be removed. Do **not** "simplify" this back to `Option<String>` — that reintroduces the un-removable-secret bug, and note that a client which serialises unchanged fields as explicit `null` now wipes the secret.

### Auto-disable + `failure_count` (0.1.111)

`AUTO_DISABLE_FAILURE_THRESHOLD = 20` / `AUTO_DISABLED_STATUS = "auto_disabled"` (`outgoing_webhooks/service.rs` ~20–24). After 20 **consecutive** failed deliveries the row flips to `enabled = false`, `last_status = "auto_disabled"` and stops receiving events. Re-enable is `PUT /api/v1/outgoing-webhooks/{id}` `{"enabled": true}`, which is charged against the plan webhook cap — so re-enabling can fail `429 QUOTA_EXCEEDED`. `last_status` domain is now `success | failed | auto_disabled`.

`failure_count` is incremented once per **delivery**, not once per retry attempt, so for the same real-world failures it is ~5x smaller than pre-0.1.111 numbers; any inherited `failure_count > N` threshold is wrong. A successful delivery resets it to 0, including a successful manual retry.

### Delivery history retention

`outgoing_webhook_deliveries` rows are deleted by the per-organization retention sweep at the plan's `history_retention_days` (free 1, starter 7, pro 30, enterprise 90). The API only ever exposed the newest 100 deliveries per webhook.

## Per-job `completion_webhook`

`spawn_completion_webhook(organization_id, url, event, payload, secret)` (`service.rs:1143`) / `deliver_completion_webhook` — SSRF check + POST JSON with `X-Spooled-Event`. Since 0.1.121 they run through a process-wide `CompletionGate` (`:1037`, BEFLOW-27, W5-R2): the first `OutgoingWebhookService` built (AppState's, in production; tests under plain `cargo test` share it, CI uses nextest) lends it its HTTP client and its `DeliveryLimits`, but NOT its request slots (consumers wait on those inline, so a tenant's completion backlog queued there stalled everyone's org webhooks). The gate has its own slot pool and a per-organization lane: a quarter of the slots and of the pending bound per org; past the pending bound a completion webhook is dropped and counted (`completion_webhooks_dropped()`, `spooled_completion_webhooks_dropped_total`). Still a single attempt and no delivery log. The organization id is the first argument, so every call site (REST, gRPC, scheduler) must pass it. When `completion_webhook_secret` is set on the job (REST create), also sends `X-Spooled-Timestamp` + `X-Spooled-Signature` (`sha256=` HMAC of `{ts}.{body}`) — same scheme as org outgoing webhooks. Absent secret → unsigned (backward compatible).
