#!/usr/bin/env bash

set -euo pipefail

ROOT_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
DEMO_DIR="${DEMO_DIR:-${ROOT_DIR}/.demo}"
LOG_DIR="${LOG_DIR:-${DEMO_DIR}/logs}"
STATE_FILE="${STATE_FILE:-${DEMO_DIR}/state.env}"
# Minted fresh on every start and thrown away by the next one; see
# `mint_fleet_identities`.
PKI_DIR="${PKI_DIR:-${DEMO_DIR}/pki}"
# The trust domain every name in this demo fleet belongs to. `.local` and a
# `demo.` label so that nothing here could be mistaken for a real fleet.
FLEET_TRUST_DOMAIN="${FLEET_TRUST_DOMAIN:-demo.deislet.local}"
# Where `cargo build` leaves the demo binaries. CARGO_TARGET_DIR is honoured
# because this repo is often built with a shared target directory.
BIN_DIR="${CARGO_TARGET_DIR:-${ROOT_DIR}/target}/debug"

DEFAULT_DEIS_CONTROL_PORT=8085
DEFAULT_DEIS_RUNTIME_PORT=8086
DEFAULT_DEIS_PROXY_PORT=8087
DEFAULT_DEIS_PROXY_TLS_PORT=8088
# The control plane's machine channel: a second listener carrying what only
# Deislet's own processes call, behind a handshake that demands a certificate
# from both ends. Separate from the port above so that what a browser reaches
# and what a node reaches are two sockets an operator can bind apart.
DEFAULT_DEIS_CONTROL_MACHINE_PORT=8089
# The node's own authenticated management plane: where the control plane
# pushes deployments and fires cron once it dials by certificate. A third
# socket on the node, beside its data port and the plaintext management
# plane, because a client-certificate demand on either of those would end
# every browser request or break the door the control plane drives today.
DEFAULT_DEIS_RUNTIME_MACHINE_PORT=8097
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
# A second, much smaller project, built and never deployed. The example app
# above declares queues, cron, KV and a service binding, and `deis build`
# refuses to build it for Deno Deploy because Deno Deploy has no queues - so
# the one project the demo deploys cannot also be the one that shows four
# targets coming out of one source tree. This is that one: a fetch handler and
# nothing else, which every vendor here can host.
PORTABLE_SRC_DIR="${PORTABLE_SRC_DIR:-${ROOT_DIR}/scripts/demo-assets/portable-app}"
# The string that handler answers with. Every vendor's output is searched for
# it, so an adapter that laid down the right file names around none of the
# application fails rather than passes.
PORTABLE_MARKER="portable-handler-marker"
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
# The proxy's own id. Spelled once because three things have to agree on it:
# what the proxy calls itself on the control channel, the name in its
# certificate, and the id it puts on a routing-table acknowledgement - which
# the control plane now holds against the name the connection proved.
PROXY_ID="${PROXY_ID:-deis-proxy-demo-01}"
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
# second with a 301; public/_headers is one block, /css/*, that puts a
# year-long Cache-Control and a Referrer-Policy on everything under /css/ and
# on nothing else; and no file, no rule and no function route claims
# SITE_MISSING_PATH, which is what makes it the path the app's not-found
# behaviour answers.
SITE_OLD_PATH="/old-home"
SITE_REDIRECT_TARGET="/index.html"
SITE_STYLESHEET="/css/app.css"
SITE_STYLESHEET_CACHE_CONTROL="public, max-age=31536000, immutable"
SITE_STYLESHEET_REFERRER_POLICY="strict-origin-when-cross-origin"
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

# The ceiling on the node's log file, in bytes.
#
# Why there is one at all. `${LOG_DIR}/deis-runtime.log` is not written only by
# the node: an application's own `console` line lands in it, and an
# application's line carries whatever a request put in it. So the file's size
# is decided by whoever can reach the node's port, which on this demo is
# anybody - the routes are unauthenticated by design, that being what most of
# the checks exercise. Measured against this demo as it shipped: one
# unauthenticated `GET /hello/<7000 characters>` adds about 51KB to the file,
# so twenty thousand of them fill a gigabyte, and nothing anywhere was
# counting.
#
# Why the number is this large. A single, entirely ordinary `verify` run
# already writes about 35MB into it, almost all of which is `h2` and `hyper`
# TRACE events: deis-runtime installs no level filter on its subscriber
# (crates/deis-runtime/src/main.rs), so every event every dependency emits is
# written out. A ceiling has to leave a normal run far below it or it becomes
# a way for the demo to lose its own evidence, so this is seven times what a
# run needs. Lowering it is a matter of that filter, not of this number.
#
# What happens at the ceiling is not silence: the writer says it stopped, and
# the last check in `verify` reads the file for that sentence and fails the
# run. A run whose log was cut short is a run whose log-reading checks were
# reading a partial file, and that has to be said rather than discovered.
DEMO_LOG_MAX_BYTES="${DEMO_LOG_MAX_BYTES:-268435456}"

# The sentence the writer leaves behind when it stops. Written here so the
# writer and the check that looks for it cannot drift apart.
DEMO_LOG_CAPPED_MARKER="deis demo: this log reached its ceiling and was cut off here"

# Copy stdin into $1, stopping at DEMO_LOG_MAX_BYTES.
#
# `LC_ALL=C` so `length()` counts bytes rather than characters - the ceiling
# is a disk ceiling. `fflush()` after every line because the readiness probes
# and fifteen checks read this file while it is being written, and an awk
# that buffered would make the node look silent. The line that ends it is
# written *through* the same fflush, so the marker is on disk before anything
# further is dropped.
bounded_log() {
  local file=$1
  LC_ALL=C awk -v cap="$DEMO_LOG_MAX_BYTES" -v marker="$DEMO_LOG_CAPPED_MARKER" '
    BEGIN { written = 0; capped = 0 }
    capped { next }
    {
      len = length($0) + 1
      if (written + len > cap) {
        print marker " (" cap " bytes). Everything after this point was dropped, so any check that reads this file from here on is reading a partial log."
        fflush()
        capped = 1
        next
      }
      print
      written += len
      fflush()
    }
  ' > "$file"
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
    "${DEMO_DIR}/deis-config" \
    "${PKI_DIR}"
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

# ---- the demo's own certificate authority ---------------------------------
#
# The machine channel authenticates both ends by certificate, so this script
# has to issue some. openssl is already what `generate_secret` reaches for, so
# this adds no tool; what it does add is the one shape of certificate the
# platform will actually load, in a place where getting it wrong is visible.
#
# Nothing about this is how a real installation gets its certificates. A real
# one has a certificate authority with a life longer than one script run, and
# `deis-identity` deliberately knows nothing about where the three files came
# from. Two days of validity is chosen so a `.demo` directory left over from
# last week fails at load with something an operator can read, rather than at
# a handshake days later.
#
# Two things here are easy to get wrong and expensive to diagnose:
#
#   * the extensions go in an `-extfile`, not `-addext`. `openssl x509 -req`
#     does not carry extensions over from the signing request unless it is
#     given `-copy_extensions`, which is not spelled the same way across the
#     openssl versions this script runs on. Get it wrong and every certificate
#     is signed with no URI subject alternative name at all, which surfaces
#     much later as `MachineIdentity::load` reporting that a certificate names
#     no machine, with nothing pointing back at this function.
#   * every leaf carries both `serverAuth` and `clientAuth`, because each
#     process here is a server on one connection and a client on another: the
#     control plane serves the machine channel and dials its nodes, and a node
#     serves its management plane and dials the control plane.
mint_ca() {
  local prefix=$1
  local subject=$2
  openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:P-256 -nodes \
    -keyout "${prefix}.key" -out "${prefix}.crt" -days 2 \
    -subj "/CN=${subject}" >/dev/null 2>&1
  chmod 600 "${prefix}.key"
}

# mint_leaf <ca-prefix> <leaf-prefix> <spiffe-name> [extra-san]
mint_leaf() {
  local ca=$1
  local leaf=$2
  local spiffe=$3
  local extra=${4:-}
  local san="URI:${spiffe}"
  if [ -n "$extra" ]; then
    san="${san},${extra}"
  fi
  # The subject is decoration and is deliberately not the SPIFFE name: the
  # identity lives in the URI subject alternative name below, which is the only
  # thing the platform reads, and openssl parses a `/` in a subject string as
  # the separator between one relative distinguished name and the next - so a
  # `spiffe://` subject is refused outright with a message about a missing `=`.
  openssl req -newkey ec -pkeyopt ec_paramgen_curve:P-256 -nodes \
    -keyout "${leaf}.key" -out "${leaf}.csr" -subj "/CN=${leaf##*/}" \
    >/dev/null 2>&1
  cat > "${leaf}.ext" <<EXT
subjectAltName=${san}
basicConstraints=CA:FALSE
extendedKeyUsage=serverAuth,clientAuth
EXT
  openssl x509 -req -in "${leaf}.csr" -CA "${ca}.crt" -CAkey "${ca}.key" \
    -CAcreateserial -days 2 -extfile "${leaf}.ext" -out "${leaf}.crt" \
    >/dev/null 2>&1
  chmod 600 "${leaf}.key"
  rm -f "${leaf}.csr" "${leaf}.ext"
}

# One authority, three machines, and two certificates that must not work.
#
# The last two are the capability this script did not have before: a wrong
# credential used to be a garbage string, and under a fleet-wide key that was
# the only wrong credential expressible. Now it can be a second identity that
# was really issued - by our own authority, naming a machine this one is not -
# and one issued by an authority this fleet never heard of. Neither can be
# faked with a bad string; both have to be minted.
mint_fleet_identities() {
  mkdir -p "$PKI_DIR"
  mint_ca "${PKI_DIR}/ca" "deislet demo fleet CA"

  # The control plane's own leaf carries an address as well as a name. Peers
  # authenticate it by the SPIFFE name alone and would not need one, but this
  # script's own probes are curl and grpcurl, which verify a server by host or
  # address in the ordinary way before any of that. A leaf with only a URI SAN
  # makes every probe fail hostname verification, and reaching for `-k` to get
  # past it would throw away what the probe is there to prove. That is a demo
  # harness concession, not a product requirement.
  mint_leaf "${PKI_DIR}/ca" "${PKI_DIR}/control" \
    "spiffe://${FLEET_TRUST_DOMAIN}/deislet/control" "IP:127.0.0.1"
  mint_leaf "${PKI_DIR}/ca" "${PKI_DIR}/node" \
    "spiffe://${FLEET_TRUST_DOMAIN}/deislet/node/${NODE_ID}" "IP:127.0.0.1"
  mint_leaf "${PKI_DIR}/ca" "${PKI_DIR}/proxy" \
    "spiffe://${FLEET_TRUST_DOMAIN}/deislet/proxy/${PROXY_ID}"

  # The replacement proxy the zero-downtime restart check starts. It is a
  # second machine with a name of its own, not the same one twice: the
  # handover is two processes alive at once, and each has to be able to say
  # which it is.
  mint_leaf "${PKI_DIR}/ca" "${PKI_DIR}/proxy-restart" \
    "spiffe://${FLEET_TRUST_DOMAIN}/deislet/proxy/deis-proxy-demo-restart"

  # A second real machine of this fleet: the node that joins by registering,
  # which it can only do by proving a name of its own.
  mint_leaf "${PKI_DIR}/ca" "${PKI_DIR}/unlisted" \
    "spiffe://${FLEET_TRUST_DOMAIN}/deislet/node/${NODE_ID}-unlisted" "IP:127.0.0.1"

  # Signed by this fleet, naming a node that is not the one running here.
  mint_leaf "${PKI_DIR}/ca" "${PKI_DIR}/impostor" \
    "spiffe://${FLEET_TRUST_DOMAIN}/deislet/node/deis-runtime-demo-99" "IP:127.0.0.1"

  # Well formed, correctly named, and signed by nobody this fleet trusts.
  mint_ca "${PKI_DIR}/rogue-ca" "somebody else entirely"
  mint_leaf "${PKI_DIR}/rogue-ca" "${PKI_DIR}/rogue" \
    "spiffe://${FLEET_TRUST_DOMAIN}/deislet/node/${NODE_ID}" "IP:127.0.0.1"
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

# Wait until deis-proxy can actually route to the deployed application.
#
# Answering the port is not enough, and since the edge was made to fail closed
# it is not even close: a proxy that has never managed to fetch its routing
# table refuses every request with 503 rather than forwarding it blind, and it
# is listening the whole time it does so. So `start` used to return while the
# front door was still turning traffic away, and the checks that go through it
# - every streaming check - failed on a demo that was
# perfectly healthy a second later.
#
# The probe is therefore a real request for the application that was just
# deployed, retried until the proxy answers it. curl paces its own retries and
# `--retry-all-errors` covers the 503, so there is no shell-level sleeping and
# nothing here waits for a guessed duration.
wait_for_proxy_routing() {
  local attempts=$1
  curl -sf -o /dev/null -m 2 \
    --retry "$attempts" --retry-delay 1 --retry-max-time $((attempts * 2)) \
    --retry-connrefused --retry-all-errors \
    -H "x-deis-app-id: ${APP_ID}" \
    "http://127.0.0.1:${DEIS_PROXY_PORT}/" >/dev/null 2>&1
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
DEIS_CONTROL_MACHINE_PORT=$DEIS_CONTROL_MACHINE_PORT
DEIS_RUNTIME_MACHINE_PORT=$DEIS_RUNTIME_MACHINE_PORT
PKI_DIR=$PKI_DIR
FLEET_TRUST_DOMAIN=$FLEET_TRUST_DOMAIN
PROXY_ID=$PROXY_ID
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
DEMO_APP_DEPLOY_FROM=${DEMO_APP_DEPLOY_FROM:-0}
DEMO_APP_DEPLOY_TO=${DEMO_APP_DEPLOY_TO:-0}
DEMO_PREVIEW_DEPLOY_FROM=${DEMO_PREVIEW_DEPLOY_FROM:-0}
DEMO_PREVIEW_DEPLOY_TO=${DEMO_PREVIEW_DEPLOY_TO:-0}
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
  chmod 600 "$DEIS_TELEMETRY_CONFIG"

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
  chmod 600 "$DEIS_STORE_CONFIG"

  cat > "$DEIS_QUEUE_CONFIG" <<EOF
listen = "127.0.0.1:${DEIS_QUEUE_PORT}"
data_dir = "${DEMO_DIR}/deis-queue-data"
auth_secret = "${STORAGE_AUTH_SECRET}"
EOF
  chmod 600 "$DEIS_QUEUE_CONFIG"

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
  #
  # /redirect-to is the third, and it is the security half of the same
  # sentence: it answers 302 with whatever Location it is handed. Every hop of
  # /redirect stays inside this origin, which the node allows, so a node that
  # judged only the URL the application named would pass the ceiling check
  # unchanged. Pointing a hop somewhere the policy refuses is the only way to
  # show that each hop is judged, and it needs an origin willing to send a
  # visitor off its own address -- which is what an open redirect on a
  # perfectly ordinary upstream is.
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
        if parsed.path == "/redirect-to":
            self.send_redirect_to(urllib.parse.parse_qs(parsed.query))
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

    def send_redirect_to(self, query):
        # One hop, to an address chosen by the caller. The Location is sent
        # exactly as given, absolute host and all: what is being exercised is
        # a hop that leaves this origin, so rewriting it to something local
        # would remove the only thing this route is for.
        self.send_response(302)
        self.send_header("Location", query.get("url", ["/payload.txt"])[0])
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
#   1. `deis build --vendor deislet`  -> dist/providers/deislet/<digest>/bundle.zip
#   2. POST /api/v1/apps              -> the application exists
#   3. POST .../versions              -> the bundle becomes an immutable version
#   4. POST .../environments          -> production exists
#   5. POST .../deployments           -> the environment points at the version
#
# Step 5 is where the control plane pushes a pointer to every node; each node
# downloads the bundle over HTTP, checks its sha256 and loads it. The bundle
# itself never travels inside a gRPC message.

# Print the control plane's base URL.
#
# Every path this file appends to it is spelled under one of the two versioned
# surfaces, never the flat `/api/...` tree the control plane still serves for
# callers written before them: `/api/v1/...` for the product API - what a
# developer's application, its versions, environments, deployments, secrets,
# hostname grants, credentials and audit log are - and `/api/platform/...` for
# the operator diagnostics, which here are the fleet view, the global routing
# table and taking an application off a node. The flat tree carries
# `Deprecation` and `Sunset` on every answer, so a demo that kept spelling it
# would be quoting the surface the platform is asking people to leave.
#
# The two versioned surfaces also answer differently, and the checks below are
# written for those answers rather than the flat tree's: a list comes back as
# {schemaVersion, items, page} and a refusal as {"error":{code, message,
# requestId, violations, ...}}, so a list is read at `.items[]` and a refusal
# at `.error.message`.
#
# `PUT /api/nodes/<id>/registration` is the one path here that is none of
# this. It is the channel a node itself calls, it is not part of the Control
# API, and it is not versioned; see check_the_node_channel_is_not_on_the_human_listener.
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
  curl -s -H "Authorization: Bearer ${API_KEY}" "$(control_url)$1" || true
}

# Every item of one Control API list, whatever its size, as one JSON array.
#
# A list route answers one page at a time - at most 1000 items, 100 when no
# limit is named - and says where the next one starts in `page.nextCursor`.
# A check that reads only the first page is a check against a small demo,
# not against the catalog it is really run on: an item on the second page is
# invisible to it. So this walks the cursor until the route says there is no
# more, and a caller that wants "is X already there" gets to ask the whole
# collection. The path must carry no query string of its own.
control_list_all() {
  local path=$1
  local page cursor items='[]'
  page=$(control_api "${path}?limit=1000")
  items=$(jq -c --argjson page "${page:-null}" '. + ($page.items // [])' <<<"$items")
  cursor=$(jq -r '.page.nextCursor // empty' <<<"${page:-null}" 2>/dev/null)
  while [ -n "$cursor" ]; do
    page=$(control_api "${path}?limit=1000&cursor=${cursor}")
    items=$(jq -c --argjson page "${page:-null}" '. + ($page.items // [])' <<<"$items")
    cursor=$(jq -r '.page.nextCursor // empty' <<<"${page:-null}" 2>/dev/null)
  done
  printf '%s\n' "$items"
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
    # Where deis-queue listens, for `deis queue dead-letters` and `deis queue
    # replay`. It is not fetched from the control plane the way deis-store's
    # address is - the storage-token route answers with the store's address
    # and the tenant's token, and that same token is what deis-queue checks -
    # so the CLI is told it here, exactly as a person would put it in
    # deis.toml under [queue].
    "DEIS_QUEUE_ENDPOINT=http://127.0.0.1:${DEIS_QUEUE_PORT}"
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

  add_blob_route "$dest"
  add_stream_route "$dest"
  add_timers_route "$dest"
  add_cookie_route "$dest"

  echo "$dest"
}

# The route the streaming checks talk to.
STREAM_ROUTE="/api/stream"

# Give the staged copy a streaming handler if the example app does not carry
# one.
#
# Written here for the same reason the object-storage handler is: the checks
# below need an application that actually returns a body it is
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
//   throw    `after` frames and then a failure, which the client meets long
//            after it was told 200.
//   relay    `url` is fetched and its body handed straight on. Nothing here
//            reads it, so nothing here holds it.
//   report   not a stream at all: what became of the producers above.
//
// The producer below is a loop writing into the identity `new
// TransformStream()`, rather than the underlying source of a `new
// ReadableStream({ pull, cancel })`, because the identity transform and the
// bodies of Request and Response are the whole of the stream surface Edge
// Canon v1 defines. A direct `ReadableStream`/`WritableStream` constructor is
// refused by the build (EC_STREAM_DIRECT_CONSTRUCTOR_NONPORTABLE), and so is a
// `TransformStream` handed a transformer or a queuing strategy
// (EC_STREAM_TRANSFORMER_NONPORTABLE) - the shapes that only some providers
// have. So the three things the underlying source used to be told by being
// called back are read off the writer instead:
//
//   backpressure  `writer.ready` and the promise `writer.write()` returns are
//                 what `pull` was: the identity transform gives its readable
//                 end a high water mark of zero, so neither settles until the
//                 node's pump asks for the next chunk. That is what keeps the
//                 frames from being produced in one burst before anyone reads
//                 them, and it is what the gap below is measured against.
//   cancellation  when the node cancels the readable - the client hung up, or
//                 the stream ran past stream_timeout_ms - the transform errors
//                 its writable end, so `writer.closed` rejects. That rejection
//                 is this handler's `cancel` hook. It is watched rather than
//                 waited for at the next write, because by the time it arrives
//                 the stream's owner has been released and the timer this
//                 producer is sitting on has been swept with it: a loop that
//                 only noticed at its next write would never reach one.
//   failure       `writer.abort(...)` errors the readable the node is
//                 draining, which is what ends the body without its
//                 terminating chunk.
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

  // One line at the top of this invocation, carrying the tag the caller
  // invented for this stream, before anything is produced.
  //
  // It is not written for a person. The node files every line of one
  // invocation -- this one, and its own later line about what became of the
  // body -- under the same `trace_id`, so a caller that can find this line
  // learns the id of the invocation it has just started and can then tell the
  // node's sentence about *its* stream from a sentence about somebody else's.
  // Without it the only handle on the node's line is its wording, which is the
  // same for every stream on the node - see `trace_of_witness` in
  // scripts/demo.sh for what that cost.
  if (tag !== "") console.log("stream witness " + tag);

  const frames = Math.min(Math.max(Number(url.searchParams.get("frames")) || 5, 1), 100);
  const gapMs = Math.min(Math.max(Number(url.searchParams.get("gapMs")) || 120, 0), 2000);
  const throwAfter = mode === "throw"
    ? Math.min(Math.max(Number(url.searchParams.get("after")) || 2, 1), 100)
    : -1;

  const encoder = new TextEncoder();
  // The one stream shape every target defines: bytes written at one end come
  // out of the other unchanged. The readable half becomes the response body,
  // the writable half is this handler's.
  const identity = new TransformStream();
  const writer = identity.writable.getWriter();

  let sent = 0;
  // Set just before this producer ends its own stream, so that the rejection
  // an abort causes is not mistaken for the node cancelling us. The
  // distinction is the whole of what `mode=throw` and `mode=endless` are for:
  // a stream this handler failed on purpose was not cancelled, and reporting
  // it as one would make the hang-up evidence unreadable.
  let ending = false;
  let cancelled = false;

  // The notice that there is nobody left to write to, and the promise the
  // pause below races against so that a producer parked on a swept timer still
  // returns. `writer.closed` rejects with the reason the node cancelled the
  // readable with; it resolves, without rejecting, when this producer closes
  // the stream itself.
  const stopped = writer.closed.then(
    () => {},
    () => {
      if (ending) return;
      cancelled = true;
      lastCancelled = tag;
    },
  );

  const pause = (ms: number) =>
    new Promise((resolve) => setTimeout(resolve, ms));

  // Started and not awaited: the head has to reach the client before the node
  // begins draining the readable, and nothing below can make progress until it
  // does.
  const produce = async () => {
    for (;;) {
      // The consumer asking for the next chunk. Everything after it is on the
      // far side of a read, so nothing is produced for a client that is not
      // there.
      try {
        await writer.ready;
      } catch (_) {
        return;
      }
      if (cancelled) return;

      // The gap is what makes "frame by frame" visible from outside: frames
      // produced in one burst would arrive in one burst no matter how they
      // crossed the boundary, and would prove nothing.
      if (gapMs > 0 && sent > 0) {
        await Promise.race([pause(gapMs), stopped]);
        if (cancelled) return;
      }

      if (throwAfter > 0 && sent >= throwAfter) {
        ending = true;
        try {
          // Errors the readable the node is draining. Every frame written so
          // far has already been taken by the pump -- that is what let the
          // `writer.ready` above settle -- so none of them is discarded along
          // with the queue, and what the client loses is only the terminating
          // chunk.
          await writer.abort(
            new Error(
              "the demo handler failed this stream on purpose after " + sent + " frame(s)",
            ),
          );
        } catch (_) {
          // The stream was already over. There is no second failure to report
          // and no client left to report it to.
        }
        return;
      }

      if (mode === "sse" && sent >= frames) {
        ending = true;
        try {
          await writer.close();
        } catch (_) {
          // As above: a stream the node ended first is ended either way.
        }
        return;
      }

      sent += 1;
      framesProduced += 1;
      try {
        await writer.write(encoder.encode("data: frame " + sent + " at " + Date.now() + "\n\n"));
      } catch (_) {
        // Rejects for the same reason `writer.closed` does, and the handler
        // attached to that has already recorded the tag.
        return;
      }
    }
  };

  // Caught here rather than left to the isolate. An unhandled rejection is not
  // a local failure: the runtime reports it against the event loop, which ends
  // every other stream this application has open along with this one.
  produce().catch(() => {});

  return new Response(identity.readable, {
    headers: {
      "content-type": "text/event-stream",
      "cache-control": "no-cache",
    },
  });
}
TS
}

# The route the timer-ownership check talks to.
TIMER_ROUTE="/api/timers"

