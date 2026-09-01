#!/usr/bin/env bash
#
# Regression tests for the parts of `demo.sh verify` that read the node's log.
#
# Why this exists
# ---------------
# `demo.sh verify` is the evidence this project is judged by, and thirteen of
# its checks get that evidence by grepping the node's log file. Two defects in
# that path were found together, and neither of them made a check say "failed":
#
#   1. The log is not written only by the node. An application's own log lines
#      land in the same file, and an application's log line carries whatever a
#      request put in it - so one unauthenticated `GET /hello/%00` puts a NUL
#      byte in it. GNU grep then calls the whole stream binary and writes
#      nothing at all on stdout, and every check that reads the log sees an
#      empty log while the node's own sentence sits in the file.
#
#   2. Two of those checks quoted a log line through an unguarded pipeline.
#      A grep that matches nothing exits 1, `pipefail` makes that the
#      pipeline's status and `set -e` ends the run there - no line for the
#      check, no summary, nothing but an exit status to say the run did not
#      finish rather than that it had not finished yet.
#
# Together they meant one request from outside deleted half the proof and
# stopped the run without a word. These tests hold both closed.
#
# What they test
# --------------
# The real definitions, read out of `scripts/demo.sh` at run time rather than
# copied here: a test that carried its own copy of `node_log` would go on
# passing after somebody changed the one that ships.
#
# Usage: test-demo-witnesses.sh

set -euo pipefail

ROOT_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
DEMO_SH="${ROOT_DIR}/scripts/demo.sh"

WORK_DIR=$(mktemp -d)
trap 'rm -rf "$WORK_DIR"' EXIT

failures=0

pass() { echo "   ✓ $1"; }
fail() { echo "   ✗ $1"; [ -n "${2:-}" ] && echo "     $2"; failures=$((failures + 1)); return 0; }

# One function's body, lifted out of demo.sh exactly as it ships.
definition_of() {
  local name=$1
  local text
  text=$(sed -n "/^${name}() {/,/^}/p" "$DEMO_SH")
  if [ -z "$text" ]; then
    echo "❌ ${DEMO_SH} defines no ${name}(); this test is out of date" >&2
    exit 1
  fi
  printf '%s\n' "$text"
}

# A node log shaped like the real one: tracing's ANSI colouring around the
# level, an application line carrying a NUL byte the way `GET /hello/%00`
# produces one, and a WARN line the node wrote *after* it. The last is the
# point - GNU grep suppresses everything from the NUL onwards, so a check
# looking for that line is the one that goes blind.
LOG_DIR="${WORK_DIR}/logs"
mkdir -p "$LOG_DIR"
{
  printf '2026-09-02T00:00:00.000000Z \033[33m WARN\033[0m ThreadId(01) an early warning\n'
  printf '2026-09-02T00:00:01.000000Z  INFO ThreadId(02) greeted \000 from v1\n'
  printf '2026-09-02T00:00:02.000000Z  WARN ThreadId(03) A response stream of app demo.production was closed after 4000ms: it is past stream_timeout_ms\n'
  printf '2026-09-02T00:00:03.000000Z  WARN ThreadId(04) 1 interval(s) were still armed when this invocation ended, and have been cleared\n'
} > "${LOG_DIR}/deis-runtime.log"

export LOG_DIR

eval "$(definition_of node_log)"

# ----------------------------------------------------------------------
# 1. A NUL an application logged does not hide the node's own lines
# ----------------------------------------------------------------------

# Counted with `wc -l` and not with `grep -c`, because `-c` is the one form
# that is *not* affected: grep suppresses the matching lines on a binary
# stream but still counts them, so a test written with `-c` passes against
# the defect. Every check in demo.sh reads lines.
visible=$(node_log | grep " WARN " | wc -l)
if [ "$visible" -eq 3 ]; then
  pass "node_log hands on every WARN line although the log carries a NUL byte"
else
  fail "node_log hands on every WARN line although the log carries a NUL byte" \
    "expected 3 WARN lines, ${visible} came through - a NUL byte from one request has made the rest of the log invisible"
fi

# The two counters the streaming and timer checks assert on, run against the
# same poisoned log. Both count lines that come after the NUL.
eval "$(definition_of stream_budget_log_lines)"
eval "$(definition_of timer_cleared_log_lines)"

counted=$(stream_budget_log_lines)
if [ "$counted" -eq 1 ]; then
  pass "stream_budget_log_lines counts the line the node wrote after the NUL"
else
  fail "stream_budget_log_lines counts the line the node wrote after the NUL" \
    "expected 1, got ${counted}"
fi

