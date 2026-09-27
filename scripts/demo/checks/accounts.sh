# One group of `scripts/demo.sh verify`: accounts, scoped tokens and the audit
# log. Sourced by `scripts/demo.sh`.

# The node told the control plane it exists, on the path the control plane
# actually serves.
#
# This is a seam no compiler watches: deis-runtime builds the URL and
# deis-control serves it, in two crates that do not depend on each other. They
# were once out of step - the node called `POST .../register` while the control
# plane served `PUT .../registration` - and nothing failed visibly, because a
# node that cannot register still fetches its own assignments and still serves
# every request. What was lost was the control plane knowing about nodes
# nobody had written into its configuration file.
# ----------------------------------------------------------------------
# Accounts, scoped tokens and the audit log
# ----------------------------------------------------------------------
#
# `start` wrote no human credential into any configuration file. The first
# owner logged in by trading the one-time bootstrap token the control plane
# wrote into its data directory, and the operator credential the rest of
# this file presents was minted from that account, scoped to the verbs the
# demo uses. These checks watch the control plane tell credentials apart:
# nothing at all gets in, the bootstrap credential is spent after its one
# use, a narrow token reaches one environment and nothing else, and every
# one of them leaves a line behind that names what it did.
#
# They run late on purpose. One of them re-points the preview environment at
# the version it is already running, which is a real deploy made with a real
# scoped credential; every check that expects a particular version on preview
# has finished by then.

# Run the CLI as somebody other than the demo's operator.
#
# The assignment after the array is what does it: `env` applies assignments
# left to right, so this second DEIS_CONTROL_TOKEN replaces the one
# `deis_cli_env` put in the array. Everything else - the endpoints, the
# demo's own config home - stays exactly as `deis_cli` sets it, which is the
# point: only the credential differs.
deis_cli_as() {
  local credential=$1
  shift
  deis_cli_env
  env "${DEIS_CLI_ENV[@]}" "DEIS_CONTROL_TOKEN=${credential}" "${BIN_DIR}/deis" "$@"
}

# One request to the control plane with a chosen credential, answering with
# the HTTP status alone. Used where the claim is about who is refused, which
# `deis` deliberately reports as a message rather than as a number.
control_status() {
  local method=$1 path=$2 credential=${3:-} body=${4:-}
  local args=(-s -o /dev/null -w '%{http_code}' -X "$method" "$(control_url)${path}")
  if [ -n "$credential" ]; then
    args+=(-H "Authorization: Bearer ${credential}")
  fi
  if [ -n "$body" ]; then
    args+=(-H "Content-Type: application/json" --data "$body")
  fi
  # `%{http_code}` already prints 000 for a request that never got an answer,
  # so the caller's message reads "answered HTTP 000" instead of the run
  # ending on curl's exit 7. See `app_request`.
  curl "${args[@]}" || true
}

# The same, answering with the body, so a check can assert on what the caller
# was told rather than only on the number.
control_answer() {
  local method=$1 path=$2 credential=$3 body=${4:-}
  local args=(-s -X "$method" "$(control_url)${path}")
  if [ -n "$credential" ]; then
    args+=(-H "Authorization: Bearer ${credential}")
  fi
  # A body, on the same terms as `control_status`. A check that wants to read
  # the sentence a refusal carries has to send the request the caller would
  # really have sent: a write route validates its body, so a request that
  # omits it is refused for the wrong reason and the sentence under test is
  # never reached.
  if [ -n "$body" ]; then
    args+=(-H "Content-Type: application/json" --data "$body")
  fi
  curl "${args[@]}"
}

# A well-formed credential that was never minted, used to prove that a
# revoked token and one that never existed look identical to the caller.
NEVER_MINTED_TOKEN="deis_pat_00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff0011"

# Create an account if it is not already there. `verify` can be run twice
# against one demo, and the second run must not fail on a name that exists.
ensure_account() {
  deis_cli account create "$1" --role "$2" --json >/dev/null 2>&1 || true
}

# No credential at all, on any host. This is the property DNS rebinding used
# to attack: a page that could reach the control plane's port while looking
# like a local visitor. There is no visitor to look like any more - the gate
# asks every caller for a credential before it asks anything else about
# them - so the request below carries an attacker's Host header on purpose
# and expects the same answer as the polite one.
check_no_credential_gets_into_the_human_api() {
  local title="the human API answers a caller with no credential 401, on every host"

  local listed rebound write
  listed=$(control_status GET "/api/v1/apps")
  rebound=$(curl -s -o /dev/null -w '%{http_code}' \
    -H "Host: control-plane.attacker.example" "$(control_url)/api/v1/apps" || true)
  write=$(control_status POST "/api/v1/apps/${APP_ID}/environments/${ENV_NAME}/deployments" "" \
    '{"version_id":"never-checked"}')

  if [ "$listed" = "401" ] && [ "$rebound" = "401" ] && [ "$write" = "401" ]; then
    pass_check "$title" \
      "GET /api/v1/apps with no credential answered HTTP ${listed}; the same request under an attacker's Host header answered ${rebound}, which is the rebinding question and it has one answer now; and POSTing a deployment with no credential answered ${write}. There is no path into this API that does not present a token first - the operator's own calls pass because they carry one, not because of where they come from"
  else
    fail_check "$title" \
      "GET /api/v1/apps with no credential answered ${listed:-none}, the attacker-Host variant ${rebound:-none}, and the credential-less deploy POST ${write:-none} (expected 401 each)"
  fi
}