# Give the staged copy a handler that deliberately leaves timers behind.
#
# Written here for the same reason the streaming handler is: the check below
# needs an application that actually tries to keep work running past its own
# request, and the interesting part is what the node does about it. The moment
# `functions/api/timers.ts` appears in the example app's own sources this stops
# writing anything.
add_timers_route() {
  local dest=$1
  local handler="${dest}/functions${TIMER_ROUTE}.ts"

  if [ -f "$handler" ]; then
    return 0
  fi

  mkdir -p "$(dirname "$handler")"
  cat > "$handler" <<'TS'
// Everything an application can do to keep a timer running past its request.
//
// An isolate is reused for every request to this application on a node, so a
// timer left armed does not stop when the request that armed it does: it fires
// inside later requests, on their CPU and against their rate limit. `?mode=arm`
// tries every way there is to leave one behind, and `?mode=report` -- a
// separate request, in the same isolate -- says whether any of them is still
// ticking.
//
// The counters are module-level on purpose: that is what makes "still ticking"
// a fact a later request can read.
let intervalTicks = 0;
let timeoutTicks = 0;
let ownerTicks = 0;
let doors: Record<string, string> = {};

function json(payload: unknown): Response {
  return new Response(JSON.stringify(payload), {
    headers: { "content-type": "application/json; charset=utf-8" },
  });
}

export default async function handler(context: any): Promise<Response> {
  const url = new URL(context.request.url);

  if (url.searchParams.get("mode") !== "arm") {
    // Long enough that a survivor ticking every 5ms would be seen many times
    // over, and short enough not to be a wait.
    const before = { intervalTicks, timeoutTicks, ownerTicks };
    await new Promise((resolve) => setTimeout(resolve, 250));
    return json({ doors, before, after: { intervalTicks, timeoutTicks, ownerTicks } });
  }

  // The same marker the streaming handler writes, for the same reason: the
  // node's own warning about the timers this invocation left armed is written
  // under this invocation's `trace_id`, and this line is how a caller learns
  // what that id is. See `trace_of_witness` in scripts/demo.sh.
  const tag = url.searchParams.get("tag") ?? "";
  if (tag !== "") console.log("timer witness " + tag);

  doors = {};

  // The plain one. Never stops on its own.
  setInterval(() => { intervalTicks += 1; }, 5);

  // The one that used to go untracked, on the reasoning that a setTimeout
  // fires once and is gone. This callback arms the next one before it returns,
  // which is an interval written in two lines.
  const again = () => { timeoutTicks += 1; setTimeout(again, 5); };
  setTimeout(again, 5);

  // The two doors that used to let a handler choose what owns a timer, and so
  // file one in a set the end-of-invocation sweep never looks in.
  const runtime = globalThis as any;
  doors.scope = typeof runtime.__deis_runtime_stream_scope;
  try {
    doors.claim = runtime.__deis_runtime_claim_stream_scope() === null
      ? "null"
      : "handed over";
  } catch (e: any) {
    doors.claim = "threw:" + (e && e.name ? e.name : String(e));
  }
  try {
    const scope = runtime.__deis_runtime_stream_scope;
    scope.enter({ guards: new Set(), intervals: new Set(), timeouts: new Set() });
    setInterval(() => { ownerTicks += 1; }, 5);
    scope.leave();
    doors.invented = "entered";
  } catch (e: any) {
    doors.invented = "threw:" + (e && e.name ? e.name : String(e));
  }
  try {
    const scope = runtime.__deis_runtime_stream_scope;
    const owner = scope.adopt();
    scope.enter(owner);
    setInterval(() => { ownerTicks += 1; }, 5);
    scope.leave();
    doors.adopted = "entered";
  } catch (e: any) {
    doors.adopted = "threw:" + (e && e.name ? e.name : String(e));
  }

  // Held open long enough for the timers above to fire while their own request
  // is still running. Without this the check could not tell "swept" from "never
  // ran at all".
  await new Promise((resolve) => setTimeout(resolve, 100));

  return json({ doors, intervalTicks, timeoutTicks, ownerTicks });
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
# reason the streaming and object-storage handlers are: the moment
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
# Written here for the same reason the streaming handler is: the checks below
# need an application that actually calls a declared blob binding, and a
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
  # `context.env.BLOB` in an app that does not declare BLOB gets the
  # "this service is not available to this app" error rather than an object
  # store. Enabled here, in the staged copy, for the same reason the handler
  # is written here.
  local config="${dest}/.config.json"
  if [ -f "$config" ] && command -v jq >/dev/null 2>&1; then
    local with_blob
    with_blob=$(jq '.bindings.BLOB = { "kind": "blob" }' "$config") \
      && printf '%s\n' "$with_blob" > "$config"
  fi

  if [ -f "$handler" ]; then
    return 0
  fi

  mkdir -p "$(dirname "$handler")"
  cat > "$handler" <<'TS'
// Object storage through the declared `context.env.BLOB` binding: list, head, ranged read and
// conditional write, exposed as the HTTP an application would build on them.
const json = (body: any, status = 200) =>
  new Response(JSON.stringify(body), {
    status,
    headers: { "content-type": "application/json; charset=utf-8" },
  });

export default async function handler(context: any): Promise<Response> {
  const blob = context.env.BLOB;
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
# rather than kept in `demo-assets` for the same reason the streaming handler
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
//
// That was only ever half of what makes it a fact, and the half that was
// missing is the one an attacker uses. A service call overwrites the header;
// a request arriving over HTTP used to carry the client's value straight into
// the handler, so `curl -H 'x-deis-caller: admin-app'` was indistinguishable
// here from a call the platform had stamped. The node now drops its own
// `x-deis-` namespace at its entry and puts back only what it decided, so
// this header is absent on a request from outside and present only on a call
// the platform made. See check_a_client_cannot_forge_the_platform_s_headers.
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
  # Either side of the deploy, so that the checks reading what this deploy
  # made the node say can read those lines and no others. See
  # `node_log_window`.
  DEMO_APP_DEPLOY_FROM=$(node_log_mark)
  cli_deploy "$src_v1" "$ENV_NAME" cli-deploy-v1 || return 1
  DEMO_APP_DEPLOY_TO=$(node_log_mark)
  APP_V1_ID=$(report_field cli-deploy-v1 '.version.id')
  APP_V1_NUMBER=$(report_field cli-deploy-v1 '.version.number')
  echo "   ✓ deis deploy: version ${APP_V1_NUMBER} (${APP_V1_ID}) is serving ${ENV_NAME} on $(report_field cli-deploy-v1 '.delivery.delivered | length') node(s)"

  # The value is piped in rather than passed as an argument, which is what
  # the CLI recommends and what keeps it out of the shell history and out of
  # every other user's `ps` output. A secret is not part of the immutable
  # bundle, so this produces no new version; the control plane re-delivers
  # the running one with the new environment attached, and `deis secret set`
  # fails if a node does not take that.
  #
  # This is why the example app declares DEMO_SECRET with "required": false
  # (scripts/demo-assets/example-app/.config.json) rather than under the older
  # "secrets": [...] spelling. Under the locked standard that spelling is
  # shorthand for a required secret, and the control plane refuses to point an
  # environment at a version whose required secret has no value yet
  # (EC_ENV_REQUIRED_MISSING) - so a required secret has to be set before the
  # deploy that needs it, and a secret can only be set on an environment that
  # exists. The demo's story is the other order: deploy first, then add the
  # secret and watch the running version be re-delivered with it. A secret
  # that arrives after the deploy is, by the standard's own definition, not
  # one the deploy required.
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
  DEMO_PREVIEW_DEPLOY_FROM=$(node_log_mark)
  cli_deploy "$src_v2" "$PREVIEW_ENV_NAME" cli-deploy-v2-preview || return 1
  DEMO_PREVIEW_DEPLOY_TO=$(node_log_mark)
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

  # Is a previous run still up? The answer is in the state file, and the file
  # is read in a subshell rather than sourced here.
  #
  # Sourcing it here is what this used to do, and the file is a complete
  # settings dump: SECRET, API_KEY, STORAGE_AUTH_SECRET, APP_SRC_DIR, APP_ID,
  # ENV_NAME, NODE_ID, DEMO_SECRET_VALUE and every port. So every one of them
  # was silently replaced by the last run's value, and the option the person
  # typed on the command line lost to a file they had never opened:
  # `demo.sh start --secret <hex>` came up on the *previous* run's node key,
  # `--app-dir <path>` deployed the previous run's sources, and nothing said
  # so. Only the ten ports were rescued afterwards, by copying them aside
  # first, which fixed the ten symptoms that had been noticed and left the
  # rule -- a start is configured by its arguments, never by a leftover file
  # -- untrue for everything else.
  #
  # A subshell makes it structural rather than a list to keep in step: the
  # only thing that crosses back out is the pids that are still alive, as
  # text.
  local still_running=""
  if [ -f "$STATE_FILE" ]; then
    still_running=$(
      # shellcheck disable=SC1090
      source "$STATE_FILE"
      for pid in "${DEIS_CONTROL_PID:-}" "${DEIS_RUNTIME_PID:-}" \
        "${DEIS_PROXY_PID:-}" "${DEIS_STORE_PID:-}" \
        "${DEIS_QUEUE_PID:-}" "${DEIS_TELEMETRY_PID:-}"; do
        if pid_running "$pid"; then
          printf '%s ' "$pid"
        fi
      done
    )
  fi
  if [ -n "$still_running" ]; then
    echo "⚠️  An existing instance is running (pids: ${still_running}), please run stop first"
    exit 1
  fi

  # The state file is how `stop` finds the processes, so deleting the demo
  # directory without stopping first orphans a whole run: its components keep
  # their ports, and the new control plane would either fail to bind or, worse,
  # the probe would find the old one and every later step would address it.
  # Look for the processes themselves rather than trusting the file.
  #
  # The two processes that are not components are looked for by the same rule,
  # and they are the ones this actually caught. `report_service` calls a
  # service ready when the port answers and the child it started is still
  # alive, which is the right pair of questions and still loses a race: an
  # orphaned origin holding the claimed port answers `/payload.txt` with 200
  # in under a millisecond, while the child that lost the bind takes a few
  # more to die. The run then said "✓ Origin is serving", and every later
  # fetch went to a stranger's process running whatever revision of origin.py
  # was current when it was started - which is how a run reported the outbound
  # policy broken on an origin that simply did not have the route yet. The
  # certificate authority is in the same position and leaked the same way.
  # Both are refused here rather than raced there.
  local orphans
  orphans=$(pgrep -f \
    "${BIN_DIR}/deis-(control|runtime|proxy|store|queue|telemetry)|${DEMO_DIR}/origin/origin.py|${BIN_DIR}/examples/acme_test_ca" \
    2>/dev/null || true)
  if [ -n "$orphans" ]; then
    echo "⚠️  Processes from an earlier run are still running (pids: $(echo "$orphans" | tr '\n' ' '))."
    echo "    They hold their ports and their data. Stop them first:"
    echo "      ./scripts/demo.sh stop      # if .demo/state.env still exists"
    echo "      pkill -f '${BIN_DIR}/deis-'; pkill -f '${DEMO_DIR}/origin/origin.py'  # otherwise"
    exit 1
  fi

  claim_port DEIS_RUNTIME_PORT "${DEIS_RUNTIME_PORT:-$DEFAULT_DEIS_RUNTIME_PORT}"
  claim_port DEIS_PROXY_PORT "${DEIS_PROXY_PORT:-$DEFAULT_DEIS_PROXY_PORT}"
  claim_port DEIS_PROXY_TLS_PORT "${DEIS_PROXY_TLS_PORT:-$DEFAULT_DEIS_PROXY_TLS_PORT}"
  claim_port DEIS_CONTROL_MACHINE_PORT \
    "${DEIS_CONTROL_MACHINE_PORT:-$DEFAULT_DEIS_CONTROL_MACHINE_PORT}"
  claim_port DEIS_RUNTIME_MACHINE_PORT \
    "${DEIS_RUNTIME_MACHINE_PORT:-$DEFAULT_DEIS_RUNTIME_MACHINE_PORT}"
  claim_port ACME_CA_PORT "${ACME_CA_PORT:-$DEFAULT_ACME_CA_PORT}"
  claim_port DEIS_STORE_PORT "${DEIS_STORE_PORT:-$DEFAULT_DEIS_STORE_PORT}"
  claim_port DEIS_QUEUE_PORT "${DEIS_QUEUE_PORT:-$DEFAULT_DEIS_QUEUE_PORT}"
  claim_port DEIS_TELEMETRY_PORT "${DEIS_TELEMETRY_PORT:-$DEFAULT_DEIS_TELEMETRY_PORT}"
  claim_port DEIS_TELEMETRY_METRICS_PORT \
    "${DEIS_TELEMETRY_METRICS_PORT:-$DEFAULT_DEIS_TELEMETRY_METRICS_PORT}"
  claim_port DEIS_ORIGIN_PORT "${DEIS_ORIGIN_PORT:-$DEFAULT_DEIS_ORIGIN_PORT}"

  # A retired value, generated so that checks have a plausible string to
  # bring. It authenticated a node to the control plane once; nothing is
  # configured with it now, and two checks send it precisely to show that the
  # doors it used to open are not locked against it but absent.
  SECRET=${SECRET:-$(generate_secret)}

  # The shared secret deis-store's, deis-queue's and deis-control's
  # per-tenant capability tokens are all derived from - see
  # crates/deis-store/src/auth.rs. This one is live, and it authenticates a
  # *storage call* to whichever tenant it claims - a fleet-wide value still,
  # which is the next thing to go the way the node key went. Configured on all
  # three of deis-store,
  # deis-queue and deis-control below - see check_storage_capability_token for
  # what it looks like when only some of the three carry it.
  STORAGE_AUTH_SECRET=${STORAGE_AUTH_SECRET:-$(generate_secret)}

  # The credential a person - the `deis` CLI, a dashboard - presents to the
  # control plane's human API. A different channel again:
  # STORAGE_AUTH_SECRET authenticates a *storage call*, and this one
  # authenticates *a caller who may deploy*. It is generated
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

  # Before any configuration names them: the three machines of this demo fleet
  # each need a certificate, and two more have to exist for the checks that
  # prove the wrong one does not work.
  mint_fleet_identities

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
deis_runtime_poll_interval = 5
# Far below the product default of 600, and set here for a reason this script
# has met: a check that registers an extra node and is interrupted before it
# deregisters leaves a machine on the fleet that never answers again. A
# deployment that froze it as a target then cannot finish, and every following
# deployment to that environment is refused - which has poisoned not just the
# interrupted run but the two after it. Two minutes is long enough that nothing
# in this demo is given up on while it is merely busy, and short enough that a
# run which does leave a ghost behind recovers inside the next one instead of
# handing the failure forward.
deployment_target_abandon_after_secs = 120
bundle_dir = "${DEMO_DIR}/bundles"
public_base_url = "http://127.0.0.1:${DEIS_CONTROL_PORT}"
secret_key_file = "${SECRET_KEY_FILE}"
store_endpoint = "http://127.0.0.1:${DEIS_STORE_PORT}"
queue_endpoint = "http://127.0.0.1:${DEIS_QUEUE_PORT}"
storage_auth_secret = "${STORAGE_AUTH_SECRET}"

# The machine channel. Everything only Deislet's own processes call is served
# here and nowhere else: the assignment list, which carries every environment's
# decrypted secrets; the routing table, which is a map of every hostname on the
# platform; and the bundle download. A caller that cannot present a certificate
# this fleet signed does not get a connection at all, so none of it is reachable
# from where the dashboard is.
#
# public_base_url is what the bundle links in an assignment carry. It has to
# name this listener rather than the one above, because this is where the
# download is served.
[machine]
bind_addr = "127.0.0.1"
port = ${DEIS_CONTROL_MACHINE_PORT}
certificate = "${PKI_DIR}/control.crt"
private_key = "${PKI_DIR}/control.key"
trust_root = "${PKI_DIR}/ca.crt"
public_base_url = "https://127.0.0.1:${DEIS_CONTROL_MACHINE_PORT}"

[[deis_runtime_nodes]]
id = "${NODE_ID}"
addr = "http://127.0.0.1:${DEIS_RUNTIME_PORT}"
EOF
    # Every generated configuration is the operator's alone: this one holds
    # api_keys and storage_auth_secret, and every machine's names the private
    # key behind its own certificate (docs/trust-boundaries.md).
    chmod 600 "$DEIS_CONTROL_CONFIG"

    # [control] is architecture principle 8: on startup the node asks the
    # control plane which applications and versions it should be running,
    # downloads each bundle itself, and only reports healthy once they are
    # loaded. The control plane still pushes new deployments at it; the pull
    # is what makes a restarted node come back with what it had.
    #
    # There is no `secret` line, and that is the finished state rather than an
    # omission: the assignment list carries every environment's decrypted
    # secrets, and what entitles this node to it is the certificate in
    # [machine_identity] below, which names this one node. A shared value that
    # said "some machine in this installation" could never have said that.
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
retry_interval_secs = 2

# This node's own identity, and where the machine channel is. With it the node
# registers, asks for its assignments and downloads its bundles over mutual
# TLS, proving a certificate that names this node and no other. All three used
# to go to the listener a browser reaches, guarded by the value the whole fleet
# shared; none of them do now, and there is no fleet value left to guard them.
[machine_identity]
control_endpoint = "https://127.0.0.1:${DEIS_CONTROL_MACHINE_PORT}"
certificate = "${PKI_DIR}/node.crt"
private_key = "${PKI_DIR}/node.key"
trust_root = "${PKI_DIR}/ca.crt"
trust_domain = "${FLEET_TRUST_DOMAIN}"
# And the other direction: what the control plane pushes into this node
# arrives here, admitted only if the connection proved the control plane's own
# name. There is no second way in - the data port has no management dispatcher
# on it at all, so a management path there is read as a request for a page the
# application does not have, which verify asks about directly.
management_listen = "127.0.0.1:${DEIS_RUNTIME_MACHINE_PORT}"

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
    chmod 600 "$DEIS_RUNTIME_CONFIG"

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
associated_id = "${PROXY_ID}"

# The proxy's identity on the machine channel. With it the routing table is
# fetched as an RPC over mutual TLS instead of over the address above with the
# shared key, and the acknowledgement it sends - which table version this proxy
# is already running - is held by the control plane against the name this
# connection proved, so no proxy can acknowledge on another's behalf.
[machine_identity]
control_endpoint = "https://127.0.0.1:${DEIS_CONTROL_MACHINE_PORT}"
certificate = "${PKI_DIR}/proxy.crt"
private_key = "${PKI_DIR}/proxy.key"
trust_root = "${PKI_DIR}/ca.crt"
trust_domain = "${FLEET_TRUST_DOMAIN}"

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
    chmod 600 "$DEIS_PROXY_CONFIG"

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
  # Through `bounded_log` rather than straight into the file: this is the one
  # component's log an application - and so anyone who can reach the node -
  # writes into, and it had no ceiling at all. See `DEMO_LOG_MAX_BYTES`.
  #
  # A process substitution and not a pipe, because `$!` after `cmd | writer &`
  # is the writer's pid, and `stop` would then signal the writer while the
  # node kept its port. With `>` `>(...)` the job is the node and `$!` is the
  # node.
  DEIS_RUNTIME_CONFIG="$DEIS_RUNTIME_CONFIG" \
    DEIS_RUNTIME_HTTP_PORT="$DEIS_RUNTIME_PORT" \
    "${BIN_DIR}/deis-runtime" \
    > >(bounded_log "${LOG_DIR}/deis-runtime.log") 2>&1 &
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

  # Written here, before the deploy, and again at the end. Every process this
  # run owns exists by now, and the state file is the only thing `stop` reads
  # to find them: a deploy that is refused below ends this start with exit 1,
  # and a start that had written nothing yet left eight processes holding
  # their ports with no file naming their pids - the next `start` refused to
  # run and `stop` had nothing to stop. The second write, at the end, adds
  # what the deploy produced.
  write_state

  deploy_example_app

  # Only now can the front door answer for this application: the routing table
  # it needs did not exist until the deploy above created it. Everything the
  # verification drives through the proxy depends on this having happened.
  if ! wait_for_proxy_routing 60; then
    echo "⚠️  deis-proxy is listening but still cannot route to ${APP_ID}."
    echo "    Its log is ${LOG_DIR}/deis-proxy.log; the checks that go through"
    echo "    the proxy will fail until it has fetched the routing table."
  fi

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

# How many verdicts a complete run prints, and the number every run is held
# to.
#
# The summary is a tally of what happened to the checks that ran, and a tally
# has nothing to say about a check that did not run. Delete a call from the
# list in `verify_demo` -- by accident, in a merge, or because a check was
# being skipped "for now" -- and the run goes from "94 passed, 0 failed" to
# "93 passed, 0 failed", which reads as a slightly shorter green run and not
# as one that stopped asking a question. The whole of the difference is a
# digit nobody has the previous run in front of them to compare against.
#
# So the list has a length, it is written down, and a run that does not reach
# it fails. Adding a check means changing this number, which is the point:
# the number is the roll call.
VERIFY_EXPECTED_CHECKS=99

# Every claim a verdict has been printed for, one per line.
#
# The count alone cannot tell "a check vanished" from "a check vanished and
# another printed twice"; two verdicts under one claim keep the tally right
# while the list is wrong. Claims are compared rather than function names
# because the claim is what a reader is told was proved.
VERIFY_CLAIMS=""

pass_check() {
  CHECKS_PASSED=$((CHECKS_PASSED + 1))
  # `:-` so a harness that lifts this function out of the file -- which
  # scripts/test-demo-witnesses.sh does -- is not ended by `set -u` on a
  # variable it had no reason to know about.
  VERIFY_CLAIMS="${VERIFY_CLAIMS:-}
$1"
  echo "   ✓ $1"
  [ -n "${2:-}" ] && echo "     $2"
  return 0
}

fail_check() {
  CHECKS_FAILED=$((CHECKS_FAILED + 1))
  VERIFY_CLAIMS="${VERIFY_CLAIMS:-}
$1"
  echo "   ✗ $1"
  [ -n "${2:-}" ] && echo "     $2"
  return 0
}

# ----------------------------------------------------------------------
# A run that stops has to say so
# ----------------------------------------------------------------------
#
# `set -e` ends this script at the first command that fails outside a
# condition, and the checks are written on the assumption that a component
# may be down - so one unguarded command reaching a port with nothing on it
# used to take the whole run with it and print nothing at all: no line for
# the check it was in, no summary, and an exit status the reader has to know
# to look at. A run that stopped in the middle then looks exactly like a run
# that has not finished yet, which is the worse of the two, because the
# checks that never ran are indistinguishable from checks that passed.
#
# Two things keep that from happening. The convention, which applies to every
# check below: a curl whose answer is read is followed by `|| true`, because
# what a check does with that answer is judge it, and "nothing answered"
# is one of the judgements it is written to make. And these two traps, for
# every other cause including the ones nobody has met yet: whatever kills the
# run, it names the command and prints the tally, so the last line of a
# verify run is always either its summary or a sentence saying the summary is
# missing and why.
#
# Measured, with the proxy killed and everything else up: as this file
# shipped, the run stopped at the first check that addressed the proxy and
# printed 28 lines ending in a ✓ - which is what a run still in progress
# looks like. With both of the above, the same conditions print 199 lines,
# name seven checks as failed, and end "79 passed, 7 failed, and the run did
# not reach the end of its list".
VERIFY_REACHED_ITS_SUMMARY=false
VERIFY_DIED_AT=""

on_verify_error() {
  # $1 line, $2 the command as bash was about to run it, $3 its status.
  VERIFY_DIED_AT="demo.sh:$1: \`$2\` exited $3"
}

on_verify_exit() {
  local status=$?
  trap - ERR EXIT
  [ "$VERIFY_REACHED_ITS_SUMMARY" = "true" ] && return 0

  echo
  echo "   ✗ the run stopped before it had finished"
  echo "     ${VERIFY_DIED_AT:-no failing command was recorded}. Everything after this point was never asked, so the checks below the last one printed are neither passed nor failed - they are unknown."
  echo
  echo "${CHECKS_PASSED} passed, ${CHECKS_FAILED} failed, and the run did not reach the end of its list"
  # Never green. A run that stopped short proved nothing about the checks it
  # never asked, and a zero here would tell a pipeline it had.
  [ "$status" -eq 0 ] && status=1
  exit "$status"
}

# One request to the node, with the tenant header deis-proxy would have
# stamped. Writes the response headers to $1 and the body to stdout.
#
# `|| true` here, and on every other helper below that reaches a component.
# curl exits 7 when nothing is listening; a check reads the answer with
# `body=$(app_request ...)`; and an assignment that fails under
# `set -euo pipefail` is the end of the whole run - no line for the check it
# was in, no summary, and nothing but an exit status to say the run stopped
# rather than that it had not finished yet. Measured: with the node's port
# pointed at nothing, check_static_asset, check_route_parameter and
# check_app_fetch_allowed each ended the run and printed not one character
# between them. "The component did not answer" is an outcome every one of
# these checks is written to report, so it has to reach the check as the
# empty body it is rather than as a dead script.
app_request() {
  local header_file=$1
  local path=$2
  shift 2
  curl -s -D "$header_file" -H "x-deis-app-id: ${APP_ID}" "$@" \
    "http://127.0.0.1:${DEIS_RUNTIME_PORT}${path}" || true
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
    "http://127.0.0.1:${DEIS_RUNTIME_PORT}${path}" || true
}

# Read one header value out of a saved response, lower-casing the name.
#
# Nothing at all for a response that never arrived. A curl that could not
# connect leaves this file empty, or never creates it, and `tr < missing`
# exits 1 - which in `value=$(header_value ...)` ends the run for the same
# reason described at `app_request`. An absent header and an absent response
# both read as empty here; the checks tell them apart by the status, which
# is empty only in the second case.
header_value() {
  local file=$1
  local name=$2
  [ -s "$file" ] || return 0
  tr -d '\r' < "$file" | awk -v want="$(printf '%s' "$name" | tr '[:upper:]' '[:lower:]')" '
    { split($0, kv, ":"); k = tolower(kv[1]);
      if (k == want) { sub(/^[^:]*:[ ]*/, ""); print; exit } }'
}

# The status code of a saved response, or nothing when there was no response
# to save. `head -n 1` on a file curl never wrote exits 1, `pipefail` makes
# that the pipeline's status, and `status=$(http_status ...)` then ends the
# run - see `app_request`. Every caller already prints `${status:-none}` or
# compares it against the code it wanted, so empty is a value they handle.
http_status() {
  [ -s "$1" ] || return 0
  head -n 1 "$1" | tr -d '\r' | awk '{print $2}'
}

# The node's log, as text, with the terminal colouring taken out.
#
# Two things are removed, for two different reasons.
#
# The ANSI escapes, because tracing writes its field names and values wrapped
# in them when the log is a file as much as when it is a terminal: what reads
# as `keys=["ASSETS"]` on the page is `keys` ESC [0m ESC [2m = ... in the
# bytes, and a grep for what the line says has to read what a person reads.
#
# The NUL bytes, because this file is not written only by the node. An
# application's own log line lands here too, and an application's log line
# carries whatever a request put in it: one unauthenticated
# `GET /hello/%00` makes the example app write a NUL byte into this file.
# From that byte on, GNU grep calls the stream binary - it says "binary file
# matches" on stderr, exits 0 and writes nothing at all on stdout - so every
# check that reads this log sees an empty log. Thirteen checks read it and
# twelve of them assert on what they find, so twelve report that the node
# said nothing while the node's own sentence sits in the file - and the run
# itself dies at the first of them rather than saying so. One byte from
# outside, and half the evidence is gone without a word. Deleting it costs
# nothing:
# no check reads it, and it is the only byte that can decide binary-or-text
# here, because everything else in this file came out of a Rust String and is
# therefore valid UTF-8.
#
# The newline half of this is now closed on every route an application's
# text takes to this file: at the producer
# (`crates/deis-runtime/src/runtime/ops.rs::escape_control` escapes every
# control character out of a message before `op_log` hands it to `tracing`),
# under the node's whole `tracing` output (`deis_wire::logging::LineSafe`),
# and in the native console copy that writes to stdout past both
# (`escapeControl` in js/runtime.js). One request can no longer end the
# node's line and begin a whole line of its own. Measured before it was: `tracing-subscriber` 0.3.23
# escapes ESC and BEL and does not escape LF or CR, and `tracing` appends the
# real `app_id=` and `trace_id=` fields *after* an injected newline - so the
# forged line arrived already carrying the genuine invocation id, which is why
# attribution alone could never have caught it.
#
# What is still true, and is why the checks below narrow what they read: an
# application's line lands in this file, and one line of the node's exact
# wording needs no control character at all. So a check that matches on
# wording is still matching text anything reaching this node can have written
# here. See `node_log_before_verify`, `node_log_since` and `node_log_window`.
node_log() {
  LC_ALL=C tr -d '\000' < "${LOG_DIR}/deis-runtime.log" \
    | sed 's/\x1b\[[0-9;]*m//g'
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

# ----------------------------------------------------------------------
# Whose line is it
# ----------------------------------------------------------------------
#
# The node's log is not a private channel between the node and this script.
# Every application log line lands in the same file, and an application's line
# carries whatever a request put in it, so the file is writable - in content,
# not only in volume - by anyone who can reach the port. Measured against this
# demo as it shipped: one unauthenticated
#
#   GET /hello/x%0A<timestamp>%20%20WARN%20ThreadId(99)%20...stream_timeout_ms
#
# put a line in the file that carries a plausible timestamp, the WARN level,
# a thread id, the node's exact wording and the `app_id=` and `trace_id=`
# fields tracing appends - indistinguishable, read as text, from one the node
# wrote. And no forgery was needed for the cheaper half: an ordinary
# `GET /api/timers?mode=arm` from outside makes the node write a real
# "were still armed" line, which was equally enough.
#
# Both mattered because two checks asserted on a *count* of matching lines
# going up while they ran. That count is a fact about the file, not about the
# check: it moved when anything else on the node moved, and it could be moved
# on purpose by one request. The evidence a check prints as the node's own
# sentence could likewise be written from outside.
#
# So the question a check asks the log is no longer "did a line like this
# appear" but "did the node write this about the invocation I started". Every
# line one invocation produces - the handler's own, and the node's about it -
# carries the same `trace_id`. The handler logs a marker naming a tag this
# script invented a moment ago; this reads the id off that marker; and the
# checks then count and quote only lines carrying it. An outsider cannot
# produce a line under that id without knowing the tag, which never left this
# script and the node.
#
# What this does not close, said plainly: anyone who can *read* the log file
# can read the id out of it and then write a line carrying it.
#
# One thing it did not close when it was written, and now does. An
# application could put a newline and a whole fabricated line into this file,
# and `tracing` appended the real `app_id=` and `trace_id=` fields after the
# injected newline - so the forged line carried the genuine invocation id of
# the request that forged it, and matched a `trace_id="<id>"` filter exactly.
# The sentence above about an outsider needing to know the tag was therefore
# not true of an outsider who could reach a handler that logs what they sent.
# Three things make it true now, one per route an application's text takes
# to this file: `escape_control` in crates/deis-runtime/src/runtime/ops.rs at
# the app-log producer; `deis_wire::logging::LineSafe` under the node's whole
# `tracing` output; and `escapeControl` in js/runtime.js for the native
# console copy, which writes to stdout past both of the others. No message
# reaching this file can contain a newline any more.
#
# This makes the evidence unforgeable by someone who only has HTTP, which is
# what the demo hands to the world; it does not make the log a trustworthy
# channel.
#
# Answers nothing when the marker was never logged, which every caller
# reports as a failure rather than treating as zero matching lines.
trace_of_witness() {
  local marker=$1
  # `.*trace_id=` is greedy on purpose: a message that itself contains the
  # word matches the earlier one, and the field tracing appended is the last.
  node_log | grep -F -- "$marker" | sed -n 's/.*trace_id=//p' | tr -d '"' \
    | awk '{ print $1 }' | grep -x '[0-9a-f]\{8,\}' | tail -n 1 || true
}

# ----------------------------------------------------------------------
# Which part of the log a check may read
# ----------------------------------------------------------------------
#
# `trace_of_witness` asks the right question - "did the node write this about
# the invocation I started" - and can only be asked of a line that carries a
# `trace_id`. Most of the lines these checks read carry none, and cannot: a
# ceiling applied when a version was deployed, a consumer's line as it was
# spawned, a header dropped for an application with no compatibility date.
# There is no invocation to file those under, because there was none.
#
# What is still true about them is *when* they were written. A log file is
# appended to and nothing else, so a line that was already in it before this
# verification phase began cannot have been put there by anything the
# verification phase provoked - which is every request an outsider could make
# while these checks run, and every application line echoing one back.
# `node_log_before_verify` is that prefix, and it is the window deployment and
# startup evidence belongs in.
#
# `node_log_since` is the other half, for a line the node writes *while* a
# check runs: take a mark immediately before acting, read only what the file
# gained after it. It does not name an invocation the way a trace id does -
# anything else reaching the node inside that window is in the window too -
# but it excludes every line already in the file, and a forgery planted
# earlier in the run is exactly a line already in the file.
#
# Neither replaces the strongest form, which is a value this script invented a
# moment ago and the node then wrote back: the `probe=` nonce
# `check_app_fetch_refused` puts in the address it asks a handler to call. A
# line naming that address is a line about this call and no other, and nothing
# that cannot read this script's memory can compose one.
#
# Worth saying plainly, because the alternative is a check that reads stronger
# than it is: none of the three makes the log a trustworthy channel. An
# application's message still lands in this file, and while it can no longer
# carry a newline out of it - `escape_control` in
# crates/deis-runtime/src/runtime/ops.rs stops that at the producer - it can
# still contain the node's exact wording on one line. What these three do is
# make *when* and *about what* part of the assertion, so that repeating the
# node's wording is no longer enough on its own.

# Set once, at the top of the verification phase. Zero means "not taken", and
# `node_log_before_verify` then reads the whole file rather than nothing:
# reading too much makes a check weaker, reading nothing makes every check
# that uses it fail with a sentence about the node saying nothing, which is
# the wrong sentence and sends someone looking in the wrong place.
DEMO_VERIFY_LOG_MARK=0

# How many lines are in the node's log right now.
node_log_mark() {
  local log="${LOG_DIR}/deis-runtime.log"
  if [ ! -f "$log" ]; then
    echo 0
    return 0
  fi
  wc -l < "$log" | tr -d ' '
}

# The node's log as it stood when verification started: deployment and startup
# evidence, with everything this phase provoked excluded.
node_log_before_verify() {
  if [ "${DEMO_VERIFY_LOG_MARK:-0}" -le 0 ]; then
    node_log
    return 0
  fi
  node_log | head -n "${DEMO_VERIFY_LOG_MARK}"
}

# Only what the log gained after `$1`, which a caller takes with
# `node_log_mark` immediately before the request it is about to judge.
node_log_since() {
  local mark=${1:-0}
  node_log | tail -n "+$((mark + 1))"
}

# The lines one deployment wrote, between two marks the start phase took
# either side of it.
#
# This is the strong form of `node_log_before_verify`, and it exists because
# the weak form was not enough for the one check that has nothing else. "The
# file as it stood before verification began" is a window minutes wide with
# the node listening on an unauthenticated port for all of it; "the lines
# `deis deploy` produced" is a window a few hundred milliseconds wide, and to
# put a line in it something would have to already be running and be watching
# for the deploy.
#
# Falls back to the wide window when the marks are absent, which is what a
# state file written by an older copy of this script has. Failing instead
# would report a clamp that was never tested rather than one tested less
# tightly, and that is the wrong sentence: it sends someone to look at the
# node when what changed is the shape of a file on disk.
node_log_window() {
  local from=${1:-0} to=${2:-0}
  if [ "$to" -gt "$from" ]; then
    node_log | sed -n "$((from + 1)),${to}p"
    return 0
  fi
  node_log_before_verify
}

# How a check should describe the window it just read, so that the evidence
# says which of the two it got rather than implying the tight one.
node_log_window_named() {
  local from=${1:-0} to=${2:-0} what=$3
  if [ "$to" -gt "$from" ]; then
    printf 'lines %s-%s of the node log, which is %s and nothing else' "$from" "$to" "$what"
  else
    printf 'the node log as it stood before verification began - this run recorded no narrower window for %s' "$what"
  fi
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
    "http://127.0.0.1:${DEIS_RUNTIME_PORT}/" || true)
  local length
  length=$(curl -s -o /dev/null -w '%{size_download}' \
    -H "x-deis-app-id: ${APP_ID}" -H "If-None-Match: ${etag}" \
    "http://127.0.0.1:${DEIS_RUNTIME_PORT}/" || true)

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
  local title="a _headers block puts its headers on the asset it names and on nothing else"
  local styled="${LOG_DIR}/verify-site-headers-css.headers"
  local plain="${LOG_DIR}/verify-site-headers-root.headers"

  app_request "$styled" "$SITE_STYLESHEET" > /dev/null
  app_request "$plain" "/" > /dev/null

  local styled_cache styled_referrer plain_cache plain_referrer
  styled_cache=$(header_value "$styled" Cache-Control)
  styled_referrer=$(header_value "$styled" Referrer-Policy)
  plain_cache=$(header_value "$plain" Cache-Control)
  plain_referrer=$(header_value "$plain" Referrer-Policy)

  # Two files, not one. A node that applied the block to every response would
  # put the year-long Cache-Control and the Referrer-Policy on the HTML too,
  # and asking only about the stylesheet could not tell that apart from the
  # block matching the path it names.
  #
  # The file carries one block. It used to carry two - a `/*` block with the
  # Referrer-Policy above the `/css/*` block - and this check used to read
  # the Referrer-Policy on the stylesheet as proof that both matching blocks
  # were composed. The locked Edge Canon standard lowers `_headers` with
  # first-match semantics, and a file with more than one block is refused at
  # build (EC_ROUTING_SOURCE_UNREPRESENTABLE), so composition is no longer
  # something an application can be promised. What is promised is the block:
  # both of its headers on the path it names, neither anywhere else.
  if [ "$styled_cache" = "$SITE_STYLESHEET_CACHE_CONTROL" ] \
    && [ "$styled_referrer" = "$SITE_STYLESHEET_REFERRER_POLICY" ] \
    && [ "$plain_cache" != "$SITE_STYLESHEET_CACHE_CONTROL" ] \
    && [ -z "$plain_referrer" ]; then
    pass_check "$title" \
      "GET ${SITE_STYLESHEET} carries cache-control: ${styled_cache} and referrer-policy: ${styled_referrer}, both from the one /css/* block; GET / matches no block, so it carries no referrer-policy and keeps the node's own cache-control: ${plain_cache}. One block and not the /* block this file used to open with: the locked standard lowers _headers first-match, and a bundle with two blocks is refused at build (EC_ROUTING_SOURCE_UNREPRESENTABLE) rather than have one target compose them and another not"
  else
    fail_check "$title" \
      "expected ${SITE_STYLESHEET} to carry cache-control: ${SITE_STYLESHEET_CACHE_CONTROL} and referrer-policy: ${SITE_STYLESHEET_REFERRER_POLICY}, and / to carry no referrer-policy and keep the node's default cache-control; got '${styled_cache:-<none>}' and '${styled_referrer:-<none>}' on the stylesheet, '${plain_cache:-<none>}' and '${plain_referrer:-<none>}' on /"
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
  answered_json "$title" assets-binding "$body" || return 0

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

# The application the check below tries, and fails, to deploy. A name of its
# own, and a project of its own, so that the refusal cannot cost the example
# app its deployment and nothing else in this run reads what it left behind.
RESERVED_ENV_APP_ID="${RESERVED_ENV_APP_ID:-demo-reserved-env}"

# Lay down a project that writes `ASSETS` into its own environment, and echo
# the directory.
#
# Until the reserved-name rule reached the environment declarations, this was
# the example app itself: it shipped a variable called ASSETS, the node lifted
# it back out of the map, and the check below read the node's log to prove it
# had. That was the weaker claim. A variable of that name is now refused where
# it is written - the compiler will not mint a declaration for a name the
# platform publishes - so an author is told before anything is built, and the
# demo makes the earlier claim instead.
#
# Otherwise the smallest project the compiler accepts, so that nothing else in
# it can be what was refused.
prepare_reserved_env_source() {
  local dest="${DEMO_DIR}/app-reserved-env"

  rm -rf "$dest"
  mkdir -p "${dest}/functions/api"

  cat > "${dest}/.config.json" <<JSON
{
  "version": "1.0.0",
  "name": "${RESERVED_ENV_APP_ID}",
  "runtime": "standard-v1",
  "language": "typescript",
  "functionRoot": "./functions",
  "compatibilityDate": "2026-09-01",
  "environment": {
    "variables": {
      "ASSETS": "an-application-cannot-take-this-name"
    }
  },
  "vendors": { "deislet": { "enabled": true } },
  "build": { "outDir": "./dist" }
}
JSON

  cat > "${dest}/functions/api/asset.ts" <<'TS'
// A handler that reads the name it declared a value under. The value is the
// point: if the project could be built, this would answer with the string,
// and `env.ASSETS.fetch()` in any other handler of the same application would
// throw "fetch is not a function" instead of reaching the version's files.
export default async function handler(context: any): Promise<Response> {
  return new Response(String(context.env.ASSETS) + "\n", {
    headers: { "content-type": "text/plain; charset=utf-8" },
  });
}
TS

  echo "$dest"
}

check_the_assets_name_cannot_be_declared_by_an_application() {
  local title="an application declaring ASSETS in its own env is refused by the build, and a legal one gets the platform's binding"
  local json="${LOG_DIR}/cli-deploy-reserved-env.json"
  local errors="${LOG_DIR}/cli-deploy-reserved-env.log"

  local dir
  if ! dir=$(prepare_reserved_env_source); then
    fail_check "$title" "the project could not be staged under ${DEMO_DIR}"
    return
  fi

  # Through `deis deploy` and not `deis build`, for the reason the WebSocket
  # refusal spells out: the claim is about what a person who tried to ship
  # this would meet, and the deploy's first step is the build.
  local status=0
  (cd "$dir" && deis_cli deploy -e "$ENV_NAME" --app "$RESERVED_ENV_APP_ID" --json) \
    > "$json" 2> "$errors" || status=$?

  local message
  message=$(grep -a -m 1 -o 'EC_ENV_NAMESPACE_COLLISION.*' "$errors" 2>/dev/null || true)

  # What was left behind, on both sides: a bundle under the project, or an
  # application in the catalog. The list is walked whole, so an application on
  # a later page could not hide from this.
  local built="no"
  [ -e "${dir}/dist" ] && built="yes"
  local listed
  listed=$(control_list_all /api/v1/apps \
    | jq -r --arg id "$RESERVED_ENV_APP_ID" '[.[] | select(.app_id == $id)] | length' 2>/dev/null || true)

  # The other half of the same claim, from the application that *was* built:
  # the name the refusal protects holds the platform's object, and no value of
  # the application's is visible under it. Without this half the check would
  # only say the platform refuses a name, not that it uses it.
  local body kind callable leaked
  body=$(app_request "${LOG_DIR}/verify-assets-shadow.headers" "/api/asset")
  answered_json "$title" assets-shadow "$body" || return 0
  kind=$(echo "$body" | jq -r '.bindingKind // empty' 2>/dev/null || true)
  callable=$(echo "$body" | jq -r '.bindingIsCallable // false' 2>/dev/null || true)
  leaked=$(echo "$body" | jq -r '.declaredValueVisible // empty' 2>/dev/null || true)

  if [ "$status" -ne 0 ] && echo "$message" | grep -q "ASSETS" \
    && [ "$built" = "no" ] && [ "${listed:-x}" = "0" ] \
    && [ "$kind" = "object" ] && [ "$callable" = "true" ] && [ -z "$leaked" ]; then
    pass_check "$title" \
      "deis deploy --app ${RESERVED_ENV_APP_ID} exited ${status} at its first step, the build: \"$(echo "$message" | cut -c1-200)\". No dist/ was written under the project and the control plane lists no application called ${RESERVED_ENV_APP_ID}. Meanwhile ${APP_ID}, which declares no such name, sees env.ASSETS as an ${kind} whose fetch() is a function. The platform owns this name at both ends: an application cannot take it, and what a handler finds under it is the platform's own binding"
  else
    fail_check "$title" \
      "expected a non-zero exit naming EC_ENV_NAMESPACE_COLLISION and ASSETS, no dist/ and no application in the catalog, and env.ASSETS on ${APP_ID} an object with a callable fetch; got exit ${status}, message '${message:-<none>}', dist written: ${built}, applications called ${RESERVED_ENV_APP_ID}: ${listed:-<unreadable>}, env.ASSETS a ${kind:-<none>} (callable: ${callable}) with the application's own value $(if [ -n "$leaked" ]; then echo "visible as '${leaked}'"; else echo "not visible"; fi). The CLI said: $(tail -n 3 "$errors" | tr '\n' ' ' | head -c 300)"
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
# The proxy choosing an application by host *and* path prefix
# ----------------------------------------------------------------------
#
# The README puts "host and path prefix" in bold, and until this check nothing
# exercised the second half. Every request the demo sends through deis-proxy
# either carries `x-deis-app-id` - which names an application outright and
# never consults a route - or reaches an environment's automatic address under
# `routing.preview_zone`, which is a host and nothing else. A proxy that
# ignored `path_prefix` entirely, or matched it as a plain string prefix, left
# all of that green.
#
# The catalog is what this reads from, and it is reached with curl rather than
# with `deis`: routes are an operator's object and the CLI has no verb for one
# (the same reason `upload_altered_bundle` above talks to the control plane
# directly). What is under test is the proxy, not the command.

# A host two applications are bound to here, one of them under a path prefix
# of the other.
#
# One label on purpose. deis-proxy orders a certificate for every host in the
# routing table, and `is_certifiable_name` in crates/deis-proxy/src/acme/names.rs
# leaves out anything that is not a public host name - a single label among
# them, since there is no registered domain for an authority to check. So this
# name routes without adding an order at the stub authority, which is what the
# certificate checks below count.
ROUTED_HOST="${ROUTED_HOST:-deislet-edge}"
# The prefix the second application answers under, and a path that begins with
# the same letters while being outside it. `/verifyx` is the case a plain
# string prefix would swallow.
ROUTED_PREFIX="/verify"
ROUTED_NEAR_MISS="/verifyx"

# Grant one application the right to claim a hostname, unless the registry
# already says so.
#
# A hostname is global on this platform: a route claiming one is refused
# unless a domain grant covers it, whoever the caller is (the refusal itself
# is exercised by deis-control's own API tests, which drive the hijack
# reproduction end to end). So this demo grants exactly the one name its
# routing checks claim, to exactly the two applications that claim it, and
# nothing wider. Both grants are equal-specificity peers on one name; the
# (host, path prefix) uniqueness rule is what keeps their two routes apart.
ensure_domain_grant() {
  local pattern=$1 app=$2
  local existing
  # The whole registry, not its first page: a grant that sits past the
  # hundredth row would otherwise be granted again, and the 409 that answers
  # a duplicate would be read by the routing checks as a refusal.
  existing=$(control_list_all /api/v1/domains \
    | jq -r --arg pattern "$pattern" --arg app "$app" \
      '[.[] | select(.pattern == $pattern and .app_id == $app)] | length' \
    2>/dev/null || echo 0)
  if [ "${existing:-0}" -ge 1 ]; then
    return 0
  fi
  curl -s -o /dev/null -w '%{http_code}' -X POST \
    -H "Authorization: Bearer ${API_KEY}" -H 'content-type: application/json' \
    -d "{\"pattern\":\"${pattern}\",\"app_id\":\"${app}\"}" \
    "$(control_url)/api/v1/domains" || true
}

# Create one route unless the catalog already has it.
#
# `verify` can be run twice against one demo and the catalog accepts duplicate
# routes without complaint, so a second run would otherwise pile up rules that
# say the same thing.
ensure_catalog_route() {
  local app=$1 environment=$2 host=$3 prefix=$4
  local existing
  existing=$(curl -s -H "Authorization: Bearer ${API_KEY}" "$(control_url)/api/platform/routes" \
    | jq -r --arg app "$app" --arg host "$host" --arg prefix "$prefix" \
      '[.routes[] | select(.app_id == $app and .host == $host and .path_prefix == $prefix)] | length' \
    2>/dev/null || echo 0)
  if [ "${existing:-0}" -ge 1 ]; then
    return 0
  fi
  curl -s -o /dev/null -w '%{http_code}' -X POST \
    -H "Authorization: Bearer ${API_KEY}" -H 'content-type: application/json' \
    -d "{\"host\":\"${host}\",\"path_prefix\":\"${prefix}\",\"priority\":0}" \
    "$(control_url)/api/v1/apps/${app}/environments/${environment}/routes" || true
}

# One request through deis-proxy for a host the catalog claims, with no
# `x-deis-app-id` on it. The absence is the point: with the header there is
# nothing for a route to decide.
routed_request() {
  local host=$1
  local path=$2
  local header_file=$3
  curl -s -D "$header_file" -m 10 --noproxy '*' -H "Host: ${host}" \
    "http://127.0.0.1:${DEIS_PROXY_PORT}${path}" || true
}

check_the_proxy_routes_by_host_and_path_prefix() {
  local title="deis-proxy picks the application by host and path prefix, not by host alone"
  local created_root created_prefix

  local granted_root granted_prefix
  granted_root=$(ensure_domain_grant "$ROUTED_HOST" "$APP_ID")
  granted_prefix=$(ensure_domain_grant "$ROUTED_HOST" "$CALLEE_APP_ID")
  case "${granted_root}${granted_prefix}" in
    *4[0-9][0-9]*|*5[0-9][0-9]*)
      fail_check "$title" \
        "granting ${ROUTED_HOST} to the demo applications failed (${granted_root}/${granted_prefix}), so their routes cannot be claimed"
      return
      ;;
  esac
  created_root=$(ensure_catalog_route "$APP_ID" "$ENV_NAME" "$ROUTED_HOST" "/")
  created_prefix=$(ensure_catalog_route "$CALLEE_APP_ID" "$ENV_NAME" "$ROUTED_HOST" "$ROUTED_PREFIX")
  case "${created_root}${created_prefix}" in
    *4[0-9][0-9]*|*5[0-9][0-9]*)
      fail_check "$title" \
        "the catalog refused a route: / -> ${APP_ID} answered '${created_root}', ${ROUTED_PREFIX} -> ${CALLEE_APP_ID} answered '${created_prefix}'"
      return
      ;;
  esac

  # The proxy pulls the routing table on its own schedule, so the condition is
  # polled rather than waited out: what ends this loop is the prefix route
  # answering, which is the same fact the check is about.
  local headers="${LOG_DIR}/verify-routed.headers"
  local rounds=30 body=""
  while [ "$rounds" -gt 0 ]; do
    body=$(routed_request "$ROUTED_HOST" "$ROUTED_PREFIX" "$headers")
    case "$body" in
      *'"handlerOf"'*) break ;;
    esac
    rounds=$((rounds - 1))
    poll_pause
  done

  local prefix_status prefix_handler
  prefix_status=$(http_status "$headers")
  prefix_handler=$(echo "$body" | jq -r '.handlerOf // empty' 2>/dev/null || true)
  if [ "$prefix_handler" != "verify" ]; then
    fail_check "$title" \
      "http://${ROUTED_HOST}${ROUTED_PREFIX} through deis-proxy did not reach ${CALLEE_APP_ID}: HTTP ${prefix_status:-none}, body $(printf '%s' "$body" | tr '\n' ' ' | head -c 200)"
    return
  fi

  # The same host, a path the prefix does not cover: the other application.
  local root_body root_label
  root_body=$(routed_request "$ROUTED_HOST" "/hello/routed" "${LOG_DIR}/verify-routed-root.headers")
  root_label=$(echo "$root_body" | jq -r '.label // empty' 2>/dev/null || true)

  # A path that starts with the same letters and is not inside the prefix. It
  # belongs to the `/` route, so the answer is the other application's own
  # not-found page - which no other application on this demo serves.
  local near_body near_status
  near_body=$(routed_request "$ROUTED_HOST" "$ROUTED_NEAR_MISS" "${LOG_DIR}/verify-routed-near.headers")
  near_status=$(http_status "${LOG_DIR}/verify-routed-near.headers")

  # And a host nothing claims, so that "the host decided it" is observed and
  # not assumed: without this the two answers above would also be produced by
  # a proxy that routed on the path alone.
  local stranger_status
  stranger_status=$(curl -s -o /dev/null -w '%{http_code}' -m 10 --noproxy '*' \
    -H "Host: nothing-claims-this" "http://127.0.0.1:${DEIS_PROXY_PORT}${ROUTED_PREFIX}" || true)

  if [ "$root_label" != "v1" ]; then
    fail_check "$title" \
      "${ROUTED_PREFIX} reached ${CALLEE_APP_ID}, but /hello/routed on the same host did not reach ${APP_ID}: $(printf '%s' "$root_body" | tr '\n' ' ' | head -c 200)"
    return
  fi
  if [ "$near_status" != "404" ] || ! echo "$near_body" | grep -q "$SITE_NOT_FOUND_MARKER"; then
    fail_check "$title" \
      "${ROUTED_NEAR_MISS} answered HTTP ${near_status} without ${APP_ID}'s own not-found page, so the ${ROUTED_PREFIX} route is matching on letters rather than on path segments and has captured a path that is not under it: $(printf '%s' "$near_body" | tr '\n' ' ' | head -c 160)"
    return
  fi
  if [ "$stranger_status" != "404" ]; then
    fail_check "$title" \
      "${ROUTED_PREFIX} on a host no route claims answered HTTP ${stranger_status} rather than 404, so the host is not part of the decision and these routes are answering for every name that reaches this proxy"
    return
  fi

  pass_check "$title" \
    "two routes on the one host ${ROUTED_HOST}: / to ${APP_ID}/${ENV_NAME} and ${ROUTED_PREFIX} to ${CALLEE_APP_ID}/${ENV_NAME}. With no x-deis-app-id on any of them, GET ${ROUTED_PREFIX} answered HTTP ${prefix_status} from ${CALLEE_APP_ID}'s own handler (handlerOf=${prefix_handler}), GET /hello/routed answered from ${APP_ID} version ${root_label}, GET ${ROUTED_NEAR_MISS} answered ${APP_ID}'s 404 page because a prefix covers whole path segments and not the letters they start with, and the same ${ROUTED_PREFIX} on a host nothing claims answered HTTP ${stranger_status}"
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
  local gap=80
  stream_exchange "$DEIS_RUNTIME_PORT" "$APP_ID" \
    "${STREAM_ROUTE}?mode=endless&gapMs=${gap}&tag=${tag}" \
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

  # cancel() having run is one half. The other half - the producer stopped -
  # is measured, not inferred from it, because a runtime whose cancel() fires
  # while its pull loop goes on writing into a closed stream would pass the
  # first half with the sentence below intact.
  #
  # The measurement has to be longer than the producer's period. It writes a
  # frame every ${gap}ms, so two reads of its counter tens of milliseconds
  # apart are equal whether or not it is still running - measured at 8 of 8
  # trials against a producer known to be alive - and say nothing. A producer
  # has stopped when its counter does not move across an interval longer than
  # its period: read the counter, let a full second pass (poll_pause, which is
  # curl's own retry pacing and not a shell sleep - twelve periods), read it
  # again. One frame may still land after cancel(): the pull that was already
  # past its timer when the client left. So a counter that moved is given
  # another interval before it is called a running producer; a live one moves
  # in every interval and never settles.
  local rounds=0 settled="" produced_first="" produced_second=""
  while [ "$rounds" -lt 3 ]; do
    produced_first=$(app_request "${LOG_DIR}/verify-stream-hangup.headers" \
      "${STREAM_ROUTE}?mode=report" | jq -r '.framesProduced')
    poll_pause
    produced_second=$(app_request "${LOG_DIR}/verify-stream-hangup.headers" \
      "${STREAM_ROUTE}?mode=report" | jq -r '.framesProduced')
    rounds=$(( rounds + 1 ))
    if [ -n "$produced_first" ] && [ "$produced_first" = "$produced_second" ]; then
      settled=yes
      break
    fi
  done

  if [ "$settled" = "yes" ]; then
    pass_check "$title" \
      "the client read ${frames} frame(s) and closed the socket; the handler's cancel() ran with tag ${tag}, and the frame counter stood still at ${produced_first} across a full second - more than twelve of its ${gap}ms periods, in round ${rounds} of at most 3 - so the producer stopped rather than writing into nothing"
  else
    fail_check "$title" \
      "cancel() ran, but the producer kept going: the frame counter moved from ${produced_first} to ${produced_second} across a full second after the client had gone, and did so in each of ${rounds} round(s)"
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

# The lines of one invocation: those whose *last* `trace_id="..."` field is
# the given id.
#
# tracing appends its fields after the message, so the last such field on a
# line is the node's, and an earlier one is text the application put in its
# message. That is not a hypothetical: an application's line now arrives
# escaped onto one line with everything a request put in it - the node's
# wording, the level, and a `trace_id="..."` naming whatever id the request
# chose - and a reader that took any occurrence of the field would count that
# line for the invocation it names. The witness suite holds this against a
# line copied from the real log. Used by the two counters and two quoters
# below and by nothing else; `trace_of_witness` already reads the last field
# for the same reason.
lines_of_invocation() {
  local trace=$1
  grep -F "trace_id=\"${trace}\"" | grep -v -E "trace_id=\"${trace}\".*trace_id=\"" || true
}

# How many times this node has said, at warn, that it closed *one named
# invocation's* stream for running past its budget.
#
# By invocation, not by wording. Several checks here leave a stream behind
# that meets the same fate and is logged in the same words, so the count of
# lines that match the wording says nothing about any one of them - and,
# worse, it is a count over a file anything reaching the node can add to. See
# `trace_of_witness`. `$1` is the invocation id the stream belonged to; a
# count over that is a count of what the node said about this stream and
# nothing else.
stream_budget_log_lines() {
  local trace=$1
  if [ -z "$trace" ]; then
    echo 0
    return 0
  fi
  node_log | grep " WARN " | grep "is past stream_timeout_ms" \
    | lines_of_invocation "$trace" | grep -c . || true
}

check_a_stream_past_its_budget_is_closed_by_the_node() {
  local title="a stream that runs past stream_timeout_ms is closed by the node"
  local report="${LOG_DIR}/verify-stream-budget.json"
  # A tag this run invents. It reaches the handler, which logs it, and nothing
  # else knows it - which is what makes the node's line about this stream
  # findable and what stops any other line from standing in for it. `$$` alone
  # was not enough even before anybody was being adversarial: two `verify` runs
  # against one demo can be the same shell.
  local tag="budget-$$-${RANDOM}"

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
  # Which invocation this stream was, read off the marker the handler logged
  # under the tag above. Everything below is then asked about that invocation
  # rather than about the wording of a line, so neither a second `verify` run,
  # nor the parallel-request check's abandoned stream, nor a request from
  # outside can supply the evidence this check reports as its own.
  local trace
  trace=$(trace_of_witness "stream witness ${tag}")
  if [ -z "$trace" ]; then
    fail_check "$title" \
      "the node logged no 'stream witness ${tag}' line, so this check cannot tell which invocation its own stream was and will not read another one's evidence as its own; the log is ${LOG_DIR}/deis-runtime.log"
    return
  fi

  # Only the lines this invocation caused, and only lines carrying the level
  # the evidence claims. `stream_budget_log_lines` counts at warn, so a line
  # demoted to debug - which is where a message stops being something an
  # operator sees - fails here rather than passing on the wording alone.
  #
  # `|| true` on the quotation: a grep that matches nothing exits 1, pipefail
  # makes that the pipeline's status, and an unguarded assignment under
  # `set -e` ends the run right here - with no line for this check, no
  # summary and no clue, which is what a run that had merely not finished
  # yet looks like. The line being absent is one of the two outcomes this
  # check exists to tell apart, so it has to reach the `if` below rather
  # than kill the script on the way; the failure branch already prints
  # `${said:-<none>}` for it.
  local logged said
  logged=$(stream_budget_log_lines "$trace")
  said=$(node_log | grep " WARN " | grep "is past stream_timeout_ms" \
    | lines_of_invocation "$trace" | tail -n 1 || true)

  if [ "$timed_out" != "true" ] && [ "$clean" != "true" ] \
    && [ "$last" -ge "$floor" ] && [ "$last" -le "$ceiling" ] && [ "$logged" -ge 1 ]; then
    pass_check "$title" \
      "an endless stream ran for ${last}ms against the ${DEMO_STREAM_TIMEOUT_MS}ms ceiling this node is configured with, delivered ${frames} frame(s), and was then ended without its terminating chunk. The node wrote ${logged} line(s) at warn about invocation ${trace} - the one this check started, and no other - the last of them: \"$(printf '%s' "$said" | log_message | head -c 200)\""
  else
    fail_check "$title" \
      "expected the node to close the stream between ${floor}ms and ${ceiling}ms and to say so at warn; last frame at ${last}ms, clean_end=${clean}, client gave up=${timed_out}, warn line(s) about the ceiling filed under this check's own invocation ${trace}=${logged}"
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

# How many times this node has said, at warn, that it cleared timers an
# invocation left armed, filed under the invocation that armed them. The same
# reason as `stream_budget_log_lines`, and the same `$1`: counting the lines
# that match the wording counts a second `verify` run's line, another check's
# line, and any line a request from outside put in the file, none of which is
# evidence about the request this check made.
timer_cleared_log_lines() {
  local trace=$1
  if [ -z "$trace" ]; then
    echo 0
    return 0
  fi
  node_log | grep " WARN " | grep "were still armed when this invocation ended" \
    | lines_of_invocation "$trace" | grep -c . || true
}

check_no_timer_outlives_the_request_that_armed_it() {
  local title="no timer outlives the request that armed it, however the handler files it"
  local headers="${LOG_DIR}/verify-timers.headers"
  # A tag this run invents, for the reason given at `trace_of_witness`: the
  # node's warning about cleared timers is written for every request that
  # leaves one armed, so one `GET ${TIMER_ROUTE}?mode=arm` from outside used
  # to satisfy the assertion below on its own, and a line forged through an
  # application's own logging used to be quoted here as the node's.
  local tag="timers-$$-${RANDOM}"

  # One request arms everything it can; a second, in the same isolate, reads
  # whether any of it is still ticking. Two requests is the whole point: a timer
  # that fires inside the request that armed it is ordinary, and a timer that
  # fires inside the next one is somebody else's CPU and somebody else's rate
  # limit being spent.
  local armed
  armed=$(app_request "$headers" "${TIMER_ROUTE}?mode=arm&tag=${tag}")
  if [ "$(http_status "$headers")" != "200" ]; then
    fail_check "$title" "GET ${TIMER_ROUTE}?mode=arm answered $(http_status "$headers"): $(printf '%s' "$armed" | head -c 200)"
    return
  fi

  local armed_interval armed_timeout scope claim invented adopted
  armed_interval=$(printf '%s' "$armed" | jq -r '.intervalTicks // 0')
  armed_timeout=$(printf '%s' "$armed" | jq -r '.timeoutTicks // 0')
  scope=$(printf '%s' "$armed" | jq -r '.doors.scope // "?"')
  claim=$(printf '%s' "$armed" | jq -r '.doors.claim // "?"')
  invented=$(printf '%s' "$armed" | jq -r '.doors.invented // "?"')
  adopted=$(printf '%s' "$armed" | jq -r '.doors.adopted // "?"')

  local report before_i after_i before_t after_t before_o after_o
  report=$(app_request "$headers" "${TIMER_ROUTE}?mode=report")
  before_i=$(printf '%s' "$report" | jq -r '.before.intervalTicks // -1')
  after_i=$(printf '%s' "$report" | jq -r '.after.intervalTicks // -2')
  before_t=$(printf '%s' "$report" | jq -r '.before.timeoutTicks // -1')
  after_t=$(printf '%s' "$report" | jq -r '.after.timeoutTicks // -2')
  before_o=$(printf '%s' "$report" | jq -r '.before.ownerTicks // -1')
  after_o=$(printf '%s' "$report" | jq -r '.after.ownerTicks // -2')

  # The node has to say what it cleared, with the numbers: an operator whose
  # application is slower than it should be needs this line to be findable.
  # At warn, which is what the evidence below claims and what nothing used to
  # read - the old grep took the last line matching the words, which for a
  # message this long is tracing's wrapped continuation and carries no level
  # at all.
  #
  # `|| true` for the same reason it is on the quotation in
  # `check_a_stream_past_its_budget_is_closed_by_the_node`: without it a grep
  # that matches nothing ends the whole run under `set -euo pipefail`, and
  # the missing line is exactly what the failure branch below is written to
  # report.
  local trace said_lines said
  trace=$(trace_of_witness "timer witness ${tag}")
  if [ -z "$trace" ]; then
    fail_check "$title" \
      "the node logged no 'timer witness ${tag}' line, so this check cannot tell which invocation armed the timers and will not read another request's evidence as its own; the log is ${LOG_DIR}/deis-runtime.log"
    return
  fi
  said_lines=$(timer_cleared_log_lines "$trace")
  said=$(node_log | grep " WARN " | grep "were still armed when this invocation ended" \
    | lines_of_invocation "$trace" | tail -n 1 || true)

  if [ "$armed_interval" -gt 0 ] && [ "$armed_timeout" -gt 0 ] \
    && [ "$scope" = "undefined" ] && [ "$claim" = "null" ] \
    && [ "${invented#threw:}" != "$invented" ] && [ "${adopted#threw:}" != "$adopted" ] \
    && [ "$after_i" = "$before_i" ] && [ "$after_t" = "$before_t" ] \
    && [ "$after_o" = "$before_o" ] && [ "$said_lines" -ge 1 ]; then
    pass_check "$title" \
      "the arming request watched its own timers fire (${armed_interval} interval tick(s), ${armed_timeout} tick(s) of a setTimeout that rearms itself) and could reach neither door to the runtime's ownership of them: __deis_runtime_stream_scope is ${scope} and the claim answers ${claim}, so enter() ${invented} and adopt() ${adopted}. A later request through the same isolate then sat for 250ms and none of the three counters moved (${before_i}/${before_t}/${before_o}). The node said so at warn, about invocation ${trace} and no other: \"$(printf '%s' "$said" | log_message | head -c 160)\""
  else
    fail_check "$title" \
      "arming request: intervalTicks=${armed_interval} timeoutTicks=${armed_timeout} scope=${scope} claim=${claim} invented=${invented} adopted=${adopted}; later request: intervals ${before_i}->${after_i}, timeouts ${before_t}->${after_t}, redirected ${before_o}->${after_o}; warn lines naming cleared timers filed under this check's own invocation ${trace}=${said_lines}, the last reading \"$(printf '%s' "${said:-<none>}" | log_message | head -c 160)\""
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
  local title="context.env.KV round-trips through deis-store"
  local key="demo-kv-$$"
  local value="stored-through-deis-store"
  local headers="${LOG_DIR}/verify-kv.headers"

  local put
  put=$(curl -s -D "$headers" -X PUT -H "x-deis-app-id: ${APP_ID}" \
    --data-binary "$value" \
    "http://127.0.0.1:${DEIS_RUNTIME_PORT}/api/kv/${key}" || true)
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
    -H "x-deis-binding: KV" \
    -d "{\"key\":\"${key}\"}" \
    "127.0.0.1:${DEIS_STORE_PORT}" deis.storage.KvStore/Get 2>&1)
  direct_value=$(echo "$direct" | jq -r '.value // empty' 2>/dev/null | openssl base64 -d -A 2>/dev/null || true)

  if [ "$direct_value" = "$value" ]; then
    pass_check "$title" \
      "PUT then GET returned ${value} from the node, and deis-store's own KvStore.Get answers the same value directly for tenant ${tenant}, binding KV, key ${key}"
  else
    fail_check "$title" \
      "the node answered ${value}, but deis-store's own KvStore.Get for tenant ${tenant}, binding KV, key ${key} returned: $(echo "$direct" | tr '\n' ' ' | head -c 200)"
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
  local title="three named kv bindings of one application never see each other"
  local key="binding-probe-$$"
  local headers="${LOG_DIR}/verify-binding.headers"

  # The same key, written through three named namespaces of the one tenant.
  app_request "$headers" "/api/bindings/HOT?key=${key}" -X PUT --data-binary "hot-value" >/dev/null
  if [ "$(http_status "$headers")" != "201" ]; then
    fail_check "$title" "PUT through HOT answered $(http_status "$headers")"
    return
  fi
  app_request "$headers" "/api/bindings/COLD?key=${key}" -X PUT --data-binary "cold-value" >/dev/null
  app_request "$headers" "/api/bindings/default?key=${key}" -X PUT --data-binary "kv-value" >/dev/null

  local hot cold kv hot_ns same_object
  hot=$(app_request "${LOG_DIR}/verify-binding-hot.headers" "/api/bindings/HOT?key=${key}")
  cold=$(app_request "${LOG_DIR}/verify-binding-cold.headers" "/api/bindings/COLD?key=${key}")
  kv=$(app_request "${LOG_DIR}/verify-binding-plain.headers" "/api/bindings/default?key=${key}")
  hot_ns=$(echo "$hot" | jq -r '.namespace // empty')
  same_object=$(echo "$hot" | jq -r '.sameObjectOnEnv // empty')

  if [ "$(echo "$hot" | jq -r '.value // empty')" != "hot-value" ] \
    || [ "$(echo "$cold" | jq -r '.value // empty')" != "cold-value" ] \
    || [ "$(echo "$kv" | jq -r '.value // empty')" != "kv-value" ]; then
    fail_check "$title" \
      "the same key read back differently per namespace was expected; HOT=$(echo "$hot" | jq -c '.value'), COLD=$(echo "$cold" | jq -c '.value'), KV=$(echo "$kv" | jq -c '.value')"
    return
  fi
  if [ "$same_object" != "true" ] || [ "$hot_ns" != "HOT" ]; then
    fail_check "$title" \
      "context.env.HOT must be the declared object and know its own name; sameObjectOnEnv=${same_object}, namespace=${hot_ns}"
    return
  fi

  # Reading it back through the node proves the isolate got three answers, not
  # where they are kept. deis-store's own KvStore.Get, with the binding header
  # this platform stamps, is what proves the three are three namespaces on
  # disk. Each call carries the logical binding name the application declared.
  if ! command -v grpcurl >/dev/null 2>&1 || ! command -v openssl >/dev/null 2>&1; then
    fail_check "$title" \
      "the three namespaces answered hot-value / cold-value / kv-value through the node, but grpcurl or openssl is missing so deis-store could not be asked directly"
    return
  fi

  local tenant token direct_hot direct_kv
  tenant="${APP_ID}.${ENV_NAME}"
  token=$(storage_token "$tenant")
  direct_hot=$(grpcurl -plaintext -max-time 5 \
    -import-path "${ROOT_DIR}/crates/deis-store/proto" -proto storage.proto \
    -H "x-deis-tenant: ${tenant}" -H "x-deis-storage-token: ${token}" \
    -H "x-deis-binding: HOT" \
    -d "{\"key\":\"${key}\"}" \
    "127.0.0.1:${DEIS_STORE_PORT}" deis.storage.KvStore/Get 2>&1 \
    | jq -r '.value // empty' 2>/dev/null | openssl base64 -d -A 2>/dev/null || true)
  direct_kv=$(grpcurl -plaintext -max-time 5 \
    -import-path "${ROOT_DIR}/crates/deis-store/proto" -proto storage.proto \
    -H "x-deis-tenant: ${tenant}" -H "x-deis-storage-token: ${token}" \
    -H "x-deis-binding: KV" \
    -d "{\"key\":\"${key}\"}" \
    "127.0.0.1:${DEIS_STORE_PORT}" deis.storage.KvStore/Get 2>&1 \
    | jq -r '.value // empty' 2>/dev/null | openssl base64 -d -A 2>/dev/null || true)

  if [ "$direct_hot" = "hot-value" ] && [ "$direct_kv" = "kv-value" ]; then
    pass_check "$title" \
      "key ${key} written through context.env.HOT, COLD and KV reads back separately; deis-store's own KvStore.Get for tenant ${tenant} answers the matching values with x-deis-binding: HOT and KV"
  else
    fail_check "$title" \
      "deis-store answered ${direct_hot:-<nothing>} for tenant ${tenant} with binding HOT and ${direct_kv:-<nothing>} with binding KV; expected hot-value and kv-value"
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
      "deis kv put --binding HOT wrote ${key}; reading it back with --binding HOT gives written-through-hot and reports the namespace as ${hot_tenant}, the same command without --binding reports found=false, and the application reading context.env.HOT sees the CLI's value"
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

# The other half of what makes `x-deis-caller` evidence: that nothing outside
# the platform can write one.
#
# Addressed to the node directly, on purpose. deis-proxy sweeps the same
# namespace at the edge, and a check that only went through the proxy would
# pass on a node that had no rule of its own -- while every other node on the
# network, and this very command, can reach the node without going through the
# proxy at all.
check_a_client_cannot_forge_the_platform_s_headers() {
  local title="a client cannot hand a handler the platform's own headers"
  local headers="${LOG_DIR}/verify-forged-platform-headers.headers"
  local body
  body=$(curl -s -D "$headers" \
    -H "x-deis-app-id: ${CALLEE_APP_ID}" \
    -H "x-deis-caller: admin-app.production" \
    -H "x-deis-tenant: victim.production" \
    -H "x-deis-storage-token: forged" \
    -H "x-demo-note: sent by a client" \
    "http://127.0.0.1:${DEIS_RUNTIME_PORT}/verify" || true)

  local status called_by note
  status=$(http_status "$headers")
  called_by=$(echo "$body" | jq -r '.calledBy // "absent"' 2>/dev/null || true)
  note=$(echo "$body" | jq -r '.note // "absent"' 2>/dev/null || true)

  # The callee is the same handler the service-call check drives, so this is
  # the same code reading the same header: once because the platform wrote it,
  # once because a client tried to.
  # 202 is the callee's own answer, the same one the service-call check above
  # reads; what is under test is what the handler was told about its caller.
  if [ "$status" = "202" ] && [ "$called_by" = "absent" ] \
    && [ "$note" = "sent by a client" ]; then
    pass_check "$title" \
      "curl sent x-deis-caller: admin-app.production, x-deis-tenant and x-deis-storage-token straight to the node; the handler saw no caller at all, while its own x-demo-note came through untouched"
  else
    fail_check "$title" \
      "the node answered ${status} and the handler read calledBy=${called_by:-<nothing>}, note=${note:-<nothing>}; a forged caller reaching a handler is the whole of this check"
  fi
}

check_a_service_binding_is_not_a_hole_in_the_outbound_policy() {
  local title="a service call reaches an application no address would have reached"

  # The callee listens nowhere of its own: it is an isolate on this node, and
  # the only HTTP address that leads to it is this node's own, which is a
  # loopback address. This is what the outbound policy says about that -- so
  # the successful call above is not the fetch below with a shorter name, it
  # is a different mechanism with no address in it at all.
  # A value this run invented, carried in the address so that the node writes
  # it back in its refusal line. The egress policy judges scheme, host and
  # port; a query string cannot change what it decides.
  local probe="service-egress-$$-${RANDOM}"
  local target="http://127.0.0.1:${DEIS_RUNTIME_PORT}/api/call?probe=${probe}"
  local mark
  mark=$(node_log_mark)
  local body
  body=$(app_fetch service-egress "$target")
  answered_json "$title" service-egress "$body" || return 0

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
  # The nonce is what ties this line to this call. Without it the match was
  # "some line, anywhere in the file, saying an outbound fetch to something
  # containing 127.0.0.1 was refused" - which check_app_fetch_refused's own
  # line satisfies, and which one echoed request can write outright.
  local logged
  logged=$(node_log_since "$mark" 2>/dev/null \
    | grep -m 1 -F "Refused an outbound fetch" | grep -F "${probe}" || true)

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
# GET /api/v1/apps/:app_id/environments/:env/storage-token. Without that
# route the three data commands have nothing to authenticate with and every
# one of them fails at the first call.

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
  if ! deis_cli kv get "$key" --binding KV --json > "$out" 2> "${LOG_DIR}/cli-kv-get.log"; then
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
    && [ "$tenant" = "${APP_ID}.${ENV_NAME}@KV" ]; then
    pass_check "$title" \
      "the handler stored ${key} through context.env.KV; \`deis kv get --binding KV\` asked the control plane for a storage token, presented it to deis-store as namespace ${tenant}, and read back \"${read_back}\" ($(printf '%s' "$read_back" | wc -c) bytes). The CLI never sees storage_auth_secret: it is handed a token for this one tenant"
  else
    fail_check "$title" \
      "deis kv get ${key} --binding KV reports namespace '${tenant:-<none>}', found=${found}, value '${read_back:-<none>}'; expected ${APP_ID}.${ENV_NAME}@KV and \"${value}\""
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
  local route="/api/v1/apps/${APP_ID}/environments/${ENV_NAME}/storage-token"
  local minted="${LOG_DIR}/verify-storage-token.json"

  local anonymous authorised
  anonymous=$(curl -s -o /dev/null -w '%{http_code}' "$(control_url)${route}" || true)
  authorised=$(curl -s -o "$minted" -w '%{http_code}' \
    -H "Authorization: Bearer ${API_KEY}" "$(control_url)${route}" || true)

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

# The SQL sandbox decides what a tenant's statement may touch. On its own that
# leaves how much it may cost decided by nobody, and one tenant is one query
# away from denying deis-store to every tenant on the node: a `WITH RECURSIVE`
# counting to a hundred million ran 95.7 seconds on one core, and killing the
# client changed nothing, because the statement runs inside a blocking task
# that dropping the caller's future does not cancel.
#
# This is that query, sent the way an application or an operator sends one. It
# has to come back refused, quickly, saying which ceiling it crossed - a query
# killed with "database error" teaches nobody anything.
check_a_runaway_query_is_stopped_and_names_its_ceiling() {
  local title="a query with no end is stopped, and says which ceiling stopped it"
  local out="${LOG_DIR}/cli-db-runaway.log"
  local sql="WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x+1 FROM c WHERE x < 2000000000) SELECT count(*) FROM c"

  local started elapsed rc
  started=$(date +%s)
  deis_cli db query "$sql" --json > "$out" 2>&1 && rc=0 || rc=$?
  elapsed=$(( $(date +%s) - started ))

  local message
  message=$(tr '\n' ' ' < "$out" | sed 's/  */ /g')

  # Under twenty seconds: the store's own ceiling is five, and anything near
  # the ninety-six this used to take means nothing stopped it.
  if [ "$rc" -ne 0 ] && [ "$elapsed" -lt 20 ] \
    && echo "$message" | grep -q 'ms the store gives one request'; then
    pass_check "$title" \
      "\`deis db query\` sent a recursive count to two billion and deis-store stopped it after ${elapsed}s rather than running it to the end, answering: $(echo "$message" | head -c 240). The refusal rides inside the response, like a SQL typo, and names both the ceiling and what to change"
  else
    fail_check "$title" \
      "expected a refusal naming the statement ceiling within 20s; the call exited ${rc} after ${elapsed}s saying: $(echo "$message" | head -c 300)"
  fi
}

# The other half of the same gap, and the one that takes the process down
# rather than merely busy: `SELECT randomblob(20000000)` took deis-store from
# 18 MB resident to 1476 MB, because a blob comes back as a JSON array of byte
# values and the whole result set was built before anything looked at its size.
# SQLite is the only thing that can refuse this one - the blob is a single
# instruction, so nothing watching between instructions ever gets a turn.
check_a_single_value_cannot_be_bigger_than_a_row_may_be() {
  local title="one cell cannot be made larger than a row is allowed to be"
  local out="${LOG_DIR}/cli-db-huge-value.log"

  local rc
  deis_cli db query "SELECT randomblob(20000000) AS b" --json > "$out" 2>&1 && rc=0 || rc=$?

  local message
  message=$(tr '\n' ' ' < "$out" | sed 's/  */ /g')

  if [ "$rc" -ne 0 ] && echo "$message" | grep -q '8388608' \
    && echo "$message" | grep -q 'blob service'; then
    pass_check "$title" \
      "deis-store refused a twenty-megabyte cell before allocating it, naming its 8388608-byte ceiling on any single string, blob or row and pointing at the blob service for payloads that size: $(echo "$message" | head -c 240)"
  else
    fail_check "$title" \
      "expected a refusal naming the 8388608-byte value ceiling; the call exited ${rc} saying: $(echo "$message" | head -c 300)"
  fi
}

# ----------------------------------------------------------------------
# What a tenant's SQL may touch
# ----------------------------------------------------------------------
#
# The two checks above are about what a statement may *cost*. This one is
# about what it may *reach*, which is the question the cost ceilings leave
# entirely open - and the only one of the three storage services where nothing
# asked it. KV has check_a_binding_cannot_reach_another_tenant and blob has
# the tenant its capability token names; SQL had the runaway query and the
# oversized cell, and neither of them would notice if every tenant's statement
# ran against every tenant's file.
#
# A file per tenant is not by itself isolation, because plain SQLite can name
# a second file: `PRAGMA database_list` hands a caller the absolute path of
# its own database, which is every other tenant's path with one component
# changed; `ATTACH DATABASE` then opens that path to read and write; `VACUUM
# INTO` writes a copy of the caller's data anywhere the process can write. All
# three are ordinary SQL and all three arrive through the same
# the declared database binding an application already has.
#
# So the boundary is held under the SQL, by an authorizer callback and by
# `SQLITE_LIMIT_ATTACHED` pinned to zero
# (crates/deis-store/src/database.rs). This is the witness that it is held,
# driven the way a person drives it: `deis db`, once as each of two tenants of
# the same application, with the second one naming the first one's file by its
# real path on this machine.

# Run one statement as one tenant, and echo the exit status followed by
# everything the command said, on one line.
#
# Both halves are asserted on by the check below and they fail differently: a
# refusal that came back as an exit status with no message teaches an
# application author nothing, and a refusal reported in prose that exited zero
# is one no script can branch on.
tenant_sql() {
  local environment=$1
  local verb=$2
  local sql=$3
  local out="${LOG_DIR}/verify-sql-isolation.log"
  local rc=0
  case "$verb" in
    execute)
      deis_cli db execute -y -e "$environment" "$sql" --json > "$out" 2>&1 || rc=$?
      ;;
    *)
      deis_cli db query -e "$environment" "$sql" --json > "$out" 2>&1 || rc=$?
      ;;
  esac
  printf '%s %s\n' "$rc" "$(tr '\n' ' ' < "$out" | sed 's/  */ /g')"
}

check_a_tenants_sql_cannot_reach_another_tenants_tables_or_files() {
  local title="one tenant's SQL cannot read another tenant's tables, or name its file"
  local table="tenant_isolation_probe"
  local marker="only-${ENV_NAME}-may-read-this-$$-${RANDOM}"
  # Where deis-store keeps this tenant's file: <data_dir>/db/<tenant>.sqlite,
  # the layout crates/deis-store/src/database.rs documents. Spelled out here
  # because the whole point is that the other tenant names a path that really
  # exists and really holds the rows below - a refusal to open a file that was
  # not there would prove nothing.
  local victim_file="${DEMO_DIR}/deis-store-data/db/${APP_ID}.${ENV_NAME}.sqlite"
  local answer rc message

  answer=$(tenant_sql "$ENV_NAME" execute \
    "CREATE TABLE IF NOT EXISTS ${table} (marker TEXT)")
  if [ "${answer%% *}" != "0" ]; then
    fail_check "$title" "${ENV_NAME} could not create ${table}: ${answer#* }"
    return
  fi
  answer=$(tenant_sql "$ENV_NAME" execute \
    "INSERT INTO ${table} (marker) VALUES ('${marker}')")
  if [ "${answer%% *}" != "0" ]; then
    fail_check "$title" "${ENV_NAME} could not write its marker row: ${answer#* }"
    return
  fi

  answer=$(tenant_sql "$ENV_NAME" query \
    "SELECT marker FROM ${table} WHERE marker = '${marker}'")
  case "${answer#* }" in
    *"$marker"*) ;;
    *)
      fail_check "$title" \
        "${ENV_NAME} cannot read back its own row, so there is nothing for the other tenant to be kept away from: ${answer#* }"
      return
      ;;
  esac

  if [ ! -f "$victim_file" ]; then
    fail_check "$title" \
      "${ENV_NAME} wrote a row but ${victim_file} does not exist, so the path the other tenant names below is not the file holding it and every refusal after this would be about nothing"
    return
  fi

  # Now the other tenant. The same application, its other environment, which
  # this platform charges as a separate tenant - so this is the case an
  # application author actually meets, and not a contrived second app.
  local read_rc read_message attach_rc attach_message expr_rc expr_message
  local pragma_rc pragma_message vacuum_rc vacuum_message
  local copy="${DEMO_DIR}/sql-isolation-copy-$$.sqlite"

  answer=$(tenant_sql "$PREVIEW_ENV_NAME" query "SELECT marker FROM ${table}")
  read_rc=${answer%% *}; read_message=${answer#* }
  answer=$(tenant_sql "$PREVIEW_ENV_NAME" query \
    "ATTACH DATABASE '${victim_file}' AS victim")
  attach_rc=${answer%% *}; attach_message=${answer#* }
  # The same ATTACH with the filename built by an expression, which is the
  # spelling SQLite cannot hand to the authorizer: it arrives with the name
  # missing and has to be refused on the action alone.
  answer=$(tenant_sql "$PREVIEW_ENV_NAME" query \
    "ATTACH DATABASE ('${victim_file}' || '') AS victim")
  expr_rc=${answer%% *}; expr_message=${answer#* }
  # How a tenant would learn the path in the first place.
  answer=$(tenant_sql "$PREVIEW_ENV_NAME" query "PRAGMA database_list")
  pragma_rc=${answer%% *}; pragma_message=${answer#* }
  # And writing a file rather than reading one.
  answer=$(tenant_sql "$PREVIEW_ENV_NAME" execute "VACUUM INTO '${copy}'")
  vacuum_rc=${answer%% *}; vacuum_message=${answer#* }

  local copied=no
  [ -e "$copy" ] && copied=yes
  rm -f "$copy"

  if [ "$read_rc" = "0" ] || [ "$attach_rc" = "0" ] || [ "$expr_rc" = "0" ] \
    || [ "$pragma_rc" = "0" ] || [ "$vacuum_rc" = "0" ] || [ "$copied" = "yes" ]; then
    fail_check "$title" \
      "${APP_ID}.${PREVIEW_ENV_NAME} was allowed something it must not be: SELECT exited ${read_rc}, ATTACH ${attach_rc}, ATTACH from an expression ${expr_rc}, PRAGMA database_list ${pragma_rc}, VACUUM INTO ${vacuum_rc} (wrote a file: ${copied}). The SELECT said: $(printf '%s' "$read_message" | head -c 200)"
    return
  fi

  # The refusals also have to say which rule refused them. "database error"
  # on all five would be this same set of exit codes and would leave an
  # author with nothing to act on.
  case "$read_message" in
    *"no such table"*) ;;
    *)
      fail_check "$title" \
        "${PREVIEW_ENV_NAME} was refused the read, but not as a table it does not have: ${read_message}. It matters which: a tenant whose statement failed for some other reason may still be looking at the wrong file"
      return
      ;;
  esac
  local refusal
  for refusal in "$attach_message" "$expr_message"; do
    case "$refusal" in
      *ATTACH*) ;;
      *)
        fail_check "$title" \
          "an ATTACH from ${PREVIEW_ENV_NAME} was refused without naming ATTACH: ${refusal}"
        return
        ;;
    esac
  done
  case "$pragma_message" in
    *"PRAGMA database_list"*) ;;
    *)
      fail_check "$title" "PRAGMA database_list was refused without naming itself: ${pragma_message}"
      return
      ;;
  esac
  case "$vacuum_message" in
    *VACUUM*) ;;
    *)
      fail_check "$title" "VACUUM INTO was refused without naming VACUUM: ${vacuum_message}"
      return
      ;;
  esac

  pass_check "$title" \
    "${APP_ID}.${ENV_NAME} wrote a row only it should see into ${table}, and it is in ${victim_file} on this machine. ${APP_ID}.${PREVIEW_ENV_NAME} - the same application, the other environment, a separate tenant - selected from that table and was told \"$(printf '%s' "$read_message" | sed 's/^error: //' | head -c 90)\"; naming the file outright was refused (\"$(printf '%s' "$attach_message" | sed 's/^error: //' | head -c 120)\"), and so was the same ATTACH with the path built by an expression, PRAGMA database_list that would have told it the path, and VACUUM INTO, which wrote no file. Five statements, five refusals, each naming what it refused"
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
  local title="context.env.BLOB lists a page and rolls a folder up at a delimiter"
  local headers="${LOG_DIR}/verify-blob-list.headers"

  local failed
  if ! failed=$(seed_blob_objects); then
    fail_check "$title" "the application could not store ${failed}"
    return
  fi

  # Everything this check asserts on lives under one prefix it owns.
  #
  # It used to list the tenant's root and expect to find exactly
  # `index.html` there, which held only on a store nothing else had written
  # to - so a second `verify` against the same running demo failed it, the
  # conditional-write checks having left their own keys at the root by then.
  # A check that passes only on a fresh instance reads as a real defect to
  # whoever runs it twice, so the listing is scoped instead.
  local scope="listing/"
  local seeded
  for seeded in "index.html" "img/logo.png" "img/hero.png" "img/icons/star.svg"; do
    curl -s -o /dev/null -D "$headers" -X PUT -H "x-deis-app-id: ${APP_ID}" \
      --data-binary "0123456789abcdef" \
      "http://127.0.0.1:${DEIS_RUNTIME_PORT}${BLOB_ROUTE}?key=${scope}${seeded}&contentType=text/plain"
    if [ "$(http_status "$headers")" != "201" ]; then
      fail_check "$title" "the application could not store ${scope}${seeded}"
      return
    fi
  done

  # A delimiter one level in: `listing/index.html` is a key here, and
  # everything under `listing/img/` is one folder rather than three keys.
  local top
  top=$(app_request "$headers" "${BLOB_ROUTE}?op=list&prefix=${scope}&delimiter=%2F")
  local keys prefixes
  keys=$(echo "$top" | jq -r '[.objects[].key] | join(",")' 2>/dev/null || true)
  prefixes=$(echo "$top" | jq -r '.prefixes | join(",")' 2>/dev/null || true)

  # One level down, one object per page, walked to the end the way the binding
  # documents: page until list_complete, not until a page comes back short.
  local cursor="" walked="" pages=0 complete="false"
  while [ "$complete" != "true" ] && [ "$pages" -lt 10 ]; do
    local page
    page=$(app_request "$headers" "${BLOB_ROUTE}?op=list&prefix=${scope}img/&limit=1&cursor=${cursor}")
    walked="${walked}$(echo "$page" | jq -r '[.objects[].key] | join(",")' 2>/dev/null || true),"
    complete=$(echo "$page" | jq -r '.list_complete' 2>/dev/null || echo false)
    cursor=$(echo "$page" | jq -r '.cursor // ""' 2>/dev/null || true)
    pages=$((pages + 1))
  done
  walked=$(echo "$walked" | tr -s ',' ',' | sed 's/,$//')

  local want_keys="${scope}index.html"
  local want_prefixes="${scope}img/"
  local want_walk="${scope}img/hero.png,${scope}img/icons/star.svg,${scope}img/logo.png"
  if [ "$keys" = "$want_keys" ] && [ "$prefixes" = "$want_prefixes" ] \
    && [ "$walked" = "$want_walk" ]; then
    pass_check "$title" \
      "list({ prefix: \"${scope}\", delimiter: \"/\" }) answered objects [${keys}] and prefixes [${prefixes}] - the three files under ${scope}img/ cost one entry, not three; paging that folder one object at a time took ${pages} pages and visited ${walked} in key order, each key exactly once"
  else
    fail_check "$title" \
      "expected objects [${want_keys}], prefixes [${want_prefixes}] and a walk of ${want_walk}; got objects [${keys:-<none>}], prefixes [${prefixes:-<none>}], walk [${walked:-<none>}] over ${pages} pages"
  fi
}

