-- Fast, deterministic lookup key for API-key authentication.
--
-- Every live key shares the `sp_live_` prefix (test keys share `sp_test_`), so the
-- previous indexed lookup on `key_prefix` (first 8 chars) matched effectively every
-- key: authentication ran `WHERE key_prefix = 'sp_live_' ... LIMIT 100` and then
-- bcrypt-verified each of the up-to-100 candidates. A single bogus key therefore
-- cost ~100 bcrypt verifications (~7.7s observed) — slow for legitimate cold-cache
-- auth and a cheap, unauthenticated CPU-amplification vector.
--
-- `lookup_hash = hex(sha256(plaintext_key))` is unique per key, so authentication
-- becomes a single indexed row read followed by exactly one bcrypt verify. It is
-- NOT a security boundary — bcrypt still verifies the credential after the lookup;
-- the hash only replaces the non-selective prefix scan.
--
-- Existing rows stay NULL: the plaintext key is not recoverable from the bcrypt
-- hash, so they cannot be backfilled here. They are backfilled lazily the next time
-- each key authenticates successfully (the auth path computes the hash from the
-- presented plaintext and writes it), after which that key uses the fast path.
ALTER TABLE api_keys ADD COLUMN IF NOT EXISTS lookup_hash TEXT;

-- Partial index: only backfilled rows carry a lookup_hash, and NULLs never need to
-- be looked up. Keeps the index small and lets the legacy fallback scan target
-- `lookup_hash IS NULL` rows only.
CREATE INDEX IF NOT EXISTS idx_api_keys_lookup_hash
    ON api_keys (lookup_hash)
    WHERE lookup_hash IS NOT NULL;