# The bootstrap credential's whole life: minted by the control plane into its
# data directory on a first start, traded for the first owner account by
# `deis login --bootstrap`, revoked and its file deleted in the same breath.
# `bootstrap_operator_session` kept the spent secret so this check can
# present it a second time and watch it be refused like a token that never
# existed.
check_the_bootstrap_credential_worked_exactly_once() {
  local title="the bootstrap credential logged the first owner in once and is spent"
  local spent_file="${DEMO_DIR}/used-bootstrap.token"
  local spent
  spent=$(cat "$spent_file" 2>/dev/null || true)
  if [ -z "$spent" ]; then
    fail_check "$title" "no spent bootstrap credential was kept at ${spent_file}; either start did not log in through one or it was thrown away"
    return
  fi

  local file_gone reused
  file_gone=yes
  [ -f "${CONTROL_DATA_DIR}/bootstrap.token" ] && file_gone=no
  reused=$(control_status GET "/api/v1/apps" "$spent")

  # The row the login created stays in the database as the record of what
  # happened: origin `bootstrap`, and revoked. Listed with the revoked ones,
  # because that is the half the claim is about.
  local record
  record=$(deis_cli token list --include-revoked --json 2>/dev/null \
    | jq -r 'first(.tokens[] | select(.origin == "bootstrap") | "account=\(.account) revoked=\(.revoked_at != null)") // empty')

  if [ "$file_gone" = "yes" ] && [ "$reused" = "401" ] \
    && echo "$record" | grep -q 'account=bootstrap revoked=true'; then
    pass_check "$title" \
      "the file the control plane wrote the bootstrap token to is gone, revoked the moment the login used it; presenting the spent secret again answered HTTP ${reused}, byte for byte the answer a token that never existed gets; and \`deis token list --include-revoked\` still shows the row - ${record} - which is the audit trail of how this installation's first owner was made"
  else
    fail_check "$title" \
      "the bootstrap file still present: ${file_gone}; presenting the spent secret answered ${reused:-none} (expected 401); the listing's bootstrap-origin row reads '${record:-none}' (expected account=bootstrap revoked=true)"
  fi
}

check_a_scoped_token_reaches_one_environment_and_no_other() {
  local title="a token scoped to one environment deploys there and is refused everywhere else"
  ensure_account ci deployer

  local minted token
  minted=$(deis_cli token create --account ci \
    --scope "deploy:${APP_ID}/${PREVIEW_ENV_NAME}" preview-pipeline --json 2>/dev/null || echo '{}')
  token=$(echo "$minted" | jq -r '.secret // empty')
  if [ -z "$token" ]; then
    fail_check "$title" "could not mint a scoped token: ${minted}"
    return
  fi

  # The version the preview environment already runs. Pointing it back at the
  # same version is a real deploy - a pointer row, a push to the node - and
  # changes nothing about what is being served, which is what makes it safe
  # to make with somebody else's credential in the middle of a demo.
  local preview_version
  preview_version=$(deployed_version "$PREVIEW_ENV_NAME")

  # A deploy carries the application's configuration with it. Under the
  # locked environment standard a config value is deployment-time input: it
  # is not in the bundle, not in the canonical artifact and not kept by the
  # control plane between deploys, so every deploy request states the values
  # its declarations ask for and a request that omits a required one is
  # refused with EC_ENV_REQUIRED_MISSING before authority is ever the
  # question. The CLI reads them from the project file; this check is a
  # `curl` standing in for the CLI, so it reads them from the same place -
  # the staged project - rather than repeating them here, which would go
  # stale the first time the example app gains a variable.
  # The values of the version being re-pointed at, not of some other build.
  # `preview` runs the app-v2 project, whose BUILD_LABEL is what the earlier
  # check on two environments serving at once reads back, so sending app-v1's
  # values here would quietly relabel preview and break that claim one run
  # later.
  local config_values
  config_values=$(jq -c '.environment.variables // {}' \
    < "${DEMO_DIR}/app-v2/.config.json" 2>/dev/null || echo '{}')

  local allowed refused_env refused_secret refused_platform refused_token
  allowed=$(control_status POST \
    "/api/v1/apps/${APP_ID}/environments/${PREVIEW_ENV_NAME}/deployments" "$token" \
    "{\"version_id\":\"${preview_version}\",\"note\":\"deployed by a scoped token\",\"config_values\":${config_values}}")
  refused_env=$(control_status POST \
    "/api/v1/apps/${APP_ID}/environments/${ENV_NAME}/deployments" "$token" \
    "{\"version_id\":\"${preview_version}\",\"config_values\":${config_values}}")
  refused_secret=$(control_status PUT \
    "/api/v1/apps/${APP_ID}/environments/${PREVIEW_ENV_NAME}/secrets/CI_ATTEMPT" "$token" \
    '{"value":"this must never be stored"}')
  refused_platform=$(control_status GET "/api/v1/audit" "$token")
  refused_token=$(control_status GET "/api/v1/tokens" "$token")

  # `/api/v1` answers a refusal in the structured envelope, so the sentence
  # the caller is shown is `.error.message` - which is the legacy `error`
  # and `details` strings joined, and still the one that names the scope
  # that would have worked.
  local told
  told=$(control_answer POST \
    "/api/v1/apps/${APP_ID}/environments/${ENV_NAME}/deployments" "$token" \
    "{\"version_id\":\"${preview_version}\",\"config_values\":${config_values}}" \
    | jq -r '.error.message // empty')

  # Narrow has to stay usable. `deis status` walks every environment of the
  # application, so a credential that reaches one of them would break the
  # command outright if a refusal on the others were an error rather than a
  # row; this is the assertion that it is a row.
  local status_json="${LOG_DIR}/verify-scoped-status.json"
  local status_exit=0
  deis_cli_as "$token" status --json > "$status_json" 2>/dev/null || status_exit=$?
  local seen hidden
  seen=$(jq -r --arg env "$PREVIEW_ENV_NAME" \
    'first(.environments[] | select(.name == $env) | .version_number) // empty' \
    < "$status_json" 2>/dev/null || true)
  # `[...][0] | tostring` rather than jq's `//`: the value being looked for
  # here is `false`, and `//` treats false exactly like a missing field, so
  # the one answer this assertion is about would read as "no answer at all".
  hidden=$(jq -r --arg env "$ENV_NAME" \
    '[.environments[] | select(.name == $env) | .visible][0] | tostring' \
    < "$status_json" 2>/dev/null || true)

  # 201 or 202, and the difference is not about authority. The route answers
  # 201 when the pointer was moved and the node already has the version, and
  # 202 when the delivery is being coordinated and the caller is given a
  # `Location` to poll; which one a particular deploy gets depends on whether
  # this environment's configuration snapshot is the one the node is already
  # holding, not on what the credential was allowed to do. The claim under
  # test is that this token may deploy here at all, so both answers satisfy
  # it and the sentence below says which one arrived.
  if { [ "$allowed" = "201" ] || [ "$allowed" = "202" ]; } && [ "$refused_env" = "403" ] && [ "$refused_secret" = "403" ] \
    && [ "$refused_platform" = "403" ] && [ "$refused_token" = "403" ] \
    && [ "$status_exit" = "0" ] && [ -n "$seen" ] && [ "$hidden" = "false" ] \
    && echo "$told" | grep -q "deploy:${APP_ID}/${ENV_NAME}"; then
    pass_check "$title" \
      "carrying only deploy:${APP_ID}/${PREVIEW_ENV_NAME}, the token re-deployed ${PREVIEW_ENV_NAME} to ${preview_version} (HTTP ${allowed} - accepted, whether delivered outright or handed back a Location to poll), sending the project's own configuration ${config_values} with it because a deploy carries its configuration rather than finding it stored; the same call against ${ENV_NAME} answered ${refused_env}, writing a secret on its own environment ${refused_secret}, reading the audit log ${refused_platform} and listing credentials ${refused_token}. The refusal names the scope that would have worked: \"${told}\". \`deis status\` still works for it: ${PREVIEW_ENV_NAME} shows version ${seen} and ${ENV_NAME} comes back visible=${hidden} rather than failing the command"
  else
    fail_check "$title" \
      "deploy to ${PREVIEW_ENV_NAME} answered ${allowed} (expected 201 Created or 202 Accepted); ${ENV_NAME} ${refused_env}, secret ${refused_secret}, audit ${refused_platform}, tokens ${refused_token} (expected 403 each). \`deis status\` exited ${status_exit} showing ${PREVIEW_ENV_NAME} version '${seen:-none}' and ${ENV_NAME} visible '${hidden:-none}'. The refusal said: ${told:-nothing}"
  fi
}

