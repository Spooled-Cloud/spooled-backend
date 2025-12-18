-- Worker Column Aliases Migration
-- The code uses different column names than the original schema:
--   - Code: max_concurrent_jobs, current_job_count, created_at, updated_at
--   - Schema: max_concurrency, current_jobs, registered_at
-- This migration adds the new columns and migrates data

-- Add new columns with code-expected names
ALTER TABLE workers ADD COLUMN IF NOT EXISTS max_concurrent_jobs INT;
ALTER TABLE workers ADD COLUMN IF NOT EXISTS current_job_count INT DEFAULT 0;

-- Add created_at and updated_at columns (code expects these, schema has registered_at)
ALTER TABLE workers ADD COLUMN IF NOT EXISTS created_at TIMESTAMPTZ DEFAULT CURRENT_TIMESTAMP;
ALTER TABLE workers ADD COLUMN IF NOT EXISTS updated_at TIMESTAMPTZ DEFAULT CURRENT_TIMESTAMP;

-- Migrate existing data from old columns to new columns
UPDATE workers SET 
    max_concurrent_jobs = COALESCE(max_concurrent_jobs, max_concurrency),
    current_job_count = COALESCE(current_job_count, current_jobs),
    created_at = COALESCE(created_at, registered_at),
    updated_at = COALESCE(updated_at, registered_at);

-- Set defaults on new columns
ALTER TABLE workers ALTER COLUMN max_concurrent_jobs SET DEFAULT 5;
ALTER TABLE workers ALTER COLUMN current_job_count SET DEFAULT 0;

-- Add NOT NULL constraint after data migration
ALTER TABLE workers ALTER COLUMN max_concurrent_jobs SET NOT NULL;
ALTER TABLE workers ALTER COLUMN current_job_count SET NOT NULL;
ALTER TABLE workers ALTER COLUMN created_at SET NOT NULL;
ALTER TABLE workers ALTER COLUMN updated_at SET NOT NULL;

-- Update constraint to use new column name
ALTER TABLE workers DROP CONSTRAINT IF EXISTS workers_concurrency_check;
ALTER TABLE workers ADD CONSTRAINT workers_concurrency_check 
    CHECK (max_concurrent_jobs > 0 AND max_concurrent_jobs <= 1000);

COMMENT ON COLUMN workers.max_concurrent_jobs IS 'Maximum concurrent jobs this worker can process';
COMMENT ON COLUMN workers.current_job_count IS 'Current number of jobs being processed';
COMMENT ON COLUMN workers.created_at IS 'Worker creation timestamp';
COMMENT ON COLUMN workers.updated_at IS 'Worker last update timestamp';
COMMENT ON COLUMN workers.max_concurrency IS 'DEPRECATED: Use max_concurrent_jobs instead';
COMMENT ON COLUMN workers.current_jobs IS 'DEPRECATED: Use current_job_count instead';
COMMENT ON COLUMN workers.registered_at IS 'DEPRECATED: Use created_at instead';
