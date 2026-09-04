#!/usr/bin/env bash
#
# Regression tests for the parts of `demo.sh verify` that read the node's log.
#
# Why this exists
# ---------------
# `demo.sh verify` is the evidence this project is judged by, and fifteen of
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
# The sections after them hold closed the rest of what a later round found in
# the same place: a check that passed because something outside it moved (a
# counted log line that any caller could produce, and one that a caller could
# forge outright), a `start` that replaced the settings it was given with the
# last run's, a summary that could not report a check that had left the list,
# and a cross-caller check that nobody had ever seen say no.
#
# What they test
# --------------
# The real definitions, read out of `scripts/demo.sh` at run time rather than
# copied here: a test that carried its own copy of `node_log` would go on
# passing after somebody changed the one that ships.
#
# Usage: test-demo-witnesses.sh [path/to/demo.sh]
#
# The optional argument is there so a candidate copy can be held to these
# tests before it is the one that ships - and so that "this used to fail" is
# a thing anybody can re-run against an older copy rather than a claim in a
# commit message.

set -euo pipefail

ROOT_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
DEMO_SH="${1:-${ROOT_DIR}/scripts/demo.sh}"

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
#
# The two node lines carry the `trace_id` field tracing appends, because that
# is what the counters read: they count what one invocation caused, not what
# matches a form of words. See section 7.
POISONED_TRACE=00000000000000000000000000000042
{
  printf '2026-09-02T00:00:00.000000Z \033[33m WARN\033[0m ThreadId(01) an early warning\n'
  printf '2026-09-02T00:00:01.000000Z  INFO ThreadId(02) greeted \000 from v1\n'
  printf '2026-09-02T00:00:02.000000Z  WARN ThreadId(03) A response stream of app demo.production was closed after 4000ms: it is past stream_timeout_ms app_id="demo.production" trace_id="%s" stream_id=1\n' "$POISONED_TRACE"
  printf '2026-09-02T00:00:03.000000Z  WARN ThreadId(04) 1 interval(s) were still armed when this invocation ended, and have been cleared app_id="demo.production" trace_id="%s"\n' "$POISONED_TRACE"
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
eval "$(definition_of lines_of_invocation)"
eval "$(definition_of stream_budget_log_lines)"
eval "$(definition_of timer_cleared_log_lines)"

counted=$(stream_budget_log_lines "$POISONED_TRACE")
if [ "$counted" -eq 1 ]; then
  pass "stream_budget_log_lines counts the line the node wrote after the NUL"
else
  fail "stream_budget_log_lines counts the line the node wrote after the NUL" \
    "expected 1, got ${counted}"
fi

counted=$(timer_cleared_log_lines "$POISONED_TRACE")
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
  # The whole assignment, continuation lines included: these quotations now
  # name the invocation they belong to and are wrapped over two lines, and a
  # probe fed half a statement measures its own truncation.
  statement=$(awk -v start="$line_no" 'NR >= start { print; if ($0 !~ /\\$/) exit }' "$DEMO_SH")
  {
    echo '#!/usr/bin/env bash'
    echo 'set -euo pipefail'
    definition_of node_log
    # The quotations now name the invocation they are about, so the probe has
    # to supply one. Any id will do: the point of this test is a log that does
    # not carry the line, and an id nothing logged is one more way not to
    # carry it.
    echo 'trace=00000000000000000000000000000000'
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

# ----------------------------------------------------------------------
# 7. A counted line is one this invocation caused
# ----------------------------------------------------------------------
#
# Two checks assert that the node said something at warn while they ran, and
# both used to do it by counting the lines whose *wording* matched. The
# wording is the same for every stream on the node and for every request that
# leaves a timer armed, so the count answered a question about the whole node
# and was read as an answer about one check's own request.
#
# Two ways to move it, both measured against the demo as it shipped. The
# cheap one needs no forgery at all: `GET /api/timers?mode=arm` from outside,
# unauthenticated like every route here, makes the node write a real
# "were still armed" line and the counter goes up. The other is a forged
# line - one `GET /hello/<newline><a whole fabricated WARN line>` put a line
# in the file carrying a plausible timestamp, the level, a thread id, the
# node's exact wording and the `app_id=`/`trace_id=` fields tracing appends,
# and that line was then printed by `verify` as the node's own evidence.
#
# Both are closed by the same change: count and quote by invocation. Every
# line one invocation produces carries the same `trace_id`; the handler logs
# a marker naming a tag the run invented; the checks read the id off that
# marker and count only lines carrying it.

attributed="${WORK_DIR}/attributed"
mkdir -p "$attributed"
mine=1111111111111111aaaaaaaaaaaaaaaa
theirs=2222222222222222bbbbbbbbbbbbbbbb
{
  # The marker this run's own request left behind.
  printf '2026-09-02T00:00:00.000000Z  INFO ThreadId(01) stream witness budget-42 app_id="demo.production" trace_id="%s"\n' "$mine"
  printf '2026-09-02T00:00:00.000000Z  INFO ThreadId(01) timer witness timers-42 app_id="demo.production" trace_id="%s"\n' "$mine"
  # A genuine line the node wrote about somebody else's stream and somebody
  # else's timers. Same words, same level, different invocation.
  printf '2026-09-02T00:00:01.000000Z  WARN ThreadId(02) A response stream of app demo.production was closed after 4000ms: it is past stream_timeout_ms app_id="demo.production" trace_id="%s" stream_id=7\n' "$theirs"
  printf '2026-09-02T00:00:02.000000Z  WARN ThreadId(03) 1 interval(s) were still armed when this invocation ended, and have been cleared app_id="demo.production" trace_id="%s"\n' "$theirs"
  # And a line forged from outside through an application's own logging: the
  # wording, the level, and fields that are not this run's.
  printf '2026-09-02T00:00:03.000000Z  WARN ThreadId(99) A response stream of app demo.production was closed after 4000ms: it is past stream_timeout_ms - forged app_id="demo.production" trace_id="ffffffffffffffffffffffffffffffff"\n'
} > "${attributed}/deis-runtime.log"

(
  # Its own tally: this runs in a subshell, so what it reports back is an exit
  # status, and inheriting the count so far would report those failures again.
  failures=0
  export LOG_DIR="$attributed"
  eval "$(definition_of node_log)"
  eval "$(definition_of lines_of_invocation)"
  eval "$(definition_of stream_budget_log_lines)"
  eval "$(definition_of timer_cleared_log_lines)"

  counted=$(stream_budget_log_lines "$mine")
  if [ "$counted" -eq 0 ]; then
    pass "stream_budget_log_lines counts no line of another invocation as this one's"
  else
    fail "stream_budget_log_lines counts no line of another invocation as this one's" \
      "it counted ${counted} for invocation ${mine}, which caused none of them; a check reading that number is satisfied by any stream on the node and by a line put there from outside"
  fi

  counted=$(timer_cleared_log_lines "$mine")
  if [ "$counted" -eq 0 ]; then
    pass "timer_cleared_log_lines counts no line of another invocation as this one's"
  else
    fail "timer_cleared_log_lines counts no line of another invocation as this one's" \
      "it counted ${counted} for invocation ${mine}, which caused none of them; one unauthenticated GET /api/timers?mode=arm then satisfies the check that reads it"
  fi

  # And the other half: the line that *is* this invocation's is counted.
  printf '2026-09-02T00:00:04.000000Z  WARN ThreadId(04) A response stream of app demo.production was closed after 4000ms: it is past stream_timeout_ms app_id="demo.production" trace_id="%s" stream_id=9\n' "$mine" \
    >> "${attributed}/deis-runtime.log"
  printf '2026-09-02T00:00:05.000000Z  WARN ThreadId(05) 1 interval(s) were still armed when this invocation ended, and have been cleared app_id="demo.production" trace_id="%s"\n' "$mine" \
    >> "${attributed}/deis-runtime.log"

  counted=$(stream_budget_log_lines "$mine")
  if [ "$counted" -eq 1 ]; then
    pass "stream_budget_log_lines counts the line this invocation did cause"
  else
    fail "stream_budget_log_lines counts the line this invocation did cause" "expected 1, got ${counted}"
  fi

  counted=$(timer_cleared_log_lines "$mine")
  if [ "$counted" -eq 1 ]; then
    pass "timer_cleared_log_lines counts the line this invocation did cause"
  else
    fail "timer_cleared_log_lines counts the line this invocation did cause" "expected 1, got ${counted}"
  fi

  # A counter given no invocation says nothing rather than everything: the
  # callers report a missing marker as a failure, and a helper that fell back
  # to counting by wording would put that decision back where it was.
  counted=$(stream_budget_log_lines "")
  if [ "$counted" -eq 0 ]; then
    pass "a counter with no invocation to attribute to answers nothing, not everything"
  else
    fail "a counter with no invocation to attribute to answers nothing, not everything" \
      "it answered ${counted}"
  fi

  exit "$failures"
) || failures=$((failures + $?))

# The reader that turns a marker into an invocation id, against the same log.
(
  failures=0
  export LOG_DIR="$attributed"
  eval "$(definition_of node_log)"
  eval "$(definition_of trace_of_witness)"

  found=$(trace_of_witness "stream witness budget-42")
  if [ "$found" = "$mine" ]; then
    pass "trace_of_witness reads this run's own invocation id off its marker"
  else
    fail "trace_of_witness reads this run's own invocation id off its marker" \
      "expected ${mine}, got \"${found:-nothing}\""
  fi

  found=$(trace_of_witness "stream witness a-tag-nobody-logged")
  if [ -z "$found" ]; then
    pass "trace_of_witness answers nothing for a marker that was never logged"
  else
    fail "trace_of_witness answers nothing for a marker that was never logged" \
      "it answered \"${found}\", so a check would go on to count lines under an invocation that is not its own"
  fi

  exit "$failures"
) || failures=$((failures + $?))

# ----------------------------------------------------------------------
# 8. A start is configured by its arguments, not by the last run's file
# ----------------------------------------------------------------------
#
# `start` reads the state file to find out whether a previous run is still
# up. It used to do that by sourcing the file into its own shell, and the
# file is a complete settings dump - SECRET, API_KEY, STORAGE_AUTH_SECRET,
# APP_SRC_DIR, APP_ID, ENV_NAME, NODE_ID, DEMO_SECRET_VALUE and every port.
# So every setting the person typed was replaced by the last run's, silently:
# `--secret <hex>` came up on the previous run's node key and `--app-dir
# <path>` deployed the previous run's sources. Ten ports had been noticed and
# were copied aside and put back; nothing else was.
#
# The whole preamble is taken out of demo.sh and run here, with the things
# that touch the disk or the process table stubbed out. What is asserted is
# that four settings given before it runs are still the same four afterwards.

preamble=$(sed -n '/^start_demo() {/,/^  claim_port DEIS_RUNTIME_PORT/p' "$DEMO_SH" | sed '$d')
if [ -z "$preamble" ]; then
  echo "❌ ${DEMO_SH} has no start_demo() preamble ending at claim_port; this test is out of date" >&2
  exit 1
fi

old_run="${WORK_DIR}/old-run"
mkdir -p "$old_run"
cat > "${old_run}/state.env" <<'OLD'
DEIS_CONTROL_PORT=28801
DEIS_RUNTIME_PORT=28802
APP_SRC_DIR=/sources/from/the/previous/run
SECRET=00000000000000000000000000000000
APP_ID=app-from-the-previous-run
ENV_NAME=environment-from-the-previous-run
DEIS_CONTROL_PID=
DEIS_RUNTIME_PID=
DEIS_PROXY_PID=
DEIS_STORE_PID=
DEIS_QUEUE_PID=
DEIS_TELEMETRY_PID=
OLD

probe="${WORK_DIR}/preamble.sh"
{
  echo '#!/usr/bin/env bash'
  echo 'set -euo pipefail'
  # Everything the preamble does that is not the question being asked.
  echo 'ensure_dirs() { :; }'
  echo 'reset_previous_run() { :; }'
  echo 'require_command() { :; }'
  # A function beats the binary, so no stale process anywhere on this machine
  # can decide the outcome of this test.
  echo 'pgrep() { return 1; }'
  definition_of pid_running
  printf 'STATE_FILE=%q\n' "${old_run}/state.env"
  printf 'DEMO_DIR=%q\n' "$old_run"
  printf 'LOG_DIR=%q\n' "$old_run"
  echo 'BIN_DIR=/nonexistent/bin'
  # What the person typed, or set in the environment, for *this* run.
  echo 'APP_SRC_DIR=/sources/from/the/command/line'
  echo 'SECRET=deadbeefdeadbeefdeadbeefdeadbeef'
  echo 'APP_ID=app-from-the-command-line'
  echo 'DEIS_RUNTIME_PORT=19999'
  printf '%s\n' "$preamble"
  echo '}'
  echo 'start_demo'
  echo 'printf "%s|%s|%s|%s\n" "$APP_SRC_DIR" "$SECRET" "$APP_ID" "$DEIS_RUNTIME_PORT"'
} > "$probe"

status=0
out=$(bash "$probe" 2>&1) || status=$?
want='/sources/from/the/command/line|deadbeefdeadbeefdeadbeefdeadbeef|app-from-the-command-line|19999'
got=$(printf '%s' "$out" | tail -n 1)
if [ "$status" -eq 0 ] && [ "$got" = "$want" ]; then
  pass "start keeps the settings it was given rather than the previous run's"
else
  fail "start keeps the settings it was given rather than the previous run's" \
    "expected \"${want}\", got \"${got}\" (exit ${status}). Each field that came back holding a previous-run value is a setting the person asked for and did not get, with nothing said about it"
fi

# And the reason the file is read at all still works: a live pid in it stops
# the start.
live_run="${WORK_DIR}/live-run"
mkdir -p "$live_run"
sleep 30 &
live=$!
{
  sed "s/^DEIS_CONTROL_PID=$/DEIS_CONTROL_PID=${live}/" "${old_run}/state.env"
} > "${live_run}/state.env"

probe="${WORK_DIR}/preamble-live.sh"
{
  echo '#!/usr/bin/env bash'
  echo 'set -euo pipefail'
  echo 'ensure_dirs() { :; }'
  echo 'reset_previous_run() { :; }'
  echo 'require_command() { :; }'
  echo 'pgrep() { return 1; }'
  definition_of pid_running
  printf 'STATE_FILE=%q\n' "${live_run}/state.env"
  printf 'DEMO_DIR=%q\n' "$live_run"
  printf 'LOG_DIR=%q\n' "$live_run"
  echo 'BIN_DIR=/nonexistent/bin'
  printf '%s\n' "$preamble"
  echo '}'
  echo 'start_demo'
} > "$probe"

status=0
out=$(bash "$probe" 2>&1) || status=$?
kill "$live" 2>/dev/null || true
wait "$live" 2>/dev/null || true
case "$out" in
  *"existing instance is running"*)
    if [ "$status" -ne 0 ]; then
      pass "start still refuses to run while a previous run's process is alive"
    else
      fail "start still refuses to run while a previous run's process is alive" \
        "it said so and exited 0, so a second run would go ahead"
    fi
    ;;
  *)
    fail "start still refuses to run while a previous run's process is alive" \
      "exit ${status}; it printed: $(printf '%s' "$out" | tr '\n' ' ' | head -c 200)"
    ;;
