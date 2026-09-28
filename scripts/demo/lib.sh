# The demo's shared helpers: where it keeps its state and which ports it uses,
# the certificate authority and the identities it mints, the processes it
# starts and stops, the control plane and `deis` CLI helpers, and the
# verification counters every check reports through.
#
# Sourced by `scripts/demo.sh`, which is the only entry point; nothing here
# runs on its own.

ROOT_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
DEMO_DIR="${DEMO_DIR:-${ROOT_DIR}/.demo}"
LOG_DIR="${LOG_DIR:-${DEMO_DIR}/logs}"
STATE_FILE="${STATE_FILE:-${DEMO_DIR}/state.env}"
# The control plane's own state directory, named `data_dir` in its
# configuration: where the one-time bootstrap token a first start mints is
# written. Cleared with everything else a new run throws away, so every
# `start` mints a fresh bootstrap credential.
CONTROL_DATA_DIR="${CONTROL_DATA_DIR:-${DEMO_DIR}/control-data}"
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
    "${CONTROL_DATA_DIR}" \
    "${DEMO_DIR}/used-bootstrap.token" \
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
#   * the authority carries keyUsage and every leaf carries its key
#     identifiers, which strict verifiers (Python 3.13+'s ssl module, used by
#     the streaming checks) refuse a chain without;
#   * every leaf carries both `serverAuth` and `clientAuth`, because each
#     process here is a server on one connection and a client on another: the
#     control plane serves the machine channel and dials its nodes, and a node
#     serves its management plane and dials the control plane.
mint_ca() {
  local prefix=$1
  local subject=$2
  openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:P-256 -nodes \
    -keyout "${prefix}.key" -out "${prefix}.crt" -days 2 \
    -subj "/CN=${subject}" \
    -addext "basicConstraints=critical,CA:TRUE" \
    -addext "keyUsage=critical,keyCertSign,cRLSign" >/dev/null 2>&1
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
  chmod 600 "${leaf}.key"
  cat > "${leaf}.ext" <<EXT
subjectAltName=${san}
basicConstraints=CA:FALSE
keyUsage=critical,digitalSignature
extendedKeyUsage=serverAuth,clientAuth
subjectKeyIdentifier=hash
authorityKeyIdentifier=keyid
EXT
  openssl x509 -req -in "${leaf}.csr" -CA "${ca}.crt" -CAkey "${ca}.key" \
    -CAcreateserial -days 2 -extfile "${leaf}.ext" -out "${leaf}.crt" \
    >/dev/null 2>&1
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
    "spiffe://${FLEET_TRUST_DOMAIN}/deislet/proxy/${PROXY_ID}" "IP:127.0.0.1"

  # The replacement proxy the zero-downtime restart check starts. It is a
  # second machine with a name of its own, not the same one twice: the
  # handover is two processes alive at once, and each has to be able to say
  # which it is. The IP SAN is there because it forwards to the node over
  # mutual TLS, as its colleague does.
  mint_leaf "${PKI_DIR}/ca" "${PKI_DIR}/proxy-restart" \
    "spiffe://${FLEET_TRUST_DOMAIN}/deislet/proxy/deis-proxy-demo-restart" "IP:127.0.0.1"

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

# Wait for the node's data port to answer over mutual TLS. The shape is
# `wait_for_http_any`'s, with the demo proxy's certificate in front and the
# port read from the environment - which is what lets it sit behind
# `wait_for_service` beside the plaintext probes above: a node that has bound
# the port but not yet loaded its applications answers 503 with
# `x-deis-node-readiness: starting`, and curl's --retry retries that too, so
# only a node serving its assignment reports ready. The handshake itself is
# retried as well: until the node is up the connection is refused, and
# --retry-connrefused covers that.
wait_for_runtime() {
  local attempts=$1
  runtime_curl -s -o /dev/null -m 2 \
    --retry "$attempts" --retry-delay 1 --retry-max-time $((attempts * 2)) \
    --retry-connrefused \
    "$(runtime_data_url)/health" >/dev/null 2>&1
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
#
# The application is named the way a visitor names it: by the Host header.
# The edge routes by host alone - a request header naming an application is
# not a routing input anywhere - so the probe asks for the automatic address
# the routing table synthesises for the default environment,
# `<app>.<preview zone>`, which is the same name the checks below send.
wait_for_proxy_routing() {
  local attempts=$1
  curl -sf -o /dev/null -m 2 \
    --retry "$attempts" --retry-delay 1 --retry-max-time $((attempts * 2)) \
    --retry-connrefused --retry-all-errors \
    -H "Host: ${ACME_PRODUCTION_NAME}" \
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

  # The runtime's probe takes only the retry count, its caller's target is
  # the pid to watch; the probe reads the port from the environment.
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
OPERATOR_TOKEN=$OPERATOR_TOKEN
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

# Bring up deis-telemetry, the service every other component reports to. It
# is started before the control plane on purpose: it depends on nothing,
# while every other component ships logs at it, so it should already be
# listening when the rest come up. Its HTTP scrape endpoint is a real answer
# and so a stronger readiness signal than a TCP probe.
#
# deis-store and deis-queue come up in start_storage_services, after the
# control plane: they verify every call against the keys the control plane
# publishes, and the address they fetch those from carries the port the
# control plane ends up bound to.
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

  echo "🚀 Starting Telemetry (gRPC: ${DEIS_TELEMETRY_PORT}, metrics: ${DEIS_TELEMETRY_METRICS_PORT})"
  "${BIN_DIR}/deis-telemetry" --config "$DEIS_TELEMETRY_CONFIG" \
    > "${LOG_DIR}/deis-telemetry.log" 2>&1 &
  DEIS_TELEMETRY_PID=$!

  local rc=0
  wait_for_service wait_for_http \
    "http://127.0.0.1:${DEIS_TELEMETRY_METRICS_PORT}/metrics" "$DEIS_TELEMETRY_PID" 60 || rc=$?
  report_service "Telemetry" "$rc" "${LOG_DIR}/deis-telemetry.log" \
    "logs and metrics pushed at it are dropped"
}

# deis-store (KV/DB/Blob) and deis-queue, once the control plane is up.
#
# Neither has a secret of its own. Every call must carry an Ed25519
# capability the control plane signed for the tenant, audience and binding it
# names (crates/deis-store/src/auth.rs), and the keys it is checked against
# are fetched from the control plane's JWKS at capability_jwks_url - at
# startup, every few minutes, and whenever a token names a key not yet seen,
# so a key rotation needs nothing done here. revocation_list_url is where
# both poll for revoked tokens. Both default to the control plane's default
# port, which this demo may not have been given, so both are spelled out.
start_storage_services() {
  local control_url="http://127.0.0.1:${DEIS_CONTROL_PORT}"

  cat > "$DEIS_STORE_CONFIG" <<EOF
listen = "127.0.0.1:${DEIS_STORE_PORT}"
data_dir = "${DEMO_DIR}/deis-store-data"
capability_jwks_url = "${control_url}/.well-known/deis/capability-jwks.json"
revocation_list_url = "${control_url}"
EOF
  chmod 600 "$DEIS_STORE_CONFIG"

  cat > "$DEIS_QUEUE_CONFIG" <<EOF
listen = "127.0.0.1:${DEIS_QUEUE_PORT}"
data_dir = "${DEMO_DIR}/deis-queue-data"
capability_jwks_url = "${control_url}/.well-known/deis/capability-jwks.json"
revocation_list_url = "${control_url}"
EOF
  chmod 600 "$DEIS_QUEUE_CONFIG"

  echo "🚀 Starting Store (gRPC: ${DEIS_STORE_PORT})"
  "${BIN_DIR}/deis-store" --config "$DEIS_STORE_CONFIG" \
    > "${LOG_DIR}/deis-store.log" 2>&1 &
  DEIS_STORE_PID=$!

  echo "🚀 Starting Queue (gRPC: ${DEIS_QUEUE_PORT})"
  "${BIN_DIR}/deis-queue" --config "$DEIS_QUEUE_CONFIG" \
    > "${LOG_DIR}/deis-queue.log" 2>&1 &
  DEIS_QUEUE_PID=$!

  # Both are launched before either is probed, so they come up in parallel.
  # Only a TCP probe is possible for a gRPC service here.
  local rc

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
#   1. `deis build --target deislet`  -> dist/providers/deislet/<digest>/bundle.zip
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

# GET one of the control plane's human endpoints, presenting the demo's
# operator token.
#
# Used only where there is no CLI command for the question - the fleet view is
# an operator's, not a developer's. Everything a developer does goes through
# `deis`. A credential is required: the human API has no unauthenticated path,
# so a caller that forgets it gets 401 rather than an answer.
control_api() {
  curl -s -H "Authorization: Bearer ${OPERATOR_TOKEN}" "$(control_url)$1" || true
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
    # The control plane requires a credential, so the CLI has to carry one. In
    # the environment rather than in a credentials file for the reason the
    # endpoint is: the token is minted at start time and a file committed to
    # the repository cannot know it. It is the scoped operator token
    # `bootstrap_operator_session` minted from the first owner account; the
    # CLI's own login token from that same session sits beside it in the
    # config home above. A person stores theirs with
    # `deis config set control.token`.
    "DEIS_CONTROL_TOKEN=${OPERATOR_TOKEN}"
  )
}

deis_cli() {
  deis_cli_env
  env "${DEIS_CLI_ENV[@]}" "${BIN_DIR}/deis" "$@"
}

# Trade the one-time bootstrap credential for the demo's operator credential.
#
# `start` writes no human credential into any configuration file. On a first
# start with an empty database the control plane mints a one-time bootstrap
# token, writes it 0600 into `<data_dir>/bootstrap.token`, and allows it
# exactly one thing: creating the first owner account and its first token.
# This is the same door every installation walks through, driven the way a
# person drives it - `deis login --bootstrap`.
#
# Three things happen here:
#
# 1. the login. The CLI creates the owner account and stores its new token in
#    the demo's own config home; the control plane revokes the bootstrap
#    credential and deletes its file after that first successful use;
# 2. from the owner account, the scoped operator token the rest of `start`
#    and `verify` present. It is scoped rather than flat on purpose: a
#    credential answered by no scope list is the shape this platform retired
#    with `api_keys`. The verbs cover what the demo's own calls do - deploy
#    and upload versions, write secrets, mint storage tokens, manage
#    accounts, routes and the audit log. The storage verbs name their two
#    tenants outright, because a blanket storage grant is refused where it is
#    written: such a token works against deis-store with no control plane in
#    the path and never expires, so there is no wildcard form of it to hand
#    out by accident.
# 3. the spent bootstrap secret is kept in a 0600 file beside the demo state,
#    so `verify` can present it once more and watch it be refused.
bootstrap_operator_session() {
  local bootstrap_file="${CONTROL_DATA_DIR}/bootstrap.token"
  local spent_file="${DEMO_DIR}/used-bootstrap.token"

  # The file is written during control's startup, after the database opens -
  # /health answering is not a promise it exists yet, so this is polled.
  local waited=0
  while [ ! -f "$bootstrap_file" ] && [ "$waited" -lt 60 ]; do
    sleep 0.5
    waited=$((waited + 1))
  done
  if [ ! -f "$bootstrap_file" ]; then
    echo "❌ Control wrote no bootstrap token at ${bootstrap_file}; there is no way to log in. Log: ${LOG_DIR}/deis-control.log"
    exit 1
  fi

  # Read the secret out only to keep it for the one-time-use check; the login
  # itself reads the file, exactly as a person points the CLI at it.
  local spent
  spent=$(cat "$bootstrap_file")

  DEIS_CONTROL_ENDPOINT="$(control_url)" \
    DEIS_CONFIG_HOME="${DEMO_DIR}/deis-config" \
    "${BIN_DIR}/deis" login --bootstrap "$bootstrap_file" \
    > "${LOG_DIR}/cli-login.log" 2>&1 || {
      echo "❌ deis login --bootstrap failed; see ${LOG_DIR}/cli-login.log"
      exit 1
    }

  # The owner's login token now lives in the CLI's config home, and the CLI
  # presents it by itself - no DEIS_CONTROL_TOKEN here, which is also why
  # these calls spell the environment out rather than going through
  # `deis_cli_env`, whose array carries the operator token this step exists
  # to mint.
  local owner minted
  owner=$(env \
    DEIS_CONTROL_ENDPOINT="$(control_url)" \
    DEIS_CONFIG_HOME="${DEMO_DIR}/deis-config" \
    "${BIN_DIR}/deis" token list --json 2>/dev/null \
    | jq -r '[.tokens[].account] | first // empty')
  if [ -z "$owner" ]; then
    echo "❌ could not read back the owner account the bootstrap login created; see ${LOG_DIR}/cli-login.log"
    exit 1
  fi

  minted=$(env \
    DEIS_CONTROL_ENDPOINT="$(control_url)" \
    DEIS_CONFIG_HOME="${DEMO_DIR}/deis-config" \
    "${BIN_DIR}/deis" token create --account "$owner" demo-operator \
      --scope "admin:*/*" --scope "deploy:*/*" --scope "secrets:*/*" \
      --scope "storage:${APP_ID}/${ENV_NAME}" --scope "storage:${APP_ID}/${PREVIEW_ENV_NAME}" \
      --json 2>/dev/null || echo '{}')
  OPERATOR_TOKEN=$(echo "$minted" | jq -r '.secret // empty')
  if [ -z "$OPERATOR_TOKEN" ]; then
    echo "❌ could not mint the demo's scoped operator token: ${minted}"
    exit 1
  fi

  # The bootstrap credential is spent: the control plane revoked it and
  # deleted its file when the login used it. What is kept here is a dead
  # string, kept only so `verify` can show a second presentation being
  # refused like one that never existed.
  printf '%s\n' "$spent" > "$spent_file"
  chmod 600 "$spent_file"
  if [ -f "$bootstrap_file" ]; then
    echo "⚠️  ${bootstrap_file} still exists after login; verify's single-use check will fail"
  fi
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

  echo "   Send requests with: ${DEMO_SCRIPT_DIR}/demo/lib.sh's runtime_curl -H 'x-deis-app-id: ${APP_ID}' https://127.0.0.1:${DEIS_RUNTIME_PORT}/ -- the data port admits only a fleet proxy's certificate"
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

# Every claim a verdict has been printed for, one per line.
#
# The count alone cannot tell "a check vanished" from "a check vanished and
# another printed twice"; two verdicts under one claim keep the tally right
# while the list is wrong. Claims are compared rather than function names
# because the claim is what a reader is told was proved.
VERIFY_CLAIMS=""

# How many checks the named groups add up to.
#
# `scripts/demo.sh` calls this with the whole registry after it has sourced
# every group file, and again with a single group when `--group` narrows a
# run; the number every run is held to comes from here rather than from a
# literal anybody has to remember to update.
demo_count_checks() {
  local group total=0
  for group in "$@"; do
    local -n group_checks="group_${group}_checks"
    total=$((total + ${#group_checks[@]}))
  done
  printf '%s' "$total"
}

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
# The node's data port admits a caller only over mutual TLS naming a proxy of
# this fleet, so every request the checks send there presents the demo
# proxy's own leaf - the same credential deis-proxy presents when it forwards
# a visitor's request - and verifies the node against the fleet root. The
# node's leaf carries an IP SAN for 127.0.0.1, so verification is by name and
# not by `--insecure`.
runtime_curl() {
  curl --cacert "${PKI_DIR}/ca.crt" \
    --cert "${PKI_DIR}/proxy.crt" --key "${PKI_DIR}/proxy.key" "$@"
}

runtime_data_url() {
  printf 'https://127.0.0.1:%s' "${DEIS_RUNTIME_PORT}"
}

app_request() {
  local header_file=$1
  local path=$2
  shift 2
  runtime_curl -s -D "$header_file" -H "x-deis-app-id: ${APP_ID}" "$@" \
    "$(runtime_data_url)${path}" || true
}

# The same, addressed to a named environment of the application. No header
# means production, so production keeps working for a caller that predates
# environments; naming one is how a preview is reached.
env_request() {
  local environment=$1
  local header_file=$2
  local path=$3
  shift 3
  runtime_curl -s -D "$header_file" \
    -H "x-deis-app-id: ${APP_ID}" -H "x-deis-environment: ${environment}" "$@" \
    "$(runtime_data_url)${path}" || true
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

# A capability for this demo's application and environment, minted by the
# control plane through the same route `deis kv` uses
# (GET /api/v1/apps/:app/environments/:env/storage-token) and presented by the
# operator token, which carries storage:${APP_ID}/${ENV_NAME}. `store` (the
# default) answers the token deis-store accepts for kv, sql and blob; `queue`
# answers the one deis-queue accepts. Nothing in this script can compute one
# itself: they are Ed25519 signatures by a key only the control plane holds.
storage_token() {
  local field=token
  [ "${1:-store}" = "queue" ] && field=queue_token
  control_api "/api/v1/apps/${APP_ID}/environments/${ENV_NAME}/storage-token" \
    | jq -r --arg f "$field" '.[$f] // empty' 2>/dev/null || true
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
  runtime_curl -s -G -D "${LOG_DIR}/verify-${label}.headers" \
    -H "x-deis-app-id: ${APP_ID}" \
    --data-urlencode "url=${target}" \
    "$(runtime_data_url)/api/fetch" || true
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
