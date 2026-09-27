#!/usr/bin/env bash
# Install or upgrade Deislet on this machine. Run as root from the staged
# payload that canary-release.sh rsyncs over.
#
# Existing /etc/deis/secrets.env is reused: a release must not rotate the
# master key or the storage/telemetry secrets out from under a live catalog.
# Pass --rewrite-config to regenerate the TOML files from those secrets
# (ports, bind addresses) without minting new keys.
#
# No human credential is configured anywhere. A first start of a control
# plane whose database holds no live token mints a one-time bootstrap token
# into /var/lib/deis/control/bootstrap.token; `bootstrap_login` below trades
# it for the first owner account and mints the scoped operator token that
# /etc/deis/operator.env and the CLI credentials carry.

set -euo pipefail

LAN_IP="${LAN_IP:-192.168.213.90}"
ZONE="${ZONE:-s90.deislet.test}"
NODE_ID="${NODE_ID:-s90-canary}"
PROXY_ID="${PROXY_ID:-s90-canary-proxy}"
REWRITE_CONFIG=0
INSTALL_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

CONTROL_PORT="${CONTROL_PORT:-8080}"
RUNTIME_PORT="${RUNTIME_PORT:-8086}"
PROXY_PORT="${PROXY_PORT:-80}"
STORE_PORT="${STORE_PORT:-8090}"
QUEUE_PORT="${QUEUE_PORT:-8091}"
TELEMETRY_PORT="${TELEMETRY_PORT:-8092}"
TELEMETRY_METRICS_PORT="${TELEMETRY_METRICS_PORT:-8093}"

# The two listeners Deislet's own processes talk to each other on. They are
# separate from the ports above because what is served on them is different in
# kind: an assignment list carries every environment's decrypted secrets, and
# the routing table is a map of every hostname on the installation. A caller is
# admitted there by a certificate naming a machine of this installation, and by
# nothing else.
CONTROL_MACHINE_PORT="${CONTROL_MACHINE_PORT:-8084}"
RUNTIME_MACHINE_PORT="${RUNTIME_MACHINE_PORT:-8087}"

# The name every machine here is issued under. It is not resolved and nothing
# connects to it; it is the first half of a SPIFFE name, and its only job is to
# be the same on every certificate this installation trusts.
FLEET_TRUST_DOMAIN="${FLEET_TRUST_DOMAIN:-canary.deislet.internal}"
PKI_DIR=/etc/deis/pki

BINS=(deis-control deis-runtime deis-proxy deis-store deis-queue deis-telemetry deis-backend-topology deis)

