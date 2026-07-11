# Spooled Cloud — Continuation Handoff

Last updated: 2026-07-11 (second session). This is the working state + remaining plan
for the next agent. Read `AGENTS.md`, `ROOT_DOCS.md`, and `CHANGELOG.md` first.

## Current state

- **Backend: `v0.1.94`** (audit F9 — lease_id fencing, backward compatible) supersedes
  `v0.1.93`; both are **awaiting a production redeploy**. Prod runs
  `docker-compose.prod.yml` pulling `:latest`. `:latest` is rebuilt **only on a push to
  `main`** (the release.yml manifest job), NOT on tag push. Confirm any redeploy actually
  rolled via `GET /api/v1/admin/stats` → `system.uptime_seconds` (small == fresh pod).
- F9 backend surface: claim/dequeue return `lease_id` (REST ClaimedJob + proto Job=19);
  complete/fail/heartbeat/renew take optional `lease_id` (REST body; proto Complete=4,
  Fail=5, RenewLease=4). Provided-but-stale token → 409 LEASE_EXPIRED / gRPC
  FAILED_PRECONDITION; omitted token → legacy worker+expiry fence (old SDKs unaffected).
  Locally verified end-to-end (REST + gRPC unary + ProcessJobs + stale-same-worker
  re-lease scenario) on a real Postgres.
- SDKs published: npm `@spooled/sdk` 1.0.33, go v1.0.18, PyPI `spooled` 1.0.19,
  Packagist `spooled-cloud/spooled` v1.0.15. (python publishes on GitHub Release, not tag.)
  **None of them send `lease_id` yet** — SDK-side F9 (capture from claim, echo on
  complete/fail/heartbeat, gRPC stub regen) is the in-flight work.

## What v0.1.93 fixed (external audit — all 10 findings were verified REAL first)

Queue scope is an intra-org least-privilege boundary; it was only enforced on
enqueue/claim. v0.1.93 enforces it everywhere and closes related authz gaps. Shared
helper: `require_queue_access` in `src/api/handlers/mod.rs`; `ApiKeyContext::{is_unrestricted,
can_grant_queues, queue_scope_filter}` in `src/models/api_key.rs`.

1. REST job read/control (`get`/`list`/`cancel`/`retry`/`boost_priority`/`list_dlq`/
   `retry_dlq`/`purge_dlq`/`batch_status`) now scope-checked (out-of-scope = "not found").
2. REST queue ops (`list`/`get`/`stats`/`update_config`/`pause`/`resume`/`delete`) checked.
3. Workflow job definitions + schedule create/trigger (indirect enqueue) checked.
4. Realtime: WS event fan-out gates each event by the key's scope (was a firehose when
   `queue=None`); SSE job + queue streams enforce scope.
5. gRPC `StreamJobs` + `ProcessJobs` (per message) enforce scope; unary already did.
6. `POST/PUT /api-keys` reject granting `*` or out-of-scope queues (`can_grant_queues`) —
   was a privilege-escalation (a leaked worker key could mint an org-wide key).
7. JWT middleware reads `expires_at` + current `queues` from the DB every request (was
   trusting stale token claims).
8. Email signup enforces `EMAIL_SIGNUP_ENABLED` + `REGISTRATION_MODE`; excludes
   `plan_tier='deleted'` orgs (no revival of soft-deleted orgs).
9. gRPC `ProcessJobs` retry backoff minutes→seconds (v0.1.91 fixed only the unary path).
10. gRPC dequeue paths respect paused queues (`queue_config.enabled`).

**Behavior change:** existing queue-scoped keys are now restricted to their listed
queues (they previously leaked into all queues). Unrestricted keys (`[]` or `*`) unaffected.

Verified against a real local Postgres: a key scoped to `["allowed"]` gets 403 minting a
wildcard/out-of-scope key, 404/contained on `get`/`list`/`cancel` of a `secret`-queue job,
403 on workflow/schedule/pause into `secret`, and works fine on `allowed`. 308 lib tests +
clippy clean.

## Remaining work (priority order)

