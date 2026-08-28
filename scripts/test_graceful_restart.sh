#!/usr/bin/env bash
# Zero-downtime upgrade check for deis-proxy.
#
# Starts a backend and a deis-proxy instance, then performs the same handover that
# `deis-proxy -s reload` does: spawn a replacement with --upgrade and send SIGQUIT
# to the old process. Requests keep flowing throughout; any failure during the
# handover fails the test. Every wait is on a real event (HTTP readiness via
# curl's own retry, process exit via tail --pid), never a fixed delay.
#
# A second case runs deis-proxy as a daemon started with a relative `-c` path and
# the default (relative) database from its own directory, then reloads it with
# `deis-proxy -s reload`; this is the path on which the replacement used to resolve
# every relative path against `/` and die.
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
WORK_DIR="$(mktemp -d)"
BACKEND_PORT=${BACKEND_PORT:-18081}
PROXY_PORT=${PROXY_PORT:-18080}
PIDS=()

cleanup() {
  touch "$WORK_DIR/stop"
  for pid in "${PIDS[@]}"; do
    kill -9 "$pid" 2>/dev/null || true
    # Reap it here so bash does not print a "Killed" job notice on exit.
    wait "$pid" 2>/dev/null || true
  done
  # Daemons are not children; find them through the pid files they wrote.
  for pidfile in "$WORK_DIR"/daemon/deis-proxy.pid "$WORK_DIR"/daemon/deis-proxy.pid.old; do
    [ -f "$pidfile" ] && kill -9 "$(cat "$pidfile")" 2>/dev/null || true
  done
  rm -rf "$WORK_DIR"
}
trap cleanup EXIT

wait_http() {
  curl -sf -o /dev/null -m 2 --retry 30 --retry-delay 1 --retry-all-errors --retry-connrefused "$1"
}

for tool in curl python3 tail timeout; do
  command -v "$tool" >/dev/null || { echo "missing tool: $tool"; exit 2; }
done

echo "building deis-proxy"
(cd "$ROOT_DIR" && cargo build -q -p deis-proxy)
# Respect a redirected target directory (CARGO_TARGET_DIR or .cargo/config).
TARGET_DIR="$(cd "$ROOT_DIR" && cargo metadata --format-version 1 --no-deps \
  | python3 -c 'import json, sys; print(json.load(sys.stdin)["target_directory"])')"
DEIS_PROXY="$TARGET_DIR/debug/deis-proxy"
[ -x "$DEIS_PROXY" ] || { echo "binary not found: $DEIS_PROXY"; exit 2; }

echo "starting backend on 127.0.0.1:$BACKEND_PORT"
echo ok > "$WORK_DIR/index.html"
python3 -m http.server "$BACKEND_PORT" --bind 127.0.0.1 --directory "$WORK_DIR" >/dev/null 2>&1 &
PIDS+=($!)
wait_http "http://127.0.0.1:$BACKEND_PORT/"

cat > "$WORK_DIR/deis-proxy.toml" <<EOF
listen_addr = ["127.0.0.1"]
listen_ports = [$PROXY_PORT]
upgrade_sock = "$WORK_DIR/upgrade.sock"
pid_file = "$WORK_DIR/deis-proxy.pid"
database = "$WORK_DIR/sites.db"

[[upstreams]]
host = "127.0.0.1"
port = $BACKEND_PORT

[log.error_log]
level = "info"
EOF

echo "starting deis-proxy on 127.0.0.1:$PROXY_PORT"
"$DEIS_PROXY" -c "$WORK_DIR/deis-proxy.toml" > "$WORK_DIR/old.log" 2>&1 &
OLD_PID=$!
PIDS+=($OLD_PID)
wait_http "http://127.0.0.1:$PROXY_PORT/"
echo "old process $OLD_PID is serving"

# Keep requests flowing during the handover and count failures.
FAILURES="$WORK_DIR/failures"
: > "$FAILURES"
(
  while [ ! -e "$WORK_DIR/stop" ]; do
    curl -sf -o /dev/null -m 2 "http://127.0.0.1:$PROXY_PORT/" || echo fail >> "$FAILURES"
  done
) &
LOAD_PID=$!
PIDS+=($LOAD_PID)

echo "spawning replacement and handing over the listening sockets"
"$DEIS_PROXY" -c "$WORK_DIR/deis-proxy.toml" --upgrade > "$WORK_DIR/new.log" 2>&1 &
NEW_PID=$!
PIDS+=($NEW_PID)
kill -QUIT "$OLD_PID"

