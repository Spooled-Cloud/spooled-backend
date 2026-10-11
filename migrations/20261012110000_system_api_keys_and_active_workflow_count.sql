-- System API keys and active-only workflow counting for the plan caps.
--
-- 1. `api_keys.is_system` marks the key the email-login flow creates to back
--    dashboard sessions ("Email Login"). The customer never created it, so it
--    no longer counts toward `max_api_keys`: on the Free plan (2 keys) the
--    first email login used to take the second slot, and `POST /api-keys`
--    then failed with a limit the user could not explain. Existing rows are
--    backfilled from the name AND the `sp_email` / `sk_email` prefix only that
--    flow generates, so a customer key that happens to be named "Email Login"
--    stays an ordinary key. The API never lets a caller set the flag.
--
-- 2. `max_workflows` counts only workflows that are still pending or running.
--    Every finished run used to hold a slot until retention deleted it (14
--    days on Starter) and there is no delete route, so Starter's 5 meant
--    "5 workflow runs per 14 days". Plan numbers are unchanged.
--
-- Everything else in `get_org_resource_counts` is as defined by
-- 20261011150300_count_draining_workers.sql.

SET LOCAL lock_timeout = '5s';

-- Constant default: a catalog-only change, no table rewrite.
ALTER TABLE api_keys ADD COLUMN IF NOT EXISTS is_system BOOLEAN NOT NULL DEFAULT FALSE;

COMMENT ON COLUMN api_keys.is_system IS
    'Created by Spooled itself (the email-login dashboard key), not by the customer; excluded from max_api_keys';

UPDATE api_keys
SET is_system = TRUE
WHERE name = 'Email Login'
  AND key_prefix IN ('sp_email', 'sk_email')
  AND is_system = FALSE;

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
        (SELECT COUNT(*) FROM api_keys k WHERE k.organization_id = p_org_id AND k.is_active = TRUE AND k.is_system = FALSE)::BIGINT,
        (SELECT COUNT(*) FROM schedules WHERE organization_id = p_org_id AND is_active = TRUE)::BIGINT,
        (SELECT COUNT(*) FROM workflows wf WHERE wf.organization_id = p_org_id AND wf.status IN ('pending', 'running'))::BIGINT,
        (SELECT COUNT(*) FROM outgoing_webhooks WHERE organization_id = p_org_id AND enabled = TRUE)::BIGINT,
        COALESCE(get_daily_jobs(p_org_id), 0);
END;
$$ LANGUAGE plpgsql;

COMMENT ON FUNCTION get_org_resource_counts IS
    'Get all resource counts for limit checking (queues include implicit and scheduled queues; workers include live draining ones; api_keys exclude system keys; workflows count pending/running only)';
