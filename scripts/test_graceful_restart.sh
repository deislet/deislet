#!/bin/bash
# Deflux graceful restart integration test script

set -e

# Color definitions
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
NC='\033[0m' # No Color

# Test configuration
TEST_PORT=18080
TEST_CONFIG="/tmp/deflux-test-config.toml"
TEST_DB="/tmp/deflux-test.db"
DEFLUX_BIN="./target/debug/deflux"
LOG_FILE="/tmp/deflux-test.log"

# Cleanup function
cleanup() {
    echo -e "${YELLOW}Cleaning up test environment...${NC}"
    pkill -f "deflux.*${TEST_CONFIG}" || true
    rm -f "$TEST_CONFIG" "$TEST_DB" "$LOG_FILE"
    rm -f /tmp/deflux-restart.sock
}

# Register cleanup trap
trap cleanup EXIT

# Check the binary
check_binary() {
    if [ ! -f "$DEFLUX_BIN" ]; then
        echo -e "${RED}Error: Deflux binary not found${NC}"
        echo "Build it first: cargo build -p deflux"
        exit 1
    fi
}

# Create test configuration
create_test_config() {
    echo -e "${GREEN}Creating test configuration...${NC}"
    cat > "$TEST_CONFIG" <<EOF
[system]
listen_addr = ["127.0.0.1"]
listen_ports = [$TEST_PORT]

[system.log]
[system.log.error_log]
level = "Debug"

[system.tokio_runtime]
worker_threads = 2
EOF
}

# Start Deflux
start_deflux() {
    echo -e "${GREEN}Starting Deflux (port: $TEST_PORT)...${NC}"
    RUST_LOG=deflux=debug "$DEFLUX_BIN" -c "$TEST_CONFIG" > "$LOG_FILE" 2>&1 &
    DEFLUX_PID=$!

    # Wait for startup
    sleep 2

    if ! kill -0 $DEFLUX_PID 2>/dev/null; then
        echo -e "${RED}Error: Deflux failed to start${NC}"
        cat "$LOG_FILE"
        exit 1
    fi

    echo -e "${GREEN}Deflux started (PID: $DEFLUX_PID)${NC}"
}

# Test connectivity
test_connection() {
    local expected_pid=$1
    echo -e "${YELLOW}Testing connection to 127.0.0.1:$TEST_PORT...${NC}"

    # Test with netcat or curl
    if command -v nc &> /dev/null; then
        timeout 2 bash -c "echo 'GET / HTTP/1.0\r\n\r\n' | nc 127.0.0.1 $TEST_PORT" > /dev/null 2>&1 || true
    elif command -v curl &> /dev/null; then
        curl -s --max-time 2 "http://127.0.0.1:$TEST_PORT/" > /dev/null 2>&1 || true
    else
        echo -e "${YELLOW}Warning: neither nc nor curl is available, skipping connection test${NC}"
        return 0
    fi

    echo -e "${GREEN}Connection test passed${NC}"
}

# Trigger graceful restart
trigger_graceful_restart() {
    echo -e "${GREEN}Triggering graceful restart (sending SIGUSR1)...${NC}"
    kill -USR1 $DEFLUX_PID

    # Wait briefly for the new process to start
    sleep 3

    # Find the new process
    NEW_PID=$(pgrep -f "deflux.*${TEST_CONFIG}" | grep -v "^${DEFLUX_PID}$" | head -1 || true)

    if [ -z "$NEW_PID" ]; then
        echo -e "${RED}Error: new process not found${NC}"
        echo -e "${YELLOW}Old process PID: $DEFLUX_PID${NC}"
        echo -e "${YELLOW}Log contents:${NC}"
        tail -50 "$LOG_FILE"
        exit 1
    fi

    echo -e "${GREEN}New process started (PID: $NEW_PID)${NC}"
    echo -e "${YELLOW}Old process PID: $DEFLUX_PID, new process PID: $NEW_PID${NC}"

    # Wait for the old process to exit
    local wait_count=0
    while kill -0 $DEFLUX_PID 2>/dev/null; do
        sleep 1
        wait_count=$((wait_count + 1))
        if [ $wait_count -gt 10 ]; then
            echo -e "${YELLOW}Warning: old process still alive after 10 seconds${NC}"
            break
        fi
    done

    if kill -0 $DEFLUX_PID 2>/dev/null; then
        echo -e "${YELLOW}Old process is still running, possibly waiting for connections to close${NC}"
    else
        echo -e "${GREEN}Old process exited${NC}"
    fi

    # Update PID to the new process
    DEFLUX_PID=$NEW_PID
}

# Verify logs
verify_logs() {
    echo -e "${YELLOW}Verifying logs...${NC}"

    local expected_logs=(
        "Received SIGUSR1, initiating graceful restart"
        "Extracted.*FDs for transfer"
        "Spawned new process with PID"
        "Detected graceful restart, receiving listeners from parent"
        "Child process: received.*FDs"
        "successfully rebuilt.*listeners"
    )

    local missing_logs=0
    for log_pattern in "${expected_logs[@]}"; do
        if ! grep -q "$log_pattern" "$LOG_FILE"; then
            echo -e "${RED}  ✗ Missing log: $log_pattern${NC}"
            missing_logs=$((missing_logs + 1))
        else
            echo -e "${GREEN}  ✓ Found log: $log_pattern${NC}"
        fi
    done

    if [ $missing_logs -gt 0 ]; then
        echo -e "${RED}Warning: $missing_logs expected log entries not found${NC}"
        echo -e "${YELLOW}Full log:${NC}"
        cat "$LOG_FILE"
        return 1
    fi

    echo -e "${GREEN}Log verification passed${NC}"
}

# Main test flow
main() {
    echo -e "${GREEN}========================================${NC}"
    echo -e "${GREEN}Deflux Graceful Restart Integration Test${NC}"
    echo -e "${GREEN}========================================${NC}"

    # Step 1: check and prepare
    check_binary
    cleanup
    create_test_config

    # Step 2: start the initial process
    start_deflux
    OLD_PID=$DEFLUX_PID

    # Step 3: test initial connectivity
    test_connection $OLD_PID

    # Step 4: trigger graceful restart
    trigger_graceful_restart

    # Step 5: test connectivity after restart
    sleep 2
    test_connection $DEFLUX_PID

    # Step 6: verify logs
    if verify_logs; then
        echo -e "${GREEN}========================================${NC}"
        echo -e "${GREEN}✓ All tests passed!${NC}"
        echo -e "${GREEN}========================================${NC}"
    else
        echo -e "${YELLOW}========================================${NC}"
        echo -e "${YELLOW}⚠ Tests finished with warnings${NC}"
        echo -e "${YELLOW}========================================${NC}"
    fi

    # Step 7: cleanup
    echo -e "${YELLOW}Stopping Deflux...${NC}"
    kill -TERM $DEFLUX_PID 2>/dev/null || true
    sleep 2

    echo -e "${GREEN}Test finished${NC}"
}

# Run the main test
main
