# Bringing the demo fleet up and taking it down again: `start_demo`,
# `stop_demo` and the shutdown they wait for.
#
# Sourced by `scripts/demo.sh`; nothing here runs on its own.

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
  # settings dump: SECRET, OPERATOR_TOKEN, STORAGE_AUTH_SECRET, APP_SRC_DIR, APP_ID,
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

  # No credential for a person is generated here, and none goes into any
  # configuration file. The control plane's human API has no unauthenticated
  # path, and a first start with an empty database mints a one-time bootstrap
  # token into its data directory; `bootstrap_operator_session` below logs
  # the first owner in through it and mints the scoped operator token the
  # rest of this script presents. A different channel again:
  # STORAGE_AUTH_SECRET authenticates a *storage call*, and the operator
  # token authenticates *a caller who may deploy*.

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
# Where this installation keeps its own state. Notably the one-time
# bootstrap token: a first start with no credential anywhere in the database
# mints one here, 0600, and `bootstrap_operator_session` logs the first
# owner in through it.
data_dir = "${CONTROL_DATA_DIR}"
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
    # storage_auth_secret, and every machine's names the private
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
# The data port answers only mutual TLS: the node verifies every caller
# against the fleet root and checks the certificate names a proxy, so the
# address below is unusable without the proxy's leaf from ${PKI_DIR}.
data_listen = "127.0.0.1:${DEIS_RUNTIME_PORT}"
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

  # First things first, while the freshly minted bootstrap credential is
  # still live: log the first owner in and mint the operator token every
  # later step presents. Nothing else in this demo carries a human
  # credential.
  bootstrap_operator_session

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
    DEIS_RUNTIME_DATA_LISTEN="127.0.0.1:${DEIS_RUNTIME_PORT}" \
    "${BIN_DIR}/deis-runtime" \
    > >(bounded_log "${LOG_DIR}/deis-runtime.log") 2>&1 &
  DEIS_RUNTIME_PID=$!

  local runtime_rc
  runtime_rc=0
  wait_for_service wait_for_runtime 60 "$DEIS_RUNTIME_PID" 60 || runtime_rc=$?
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
  echo "- Runtime data port: https://127.0.0.1:${DEIS_RUNTIME_PORT} (mutual TLS; needs a fleet proxy certificate)"
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
  echo "- Runtime data port: https://127.0.0.1:${DEIS_RUNTIME_PORT:-unknown} (mutual TLS; needs a fleet proxy certificate)"
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
