# One group of `scripts/demo.sh verify`: what the run itself rested on - the
# node's log, read whole, and a node taken away and given back. Sourced by
# `scripts/demo.sh`.

# The other half of the same claim, and the one that decides whether the
# control plane is a single point of failure.
#
# A node asks the control plane what to run. If it also needed it to *serve*,
# then the machine holding the catalog would be in the path of every request
# on the platform, and a reboot of it would be an outage for every tenant.
# The design says otherwise: a node serves what it has loaded, and the control
# plane's absence costs it only the ability to learn about changes.
#
# Said is not shown. This takes the control plane away while traffic is
# flowing, asks whether the application still answers - which is the assertion
# that matters, and the one with the worst consequence if it is ever wrong -
# and then puts it back and asks whether the fleet reconverges without anybody
# doing anything.
#
# Reconvergence is not automatic in the sense of being free: a node's
# registration expires, and a control plane that came back with an empty
# fleet list would not drive anyone until each node called in again. That is
# the node's own re-registration loop, and it is what the second half here
# asks about.
check_the_platform_serves_while_its_control_plane_is_away() {
  local title="applications keep serving while the control plane is away, and the fleet reconverges when it returns"

  local before_headers="${LOG_DIR}/verify-control-away-before.headers"
  app_request "$before_headers" "/" > /dev/null
  local before_status
  before_status=$(http_status "$before_headers")

  # What the edge answers for this route while everything is up. Recorded
  # rather than assumed: the assertion below is "the same answer as before",
  # which is the actual claim, and a hardcoded 200 would fail the moment a
  # route legitimately answered anything else - reporting the edge as having
  # gone dark when it had routed perfectly.
  local edge_before_headers="${LOG_DIR}/verify-control-away-edge-before.headers"
  routed_request "$ROUTED_HOST" "$ROUTED_PREFIX" "$edge_before_headers" > /dev/null
  local edge_before
  edge_before=$(http_status "$edge_before_headers")

  local old_pid="${DEIS_CONTROL_PID:-}"
  if [ -z "$old_pid" ]; then
    fail_check "$title" "this run has no control-plane pid recorded, so there is nothing to stop"
    return
  fi

  kill "$old_pid" 2>/dev/null || true
  local waited=0
  while kill -0 "$old_pid" 2>/dev/null && [ "$waited" -lt 60 ]; do
    sleep 0.5
    waited=$((waited + 1))
  done
  if kill -0 "$old_pid" 2>/dev/null; then
    fail_check "$title" "the control plane did not exit within 30s of SIGTERM"
    return
  fi

  # It really is gone: its own port answers nothing.
  local control_down
  control_down=$(curl -s -o /dev/null -w '%{http_code}' --max-time 5 \
    "http://127.0.0.1:${DEIS_CONTROL_PORT}/health" 2>/dev/null) || control_down="000"

  # The assertion this check exists for, made on both hops a real request
  # takes. Asked several times over a few seconds rather than once: a node
  # that only failed on its next reconciliation, or a proxy that only dropped
  # its table on its next poll, would both pass a single probe taken
  # immediately.
  #
  # The edge is the half that is easy to forget and the worse one to get
  # wrong. deis-proxy polls the routing table every few seconds; if a poll
  # that could not reach the control plane emptied the table, the edge would
  # go dark a few seconds after the control plane did - which would make the
  # control plane a single point of failure after all, one hop further out,
  # and the node answering perfectly well behind it.
  local serving_while_away=1 probe=0 away_status="000" edge_status="000"
  while [ "$probe" -lt 12 ]; do
    local away_headers="${LOG_DIR}/verify-control-away-${probe}.headers"
    app_request "$away_headers" "/" > /dev/null 2>&1 || true
    away_status=$(http_status "$away_headers")

    local edge_headers="${LOG_DIR}/verify-control-away-edge-${probe}.headers"
    routed_request "$ROUTED_HOST" "$ROUTED_PREFIX" "$edge_headers" > /dev/null 2>&1 || true
    edge_status=$(http_status "$edge_headers")

    if [ "$away_status" != "200" ] || [ "$edge_status" != "$edge_before" ]; then
      serving_while_away=0
      break
    fi
    probe=$((probe + 1))
    sleep 0.5
  done

  # Back, on the same database and the same configuration.
  "${BIN_DIR}/deis-control" \
    --config "$DEIS_CONTROL_CONFIG" \
    --port "$DEIS_CONTROL_PORT" \
    --database "${DEMO_DIR}/deis-control.db" \
    >> "${LOG_DIR}/deis-control.log" 2>&1 &
  DEIS_CONTROL_PID=$!
  if [ -n "${STATE_FILE:-}" ] && [ -f "$STATE_FILE" ]; then
    local rewritten="${STATE_FILE}.tmp"
    sed "s/^DEIS_CONTROL_PID=.*/DEIS_CONTROL_PID=${DEIS_CONTROL_PID}/" \
      "$STATE_FILE" > "$rewritten" && mv "$rewritten" "$STATE_FILE"
  fi

  # Waited for by asking. The fleet reconverging is the node calling in and
  # the control plane polling it back to healthy, and neither is instant.
  local healthy="" back=0 attempt=0
  while [ "$attempt" -lt 240 ]; do
    if curl -s -o /dev/null --max-time 3 "http://127.0.0.1:${DEIS_CONTROL_PORT}/health" 2>/dev/null; then
      back=1
      healthy=$(control_api /api/platform/runtime 2>/dev/null \
        | jq -r --arg id "$NODE_ID" '.nodes[] | select(.id == $id) | .healthy' 2>/dev/null) || healthy=""
      if [ "$healthy" = "true" ]; then
        break
      fi
    fi
    attempt=$((attempt + 1))
    sleep 0.5
  done

  local after_headers="${LOG_DIR}/verify-control-away-after.headers"
  app_request "$after_headers" "/" > /dev/null
  local after_status
  after_status=$(http_status "$after_headers")

  if [ "$before_status" = "200" ] && [ -n "$edge_before" ] && [ "$control_down" != "200" ] \
    && [ "$serving_while_away" = "1" ] && [ "$back" = "1" ] \
    && [ "$healthy" = "true" ] && [ "$after_status" = "200" ]; then
    pass_check "$title" \
      "the control plane was stopped - its own /health answered ${control_down} - and both hops a real request takes went on answering as they did before, ${probe} request(s) each, spread over the seconds it was gone: the node directly, and http://${ROUTED_HOST}${ROUTED_PREFIX} through deis-proxy answering ${edge_before} exactly as it did before - so the edge went on routing from the table it already had rather than emptying it when a poll could not be answered. That is what says the machine holding the catalog is not in the path of a request. It was started again on the same database, and ${NODE_ID} is reported healthy again without anybody registering it by hand; ${APP_ID} answers ${after_status}"
    return
  fi

  fail_check "$title" \
    "expected 200 before (got ${before_status}), the control plane's health to stop answering (got ${control_down}), both hops to answer while it was away as they did before (node ${away_status} wanted 200, edge ${edge_status} wanted ${edge_before:-none}, after ${probe} probe(s)), the control plane to come back (back=${back}), ${NODE_ID} to be healthy again (got ${healthy:-unknown}) and ${APP_ID} to answer 200 after (got ${after_status})"
}

