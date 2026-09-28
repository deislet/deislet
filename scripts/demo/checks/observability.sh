# One group of `scripts/demo.sh verify`: what the control plane and the node
# say happened - telemetry logs, `deis trace` and `deis tail`. Sourced by
# `scripts/demo.sh`.

check_telemetry_logs() {
  if ! command -v grpcurl >/dev/null 2>&1; then
    fail_check "the app's logs are queryable from deis-telemetry" \
      "grpcurl is not installed, so this check could not be run"
    return
  fi

  # Ask for the line the handler itself wrote, not just for "anything from
  # this app": component logs would pass that while the application's own
  # console output never left the node.
  #
  # The query names the application, not "demo.production". A node runs
  # several environments of one application and charges their storage to
  # separate tenants, but deis-telemetry keeps one application dimension and
  # carries the environment as a field, so asking for an application answers
  # with all of its environments - which is the question anybody looking at
  # logs is asking.
  # With a read capability for this one environment, minted the way `deis
  # logs` gets it: deis-telemetry refuses a query without one.
  local out token
  token=$(telemetry_token "$ENV_NAME")
  out=$(grpcurl -cacert "${PKI_DIR}/ca.crt" -max-time 10 \
    -H "authorization: Bearer ${token}" \
    -import-path "${ROOT_DIR}/crates/deis-telemetry/proto" \
    -proto telemetry.proto \
    -d "{\"app_id\":\"${APP_ID}\",\"contains\":\"greeted\",\"limit\":20}" \
    "127.0.0.1:${DEIS_TELEMETRY_PORT}" deis.telemetry.LogService/Query 2>&1)

  local count sample environment
  count=$(echo "$out" | jq -r '.entries | length' 2>/dev/null || echo 0)
  sample=$(echo "$out" | jq -r '.entries[0].message // empty' 2>/dev/null || true)
  environment=$(echo "$out" | jq -r '.entries[0].fields.environment // empty' 2>/dev/null || true)

  if [ "${count:-0}" -gt 0 ] 2>/dev/null && [ -n "$environment" ]; then
    pass_check "the app's logs are queryable from deis-telemetry" \
      "Query(app_id=${APP_ID}, contains=greeted) returned ${count} entries; newest: \"${sample}\" from environment ${environment}"
  elif [ "${count:-0}" -gt 0 ] 2>/dev/null; then
    fail_check "the app's logs are queryable from deis-telemetry" \
      "${count} entries came back but none says which environment produced it, so two environments' logs are indistinguishable"
  else
    fail_check "the app's logs are queryable from deis-telemetry" \
      "$(echo "$out" | head -n 3 | tr '\n' ' ')"
  fi
}

check_cli_trace() {
  local title="deis trace reports the invocation a request just made"
  local out="${LOG_DIR}/verify-cli-trace.log"
  : > "$out"

  # A path nothing else asks for, so the row that arrives can only describe
  # this check's own request. Matching on a path the demo uses elsewhere would
  # pass on an invocation from a minute ago.
  local probe="trace-probe-$$-${RANDOM}"

  if ! open_trace_stream "$ENV_NAME" "$out"; then
    close_trace_stream
    fail_check "$title" \
      "the stream never opened; deis trace said: $(tr '\n' ' ' < "$out" | head -c 300)"
    return
  fi

  app_request "${LOG_DIR}/verify-cli-trace.headers" "/hello/${probe}" > /dev/null

  if await_trace_row "$probe" "$out"; then
    close_trace_stream
    pass_check "$title" \
      "GET /hello/${probe} was made after the stream opened, and deis trace printed: $(printf '%s' "$TRACE_ROW" | tr -s '[:space:]' ' ')"
  else
    close_trace_stream
    fail_check "$title" \
      "the stream opened but no invocation naming ${probe} arrived within 60s; see ${out}"
  fi
}

