-- Worker Queue Names Migration
-- Adds queue_names array column to support workers processing multiple queues
-- The original schema only had queue_name (singular text) but code expects queue_names (array)

-- Add queue_names array column (nullable to support existing rows)
ALTER TABLE workers ADD COLUMN IF NOT EXISTS queue_names TEXT[] DEFAULT '{}';

-- Migrate existing queue_name data to queue_names array
UPDATE workers 
SET queue_names = ARRAY[queue_name] 
WHERE queue_names = '{}' AND queue_name IS NOT NULL;

-- Create index for efficient queue lookup (GIN index for array containment)
CREATE INDEX IF NOT EXISTS idx_workers_queue_names ON workers USING GIN(queue_names);

-- Add index for combined org + queue_names lookup
CREATE INDEX IF NOT EXISTS idx_workers_org_queues ON workers(organization_id) WHERE status IN ('healthy', 'degraded');

COMMENT ON COLUMN workers.queue_names IS 'Array of queue names this worker processes (supports multi-queue workers)';
COMMENT ON COLUMN workers.queue_name IS 'DEPRECATED: Legacy single queue name, use queue_names array instead';

