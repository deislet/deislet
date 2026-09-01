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
DEFAULT_DEIS_PROXY_TLS_PORT=8088
DEFAULT_DEIS_STORE_PORT=8090
DEFAULT_DEIS_QUEUE_PORT=8091
DEFAULT_DEIS_TELEMETRY_PORT=8092
DEFAULT_DEIS_TELEMETRY_METRICS_PORT=8093
# Not a component. A plain HTTP server the demo runs so that the example
# application has somewhere real to `fetch`, in the role an upstream API would
# play for a real application. It is what the node's `[fetch] allow` opens up,
# and the only non-public destination that is opened: deis-store's address is
# asked for by the same handler and must be refused.
DEFAULT_DEIS_ORIGIN_PORT=8094
# Also not a component. A certificate authority that speaks enough of RFC 8555
# to issue a certificate, in the same role the origin above plays: something
# real for a component to talk to. It stands in for Let's Encrypt, which no
# demo may contact - its rate limits are counted per registered domain per
# week, it needs names that resolve on the public internet, and a local demo
# that depended on it would fail for reasons that have nothing to do with this
# platform. Built from crates/deis-proxy/examples/acme_test_ca.rs, which is not
# part of any shipped binary.
DEFAULT_ACME_CA_PORT=8089

# The zone the proxy gives every environment an automatic address under, and
# therefore the names the proxy asks the authority to certify. `.test` is
# reserved by RFC 2606 and resolves nowhere, which is what makes it safe: the
# stub authority aims its challenge fetch at loopback whatever the name says,
# and nothing here can reach a real host.
#
# The two names below are not configured anywhere. They are what
# routing.preview_zone synthesises from the applications and environments the
# control plane publishes - `<app>.<zone>` for the default environment and
# `<env>--<app>.<zone>` for each one - which is the point: a certificate
# follows from what is being served, and no list of names is kept in step by
# hand.
ACME_ZONE="deislet.test"

# The example app, and the names the demo files it under. deis-store and
# deis-queue still understand exactly one tenant dimension, but that key is now
# the application and the environment joined by a dot - "demo.production" - and
# it travels in the x-deis-tenant header. So a preview environment of this same
# app gets its own KV, database, blob and queue namespace and cannot read or
# overwrite what production stored.
APP_SRC_DIR="${APP_SRC_DIR:-${ROOT_DIR}/scripts/demo-assets/example-app}"
APP_ID="${APP_ID:-demo}"
# The second application, which the example app calls through a service
# binding. Two applications is the smallest arrangement in which the question
# a service binding asks -- may this one call that one -- has an answer.
CALLEE_APP_ID="${CALLEE_APP_ID:-demo-auth}"
ENV_NAME="${ENV_NAME:-production}"
# A second environment of the same application, deployed to a different
# version on the same node. It is what proves an application is identified by
# (app, environment) and not by app id alone.
PREVIEW_ENV_NAME="${PREVIEW_ENV_NAME:-preview}"
# The two automatic addresses the proxy will certify, spelled out here only so
# the checks can name them. Nothing writes them into any configuration file.
ACME_PRODUCTION_NAME="${APP_ID}.${ACME_ZONE}"
ACME_PREVIEW_NAME="${PREVIEW_ENV_NAME}--${APP_ID}.${ACME_ZONE}"
NODE_ID="${NODE_ID:-deis-runtime-demo-01}"
# Deliberately recognisable: `verify` greps the logs and the data directories
# for it to prove a decrypted secret is never written down anywhere.
DEMO_SECRET_VALUE="${DEMO_SECRET_VALUE:-demo-secret-do-not-log-me}"

# What the origin server answers with. `verify` asserts on this exact string,
# so a handler that returned a plausible body of its own - or an error page
# from something else listening on that port - is not mistaken for a fetch
# that worked.
ORIGIN_BODY="the-demo-origin-answered"

# How long the node lets one response body take to be produced; written into
# the node's configuration at `start` and read back by the check that watches a
# stream be closed for running past it.
DEMO_STREAM_TIMEOUT_MS=4000
# SHA-256 of the string "deislet", which functions/api/crypto.ts digests. A
# `crypto.subtle.digest` that invented well-formed bytes would pass any
# "did it throw?" test; it does not pass this one.
DEISLET_SHA256="939a37e7f6ca53c6b3ba5ee5e748b1cc5dda0fe3b8d5ad6d428ab3bb5961c405"

# What the example app's .config.json asks to be held to, and what the staged
# copy of the second version is rewritten to ask for.
#
# The first is a narrowing: 1000ms is far below the node's own ceiling, so the
# node applies it and a handler that overruns is cut off at the app's number
# rather than at the node's. The second is the opposite request - ten minutes,
# which no node here allows - and it exists so that `verify` can watch the
# clamp refuse it. An application may ask to be held to less and never to
# more; without a version that asks for more, nothing would ever exercise the
# half of that sentence which is a security property.
APP_TIMEOUT_MS="${APP_TIMEOUT_MS:-1000}"
GREEDY_TIMEOUT_MS="${GREEDY_TIMEOUT_MS:-600000}"

