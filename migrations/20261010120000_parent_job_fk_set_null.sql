-- Stop deleting child jobs when their parent job is deleted.
--
-- `jobs.parent_job_id` referenced `jobs(id)` with ON DELETE CASCADE. Retention
-- removes completed parents after the plan's retention window, expiry removes
-- jobs past `expires_at`, and the DLQ purge removes dead-lettered jobs; each of
-- those silently deleted every child of the removed parent, in any status
-- (scheduled, processing or dead-lettered). The child keeps its own lifecycle
-- now: the link is cleared and the job stays.
--
-- The column had no index either, so every deleted row made Postgres scan the
-- whole jobs table to find referencing rows, and the retention batch (up to
-- 10,000 rows) stalled the scheduler loop on large tables.

CREATE INDEX IF NOT EXISTS idx_jobs_parent_job_id
    ON jobs (parent_job_id)
    WHERE parent_job_id IS NOT NULL;

-- Drop the old foreign key whatever it is called, so a database whose
-- constraint got a non-default name cannot end up with both rules.
DO $$
DECLARE
    fk_name TEXT;
BEGIN
    FOR fk_name IN
        SELECT c.conname
        FROM pg_constraint c
        JOIN pg_attribute a
          ON a.attrelid = c.conrelid AND a.attnum = ANY (c.conkey)
        WHERE c.conrelid = 'jobs'::regclass
          AND c.contype = 'f'
          AND c.confrelid = 'jobs'::regclass
          AND a.attname = 'parent_job_id'
    LOOP
        EXECUTE format('ALTER TABLE jobs DROP CONSTRAINT %I', fk_name);
    END LOOP;
END $$;

ALTER TABLE jobs
    ADD CONSTRAINT jobs_parent_job_id_fkey
    FOREIGN KEY (parent_job_id) REFERENCES jobs(id) ON DELETE SET NULL;

-- A job still waiting on a dependency that is deleted before it completed (an
-- expired pending parent, a queue deleted with its jobs, a purged dead-letter)
-- can never have that dependency met. With the cascade gone it would otherwise
-- sit blocked until the orphan sweep releases it an hour later and runs it as
-- if the dependency had succeeded. Cancel those dependents instead. Dependents
-- of a COMPLETED job are left alone: their dependency was satisfied.
--
-- The trigger name starts with an upper-case "A" on purpose: row triggers for
-- the same event fire in name order, and it must run before the foreign key
-- actions ("RI_ConstraintTrigger_*") clear `parent_job_id` and delete the
-- `job_dependencies` edges it looks up.
CREATE OR REPLACE FUNCTION cancel_blocked_dependents_of_deleted_job()
RETURNS TRIGGER AS $$
BEGIN
    IF OLD.status IS DISTINCT FROM 'completed' THEN
        UPDATE jobs
        SET status = 'cancelled',
            last_error = 'Dependency job ' || OLD.id || ' was deleted before it completed',
            completed_at = NOW(),
            updated_at = NOW()
        WHERE parent_job_id = OLD.id
          AND organization_id = OLD.organization_id
          AND status IN ('pending', 'scheduled')
          AND dependencies_met IS NOT TRUE;

        UPDATE jobs
        SET status = 'cancelled',
            last_error = 'Dependency job ' || OLD.id || ' was deleted before it completed',
            completed_at = NOW(),
            updated_at = NOW()
        WHERE id IN (
                SELECT jd.job_id FROM job_dependencies jd WHERE jd.depends_on_job_id = OLD.id
            )
          AND organization_id = OLD.organization_id
          AND status IN ('pending', 'scheduled')
          AND dependencies_met IS NOT TRUE;
    END IF;
    RETURN OLD;
END;
$$ LANGUAGE plpgsql;

DROP TRIGGER IF EXISTS "A_cancel_blocked_dependents_on_delete" ON jobs;
CREATE TRIGGER "A_cancel_blocked_dependents_on_delete"
    AFTER DELETE ON jobs
    FOR EACH ROW
    EXECUTE FUNCTION cancel_blocked_dependents_of_deleted_job();
