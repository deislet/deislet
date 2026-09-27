# One group of `scripts/demo.sh verify`: the environment's secret reaching the
# isolate, and that it is written down nowhere. Sourced by `scripts/demo.sh`.

# The delivery half: the environment's secret reaches the isolate.
check_secret_delivery() {
  local headers="${LOG_DIR}/verify-secret.headers"
  local body
  body=$(app_request "$headers" "/hello/world")

  if [ "$(echo "$body" | jq -r '.secretPresent // false' || true)" = "true" ]; then
    pass_check "the environment's secret reached the isolate" \
      "the handler sees DEMO_SECRET; its value is never in the response"
  else
    fail_check "the environment's secret reached the isolate" \
      "the handler reports it missing: ${body}"
  fi
}

# The containment half, kept apart from the check above so that each entry in
# the group array prints exactly one verdict: the roll call compares the number
# of registered checks with the number of verdicts a complete run printed.
check_secret_delivery_is_written_nowhere() {

  # The value is decrypted only in memory, at the moment of delivery. Nothing
  # on either side's disk may contain it - and that includes the CLI's own
  # --json reports, which are written under LOG_DIR precisely so this sweep
  # covers them. `deis secret set` and `deis secret list` both answer with a
  # document about the secret; one of them printing the value back is exactly
  # the leak this check exists to catch, so no path under LOG_DIR is
  # excluded.
  # `|| true` at the end: this whole pipeline finding nothing is the passing
  # case, and under `set -euo pipefail` a grep that matches nothing would
  # end the run instead.
  #
  # Every component's data directory is swept, not just the node's: the
  # control plane stores the secret encrypted, and the storage services must
  # never have seen it at all, so a plaintext hit in any of them is a leak.
  local hits
  hits=$(grep -rl "$DEMO_SECRET_VALUE" "$LOG_DIR" \
    "${DEMO_DIR}/deis-runtime-data" "${DEMO_DIR}/deis-store-data" \
    "${DEMO_DIR}/deis-queue-data" "${DEMO_DIR}/deis-telemetry-data" \
    "${DEMO_DIR}/bundles" "${DEMO_DIR}/deis-control.db" "${DEMO_DIR}/deis-proxy.db" \
    2>/dev/null | head -n 5 || true)
  if [ -z "$hits" ]; then
    pass_check "the decrypted secret is written nowhere" \
      "no hit in any component's data directory, the bundle store, either database or any log"
  else
    fail_check "the decrypted secret is written nowhere" "found in: ${hits}"
  fi
}

check_cli_secret_list() {
  local json="${LOG_DIR}/cli-secret-list.json"
  local text="${LOG_DIR}/cli-secret-list.txt"
  local title="deis secret list shows the name and never the value"

  if ! deis_cli secret list -e "$ENV_NAME" --json > "$json" 2> "${LOG_DIR}/cli-secret-list.log"; then
    fail_check "$title" \
      "the command failed: $(tail -n 3 "${LOG_DIR}/cli-secret-list.log" | tr '\n' ' ')"
    return
  fi
  # The human form is checked too. It is the one somebody will actually read,
  # and it is a different code path from --json, so a value could leak in one
  # while the other stayed clean.
  #
  # Its exit status is judged rather than discarded, and its output has to
  # name the secret before the sweep below means anything. Both were missing:
  # a command that failed wrote an error message into this file, an error
  # message contains no secret, and the check went on to report that "neither
  # the JSON nor the human output contains the value" about an output that
  # was never produced.
  if ! deis_cli secret list -e "$ENV_NAME" > "$text" 2>&1; then
    fail_check "$title" \
      "the --json form worked but the human form failed, so the listing a person actually reads was never produced: $(tail -n 3 "$text" | tr '\n' ' ')"
    return
  fi

  local names
  names=$(jq -r '.secrets[].name' < "$json" 2>/dev/null | tr '\n' ' ' || true)

  if ! echo "$names" | grep -q "DEMO_SECRET"; then
    fail_check "$title" "DEMO_SECRET is not listed; names were: ${names:-none}"
    return
  fi
  if ! grep -q "DEMO_SECRET" "$text"; then
    fail_check "$title" \
      "the human listing does not name DEMO_SECRET at all, so finding no value in it says nothing: $(head -c 200 "$text" | tr '\n' ' ')"
    return
  fi

  # Both outputs are also swept by check_secret_delivery, which greps every
  # file under LOG_DIR. Asserting it here as well keeps the failure readable:
  # this check names the command that leaked, the sweep only names the file.
  if grep -q "$DEMO_SECRET_VALUE" "$json" "$text" 2>/dev/null; then
    fail_check "$title" \
      "the value came back in the output of \`deis secret list\`"
    return
  fi

  pass_check "$title" \
    "names listed: ${names}- both forms of the command ran, both name DEMO_SECRET, and neither contains the value. There is no command that prints one: the control plane has no endpoint that returns it"
}

# The checks this group asks, in the order a run asks them, and the
# heading `verify` prints before them.
group_secrets_label="secrets"
group_secrets_checks=(
  # The list runs before the sweep: it writes its own output under LOG_DIR,
  # and the sweep is what would catch a value in it.
  check_cli_secret_list
  check_secret_delivery
  check_secret_delivery_is_written_nowhere
)
