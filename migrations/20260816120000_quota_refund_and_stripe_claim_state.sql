-- Audit 2026-08-16 remediation: B-29 (quota refunds) and B-33 (Stripe claims).
--
-- ============================================================================
-- B-29: sign-safe daily-quota refunds
-- ============================================================================
-- `bulk_enqueue` reserves quota for the whole batch up front and then refunds
-- the portion that did not become jobs (in-batch duplicates, idempotency hits,
-- or a fully failed insert). It did that by calling `increment_daily_jobs` with
-- a NEGATIVE count, and that function was never written to accept one:
--
--   * its daily/monthly branches ASSIGN rather than add when the reset date has
--     rolled over -- so a refund landing after midnight set `jobs_created_today`
--     to a negative number, handing the org `limit + refunded` jobs for the day;
--   * `total_jobs_created` is added to unconditionally, so every refund reduced
--     a lifetime counter that must only ever grow (it is surfaced to operators
--     by GET /admin/organizations/{id}).
--
-- This function is the refund primitive: positive input, floored at zero, and
-- it COMPARES the reset columns instead of writing them so a refund can never
-- trigger a reset. A reservation made before a rollover simply has nothing left
-- to refund today, which is correct -- that day's counter is already gone.
CREATE OR REPLACE FUNCTION refund_daily_jobs(p_org_id TEXT, p_count INT)
RETURNS BIGINT AS $$
DECLARE
    v_today BIGINT;
BEGIN
    IF p_count IS NULL OR p_count <= 0 THEN
        SELECT jobs_created_today INTO v_today
          FROM organization_usage WHERE organization_id = p_org_id;
        RETURN COALESCE(v_today, 0);
    END IF;

    UPDATE organization_usage SET
        jobs_created_today = CASE
            WHEN last_daily_reset = CURRENT_DATE
            THEN GREATEST(0, jobs_created_today - p_count)
            ELSE jobs_created_today
        END,
        jobs_created_month = CASE
            WHEN last_monthly_reset = DATE_TRUNC('month', CURRENT_DATE)::DATE
            THEN GREATEST(0, jobs_created_month - p_count)
            ELSE jobs_created_month
        END,
        -- total_jobs_created IS reversed here, unlike the daily/monthly columns
        -- it needs no reset guard (it never resets).
        --
        -- It would be wrong to skip it: try_increment_daily_jobs charges the
        -- full RESERVATION to this column, and a reservation is provisional. A
        -- bulk request of 100 rows that all dedupe would otherwise add 100 to a
        -- "jobs ever created" counter while creating zero jobs, and the counter
        -- is surfaced to operators by GET /admin/organizations/{id}. Reversing a
        -- provisional charge is not the same as decrementing a lifetime total.
        total_jobs_created = GREATEST(0, total_jobs_created - p_count),
        updated_at = CURRENT_TIMESTAMP
    WHERE organization_id = p_org_id
    RETURNING jobs_created_today INTO v_today;

    RETURN COALESCE(v_today, 0);
END;
$$ LANGUAGE plpgsql;

COMMENT ON FUNCTION refund_daily_jobs IS
    'Reverse an unused job-quota RESERVATION. p_count is POSITIVE. Floors every counter at zero and never crosses a daily/monthly reset boundary.';

-- KNOWN RESIDUAL, documented rather than papered over.
--
-- If a reservation is taken just before midnight, the batch fails or dedupes,
-- AND another enqueue for the same org rolls last_daily_reset to the new day
-- before the refund runs, the guard below sees last_daily_reset = CURRENT_DATE
-- and applies the refund to the NEW day's counter instead of the (already gone)
-- old one.
--
-- The effect is bounded and small, which is why it is left open. The refund
-- cannot make the counter negative (GREATEST(0, ...)), so the org gains at most
-- `min(refund_size, jobs_already_enqueued_today)` extra headroom — and the race
-- requires the refund to land within milliseconds of midnight, when
-- jobs_already_enqueued_today is by definition near zero. Realistic worst case
-- is a couple of jobs, once per midnight, only for a batch that straddled it.
--
-- Closing it properly requires the reservation to carry the date it was charged
-- to, which means threading a token through every reserve/refund call site
-- (jobs bulk, schedules, webhooks, workflows). That plumbing costs more than the
-- defect. Revisit if daily caps ever become a billing boundary rather than a
-- throttle.

-- Repair drift already caused by negative increments before this migration.
UPDATE organization_usage
   SET jobs_created_today = GREATEST(0, jobs_created_today),
       jobs_created_month = GREATEST(0, jobs_created_month),
       total_jobs_created = GREATEST(0, total_jobs_created)
 WHERE jobs_created_today < 0
    OR jobs_created_month < 0
    OR total_jobs_created < 0;

-- Make a regression fail loudly instead of silently granting quota.
ALTER TABLE organization_usage
    DROP CONSTRAINT IF EXISTS organization_usage_counters_non_negative;
ALTER TABLE organization_usage
    ADD CONSTRAINT organization_usage_counters_non_negative
    CHECK (
        jobs_created_today >= 0
        AND jobs_created_month >= 0
        AND total_jobs_created >= 0
    );

-- ============================================================================
-- B-33: Stripe event claims must record COMPLETION, not attempt
-- ============================================================================
-- The webhook handler claimed an event id with `INSERT ... ON CONFLICT DO
-- NOTHING`, processed it, and DELETEd the claim only if the handler returned
-- Err. That covers a returned error and nothing else: a crash, OOM kill, or a
-- deploy rolling the container between the claim and the handler left the claim
-- committed with the work never done. Stripe then retried for up to three days
-- and every retry was answered "200 OK, already processed" -- so a paid invoice,
-- a cancellation, or a checkout completion was discarded silently.
--
-- `status` distinguishes a finished event from an in-flight one, and
-- `claimed_at` lets a stale 'processing' row be re-claimed. Existing rows are
-- backfilled to 'done': they are historical claims whose handlers did run.
ALTER TABLE processed_stripe_events
    ADD COLUMN IF NOT EXISTS status TEXT NOT NULL DEFAULT 'done';
ALTER TABLE processed_stripe_events
    ADD COLUMN IF NOT EXISTS claimed_at TIMESTAMPTZ NOT NULL DEFAULT NOW();

ALTER TABLE processed_stripe_events
    DROP CONSTRAINT IF EXISTS processed_stripe_events_status_check;
ALTER TABLE processed_stripe_events
    ADD CONSTRAINT processed_stripe_events_status_check
    CHECK (status IN ('processing', 'done'));

-- Lets the stale-claim predicate use an index instead of scanning.
CREATE INDEX IF NOT EXISTS idx_processed_stripe_events_processing
    ON processed_stripe_events (claimed_at)
    WHERE status = 'processing';

COMMENT ON COLUMN processed_stripe_events.status IS
    'processing = claimed, handler not finished; done = fully applied. Only done suppresses a retry.';
COMMENT ON COLUMN processed_stripe_events.claimed_at IS
    'When the claim was taken. A processing row older than the stale window is re-claimable (crashed attempt).';
