-- Job Dependencies Migration
-- Enables job workflow support with parent-child relationships

-- Create job dependencies table for many-to-many relationships
CREATE TABLE IF NOT EXISTS job_dependencies (
    id TEXT PRIMARY KEY DEFAULT gen_random_uuid()::TEXT,
    job_id TEXT NOT NULL REFERENCES jobs(id) ON DELETE CASCADE,
    depends_on_job_id TEXT NOT NULL REFERENCES jobs(id) ON DELETE CASCADE,
    dependency_type TEXT NOT NULL DEFAULT 'all', -- 'all' = wait for all, 'any' = wait for any
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    
    -- Prevent duplicate dependencies
    CONSTRAINT unique_job_dependency UNIQUE (job_id, depends_on_job_id),
    -- Prevent self-dependency
    CONSTRAINT no_self_dependency CHECK (job_id != depends_on_job_id)
);

-- Create index for efficient lookups
CREATE INDEX IF NOT EXISTS idx_job_dependencies_job_id ON job_dependencies(job_id);
CREATE INDEX IF NOT EXISTS idx_job_dependencies_depends_on ON job_dependencies(depends_on_job_id);

-- Add workflow columns to jobs table
ALTER TABLE jobs ADD COLUMN IF NOT EXISTS workflow_id TEXT;
ALTER TABLE jobs ADD COLUMN IF NOT EXISTS workflow_step INT DEFAULT 0;
ALTER TABLE jobs ADD COLUMN IF NOT EXISTS dependency_mode TEXT DEFAULT 'all'; -- 'all' or 'any'
ALTER TABLE jobs ADD COLUMN IF NOT EXISTS dependencies_met BOOLEAN DEFAULT TRUE;

-- Create index for workflow queries
CREATE INDEX IF NOT EXISTS idx_jobs_workflow_id ON jobs(workflow_id) WHERE workflow_id IS NOT NULL;
CREATE INDEX IF NOT EXISTS idx_jobs_dependencies_met ON jobs(dependencies_met) WHERE dependencies_met = FALSE;

-- Create workflows table for tracking workflow state
CREATE TABLE IF NOT EXISTS workflows (
    id TEXT PRIMARY KEY DEFAULT gen_random_uuid()::TEXT,
    organization_id TEXT NOT NULL REFERENCES organizations(id),
    name TEXT NOT NULL,
    description TEXT,
    status TEXT NOT NULL DEFAULT 'pending', -- pending, running, completed, failed, cancelled
    total_jobs INT NOT NULL DEFAULT 0,
    completed_jobs INT NOT NULL DEFAULT 0,
    failed_jobs INT NOT NULL DEFAULT 0,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    started_at TIMESTAMPTZ,
    completed_at TIMESTAMPTZ,
    metadata JSONB
);

-- Index for workflow queries
CREATE INDEX IF NOT EXISTS idx_workflows_organization_id ON workflows(organization_id);
CREATE INDEX IF NOT EXISTS idx_workflows_status ON workflows(status);

-- RLS for job_dependencies
ALTER TABLE job_dependencies ENABLE ROW LEVEL SECURITY;

CREATE POLICY job_dependencies_org_isolation ON job_dependencies
    USING (
        EXISTS (
            SELECT 1 FROM jobs 
            WHERE jobs.id = job_dependencies.job_id 
            AND jobs.organization_id = current_setting('app.current_org_id', true)
        )
    );

-- RLS for workflows
ALTER TABLE workflows ENABLE ROW LEVEL SECURITY;

CREATE POLICY workflows_org_isolation ON workflows
    USING (organization_id = current_setting('app.current_org_id', true));

-- Function to check if job dependencies are met
CREATE OR REPLACE FUNCTION check_job_dependencies_met(p_job_id TEXT)
RETURNS BOOLEAN AS $$
DECLARE
    v_dependency_mode TEXT;
    v_total_deps INT;
    v_completed_deps INT;
    v_any_completed BOOLEAN;