check_a_credential_cannot_wire_an_app_to_one_it_cannot_see() {
  local title="a service binding needs authority over the application it names"
  ensure_account app-team deployer

  # A credential that owns the caller outright and has never heard of the
  # callee. Uploading the example app's own bundle with it is the whole test:
  # that bundle declares "AUTH": a service binding to ${CALLEE_APP_ID}, which
  # is one application claiming a right to call another, and the route the
  # upload arrives on names only the caller.
  # `deploy` and nothing else, because that is the credential a build pipeline
  # for this application actually holds, and the account's own role -
  # `deployer` - is the ceiling that stops it holding more.
  local minted token
  minted=$(deis_cli token create --account app-team \
    --scope "deploy:${APP_ID}/*" \
    caller-pipeline --json 2>/dev/null || echo '{}')
  token=$(echo "$minted" | jq -r '.secret // empty')
  if [ -z "$token" ]; then
    fail_check "$title" "could not mint a scoped token: ${minted}"
    return
  fi

  local bundle
  bundle=$(jq -r '.bundle.path // empty' < "${LOG_DIR}/cli-deploy-v1.json" 2>/dev/null || true)
  if [ ! -f "$bundle" ]; then
    fail_check "$title" "the example app's bundle is not at ${bundle}, so nothing could be uploaded"
    return
  fi

  local refused told
  refused=$(curl -s -o /dev/null -w '%{http_code}' -X POST \
    -H "Authorization: Bearer ${token}" -H "Content-Type: application/zip" \
    --data-binary "@${bundle}" "$(control_url)/api/v1/apps/${APP_ID}/versions" || true)
  told=$(curl -s -X POST \
    -H "Authorization: Bearer ${token}" -H "Content-Type: application/zip" \
    --data-binary "@${bundle}" "$(control_url)/api/v1/apps/${APP_ID}/versions" \
    | jq -r '.error.message // empty' 2>/dev/null || true)

  # The same bundle, with the same account, once it may also *see* the callee.
  # Read is the weakest verb in the grammar, which is the point: this is a
  # boundary between teams, not a second deploy permission.
  local widened widened_token accepted
  widened=$(deis_cli token create --account app-team \
    --scope "deploy:${APP_ID}/*" --scope "read:${CALLEE_APP_ID}/*" \
    caller-pipeline-widened --json 2>/dev/null || echo '{}')
  widened_token=$(echo "$widened" | jq -r '.secret // empty')
  accepted=$(curl -s -o /dev/null -w '%{http_code}' -X POST \
    -H "Authorization: Bearer ${widened_token}" -H "Content-Type: application/zip" \
    --data-binary "@${bundle}" "$(control_url)/api/v1/apps/${APP_ID}/versions" || true)

  # And it is written down, like every other refusal on the human API.
  local audited
  audited=$(deis_cli audit --app "$APP_ID" --outcome refused --json 2>/dev/null \
    | jq -r '[.entries[] | select(.action == "version.upload" and .status == 403)] | length' 2>/dev/null || echo 0)

  if [ "$refused" = "403" ] \
    && echo "$told" | grep -q "read:${CALLEE_APP_ID}/\*" \
    && { [ "$accepted" = "200" ] || [ "$accepted" = "201" ]; } \
    && [ "${audited:-0}" -ge 1 ]; then
    pass_check "$title" \
      "a token carrying deploy:${APP_ID}/* -- everything a build pipeline for the caller needs -- could not upload a bundle declaring a service binding to ${CALLEE_APP_ID}: HTTP ${refused}, \"$(printf '%s' "$told" | head -c 140)\". Adding read:${CALLEE_APP_ID}/* to the same account let the identical bundle through (HTTP ${accepted}), and the refusal is in the audit log (${audited} refused version.upload row(s))"
  else
    fail_check "$title" \
      "expected 403 naming read:${CALLEE_APP_ID}/* and then an accepted upload; the narrow token got ${refused} saying \"$(printf '%s' "${told:-nothing}" | head -c 160)\", the widened one got ${accepted}, and the audit log holds ${audited:-0} refused version.upload row(s)"
  fi
}

