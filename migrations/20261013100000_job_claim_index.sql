-- no-transaction
-- An index the job claim can walk in claim order.
--
-- Every claim (REST `POST /jobs/claim`, the gRPC Dequeue paths and the
-- built-in worker) selects with
--     organization_id = ? AND queue_name = ? AND status IN ('pending', 'scheduled')
--     ORDER BY priority DESC, created_at ASC LIMIT n FOR UPDATE SKIP LOCKED
-- The only ordered index, idx_jobs_priority, is partial on `status = 'pending'`,
-- which `status IN ('pending', 'scheduled')` does not imply, and it is not keyed
-- by queue. So each claim read and sorted every pending row of the queue: about
-- 270 ms and an on-disk sort per poll with 300k pending jobs. This index has the
-- claim's predicate verbatim (the planner only uses a partial index whose
-- predicate the query implies) and its sort order, so a claim reads the first
-- rows of one queue. EXPLAIN ANALYZE of the claim on PG16 with 300k rows:
-- before, Seq Scan + Sort (external merge, 14.8 MB on disk), 252 ms; after,
-- Index Scan using idx_jobs_claim, 0.4 ms. The regression test
-- w5_claim_query_uses_the_claim_index checks the plan.
--
-- CONCURRENTLY keeps the jobs table writable while the index builds, which is
-- why this file runs outside a transaction. It is a single statement on
-- purpose: Postgres runs a multi-statement string as one implicit transaction
-- block and refuses CREATE INDEX CONCURRENTLY inside it, so this file cannot
-- carry the usual `SET lock_timeout` line. The build only takes a SHARE UPDATE
-- EXCLUSIVE lock, which never blocks reads or writes. Should the build ever be
-- interrupted it leaves an INVALID index that IF NOT EXISTS skips on the next
-- start; 20261013100001 rebuilds it then.
CREATE INDEX CONCURRENTLY IF NOT EXISTS idx_jobs_claim
    ON jobs (organization_id, queue_name, priority DESC, created_at ASC)
    WHERE status IN ('pending', 'scheduled');
