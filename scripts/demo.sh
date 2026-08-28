#!/usr/bin/env bash

set -euo pipefail

ROOT_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
DEMO_DIR="${DEMO_DIR:-${ROOT_DIR}/.demo}"
LOG_DIR="${LOG_DIR:-${DEMO_DIR}/logs}"
STATE_FILE="${STATE_FILE:-${DEMO_DIR}/state.env}"

DEFAULT_DENIRA_PORT=8085
DEFAULT_DENIX_PORT=8086
DEFAULT_DEFLUX_PORT=8087

usage() {
  cat <<'EOF'
Usage:
  demo.sh [start|stop|status] [options]

Options:
  --denira-port <port>       Denira HTTP/gRPC port (default 8085)
  --denix-port <port>        Denix HTTP port (default 8086)
  --deflux-port <port>       Deflux HTTP port (default 8087)
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
# denix reject unrouted requests with 4xx while being perfectly healthy).
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
DENIRA_PORT=$DENIRA_PORT
DENIX_PORT=$DENIX_PORT
DEFLUX_PORT=$DEFLUX_PORT
DENIRA_PID=$DENIRA_PID
DENIX_PID=$DENIX_PID
DEFLUX_PID=$DEFLUX_PID
DENIRA_CONFIG=$DENIRA_CONFIG
DENIX_CONFIG=$DENIX_CONFIG
DEFLUX_CONFIG=$DEFLUX_CONFIG
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

  local user_denira_port
  local user_denix_port
  local user_deflux_port
  user_denira_port=${DENIRA_PORT:-}
  user_denix_port=${DENIX_PORT:-}
  user_deflux_port=${DEFLUX_PORT:-}

  if [ -f "$STATE_FILE" ]; then
    # shellcheck disable=SC1090
    source "$STATE_FILE"
    if pid_running "$DENIRA_PID" || pid_running "$DENIX_PID" || pid_running "$DEFLUX_PID"; then
      echo "⚠️  An existing instance is running, please run stop first"
      exit 1
    fi
  fi

  DENIRA_PORT=$user_denira_port
  DENIX_PORT=$user_denix_port
  DEFLUX_PORT=$user_deflux_port

  DENIX_PORT=$(find_available_port "${DENIX_PORT:-$DEFAULT_DENIX_PORT}")
  DEFLUX_PORT=$(find_available_port "${DEFLUX_PORT:-$DEFAULT_DEFLUX_PORT}")

  SECRET=${SECRET:-$(generate_secret)}

  DENIRA_CONFIG="${DEMO_DIR}/denira.toml"
  DENIX_CONFIG="${DEMO_DIR}/denix.toml"
  DEFLUX_CONFIG="${DEMO_DIR}/deflux.toml"

  local start_port
  start_port=${DENIRA_PORT:-$DEFAULT_DENIRA_PORT}
  local max_retries
  max_retries=20
  local attempt
  attempt=0
  local denira_ready
  denira_ready=0

  while [ $attempt -le $max_retries ]; do
    DENIRA_PORT=$((start_port + attempt))

    cat > "$DENIRA_CONFIG" <<EOF
bind_addr = "0.0.0.0"
port = ${DENIRA_PORT}
heartbeat_interval = 30
heartbeat_timeout = 90
worker_preshared_keys = ["${SECRET}"]
denix_poll_interval = 5

[[denix_nodes]]
id = "denix-demo-01"
addr = "http://127.0.0.1:${DENIX_PORT}"
EOF

    cat > "$DENIX_CONFIG" <<EOF
http_port = ${DENIX_PORT}

[supervisor]
enabled = true
addr = "http://127.0.0.1:${DENIRA_PORT}"
associated_id = "denix-demo-01"
secret = "${SECRET}"
reconnect_interval = 5
EOF

    cat > "$DEFLUX_CONFIG" <<EOF
listen_addr = ["127.0.0.1"]
listen_ports = [${DEFLUX_PORT}]
database = "${DEMO_DIR}/deflux.db"
pid_file = "${DEMO_DIR}/deflux.pid"
upgrade_sock = "${DEMO_DIR}/deflux_upgrade.sock"

[[upstreams]]
host = "127.0.0.1"
port = ${DENIX_PORT}

[log.error_log]
level = "info"
path = "${LOG_DIR}/deflux_error.log"

[supervisor]
addr = "http://127.0.0.1:${DENIRA_PORT}"
associated_id = "deflux-demo-01"
secret = "${SECRET}"
EOF

    echo "🚀 Starting Denira (HTTP/gRPC: ${DENIRA_PORT})"
    (
      cd "$ROOT_DIR"
      cargo run -p denira -- \
        --config "$DENIRA_CONFIG" \
        --port "$DENIRA_PORT" \
        --database "${DEMO_DIR}/denira.db" \
        > "${LOG_DIR}/denira.log" 2>&1
    ) &
    DENIRA_PID=$!

    if wait_for_http "http://127.0.0.1:${DENIRA_PORT}/health" 200; then
      denira_ready=1
      break
    fi

    if grep -q "Address already in use" "${LOG_DIR}/denira.log"; then
      echo "⚠️  Port ${DENIRA_PORT} is already in use, trying the next port"
      kill "$DENIRA_PID" || true
      attempt=$((attempt + 1))
      continue
    fi

    echo "❌ Denira API is not ready, please check the log: ${LOG_DIR}/denira.log"
    exit 1
  done

  if [ $denira_ready -ne 1 ]; then
    echo "❌ Could not find an available port to start Denira"
    exit 1
  fi

  echo "🚀 Starting Denix (HTTP: ${DENIX_PORT})"
  (
    cd "$ROOT_DIR"
    DENIX_CONFIG="$DENIX_CONFIG" \
      DENIX_HTTP_PORT="$DENIX_PORT" \
      cargo run -p denix \
      > "${LOG_DIR}/denix.log" 2>&1
  ) &
  DENIX_PID=$!

  if ! wait_for_http_any "http://127.0.0.1:${DENIX_PORT}/health" 60; then
    echo "⚠️  Denix health check failed, please check the log: ${LOG_DIR}/denix.log"
  fi

  echo "🚀 Starting Deflux (HTTP: ${DEFLUX_PORT})"
  (
    cd "$ROOT_DIR"
    cargo run -p deflux --bin deflux -- -c "$DEFLUX_CONFIG" \
      > "${LOG_DIR}/deflux.log" 2>&1
  ) &
  DEFLUX_PID=$!

  if command -v jq >/dev/null 2>&1; then
    echo "🚀 Deploying example app"
    # Package the example app and deploy it through denira, which relays
    # to the denix gRPC management plane.
    APP_ZIP="${DEMO_DIR}/example-app.zip"
    if python3 -c "
import zipfile, pathlib, sys
src = pathlib.Path('${ROOT_DIR}/scripts/demo-assets/example-app')
with zipfile.ZipFile('${APP_ZIP}', 'w') as zf:
    for f in sorted(src.iterdir()):
        zf.write(f, f.name)
" 2> "${LOG_DIR}/denix_deploy.log"; then
      http_code=$(curl -s -o "${LOG_DIR}/denix_deploy.log" -w '%{http_code}' \
        -X PUT --data-binary "@${APP_ZIP}" \
        -H 'content-type: application/zip' \
        "http://127.0.0.1:${DENIRA_PORT}/api/denix/denix-demo-01/apps/demo" || echo 000)
      if [ "$http_code" = "200" ]; then
        echo "Example app deployed: send requests with header 'x-deis-app-id: demo'"
      else
        echo "WARN: example app deployment failed (HTTP ${http_code}), see ${LOG_DIR}/denix_deploy.log"
      fi
    else
      echo "WARN: failed to package example app, see ${LOG_DIR}/denix_deploy.log"
    fi
  else
    echo "⚠️  jq not found, skipping example app deployment"
  fi

  write_state

  echo "✅ Demo environment started"
  echo "- Denira API: http://127.0.0.1:${DENIRA_PORT}"
  echo "- Denix: http://127.0.0.1:${DENIX_PORT}"
  echo "- Deflux: http://127.0.0.1:${DEFLUX_PORT}"
}

