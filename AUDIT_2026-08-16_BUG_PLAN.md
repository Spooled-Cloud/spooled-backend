# Spooled Backend — Deep Bug Audit & Remediation Plan

**Audit date:** 2026-08-16
**Codebase reviewed:** `spooled-backend` @ `9ae1d90` (`chore(db): publish postgres on host loopback`), `Cargo.toml` version `0.1.110`
**Scope:** whole Rust backend (`src/**`, 62 files, ~35 300 lines), migrations, and the deployment/config surface that the backend depends on.
**Mode:** READ-ONLY review. **Nothing in this audit has been fixed.** Every item below is a proposal for a future change.

---

## 0. How to use this document (read this first)

This file is written so that an engineer or an AI agent who has **never seen this
codebase** can pick up any single finding and act on it without needing the
original reviewer, and without needing to have read the rest of the file.

Every finding follows the same fixed template:

| Field | Meaning |
| --- | --- |
| **ID** | Stable identifier, e.g. `B-07`. Quote this in commits/PRs. |
| **Severity** | `CRITICAL` / `HIGH` / `MEDIUM` / `LOW` / `NIT`. Defined in §1. |
| **Confidence** | `CONFIRMED` (read the code and the logic provably breaks) or `SUSPECTED` (looks wrong, needs a runtime check first — the check is spelled out). |
| **Where** | `path/to/file.rs:LINE` plus the function name. Line numbers are as of commit `9ae1d90`; if they have drifted, search for the quoted snippet instead. |
| **What is wrong** | The defect, stated plainly. |
| **Why it is wrong** | The mechanism — what the code does vs. what it must do. |
| **How it fails in production** | A concrete trigger: specific request, specific state, specific observable damage. If a finding has no plausible trigger it is not a bug and does not belong here. |
| **Proposed fix** | Concrete direction, with code sketch where the shape is non-obvious. |
| **How to verify the fix** | The exact test/command/query that must go from failing to passing. |
| **Blast radius** | What else touches this code and could break when it changes. |

### Ground rules for whoever implements these

1. **Do not batch unrelated findings into one commit.** One finding (or one
   tightly-coupled group, explicitly noted) per commit, message referencing the ID.
2. **`SUSPECTED` findings must be reproduced before being "fixed".** Each one
   lists the check to run. If the check shows the code is fine, mark the finding
   `INVALID` in this file with a one-line reason — do not silently delete it.
3. **This codebase has been audited several times before** (see §2). Prior audits
   fixed a large number of issues; the fixes themselves are now part of the
   surface being reviewed. Where a finding is a *regression of* or *gap left by*
   a prior fix, that is stated explicitly.
4. **Never weaken a security control to make a test pass.** Several findings sit
   next to authorization code; the correct fix is always to tighten, never to
   relax the check.
5. **Migrations are append-only.** Do not edit an existing file in `migrations/`;
   add a new timestamped one.

### Status legend for tracking work

- `[ ]` not started · `[~]` in progress · `[x]` fixed + verified · `[!]` INVALID (with reason) · `[-]` deliberately deferred (with reason and owner)

---

## 1. Severity definitions used in this document

| Severity | Definition |
| --- | --- |
| **CRITICAL** | Cross-tenant data exposure, authentication/authorization bypass, remote crash of the whole process, silent data loss, or money handled wrongly. Ship a fix before the next release. |
| **HIGH** | Corrupts state for one tenant, loses one tenant's jobs, breaks a documented API contract, or is a denial-of-service vector reachable by an authenticated caller. |
| **MEDIUM** | Wrong behaviour in a real but narrower path: an edge case, a race that needs contention, a limit that does not hold under concurrency, a misleading error code. |
| **LOW** | Behaviour is technically wrong but self-correcting, cosmetic at the API layer, or requires an unlikely combination of state. |
| **NIT** | Correctness-neutral: clarity, dead code, a comment that lies, a log line that misleads an operator. Batch these. |

---

## 2. Prior audit history (context — do not re-litigate)

The backend has been through several remediation waves. Knowing what was already
fixed prevents "rediscovering" a closed issue and, more usefully, tells you which
code is *new* and therefore least-tested.

| Release | What it changed (condensed) |
| --- | --- |
| `v0.1.83` | 28-bug sweep; 24 fixed and live-verified. |
| `v0.1.86` | Atomic resource caps via `pg_advisory_xact_lock`; quota `429 QUOTA_EXCEEDED`. |
| `v0.1.88` | Expired-lease worker-op rejection (`409 LEASE_EXPIRED`). REST auth became Bearer-only (`X-API-Key` is gRPC-only). |
| `v0.1.89` | Workflow-retry cascade to dependents; cron `dom`/`dow` OR semantics; schedule-trigger `429`; gRPC fail-open removed. |
| `v0.1.90` | **Security:** indexed `api_keys.lookup_hash` — removed a bcrypt-scan amplification on REST+gRPC auth. `custom_limits`/`stripe` null-reset via `double_option`. |
| `v0.1.91` | Payload-too-large `200` → `413`; atomic daily quota on bulk + gRPC enqueue; gRPC retry backoff minutes → seconds; lease reclaim `<=`. |
| `v0.1.92` | **Security:** `/auth/login` ported to `lookup_hash` (was an unauthenticated bcrypt-scan DoS); `/ws` bound to `ApiKeyContext` (was accepting refresh tokens + confused-deputy org binding). |
| `v0.1.93` | **Security, major:** queue-scope enforced on *all* REST job read/control paths, REST queue ops, workflow/schedule indirect enqueue, realtime WS/SSE fan-out, and gRPC `StreamJobs`/`ProcessJobs`. Scoped keys can no longer mint wildcard keys (`can_grant_queues`). JWT middleware reads `expires_at` + current queues from the DB instead of trusting token claims. |
| `v0.1.94` | `lease_id` fencing on `complete`/`fail`/`heartbeat`/`renew`; stale lease → `409 LEASE_EXPIRED`. |
| `v0.1.110` | Current. Billing/invoice gating, outgoing-webhook retries, REST worker contract alignment, OpenAPI sync. |

**Implication for this audit:** the code added *after* `v0.1.94` (billing gating,
outgoing webhooks, worker contract) has had the least adversarial review and is
weighted accordingly below.

### Known-open items carried forward (operator action, not code)

- [ ] **`certs/grpc-key.pem` is tracked in git and baked into the image.** Must be rotated, moved to a mounted secret, and added to `.dockerignore`. Flagged in multiple prior sessions; still open. *Operator action — an agent cannot rotate this.*
- [ ] **`ADMIN_API_KEY` was printed into an agent transcript** in a prior session and has not been rotated. *Operator action.*

---

## 3. Findings

> **STATUS: audit complete, remediation IMPLEMENTED.** All 35 findings have been
> acted on — see **§5 Implementation record** for the per-finding outcome,
> including four findings that verification proved **INVALID** and one new
> finding (**B-36**) discovered while implementing. §3 below preserves the
> findings *as originally written*, so the corrections in §5 can be read against
> them; where §5 says a finding is INVALID, §5 is authoritative.

### 3.1 Outgoing webhooks (`src/outgoing_webhooks/`, `src/api/handlers/outgoing_webhooks.rs`)

This subsystem is one of the newest (delivery retries landed in `fbf9e8b`, just
before `v0.1.110`) and it carries the highest density of findings.

---

#### `[ ]` B-01 — Queue-scoped API keys can read **all** webhook delivery payloads, escaping their queue scope

