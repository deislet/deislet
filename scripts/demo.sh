#!/usr/bin/env bash

set -euo pipefail

ROOT_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
DEMO_DIR="${DEMO_DIR:-${ROOT_DIR}/.demo}"
LOG_DIR="${LOG_DIR:-${DEMO_DIR}/logs}"
STATE_FILE="${STATE_FILE:-${DEMO_DIR}/state.env}"

DEFAULT_DEIS_CONTROL_PORT=8085
DEFAULT_DEIS_RUNTIME_PORT=8086
DEFAULT_DEIS_PROXY_PORT=8087

usage() {
  cat <<'EOF'
Usage:
  demo.sh [start|stop|status] [options]

Options:
  --deis-control-port <port>       Control HTTP/gRPC port (default 8085)
  --deis-runtime-port <port>        Runtime HTTP port (default 8086)
  --deis-proxy-port <port>       Proxy HTTP port (default 8087)
  --secret <hex>             Supervisor shared secret (64-char hex)
  --demo-dir <path>          Runtime directory (default .demo)
  --log-dir <path>           Log directory (default .demo/logs)
  --help                     Show this help
EOF
}

require_command() {
  local name=$1
  if ! command -v "$name" >/dev/null 2>&1; then
    echo "❌ Missing dependency: $name"
    exit 1
  fi
}

ensure_dirs() {
  mkdir -p "$LOG_DIR"
}

port_in_use() {
  local port=$1
  if command -v lsof >/dev/null 2>&1; then
    lsof -iTCP:"$port" -sTCP:LISTEN >/dev/null 2>&1
  elif command -v ss >/dev/null 2>&1; then
    if ss -lnt | grep -qE "[:.]${port}([[:space:]]|$)"; then
      return 0
    fi
    if command -v sudo >/dev/null 2>&1 && sudo -n ss -lntp 2>/dev/null | grep -qE "[:.]${port}([[:space:]]|$)"; then
      return 0
    fi
    return 1
  elif command -v nc >/dev/null 2>&1; then
    nc -z 127.0.0.1 "$port" >/dev/null 2>&1 || nc -z ::1 "$port" >/dev/null 2>&1
  else
    echo "❌ No port probing tool found, please install ss/lsof/nc" >&2
    exit 1
  fi
}

find_available_port() {
  local port=$1
  while port_in_use "$port"; do
    port=$((port + 1))
  done
  echo "$port"
}

generate_secret() {
  if [ -n "${DEMO_SECRET:-}" ]; then
    echo "$DEMO_SECRET"
    return
  fi

  if command -v openssl >/dev/null 2>&1; then
    openssl rand -hex 32
  else
    od -An -N32 -tx1 /dev/urandom | tr -d ' \n'
  fi
}

# Wait until the URL answers with 2xx. curl paces the retries itself
# (1s apart), so there is no busy loop and no shell-level sleeping.
wait_for_http() {
  local url=$1
  local attempts=$2
  curl -sf -o /dev/null -m 2 \
    --retry "$attempts" --retry-delay 1 --retry-max-time $((attempts * 2)) \
    --retry-connrefused --retry-all-errors \
    "$url" >/dev/null 2>&1
}

# Wait until the URL answers with any HTTP status at all (services like
# deis-runtime reject unrouted requests with 4xx while being perfectly healthy).
wait_for_http_any() {
  local url=$1
  local attempts=$2
  curl -s -o /dev/null -m 2 \
    --retry "$attempts" --retry-delay 1 --retry-max-time $((attempts * 2)) \
    --retry-connrefused \
    "$url" >/dev/null 2>&1
}

pid_running() {
  local pid=$1
  if [ -z "$pid" ]; then
    return 1
  fi
  kill -0 "$pid" >/dev/null 2>&1
}

