# One group of `scripts/demo.sh verify`: the web platform an application runs
# on - what owns its timers, where it may fetch, what `crypto` and the Cache
# API answer, and what one caller is handed that another stored. Sourced by
# `scripts/demo.sh`.

# How many times this node has said, at warn, that it cleared timers an
# invocation left armed, filed under the invocation that armed them. The same
# reason as `stream_budget_log_lines`, and the same `$1`: counting the lines
# that match the wording counts a second `verify` run's line, another check's
# line, and any line a request from outside put in the file, none of which is
# evidence about the request this check made.
timer_cleared_log_lines() {
  local trace=$1
  if [ -z "$trace" ]; then
    echo 0
    return 0
  fi
  node_log | grep " WARN " | grep "were still armed when this invocation ended" \
    | lines_of_invocation "$trace" | grep -c . || true
}

check_no_timer_outlives_the_request_that_armed_it() {
  local title="no timer outlives the request that armed it, however the handler files it"
  local headers="${LOG_DIR}/verify-timers.headers"
  # A tag this run invents, for the reason given at `trace_of_witness`: the
  # node's warning about cleared timers is written for every request that
  # leaves one armed, so one `GET ${TIMER_ROUTE}?mode=arm` from outside used
  # to satisfy the assertion below on its own, and a line forged through an
  # application's own logging used to be quoted here as the node's.
  local tag="timers-$$-${RANDOM}"

  # One request arms everything it can; a second, in the same isolate, reads
  # whether any of it is still ticking. Two requests is the whole point: a timer
  # that fires inside the request that armed it is ordinary, and a timer that
  # fires inside the next one is somebody else's CPU and somebody else's rate
  # limit being spent.
  local armed
  armed=$(app_request "$headers" "${TIMER_ROUTE}?mode=arm&tag=${tag}")
  if [ "$(http_status "$headers")" != "200" ]; then
    fail_check "$title" "GET ${TIMER_ROUTE}?mode=arm answered $(http_status "$headers"): $(printf '%s' "$armed" | head -c 200)"
    return
  fi

  local armed_interval armed_timeout scope claim invented adopted
  armed_interval=$(printf '%s' "$armed" | jq -r '.intervalTicks // 0')
  armed_timeout=$(printf '%s' "$armed" | jq -r '.timeoutTicks // 0')
  scope=$(printf '%s' "$armed" | jq -r '.doors.scope // "?"')
  claim=$(printf '%s' "$armed" | jq -r '.doors.claim // "?"')
  invented=$(printf '%s' "$armed" | jq -r '.doors.invented // "?"')
  adopted=$(printf '%s' "$armed" | jq -r '.doors.adopted // "?"')

  local report before_i after_i before_t after_t before_o after_o
  report=$(app_request "$headers" "${TIMER_ROUTE}?mode=report")
  before_i=$(printf '%s' "$report" | jq -r '.before.intervalTicks // -1')
  after_i=$(printf '%s' "$report" | jq -r '.after.intervalTicks // -2')
  before_t=$(printf '%s' "$report" | jq -r '.before.timeoutTicks // -1')
  after_t=$(printf '%s' "$report" | jq -r '.after.timeoutTicks // -2')
  before_o=$(printf '%s' "$report" | jq -r '.before.ownerTicks // -1')
  after_o=$(printf '%s' "$report" | jq -r '.after.ownerTicks // -2')

  # The node has to say what it cleared, with the numbers: an operator whose
  # application is slower than it should be needs this line to be findable.
  # At warn, which is what the evidence below claims and what nothing used to
  # read - the old grep took the last line matching the words, which for a
  # message this long is tracing's wrapped continuation and carries no level
  # at all.
  #
  # `|| true` for the same reason it is on the quotation in
  # `check_a_stream_past_its_budget_is_closed_by_the_node`: without it a grep
  # that matches nothing ends the whole run under `set -euo pipefail`, and
  # the missing line is exactly what the failure branch below is written to
  # report.
  local trace said_lines said
  trace=$(trace_of_witness "timer witness ${tag}")
  if [ -z "$trace" ]; then
    fail_check "$title" \
      "the node logged no 'timer witness ${tag}' line, so this check cannot tell which invocation armed the timers and will not read another request's evidence as its own; the log is ${LOG_DIR}/deis-runtime.log"
    return
  fi
  said_lines=$(timer_cleared_log_lines "$trace")
  said=$(node_log | grep " WARN " | grep "were still armed when this invocation ended" \
    | lines_of_invocation "$trace" | tail -n 1 || true)

  if [ "$armed_interval" -gt 0 ] && [ "$armed_timeout" -gt 0 ] \
    && [ "$scope" = "undefined" ] && [ "$claim" = "null" ] \
    && [ "${invented#threw:}" != "$invented" ] && [ "${adopted#threw:}" != "$adopted" ] \
    && [ "$after_i" = "$before_i" ] && [ "$after_t" = "$before_t" ] \
    && [ "$after_o" = "$before_o" ] && [ "$said_lines" -ge 1 ]; then
    pass_check "$title" \
      "the arming request watched its own timers fire (${armed_interval} interval tick(s), ${armed_timeout} tick(s) of a setTimeout that rearms itself) and could reach neither door to the runtime's ownership of them: __deis_runtime_stream_scope is ${scope} and the claim answers ${claim}, so enter() ${invented} and adopt() ${adopted}. A later request through the same isolate then sat for 250ms and none of the three counters moved (${before_i}/${before_t}/${before_o}). The node said so at warn, about invocation ${trace} and no other: \"$(printf '%s' "$said" | log_message | head -c 160)\""
  else
    fail_check "$title" \
      "arming request: intervalTicks=${armed_interval} timeoutTicks=${armed_timeout} scope=${scope} claim=${claim} invented=${invented} adopted=${adopted}; later request: intervals ${before_i}->${after_i}, timeouts ${before_t}->${after_t}, redirected ${before_o}->${after_o}; warn lines naming cleared timers filed under this check's own invocation ${trace}=${said_lines}, the last reading \"$(printf '%s' "${said:-<none>}" | log_message | head -c 160)\""
  fi
}

