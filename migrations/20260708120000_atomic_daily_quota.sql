-- Atomic daily-quota enforcement.
--
-- The enqueue path used to check the daily job count and increment it in two
-- separate statements, so N concurrent enqueues could all read a count under
-- the cap and then all increment, overshooting the per-plan jobs_per_day limit.
-- This function does the check-and-increment atomically under a row lock, so
-- concurrent callers serialize on the org's usage row and the cap is never
-- exceeded.
--
-- Returns the new daily count, or -1 if the increment would exceed p_limit.
-- p_limit < 0 means unlimited (always increments).
CREATE OR REPLACE FUNCTION try_increment_daily_jobs(
    p_org_id TEXT,
    p_count INT,
    p_limit BIGINT
) RETURNS BIGINT AS $$
DECLARE
    v_today BIGINT;
    v_daily_reset DATE;
BEGIN
    -- Ensure a row exists so it can be locked (all counter columns default to 0).
    INSERT INTO organization_usage (organization_id)
    VALUES (p_org_id)
    ON CONFLICT (organization_id) DO NOTHING;

    -- Lock this org's usage row; concurrent callers block here and serialize.
    SELECT jobs_created_today, last_daily_reset
      INTO v_today, v_daily_reset
      FROM organization_usage
     WHERE organization_id = p_org_id
       FOR UPDATE;

    -- Apply the daily reset in-memory before the cap check so a new day starts
    -- the count from zero.
    IF v_daily_reset < CURRENT_DATE THEN
        v_today := 0;
    END IF;

    -- Enforce the cap atomically.
    IF p_limit >= 0 AND v_today + p_count > p_limit THEN
        RETURN -1;
    END IF;

    -- Under the cap: commit the increment with the same daily/monthly reset
    -- semantics as increment_daily_jobs.
    UPDATE organization_usage SET
        jobs_created_today = CASE
            WHEN last_daily_reset < CURRENT_DATE THEN p_count
            ELSE jobs_created_today + p_count END,
        last_daily_reset = CURRENT_DATE,
        jobs_created_month = CASE
            WHEN last_monthly_reset < DATE_TRUNC('month', CURRENT_DATE)::DATE THEN p_count
            ELSE jobs_created_month + p_count END,
        last_monthly_reset = DATE_TRUNC('month', CURRENT_DATE)::DATE,
        total_jobs_created = total_jobs_created + p_count,
        updated_at = CURRENT_TIMESTAMP
    WHERE organization_id = p_org_id
    RETURNING jobs_created_today INTO v_today;

    RETURN v_today;
END;
$$ LANGUAGE plpgsql;

COMMENT ON FUNCTION try_increment_daily_jobs IS
    'Atomically enforce + increment the daily job cap under a row lock. Returns the new daily count, or -1 if the increment would exceed p_limit (p_limit < 0 = unlimited).';