usage() {
  cat <<'EOF'
Usage: remote-install.sh [--rewrite-config]

  --rewrite-config  regenerate /etc/deis/*.toml from secrets.env
                    (never mints new keys)
EOF
}

log() { printf '%s\n' "$*"; }
die() { printf 'error: %s\n' "$*" >&2; exit 1; }

while [[ $# -gt 0 ]]; do
  case "$1" in
    --rewrite-config) REWRITE_CONFIG=1; shift ;;
    -h|--help) usage; exit 0 ;;
    *) die "unknown argument: $1" ;;
  esac
done

need_root() {
  if [[ ${EUID} -ne 0 ]]; then
    die "run as root (sudo $0)"
  fi
}

require_bins() {
  local missing=()
  local name
  for name in "${BINS[@]}"; do
    if [[ ! -x "${INSTALL_ROOT}/bin/${name}" ]]; then
      missing+=("${name}")
    fi
  done
  if ((${#missing[@]})); then
    die "missing binaries in ${INSTALL_ROOT}/bin: ${missing[*]}"
  fi
}

ensure_user() {
  if ! getent passwd deis >/dev/null; then
    useradd --system --home-dir /var/lib/deis --shell /usr/sbin/nologin --user-group deis
    log "created system user deis"
  fi
}

ensure_dirs() {
  mkdir -p \
    /etc/deis \
    /var/lib/deis/control/bundles \
    /var/lib/deis/runtime \
    /var/lib/deis/proxy \
    /var/lib/deis/store \
    /var/lib/deis/store-snapshots \
    /var/lib/deis/queue \
    /var/lib/deis/telemetry
  chmod 750 /etc/deis
  chmod 700 /var/lib/deis /var/lib/deis/control
  chown -R deis:deis /var/lib/deis
  chown deis:deis /etc/deis
}

ensure_secrets() {
  local secrets=/etc/deis/secrets.env
  if [[ -f ${secrets} ]]; then
    log "reusing existing ${secrets}"
    # shellcheck disable=SC1090
    source "${secrets}"
    return
  fi

  TELEMETRY_AUTH="$(openssl rand -hex 32)"
  MASTER_KEY="$(openssl rand -hex 32)"

  umask 077
  cat >"${secrets}" <<EOF
# Generated $(date -Is). Owner-only. Do not commit. Releases reuse this file.
LAN_IP=${LAN_IP}
ZONE=${ZONE}
NODE_ID=${NODE_ID}
PROXY_ID=${PROXY_ID}
TELEMETRY_AUTH=${TELEMETRY_AUTH}
MASTER_KEY=${MASTER_KEY}
CONTROL_PORT=${CONTROL_PORT}
RUNTIME_PORT=${RUNTIME_PORT}
PROXY_PORT=${PROXY_PORT}
STORE_PORT=${STORE_PORT}
QUEUE_PORT=${QUEUE_PORT}
TELEMETRY_PORT=${TELEMETRY_PORT}
TELEMETRY_METRICS_PORT=${TELEMETRY_METRICS_PORT}
EOF
  chown deis:deis "${secrets}"
  chmod 600 "${secrets}"
  log "wrote ${secrets}"
}

# ---- the installation's certificate authority ----------------------------
#
# Control, node and proxy all live on this one machine, so one authority minted
# here is enough for them to prove who they are to each other. A multi-machine
# installation would put the authority somewhere with a life longer than any
# one host and hand each machine only its own leaf and the root; nothing in
# Deislet knows or cares where the three files came from, so that substitution
# costs nothing here.
#
# Two things are easy to get wrong and expensive to diagnose:
#
#   * the extensions go in an -extfile, not -addext. `openssl x509 -req` does
#     not carry extensions over from the signing request unless given
#     -copy_extensions, which is not spelled the same way across openssl
#     versions. Get it wrong and every certificate is signed with no URI
#     subject alternative name at all, which surfaces much later as a process
#     reporting that its certificate names no machine.
#   * every leaf carries both serverAuth and clientAuth, because each process
#     here is a server on one connection and a client on another: the control
#     plane serves the machine channel and dials its nodes, and a node serves
#     its management plane and dials the control plane.
#
# Like secrets.env, existing files are reused: a release must not hand a live
# installation new names. Delete /etc/deis/pki to start over, and remember that
# every process has to be restarted to pick the new root up.
mint_ca() {
  local prefix=$1
  openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:P-256 -nodes \
    -keyout "${prefix}.key" -out "${prefix}.crt" -days 3650 \
    -subj "/CN=${FLEET_TRUST_DOMAIN} fleet authority" >/dev/null 2>&1
  chmod 600 "${prefix}.key"
}

# mint_leaf <leaf-prefix> <spiffe-path> [extra-san]
mint_leaf() {
  local leaf=$1
  local path=$2
  local extra=${3:-}
  local san="URI:spiffe://${FLEET_TRUST_DOMAIN}${path}"
  if [[ -n ${extra} ]]; then
    san="${san},${extra}"
  fi
  # The subject is decoration and deliberately not the SPIFFE name: the
  # identity lives in the URI subject alternative name, which is the only thing
  # the platform reads, and openssl parses a `/` in a subject string as the
  # separator between one relative distinguished name and the next - so a
  # `spiffe://` subject is refused outright.
  openssl req -newkey ec -pkeyopt ec_paramgen_curve:P-256 -nodes \
    -keyout "${leaf}.key" -out "${leaf}.csr" -subj "/CN=${leaf##*/}" \
    >/dev/null 2>&1
  cat > "${leaf}.ext" <<EXT
subjectAltName=${san}
basicConstraints=CA:FALSE
extendedKeyUsage=serverAuth,clientAuth
EXT
  openssl x509 -req -in "${leaf}.csr" -CA "${PKI_DIR}/ca.crt" \
    -CAkey "${PKI_DIR}/ca.key" -CAcreateserial -days 825 \
    -extfile "${leaf}.ext" -out "${leaf}.crt" >/dev/null 2>&1
  chmod 600 "${leaf}.key"
  rm -f "${leaf}.csr" "${leaf}.ext"
}

