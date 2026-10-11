-- Read `custom_limits` retention overrides without ever failing.
--
-- The scheduler's retention sweep cast `custom_limits->>'job_retention_days'`
-- (and `history_retention_days`) straight to INT for every org in one
-- statement. A single org with a string ("90"), a fraction (1.5) or an
-- out-of-range number made each sweep statement error, so retention stopped
-- for EVERY org until the value was fixed; a stored -1 passed the cast and put
-- the cutoff in the future, deleting all of that org's completed jobs.
--
-- `custom_retention_days` returns the override only when it is a JSON number
-- written as a whole number of days in 0..=36500, and NULL otherwise, so the
-- caller falls back to the plan default. The Rust side
-- (`PlanLimits::for_tier_with_overrides`) applies the same rule, and the admin
-- API now rejects anything else before it is stored.
--
-- The nested CASE is deliberate: Postgres does not promise the evaluation
-- order of AND operands, but it does evaluate a CASE branch only after its
-- condition holds, so the INT cast can never see a value the guard rejected.

SET LOCAL lock_timeout = '5s';

CREATE OR REPLACE FUNCTION custom_retention_days(p_limits JSONB, p_key TEXT)
RETURNS INT AS $$
    SELECT CASE
        WHEN jsonb_typeof(p_limits -> p_key) = 'number'
             AND (p_limits ->> p_key) ~ '^[0-9]{1,5}$'
        THEN CASE
            WHEN (p_limits ->> p_key)::INT <= 36500 THEN (p_limits ->> p_key)::INT
        END
    END;
$$ LANGUAGE sql IMMUTABLE;

COMMENT ON FUNCTION custom_retention_days IS
    'A custom_limits retention override in days (whole number 0..36500), or NULL to use the plan default';
