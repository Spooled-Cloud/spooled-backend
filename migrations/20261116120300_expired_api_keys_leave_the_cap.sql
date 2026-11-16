-- An expired API key no longer counts toward max_api_keys.
--
-- get_org_resource_counts counted every active, non-system key, including
-- keys whose expires_at had passed. Such a key authenticates nothing, yet it
-- held a slot: an organization at its limit had to deactivate keys that had
-- already stopped working before it could create a new one (API-3).
--
-- Everything else is as defined by
-- 20261012110000_system_api_keys_and_active_workflow_count.sql; the worker
-- column keeps calling worker_counts_toward_cap (20261116120200).

SET LOCAL lock_timeout = '5s';

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
        (SELECT COUNT(*) FROM api_keys k
         WHERE k.organization_id = p_org_id
           AND k.is_active = TRUE
           AND k.is_system = FALSE
           AND (k.expires_at IS NULL OR k.expires_at > NOW()))::BIGINT,
        (SELECT COUNT(*) FROM schedules WHERE organization_id = p_org_id AND is_active = TRUE)::BIGINT,
        (SELECT COUNT(*) FROM workflows wf WHERE wf.organization_id = p_org_id AND wf.status IN ('pending', 'running'))::BIGINT,
        (SELECT COUNT(*) FROM outgoing_webhooks WHERE organization_id = p_org_id AND enabled = TRUE)::BIGINT,
        COALESCE(get_daily_jobs(p_org_id), 0);
END;
$$ LANGUAGE plpgsql;

COMMENT ON FUNCTION get_org_resource_counts IS
    'Get all resource counts for limit checking (queues include implicit and scheduled queues; workers count while heartbeating; api_keys exclude system and expired keys; workflows count pending/running only)';
