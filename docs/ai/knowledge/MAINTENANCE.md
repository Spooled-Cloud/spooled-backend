# Knowledge Base Maintenance

Update the KB in the **same change** when you touch these areas.

| Code area | KB files to touch |
|-----------|-------------------|
| `src/api/handlers/jobs.rs`, `src/queue/` (idempotency scope on create and bulk, retry of a cancelled workflow's job, DLQ retry filter) | `dataflows/jobs-queue.md` (lists, search, history, timeouts, Round-2 fixes 0.1.128), `contracts/invariants.md` (#38, #39) |
| `src/grpc/services/queue_service.rs` (Enqueue idempotency scope and new-queue lock, ProcessJobs queue names), `proto/` | `dataflows/grpc-rest.md` (Parity fixes 0.1.128), `contracts/invariants.md`, findings if defaults change |
| `src/api/middleware/auth.rs` (`authenticate_jwt_token`, `cache_api_key`), `admin_auth.rs`, `api/handlers/auth.rs` (`is_token_revoked`, logout, `/auth/me`, `/auth/validate`), table `revoked_sessions` | `dataflows/auth.md` (Sessions and logout, API key resolution), `contracts/invariants.md` #36 |
| `src/api/handlers/email_login.rs` (`complete_signup`), `handlers/organizations.rs` (`create`), billing-email changes and restore in `admin.rs` (`revoke_email_login_identity`, `update_organization`) | `dataflows/auth.md` (Billing email, Organization create and restore 0.1.128) |
| `src/api/middleware/client_ip.rs`, `main.rs` (ConnectInfo), any per-address limiter | `dataflows/auth.md` (Client IP), `contracts/invariants.md` #30 |
| `src/api/handlers/billing.rs` (subscription and invoice handlers, `create_checkout` operator-plan 409, org delete cancels), Stripe migrations | `dataflows/billing-stripe.md`, findings |
| `src/outgoing_webhooks/` (service, retry claim, `events.rs`, `format.rs`), `security/http_client.rs` (no redirects), `handlers/webhooks.rs`, `handlers/outgoing_webhooks.rs`, `models/webhook.rs`, table `outgoing_webhook_deliveries` | `dataflows/webhooks.md`, `contracts/invariants.md` (#37, #41, #42), `docs/guides/webhooks.md` |
| `src/api/middleware/limits.rs`, `config/plans.rs` | `dataflows/billing-stripe.md`, `contracts/invariants.md` |
| Cap-counting SQL (`get_org_resource_counts`), `handlers/api_keys.rs` (list paging, re-activation) / `schedules.rs` / `workflows.rs` re-activation and retry, the `queues` lock (`queue_name_exists`) taken by every path that opens a queue, `custom_limits`, soft delete (`admin.rs`) | `contracts/invariants.md` #25-#29 |
| `src/scheduler/` (cron clock, expiry sweep and the workflows it finishes, any-mode dependency sweeps), trigger `cancel_blocked_dependents_of_deleted_job` | `dataflows/jobs-queue.md` (retention, sweeps, cron, Round-2 fixes 0.1.128), `dataflows/webhooks.md` (workflow outcome announcements), `contracts/invariants.md` #35, #41, chesterton fences |
| `src/api/handlers/realtime.rs` (`ClientCommand`, subscription filter list, SSE channel loss) | `dataflows/auth.md` (Realtime, WebSocket commands), `contracts/invariants.md` #36, `docs/openapi.yaml` (WebSocket, `/events`) |
| `src/queue/mod.rs` (`fail_by_worker`, `release_jobs_for_deregistered_worker`, dependency recompute), scheduler lease-expiry / offline requeues and cron (`is_claimable_queue_name`), `models/{job,schedule,workflow,webhook}.rs` queue-name rules | `dataflows/jobs-queue.md` (Job lifecycle fixes 0.1.126), `dataflows/grpc-rest.md`, `docs/openapi.yaml`, `docs/guides/{jobs,retries,workers}.md` |
| `src/api/handlers/schedules.rs` `history` (`effective_run_outcome`), `models/job.rs` `JobSummary` | `dataflows/jobs-queue.md` (Schedule history follows the job, summaries), `docs/openapi.yaml` (`ScheduleRun`, `JobSummary`) |
| `src/api/handlers/health.rs` (`dashboard_data`: job, queue and worker summaries), `models/workflow.rs` `WorkflowProgress`, workflow create limits | `dataflows/jobs-queue.md` (Round-2 fixes 0.1.128), `docs/openapi.yaml` (`DashboardData`, `JobSummaryStats`, `CreateWorkflowRequest`) |
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
