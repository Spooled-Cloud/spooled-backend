-- Migration: Add email login codes table and Stripe billing fields
-- Created: 2024-12-13

-- Email login codes table for passwordless authentication
CREATE TABLE IF NOT EXISTS email_login_codes (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    email VARCHAR(255) NOT NULL,
    code VARCHAR(6) NOT NULL,
    organization_id VARCHAR(255) REFERENCES organizations(id) ON DELETE CASCADE,
    expires_at TIMESTAMPTZ NOT NULL,
    used_at TIMESTAMPTZ,
    attempts INTEGER NOT NULL DEFAULT 0,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

-- Index for looking up codes by email
CREATE INDEX IF NOT EXISTS idx_email_login_codes_email ON email_login_codes(email);

-- Index for cleanup of expired codes
CREATE INDEX IF NOT EXISTS idx_email_login_codes_expires ON email_login_codes(expires_at);

-- Add Stripe billing fields to organizations
ALTER TABLE organizations ADD COLUMN IF NOT EXISTS stripe_customer_id VARCHAR(255);
ALTER TABLE organizations ADD COLUMN IF NOT EXISTS stripe_subscription_id VARCHAR(255);
ALTER TABLE organizations ADD COLUMN IF NOT EXISTS stripe_subscription_status VARCHAR(50);
ALTER TABLE organizations ADD COLUMN IF NOT EXISTS stripe_current_period_end TIMESTAMPTZ;
ALTER TABLE organizations ADD COLUMN IF NOT EXISTS stripe_cancel_at_period_end BOOLEAN DEFAULT FALSE;

-- Index for Stripe customer ID lookups
CREATE INDEX IF NOT EXISTS idx_organizations_stripe_customer ON organizations(stripe_customer_id) WHERE stripe_customer_id IS NOT NULL;

-- Index for Stripe subscription ID lookups (for webhook handling)
CREATE INDEX IF NOT EXISTS idx_organizations_stripe_subscription ON organizations(stripe_subscription_id) WHERE stripe_subscription_id IS NOT NULL;

-- Cleanup function for expired login codes (run via pg_cron or similar)
-- DELETE FROM email_login_codes WHERE expires_at < NOW() - INTERVAL '1 day';
