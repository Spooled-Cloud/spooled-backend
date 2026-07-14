-- Prevent cancelled (or other terminal) workflows from flipping back to
-- completed/failed/running when an in-flight job later settles.
CREATE OR REPLACE FUNCTION update_workflow_progress()
RETURNS TRIGGER AS $$
BEGIN
    IF NEW.workflow_id IS NOT NULL AND NEW.status IN ('completed', 'failed') THEN
        UPDATE workflows
        SET
            completed_jobs = (
                SELECT COUNT(*) FROM jobs
                WHERE workflow_id = NEW.workflow_id AND status = 'completed'
            ),
            failed_jobs = (
                SELECT COUNT(*) FROM jobs
                WHERE workflow_id = NEW.workflow_id AND status IN ('failed', 'deadletter')
            ),
            status = CASE
                WHEN (SELECT COUNT(*) FROM jobs WHERE workflow_id = NEW.workflow_id AND status IN ('pending', 'scheduled', 'processing')) = 0
                     AND (SELECT COUNT(*) FROM jobs WHERE workflow_id = NEW.workflow_id AND status IN ('failed', 'deadletter')) = 0
                THEN 'completed'
                WHEN (SELECT COUNT(*) FROM jobs WHERE workflow_id = NEW.workflow_id AND status IN ('failed', 'deadletter')) > 0
                     AND (SELECT COUNT(*) FROM jobs WHERE workflow_id = NEW.workflow_id AND status IN ('pending', 'scheduled', 'processing')) = 0
                THEN 'failed'
                ELSE 'running'
            END,
            completed_at = CASE
                WHEN (SELECT COUNT(*) FROM jobs WHERE workflow_id = NEW.workflow_id AND status IN ('pending', 'scheduled', 'processing')) = 0
                THEN NOW()
                ELSE NULL
            END
        WHERE id = NEW.workflow_id
          AND status IN ('pending', 'running');
    END IF;

    RETURN NEW;
END;
$$ LANGUAGE plpgsql;