stop_demo() {
  load_state

  if pid_running "$DENIX_PID"; then
    echo "🛑 Stopping Denix (PID: $DENIX_PID)"
    kill "$DENIX_PID" || true
  fi

  if pid_running "$DEFLUX_PID"; then
    echo "🛑 Stopping Deflux (PID: $DEFLUX_PID)"
    kill "$DEFLUX_PID" || true
  fi

  if pid_running "$DENIRA_PID"; then
    echo "🛑 Stopping Denira (PID: $DENIRA_PID)"
    kill "$DENIRA_PID" || true
  fi

  echo "✅ Stop signals sent; force-kill manually if needed"
}

status_demo() {
  load_state

  echo "Status"
  echo "- Denira PID: ${DENIRA_PID}"
  echo "- Denix PID: ${DENIX_PID}"
  echo "- Deflux PID: ${DEFLUX_PID}"
  echo "- Denira API: http://127.0.0.1:${DENIRA_PORT}"
  echo "- Denix: http://127.0.0.1:${DENIX_PORT}"
  echo "- Deflux: http://127.0.0.1:${DEFLUX_PORT}"
}

action="start"
if [ $# -gt 0 ] && [[ $1 != -* ]]; then
  action=$1
  shift
fi

while [ $# -gt 0 ]; do
  case "$1" in
    --denira-port)
      DENIRA_PORT=$2
      shift 2
      ;;
      --denix-port)
      DENIX_PORT=$2
      shift 2
      ;;
    --deflux-port)
      DEFLUX_PORT=$2
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
