#!/usr/bin/env bash
# Install or upgrade Deislet on this machine. Run as root from the staged
# payload that canary-release.sh rsyncs over.
#
# Existing /etc/deis/secrets.env is reused: a release must not rotate the
# master key or the API/node secrets out from under a live catalog. Pass
# --rewrite-config to regenerate the TOML files from those secrets (ports,
# bind addresses) without minting new keys.

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

BINS=(deis-control deis-runtime deis-proxy deis-store deis-queue deis-telemetry deis)

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

  NODE_PSK="$(openssl rand -hex 32)"
  API_KEY="$(openssl rand -hex 32)"
  STORAGE_AUTH="$(openssl rand -hex 32)"
  TELEMETRY_AUTH="$(openssl rand -hex 32)"
  MASTER_KEY="$(openssl rand -hex 32)"

  umask 077
  cat >"${secrets}" <<EOF
# Generated $(date -Is). Owner-only. Do not commit. Releases reuse this file.
LAN_IP=${LAN_IP}
ZONE=${ZONE}
NODE_ID=${NODE_ID}
PROXY_ID=${PROXY_ID}
NODE_PSK=${NODE_PSK}
API_KEY=${API_KEY}
STORAGE_AUTH=${STORAGE_AUTH}
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
node_preshared_keys = ["${NODE_PSK}"]
api_auth_enabled = true
api_keys = ["${API_KEY}"]
api_cors_allowed_origins = []
bundle_dir = "/var/lib/deis/control/bundles"
public_base_url = "http://${LAN_IP}:${CONTROL_PORT}"
secret_key_file = "/etc/deis/secret.key"
store_endpoint = "http://${LAN_IP}:${STORE_PORT}"
queue_endpoint = "http://${LAN_IP}:${QUEUE_PORT}"
storage_auth_secret = "${STORAGE_AUTH}"
deis_runtime_poll_interval = 15
node_registration_ttl_secs = 90
cron_tick_seconds = 60

[[deis_runtime_nodes]]
id = "${NODE_ID}"
addr = "http://${LAN_IP}:${RUNTIME_PORT}"
EOF

  cat >/etc/deis/runtime.toml <<EOF
node_id = "${NODE_ID}"
http_port = ${RUNTIME_PORT}
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

[control]
endpoint = "http://${LAN_IP}:${CONTROL_PORT}"
secret = "${NODE_PSK}"
request_timeout_secs = 30
retry_interval_secs = 15
reconcile_interval_secs = 60

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
secret = "${NODE_PSK}"
heartbeat_interval = 30

[routing]
control_poll_interval_secs = 5
preview_zone = "${ZONE}"
EOF

  cat >/etc/deis/store.toml <<EOF
listen = "0.0.0.0:${STORE_PORT}"
auth_secret = "${STORAGE_AUTH}"
data_dir = "/var/lib/deis/store"
snapshot_dir = "/var/lib/deis/store-snapshots"
snapshot_interval_secs = 86400
snapshot_retention = 7
shared_connections = 8
tenant_connections = 4
EOF

  cat >/etc/deis/queue.toml <<EOF
listen = "0.0.0.0:${QUEUE_PORT}"
auth_secret = "${STORAGE_AUTH}"
data_dir = "/var/lib/deis/queue"
connections = 8
EOF

  cat >/etc/deis/telemetry.toml <<EOF
listen = "0.0.0.0:${TELEMETRY_PORT}"
metrics_listen = "0.0.0.0:${TELEMETRY_METRICS_PORT}"
auth_secret = "${TELEMETRY_AUTH}"
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
  install -o root -g root -m 644 "${INSTALL_ROOT}/systemd/deislet.target" /etc/systemd/system/
  systemctl daemon-reload
  systemctl enable deis-telemetry deis-store deis-queue deis-control deis-runtime deis-proxy deislet.target
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

write_operator_env() {
  umask 077
  cat >/etc/deis/operator.env <<EOF
# Source this file to talk to this canary from a shell.
export DEIS_CONTROL_ENDPOINT=http://${LAN_IP}:${CONTROL_PORT}
export DEIS_CONTROL_TOKEN=${API_KEY}
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
token = "${API_KEY}"

[telemetry."http://${LAN_IP}:${TELEMETRY_PORT}"]
auth_secret = "${TELEMETRY_AUTH}"
EOF
    cat >/home/leo/.config/deis/env <<EOF
export DEIS_CONTROL_ENDPOINT=http://${LAN_IP}:${CONTROL_PORT}
export DEIS_CONTROL_TOKEN=${API_KEY}
export DEIS_TELEMETRY_ENDPOINT=http://${LAN_IP}:${TELEMETRY_PORT}
export DEIS_TELEMETRY_AUTH_SECRET=${TELEMETRY_AUTH}
export DEIS_QUEUE_ENDPOINT=http://${LAN_IP}:${QUEUE_PORT}
EOF
    chown -R leo:leo /home/leo/.config/deis
    chmod 700 /home/leo/.config/deis
    chmod 600 /home/leo/.config/deis/credentials.toml /home/leo/.config/deis/env
  fi
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
  log "  runtime     http://${LAN_IP}:${RUNTIME_PORT}/"
  log "  proxy       http://${LAN_IP}:${PROXY_PORT}/"
  log "  preview     http://<env>--<app>.${ZONE}/  (production: http://<app>.${ZONE}/)"
  log "  store gRPC  ${LAN_IP}:${STORE_PORT}"
  log "  queue gRPC  ${LAN_IP}:${QUEUE_PORT}"
  log "  telemetry   ${LAN_IP}:${TELEMETRY_PORT}  (metrics ${LAN_IP}:${TELEMETRY_METRICS_PORT})"
  log "credentials:  /etc/deis/operator.env"
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

if ((FIRST_INSTALL)) || ((REWRITE_CONFIG)); then
  write_master_key
  write_configs
  write_operator_env
  log "wrote configuration under /etc/deis"
else
  log "keeping existing /etc/deis/*.toml (pass --rewrite-config to regenerate)"
fi

install_bins
install_units
install_runtime_fs
write_release_stamp
ensure_hosts
start_stack
wait_http "http://127.0.0.1:${CONTROL_PORT}/health" deis-control
wait_http "http://127.0.0.1:${RUNTIME_PORT}/health" deis-runtime -H "x-deis-app-id: _system"
wait_tcp 127.0.0.1 "${STORE_PORT}" deis-store
wait_tcp 127.0.0.1 "${QUEUE_PORT}" deis-queue
wait_tcp 127.0.0.1 "${TELEMETRY_PORT}" deis-telemetry
wait_tcp 127.0.0.1 "${PROXY_PORT}" deis-proxy
print_status
