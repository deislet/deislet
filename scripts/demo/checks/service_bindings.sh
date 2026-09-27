# One group of `scripts/demo.sh verify`: one application calling another, what
# the platform stamps on the way, and who is allowed to say a call may happen.
# Sourced by `scripts/demo.sh`.

# ---- service bindings -------------------------------------------------

check_one_application_calls_another() {
  local title="an application calls another application on this platform"
  local headers="${LOG_DIR}/verify-service-call.headers"
  local body
  body=$(app_request "$headers" "/api/call")

  local status called_by note received app environment
  status=$(echo "$body" | jq -r '.status // empty' 2>/dev/null || true)
  app=$(echo "$body" | jq -r '.app // empty' 2>/dev/null || true)
  environment=$(echo "$body" | jq -r '.environment // empty' 2>/dev/null || true)
  called_by=$(echo "$body" | jq -r '.body | fromjson | .calledBy // empty' 2>/dev/null || true)
  note=$(echo "$body" | jq -r '.body | fromjson | .note // empty' 2>/dev/null || true)
  received=$(echo "$body" | jq -r '.body | fromjson | .received // empty' 2>/dev/null || true)

  if [ "$(http_status "$headers")" = "200" ] \
    && [ "$status" = "202" ] \
    && [ "$called_by" = "${APP_ID}.${ENV_NAME}" ] \
    && [ "$note" = "sent by the caller" ] \
    && [ "$received" = "ticket-from-v1" ] \
    && [ "$app" = "$CALLEE_APP_ID" ] \
    && [ "$environment" = "$ENV_NAME" ]; then
    pass_check "$title" \
      "${APP_ID} reached ${CALLEE_APP_ID}.${ENV_NAME} through its AUTH binding with no address anywhere: the callee ran its own handler, answered its own 202, saw the body and the header the caller sent, and read x-deis-caller as ${called_by} -- which the caller had set to \"a-caller-that-is-not-me\" and the platform overwrote"
  else
    fail_check "$title" \
      "expected the callee to answer 202 as ${CALLEE_APP_ID}.${ENV_NAME} and to see caller ${APP_ID}.${ENV_NAME}; the node answered $(http_status "$headers") and the body was $(printf '%s' "$body" | head -c 300)"
  fi
}

# The other half of what makes `x-deis-caller` evidence: that nothing outside
# the platform can write one.
#
# Addressed to the node directly, on purpose. deis-proxy sweeps the same
# namespace at the edge, and a check that only went through the proxy would
# pass on a node that had no rule of its own -- while every other node on the
# network, and this very command, can reach the node without going through the
# proxy at all.
check_a_client_cannot_forge_the_platform_s_headers() {
  local title="a client cannot hand a handler the platform's own headers"
  local headers="${LOG_DIR}/verify-forged-platform-headers.headers"
  local body
  body=$(runtime_curl -s -D "$headers" \
    -H "x-deis-app-id: ${CALLEE_APP_ID}" \
    -H "x-deis-caller: admin-app.production" \
    -H "x-deis-tenant: victim.production" \
    -H "x-deis-storage-token: forged" \
    -H "x-demo-note: sent by a client" \
    "$(runtime_data_url)/verify" || true)

  local status called_by note
  status=$(http_status "$headers")
  called_by=$(echo "$body" | jq -r '.calledBy // "absent"' 2>/dev/null || true)
  note=$(echo "$body" | jq -r '.note // "absent"' 2>/dev/null || true)

  # The callee is the same handler the service-call check drives, so this is
  # the same code reading the same header: once because the platform wrote it,
  # once because a client tried to.
  # 202 is the callee's own answer, the same one the service-call check above
  # reads; what is under test is what the handler was told about its caller.
  if [ "$status" = "202" ] && [ "$called_by" = "absent" ] \
    && [ "$note" = "sent by a client" ]; then
    pass_check "$title" \
      "curl sent x-deis-caller: admin-app.production, x-deis-tenant and x-deis-storage-token straight to the node; the handler saw no caller at all, while its own x-demo-note came through untouched"
  else
    fail_check "$title" \
      "the node answered ${status} and the handler read calledBy=${called_by:-<nothing>}, note=${note:-<nothing>}; a forged caller reaching a handler is the whole of this check"
  fi
}

