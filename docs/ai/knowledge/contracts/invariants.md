# Backend invariants / compatibility

1. **Tenant isolation** is application `organization_id` scoping + API-key/JWT queue ACL. RLS is not a live backstop.
2. **Idempotent enqueue** when `idempotency_key` set: unique per org.
3. **At-least-once** processing: leases + retry + DLQ; workers must be idempotent.
4. **gRPC proto3 zeros** for `max_retries` / `timeout_seconds` mean “server default 3/300”, not zero. Documented in enqueue code comments.
5. **REST may send `max_retries: 0`** (instant DLQ on first fail). Clients must not assume REST and gRPC omit-semantics are identical for zero.
6. **Stripe webhook** must verify signature; events must be deduped; prefer 5xx over silent drop for ordering/link races.
7. **Plan limits** gate enqueue/resources; changing `config/plans.rs` or env plan overrides is a money-path change — owner sign-off.
8. **OpenAPI** is hand-maintained — update `docs/openapi.yaml` when changing public REST shapes.
9. **Admin key** never logged; compare constant-time.
10. **Outgoing webhook secrets** optional; completion_webhook URLs are unsigned — callers must not assume HMAC.
