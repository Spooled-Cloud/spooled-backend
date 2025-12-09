-- Spooled Cloud Database Schema
-- Migration 2: Table partitioning strategy for dead tuple management
-- 
-- NOTE: This migration sets up the infrastructure for partitioning.
-- In production, you would convert the jobs table to a partitioned table
-- during a maintenance window. This migration creates the helper procedures.

-- ============================================================================
-- PARTITION MANAGEMENT PROCEDURES
-- ============================================================================

-- Function to create a new monthly partition
CREATE OR REPLACE FUNCTION create_jobs_partition(partition_date DATE)
RETURNS TEXT AS $$
DECLARE
    partition_name TEXT;
    start_date DATE;
    end_date DATE;
BEGIN
    partition_name := 'jobs_' || to_char(partition_date, 'YYYY_MM');
    start_date := date_trunc('month', partition_date)::DATE;
    end_date := (date_trunc('month', partition_date) + INTERVAL '1 month')::DATE;
    
    -- Check if partition already exists
    IF EXISTS (
        SELECT 1 FROM pg_tables 
        WHERE tablename = partition_name
    ) THEN
        RETURN 'Partition ' || partition_name || ' already exists';
    END IF;
    
    -- Create the partition (when using partitioned tables)
    -- This is a template - actual partitioning requires converting the table
    -- EXECUTE format(
    --     'CREATE TABLE %I PARTITION OF jobs FOR VALUES FROM (%L) TO (%L)',
    --     partition_name, start_date, end_date
    -- );
    
    RETURN 'Partition template: ' || partition_name || ' (' || start_date || ' to ' || end_date || ')';
END;
$$ LANGUAGE plpgsql;

-- Procedure to maintain partitions (create new, drop old)
CREATE OR REPLACE PROCEDURE maintain_job_partitions()
LANGUAGE plpgsql
AS $$
DECLARE
    partition_record RECORD;
    retention_date DATE;
BEGIN
    -- Retention: 90 days
    retention_date := CURRENT_DATE - INTERVAL '90 days';

    -- Log start
    RAISE NOTICE 'Starting partition maintenance, retention date: %', retention_date;

    -- Find and drop old partitions (when partitioning is enabled)
    -- FOR partition_record IN
    --     SELECT schemaname, tablename
    --     FROM pg_tables
    --     WHERE tablename LIKE 'jobs_%'
    --     AND to_date(substring(tablename, 6, 7), 'YYYY_MM') < retention_date
    -- LOOP
    --     EXECUTE 'DROP TABLE ' || partition_record.schemaname || '.' || partition_record.tablename;
    --     RAISE NOTICE 'Dropped partition %', partition_record.tablename;
    -- END LOOP;

    -- Create next month's partition if it doesn't exist
    PERFORM create_jobs_partition(CURRENT_DATE + INTERVAL '1 month');

    RAISE NOTICE 'Partition maintenance completed';
END;
$$;

-- ============================================================================
-- ARCHIVE TABLE FOR COMPLETED JOBS
-- ============================================================================

-- Archive table for long-term storage of completed jobs
-- This table uses BRIN indexes (100x smaller than B-Tree)
CREATE TABLE jobs_archive (
    id TEXT NOT NULL,
    organization_id TEXT NOT NULL,
    queue_name TEXT NOT NULL,
    status TEXT NOT NULL,
    payload JSONB NOT NULL,
    result JSONB,
    retry_count INT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL,
    completed_at TIMESTAMPTZ,
    archived_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    
    PRIMARY KEY (id, created_at)
);

-- BRIN index (perfect for append-only archived data)
CREATE INDEX idx_jobs_archive_created_brin ON jobs_archive USING BRIN (created_at) WITH (pages_per_range = 64);
CREATE INDEX idx_jobs_archive_org_created ON jobs_archive(organization_id, created_at DESC);

COMMENT ON TABLE jobs_archive IS 'Archived completed jobs for historical analysis (BRIN indexed)';

-- Function to archive old completed jobs
CREATE OR REPLACE FUNCTION archive_completed_jobs(days_old INT DEFAULT 30)
RETURNS BIGINT AS $$
DECLARE
    archived_count BIGINT;
