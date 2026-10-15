-- Logout used to record session revocation only in Redis. An eviction
-- (allkeys-lru) or a GET timeout then accepted the still-valid refresh token.
-- This row is the source of truth; Redis is a cache of it.

CREATE TABLE IF NOT EXISTS revoked_sessions (
    sid TEXT PRIMARY KEY,
    expires_at TIMESTAMPTZ NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_revoked_sessions_expires_at
    ON revoked_sessions (expires_at);