# `fetch` exists at all, and reaches a destination the operator opened up.
#
# Edge Canon lists `fetch` under basic features (SPECIFICATION_BASIC.md
# section 8.1), so a handler calling it must not meet a ReferenceError. The
# handler reports `available` separately from `reached` precisely so this check
# can tell "the platform has no fetch" from "the destination was refused" -
# without that, a platform missing `fetch` entirely would pass the refusal
# check below and fail nothing.
check_app_fetch_allowed() {
  local title="a handler's fetch reaches a destination the node allows"

  # A run started before the origin existed has no port for it in its state,
  # and the node it started has no `[fetch] allow` line either. Say that
  # rather than dying on an unset variable half way through the run.
  if [ -z "${DEIS_ORIGIN_PORT:-}" ]; then
    fail_check "$title" \
      "this run's state names no fetch origin, so it was started before the demo had one. Restart it: ./scripts/demo.sh stop && ./scripts/demo.sh start"
    return
  fi

  local target="http://127.0.0.1:${DEIS_ORIGIN_PORT}/payload.txt"
  local body
  body=$(app_fetch fetch-allowed "$target")
  answered_json "$title" fetch-allowed "$body" || return 0

  local available reached status got
  available=$(echo "$body" | jq -r '.available // false' 2>/dev/null || true)
  reached=$(echo "$body" | jq -r '.reached // false' 2>/dev/null || true)
  status=$(echo "$body" | jq -r '.status // empty' 2>/dev/null || true)
  got=$(echo "$body" | jq -r '.body // empty' 2>/dev/null || true)

  if [ "$available" != "true" ]; then
    fail_check "$title" \
      "the isolate has no fetch: $(echo "$body" | tr '\n' ' ' | head -c 300)"
    return
  fi
  if [ "$reached" != "true" ] || [ "$status" != "200" ]; then
    fail_check "$title" \
      "GET ${target} from inside the handler did not answer 200: $(echo "$body" | tr '\n' ' ' | head -c 300)"
    return
  fi
  # The body, not just the status: something else listening on that port
  # answers 200 too, and this is what makes the check about the demo's own
  # origin rather than about any server at all.
  case "$got" in
    *"$ORIGIN_BODY"*)
      pass_check "$title" \
        "the handler called ${target} and got the origin's own body back (HTTP ${status}, \"${ORIGIN_BODY}\"); that address is the single entry in the node's [fetch] allow"
      ;;
    *)
      fail_check "$title" \
        "the fetch answered ${status} but the body was not the origin's: $(echo "$got" | tr '\n' ' ' | head -c 200)"
      ;;
  esac
}

