# One group of `scripts/demo.sh verify`: nodes registering with the control
# plane, and the channel a machine has to prove itself on. Sourced by
# `scripts/demo.sh`.

check_cli_status() {
  local out="${LOG_DIR}/cli-status.json"
  local title="deis status reports the node actually serving"

  if ! deis_cli status --json > "$out" 2> "${LOG_DIR}/cli-status.log"; then
    fail_check "$title" \
      "the command failed: $(tail -n 3 "${LOG_DIR}/cli-status.log" | tr '\n' ' ')"
    return
  fi

  local healthy reachable serving envs prod_number
  healthy=$(jq -r --arg id "$NODE_ID" \
    '.nodes[] | select(.id == $id) | .healthy' < "$out" 2>/dev/null || true)
  reachable=$(jq -r --arg id "$NODE_ID" \
    '.nodes[] | select(.id == $id) | .reachable' < "$out" 2>/dev/null || true)
  # `serving` is what the node itself answered when asked, not what the
  # catalog intended. That distinction is the whole value of the command:
  # the catalog says what should be running, the node says what is.
  serving=$(jq -r --arg id "$NODE_ID" \
    '.nodes[] | select(.id == $id) | .serving // [] | map(.environment) | sort | join(", ")' \
    < "$out" 2>/dev/null || true)
  envs=$(jq -r '[.environments[] | "\(.name)=v\(.version_number // "none")"] | join(" ")' \
    < "$out" 2>/dev/null || true)
  prod_number=$(jq -r --arg env "$ENV_NAME" \
    '.environments[] | select(.name == $env) | .version_number // empty' < "$out" 2>/dev/null || true)

  if [ "$healthy" = "true" ] && [ "$reachable" = "true" ] \
    && echo "$serving" | grep -q "$ENV_NAME" \
    && echo "$serving" | grep -q "$PREVIEW_ENV_NAME" \
    && [ -n "$prod_number" ]; then
    pass_check "$title" \
      "node ${NODE_ID} is healthy and answered when asked; it reports serving ${APP_ID} in: ${serving}. The catalog side reads ${envs}"
  else
    fail_check "$title" \
      "node ${NODE_ID}: healthy=${healthy} reachable=${reachable} serving='${serving}' (expected both ${ENV_NAME} and ${PREVIEW_ENV_NAME}); environments read ${envs}"
  fi
}

check_node_registration() {
  local title="the node registers itself with the control plane"
  local fleet="${LOG_DIR}/verify-fleet.json"

  # No CLI command covers the fleet, so this reads the control plane's own
  # view of it. That is an operator question, not a deployment step.
  control_api /api/platform/runtime > "$fleet" 2>/dev/null || true

  local registered addr configured_addr
  registered=$(jq -r --arg id "$NODE_ID" \
    '.nodes[] | select(.id == $id) | .registered_at // empty' < "$fleet" 2>/dev/null || true)
  addr=$(jq -r --arg id "$NODE_ID" \
    '.nodes[] | select(.id == $id) | .addr // empty' < "$fleet" 2>/dev/null || true)
  configured_addr="http://127.0.0.1:${DEIS_RUNTIME_PORT}"

  # The give-away for the old mismatch: the node says so once and gives up.
  if grep -q "no node registration endpoint" "${LOG_DIR}/deis-runtime.log" 2>/dev/null; then
    fail_check "$title" \
      "the node reports that this control plane has no registration endpoint, so the two halves disagree about the path or the method"
    return
  fi

  if [ -z "$registered" ]; then
    fail_check "$title" \
      "the control plane has no registration timestamp for ${NODE_ID}; the fleet reads: $(head -c 300 "$fleet")"
    return
  fi

  # A node reports the address it derives from its own hostname; the operator
  # who wrote the file knows what routes from the control plane. The file
  # wins, or a node whose hostname does not resolve here would replace a
  # working address with a broken one and every push to it would start failing.
  if [ "$addr" != "$configured_addr" ]; then
    fail_check "$title" \
      "the node's self-reported address replaced the one in deis_runtime_nodes: ${configured_addr} became ${addr}, so every deployment push and cron fire now goes somewhere the operator did not choose"
    return
  fi

  pass_check "$title" \
    "${NODE_ID} registered at ${registered} on PUT /api/nodes/${NODE_ID}/registration, and the address from deis_runtime_nodes (${addr}) survived its self-report"
}

