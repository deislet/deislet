# One group of `scripts/demo.sh verify`: what an application may ask to be
# held to, what the node clamps, and what bounds its fan-out. Sourced by
# `scripts/demo.sh`.

# ----------------------------------------------------------------------
# What an application may ask to be held to
# ----------------------------------------------------------------------
#
# A bundle manifest can carry a `limits` block. It is read by the control
# plane when the version is uploaded, travels to the node with the deployment,
# and is clamped field by field against the node's own configuration when the
# isolate is built. The direction is the whole point: an application may ask
# to be held to *less* than the node allows and never to more.
#
# The demo runs both halves at once. Production is v1, whose sources ask for a
# one-second wall clock where the node allows thirty; the preview is v2, whose
# staged copy asks for ten minutes. One is applied, the other is refused, and
# the two are the same application on the same node.

# One request to the slow handler, answered with "<status> <seconds>".
#
# `%{time_total}` rather than a clock read either side of it, because what is
# being measured is when the platform stopped the handler, and a shell's own
# timing would add its process startup to the number being judged.
timed_slow_request() {
  local environment=$1
  local ms=$2
  runtime_curl -s -o /dev/null -w '%{http_code} %{time_total}' \
    -H "x-deis-app-id: ${APP_ID}" -H "x-deis-environment: ${environment}" \
    "$(runtime_data_url)/api/slow?ms=${ms}"
}