# The check this whole arrangement exists for.
#
# deis-store trusts the tenant header it is given: it has to, because until an
# application could open an outbound connection, nothing but the node could
# reach it. The moment `fetch` exists, a handler that can reach deis-store can
# read and delete any other application's keys, rows and blobs by asking with
# a different `x-deis-tenant`. So the handler is pointed straight at it here,
# and a call that gets through is a failure - not a curiosity, and not
# something to note and move on from.
#
# Nothing is asserted about what the *handler* was told: no wording, no status
# code, no particular refusal message. A refusal reaches an application as an
# ordinary failed fetch and that is all it has to be.
#
# What is asserted is why the node did not make the call, read from the node's
# own line rather than from the message the handler caught. Without it this
# check passed on a destination that was merely unreachable - a name that does
# not resolve, a port nothing listens on - because from inside the isolate
# those are the same answer, and the sentence it printed about the policy was
# this script's opinion rather than something anybody had observed.
check_app_fetch_refused() {
  local title="a handler's fetch to deis-store is refused"
  # The address carries a value this run invented, which is the whole of what
  # makes the node's line about it evidence rather than wording. The egress
  # policy judges scheme, host and port, so a query string cannot change the
  # verdict; `fetch_policy::loggable` strips credentials and keeps everything
  # else, so the node writes the nonce back out in the line below.
  #
  # Without it this check read `grep "Refused an outbound fetch from demo.production to
  # http://127.0.0.1:8087/:"`, which is a *wording* match on a file anything
  # that can reach this node's unauthenticated routes can append to. One
  # `POST /api/enqueue` whose job key is that sentence put a line in the file
  # that satisfied it, and the check reported an egress policy it had not
  # seen. A nonce cannot be echoed by someone who has not read it.
  local probe="fetch-refused-$$-${RANDOM}"
  local target="http://127.0.0.1:${DEIS_STORE_PORT}/?probe=${probe}"
  local mark
  mark=$(node_log_mark)
  local body
  body=$(app_fetch fetch-refused "$target")
  answered_json "$title" fetch-refused "$body" || return 0

  local available reached status error
  available=$(echo "$body" | jq -r '.available // false' 2>/dev/null || true)
  reached=$(echo "$body" | jq -r '.reached // false' 2>/dev/null || true)
  status=$(echo "$body" | jq -r '.status // empty' 2>/dev/null || true)
  error=$(echo "$body" | jq -r '.error // empty' 2>/dev/null || true)

  # Without this the check would pass on a platform that has no `fetch` at
  # all, and report an egress policy that does not exist.
  if [ "$available" != "true" ]; then
    fail_check "$title" \
      "nothing was refused: the isolate has no fetch to refuse with. $(echo "$body" | tr '\n' ' ' | head -c 300)"
    return
  fi

  if [ "$reached" = "true" ]; then
    fail_check "$title" \
      "the application reached deis-store on its own (HTTP ${status}). Any handler on this node can now address storage directly with a tenant header of its choosing, which is every other application's data"
    return
  fi

  # The node's own account of the decision, naming this destination and the
  # rule that stopped it. A DNS failure or a dead port produces a line here
  # too, and it says something else - which is the whole reason to read it.
  # Two narrowings, not one. `-F` because the address is matched as the text
  # it is rather than as a pattern whose dots match anything, and the window
  # because a line that was already in the file when this check took its mark
  # is not a line about the call this check just made.
  local refused_line
  refused_line=$(node_log_since "$mark" 2>/dev/null \
    | grep -m 1 -F "Refused an outbound fetch from ${APP_ID}.${ENV_NAME} to ${target}:" || true)
  case "$refused_line" in
    *"the platform's own service endpoints"*) ;;
    *)
      fail_check "$title" \
        "the call was not made (${error}), but the node's line for ${target} does not say the egress policy refused it as one of the platform's own service endpoints: \"$(printf '%s' "${refused_line:-never written}" | log_message | head -c 200)\". An address that is merely unreachable answers the handler exactly the same way, so without that line this check would report a policy it never saw"
      return
      ;;
  esac

  pass_check "$title" \
    "the handler asked for ${target} and the platform did not make the call: ${error}. The node says why, at warn, in a line naming the probe=${probe} this run invented for this one call and written after this check began: \"$(printf '%s' "$refused_line" | log_message | head -c 200)\" - so what stopped it is the policy, not the network it sits on with the origin this same handler reaches freely"
}