ensure_pki() {
  if [[ -f ${PKI_DIR}/ca.crt ]]; then
    log "reusing existing ${PKI_DIR}"
    return
  fi
  command -v openssl >/dev/null 2>&1 || die "openssl is needed to issue this installation's certificates"

  mkdir -p "${PKI_DIR}"
  chmod 700 "${PKI_DIR}"
  mint_ca "${PKI_DIR}/ca"

  # Each leaf carries the address its own listener answers on, because a peer
  # dialling it checks the name it dialled as well as the certificate: an IP
  # address reached over a name it does not carry is refused at the handshake.
  mint_leaf "${PKI_DIR}/control" "/deislet/control" "IP:${LAN_IP},IP:127.0.0.1"
  mint_leaf "${PKI_DIR}/node" "/deislet/node/${NODE_ID}" "IP:${LAN_IP},IP:127.0.0.1"
  mint_leaf "${PKI_DIR}/proxy" "/deislet/proxy/${PROXY_ID}"

  chown -R deis:deis "${PKI_DIR}"
  log "issued ${PKI_DIR}: one authority and three machines under spiffe://${FLEET_TRUST_DOMAIN}"
}

write_master_key() {
  printf '%s\n' "${MASTER_KEY}" >/etc/deis/secret.key
  chown deis:deis /etc/deis/secret.key
  chmod 600 /etc/deis/secret.key
}

