# Verified findings (backend)

Cartography date: 2026-07-15. Only code-verified items. No amount/formula changes proposed without owner sign-off.

## Open

| ID | Sev | Summary | Evidence | Trigger | Impact | Fix direction |
|----|-----|---------|----------|---------|--------|---------------|
| BE-01 | P1 | gRPC enqueue defaults hardcoded 3/300; ignore `QUEUE_DEFAULT_*` | `src/grpc/services/queue_service.rs` ~401–413 vs `src/config/mod.rs` ~370–377 | Operator sets env default ≠ 3/300; client omits fields | REST/gRPC behavioral split | Read settings in gRPC path (keep ≤0 → default mapping) |
| BE-02 | P1 | REST allows `max_retries=0`; gRPC cannot express 0 | `src/models/job.rs` ~214; gRPC clamp `1..=100` | Client wants no-retry via gRPC | Instant DLQ on REST; impossible on gRPC | Proto optional/wrapper or docs-only contract |
| BE-03 | P2 | Custom webhook / schedules / workflows ignore settings defaults | `handlers/webhooks.rs` ~144; schedules/workflows `unwrap_or(3)` | Env default changed | Ingest paths diverge | Route through settings |
| BE-04 | P2 | `completion_webhook` unsigned | `outgoing_webhooks/service.rs` ~372–400 | Attacker on network path | Spoofable completion callbacks | Optional HMAC parity with org webhooks |
| BE-05 | P2 | Rate limit fail-open on Redis errors (public path) | `plan_rate_limit.rs` ~234 | Redis down | Limits bypassed | Fail-closed for hosted SaaS; document self-host |
| BE-06 | P2 | `past_due` does not revoke plan quotas | `billing.rs` status update; `limits.rs` reads `plan_tier` | Failed invoice, no cancel yet | Paid entitlements during dunning | Owner policy |
| BE-07 | P2 | Enterprise not Stripe-mappable | `billing.rs` `determine_plan_tier` starter/pro only | Enterprise checkout | Tier stuck | Env price id or admin-only |
| BE-08 | P2 | Checkout trusts `client_reference_id`; no `payment_status` check | `billing.rs` ~349–351 | Malicious/odd Checkout session | Wrong org link / unpaid grant risk | Verify payment_status + server-side metadata |
| BE-09 | P2 | Dispute events unhandled | no `charge.dispute` in webhook match ~240 | Chargeback | Entitlements until cancel | Handle dispute → suspend |
| BE-10 | P2 | Tracked `certs/grpc-key.pem` private key in public repo | `certs/grpc-key.pem` header `BEGIN PRIVATE KEY` | Key ever used outside disposable local | Key compromise | Confirm disposable; remove/rotate |
| BE-11 | P3 | Query `?token=`/`?api_key=` on protected routes | `middleware/auth.rs` ~97–111 | Token in URL | Log/Referer leak | Limit to WS/SSE |
| BE-12 | P3 | Admin routes unthrottled | `api/mod.rs` admin nest | Brute force admin key | Abuse | Rate limit |
| BE-13 | P3 | Email check enumeration | `email_login.rs` exists/available | Probe emails | Account enum | Uniform response |
| BE-14 | P3 | Retention SQL ignores some `SPOOLED_PLAN_*` env windows | `scheduler/mod.rs` ~1010–1017 | Custom env retention | Unexpected purge window | Read settings |
| BE-15 | P3 | Outgoing webhook retry UUID/TEXT bind risk | `outgoing_webhooks.rs` ~504–513 | Retry delivery | 500s | Cast/bind as text |
| BE-16 | P3 | OpenAPI hand drift risk | `docs/openapi.yaml` | API change without yaml | Client codegen lies | CI drift check or codegen |

## P0

None proven this cartography pass against current `0.1.106`.

## Machine-readable

See `findings.jsonl` and `prior-audit-reverify.jsonl`.
