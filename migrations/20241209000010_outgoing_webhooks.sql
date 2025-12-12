-- Outgoing Webhooks Feature
-- Allows organizations to configure webhooks for receiving notifications
-- when events occur in the system (job.created, job.completed, etc.)

-- ============================================================================
-- OUTGOING_WEBHOOKS TABLE
-- ============================================================================
CREATE TABLE outgoing_webhooks (
    id TEXT PRIMARY KEY DEFAULT gen_random_uuid()::TEXT,
    organization_id TEXT NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    name TEXT NOT NULL,
    url TEXT NOT NULL,
    secret TEXT,  -- Optional secret for HMAC signing
    events TEXT[] NOT NULL DEFAULT ARRAY[]::TEXT[],
    enabled BOOLEAN NOT NULL DEFAULT TRUE,
    failure_count INT NOT NULL DEFAULT 0,
    last_triggered_at TIMESTAMPTZ,
    last_status TEXT,  -- 'success' or 'failed'
    created_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    
    -- Validate event types
    CONSTRAINT outgoing_webhook_events_check CHECK (
        events <@ ARRAY[
            'job.created',
            'job.started', 
            'job.completed',
            'job.failed',
            'job.cancelled',
            'queue.paused',
            'queue.resumed',
            'worker.registered',
            'worker.deregistered',
            'schedule.triggered'
        ]::TEXT[]
    ),
    CONSTRAINT outgoing_webhook_name_length CHECK (char_length(name) >= 1 AND char_length(name) <= 255),
    CONSTRAINT outgoing_webhook_url_length CHECK (char_length(url) >= 1 AND char_length(url) <= 2048)
);

-- Indexes
CREATE INDEX idx_outgoing_webhooks_org ON outgoing_webhooks(organization_id);
CREATE INDEX idx_outgoing_webhooks_org_enabled ON outgoing_webhooks(organization_id, enabled) WHERE enabled = TRUE;

COMMENT ON TABLE outgoing_webhooks IS 'Webhook configurations for sending notifications to external URLs';
COMMENT ON COLUMN outgoing_webhooks.secret IS 'Optional HMAC secret for signing webhook payloads';
COMMENT ON COLUMN outgoing_webhooks.events IS 'Array of event types to trigger this webhook';

-- ============================================================================
-- OUTGOING_WEBHOOK_DELIVERIES TABLE
-- ============================================================================
CREATE TABLE outgoing_webhook_deliveries (
    id TEXT PRIMARY KEY DEFAULT gen_random_uuid()::TEXT,
    webhook_id TEXT NOT NULL REFERENCES outgoing_webhooks(id) ON DELETE CASCADE,
    event TEXT NOT NULL,
    payload JSONB NOT NULL,
    status TEXT NOT NULL DEFAULT 'pending',
    status_code INT,
    response_body TEXT,
    error TEXT,
    attempts INT NOT NULL DEFAULT 0,
    created_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    delivered_at TIMESTAMPTZ,
    
    CONSTRAINT delivery_status_check CHECK (status IN ('pending', 'success', 'failed'))
);

-- Indexes
CREATE INDEX idx_outgoing_webhook_deliveries_webhook ON outgoing_webhook_deliveries(webhook_id, created_at DESC);
CREATE INDEX idx_outgoing_webhook_deliveries_status ON outgoing_webhook_deliveries(webhook_id, status) WHERE status = 'pending';

COMMENT ON TABLE outgoing_webhook_deliveries IS 'Delivery history for outgoing webhooks';

-- ============================================================================
-- ROW-LEVEL SECURITY
-- ============================================================================
ALTER TABLE outgoing_webhooks ENABLE ROW LEVEL SECURITY;
ALTER TABLE outgoing_webhook_deliveries ENABLE ROW LEVEL SECURITY;

CREATE POLICY org_isolation_outgoing_webhooks ON outgoing_webhooks
    USING (organization_id = current_setting('app.current_org_id', true)::TEXT)
    WITH CHECK (organization_id = current_setting('app.current_org_id', true)::TEXT);

CREATE POLICY org_isolation_outgoing_webhook_deliveries ON outgoing_webhook_deliveries
    USING (webhook_id IN (
        SELECT id FROM outgoing_webhooks 
        WHERE organization_id = current_setting('app.current_org_id', true)::TEXT
    ));

-- ============================================================================
-- TRIGGERS
-- ============================================================================
CREATE TRIGGER outgoing_webhooks_updated_at
    BEFORE UPDATE ON outgoing_webhooks
    FOR EACH ROW
    EXECUTE FUNCTION update_updated_at();
