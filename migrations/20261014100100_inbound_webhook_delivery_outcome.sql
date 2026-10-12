-- The inbound webhook log records which job a request resolved to.
--
-- `POST /webhooks/{org_id}/custom` writes one webhook_deliveries row per
-- accepted request, but not the job it made, nor whether the request created
-- that job or matched an existing one through its idempotency key. The log
-- (GET /webhooks/deliveries) shows both now: `job_id`, and `status` 'created'
-- or 'duplicate'. Rows written before this migration keep NULL in both.
--
-- Both columns are nullable without a default, so adding them is a catalog
-- change. The status check is added NOT VALID: it holds for every new or
-- updated row, and the existing rows are all NULL (the column is new), so
-- validating would only scan the log while this transaction holds its lock.

SET LOCAL lock_timeout = '5s';

ALTER TABLE webhook_deliveries ADD COLUMN IF NOT EXISTS job_id TEXT;
ALTER TABLE webhook_deliveries ADD COLUMN IF NOT EXISTS status TEXT;

ALTER TABLE webhook_deliveries DROP CONSTRAINT IF EXISTS webhook_deliveries_status_check;
ALTER TABLE webhook_deliveries
    ADD CONSTRAINT webhook_deliveries_status_check
    CHECK (status IS NULL OR status IN ('created', 'duplicate')) NOT VALID;

COMMENT ON COLUMN webhook_deliveries.job_id IS 'Job the request created or matched (NULL before 0.1.122)';
COMMENT ON COLUMN webhook_deliveries.status IS
    'created: a new job; duplicate: the idempotency key matched an existing job (NULL before 0.1.122)';