BEGIN
    -- Get the dependency mode for this job
    SELECT dependency_mode INTO v_dependency_mode
    FROM jobs WHERE id = p_job_id;
    
    -- Count dependencies
    SELECT 
        COUNT(*),
        COUNT(*) FILTER (WHERE j.status = 'completed'),
        bool_or(j.status = 'completed')
    INTO v_total_deps, v_completed_deps, v_any_completed
    FROM job_dependencies jd
    JOIN jobs j ON j.id = jd.depends_on_job_id
    WHERE jd.job_id = p_job_id;
    
    -- No dependencies means job is ready
    IF v_total_deps = 0 THEN
        RETURN TRUE;
    END IF;
    
    -- Check based on dependency mode
    IF v_dependency_mode = 'any' THEN
        RETURN COALESCE(v_any_completed, FALSE);
    ELSE
        -- 'all' mode (default)
        RETURN v_completed_deps = v_total_deps;
    END IF;
END;
$$ LANGUAGE plpgsql;

-- Trigger to update dependencies_met when dependencies complete
CREATE OR REPLACE FUNCTION update_dependent_jobs()
RETURNS TRIGGER AS $$
BEGIN
    -- Only act when a job is completed
    IF NEW.status = 'completed' AND (OLD.status IS NULL OR OLD.status != 'completed') THEN
        -- Update all jobs that depend on this job
        UPDATE jobs
        SET 
            dependencies_met = check_job_dependencies_met(id),
            updated_at = NOW()
        WHERE id IN (
            SELECT job_id FROM job_dependencies WHERE depends_on_job_id = NEW.id
        );
    END IF;
    
    RETURN NEW;
END;
$$ LANGUAGE plpgsql;

-- Create trigger for dependency updates
DROP TRIGGER IF EXISTS trigger_update_dependent_jobs ON jobs;
CREATE TRIGGER trigger_update_dependent_jobs
    AFTER UPDATE OF status ON jobs
    FOR EACH ROW
    EXECUTE FUNCTION update_dependent_jobs();

-- Update workflow progress when jobs complete
CREATE OR REPLACE FUNCTION update_workflow_progress()
RETURNS TRIGGER AS $$
BEGIN
    IF NEW.workflow_id IS NOT NULL AND NEW.status IN ('completed', 'failed') THEN
        -- Update workflow counters
        UPDATE workflows
        SET 
            completed_jobs = (
                SELECT COUNT(*) FROM jobs 
                WHERE workflow_id = NEW.workflow_id AND status = 'completed'
            ),
            failed_jobs = (
                SELECT COUNT(*) FROM jobs 
                WHERE workflow_id = NEW.workflow_id AND status IN ('failed', 'deadletter')
            ),
            status = CASE
                WHEN (SELECT COUNT(*) FROM jobs WHERE workflow_id = NEW.workflow_id AND status IN ('pending', 'scheduled', 'processing')) = 0
                     AND (SELECT COUNT(*) FROM jobs WHERE workflow_id = NEW.workflow_id AND status IN ('failed', 'deadletter')) = 0
                THEN 'completed'
                WHEN (SELECT COUNT(*) FROM jobs WHERE workflow_id = NEW.workflow_id AND status IN ('failed', 'deadletter')) > 0
                     AND (SELECT COUNT(*) FROM jobs WHERE workflow_id = NEW.workflow_id AND status IN ('pending', 'scheduled', 'processing')) = 0
                THEN 'failed'
                ELSE 'running'
            END,
            completed_at = CASE
                WHEN (SELECT COUNT(*) FROM jobs WHERE workflow_id = NEW.workflow_id AND status IN ('pending', 'scheduled', 'processing')) = 0
                THEN NOW()
                ELSE NULL
            END
        WHERE id = NEW.workflow_id;
    END IF;
    
    RETURN NEW;
END;
$$ LANGUAGE plpgsql;

DROP TRIGGER IF EXISTS trigger_update_workflow_progress ON jobs;
CREATE TRIGGER trigger_update_workflow_progress
    AFTER UPDATE OF status ON jobs
    FOR EACH ROW
    EXECUTE FUNCTION update_workflow_progress();

COMMENT ON TABLE job_dependencies IS 'Stores job dependency relationships for workflow support';
COMMENT ON TABLE workflows IS 'Stores workflow definitions and progress tracking';
COMMENT ON FUNCTION check_job_dependencies_met IS 'Checks if all/any dependencies are met for a job';

