-- Spooled Cloud Database Schema
-- Migration 6: Convert jobs table to partitioned table for scalability
--
-- This migration creates a partitioned version of the jobs table by created_at month.
-- Benefits:
-- - Faster queries on time-range data
-- - Efficient partition-level VACUUM and maintenance
-- - Easy archival/deletion of old data by dropping partitions
--
-- IMPORTANT: In a production migration, you would:
-- 1. Create the new partitioned table structure
-- 2. Copy data from jobs to jobs_partitioned
-- 3. Rename tables atomically
-- 4. Update indexes and constraints
--
-- This migration creates the infrastructure for new deployments.
-- For existing deployments, run during a maintenance window.

-- ============================================================================
-- PARTITIONED JOBS TABLE
-- ============================================================================

-- Note: If migrating existing data, you need to:
-- 1. Rename existing jobs table: ALTER TABLE jobs RENAME TO jobs_old;
-- 2. Create partitioned table with this structure
-- 3. Create partitions for existing date ranges
-- 4. Copy data with proper partition routing
-- 5. Swap tables and verify

-- Create partitioned jobs table (for new deployments)
-- Existing jobs table from migration 1 is NOT dropped - this is additive
CREATE TABLE IF NOT EXISTS jobs_partitioned (
    id TEXT NOT NULL,
    organization_id TEXT NOT NULL,
    queue_name TEXT NOT NULL,
    status TEXT NOT NULL DEFAULT 'pending',
    payload JSONB NOT NULL,
    result JSONB,
    priority INT NOT NULL DEFAULT 0,
    max_retries INT NOT NULL DEFAULT 3,
    retry_count INT NOT NULL DEFAULT 0,
    timeout_seconds INT NOT NULL DEFAULT 300,
    created_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    scheduled_at TIMESTAMPTZ,
    started_at TIMESTAMPTZ,
    completed_at TIMESTAMPTZ,
    expires_at TIMESTAMPTZ,
    idempotency_key TEXT,
    parent_job_id TEXT,
    dependencies_met BOOLEAN DEFAULT TRUE,
    assigned_worker_id TEXT,
    lease_id TEXT,
    lease_expires_at TIMESTAMPTZ,
    last_error TEXT,
    tags JSONB,
    CONSTRAINT jobs_partitioned_pkey PRIMARY KEY (id, created_at)
) PARTITION BY RANGE (created_at);

-- Create initial partitions (current month + 3 months ahead)
-- These are created if they don't exist

-- Function to create partition for a specific month
CREATE OR REPLACE FUNCTION create_jobs_partition_for_month(partition_date DATE)
RETURNS TEXT AS $$
DECLARE
    partition_name TEXT;
    start_date DATE;
    end_date DATE;
BEGIN
    partition_name := 'jobs_p' || to_char(partition_date, 'YYYY_MM');
    start_date := date_trunc('month', partition_date)::DATE;
    end_date := (date_trunc('month', partition_date) + INTERVAL '1 month')::DATE;
    
    -- Check if partition already exists
    IF EXISTS (
        SELECT 1 FROM pg_tables 
        WHERE tablename = partition_name
    ) THEN
        RETURN 'Partition ' || partition_name || ' already exists';
    END IF;
    
    -- Create the partition
    EXECUTE format(
        'CREATE TABLE IF NOT EXISTS %I PARTITION OF jobs_partitioned FOR VALUES FROM (%L) TO (%L)',
        partition_name, start_date, end_date
    );
    
    -- Create indexes on the partition
    EXECUTE format(
        'CREATE INDEX IF NOT EXISTS %I ON %I (organization_id, queue_name, status, priority DESC, created_at)',
        partition_name || '_idx_queue', partition_name
    );
    EXECUTE format(
        'CREATE INDEX IF NOT EXISTS %I ON %I (status, scheduled_at) WHERE status = ''scheduled''',
        partition_name || '_idx_scheduled', partition_name
    );
    EXECUTE format(
        'CREATE INDEX IF NOT EXISTS %I ON %I (status, lease_expires_at) WHERE status = ''processing''',
        partition_name || '_idx_lease', partition_name
    );
    
    RETURN 'Created partition ' || partition_name || ' for ' || start_date || ' to ' || end_date;
END;
$$ LANGUAGE plpgsql;

-- Create partitions for current period
SELECT create_jobs_partition_for_month(CURRENT_DATE);
SELECT create_jobs_partition_for_month((CURRENT_DATE + INTERVAL '1 month')::DATE);
SELECT create_jobs_partition_for_month((CURRENT_DATE + INTERVAL '2 months')::DATE);
SELECT create_jobs_partition_for_month((CURRENT_DATE + INTERVAL '3 months')::DATE);

-- ============================================================================
-- PARTITION MAINTENANCE PROCEDURE
-- ============================================================================

-- Procedure to maintain partitions automatically
-- Should be called daily via cron or pg_cron
CREATE OR REPLACE PROCEDURE maintain_jobs_partitions()
LANGUAGE plpgsql
AS $$
DECLARE
    partition_record RECORD;
    retention_months INT := 6; -- Keep 6 months of data
    retention_date DATE;
    months_ahead INT := 3; -- Create partitions 3 months ahead
    future_date DATE;
    result TEXT;
