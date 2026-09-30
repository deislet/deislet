#!/usr/bin/env bash
# Release Deislet onto the LAN canary (default: s90 / 192.168.213.90).
#
# Repeated releases replace binaries and restart systemd. They do not rotate
# keys or wipe /var/lib/deis. Typical loop after a change:
#
#   ./scripts/canary-release.sh release
#
# First machine, or to also push the example apps:
#
#   ./scripts/canary-release.sh release --apps

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
CANARY_DIR="${ROOT}/scripts/canary"
BIN_DIR="${CARGO_TARGET_DIR:-${ROOT}/target}/release"
SSH_HOST="${SSH_HOST:-s90}"
LAN_IP="${LAN_IP:-192.168.213.90}"
ZONE="${ZONE:-s90.deislet.test}"
NODE_ID="${NODE_ID:-s90-canary}"
PROXY_ID="${PROXY_ID:-${NODE_ID}-proxy}"
STAGE="${STAGE:-${TMPDIR:-/tmp}/deislet-canary-${SSH_HOST}}"
REMOTE_STAGE="${REMOTE_STAGE:-/var/tmp/deislet-canary}"
REWRITE_CONFIG=0
WITH_APPS=0
BINS=(deis-control deis-runtime deis-proxy deis-store deis-queue deis-telemetry deis-backend-topology deis)

# The control plane's continuous backup, shipped with every release so the
# canary never runs without it. Pinned: the configuration in
# scripts/canary/litestream.yml, the replica format and the restore drill are
# written for the 0.5 series, and the digests below are the ones the v0.5.17
# release publishes in its checksums.txt.
LITESTREAM_VERSION=0.5.17
LITESTREAM_ARCH="${LITESTREAM_ARCH:-x86_64}"
LITESTREAM_CACHE="${LITESTREAM_CACHE:-${XDG_CACHE_HOME:-${HOME}/.cache}/deislet}"
declare -A LITESTREAM_SHA256=(
  [x86_64]=cfb371176d164437ae869f8351cfde49bd1804ae71c61923f75c9cba9c9c006d
  [arm64]=f8ca4a050095c1efbda2c4365172e61bf9d955ea0d9ac42f448b52e51819baa5
)

log() { printf '%s\n' "$*"; }
die() { printf 'error: %s\n' "$*" >&2; exit 1; }