write_configs() {
  umask 077

  cat >/etc/deis/control.toml <<EOF
bind_addr = "0.0.0.0"
port = ${CONTROL_PORT}
heartbeat_interval = 15
heartbeat_timeout = 90
# Where this installation keeps its own state. Notably the one-time
# bootstrap token: a first start with no credential anywhere in the database
# mints one here, 0600, and bootstrap_login below logs the first owner in
# through it.
data_dir = "/var/lib/deis/control"
api_cors_allowed_origins = []
bundle_dir = "/var/lib/deis/control/bundles"
public_base_url = "http://${LAN_IP}:${CONTROL_PORT}"
secret_key_file = "/etc/deis/secret.key"
store_endpoint = "http://${LAN_IP}:${STORE_PORT}"
queue_endpoint = "http://${LAN_IP}:${QUEUE_PORT}"
deis_runtime_poll_interval = 15
node_registration_ttl_secs = 90
cron_tick_seconds = 60

[[deis_runtime_nodes]]
id = "${NODE_ID}"
addr = "http://${LAN_IP}:${RUNTIME_PORT}"

# The listener this installation's own processes talk to. Registration,
# assignments, bundle downloads and the routing table are served here and
# nowhere else, and a caller is admitted only by a certificate that chains to
# the root below and names a machine of this installation. Leave it out and
# no node can register and no proxy can fetch a routing table.
[machine]
bind_addr = "0.0.0.0"
port = ${CONTROL_MACHINE_PORT}
certificate = "${PKI_DIR}/control.crt"
private_key = "${PKI_DIR}/control.key"
trust_root = "${PKI_DIR}/ca.crt"
# The longest a renewal can go unnoticed. Far shorter than any certificate
# lifetime in use, and a rotation needs no restart.
identity_reload_seconds = 60
# Where nodes reach this listener, which is what bundle URLs in an assignment
# are built from. public_base_url above names where a browser finds the
# dashboard, and that is the wrong address to hand a node.
public_base_url = "https://${LAN_IP}:${CONTROL_MACHINE_PORT}"
EOF

  cat >/etc/deis/runtime.toml <<EOF
node_id = "${NODE_ID}"
# The data port answers only mutual TLS: every caller is verified against the
# fleet root and must present a certificate naming a proxy of this
# installation, which is what deis-proxy forwards with and what the health
# wait below presents.
data_listen = "0.0.0.0:${RUNTIME_PORT}"
data_dir = "/var/lib/deis/runtime"
shutdown_grace_period_secs = 30
max_request_body_bytes = 33554432
default_memory_limit_mb = 256

[default_execution_limits]
timeout_ms = 30000
cpu_time_limit_ms = 5000
wait_until_timeout_ms = 30000
stream_timeout_ms = 300000
rate_limit_ops_per_sec = 1000.0
max_subrequests = 50

[logging]
enabled = true
endpoint = "http://${LAN_IP}:${TELEMETRY_PORT}"
auth_secret = "${TELEMETRY_AUTH}"
buffer_size = 10000
flush_interval_secs = 5
batch_size = 100
max_retries = 3
max_pending_entries = 10000

# On startup the node asks the control plane which applications and versions
# it should be running, downloads each bundle itself, and only reports healthy
# once they are loaded. There is no secret line and that is the finished
# state rather than an omission: the assignment list carries every
# environment's decrypted secrets, and what entitles this node to it is the
# certificate below, which names this one node.
[control]
endpoint = "http://${LAN_IP}:${CONTROL_PORT}"
request_timeout_secs = 30
retry_interval_secs = 15
reconcile_interval_secs = 60

# This node's own identity, and where the machine channel is. Registration,
# assignments and bundle downloads all go over it, under a certificate naming
# this node and no other.
[machine_identity]
control_endpoint = "https://${LAN_IP}:${CONTROL_MACHINE_PORT}"
certificate = "${PKI_DIR}/node.crt"
private_key = "${PKI_DIR}/node.key"
trust_root = "${PKI_DIR}/ca.crt"
trust_domain = "${FLEET_TRUST_DOMAIN}"
# The other direction: what the control plane pushes into this node arrives
# here, admitted only if the connection proved the control plane's own name.
management_listen = "${LAN_IP}:${RUNTIME_MACHINE_PORT}"

[bundles]
max_bytes = 268435456
download_timeout_secs = 300

[assets]
cache_control = "public, max-age=0, must-revalidate"
max_bytes = 134217728

[fetch]
allow = []
proxy_from_env = false
EOF

  cat >/etc/deis/proxy.toml <<EOF
daemon = false
pid_file = "/var/lib/deis/proxy/deis-proxy.pid"
upgrade_sock = "/var/lib/deis/proxy/deis_proxy_upgrade.sock"
database = "/var/lib/deis/proxy/sites.db"
listen_addr = ["0.0.0.0"]
listen_ports = [${PROXY_PORT}]
balancing_strategy = "round_robin"
health_check_interval_secs = 5
upstream_connect_timeout_secs = 5
upstream_read_timeout_secs = 30
grace_period_secs = 10
graceful_shutdown_timeout_secs = 5

[[upstreams]]
host = "${LAN_IP}"
port = ${RUNTIME_PORT}
weight = 1

[https]
auto_redirect_to_https = false
auto_offload_https = true

[health_check]
mode = "http"
path = "/health"
app_id = "_system"
timeout_secs = 2
healthy_after = 1
unhealthy_after = 2
readiness = "reported"

[compression]
enabled = true
algo = "gzip"
level = 6

[log.error_log]
level = "info"

[control]
addr = "http://${LAN_IP}:${CONTROL_PORT}"
associated_id = "${PROXY_ID}"
heartbeat_interval = 30

# The proxy's identity on the machine channel. The routing table is fetched
# over it, and the acknowledgement this proxy sends - which table version it is
# already running - is held against the name this connection proved, so no
# proxy can acknowledge on another's behalf.
[machine_identity]
control_endpoint = "https://${LAN_IP}:${CONTROL_MACHINE_PORT}"
certificate = "${PKI_DIR}/proxy.crt"
private_key = "${PKI_DIR}/proxy.key"
trust_root = "${PKI_DIR}/ca.crt"
trust_domain = "${FLEET_TRUST_DOMAIN}"

[routing]
control_poll_interval_secs = 5
preview_zone = "${ZONE}"
EOF

  # deis-store and deis-queue hold no secret: every call carries a capability
  # the control plane signed, checked against the public keys they fetch from
  # its JWKS (and refresh on their own when it rotates). Both default to the
  # control plane's default port, so both addresses are spelled out.
  cat >/etc/deis/store.toml <<EOF
listen = "0.0.0.0:${STORE_PORT}"
capability_jwks_url = "http://127.0.0.1:${CONTROL_PORT}/.well-known/deis/capability-jwks.json"
revocation_list_url = "http://127.0.0.1:${CONTROL_PORT}"
data_dir = "/var/lib/deis/store"
snapshot_dir = "/var/lib/deis/store-snapshots"
snapshot_interval_secs = 86400
snapshot_retention = 7
shared_connections = 8
tenant_connections = 4
EOF

  cat >/etc/deis/queue.toml <<EOF
listen = "0.0.0.0:${QUEUE_PORT}"
capability_jwks_url = "http://127.0.0.1:${CONTROL_PORT}/.well-known/deis/capability-jwks.json"
revocation_list_url = "http://127.0.0.1:${CONTROL_PORT}"
data_dir = "/var/lib/deis/queue"
connections = 8
EOF

  # deis-telemetry takes no shared secret: it verifies control-signed
  # capabilities by capability_public_key, and without one it does not
  # enforce. The node still presents a token derived from TELEMETRY_AUTH
  # ([logging] above), which is accepted either way.
  cat >/etc/deis/telemetry.toml <<EOF
listen = "0.0.0.0:${TELEMETRY_PORT}"
metrics_listen = "0.0.0.0:${TELEMETRY_METRICS_PORT}"
data_dir = "/var/lib/deis/telemetry"
log_retention_days = 7
metric_retention_days = 30
trace_retention_days = 3
EOF

  chown deis:deis /etc/deis/*.toml /etc/deis/secret.key
  chmod 600 /etc/deis/*.toml /etc/deis/secret.key
}

install_bins() {
  local name
  for name in "${BINS[@]}"; do
    install -o root -g root -m 755 "${INSTALL_ROOT}/bin/${name}" "/usr/local/bin/${name}"
  done
  log "installed binaries to /usr/local/bin"
}

install_units() {
  install -o root -g root -m 644 "${INSTALL_ROOT}/systemd/"*.service /etc/systemd/system/
  install -o root -g root -m 644 "${INSTALL_ROOT}/systemd/deislet.slice" /etc/systemd/system/
  install -o root -g root -m 644 "${INSTALL_ROOT}/systemd/deislet.target" /etc/systemd/system/
  systemctl daemon-reload
  systemctl enable deis-telemetry deis-store deis-queue deis-control deis-runtime deis-proxy deislet.target
}

# Build the runtime's startup snapshot, before the (hardened) service needs
# one.
#
# deis-runtime.service runs with ProtectHome=true, so the service itself
# cannot read the isolate JS it loads from the install tree -- building a
# startup snapshot means exactly that read. This runs the same binary the
# service will run, as root, with the snapshot path the unit's environment
# names: the resulting file is everything the hardened service starts from,
# and a snapshot that does not match the binary fails the start loudly
# instead of serving from something unhardened. Must run after install_bins
# and install_runtime_fs, and before start_stack.
warm_runtime_snapshot() {
  local snapshot=/var/lib/deis/runtime/startup-snapshot.bin
  install -o deis -g deis -m 755 -d /var/lib/deis/runtime
  DEIS_RUNTIME_SNAPSHOT_PATH="${snapshot}" /usr/local/bin/deis-runtime \
    --config /etc/deis/runtime.toml --warm-snapshot \
    || die "the runtime startup snapshot could not be built; the hardened service would fail to start"
  chmod 640 "${snapshot}"
  chown deis:deis "${snapshot}"
  log "runtime startup snapshot is warm at ${snapshot}"
}

install_runtime_fs() {
  local src="${INSTALL_ROOT}/runtime-fs"
  if [[ ! -d ${src} ]]; then
    die "missing ${src}: deis-runtime loads isolate JS from compile-time paths"
  fi
  # Contents are absolute paths with the leading slash stripped
  # (ext/..., home/l/.cargo/...). Overlay onto /.
  rsync -a "${src}/" /
  if [[ -d /ext/deislet ]]; then
    chmod -R a+rX /ext/deislet
  fi
  if [[ -d /home/l/.cargo ]]; then
    chmod 755 /home /home/l /home/l/.cargo /home/l/.cargo/registry /home/l/.cargo/registry/src || true
    chmod -R a+rX /home/l/.cargo/registry/src
  fi
  log "installed isolate JS at compile-time paths"
}

# The credential a person - the `deis` CLI, a dashboard - presents to the
# control plane's human API. It is minted, not configured: the operator token
# this writes is a scoped one created from the first owner account, which the
# bootstrap login below creates. Runs after the stack is up, because that is
# when the bootstrap token exists.
write_operator_env() {
  local operator_token=$1
  umask 077
  cat >/etc/deis/operator.env <<EOF
# Source this file to talk to this canary from a shell.
export DEIS_CONTROL_ENDPOINT=http://${LAN_IP}:${CONTROL_PORT}
export DEIS_CONTROL_TOKEN=${operator_token}
export DEIS_TELEMETRY_ENDPOINT=http://${LAN_IP}:${TELEMETRY_PORT}
export DEIS_TELEMETRY_AUTH_SECRET=${TELEMETRY_AUTH}
export DEIS_QUEUE_ENDPOINT=http://${LAN_IP}:${QUEUE_PORT}
EOF
  chown deis:deis /etc/deis/operator.env
  chmod 600 /etc/deis/operator.env

  if getent passwd leo >/dev/null; then
    mkdir -p /home/leo/.config/deis
    cat >/home/leo/.config/deis/credentials.toml <<EOF
[endpoints."http://${LAN_IP}:${CONTROL_PORT}"]
token = "${operator_token}"

[telemetry."http://${LAN_IP}:${TELEMETRY_PORT}"]
auth_secret = "${TELEMETRY_AUTH}"
EOF
    cat >/home/leo/.config/deis/env <<EOF
export DEIS_CONTROL_ENDPOINT=http://${LAN_IP}:${CONTROL_PORT}
export DEIS_CONTROL_TOKEN=${operator_token}
export DEIS_TELEMETRY_ENDPOINT=http://${LAN_IP}:${TELEMETRY_PORT}
export DEIS_TELEMETRY_AUTH_SECRET=${TELEMETRY_AUTH}
export DEIS_QUEUE_ENDPOINT=http://${LAN_IP}:${QUEUE_PORT}
EOF
    chown -R leo:leo /home/leo/.config/deis
    chmod 700 /home/leo/.config/deis
    chmod 600 /home/leo/.config/deis/credentials.toml /home/leo/.config/deis/env
  fi
}

# Trade the one-time bootstrap credential for the operator credential.
#
# A first start with an empty database wrote /var/lib/deis/control/
# bootstrap.token, 0600, good for exactly one thing: creating the first owner
# account and its first token. `deis login --bootstrap` does that, and the
# control plane revokes the bootstrap credential and deletes the file the
# moment the login uses it. From the owner account this then mints the
# scoped operator token the operator.env and CLI credentials carry - admin
# over the platform plus the deploy, secrets and storage verbs, because an
# installation's operator does all of those - and never writes the bootstrap
# secret itself anywhere.
#
# Root's own login token stays in /root/.config/deis, which is why every CLI
# call here names DEIS_CONFIG_HOME: the operator.env token is minted by the
# CLI that holds the login credential, and spelling it out keeps the two
# stores apart.
bootstrap_login() {
  local bootstrap_file=/var/lib/deis/control/bootstrap.token
  local waited=0
  while [[ ! -f ${bootstrap_file} && ${waited} -lt 60 ]]; do
    sleep 0.5
    waited=$((waited + 1))
  done
  if [[ ! -f ${bootstrap_file} ]]; then
    dump_unit deis-control
    die "control wrote no bootstrap token at ${bootstrap_file}; there is no way to log in"
  fi

  DEIS_CONTROL_ENDPOINT="http://${LAN_IP}:${CONTROL_PORT}" DEIS_CONFIG_HOME=/root/.config/deis \
    /usr/local/bin/deis login --bootstrap "${bootstrap_file}" \
    || die "deis login --bootstrap failed"

  local owner operator_token
  owner=$(DEIS_CONTROL_ENDPOINT="http://${LAN_IP}:${CONTROL_PORT}" DEIS_CONFIG_HOME=/root/.config/deis \
    /usr/local/bin/deis token list --json 2>/dev/null \
    | python3 -c 'import json,sys; tokens=json.load(sys.stdin).get("tokens",[]); print(tokens[0]["account"] if tokens else "")') \
    || owner=""
  [[ -n ${owner} ]] || die "could not read back the owner account the bootstrap login created"

  operator_token=$(DEIS_CONTROL_ENDPOINT="http://${LAN_IP}:${CONTROL_PORT}" DEIS_CONFIG_HOME=/root/.config/deis \
    /usr/local/bin/deis token create --account "${owner}" canary-operator \
      --scope 'admin:*/*' --scope 'deploy:*/*' --scope 'secrets:*/*' \
      --scope "storage:demo/production" --scope "storage:demo/preview" \
      --json 2>/dev/null \
    | python3 -c 'import json,sys; print(json.load(sys.stdin).get("secret",""))') \
    || operator_token=""
  [[ -n ${operator_token} ]] || die "could not mint the scoped operator token"

  write_operator_env "${operator_token}"

  if [[ -f ${bootstrap_file} ]]; then
    die "${bootstrap_file} still exists after login; the bootstrap credential should have been revoked and deleted by its first use"
  fi
  log "logged the first owner (${owner}) in through the bootstrap token and minted the scoped operator credential"
}

