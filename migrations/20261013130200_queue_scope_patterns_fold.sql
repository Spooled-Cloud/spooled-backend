-- Let `queue_in_key_scope()` (20261013130100) fold to the old index-friendly
-- predicate for exact-name scopes.
--
-- Its `prefix.*` arm called array_to_string(), which is STABLE (it takes
-- anyarray), so the planner kept the whole arm in every plan:
-- `queue_in_key_scope(queue_name, ARRAY['orders.eu'])` could not become
-- `queue_name = ANY('{orders.eu}')`, and a scoped key's dashboard and stats
-- counts scanned the org's whole job range instead of its queue's slice of
-- idx_jobs_org_queue_status (1.5 ms became 43 ms on 130k jobs). The arm also
-- re-ran the regexp rewrite on every row for a prefix scope.
--
-- The rewrite now lives in `queue_scope_like_patterns()`, declared IMMUTABLE:
-- it reads nothing but its argument, and array_to_string() of a text[] does not
-- depend on any setting. With a bound scope (a custom plan) the planner computes
-- it once at plan time, so an exact-name scope keeps only
-- `queue_name = ANY($scope)` and a prefix scope a plain `LIKE ANY('{orders.%}')`.
-- Same answers as before for every scope.
--
-- COST 100 (the default for SQL functions) is stated because the plan cache
-- relies on it: a generic plan of a scan, which would call the function on each
-- row, costs more than re-planning with the scope folded in, while by-id
-- statements (settle, renew, get) still settle on their generic plan.

SET LOCAL lock_timeout = '5s';

-- The scope's entries as LIKE patterns (`prefix.*` as `prefix.%`, other entries
-- escaped to match themselves only), or NULL when no entry is a `prefix.*`
-- pattern.
CREATE OR REPLACE FUNCTION queue_scope_like_patterns(p_scope TEXT[])
RETURNS TEXT[] AS $$
    SELECT CASE WHEN strpos(array_to_string(p_scope, chr(31)), '.*') > 0 THEN
        string_to_array(
            regexp_replace(
                replace(replace(replace(array_to_string(p_scope, chr(31)), '\', '\\'), '%', '\%'), '_', '\_'),
                '([^' || chr(31) || '])\.\*(' || chr(31) || '|$)', '\1.%\2', 'g'),
            chr(31))
    END;
$$ LANGUAGE sql IMMUTABLE PARALLEL SAFE COST 100;

COMMENT ON FUNCTION queue_scope_like_patterns IS
    'LIKE patterns of an API key queue scope (prefix.* as prefix.%), NULL without a prefix.* entry; used by queue_in_key_scope';

CREATE OR REPLACE FUNCTION queue_in_key_scope(p_queue_name TEXT, p_scope TEXT[])
RETURNS BOOLEAN AS $$
    SELECT p_scope IS NULL
        OR cardinality(p_scope) = 0
        OR '*' = ANY(p_scope)
        OR p_queue_name = ANY(p_scope)
        OR (
            queue_scope_like_patterns(p_scope) IS NOT NULL
            AND p_queue_name LIKE ANY(queue_scope_like_patterns(p_scope))
        );
$$ LANGUAGE sql STABLE PARALLEL SAFE;