# The check above only shows that a node the config file already names also
# calls the registration endpoint - which every node here always does, so it
# would pass the same way even if a machine absent from deis_runtime_nodes
# could never join at all. That absence is exactly what "self-registration"
# is supposed to fix (see the module docs on
# crates/deis-control/src/api/registration.rs), and the demo's one node,
# being pre-listed, never puts it to the test.
#
# This calls the same endpoint by hand for an id that appears nowhere in the
# config file, with the node credential the demo's real node also uses, and
# checks that the control plane's fleet view now carries it with
# source=registered - not because an operator wrote it down, but because it
# announced itself. This is not a REST shortcut around `deis deploy`: there
# is no CLI command for a node to register with (that is deis-runtime's job,
# not a developer's), so speaking the same protocol a node speaks is the only
# way to exercise this from outside deis-runtime's own code. The address
# given is the demo's real node, so this is a second identity for a process
# that already exists rather than a node calling home to nowhere.
check_dynamic_node_registration() {
  local title="a node absent from deis_runtime_nodes joins the fleet by registering itself"
  local dynamic_id="${NODE_ID}-unlisted"
  local fleet="${LOG_DIR}/verify-fleet-dynamic.json"

  # Over the machine channel, proving a certificate that names this machine
  # and no other - which is the only way to join now. A registration is a
  # claim about which machine is at an address, and the control plane holds
  # that claim against the name the connection proved.
  if ! command -v grpcurl >/dev/null 2>&1; then
    fail_check "$title" \
      "grpcurl is not installed, so a node could not be registered the way a node registers"
    return
  fi

  local answer="${LOG_DIR}/verify-dynamic-register.log"
  grpcurl -max-time 10 \
    -cacert "${PKI_DIR}/ca.crt" \
    -cert "${PKI_DIR}/unlisted.crt" -key "${PKI_DIR}/unlisted.key" \
    -import-path "${ROOT_DIR}/crates/deis-wire/proto" -proto machine.proto \
    -d "{\"node_id\":\"${dynamic_id}\",\"registration_json\":\"{\\\"address\\\":\\\"http://127.0.0.1:${DEIS_RUNTIME_PORT}\\\"}\"}" \
    "127.0.0.1:${DEIS_CONTROL_MACHINE_PORT}" \
    deis.machine.MachineChannel/Register > "$answer" 2>&1 || true

  if ! grep -q "registrationJson" "$answer"; then
    fail_check "$title" \
      "Register over the machine channel, proving ${dynamic_id}'s own certificate, was not answered: $(tr '\n' ' ' < "$answer" | head -c 200)"
    return
  fi

  control_api /api/platform/runtime > "$fleet" 2>/dev/null || true

  local source registered_at
  source=$(jq -r --arg id "$dynamic_id" \
    '.nodes[] | select(.id == $id) | .source // empty' < "$fleet" 2>/dev/null || true)
  registered_at=$(jq -r --arg id "$dynamic_id" \
    '.nodes[] | select(.id == $id) | .registered_at // empty' < "$fleet" 2>/dev/null || true)

  # Take the invented identity away again, and check that it went.
  #
  # Nothing refreshes this registration, and a registration nobody refreshes
  # is a lease that runs out: the control plane then counts it among the nodes
  # it has lost and names it in the answer to every later deployment and
  # application deletion, because a node that stopped talking is normally one
  # that is still serving what it last received. This one is not - it is a
  # second name for the process already on the list as ${NODE_ID}.
  #
  # The removal is part of the verdict below rather than a swept-up loose end.
  # It used to be one line with its status thrown away and `|| true` after it,
  # which is the shape of every unwitnessed cleanup: a control plane that
  # answered 500 here left the phantom on the fleet, this check went on saying
  # ✓, and the run after it inherited a node nobody could explain. It is also
  # the only exercise `DELETE .../registration` gets anywhere in this file, so
  # discarding the answer left half the endpoint unchecked as well.
  local removed still_listed
  local removed_answer="${LOG_DIR}/verify-dynamic-deregister.log"
  grpcurl -max-time 10 \
    -cacert "${PKI_DIR}/ca.crt" \
    -cert "${PKI_DIR}/unlisted.crt" -key "${PKI_DIR}/unlisted.key" \
    -import-path "${ROOT_DIR}/crates/deis-wire/proto" -proto machine.proto \
    -d "{\"node_id\":\"${dynamic_id}\"}" \
    "127.0.0.1:${DEIS_CONTROL_MACHINE_PORT}" \
    deis.machine.MachineChannel/Deregister > "$removed_answer" 2>&1 || true
  removed=$(grep -c '"removed": true' "$removed_answer" 2>/dev/null || echo 0)
  control_api /api/platform/runtime > "$fleet" 2>/dev/null || true
  still_listed=$(jq -r --arg id "$dynamic_id" \
    '[.nodes[] | select(.id == $id)] | length' < "$fleet" 2>/dev/null || echo "?")

  if [ "$source" = "registered" ] && [ -n "$registered_at" ] \
    && [ "$removed" != "0" ] && [ "$still_listed" = "0" ]; then
    pass_check "$title" \
      "${dynamic_id} is not in deis_runtime_nodes; it joined the fleet by proving a certificate that names it and nothing else, with source=registered since ${registered_at}. Deregister, proving the same certificate, then reported it removed and the fleet no longer lists it - so the run leaves behind exactly the nodes the configuration file names. Joining used to be a PUT carrying a string every machine in the fleet held; it now takes an identity somebody had to issue"
  else
    fail_check "$title" \
      "Register over the machine channel for ${dynamic_id} left the fleet showing source='${source:-<absent>}' registered_at='${registered_at:-<none>}'; the Deregister that takes the invented identity away again reported removed=${removed} and left ${still_listed} entry/entries for it on the fleet"
  fi
}

