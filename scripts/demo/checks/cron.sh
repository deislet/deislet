# One group of `scripts/demo.sh verify`: the scheduler firing an application's
# cron triggers once each. Sourced by `scripts/demo.sh`.

# The UTC second of an ISO 8601 instant as digits:
# "2026-09-02T12:01:00.000Z" -> 20260902120100.
#
# Compared as a number rather than as text so that nothing rests on how a
# locale collates '.' against 'Z', and cut at the second because the marker a
# verify run takes for itself carries no fraction.
cron_instant_digits() {
  local at=${1%%.*}
  at=${at//[-T:Z]/}
  printf '%s' "$at"
}

# Does this record describe a tick that fired after the moment this verify run
# chose for itself?
#
# The decision is kept apart from the fetching so it can be held to canned
# values in scripts/test-demo-witnesses.sh rather than only to a live demo.
# All three have to hold:
#
#   * the counter is above the one that was in the store when this run started
#     looking, so the record an earlier verify against the same `start` left
#     behind is not enough on its own;
#   * the timestamp has the shape `toISOString()` produces, so a stored string
#     that is not an instant cannot pass as one;
#   * that instant is later than the marker, which is what a tick scheduled
#     before this run began cannot satisfy - including the one that was due in
#     the same second the marker was taken.
cron_tick_is_fresh() {
  local previous_tick=$1 marker=$2 tick=$3 at=$4

  case "$tick" in
    '' | *[!0-9]*) return 1 ;;
  esac
  [ "$tick" -gt "$previous_tick" ] || return 1

  printf '%s' "$at" \
    | grep -Eq '^[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:[0-9]{2}(\.[0-9]+)?Z$' \
    || return 1

  [ "$(cron_instant_digits "$at")" -gt "$(cron_instant_digits "$marker")" ]
}

