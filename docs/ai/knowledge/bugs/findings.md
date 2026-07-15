# Verified findings (backend)

Cartography + fix pass: 2026-07-15. Only code-verified items. No amount/formula changes proposed without owner sign-off.

## Open

| ID | Sev | Summary | Evidence | Trigger | Impact | Fix direction |
|----|-----|---------|----------|---------|--------|---------------|
| BE-01 | P1 | ~~gRPC hardcoded defaults~~ **FIXED 0.1.107** — reads `QUEUE_DEFAULT_*` | `src/grpc/services/queue_service.rs` ~401–413 vs `src/config/mod.rs` ~370–377 | Operator sets env default ≠ 3/300; client omits fields | REST/gRPC behavioral split | Read settings in gRPC path (keep ≤0 → default mapping) |
| BE-02 | P1 | REST allows `max_retries=0`; gRPC cannot express 0 | `src/models/job.rs` ~214; gRPC clamp `1..=100` | Client wants no-retry via gRPC | Instant DLQ on REST; impossible on gRPC | Proto optional/wrapper or docs-only contract |
| BE-03 | P2 | ~~ingest/schedule/workflow ignore settings~~ **FIXED 0.1.107** | `handlers/webhooks.rs` ~144; schedules/workflows `unwrap_or(3)` | Env default changed | Ingest paths diverge | Route through settings |
| BE-04 | P2 | `completion_webhook` unsigned | `outgoing_webhooks/service.rs` ~372–400 | Attacker on network path | Spoofable completion callbacks | Optional HMAC parity with org webhooks |
| BE-05 | P2 | Rate limit fail-open on Redis errors (public path) | `plan_rate_limit.rs` ~234 | Redis down | Limits bypassed | Fail-closed for hosted SaaS; document self-host |
| BE-06 | P2 | `past_due` does not revoke plan quotas | `billing.rs` status update; `limits.rs` reads `plan_tier` | Failed invoice, no cancel yet | Paid entitlements during dunning | Owner policy |
| BE-07 | P2 | Enterprise not Stripe-mappable | `billing.rs` `determine_plan_tier` starter/pro only | Enterprise checkout | Tier stuck | Env price id or admin-only |
| BE-08 | P2 | Checkout still trusts `client_reference_id`; **payment_status gate added 0.1.107** (unpaid ignored) | `billing.rs` ~349–351 | Malicious/odd Checkout session | Wrong org link / unpaid grant risk | Verify payment_status + server-side metadata |
| BE-09 | P2 | Dispute events unhandled | no `charge.dispute` in webhook match ~240 | Chargeback | Entitlements until cancel | Handle dispute → suspend |
| BE-10 | P2 | Tracked local key; **mitigated** via `certs/README.md` (disposable/dev) | `certs/grpc-key.pem` header `BEGIN PRIVATE KEY` | Key ever used outside disposable local | Key compromise | Confirm disposable; remove/rotate |
| BE-11 | P3 | Query `?token=`/`?api_key=` on protected routes | `middleware/auth.rs` ~97–111 | Token in URL | Log/Referer leak | Limit to WS/SSE |
| BE-12 | P3 | ~~admin unthrottled~~ **FIXED 0.1.107** Redis IP window 60/min | `api/mod.rs` admin nest | Brute force admin key | Abuse | Rate limit |
| BE-13 | P3 | Email check enumeration | `email_login.rs` exists/available | Probe emails | Account enum | Uniform response |
| BE-14 | P3 | Retention SQL ignores some `SPOOLED_PLAN_*` env windows | `scheduler/mod.rs` ~1010–1017 | Custom env retention | Unexpected purge window | Read settings |
| BE-15 | P3 | Outgoing webhook retry UUID/TEXT bind risk | `outgoing_webhooks.rs` ~504–513 | Retry delivery | 500s | Cast/bind as text |
| BE-16 | P3 | OpenAPI hand drift risk | `docs/openapi.yaml` | API change without yaml | Client codegen lies | CI drift check or codegen |
| BE-17 | P3 | ~~`GET /jobs?queue=` silently ignored~~ **FIXED + live** (Portainer tip Pull 2026-07-15 ~02:39Z) — `serde(alias = "queue")` | `src/models/job.rs` `ListJobsQuery` | Clients using short `queue` param | Unfiltered list (looks like filter broken) | Alias + keep OpenAPI `queue_name` |
| BE-18 | P2 | ~~`GET /jobs/stats` ignored `queue_name`~~ **FIXED + live** (Portainer tip Pull 2026-07-15 ~02:59Z) | `handlers/jobs.rs` `stats` | Client passes queue filter | Always org totals | Bind `JobStatsQuery` + SQL `queue_name = $3` |

## Live gap re-verify (2026-07-15 on `0.1.107`)

| Gap | Result |
|-----|--------|
| DST fall-back / spring-forward | Unit: NY fall-back once; spring-forward skips 02:30 gap; Kyiv EET/EEST (IANA still has DST) + Kyiv fall-back once. Live: Kyiv noon → `09:00Z` (Jul). |
| Cron auto-fire + catch-up | Live: `*/1` auto-fired once; deactivate→125s past due→reactivate → `run_count=1` (skip-missed, run once). |
| ProcessJobs + StreamJobs consume | Live Node: ProcessJobs drained 5/5; double-complete → `NOT_FOUND`; StreamJobs 2/2 complete. |

## P0

None proven this cartography pass against current `0.1.107` (open items remain below).

## Machine-readable

See `findings.jsonl` and `prior-audit-reverify.jsonl`.
