#!/bin/bash
# Spooled Backend - Performance Baseline Test
#
# Runs a comprehensive performance baseline to measure:
# - Maximum throughput (jobs/sec)
# - Latency percentiles (p50, p90, p95, p99)
# - Error rates under load
# - System resource usage
#
# Prerequisites:
#   - k6 installed (brew install k6)
#   - Backend running with test data
#   - API key for testing
#
# Usage:
#   ./loadtest/run-perf-baseline.sh
#
# Environment variables:
#   BASE_URL  - API base URL (default: http://localhost:8080)
#   API_KEY   - API key for authentication
#   OUTPUT_DIR - Directory for results (default: ./loadtest/results)

set -euo pipefail

# Configuration
BASE_URL="${BASE_URL:-http://localhost:8080}"
API_KEY="${API_KEY:-}"
OUTPUT_DIR="${OUTPUT_DIR:-./loadtest/results}"
TIMESTAMP=$(date +%Y%m%d_%H%M%S)

# Colors for output
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
NC='\033[0m' # No Color

log() {
    echo -e "${BLUE}[$(date '+%H:%M:%S')]${NC} $1"
}

success() {
    echo -e "${GREEN}[$(date '+%H:%M:%S')] ✓${NC} $1"
}

warn() {
    echo -e "${YELLOW}[$(date '+%H:%M:%S')] ⚠${NC} $1"
}

error() {
    echo -e "${RED}[$(date '+%H:%M:%S')] ✗${NC} $1" >&2
}

# Check prerequisites
check_prerequisites() {
    log "Checking prerequisites..."
    
    if ! command -v k6 &> /dev/null; then
        error "k6 is not installed. Install with: brew install k6"
        exit 1
    fi
    
    if [ -z "$API_KEY" ]; then
        warn "API_KEY not set. Tests may fail authentication."
        warn "Set API_KEY environment variable or create a test key."
    fi
    
    # Check if backend is accessible
    if ! curl -sf "${BASE_URL}/health" > /dev/null; then
        error "Backend not accessible at ${BASE_URL}"
        exit 1
    fi
    
    success "Prerequisites check passed"
}

# Create output directory
setup_output() {
    mkdir -p "$OUTPUT_DIR"
    RESULTS_FILE="$OUTPUT_DIR/baseline_${TIMESTAMP}.json"
    SUMMARY_FILE="$OUTPUT_DIR/baseline_${TIMESTAMP}_summary.txt"
    log "Results will be saved to: $OUTPUT_DIR"
}

# Run smoke test first
run_smoke_test() {
    log "Running smoke test (quick validation)..."
    
    k6 run \
        --env "BASE_URL=${BASE_URL}" \
        --env "API_KEY=${API_KEY}" \
        --env "SCENARIO=smoke" \
        --quiet \
        loadtest/k6-load-test.js
    
    if [ $? -eq 0 ]; then
        success "Smoke test passed"
    else
        error "Smoke test failed - aborting baseline"
        exit 1
    fi
}

# Run baseline test
run_baseline_test() {
    log "Running performance baseline test (1000 req/sec for 5 min)..."
    log "This will help establish your system's baseline performance."
    
    k6 run \
        --env "BASE_URL=${BASE_URL}" \
        --env "API_KEY=${API_KEY}" \
        --env "SCENARIO=baseline" \
        --out "json=${RESULTS_FILE}" \
        loadtest/k6-load-test.js 2>&1 | tee "$SUMMARY_FILE"
    
    success "Baseline test completed"
}

# Run throughput test
run_throughput_test() {
    log "Running high throughput test (ramping to 10k req/sec)..."
    warn "This is an aggressive test - monitor system resources!"
    
    THROUGHPUT_FILE="$OUTPUT_DIR/throughput_${TIMESTAMP}.json"
    THROUGHPUT_SUMMARY="$OUTPUT_DIR/throughput_${TIMESTAMP}_summary.txt"
    
    k6 run \
        --env "BASE_URL=${BASE_URL}" \
        --env "API_KEY=${API_KEY}" \
        --env "SCENARIO=throughput" \
        --out "json=${THROUGHPUT_FILE}" \
        loadtest/k6-load-test.js 2>&1 | tee "$THROUGHPUT_SUMMARY"
    
    success "Throughput test completed"
}

# Generate summary report
generate_report() {
    log "Generating performance report..."
    
    REPORT_FILE="$OUTPUT_DIR/report_${TIMESTAMP}.md"
    
    cat > "$REPORT_FILE" << EOF
# Spooled Backend Performance Baseline Report

**Date:** $(date '+%Y-%m-%d %H:%M:%S')
**Target:** ${BASE_URL}
**Test Duration:** Baseline (5 min @ 1k/s) + Throughput (15 min ramping to 10k/s)

## Test Configuration

- **Baseline Scenario:** 1,000 requests/second for 5 minutes
- **Throughput Scenario:** Ramp from 100 to 10,000 requests/second

## Performance Targets

| Metric | Target | Description |
|--------|--------|-------------|
| p50 Latency | < 50ms | Median response time |
| p95 Latency | < 200ms | 95th percentile |
| p99 Latency | < 500ms | 99th percentile |
| Error Rate | < 1% | Failed requests |
| Throughput | 10,000/sec | Jobs enqueued per second |

## Results

See the following files for detailed results:
- \`baseline_${TIMESTAMP}_summary.txt\` - Baseline test output
- \`throughput_${TIMESTAMP}_summary.txt\` - Throughput test output
- \`*.json\` files - Raw k6 metrics for analysis

## Key Metrics to Monitor

During the test, monitor these system metrics:

### PostgreSQL
- Active connections (should stay below pool limit)
- Query latency
- Dead tuples / autovacuum activity
- CPU and I/O usage

### Redis
- Operations per second
- Memory usage
- Connected clients

### Application
- CPU usage
- Memory usage
- Open file descriptors
- Network I/O

## Recommendations

Based on the results, consider:

1. **If p99 > 500ms:** Scale up database or add read replicas
2. **If errors > 1%:** Check for connection pool exhaustion
3. **If throughput < 5k/sec:** Profile slow queries, optimize indexes
4. **If memory growing:** Check for connection leaks

EOF

    success "Report generated: $REPORT_FILE"
}

# Main execution
main() {
    echo ""
    echo "========================================"
    echo " Spooled Backend Performance Baseline"
    echo "========================================"
    echo ""
    
    check_prerequisites
    setup_output
    
    echo ""
    log "Starting performance baseline..."
    log "Target: ${BASE_URL}"
    echo ""
    
    run_smoke_test
    echo ""
    
    run_baseline_test
    echo ""
    
    read -p "Run aggressive throughput test (10k/sec target)? [y/N] " -n 1 -r
    echo ""
    if [[ $REPLY =~ ^[Yy]$ ]]; then
        run_throughput_test
        echo ""
    fi
    
    generate_report
    
    echo ""
    echo "========================================"
    success "Performance baseline complete!"
    echo "========================================"
    echo ""
    log "Results saved to: $OUTPUT_DIR"
    log "Report: $OUTPUT_DIR/report_${TIMESTAMP}.md"
    echo ""
}

main "$@"