# The security half of what a redirect chain costs.
#
# check_a_redirect_chain_costs_a_subrequest_per_hop watches the accounting: one
# fetch() of a URL that redirects N times spends N+1 of the invocation's
# outbound calls. Every hop of that chain lands back on the demo's own origin,
# which the node allows - so a node that judged the URL the application named
# and then followed the redirects with no further questions passes it exactly
# as a node that judges every hop does. The accounting was witnessed; the
# policy was not.
#
# This is the case that tells them apart, and it is the one an attacker
# reaches for. The application names one address, and it is an address it is
# allowed to name. The origin answers 302 to the control plane - an address no
# application on this node may call. If a hop is not judged, that call is
# made, and a handler has reached the platform's own API by asking a server it
# is allowed to talk to for directions.
#
# The pair is what makes it evidence. The same handler, the same origin and
# the same redirect route, sent once to a hop the policy refuses and once to a
# hop it allows: one is stopped naming the hop's own address, the other
# arrives with the origin's body. So what stopped the first is where the hop
# pointed, and not the redirect.
check_a_redirect_hop_to_a_refused_host_is_stopped() {
  local title="the outbound policy judges every hop of a redirect, not just the address the handler named"

  if [ -z "${DEIS_ORIGIN_PORT:-}" ]; then
    fail_check "$title" \
      "this run's state names no fetch origin, so there is no allowed address to be redirected away from. Restart it: ./scripts/demo.sh stop && ./scripts/demo.sh start"
    return
  fi

  # Where the hop lands. The control plane rather than deis-store, so that
  # this check and check_app_fetch_refused leave one node log line each and
  # neither can be read as the other's.
  local refused_hop="http://127.0.0.1:${DEIS_CONTROL_PORT}/api/v1/apps"
  local refused_encoded="http%3A%2F%2F127.0.0.1%3A${DEIS_CONTROL_PORT}%2Fapi%2Fv1%2Fapps"
  local allowed_encoded="http%3A%2F%2F127.0.0.1%3A${DEIS_ORIGIN_PORT}%2Fpayload.txt"
  local origin="http://127.0.0.1:${DEIS_ORIGIN_PORT}/redirect-to?url="

  # Read out of the node's configuration rather than asserted. The evidence
  # this check prints used to say the origin was "the single entry in this
  # node's [fetch] allow", which nothing here had looked at: an allow list
  # with the refused address in it would have produced that sentence word for
  # word. Measured - the control plane's own address added to the list, the
  # node restarted - the hop was still refused, because it is refused as a
  # platform service endpoint whatever the allow list says, and the check
  # went on printing a claim about a file it had never opened. It prints what
  # the file says now.
  local allow_list
  allow_list=$(sed -n 's/^allow = //p' "${DEIS_RUNTIME_CONFIG}" | head -n 1)

  # Before the refused hop, so the node's line about it is read out of what
  # this pair of calls added rather than out of the whole file. There is no
  # nonce to use here: the address is the control plane's own, and it is
  # named that way on purpose so that this check and check_app_fetch_refused
  # leave one line each. Putting a query on it would work the same way it does
  # there, but the address travels through the origin's /redirect-to as an
  # encoded parameter and comes back as a Location header, and this check
  # cannot verify what that round trip does to it.
  local mark
  mark=$(node_log_mark)

  local refused_body allowed_body
  refused_body=$(app_fetch redirect-hop-refused "${origin}${refused_encoded}")
  answered_json "$title" redirect-hop-refused "$refused_body" || return 0
  allowed_body=$(app_fetch redirect-hop-allowed "${origin}${allowed_encoded}")
  answered_json "$title" redirect-hop-allowed "$allowed_body" || return 0

  local refused_reached refused_error allowed_reached allowed_status allowed_text
  refused_reached=$(echo "$refused_body" | jq -r '.reached // false' 2>/dev/null || true)
  refused_error=$(echo "$refused_body" | jq -r '.error // empty' 2>/dev/null || true)
  allowed_reached=$(echo "$allowed_body" | jq -r '.reached // false' 2>/dev/null || true)
  allowed_status=$(echo "$allowed_body" | jq -r '.status // empty' 2>/dev/null || true)
  allowed_text=$(echo "$allowed_body" | jq -r '.body // empty' 2>/dev/null || true)

  # The control first. If the redirect route itself does not work, the
  # refusal above is a broken fixture and not a policy decision, and saying so
  # here is what keeps the two apart.
  case "$allowed_text" in
    *"$ORIGIN_BODY"*) ;;
    *)
      fail_check "$title" \
        "the same handler, the same origin and the same /redirect-to route sent to a hop the node allows did not arrive: reached=${allowed_reached}, HTTP ${allowed_status:-none}, body $(printf '%s' "$allowed_text" | tr '\n' ' ' | head -c 160). Until that works, a refusal on the other hop says nothing about the policy"
      return
      ;;
  esac

  if [ "$refused_reached" = "true" ]; then
    fail_check "$title" \
      "the handler fetched an address the node allows, the origin answered 302 to ${refused_hop}, and the node followed it: a handler can reach anything on this machine through an upstream willing to redirect, which is every egress rule on this node undone by one Location header"
    return
  fi

  # The refusal has to name where the *hop* pointed. Naming the URL the
  # handler passed would mean the first call was refused - which would be a
  # different platform and a much smaller claim.
  case "$refused_error" in
    *"${refused_hop}"*) ;;
    *)
      fail_check "$title" \
        "the call did not complete, but the refusal does not name ${refused_hop}, so it is not the hop that was judged: $(printf '%s' "${refused_error:-<none>}" | head -c 240)"
      return
      ;;
  esac
  case "$refused_error" in
    *"the platform's own service endpoints"*) ;;
    *)
      fail_check "$title" \
        "the hop to ${refused_hop} did not complete, but not because the egress policy refused it: $(printf '%s' "$refused_error" | head -c 240). A hop that merely failed to connect would answer the handler the same way"
      return
      ;;
  esac

  # And the node's own line about it, which is where an operator watching for
  # applications probing the platform sees this happen.
  local logged
  logged=$(node_log_since "$mark" 2>/dev/null \
    | grep -m 1 -F "Refused an outbound fetch from ${APP_ID}.${ENV_NAME} to ${refused_hop}:" || true)
  if [ -z "$logged" ]; then
    fail_check "$title" \
      "the hop was refused to the handler, but the node wrote no line naming ${refused_hop} as a destination it refused, so nothing an operator reads records that an application was redirected onto the platform's own API"
    return
  fi

  pass_check "$title" \
    "the handler called one address, ${origin}<...>, and this node's [fetch] allow is ${allow_list:-<unreadable>}. Pointed at ${refused_hop} the origin's 302 was refused where it landed - \"$(printf '%s' "$refused_error" | head -c 200)\" - and the node recorded it at warn: \"$(printf '%s' "$logged" | log_message | head -c 160)\". Pointed at /payload.txt instead, the identical call followed the hop and came back with the origin's own body (HTTP ${allowed_status}, \"${ORIGIN_BODY}\"), so it is where the hop went that decided it"
}