1. **Verify v0.1.93 + v0.1.94 live on prod after the redeploy** (blocked until the
   operator redeploys). With a throwaway org against `https://api.spooled.cloud`:
   - v0.1.93 scoped-key containment: mint-wildcard → 403; read/cancel/workflow/schedule/
     pause a different queue → 403/404; realtime + gRPC scope.
   - v0.1.94 lease fencing: claim returns `lease_id`; complete/heartbeat/fail with a
     wrong `lease_id` → 409 LEASE_EXPIRED; correct token and token-omitted both succeed.
   Confirm `uptime_seconds` is small first.

2. **F9 SDK side — capture + echo `lease_id` in all 4 SDKs, then release.** Backend
   (v0.1.94) is done and backward compatible. Per-SDK: add `lease_id` to the claimed-job
   type, send it in complete/fail/heartbeat bodies (REST) and Complete/Fail/RenewLease
   (gRPC — regenerate stubs from `spooled-backend/proto/spooled.proto`: python
   `spooled_pb2*`, php `scripts/generate-grpc.php`, go `make generate-proto`, node loads
   the .proto dynamically — copy the updated file), thread it through the worker loops
   (go workers pass only jobID — stash lease on the activeJob struct), bump versions +
   changelogs, release (node/go/php: tag push; python: GitHub Release). Registries are
   immutable — new bug = new version; never re-tag.

3. **Push the frontend F10 copy fix.** `spooled-frontend` commit `3ecb591`
   (`docs(security,privacy): correct overstated DB-TLS and RBAC claims`) is committed
   locally but NOT pushed, to avoid a non-fast-forward against the in-progress homepage
   redesign in the same repo. `spooled-frontend` has a large **uncommitted homepage
   redesign** (Hero/HeroConsole/home components + global.css); do not disturb it. Push
   `3ecb591` when the redesign lands (or coordinate), so both go up together.

4. **Keep auditing.** Each deeper pass in this project has found more real bugs. Continue
   adversarial review (realtime, billing/Stripe, gRPC lifecycle, cron/DST, config matrix)
   and fix what's confirmed. Do not claim "bug-free".

### Known / accepted (do not re-report as new)
- Bogus `sp_live_*` key / `/auth/login` ~seconds latency = the `lookup_hash` backfill
  transient; shrinks as legacy keys re-authenticate (can't backfill offline).
- Workflow-create + manual schedule-trigger credit the daily quota non-atomically
  (bounded by their own resource caps).
- SDK auto-retry checks only the `Idempotency-Key` HEADER, not a body `idempotency_key`
  (node/go/php; python honors body) — reliability gap, not a dup risk.
- Hard 1 MiB payload cap returns 400 VALIDATION_ERROR, not 413 (taxonomy).

## Operator actions (agent CANNOT do these — tell the user)
- **Redeploy `v0.1.93`** to prod (pull the new `:latest`).
- **Rotate `ADMIN_API_KEY`** — exposed in a prior chat transcript.
- **Rotate + secret-mount `certs/grpc-key.pem`** — a self-signed origin gRPC key is
  tracked in git and baked into the image (`Dockerfile: COPY certs /certs`,
  `docker-compose.prod.yml` references it). Add a `.dockerignore`.

## How to build / test / release (backend)
- Local DB: the `.env` `DATABASE_URL` points at `:5433` which is usually DOWN. Use a
  throwaway DB on the local Postgres (`:5432`) + Redis (`:6379`); migrations auto-apply on
  boot. `cargo fmt && rtk cargo check && rtk cargo test --lib && rtk cargo clippy --lib`.
- Release: bump `Cargo.toml` + `Cargo.lock`, prepend `CHANGELOG.md`, commit, tag `vX.Y.Z`,
  `git push origin main` AND `git push origin vX.Y.Z`. The `main` push rebuilds `:latest`.
- Prod tests: create throwaway orgs (slugs `zz-*`), and HARD-DELETE them when done
  (`DELETE /api/v1/admin/organizations/<id>?hard_delete=true`; plain DELETE is a soft
  tombstone). Never touch real accounts. Admin key from `$SPOOLED_ADMIN_KEY`.
