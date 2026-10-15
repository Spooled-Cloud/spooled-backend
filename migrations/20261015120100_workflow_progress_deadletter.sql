-- A dead-lettered or cancelled workflow step must update the workflow row.
--
-- update_workflow_progress only ran when the new job status was completed or
-- failed. Worker failures are stored as deadletter, and dependency cancels as
-- cancelled, so neither transition touched workflows.failed_jobs or
-- workflows.status. The list endpoint reads that stored row, so a workflow
-- whose step had dead-lettered stayed running with failed_jobs 0 until a
-- later completed/failed transition or the idle sweep. The SELECT below
-- already counts deadletter and cancelled; the guard now lets those
-- transitions in. Terminal workflows stay terminal (status filter).

SET LOCAL lock_timeout = '5s';

CREATE OR REPLACE FUNCTION update_workflow_progress()
RETURNS TRIGGER AS $$
DECLARE
    v_active BIGINT;
    v_failed BIGINT;
    v_cancelled BIGINT;
BEGIN
    IF NEW.workflow_id IS NOT NULL
       AND NEW.status IN ('completed', 'failed', 'deadletter', 'cancelled') THEN
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