# The other side of the two checks above: the routes a node used to call, on
# the listener a browser reaches.
#
# Self-registration is the one route on this control plane where an unlisted
# machine can put itself on the fleet, and a node on the fleet receives
# deployment pushes, cron fires and decrypted secrets. The two checks above
# register over the machine channel, proving a certificate that names one
# machine. This one asks what happened to the old way in: the same three
# requests, on the human listener, carrying a bearer value.
#
# The answer is 404 rather than 401, and the difference is the whole point. A
# 401 would say "these routes are here and a credential decides"; a 404 says
# they are not served here at all, so there is no string a caller could bring
# that would change the answer - including the fleet secret this demo used to
# configure everywhere and now configures nowhere.
#
# Nothing here can succeed, so nothing here leaves a node behind - and the
# fleet is read afterwards to say so.
check_the_node_channel_is_not_on_the_human_listener() {
  local title="nothing a node calls is served where the dashboard is"
  local invented="${NODE_ID}-uninvited-$$"

  # The three that used to be here, asked exactly as a node used to ask them,
  # carrying the kind of credential that used to open them. Nothing in this
  # demo configures a fleet key any more - that is the point of the value
  # below still being sent: whatever string a caller brings, these paths are
  # not here. A 404 says so; a 401 would have said a credential could change
  # it.
  local registration assignments routing
  registration=$(curl -s -o /dev/null -w '%{http_code}' -X PUT \
    -H "Authorization: Bearer ${SECRET}" -H 'content-type: application/json' \
    -d "{\"address\":\"http://127.0.0.1:${DEIS_RUNTIME_PORT}\"}" \
    "$(control_url)/api/nodes/${invented}/registration" || true)
  assignments=$(curl -s -o /dev/null -w '%{http_code}' \
    -H "Authorization: Bearer ${SECRET}" \
    "$(control_url)/api/nodes/${NODE_ID}/assignments" || true)
  routing=$(curl -s -o /dev/null -w '%{http_code}' \
    -H "Authorization: Bearer ${SECRET}" \
    "$(control_url)/api/routing-table" || true)

  # And the fleet, which is where anything that got through would show up.
  local fleet="${LOG_DIR}/verify-node-channel-fleet.json"
  control_api /api/platform/runtime > "$fleet" 2>/dev/null || true
  local joined healthy identity
  joined=$(jq -r --arg id "$invented" \
    '[.nodes[] | select(.id == $id)] | length' < "$fleet" 2>/dev/null || echo "?")
  healthy=$(jq -r --arg id "$NODE_ID" \
    '.nodes[] | select(.id == $id) | .healthy' < "$fleet" 2>/dev/null || true)
  identity=$(jq -r --arg id "$NODE_ID" \
    '.nodes[] | select(.id == $id) | .identity // "none"' < "$fleet" 2>/dev/null || true)

  if [ "$registration" = "404" ] && [ "$assignments" = "404" ] && [ "$routing" = "404" ] \
    && [ "$joined" = "0" ] && [ "$healthy" = "true" ] \
    && [ "$identity" != "none" ]; then
    pass_check "$title" \
      "PUT .../registration, GET .../assignments and GET the routing table all answered ${registration}/${assignments}/${routing} on the listener that serves the dashboard - carrying the fleet key, which used to open all three. They are not refused here, they are not here: the machine channel serves them and a caller there proves a name. The fleet lists ${joined} node(s) under ${invented}, and ${NODE_ID} is healthy and known by the name it proved (${identity}), so the fleet is being run from the other socket"
  else
    fail_check "$title" \
      "expected 404 from the human listener for registration (got ${registration}), assignments (got ${assignments}) and the routing table (got ${routing}), no fleet entry for ${invented} (found ${joined}), ${NODE_ID} healthy (healthy=${healthy:-unknown}) and known by a proved name (identity=${identity:-unknown})"
  fi
}