write_release_stamp() {
  umask 022
  cat >/var/lib/deis/RELEASE <<EOF
sha=${RELEASE_SHA:-unknown}
built_at=${RELEASE_TIME:-unknown}
installed_at=$(date -Is)
host=$(hostname)
EOF
  chown deis:deis /var/lib/deis/RELEASE
  chmod 644 /var/lib/deis/RELEASE
}

ensure_hosts() {
  local line="${LAN_IP} ${NODE_ID} demo.${ZONE} preview--demo.${ZONE} demo-auth.${ZONE}"
  if grep -qE "[[:space:]]demo\\.${ZONE}([[:space:]]|$)" /etc/hosts; then
    sed -i -E "s|^[0-9.]+[[:space:]].*demo\\.${ZONE}.*$|${line}|" /etc/hosts
  else
    printf '%s\n' "${line}" >>/etc/hosts
  fi
}

# Ask each binary whether the file just written to it is usable, before
# anything is started against it.
#
# The failure this closes is the quiet one. These TOML files are written by
# hand, by the heredocs above, and serde ignores a field it does not know - so
# a setting spelled wrong is not an error, it is an absence, and the service
# starts on a default nobody chose. Under the machine channel that is
# expensive: a control plane whose `[machine]` section did not parse serves no
# node at all, and it says so only as six other services failing to attach for
# reasons none of them can name.
#
# Run before `start_stack` so an install that would not have worked stops here,
# at the file, rather than after six `systemctl restart` calls.
check_configs() {
  local failed=0
  /usr/local/bin/deis-control --config /etc/deis/control.toml --check-config \
    || { log "error: /etc/deis/control.toml is not usable"; failed=1; }
  /usr/local/bin/deis-runtime --config /etc/deis/runtime.toml --check-config \
    || { log "error: /etc/deis/runtime.toml is not usable"; failed=1; }
  if ((failed)); then
    die "the configuration written by this installer does not load; nothing was started"
  fi
  log "configuration checks passed"
}