# The Web Crypto globals, judged on their values.
#
# SPECIFICATION_BASIC.md section 8.1 lists `crypto.randomUUID` and
# `crypto.subtle`. The digest is checked against the known SHA-256 of a fixed
# string, so an implementation that returned well-formed bytes of its own
# invention fails here rather than passing a "did it throw?" test.
check_web_crypto() {
  local title="crypto.randomUUID and crypto.subtle.digest produce real values"
  local headers="${LOG_DIR}/verify-crypto.headers"
  local body
  body=$(app_request "$headers" "/api/crypto")
  answered_json "$title" crypto "$body" || return 0

  local has_crypto uuid differ digest
  has_crypto=$(echo "$body" | jq -r '.present.crypto // false' 2>/dev/null || true)
  uuid=$(echo "$body" | jq -r '.uuid // empty' 2>/dev/null || true)
  differ=$(echo "$body" | jq -r '.uuidsDiffer // false' 2>/dev/null || true)
  digest=$(echo "$body" | jq -r '.digestHex // empty' 2>/dev/null || true)

  if [ "$has_crypto" != "true" ]; then
    fail_check "$title" \
      "the isolate has no crypto: $(echo "$body" | tr '\n' ' ' | head -c 300)"
    return
  fi

  # Version 4, variant 1: the shape a UUID has to have. A generator that
  # returned a well-formed string of the wrong version is still wrong, and a
  # loose length check would not notice.
  if ! echo "$uuid" | grep -qiE '^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$'; then
    fail_check "$title" "randomUUID answered '${uuid}', which is not a version 4 UUID"
    return
  fi
  if [ "$differ" != "true" ]; then
    fail_check "$title" "randomUUID returned the same id twice"
    return
  fi
  if [ "$digest" != "$DEISLET_SHA256" ]; then
    fail_check "$title" \
      "subtle.digest('SHA-256', 'deislet') answered ${digest:-nothing}, expected ${DEISLET_SHA256}"
    return
  fi

  pass_check "$title" \
    "randomUUID gave ${uuid} and a different one on the next call; subtle.digest('SHA-256', 'deislet') gave ${digest}, which is the known digest of that string"
}