# ----------------------------------------------------------------------
# the machine channel
# ----------------------------------------------------------------------
#
# The three checks below are about the socket only Deislet's own processes
# talk on. What it answers with is every listed environment's decrypted
# secrets, a map of every hostname on the platform, and the code of every
# deployed version - so what may not reach it is as much the point as what
# may, and the first two checks are the only ones in this file whose expected
# outcome is that no request happens at all.
#
# The third is the one that could not have been written before. Under a
# fleet-wide key every machine presented the same credential, so "a valid
# credential held by the wrong machine" was not a thing that existed: a wrong
# credential was a bad string. It now has to be minted - a real certificate,
# signed by this fleet's own authority, naming a node that is not the one
# running here - and the refusal it earns is the whole reason this channel
# was built.

# Call the machine channel with whatever certificate is named, and report only
# whether a request happened. `--max-time` is generous because the answer this
# asks for is "nothing at all", and a slow refusal and a hang are different
# verdicts.
machine_channel_http() {
  local label=$1
  shift
  curl -s -o /dev/null -w '%{http_code}' --max-time 8 \
    --cacert "${PKI_DIR}/ca.crt" "$@" \
    "https://127.0.0.1:${DEIS_CONTROL_MACHINE_PORT}${MACHINE_PROBE_PATH}" 2>/dev/null
  printf ' %s' "$?"
}

# A path on the machine listener that exists. The version id is nonsense on
# purpose: a caller that gets through is answered 404 by the route, which is
# how this tells "the handshake succeeded and the route said no" from "there
# was never a connection".
MACHINE_PROBE_PATH="/api/apps/demo/versions/00000000-0000-0000-0000-000000000000/bundle"