check_a_service_binding_is_not_a_hole_in_the_outbound_policy() {
  local title="a service call reaches an application no address would have reached"

  # The callee listens nowhere of its own: it is an isolate on this node, and
  # the only HTTP address that leads to it is this node's own, which is a
  # loopback address. This is what the outbound policy says about that -- so
  # the successful call above is not the fetch below with a shorter name, it
  # is a different mechanism with no address in it at all.
  # A value this run invented, carried in the address so that the node writes
  # it back in its refusal line. The egress policy judges scheme, host and
  # port; a query string cannot change what it decides.
  local probe="service-egress-$$-${RANDOM}"
  local target="http://127.0.0.1:${DEIS_RUNTIME_PORT}/api/call?probe=${probe}"
  local mark
  mark=$(node_log_mark)
  local body
  body=$(app_fetch service-egress "$target")
  answered_json "$title" service-egress "$body" || return 0

  local available reached error
  available=$(echo "$body" | jq -r '.available // false' 2>/dev/null || true)
  reached=$(echo "$body" | jq -r '.reached // false' 2>/dev/null || true)
  error=$(echo "$body" | jq -r '.error // empty' 2>/dev/null || true)

  if [ "$available" != "true" ]; then
    fail_check "$title" \
      "nothing was refused: the isolate has no fetch to refuse with. $(echo "$body" | tr '\n' ' ' | head -c 200)"
    return
  fi
  if [ "$reached" = "true" ]; then
    fail_check "$title" \
      "the handler reached this node's own HTTP port on its own, so the outbound policy is not refusing private addresses and the comparison this check makes is meaningless"
    return
  fi

  # The node said so about the address, not about a binding: the two
  # mechanisms do not share a decision point, which is why one cannot be
  # widened into the other.
  # The nonce is what ties this line to this call. Without it the match was
  # "some line, anywhere in the file, saying an outbound fetch to something
  # containing 127.0.0.1 was refused" - which check_app_fetch_refused's own
  # line satisfies, and which one echoed request can write outright.
  local logged
  logged=$(node_log_since "$mark" 2>/dev/null \
    | grep -m 1 -F "Refused an outbound fetch" | grep -F "${probe}" || true)

  if [ -n "$logged" ]; then
    pass_check "$title" \
      "a handler fetching ${target} -- the only HTTP address the callee can be reached at -- is refused by the outbound policy (${error}), while the same handler reaching the same application through its AUTH binding answers 202. The binding carries no address, so nothing about the policy was widened to make it work"
  else
    fail_check "$title" \
      "the handler was refused (${error:-<no message>}) but no node log line names the refused address, so it is not clear the outbound policy is what stopped it"
  fi
}

check_a_preview_calls_the_preview_and_not_production() {
  local title="a service binding follows the caller's own environment"
  local headers="${LOG_DIR}/verify-service-preview.headers"

  # The preview of the caller runs the same bundle, so the same AUTH binding.
  # The callee has no preview deployed, so this must fail -- loudly, naming
  # the environment it looked for -- rather than quietly answering out of the
  # callee's production copy, which is another tenant's data.
  local body environment refused
  body=$(env_request "$PREVIEW_ENV_NAME" "$headers" "/api/call")
  environment=$(echo "$body" | jq -r '.environment // empty' 2>/dev/null || true)
  refused=$(echo "$body" | jq -r '.refused // empty' 2>/dev/null || true)

  if [ "$(http_status "$headers")" = "502" ] \
    && [ "$environment" = "$PREVIEW_ENV_NAME" ] \
    && echo "$refused" | grep -q "${CALLEE_APP_ID}.${PREVIEW_ENV_NAME}" \
    && echo "$refused" | grep -q "not deployed on this node"; then
    pass_check "$title" \
      "the ${PREVIEW_ENV_NAME} of ${APP_ID} runs the same bundle and the same AUTH binding, and it resolved to ${CALLEE_APP_ID}.${PREVIEW_ENV_NAME} -- which is not deployed -- rather than to ${CALLEE_APP_ID}.${ENV_NAME}, which is. It failed saying so: \"$(printf '%s' "$refused" | head -c 140)\""
  else
    fail_check "$title" \
      "expected the preview's binding to resolve to ${CALLEE_APP_ID}.${PREVIEW_ENV_NAME} and be refused; the node answered $(http_status "$headers"), environment=${environment:-<none>}, refusal=$(printf '%s' "${refused:-<none>}" | head -c 200)"
  fi
}