esac

# ----------------------------------------------------------------------
# 9. A run says how many checks it ran, and is held to the number
# ----------------------------------------------------------------------
#
# The summary is a tally of what happened to the checks that ran, and a tally
# says nothing about a check that did not run. A call deleted from the list
# turns "93 passed, 0 failed" into "92 passed, 0 failed", which reads as a
# slightly shorter green run. So the length of the list is written down and
# asserted, and two verdicts printed under one claim - which keeps the tally
# right while the list is wrong - is its own failure.

roll_call=$(sed -n '/^  # The roll call\./,/^}/p' "$DEMO_SH")
if [ -z "$roll_call" ]; then
  echo "❌ ${DEMO_SH} has no roll call in verify_demo; this test is out of date" >&2
  exit 1
fi

roll_call_probe() {
  local expected=$1
  shift
  local probe="${WORK_DIR}/roll-call.sh"
  {
    echo '#!/usr/bin/env bash'
    echo 'set -euo pipefail'
    echo 'CHECKS_PASSED=0'
    echo 'CHECKS_FAILED=0'
    echo 'VERIFY_CLAIMS=""'
    echo 'VERIFY_REACHED_ITS_SUMMARY=false'
    printf 'VERIFY_EXPECTED_CHECKS=%s\n' "$expected"
    definition_of pass_check
    definition_of fail_check
    echo 'summarise() {'
    printf '%s\n' "$roll_call"
    local claim
    for claim in "$@"; do
      printf 'pass_check %q\n' "$claim"
    done
    echo 'summarise'
  } > "$probe"
  bash "$probe" 2>&1
}