# The trap this closes is not a refusal, it is the absence of one.
#
# Registration, the assignment list, the bundle download and the routing table
# are served only on the machine channel now. A control plane of this version
# does not serve their HTTP forms at all - the checks above prove those paths
# answer 404 - so a node or a proxy configured the old way has nowhere to ask.
#
# It does not fail, though. It starts, binds its ports, reports itself up, and
# then retries a 404 for as long as it runs: the node loads nothing and
# announces nothing, the proxy routes on whatever it can work out from its own
# file. Both look alive from outside and neither says why it is useless.
#
# So both refuse to start without a `[machine_identity]` table, and this is
# what holds them to it. The configurations below are the old shape exactly -
# an address to reach the control plane at, and nothing to prove - and what is
# asserted is that the process is gone and said which table is missing. A
# message that named neither would leave an operator reading a stack of 404s.
check_a_process_with_no_certificate_refuses_to_start() {
  local title="a process told where its control plane is, and given nothing to prove, refuses to start"
  local dir="${DEMO_DIR}/no-identity"
  mkdir -p "$dir"

  local node_config="${dir}/deis-runtime.toml"
  cat > "$node_config" <<EOF
node_id = "deis-runtime-demo-no-identity"
data_listen = "127.0.0.1:1"
data_dir = "${dir}/runtime-data"

[control]
endpoint = "http://127.0.0.1:${DEIS_CONTROL_PORT}"
EOF

  local proxy_config="${dir}/deis-proxy.toml"
  cat > "$proxy_config" <<EOF
listen_addr = ["127.0.0.1"]
listen_ports = [1]
database = "${dir}/deis-proxy.db"
pid_file = "${dir}/deis-proxy.pid"

[[upstreams]]
host = "127.0.0.1"
port = ${DEIS_RUNTIME_PORT}

[control]
addr = "http://127.0.0.1:${DEIS_CONTROL_PORT}"
associated_id = "deis-proxy-demo-no-identity"
EOF

  local node_out proxy_out node_rc proxy_rc
  node_rc=0
  node_out=$(DEIS_RUNTIME_CONFIG="$node_config" timeout 20 "${BIN_DIR}/deis-runtime" 2>&1) || node_rc=$?
  proxy_rc=0
  proxy_out=$(timeout 20 "${BIN_DIR}/deis-proxy" -c "$proxy_config" 2>&1) || proxy_rc=$?

  local node_named proxy_named
  node_named=no
  proxy_named=no
  case "$node_out" in *machine_identity*) node_named=yes ;; esac
  case "$proxy_out" in *machine_identity*) proxy_named=yes ;; esac

  # A port of 1 cannot be bound by an unprivileged process, so a run that got
  # as far as binding would also exit non-zero. The message is what tells the
  # two apart, which is why both halves are asserted and neither alone.
  if [ "$node_rc" -ne 0 ] && [ "$proxy_rc" -ne 0 ] \
    && [ "$node_named" = yes ] && [ "$proxy_named" = yes ]; then
    pass_check "$title" \
      "deis-runtime exited ${node_rc} and deis-proxy exited ${proxy_rc}, each naming [machine_identity] as what is missing - so a machine that could only have polled a 404 for the life of the process never gets that far. Everything either of them asks its control plane for is served on the machine channel, where a shared value opens nothing, and a process that started without a certificate would have looked healthy while doing nothing at all"
  else
    fail_check "$title" \
      "expected both to exit non-zero naming [machine_identity]: deis-runtime exited ${node_rc} (named it: ${node_named}) saying \"$(printf '%s' "$node_out" | tr '\n' ' ' | head -c 200)\"; deis-proxy exited ${proxy_rc} (named it: ${proxy_named}) saying \"$(printf '%s' "$proxy_out" | tr '\n' ' ' | head -c 200)\""
  fi
}

check_the_machine_channel_refuses_a_caller_with_no_certificate() {
  local title="the machine channel will not talk to a caller that proves nothing"

  local anonymous named
  anonymous=$(machine_channel_http anonymous)
  named=$(machine_channel_http node \
    --cert "${PKI_DIR}/node.crt" --key "${PKI_DIR}/node.key")

  # `000` is curl reporting that no HTTP status was ever received, and the
  # non-zero exit beside it is the transport failing. The node's own
  # certificate gets 404 from the same path, which is what says the listener
  # is there and answering - so the refusal above is about the credential and
  # not about the address being wrong.
  if [ "$anonymous" = "000 56" ] || [ "${anonymous%% *}" = "000" ]; then
    if [ "${named%% *}" = "404" ]; then
      pass_check "$title" \
        "a request to the machine listener with no client certificate got no HTTP status at all (curl reported \"${anonymous}\", status then exit code) - the connection ended during the handshake, so no request was ever served. The same path with ${NODE_ID}'s own certificate answered ${named%% *}, which is the route refusing a version id that does not exist: the listener is there, it answers, and what it would not do is talk to a caller that proved nothing"
    else
      fail_check "$title" \
        "the anonymous caller was refused (${anonymous}) but the node's own certificate did not reach the route either (got ${named}), so this check proved nothing about the credential"
    fi
  else
    fail_check "$title" \
      "a caller with no client certificate reached the machine listener and got ${anonymous}; every environment's decrypted secrets are served on that socket"
  fi
}