check_cli_tail() {
  local out="${LOG_DIR}/verify-cli-tail.log"
  local title="deis tail streams a line from a request made after it started"
  : > "$out"

  # A name nothing else produces, so the line that arrives can only have come
  # from the request made below - after the stream was already open. Grepping
  # for a word the app logs on every request would pass on a line from an
  # hour ago and prove nothing about following.
  local probe="tail-probe-$$-${RANDOM}"

  # The CLI's output is read through a pipe rather than tailed out of a file,
  # so waiting for a line is a blocking read that returns the moment one
  # arrives. `exec` means the pid below is the CLI itself and not a shell
  # around it, which matters when it is interrupted at the end.
  deis_cli_env
  coproc DEIS_TAIL { exec env "${DEIS_CLI_ENV[@]}" "${BIN_DIR}/deis" tail -e "$ENV_NAME" 2>&1; }

  # A coprocess that has already exited is reaped and its array removed, so
  # both of these are unset when `deis tail` dies on startup. Under `set -u` a
  # bare expansion would end the whole run instead of this one check - see the
  # same guard in open_trace_stream. The descriptor stays readable to EOF once
  # taken, so a CLI that exits mid-stream still leaves a failed check behind.
  local tail_pid=${DEIS_TAIL_PID:-}
  local tail_fd=${DEIS_TAIL[0]:-}
  if [ -z "$tail_fd" ]; then
    fail_check "$title" \
      "deis tail -e ${ENV_NAME} exited before one line could be read from it"
    return
  fi

  # Wait until the stream is really open before producing anything to see.
  #
  # `deis tail` prints this banner only after the Tail call has returned, and
  # deis-telemetry registers the subscriber inside that handler before it
  # answers - so the banner is proof that a line produced from now on reaches
  # this stream. Doing it the other way round is the classic flake: the
  # request is logged, flushed and broadcast while nobody is listening, and a
  # tail that correctly shows only new lines looks broken.
  local line opened=0
  while IFS= read -r -t 30 -u "$tail_fd" line; do
    printf '%s\n' "$line" >> "$out"
    case "$line" in
      *"following new lines only"*)
        opened=1
        break
        ;;
    esac
  done

  if [ "$opened" -ne 1 ]; then
    kill "$tail_pid" 2>/dev/null || true
    wait "$tail_pid" 2>/dev/null || true
    fail_check "$title" \
      "the stream never opened; deis tail said: $(tr '\n' ' ' < "$out" | head -c 300)"
    return
  fi

  app_request "${LOG_DIR}/verify-cli-tail.headers" "/hello/${probe}" > /dev/null

  local seen=0
  while IFS= read -r -t 60 -u "$tail_fd" line; do
    printf '%s\n' "$line" >> "$out"
    case "$line" in
      *"greeted ${probe}"*)
        seen=1
        break
        ;;
    esac
  done

  # SIGINT, not SIGKILL: `deis tail` races Ctrl-C against the stream so that
  # it unwinds normally and closes the connection. That is the path a person
  # takes to stop it, so it is the one worth exercising.
  kill -INT "$tail_pid" 2>/dev/null || true
  wait "$tail_pid" 2>/dev/null || true

  if [ "$seen" -eq 1 ]; then
    pass_check "$title" \
      "GET /hello/${probe} was made after the stream opened, and deis tail printed: $(grep -m 1 "greeted ${probe}" "$out" | sed 's/[[:space:]][[:space:]]*/ /g')"
  else
    fail_check "$title" \
      "the stream opened but nothing mentioning ${probe} arrived within 60s; see ${out}"
  fi
}

# What one environment's read capability reaches. deis-telemetry holds every
# tenant's records in one store, so the capability is the only thing between
# them: a reader minted for ${APP_ID}/${ENV_NAME} asking for another
# application's logs is refused rather than answered with an empty page, and
# so is a query that names no application at all. The same question asked
# with the platform-wide capability an operator holding read:*/* gets is
# answered, so the refusal is about the capability and not the query.
check_a_telemetry_reader_sees_only_its_own_tenant() {
  local title="a telemetry read capability for one environment cannot read another application"

  if ! command -v grpcurl >/dev/null 2>&1; then
    fail_check "$title" "grpcurl is not installed, so this check could not be run"
    return
  fi

  query_logs_as() {
    grpcurl -cacert "${PKI_DIR}/ca.crt" -max-time 10 \
      -H "authorization: Bearer $1" \
      -import-path "${ROOT_DIR}/crates/deis-telemetry/proto" -proto telemetry.proto \
      -d "$2" \
      "127.0.0.1:${DEIS_TELEMETRY_PORT}" deis.telemetry.LogService/Query 2>&1 || true
  }

  local reader platform other unnamed operator
  reader=$(telemetry_token "$ENV_NAME")
  platform=$(telemetry_token --platform)
  other=$(query_logs_as "$reader" "{\"app_id\":\"${CALLEE_APP_ID}\",\"limit\":5}")
  unnamed=$(query_logs_as "$reader" '{"limit":5}')
  operator=$(query_logs_as "$platform" "{\"app_id\":\"${CALLEE_APP_ID}\",\"limit\":5}")

  if echo "$other" | grep -q 'Code: PermissionDenied' \
    && echo "$unnamed" | grep -q 'Code: PermissionDenied' \
    && [ -n "$platform" ] && ! echo "$operator" | grep -q 'Code:'; then
    pass_check "$title" \
      "the reader minted for ${APP_ID}/${ENV_NAME} asked for ${CALLEE_APP_ID}'s logs and was refused ($(echo "$other" | grep -m1 'Message:' | sed 's/^ *Message: *//' | head -c 120)); a query naming no application was refused too. The platform capability asked the same question and was answered"
  else
    fail_check "$title" \
      "other application: $(echo "$other" | tr '\n' ' ' | head -c 160); no application: $(echo "$unnamed" | tr '\n' ' ' | head -c 120); platform: $(echo "$operator" | tr '\n' ' ' | head -c 120)"
  fi
}

# The checks this group asks, in the order a run asks them, and the
# heading `verify` prints before them.
group_observability_label="observability"
group_observability_checks=(
  check_telemetry_logs
  check_a_telemetry_reader_sees_only_its_own_tenant
  check_cli_trace
  check_cli_tail
)
