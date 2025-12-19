# Backup Restore Drill Guide

This document provides procedures for conducting regular backup restore drills to validate backup integrity and practice recovery procedures.

## Overview

**Purpose**: Verify that backups are valid and the team can successfully restore data within RTO targets.

**Frequency**: Monthly (at minimum)

**Target RTO**: 30 minutes for full restore from backup

**Target RPO**: 5 minutes (with WAL archiving) or last daily backup (without WAL)

---

## Pre-Drill Checklist

Before starting a restore drill:

- [ ] Identify a recent backup to test (ideally from the last 24 hours)
- [ ] Ensure test environment is available (isolated from production)
- [ ] Confirm you have database admin credentials
- [ ] Verify sufficient disk space for restored data (2x backup size minimum)
- [ ] Schedule the drill during low-traffic period (if testing in staging)
- [ ] Notify relevant team members

---

## Restore Drill Procedure

### Step 1: Locate the Backup

```bash
# List available backups
ls -lh /var/backups/spooled/

# Or check the latest symlink
ls -l /var/backups/spooled/latest.dump
```

Note the backup file details:
- **Filename**: ________________________________
- **Size**: ________________________________
- **Date Created**: ________________________________

### Step 2: Create Test Environment

```bash
# Create a test database (use unique name with timestamp)
TEST_DB="spooled_restore_drill_$(date +%Y%m%d_%H%M%S)"
createdb -h localhost -U spooled "$TEST_DB"
```

### Step 3: Restore the Backup

Record the start time: ________________________________

```bash
# Set backup file path
BACKUP_FILE="/var/backups/spooled/latest.dump"

# Restore to test database
pg_restore \
  --dbname="postgres://spooled@localhost:5432/$TEST_DB" \
  --verbose \
  --no-owner \
  --no-privileges \
  "$BACKUP_FILE"
```

Record the end time: ________________________________

**Restore Duration**: ________________________________

### Step 4: Verify Data Integrity

Run the verification queries:

```bash
TEST_URL="postgres://spooled@localhost:5432/$TEST_DB"

# Check table counts
psql "$TEST_URL" -c "
SELECT 
  (SELECT COUNT(*) FROM jobs) as jobs,
  (SELECT COUNT(*) FROM organizations) as organizations,
  (SELECT COUNT(*) FROM workers) as workers,
  (SELECT COUNT(*) FROM schedules) as schedules,
  (SELECT COUNT(*) FROM api_keys) as api_keys;
"

# Check for critical tables
psql "$TEST_URL" -c "
SELECT table_name, pg_size_pretty(pg_total_relation_size(table_name::regclass)) as size
FROM information_schema.tables 
WHERE table_schema = 'public' AND table_type = 'BASE TABLE'
ORDER BY pg_total_relation_size(table_name::regclass) DESC;
"

# Check constraints integrity
psql "$TEST_URL" -c "
SELECT COUNT(*) as constraint_count 
FROM information_schema.table_constraints 
WHERE table_schema = 'public';
"

# Check recent data exists (jobs from last 24h before backup)
psql "$TEST_URL" -c "
SELECT status, COUNT(*) as count 
FROM jobs 
WHERE created_at > NOW() - INTERVAL '7 days'
GROUP BY status;
"
```

### Step 5: Record Results

| Metric | Value |
|--------|-------|
| Jobs Count | |
| Organizations Count | |
| Workers Count | |
| Schedules Count | |
| API Keys Count | |
| Total Tables | |
| Constraints Count | |

### Step 6: Cleanup

```bash
# Drop the test database
dropdb "$TEST_DB"
```

---

## Automated Verification Script

Use the existing verification script for automated drills:

```bash
# Run automated verification
./scripts/verify_backup.sh /var/backups/spooled/latest.dump
```

Expected output:
```
VERIFY_SUCCESS backup=/path/to/backup tables=N jobs=N
```

---

## Drill Results Template

