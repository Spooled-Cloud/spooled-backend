-- Count every queue an organization actually has, not only configured ones.
--
-- Queues are implicit: the first job into a name creates the queue, and
-- `queue_config` rows exist only for queues someone configured or paused. The
-- `queues` figure in get_org_resource_counts counted `queue_config` rows alone, so
-- `GET /organizations/usage`, the dashboard meter and the plan's `max_queues` cap
-- all ignored queues created by enqueueing (a free org showed "1/2" while using 4).
--
-- A queue now exists when it has a `queue_config` row, holds jobs, or is the
-- target of an active schedule (so a rarely-firing schedule's queue does not
-- disappear between runs once retention has purged its jobs).

CREATE OR REPLACE FUNCTION get_org_queue_count(p_org_id TEXT)
RETURNS BIGINT AS $$
    -- Distinct job queue names via a loose index scan on
    -- idx_jobs_org_queue_status (organization_id, queue_name, status): one index
    -- probe per distinct queue instead of reading every job row of the org.
    WITH RECURSIVE job_queues AS (
        (
            SELECT queue_name FROM jobs
            WHERE organization_id = p_org_id
            ORDER BY queue_name
            LIMIT 1
        )
        UNION ALL
        SELECT (
            SELECT j.queue_name FROM jobs j
            WHERE j.organization_id = p_org_id
              AND j.queue_name > jq.queue_name
            ORDER BY j.queue_name
            LIMIT 1
        )
        FROM job_queues jq
        WHERE jq.queue_name IS NOT NULL
    )
    SELECT COUNT(*)::BIGINT FROM (
        SELECT queue_name FROM job_queues WHERE queue_name IS NOT NULL
        UNION
        SELECT queue_name FROM queue_config WHERE organization_id = p_org_id
        UNION
        SELECT queue_name FROM schedules
        WHERE organization_id = p_org_id AND is_active = TRUE
    ) AS all_queues;
$$ LANGUAGE sql STABLE;

COMMENT ON FUNCTION get_org_queue_count IS
    'Distinct queues of an org: configured, holding jobs, or targeted by an active schedule';

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
        (SELECT COUNT(*) FROM workers WHERE organization_id = p_org_id AND status IN ('healthy', 'degraded'))::BIGINT,
        (SELECT COUNT(*) FROM api_keys WHERE organization_id = p_org_id AND is_active = TRUE)::BIGINT,
        (SELECT COUNT(*) FROM schedules WHERE organization_id = p_org_id AND is_active = TRUE)::BIGINT,
        (SELECT COUNT(*) FROM workflows WHERE organization_id = p_org_id)::BIGINT,
        (SELECT COUNT(*) FROM outgoing_webhooks WHERE organization_id = p_org_id AND enabled = TRUE)::BIGINT,
        COALESCE(get_daily_jobs(p_org_id), 0);
END;
$$ LANGUAGE plpgsql;

COMMENT ON FUNCTION get_org_resource_counts IS
    'Get all resource counts for limit checking (queues include implicit and scheduled queues)';
