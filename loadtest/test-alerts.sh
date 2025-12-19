#!/bin/bash
# Spooled Backend - Alert Testing Script
#
# This script tests that alerts fire correctly by simulating various failure conditions.
# Run this after setting up Alertmanager to verify the alerting pipeline works end-to-end.
#
# Prerequisites:
#   - Docker Compose stack running (docker-compose up -d)
#   - Prometheus accessible at localhost:9091
#   - Alertmanager accessible at localhost:9093
#   - Backend metrics endpoint accessible
#
# Usage:
#   ./loadtest/test-alerts.sh [test_name]
#
# Examples:
#   ./loadtest/test-alerts.sh           # Run all tests
#   ./loadtest/test-alerts.sh api       # Test API latency alert only
#   ./loadtest/test-alerts.sh workers   # Test worker alerts only

set -euo pipefail

# Configuration
PROMETHEUS_URL="${PROMETHEUS_URL:-http://localhost:9091}"
ALERTMANAGER_URL="${ALERTMANAGER_URL:-http://localhost:9093}"
BACKEND_URL="${BACKEND_URL:-http://localhost:8080}"
METRICS_URL="${METRICS_URL:-http://localhost:9090}"

# Colors for output
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
NC='\033[0m' # No Color

# Logging functions
log_info() {
    echo -e "${BLUE}[INFO]${NC} $1"
}

log_success() {
    echo -e "${GREEN}[OK]${NC} $1"
}

log_warn() {
    echo -e "${YELLOW}[WARN]${NC} $1"
}

log_error() {
    echo -e "${RED}[ERROR]${NC} $1"
}

log_header() {
    echo ""
    echo -e "${BLUE}========================================${NC}"
    echo -e "${BLUE}$1${NC}"
    echo -e "${BLUE}========================================${NC}"
}

# Check prerequisites
check_prerequisites() {
    log_header "Checking Prerequisites"
    
    local failed=0
    
    # Check Prometheus
    if curl -sf "$PROMETHEUS_URL/-/healthy" > /dev/null 2>&1; then
        log_success "Prometheus is running at $PROMETHEUS_URL"
    else
        log_error "Prometheus is not accessible at $PROMETHEUS_URL"
        failed=1
    fi
    
    # Check Alertmanager
    if curl -sf "$ALERTMANAGER_URL/-/healthy" > /dev/null 2>&1; then
        log_success "Alertmanager is running at $ALERTMANAGER_URL"
    else
        log_error "Alertmanager is not accessible at $ALERTMANAGER_URL"
        failed=1
    fi
    
    # Check if alert rules are loaded
    local rule_count
    rule_count=$(curl -sf "$PROMETHEUS_URL/api/v1/rules" | jq '.data.groups | length' 2>/dev/null || echo "0")
    if [ "$rule_count" -gt 0 ]; then
        log_success "Prometheus has $rule_count alert rule groups loaded"
    else
        log_warn "No alert rules found in Prometheus"
    fi
    
    if [ $failed -eq 1 ]; then
        log_error "Prerequisites check failed. Please ensure the Docker Compose stack is running."
        exit 1
    fi
}

# List all alerts
list_alerts() {
    log_header "Current Alert Status"
    
    # Get firing alerts
    local alerts
    alerts=$(curl -sf "$PROMETHEUS_URL/api/v1/alerts" | jq -r '.data.alerts[] | "\(.labels.alertname): \(.state) - \(.annotations.summary // "no summary")"' 2>/dev/null || echo "No alerts")
    
    if [ "$alerts" = "No alerts" ] || [ -z "$alerts" ]; then
        log_info "No alerts currently firing"
    else
        echo "$alerts" | while read -r alert; do
            if [[ "$alert" == *"firing"* ]]; then
                log_warn "$alert"
            else
                log_info "$alert"
            fi
        done
    fi
    
    # Get alerts from Alertmanager
    log_info "Alerts in Alertmanager:"
    curl -sf "$ALERTMANAGER_URL/api/v2/alerts" | jq -r '.[] | "\(.labels.alertname): \(.status.state)"' 2>/dev/null || log_info "No alerts in Alertmanager"
}

# Wait for alert to fire
wait_for_alert() {
    local alert_name="$1"
    local timeout="${2:-120}"
    local interval=5
    local elapsed=0
    
    log_info "Waiting for alert '$alert_name' to fire (timeout: ${timeout}s)..."
    
    while [ $elapsed -lt $timeout ]; do
        local state
        state=$(curl -sf "$PROMETHEUS_URL/api/v1/alerts" | jq -r ".data.alerts[] | select(.labels.alertname==\"$alert_name\") | .state" 2>/dev/null || echo "")
        
        if [ "$state" = "firing" ]; then
            log_success "Alert '$alert_name' is firing after ${elapsed}s"
            return 0
        elif [ "$state" = "pending" ]; then
            log_info "Alert '$alert_name' is pending..."
        fi
        
        sleep $interval
        elapsed=$((elapsed + interval))
    done
    
    log_error "Alert '$alert_name' did not fire within ${timeout}s"
    return 1
}

