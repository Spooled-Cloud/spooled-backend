#!/bin/bash
# Spooled Backend - Partition Maintenance Script
#
# This script maintains job table partitions:
# - Creates future partitions (3 months ahead)
# - Drops old partitions beyond retention period (6 months)
# - Reports partition health statistics
#
# Run daily via cron:
#   0 2 * * * /path/to/partition_maintenance.sh >> /var/log/spooled/partition.log 2>&1
#
# Or use pg_cron directly in PostgreSQL for simpler scheduling.

set -euo pipefail

# Configuration
DB_URL="${DATABASE_DIRECT_URL:-${DATABASE_URL:-postgres://spooled:spooled@localhost:5432/spooled}}"
LOG_PREFIX="[$(date '+%Y-%m-%d %H:%M:%S')] partition_maintenance:"

log() {
    echo "$LOG_PREFIX $1"
}

error() {
    echo "$LOG_PREFIX ERROR: $1" >&2
}

# Check if psql is available
if ! command -v psql &> /dev/null; then
    error "psql is not installed or not in PATH"
    exit 1
fi

log "Starting partition maintenance"

# Run the maintenance procedure
psql "$DB_URL" -v ON_ERROR_STOP=1 <<EOF
-- Run partition maintenance
CALL maintain_jobs_partitions();

-- Show partition health
SELECT * FROM check_partition_health();
EOF

if [ $? -eq 0 ]; then
    log "Partition maintenance completed successfully"
else
    error "Partition maintenance failed"
    exit 1
fi

# Optionally run VACUUM ANALYZE on partitions
log "Running VACUUM ANALYZE on job partitions..."
psql "$DB_URL" -c "VACUUM ANALYZE jobs;" 2>/dev/null || true
psql "$DB_URL" -c "VACUUM ANALYZE jobs_partitioned;" 2>/dev/null || true

log "All maintenance tasks completed"

