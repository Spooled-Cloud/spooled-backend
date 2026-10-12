-- Keep queue defaults inside the range a job may have.
--
-- `PUT /queues/{name}/config` stored `max_retries` and `default_timeout`
-- without validating them. Jobs that omit those fields take the queue's
-- values, so a stored `default_timeout` of 0 or 172800 made every later
-- enqueue to the queue fail the `jobs_timeout_check` (an HTTP 500), and a
-- negative `max_retries` sent each job to the dead-letter queue on its first
-- failure. The API validates both fields now; these checks hold the table to
-- the same ranges.
--
-- Rows written before the fix are brought into range first (the clamp is what
-- the jobs table would have accepted anyway), so the checks validate on every
-- database. queue_config holds one row per configured queue.

SET LOCAL lock_timeout = '5s';

UPDATE queue_config
SET max_retries = LEAST(GREATEST(max_retries, 0), 100),
    updated_at = NOW()
WHERE max_retries < 0 OR max_retries > 100;

UPDATE queue_config
SET default_timeout = LEAST(GREATEST(default_timeout, 1), 86400),
    updated_at = NOW()
WHERE default_timeout < 1 OR default_timeout > 86400;

ALTER TABLE queue_config DROP CONSTRAINT IF EXISTS queue_config_max_retries_check;
ALTER TABLE queue_config
    ADD CONSTRAINT queue_config_max_retries_check
    CHECK (max_retries >= 0 AND max_retries <= 100) NOT VALID;
ALTER TABLE queue_config VALIDATE CONSTRAINT queue_config_max_retries_check;

ALTER TABLE queue_config DROP CONSTRAINT IF EXISTS queue_config_default_timeout_check;
ALTER TABLE queue_config
    ADD CONSTRAINT queue_config_default_timeout_check
    CHECK (default_timeout > 0 AND default_timeout <= 86400) NOT VALID;
ALTER TABLE queue_config VALIDATE CONSTRAINT queue_config_default_timeout_check;
