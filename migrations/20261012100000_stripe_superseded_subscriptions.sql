-- Remember the subscriptions a checkout replaced until Stripe confirms they are
-- canceled.
--
-- When a checkout created by POST /billing/checkout completes for an org that
-- already pays for a subscription, the webhook relinks the org to the new
-- subscription and cancels the old one. The cancel used to be a single
-- best-effort call after the relink: if it failed (Stripe error, timeout, the
-- request dropped by the 30 s router timeout), the org no longer pointed at the
-- old subscription, so no retry could find it and the customer was billed for
-- both. The relink now records the old id here in the same statement, and the
-- id is removed only after Stripe confirms the cancel (or reports the
-- subscription deleted); until then the webhook answers 5xx and Stripe's retry
-- finishes the cancel.
--
-- An array because a second upgrade can complete before the first cancel has
-- gone through. Constant default: no table rewrite.

SET LOCAL lock_timeout = '5s';

ALTER TABLE organizations
    ADD COLUMN IF NOT EXISTS stripe_superseded_subscription_ids TEXT[] NOT NULL DEFAULT '{}';

COMMENT ON COLUMN organizations.stripe_superseded_subscription_ids IS
    'Stripe subscriptions a checkout replaced that still have to be canceled';
