# Dataflow: gRPC ↔ REST

## Shared domain

Both paths mutate the same Postgres job/worker tables and honor org scoping from API-key auth.

## Auth difference

| | REST | gRPC |
|--|------|------|
| API key | Bearer | `x-api-key` (or Bearer treated as key) |
| JWT | yes | no |
| Admin | `X-Admin-Key` on admin REST only | n/a |

## Default difference (resolved in 0.1.107)

REST and gRPC both read `QUEUE_DEFAULT_*` from settings when the client omits values (gRPC via proto3 `<=0`). Explicit REST `max_retries: 0` remains allowed; gRPC still cannot express zero retries without a proto change.

## Worker field names

The database uses `max_concurrent_jobs`, `current_job_count`, and `created_at`; REST public JSON keeps `max_concurrency`, `current_jobs`, and `registered_at`. Keep handlers and OpenAPI mapped at the boundary so SDKs do not see DB-only column names.

## Streaming

`StreamJobs` / `ProcessJobs` exist on QueueService. Live consume re-verified 2026-07-15 on `0.1.107` (bounded): ProcessJobs dequeue→complete ×5 + stale double-complete rejected; StreamJobs drained ×2. Long-lived streaming under load remains residual operational risk. Prefer bounded streams in tests.

## Proto JSON

camelCase: `jobId`, `leaseId`, etc.
