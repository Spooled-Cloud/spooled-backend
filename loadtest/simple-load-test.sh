#!/bin/bash
#
# Simple Load Test for Spooled Cloud API
# Uses curl and basic shell scripting - no dependencies required
#
# Usage:
#   ./loadtest/simple-load-test.sh [BASE_URL] [API_KEY] [CONCURRENT] [DURATION]
#
# Example:
#   ./loadtest/simple-load-test.sh http://localhost:8080 sk_test_xxx 10 30
#

set -e

# Configuration
BASE_URL="${1:-http://localhost:8080}"
API_KEY="${2:-sk_test_default}"
CONCURRENT="${3:-10}"
DURATION="${4:-30}"

# Colors
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
NC='\033[0m' # No Color

# Counters (using files for cross-process communication)
TEMP_DIR=$(mktemp -d)
echo "0" > "$TEMP_DIR/success"
echo "0" > "$TEMP_DIR/failure"
echo "0" > "$TEMP_DIR/total"

cleanup() {
    rm -rf "$TEMP_DIR"
    # Kill all background jobs
    jobs -p | xargs -r kill 2>/dev/null || true
}
trap cleanup EXIT

increment() {
    local file="$TEMP_DIR/$1"
    local current=$(cat "$file")
    echo $((current + 1)) > "$file"
}

print_header() {
    echo -e "${BLUE}╔══════════════════════════════════════════════════════════════╗${NC}"
    echo -e "${BLUE}║         Spooled Cloud - Simple Load Test                     ║${NC}"
    echo -e "${BLUE}╚══════════════════════════════════════════════════════════════╝${NC}"
    echo ""
    echo -e "${YELLOW}Configuration:${NC}"
    echo "  Base URL:    $BASE_URL"
    echo "  Concurrent:  $CONCURRENT workers"
    echo "  Duration:    $DURATION seconds"
    echo ""
}

# Test health endpoint
test_health() {
    local start_time=$(date +%s%N)
    local response=$(curl -s -o /dev/null -w "%{http_code}" "$BASE_URL/health")
    local end_time=$(date +%s%N)
    local duration=$(( (end_time - start_time) / 1000000 ))
    
    increment total
    if [ "$response" == "200" ]; then
        increment success
        echo "✓ Health check: ${duration}ms"
    else
        increment failure
        echo "✗ Health check failed: HTTP $response"
    fi
}

# Test job enqueue
test_enqueue() {
    local queue_name="test-queue-$(( RANDOM % 5 + 1 ))"
    local payload=$(cat <<EOF
{
    "queue_name": "$queue_name",
    "payload": {"task": "test", "timestamp": $(date +%s)},
    "priority": $(( RANDOM % 20 - 10 )),
    "max_retries": 3
}
EOF
)
    
    local start_time=$(date +%s%N)
    local response=$(curl -s -o /dev/null -w "%{http_code}" \
        -X POST "$BASE_URL/api/v1/jobs" \
        -H "Content-Type: application/json" \
        -H "Authorization: Bearer $API_KEY" \
        -d "$payload")
    local end_time=$(date +%s%N)
    local duration=$(( (end_time - start_time) / 1000000 ))
    
    increment total
    if [ "$response" == "201" ] || [ "$response" == "200" ]; then
        increment success
        echo "✓ Enqueue job: ${duration}ms"
    else
        increment failure
        echo "✗ Enqueue failed: HTTP $response"
    fi
}

# Test job list
test_list_jobs() {
    local start_time=$(date +%s%N)
    local response=$(curl -s -o /dev/null -w "%{http_code}" \
        "$BASE_URL/api/v1/jobs?limit=10" \
        -H "Authorization: Bearer $API_KEY")
    local end_time=$(date +%s%N)
    local duration=$(( (end_time - start_time) / 1000000 ))
    
    increment total
    if [ "$response" == "200" ]; then
        increment success
        echo "✓ List jobs: ${duration}ms"
    else
        increment failure
        echo "✗ List jobs failed: HTTP $response"
    fi
}

