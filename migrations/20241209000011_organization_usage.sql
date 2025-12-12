-- ============================================================================
-- ORGANIZATION USAGE TRACKING
-- ============================================================================
-- Tracks resource usage per organization for plan limit enforcement.
-- Daily counters are reset at midnight UTC.

CREATE TABLE organization_usage (
    organization_id TEXT PRIMARY KEY REFERENCES organizations(id) ON DELETE CASCADE,
    
    -- Daily job counters (reset at midnight UTC)
    jobs_created_today BIGINT NOT NULL DEFAULT 0,
    last_daily_reset DATE NOT NULL DEFAULT CURRENT_DATE,
    
    -- Monthly job counters (reset on 1st of month)
    jobs_created_month BIGINT NOT NULL DEFAULT 0,
    last_monthly_reset DATE NOT NULL DEFAULT DATE_TRUNC('month', CURRENT_DATE)::DATE,
    
    -- All-time counters (never reset)
    total_jobs_created BIGINT NOT NULL DEFAULT 0,
    total_jobs_completed BIGINT NOT NULL DEFAULT 0,
    total_jobs_failed BIGINT NOT NULL DEFAULT 0,
    
    -- Timestamps
    created_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE INDEX idx_organization_usage_daily_reset ON organization_usage(last_daily_reset);

COMMENT ON TABLE organization_usage IS 'Tracks resource usage per organization for plan limits';
COMMENT ON COLUMN organization_usage.jobs_created_today IS 'Jobs created since midnight UTC - reset daily';
COMMENT ON COLUMN organization_usage.jobs_created_month IS 'Jobs created this month - reset on 1st';

-- ============================================================================
-- FUNCTION: Increment daily job counter
-- ============================================================================
-- Atomically increments the job counter, resetting if date has changed.
-- Returns the new count.

CREATE OR REPLACE FUNCTION increment_daily_jobs(p_org_id TEXT, p_count INT DEFAULT 1)
RETURNS BIGINT AS $$
DECLARE
    v_new_count BIGINT;
BEGIN
    INSERT INTO organization_usage (organization_id, jobs_created_today, jobs_created_month, total_jobs_created)
    VALUES (p_org_id, p_count, p_count, p_count)
    ON CONFLICT (organization_id) DO UPDATE SET
        -- Reset daily counter if date changed
        jobs_created_today = CASE 
            WHEN organization_usage.last_daily_reset < CURRENT_DATE 
            THEN p_count 
            ELSE organization_usage.jobs_created_today + p_count 
        END,
        last_daily_reset = CURRENT_DATE,
        -- Reset monthly counter if month changed
        jobs_created_month = CASE 
            WHEN organization_usage.last_monthly_reset < DATE_TRUNC('month', CURRENT_DATE)::DATE 
            THEN p_count 
            ELSE organization_usage.jobs_created_month + p_count 
        END,
        last_monthly_reset = DATE_TRUNC('month', CURRENT_DATE)::DATE,
        -- Always increment total
        total_jobs_created = organization_usage.total_jobs_created + p_count,
        updated_at = CURRENT_TIMESTAMP
    RETURNING jobs_created_today INTO v_new_count;
    
    RETURN v_new_count;
END;
$$ LANGUAGE plpgsql;

COMMENT ON FUNCTION increment_daily_jobs IS 'Atomically increment job counter with auto-reset';

-- ============================================================================
-- FUNCTION: Get current daily job count
-- ============================================================================
-- Returns the current daily job count, accounting for date rollover.

CREATE OR REPLACE FUNCTION get_daily_jobs(p_org_id TEXT)
RETURNS BIGINT AS $$
DECLARE
    v_count BIGINT;
    v_last_reset DATE;
BEGIN
    SELECT jobs_created_today, last_daily_reset 
    INTO v_count, v_last_reset
    FROM organization_usage 
    WHERE organization_id = p_org_id;
    
    IF NOT FOUND THEN
        RETURN 0;
    END IF;
    
    -- If last reset was before today, count is effectively 0
    IF v_last_reset < CURRENT_DATE THEN
        RETURN 0;
    END IF;
    
    RETURN v_count;
END;
$$ LANGUAGE plpgsql;

COMMENT ON FUNCTION get_daily_jobs IS 'Get current daily job count with date awareness';

-- ============================================================================
-- FUNCTION: Increment completion/failure counters
-- ============================================================================

CREATE OR REPLACE FUNCTION increment_job_completed(p_org_id TEXT)
RETURNS VOID AS $$
BEGIN
    UPDATE organization_usage 
    SET total_jobs_completed = total_jobs_completed + 1,
        updated_at = CURRENT_TIMESTAMP
    WHERE organization_id = p_org_id;
END;
$$ LANGUAGE plpgsql;

CREATE OR REPLACE FUNCTION increment_job_failed(p_org_id TEXT)
RETURNS VOID AS $$
BEGIN
    UPDATE organization_usage 
    SET total_jobs_failed = total_jobs_failed + 1,
        updated_at = CURRENT_TIMESTAMP
    WHERE organization_id = p_org_id;
END;
$$ LANGUAGE plpgsql;

-- ============================================================================
-- FUNCTION: Get organization resource counts
-- ============================================================================
-- Returns current counts for all limited resources.

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
        (SELECT COUNT(*) FROM outgoing_webhooks WHERE organization_id = p_org_id AND is_active = TRUE)::BIGINT,
        COALESCE(get_daily_jobs(p_org_id), 0);
END;
$$ LANGUAGE plpgsql;

COMMENT ON FUNCTION get_org_resource_counts IS 'Get all resource counts for limit checking';

-- ============================================================================
-- Initialize usage records for existing organizations
-- ============================================================================

INSERT INTO organization_usage (organization_id)
SELECT id FROM organizations
ON CONFLICT (organization_id) DO NOTHING;

-- ============================================================================
-- Trigger: Auto-create usage record on new organization
-- ============================================================================

CREATE OR REPLACE FUNCTION create_org_usage_on_insert()
RETURNS TRIGGER AS $$
BEGIN
    INSERT INTO organization_usage (organization_id)
    VALUES (NEW.id)
    ON CONFLICT (organization_id) DO NOTHING;
    RETURN NEW;
END;
$$ LANGUAGE plpgsql;

CREATE TRIGGER trigger_create_org_usage
    AFTER INSERT ON organizations
    FOR EACH ROW
    EXECUTE FUNCTION create_org_usage_on_insert();

COMMENT ON TRIGGER trigger_create_org_usage ON organizations IS 'Auto-create usage tracking record';