- **Severity:** HIGH
- **Confidence:** CONFIRMED
- **Where:** [`src/api/handlers/outgoing_webhooks.rs:441`](src/api/handlers/outgoing_webhooks.rs#L441) — `deliveries()`; same gap at [`:498`](src/api/handlers/outgoing_webhooks.rs#L498) — `retry_delivery()`

**What is wrong.** Every other handler in this file starts with
`require_unrestricted_key(&ctx, "manage outgoing webhooks")?`. `deliveries()` and
`retry_delivery()` do not. Compare:

```rust
// list()   line 54  -> require_unrestricted_key(&ctx, "manage outgoing webhooks")?;
// create() line 87  -> require_unrestricted_key(...)
// get()    line 166 -> require_unrestricted_key(...)
// update() line 199 -> require_unrestricted_key(...)
// delete() line 286 -> require_unrestricted_key(...)
// test()   line 325 -> require_unrestricted_key(...)
// deliveries()    line 441 -> (missing)
// retry_delivery()line 498 -> (missing)
```

**Why it is wrong.** `v0.1.93` established the rule that a queue-scoped key may
only observe data from queues in its scope. A webhook delivery row's `payload`
column contains the **full event envelope**, including the job payload, for
whatever queue produced the event. `deliveries()` returns `payload` verbatim
(`SELECT id, webhook_id, event, payload, ...` at line 464) filtered only by
`webhook_id`. Org ownership *is* checked (lines 447–460), so this is not
cross-tenant — but it is a clean **intra-tenant scope escape**.

**How it fails in production.** An organization issues a partner an API key
scoped to `queue: partner-imports` only. That partner calls
`GET /api/v1/outgoing-webhooks/{any-webhook-id}/deliveries` (webhook ids are
discoverable — `retry_delivery` distinguishes "Webhook not found" from "Delivery
not found", and ids are UUIDs returned in dashboard/API responses to anyone in
the org). They receive up to 100 delivery envelopes containing job payloads from
`billing`, `pii-export`, and every other queue. `retry_delivery` additionally
lets them re-fire another queue's event at the org's endpoint.

**Proposed fix.** Add `require_unrestricted_key(&ctx, "manage outgoing webhooks")?;`
as the first statement of both handlers. Managing/observing org-wide webhook
delivery history is an org-admin operation, exactly like the other six handlers
in the file. Do **not** attempt to filter deliveries by queue — the delivery row
has no queue column, and inferring queue from the payload is unreliable.

**How to verify the fix.** New test in `tests/security_tests.rs`: create an org,
create an outgoing webhook, trigger one delivery, mint a queue-scoped key, and
assert `GET /outgoing-webhooks/{id}/deliveries` → `403` and
`POST /outgoing-webhooks/{id}/retry/{delivery_id}` → `403`. Assert an
unrestricted key still gets `200`.

**Blast radius.** Any SDK or dashboard call that reads delivery history with a
scoped key will start receiving `403`. Check `spooled-dashboard` — it
authenticates with a session JWT that maps to an unrestricted context, so it
should be unaffected, but confirm before shipping.

---

#### `[ ]` B-02 — Disabled-then-enabled webhooks bypass the plan's webhook cap

- **Severity:** MEDIUM
- **Confidence:** CONFIRMED
- **Where:** [`src/api/handlers/outgoing_webhooks.rs:193`](src/api/handlers/outgoing_webhooks.rs#L193) — `update()`

**What is wrong.** `create()` enforces the cap under an advisory lock, and
deliberately counts only *enabled* webhooks:

```rust
// create(), lines 112-121
let adding = if request.enabled { 1 } else { 0 };
let mut tx = state.db.pool().begin().await?;
lock_resource(&mut tx, &ctx.organization_id, "webhooks").await?;
check_resource_limit_conn(&mut tx, &ctx.organization_id, "webhooks", adding).await
```

`update()` can flip `enabled: false → true` (line 248, `let enabled =
request.enabled.unwrap_or(existing.enabled);`) with **no limit check at all**.

**Why it is wrong.** The counting function only counts enabled rows —
`migrations/20241212000001_fix_org_resource_counts.sql`:
`(SELECT COUNT(*) FROM outgoing_webhooks WHERE organization_id = p_org_id AND enabled = TRUE)`.
So `enabled` is the thing being capped, and the only path that sets `enabled`
after creation does not check the cap.

**How it fails in production.** A `free`-plan org whose webhook cap is N:
`POST /outgoing-webhooks` × 50 with `{"enabled": false}` — all succeed, `adding`
is 0 every time. Then `PUT /outgoing-webhooks/{id}` × 50 with
`{"enabled": true}` — all succeed. The org now runs 50 enabled webhooks on a
plan that allows N. Every job event fans out 50× (see B-04), multiplying the
outbound-request cost the plan was sized for.

**Proposed fix.** In `update()`, when the effective `enabled` is `true` and
`existing.enabled` was `false`, take the same advisory lock and run
`check_resource_limit_conn(..., "webhooks", 1)` inside a transaction that also
performs the UPDATE. When `enabled` is unchanged or being turned off, skip the
check (turning off must never fail on quota).

**How to verify the fix.** Test: set `custom_limits` to `{"max_webhooks": 2}`,
create 3 disabled webhooks (expect `201`×3), enable two (expect `200`×2), enable
the third → expect `429` with `code: "QUOTA_EXCEEDED"`. Then disable one and
re-enable the third → expect `200`.

**Blast radius.** `update()` becomes fallible on quota. The dashboard's webhook
toggle must render the `429` body rather than assuming success.

---

#### `[ ]` B-03 — `failure_count` is incremented per **attempt**, not per delivery, and is never read

- **Severity:** MEDIUM
- **Confidence:** CONFIRMED
- **Where:** [`src/outgoing_webhooks/service.rs:323`](src/outgoing_webhooks/service.rs#L323) — `record_attempt()`

**What is wrong.** Two defects in one field.

1. `record_attempt` is called once per *attempt* (from `try_deliver`, line 252 /
   265), and each call runs:
   ```sql
   failure_count = CASE WHEN $2 = 'success' THEN 0 ELSE failure_count + 1 END
   ```
   `deliver()` retries up to `max_attempts` times for one event, so a single
   failing event increments `failure_count` by up to `max_attempts`.
2. Nothing ever reads it to make a decision. `grep -rn "failure_count" src/`
   returns only writes plus `SELECT` lists that ship it to the API. There is no
   circuit breaker, no auto-disable, no alert threshold.

**Why it is wrong.** The field is presented through the API
(`OutgoingWebhookSummary`, `src/models/webhook.rs:70`) as "how many times has
this webhook failed", and a user reading it will size their alerting against it.
With per-attempt counting the number is inflated by a factor equal to the retry
count, so it is not the quantity its name and its API exposure promise.

**How it fails in production.** A customer's endpoint has a 30-second outage
during which 4 events fire. With `max_attempts = 5` the dashboard reports
`failure_count: 20`, not `4`. Worse, because nothing consumes the field, a
webhook pointing at a permanently dead host is retried forever at full rate —
see B-04, which this field exists to prevent and does not.

**Proposed fix.** Two separable changes; do them in one commit since they touch
the same statement:
- Move the `failure_count` bump out of `record_attempt` into `deliver()`, run
  once after the retry loop resolves, incrementing only when the final outcome
  was failure. Keep `last_status`/`last_triggered_at` per-attempt (those are
  genuinely "most recent attempt" fields).
- Add a consumer: in `dispatch_event`'s SELECT, skip webhooks whose
  `failure_count` exceeds a threshold (config, default e.g. 20), and set
  `enabled = false` with a `last_status = 'auto_disabled'` marker so the user can
  see why. Note the interaction with B-02: auto-disabling frees cap headroom, so
  re-enabling must go through the capped path.

**How to verify the fix.** Point a webhook at a always-500 test server, dispatch
one event with `max_attempts = 5`, assert `failure_count == 1` (not 5). Then
dispatch until the threshold and assert `enabled` flips to `false` and no
further HTTP requests are made.

**Blast radius.** Auto-disable is a behaviour change users will notice. It needs
a line in `CHANGELOG.md` and probably a dashboard affordance to re-enable.

---

#### `[ ]` B-04 — Unbounded fan-out of delivery tasks: one slow endpoint can exhaust process memory

- **Severity:** HIGH
- **Confidence:** CONFIRMED
- **Where:** [`src/outgoing_webhooks/service.rs:91-106`](src/outgoing_webhooks/service.rs#L91) — `dispatch_event()`, together with [`:126-140`](src/outgoing_webhooks/service.rs#L126) — `deliver()`

**What is wrong.** There is no bound anywhere on how many webhook deliveries are
in flight. `dispatch_event` spawns one unbounded `tokio::spawn` per matching
webhook:

```rust
for webhook in webhooks {
    let payload = payload.clone();          // full job payload, cloned per webhook
    let service = self.clone();
    tokio::spawn(async move { service.deliver(webhook, ...).await });
}
```

and each spawned task then *sleeps inside itself* between retries:

```rust
let backoff = Duration::from_secs(2_u64.pow((attempt - 1).min(5) as u32));
tokio::time::sleep(backoff).await;
```

**Why it is wrong.** Task lifetime is decoupled from request rate. With a 10 s
request timeout and backoffs of 1+2+4+8 s, a single event against a dead endpoint
occupies a task for roughly 55 s while holding a clone of the job payload (up to
the 1 MiB plan cap). Nothing applies backpressure: `spawn_dispatch` is called
from the hot enqueue/complete paths and returns instantly.

**How it fails in production.** An org enqueues 5 000 jobs/minute on a plan that
allows it, with one enabled webhook on `job.completed` pointing at an endpoint
that black-holes connections. Steady state ≈ 5 000 × (55 s / 60 s) ≈ 4 600
concurrent tasks, each holding a payload clone — hundreds of MB of resident
memory plus 4 600 sockets, in the same process that serves the API. Multiply by
the number of enabled webhooks (see B-02, where that number is not actually
capped). The failure mode is OOM or connection-pool starvation of the whole
backend, triggered by one tenant's misconfigured URL.

**Proposed fix.** Replace fire-and-forget spawning with a bounded worker pool:
- A `tokio::sync::Semaphore` (config: `outgoing_webhooks.max_concurrent_deliveries`,
  default e.g. 64) acquired inside the spawned task before the first HTTP call,
  **or**
- better, a bounded `mpsc` channel of delivery jobs plus a fixed set of consumer
  tasks. When the channel is full, drop with a `warn!` and a counter metric
  rather than growing memory.

Either way, add a metric (`outgoing_webhook_deliveries_inflight`) so the
saturation is visible. B-03's circuit breaker is the complementary fix — it
stops feeding the pool from a known-dead endpoint.

**How to verify the fix.** Load test: point a webhook at a listener that accepts
connections and never responds, dispatch 10 000 events, and assert
`outgoing_webhook_deliveries_inflight` plateaus at the configured maximum and
process RSS stays flat. Before the fix the same test grows both without bound.

**Blast radius.** Delivery latency under load becomes queue-dependent rather than
immediate. Document that ordering is still not guaranteed (it never was).

---

#### `[ ]` B-05 — `outgoing_webhook_deliveries` grows forever; no retention anywhere

- **Severity:** MEDIUM
- **Confidence:** CONFIRMED
- **Where:** table created in [`migrations/20241209000010_outgoing_webhooks.sql:52`](migrations/20241209000010_outgoing_webhooks.sql#L52); written at [`src/outgoing_webhooks/service.rs:293`](src/outgoing_webhooks/service.rs#L293); **no deleter exists**

**What is wrong.** The scheduler has a per-org, plan-tier-aware retention sweep
(`cleanup_old_data_by_org`, `src/scheduler/mod.rs:995+`) covering jobs and job
history. `outgoing_webhook_deliveries` is not in it.
`grep -rn "outgoing_webhook_deliveries" src/` shows exactly four sites: one
INSERT, three SELECTs. Nothing deletes.

**Why it is wrong.** Every attempt of every delivery writes a row whose `payload`
column holds the full event envelope — i.e. a copy of the job payload, bounded
only by the 1 MiB plan cap. The read path only ever exposes the newest 100
(`deliveries()`, `LIMIT 100`), so 100% of the rows beyond that are write-only
storage.

**How it fails in production.** An org doing 1 M job completions/month with one
enabled webhook and a 2 KB payload writes ~2 GB/month of rows that no endpoint
can ever return. Growth is silent until the volume shows up as disk pressure or
as autovacuum falling behind on a table nobody is monitoring. It also silently
extends the retention of customer job payloads beyond the retention promised for
`jobs` — a data-handling inconsistency, not just a disk problem.

**Proposed fix.** Add `outgoing_webhook_deliveries` to `cleanup_old_data_by_org`,
using the same `history_retention_days` the org's plan already defines (a
delivery record is history). Delete by `created_at < cutoff`, batched with a
`LIMIT` so the sweep cannot lock the table for long. Add an index on
`created_at` if the existing `(webhook_id, created_at DESC)` index does not serve
the delete plan — verify with `EXPLAIN` before adding, the existing one may
suffice.

**How to verify the fix.** Insert rows dated older than the tier's retention,
run one scheduler sweep, assert they are gone and that rows inside the window
survive. Confirm the org's own retention override in `custom_limits` is honoured,
matching how jobs are handled.

**Blast radius.** Same sweep that handles jobs — if the delete is not batched it
will lengthen a loop that already does several table scans. Measure sweep
duration before/after.

---

#### `[ ]` B-06 — `update()` can never clear a webhook secret

- **Severity:** LOW
- **Confidence:** CONFIRMED
- **Where:** [`src/api/handlers/outgoing_webhooks.rs:247`](src/api/handlers/outgoing_webhooks.rs#L247)

**What is wrong.** `let secret = request.secret.or(existing.secret);` — a `None`
in the request means "keep existing", so there is no representable request that
sets the secret back to NULL.

**Why it is wrong.** This is the exact bug class fixed in `v0.1.90` for
`custom_limits`/`stripe` using `double_option` (see §2). `Option<String>` cannot
distinguish "field absent" from "field explicitly null"; `Option<Option<String>>`
with `serde(default, deserialize_with = "double_option")` can.

**How it fails in production.** A user rotates away from signed webhooks (or
pastes a secret into the wrong webhook and wants it gone). `PUT` with
`{"secret": null}` returns `200` and the old secret is still active and still
signing. From the API response they cannot tell — the response echoes the stored
secret, so they *can* see it did not change, but the operation silently did
something other than what was asked.

**Proposed fix.** Change `UpdateOutgoingWebhookRequest::secret` to
`Option<Option<String>>` with the project's existing `double_option` helper
(find it via `grep -rn "double_option" src/`, it is already used for
`custom_limits`), then: `None` → keep, `Some(None)` → set NULL, `Some(Some(s))`
→ set `s`.

**How to verify the fix.** `PUT {"secret": null}` → `200`, `GET` shows
`secret: null`, and a subsequent delivery carries no `X-Spooled-Signature`
header. `PUT {}` (secret absent) leaves the secret unchanged.

**Blast radius.** Only this endpoint. Check the OpenAPI spec is regenerated —
`eb4543d` shows contracts are kept in sync deliberately.

---

#### `[ ]` B-07 — Completion webhooks and org webhooks send different envelopes

- **Severity:** LOW
- **Confidence:** CONFIRMED
- **Where:** [`src/outgoing_webhooks/service.rs:118`](src/outgoing_webhooks/service.rs#L118) (org) vs [`:401`](src/outgoing_webhooks/service.rs#L401) (per-job completion)

**What is wrong.** The org-webhook envelope is
`{id, event, created_at, data}`; the per-job `completion_webhook` envelope is
`{event, created_at, data}` — no `id`. The org path also always sends
`X-Spooled-Delivery-Attempt`; the completion path never does.

**Why it is wrong.** Receivers are told (by the docs and by the shape of the org
webhook) that `id` is the delivery identifier used for idempotent processing.
A receiver that dedupes on `body.id` silently degrades to "no dedupe" for
completion webhooks, because the field is `undefined` rather than absent-and-loud.

**How it fails in production.** A customer writes one handler for both webhook
kinds, keyed on `body.id`. Completion webhooks all hash to the same
`undefined` key: either every completion after the first is dropped as a
duplicate, or the dedupe map key collides. Both are data-losing and neither
produces an error anyone sees.

**Proposed fix.** Generate a `Uuid::new_v4()` in `deliver_completion_webhook`,
include it as `id`, and send `X-Spooled-Delivery-Attempt: 1`. This is additive —
existing receivers ignoring the field are unaffected.

**How to verify the fix.** Assert the completion webhook body has a `id`
parseable as a UUID and that two deliveries of the same event carry different
ids. Update `docs/` and the OpenAPI/webhook reference in the same commit.

**Blast radius.** Documentation and SDK webhook-verification helpers, if any
assert on the exact key set.

---

### 3.2 Workers (`src/worker/mod.rs`, `src/api/handlers/workers.rs`)

---

#### `[ ]` B-08 — Built-in worker writes the **deprecated** capacity columns, so the API always reports 0 active jobs and mis-marks workers `degraded`

- **Severity:** MEDIUM
- **Confidence:** CONFIRMED
- **Where:** [`src/worker/mod.rs:307-325`](src/worker/mod.rs#L307) — `send_heartbeat()`

**What is wrong.** The heartbeat UPDATE writes `current_jobs` and reads
`max_concurrency`:

```rust
UPDATE workers
SET last_heartbeat = NOW(),
    current_jobs = $1,
    status = CASE WHEN $1 > max_concurrency THEN 'degraded' ELSE 'healthy' END
WHERE id = $2 AND organization_id = $3
```

Both are the **deprecated** columns.
`migrations/20241209000007_worker_column_aliases.sql` explicitly renames the
live pair and marks the old pair dead:

```sql
COMMENT ON COLUMN workers.max_concurrency IS 'DEPRECATED: Use max_concurrent_jobs instead';
COMMENT ON COLUMN workers.current_jobs   IS 'DEPRECATED: Use current_job_count instead';
```

Everything else — `register_worker()` in the same file (line 198), the REST
handlers, and the `Worker` model — uses `max_concurrent_jobs` / `current_job_count`.

**Why it is wrong.** The two column pairs were never kept in sync after the
migration's one-time backfill. `register_worker()` inserts only
`max_concurrent_jobs`, so `max_concurrency` keeps its schema default of `5`
forever, and `current_job_count` is never advanced past its insert-time `0`.
The statement does not error — the deprecated columns still exist — so the bug
is completely silent.

**How it fails in production.** Two distinct user-visible faults:
1. `GET /api/v1/workers` and the dashboard read `current_job_count`, which the
   built-in worker never updates → a busy worker always displays **0 active
   jobs**. An operator diagnosing a backlog sees idle workers and concludes the
   queue is broken.
2. A worker configured with `max_concurrency: 50` is compared against the
   default `5`, so as soon as it holds 6 jobs it flips itself to `degraded`.
   `degraded` workers still count toward the plan's worker cap and are surfaced
   as unhealthy in `/health` — a healthy fleet reports itself sick.

Note this affects the **embedded** worker (dev/test per the module docs). Confirm
whether any production deployment runs it before down-grading the severity; the
`/health` worker counts at `src/api/handlers/health.rs:276` are computed from the
same columns and are user-visible either way.

**Proposed fix.** Change the statement to write `current_job_count` and compare
against `max_concurrent_jobs`, and also set `updated_at = NOW()` (currently never
touched by heartbeats, so `updated_at` lies about worker liveness). Then, in a
separate follow-up commit, drop the three deprecated columns
(`max_concurrency`, `current_jobs`, `registered_at`) in a new migration so this
class of bug cannot recur — grep the whole tree first, including
`tests/` and `spooled-dashboard`.

**How to verify the fix.** Integration test: start the embedded worker with
`max_concurrency: 10`, enqueue 6 jobs, and assert `GET /workers/{id}` reports
`current_jobs: 6` and `status: "healthy"`. Before the fix it reports `0` and
`degraded`.

**Blast radius.** `/health`'s active-worker counts; the dashboard's worker table;
any customer alerting on `status == "degraded"` — that alert will (correctly)
stop firing.

---

#### `[ ]` B-09 — `workers_active` gauge drifts permanently negative

- **Severity:** LOW
- **Confidence:** CONFIRMED
- **Where:** [`src/api/handlers/workers.rs:328`](src/api/handlers/workers.rs#L328) — `deregister()`

**What is wrong.** `deregister()` decrements unconditionally:

```rust
state.metrics.workers_active.dec();
if previous_status == "healthy" {
    state.metrics.workers_healthy.dec();
}
```

The `workers_healthy` decrement is correctly guarded by the previous status.
`workers_active` is not, and `deregister()` succeeds on a worker that is already
`offline` — the lookup only filters on `id` + `organization_id`, never on status.

**Why it is wrong.** These are Prometheus gauges maintained by hand across
handler invocations, with no reconciliation against the `workers` table. Any
non-idempotent path corrupts them until process restart. `deregister()` is
naturally retried by clients (it is idempotent from their perspective — a second
`DELETE` returns `204`).

**How it fails in production.** An SDK retries deregistration on shutdown, or a
supervisor restarts a worker container three times: each extra `DELETE` decrements
`workers_active` again. The gauge goes negative and stays there, so any dashboard
or alert built on `workers_active` is wrong until the backend is redeployed.
Note the stale-worker reaper (`src/scheduler/mod.rs:868`) also flips workers to
`offline` *without* touching the gauges, so drift accumulates from that direction
too.

**Proposed fix.** Guard the decrement the same way the healthy one is guarded —
`if previous_status != "offline"` — and, better, stop hand-maintaining these
gauges: compute them from the table in the metrics-collection path
(`SELECT status, COUNT(*) FROM workers GROUP BY status`), which is
self-correcting across restarts and reaper transitions alike. The same reasoning
applies to `workers_healthy`.

**How to verify the fix.** Register one worker, `DELETE` it three times, and
assert `workers_active` on `/metrics` is `0`, not `-2`.

**Blast radius.** Metrics only. If any alert rule currently compensates for the
drift, it must be updated.

---

#### `[ ]` B-10 — REST `register()` generates a fresh UUID, making its `ON CONFLICT` clause dead code

- **Severity:** NIT (but see the note — it hides a real gap)
- **Confidence:** CONFIRMED
- **Where:** [`src/api/handlers/workers.rs:126`](src/api/handlers/workers.rs#L126) and [`:152`](src/api/handlers/workers.rs#L152)

**What is wrong.** `let worker_id = Uuid::new_v4().to_string();` — the id is
generated server-side, immediately before the INSERT. The statement then carries:

```sql
ON CONFLICT (id) DO UPDATE SET status = 'healthy', last_heartbeat = NOW(), updated_at = NOW()
WHERE workers.organization_id = EXCLUDED.organization_id
```

plus the follow-up guard `if result.rows_affected() == 0 { Conflict("Worker ID
already in use") }`. A freshly generated v4 UUID cannot collide, so the conflict
branch and the `409` it produces are unreachable.

**Why it is wrong.** It is not merely dead — it is *misleading*. The comment
above it ("Previously, an attacker could update another org's worker status by
using their ID") describes a threat model in which the client supplies the id,
which this endpoint does not allow. A future change that adds a client-supplied
id will look protected when the protection was never exercised.

Contrast the gRPC path (`src/grpc/services/worker_service.rs:318`), which does
upsert on a caller-supplied id — that one needs the guard and has it.

**How it fails in production.** It does not fail; it misleads. The concrete
consequence is the absence of any way for a restarting worker to reclaim its
previous identity over REST: every restart creates a new row. Those rows persist
as `healthy` until the reaper's two-minute window flips them to `offline`, during
which they consume the org's worker cap. A crash-looping worker on a tight plan
can therefore `429` itself out of registering.

**Proposed fix.** Either (a) accept an optional client-supplied `worker_id` in
`RegisterWorkerRequest` so restarts are true upserts and the `ON CONFLICT` guard
becomes live — matching gRPC's semantics and eliminating the cap-churn; or (b)
delete the unreachable `ON CONFLICT` clause and the `rows_affected` check and
document that REST registration is always-new. **(a) is the better fix** because
it also closes the cap-churn gap; do not do both.

**How to verify the fix.** For (a): register with an explicit `worker_id`, kill
and re-register with the same id, assert the worker count for the org stays at 1
and the second call returns `200`/`201` consistently. For (b): assert the removed
branch has no test asserting a `409` from this endpoint (there may be one that
currently passes vacuously — check `tests/endpoint_tests.rs`).

**Blast radius.** Option (a) changes a request schema (additive, optional) and
needs OpenAPI + SDK updates. Option (b) is code-only.

---

### 3.3 Queue core (`src/queue/mod.rs`) and scheduler (`src/scheduler/mod.rs`)

---

#### `[ ]` B-11 — `fail_by_worker`'s dead-letter path is not transactional: the job can dead-letter with no audit row

- **Severity:** MEDIUM
- **Confidence:** CONFIRMED
- **Where:** [`src/queue/mod.rs:893-951`](src/queue/mod.rs#L893) — `fail_by_worker()`, dead-letter branch

**What is wrong.** The sibling method `complete_by_worker` opens a transaction
(`let mut tx = self.db.begin().await?;`) and commits status + dependency updates
atomically. `fail_by_worker` does not: the status UPDATE (line 893) and the
`INSERT INTO dead_letter_queue` (line 931) are two independent statements, each
executed with `&*self.db` — separate pooled connections, separate implicit
transactions.

**Why it is wrong.** Between them the process can crash, the pool can hand back
an error, or the connection can drop. The UPDATE has already committed, so the
job is `deadletter` while the audit table has no record of why.

**How it fails in production.** A deploy or OOM-kill lands between the two
statements for an in-flight failure. The job shows `status: deadletter` in
`/jobs` (the DLQ *read* path queries `jobs`, so users still see it), but
`dead_letter_queue` — which stores `original_payload` and `error_details` and is
the thing an operator greps during an incident — is missing that row. The gap is
undetectable after the fact because nothing reconciles the two.

Note the same two-statement shape exists at
`src/grpc/services/queue_service.rs:824` and `:2010`; both are labelled "Parity
with REST" and inherit the same non-atomicity. Fix all three together.

**Proposed fix.** Wrap the status UPDATE and the DLQ INSERT of each dead-letter
path in one transaction, mirroring `complete_by_worker`. The INSERT's
`SELECT ... FROM jobs WHERE id = $3 AND organization_id = $4` must run inside
that transaction so it observes the row it is about to describe.

**How to verify the fix.** Unit/integration test with an injected failure between
the two statements (or simply assert, for every dead-lettered job produced by the
existing test suite, that a matching `dead_letter_queue` row exists). A cheap
production check: `SELECT COUNT(*) FROM jobs j WHERE j.status='deadletter' AND
NOT EXISTS (SELECT 1 FROM dead_letter_queue d WHERE d.job_id = j.id)` — run this
before the fix to size the existing damage. **Expect a non-zero result even
after this fix, because of B-12.**

**Blast radius.** Adds a transaction to the failure hot path. `fail_by_worker` is
called on every job failure; measure the added latency under load.

---

#### `[ ]` B-12 — The stale-worker reaper dead-letters jobs **without** writing a `dead_letter_queue` row

- **Severity:** MEDIUM
- **Confidence:** CONFIRMED
- **Where:** [`src/scheduler/mod.rs:949-971`](src/scheduler/mod.rs#L949) — `cleanup_stale_workers()`

**What is wrong.** Four code paths move a job to `deadletter`. Three of them
insert the corresponding audit row:

| Path | Location | Writes `dead_letter_queue`? |
| --- | --- | --- |
| REST/SDK explicit fail | `src/queue/mod.rs:931` | yes |
| gRPC `FailJob` | `src/grpc/services/queue_service.rs:824` | yes |
| gRPC `ProcessJobs` | `src/grpc/services/queue_service.rs:2010` | yes |
| scheduler expired-lease sweep | `src/scheduler/mod.rs:216` | yes (`WITH deadlettered AS ...`) |
| **stale-worker sweep** | **`src/scheduler/mod.rs:949`** | **no** |

The stale-worker branch is a bare `UPDATE jobs SET status = 'deadletter' ...`
with no INSERT — and the file's own comment at line 211 documents the convention
it is breaking ("Also insert a `dead_letter_queue` row, matching the explicit-fail
path in queue/mod.rs").

**Why it is wrong.** `dead_letter_queue` is the audit trail: it is the only place
that retains `original_payload` and `error_details` for a dead-lettered job, and
it is what survives the retention sweep that eventually deletes the `jobs` row.
A job that dies because its worker vanished — arguably the *most* interesting
dead-letter cause for debugging — is exactly the one with no audit record.

**How it fails in production.** A worker host is terminated while holding 200
jobs at `retry_count >= max_retries`. Two minutes later the reaper dead-letters
all 200. The user sees them in the DLQ view (which reads `jobs`) and, days later
after retention deletes the `jobs` rows, has no record they ever existed — while
jobs that failed through the normal path from the same incident are still fully
documented in `dead_letter_queue`.

**Proposed fix.** Convert the UPDATE to the same `WITH deadlettered AS (UPDATE
... RETURNING ...) INSERT INTO dead_letter_queue SELECT ... FROM deadlettered`
shape already used at `src/scheduler/mod.rs:216`. That form is atomic in a single
statement, so it also avoids B-11's problem by construction — consider making it
the pattern for all five paths.

**How to verify the fix.** Register a worker, claim a job with
`retry_count = max_retries`, stop heartbeating, wait out the reaper, and assert a
`dead_letter_queue` row exists with `reason = 'Max retries exceeded after worker
went offline'`.

**Blast radius.** None beyond the sweep. Fix alongside B-11 and re-run the
reconciliation query above; it should return 0 for newly created rows.

---

#### `[ ]` B-13 — Dead-lettered jobs keep a stale `lease_expires_at`

- **Severity:** LOW
- **Confidence:** CONFIRMED
- **Where:** [`src/queue/mod.rs:893-901`](src/queue/mod.rs#L893)

**What is wrong.** The retry branch of `fail_by_worker` clears the full lease
triple:

```rust
assigned_worker_id = NULL, lease_id = NULL, lease_expires_at = NULL,
```

The dead-letter branch, twenty lines later, clears only two:

```rust
assigned_worker_id = NULL, lease_id = NULL,     // lease_expires_at left as-is
```

**Why it is wrong.** The invariant everywhere else in the codebase is that a job
not in `processing` has no lease. `classify_worker_miss` (line 712) reads
`lease_expires_at` to decide `LeaseExpired` vs `NotOwned`, and the expired-lease
sweep filters on it. Leaving a non-NULL value on a terminal row means the column
no longer means what every reader assumes.

**How it fails in production.** Today the readers all also filter on
`status = 'processing'`, so nothing misbehaves — this is a latent inconsistency,
not an active fault. It becomes a real bug the moment any future query filters on
`lease_expires_at` alone (e.g. an "expired leases" dashboard panel or a reclaim
query that drops the status predicate), at which point dead-lettered jobs from
months ago appear as expiring leases.

**Proposed fix.** Add `lease_expires_at = NULL` to the dead-letter UPDATE. One
line; group it with B-11 since it is the same statement.

**How to verify the fix.** Assert `lease_expires_at IS NULL` for every job with
`status = 'deadletter'`. Add it as an invariant assertion in the integration
suite so all five dead-letter paths are covered.

**Blast radius.** None.

---

#### `[ ]` B-14 — `record_history` failures are reported as if the operation itself failed

- **Severity:** MEDIUM
- **Confidence:** CONFIRMED
- **Where:** [`src/queue/mod.rs:691`](src/queue/mod.rs#L691) (`complete_by_worker`), [`:583`](src/queue/mod.rs#L583) (`complete_with_worker_and_org`), [`:867`](src/queue/mod.rs#L867) and [`:953`](src/queue/mod.rs#L953) (`fail_by_worker`)

**What is wrong.** In each of these methods the state change is committed, and
*then* history is recorded with the error propagated:

```rust
tx.commit().await?;                                   // job is now completed — durable
// ...
self.record_history(job_id, "completed", json!({})).await?;   // <-- `?`
Ok(WorkerOpOutcome::Ok)
```

If `record_history` fails, the method returns `Err` even though the job
completion has already committed.

Note the codebase already knows this is wrong and does it correctly elsewhere:
`dequeue_batch` (line 261) deliberately ignores history errors — `let _ = self
.record_history(...)` with the comment "batch operation, don't fail on history
errors".

**Why it is wrong.** The return value is the handler's only signal. An `Err`
becomes a `500` (or a gRPC `INTERNAL`), which every SDK treats as "the call did
not happen" and retries.

**How it fails in production.** A transient DB hiccup on the `job_history`
INSERT — or simply a full/locked history table — makes `CompleteJob` return
`500` for a job that is already `completed`. The SDK retries; the retry now fails
the lease check and returns `409 LEASE_EXPIRED` or `404`. From the worker's point
of view the job "failed to complete" and, depending on SDK behaviour, it may
re-execute the job's business logic — turning a cosmetic logging failure into a
**duplicate side effect**, which is precisely what the lease/fencing work in
`v0.1.94` exists to prevent.

**Proposed fix.** Make post-commit history recording non-fatal in all four sites:
replace `.await?` with a logged best-effort call, matching `dequeue_batch`:

```rust
if let Err(e) = self.record_history(job_id, "completed", json!({})).await {
    warn!(job_id = %job_id, error = %e, "Failed to record job history (job already completed)");
}
```

Do **not** instead move the history INSERT inside the transaction: that would
make a history failure roll back a legitimate completion, which is worse.

**How to verify the fix.** Point `record_history` at a failing statement (or
temporarily rename `job_history`) and assert `complete_by_worker` still returns
`Ok(WorkerOpOutcome::Ok)` and logs a warning. Confirm the job is `completed` in
the DB.

**Blast radius.** History gaps become possible and silent — that is the intended
trade. Add the warn-level log so the gap is at least greppable.

---

#### `[ ]` B-15 — Comment claims multi-parent dependency support that the query does not implement

- **Severity:** LOW
- **Confidence:** CONFIRMED
- **Where:** [`src/queue/mod.rs:517-541`](src/queue/mod.rs#L517)

**What is wrong.** The comment says "A child job's dependencies are met when ALL
its parent jobs are completed", and the SQL's inline comment admits the opposite
two lines later ("For now, we only support single-parent dependencies via
parent_job_id"). The query keys entirely on the scalar `jobs.parent_job_id`.

Real multi-parent dependencies exist and live in a different table —
`job_dependencies` (`migrations/20241209000003_job_dependencies.sql`), with a
`dependency_mode` of `'all'`/`'any'` — and are resolved by a DB trigger
(`update_dependent_jobs` → `check_job_dependencies_met`) plus the scheduler
sweep at `src/scheduler/mod.rs:734`.

**Why it is wrong.** Two parallel dependency mechanisms with one misleading
comment is how someone "fixes" the wrong one. The code is correct; the comment
is not.

**How it fails in production.** It does not, directly. The risk is a future
change to this query intended to alter multi-parent behaviour, which would have
no effect on workflow jobs (they use `job_dependencies`) while appearing to.

**Proposed fix.** Rewrite the doc comment to state precisely: this path handles
only the legacy scalar `parent_job_id` edge; workflow dependencies are handled by
the `update_dependent_jobs` trigger and the scheduler sweep. Cross-reference both
locations by path.

**How to verify the fix.** N/A — documentation. Batch with other NIT-level items.

**Blast radius.** None.

---

#### `[ ]` B-16 — Jobs whose dependency dead-letters can strand as permanently un-runnable

- **Severity:** MEDIUM
- **Confidence:** SUSPECTED — **reproduce before fixing** (procedure below)
- **Where:** `check_job_dependencies_met()` in [`migrations/20241209000003_job_dependencies.sql`](migrations/20241209000003_job_dependencies.sql), vs. the compensating sweep at [`src/scheduler/mod.rs:742-806`](src/scheduler/mod.rs#L742)

**What is wrong.** `check_job_dependencies_met` only ever returns true for
`'all'` mode when `v_completed_deps = v_total_deps`. A dependency that ends in
`failed`/`deadletter`/`cancelled` never counts as completed, so the dependent's
`dependencies_met` stays `FALSE` and the dequeue predicate
(`AND (dependencies_met IS NULL OR dependencies_met = TRUE)`,
`src/queue/mod.rs:357`) permanently skips it.

The scheduler sweep at line 742 is the compensating control: it cancels jobs
whose dependencies can no longer be met, looping ten times so multi-level chains
cascade in one pass. **The concern is whether that sweep covers every case**, in
particular:

- a chain deeper than 10 levels in a single sweep (it would need multiple sweeps
  — probably fine, but unverified);
- a dependency in a *different organization* — the sweep's join requires
  `dep.organization_id = child.organization_id`, so a cross-org dependency row
  (which `job_dependencies` does not forbid; it has no `organization_id` column
  and only FKs to `jobs`) would make the child un-cancellable **and**
  un-runnable, forever;
- a dependency row pointing at a job that was **deleted** by the retention sweep
  — the `JOIN jobs dep` drops the row entirely, so `NOT EXISTS(... dep.status NOT
  IN (terminal))` is vacuously true for `'any'` mode (cancels, fine) but the
  `'all'` branch's `EXISTS(... dep.status IN (terminal))` is vacuously false
  (does not cancel), leaving the child stranded.

The last case looks like a genuine hole: `job_dependencies.depends_on_job_id` is
`REFERENCES jobs(id) ON DELETE CASCADE`, so retention deleting the parent job
deletes the dependency row as well — after which the child has *no* dependency
rows, `EXISTS (SELECT 1 FROM job_dependencies jd WHERE jd.job_id = child.id)` is
false, and the sweep skips it. Its `dependencies_met` is still `FALSE` from
before, so it never runs and never gets cancelled.

**How it fails in production (if confirmed).** A workflow job blocked on a
long-running parent sits `pending` with `dependencies_met = FALSE`. Retention
(free tier: 3 days) deletes the parent `jobs` row; the cascade removes the
dependency row. The child is now invisible to both the dequeue path and the
cancellation sweep. It counts against the org's `active_jobs` cap forever
(`get_org_resource_counts` counts `status IN ('pending','processing','scheduled')`),
so the org slowly loses quota to ghost jobs it cannot see a reason for and cannot
clear through the API.

**How to reproduce (do this first).** Against a scratch DB:
1. Create two jobs A and B in one org; insert `job_dependencies(job_id=B,
   depends_on_job_id=A)`; set `B.dependencies_met = FALSE`.
2. `DELETE FROM jobs WHERE id = A;` (simulating the retention sweep).
3. Confirm the dependency row cascaded away:
   `SELECT COUNT(*) FROM job_dependencies WHERE job_id = B;` → expect `0`.
4. Run one scheduler sweep (or execute the query at `src/scheduler/mod.rs:744`
   by hand) and assert B is **not** cancelled.
5. Attempt to dequeue from B's queue and assert B is not returned.
If steps 4 and 5 both hold, the finding is CONFIRMED. If the retention sweep
deletes dependent jobs too, or refuses to delete jobs with dependents, it is
INVALID — record that and why.

**Proposed fix (only after reproducing).** Add a clause to the sweep that cancels
jobs with `dependencies_met = FALSE` that have **no** `job_dependencies` rows and
no `parent_job_id` — such a job's blocker is unreachable by definition. Use a
grace period (e.g. `updated_at < NOW() - INTERVAL '1 hour'`) so a job in the
middle of having its dependencies inserted is never swept. Separately consider
whether retention should refuse to delete a job that still has dependents.

**How to verify the fix.** Re-run the reproduction; B should end `cancelled` with
`last_error` naming the unreachable dependency, and the org's `active_jobs` count
should drop by one.

**Blast radius.** The sweep gains a clause that can cancel jobs — get the grace
period right or it will cancel jobs mid-creation. Test the insert-then-wait
window explicitly.

---

#### `[ ]` B-17 — `job.cancelled` webhooks are dispatched before the cancelling transaction is durable

- **Severity:** LOW
- **Confidence:** CONFIRMED
- **Where:** [`src/scheduler/mod.rs:719-731`](src/scheduler/mod.rs#L719) and [`:789-801`](src/scheduler/mod.rs#L789)

**What is wrong.** Both cancellation sweeps use `UPDATE ... RETURNING` and then
immediately `spawn_dispatch` a `job.cancelled` webhook per returned row. The
statements execute on `&*self.db` (implicit transaction), so by the time
`fetch_all` returns the write *is* committed — this one is actually safe.

The real defect is narrower: the dispatch loop runs **inside** the ten-iteration
cascade loop, and the loop re-queries without any guard against re-selecting rows
it already cancelled. Re-selection cannot happen (the predicate requires
`status IN ('pending','scheduled')` and the rows are now `cancelled`), so no
duplicate fires today.

**Why it is still worth recording.** The safety of both properties depends on
implicit-transaction semantics and on the exact status predicate — neither is
stated anywhere near the code, and the loop's shape invites someone to add a
`RETURNING` column or relax the predicate and silently produce duplicate
webhooks. Webhook consumers are told deliveries are at-least-once, so a duplicate
is not a contract break, but a *storm* of them is an incident.

**Proposed fix.** No behaviour change required. Add a comment at the loop head
stating the two invariants that make it correct (rows leave the predicate once
cancelled; the statement is self-committing), so the next editor knows what they
must preserve. If B-04's bounded dispatcher lands, this loop becomes a natural
place to batch.

**How to verify the fix.** N/A — comment only. Confirm with a test that a
ten-level dependency chain produces exactly one `job.cancelled` per job.

**Blast radius.** None.

---

### 3.4 Authentication (`src/api/middleware/auth.rs`)

---

#### `[ ]` B-18 — API keys are accepted in the **query string** on every endpoint

- **Severity:** HIGH
- **Confidence:** CONFIRMED
- **Where:** [`src/api/middleware/auth.rs:97-123`](src/api/middleware/auth.rs#L97) — `authenticate_api_key()`

**What is wrong.** When the `Authorization` header is absent, the middleware
falls back to reading the credential out of the URL:

```rust
let query = request.uri().query().unwrap_or("");
for pair in query.split('&') {
    let mut parts = pair.splitn(2, '=');
    if let (Some(key), Some(value)) = (parts.next(), parts.next()) {
        if key == "token" || key == "api_key" { found_token = Some(value.to_string()); break; }
    }
}
```

This middleware is attached with `.route_layer(...)` to the **entire**
`protected_routes` router (`src/api/mod.rs:369`), so `?api_key=sp_live_…` works
on all ~60 protected endpoints, not only the ones that need it.

**Why it is wrong.** Query strings are logged and retained by everything in the
path in a way headers are not: the reverse proxy's access log, Cloudflare's
request log, any APM/tracing span that records `http.url`, browser history, and
the `Referer` header sent to third-party origins if a URL is ever rendered in a
page. This is CWE-598 (use of GET with sensitive query strings). A long-lived
credential — API keys here default to no expiry (`expires_at` is nullable) —
should never travel in a URL.

The fallback exists for a real reason: browser `EventSource` and `WebSocket`
cannot set an `Authorization` header, so `/ws` and `/events*` genuinely need it.
That justification does not extend to `POST /jobs` or `DELETE /api-keys/{id}`.

**How it fails in production.** Any of: (a) a support engineer pastes a failing
request URL (with `?api_key=`) into a ticket, leaking a live production key into
a third-party helpdesk; (b) Cloudflare/nginx access logs, which are retained and
often shipped to a log vendor, accumulate live credentials in plaintext — a log
export or a log-vendor breach becomes a full key compromise; (c) `TraceLayer`
(mounted at `src/api/mod.rs:137`) records the request URI in spans, so keys land
in the tracing backend. None of these produces an alert; the key stays valid
until someone rotates it, and nothing here would tell them to.

**Proposed fix.** Restrict the query-string fallback to the routes that cannot
send a header. Concretely: remove the fallback from `authenticate_api_key`, and
mount a second middleware (`authenticate_api_key_allow_query`) only on `/ws`,
`/events`, `/events/jobs/{id}`, and `/events/queues/{name}`. Additionally, for
those four routes, prefer a short-lived token: the realtime endpoints already
accept a JWT access token, so document that as the supported path and treat raw
API keys in the query string as deprecated with a removal date.

Independently, scrub the credential from logs regardless of route: add a
`MakeSpan` to `TraceLayer` that redacts `token`/`api_key` query parameters, so
the tracing backend never receives them even during the deprecation window.

**How to verify the fix.** `curl "$API/api/v1/jobs?api_key=$KEY"` → `401`
(previously `200`). `curl "$API/api/v1/events?api_key=$KEY"` → still streams.
Assert the emitted trace span's `http.url` contains `api_key=REDACTED`.

**Blast radius.** Any customer currently authenticating REST calls via
`?api_key=` breaks. Search production access logs for
`api_key=` / `token=` on non-realtime paths **before** shipping to size the
blast radius, and announce it. This is a breaking change and must be a minor
version bump with a `CHANGELOG.md` entry.

---

#### `[ ]` B-19 — Every authenticated request fires a `last_used` UPDATE, producing one row-version per request

- **Severity:** HIGH
- **Confidence:** CONFIRMED
- **Where:** [`src/api/middleware/auth.rs:282-290`](src/api/middleware/auth.rs#L282) — `authenticate_api_key_token()`

**What is wrong.** After every successful API-key authentication:

```rust
let db = state.db.pool_arc();
let key_id = api_key.id.clone();
tokio::spawn(async move {
    let _ = sqlx::query("UPDATE api_keys SET last_used = NOW() WHERE id = $1")
        .bind(&key_id).execute(&*db).await;
});
```

There is no throttle, no coalescing, and no sampling. It runs on the cache-hit
path too, so the Redis API-key cache — whose entire purpose is to keep
authentication off the database — does not prevent it.

**Why it is wrong.** In PostgreSQL an `UPDATE` is an insert-plus-tombstone:
every request creates a new row version of the *same* row. All requests using
one API key serialize on that row's lock, and the dead tuples must be vacuumed.
This converts a read-only authentication path into a write-amplified one, at
exactly 1 write per API request.

**How it fails in production.** A single customer driving 500 req/s through one
API key produces 500 updates/s to one row. Consequences, in the order they will
be noticed: (1) each request now waits on a row lock held by the previous one,
adding tail latency to *authentication* — the first thing every request does;
(2) `api_keys` bloats and autovacuum runs continuously on it; (3) the spawned
tasks are unbounded, so if the pool saturates they queue up and hold connections
that the actual request handlers need — a self-inflicted connection-pool
exhaustion under exactly the load where you least want it. This is the same
unbounded-`tokio::spawn` shape as B-04, on a hotter path.

**Proposed fix.** Make `last_used` coarse and cheap. Either:
- **Preferred:** write it at most once per key per interval (e.g. 5 minutes),
  gated by a Redis `SET NX EX 300` on `api_key_last_used:{id}` — one write per
  key per interval instead of per request, and no extra state in the process; or
- keep a process-local `HashMap<String, Instant>` of last-write times and skip
  if within the interval (simpler, but each replica writes independently).

Document that `last_used` has interval-level granularity — it is a
"has this key been used recently" field, and nothing needs it to be exact.

**How to verify the fix.** Drive 1 000 authenticated requests with one key inside
one interval and assert `api_keys`' `n_tup_upd` (from `pg_stat_user_tables`)
increased by 1, not 1 000. Confirm `last_used` still advances across intervals.

**Blast radius.** Any dashboard or admin view that shows "last used" now shows a
value up to the interval stale. That is acceptable and should be stated in the
API docs.

---

#### `[ ]` B-20 — Cached API keys bypass the org soft-delete check and keep a stale queue scope for up to 60 s

- **Severity:** MEDIUM
- **Confidence:** CONFIRMED
- **Where:** [`src/api/middleware/auth.rs:254-268`](src/api/middleware/auth.rs#L254)

**What is wrong.** On a Redis cache hit the middleware returns the cached record
verbatim, by design:

```rust
Ok(Some(cached)) => {
    // Cache hit - trust it! The cache key already includes token hash.
    // No need to re-verify bcrypt (saves 50-100ms per request).
    cached
}
```

The cached `ApiKeyRecord` carries `queues`, `is_active` and `expires_at`, all of
which are re-checked from the cached copy. Two things are checked **only** on the
DB path:

1. **Org soft-delete.** `fetch_api_key`'s query joins
   `INNER JOIN organizations o ... AND o.plan_tier <> 'deleted'`. A cache hit
   skips that join entirely, so a key belonging to a just-soft-deleted org keeps
   authenticating for the remainder of the TTL.
2. **Queue-scope narrowing.** `v0.1.93` deliberately changed the JWT path to read
   "CURRENT queues from DB" precisely because "a key that was later narrowed
   (e.g. `["*"]` -> `["emails"]`) [kept] its old wildcard scope". The API-key
   path re-introduces exactly that staleness through the cache, bounded by the
   TTL rather than by the token lifetime.

There *is* invalidation on revoke (`invalidate_api_key_cache`,
`src/api/handlers/api_keys.rs:294`, called from update and revoke), so an
explicit `PUT /api-keys/{id}` does flush the entry. Soft-deleting an org
(`src/api/handlers/admin.rs`) does not.

**Why it is wrong.** `API_KEY_CACHE_TTL_SECS = 60` was chosen (per its own doc
comment) to bound exactly this class of staleness, so the design intent is
"60 seconds is acceptable". The problem is that org soft-delete is a
**security/compliance** action — "this tenant is gone" — and it silently is not,
for a minute, on a path nobody documented.

**How it fails in production.** An operator soft-deletes an abusive or
non-paying org via `/admin/organizations/{id}`. For the next 60 seconds every
already-warm API key of that org continues to enqueue jobs, consume quota, and
read data. If the abuse is what prompted the deletion, that is a minute of
continued abuse *after* the operator was told the deletion succeeded.

**Proposed fix.** Two changes, one commit:
- In the org soft-delete path, invalidate every cached key for that org. The
  simplest correct approach: maintain a Redis set `org_api_keys:{org_id}`
  populated at cache-write time and delete all members on soft-delete. (Do not
  use `KEYS api_key:*` — `src/api/handlers/api_keys.rs:288` records that this
  exact wildcard bug was already fixed once.)
- Alternatively/additionally, store an `org_epoch` per organization in Redis,
  include it in the cache key, and bump it on soft-delete; that invalidates
  everything for the org in one write and cannot miss a key.

**How to verify the fix.** Authenticate with a key (warming the cache),
soft-delete the org, then immediately re-authenticate: expect `401` on the first
request, not after 60 s.

**Blast radius.** Adds a Redis write on the cache path. Confirm the extra key
does not blow the cache's memory budget for orgs with many keys (bounded by the
`api_keys` plan cap, so small).

---

#### `[ ]` B-21 — The API-key cache key is a 64-bit truncation of SHA-256 and a cache hit skips bcrypt entirely

- **Severity:** LOW
- **Confidence:** CONFIRMED
- **Where:** [`src/api/middleware/auth.rs:465-470`](src/api/middleware/auth.rs#L465) — `hash_for_lookup()`, used at `:256`

**What is wrong.** There are two lookup-hash functions with near-identical names
and different lengths:

| Function | Location | Output | Used for |
| --- | --- | --- | --- |
| `api_key_lookup_hash` | `src/models/api_key.rs:21` | full SHA-256, 64 hex chars | DB column `api_keys.lookup_hash` |
| `hash_for_lookup` | `src/api/middleware/auth.rs:465` | **first 16 hex chars** (64 bits) | Redis cache key |

The DB path treats its hash as non-authoritative and still runs
`bcrypt::verify` ("Defense in depth: verify bcrypt even though lookup_hash
matched, so a (practically impossible) SHA-256 collision can't authenticate").
The cache path explicitly does **not**: a hit is trusted without bcrypt.

So the cache's 64-bit truncated digest is the *sole* thing standing between a
presented token and an authenticated context.

**Why it is wrong.** A 64-bit second-preimage against a specific cached key is
~2⁶⁴ work — not a practical attack today, which is why this is LOW. But the
asymmetry is gratuitous: the full 64-char hash is already computed by the sibling
function, the cache-key length is irrelevant to performance, and the
defence-in-depth argument the DB path documents applies verbatim here. The
truncation buys nothing and removes a margin the codebase elsewhere insists on.

**How it fails in production.** Not exploitable at current parameters. It becomes
exploitable if anyone shortens the truncation further, or if the number of
simultaneously cached keys grows enormously (multi-target attacks scale as
2⁶⁴/T). Treat it as a latent margin problem, not an incident.

**Proposed fix.** Use `crate::models::api_key_lookup_hash(token)` (the full
digest) for the cache key too, and delete `hash_for_lookup`. Optionally also
re-verify bcrypt on cache hit — but measure first; that would undo the 50–100 ms
saving the cache exists for, and with a full-length digest it is arguably
unnecessary.

Note the cache-key change invalidates every existing entry on deploy — harmless
(a cold cache re-populates), but expect a brief authentication-latency spike.

**How to verify the fix.** Unit test asserting the cache key is 64 hex chars.
Confirm `hash_for_lookup` has no remaining callers before deleting it, including
the existing test at `src/api/middleware/auth.rs:525`.

**Blast radius.** One deploy-time cold-cache spike. Nothing else reads these
keys.

---

### 3.5 Rate limiting (`src/api/middleware/plan_rate_limit.rs`, `rate_limit.rs`)

---

#### `[ ]` B-22 — Public and admin rate limits key on a client-controlled `X-Forwarded-For`, so they are trivially bypassed

- **Severity:** HIGH
- **Confidence:** CONFIRMED (code); **verify the proxy topology** before choosing the fix
- **Where:** [`src/api/middleware/plan_rate_limit.rs:64-95`](src/api/middleware/plan_rate_limit.rs#L64) — `extract_client_id()`, used by `plan_rate_limit_middleware` (public routes, `:246`) and `admin_rate_limit_middleware` (`:302`)

**What is wrong.** The client identity for unauthenticated requests is the
**leftmost** entry of `X-Forwarded-For`:

```rust
if let Some(forwarded) = request.headers().get("X-Forwarded-For")... {
    if let Some(ip) = forwarded.split(',').next() {      // <-- leftmost
        ...
        return format!("ip:{}", trimmed);
    }
}
```

`X-Forwarded-For` is a list that each proxy **appends** to. The leftmost entry is
therefore whatever the *original client* sent — fully attacker-controlled. The
genuine client IP, added by the edge proxy, is at the right-hand end.

**Why it is wrong.** This limiter is the only brute-force control on the public
routes, which include (`src/api/mod.rs:170-206`):

- `POST /auth/login`
- `POST /auth/email/start` and `POST /auth/email/verify` — the passwordless flow, which verifies a short numeric code
- `POST /organizations` — org creation
- `POST /billing/webhook`

and, separately, `admin_rate_limit_middleware` guards the `X-Admin-Key` routes at
60 requests/minute.

An attacker sends `X-Forwarded-For: 1.2.3.<random>` with each request and lands
in a fresh Redis bucket every time. The rate limit is not degraded — it is
absent.

**How it fails in production.** The email-login code is the sharpest case: an
attacker who knows a target's email address calls `/auth/email/start` once, then
brute-forces `/auth/email/verify` with a rotating spoofed `X-Forwarded-For`.
Whether that succeeds depends on the code's entropy and on any per-identifier
attempt counter in `src/api/handlers/email_login.rs` — **see B-25, which resolves
this dependency.** The same technique removes the 60/min cushion in front of
`X-Admin-Key`.

**Proposed fix.** Do not trust `X-Forwarded-For` from the client. In order of
preference:
1. Prefer the edge's own header. Production fronts the API with Cloudflare, which
   sets `CF-Connecting-IP` to the true client IP and cannot be spoofed past the
   edge. Read that first, and only fall back to XFF.
2. Where XFF must be used, parse it **right to left**, skipping a configured
   number of trusted proxy hops (`TRUSTED_PROXY_HOPS`), and take the first
   untrusted entry — never the leftmost.
3. Ignore XFF entirely when the connection's peer address is not in a configured
   trusted-proxy CIDR list.

Apply the same function to both middlewares; they share `extract_client_id`, so
this is one fix in one place. The dead module `rate_limit.rs` has the identical
defect at line 129 — see B-23.

**How to verify the fix.** With the fix deployed, send 200 requests to
`/auth/email/start` from one host, each with a different spoofed
`X-Forwarded-For`, and assert `429` once the real limit is reached. Before the
fix all 200 succeed. Add this as a regression test.

**Blast radius.** If the deployment ever runs *without* a trusted edge proxy
(self-host, docker-compose, local dev), `CF-Connecting-IP` is absent and the
fallback must still work — otherwise every self-hosted user is bucketed together
under one identity. Make the trusted-proxy behaviour configurable and default it
to "no trusted proxy" for self-host.

---

#### `[ ]` B-23 — `src/api/middleware/rate_limit.rs` is dead code whose comments claim protections it does not provide

- **Severity:** LOW
- **Confidence:** CONFIRMED
- **Where:** whole file [`src/api/middleware/rate_limit.rs`](src/api/middleware/rate_limit.rs); exported at [`src/api/middleware/mod.rs:22`](src/api/middleware/mod.rs#L22)

**What is wrong.** `rate_limit_middleware` is `pub`, re-exported, and **never
mounted** — `src/api/mod.rs` only ever attaches
`plan_rate_limit::plan_rate_limit_middleware` and
`plan_rate_limit::admin_rate_limit_middleware`. Verify with:

```bash
grep -rn "rate_limit_middleware" --include="*.rs" src/ | grep -v plan_rate_limit
```

which returns only the definition and the re-export.

The file is worse than merely unused, because its comments assert guarantees the
code does not implement:

```rust
/// No longer fails open when Redis is unavailable.
/// Uses a stricter fallback limit to prevent DoS attacks.
```

Every failure branch in `check_rate_limit` returns `Ok(RateLimitInfo { ... })` —
including "no Redis configured" (line 188), "connection failed" (line 205) and
"pipeline error" (line 249). `Ok` means **allow**. The "strict fallback limit" is
only ever reflected in the response *headers*; no counter enforces it. The module
fails open in every degraded case while stating the opposite.

The live module (`plan_rate_limit.rs`) handles this correctly via
`settings.rate_limit.fail_closed_on_redis_error`.

**How it fails in production.** It does not — it is not mounted. The risk is
purely that someone mounts it (it is exported and named exactly like the thing
you would reach for), trusts the comment, and ships a rate limiter that
disappears whenever Redis hiccups.

**Proposed fix.** Delete the module and its re-export. If any of it is worth
keeping, move the useful parts into `plan_rate_limit.rs`. Before deleting, check
for references: `grep -rn "RateLimitState\|rate_limit_middleware" tests/ src/`.

**How to verify the fix.** `cargo build` and `cargo clippy` clean; no new
`unused_imports` warnings in `src/api/middleware/mod.rs`.

**Blast radius.** None if genuinely unreferenced. Confirm first — it is a `pub`
item re-exported from `src/lib.rs`'s module tree.

---

#### `[ ]` B-24 — `/metrics` may be unauthenticated depending on deployment config

- **Severity:** MEDIUM
- **Confidence:** SUSPECTED — **check the production environment before acting**
- **Where:** route [`src/api/mod.rs:156`](src/api/mod.rs#L156); handler [`src/api/handlers/metrics.rs`](src/api/handlers/metrics.rs)

**What is wrong.** `/metrics` is registered on the root router, *outside* the
`/api/v1` nest, so neither the authentication middleware nor either rate limiter
applies to it. Its only protection is inside the handler:

```rust
if let Some(ref expected_token) = state.settings.server.metrics_token {
    // ... constant-time Bearer comparison ...
}
```

When `METRICS_TOKEN` is unset, the guard is skipped entirely and the endpoint is
world-readable.

**Why it might be wrong.** Whether this matters depends on two facts this audit
did **not** verify:
1. Is `METRICS_TOKEN` set in the production environment?
2. Do any metrics carry per-organization labels (org id, queue name)?

If (1) is no and (2) is yes, the endpoint publishes per-tenant operational data
(queue names, job volumes, error rates) to anyone who asks. If (2) is no, the
exposure is limited to aggregate platform metrics — still useful reconnaissance
(fleet size, throughput, error trends) but not tenant data.

**How to check (do this first).**
```bash
curl -sS -o /dev/null -w '%{http_code}\n' https://api.spooled.cloud/metrics
```
`200` means unauthenticated. Then inspect the body for label keys:
```bash
curl -sS https://api.spooled.cloud/metrics | grep -oE '\{[^}]*\}' | sort -u | head
```
Look for `organization_id`, `org`, or `queue` labels. Record the result in this
document either way.

**Proposed fix (if unauthenticated).** Set `METRICS_TOKEN` in the production
environment — the mechanism already exists and needs no code change. Belt and
braces: bind the metrics server to a private interface (there is a dedicated
metrics port, see `src/main.rs:101`) and stop routing `/metrics` on the public
listener at all. If per-org labels exist, treat their removal as a separate
decision — high-cardinality per-tenant labels are also a Prometheus cost problem,
not just an exposure one.

**How to verify the fix.** The curl above returns `401`. Prometheus scraping
still works with its configured bearer token.

**Blast radius.** If a scraper is currently configured without a token, it stops
working the moment the token is set. Coordinate with whoever owns the monitoring
stack.

---

### 3.6 Passwordless email login (`src/api/handlers/email_login.rs`)

**Context that limits B-22.** The per-code brute-force cap here is implemented
correctly and atomically:

```rust
// verify(), lines 365-372
UPDATE email_login_codes SET attempts = attempts + 1
WHERE id = $1 AND attempts < $2 RETURNING attempts
```

with `MAX_CODE_ATTEMPTS = 5` and `MAX_CODES_PER_HOUR = 5` (per email). So even
with the IP limiter fully bypassed via B-22, an attacker gets at most ~25 guesses
per hour against a 10⁶ code space. **That is why B-22 is HIGH and not CRITICAL.**
Do not weaken either constant when fixing anything below.

---

#### `[ ]` B-25 — Two different client-IP extractors exist; the security-critical one is the spoofable one

- **Severity:** MEDIUM (this is the concrete fix path for B-22)
- **Confidence:** CONFIRMED
- **Where:** correct implementation at [`src/api/handlers/email_login.rs:145-170`](src/api/handlers/email_login.rs#L145) — `extract_client_ip()`; spoofable implementation at [`src/api/middleware/plan_rate_limit.rs:64`](src/api/middleware/plan_rate_limit.rs#L64) — `extract_client_id()`

**What is wrong.** The codebase already contains a correct client-IP extractor.
`email_login.rs` prefers the edge-set headers before falling back to XFF:

```rust
// CF-Connecting-IP (Cloudflare)  -> trusted, set by the edge
// X-Real-IP                      -> trusted, set by nginx
// X-Forwarded-For (first IP)     -> only as a last resort
```

The middleware used by the *actual* rate limiters does not — it goes straight to
the leftmost `X-Forwarded-For` (B-22).

**Why it is wrong.** This is not a missing-knowledge problem, it is a
duplication problem: two functions with the same job, different behaviour, and
no shared definition. The one used by the security control is the weaker one.
Whichever gets fixed, the other will drift again.

**How it fails in production.** Exactly as B-22 describes — and the presence of
the correct version makes the failure worse, because a reviewer grepping for
`CF-Connecting-IP` finds a hit and concludes the codebase handles this properly.

**Proposed fix.** Extract one shared `client_ip(&HeaderMap) -> String` into a
common module (e.g. `src/api/middleware/mod.rs` or `src/security/`), implement
the trusted-header ordering from `email_login.rs` plus the trusted-proxy
handling described in B-22, and call it from **all four** sites:
`plan_rate_limit::extract_client_id`, `admin_rate_limit_middleware`,
`email_login::extract_client_ip`, and `rate_limit::extract_client_id` (or delete
the last one per B-23).

**How to verify the fix.** One unit test table covering: CF header present
(wins), only `X-Real-IP` (wins), only XFF with a spoofed leftmost entry
(rejected/ignored per the trusted-proxy config), and no headers at all (stable
fallback, not a random per-request value).

**Blast radius.** Changes bucket identity for existing clients, so live rate-limit
counters reset on deploy. Harmless, but expect a one-off step in the limiter
metrics.

---

#### `[ ]` B-26 — Anyone can permanently lock a known email out of login

- **Severity:** MEDIUM
- **Confidence:** CONFIRMED
- **Where:** [`src/api/handlers/email_login.rs:334-347`](src/api/handlers/email_login.rs#L334) — `verify()` selection, combined with `MAX_CODES_PER_HOUR` at [`:26`](src/api/handlers/email_login.rs#L26) and its enforcement at [`:208`](src/api/handlers/email_login.rs#L208)

**What is wrong.** Two behaviours combine into an account-level denial of
service:

1. `verify()` only ever considers the **newest** unused code for an email:
   ```sql
   WHERE email = $1 AND expires_at > NOW() AND used_at IS NULL AND code NOT LIKE 'signup:%'
   ORDER BY created_at DESC
   LIMIT 1
   ```
   Any older code is unreachable, even though it is unexpired and unused.
2. `start()` caps code issuance at `MAX_CODES_PER_HOUR = 5` **per email**, and
   that cap is not scoped to the requester — anyone who knows the address can
   consume it.

**Why it is wrong.** Requesting a login code is an unauthenticated action keyed
solely on someone else's email address. Because a new code silently invalidates
the previous one *for verification purposes*, an attacker's request destroys the
victim's in-flight code.

**How it fails in production.** Victim clicks "email me a code" and opens their
inbox. Attacker (who only needs the victim's email address, and who is not
rate-limited per B-22) calls `POST /auth/email/start` for that address. The
victim types the code they received — `verify()` selects the attacker-triggered
newer code, `constant_time_compare` fails, and the victim sees "Invalid code".
The victim retries; the attacker repeats. After five issuances in the hour,
`start()` returns "Too many login attempts. Please try again later." and the
victim cannot obtain a working code **for the rest of the hour**. Repeat hourly
for indefinite lockout. Nothing in the logs distinguishes this from a confused
user, and the victim has no self-service remedy.

**Proposed fix.** Break the coupling — accept any unexpired, unused code rather
than only the newest:
- Change `verify()` to select **all** candidate rows for the email (bounded, e.g.
  the newest 5, still excluding `signup:%`) and constant-time-compare against
  each, consuming an attempt from the row that matches; if none matches, consume
  an attempt from the newest. This keeps the per-code cap intact — the attacker
  cannot gain guesses — while making a legitimately-delivered code work even if a
  newer one was issued afterwards.
- Additionally consider making `start()` idempotent within a short window: if an
  unexpired unused code already exists for the email and was issued less than
  ~60 s ago, re-send that same code instead of minting a new one. This removes
  the attacker's ability to churn codes at all.

Do **not** fix this by raising `MAX_CODES_PER_HOUR`; that weakens the brute-force
bound that B-22's severity assessment depends on.

**How to verify the fix.** Test: request code A; request code B for the same
email; assert verifying with A succeeds. Then assert that five wrong guesses
against A still lock A specifically, and that the total number of guesses
available across A and B has not increased beyond `MAX_CODES_PER_HOUR ×
MAX_CODE_ATTEMPTS`.

**Blast radius.** `verify()` does more comparisons per call (bounded, cheap). The
"newest code wins" behaviour is what users are used to from other services;
document the change.

---

#### `[ ]` B-27 — The public `check-email` endpoint writes a database row per request, into the table it also scans to rate-limit itself

- **Severity:** MEDIUM
- **Confidence:** CONFIRMED
- **Where:** [`src/api/handlers/email_login.rs:53-142`](src/api/handlers/email_login.rs#L53) — `check_email()`

**What is wrong.** This unauthenticated `GET` does three DB operations per call:

```rust
// 1. count rows in the last minute
SELECT COUNT(*) FROM email_login_codes WHERE email LIKE 'check:%' AND code = $1 AND created_at > NOW() - INTERVAL '1 minute'
// 2. count rows in the last hour  (same shape)
// 3. insert a marker row
INSERT INTO email_login_codes (email, code, expires_at, used_at) VALUES ($1, $2, NOW() + INTERVAL '1 hour', NOW())
//   with email = format!("check:{}", email)  and  code = client_ip
```

So the rate limiter is implemented by *writing to* and *scanning* the same table,
using the `email` column to hold `check:<email>` and the `code` column to hold an
IP address.

**Why it is wrong.** Three compounding problems:

1. **Write amplification on an unauthenticated endpoint.** Every request — including
   rejected ones? no, rejected ones return before the insert, but every *allowed*
   one — inserts a row. There is no cleanup path: `grep -rn "check:" src/` shows
   the prefix is only ever written and counted, never deleted, and the scheduler's
   retention sweep covers `jobs`/history, not `email_login_codes`.
2. **The rate-limit scan degrades as the table grows.** `email LIKE 'check:%'` is
   not an equality predicate, so unless a supporting index exists it is a scan,
   and it runs **twice per request** against a table this endpoint is
   monotonically growing. The endpoint gets slower the more it is used — the
   worst possible shape for a DoS-facing path.
3. **Column semantics are overloaded.** `code` holds an IP; `email` holds a
   prefixed key; `used_at` is pre-set to `NOW()` purely to keep these rows out of
   the login queries. `verify()` already needed a defensive `AND code NOT LIKE
   'signup:%'` filter (line 344) for the same reason, and its comment documents
   that a previous version of this overloading *did* break login. That is a
   near-miss that will recur.

**How it fails in production.** A scripted signup-form scraper (or just an
attacker probing for registered addresses) sends `check-email` requests from a
pool of IPs. Each allowed request permanently adds a row. After a few million
rows the two `COUNT(*)` scans dominate the request, `email_login_codes` bloats,
and the *login* path — which queries the same table on every `verify()` — slows
down with it. The blast radius of abusing an email-availability check is
therefore the whole authentication system.

**Proposed fix.** Move this rate limiter to Redis, where every other limiter in
this codebase already lives:
- Replace the two counts + insert with `cache.check_rate_limit(&format!("check_email:{}", ip), MAX_EMAIL_CHECKS_PER_MINUTE, 60)`
  and an hourly equivalent — the helper already exists
  (`src/cache/mod.rs`, used by `plan_rate_limit.rs`).
- Keep a fail-closed/fail-open decision consistent with
  `settings.rate_limit.fail_closed_on_redis_error`.
- Add a one-off migration (or a scheduler sweep clause) deleting existing
  `email LIKE 'check:%'` rows, and add `email_login_codes` to the retention sweep
  generally so expired login codes do not accumulate either.
- Use the shared IP extractor from B-25.

**How to verify the fix.** Assert that N `check-email` calls add zero rows to
`email_login_codes`, that the 11th call within a minute returns `429`, and that
`SELECT COUNT(*) FROM email_login_codes WHERE email LIKE 'check:%'` is 0 after
the cleanup migration.

**Blast radius.** If Redis is unavailable, this endpoint's limiter changes
behaviour — pick fail-open (matching the current self-host default) so signup
does not break in self-hosted deployments without Redis.

---

#### `[ ]` B-28 — `check-email` is an account-enumeration oracle by design

- **Severity:** LOW (accepted-risk; record the decision explicitly)
- **Confidence:** CONFIRMED
- **Where:** [`src/api/handlers/email_login.rs:138-141`](src/api/handlers/email_login.rs#L138)

**What is wrong.** The endpoint answers, for any address, whether an account
exists:

```rust
Ok(Json(CheckEmailResponse { available: exists.is_none(), exists: exists.is_some() }))
```

**Why it is (arguably) fine.** Signup forms need this to show "that email is
already registered", and the handler's own doc comment shows the risk was
considered ("Rate limited to prevent email enumeration attacks"). Most consumer
services make the same trade.

**Why it still belongs in this document.** The mitigation is the rate limiter,
and that rate limiter is the one B-27 shows to be structurally weak and B-22/B-25
show to be IP-spoofable. The accepted risk was accepted on the assumption that
the limiter works. Once B-22/B-25/B-27 are fixed, this returns to genuinely
accepted-risk status; until then, enumeration is effectively unlimited.

There is also a smaller detail: `CheckEmailRequest` carries no `#[validate(email)]`
(compare `StartEmailLoginRequest` at line 175), so arbitrary strings reach the
query and — today — get written into `email_login_codes` by B-27's insert.

**Proposed fix.** No change to the endpoint's contract. Do add
`#[validate(email)]` to `CheckEmailRequest` and run it, so non-addresses are
rejected before touching the database. Then record in this file that enumeration
via `check-email` is a **deliberate** trade-off, so the next auditor does not
re-raise it.

**How to verify the fix.** `GET /auth/check-email?email=notanemail` → `400`.
Valid addresses behave as before.

**Blast radius.** Any frontend calling this with a partial address as the user
types will start getting `400`s. Check `spooled-frontend`'s signup flow before
shipping — debounced "as you type" validation is a common pattern and would
break.

---

### 3.7 Quota accounting (`src/api/handlers/jobs.rs`, and every indirect enqueue path)

---

#### `[ ]` B-29 — The bulk-enqueue quota **refund** can drive counters negative, granting free quota across a day boundary

- **Severity:** HIGH
- **Confidence:** CONFIRMED
- **Where:** [`src/api/handlers/jobs.rs:1266-1281`](src/api/handlers/jobs.rs#L1266) — `bulk_enqueue()`; SQL function at [`migrations/20241209000011_organization_usage.sql:40`](migrations/20241209000011_organization_usage.sql#L40)

**What is wrong.** `bulk_enqueue` reserves quota for the whole batch up front and
then refunds the unused portion by calling the increment function with a
**negative** count:

```rust
let unused = job_count as i64 - successful_count as i64;
if unused > 0 {
    increment_daily_jobs(state.db.pool(), &org_id, -(unused as i32)).await
}
```

`increment_daily_jobs` was never written to accept negatives. It has no
lower clamp on any counter, and — critically — it contains a daily/monthly reset
branch that **assigns** rather than adds:

```sql
jobs_created_today = CASE
    WHEN organization_usage.last_daily_reset < CURRENT_DATE
    THEN p_count                                   -- <-- assignment, not addition
    ELSE organization_usage.jobs_created_today + p_count
END,
...
total_jobs_created = organization_usage.total_jobs_created + p_count,  -- <-- always adds
```

**Why it is wrong.** Two independent defects follow:

1. **Day-boundary sign flip.** If the reservation happens before midnight and the
   refund lands after it, the reset branch is taken and `jobs_created_today` is
   set to `p_count` — a *negative* number. The org's daily usage is now below
   zero, so the effective daily cap for that day is `limit + unused`.
2. **Monotonic counters are decremented unconditionally.** `total_jobs_created`
   is a lifetime total and `jobs_created_month` is a monthly total; both take
   `+ p_count` with no reset guard, so every refund reduces them. `total_jobs_created`
   is surfaced to operators at `src/api/handlers/admin.rs:302`, so the admin view
   of "jobs this org has ever created" drifts downward over time and can go
   negative.

**How it fails in production.** Deliberately: an org submits a bulk batch of
1 000 items whose idempotency keys already exist, timed so the insert completes
just after midnight. `successful_count = 0`, `unused = 1000`, the refund lands in
the new day, and `jobs_created_today` becomes `-1000` — 1 000 free jobs on top of
the plan's daily cap, repeatable every night. Accidentally: any large bulk
request that straddles midnight does the same thing without anyone trying, and
`total_jobs_created` is corrupted by every duplicate-heavy batch regardless of
time of day.

Note the fallback branch (line 1284) has the mirror-image issue in a benign
direction — it credits `successful_count` with the same reset semantics.

**Proposed fix.** Do not reuse an increment function for refunds. Add a dedicated,
sign-safe SQL function, e.g.:

```sql
CREATE OR REPLACE FUNCTION refund_daily_jobs(p_org_id TEXT, p_count INT)
RETURNS BIGINT AS $$
  -- p_count is a POSITIVE number of credits to return.
  -- Never crosses a reset boundary: if last_daily_reset <> CURRENT_DATE the
  -- reservation belonged to a previous day and there is nothing to refund today.
  UPDATE organization_usage SET
      jobs_created_today = CASE WHEN last_daily_reset = CURRENT_DATE
                                THEN GREATEST(0, jobs_created_today - p_count)
                                ELSE jobs_created_today END,
      jobs_created_month = CASE WHEN last_monthly_reset = DATE_TRUNC('month', CURRENT_DATE)::DATE
                                THEN GREATEST(0, jobs_created_month - p_count)
                                ELSE jobs_created_month END,
      updated_at = CURRENT_TIMESTAMP
  WHERE organization_id = p_org_id
  RETURNING jobs_created_today;
$$ LANGUAGE sql;
```

Key properties to preserve: `GREATEST(0, …)` floors every counter; the reset
columns are **compared, not written**, so a refund can never trigger a reset; and
`total_jobs_created` is left alone entirely because it is a lifetime total that
should only ever grow.

Then change `jobs.rs:1273` to call `refund_daily_jobs(pool, &org_id, unused)`.
Add a `CHECK (jobs_created_today >= 0 AND jobs_created_month >= 0 AND
total_jobs_created >= 0)` constraint in the same migration so a future
regression fails loudly instead of silently granting quota.

**How to verify the fix.** Unit-level: call the reservation with 100, refund 100,
assert all three counters return to their starting values and none is negative.
Boundary: set `last_daily_reset` to yesterday, refund 100, assert
`jobs_created_today` is unchanged (not `-100`). Then repair existing data:
`UPDATE organization_usage SET jobs_created_today = GREATEST(0, jobs_created_today),
jobs_created_month = GREATEST(0, jobs_created_month), total_jobs_created =
GREATEST(0, total_jobs_created);` — run a `SELECT` first to see how much drift
already exists and record the number here.

**Blast radius.** `total_jobs_created` stops decreasing, so admin stats will jump
relative to their previously-corrupted values. Note that in the changelog so it
is not mistaken for a new bug.

---

#### `[ ]` B-30 — Four indirect enqueue paths still use the pre-`v0.1.91` non-atomic quota pattern

- **Severity:** MEDIUM
- **Confidence:** CONFIRMED
- **Where:** [`src/api/handlers/schedules.rs:563,634`](src/api/handlers/schedules.rs#L563), [`src/api/handlers/webhooks.rs:133,186`](src/api/handlers/webhooks.rs#L133), [`src/api/handlers/workflows.rs:296,464`](src/api/handlers/workflows.rs#L296), [`src/scheduler/mod.rs:427,566`](src/scheduler/mod.rs#L427)

**What is wrong.** `v0.1.91` introduced `try_increment_daily_jobs` — a single
row-locked check-and-increment — and adopted it in the two direct enqueue paths:

| Path | Quota call | Atomic? |
| --- | --- | --- |
| `POST /jobs` | `jobs.rs:306` `try_increment_daily_jobs` | yes |
| `POST /jobs/bulk` | `jobs.rs:1114` `try_increment_daily_jobs` | yes |
| gRPC `Enqueue` | `queue_service.rs:467` `try_increment_daily_jobs` | yes |
| `POST /schedules/{id}/trigger` | `check_job_limits` … then `increment_daily_jobs` | **no** |
| `POST /webhooks/{org_id}/custom` | `check_job_limits` … then `increment_daily_jobs` | **no** |
| `POST /workflows` | `check_job_limits` … then `increment_daily_jobs` | **no** |
| cron scheduler tick | `check_job_limits_generic` … then `increment_daily_jobs` | **no** |

The four "no" rows use exactly the read-then-write pattern the release notes
describe as fixed: `check_job_limits` reads the current count, the job is
inserted, and `increment_daily_jobs` then bumps the counter unconditionally —
it has no cap parameter and cannot reject.

**Why it is wrong.** Between the read and the write, any number of concurrent
requests can each observe a count below the cap. All of them then insert and all
of them increment. The cap is enforced only against a stale snapshot.

**How it fails in production.** The custom-webhook route is the sharpest case: it
is a public endpoint (`src/api/mod.rs:184`, authenticated by a per-org webhook
signature, not an API key) so an attacker holding a leaked webhook token — or
simply a busy upstream like Stripe or GitHub replaying events — can fire N
concurrent requests. With a `free`-tier cap of 1 000/day, 200 concurrent requests
issued when the count is at 999 all pass the check and all insert: the org lands
at ~1 199 jobs on a 1 000 cap. The overshoot is proportional to concurrency, and
because `increment_daily_jobs` cannot reject, nothing self-corrects.

`POST /workflows` amplifies it differently: one request enqueues `job_count`
jobs, so a handful of concurrent workflow creations can overshoot by hundreds.

**Proposed fix.** Port each of the four to the atomic pattern already used by
`jobs.rs:303-335`:

```rust
let limit = daily_jobs_limit(pool, &org_id).await.unwrap_or(-1);
match try_increment_daily_jobs(pool, &org_id, n, limit).await {
    Ok(c) if c >= 0 => { /* proceed */ }
    Ok(_)          => { /* over cap: reject 429 QUOTA_EXCEEDED, undo any insert */ }
    Err(e)         => { /* counter unavailable: proceed best-effort, warn */ }
}
```

Two ordering details matter and are worth stating for whoever implements this:
- **Reserve before inserting**, then refund on failure (via B-29's
  `refund_daily_jobs`), rather than insert-then-increment. `jobs.rs` currently
  inserts first and deletes the job on over-cap (line 315), which works for a
  single row but does not generalise to the multi-job workflow path.
- The schedule-trigger path must keep returning `429` with
  `code: "QUOTA_EXCEEDED"` — `v0.1.91` specifically changed it from `403`, so do
  not regress that status.

**How to verify the fix.** For each path, fire N concurrent requests with the
org's count set to `cap - 1` and assert exactly one succeeds and the rest return
`429 QUOTA_EXCEEDED`. The existing single-enqueue concurrency test can be used as
a template — find it with `grep -rn "QUOTA_EXCEEDED" tests/`.

**Blast radius.** Four handlers gain a failure mode they did not have. The cron
scheduler in particular must degrade sanely: a schedule that cannot fire because
the org is over quota should log and skip (the `v0.1.80` "cron skip-with-log"
policy), not crash the sweep or silently drop the schedule's `next_run`.

---

### 3.8 SSRF protection (`src/security/url_validator.rs`, `src/security/http_client.rs`)

---

#### `[ ]` B-31 — DNS-rebinding protection is claimed but not implemented: validation and connection resolve independently

- **Severity:** HIGH
- **Confidence:** CONFIRMED
- **Where:** [`src/security/url_validator.rs:424-465`](src/security/url_validator.rs#L424) — `validate_dns_resolution()`; consumed by [`src/security/http_client.rs:28`](src/security/http_client.rs#L28) — `build_outbound_http_client()`

**What is wrong.** The validator resolves the hostname and checks every returned
IP against the blocklist:

```rust
/// This prevents DNS rebinding attacks where a hostname initially resolves to a public IP
/// but later resolves to an internal IP.
fn validate_dns_resolution(...) {
    let resolved: Vec<_> = addr.to_socket_addrs()...;
    for socket_addr in &resolved { validate_ip_address(&socket_addr.ip(), options)?; }
}
```

The HTTP request is then made by a `reqwest::Client` that is given the **URL**,
not the validated IP:

```rust
let response = request_builder.body(payload_json).send().await;   // service.rs:240
```

`reqwest` performs its own, independent DNS resolution when it opens the
connection. The two lookups are separate events against a hostname the attacker
controls. Nothing carries the validated IP from the check to the connect.

**Why it is wrong.** This is the textbook TOCTOU that the function's own doc
comment claims to prevent. Validating a name and then connecting by name is not
rebinding protection — protection requires *pinning*: connect to the exact IP
that was validated.

The redirect policy has the same shape at a second level: it validates
`attempt.url()` and then lets `reqwest` resolve it again on the follow.

**How it fails in production.** An attacker registers a webhook URL at a
hostname they control with a 1-second TTL. The first lookup (validation) answers
with a public IP and passes. The second lookup (connect, milliseconds later)
answers `169.254.169.254`, or a private RFC1918 address inside the deployment's
network. The webhook dispatcher then issues an authenticated-from-inside HTTP
POST to that address and — because `try_deliver` records the response status and
`test()` returns `status_code` and `response_time_ms` to the caller — the
attacker gets an oracle for what is reachable internally. On a cloud VM the
metadata endpoint is the prize; on this deployment (Docker on a VPS) the prize is
every service on the internal Docker network and on `127.0.0.1`, including
Postgres and Redis.

The attack requires no special timing skill: the attacker's own DNS server simply
alternates answers, and the webhook is retried up to `max_attempts` times per
event (B-04), giving many chances per event.

**Proposed fix.** Pin the validated address. Two workable approaches:

1. **Resolve-then-pin (simplest).** Have the validator *return* the validated
   `SocketAddr`s, then build the request with
   `Client::builder().resolve(host, validated_addr)` — or use
   `reqwest::ClientBuilder::resolve_to_addrs` per request — so the connection
   goes to exactly the IP that passed. The `Host` header and TLS SNI stay the
   hostname, so virtual hosting and certificate validation still work.
2. **Custom DNS resolver (most robust).** Supply a
   `reqwest::dns::Resolve` implementation that runs `validate_ip_address` on
   every address it is about to return, for **every** lookup the client makes —
   including redirects. This closes the redirect leg automatically and removes
   the need to thread addresses through the call sites.

Approach 2 is preferable because it also fixes B-32 (it can be async) and covers
the redirect path without a second mechanism. Whichever is chosen, update the doc
comment so it describes what the code actually guarantees.

**How to verify the fix.** Stand up a DNS server that alternates between a public
IP and `127.0.0.1` for a test hostname. Register a webhook at that hostname and
dispatch repeatedly; assert that no request ever reaches the local listener, and
that the failure is logged as an SSRF block rather than a connection error.
Without the fix the local listener receives requests.

**Blast radius.** Pinning changes connection behaviour for every outbound webhook.
Watch for: hosts behind round-robin DNS or anycast where pinning one address
reduces availability (mitigate by pinning *all* validated addresses, not just the
first), and hosts whose TLS certificate is validated against SNI (unaffected by
pinning, but verify with a real HTTPS endpoint in the test).

---

#### `[ ]` B-32 — SSRF validation performs **blocking** DNS resolution on the async runtime

- **Severity:** MEDIUM
- **Confidence:** CONFIRMED
- **Where:** [`src/security/url_validator.rs:438`](src/security/url_validator.rs#L438) — `addr.to_socket_addrs()`

**What is wrong.** `std::net::ToSocketAddrs::to_socket_addrs` is a **synchronous,
blocking** call — it calls the platform resolver and parks the OS thread until it
returns. It is invoked from:

- `OutgoingWebhookService::try_deliver` → `validate_url` (per delivery attempt),
- `outgoing_webhooks::create` / `update` / `test` (per API request),
- `deliver_completion_webhook` (per completed job with a completion webhook),
- and, worst, from inside the **redirect-policy closure** in
  `build_outbound_http_client`, which `reqwest` invokes on a runtime thread while
  driving the connection.

All of these are `async fn`s running on the Tokio runtime. None uses
`spawn_blocking`.

**Why it is wrong.** A blocking call on a runtime worker thread stalls every
other task scheduled on that thread. DNS is exactly the operation you must not
block on: a hostile or dead nameserver can hold the call for the resolver's
full timeout (commonly 5 s per attempt, several attempts), and the *attacker
supplies the hostname*.

**How it fails in production.** An attacker registers webhooks pointing at
hostnames served by a nameserver that accepts queries and never answers. Each
delivery attempt parks a Tokio worker thread for the resolver timeout. Combined
with B-04's unbounded delivery fan-out, a few hundred such deliveries park every
worker thread in the runtime — at which point the API stops serving *all*
requests, not just webhooks. This is a cheap, remote, unauthenticated-adjacent
(any org can register a webhook) denial of service against the whole backend.

**Proposed fix.** Make resolution async. Preferred: adopt B-31's approach 2 (a
custom `reqwest::dns::Resolve`), which is async by construction and removes the
blocking call from both the validation path and the redirect path. If validation
must stay synchronous for now, wrap the resolution in
`tokio::task::spawn_blocking` and `await` it, and make `validate_webhook_url`
an `async fn` — noting that the redirect-policy closure is synchronous and cannot
`await`, which is itself an argument for approach 2.

Add an explicit resolution timeout regardless of approach; the platform default
is too long for a request path.

**How to verify the fix.** Point a webhook at a hostname served by a blackholed
nameserver, dispatch 200 deliveries concurrently, and assert the API's p99 latency
on an unrelated endpoint (`GET /health`) is unaffected. Before the fix it
degrades sharply or times out.

**Blast radius.** Changing `validate_webhook_url` to `async` touches every caller
(6 sites). Keep a synchronous variant for the pure-IP/scheme/hostname checks that
do not need DNS, so unit tests stay simple.

---

### 3.9 Stripe billing (`src/api/handlers/billing.rs`)

> This subsystem is visibly the most carefully hardened in the codebase —
> multi-signature rotation support, atomic event claiming, ordering guards, and
> terminal-status handling are all present and correct. The two findings below
> are the remaining gaps, not a general indictment.

---

#### `[ ]` B-33 — A crash between claiming and processing a Stripe event loses that event permanently

- **Severity:** HIGH
- **Confidence:** CONFIRMED
- **Where:** [`src/api/handlers/billing.rs:252-300`](src/api/handlers/billing.rs#L252) — `webhook()`

**What is wrong.** Idempotency is implemented as claim → process → release-on-error:

```rust
// 1. CLAIM (its own statement, therefore its own committed transaction)
INSERT INTO processed_stripe_events (event_id) VALUES ($1) ON CONFLICT (event_id) DO NOTHING
if claimed.rows_affected() == 0 { return Ok(StatusCode::OK); }   // already claimed -> skip

// 2. PROCESS
let outcome = match event.event_type.as_str() { ... };

// 3. RELEASE only if the handler returned Err
if let Err(e) = outcome {
    DELETE FROM processed_stripe_events WHERE event_id = $1
    return Err(e);
}
```

The comment claims "The claim is only made durable on success". That is not what
the code does: step 1 is a standalone statement and is **durable immediately**.
Step 3 is a compensating action that only runs if the handler returns an `Err`
*and the process is still alive to run it*.

**Why it is wrong.** The compensation covers exactly one failure mode (a handler
returning `Err`) and misses every abrupt one: process crash, OOM kill, container
stop during a deploy, panic, or the `DELETE` itself failing (which the code
logs and then gives up on, at line 296). In all of those, the claim survives and
the event is marked processed although it was not.

Stripe will retry — and every retry hits `rows_affected() == 0` and returns
`200 OK`. The event is discarded forever, with a log line that reads like normal
duplicate suppression.

**How it fails in production.** A deploy rolls pods while an `invoice.paid`
webhook is in flight. The claim committed; the handler never ran. Stripe retries
for up to three days and each retry is answered `200 OK, already-claimed`. The
customer paid, the subscription is never marked active, and the org keeps or
loses access incorrectly depending on which handler was lost:

| Lost event | Consequence |
| --- | --- |
| `invoice.paid` | Paid customer not marked active — may be downgraded or blocked |
| `checkout.session.completed` | Purchase never provisions the plan |
| `customer.subscription.deleted` | Cancelled customer keeps a paid plan indefinitely |
| `invoice.payment_failed` | Failed payment never triggers dunning |

Every row is a money error in one direction or the other, and none of them raises
an alert — the retries all return `200`.

**Proposed fix.** Make the claim reflect *completion*, not *attempt*. Add a state
column and a timestamp:

```sql
ALTER TABLE processed_stripe_events ADD COLUMN status TEXT NOT NULL DEFAULT 'done';
ALTER TABLE processed_stripe_events ADD COLUMN claimed_at TIMESTAMPTZ NOT NULL DEFAULT NOW();
```

Then:
- Claim with `status = 'processing'`.
- On success, `UPDATE ... SET status = 'done'`.
- On the duplicate path, skip **only** when `status = 'done'`, or when
  `status = 'processing'` and `claimed_at > NOW() - INTERVAL '5 minutes'` (a
  concurrent in-flight attempt). A `processing` row older than that is a crashed
  attempt: re-claim it (`UPDATE ... SET claimed_at = NOW()` guarded on the stale
  predicate) and process the event.

The handlers are already documented as idempotent and ordering-guarded
(`stripe_event_applies`), so reprocessing a stale claim is safe — that property
is what makes this fix viable.

An alternative — doing the claim and the handler's writes in one transaction —
is cleaner but larger: the handlers currently take `&AppState` and use the pool
directly, so it would mean threading a transaction through all six. The
status-column approach is the smaller correct change.

**How to verify the fix.** Simulate a crash: claim an event, then abort the
process before the handler completes (a test seam or a manual
`INSERT ... status='processing', claimed_at = NOW() - INTERVAL '10 minutes'`).
Re-deliver the same event id and assert it is now processed rather than skipped.
Assert a genuinely concurrent duplicate (fresh `claimed_at`) is still skipped.

Also: audit for damage already done —
```sql
SELECT event_id, received_at FROM processed_stripe_events ORDER BY received_at DESC LIMIT 50;
```
and cross-check against the Stripe dashboard's event log for events marked
delivered that produced no corresponding change in `organizations`. Record what
you find here.

**Blast radius.** Adds a migration and changes duplicate-suppression semantics.
The 5-minute staleness window must exceed the longest handler runtime — measure
`handle_checkout_completed`, which makes outbound Stripe API calls
(`cancel_stripe_subscription`, line 652), before settling on the value.

---

#### `[ ]` B-34 — Stripe signature is computed over a lossy UTF-8 conversion of the body

- **Severity:** LOW
- **Confidence:** CONFIRMED
- **Where:** [`src/api/handlers/billing.rs:1031`](src/api/handlers/billing.rs#L1031)

**What is wrong.** The signed payload is built by converting the raw body through
`String::from_utf8_lossy`:

```rust
let signed_payload = format!("{}.{}", timestamp_str, String::from_utf8_lossy(body));
mac.update(signed_payload.as_bytes());
```

`from_utf8_lossy` replaces any invalid byte sequence with U+FFFD (`EF BF BD`),
so the bytes fed to the HMAC are not necessarily the bytes Stripe signed.

**Why it is wrong.** HMAC verification must operate on the exact received bytes.
The conversion is also unnecessary — `body` is already `&[u8]` and the MAC only
needs bytes.

**How it fails in production.** In practice, not at all today: Stripe sends
UTF-8 JSON, so the lossy path is never taken and the digest is correct. The
failure direction is also safe — a mangled body produces a *mismatch* (rejection),
never a false accept. It is recorded because it is a latent correctness defect in
a signature check, which is precisely the kind of code that should have none, and
because the fix is three lines.

**Proposed fix.** MAC over the raw bytes:

```rust
mac.update(timestamp_str.as_bytes());
mac.update(b".");
mac.update(body);
```

**How to verify the fix.** Existing Stripe webhook tests
(`tests/stripe_webhook_tests.rs`) must still pass unchanged. Add one case with a
body containing a non-UTF-8 byte, signed correctly, and assert it verifies.

**Blast radius.** None — for all valid UTF-8 bodies the digest is byte-identical
to the current one.

---

### 3.10 Dead code carrying security claims

---

#### `[ ]` B-35 — `EventBroadcaster` is dead code whose doc comments describe a tenant-isolation design that is not in use

- **Severity:** NIT
- **Confidence:** CONFIRMED
- **Where:** [`src/api/handlers/realtime.rs:190-234`](src/api/handlers/realtime.rs#L190)

**What is wrong.** `EventBroadcaster` is constructed in exactly one place — its
own unit test at line 1127. Verify:

```bash
grep -rn "EventBroadcaster" --include="*.rs" src/
```

The production realtime path uses Redis pub/sub plus DB polling instead
(`sse_job_handler`, `sse_queue_handler`, `websocket_handler`). The struct's
comments nonetheless read as live design documentation:

```rust
/// Now supports organization-scoped event broadcasting
/// Now requires organization_id to prevent cross-tenant leakage
```

and the isolation it advertises is not actually implemented even within the dead
code — `subscribe_for_org` ignores its argument entirely:

```rust
pub fn subscribe_for_org(&self, _org_id: &str) -> broadcast::Receiver<OrgScopedEvent> {
    // In production, consider using separate channels per org for better performance
    self.sender.subscribe()          // single global channel, every org's events
}
```

**Why it is wrong.** Same failure mode as B-23: a plausible-looking, `pub`,
security-commented component that nobody has exercised. If someone wires it up,
they inherit two problems at once — a single global `broadcast::channel` with
`BROADCAST_CAPACITY = 1000` shared by all tenants (so a noisy org's burst causes
`RecvError::Lagged` and silent event loss for *other* orgs), and no
`RecvError::Lagged` handling anywhere in the file (`grep -n "Lagged" src/api/handlers/realtime.rs`
returns nothing), so a lagging subscriber's stream would terminate rather than
resynchronise.

**How it fails in production.** It does not — it is not wired up.

**Proposed fix.** Delete `EventBroadcaster`, `OrgScopedEvent`, and their tests.
If the design is wanted later, the two problems above must be solved first:
per-org channels (or a per-org filter applied before the channel), and explicit
`Lagged` handling that tells the client it missed events rather than dropping the
connection.

**How to verify the fix.** `cargo build` and `cargo clippy` clean. Confirm
`RealtimeEvent` itself is still used by the live handlers before deleting it —
it may be shared.

**Blast radius.** None if genuinely unreferenced outside its own tests. Check
`tests/` as well as `src/`.

---

## 4. Summary and suggested order of work

### 4.1 All findings at a glance

| ID | Severity | Confidence | Area | One-line summary |
| --- | --- | --- | --- | --- |
| B-01 | HIGH | CONFIRMED | Outgoing webhooks | Scoped keys read all delivery payloads (`require_unrestricted_key` missing on 2 handlers) |
| B-02 | MEDIUM | CONFIRMED | Outgoing webhooks | `enabled: false → true` bypasses the webhook plan cap |
| B-03 | MEDIUM | CONFIRMED | Outgoing webhooks | `failure_count` counts attempts, not deliveries, and nothing reads it |
| B-04 | HIGH | CONFIRMED | Outgoing webhooks | Unbounded delivery task fan-out → memory/socket exhaustion |
| B-05 | MEDIUM | CONFIRMED | Outgoing webhooks | `outgoing_webhook_deliveries` has no retention |
| B-06 | LOW | CONFIRMED | Outgoing webhooks | Webhook secret can never be cleared |
| B-07 | LOW | CONFIRMED | Outgoing webhooks | Completion webhooks omit the `id` field org webhooks carry |
| B-08 | MEDIUM | CONFIRMED | Workers | Heartbeat writes deprecated columns → API shows 0 jobs, false `degraded` |
| B-09 | LOW | CONFIRMED | Workers | `workers_active` gauge drifts negative |
| B-10 | NIT | CONFIRMED | Workers | REST `register` `ON CONFLICT` is unreachable; restarts churn the worker cap |
| B-11 | MEDIUM | CONFIRMED | Queue | Dead-letter status + audit row are not in one transaction |
| B-12 | MEDIUM | CONFIRMED | Scheduler | Stale-worker sweep dead-letters without an audit row |
| B-13 | LOW | CONFIRMED | Queue | Dead-lettered jobs keep a stale `lease_expires_at` |
| B-14 | MEDIUM | CONFIRMED | Queue | History-write failure reported as operation failure → duplicate side effects |
| B-15 | LOW | CONFIRMED | Queue | Comment claims multi-parent dependency support that does not exist |
| B-16 | MEDIUM | **SUSPECTED** | Scheduler | Jobs whose dependency row cascades away may strand un-runnable |
| B-17 | LOW | CONFIRMED | Scheduler | Cancellation-webhook loop's correctness rests on undocumented invariants |
| B-18 | HIGH | CONFIRMED | Auth | API keys accepted in the query string on every endpoint |
| B-19 | HIGH | CONFIRMED | Auth | `last_used` UPDATE per request → row-lock contention + bloat |
| B-20 | MEDIUM | CONFIRMED | Auth | Cached keys skip the org soft-delete check for up to 60 s |
| B-21 | LOW | CONFIRMED | Auth | Cache key is a 64-bit hash truncation and a hit skips bcrypt |
| B-22 | HIGH | CONFIRMED | Rate limiting | Limits key on client-controlled `X-Forwarded-For` → bypassable |
| B-23 | LOW | CONFIRMED | Rate limiting | Dead `rate_limit.rs` fails open while claiming it does not |
| B-24 | MEDIUM | **SUSPECTED** | Observability | `/metrics` may be unauthenticated (config-dependent) |
| B-25 | MEDIUM | CONFIRMED | Auth/rate limiting | Two client-IP extractors; the security-critical one is the weak one |
| B-26 | MEDIUM | CONFIRMED | Email login | Anyone can lock a known email out of login |
| B-27 | MEDIUM | CONFIRMED | Email login | `check-email` writes a DB row per request into the table it scans |
| B-28 | LOW | CONFIRMED | Email login | `check-email` is an enumeration oracle (accepted risk; record it) |
| B-29 | HIGH | CONFIRMED | Quota | Bulk refund can drive counters negative → free quota across midnight |
| B-30 | MEDIUM | CONFIRMED | Quota | Four indirect enqueue paths still use the non-atomic quota pattern |
| B-31 | HIGH | CONFIRMED | SSRF | DNS-rebinding protection claimed but not implemented (no IP pinning) |
| B-32 | MEDIUM | CONFIRMED | SSRF | Blocking DNS resolution on the async runtime → runtime starvation |
| B-33 | HIGH | CONFIRMED | Billing | A crash between claim and process loses a Stripe event permanently |
| B-34 | LOW | CONFIRMED | Billing | Signature computed over a lossy UTF-8 conversion of the body |
| B-35 | NIT | CONFIRMED | Realtime | `EventBroadcaster` is dead code with misleading isolation comments |

**Totals:** 8 HIGH · 13 MEDIUM · 10 LOW · 2 NIT · 2 SUSPECTED (B-16, B-24) · 0 CRITICAL.

No cross-tenant data exposure and no authentication bypass were found. The prior
audit waves (§2) evidently closed that class of problem: every queue-scope and
org-isolation predicate spot-checked during this review was present and correct.
What remains clusters into four themes:

1. **Unbounded work** — B-04, B-19, B-32, and B-27 all spawn or write without a
   ceiling. These are the findings most likely to cause a real outage.
2. **Compensating actions that assume the process survives** — B-11, B-12, B-33.
   Each is correct on the happy path and loses data on a crash or deploy.
3. **Trusting client-supplied input for identity** — B-18, B-22, B-25.
4. **Dead or divergent code carrying confident security comments** — B-23, B-35,
   B-08's deprecated columns, B-15. Individually harmless; collectively they make
   the codebase read as safer than it is.

### 4.2 Suggested order

Sequenced by risk-reduction per unit of effort, not strictly by severity.

**Wave 1 — verification only, no code (do this first, it is cheap and it
re-scopes the rest).**
- [ ] B-24: `curl` `/metrics` in production; record the result in this file.
- [ ] B-16: run the reproduction; mark CONFIRMED or INVALID.
- [ ] B-29: run the drift query on `organization_usage`; record how much damage exists.
- [ ] B-33: cross-check `processed_stripe_events` against Stripe's event log for silently-lost events.
- [ ] B-18: grep production access logs for `api_key=`/`token=` on non-realtime paths to size the breaking change.

**Wave 2 — small, self-contained, high value.**
- [ ] B-01 (two `require_unrestricted_key` lines)
- [ ] B-13 + B-11 + B-12 (one dead-letter-path commit each; B-13 rides along with B-11)
- [ ] B-14 (four `.await?` → logged best-effort)
- [ ] B-34, B-06, B-07, B-09

**Wave 3 — the identity/limits cluster (do B-25 first; B-22 depends on it).**
- [ ] B-25 → B-22 → B-27 → B-26 → B-28
- [ ] B-19, B-20, B-21

**Wave 4 — the unbounded-work cluster.**
- [ ] B-31 + B-32 together (one custom `reqwest::dns::Resolve` fixes both)
- [ ] B-04 + B-03 together (bounded dispatcher + circuit breaker)
- [ ] B-05

**Wave 5 — quota and billing correctness.**
- [ ] B-29 (add `refund_daily_jobs` + CHECK constraints) → B-30 (port the four paths)
- [ ] B-33

**Wave 6 — breaking / coordinated.**
- [ ] B-18 (needs an announcement and a version bump)
- [ ] B-02, B-08, B-10

**Wave 7 — cleanup, batch into one commit.**
- [ ] B-15, B-17, B-23, B-35

### 4.3 What this audit did **not** cover

Stated plainly so nobody mistakes this document for exhaustive. The following
were not read, or were only skimmed via grep:

- `src/grpc/services/queue_service.rs` (2 362 lines) and `worker_service.rs` — only the dead-letter parity sites and the quota call sites were examined. **This is the largest unreviewed surface and should be the next audit's starting point.**
- `src/api/handlers/admin.rs`, `organizations.rs`, `schedules.rs`, `workflows.rs`, `queues.rs`, `health.rs`, `webhooks.rs`, `auth.rs`, `api_keys.rs` — read only around specific call sites.
- `src/api/handlers/realtime.rs` beyond the broadcaster; the SSE/WS polling loops were not reviewed for leaks or scope.
- `src/config/plans.rs`, `src/error/`, `src/observability/`, `src/models/**` (except `api_key.rs`, `job.rs` partially).
- `src/queue/mod.rs` lines 980–1387; `src/scheduler/mod.rs` lines 1–640 and 1000–1187.
- The `tests/` tree (≈600 KB) — not assessed for coverage gaps or for tests that pass vacuously.
- Dependency/supply-chain review (`cargo audit`, `cargo deny`) — not run.
- No code was executed: nothing was compiled, no tests were run, and no queries
  were executed against any database. Every CONFIRMED finding is confirmed by
  reading, not by observation.

---

## 5. Implementation record (2026-08-16)

Everything below was implemented in one pass against commit `9ae1d90`.
Gates at completion: `cargo check --all-targets` clean, `cargo clippy
--all-targets` clean, `cargo fmt --check` clean, **332/332 unit tests pass**.

> **Test status: the full suite has been run, twice, and passes.**
>
> | Run | Result |
> | --- | --- |
> | `cargo test --all-features` against **local PostgreSQL 17 + Redis** | **1098 passed, 0 failed**, 3 ignored |
> | `cargo test --all-features` via **testcontainers** (PostgreSQL 16, the CI path) | **1098 passed, 0 failed**, 3 ignored |
>
> Identical results on both paths. `cargo clippy --all-targets --all-features`
> and `cargo fmt --check` are clean.
>
> Getting there required two fixes and one harness change, all recorded in §5.6:
> the integration targets did **not compile** against this work until now, because
> six of the eight files in `tests/` are gated behind the `docker-tests` feature
> and `default = []` — so the plain `cargo test` used throughout the earlier
> passes had never built them.
>
> Separately, the SQL was exercised directly against PostgreSQL (scratch DB, all
> 22 migrations, then dropped) — see §5.5.

### 5.1 Verification results (§4.2 Wave 1)

| Check | Result |
| --- | --- |
| B-24 — is `/metrics` public? | **`curl https://api.spooled.cloud/metrics` → `401`.** `METRICS_TOKEN` is set in production. **Finding INVALID**, no code change needed. |
| B-14 — does a history-write failure propagate? | **No.** `record_history` (`src/queue/mod.rs`) logs and returns `Ok(())` unconditionally; the `?` at its call sites can never fire. **Finding INVALID.** |
| B-20 — does org soft-delete leave cached keys valid? | **Partly INVALID.** `src/api/handlers/admin.rs:589-605` already evicts every cached key on soft-delete, and `PUT /api-keys/{id}` already invalidates on scope change. The genuinely uninvalidated cache was `org_rate_limits:{org_id}` → filed and fixed as **B-36**. |
| B-16 — can a dependent strand? | Confirmed reachable by inspection: `job_dependencies.depends_on_job_id` is `ON DELETE CASCADE`, so retention deleting the blocker removes the edge, and the child then matches neither the dequeue predicate nor the cancellation sweep. Fixed. |
| B-29 / B-33 — production data drift | **NOT run** — needs production DB and Stripe dashboard access this session did not have. The queries are in the findings; the migration includes a repair `UPDATE` for B-29. **Operator action.** |
| B-18 — who actually uses `?api_key=` | Cross-repo scan: the **Go SDK's SSE client** sends `?api_key=` with no header; the dashboard sends `?token=<jwt>` on WS/SSE. The PHP SDK sends `?api_key=` on `getRaw()` but *also* sends `Authorization: Bearer`. Fix scoped accordingly. |

### 5.2 Per-finding outcome

| ID | Outcome | What was done |
| --- | --- | --- |
| B-01 | fixed | `require_unrestricted_key` added to `deliveries()` and `retry_delivery()`. |
| B-02 | fixed | `update()` now takes the advisory lock and charges the webhooks cap on a `false → true` transition only; turning one off never fails on quota. |
| B-03 | fixed | `failure_count` moved out of `record_attempt` into a new `record_delivery_outcome`, called once per delivery. Added the missing consumer: an auto-disable breaker at `AUTO_DISABLE_FAILURE_THRESHOLD = 20` that sets `enabled = false` with `last_status = 'auto_disabled'`. Manual retries clear the streak. |
| B-04 | fixed | `OutgoingWebhookService` now holds a `Semaphore`; `deliver()` holds a permit across retries and backoff sleeps. Capacity from new `OUTGOING_WEBHOOK_MAX_CONCURRENT_DELIVERIES` (default 64). `deliveries_in_flight()` exposed for metrics. |
| B-05 | fixed | `outgoing_webhook_deliveries` added to the per-org retention sweep under `history_retention_days`, batched at 10 000. `email_login_codes` added too — same unbounded-growth shape, and `verify()` scans that table on every login. |
| B-06 | fixed | `secret` is now `Option<Option<String>>` via a shared `double_option` helper (hoisted out of `admin.rs` into `src/models/serde_helpers.rs`). `null` clears, absent keeps. Length checked by hand since `validator` cannot see through the nesting. |
| B-07 | fixed | Completion webhooks now carry `id` and `X-Spooled-Delivery-Attempt`, matching org webhooks so receivers can dedupe on `body.id` for both kinds. |
| B-08 | fixed | Built-in worker heartbeat writes `current_job_count` and compares `max_concurrent_jobs` (the live columns) and sets `updated_at`. It had been writing the deprecated pair, so the API always reported 0 active jobs and workers flipped to `degraded` above 5. |
| B-09 | fixed | `workers_active.dec()` guarded on `previous_status != "offline"`. |
| B-10 | fixed | Took option (a): `RegisterWorkerRequest.worker_id` is now optional, making the `ON CONFLICT` path live so a restart reuses one row. Re-registration is not charged against the cap (counted inside the advisory lock) and refreshes declared capabilities. |
| B-11 | fixed | All dead-letter paths are now atomic. `queue/mod.rs` (`fail_by_worker`, `fail`) and the scheduler use a data-modifying CTE; gRPC `fail` likewise; gRPC `ProcessJobs` keeps its transaction but now **rolls back and returns INTERNAL** instead of logging past a failed insert — in Postgres that failure poisons the transaction, so the old log-and-continue guaranteed the later commit would fail anyway. |
| B-12 | fixed | Stale-worker sweep converted to the same CTE, so it writes the `dead_letter_queue` audit row it was skipping. |
| B-13 | fixed | `lease_expires_at = NULL` added to every dead-letter UPDATE. |
| B-14 | **INVALID** | See §5.1. |
| B-15 | fixed | Doc comment rewritten to say what the query actually covers (legacy scalar `parent_job_id` only) and to point at the trigger + sweep that handle workflow dependencies. |
| B-16 | fixed | New sweep clause unblocks jobs with `dependencies_met = FALSE`, no `parent_job_id`, and **no** `job_dependencies` rows, after a 1-hour grace period (`POST /jobs` inserts the job before its edges, so the grace window is load-bearing). Marks runnable rather than cancelling: if the blocker completed and was then retained away, the job *should* run. |
| B-17 | fixed | Loop invariants documented at the cascade loop head. |
| B-18 | fixed | Auth split into `authenticate_api_key` (header only) and `authenticate_api_key_allow_query`. The four realtime routes moved to their own router carrying the query-tolerant variant; all other protected routes are header-only. Query values are now percent-decoded and empty values rejected. **The Go SDK's SSE client and the dashboard's `?token=` WS/SSE both keep working.** |
| B-19 | fixed | `last_used` writes gated by a new `RedisCache::set_if_absent` (`SET NX EX`) at 5-minute granularity — one write per key per interval instead of one per request. Falls back to write-through with no Redis. |
| B-20 | **mostly INVALID** | See §5.1; superseded by B-36. |
| B-21 | fixed | Cache key now uses the full `api_key_lookup_hash` (64 hex) instead of a 64-bit truncation. |
| B-22 | fixed | See B-25 — one shared extractor, counted from the right. |
| B-23 | fixed | `src/api/middleware/rate_limit.rs` deleted (314 lines) with its re-export. Never mounted, and it failed open in every degraded branch while its comment claimed otherwise. |
| B-24 | **INVALID** | Production already returns `401`. |
| B-25 | fixed | New `src/api/middleware/client_ip.rs`: prefers a configured edge header (`TRUSTED_CLIENT_IP_HEADER`), then counts `X-Forwarded-For` from the **right** skipping `TRUSTED_PROXY_HOPS`, then a stable `"unknown"`. Default is **trust nothing**, correct for self-host. Used by both rate limiters and by email-login. The User-Agent fallback was removed — also client-controlled. |
| B-26 | fixed | `verify()` now constant-time-compares against **all** live codes for the address (bounded by `MAX_CODES_PER_HOUR`), charging the attempt to the matching row, or to the newest when nothing matches. The per-code cap is unchanged, so an attacker gains no guesses — but a third party can no longer invalidate a victim's in-flight code and lock them out. |
| B-27 | fixed | `check-email` throttling moved to Redis (`check_rate_limit`), so the endpoint no longer writes a permanent row per request into the table it scans — and no longer slows the login path as it is abused. Fails open without Redis unless `RATE_LIMIT_FAIL_CLOSED`. |
| B-28 | fixed | `query.validate()` is now actually called; the `#[validate(email)]` derive was present but never run. |
| B-29 | fixed | New `refund_daily_jobs` SQL function: positive input, `GREATEST(0, …)` floor, **compares** the reset columns instead of writing them (so a refund can never trigger a reset), and leaves `total_jobs_created` alone. `bulk_enqueue` switched to it. Migration repairs existing negative rows and adds a `CHECK` so a regression fails loudly. |
| B-30 | fixed | All four indirect paths ported to `try_increment_daily_jobs` with reserve-before-insert and refund-on-failure. Each preserves its own error shape (schedules returns a bare `Response`; webhooks/workflows wrap in `AppError::LimitExceeded`; the scheduler keeps advance-and-skip so it cannot spin). `webhooks.rs` refunds on the idempotent-dedupe branch, which is the common case under upstream retries. |
| B-31 | fixed | New `SsrfGuardedResolver` implementing `reqwest::dns::Resolve`: every address the client is about to connect to is checked against the same IP policy, on every lookup, redirects included. Rejects the whole lookup if **any** answer is blocked, so an attacker cannot mix one public address in with internal ones. The URL-time check is kept for fast user feedback, with its doc comment corrected to stop claiming rebinding protection it never provided. |
| B-32 | fixed | Same change: resolution now runs on `spawn_blocking` under a 5-second timeout instead of blocking a runtime worker, and the redirect closure no longer resolves at all (`check_dns_resolution: false`). |
| B-33 | fixed | Claims now record **completion**: `status='processing'` on claim, `'done'` only after the handler succeeds. A duplicate is suppressed only when `done`, or `processing` and fresher than `STRIPE_CLAIM_STALE_AFTER_SECS`; a stale `processing` row is re-claimed. A crash between claim and handler no longer discards a paid invoice. |
| B-34 | fixed | Signature HMAC computed over the raw body bytes rather than `String::from_utf8_lossy`. |
| B-35 | fixed | `EventBroadcaster` / `OrgScopedEvent` deleted with their tests, replaced by a comment recording the two problems any future broadcaster must solve first (per-org channels; explicit `Lagged` handling). |
| **B-36** | **new, fixed** | `org_rate_limits:{org_id}` was cached for 60 s and **never invalidated anywhere** — so an admin plan change or an org deletion left the old rps/burst in force for up to a minute, which in the downgrade direction is a minute of unpaid headroom. Added `org_rate_limits_key()` + `invalidate_org_rate_limits()` as the single source of truth (the key was previously formatted independently in three modules), called from `update_organization` and both delete branches, and adopted by both gRPC services. |

### 5.2b Adversarial review of the remediation, and what it caught

The finished diff was put through a 5-lens adversarial review (SQL/data
correctness, auth, concurrency, regression, billing), with every candidate
finding then handed to two independent skeptics instructed to **refute** it and
to default to "not a defect". 19 candidates were raised; **5 survived
verification**, and every one of them was a real defect introduced by the
remediation itself:

| Confirmed against the fix | Severity | Resolution |
| --- | --- | --- |
| `client_ip()` off-by-one selected the **caller-supplied** `X-Forwarded-For` entry | HIGH | **Fixed.** With `hops` trusted proxies the client address is at index `len - hops`, not `len - hops - 1`. The wrong index reinstated the exact bypass B-22 existed to close: send `X-Forwarded-For: <random>`, the proxy appends the true address after it, and the attacker's value is the one picked. Tests rewritten to assert the proxy-appended entry wins. |
| `refund_daily_jobs` did not reverse `total_jobs_created` | MEDIUM | **Fixed.** The reasoning behind "lifetime totals only grow" was wrong in the presence of reservations: `try_increment_daily_jobs` charges the **reservation**, which is provisional, so its reversal must be too. A 100-item bulk that fully deduped previously added 100 to a "jobs ever created" counter while creating none. Reversing a provisional charge is not the same as decrementing a lifetime total. |
| Email verify compared one guess against every live code while charging one attempt | MEDIUM | **Fixed.** The B-26 fix (compare against all live codes) removed the lockout but multiplied brute-force reach by the number of outstanding codes. Added a per-**address** attempt budget in Redis, so total guesses against an address are independent of how many codes exist. Deliberately *not* fixed by charging every candidate row — that would let five wrong guesses burn all live codes and recreate the lockout. |
| Stripe-driven `plan_tier` changes left the rate-limit cache stale | LOW | **Fixed.** B-36 wired invalidation into the admin path only; the Stripe path is the one real customers take. Now invalidated on subscription update, cancellation, and reconcile. |
| Refund can land on the wrong day when a concurrent enqueue rolls the reset date | MEDIUM | **Documented, not fixed** — see the comment in the migration. The refund cannot drive the counter negative, so the org gains at most `min(refund, jobs_enqueued_today)` headroom, and the race needs the refund within milliseconds of midnight, when that second term is near zero. Closing it needs a reservation token threaded through four call sites, which costs more than the defect. |

**Read the "refuted" count with care.** Most of the 14 refutations are a
methodological artifact, not a verdict that the reviewers were wrong: several
findings (semaphore acquired after spawn, blocking `getaddrinfo` in the delivery
path, the `"unknown"`-for-everything default, the worker gauge double-increment,
the Stripe `200 OK` for an unprocessed event, the un-owned claim release, the
`check-slug` extractor) were **fixed while the verification phase was still
running**, so the skeptics correctly reported that the code no longer matched the
claim. Those fixes are listed below and are real work, not dismissed findings.

### 5.2c Additional fixes made in response to the review

| Fix | Why it mattered |
| --- | --- |
| Delivery permit acquired **before** `tokio::spawn`, not inside the task | Acquiring inside bounded concurrent HTTP but not the number of tasks: they piled up parked on the semaphore, each holding its own payload clone, so B-04's memory exhaustion still happened. Awaiting before the spawn is safe because `dispatch_event` already runs detached. |
| `validate_url` on the delivery path no longer resolves DNS | The B-31/B-32 resolver fixed the client, but `try_deliver` still called the blocking `to_socket_addrs()` on every **attempt**, on the runtime. DNS validation now runs only at configure time; the IP policy is enforced per-connection by the resolver, which is strictly stronger. |
| `retry_delivery` acquires a permit | Otherwise the manual-retry endpoint was an unmetered path to the same exhaustion the cap prevents. |
| Worker registration gauges gated on `adding > 0` | The B-10 upsert made re-registration free of a new row, but the gauge still incremented every restart while `deregister` decrements once — B-09's drift from the other direction. |
| `client_ip` falls back to well-known edge headers | With `TRUSTED_PROXY_HOPS` defaulting to 0 and no `TRUSTED_CLIENT_IP_HEADER` set, the new extractor returned `"unknown"` for **every** request, collapsing all public callers into one bucket — worse than the spoofable behaviour it replaced. It now consults `CF-Connecting-IP`/`X-Real-IP` by default, so a Cloudflare or nginx deployment is protected without configuration. |
| Stripe returns `409`, not `200`, when a claim is held by a live attempt | Answering `200` tells Stripe the event was handled when nobody is handling it. Only a `done` claim now suppresses a retry. |
| Stripe claim release/finalize guarded on our own `claimed_at` | An attempt that outlived the stale window could otherwise delete a claim a *different* live attempt had taken over. |
| `check-slug` migrated to the shared extractor and to Redis throttling | It kept a private spoofable extractor and wrote a row per request into `email_login_codes` — and per the cross-repo scan it is the **highest-frequency** public endpoint, called as the user types in the signup form. |

### 5.3 New configuration

All default to previous behaviour except where noted.

| Variable | Default | Effect |
| --- | --- | --- |
| `OUTGOING_WEBHOOK_MAX_ATTEMPTS` | `5` | Was a hardcoded literal at `src/api/mod.rs:60`. |
| `OUTGOING_WEBHOOK_MAX_CONCURRENT_DELIVERIES` | `64` | **New ceiling** (B-04). Previously unbounded. |
| `TRUSTED_CLIENT_IP_HEADER` | unset → falls back to `CF-Connecting-IP`, then `X-Real-IP` | Edge header carrying the real client IP. Set it explicitly to pin one header. |
| `TRUSTED_PROXY_HOPS` | `0` | How many entries our own proxies appended to `X-Forwarded-For`. `0` means that header is not consulted at all (self-host default). Set to `1` behind a single proxy. |

### 5.5 SQL verified against a real PostgreSQL

Run against a scratch database on local PostgreSQL 17 with all 22 migrations
applied from scratch (then dropped). This is execution, not reasoning — it is
what turns the SQL half of this work from "reviewed" into "verified". The
adversarial review named the rewritten CTEs "the most likely bug in the diff";
these runs disprove that specific concern.

| Property | Result |
| --- | --- |
| All 22 migrations apply to an empty DB | pass — including `20260816120000` |
| Reserve 100 → refund 100 | all three counters return to `0`, `total_jobs_created` included |
| Refund larger than the balance | floors at `0`; never negative |
| Refund whose reservation belonged to an earlier day | **skipped** — counter and `last_daily_reset` untouched, so a refund cannot cross a reset boundary |
| `try_increment_daily_jobs` at the cap / over the cap | returns the count / returns `-1` |
| `CHECK` on negative counters | rejects with `check_violation` |
| Stripe claim (a) first delivery | granted |
| Stripe claim (b) duplicate while fresh `processing` | denied → handler returns `409`, Stripe retries |
| Stripe claim (c) `processing` older than the stale window | **re-claim granted** — this is the crashed-attempt recovery B-33 exists for |
| Stripe claim (d) `done` | denied, row untouched → handler returns `200` |
| Claim release guarded on own `claimed_at` | own claim deleted; a superseded attempt **cannot** delete the live holder's claim |
| Dead-letter CTE (`fail_by_worker`) | job settled, `lease_expires_at` cleared, exactly one audit row, written atomically |
| Same CTE against a job the worker does not own | zero rows, no audit row — this is how the handler detects "not owned" |
| gRPC CTE, **retry** branch | returns `new_status=pending` with a backoff delay and writes **no** audit row (the conditional `WHERE u.status='deadletter'` behaves) |
| Delivery retention across plan tiers | free-tier 5-day-old row deleted, free-tier 2-hour row kept, pro-tier 5-day-old row kept |
| B-16 premise | deleting the blocking job **does** cascade the `job_dependencies` edge away, confirming the strand condition is reachable |
| Orphan-dependency rescue | 2-hour-old stranded job released; a job created moments ago left alone by the grace window |

### 5.6 Making the integration suite runnable, and what it caught

The suite was never run during the remediation because the Docker engine on the
work machine would not start (the VM wants a 32 GiB disk image; the host was at
97% full). That was treated as an acceptable gap for too long. It was not.

**What running it caught.** The integration targets did not even *compile*
against this work:

- `tests/common/mod.rs` and every dependent target failed on `sqlx::query` with a
  dynamic string — the crate requires `sqlx::AssertSqlSafe` for those.
- `tests/integration_tests.rs:2303` still called
  `OutgoingWebhookService::new` with two arguments; B-04 added the concurrency
  ceiling as a third.

Neither is caught by `cargo test`, `cargo clippy` or `cargo fmt` without
`--all-features`, because `default = []` and the six Docker-dependent test files
are behind the `docker-tests` feature. **Any future change must be checked with
`--all-features`**; without it roughly 3% of the suite compiles and the run still
reports success. This is now called out in `CONTRIBUTING.md`.

**The harness change.** `TestDatabase`/`TestRedis` now honour `TEST_DATABASE_URL`
and `TEST_REDIS_URL`, falling back to containers when unset. On the borrowed
path each `TestDatabase` creates its own uniquely-named database on the supplied
server, runs the full migration set, and drops it on `Drop` (with `FORCE`, so a
pool that has not finished releasing cannot block teardown). Isolation is
therefore identical to the container path, and a run leaves no residue —
verified: zero `spooled_test_*` databases remain afterwards.

This is a permanent improvement, not a workaround for one bad afternoon: the
suite is no longer silently unrunnable whenever Docker is unhappy, which is
precisely when a regression is most likely to slip through.

### 5.4 Deployment notes

1. **Run the migration** — `migrations/20260816120000_quota_refund_and_stripe_claim_state.sql`. Additive plus one repair `UPDATE`; the `CHECK` constraint would fail to apply if any counter were still negative, which is why the repair runs first.
2. Optionally set `TRUSTED_CLIENT_IP_HEADER=CF-Connecting-IP` and `TRUSTED_PROXY_HOPS=1`. Not required — production sits behind a Cloudflare Tunnel and `CF-Connecting-IP` is consulted by default — but pinning the header explicitly is clearer than relying on the fallback order. **Verify after deploy** that public-route `429`s are per-client and not global.
3. **Breaking:** REST endpoints no longer accept `?api_key=` / `?token=`. Realtime (`/ws`, `/events*`) still does. Needs a `CHANGELOG.md` entry and a minor version bump.
4. **Behaviour change:** outgoing webhooks auto-disable after 20 consecutive failed deliveries. Users need a way to see and undo this; `last_status = 'auto_disabled'` is the marker.
5. **Behaviour change:** `PUT /outgoing-webhooks/{id}` with `{"secret": null}` now CLEARS the secret. A client that serialises unchanged fields as explicit `null` will clear secrets it meant to keep — check the dashboard's update payload before deploying.
6. Still open, unchanged, **operator-only**: rotate `certs/grpc-key.pem` (tracked in git) and rotate `ADMIN_API_KEY`.

---

## 9. Coverage log (what has actually been read)

Updated as the review proceeds, so a resumed session knows where to pick up.

| Area | Files | Status | Findings |
| --- | --- | --- | --- |
| Outgoing webhooks | `src/outgoing_webhooks/service.rs`, `src/api/handlers/outgoing_webhooks.rs`, `src/models/webhook.rs` (partial) | read in full | B-01 … B-07 |
| Workers | `src/worker/mod.rs`, `src/api/handlers/workers.rs` | read in full | B-08 … B-10 |
| Queue core | `src/queue/mod.rs` lines 1–980 | read | B-11, B-13, B-14, B-15 |
| Scheduler | `src/scheduler/mod.rs` lines 640–1000 | read | B-12, B-16, B-17 |
| Limits / plans | `src/api/middleware/limits.rs`, `migrations/*resource_counts*`, `*atomic_daily_quota*` | read in full | (supporting) |
| Migrations | `20241209000003_job_dependencies`, `20241209000007_worker_column_aliases`, `20241209000010_outgoing_webhooks`, `20260714120000_fix_cancelled_workflow_progress`, `20241217000001_fix_workflows_cascade` | read | B-08, B-12, B-16 |
| Authentication | `src/api/middleware/auth.rs`, `src/models/api_key.rs` | read in full | B-18 … B-21 |
| Rate limiting | `src/api/middleware/plan_rate_limit.rs`, `rate_limit.rs`; `src/api/mod.rs` (routing/layer order) | read in full | B-22, B-23, B-24 |
| Email login | `src/api/handlers/email_login.rs` lines 1–190, 320–460 | read | B-25 … B-28 |
| Quota accounting | `src/api/handlers/jobs.rs` 280–360, 1100–1310; all `increment_daily_jobs` call sites; `migrations/20241209000011_organization_usage.sql` | read | B-29, B-30 |
| SSRF | `src/security/url_validator.rs` (structure + `validate_dns_resolution`), `src/security/http_client.rs` | read | B-31, B-32 |
| Billing | `src/api/handlers/billing.rs` 215–340, 985–1065 | read | B-33, B-34 |
| Realtime | `src/api/handlers/realtime.rs` 1–240 (broadcaster only) | skimmed | B-35 |
| Cache | `src/cache/mod.rs` (`get_connection`, `check_token_bucket`) | skimmed | none |
| Metrics | `src/api/handlers/metrics.rs` (auth guard) | skimmed | B-24 |
| **Not reviewed** | see §4.3 — chiefly `src/grpc/**` (3 000+ lines), `admin.rs`, `organizations.rs`, `schedules.rs`, `workflows.rs`, `queues.rs`, `health.rs`, `webhooks.rs`, `auth.rs`, `api_keys.rs`, `config/plans.rs`, `models/**`, the realtime polling loops, and the whole `tests/` tree | | |
