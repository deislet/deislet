#!/bin/bash
#
# Start test backend servers for Deflux load balancer testing
#

set -e

# Check if Python 3 is available
if ! command -v python3 &> /dev/null; then
    echo "Error: Python 3 is required but not found"
    exit 1
fi

# Install required Python packages if not available
echo "Checking Python dependencies..."
python3 -c "import aiohttp, aiohttp_cors" 2>/dev/null || {
    echo "Installing required Python packages..."
    pip3 install aiohttp aiohttp-cors
}

# Default configuration
DEFAULT_PORTS="8001 8002 8003"
DEFAULT_NAME="test-backend"
DEFAULT_DELAY=0
DEFAULT_ERROR_RATE=0.0

# Parse command line arguments
PORTS=${1:-$DEFAULT_PORTS}
NAME=${2:-$DEFAULT_NAME}
DELAY=${3:-$DEFAULT_DELAY}
ERROR_RATE=${4:-$DEFAULT_ERROR_RATE}

echo "Starting test backend servers..."
echo "Ports: $PORTS"
echo "Name: $NAME"
echo "Delay: ${DELAY}ms"
echo "Error Rate: ${ERROR_RATE}"
echo

# Check if test_backend.py exists
if [ ! -f "test_backend.py" ]; then
    echo "Error: test_backend.py not found in current directory"
    exit 1
fi

# Start backend servers
echo "Starting backend servers..."
exec python3 test_backend.py $PORTS --name "$NAME" --delay "$DELAY" --error-rate "$ERROR_RATE"