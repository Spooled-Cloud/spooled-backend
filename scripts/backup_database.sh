#!/bin/bash
# Spooled Backend - Database Backup Script
#
# Creates a compressed logical backup of the PostgreSQL database.
#
# Usage:
#   ./scripts/backup_database.sh
#
# Environment variables:
#   DATABASE_DIRECT_URL - PostgreSQL connection string (bypasses PgBouncer)
#   BACKUP_DIR         - Backup destination directory (default: /var/backups/spooled)
#   BACKUP_RETENTION   - Days to keep backups (default: 30)
#
# Schedule with cron:
#   0 2 * * * /opt/spooled/scripts/backup_database.sh >> /var/log/spooled/backup.log 2>&1

set -euo pipefail

# Configuration
BACKUP_DIR="${BACKUP_DIR:-/var/backups/spooled}"
BACKUP_RETENTION="${BACKUP_RETENTION:-30}"
TIMESTAMP=$(date +%Y%m%d_%H%M%S)
BACKUP_FILE="$BACKUP_DIR/spooled_$TIMESTAMP.dump"

# Logging
log() {
    echo "[$(date '+%Y-%m-%d %H:%M:%S')] $1"
}

error() {
    echo "[$(date '+%Y-%m-%d %H:%M:%S')] ERROR: $1" >&2
}

# Check prerequisites
if ! command -v pg_dump &> /dev/null; then
    error "pg_dump is not installed"
    exit 1
fi

# Get database URL
if [ -z "${DATABASE_DIRECT_URL:-}" ]; then
    if [ -z "${DATABASE_URL:-}" ]; then
        error "DATABASE_DIRECT_URL or DATABASE_URL must be set"
        exit 1
    fi
    DB_URL="$DATABASE_URL"
else
    DB_URL="$DATABASE_DIRECT_URL"
fi

# Create backup directory
mkdir -p "$BACKUP_DIR"

log "Starting database backup..."
log "Destination: $BACKUP_FILE"

# Perform backup
START_TIME=$(date +%s)

pg_dump "$DB_URL" \
    --format=custom \
    --compress=9 \
    --verbose \
    --file="$BACKUP_FILE" \
    2>&1 | while read -r line; do log "pg_dump: $line"; done

END_TIME=$(date +%s)
DURATION=$((END_TIME - START_TIME))

# Verify backup file
if [ ! -f "$BACKUP_FILE" ]; then
    error "Backup file was not created"
    exit 1
fi

BACKUP_SIZE=$(du -h "$BACKUP_FILE" | cut -f1)
log "Backup completed in ${DURATION}s, size: $BACKUP_SIZE"

# Create latest symlink
ln -sf "$BACKUP_FILE" "$BACKUP_DIR/latest.dump"
log "Updated latest.dump symlink"

# Cleanup old backups
log "Cleaning up backups older than $BACKUP_RETENTION days..."
DELETED_COUNT=$(find "$BACKUP_DIR" -name "spooled_*.dump" -mtime +"$BACKUP_RETENTION" -delete -print | wc -l)
log "Deleted $DELETED_COUNT old backup(s)"

# List current backups
log "Current backups:"
ls -lh "$BACKUP_DIR"/*.dump 2>/dev/null | tail -10 || true

log "Backup process completed successfully"

# Output for monitoring (can be parsed by backup monitoring tools)
echo "BACKUP_SUCCESS timestamp=$TIMESTAMP duration=${DURATION}s size=$BACKUP_SIZE"
