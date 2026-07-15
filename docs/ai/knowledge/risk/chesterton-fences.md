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
