#!/usr/bin/env bash

set -euo pipefail

ROOT_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
DEMO_DIR="${DEMO_DIR:-${ROOT_DIR}/.demo}"
LOG_DIR="${LOG_DIR:-${DEMO_DIR}/logs}"
STATE_FILE="${STATE_FILE:-${DEMO_DIR}/state.env}"
# Where `cargo build` leaves the demo binaries. CARGO_TARGET_DIR is honoured
# because this repo is often built with a shared target directory.
BIN_DIR="${CARGO_TARGET_DIR:-${ROOT_DIR}/target}/debug"

DEFAULT_DEIS_CONTROL_PORT=8085
DEFAULT_DEIS_RUNTIME_PORT=8086
DEFAULT_DEIS_PROXY_PORT=8087
DEFAULT_DEIS_STORE_PORT=8090
DEFAULT_DEIS_QUEUE_PORT=8091
DEFAULT_DEIS_TELEMETRY_PORT=8092
DEFAULT_DEIS_TELEMETRY_METRICS_PORT=8093

# The example app, and the names the demo files it under. deis-store and
# deis-queue still understand exactly one tenant dimension, but that key is now
# the application and the environment joined by a dot - "demo.production" - and
# it travels in the x-deis-tenant header. So a preview environment of this same
# app gets its own KV, database, blob and queue namespace and cannot read or
# overwrite what production stored.
APP_SRC_DIR="${APP_SRC_DIR:-${ROOT_DIR}/scripts/demo-assets/example-app}"
APP_ID="${APP_ID:-demo}"
ENV_NAME="${ENV_NAME:-production}"
# A second environment of the same application, deployed to a different
# version on the same node. It is what proves an application is identified by
# (app, environment) and not by app id alone.
PREVIEW_ENV_NAME="${PREVIEW_ENV_NAME:-preview}"
NODE_ID="${NODE_ID:-deis-runtime-demo-01}"
# Deliberately recognisable: `verify` greps the logs and the data directories
# for it to prove a decrypted secret is never written down anywhere.
DEMO_SECRET_VALUE="${DEMO_SECRET_VALUE:-demo-secret-do-not-log-me}"

usage() {
  cat <<'EOF'
Usage:
  demo.sh [start|stop|status|verify] [options]

Actions:
  start   Build everything, bring the six components up, and deploy the
          example app the way a developer would - through the `deis` CLI:
          `deis deploy` builds the bundle, creates the app and its
          environment, uploads the bundle as an immutable version and points
          the environment at it, then `deis secret set` adds a secret. The
          node fetches the bundle itself. Nothing here calls the control
          plane's REST API by hand.
  verify  Exercise the running demo end to end and report each check.
  stop    Stop every component and wait until each has really exited.
  status  Print pids, ports and what is deployed.

Options:
  --deis-control-port <port>            Control HTTP/gRPC port (default 8085)
  --deis-runtime-port <port>            Runtime HTTP port (default 8086)
  --deis-proxy-port <port>              Proxy HTTP port (default 8087)
  --deis-store-port <port>              Store gRPC port, KV/DB/Blob (default 8090)
  --deis-queue-port <port>              Queue gRPC port (default 8091)
  --deis-telemetry-port <port>          Telemetry gRPC port (default 8092)
  --deis-telemetry-metrics-port <port>  Telemetry Prometheus port (default 8093)
  --secret <hex>                        Control shared secret (64-char hex)
  --demo-dir <path>                     Runtime directory (default .demo)
  --app-dir <path>                      Example app sources (default scripts/demo-assets/example-app)
  --log-dir <path>                      Log directory (default .demo/logs)
  --help                                Show this help
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

# `start` begins a new run, so it clears what the previous one left behind.
#
# This is not tidiness. deis-store and deis-queue partition by tenant --
# "<app>.<environment>" -- and refuse to start on a directory written before
# that, because opening it would create a second, empty set of tables beside
# the old rows: every application would look healthy with all of its data
# gone. The demo would rather hand them an empty directory than have anyone
# meet that refusal. The master key is kept, so the encrypted secrets in a
# database that survives an interrupted run still decrypt.
reset_previous_run() {
  rm -rf \
    "${DEMO_DIR}/deis-store-data" \
    "${DEMO_DIR}/deis-queue-data" \
    "${DEMO_DIR}/deis-telemetry-data" \
    "${DEMO_DIR}/deis-runtime-data" \
    "${DEMO_DIR}/bundles" \
    "${DEMO_DIR}/deis-control.db" \
    "${DEMO_DIR}/deis-proxy.db" \
    "${DEMO_DIR}/deis-config"
}

# `ss` is asked first on purpose: it lists every listening socket on the host,
# while `lsof` without root only sees this user's own processes. Trusting lsof
# first makes a port owned by another user look free, and the service that is
# then handed that port dies on bind.
port_in_use() {
  local port=$1
  if command -v ss >/dev/null 2>&1; then
    ss -lnt | grep -qE "[:.]${port}([[:space:]]|$)"
  elif command -v lsof >/dev/null 2>&1; then
    lsof -iTCP:"$port" -sTCP:LISTEN >/dev/null 2>&1
  elif command -v nc >/dev/null 2>&1; then
    nc -z 127.0.0.1 "$port" >/dev/null 2>&1 || nc -z ::1 "$port" >/dev/null 2>&1
  else
    echo "❌ No port probing tool found, please install ss/lsof/nc" >&2
    exit 1
  fi
}

# Ports already handed out during this start, which nothing is listening on
# yet. Without this record two services asked for a port in the same second
# would both be given the same free one, and the second to bind would die.
CLAIMED_PORTS=""

port_claimed() {
  case " ${CLAIMED_PORTS} " in
    *" $1 "*) return 0 ;;
    *) return 1 ;;
  esac
}

# Pick the first free port at or above $2 and store it in the variable named
# by $1. It assigns rather than echoes because a command substitution runs in
# a subshell, which would throw away the record of what was already claimed.
claim_port() {
  local var=$1
  local port=$2
  while port_in_use "$port" || port_claimed "$port"; do
    port=$((port + 1))
  done
  CLAIMED_PORTS="${CLAIMED_PORTS} ${port}"
  printf -v "$var" '%s' "$port"
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

# Wait until a gRPC listener accepts TCP connections. gRPC speaks h2c, so a
# plain curl request gets no usable HTTP reply; what proves readiness is that
# the connection was established at all. curl exits 7 only when it never
# managed to connect, so that one code is the not-ready signal and anything
# else means the socket answered. curl paces its own retries (1s apart), so
# there is no busy loop and no shell-level sleeping.
wait_for_tcp() {
  local port=$1
  local attempts=$2
  local rc=0
  curl -s -o /dev/null -m 2 \
    --retry "$attempts" --retry-delay 1 --retry-max-time $((attempts * 2)) \
    --retry-connrefused \
    "http://127.0.0.1:${port}/" >/dev/null 2>&1 || rc=$?
  [ "$rc" -ne 7 ]
}

# Wait for one service to start serving, giving up early if it dies.
#
# Each round hands the pacing to curl, which spaces its own retries a second
# apart, and then re-checks the process; there is no shell-level sleeping. The
# process check matters twice over: a service killed by a port clash dies in
# milliseconds, and -- because a port held by an unrelated process answers the
# probe perfectly well -- a socket that responds is only proof of readiness if
# our own process is the one still running.
#
# Returns 0 when ready, 1 when it never became ready, 2 when it exited.
wait_for_service() {
  local probe=$1
  local target=$2
  local pid=$3
  local rounds=$4
  local round=0

  while [ "$round" -lt "$rounds" ]; do
    if ! pid_running "$pid"; then
      return 2
    fi
    if "$probe" "$target" 2; then
      if pid_running "$pid"; then
        return 0
      fi
      return 2
    fi
    round=$((round + 1))
  done
  return 1
}

# Say what a readiness outcome means for the demo. A service that died left
# its reason in its own log, and the usual reason is a port clash, so the last
# few lines are shown inline rather than left to be hunted down.
report_service() {
  local name=$1
  local rc=$2
  local log=$3
  local consequence=$4

  case "$rc" in
    0)
      echo "   ✓ ${name} is serving"
      ;;
    2)
      echo "❌ ${name} exited during startup; ${consequence}"
      tail -n 5 "$log" 2>/dev/null | sed 's/^/     /'
      ;;
    *)
      echo "⚠️  ${name} never became ready; ${consequence}"
      echo "     See ${log}"
      ;;
  esac
}

pid_running() {
  local pid=$1
  if [ -z "$pid" ]; then
    return 1
  fi
  kill -0 "$pid" >/dev/null 2>&1
}