check_blob_head() {
  local title="context.env.BLOB reads an object's size and etag without its body"
  local headers="${LOG_DIR}/verify-blob-head.headers"

  local head
  head=$(app_request "$headers" "${BLOB_ROUTE}?op=head&key=index.html")
  local size etag type
  size=$(echo "$head" | jq -r '.size // empty' 2>/dev/null || true)
  etag=$(echo "$head" | jq -r '.etag // empty' 2>/dev/null || true)
  type=$(echo "$head" | jq -r '.contentType // empty' 2>/dev/null || true)

  local missing_status
  missing_status=$(curl -s -o /dev/null -w '%{http_code}' -H "x-deis-app-id: ${APP_ID}" \
    "http://127.0.0.1:${DEIS_RUNTIME_PORT}${BLOB_ROUTE}?op=head&key=never-written" || true)

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
    "http://127.0.0.1:${DEIS_RUNTIME_PORT}${BLOB_ROUTE}?key=index.html" || true)
  local absent
  absent=$(curl -s -o /dev/null -w '%{http_code}' -H "x-deis-app-id: ${APP_ID}" \
    -H "range: bytes=0-1" \
    "http://127.0.0.1:${DEIS_RUNTIME_PORT}${BLOB_ROUTE}?key=never-written" || true)

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
    --data-binary "held by the first writer" "${url}?key=${key}&ifNoneMatch=*" || true)
  second=$(curl -s -o "${LOG_DIR}/verify-blob-cas-second.json" -w '%{http_code}' \
    -X PUT -H "x-deis-app-id: ${APP_ID}" \
    --data-binary "held by the second writer" "${url}?key=${key}&ifNoneMatch=*" || true)

  local kept
  kept=$(app_request "$headers" "${BLOB_ROUTE}?key=${key}")

  # Compare-and-set on the etag: the current one writes, the superseded one
  # does not.
  local etag
  etag=$(app_request "$headers" "${BLOB_ROUTE}?op=head&key=${key}" | jq -r '.etag // empty' 2>/dev/null || true)
  local updated stale
  updated=$(curl -s -o /dev/null -w '%{http_code}' -X PUT -H "x-deis-app-id: ${APP_ID}" \
    --data-binary "second version" "${url}?key=${key}&ifMatch=${etag}" || true)
  stale=$(curl -s -o /dev/null -w '%{http_code}' -X PUT -H "x-deis-app-id: ${APP_ID}" \
    --data-binary "third version" "${url}?key=${key}&ifMatch=${etag}" || true)

  # And a conditional delete against the etag that is no longer current.
  local stale_delete
  stale_delete=$(curl -s -o /dev/null -w '%{http_code}' -X DELETE -H "x-deis-app-id: ${APP_ID}" \
    "${url}?key=${key}&ifMatch=${etag}" || true)

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

  # `--binding BLOB` on every call, for the same reason `add_blob_route`
  # declares the binding: the handler stores through `context.env.BLOB`, and a
  # named binding is its own namespace inside the tenant. An operator who omits
  # the name is not looking at the application's objects, so the CLI has to be
  # given the name the application was given or this check would be comparing
  # two different namespaces and calling the difference a bug.
  if ! deis_cli blob list img/ --binding BLOB --json > "${out}-list.json" 2> "${out}-list.log"; then
    fail_check "$title" \
      "deis blob list failed: $(tail -n 3 "${out}-list.log" | tr '\n' ' ')"
    return
  fi
  if ! deis_cli blob head index.html --binding BLOB --json > "${out}-head.json" 2> "${out}-head.log"; then
    fail_check "$title" \
      "deis blob head failed: $(tail -n 3 "${out}-head.log" | tr '\n' ' ')"
    return
  fi
  if ! deis_cli blob get index.html --offset 4 --length 6 --binding BLOB --json \
    > "${out}-get.json" 2> "${out}-get.log"; then
    fail_check "$title" \
      "deis blob get --offset failed: $(tail -n 3 "${out}-get.log" | tr '\n' ' ')"
    return
  fi

  # The same listing with no binding named. It has to come back empty: that is
  # what makes the three answers above evidence that the CLI reached the
  # application's namespace rather than evidence that every namespace answers
  # the same. If this ever listed the seeded keys, a binding would not be an
  # isolation boundary at all.
  if ! deis_cli blob list img/ --json > "${out}-unnamed.json" 2> "${out}-unnamed.log"; then
    fail_check "$title" \
      "deis blob list without --binding failed: $(tail -n 3 "${out}-unnamed.log" | tr '\n' ' ')"
    return
  fi

  local listed folders headed_size headed_etag sliced offset length
  local namespace unnamed_namespace unnamed_count
  listed=$(jq -r '[.objects[].key] | join(",")' < "${out}-list.json" 2>/dev/null || true)
  folders=$(jq -r '.prefixes | join(",")' < "${out}-list.json" 2>/dev/null || true)
  # Read off the answer rather than assumed: the CLI prints the namespace it
  # presented to deis-store, and that string is the whole claim here.
  namespace=$(jq -r '.tenant // empty' < "${out}-list.json" 2>/dev/null || true)
  headed_size=$(jq -r '.size // empty' < "${out}-head.json" 2>/dev/null || true)
  headed_etag=$(jq -r '.sha256 // empty' < "${out}-head.json" 2>/dev/null || true)
  # A blob body is bytes, so --json carries it base64 for the reason
  # `deis kv get` does.
  sliced=$(jq -r '.value_base64 // empty' < "${out}-get.json" 2>/dev/null \
    | openssl base64 -d -A 2>/dev/null || true)
  offset=$(jq -r '.range.offset // empty' < "${out}-get.json" 2>/dev/null || true)
  length=$(jq -r '.range.length // empty' < "${out}-get.json" 2>/dev/null || true)
  unnamed_namespace=$(jq -r '.tenant // empty' < "${out}-unnamed.json" 2>/dev/null || true)
  unnamed_count=$(jq -r '.objects | length' < "${out}-unnamed.json" 2>/dev/null || true)

  if [ "$listed" = "img/hero.png,img/icons/star.svg,img/logo.png" ] \
    && [ "$namespace" = "${APP_ID}.${ENV_NAME}@BLOB" ] \
    && [ "$headed_size" = "16" ] && [ -n "$headed_etag" ] \
    && [ "$sliced" = "456789" ] && [ "$offset" = "4" ] && [ "$length" = "6" ] \
    && [ "$unnamed_namespace" = "${APP_ID}.${ENV_NAME}" ] && [ "$unnamed_count" = "0" ]; then
    pass_check "$title" \
      "\`deis blob list img/ --binding BLOB\` printed ${listed} (prefixes [${folders}]) and reported the namespace as ${namespace} - the one \`context.env.BLOB\` writes into; \`deis blob head index.html --binding BLOB\` reported 16 bytes and sha256 ${headed_etag:0:12}; \`deis blob get index.html --offset 4 --length 6 --binding BLOB\` returned '${sliced}' and reported range offset ${offset} length ${length}. Every one of these objects was stored by the application, not by the CLI. The same listing without --binding looks at ${unnamed_namespace} and finds ${unnamed_count} objects there, so the named namespace sits beside the unnamed one rather than on top of it"
  else
    fail_check "$title" \
      "expected the three img/ keys in namespace ${APP_ID}.${ENV_NAME}@BLOB, a 16-byte head, the slice '456789' at offset 4 length 6, and nothing under img/ in ${APP_ID}.${ENV_NAME}; got list [${listed:-<none>}] in namespace '${namespace:-<none>}', head size '${headed_size:-<none>}', slice '${sliced:-<none>}' at offset '${offset:-<none>}' length '${length:-<none>}', and ${unnamed_count:-<none>} objects in '${unnamed_namespace:-<none>}'"
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
    "http://127.0.0.1:${DEIS_RUNTIME_PORT}/api/enqueue" || true)
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

  # Taken before the request, so that the line read below is one this request
  # could have written and nothing earlier can be.
  local mark
  mark=$(node_log_mark)

  local sent
  sent=$(curl -s -D "$headers" -X POST -H "x-deis-app-id: ${APP_ID}" \
    -H 'content-type: application/json' -d "$jobs" \
    "http://127.0.0.1:${DEIS_RUNTIME_PORT}/api/enqueue-batch" || true)
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
  # Written by the handler on the `sendBatch` path of the request this check
  # made a moment ago, so only the part of the log that request could have
  # written is read. The wording alone is a fact about the file: this check
  # runs more than once across runs against a long-lived node, and anything
  # that can reach /api/enqueue can put the sentence there directly.
  local batch_line
  batch_line=$(node_log_since "$mark" | grep -m 1 -F "queued ${size} job(s) in one batch" || true)

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

  # Startup evidence: written once, by the consumer task as it was spawned,
  # long before this phase began. The prefix that existed then is where it is,
  # and is a window nothing this phase provokes can reach into.
  local line applied
  line=$(node_log_before_verify \
    | grep -m 1 -F "Consuming queue jobs for app ${APP_ID}.${ENV_NAME}:" || true)
  applied=$(echo "$line" | sed -n 's/.*at most \([0-9]*\) batch(es).*/\1/p')

  # The line above is written once, by the consumer task as it is spawned, and
  # then it sits in the log for the rest of the run. So a check that read only
  # the line was reporting what the node *said when it started*, and this one
  # passed with deis-queue killed and its port free -- measured, not supposed.
  # The claim is about a consumer that is holding batches, which is the present
  # tense, so the present tense has to be asked: one message of this check's
  # own, enqueued now, and consumed now.
  #
  # Its own message rather than another check's: an assertion resting on what
  # some earlier check left behind is the same defect as one resting on a line
  # some earlier run left behind. The consumer writes the value under
  # `job:<key>` in KV, which is the effect an outsider cannot produce for this
  # key because nobody else knows it.
  local key="concurrency-$$-${RANDOM}"
  local value="consumed-while-this-check-watched"
  local headers="${LOG_DIR}/verify-queue-concurrency.headers"
  local sent
  sent=$(curl -s -D "$headers" -X POST -H "x-deis-app-id: ${APP_ID}" \
    -H 'content-type: application/json' \
    -d "{\"key\":\"${key}\",\"value\":\"${value}\"}" \
    "http://127.0.0.1:${DEIS_RUNTIME_PORT}/api/enqueue" || true)
  local accepted
  accepted=$(http_status "$headers")

  # The consumer is a poller, so what there is to do is wait for its effect.
  # curl paces its own retries a second apart; nothing here sleeps.
  local got consumed=""
  if [ "$accepted" = "202" ]; then
    got=$(curl -sf --retry 45 --retry-delay 1 --retry-max-time 90 --retry-all-errors \
      -H "x-deis-app-id: ${APP_ID}" \
      "http://127.0.0.1:${DEIS_RUNTIME_PORT}/api/kv/job:${key}" 2>/dev/null || echo '')
    consumed=$(echo "$got" | jq -r '.value // empty' 2>/dev/null || true)
  fi

  if [ "$applied" = "$declared" ] && [ -n "$line" ] \
    && echo "$line" | grep -q "Outstanding, not parallel" \
    && [ "$consumed" = "$value" ]; then
    pass_check "$title" \
      "the project asks for maxConcurrency ${declared} on queue jobs and the node reported \"$(echo "$line" | log_message)\". The consumer that line describes is the one running now: job:${key}, enqueued by this check, came back out of it"
  else
    fail_check "$title" \
      "expected the node to log that it holds ${declared} batch(es) of queue jobs at once, to say the number is not parallelism, and that consumer to still be consuming; it reported '${applied:-<none>}' in the line $(if [ -n "$line" ]; then echo "\"$(echo "$line" | log_message)\""; else echo "which was never written"; fi), POST /api/enqueue answered ${accepted:-nothing} and job:${key} read back '${consumed:-<nothing>}' within 90s"
  fi
}

