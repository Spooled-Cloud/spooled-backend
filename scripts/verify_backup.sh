#!/bin/bash
# Spooled Backend - Backup Verification Script
#
# Verifies database backup integrity by restoring to a test database.
#
# Usage:
#   ./scripts/verify_backup.sh [backup_file]
#
# If no backup file specified, uses the latest backup.
#
# Run monthly to ensure backups are restorable.

set -euo pipefail

# Configuration
BACKUP_DIR="${BACKUP_DIR:-/var/backups/spooled}"
TEST_DB="spooled_backup_test_$$"

# Logging
log() {
    echo "[$(date '+%Y-%m-%d %H:%M:%S')] $1"
}

error() {
    echo "[$(date '+%Y-%m-%d %H:%M:%S')] ERROR: $1" >&2
}

cleanup() {
    log "Cleaning up test database..."
    dropdb --if-exists "$TEST_DB" 2>/dev/null || true
}

trap cleanup EXIT

# Get backup file
BACKUP_FILE="${1:-$BACKUP_DIR/latest.dump}"

if [ ! -f "$BACKUP_FILE" ]; then
    error "Backup file not found: $BACKUP_FILE"
    exit 1
fi

# Get database connection (need admin access to create test db)
if [ -z "${DATABASE_DIRECT_URL:-}" ]; then
    if [ -z "${DATABASE_URL:-}" ]; then
        error "DATABASE_DIRECT_URL or DATABASE_URL must be set"
        exit 1
    fi
    DB_URL="$DATABASE_URL"
else
    DB_URL="$DATABASE_DIRECT_URL"
fi

# Extract connection details for createdb/dropdb
DB_HOST=$(echo "$DB_URL" | sed -n 's/.*@\([^:\/]*\).*/\1/p')
DB_PORT=$(echo "$DB_URL" | sed -n 's/.*:\([0-9]*\)\/.*/\1/p')
DB_USER=$(echo "$DB_URL" | sed -n 's/.*\/\/\([^:]*\):.*/\1/p')

log "Starting backup verification..."
log "Backup file: $BACKUP_FILE"
log "File size: $(du -h "$BACKUP_FILE" | cut -f1)"
log "File date: $(stat -c %y "$BACKUP_FILE" 2>/dev/null || stat -f %Sm "$BACKUP_FILE")"

# Create test database
log "Creating test database: $TEST_DB"
createdb -h "$DB_HOST" -p "${DB_PORT:-5432}" -U "$DB_USER" "$TEST_DB"

# Restore backup
log "Restoring backup to test database..."
START_TIME=$(date +%s)

pg_restore \
    --dbname="postgres://$DB_USER@$DB_HOST:${DB_PORT:-5432}/$TEST_DB" \
    --verbose \
    --no-owner \
    --no-privileges \
    "$BACKUP_FILE" 2>&1 | while read -r line; do log "pg_restore: $line"; done || true

END_TIME=$(date +%s)
DURATION=$((END_TIME - START_TIME))
log "Restore completed in ${DURATION}s"

# Verify data integrity
log "Verifying data integrity..."

TEST_URL="postgres://$DB_USER@$DB_HOST:${DB_PORT:-5432}/$TEST_DB"

JOBS_COUNT=$(psql "$TEST_URL" -t -c "SELECT COUNT(*) FROM jobs;" 2>/dev/null | tr -d ' ')
ORGS_COUNT=$(psql "$TEST_URL" -t -c "SELECT COUNT(*) FROM organizations;" 2>/dev/null | tr -d ' ')
WORKERS_COUNT=$(psql "$TEST_URL" -t -c "SELECT COUNT(*) FROM workers;" 2>/dev/null | tr -d ' ')
SCHEDULES_COUNT=$(psql "$TEST_URL" -t -c "SELECT COUNT(*) FROM schedules;" 2>/dev/null | tr -d ' ')

log "Data counts:"
log "  - Jobs: $JOBS_COUNT"
log "  - Organizations: $ORGS_COUNT"
log "  - Workers: $WORKERS_COUNT"
log "  - Schedules: $SCHEDULES_COUNT"

# Check for critical tables
TABLES=$(psql "$TEST_URL" -t -c "
    SELECT COUNT(*) FROM information_schema.tables 
    WHERE table_schema = 'public' AND table_type = 'BASE TABLE';
" | tr -d ' ')

log "  - Total tables: $TABLES"

if [ "$TABLES" -lt 5 ]; then
    error "Expected at least 5 tables, found $TABLES"
    exit 1
fi

# Check constraints
CONSTRAINTS=$(psql "$TEST_URL" -t -c "
    SELECT COUNT(*) FROM information_schema.table_constraints 
    WHERE table_schema = 'public';
" | tr -d ' ')

log "  - Constraints: $CONSTRAINTS"

log "=========================================="
log "BACKUP VERIFICATION SUCCESSFUL"
log "=========================================="
log "Backup: $BACKUP_FILE"
log "Restore time: ${DURATION}s"
log "Records verified: jobs=$JOBS_COUNT, orgs=$ORGS_COUNT"

echo "VERIFY_SUCCESS backup=$BACKUP_FILE tables=$TABLES jobs=$JOBS_COUNT"