# Test queue stats
test_queue_stats() {
    local start_time=$(date +%s%N)
    local response=$(curl -s -o /dev/null -w "%{http_code}" \
        "$BASE_URL/api/v1/queues" \
        -H "Authorization: Bearer $API_KEY")
    local end_time=$(date +%s%N)
    local duration=$(( (end_time - start_time) / 1000000 ))
    
    increment total
    if [ "$response" == "200" ]; then
        increment success
        echo "✓ Queue list: ${duration}ms"
    else
        increment failure
        echo "✗ Queue list failed: HTTP $response"
    fi
}

# Worker function that runs tests in a loop
worker() {
    local worker_id=$1
    local end_time=$(($(date +%s) + DURATION))
    
    while [ $(date +%s) -lt $end_time ]; do
        # Randomly select a test
        local test_type=$(( RANDOM % 100 ))
        
        if [ $test_type -lt 10 ]; then
            test_health
        elif [ $test_type -lt 60 ]; then
            test_enqueue
        elif [ $test_type -lt 80 ]; then
            test_list_jobs
        else
            test_queue_stats
        fi
        
        # Small random delay
        sleep 0.$(( RANDOM % 3 + 1 ))
    done
}

# Progress reporter
progress_reporter() {
    local end_time=$(($(date +%s) + DURATION + 5))
    local start_time=$(date +%s)
    
    while [ $(date +%s) -lt $end_time ]; do
        sleep 5
        local elapsed=$(($(date +%s) - start_time))
        local success=$(cat "$TEMP_DIR/success")
        local failure=$(cat "$TEMP_DIR/failure")
        local total=$(cat "$TEMP_DIR/total")
        local rps=0
        if [ $elapsed -gt 0 ]; then
            rps=$(echo "scale=2; $total / $elapsed" | bc)
        fi
        
        echo ""
        echo -e "${BLUE}[Progress @ ${elapsed}s] Total: $total | Success: $success | Failed: $failure | RPS: $rps${NC}"
    done
}

# Main execution
main() {
    print_header
    
    # Pre-flight check
    echo -e "${YELLOW}Pre-flight check...${NC}"
    local health_response=$(curl -s -o /dev/null -w "%{http_code}" "$BASE_URL/health" 2>/dev/null || echo "000")
    if [ "$health_response" != "200" ]; then
        echo -e "${RED}Error: API not accessible at $BASE_URL (HTTP $health_response)${NC}"
        exit 1
    fi
    echo -e "${GREEN}✓ API is healthy${NC}"
    echo ""
    
    echo -e "${YELLOW}Starting load test with $CONCURRENT concurrent workers for $DURATION seconds...${NC}"
    echo ""
    
    # Start progress reporter in background
    progress_reporter &
    local reporter_pid=$!
    
    # Start workers
    local pids=""
    for i in $(seq 1 $CONCURRENT); do
        worker $i &
        pids="$pids $!"
    done
    
    # Wait for all workers to complete
    for pid in $pids; do
        wait $pid 2>/dev/null || true
    done
    
    # Stop progress reporter
    kill $reporter_pid 2>/dev/null || true
    
    # Final results
    echo ""
    echo ""
    echo -e "${BLUE}╔══════════════════════════════════════════════════════════════╗${NC}"
    echo -e "${BLUE}║                    Final Results                             ║${NC}"
    echo -e "${BLUE}╚══════════════════════════════════════════════════════════════╝${NC}"
    
    local success=$(cat "$TEMP_DIR/success")
    local failure=$(cat "$TEMP_DIR/failure")
    local total=$(cat "$TEMP_DIR/total")
    local success_rate=0
    if [ $total -gt 0 ]; then
        success_rate=$(echo "scale=2; $success * 100 / $total" | bc)
    fi
    local rps=$(echo "scale=2; $total / $DURATION" | bc)
    
    echo ""
    echo -e "  Total Requests:   ${YELLOW}$total${NC}"
    echo -e "  Successful:       ${GREEN}$success${NC}"
    echo -e "  Failed:           ${RED}$failure${NC}"
    echo -e "  Success Rate:     ${success_rate}%"
    echo -e "  Requests/Second:  ${rps}"
    echo ""
    
    if [ $failure -eq 0 ]; then
        echo -e "${GREEN}✓ All requests successful!${NC}"
    elif [ $(echo "$success_rate > 99" | bc) -eq 1 ]; then
        echo -e "${YELLOW}⚠ Minor issues detected (>99% success)${NC}"
    else
        echo -e "${RED}✗ Significant failures detected${NC}"
    fi
}

main