# Paths in the example app's static site that `verify` asserts on. They live
# here rather than inline so the file the app ships and the check that reads it
# name the same thing in one place.
#
# scripts/demo-assets/example-app/public/_redirects sends the first to the
# second with a 301; public/_headers puts a year-long Cache-Control on
# everything under /css/; and no file, no rule and no function route claims
# SITE_MISSING_PATH, which is what makes it the path the app's not-found
# behaviour answers.
SITE_OLD_PATH="/old-home"
SITE_REDIRECT_TARGET="/index.html"
SITE_STYLESHEET="/css/app.css"
SITE_STYLESHEET_CACHE_CONTROL="public, max-age=31536000, immutable"
SITE_MISSING_PATH="/no-such-page"
# A marker only public/404.html carries, so a check cannot mistake any other
# 404 - the node's own, a handler's - for the page the bundle ships.
SITE_NOT_FOUND_MARKER="the-bundles-own-404-page"

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
  --deis-origin-port <port>             Origin the example app fetches (default 8094)
  --deis-proxy-tls-port <port>          Proxy HTTPS port (default 8088)
  --acme-ca-port <port>                 Stub certificate authority (default 8089)
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
    "${DEMO_DIR}/origin" \
    "${DEMO_DIR}/acme" \
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
# Owner-only: this file holds the node pre-shared key and the storage
# capability secret the components authenticate to each other with, and the
# value of the demo secret, which `verify` needs in order to go looking for
# it.
write_state() {
  touch "$STATE_FILE"
  chmod 600 "$STATE_FILE"
  cat > "$STATE_FILE" <<EOF
DEIS_CONTROL_PORT=$DEIS_CONTROL_PORT
DEIS_RUNTIME_PORT=$DEIS_RUNTIME_PORT
DEIS_PROXY_PORT=$DEIS_PROXY_PORT
DEIS_PROXY_TLS_PORT=$DEIS_PROXY_TLS_PORT
ACME_CA_PORT=$ACME_CA_PORT
ACME_CA_PID=${ACME_CA_PID:-}
ACME_CA_ROOT=${ACME_CA_ROOT:-}
ACME_STORE_DIR=${ACME_STORE_DIR:-}
ACME_ZONE=$ACME_ZONE
ACME_PRODUCTION_NAME=$ACME_PRODUCTION_NAME
ACME_PREVIEW_NAME=$ACME_PREVIEW_NAME
DEIS_STORE_PORT=$DEIS_STORE_PORT
DEIS_QUEUE_PORT=$DEIS_QUEUE_PORT
DEIS_TELEMETRY_PORT=$DEIS_TELEMETRY_PORT
DEIS_TELEMETRY_METRICS_PORT=$DEIS_TELEMETRY_METRICS_PORT
DEIS_ORIGIN_PORT=$DEIS_ORIGIN_PORT
DEIS_ORIGIN_PID=${DEIS_ORIGIN_PID:-}
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
API_KEY=$API_KEY
STORAGE_AUTH_SECRET=$STORAGE_AUTH_SECRET
SECRET_KEY_FILE=$SECRET_KEY_FILE
APP_ID=$APP_ID
CALLEE_APP_ID=$CALLEE_APP_ID
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
    # A second invocation because `--example` narrows a build to that one
    # target. The stub certificate authority is an example rather than a
    # binary so that `cargo build --workspace` never produces it: nothing
    # that ships should be able to reach a fake authority.
    cargo build -p deis-proxy --example acme_test_ca
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

  # auth_secret turns on the capability-token check
  # (crates/deis-store/src/auth.rs): every call must then present
  # x-deis-storage-token for the tenant it names, or it is refused before any
  # data is touched. Configured here, on deis-queue below and as
  # storage_auth_secret on deis-control, so the demo proves the feature
  # rather than leaving it dark the way an all-loopback install with no
  # secret configured would.
  cat > "$DEIS_STORE_CONFIG" <<EOF
listen = "127.0.0.1:${DEIS_STORE_PORT}"
data_dir = "${DEMO_DIR}/deis-store-data"
auth_secret = "${STORAGE_AUTH_SECRET}"
EOF

  cat > "$DEIS_QUEUE_CONFIG" <<EOF
listen = "127.0.0.1:${DEIS_QUEUE_PORT}"
data_dir = "${DEMO_DIR}/deis-queue-data"
auth_secret = "${STORAGE_AUTH_SECRET}"
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

# Bring up the destination the example application is allowed to fetch.
#
# `fetch` is a basic requirement of Edge Canon, and a platform that grants it
# has to say where an application may go. deis-runtime denies every
# non-public address by default, so without something here the only honest
# demonstration of a working `fetch` would need the internet - and the check
# that matters could not be shown at all, because "refused" and "no network"
# would look the same from the outside.
#
# So the demo serves its own upstream on a port of its own and opens exactly
# that one address in the node's `[fetch] allow`. deis-store sits one port
# away, is not opened, and is asked for by the same handler: one call gets
# through, the neighbouring one does not, and the difference is the policy
# rather than the network.
#
# python3 is the only thing in this script that needs it, which is why its
# absence is reported and does not stop the run: everything except the two
# fetch checks works without it, and those two then fail loudly with the
# reason instead of being quietly skipped.
start_origin() {
  local dir="${DEMO_DIR}/origin"
  mkdir -p "$dir"
  printf '%s\n' "$ORIGIN_BODY" > "${dir}/payload.txt"

  if ! command -v python3 >/dev/null 2>&1; then
    echo "⚠️  python3 not found, so the fetch origin is not started."
    echo "     The example app's outbound-fetch checks will fail; everything else runs."
    DEIS_ORIGIN_PID=""
    return 0
  fi

  # `python3 -m http.server` with two paths added.
  #
  # Everything under / is served from the directory exactly as that module
  # would serve it, which is what the two egress checks use. /slow is the first
  # addition: a body written a piece at a time with a pause between the
  # pieces, and no length announced. It exists because "the upstream's body
  # was passed through without being collected" cannot be shown against an
  # upstream that answers instantly -- collected or not, the answer looks the
  # same from outside. Against this one it does: a handler that collected the
  # body could not send its first byte until the upstream had sent its last.
  #
  # /redirect is the second: it answers 302 to itself with one hop fewer until
  # the hops run out and it points at /payload.txt. It exists for the
  # subrequest ceiling, which counts every hop a redirect chain takes -- so a
  # handler containing exactly one `fetch` can exhaust the ceiling, and that
  # cannot be shown against a destination that answers straight away.
  cat > "${dir}/origin.py" <<'ORIGIN_PY'
import functools
import http.server
import socketserver
import sys
import time
import urllib.parse


class Origin(http.server.SimpleHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def do_GET(self):
        parsed = urllib.parse.urlparse(self.path)
        if parsed.path == "/slow":
            self.send_slowly(urllib.parse.parse_qs(parsed.query))
            return
        if parsed.path == "/redirect":
            self.send_redirect(urllib.parse.parse_qs(parsed.query))
            return
        super().do_GET()

    def send_redirect(self, query):
        # One hop fewer each time, ending at a real file, so ?hops=3 is three
        # 302s and then a 200: four connections in all for one fetch() call.
        hops = int(query.get("hops", ["1"])[0])
        target = "/redirect?hops=%d" % (hops - 1) if hops > 1 else "/payload.txt"
        self.send_response(302)
        self.send_header("Location", target)
        self.send_header("Content-Length", "0")
        self.end_headers()

    def send_slowly(self, query):
        pieces = int(query.get("pieces", ["6"])[0])
        gap_ms = int(query.get("gapMs", ["150"])[0])
        # No Content-Length and no Transfer-Encoding: the body is framed by the
        # connection closing, which is the simplest way for a server this small
        # to answer without knowing its own length in advance.
        self.send_response(200)
        self.send_header("Content-Type", "text/plain; charset=utf-8")
        self.send_header("Connection", "close")
        self.end_headers()
        self.close_connection = True
        for i in range(pieces):
            if i:
                time.sleep(gap_ms / 1000.0)
            self.wfile.write(("piece %d\n" % (i + 1)).encode("utf-8"))
            self.wfile.flush()

    def log_message(self, fmt, *args):
        sys.stderr.write("%s - %s\n" % (self.address_string(), fmt % args))


class Threaded(socketserver.ThreadingMixIn, http.server.HTTPServer):
    daemon_threads = True
    allow_reuse_address = True


port = int(sys.argv[1])
directory = sys.argv[2]
handler = functools.partial(Origin, directory=directory)
Threaded(("127.0.0.1", port), handler).serve_forever()
ORIGIN_PY

  echo "🚀 Starting the fetch origin (HTTP: ${DEIS_ORIGIN_PORT})"
  python3 "${dir}/origin.py" "$DEIS_ORIGIN_PORT" "$dir" \
    > "${LOG_DIR}/origin.log" 2>&1 &
  DEIS_ORIGIN_PID=$!

  local rc=0
  wait_for_service wait_for_http \
    "http://127.0.0.1:${DEIS_ORIGIN_PORT}/payload.txt" "$DEIS_ORIGIN_PID" 30 || rc=$?
  report_service "Origin" "$rc" "${LOG_DIR}/origin.log" \
    "the example app has nowhere to fetch and those checks fail"
}

# The certificate authority the proxy obtains its certificates from.
#
# Not a component, and not a real authority. It is the stub the deis-proxy
# integration tests drive, built as an example so that `cargo build
# --workspace` never produces it, run here on a port the script claimed. It
# signs with a root of its own, which it writes out so `verify` can hand it to
# curl - that is what makes "the certificate served is the one that was issued"
# checkable rather than assertable.
#
# It is started before the proxy because the proxy's configuration file has to
# carry the directory URL, and it is told the proxy's plain HTTP port because
# that is where it will fetch the http-01 challenge from: on a real deployment
# that is port 80, reached over the internet at the name being certified.
start_acme_ca() {
  local binary="${BIN_DIR}/examples/acme_test_ca"
  ACME_CA_ROOT="${DEMO_DIR}/acme-ca-root.pem"
  ACME_CA_DIRECTORY="http://127.0.0.1:${ACME_CA_PORT}/directory"

  if [ ! -x "$binary" ]; then
    echo "❌ ${binary} was not built, so automatic certificates cannot be demonstrated."
    exit 1
  fi

  echo "🚀 Starting the stub certificate authority (HTTP: ${ACME_CA_PORT})"
  "$binary" \
    --port "$ACME_CA_PORT" \
    --validation-port "$DEIS_PROXY_PORT" \
    --root-out "$ACME_CA_ROOT" \
    > "${LOG_DIR}/acme-ca.log" 2>&1 &
  ACME_CA_PID=$!

  local rc=0
  wait_for_service wait_for_http "$ACME_CA_DIRECTORY" "$ACME_CA_PID" 30 || rc=$?
  report_service "Certificate authority" "$rc" "${LOG_DIR}/acme-ca.log" \
    "the proxy can obtain no certificate and the automatic certificate checks fail"
}

# What the authority has been asked for, from its own records rather than from
# the proxy's log: which names it issued for, how many accounts were
# registered, and every challenge it fetched with whether the answer matched.
acme_ca_stats() {
  curl -s -m 5 "http://127.0.0.1:${ACME_CA_PORT}/stats" 2>/dev/null || echo '{}'
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

# GET one of the control plane's human endpoints, presenting the API key.
#
# Used only where there is no CLI command for the question - the fleet view is
# an operator's, not a developer's. Everything a developer does goes through
# `deis`. The key is required now that `start` turns `api_auth_enabled` on, so
# a caller that forgets it gets 401 rather than an answer.
control_api() {
  curl -s -H "Authorization: Bearer ${API_KEY}" "$(control_url)$1"
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
    # The control plane requires it now, so the CLI has to carry it. In the
    # environment rather than in a credentials file for the reason the endpoint
    # is: the demo's key is minted at start time and a file committed to the
    # repository cannot know it. A person stores theirs with
    # `deis config set control.token`.
    "DEIS_CONTROL_TOKEN=${API_KEY}"
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
  #
  # It also asks for a wall-clock ceiling ten minutes long, where the app's
  # own sources ask for one second. That is the greedy direction, which no
  # application is allowed to move in: the node clamps it back to its own
  # ceiling and says so. The preview environment is deployed from this tree,
  # so the demo runs one environment that narrowed its limits and one that
  # tried to widen them, side by side on the same node.
  if [ "$label" != "v1" ]; then
    sed -i "s/\"BUILD_LABEL\": \"v1\"/\"BUILD_LABEL\": \"${label}\"/" "$dest/.config.json"
    sed -i "s/\"timeoutMs\": ${APP_TIMEOUT_MS}/\"timeoutMs\": ${GREEDY_TIMEOUT_MS}/" \
      "$dest/.config.json"
    sed -i "s/data-label=\"v1\"/data-label=\"${label}\"/" "$dest/public/index.html"
    # And it names no compatibility date at all, where the app's own sources
    # name one. That is what an application deployed before the mechanism
    # existed looks like, and the demo runs one of each side by side on the
    # same node: production is written against 2026-09-01 and gets every
    # Set-Cookie header it sets, the preview names no date and gets the one
    # answer this platform used to give. Neither environment can change the
    # other's behaviour, which is the whole point of the date.
    if command -v jq >/dev/null 2>&1; then
      local undated
      undated=$(jq 'del(.compatibilityDate)' "$dest/.config.json") \
        && printf '%s\n' "$undated" > "$dest/.config.json"
    fi
  fi

  add_websocket_route "$dest"
  add_blob_route "$dest"
  add_stream_route "$dest"
  add_cookie_route "$dest"

  echo "$dest"
}

# The route the WebSocket check talks to, derived from the handler's path in
# the function tree the way every other route is.
WS_ROUTE="/api/ws"

# Give the staged copy a WebSocket handler if the example app does not carry
# one.
#
# The check below needs an application that upgrades, and the example app has
# no such handler today. Writing it into the staged copy keeps the check
# self-contained instead of leaving the one hop nobody has ever exercised
# unexercised; the moment `functions/api/ws.ts` appears in the example app's
# own sources this stops writing anything and the app's version is what gets
# deployed, so there is nothing here to remove by hand.
#
# The handler answers a binary frame with its length and its bytes in hex
# rather than with the bytes. An application can be handed a binary frame -
# the node base64s it across the boundary and the isolate sees an ArrayBuffer
# - but it has no way to send one: `op_ws_server_send` takes a string and only
# ever emits a text message. Describing the bytes is therefore the only way a
# handler can prove which ones reached it.
add_websocket_route() {
  local dest=$1
  local handler="${dest}/functions${WS_ROUTE}.ts"

  if [ -f "$handler" ]; then
    return 0
  fi

  mkdir -p "$(dirname "$handler")"
  cat > "$handler" <<'TS'
// A WebSocket echo endpoint, in the shape Edge Canon describes: make a
// WebSocketPair, accept the server half, and answer 101 with the client half
// attached to the Response.
export default async function handler(context: any): Promise<Response> {
  if (context.request.headers.get("upgrade") !== "websocket") {
    return new Response("this path answers a WebSocket handshake\n", {
      status: 426,
      headers: { "content-type": "text/plain; charset=utf-8" },
    });
  }

  const pair = new WebSocketPair();
  const client = pair[0];
  const server = pair[1];
  server.accept();

  server.addEventListener("message", (event: any) => {
    const data = event.data;
    // Prefixed so an answer cannot be mistaken for the frame that caused it,
    // and so the two frame kinds are told apart by the reply itself.
    if (typeof data === "string") {
      server.send("text:" + data);
      return;
    }
    const bytes = new Uint8Array(data);
    let hex = "";
    for (let i = 0; i < bytes.length; i++) {
      hex += bytes[i].toString(16).padStart(2, "0");
    }
    server.send("binary:" + bytes.length + ":" + hex);
  });

  return new Response(null, { status: 101, webSocket: client });
}
TS
}

# The route the streaming checks talk to.
STREAM_ROUTE="/api/stream"

# Give the staged copy a streaming handler if the example app does not carry
# one.
#
# Written here for the same reason the WebSocket and object-storage handlers
# are: the checks below need an application that actually returns a body it is
# still producing, and a feature nothing calls is a feature nobody has. The
# moment `functions/api/stream.ts` appears in the example app's own sources
# this stops writing anything.
#
# One handler covers every shape the checks need, chosen by `?mode=`, because
# they are all the same mechanism seen from different ends: a feed that ends
# normally, one that never ends, one that fails part-way, an upstream body
# passed straight through, and a way to ask the isolate what became of a
# producer whose client walked away.
add_stream_route() {
  local dest=$1
  local handler="${dest}/functions${STREAM_ROUTE}.ts"

  if [ -f "$handler" ]; then
    return 0
  fi

  mkdir -p "$(dirname "$handler")"
  cat > "$handler" <<'TS'
// A response body that is produced after the head has gone out.
//
// `?mode=` picks the shape:
//
//   sse      the default: `frames` server-sent-events frames, `gapMs` apart,
//            then the stream ends normally.
//   endless  frames for as long as anyone reads. Used to watch a client hang
//            up, and to watch the node close a stream that runs past its
//            budget.
//   throw    `after` frames and then an exception, which the client meets long
//            after it was told 200.
//   relay    `url` is fetched and its body handed straight on. Nothing here
//            reads it, so nothing here holds it.
//   report   not a stream at all: what became of the producers above.
//
// The two module-level values are deliberate. An isolate is reused across
// requests for the same application, so a producer that was cancelled can say
// so to a later request -- which is the only way, from outside, to tell "the
// client went away and the producer stopped" from "the client went away and
// the producer kept going".
let lastCancelled: string | null = null;
let framesProduced = 0;

export default async function handler(context: any): Promise<Response> {
  const url = new URL(context.request.url);
  const mode = url.searchParams.get("mode") ?? "sse";

  if (mode === "report") {
    return new Response(
      JSON.stringify({ lastCancelled, framesProduced }),
      { headers: { "content-type": "application/json; charset=utf-8" } },
    );
  }

  if (mode === "relay") {
    const target = url.searchParams.get("url");
    if (!target) {
      return new Response("pass the upstream as ?url=<absolute http url>\n", {
        status: 400,
        headers: { "content-type": "text/plain; charset=utf-8" },
      });
    }
    const upstream = await fetch(target);
    // `upstream.body` is handed over, not read. The whole point: this handler
    // never holds the object, so its size is the upstream's problem and not
    // this isolate's heap.
    return new Response(upstream.body, {
      status: upstream.status,
      headers: {
        "content-type":
          upstream.headers.get("content-type") ?? "application/octet-stream",
      },
    });
  }

  const tag = url.searchParams.get("tag") ?? "";
  const frames = Math.min(Math.max(Number(url.searchParams.get("frames")) || 5, 1), 100);
  const gapMs = Math.min(Math.max(Number(url.searchParams.get("gapMs")) || 120, 0), 2000);
  const throwAfter = mode === "throw"
    ? Math.min(Math.max(Number(url.searchParams.get("after")) || 2, 1), 100)
    : -1;

  const encoder = new TextEncoder();
  let sent = 0;

  const stream = new ReadableStream({
    async pull(controller) {
      // The gap is what makes "frame by frame" visible from outside: frames
      // produced in one burst would arrive in one burst no matter how they
      // crossed the boundary, and would prove nothing.
      if (gapMs > 0 && sent > 0) {
        await new Promise((resolve) => setTimeout(resolve, gapMs));
      }
      if (throwAfter > 0 && sent >= throwAfter) {
        throw new Error(
          "the demo handler failed this stream on purpose after " + sent + " frame(s)",
        );
      }
      if (mode === "sse" && sent >= frames) {
        controller.close();
        return;
      }
      sent += 1;
      framesProduced += 1;
      controller.enqueue(encoder.encode("data: frame " + sent + " at " + Date.now() + "\n\n"));
    },
    cancel(_reason) {
      // Reached when the node tells this producer to stop: the client hung up,
      // or the stream ran past the node's stream_timeout_ms.
      lastCancelled = tag;
    },
  });

  return new Response(stream, {
    headers: {
      "content-type": "text/event-stream",
      "cache-control": "no-cache",
    },
  });
}
TS
}

# The route the compatibility-date checks talk to.
COOKIE_ROUTE="/api/cookies"

# Give the staged copy a handler that sets two cookies in one response.
#
# Two, because one is the number this platform used to send however many a
# handler set: the response header list went through a map on its way out of
# the isolate, and a map keeps one value per name. `Set-Cookie` is the one
# header the Fetch specification says must not be combined, so it is the one
# header a map silently destroys - a session cookie and a CSRF cookie went in
# and one came out, with nothing anywhere saying so.
#
# The handler is written here rather than in the example app for the same
# reason the WebSocket and streaming handlers are: the moment
# `functions/api/cookies.ts` appears in the app's own sources this stops
# writing anything and the app's version is what gets deployed.
add_cookie_route() {
  local dest=$1
  local handler="${dest}/functions${COOKIE_ROUTE}.ts"

  if [ -f "$handler" ]; then
    return 0
  fi

  mkdir -p "$(dirname "$handler")"
  cat > "$handler" <<'TS'
// Two cookies in one response, which is what a login that also issues a CSRF
// token does.
//
// `headers.append` is the only way to say it: `Headers` combines repeated
// names into one comma-joined value for every header except `Set-Cookie`,
// which the Fetch specification exempts precisely because a cookie value may
// contain a comma. Whether both survive the trip out of the isolate is decided
// by this application's compatibility date, not by anything written here.
export default async function handler(_context: any): Promise<Response> {
  const headers = new Headers({
    "content-type": "application/json; charset=utf-8",
  });
  headers.append("set-cookie", "session=abc123; Path=/; HttpOnly");
  headers.append("set-cookie", "csrf=xyz789; Path=/");
  return new Response(JSON.stringify({ cookiesSet: 2 }), {
    status: 200,
    headers,
  });
}
TS
}

# The route the object-storage checks talk to.
BLOB_ROUTE="/api/objects"

# Give the staged copy an object-storage handler if the example app does not
# carry one.
#
# Written here for the same reason the WebSocket handler is: the checks below
# need an application that actually calls `context.services.blob`, and a
# feature nothing calls is a feature nobody has. The moment
# `functions/api/objects.ts` appears in the example app's own sources this
# stops writing anything.
#
# The handler is deliberately an HTTP shim over the binding rather than a
# JSON toy: it forwards a `Range` header straight into `blob.get` and builds
# a 206 with a `Content-Range` out of what comes back, which is the exact
# thing the range feature exists to make possible. If the numbers the store
# resolves were wrong, the header this writes would be wrong with them.
add_blob_route() {
  local dest=$1
  local handler="${dest}/functions${BLOB_ROUTE}.ts"

  # The binding only exists if the application asks for it: a handler calling
  # `context.services.blob` in an app that does not declare "blob" gets the
  # "this service is not available to this app" error rather than an object
  # store. Enabled here, in the staged copy, for the same reason the handler
  # is written here.
  local config="${dest}/.config.json"
  if [ -f "$config" ] && command -v jq >/dev/null 2>&1; then
    local with_blob
    with_blob=$(jq '.services.blob = { "enabled": true }' "$config") \
      && printf '%s\n' "$with_blob" > "$config"
  fi

  if [ -f "$handler" ]; then
    return 0
  fi

  mkdir -p "$(dirname "$handler")"
  cat > "$handler" <<'TS'
// Object storage through `context.services.blob`: list, head, ranged read and
// conditional write, exposed as the HTTP an application would build on them.
const json = (body: any, status = 200) =>
  new Response(JSON.stringify(body), {
    status,
    headers: { "content-type": "application/json; charset=utf-8" },
  });

export default async function handler(context: any): Promise<Response> {
  const blob = context.services.blob;
  const url = new URL(context.request.url);
  const params = url.searchParams;
  const op = params.get("op") || "";
  const key = params.get("key") || "";
  const method = context.request.method;

  if (op === "list") {
    const page = await blob.list({
      prefix: params.get("prefix") || "",
      delimiter: params.get("delimiter") || undefined,
      limit: params.get("limit") ? Number(params.get("limit")) : undefined,
      cursor: params.get("cursor") || undefined,
    });
    return json({
      objects: page.objects.map((o: any) => ({ key: o.key, size: o.size, etag: o.etag })),
      prefixes: page.prefixes,
      list_complete: page.list_complete,
      cursor: page.cursor === undefined ? null : page.cursor,
    });
  }

  if (op === "head") {
    const meta = await blob.head(key);
    if (meta === null) return json({ key, found: false }, 404);
    return json({
      key: meta.key,
      found: true,
      size: meta.size,
      etag: meta.etag,
      contentType: meta.contentType,
    });
  }

  if (method === "PUT" || method === "POST") {
    const onlyIf: any = {};
    if (params.get("ifMatch")) onlyIf.ifMatch = params.get("ifMatch");
    if (params.get("ifNoneMatch")) onlyIf.ifNoneMatch = params.get("ifNoneMatch");
    const written = await blob.put(key, await context.request.text(), {
      contentType: params.get("contentType") || undefined,
      onlyIf: Object.keys(onlyIf).length > 0 ? onlyIf : undefined,
    });
    // null, and only null, means the condition did not hold. Every other
    // failure would have thrown.
    if (written === null) {
      const now = await blob.head(key);
      return json({ key, stored: false, etag: now === null ? null : now.etag }, 412);
    }
    return json({ key, stored: true, etag: written.etag, size: written.size }, 201);
  }

  if (method === "DELETE") {
    const ifMatch = params.get("ifMatch");
    const deleted = await blob.delete(key, ifMatch ? { onlyIf: { ifMatch } } : undefined);
    return json({ key, deleted }, deleted ? 200 : 412);
  }

  // A read. A `Range` header is forwarded exactly as it arrived; an
  // unsatisfiable one throws, which is what 416 is for, and a missing key is
  // null, which is what 404 is for.
  const range = context.request.headers.get("range");
  let object: any;
  try {
    object = await blob.get(key, range ? { range } : undefined);
  } catch (error: any) {
    return json({ key, error: String(error && error.message) }, 416);
  }
  if (object === null) return json({ key, found: false }, 404);

  const headers: Record<string, string> = {
    "content-type": object.contentType || "application/octet-stream",
    etag: '"' + object.etag + '"',
  };
  if (object.range !== null) {
    const first = object.range.offset;
    const last = first + object.range.length - 1;
    headers["content-range"] = "bytes " + first + "-" + last + "/" + object.size;
    return new Response(await object.text(), { status: 206, headers });
  }
  return new Response(await object.text(), { status: 200, headers });
}
TS
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

# The second application: the one the example app calls.
#
# It exists because a service binding needs two applications, and because the
# consenting half of one lives in the *callee's* own bundle. Written here
# rather than kept in `demo-assets` for the same reason the WebSocket handler
# is written into the staged copy: it is scaffolding for a check, and the
# moment a second example project appears this stops writing anything.
#
# `callableBy` is the whole of the consent. It names `demo` and nothing else,
# so the check below can prove both halves from one deployment: `demo` gets
# through, and a caller that is not on the list does not.
prepare_callee_source() {
  local dest="${DEMO_DIR}/app-callee"

  rm -rf "$dest"
  mkdir -p "${dest}/functions"

  cat > "${dest}/.config.json" <<JSON
{
  "version": "1.0.0",
  "name": "${CALLEE_APP_ID}",
  "runtime": "standard-v1",
  "language": "typescript",
  "functionRoot": "./functions",
  "compatibilityDate": "2026-09-01",
  "callableBy": ["${APP_ID}"],
  "vendors": { "deislet": { "enabled": true } },
  "build": { "outDir": "./dist" }
}
JSON

  cat > "${dest}/functions/verify.ts" <<'TS'
// The callee of the demo's service binding.
//
// It answers with what the platform told it about the call rather than with
// anything the caller wrote: `x-deis-caller` is stamped by the node over
// whatever the caller put there, so a callee reading it is reading a fact.
export default async function handler(context: any): Promise<Response> {
  const caller = context.request.headers.get("x-deis-caller");
  const note = context.request.headers.get("x-demo-note");
  const body = await context.request.text();
  return new Response(
    JSON.stringify({
      calledBy: caller,
      note,
      received: body,
      // Proof the callee ran its own handler rather than anything of the
      // caller's: this is the callee's own environment table.
      handlerOf: "verify",
    }),
    { status: 202, headers: { "content-type": "application/json; charset=utf-8" } },
  );
}
TS

  echo "$dest"
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
  # The application the example app's AUTH binding calls. Deployed after the
  # caller on purpose: a binding is resolved when the call is made, not when
  # the caller is deployed, so neither application has to exist before the
  # other and there is no order to get right.
  local src_callee
  src_callee=$(prepare_callee_source) || return 1
  # `--app` is spelled out because the demo puts DEIS_APP in the CLI's
  # environment for the example app, and a deploy that took the environment's
  # answer would file this bundle under the wrong application - which the
  # control plane refuses, correctly, by comparing the manifest.
  cli_deploy "$src_callee" "$ENV_NAME" cli-deploy-callee --app "$CALLEE_APP_ID" || return 1
  echo "   ✓ deis deploy: ${CALLEE_APP_ID} is serving ${ENV_NAME}; it consents to be called by ${APP_ID}"

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
  local user_deis_proxy_tls_port
  local user_acme_ca_port
  local user_deis_store_port
  local user_deis_queue_port
  local user_deis_telemetry_port
  local user_deis_telemetry_metrics_port
  local user_deis_origin_port
  user_deis_control_port=${DEIS_CONTROL_PORT:-}
  user_deis_runtime_port=${DEIS_RUNTIME_PORT:-}
  user_deis_proxy_port=${DEIS_PROXY_PORT:-}
  user_deis_proxy_tls_port=${DEIS_PROXY_TLS_PORT:-}
  user_acme_ca_port=${ACME_CA_PORT:-}
  user_deis_store_port=${DEIS_STORE_PORT:-}
  user_deis_queue_port=${DEIS_QUEUE_PORT:-}
  user_deis_telemetry_port=${DEIS_TELEMETRY_PORT:-}
  user_deis_telemetry_metrics_port=${DEIS_TELEMETRY_METRICS_PORT:-}
  user_deis_origin_port=${DEIS_ORIGIN_PORT:-}

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
  DEIS_PROXY_TLS_PORT=$user_deis_proxy_tls_port
  ACME_CA_PORT=$user_acme_ca_port
  DEIS_STORE_PORT=$user_deis_store_port
  DEIS_QUEUE_PORT=$user_deis_queue_port
  DEIS_TELEMETRY_PORT=$user_deis_telemetry_port
  DEIS_TELEMETRY_METRICS_PORT=$user_deis_telemetry_metrics_port
  DEIS_ORIGIN_PORT=$user_deis_origin_port

  claim_port DEIS_RUNTIME_PORT "${DEIS_RUNTIME_PORT:-$DEFAULT_DEIS_RUNTIME_PORT}"
  claim_port DEIS_PROXY_PORT "${DEIS_PROXY_PORT:-$DEFAULT_DEIS_PROXY_PORT}"
  claim_port DEIS_PROXY_TLS_PORT "${DEIS_PROXY_TLS_PORT:-$DEFAULT_DEIS_PROXY_TLS_PORT}"
  claim_port ACME_CA_PORT "${ACME_CA_PORT:-$DEFAULT_ACME_CA_PORT}"
  claim_port DEIS_STORE_PORT "${DEIS_STORE_PORT:-$DEFAULT_DEIS_STORE_PORT}"
  claim_port DEIS_QUEUE_PORT "${DEIS_QUEUE_PORT:-$DEFAULT_DEIS_QUEUE_PORT}"
  claim_port DEIS_TELEMETRY_PORT "${DEIS_TELEMETRY_PORT:-$DEFAULT_DEIS_TELEMETRY_PORT}"
  claim_port DEIS_TELEMETRY_METRICS_PORT \
    "${DEIS_TELEMETRY_METRICS_PORT:-$DEFAULT_DEIS_TELEMETRY_METRICS_PORT}"
  claim_port DEIS_ORIGIN_PORT "${DEIS_ORIGIN_PORT:-$DEFAULT_DEIS_ORIGIN_PORT}"

  SECRET=${SECRET:-$(generate_secret)}

  # The shared secret deis-store's, deis-queue's and deis-control's
  # per-tenant capability tokens are all derived from - see
  # crates/deis-store/src/auth.rs. A separate value from SECRET on purpose:
  # SECRET authenticates a *node* to the control plane, this one authenticates
  # a *storage call* to whichever tenant it claims, and the two channels have
  # nothing to do with each other. Configured on all three of deis-store,
  # deis-queue and deis-control below - see check_storage_capability_token for
  # what it looks like when only some of the three carry it.
  STORAGE_AUTH_SECRET=${STORAGE_AUTH_SECRET:-$(generate_secret)}

  # The credential a person - the `deis` CLI, a dashboard - presents to the
  # control plane's human API. A third value, and a third channel: SECRET
  # authenticates a *node*, STORAGE_AUTH_SECRET authenticates a *storage call*,
  # and this one authenticates *a caller who may deploy*. It is generated
  # rather than left out because `api_auth_enabled` defaults to off, and a demo
  # that left it off would leave every catalog route - the deploy, the version
  # upload, the storage-token mint - open to anyone who can reach the port,
  # while looking exactly like one where authentication works.
  API_KEY=${API_KEY:-$(generate_secret)}

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
  start_origin
  start_acme_ca

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
    # learns where storage lives from its own file. storage_auth_secret must
    # be the identical string as auth_secret on both deis-store and
    # deis-queue above - it is what this control plane mints each tenant's
    # capability token from; set it here and not on the two services, or on
    # the services and not here, and every storage call is refused.
    cat > "$DEIS_CONTROL_CONFIG" <<EOF
bind_addr = "0.0.0.0"
port = ${DEIS_CONTROL_PORT}
api_auth_enabled = true
api_keys = ["${API_KEY}"]
heartbeat_interval = 30
heartbeat_timeout = 90
node_preshared_keys = ["${SECRET}"]
deis_runtime_poll_interval = 5
bundle_dir = "${DEMO_DIR}/bundles"
public_base_url = "http://127.0.0.1:${DEIS_CONTROL_PORT}"
secret_key_file = "${SECRET_KEY_FILE}"
store_endpoint = "http://127.0.0.1:${DEIS_STORE_PORT}"
queue_endpoint = "http://127.0.0.1:${DEIS_QUEUE_PORT}"
storage_auth_secret = "${STORAGE_AUTH_SECRET}"

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
    #
    # [fetch] is the node's outbound policy, and this is the whole of it: one
    # address opened, the demo's own origin server. Everything an application
    # can name that is not a public internet address stays refused - which on
    # this machine is everything else, deis-store and deis-queue and the
    # control plane included. Their addresses are refused twice over: they are
    # loopback, and they are the platform's own service endpoints, which no
    # allowlist entry can reach. `verify` asks for one of each.
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

[fetch]
allow = ["127.0.0.1:${DEIS_ORIGIN_PORT}"]

# Four seconds, where the platform default is five minutes.
#
# This is the ceiling on how long one response body may take to be produced,
# counted from the moment its head reached the client. It is short here for one
# reason: the verification opens a stream that never ends and watches the node
# close it, and a check that waited out the real default would take five
# minutes. Everything else the streaming checks do finishes inside a second, so
# the small number costs them nothing.
[default_execution_limits]
stream_timeout_ms = ${DEMO_STREAM_TIMEOUT_MS}
EOF

    ACME_STORE_DIR="${DEMO_DIR}/acme"
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

# Every environment of every application gets an address under this zone,
# synthesised from the routing table the control plane publishes. Those
# addresses are the names the block below certifies: nothing lists them.
[routing]
preview_zone = "${ACME_ZONE}"

[tls]
ports = [${DEIS_PROXY_TLS_PORT}]

# Automatic certificates. Off by default in a real installation; on here
# because demonstrating it is the point.
#
# directory_url points at the stub authority this script started. In a real
# installation it would be left unset, which is Let's Encrypt staging, until a
# name has been issued successfully - and only then set to production.
#
# The intervals are seconds rather than the twelve hours a real installation
# uses, so a renewal pass happens while the demo is watching.
[tls.acme]
enabled = true
directory_url = "${ACME_CA_DIRECTORY}"
contact_email = "ops@deislet.test"
terms_of_service_agreed = true
storage_dir = "${ACME_STORE_DIR}"
check_interval_secs = 2
retry_backoff_secs = 1
max_retry_backoff_secs = 4
# The automatic per-environment addresses are certified one by one here. A
# real installation with churning preview environments would leave this off
# and place one wildcard certificate instead, because each name costs one of
# the fifty certificates a week a registered domain gets and a wildcard needs
# dns-01.
include_preview_addresses = true
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
  echo "- Proxy TLS: https://${ACME_PRODUCTION_NAME}:${DEIS_PROXY_TLS_PORT}/ (certificate obtained over ACME; curl needs --cacert ${ACME_CA_ROOT} --resolve)"
  echo "- Store (gRPC): 127.0.0.1:${DEIS_STORE_PORT}"
  echo "- Queue (gRPC): 127.0.0.1:${DEIS_QUEUE_PORT}"
  echo "- Telemetry (gRPC): 127.0.0.1:${DEIS_TELEMETRY_PORT}"
  echo "- Telemetry metrics: http://127.0.0.1:${DEIS_TELEMETRY_METRICS_PORT}/metrics"
  echo "- Fetch origin: http://127.0.0.1:${DEIS_ORIGIN_PORT}/payload.txt (the one address applications may reach)"
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
  # Not a component, but it holds a port like one, and a port still held is
  # what makes the next `start` fail to bind.
  stop_service "Origin" "${DEIS_ORIGIN_PID:-}" || failed=1
  stop_service "Certificate authority" "${ACME_CA_PID:-}" || failed=1

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

# The node's log with the terminal colouring taken out.
#
# tracing writes its field names and values wrapped in ANSI escapes when the
# log is a file as much as when it is a terminal, so `keys=["ASSETS"]` on the
# page is `keys` ESC [0m ESC [2m = ... in the bytes. A grep for what the line
# says has to read what a person reads.
node_log() {
  sed 's/\x1b\[[0-9;]*m//g' "${LOG_DIR}/deis-runtime.log"
}

# What one log line says, without the timestamp, level, thread, tracing span
# and trailing key=value fields around it.
#
# Quoting the node's own sentence is the point of the checks that read the log
# - the numbers in it are the node's, not this script's idea of them - and a
# quotation is only readable if it is the sentence rather than the whole line.
log_message() {
  sed 's/^[^ ]* *[A-Z]* *ThreadId([0-9]*) //; s/^request{[^}]*}: //; s/ app_id=.*//'
}

# base64(HMAC-SHA256(STORAGE_AUTH_SECRET, tenant)) - the identical
# construction deis_wire::conventions::storage_token uses on every side of
# the storage wire (crates/deis-wire/src/conventions.rs), so a call made with
# this is indistinguishable to deis-store or deis-queue from one deis-runtime
# itself made. Needed here only to let check_kv_roundtrip's direct call to
# deis-store present a token now that auth_secret is configured; the
# capability-token check itself proves the *absence* of a token is refused,
# which needs no token to be computed at all.
storage_token() {
  local tenant=$1
  printf '%s' "$tenant" | openssl dgst -sha256 -hmac "$STORAGE_AUTH_SECRET" -binary \
    | openssl base64 -A
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

# ----------------------------------------------------------------------
# What a version's static site says about itself
# ----------------------------------------------------------------------
#
# `_redirects`, `_headers` and the not-found behaviour are three files' worth
# of behaviour an author gets without writing a function. They are parsed when
# the bundle is built, travel in the manifest, are compiled once when the
# version loads, and are applied by the HTTP entry before an isolate is
# started - so every check below is answered without one.
#
# The example app ships all three (scripts/demo-assets/example-app/public).
# Nothing else in this file would notice if the entry went back to a plain
# index lookup: every other asset check asks for a path that names a real
# file, which a lookup answers on its own.

check_site_redirect() {
  local title="a _redirects rule answers with a redirect, and the query rides along"
  local headers="${LOG_DIR}/verify-site-redirect.headers"
  # A query the target does not carry one of its own, so what arrives at the
  # target can only have come from the request. A rule that dropped it would
  # land a campaign link on the new page with the tracking the author was
  # counting gone, and the redirect would still look correct.
  local query="ref=demo-campaign"

  app_request "$headers" "${SITE_OLD_PATH}?${query}" > /dev/null

  local status location
  status=$(http_status "$headers")
  location=$(header_value "$headers" Location)

  local expected="${SITE_REDIRECT_TARGET}?${query}"
  if [ "$status" = "301" ] && [ "$location" = "$expected" ]; then
    pass_check "$title" \
      "GET ${SITE_OLD_PATH}?${query} answered HTTP ${status} with location: ${location}, without starting an isolate. 301 and not the 302 a two-field rule defaults to, because the rule in _redirects carries a third field"
  else
    fail_check "$title" \
      "GET ${SITE_OLD_PATH}?${query} answered HTTP ${status} with location '${location:-<none>}'; expected HTTP 301 to ${expected}"
  fi
}

check_site_headers() {
  local title="a _headers block puts its header on the asset it names"
  local styled="${LOG_DIR}/verify-site-headers-css.headers"
  local plain="${LOG_DIR}/verify-site-headers-root.headers"

  app_request "$styled" "$SITE_STYLESHEET" > /dev/null
  app_request "$plain" "/" > /dev/null

  local styled_cache plain_cache referrer
  styled_cache=$(header_value "$styled" Cache-Control)
  plain_cache=$(header_value "$plain" Cache-Control)
  referrer=$(header_value "$styled" Referrer-Policy)

  # Two files, not one. A node that applied every block to every response
  # would put the year-long Cache-Control on the HTML too, and asking only
  # about the stylesheet could not tell that apart from the block matching the
  # path it names.
  if [ "$styled_cache" = "$SITE_STYLESHEET_CACHE_CONTROL" ] \
    && [ -n "$referrer" ] \
    && [ "$plain_cache" != "$SITE_STYLESHEET_CACHE_CONTROL" ]; then
    pass_check "$title" \
      "GET ${SITE_STYLESHEET} carries cache-control: ${styled_cache} from the /css/* block and referrer-policy: ${referrer} from the /* block above it; GET / matches only the second and keeps the node's own cache-control: ${plain_cache}"
  else
    fail_check "$title" \
      "expected ${SITE_STYLESHEET} to carry cache-control: ${SITE_STYLESHEET_CACHE_CONTROL} and a referrer-policy, and / to keep the node's default; got '${styled_cache:-<none>}' and '${referrer:-<none>}' on the stylesheet, '${plain_cache:-<none>}' on /"
  fi
}

check_site_not_found() {
  local title="a path naming no file falls back the way the app configured"
  local headers="${LOG_DIR}/verify-site-404.headers"
  local body
  body=$(app_request "$headers" "$SITE_MISSING_PATH")

  local status
  status=$(http_status "$headers")

  if [ "$status" = "404" ] && echo "$body" | grep -q "$SITE_NOT_FOUND_MARKER"; then
    pass_check "$title" \
      "GET ${SITE_MISSING_PATH} answered HTTP ${status} with the bundle's own 404.html - it carries ${SITE_NOT_FOUND_MARKER}, which nothing else does. The project asks for \"notFound\": \"404-page\", so that page is served instead of the isolate's plain \"Not Found\"; a path a function route claims is still handed to the isolate, which is why /hello/world answers from the handler and not from here"
  else
    fail_check "$title" \
      "GET ${SITE_MISSING_PATH} answered HTTP ${status} with a body that does not carry ${SITE_NOT_FOUND_MARKER}: $(echo "$body" | tr '\n' ' ' | head -c 200)"
  fi
}

# ----------------------------------------------------------------------
# env.ASSETS
# ----------------------------------------------------------------------
#
# The other half of asset-first: the entry answers a request for a file
# before an isolate exists, and this is the way back in for the handler that
# ran anyway and wants the asset layer's answer for a path of its own.
#
# functions/api/asset.ts reports what `context.env.ASSETS` actually is as well
# as what it answered, which is what lets the second check below tell a
# working binding from a string the application put there.

check_assets_binding() {
  local title="env.ASSETS.fetch() answers a handler with this version's own file"
  local body
  body=$(app_request "${LOG_DIR}/verify-assets-binding.headers" \
    "/api/asset?path=${SITE_STYLESHEET}")
  answered_json "$title" assets-binding "$body" || return

  local status ctype got reported_bytes
  status=$(echo "$body" | jq -r '.status // empty' 2>/dev/null || true)
  ctype=$(echo "$body" | jq -r '.contentType // empty' 2>/dev/null || true)
  got=$(echo "$body" | jq -r '.body // empty' 2>/dev/null || true)
  # What the handler measured on its side of the boundary, which is the one
  # number a shell cannot fake: `$(...)` eats a trailing newline, so a
  # comparison of the text alone would accept an answer one byte short.
  reported_bytes=$(echo "$body" | jq -r '.bytes // empty' 2>/dev/null || true)

  # The bytes on disk, from the tree this version was built from. Comparing
  # digests rather than eyeballing a prefix: a binding that answered with the
  # right content type and half the file would pass anything looser.
  local file="${DEMO_DIR}/app-v1/public${SITE_STYLESHEET}"
  local expected want_digest got_digest on_disk_bytes
  expected=$(cat "$file" 2>/dev/null || true)
  on_disk_bytes=$(wc -c < "$file" 2>/dev/null | tr -d ' ' || true)
  want_digest=$(printf '%s' "$expected" | openssl dgst -sha256 | awk '{print $NF}' | cut -c1-12)
  got_digest=$(printf '%s' "$got" | openssl dgst -sha256 | awk '{print $NF}' | cut -c1-12)

  if [ -z "$expected" ]; then
    fail_check "$title" \
      "the staged source tree has no ${file}, so there is nothing to compare the binding's answer against"
    return
  fi

  if [ "$status" = "200" ] && [ "$got" = "$expected" ] \
    && [ "$reported_bytes" = "$on_disk_bytes" ]; then
    pass_check "$title" \
      "the handler called env.ASSETS.fetch(\"${SITE_STYLESHEET}\") and got HTTP ${status}, content-type ${ctype} and ${reported_bytes} bytes - the size of ${file} on disk - whose text has sha256 ${got_digest}, the digest of that file. Nothing was fetched over the network: those bytes have been in the node's memory since the version loaded"
  else
    fail_check "$title" \
      "env.ASSETS.fetch(\"${SITE_STYLESHEET}\") answered HTTP ${status:-<none>} with ${reported_bytes:-0} bytes and sha256 ${got_digest:-<none>}; expected 200 with ${on_disk_bytes:-0} bytes and ${want_digest} (the packaged file)"
  fi
}

check_assets_binding_is_not_shadowable() {
  local title="an application declaring ASSETS in its own env does not get its string"
  local body
  body=$(app_request "${LOG_DIR}/verify-assets-shadow.headers" "/api/asset")
  answered_json "$title" assets-shadow "$body" || return

  local kind callable leaked
  kind=$(echo "$body" | jq -r '.bindingKind // empty' 2>/dev/null || true)
  callable=$(echo "$body" | jq -r '.bindingIsCallable // false' 2>/dev/null || true)
  leaked=$(echo "$body" | jq -r '.declaredValueVisible // empty' 2>/dev/null || true)

  # The node's own account of what it did with the key, which is the half a
  # response cannot show: an application that never declared ASSETS would
  # produce the same JSON above.
  local dropped
  dropped=$(node_log \
    | grep -m 1 'named like reserved platform keys' \
    | grep 'ASSETS' || true)

  if [ "$kind" = "object" ] && [ "$callable" = "true" ] && [ -z "$leaked" ] \
    && [ -n "$dropped" ]; then
    pass_check "$title" \
      "the bundle's manifest declares an ordinary variable called ASSETS; the node dropped it before the map became context.env ($(echo "$dropped" | sed 's/.*keys=/keys=/')), and inside the isolate env.ASSETS is an ${kind} whose fetch() is a function, not the string the application shipped"
  else
    fail_check "$title" \
      "env.ASSETS is a ${kind:-<none>} (callable: ${callable}), the application's own value is $(if [ -n "$leaked" ]; then echo "visible as '${leaked}'"; else echo "not visible"; fi), and the node's log $(if [ -n "$dropped" ]; then echo "reports the key dropped"; else echo "never reports dropping a reserved key"; fi)"
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

# ----------------------------------------------------------------------
# The WebSocket hop
# ----------------------------------------------------------------------
#
# One handshake and one frame of each kind, performed by hand against
# deis-proxy.
#
# Through deis-proxy rather than straight at the node on purpose. An upgrade
# is the one exchange a reverse proxy is most likely to ruin -- it can answer
# the 101 itself, rewrite Sec-WebSocket-Accept, buffer the frames, or hold the
# connection only as long as it holds an ordinary response -- and this hop had
# never been exercised with a real handshake at all.
#
# By hand because of the evidence. A client library invents a fresh
# Sec-WebSocket-Key on every run and checks the answer privately, which is
# precisely the value worth printing here. RFC 6455 section 1.3 publishes one
# key and the answer to it, so sending that key makes the correct
# Sec-WebSocket-Accept a constant that can be shown and compared. The nonce is
# only a handshake nonce; nothing about the connection depends on it being
# unpredictable.
#
# python3 is used the same way `start_origin` uses it -- from the standard
# library, nothing installed -- and its absence fails the check with the
# reason rather than skipping it.
WS_KEY="dGhlIHNhbXBsZSBub25jZQ=="
WS_ACCEPT="s3pPLMBiTxaQ9kYGzzhZRbK+xOo="
# Bytes no path that mistook the frame for text could carry through intact:
# 0x00 ends a C string, 0xff and 0xfe never occur in UTF-8 at all, and 0x80 is
# a continuation byte with no lead byte in front of it.
WS_BINARY_HEX="00fffe807f41"
WS_TEXT="demo-websocket-frame"

websocket_exchange() {
  local port=$1
  python3 - "$port" "$APP_ID" "$WS_ROUTE" "$WS_KEY" "$WS_ACCEPT" "$WS_TEXT" "$WS_BINARY_HEX" <<'PY'
import json
import os
import socket
import struct
import sys

port, app, path, key, accept_expected, text, binary_hex = sys.argv[1:8]
payload = bytes.fromhex(binary_hex)
report = {"ok": False}


def read_until_headers_end(sock):
    # One byte at a time: a buffered read would pull whatever follows the
    # header terminator out of the socket and into a buffer this function
    # throws away, eating the first frames of a server that starts talking
    # straight after the 101.
    raw = b""
    while not raw.endswith(b"\r\n\r\n"):
        chunk = sock.recv(1)
        if not chunk:
            raise EOFError("closed after %d bytes of handshake response" % len(raw))
        raw += chunk
        if len(raw) > 8192:
            raise ValueError("handshake response never ended")
    return raw.decode("latin-1")


def recv_exact(sock, count):
    buf = b""
    while len(buf) < count:
        chunk = sock.recv(count - len(buf))
        if not chunk:
            raise EOFError("closed after %d of %d frame bytes" % (len(buf), count))
        buf += chunk
    return buf


def send_frame(sock, opcode, body):
    mask = os.urandom(4)
    masked = bytes(byte ^ mask[i % 4] for i, byte in enumerate(body))
    header = bytes([0x80 | opcode])
    size = len(body)
    if size < 126:
        header += bytes([0x80 | size])
    elif size < 65536:
        header += bytes([0x80 | 126]) + struct.pack("!H", size)
    else:
        header += bytes([0x80 | 127]) + struct.pack("!Q", size)
    sock.sendall(header + mask + masked)


def read_frame(sock):
    # Ping and pong are not the answer and are not a failure either; keep
    # reading rather than deciding the check on frame order.
    while True:
        first, second = recv_exact(sock, 2)
        opcode = first & 0x0F
        size = second & 0x7F
        if size == 126:
            size = struct.unpack("!H", recv_exact(sock, 2))[0]
        elif size == 127:
            size = struct.unpack("!Q", recv_exact(sock, 8))[0]
        if second & 0x80:
            mask = recv_exact(sock, 4)
            body = bytes(b ^ mask[i % 4] for i, b in enumerate(recv_exact(sock, size)))
        else:
            body = recv_exact(sock, size)
        if opcode in (0x9, 0xA):
            continue
        if opcode == 0x8:
            raise EOFError("the peer closed the connection instead of answering")
        return opcode, body


try:
    sock = socket.create_connection(("127.0.0.1", int(port)), timeout=10)
    sock.settimeout(10)
    request = (
        "GET %s HTTP/1.1\r\n"
        "Host: 127.0.0.1:%s\r\n"
        "Upgrade: websocket\r\n"
        "Connection: Upgrade\r\n"
        "Sec-WebSocket-Key: %s\r\n"
        "Sec-WebSocket-Version: 13\r\n"
        "x-deis-app-id: %s\r\n"
        "\r\n" % (path, port, key, app)
    )
    sock.sendall(request.encode("ascii"))

    head = read_until_headers_end(sock)
    lines = head.split("\r\n")
    report["status_line"] = lines[0]
    headers = {}
    for line in lines[1:]:
        if ":" in line:
            name, value = line.split(":", 1)
            headers[name.strip().lower()] = value.strip()
    report["accept"] = headers.get("sec-websocket-accept", "")
    report["upgrade"] = headers.get("upgrade", "")
    report["connection"] = headers.get("connection", "")
    report["switched"] = (
        report["status_line"].endswith("101 Switching Protocols")
        and report["upgrade"].lower() == "websocket"
        and report["connection"].lower() == "upgrade"
        and report["accept"] == accept_expected
    )

    if report["switched"]:
        send_frame(sock, 0x1, text.encode("utf-8"))
        report["text_echo"] = read_frame(sock)[1].decode("utf-8", "replace")
        send_frame(sock, 0x2, payload)
        report["binary_echo"] = read_frame(sock)[1].decode("utf-8", "replace")
        report["text_expected"] = "text:" + text
        report["binary_expected"] = "binary:%d:%s" % (len(payload), binary_hex)
        report["echoed"] = (
            report["text_echo"] == report["text_expected"]
            and report["binary_echo"] == report["binary_expected"]
        )
        # A close frame, so the node sees a closing handshake rather than a
        # connection that vanished and logs an error about it.
        send_frame(sock, 0x8, struct.pack("!H", 1000))
    report["ok"] = report["switched"] and report.get("echoed", False)
    sock.close()
except Exception as failure:
    report["error"] = "%s: %s" % (type(failure).__name__, failure)

print(json.dumps(report))
PY
}

# One field out of the exchange report, empty when it is not there.
ws_field() {
  jq -r "$2 // empty" < "$1" 2>/dev/null || true
}

check_websocket_through_proxy() {
  local switched="a WebSocket handshake through deis-proxy switches protocols"
  local echoed="a text frame and a binary frame survive the round trip through deis-proxy"
  local report="${LOG_DIR}/verify-websocket.json"

  if ! command -v python3 >/dev/null 2>&1; then
    fail_check "$switched" "python3 is not installed, so no handshake was made"
    fail_check "$echoed" "python3 is not installed, so no frame was sent"
    return
  fi

  websocket_exchange "$DEIS_PROXY_PORT" \
    > "$report" 2> "${LOG_DIR}/verify-websocket.log" || true

  local error
  error=$(ws_field "$report" '.error')
  if [ -n "$error" ]; then
    fail_check "$switched" "127.0.0.1:${DEIS_PROXY_PORT}${WS_ROUTE} -> ${error}"
    fail_check "$echoed" "there was no connection to send a frame over"
    return
  fi

  if [ "$(ws_field "$report" '.switched')" = "true" ]; then
    pass_check "$switched" \
      "$(ws_field "$report" '.status_line'), Upgrade: $(ws_field "$report" '.upgrade'), Connection: $(ws_field "$report" '.connection'), Sec-WebSocket-Accept: $(ws_field "$report" '.accept') -- the RFC 6455 answer to the key that was sent, so the 101 came from the node and was relayed, not invented by the proxy"
  else
    local hint=""
    # The handler is written into the staged sources at `start`, so a demo
    # started before this check existed serves a 404 here and the reason is
    # worth saying rather than leaving to be guessed.
    case "$(ws_field "$report" '.status_line')" in
      *404*) hint="; ${WS_ROUTE} is not in the deployed bundle, so this demo was started before the check existed - run \`demo.sh stop\` and \`demo.sh start\` again" ;;
    esac
    fail_check "$switched" \
      "got \"$(ws_field "$report" '.status_line')\" with Sec-WebSocket-Accept \"$(ws_field "$report" '.accept')\", expected \"HTTP/1.1 101 Switching Protocols\" and \"${WS_ACCEPT}\"${hint}"
    fail_check "$echoed" "the handshake did not complete, so no frame was sent"
    return
  fi

  if [ "$(ws_field "$report" '.echoed')" = "true" ]; then
    pass_check "$echoed" \
      "text \"${WS_TEXT}\" -> \"$(ws_field "$report" '.text_echo')\"; binary ${WS_BINARY_HEX} ($(( ${#WS_BINARY_HEX} / 2 )) bytes that are not valid UTF-8: 0xff and 0xfe never occur in it and 0x80 has no lead byte in front of it) -> \"$(ws_field "$report" '.binary_echo')\""
  else
    fail_check "$echoed" \
      "text -> \"$(ws_field "$report" '.text_echo')\", expected \"$(ws_field "$report" '.text_expected')\"; binary -> \"$(ws_field "$report" '.binary_echo')\", expected \"$(ws_field "$report" '.binary_expected')\""
  fi
}

# Read a streaming response over a raw socket, recording when each piece of the
# body arrived.
#
# A socket rather than curl because the whole claim is about *timing and
# framing*, and curl reports neither: what has to be shown is that the head
# arrived long before the last byte, that each frame arrived on its own, that
# the body is chunked, and -- for the failure cases -- that the terminating
# zero-length chunk never came. All four are visible here and nowhere else.
#
# Arguments: port, app id, request target, how long to keep reading (ms), how
# many frames to stop after (0 = until the body ends), and what to offer in
# Accept-Encoding.
#
# Stopping after N frames closes the socket where it stands, which is exactly
# what a client hanging up looks like to the node.
stream_exchange() {
  python3 - "$1" "$2" "$3" "$4" "$5" "$6" <<'PYSTREAM'
import json
import socket
import sys
import time

port = int(sys.argv[1])
app_id = sys.argv[2]
target = sys.argv[3]
max_ms = int(sys.argv[4])
stop_after = int(sys.argv[5])
accept_encoding = sys.argv[6]

report = {
    "status_line": "",
    "headers": {},
    "frames": [],
    "clean_end": False,
    "error": None,
}
started = time.monotonic()


def ms():
    return int((time.monotonic() - started) * 1000)


try:
    sock = socket.create_connection(("127.0.0.1", port), timeout=5)
    request = (
        "GET %s HTTP/1.1\r\n"
        "Host: 127.0.0.1\r\n"
        "x-deis-app-id: %s\r\n"
        "Accept-Encoding: %s\r\n"
        "Accept: text/event-stream\r\n"
        "Connection: close\r\n\r\n" % (target, app_id, accept_encoding)
    )
    sock.sendall(request.encode("utf-8"))
    sock.settimeout(0.2)

    buf = b""
    closed = False

    def read_more():
        # True: something arrived. False: the peer closed. None: nothing yet.
        global buf, closed
        try:
            data = sock.recv(65536)
        except socket.timeout:
            return None
        if not data:
            closed = True
            return False
        buf += data
        return True

    while b"\r\n\r\n" not in buf:
        if ms() > max_ms:
            raise RuntimeError("no response header within %dms" % max_ms)
        if read_more() is False:
            raise RuntimeError("the connection closed before any header arrived")

    head, buf = buf.split(b"\r\n\r\n", 1)
    lines = head.decode("latin1").split("\r\n")
    report["status_line"] = lines[0]
    report["header_ms"] = ms()
    for line in lines[1:]:
        if ":" in line:
            name, value = line.split(":", 1)
            report["headers"][name.strip().lower()] = value.strip()
    chunked = report["headers"].get("transfer-encoding", "").lower() == "chunked"
    report["chunked"] = chunked

    def record(payload):
        report["frames"].append(
            {"at_ms": ms(), "text": payload.decode("utf-8", "replace")}
        )

    while ms() <= max_ms:
        if stop_after and len(report["frames"]) >= stop_after:
            break
        if chunked:
            if b"\r\n" not in buf:
                if read_more() is False:
                    break
                continue
            size_line, rest = buf.split(b"\r\n", 1)
            try:
                size = int(size_line.split(b";")[0], 16)
            except ValueError:
                report["error"] = "unparsable chunk size %r" % size_line[:32]
                break
            if size == 0:
                report["clean_end"] = True
                break
            if len(rest) < size + 2:
                if read_more() is False:
                    break
                continue
            record(rest[:size])
            buf = rest[size + 2 :]
        else:
            if buf:
                record(buf)
                buf = b""
                continue
            state = read_more()
            if state is False:
                # No chunked framing: the body was delimited by the close, so
                # reaching it is the end of a complete body.
                report["clean_end"] = True
                break
    report["timed_out"] = ms() > max_ms
    sock.close()
except Exception as failure:
    report["error"] = "%s: %s" % (type(failure).__name__, failure)

print(json.dumps(report))
PYSTREAM
}

# One field out of a streaming report, empty when it is not there.
stream_field() {
  jq -r "$2 // empty" < "$1" 2>/dev/null || true
}

# The frames a report collected, as "n@Xms" for the evidence string.
stream_frame_times() {
  jq -r '[.frames[] | "\(.at_ms)ms"] | join(", ")' < "$1" 2>/dev/null || true
}

check_server_sent_events_reach_the_client_frame_by_frame() {
  local title="server-sent events reach a client through deis-proxy frame by frame"
  local report="${LOG_DIR}/verify-sse.json"
  local frames=5
  local gap=150

  if ! command -v python3 >/dev/null 2>&1; then
    fail_check "$title" "python3 is not installed, so no stream was opened"
    return
  fi

  stream_exchange "$DEIS_PROXY_PORT" "$APP_ID" \
    "${STREAM_ROUTE}?mode=sse&frames=${frames}&gapMs=${gap}&tag=sse" \
    6000 0 "gzip" > "$report" 2>"${LOG_DIR}/verify-sse.log" || true

  local error
  error=$(stream_field "$report" '.error')
  if [ -n "$error" ]; then
    fail_check "$title" "127.0.0.1:${DEIS_PROXY_PORT}${STREAM_ROUTE} -> ${error}"
    return
  fi

  local status ctype count first last header_ms clean
  status=$(stream_field "$report" '.status_line')
  ctype=$(stream_field "$report" '.headers["content-type"]')
  count=$(jq '.frames | length' < "$report")
  first=$(jq -r '.frames[0].at_ms // -1' < "$report")
  last=$(jq -r '.frames[-1].at_ms // -1' < "$report")
  header_ms=$(stream_field "$report" '.header_ms')
  clean=$(stream_field "$report" '.clean_end')

  # The gap the handler was asked for is what makes this checkable: with
  # ${frames} frames ${gap}ms apart the last one cannot exist until
  # $(( (frames - 1) * gap ))ms after the first, so a body that had been
  # collected before the head went out could not show this spread.
  local spread=$(( last - first ))
  local expected=$(( (frames - 1) * gap * 2 / 3 ))

  case "$status" in
    *200*) ;;
    *)
      fail_check "$title" "got \"${status}\" instead of HTTP 200; a demo started before this check existed serves 404 here - run \`demo.sh stop\` and \`demo.sh start\` again"
      return
      ;;
  esac

  if [ "$count" = "$frames" ] && [ "$clean" = "true" ] && [ "$spread" -ge "$expected" ]; then
    pass_check "$title" \
      "${status}, content-type ${ctype}; the head arrived at ${header_ms}ms and the ${count} frames at $(stream_frame_times "$report") - ${spread}ms apart end to end, for a handler that pauses ${gap}ms between them. The body ended with its terminating chunk, so the client saw a complete response"
  else
    fail_check "$title" \
      "expected ${frames} frames spread over at least ${expected}ms and a clean end; got ${count} frame(s) at $(stream_frame_times "$report"), spread ${spread}ms, clean_end=${clean}"
  fi
}

check_a_streamed_response_is_chunked_and_left_uncompressed() {
  local title="a streamed response crosses deis-proxy chunked and uncompressed"
  local report="${LOG_DIR}/verify-sse.json"

  if [ ! -s "$report" ]; then
    fail_check "$title" "the events check produced no report to read"
    return
  fi

  local chunked length encoding
  chunked=$(stream_field "$report" '.chunked')
  length=$(stream_field "$report" '.headers["content-length"]')
  encoding=$(stream_field "$report" '.headers["content-encoding"]')

  # Both halves matter and they fail differently. A Content-Length would mean
  # the node knew the size, which it can only do by having collected the body.
  # A Content-Encoding would mean the proxy compressed it - and pingora's
  # compressors are not flushed per chunk, so a 40-byte frame handed to gzip
  # comes back out as nothing and the client waits for bytes the proxy is
  # holding.
  if [ "$chunked" = "true" ] && [ -z "$length" ] && [ -z "$encoding" ]; then
    pass_check "$title" \
      "Transfer-Encoding: chunked, no Content-Length, and no Content-Encoding even though the request offered \"Accept-Encoding: gzip\" - so nothing between the isolate and the client is holding the body to measure or compress it"
  else
    fail_check "$title" \
      "chunked=${chunked}, Content-Length=\"${length}\", Content-Encoding=\"${encoding}\"; a length means the body was collected and an encoding means the proxy is buffering it inside a compressor"
  fi
}

check_a_stream_fed_from_fetch_is_not_collected() {
  local title="a body taken from fetch() is passed on without being collected"
  local report="${LOG_DIR}/verify-stream-relay.json"
  local pieces=6
  local gap=150

  if ! command -v python3 >/dev/null 2>&1; then
    fail_check "$title" "python3 is not installed, so no stream was opened"
    return
  fi
  if [ -z "${DEIS_ORIGIN_PORT:-}" ]; then
    fail_check "$title" "the demo's origin server is not running, so there is nothing to relay"
    return
  fi

  # The upstream writes its pieces slowly and announces no length. A handler
  # that read the body before answering could not send its first byte until the
  # upstream had sent its last, so the two timings below are the whole proof.
  local upstream="http%3A//127.0.0.1%3A${DEIS_ORIGIN_PORT}/slow%3Fpieces%3D${pieces}%26gapMs%3D${gap}"
  stream_exchange "$DEIS_PROXY_PORT" "$APP_ID" \
    "${STREAM_ROUTE}?mode=relay&url=${upstream}" \
    8000 0 "gzip" > "$report" 2>"${LOG_DIR}/verify-stream-relay.log" || true

  local error
  error=$(stream_field "$report" '.error')
  if [ -n "$error" ]; then
    fail_check "$title" "${error}"
    return
  fi

  local status body count first last
  status=$(stream_field "$report" '.status_line')
  body=$(jq -r '[.frames[].text] | join("")' < "$report")
  count=$(jq '.frames | length' < "$report")
  first=$(jq -r '.frames[0].at_ms // -1' < "$report")
  last=$(jq -r '.frames[-1].at_ms // -1' < "$report")
  local spread=$(( last - first ))
  local expected=$(( (pieces - 1) * gap * 2 / 3 ))

  # Built the same way the body was read, so the trailing newline the upstream
  # sends is stripped from both sides rather than from one of them.
  local wanted
  wanted=$(seq 1 "$pieces" | sed 's/^/piece /')

  if [ "$status" != "${status#*200}" ] && [ "$body" = "$wanted" ] && [ "$spread" -ge "$expected" ]; then
    pass_check "$title" \
      "the handler returned new Response(upstream.body) and the ${pieces} pieces reached the client at $(stream_frame_times "$report") - ${spread}ms end to end, matching the pace the upstream wrote them at. The application's manifest asks for a 32MB heap and it never held the object at all"
  else
    fail_check "$title" \
      "expected ${pieces} pieces spread over at least ${expected}ms; got ${count} frame(s) spread ${spread}ms, status \"${status}\", body $(printf '%s' "$body" | tr '\n' '/' | head -c 200)"
  fi
}

check_a_stream_stops_when_the_client_hangs_up() {
  local title="a client that hangs up mid-stream stops the producer"
  local report="${LOG_DIR}/verify-stream-hangup.json"
  local tag="hangup-$$"

  if ! command -v python3 >/dev/null 2>&1; then
    fail_check "$title" "python3 is not installed, so no stream was opened"
    return
  fi

  # Two frames and then the socket is closed where it stands, which is what a
  # visitor closing a tab looks like from inside the node.
  stream_exchange "$DEIS_RUNTIME_PORT" "$APP_ID" \
    "${STREAM_ROUTE}?mode=endless&gapMs=80&tag=${tag}" \
    6000 2 "identity" > "$report" 2>"${LOG_DIR}/verify-stream-hangup.log" || true

  local frames
  frames=$(jq '.frames | length' < "$report")
  if [ "$frames" -lt 2 ]; then
    fail_check "$title" "the stream produced ${frames} frame(s) before the client left, so there was nothing to interrupt: $(stream_field "$report" '.error')"
    return
  fi

  # Poll the application until its producer says it was cancelled. The isolate
  # is reused across requests, so the handler that was cancelled can be asked
  # about it by a later one; polling the fact itself rather than waiting a
  # fixed time is what keeps this from being a race in either direction.
  local waited=0 cancelled="" produced_first="" produced_second=""
  while [ "$waited" -lt 100 ]; do
    cancelled=$(app_request "${LOG_DIR}/verify-stream-hangup.headers" \
      "${STREAM_ROUTE}?mode=report" | jq -r '.lastCancelled // empty')
    [ "$cancelled" = "$tag" ] && break
    waited=$(( waited + 1 ))
  done

  if [ "$cancelled" != "$tag" ]; then
    fail_check "$title" \
      "the producer was never told its client had gone: after ${waited} polls the isolate still reports lastCancelled=\"${cancelled}\". It would go on filling a queue nobody reads until stream_timeout_ms"
    return
  fi

  produced_first=$(app_request "${LOG_DIR}/verify-stream-hangup.headers" \
    "${STREAM_ROUTE}?mode=report" | jq -r '.framesProduced')
  produced_second=$(app_request "${LOG_DIR}/verify-stream-hangup.headers" \
    "${STREAM_ROUTE}?mode=report" | jq -r '.framesProduced')

  if [ "$produced_first" = "$produced_second" ]; then
    pass_check "$title" \
      "the client read ${frames} frame(s) and closed the socket; the handler's cancel() ran with tag ${tag}, and the frame counter stood still at ${produced_first} across two later requests - the producer stopped rather than writing into nothing"
  else
    fail_check "$title" \
      "cancel() ran, but the producer kept going: the frame counter moved from ${produced_first} to ${produced_second} after the client had gone"
  fi
}

check_a_producer_that_throws_leaves_a_failed_transfer() {
  local title="a producer that throws part-way leaves the client a failed transfer"
  local report="${LOG_DIR}/verify-stream-throw.json"

  if ! command -v python3 >/dev/null 2>&1; then
    fail_check "$title" "python3 is not installed, so no stream was opened"
    return
  fi

  stream_exchange "$DEIS_PROXY_PORT" "$APP_ID" \
    "${STREAM_ROUTE}?mode=throw&after=2&gapMs=80&tag=throw" \
    6000 0 "identity" > "$report" 2>"${LOG_DIR}/verify-stream-throw.log" || true

  local status frames clean
  status=$(stream_field "$report" '.status_line')
  frames=$(jq '.frames | length' < "$report")
  clean=$(stream_field "$report" '.clean_end')

  # 200 and an incomplete body is the only honest ending available: the status
  # line left the node before the handler failed, so there is no 500 left to
  # send. What must not happen is a *clean* end, which the client would read as
  # a short but complete answer.
  if [ "$status" != "${status#*200}" ] && [ "$frames" -eq 2 ] && [ "$clean" != "true" ]; then
    pass_check "$title" \
      "${status} went out first, the 2 frames the handler managed arrived at $(stream_frame_times "$report"), and then the body ended without its terminating chunk - so the client's read fails instead of succeeding on half an answer"
  else
    fail_check "$title" \
      "expected HTTP 200, 2 frames and no terminating chunk; got \"${status}\", ${frames} frame(s), clean_end=${clean}"
  fi
}

check_a_stream_past_its_budget_is_closed_by_the_node() {
  local title="a stream that runs past stream_timeout_ms is closed by the node"
  local report="${LOG_DIR}/verify-stream-budget.json"
  local tag="budget-$$"

  if ! command -v python3 >/dev/null 2>&1; then
    fail_check "$title" "python3 is not installed, so no stream was opened"
    return
  fi

  # Read for well past the ceiling. The node is what has to end this, and the
  # client keeps asking until it does.
  local patience=$(( DEMO_STREAM_TIMEOUT_MS * 3 ))
  stream_exchange "$DEIS_RUNTIME_PORT" "$APP_ID" \
    "${STREAM_ROUTE}?mode=endless&gapMs=120&tag=${tag}" \
    "$patience" 0 "identity" > "$report" 2>"${LOG_DIR}/verify-stream-budget.log" || true

  local frames last clean timed_out
  frames=$(jq '.frames | length' < "$report")
  last=$(jq -r '.frames[-1].at_ms // -1' < "$report")
  clean=$(stream_field "$report" '.clean_end')
  timed_out=$(stream_field "$report" '.timed_out')

  local floor=$(( DEMO_STREAM_TIMEOUT_MS / 2 ))
  local ceiling=$(( DEMO_STREAM_TIMEOUT_MS * 2 ))
  local logged
  logged=$(grep -c "is past stream_timeout_ms" "${LOG_DIR}/deis-runtime.log" 2>/dev/null || echo 0)

  if [ "$timed_out" != "true" ] && [ "$clean" != "true" ] \
    && [ "$last" -ge "$floor" ] && [ "$last" -le "$ceiling" ] && [ "$logged" -ge 1 ]; then
    pass_check "$title" \
      "an endless stream ran for ${last}ms against the ${DEMO_STREAM_TIMEOUT_MS}ms ceiling this node is configured with, delivered ${frames} frame(s), and was then ended without its terminating chunk. The node's log says why, at warn, ${logged} time(s): \"is past stream_timeout_ms\""
  else
    fail_check "$title" \
      "expected the node to close the stream between ${floor}ms and ${ceiling}ms and to say so; last frame at ${last}ms, clean_end=${clean}, client gave up=${timed_out}, log lines=${logged}"
  fi
}

check_a_request_is_served_while_a_stream_is_open() {
  local title="another request is answered while a stream is still open"
  local report="${LOG_DIR}/verify-stream-parallel.json"
  local tag="parallel-$$"

  if ! command -v python3 >/dev/null 2>&1; then
    fail_check "$title" "python3 is not installed, so no stream was opened"
    return
  fi

  # The application counts every frame it has ever produced, so "the stream is
  # producing right now" is a fact that can be asked for rather than waited
  # out. Each poll below is a real request to the same isolate, which is also
  # what paces the loop.
  local before
  before=$(app_request "${LOG_DIR}/verify-stream-parallel.headers" \
    "${STREAM_ROUTE}?mode=report" | jq -r '.framesProduced')

  # Held open in the background for long enough to make an ordinary request
  # underneath it. Both go to the same application, which on this node is one
  # isolate on one thread -- the arrangement that would let a slow reader block
  # everybody if a stream owned the isolate for its lifetime.
  stream_exchange "$DEIS_RUNTIME_PORT" "$APP_ID" \
    "${STREAM_ROUTE}?mode=endless&gapMs=200&tag=${tag}" \
    4000 0 "identity" > "$report" 2>"${LOG_DIR}/verify-stream-parallel.log" &
  local holder=$!

  local waited=0 producing=""
  while [ "$waited" -lt 60 ]; do
    producing=$(app_request "${LOG_DIR}/verify-stream-parallel.headers" \
      "${STREAM_ROUTE}?mode=report" | jq -r '.framesProduced')
    [ -n "$producing" ] && [ "$producing" -gt "$before" ] && break
    waited=$(( waited + 1 ))
  done

  if [ -z "$producing" ] || [ "$producing" -le "$before" ]; then
    kill "$holder" 2>/dev/null || true
    wait "$holder" 2>/dev/null || true
    fail_check "$title" \
      "the stream never started producing: the frame counter stayed at ${before} across ${waited} requests. $(stream_field "$report" '.error')"
    return
  fi

  local headers="${LOG_DIR}/verify-stream-parallel.headers"
  local started ended body status length
  started=$(date +%s%N)
  body=$(app_request "$headers" "/hello/parallel")
  ended=$(date +%s%N)
  status=$(http_status "$headers")
  length=$(header_value "$headers" "content-length")
  local took=$(( (ended - started) / 1000000 ))

  # Still running means the stream was open for the whole of the request above,
  # which is the claim: the answer did not have to wait for the stream to end.
  local still_open=no
  kill -0 "$holder" 2>/dev/null && still_open=yes

  wait "$holder" 2>/dev/null || true
  local frames
  frames=$(jq '.frames | length' < "$report")

  if [ "$status" = "200" ] && [ "$still_open" = "yes" ] && [ -n "$length" ]; then
    pass_check "$title" \
      "the isolate had a stream open and producing (the frame counter had moved from ${before} to ${producing}) when GET /hello/parallel answered ${status} in ${took}ms with Content-Length ${length} - an ordinary answer on the ordinary road. The stream was still open afterwards and went on to deliver ${frames} frames"
  else
    fail_check "$title" \
      "GET /hello/parallel answered \"${status}\" in ${took}ms (Content-Length \"${length}\", stream still open: ${still_open}) while the stream delivered ${frames} frame(s); a stream that owned the isolate would have queued this request behind it. Body: $(printf '%s' "$body" | head -c 200)"
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
  if ! command -v openssl >/dev/null 2>&1; then
    fail_check "$title" \
      "PUT then GET returned ${value} from the node, but openssl is not installed so this check could not mint the capability token deis-store now requires to be asked directly"
    return
  fi

  # deis-store's KvStore.Get answers `value` as bytes (GetResponse.value is
  # `bytes`, not `string` - a KV value is whatever the application wrote), so
  # grpcurl's JSON rendering of it is base64, same as any protobuf bytes
  # field. Decode before comparing, or every value that happens not to equal
  # its own base64 form would fail this check regardless of whether
  # deis-store actually holds the right bytes.
  local tenant="${APP_ID}.${ENV_NAME}"
  local direct direct_value
  direct=$(grpcurl -plaintext -max-time 5 \
    -import-path "${ROOT_DIR}/crates/deis-store/proto" -proto storage.proto \
    -H "x-deis-tenant: ${tenant}" \
    -H "x-deis-storage-token: $(storage_token "$tenant")" \
    -d "{\"key\":\"${key}\"}" \
    "127.0.0.1:${DEIS_STORE_PORT}" deis.storage.KvStore/Get 2>&1)
  direct_value=$(echo "$direct" | jq -r '.value // empty' 2>/dev/null | openssl base64 -d -A 2>/dev/null || true)

  if [ "$direct_value" = "$value" ]; then
    pass_check "$title" \
      "PUT then GET returned ${value} from the node, and deis-store's own KvStore.Get answers the same value directly for tenant ${tenant}, key ${key}"
  else
    fail_check "$title" \
      "the node answered ${value}, but deis-store's own KvStore.Get for tenant ${tenant}, key ${key} returned: $(echo "$direct" | tr '\n' ' ' | head -c 200)"
  fi
}

# The other half of the capability-token feature: not that a call carrying
# the right token succeeds (check_kv_roundtrip's direct call already proves
# that, now that this demo configures auth_secret), but that a call carrying
# none is refused before it touches any data. Without this check, `start`
# could configure auth_secret on none of the three places it belongs and
# every check above would still pass - loopback alone would still be
# standing in for the whole of it, silently, which is exactly the gap
# crates/deis-store/src/auth.rs exists to close.
# ---- named bindings ---------------------------------------------------
#
# The claim: an application can declare any number of namespaces of a kind,
# reach each by the name it chose, and never have two of them touch. The
# checks below prove it from three sides -- from inside the isolate, from
# deis-store's own service, and from the CLI -- because the first alone would
# be satisfied by a node that kept three maps in memory.

check_named_bindings_are_separate_namespaces() {
  local title="two named kv bindings of one application never see each other"
  local key="binding-probe-$$"
  local headers="${LOG_DIR}/verify-binding.headers"

  # The same key, written through three namespaces of the one tenant.
  app_request "$headers" "/api/bindings/HOT?key=${key}" -X PUT --data-binary "hot-value" >/dev/null
  if [ "$(http_status "$headers")" != "201" ]; then
    fail_check "$title" "PUT through HOT answered $(http_status "$headers")"
    return
  fi
  app_request "$headers" "/api/bindings/COLD?key=${key}" -X PUT --data-binary "cold-value" >/dev/null
  app_request "$headers" "/api/bindings/default?key=${key}" -X PUT --data-binary "unnamed-value" >/dev/null

  local hot cold unnamed hot_ns same_object
  hot=$(app_request "${LOG_DIR}/verify-binding-hot.headers" "/api/bindings/HOT?key=${key}")
  cold=$(app_request "${LOG_DIR}/verify-binding-cold.headers" "/api/bindings/COLD?key=${key}")
  unnamed=$(app_request "${LOG_DIR}/verify-binding-plain.headers" "/api/bindings/default?key=${key}")
  hot_ns=$(echo "$hot" | jq -r '.namespace // empty')
  same_object=$(echo "$hot" | jq -r '.sameObjectOnEnv // empty')

  if [ "$(echo "$hot" | jq -r '.value // empty')" != "hot-value" ] \
    || [ "$(echo "$cold" | jq -r '.value // empty')" != "cold-value" ] \
    || [ "$(echo "$unnamed" | jq -r '.value // empty')" != "unnamed-value" ]; then
    fail_check "$title" \
      "the same key read back differently per namespace was expected; HOT=$(echo "$hot" | jq -c '.value'), COLD=$(echo "$cold" | jq -c '.value'), unnamed=$(echo "$unnamed" | jq -c '.value')"
    return
  fi
  if [ "$same_object" != "true" ] || [ "$hot_ns" != "HOT" ]; then
    fail_check "$title" \
      "env.HOT and context.bindings.HOT must be one object and the object must know its own name; sameObjectOnEnv=${same_object}, namespace=${hot_ns}"
    return
  fi

  # Reading it back through the node proves the isolate got three answers, not
  # where they are kept. deis-store's own KvStore.Get, with the binding header
  # this platform stamps, is what proves the three are three namespaces on
  # disk -- and the same call without the header is what proves the unnamed
  # one is exactly where it always was.
  if ! command -v grpcurl >/dev/null 2>&1 || ! command -v openssl >/dev/null 2>&1; then
    fail_check "$title" \
      "the three namespaces answered hot-value / cold-value / unnamed-value through the node, but grpcurl or openssl is missing so deis-store could not be asked directly"
    return
  fi

  local tenant token direct_hot direct_plain
  tenant="${APP_ID}.${ENV_NAME}"
  token=$(storage_token "$tenant")
  direct_hot=$(grpcurl -plaintext -max-time 5 \
    -import-path "${ROOT_DIR}/crates/deis-store/proto" -proto storage.proto \
    -H "x-deis-tenant: ${tenant}" -H "x-deis-storage-token: ${token}" \
    -H "x-deis-binding: HOT" \
    -d "{\"key\":\"${key}\"}" \
    "127.0.0.1:${DEIS_STORE_PORT}" deis.storage.KvStore/Get 2>&1 \
    | jq -r '.value // empty' 2>/dev/null | openssl base64 -d -A 2>/dev/null || true)
  direct_plain=$(grpcurl -plaintext -max-time 5 \
    -import-path "${ROOT_DIR}/crates/deis-store/proto" -proto storage.proto \
    -H "x-deis-tenant: ${tenant}" -H "x-deis-storage-token: ${token}" \
    -d "{\"key\":\"${key}\"}" \
    "127.0.0.1:${DEIS_STORE_PORT}" deis.storage.KvStore/Get 2>&1 \
    | jq -r '.value // empty' 2>/dev/null | openssl base64 -d -A 2>/dev/null || true)

  if [ "$direct_hot" = "hot-value" ] && [ "$direct_plain" = "unnamed-value" ]; then
    pass_check "$title" \
      "key ${key} written through HOT, COLD and context.services.kv reads back as hot-value, cold-value and unnamed-value; deis-store's own KvStore.Get for tenant ${tenant} answers hot-value with x-deis-binding: HOT and unnamed-value with no binding header at all, so the named namespaces are beside the unnamed one and not on top of it"
  else
    fail_check "$title" \
      "deis-store answered ${direct_hot:-<nothing>} for tenant ${tenant} with binding HOT and ${direct_plain:-<nothing>} with no binding; expected hot-value and unnamed-value"
  fi
}

check_a_binding_cannot_reach_another_tenant() {
  local title="a binding name cannot be made to address another tenant"

  if ! command -v grpcurl >/dev/null 2>&1 || ! command -v openssl >/dev/null 2>&1; then
    fail_check "$title" "grpcurl or openssl is missing, so this check could not be run"
    return
  fi

  local tenant token
  tenant="${APP_ID}.${ENV_NAME}"
  token=$(storage_token "$tenant")

  # Every shape that would have to work for a binding to escape its tenant:
  # the tenant separator, the namespace separator, a traversal, and a name
  # long enough to run past the ceiling. deis-store refuses each before it
  # touches anything on disk, because the composed value is validated as a
  # whole and neither half may contain the other's separator.
  local hostile refused=0 total=0 sample=""
  for hostile in ".." "a.b" "a@b" "../../etc" "$(printf 'x%.0s' $(seq 1 65))"; do
    total=$((total + 1))
    local answer
    answer=$(grpcurl -plaintext -max-time 5 \
      -import-path "${ROOT_DIR}/crates/deis-store/proto" -proto storage.proto \
      -H "x-deis-tenant: ${tenant}" -H "x-deis-storage-token: ${token}" \
      -H "x-deis-binding: ${hostile}" \
      -d '{"key":"anything"}' \
      "127.0.0.1:${DEIS_STORE_PORT}" deis.storage.KvStore/Get 2>&1 || true)
    if echo "$answer" | grep -qi "namespace id"; then
      refused=$((refused + 1))
      [ -z "$sample" ] && sample=$(echo "$answer" | tr '\n' ' ' | head -c 160)
    fi
  done

  # And the token itself: it is minted for the tenant, so it does not become a
  # different token for a different binding. A call naming another tenant is
  # refused whatever binding it asks for.
  local cross
  cross=$(grpcurl -plaintext -max-time 5 \
    -import-path "${ROOT_DIR}/crates/deis-store/proto" -proto storage.proto \
    -H "x-deis-tenant: ${CALLEE_APP_ID}.${ENV_NAME}" -H "x-deis-storage-token: ${token}" \
    -H "x-deis-binding: HOT" \
    -d '{"key":"anything"}' \
    "127.0.0.1:${DEIS_STORE_PORT}" deis.storage.KvStore/Get 2>&1 || true)

  if [ "$refused" -eq "$total" ] && echo "$cross" | grep -qi "unauthenticated\|token"; then
    pass_check "$title" \
      "deis-store refused all ${total} hostile binding names for tenant ${tenant} (\"${sample}\"), and the capability token minted for ${tenant} was refused when it named ${CALLEE_APP_ID}.${ENV_NAME} even with a legal binding: the token authorises the tenant and the binding only chooses a namespace inside it"
    return
  fi
  fail_check "$title" \
    "${refused} of ${total} hostile binding names were refused, and the cross-tenant call answered: $(echo "$cross" | tr '\n' ' ' | head -c 200)"
}

check_cli_reads_a_named_binding() {
  local title="deis kv reads the namespace a binding names, and says which one"
  local key="cli-binding-$$"

  if ! deis_cli kv put "$key" "written-through-hot" --binding HOT -e "$ENV_NAME" \
    > "${LOG_DIR}/verify-cli-binding-put.log" 2>&1; then
    fail_check "$title" "deis kv put --binding HOT failed: $(tail -n 5 "${LOG_DIR}/verify-cli-binding-put.log")"
    return
  fi

  local hot unnamed
  hot=$(deis_cli kv get "$key" --binding HOT -e "$ENV_NAME" --json 2>/dev/null || true)
  unnamed=$(deis_cli kv get "$key" -e "$ENV_NAME" --json 2>/dev/null || true)

  # `.found` is read without a `//` fallback: jq treats `false` as unset, so
  # `.found // empty` would turn the very answer this check is looking for
  # into no answer at all. The value comes back base64 because a KV value is
  # bytes and this CLI never puts raw bytes on a terminal.
  local hot_value hot_tenant unnamed_found
  hot_value=$(echo "$hot" | jq -r '.value_base64 // empty' 2>/dev/null \
    | openssl base64 -d -A 2>/dev/null || true)
  hot_tenant=$(echo "$hot" | jq -r '.tenant // empty' 2>/dev/null || true)
  unnamed_found=$(echo "$unnamed" | jq -r 'if has("found") then .found else "" end' 2>/dev/null || true)

  # The same key through the isolate's own HOT binding, so the CLI and the
  # application are shown to be looking at one namespace rather than two that
  # happen to answer.
  local through_app
  through_app=$(app_request "${LOG_DIR}/verify-cli-binding-app.headers" \
    "/api/bindings/HOT?key=${key}" | jq -r '.value // empty' || true)

  if [ "$hot_value" = "written-through-hot" ] \
    && [ "$hot_tenant" = "${APP_ID}.${ENV_NAME}@HOT" ] \
    && [ "$unnamed_found" = "false" ] \
    && [ "$through_app" = "written-through-hot" ]; then
    pass_check "$title" \
      "deis kv put --binding HOT wrote ${key}; reading it back with --binding HOT gives written-through-hot and reports the namespace as ${hot_tenant}, the same command without --binding reports found=false, and the application reading ctx.bindings.HOT sees the CLI's value"
  else
    fail_check "$title" \
      "--binding HOT read ${hot_value:-<nothing>} in namespace ${hot_tenant:-<none>}, the unnamed namespace reported found=${unnamed_found:-<none>}, and the app read ${through_app:-<nothing>}"
  fi
}

# ---- service bindings -------------------------------------------------

check_one_application_calls_another() {
  local title="an application calls another application on this platform"
  local headers="${LOG_DIR}/verify-service-call.headers"
  local body
  body=$(app_request "$headers" "/api/call")

  local status called_by note received app environment
  status=$(echo "$body" | jq -r '.status // empty' 2>/dev/null || true)
  app=$(echo "$body" | jq -r '.app // empty' 2>/dev/null || true)
  environment=$(echo "$body" | jq -r '.environment // empty' 2>/dev/null || true)
  called_by=$(echo "$body" | jq -r '.body | fromjson | .calledBy // empty' 2>/dev/null || true)
  note=$(echo "$body" | jq -r '.body | fromjson | .note // empty' 2>/dev/null || true)
  received=$(echo "$body" | jq -r '.body | fromjson | .received // empty' 2>/dev/null || true)

  if [ "$(http_status "$headers")" = "200" ] \
    && [ "$status" = "202" ] \
    && [ "$called_by" = "${APP_ID}.${ENV_NAME}" ] \
    && [ "$note" = "sent by the caller" ] \
    && [ "$received" = "ticket-from-v1" ] \
    && [ "$app" = "$CALLEE_APP_ID" ] \
    && [ "$environment" = "$ENV_NAME" ]; then
    pass_check "$title" \
      "${APP_ID} reached ${CALLEE_APP_ID}.${ENV_NAME} through its AUTH binding with no address anywhere: the callee ran its own handler, answered its own 202, saw the body and the header the caller sent, and read x-deis-caller as ${called_by} -- which the caller had set to \"a-caller-that-is-not-me\" and the platform overwrote"
  else
    fail_check "$title" \
      "expected the callee to answer 202 as ${CALLEE_APP_ID}.${ENV_NAME} and to see caller ${APP_ID}.${ENV_NAME}; the node answered $(http_status "$headers") and the body was $(printf '%s' "$body" | head -c 300)"
  fi
}

check_a_service_binding_is_not_a_hole_in_the_outbound_policy() {
  local title="a service call reaches an application no address would have reached"

  # The callee listens nowhere of its own: it is an isolate on this node, and
  # the only HTTP address that leads to it is this node's own, which is a
  # loopback address. This is what the outbound policy says about that -- so
  # the successful call above is not the fetch below with a shorter name, it
  # is a different mechanism with no address in it at all.
  local target="http://127.0.0.1:${DEIS_RUNTIME_PORT}/api/call"
  local body
  body=$(app_fetch service-egress "$target")
  answered_json "$title" service-egress "$body" || return

  local available reached error
  available=$(echo "$body" | jq -r '.available // false' 2>/dev/null || true)
  reached=$(echo "$body" | jq -r '.reached // false' 2>/dev/null || true)
  error=$(echo "$body" | jq -r '.error // empty' 2>/dev/null || true)

  if [ "$available" != "true" ]; then
    fail_check "$title" \
      "nothing was refused: the isolate has no fetch to refuse with. $(echo "$body" | tr '\n' ' ' | head -c 200)"
    return
  fi
  if [ "$reached" = "true" ]; then
    fail_check "$title" \
      "the handler reached this node's own HTTP port on its own, so the outbound policy is not refusing private addresses and the comparison this check makes is meaningless"
    return
  fi

  # The node said so about the address, not about a binding: the two
  # mechanisms do not share a decision point, which is why one cannot be
  # widened into the other.
  local logged
  logged=$(node_log 2>/dev/null | grep -m 1 "Refused an outbound fetch" | grep "127.0.0.1" || true)

  if [ -n "$logged" ]; then
    pass_check "$title" \
      "a handler fetching ${target} -- the only HTTP address the callee can be reached at -- is refused by the outbound policy (${error}), while the same handler reaching the same application through its AUTH binding answers 202. The binding carries no address, so nothing about the policy was widened to make it work"
  else
    fail_check "$title" \
      "the handler was refused (${error:-<no message>}) but no node log line names the refused address, so it is not clear the outbound policy is what stopped it"
  fi
}

check_a_preview_calls_the_preview_and_not_production() {
  local title="a service binding follows the caller's own environment"
  local headers="${LOG_DIR}/verify-service-preview.headers"

  # The preview of the caller runs the same bundle, so the same AUTH binding.
  # The callee has no preview deployed, so this must fail -- loudly, naming
  # the environment it looked for -- rather than quietly answering out of the
  # callee's production copy, which is another tenant's data.
  local body environment refused
  body=$(env_request "$PREVIEW_ENV_NAME" "$headers" "/api/call")
  environment=$(echo "$body" | jq -r '.environment // empty' 2>/dev/null || true)
  refused=$(echo "$body" | jq -r '.refused // empty' 2>/dev/null || true)

  if [ "$(http_status "$headers")" = "502" ] \
    && [ "$environment" = "$PREVIEW_ENV_NAME" ] \
    && echo "$refused" | grep -q "${CALLEE_APP_ID}.${PREVIEW_ENV_NAME}" \
    && echo "$refused" | grep -q "not deployed on this node"; then
    pass_check "$title" \
      "the ${PREVIEW_ENV_NAME} of ${APP_ID} runs the same bundle and the same AUTH binding, and it resolved to ${CALLEE_APP_ID}.${PREVIEW_ENV_NAME} -- which is not deployed -- rather than to ${CALLEE_APP_ID}.${ENV_NAME}, which is. It failed saying so: \"$(printf '%s' "$refused" | head -c 140)\""
  else
    fail_check "$title" \
      "expected the preview's binding to resolve to ${CALLEE_APP_ID}.${PREVIEW_ENV_NAME} and be refused; the node answered $(http_status "$headers"), environment=${environment:-<none>}, refusal=$(printf '%s' "${refused:-<none>}" | head -c 200)"
  fi
}

check_a_callee_decides_who_may_call_it() {
  local title="being named by a caller is not consent"

  # The callee lists exactly one application. The check deploys a version of
  # it that lists nobody, tries the same call, and puts the original back --
  # so what is proven is that consent is read from the version serving the
  # callee at the moment of the call, not resolved into the caller's
  # deployment when the caller was deployed.
  local src="${DEMO_DIR}/app-callee"
  local original
  original=$(cat "${src}/.config.json")

  if ! command -v jq >/dev/null 2>&1; then
    fail_check "$title" "jq is missing, so this check could not be run"
    return
  fi

  local withdrawn
  withdrawn=$(jq '.callableBy = []' "${src}/.config.json") \
    && printf '%s\n' "$withdrawn" > "${src}/.config.json"
  if ! cli_deploy "$src" "$ENV_NAME" cli-deploy-callee-withdrawn --app "$CALLEE_APP_ID"; then
    printf '%s\n' "$original" > "${src}/.config.json"
    fail_check "$title" "could not deploy the callee with consent withdrawn"
    return
  fi

  local body refused
  body=$(app_request "${LOG_DIR}/verify-service-consent.headers" "/api/call")
  refused=$(echo "$body" | jq -r '.refused // empty' 2>/dev/null || true)

  # Put the consenting version back, so every later check sees the platform
  # as the rest of the run left it.
  printf '%s\n' "$original" > "${src}/.config.json"
  cli_deploy "$src" "$ENV_NAME" cli-deploy-callee-restored --app "$CALLEE_APP_ID" || true

  local restored
  restored=$(app_request "${LOG_DIR}/verify-service-consent-back.headers" "/api/call" \
    | jq -r '.status // empty' 2>/dev/null || true)

  if echo "$refused" | grep -q "callableBy" \
    && echo "$refused" | grep -q "${APP_ID}" \
    && [ "$restored" = "202" ]; then
    pass_check "$title" \
      "with ${CALLEE_APP_ID} deployed listing nobody, the identical call from ${APP_ID} -- same bundle, same binding, nothing redelivered to it -- was refused: \"$(printf '%s' "$refused" | head -c 160)\". Redeploying the callee with ${APP_ID} on the list made the same call answer 202 again"
  else
    fail_check "$title" \
      "expected a refusal naming callableBy while consent was withdrawn and 202 after it was restored; the refusal was $(printf '%s' "${refused:-<none>}" | head -c 200) and the restored call answered ${restored:-<none>}"
  fi
}

check_storage_capability_token() {
  local title="a storage call presenting no capability token is refused"

  if ! command -v grpcurl >/dev/null 2>&1; then
    fail_check "$title" "grpcurl is not installed, so this check could not be run"
    return
  fi

  local tenant="${APP_ID}.${ENV_NAME}"
  local out rc
  out=$(grpcurl -plaintext -max-time 5 \
    -import-path "${ROOT_DIR}/crates/deis-store/proto" -proto storage.proto \
    -H "x-deis-tenant: ${tenant}" \
    -d "{\"key\":\"whatever-this-key-is-does-not-matter\"}" \
    "127.0.0.1:${DEIS_STORE_PORT}" deis.storage.KvStore/Get 2>&1) && rc=0 || rc=$?

  if [ "$rc" -ne 0 ] && echo "$out" | grep -q 'Code: Unauthenticated'; then
    pass_check "$title" \
      "KvStore.Get for tenant ${tenant} with no x-deis-storage-token header was refused before deis-store looked at any data: $(echo "$out" | tr '\n' ' ' | sed 's/  */ /g')"
  else
    fail_check "$title" \
      "expected the call to be refused as Code: Unauthenticated with no token; deis-store answered: $(echo "$out" | tr '\n' ' ' | head -c 300)"
  fi
}

# ----------------------------------------------------------------------
# The credential `deis kv`, `deis db` and `deis blob` act with
# ----------------------------------------------------------------------
#
# deis-store authorises a call by the tenant it names and the capability token
# it presents. A deployment already carries one, which is how an application
# reaches its own data; a person has none, which is why the control plane
# mints one on
# GET /api/apps/:app_id/environments/:env/storage-token. Without that route
# the three data commands have nothing to authenticate with and every one of
# them fails at the first call.

check_data_cli_reads_the_apps_write() {
  local title="deis kv reads back the value the application wrote"
  local key="cli-kv-$$-${RANDOM}"
  local value="written-by-the-handler-read-by-the-cli"
  local headers="${LOG_DIR}/verify-cli-kv.headers"

  # Written through the application, not through the CLI: the point is that
  # the two arrive at the same row, so the write has to come from the side
  # that is not being tested.
  curl -s -o /dev/null -D "$headers" -X PUT -H "x-deis-app-id: ${APP_ID}" \
    --data-binary "$value" \
    "http://127.0.0.1:${DEIS_RUNTIME_PORT}/api/kv/${key}"
  if [ "$(http_status "$headers")" != "201" ]; then
    fail_check "$title" \
      "the handler could not store ${key}: HTTP $(http_status "$headers")"
    return
  fi

  local out="${LOG_DIR}/cli-kv-get.json"
  if ! deis_cli kv get "$key" --json > "$out" 2> "${LOG_DIR}/cli-kv-get.log"; then
    fail_check "$title" \
      "deis kv get ${key} failed: $(tail -n 3 "${LOG_DIR}/cli-kv-get.log" | tr '\n' ' ')"
    return
  fi

  # A KV value is whatever bytes an application wrote, so `--json` carries it
  # base64 rather than as text it might not be. Decoding before comparing is
  # what makes this a check on the bytes instead of on their encoding.
  local tenant found read_back
  tenant=$(jq -r '.tenant // empty' < "$out" 2>/dev/null || true)
  found=$(jq -r '.found // false' < "$out" 2>/dev/null || true)
  read_back=$(jq -r '.value_base64 // empty' < "$out" 2>/dev/null \
    | openssl base64 -d -A 2>/dev/null || true)

  if [ "$found" = "true" ] && [ "$read_back" = "$value" ] \
    && [ "$tenant" = "${APP_ID}.${ENV_NAME}" ]; then
    pass_check "$title" \
      "the handler stored ${key} through context.services.kv; \`deis kv get\` asked the control plane for a storage token, presented it to deis-store as tenant ${tenant}, and read back \"${read_back}\" ($(printf '%s' "$read_back" | wc -c) bytes). The CLI never sees storage_auth_secret: it is handed a token for this one tenant"
  else
    fail_check "$title" \
      "deis kv get ${key} reports tenant '${tenant:-<none>}', found=${found}, value '${read_back:-<none>}'; expected ${APP_ID}.${ENV_NAME} and \"${value}\""
  fi
}

# The token is a credential that works directly against deis-store, without
# going through this control plane at all, and it does not expire. So the
# route that mints one is gated exactly as the deploy route is - and this is
# the check that the gate is really there rather than being a comment about
# one. Without it, `start` could leave api_auth_enabled off and every other
# check in this file would pass unchanged.
check_storage_token_requires_the_api_key() {
  local title="the storage-token route refuses a caller with no API key"
  local route="/api/apps/${APP_ID}/environments/${ENV_NAME}/storage-token"
  local minted="${LOG_DIR}/verify-storage-token.json"

  local anonymous authorised
  anonymous=$(curl -s -o /dev/null -w '%{http_code}' "$(control_url)${route}")
  authorised=$(curl -s -o "$minted" -w '%{http_code}' \
    -H "Authorization: Bearer ${API_KEY}" "$(control_url)${route}")

  local tenant endpoint token_length
  tenant=$(jq -r '.tenant // empty' < "$minted" 2>/dev/null || true)
  endpoint=$(jq -r '.store_endpoint // empty' < "$minted" 2>/dev/null || true)
  token_length=$(jq -r '.token // empty' < "$minted" 2>/dev/null | tr -d '\n' | wc -c)

  if [ "$anonymous" = "401" ] && [ "$authorised" = "200" ] \
    && [ "$tenant" = "${APP_ID}.${ENV_NAME}" ] && [ "${token_length:-0}" -gt 0 ]; then
    pass_check "$title" \
      "GET ${route} with no credential answered HTTP ${anonymous} and no token; the same request carrying the API key answered HTTP ${authorised} with tenant ${tenant}, store_endpoint ${endpoint} and a ${token_length}-character token, which is not printed here - it reads and writes that tenant's data directly against deis-store and does not expire"
  else
    fail_check "$title" \
      "anonymous GET ${route} answered HTTP ${anonymous} (expected 401) and the authenticated one HTTP ${authorised} for tenant '${tenant:-<none>}' with a ${token_length:-0}-character token (expected 200 for ${APP_ID}.${ENV_NAME})"
  fi
}

# ----------------------------------------------------------------------
# Object storage beyond put / get / delete
# ----------------------------------------------------------------------
#
# The four verbs an object store is not one without: a listing, a metadata
# read, a byte range and a conditional write. Each is checked twice over -
# once through the application, which is the only path that proves the JS
# binding is reachable, and once through `deis blob`, which is the only path
# that proves an operator can reach it. A verb that existed on the wire and in
# the client but was spelled in neither of those places would be a feature
# delivered to nobody.

# Objects the checks below share. Written through the application so that
# every later assertion is about data the binding really stored.
seed_blob_objects() {
  local headers="${LOG_DIR}/verify-blob-seed.headers"
  local key
  for key in "img/logo.png" "img/hero.png" "img/icons/star.svg" "index.html"; do
    curl -s -o /dev/null -D "$headers" -X PUT -H "x-deis-app-id: ${APP_ID}" \
      --data-binary "0123456789abcdef" \
      "http://127.0.0.1:${DEIS_RUNTIME_PORT}${BLOB_ROUTE}?key=${key}&contentType=text/plain"
    if [ "$(http_status "$headers")" != "201" ]; then
      echo "$key"
      return 1
    fi
  done
  return 0
}

check_blob_listing() {
  local title="context.services.blob lists a page and rolls a folder up at a delimiter"
  local headers="${LOG_DIR}/verify-blob-list.headers"

  local failed
  if ! failed=$(seed_blob_objects); then
    fail_check "$title" "the application could not store ${failed}"
    return
  fi

  # A delimiter at the top level: `index.html` is a key here, and everything
  # under `img/` is one folder rather than three keys.
  local top
  top=$(app_request "$headers" "${BLOB_ROUTE}?op=list&delimiter=%2F")
  local keys prefixes
  keys=$(echo "$top" | jq -r '[.objects[].key] | join(",")' 2>/dev/null || true)
  prefixes=$(echo "$top" | jq -r '.prefixes | join(",")' 2>/dev/null || true)

  # One level down, one object per page, walked to the end the way the binding
  # documents: page until list_complete, not until a page comes back short.
  local cursor="" walked="" pages=0 complete="false"
  while [ "$complete" != "true" ] && [ "$pages" -lt 10 ]; do
    local page
    page=$(app_request "$headers" "${BLOB_ROUTE}?op=list&prefix=img/&limit=1&cursor=${cursor}")
    walked="${walked}$(echo "$page" | jq -r '[.objects[].key] | join(",")' 2>/dev/null || true),"
    complete=$(echo "$page" | jq -r '.list_complete' 2>/dev/null || echo false)
    cursor=$(echo "$page" | jq -r '.cursor // ""' 2>/dev/null || true)
    pages=$((pages + 1))
  done
  walked=$(echo "$walked" | tr -s ',' ',' | sed 's/,$//')

  if [ "$keys" = "index.html" ] && [ "$prefixes" = "img/" ] \
    && [ "$walked" = "img/hero.png,img/icons/star.svg,img/logo.png" ]; then
    pass_check "$title" \
      "list({ delimiter: \"/\" }) answered objects [${keys}] and prefixes [${prefixes}] - the three files under img/ cost one entry, not three; paging img/ one object at a time took ${pages} pages and visited ${walked} in key order, each key exactly once"
  else
    fail_check "$title" \
      "expected objects [index.html], prefixes [img/] and a walk of img/hero.png,img/icons/star.svg,img/logo.png; got objects [${keys:-<none>}], prefixes [${prefixes:-<none>}], walk [${walked:-<none>}] over ${pages} pages"
  fi
}

check_blob_head() {
  local title="context.services.blob reads an object's size and etag without its body"
  local headers="${LOG_DIR}/verify-blob-head.headers"

  local head
  head=$(app_request "$headers" "${BLOB_ROUTE}?op=head&key=index.html")
  local size etag type
  size=$(echo "$head" | jq -r '.size // empty' 2>/dev/null || true)
  etag=$(echo "$head" | jq -r '.etag // empty' 2>/dev/null || true)
  type=$(echo "$head" | jq -r '.contentType // empty' 2>/dev/null || true)

  local missing_status
  missing_status=$(curl -s -o /dev/null -w '%{http_code}' -H "x-deis-app-id: ${APP_ID}" \
    "http://127.0.0.1:${DEIS_RUNTIME_PORT}${BLOB_ROUTE}?op=head&key=never-written")

  # The etag has to be the object's sha256, because that is the value a
  # conditional write compares against; a made-up identity would still look
  # like a string here but would not condition anything. Computed here rather
  # than compared against itself, so this check would fail if the store ever
  # started answering some other identity under this name.
  if ! command -v openssl >/dev/null 2>&1; then
    fail_check "$title" \
      "openssl is not installed, so this check could not work out what the object's sha256 should be"
    return
  fi
  local expected
  expected=$(printf '0123456789abcdef' | openssl dgst -sha256 -hex 2>/dev/null | awk '{print $NF}')

  if [ "$size" = "16" ] && [ "$etag" = "$expected" ] && [ "$type" = "text/plain" ] \
    && [ "$missing_status" = "404" ]; then
    pass_check "$title" \
      "head(\"index.html\") answered size 16, content-type text/plain and etag ${etag}, which is the sha256 of the 16 bytes stored - the same value --if-match takes; head of a key that was never written answered HTTP ${missing_status} without moving a byte"
  else
    fail_check "$title" \
      "expected size 16, content-type text/plain, etag ${expected} and HTTP 404 for a missing key; got size '${size:-<none>}', type '${type:-<none>}', etag '${etag:-<none>}', missing-key HTTP ${missing_status}"
  fi
}

check_blob_range_read() {
  local title="an application forwards a Range header into blob.get and answers a correct 206"
  local headers="${LOG_DIR}/verify-blob-range.headers"

  # bytes=4-9 of "0123456789abcdef" is "456789".
  local body
  body=$(app_request "$headers" "${BLOB_ROUTE}?key=index.html" -H "range: bytes=4-9")
  local status range
  status=$(http_status "$headers")
  range=$(header_value "$headers" "content-range")

  # A suffix range, which is the form a resumed download sends.
  local suffix_headers="${LOG_DIR}/verify-blob-suffix.headers"
  local suffix
  suffix=$(app_request "$suffix_headers" "${BLOB_ROUTE}?key=index.html" -H "range: bytes=-4")
  local suffix_range
  suffix_range=$(header_value "$suffix_headers" "content-range")

  # A range that names no byte of the object. It has to arrive as something
  # other than "no such object", or the application cannot answer 416 where it
  # owes 416 and 404 where it owes 404.
  local unsatisfiable
  unsatisfiable=$(curl -s -o /dev/null -w '%{http_code}' -H "x-deis-app-id: ${APP_ID}" \
    -H "range: bytes=99-" \
    "http://127.0.0.1:${DEIS_RUNTIME_PORT}${BLOB_ROUTE}?key=index.html")
  local absent
  absent=$(curl -s -o /dev/null -w '%{http_code}' -H "x-deis-app-id: ${APP_ID}" \
    -H "range: bytes=0-1" \
    "http://127.0.0.1:${DEIS_RUNTIME_PORT}${BLOB_ROUTE}?key=never-written")

  if [ "$status" = "206" ] && [ "$body" = "456789" ] \
    && [ "$range" = "bytes 4-9/16" ] && [ "$suffix" = "cdef" ] \
    && [ "$suffix_range" = "bytes 12-15/16" ] \
    && [ "$unsatisfiable" = "416" ] && [ "$absent" = "404" ]; then
    pass_check "$title" \
      "GET with 'range: bytes=4-9' answered HTTP ${status} with body '${body}' and 'content-range: ${range}'; 'range: bytes=-4' answered '${suffix}' and '${suffix_range}', so a suffix range resolves against the real size; 'bytes=99-' on a 16-byte object answered HTTP ${unsatisfiable} while the same range on a key that does not exist answered HTTP ${absent} - the two failures an application has to tell apart"
  else
    fail_check "$title" \
      "expected 206/'456789'/'bytes 4-9/16', suffix 'cdef'/'bytes 12-15/16', 416 for an unsatisfiable range and 404 for a missing key; got ${status}/'${body}'/'${range:-<none>}', suffix '${suffix}'/'${suffix_range:-<none>}', unsatisfiable ${unsatisfiable}, missing ${absent}"
  fi
}

check_blob_conditional_write() {
  local title="two conditional writes of one key: one lands, the other is told it lost"
  local key="lock-$$"
  local headers="${LOG_DIR}/verify-blob-cas.headers"
  local url="http://127.0.0.1:${DEIS_RUNTIME_PORT}${BLOB_ROUTE}"

  # Create-once. The second attempt must not overwrite, and must not look like
  # a transport failure either: the application answers 412 for exactly this.
  local first second
  first=$(curl -s -o /dev/null -w '%{http_code}' -X PUT -H "x-deis-app-id: ${APP_ID}" \
    --data-binary "held by the first writer" "${url}?key=${key}&ifNoneMatch=*")
  second=$(curl -s -o "${LOG_DIR}/verify-blob-cas-second.json" -w '%{http_code}' \
    -X PUT -H "x-deis-app-id: ${APP_ID}" \
    --data-binary "held by the second writer" "${url}?key=${key}&ifNoneMatch=*")

  local kept
  kept=$(app_request "$headers" "${BLOB_ROUTE}?key=${key}")

  # Compare-and-set on the etag: the current one writes, the superseded one
  # does not.
  local etag
  etag=$(app_request "$headers" "${BLOB_ROUTE}?op=head&key=${key}" | jq -r '.etag // empty' 2>/dev/null || true)
  local updated stale
  updated=$(curl -s -o /dev/null -w '%{http_code}' -X PUT -H "x-deis-app-id: ${APP_ID}" \
    --data-binary "second version" "${url}?key=${key}&ifMatch=${etag}")
  stale=$(curl -s -o /dev/null -w '%{http_code}' -X PUT -H "x-deis-app-id: ${APP_ID}" \
    --data-binary "third version" "${url}?key=${key}&ifMatch=${etag}")

  # And a conditional delete against the etag that is no longer current.
  local stale_delete
  stale_delete=$(curl -s -o /dev/null -w '%{http_code}' -X DELETE -H "x-deis-app-id: ${APP_ID}" \
    "${url}?key=${key}&ifMatch=${etag}")

  local final
  final=$(app_request "$headers" "${BLOB_ROUTE}?key=${key}")

  if [ "$first" = "201" ] && [ "$second" = "412" ] \
    && [ "$kept" = "held by the first writer" ] \
    && [ "$updated" = "201" ] && [ "$stale" = "412" ] \
    && [ "$stale_delete" = "412" ] && [ "$final" = "second version" ]; then
    pass_check "$title" \
      "two PUTs of ${key} with onlyIf { ifNoneMatch: \"*\" } answered HTTP ${first} and ${second}, and the object still reads \"${kept}\" - the second writer did not overwrite; a PUT quoting the current etag ${etag:0:12} answered ${updated} while the same etag quoted again after it was superseded answered ${stale}, and a DELETE quoting it answered ${stale_delete}. The object is \"${final}\": every refused write left it alone"
  else
    fail_check "$title" \
      "expected 201 then 412 with the first writer's bytes kept, then 201/412/412 on the compare-and-set; got ${first}/${second} with '${kept}', then ${updated}/${stale}/${stale_delete} with '${final}'"
  fi
}

check_cli_blob_verbs() {
  local title="deis blob list, head and get --offset reach the same objects the app stored"
  local out="${LOG_DIR}/cli-blob"

  if ! deis_cli blob list img/ --json > "${out}-list.json" 2> "${out}-list.log"; then
    fail_check "$title" \
      "deis blob list failed: $(tail -n 3 "${out}-list.log" | tr '\n' ' ')"
    return
  fi
  if ! deis_cli blob head index.html --json > "${out}-head.json" 2> "${out}-head.log"; then
    fail_check "$title" \
      "deis blob head failed: $(tail -n 3 "${out}-head.log" | tr '\n' ' ')"
    return
  fi
  if ! deis_cli blob get index.html --offset 4 --length 6 --json > "${out}-get.json" 2> "${out}-get.log"; then
    fail_check "$title" \
      "deis blob get --offset failed: $(tail -n 3 "${out}-get.log" | tr '\n' ' ')"
    return
  fi

  local listed folders headed_size headed_etag sliced offset length
  listed=$(jq -r '[.objects[].key] | join(",")' < "${out}-list.json" 2>/dev/null || true)
  folders=$(jq -r '.prefixes | join(",")' < "${out}-list.json" 2>/dev/null || true)
  headed_size=$(jq -r '.size // empty' < "${out}-head.json" 2>/dev/null || true)
  headed_etag=$(jq -r '.sha256 // empty' < "${out}-head.json" 2>/dev/null || true)
  # A blob body is bytes, so --json carries it base64 for the reason
  # `deis kv get` does.
  sliced=$(jq -r '.value_base64 // empty' < "${out}-get.json" 2>/dev/null \
    | openssl base64 -d -A 2>/dev/null || true)
  offset=$(jq -r '.range.offset // empty' < "${out}-get.json" 2>/dev/null || true)
  length=$(jq -r '.range.length // empty' < "${out}-get.json" 2>/dev/null || true)

  if [ "$listed" = "img/hero.png,img/icons/star.svg,img/logo.png" ] \
    && [ "$headed_size" = "16" ] && [ -n "$headed_etag" ] \
    && [ "$sliced" = "456789" ] && [ "$offset" = "4" ] && [ "$length" = "6" ]; then
    pass_check "$title" \
      "\`deis blob list img/\` printed ${listed} (prefixes [${folders}]); \`deis blob head index.html\` reported 16 bytes and sha256 ${headed_etag:0:12}; \`deis blob get index.html --offset 4 --length 6\` returned '${sliced}' and reported range offset ${offset} length ${length}. Every one of these objects was stored by the application, not by the CLI"
  else
    fail_check "$title" \
      "expected the three img/ keys, a 16-byte head and the slice '456789' at offset 4 length 6; got list [${listed:-<none>}], head size '${headed_size:-<none>}', slice '${sliced:-<none>}' at offset '${offset:-<none>}' length '${length:-<none>}'"
  fi
}

check_cli_blob_conditional_write() {
  local title="deis blob put --if-match exits 3 when it loses, and stores nothing"
  local key="cli-lock-$$"
  local out="${LOG_DIR}/cli-blob-cas"

  # Create it exactly once. A second create-if-absent has to be refused with
  # the exit code a script can branch on, and it must not be the code an
  # unreachable store would produce.
  if ! printf 'first' | deis_cli blob put "$key" --if-none-match '*' --json \
    > "${out}-first.json" 2> "${out}-first.log"; then
    fail_check "$title" \
      "the first create failed: $(tail -n 3 "${out}-first.log" | tr '\n' ' ')"
    return
  fi
  local etag
  etag=$(jq -r '.sha256 // empty' < "${out}-first.json" 2>/dev/null || true)

  local second_code=0
  printf 'second' | deis_cli blob put "$key" --if-none-match '*' --json \
    > "${out}-second.json" 2> "${out}-second.log" || second_code=$?

  # And the same for a delete quoting an etag that is no longer current.
  printf 'updated' | deis_cli blob put "$key" --if-match "$etag" --json \
    > "${out}-update.json" 2> "${out}-update.log" || true
  local delete_code=0
  deis_cli blob delete "$key" -y --if-match "$etag" --json \
    > "${out}-delete.json" 2> "${out}-delete.log" || delete_code=$?

  local stored_now refused_etag
  stored_now=$(deis_cli blob get "$key" --json 2>/dev/null | jq -r '.value_base64 // empty' \
    | openssl base64 -d -A 2>/dev/null || true)
  refused_etag=$(jq -r '.etag // empty' < "${out}-second.json" 2>/dev/null || true)

  if [ "$second_code" = "3" ] && [ "$delete_code" = "3" ] \
    && [ "$stored_now" = "updated" ] && [ -n "$refused_etag" ]; then
    pass_check "$title" \
      "\`deis blob put ${key} --if-none-match '*'\` succeeded once and then exited ${second_code} - its own code, not the 1 an unreachable store gives - reporting the object is now ${refused_etag:0:12}; \`deis blob delete --if-match\` with the superseded etag also exited ${delete_code}, and the object still reads '${stored_now}'"
  else
    fail_check "$title" \
      "expected exit 3 from both the second create and the stale delete, with the object left reading 'updated'; got exit ${second_code} and ${delete_code}, object '${stored_now:-<none>}', reported etag '${refused_etag:-<none>}'"
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

# The producer half of queueing past one message at a time.
#
# Fifty messages sent with `send` in a loop are fifty op dispatches against
# this application's op-rate ceiling and fifty round trips to deis-queue, and
# there is no point at which the fifty are one thing: a handler cut off half
# way has enqueued half of them. `sendBatch` is one dispatch, one round trip
# and one transaction on the queue's side, so the array either arrives whole or
# not at all.
#
# What can be observed from outside is the two ends: one request goes in, and
# every message comes out of the consumer. The middle - that it was one call
# rather than five - is read off the handler's own log line, which only the
# `sendBatch` path writes.
check_queue_send_batch() {
  local title="a producer enqueues a whole array in one call and every message arrives"
  local stamp="batch-$$-${RANDOM}"
  local headers="${LOG_DIR}/verify-queue-batch.headers"
  local size=5

  local jobs="[" i
  for i in $(seq 1 $size); do
    [ "$i" = 1 ] || jobs="${jobs},"
    jobs="${jobs}{\"key\":\"${stamp}-${i}\",\"value\":\"batched-${i}\"}"
  done
  jobs="${jobs}]"

  local sent
  sent=$(curl -s -D "$headers" -X POST -H "x-deis-app-id: ${APP_ID}" \
    -H 'content-type: application/json' -d "$jobs" \
    "http://127.0.0.1:${DEIS_RUNTIME_PORT}/api/enqueue-batch")
  if [ "$(http_status "$headers")" != "202" ]; then
    fail_check "$title" \
      "POST /api/enqueue-batch answered $(http_status "$headers"): ${sent}"
    return
  fi

  local queued
  queued=$(echo "$sent" | jq -r '.queued // empty' 2>/dev/null || true)

  # The consumer is a poller, so the only thing to do is wait for the effect.
  # curl paces its own retries a second apart, and only the first key needs the
  # long budget: the batch is one transaction and one delivery, so by the time
  # one of them has been handled the rest are in the same batch.
  local missing="" got
  for i in $(seq 1 $size); do
    got=$(curl -sf --retry 45 --retry-delay 1 --retry-max-time 90 --retry-all-errors \
      -H "x-deis-app-id: ${APP_ID}" \
      "http://127.0.0.1:${DEIS_RUNTIME_PORT}/api/kv/job:${stamp}-${i}" 2>/dev/null || echo '')
    if [ "$(echo "$got" | jq -r '.value // empty' 2>/dev/null || true)" != "batched-${i}" ]; then
      missing="${missing} job:${stamp}-${i}"
    fi
  done

  # Written by the handler only on the `sendBatch` path; /api/enqueue logs one
  # line per message and never this one.
  local batch_line
  batch_line=$(node_log | grep -m 1 "queued ${size} job(s) in one batch" || true)

  if [ "$queued" = "$size" ] && [ -z "$missing" ] && [ -n "$batch_line" ]; then
    pass_check "$title" \
      "one POST to /api/enqueue-batch answered 202 with queued=${size}, the handler logged \"$(echo "$batch_line" | log_message)\", and the consumer wrote all ${size} keys job:${stamp}-1 .. job:${stamp}-${size}"
  else
    fail_check "$title" \
      "expected 202 with queued=${size} (got '${queued:-<none>}'), all ${size} keys written (missing:${missing:- none}) and the handler's one-batch line (${batch_line:+present}${batch_line:-never written})"
  fi
}

# What `maxConcurrency` means on a platform where one application in one
# environment is one isolate on one thread.
#
# It cannot mean parallelism, because the handler is serialised by the isolate
# whatever the number says. It means how many batches the node's poller may
# hold at once - leased, waiting their turn on the runtime thread, or being
# acknowledged - so the next batch is already in hand when the handler
# finishes. The number is only half of what has to be true; the other half is
# that the node says which of the two it is, because the name reads as
# parallelism on every other platform.
check_queue_consumer_concurrency() {
  local title="a queue consumer holds the batches its manifest asks for, and the node says that is not parallelism"

  local declared
  declared=$(jq -r '.queues[0].maxConcurrency // empty' "${APP_SRC_DIR}/.config.json" \
    2>/dev/null || true)
  [ -n "$declared" ] || declared=2

  local line applied
  line=$(node_log | grep -m 1 "Consuming queue jobs for app ${APP_ID}.${ENV_NAME}:" || true)
  applied=$(echo "$line" | sed -n 's/.*at most \([0-9]*\) batch(es).*/\1/p')

  if [ "$applied" = "$declared" ] && [ -n "$line" ] \
    && echo "$line" | grep -q "Outstanding, not parallel"; then
    pass_check "$title" \
      "the project asks for maxConcurrency ${declared} on queue jobs and the node reported \"$(echo "$line" | log_message)\""
  else
    fail_check "$title" \
      "expected the node to log that it holds ${declared} batch(es) of queue jobs at once and to say the number is not parallelism; it reported '${applied:-<none>}' in the line $(if [ -n "$line" ]; then echo "\"$(echo "$line" | log_message)\""; else echo "which was never written"; fi)"
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

# ----------------------------------------------------------------------
# What an application may ask to be held to
# ----------------------------------------------------------------------
#
# A bundle manifest can carry a `limits` block. It is read by the control
# plane when the version is uploaded, travels to the node with the deployment,
# and is clamped field by field against the node's own configuration when the
# isolate is built. The direction is the whole point: an application may ask
# to be held to *less* than the node allows and never to more.
#
# The demo runs both halves at once. Production is v1, whose sources ask for a
# one-second wall clock where the node allows thirty; the preview is v2, whose
# staged copy asks for ten minutes. One is applied, the other is refused, and
# the two are the same application on the same node.

# One request to the slow handler, answered with "<status> <seconds>".
#
# `%{time_total}` rather than a clock read either side of it, because what is
# being measured is when the platform stopped the handler, and a shell's own
# timing would add its process startup to the number being judged.
timed_slow_request() {
  local environment=$1
  local ms=$2
  curl -s -o /dev/null -w '%{http_code} %{time_total}' \
    -H "x-deis-app-id: ${APP_ID}" -H "x-deis-environment: ${environment}" \
    "http://127.0.0.1:${DEIS_RUNTIME_PORT}/api/slow?ms=${ms}"
}

check_app_limits_narrowed() {
  local title="an application narrowing its own timeout is held to its number"

  # Twice the app's ceiling and a small fraction of the node's, so the two
  # possible outcomes are far apart: cut off at about a second means the app's
  # number is in force, answered at about two means the node's is.
  local over=$((APP_TIMEOUT_MS * 2))
  local under=$((APP_TIMEOUT_MS / 5))

  local overran within
  overran=$(timed_slow_request "$ENV_NAME" "$over")
  within=$(timed_slow_request "$ENV_NAME" "$under")

  # The node's own account of the ceiling it installed, so the numbers below
  # are the node's rather than this script's idea of them.
  local applied_line node_default applied
  applied_line=$(node_log \
    | grep -m 1 "App ${APP_ID}.${ENV_NAME} narrows the default execution timeout" || true)
  applied=$(echo "$applied_line" | sed -n 's/.*timeout: \([0-9]*\)ms.*/\1/p')
  node_default=$(echo "$applied_line" | sed -n 's/.*platform default \([0-9]*\)ms.*/\1/p')

  local cut_off_line
  cut_off_line=$(node_log \
    | grep -m 1 "App ${APP_ID}.${ENV_NAME} was cut off on one request" || true)

  local over_status over_seconds within_status
  over_status=${overran%% *}
  over_seconds=${overran##* }
  within_status=${within%% *}

  if [ "$applied" = "$APP_TIMEOUT_MS" ] && [ -n "$node_default" ] \
    && [ "$node_default" != "$APP_TIMEOUT_MS" ] \
    && [ "$over_status" = "500" ] && [ "$within_status" = "200" ] \
    && [ -n "$cut_off_line" ]; then
    pass_check "$title" \
      "the manifest asks for ${APP_TIMEOUT_MS}ms and the node's own ceiling is ${node_default}ms; the node logged \"$(echo "$applied_line" | log_message)\". GET /api/slow?ms=${over} was cut off after ${over_seconds}s with HTTP ${over_status} - under the node's ${node_default}ms it would have answered - while GET /api/slow?ms=${under} answered HTTP ${within_status}"
  else
    fail_check "$title" \
      "expected the node to apply ${APP_TIMEOUT_MS}ms (it reports '${applied:-<none>}ms', platform default '${node_default:-<none>}ms'), /api/slow?ms=${over} to be cut off (got HTTP ${over_status} after ${over_seconds}s) and /api/slow?ms=${under} to answer 200 (got HTTP ${within_status}); the cut-off line was $(if [ -n "$cut_off_line" ]; then echo "present"; else echo "never written"; fi)"
  fi
}

# The half that is a security property rather than a convenience.
#
# An invocation holds a runtime thread for its whole duration, so an
# application that could set its own timeout to ten minutes could park one of
# this node's threads for ten minutes, and every other tenant queued behind
# that thread would wait with it. The ceiling is therefore the node's to set
# and an application may only ask for less of it.
#
# The witness is the node's log, and deliberately so: what has to be shown is
# that a number the application asked for never became the number in force,
# and the only place both numbers appear together is the line the clamp
# writes. Demonstrating it by request would mean a request that runs for the
# node's full thirty seconds.
check_app_limits_clamped() {
  local title="an application asking for more than the node allows is held to the node's value"

  local asked_line clamp_line applied narrowed
  asked_line=$(node_log \
    | grep -m 1 "timeout_ms=Some(${GREEDY_TIMEOUT_MS})" || true)
  clamp_line=$(node_log \
    | grep -m 1 "App ${APP_ID}.${PREVIEW_ENV_NAME} asks for a ${GREEDY_TIMEOUT_MS}ms execution timeout" || true)
  applied=$(echo "$clamp_line" | sed -n "s/.*above this node's \([0-9]*\)ms ceiling.*/\1/p")

  # The line that must *not* exist: "narrows" is what the node writes when it
  # takes an application's number, so the greedy version producing one would
  # mean the clamp had let it through.
  narrowed=$(node_log \
    | grep -m 1 "App ${APP_ID}.${PREVIEW_ENV_NAME} narrows the default execution timeout: ${GREEDY_TIMEOUT_MS}ms" || true)

  if [ -n "$asked_line" ] && [ -n "$applied" ] \
    && [ "$applied" != "$GREEDY_TIMEOUT_MS" ] && [ -z "$narrowed" ]; then
    pass_check "$title" \
      "the ${PREVIEW_ENV_NAME} version's manifest reached the node asking for timeout_ms=${GREEDY_TIMEOUT_MS}, and the node answered \"$(echo "$clamp_line" | log_message | sed 's/\. An invocation.*//')\". ${applied}ms is the number in force; no line says this environment narrowed anything, because it did not"
    return
  fi

  fail_check "$title" \
    "expected the node to log that ${APP_ID}.${PREVIEW_ENV_NAME} asked for ${GREEDY_TIMEOUT_MS}ms and was held to its own ceiling; the deployment line was $(if [ -n "$asked_line" ]; then echo "present"; else echo "absent"; fi), the clamp reports applying '${applied:-<none>}ms', and a \"narrows\" line for ${GREEDY_TIMEOUT_MS}ms was $(if [ -n "$narrowed" ]; then echo "written, so the request was granted"; else echo "absent"; fi)"
}

# The ceiling on fan-out, which none of the other limits reaches.
#
# An invocation that answers one inbound request with two hundred outbound ones
# is spending this node's sockets and its outbound bandwidth, both of which
# every other tenant on the box is also using - and it does so while looking
# healthy on every other ceiling: the calls run concurrently, so the wall clock
# sees one slow request, the CPU clock sees almost nothing, and the op rate
# limit sees a couple of hundred dispatches spread over a second against a
# ceiling of a thousand.
#
# The example project narrows the node's number in its own manifest, which is
# the same "narrowing only" path the timeout takes, so this also shows a second
# limit travelling from a project file into a node's behaviour.
check_subrequest_ceiling() {
  local title="an invocation is held to a ceiling on outbound calls, and the refusal names it"
  local headers="${LOG_DIR}/verify-subrequests.headers"

  # Read out of the project the demo actually deployed rather than repeated
  # here, so the number this check believes in cannot drift from the number
  # the application asked for.
  local declared
  declared=$(jq -r '.limits.maxSubrequests // empty' "${APP_SRC_DIR}/.config.json" \
    2>/dev/null || true)
  [ -n "$declared" ] || declared=6

  local target="http%3A%2F%2F127.0.0.1%3A${DEIS_ORIGIN_PORT}%2Fpayload.txt"
  local under=$((declared - 2))
  local over=$((declared + 3))

  local within beyond within_done beyond_done refusal
  within=$(app_request "$headers" "/api/fanout?n=${under}&url=${target}")
  if [ "$(http_status "$headers")" != "200" ]; then
    fail_check "$title" \
      "GET /api/fanout?n=${under} answered $(http_status "$headers"): $(echo "$within" | head -c 200)"
    return
  fi
  beyond=$(app_request "$headers" "/api/fanout?n=${over}&url=${target}")

  within_done=$(echo "$within" | jq -r '.completed // empty' 2>/dev/null || true)
  beyond_done=$(echo "$beyond" | jq -r '.completed // empty' 2>/dev/null || true)
  refusal=$(echo "$beyond" | jq -r '.error // empty' 2>/dev/null || true)

  # The node's own two lines: the one that says which number it applied, and
  # the one that says it refused a call. The numbers quoted below are the
  # node's rather than this script's idea of them.
  local narrowed_line refused_line
  narrowed_line=$(node_log \
    | grep -m 1 "App ${APP_ID}.${ENV_NAME} narrows the default subrequest ceiling" || true)
  refused_line=$(node_log \
    | grep -m 1 "this invocation has already made ${declared} outbound call(s)" || true)

  if [ "$within_done" = "$under" ] && [ "$beyond_done" = "$declared" ] \
    && [ -n "$narrowed_line" ] && [ -n "$refused_line" ] \
    && echo "$refusal" | grep -q "${declared} outbound call"; then
    pass_check "$title" \
      "the manifest asks for ${declared} outbound calls per invocation and the node logged \"$(echo "$narrowed_line" | log_message)\". GET /api/fanout?n=${under} made all ${within_done}; GET /api/fanout?n=${over} made ${beyond_done} and the ${beyond_done}th+1 came back to the handler as: $(echo "$refusal" | head -c 220)"
  else
    fail_check "$title" \
      "expected ${under} of ${under} calls to complete (got '${within_done:-<none>}') and exactly ${declared} of ${over} (got '${beyond_done:-<none>}') with a refusal naming ${declared} (got '${refusal:-<none>}'); the node's narrowing line was $(if [ -n "$narrowed_line" ]; then echo "present"; else echo "never written"; fi) and its refusal line $(if [ -n "$refused_line" ]; then echo "present"; else echo "never written"; fi)"
  fi
}

# What a redirect chain costs, which is the part an application author has to
# be told rather than left to discover.
#
# `fetch` follows redirects, and this platform follows them by hand in
# `js/bootstrap.js` so that every hop is judged by the same egress policy as
# the destination the application named. Each hop is also a separate connection
# leaving this node, so each hop costs one subrequest. The consequence is worth
# demonstrating rather than asserting: a handler containing exactly one
# `fetch` can exhaust the ceiling, and the only way that can happen is if hops
# are counted.
check_a_redirect_chain_costs_a_subrequest_per_hop() {
  local title="each hop of a redirect chain costs one of the invocation's outbound calls"
  local headers="${LOG_DIR}/verify-subrequest-redirects.headers"

  local declared
  declared=$(jq -r '.limits.maxSubrequests // empty' "${APP_SRC_DIR}/.config.json" \
    2>/dev/null || true)
  [ -n "$declared" ] || declared=6

  # One fetch plus this many redirects is exactly the ceiling; one more hop is
  # one connection too many.
  local fits=$((declared - 1))
  local spills=$declared

  local base="http%3A%2F%2F127.0.0.1%3A${DEIS_ORIGIN_PORT}%2Fredirect%3Fhops%3D"
  local within beyond within_done beyond_done refusal
  within=$(app_request "$headers" "/api/fanout?n=1&url=${base}${fits}")
  if [ "$(http_status "$headers")" != "200" ]; then
    fail_check "$title" \
      "GET /api/fanout over a ${fits}-hop chain answered $(http_status "$headers"): $(echo "$within" | head -c 200)"
    return
  fi
  beyond=$(app_request "$headers" "/api/fanout?n=1&url=${base}${spills}")

  within_done=$(echo "$within" | jq -r '.completed // empty' 2>/dev/null || true)
  beyond_done=$(echo "$beyond" | jq -r '.completed // empty' 2>/dev/null || true)
  refusal=$(echo "$beyond" | jq -r '.error // empty' 2>/dev/null || true)

  if [ "$within_done" = "1" ] && [ "$beyond_done" = "0" ] \
    && echo "$refusal" | grep -q "${declared} outbound call"; then
    pass_check "$title" \
      "one fetch() of a URL that redirects ${fits} times finished - ${declared} connections, exactly this application's ceiling - while the same single fetch() of a URL that redirects ${spills} times was refused: $(echo "$refusal" | head -c 220). The handler makes one call either way, so the ceiling can only have been reached by counting hops"
    return
  fi

  fail_check "$title" \
    "expected the ${fits}-hop chain to complete (completed='${within_done:-<none>}') and the ${spills}-hop chain to be refused with a message naming ${declared} (completed='${beyond_done:-<none>}', error='${refusal:-<none>}')"
}

# ----------------------------------------------------------------------
# Compatibility date and flags
# ----------------------------------------------------------------------
#
# An application says which day's platform it was written against, and the
# platform keeps giving it that day's answers. Without it, every fix that
# changes an observable behaviour is a breaking change for the whole fleet,
# delivered on a node restart nobody scheduled.
#
# The demo runs both sides at once, on one node, at the same moment:
# production is built from sources naming 2026-09-01 and the preview tree has
# the key deleted, which is what an application deployed before any of this
# existed looks like. The behaviour they differ on is real and was a defect:
# a response carrying two Set-Cookie headers used to arrive with one.

# Ask the cookie handler and count the Set-Cookie headers that came back.
set_cookie_count() {
  local environment=$1
  local headers="${LOG_DIR}/verify-cookies-${environment}.headers"
  env_request "$environment" "$headers" "$COOKIE_ROUTE" > /dev/null
  grep -ci '^set-cookie:' "$headers" || true
}

check_compatibility_date_is_visible() {
  local title="deis deploy and deis versions name the date an application runs under"
  local report="${LOG_DIR}/cli-deploy-v1.json"
  local json="${LOG_DIR}/cli-compat-versions.json"
  local text="${LOG_DIR}/cli-compat-versions.txt"

  if ! deis_cli versions --json > "$json" 2> "${LOG_DIR}/cli-compat-versions.log" \
    || ! deis_cli versions > "$text" 2>> "${LOG_DIR}/cli-compat-versions.log"; then
    fail_check "$title" \
      "deis versions failed: $(tail -n 3 "${LOG_DIR}/cli-compat-versions.log" | tr '\n' ' ')"
    return
  fi

  local deployed live_date preview_date column
  deployed=$(jq -r '.version.compatibility.date // empty' < "$report" 2>/dev/null || true)
  live_date=$(jq -r --arg env "$ENV_NAME" \
    '.versions[] | select(.environments | index($env)) | .compatibility.date // "none"' \
    < "$json" 2>/dev/null || true)
  preview_date=$(jq -r --arg env "$PREVIEW_ENV_NAME" \
    '.versions[] | select(.environments | index($env)) | .compatibility.date // "none"' \
    < "$json" 2>/dev/null || true)
  column=$(grep -c 'COMPAT' "$text" || true)

  if [ -n "$deployed" ] && [ "$live_date" = "$deployed" ] \
    && [ "$preview_date" = "none" ] && [ "${column:-0}" -ge 1 ] \
    && grep -q 'none (oldest)' "$text"; then
    pass_check "$title" \
      "the deploy report says the version it uploaded runs under ${deployed}; \`deis versions\` prints a COMPAT column where ${ENV_NAME}'s version reads ${live_date} and ${PREVIEW_ENV_NAME}'s reads \"none (oldest)\". Nobody had to open a bundle"
  else
    fail_check "$title" \
      "deploy report date '${deployed:-<none>}', ${ENV_NAME} version date '${live_date:-<none>}', ${PREVIEW_ENV_NAME} version date '${preview_date:-<none>}', COMPAT header found ${column:-0} time(s); the table was: $(tr '\n' '|' < "$text" | cut -c1-400)"
  fi
}

check_every_set_cookie_reaches_a_dated_application() {
  local title="an application written against 2026-09-01 gets every Set-Cookie it sets"
  local count
  count=$(set_cookie_count "$ENV_NAME")

  local date
  date=$(jq -r '.version.compatibility.date // empty' \
    < "${LOG_DIR}/cli-deploy-v1.json" 2>/dev/null || true)

  if [ "$count" = "2" ]; then
    pass_check "$title" \
      "GET ${COOKIE_ROUTE} on ${ENV_NAME} returned ${count} Set-Cookie headers - the session cookie and the CSRF cookie the handler set with headers.append - because this version's manifest names compatibility date ${date:-<none>}"
  else
    fail_check "$title" \
      "expected 2 Set-Cookie headers from ${ENV_NAME} (manifest date '${date:-<none>}'), got ${count}: $(tr -d '\r' < "${LOG_DIR}/verify-cookies-${ENV_NAME}.headers" | grep -i '^set-cookie' | tr '\n' ' ')"
  fi
}

# The half that is the whole point: the old answer survives for the
# application that was written against it, on the same node, in the same
# second, from the same handler source.
check_an_undated_application_keeps_the_old_answer() {
  local title="an application with no compatibility date still gets the answer it was written against"
  local count
  count=$(set_cookie_count "$PREVIEW_ENV_NAME")

  # The node's own account of what it dropped and why, so this quotes the
  # node rather than this script's idea of it.
  local dropped
  dropped=$(node_log \
    | grep -m 1 "App ${APP_ID}.${PREVIEW_ENV_NAME} set 2 Set-Cookie headers" || true)

  if [ "$count" = "1" ] && [ -n "$dropped" ]; then
    pass_check "$title" \
      "the same handler on ${PREVIEW_ENV_NAME}, whose manifest names no date, returned ${count} Set-Cookie header where ${ENV_NAME} returned 2. The node said so: \"$(echo "$dropped" | log_message | cut -c1-200)\""
  else
    fail_check "$title" \
      "expected exactly 1 Set-Cookie header from ${PREVIEW_ENV_NAME} and a node log line naming the drop; got ${count} header(s) and the line was $(if [ -n "$dropped" ]; then echo "present"; else echo "never written"; fi)"
  fi
}

# Upload a bundle whose manifest has been altered, straight to the control
# plane, and print the HTTP status.
#
# Past the CLI on purpose. Everything else in this demo goes through `deis`,
# but the claim here is about what the *control plane* refuses to store, and
# the CLI would have refused these bundles itself - which would have proved
# the wrong half. A fleet where one node is older than the date a version
# names is the case this protects, and the check that stops it is the one at
# upload.
upload_altered_bundle() {
  local label=$1
  local mutation=$2
  local source="${DEMO_DIR}/app-v1/dist/deislet/bundle.zip"
  local patched="${DEMO_DIR}/${label}.zip"
  local body="${LOG_DIR}/${label}-upload.json"

  python3 - "$source" "$patched" "$mutation" <<'ALTER' || return 1
import json, sys, zipfile

source, target, mutation = sys.argv[1], sys.argv[2], sys.argv[3]
with zipfile.ZipFile(source) as archive:
    entries = [(info.filename, archive.read(info.filename)) for info in archive.infolist()]

with zipfile.ZipFile(target, "w", zipfile.ZIP_DEFLATED) as out:
    for name, data in entries:
        if name == "manifest.json":
            manifest = json.loads(data)
            exec(mutation, {"manifest": manifest})
            data = json.dumps(manifest).encode()
        out.writestr(name, data)
ALTER

  curl -s -o "$body" -w '%{http_code}' -X POST \
    -H "Authorization: Bearer ${API_KEY}" \
    -H "Content-Type: application/zip" \
    --data-binary "@${patched}" \
    "$(control_url)/api/apps/${APP_ID}/versions"
}

# Read whatever the control plane said about a refused upload.
refusal_message() {
  local file=$1
  local message
  message=$(jq -r '.error // .message // empty' < "$file" 2>/dev/null || true)
  if [ -z "$message" ]; then
    message=$(head -c 300 "$file" 2>/dev/null || true)
  fi
  echo "$message"
}

check_a_date_this_platform_cannot_honour_is_refused() {
  local title="a bundle naming a compatibility date this build does not know is refused at upload"

  if ! command -v python3 >/dev/null 2>&1; then
    fail_check "$title" "python3 is needed to alter a bundle's manifest"
    return
  fi

  local status message
  status=$(upload_altered_bundle "future-date-bundle" \
    'manifest["compatibility"] = {"date": "2099-01-01"}')
  message=$(refusal_message "${LOG_DIR}/future-date-bundle-upload.json")

  if [ "$status" = "400" ] && echo "$message" | grep -q "2099-01-01"; then
    pass_check "$title" \
      "POST /api/apps/${APP_ID}/versions with a manifest naming 2099-01-01 answered HTTP ${status}: \"$(echo "$message" | cut -c1-220)\". No version row was written, so no node was ever asked for semantics this build does not have"
  else
    fail_check "$title" \
      "expected HTTP 400 naming 2099-01-01, got HTTP ${status} and message '${message:-<empty>}'"
  fi
}

check_an_unknown_compatibility_flag_is_refused() {
  local title="a bundle naming a behaviour flag this build does not know is refused at upload"

  if ! command -v python3 >/dev/null 2>&1; then
    fail_check "$title" "python3 is needed to alter a bundle's manifest"
    return
  fi

  local status message
  status=$(upload_altered_bundle "unknown-flag-bundle" \
    'manifest["compatibility"] = {"flags": ["cookies_but_faster"]}')
  message=$(refusal_message "${LOG_DIR}/unknown-flag-bundle-upload.json")

  if [ "$status" = "400" ] && echo "$message" | grep -q "cookies_but_faster"; then
    pass_check "$title" \
      "POST /api/apps/${APP_ID}/versions with the flag \"cookies_but_faster\" answered HTTP ${status}: \"$(echo "$message" | cut -c1-220)\". A flag nothing implements is refused rather than ignored, so an application is never told it has a behaviour it does not"
  else
    fail_check "$title" \
      "expected HTTP 400 naming cookies_but_faster, got HTTP ${status} and message '${message:-<empty>}'"
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

# Ask the example app to fetch a URL and hand back the JSON it answers with.
#
# The destination is passed as a query parameter rather than baked into the
# app, because the two checks below need the same handler to be pointed at two
# different addresses: one it is allowed to reach and one it is not. What the
# platform does with the second is the difference between an egress policy and
# a comment about one.
app_fetch() {
  local label=$1
  local target=$2
  curl -s -G -D "${LOG_DIR}/verify-${label}.headers" \
    -H "x-deis-app-id: ${APP_ID}" \
    --data-urlencode "url=${target}" \
    "http://127.0.0.1:${DEIS_RUNTIME_PORT}/api/fetch"
}

# Did the handler answer at all?
#
# Worth its own step because of what it caught the first time it ran: an
# outbound call that aborts the node process. There is then no JSON to read,
# and every field below reads as `false` - so the fetch checks would have
# reported "this isolate has no fetch" and the refusal check would have
# reported a refusal, when what actually happened is that the node died
# mid-request. Both are the wrong sentence. Returns non-zero having said so.
answered_json() {
  local title=$1
  local label=$2
  local body=$3

  if echo "$body" | jq -e . >/dev/null 2>&1; then
    return 0
  fi

  local status
  status=$(http_status "${LOG_DIR}/verify-${label}.headers" 2>/dev/null || true)
  fail_check "$title" \
    "the node did not answer with JSON (HTTP ${status:-no response}). Either the request never reached the handler, or the node stopped answering while it ran - check .demo/logs/deis-runtime.log. Body: $(echo "$body" | tr '\n' ' ' | head -c 200)"
  return 1
}

# `fetch` exists at all, and reaches a destination the operator opened up.
#
# Edge Canon lists `fetch` under basic features (SPECIFICATION_BASIC.md
# section 8.1), so a handler calling it must not meet a ReferenceError. The
# handler reports `available` separately from `reached` precisely so this check
# can tell "the platform has no fetch" from "the destination was refused" -
# without that, a platform missing `fetch` entirely would pass the refusal
# check below and fail nothing.
check_app_fetch_allowed() {
  local title="a handler's fetch reaches a destination the node allows"

  # A run started before the origin existed has no port for it in its state,
  # and the node it started has no `[fetch] allow` line either. Say that
  # rather than dying on an unset variable half way through the run.
  if [ -z "${DEIS_ORIGIN_PORT:-}" ]; then
    fail_check "$title" \
      "this run's state names no fetch origin, so it was started before the demo had one. Restart it: ./scripts/demo.sh stop && ./scripts/demo.sh start"
    return
  fi

  local target="http://127.0.0.1:${DEIS_ORIGIN_PORT}/payload.txt"
  local body
  body=$(app_fetch fetch-allowed "$target")
  answered_json "$title" fetch-allowed "$body" || return

  local available reached status got
  available=$(echo "$body" | jq -r '.available // false' 2>/dev/null || true)
  reached=$(echo "$body" | jq -r '.reached // false' 2>/dev/null || true)
  status=$(echo "$body" | jq -r '.status // empty' 2>/dev/null || true)
  got=$(echo "$body" | jq -r '.body // empty' 2>/dev/null || true)

  if [ "$available" != "true" ]; then
    fail_check "$title" \
      "the isolate has no fetch: $(echo "$body" | tr '\n' ' ' | head -c 300)"
    return
  fi
  if [ "$reached" != "true" ] || [ "$status" != "200" ]; then
    fail_check "$title" \
      "GET ${target} from inside the handler did not answer 200: $(echo "$body" | tr '\n' ' ' | head -c 300)"
    return
  fi
  # The body, not just the status: something else listening on that port
  # answers 200 too, and this is what makes the check about the demo's own
  # origin rather than about any server at all.
  case "$got" in
    *"$ORIGIN_BODY"*)
      pass_check "$title" \
        "the handler called ${target} and got the origin's own body back (HTTP ${status}, \"${ORIGIN_BODY}\"); that address is the single entry in the node's [fetch] allow"
      ;;
    *)
      fail_check "$title" \
        "the fetch answered ${status} but the body was not the origin's: $(echo "$got" | tr '\n' ' ' | head -c 200)"
      ;;
  esac
}

# The check this whole arrangement exists for.
#
# deis-store trusts the tenant header it is given: it has to, because until an
# application could open an outbound connection, nothing but the node could
# reach it. The moment `fetch` exists, a handler that can reach deis-store can
# read and delete any other application's keys, rows and blobs by asking with
# a different `x-deis-tenant`. So the handler is pointed straight at it here,
# and a call that gets through is a failure - not a curiosity, and not
# something to note and move on from.
#
# Note what is *not* asserted: no wording, no status code, no particular
# refusal message. Only that the application did not reach it. Any refusal
# passes; only an answer fails.
check_app_fetch_refused() {
  local title="a handler's fetch to deis-store is refused"
  local target="http://127.0.0.1:${DEIS_STORE_PORT}/"
  local body
  body=$(app_fetch fetch-refused "$target")
  answered_json "$title" fetch-refused "$body" || return

  local available reached status error
  available=$(echo "$body" | jq -r '.available // false' 2>/dev/null || true)
  reached=$(echo "$body" | jq -r '.reached // false' 2>/dev/null || true)
  status=$(echo "$body" | jq -r '.status // empty' 2>/dev/null || true)
  error=$(echo "$body" | jq -r '.error // empty' 2>/dev/null || true)

  # Without this the check would pass on a platform that has no `fetch` at
  # all, and report an egress policy that does not exist.
  if [ "$available" != "true" ]; then
    fail_check "$title" \
      "nothing was refused: the isolate has no fetch to refuse with. $(echo "$body" | tr '\n' ' ' | head -c 300)"
    return
  fi

  if [ "$reached" = "true" ]; then
    fail_check "$title" \
      "the application reached deis-store on its own (HTTP ${status}). Any handler on this node can now address storage directly with a tenant header of its choosing, which is every other application's data"
    return
  fi

  pass_check "$title" \
    "the handler asked for ${target} and the platform did not make the call: ${error}. It sits on the same loopback interface as the origin the same handler reaches freely, so what stopped it is the policy and not the network"
}

# The Web Crypto globals, judged on their values.
#
# SPECIFICATION_BASIC.md section 8.1 lists `crypto.randomUUID` and
# `crypto.subtle`. The digest is checked against the known SHA-256 of a fixed
# string, so an implementation that returned well-formed bytes of its own
# invention fails here rather than passing a "did it throw?" test.
check_web_crypto() {
  local title="crypto.randomUUID and crypto.subtle.digest produce real values"
  local headers="${LOG_DIR}/verify-crypto.headers"
  local body
  body=$(app_request "$headers" "/api/crypto")
  answered_json "$title" crypto "$body" || return

  local has_crypto uuid differ digest
  has_crypto=$(echo "$body" | jq -r '.present.crypto // false' 2>/dev/null || true)
  uuid=$(echo "$body" | jq -r '.uuid // empty' 2>/dev/null || true)
  differ=$(echo "$body" | jq -r '.uuidsDiffer // false' 2>/dev/null || true)
  digest=$(echo "$body" | jq -r '.digestHex // empty' 2>/dev/null || true)

  if [ "$has_crypto" != "true" ]; then
    fail_check "$title" \
      "the isolate has no crypto: $(echo "$body" | tr '\n' ' ' | head -c 300)"
    return
  fi

  # Version 4, variant 1: the shape a UUID has to have. A generator that
  # returned a well-formed string of the wrong version is still wrong, and a
  # loose length check would not notice.
  if ! echo "$uuid" | grep -qiE '^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$'; then
    fail_check "$title" "randomUUID answered '${uuid}', which is not a version 4 UUID"
    return
  fi
  if [ "$differ" != "true" ]; then
    fail_check "$title" "randomUUID returned the same id twice"
    return
  fi
  if [ "$digest" != "$DEISLET_SHA256" ]; then
    fail_check "$title" \
      "subtle.digest('SHA-256', 'deislet') answered ${digest:-nothing}, expected ${DEISLET_SHA256}"
    return
  fi

  pass_check "$title" \
    "randomUUID gave ${uuid} and a different one on the next call; subtle.digest('SHA-256', 'deislet') gave ${digest}, which is the known digest of that string"
}

# The Cache API past `caches.default`, and the honest half of it.
#
# Two things have to be true and they are different in kind. The first is
# ordinary: a cache an application opens by name is its own namespace, so the
# same key in two caches gives two answers and emptying one leaves the other
# alone. A named cache that could answer the default cache's read with the
# wrong page would be a correctness bug nothing would report.
#
# The second is the product decision. This cache lives in the isolate's own
# memory: it is gone when a new version is deployed and it is not shared with
# the other nodes of an installation. `caches.scope` is what makes that
# checkable from inside an application rather than something an author has to
# have read, because the two kinds of miss mean opposite things - an entry
# evicted from the store you wrote to, against a store that is new and lost
# everything at once. Two requests seeing the same `scope.id` is the store
# outliving a request; an id that changed is the second kind of miss.
check_named_caches() {
  local title="an application opens a cache of its own, and can tell which store it is holding"
  local headers="${LOG_DIR}/verify-cache.headers"

  local first second
  first=$(app_request "$headers" "/api/cache")
  if [ "$(http_status "$headers")" != "200" ]; then
    fail_check "$title" \
      "GET /api/cache answered $(http_status "$headers"): $(echo "$first" | head -c 200)"
    return
  fi
  # A second invocation of the same isolate. The scope is the store's
  # identity, so this is what shows the cache outliving one request.
  second=$(app_request "$headers" "/api/cache")

  local from_default from_pages emptied empty_after survived kind first_id second_id
  from_default=$(echo "$first" | jq -r '.fromDefault // empty' 2>/dev/null || true)
  from_pages=$(echo "$first" | jq -r '.fromPages // empty' 2>/dev/null || true)
  emptied=$(echo "$first" | jq -r '.emptied // empty' 2>/dev/null || true)
  empty_after=$(echo "$first" | jq -r '.namedCacheEmptyAfterDelete // empty' 2>/dev/null || true)
  survived=$(echo "$first" | jq -r '.defaultSurvived // empty' 2>/dev/null || true)
  kind=$(echo "$first" | jq -r '.scopeKind // empty' 2>/dev/null || true)
  first_id=$(echo "$first" | jq -r '.scopeId // empty' 2>/dev/null || true)
  second_id=$(echo "$second" | jq -r '.scopeId // empty' 2>/dev/null || true)

  if [ "$from_default" = "default-copy" ] && [ "$from_pages" = "pages-copy" ] \
    && [ "$emptied" = "true" ] && [ "$empty_after" = "true" ] \
    && [ "$survived" = "default-copy" ] && [ "$kind" = "isolate" ] \
    && [ -n "$first_id" ] && [ "$first_id" = "$second_id" ]; then
    pass_check "$title" \
      "caches.default and caches.open(\"pages\") answered the same key with \"${from_default}\" and \"${from_pages}\"; caches.delete(\"pages\") emptied that one and left the default entry; and two requests both reported scope kind \"${kind}\" with the same store id ${first_id}, which is what tells an entry that was evicted from a store that is new"
  else
    fail_check "$title" \
      "expected the default cache to answer 'default-copy' (got '${from_default:-<none>}'), the named cache 'pages-copy' (got '${from_pages:-<none>}'), delete to report true and empty the name (got '${emptied:-<none>}'/'${empty_after:-<none>}') while the default kept its entry (got '${survived:-<none>}'), and both requests to report scope kind 'isolate' (got '${kind:-<none>}') with one stable id (got '${first_id:-<none>}' then '${second_id:-<none>}')"
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

# ----------------------------------------------------------------------
# Automatic certificates
# ----------------------------------------------------------------------
#
# Five claims, and what each is judged on:
#
#   1. the names certified are the ones the control plane says are served -
#      judged on the authority's own record of what it issued;
#   2. a client that trusts the authority is served that certificate over the
#      proxy's TLS port, chosen by SNI - judged by curl completing a verified
#      handshake and the application answering;
#   3. the challenge path answers only what the proxy published - judged on
#      what the proxy answers for a token it never published and for a path
#      that tries to walk out of the challenge directory;
#   4. a certificate that is still current is not ordered again - judged on
#      the authority's count staying put across several renewal passes;
#   5. a second process over the same store reuses the account key and the
#      certificates - judged on the authority registering no second account
#      and issuing nothing more.

# Where the store keeps one name's files. Mirrors the layout in
# crates/deis-proxy/src/acme/store.rs: <storage>/<authority>/certificates/<name>/.
acme_certificate_file() {
  local name=$1
  local file=$2
  echo "${ACME_STORE_DIR}/127.0.0.1-${ACME_CA_PORT}/certificates/${name}/${file}"
}

# One second between rounds of a poll, paced by curl's own retry rather than
# by the shell counting seconds - the same reason `wait_for_http` above leaves
# the pacing to curl. Nothing is being waited *for* here: what decides whether
# a loop ends is the condition it re-reads after this returns. Port 9 is
# discard; nothing answers it, so curl fails, waits its retry delay and fails
# again, which is the second.
acme_pause() {
  curl -sf -o /dev/null -m 1 --retry 1 --retry-delay 1 --retry-all-errors \
    "http://127.0.0.1:9/" >/dev/null 2>&1 || true
}

# Poll until the authority reports it has issued for `$1` names, or give up.
# A real condition on every round - the authority's own count - and never a
# wait for a duration picked to be long enough.
wait_for_acme_issue() {
  local wanted=$1
  local attempts=${2:-60}
  local issued=0
  while [ "$attempts" -gt 0 ]; do
    issued=$(acme_ca_stats | jq -r '.issued | length' 2>/dev/null || echo 0)
    if [ "${issued:-0}" -ge "$wanted" ]; then
      return 0
    fi
    attempts=$((attempts - 1))
    acme_pause
  done
  return 1
}

check_acme_certificate_follows_the_routing_table() {
  local claim="the names certified are the ones the control plane says are served"

  if ! wait_for_acme_issue 2; then
    fail_check "$claim" \
      "the authority issued $(acme_ca_stats | jq -c '.issued') within the wait; the proxy log says: $(grep -a -i "certificate" "${LOG_DIR}/deis_proxy_error.log" 2>/dev/null | tail -n 3 | tr '\n' ' ')"
    return
  fi

  local issued
  issued=$(acme_ca_stats | jq -r '.issued | sort | join(", ")')
  case "$issued" in
    *"${ACME_PRODUCTION_NAME}"*)
      case "$issued" in
        *"${ACME_PREVIEW_NAME}"*) : ;;
        *)
          fail_check "$claim" "the authority issued for [${issued}], which does not include the preview environment's address ${ACME_PREVIEW_NAME}"
          return
          ;;
      esac
      ;;
    *)
      fail_check "$claim" "the authority issued for [${issued}], which does not include ${ACME_PRODUCTION_NAME}"
      return
      ;;
  esac

  # The names are nowhere in the proxy's configuration file. They were
  # synthesised from the applications and environments the control plane
  # publishes, which is what "derived rather than listed" means.
  if grep -q "$ACME_PRODUCTION_NAME" "$DEIS_PROXY_CONFIG"; then
    fail_check "$claim" \
      "${ACME_PRODUCTION_NAME} is written in ${DEIS_PROXY_CONFIG}, so it was configured rather than derived"
    return
  fi

  local name
  for name in "$ACME_PRODUCTION_NAME" "$ACME_PREVIEW_NAME"; do
    if [ ! -f "$(acme_certificate_file "$name" fullchain.pem)" ] \
      || [ ! -f "$(acme_certificate_file "$name" privkey.pem)" ]; then
      fail_check "$claim" \
        "the authority issued for ${name} but $(acme_certificate_file "$name" fullchain.pem) and its key are not both on disk, so a restart would order it again"
      return
    fi
  done

  local validated
  validated=$(acme_ca_stats | jq -r '[.validations[] | select(.matched) | .name] | sort | join(", ")')
  pass_check "$claim" \
    "the authority issued for [${issued}] and neither name appears anywhere in ${DEIS_PROXY_CONFIG}; both came from routing.preview_zone = \"${ACME_ZONE}\" applied to the app and environments the control plane publishes. It validated [${validated}] by fetching /.well-known/acme-challenge/<token> from the proxy's own HTTP port"
}

check_acme_certificate_is_served_over_tls() {
  local claim="a client that trusts the authority is served the issued certificate over TLS"
  local headers="${LOG_DIR}/verify-acme-tls.headers"
  local body status presented

  if [ ! -f "${ACME_CA_ROOT}" ]; then
    fail_check "$claim" "the authority's root was not written to ${ACME_CA_ROOT}, so nothing can verify the handshake"
    return
  fi

  # --cacert with nothing else: the handshake is verified against the stub
  # authority's root and against the SNI name. A proxy presenting a
  # self-signed certificate, or one for another name, fails here rather than
  # being reported as a success. --resolve is what stands in for the DNS
  # record that would point the name at this machine.
  # --noproxy '*' because every address in this demo is loopback and a
  # machine with https_proxy set would otherwise send this through it: curl
  # honours the proxy for a host name even when --resolve pins the address,
  # and the tunnel fails with an SSL error that looks like a certificate
  # problem and is not one.
  body=$(curl -s -D "$headers" -m 15 \
    --noproxy '*' \
    --cacert "${ACME_CA_ROOT}" \
    --resolve "${ACME_PRODUCTION_NAME}:${DEIS_PROXY_TLS_PORT}:127.0.0.1" \
    "https://${ACME_PRODUCTION_NAME}:${DEIS_PROXY_TLS_PORT}/" 2>"${LOG_DIR}/verify-acme-tls.log")

  status=$(http_status "$headers" 2>/dev/null || echo "")
  if [ -z "$status" ]; then
    fail_check "$claim" \
      "the TLS handshake did not complete: $(tr '\n' ' ' < "${LOG_DIR}/verify-acme-tls.log" | head -c 300)"
    return
  fi

  # What was actually presented for that SNI name, printed as the evidence.
  # Read a second time through s_client rather than inferred from curl's
  # success, so the line says which certificate rather than only that one
  # worked.
  presented=$(openssl s_client -connect "127.0.0.1:${DEIS_PROXY_TLS_PORT}" \
    -servername "${ACME_PRODUCTION_NAME}" </dev/null 2>/dev/null \
    | openssl x509 -noout -subject -issuer -ext subjectAltName 2>/dev/null \
    | tr '\n' ' ' | tr -s ' ')

  case "$presented" in
    *"${ACME_PRODUCTION_NAME}"*) : ;;
    *)
      fail_check "$claim" "the certificate presented for SNI ${ACME_PRODUCTION_NAME} was: ${presented:-nothing}"
      return
      ;;
  esac

  if [ "$status" != "200" ]; then
    fail_check "$claim" \
      "the handshake verified but the application answered ${status}: $(echo "$body" | tr '\n' ' ' | head -c 200)"
    return
  fi

  pass_check "$claim" \
    "curl --cacert ${ACME_CA_ROOT} https://${ACME_PRODUCTION_NAME}:${DEIS_PROXY_TLS_PORT}/ verified the chain against the authority's root and got HTTP ${status}; the certificate chosen for that SNI name was ${presented}"
}

