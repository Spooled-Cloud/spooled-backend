-- A workflow with a cancelled step is not 'completed'.
--
-- The progress trigger decided a workflow's outcome from its failed and
-- dead-lettered jobs only. When a step had been cancelled (its dependency
-- dead-lettered and the scheduler sweep cancelled it, or someone cancelled the
-- job) and the remaining jobs then finished, the workflow ended 'completed'
-- with `completed_jobs < total_jobs` and a step that never ran. It now ends
-- 'failed' when a step failed or dead-lettered, else 'cancelled' when a step
-- was cancelled, else 'completed', which is how the scheduler sweep already
-- closes workflows that have nothing left to run. The guard that keeps
-- terminal workflows terminal (20260714120000) is unchanged.

SET LOCAL lock_timeout = '5s';

CREATE OR REPLACE FUNCTION update_workflow_progress()
RETURNS TRIGGER AS $$
DECLARE
    v_active BIGINT;
    v_failed BIGINT;
    v_cancelled BIGINT;
BEGIN
    IF NEW.workflow_id IS NOT NULL AND NEW.status IN ('completed', 'failed') THEN
        SELECT
            COUNT(*) FILTER (WHERE status IN ('pending', 'scheduled', 'processing')),
            COUNT(*) FILTER (WHERE status IN ('failed', 'deadletter')),
            COUNT(*) FILTER (WHERE status = 'cancelled')
        INTO v_active, v_failed, v_cancelled
        FROM jobs
        WHERE workflow_id = NEW.workflow_id;

        UPDATE workflows
        SET
            completed_jobs = (
                SELECT COUNT(*) FROM jobs
                WHERE workflow_id = NEW.workflow_id AND status = 'completed'
            ),
            failed_jobs = v_failed,
            status = CASE
                WHEN v_active > 0 THEN 'running'
                WHEN v_failed > 0 THEN 'failed'
                WHEN v_cancelled > 0 THEN 'cancelled'
                ELSE 'completed'
            END,
            completed_at = CASE
                WHEN v_active = 0 THEN NOW()
                ELSE NULL
            END
        WHERE id = NEW.workflow_id
          AND status IN ('pending', 'running');
    END IF;

    RETURN NEW;
END;
$$ LANGUAGE plpgsql;
