# Knowledge Base Maintenance

Update the KB in the **same change** when you touch these areas.

| Code area | KB files to touch |
|-----------|-------------------|
| `src/api/handlers/jobs.rs`, `src/queue/` | `dataflows/jobs-queue.md`, `contracts/invariants.md` |
| `src/grpc/services/queue_service.rs`, `proto/` | `dataflows/grpc-rest.md`, `contracts/invariants.md`, findings if defaults change |
| `src/api/middleware/auth.rs`, `admin_auth.rs` | `dataflows/auth.md` |
| `src/api/handlers/billing.rs`, Stripe migrations | `dataflows/billing-stripe.md`, findings |
| `src/outgoing_webhooks/`, `handlers/webhooks.rs` | `dataflows/webhooks.md` |
| `src/api/middleware/limits.rs`, `config/plans.rs` | `dataflows/billing-stripe.md`, `contracts/invariants.md` |
| `src/scheduler/` | `dataflows/jobs-queue.md`, chesterton fences |
| Defaults / env (`QUEUE_DEFAULT_*`) | `contracts/invariants.md` + workspace `CROSS-REPO-CONTRACTS.md` |
| Security-relevant behavior | `bugs/findings.jsonl` + `findings.md` |

## Rules

1. Prefer path:line citations over narrative.
2. Mark severity P0–P3; money/idempotency/tenant = highest care.
3. Do **not** change billing formulas or plan limits without owner sign-off.
4. Same-artifact version surfaces: `Cargo.toml`, `Cargo.lock` root package, `CHANGELOG.md`, tag — keep aligned before release.
5. Optional hooks that block casual PRs: **not** installed; use this matrix + agent rules instead.