start_stack() {
  systemctl restart deis-telemetry deis-store deis-queue
  systemctl restart deis-control
  systemctl restart deis-runtime
  systemctl restart deis-proxy
}

dump_unit() {
  local unit=$1
  systemctl status "${unit}" --no-pager -l || true
  journalctl -u "${unit}" -n 80 --no-pager || true
}

wait_http() {
  local url=$1
  local unit=$2
  shift 2
  if curl -fsS --max-time 2 --retry 45 --retry-delay 2 --retry-all-errors "$@" "${url}" >/dev/null; then
    log "ready: ${unit} (${url})"
    return 0
  fi
  dump_unit "${unit}"
  die "${unit} did not become ready at ${url}"
}

wait_tcp() {
  local host=$1
  local port=$2
  local unit=$3
  if python3 - "${host}" "${port}" "${unit}" <<'PY'
import socket, sys, subprocess, time
host, port, unit = sys.argv[1], int(sys.argv[2]), sys.argv[3]
deadline = time.monotonic() + 90
while time.monotonic() < deadline:
    try:
        with socket.create_connection((host, port), timeout=1):
            sys.exit(0)
    except OSError:
        active = subprocess.run(
            ["systemctl", "is-active", "--quiet", unit]
        ).returncode == 0
        if not active:
            sys.exit(2)
        time.sleep(0.5)
sys.exit(1)
PY
  then
    log "ready: ${unit} (${host}:${port})"
    return 0
  fi
  dump_unit "${unit}"
  die "${unit} did not listen on ${host}:${port}"
}