# The dead letter table, from a terminal.
#
# A message that fails more times than the queue's ceiling allows is not
# deleted: deis-queue moves it to a dead letter table and stops delivering it.
# Until `deis queue` existed nothing outside deis-queue's own tests ever read
# that table, so the work in it was reachable only by opening queue.sqlite
# with the sqlite3 shell on whichever host holds it - which made the dead
# letter table a place work went to disappear rather than a place it waits.
#
# The poison needs no change to the application. A job whose key is longer
# than the 1024 bytes deis-store accepts (crates/deis-store/src/kv.rs) can
# never be written, so the consumer's put throws on every delivery, the
# handler asks for that message back, and the queue gives up on it once the
# attempts run out.
check_queue_dead_letters() {
  local title="a message the consumer cannot handle is dead lettered, and deis queue shows it and puts it back"
  local out="${LOG_DIR}/cli-queue-dead-letters"
  local headers="${LOG_DIR}/verify-queue-dead-letter.headers"
  local key
  key="poison-$$-${RANDOM}-$(printf '%1100s' '' | tr ' ' 'x')"

  local sent
  sent=$(curl -s -D "$headers" -X POST -H "x-deis-app-id: ${APP_ID}" \
    -H 'content-type: application/json' \
    -d "{\"key\":\"${key}\",\"value\":\"never-stored\"}" \
    "http://127.0.0.1:${DEIS_RUNTIME_PORT}/api/enqueue" || true)
  if [ "$(http_status "$headers")" != "202" ]; then
    fail_check "$title" "POST /api/enqueue answered $(http_status "$headers"): ${sent}"
    return
  fi

  # The wait is real: the node hands the message back with a ten-second retry
  # delay and the queue only gives up at the fifth delivery, so this is around
  # a minute. Polled on the condition - the message appearing in the table -
  # rather than assumed, and every poll is the operator's own command; the
  # second between rounds is poll_pause, curl's retry pacing, like every
  # other poll in this file.
  local waited=0 found="" listed=""
  while [ "$waited" -lt 180 ]; do
    if deis_cli queue dead-letters --queue jobs --json \
      > "${out}-list.json" 2> "${out}-list.log"; then
      # Selected by body rather than by position: two verify runs against one
      # demo would each have their own poison message in this table.
      found=$(jq -r --arg key "$key" \
        '.messages[] | select((.body_base64 | @base64d) | contains($key)) | .id' \
        < "${out}-list.json" 2>/dev/null | head -n 1)
      if [ -n "$found" ]; then
        listed=$(jq -c --arg id "$found" '.messages[] | select(.id == $id)' \
          < "${out}-list.json" 2>/dev/null || true)
        break
      fi
    fi
    poll_pause
    waited=$((waited + 1))
  done

  if [ -z "$found" ]; then
    fail_check "$title" \
      "nothing was dead lettered for ${key:0:24}... within ${waited}s; the last listing was $(head -c 300 "${out}-list.json" 2>/dev/null || echo '<none>'), the last error $(tail -n 2 "${out}-list.log" 2>/dev/null | tr '\n' ' ')"
    return
  fi

  local attempts reason ceiling
  attempts=$(echo "$listed" | jq -r '.attempts // empty' 2>/dev/null || true)
  reason=$(echo "$listed" | jq -r '.reason // empty' 2>/dev/null || true)
  ceiling=5

  # The human report, which is what an operator actually reads.
  if ! deis_cli queue dead-letters --queue jobs > "${out}-report.txt" 2>&1; then
    fail_check "$title" \
      "deis queue dead-letters failed without --json: $(tail -n 3 "${out}-report.txt" | tr '\n' ' ')"
    return
  fi

  # And back out again. --delay is what an operator uses to put work back
  # while a fix is still rolling out; an hour here also keeps this demo's
  # consumer from spending the rest of the run re-poisoning itself.
  if ! deis_cli queue replay --queue jobs "$found" --delay 3600 --json \
    > "${out}-replay.json" 2> "${out}-replay.log"; then
    fail_check "$title" \
      "deis queue replay failed: $(tail -n 3 "${out}-replay.log" | tr '\n' ' ')"
    return
  fi
  local replayed not_found
  replayed=$(jq -r '.replayed // empty' < "${out}-replay.json" 2>/dev/null || true)
  not_found=$(jq -r '.not_found // empty' < "${out}-replay.json" 2>/dev/null || true)

  # It has to be gone from the table: the replay moves the row, it does not
  # copy it, and a message still listed here is one nobody has put back.
  local still_there=""
  if deis_cli queue dead-letters --queue jobs --json \
    > "${out}-after.json" 2> "${out}-after.log"; then
    still_there=$(jq -r --arg id "$found" '.messages[] | select(.id == $id) | .id' \
      < "${out}-after.json" 2>/dev/null || true)
  fi

  if [ "$attempts" = "$ceiling" ] \
    && [ "$reason" = "the consumer reported it failed" ] \
    && grep -q "$found" "${out}-report.txt" \
    && grep -q "the consumer reported it failed" "${out}-report.txt" \
    && [ "$replayed" = "1" ] && [ "$not_found" = "0" ] && [ -z "$still_there" ]; then
    pass_check "$title" \
      "the consumer failed the message ${ceiling} times over ${waited}s and the queue gave up on it; \`deis queue dead-letters\` showed it as ${found} with ${attempts} attempts and the reason \"${reason}\" - not \"its lease lapsed\", which is the other way a message gets here and needs a different fix - and \`deis queue replay --delay 3600\` put it back and left the table without it"
  else
    fail_check "$title" \
      "expected ${ceiling} attempts with the reason \"the consumer reported it failed\", the id and the reason in the human report, and a replay of 1 with 0 not found that empties the row; got attempts '${attempts:-<none>}', reason '${reason:-<none>}', replayed '${replayed:-<none>}', not_found '${not_found:-<none>}', still listed '${still_there:-<no>}'"
  fi
}