Copy and fill this template for each drill:

```markdown
## Restore Drill Report

**Date**: YYYY-MM-DD
**Conducted By**: 
**Backup Tested**: 
**Backup Date**: 

### Timing

| Phase | Duration |
|-------|----------|
| Backup location | |
| Test DB creation | |
| Restore | |
| Verification | |
| Cleanup | |
| **Total** | |

### Data Verification

| Table | Expected | Actual | Match? |
|-------|----------|--------|--------|
| jobs | | | |
| organizations | | | |
| workers | | | |
| schedules | | | |

### Issues Encountered

- [ ] None
- [ ] Issue 1: ________________
- [ ] Issue 2: ________________

### Actions Required

- [ ] None
- [ ] Action 1: ________________
- [ ] Action 2: ________________

### RTO Assessment

- [ ] Met target (< 30 minutes)
- [ ] Exceeded target by: ________________

### Sign-off

Drill completed successfully: [ ] Yes [ ] No

Notes: ________________________________________________
```

---

## Expected Failure Modes

### PostgreSQL Restore Failures

| Failure | Cause | Resolution |
|---------|-------|------------|
| "Permission denied" | User lacks createdb privilege | Use superuser or grant permission |
| "Disk full" | Insufficient space | Free space or use larger volume |
| "Connection refused" | PostgreSQL not running | Start PostgreSQL service |
| "Database exists" | Test DB name collision | Use unique timestamp-based name |
| "Invalid format" | Corrupt backup file | Test earlier backup, investigate backup process |

### Data Integrity Issues

| Issue | Cause | Resolution |
|-------|-------|------------|
| Missing tables | Incomplete backup | Check pg_dump logs, verify backup process |
| Zero row counts | Backup taken during truncate | Schedule backups during low-activity periods |
| Foreign key violations | Partial restore | Disable constraints during restore, re-enable after |
| Sequence misalignment | pg_restore limitation | Run `SELECT setval()` to reset sequences |

### Redis Recovery Notes

Redis data is **not critical** for restore - the system operates in fallback mode:

- Workers poll database directly (every 5 seconds)
- Rate limiting uses conservative fallback (10 req/min)
- Real-time notifications disabled until Redis reconnects

Redis data is automatically rebuilt as the system operates.

---

## Disaster Recovery Scenarios

### Scenario A: Database Corruption

1. Stop application immediately
2. Assess damage scope
3. Restore from latest clean backup
4. Apply WAL logs if available (PITR)
5. Verify data integrity
6. Resume application

### Scenario B: Complete Infrastructure Loss

1. Provision new infrastructure
2. Restore database from off-site backup
3. Start Redis fresh (will rebuild)
4. Deploy application
5. Verify health endpoints
6. Monitor for issues

### Scenario C: Accidental Data Deletion

1. Identify affected data and time of deletion
2. If within PITR window: restore to point-in-time
3. If outside window: restore from daily backup
4. Calculate data loss (RPO breach)
5. Document incident

---

## Monthly Drill Schedule

| Month | Week | Environment | Type |
|-------|------|-------------|------|
| Jan | 2 | Staging | Full restore |
| Feb | 2 | Staging | Full restore |
| Mar | 2 | Staging | PITR test |
| Apr | 2 | Staging | Full restore |
| May | 2 | Staging | Full restore |
| Jun | 2 | Staging | Full infrastructure rebuild |
| Jul | 2 | Staging | Full restore |
| Aug | 2 | Staging | Full restore |
| Sep | 2 | Staging | PITR test |
| Oct | 2 | Staging | Full restore |
| Nov | 2 | Staging | Full restore |
| Dec | 2 | Staging | Full infrastructure rebuild |

---

## Related Documentation

- [Operations Guide](operations.md) - General operational procedures
- [Backup Script](../../scripts/backup_database.sh) - Automated backup
- [Verify Script](../../scripts/verify_backup.sh) - Automated verification
