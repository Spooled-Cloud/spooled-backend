# Dataflow: gRPC ↔ REST

## Shared domain

Both paths mutate the same Postgres job/worker tables and honor org scoping from API-key auth.

## Auth difference

| | REST | gRPC |
|--|------|------|
| API key | Bearer | `x-api-key` (or Bearer treated as key) |
| JWT | yes | no |
| Admin | `X-Admin-Key` on admin REST only | n/a |

## Default difference (P1)

REST reads `QUEUE_DEFAULT_*` from settings. gRPC hardcodes 3/300 when field `<=0`. If an operator sets `QUEUE_DEFAULT_MAX_RETRIES=5`, REST omit gets 5; gRPC omit still gets 3.

## Streaming

`StreamJobs` / `ProcessJobs` exist on QueueService. Long-lived streaming under load remains residual operational risk (workspace boundary notes). Prefer bounded streams in tests.

## Proto JSON

camelCase: `jobId`, `leaseId`, etc.
