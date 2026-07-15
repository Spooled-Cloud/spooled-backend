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

## Streaming

`StreamJobs` / `ProcessJobs` exist on QueueService. Long-lived streaming under load remains residual operational risk (workspace boundary notes). Prefer bounded streams in tests.

## Proto JSON

camelCase: `jobId`, `leaseId`, etc.
