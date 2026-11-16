-- A dependency's completion only touches dependents that are still waiting.
--
-- update_dependent_jobs recomputed dependencies_met of every dependent of a
-- completed job, in any status. The jobs_updated_at trigger stamps
-- updated_at on every UPDATE of jobs, so a dependent that had long finished
-- (completed, failed, dead-lettered, cancelled) got a fresh updated_at each
-- time another of its dependencies completed. Retention, the dashboard's
-- failed_24h and DLQ purge older_than age finished jobs by updated_at, so
-- such a job was kept longer, counted as a recent failure, or escaped a purge
-- (CORE-7).
--
-- Only pending and scheduled dependents are gated by dependencies_met, so
-- only they are recomputed. A retry of a finished dependent recomputes its
-- flag itself (job, DLQ and workflow retry all call
-- check_job_dependencies_met).
--
-- Everything else is as defined by 20241209000003_job_dependencies.

SET LOCAL lock_timeout = '5s';

CREATE OR REPLACE FUNCTION update_dependent_jobs()
RETURNS TRIGGER AS $$
BEGIN
    IF NEW.status = 'completed' AND (OLD.status IS NULL OR OLD.status != 'completed') THEN
        UPDATE jobs
        SET
            dependencies_met = check_job_dependencies_met(id),
            updated_at = NOW()
        WHERE id IN (
            SELECT job_id FROM job_dependencies WHERE depends_on_job_id = NEW.id
        )
          AND status IN ('pending', 'scheduled');
    END IF;

    RETURN NEW;
END;
$$ LANGUAGE plpgsql;