check_a_scoped_token_cannot_unload_an_application_from_a_node() {
  local title="a token for one environment cannot take the application off a node"
  ensure_account ci deployer

  # Exactly the credential a preview pipeline holds, and the same one the
  # scoped-token check proves can deploy to ${PREVIEW_ENV_NAME} and nowhere
  # else. `DELETE /api/platform/runtime/<node>/apps/<app>` stops the
  # application on the node; without ?environment= it stops every environment
  # of it, so it is a real outage of ${ENV_NAME} made with a credential that
  # may not deploy there. It used to be allowed, because the route named no
  # environment in its path and the authorisation table asked for "any
  # environment the caller holds" - which one preview scope answers.
  #
  # Every call below is one that has to be refused, so nothing on the node is
  # actually unloaded and the checks after this one still have an application
  # to look at.
  local minted token
  minted=$(deis_cli token create --account ci \
    --scope "deploy:${APP_ID}/${PREVIEW_ENV_NAME}" unload-attempt --json 2>/dev/null || echo '{}')
  token=$(echo "$minted" | jq -r '.secret // empty')
  if [ -z "$token" ]; then
    fail_check "$title" "could not mint a scoped token: ${minted}"
    return
  fi

  local whole named other told
  whole=$(control_status DELETE \
    "/api/platform/runtime/${NODE_ID}/apps/${APP_ID}" "$token")
  named=$(control_status DELETE \
    "/api/platform/runtime/${NODE_ID}/apps/${APP_ID}?environment=${ENV_NAME}" "$token")
  other=$(control_status DELETE \
    "/api/platform/runtime/${NODE_ID}/apps/${CALLEE_APP_ID}" "$token")
  # As above: `/api/platform` refuses in the structured envelope, and the
  # sentence naming the scope that would have worked is `.error.message`.
  told=$(control_answer DELETE \
    "/api/platform/runtime/${NODE_ID}/apps/${APP_ID}" "$token" \
    | jq -r '.error.message // empty')

  # The application is still being served, which is the claim that matters:
  # a refusal that had nonetheless unloaded something would be no refusal.
  local headers="${LOG_DIR}/verify-unload-refused.headers"
  app_request "$headers" "/" > /dev/null
  local still_serving
  still_serving=$(http_status "$headers")

  local audited
  audited=$(deis_cli audit --outcome refused --json 2>/dev/null \
    | jq -r '[.entries[] | select(.action == "node.app.unload")] | length' 2>/dev/null || echo 0)

  if [ "$whole" = "403" ] && [ "$named" = "403" ] && [ "$other" = "403" ] \
    && [ "$still_serving" = "200" ] && [ "${audited:-0}" -ge 3 ] \
    && echo "$told" | grep -q "deploy:${APP_ID}/\*"; then
    pass_check "$title" \
      "carrying only deploy:${APP_ID}/${PREVIEW_ENV_NAME}, the token was refused DELETE /api/platform/runtime/${NODE_ID}/apps/${APP_ID} (HTTP ${whole}), the same call naming ${ENV_NAME} (${named}) and the one against ${CALLEE_APP_ID} (${other}). The refusal asks for the whole application - \"$(printf '%s' "$told" | head -c 120)\" - because a call that names no environment stops every one of them. ${APP_ID} is still served (HTTP ${still_serving}) and ${audited} refused node.app.unload row(s) are on the record"
  else
    fail_check "$title" \
      "expected 403 for the whole application (got ${whole}), for ${ENV_NAME} (got ${named}) and for ${CALLEE_APP_ID} (got ${other}), naming deploy:${APP_ID}/* -- it said \"$(printf '%s' "${told:-nothing}" | head -c 160)\". ${APP_ID} answered ${still_serving} (expected 200) and the audit log holds ${audited:-0} refused node.app.unload row(s)"
  fi
}

# One gRPC call at the node's data port, answering with the grpc-status the
# node put in its response.
#
# A refusal is a trailers-only response - HTTP 200 with the status in the
# headers and no message at all - so reading the headers is enough and nothing
# here has to decode protobuf. An empty status means the call was served: a
# served answer puts its status in the trailers instead.
#
# The call goes over mutual TLS with the demo proxy's leaf, because that is
# the only shape of caller the data port talks to at all now - which is also
# why no credential header is asked about any more: a gRPC service that never
# sees the request has no string to answer a header with, and the door the
# header used to open was on a listener that no longer takes plaintext.
node_grpc_status() {
  local path=$1 frame=$2
  local args=(-s -o /dev/null -D - -X POST
    -H "content-type: application/grpc" -H "te: trailers"
    --data-binary "@${frame}")
  runtime_curl "${args[@]}" "$(runtime_data_url)${path}" 2>/dev/null \
    | tr -d '\r' | awk 'tolower($1) == "grpc-status:" {print $2}' | head -n 1
}

# Nothing in Deislet issues, renews or revokes a certificate - that is the
# installation's own authority, and keeping it out is what lets any PKI be
# substituted for any other. The cost of that decision is that expiry becomes
# somebody else's job, and nothing would notice it being forgotten: an
# expiring certificate is invisible right up to the moment it expires, and
# then every connection on the machine channel fails at the same instant.
# Nodes stop fetching assignments, the control plane stops being able to drive
# them, the proxy stops seeing route changes - all at once, for a reason none
# of those processes would name.
#
# So each process says, at startup, when its own certificate runs out. This
# asks whether it really did. The date is the useful half: an operator reading
# a healthy process's log should be able to find out when somebody has to act,
# rather than finding out when it has already become urgent.
#
# This demo's certificates are minted for two days, so none of them is close
# enough to expiry for the warning to fire - which is the other half of the
# rule and is checked here too. A threshold that fired on a certificate issued
# minutes ago would teach an operator that these warnings are noise, on the
# way to the one that is not.
check_every_process_says_when_its_certificate_runs_out() {
  local title="each process says at startup when its own certificate expires"
  local phrase="certificate is good for that long"

  # deis-proxy's own tracing goes to the error log pingora is configured with,
  # not to the file its stdout is redirected into - that one carries pingora's
  # own lines. Reading the wrong one here would report the proxy as silent
  # while it was saying exactly what was asked of it.
  local control node proxy
  control=$(grep -c "$phrase" "${LOG_DIR}/deis-control.log" 2>/dev/null) || control=0
  node=$(grep -c "$phrase" "${LOG_DIR}/deis-runtime.log" 2>/dev/null) || node=0
  proxy=$(grep -c "$phrase" "${LOG_DIR}/deis_proxy_error.log" 2>/dev/null) || proxy=0

  # The name and the remaining time have to be on the line itself. A line that
  # said only "expires soon" would send the reader back to openssl to find out
  # which of three machines it was about.
  local named remaining
  named=$(grep "$phrase" "${LOG_DIR}/deis-control.log" 2>/dev/null \
    | grep -c "identity=spiffe://${FLEET_TRUST_DOMAIN}/deislet/control") || named=0
  remaining=$(grep "$phrase" "${LOG_DIR}/deis-control.log" 2>/dev/null \
    | grep -oE 'remaining_secs=[0-9]+' | head -n 1 | cut -d= -f2)

  # And nothing is warning about it, because nothing should be: every
  # certificate here was minted minutes ago for two days.
  local premature=0
  premature=$(cat "${LOG_DIR}/deis-control.log" "${LOG_DIR}/deis-runtime.log" \
    "${LOG_DIR}/deis_proxy_error.log" 2>/dev/null \
    | grep -c "has used four fifths of its life\|in the last twentieth of its life\|certificate has expired") || premature=0

  if [ "$control" -ge 1 ] && [ "$node" -ge 1 ] && [ "$proxy" -ge 1 ] \
    && [ "$named" -ge 1 ] && [ -n "$remaining" ] && [ "$remaining" -gt 0 ] \
    && [ "$premature" -eq 0 ]; then
    pass_check "$title" \
      "the control plane, the node and the proxy each logged when their own certificate runs out at startup, and the control plane's line names spiffe://${FLEET_TRUST_DOMAIN}/deislet/control with ${remaining} second(s) left - so an operator reading a healthy process's log learns the date rather than learning it from every connection failing at once. None of the three warned, which is correct: these were minted minutes ago for two days, and a threshold that fired on a fresh certificate would teach an operator that this warning is noise"
    return
  fi

  fail_check "$title" \
    "expected all three to say when their certificate expires (control ${control}, node ${node}, proxy ${proxy} line(s)), the control plane's line to name its own identity (${named}) and carry a positive remaining_secs (got \"${remaining:-none}\"), and nothing to be warning yet (found ${premature})"
}