check_the_machine_channel_refuses_a_certificate_this_fleet_did_not_sign() {
  local title="a certificate this fleet did not sign does not open the machine channel"

  local rogue
  rogue=$(machine_channel_http rogue \
    --cert "${PKI_DIR}/rogue.crt" --key "${PKI_DIR}/rogue.key")

  if [ "${rogue%% *}" = "000" ]; then
    pass_check "$title" \
      "a certificate naming spiffe://${FLEET_TRUST_DOMAIN}/deislet/node/${NODE_ID} - the right name, correctly spelled, signed by an authority this fleet has never heard of - got no HTTP status at all (curl reported \"${rogue}\"). Being able to spell a machine's name is not the same as being that machine, and the handshake is where the difference is decided"
  else
    fail_check "$title" \
      "a certificate signed by an authority outside this fleet reached the machine listener and got ${rogue}"
  fi
}

check_a_fleet_certificate_only_speaks_for_the_machine_it_names() {
  local title="a real fleet certificate cannot ask for another machine's secrets"

  if ! command -v grpcurl >/dev/null 2>&1; then
    fail_check "$title" \
      "grpcurl is not installed, so the machine channel's own RPC could not be called and the check that a certificate speaks only for the machine it names did not run"
    return
  fi

  local answer="${LOG_DIR}/verify-machine-impostor.log"
  grpcurl -max-time 10 \
    -cacert "${PKI_DIR}/ca.crt" \
    -cert "${PKI_DIR}/impostor.crt" -key "${PKI_DIR}/impostor.key" \
    -import-path "${ROOT_DIR}/crates/deis-wire/proto" -proto machine.proto \
    -d "{\"node_id\":\"${NODE_ID}\"}" \
    "127.0.0.1:${DEIS_CONTROL_MACHINE_PORT}" \
    deis.machine.MachineChannel/FetchAssignments > "$answer" 2>&1 || true

  # And the same call made by the machine that certificate does name, so the
  # refusal above is about who asked rather than about the RPC being broken.
  #
  # Only whether it was answered is kept: the answer names every deployment
  # this node runs and the secret revisions it may fetch next, which is no
  # business of a log file.
  local own="${LOG_DIR}/verify-machine-own.count"
  grpcurl -max-time 10 \
    -cacert "${PKI_DIR}/ca.crt" \
    -cert "${PKI_DIR}/node.crt" -key "${PKI_DIR}/node.key" \
    -import-path "${ROOT_DIR}/crates/deis-wire/proto" -proto machine.proto \
    -d "{\"node_id\":\"${NODE_ID}\"}" \
    "127.0.0.1:${DEIS_CONTROL_MACHINE_PORT}" \
    deis.machine.MachineChannel/FetchAssignments 2>&1 \
    | grep -c "assignmentsJson" > "$own" || true

  local refused told served
  refused=$(grep -c "PermissionDenied" "$answer" 2>/dev/null || echo 0)
  told=$(grep -m1 "Message:" "$answer" 2>/dev/null | sed 's/^ *Message: *//' || true)
  served=$(cat "$own" 2>/dev/null || echo 0)

  # The refusal has to name both machines. "Denied" on its own leaves an
  # operator with a node that cannot start and no way to tell a misissued
  # certificate from a mistyped node id.
  if [ "$refused" != "0" ] && [ "$served" != "0" ] \
    && printf '%s' "$told" | grep -q "deis-runtime-demo-99" \
    && printf '%s' "$told" | grep -q "${NODE_ID}"; then
    pass_check "$title" \
      "a certificate this fleet really issued, naming deis-runtime-demo-99, asked for ${NODE_ID}'s assignment list - the answer that decides which secrets and capabilities that node may fetch - and was refused PermissionDenied: \"$(printf '%s' "$told" | head -c 160)\". The same call with ${NODE_ID}'s own certificate was answered. Under one shared fleet key this refusal could not be expressed at all: both callers would have presented the same credential"
    else
    fail_check "$title" \
      "expected PermissionDenied naming both machines for the impostor (found ${refused} such line(s), message \"${told:-nothing}\") and an answer for ${NODE_ID}'s own certificate (found ${served} answer(s))"
  fi
}

