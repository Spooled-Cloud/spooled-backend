# Dataflow: Billing / Stripe

## Endpoints

| Route | Auth | Role |
|-------|------|------|
| `POST /api/v1/billing/webhook` | `Stripe-Signature` | Lifecycle events |
| `GET /api/v1/billing/status` | Bearer | Org Stripe/plan fields |
| `POST /api/v1/billing/portal` | Bearer | Customer portal (allowlisted return URL) |

## Webhook pipeline (`handlers/billing.rs`)

1. Verify signature (multi-`v1` support; skew ~300s).
2. Claim event id in `processed_stripe_events` (dedupe migration `20260707000001_stripe_event_dedupe.sql`).
3. Handle checkout completed, subscription created/updated/deleted, invoice paid/failed.
4. Monotonic guard via `stripe_last_event_at` where applicable.
5. On handler error, release claim so Stripe can retry; return 5xx for unlinked/recent subscription updates so events are not ACK-dropped.

## Entitlements

Enqueue/resource gates use **plan_tier quotas** (`middleware/limits.rs` + `config/plans.rs`), not a Stripe usage meter API.  
`past_due` updates subscription status fields but does **not** automatically revoke paid quotas until cancel/downgrade — runbook-dependent (P2 / owner question).

## Known contract gaps (verified)

- Enterprise price id not mapped in `determine_plan_tier` (starter/pro only).
- `checkout.session.completed` trusts `client_reference_id` as org id; no `payment_status` check observed.
- `charge.dispute.*` not handled.
- Outbound Stripe HTTP calls: no pinned `Stripe-Version` header observed.