# The Cache API past `caches.default`, and the honest half of it.
#
# Two things have to be true and they are different in kind. The first is
# ordinary: a cache an application opens by name is its own namespace, so the
# same key in two caches gives two answers and emptying one leaves the other
# alone. A named cache that could answer the default cache's read with the
# wrong page would be a correctness bug nothing would report.
#
# The second is the product decision. This cache lives in the isolate's own
# memory: it is gone when a new version is deployed and it is not shared with
# the other nodes of an installation. `caches.scope` is what makes that
# checkable from inside an application rather than something an author has to
# have read, because the two kinds of miss mean opposite things - an entry
# evicted from the store you wrote to, against a store that is new and lost
# everything at once. Two requests seeing the same `scope.id` is the store
# outliving a request; an id that changed is the second kind of miss.
check_named_caches() {
  local title="an application opens a cache of its own, and can tell which store it is holding"
  local headers="${LOG_DIR}/verify-cache.headers"

  local first second
  first=$(app_request "$headers" "/api/cache")
  if [ "$(http_status "$headers")" != "200" ]; then
    fail_check "$title" \
      "GET /api/cache answered $(http_status "$headers"): $(echo "$first" | head -c 200)"
    return
  fi
  # A second invocation of the same isolate. The scope is the store's
  # identity, so this is what shows the cache outliving one request.
  second=$(app_request "$headers" "/api/cache")

  local from_default from_pages emptied empty_after survived kind first_id second_id
  from_default=$(echo "$first" | jq -r '.fromDefault // empty' 2>/dev/null || true)
  from_pages=$(echo "$first" | jq -r '.fromPages // empty' 2>/dev/null || true)
  emptied=$(echo "$first" | jq -r '.emptied // empty' 2>/dev/null || true)
  empty_after=$(echo "$first" | jq -r '.namedCacheEmptyAfterDelete // empty' 2>/dev/null || true)
  survived=$(echo "$first" | jq -r '.defaultSurvived // empty' 2>/dev/null || true)
  kind=$(echo "$first" | jq -r '.scopeKind // empty' 2>/dev/null || true)
  first_id=$(echo "$first" | jq -r '.scopeId // empty' 2>/dev/null || true)
  second_id=$(echo "$second" | jq -r '.scopeId // empty' 2>/dev/null || true)

  if [ "$from_default" = "default-copy" ] && [ "$from_pages" = "pages-copy" ] \
    && [ "$emptied" = "true" ] && [ "$empty_after" = "true" ] \
    && [ "$survived" = "default-copy" ] && [ "$kind" = "isolate" ] \
    && [ -n "$first_id" ] && [ "$first_id" = "$second_id" ]; then
    pass_check "$title" \
      "caches.default and caches.open(\"pages\") answered the same key with \"${from_default}\" and \"${from_pages}\"; caches.delete(\"pages\") emptied that one and left the default entry; and two requests both reported scope kind \"${kind}\" with the same store id ${first_id}, which is what tells an entry that was evicted from a store that is new"
  else
    fail_check "$title" \
      "expected the default cache to answer 'default-copy' (got '${from_default:-<none>}'), the named cache 'pages-copy' (got '${from_pages:-<none>}'), delete to report true and empty the name (got '${emptied:-<none>}'/'${empty_after:-<none>}') while the default kept its entry (got '${survived:-<none>}'), and both requests to report scope kind 'isolate' (got '${kind:-<none>}') with one stable id (got '${first_id:-<none>}' then '${second_id:-<none>}')"
  fi
}