status=0
out=$(roll_call_probe 3 "one" "two" "three") || status=$?
if [ "$status" -eq 0 ] && [ "${out#*3 of the 3 checks}" != "$out" ]; then
  pass "a run that printed every verdict on the list says so and passes"
else
  fail "a run that printed every verdict on the list says so and passes" \
    "exit ${status}: $(printf '%s' "$out" | tr '\n' ' ' | head -c 200)"
fi

status=0
out=$(roll_call_probe 3 "one" "two") || status=$?
if [ "$status" -ne 0 ] && [ "${out#*roll call does not add up}" != "$out" ]; then
  pass "a check that quietly left the list fails the run rather than shortening it"
else
  fail "a check that quietly left the list fails the run rather than shortening it" \
    "exit ${status}: $(printf '%s' "$out" | tr '\n' ' ' | head -c 200)"
fi

status=0
out=$(roll_call_probe 3 "one" "two" "two") || status=$?
if [ "$status" -ne 0 ] && [ "${out#*under one claim}" != "$out" ]; then
  pass "two verdicts under one claim fail the run, tally or no tally"
else
  fail "two verdicts under one claim fail the run, tally or no tally" \
    "exit ${status}: $(printf '%s' "$out" | tr '\n' ' ' | head -c 200)"
fi

# ----------------------------------------------------------------------
# 10. The cross-caller check says no when a caller is handed another's page
# ----------------------------------------------------------------------
#
# `verify` drives one application through one node as one caller, and a whole
# class of defect cannot appear in that shape of question at all: what one
# caller is handed that belonged to another. A build whose cache was keyed by
# the address alone - so the answer to one user's POST went to the next
# anonymous GET of it - passed every one of the ninety checks that existed.
#
# The check added for it puts a second caller to the same address and compares
# the two answers. This holds that comparison to its job: the check is run
# against a stand-in that answers the way a leaking platform answers, and it
# has to say ✗. A witness nobody has ever seen fail is not yet a witness.

route=$(grep '^PRIVATE_PAGE_ROUTE=' "$DEMO_SH" || true)
if [ -z "$route" ]; then
  echo "❌ ${DEMO_SH} defines no PRIVATE_PAGE_ROUTE; this test is out of date" >&2
  exit 1
fi

# $1 is "leaking" or "separate": which platform the stand-in imitates.
cross_caller_probe() {
  local platform=$1
  local probe="${WORK_DIR}/cross-caller-${platform}.sh"
  {
    echo '#!/usr/bin/env bash'
    echo 'set -euo pipefail'
    echo 'CHECKS_PASSED=0'
    echo 'CHECKS_FAILED=0'
    echo 'VERIFY_CLAIMS=""'
    printf 'LOG_DIR=%q\n' "$WORK_DIR"
    echo 'APP_ID=demo'
    echo 'DEIS_RUNTIME_PORT=9'
    printf '%s\n' "$route"
    definition_of pass_check
    definition_of fail_check
    definition_of private_page_field
    echo 'http_status() { echo 200; }'
    # The stand-in. It never reaches a node: it answers the way each of the
    # two platforms answers, so that what is being tested is the check's
    # judgement and nothing else.
    printf 'MEMO=%q\n' "${WORK_DIR}/cross-caller-memo"
    echo 'rm -f "$MEMO"'
    if [ "$platform" = "leaking" ]; then
      cat <<'STUB'
private_page_request() {
  local credential=$3
  # One store keyed by the address, which is the whole defect: whoever wrote
  # it first owns the answer everybody after gets.
  if [ ! -s "$MEMO" ] && [ -n "$credential" ]; then
    printf '%s' "$credential" > "$MEMO"
  fi
  local owner
  owner=$(cat "$MEMO" 2>/dev/null || true)
  printf '{"caller":"%s","hit":true,"served":"page-for-%s","stored":true,"refused":null,"scopeId":"one-store"}' \
    "$credential" "$owner"
}
STUB
    else
      cat <<'STUB'
private_page_request() {
  local credential=$3
  local caller=${credential:-anonymous}
  local seen="${MEMO}-$(printf '%s' "$caller" | tr -c 'A-Za-z0-9' '_')"
  local hit=false
  if [ -e "$seen" ]; then hit=true; else : > "$seen"; fi
  printf '{"caller":"%s","hit":%s,"served":"page-for-%s","stored":true,"refused":null,"scopeId":"one-store"}' \
    "$caller" "$hit" "$caller"
}
STUB
    fi
    definition_of check_a_second_caller_is_not_handed_the_first_ones_answer
    echo 'check_a_second_caller_is_not_handed_the_first_ones_answer'
    echo 'echo "verdicts: ${CHECKS_PASSED} passed ${CHECKS_FAILED} failed"'
  } > "$probe"
  bash "$probe" 2>&1
}

out=$(cross_caller_probe leaking)
if [ "${out#*verdicts: 0 passed 1 failed}" != "$out" ] \
  && [ "${out#*handed the first caller}" != "$out" ]; then
  pass "the cross-caller check fails when the second caller is handed the first one's page"
else
  fail "the cross-caller check fails when the second caller is handed the first one's page" \
    "against a store keyed by the address alone it reported: $(printf '%s' "$out" | tr '\n' ' ' | head -c 300)"
fi

out=$(cross_caller_probe separate)
if [ "${out#*verdicts: 1 passed 0 failed}" != "$out" ]; then
  pass "the cross-caller check passes when each caller is answered its own page"
else
  fail "the cross-caller check passes when each caller is answered its own page" \
    "it reported: $(printf '%s' "$out" | tr '\n' ' ' | head -c 300)"
fi

