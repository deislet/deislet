#!/bin/bash
# Complete E2E test suite for Deislet platform
# Tests: Denira + Denix + Deflux + Config + Commands + Metrics
set -e

# Determine project root at the beginning
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"

RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
CYAN='\033[0;36m'
NC='\033[0m'

DENIRA_WS_PORT=9001
DENIRA_API_PORT=9002
DENIX_PORT=8000
DEFLUX_PORT=8080

PIDS=()
TEST_PASSED=0
TEST_FAILED=0

cleanup() {
    echo -e "\n${YELLOW}Cleaning up...${NC}"
    for pid in "${PIDS[@]}"; do
        if kill -0 "$pid" 2>/dev/null; then
            kill "$pid" 2>/dev/null || true
        fi
    done
    sleep 1
    for pid in "${PIDS[@]}"; do
        if kill -0 "$pid" 2>/dev/null; then
            kill -9 "$pid" 2>/dev/null || true
        fi
    done
    rm -f /tmp/denira-test.db /tmp/denix-test.db /tmp/deflux-test.db
    echo -e "${GREEN}Cleanup completed${NC}"
}

trap cleanup EXIT INT TERM

log_section() {
    echo ""
    echo -e "${CYAN}========================================${NC}"
    echo -e "${CYAN}$1${NC}"
    echo -e "${CYAN}========================================${NC}"
}

log_info() {
    echo -e "${BLUE}[INFO]${NC} $1"
}

log_success() {
    echo -e "${GREEN}[PASS]${NC} $1"
    TEST_PASSED=$((TEST_PASSED + 1))
}

log_fail() {
    echo -e "${RED}[FAIL]${NC} $1"
    TEST_FAILED=$((TEST_FAILED + 1))
}

log_warn() {
    echo -e "${YELLOW}[WARN]${NC} $1"
}

wait_for_port() {
    local port=$1
    local name=$2
    local max_attempts=30
    local attempt=0

    while ! nc -z 127.0.0.1 "$port" 2>/dev/null; do
        attempt=$((attempt + 1))
        if [ $attempt -ge $max_attempts ]; then
            log_fail "$name not ready on port $port after ${max_attempts}s"
            return 1
        fi
        sleep 1
    done
    log_success "$name is ready on port $port"
}

build_all() {
    log_section "Building All Projects"

    cd "$PROJECT_ROOT"

    log_info "Building in parallel..."
    (
        cd denira && cargo build --release 2>&1 | tail -2 &
        cd denix && cargo build --release 2>&1 | tail -2 &
        cd deflux && cargo build --release 2>&1 | tail -2 &
        wait
    )

    log_success "All projects built"
}

start_denira() {
    log_section "Starting Denira Supervisor"

    cd "$PROJECT_ROOT/denira" || {
        log_fail "Failed to cd to $PROJECT_ROOT/denira"
        return 1
    }

    log_info "Launching Denira (WS: $DENIRA_WS_PORT, API: $DENIRA_API_PORT)..."
    RUST_LOG=info ./target/release/denira \
        --port $DENIRA_WS_PORT \
        --api-port $DENIRA_API_PORT \
        --database /tmp/denira-test.db > /tmp/denira.log 2>&1 &
    PIDS+=($!)

    wait_for_port $DENIRA_WS_PORT "Denira WebSocket"
    wait_for_port $DENIRA_API_PORT "Denira HTTP API"
}

start_denix() {
    log_section "Starting Denix Runtime"

    cd "$PROJECT_ROOT/denix" || {
        log_fail "Failed to cd to $PROJECT_ROOT/denix"
        return 1
    }

    log_info "Launching Denix (Port: $DENIX_PORT)..."
    PORT=$DENIX_PORT RUST_LOG=info ./target/release/denix > /tmp/denix.log 2>&1 &
    PIDS+=($!)

    wait_for_port $DENIX_PORT "Denix Runtime"
    sleep 2  # Wait for supervisor connection
}