check_app_limits_narrowed() {
  local title="an application narrowing its own timeout is held to its number"

  # Twice the app's ceiling and a small fraction of the node's, so the two
  # possible outcomes are far apart: cut off at about a second means the app's
  # number is in force, answered at about two means the node's is.
  local over=$((APP_TIMEOUT_MS * 2))
  local under=$((APP_TIMEOUT_MS / 5))

  # Before the two requests, so that the cut-off line read below is one of
  # these two requests' and not a line already sitting in the file.
  local mark
  mark=$(node_log_mark)

  local overran within
  overran=$(timed_slow_request "$ENV_NAME" "$over")
  within=$(timed_slow_request "$ENV_NAME" "$under")

  # The node's own account of the ceiling it installed, so the numbers below
  # are the node's rather than this script's idea of them. Written when this
  # version was deployed, which is before this phase started, so it is read
  # from the prefix that existed then: the wording on its own is text anything
  # reaching this node can have an application echo into the same file.
  local applied_line node_default applied
  applied_line=$(node_log_window "${DEMO_APP_DEPLOY_FROM:-0}" "${DEMO_APP_DEPLOY_TO:-0}" \
    | grep -m 1 -F "App ${APP_ID}.${ENV_NAME} narrows the default execution timeout" || true)
  applied=$(echo "$applied_line" | sed -n 's/.*timeout: \([0-9]*\)ms.*/\1/p')
  node_default=$(echo "$applied_line" | sed -n 's/.*platform default \([0-9]*\)ms.*/\1/p')

  # The other half is about one of the two requests just made, so it is read
  # from what the file gained since the mark above.
  local cut_off_line
  cut_off_line=$(node_log_since "$mark" \
    | grep -m 1 -F "App ${APP_ID}.${ENV_NAME} was cut off on one request" || true)

  local over_status over_seconds within_status
  over_status=${overran%% *}
  over_seconds=${overran##* }
  within_status=${within%% *}

  if [ "$applied" = "$APP_TIMEOUT_MS" ] && [ -n "$node_default" ] \
    && [ "$node_default" != "$APP_TIMEOUT_MS" ] \
    && [ "$over_status" = "500" ] && [ "$within_status" = "200" ] \
    && [ -n "$cut_off_line" ]; then
    pass_check "$title" \
      "the manifest asks for ${APP_TIMEOUT_MS}ms and the node's own ceiling is ${node_default}ms; the node logged \"$(echo "$applied_line" | log_message)\". GET /api/slow?ms=${over} was cut off after ${over_seconds}s with HTTP ${over_status} - under the node's ${node_default}ms it would have answered - while GET /api/slow?ms=${under} answered HTTP ${within_status}"
  else
    fail_check "$title" \
      "expected the node to apply ${APP_TIMEOUT_MS}ms (it reports '${applied:-<none>}ms', platform default '${node_default:-<none>}ms'), /api/slow?ms=${over} to be cut off (got HTTP ${over_status} after ${over_seconds}s) and /api/slow?ms=${under} to answer 200 (got HTTP ${within_status}); the cut-off line was $(if [ -n "$cut_off_line" ]; then echo "present"; else echo "never written"; fi)"
  fi
}

# The half that is a security property rather than a convenience.
#
# An invocation holds a runtime thread for its whole duration, so an
# application that could set its own timeout to ten minutes could park one of
# this node's threads for ten minutes, and every other tenant queued behind
# that thread would wait with it. The ceiling is therefore the node's to set
# and an application may only ask for less of it.
#
# The witness is the node's log, and deliberately so: what has to be shown is
# that a number the application asked for never became the number in force,
# and the only place both numbers appear together is the line the clamp
# writes. Demonstrating it by request would mean a request that runs for the
# node's full thirty seconds.
check_app_limits_clamped() {
  local title="an application asking for more than the node allows is held to the node's value"

  # All three lines are about a deployment, and every deployment this demo
  # makes happened before this phase began. So all three are read from the
  # prefix the log had then, and not from the whole file.
  #
  # This is a security check and it had wording as its only evidence, which
  # made it the cheapest one here to turn green falsely: the sentences below
  # contain no value this script invented, so an application asked to log the
  # right sentence - which one unauthenticated request is enough to arrange,
  # because the demo handler logs a job key verbatim - satisfied both of the
  # two that have to be present. The clamp was then reported from a line
  # nobody had clamped anything to write.
  #
  # A window is not as strong as a nonce. It cannot be one here: there is no
  # request to put a nonce in, because the evidence is about a deploy that
  # already happened. So the window is made as small as the thing it is about
  # - the lines `deis deploy -e preview` itself produced, bracketed by two
  # marks the start phase took either side of that one command. Reading the
  # whole file, or even the whole of it before verification, leaves minutes in
  # which the node was listening on an unauthenticated port; this leaves the
  # duration of one deploy.
  local window
  window=$(node_log_window_named "${DEMO_PREVIEW_DEPLOY_FROM:-0}" \
    "${DEMO_PREVIEW_DEPLOY_TO:-0}" "the deploy of ${APP_ID}.${PREVIEW_ENV_NAME}")

  local asked_line clamp_line applied narrowed
  asked_line=$(node_log_window "${DEMO_PREVIEW_DEPLOY_FROM:-0}" "${DEMO_PREVIEW_DEPLOY_TO:-0}" \
    | grep -m 1 -F "timeout_ms=Some(${GREEDY_TIMEOUT_MS})" || true)
  clamp_line=$(node_log_window "${DEMO_PREVIEW_DEPLOY_FROM:-0}" "${DEMO_PREVIEW_DEPLOY_TO:-0}" \
    | grep -m 1 -F "App ${APP_ID}.${PREVIEW_ENV_NAME} asks for a ${GREEDY_TIMEOUT_MS}ms execution timeout" || true)
  applied=$(echo "$clamp_line" | sed -n "s/.*above this node's \([0-9]*\)ms ceiling.*/\1/p")

  # The line that must *not* exist: "narrows" is what the node writes when it
  # takes an application's number, so the greedy version producing one would
  # mean the clamp had let it through. Read from the same window as the other
  # two, and for a second reason: an absence is only meaningful over the range
  # the presences were read from. Over the whole file, one line from a later
  # deploy - or one an application echoed - would report the clamp broken.
  narrowed=$(node_log_window "${DEMO_PREVIEW_DEPLOY_FROM:-0}" "${DEMO_PREVIEW_DEPLOY_TO:-0}" \
    | grep -m 1 -F "App ${APP_ID}.${PREVIEW_ENV_NAME} narrows the default execution timeout: ${GREEDY_TIMEOUT_MS}ms" || true)

  if [ -n "$asked_line" ] && [ -n "$applied" ] \
    && [ "$applied" != "$GREEDY_TIMEOUT_MS" ] && [ -z "$narrowed" ]; then
    pass_check "$title" \
      "the ${PREVIEW_ENV_NAME} version's manifest reached the node asking for timeout_ms=${GREEDY_TIMEOUT_MS}, and the node answered \"$(echo "$clamp_line" | log_message | sed 's/\. An invocation.*//')\". ${applied}ms is the number in force; no line says this environment narrowed anything, because it did not. All three readings are of ${window}"
    return
  fi

  fail_check "$title" \
    "expected the node to log that ${APP_ID}.${PREVIEW_ENV_NAME} asked for ${GREEDY_TIMEOUT_MS}ms and was held to its own ceiling, in ${window}; the deployment line was $(if [ -n "$asked_line" ]; then echo "present"; else echo "absent"; fi), the clamp reports applying '${applied:-<none>}ms', and a \"narrows\" line for ${GREEDY_TIMEOUT_MS}ms was $(if [ -n "$narrowed" ]; then echo "written, so the request was granted"; else echo "absent"; fi)"
}

# The ceiling on fan-out, which none of the other limits reaches.
#
# An invocation that answers one inbound request with two hundred outbound ones
# is spending this node's sockets and its outbound bandwidth, both of which
# every other tenant on the box is also using - and it does so while looking
# healthy on every other ceiling: the calls run concurrently, so the wall clock
# sees one slow request, the CPU clock sees almost nothing, and the op rate
# limit sees a couple of hundred dispatches spread over a second against a
# ceiling of a thousand.
#
# The example project narrows the node's number in its own manifest, which is
# the same "narrowing only" path the timeout takes, so this also shows a second
# limit travelling from a project file into a node's behaviour.
check_subrequest_ceiling() {
  local title="an invocation is held to a ceiling on outbound calls, and the refusal names it"
  local headers="${LOG_DIR}/verify-subrequests.headers"

  # Read out of the project the demo actually deployed rather than repeated
  # here, so the number this check believes in cannot drift from the number
  # the application asked for.
  local declared
  declared=$(jq -r '.limits.maxSubrequests // empty' "${APP_SRC_DIR}/.config.json" \
    2>/dev/null || true)
  [ -n "$declared" ] || declared=6

  local target="http%3A%2F%2F127.0.0.1%3A${DEIS_ORIGIN_PORT}%2Fpayload.txt"
  local under=$((declared - 2))
  local over=$((declared + 3))

  # Before the fan-out, so the refusal line below is one this check's own
  # second request provoked.
  local mark
  mark=$(node_log_mark)

  local within beyond within_done beyond_done refusal
  within=$(app_request "$headers" "/api/fanout?n=${under}&url=${target}")
  if [ "$(http_status "$headers")" != "200" ]; then
    fail_check "$title" \
      "GET /api/fanout?n=${under} answered $(http_status "$headers"): $(echo "$within" | head -c 200)"
    return
  fi
  beyond=$(app_request "$headers" "/api/fanout?n=${over}&url=${target}")

  within_done=$(echo "$within" | jq -r '.completed // empty' 2>/dev/null || true)
  beyond_done=$(echo "$beyond" | jq -r '.completed // empty' 2>/dev/null || true)
  refusal=$(echo "$beyond" | jq -r '.error // empty' 2>/dev/null || true)

  # The node's own two lines: the one that says which number it applied, and
  # the one that says it refused a call. The numbers quoted below are the
  # node's rather than this script's idea of them - and each is read from the
  # window it can have been written in. The narrowing is a deployment fact and
  # predates this phase; the refusal is about the fan-out two lines up.
  local narrowed_line refused_line
  narrowed_line=$(node_log_window "${DEMO_APP_DEPLOY_FROM:-0}" "${DEMO_APP_DEPLOY_TO:-0}" \
    | grep -m 1 -F "App ${APP_ID}.${ENV_NAME} narrows the default subrequest ceiling" || true)
  refused_line=$(node_log_since "$mark" \
    | grep -m 1 -F "this invocation has already made ${declared} outbound call(s)" || true)

  if [ "$within_done" = "$under" ] && [ "$beyond_done" = "$declared" ] \
    && [ -n "$narrowed_line" ] && [ -n "$refused_line" ] \
    && echo "$refusal" | grep -q "${declared} outbound call"; then
    pass_check "$title" \
      "the manifest asks for ${declared} outbound calls per invocation and the node logged \"$(echo "$narrowed_line" | log_message)\". GET /api/fanout?n=${under} made all ${within_done}; GET /api/fanout?n=${over} made ${beyond_done} and the ${beyond_done}th+1 came back to the handler as: $(echo "$refusal" | head -c 220)"
  else
    fail_check "$title" \
      "expected ${under} of ${under} calls to complete (got '${within_done:-<none>}') and exactly ${declared} of ${over} (got '${beyond_done:-<none>}') with a refusal naming ${declared} (got '${refusal:-<none>}'); the node's narrowing line was $(if [ -n "$narrowed_line" ]; then echo "present"; else echo "never written"; fi) and its refusal line $(if [ -n "$refused_line" ]; then echo "present"; else echo "never written"; fi)"
  fi
}

# What a redirect chain costs, which is the part an application author has to
# be told rather than left to discover.
#
# `fetch` follows redirects, and this platform follows them by hand in
# `js/bootstrap.js` so that every hop is judged by the same egress policy as
# the destination the application named. Each hop is also a separate connection
# leaving this node, so each hop costs one subrequest. The consequence is worth
# demonstrating rather than asserting: a handler containing exactly one
# `fetch` can exhaust the ceiling, and the only way that can happen is if hops
# are counted.
check_a_redirect_chain_costs_a_subrequest_per_hop() {
  local title="each hop of a redirect chain costs one of the invocation's outbound calls"
  local headers="${LOG_DIR}/verify-subrequest-redirects.headers"

  local declared
  declared=$(jq -r '.limits.maxSubrequests // empty' "${APP_SRC_DIR}/.config.json" \
    2>/dev/null || true)
  [ -n "$declared" ] || declared=6

  # One fetch plus this many redirects is exactly the ceiling; one more hop is
  # one connection too many.
  local fits=$((declared - 1))
  local spills=$declared

  local base="http%3A%2F%2F127.0.0.1%3A${DEIS_ORIGIN_PORT}%2Fredirect%3Fhops%3D"
  local within beyond within_done beyond_done refusal
  within=$(app_request "$headers" "/api/fanout?n=1&url=${base}${fits}")
  if [ "$(http_status "$headers")" != "200" ]; then
    fail_check "$title" \
      "GET /api/fanout over a ${fits}-hop chain answered $(http_status "$headers"): $(echo "$within" | head -c 200)"
    return
  fi
  beyond=$(app_request "$headers" "/api/fanout?n=1&url=${base}${spills}")

  within_done=$(echo "$within" | jq -r '.completed // empty' 2>/dev/null || true)
  beyond_done=$(echo "$beyond" | jq -r '.completed // empty' 2>/dev/null || true)
  refusal=$(echo "$beyond" | jq -r '.error // empty' 2>/dev/null || true)

  if [ "$within_done" = "1" ] && [ "$beyond_done" = "0" ] \
    && echo "$refusal" | grep -q "${declared} outbound call"; then
    pass_check "$title" \
      "one fetch() of a URL that redirects ${fits} times finished - ${declared} connections, exactly this application's ceiling - while the same single fetch() of a URL that redirects ${spills} times was refused: $(echo "$refusal" | head -c 220). The handler makes one call either way, so the ceiling can only have been reached by counting hops"
    return
  fi

  fail_check "$title" \
    "expected the ${fits}-hop chain to complete (completed='${within_done:-<none>}') and the ${spills}-hop chain to be refused with a message naming ${declared} (completed='${beyond_done:-<none>}', error='${refusal:-<none>}')"
}

# The one verdict no other observer on this platform can produce.
#
# V8's near-heap-limit callback is the only place on a node that knows an
# isolate died of its heap: from the request path an out-of-memory kill and a
# handler that threw are the same terminated execution, and both answer 500.
# The callback records `memory-limit` against the invocation and increments
# `deis_runtime_memory_cutoffs_total` in the same breath, so a row that says
# memory-limit is that callback having run.
#
# The counter is the second half of that verdict, and this check waits for it
# rather than assuming it. A node pushes its traces every second and its
# counters every ten - metrics.rs takes `flush_interval_secs * 3` with a floor
# of ten, and this demo configures a one-second flush - so the row arrives
# first and the sample follows, which is a reason to wait for the sample and
# not a reason to skip it.
#
# It used to be read once, straight after the row, and reported however it
# stood. An unchanged counter printed a sentence about cadence and the check
# passed; so did a counter that was never readable at all, which is how this
# was found - pointing the read below at a port nothing listens on left the
# check green while it printed "still reads nothing, which is the cadence and
# not a gap". The README sells this counter as the second proof that a heap
# ceiling is enforced, and a proof that cannot fail is not one.

# What deis-telemetry currently reports for this tenant's memory cut-offs, or
# nothing - which covers both "no node has pushed that series yet" and "the
# scrape endpoint did not answer". The waiter below fails on either rather
# than excusing them, because from here they are the same missing evidence.
memory_cutoff_counter() {
  curl -s -m 5 "http://127.0.0.1:${DEIS_TELEMETRY_METRICS_PORT}/metrics" 2>/dev/null \
    | grep "^deis_runtime_memory_cutoffs_total{app_id=\"${APP_ID}\",environment=\"${ENV_NAME}\"" \
    | awk '{print $NF}' || true
}

# Read that counter until it stands above $1, and echo what it last read.
#
# Every round is a real read of the series rather than a wait long enough to
# be safe, and the budget is a count of rounds rather than a duration this
# script measures: sixty of them at about a second each is six times the
# ten-second push interval, so exhausting it means the sample is not coming
# rather than that it is late. `awk` does the comparison because the
# exposition format renders this as a gauge, and a gauge may carry a decimal
# point that `-gt` cannot read.
await_memory_cutoff_counter() {
  local floor=${1:-0}
  local rounds=${2:-60}
  local now=""
  while [ "$rounds" -gt 0 ]; do
    now=$(memory_cutoff_counter)
    if [ -n "$now" ] && awk -v now="$now" -v floor="$floor" 'BEGIN { exit !(now > floor) }'; then
      echo "$now"
      return 0
    fi
    rounds=$((rounds - 1))
    poll_pause
  done
  echo "$now"
  return 1
}

check_memory_cutoff() {
  local title="an isolate that exhausts its heap is cut off, and the platform says so"
  local out="${LOG_DIR}/verify-memory-cutoff.log"
  : > "$out"

  # Read before anything is driven, so the number afterwards is this
  # invocation's and not one left behind by an earlier `verify` against the
  # same running demo.
  local before
  before=$(memory_cutoff_counter)

  if ! open_trace_stream "$ENV_NAME" "$out"; then
    close_trace_stream
    fail_check "$title" \
      "the stream never opened; deis trace said: $(tr '\n' ' ' < "$out" | head -c 300)"
    return
  fi

  # A tag this run invents, so the trace row judged below is the one written
  # for this request. Without it the check waits for *an* invocation of this
  # environment to end out of heap, and one `GET /api/heap` from outside -
  # the route is unauthenticated, like every other - supplies that ending
  # while this request fails for some other reason entirely.
  local tag="heap-$$-${RANDOM}"
  local answered status seconds
  answered=$(runtime_curl -s -o /dev/null -w '%{http_code} %{time_total}' \
    -H "x-deis-app-id: ${APP_ID}" \
    "$(runtime_data_url)/api/heap?tag=${tag}" || true)
  status=${answered%% *}
  seconds=${answered##* }

  # Which invocation that was, read off the marker the handler logged before
  # its first allocation. `deis trace` prints the same id as `trace=<id>`.
  local trace
  trace=$(trace_of_witness "heap witness ${tag}")
  if [ -z "$trace" ]; then
    close_trace_stream
    fail_check "$title" \
      "GET /api/heap answered HTTP ${status} after ${seconds}s, but the node logged no 'heap witness ${tag}' line, so this check cannot tell which invocation was its own and will not accept another caller's out-of-heap ending as evidence for it; the log is ${LOG_DIR}/deis-runtime.log"
    return
  fi

  if ! await_trace_row "memory-limit" "$out" "trace=${trace}"; then
    close_trace_stream
    fail_check "$title" \
      "GET /api/heap answered HTTP ${status} after ${seconds}s, but invocation ${trace} - this request's own - was not reported as memory-limit within 60s, so either the ceiling was never reached or the near-heap-limit callback did not classify it; see ${out}"
    return
  fi
  close_trace_stream

  if [ "$status" != "500" ]; then
    fail_check "$title" \
      "an invocation was recorded as memory-limit, but GET /api/heap answered HTTP ${status} rather than failing; a handler killed for its heap cannot also have answered"
    return
  fi

  # A deployment fact, read from the prefix that existed before this phase
  # began. Unlike every other log read in these checks this one decides
  # nothing - the number lands in the pass sentence and in no condition - so
  # a forged line here misreports a ceiling rather than turning a check green.
  # It is narrowed anyway: a number an operator reads as the node's should be
  # the node's.
  local ceiling after
  ceiling=$(node_log_window "${DEMO_APP_DEPLOY_FROM:-0}" "${DEMO_APP_DEPLOY_TO:-0}" \
    | sed -n "s/.*App ${APP_ID}.${ENV_NAME} narrows the default heap ceiling: \([0-9]*\)MB.*/\1/p" \
    | head -n 1)

  # The counter has to arrive, and not arriving is the failure being waited
  # for: a scrape endpoint that is down, a node whose metrics shipper has
  # stopped, a callback that records the verdict without charging the counter.
  # Every one of those leaves the README's second proof missing while the
  # first still reads perfectly, which is exactly the state nobody notices.
  if ! after=$(await_memory_cutoff_counter "${before:-0}"); then
    fail_check "$title" \
      "deis trace calls the invocation memory-limit, but deis_runtime_memory_cutoffs_total{app_id=\"${APP_ID}\",environment=\"${ENV_NAME}\"} never rose above ${before:-nothing}: after sixty reads of http://127.0.0.1:${DEIS_TELEMETRY_METRICS_PORT}/metrics it reads ${after:-nothing}. A node pushes that series every ten seconds, so a minute of reads is not a cadence problem - the near-heap-limit callback increments the counter in the same breath as it records the verdict, and one of the two never left the node"
    return
  fi

  pass_check "$title" \
    "GET /api/heap allocated past the ${ceiling:-?}MB heap this version's manifest asks for and answered HTTP ${status} after ${seconds}s. deis trace calls invocation ${trace} - this request's own, and no other - memory-limit, which nothing but the near-heap-limit callback can produce - from the request path an out-of-heap kill and a handler that threw are the same terminated execution: $(printf '%s' "$TRACE_ROW" | tr -s '[:space:]' ' '). The same callback increments deis_runtime_memory_cutoffs_total, and deis-telemetry's scrape for this tenant went from ${before:-nothing} to ${after} while this check waited for it - that series carries no invocation dimension, so its rise says the counter is written at all, and the row above is what says this request is the one that was cut off"
}

# The checks this group asks, in the order a run asks them, and the
# heading `verify` prints before them.
group_limits_label="resource limits"
group_limits_checks=(
  # Both halves need production on v1 and the preview on v2: v1's manifest
  # narrows its timeout and v2's asks to widen it, so these run before the
  # rollback puts v2 on production.
  check_app_limits_narrowed
  check_app_limits_clamped
  # The subrequest ceiling, which is the one limit that bounds fan-out. Both
  # halves talk to the demo's own origin, which is the single address this
  # node's egress policy allows.
  check_subrequest_ceiling
  check_a_redirect_chain_costs_a_subrequest_per_hop
  check_memory_cutoff
)
