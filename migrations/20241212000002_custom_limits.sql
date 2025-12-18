-- Custom Limits Migration
-- Adds per-organization custom limits override capability

-- Add custom_limits column to organizations table
-- This allows admins to override default plan limits for specific organizations
ALTER TABLE organizations ADD COLUMN IF NOT EXISTS custom_limits JSONB;

-- Example custom_limits structure:
-- {
--   "max_jobs_per_day": 50000,
--   "max_queues": 100,
--   "max_workers": 50,
--   "max_api_keys": 100,
--   "max_schedules": 200,
--   "max_workflows": 100,
--   "max_webhooks": 50,
--   "max_payload_size_bytes": 2097152,
--   "rate_limit_requests_per_second": 200
-- }
-- 
-- Only the limits that differ from plan defaults need to be specified.
-- null values or missing keys fall back to plan defaults.

COMMENT ON COLUMN organizations.custom_limits IS 'Optional per-org limit overrides as JSONB. Keys match PlanLimits fields. Null values use plan defaults.';