check_acme_challenge_answers_only_what_was_published() {
  local claim="the challenge path answers only tokens this instance published"
  local unknown traversal encoded

  unknown=$(curl -s -o /dev/null -w '%{http_code}' -m 5 --noproxy '*' \
    -H "Host: ${ACME_PRODUCTION_NAME}" \
    "http://127.0.0.1:${DEIS_PROXY_PORT}/.well-known/acme-challenge/never-published-token")
  if [ "$unknown" != "404" ]; then
    fail_check "$claim" \
      "a token nothing published was answered ${unknown} rather than 404; an authority given anything else could believe a validation had succeeded"
    return
  fi

  # The token is a path segment chosen by whoever sends the request, and it
  # is used to name a file. Neither of these may read one.
  traversal=$(curl -s -m 5 --path-as-is --noproxy '*' \
    -H "Host: ${ACME_PRODUCTION_NAME}" \
    "http://127.0.0.1:${DEIS_PROXY_PORT}/.well-known/acme-challenge/../../../etc/passwd" | head -c 200)
  encoded=$(curl -s -m 5 --path-as-is --noproxy '*' \
    -H "Host: ${ACME_PRODUCTION_NAME}" \
    "http://127.0.0.1:${DEIS_PROXY_PORT}/.well-known/acme-challenge/..%2f..%2f..%2fetc%2fpasswd" | head -c 200)

  case "${traversal}${encoded}" in
    *root:*)
      fail_check "$claim" "a challenge path walked out of the challenge directory and read a file: ${traversal} ${encoded}"
      return
      ;;
  esac

  local served
  served=$(grep -a -c "an http-01 challenge answer was served" "${LOG_DIR}/deis_proxy_error.log" 2>/dev/null || echo 0)
  pass_check "$claim" \
    "the proxy served ${served} real challenge answer(s) to the authority, answered 404 to a token nothing published, and returned no file for ../../../etc/passwd in either its plain or its percent-encoded form"
}

