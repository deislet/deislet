#!/bin/bash
#
# Comprehensive test script for Deflux load balancer
# Tests the complete system: fake_superv + deflux + test backends
#

set -e

# Colors for output
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
NC='\033[0m' # No Color

# Configuration
DEFLUX_PORT=8080
DEFLUX_HTTPS_PORT=8443
SUPERVISOR_PORT=9001
BACKEND_PORTS="8001 8002 8003"
TEST_CONFIG="test_config.toml"

# PIDs tracking
PIDS=()

# Cleanup function
cleanup() {
    echo -e "\n${YELLOW}Cleaning up processes...${NC}"
    
    # Kill all background processes
    for pid in "${PIDS[@]}"; do
        if kill -0 "$pid" 2>/dev/null; then
            echo "Killing process $pid"
            kill "$pid" 2>/dev/null || true
        fi
    done
    
    # Wait a bit for graceful shutdown
    sleep 2
    
    # Force kill if still running
    for pid in "${PIDS[@]}"; do
        if kill -0 "$pid" 2>/dev/null; then
            echo "Force killing process $pid"
            kill -9 "$pid" 2>/dev/null || true
        fi
    done
    
    echo -e "${GREEN}Cleanup completed${NC}"
}

# Set up trap for cleanup
trap cleanup EXIT INT TERM

# Check dependencies
check_dependencies() {
    echo -e "${BLUE}Checking dependencies...${NC}"
    
    # Check if cargo is available
    if ! command -v cargo &> /dev/null; then
        echo -e "${RED}Error: cargo is not installed${NC}"
        exit 1
    fi
    
    # Check if python3 is available
    if ! command -v python3 &> /dev/null; then
        echo -e "${RED}Error: python3 is not installed${NC}"
        exit 1
    fi
    
    # Check if required Python packages are available
    python3 -c "import aiohttp, aiohttp_cors" 2>/dev/null || {
        echo -e "${YELLOW}Installing required Python packages...${NC}"
        pip3 install aiohttp aiohttp-cors
    }
    
    echo -e "${GREEN}Dependencies check passed${NC}"
}

# Build the project
build_project() {
    echo -e "${BLUE}Building Deflux and Mock Supervisor...${NC}"
    
    if ! cargo build --release --bin deflux --bin mock_superv; then
        echo -e "${RED}Error: Failed to build binaries${NC}"
        exit 1
    fi
    
    echo -e "${GREEN}Build completed${NC}"
}

# Start fake supervisor
start_fake_supervisor() {
    echo -e "${BLUE}Starting mock supervisor (gRPC) on port $SUPERVISOR_PORT...${NC}"
    
    # Assume we are in crates/deflux, target is at workspace root
    TARGET_DIR="../../target/release"
    
    if [[ ! -f "$TARGET_DIR/mock_superv" ]]; then
        echo -e "${RED}Error: mock_superv binary not found at $TARGET_DIR/mock_superv${NC}"
        exit 1
    fi

    "$TARGET_DIR/mock_superv" "127.0.0.1:$SUPERVISOR_PORT" &
    local pid=$!
    PIDS+=($pid)
    
    # Wait for supervisor to start
    echo "Waiting for supervisor to start..."
    sleep 3
    
    # Check if supervisor is running
    if ! kill -0 $pid 2>/dev/null; then
        echo -e "${RED}Error: Failed to start mock supervisor${NC}"
        exit 1
    fi
    
    echo -e "${GREEN}Mock supervisor started (PID: $pid)${NC}"
}

# Start test backends
start_test_backends() {
    echo -e "${BLUE}Starting test backends on ports: $BACKEND_PORTS...${NC}"
    
    python3 test_backend.py $BACKEND_PORTS --name "test-backend" &
    local pid=$!
    PIDS+=($pid)
    
    # Wait for backends to start
    echo "Waiting for test backends to start..."
    sleep 3
    
    # Check if backends are running
    if ! kill -0 $pid 2>/dev/null; then
        echo -e "${RED}Error: Failed to start test backends${NC}"
        exit 1
    fi
    
    echo -e "${GREEN}Test backends started (PID: $pid)${NC}"
    
    # Test backend connectivity
    for port in $BACKEND_PORTS; do
        if curl -s "http://127.0.0.1:$port/health" > /dev/null; then
            echo -e "${GREEN}Backend on port $port is healthy${NC}"
        else
            echo -e "${YELLOW}Warning: Backend on port $port is not responding${NC}"
        fi
    done
}