# The capability token on the queue, asked the one question nothing else in
# this demo asks it.
#
# `start` writes the same auth_secret onto deis-store, deis-queue and
# deis-control, and every queue check above reaches the queue through the
# node - which always carries the token the control plane handed it with the
# deployment. So DEIS_QUEUE_PORT was configured, started, waited on and
# printed in the summary, and never dialled by anything without a credential:
# deis-queue could have been started with no auth_secret at all, or its
# interceptor could have been left off the service, and every queue check
# would still have been green. That is the hole check_storage_capability_token
# closes on deis-store; this is the queue's half of it
# (crates/deis-queue/src/auth.rs).
#
# ListDeadLetters is the call to ask it with: it is the operator's read of the
# dead letter table and changes nothing, so neither the refusal nor the served
# call moves a message the rest of the demo depends on. Three probes, because
# a refusal alone would also be what a queue that refuses everything looks
# like: no token, a wrong token, and the real one.
check_queue_capability_token() {
  local title="a queue call presenting no capability token is refused"

  if ! command -v grpcurl >/dev/null 2>&1; then
    fail_check "$title" "grpcurl is not installed, so this check could not be run"
    return
  fi
  if ! command -v openssl >/dev/null 2>&1; then
    fail_check "$title" \
      "openssl is not installed, so this check could not mint the token its third probe needs"
    return
  fi

  # The same tenant form the node presents for this application and
  # environment, and the same token construction every side of the platform
  # uses (deis_wire::conventions::storage_token).
  local tenant="${APP_ID}.${ENV_NAME}"
  local request='{"queue_name":"jobs","limit":1}'
  local none wrong served none_rc wrong_rc served_rc

  none=$(grpcurl -plaintext -max-time 5 \
    -import-path "${ROOT_DIR}/crates/deis-queue/proto" -proto queue.proto \
    -H "x-deis-tenant: ${tenant}" \
    -d "$request" \
    "127.0.0.1:${DEIS_QUEUE_PORT}" deis.queue.Queue/ListDeadLetters 2>&1) && none_rc=0 || none_rc=$?

  wrong=$(grpcurl -plaintext -max-time 5 \
    -import-path "${ROOT_DIR}/crates/deis-queue/proto" -proto queue.proto \
    -H "x-deis-tenant: ${tenant}" \
    -H "x-deis-storage-token: not-the-token-for-this-tenant" \
    -d "$request" \
    "127.0.0.1:${DEIS_QUEUE_PORT}" deis.queue.Queue/ListDeadLetters 2>&1) && wrong_rc=0 || wrong_rc=$?

  served=$(grpcurl -plaintext -max-time 5 \
    -import-path "${ROOT_DIR}/crates/deis-queue/proto" -proto queue.proto \
    -H "x-deis-tenant: ${tenant}" \
    -H "x-deis-storage-token: $(storage_token "$tenant")" \
    -d "$request" \
    "127.0.0.1:${DEIS_QUEUE_PORT}" deis.queue.Queue/ListDeadLetters 2>&1) && served_rc=0 || served_rc=$?

  if [ "$none_rc" -ne 0 ] && echo "$none" | grep -q 'Code: Unauthenticated' \
    && [ "$wrong_rc" -ne 0 ] && echo "$wrong" | grep -q 'Code: Unauthenticated' \
    && [ "$served_rc" -eq 0 ]; then
    pass_check "$title" \
      "ListDeadLetters at deis-queue's own port for tenant ${tenant} was refused with no x-deis-storage-token header - $(echo "$none" | tr '\n' ' ' | sed 's/  */ /g' | head -c 120) - and refused again with a token that is not this tenant's, while the same call carrying the token the control plane mints from the same secret was served. Nothing that can merely reach the queue's port can list this tenant's dead letters. Drain and replay were not probed here: they sit behind the same interceptor (crates/deis-queue/src/server.rs wraps the whole service in it), which this check shows is switched on and does not show is complete"
    return
  fi

  fail_check "$title" \
    "expected Code: Unauthenticated with no token (grpcurl exited ${none_rc}, said $(echo "$none" | tr '\n' ' ' | head -c 140)), the same with a wrong one (exited ${wrong_rc}, said $(echo "$wrong" | tr '\n' ' ' | head -c 140)), and the real token to be served (exited ${served_rc}, said $(echo "$served" | tr '\n' ' ' | head -c 140))"
}

# The UTC second of an ISO 8601 instant as digits:
# "2026-09-02T12:01:00.000Z" -> 20260902120100.
#
# Compared as a number rather than as text so that nothing rests on how a
# locale collates '.' against 'Z', and cut at the second because the marker a
# verify run takes for itself carries no fraction.
cron_instant_digits() {
  local at=${1%%.*}
  at=${at//[-T:Z]/}
  printf '%s' "$at"
}

# Does this record describe a tick that fired after the moment this verify run
# chose for itself?
#
# The decision is kept apart from the fetching so it can be held to canned
# values in scripts/test-demo-witnesses.sh rather than only to a live demo.
# All three have to hold:
#
#   * the counter is above the one that was in the store when this run started
#     looking, so the record an earlier verify against the same `start` left
#     behind is not enough on its own;
#   * the timestamp has the shape `toISOString()` produces, so a stored string
#     that is not an instant cannot pass as one;
#   * that instant is later than the marker, which is what a tick scheduled
#     before this run began cannot satisfy - including the one that was due in
#     the same second the marker was taken.
cron_tick_is_fresh() {
  local previous_tick=$1 marker=$2 tick=$3 at=$4

  case "$tick" in
    '' | *[!0-9]*) return 1 ;;
  esac
  [ "$tick" -gt "$previous_tick" ] || return 1

  printf '%s' "$at" \
    | grep -Eq '^[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:[0-9]{2}(\.[0-9]+)?Z$' \
    || return 1

  [ "$(cron_instant_digits "$at")" -gt "$(cron_instant_digits "$marker")" ]
}

check_cron() {
  # The whole cron path in one observation: the expression rode into the
  # control plane inside the bundle manifest, the scheduler rebuilt its task
  # list from the catalog, it picked a node and called Trigger/OnScheduled,
  # and the app's `scheduled` export wrote the record below.
  #
  # What this check used to be, and why it is not that any more. It read
  # `cron:last-run` and passed on any non-empty value. The key is fixed, so a
  # second `verify` against one `start` passed on the value the first one had
  # already seen - cron could have stopped firing in between and nothing here
  # would say so. The value was never parsed, so anything at all in that key
  # satisfied it. And the app's own KV endpoint takes no credential, so
  # anything that could reach the node could write the key by hand. Three ways
  # to pass without a tick having happened.
  #
  # So the evidence now has to be all of: a tick this run had not already seen
  # (a counter that only goes up), scheduled after a moment this run chose (an
  # instant that parses, compared against that marker), recorded by the
  # platform as well as by the application (`deis trace --kind cron`, which
  # comes from the node's own report to deis-telemetry and not through KV),
  # and unwritable from the endpoint this check reads it through (the app
  # refuses a PUT under `cron:`).
  #
  # The expression is "every minute" and the demo sets the scheduler's tick to
  # five seconds, so each round of the wait is a minute plus a little rather
  # than the three minutes the default tick would need. curl paces its own
  # retries a second apart; nothing here sleeps.
  local title="a cron expression fires on one node and runs the app's handler"
  local kv="http://127.0.0.1:${DEIS_RUNTIME_PORT}/api/kv"

  # The moment this run chose. Taken before anything is read, so every
  # judgement below is about what happened after it.
  local marker marker_us
  marker=$(date -u +%Y-%m-%dT%H:%M:%SZ)
  marker_us=$(( $(date -u +%s) * 1000000 ))

  # Half one: the endpoint this check reads the record through cannot also
  # write it. The refusal is by prefix rather than by key, so the probe writes
  # a name of its own instead of touching the record the rest of the check
  # depends on - a probe that could corrupt the evidence it is checking would
  # be its own defect.
  local probe_key="cron:forgery-probe-$$-${RANDOM}"
  local probe_headers="${LOG_DIR}/verify-cron-forgery.headers"
  local refused forged_status probe_found
  refused=$(curl -s -D "$probe_headers" -X PUT -H "x-deis-app-id: ${APP_ID}" \
    --data-binary '{"at":"2099-01-01T00:00:00.000Z","tick":999999}' \
    "${kv}/${probe_key}" 2>/dev/null || true)
  forged_status=$(http_status "$probe_headers")
  # `tostring` rather than `// empty`: jq's `//` yields its right side for
  # `false` as well as for null, so the one answer this probe wants to see -
  # "found": false - would come back indistinguishable from no answer at all.
  probe_found=$(curl -s -m 5 -H "x-deis-app-id: ${APP_ID}" "${kv}/${probe_key}" 2>/dev/null \
    | jq -r '.found | tostring' 2>/dev/null || true)

  # What the store already held. Missing - the first minute after `start` -
  # and a value this app did not write both read as tick 0, which any real
  # tick beats.
  local seen previous_record previous_tick previous_at
  seen=$(curl -s -m 5 -H "x-deis-app-id: ${APP_ID}" "${kv}/cron:last-run" 2>/dev/null || true)
  previous_record=$(echo "$seen" | jq -r '.value // empty' 2>/dev/null || true)
  previous_tick=$(echo "$previous_record" | jq -r '.tick // empty' 2>/dev/null || true)
  previous_at=$(echo "$previous_record" | jq -r '.at // empty' 2>/dev/null || true)
  case "$previous_tick" in
    '' | *[!0-9]*) previous_tick=0 ;;
  esac

  # Half two: wait for one named key rather than re-reading a key that is
  # already there. `cron:tick:<n>` does not exist until tick n fires, so curl
  # retries a 404 until it does and the wait is paced by curl itself. A tick
  # that turns out to have been scheduled before the marker is not evidence
  # for this run, so the wait moves on to the next one rather than settling
  # for it; three rounds is two more than a healthy demo needs and still
  # bounded.
  local want=$((previous_tick + 1))
  local round=0 fresh=0 record="" tick="" at="" waited_for=""
  while [ "$round" -lt 3 ]; do
    round=$((round + 1))
    waited_for="cron:tick:${want}"
    record=$(curl -sf --retry 100 --retry-delay 1 --retry-max-time 130 --retry-all-errors \
      -H "x-deis-app-id: ${APP_ID}" "${kv}/${waited_for}" 2>/dev/null \
      | jq -r '.value // empty' 2>/dev/null || true)
    if [ -z "$record" ]; then
      break
    fi
    tick=$(echo "$record" | jq -r '.tick // empty' 2>/dev/null || true)
    at=$(echo "$record" | jq -r '.at // empty' 2>/dev/null || true)
    if cron_tick_is_fresh "$previous_tick" "$marker" "$tick" "$at"; then
      fresh=1
      break
    fi
    want=$((want + 1))
  done

  # Half three: the platform's own account of the same tick, which does not
  # come through KV at all. The node reports every invocation to
  # deis-telemetry with the kind it ran under, so `deis trace --kind cron` is
  # the other side of the same event - the way the log-line checks pair what
  # an application said with the trace id the platform gave the invocation
  # that said it. An application that wrote the KV record without a tick, or
  # an outsider that wrote it from outside, produces no row here.
  #
  # Polled rather than asked once: a node ships what it has recorded in
  # batches, so the row for a tick arrives a few seconds after the KV record
  # the handler wrote inside it, and one query at the moment the record
  # appears asks too early. The condition is the row, not a duration - and
  # the poll only runs when there is a record to corroborate, so a demo whose
  # cron never fired fails on the record rather than spending a minute here.
  local traces="${LOG_DIR}/verify-cron-trace.json"
  local platform_ticks=0 platform_newest="" trace_waited=0
  # A node buffers its traces and pushes them on a timer, so the record of
  # the tick the store already shows may not have reached deis-telemetry yet.
  # Ask once; if it is not there, wait on the platform's own event - `deis
  # trace --follow` streams each record as it lands, and the first one to
  # arrive (this tick's, or the next tick's, either of which started after
  # the marker) ends the wait - and ask again. `timeout` bounds the wait; no
  # polling loop and nothing here sleeps.
  read_platform_ticks() {
    deis_cli trace --kind cron --since 15m --limit 200 --json \
      > "$traces" 2> "${LOG_DIR}/verify-cron-trace.log" || return 0
    platform_ticks=$(jq -r --argjson since "$marker_us" \
      '[.invocations[] | select(.started_us >= $since)] | length' \
      < "$traces" 2>/dev/null || echo 0)
    platform_newest=$(jq -r --argjson since "$marker_us" \
      '[.invocations[] | select(.started_us >= $since)] | last | .time // empty' \
      < "$traces" 2>/dev/null || true)
    case "$platform_ticks" in
      '' | *[!0-9]*) platform_ticks=0 ;;
    esac
  }
  if [ "$fresh" = "1" ]; then
    read_platform_ticks
    if [ "$platform_ticks" -lt 1 ]; then
      local followed_from=$SECONDS
      deis_cli_env
      timeout 60 env "${DEIS_CLI_ENV[@]}" "${BIN_DIR}/deis" trace --kind cron --follow --json \
        2>> "${LOG_DIR}/verify-cron-trace.log" | head -n 1 > /dev/null || true
      trace_waited=$((SECONDS - followed_from))
      read_platform_ticks
    fi
  fi

  if [ "$fresh" = "1" ] && [ "$forged_status" = "403" ] && [ "$probe_found" = "false" ] \
    && [ "$platform_ticks" -ge 1 ]; then
    pass_check "$title" \
      "at ${marker} the store held $(if [ -n "$previous_at" ]; then echo "tick ${previous_tick} scheduled ${previous_at}"; else echo "no cron record at all"; fi); this check then waited for ${waited_for}, a key that does not exist until that tick fires, and the scheduled handler wrote it with tick ${tick} scheduled ${at} - later than the marker, so no earlier run and no earlier tick can be what satisfied this. The platform says the same thing by its own route, which does not pass through KV at all: deis trace --kind cron lists ${platform_ticks} cron invocation(s) that the node reported to deis-telemetry as having started after that marker, the newest at ${platform_newest} (node-local time). That is corroboration that cron ran in this window rather than a second reading of this one tick - the trace carries the expression, not the counter - and it is a row an application cannot write and a caller of the KV endpoint cannot produce. And the record is not something a caller can write: PUT ${probe_key} through the app's own KV endpoint - the credential-free endpoint this check reads through - answered ${forged_status} and stored nothing"
  else
    fail_check "$title" \
      "expected a cron tick scheduled after ${marker}: waited for ${waited_for:-<nothing>} through ${round} round(s) from tick ${previous_tick} and got $(if [ -n "$record" ]; then echo "tick '${tick:-<none>}' scheduled '${at:-<none>}'"; else echo "nothing"; fi); deis trace --kind cron reports ${platform_ticks} invocation(s) started after the marker after ${trace_waited}s of asking for one (expected at least 1, see ${LOG_DIR}/verify-cron-trace.log; 0s means the record above never arrived, so the platform's side was not asked); and a PUT under the cron: prefix answered ${forged_status:-<none>} (expected 403) leaving found='${probe_found:-<none>}' (expected false)$(if [ -n "$refused" ]; then echo ", saying \"$(printf '%s' "$refused" | head -c 120)\""; fi)"
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
# a preview environment's declared KV binding is invisible to production and vice versa.
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

  # Before the two requests, so that the cut-off line read below is one of
  # these two requests' and not a line already sitting in the file.
  local mark
  mark=$(node_log_mark)

  local overran within
  overran=$(timed_slow_request "$ENV_NAME" "$over")
  within=$(timed_slow_request "$ENV_NAME" "$under")

  # The node's own account of the ceiling it installed, so the numbers below
  # are the node's rather than this script's idea of them. Written when this
  # version was deployed, which is before this phase started, so it is read
  # from the prefix that existed then: the wording on its own is text anything
  # reaching this node can have an application echo into the same file.
  local applied_line node_default applied
  applied_line=$(node_log_window "${DEMO_APP_DEPLOY_FROM:-0}" "${DEMO_APP_DEPLOY_TO:-0}" \
    | grep -m 1 -F "App ${APP_ID}.${ENV_NAME} narrows the default execution timeout" || true)
  applied=$(echo "$applied_line" | sed -n 's/.*timeout: \([0-9]*\)ms.*/\1/p')
  node_default=$(echo "$applied_line" | sed -n 's/.*platform default \([0-9]*\)ms.*/\1/p')

  # The other half is about one of the two requests just made, so it is read
  # from what the file gained since the mark above.
  local cut_off_line
  cut_off_line=$(node_log_since "$mark" \
    | grep -m 1 -F "App ${APP_ID}.${ENV_NAME} was cut off on one request" || true)

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

  # All three lines are about a deployment, and every deployment this demo
  # makes happened before this phase began. So all three are read from the
  # prefix the log had then, and not from the whole file.
  #
  # This is a security check and it had wording as its only evidence, which
  # made it the cheapest one here to turn green falsely: the sentences below
  # contain no value this script invented, so an application asked to log the
  # right sentence - which one unauthenticated request is enough to arrange,
  # because the demo handler logs a job key verbatim - satisfied both of the
  # two that have to be present. The clamp was then reported from a line
  # nobody had clamped anything to write.
  #
  # A window is not as strong as a nonce. It cannot be one here: there is no
  # request to put a nonce in, because the evidence is about a deploy that
  # already happened. So the window is made as small as the thing it is about
  # - the lines `deis deploy -e preview` itself produced, bracketed by two
  # marks the start phase took either side of that one command. Reading the
  # whole file, or even the whole of it before verification, leaves minutes in
  # which the node was listening on an unauthenticated port; this leaves the
  # duration of one deploy.
  local window
  window=$(node_log_window_named "${DEMO_PREVIEW_DEPLOY_FROM:-0}" \
    "${DEMO_PREVIEW_DEPLOY_TO:-0}" "the deploy of ${APP_ID}.${PREVIEW_ENV_NAME}")

  local asked_line clamp_line applied narrowed
  asked_line=$(node_log_window "${DEMO_PREVIEW_DEPLOY_FROM:-0}" "${DEMO_PREVIEW_DEPLOY_TO:-0}" \
    | grep -m 1 -F "timeout_ms=Some(${GREEDY_TIMEOUT_MS})" || true)
  clamp_line=$(node_log_window "${DEMO_PREVIEW_DEPLOY_FROM:-0}" "${DEMO_PREVIEW_DEPLOY_TO:-0}" \
    | grep -m 1 -F "App ${APP_ID}.${PREVIEW_ENV_NAME} asks for a ${GREEDY_TIMEOUT_MS}ms execution timeout" || true)
  applied=$(echo "$clamp_line" | sed -n "s/.*above this node's \([0-9]*\)ms ceiling.*/\1/p")

  # The line that must *not* exist: "narrows" is what the node writes when it
  # takes an application's number, so the greedy version producing one would
  # mean the clamp had let it through. Read from the same window as the other
  # two, and for a second reason: an absence is only meaningful over the range
  # the presences were read from. Over the whole file, one line from a later
  # deploy - or one an application echoed - would report the clamp broken.
  narrowed=$(node_log_window "${DEMO_PREVIEW_DEPLOY_FROM:-0}" "${DEMO_PREVIEW_DEPLOY_TO:-0}" \
    | grep -m 1 -F "App ${APP_ID}.${PREVIEW_ENV_NAME} narrows the default execution timeout: ${GREEDY_TIMEOUT_MS}ms" || true)

  if [ -n "$asked_line" ] && [ -n "$applied" ] \
    && [ "$applied" != "$GREEDY_TIMEOUT_MS" ] && [ -z "$narrowed" ]; then
    pass_check "$title" \
      "the ${PREVIEW_ENV_NAME} version's manifest reached the node asking for timeout_ms=${GREEDY_TIMEOUT_MS}, and the node answered \"$(echo "$clamp_line" | log_message | sed 's/\. An invocation.*//')\". ${applied}ms is the number in force; no line says this environment narrowed anything, because it did not. All three readings are of ${window}"
    return
  fi

  fail_check "$title" \
    "expected the node to log that ${APP_ID}.${PREVIEW_ENV_NAME} asked for ${GREEDY_TIMEOUT_MS}ms and was held to its own ceiling, in ${window}; the deployment line was $(if [ -n "$asked_line" ]; then echo "present"; else echo "absent"; fi), the clamp reports applying '${applied:-<none>}ms', and a \"narrows\" line for ${GREEDY_TIMEOUT_MS}ms was $(if [ -n "$narrowed" ]; then echo "written, so the request was granted"; else echo "absent"; fi)"
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

  # Before the fan-out, so the refusal line below is one this check's own
  # second request provoked.
  local mark
  mark=$(node_log_mark)

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
  # node's rather than this script's idea of them - and each is read from the
  # window it can have been written in. The narrowing is a deployment fact and
  # predates this phase; the refusal is about the fan-out two lines up.
  local narrowed_line refused_line
  narrowed_line=$(node_log_window "${DEMO_APP_DEPLOY_FROM:-0}" "${DEMO_APP_DEPLOY_TO:-0}" \
    | grep -m 1 -F "App ${APP_ID}.${ENV_NAME} narrows the default subrequest ceiling" || true)
  refused_line=$(node_log_since "$mark" \
    | grep -m 1 -F "this invocation has already made ${declared} outbound call(s)" || true)

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
  # Before the request, because the drop this reads is that request's.
  local mark
  mark=$(node_log_mark)

  local count
  count=$(set_cookie_count "$PREVIEW_ENV_NAME")

  # The node's own account of what it dropped and why, so this quotes the
  # node rather than this script's idea of it - and out of the lines the
  # request just made added, so the sentence cannot be one left behind by an
  # earlier run against the same node or written into the file from outside.
  local dropped
  dropped=$(node_log_since "$mark" \
    | grep -m 1 -F "App ${APP_ID}.${PREVIEW_ENV_NAME} set 2 Set-Cookie headers" || true)

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
  local source
  source=$(jq -r '.bundle.path // empty' < "${LOG_DIR}/cli-deploy-v1.json" 2>/dev/null || true)
  local patched="${DEMO_DIR}/${label}.zip"
  local body="${LOG_DIR}/${label}-upload.json"

  if [ ! -f "$source" ]; then
    echo "the deployed bundle path is absent or no longer exists" >&2
    return 1
  fi

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
    "$(control_url)/api/v1/apps/${APP_ID}/versions"
}

# Read whatever the control plane said about a refused upload.
refusal_message() {
  local file=$1
  local message
  message=$(jq -r '.error.message // empty' < "$file" 2>/dev/null || true)
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
      "POST /api/v1/apps/${APP_ID}/versions with a manifest naming 2099-01-01 answered HTTP ${status}: \"$(echo "$message" | cut -c1-220)\". No version row was written, so no node was ever asked for semantics this build does not have"
  else
    fail_check "$title" \
      "expected HTTP 400 naming 2099-01-01, got HTTP ${status} and message '${message:-<empty>}'"
  fi
}

# The mirror of the check above, and the two are next to each other because
# the difference between them is the rule (`deis_bundle::forward`). A date this
# build cannot honour is refused: it asks for the whole set of answers as of a
# day nobody here has seen, and there is no honest subset of that to serve. A
# flag name this build does not know is accepted and ignored: it names one
# behaviour this build does not have in either direction, so ignoring it is
# exactly what this build was already doing.
#
# This used to expect the refusal, and that refusal was the rollback failure
# in miniature - a control plane one release behind the builder could not take,
# or later re-deliver, any version naming a flag added since, and the manifest
# is inside the archive whose digest is the version id, so nothing repaired
# one. The author's own misspelling is still refused, at the build.
check_an_unknown_compatibility_flag_is_carried_not_refused() {
  local title="a bundle naming a behaviour flag this build does not know is stored, not refused"

  if ! command -v python3 >/dev/null 2>&1; then
    fail_check "$title" "python3 is needed to alter a bundle's manifest"
    return
  fi

  local status message
  status=$(upload_altered_bundle "unknown-flag-bundle" \
    'manifest["compatibility"] = {"date": "2026-09-01", "flags": ["cookies_but_faster"]}')
  message=$(refusal_message "${LOG_DIR}/unknown-flag-bundle-upload.json")

  local stored_date
  stored_date=$(jq -r '.version.manifest.compatibility.date // empty' \
    < "${LOG_DIR}/unknown-flag-bundle-upload.json" 2>/dev/null || true)

  if [ "$status" = "201" ] && [ "$stored_date" = "2026-09-01" ]; then
    pass_check "$title" \
      "POST /api/v1/apps/${APP_ID}/versions with the flag \"cookies_but_faster\" answered HTTP ${status} and stored the version with its date 2026-09-01 intact. The flag names a behaviour this build does not have in either direction, so it is ignored and named at warn rather than costing the whole version - which is what a control plane one release behind a builder has to be able to do"
  else
    fail_check "$title" \
      "expected HTTP 201 with the compatibility date carried through, got HTTP ${status}, date '${stored_date:-<none>}' and message '${message:-<empty>}'"
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
    "http://127.0.0.1:${DEIS_RUNTIME_PORT}/api/fetch" || true
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
  # Non-zero so the caller can stop, and every caller stops with
  # `|| return 0` rather than with a bare `|| return`. A bare one hands this
  # 1 back as the check function's own status, and a check that returns
  # non-zero under `set -e` ends the whole verify run - so the seven checks
  # that use this helper each turned "the node did not answer" into "the run
  # stopped without a summary", which is the one outcome worse than the
  # failure they had just recorded.
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
  answered_json "$title" fetch-allowed "$body" || return 0

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
# Nothing is asserted about what the *handler* was told: no wording, no status
# code, no particular refusal message. A refusal reaches an application as an
# ordinary failed fetch and that is all it has to be.
#
# What is asserted is why the node did not make the call, read from the node's
# own line rather than from the message the handler caught. Without it this
# check passed on a destination that was merely unreachable - a name that does
# not resolve, a port nothing listens on - because from inside the isolate
# those are the same answer, and the sentence it printed about the policy was
# this script's opinion rather than something anybody had observed.
check_app_fetch_refused() {
  local title="a handler's fetch to deis-store is refused"
  # The address carries a value this run invented, which is the whole of what
  # makes the node's line about it evidence rather than wording. The egress
  # policy judges scheme, host and port, so a query string cannot change the
  # verdict; `fetch_policy::loggable` strips credentials and keeps everything
  # else, so the node writes the nonce back out in the line below.
  #
  # Without it this check read `grep "Refused an outbound fetch from demo.production to
  # http://127.0.0.1:8087/:"`, which is a *wording* match on a file anything
  # that can reach this node's unauthenticated routes can append to. One
  # `POST /api/enqueue` whose job key is that sentence put a line in the file
  # that satisfied it, and the check reported an egress policy it had not
  # seen. A nonce cannot be echoed by someone who has not read it.
  local probe="fetch-refused-$$-${RANDOM}"
  local target="http://127.0.0.1:${DEIS_STORE_PORT}/?probe=${probe}"
  local mark
  mark=$(node_log_mark)
  local body
  body=$(app_fetch fetch-refused "$target")
  answered_json "$title" fetch-refused "$body" || return 0

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

  # The node's own account of the decision, naming this destination and the
  # rule that stopped it. A DNS failure or a dead port produces a line here
  # too, and it says something else - which is the whole reason to read it.
  # Two narrowings, not one. `-F` because the address is matched as the text
  # it is rather than as a pattern whose dots match anything, and the window
  # because a line that was already in the file when this check took its mark
  # is not a line about the call this check just made.
  local refused_line
  refused_line=$(node_log_since "$mark" 2>/dev/null \
    | grep -m 1 -F "Refused an outbound fetch from ${APP_ID}.${ENV_NAME} to ${target}:" || true)
  case "$refused_line" in
    *"the platform's own service endpoints"*) ;;
    *)
      fail_check "$title" \
        "the call was not made (${error}), but the node's line for ${target} does not say the egress policy refused it as one of the platform's own service endpoints: \"$(printf '%s' "${refused_line:-never written}" | log_message | head -c 200)\". An address that is merely unreachable answers the handler exactly the same way, so without that line this check would report a policy it never saw"
      return
      ;;
  esac

  pass_check "$title" \
    "the handler asked for ${target} and the platform did not make the call: ${error}. The node says why, at warn, in a line naming the probe=${probe} this run invented for this one call and written after this check began: \"$(printf '%s' "$refused_line" | log_message | head -c 200)\" - so what stopped it is the policy, not the network it sits on with the origin this same handler reaches freely"
}

# The security half of what a redirect chain costs.
#
# check_a_redirect_chain_costs_a_subrequest_per_hop watches the accounting: one
# fetch() of a URL that redirects N times spends N+1 of the invocation's
# outbound calls. Every hop of that chain lands back on the demo's own origin,
# which the node allows - so a node that judged the URL the application named
# and then followed the redirects with no further questions passes it exactly
# as a node that judges every hop does. The accounting was witnessed; the
# policy was not.
#
# This is the case that tells them apart, and it is the one an attacker
# reaches for. The application names one address, and it is an address it is
# allowed to name. The origin answers 302 to the control plane - an address no
# application on this node may call. If a hop is not judged, that call is
# made, and a handler has reached the platform's own API by asking a server it
# is allowed to talk to for directions.
#
# The pair is what makes it evidence. The same handler, the same origin and
# the same redirect route, sent once to a hop the policy refuses and once to a
# hop it allows: one is stopped naming the hop's own address, the other
# arrives with the origin's body. So what stopped the first is where the hop
# pointed, and not the redirect.
check_a_redirect_hop_to_a_refused_host_is_stopped() {
  local title="the outbound policy judges every hop of a redirect, not just the address the handler named"

  if [ -z "${DEIS_ORIGIN_PORT:-}" ]; then
    fail_check "$title" \
      "this run's state names no fetch origin, so there is no allowed address to be redirected away from. Restart it: ./scripts/demo.sh stop && ./scripts/demo.sh start"
    return
  fi

  # Where the hop lands. The control plane rather than deis-store, so that
  # this check and check_app_fetch_refused leave one node log line each and
  # neither can be read as the other's.
  local refused_hop="http://127.0.0.1:${DEIS_CONTROL_PORT}/api/v1/apps"
  local refused_encoded="http%3A%2F%2F127.0.0.1%3A${DEIS_CONTROL_PORT}%2Fapi%2Fv1%2Fapps"
  local allowed_encoded="http%3A%2F%2F127.0.0.1%3A${DEIS_ORIGIN_PORT}%2Fpayload.txt"
  local origin="http://127.0.0.1:${DEIS_ORIGIN_PORT}/redirect-to?url="

  # Read out of the node's configuration rather than asserted. The evidence
  # this check prints used to say the origin was "the single entry in this
  # node's [fetch] allow", which nothing here had looked at: an allow list
  # with the refused address in it would have produced that sentence word for
  # word. Measured - the control plane's own address added to the list, the
  # node restarted - the hop was still refused, because it is refused as a
  # platform service endpoint whatever the allow list says, and the check
  # went on printing a claim about a file it had never opened. It prints what
  # the file says now.
  local allow_list
  allow_list=$(sed -n 's/^allow = //p' "${DEIS_RUNTIME_CONFIG}" | head -n 1)

  # Before the refused hop, so the node's line about it is read out of what
  # this pair of calls added rather than out of the whole file. There is no
  # nonce to use here: the address is the control plane's own, and it is
  # named that way on purpose so that this check and check_app_fetch_refused
  # leave one line each. Putting a query on it would work the same way it does
  # there, but the address travels through the origin's /redirect-to as an
  # encoded parameter and comes back as a Location header, and this check
  # cannot verify what that round trip does to it.
  local mark
  mark=$(node_log_mark)

  local refused_body allowed_body
  refused_body=$(app_fetch redirect-hop-refused "${origin}${refused_encoded}")
  answered_json "$title" redirect-hop-refused "$refused_body" || return 0
  allowed_body=$(app_fetch redirect-hop-allowed "${origin}${allowed_encoded}")
  answered_json "$title" redirect-hop-allowed "$allowed_body" || return 0

  local refused_reached refused_error allowed_reached allowed_status allowed_text
  refused_reached=$(echo "$refused_body" | jq -r '.reached // false' 2>/dev/null || true)
  refused_error=$(echo "$refused_body" | jq -r '.error // empty' 2>/dev/null || true)
  allowed_reached=$(echo "$allowed_body" | jq -r '.reached // false' 2>/dev/null || true)
  allowed_status=$(echo "$allowed_body" | jq -r '.status // empty' 2>/dev/null || true)
  allowed_text=$(echo "$allowed_body" | jq -r '.body // empty' 2>/dev/null || true)

  # The control first. If the redirect route itself does not work, the
  # refusal above is a broken fixture and not a policy decision, and saying so
  # here is what keeps the two apart.
  case "$allowed_text" in
    *"$ORIGIN_BODY"*) ;;
    *)
      fail_check "$title" \
        "the same handler, the same origin and the same /redirect-to route sent to a hop the node allows did not arrive: reached=${allowed_reached}, HTTP ${allowed_status:-none}, body $(printf '%s' "$allowed_text" | tr '\n' ' ' | head -c 160). Until that works, a refusal on the other hop says nothing about the policy"
      return
      ;;
  esac

  if [ "$refused_reached" = "true" ]; then
    fail_check "$title" \
      "the handler fetched an address the node allows, the origin answered 302 to ${refused_hop}, and the node followed it: a handler can reach anything on this machine through an upstream willing to redirect, which is every egress rule on this node undone by one Location header"
    return
  fi

  # The refusal has to name where the *hop* pointed. Naming the URL the
  # handler passed would mean the first call was refused - which would be a
  # different platform and a much smaller claim.
  case "$refused_error" in
    *"${refused_hop}"*) ;;
    *)
      fail_check "$title" \
        "the call did not complete, but the refusal does not name ${refused_hop}, so it is not the hop that was judged: $(printf '%s' "${refused_error:-<none>}" | head -c 240)"
      return
      ;;
  esac
  case "$refused_error" in
    *"the platform's own service endpoints"*) ;;
    *)
      fail_check "$title" \
        "the hop to ${refused_hop} did not complete, but not because the egress policy refused it: $(printf '%s' "$refused_error" | head -c 240). A hop that merely failed to connect would answer the handler the same way"
      return
      ;;
  esac

  # And the node's own line about it, which is where an operator watching for
  # applications probing the platform sees this happen.
  local logged
  logged=$(node_log_since "$mark" 2>/dev/null \
    | grep -m 1 -F "Refused an outbound fetch from ${APP_ID}.${ENV_NAME} to ${refused_hop}:" || true)
  if [ -z "$logged" ]; then
    fail_check "$title" \
      "the hop was refused to the handler, but the node wrote no line naming ${refused_hop} as a destination it refused, so nothing an operator reads records that an application was redirected onto the platform's own API"
    return
  fi

  pass_check "$title" \
    "the handler called one address, ${origin}<...>, and this node's [fetch] allow is ${allow_list:-<unreadable>}. Pointed at ${refused_hop} the origin's 302 was refused where it landed - \"$(printf '%s' "$refused_error" | head -c 200)\" - and the node recorded it at warn: \"$(printf '%s' "$logged" | log_message | head -c 160)\". Pointed at /payload.txt instead, the identical call followed the hop and came back with the origin's own body (HTTP ${allowed_status}, \"${ORIGIN_BODY}\"), so it is where the hop went that decided it"
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
  answered_json "$title" crypto "$body" || return 0

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

