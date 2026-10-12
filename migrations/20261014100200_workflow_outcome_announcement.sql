-- Remember which workflow outcome has been announced.
--
-- A workflow ends 'completed' or 'failed' in two places: the progress trigger
-- on jobs (update_workflow_progress), which runs inside whatever transaction
-- finished the last job (REST, gRPC, the built-in worker), and the scheduler's
-- terminate sweep. Neither can send an outgoing webhook, so the scheduler
-- announces outcomes after the fact: it picks completed/failed workflows whose
-- `completed_at` differs from `announced_completed_at`, copies the one into the
-- other in the same statement, and sends workflow.completed / workflow.failed
-- for exactly the rows it updated. One announcement per transition, whichever
-- path ended the workflow and however many scheduler instances run: a retry
-- clears completed_at, so the next outcome gets a new completed_at and is
-- announced again.
--
-- Nullable without a default: a catalog change, no rewrite. Existing rows keep
-- NULL; the sweep only looks at the last hour, so old outcomes are never
-- announced (no webhook could subscribe to these events before this release).

SET LOCAL lock_timeout = '5s';

ALTER TABLE workflows ADD COLUMN IF NOT EXISTS announced_completed_at TIMESTAMPTZ;

COMMENT ON COLUMN workflows.announced_completed_at IS
    'completed_at of the last outcome sent as workflow.completed/workflow.failed (scheduler)';
