-- Rebuild idx_workflows_unannounced_outcome if its concurrent build was
-- interrupted.
--
-- A CREATE INDEX CONCURRENTLY that fails part-way (cancelled, or chosen as the
-- deadlock victim while other app instances wait on the migrator's advisory
-- lock) leaves an INVALID index behind. The previous migration then fails and
-- runs again on the next start, where IF NOT EXISTS skips the invalid index and
-- records success, and the planner never uses it. This migration notices that
-- case and rebuilds the index in place; on a database where the concurrent
-- build succeeded it does nothing. The rebuild blocks writes to workflows for
-- the duration of the build.

SET LOCAL lock_timeout = '5s';

DO $$
BEGIN
    IF EXISTS (
        SELECT 1
        FROM pg_index i
        JOIN pg_class c ON c.oid = i.indexrelid
        JOIN pg_namespace n ON n.oid = c.relnamespace
        WHERE c.relname = 'idx_workflows_unannounced_outcome'
          AND n.nspname = current_schema()
          AND NOT i.indisvalid
    ) THEN
        DROP INDEX idx_workflows_unannounced_outcome;
        CREATE INDEX idx_workflows_unannounced_outcome
            ON workflows (completed_at)
            WHERE status IN ('completed', 'failed')
              AND announced_completed_at IS DISTINCT FROM completed_at;
    END IF;
END $$;