# ----------------------------------------------------------------------
# Two callers, one address
# ----------------------------------------------------------------------
#
# Everything else in this file drives one deployed application through one
# node as a single caller, and asks what that caller was given. A whole class
# of defect is invisible to that shape of question, because it needs two
# callers to appear at all: what one caller is handed that belonged to
# another. `verify` reported "90 passed, 0 failed" on a build whose cache was
# keyed by the address alone, so the answer to one user's `POST /orders` was
# handed to the next anonymous `GET /orders` - a live cross-caller disclosure,
# every check green, and the person who found it was a person.
#
# So there is a second caller here. The two requests differ only in the
# credential they carry, they ask for the same address, and the check compares
# the two answers. That comparison is the whole mechanism, and it is not
# specific to a cache: it is what any check of a per-caller boundary looks
# like.
#
# The address they ask for is a route in the example app that follows the
# pattern the Cache API is written for. It is deliberately not a route that
# knows it is being tested: a handler with a special case for the second
# caller would be proving something about itself.

# The route both callers ask for. Named once so the handler in the example
# app's sources and the two checks below cannot drift apart.
PRIVATE_PAGE_ROUTE="/api/private-page"

# One caller's request for it. An empty credential means an anonymous caller,
# which is a caller like any other and the one every mistaken variant matches.
private_page_request() {
  local headers=$1
  local mode=$2
  local credential=$3

  if [ -n "$credential" ]; then
    curl -s -D "$headers" -H "x-deis-app-id: ${APP_ID}" \
      -H "authorization: ${credential}" \
      "http://127.0.0.1:${DEIS_RUNTIME_PORT}${PRIVATE_PAGE_ROUTE}?mode=${mode}" || true
  else
    curl -s -D "$headers" -H "x-deis-app-id: ${APP_ID}" \
      "http://127.0.0.1:${DEIS_RUNTIME_PORT}${PRIVATE_PAGE_ROUTE}?mode=${mode}" || true
  fi
}

# One field of one answer, or nothing when the field is absent or the answer
# was not JSON at all.
#
# Not `jq -r '.field // empty'`, which is the idiom used everywhere else in
# this file: `//` in jq treats `false` as absent, so `.stored // empty`
# answers nothing for `"stored": false` - and `false` is the value the second
# check below exists to see. Measured: the check reported `stored=<none>` and
# failed against a platform that had refused the put exactly as it should.
private_page_field() {
  printf '%s' "$1" | jq -r "if $2 == null then empty else $2 end" 2>/dev/null || true
}

check_a_second_caller_is_not_handed_the_first_ones_answer() {
  local title="a second caller asking for the same address is not handed the first caller's answer"
  local headers="${LOG_DIR}/verify-private-page.headers"
  # Credentials this run invents. Nothing an earlier run, or anybody else on
  # this node, can have left in the store reads as either caller's answer.
  local first="Bearer first-$$-${RANDOM}"
  local second="Bearer second-$$-${RANDOM}"

  local one two three four
  one=$(private_page_request "$headers" vary "$first")
  if [ "$(http_status "$headers")" != "200" ]; then
    fail_check "$title" \
      "GET ${PRIVATE_PAGE_ROUTE} answered $(http_status "$headers") for the first caller: $(printf '%s' "$one" | head -c 200)"
    return
  fi
  # The same caller again. This is what makes the rest of the check mean
  # something: a platform with no cache at all would hand nobody anybody
  # else's page, and would pass every "no leak" assertion below while giving
  # the application none of what it asked for.
  two=$(private_page_request "$headers" vary "$first")
  # A caller with no credential, asking for the same address.
  three=$(private_page_request "$headers" vary "")
  # And a third caller, with a credential of its own, after the anonymous one.
  four=$(private_page_request "$headers" vary "$second")

  local stored_one hit_two served_two served_three served_four
  local scope_one scope_two
  stored_one=$(private_page_field "$one" '.stored')
  scope_one=$(private_page_field "$one" '.scopeId')
  hit_two=$(private_page_field "$two" '.hit')
  served_two=$(private_page_field "$two" '.served')
  scope_two=$(private_page_field "$two" '.scopeId')
  served_three=$(private_page_field "$three" '.served')
  served_four=$(private_page_field "$four" '.served')

  # A cache that lost its store between the two requests would report a miss
  # for the second, and a miss proves nothing about what the store would have
  # handed back. Said separately so a failure names that rather than blaming
  # the caching rules.
  if [ -n "$scope_one" ] && [ "$scope_one" != "$scope_two" ]; then
    fail_check "$title" \
      "the isolate's cache store changed between the first caller's two requests (${scope_one} then ${scope_two}), so nothing here was served out of the store the first request wrote to and this check proves nothing either way"
    return
  fi

  if [ "$stored_one" = "true" ] && [ "$hit_two" = "true" ] \
    && [ "$served_two" = "page-for-${first}" ] \
    && [ "$served_three" = "page-for-anonymous" ] \
    && [ "$served_four" = "page-for-${second}" ]; then
    pass_check "$title" \
      "the first caller's page was stored under its credential and served back to it out of the cache (store ${scope_one}); the anonymous caller and a second credentialed caller asked the same address next and were each answered their own page - \"${served_three}\" and \"page-for-${second}\" - rather than \"page-for-${first}\""
    return
  fi

  local leaked=""
  if [ "$served_three" = "page-for-${first}" ]; then
    leaked=" The anonymous caller was handed the first caller's page."
  fi
  if [ "$served_four" = "page-for-${first}" ] || [ "$served_four" = "page-for-anonymous" ]; then
    leaked="${leaked} The second credentialed caller was handed \"${served_four}\", which is not its own."
  fi
  fail_check "$title" \
    "expected the first caller's put to be accepted (got stored=${stored_one:-<none>}), its second request to be a hit for \"page-for-${first}\" (got hit=${hit_two:-<none>} serving \"${served_two:-<none>}\"), the anonymous caller to be answered \"page-for-anonymous\" (got \"${served_three:-<none>}\") and the second credentialed caller \"page-for-${second}\" (got \"${served_four:-<none>}\").${leaked}"
}

check_a_credentialed_answer_that_records_no_owner_is_not_stored() {
  local title="a credentialed answer that records nothing about whose it is is refused by the cache, not stored"
  local headers="${LOG_DIR}/verify-private-page-bare.headers"
  local credential="Bearer bare-$$-${RANDOM}"

  # The same two callers, against a handler that caches a credentialed answer
  # while saying nothing about whose it is. RFC 9111 section 3.5: a shared
  # cache may not reuse such a response. The platform is what has to refuse
  # it, because the handler here does not know it is doing anything wrong -
  # which is the point, since neither does the handler in a real application.
  local one two
  one=$(private_page_request "$headers" bare "$credential")
  if [ "$(http_status "$headers")" != "200" ]; then
    fail_check "$title" \
      "GET ${PRIVATE_PAGE_ROUTE}?mode=bare answered $(http_status "$headers"): $(printf '%s' "$one" | head -c 200)"
    return
  fi
  two=$(private_page_request "$headers" bare "")

  local stored refused served_two
  stored=$(private_page_field "$one" '.stored')
  refused=$(private_page_field "$one" '.refused')
  served_two=$(private_page_field "$two" '.served')

  # Two things, and both are needed. That the anonymous caller was not handed
  # the credentialed page is the property; that the platform said why is what
  # keeps a handler from believing it has a cache when it has none.
  if [ "$stored" = "false" ] \
    && printf '%s' "$refused" | grep -qi "authorization" \
    && [ "$served_two" = "page-for-anonymous" ]; then
    pass_check "$title" \
      "the put was refused and named the reason - \"$(printf '%s' "$refused" | head -c 220)\" - and the anonymous caller that asked next was answered its own page rather than \"page-for-${credential}\""
  else
    fail_check "$title" \
      "expected the cache to refuse the put and name Authorization (got stored=${stored:-<none>}, refused=\"${refused:-<none>}\") and the next anonymous caller to be answered \"page-for-anonymous\" (got \"${served_two:-<none>}\")"
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
#
# Named for what it does rather than for the certificate checks it was written
# for: the memory cut-off check waits on a metrics sample with it too.
poll_pause() {
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
    poll_pause
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

  # What the authority actually fetched from this proxy, and for which names.
  # The evidence line has always ended by saying the authority validated these
  # names over http-01, and nothing checked it: a stub that issued on request
  # without ever calling back would have left this list empty and the sentence
  # standing.
  local validated
  validated=$(acme_ca_stats | jq -r '[.validations[] | select(.matched) | .name] | sort | join(", ")')
  for name in "$ACME_PRODUCTION_NAME" "$ACME_PREVIEW_NAME"; do
    case ", ${validated}," in
      *", ${name},"*) ;;
      *)
        fail_check "$claim" \
          "the authority issued for [${issued}], but its record of the challenges it fetched back from this proxy does not name ${name}: [${validated:-none}]. A certificate nobody was challenged for says nothing about whether this proxy can answer for the name"
        return
        ;;
    esac
  done

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
    "https://${ACME_PRODUCTION_NAME}:${DEIS_PROXY_TLS_PORT}/" 2>"${LOG_DIR}/verify-acme-tls.log" || true)

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
    "http://127.0.0.1:${DEIS_PROXY_PORT}/.well-known/acme-challenge/never-published-token" || true)
  if [ "$unknown" != "404" ]; then
    fail_check "$claim" \
      "a token nothing published was answered ${unknown} rather than 404; an authority given anything else could believe a validation had succeeded"
    return
  fi

  # The token is a path segment chosen by whoever sends the request, and it
  # is used to name a file. Neither of these may read one.
  traversal=$(curl -s -m 5 --path-as-is --noproxy '*' \
    -H "Host: ${ACME_PRODUCTION_NAME}" \
    "http://127.0.0.1:${DEIS_PROXY_PORT}/.well-known/acme-challenge/../../../etc/passwd" | head -c 200 || true)
  encoded=$(curl -s -m 5 --path-as-is --noproxy '*' \
    -H "Host: ${ACME_PRODUCTION_NAME}" \
    "http://127.0.0.1:${DEIS_PROXY_PORT}/.well-known/acme-challenge/..%2f..%2f..%2fetc%2fpasswd" | head -c 200 || true)

  case "${traversal}${encoded}" in
    *root:*)
      fail_check "$claim" "a challenge path walked out of the challenge directory and read a file: ${traversal} ${encoded}"
      return
      ;;
  esac

  # The positive, without which the two refusals above prove nothing: a proxy
  # that answered 404 to every challenge path in existence would satisfy both
  # of them. This number was already printed in the evidence and never
  # asserted, which is a sentence about a real challenge answer resting on a
  # count that was allowed to be zero.
  # `|| true` and not `|| echo 0`: grep prints its own zero before exiting
  # non-zero, so the second form makes the value two lines long on exactly
  # the count this assertion is about.
  local served
  served=$(grep -a -c "an http-01 challenge answer was served" "${LOG_DIR}/deis_proxy_error.log" 2>/dev/null || true)
  if [ "${served:-0}" -lt 1 ]; then
    fail_check "$claim" \
      "the proxy answered 404 to a token nothing published and served no file for a traversal, but it has never served a real challenge answer either (${served} in ${LOG_DIR}/deis_proxy_error.log). A proxy that refused every challenge path would pass both halves above, so with nothing served there is no evidence the path answers what was published"
    return
  fi

  pass_check "$claim" \
    "the proxy served ${served} real challenge answer(s) to the authority, answered 404 to a token nothing published, and returned no file for ../../../etc/passwd in either its plain or its percent-encoded form"
}

# How many renewal passes this proxy has finished. Zero is a real answer here
# - it is what a proxy that has never got that far reports - so the count has
# to survive being zero, which `grep -c ... || echo 0` does not: grep prints
# its own zero first and the value becomes two lines.
acme_pass_count() {
  local counted
  counted=$(grep -a -c "automatic certificate pass finished" \
    "${LOG_DIR}/deis_proxy_error.log" 2>/dev/null || true)
  echo "${counted:-0}"
}

check_acme_does_not_reissue_a_current_certificate() {
  local claim="a certificate that is still current is not ordered again"
  local before after passes

  before=$(acme_ca_stats | jq -r '.issued | length')
  # Wait for the renewal loop to run several more passes. The condition is
  # the proxy's own count of finished passes, read from its log, not a
  # duration: check_interval_secs is 2 in this configuration.
  local start_passes attempts
  start_passes=$(acme_pass_count)
  passes=$start_passes
  attempts=30
  while [ "$attempts" -gt 0 ]; do
    passes=$(acme_pass_count)
    if [ "$((passes - start_passes))" -ge 3 ]; then
      break
    fi
    attempts=$((attempts - 1))
    poll_pause
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

[machine_identity]
control_endpoint = "https://127.0.0.1:${DEIS_CONTROL_MACHINE_PORT}"
certificate = "${PKI_DIR}/proxy-restart.crt"
private_key = "${PKI_DIR}/proxy-restart.key"
trust_root = "${PKI_DIR}/ca.crt"
trust_domain = "${FLEET_TRUST_DOMAIN}"

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
  # The same mode as every other generated file that carries a credential:
  # this one names the private key behind this proxy's own certificate under
  # [machine_identity], and docs/trust-boundaries.md says the demo writes
  # every such file 0600. This one was the exception
  # until a round read the directory listing against that sentence, and it
  # stays behind after the check (only the db and pid file are removed), so
  # the mode is for the life of .demo/ and not for the seconds the process
  # runs. scripts/test-demo-witnesses.sh holds every credential-bearing
  # writer in this file to it.
  chmod 600 "$config"

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
    poll_pause
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

# ----------------------------------------------------------------------
# The compiler
# ----------------------------------------------------------------------
#
# The README says one project builds for four hosts. The demo built one of
# them: `deis deploy` compiles for `deislet` because that is the only vendor
# the example app enables, and nothing here had ever run the other three
# adapters at all. Three of the four claims in that sentence were carried by
# the sentence.
#
# This builds the portable fixture for all four and looks at what came out. It
# deploys nothing: three of these artifacts are for platforms that are not
# running anywhere near this machine, so what can be checked is that each one
# exists, carries this application rather than a stub, and is accompanied by
# the file that host needs to accept it - `wrangler.toml` for Cloudflare,
# `deno.json` for Deno Deploy, `edgeone.json` for EdgeOne. Without that last
# part the check would pass on four copies of one bundle in four directories.
#
# `deis build` is run directly rather than through `deis_cli`: a build talks to
# nothing, and handing it the demo's control-plane endpoint and credential
# would suggest otherwise.

# One file out of a build, checked for existence and for carrying the fixture's
# marker. Echoes what is wrong, or nothing when it is right.
portable_artifact_fault() {
  local file=$1
  local what=$2
  if [ ! -f "$file" ]; then
    echo "${what} is missing (${file})"
    return
  fi
  if ! grep -q "$PORTABLE_MARKER" "$file"; then
    echo "${what} does not carry the handler's marker, so it is not this application"
  fi
}

# A provider payload is immutable and named by its complete file-set digest.
# This demo builds each fresh fixture once, so exactly one child is expected.
provider_payload_dir() {
  local project=$1
  local vendor=$2
  local found=""
  local candidate
  for candidate in "${project}/dist/providers/${vendor}/"*; do
    [ -d "$candidate" ] || continue
    if [ -n "$found" ]; then
      return 1
    fi
    found=$candidate
  done
  [ -n "$found" ] || return 1
  echo "$found"
}

# What the build's own log says was verified against a vendor's tool and what
# was not, one clause per vendor target, for the evidence line below.
#
# `deis build -v all` runs `wrangler deploy --dry-run` and `deno check` when
# those tools are on PATH and says at ⚠ when they are not; the tencent target
# has no local dry-run at all and says so every time. The files and markers
# the check asserts on are the same either way, so a ✓ that did not quote
# this read identically on a machine with both tools, on one with neither,
# and on ubuntu-latest, which has neither - the local run silently carried a
# wrangler acceptance that CI can never reproduce, and nothing said which of
# the two runs a reader was looking at. A target the log says nothing about,
# neither verified nor skipped, is reported as such and the check treats it
# as a fault: it means the build stopped saying, and a check that fills the
# silence in would be back where it started.
vendor_verification_summary() {
  local log=$1
  local cloudflare deno tencent
  if grep -q "wrangler accepted the Worker" "$log" 2>/dev/null; then
    cloudflare="cloudflare verified (wrangler deploy --dry-run accepted the Worker)"
  elif grep -q "the cloudflare target was not verified" "$log" 2>/dev/null; then
    cloudflare="cloudflare NOT verified (wrangler is not on PATH)"
  else
    cloudflare="cloudflare: the build log says nothing about verification"
  fi
  if grep -q "deno check accepted the program" "$log" 2>/dev/null; then
    deno="deno verified (deno check accepted the program)"
  elif grep -q "the deno target was not verified" "$log" 2>/dev/null; then
    deno="deno NOT verified (deno is not on PATH)"
  else
    deno="deno: the build log says nothing about verification"
  fi
  if grep -q "the tencent target was not verified against EdgeOne" "$log" 2>/dev/null; then
    tencent="tencent NOT verified (EdgeOne has no local dry-run)"
  else
    tencent="tencent: the build log says nothing about verification"
  fi
  echo "${cloudflare}; ${deno}; ${tencent}"
}

check_every_compiler_target_is_built() {
  local title="one project compiles for all four targets, each with the files that host needs"
  local dir="${DEMO_DIR}/app-portable"
  local log="${LOG_DIR}/cli-build-portable.log"

  if [ ! -f "${PORTABLE_SRC_DIR}/.config.json" ]; then
    fail_check "$title" "no portable fixture at ${PORTABLE_SRC_DIR}"
    return
  fi
  if ! command -v python3 >/dev/null 2>&1; then
    fail_check "$title" "python3 is needed to look inside the deislet bundle"
    return
  fi

  # A fresh copy every time: a build writes dist/ into the project it is
  # given, and artifacts left by a previous run would be indistinguishable
  # from artifacts this one produced.
  rm -rf "$dir"
  mkdir -p "$dir"
  cp -R "${PORTABLE_SRC_DIR}/." "$dir/"

  if ! ( cd "$dir" && "${BIN_DIR}/deis" build -v all ) > "$log" 2>&1; then
    fail_check "$title" \
      "deis build -v all failed: $(tail -n 6 "$log" | tr '\n' ' ' | head -c 300)"
    return
  fi

  # What each vendor has to have produced. The entry file is the application;
  # the second file is what makes the directory deployable to that host and to
  # no other.
  local cloudflare deno tencent deislet
  cloudflare=$(provider_payload_dir "$dir" cloudflare || true)
  deno=$(provider_payload_dir "$dir" deno || true)
  tencent=$(provider_payload_dir "$dir" tencent || true)
  deislet=$(provider_payload_dir "$dir" deislet || true)
  local faults=""
  [ -n "$cloudflare" ] || faults="${faults}Cloudflare payload identity was not published "
  [ -n "$deno" ] || faults="${faults}Deno payload identity was not published "
  [ -n "$tencent" ] || faults="${faults}EdgeOne payload identity was not published "
  [ -n "$deislet" ] || faults="${faults}Deislet payload identity was not published "
  faults="${faults}$(portable_artifact_fault "${cloudflare}/index.js" "Cloudflare's index.js") "
  faults="${faults}$(portable_artifact_fault "${deno}/project/functions/[[all]].js" "Deno's canonical module") "
  faults="${faults}$(portable_artifact_fault "${tencent}/edge-functions/index.js" "EdgeOne's index.js") "

  local file
  for file in "${cloudflare}/wrangler.toml" "${cloudflare}/package.json" \
    "${deno}/deno.json" "${deno}/_deis_internal/router.ts" \
    "${tencent}/edgeone.json" "${deislet}/bundle.zip"; do
    [ -f "$file" ] || faults="${faults}${file} was not written "
  done

  # The deislet bundle is a zip, so its entry is read out of the archive
  # rather than grepped through the compression.
  local bundled=""
  if [ -f "${deislet}/bundle.zip" ]; then
    bundled=$(python3 - "${deislet}/bundle.zip" "$PORTABLE_MARKER" <<'BUNDLE'
import sys, zipfile
with zipfile.ZipFile(sys.argv[1]) as archive:
    names = archive.namelist()
    worker = archive.read("worker/index.js").decode("utf-8", "replace")
print("ok" if sys.argv[2] in worker and "manifest.json" in names else "")
BUNDLE
)
    [ "$bundled" = "ok" ] || faults="${faults}the deislet bundle has no manifest.json, or its worker is not this application "
  fi

  # And that the three JavaScript entries are three files rather than one
  # copied three times: each adapter builds the context its own host hands a
  # handler, so two vendors producing byte-identical output would mean one of
  # them was never really targeted.
  local cf_sum eo_sum
  cf_sum=$(openssl dgst -sha256 "${cloudflare}/index.js" 2>/dev/null | awk '{print $NF}')
  eo_sum=$(openssl dgst -sha256 "${tencent}/edge-functions/index.js" 2>/dev/null | awk '{print $NF}')
  if [ -n "$cf_sum" ] && [ "$cf_sum" = "$eo_sum" ]; then
    faults="${faults}Cloudflare's and EdgeOne's entry files are byte-identical, so one adapter emitted the other's output "
  fi

  # The vendor's own configuration has to be about this project.
  grep -q '^name = "portable"' "${cloudflare}/wrangler.toml" 2>/dev/null \
    || faults="${faults}wrangler.toml does not name the project "
  grep -q '"portable"' "${tencent}/edgeone.json" 2>/dev/null \
    || faults="${faults}edgeone.json does not name the project "
  grep -q '"/\*"' "${deno}/_deis_internal/router.ts" 2>/dev/null \
    || faults="${faults}Deno's provider router does not invoke the canonical module through its catch-all "

  # Which vendor tools stood behind this run, from the build's own log.
  local verified
  verified=$(vendor_verification_summary "$log")
  case "$verified" in
    *"says nothing about verification"*)
      faults="${faults}the build log no longer says, per vendor target, whether the vendor's tool verified the artifact or was skipped (${verified}) " ;;
  esac

  faults=$(echo "$faults" | sed 's/^ *//; s/ *$//')
  if [ -n "$faults" ]; then
    fail_check "$title" \
      "deis build -v all reported success, but: ${faults}. The build said: $(tail -n 5 "$log" | tr '\n' ' ' | head -c 200)"
    return
  fi

  pass_check "$title" \
    "one \`deis build -v all\` over ${PORTABLE_SRC_DIR} published one canonical parent and four immutable payload directories under dist/providers/<vendor>/<digest>. Deislet carries bundle.zip, Cloudflare carries index.js plus the wrangler.toml naming \"portable\", Deno carries the reconstructed canonical module and its generated catch-all router, and EdgeOne carries edge-functions/index.js plus edgeone.json. Every entry carries the handler's own marker, and Cloudflare's and EdgeOne's differ from one another, so each packager built the shape its host expects from the same parent rather than copying one output four times. The files are this compiler's own work; what a vendor's tool said about them on this machine, from the build log: ${verified}. A target marked NOT verified was verified nowhere in this run"
}

# The application the check below tries, and fails, to deploy. A name of its
# own so that the refusal is filed under nothing the rest of the run reads.
WEBSOCKET_APP_ID="${WEBSOCKET_APP_ID:-demo-websocket}"

# Lay down a project whose one handler is the WebSocket echo endpoint this
# demo used to deploy, and echo the directory.
#
# Until the standard was locked the handler below was written into the staged
# copy of the example app, and two checks drove a real RFC 6455 handshake
# through deis-proxy and echoed a text frame and a binary frame over it. Edge
# Canon v1 defines no WebSocket application API - no WebSocketPair, no
# WebSocketServer, no 101 with a socket attached - because only some of the
# providers it covers have one, and a handler that runs on those alone is not
# portable. So what the platform promises about this handler changed: not
# that it works, but that it is refused by the build, which is the first step
# of `deis deploy`, before an application, an environment or a version exists
# for it. The handler is staged in a project of its own so that the refusal
# cannot cost the example app its deployment, and the project is otherwise
# the smallest one the compiler accepts, so that nothing else in it can be
# what was refused.
prepare_websocket_source() {
  local dest="${DEMO_DIR}/app-websocket"

  rm -rf "$dest"
  mkdir -p "${dest}/functions/api"

  cat > "${dest}/.config.json" <<JSON
{
  "version": "1.0.0",
  "name": "${WEBSOCKET_APP_ID}",
  "runtime": "standard-v1",
  "language": "typescript",
  "functionRoot": "./functions",
  "compatibilityDate": "2026-09-01",
  "vendors": { "deislet": { "enabled": true } },
  "build": { "outDir": "./dist" }
}
JSON

  cat > "${dest}/functions/api/ws.ts" <<'TS'
// A WebSocket echo endpoint in the shape some providers describe: make a
// WebSocketPair, accept the server half, and answer 101 with the client half
// attached to the Response. Edge Canon v1 has no portable form of any of it,
// so the build refuses this file at the reference to WebSocketPair.
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
    server.send("text:" + String(event.data));
  });

  return new Response(null, { status: 101, webSocket: client });
}
TS

  echo "$dest"
}

check_a_websocket_handler_is_refused_by_the_build() {
  local title="a handler that reaches for WebSocketPair is refused by the build, before anything is deployed"
  local json="${LOG_DIR}/cli-deploy-websocket.json"
  local errors="${LOG_DIR}/cli-deploy-websocket.log"

  local dir
  if ! dir=$(prepare_websocket_source); then
    fail_check "$title" "the project could not be staged under ${DEMO_DIR}"
    return
  fi

  # Through `deis deploy` and not `deis build`, because the claim is about
  # what a person who tried to ship this would meet: the deploy's first step
  # is the build, and a deploy that is refused there has nothing left to do.
  # `--app` for the reason the callee's deploy spells it: DEIS_APP in the
  # CLI's environment names the example app.
  local status=0
  (cd "$dir" && deis_cli deploy -e "$ENV_NAME" --app "$WEBSOCKET_APP_ID" --json) \
    > "$json" 2> "$errors" || status=$?

  local message
  message=$(grep -a -m 1 -o 'EC_STREAM_WEBSOCKET_NONPORTABLE.*' "$errors" 2>/dev/null || true)

  # What was left behind, on both sides: a bundle under the project, or an
  # application in the catalog. The list is walked whole, so an application
  # on a later page could not hide from this.
  local built="no"
  [ -e "${dir}/dist" ] && built="yes"
  local listed
  listed=$(control_list_all /api/v1/apps \
    | jq -r --arg id "$WEBSOCKET_APP_ID" '[.[] | select(.app_id == $id)] | length' 2>/dev/null || true)

  if [ "$status" -ne 0 ] && echo "$message" | grep -q "WebSocketPair" \
    && [ "$built" = "no" ] && [ "${listed:-x}" = "0" ]; then
    pass_check "$title" \
      "deis deploy --app ${WEBSOCKET_APP_ID} exited ${status} at its first step, the build: \"$(echo "$message" | cut -c1-200)\". No dist/ was written under the project and the control plane lists no application called ${WEBSOCKET_APP_ID}. This handler used to be deployed by this demo and driven through deis-proxy with a real handshake; the locked standard removed the server-side WebSocket API rather than leave it to the providers that have one, so what an application is promised is this refusal, and not a 101"
  else
    fail_check "$title" \
      "expected a non-zero exit naming EC_STREAM_WEBSOCKET_NONPORTABLE and WebSocketPair, no dist/ and no application in the catalog; got exit ${status}, message '${message:-<none>}', dist written: ${built}, applications called ${WEBSOCKET_APP_ID}: ${listed:-<unreadable>}. The CLI said: $(tail -n 3 "$errors" | tr '\n' ' ' | head -c 300)"
  fi
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
  #
  # Its exit status is judged rather than discarded, and its output has to
  # name the secret before the sweep below means anything. Both were missing:
  # a command that failed wrote an error message into this file, an error
  # message contains no secret, and the check went on to report that "neither
  # the JSON nor the human output contains the value" about an output that
  # was never produced.
  if ! deis_cli secret list -e "$ENV_NAME" > "$text" 2>&1; then
    fail_check "$title" \
      "the --json form worked but the human form failed, so the listing a person actually reads was never produced: $(tail -n 3 "$text" | tr '\n' ' ')"
    return
  fi

  local names
  names=$(jq -r '.secrets[].name' < "$json" 2>/dev/null | tr '\n' ' ' || true)

  if ! echo "$names" | grep -q "DEMO_SECRET"; then
    fail_check "$title" "DEMO_SECRET is not listed; names were: ${names:-none}"
    return
  fi
  if ! grep -q "DEMO_SECRET" "$text"; then
    fail_check "$title" \
      "the human listing does not name DEMO_SECRET at all, so finding no value in it says nothing: $(head -c 200 "$text" | tr '\n' ' ')"
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
    "names listed: ${names}- both forms of the command ran, both name DEMO_SECRET, and neither contains the value. There is no command that prints one: the control plane has no endpoint that returns it"
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
  # `%{http_code}` already prints 000 for a request that never got an answer,
  # so the caller's message reads "answered HTTP 000" instead of the run
  # ending on curl's exit 7. See `app_request`.
  curl "${args[@]}" || true
}