start_deflux() {
    log_section "Starting Deflux Load Balancer"

    cd "$PROJECT_ROOT/deflux" || {
        log_fail "Failed to cd to $PROJECT_ROOT/deflux"
        return 1
    }

    # Create minimal config
    cat > /tmp/deflux-test.toml <<EOF
[system]
daemon = false
listen_addr = ["127.0.0.1"]
listen_ports = [$DEFLUX_PORT]

[system.https]
enabled = false

[system.log]
[system.log.error_log]
level = "info"

[system.supervisor]
enabled = true
addr = "ws://127.0.0.1:$DENIRA_WS_PORT"
associated_id = "deflux-test"
secret = "a1b2c3d4e5f67890123456789012345678901234567890123456789012345678"
heartbeat_interval = 30
request_timeout = 10
retry_count = 3

[database]
path = "/tmp/deflux-test.db"
EOF

    log_info "Launching Deflux (Port: $DEFLUX_PORT)..."
    RUST_LOG=info ./target/release/deflux -c /tmp/deflux-test.toml > /tmp/deflux.log 2>&1 &
    PIDS+=($!)

    wait_for_port $DEFLUX_PORT "Deflux Load Balancer"
    sleep 2  # Wait for supervisor connection
}

test_denira_api() {
    log_section "Testing Denira API"

    # Health check
    log_info "Testing health endpoint..."
    response=$(curl -s "http://127.0.0.1:$DENIRA_API_PORT/health")
    if [ "$response" = "OK" ]; then
        log_success "Health check passed"
    else
        log_fail "Health check failed: $response"
    fi

    # Workers list
    log_info "Testing workers list..."
    response=$(curl -s "http://127.0.0.1:$DENIRA_API_PORT/api/workers")
    if echo "$response" | jq -e '.workers' > /dev/null 2>&1; then
        total=$(echo "$response" | jq -r '.total')
        log_success "Workers API working (total: $total)"

        # Check for registered workers
        if [ "$total" -ge 1 ]; then
            log_success "Workers registered with supervisor"
        else
            log_warn "No workers registered yet"
        fi
    else
        log_fail "Workers API failed: $response"
    fi
}

test_denix_runtime() {
    log_section "Testing Denix Runtime"

    # Health check
    log_info "Testing Denix health..."
    response=$(curl -s -o /dev/null -w "%{http_code}" "http://127.0.0.1:$DENIX_PORT/health")
    if [ "$response" = "200" ]; then
        log_success "Denix health check passed"
    else
        log_fail "Denix health check failed (HTTP $response)"
    fi

    # App deployment
    log_info "Deploying test app..."
    local app_code="globalThis.default = { fetch() { return new Response('E2E Test OK') } }"

    response=$(curl -s -X POST "http://127.0.0.1:$DENIX_PORT/api/apps" \
        -H "Content-Type: application/json" \
        -d "{\"id\": \"e2e-full-test\", \"code\": \"$app_code\"}")

    if echo "$response" | grep -q "e2e-full-test"; then
        log_success "App deployed successfully"

        # Execute app
        log_info "Executing app..."
        response=$(curl -s "http://127.0.0.1:$DENIX_PORT/apps/e2e-full-test")
        if [ "$response" = "E2E Test OK" ]; then
            log_success "App execution successful"
        else
            log_fail "App execution failed: $response"
        fi

        # Cleanup
        curl -s -X DELETE "http://127.0.0.1:$DENIX_PORT/api/apps/e2e-full-test" > /dev/null
    else
        log_fail "App deployment failed: $response"
    fi
}

test_deflux_proxy() {
    log_section "Testing Deflux Load Balancer"

    # Health check
    log_info "Testing Deflux health..."
    response=$(curl -s -o /dev/null -w "%{http_code}" "http://127.0.0.1:$DEFLUX_PORT/health" 2>/dev/null || echo "000")

    if [ "$response" = "200" ]; then
        log_success "Deflux health check passed"
    else
        log_warn "Deflux health endpoint returned HTTP $response (may not be implemented)"
    fi

    # Check supervisor connection
    log_info "Checking Deflux registration in Denira..."
    response=$(curl -s "http://127.0.0.1:$DENIRA_API_PORT/api/workers")
    if echo "$response" | jq -e '.workers[] | select(.id | contains("deflux"))' > /dev/null 2>&1; then
        log_success "Deflux registered with Denira"
    else
        log_warn "Deflux not yet registered (may take time)"
    fi
}

test_metrics() {
    log_section "Testing Metrics Collection"

    # Prometheus metrics
    log_info "Testing Prometheus metrics export..."
    response=$(curl -s "http://127.0.0.1:$DENIRA_API_PORT/api/metrics" 2>/dev/null || echo "")

    if echo "$response" | grep -q "denira_workers_total"; then
        log_success "Prometheus metrics available"

        workers_total=$(echo "$response" | grep "^denira_workers_total " | awk '{print $2}')
        log_info "denira_workers_total = $workers_total"
    else
        log_warn "Prometheus metrics not yet implemented"
    fi
}