counted=$(timer_cleared_log_lines)
if [ "$counted" -eq 1 ]; then
  pass "timer_cleared_log_lines counts the line the node wrote after the NUL"
else
  fail "timer_cleared_log_lines counts the line the node wrote after the NUL" \
    "expected 1, got ${counted}"
fi

# ----------------------------------------------------------------------
# 2. A quotation that finds nothing does not end the run
# ----------------------------------------------------------------------
#
# The assignments are taken from demo.sh as they ship and run against a log
# that does not contain what they look for, which is the case each check is
# written to report. Under `set -euo pipefail` an unguarded one exits here
# instead of reaching its own `if`.

empty_log="${WORK_DIR}/empty-logs"
mkdir -p "$empty_log"
printf '2026-09-02T00:00:00.000000Z  INFO ThreadId(01) nothing to see\n' \
  > "${empty_log}/deis-runtime.log"

quotations=$(grep -n '^  said=\$(node_log | grep " WARN "' "$DEMO_SH" || true)
if [ -z "$quotations" ]; then
  echo "❌ ${DEMO_SH} has no 'said=\$(node_log | grep \" WARN \" ...' line; this test is out of date" >&2
  exit 1
fi

# Each one runs in a bash of its own rather than in a subshell here. bash
# turns `set -e` off inside anything it is asked to test the outcome of - an
# `if` condition included - so a subshell in the `if` below would have been a
# test that could not fail. A separate process is judged on its exit status
# with errexit really in force, which is the state `verify` runs in.
probe="${WORK_DIR}/probe.sh"
while IFS= read -r entry; do
  line_no=${entry%%:*}
  statement=${entry#*:}
  {
    echo '#!/usr/bin/env bash'
    echo 'set -euo pipefail'
    definition_of node_log
    printf '%s\n' "$statement"
    echo 'printf "%s" "${said:-}" > /dev/null'
  } > "$probe"
  status=0
  LOG_DIR="$empty_log" bash "$probe" >/dev/null 2>&1 || status=$?
  if [ "$status" -eq 0 ]; then
    pass "demo.sh:${line_no} survives a log that does not carry the line it quotes"
  else
    fail "demo.sh:${line_no} survives a log that does not carry the line it quotes" \
      "the assignment exited ${status}, so under set -euo pipefail the whole verify run ends here - no failure line for the check, no summary, and nothing but the exit status to say the run did not finish"
  fi
done <<< "$quotations"

# ----------------------------------------------------------------------
# 3. A stale helper process is refused rather than raced
# ----------------------------------------------------------------------
#
# `start` refuses to run while processes from an earlier run are still up,
# because an orphan holding a port answers the readiness probe and the run
# then talks to it instead of to what it started. That guard listed the six
# components and neither of the two processes the demo runs that are not
# components - the fetch origin and the stub certificate authority - and those
# were the two that actually leaked: an orphaned origin answered
# `/payload.txt` in under a millisecond while the child that lost the bind was
# still dying, so the run printed "✓ Origin is serving" and every later fetch
# went to a stranger running whatever revision of origin.py it had started
# with.
#
# The pattern is taken out of demo.sh and run here, so a pattern that stops
# naming them fails this rather than costing somebody a verification run.

fixture_bin="${WORK_DIR}/bin"
fixture_demo="${WORK_DIR}/demo"
mkdir -p "${fixture_bin}" "${fixture_demo}/origin"

# A process that only has to exist and stay alive under the right argv.
# `signal.pause()` rather than a sleep: nothing here is being waited out, the
# process is a fixture that lives until it is killed.
cat > "${fixture_demo}/origin/origin.py" <<'FIXTURE'
import signal
signal.pause()
FIXTURE

python3 "${fixture_demo}/origin/origin.py" &
planted=$!
trap 'kill "$planted" 2>/dev/null; rm -rf "$WORK_DIR"' EXIT

scan=$(sed -n '/^  orphans=\$(pgrep -f/,/|| true)$/p' "$DEMO_SH")
if [ -z "$scan" ]; then
  echo "❌ ${DEMO_SH} has no 'orphans=\$(pgrep -f ...' scan; this test is out of date" >&2
  exit 1
fi

# Written to a file and run as a script, not passed to `bash -c` as a string:
# the scan is several lines of shell with its own quoting, and re-quoting it
# through an argument is how a test ends up measuring the quoting instead of
# the pattern.
probe="${WORK_DIR}/scan.sh"
{
  echo '#!/usr/bin/env bash'
  echo 'set -uo pipefail'
  printf 'BIN_DIR=%q\n' "$fixture_bin"
  printf 'DEMO_DIR=%q\n' "$fixture_demo"
  printf '%s\n' "$scan"
  echo 'printf "%s" "${orphans}"'
} > "$probe"

found=$(bash "$probe" 2>/dev/null || true)

case " $(printf '%s' "$found" | tr '\n' ' ') " in
  *" ${planted} "*)
    pass "start's orphan scan sees a stale fetch origin"
    ;;
  *)
    fail "start's orphan scan sees a stale fetch origin" \
      "pid ${planted} is running as ${fixture_demo}/origin/origin.py and the scan did not name it, so a start would go ahead and race it for the port"
    ;;