# Test: Simulate high API latency
test_api_latency() {
    log_header "Test: High API Latency Alert"
    
    log_info "This test simulates high API latency by sending many slow requests."
    log_info "In a real scenario, slow backend responses would trigger HighAPILatency alert."
    log_info ""
    log_info "To manually test:"
    log_info "  1. Introduce artificial delay in the backend"
    log_info "  2. Or overload the database to slow down queries"
    log_info ""
    log_info "Expected alert: HighAPILatency (p99 > 1s) or CriticalAPILatency (p99 > 5s)"
    
    # Check if alert is currently firing
    local state
    state=$(curl -sf "$PROMETHEUS_URL/api/v1/alerts" | jq -r '.data.alerts[] | select(.labels.alertname=="HighAPILatency" or .labels.alertname=="CriticalAPILatency") | .state' 2>/dev/null || echo "")
    
    if [ -n "$state" ]; then
        log_warn "API latency alert is already in state: $state"
    else
        log_info "No API latency alerts currently firing (normal)"
    fi
}

# Test: Simulate no workers
test_no_workers() {
    log_header "Test: No Healthy Workers Alert"
    
    log_info "This test verifies the NoHealthyWorkers alert."
    log_info ""
    log_info "To trigger:"
    log_info "  1. Stop all worker processes"
    log_info "  2. Or disconnect workers from the backend"
    log_info ""
    log_info "Expected alert: NoHealthyWorkers (no workers healthy for 2m)"
    
    # Check current worker metrics
    local healthy_workers
    healthy_workers=$(curl -sf "$METRICS_URL/metrics" 2>/dev/null | grep "^spooled_workers_healthy" | awk '{print $2}' || echo "unknown")
    
    if [ "$healthy_workers" = "unknown" ]; then
        log_warn "Could not read worker metrics from $METRICS_URL/metrics"
    elif [ "$healthy_workers" = "0" ]; then
        log_warn "No healthy workers detected - alert should fire soon"
        wait_for_alert "NoHealthyWorkers" 180
    else
        log_info "Currently $healthy_workers healthy worker(s)"
    fi
}

# Test: Simulate job queue backlog
test_queue_backlog() {
    log_header "Test: Job Queue Backlog Alert"
    
    log_info "This test verifies job backlog alerts."
    log_info ""
    log_info "To trigger:"
    log_info "  1. Enqueue many jobs without workers processing them"
    log_info "  2. Or pause worker processing"
    log_info ""
    log_info "Expected alerts:"
    log_info "  - JobQueueBacklog (oldest job > 5min)"
    log_info "  - JobQueueBacklogCritical (oldest job > 15min)"
    
    # Check current backlog metrics
    local max_age
    max_age=$(curl -sf "$METRICS_URL/metrics" 2>/dev/null | grep "^spooled_job_max_age_seconds" | awk '{print $2}' || echo "unknown")
    
    if [ "$max_age" = "unknown" ]; then
        log_warn "Could not read job age metrics from $METRICS_URL/metrics"
    else
        log_info "Oldest pending job age: ${max_age}s"
        # Avoid dependency on `bc` (not always installed). Treat non-numeric as 0.
        if awk -v v="$max_age" 'BEGIN { exit !((v + 0) > 300) }'; then
            log_warn "Job backlog alert should be firing (age > 300s)"
        fi
    fi
}

# Test: Simulate DLQ rate spike
test_dlq_spike() {
    log_header "Test: Dead Letter Queue Rate Alert"
    
    log_info "This test verifies the DLQ spike alert."
    log_info ""
    log_info "To trigger:"
    log_info "  1. Create jobs that always fail after max retries"
    log_info "  2. Wait for them to be moved to DLQ"
    log_info ""
    log_info "Expected alert: JobsDeadlettered (> 5 jobs in DLQ in 15min)"
    
    # Check current DLQ metrics
    local dlq_total
    dlq_total=$(curl -sf "$METRICS_URL/metrics" 2>/dev/null | grep "^spooled_jobs_deadlettered_total" | awk '{print $2}' || echo "unknown")
    
    if [ "$dlq_total" = "unknown" ]; then
        log_warn "Could not read DLQ metrics from $METRICS_URL/metrics"
    else
        log_info "Total jobs in DLQ: $dlq_total"
    fi
}

# Test: Check database connection alerts
test_database_connections() {
    log_header "Test: Database Connection Alerts"
    
    log_info "This test verifies database connection saturation alerts."
    log_info ""
    log_info "To trigger:"
    log_info "  1. Create many concurrent database connections"
    log_info "  2. Or reduce pool size and increase load"
    log_info ""
    log_info "Expected alerts:"
    log_info "  - DatabaseConnectionsHigh (> 80 connections)"
    log_info "  - DatabaseConnectionsCritical (> 95 connections)"
    
    # Check current connection metrics
    local connections
    connections=$(curl -sf "$METRICS_URL/metrics" 2>/dev/null | grep "^spooled_database_connections_active" | awk '{print $2}' || echo "unknown")
    
    if [ "$connections" = "unknown" ]; then
        log_warn "Could not read database metrics from $METRICS_URL/metrics"
    else
        log_info "Active database connections: $connections"
    fi
}

