# Dataflow: Billing / Stripe

## Endpoints

| Route | Auth | Role |
|-------|------|------|
| `POST /api/v1/billing/webhook` | `Stripe-Signature` | Lifecycle events |
| `GET /api/v1/billing/status` | Bearer, **unrestricted key** | Org Stripe/plan fields; a queue-scoped key is 403 `ACCESS_DENIED` (0.1.120, BEBILL-23; `billing.rs:131`) |
| `POST /api/v1/billing/checkout` | Bearer, **unrestricted key** | Server-side Stripe Checkout Session for `starter` / `pro` (0.1.120, BEBILL-02; `billing.rs:434`) |
| `POST /api/v1/billing/portal` | Bearer | Customer portal (allowlisted return URL) |

## Checkout and the trust model (0.1.120, BEBILL-02)

`POST /billing/checkout` (`create_checkout` `billing.rs:434`) body `{"plan": "starter"|"pro"}` creates a Stripe
Checkout Session with the secret key and returns `{url, session_id}`. The session carries
`client_reference_id`, `metadata[spooled_org_id]` and `subscription_data[metadata][spooled_org_id]`, all set
to the **authenticated** org (`CHECKOUT_ORG_METADATA_KEY` `:106`, `checkout_session_form` `:390`). It reuses the
org's `stripe_customer_id` (so the portal keeps showing everything), else sets `customer_email` to the
**verified** billing email. Return URLs: `<FRONTEND_URL or https://spooled.cloud>/account/billing?checkout=success|cancel`
(`checkout_return_base` `:375`; `FRONTEND_URL` is read straight from the environment and forwarded by
`docker-compose.prod.yml`). Errors: 400 `VALIDATION_ERROR` (bad/missing plan or body), 403 queue-scoped key,
404 org missing or soft-deleted, 409 `CONFLICT` when the org already has that plan with an `active`/`trialing`
subscription (the message points to the portal), 503 `SERVICE_UNAVAILABLE` when Stripe, that plan's price id or the
Stripe call is unavailable (`CheckoutError::Unavailable`). A different plan is allowed: the webhook then cancels the
old subscription at once, without proration (the portal is the fairer way to change plans).

**Who may say which org paid** (`checkout_org_ref` `:803`, `handle_checkout_completed` `:828`):

| Session | Created by | Link rule | Cancels? |
|---------|-----------|-----------|----------|
| `metadata.spooled_org_id` present | `POST /billing/checkout` (only this server can set it) | **Trusted**: link that org (customer, subscription, `stripe_last_event_at`) and reconcile its tier from Stripe | Yes: the replaced subscription (see below) |
| only `client_reference_id` | legacy public Payment Link (URL parameter, anyone can edit) | **Unverified**: link only when the org has a **verified** billing email equal (ignoring case) to `customer_details.email` / `customer_email`, **and** no live subscription (none, the same subscription on a retry, or `canceled`/`cancelled`/`incomplete_expired`/`unpaid`) | **Never** |
| neither, or no customer | - | ACK, nothing linked | - |

An unverified checkout that fails the rule is ACKed with a `warn!` `Not linking a Payment Link checkout`
(org id, customer id, subscription id, event id; no email). The customer paid and stays on the old plan until an
operator links them by hand (admin API). Before 0.1.120 anyone who knew an org id could pay through a Payment Link
with `?client_reference_id=<victim org>`, take over that org's billing and have its real subscription cancelled.
A `mode=payment` session (no subscription) keeps the org's subscription id (`COALESCE`).

## Delayed payments (0.1.120, BEBILL-15)

`checkout.session.completed` with `payment_status` not `paid`/`no_payment_required` (SEPA, ACH: `unpaid`) is an
ACKed no-op logged at info; the money arrives later as its own event `checkout.session.async_payment_succeeded`,
which runs the same linker (and is claimed for dedupe under its own event id). `async_payment_failed` is a `warn!`
with ids only and links nothing. The Stripe dashboard endpoint must be subscribed to both `async_payment_*` events
or a delayed checkout is never linked (operator action in the 0.1.120 changelog).

## Replaced subscriptions (0.1.120, BEBILL-14)

Because checkout reuses the customer, one Stripe customer can hold the old and the new subscription.

- `organizations.stripe_superseded_subscription_ids TEXT[]` (migration `20261012100000`) holds subscriptions a
  checkout replaced that Stripe has not confirmed canceled. The **trusted** relink appends the old id in the same
  `UPDATE` that relinks (the old value is read in the `SET` expression); it is never appended when the old status is
  already ended.
- `cancel_superseded_subscriptions` (`billing.rs:987`) cancels each id and removes it (`forget_superseded_subscription`
  `:1027`) only after Stripe confirms: 2xx, 404, or a 4xx where a `GET` shows `canceled`/`incomplete_expired`
  (`cancel_stripe_subscription` `:1238`, `stripe_subscription_has_ended` `:1286`). The first failure is returned, so the
  webhook answers 5xx, releases its claim and Stripe's redelivery finishes the cancel. The pass also runs when the relink
  was skipped as stale, and it never cancels the org's **current** subscription (`warn!` and forgets the id instead).
  `customer.subscription.deleted` also forgets the id.
