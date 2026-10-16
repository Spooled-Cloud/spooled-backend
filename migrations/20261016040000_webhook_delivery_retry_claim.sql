-- One sender at a time for a webhook delivery retry.
--
-- The automatic retry sleeps, then sends. A manual retry of the same row could
-- send during that sleep, and both writes landed on the delivery (last one
-- wins). A claim taken before the request lets only one of them proceed. The
-- timestamp expires so a sender that dies mid-request does not stick the row.

SET LOCAL lock_timeout = '5s';

ALTER TABLE outgoing_webhook_deliveries
    ADD COLUMN IF NOT EXISTS retry_claim_token BIGINT NOT NULL DEFAULT 0,
    ADD COLUMN IF NOT EXISTS retry_claimed_at TIMESTAMPTZ;

COMMENT ON COLUMN outgoing_webhook_deliveries.retry_claim_token IS
    'Bumped when a retry claims the row. The outcome write must match it.';
COMMENT ON COLUMN outgoing_webhook_deliveries.retry_claimed_at IS
    'When the current claim was taken. NULL means nobody is sending.';