check_a_callee_decides_who_may_call_it() {
  local title="being named by a caller is not consent"

  # The callee lists exactly one application. The check deploys a version of
  # it that lists nobody, tries the same call, and puts the original back --
  # so what is proven is that consent is read from the version serving the
  # callee at the moment of the call, not resolved into the caller's
  # deployment when the caller was deployed.
  local src="${DEMO_DIR}/app-callee"
  local original
  original=$(cat "${src}/.config.json")

  if ! command -v jq >/dev/null 2>&1; then
    fail_check "$title" "jq is missing, so this check could not be run"
    return
  fi

  local withdrawn
  withdrawn=$(jq '.callableBy = []' "${src}/.config.json") \
    && printf '%s\n' "$withdrawn" > "${src}/.config.json"
  if ! cli_deploy "$src" "$ENV_NAME" cli-deploy-callee-withdrawn --app "$CALLEE_APP_ID"; then
    printf '%s\n' "$original" > "${src}/.config.json"
    fail_check "$title" "could not deploy the callee with consent withdrawn"
    return
  fi

  local body refused
  body=$(app_request "${LOG_DIR}/verify-service-consent.headers" "/api/call")
  refused=$(echo "$body" | jq -r '.refused // empty' 2>/dev/null || true)

  # Put the consenting version back, so every later check sees the platform
  # as the rest of the run left it.
  printf '%s\n' "$original" > "${src}/.config.json"
  cli_deploy "$src" "$ENV_NAME" cli-deploy-callee-restored --app "$CALLEE_APP_ID" || true

  local restored
  restored=$(app_request "${LOG_DIR}/verify-service-consent-back.headers" "/api/call" \
    | jq -r '.status // empty' 2>/dev/null || true)

  if echo "$refused" | grep -q "callableBy" \
    && echo "$refused" | grep -q "${APP_ID}" \
    && [ "$restored" = "202" ]; then
    pass_check "$title" \
      "with ${CALLEE_APP_ID} deployed listing nobody, the identical call from ${APP_ID} -- same bundle, same binding, nothing redelivered to it -- was refused: \"$(printf '%s' "$refused" | head -c 160)\". Redeploying the callee with ${APP_ID} on the list made the same call answer 202 again"
  else
    fail_check "$title" \
      "expected a refusal naming callableBy while consent was withdrawn and 202 after it was restored; the refusal was $(printf '%s' "${refused:-<none>}" | head -c 200) and the restored call answered ${restored:-<none>}"
  fi
}

# The checks this group asks, in the order a run asks them, and the
# heading `verify` prints before them.
group_service_bindings_label="calling another application"
group_service_bindings_checks=(
  # The consent check redeploys the callee twice and puts it back the way it
  # was, so it runs after the two that need the callee as the run left it.
  check_one_application_calls_another
  # Straight after it, because it is the same header read by the same handler:
  # one call the platform stamped, one a client tried to stamp itself.
  check_a_client_cannot_forge_the_platform_s_headers
  check_a_service_binding_is_not_a_hole_in_the_outbound_policy
  check_a_preview_calls_the_preview_and_not_production
  check_a_callee_decides_who_may_call_it
)
