-- Deleting one branch of an any-join must not cancel a dependent that can
-- still run.
--
-- cancel_blocked_dependents_of_deleted_job cancelled every pending or
-- scheduled dependent whose dependencies_met was not true, with no
-- dependency_mode test. The dependency sweep cancels an any-join only when
-- every dependency is failed, dead-lettered or cancelled. Expiry, purge and
-- DELETE of one alternative therefore cancelled a child that still had a
-- live branch, and that child's last_error is not one of the reasons a retry
-- reopens.
--
-- An any-join is cancelled only when no dependency other than the one being
-- deleted can still complete. The edge to a surviving dependency stays, so
-- the orphan sweep cannot release the child. all-mode is unchanged: one
-- dependency deleted before it completed is fatal.
--
-- The child is locked before that decision. Concurrent deletes of the last
-- two alternatives otherwise both see the other parent still pending, neither
-- cancels the child, and after both commit the orphan sweep marks it runnable.

CREATE OR REPLACE FUNCTION cancel_blocked_dependents_of_deleted_job()
RETURNS TRIGGER AS $$
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
          AND status IN ('pending', 'scheduled')
          AND dependencies_met IS NOT TRUE
        ORDER BY id
        FOR NO KEY UPDATE;

        UPDATE jobs
        SET status = 'cancelled',
            last_error = 'Dependency job ' || OLD.id || ' was deleted before it completed',
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
            last_error = 'Dependency job ' || OLD.id || ' was deleted before it completed',
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
    END IF;
    RETURN OLD;
END;
$$ LANGUAGE plpgsql;
