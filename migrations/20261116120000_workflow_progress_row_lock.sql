-- Two workflow steps that finish at the same time must close the workflow.
--
-- update_workflow_progress counted the workflow's job statuses before it
-- touched the workflow row. When two steps completed in concurrent
-- transactions, each count ran on a snapshot taken before the other step
-- committed, so both saw the other step still processing and both wrote
-- 'running'. The workflow then stayed running for ever: it held a
-- max_workflows slot, workflow.completed was never sent, and the scheduler
-- sweep only closed workflows that had a failed or cancelled step.
--
-- The trigger now locks the workflow row first. The second transaction waits
-- for the first to commit, and its count (a new statement, so a new snapshot
-- under READ COMMITTED) sees the other step's final status. The lock order is
-- unchanged: the old UPDATE took the same row lock, only later.
--
-- Everything else is as defined by 20261015120100_workflow_progress_deadletter.

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
        -- Serialize concurrent step transitions of one workflow before counting.
        PERFORM 1 FROM workflows WHERE id = NEW.workflow_id FOR UPDATE;

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
