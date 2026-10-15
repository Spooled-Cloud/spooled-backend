# Knowledge Base Maintenance

Update the KB in the **same change** when you touch these areas.

| Code area | KB files to touch |
|-----------|-------------------|
| `src/api/handlers/jobs.rs`, `src/queue/` | `dataflows/jobs-queue.md` (lists, search, history, timeouts), `contracts/invariants.md` (#38, #39) |
| `src/grpc/services/queue_service.rs`, `proto/` | `dataflows/grpc-rest.md`, `contracts/invariants.md`, findings if defaults change |
| `src/api/middleware/auth.rs`, `admin_auth.rs` | `dataflows/auth.md` |
| `src/api/handlers/email_login.rs` | `dataflows/auth.md` |
| `src/api/middleware/client_ip.rs`, `main.rs` (ConnectInfo), any per-address limiter | `dataflows/auth.md` (Client IP), `contracts/invariants.md` #30 |
| `src/api/handlers/billing.rs`, Stripe migrations | `dataflows/billing-stripe.md`, findings |
| `src/outgoing_webhooks/` (service, `events.rs`, `format.rs`), `handlers/webhooks.rs`, `handlers/outgoing_webhooks.rs`, `models/webhook.rs` | `dataflows/webhooks.md`, `contracts/invariants.md` (#37, #41, #42), `docs/guides/webhooks.md` |
| `src/api/middleware/limits.rs`, `config/plans.rs` | `dataflows/billing-stripe.md`, `contracts/invariants.md` |
| Cap-counting SQL (`get_org_resource_counts`), `handlers/api_keys.rs` / `schedules.rs` / `workflows.rs` re-activation and retry, `custom_limits`, soft delete (`admin.rs`) | `contracts/invariants.md` #25-#29 |
| `src/scheduler/` | `dataflows/jobs-queue.md` (retention, sweeps, cron), `dataflows/webhooks.md` (workflow outcome announcements), `contracts/invariants.md` #35, #41, chesterton fences |
| `src/api/handlers/realtime.rs` | `dataflows/auth.md` (Realtime), `contracts/invariants.md` #36 |
| `src/api/handlers/schedules.rs` `history` (`effective_run_outcome`), `models/job.rs` `JobSummary` | `dataflows/jobs-queue.md` (Schedule history follows the job, summaries), `docs/openapi.yaml` (`ScheduleRun`, `JobSummary`) |
| `src/models/api_key.rs` queue scope, any SQL filtering rows by a key's queues, `queue_in_key_scope` | `dataflows/auth.md` (queue scope), `dataflows/grpc-rest.md`, `contracts/invariants.md` #32 |
| Claim queries (`queue/mod.rs` `CLAIM_BATCH_SQL`, gRPC claims), indexes on `jobs` | `contracts/invariants.md` #33 (#39 for `idx_jobs_org_job_type`), `dataflows/jobs-queue.md`, chesterton #24, #28 |
| Defaults / env (`QUEUE_DEFAULT_*`) | `contracts/invariants.md` + workspace `CROSS-REPO-CONTRACTS.md` |
| `src/api/middleware/plan_rate_limit.rs`, `cache/mod.rs` token bucket, CORS in `api/mod.rs` (`cors_exposed_headers`) | `dataflows/auth.md` (Plan rate limiter and headers), `contracts/invariants.md` #31, #43 |
| Triggers on `jobs`/`workers` (`trigger_count_worker_job_outcome`, `update_workflow_progress`, `update_dependent_jobs`), `models/worker.rs` counters, `dead_letter_oversized` | `dataflows/jobs-queue.md` (Worker counters), `dataflows/grpc-rest.md`, `contracts/invariants.md` #40, chesterton fences |
| Routes in `src/api/mod.rs`, `docs/openapi.yaml`, `scripts/check_openapi_drift.py` | `contracts/invariants.md` #8 (the CI `openapi-drift` job fails a route without its documented path) |
| New env setting | `docker-compose.prod.yml` forward, `.env.example`, README env table, `docs/guides/operations.md`, `contracts/invariants.md` #20 |
| Statements the docs make about code: gRPC payload/result limits (`docs/guides/grpc-server.md`), documented response codes (`docs/openapi.yaml`), the `jobs_partitioned` warning (`docs/guides/operations.md`), finding statuses | `tests/docs_contract_tests.rs` pins them to the code; change both together |
| Security-relevant behavior | `bugs/findings.jsonl` + `findings.md` |

## Rules

1. Prefer path:line citations over narrative.
2. Mark severity P0–P3; money/idempotency/tenant = highest care.
3. Do **not** change billing formulas or plan limits without owner sign-off.
4. Same-artifact version surfaces: `Cargo.toml`, `Cargo.lock` root package, `CHANGELOG.md`, tag — keep aligned before release.
5. Optional hooks that block casual PRs: **not** installed; use this matrix + agent rules instead.