# Save what `verify`, `status` and `stop` need in a later shell.
#
# Owner-only: this file holds the node pre-shared key the components
# authenticate to each other with, and the value of the demo secret, which
# `verify` needs in order to go looking for it.
write_state() {
  touch "$STATE_FILE"
  chmod 600 "$STATE_FILE"
  cat > "$STATE_FILE" <<EOF
DEIS_CONTROL_PORT=$DEIS_CONTROL_PORT
DEIS_RUNTIME_PORT=$DEIS_RUNTIME_PORT
DEIS_PROXY_PORT=$DEIS_PROXY_PORT
DEIS_STORE_PORT=$DEIS_STORE_PORT
DEIS_QUEUE_PORT=$DEIS_QUEUE_PORT
DEIS_TELEMETRY_PORT=$DEIS_TELEMETRY_PORT
DEIS_TELEMETRY_METRICS_PORT=$DEIS_TELEMETRY_METRICS_PORT
DEIS_CONTROL_PID=$DEIS_CONTROL_PID
DEIS_RUNTIME_PID=$DEIS_RUNTIME_PID
DEIS_PROXY_PID=$DEIS_PROXY_PID
DEIS_STORE_PID=$DEIS_STORE_PID
DEIS_QUEUE_PID=$DEIS_QUEUE_PID
DEIS_TELEMETRY_PID=$DEIS_TELEMETRY_PID
DEIS_CONTROL_CONFIG=$DEIS_CONTROL_CONFIG
DEIS_RUNTIME_CONFIG=$DEIS_RUNTIME_CONFIG
DEIS_PROXY_CONFIG=$DEIS_PROXY_CONFIG
DEIS_STORE_CONFIG=$DEIS_STORE_CONFIG
DEIS_QUEUE_CONFIG=$DEIS_QUEUE_CONFIG
DEIS_TELEMETRY_CONFIG=$DEIS_TELEMETRY_CONFIG
SECRET=$SECRET
SECRET_KEY_FILE=$SECRET_KEY_FILE
APP_ID=$APP_ID
ENV_NAME=$ENV_NAME
PREVIEW_ENV_NAME=$PREVIEW_ENV_NAME
NODE_ID=$NODE_ID
APP_SRC_DIR=$APP_SRC_DIR
APP_V1_ID=${APP_V1_ID:-}
APP_V2_ID=${APP_V2_ID:-}
APP_V1_NUMBER=${APP_V1_NUMBER:-}
APP_V2_NUMBER=${APP_V2_NUMBER:-}
DEMO_SECRET_VALUE=$DEMO_SECRET_VALUE
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

# Bring up the three services the platform stores things in or reports to:
# deis-store (KV/DB/Blob), deis-queue and deis-telemetry. They are started
# before the control plane on purpose: none of them depends on it, while
# every other component ships logs at deis-telemetry, so it should already be
# listening when the rest come up.
#
# Each one is a gRPC service. deis-telemetry additionally exposes a plain
# HTTP scrape endpoint, which is a real answer and so a stronger readiness
# signal; the other two can only be probed at the TCP level.
# Build every demo binary once, up front.
#
# The services are then launched as binaries rather than through `cargo run`,
# for two reasons. Six concurrent `cargo run` invocations queue up on the
# build-directory lock, so the ones at the back look hung. More importantly,
# `cargo run` makes the service a grandchild: `$!` is cargo's pid, and killing
# it on `stop` leaves the real process running and its port occupied.
build_binaries() {
  echo "🔨 Building demo binaries"
  if ! (
    cd "$ROOT_DIR"
    cargo build \
      -p deis-control -p deis-runtime -p deis-proxy \
      -p deis-store -p deis-queue -p deis-telemetry \
      -p deis
  ) > "${LOG_DIR}/build.log" 2>&1; then
    echo "❌ Build failed, nothing was started. See ${LOG_DIR}/build.log"
    tail -n 20 "${LOG_DIR}/build.log" | sed 's/^/     /'
    exit 1
  fi
}

start_data_plane() {
  cat > "$DEIS_TELEMETRY_CONFIG" <<EOF
listen = "127.0.0.1:${DEIS_TELEMETRY_PORT}"
metrics_listen = "127.0.0.1:${DEIS_TELEMETRY_METRICS_PORT}"
data_dir = "${DEMO_DIR}/deis-telemetry-data"
EOF

  cat > "$DEIS_STORE_CONFIG" <<EOF
listen = "127.0.0.1:${DEIS_STORE_PORT}"
data_dir = "${DEMO_DIR}/deis-store-data"
EOF

  cat > "$DEIS_QUEUE_CONFIG" <<EOF
listen = "127.0.0.1:${DEIS_QUEUE_PORT}"
data_dir = "${DEMO_DIR}/deis-queue-data"
EOF

  echo "🚀 Starting Telemetry (gRPC: ${DEIS_TELEMETRY_PORT}, metrics: ${DEIS_TELEMETRY_METRICS_PORT})"
  "${BIN_DIR}/deis-telemetry" --config "$DEIS_TELEMETRY_CONFIG" \
    > "${LOG_DIR}/deis-telemetry.log" 2>&1 &
  DEIS_TELEMETRY_PID=$!

  echo "🚀 Starting Store (gRPC: ${DEIS_STORE_PORT})"
  "${BIN_DIR}/deis-store" --config "$DEIS_STORE_CONFIG" \
    > "${LOG_DIR}/deis-store.log" 2>&1 &
  DEIS_STORE_PID=$!

  echo "🚀 Starting Queue (gRPC: ${DEIS_QUEUE_PORT})"
  "${BIN_DIR}/deis-queue" --config "$DEIS_QUEUE_CONFIG" \
    > "${LOG_DIR}/deis-queue.log" 2>&1 &
  DEIS_QUEUE_PID=$!

  # All three are launched before any is probed, so they come up in parallel.
  local rc

  rc=0
  wait_for_service wait_for_http \
    "http://127.0.0.1:${DEIS_TELEMETRY_METRICS_PORT}/metrics" "$DEIS_TELEMETRY_PID" 60 || rc=$?
  report_service "Telemetry" "$rc" "${LOG_DIR}/deis-telemetry.log" \
    "logs and metrics pushed at it are dropped"

  rc=0
  wait_for_service wait_for_tcp "$DEIS_STORE_PORT" "$DEIS_STORE_PID" 60 || rc=$?
  report_service "Store" "$rc" "${LOG_DIR}/deis-store.log" \
    "ctx.kv / ctx.db / ctx.blob calls will fail"

  rc=0
  wait_for_service wait_for_tcp "$DEIS_QUEUE_PORT" "$DEIS_QUEUE_PID" 60 || rc=$?
  report_service "Queue" "$rc" "${LOG_DIR}/deis-queue.log" \
    "ctx.queue calls will fail"
}

# ----------------------------------------------------------------------
# Deploying the example app the way a developer would
# ----------------------------------------------------------------------
#
# Nothing here is a shortcut. The same five steps a person takes:
#
#   1. `deis build --vendor deislet`  -> dist/deislet/bundle.zip
#   2. POST /api/apps                 -> the application exists
#   3. POST .../versions              -> the bundle becomes an immutable version
#   4. POST .../environments          -> production exists
#   5. POST .../deployments           -> the environment points at the version
#
# Step 5 is where the control plane pushes a pointer to every node; each node
# downloads the bundle over HTTP, checks its sha256 and loads it. The bundle
# itself never travels inside a gRPC message.

# Print the control plane's base URL.
control_url() {
  echo "http://127.0.0.1:${DEIS_CONTROL_PORT}"
}

# Run the developer CLI the way a developer would.
#
# Every deployment step below goes through this rather than through curl
# against the REST API, and that is the point: `deis` is what a person has.
# A demo that reached past it into the API would prove the control plane
# works while leaving the only interface anybody actually types untested --
# and the first person to try would be the one to find out.
#
# The platform's address is handed over in the environment rather than in a
# deis.toml, because the demo picks its ports at start time and a file
# committed to the repo cannot know them.
#
# DEIS_CONFIG_HOME is the load-bearing one. Without it the CLI reads, and
# `deis config set` would write, the developer's own
# ~/.config/deis/credentials.toml. A demo run must never touch the credential
# someone uses against a real control plane, so it gets a config home of its
# own inside the demo directory.
#
# The settings are collected into an array rather than written as a command
# prefix so that check_cli_tail can `exec` the binary directly inside its
# coprocess. A `deis tail` that has to be interrupted must be the process
# that receives the signal; a shell wrapping it would swallow the Ctrl-C the
# CLI is written to handle.
deis_cli_env() {
  DEIS_CLI_ENV=(
    "DEIS_CONTROL_ENDPOINT=$(control_url)"
    "DEIS_TELEMETRY_ENDPOINT=http://127.0.0.1:${DEIS_TELEMETRY_PORT}"
    "DEIS_APP=${APP_ID}"
    "DEIS_ENVIRONMENT=${ENV_NAME}"
    "DEIS_CONFIG_HOME=${DEMO_DIR}/deis-config"
  )
}

deis_cli() {
  deis_cli_env
  env "${DEIS_CLI_ENV[@]}" "${BIN_DIR}/deis" "$@"
}

# Lay down a copy of the example app's sources and echo the directory.
#
# The copy lives under the demo directory rather than in the repo because a
# build writes dist/ into the project it is given, and because the second
# version rewrites two of the source files. Nothing is compiled here: the
# build is `deis deploy`'s first step, so that a project which does not
# compile creates no app, no environment and no version.
prepare_app_source() {
  local label=$1
  local dest="${DEMO_DIR}/app-${label}"

  rm -rf "$dest"
  mkdir -p "$dest"
  cp -R "${APP_SRC_DIR}/." "$dest/"

  # Version 2 differs in two places, one on each side of the asset-first
  # split: the label a handler returns, and the static file itself. A
  # rollback has to bring both back.
  if [ "$label" != "v1" ]; then
    sed -i "s/\"BUILD_LABEL\": \"v1\"/\"BUILD_LABEL\": \"${label}\"/" "$dest/.config.json"
    sed -i "s/data-label=\"v1\"/data-label=\"${label}\"/" "$dest/public/index.html"
  fi

  echo "$dest"
}

# `deis deploy` from a prepared source tree, with the report kept for `verify`.
#
# The command is run from inside the project directory because that is where
# a developer runs it: `deis deploy` reads .config.json from the working
# directory, builds the bundle, creates the application and the environment
# if they are new, uploads the bundle as an immutable version and moves the
# environment's pointer to it. One command covers what used to be four REST
# calls here.
#
# --json is asked for so `verify` can check the report later rather than
# scrape human output. It is written under LOG_DIR on purpose: that is the
# tree check_secret_delivery sweeps for the plaintext secret, so a report
# that ever echoed one would fail that check instead of going unnoticed.
#
# A non-zero exit means at least one node did not take the deployment. That
# node is still serving the previous version, which is exactly the failure a
# green deploy must not hide, so it stops the run.
cli_deploy() {
  local dir=$1
  local environment=$2
  local report=$3
  shift 3

  local json="${LOG_DIR}/${report}.json"
  local errors="${LOG_DIR}/${report}.log"

  if ! (cd "$dir" && deis_cli deploy -e "$environment" --json "$@") \
    > "$json" 2> "$errors"; then
    echo "❌ deis deploy -e ${environment} failed; see ${errors}" >&2
    tail -n 20 "$errors" | sed 's/^/     /' >&2
    return 1
  fi
}

# Everything `verify` needs to know about a deploy report, read once.
report_field() {
  jq -r "$2 // empty" < "${LOG_DIR}/$1.json" 2>/dev/null || true
}

deploy_example_app() {
  if ! command -v jq >/dev/null 2>&1; then
    echo "⚠️  jq not found; the example app is not deployed and nothing can be verified"
    return 0
  fi

  echo "🚀 Deploying the example app with the deis CLI"

  local src_v1
  src_v1=$(prepare_app_source v1) || return 1

  # One command: build, create the application, create the environment,
  # upload the version, point production at it, and wait for the nodes to
  # say they took it.
  cli_deploy "$src_v1" "$ENV_NAME" cli-deploy-v1 || return 1
  APP_V1_ID=$(report_field cli-deploy-v1 '.version.id')
  APP_V1_NUMBER=$(report_field cli-deploy-v1 '.version.number')
  echo "   ✓ deis deploy: version ${APP_V1_NUMBER} (${APP_V1_ID}) is serving ${ENV_NAME} on $(report_field cli-deploy-v1 '.delivery.delivered | length') node(s)"

  # The value is piped in rather than passed as an argument, which is what
  # the CLI recommends and what keeps it out of the shell history and out of
  # every other user's `ps` output. A secret is not part of the immutable
  # bundle, so this produces no new version; the control plane re-delivers
  # the running one with the new environment attached, and `deis secret set`
  # fails if a node does not take that.
  if ! printf '%s' "$DEMO_SECRET_VALUE" \
    | deis_cli secret set DEMO_SECRET -e "$ENV_NAME" --json \
      > "${LOG_DIR}/cli-secret-set.json" 2> "${LOG_DIR}/cli-secret-set.log"; then
    echo "❌ deis secret set DEMO_SECRET failed; see ${LOG_DIR}/cli-secret-set.log" >&2
    tail -n 20 "${LOG_DIR}/cli-secret-set.log" | sed 's/^/     /' >&2
    return 1
  fi
  echo "   ✓ deis secret set: DEMO_SECRET stored and re-delivered to the running version"

  # A preview of the same application, on the same node, deployed to a
  # different version than production. Before an application was identified
  # by (app, environment), this second deployment overwrote the first on the
  # node -- same slot, so production silently started serving the preview's
  # code, the preview's environment map and the preview's secrets. `verify`
  # checks both halves of the fix: each environment serves its own version,
  # and neither can see the other's KV.
  #
  # It is also the second version production rolls back from later.
  local src_v2
  src_v2=$(prepare_app_source v2) || return 1
  cli_deploy "$src_v2" "$PREVIEW_ENV_NAME" cli-deploy-v2-preview || return 1
  APP_V2_ID=$(report_field cli-deploy-v2-preview '.version.id')
  APP_V2_NUMBER=$(report_field cli-deploy-v2-preview '.version.number')
  echo "   ✓ deis deploy -e ${PREVIEW_ENV_NAME}: version ${APP_V2_NUMBER} (${APP_V2_ID}) is serving ${PREVIEW_ENV_NAME}"

  echo "   Send requests with: curl -H 'x-deis-app-id: ${APP_ID}' http://127.0.0.1:${DEIS_RUNTIME_PORT}/"
}

start_demo() {
  ensure_dirs
  reset_previous_run
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
  local user_deis_store_port
  local user_deis_queue_port
  local user_deis_telemetry_port
  local user_deis_telemetry_metrics_port
  user_deis_control_port=${DEIS_CONTROL_PORT:-}
  user_deis_runtime_port=${DEIS_RUNTIME_PORT:-}
  user_deis_proxy_port=${DEIS_PROXY_PORT:-}
  user_deis_store_port=${DEIS_STORE_PORT:-}
  user_deis_queue_port=${DEIS_QUEUE_PORT:-}
  user_deis_telemetry_port=${DEIS_TELEMETRY_PORT:-}
  user_deis_telemetry_metrics_port=${DEIS_TELEMETRY_METRICS_PORT:-}

  if [ -f "$STATE_FILE" ]; then
    # shellcheck disable=SC1090
    source "$STATE_FILE"
    if pid_running "${DEIS_CONTROL_PID:-}" || pid_running "${DEIS_RUNTIME_PID:-}" \
      || pid_running "${DEIS_PROXY_PID:-}" || pid_running "${DEIS_STORE_PID:-}" \
      || pid_running "${DEIS_QUEUE_PID:-}" || pid_running "${DEIS_TELEMETRY_PID:-}"; then
      echo "⚠️  An existing instance is running, please run stop first"
      exit 1
    fi
  fi

  # The state file is how `stop` finds the processes, so deleting the demo
  # directory without stopping first orphans a whole run: its components keep
  # their ports, and the new control plane would either fail to bind or, worse,
  # the probe would find the old one and every later step would address it.
  # Look for the processes themselves rather than trusting the file.
  local orphans
  orphans=$(pgrep -f "${BIN_DIR}/deis-(control|runtime|proxy|store|queue|telemetry)" 2>/dev/null || true)
  if [ -n "$orphans" ]; then
    echo "⚠️  Components from an earlier run are still running (pids: $(echo "$orphans" | tr '\n' ' '))."
    echo "    They hold their ports and their data. Stop them first:"
    echo "      ./scripts/demo.sh stop      # if .demo/state.env still exists"
    echo "      pkill -f '${BIN_DIR}/deis-'  # otherwise"
    exit 1
  fi

  DEIS_CONTROL_PORT=$user_deis_control_port
  DEIS_RUNTIME_PORT=$user_deis_runtime_port
  DEIS_PROXY_PORT=$user_deis_proxy_port
  DEIS_STORE_PORT=$user_deis_store_port
  DEIS_QUEUE_PORT=$user_deis_queue_port
  DEIS_TELEMETRY_PORT=$user_deis_telemetry_port
  DEIS_TELEMETRY_METRICS_PORT=$user_deis_telemetry_metrics_port

  claim_port DEIS_RUNTIME_PORT "${DEIS_RUNTIME_PORT:-$DEFAULT_DEIS_RUNTIME_PORT}"
  claim_port DEIS_PROXY_PORT "${DEIS_PROXY_PORT:-$DEFAULT_DEIS_PROXY_PORT}"
  claim_port DEIS_STORE_PORT "${DEIS_STORE_PORT:-$DEFAULT_DEIS_STORE_PORT}"
  claim_port DEIS_QUEUE_PORT "${DEIS_QUEUE_PORT:-$DEFAULT_DEIS_QUEUE_PORT}"
  claim_port DEIS_TELEMETRY_PORT "${DEIS_TELEMETRY_PORT:-$DEFAULT_DEIS_TELEMETRY_PORT}"
  claim_port DEIS_TELEMETRY_METRICS_PORT \
    "${DEIS_TELEMETRY_METRICS_PORT:-$DEFAULT_DEIS_TELEMETRY_METRICS_PORT}"

  SECRET=${SECRET:-$(generate_secret)}

  # The master key that encrypts environment secrets. 32 bytes as 64 hex
  # characters, which is what `openssl rand -hex 32` produces. It never enters
  # the database and is never logged; without it every secret endpoint answers
  # 503 instead of storing anything in the clear.
  SECRET_KEY_FILE="${DEMO_DIR}/secret.key"
  if [ ! -f "$SECRET_KEY_FILE" ]; then
    generate_secret > "$SECRET_KEY_FILE"
    chmod 600 "$SECRET_KEY_FILE"
  fi

  DEIS_CONTROL_CONFIG="${DEMO_DIR}/deis-control.toml"
  DEIS_RUNTIME_CONFIG="${DEMO_DIR}/deis-runtime.toml"
  DEIS_PROXY_CONFIG="${DEMO_DIR}/deis-proxy.toml"
  DEIS_STORE_CONFIG="${DEMO_DIR}/deis-store.toml"
  DEIS_QUEUE_CONFIG="${DEMO_DIR}/deis-queue.toml"
  DEIS_TELEMETRY_CONFIG="${DEMO_DIR}/deis-telemetry.toml"

  build_binaries
  start_data_plane

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

    # bundle_dir is where an uploaded version's zip lives and where nodes
    # fetch it from; public_base_url is the address those download links
    # carry, which has to be reachable from the node rather than from the
    # control plane's own point of view. secret_key_file is what makes the
    # secret endpoints work at all: without it they answer 503 rather than
    # storing anything in the clear. store_endpoint / queue_endpoint are the
    # addresses handed down to nodes with every deployment - a node never
    # learns where storage lives from its own file.
    cat > "$DEIS_CONTROL_CONFIG" <<EOF
bind_addr = "0.0.0.0"
port = ${DEIS_CONTROL_PORT}
heartbeat_interval = 30
heartbeat_timeout = 90
node_preshared_keys = ["${SECRET}"]
deis_runtime_poll_interval = 5
bundle_dir = "${DEMO_DIR}/bundles"
public_base_url = "http://127.0.0.1:${DEIS_CONTROL_PORT}"
secret_key_file = "${SECRET_KEY_FILE}"
store_endpoint = "http://127.0.0.1:${DEIS_STORE_PORT}"
queue_endpoint = "http://127.0.0.1:${DEIS_QUEUE_PORT}"

[[deis_runtime_nodes]]
id = "${NODE_ID}"
addr = "http://127.0.0.1:${DEIS_RUNTIME_PORT}"
EOF

    # [control] is architecture principle 8: on startup the node asks the
    # control plane which applications and versions it should be running,
    # downloads each bundle itself, and only reports healthy once they are
    # loaded. The control plane still pushes new deployments at it; the pull
    # is what makes a restarted node come back with what it had.
    #
    # `secret` is the same value as the control plane's node_preshared_keys.
    # The assignment list carries every environment's decrypted secrets, so the
    # control plane serves it - and the bundle download - only to a caller that
    # presents a node key. Without this line the node gets 401 and never loads
    # anything it was assigned.
    #
    # [logging] is what makes the app's own log lines queryable: without it
    # they stop at this node's stdout and deis-telemetry has nothing to answer
    # with. The flush interval is dropped to a second so a request's logs are
    # queryable while the person who made it is still looking.
    cat > "$DEIS_RUNTIME_CONFIG" <<EOF
node_id = "${NODE_ID}"
http_port = ${DEIS_RUNTIME_PORT}
data_dir = "${DEMO_DIR}/deis-runtime-data"

[logging]
enabled = true
endpoint = "http://127.0.0.1:${DEIS_TELEMETRY_PORT}"
flush_interval_secs = 1

[control]
endpoint = "http://127.0.0.1:${DEIS_CONTROL_PORT}"
secret = "${SECRET}"
retry_interval_secs = 2
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

[control]
addr = "http://127.0.0.1:${DEIS_CONTROL_PORT}"
associated_id = "deis-proxy-demo-01"
secret = "${SECRET}"
EOF

    echo "🚀 Starting Control (HTTP/gRPC: ${DEIS_CONTROL_PORT})"
    "${BIN_DIR}/deis-control" \
      --config "$DEIS_CONTROL_CONFIG" \
      --port "$DEIS_CONTROL_PORT" \
      --database "${DEMO_DIR}/deis-control.db" \
      > "${LOG_DIR}/deis-control.log" 2>&1 &
    DEIS_CONTROL_PID=$!

    # The pid check is the load-bearing half. A port already held by another
    # control plane -- an orphan from a previous run, say -- answers /health
    # perfectly well, so a probe on its own would report "ready" while our own
    # process had already died on bind, and every later step would silently
    # talk to the wrong instance.
    if wait_for_http "http://127.0.0.1:${DEIS_CONTROL_PORT}/health" 200 \
      && pid_running "$DEIS_CONTROL_PID"; then
      deis_control_ready=1
      break
    fi

    if grep -q "Address already in use" "${LOG_DIR}/deis-control.log"; then
      echo "⚠️  Port ${DEIS_CONTROL_PORT} is already in use, trying the next port"
      kill "$DEIS_CONTROL_PID" || true
      attempt=$((attempt + 1))
      continue
    fi

    if ! pid_running "$DEIS_CONTROL_PID"; then
      echo "❌ Control exited during startup; nothing else can be deployed. Last lines:"
      tail -n 10 "${LOG_DIR}/deis-control.log" | sed 's/^/     /'
      exit 1
    fi

    echo "❌ Control API is not ready, please check the log: ${LOG_DIR}/deis-control.log"
    exit 1
  done

  if [ $deis_control_ready -ne 1 ]; then
    echo "❌ Could not find an available port to start Control"
    exit 1
  fi

  echo "🚀 Starting Runtime (HTTP: ${DEIS_RUNTIME_PORT})"
  DEIS_RUNTIME_CONFIG="$DEIS_RUNTIME_CONFIG" \
    DEIS_RUNTIME_HTTP_PORT="$DEIS_RUNTIME_PORT" \
    "${BIN_DIR}/deis-runtime" \
    > "${LOG_DIR}/deis-runtime.log" 2>&1 &
  DEIS_RUNTIME_PID=$!

  local runtime_rc
  runtime_rc=0
  wait_for_service wait_for_http_any "http://127.0.0.1:${DEIS_RUNTIME_PORT}/health" \
    "$DEIS_RUNTIME_PID" 60 || runtime_rc=$?
  report_service "Runtime" "$runtime_rc" "${LOG_DIR}/deis-runtime.log" \
    "no application can serve a request"

  echo "🚀 Starting Proxy (HTTP: ${DEIS_PROXY_PORT})"
  "${BIN_DIR}/deis-proxy" -c "$DEIS_PROXY_CONFIG" \
    > "${LOG_DIR}/deis-proxy.log" 2>&1 &
  DEIS_PROXY_PID=$!

  local proxy_rc
  proxy_rc=0
  wait_for_service wait_for_http_any "http://127.0.0.1:${DEIS_PROXY_PORT}/" \
    "$DEIS_PROXY_PID" 60 || proxy_rc=$?
  report_service "Proxy" "$proxy_rc" "${LOG_DIR}/deis-proxy.log" \
    "the demo entry point is unreachable"

  deploy_example_app

  write_state

  echo "✅ Demo environment started"
  echo "- Control API: http://127.0.0.1:${DEIS_CONTROL_PORT}"
  echo "- Runtime: http://127.0.0.1:${DEIS_RUNTIME_PORT}"
  echo "- Proxy: http://127.0.0.1:${DEIS_PROXY_PORT}"
  echo "- Store (gRPC): 127.0.0.1:${DEIS_STORE_PORT}"
  echo "- Queue (gRPC): 127.0.0.1:${DEIS_QUEUE_PORT}"
  echo "- Telemetry (gRPC): 127.0.0.1:${DEIS_TELEMETRY_PORT}"
  echo "- Telemetry metrics: http://127.0.0.1:${DEIS_TELEMETRY_METRICS_PORT}/metrics"
}

# How long one component may take to drain before it is killed outright.
#
# It has to exceed each component's own drain window -- deis-control gives
# in-flight requests 15s, deis-runtime defaults to 30 -- or a normal, healthy
# shutdown would end in a SIGKILL every time.
STOP_GRACE_SECONDS=45

# Block until a process that is not our child has exited.
#
# `tail --pid` sleeps on the process itself and returns the moment it is gone,
# so this waits on the actual event instead of guessing at a duration; it also
# returns immediately for a pid that has already exited. Returns non-zero if
# the wait timed out, or if this system has no `tail --pid` to wait with -- in
# which case the caller reports that it could not confirm the exit rather than
# claiming one.
wait_for_exit() {
  local pid=$1
  local seconds=$2
  timeout "$seconds" tail --pid="$pid" -f /dev/null >/dev/null 2>&1
}

# Ask one component to stop, then wait until it really has.
#
# SIGTERM is a request, not an instant stop: every component drains in-flight
# work first. Returning before the process has gone would leave its port held,
# so the next `start` would fail to bind against a service the script had just
# reported as stopped. Returns non-zero if the process is still alive at the
# end, having said what that costs.
stop_service() {
  local name=$1
  local pid=$2

  if ! pid_running "$pid"; then
    return 0
  fi

  echo "🛑 Stopping ${name} (PID: ${pid})"
  kill "$pid" 2>/dev/null || true

  wait_for_exit "$pid" "$STOP_GRACE_SECONDS" || true
  if ! pid_running "$pid"; then
    return 0
  fi

  echo "⚠️  ${name} did not exit within ${STOP_GRACE_SECONDS}s; killing it. Whatever it was still draining is cut off."
  kill -9 "$pid" 2>/dev/null || true

  wait_for_exit "$pid" 5 || true
  if ! pid_running "$pid"; then
    return 0
  fi

  echo "❌ ${name} (PID: ${pid}) survived SIGKILL; port ${name} was using stays held"
  return 1
}

# SIGTERM, not SIGINT. Bash sets SIGINT to "ignored" for every command it
# starts asynchronously, and that disposition survives exec, so a service that
# does not install its own SIGINT handler never sees the signal at all and
# keeps running with its port held. SIGTERM is not ignored, and deis-store,
# deis-queue, deis-telemetry and deis-proxy all drain on it.
#
# Every component installs its own SIGTERM handler and drains in-flight work,
# so the signal is a request to finish, not an instant stop. stop_demo waits
# for each process to actually go away rather than assuming the signal landed.
stop_demo() {
  load_state

  local failed=0

  stop_service "Runtime" "${DEIS_RUNTIME_PID:-}" || failed=1
  stop_service "Proxy" "${DEIS_PROXY_PID:-}" || failed=1
  stop_service "Control" "${DEIS_CONTROL_PID:-}" || failed=1

  # The data plane goes last: the components above may still be flushing logs
  # to deis-telemetry or finishing storage calls while they shut down.
  stop_service "Queue" "${DEIS_QUEUE_PID:-}" || failed=1
  stop_service "Store" "${DEIS_STORE_PID:-}" || failed=1
  stop_service "Telemetry" "${DEIS_TELEMETRY_PID:-}" || failed=1

  if [ "$failed" -ne 0 ]; then
    echo "❌ Some components are still running; their ports stay held and the next start will fail to bind"
    return 1
  fi

  echo "✅ Demo environment stopped; every component has exited"
}

# ----------------------------------------------------------------------
# Verification
# ----------------------------------------------------------------------
#
# Each check exercises one thing the platform promises and prints the evidence
# it is judging, so a failure says what was actually seen rather than only
# that something went wrong.

CHECKS_PASSED=0
CHECKS_FAILED=0

pass_check() {
  CHECKS_PASSED=$((CHECKS_PASSED + 1))
  echo "   ✓ $1"
  [ -n "${2:-}" ] && echo "     $2"
  return 0
}

fail_check() {
  CHECKS_FAILED=$((CHECKS_FAILED + 1))
  echo "   ✗ $1"
  [ -n "${2:-}" ] && echo "     $2"
  return 0
}

# One request to the node, with the tenant header deis-proxy would have
# stamped. Writes the response headers to $1 and the body to stdout.
app_request() {
  local header_file=$1
  local path=$2
  shift 2
  curl -s -D "$header_file" -H "x-deis-app-id: ${APP_ID}" "$@" \
    "http://127.0.0.1:${DEIS_RUNTIME_PORT}${path}"
}

# The same, addressed to a named environment of the application. No header
# means production, so production keeps working for a caller that predates
# environments; naming one is how a preview is reached.
env_request() {
  local environment=$1
  local header_file=$2
  local path=$3
  shift 3
  curl -s -D "$header_file" \
    -H "x-deis-app-id: ${APP_ID}" -H "x-deis-environment: ${environment}" "$@" \
    "http://127.0.0.1:${DEIS_RUNTIME_PORT}${path}"
}

# Read one header value out of a saved response, lower-casing the name.
header_value() {
  local file=$1
  local name=$2
  tr -d '\r' < "$file" | awk -v want="$(echo "$name" | tr 'A-Z' 'a-z')" '
    { split($0, kv, ":"); k = tolower(kv[1]);
      if (k == want) { sub(/^[^:]*:[ ]*/, ""); print; exit } }'
}

http_status() {
  head -n 1 "$1" | tr -d '\r' | awk '{print $2}'
}

check_static_asset() {
  local headers="${LOG_DIR}/verify-asset.headers"
  local body
  body=$(app_request "$headers" "/")

  local status etag ctype
  status=$(http_status "$headers")
  etag=$(header_value "$headers" ETag)
  ctype=$(header_value "$headers" Content-Type)

  if [ "$status" != "200" ] || ! echo "$body" | grep -q "static asset"; then
    fail_check "static asset served from the bundle" "GET / answered ${status}"
    return
  fi
  if [ -z "$etag" ]; then
    fail_check "static asset carries an ETag" "no ETag header on GET /"
    return
  fi
  pass_check "static asset served from the bundle" \
    "HTTP ${status}, content-type ${ctype}, ETag ${etag}; the body is the packaged file, not a handler's answer"

  local code
  code=$(curl -s -o /dev/null -w '%{http_code}' \
    -H "x-deis-app-id: ${APP_ID}" -H "If-None-Match: ${etag}" \
    "http://127.0.0.1:${DEIS_RUNTIME_PORT}/")
  local length
  length=$(curl -s -o /dev/null -w '%{size_download}' \
    -H "x-deis-app-id: ${APP_ID}" -H "If-None-Match: ${etag}" \
    "http://127.0.0.1:${DEIS_RUNTIME_PORT}/")

  if [ "$code" = "304" ] && [ "$length" = "0" ]; then
    pass_check "If-None-Match on that ETag answers 304 with no body" \
      "HTTP 304, ${length} bytes"
  else
    fail_check "If-None-Match on that ETag answers 304 with no body" \
      "got HTTP ${code} with ${length} bytes"
  fi
}

check_route_parameter() {
  local headers="${LOG_DIR}/verify-route.headers"
  local body
  body=$(app_request "$headers" "/hello/world")

  local greeting
  greeting=$(echo "$body" | jq -r '.greeting // empty' 2>/dev/null || true)
  if [ "$greeting" = "Hello, world!" ]; then
    pass_check "a route placeholder reaches context.params" \
      "GET /hello/world -> ${body}"
  else
    fail_check "a route placeholder reaches context.params" \
      "GET /hello/world -> $(http_status "$headers") ${body}"
  fi
}

check_secret_delivery() {
  local headers="${LOG_DIR}/verify-secret.headers"
  local body
  body=$(app_request "$headers" "/hello/world")

  if [ "$(echo "$body" | jq -r '.secretPresent // false' || true)" = "true" ]; then
    pass_check "the environment's secret reached the isolate" \
      "the handler sees DEMO_SECRET; its value is never in the response"
  else
    fail_check "the environment's secret reached the isolate" \
      "the handler reports it missing: ${body}"
  fi

  # The value is decrypted only in memory, at the moment of delivery. Nothing
  # on either side's disk may contain it - and that includes the CLI's own
  # --json reports, which are written under LOG_DIR precisely so this sweep
  # covers them. `deis secret set` and `deis secret list` both answer with a
  # document about the secret; one of them printing the value back is exactly
  # the leak this check exists to catch, so no path under LOG_DIR is
  # excluded.
  # `|| true` at the end: this whole pipeline finding nothing is the passing
  # case, and under `set -euo pipefail` a grep that matches nothing would
  # end the run instead.
  #
  # Every component's data directory is swept, not just the node's: the
  # control plane stores the secret encrypted, and the storage services must
  # never have seen it at all, so a plaintext hit in any of them is a leak.
  local hits
  hits=$(grep -rl "$DEMO_SECRET_VALUE" "$LOG_DIR" \
    "${DEMO_DIR}/deis-runtime-data" "${DEMO_DIR}/deis-store-data" \
    "${DEMO_DIR}/deis-queue-data" "${DEMO_DIR}/deis-telemetry-data" \
    "${DEMO_DIR}/bundles" "${DEMO_DIR}/deis-control.db" "${DEMO_DIR}/deis-proxy.db" \
    2>/dev/null | head -n 5 || true)
  if [ -z "$hits" ]; then
    pass_check "the decrypted secret is written nowhere" \
      "no hit in any component's data directory, the bundle store, either database or any log"
  else
    fail_check "the decrypted secret is written nowhere" "found in: ${hits}"
  fi
}

check_kv_roundtrip() {
  local title="context.services.kv round-trips through deis-store"
  local key="demo-kv-$$"
  local value="stored-through-deis-store"
  local headers="${LOG_DIR}/verify-kv.headers"

  local put
  put=$(curl -s -D "$headers" -X PUT -H "x-deis-app-id: ${APP_ID}" \
    --data-binary "$value" \
    "http://127.0.0.1:${DEIS_RUNTIME_PORT}/api/kv/${key}")
  if [ "$(http_status "$headers")" != "201" ]; then
    fail_check "$title" "PUT answered $(http_status "$headers"): ${put}"
    return
  fi

  local got
  got=$(app_request "${LOG_DIR}/verify-kv-get.headers" "/api/kv/${key}")
  if [ "$(echo "$got" | jq -r '.value // empty' || true)" != "$value" ]; then
    fail_check "$title" \
      "GET answered $(http_status "${LOG_DIR}/verify-kv-get.headers"): ${got}"
    return
  fi

  # Reading it back through the node only proves the isolate received an
  # answer, not where the answer came from - a cache inside the node would
  # look identical from here. What proves deis-store itself holds the row is
  # asking deis-store's own KvStore.Get directly, over the same gRPC service
  # the node calls and with the same tenant header a request for this
  # application and environment carries (deis_wire::conventions::tenant_id).
  #
  # A grep of deis-store's sqlite file at rest was tried here before and
  # dropped: that file runs in WAL mode, a just-written row can sit in the
  # -wal file for a while, and a grep of the base file alone would then find
  # nothing and prove nothing either way about whether the write happened -
  # which is not evidence, in either direction. Asking the service is
  # deterministic instead of racing its checkpoint.
  if ! command -v grpcurl >/dev/null 2>&1; then
    fail_check "$title" \
      "PUT then GET returned ${value} from the node, but grpcurl is not installed so deis-store could not be asked directly whether it holds the row"
    return
  fi

  local tenant="${APP_ID}.${ENV_NAME}"
  local direct direct_value
  direct=$(grpcurl -plaintext -max-time 5 \
    -import-path "${ROOT_DIR}/crates/deis-store/proto" -proto storage.proto \
    -H "x-deis-tenant: ${tenant}" \
    -d "{\"key\":\"${key}\"}" \
    "127.0.0.1:${DEIS_STORE_PORT}" deis.storage.KvStore/Get 2>&1)
  direct_value=$(echo "$direct" | jq -r '.value // empty' 2>/dev/null || true)

  if [ "$direct_value" = "$value" ]; then
    pass_check "$title" \
      "PUT then GET returned ${value} from the node, and deis-store's own KvStore.Get answers the same value directly for tenant ${tenant}, key ${key}"
  else
    fail_check "$title" \
      "the node answered ${value}, but deis-store's own KvStore.Get for tenant ${tenant}, key ${key} returned: $(echo "$direct" | tr '\n' ' ' | head -c 200)"
  fi
}

check_queue_roundtrip() {
  local key="job-$$"
  local value="handled-by-the-queue-consumer"
  local headers="${LOG_DIR}/verify-queue.headers"

  local sent
  sent=$(curl -s -D "$headers" -X POST -H "x-deis-app-id: ${APP_ID}" \
    -H 'content-type: application/json' \
    -d "{\"key\":\"${key}\",\"value\":\"${value}\"}" \
    "http://127.0.0.1:${DEIS_RUNTIME_PORT}/api/enqueue")
  if [ "$(http_status "$headers")" != "202" ]; then
    fail_check "a queue message is produced and consumed" \
      "POST /api/enqueue answered $(http_status "$headers"): ${sent}"
    return
  fi

  # The consumer is a poller: it leases the batch on its own schedule, so the
  # only thing to do is wait for the effect it has. curl paces its own
  # retries a second apart and gives up after the budget below.
  local got
  got=$(curl -sf --retry 45 --retry-delay 1 --retry-max-time 90 --retry-all-errors \
    -H "x-deis-app-id: ${APP_ID}" \
    "http://127.0.0.1:${DEIS_RUNTIME_PORT}/api/kv/job:${key}" 2>/dev/null || echo '')

  if [ "$(echo "$got" | jq -r '.value // empty' 2>/dev/null || true)" = "$value" ]; then
    pass_check "a queue message is produced and consumed" \
      "the handler sent it to deis-queue, the node's poller leased it, and the consumer wrote job:${key}"
  else
    fail_check "a queue message is produced and consumed" \
      "nothing was written for job:${key} within 90s; last answer: ${got:-<none>}"
  fi
}

check_cron() {
  # The whole cron path in one observation: the expression rode into the
  # control plane inside the bundle manifest, the scheduler rebuilt its task
  # list from the catalog, it picked this node and called Trigger/OnScheduled,
  # and the app's `scheduled` export wrote the key below.
  #
  # The expression is "every minute" and the demo sets the scheduler's tick to
  # five seconds, so the wait is a minute plus a little rather than the three
  # minutes the default tick would need. curl paces its own retries a second
  # apart.
  local got
  got=$(curl -sf --retry 150 --retry-delay 1 --retry-max-time 180 --retry-all-errors \
    -H "x-deis-app-id: ${APP_ID}" \
    "http://127.0.0.1:${DEIS_RUNTIME_PORT}/api/kv/cron:last-run" 2>/dev/null || echo '')

  local at
  at=$(echo "$got" | jq -r '.value // empty' 2>/dev/null || true)
  if [ -n "$at" ]; then
    pass_check "a cron expression fires on one node and runs the app's handler"       "deis-control triggered the node and the scheduled handler recorded ${at}"
  else
    fail_check "a cron expression fires on one node and runs the app's handler"       "nothing was recorded for cron:last-run within 120s; last answer: ${got:-<none>}"
  fi
}

# Both environments of the application answer at once, each on its own
# version.
#
# Production is on v1 and the preview on v2. When an application was keyed by
# app id alone, the node had one slot for both: whichever arrived last won,
# and the label below came back the same from both. The header is the only
# difference between the two requests.
check_second_environment() {
  local prod preview
  prod=$(app_request "${LOG_DIR}/verify-env-production.headers" "/hello/world")
  preview=$(env_request "$PREVIEW_ENV_NAME" \
    "${LOG_DIR}/verify-env-preview.headers" "/hello/world")

  local prod_label preview_label
  prod_label=$(echo "$prod" | jq -r '.label // empty' 2>/dev/null || true)
  preview_label=$(echo "$preview" | jq -r '.label // empty' 2>/dev/null || true)

  if [ "$prod_label" = "v1" ] && [ "$preview_label" = "v2" ]; then
    pass_check "two environments of one application serve at once, each its own version" \
      "no environment header -> ${prod_label}; x-deis-environment: ${PREVIEW_ENV_NAME} -> ${preview_label}"
  else
    fail_check "two environments of one application serve at once, each its own version" \
      "expected v1 from ${ENV_NAME} and v2 from ${PREVIEW_ENV_NAME}, got ${prod_label:-<none>} and ${preview_label:-<none>} (${prod} / ${preview})"
  fi
}

# The storage tenant is (app, environment), so what a preview writes through
# ctx.services.kv is invisible to production and the other way round.
#
# This is the half that cannot be undone if it is wrong: a preview build that
# overwrites a production key cannot be rolled back, because rolling back
# changes which code runs, not what the code already wrote.
check_environment_kv_isolation() {
  local shared="demo-env-shared-$$"
  local preview_only="demo-env-preview-only-$$"
  local headers="${LOG_DIR}/verify-env-kv.headers"

  env_request "$ENV_NAME" "$headers" "/api/kv/${shared}" \
    -X PUT --data-binary "written-by-production" > /dev/null
  if [ "$(http_status "$headers")" != "201" ]; then
    fail_check "a preview environment cannot see production's KV, or the reverse" \
      "production could not write ${shared}: HTTP $(http_status "$headers")"
    return
  fi

  env_request "$PREVIEW_ENV_NAME" "$headers" "/api/kv/${shared}" \
    -X PUT --data-binary "written-by-preview" > /dev/null
  if [ "$(http_status "$headers")" != "201" ]; then
    fail_check "a preview environment cannot see production's KV, or the reverse" \
      "the preview could not write ${shared}: HTTP $(http_status "$headers")"
    return
  fi

  env_request "$PREVIEW_ENV_NAME" "$headers" "/api/kv/${preview_only}" \
    -X PUT --data-binary "preview-only" > /dev/null

  local from_production from_preview absent_status
  from_production=$(env_request "$ENV_NAME" "$headers" "/api/kv/${shared}" \
    | jq -r '.value // empty' 2>/dev/null || true)
  from_preview=$(env_request "$PREVIEW_ENV_NAME" "$headers" "/api/kv/${shared}" \
    | jq -r '.value // empty' 2>/dev/null || true)
  env_request "$ENV_NAME" "$headers" "/api/kv/${preview_only}" > /dev/null
  absent_status=$(http_status "$headers")

  if [ "$from_production" = "written-by-production" ] \
    && [ "$from_preview" = "written-by-preview" ] \
    && [ "$absent_status" = "404" ]; then
    pass_check "a preview environment cannot see production's KV, or the reverse" \
      "both wrote ${shared}: production reads back \"${from_production}\", the preview \"${from_preview}\"; ${preview_only}, written only by the preview, is 404 in production"
  else
    fail_check "a preview environment cannot see production's KV, or the reverse" \
      "production read \"${from_production:-<none>}\", the preview read \"${from_preview:-<none>}\", and the preview-only key answered ${absent_status} in production instead of 404"
  fi
}

# Which version an environment is on right now, straight from the control
# plane rather than inferred from a response.
# Which version an environment is running, according to the control plane,
# read through the CLI rather than the REST API.
deployed_version() {
  local environment=${1:-$ENV_NAME}
  deis_cli status --json 2>/dev/null \
    | jq -r --arg env "$environment" \
      '.environments[] | select(.name == $env) | .version_id // empty' \
    2>/dev/null || true
}

# The static label the served asset carries, which rides inside the version
# bundle rather than being produced by a handler.
asset_label() {
  curl -s -H "x-deis-app-id: ${APP_ID}" "http://127.0.0.1:${DEIS_RUNTIME_PORT}/" \
    | grep -o 'data-label="[^"]*"' || true
}

check_rollback() {
  if [ -z "${APP_V2_ID:-}" ]; then
    fail_check "deis rollback puts the previous version back" \
      "no second version was uploaded, so there is nothing to roll back from"
    return
  fi

  local before label_before asset_before
  before=$(app_request "${LOG_DIR}/verify-rollback-before.headers" "/hello/world")
  label_before=$(echo "$before" | jq -r '.label // empty' || true)
  asset_before=$(asset_label)

  # Deploy the preview's version to production, from the same source tree
  # the preview was built from. --no-build reuses the bundle that build left
  # behind, so this is the identical artefact rather than a rebuild that
  # merely ought to match.
  if ! cli_deploy "${DEMO_DIR}/app-v2" "$ENV_NAME" cli-rollback-v2 --no-build; then
    fail_check "deis rollback puts the previous version back" \
      "deis deploy could not put v2 on ${ENV_NAME}; see ${LOG_DIR}/cli-rollback-v2.log"
    return
  fi

  local after label_after asset_v2
  after=$(app_request "${LOG_DIR}/verify-rollback-v2.headers" "/hello/world")
  label_after=$(echo "$after" | jq -r '.label // empty' || true)
  asset_v2=$(asset_label)

  if [ "$label_after" = "$label_before" ]; then
    fail_check "deis rollback puts the previous version back" \
      "deploying v2 did not change the response: still ${label_after}"
    return
  fi

  # No --to: the point of the command is that a person under pressure does
  # not have to know a version number to undo the last deployment.
  if ! deis_cli rollback -e "$ENV_NAME" --json \
    > "${LOG_DIR}/cli-rollback.json" 2> "${LOG_DIR}/cli-rollback.log"; then
    fail_check "deis rollback puts the previous version back" \
      "the rollback command failed: $(tail -n 3 "${LOG_DIR}/cli-rollback.log" | tr '\n' ' ')"
    return
  fi

  local rolled_from rolled_to rolled_to_number
  rolled_from=$(jq -r '.from.id // empty' < "${LOG_DIR}/cli-rollback.json" || true)
  rolled_to=$(jq -r '.to.id // empty' < "${LOG_DIR}/cli-rollback.json" || true)
  rolled_to_number=$(jq -r '.to.number // empty' < "${LOG_DIR}/cli-rollback.json" || true)

  local back label_back asset_back
  back=$(app_request "${LOG_DIR}/verify-rollback-after.headers" "/hello/world")
  label_back=$(echo "$back" | jq -r '.label // empty' || true)
  asset_back=$(asset_label)

  # Assets ride inside the version bundle (see "asset-first serving" in
  # implementation-plan.zh.md, principle 5), so a rollback that put the code
  # back without the asset is exactly the bug this check exists to catch:
  # deploying v2 must have changed the served asset, and rolling back must
  # have restored the original one.
  if [ "$label_back" = "$label_before" ] && [ "$rolled_to" = "$APP_V1_ID" ] \
    && [ "$rolled_from" = "$APP_V2_ID" ] \
    && [ -n "$asset_v2" ] && [ "$asset_v2" != "$asset_before" ] \
    && [ "$asset_back" = "$asset_before" ]; then
    pass_check "deis rollback puts the previous version back" \
      "deis rollback moved ${ENV_NAME} to version ${rolled_to_number}; the handler answered ${label_before} -> ${label_after} -> ${label_back} and the static asset went ${asset_before} -> ${asset_v2} -> ${asset_back} with it"
  else
    fail_check "deis rollback puts the previous version back" \
      "expected label ${label_before} and asset ${asset_before} from ${APP_V1_ID}, got label ${label_back} and asset ${asset_back} from ${rolled_to} (v2 served asset ${asset_v2})"
  fi
}

check_telemetry_logs() {
  if ! command -v grpcurl >/dev/null 2>&1; then
    fail_check "the app's logs are queryable from deis-telemetry" \
      "grpcurl is not installed, so this check could not be run"
    return
  fi

  # Ask for the line the handler itself wrote, not just for "anything from
  # this app": component logs would pass that while the application's own
  # console output never left the node.
  #
  # The query names the application, not "demo.production". A node runs
  # several environments of one application and charges their storage to
  # separate tenants, but deis-telemetry keeps one application dimension and
  # carries the environment as a field, so asking for an application answers
  # with all of its environments - which is the question anybody looking at
  # logs is asking.
  local out
  out=$(grpcurl -plaintext -max-time 10 \
    -import-path "${ROOT_DIR}/crates/deis-telemetry/proto" \
    -proto telemetry.proto \
    -d "{\"app_id\":\"${APP_ID}\",\"contains\":\"greeted\",\"limit\":20}" \
    "127.0.0.1:${DEIS_TELEMETRY_PORT}" deis.telemetry.LogService/Query 2>&1)

  local count sample environment
  count=$(echo "$out" | jq -r '.entries | length' 2>/dev/null || echo 0)
  sample=$(echo "$out" | jq -r '.entries[0].message // empty' 2>/dev/null || true)
  environment=$(echo "$out" | jq -r '.entries[0].fields.environment // empty' 2>/dev/null || true)

  if [ "${count:-0}" -gt 0 ] 2>/dev/null && [ -n "$environment" ]; then
    pass_check "the app's logs are queryable from deis-telemetry" \
      "Query(app_id=${APP_ID}, contains=greeted) returned ${count} entries; newest: \"${sample}\" from environment ${environment}"
  elif [ "${count:-0}" -gt 0 ] 2>/dev/null; then
    fail_check "the app's logs are queryable from deis-telemetry" \
      "${count} entries came back but none says which environment produced it, so two environments' logs are indistinguishable"
  else
    fail_check "the app's logs are queryable from deis-telemetry" \
      "$(echo "$out" | head -n 3 | tr '\n' ' ')"
  fi
}

# --- what the CLI itself proves -------------------------------------------
#
# The checks below are about `deis` rather than about the platform. They
# exist because the CLI is the only interface a developer has: the control
# plane can be perfectly correct while the one command anybody types is
# broken, and nothing else in this file would notice.

check_cli_deploy() {
  local report="${LOG_DIR}/cli-deploy-v1.json"
  local title="deis deploy takes a project from source to serving"

  if [ ! -s "$report" ]; then
    fail_check "$title" \
      "no deploy report at ${report}, so start did not go through the CLI"
    return
  fi

  local app_created env_created version_created number sha delivered nodes
  app_created=$(jq -r '.app_created' < "$report" 2>/dev/null || true)
  env_created=$(jq -r '.environment_created' < "$report" 2>/dev/null || true)
  version_created=$(jq -r '.version_created' < "$report" 2>/dev/null || true)
  number=$(jq -r '.version.number // empty' < "$report" 2>/dev/null || true)
  sha=$(jq -r '.version.sha256 // empty' < "$report" 2>/dev/null | cut -c1-12 || true)
  delivered=$(jq -r '.delivery.delivered | length' < "$report" 2>/dev/null || echo 0)
  nodes=$(jq -r '.delivery.delivered | join(", ")' < "$report" 2>/dev/null || true)

  # The report says what the CLI did; the request says the result is real.
  # Both halves are needed: a deploy that recorded a version nothing serves
  # is the failure a report on its own would hide.
  local headers="${LOG_DIR}/verify-cli-deploy.headers"
  local body status label
  body=$(app_request "$headers" "/hello/world")
  status=$(http_status "$headers")
  label=$(echo "$body" | jq -r '.label // empty' 2>/dev/null || true)

  if [ "$app_created" = "true" ] && [ "$env_created" = "true" ] \
    && [ "$version_created" = "true" ] && [ "${delivered:-0}" -ge 1 ] \
    && [ "$status" = "200" ] && [ "$label" = "v1" ]; then
    pass_check "$title" \
      "one \`deis deploy\` in the project directory built the bundle, created application ${APP_ID}, created environment ${ENV_NAME}, uploaded version ${number} (sha256 ${sha}) and ${delivered} node(s) took it (${nodes}); GET /hello/world now answers ${status} from ${label}. No REST call was made by hand"
  else
    fail_check "$title" \
      "report says app_created=${app_created} environment_created=${env_created} version_created=${version_created} delivered=${delivered}; the app answers ${status} from label '${label}' (expected 200 from v1)"
  fi
}

check_cli_versions() {
  local out="${LOG_DIR}/cli-versions.json"
  local title="deis versions lists what was deployed and marks the live one"

  if ! deis_cli versions --json > "$out" 2> "${LOG_DIR}/cli-versions.log"; then
    fail_check "$title" \
      "the command failed: $(tail -n 3 "${LOG_DIR}/cli-versions.log" | tr '\n' ' ')"
    return
  fi

  local total live_number live_id preview_number listed
  total=$(jq -r '.total // 0' < "$out" 2>/dev/null || echo 0)
  # The live one is not a separate field: a version is marked live by the
  # environments listed against it, which is the honest way round - a version
  # is not "current", an environment points at one.
  live_number=$(jq -r --arg env "$ENV_NAME" \
    '.versions[] | select(.environments | index($env)) | .number' < "$out" 2>/dev/null || true)
  live_id=$(jq -r --arg env "$ENV_NAME" \
    '.versions[] | select(.environments | index($env)) | .id' < "$out" 2>/dev/null || true)
  preview_number=$(jq -r --arg env "$PREVIEW_ENV_NAME" \
    '.versions[] | select(.environments | index($env)) | .number' < "$out" 2>/dev/null || true)
  listed=$(jq -r '[.versions[] | "\(.number)->[\(.environments | join(","))]"] | join(" ")' \
    < "$out" 2>/dev/null || true)

  local expected
  expected=$(deployed_version "$ENV_NAME")

  if [ "${total:-0}" -ge 2 ] && [ -n "$live_number" ] && [ "$live_id" = "$expected" ] \
    && [ -n "$preview_number" ] && [ "$live_number" != "$preview_number" ]; then
    pass_check "$title" \
      "${total} versions listed; ${listed}. Version ${live_number} is marked as the one ${ENV_NAME} runs and its id matches what the control plane reports (${live_id})"
  else
    fail_check "$title" \
      "total=${total}, ${ENV_NAME} marked on version '${live_number}' (id ${live_id}, control plane says ${expected}), ${PREVIEW_ENV_NAME} marked on '${preview_number}'; listing was: ${listed}"
  fi
}

check_cli_secret_list() {
  local json="${LOG_DIR}/cli-secret-list.json"
  local text="${LOG_DIR}/cli-secret-list.txt"
  local title="deis secret list shows the name and never the value"

  if ! deis_cli secret list -e "$ENV_NAME" --json > "$json" 2> "${LOG_DIR}/cli-secret-list.log"; then
    fail_check "$title" \
      "the command failed: $(tail -n 3 "${LOG_DIR}/cli-secret-list.log" | tr '\n' ' ')"
    return
  fi
  # The human form is checked too. It is the one somebody will actually read,
  # and it is a different code path from --json, so a value could leak in one
  # while the other stayed clean.
  deis_cli secret list -e "$ENV_NAME" > "$text" 2>&1 || true

  local names
  names=$(jq -r '.secrets[].name' < "$json" 2>/dev/null | tr '\n' ' ' || true)

  if ! echo "$names" | grep -q "DEMO_SECRET"; then
    fail_check "$title" "DEMO_SECRET is not listed; names were: ${names:-none}"
    return
  fi

  # Both outputs are also swept by check_secret_delivery, which greps every
  # file under LOG_DIR. Asserting it here as well keeps the failure readable:
  # this check names the command that leaked, the sweep only names the file.
  if grep -q "$DEMO_SECRET_VALUE" "$json" "$text" 2>/dev/null; then
    fail_check "$title" \
      "the value came back in the output of \`deis secret list\`"
    return
  fi

  pass_check "$title" \
    "names listed: ${names}- and neither the JSON nor the human output contains the value. There is no command that prints one: the control plane has no endpoint that returns it"
}

check_cli_status() {
  local out="${LOG_DIR}/cli-status.json"
  local title="deis status reports the node actually serving"

  if ! deis_cli status --json > "$out" 2> "${LOG_DIR}/cli-status.log"; then
    fail_check "$title" \
      "the command failed: $(tail -n 3 "${LOG_DIR}/cli-status.log" | tr '\n' ' ')"
    return
  fi

  local healthy reachable serving envs prod_number
  healthy=$(jq -r --arg id "$NODE_ID" \
    '.nodes[] | select(.id == $id) | .healthy' < "$out" 2>/dev/null || true)
  reachable=$(jq -r --arg id "$NODE_ID" \
    '.nodes[] | select(.id == $id) | .reachable' < "$out" 2>/dev/null || true)
  # `serving` is what the node itself answered when asked, not what the
  # catalog intended. That distinction is the whole value of the command:
  # the catalog says what should be running, the node says what is.
  serving=$(jq -r --arg id "$NODE_ID" \
    '.nodes[] | select(.id == $id) | .serving // [] | map(.environment) | sort | join(", ")' \
    < "$out" 2>/dev/null || true)
  envs=$(jq -r '[.environments[] | "\(.name)=v\(.version_number // "none")"] | join(" ")' \
    < "$out" 2>/dev/null || true)
  prod_number=$(jq -r --arg env "$ENV_NAME" \
    '.environments[] | select(.name == $env) | .version_number // empty' < "$out" 2>/dev/null || true)

  if [ "$healthy" = "true" ] && [ "$reachable" = "true" ] \
    && echo "$serving" | grep -q "$ENV_NAME" \
    && echo "$serving" | grep -q "$PREVIEW_ENV_NAME" \
    && [ -n "$prod_number" ]; then
    pass_check "$title" \
      "node ${NODE_ID} is healthy and answered when asked; it reports serving ${APP_ID} in: ${serving}. The catalog side reads ${envs}"
  else
    fail_check "$title" \
      "node ${NODE_ID}: healthy=${healthy} reachable=${reachable} serving='${serving}' (expected both ${ENV_NAME} and ${PREVIEW_ENV_NAME}); environments read ${envs}"
  fi
}

# The node told the control plane it exists, on the path the control plane
# actually serves.
#
# This is a seam no compiler watches: deis-runtime builds the URL and
# deis-control serves it, in two crates that do not depend on each other. They
# were once out of step - the node called `POST .../register` while the control
# plane served `PUT .../registration` - and nothing failed visibly, because a
# node that cannot register still fetches its own assignments and still serves
# every request. What was lost was the control plane knowing about nodes
# nobody had written into its configuration file.
check_node_registration() {
  local title="the node registers itself with the control plane"
  local fleet="${LOG_DIR}/verify-fleet.json"

  # No CLI command covers the fleet, so this reads the control plane's own
  # view of it. That is an operator question, not a deployment step.
  curl -s "$(control_url)/api/runtime" > "$fleet" 2>/dev/null || true

  local registered addr configured_addr
  registered=$(jq -r --arg id "$NODE_ID" \
    '.nodes[] | select(.id == $id) | .registered_at // empty' < "$fleet" 2>/dev/null || true)
  addr=$(jq -r --arg id "$NODE_ID" \
    '.nodes[] | select(.id == $id) | .addr // empty' < "$fleet" 2>/dev/null || true)
  configured_addr="http://127.0.0.1:${DEIS_RUNTIME_PORT}"

  # The give-away for the old mismatch: the node says so once and gives up.
  if grep -q "no node registration endpoint" "${LOG_DIR}/deis-runtime.log" 2>/dev/null; then
    fail_check "$title" \
      "the node reports that this control plane has no registration endpoint, so the two halves disagree about the path or the method"
    return
  fi

  if [ -z "$registered" ]; then
    fail_check "$title" \
      "the control plane has no registration timestamp for ${NODE_ID}; the fleet reads: $(head -c 300 "$fleet")"
    return
  fi

  # A node reports the address it derives from its own hostname; the operator
  # who wrote the file knows what routes from the control plane. The file
  # wins, or a node whose hostname does not resolve here would replace a
  # working address with a broken one and every push to it would start failing.
  if [ "$addr" != "$configured_addr" ]; then
    fail_check "$title" \
      "the node's self-reported address replaced the one in deis_runtime_nodes: ${configured_addr} became ${addr}, so every deployment push and cron fire now goes somewhere the operator did not choose"
    return
  fi

  pass_check "$title" \
    "${NODE_ID} registered at ${registered} on PUT /api/nodes/${NODE_ID}/registration, and the address from deis_runtime_nodes (${addr}) survived its self-report"
}

# The check above only shows that a node the config file already names also
# calls the registration endpoint - which every node here always does, so it
# would pass the same way even if a machine absent from deis_runtime_nodes
# could never join at all. That absence is exactly what "self-registration"
# is supposed to fix (see the module docs on
# crates/deis-control/src/api/registration.rs), and the demo's one node,
# being pre-listed, never puts it to the test.
#
# This calls the same endpoint by hand for an id that appears nowhere in the
# config file, with the node credential the demo's real node also uses, and
# checks that the control plane's fleet view now carries it with
# source=registered - not because an operator wrote it down, but because it
# announced itself. This is not a REST shortcut around `deis deploy`: there
# is no CLI command for a node to register with (that is deis-runtime's job,
# not a developer's), so speaking the same protocol a node speaks is the only
# way to exercise this from outside deis-runtime's own code. The address
# given is the demo's real node, so this is a second identity for a process
# that already exists rather than a node calling home to nowhere.
check_dynamic_node_registration() {
  local title="a node absent from deis_runtime_nodes joins the fleet by registering itself"
  local dynamic_id="${NODE_ID}-unlisted"
  local fleet="${LOG_DIR}/verify-fleet-dynamic.json"

  local status
  status=$(curl -s -o /dev/null -w '%{http_code}' \
    -X PUT -H "Authorization: Bearer ${SECRET}" -H 'content-type: application/json' \
    -d "{\"address\":\"http://127.0.0.1:${DEIS_RUNTIME_PORT}\"}" \
    "$(control_url)/api/nodes/${dynamic_id}/registration")

  if [ "$status" != "200" ]; then
    fail_check "$title" \
      "PUT /api/nodes/${dynamic_id}/registration, presenting the node credential, answered HTTP ${status}"
    return
  fi

  curl -s "$(control_url)/api/runtime" > "$fleet" 2>/dev/null || true

  local source registered_at
  source=$(jq -r --arg id "$dynamic_id" \
    '.nodes[] | select(.id == $id) | .source // empty' < "$fleet" 2>/dev/null || true)
  registered_at=$(jq -r --arg id "$dynamic_id" \
    '.nodes[] | select(.id == $id) | .registered_at // empty' < "$fleet" 2>/dev/null || true)

  if [ "$source" = "registered" ] && [ -n "$registered_at" ]; then
    pass_check "$title" \
      "${dynamic_id} is not in deis_runtime_nodes; PUT .../registration alone put it on the fleet, with source=registered since ${registered_at}"
  else
    fail_check "$title" \
      "PUT .../registration for ${dynamic_id} answered HTTP ${status}, but the fleet shows source='${source:-<absent>}' registered_at='${registered_at:-<none>}' instead of a registered entry"
  fi
}

check_cli_tail() {
  local out="${LOG_DIR}/verify-cli-tail.log"
  local title="deis tail streams a line from a request made after it started"
  : > "$out"

  # A name nothing else produces, so the line that arrives can only have come
  # from the request made below - after the stream was already open. Grepping
  # for a word the app logs on every request would pass on a line from an
  # hour ago and prove nothing about following.
  local probe="tail-probe-$$-${RANDOM}"

  # The CLI's output is read through a pipe rather than tailed out of a file,
  # so waiting for a line is a blocking read that returns the moment one
  # arrives. `exec` means the pid below is the CLI itself and not a shell
  # around it, which matters when it is interrupted at the end.
  deis_cli_env
  coproc DEIS_TAIL { exec env "${DEIS_CLI_ENV[@]}" "${BIN_DIR}/deis" tail -e "$ENV_NAME" 2>&1; }
  local tail_pid=$DEIS_TAIL_PID

  # Wait until the stream is really open before producing anything to see.
  #
  # `deis tail` prints this banner only after the Tail call has returned, and
  # deis-telemetry registers the subscriber inside that handler before it
  # answers - so the banner is proof that a line produced from now on reaches
  # this stream. Doing it the other way round is the classic flake: the
  # request is logged, flushed and broadcast while nobody is listening, and a
  # tail that correctly shows only new lines looks broken.
  local line opened=0
  while IFS= read -r -t 30 -u "${DEIS_TAIL[0]}" line; do
    printf '%s\n' "$line" >> "$out"
    case "$line" in
      *"following new lines only"*)
        opened=1
        break
        ;;
    esac
  done

  if [ "$opened" -ne 1 ]; then
    kill "$tail_pid" 2>/dev/null || true
    wait "$tail_pid" 2>/dev/null || true
    fail_check "$title" \
      "the stream never opened; deis tail said: $(tr '\n' ' ' < "$out" | head -c 300)"
    return
  fi

  app_request "${LOG_DIR}/verify-cli-tail.headers" "/hello/${probe}" > /dev/null

  local seen=0
  while IFS= read -r -t 60 -u "${DEIS_TAIL[0]}" line; do
    printf '%s\n' "$line" >> "$out"
    case "$line" in
      *"greeted ${probe}"*)
        seen=1
        break
        ;;
    esac
  done

  # SIGINT, not SIGKILL: `deis tail` races Ctrl-C against the stream so that
  # it unwinds normally and closes the connection. That is the path a person
  # takes to stop it, so it is the one worth exercising.
  kill -INT "$tail_pid" 2>/dev/null || true
  wait "$tail_pid" 2>/dev/null || true

  if [ "$seen" -eq 1 ]; then
    pass_check "$title" \
      "GET /hello/${probe} was made after the stream opened, and deis tail printed: $(grep -m 1 "greeted ${probe}" "$out" | sed 's/[[:space:]][[:space:]]*/ /g')"
  else
    fail_check "$title" \
      "the stream opened but nothing mentioning ${probe} arrived within 60s; see ${out}"
  fi
}