if ! timeout 60 tail --pid="$OLD_PID" -f /dev/null; then
  echo "FAIL: old process $OLD_PID did not exit"
  echo "--- old process log"; tail -40 "$WORK_DIR/old.log"
  echo "--- replacement log"; tail -40 "$WORK_DIR/new.log"
  exit 1
fi
echo "old process $OLD_PID exited"

if ! kill -0 "$NEW_PID" 2>/dev/null; then
  echo "FAIL: replacement process died"
  cat "$WORK_DIR/new.log"
  exit 1
fi
wait_http "http://127.0.0.1:$PROXY_PORT/"
echo "replacement process $NEW_PID is serving"

touch "$WORK_DIR/stop"
wait "$LOAD_PID" || true

FAILED=$(wc -l < "$FAILURES")
echo "requests failed during handover: $FAILED"
if ! grep -q "listener sockets sent" "$WORK_DIR/old.log"; then
  echo "warning: old process log does not mention the socket transfer"
  tail -20 "$WORK_DIR/old.log"
fi
if [ "$FAILED" -ne 0 ]; then
  echo "FAIL"
  exit 1
fi
echo "PASS: foreground handover"

# Wait for the replacement to finish so the proxy port is free again.
kill "$NEW_PID"
timeout 60 tail --pid="$NEW_PID" -f /dev/null || true

# --- Daemon mode with relative paths -------------------------------------
# Everything relative, resolved against the directory deis-proxy is started from:
# the config path, the default database (sites.db), the pid file, the upgrade
# socket and the error log. `user`/`group` stay unset so no root is needed.
DAEMON_DIR="$WORK_DIR/daemon"
mkdir -p "$DAEMON_DIR"
cat > "$DAEMON_DIR/deis-proxy.toml" <<EOF
listen_addr = ["127.0.0.1"]
listen_ports = [$PROXY_PORT]
upgrade_sock = "upgrade.sock"
pid_file = "deis-proxy.pid"
grace_period_secs = 2

[[upstreams]]
host = "127.0.0.1"
port = $BACKEND_PORT

[log.error_log]
level = "info"
path = "daemon.log"
EOF

echo "starting deis-proxy as a daemon from $DAEMON_DIR with relative paths"
(cd "$DAEMON_DIR" && "$DEIS_PROXY" -c deis-proxy.toml -d)
wait_http "http://127.0.0.1:$PROXY_PORT/"
DAEMON_PID=$(cat "$DAEMON_DIR/deis-proxy.pid")
[ -f "$DAEMON_DIR/sites.db" ] || { echo "FAIL: database not created next to the config"; exit 1; }
echo "daemon $DAEMON_PID is serving"

: > "$FAILURES"
(
  while [ ! -e "$WORK_DIR/stop" ]; do
    curl -sf -o /dev/null -m 2 "http://127.0.0.1:$PROXY_PORT/" || echo fail >> "$FAILURES"
  done
) &
LOAD_PID=$!
PIDS+=($LOAD_PID)

echo "reloading the daemon with -s reload"
(cd "$DAEMON_DIR" && "$DEIS_PROXY" -c deis-proxy.toml -s reload)

if ! timeout 60 tail --pid="$DAEMON_PID" -f /dev/null; then
  echo "FAIL: old daemon $DAEMON_PID did not exit"
  tail -40 "$DAEMON_DIR/daemon.log"
  exit 1
fi
echo "old daemon $DAEMON_PID exited"

NEW_DAEMON_PID=$(cat "$DAEMON_DIR/deis-proxy.pid")
if [ "$NEW_DAEMON_PID" = "$DAEMON_PID" ] || ! kill -0 "$NEW_DAEMON_PID" 2>/dev/null; then
  echo "FAIL: no replacement daemon (pid file says $NEW_DAEMON_PID)"
  tail -40 "$DAEMON_DIR/daemon.log"
  exit 1
fi
wait_http "http://127.0.0.1:$PROXY_PORT/"
echo "replacement daemon $NEW_DAEMON_PID is serving"

touch "$WORK_DIR/stop"
wait "$LOAD_PID" || true
rm -f "$WORK_DIR/stop"

FAILED=$(wc -l < "$FAILURES")
echo "requests failed during daemon reload: $FAILED"
if ! grep -q "replacement process is ready" "$DAEMON_DIR/daemon.log"; then
  echo "warning: daemon log does not mention the readiness handshake"
  tail -20 "$DAEMON_DIR/daemon.log"
fi
if [ "$FAILED" -ne 0 ]; then
  echo "FAIL"
  exit 1
fi
echo "PASS: daemon reload with relative paths"
