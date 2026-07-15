# Spooled Backend — Agent Knowledge Base

**Repo:** public OSS (`Spooled-Cloud/spooled-backend`)  
**Version at cartography:** `0.1.109` (live re-verified 2026-07-15 ~05:56Z after Portainer `:latest` Pull; tip `f6541ef`)  
**Load this folder before editing queue, auth, billing, gRPC, or webhooks.**

## Read order

1. This file
2. `MAINTENANCE.md`
3. `dataflows/` (jobs, auth, billing, webhooks, grpc-rest)
4. `contracts/invariants.md`
5. `bugs/findings.md` + `bugs/prior-audit-reverify.jsonl`
6. `risk/chesterton-fences.md` + `risk/change-playbook.md`
7. Workspace map (if available): `../../../../docs/ai/workspace/` or sibling workspace `docs/ai/workspace/`

## What this service is

Single-crate Rust API: multi-tenant job queue with leases/retries/DLQ, workers, schedules, workflows, outgoing webhooks, Stripe billing. REST (`/api/v1`) + gRPC (`QueueService` / `WorkerService`). In-process **Scheduler**; production workers are **external** clients.

## Entry points

| Kind | Path |
|------|------|
| Binary | `src/main.rs` |
| REST router | `src/api/mod.rs` |
| gRPC | `proto/spooled.proto`, `src/grpc/` |
| Queue core | `src/queue/mod.rs` |
| Scheduler | `src/scheduler/mod.rs` |
| OpenAPI (hand) | `docs/openapi.yaml` |

## Workspace pointer

Multi-repo rules, live deploy boundary, and prod QA live in the workspace `AGENTS.md` (parent of this repo), not duplicated here.