# The check above asks the control plane to unload an application and is
# refused there. This one goes at the node directly, the way anything that can
# route to the node's port can.
#
# The node serves the control plane over gRPC: ApplyDeployment, UnloadApp,
# GetStatus, StartInspect, PushConfig and the trigger service's OnQueue and
# OnScheduled. Every one of them is administrative control of the node rather
# than application traffic, and all of them used to be dispatched on this
# port, the one that carries public traffic, with no credential checked at all
# - so GetStatus listed every tenant's applications on the node, UnloadApp
# took one offline and the next request to it answered 500, StartInspect
# minted an inspector session for another tenant's isolate, and
# ApplyDeployment would have run a bundle of the caller's choosing under that
# tenant's endpoint.
#
# Then for a while they were refused here, on a shared value: whoever held the
# fleet key was taken for the control plane. Both of those states are gone,
# and what replaced the second one is worth stating precisely, because it is
# not a stricter refusal - it is nothing to refuse. This port has no
# management dispatcher on it at all. A POST to a management path is ordinary
# application traffic and is answered as such: without an `x-deis-app-id`
# header the node asks for one, and with a real one the path is handed to that
# tenant's own code, which has no such route and says 404 with a trace id on
# it. Neither answer carries a grpc-status, because no gRPC service saw the
# request.
#
# That distinction is the whole check. "Refused" is a door with a lock, and a
# lock is a thing that can be picked, misconfigured, or quietly opened by a
# later change. "Not dispatched" is no door. So this asserts the shape of the
# answers rather than a status code: a management path here looks exactly like
# a request for a page the application does not have.
#
# What drives the node instead is the second socket, the one
# `[machine_identity] management_listen` binds, where the caller proves a
# certificate naming this node's control plane before it sends a request. A
# plaintext caller does not get a connection to it at all, which is the next
# probe.
#
# The last two assertions are the other half - the platform still works -
# because the control plane's own health poll has to reach the node somehow,
# and a node it cannot reach is reported unhealthy. They are what says the
# management plane moved rather than simply closed.
check_the_node_management_plane_is_not_on_the_data_port() {
  local title="a node is driven only over the socket where its control plane proves its name"
  local frame="${LOG_DIR}/verify-grpc-empty-frame.bin"
  # One uncompressed, zero-length message: a complete GetStatusRequest and a
  # complete ScheduledEvent, both of which encode to nothing under proto3. The
  # bytes come from /dev/zero because a shell variable cannot carry a NUL.
  head -c 5 /dev/zero > "$frame"

  # No grpc-status on either, which is the assertion: a status would mean a
  # gRPC service had seen the request and decided something about it.
  local grpc_none grpc_trigger
  grpc_none=$(node_grpc_status "/deis.management.RuntimeManagement/GetStatus" "$frame")
  grpc_trigger=$(node_grpc_status "/deis.trigger.Trigger/OnScheduled" "$frame")

  # And what the port does answer, which is what says these are being read as
  # application traffic rather than refused as management calls. These go
  # over the same mutual TLS as every caller of this port now: the credential
  # question is the certificate's, and the check below asks it directly.
  local without_app_id with_app_id trigger_with_app_id traced
  without_app_id=$(runtime_curl -s -o /dev/null -w '%{http_code}' -X POST \
    -H "content-type: application/grpc" -H "te: trailers" \
    --data-binary "@${frame}" \
    "$(runtime_data_url)/deis.management.RuntimeManagement/GetStatus" 2>/dev/null || true)

  local traced_headers="${LOG_DIR}/verify-node-management-path.headers"
  with_app_id=$(runtime_curl -s -o /dev/null -D "$traced_headers" -w '%{http_code}' -X POST \
    -H "content-type: application/grpc" -H "te: trailers" -H "x-deis-app-id: ${APP_ID}" \
    --data-binary "@${frame}" \
    "$(runtime_data_url)/deis.management.RuntimeManagement/GetStatus" 2>/dev/null || true)
  traced=$(tr -d '\r' < "$traced_headers" 2>/dev/null \
    | awk 'tolower($1) == "x-deis-trace-id:" {print $2}' | head -n 1)

  trigger_with_app_id=$(runtime_curl -s -o /dev/null -w '%{http_code}' -X POST \
    -H "content-type: application/grpc" -H "te: trailers" -H "x-deis-app-id: ${APP_ID}" \
    --data-binary "@${frame}" \
    "$(runtime_data_url)/deis.trigger.Trigger/OnScheduled" 2>/dev/null || true)

  # And the socket that does drive this node, asked in plaintext. A caller
  # that cannot prove the control plane's name never completes a handshake,
  # so there is no HTTP response at all and curl exits non-zero.
  local plaintext_at_machine_plane=0
  curl -s -o /dev/null -X POST \
    -H "content-type: application/grpc" -H "te: trailers" \
    --data-binary "@${frame}" \
    "http://127.0.0.1:${DEIS_RUNTIME_MACHINE_PORT}/deis.management.RuntimeManagement/GetStatus" \
    >/dev/null 2>&1 || plaintext_at_machine_plane=$?

  # Nothing above unloaded anything, so the application is still there.
  local headers="${LOG_DIR}/verify-node-management-auth.headers"
  app_request "$headers" "/" > /dev/null
  local still_serving
  still_serving=$(http_status "$headers")

  local fleet healthy
  fleet="${LOG_DIR}/verify-node-management-auth-fleet.json"
  control_api /api/platform/runtime > "$fleet" 2>/dev/null || true
  healthy=$(jq -r --arg id "$NODE_ID" \
    '.nodes[] | select(.id == $id) | .healthy' < "$fleet" 2>/dev/null || true)

  if [ -z "$grpc_none" ] && [ -z "$grpc_trigger" ] \
    && [ "$without_app_id" = "400" ] && [ "$with_app_id" = "404" ] \
    && [ "$trigger_with_app_id" = "404" ] && [ -n "$traced" ] \
    && [ "$plaintext_at_machine_plane" -ne 0 ] \
    && [ "$still_serving" = "200" ] && [ "$healthy" = "true" ]; then
    pass_check "$title" \
      "GetStatus at the node's data port, asked over the mutual TLS the port now requires, answered no grpc-status at all - and neither did the trigger service's OnScheduled. What the port did answer is HTTP ${without_app_id} asking for an x-deis-app-id header, and with ${APP_ID}'s header on it, HTTP ${with_app_id} carrying trace id ${traced}: the path was handed to the application's own router, which has no such route. So there is no management call to refuse here, and no string a caller could bring that would change that - the questions a shared value used to answer are the certificate's now, and the next check asks those. A plaintext call at the mutually authenticated plane on port ${DEIS_RUNTIME_MACHINE_PORT} got no response at all (curl exited ${plaintext_at_machine_plane}), because a caller that has not proved this node's control plane's name does not get a connection. ${APP_ID} still answers HTTP ${still_serving}, and the control plane reports ${NODE_ID} healthy, which is its own health poll getting through the one door that is open"
  else
    fail_check "$title" \
      "expected no grpc-status on the data port for GetStatus (got \"${grpc_none:-none}\") or OnScheduled (got \"${grpc_trigger:-none}\"); HTTP 400 without an app id (got ${without_app_id:-none}), 404 with one (got ${with_app_id:-none}) carrying a trace id (got \"${traced:-none}\"), and 404 for OnScheduled with one (got ${trigger_with_app_id:-none}); a plaintext call at port ${DEIS_RUNTIME_MACHINE_PORT} refused at the handshake (curl exited ${plaintext_at_machine_plane}, expected non-zero). ${APP_ID} answered ${still_serving} (expected 200) and the control plane reports ${NODE_ID} healthy=${healthy:-unknown}"
  fi
}

