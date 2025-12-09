-- API Key Prefix Migration
-- Adds key_prefix column for efficient API key lookup optimization

-- Add key_prefix column to api_keys table
-- This stores the first 8 characters of the raw API key for fast indexed lookup
-- Without this, every authentication requires scanning all active API keys
ALTER TABLE api_keys ADD COLUMN IF NOT EXISTS key_prefix TEXT;

-- Create index for fast prefix-based lookup
-- This allows O(1) lookup instead of O(n) scan for API key authentication
CREATE INDEX IF NOT EXISTS idx_api_keys_prefix ON api_keys(key_prefix) WHERE key_prefix IS NOT NULL;

-- Backfill existing keys (they will have NULL prefix and fall back to full scan)
-- New keys will have prefix populated at creation time

COMMENT ON COLUMN api_keys.key_prefix IS 'First 8 characters of raw API key for fast indexed lookup';
