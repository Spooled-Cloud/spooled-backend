-- A worker that stopped heartbeating no longer holds a slot of the worker cap.
--
-- worker_counts_toward_cap counted every healthy or degraded worker, however
-- long it had been silent, until the stale-worker sweep marked it offline. The
-- sweep runs every five minutes on top of its two-minute threshold, so after
-- a worker was stopped without deregistering (Ctrl+C), starting it again on
-- the Free plan (max_workers 1) failed with "workers limit reached (1/1)" for
-- up to seven minutes (NP-1).
--
-- A worker now counts only while its last heartbeat is within the sweep's
-- threshold (two minutes), whatever its status; draining workers already
-- worked this way. The registration paths use the same function to tell a
-- re-registration from a new worker, so a silent worker re-registering takes
-- a slot again, which it no longer holds.

SET LOCAL lock_timeout = '5s';

CREATE OR REPLACE FUNCTION worker_counts_toward_cap(p_status TEXT, p_last_heartbeat TIMESTAMPTZ)
RETURNS BOOLEAN AS $$
    SELECT p_status IN ('healthy', 'degraded', 'draining')
        AND p_last_heartbeat > NOW() - INTERVAL '2 minutes';
$$ LANGUAGE sql STABLE;

COMMENT ON FUNCTION worker_counts_toward_cap IS
    'Whether a worker holds a slot of the plan worker cap: healthy, degraded or draining with a heartbeat in the last 2 minutes (the stale-worker sweep threshold)';
