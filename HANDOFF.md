# Spooled Cloud — Continuation Handoff

Last updated: 2026-07-11 (third session). This is the working state + remaining plan
for the next agent. Read `AGENTS.md`, `ROOT_DOCS.md`, and `CHANGELOG.md` first.

## Current state

- **Backend: `v0.1.94` is LIVE on prod** (verified earlier 2026-07-11). **`v0.1.95`
  is committed locally** (billing / soft-delete / gRPC-stream hardening) and needs
  tag + `git push origin main` + `git push origin v0.1.95` so `:latest` rebuilds.
  Confirm redeploy via `GET /api/v1/admin/stats` → `system.uptime_seconds` (small ==
  fresh pod) and pod log `Starting Spooled Backend v0.1.95`.
- F9 lease fencing + queue-scope containment remain live on v0.1.94 (see below).
- **SDKs published WITH F9 lease_id support**: npm `@spooled/sdk` 1.0.34, go v1.0.19,
  PyPI `spooled` 1.0.20, Packagist `spooled-cloud/spooled` v1.0.16.

## What v0.1.95 fixes (this session — code verified; prod NOT yet redeployed)

1. **Stripe cancel → paid tier restore.** `subscription.updated` with terminal
   status (`canceled`/`unpaid`/`incomplete_expired`) forced `plan_tier=free` so it
   cannot COALESCE a price-mapped paid tier back after `subscription.deleted`.
2. **Portal `return_url` open redirect.** Allowlisted to `CORS_ALLOWED_ORIGINS`.
3. **Soft-delete revival.** Email verify rejects deleted/missing org; admin key
   mint rejects deleted orgs; REST+gRPC auth join excludes `plan_tier='deleted'`.
4. **gRPC stream revoke lag.** StreamJobs ~30s re-auth + per-poll rate limit;
   ProcessJobs re-auth every 32 msgs; empty `worker_id` + 1 MiB result cap on
   ProcessJobs complete.

Local: `cargo fmt` + `cargo test --lib` (311) + `cargo clippy --lib -D warnings` green.

## What v0.1.93 / v0.1.94 fixed (external audit — LIVE on prod)

Queue scope + F9 lease fencing. Details in CHANGELOG. Prod-smoked 2026-07-11
(throwaway zz-* orgs hard-deleted).

## Remaining work (priority order)

1. **Ship v0.1.95** — tag + push main + tag; wait for `:latest`; live-verify:
   - Soft-delete: create zz-* org, start email code, soft-delete, verify → reject;
     admin mint key on deleted → 400; surviving key (if any) → 401.
   - Portal: `POST /billing/portal` with `return_url=https://evil.example/` → 400.
   - (Stripe cancel race needs Stripe test-mode or fixture; covered by unit tests.)
2. **Keep auditing.** Next highest-value unexplored:
   - Outgoing webhooks SSRF/signing
   - Dashboard API surface (spooled-dashboard → backend)
   - Config: staging CORS=Any; `EMAIL_SIGNUP_ENABLED` vs `POST /organizations`
   - StreamJobs claim-then-send lease leak on disconnect (eager release?)
   - Per-org gRPC stream caps
3. **Frontend:** ask user what dark-mode punch-list item **9)** was (cut off).
   Re-check `/status`, `/login`, `/signup`, `/account`, `/docs/*`, `/compare/*`
   both themes via computed styles (screenshotter blanks when scrolled).
4. **Known/accepted sweep** — still deferred unless product asks:
   - SDK body-`idempotency_key` retry gap (node/go/php)
   - 1 MiB cap → 400 not 413

### Known / accepted (do not re-report as new)
- Bogus `sp_live_*` key / `/auth/login` ~seconds latency = the `lookup_hash` backfill
  transient; shrinks as legacy keys re-authenticate (can't backfill offline).
- Workflow-create + manual schedule-trigger credit the daily quota non-atomically
  (bounded by their own resource caps).
- SDK auto-retry checks only the `Idempotency-Key` HEADER, not a body `idempotency_key`
  (node/go/php; python honors body) — reliability gap, not a dup risk.
- Hard 1 MiB payload cap returns 400 VALIDATION_ERROR, not 413 (taxonomy).
- Soft-delete → API-key cache ≤60s race (mitigated by revoke + invalidation).
- `StreamJobs` disconnect after claim → lease until expiry (scheduler ~10s after).
- Staging CORS permissive; `EMAIL_SIGNUP_ENABLED=false` does not gate `POST /organizations`.

## Operator actions (agent CANNOT do these — tell the user)
- **Rotate `ADMIN_API_KEY`** — exposed in a prior chat transcript (also stored in the
  workspace-root `.env`, which is outside any git repo; rotate + update both).
- **Rotate + secret-mount `certs/grpc-key.pem`** — a self-signed origin gRPC key is
  tracked in git and baked into the image (`Dockerfile: COPY certs /certs`,
  `docker-compose.prod.yml` references it). Add a `.dockerignore`.
- **Redeploy / push v0.1.95** if the agent committed but did not push (or if CI
  needs a manual nudge).

## How to build / test / release (backend)
- Local DB: the `.env` `DATABASE_URL` points at `:5433` which is usually DOWN. Use a
  throwaway DB on the local Postgres (`:5432`) + Redis (`:6379`); migrations auto-apply on
  boot. `cargo fmt && rtk cargo check && rtk cargo test --lib && rtk cargo clippy --lib`.
- Release: bump `Cargo.toml` + `Cargo.lock`, prepend `CHANGELOG.md`, commit, tag `vX.Y.Z`,
  `git push origin main` AND `git push origin vX.Y.Z`. The `main` push rebuilds `:latest`.
- Prod tests: create throwaway orgs (slugs `zz-*`), and HARD-DELETE them when done
  (`DELETE /api/v1/admin/organizations/<id>?hard_delete=true`; plain DELETE is a soft
  tombstone). Never touch real accounts. Admin key from `$SPOOLED_ADMIN_KEY` /
  `ADMIN_API_KEY` in workspace-root `.env` (never print it).
