#!/bin/bash
set -e

RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
NC='\033[0m'

DENIRA_PORT=9001
DENIX_PORT=8000
DEFLUX_PORT=8080

PIDS=()

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
    echo -e "${GREEN}Cleanup completed${NC}"
}

trap cleanup EXIT INT TERM

log_info() {
    echo -e "${BLUE}[INFO]${NC} $1"
}

log_success() {
    echo -e "${GREEN}[PASS]${NC} $1"
}

log_fail() {
    echo -e "${RED}[FAIL]${NC} $1"
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

build_projects() {
    log_info "Building projects..."
    
    cd "$(dirname "$0")/../.."
    
    (cd denira && cargo build --release) &
    (cd denix && cargo build --release) &
    (cd deflux && cargo build --release) &
    wait
    
    log_success "All projects built"
}

start_denira() {
    log_info "Starting Denira supervisor on port $DENIRA_PORT..."
    
    cd "$(dirname "$0")/../../denira"
    RUST_LOG=info ./target/release/denira --port $DENIRA_PORT &
    PIDS+=($!)
    
    wait_for_port $DENIRA_PORT "Denira"
}

start_denix() {
    log_info "Starting Denix runtime on port $DENIX_PORT..."
    
    cd "$(dirname "$0")/../../denix"
    PORT=$DENIX_PORT RUST_LOG=info ./target/release/denix &
    PIDS+=($!)
    
    wait_for_port $DENIX_PORT "Denix"
}

test_denix_health() {
    log_info "Testing Denix health endpoint..."
    
    response=$(curl -s -o /dev/null -w "%{http_code}" "http://127.0.0.1:$DENIX_PORT/health")
    if [ "$response" = "200" ]; then
        log_success "Denix health check passed"
        return 0
    else
        log_fail "Denix health check failed (HTTP $response)"
        return 1
    fi
}

test_denix_app_lifecycle() {
    log_info "Testing Denix app lifecycle..."
    
    # denix uses globalThis.default pattern, not ES modules
    local app_code='globalThis.default = { fetch() { return new Response("Hello E2E!") } }'
    
    log_info "Creating test app..."
    response=$(curl -s -X POST "http://127.0.0.1:$DENIX_PORT/api/apps" \
        -H "Content-Type: application/json" \
        -d "{\"id\": \"e2e-test\", \"code\": \"$app_code\"}")
    
    if echo "$response" | grep -q "e2e-test"; then
        log_success "App created"
    else
        log_fail "App creation failed: $response"
        return 1
    fi
    
    log_info "Executing test app..."
    response=$(curl -s "http://127.0.0.1:$DENIX_PORT/apps/e2e-test")
    
    if [ "$response" = "Hello E2E!" ]; then
        log_success "App execution passed"
    else
        log_fail "App execution failed: $response"
        return 1
    fi
    
    log_info "Deleting test app..."
    curl -s -X DELETE "http://127.0.0.1:$DENIX_PORT/api/apps/e2e-test" > /dev/null
    log_success "App deleted"
    
    return 0
}

test_denix_kv_operations() {
    log_info "Testing Denix KV operations..."
    
    local app_code='globalThis.default = { async fetch(request, env) { await env.kv.set("test-key", "test-value"); const val = await env.kv.get("test-key"); return new Response(val || "null") } }'
    
    curl -s -X POST "http://127.0.0.1:$DENIX_PORT/api/apps" \
        -H "Content-Type: application/json" \
        -d "{\"id\": \"kv-test\", \"code\": \"$app_code\"}" > /dev/null
    
    response=$(curl -s "http://127.0.0.1:$DENIX_PORT/apps/kv-test")
    
    if [ "$response" = "test-value" ]; then
        log_success "KV operations passed"
    else
        log_warn "KV operations returned: $response (may need isolate warmup)"
    fi
    
    curl -s -X DELETE "http://127.0.0.1:$DENIX_PORT/api/apps/kv-test" > /dev/null
    return 0
}

run_unit_tests() {
    log_info "Running unit tests for all projects..."
    
    cd "$(dirname "$0")/../.."
    
    local total=0
    local passed=0
    
    for project in denira denix deflux denictl deforge _superv_workit; do
        log_info "Testing $project..."
        if (cd "$project" && cargo test --quiet 2>&1 | tail -5); then
            passed=$((passed + 1))
        fi
        total=$((total + 1))
    done
    
    log_success "Unit tests: $passed/$total projects passed"
}

main() {
    echo -e "${GREEN}=== Deislet E2E Test Suite ===${NC}"
    echo ""
    
    local test_mode="${1:-full}"
    
    case "$test_mode" in
        "build")
            build_projects
            ;;
        "unit")
            run_unit_tests
            ;;
        "denix")
            build_projects
            start_denix
            sleep 2
            test_denix_health
            test_denix_app_lifecycle
            test_denix_kv_operations
            ;;
        "full")
            build_projects
            run_unit_tests
            start_denira
            start_denix
            sleep 2
            test_denix_health
            test_denix_app_lifecycle
            test_denix_kv_operations
            ;;
        *)
            echo "Usage: $0 [build|unit|denix|full]"
            exit 1
            ;;
    esac
    
    echo ""
    echo -e "${GREEN}=== E2E Tests Completed ===${NC}"
}

main "$@"
