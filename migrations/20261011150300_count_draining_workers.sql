-- Count draining workers toward the plan's worker cap.
--
-- `get_org_resource_counts` counted only `healthy` and `degraded` workers, so a
-- worker could drop out of the `max_workers` count by heartbeating
-- `status = 'draining'` while it kept running, and an org could register any
-- number of workers that way. A draining worker is still alive and still holds
-- its jobs, so it now holds its slot too.
--
-- Only while it heartbeats, though: the stale-worker sweep marks `healthy` and
-- `degraded` workers offline after two minutes of silence but leaves draining
-- ones alone, so a draining worker that crashed would otherwise hold a slot for
-- ever. One silent for two minutes gives its slot back.
--
-- `worker_counts_toward_cap` is the single rule: the registration paths use it
-- to tell a re-registration (no new slot) from a new worker.

SET LOCAL lock_timeout = '5s';

CREATE OR REPLACE FUNCTION worker_counts_toward_cap(p_status TEXT, p_last_heartbeat TIMESTAMPTZ)
RETURNS BOOLEAN AS $$
    SELECT p_status IN ('healthy', 'degraded')
        OR (p_status = 'draining' AND p_last_heartbeat > NOW() - INTERVAL '2 minutes');
$$ LANGUAGE sql STABLE;

COMMENT ON FUNCTION worker_counts_toward_cap IS
    'Whether a worker holds a slot of the plan worker cap: healthy, degraded, or draining with a heartbeat in the last 2 minutes';

CREATE OR REPLACE FUNCTION get_org_resource_counts(p_org_id TEXT)
RETURNS TABLE (
    active_jobs BIGINT,
    queues BIGINT,
    workers BIGINT,
    api_keys BIGINT,
    schedules BIGINT,
    workflows BIGINT,
    webhooks BIGINT,
    jobs_today BIGINT
) AS $$
BEGIN
    RETURN QUERY
    SELECT
        (SELECT COUNT(*) FROM jobs WHERE organization_id = p_org_id AND status IN ('pending', 'processing', 'scheduled'))::BIGINT,
        get_org_queue_count(p_org_id),
        (SELECT COUNT(*) FROM workers w WHERE w.organization_id = p_org_id AND worker_counts_toward_cap(w.status, w.last_heartbeat))::BIGINT,
        (SELECT COUNT(*) FROM api_keys WHERE organization_id = p_org_id AND is_active = TRUE)::BIGINT,
        (SELECT COUNT(*) FROM schedules WHERE organization_id = p_org_id AND is_active = TRUE)::BIGINT,
        (SELECT COUNT(*) FROM workflows WHERE organization_id = p_org_id)::BIGINT,
        (SELECT COUNT(*) FROM outgoing_webhooks WHERE organization_id = p_org_id AND enabled = TRUE)::BIGINT,
        COALESCE(get_daily_jobs(p_org_id), 0);
END;
$$ LANGUAGE plpgsql;

COMMENT ON FUNCTION get_org_resource_counts IS
    'Get all resource counts for limit checking (queues include implicit and scheduled queues; workers include live draining ones)';
