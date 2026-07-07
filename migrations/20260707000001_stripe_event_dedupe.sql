-- Stripe billing webhook idempotency + ordering guard
--
-- 1. processed_stripe_events: dedupe store for already-applied webhook event
--    ids. Stripe re-delivers events (retries for up to ~3 days) and does not
--    guarantee ordering. Rows only need to outlive the retry window; the
--    webhook handler prunes rows older than 30 days opportunistically.
--
-- 2. organizations.stripe_last_event_at: the Stripe-side timestamp
--    (event.created) of the last applied billing event for the org. Handlers
--    refuse to apply events older than this, so a stale re-delivered event can
--    no longer rewrite newer plan/subscription state.

CREATE TABLE IF NOT EXISTS processed_stripe_events (
    event_id TEXT PRIMARY KEY,
    received_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX IF NOT EXISTS idx_processed_stripe_events_received_at
    ON processed_stripe_events (received_at);

ALTER TABLE organizations
    ADD COLUMN IF NOT EXISTS stripe_last_event_at TIMESTAMPTZ;