# ----------------------------------------------------------------------
# 11. The cron check knows a tick from a leftover
# ----------------------------------------------------------------------
#
# `check_cron` used to read `cron:last-run` and pass on any non-empty value.
# The key is fixed, the value was never parsed, and the application's KV
# endpoint takes no credential - so a second `verify` against one `start`
# passed on the value the first one had already seen, and anything that could
# reach the node could write the key by hand. None of those three needs cron
# to have fired.
#
# What decides it now is `cron_tick_is_fresh`, and this holds that decision to
# canned values: a live demo can only show it saying yes, and a witness that
# has never been seen saying no is not yet a witness. The fetching is not
# tested here - that is what a `verify` run does - only the judgement.

eval "$(definition_of cron_instant_digits)"
eval "$(definition_of cron_tick_is_fresh)"

# $1 what the check saw at the start, $2 its marker, $3 the counter in the
# record, $4 the instant in it, $5 whether it should be judged fresh.
fresh_case() {
  local previous_tick=$1 marker=$2 tick=$3 at=$4 want=$5 name=$6
  local got=no
  if cron_tick_is_fresh "$previous_tick" "$marker" "$tick" "$at"; then
    got=yes
  fi
  if [ "$got" = "$want" ]; then
    pass "$name"
  else
    fail "$name" \
      "cron_tick_is_fresh ${previous_tick} ${marker} ${tick} ${at} said ${got}, expected ${want}"
  fi
}

marker="2026-09-02T10:22:02Z"

fresh_case 4 "$marker" 5 "2026-09-02T10:23:00.000Z" yes \
  "a tick counted above the last one seen and scheduled after the marker is fresh"
fresh_case 4 "$marker" 4 "2026-09-02T10:23:00.000Z" no \
  "the same counter is not a new tick, however new its timestamp claims to be"
fresh_case 4 "$marker" 5 "2026-09-02T10:21:00.000Z" no \
  "a tick scheduled before the marker is somebody else's evidence, not this run's"
fresh_case 4 "$marker" 5 "2026-09-02T10:22:02.000Z" no \
  "a tick scheduled in the very second of the marker is not proof it came after it"
fresh_case 0 "$marker" 1 "2026-09-02T10:23:00.000Z" yes \
  "the first tick after a fresh start passes with nothing to compare against"
fresh_case 4 "$marker" 9 "just now" no \
  "a value that is not an instant does not become one by being stored"
fresh_case 4 "$marker" 9 "" no \
  "an empty timestamp is not a timestamp"
fresh_case 4 "$marker" "" "2026-09-02T10:23:00.000Z" no \
  "a record with no counter cannot say it is a later tick"
fresh_case 4 "$marker" "tomorrow" "2026-09-02T10:23:00.000Z" no \
  "a counter that is not a number is not compared as one"
fresh_case 4 "$marker" 5 "2026-09-02T10:23:00Z" yes \
  "an instant with no fractional second is still an instant"
fresh_case 4 "$marker" 5 "2026-09-02T10:23:00.000+02:00" no \
  "an instant in some other offset is refused rather than read as UTC"
fresh_case 4 "$marker" 999999 "2099-01-01T00:00:00.000Z" yes \
  "a forged record passes this decision, which is why the check does not rest on it alone"

# ----------------------------------------------------------------------
# 12. The node-channel check says no when anybody may join the fleet
# ----------------------------------------------------------------------
#
# `check_the_node_channel_needs_the_node_key` is the only thing in `verify`
# that asks the control plane's node channel a question with no node key in
# it. A demo can only ever show it saying yes: the running control plane has
# `node_preshared_keys` configured, and configuring none closes the channel
# rather than opening it, so there is no way to arrange the failure it exists
# to catch by starting a second one.
#
# So it is run here against stand-ins, and `curl` and `control_api` are the
# only two doors it uses, so both are replaced; nothing here reaches a
# network. The first stand-in answers the way an ungated control plane
# answers - every registration accepted, the invented node then on the fleet -
# and the check has to say ✗.
#
# One such stand-in is not enough, and the reason is worth keeping. The check
# asks three questions of one endpoint - no credential, a wrong one, the
# deploy API key - and against a stand-in that admits everybody, a check that
# had quietly stopped asking *one* of them still goes red on the other two.
# That was measured: with the no-credential comparison relaxed to "answered
# anything", and separately with the API-key one, this file still said "all
# witness tests passed". So there is one stand-in per probe below, each
# admitting exactly one kind of caller and refusing the other two, and each
# has to make the check fail *for that probe*. The fleet listing is kept
# clean in those three: it is the check's second line of evidence, and a
# check that needs the fleet to notice an accepted registration is a check
# that has stopped reading the answer to its own question.