# Start Deflux load balancer
start_deflux() {
    echo -e "${BLUE}Starting Deflux load balancer...${NC}"
    
    # Use the test configuration
    RUST_LOG=info ./target/release/deflux -c "$TEST_CONFIG" &
    local pid=$!
    PIDS+=($pid)
    
    # Wait for Deflux to start
    echo "Waiting for Deflux to start..."
    sleep 5
    
    # Check if Deflux is running
    if ! kill -0 $pid 2>/dev/null; then
        echo -e "${RED}Error: Failed to start Deflux${NC}"
        exit 1
    fi
    
    echo -e "${GREEN}Deflux started (PID: $pid)${NC}"
}

# Test basic connectivity
test_connectivity() {
    echo -e "${BLUE}Testing basic connectivity...${NC}"
    
    # Test HTTP port
    if curl -s -o /dev/null -w "%{http_code}" "http://127.0.0.1:$DEFLUX_PORT/" | grep -q "200"; then
        echo -e "${GREEN}HTTP port $DEFLUX_PORT is responding${NC}"
    else
        echo -e "${YELLOW}Warning: HTTP port $DEFLUX_PORT is not responding as expected${NC}"
    fi
    
    # Test load balancing by making multiple requests
    echo -e "${BLUE}Testing load balancing...${NC}"
    
    for i in {1..6}; do
        echo -n "Request $i: "
        response=$(curl -s "http://127.0.0.1:$DEFLUX_PORT/" | jq -r '.backend' 2>/dev/null || echo "error")
        echo -e "${GREEN}Backend: $response${NC}"
        sleep 1
    done
}

# Test health checks
test_health_checks() {
    echo -e "${BLUE}Testing health checks...${NC}"
    
    # Make one backend unhealthy
    echo "Making backend-2 unhealthy..."
    curl -s -X POST "http://127.0.0.1:8002/toggle-health" > /dev/null
    
    sleep 2
    
    # Test that requests are still handled
    echo "Testing with unhealthy backend..."
    for i in {1..4}; do
        echo -n "Request $i: "
        response=$(curl -s "http://127.0.0.1:$DEFLUX_PORT/" | jq -r '.backend' 2>/dev/null || echo "error")
        echo -e "${GREEN}Backend: $response${NC}"
        sleep 1
    done
    
    # Make backend healthy again
    echo "Making backend-2 healthy again..."
    curl -s -X POST "http://127.0.0.1:8002/toggle-health" > /dev/null
    
    sleep 2
    echo -e "${GREEN}Health check testing completed${NC}"
}

# Test error handling
test_error_handling() {
    echo -e "${BLUE}Testing error handling...${NC}"
    
    # Set error rate on one backend
    echo "Setting 50% error rate on backend-3..."
    curl -s -X POST "http://127.0.0.1:8003/set-error-rate/0.5" > /dev/null
    
    # Test error handling
    echo "Testing error handling..."
    success_count=0
    error_count=0
    
    for i in {1..10}; do
        http_code=$(curl -s -o /dev/null -w "%{http_code}" "http://127.0.0.1:$DEFLUX_PORT/")
        if [[ "$http_code" == "200" ]]; then
            ((success_count++))
        else
            ((error_count++))
        fi
    done
    
    echo -e "${GREEN}Success: $success_count, Errors: $error_count${NC}"
    
    # Reset error rate
    curl -s -X POST "http://127.0.0.1:8003/set-error-rate/0.0" > /dev/null
    
    echo -e "${GREEN}Error handling testing completed${NC}"
}