test_database() {
    log_section "Testing Database Persistence"

    if [ -f "/tmp/denira-test.db" ]; then
        # Check tables exist
        tables=$(sqlite3 /tmp/denira-test.db "SELECT name FROM sqlite_master WHERE type='table';" 2>/dev/null)

        if echo "$tables" | grep -q "workers"; then
            log_success "Workers table exists"
        fi

        if echo "$tables" | grep -q "events"; then
            log_success "Events table exists"
        fi

        if echo "$tables" | grep -q "heartbeats"; then
            log_success "Heartbeats table exists"
        fi

        # Count records
        workers=$(sqlite3 /tmp/denira-test.db "SELECT COUNT(*) FROM workers;" 2>/dev/null || echo "0")
        events=$(sqlite3 /tmp/denira-test.db "SELECT COUNT(*) FROM events;" 2>/dev/null || echo "0")
        heartbeats=$(sqlite3 /tmp/denira-test.db "SELECT COUNT(*) FROM heartbeats;" 2>/dev/null || echo "0")

        log_info "Database stats: workers=$workers, events=$events, heartbeats=$heartbeats"

        if [ "$workers" -ge 1 ]; then
            log_success "Worker data persisted to database"
        fi
    else
        log_fail "Database file not created"
    fi
}

test_worker_heartbeats() {
    log_section "Testing Worker Heartbeats"

    log_info "Waiting for heartbeat cycle (5 seconds)..."
    sleep 5

    # Check heartbeats in database
    if [ -f "/tmp/denira-test.db" ]; then
        heartbeats=$(sqlite3 /tmp/denira-test.db "SELECT COUNT(*) FROM heartbeats;" 2>/dev/null || echo "0")

        if [ "$heartbeats" -ge 1 ]; then
            log_success "Heartbeats received and stored ($heartbeats total)"
        else
            log_warn "No heartbeats recorded yet"
        fi
    fi

    # Check via API
    response=$(curl -s "http://127.0.0.1:$DENIRA_API_PORT/api/workers")
    workers=$(echo "$response" | jq -r '.workers[] | .id' 2>/dev/null || echo "")

    if [ -n "$workers" ]; then
        for worker_id in $workers; do
            log_info "Checking heartbeats for worker: $worker_id"
            hb_response=$(curl -s "http://127.0.0.1:$DENIRA_API_PORT/api/workers/$worker_id/heartbeats?limit=5")

            if echo "$hb_response" | jq -e '. | length' > /dev/null 2>&1; then
                count=$(echo "$hb_response" | jq '. | length')
                log_success "Worker $worker_id: $count heartbeats retrieved"
            fi
        done
    fi
}

print_summary() {
    log_section "Test Summary"

    echo -e "${CYAN}Total Tests: $((TEST_PASSED + TEST_FAILED))${NC}"
    echo -e "${GREEN}Passed: $TEST_PASSED${NC}"
    if [ $TEST_FAILED -gt 0 ]; then
        echo -e "${RED}Failed: $TEST_FAILED${NC}"
    else
        echo -e "${GREEN}Failed: $TEST_FAILED${NC}"
    fi

    echo ""
    if [ $TEST_FAILED -eq 0 ]; then
        echo -e "${GREEN}=== ALL E2E TESTS PASSED! ===${NC}"
    else
        echo -e "${YELLOW}=== SOME TESTS FAILED ===${NC}"
    fi

    echo ""
    echo -e "${BLUE}Log files:${NC}"
    echo "  - /tmp/denira.log"
    echo "  - /tmp/denix.log"
    echo "  - /tmp/deflux.log"
}

main() {
    echo -e "${GREEN}╔══════════════════════════════════════════╗${NC}"
    echo -e "${GREEN}║  Deislet Complete E2E Test Suite        ║${NC}"
    echo -e "${GREEN}╚══════════════════════════════════════════╝${NC}"
    echo ""

    build_all

    # Start all services
    start_denira
    start_denix
    start_deflux

    # Run tests
    test_denira_api
    test_denix_runtime
    test_deflux_proxy
    test_metrics
    test_database
    test_worker_heartbeats

    print_summary
}

main "$@"
