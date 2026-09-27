# One group of `scripts/demo.sh verify`: what the `deis` CLI itself reports
# about the deploy it has just made. Sourced by `scripts/demo.sh`.

check_cli_deploy() {
  local report="${LOG_DIR}/cli-deploy-v1.json"
  local title="deis deploy takes a project from source to serving"

  if [ ! -s "$report" ]; then
    fail_check "$title" \
      "no deploy report at ${report}, so start did not go through the CLI"
    return
  fi

  local app_created env_created version_created number sha delivered nodes
  app_created=$(jq -r '.app_created' < "$report" 2>/dev/null || true)
  env_created=$(jq -r '.environment_created' < "$report" 2>/dev/null || true)
  version_created=$(jq -r '.version_created' < "$report" 2>/dev/null || true)
  number=$(jq -r '.version.number // empty' < "$report" 2>/dev/null || true)
  sha=$(jq -r '.version.sha256 // empty' < "$report" 2>/dev/null | cut -c1-12 || true)
  delivered=$(jq -r '.delivery.delivered | length' < "$report" 2>/dev/null || echo 0)
  nodes=$(jq -r '.delivery.delivered | join(", ")' < "$report" 2>/dev/null || true)

  # The report says what the CLI did; the request says the result is real.
  # Both halves are needed: a deploy that recorded a version nothing serves
  # is the failure a report on its own would hide.
  local headers="${LOG_DIR}/verify-cli-deploy.headers"
  local body status label
  body=$(app_request "$headers" "/hello/world")
  status=$(http_status "$headers")
  label=$(echo "$body" | jq -r '.label // empty' 2>/dev/null || true)

  if [ "$app_created" = "true" ] && [ "$env_created" = "true" ] \
    && [ "$version_created" = "true" ] && [ "${delivered:-0}" -ge 1 ] \
    && [ "$status" = "200" ] && [ "$label" = "v1" ]; then
    pass_check "$title" \
      "one \`deis deploy\` in the project directory built the bundle, created application ${APP_ID}, created environment ${ENV_NAME}, uploaded version ${number} (sha256 ${sha}) and ${delivered} node(s) took it (${nodes}); GET /hello/world now answers ${status} from ${label}. No REST call was made by hand"
  else
    fail_check "$title" \
      "report says app_created=${app_created} environment_created=${env_created} version_created=${version_created} delivered=${delivered}; the app answers ${status} from label '${label}' (expected 200 from v1)"
  fi
}

check_cli_versions() {
  local out="${LOG_DIR}/cli-versions.json"
  local title="deis versions lists what was deployed and marks the live one"

  if ! deis_cli versions --json > "$out" 2> "${LOG_DIR}/cli-versions.log"; then
    fail_check "$title" \
      "the command failed: $(tail -n 3 "${LOG_DIR}/cli-versions.log" | tr '\n' ' ')"
    return
  fi

  local total live_number live_id preview_number listed
  total=$(jq -r '.total // 0' < "$out" 2>/dev/null || echo 0)
  # The live one is not a separate field: a version is marked live by the
  # environments listed against it, which is the honest way round - a version
  # is not "current", an environment points at one.
  live_number=$(jq -r --arg env "$ENV_NAME" \
    '.versions[] | select(.environments | index($env)) | .number' < "$out" 2>/dev/null || true)
  live_id=$(jq -r --arg env "$ENV_NAME" \
    '.versions[] | select(.environments | index($env)) | .id' < "$out" 2>/dev/null || true)
  preview_number=$(jq -r --arg env "$PREVIEW_ENV_NAME" \
    '.versions[] | select(.environments | index($env)) | .number' < "$out" 2>/dev/null || true)
  listed=$(jq -r '[.versions[] | "\(.number)->[\(.environments | join(","))]"] | join(" ")' \
    < "$out" 2>/dev/null || true)

  local expected
  expected=$(deployed_version "$ENV_NAME")

  if [ "${total:-0}" -ge 2 ] && [ -n "$live_number" ] && [ "$live_id" = "$expected" ] \
    && [ -n "$preview_number" ] && [ "$live_number" != "$preview_number" ]; then
    pass_check "$title" \
      "${total} versions listed; ${listed}. Version ${live_number} is marked as the one ${ENV_NAME} runs and its id matches what the control plane reports (${live_id})"
  else
    fail_check "$title" \
      "total=${total}, ${ENV_NAME} marked on version '${live_number}' (id ${live_id}, control plane says ${expected}), ${PREVIEW_ENV_NAME} marked on '${preview_number}'; listing was: ${listed}"
  fi
}

# The checks this group asks, in the order a run asks them, and the
# heading `verify` prints before them.
group_cli_label="the CLI deploys"
group_cli_checks=(
  check_cli_deploy
  check_cli_versions
)