check_acme_does_not_reissue_a_current_certificate() {
  local claim="a certificate that is still current is not ordered again"
  local before after passes

  before=$(acme_ca_stats | jq -r '.issued | length')
  # Wait for the renewal loop to run several more passes. The condition is
  # the proxy's own count of finished passes, read from its log, not a
  # duration: check_interval_secs is 2 in this configuration.
  local start_passes attempts
  start_passes=$(grep -a -c "automatic certificate pass finished" "${LOG_DIR}/deis_proxy_error.log" 2>/dev/null || echo 0)
  passes=$start_passes
  attempts=30
  while [ "$attempts" -gt 0 ]; do
    passes=$(grep -a -c "automatic certificate pass finished" "${LOG_DIR}/deis_proxy_error.log" 2>/dev/null || echo 0)
    if [ "$((passes - start_passes))" -ge 3 ]; then
      break
    fi
    attempts=$((attempts - 1))
    acme_pause
  done

  after=$(acme_ca_stats | jq -r '.issued | length')
  if [ "$((passes - start_passes))" -lt 3 ]; then
    fail_check "$claim" \
      "only $((passes - start_passes)) renewal pass(es) ran while this check watched, so nothing was really observed; the proxy log is ${LOG_DIR}/deis_proxy_error.log"
    return
  fi
  if [ "$after" != "$before" ]; then
    fail_check "$claim" \
      "the authority issued ${before} certificate(s) before three more renewal passes and ${after} after; a current certificate is being reordered, which on a real authority spends the fifty a registered domain gets per week"
    return
  fi

  pass_check "$claim" \
    "the renewal loop ran $((passes - start_passes)) more passes (check_interval_secs = 2) and the authority's issued count stayed at ${after}; the passes found the certificates on disk with more than renew_before_days left and asked for nothing"
}