BEGIN
    -- Calculate retention cutoff
    retention_date := (CURRENT_DATE - (retention_months || ' months')::INTERVAL)::DATE;
    
    RAISE NOTICE 'Starting partition maintenance at %, retention cutoff: %', CURRENT_DATE, retention_date;
    
    -- Drop old partitions beyond retention period
    FOR partition_record IN
        SELECT schemaname, tablename
        FROM pg_tables
        WHERE tablename LIKE 'jobs_p%'
        AND tablename ~ '^jobs_p[0-9]{4}_[0-9]{2}$'
        ORDER BY tablename
    LOOP
        -- Extract date from partition name (jobs_pYYYY_MM)
        DECLARE
            partition_date DATE;
        BEGIN
            partition_date := to_date(substring(partition_record.tablename from 7), 'YYYY_MM');
            
            IF partition_date < retention_date THEN
                -- Archive before dropping (optional - depends on archival strategy)
                RAISE NOTICE 'Dropping old partition: %', partition_record.tablename;
                EXECUTE format('DROP TABLE IF EXISTS %I.%I', partition_record.schemaname, partition_record.tablename);
            END IF;
        EXCEPTION WHEN OTHERS THEN
            RAISE WARNING 'Could not parse partition name: %', partition_record.tablename;
        END;
    END LOOP;
    
    -- Create future partitions
    FOR i IN 0..months_ahead LOOP
        future_date := date_trunc('month', CURRENT_DATE + (i || ' months')::INTERVAL)::DATE;
        SELECT create_jobs_partition_for_month(future_date) INTO result;
        IF result NOT LIKE '%already exists%' THEN
            RAISE NOTICE '%', result;
        END IF;
    END LOOP;
    
    RAISE NOTICE 'Partition maintenance completed';
END;
$$;

-- ============================================================================
-- IDEMPOTENCY INDEX FOR PARTITIONED TABLE
-- ============================================================================

-- Unique constraint for idempotency (partial index on non-null keys)
-- Note: For partitioned tables, unique indexes must include all partition key columns
CREATE UNIQUE INDEX IF NOT EXISTS idx_jobs_partitioned_idempotency 
ON jobs_partitioned (organization_id, idempotency_key, created_at)
WHERE idempotency_key IS NOT NULL;

-- ============================================================================
-- VIEW TO TRANSPARENTLY USE PARTITIONED OR REGULAR TABLE
-- ============================================================================

-- This view allows switching between partitioned and non-partitioned tables
-- by changing this view definition rather than application code
-- For now, it points to the original jobs table
-- To switch to partitioned: CREATE OR REPLACE VIEW jobs_view AS SELECT * FROM jobs_partitioned;
CREATE OR REPLACE VIEW jobs_view AS SELECT * FROM jobs;

COMMENT ON VIEW jobs_view IS 'Abstraction layer to switch between jobs and jobs_partitioned';

-- ============================================================================
-- HELPER FUNCTIONS
-- ============================================================================

-- Function to check partition health
CREATE OR REPLACE FUNCTION check_partition_health()
RETURNS TABLE (
    partition_name TEXT,
    partition_range TEXT,
    row_count BIGINT,
    size_mb NUMERIC
) AS $$
BEGIN
    RETURN QUERY
    SELECT 
        t.tablename::TEXT,
        (SELECT pg_get_expr(relpartbound, oid) FROM pg_class WHERE relname = t.tablename)::TEXT,
        (SELECT reltuples::BIGINT FROM pg_class WHERE relname = t.tablename),
        ROUND((pg_table_size(t.tablename::regclass)::NUMERIC / 1024 / 1024), 2)
    FROM pg_tables t
    WHERE t.tablename LIKE 'jobs_p%'
    ORDER BY t.tablename;
END;
$$ LANGUAGE plpgsql;

COMMENT ON FUNCTION check_partition_health IS 'Returns health stats for all job partitions';

-- ============================================================================
-- NOTES FOR PRODUCTION MIGRATION
-- ============================================================================

-- To migrate existing data to partitioned table:
--
-- 1. Create all necessary partitions first:
--    SELECT create_jobs_partition_for_month(date_trunc('month', min(created_at))::DATE)
--    FROM jobs;
--    -- Repeat for each month of data
--
-- 2. Copy data (during low traffic):
--    INSERT INTO jobs_partitioned SELECT * FROM jobs;
--
-- 3. Verify row counts match:
--    SELECT COUNT(*) FROM jobs;
--    SELECT COUNT(*) FROM jobs_partitioned;
--
-- 4. Rename tables:
--    ALTER TABLE jobs RENAME TO jobs_legacy;
--    ALTER TABLE jobs_partitioned RENAME TO jobs;
--
-- 5. Update foreign keys and triggers as needed
--
-- 6. Update the jobs_view to point to new table

-- Schedule maintenance with pg_cron (if available):
-- SELECT cron.schedule('maintain-partitions', '0 2 * * *', 'CALL maintain_jobs_partitions()');
