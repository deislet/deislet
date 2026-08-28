#!/usr/bin/env bash
# Zero-downtime upgrade check for deflux.
#
# Starts a backend and a deflux instance, then performs the same handover that
# `deflux -s reload` does: spawn a replacement with --upgrade and send SIGQUIT
# to the old process. Requests keep flowing throughout; any failure during the
# handover fails the test. Every wait is on a real event (HTTP readiness via
# curl's own retry, process exit via tail --pid), never a fixed delay.
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
WORK_DIR="$(mktemp -d)"
BACKEND_PORT=${BACKEND_PORT:-18081}
PROXY_PORT=${PROXY_PORT:-18080}
PIDS=()

cleanup() {
  touch "$WORK_DIR/stop"
  for pid in "${PIDS[@]}"; do
    kill "$pid" 2>/dev/null || true
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

echo "building deflux"
(cd "$ROOT_DIR" && cargo build -q -p deflux)
# Respect a redirected target directory (CARGO_TARGET_DIR or .cargo/config).
TARGET_DIR="$(cd "$ROOT_DIR" && cargo metadata --format-version 1 --no-deps \
  | python3 -c 'import json, sys; print(json.load(sys.stdin)["target_directory"])')"
DEFLUX="$TARGET_DIR/debug/deflux"
[ -x "$DEFLUX" ] || { echo "binary not found: $DEFLUX"; exit 2; }

echo "starting backend on 127.0.0.1:$BACKEND_PORT"
echo ok > "$WORK_DIR/index.html"
python3 -m http.server "$BACKEND_PORT" --bind 127.0.0.1 --directory "$WORK_DIR" >/dev/null 2>&1 &
PIDS+=($!)
wait_http "http://127.0.0.1:$BACKEND_PORT/"

cat > "$WORK_DIR/deflux.toml" <<EOF
listen_addr = ["127.0.0.1"]
listen_ports = [$PROXY_PORT]
upgrade_sock = "$WORK_DIR/upgrade.sock"
pid_file = "$WORK_DIR/deflux.pid"
database = "$WORK_DIR/sites.db"

[[upstreams]]
host = "127.0.0.1"
port = $BACKEND_PORT

[log.error_log]
level = "info"
EOF

echo "starting deflux on 127.0.0.1:$PROXY_PORT"
"$DEFLUX" -c "$WORK_DIR/deflux.toml" > "$WORK_DIR/old.log" 2>&1 &
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
"$DEFLUX" -c "$WORK_DIR/deflux.toml" --upgrade > "$WORK_DIR/new.log" 2>&1 &
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
echo "PASS"