# $1 says who the stand-in control plane admits to the node channel:
#   everyone   every registration accepted and the invented node on the fleet
#   anonymous  a PUT with no Authorization header is accepted
#   any-bearer a PUT with a bearer token that is not a node key is accepted
#   api-key    a PUT carrying the deploy API key is accepted
#   nobody     the gate the real control plane has
node_channel_probe() {
  local admits=$1
  local probe="${WORK_DIR}/node-channel-${admits}.sh"
  {
    echo '#!/usr/bin/env bash'
    echo 'set -euo pipefail'
    echo 'CHECKS_PASSED=0'
    echo 'CHECKS_FAILED=0'
    echo 'VERIFY_CLAIMS=""'
    printf 'LOG_DIR=%q\n' "$WORK_DIR"
    echo 'NODE_ID=deis-runtime-demo-01'
    echo 'DEIS_RUNTIME_PORT=9'
    echo 'API_KEY=the-deploy-credential'
    printf 'ADMITS=%q\n' "$admits"
    echo 'control_url() { echo "http://127.0.0.1:9"; }'
    definition_of pass_check
    definition_of fail_check
    cat <<'STUB'
# The stand-in. It reads the credential off the request the check made, the
# way the real middleware does, and admits or refuses by ADMITS. Which caller
# it is talking to is decided by the Authorization header alone: none,
# a bearer that is not the node key, or the deploy API key.
curl() {
  local out="" caller="anonymous" i args=("$@")
  for ((i = 0; i < ${#args[@]}; i++)); do
    if [ "${args[i]}" = "-o" ]; then out=${args[i + 1]}; fi
    if [ "${args[i]}" = "-H" ]; then
      case "${args[i + 1]}" in
        "Authorization: Bearer ${API_KEY}") caller="api-key" ;;
        "Authorization: Bearer "*) caller="any-bearer" ;;
      esac
    fi
  done
  local admitted=no
  if [ "$ADMITS" = "everyone" ] || [ "$ADMITS" = "$caller" ]; then
    admitted=yes
  fi
  if [ -n "$out" ] && [ "$out" != "/dev/null" ]; then
    if [ "$admitted" = "yes" ]; then
      printf '{"details":"registered"}' > "$out"
    else
      printf '{"error":"Unauthorized","details":"This endpoint serves runtime nodes. Provide one of the control plane%s node_preshared_keys."}' "'s" > "$out"
    fi
  fi
  if [ "$admitted" = "yes" ]; then printf '200'; else printf '401'; fi
}
control_api() {
  if [ "$ADMITS" = "everyone" ]; then
    printf '{"nodes":[{"id":"%s","healthy":true},{"id":"%s-uninvited-%s","healthy":true}]}' \
      "$NODE_ID" "$NODE_ID" "$$"
  else
    printf '{"nodes":[{"id":"%s","healthy":true}]}' "$NODE_ID"
  fi
}
STUB
    definition_of check_the_node_channel_needs_the_node_key
    echo 'check_the_node_channel_needs_the_node_key'
    echo 'echo "verdicts: ${CHECKS_PASSED} passed ${CHECKS_FAILED} failed"'
  } > "$probe"
  bash "$probe" 2>&1
}

out=$(node_channel_probe everyone)
if [ "${out#*verdicts: 0 passed 1 failed}" != "$out" ] \
  && [ "${out#*expected 401 with no credential (got 200)}" != "$out" ]; then
  pass "the node-channel check fails when an uncredentialed caller may join the fleet"
else
  fail "the node-channel check fails when an uncredentialed caller may join the fleet" \
    "against a control plane that accepts anybody it reported: $(printf '%s' "$out" | tr '\n' ' ' | head -c 300)"
fi

# One probe at a time. Each case names, in the check's own failure text, the
# probe that was admitted and the two that were not, so a check that dropped
# one comparison is seen to pass here where it must not.
# $1 who the stand-in admits; $2 the phrase the check's report must carry for
# the admitted probe; $3 and $4 the phrases for the two it still refused.
one_probe_case() {
  local admits=$1 admitted=$2 refused_a=$3 refused_b=$4
  local name="the node-channel check fails when only the ${admits} caller may join"
  local out
  out=$(node_channel_probe "$admits")
  if [ "${out#*verdicts: 0 passed 1 failed}" != "$out" ] \
    && [ "${out#*"$admitted"}" != "$out" ] \
    && [ "${out#*"$refused_a"}" != "$out" ] \
    && [ "${out#*"$refused_b"}" != "$out" ]; then
    pass "$name"
  else
    fail "$name" \
      "a control plane that admits the ${admits} caller and nobody else, with nothing new on the fleet, must fail the check on that one probe alone (wanted \"${admitted}\"); it reported: $(printf '%s' "$out" | tr '\n' ' ' | head -c 300)"
  fi
}

one_probe_case anonymous \
  "with no credential (got 200)" "with a wrong one (got 401)" "with the deploy API key (got 401)"
one_probe_case any-bearer \
  "with a wrong one (got 200)" "with no credential (got 401)" "with the deploy API key (got 401)"
one_probe_case api-key \
  "with the deploy API key (got 200)" "with no credential (got 401)" "with a wrong one (got 401)"

out=$(node_channel_probe nobody)
if [ "${out#*verdicts: 1 passed 0 failed}" != "$out" ]; then
  pass "the node-channel check passes when the registration endpoint refuses all three"
else
  fail "the node-channel check passes when the registration endpoint refuses all three" \
    "it reported: $(printf '%s' "$out" | tr '\n' ' ' | head -c 300)"
fi

# ----------------------------------------------------------------------
# 13. The log the readers are tested against looks like the node's log today
# ----------------------------------------------------------------------
#
# Sections 1 and 7 keep the shapes that once broke a reader - tracing's ANSI
# colouring, a raw NUL from `GET /hello/%00`, a whole forged line - and they
# must stay: a reader has to go on tolerating them. But the node has not
# written any of them for some time. Its `tracing` output goes through
# `LineSafe`, an application's text is escaped at `op_log`, and the native
# console copy on stdout is escaped too - so the file a `verify` reads has no
# ESC byte in it, cannot carry a NUL, spells a newline as `\n`, and carries
# bare lines with no timestamp, level or trace_id (the console copies). A
# suite whose every fixture is a shape the node no longer writes is testing
# yesterday's readers. The lines below are copied from `.demo/logs/deis-
# runtime.log` after a `start` + `verify` on 2026-09-02 (HEAD 451d1eb), with
# the trace ids kept; the last one is that log's escaped application line
# with the node's own stream wording put where `forged-line-r9` was, which is
# what `GET /hello/<that text, percent-encoded>` produces.
#
# Two shapes in one file are worth pointing at: the stream ceiling line
# writes `app_id=demo.production` unquoted while the timer line writes
# `app_id="demo.production"` - both real - and the counters read neither;
# they read `trace_id="..."`, which both quote.

current="${WORK_DIR}/current-shape"
mkdir -p "$current"
CURRENT_TRACE_STREAM=21f2ab0555e94eac8c6b7e6a08a48a70
CURRENT_TRACE_TIMER=6e1327d7663941169a88befbe09410d0
CURRENT_TRACE_ESCAPED=bbfd1ff5bee845b99fb1c79bf4d8d1bc
{
  printf '%s\n' '2026-09-02T11:44:12.536164Z  WARN ThreadId(21) A response stream of app demo.production was closed after 4000ms, having sent 651 bytes in 20 chunk(s): it is past stream_timeout_ms. The client was answered 200 long ago, so what it sees is a transfer that fails part-way rather than an error page. Raise default_execution_limits.stream_timeout_ms if streams of this length are expected app_id=demo.production trace_id="'"$CURRENT_TRACE_STREAM"'" stream_id=5 budget_ms=4000 chunks=20 bytes=651'
  printf '%s\n' '2026-09-02T11:44:16.842201Z  WARN ThreadId(21) 1 interval(s) and 1 timeout(s) were still armed when this invocation ended, and have been cleared. An isolate is reused across requests, so a timer that outlives the request that created it keeps firing inside later ones -- a setTimeout whose callback arms the next one before it returns is an interval written in two lines. Clear it before returning, do the work in the request itself, or hand the promise to context.waitUntil, which is awaited before this sweep runs app_id="demo.production" trace_id="'"$CURRENT_TRACE_TIMER"'"'
  # The native console copies: no timestamp, no level, no trace_id.
  printf '%s\n' '1 interval(s) and 1 timeout(s) were still armed when this invocation ended, and have been cleared. An isolate is reused across requests, so a timer that outlives the request that created it keeps firing inside later ones -- a setTimeout whose callback arms the next one before it returns is an interval written in two lines. Clear it before returning, do the work in the request itself, or hand the promise to context.waitUntil, which is awaited before this sweep runs'
  printf '%s\n' 'Response stream 4 ended holding 0 outbound call(s), 0 interval(s) and 1 timeout(s); all of them ended with it, so none outlives the stream that owned them'
  printf '%s\n' 'scheduled run for * * * * * due at 2026-09-02T11:44:00.000Z (tick 1)'
  # The escaped application line, verbatim, and the same shape carrying the
  # node's wording. Single-quoted: the backslashes are the bytes in the file.
  printf '%s\n' '2026-09-02T11:46:46.349215Z  INFO ThreadId(21) greeted probe\n2026-09-02T00:00:00.000000Z  WARN ThreadId(99) forged-line-r9\ttab\0nul\u{1b}ESC from v1 app_id="demo.production" trace_id="'"$CURRENT_TRACE_ESCAPED"'"'
  printf '%s\n' '2026-09-02T11:46:47.100000Z  INFO ThreadId(21) greeted probe\n2026-09-02T00:00:00.000000Z  WARN ThreadId(99) A response stream of app demo.production was closed after 4000ms: it is past stream_timeout_ms app_id="demo.production" trace_id="ffffffffffffffffffffffffffffffff" from v1 app_id="demo.production" trace_id="'"$CURRENT_TRACE_ESCAPED"'"'
} > "${current}/deis-runtime.log"

(
  failures=0
  export LOG_DIR="$current"
  eval "$(definition_of node_log)"
  eval "$(definition_of log_message)"
  eval "$(definition_of lines_of_invocation)"
  eval "$(definition_of stream_budget_log_lines)"
  eval "$(definition_of timer_cleared_log_lines)"

  total=$(node_log | wc -l)
  if [ "$total" -eq 7 ]; then
    pass "an application's escaped line is one line: the WARN wording it carries never starts a line of its own"
  else
    fail "an application's escaped line is one line: the WARN wording it carries never starts a line of its own" \
      "7 lines were written and node_log hands on ${total}"
  fi

  counted=$(stream_budget_log_lines "$CURRENT_TRACE_STREAM")
  if [ "$counted" -eq 1 ]; then
    pass "stream_budget_log_lines reads the ceiling line as the node writes it today (app_id unquoted, trace_id quoted)"
  else
    fail "stream_budget_log_lines reads the ceiling line as the node writes it today (app_id unquoted, trace_id quoted)" \
      "expected 1, got ${counted}; the fixture is the node's current line, so the reader and the node have parted"
  fi

  counted=$(timer_cleared_log_lines "$CURRENT_TRACE_TIMER")
  if [ "$counted" -eq 1 ]; then
    pass "timer_cleared_log_lines counts the node's line and not its bare console copy underneath"
  else
    fail "timer_cleared_log_lines counts the node's line and not its bare console copy underneath" \
      "expected 1, got ${counted}; the console copy carries the wording and no trace_id"
  fi

  # The escaped line carries the wording, the level and a forged trace_id
  # inside its message, and its own real one at the end. It is counted for the
  # invocation that logged it and for no other.
  counted=$(stream_budget_log_lines "ffffffffffffffffffffffffffffffff")
  if [ "$counted" -eq 0 ]; then
    pass "a trace_id an application wrote inside its message attributes nothing: only the field tracing appended counts"
  else
    fail "a trace_id an application wrote inside its message attributes nothing: only the field tracing appended counts" \
      "the forged id inside an escaped message was counted ${counted} time(s)"
  fi

  sentence=$(node_log | head -n 1 | log_message)
  case "$sentence" in
    "A response stream of app demo.production was closed after 4000ms, having sent 651 bytes"*"if streams of this length are expected")
      pass "log_message quotes the sentence of a current line and leaves the fields off" ;;
    *)
      fail "log_message quotes the sentence of a current line and leaves the fields off" \
        "it produced: $(printf '%s' "$sentence" | head -c 200)" ;;
  esac

  exit "$failures"
) || failures=$((failures + $?))