# The other half of the sentence the check above starts. Its probes speak
# mutual TLS and reach the application, which leaves open who the certificate
# itself admits. The answer the design wants is a fixed list: a caller whose
# certificate names a proxy of this fleet. Everything else is refused at the
# handshake, where nothing is dispatched and nothing of the request is read -
# so these three witnesses assert the shape of the answers, as the one above
# does, and the first two of them are the two ways to fail the same way: to
# bring nothing at all, and to bring a real certificate that speaks for the
# wrong machine. The third is a certificate in the right shape from an
# authority this fleet has never heard of, which is the attack the shared
# value could not tell from a colleague.
check_the_data_port_admits_a_fleet_proxy_and_nothing_else() {
  local title="a node's data port answers a fleet proxy and nothing else"

  local anonymous named rogue
  anonymous=$(curl -s -o /dev/null -w '%{http_code}' \
    --cacert "${PKI_DIR}/ca.crt" \
    "$(runtime_data_url)/" 2>/dev/null || true)
  named=$(curl -s -o /dev/null -w '%{http_code}' \
    --cacert "${PKI_DIR}/ca.crt" \
    --cert "${PKI_DIR}/impostor.crt" --key "${PKI_DIR}/impostor.key" \
    "$(runtime_data_url)/" 2>/dev/null || true)
  rogue=$(curl -s -o /dev/null -w '%{http_code}' \
    --cacert "${PKI_DIR}/ca.crt" \
    --cert "${PKI_DIR}/rogue.crt" --key "${PKI_DIR}/rogue.key" \
    "$(runtime_data_url)/" 2>/dev/null || true)

  # `000` is curl reporting that no HTTP status was ever received: the
  # handshake ended and no request was served. The proxy's own leaf got a
  # status from the same path in the checks above, which is what says the
  # refusals here are about the credential and not about the address.
  if [ "${anonymous}" = "000" ] && [ "${named}" = "000" ] && [ "${rogue}" = "000" ]; then
    pass_check "$title" \
      "three callers asked this node's data port for ${APP_ID}: one with no certificate, one with a real certificate of this fleet naming the wrong machine, and one with a certificate naming the right machine that this fleet never signed. None got an HTTP status at all - curl reported 000 for each - because the node ends every handshake where the certificate does not name a proxy of this fleet, before a byte of any request is read. What a shared value could not have said - is this caller the proxy or somebody who read the same configuration file - the handshake says about each of them"
  else
    fail_check "$title" \
      "expected 000 from all three callers at the data port: no certificate got ${anonymous:-none}, a certificate naming another machine got ${named:-none}, a certificate from an outside authority got ${rogue:-none}"
  fi
}

