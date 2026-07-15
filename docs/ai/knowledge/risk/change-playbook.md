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
10. After a migration is applied in prod, never redeploy an older image/tag that lacks that migration file (sqlx hard-fails). Day-to-day image = `:latest` from `main`. Tag builds must not overwrite floating `major.minor` GHCR tags (exact version tags only).
