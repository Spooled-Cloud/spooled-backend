# Chesterton fences (backend)

Do not remove without understanding why it exists.

1. **gRPC `max_retries <= 0` → 3** — proto3 cannot distinguish omit vs zero; mapping prevents instant DLQ (fixed in `0.1.106` era). Changing back to “0 means zero retries” breaks omit clients.
2. **JWT re-checked against DB** — claims alone are insufficient after key revoke/queue-scope change.
3. **Stripe event dedupe + monotonic timestamp** — prevents stale resends re-granting tiers after cancel.
4. **Idempotency unique index filtered WHERE key IS NOT NULL** — allows many jobs without keys without colliding on NULL.
5. **Completion webhook unsigned** — intentional best-effort fire-and-forget vs org webhook HMAC path; “add signing” must version payloads for existing consumers.
6. **RLS present but unused** — honesty path chosen over fake security theater; enabling RLS needs GUC-per-query and role changes — large migration.
7. **Rate-limit fail-open when Redis missing** — self-host friendliness; hosted SaaS may want fail-closed.
8. **Admin constant-time compare after hash** — do not “simplify” to string `==`.