- `handle_subscription_updated` (`:1040`) refuses an event for a **different** subscription than the org's current one
  while that one is live (anything except `canceled`/`cancelled`/`incomplete_expired`/`unpaid`). Without this,
  `subscription.created(new)` arriving before `checkout.completed` hid the old subscription (never cancelled, double
  billing). Consequence: a second subscription someone creates by hand for an already linked customer is ignored until
  the org is switched over through checkout or the admin API.
- All Stripe calls (checkout, portal, reconcile, cancel, ended check) use one shared `reqwest` client with a 10 s
  timeout and 5 s connect timeout (`stripe_http_client` `:87`); the cancel used to wait 20 s and the others had none.
- Test seam: `billing::with_stripe_api_base(base, fut)` (`:75`) is a tokio **task-local** base URL, so tests point
  Stripe calls at `wiremock`; the service itself never sets it and production always uses `https://api.stripe.com`.
  New billing tests must use it, never a real Stripe key.

## Events before the checkout link: 409, not 500 (0.1.120, BEBILL-25)

`customer.subscription.created/updated`, `invoice.paid` and `invoice.payment_failed` for a customer no org is linked
to yet (and an event younger than 24 h, `should_request_retry_for_unlinked`) answer **409 `CONFLICT`** through
`retry_until_linked` (`:1449`), logged at **info**. Stripe retries any non-2xx, so the retry-until-linked behaviour is
unchanged; it used to be a 500 with an error log on every new subscription. Older orphan events are ACKed.

## Webhook pipeline (`handlers/billing.rs`)

1. Verify signature (multi-`v1` support; skew ~300s; any matching `v1` during secret rotation).
2. Claim event id in `processed_stripe_events` (dedupe migration `20260707000001_stripe_event_dedupe.sql`) **before** handler work; release claim on handler error so Stripe can retry.
3. Handle `checkout.session.completed`, `checkout.session.async_payment_succeeded`, `checkout.session.async_payment_failed`, `customer.subscription.created/updated/deleted`, `invoice.paid`, `invoice.payment_failed` (eight events; the Stripe dashboard endpoint must send all eight).
4. Monotonic guard via `stripe_last_event_at` (`NULL OR <= event.created`) on subscription.* handlers — equal second timestamps both apply (Stripe has no finer signal). invoice.* handlers are excluded from the guard (they neither check nor advance it) so out-of-order invoice.* events cannot suppress subscription.* events. Residual: distinct out-of-order invoice.paid / invoice.payment_failed events can transiently flip org status against each other until the next subscription.* event corrects it (exact re-deliveries still deduped via `processed_stripe_events`).
5. On handler error, release claim so Stripe can retry; answer non-2xx (409 for unlinked-customer events, 5xx for failures such as a failed cancel of a replaced subscription) so events are not ACK-dropped.

### Regression coverage

| Layer | What |
|-------|------|
| Unit (`billing.rs` tests) | Valid HMAC; tampered body; stale/future `t=`; missing `t`/`v1`; multi-`v1` rotation; ordering predicate helper |
| Docker (`tests/stripe_webhook_tests.rs`, `--features docker-tests`) | Signed same-`event_id` replay no-op; older `created` after newer rejected; claim release on 5xx; multi-`v1` E2E |

Residual: same-second `<=` ambiguity; no live Stripe `events resend` proof (fixture secret only).

## Entitlements

Enqueue/resource gates use **plan_tier quotas** (`middleware/limits.rs` + `config/plans.rs`), not a Stripe usage meter API. `GET /organizations/usage` treats a limit of `0` (free workflows) as disabled with `percentage: null`; dividing by zero is NaN/Inf, not a usage percent.  
`past_due` updates subscription status fields but does **not** automatically revoke paid quotas until cancel/downgrade — runbook-dependent (P2 / owner question).

## Known contract gaps (verified)

- Enterprise price id not mapped in `determine_plan_tier` (starter/pro only).
- `checkout.session.completed` checks paid/no-payment status before linking, and since 0.1.120 trusts only the server-set metadata (BE-08 / BEBILL-02 fixed; see "Checkout and the trust model").
- `charge.dispute.*` not handled.
- Outbound Stripe HTTP calls: no pinned `Stripe-Version` header observed.
- **W4-02 (P3, open, owner decision):** two server checkouts paid in two tabs: the older event is skipped as stale (`stripe_last_event_at` guard), so its subscription stays billing and unlinked and is never in the superseded list. The obvious fix (cancel it) would cancel a subscription the customer paid for.
- Invoice handlers are not scoped to the org's current subscription id: with a reused customer, `invoice.payment_failed` for a pending or replaced subscription can mark the org `past_due` (cosmetic, `past_due` keeps paid entitlements). Deferred.
- A trusted checkout for a different plan cancels the previous subscription immediately and without proration (contract C1 only returns 409 for the same plan).

## Organization delete (0.1.117)

Self-serve `DELETE /organizations/{id}` and admin soft/hard delete call
`billing::cancel_subscription_before_org_delete` first; a failed cancel aborts
the delete with 409 (BE-48). A 404 from Stripe counts as already canceled.
Subscription/checkout handlers skip orgs whose `plan_tier = 'deleted'`, so a late
Stripe event cannot resurrect a soft-deleted org. Gap: `cancel_subscription_before_org_delete` cancels only the org's
current subscription, not ids still pending in `stripe_superseded_subscription_ids` (they exist only briefly while a
cancel retry is outstanding); a follow-up should call `cancel_superseded_subscriptions` there, with its own test.

