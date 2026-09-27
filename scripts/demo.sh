#!/usr/bin/env bash
#
# The demo fleet: `start`, `verify`, `status` and `stop`.
#
# Everything the actions are made of lives under `scripts/demo/`: the shared
# helpers and the verification counters in `lib.sh`, the deployment and the
# shutdown in `lifecycle.sh`, and one file per group of checks under
# `checks/`. This file parses the arguments, reads the group registry and
# dispatches.

set -euo pipefail

# This file's own directory: the sources below are found relative to it, so the
# demo runs from any working directory.
DEMO_SCRIPT_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)

source "${DEMO_SCRIPT_DIR}/demo/lib.sh"
source "${DEMO_SCRIPT_DIR}/demo/lifecycle.sh"

# Every group of checks a run may ask, in the order a complete run asks them.
#
# The order is not the order the files are read in, and it is not arbitrary: a
# check that moves production, replaces an isolate or stands a second proxy up
# runs after the checks that need the fleet as they found it.
DEMO_CHECK_GROUPS=(
  # The CLI checks come first because everything after them rests on what
  # `deis deploy` put there. If the deploy report is wrong, saying so before
  # eleven other checks fail for the same reason saves reading all eleven.
  cli
  compiler
  assets
  routing
  streaming
  web_platform
  secrets
  storage
  queue
  cron
  service_bindings
  environments
  limits
  compatibility
  versions
  observability
  fleet
  accounts
  acme
  evidence
)

# The functions of every group, and the array that names them.
for demo_group in "${DEMO_CHECK_GROUPS[@]}"; do
  source "${DEMO_SCRIPT_DIR}/demo/checks/${demo_group}.sh"
done

# How many checks a complete run prints a verdict for: the length of every
# group's array in the registry above, summed.
#
# The summary is a tally of what happened to the checks that ran, and a tally
# has nothing to say about a check that did not run. A call deleted from a
# group's array - by accident, in a merge, or because a check was being
# skipped "for now" - turns a green run into a slightly shorter one, and the
# whole of the difference is a digit nobody has the previous run in front of
# them to compare against. So the number is derived from the arrays rather
# than written down, and adding a check to a group is the only way to change
# it. That is the roll call.
VERIFY_EXPECTED_CHECKS=$(demo_count_checks "${DEMO_CHECK_GROUPS[@]}")

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
  --group <name>                        Run one group of checks (verify only)
  --help                                Show this help
EOF
}

verify_demo() {
  # The groups this run asks: all of them, or the one `--group` named. What
  # the run is held to follows the same choice, so a single-group run is
  # measured against the group it asked for. Named before the state is read,
  # so a mistyped group is reported whether or not a demo is running.
  local -a groups=("${DEMO_CHECK_GROUPS[@]}")
  if [ -n "${VERIFY_GROUP:-}" ]; then
    local known=no
    local group
    for group in "${DEMO_CHECK_GROUPS[@]}"; do
      if [ "$group" = "$VERIFY_GROUP" ]; then
        known=yes
      fi
    done
    if [ "$known" = "no" ]; then
      echo "❌ No such check group: ${VERIFY_GROUP}"
      echo "   Groups: ${DEMO_CHECK_GROUPS[*]}"
      exit 1
    fi
    groups=("$VERIFY_GROUP")
    VERIFY_EXPECTED_CHECKS=$(demo_count_checks "${groups[@]}")
  fi

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

  # The registry's order is the run's order; each group prints its own heading
  # and asks its own checks. See `DEMO_CHECK_GROUPS` above.
  local demo_group
  for demo_group in "${groups[@]}"; do
    local -n group_checks="group_${demo_group}_checks"
    local -n group_label="group_${demo_group}_label"
    echo "-- ${group_label} --"
    local check
    for check in "${group_checks[@]}"; do
      "$check"
    done
  done

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
    echo "     ${counted} verdict(s) were printed and this run asks ${VERIFY_EXPECTED_CHECKS}. If a check was added or removed on purpose, the group arrays in scripts/demo/checks/ are where a run's list comes from; until then the difference is a question that stopped being asked, and no count of what did run says anything about it."
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

# Sourcing this file defines the demo and stops here; the arguments and the
# actions belong to whoever executed it. Sourcing it is how a tool reads the
# registry - `VERIFY_EXPECTED_CHECKS` in particular, which is derived and so
# has no literal to grep for.
if [ "${BASH_SOURCE[0]}" != "$0" ]; then
  return 0
fi

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
    --group)
      VERIFY_GROUP=$2
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