# ----------------------------------------------------------------------
# Two callers, one address
# ----------------------------------------------------------------------
#
# Everything else in this file drives one deployed application through one
# node as a single caller, and asks what that caller was given. A whole class
# of defect is invisible to that shape of question, because it needs two
# callers to appear at all: what one caller is handed that belonged to
# another. `verify` reported "90 passed, 0 failed" on a build whose cache was
# keyed by the address alone, so the answer to one user's `POST /orders` was
# handed to the next anonymous `GET /orders` - a live cross-caller disclosure,
# every check green, and the person who found it was a person.
#
# So there is a second caller here. The two requests differ only in the
# credential they carry, they ask for the same address, and the check compares
# the two answers. That comparison is the whole mechanism, and it is not
# specific to a cache: it is what any check of a per-caller boundary looks
# like.
#
# The address they ask for is a route in the example app that follows the
# pattern the Cache API is written for. It is deliberately not a route that
# knows it is being tested: a handler with a special case for the second
# caller would be proving something about itself.

# The route both callers ask for. Named once so the handler in the example
# app's sources and the two checks below cannot drift apart.
PRIVATE_PAGE_ROUTE="/api/private-page"

# One caller's request for it. An empty credential means an anonymous caller,
# which is a caller like any other and the one every mistaken variant matches.
private_page_request() {
  local headers=$1
  local mode=$2
  local credential=$3

  if [ -n "$credential" ]; then
    curl -s -D "$headers" -H "x-deis-app-id: ${APP_ID}" \
      -H "authorization: ${credential}" \
      "http://127.0.0.1:${DEIS_RUNTIME_PORT}${PRIVATE_PAGE_ROUTE}?mode=${mode}" || true
  else
    curl -s -D "$headers" -H "x-deis-app-id: ${APP_ID}" \
      "http://127.0.0.1:${DEIS_RUNTIME_PORT}${PRIVATE_PAGE_ROUTE}?mode=${mode}" || true
  fi
}

# One field of one answer, or nothing when the field is absent or the answer
# was not JSON at all.
#
# Not `jq -r '.field // empty'`, which is the idiom used everywhere else in
# this file: `//` in jq treats `false` as absent, so `.stored // empty`
# answers nothing for `"stored": false` - and `false` is the value the second
# check below exists to see. Measured: the check reported `stored=<none>` and
# failed against a platform that had refused the put exactly as it should.
private_page_field() {
  printf '%s' "$1" | jq -r "if $2 == null then empty else $2 end" 2>/dev/null || true
}

check_a_second_caller_is_not_handed_the_first_ones_answer() {
  local title="a second caller asking for the same address is not handed the first caller's answer"
  local headers="${LOG_DIR}/verify-private-page.headers"
  # Credentials this run invents. Nothing an earlier run, or anybody else on
  # this node, can have left in the store reads as either caller's answer.
  local first="Bearer first-$$-${RANDOM}"
  local second="Bearer second-$$-${RANDOM}"

  local one two three four
  one=$(private_page_request "$headers" vary "$first")
  if [ "$(http_status "$headers")" != "200" ]; then
    fail_check "$title" \
      "GET ${PRIVATE_PAGE_ROUTE} answered $(http_status "$headers") for the first caller: $(printf '%s' "$one" | head -c 200)"
    return
  fi
  # The same caller again. This is what makes the rest of the check mean
  # something: a platform with no cache at all would hand nobody anybody
  # else's page, and would pass every "no leak" assertion below while giving
  # the application none of what it asked for.
  two=$(private_page_request "$headers" vary "$first")
  # A caller with no credential, asking for the same address.
  three=$(private_page_request "$headers" vary "")
  # And a third caller, with a credential of its own, after the anonymous one.
  four=$(private_page_request "$headers" vary "$second")

  local stored_one hit_two served_two served_three served_four
  local scope_one scope_two
  stored_one=$(private_page_field "$one" '.stored')
  scope_one=$(private_page_field "$one" '.scopeId')
  hit_two=$(private_page_field "$two" '.hit')
  served_two=$(private_page_field "$two" '.served')
  scope_two=$(private_page_field "$two" '.scopeId')
  served_three=$(private_page_field "$three" '.served')
  served_four=$(private_page_field "$four" '.served')

  # A cache that lost its store between the two requests would report a miss
  # for the second, and a miss proves nothing about what the store would have
  # handed back. Said separately so a failure names that rather than blaming
  # the caching rules.
  if [ -n "$scope_one" ] && [ "$scope_one" != "$scope_two" ]; then
    fail_check "$title" \
      "the isolate's cache store changed between the first caller's two requests (${scope_one} then ${scope_two}), so nothing here was served out of the store the first request wrote to and this check proves nothing either way"
    return
  fi

  if [ "$stored_one" = "true" ] && [ "$hit_two" = "true" ] \
    && [ "$served_two" = "page-for-${first}" ] \
    && [ "$served_three" = "page-for-anonymous" ] \
    && [ "$served_four" = "page-for-${second}" ]; then
    pass_check "$title" \
      "the first caller's page was stored under its credential and served back to it out of the cache (store ${scope_one}); the anonymous caller and a second credentialed caller asked the same address next and were each answered their own page - \"${served_three}\" and \"page-for-${second}\" - rather than \"page-for-${first}\""
    return
  fi

  local leaked=""
  if [ "$served_three" = "page-for-${first}" ]; then
    leaked=" The anonymous caller was handed the first caller's page."
  fi
  if [ "$served_four" = "page-for-${first}" ] || [ "$served_four" = "page-for-anonymous" ]; then
    leaked="${leaked} The second credentialed caller was handed \"${served_four}\", which is not its own."
  fi
  fail_check "$title" \
    "expected the first caller's put to be accepted (got stored=${stored_one:-<none>}), its second request to be a hit for \"page-for-${first}\" (got hit=${hit_two:-<none>} serving \"${served_two:-<none>}\"), the anonymous caller to be answered \"page-for-anonymous\" (got \"${served_three:-<none>}\") and the second credentialed caller \"page-for-${second}\" (got \"${served_four:-<none>}\").${leaked}"
}