verify_demo() {
  load_state
  require_command curl
  require_command jq

  echo "🔎 Verifying the demo end to end"
  echo
  # The CLI checks come first because everything after them rests on what
  # `deis deploy` put there. If the deploy report is wrong, saying so before
  # eleven other checks fail for the same reason saves reading all eleven.
  echo "-- the CLI deploys --"
  check_cli_deploy
  check_cli_versions
  echo "-- asset-first serving --"
  check_static_asset
  echo "-- routing --"
  check_route_parameter
  echo "-- secrets --"
  # The list runs before the sweep: it writes its own output under LOG_DIR,
  # and the sweep is what would catch a value in it.
  check_cli_secret_list
  check_secret_delivery
  echo "-- storage --"
  check_kv_roundtrip
  echo "-- queue --"
  check_queue_roundtrip
  echo "-- cron --"
  check_cron
  echo "-- environments --"
  check_second_environment
  check_environment_kv_isolation
  echo "-- versions --"
  # This one moves production to v2 and back, so it runs after every check
  # that expects production to be on v1.
  check_rollback
  echo "-- observability --"
  check_telemetry_logs
  check_cli_tail
  echo "-- the fleet --"
  check_node_registration
  check_dynamic_node_registration
  check_cli_status

  echo
  echo "${CHECKS_PASSED} passed, ${CHECKS_FAILED} failed"
  [ "$CHECKS_FAILED" -eq 0 ]
}