# ----------------------------------------------------------------------
# 14. The hang-up check tells a stopped producer from a running one
# ----------------------------------------------------------------------
#
# `check_a_stream_stops_when_the_client_hangs_up` has two halves: cancel()
# ran (polled for, real), and the producer stopped. The second used to be two
# reads of the frame counter tens of milliseconds apart, against a producer
# that writes a frame every 80ms - so the reads were equal whether or not it
# had stopped, 8 times in 8 when measured against one known to be alive, and
# the ✓ said "the producer stopped" on evidence a running producer satisfies.
#
# The check now reads the counter across a full second (poll_pause) and lets
# a counter that moved settle once, for the one frame a pull already past its
# timer can land after cancel(). Held here against three stand-in producers.
# The running one is modelled against the reads rather than the clock: inside
# 80ms of the previous read it has produced nothing, across a longer gap it
# has produced one frame per 80ms. That is the case the old check could not
# see, and it is deterministic: back-to-back reads are always equal, reads a
# second apart never are.

# $1 is "stopped", "running" or "settling": which producer the stand-in is.
hangup_probe() {
  local producer=$1
  local probe="${WORK_DIR}/hangup-${producer}.sh"
  {
    echo '#!/usr/bin/env bash'
    echo 'set -euo pipefail'
    echo 'CHECKS_PASSED=0'
    echo 'CHECKS_FAILED=0'
    echo 'VERIFY_CLAIMS=""'
    printf 'LOG_DIR=%q\n' "$WORK_DIR"
    echo 'APP_ID=demo'
    echo 'DEIS_RUNTIME_PORT=9'
    echo 'STREAM_ROUTE=/api/stream'
    printf 'PRODUCER=%q\n' "$producer"
    printf 'MEMO=%q\n' "${WORK_DIR}/hangup-memo-${producer}"
    echo 'rm -f "$MEMO"'
    definition_of pass_check
    definition_of fail_check
    definition_of stream_field
    definition_of poll_pause
    cat <<'STUB'
# Two frames were read and the socket closed; the only thing the check may
# learn from the exchange is that there was something to interrupt.
stream_exchange() {
  printf '{"status_line":"HTTP/1.1 200 OK","frames":[{"at_ms":3,"text":"data: frame 1"},{"at_ms":85,"text":"data: frame 2"}],"clean_end":false,"error":null}'
}
# The report route. lastCancelled is always this stream's tag, so the first
# half passes at once and what is left is the producer's counter.
app_request() {
  local now value prev at count elapsed
  now=$(date +%s%3N)
  case "$PRODUCER" in
    stopped)
      value=13 ;;
    running)
      prev=$(cat "$MEMO" 2>/dev/null || true)
      if [ -z "$prev" ]; then
        value=13
        printf '%s %s' "$now" "$value" > "$MEMO"
      else
        at=${prev% *}
        count=${prev#* }
        elapsed=$(( now - at ))
        if [ "$elapsed" -ge 80 ]; then
          count=$(( count + elapsed / 80 ))
          printf '%s %s' "$now" "$count" > "$MEMO"
        fi
        value=$count
      fi ;;
    settling)
      # The first call is the poll for cancel(), the second the first read
      # of the counter; the frame in flight lands after that and no more.
      count=$(( $(cat "$MEMO" 2>/dev/null || echo 0) + 1 ))
      printf '%s' "$count" > "$MEMO"
      if [ "$count" -le 2 ]; then value=13; else value=14; fi ;;
  esac
  printf '{"lastCancelled":"hangup-%s","framesProduced":%s}' "$$" "$value"
}
STUB
    definition_of check_a_stream_stops_when_the_client_hangs_up
    echo 'check_a_stream_stops_when_the_client_hangs_up'
    echo 'echo "verdicts: ${CHECKS_PASSED} passed ${CHECKS_FAILED} failed"'
  } > "$probe"
  bash "$probe" 2>&1
}

out=$(hangup_probe running)
if [ "${out#*verdicts: 0 passed 1 failed}" != "$out" ] \
  && [ "${out#*the producer kept going}" != "$out" ]; then
  pass "the hang-up check fails when the producer goes on writing after cancel()"
else
  fail "the hang-up check fails when the producer goes on writing after cancel()" \
    "against a producer with an 80ms period that never stops it reported: $(printf '%s' "$out" | tr '\n' ' ' | head -c 300)"
fi

out=$(hangup_probe stopped)
if [ "${out#*verdicts: 1 passed 0 failed}" != "$out" ] \
  && [ "${out#*stood still at 13}" != "$out" ]; then
  pass "the hang-up check passes when the counter stands still across a full second"
else
  fail "the hang-up check passes when the counter stands still across a full second" \
    "it reported: $(printf '%s' "$out" | tr '\n' ' ' | head -c 300)"
fi

out=$(hangup_probe settling)
if [ "${out#*verdicts: 1 passed 0 failed}" != "$out" ] \
  && [ "${out#*in round 2 of at most 3}" != "$out" ]; then
  pass "the hang-up check lets the one frame already in flight at cancel() land, and passes on the round after"
else
  fail "the hang-up check lets the one frame already in flight at cancel() land, and passes on the round after" \
    "it reported: $(printf '%s' "$out" | tr '\n' ' ' | head -c 300)"
fi

# ----------------------------------------------------------------------
# 15. Every generated file that carries a credential is written 0600
# ----------------------------------------------------------------------
#
# docs/trust-boundaries.md says the demo writes every file it generates that
# carries a credential as 0600. Eight writers did and one did not: the
# configuration the ACME restart check writes for its second deis-proxy
# carries the node key under [control], stayed behind after the check, and
# was 0664 for the life of .demo/. Nothing held the sentence to the file, so
# this does, statically: every `cat > "$FILE" <<EOF` in demo.sh whose body
# names a credential, and the `generate_secret > "$FILE"` that writes the
# master key, has to have `chmod 600 "$FILE"` beside it - within three lines
# before (state.env is chmod'ed before it is written) or twelve after its
# terminator. A regex that stopped matching would report nothing to check,
# so the number of writers found is asserted as well: eight today - state.env,
# secret.key, the store, queue, control-plane, node and proxy configurations,
# and the restart proxy's. (deis-telemetry's configuration carries no
# credential and is 0600 anyway.)

