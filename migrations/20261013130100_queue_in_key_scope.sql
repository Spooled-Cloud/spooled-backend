-- `queue_in_key_scope(queue_name, scope)`: the SQL twin of the Rust
-- `queue_scope_allows` (models/api_key.rs), for queries that filter rows by an
-- API key's `queues` list.
--
-- Keys may list `prefix.*` patterns (`orders.*` covers every queue whose name
-- starts with `orders.`). The filters written as `queue_name = ANY($scope)`
-- match listed names only, so they hide every queue a pattern covers; this
-- function matches like the Rust check:
--   * NULL scope (what `queue_scope_filter()` binds for an unrestricted key),
--     an empty list or a `*` entry: every queue;
--   * `prefix.*` (non-empty prefix, no other `*`): names starting with `prefix.`;
--   * anything else: that exact name.
-- Exact entries go through LIKE too, with `\`, `%` and `_` escaped, and an entry
-- with a misplaced `*` (rejected at create since this release) stays a literal
-- that no queue name can equal, as in Rust. The entries are joined with the
-- unit separator (chr(31)) for the rewrite, a character no entry holds.
--
-- Inlinable (one SELECT, no sub-query), and the pattern rewrite only runs for a
-- scope that holds a `.*`, so an exact-name scope costs about what
-- `= ANY` did. Use: `queue_in_key_scope(queue_name, $n::TEXT[])` in place of
-- `($n::TEXT[] IS NULL OR queue_name = ANY($n))`, and
-- `queue_in_key_scope(jobs.queue_name, k.queues)` in place of
-- `(cardinality(k.queues) = 0 OR '*' = ANY(k.queues) OR jobs.queue_name = ANY(k.queues))`.

SET LOCAL lock_timeout = '5s';

CREATE OR REPLACE FUNCTION queue_in_key_scope(p_queue_name TEXT, p_scope TEXT[])
RETURNS BOOLEAN AS $$
    SELECT p_scope IS NULL
        OR cardinality(p_scope) = 0
        OR '*' = ANY(p_scope)
        OR p_queue_name = ANY(p_scope)
        OR (
            strpos(array_to_string(p_scope, chr(31)), '.*') > 0
            AND p_queue_name LIKE ANY(string_to_array(
                regexp_replace(
                    replace(replace(replace(array_to_string(p_scope, chr(31)), '\', '\\'), '%', '\%'), '_', '\_'),
                    '([^' || chr(31) || '])\.\*(' || chr(31) || '|$)', '\1.%\2', 'g'),
                chr(31)))
        );
$$ LANGUAGE sql STABLE PARALLEL SAFE;

COMMENT ON FUNCTION queue_in_key_scope IS
    'Whether an API key queue scope (NULL/empty/* = all, prefix.* = names starting with prefix., else exact) covers a queue; mirrors models::queue_scope_allows';