# Test metrics and monitoring
test_metrics() {
    echo -e "${BLUE}Testing metrics collection...${NC}"
    
    # Get metrics from backends
    for port in $BACKEND_PORTS; do
        echo "Metrics from backend on port $port:"
        curl -s "http://127.0.0.1:$port/metrics" | jq '.' 2>/dev/null || echo "No valid JSON response"
        echo
    done
    
    echo -e "${GREEN}Metrics testing completed${NC}"
}

# Performance test
performance_test() {
    echo -e "${BLUE}Running basic performance test...${NC}"
    
    # Check if ab (Apache Bench) is available
    if command -v ab &> /dev/null; then
        echo "Running Apache Bench test (100 requests, concurrency 10)..."
        ab -n 100 -c 10 "http://127.0.0.1:$DEFLUX_PORT/" || echo "Apache Bench test failed"
    elif command -v curl &> /dev/null; then
        echo "Running simple curl-based performance test..."
        start_time=$(date +%s)
        
        for i in {1..50}; do
            curl -s "http://127.0.0.1:$DEFLUX_PORT/" > /dev/null &
            if (( i % 10 == 0 )); then
                wait
            fi
        done
        wait
        
        end_time=$(date +%s)
        duration=$((end_time - start_time))
        echo -e "${GREEN}Completed 50 requests in ${duration}s${NC}"
    else
        echo -e "${YELLOW}No performance testing tools available${NC}"
    fi
}

# Show logs
show_logs() {
    echo -e "${BLUE}Recent logs:${NC}"
    
    if [[ -f "/tmp/deflux_access.log" ]]; then
        echo -e "${BLUE}Access logs:${NC}"
        tail -10 /tmp/deflux_access.log 2>/dev/null || echo "No access logs found"
    fi
    
    if [[ -f "/tmp/deflux_error.log" ]]; then
        echo -e "${BLUE}Error logs:${NC}"
        tail -10 /tmp/deflux_error.log 2>/dev/null || echo "No error logs found"
    fi
}

# Main test execution
main() {
    echo -e "${GREEN}=== Deflux Load Balancer Comprehensive Test Suite ===${NC}"
    echo -e "${BLUE}Configuration:${NC}"
    echo "  - Deflux HTTP port: $DEFLUX_PORT"
    echo "  - Deflux HTTPS port: $DEFLUX_HTTPS_PORT"
    echo "  - Supervisor port: $SUPERVISOR_PORT"
    echo "  - Backend ports: $BACKEND_PORTS"
    echo "  - Test config: $TEST_CONFIG"
    echo
    
    check_dependencies
    build_project
    
    echo -e "\n${GREEN}=== Starting Services ===${NC}"
    start_fake_supervisor
    start_test_backends
    start_deflux
    
    echo -e "\n${GREEN}=== Running Tests ===${NC}"
    sleep 3  # Give everything time to fully start
    
    test_connectivity
    test_health_checks
    test_error_handling
    test_metrics
    performance_test
    
    echo -e "\n${GREEN}=== Test Results ===${NC}"
    show_logs
    
    echo -e "\n${GREEN}=== Test Suite Completed ===${NC}"
    echo -e "${BLUE}All services are still running. Press Ctrl+C to stop.${NC}"
    
    # Keep running until user interrupts
    while true; do
        sleep 1
    done
}

# Handle script arguments
case "${1:-}" in
    "deps")
        check_dependencies
        ;;
    "build")
        build_project
        ;;
    "supervisor")
        start_fake_supervisor
        wait
        ;;
    "backends")
        start_test_backends
        wait
        ;;
    "deflux")
        start_deflux
        wait
        ;;
    "test")
        main
        ;;
    *)
        echo "Usage: $0 [deps|build|supervisor|backends|deflux|test]"
        echo "  deps      - Check dependencies"
        echo "  build     - Build the project"
        echo "  supervisor - Start fake supervisor only"
        echo "  backends  - Start test backends only"
        echo "  deflux    - Start deflux only"
        echo "  test      - Run full test suite (default)"
        echo
        main
        ;;
esac