esac

kill "$planted" 2>/dev/null || true
wait "$planted" 2>/dev/null || true
trap 'rm -rf "$WORK_DIR"' EXIT

# ----------------------------------------------------------------------
# 4. A component that is not answering is reported, not fatal
# ----------------------------------------------------------------------
#
# Every check reaches its component through one of these helpers, and each of
# them used to hand curl's exit status back to the caller. curl exits 7 when
# nothing is listening; a check reads the answer with `body=$(app_request
# ...)`; and an assignment that fails under `set -euo pipefail` is the end of
# the run. Measured against a port with nothing on it,
# check_static_asset, check_route_parameter and check_app_fetch_allowed each
# ended the run and printed not one character between them - no failure line,
# no summary, and only the exit status to say the run had stopped rather than
# that it had not finished yet.
#
# Each helper is run here exactly as it ships, in a bash of its own, against a
# port nothing is bound to. A separate process rather than a subshell in an
# `if`: bash turns errexit off inside anything it is asked to test the outcome
# of, so a subshell here would be a test that could not fail.

dead_port_probe() {
  local helper=$1
  local call=$2
  local probe="${WORK_DIR}/dead-${helper}.sh"
  {
    echo '#!/usr/bin/env bash'
    echo 'set -euo pipefail'
    echo 'APP_ID=demo'
    echo 'API_KEY=irrelevant'
    echo 'DEIS_RUNTIME_PORT=9'
    echo 'DEIS_PROXY_PORT=9'
    echo 'DEIS_CONTROL_PORT=9'
    printf 'LOG_DIR=%q\n' "$WORK_DIR"
    echo 'control_url() { printf "http://127.0.0.1:%s" "$DEIS_CONTROL_PORT"; }'
    definition_of "$helper"
    # A plain assignment, not `local x=...`: `local` returns its own status
    # and would hide exactly the failure being tested for.
    echo 'answer='
    printf 'answer=$(%s)\n' "$call"
    echo 'printf "%s" "${answer}" > /dev/null'
  } > "$probe"
  local status=0
  bash "$probe" >/dev/null 2>&1 || status=$?
  printf '%s' "$status"
}

for pair in \
  'app_request|app_request "${LOG_DIR}/h" /' \
  'env_request|env_request preview "${LOG_DIR}/h" /' \
  'routed_request|routed_request example.test / "${LOG_DIR}/h"' \
  'app_fetch|app_fetch probe http://127.0.0.1:9/' \
  'control_api|control_api /api/runtime' \
  'control_status|control_status GET /api/runtime ""'