write_state() {
  cat > "$STATE_FILE" <<EOF
DEIS_CONTROL_PORT=$DEIS_CONTROL_PORT
DEIS_RUNTIME_PORT=$DEIS_RUNTIME_PORT
DEIS_PROXY_PORT=$DEIS_PROXY_PORT
DEIS_CONTROL_PID=$DEIS_CONTROL_PID
DEIS_RUNTIME_PID=$DEIS_RUNTIME_PID
DEIS_PROXY_PID=$DEIS_PROXY_PID
DEIS_CONTROL_CONFIG=$DEIS_CONTROL_CONFIG
DEIS_RUNTIME_CONFIG=$DEIS_RUNTIME_CONFIG
DEIS_PROXY_CONFIG=$DEIS_PROXY_CONFIG
SECRET=$SECRET
EOF
}

load_state() {
  if [ ! -f "$STATE_FILE" ]; then
    echo "⚠️  Run state not found: $STATE_FILE"
    exit 1
  fi
  # shellcheck disable=SC1090
  source "$STATE_FILE"
}

start_demo() {
  ensure_dirs
  require_command cargo
  require_command curl

  if ! command -v lsof >/dev/null 2>&1 \
    && ! command -v ss >/dev/null 2>&1 \
    && ! command -v nc >/dev/null 2>&1; then
    echo "❌ No port probing tool found, please install ss/lsof/nc" >&2
    exit 1
  fi

  local user_deis_control_port
  local user_deis_runtime_port
  local user_deis_proxy_port
  user_deis_control_port=${DEIS_CONTROL_PORT:-}
  user_deis_runtime_port=${DEIS_RUNTIME_PORT:-}
  user_deis_proxy_port=${DEIS_PROXY_PORT:-}

  if [ -f "$STATE_FILE" ]; then
    # shellcheck disable=SC1090
    source "$STATE_FILE"
    if pid_running "${DEIS_CONTROL_PID:-}" || pid_running "${DEIS_RUNTIME_PID:-}" || pid_running "${DEIS_PROXY_PID:-}"; then
      echo "⚠️  An existing instance is running, please run stop first"
      exit 1
    fi
  fi

  DEIS_CONTROL_PORT=$user_deis_control_port
  DEIS_RUNTIME_PORT=$user_deis_runtime_port
  DEIS_PROXY_PORT=$user_deis_proxy_port

  DEIS_RUNTIME_PORT=$(find_available_port "${DEIS_RUNTIME_PORT:-$DEFAULT_DEIS_RUNTIME_PORT}")
  DEIS_PROXY_PORT=$(find_available_port "${DEIS_PROXY_PORT:-$DEFAULT_DEIS_PROXY_PORT}")

  SECRET=${SECRET:-$(generate_secret)}

  DEIS_CONTROL_CONFIG="${DEMO_DIR}/deis-control.toml"
  DEIS_RUNTIME_CONFIG="${DEMO_DIR}/deis-runtime.toml"
  DEIS_PROXY_CONFIG="${DEMO_DIR}/deis-proxy.toml"

  local start_port
  start_port=${DEIS_CONTROL_PORT:-$DEFAULT_DEIS_CONTROL_PORT}
  local max_retries
  max_retries=20
  local attempt
  attempt=0
  local deis_control_ready
  deis_control_ready=0

  while [ $attempt -le $max_retries ]; do
    DEIS_CONTROL_PORT=$((start_port + attempt))

    cat > "$DEIS_CONTROL_CONFIG" <<EOF
bind_addr = "0.0.0.0"
port = ${DEIS_CONTROL_PORT}
heartbeat_interval = 30
heartbeat_timeout = 90
worker_preshared_keys = ["${SECRET}"]
deis_runtime_poll_interval = 5

[[deis_runtime_nodes]]
id = "deis-runtime-demo-01"
addr = "http://127.0.0.1:${DEIS_RUNTIME_PORT}"
EOF

    cat > "$DEIS_RUNTIME_CONFIG" <<EOF
http_port = ${DEIS_RUNTIME_PORT}

[supervisor]
enabled = true
addr = "http://127.0.0.1:${DEIS_CONTROL_PORT}"
associated_id = "deis-runtime-demo-01"
secret = "${SECRET}"
reconnect_interval = 5
EOF

    cat > "$DEIS_PROXY_CONFIG" <<EOF
listen_addr = ["127.0.0.1"]
listen_ports = [${DEIS_PROXY_PORT}]
database = "${DEMO_DIR}/deis-proxy.db"
pid_file = "${DEMO_DIR}/deis-proxy.pid"
upgrade_sock = "${DEMO_DIR}/deis_proxy_upgrade.sock"

[[upstreams]]
host = "127.0.0.1"
port = ${DEIS_RUNTIME_PORT}

[log.error_log]
level = "info"
path = "${LOG_DIR}/deis_proxy_error.log"

[supervisor]
addr = "http://127.0.0.1:${DEIS_CONTROL_PORT}"
associated_id = "deis-proxy-demo-01"
secret = "${SECRET}"
EOF

    echo "🚀 Starting Control (HTTP/gRPC: ${DEIS_CONTROL_PORT})"
    (
      cd "$ROOT_DIR"
      cargo run -p deis-control -- \
        --config "$DEIS_CONTROL_CONFIG" \
        --port "$DEIS_CONTROL_PORT" \
        --database "${DEMO_DIR}/deis-control.db" \
        > "${LOG_DIR}/deis-control.log" 2>&1
    ) &
    DEIS_CONTROL_PID=$!

    if wait_for_http "http://127.0.0.1:${DEIS_CONTROL_PORT}/health" 200; then
      deis_control_ready=1
      break
    fi

    if grep -q "Address already in use" "${LOG_DIR}/deis-control.log"; then
      echo "⚠️  Port ${DEIS_CONTROL_PORT} is already in use, trying the next port"
      kill "$DEIS_CONTROL_PID" || true
      attempt=$((attempt + 1))
      continue
    fi

    echo "❌ Control API is not ready, please check the log: ${LOG_DIR}/deis-control.log"
    exit 1
  done

  if [ $deis_control_ready -ne 1 ]; then
    echo "❌ Could not find an available port to start Control"
    exit 1
  fi

  echo "🚀 Starting Runtime (HTTP: ${DEIS_RUNTIME_PORT})"
  (
    cd "$ROOT_DIR"
    DEIS_RUNTIME_CONFIG="$DEIS_RUNTIME_CONFIG" \
      DEIS_RUNTIME_HTTP_PORT="$DEIS_RUNTIME_PORT" \
      cargo run -p deis-runtime \
      > "${LOG_DIR}/deis-runtime.log" 2>&1
  ) &
  DEIS_RUNTIME_PID=$!

  if ! wait_for_http_any "http://127.0.0.1:${DEIS_RUNTIME_PORT}/health" 60; then
    echo "⚠️  Runtime health check failed, please check the log: ${LOG_DIR}/deis-runtime.log"
  fi

  echo "🚀 Starting Proxy (HTTP: ${DEIS_PROXY_PORT})"
  (
    cd "$ROOT_DIR"
    cargo run -p deis-proxy --bin deis-proxy -- -c "$DEIS_PROXY_CONFIG" \
      > "${LOG_DIR}/deis-proxy.log" 2>&1
  ) &
  DEIS_PROXY_PID=$!

  if command -v jq >/dev/null 2>&1; then
    echo "🚀 Deploying example app"
    # Package the example app and deploy it through deis-control, which relays
    # to the deis-runtime gRPC management plane.
    APP_ZIP="${DEMO_DIR}/example-app.zip"
    if python3 -c "
import zipfile, pathlib, sys
src = pathlib.Path('${ROOT_DIR}/scripts/demo-assets/example-app')
with zipfile.ZipFile('${APP_ZIP}', 'w') as zf:
    for f in sorted(src.iterdir()):
        zf.write(f, f.name)
" 2> "${LOG_DIR}/deis_runtime_deploy.log"; then
      http_code=$(curl -s -o "${LOG_DIR}/deis_runtime_deploy.log" -w '%{http_code}' \
        -X PUT --data-binary "@${APP_ZIP}" \
        -H 'content-type: application/zip' \
        "http://127.0.0.1:${DEIS_CONTROL_PORT}/api/runtime/deis-runtime-demo-01/apps/demo" || echo 000)
      if [ "$http_code" = "200" ]; then
        echo "Example app deployed: send requests with header 'x-deis-app-id: demo'"
      else
        echo "WARN: example app deployment failed (HTTP ${http_code}), see ${LOG_DIR}/deis_runtime_deploy.log"
      fi
    else
      echo "WARN: failed to package example app, see ${LOG_DIR}/deis_runtime_deploy.log"
    fi
  else
    echo "⚠️  jq not found, skipping example app deployment"
  fi

  write_state

  echo "✅ Demo environment started"
  echo "- Control API: http://127.0.0.1:${DEIS_CONTROL_PORT}"
  echo "- Runtime: http://127.0.0.1:${DEIS_RUNTIME_PORT}"
  echo "- Proxy: http://127.0.0.1:${DEIS_PROXY_PORT}"
}

