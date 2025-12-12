-- ============================================================================
-- FIX: get_org_resource_counts function
-- ============================================================================
-- The original function used `is_active` for outgoing_webhooks table,
-- but that table uses `enabled` column instead.

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
        (SELECT COUNT(*) FROM queue_config WHERE organization_id = p_org_id)::BIGINT,
        (SELECT COUNT(*) FROM workers WHERE organization_id = p_org_id AND status IN ('healthy', 'degraded'))::BIGINT,
        (SELECT COUNT(*) FROM api_keys WHERE organization_id = p_org_id AND is_active = TRUE)::BIGINT,
        (SELECT COUNT(*) FROM schedules WHERE organization_id = p_org_id AND is_active = TRUE)::BIGINT,
        (SELECT COUNT(*) FROM workflows WHERE organization_id = p_org_id)::BIGINT,
        (SELECT COUNT(*) FROM outgoing_webhooks WHERE organization_id = p_org_id AND enabled = TRUE)::BIGINT,
        COALESCE(get_daily_jobs(p_org_id), 0);
END;
$$ LANGUAGE plpgsql;

COMMENT ON FUNCTION get_org_resource_counts IS 'Get all resource counts for limit checking (fixed: outgoing_webhooks uses enabled column)';