check_a_credentialed_answer_that_records_no_owner_is_not_stored() {
  local title="a credentialed answer that records nothing about whose it is is refused by the cache, not stored"
  local headers="${LOG_DIR}/verify-private-page-bare.headers"
  local credential="Bearer bare-$$-${RANDOM}"

  # The same two callers, against a handler that caches a credentialed answer
  # while saying nothing about whose it is. RFC 9111 section 3.5: a shared
  # cache may not reuse such a response. The platform is what has to refuse
  # it, because the handler here does not know it is doing anything wrong -
  # which is the point, since neither does the handler in a real application.
  local one two
  one=$(private_page_request "$headers" bare "$credential")
  if [ "$(http_status "$headers")" != "200" ]; then
    fail_check "$title" \
      "GET ${PRIVATE_PAGE_ROUTE}?mode=bare answered $(http_status "$headers"): $(printf '%s' "$one" | head -c 200)"
    return
  fi
  two=$(private_page_request "$headers" bare "")

  local stored refused served_two
  stored=$(private_page_field "$one" '.stored')
  refused=$(private_page_field "$one" '.refused')
  served_two=$(private_page_field "$two" '.served')

  # Two things, and both are needed. That the anonymous caller was not handed
  # the credentialed page is the property; that the platform said why is what
  # keeps a handler from believing it has a cache when it has none.
  if [ "$stored" = "false" ] \
    && printf '%s' "$refused" | grep -qi "authorization" \
    && [ "$served_two" = "page-for-anonymous" ]; then
    pass_check "$title" \
      "the put was refused and named the reason - \"$(printf '%s' "$refused" | head -c 220)\" - and the anonymous caller that asked next was answered its own page rather than \"page-for-${credential}\""
  else
    fail_check "$title" \
      "expected the cache to refuse the put and name Authorization (got stored=${stored:-<none>}, refused=\"${refused:-<none>}\") and the next anonymous caller to be answered \"page-for-anonymous\" (got \"${served_two:-<none>}\")"
  fi
}

# The checks this group asks, in the order a run asks them, and the
# heading `verify` prints before them.
group_web_platform_label="the web platform"
group_web_platform_checks=(
  # First of these, because it is about the one piece of `globalThis` an
  # application is not meant to be able to use at all: what owns its timers.
  check_no_timer_outlives_the_request_that_armed_it
  # Here rather than at the end: these are about what a handler finds on
  # `globalThis`, which is the same subject as the check above, and they run
  # before the rollback moves production around underneath them.
  check_app_fetch_allowed
  check_app_fetch_refused
  # Straight after the refusal, because it is the same policy asked the same
  # question about an address the application never named.
  check_a_redirect_hop_to_a_refused_host_is_stopped
  check_web_crypto
  check_named_caches
  # Straight after the cache the application holds, because these two are the
  # same store asked the one question a single caller cannot ask it: what a
  # second caller is handed. They run before the rollback, which replaces the
  # isolate and with it the store they read.
  check_a_second_caller_is_not_handed_the_first_ones_answer
  check_a_credentialed_answer_that_records_no_owner_is_not_stored
)
