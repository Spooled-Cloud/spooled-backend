-- A deleted dependency must not let a workflow retry run its dependents.
--
-- When a workflow step dead-letters, the dependency sweep cancels its
-- dependents with a sweep reason ('Dependency job failed or was
-- deadlettered', 'Parent job failed or was deadlettered'), which
-- POST /workflows/{id}/retry re-opens. POST /jobs/dlq/purge and
-- DELETE /queues/{name}?delete_jobs=true then deleted the dead-lettered step:
-- its job_dependencies edges cascaded away, so the retry re-opened the
-- cancelled dependent with check_job_dependencies_met = TRUE and it ran
-- although its dependency never succeeded (CORE-2). Retention already keeps
-- such steps while their workflow exists (W5-R5); user-initiated deletes did
-- not.
--
-- cancel_blocked_dependents_of_deleted_job already cancels pending and
-- scheduled dependents of a job deleted before it completed, with the reason
-- 'Dependency job <id> was deleted before it completed', which no retry
-- re-opens. It now also gives that reason to dependents the sweep had already
-- cancelled, so they stay cancelled: the dependency they waited for is gone
-- and can never complete. Their status and completed_at are left as they
-- were. An any-join keeps its sweep reason while it still has another
-- dependency (that one may be retried and complete).
--
-- Everything else is as defined by 20261016120000_any_mode_delete_dependents.

SET LOCAL lock_timeout = '5s';

CREATE OR REPLACE FUNCTION cancel_blocked_dependents_of_deleted_job()
RETURNS TRIGGER AS $$
DECLARE
    v_reason TEXT := 'Dependency job ' || OLD.id || ' was deleted before it completed';
    v_sweep_reasons TEXT[] := ARRAY[
        'Dependency job failed or was deadlettered',
        'Parent job failed or was deadlettered'
    ];
BEGIN
    IF OLD.status IS DISTINCT FROM 'completed' THEN
        -- The dependents are found through the parent_job_id and
        -- job_dependencies indexes (a UNION, not an OR, which would scan the
        -- org's whole pending backlog for every deleted row). Rows are locked
        -- in id order within one call; NO KEY UPDATE does not block inserts
        -- that reference these rows by foreign key.
        PERFORM 1
        FROM jobs
        WHERE id IN (
                SELECT child.id FROM jobs child WHERE child.parent_job_id = OLD.id
                UNION
                SELECT jd.job_id FROM job_dependencies jd WHERE jd.depends_on_job_id = OLD.id
            )
          AND organization_id = OLD.organization_id
          AND (
              (status IN ('pending', 'scheduled') AND dependencies_met IS NOT TRUE)
              OR (status = 'cancelled' AND last_error = ANY(v_sweep_reasons))
          )
        ORDER BY id
        FOR NO KEY UPDATE;

        UPDATE jobs
        SET status = 'cancelled',
            last_error = v_reason,
            completed_at = NOW(),
            updated_at = NOW()
        WHERE parent_job_id = OLD.id
          AND organization_id = OLD.organization_id
          AND status IN ('pending', 'scheduled')
          AND dependencies_met IS NOT TRUE
          AND (
              COALESCE(dependency_mode, 'all') <> 'any'
              OR NOT EXISTS (
                  SELECT 1
                  FROM job_dependencies jd
                  JOIN jobs dep ON dep.id = jd.depends_on_job_id
                  WHERE jd.job_id = jobs.id
                    AND dep.id <> OLD.id
                    AND dep.organization_id = jobs.organization_id
                    AND dep.status NOT IN ('failed', 'deadletter', 'cancelled')
              )
          );

        UPDATE jobs
        SET status = 'cancelled',
            last_error = v_reason,
            completed_at = NOW(),
            updated_at = NOW()
        WHERE id IN (
                SELECT jd.job_id FROM job_dependencies jd WHERE jd.depends_on_job_id = OLD.id
            )
          AND organization_id = OLD.organization_id
          AND status IN ('pending', 'scheduled')
          AND dependencies_met IS NOT TRUE
          AND (
              COALESCE(dependency_mode, 'all') <> 'any'
              OR NOT EXISTS (
                  SELECT 1
                  FROM job_dependencies jd
                  JOIN jobs dep ON dep.id = jd.depends_on_job_id
                  WHERE jd.job_id = jobs.id
                    AND dep.id <> OLD.id
                    AND dep.organization_id = jobs.organization_id
                    AND dep.status NOT IN ('failed', 'deadletter', 'cancelled')
              )
          );

        -- Dependents the sweep already cancelled because this job failed: a
        -- retry would re-open them with the edge gone. Only the reason
        -- changes.
        UPDATE jobs
        SET last_error = v_reason
        WHERE id IN (
                SELECT child.id FROM jobs child WHERE child.parent_job_id = OLD.id
                UNION
                SELECT jd.job_id FROM job_dependencies jd WHERE jd.depends_on_job_id = OLD.id
            )
          AND organization_id = OLD.organization_id
          AND status = 'cancelled'
          AND last_error = ANY(v_sweep_reasons)
          AND (
              COALESCE(dependency_mode, 'all') <> 'any'
              OR NOT EXISTS (
                  SELECT 1
                  FROM job_dependencies jd
                  WHERE jd.job_id = jobs.id
                    AND jd.depends_on_job_id <> OLD.id
              )
          );
    END IF;
    RETURN OLD;
END;
$$ LANGUAGE plpgsql;
