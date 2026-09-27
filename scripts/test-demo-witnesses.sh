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
# The real definitions, read out of the demo's sources at run time rather
# than copied here: a test that carried its own copy of `node_log` would go on
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

# The demo is one entry point plus everything it sources beside it, under
# `scripts/demo/`. Every test below reads the definitions as they ship, which
# means reading whatever file holds them; a candidate copy passed as $1 is
# searched the same way, so its tree has to sit beside it.
DEMO_FILES=("$DEMO_SH")
if [ -d "$(dirname "$DEMO_SH")/demo" ]; then
  while IFS= read -r demo_file; do
    DEMO_FILES+=("$demo_file")
  done < <(find "$(dirname "$DEMO_SH")/demo" -name '*.sh' | sort)
fi

# A path fit for a test's name: relative to the repository when it is under it.
demo_path() {
  printf '%s' "${1#"$ROOT_DIR"/}"
}

WORK_DIR=$(mktemp -d)
trap 'rm -rf "$WORK_DIR"' EXIT

failures=0

pass() { echo "   ✓ $1"; }
fail() { echo "   ✗ $1"; [ -n "${2:-}" ] && echo "     $2"; failures=$((failures + 1)); return 0; }

# One function's body, lifted out of the demo's sources exactly as it ships.
#
# The demo is an entry point plus one file per library and per group of
# checks, so the body is taken from whichever of them defines the function -
# the first file that has it, with the entry point searched first.
definition_of() {
  local name=$1
  local text="" file
  for file in "${DEMO_FILES[@]}"; do
    text=$(sed -n "/^${name}() {/,/^}/p" "$file")
    [ -n "$text" ] && break
  done
  if [ -z "$text" ]; then
    echo "❌ neither ${DEMO_SH} nor anything under scripts/demo/ defines ${name}(); this test is out of date" >&2
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

quotations=$(grep -nH '^  said=\$(node_log | grep " WARN "' "${DEMO_FILES[@]}" || true)
if [ -z "$quotations" ]; then
  echo "❌ no file under scripts/demo/ has a 'said=\$(node_log | grep \" WARN \" ...' line; this test is out of date" >&2
  exit 1
fi

# Each one runs in a bash of its own rather than in a subshell here. bash
# turns `set -e` off inside anything it is asked to test the outcome of - an
# `if` condition included - so a subshell in the `if` below would have been a
# test that could not fail. A separate process is judged on its exit status
# with errexit really in force, which is the state `verify` runs in.
probe="${WORK_DIR}/probe.sh"
while IFS= read -r entry; do
  source_file=${entry%%:*}
  line_no=${entry#*:}
  line_no=${line_no%%:*}
  # The whole assignment, continuation lines included: these quotations now
  # name the invocation they belong to and are wrapped over two lines, and a
  # probe fed half a statement measures its own truncation.
  statement=$(awk -v start="$line_no" 'NR >= start { print; if ($0 !~ /\\$/) exit }' "$source_file")
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
  where=$(demo_path "$source_file")
  if [ "$status" -eq 0 ]; then
    pass "${where}:${line_no} survives a log that does not carry the line it quotes"
  else
    fail "${where}:${line_no} survives a log that does not carry the line it quotes" \
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

scan=$(sed -n '/^  orphans=\$(pgrep -f/,/|| true)$/p' "${DEMO_FILES[@]}")
if [ -z "$scan" ]; then
  echo "❌ scripts/demo/ has no 'orphans=\$(pgrep -f ...' scan; this test is out of date" >&2
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
    echo 'OPERATOR_TOKEN=irrelevant'
    echo 'DEIS_RUNTIME_PORT=9'
    echo 'DEIS_PROXY_PORT=9'
    echo 'DEIS_CONTROL_PORT=9'
    # The runtime helpers now carry the demo proxy's certificate, so the
    # environment they are lifted into has to name a PKI the way the demo's
    # does. It points at nothing here - which is the point: a curl that
    # cannot even load a key has to die the same guarded death as one that
    # cannot connect.
    printf 'PKI_DIR=%q\n' "${WORK_DIR}/pki"
    printf 'LOG_DIR=%q\n' "$WORK_DIR"
    echo 'control_url() { printf "http://127.0.0.1:%s" "$DEIS_CONTROL_PORT"; }'
    definition_of runtime_curl
    definition_of runtime_data_url
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
  'control_api|control_api /api/platform/runtime' \
  'control_status|control_status GET /api/platform/runtime ""'
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
# that anything did. `runtime_curl` is the demo's TLS-carrying curl, so it is
# counted with the rest: a check that reads its answer has the same duty to
# guard the assignment.
unguarded=$(python3 - "${DEMO_FILES[@]}" <<'SCAN'
import re, sys

bad = []
for path in sys.argv[1:]:
    lines = open(path, encoding="utf-8").read().split("\n")
    i = 0
    while i < len(lines):
        line = lines[i]
        assignment = re.match(r"^\s*([A-Za-z_][A-Za-z_0-9]*)=\$\(\s*((?:runtime_)?curl\b.*)$", line)
        if assignment and not re.match(r"^\s*(local|export|declare|readonly)\b", line):
            j = i
            while lines[j].rstrip().endswith("\\"):
                j += 1
            if "||" not in "\n".join(lines[i:j + 1]):
                bad.append(f"{path}:{i + 1}: {lines[i].strip()}")
            i = j
        i += 1
print("\n".join(bad))
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

givers_up=$(grep -nH 'answered_json .* || return' "${DEMO_FILES[@]}" || true)
if [ -z "$givers_up" ]; then
  echo "❌ no file under scripts/demo/ has an 'answered_json ... || return' line; this test is out of date" >&2
  exit 1
fi

while IFS= read -r entry; do
  source_file=${entry%%:*}
  line_no=${entry#*:}
  line_no=${line_no%%:*}
  statement=${entry#*:}
  statement=${statement#*:}
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
  where=$(demo_path "$source_file")
  if [ "$status" -eq 0 ]; then
    pass "${where}:${line_no} gives up on the check without ending the run"
  else
    fail "${where}:${line_no} gives up on the check without ending the run" \
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
  # The two traps and their flag, lifted out of the demo's sources as they ship.
  sed -n '/^VERIFY_REACHED_ITS_SUMMARY=/,/^}/p' "${DEMO_FILES[@]}"
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
  sed -n '/^VERIFY_REACHED_ITS_SUMMARY=/,/^}/p' "${DEMO_FILES[@]}"
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
  sed -n '/^VERIFY_REACHED_ITS_SUMMARY=/,/^}/p' "${DEMO_FILES[@]}"
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
# file is a complete settings dump - SECRET, API_KEY,
# APP_SRC_DIR, APP_ID, ENV_NAME, NODE_ID, DEMO_SECRET_VALUE and every port.
# So every setting the person typed was replaced by the last run's, silently:
# `--secret <hex>` came up on the previous run's node key and `--app-dir
# <path>` deployed the previous run's sources. Ten ports had been noticed and
# were copied aside and put back; nothing else was.
#
# The whole preamble is taken out of demo.sh and run here, with the things
# that touch the disk or the process table stubbed out. What is asserted is
# that four settings given before it runs are still the same four afterwards.

preamble=$(sed -n '/^start_demo() {/,/^  claim_port DEIS_RUNTIME_PORT/p' "${DEMO_FILES[@]}" | sed '$d')
if [ -z "$preamble" ]; then
  echo "❌ scripts/demo/ has no start_demo() preamble ending at claim_port; this test is out of date" >&2
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

roll_call=$(sed -n '/^  # The roll call\./,/^}/p' "${DEMO_FILES[@]}")
if [ -z "$roll_call" ]; then
  echo "❌ scripts/demo.sh has no roll call in verify_demo; this test is out of date" >&2
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

route=$(grep -h '^PRIVATE_PAGE_ROUTE=' "${DEMO_FILES[@]}" || true)
if [ -z "$route" ]; then
  echo "❌ scripts/demo/ defines no PRIVATE_PAGE_ROUTE; this test is out of date" >&2
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
# 12. The node-channel check says no when a node route is still on the
#     human listener
# ----------------------------------------------------------------------
#
# `check_the_node_channel_is_not_on_the_human_listener` is the canary that
# the node routes really moved. It asks the listener that serves the
# dashboard for the three things a node used to ask it for - registration,
# the assignment list, the routing table - carrying the fleet key that used
# to open all three, and every one of them has to answer 404.
#
# A demo can only ever show it saying yes: the running control plane does not
# mount those routes, and there is no way to arrange the failure it exists to
# catch by starting a second one.
#
# So it is run here against stand-ins, and `curl` and `control_api` are the
# only two doors it uses, so both are replaced; nothing here reaches a
# network.
#
# One stand-in is not enough, and the reason is the same one this file
# already learned once: the check asks three questions, and against a
# stand-in that still serves all three, a check that had quietly stopped
# asking one of them still goes red on the other two. So there is one
# stand-in per route below, each still serving exactly one of them and 404ing
# the other two, and each has to make the check fail *for that route*.

# $1 says which route the stand-in control plane still serves on the human
# listener:
#   all          every node route still mounted there, as before the cutover
#   registration only PUT /api/nodes/{id}/registration
#   assignments  only GET /api/nodes/{id}/assignments
#   routing      only GET /api/routing-table
#   none         what the real control plane does now
node_channel_probe() {
  local serves=$1
  local probe="${WORK_DIR}/node-channel-${serves}.sh"
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
    echo 'SECRET=the-fleet-key'
    printf 'SERVES=%q\n' "$serves"
    echo 'control_url() { echo "http://127.0.0.1:9"; }'
    definition_of pass_check
    definition_of fail_check
    cat <<'STUB'
# The stand-in. It decides which of the three routes it still serves from the
# URL the check asked for, and answers 200 for that one and 404 for the rest -
# which is what a control plane that had moved only some of them would do.
curl() {
  local out="" route="other" i args=("$@")
  for ((i = 0; i < ${#args[@]}; i++)); do
    if [ "${args[i]}" = "-o" ]; then out=${args[i + 1]}; fi
    case "${args[i]}" in
      *"/registration") route="registration" ;;
      *"/assignments") route="assignments" ;;
      *"/api/routing-table") route="routing" ;;
    esac
  done
  local served=no
  if [ "$SERVES" = "all" ] || [ "$SERVES" = "$route" ]; then
    served=yes
  fi
  if [ -n "$out" ] && [ "$out" != "/dev/null" ]; then : > "$out"; fi
  if [ "$served" = "yes" ]; then printf '200'; else printf '404'; fi
}
control_api() {
  # The fleet as it stands with the real node registered by certificate. An
  # invented node appears only where the stand-in still accepts registrations,
  # because that is what getting through would leave behind.
  if [ "$SERVES" = "all" ] || [ "$SERVES" = "registration" ]; then
    printf '{"nodes":[{"id":"%s","healthy":true,"identity":"spiffe://demo/deislet/node/%s"},{"id":"%s-uninvited-%s","healthy":true}]}' \
      "$NODE_ID" "$NODE_ID" "$NODE_ID" "$$"
  else
    printf '{"nodes":[{"id":"%s","healthy":true,"identity":"spiffe://demo/deislet/node/%s"}]}' \
      "$NODE_ID" "$NODE_ID"
  fi
}
STUB
    definition_of check_the_node_channel_is_not_on_the_human_listener
    echo 'check_the_node_channel_is_not_on_the_human_listener'
    echo 'echo "verdicts: ${CHECKS_PASSED} passed ${CHECKS_FAILED} failed"'
  } > "$probe"
  bash "$probe" 2>&1
}

out=$(node_channel_probe all)
if [ "${out#*verdicts: 0 passed 1 failed}" != "$out" ]; then
  pass "the node-channel check fails when every node route is still on the human listener"
else
  fail "the node-channel check fails when every node route is still on the human listener" \
    "it reported: $(printf '%s' "$out" | tr '\n' ' ' | head -c 300)"
fi

# One route at a time. Each case names, in the check's own failure text, the
# route that was still served, so a check that dropped one comparison is seen
# to pass here where it must not.
one_route_case() {
  local serves=$1 phrase=$2
  local name="the node-channel check fails when only ${serves} is still on the human listener"
  local out
  out=$(node_channel_probe "$serves")
  if [ "${out#*verdicts: 0 passed 1 failed}" != "$out" ] \
    && [ "${out#*"$phrase"}" != "$out" ]; then
    pass "$name"
  else
    fail "$name" \
      "a control plane that still serves ${serves} and nothing else must fail the check on that route alone (wanted \"${phrase}\"); it reported: $(printf '%s' "$out" | tr '\n' ' ' | head -c 300)"
  fi
}

one_route_case registration "registration (got 200)"
one_route_case assignments "assignments (got 200)"
one_route_case routing "routing table (got 200)"

out=$(node_channel_probe none)
if [ "${out#*verdicts: 1 passed 0 failed}" != "$out" ]; then
  pass "the node-channel check passes when the human listener serves none of the three"
else
  fail "the node-channel check passes when the human listener serves none of the three" \
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
# so the number of writers found is asserted as well: two today - state.env
# and secret.key.
#
# It was eight. Three configurations - the node's, the proxy's and the restart
# proxy's - stopped carrying a credential value because nothing in this demo
# configures a fleet key any more: each of those machines proves a certificate
# instead. What they carry now is the *path* to a private key, which is a path
# and not a secret, so this check no longer has anything to say about them -
# and the file that does hold the secret, the key itself, is held to 0600 by
# the check below rather than by this one. (deis-telemetry's configuration
# carries no credential and is 0600 anyway.)
#
# It was five. The store, queue and control-plane configurations stopped
# carrying the fleet-wide storage secret when storage calls moved to
# capabilities the control plane signs: its signing keys live in its own
# database, encrypted under secret.key, and deis-store and deis-queue fetch
# only the public halves.

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

# Read file by file: the awk below counts lines from the start of what it is
# given, and a chmod twelve lines away in another file is not twelve lines
# away.
writers=$(for demo_file in "${DEMO_FILES[@]}"; do
  awk -v want='(^|[^_a-z])secret = "|auth_secret = "|node_preshared_keys = |api_keys = |^SECRET=|^API_KEY=|^OPERATOR_TOKEN=' \
    -f "${WORK_DIR}/credential-writers.awk" "$demo_file"
done)
found=$(printf '%s\n' "$writers" | grep -c . || true)
unset_writers=$(printf '%s\n' "$writers" | grep ' UNSET$' || true)
if [ "$found" -ge 2 ] && [ -z "$unset_writers" ]; then
  pass "every one of the ${found} credential-bearing files demo.sh writes is chmod 600 where it is written"
else
  fail "every one of the ${found} credential-bearing files demo.sh writes is chmod 600 where it is written" \
    "docs/trust-boundaries.md promises 0600 for all of them; found ${found} writer(s) (at least 2 expected), without a chmod: $(printf '%s' "$unset_writers" | tr '\n' ';')"
fi

# ----------------------------------------------------------------------
# 15b. Every private key demo.sh mints is written 0600
# ----------------------------------------------------------------------
#
# The check above used to cover every credential this demo writes down,
# because every credential was a string in a configuration file. It is not any
# more: each machine now proves a certificate, and the thing that must not be
# readable is the private key beside it. A key minted 0644 would be a demo
# that hands any process on the box the identity of the control plane - the
# same failure the fleet key was, with a longer name.
#
# Held statically, like the one above, because a permission is only observable
# at the moment the file is made: openssl writes the key with the process
# umask, so whether it is 0600 depends on the environment the demo happened to
# run in unless the script says so itself. Every `-keyout "..."` in demo.sh
# therefore has to have a `chmod 600` on the same path within twelve lines.
key_writers=$(for demo_file in "${DEMO_FILES[@]}"; do
  awk '
  { lines[NR] = $0 }
  END {
    for (i = 1; i <= NR; i++) {
      if (lines[i] !~ /-keyout "/) continue
      v = lines[i]
      sub(/.*-keyout "/, "", v)
      sub(/".*/, "", v)
      ok = 0
      for (k = i; k <= i + 12 && k <= NR; k++) {
        if (index(lines[k], "chmod 600 \"" v "\"")) ok = 1
      }
      printf "%d %s %s\n", i, v, (ok ? "0600" : "UNSET")
    }
  }' "$demo_file"
done)
keys_found=$(printf '%s\n' "$key_writers" | grep -c . || true)
keys_unset=$(printf '%s\n' "$key_writers" | grep ' UNSET$' || true)
if [ "$keys_found" -ge 2 ] && [ -z "$keys_unset" ]; then
  pass "every one of the ${keys_found} private keys demo.sh mints is chmod 600 where it is minted"
else
  fail "every one of the ${keys_found} private keys demo.sh mints is chmod 600 where it is minted" \
    "a key readable by anything on the box is the identity it names, handed over; found ${keys_found} minting site(s) (at least 2 expected - the authority and the leaves), without a chmod: $(printf '%s' "$keys_unset" | tr '\n' ';')"
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
  ⚠ the deno target was not verified: nothing on PATH answered to deno, so nothing parsed and resolved its generated module graph with the provider tool. Install deno to have the build refuse a program Deno Deploy could not load
  ⚡ Bundling for tencent...
  ⚠ the tencent target was not verified against EdgeOne: there is no local dry-run for it, so only this compiler's own checks stand behind the artifact
  ✓ Bundle manifest reads back: app portable, entry worker/index.js, 1 route(s), 0 asset(s)
LOG

# The same two refusals as a Windows host words them. There the message names
# every spelling it tried, because a bare name is resolved by appending `.exe`
# and npm installs a global CLI as `<name>.cmd` - so "not on PATH" alone would
# send a reader to install a tool that is already installed. The summary reads
# the same, which is the point: it keys on what the build decided, not on how
# the sentence ends.
vendor_case "neither tool answered on a Windows host" \
  "cloudflare NOT verified (wrangler is not on PATH); deno NOT verified (deno is not on PATH); tencent NOT verified (EdgeOne has no local dry-run)" <<'LOG'
  ⚠ the cloudflare target was not verified: nothing on PATH answered to wrangler, wrangler.cmd, wrangler.bat, so nothing checked that this Worker deploys. Install wrangler to have the build refuse an artifact Cloudflare would reject
  ⚠ the deno target was not verified: nothing on PATH answered to deno, deno.cmd, deno.bat, so nothing parsed and resolved its generated module graph with the provider tool. Install deno to have the build refuse a program Deno Deploy could not load
  ⚠ the tencent target was not verified against EdgeOne: there is no local dry-run for it, so only this compiler's own checks stand behind the artifact
LOG

vendor_case "neither tool is on PATH (ubuntu-latest)" \
  "cloudflare NOT verified (wrangler is not on PATH); deno NOT verified (deno is not on PATH); tencent NOT verified (EdgeOne has no local dry-run)" <<'LOG'
  ⚠ the cloudflare target was not verified: nothing on PATH answered to wrangler, so nothing checked that this Worker deploys. Install wrangler to have the build refuse an artifact Cloudflare would reject
  ⚠ the deno target was not verified: nothing on PATH answered to deno, so nothing parsed and resolved its generated module graph with the provider tool. Install deno to have the build refuse a program Deno Deploy could not load
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
# 17. A three-probe check says no for each probe on its own
# ----------------------------------------------------------------------
#
# Section 12's lesson generalises: `check_queue_capability_token` asks
# deis-queue three things (no token, a wrong token, the real one). It had no
# witness at all, so a comparison dropped from it would go unnoticed for the
# same reason. It is run here against a stand-in that admits exactly one of
# the callers it must refuse, and once against one that refuses even the real
# credential, which is not a gate but an outage and must not pass either.
#
# The node-management counterpart this section used to witness was deleted
# along with the fleet-shared node key it policed; the machine channel that
# replaced it is driven over mTLS and has no key-string comparisons left to
# stand in for.

# The queue. `grpcurl` and `storage_token` are the two doors; `command -v`
# is answered too, because the machine running this file need not have
# grpcurl installed (CI's witness job does not).
# $1 says who the stand-in queue serves: everyone, none (the caller with no
# token), wrong (a garbled token), other (the control plane's deis-store
# token, a real signature for another audience), gated (the queue token only)
# or closed (nobody, the queue token included).
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
storage_token() { printf 'the-%s-token' "${1:-store}"; }
grpcurl() {
  local caller="none" i args=("$@")
  for ((i = 0; i < ${#args[@]}; i++)); do
    if [ "${args[i]}" = "-H" ]; then
      case "${args[i + 1]}" in
        "authorization: Bearer the-queue-token") caller="real" ;;
        "authorization: Bearer the-store-token") caller="other" ;;
        "authorization: Bearer "*) caller="wrong" ;;
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
  if [ "$caller" = "other" ]; then
    printf 'ERROR:\n  Code: PermissionDenied\n  Message: wrong audience\n'
    return 1
  fi
  printf 'ERROR:\n  Code: Unauthenticated\n  Message: missing or malformed Authorization: Bearer <token> header\n'
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
queue_token_case wrong "the same with a garbled one (exited 0"
queue_token_case other "PermissionDenied with the deis-store token (exited 0"
queue_token_case closed "the queue token to be served (exited 1"

out=$(queue_token_probe gated)
if [ "${out#*verdicts: 1 passed 0 failed}" != "$out" ]; then
  pass "the queue-token check passes when only the tenant's own token is served"
else
  fail "the queue-token check passes when only the tenant's own token is served" \
    "it reported: $(printf '%s' "$out" | tr '\n' ' ' | head -c 300)"
fi



# ----------------------------------------------------------------------
# 18. A streaming CLI that dies on startup costs one check, not the run
# ----------------------------------------------------------------------
#
# `deis trace --follow` and `deis tail` are read through a coprocess, and bash
# removes a coprocess's array and pid variable the moment it reaps a process
# that has already exited. So when either CLI dies at startup - a build that is
# half written, a node that is not up, a flag the CLI no longer takes - the
# descriptor the reader asks for is not a closed one, it has no value at all.
# Under `set -euo pipefail` that expansion is an unbound variable, and an
# unbound variable ends the run where it stands.
#
# It has happened: a run reported 47 checks, printed
# "demo.sh: line 8470: DEIS_TRACE[0]: unbound variable", and never asked the
# other 47. That is worse than a failed check, because the summary of a run
# that stopped early cannot say whether the checks below it would have passed
# - and the same shape had already been found once before, in section 2.
#
# The stand-in CLI here exits immediately, which is what all of those causes
# look like from the caller's side. What is tested is the definitions as they
# ship: each entry point must come back with a verdict, and the script must
# reach the line after it.

trace_stream_probe() {
  local work="${WORK_DIR}/coproc"
  rm -rf "$work"
  mkdir -p "${work}/bin" "${work}/logs"

  # A `deis` that is gone before its first line, the way every one of the
  # causes above presents itself to the reader.
  printf '#!/bin/sh\nexit 3\n' > "${work}/bin/deis"
  chmod +x "${work}/bin/deis"

  # Run in a child shell with the same flags demo.sh sets, so that an unbound
  # variable ends that shell and this one goes on to report it.
  BASH_ENV=/dev/null bash --noprofile --norc -c '
    set -euo pipefail
    work=$1
    shift
    DEMO_FILES=("$@")

    BIN_DIR="${work}/bin"
    LOG_DIR="${work}/logs"
    ENV_NAME=witness-env
    DEIS_CLI_ENV=()
    deis_cli_env() { DEIS_CLI_ENV=(DEIS_WITNESS=1); }
    app_request() { return 0; }
    pass_check() { echo "PASS ${1}"; }
    fail_check() { echo "FAIL ${1}"; }

    for name in open_trace_stream await_trace_row close_trace_stream check_cli_tail; do
      for demo_file in "${DEMO_FILES[@]}"; do
        body=$(sed -n "/^${name}() {/,/^}/p" "$demo_file")
        [ -n "$body" ] && break
      done
      eval "$body"
    done

    transcript="${LOG_DIR}/transcript.log"
    : > "$transcript"

    if open_trace_stream "$ENV_NAME" "$transcript"; then
      echo "OPENED"
    else
      echo "open_trace_stream reported failure"
    fi

    close_trace_stream
    echo "close_trace_stream returned"

    if await_trace_row "a-row-nobody-will-write" "$transcript"; then
      echo "ROW"
    else
      echo "await_trace_row reported failure"
    fi

    check_cli_tail

    echo "reached the end"
  ' witness "$work" "${DEMO_FILES[@]}" 2>&1
}

probe_out=$(trace_stream_probe) || true

# The whole point: the child shell got to its last line. A bare expansion of
# the coprocess variables never does.
if [ "${probe_out#*reached the end}" != "$probe_out" ]; then
  pass "a deis that exits at once leaves the run to carry on"
else
  fail "a deis that exits at once leaves the run to carry on" \
    "the child shell stopped early; it printed: $(printf '%s' "$probe_out" | tr '\n' ' ' | head -c 400)"
fi

# Each entry point separately, because the run carrying on is no use if a
# check ends up passing or saying nothing on the way there.
for wanted in \
  'open_trace_stream reported failure' \
  'close_trace_stream returned' \
  'await_trace_row reported failure'; do
  if [ "${probe_out#*${wanted}}" != "$probe_out" ]; then
    pass "with no stream to read: ${wanted}"
  else
    fail "with no stream to read: ${wanted}" \
      "it printed: $(printf '%s' "$probe_out" | tr '\n' ' ' | head -c 400)"
  fi
done

if [ "${probe_out#*FAIL }" != "$probe_out" ] && [ "${probe_out#*PASS }" = "$probe_out" ]; then
  pass "check_cli_tail fails rather than passing or ending the run"
else
  fail "check_cli_tail fails rather than passing or ending the run" \
    "it printed: $(printf '%s' "$probe_out" | tr '\n' ' ' | head -c 400)"
fi

echo
if [ "$failures" -eq 0 ]; then
  echo "all witness tests passed"
else
  echo "${failures} witness test(s) failed"
  exit 1
fi
