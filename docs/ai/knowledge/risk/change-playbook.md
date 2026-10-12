# Change playbook (backend)

1. Identify surface: REST, gRPC, both, or worker-facing.
2. Check `contracts/invariants.md` and workspace `CROSS-REPO-CONTRACTS.md`.
3. If defaults / auth / billing / idempotency: add or update tests under `tests/` (`integration_tests`, `security_tests`, `real_api_tests`).
4. Update hand `docs/openapi.yaml` for REST shape changes.
5. If proto changes: regenerate; bump carefully; update all four SDKs in coordinated PRs.
6. Update `docs/ai/knowledge/` per `MAINTENANCE.md`.
7. Validate: `cargo fmt`, `cargo test` / nextest, `cargo clippy` per CI.
8. Release: sync `Cargo.toml` version + changelog + tag; deploy separately; verify with authenticated dashboard version — not `/health` alone.
9. No Portainer Pull from agent sessions without operator OK.
10. Since 0.1.119 an older image boots against a database a newer release already migrated (`run_migrator` uses `set_ignore_missing`; checksums of known migrations are still validated), but the older code still has to work on the newer schema, so keep migrations backward compatible for one release (add before use, drop only after). Day-to-day image = `:latest` from `main`. Tag builds must not overwrite floating `major.minor` GHCR tags (exact version tags only). Images before 0.1.119 still hard-fail with `previously applied but is missing`.
11. New migrations: `SET LOCAL lock_timeout = '5s'` first; foreign keys `NOT VALID`, then `VALIDATE CONSTRAINT` separately; never edit an applied file. The `build.rs` `rerun-if-changed=migrations` line makes a new `.sql` alone trigger an embed.
12. A new setting the app reads must also be forwarded in `docker-compose.prod.yml` (`${NAME:-default}`; blank counts as unset) and added to the guard list in `config::tests::prod_compose_forwards_every_operator_setting`; a new request-path bcrypt goes through `bcrypt_gate::verify`; a transaction holding an advisory lock uses `_conn` helpers, never a second pool connection.
13. Stripe code: every outbound call goes through `stripe_http_client()` (10 s timeout) and `stripe_api_url()`; tests point it at `wiremock` with `billing::with_stripe_api_base` (a task-local) and never use a real key. A change to who may link an org, or to what gets cancelled, is a money-path change: owner sign-off (MAINTENANCE rule 3).
14. A migration that redefines `get_org_resource_counts` starts from the newest definition (`20261012110000`), or it drops the system-key, workflow-status and draining-worker rules. A new `custom_limits` key goes into `OVERRIDE_KEYS` (`config/plans.rs`) only, and is added to README, OpenAPI and the admin editor; SQL reads retention overrides through `custom_retention_days()`.
15. A handler that limits by client address takes `PeerAddr` and calls `client_ip_from_peer`; a new env var the app reads is forwarded in `docker-compose.prod.yml` (item 12), as `TURNSTILE_SECRET_KEY` and `FRONTEND_URL` are.
16. A query that filters rows by an API key's queues uses `queue_in_key_scope(queue_name, $scope::TEXT[])` bound to `ctx.queue_scope_filter()` (invariant 32); a change to a claim query or to `idx_jobs_claim` changes both (invariant 33); a retention statement that deletes jobs keeps its workflow, dependent and `SKIP LOCKED` guards (invariant 35) and any new foreign key to `jobs(id)` is indexed.