check_acme_restart_reuses_the_account_and_certificates() {
  local claim="a second process over the same store reuses the account key and the certificates"
  local config="${DEMO_DIR}/deis-proxy-restart.toml"
  local log="${LOG_DIR}/deis-proxy-restart.log"
  local error_log="${LOG_DIR}/deis_proxy_restart_error.log"
  local http_port tls_port
  local accounts_before issued_before accounts_after issued_after

  accounts_before=$(acme_ca_stats | jq -r '.accounts')
  issued_before=$(acme_ca_stats | jq -r '.issued | length')

  # Its own ports, its own database and its own pid file, but the same ACME
  # storage directory. That directory is the whole question: an operator's
  # restart is a new process reading what the last one left.
  claim_port http_port "$((DEIS_PROXY_PORT + 100))"
  claim_port tls_port "$((DEIS_PROXY_TLS_PORT + 100))"

  cat > "$config" <<EOF
listen_addr = ["127.0.0.1"]
listen_ports = [${http_port}]
database = "${DEMO_DIR}/deis-proxy-restart.db"
pid_file = "${DEMO_DIR}/deis-proxy-restart.pid"
upgrade_sock = "${DEMO_DIR}/deis_proxy_restart_upgrade.sock"

[[upstreams]]
host = "127.0.0.1"
port = ${DEIS_RUNTIME_PORT}

[log.error_log]
level = "info"
path = "${error_log}"

[control]
addr = "http://127.0.0.1:${DEIS_CONTROL_PORT}"
associated_id = "deis-proxy-demo-restart"
secret = "${SECRET}"

[routing]
preview_zone = "${ACME_ZONE}"

[tls]
ports = [${tls_port}]

[tls.acme]
enabled = true
directory_url = "${ACME_CA_DIRECTORY:-http://127.0.0.1:${ACME_CA_PORT}/directory}"
contact_email = "ops@deislet.test"
terms_of_service_agreed = true
storage_dir = "${ACME_STORE_DIR}"
check_interval_secs = 2
retry_backoff_secs = 1
max_retry_backoff_secs = 4
include_preview_addresses = true
EOF

  rm -f "$error_log"
  "${BIN_DIR}/deis-proxy" -c "$config" > "$log" 2>&1 &
  local pid=$!

  # Wait for it to finish a renewal pass, which is the moment it has decided
  # whether anything needs ordering.
  local attempts=30
  local finished=0
  while [ "$attempts" -gt 0 ]; do
    if ! pid_running "$pid"; then
      break
    fi
    if grep -a -q "automatic certificate pass finished" "$error_log" 2>/dev/null; then
      finished=1
      break
    fi
    attempts=$((attempts - 1))
    acme_pause
  done

  accounts_after=$(acme_ca_stats | jq -r '.accounts')
  issued_after=$(acme_ca_stats | jq -r '.issued | length')

  local reused=0
  if grep -a -q "the stored ACME account key was loaded" "$error_log" 2>/dev/null; then
    reused=1
  fi

  stop_service "Restarted proxy" "$pid" >/dev/null 2>&1 || true
  rm -f "${DEMO_DIR}/deis-proxy-restart.db" "${DEMO_DIR}/deis-proxy-restart.pid"

  if [ "$finished" -ne 1 ]; then
    fail_check "$claim" \
      "the second process never finished a renewal pass; its log said: $(tail -n 5 "$error_log" 2>/dev/null | tr '\n' ' ' | head -c 300)"
    return
  fi
  if [ "$reused" -ne 1 ]; then
    fail_check "$claim" \
      "the second process did not report loading the stored account key, so it registered a new one; its log said: $(grep -a -i "account" "$error_log" 2>/dev/null | tail -n 3 | tr '\n' ' ' | head -c 300)"
    return
  fi
  if [ "$accounts_after" != "$accounts_before" ] || [ "$issued_after" != "$issued_before" ]; then
    fail_check "$claim" \
      "the authority went from ${accounts_before} account(s) and ${issued_before} certificate(s) to ${accounts_after} and ${issued_after}; a restart that re-registers or reissues spends limits that are counted per address and per registered domain"
    return
  fi

  pass_check "$claim" \
    "a second deis-proxy over ${ACME_STORE_DIR} logged \"the stored ACME account key was loaded\", finished a renewal pass, and left the authority at ${accounts_after} account and ${issued_after} certificates - it asked for nothing"
}

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
# ----------------------------------------------------------------------
# Accounts, scoped tokens and the audit log
# ----------------------------------------------------------------------
#
# `start` turns api_auth_enabled on and puts exactly one entry in api_keys,
# which is the shape of every installation that existed before there were
# accounts. These checks mint accounts and narrow tokens beside that key and
# watch the control plane tell them apart: the old flat key still does
# everything, a narrow one reaches one environment and nothing else, and both
# of them leave a line behind that names what they did.
#
# They run late on purpose. One of them re-points the preview environment at
# the version it is already running, which is a real deploy made with a real
# scoped credential; every check that expects a particular version on preview
# has finished by then.