status_demo() {
  load_state

  echo "Status"
  echo "- Control PID: ${DEIS_CONTROL_PID}"
  echo "- Runtime PID: ${DEIS_RUNTIME_PID}"
  echo "- Proxy PID: ${DEIS_PROXY_PID}"
  echo "- Store PID: ${DEIS_STORE_PID:-unknown}"
  echo "- Queue PID: ${DEIS_QUEUE_PID:-unknown}"
  echo "- Telemetry PID: ${DEIS_TELEMETRY_PID:-unknown}"
  echo "- Control API: http://127.0.0.1:${DEIS_CONTROL_PORT}"
  echo "- Runtime: http://127.0.0.1:${DEIS_RUNTIME_PORT}"
  echo "- Proxy: http://127.0.0.1:${DEIS_PROXY_PORT}"
  echo "- Store (gRPC): 127.0.0.1:${DEIS_STORE_PORT:-unknown}"
  echo "- Queue (gRPC): 127.0.0.1:${DEIS_QUEUE_PORT:-unknown}"
  echo "- Telemetry (gRPC): 127.0.0.1:${DEIS_TELEMETRY_PORT:-unknown}"
  echo "- Telemetry metrics: http://127.0.0.1:${DEIS_TELEMETRY_METRICS_PORT:-unknown}/metrics"

  if command -v jq >/dev/null 2>&1 && [ -n "${APP_ID:-}" ]; then
    local current
    current=$(deployed_version 2>/dev/null || true)
    echo "- App ${APP_ID}/${ENV_NAME}: ${current:-not deployed} (uploaded: ${APP_V1_ID:-none} ${APP_V2_ID:-})"
  fi
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
    --deis-store-port)
      DEIS_STORE_PORT=$2
      shift 2
      ;;
    --deis-queue-port)
      DEIS_QUEUE_PORT=$2
      shift 2
      ;;
    --deis-telemetry-port)
      DEIS_TELEMETRY_PORT=$2
      shift 2
      ;;
    --deis-telemetry-metrics-port)
      DEIS_TELEMETRY_METRICS_PORT=$2
      shift 2
      ;;
    --secret)
      SECRET=$2
      shift 2
      ;;
    --app-dir)
      APP_SRC_DIR=$2
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
  verify)
    verify_demo
    ;;
  *)
    usage
    exit 1
    ;;
 esac
