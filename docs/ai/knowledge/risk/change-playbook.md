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