# The claim the whole pull-based design exists to make, asked of real
# processes for the first time.
#
# Architecture principle 8: a node is not told what to run and then trusted to
# remember it. On startup it asks the control plane what it should be running,
# downloads each bundle itself, and only reports healthy once they are loaded.
# The control plane still pushes new deployments at it; the pull is what makes
# a *restarted* node come back with what it had.
#
# Every other check in this file exercises that on a node that has been up the
# whole time, which is exactly the case where the claim costs nothing. On a
# local network machines reboot - for a kernel update, for a power cut, for a
# careless `systemctl restart` - and the difference between a platform that
# survives that and one that needs a human to redeploy afterwards is this one
# behaviour. So: take the node away and put it back, touch nothing else, and
# ask whether the application is serving again.
#
# What makes this a real answer rather than a coincidence: the node's data
# directory survives the restart, so a node that merely reloaded its local
# bundle store would look identical from outside. The log line asserted below
# is what separates them - the node says it has the control plane's list, by
# which point it has dialled the machine channel, proved its certificate and
# been answered. Nothing redeploys anything in between, and the check fails if
# the version serving afterwards is not the one that was serving before.
check_a_restarted_node_comes_back_with_what_it_had() {
  local title="a node that is taken away and put back comes back serving what it had"

  # What it serves now, and under which version.
  local before_headers="${LOG_DIR}/verify-restart-before.headers"
  app_request "$before_headers" "/" > /dev/null
  local before_status version_before
  before_status=$(http_status "$before_headers")
  version_before=$(control_api "/api/v1/apps/${APP_ID}/environments/${ENV_NAME}/deployment" \
    2>/dev/null | jq -r '.deployment.version_id // empty')

  # The marker the restarted node has to write after this point, so a line
  # left over from the first start cannot be mistaken for the new one.
  local lines_before
  lines_before=$(grep -c "it has the control plane.s assignment list" \
    "${LOG_DIR}/deis-runtime.log" 2>/dev/null) || lines_before=0

  # Away. SIGTERM and not SIGKILL: this is a reboot, not a crash, and a node
  # given no chance to shut down cleanly would be testing something else.
  local old_pid="${DEIS_RUNTIME_PID:-}"
  if [ -z "$old_pid" ]; then
    fail_check "$title" "this run has no runtime pid recorded, so there is nothing to restart"
    return
  fi
  kill "$old_pid" 2>/dev/null || true
  local waited=0
  while kill -0 "$old_pid" 2>/dev/null && [ "$waited" -lt 60 ]; do
    sleep 0.5
    waited=$((waited + 1))
  done
  if kill -0 "$old_pid" 2>/dev/null; then
    fail_check "$title" "the node did not exit within 30s of SIGTERM, so the restart never happened"
    return
  fi

  # While it is down, the application must not be served by anything else -
  # otherwise "it came back" would be a sentence about the proxy's cache.
  local down_status
  down_status=$(runtime_curl -s -o /dev/null -w '%{http_code}' --max-time 5 \
    -H "x-deis-app-id: ${APP_ID}" "$(runtime_data_url)/" 2>/dev/null) || down_status="000"

  # And back. The same configuration and the same data directory: this is the
  # machine coming up again, not a new one joining.
  DEIS_RUNTIME_CONFIG="$DEIS_RUNTIME_CONFIG" \
    DEIS_RUNTIME_DATA_LISTEN="127.0.0.1:${DEIS_RUNTIME_PORT}" \
    "${BIN_DIR}/deis-runtime" \
    >> "${LOG_DIR}/deis-runtime.log" 2>&1 &
  DEIS_RUNTIME_PID=$!

  # The state file has to learn the new pid. `verify` runs in its own shell,
  # so the variable set here dies with it, and `stop` reads the file - which
  # would still name the process this check just killed. The node started here
  # would then survive `stop`, hold the port, and the next `start` would refuse
  # to run at all, naming a pid nobody can account for. That is a worse failure
  # than the one this check was written to catch, and it is caused by the check
  # itself, so it is this check's to prevent.
  if [ -n "${STATE_FILE:-}" ] && [ -f "$STATE_FILE" ]; then
    local rewritten="${STATE_FILE}.tmp"
    sed "s/^DEIS_RUNTIME_PID=.*/DEIS_RUNTIME_PID=${DEIS_RUNTIME_PID}/" \
      "$STATE_FILE" > "$rewritten" && mv "$rewritten" "$STATE_FILE"
  fi

  # Waited for by asking, not by sleeping: the node loads a V8 snapshot and
  # then its applications, and how long that takes is the machine's business.
  local ready=0 attempt=0 after_status="000"
  while [ "$attempt" -lt 240 ]; do
    after_status=$(runtime_curl -s -o /dev/null -w '%{http_code}' --max-time 5 \
      -H "x-deis-app-id: ${APP_ID}" "$(runtime_data_url)/" 2>/dev/null) || after_status="000"
    if [ "$after_status" = "200" ]; then
      ready=1
      break
    fi
    attempt=$((attempt + 1))
    sleep 0.5
  done

  # Waited for, not read once. The node answers on its data port as soon as
  # the application is loaded, and the line that says it had the control
  # plane's list is written by another thread - so reading the count at the
  # moment the first 200 arrives catches the file before that line reaches it,
  # and reports the node as having reloaded from local disk when it had not.
  local lines_after="$lines_before" line_attempt=0
  while [ "$line_attempt" -lt 120 ]; do
    lines_after=$(grep -c "it has the control plane.s assignment list" \
      "${LOG_DIR}/deis-runtime.log" 2>/dev/null) || lines_after=0
    if [ "$lines_after" -gt "$lines_before" ]; then
      break
    fi
    line_attempt=$((line_attempt + 1))
    sleep 0.5
  done

  local version_after
  version_after=$(control_api "/api/v1/apps/${APP_ID}/environments/${ENV_NAME}/deployment" \
    2>/dev/null | jq -r '.deployment.version_id // empty')

  # The control plane learns a node is back by polling it, and that poll runs
  # on its own interval. Asked once, the answer here would be "not healthy" -
  # true at that instant and about the poller rather than about the node. So
  # this waits for the fleet to agree, which is the thing actually being
  # claimed: the platform, not just the process, came back.
  local healthy="" health_attempt=0
  while [ "$health_attempt" -lt 120 ]; do
    healthy=$(control_api /api/platform/runtime 2>/dev/null \
      | jq -r --arg id "$NODE_ID" '.nodes[] | select(.id == $id) | .healthy' 2>/dev/null) || healthy=""
    if [ "$healthy" = "true" ]; then
      break
    fi
    health_attempt=$((health_attempt + 1))
    sleep 0.5
  done

  if [ "$ready" = "1" ] && [ "$before_status" = "200" ] && [ "$down_status" != "200" ] \
    && [ "$lines_after" -gt "$lines_before" ] \
    && [ -n "$version_before" ] && [ "$version_after" = "$version_before" ] \
    && [ "$healthy" = "true" ]; then
    pass_check "$title" \
      "${APP_ID} answered 200 before the restart and ${down_status} while the node was gone - so nothing else was serving it - and 200 again afterwards, on the same version ${version_before}, with nobody deploying anything in between. The node said it had the control plane's assignment list ${lines_after} time(s) against ${lines_before} before, which means it dialled the machine channel, proved its certificate and was answered rather than quietly reloading its own bundle directory. The control plane reports ${NODE_ID} healthy again"
    return
  fi

  fail_check "$title" \
    "expected 200 before (got ${before_status}), no answer while down (got ${down_status}), 200 after (got ${after_status}, ready=${ready}), a new assignment-list line (${lines_before} -> ${lines_after}), the same version (${version_before:-none} -> ${version_after:-none}) and ${NODE_ID} healthy (got ${healthy:-unknown})"
}

