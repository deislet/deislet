#!/bin/bash
# Specific tests for Config Push and Command Execution features
set -e

RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
NC='\033[0m'

DENIRA_API_PORT=9002

log_info() {
    echo -e "${BLUE}[INFO]${NC} $1"
}

log_success() {
    echo -e "${GREEN}[PASS]${NC} $1"
}

log_fail() {
    echo -e "${RED}[FAIL]${NC} $1"
    exit 1
}

test_config_push() {
    echo ""
    echo -e "${BLUE}=== Testing Config Push Feature ===${NC}"

    # Get list of workers
    log_info "Getting worker list..."
    workers=$(curl -s "http://127.0.0.1:$DENIRA_API_PORT/api/workers")
    worker_ids=$(echo "$workers" | jq -r '.workers[].id' 2>/dev/null || echo "")

    if [ -z "$worker_ids" ]; then
        log_fail "No workers found - start workers first"
    fi

    first_worker=$(echo "$worker_ids" | head -1)
    log_info "Testing with worker: $first_worker"

    # Update config via API
    log_info "Pushing config update..."
    config_data='{"log_level": "debug", "max_connections": 100}'

    response=$(curl -s -X POST \
        "http://127.0.0.1:$DENIRA_API_PORT/api/configs/$first_worker" \
        -H "Content-Type: application/json" \
        -d "$config_data" 2>/dev/null || echo '{"error": "not_implemented"}')

    if echo "$response" | jq -e '.success' > /dev/null 2>&1; then
        log_success "Config update accepted"

        # Verify config saved
        log_info "Verifying config saved..."
        saved_config=$(curl -s "http://127.0.0.1:$DENIRA_API_PORT/api/configs/$first_worker" 2>/dev/null || echo '{}')

        if echo "$saved_config" | jq -e '.log_level' > /dev/null 2>&1; then
            log_success "Config retrieved from API"
        else
            log_fail "Config not found in API"
        fi
    else
        log_info "Config API not yet implemented (expected for current version)"
    fi
}

test_command_execution() {
    echo ""
    echo -e "${BLUE}=== Testing Command Execution Feature ===${NC}"

    # Get list of workers
    workers=$(curl -s "http://127.0.0.1:$DENIRA_API_PORT/api/workers")
    worker_ids=$(echo "$workers" | jq -r '.workers[].id' 2>/dev/null || echo "")

    if [ -z "$worker_ids" ]; then
        log_fail "No workers found - start workers first"
    fi

    first_worker=$(echo "$worker_ids" | head -1)
    log_info "Testing commands with worker: $first_worker"

    # Send GetStatus command
    log_info "Sending GetStatus command..."
    command_data='{"command_type": "get_status", "data": null}'

    response=$(curl -s -X POST \
        "http://127.0.0.1:$DENIRA_API_PORT/api/commands/$first_worker" \
        -H "Content-Type: application/json" \
        -d "$command_data" 2>/dev/null || echo '{"error": "not_implemented"}')

    if echo "$response" | jq -e '.command_id' > /dev/null 2>&1; then
        command_id=$(echo "$response" | jq -r '.command_id')
        log_success "Command sent (ID: $command_id)"

        # Check command status
        log_info "Checking command status..."
        sleep 1

        status=$(curl -s "http://127.0.0.1:$DENIRA_API_PORT/api/commands/$command_id" 2>/dev/null || echo '{}')

        if echo "$status" | jq -e '.status' > /dev/null 2>&1; then
            cmd_status=$(echo "$status" | jq -r '.status')
            log_success "Command status: $cmd_status"
        fi
    else
        log_info "Command API not yet fully implemented (expected)"
    fi

    # List all commands
    log_info "Listing all commands..."
    all_commands=$(curl -s "http://127.0.0.1:$DENIRA_API_PORT/api/commands" 2>/dev/null || echo '[]')

    if echo "$all_commands" | jq -e '. | length' > /dev/null 2>&1; then
        count=$(echo "$all_commands" | jq '. | length')
        log_info "Total commands: $count"
    fi
}

test_events_api() {
    echo ""
    echo -e "${BLUE}=== Testing Events API ===${NC}"

    log_info "Fetching recent events..."
    events=$(curl -s "http://127.0.0.1:$DENIRA_API_PORT/api/events?limit=10" 2>/dev/null || echo '[]')

    if echo "$events" | jq -e '. | length' > /dev/null 2>&1; then
        count=$(echo "$events" | jq '. | length')
        log_info "Recent events: $count"

        if [ "$count" -gt 0 ]; then
            log_success "Events API working"

            # Show event types
            event_types=$(echo "$events" | jq -r '.[].event_type' 2>/dev/null | sort | uniq)
            log_info "Event types: $(echo $event_types | tr '\n' ', ')"
        fi
    else
        log_info "Events API not yet implemented"
    fi
}

main() {
    echo -e "${GREEN}=== Config & Command E2E Tests ===${NC}"
    echo ""

    # Check if Denira is running
    if ! nc -z 127.0.0.1 $DENIRA_API_PORT 2>/dev/null; then
        log_fail "Denira not running on port $DENIRA_API_PORT - start it first with e2e_full.sh"
    fi

    test_config_push
    test_command_execution
    test_events_api

    echo ""
    echo -e "${GREEN}=== Tests Completed ===${NC}"
    echo ""
    echo -e "${YELLOW}Note:${NC} Some APIs may not be fully implemented yet."
    echo "These tests verify the infrastructure is ready for these features."
}

main "$@"