print_status() {
  systemctl is-active deis-telemetry deis-store deis-queue deis-control deis-runtime deis-proxy
  log ""
  log "addresses:"
  log "  console     http://${LAN_IP}:${CONTROL_PORT}/"
  log "  control API http://${LAN_IP}:${CONTROL_PORT}/api"
  log "  health      http://${LAN_IP}:${CONTROL_PORT}/health"
  log "  runtime     https://${LAN_IP}:${RUNTIME_PORT}/  (mutual TLS; a fleet proxy certificate is required)"
  log "  proxy       http://${LAN_IP}:${PROXY_PORT}/"
  log "  preview     http://<env>--<app>.${ZONE}/  (production: http://<app>.${ZONE}/)"
  log "  store gRPC  ${LAN_IP}:${STORE_PORT}"
  log "  queue gRPC  ${LAN_IP}:${QUEUE_PORT}"
  log "  telemetry   ${LAN_IP}:${TELEMETRY_PORT}  (metrics ${LAN_IP}:${TELEMETRY_METRICS_PORT})"
  log "credentials:  /etc/deis/operator.env (minted through the bootstrap token;"
  log "              a fresh database that has not been logged in yet keeps its token at /var/lib/deis/control/bootstrap.token)"
  if [[ -f /var/lib/deis/RELEASE ]]; then
    log "release:"
    sed 's/^/  /' /var/lib/deis/RELEASE
  fi
}

