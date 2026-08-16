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
10. **Outgoing webhook secrets** optional; per-job `completion_webhook` is unsigned unless `completion_webhook_secret` is set (then same HMAC as org webhooks). On `PUT /api/v1/outgoing-webhooks/{id}` the org-webhook `secret` is **three-state**: omitted keeps the stored secret, explicit `null` clears it (deliveries go out unsigned), a string replaces it. Collapsing that back to "null means no change" reintroduces the un-removable-secret bug.
11. **Worker registration is an upsert on `worker_id`** when the client supplies one; omitting it mints a UUID per registration. Both shapes must keep working — a stable id must not be double-charged against the plan worker cap, and an id owned by another org must stay a 409.
