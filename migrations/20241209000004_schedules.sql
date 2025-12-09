-- Schedules table for cron-based recurring jobs
-- This migration adds support for scheduled/recurring job execution

-- Create function to update updated_at timestamp
CREATE OR REPLACE FUNCTION update_updated_at_column()
RETURNS TRIGGER AS $$
BEGIN
    NEW.updated_at = CURRENT_TIMESTAMP;
    RETURN NEW;
END;
$$ language 'plpgsql';

-- Create schedules table
CREATE TABLE schedules (
    id TEXT PRIMARY KEY DEFAULT gen_random_uuid()::TEXT,
    organization_id TEXT NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    name TEXT NOT NULL,
    description TEXT,
    cron_expression TEXT NOT NULL,
    timezone TEXT NOT NULL DEFAULT 'UTC',
    queue_name TEXT NOT NULL,
    payload_template JSONB NOT NULL DEFAULT '{}',
    priority INT NOT NULL DEFAULT 0,
    max_retries INT NOT NULL DEFAULT 3,
    timeout_seconds INT NOT NULL DEFAULT 300,
    is_active BOOLEAN NOT NULL DEFAULT TRUE,
    last_run_at TIMESTAMPTZ,
    next_run_at TIMESTAMPTZ,
    run_count BIGINT NOT NULL DEFAULT 0,
    tags JSONB,
    metadata JSONB,
    created_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP
);

-- Indexes for efficient querying
CREATE INDEX idx_schedules_organization_id ON schedules(organization_id);
CREATE INDEX idx_schedules_is_active ON schedules(is_active) WHERE is_active = TRUE;
CREATE INDEX idx_schedules_next_run_at ON schedules(next_run_at) WHERE is_active = TRUE;
CREATE INDEX idx_schedules_queue_name ON schedules(queue_name);

-- Schedule runs history table
CREATE TABLE schedule_runs (
    id TEXT PRIMARY KEY DEFAULT gen_random_uuid()::TEXT,
    schedule_id TEXT NOT NULL REFERENCES schedules(id) ON DELETE CASCADE,
    job_id TEXT REFERENCES jobs(id) ON DELETE SET NULL,
    status TEXT NOT NULL DEFAULT 'pending', -- pending, running, completed, failed
    error_message TEXT,
    started_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    completed_at TIMESTAMPTZ
);

-- Index for efficient history lookup
CREATE INDEX idx_schedule_runs_schedule_id ON schedule_runs(schedule_id);
CREATE INDEX idx_schedule_runs_started_at ON schedule_runs(started_at);

-- Enable Row Level Security
ALTER TABLE schedules ENABLE ROW LEVEL SECURITY;
ALTER TABLE schedule_runs ENABLE ROW LEVEL SECURITY;

-- RLS Policies for schedules
CREATE POLICY schedules_org_isolation ON schedules
    FOR ALL
    USING (organization_id = current_setting('app.current_org_id', true));

-- RLS Policies for schedule_runs (through schedules)
CREATE POLICY schedule_runs_org_isolation ON schedule_runs
    FOR ALL
    USING (
        schedule_id IN (
            SELECT id FROM schedules 
            WHERE organization_id = current_setting('app.current_org_id', true)
        )
    );

-- Function to process due schedules
-- Called by the scheduler periodically
CREATE OR REPLACE FUNCTION process_due_schedules()
RETURNS INTEGER
LANGUAGE plpgsql
AS $$
DECLARE
    v_count INTEGER := 0;
    v_schedule RECORD;
    v_job_id TEXT;
    v_run_id TEXT;
BEGIN
    -- Lock and process schedules that are due
    FOR v_schedule IN
        SELECT * FROM schedules
        WHERE is_active = TRUE
          AND next_run_at IS NOT NULL
          AND next_run_at <= NOW()
        FOR UPDATE SKIP LOCKED
    LOOP
        BEGIN
            -- Create job from schedule
            v_job_id := gen_random_uuid()::TEXT;
            
            INSERT INTO jobs (
                id, organization_id, queue_name, status, payload,
                priority, max_retries, timeout_seconds, tags,
                created_at, updated_at
            )
            VALUES (
                v_job_id, v_schedule.organization_id, v_schedule.queue_name,
                'pending', v_schedule.payload_template, v_schedule.priority,
                v_schedule.max_retries, v_schedule.timeout_seconds,
                v_schedule.tags, NOW(), NOW()
            );

            -- Record the run
            v_run_id := gen_random_uuid()::TEXT;
            INSERT INTO schedule_runs (id, schedule_id, job_id, status, started_at, completed_at)
            VALUES (v_run_id, v_schedule.id, v_job_id, 'completed', NOW(), NOW());

            -- Update schedule
            -- Note: next_run_at needs to be calculated by the application
            -- as PostgreSQL doesn't have native cron parsing
            UPDATE schedules
            SET 
                last_run_at = NOW(),
                run_count = run_count + 1,
                next_run_at = NULL, -- Will be recalculated by application
                updated_at = NOW()
            WHERE id = v_schedule.id;

            v_count := v_count + 1;

        EXCEPTION WHEN OTHERS THEN
            -- Record failed run
            INSERT INTO schedule_runs (id, schedule_id, status, error_message, started_at, completed_at)
            VALUES (gen_random_uuid()::TEXT, v_schedule.id, 'failed', SQLERRM, NOW(), NOW());
        END;
    END LOOP;

    RETURN v_count;
END;
$$;

-- Trigger to update updated_at on schedules
CREATE TRIGGER update_schedules_updated_at
    BEFORE UPDATE ON schedules
    FOR EACH ROW
    EXECUTE FUNCTION update_updated_at_column();

-- Comment on tables
COMMENT ON TABLE schedules IS 'Cron-based recurring job schedules';
COMMENT ON TABLE schedule_runs IS 'History of schedule executions';
COMMENT ON FUNCTION process_due_schedules() IS 'Process all schedules that are due for execution';