# Run the CLI as somebody other than the demo's flat key.
#
# The assignment after the array is what does it: `env` applies assignments
# left to right, so this second DEIS_CONTROL_TOKEN replaces the one
# `deis_cli_env` put in the array. Everything else - the endpoints, the
# demo's own config home - stays exactly as `deis_cli` sets it, which is the
# point: only the credential differs.
deis_cli_as() {
  local credential=$1
  shift
  deis_cli_env
  env "${DEIS_CLI_ENV[@]}" "DEIS_CONTROL_TOKEN=${credential}" "${BIN_DIR}/deis" "$@"
}

# One request to the control plane with a chosen credential, answering with
# the HTTP status alone. Used where the claim is about who is refused, which
# `deis` deliberately reports as a message rather than as a number.
control_status() {
  local method=$1 path=$2 credential=$3 body=${4:-}
  local args=(-s -o /dev/null -w '%{http_code}' -X "$method" "$(control_url)${path}")
  if [ -n "$credential" ]; then
    args+=(-H "Authorization: Bearer ${credential}")
  fi
  if [ -n "$body" ]; then
    args+=(-H "Content-Type: application/json" --data "$body")
  fi
  curl "${args[@]}"
}

# The same, answering with the body, so a check can assert on what the caller
# was told rather than only on the number.
control_answer() {
  local method=$1 path=$2 credential=$3
  local args=(-s -X "$method" "$(control_url)${path}")
  if [ -n "$credential" ]; then
    args+=(-H "Authorization: Bearer ${credential}")
  fi
  curl "${args[@]}"
}