# The same, answering with the body, so a check can assert on what the caller
# was told rather than only on the number.
control_answer() {
  local method=$1 path=$2 credential=$3 body=${4:-}
  local args=(-s -X "$method" "$(control_url)${path}")
  if [ -n "$credential" ]; then
    args+=(-H "Authorization: Bearer ${credential}")
  fi
  # A body, on the same terms as `control_status`. A check that wants to read
  # the sentence a refusal carries has to send the request the caller would
  # really have sent: a write route validates its body, so a request that
  # omits it is refused for the wrong reason and the sentence under test is
  # never reached.
  if [ -n "$body" ]; then
    args+=(-H "Content-Type: application/json" --data "$body")
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

  # A deploy carries the application's configuration with it. Under the
  # locked environment standard a config value is deployment-time input: it
  # is not in the bundle, not in the canonical artifact and not kept by the
  # control plane between deploys, so every deploy request states the values
  # its declarations ask for and a request that omits a required one is
  # refused with EC_ENV_REQUIRED_MISSING before authority is ever the
  # question. The CLI reads them from the project file; this check is a
  # `curl` standing in for the CLI, so it reads them from the same place -
  # the staged project - rather than repeating them here, which would go
  # stale the first time the example app gains a variable.
  # The values of the version being re-pointed at, not of some other build.
  # `preview` runs the app-v2 project, whose BUILD_LABEL is what the earlier
  # check on two environments serving at once reads back, so sending app-v1's
  # values here would quietly relabel preview and break that claim one run
  # later.
  local config_values
  config_values=$(jq -c '.environment.variables // {}' \
    < "${DEMO_DIR}/app-v2/.config.json" 2>/dev/null || echo '{}')

  local allowed refused_env refused_secret refused_platform refused_token
  allowed=$(control_status POST \
    "/api/v1/apps/${APP_ID}/environments/${PREVIEW_ENV_NAME}/deployments" "$token" \
    "{\"version_id\":\"${preview_version}\",\"note\":\"deployed by a scoped token\",\"config_values\":${config_values}}")
  refused_env=$(control_status POST \
    "/api/v1/apps/${APP_ID}/environments/${ENV_NAME}/deployments" "$token" \
    "{\"version_id\":\"${preview_version}\",\"config_values\":${config_values}}")
  refused_secret=$(control_status PUT \
    "/api/v1/apps/${APP_ID}/environments/${PREVIEW_ENV_NAME}/secrets/CI_ATTEMPT" "$token" \
    '{"value":"this must never be stored"}')
  refused_platform=$(control_status GET "/api/v1/audit" "$token")
  refused_token=$(control_status GET "/api/v1/tokens" "$token")

  # `/api/v1` answers a refusal in the structured envelope, so the sentence
  # the caller is shown is `.error.message` - which is the legacy `error`
  # and `details` strings joined, and still the one that names the scope
  # that would have worked.
  local told
  told=$(control_answer POST \
    "/api/v1/apps/${APP_ID}/environments/${ENV_NAME}/deployments" "$token" \
    "{\"version_id\":\"${preview_version}\",\"config_values\":${config_values}}" \
    | jq -r '.error.message // empty')

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

  # 201 or 202, and the difference is not about authority. The route answers
  # 201 when the pointer was moved and the node already has the version, and
  # 202 when the delivery is being coordinated and the caller is given a
  # `Location` to poll; which one a particular deploy gets depends on whether
  # this environment's configuration snapshot is the one the node is already
  # holding, not on what the credential was allowed to do. The claim under
  # test is that this token may deploy here at all, so both answers satisfy
  # it and the sentence below says which one arrived.
  if { [ "$allowed" = "201" ] || [ "$allowed" = "202" ]; } && [ "$refused_env" = "403" ] && [ "$refused_secret" = "403" ] \
    && [ "$refused_platform" = "403" ] && [ "$refused_token" = "403" ] \
    && [ "$status_exit" = "0" ] && [ -n "$seen" ] && [ "$hidden" = "false" ] \
    && echo "$told" | grep -q "deploy:${APP_ID}/${ENV_NAME}"; then
    pass_check "$title" \
      "carrying only deploy:${APP_ID}/${PREVIEW_ENV_NAME}, the token re-deployed ${PREVIEW_ENV_NAME} to ${preview_version} (HTTP ${allowed} - accepted, whether delivered outright or handed back a Location to poll), sending the project's own configuration ${config_values} with it because a deploy carries its configuration rather than finding it stored; the same call against ${ENV_NAME} answered ${refused_env}, writing a secret on its own environment ${refused_secret}, reading the audit log ${refused_platform} and listing credentials ${refused_token}. The refusal names the scope that would have worked: \"${told}\". \`deis status\` still works for it: ${PREVIEW_ENV_NAME} shows version ${seen} and ${ENV_NAME} comes back visible=${hidden} rather than failing the command"
  else
    fail_check "$title" \
      "deploy to ${PREVIEW_ENV_NAME} answered ${allowed} (expected 201 Created or 202 Accepted); ${ENV_NAME} ${refused_env}, secret ${refused_secret}, audit ${refused_platform}, tokens ${refused_token} (expected 403 each). \`deis status\` exited ${status_exit} showing ${PREVIEW_ENV_NAME} version '${seen:-none}' and ${ENV_NAME} visible '${hidden:-none}'. The refusal said: ${told:-nothing}"
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

  local bundle
  bundle=$(jq -r '.bundle.path // empty' < "${LOG_DIR}/cli-deploy-v1.json" 2>/dev/null || true)
  if [ ! -f "$bundle" ]; then
    fail_check "$title" "the example app's bundle is not at ${bundle}, so nothing could be uploaded"
    return
  fi

  local refused told
  refused=$(curl -s -o /dev/null -w '%{http_code}' -X POST \
    -H "Authorization: Bearer ${token}" -H "Content-Type: application/zip" \
    --data-binary "@${bundle}" "$(control_url)/api/v1/apps/${APP_ID}/versions" || true)
  told=$(curl -s -X POST \
    -H "Authorization: Bearer ${token}" -H "Content-Type: application/zip" \
    --data-binary "@${bundle}" "$(control_url)/api/v1/apps/${APP_ID}/versions" \
    | jq -r '.error.message // empty' 2>/dev/null || true)

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
    --data-binary "@${bundle}" "$(control_url)/api/v1/apps/${APP_ID}/versions" || true)

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

check_a_scoped_token_cannot_unload_an_application_from_a_node() {
  local title="a token for one environment cannot take the application off a node"
  ensure_account ci deployer

  # Exactly the credential a preview pipeline holds, and the same one the
  # scoped-token check proves can deploy to ${PREVIEW_ENV_NAME} and nowhere
  # else. `DELETE /api/platform/runtime/<node>/apps/<app>` stops the
  # application on the node; without ?environment= it stops every environment
  # of it, so it is a real outage of ${ENV_NAME} made with a credential that
  # may not deploy there. It used to be allowed, because the route named no
  # environment in its path and the authorisation table asked for "any
  # environment the caller holds" - which one preview scope answers.
  #
  # Every call below is one that has to be refused, so nothing on the node is
  # actually unloaded and the checks after this one still have an application
  # to look at.
  local minted token
  minted=$(deis_cli token create --account ci \
    --scope "deploy:${APP_ID}/${PREVIEW_ENV_NAME}" unload-attempt --json 2>/dev/null || echo '{}')
  token=$(echo "$minted" | jq -r '.secret // empty')
  if [ -z "$token" ]; then
    fail_check "$title" "could not mint a scoped token: ${minted}"
    return
  fi

  local whole named other told
  whole=$(control_status DELETE \
    "/api/platform/runtime/${NODE_ID}/apps/${APP_ID}" "$token")
  named=$(control_status DELETE \
    "/api/platform/runtime/${NODE_ID}/apps/${APP_ID}?environment=${ENV_NAME}" "$token")
  other=$(control_status DELETE \
    "/api/platform/runtime/${NODE_ID}/apps/${CALLEE_APP_ID}" "$token")
  # As above: `/api/platform` refuses in the structured envelope, and the
  # sentence naming the scope that would have worked is `.error.message`.
  told=$(control_answer DELETE \
    "/api/platform/runtime/${NODE_ID}/apps/${APP_ID}" "$token" \
    | jq -r '.error.message // empty')

  # The application is still being served, which is the claim that matters:
  # a refusal that had nonetheless unloaded something would be no refusal.
  local headers="${LOG_DIR}/verify-unload-refused.headers"
  app_request "$headers" "/" > /dev/null
  local still_serving
  still_serving=$(http_status "$headers")

  local audited
  audited=$(deis_cli audit --outcome refused --json 2>/dev/null \
    | jq -r '[.entries[] | select(.action == "node.app.unload")] | length' 2>/dev/null || echo 0)

  if [ "$whole" = "403" ] && [ "$named" = "403" ] && [ "$other" = "403" ] \
    && [ "$still_serving" = "200" ] && [ "${audited:-0}" -ge 3 ] \
    && echo "$told" | grep -q "deploy:${APP_ID}/\*"; then
    pass_check "$title" \
      "carrying only deploy:${APP_ID}/${PREVIEW_ENV_NAME}, the token was refused DELETE /api/platform/runtime/${NODE_ID}/apps/${APP_ID} (HTTP ${whole}), the same call naming ${ENV_NAME} (${named}) and the one against ${CALLEE_APP_ID} (${other}). The refusal asks for the whole application - \"$(printf '%s' "$told" | head -c 120)\" - because a call that names no environment stops every one of them. ${APP_ID} is still served (HTTP ${still_serving}) and ${audited} refused node.app.unload row(s) are on the record"
  else
    fail_check "$title" \
      "expected 403 for the whole application (got ${whole}), for ${ENV_NAME} (got ${named}) and for ${CALLEE_APP_ID} (got ${other}), naming deploy:${APP_ID}/* -- it said \"$(printf '%s' "${told:-nothing}" | head -c 160)\". ${APP_ID} answered ${still_serving} (expected 200) and the audit log holds ${audited:-0} refused node.app.unload row(s)"
  fi
}

# One gRPC call at the node's own management plane, answering with the
# grpc-status the node put in its response.
#
# A refusal is a trailers-only response - HTTP 200 with the status in the
# headers and no message at all - so reading the headers is enough and nothing
# here has to decode protobuf. An empty status means the call was served: a
# served answer puts its status in the trailers instead.
node_grpc_status() {
  local path=$1 credential=$2 frame=$3
  local args=(-s -o /dev/null -D - -X POST
    -H "content-type: application/grpc" -H "te: trailers"
    --data-binary "@${frame}")
  if [ -n "$credential" ]; then
    args+=(-H "authorization: Bearer ${credential}")
  fi
  curl "${args[@]}" "http://127.0.0.1:${DEIS_RUNTIME_PORT}${path}" 2>/dev/null \
    | tr -d '\r' | awk 'tolower($1) == "grpc-status:" {print $2}' | head -n 1
}

# Nothing in Deislet issues, renews or revokes a certificate - that is the
# installation's own authority, and keeping it out is what lets any PKI be
# substituted for any other. The cost of that decision is that expiry becomes
# somebody else's job, and nothing would notice it being forgotten: an
# expiring certificate is invisible right up to the moment it expires, and
# then every connection on the machine channel fails at the same instant.
# Nodes stop fetching assignments, the control plane stops being able to drive
# them, the proxy stops seeing route changes - all at once, for a reason none
# of those processes would name.
#
# So each process says, at startup, when its own certificate runs out. This
# asks whether it really did. The date is the useful half: an operator reading
# a healthy process's log should be able to find out when somebody has to act,
# rather than finding out when it has already become urgent.
#
# This demo's certificates are minted for two days, so none of them is close
# enough to expiry for the warning to fire - which is the other half of the
# rule and is checked here too. A threshold that fired on a certificate issued
# minutes ago would teach an operator that these warnings are noise, on the
# way to the one that is not.
check_every_process_says_when_its_certificate_runs_out() {
  local title="each process says at startup when its own certificate expires"
  local phrase="certificate is good for that long"

  # deis-proxy's own tracing goes to the error log pingora is configured with,
  # not to the file its stdout is redirected into - that one carries pingora's
  # own lines. Reading the wrong one here would report the proxy as silent
  # while it was saying exactly what was asked of it.
  local control node proxy
  control=$(grep -c "$phrase" "${LOG_DIR}/deis-control.log" 2>/dev/null) || control=0
  node=$(grep -c "$phrase" "${LOG_DIR}/deis-runtime.log" 2>/dev/null) || node=0
  proxy=$(grep -c "$phrase" "${LOG_DIR}/deis_proxy_error.log" 2>/dev/null) || proxy=0

  # The name and the remaining time have to be on the line itself. A line that
  # said only "expires soon" would send the reader back to openssl to find out
  # which of three machines it was about.
  local named remaining
  named=$(grep "$phrase" "${LOG_DIR}/deis-control.log" 2>/dev/null \
    | grep -c "identity=spiffe://${FLEET_TRUST_DOMAIN}/deislet/control") || named=0
  remaining=$(grep "$phrase" "${LOG_DIR}/deis-control.log" 2>/dev/null \
    | grep -oE 'remaining_secs=[0-9]+' | head -n 1 | cut -d= -f2)

  # And nothing is warning about it, because nothing should be: every
  # certificate here was minted minutes ago for two days.
  local premature=0
  premature=$(cat "${LOG_DIR}/deis-control.log" "${LOG_DIR}/deis-runtime.log" \
    "${LOG_DIR}/deis_proxy_error.log" 2>/dev/null \
    | grep -c "has used four fifths of its life\|in the last twentieth of its life\|certificate has expired") || premature=0

  if [ "$control" -ge 1 ] && [ "$node" -ge 1 ] && [ "$proxy" -ge 1 ] \
    && [ "$named" -ge 1 ] && [ -n "$remaining" ] && [ "$remaining" -gt 0 ] \
    && [ "$premature" -eq 0 ]; then
    pass_check "$title" \
      "the control plane, the node and the proxy each logged when their own certificate runs out at startup, and the control plane's line names spiffe://${FLEET_TRUST_DOMAIN}/deislet/control with ${remaining} second(s) left - so an operator reading a healthy process's log learns the date rather than learning it from every connection failing at once. None of the three warned, which is correct: these were minted minutes ago for two days, and a threshold that fired on a fresh certificate would teach an operator that this warning is noise"
    return
  fi

  fail_check "$title" \
    "expected all three to say when their certificate expires (control ${control}, node ${node}, proxy ${proxy} line(s)), the control plane's line to name its own identity (${named}) and carry a positive remaining_secs (got \"${remaining:-none}\"), and nothing to be warning yet (found ${premature})"
}

# The check above asks the control plane to unload an application and is
# refused there. This one goes at the node directly, the way anything that can
# route to the node's port can.
#
# The node serves the control plane over gRPC: ApplyDeployment, UnloadApp,
# GetStatus, StartInspect, PushConfig and the trigger service's OnQueue and
# OnScheduled. Every one of them is administrative control of the node rather
# than application traffic, and all of them used to be dispatched on this
# port, the one that carries public traffic, with no credential checked at all
# - so GetStatus listed every tenant's applications on the node, UnloadApp
# took one offline and the next request to it answered 500, StartInspect
# minted an inspector session for another tenant's isolate, and
# ApplyDeployment would have run a bundle of the caller's choosing under that
# tenant's endpoint.
#
# Then for a while they were refused here, on a shared value: whoever held the
# fleet key was taken for the control plane. Both of those states are gone,
# and what replaced the second one is worth stating precisely, because it is
# not a stricter refusal - it is nothing to refuse. This port has no
# management dispatcher on it at all. A POST to a management path is ordinary
# application traffic and is answered as such: without an `x-deis-app-id`
# header the node asks for one, and with a real one the path is handed to that
# tenant's own code, which has no such route and says 404 with a trace id on
# it. Neither answer carries a grpc-status, because no gRPC service saw the
# request.
#
# That distinction is the whole check. "Refused" is a door with a lock, and a
# lock is a thing that can be picked, misconfigured, or quietly opened by a
# later change. "Not dispatched" is no door. So this asserts the shape of the
# answers rather than a status code: a management path here looks exactly like
# a request for a page the application does not have.
#
# What drives the node instead is the second socket, the one
# `[machine_identity] management_listen` binds, where the caller proves a
# certificate naming this node's control plane before it sends a request. A
# plaintext caller does not get a connection to it at all, which is the next
# probe.
#
# The last two assertions are the other half - the platform still works -
# because the control plane's own health poll has to reach the node somehow,
# and a node it cannot reach is reported unhealthy. They are what says the
# management plane moved rather than simply closed.
check_the_node_management_plane_is_not_on_the_data_port() {
  local title="a node is driven only over the socket where its control plane proves its name"
  local frame="${LOG_DIR}/verify-grpc-empty-frame.bin"
  # One uncompressed, zero-length message: a complete GetStatusRequest and a
  # complete ScheduledEvent, both of which encode to nothing under proto3. The
  # bytes come from /dev/zero because a shell variable cannot carry a NUL.
  head -c 5 /dev/zero > "$frame"

  # No grpc-status on any of them, which is the assertion: a status would mean
  # a gRPC service had seen the request and decided something about it.
  local grpc_none grpc_wrong grpc_key grpc_trigger
  grpc_none=$(node_grpc_status "/deis.management.RuntimeManagement/GetStatus" "" "$frame")
  grpc_wrong=$(node_grpc_status "/deis.management.RuntimeManagement/GetStatus" "not-the-node-key" "$frame")
  # The value this demo used to configure everywhere, sent now that nothing
  # configures it. It buys exactly as much as the wrong one above, and as much
  # as bringing nothing: the door it used to open is not locked, it is gone.
  grpc_key=$(node_grpc_status "/deis.management.RuntimeManagement/GetStatus" "$SECRET" "$frame")
  grpc_trigger=$(node_grpc_status "/deis.trigger.Trigger/OnScheduled" "" "$frame")

  # And what the port does answer, which is what says these are being read as
  # application traffic rather than refused as management calls.
  local without_app_id with_app_id trigger_with_app_id traced
  without_app_id=$(curl -s -o /dev/null -w '%{http_code}' -X POST \
    -H "content-type: application/grpc" -H "te: trailers" \
    --data-binary "@${frame}" \
    "http://127.0.0.1:${DEIS_RUNTIME_PORT}/deis.management.RuntimeManagement/GetStatus" 2>/dev/null || true)

  local traced_headers="${LOG_DIR}/verify-node-management-path.headers"
  with_app_id=$(curl -s -o /dev/null -D "$traced_headers" -w '%{http_code}' -X POST \
    -H "content-type: application/grpc" -H "te: trailers" -H "x-deis-app-id: ${APP_ID}" \
    --data-binary "@${frame}" \
    "http://127.0.0.1:${DEIS_RUNTIME_PORT}/deis.management.RuntimeManagement/GetStatus" 2>/dev/null || true)
  traced=$(tr -d '\r' < "$traced_headers" 2>/dev/null \
    | awk 'tolower($1) == "x-deis-trace-id:" {print $2}' | head -n 1)

  trigger_with_app_id=$(curl -s -o /dev/null -w '%{http_code}' -X POST \
    -H "content-type: application/grpc" -H "te: trailers" -H "x-deis-app-id: ${APP_ID}" \
    --data-binary "@${frame}" \
    "http://127.0.0.1:${DEIS_RUNTIME_PORT}/deis.trigger.Trigger/OnScheduled" 2>/dev/null || true)

  # And the socket that does drive this node, asked in plaintext. A caller
  # that cannot prove the control plane's name never completes a handshake,
  # so there is no HTTP response at all and curl exits non-zero.
  local plaintext_at_machine_plane=0
  curl -s -o /dev/null -X POST \
    -H "content-type: application/grpc" -H "te: trailers" \
    --data-binary "@${frame}" \
    "http://127.0.0.1:${DEIS_RUNTIME_MACHINE_PORT}/deis.management.RuntimeManagement/GetStatus" \
    >/dev/null 2>&1 || plaintext_at_machine_plane=$?

  # Nothing above unloaded anything, so the application is still there.
  local headers="${LOG_DIR}/verify-node-management-auth.headers"
  app_request "$headers" "/" > /dev/null
  local still_serving
  still_serving=$(http_status "$headers")

  local fleet healthy
  fleet="${LOG_DIR}/verify-node-management-auth-fleet.json"
  control_api /api/platform/runtime > "$fleet" 2>/dev/null || true
  healthy=$(jq -r --arg id "$NODE_ID" \
    '.nodes[] | select(.id == $id) | .healthy' < "$fleet" 2>/dev/null || true)

  if [ -z "$grpc_none" ] && [ -z "$grpc_wrong" ] && [ -z "$grpc_key" ] && [ -z "$grpc_trigger" ] \
    && [ "$without_app_id" = "400" ] && [ "$with_app_id" = "404" ] \
    && [ "$trigger_with_app_id" = "404" ] && [ -n "$traced" ] \
    && [ "$plaintext_at_machine_plane" -ne 0 ] \
    && [ "$still_serving" = "200" ] && [ "$healthy" = "true" ]; then
    pass_check "$title" \
      "GetStatus at the node's data port answered no grpc-status at all - with no credential, with a wrong one, and with the fleet secret this demo used to configure everywhere - and neither did the trigger service's OnScheduled. What it did answer is HTTP ${without_app_id} asking for an x-deis-app-id header, and with ${APP_ID}'s header on it, HTTP ${with_app_id} carrying trace id ${traced}: the path was handed to the application's own router, which has no such route. So there is no management call to refuse here and no string that would change that. A plaintext call at the mutually authenticated plane on port ${DEIS_RUNTIME_MACHINE_PORT} got no response at all (curl exited ${plaintext_at_machine_plane}), because a caller that has not proved this node's control plane's name does not get a connection. ${APP_ID} still answers HTTP ${still_serving}, and the control plane reports ${NODE_ID} healthy, which is its own health poll getting through the one door that is open"
  else
    fail_check "$title" \
      "expected no grpc-status on the data port with no credential (got \"${grpc_none:-none}\"), a wrong one (got \"${grpc_wrong:-none}\"), the retired fleet secret (got \"${grpc_key:-none}\") or on OnScheduled (got \"${grpc_trigger:-none}\"); HTTP 400 without an app id (got ${without_app_id:-none}), 404 with one (got ${with_app_id:-none}) carrying a trace id (got \"${traced:-none}\"), and 404 for OnScheduled with one (got ${trigger_with_app_id:-none}); a plaintext call at port ${DEIS_RUNTIME_MACHINE_PORT} refused at the handshake (curl exited ${plaintext_at_machine_plane}, expected non-zero). ${APP_ID} answered ${still_serving} (expected 200) and the control plane reports ${NODE_ID} healthy=${healthy:-unknown}"
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

  local route="/api/v1/apps/${APP_ID}/environments/${ENV_NAME}/storage-token"
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

  local route="/api/v1/apps/${APP_ID}/environments/${ENV_NAME}/deployment"
  local before after unknown revoked_body unknown_body
  before=$(control_status GET "$route" "$token")
  deis_cli token revoke "$id" --json >/dev/null 2>&1 || true
  after=$(control_status GET "$route" "$token")
  unknown=$(control_status GET "$route" "$NEVER_MINTED_TOKEN")
  # `/api/v1` answers a refusal in the structured envelope, and every
  # envelope carries the `requestId` of the request it answers - a fresh
  # UUIDv7 each time, which two answers can never share. It says nothing
  # about the credential, so it is dropped here and everything else - the
  # code, the sentence, the violations - is what the two answers are
  # compared on. Comparing the raw bytes instead would report the two
  # credentials as distinguishable on every run, which is the opposite of
  # what this check is about.
  revoked_body=$(control_answer GET "$route" "$token" \
    | jq -S 'del(.error.requestId)' 2>/dev/null || true)
  unknown_body=$(control_answer GET "$route" "$NEVER_MINTED_TOKEN" \
    | jq -S 'del(.error.requestId)' 2>/dev/null || true)

  # The control plane's own log does tell the two apart, which is the point:
  # "a revoked token was presented" means a credential is still deployed
  # somewhere and someone has to go and replace it.
  local said
  said=$(grep -c 'the token presented was revoked' "${LOG_DIR}/deis-control.log" 2>/dev/null || true)

  if [ "$before" = "200" ] && [ "$after" = "401" ] && [ "$unknown" = "401" ] \
    && [ "$revoked_body" = "$unknown_body" ] && [ "${said:-0}" -ge 1 ]; then
    pass_check "$title" \
      "the token read ${route} (HTTP ${before}); after \`deis token revoke ${id}\` the very next request answered ${after}, and apart from the per-request \`requestId\` every byte of it is the answer a token that was never minted gets (${unknown}). No cache had to expire: every request looks the row up. The control plane's log says \"the token presented was revoked\" ${said} time(s), which a stolen credential cannot see"
  else
    fail_check "$title" \
      "before revoking ${before} (expected 200), after ${after} and an unknown token ${unknown} (expected 401 each); the two answers were $( [ "$revoked_body" = "$unknown_body" ] && echo identical || echo different ), and the log named a revoked token ${said:-0} time(s)"
  fi
}

check_the_audit_log_names_who_changed_what() {
  local title="the audit log names the secret that was written, its application and its author"
  local report="${LOG_DIR}/verify-audit-secret.json"
  deis_cli audit --action secret.write --limit 20 --json > "$report" 2>/dev/null || true

  # The row is chosen by what happened - a secret written on this
  # application's ${ENV_NAME} and accepted - and never by the name it carries.
  # It used to be selected with `select(.target == "DEMO_SECRET")` and then
  # asserted to have that target, which is the check reading back its own
  # filter: a control plane that recorded every secret write against an empty
  # name, or against the value instead of the name, would have produced no
  # matching row and the assertion would have been about a row that was not
  # there rather than about the name being right. Only one row can match: the
  # other secret.write in this report is the scoped token's refused attempt on
  # the preview.
  local entry total app environment target outcome actor from
  total=$(jq -r '.total // 0' < "$report" 2>/dev/null || echo 0)
  entry=$(jq -c --arg app "$APP_ID" --arg env "$ENV_NAME" \
    'first(.entries[] | select(.app_id == $app and .environment == $env and .outcome == "accepted"))' \
    < "$report" 2>/dev/null || true)
  app=$(echo "$entry" | jq -r '.app_id // empty' 2>/dev/null || true)
  environment=$(echo "$entry" | jq -r '.environment // empty' 2>/dev/null || true)
  target=$(echo "$entry" | jq -r '.target // empty' 2>/dev/null || true)
  outcome=$(echo "$entry" | jq -r '.outcome // empty' 2>/dev/null || true)
  actor=$(echo "$entry" | jq -r '.actor_credential // empty' 2>/dev/null || true)
  # "From where" is half of what the line is for, and it is the field most
  # easily left empty: it comes from the connection rather than from the
  # request, so a server assembled without connect info would record every
  # call as coming from nowhere and nothing else would notice.
  from=$(echo "$entry" | jq -r '.peer // empty' 2>/dev/null || true)

  # The whole reason a secret write is recorded by name and never by value.
  local leaked=no
  grep -q "$DEMO_SECRET_VALUE" "$report" 2>/dev/null && leaked=yes

  if [ "${total:-0}" -ge 1 ] \
    && [ "$app" = "$APP_ID" ] && [ "$environment" = "$ENV_NAME" ] && [ "$target" = "DEMO_SECRET" ] \
    && [ "$outcome" = "accepted" ] && [ -n "$actor" ] && [ -n "$from" ] && [ "$leaked" = "no" ]; then
    pass_check "$title" \
      "\`deis audit --action secret.write\` returned ${total} line(s); the one for DEMO_SECRET names ${app}/${environment}, the secret ${target}, the credential ${actor} that wrote it, the address ${from} it came from and the outcome ${outcome}. The ${#DEMO_SECRET_VALUE}-character value is not in the report, and the audit table lives in the same database the secret sweep already greps"
  else
    fail_check "$title" \
      "the accepted secret.write on ${APP_ID}/${ENV_NAME} reports app '${app:-none}', environment '${environment:-none}', target '${target:-none}' (expected DEMO_SECRET), outcome '${outcome:-none}', credential '${actor:-none}' from '${from:-nowhere}' out of ${total} line(s), and the value was $( [ "$leaked" = yes ] && echo PRESENT || echo absent )"
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

  # The anonymous count is asserted rather than only printed. It is the half
  # of the claim that is about callers the platform cannot name - the
  # storage-token check makes one such call - and a control plane that only
  # recorded refusals it could attribute to a credential would leave exactly
  # those out while this line went on reporting a number.
  if [ "${total:-0}" -ge 1 ] && [ "$deploy_refusal" = "403" ] \
    && [ "${anonymous:-0}" -ge 1 ] \
    && echo "$detail" | grep -q "deploy:${APP_ID}/${ENV_NAME}"; then
    pass_check "$title" \
      "${total} refusal(s) are on the record, ${anonymous} of them from a caller who presented nothing at all. The scoped token's attempt to deploy ${APP_ID}/${ENV_NAME} is there as HTTP ${deploy_refusal} with the reason \"${detail}\" - which is how an operator finds a pipeline still holding a credential that was narrowed"
  else
    fail_check "$title" \
      "the log holds ${total:-0} refusal(s), ${anonymous:-0} of them anonymous (expected at least one of each); the refused deploy on ${APP_ID}/${ENV_NAME} reports status '${deploy_refusal:-none}' with detail '${detail:-none}'"
  fi
}

check_the_audit_log_has_no_way_to_be_erased() {
  local title="no caller of this API can erase a line of the audit log"
  local before after removed created

  before=$(deis_cli audit --limit 500 --json 2>/dev/null | jq -r '.total // 0')
  # The flat api_keys entry is the most powerful credential this control
  # plane has - it is unscoped, so it passes every scope check there is. If
  # anything could delete a line, this would.
  removed=$(control_status DELETE "/api/v1/audit" "$API_KEY")
  created=$(control_status POST "/api/v1/audit" "$API_KEY" '{"action":"forged"}')
  after=$(deis_cli audit --limit 500 --json 2>/dev/null | jq -r '.total // 0')

  if [ "$removed" = "405" ] && [ "$created" = "405" ] && [ "${after:-0}" -ge "${before:-0}" ]; then
    pass_check "$title" \
      "DELETE /api/v1/audit answered ${removed} and POST answered ${created} to the unscoped api_keys credential - the route is a read and there is no writing or deleting endpoint behind it. The log went from ${before} to ${after} line(s) across both attempts; removing one means holding the database file, which is the same access that holds the secret master key"
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
  control_api /api/platform/runtime > "$fleet" 2>/dev/null || true

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

  # Over the machine channel, proving a certificate that names this machine
  # and no other - which is the only way to join now. A registration is a
  # claim about which machine is at an address, and the control plane holds
  # that claim against the name the connection proved.
  if ! command -v grpcurl >/dev/null 2>&1; then
    fail_check "$title" \
      "grpcurl is not installed, so a node could not be registered the way a node registers"
    return
  fi

  local answer="${LOG_DIR}/verify-dynamic-register.log"
  grpcurl -max-time 10 \
    -cacert "${PKI_DIR}/ca.crt" \
    -cert "${PKI_DIR}/unlisted.crt" -key "${PKI_DIR}/unlisted.key" \
    -import-path "${ROOT_DIR}/crates/deis-wire/proto" -proto machine.proto \
    -d "{\"node_id\":\"${dynamic_id}\",\"registration_json\":\"{\\\"address\\\":\\\"http://127.0.0.1:${DEIS_RUNTIME_PORT}\\\"}\"}" \
    "127.0.0.1:${DEIS_CONTROL_MACHINE_PORT}" \
    deis.machine.MachineChannel/Register > "$answer" 2>&1 || true

  if ! grep -q "registrationJson" "$answer"; then
    fail_check "$title" \
      "Register over the machine channel, proving ${dynamic_id}'s own certificate, was not answered: $(tr '\n' ' ' < "$answer" | head -c 200)"
    return
  fi

  control_api /api/platform/runtime > "$fleet" 2>/dev/null || true

  local source registered_at
  source=$(jq -r --arg id "$dynamic_id" \
    '.nodes[] | select(.id == $id) | .source // empty' < "$fleet" 2>/dev/null || true)
  registered_at=$(jq -r --arg id "$dynamic_id" \
    '.nodes[] | select(.id == $id) | .registered_at // empty' < "$fleet" 2>/dev/null || true)

  # Take the invented identity away again, and check that it went.
  #
  # Nothing refreshes this registration, and a registration nobody refreshes
  # is a lease that runs out: the control plane then counts it among the nodes
  # it has lost and names it in the answer to every later deployment and
  # application deletion, because a node that stopped talking is normally one
  # that is still serving what it last received. This one is not - it is a
  # second name for the process already on the list as ${NODE_ID}.
  #
  # The removal is part of the verdict below rather than a swept-up loose end.
  # It used to be one line with its status thrown away and `|| true` after it,
  # which is the shape of every unwitnessed cleanup: a control plane that
  # answered 500 here left the phantom on the fleet, this check went on saying
  # ✓, and the run after it inherited a node nobody could explain. It is also
  # the only exercise `DELETE .../registration` gets anywhere in this file, so
  # discarding the answer left half the endpoint unchecked as well.
  local removed still_listed
  local removed_answer="${LOG_DIR}/verify-dynamic-deregister.log"
  grpcurl -max-time 10 \
    -cacert "${PKI_DIR}/ca.crt" \
    -cert "${PKI_DIR}/unlisted.crt" -key "${PKI_DIR}/unlisted.key" \
    -import-path "${ROOT_DIR}/crates/deis-wire/proto" -proto machine.proto \
    -d "{\"node_id\":\"${dynamic_id}\"}" \
    "127.0.0.1:${DEIS_CONTROL_MACHINE_PORT}" \
    deis.machine.MachineChannel/Deregister > "$removed_answer" 2>&1 || true
  removed=$(grep -c '"removed": true' "$removed_answer" 2>/dev/null || echo 0)
  control_api /api/platform/runtime > "$fleet" 2>/dev/null || true
  still_listed=$(jq -r --arg id "$dynamic_id" \
    '[.nodes[] | select(.id == $id)] | length' < "$fleet" 2>/dev/null || echo "?")

  if [ "$source" = "registered" ] && [ -n "$registered_at" ] \
    && [ "$removed" != "0" ] && [ "$still_listed" = "0" ]; then
    pass_check "$title" \
      "${dynamic_id} is not in deis_runtime_nodes; it joined the fleet by proving a certificate that names it and nothing else, with source=registered since ${registered_at}. Deregister, proving the same certificate, then reported it removed and the fleet no longer lists it - so the run leaves behind exactly the nodes the configuration file names. Joining used to be a PUT carrying a string every machine in the fleet held; it now takes an identity somebody had to issue"
  else
    fail_check "$title" \
      "Register over the machine channel for ${dynamic_id} left the fleet showing source='${source:-<absent>}' registered_at='${registered_at:-<none>}'; the Deregister that takes the invented identity away again reported removed=${removed} and left ${still_listed} entry/entries for it on the fleet"
  fi
}