do
  helper=${pair%%|*}
  call=${pair#*|}
  status=$(dead_port_probe "$helper" "$call")
  if [ "$status" -eq 0 ]; then
    pass "${helper} lets the check report a component that is not answering"
  else
    fail "${helper} lets the check report a component that is not answering" \
      "the assignment exited ${status}, so under set -euo pipefail the verify run ends inside whichever check called it - no failure line, no summary, and nothing but an exit status to say the run stopped rather than that it had not finished"
  fi
done

# The two readers of a saved response, against a header file curl never wrote,
# which is what a request that could not connect leaves behind. `head` and
# `tr` on a missing file exit 1, and `status=$(http_status ...)` then ends the
# run for the same reason.
missing="${WORK_DIR}/never-written.headers"
rm -f "$missing"
for pair in \
  'http_status|http_status "'"$missing"'"' \
  'header_value|header_value "'"$missing"'" Content-Type'
do
  helper=${pair%%|*}
  call=${pair#*|}
  probe="${WORK_DIR}/missing-${helper}.sh"
  {
    echo '#!/usr/bin/env bash'
    echo 'set -euo pipefail'
    definition_of "$helper"
    echo 'answer='
    printf 'answer=$(%s)\n' "$call"
    echo 'printf "[%s]" "${answer}"'
  } > "$probe"
  status=0
  out=$(bash "$probe" 2>/dev/null) || status=$?
  if [ "$status" -eq 0 ] && [ "$out" = "[]" ]; then
    pass "${helper} answers nothing for a response that never arrived"
  else
    fail "${helper} answers nothing for a response that never arrived" \
      "exited ${status} and printed ${out:-nothing}; a check reading it with a plain assignment ends the whole run there"
  fi
done

# The same rule for the curls the checks run inline rather than through a
# helper. This one is read out of demo.sh rather than executed: there are
# twenty-six of them, they address ports that are only up while the demo is,
# and the property wanted is that none of them can end the run - which is a
# property of how they are written. A check whose curl exits 7 and takes the
# run with it prints nothing, so the reader learns neither what failed nor
# that anything did.
unguarded=$(python3 - "$DEMO_SH" <<'SCAN'
import re, sys

lines = open(sys.argv[1], encoding="utf-8").read().split("\n")
i = 0
while i < len(lines):
    line = lines[i]
    assignment = re.match(r"^\s*([A-Za-z_][A-Za-z_0-9]*)=\$\(\s*(curl\b.*)$", line)
    if assignment and not re.match(r"^\s*(local|export|declare|readonly)\b", line):
        j = i
        while lines[j].rstrip().endswith("\\"):
            j += 1
        if "||" not in "\n".join(lines[i:j + 1]):
            print(f"{i + 1}: {lines[i].strip()}")
        i = j
    i += 1
SCAN
)
if [ -z "$unguarded" ]; then
  pass "every curl a check reads the answer of is guarded"
else
  fail "every curl a check reads the answer of is guarded" \
    "these assignments hand curl's exit status to the caller, and under set -euo pipefail that ends the verify run where the check should have reported: $(printf '%s' "$unguarded" | tr '\n' '; ' | head -c 300)"
fi

# ----------------------------------------------------------------------
# 5. A check that gives up does not take the run with it
# ----------------------------------------------------------------------
#
# `answered_json` records the failure itself and returns 1 so its caller can
# stop. The callers used to stop with a bare `|| return`, which hands that 1
# back as the check function's own status - and a check that returns non-zero
# under `set -e` ends the whole verify run. So the seven checks that use it
# each turned "the node did not answer" into "the run stopped without a
# summary", which is worse than the failure they had just recorded.
#
# Every one of those lines is taken out of demo.sh as it ships and run inside
# a function, against a body that is not JSON.

givers_up=$(grep -n 'answered_json .* || return' "$DEMO_SH" || true)
if [ -z "$givers_up" ]; then
  echo "❌ ${DEMO_SH} has no 'answered_json ... || return' line; this test is out of date" >&2
  exit 1
fi

while IFS= read -r entry; do
  line_no=${entry%%:*}
  statement=${entry#*:}
  probe="${WORK_DIR}/giveup.sh"
  {
    echo '#!/usr/bin/env bash'
    echo 'set -euo pipefail'
    echo 'CHECKS_PASSED=0'
    echo 'CHECKS_FAILED=0'
    printf 'LOG_DIR=%q\n' "$WORK_DIR"
    definition_of fail_check
    definition_of http_status
    definition_of answered_json
    echo 'a_check() {'
    echo '  local title="a check"'
    echo '  local body="<html>not json</html>"'
    echo '  local refused_body="$body" allowed_body="$body"'
    printf '%s\n' "$statement"
    echo '  return 0'
    echo '}'
    echo 'a_check'
  } > "$probe"
  status=0
  bash "$probe" >/dev/null 2>&1 || status=$?
  if [ "$status" -eq 0 ]; then
    pass "demo.sh:${line_no} gives up on the check without ending the run"
  else
    fail "demo.sh:${line_no} gives up on the check without ending the run" \
      "the check function returned ${status}, and a check that returns non-zero under set -e ends the verify run right after it has recorded its own failure"
  fi
done <<< "$givers_up"

# ----------------------------------------------------------------------
# 6. A run that stops anyway says so
# ----------------------------------------------------------------------
#
# The helpers above close the causes that are known. These two traps are for
# every other cause, the ones nobody has met yet: whatever ends the run, it
# has to name the command and print the tally, because a run that stopped in
# the middle otherwise looks exactly like a run that has not finished - and
# the checks that never ran are then indistinguishable from checks that
# passed.

probe="${WORK_DIR}/stopped.sh"
{
  echo '#!/usr/bin/env bash'
  echo 'set -euo pipefail'
  echo 'CHECKS_PASSED=0'
  echo 'CHECKS_FAILED=0'
  definition_of pass_check
  definition_of fail_check
  # The two traps and their flag, lifted out of demo.sh as they ship.
  sed -n '/^VERIFY_REACHED_ITS_SUMMARY=/,/^}/p' "$DEMO_SH"
  definition_of on_verify_exit
  echo 'set -E'
  echo 'trap '"'"'on_verify_error "$LINENO" "$BASH_COMMAND" "$?"'"'"' ERR'
  echo 'trap on_verify_exit EXIT'
  echo 'pass_check "one that passed"'
  echo 'fail_check "one that failed"'
  echo 'answer='
  echo 'answer=$(curl -s -m 2 http://127.0.0.1:9/)'
  echo 'VERIFY_REACHED_ITS_SUMMARY=true'
  echo 'echo "${CHECKS_PASSED} passed, ${CHECKS_FAILED} failed"'
} > "$probe"

status=0
out=$(bash "$probe" 2>&1) || status=$?
stopped=no
tallied=no
case "$out" in *"the run stopped before it had finished"*) stopped=yes ;; esac
case "$out" in *"1 passed, 1 failed, and the run did not reach the end of its list"*) tallied=yes ;; esac

if [ "$stopped" = "yes" ] && [ "$tallied" = "yes" ] && [ "$status" -ne 0 ]; then
  pass "a run killed in the middle prints why it stopped and what it had counted"
else
  fail "a run killed in the middle prints why it stopped and what it had counted" \
    "exit ${status}, stopped-line=${stopped}, tally-line=${tallied}; it printed: $(printf '%s' "$out" | tr '\n' ' ' | head -c 200)"
fi

# A run that stops must never exit 0, whatever stopped it. Zero is what a
# pipeline reads as "every check passed", and a run that stopped short proved
# nothing about the checks it never asked.
probe="${WORK_DIR}/stopped-clean.sh"
{
  echo '#!/usr/bin/env bash'
  echo 'set -euo pipefail'
  echo 'CHECKS_PASSED=0'
  echo 'CHECKS_FAILED=0'
  definition_of pass_check
  definition_of fail_check
  sed -n '/^VERIFY_REACHED_ITS_SUMMARY=/,/^}/p' "$DEMO_SH"
  definition_of on_verify_exit
  echo 'trap on_verify_exit EXIT'
  echo 'pass_check "one that passed"'
  # A component's helper calling `exit 0` from inside a check, which is the
  # one way to stop early with a status that reads as success.
  echo 'exit 0'
} > "$probe"

status=0
out=$(bash "$probe" 2>&1) || status=$?
if [ "$status" -ne 0 ]; then
  pass "a run that stops short never exits 0"
else
  fail "a run that stops short never exits 0" \
    "it exited 0 after printing: $(printf '%s' "$out" | tr '\n' ' ' | head -c 160). A pipeline reads that as every check having passed"
fi

# And the other way round: a run that did reach its summary must not have the
# trap speak over it, or every failing verify would end with two conclusions.
probe="${WORK_DIR}/finished.sh"
{
  echo '#!/usr/bin/env bash'
  echo 'set -euo pipefail'
  echo 'CHECKS_PASSED=0'
  echo 'CHECKS_FAILED=0'
  definition_of pass_check
  definition_of fail_check
  sed -n '/^VERIFY_REACHED_ITS_SUMMARY=/,/^}/p' "$DEMO_SH"
  definition_of on_verify_exit
  echo 'set -E'
  echo 'trap '"'"'on_verify_error "$LINENO" "$BASH_COMMAND" "$?"'"'"' ERR'
  echo 'trap on_verify_exit EXIT'
  echo 'pass_check "one that passed"'
  echo 'fail_check "one that failed"'
  echo 'VERIFY_REACHED_ITS_SUMMARY=true'
  echo 'echo "${CHECKS_PASSED} passed, ${CHECKS_FAILED} failed"'
  echo '[ "$CHECKS_FAILED" -eq 0 ]'
} > "$probe"

status=0
out=$(bash "$probe" 2>&1) || status=$?
case "$out" in
  *"the run stopped before it had finished"*)
    fail "a finished run reports only its own summary" \
      "the trap spoke over a run that had reached the end of its list: $(printf '%s' "$out" | tr '\n' ' ' | head -c 160)"
    ;;
  *"1 passed, 1 failed"*)
    if [ "$status" -eq 1 ]; then
      pass "a finished run reports only its own summary, and fails on its failures"
    else
      fail "a finished run reports only its own summary, and fails on its failures" \
        "the summary was printed but the run exited ${status} rather than 1"
    fi
    ;;
  *)
    fail "a finished run reports only its own summary" \
      "no summary at all: $(printf '%s' "$out" | tr '\n' ' ' | head -c 160)"
    ;;
esac

echo
if [ "$failures" -eq 0 ]; then
  echo "all witness tests passed"
else
  echo "${failures} witness test(s) failed"
  exit 1
fi