BEGIN
    WITH archived AS (
        DELETE FROM jobs
        WHERE status = 'completed'
        AND completed_at < CURRENT_TIMESTAMP - (days_old || ' days')::INTERVAL
        RETURNING *
    )
    INSERT INTO jobs_archive (
        id, organization_id, queue_name, status, payload, result,
        retry_count, created_at, completed_at
    )
    SELECT 
        id, organization_id, queue_name, status, payload, result,
        retry_count, created_at, completed_at
    FROM archived;
    
    GET DIAGNOSTICS archived_count = ROW_COUNT;
    
    RAISE NOTICE 'Archived % completed jobs older than % days', archived_count, days_old;
    
    RETURN archived_count;
END;
$$ LANGUAGE plpgsql;

-- ============================================================================
-- CLEANUP PROCEDURES
-- ============================================================================

-- Procedure to clean up expired jobs
CREATE OR REPLACE PROCEDURE cleanup_expired_jobs()
LANGUAGE plpgsql
AS $$
DECLARE
    deleted_count BIGINT;
BEGIN
    DELETE FROM jobs
    WHERE expires_at IS NOT NULL
    AND expires_at < CURRENT_TIMESTAMP
    AND status IN ('pending', 'scheduled');
    
    GET DIAGNOSTICS deleted_count = ROW_COUNT;
    
    IF deleted_count > 0 THEN
        RAISE NOTICE 'Cleaned up % expired jobs', deleted_count;
    END IF;
END;
$$;

-- Procedure to cleanup old webhook deliveries
CREATE OR REPLACE PROCEDURE cleanup_old_webhook_deliveries(days_old INT DEFAULT 30)
LANGUAGE plpgsql
AS $$
DECLARE
    deleted_count BIGINT;
BEGIN
    DELETE FROM webhook_deliveries
    WHERE created_at < CURRENT_TIMESTAMP - (days_old || ' days')::INTERVAL;
    
    GET DIAGNOSTICS deleted_count = ROW_COUNT;
    
    IF deleted_count > 0 THEN
        RAISE NOTICE 'Cleaned up % old webhook deliveries', deleted_count;
    END IF;
END;
$$;

-- Procedure to cleanup old job history
CREATE OR REPLACE PROCEDURE cleanup_old_job_history(days_old INT DEFAULT 90)
LANGUAGE plpgsql
AS $$
DECLARE
    deleted_count BIGINT;
BEGIN
    -- Only delete history for jobs that no longer exist or are archived
    DELETE FROM job_history
    WHERE created_at < CURRENT_TIMESTAMP - (days_old || ' days')::INTERVAL
    AND job_id NOT IN (SELECT id FROM jobs);
    
    GET DIAGNOSTICS deleted_count = ROW_COUNT;
    
    IF deleted_count > 0 THEN
        RAISE NOTICE 'Cleaned up % old job history entries', deleted_count;
    END IF;
END;
$$;

-- ============================================================================
-- SCHEDULED MAINTENANCE (using pg_cron if available)
-- ============================================================================

-- Note: These would be scheduled with pg_cron in production:
-- SELECT cron.schedule('archive-completed-jobs', '0 3 * * *', 'SELECT archive_completed_jobs(30)');
-- SELECT cron.schedule('cleanup-expired-jobs', '*/15 * * * *', 'CALL cleanup_expired_jobs()');
-- SELECT cron.schedule('cleanup-webhook-deliveries', '0 4 * * 0', 'CALL cleanup_old_webhook_deliveries(30)');
-- SELECT cron.schedule('cleanup-job-history', '0 4 * * 0', 'CALL cleanup_old_job_history(90)');
-- SELECT cron.schedule('maintain-partitions', '0 2 * * 0', 'CALL maintain_job_partitions()');

COMMENT ON FUNCTION archive_completed_jobs IS 'Archive completed jobs older than N days to archive table';
COMMENT ON PROCEDURE cleanup_expired_jobs IS 'Delete pending/scheduled jobs past their expiration';