check_a_wide_credential_cannot_mint_a_storage_token() {
  local title="minting a storage token needs that exact tenant written down"
  ensure_account data-team maintainer

  local wide narrow blanket_status
  wide=$(deis_cli token create --account data-team \
    --scope 'deploy:*/*' --scope 'secrets:*/*' everything --json 2>/dev/null \
    | jq -r '.secret // empty')
  narrow=$(deis_cli token create --account data-team \
    --scope "storage:${APP_ID}/${ENV_NAME}" one-tenant --json 2>/dev/null \
    | jq -r '.secret // empty')

  # A blanket storage grant is refused where it is written, not where it
  # would be used: the token it hands out never expires and works against
  # deis-store without this control plane in the path.
  blanket_status=0
  deis_cli token create --account data-team --scope 'storage:*/*' blanket --json \
    >/dev/null 2>&1 || blanket_status=$?

  local route="/api/v1/apps/${APP_ID}/environments/${ENV_NAME}/storage-token"
  local refused allowed tenant
  refused=$(control_status GET "$route" "$wide")
  allowed=$(control_status GET "$route" "$narrow")
  tenant=$(control_answer GET "$route" "$narrow" | jq -r '.tenant // empty')

  if [ "$refused" = "403" ] && [ "$allowed" = "200" ] \
    && [ "$tenant" = "${APP_ID}.${ENV_NAME}" ] && [ "$blanket_status" -ne 0 ]; then
    pass_check "$title" \
      "a credential carrying deploy:*/* and secrets:*/* was refused GET ${route} (HTTP ${refused}); the one carrying storage:${APP_ID}/${ENV_NAME} was served (HTTP ${allowed}) for tenant ${tenant}. \`deis token create --scope storage:*/*\` was refused outright (exit ${blanket_status}), so there is no blanket form of the grant to hand out by accident"
  else
    fail_check "$title" \
      "the wide credential answered ${refused} (expected 403), the tenant-scoped one ${allowed} for tenant '${tenant:-none}' (expected 200 for ${APP_ID}.${ENV_NAME}), and minting storage:*/* exited ${blanket_status} (expected non-zero)"
  fi
}

check_a_revoked_token_stops_at_its_next_call() {
  local title="a revoked token is refused at once, and looks like one that never existed"
  ensure_account sre viewer

  local minted token id
  minted=$(deis_cli token create --account sre \
    --scope "read:${APP_ID}/${ENV_NAME}" laptop --json 2>/dev/null || echo '{}')
  token=$(echo "$minted" | jq -r '.secret // empty')
  id=$(echo "$minted" | jq -r '.id // empty')
  if [ -z "$token" ]; then
    fail_check "$title" "could not mint a token to revoke: ${minted}"
    return
  fi

  local route="/api/v1/apps/${APP_ID}/environments/${ENV_NAME}/deployment"
  local before after unknown revoked_body unknown_body
  before=$(control_status GET "$route" "$token")
  deis_cli token revoke "$id" --json >/dev/null 2>&1 || true
  after=$(control_status GET "$route" "$token")
  unknown=$(control_status GET "$route" "$NEVER_MINTED_TOKEN")
  # `/api/v1` answers a refusal in the structured envelope, and every
  # envelope carries the `requestId` of the request it answers - a fresh
  # UUIDv7 each time, which two answers can never share. It says nothing
  # about the credential, so it is dropped here and everything else - the
  # code, the sentence, the violations - is what the two answers are
  # compared on. Comparing the raw bytes instead would report the two
  # credentials as distinguishable on every run, which is the opposite of
  # what this check is about.
  revoked_body=$(control_answer GET "$route" "$token" \
    | jq -S 'del(.error.requestId)' 2>/dev/null || true)
  unknown_body=$(control_answer GET "$route" "$NEVER_MINTED_TOKEN" \
    | jq -S 'del(.error.requestId)' 2>/dev/null || true)

  # The control plane's own log does tell the two apart, which is the point:
  # "a revoked token was presented" means a credential is still deployed
  # somewhere and someone has to go and replace it.
  local said
  said=$(grep -c 'the token presented was revoked' "${LOG_DIR}/deis-control.log" 2>/dev/null || true)

  if [ "$before" = "200" ] && [ "$after" = "401" ] && [ "$unknown" = "401" ] \
    && [ "$revoked_body" = "$unknown_body" ] && [ "${said:-0}" -ge 1 ]; then
    pass_check "$title" \
      "the token read ${route} (HTTP ${before}); after \`deis token revoke ${id}\` the very next request answered ${after}, and apart from the per-request \`requestId\` every byte of it is the answer a token that was never minted gets (${unknown}). No cache had to expire: every request looks the row up. The control plane's log says \"the token presented was revoked\" ${said} time(s), which a stolen credential cannot see"
  else
    fail_check "$title" \
      "before revoking ${before} (expected 200), after ${after} and an unknown token ${unknown} (expected 401 each); the two answers were $( [ "$revoked_body" = "$unknown_body" ] && echo identical || echo different ), and the log named a revoked token ${said:-0} time(s)"
  fi
}

check_the_audit_log_names_who_changed_what() {
  local title="the audit log names the secret that was written, its application and its author"
  local report="${LOG_DIR}/verify-audit-secret.json"
  deis_cli audit --action secret.write --limit 20 --json > "$report" 2>/dev/null || true

  # The row is chosen by what happened - a secret written on this
  # application's ${ENV_NAME} and accepted - and never by the name it carries.
  # It used to be selected with `select(.target == "DEMO_SECRET")` and then
  # asserted to have that target, which is the check reading back its own
  # filter: a control plane that recorded every secret write against an empty
  # name, or against the value instead of the name, would have produced no
  # matching row and the assertion would have been about a row that was not
  # there rather than about the name being right. Only one row can match: the
  # other secret.write in this report is the scoped token's refused attempt on
  # the preview.
  local entry total app environment target outcome actor from
  total=$(jq -r '.total // 0' < "$report" 2>/dev/null || echo 0)
  entry=$(jq -c --arg app "$APP_ID" --arg env "$ENV_NAME" \
    'first(.entries[] | select(.app_id == $app and .environment == $env and .outcome == "accepted"))' \
    < "$report" 2>/dev/null || true)
  app=$(echo "$entry" | jq -r '.app_id // empty' 2>/dev/null || true)
  environment=$(echo "$entry" | jq -r '.environment // empty' 2>/dev/null || true)
  target=$(echo "$entry" | jq -r '.target // empty' 2>/dev/null || true)
  outcome=$(echo "$entry" | jq -r '.outcome // empty' 2>/dev/null || true)
  actor=$(echo "$entry" | jq -r '.actor_credential // empty' 2>/dev/null || true)
  # "From where" is half of what the line is for, and it is the field most
  # easily left empty: it comes from the connection rather than from the
  # request, so a server assembled without connect info would record every
  # call as coming from nowhere and nothing else would notice.
  from=$(echo "$entry" | jq -r '.peer // empty' 2>/dev/null || true)

  # The whole reason a secret write is recorded by name and never by value.
  local leaked=no
  grep -q "$DEMO_SECRET_VALUE" "$report" 2>/dev/null && leaked=yes

  if [ "${total:-0}" -ge 1 ] \
    && [ "$app" = "$APP_ID" ] && [ "$environment" = "$ENV_NAME" ] && [ "$target" = "DEMO_SECRET" ] \
    && [ "$outcome" = "accepted" ] && [ -n "$actor" ] && [ -n "$from" ] && [ "$leaked" = "no" ]; then
    pass_check "$title" \
      "\`deis audit --action secret.write\` returned ${total} line(s); the one for DEMO_SECRET names ${app}/${environment}, the secret ${target}, the credential ${actor} that wrote it, the address ${from} it came from and the outcome ${outcome}. The ${#DEMO_SECRET_VALUE}-character value is not in the report, and the audit table lives in the same database the secret sweep already greps"
  else
    fail_check "$title" \
      "the accepted secret.write on ${APP_ID}/${ENV_NAME} reports app '${app:-none}', environment '${environment:-none}', target '${target:-none}' (expected DEMO_SECRET), outcome '${outcome:-none}', credential '${actor:-none}' from '${from:-nowhere}' out of ${total} line(s), and the value was $( [ "$leaked" = yes ] && echo PRESENT || echo absent )"
  fi
}