usage() {
  cat <<EOF
Usage: $(basename "$0") <command> [options]

Commands:
  build      console + cargo --release of the six services and \`deis\`
  ship       rsync binaries, units and isolate JS onto the host
  install    root install/upgrade on the host (keeps secrets)
  verify     probe health and, if demo is deployed, /hello/world
  status     systemd state, listening ports, RELEASE stamp
  apps       deploy demo-auth + demo (production and preview)
  credentials
             copy operator.env into ~/.config/deis on this machine
  release    build + ship + install + verify
             (add --apps to also deploy the example applications)

Options:
  --host <ssh>       SSH alias or address (default: ${SSH_HOST})
  --lan-ip <ip>      address published to nodes and operators (default: ${LAN_IP})
  --zone <dns>       proxy preview_zone (default: ${ZONE})
  --node-id <id>     runtime node_id (default: ${NODE_ID})
  --rewrite-config   regenerate /etc/deis/*.toml from secrets.env
  --apps             with release: also run the apps step
  -h, --help

Environment: SSH_HOST, LAN_IP, ZONE, NODE_ID, CARGO_TARGET_DIR, STAGE,
LITESTREAM_ARCH (the canary's CPU: x86_64 or arm64), LITESTREAM_CACHE.
Build and ship from the same workstation: deis-runtime still opens isolate
JS at the absolute paths recorded when it was compiled.
EOF
}

ssh_host() {
  ssh -o BatchMode=yes -o ConnectTimeout=10 "${SSH_HOST}" "$@"
}

parse_global() {
  while [[ $# -gt 0 ]]; do
    case "$1" in
      --host) SSH_HOST="$2"; STAGE="${TMPDIR:-/tmp}/deislet-canary-${SSH_HOST}"; shift 2 ;;
      --lan-ip) LAN_IP="$2"; shift 2 ;;
      --zone) ZONE="$2"; shift 2 ;;
      --node-id) NODE_ID="$2"; PROXY_ID="${NODE_ID}-proxy"; shift 2 ;;
      --rewrite-config) REWRITE_CONFIG=1; shift ;;
      --apps) WITH_APPS=1; shift ;;
      -h|--help) usage; exit 0 ;;
      --) shift; break ;;
      -*) die "unknown option: $1" ;;
      *) break ;;
    esac
  done
  REMAINING=("$@")
}

cmd_build() {
  command -v cargo >/dev/null || die "cargo is not on PATH"
  command -v npm >/dev/null || die "npm is not on PATH (needed to embed the console)"
  local web="${ROOT}/crates/deis-control/web"
  log "==> console"
  (
    cd "${web}"
    if [[ ! -d node_modules ]]; then
      npm ci
    fi
    npm run build
  )
  log "==> cargo --release"
  (
    cd "${ROOT}"
    cargo build --release --locked \
      -p deis-control -p deis-runtime -p deis-proxy \
      -p deis-store -p deis-queue -p deis-telemetry -p deis-backend-topology -p deis
  )
  local name
  for name in "${BINS[@]}"; do
    [[ -x ${BIN_DIR}/${name} ]] || die "build did not produce ${BIN_DIR}/${name}"
  done
  log "binaries in ${BIN_DIR}"
}

# Fetch the pinned Litestream release once, verify it against the digest
# recorded above, and put the binary in the payload. A mismatch stops the
# release: a backup tool that is not the one this repository was written for
# is not a backup anyone has tested.
stage_litestream() {
  local expected=${LITESTREAM_SHA256[${LITESTREAM_ARCH}]:-}
  [[ -n ${expected} ]] || die "no pinned Litestream digest for ${LITESTREAM_ARCH}; use x86_64 or arm64"
  local tarball="litestream-${LITESTREAM_VERSION}-linux-${LITESTREAM_ARCH}.tar.gz"
  local cached="${LITESTREAM_CACHE}/${tarball}"
  mkdir -p "${LITESTREAM_CACHE}"
  if [[ ! -f ${cached} ]]; then
    log "==> fetching ${tarball}"
    curl -fsSLo "${cached}.part" \
      "https://github.com/benbjohnson/litestream/releases/download/v${LITESTREAM_VERSION}/${tarball}"
    mv "${cached}.part" "${cached}"
  fi
  local actual
  actual=$(sha256sum "${cached}" | cut -d' ' -f1)
  if [[ ${actual} != "${expected}" ]]; then
    rm -f "${cached}"
    die "${tarball} has digest ${actual}, expected ${expected}; the cached copy was removed"
  fi
  tar -xzf "${cached}" -C "${STAGE}/bin" litestream
  chmod 755 "${STAGE}/bin/litestream"
}

stage_payload() {
  local name
  rm -rf "${STAGE}"
  mkdir -p "${STAGE}/bin" "${STAGE}/systemd"
  for name in "${BINS[@]}"; do
    [[ -x ${BIN_DIR}/${name} ]] || die "missing ${BIN_DIR}/${name}; run: $0 build"
    install -m 755 "${BIN_DIR}/${name}" "${STAGE}/bin/${name}"
  done
  stage_litestream
  cp -a "${CANARY_DIR}/systemd/." "${STAGE}/systemd/"
  install -m 644 "${CANARY_DIR}/litestream.yml" "${STAGE}/litestream.yml"
  install -m 755 "${CANARY_DIR}/remote-install.sh" "${STAGE}/remote-install.sh"
  python3 "${CANARY_DIR}/extract-runtime-fs.py" \
    "${BIN_DIR}/deis-runtime" "${STAGE}/runtime-fs"
  {
    echo "sha=$(git -C "${ROOT}" rev-parse HEAD 2>/dev/null || echo unknown)"
    echo "short=$(git -C "${ROOT}" rev-parse --short=12 HEAD 2>/dev/null || echo unknown)"
    echo "built_at=$(date -u +%Y-%m-%dT%H:%M:%SZ)"
    echo "built_on=$(hostname)"
  } >"${STAGE}/RELEASE"
}

cmd_ship() {
  stage_payload
  log "==> rsync ${STAGE}/ -> ${SSH_HOST}:${REMOTE_STAGE}/"
  ssh_host "mkdir -p '${REMOTE_STAGE}'"
  rsync -az --delete \
    --exclude operator.env \
    "${STAGE}/" "${SSH_HOST}:${REMOTE_STAGE}/"
  log "shipped to ${SSH_HOST}:${REMOTE_STAGE}"
}

cmd_install() {
  local sha built
  sha="$(git -C "${ROOT}" rev-parse HEAD 2>/dev/null || echo unknown)"
  built="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
  if [[ -f ${STAGE}/RELEASE ]]; then
    sha="$(awk -F= '/^sha=/{print $2}' "${STAGE}/RELEASE")"
    built="$(awk -F= '/^built_at=/{print $2}' "${STAGE}/RELEASE")"
  fi
  log "==> install on ${SSH_HOST}"
  local -a remote=(
    sudo env
    "LAN_IP=${LAN_IP}"
    "ZONE=${ZONE}"
    "NODE_ID=${NODE_ID}"
    "PROXY_ID=${PROXY_ID}"
    "RELEASE_SHA=${sha}"
    "RELEASE_TIME=${built}"
    bash "${REMOTE_STAGE}/remote-install.sh"
  )
  if ((REWRITE_CONFIG)); then
    remote+=(--rewrite-config)
  fi
  ssh_host "${remote[@]}"
}

curl_noproxy() {
  curl --noproxy '*' "$@"
}

# The node's data port answers mutual TLS and nothing else, so the probes
# that reach it run on the host itself, where this installation's fleet PKI
# lives (/etc/deis/pki, root-owned), presenting the proxy leaf exactly the
# way deis-proxy does and verifying the node against the fleet root.
# The arguments are re-quoted for the remote shell, which would otherwise split
# a header such as 'x-deis-app-id: _system' into an empty header and a stray
# URL.
runtime_mtls() {
  ssh_host "sudo curl -fsS --max-time 5 --noproxy '*' \
    --cacert /etc/deis/pki/ca.crt \
    --cert /etc/deis/pki/proxy.crt --key /etc/deis/pki/proxy.key \
    $(printf '%q ' "$@")"
}

cmd_verify() {
  log "==> verify ${LAN_IP}"
  local health
  health="$(curl_noproxy -fsS --max-time 5 "http://${LAN_IP}:8080/health")"
  [[ ${health} == OK ]] || die "control /health returned ${health@Q}"
  log "control /health OK"
  runtime_mtls -H 'x-deis-app-id: _system' \
    "https://127.0.0.1:8086/health" >/dev/null
  log "runtime /health OK (mutual TLS, probed on the host with the fleet proxy leaf)"
  local code
  code="$(ssh_host "sudo curl -sS --max-time 5 --noproxy '*' -o /dev/null -w '%{http_code}' \
    --cacert /etc/deis/pki/ca.crt \
    --cert /etc/deis/pki/proxy.crt --key /etc/deis/pki/proxy.key \
    -H 'x-deis-app-id: demo' 'https://127.0.0.1:8086/hello/world'" || true)"
  if [[ ${code} == 200 ]]; then
    log "demo /hello/world via runtime 200"
    curl_noproxy -fsS --max-time 5 --resolve "demo.${ZONE}:80:${LAN_IP}" \
      "http://demo.${ZONE}/hello/world" >/dev/null
    log "demo /hello/world via proxy 200"
  else
    log "demo not serving yet (runtime /hello/world -> ${code:-none}); run: $0 apps"
  fi
}

cmd_status() {
  ssh_host 'echo "=== units ==="; systemctl is-active deis-telemetry deis-store deis-queue deis-control deis-runtime deis-proxy; echo "=== ports ==="; ss -lnt | awk "NR==1 || /:(80|8080|8086|8090|8091|8092|8093) /"; echo "=== RELEASE ==="; sudo cat /var/lib/deis/RELEASE 2>/dev/null || echo none'
  log "=== from here ==="
  (cmd_verify) || true
}

fetch_operator_env() {
  local dest=${1:-${STAGE}/operator.env}
  mkdir -p "$(dirname "${dest}")"
  ssh_host 'sudo cat /etc/deis/operator.env' >"${dest}"
  chmod 600 "${dest}"
}

merge_credentials() {
  local env_file=$1
  # shellcheck disable=SC1090
  source "${env_file}"
  python3 - "${HOME}/.config/deis/credentials.toml" \
    "${DEIS_CONTROL_ENDPOINT}" "${DEIS_CONTROL_TOKEN}" <<'PY'
import re
import sys
from pathlib import Path

path = Path(sys.argv[1])
control, token = sys.argv[2:4]
path.parent.mkdir(parents=True, exist_ok=True)
text = path.read_text() if path.exists() else ""

def drop_section(src: str, header: str) -> str:
    pat = re.compile(
        r"^\[" + re.escape(header) + r"\][^\n]*\n(?:(?!^\[).*\n)*",
        re.M,
    )
    return pat.sub("", src)

text = drop_section(text, f'endpoints."{control}"')
text = text.strip()
parts = []
if text:
    parts.append(text)
parts.append(f'[endpoints."{control}"]\ntoken = "{token}"')
path.write_text("\n\n".join(parts) + "\n")
path.chmod(0o600)
print(path)
PY
}

cmd_credentials() {
  fetch_operator_env "${STAGE}/operator.env"
  local path
  path="$(merge_credentials "${STAGE}/operator.env")"
  log "wrote ${path}"
  log "shell: source ${STAGE}/operator.env"
}

cmd_apps() {
  command -v node >/dev/null || die "node is not on PATH (needed to bundle)"
  local deis="${BIN_DIR}/deis"
  [[ -x ${deis} ]] || die "missing ${deis}; run: $0 build"
  fetch_operator_env "${STAGE}/operator.env"
  merge_credentials "${STAGE}/operator.env" >/dev/null
  # shellcheck disable=SC1091
  source "${STAGE}/operator.env"
  export DEIS_CONTROL_ENDPOINT DEIS_CONTROL_TOKEN
  export DEIS_TELEMETRY_ENDPOINT
  export DEIS_QUEUE_ENDPOINT

  local callee
  callee="$(mktemp -d "${TMPDIR:-/tmp}/deislet-demo-auth.XXXXXX")"
  mkdir -p "${callee}/functions"
  cat >"${callee}/.config.json" <<'JSON'
{
  "version": "1.0.0",
  "name": "demo-auth",
  "runtime": "standard-v1",
  "language": "typescript",
  "functionRoot": "./functions",
  "compatibilityDate": "2026-09-01",
  "callableBy": ["demo"],
  "vendors": { "deislet": { "enabled": true } },
  "build": { "outDir": "./dist" }
}
JSON
  cat >"${callee}/functions/verify.ts" <<'TS'
export default async function handler(context: any): Promise<Response> {
  const caller = context.request.headers.get("x-deis-caller");
  const note = context.request.headers.get("x-demo-note");
  return new Response(
    JSON.stringify({ callee: "demo-auth", caller, note }),
    { status: 200, headers: { "content-type": "application/json; charset=utf-8" } },
  );
}
TS

  log "==> deploy demo-auth"
  (cd "${callee}" && "${deis}" deploy --app demo-auth)
  log "==> deploy demo production"
  (cd "${ROOT}/scripts/demo-assets/example-app" && "${deis}" deploy --app demo)
  log "==> secret DEMO_SECRET"
  printf '%s' 's90-canary-demo-secret' | "${deis}" secret set DEMO_SECRET --app demo -e production
  log "==> deploy demo preview"
  (cd "${ROOT}/scripts/demo-assets/example-app" && "${deis}" deploy --app demo -e preview)
  "${deis}" versions --app demo
  "${deis}" status --app demo
  rm -rf "${callee}"

  curl_noproxy -fsS --max-time 5 --resolve "demo.${ZONE}:80:${LAN_IP}" \
    "http://demo.${ZONE}/hello/world"
  printf '\n'
  curl_noproxy -fsS --max-time 5 --resolve "preview--demo.${ZONE}:80:${LAN_IP}" \
    "http://preview--demo.${ZONE}/hello/world"
  printf '\n'
}

cmd_release() {
  cmd_build
  cmd_ship
  cmd_install
  cmd_credentials
  cmd_verify
  if ((WITH_APPS)); then
    cmd_apps
  fi
  log ""
  log "console  http://${LAN_IP}:8080/"
  log "proxy    http://demo.${ZONE}/"
  log "token    ${STAGE}/operator.env"
}

main() {
  if [[ $# -lt 1 ]]; then
    usage
    exit 2
  fi
  local cmd=$1
  shift
  parse_global "$@"
  set -- "${REMAINING[@]+"${REMAINING[@]}"}"
  case "${cmd}" in
    build) cmd_build ;;
    ship) cmd_ship ;;
    install) cmd_install ;;
    verify) cmd_verify ;;
    status) cmd_status ;;
    apps) cmd_apps ;;
    credentials) cmd_credentials ;;
    release) cmd_release ;;
    -h|--help) usage ;;
    *) die "unknown command: ${cmd} (try --help)" ;;
  esac
}

main "$@"
