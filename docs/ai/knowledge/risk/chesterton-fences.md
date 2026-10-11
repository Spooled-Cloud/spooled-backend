# Chesterton fences (backend)

Do not remove without understanding why it exists.

1. **gRPC `max_retries <= 0` → 3** — proto3 cannot distinguish omit vs zero; mapping uses `QUEUE_DEFAULT_*` when `<=0` (since `0.1.107`; instant-DLQ fix landed in `0.1.106`). Changing back to “0 means zero retries” breaks omit clients.
2. **JWT re-checked against DB** — claims alone are insufficient after key revoke/queue-scope change.
3. **Stripe event dedupe + monotonic timestamp** — prevents stale resends re-granting tiers after cancel.
4. **Idempotency unique index filtered WHERE key IS NOT NULL** — allows many jobs without keys without colliding on NULL.
5. **Completion webhook optional HMAC** — default unsigned (existing receivers); `completion_webhook_secret` opts into org-webhook-compatible signing without forcing all consumers.
6. **RLS present but unused** — honesty path chosen over fake security theater; enabling RLS needs GUC-per-query and role changes — large migration.
7. **Rate-limit fail-open by default** — self-host without Redis keeps working; set `RATE_LIMIT_FAIL_CLOSED=true` on hosted SaaS.
8. **Admin constant-time compare after hash** — do not “simplify” to string `==`.
9. **Unverified billing email is ignored by email login** (0.1.117) — do not "simplify" the login lookup back to `billing_email = $1`; that reopens identity squatting (BE-42).
10. **Fall-back repeat guard skipped only for interval schedules** (0.1.117) — minute `*`/`*/N` or hour `*`; a step hour with a fixed minute keeps the guard (else `0 */2` double-fires). The guard must jump past the reference wall clock rather than to the next day (BE-45).
11. **`parent_job_id` is SET NULL, plus the `"A_cancel_blocked_dependents_on_delete"` trigger** (0.1.117) — switching back to CASCADE silently deletes live children; dropping or renaming the trigger (it must sort before `RI_ConstraintTrigger_*`) lets dependents of never-completed jobs run after the orphan sweep.
12. **Legacy `lookup_hash IS NULL` keys are still scanned** (0.1.119, operator decision) — the scan is bounded (bcrypt gate; one scan at a time per transport, REST/login and gRPC on separate turns; a wait that covers one scan at the measured verify speed, capped at 10 s, then 503; concurrent requests for one token share one scan; negative cache; per-IP budget on REST and one shared budget on gRPC, both charged before the scan and refunded when the key is found) rather than removed; do not rotate, deactivate or drop legacy keys, do not gate the record-cache or `lookup_hash` fast path on the budget, do not merge the two scan turns back into one (a gRPC flood then locks REST legacy keys out, W3R-08), and do not answer a scan that never ran with 401 (SDKs do not retry it, W3R-07) (BEAUTH-05, BEGRPC-13).
13. **Oversized jobs are dead-lettered on gRPC claim** (0.1.119) — a job over 4 MiB cannot be sent, and skipping it would block the queue head; do not turn that into a silent skip or an unbounded retry (BEGRPC-04).
14. **gRPC `max_concurrency` is clamped, not rejected** (0.1.119) — proto3 cannot say "unset" and fleets that sent up to 1000 must keep registering; same for the empty hostname and the 255-byte claim-path queue-name limit (BEGRPC-09).
15. **Draining workers count toward the cap only while they heartbeat** (0.1.119) — counting them for ever would let a crashed draining worker hold a slot; not counting them was the bypass (BEGRPC-15).
16. **An omitted gRPC lease is 300 s, not the REST 30 s** (0.1.119, operator decision) — `GRPC_DEFAULT_LEASE_DURATION_SECS` exists because the Go SDK `StreamJobs` cannot send a lease and Go/PHP `Dequeue`/`RenewLease` send 0 unless set; "unifying" it with `WORKER_LEASE_DURATION_SECS` re-delivers their running jobs to a second worker (W3R-03). For the same reason `0` means unset (300), never the 5 s minimum, and a value at or below `WORKER_HEARTBEAT_INTERVAL_SECS` refuses to start.