# A well-formed credential that was never minted, used to prove that a
# revoked token and one that never existed look identical to the caller.
NEVER_MINTED_TOKEN="deis_pat_00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff0011"

# Create an account if it is not already there. `verify` can be run twice
# against one demo, and the second run must not fail on a name that exists.
ensure_account() {
  deis_cli account create "$1" --role "$2" --json >/dev/null 2>&1 || true
}

check_the_configured_api_key_is_listed_as_unscoped() {
  local title="the api_keys entry from the configuration file is visible, and visibly unscoped"
  local listed
  listed=$(deis_cli token list --json 2>/dev/null || echo '{}')

  local unscoped origin scopes id
  unscoped=$(echo "$listed" | jq -r '.unscoped // 0')
  origin=$(echo "$listed" | jq -r '[.tokens[] | select(.origin == "config")] | length')
  scopes=$(echo "$listed" | jq -r 'first(.tokens[] | select(.origin == "config") | .scopes) // "none"')
  id=$(echo "$listed" | jq -r 'first(.tokens[] | select(.origin == "config") | .id) // "none"')

  if [ "${origin:-0}" -ge 1 ] && [ "${unscoped:-0}" -ge 1 ] && [ "$scopes" = "unscoped" ]; then
    pass_check "$title" \
      "\`deis token list\` shows ${origin} credential(s) whose origin is the configuration file, filed under id ${id} with scopes \"${scopes}\", and reports ${unscoped} live unscoped credential(s) in total. That number is what an operator drives to zero; the key itself is not in the database at all, only this record of it"
  else
    fail_check "$title" \
      "the listing reports ${origin:-0} configuration credential(s) with scopes '${scopes}' and ${unscoped:-0} unscoped in total; expected the demo's single api_keys entry to be listed and counted"
  fi
}

check_a_scoped_token_reaches_one_environment_and_no_other() {
  local title="a token scoped to one environment deploys there and is refused everywhere else"
  ensure_account ci deployer

  local minted token
  minted=$(deis_cli token create --account ci \
    --scope "deploy:${APP_ID}/${PREVIEW_ENV_NAME}" preview-pipeline --json 2>/dev/null || echo '{}')
  token=$(echo "$minted" | jq -r '.secret // empty')
  if [ -z "$token" ]; then
    fail_check "$title" "could not mint a scoped token: ${minted}"
    return
  fi

  # The version the preview environment already runs. Pointing it back at the
  # same version is a real deploy - a pointer row, a push to the node - and
  # changes nothing about what is being served, which is what makes it safe
  # to make with somebody else's credential in the middle of a demo.
  local preview_version
  preview_version=$(deployed_version "$PREVIEW_ENV_NAME")

  local allowed refused_env refused_secret refused_platform refused_token
  allowed=$(control_status POST \
    "/api/apps/${APP_ID}/environments/${PREVIEW_ENV_NAME}/deployments" "$token" \
    "{\"version_id\":\"${preview_version}\",\"note\":\"deployed by a scoped token\"}")
  refused_env=$(control_status POST \
    "/api/apps/${APP_ID}/environments/${ENV_NAME}/deployments" "$token" \
    "{\"version_id\":\"${preview_version}\"}")
  refused_secret=$(control_status PUT \
    "/api/apps/${APP_ID}/environments/${PREVIEW_ENV_NAME}/secrets/CI_ATTEMPT" "$token" \
    '{"value":"this must never be stored"}')
  refused_platform=$(control_status GET "/api/audit" "$token")
  refused_token=$(control_status GET "/api/tokens" "$token")

  local told
  told=$(control_answer POST \
    "/api/apps/${APP_ID}/environments/${ENV_NAME}/deployments" "$token" \
    | jq -r '.details // .error // empty')

  # Narrow has to stay usable. `deis status` walks every environment of the
  # application, so a credential that reaches one of them would break the
  # command outright if a refusal on the others were an error rather than a
  # row; this is the assertion that it is a row.
  local status_json="${LOG_DIR}/verify-scoped-status.json"
  local status_exit=0
  deis_cli_as "$token" status --json > "$status_json" 2>/dev/null || status_exit=$?
  local seen hidden
  seen=$(jq -r --arg env "$PREVIEW_ENV_NAME" \
    'first(.environments[] | select(.name == $env) | .version_number) // empty' \
    < "$status_json" 2>/dev/null || true)
  # `[...][0] | tostring` rather than jq's `//`: the value being looked for
  # here is `false`, and `//` treats false exactly like a missing field, so
  # the one answer this assertion is about would read as "no answer at all".
  hidden=$(jq -r --arg env "$ENV_NAME" \
    '[.environments[] | select(.name == $env) | .visible][0] | tostring' \
    < "$status_json" 2>/dev/null || true)

  if [ "$allowed" = "201" ] && [ "$refused_env" = "403" ] && [ "$refused_secret" = "403" ] \
    && [ "$refused_platform" = "403" ] && [ "$refused_token" = "403" ] \
    && [ "$status_exit" = "0" ] && [ -n "$seen" ] && [ "$hidden" = "false" ] \
    && echo "$told" | grep -q "deploy:${APP_ID}/${ENV_NAME}"; then
    pass_check "$title" \
      "carrying only deploy:${APP_ID}/${PREVIEW_ENV_NAME}, the token re-deployed ${PREVIEW_ENV_NAME} to ${preview_version} (HTTP ${allowed}); the same call against ${ENV_NAME} answered ${refused_env}, writing a secret on its own environment ${refused_secret}, reading the audit log ${refused_platform} and listing credentials ${refused_token}. The refusal names the scope that would have worked: \"${told}\". \`deis status\` still works for it: ${PREVIEW_ENV_NAME} shows version ${seen} and ${ENV_NAME} comes back visible=${hidden} rather than failing the command"
  else
    fail_check "$title" \
      "deploy to ${PREVIEW_ENV_NAME} answered ${allowed} (expected 201 Created); ${ENV_NAME} ${refused_env}, secret ${refused_secret}, audit ${refused_platform}, tokens ${refused_token} (expected 403 each). \`deis status\` exited ${status_exit} showing ${PREVIEW_ENV_NAME} version '${seen:-none}' and ${ENV_NAME} visible '${hidden:-none}'. The refusal said: ${told:-nothing}"
  fi
}

check_a_credential_cannot_wire_an_app_to_one_it_cannot_see() {
  local title="a service binding needs authority over the application it names"
  ensure_account app-team deployer

  # A credential that owns the caller outright and has never heard of the
  # callee. Uploading the example app's own bundle with it is the whole test:
  # that bundle declares "AUTH": a service binding to ${CALLEE_APP_ID}, which
  # is one application claiming a right to call another, and the route the
  # upload arrives on names only the caller.
  # `deploy` and nothing else, because that is the credential a build pipeline
  # for this application actually holds, and the account's own role -
  # `deployer` - is the ceiling that stops it holding more.
  local minted token
  minted=$(deis_cli token create --account app-team \
    --scope "deploy:${APP_ID}/*" \
    caller-pipeline --json 2>/dev/null || echo '{}')
  token=$(echo "$minted" | jq -r '.secret // empty')
  if [ -z "$token" ]; then
    fail_check "$title" "could not mint a scoped token: ${minted}"
    return
  fi

  local bundle="${DEMO_DIR}/app-v1/dist/deislet/bundle.zip"
  if [ ! -f "$bundle" ]; then
    fail_check "$title" "the example app's bundle is not at ${bundle}, so nothing could be uploaded"
    return
  fi

  local refused told
  refused=$(curl -s -o /dev/null -w '%{http_code}' -X POST \
    -H "Authorization: Bearer ${token}" -H "Content-Type: application/zip" \
    --data-binary "@${bundle}" "$(control_url)/api/apps/${APP_ID}/versions")
  told=$(curl -s -X POST \
    -H "Authorization: Bearer ${token}" -H "Content-Type: application/zip" \
    --data-binary "@${bundle}" "$(control_url)/api/apps/${APP_ID}/versions" \
    | jq -r '.error // empty' 2>/dev/null || true)

  # The same bundle, with the same account, once it may also *see* the callee.
  # Read is the weakest verb in the grammar, which is the point: this is a
  # boundary between teams, not a second deploy permission.
  local widened widened_token accepted
  widened=$(deis_cli token create --account app-team \
    --scope "deploy:${APP_ID}/*" --scope "read:${CALLEE_APP_ID}/*" \
    caller-pipeline-widened --json 2>/dev/null || echo '{}')
  widened_token=$(echo "$widened" | jq -r '.secret // empty')
  accepted=$(curl -s -o /dev/null -w '%{http_code}' -X POST \
    -H "Authorization: Bearer ${widened_token}" -H "Content-Type: application/zip" \
    --data-binary "@${bundle}" "$(control_url)/api/apps/${APP_ID}/versions")

  # And it is written down, like every other refusal on the human API.
  local audited
  audited=$(deis_cli audit --app "$APP_ID" --outcome refused --json 2>/dev/null \
    | jq -r '[.entries[] | select(.action == "version.upload" and .status == 403)] | length' 2>/dev/null || echo 0)

  if [ "$refused" = "403" ] \
    && echo "$told" | grep -q "read:${CALLEE_APP_ID}/\*" \
    && { [ "$accepted" = "200" ] || [ "$accepted" = "201" ]; } \
    && [ "${audited:-0}" -ge 1 ]; then
    pass_check "$title" \
      "a token carrying deploy:${APP_ID}/* -- everything a build pipeline for the caller needs -- could not upload a bundle declaring a service binding to ${CALLEE_APP_ID}: HTTP ${refused}, \"$(printf '%s' "$told" | head -c 140)\". Adding read:${CALLEE_APP_ID}/* to the same account let the identical bundle through (HTTP ${accepted}), and the refusal is in the audit log (${audited} refused version.upload row(s))"
  else
    fail_check "$title" \
      "expected 403 naming read:${CALLEE_APP_ID}/* and then an accepted upload; the narrow token got ${refused} saying \"$(printf '%s' "${told:-nothing}" | head -c 160)\", the widened one got ${accepted}, and the audit log holds ${audited:-0} refused version.upload row(s)"
  fi
}