check_cron() {
  # The whole cron path in one observation: the expression rode into the
  # control plane inside the bundle manifest, the scheduler rebuilt its task
  # list from the catalog, it picked a node and called Trigger/OnScheduled,
  # and the app's `scheduled` export wrote the record below.
  #
  # What this check used to be, and why it is not that any more. It read
  # `cron:last-run` and passed on any non-empty value. The key is fixed, so a
  # second `verify` against one `start` passed on the value the first one had
  # already seen - cron could have stopped firing in between and nothing here
  # would say so. The value was never parsed, so anything at all in that key
  # satisfied it. And the app's own KV endpoint takes no credential, so
  # anything that could reach the node could write the key by hand. Three ways
  # to pass without a tick having happened.
  #
  # So the evidence now has to be all of: a tick this run had not already seen
  # (a counter that only goes up), scheduled after a moment this run chose (an
  # instant that parses, compared against that marker), recorded by the
  # platform as well as by the application (`deis trace --kind cron`, which
  # comes from the node's own report to deis-telemetry and not through KV),
  # and unwritable from the endpoint this check reads it through (the app
  # refuses a PUT under `cron:`).
  #
  # The expression is "every minute" and the demo sets the scheduler's tick to
  # five seconds, so each round of the wait is a minute plus a little rather
  # than the three minutes the default tick would need. curl paces its own
  # retries a second apart; nothing here sleeps.
  local title="a cron expression fires on one node and runs the app's handler"
  local kv="https://127.0.0.1:${DEIS_RUNTIME_PORT}/api/kv"

  # The moment this run chose. Taken before anything is read, so every
  # judgement below is about what happened after it.
  local marker marker_us
  marker=$(date -u +%Y-%m-%dT%H:%M:%SZ)
  marker_us=$(( $(date -u +%s) * 1000000 ))

  # Half one: the endpoint this check reads the record through cannot also
  # write it. The refusal is by prefix rather than by key, so the probe writes
  # a name of its own instead of touching the record the rest of the check
  # depends on - a probe that could corrupt the evidence it is checking would
  # be its own defect.
  local probe_key="cron:forgery-probe-$$-${RANDOM}"
  local probe_headers="${LOG_DIR}/verify-cron-forgery.headers"
  local refused forged_status probe_found
  refused=$(runtime_curl -s -D "$probe_headers" -X PUT -H "x-deis-app-id: ${APP_ID}" \
    --data-binary '{"at":"2099-01-01T00:00:00.000Z","tick":999999}' \
    "${kv}/${probe_key}" 2>/dev/null || true)
  forged_status=$(http_status "$probe_headers")
  # `tostring` rather than `// empty`: jq's `//` yields its right side for
  # `false` as well as for null, so the one answer this probe wants to see -
  # "found": false - would come back indistinguishable from no answer at all.
  probe_found=$(runtime_curl -s -m 5 -H "x-deis-app-id: ${APP_ID}" "${kv}/${probe_key}" 2>/dev/null \
    | jq -r '.found | tostring' 2>/dev/null || true)

  # What the store already held. Missing - the first minute after `start` -
  # and a value this app did not write both read as tick 0, which any real
  # tick beats.
  local seen previous_record previous_tick previous_at
  seen=$(runtime_curl -s -m 5 -H "x-deis-app-id: ${APP_ID}" "${kv}/cron:last-run" 2>/dev/null || true)
  previous_record=$(echo "$seen" | jq -r '.value // empty' 2>/dev/null || true)
  previous_tick=$(echo "$previous_record" | jq -r '.tick // empty' 2>/dev/null || true)
  previous_at=$(echo "$previous_record" | jq -r '.at // empty' 2>/dev/null || true)
  case "$previous_tick" in
    '' | *[!0-9]*) previous_tick=0 ;;
  esac

  # Half two: wait for one named key rather than re-reading a key that is
  # already there. `cron:tick:<n>` does not exist until tick n fires, so curl
  # retries a 404 until it does and the wait is paced by curl itself. A tick
  # that turns out to have been scheduled before the marker is not evidence
  # for this run, so the wait moves on to the next one rather than settling
  # for it; three rounds is two more than a healthy demo needs and still
  # bounded.
  local want=$((previous_tick + 1))
  local round=0 fresh=0 record="" tick="" at="" waited_for=""
  while [ "$round" -lt 3 ]; do
    round=$((round + 1))
    waited_for="cron:tick:${want}"
    record=$(runtime_curl -sf --retry 100 --retry-delay 1 --retry-max-time 130 --retry-all-errors \
      -H "x-deis-app-id: ${APP_ID}" "${kv}/${waited_for}" 2>/dev/null \
      | jq -r '.value // empty' 2>/dev/null || true)
    if [ -z "$record" ]; then
      break
    fi
    tick=$(echo "$record" | jq -r '.tick // empty' 2>/dev/null || true)
    at=$(echo "$record" | jq -r '.at // empty' 2>/dev/null || true)
    if cron_tick_is_fresh "$previous_tick" "$marker" "$tick" "$at"; then
      fresh=1
      break
    fi
    want=$((want + 1))
  done

  # Half three: the platform's own account of the same tick, which does not
  # come through KV at all. The node reports every invocation to
  # deis-telemetry with the kind it ran under, so `deis trace --kind cron` is
  # the other side of the same event - the way the log-line checks pair what
  # an application said with the trace id the platform gave the invocation
  # that said it. An application that wrote the KV record without a tick, or
  # an outsider that wrote it from outside, produces no row here.
  #
  # Polled rather than asked once: a node ships what it has recorded in
  # batches, so the row for a tick arrives a few seconds after the KV record
  # the handler wrote inside it, and one query at the moment the record
  # appears asks too early. The condition is the row, not a duration - and
  # the poll only runs when there is a record to corroborate, so a demo whose
  # cron never fired fails on the record rather than spending a minute here.
  local traces="${LOG_DIR}/verify-cron-trace.json"
  local platform_ticks=0 platform_newest="" trace_waited=0
  # A node buffers its traces and pushes them on a timer, so the record of
  # the tick the store already shows may not have reached deis-telemetry yet.
  # Ask once; if it is not there, wait on the platform's own event - `deis
  # trace --follow` streams each record as it lands, and the first one to
  # arrive (this tick's, or the next tick's, either of which started after
  # the marker) ends the wait - and ask again. `timeout` bounds the wait; no
  # polling loop and nothing here sleeps.
  read_platform_ticks() {
    deis_cli trace --kind cron --since 15m --limit 200 --json \
      > "$traces" 2> "${LOG_DIR}/verify-cron-trace.log" || return 0
    platform_ticks=$(jq -r --argjson since "$marker_us" \
      '[.invocations[] | select(.started_us >= $since)] | length' \
      < "$traces" 2>/dev/null || echo 0)
    platform_newest=$(jq -r --argjson since "$marker_us" \
      '[.invocations[] | select(.started_us >= $since)] | last | .time // empty' \
      < "$traces" 2>/dev/null || true)
    case "$platform_ticks" in
      '' | *[!0-9]*) platform_ticks=0 ;;
    esac
  }
  if [ "$fresh" = "1" ]; then
    read_platform_ticks
    if [ "$platform_ticks" -lt 1 ]; then
      local followed_from=$SECONDS
      deis_cli_env
      timeout 60 env "${DEIS_CLI_ENV[@]}" "${BIN_DIR}/deis" trace --kind cron --follow --json \
        2>> "${LOG_DIR}/verify-cron-trace.log" | head -n 1 > /dev/null || true
      trace_waited=$((SECONDS - followed_from))
      read_platform_ticks
    fi
  fi

  if [ "$fresh" = "1" ] && [ "$forged_status" = "403" ] && [ "$probe_found" = "false" ] \
    && [ "$platform_ticks" -ge 1 ]; then
    pass_check "$title" \
      "at ${marker} the store held $(if [ -n "$previous_at" ]; then echo "tick ${previous_tick} scheduled ${previous_at}"; else echo "no cron record at all"; fi); this check then waited for ${waited_for}, a key that does not exist until that tick fires, and the scheduled handler wrote it with tick ${tick} scheduled ${at} - later than the marker, so no earlier run and no earlier tick can be what satisfied this. The platform says the same thing by its own route, which does not pass through KV at all: deis trace --kind cron lists ${platform_ticks} cron invocation(s) that the node reported to deis-telemetry as having started after that marker, the newest at ${platform_newest} (node-local time). That is corroboration that cron ran in this window rather than a second reading of this one tick - the trace carries the expression, not the counter - and it is a row an application cannot write and a caller of the KV endpoint cannot produce. And the record is not something a caller can write: PUT ${probe_key} through the app's own KV endpoint - the credential-free endpoint this check reads through - answered ${forged_status} and stored nothing"
  else
    fail_check "$title" \
      "expected a cron tick scheduled after ${marker}: waited for ${waited_for:-<nothing>} through ${round} round(s) from tick ${previous_tick} and got $(if [ -n "$record" ]; then echo "tick '${tick:-<none>}' scheduled '${at:-<none>}'"; else echo "nothing"; fi); deis trace --kind cron reports ${platform_ticks} invocation(s) started after the marker after ${trace_waited}s of asking for one (expected at least 1, see ${LOG_DIR}/verify-cron-trace.log; 0s means the record above never arrived, so the platform's side was not asked); and a PUT under the cron: prefix answered ${forged_status:-<none>} (expected 403) leaving found='${probe_found:-<none>}' (expected false)$(if [ -n "$refused" ]; then echo ", saying \"$(printf '%s' "$refused" | head -c 120)\""; fi)"
  fi
}

# The checks this group asks, in the order a run asks them, and the
# heading `verify` prints before them.
group_cron_label="cron"
group_cron_checks=(
  check_cron
)