check_a_refused_call_is_recorded_with_the_reason() {
  local title="a refused call is recorded as fully as an accepted one"
  local report="${LOG_DIR}/verify-audit-refused.json"
  deis_cli audit --outcome refused --limit 50 --json > "$report" 2>/dev/null || true

  local total deploy_refusal detail anonymous
  total=$(jq -r '.total // 0' < "$report" 2>/dev/null || echo 0)
  deploy_refusal=$(jq -r --arg app "$APP_ID" --arg env "$ENV_NAME" \
    'first(.entries[] | select(.action == "deploy" and .app_id == $app and .environment == $env) | .status) // empty' \
    < "$report" 2>/dev/null || true)
  detail=$(jq -r --arg app "$APP_ID" --arg env "$ENV_NAME" \
    'first(.entries[] | select(.action == "deploy" and .app_id == $app and .environment == $env) | .detail) // empty' \
    < "$report" 2>/dev/null || true)
  anonymous=$(jq -r '[.entries[] | select(.actor_kind == "unknown")] | length' < "$report" 2>/dev/null || echo 0)

  # The anonymous count is asserted rather than only printed. It is the half
  # of the claim that is about callers the platform cannot name - the
  # storage-token check makes one such call - and a control plane that only
  # recorded refusals it could attribute to a credential would leave exactly
  # those out while this line went on reporting a number.
  if [ "${total:-0}" -ge 1 ] && [ "$deploy_refusal" = "403" ] \
    && [ "${anonymous:-0}" -ge 1 ] \
    && echo "$detail" | grep -q "deploy:${APP_ID}/${ENV_NAME}"; then
    pass_check "$title" \
      "${total} refusal(s) are on the record, ${anonymous} of them from a caller who presented nothing at all. The scoped token's attempt to deploy ${APP_ID}/${ENV_NAME} is there as HTTP ${deploy_refusal} with the reason \"${detail}\" - which is how an operator finds a pipeline still holding a credential that was narrowed"
  else
    fail_check "$title" \
      "the log holds ${total:-0} refusal(s), ${anonymous:-0} of them anonymous (expected at least one of each); the refused deploy on ${APP_ID}/${ENV_NAME} reports status '${deploy_refusal:-none}' with detail '${detail:-none}'"
  fi
}

check_the_audit_log_has_no_way_to_be_erased() {
  local title="no caller of this API can erase a line of the audit log"
  local before after removed created

  before=$(deis_cli audit --limit 500 --json 2>/dev/null | jq -r '.total // 0')
  # The demo's operator credential is the widest thing a caller holds here -
  # admin:*/* among its scopes, minted from the first owner account - so it
  # passes every admin scope check there is. If anything could delete a line,
  # this would.
  removed=$(control_status DELETE "/api/v1/audit" "$OPERATOR_TOKEN")
  created=$(control_status POST "/api/v1/audit" "$OPERATOR_TOKEN" '{"action":"forged"}')
  after=$(deis_cli audit --limit 500 --json 2>/dev/null | jq -r '.total // 0')

  if [ "$removed" = "405" ] && [ "$created" = "405" ] && [ "${after:-0}" -ge "${before:-0}" ]; then
    pass_check "$title" \
      "DELETE /api/v1/audit answered ${removed} and POST answered ${created} to the demo's operator credential, which carries admin:*/* - the route is a read and there is no writing or deleting endpoint behind it. The log went from ${before} to ${after} line(s) across both attempts; removing one means holding the database file, which is the same access that holds the secret master key"
    else
    fail_check "$title" \
      "DELETE answered ${removed} and POST answered ${created} (expected 405 for both), and the log went from ${before:-?} to ${after:-?} lines"
  fi
}

# The checks this group asks, in the order a run asks them, and the
# heading `verify` prints before them.
group_accounts_label="accounts, scopes and the audit log"
group_accounts_checks=(
  # After every check that expects a particular version on the preview
  # environment: the scoped-token check re-deploys preview to the version it
  # is already running, which is what proves a narrow credential can really
  # ship rather than only be refused.
  check_no_credential_gets_into_the_human_api
  check_the_bootstrap_credential_worked_exactly_once
  check_a_scoped_token_reaches_one_environment_and_no_other
  check_a_credential_cannot_wire_an_app_to_one_it_cannot_see
  check_a_scoped_token_cannot_unload_an_application_from_a_node
  check_the_node_management_plane_is_not_on_the_data_port
  # And the certificate question that check above leaves open: which callers
  # the data port's own handshake admits.
  check_the_data_port_admits_a_fleet_proxy_and_nothing_else
  check_every_process_says_when_its_certificate_runs_out
  check_a_wide_credential_cannot_mint_a_storage_token
  check_a_revoked_token_stops_at_its_next_call
  check_the_audit_log_names_who_changed_what
  check_a_refused_call_is_recorded_with_the_reason
  check_the_audit_log_has_no_way_to_be_erased
)
