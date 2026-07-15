-- Optional HMAC secret for per-job completion_webhook (parity with org outgoing webhooks).
-- When NULL/empty, delivery stays unsigned (backward compatible).
ALTER TABLE jobs
    ADD COLUMN IF NOT EXISTS completion_webhook_secret TEXT;