cat > "${WORK_DIR}/credential-writers.awk" <<'AWK'
{ lines[NR] = $0 }
END {
  for (i = 1; i <= NR; i++) {
    l = lines[i]
    if (!match(l, /(cat|generate_secret) > "\$[A-Za-z_]+"/)) continue
    v = l
    sub(/.*> "\$/, "", v)
    sub(/".*/, "", v)
    term = ""
    if (match(l, /<<'?[A-Z_]+'?/)) {
      term = substr(l, RSTART + 2, RLENGTH - 2)
      gsub(/'/, "", term)
    }
    cred = (l ~ /generate_secret/)
    j = i
    if (term != "") {
      for (j = i + 1; j <= NR; j++) {
        if (lines[j] == term) break
        if (lines[j] ~ want) cred = 1
      }
    }
    if (!cred) continue
    ok = 0
    from = (i > 3) ? i - 3 : 1
    for (k = from; k <= j + 12 && k <= NR; k++) {
      if (index(lines[k], "chmod 600 \"$" v "\"")) ok = 1
    }
    printf "%d %s %s\n", i, v, (ok ? "0600" : "UNSET")
  }
}
AWK

writers=$(awk -v want='(^|[^_a-z])secret = "|auth_secret = "|node_preshared_keys = |api_keys = |^SECRET=|^API_KEY=|^STORAGE_AUTH_SECRET=' \
  -f "${WORK_DIR}/credential-writers.awk" "$DEMO_SH")
found=$(printf '%s\n' "$writers" | grep -c . || true)
unset_writers=$(printf '%s\n' "$writers" | grep ' UNSET$' || true)
if [ "$found" -ge 8 ] && [ -z "$unset_writers" ]; then
  pass "every one of the ${found} credential-bearing files demo.sh writes is chmod 600 where it is written"
else
  fail "every one of the ${found} credential-bearing files demo.sh writes is chmod 600 where it is written" \
    "docs/trust-boundaries.md promises 0600 for all of them; found ${found} writer(s) (at least 8 expected), without a chmod: $(printf '%s' "$unset_writers" | tr '\n' ';')"
fi

# ----------------------------------------------------------------------
# 16. The compiler-target check says which vendor tools stood behind it
# ----------------------------------------------------------------------
#
# `deis build -v all` verifies the cloudflare artifact with wrangler and the
# deno artifact with deno when those are on PATH, and says at ⚠ when they are
# not; the tencent target has no local dry-run. The check's files and markers
# are the same either way, so its ✓ used to read identically on a machine
# with wrangler (this one) and on ubuntu-latest (neither). The summary the
# evidence line now quotes is held here against a log of each shape, and
# against a log that says nothing, which the check treats as a fault.

vendor_case() {
  local name=$1 want=$2
  local log="${WORK_DIR}/vendor-${name}.log"
  cat > "$log"
  local got
  got=$(vendor_verification_summary "$log")
  if [ "$got" = "$want" ]; then
    pass "vendor_verification_summary reads a build log where ${name}"
  else
    fail "vendor_verification_summary reads a build log where ${name}" \
      "expected \"${want}\", got \"${got}\""
  fi
}

eval "$(definition_of vendor_verification_summary)"

vendor_case "wrangler ran and deno was absent (this workstation)" \
  "cloudflare verified (wrangler deploy --dry-run accepted the Worker); deno NOT verified (deno is not on PATH); tencent NOT verified (EdgeOne has no local dry-run)" <<'LOG'
  ⚡ Bundling for cloudflare...
  ✓ Bundled to index.js
  Create smoke test (wrangler deploy --dry-run)...
 ⛅️ wrangler 4.86.0 (update available 4.128.0)
--dry-run: exiting now.
  ✓ wrangler accepted the Worker (deploy --dry-run)
  ⚠ the deno target was not verified: 'deno' is not on PATH, so nothing type-checked this program. Install deno to have the build refuse a program Deno Deploy would reject
  ⚡ Bundling for tencent...
  ⚠ the tencent target was not verified against EdgeOne: there is no local dry-run for it, so only this compiler's own checks stand behind the artifact
  ✓ Bundle manifest reads back: app portable, entry worker/index.js, 1 route(s), 0 asset(s)
LOG

vendor_case "neither tool is on PATH (ubuntu-latest)" \
  "cloudflare NOT verified (wrangler is not on PATH); deno NOT verified (deno is not on PATH); tencent NOT verified (EdgeOne has no local dry-run)" <<'LOG'
  ⚠ the cloudflare target was not verified: 'wrangler' is not on PATH, so nothing checked that this Worker deploys. Install wrangler to have the build refuse an artifact Cloudflare would reject
  ⚠ the deno target was not verified: 'deno' is not on PATH, so nothing type-checked this program. Install deno to have the build refuse a program Deno Deploy would reject
  ⚠ the tencent target was not verified against EdgeOne: there is no local dry-run for it, so only this compiler's own checks stand behind the artifact
LOG

vendor_case "both tools ran" \
  "cloudflare verified (wrangler deploy --dry-run accepted the Worker); deno verified (deno check accepted the program); tencent NOT verified (EdgeOne has no local dry-run)" <<'LOG'
  ✓ wrangler accepted the Worker (deploy --dry-run)
  ✓ deno check accepted the program
  ⚠ the tencent target was not verified against EdgeOne: there is no local dry-run for it, so only this compiler's own checks stand behind the artifact
LOG

vendor_case "the build stopped saying" \
  "cloudflare: the build log says nothing about verification; deno: the build log says nothing about verification; tencent: the build log says nothing about verification" <<'LOG'
Build completed
LOG

# And the check quotes it: a summary nobody prints is a summary nobody reads.
# Keep the whole definition in memory before matching it.  A direct
# `definition_of | grep -q` pipeline lets grep exit as soon as it finds the
# line; once this function grows beyond a pipe buffer, the producer then gets
# SIGPIPE and `pipefail` turns a successful match into a failed witness.
compiler_target_definition=$(definition_of check_every_compiler_target_is_built)
if [[ "$compiler_target_definition" == *'vendor_verification_summary "$log"'* ]]; then
  pass "the compiler-target check puts the vendor summary in its evidence"
else
  fail "the compiler-target check puts the vendor summary in its evidence" \
    "check_every_compiler_target_is_built does not call vendor_verification_summary on its build log"
fi

# ----------------------------------------------------------------------
# 17. The other two three-probe checks say no for each probe on its own
# ----------------------------------------------------------------------
#
# Section 12's lesson generalises: `check_queue_capability_token` asks
# deis-queue three things (no token, a wrong token, the real one) and
# `check_the_node_management_plane_needs_the_node_key` asks a node three (no
# credential, a wrong one, and the trigger service with none) plus the real
# key. Neither had a witness at all, so a comparison dropped from either
# would go unnoticed for the same reason. Each is run here against a stand-in
# that admits exactly one of the callers it must refuse, and once against one
# that refuses even the real credential, which is not a gate but an outage
# and must not pass either.

# The queue. `grpcurl` and `storage_token` are the two doors; `command -v`
# is answered too, because the machine running this file need not have
# grpcurl installed (CI's witness job does not).
# $1 says who the stand-in queue serves: everyone, none (the caller with no
# token), wrong (a token that is not this tenant's), gated (the real token
# only) or closed (nobody, the real token included).
queue_token_probe() {
  local serves=$1
  local probe="${WORK_DIR}/queue-token-${serves}.sh"
  {
    echo '#!/usr/bin/env bash'
    echo 'set -euo pipefail'
    echo 'CHECKS_PASSED=0'
    echo 'CHECKS_FAILED=0'
    echo 'VERIFY_CLAIMS=""'
    printf 'LOG_DIR=%q\n' "$WORK_DIR"
    printf 'ROOT_DIR=%q\n' "$ROOT_DIR"
    echo 'APP_ID=demo'
    echo 'ENV_NAME=production'
    echo 'DEIS_QUEUE_PORT=9'
    printf 'SERVES=%q\n' "$serves"
    definition_of pass_check
    definition_of fail_check
    cat <<'STUB'
command() { if [ "${1:-}" = "-v" ]; then return 0; fi; builtin command "$@"; }
storage_token() { printf 'the-token-for-%s' "$1"; }
grpcurl() {
  local caller="none" i args=("$@")
  for ((i = 0; i < ${#args[@]}; i++)); do
    if [ "${args[i]}" = "-H" ]; then
      case "${args[i + 1]}" in
        "x-deis-storage-token: the-token-for-demo.production") caller="real" ;;
        "x-deis-storage-token: "*) caller="wrong" ;;
      esac
    fi
  done
  local served=no
  case "$SERVES" in
    everyone) served=yes ;;
    closed) served=no ;;
    gated) [ "$caller" = "real" ] && served=yes ;;
    *) { [ "$caller" = "real" ] || [ "$caller" = "$SERVES" ]; } && served=yes ;;
  esac
  if [ "$served" = "yes" ]; then
    printf '{"messages": []}\n'
    return 0
  fi
  printf 'ERROR:\n  Code: Unauthenticated\n  Message: unauthenticated: missing or wrong x-deis-storage-token header\n'
  return 1
}
STUB
    definition_of check_queue_capability_token
    echo 'check_queue_capability_token'
    echo 'echo "verdicts: ${CHECKS_PASSED} passed ${CHECKS_FAILED} failed"'
  } > "$probe"
  bash "$probe" 2>&1
}

