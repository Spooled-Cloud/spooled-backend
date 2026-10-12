-- Remember the lease length a job was claimed (or last renewed) with.
--
-- A heartbeat that omits `lease_duration_secs` used to renew the lease for the
-- 30 s server default, so a worker that claimed with `lease_duration_secs: 600`
-- and heartbeats every few minutes lost its job after its first heartbeat. The
-- claim records the granted length here and an omitted heartbeat duration
-- renews for that length. NULL (jobs claimed before this column existed, or by
-- a path that does not record it) keeps the server default.
--
-- Nullable without a default: a catalog-only change, no table rewrite.

SET LOCAL lock_timeout = '5s';

ALTER TABLE jobs ADD COLUMN IF NOT EXISTS lease_duration_secs INTEGER;

COMMENT ON COLUMN jobs.lease_duration_secs IS
    'Lease length in seconds granted by the current claim or the last heartbeat that named one';