# ----------------------------------------------------------------------
# The evidence this run rests on
# ----------------------------------------------------------------------

check_the_node_log_this_run_read_was_whole() {
  local title="the log fifteen of these checks read was not cut off while they read it"
  local log="${LOG_DIR}/deis-runtime.log"

  if [ ! -f "$log" ]; then
    fail_check "$title" \
      "${log} does not exist, so every check above that quoted the node was quoting an empty file"
    return
  fi

  local size
  size=$(wc -c < "$log" | tr -d ' ')

  # The marker `bounded_log` writes when it stops. Its absence is the pass:
  # the ceiling exists so that an application - or anyone who can reach this
  # node's unauthenticated routes - cannot fill the disk with the operator's
  # log, and reaching it means the checks above read a partial file without
  # any of them being able to tell.
  if grep -a -q -F "$DEMO_LOG_CAPPED_MARKER" "$log"; then
    fail_check "$title" \
      "the node's log hit its ${DEMO_LOG_MAX_BYTES} byte ceiling during this run and was cut off from that point on, so every check above that read it may have been reading a file that stops before the line it wanted. Raise DEMO_LOG_MAX_BYTES and start again, and look at what filled it: ${log}"
    return
  fi

  pass_check "$title" \
    "${log} is ${size} bytes against the ${DEMO_LOG_MAX_BYTES} byte ceiling this demo gives it, and carries no cut-off marker, so the checks that read the node's own sentences read a whole file. The ceiling is there because an application's log line lands in this file and carries whatever a request put in it: without one, an unauthenticated GET is a write to the operator's disk with nothing counting it"
}

# The checks this group asks, in the order a run asks them, and the
# heading `verify` prints before them.
group_evidence_label="the evidence this run rests on"
group_evidence_checks=(
  # Last, and about the run rather than about the platform: whether the file
  # the log-reading checks above quoted was whole while they quoted it.
  # Last, and deliberately: this one takes the node away and puts it back, so
  # anything after it would be asking its questions of a node that is still
  # loading. The log check below reads a file the restart appends to, which is
  # why it stays after this rather than before it.
  check_the_platform_serves_while_its_control_plane_is_away
  check_a_restarted_node_comes_back_with_what_it_had
  check_the_node_log_this_run_read_was_whole
)