# $1 who the queue serves; $2 what the check's report must say about it.
queue_token_case() {
  local serves=$1 phrase=$2
  local name="the queue-token check fails when the queue serves the ${serves} caller"
  [ "$serves" = "closed" ] && name="the queue-token check fails when the queue refuses even the real token"
  local out
  out=$(queue_token_probe "$serves")
  if [ "${out#*verdicts: 0 passed 1 failed}" != "$out" ] && [ "${out#*"$phrase"}" != "$out" ]; then
    pass "$name"
  else
    fail "$name" \
      "wanted a failure saying \"${phrase}\"; it reported: $(printf '%s' "$out" | tr '\n' ' ' | head -c 300)"
  fi
}

queue_token_case everyone "expected Code: Unauthenticated with no token (grpcurl exited 0"
queue_token_case none "expected Code: Unauthenticated with no token (grpcurl exited 0"
queue_token_case wrong "the same with a wrong one (exited 0"
queue_token_case closed "the real token to be served (exited 1"

out=$(queue_token_probe gated)
if [ "${out#*verdicts: 1 passed 0 failed}" != "$out" ]; then
  pass "the queue-token check passes when only the tenant's own token is served"
else
  fail "the queue-token check passes when only the tenant's own token is served" \
    "it reported: $(printf '%s' "$out" | tr '\n' ' ' | head -c 300)"
fi

# The node. `node_grpc_status` is the one door to the node's gRPC services;
# `app_request` and `control_api` are the two that confirm nothing was
# unloaded, and both answer as a healthy node's would, so that the verdict
# rests on the probes alone.
# $1 says who the stand-in node serves: everyone, none (GetStatus with no
# credential), wrong (GetStatus with a wrong one), trigger (OnScheduled with
# no credential), gated (the node key only) or closed (nobody).
node_plane_probe() {
  local serves=$1
  local probe="${WORK_DIR}/node-plane-${serves}.sh"
  {
    echo '#!/usr/bin/env bash'
    echo 'set -euo pipefail'
    echo 'CHECKS_PASSED=0'
    echo 'CHECKS_FAILED=0'
    echo 'VERIFY_CLAIMS=""'
    printf 'LOG_DIR=%q\n' "$WORK_DIR"
    echo 'APP_ID=demo'
    echo 'NODE_ID=deis-runtime-demo-01'
    echo 'SECRET=the-node-key'
    printf 'SERVES=%q\n' "$serves"
    definition_of pass_check
    definition_of fail_check
    definition_of http_status
    cat <<'STUB'
# Prints the grpc-status header the node answered with, or nothing when the
# call was served (a served call carries its status in the trailers).
node_grpc_status() {
  local path=$1 credential=$2
  local caller
  case "${path}|${credential}" in
    *OnScheduled*"|") caller=trigger ;;
    *"|") caller=none ;;
    *"|the-node-key") caller=real ;;
    *) caller=wrong ;;
  esac
  local served=no
  case "$SERVES" in
    everyone) served=yes ;;
    closed) served=no ;;
    gated) [ "$caller" = "real" ] && served=yes ;;
    *) { [ "$caller" = "real" ] || [ "$caller" = "$SERVES" ]; } && served=yes ;;
  esac
  if [ "$served" = "yes" ]; then printf ''; else printf '16'; fi
}
app_request() { printf 'HTTP/1.1 200 OK\r\ncontent-length: 0\r\n\r\n' > "$1"; }
control_api() { printf '{"nodes":[{"id":"%s","healthy":true}]}' "$NODE_ID"; }
STUB
    definition_of check_the_node_management_plane_needs_the_node_key
    echo 'check_the_node_management_plane_needs_the_node_key'
    echo 'echo "verdicts: ${CHECKS_PASSED} passed ${CHECKS_FAILED} failed"'
  } > "$probe"
  bash "$probe" 2>&1
}

# $1 who the node serves; $2 what the check's report must say about it.
# A served call has no header status, which the check prints as "none".
node_plane_case() {
  local serves=$1 phrase=$2
  local name="the node-management check fails when the node serves the ${serves} caller"
  [ "$serves" = "closed" ] && name="the node-management check fails when the node refuses even the node key"
  local out
  out=$(node_plane_probe "$serves")
  if [ "${out#*verdicts: 0 passed 1 failed}" != "$out" ] && [ "${out#*"$phrase"}" != "$out" ]; then
    pass "$name"
  else
    fail "$name" \
      "wanted a failure saying \"${phrase}\"; it reported: $(printf '%s' "$out" | tr '\n' ' ' | head -c 300)"
  fi
}

node_plane_case everyone 'expected grpc-status 16 with no credential (got "none")'
node_plane_case none 'expected grpc-status 16 with no credential (got "none"), 16 with a wrong one (got "16")'
node_plane_case wrong '16 with a wrong one (got "none") and 16 on OnScheduled (got "16")'
node_plane_case trigger '16 on OnScheduled (got "none"), and the node key to be served (header status was "none"'
node_plane_case closed 'the node key to be served (header status was "16", expected none)'

out=$(node_plane_probe gated)
if [ "${out#*verdicts: 1 passed 0 failed}" != "$out" ]; then
  pass "the node-management check passes when only the node key is served"
else
  fail "the node-management check passes when only the node key is served" \
    "it reported: $(printf '%s' "$out" | tr '\n' ' ' | head -c 300)"
fi

echo
if [ "$failures" -eq 0 ]; then
  echo "all witness tests passed"
else
  echo "${failures} witness test(s) failed"
  exit 1
fi