# The other side of the two checks above: the routes a node used to call, on
# the listener a browser reaches.
#
# Self-registration is the one route on this control plane where an unlisted
# machine can put itself on the fleet, and a node on the fleet receives
# deployment pushes, cron fires and decrypted secrets. The two checks above
# register over the machine channel, proving a certificate that names one
# machine. This one asks what happened to the old way in: the same three
# requests, on the human listener, carrying a bearer value.
#
# The answer is 404 rather than 401, and the difference is the whole point. A
# 401 would say "these routes are here and a credential decides"; a 404 says
# they are not served here at all, so there is no string a caller could bring
# that would change the answer - including the fleet secret this demo used to
# configure everywhere and now configures nowhere.
#
# Nothing here can succeed, so nothing here leaves a node behind - and the
# fleet is read afterwards to say so.
check_the_node_channel_is_not_on_the_human_listener() {
  local title="nothing a node calls is served where the dashboard is"
  local invented="${NODE_ID}-uninvited-$$"

  # The three that used to be here, asked exactly as a node used to ask them,
  # carrying the kind of credential that used to open them. Nothing in this
  # demo configures a fleet key any more - that is the point of the value
  # below still being sent: whatever string a caller brings, these paths are
  # not here. A 404 says so; a 401 would have said a credential could change
  # it.
  local registration assignments routing
  registration=$(curl -s -o /dev/null -w '%{http_code}' -X PUT \
    -H "Authorization: Bearer ${SECRET}" -H 'content-type: application/json' \
    -d "{\"address\":\"http://127.0.0.1:${DEIS_RUNTIME_PORT}\"}" \
    "$(control_url)/api/nodes/${invented}/registration" || true)
  assignments=$(curl -s -o /dev/null -w '%{http_code}' \
    -H "Authorization: Bearer ${SECRET}" \
    "$(control_url)/api/nodes/${NODE_ID}/assignments" || true)
  routing=$(curl -s -o /dev/null -w '%{http_code}' \
    -H "Authorization: Bearer ${SECRET}" \
    "$(control_url)/api/routing-table" || true)

  # And the fleet, which is where anything that got through would show up.
  local fleet="${LOG_DIR}/verify-node-channel-fleet.json"
  control_api /api/platform/runtime > "$fleet" 2>/dev/null || true
  local joined healthy identity
  joined=$(jq -r --arg id "$invented" \
    '[.nodes[] | select(.id == $id)] | length' < "$fleet" 2>/dev/null || echo "?")
  healthy=$(jq -r --arg id "$NODE_ID" \
    '.nodes[] | select(.id == $id) | .healthy' < "$fleet" 2>/dev/null || true)
  identity=$(jq -r --arg id "$NODE_ID" \
    '.nodes[] | select(.id == $id) | .identity // "none"' < "$fleet" 2>/dev/null || true)

  if [ "$registration" = "404" ] && [ "$assignments" = "404" ] && [ "$routing" = "404" ] \
    && [ "$joined" = "0" ] && [ "$healthy" = "true" ] \
    && [ "$identity" != "none" ]; then
    pass_check "$title" \
      "PUT .../registration, GET .../assignments and GET the routing table all answered ${registration}/${assignments}/${routing} on the listener that serves the dashboard - carrying the fleet key, which used to open all three. They are not refused here, they are not here: the machine channel serves them and a caller there proves a name. The fleet lists ${joined} node(s) under ${invented}, and ${NODE_ID} is healthy and known by the name it proved (${identity}), so the fleet is being run from the other socket"
  else
    fail_check "$title" \
      "expected 404 from the human listener for registration (got ${registration}), assignments (got ${assignments}) and the routing table (got ${routing}), no fleet entry for ${invented} (found ${joined}), ${NODE_ID} healthy (healthy=${healthy:-unknown}) and known by a proved name (identity=${identity:-unknown})"
  fi
}


# ----------------------------------------------------------------------
# the machine channel
# ----------------------------------------------------------------------
#
# The three checks below are about the socket only Deislet's own processes
# talk on. What it answers with is every listed environment's decrypted
# secrets, a map of every hostname on the platform, and the code of every
# deployed version - so what may not reach it is as much the point as what
# may, and the first two checks are the only ones in this file whose expected
# outcome is that no request happens at all.
#
# The third is the one that could not have been written before. Under a
# fleet-wide key every machine presented the same credential, so "a valid
# credential held by the wrong machine" was not a thing that existed: a wrong
# credential was a bad string. It now has to be minted - a real certificate,
# signed by this fleet's own authority, naming a node that is not the one
# running here - and the refusal it earns is the whole reason this channel
# was built.

# Call the machine channel with whatever certificate is named, and report only
# whether a request happened. `--max-time` is generous because the answer this
# asks for is "nothing at all", and a slow refusal and a hang are different
# verdicts.
machine_channel_http() {
  local label=$1
  shift
  curl -s -o /dev/null -w '%{http_code}' --max-time 8 \
    --cacert "${PKI_DIR}/ca.crt" "$@" \
    "https://127.0.0.1:${DEIS_CONTROL_MACHINE_PORT}${MACHINE_PROBE_PATH}" 2>/dev/null
  printf ' %s' "$?"
}

# A path on the machine listener that exists. The version id is nonsense on
# purpose: a caller that gets through is answered 404 by the route, which is
# how this tells "the handshake succeeded and the route said no" from "there
# was never a connection".
MACHINE_PROBE_PATH="/api/apps/demo/versions/00000000-0000-0000-0000-000000000000/bundle"

# The trap this closes is not a refusal, it is the absence of one.
#
# Registration, the assignment list, the bundle download and the routing table
# are served only on the machine channel now. A control plane of this version
# does not serve their HTTP forms at all - the checks above prove those paths
# answer 404 - so a node or a proxy configured the old way has nowhere to ask.
#
# It does not fail, though. It starts, binds its ports, reports itself up, and
# then retries a 404 for as long as it runs: the node loads nothing and
# announces nothing, the proxy routes on whatever it can work out from its own
# file. Both look alive from outside and neither says why it is useless.
#
# So both refuse to start without a `[machine_identity]` table, and this is
# what holds them to it. The configurations below are the old shape exactly -
# an address to reach the control plane at, and nothing to prove - and what is
# asserted is that the process is gone and said which table is missing. A
# message that named neither would leave an operator reading a stack of 404s.
check_a_process_with_no_certificate_refuses_to_start() {
  local title="a process told where its control plane is, and given nothing to prove, refuses to start"
  local dir="${DEMO_DIR}/no-identity"
  mkdir -p "$dir"

  local node_config="${dir}/deis-runtime.toml"
  cat > "$node_config" <<EOF
node_id = "deis-runtime-demo-no-identity"
http_port = 1
data_dir = "${dir}/runtime-data"

[control]
endpoint = "http://127.0.0.1:${DEIS_CONTROL_PORT}"
EOF

  local proxy_config="${dir}/deis-proxy.toml"
  cat > "$proxy_config" <<EOF
listen_addr = ["127.0.0.1"]
listen_ports = [1]
database = "${dir}/deis-proxy.db"
pid_file = "${dir}/deis-proxy.pid"

[[upstreams]]
host = "127.0.0.1"
port = ${DEIS_RUNTIME_PORT}

[control]
addr = "http://127.0.0.1:${DEIS_CONTROL_PORT}"
associated_id = "deis-proxy-demo-no-identity"
EOF

  local node_out proxy_out node_rc proxy_rc
  node_rc=0
  node_out=$(DEIS_RUNTIME_CONFIG="$node_config" timeout 20 "${BIN_DIR}/deis-runtime" 2>&1) || node_rc=$?
  proxy_rc=0
  proxy_out=$(timeout 20 "${BIN_DIR}/deis-proxy" -c "$proxy_config" 2>&1) || proxy_rc=$?

  local node_named proxy_named
  node_named=no
  proxy_named=no
  case "$node_out" in *machine_identity*) node_named=yes ;; esac
  case "$proxy_out" in *machine_identity*) proxy_named=yes ;; esac

  # A port of 1 cannot be bound by an unprivileged process, so a run that got
  # as far as binding would also exit non-zero. The message is what tells the
  # two apart, which is why both halves are asserted and neither alone.
  if [ "$node_rc" -ne 0 ] && [ "$proxy_rc" -ne 0 ] \
    && [ "$node_named" = yes ] && [ "$proxy_named" = yes ]; then
    pass_check "$title" \
      "deis-runtime exited ${node_rc} and deis-proxy exited ${proxy_rc}, each naming [machine_identity] as what is missing - so a machine that could only have polled a 404 for the life of the process never gets that far. Everything either of them asks its control plane for is served on the machine channel, where a shared value opens nothing, and a process that started without a certificate would have looked healthy while doing nothing at all"
  else
    fail_check "$title" \
      "expected both to exit non-zero naming [machine_identity]: deis-runtime exited ${node_rc} (named it: ${node_named}) saying \"$(printf '%s' "$node_out" | tr '\n' ' ' | head -c 200)\"; deis-proxy exited ${proxy_rc} (named it: ${proxy_named}) saying \"$(printf '%s' "$proxy_out" | tr '\n' ' ' | head -c 200)\""
  fi
}

check_the_machine_channel_refuses_a_caller_with_no_certificate() {
  local title="the machine channel will not talk to a caller that proves nothing"

  local anonymous named
  anonymous=$(machine_channel_http anonymous)
  named=$(machine_channel_http node \
    --cert "${PKI_DIR}/node.crt" --key "${PKI_DIR}/node.key")

  # `000` is curl reporting that no HTTP status was ever received, and the
  # non-zero exit beside it is the transport failing. The node's own
  # certificate gets 404 from the same path, which is what says the listener
  # is there and answering - so the refusal above is about the credential and
  # not about the address being wrong.
  if [ "$anonymous" = "000 56" ] || [ "${anonymous%% *}" = "000" ]; then
    if [ "${named%% *}" = "404" ]; then
      pass_check "$title" \
        "a request to the machine listener with no client certificate got no HTTP status at all (curl reported \"${anonymous}\", status then exit code) - the connection ended during the handshake, so no request was ever served. The same path with ${NODE_ID}'s own certificate answered ${named%% *}, which is the route refusing a version id that does not exist: the listener is there, it answers, and what it would not do is talk to a caller that proved nothing"
    else
      fail_check "$title" \
        "the anonymous caller was refused (${anonymous}) but the node's own certificate did not reach the route either (got ${named}), so this check proved nothing about the credential"
    fi
  else
    fail_check "$title" \
      "a caller with no client certificate reached the machine listener and got ${anonymous}; every environment's decrypted secrets are served on that socket"
  fi
}

check_the_machine_channel_refuses_a_certificate_this_fleet_did_not_sign() {
  local title="a certificate this fleet did not sign does not open the machine channel"

  local rogue
  rogue=$(machine_channel_http rogue \
    --cert "${PKI_DIR}/rogue.crt" --key "${PKI_DIR}/rogue.key")

  if [ "${rogue%% *}" = "000" ]; then
    pass_check "$title" \
      "a certificate naming spiffe://${FLEET_TRUST_DOMAIN}/deislet/node/${NODE_ID} - the right name, correctly spelled, signed by an authority this fleet has never heard of - got no HTTP status at all (curl reported \"${rogue}\"). Being able to spell a machine's name is not the same as being that machine, and the handshake is where the difference is decided"
  else
    fail_check "$title" \
      "a certificate signed by an authority outside this fleet reached the machine listener and got ${rogue}"
  fi
}

check_a_fleet_certificate_only_speaks_for_the_machine_it_names() {
  local title="a real fleet certificate cannot ask for another machine's secrets"

  if ! command -v grpcurl >/dev/null 2>&1; then
    fail_check "$title" \
      "grpcurl is not installed, so the machine channel's own RPC could not be called and the check that a certificate speaks only for the machine it names did not run"
    return
  fi

  local answer="${LOG_DIR}/verify-machine-impostor.log"
  grpcurl -max-time 10 \
    -cacert "${PKI_DIR}/ca.crt" \
    -cert "${PKI_DIR}/impostor.crt" -key "${PKI_DIR}/impostor.key" \
    -import-path "${ROOT_DIR}/crates/deis-wire/proto" -proto machine.proto \
    -d "{\"node_id\":\"${NODE_ID}\"}" \
    "127.0.0.1:${DEIS_CONTROL_MACHINE_PORT}" \
    deis.machine.MachineChannel/FetchAssignments > "$answer" 2>&1 || true

  # And the same call made by the machine that certificate does name, so the
  # refusal above is about who asked rather than about the RPC being broken.
  #
  # Only whether it was answered is kept. That answer carries this
  # environment's decrypted secrets - it is the whole reason this channel
  # exists - so writing it to a file under .demo/logs would put the secret on
  # disk in the clear, which is what `check_no_secret_in_logs` is there to
  # refuse. It caught exactly that when this check was first written.
  local own="${LOG_DIR}/verify-machine-own.count"
  grpcurl -max-time 10 \
    -cacert "${PKI_DIR}/ca.crt" \
    -cert "${PKI_DIR}/node.crt" -key "${PKI_DIR}/node.key" \
    -import-path "${ROOT_DIR}/crates/deis-wire/proto" -proto machine.proto \
    -d "{\"node_id\":\"${NODE_ID}\"}" \
    "127.0.0.1:${DEIS_CONTROL_MACHINE_PORT}" \
    deis.machine.MachineChannel/FetchAssignments 2>&1 \
    | grep -c "assignmentsJson" > "$own" || true

  local refused told served
  refused=$(grep -c "PermissionDenied" "$answer" 2>/dev/null || echo 0)
  told=$(grep -m1 "Message:" "$answer" 2>/dev/null | sed 's/^ *Message: *//' || true)
  served=$(cat "$own" 2>/dev/null || echo 0)

  # The refusal has to name both machines. "Denied" on its own leaves an
  # operator with a node that cannot start and no way to tell a misissued
  # certificate from a mistyped node id.
  if [ "$refused" != "0" ] && [ "$served" != "0" ] \
    && printf '%s' "$told" | grep -q "deis-runtime-demo-99" \
    && printf '%s' "$told" | grep -q "${NODE_ID}"; then
    pass_check "$title" \
      "a certificate this fleet really issued, naming deis-runtime-demo-99, asked for ${NODE_ID}'s assignment list - the answer that carries that environment's decrypted secrets - and was refused PermissionDenied: \"$(printf '%s' "$told" | head -c 160)\". The same call with ${NODE_ID}'s own certificate was answered. Under one shared fleet key this refusal could not be expressed at all: both callers would have presented the same credential"
    else
    fail_check "$title" \
      "expected PermissionDenied naming both machines for the impostor (found ${refused} such line(s), message \"${told:-nothing}\") and an answer for ${NODE_ID}'s own certificate (found ${served} answer(s))"
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
# matters when it is interrupted; the descriptor is held in DEIS_TRACE_FD for
# await_trace_row to read from, and close_trace_stream ends the stream.
open_trace_stream() {
  local environment=$1
  local transcript=$2

  DEIS_TRACE_FD=""
  deis_cli_env
  coproc DEIS_TRACE {
    exec env "${DEIS_CLI_ENV[@]}" "${BIN_DIR}/deis" trace --follow -e "$environment" 2>&1
  }

  # bash removes the array the moment it reaps a coprocess that has already
  # exited, so a `deis trace` that dies on startup leaves ${DEIS_TRACE[0]} with
  # no value at all. Reading it then is not a check that fails but an unbound
  # variable, and `set -u` ends the entire run on the spot - the symptom being
  # a run that reported 47 checks, never asked the remaining 47, and named no
  # failing command. Take the descriptor once, through a default, so a dead
  # stream costs this one check and nothing else. Once taken it stays readable
  # to its end even if the CLI exits mid-stream: the loop simply reaches EOF.
  DEIS_TRACE_FD=${DEIS_TRACE[0]:-}
  if [ -z "$DEIS_TRACE_FD" ]; then
    printf '%s\n' \
      "deis trace --follow -e ${environment} exited before one line could be read from it" \
      >> "$transcript"
    return 1
  fi

  local line
  while IFS= read -r -t 30 -u "$DEIS_TRACE_FD" line; do
    printf '%s\n' "$line" >> "$transcript"
    case "$line" in
      *"following invocations from now on"*) return 0 ;;
    esac
  done
  return 1
}

# Read rows until one contains $1, keeping every line for the failure message.
# Sets TRACE_ROW to the row that matched.
# Read rows until one carries every string asked for, or the stream goes
# quiet for a minute.
#
# `$3` is optional and is what makes a row *this* check's. `deis trace`
# renders the invocation id as `trace=<id>` at the end of every row, so a
# caller that knows the id of the invocation it started - see
# `trace_of_witness` - can insist the row be that invocation's rather than
# the next one that happens to end the same way. Without it a check watching
# for an outcome is watching the whole environment for it, and any caller who
# can reach the node can produce one.
await_trace_row() {
  local wanted=$1
  local transcript=$2
  local also=${3:-}
  local line

  TRACE_ROW=""
  if [ -z "${DEIS_TRACE_FD:-}" ]; then
    printf '%s\n' \
      "no trace stream is open, so no row naming ${wanted} could be waited for" \
      >> "$transcript"
    return 1
  fi

  while IFS= read -r -t 60 -u "$DEIS_TRACE_FD" line; do
    printf '%s\n' "$line" >> "$transcript"
    case "$line" in
      *"$wanted"*)
        if [ -n "$also" ]; then
          case "$line" in
            *"$also"*) ;;
            *) continue ;;
          esac
        fi
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
#
# Called on both paths, including the one where the stream never opened, so the
# pid may never have been set - hence the default rather than a bare expansion.
close_trace_stream() {
  local pid=${DEIS_TRACE_PID:-}
  DEIS_TRACE_FD=""
  [ -n "$pid" ] || return 0
  kill -INT "$pid" 2>/dev/null || true
  wait "$pid" 2>/dev/null || true
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
      "GET /hello/${probe} was made after the stream opened, and deis trace printed: $(printf '%s' "$TRACE_ROW" | tr -s '[:space:]' ' ')"
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
# The counter is the second half of that verdict, and this check waits for it
# rather than assuming it. A node pushes its traces every second and its
# counters every ten - metrics.rs takes `flush_interval_secs * 3` with a floor
# of ten, and this demo configures a one-second flush - so the row arrives
# first and the sample follows, which is a reason to wait for the sample and
# not a reason to skip it.
#
# It used to be read once, straight after the row, and reported however it
# stood. An unchanged counter printed a sentence about cadence and the check
# passed; so did a counter that was never readable at all, which is how this
# was found - pointing the read below at a port nothing listens on left the
# check green while it printed "still reads nothing, which is the cadence and
# not a gap". The README sells this counter as the second proof that a heap
# ceiling is enforced, and a proof that cannot fail is not one.

# What deis-telemetry currently reports for this tenant's memory cut-offs, or
# nothing - which covers both "no node has pushed that series yet" and "the
# scrape endpoint did not answer". The waiter below fails on either rather
# than excusing them, because from here they are the same missing evidence.
memory_cutoff_counter() {
  curl -s -m 5 "http://127.0.0.1:${DEIS_TELEMETRY_METRICS_PORT}/metrics" 2>/dev/null \
    | grep "^deis_runtime_memory_cutoffs_total{app_id=\"${APP_ID}\",environment=\"${ENV_NAME}\"" \
    | awk '{print $NF}' || true
}

# Read that counter until it stands above $1, and echo what it last read.
#
# Every round is a real read of the series rather than a wait long enough to
# be safe, and the budget is a count of rounds rather than a duration this
# script measures: sixty of them at about a second each is six times the
# ten-second push interval, so exhausting it means the sample is not coming
# rather than that it is late. `awk` does the comparison because the
# exposition format renders this as a gauge, and a gauge may carry a decimal
# point that `-gt` cannot read.
await_memory_cutoff_counter() {
  local floor=${1:-0}
  local rounds=${2:-60}
  local now=""
  while [ "$rounds" -gt 0 ]; do
    now=$(memory_cutoff_counter)
    if [ -n "$now" ] && awk -v now="$now" -v floor="$floor" 'BEGIN { exit !(now > floor) }'; then
      echo "$now"
      return 0
    fi
    rounds=$((rounds - 1))
    poll_pause
  done
  echo "$now"
  return 1
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

  # A tag this run invents, so the trace row judged below is the one written
  # for this request. Without it the check waits for *an* invocation of this
  # environment to end out of heap, and one `GET /api/heap` from outside -
  # the route is unauthenticated, like every other - supplies that ending
  # while this request fails for some other reason entirely.
  local tag="heap-$$-${RANDOM}"
  local answered status seconds
  answered=$(curl -s -o /dev/null -w '%{http_code} %{time_total}' \
    -H "x-deis-app-id: ${APP_ID}" \
    "http://127.0.0.1:${DEIS_RUNTIME_PORT}/api/heap?tag=${tag}" || true)
  status=${answered%% *}
  seconds=${answered##* }

  # Which invocation that was, read off the marker the handler logged before
  # its first allocation. `deis trace` prints the same id as `trace=<id>`.
  local trace
  trace=$(trace_of_witness "heap witness ${tag}")
  if [ -z "$trace" ]; then
    close_trace_stream
    fail_check "$title" \
      "GET /api/heap answered HTTP ${status} after ${seconds}s, but the node logged no 'heap witness ${tag}' line, so this check cannot tell which invocation was its own and will not accept another caller's out-of-heap ending as evidence for it; the log is ${LOG_DIR}/deis-runtime.log"
    return
  fi

  if ! await_trace_row "memory-limit" "$out" "trace=${trace}"; then
    close_trace_stream
    fail_check "$title" \
      "GET /api/heap answered HTTP ${status} after ${seconds}s, but invocation ${trace} - this request's own - was not reported as memory-limit within 60s, so either the ceiling was never reached or the near-heap-limit callback did not classify it; see ${out}"
    return
  fi
  close_trace_stream

  if [ "$status" != "500" ]; then
    fail_check "$title" \
      "an invocation was recorded as memory-limit, but GET /api/heap answered HTTP ${status} rather than failing; a handler killed for its heap cannot also have answered"
    return
  fi

  # A deployment fact, read from the prefix that existed before this phase
  # began. Unlike every other log read in these checks this one decides
  # nothing - the number lands in the pass sentence and in no condition - so
  # a forged line here misreports a ceiling rather than turning a check green.
  # It is narrowed anyway: a number an operator reads as the node's should be
  # the node's.
  local ceiling after
  ceiling=$(node_log_window "${DEMO_APP_DEPLOY_FROM:-0}" "${DEMO_APP_DEPLOY_TO:-0}" \
    | sed -n "s/.*App ${APP_ID}.${ENV_NAME} narrows the default heap ceiling: \([0-9]*\)MB.*/\1/p" \
    | head -n 1)

  # The counter has to arrive, and not arriving is the failure being waited
  # for: a scrape endpoint that is down, a node whose metrics shipper has
  # stopped, a callback that records the verdict without charging the counter.
  # Every one of those leaves the README's second proof missing while the
  # first still reads perfectly, which is exactly the state nobody notices.
  if ! after=$(await_memory_cutoff_counter "${before:-0}"); then
    fail_check "$title" \
      "deis trace calls the invocation memory-limit, but deis_runtime_memory_cutoffs_total{app_id=\"${APP_ID}\",environment=\"${ENV_NAME}\"} never rose above ${before:-nothing}: after sixty reads of http://127.0.0.1:${DEIS_TELEMETRY_METRICS_PORT}/metrics it reads ${after:-nothing}. A node pushes that series every ten seconds, so a minute of reads is not a cadence problem - the near-heap-limit callback increments the counter in the same breath as it records the verdict, and one of the two never left the node"
    return
  fi

  pass_check "$title" \
    "GET /api/heap allocated past the ${ceiling:-?}MB heap this version's manifest asks for and answered HTTP ${status} after ${seconds}s. deis trace calls invocation ${trace} - this request's own, and no other - memory-limit, which nothing but the near-heap-limit callback can produce - from the request path an out-of-heap kill and a handler that threw are the same terminated execution: $(printf '%s' "$TRACE_ROW" | tr -s '[:space:]' ' '). The same callback increments deis_runtime_memory_cutoffs_total, and deis-telemetry's scrape for this tenant went from ${before:-nothing} to ${after} while this check waited for it - that series carries no invocation dimension, so its rise says the counter is written at all, and the row above is what says this request is the one that was cut off"
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

  # A coprocess that has already exited is reaped and its array removed, so
  # both of these are unset when `deis tail` dies on startup. Under `set -u` a
  # bare expansion would end the whole run instead of this one check - see the
  # same guard in open_trace_stream. The descriptor stays readable to EOF once
  # taken, so a CLI that exits mid-stream still leaves a failed check behind.
  local tail_pid=${DEIS_TAIL_PID:-}
  local tail_fd=${DEIS_TAIL[0]:-}
  if [ -z "$tail_fd" ]; then
    fail_check "$title" \
      "deis tail -e ${ENV_NAME} exited before one line could be read from it"
    return
  fi

  # Wait until the stream is really open before producing anything to see.
  #
  # `deis tail` prints this banner only after the Tail call has returned, and
  # deis-telemetry registers the subscriber inside that handler before it
  # answers - so the banner is proof that a line produced from now on reaches
  # this stream. Doing it the other way round is the classic flake: the
  # request is logged, flushed and broadcast while nobody is listening, and a
  # tail that correctly shows only new lines looks broken.
  local line opened=0
  while IFS= read -r -t 30 -u "$tail_fd" line; do
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
  while IFS= read -r -t 60 -u "$tail_fd" line; do
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

# ----------------------------------------------------------------------
# The evidence this run rests on
# ----------------------------------------------------------------------

check_the_node_log_this_run_read_was_whole() {
  local title="the log fifteen of these checks read was not cut off while they read it"
  local log="${LOG_DIR}/deis-runtime.log"

  if [ ! -f "$log" ]; then
    fail_check "$title" \
      "${log} does not exist, so every check above that quoted the node was quoting an empty file"
    return
  fi

  local size
  size=$(wc -c < "$log" | tr -d ' ')

  # The marker `bounded_log` writes when it stops. Its absence is the pass:
  # the ceiling exists so that an application - or anyone who can reach this
  # node's unauthenticated routes - cannot fill the disk with the operator's
  # log, and reaching it means the checks above read a partial file without
  # any of them being able to tell.
  if grep -a -q -F "$DEMO_LOG_CAPPED_MARKER" "$log"; then
    fail_check "$title" \
      "the node's log hit its ${DEMO_LOG_MAX_BYTES} byte ceiling during this run and was cut off from that point on, so every check above that read it may have been reading a file that stops before the line it wanted. Raise DEMO_LOG_MAX_BYTES and start again, and look at what filled it: ${log}"
    return
  fi

  pass_check "$title" \
    "${log} is ${size} bytes against the ${DEMO_LOG_MAX_BYTES} byte ceiling this demo gives it, and carries no cut-off marker, so the checks that read the node's own sentences read a whole file. The ceiling is there because an application's log line lands in this file and carries whatever a request put in it: without one, an unauthenticated GET is a write to the operator's disk with nothing counting it"
}

verify_demo() {
  load_state
  require_command curl
  require_command jq

  # `set -E` is what puts the ERR trap inside the check functions. Without it
  # bash runs an ERR trap only at the top level, which is the one place none
  # of this happens.
  set -E
  trap 'on_verify_error "$LINENO" "$BASH_COMMAND" "$?"' ERR
  trap on_verify_exit EXIT

  # Everything already in the node's log is deployment and startup evidence:
  # it was written before this phase made a single request, so no line an
  # outsider provokes from here on can be inside it. See
  # `node_log_before_verify`, which is what reads it.
  DEMO_VERIFY_LOG_MARK=$(node_log_mark)

  echo "🔎 Verifying the demo end to end"
  echo
  # The CLI checks come first because everything after them rests on what
  # `deis deploy` put there. If the deploy report is wrong, saying so before
  # eleven other checks fail for the same reason saves reading all eleven.
  echo "-- the CLI deploys --"
  check_cli_deploy
  check_cli_versions
  echo "-- the compiler --"
  # Beside the deploy, because it is about the same command's first step. It
  # builds a second, much smaller project and deploys none of it: three of the
  # four targets are for platforms nothing here runs.
  check_every_compiler_target_is_built
  # And the other half of what the compiler is for: a project the standard
  # has no portable shape for is refused by the same command, at the same
  # step, before an application, environment or version exists for it.
  check_a_websocket_handler_is_refused_by_the_build
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
  check_the_assets_name_cannot_be_declared_by_an_application
  echo "-- routing --"
  check_route_parameter
  # Through deis-proxy and with no x-deis-app-id header, which is the only
  # arrangement in which a route decides anything. It leaves two rules in the
  # catalog on a host nothing else here uses.
  check_the_proxy_routes_by_host_and_path_prefix
  echo "-- streaming responses --"
  # The first two go through deis-proxy, which is the only hop that could
  # quietly collect a body: everything else addresses the node directly, so a
  # proxy that buffered one would leave every other check here green.
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
  # First of these, because it is about the one piece of `globalThis` an
  # application is not meant to be able to use at all: what owns its timers.
  check_no_timer_outlives_the_request_that_armed_it
  # Here rather than at the end: these are about what a handler finds on
  # `globalThis`, which is the same subject as the check above, and they run
  # before the rollback moves production around underneath them.
  check_app_fetch_allowed
  check_app_fetch_refused
  # Straight after the refusal, because it is the same policy asked the same
  # question about an address the application never named.
  check_a_redirect_hop_to_a_refused_host_is_stopped
  check_web_crypto
  check_named_caches
  # Straight after the cache the application holds, because these two are the
  # same store asked the one question a single caller cannot ask it: what a
  # second caller is handed. They run before the rollback, which replaces the
  # isolate and with it the store they read.
  check_a_second_caller_is_not_handed_the_first_ones_answer
  check_a_credentialed_answer_that_records_no_owner_is_not_stored
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
  # What a tenant's SQL may cost, as opposed to what it may touch. Both go
  # through `deis db query`, which is the same Database service the
  # the declared database binding reaches.
  check_a_runaway_query_is_stopped_and_names_its_ceiling
  check_a_single_value_cannot_be_bigger_than_a_row_may_be
  # What a tenant's SQL may reach, as opposed to what it may spend. It writes
  # a table in production and reads for it from the preview, so it runs after
  # both cost checks and before anything that moves either environment.
  check_a_tenants_sql_cannot_reach_another_tenants_tables_or_files
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
  # Last of the queue checks that move messages: it is the only one that has
  # to wait for the attempt ceiling to be reached, and it leaves a message on
  # the queue with a long delay rather than in the dead letter table.
  check_queue_dead_letters
  # And the one that goes at deis-queue's own port instead of through the
  # node. It only reads the dead letter table, so it runs after the check that
  # fills and empties it and disturbs nothing either way.
  check_queue_capability_token
  echo "-- cron --"
  check_cron
  echo "-- calling another application --"
  # The consent check redeploys the callee twice and puts it back the way it
  # was, so it runs after the two that need the callee as the run left it.
  check_one_application_calls_another
  # Straight after it, because it is the same header read by the same handler:
  # one call the platform stamped, one a client tried to stamp itself.
  check_a_client_cannot_forge_the_platform_s_headers
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
  check_an_unknown_compatibility_flag_is_carried_not_refused
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
  # Straight after it: the same endpoint asked by a caller with no node key,
  # which is what says the two above were served for a reason. It leaves no
  # node behind, so it can run either side of them.
  check_the_node_channel_is_not_on_the_human_listener
  # The same question asked of the channel that replaces that key: what a
  # machine has to prove to reach the answers only machines may have.
  check_the_machine_channel_refuses_a_caller_with_no_certificate
  check_the_machine_channel_refuses_a_certificate_this_fleet_did_not_sign
  # And the other side of the same sentence: what happens to a machine that
  # has no certificate to prove. It is not refused at a door, it never starts.
  check_a_process_with_no_certificate_refuses_to_start
  check_a_fleet_certificate_only_speaks_for_the_machine_it_names
  check_cli_status
  echo "-- accounts, scopes and the audit log --"
  # After every check that expects a particular version on the preview
  # environment: the scoped-token check re-deploys preview to the version it
  # is already running, which is what proves a narrow credential can really
  # ship rather than only be refused.
  check_the_configured_api_key_is_listed_as_unscoped
  check_a_scoped_token_reaches_one_environment_and_no_other
  check_a_credential_cannot_wire_an_app_to_one_it_cannot_see
  check_a_scoped_token_cannot_unload_an_application_from_a_node
  check_the_node_management_plane_is_not_on_the_data_port
  check_every_process_says_when_its_certificate_runs_out
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
  echo "-- the evidence this run rests on --"
  # Last, and about the run rather than about the platform: whether the file
  # the log-reading checks above quoted was whole while they quoted it.
  check_the_node_log_this_run_read_was_whole

  # Before the summary, so the traps above stand down only once the whole
  # list has actually been asked. The line below returns non-zero when a
  # check failed, and that is a finished run reporting a failure rather than
  # a run that stopped.
  VERIFY_REACHED_ITS_SUMMARY=true

  # The roll call. A run that reached the end of its list has still proved
  # nothing about a check that quietly left the list, so the length is
  # asserted and the number is printed whether it matches or not.
  local counted roll_call duplicates
  counted=$((CHECKS_PASSED + CHECKS_FAILED))
  roll_call=0
  if [ "$counted" -ne "$VERIFY_EXPECTED_CHECKS" ]; then
    roll_call=1
    echo
    echo "   ✗ the roll call does not add up"
    echo "     ${counted} verdict(s) were printed and this file says a complete run prints ${VERIFY_EXPECTED_CHECKS}. If a check was added or removed on purpose, VERIFY_EXPECTED_CHECKS in scripts/demo.sh is the number to change; until then the difference is a question that stopped being asked, and no count of what did run says anything about it."
  fi
  duplicates=$(printf '%s\n' "$VERIFY_CLAIMS" | sed '/^$/d' | sort | uniq -d)
  if [ -n "$duplicates" ]; then
    roll_call=1
    echo
    echo "   ✗ two verdicts were printed under one claim"
    echo "     $(printf '%s' "$duplicates" | tr '\n' ';' | head -c 300). Either one check reported twice or two checks make the same claim; either way the tally above is right about its own arithmetic and wrong about how many questions were asked."
  fi

  echo
  echo "${CHECKS_PASSED} passed, ${CHECKS_FAILED} failed, ${counted} of the ${VERIFY_EXPECTED_CHECKS} checks this file lists"
  [ "$CHECKS_FAILED" -eq 0 ] && [ "$roll_call" -eq 0 ]
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
