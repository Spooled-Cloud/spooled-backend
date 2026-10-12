-- Per-worker job counters: `workers.jobs_processed` and `workers.jobs_failed`.
--
-- Nothing counted a worker's outcomes, so every worker showed "Processed 0 /
-- Failed 0". A job settles on many paths, most of them raw UPDATEs on `jobs`:
-- REST complete/fail (queue/mod.rs), the gRPC unary and streaming
-- complete/fail SQL (grpc/services/queue_service.rs), the gRPC oversized-job
-- dead-letter and the legacy QueueManager paths. One row trigger on the status
-- transition counts all of them, and any path added later, without each path
-- having to remember an extra statement.
--
-- What counts, for the worker named by OLD.assigned_worker_id in the job's
-- organization:
--   * processing -> completed: jobs_processed + 1;
--   * processing -> failed or deadletter, or processing -> pending/scheduled
--     with retry_count raised (a retry scheduled): jobs_failed + 1;
-- and only while the lease was live (OLD.lease_expires_at > now(), now() being
-- the same instant the statement used). Every worker call requires a live
-- lease; every sweep requires an expired or missing one (lease-expiry
-- recovery, offline-worker release and dead-letter), so a sweep never counts
-- as a failure of the worker. A release that keeps retry_count (worker
-- deregistration, an undelivered gRPC claim) counts nothing, and neither does
-- the gRPC dead-letter of a job too large for a gRPC message: that is the
-- server's decision, so its transaction first expires the lease
-- (lease_expires_at = now()) and then dead-letters the job. The two counters
-- are disjoint: a job retried twice and then completed adds 2 failures and
-- 1 processed.
--
-- Deferred to commit: the trigger is a constraint trigger, DEFERRABLE
-- INITIALLY DEFERRED, so the worker row is the last lock a settling
-- transaction takes and is held only through its commit. The WHEN clause is
-- still evaluated at the row update, on that update's OLD and NEW rows (now()
-- is the transaction's start either way); only the counting UPDATE waits for
-- COMMIT. As an ordinary AFTER trigger it fired before
-- trigger_update_dependent_jobs and trigger_update_workflow_progress (AFTER
-- triggers run in name order), so settling a job locked the job, then the
-- worker row, then the job's dependents: with dependency_mode 'any', one
-- worker completing a job and a dependent it was already running at the same
-- instant deadlocked (225 of 300 rounds in a two-session race; 0 deferred). A
-- failure in the counting UPDATE now fails the COMMIT instead of the
-- statement.
--
-- Cost: an UPDATE that does not set `status` never fires the trigger, and for
-- one that does, the WHEN clause is evaluated by the executor without calling
-- the function (20k lease renewals: 1048 ms with the trigger, 1048 ms without).
-- A counted transition queues one event and adds one primary-key UPDATE of the
-- worker row at commit. pgbench on PG16 (local SSD), one completion per
-- transaction: 1 client 1216 vs 1414 tps; 8 clients over 200 workers 8963 vs
-- 10094 tps (about 11%). Completions of ONE worker id still serialize on its
-- row, but only for the commit: 8 clients on one worker 8797 vs 10217 tps
-- (2040 vs 9930 while the row was held from the job's UPDATE), the same kind
-- of ceiling the per-organization daily-jobs counter row puts on enqueues.
-- Lock order is jobs rows (and whatever the other job triggers lock), then
-- workers rows at commit; no transaction locks a workers row and then a jobs
-- row.
--
-- Existing workers start at 0: past failures cannot be attributed (a failed
-- job's assigned_worker_id is cleared). Re-registering a worker id keeps its
-- counters.
--
-- Locks: the trigger first (CREATE CONSTRAINT TRIGGER takes SHARE ROW
-- EXCLUSIVE on jobs, which blocks writers only), then the columns (ACCESS
-- EXCLUSIVE on the small workers table; ADD COLUMN with a constant default is
-- catalog-only, no rewrite). Nothing can fire the trigger before the columns
-- exist: jobs writers wait for this transaction. Idempotent: a constraint
-- trigger has no CREATE OR REPLACE, so a re-run drops the trigger first. On a
-- first run DROP TRIGGER IF EXISTS finds nothing and locks nothing; a re-run
-- (or a database that ran an earlier draft of this file) drops it under ACCESS
-- EXCLUSIVE on jobs, bounded by the lock_timeout.

SET LOCAL lock_timeout = '5s';

CREATE OR REPLACE FUNCTION count_worker_job_outcome()
RETURNS TRIGGER AS $$
BEGIN
    IF NEW.status = 'completed' THEN
        UPDATE workers
        SET jobs_processed = jobs_processed + 1
        WHERE id = OLD.assigned_worker_id
          AND organization_id = OLD.organization_id;
    ELSE
        UPDATE workers
        SET jobs_failed = jobs_failed + 1
        WHERE id = OLD.assigned_worker_id
          AND organization_id = OLD.organization_id;
    END IF;
    RETURN NULL;
END;
$$ LANGUAGE plpgsql;

COMMENT ON FUNCTION count_worker_job_outcome() IS
    'Counts a worker-reported job outcome on workers.jobs_processed / jobs_failed (trigger_count_worker_job_outcome)';

DROP TRIGGER IF EXISTS trigger_count_worker_job_outcome ON jobs;

CREATE CONSTRAINT TRIGGER trigger_count_worker_job_outcome
    AFTER UPDATE OF status ON jobs
    DEFERRABLE INITIALLY DEFERRED
    FOR EACH ROW
    WHEN (
        OLD.status = 'processing'
        AND OLD.assigned_worker_id IS NOT NULL
        AND OLD.lease_expires_at > now()
        AND (
            NEW.status IN ('completed', 'failed', 'deadletter')
            OR (NEW.status IN ('pending', 'scheduled') AND NEW.retry_count > OLD.retry_count)
        )
    )
    EXECUTE FUNCTION count_worker_job_outcome();

ALTER TABLE workers
    ADD COLUMN IF NOT EXISTS jobs_processed BIGINT NOT NULL DEFAULT 0,
    ADD COLUMN IF NOT EXISTS jobs_failed BIGINT NOT NULL DEFAULT 0;

COMMENT ON COLUMN workers.jobs_processed IS
    'Jobs this worker completed (counted while its lease was live)';
COMMENT ON COLUMN workers.jobs_failed IS
    'Failures this worker reported: retries scheduled and dead-letters; lease-expiry and offline sweeps do not count';