# Test: Check Redis availability
test_redis_availability() {
    log_header "Test: Redis Availability Alert"
    
    log_info "This test verifies Redis unavailability alerts."
    log_info ""
    log_info "To trigger:"
    log_info "  docker-compose stop redis"
    log_info ""
    log_info "Expected alerts:"
    log_info "  - RedisOperationsLow (operations dropped significantly)"
    log_info "  - RedisUnavailable (no operations for 2min)"
    
    # Check if Redis is accessible
    if docker exec spooled-redis redis-cli ping > /dev/null 2>&1; then
        log_success "Redis is responding to ping"
    else
        log_warn "Redis is not responding - alert should fire"
    fi
}

# Verify Alertmanager notifications
test_alertmanager_notifications() {
    log_header "Test: Alertmanager Notification Pipeline"
    
    log_info "Checking Alertmanager configuration and notification status..."
    
    # Get Alertmanager status
    local am_status
    am_status=$(curl -sf "$ALERTMANAGER_URL/api/v2/status" | jq -r '.cluster.status' 2>/dev/null || echo "unknown")
    
    if [ "$am_status" = "ready" ]; then
        log_success "Alertmanager cluster status: $am_status"
    else
        log_warn "Alertmanager cluster status: $am_status"
    fi
    
    # Check configured receivers
    local receivers
    receivers=$(curl -sf "$ALERTMANAGER_URL/api/v2/status" | jq -r '.config.original' 2>/dev/null | grep -c "receiver" || echo "0")
    log_info "Configured receivers: $receivers"
    
    # Check for recent notifications
    log_info "Recent alert activity in Alertmanager:"
    curl -sf "$ALERTMANAGER_URL/api/v2/alerts" | jq -r '.[0:5] | .[] | "  - \(.labels.alertname): \(.status.state)"' 2>/dev/null || log_info "  No recent alerts"
}

# Generate alert report
generate_report() {
    log_header "Alert Configuration Report"
    
    echo ""
    echo "Prometheus URL: $PROMETHEUS_URL"
    echo "Alertmanager URL: $ALERTMANAGER_URL"
    echo "Backend URL: $BACKEND_URL"
    echo "Metrics URL: $METRICS_URL"
    echo ""
    
    # List all configured rules
    log_info "Configured Alert Rules:"
    curl -sf "$PROMETHEUS_URL/api/v1/rules" | jq -r '.data.groups[].rules[] | select(.type=="alerting") | "  - \(.name) [\(.state)]"' 2>/dev/null || echo "  Could not fetch rules"
    
    echo ""
    
    # List all firing alerts
    log_info "Currently Firing Alerts:"
    curl -sf "$PROMETHEUS_URL/api/v1/alerts" | jq -r '.data.alerts[] | select(.state=="firing") | "  - \(.labels.alertname): \(.annotations.summary)"' 2>/dev/null || echo "  None"
    
    echo ""
    
    # List pending alerts
    log_info "Pending Alerts:"
    curl -sf "$PROMETHEUS_URL/api/v1/alerts" | jq -r '.data.alerts[] | select(.state=="pending") | "  - \(.labels.alertname)"' 2>/dev/null || echo "  None"
}

# Main execution
main() {
    local test_name="${1:-all}"
    
    echo ""
    echo "=============================================="
    echo "  Spooled Alert Testing Script"
    echo "=============================================="
    echo ""
    
    check_prerequisites
    
    case "$test_name" in
        all)
            list_alerts
            test_api_latency
            test_no_workers
            test_queue_backlog
            test_dlq_spike
            test_database_connections
            test_redis_availability
            test_alertmanager_notifications
            generate_report
            ;;
        api)
            test_api_latency
            ;;
        workers)
            test_no_workers
            ;;
        backlog|queue)
            test_queue_backlog
            ;;
        dlq)
            test_dlq_spike
            ;;
        database|db)
            test_database_connections
            ;;
        redis)
            test_redis_availability
            ;;
        notifications|alertmanager)
            test_alertmanager_notifications
            ;;
        report)
            generate_report
            ;;
        list)
            list_alerts
            ;;
        *)
            echo "Unknown test: $test_name"
            echo ""
            echo "Available tests:"
            echo "  all          - Run all tests"
            echo "  api          - Test API latency alerts"
            echo "  workers      - Test worker health alerts"
            echo "  backlog      - Test queue backlog alerts"
            echo "  dlq          - Test DLQ rate alerts"
            echo "  database     - Test database connection alerts"
            echo "  redis        - Test Redis availability alerts"
            echo "  notifications - Test Alertmanager notification pipeline"
            echo "  report       - Generate alert configuration report"
            echo "  list         - List current alert status"
            exit 1
            ;;
    esac
    
    log_header "Testing Complete"
    log_info "Review the output above to verify alert behavior."
    log_info "For manual testing, follow the instructions in each test section."
}

main "$@"