need_root
require_bins
ensure_user
ensure_dirs

FIRST_INSTALL=0
if [[ ! -f /etc/deis/secrets.env ]]; then
  FIRST_INSTALL=1
fi

ensure_secrets
# shellcheck disable=SC1091
source /etc/deis/secrets.env
ensure_pki

if ((FIRST_INSTALL)) || ((REWRITE_CONFIG)); then
  write_master_key
  write_configs
  log "wrote configuration under /etc/deis"
else
  log "keeping existing /etc/deis/*.toml (pass --rewrite-config to regenerate)"
fi

install_bins
install_units
install_runtime_fs
write_release_stamp
ensure_hosts
check_configs
warm_runtime_snapshot
start_stack
wait_http "http://127.0.0.1:${CONTROL_PORT}/health" deis-control
# The data port admits only a proxy's certificate, so its readiness probe
# carries this installation's proxy leaf and verifies the node against the
# fleet root - the same handshake a forwarded request gets.
if curl -fsS --max-time 2 --retry 45 --retry-delay 2 --retry-all-errors \
  --cacert "${PKI_DIR}/ca.crt" \
  --cert "${PKI_DIR}/proxy.crt" --key "${PKI_DIR}/proxy.key" \
  -H "x-deis-app-id: _system" \
  "https://127.0.0.1:${RUNTIME_PORT}/health" >/dev/null; then
  log "ready: deis-runtime (https://127.0.0.1:${RUNTIME_PORT}/health, mutual TLS)"
else
  dump_unit deis-runtime
  die "deis-runtime did not become ready at https://127.0.0.1:${RUNTIME_PORT}/health"
fi
wait_tcp 127.0.0.1 "${STORE_PORT}" deis-store
wait_tcp 127.0.0.1 "${QUEUE_PORT}" deis-queue
wait_tcp 127.0.0.1 "${TELEMETRY_PORT}" deis-telemetry
wait_tcp 127.0.0.1 "${PROXY_PORT}" deis-proxy

# Only a first install (or a config rewrite) has a fresh database behind the
# control plane, and only a database with no live token gets a bootstrap
# token. An upgrade keeps the database, and with it every credential it
# issued - operator.env already holds a live token, so nothing is minted.
if ((FIRST_INSTALL)) || ((REWRITE_CONFIG)); then
  bootstrap_login
fi

print_status