stop_demo() {
  load_state

  if pid_running "$DEIS_RUNTIME_PID"; then
    echo "🛑 Stopping Runtime (PID: $DEIS_RUNTIME_PID)"
    kill "$DEIS_RUNTIME_PID" || true
  fi

  if pid_running "$DEIS_PROXY_PID"; then
    echo "🛑 Stopping Proxy (PID: $DEIS_PROXY_PID)"
    kill "$DEIS_PROXY_PID" || true
  fi

  if pid_running "$DEIS_CONTROL_PID"; then
    echo "🛑 Stopping Control (PID: $DEIS_CONTROL_PID)"
    kill "$DEIS_CONTROL_PID" || true
  fi

  echo "✅ Stop signals sent; force-kill manually if needed"
}

status_demo() {
  load_state

  echo "Status"
  echo "- Control PID: ${DEIS_CONTROL_PID}"
  echo "- Runtime PID: ${DEIS_RUNTIME_PID}"
  echo "- Proxy PID: ${DEIS_PROXY_PID}"
  echo "- Control API: http://127.0.0.1:${DEIS_CONTROL_PORT}"
  echo "- Runtime: http://127.0.0.1:${DEIS_RUNTIME_PORT}"
  echo "- Proxy: http://127.0.0.1:${DEIS_PROXY_PORT}"
}

action="start"
if [ $# -gt 0 ] && [[ $1 != -* ]]; then
  action=$1
  shift
fi

while [ $# -gt 0 ]; do
  case "$1" in
    --deis-control-port)
      DEIS_CONTROL_PORT=$2
      shift 2
      ;;
      --deis-runtime-port)
      DEIS_RUNTIME_PORT=$2
      shift 2
      ;;
    --deis-proxy-port)
      DEIS_PROXY_PORT=$2
      shift 2
      ;;
    --secret)
      SECRET=$2
      shift 2
      ;;
    --demo-dir)
      DEMO_DIR=$2
      LOG_DIR="${DEMO_DIR}/logs"
      STATE_FILE="${DEMO_DIR}/state.env"
      shift 2
      ;;
    --log-dir)
      LOG_DIR=$2
      shift 2
      ;;
    --help)
      usage
      exit 0
      ;;
    *)
      echo "Unknown argument: $1"
      usage
      exit 1
      ;;
  esac
 done

case "$action" in
  start)
    start_demo
    ;;
  stop)
    stop_demo
    ;;
  status)
    status_demo
    ;;
  *)
    usage
    exit 1
    ;;
 esac
