-- GET /workflows/{id} returned no job `key`. Workflow create used the key only
-- to wire dependencies and never stored it, so a client could not match the
-- jobs it reads back to the steps it defined. Keep the key on the job.
-- Workflows created before this column have no key recorded (NULL).

SET LOCAL lock_timeout = '5s';

ALTER TABLE jobs ADD COLUMN IF NOT EXISTS workflow_job_key TEXT;