check_a_wide_credential_cannot_mint_a_storage_token() {
  local title="minting a storage token needs that exact tenant written down"
  ensure_account data-team maintainer

  local wide narrow blanket_status
  wide=$(deis_cli token create --account data-team \
    --scope 'deploy:*/*' --scope 'secrets:*/*' everything --json 2>/dev/null \
    | jq -r '.secret // empty')
  narrow=$(deis_cli token create --account data-team \
    --scope "storage:${APP_ID}/${ENV_NAME}" one-tenant --json 2>/dev/null \
    | jq -r '.secret // empty')

  # A blanket storage grant is refused where it is written, not where it
  # would be used: the token it hands out never expires and works against
  # deis-store without this control plane in the path.
  blanket_status=0
  deis_cli token create --account data-team --scope 'storage:*/*' blanket --json \
    >/dev/null 2>&1 || blanket_status=$?

  local route="/api/apps/${APP_ID}/environments/${ENV_NAME}/storage-token"
  local refused allowed tenant
  refused=$(control_status GET "$route" "$wide")
  allowed=$(control_status GET "$route" "$narrow")
  tenant=$(control_answer GET "$route" "$narrow" | jq -r '.tenant // empty')

  if [ "$refused" = "403" ] && [ "$allowed" = "200" ] \
    && [ "$tenant" = "${APP_ID}.${ENV_NAME}" ] && [ "$blanket_status" -ne 0 ]; then
    pass_check "$title" \
      "a credential carrying deploy:*/* and secrets:*/* was refused GET ${route} (HTTP ${refused}); the one carrying storage:${APP_ID}/${ENV_NAME} was served (HTTP ${allowed}) for tenant ${tenant}. \`deis token create --scope storage:*/*\` was refused outright (exit ${blanket_status}), so there is no blanket form of the grant to hand out by accident"
  else
    fail_check "$title" \
      "the wide credential answered ${refused} (expected 403), the tenant-scoped one ${allowed} for tenant '${tenant:-none}' (expected 200 for ${APP_ID}.${ENV_NAME}), and minting storage:*/* exited ${blanket_status} (expected non-zero)"
  fi
}

check_a_revoked_token_stops_at_its_next_call() {
  local title="a revoked token is refused at once, and looks like one that never existed"
  ensure_account sre viewer

  local minted token id
  minted=$(deis_cli token create --account sre \
    --scope "read:${APP_ID}/${ENV_NAME}" laptop --json 2>/dev/null || echo '{}')
  token=$(echo "$minted" | jq -r '.secret // empty')
  id=$(echo "$minted" | jq -r '.id // empty')
  if [ -z "$token" ]; then
    fail_check "$title" "could not mint a token to revoke: ${minted}"
    return
  fi

  local route="/api/apps/${APP_ID}/environments/${ENV_NAME}/deployment"
  local before after unknown revoked_body unknown_body
  before=$(control_status GET "$route" "$token")
  deis_cli token revoke "$id" --json >/dev/null 2>&1 || true
  after=$(control_status GET "$route" "$token")
  unknown=$(control_status GET "$route" "$NEVER_MINTED_TOKEN")
  revoked_body=$(control_answer GET "$route" "$token")
  unknown_body=$(control_answer GET "$route" "$NEVER_MINTED_TOKEN")

  # The control plane's own log does tell the two apart, which is the point:
  # "a revoked token was presented" means a credential is still deployed
  # somewhere and someone has to go and replace it.
  local said
  said=$(grep -c 'the token presented was revoked' "${LOG_DIR}/deis-control.log" 2>/dev/null || true)

  if [ "$before" = "200" ] && [ "$after" = "401" ] && [ "$unknown" = "401" ] \
    && [ "$revoked_body" = "$unknown_body" ] && [ "${said:-0}" -ge 1 ]; then
    pass_check "$title" \
      "the token read ${route} (HTTP ${before}); after \`deis token revoke ${id}\` the very next request answered ${after}, byte-for-byte the same answer a token that was never minted gets (${unknown}). No cache had to expire: every request looks the row up. The control plane's log says \"the token presented was revoked\" ${said} time(s), which a stolen credential cannot see"
  else
    fail_check "$title" \
      "before revoking ${before} (expected 200), after ${after} and an unknown token ${unknown} (expected 401 each); the two answers were $( [ "$revoked_body" = "$unknown_body" ] && echo identical || echo different ), and the log named a revoked token ${said:-0} time(s)"
  fi
}

check_the_audit_log_names_who_changed_what() {
  local title="the audit log names the secret that was written, its application and its author"
  local report="${LOG_DIR}/verify-audit-secret.json"
  deis_cli audit --action secret.write --limit 20 --json > "$report" 2>/dev/null || true

  local total app environment target outcome actor from
  total=$(jq -r '.total // 0' < "$report" 2>/dev/null || echo 0)
  app=$(jq -r 'first(.entries[] | select(.target == "DEMO_SECRET") | .app_id) // empty' < "$report" 2>/dev/null || true)
  environment=$(jq -r 'first(.entries[] | select(.target == "DEMO_SECRET") | .environment) // empty' < "$report" 2>/dev/null || true)
  target=$(jq -r 'first(.entries[] | select(.target == "DEMO_SECRET") | .target) // empty' < "$report" 2>/dev/null || true)
  outcome=$(jq -r 'first(.entries[] | select(.target == "DEMO_SECRET") | .outcome) // empty' < "$report" 2>/dev/null || true)
  actor=$(jq -r 'first(.entries[] | select(.target == "DEMO_SECRET") | .actor_credential) // empty' < "$report" 2>/dev/null || true)
  # "From where" is half of what the line is for, and it is the field most
  # easily left empty: it comes from the connection rather than from the
  # request, so a server assembled without connect info would record every
  # call as coming from nowhere and nothing else would notice.
  from=$(jq -r 'first(.entries[] | select(.target == "DEMO_SECRET") | .peer) // empty' < "$report" 2>/dev/null || true)

  # The whole reason a secret write is recorded by name and never by value.
  local leaked=no
  grep -q "$DEMO_SECRET_VALUE" "$report" 2>/dev/null && leaked=yes

  if [ "$app" = "$APP_ID" ] && [ "$environment" = "$ENV_NAME" ] && [ "$target" = "DEMO_SECRET" ] \
    && [ "$outcome" = "accepted" ] && [ -n "$actor" ] && [ -n "$from" ] && [ "$leaked" = "no" ]; then
    pass_check "$title" \
      "\`deis audit --action secret.write\` returned ${total} line(s); the one for DEMO_SECRET names ${app}/${environment}, the secret ${target}, the credential ${actor} that wrote it, the address ${from} it came from and the outcome ${outcome}. The ${#DEMO_SECRET_VALUE}-character value is not in the report, and the audit table lives in the same database the secret sweep already greps"
  else
    fail_check "$title" \
      "the line for DEMO_SECRET reports app '${app:-none}', environment '${environment:-none}', target '${target:-none}', outcome '${outcome:-none}', credential '${actor:-none}' from '${from:-nowhere}' out of ${total} line(s), and the value was $( [ "$leaked" = yes ] && echo PRESENT || echo absent )"
  fi
}

check_a_refused_call_is_recorded_with_the_reason() {
  local title="a refused call is recorded as fully as an accepted one"
  local report="${LOG_DIR}/verify-audit-refused.json"
  deis_cli audit --outcome refused --limit 50 --json > "$report" 2>/dev/null || true

  local total deploy_refusal detail anonymous
  total=$(jq -r '.total // 0' < "$report" 2>/dev/null || echo 0)
  deploy_refusal=$(jq -r --arg app "$APP_ID" --arg env "$ENV_NAME" \
    'first(.entries[] | select(.action == "deploy" and .app_id == $app and .environment == $env) | .status) // empty' \
    < "$report" 2>/dev/null || true)
  detail=$(jq -r --arg app "$APP_ID" --arg env "$ENV_NAME" \
    'first(.entries[] | select(.action == "deploy" and .app_id == $app and .environment == $env) | .detail) // empty' \
    < "$report" 2>/dev/null || true)
  anonymous=$(jq -r '[.entries[] | select(.actor_kind == "anonymous")] | length' < "$report" 2>/dev/null || echo 0)

  if [ "${total:-0}" -ge 1 ] && [ "$deploy_refusal" = "403" ] \
    && echo "$detail" | grep -q "deploy:${APP_ID}/${ENV_NAME}"; then
    pass_check "$title" \
      "${total} refusal(s) are on the record, ${anonymous} of them from a caller who presented nothing at all. The scoped token's attempt to deploy ${APP_ID}/${ENV_NAME} is there as HTTP ${deploy_refusal} with the reason \"${detail}\" - which is how an operator finds a pipeline still holding a credential that was narrowed"
  else
    fail_check "$title" \
      "the log holds ${total:-0} refusal(s); the refused deploy on ${APP_ID}/${ENV_NAME} reports status '${deploy_refusal:-none}' with detail '${detail:-none}'"
  fi
}

check_the_audit_log_has_no_way_to_be_erased() {
  local title="no caller of this API can erase a line of the audit log"
  local before after removed created

  before=$(deis_cli audit --limit 500 --json 2>/dev/null | jq -r '.total // 0')
  # The flat api_keys entry is the most powerful credential this control
  # plane has - it is unscoped, so it passes every scope check there is. If
  # anything could delete a line, this would.
  removed=$(control_status DELETE "/api/audit" "$API_KEY")
  created=$(control_status POST "/api/audit" "$API_KEY" '{"action":"forged"}')
  after=$(deis_cli audit --limit 500 --json 2>/dev/null | jq -r '.total // 0')

  if [ "$removed" = "405" ] && [ "$created" = "405" ] && [ "${after:-0}" -ge "${before:-0}" ]; then
    pass_check "$title" \
      "DELETE /api/audit answered ${removed} and POST answered ${created} to the unscoped api_keys credential - the route is a read and there is no writing or deleting endpoint behind it. The log went from ${before} to ${after} line(s) across both attempts; removing one means holding the database file, which is the same access that holds the secret master key"
    else
    fail_check "$title" \
      "DELETE answered ${removed} and POST answered ${created} (expected 405 for both), and the log went from ${before:-?} to ${after:-?} lines"
  fi
}

check_node_registration() {
  local title="the node registers itself with the control plane"
  local fleet="${LOG_DIR}/verify-fleet.json"

  # No CLI command covers the fleet, so this reads the control plane's own
  # view of it. That is an operator question, not a deployment step.
  control_api /api/runtime > "$fleet" 2>/dev/null || true

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

  control_api /api/runtime > "$fleet" 2>/dev/null || true

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

# ----------------------------------------------------------------------
# `deis trace`
# ----------------------------------------------------------------------
#
# A log line says something happened; a trace record says one invocation
# happened and how the platform decided it ended. That second part is why the
# command exists next to `deis logs`: an application that fails silently writes
# no log line at all, and "the handler threw" and "we cut it off" are the same
# 500 to everyone except the node that made the decision.
#
# Both checks below read the stream rather than the stored records, for the
# reason check_cli_tail does: waiting for a record that is already there
# proves nothing about the invocation this check just caused, and a node
# buffers its traces and pushes them on a timer, so anything else would be a
# poll with a guess in it.

# Open `deis trace --follow` and return only once the stream is really open.
#
# The banner is the readiness signal. `deis trace --follow` prints it after the
# TailTraces call has returned, and deis-telemetry registers the subscriber
# inside that handler before it answers - so an invocation made from now on
# reaches this stream. Doing it the other way round is the classic flake: the
# record is pushed and broadcast while nobody is listening, and a follow that
# correctly shows only new invocations looks broken.
#
# `exec` so that the pid is the CLI itself and not a shell around it, which
# matters when it is interrupted; the coprocess file descriptors are global, so
# the caller reads from ${DEIS_TRACE[0]} and closes with close_trace_stream.
open_trace_stream() {
  local environment=$1
  local transcript=$2

  deis_cli_env
  coproc DEIS_TRACE {
    exec env "${DEIS_CLI_ENV[@]}" "${BIN_DIR}/deis" trace --follow -e "$environment" 2>&1
  }

  local line
  while IFS= read -r -t 30 -u "${DEIS_TRACE[0]}" line; do
    printf '%s\n' "$line" >> "$transcript"
    case "$line" in
      *"following invocations from now on"*) return 0 ;;
    esac
  done
  return 1
}

# Read rows until one contains $1, keeping every line for the failure message.
# Sets TRACE_ROW to the row that matched.
await_trace_row() {
  local wanted=$1
  local transcript=$2
  local line

  TRACE_ROW=""
  while IFS= read -r -t 60 -u "${DEIS_TRACE[0]}" line; do
    printf '%s\n' "$line" >> "$transcript"
    case "$line" in
      *"$wanted"*)
        TRACE_ROW=$line
        return 0
        ;;
    esac
  done
  return 1
}

# SIGINT, not SIGKILL: `deis trace --follow` races Ctrl-C against the stream so
# that it unwinds normally and closes the connection. That is the path a person
# takes to stop it, so it is the one worth exercising.
close_trace_stream() {
  kill -INT "$DEIS_TRACE_PID" 2>/dev/null || true
  wait "$DEIS_TRACE_PID" 2>/dev/null || true
}

check_cli_trace() {
  local title="deis trace reports the invocation a request just made"
  local out="${LOG_DIR}/verify-cli-trace.log"
  : > "$out"

  # A path nothing else asks for, so the row that arrives can only describe
  # this check's own request. Matching on a path the demo uses elsewhere would
  # pass on an invocation from a minute ago.
  local probe="trace-probe-$$-${RANDOM}"

  if ! open_trace_stream "$ENV_NAME" "$out"; then
    close_trace_stream
    fail_check "$title" \
      "the stream never opened; deis trace said: $(tr '\n' ' ' < "$out" | head -c 300)"
    return
  fi

  app_request "${LOG_DIR}/verify-cli-trace.headers" "/hello/${probe}" > /dev/null

  if await_trace_row "$probe" "$out"; then
    close_trace_stream
    pass_check "$title" \
      "GET /hello/${probe} was made after the stream opened, and deis trace printed: $(echo "$TRACE_ROW" | sed 's/[[:space:]][[:space:]]*/ /g')"
  else
    close_trace_stream
    fail_check "$title" \
      "the stream opened but no invocation naming ${probe} arrived within 60s; see ${out}"
  fi
}

# The one verdict no other observer on this platform can produce.
#
# V8's near-heap-limit callback is the only place on a node that knows an
# isolate died of its heap: from the request path an out-of-memory kill and a
# handler that threw are the same terminated execution, and both answer 500.
# The callback records `memory-limit` against the invocation and increments
# `deis_runtime_memory_cutoffs_total` in the same breath, so a row that says
# memory-limit is that callback having run.
#
# The counter itself is named in the evidence but is not what this check waits
# on: a node pushes its counters every ten seconds and its traces every one, so
# the verdict is here long before the sample is, and gating on the sample would
# make the check a race against a timer rather than a statement about the
# platform. The row is the deterministic witness; the counter is read once and
# reported as it stands.
# What deis-telemetry currently reports for this tenant's memory cut-offs, or
# nothing when no node has pushed that series yet.
memory_cutoff_counter() {
  curl -s -m 5 "http://127.0.0.1:${DEIS_TELEMETRY_METRICS_PORT}/metrics" 2>/dev/null \
    | grep "^deis_runtime_memory_cutoffs_total{app_id=\"${APP_ID}\",environment=\"${ENV_NAME}\"" \
    | awk '{print $NF}' || true
}

check_memory_cutoff() {
  local title="an isolate that exhausts its heap is cut off, and the platform says so"
  local out="${LOG_DIR}/verify-memory-cutoff.log"
  : > "$out"

  # Read before anything is driven, so the number afterwards is this
  # invocation's and not one left behind by an earlier `verify` against the
  # same running demo.
  local before
  before=$(memory_cutoff_counter)

  if ! open_trace_stream "$ENV_NAME" "$out"; then
    close_trace_stream
    fail_check "$title" \
      "the stream never opened; deis trace said: $(tr '\n' ' ' < "$out" | head -c 300)"
    return
  fi

  local answered status seconds
  answered=$(curl -s -o /dev/null -w '%{http_code} %{time_total}' \
    -H "x-deis-app-id: ${APP_ID}" \
    "http://127.0.0.1:${DEIS_RUNTIME_PORT}/api/heap")
  status=${answered%% *}
  seconds=${answered##* }

  if ! await_trace_row "memory-limit" "$out"; then
    close_trace_stream
    fail_check "$title" \
      "GET /api/heap answered HTTP ${status} after ${seconds}s, but no invocation ended as memory-limit within 60s - so either the ceiling was never reached or the near-heap-limit callback did not classify it; see ${out}"
    return
  fi
  close_trace_stream

  if [ "$status" != "500" ]; then
    fail_check "$title" \
      "an invocation was recorded as memory-limit, but GET /api/heap answered HTTP ${status} rather than failing; a handler killed for its heap cannot also have answered"
    return
  fi

  local ceiling after counter_note
  ceiling=$(node_log \
    | sed -n "s/.*App ${APP_ID}.${ENV_NAME} narrows the default heap ceiling: \([0-9]*\)MB.*/\1/p" \
    | head -n 1)
  after=$(memory_cutoff_counter)
  if [ -n "$after" ] && [ "$after" != "$before" ]; then
    counter_note="deis_runtime_memory_cutoffs_total for this tenant went from ${before:-nothing} to ${after} while this check ran"
  else
    counter_note="deis_runtime_memory_cutoffs_total for this tenant still reads ${after:-nothing}, which is the cadence and not a gap: a node pushes its traces every second and its cumulative counters every ten, so the verdict arrives first and the sample follows"
  fi

  pass_check "$title" \
    "GET /api/heap allocated past the ${ceiling:-?}MB heap this version's manifest asks for and answered HTTP ${status} after ${seconds}s. deis trace calls that invocation memory-limit, which nothing but the near-heap-limit callback can produce - from the request path an out-of-heap kill and a handler that threw are the same terminated execution: $(echo "$TRACE_ROW" | sed 's/[[:space:]][[:space:]]*/ /g'). The same callback increments deis_runtime_memory_cutoffs_total; ${counter_note}"
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
  # The three files a static site configures itself with, and the way back
  # into the asset layer from inside a function. All four run before the
  # rollback moves production off v1, which is the version whose bundle
  # carries them.
  check_site_redirect
  check_site_headers
  check_site_not_found
  check_assets_binding
  check_assets_binding_is_not_shadowable
  echo "-- routing --"
  check_route_parameter
  echo "-- websockets --"
  # Through deis-proxy, which is the only check here that goes through it.
  # Everything else addresses the node directly, so a proxy that mangled an
  # upgrade would have left every other check green.
  check_websocket_through_proxy
  echo "-- streaming responses --"
  # The first two go through deis-proxy for the same reason the WebSocket check
  # does: it is the only hop that could quietly collect a body, and a proxy that
  # buffered one would leave every other check here green.
  check_server_sent_events_reach_the_client_frame_by_frame
  check_a_streamed_response_is_chunked_and_left_uncompressed
  check_a_stream_fed_from_fetch_is_not_collected
  check_a_producer_that_throws_leaves_a_failed_transfer
  check_a_stream_stops_when_the_client_hangs_up
  check_a_request_is_served_while_a_stream_is_open
  # Last of the streaming checks: it is the only one that waits, because the
  # thing being watched is a ceiling running out.
  check_a_stream_past_its_budget_is_closed_by_the_node
  echo "-- the web platform --"
  # Here rather than at the end: these are about what a handler finds on
  # `globalThis`, which is the same subject as the check above, and they run
  # before the rollback moves production around underneath them.
  check_app_fetch_allowed
  check_app_fetch_refused
  check_web_crypto
  check_named_caches
  echo "-- secrets --"
  # The list runs before the sweep: it writes its own output under LOG_DIR,
  # and the sweep is what would catch a value in it.
  check_cli_secret_list
  check_secret_delivery
  echo "-- storage --"
  check_kv_roundtrip
  # Named bindings. The isolation check seeds the key the CLI check reads, so
  # it runs first; the hostile-name check needs nothing of theirs and only
  # asks deis-store.
  check_named_bindings_are_separate_namespaces
  check_a_binding_cannot_reach_another_tenant
  check_cli_reads_a_named_binding
  check_storage_capability_token
  check_storage_token_requires_the_api_key
  check_data_cli_reads_the_apps_write
  # Object storage past put / get / delete. The listing check seeds the
  # objects the four after it read, so it runs first.
  check_blob_listing
  check_blob_head
  check_blob_range_read
  check_blob_conditional_write
  check_cli_blob_verbs
  check_cli_blob_conditional_write
  echo "-- queue --"
  check_queue_roundtrip
  check_queue_send_batch
  check_queue_consumer_concurrency
  echo "-- cron --"
  check_cron
  echo "-- calling another application --"
  # The consent check redeploys the callee twice and puts it back the way it
  # was, so it runs after the two that need the callee as the run left it.
  check_one_application_calls_another
  check_a_service_binding_is_not_a_hole_in_the_outbound_policy
  check_a_preview_calls_the_preview_and_not_production
  check_a_callee_decides_who_may_call_it
  echo "-- environments --"
  check_second_environment
  check_environment_kv_isolation
  echo "-- resource limits --"
  # Both halves need production on v1 and the preview on v2: v1's manifest
  # narrows its timeout and v2's asks to widen it, so these run before the
  # rollback puts v2 on production.
  check_app_limits_narrowed
  check_app_limits_clamped
  # The subrequest ceiling, which is the one limit that bounds fan-out. Both
  # halves talk to the demo's own origin, which is the single address this
  # node's egress policy allows.
  check_subrequest_ceiling
  check_a_redirect_chain_costs_a_subrequest_per_hop
  check_memory_cutoff
  echo "-- compatibility date --"
  # Both halves need production on v1, whose sources name a date, and the
  # preview on v2, whose staged copy has the key deleted. They run before the
  # rollback for the same reason the limit checks do.
  check_compatibility_date_is_visible
  check_every_set_cookie_reaches_a_dated_application
  check_an_undated_application_keeps_the_old_answer
  check_a_date_this_platform_cannot_honour_is_refused
  check_an_unknown_compatibility_flag_is_refused
  echo "-- versions --"
  # This one moves production to v2 and back, so it runs after every check
  # that expects production to be on v1.
  check_rollback
  echo "-- observability --"
  check_telemetry_logs
  check_cli_trace
  check_cli_tail
  echo "-- the fleet --"
  check_node_registration
  check_dynamic_node_registration
  check_cli_status
  echo "-- accounts, scopes and the audit log --"
  # After every check that expects a particular version on the preview
  # environment: the scoped-token check re-deploys preview to the version it
  # is already running, which is what proves a narrow credential can really
  # ship rather than only be refused.
  check_the_configured_api_key_is_listed_as_unscoped
  check_a_scoped_token_reaches_one_environment_and_no_other
  check_a_credential_cannot_wire_an_app_to_one_it_cannot_see
  check_a_wide_credential_cannot_mint_a_storage_token
  check_a_revoked_token_stops_at_its_next_call
  check_the_audit_log_names_who_changed_what
  check_a_refused_call_is_recorded_with_the_reason
  check_the_audit_log_has_no_way_to_be_erased
  echo "-- automatic certificates --"
  # Last, because the final check starts a second deis-proxy over the same
  # certificate store and registers it with the control plane; running that
  # before the fleet checks would put an extra instance in front of them.
  check_acme_certificate_follows_the_routing_table
  check_acme_certificate_is_served_over_tls
  check_acme_challenge_answers_only_what_was_published
  check_acme_does_not_reissue_a_current_certificate
  check_acme_restart_reuses_the_account_and_certificates

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
  echo "- Origin PID: ${DEIS_ORIGIN_PID:-none}"
  echo "- Control API: http://127.0.0.1:${DEIS_CONTROL_PORT}"
  echo "- Runtime: http://127.0.0.1:${DEIS_RUNTIME_PORT}"
  echo "- Proxy: http://127.0.0.1:${DEIS_PROXY_PORT}"
  echo "- Store (gRPC): 127.0.0.1:${DEIS_STORE_PORT:-unknown}"
  echo "- Queue (gRPC): 127.0.0.1:${DEIS_QUEUE_PORT:-unknown}"
  echo "- Telemetry (gRPC): 127.0.0.1:${DEIS_TELEMETRY_PORT:-unknown}"
  echo "- Telemetry metrics: http://127.0.0.1:${DEIS_TELEMETRY_METRICS_PORT:-unknown}/metrics"
  echo "- Fetch origin: http://127.0.0.1:${DEIS_ORIGIN_PORT:-unknown}/payload.txt (PID: ${DEIS_ORIGIN_PID:-none})"
  echo "- Certificate authority: http://127.0.0.1:${ACME_CA_PORT:-unknown}/directory (PID: ${ACME_CA_PID:-none})"
  echo "- Proxy TLS: https://${ACME_PRODUCTION_NAME:-unknown}:${DEIS_PROXY_TLS_PORT:-unknown}/ (certificate obtained over ACME)"

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
    --deis-proxy-tls-port)
      DEIS_PROXY_TLS_PORT=$2
      shift 2
      ;;
    --acme-ca-port)
      ACME_CA_PORT=$2
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
    --deis-origin-port)
      DEIS_ORIGIN_PORT=$2
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
