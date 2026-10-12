-- no-transaction
-- An index of the workflow outcomes not announced yet.
--
-- Every 10 s the scheduler looks for completed/failed workflows whose outcome
-- has not been sent as an outgoing webhook (20261014100200). Without an index
-- that is a scan of every finished workflow, since idx_workflows_status only
-- narrows it to the finished ones, which is most of the table. This partial
-- index has the sweep's predicate verbatim (the planner only uses a partial
-- index whose predicate the query implies) and is keyed by completed_at, which
-- the sweep bounds to the last hour and orders by. An announced row leaves the
-- index; outcomes from before this release stay in it until retention deletes
-- the workflow, and the range scan never reaches them.
--
-- CONCURRENTLY keeps workflows writable while the index builds, which is why
-- this file runs outside a transaction. It is a single statement on purpose:
-- Postgres runs a multi-statement string as one implicit transaction block and
-- refuses CREATE INDEX CONCURRENTLY inside it, so this file cannot carry the
-- usual `SET lock_timeout` line. An interrupted build leaves an INVALID index
-- that IF NOT EXISTS skips on the next start; 20261014100202 rebuilds it then.
CREATE INDEX CONCURRENTLY IF NOT EXISTS idx_workflows_unannounced_outcome
    ON workflows (completed_at)
    WHERE status IN ('completed', 'failed')
      AND announced_completed_at IS DISTINCT FROM completed_at;
