-- Spooled Cloud Database Schema
-- Initial migration: Core tables and Row-Level Security

-- Enable required extensions
CREATE EXTENSION IF NOT EXISTS "pgcrypto";
CREATE EXTENSION IF NOT EXISTS "uuid-ossp";

-- ============================================================================
-- ORGANIZATIONS TABLE
-- ============================================================================
CREATE TABLE organizations (
    id TEXT PRIMARY KEY DEFAULT gen_random_uuid()::TEXT,
    name TEXT NOT NULL,
    slug TEXT UNIQUE NOT NULL,
    plan_tier TEXT NOT NULL DEFAULT 'free',
    billing_email TEXT,
    settings JSONB NOT NULL DEFAULT '{}',
    created_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE INDEX idx_organizations_slug ON organizations(slug);

COMMENT ON TABLE organizations IS 'Multi-tenant organizations (tenants)';
COMMENT ON COLUMN organizations.plan_tier IS 'Billing plan: free, starter, pro, enterprise';

-- ============================================================================
-- JOBS TABLE (Core queue table)
-- ============================================================================
CREATE TABLE jobs (
    id TEXT PRIMARY KEY DEFAULT gen_random_uuid()::TEXT,
    organization_id TEXT NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    queue_name TEXT NOT NULL,
    status TEXT NOT NULL DEFAULT 'pending',
    
    -- Payload and results (JSONB for flexibility)
    payload JSONB NOT NULL,
    result JSONB,
    
    -- Execution tracking
    retry_count INT NOT NULL DEFAULT 0,
    max_retries INT NOT NULL DEFAULT 3,
    last_error TEXT,
    
    -- Timing (crucial for queue ordering)
    created_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    scheduled_at TIMESTAMPTZ,
    started_at TIMESTAMPTZ,
    completed_at TIMESTAMPTZ,
    expires_at TIMESTAMPTZ,
    
    -- Priority & configuration
    priority INT NOT NULL DEFAULT 0,
    tags JSONB,
    timeout_seconds INT NOT NULL DEFAULT 300,
    
    -- Parent-child relationships (DAGs)
    parent_job_id TEXT REFERENCES jobs(id) ON DELETE CASCADE,
    
    -- Webhooks & callbacks
    completion_webhook TEXT,
    
    -- Worker assignment with lease
    assigned_worker_id TEXT,
    lease_id TEXT,
    lease_expires_at TIMESTAMPTZ,
    
    -- Idempotency (FIXED: DO UPDATE SET RETURNING pattern)
    idempotency_key TEXT,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    
    -- Constraints
    CONSTRAINT jobs_status_check CHECK (status IN ('pending', 'scheduled', 'processing', 'completed', 'failed', 'deadletter', 'cancelled')),
    CONSTRAINT jobs_priority_check CHECK (priority >= -100 AND priority <= 100),
    CONSTRAINT jobs_timeout_check CHECK (timeout_seconds > 0 AND timeout_seconds <= 86400)
);

-- CRITICAL INDEXES for queue performance
CREATE INDEX idx_jobs_org_queue_status ON jobs(organization_id, queue_name, status);
CREATE INDEX idx_jobs_org_pending ON jobs(organization_id, status) WHERE status IN ('pending', 'scheduled');
CREATE INDEX idx_jobs_org_scheduled ON jobs(organization_id, scheduled_at) WHERE scheduled_at IS NOT NULL AND status IN ('pending', 'scheduled');
CREATE INDEX idx_jobs_org_created ON jobs(organization_id, created_at DESC);
CREATE INDEX idx_jobs_deadletter ON jobs(organization_id, status) WHERE status = 'deadletter';
CREATE INDEX idx_jobs_expires ON jobs(expires_at) WHERE expires_at IS NOT NULL;
CREATE INDEX idx_jobs_priority ON jobs(organization_id, priority DESC, created_at ASC) WHERE status = 'pending';
CREATE UNIQUE INDEX idx_jobs_idempotency ON jobs(organization_id, idempotency_key) WHERE idempotency_key IS NOT NULL;
CREATE INDEX idx_jobs_lease_expires ON jobs(lease_expires_at) WHERE status = 'processing' AND lease_expires_at IS NOT NULL;

COMMENT ON TABLE jobs IS 'Core job queue table with transactional integrity';
COMMENT ON COLUMN jobs.idempotency_key IS 'Unique key per org for deduplication (DO UPDATE SET RETURNING pattern)';
COMMENT ON COLUMN jobs.lease_id IS 'Unique lease identifier for job processing';

-- ============================================================================
-- WORKERS TABLE
-- ============================================================================
CREATE TABLE workers (
    id TEXT PRIMARY KEY DEFAULT gen_random_uuid()::TEXT,
    organization_id TEXT NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    queue_name TEXT NOT NULL,
    hostname TEXT NOT NULL,
    worker_type TEXT,
    
    -- Capacity tracking
    max_concurrency INT NOT NULL DEFAULT 5,
    current_jobs INT NOT NULL DEFAULT 0,
    
    -- Health
    status TEXT NOT NULL DEFAULT 'healthy',
    last_heartbeat TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    
    -- Metadata
    metadata JSONB NOT NULL DEFAULT '{}',
    version TEXT,
    registered_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    
    -- Constraints
    CONSTRAINT workers_status_check CHECK (status IN ('healthy', 'degraded', 'offline', 'draining')),
    CONSTRAINT workers_concurrency_check CHECK (max_concurrency > 0 AND max_concurrency <= 100)
);

CREATE INDEX idx_workers_org_queue_status ON workers(organization_id, queue_name, status);
CREATE INDEX idx_workers_org_healthy ON workers(organization_id, status) WHERE status = 'healthy';
CREATE INDEX idx_workers_heartbeat ON workers(organization_id, last_heartbeat);

COMMENT ON TABLE workers IS 'Worker registration and health tracking';

-- ============================================================================
-- JOB_HISTORY TABLE (Audit trail)
-- ============================================================================
CREATE TABLE job_history (
    id TEXT PRIMARY KEY DEFAULT gen_random_uuid()::TEXT,
    job_id TEXT NOT NULL REFERENCES jobs(id) ON DELETE CASCADE,
    event_type TEXT NOT NULL,
    worker_id TEXT,
    details JSONB NOT NULL DEFAULT '{}',
    created_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE INDEX idx_job_history_job_created ON job_history(job_id, created_at DESC);
CREATE INDEX idx_job_history_event_type ON job_history(event_type);

COMMENT ON TABLE job_history IS 'Job lifecycle event audit trail';

-- ============================================================================
-- DEAD_LETTER_QUEUE TABLE
-- ============================================================================
CREATE TABLE dead_letter_queue (
    id TEXT PRIMARY KEY DEFAULT gen_random_uuid()::TEXT,
    job_id TEXT NOT NULL,
    organization_id TEXT NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    queue_name TEXT NOT NULL,
    reason TEXT NOT NULL,
    original_payload JSONB NOT NULL,
    error_details JSONB,
    created_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE INDEX idx_dlq_org_queue ON dead_letter_queue(organization_id, queue_name);
CREATE INDEX idx_dlq_org_created ON dead_letter_queue(organization_id, created_at DESC);

COMMENT ON TABLE dead_letter_queue IS 'Failed jobs after max retries for manual inspection';

-- ============================================================================
-- QUEUE_CONFIG TABLE
-- ============================================================================
CREATE TABLE queue_config (
    id TEXT PRIMARY KEY DEFAULT gen_random_uuid()::TEXT,
    organization_id TEXT NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    queue_name TEXT NOT NULL,
    max_retries INT NOT NULL DEFAULT 3,
    default_timeout INT NOT NULL DEFAULT 300,
    rate_limit INT,
    enabled BOOLEAN NOT NULL DEFAULT TRUE,
    settings JSONB NOT NULL DEFAULT '{}',
    created_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    
    CONSTRAINT queue_config_unique UNIQUE(organization_id, queue_name)
);

CREATE INDEX idx_queue_config_org_queue ON queue_config(organization_id, queue_name);

COMMENT ON TABLE queue_config IS 'Per-queue configuration per organization';

-- ============================================================================
-- API_KEYS TABLE (bcrypt hash - verification in Rust, NOT database)
-- ============================================================================
CREATE TABLE api_keys (
    id TEXT PRIMARY KEY DEFAULT gen_random_uuid()::TEXT,
    organization_id TEXT NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    key_hash TEXT NOT NULL,  -- bcrypt hash (verification happens in Rust backend)
    name TEXT NOT NULL,
    queues TEXT[] NOT NULL DEFAULT ARRAY[]::TEXT[],
    rate_limit INT,
    is_active BOOLEAN NOT NULL DEFAULT TRUE,
    created_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    last_used TIMESTAMPTZ,
    expires_at TIMESTAMPTZ
);

CREATE UNIQUE INDEX idx_api_keys_hash ON api_keys(key_hash);
CREATE INDEX idx_api_keys_org_active ON api_keys(organization_id, is_active);
CREATE INDEX idx_api_keys_last_used ON api_keys(organization_id, last_used DESC);

COMMENT ON TABLE api_keys IS 'API key storage (bcrypt verification in Rust for horizontal scaling)';
COMMENT ON COLUMN api_keys.key_hash IS 'Bcrypt hash - verify in Rust backend, NOT database';

-- ============================================================================
-- WEBHOOK_DELIVERIES TABLE
-- ============================================================================
CREATE TABLE webhook_deliveries (
    id TEXT PRIMARY KEY DEFAULT gen_random_uuid()::TEXT,
    organization_id TEXT NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    provider TEXT NOT NULL,
    event_type TEXT NOT NULL,
    payload JSONB NOT NULL,
    signature TEXT,
    matched_queue TEXT,
    created_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    
    CONSTRAINT webhook_provider_check CHECK (provider IN ('github', 'stripe', 'custom'))
);

CREATE INDEX idx_webhook_org_provider ON webhook_deliveries(organization_id, provider, created_at DESC);
CREATE INDEX idx_webhook_org_created ON webhook_deliveries(organization_id, created_at DESC);

COMMENT ON TABLE webhook_deliveries IS 'Incoming webhook delivery audit log';

-- ============================================================================
-- ROW-LEVEL SECURITY (CRITICAL FOR MULTI-TENANCY)
-- ============================================================================

-- Enable RLS on all tenant tables
ALTER TABLE jobs ENABLE ROW LEVEL SECURITY;
ALTER TABLE workers ENABLE ROW LEVEL SECURITY;
ALTER TABLE job_history ENABLE ROW LEVEL SECURITY;
ALTER TABLE dead_letter_queue ENABLE ROW LEVEL SECURITY;
ALTER TABLE api_keys ENABLE ROW LEVEL SECURITY;
ALTER TABLE webhook_deliveries ENABLE ROW LEVEL SECURITY;
ALTER TABLE queue_config ENABLE ROW LEVEL SECURITY;

-- Organization isolation policies
-- These policies ensure tenants can only access their own data

CREATE POLICY org_isolation_jobs ON jobs
    USING (organization_id = current_setting('app.current_org_id', true)::TEXT)
    WITH CHECK (organization_id = current_setting('app.current_org_id', true)::TEXT);

CREATE POLICY org_isolation_workers ON workers
    USING (organization_id = current_setting('app.current_org_id', true)::TEXT)
    WITH CHECK (organization_id = current_setting('app.current_org_id', true)::TEXT);

CREATE POLICY org_isolation_job_history ON job_history
    USING (job_id IN (
        SELECT id FROM jobs 
        WHERE organization_id = current_setting('app.current_org_id', true)::TEXT
    ));

CREATE POLICY org_isolation_dlq ON dead_letter_queue
    USING (organization_id = current_setting('app.current_org_id', true)::TEXT);

CREATE POLICY org_isolation_api_keys ON api_keys
    USING (organization_id = current_setting('app.current_org_id', true)::TEXT)
    WITH CHECK (organization_id = current_setting('app.current_org_id', true)::TEXT);

CREATE POLICY org_isolation_webhook_deliveries ON webhook_deliveries
    USING (organization_id = current_setting('app.current_org_id', true)::TEXT);

CREATE POLICY org_isolation_queue_config ON queue_config
    USING (organization_id = current_setting('app.current_org_id', true)::TEXT);

COMMENT ON POLICY org_isolation_jobs ON jobs IS 'RLS: Isolate job access by organization';

-- ============================================================================
-- HELPER FUNCTIONS
-- ============================================================================

-- Function to update timestamps automatically
CREATE OR REPLACE FUNCTION update_updated_at()
RETURNS TRIGGER AS $$
BEGIN
    NEW.updated_at = CURRENT_TIMESTAMP;
    RETURN NEW;
END;
$$ LANGUAGE plpgsql;

-- Apply timestamp triggers
CREATE TRIGGER jobs_updated_at
    BEFORE UPDATE ON jobs
    FOR EACH ROW
    EXECUTE FUNCTION update_updated_at();

CREATE TRIGGER organizations_updated_at
    BEFORE UPDATE ON organizations
    FOR EACH ROW
    EXECUTE FUNCTION update_updated_at();

CREATE TRIGGER queue_config_updated_at
    BEFORE UPDATE ON queue_config
    FOR EACH ROW
    EXECUTE FUNCTION update_updated_at();

-- ============================================================================
-- VACUUM TUNING FOR QUEUE WORKLOAD
-- Queue tables have high INSERT/UPDATE/DELETE churn = dead tuple problem
-- ============================================================================

-- Aggressive autovacuum settings for jobs table
ALTER TABLE jobs SET (
    autovacuum_vacuum_threshold = 1000,
    autovacuum_vacuum_scale_factor = 0.001,
    autovacuum_vacuum_cost_delay = 1,
    autovacuum_vacuum_cost_limit = 10000,
    autovacuum_analyze_threshold = 500,
    autovacuum_analyze_scale_factor = 0.0005
);

-- Moderate settings for other high-churn tables
ALTER TABLE job_history SET (
    autovacuum_vacuum_threshold = 500,
    autovacuum_vacuum_scale_factor = 0.01
);

ALTER TABLE workers SET (
    autovacuum_vacuum_threshold = 100,
    autovacuum_vacuum_scale_factor = 0.05
);

COMMENT ON TABLE jobs IS 'Core queue table with aggressive autovacuum tuning for dead tuple management';

-- ============================================================================
-- DEFAULT ORGANIZATION (for development/testing)
-- ============================================================================

INSERT INTO organizations (id, name, slug, plan_tier)
VALUES ('default-org', 'Default Organization', 'default', 'free')
ON CONFLICT (id) DO NOTHING;