# What a node may be issued. A node asks IssueCapability in its own name, and
# is given a capability only for tenants of deployments assigned to it: the
# node running this demo gets one for ${APP_ID}.${ENV_NAME}, and the same
# certificate asking for a tenant it does not run is refused - so a
# compromised node cannot mint its way into another tenant's data. For
# deis-telemetry it may push, never read. Each refusal is in the audit log.
check_a_node_cannot_mint_for_a_tenant_it_does_not_run() {
  local title="a node is issued capabilities only for the tenants it runs"

  if ! command -v grpcurl >/dev/null 2>&1; then
    fail_check "$title" "grpcurl is not installed, so IssueCapability could not be called"
    return
  fi

  issue_as_node() {
    local audience=$1 tenant=$2 binding=$3
    local resources
    resources=$(jq -cn --arg t "$tenant" --arg b "$binding" '[{tenant: $t, binding: $b}]')
    grpcurl -max-time 10 \
      -cacert "${PKI_DIR}/ca.crt" \
      -cert "${PKI_DIR}/node.crt" -key "${PKI_DIR}/node.key" \
      -import-path "${ROOT_DIR}/crates/deis-wire/proto" -proto machine.proto \
      -d "$(jq -cn --arg n "$NODE_ID" --arg a "$audience" --arg r "$resources" \
        '{node_id: $n, audience: $a, resources_json: $r}')" \
      "127.0.0.1:${DEIS_CONTROL_MACHINE_PORT}" \
      deis.machine.MachineChannel/IssueCapability 2>&1 || true
  }

  local own foreign read audited
  own=$(issue_as_node store "${APP_ID}.${ENV_NAME}" kv)
  foreign=$(issue_as_node store "not-deployed-here.${ENV_NAME}" kv)
  read=$(issue_as_node telemetry "${APP_ID}.${ENV_NAME}" read)
  audited=$(deis_cli audit --action capability.issue --limit 50 --json 2>/dev/null \
    | jq -r '[.entries[]? | select(.detail // "" | contains("not-deployed-here"))] | length' 2>/dev/null || echo 0)

  if echo "$own" | grep -q 'capabilityToken' \
    && echo "$foreign" | grep -q 'Code: PermissionDenied' \
    && echo "$foreign" | grep -q 'not-deployed-here' \
    && echo "$read" | grep -q 'Code: PermissionDenied' \
    && [ "${audited:-0}" -ge 1 ]; then
    pass_check "$title" \
      "${NODE_ID}'s own certificate was issued a store capability for ${APP_ID}.${ENV_NAME}, which it runs; asking for not-deployed-here.${ENV_NAME} was refused ($(echo "$foreign" | grep -m1 'Message:' | sed 's/^ *Message: *//' | head -c 140)) and written to the audit log as capability.issue; asking for a telemetry read of its own tenant was refused too - a node pushes telemetry and reads none"
  else
    fail_check "$title" \
      "own tenant: $(echo "$own" | tr '\n' ' ' | head -c 120); foreign tenant: $(echo "$foreign" | tr '\n' ' ' | head -c 160); telemetry read: $(echo "$read" | tr '\n' ' ' | head -c 120); audited refusals: ${audited:-0}"
  fi
}

# The checks this group asks, in the order a run asks them, and the
# heading `verify` prints before them.
group_fleet_label="the fleet"
group_fleet_checks=(
  check_node_registration
  check_dynamic_node_registration
  # Straight after it: the same endpoint asked by a caller with no node key,
  # which is what says the two above were served for a reason. It leaves no
  # node behind, so it can run either side of them.
  check_the_node_channel_is_not_on_the_human_listener
  # The same question asked of the channel that replaces that key: what a
  # machine has to prove to reach the answers only machines may have.
  check_the_machine_channel_refuses_a_caller_with_no_certificate
  check_the_machine_channel_refuses_a_certificate_this_fleet_did_not_sign
  # And the other side of the same sentence: what happens to a machine that
  # has no certificate to prove. It is not refused at a door, it never starts.
  check_a_process_with_no_certificate_refuses_to_start
  check_a_fleet_certificate_only_speaks_for_the_machine_it_names
  check_a_node_cannot_mint_for_a_tenant_it_does_not_run
  check_cli_status
)
