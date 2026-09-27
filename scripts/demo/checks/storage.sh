# One group of `scripts/demo.sh verify`: KV, SQL and blob storage, the
# credential each acts with, and the isolation between tenants and
# environments. Sourced by `scripts/demo.sh`.

check_kv_roundtrip() {
  local title="context.env.KV round-trips through deis-store"
  local key="demo-kv-$$"
  local value="stored-through-deis-store"
  local headers="${LOG_DIR}/verify-kv.headers"

  local put
  put=$(runtime_curl -s -D "$headers" -X PUT -H "x-deis-app-id: ${APP_ID}" \
    --data-binary "$value" \
    "$(runtime_data_url)/api/kv/${key}" || true)
  if [ "$(http_status "$headers")" != "201" ]; then
    fail_check "$title" "PUT answered $(http_status "$headers"): ${put}"
    return
  fi

  local got
  got=$(app_request "${LOG_DIR}/verify-kv-get.headers" "/api/kv/${key}")
  if [ "$(echo "$got" | jq -r '.value // empty' || true)" != "$value" ]; then
    fail_check "$title" \
      "GET answered $(http_status "${LOG_DIR}/verify-kv-get.headers"): ${got}"
    return
  fi

  # Reading it back through the node only proves the isolate received an
  # answer, not where the answer came from - a cache inside the node would
  # look identical from here. What proves deis-store itself holds the row is
  # asking deis-store's own KvStore.Get directly, over the same gRPC service
  # the node calls and with the same tenant header a request for this
  # application and environment carries (deis_wire::conventions::tenant_id).
  #
  # A grep of deis-store's sqlite file at rest was tried here before and
  # dropped: that file runs in WAL mode, a just-written row can sit in the
  # -wal file for a while, and a grep of the base file alone would then find
  # nothing and prove nothing either way about whether the write happened -
  # which is not evidence, in either direction. Asking the service is
  # deterministic instead of racing its checkpoint.
  if ! command -v grpcurl >/dev/null 2>&1; then
    fail_check "$title" \
      "PUT then GET returned ${value} from the node, but grpcurl is not installed so deis-store could not be asked directly whether it holds the row"
    return
  fi
  if ! command -v openssl >/dev/null 2>&1; then
    fail_check "$title" \
      "PUT then GET returned ${value} from the node, but openssl is not installed so this check could not decode deis-store's direct answer"
    return
  fi

  # deis-store's KvStore.Get answers `value` as bytes (GetResponse.value is
  # `bytes`, not `string` - a KV value is whatever the application wrote), so
  # grpcurl's JSON rendering of it is base64, same as any protobuf bytes
  # field. Decode before comparing, or every value that happens not to equal
  # its own base64 form would fail this check regardless of whether
  # deis-store actually holds the right bytes.
  local tenant="${APP_ID}.${ENV_NAME}"
  local direct direct_value
  direct=$(grpcurl -plaintext -max-time 5 \
    -import-path "${ROOT_DIR}/crates/deis-store/proto" -proto storage.proto \
    -H "x-deis-tenant: ${tenant}" \
    -H "authorization: Bearer $(storage_token)" \
    -H "x-deis-binding: KV" \
    -d "{\"key\":\"${key}\"}" \
    "127.0.0.1:${DEIS_STORE_PORT}" deis.storage.KvStore/Get 2>&1)
  direct_value=$(echo "$direct" | jq -r '.value // empty' 2>/dev/null | openssl base64 -d -A 2>/dev/null || true)

  if [ "$direct_value" = "$value" ]; then
    pass_check "$title" \
      "PUT then GET returned ${value} from the node, and deis-store's own KvStore.Get answers the same value directly for tenant ${tenant}, binding KV, key ${key}"
  else
    fail_check "$title" \
      "the node answered ${value}, but deis-store's own KvStore.Get for tenant ${tenant}, binding KV, key ${key} returned: $(echo "$direct" | tr '\n' ' ' | head -c 200)"
  fi
}

# The other half of the capability-token feature: not that a call carrying
# the right token succeeds (check_kv_roundtrip's direct call already proves
# that), but that a call carrying none is refused before it touches any data.
# Without this check an interceptor left off the service would go unnoticed:
# every check above would still pass, with loopback silently standing in for
# the whole of it - exactly the gap crates/deis-store/src/auth.rs exists to
# close.
# ---- named bindings ---------------------------------------------------
#
# The claim: an application can declare any number of namespaces of a kind,
# reach each by the name it chose, and never have two of them touch. The
# checks below prove it from three sides -- from inside the isolate, from
# deis-store's own service, and from the CLI -- because the first alone would
# be satisfied by a node that kept three maps in memory.

check_named_bindings_are_separate_namespaces() {
  local title="three named kv bindings of one application never see each other"
  local key="binding-probe-$$"
  local headers="${LOG_DIR}/verify-binding.headers"

  # The same key, written through three named namespaces of the one tenant.
  app_request "$headers" "/api/bindings/HOT?key=${key}" -X PUT --data-binary "hot-value" >/dev/null
  if [ "$(http_status "$headers")" != "201" ]; then
    fail_check "$title" "PUT through HOT answered $(http_status "$headers")"
    return
  fi
  app_request "$headers" "/api/bindings/COLD?key=${key}" -X PUT --data-binary "cold-value" >/dev/null
  app_request "$headers" "/api/bindings/default?key=${key}" -X PUT --data-binary "kv-value" >/dev/null

  local hot cold kv hot_ns same_object
  hot=$(app_request "${LOG_DIR}/verify-binding-hot.headers" "/api/bindings/HOT?key=${key}")
  cold=$(app_request "${LOG_DIR}/verify-binding-cold.headers" "/api/bindings/COLD?key=${key}")
  kv=$(app_request "${LOG_DIR}/verify-binding-plain.headers" "/api/bindings/default?key=${key}")
  hot_ns=$(echo "$hot" | jq -r '.namespace // empty')
  same_object=$(echo "$hot" | jq -r '.sameObjectOnEnv // empty')

  if [ "$(echo "$hot" | jq -r '.value // empty')" != "hot-value" ] \
    || [ "$(echo "$cold" | jq -r '.value // empty')" != "cold-value" ] \
    || [ "$(echo "$kv" | jq -r '.value // empty')" != "kv-value" ]; then
    fail_check "$title" \
      "the same key read back differently per namespace was expected; HOT=$(echo "$hot" | jq -c '.value'), COLD=$(echo "$cold" | jq -c '.value'), KV=$(echo "$kv" | jq -c '.value')"
    return
  fi
  if [ "$same_object" != "true" ] || [ "$hot_ns" != "HOT" ]; then
    fail_check "$title" \
      "context.env.HOT must be the declared object and know its own name; sameObjectOnEnv=${same_object}, namespace=${hot_ns}"
    return
  fi

  # Reading it back through the node proves the isolate got three answers, not
  # where they are kept. deis-store's own KvStore.Get, with the binding header
  # this platform stamps, is what proves the three are three namespaces on
  # disk. Each call carries the logical binding name the application declared.
  if ! command -v grpcurl >/dev/null 2>&1 || ! command -v openssl >/dev/null 2>&1; then
    fail_check "$title" \
      "the three namespaces answered hot-value / cold-value / kv-value through the node, but grpcurl or openssl is missing so deis-store could not be asked directly"
    return
  fi

  local tenant token direct_hot direct_kv
  tenant="${APP_ID}.${ENV_NAME}"
  token=$(storage_token)
  direct_hot=$(grpcurl -plaintext -max-time 5 \
    -import-path "${ROOT_DIR}/crates/deis-store/proto" -proto storage.proto \
    -H "x-deis-tenant: ${tenant}" -H "authorization: Bearer ${token}" \
    -H "x-deis-binding: HOT" \
    -d "{\"key\":\"${key}\"}" \
    "127.0.0.1:${DEIS_STORE_PORT}" deis.storage.KvStore/Get 2>&1 \
    | jq -r '.value // empty' 2>/dev/null | openssl base64 -d -A 2>/dev/null || true)
  direct_kv=$(grpcurl -plaintext -max-time 5 \
    -import-path "${ROOT_DIR}/crates/deis-store/proto" -proto storage.proto \
    -H "x-deis-tenant: ${tenant}" -H "authorization: Bearer ${token}" \
    -H "x-deis-binding: KV" \
    -d "{\"key\":\"${key}\"}" \
    "127.0.0.1:${DEIS_STORE_PORT}" deis.storage.KvStore/Get 2>&1 \
    | jq -r '.value // empty' 2>/dev/null | openssl base64 -d -A 2>/dev/null || true)

  if [ "$direct_hot" = "hot-value" ] && [ "$direct_kv" = "kv-value" ]; then
    pass_check "$title" \
      "key ${key} written through context.env.HOT, COLD and KV reads back separately; deis-store's own KvStore.Get for tenant ${tenant} answers the matching values with x-deis-binding: HOT and KV"
  else
    fail_check "$title" \
      "deis-store answered ${direct_hot:-<nothing>} for tenant ${tenant} with binding HOT and ${direct_kv:-<nothing>} with binding KV; expected hot-value and kv-value"
  fi
}

check_a_binding_cannot_reach_another_tenant() {
  local title="a binding name cannot be made to address another tenant"

  if ! command -v grpcurl >/dev/null 2>&1; then
    fail_check "$title" "grpcurl is missing, so this check could not be run"
    return
  fi

  local tenant token
  tenant="${APP_ID}.${ENV_NAME}"
  token=$(storage_token)

  # Every shape that would have to work for a binding to escape its tenant:
  # the tenant separator, the namespace separator, a traversal, and a name
  # long enough to run past the ceiling. deis-store refuses each before it
  # touches anything on disk, because the composed value is validated as a
  # whole and neither half may contain the other's separator.
  local hostile refused=0 total=0 sample=""
  for hostile in ".." "a.b" "a@b" "../../etc" "$(printf 'x%.0s' $(seq 1 65))"; do
    total=$((total + 1))
    local answer
    answer=$(grpcurl -plaintext -max-time 5 \
      -import-path "${ROOT_DIR}/crates/deis-store/proto" -proto storage.proto \
      -H "x-deis-tenant: ${tenant}" -H "authorization: Bearer ${token}" \
      -H "x-deis-binding: ${hostile}" \
      -d '{"key":"anything"}' \
      "127.0.0.1:${DEIS_STORE_PORT}" deis.storage.KvStore/Get 2>&1 || true)
    if echo "$answer" | grep -qi "namespace id"; then
      refused=$((refused + 1))
      [ -z "$sample" ] && sample=$(echo "$answer" | tr '\n' ' ' | head -c 160)
    fi
  done

  # And the token itself: it is minted for the tenant, so it does not become a
  # different token for a different binding. A call naming another tenant is
  # refused whatever binding it asks for.
  local cross
  cross=$(grpcurl -plaintext -max-time 5 \
    -import-path "${ROOT_DIR}/crates/deis-store/proto" -proto storage.proto \
    -H "x-deis-tenant: ${CALLEE_APP_ID}.${ENV_NAME}" -H "authorization: Bearer ${token}" \
    -H "x-deis-binding: HOT" \
    -d '{"key":"anything"}' \
    "127.0.0.1:${DEIS_STORE_PORT}" deis.storage.KvStore/Get 2>&1 || true)

  if [ "$refused" -eq "$total" ] && echo "$cross" | grep -q "Code: PermissionDenied"; then
    pass_check "$title" \
      "deis-store refused all ${total} hostile binding names for tenant ${tenant} (\"${sample}\"), and the capability token minted for ${tenant} was refused when it named ${CALLEE_APP_ID}.${ENV_NAME} even with a legal binding: the token authorises the tenant and the binding only chooses a namespace inside it"
    return
  fi
  fail_check "$title" \
    "${refused} of ${total} hostile binding names were refused, and the cross-tenant call answered: $(echo "$cross" | tr '\n' ' ' | head -c 200)"
}

check_cli_reads_a_named_binding() {
  local title="deis kv reads the namespace a binding names, and says which one"
  local key="cli-binding-$$"

  if ! deis_cli kv put "$key" "written-through-hot" --binding HOT -e "$ENV_NAME" \
    > "${LOG_DIR}/verify-cli-binding-put.log" 2>&1; then
    fail_check "$title" "deis kv put --binding HOT failed: $(tail -n 5 "${LOG_DIR}/verify-cli-binding-put.log")"
    return
  fi

  local hot unnamed
  hot=$(deis_cli kv get "$key" --binding HOT -e "$ENV_NAME" --json 2>/dev/null || true)
  unnamed=$(deis_cli kv get "$key" -e "$ENV_NAME" --json 2>/dev/null || true)

  # `.found` is read without a `//` fallback: jq treats `false` as unset, so
  # `.found // empty` would turn the very answer this check is looking for
  # into no answer at all. The value comes back base64 because a KV value is
  # bytes and this CLI never puts raw bytes on a terminal.
  local hot_value hot_tenant unnamed_found
  hot_value=$(echo "$hot" | jq -r '.value_base64 // empty' 2>/dev/null \
    | openssl base64 -d -A 2>/dev/null || true)
  hot_tenant=$(echo "$hot" | jq -r '.tenant // empty' 2>/dev/null || true)
  unnamed_found=$(echo "$unnamed" | jq -r 'if has("found") then .found else "" end' 2>/dev/null || true)

  # The same key through the isolate's own HOT binding, so the CLI and the
  # application are shown to be looking at one namespace rather than two that
  # happen to answer.
  local through_app
  through_app=$(app_request "${LOG_DIR}/verify-cli-binding-app.headers" \
    "/api/bindings/HOT?key=${key}" | jq -r '.value // empty' || true)

  if [ "$hot_value" = "written-through-hot" ] \
    && [ "$hot_tenant" = "${APP_ID}.${ENV_NAME}@HOT" ] \
    && [ "$unnamed_found" = "false" ] \
    && [ "$through_app" = "written-through-hot" ]; then
    pass_check "$title" \
      "deis kv put --binding HOT wrote ${key}; reading it back with --binding HOT gives written-through-hot and reports the namespace as ${hot_tenant}, the same command without --binding reports found=false, and the application reading context.env.HOT sees the CLI's value"
  else
    fail_check "$title" \
      "--binding HOT read ${hot_value:-<nothing>} in namespace ${hot_tenant:-<none>}, the unnamed namespace reported found=${unnamed_found:-<none>}, and the app read ${through_app:-<nothing>}"
  fi
}

check_storage_capability_token() {
  local title="a storage call presenting no capability token is refused"

  if ! command -v grpcurl >/dev/null 2>&1; then
    fail_check "$title" "grpcurl is not installed, so this check could not be run"
    return
  fi

  local tenant="${APP_ID}.${ENV_NAME}"
  local out rc
  out=$(grpcurl -plaintext -max-time 5 \
    -import-path "${ROOT_DIR}/crates/deis-store/proto" -proto storage.proto \
    -H "x-deis-tenant: ${tenant}" \
    -d "{\"key\":\"whatever-this-key-is-does-not-matter\"}" \
    "127.0.0.1:${DEIS_STORE_PORT}" deis.storage.KvStore/Get 2>&1) && rc=0 || rc=$?

  if [ "$rc" -ne 0 ] && echo "$out" | grep -q 'Code: Unauthenticated'; then
    pass_check "$title" \
      "KvStore.Get for tenant ${tenant} with no Authorization header was refused before deis-store looked at any data: $(echo "$out" | tr '\n' ' ' | sed 's/  */ /g')"
  else
    fail_check "$title" \
      "expected the call to be refused as Code: Unauthenticated with no token; deis-store answered: $(echo "$out" | tr '\n' ' ' | head -c 300)"
  fi
}

# ----------------------------------------------------------------------
# The credential `deis kv`, `deis db` and `deis blob` act with
# ----------------------------------------------------------------------
#
# deis-store authorises a call by the tenant it names and the capability token
# it presents. A deployment already carries one, which is how an application
# reaches its own data; a person has none, which is why the control plane
# mints one on
# GET /api/v1/apps/:app_id/environments/:env/storage-token. Without that
# route the three data commands have nothing to authenticate with and every
# one of them fails at the first call.

check_data_cli_reads_the_apps_write() {
  local title="deis kv reads back the value the application wrote"
  local key="cli-kv-$$-${RANDOM}"
  local value="written-by-the-handler-read-by-the-cli"
  local headers="${LOG_DIR}/verify-cli-kv.headers"

  # Written through the application, not through the CLI: the point is that
  # the two arrive at the same row, so the write has to come from the side
  # that is not being tested.
  runtime_curl -s -o /dev/null -D "$headers" -X PUT -H "x-deis-app-id: ${APP_ID}" \
    --data-binary "$value" \
    "$(runtime_data_url)/api/kv/${key}"
  if [ "$(http_status "$headers")" != "201" ]; then
    fail_check "$title" \
      "the handler could not store ${key}: HTTP $(http_status "$headers")"
    return
  fi

  local out="${LOG_DIR}/cli-kv-get.json"
  if ! deis_cli kv get "$key" --binding KV --json > "$out" 2> "${LOG_DIR}/cli-kv-get.log"; then
    fail_check "$title" \
      "deis kv get ${key} failed: $(tail -n 3 "${LOG_DIR}/cli-kv-get.log" | tr '\n' ' ')"
    return
  fi

  # A KV value is whatever bytes an application wrote, so `--json` carries it
  # base64 rather than as text it might not be. Decoding before comparing is
  # what makes this a check on the bytes instead of on their encoding.
  local tenant found read_back
  tenant=$(jq -r '.tenant // empty' < "$out" 2>/dev/null || true)
  found=$(jq -r '.found // false' < "$out" 2>/dev/null || true)
  read_back=$(jq -r '.value_base64 // empty' < "$out" 2>/dev/null \
    | openssl base64 -d -A 2>/dev/null || true)

  if [ "$found" = "true" ] && [ "$read_back" = "$value" ] \
    && [ "$tenant" = "${APP_ID}.${ENV_NAME}@KV" ]; then
    pass_check "$title" \
      "the handler stored ${key} through context.env.KV; \`deis kv get --binding KV\` asked the control plane for a storage token, presented it to deis-store as namespace ${tenant}, and read back \"${read_back}\" ($(printf '%s' "$read_back" | wc -c) bytes). The CLI never holds a signing key: it is handed a token for this one tenant"
  else
    fail_check "$title" \
      "deis kv get ${key} --binding KV reports namespace '${tenant:-<none>}', found=${found}, value '${read_back:-<none>}'; expected ${APP_ID}.${ENV_NAME}@KV and \"${value}\""
  fi
}

# The token is a credential that works directly against deis-store, without
# going through this control plane at all, and it does not expire. So the
# route that mints one sits on the human API, which has no unauthenticated
# path anywhere - and this is the check that the gate is really there rather
# than being a comment about one. Without it, a control plane that let a
# credential-less caller through would pass every other check in this file
# unchanged.
check_storage_token_route_requires_a_credential() {
  local title="the storage-token route refuses a caller with no credential"
  local route="/api/v1/apps/${APP_ID}/environments/${ENV_NAME}/storage-token"
  local minted="${LOG_DIR}/verify-storage-token.json"

  local anonymous authorised
  anonymous=$(curl -s -o /dev/null -w '%{http_code}' "$(control_url)${route}" || true)
  authorised=$(curl -s -o "$minted" -w '%{http_code}' \
    -H "Authorization: Bearer ${OPERATOR_TOKEN}" "$(control_url)${route}" || true)

  local tenant endpoint token_length
  tenant=$(jq -r '.tenant // empty' < "$minted" 2>/dev/null || true)
  endpoint=$(jq -r '.store_endpoint // empty' < "$minted" 2>/dev/null || true)
  token_length=$(jq -r '.token // empty' < "$minted" 2>/dev/null | tr -d '\n' | wc -c)

  if [ "$anonymous" = "401" ] && [ "$authorised" = "200" ] \
    && [ "$tenant" = "${APP_ID}.${ENV_NAME}" ] && [ "${token_length:-0}" -gt 0 ]; then
    pass_check "$title" \
      "GET ${route} with no credential answered HTTP ${anonymous} and no token; the same request carrying the demo's operator credential answered HTTP ${authorised} with tenant ${tenant}, store_endpoint ${endpoint} and a ${token_length}-character token, which is not printed here - it reads and writes that tenant's data directly against deis-store and does not expire"
  else
    fail_check "$title" \
      "anonymous GET ${route} answered HTTP ${anonymous} (expected 401) and the authenticated one HTTP ${authorised} for tenant '${tenant:-<none>}' with a ${token_length:-0}-character token (expected 200 for ${APP_ID}.${ENV_NAME})"
  fi
}

# The SQL sandbox decides what a tenant's statement may touch. On its own that
# leaves how much it may cost decided by nobody, and one tenant is one query
# away from denying deis-store to every tenant on the node: a `WITH RECURSIVE`
# counting to a hundred million ran 95.7 seconds on one core, and killing the
# client changed nothing, because the statement runs inside a blocking task
# that dropping the caller's future does not cancel.
#
# This is that query, sent the way an application or an operator sends one. It
# has to come back refused, quickly, saying which ceiling it crossed - a query
# killed with "database error" teaches nobody anything.
check_a_runaway_query_is_stopped_and_names_its_ceiling() {
  local title="a query with no end is stopped, and says which ceiling stopped it"
  local out="${LOG_DIR}/cli-db-runaway.log"
  local sql="WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x+1 FROM c WHERE x < 2000000000) SELECT count(*) FROM c"

  local started elapsed rc
  started=$(date +%s)
  deis_cli db query "$sql" --json > "$out" 2>&1 && rc=0 || rc=$?
  elapsed=$(( $(date +%s) - started ))

  local message
  message=$(tr '\n' ' ' < "$out" | sed 's/  */ /g')

  # Under twenty seconds: the store's own ceiling is five, and anything near
  # the ninety-six this used to take means nothing stopped it.
  if [ "$rc" -ne 0 ] && [ "$elapsed" -lt 20 ] \
    && echo "$message" | grep -q 'ms the store gives one request'; then
    pass_check "$title" \
      "\`deis db query\` sent a recursive count to two billion and deis-store stopped it after ${elapsed}s rather than running it to the end, answering: $(echo "$message" | head -c 240). The refusal rides inside the response, like a SQL typo, and names both the ceiling and what to change"
  else
    fail_check "$title" \
      "expected a refusal naming the statement ceiling within 20s; the call exited ${rc} after ${elapsed}s saying: $(echo "$message" | head -c 300)"
  fi
}

# The other half of the same gap, and the one that takes the process down
# rather than merely busy: `SELECT randomblob(20000000)` took deis-store from
# 18 MB resident to 1476 MB, because a blob comes back as a JSON array of byte
# values and the whole result set was built before anything looked at its size.
# SQLite is the only thing that can refuse this one - the blob is a single
# instruction, so nothing watching between instructions ever gets a turn.
check_a_single_value_cannot_be_bigger_than_a_row_may_be() {
  local title="one cell cannot be made larger than a row is allowed to be"
  local out="${LOG_DIR}/cli-db-huge-value.log"

  local rc
  deis_cli db query "SELECT randomblob(20000000) AS b" --json > "$out" 2>&1 && rc=0 || rc=$?

  local message
  message=$(tr '\n' ' ' < "$out" | sed 's/  */ /g')

  if [ "$rc" -ne 0 ] && echo "$message" | grep -q '2000000' \
    && echo "$message" | grep -q 'blob service'; then
    pass_check "$title" \
      "deis-store refused a twenty-megabyte cell before allocating it, naming its 2000000-byte ceiling on any single string, blob or row and pointing at the blob service for payloads that size: $(echo "$message" | head -c 240)"
  else
    fail_check "$title" \
      "expected a refusal naming the 2000000-byte value ceiling; the call exited ${rc} saying: $(echo "$message" | head -c 300)"
  fi
}

# ----------------------------------------------------------------------
# What a tenant's SQL may touch
# ----------------------------------------------------------------------
#
# The two checks above are about what a statement may *cost*. This one is
# about what it may *reach*, which is the question the cost ceilings leave
# entirely open - and the only one of the three storage services where nothing
# asked it. KV has check_a_binding_cannot_reach_another_tenant and blob has
# the tenant its capability token names; SQL had the runaway query and the
# oversized cell, and neither of them would notice if every tenant's statement
# ran against every tenant's file.
#
# A file per tenant is not by itself isolation, because plain SQLite can name
# a second file: `PRAGMA database_list` hands a caller the absolute path of
# its own database, which is every other tenant's path with one component
# changed; `ATTACH DATABASE` then opens that path to read and write; `VACUUM
# INTO` writes a copy of the caller's data anywhere the process can write. All
# three are ordinary SQL and all three arrive through the same
# the declared database binding an application already has.
#
# So the boundary is held under the SQL, by an authorizer callback and by
# `SQLITE_LIMIT_ATTACHED` pinned to zero
# (crates/deis-store/src/database.rs). This is the witness that it is held,
# driven the way a person drives it: `deis db`, once as each of two tenants of
# the same application, with the second one naming the first one's file by its
# real path on this machine.

# Run one statement as one tenant, and echo the exit status followed by
# everything the command said, on one line.
#
# Both halves are asserted on by the check below and they fail differently: a
# refusal that came back as an exit status with no message teaches an
# application author nothing, and a refusal reported in prose that exited zero
# is one no script can branch on.
tenant_sql() {
  local environment=$1
  local verb=$2
  local sql=$3
  local out="${LOG_DIR}/verify-sql-isolation.log"
  local rc=0
  case "$verb" in
    execute)
      deis_cli db execute -y -e "$environment" "$sql" --json > "$out" 2>&1 || rc=$?
      ;;
    *)
      deis_cli db query -e "$environment" "$sql" --json > "$out" 2>&1 || rc=$?
      ;;
  esac
  printf '%s %s\n' "$rc" "$(tr '\n' ' ' < "$out" | sed 's/  */ /g')"
}

check_a_tenants_sql_cannot_reach_another_tenants_tables_or_files() {
  local title="one tenant's SQL cannot read another tenant's tables, or name its file"
  local table="tenant_isolation_probe"
  local marker="only-${ENV_NAME}-may-read-this-$$-${RANDOM}"
  # Where deis-store keeps this tenant's file: <data_dir>/db/<tenant>.sqlite,
  # the layout crates/deis-store/src/database.rs documents. Spelled out here
  # because the whole point is that the other tenant names a path that really
  # exists and really holds the rows below - a refusal to open a file that was
  # not there would prove nothing.
  local victim_file="${DEMO_DIR}/deis-store-data/db/${APP_ID}.${ENV_NAME}.sqlite"
  local answer rc message

  answer=$(tenant_sql "$ENV_NAME" execute \
    "CREATE TABLE IF NOT EXISTS ${table} (marker TEXT)")
  if [ "${answer%% *}" != "0" ]; then
    fail_check "$title" "${ENV_NAME} could not create ${table}: ${answer#* }"
    return
  fi
  answer=$(tenant_sql "$ENV_NAME" execute \
    "INSERT INTO ${table} (marker) VALUES ('${marker}')")
  if [ "${answer%% *}" != "0" ]; then
    fail_check "$title" "${ENV_NAME} could not write its marker row: ${answer#* }"
    return
  fi

  answer=$(tenant_sql "$ENV_NAME" query \
    "SELECT marker FROM ${table} WHERE marker = '${marker}'")
  case "${answer#* }" in
    *"$marker"*) ;;
    *)
      fail_check "$title" \
        "${ENV_NAME} cannot read back its own row, so there is nothing for the other tenant to be kept away from: ${answer#* }"
      return
      ;;
  esac

  if [ ! -f "$victim_file" ]; then
    fail_check "$title" \
      "${ENV_NAME} wrote a row but ${victim_file} does not exist, so the path the other tenant names below is not the file holding it and every refusal after this would be about nothing"
    return
  fi

  # Now the other tenant. The same application, its other environment, which
  # this platform charges as a separate tenant - so this is the case an
  # application author actually meets, and not a contrived second app.
  local read_rc read_message attach_rc attach_message expr_rc expr_message
  local pragma_rc pragma_message vacuum_rc vacuum_message
  local copy="${DEMO_DIR}/sql-isolation-copy-$$.sqlite"

  answer=$(tenant_sql "$PREVIEW_ENV_NAME" query "SELECT marker FROM ${table}")
  read_rc=${answer%% *}; read_message=${answer#* }
  answer=$(tenant_sql "$PREVIEW_ENV_NAME" query \
    "ATTACH DATABASE '${victim_file}' AS victim")
  attach_rc=${answer%% *}; attach_message=${answer#* }
  # The same ATTACH with the filename built by an expression, which is the
  # spelling SQLite cannot hand to the authorizer: it arrives with the name
  # missing and has to be refused on the action alone.
  answer=$(tenant_sql "$PREVIEW_ENV_NAME" query \
    "ATTACH DATABASE ('${victim_file}' || '') AS victim")
  expr_rc=${answer%% *}; expr_message=${answer#* }
  # How a tenant would learn the path in the first place.
  answer=$(tenant_sql "$PREVIEW_ENV_NAME" query "PRAGMA database_list")
  pragma_rc=${answer%% *}; pragma_message=${answer#* }
  # And writing a file rather than reading one.
  answer=$(tenant_sql "$PREVIEW_ENV_NAME" execute "VACUUM INTO '${copy}'")
  vacuum_rc=${answer%% *}; vacuum_message=${answer#* }

  local copied=no
  [ -e "$copy" ] && copied=yes
  rm -f "$copy"

  if [ "$read_rc" = "0" ] || [ "$attach_rc" = "0" ] || [ "$expr_rc" = "0" ] \
    || [ "$pragma_rc" = "0" ] || [ "$vacuum_rc" = "0" ] || [ "$copied" = "yes" ]; then
    fail_check "$title" \
      "${APP_ID}.${PREVIEW_ENV_NAME} was allowed something it must not be: SELECT exited ${read_rc}, ATTACH ${attach_rc}, ATTACH from an expression ${expr_rc}, PRAGMA database_list ${pragma_rc}, VACUUM INTO ${vacuum_rc} (wrote a file: ${copied}). The SELECT said: $(printf '%s' "$read_message" | head -c 200)"
    return
  fi

  # The refusals also have to say which rule refused them. "database error"
  # on all five would be this same set of exit codes and would leave an
  # author with nothing to act on.
  case "$read_message" in
    *"no such table"*) ;;
    *)
      fail_check "$title" \
        "${PREVIEW_ENV_NAME} was refused the read, but not as a table it does not have: ${read_message}. It matters which: a tenant whose statement failed for some other reason may still be looking at the wrong file"
      return
      ;;
  esac
  local refusal
  for refusal in "$attach_message" "$expr_message"; do
    case "$refusal" in
      *ATTACH*) ;;
      *)
        fail_check "$title" \
          "an ATTACH from ${PREVIEW_ENV_NAME} was refused without naming ATTACH: ${refusal}"
        return
        ;;
    esac
  done
  case "$pragma_message" in
    *"PRAGMA database_list"*) ;;
    *)
      fail_check "$title" "PRAGMA database_list was refused without naming itself: ${pragma_message}"
      return
      ;;
  esac
  case "$vacuum_message" in
    *VACUUM*) ;;
    *)
      fail_check "$title" "VACUUM INTO was refused without naming VACUUM: ${vacuum_message}"
      return
      ;;
  esac

  pass_check "$title" \
    "${APP_ID}.${ENV_NAME} wrote a row only it should see into ${table}, and it is in ${victim_file} on this machine. ${APP_ID}.${PREVIEW_ENV_NAME} - the same application, the other environment, a separate tenant - selected from that table and was told \"$(printf '%s' "$read_message" | sed 's/^error: //' | head -c 90)\"; naming the file outright was refused (\"$(printf '%s' "$attach_message" | sed 's/^error: //' | head -c 120)\"), and so was the same ATTACH with the path built by an expression, PRAGMA database_list that would have told it the path, and VACUUM INTO, which wrote no file. Five statements, five refusals, each naming what it refused"
}

# ----------------------------------------------------------------------
# Object storage beyond put / get / delete
# ----------------------------------------------------------------------
#
# The four verbs an object store is not one without: a listing, a metadata
# read, a byte range and a conditional write. Each is checked twice over -
# once through the application, which is the only path that proves the JS
# binding is reachable, and once through `deis blob`, which is the only path
# that proves an operator can reach it. A verb that existed on the wire and in
# the client but was spelled in neither of those places would be a feature
# delivered to nobody.

# Objects the checks below share. Written through the application so that
# every later assertion is about data the binding really stored.
seed_blob_objects() {
  local headers="${LOG_DIR}/verify-blob-seed.headers"
  local key
  for key in "img/logo.png" "img/hero.png" "img/icons/star.svg" "index.html"; do
    runtime_curl -s -o /dev/null -D "$headers" -X PUT -H "x-deis-app-id: ${APP_ID}" \
      --data-binary "0123456789abcdef" \
      "$(runtime_data_url)${BLOB_ROUTE}?key=${key}&contentType=text/plain"
    if [ "$(http_status "$headers")" != "201" ]; then
      echo "$key"
      return 1
    fi
  done
  return 0
}

check_blob_listing() {
  local title="context.env.BLOB lists a page and rolls a folder up at a delimiter"
  local headers="${LOG_DIR}/verify-blob-list.headers"

  local failed
  if ! failed=$(seed_blob_objects); then
    fail_check "$title" "the application could not store ${failed}"
    return
  fi

  # Everything this check asserts on lives under one prefix it owns.
  #
  # It used to list the tenant's root and expect to find exactly
  # `index.html` there, which held only on a store nothing else had written
  # to - so a second `verify` against the same running demo failed it, the
  # conditional-write checks having left their own keys at the root by then.
  # A check that passes only on a fresh instance reads as a real defect to
  # whoever runs it twice, so the listing is scoped instead.
  local scope="listing/"
  local seeded
  for seeded in "index.html" "img/logo.png" "img/hero.png" "img/icons/star.svg"; do
    runtime_curl -s -o /dev/null -D "$headers" -X PUT -H "x-deis-app-id: ${APP_ID}" \
      --data-binary "0123456789abcdef" \
      "$(runtime_data_url)${BLOB_ROUTE}?key=${scope}${seeded}&contentType=text/plain"
    if [ "$(http_status "$headers")" != "201" ]; then
      fail_check "$title" "the application could not store ${scope}${seeded}"
      return
    fi
  done

  # A delimiter one level in: `listing/index.html` is a key here, and
  # everything under `listing/img/` is one folder rather than three keys.
  local top
  top=$(app_request "$headers" "${BLOB_ROUTE}?op=list&prefix=${scope}&delimiter=%2F")
  local keys prefixes
  keys=$(echo "$top" | jq -r '[.objects[].key] | join(",")' 2>/dev/null || true)
  prefixes=$(echo "$top" | jq -r '.prefixes | join(",")' 2>/dev/null || true)

  # One level down, one object per page, walked to the end the way the binding
  # documents: page until list_complete, not until a page comes back short.
  local cursor="" walked="" pages=0 complete="false"
  while [ "$complete" != "true" ] && [ "$pages" -lt 10 ]; do
    local page
    page=$(app_request "$headers" "${BLOB_ROUTE}?op=list&prefix=${scope}img/&limit=1&cursor=${cursor}")
    walked="${walked}$(echo "$page" | jq -r '[.objects[].key] | join(",")' 2>/dev/null || true),"
    complete=$(echo "$page" | jq -r '.list_complete' 2>/dev/null || echo false)
    cursor=$(echo "$page" | jq -r '.cursor // ""' 2>/dev/null || true)
    pages=$((pages + 1))
  done
  walked=$(echo "$walked" | tr -s ',' ',' | sed 's/,$//')

  local want_keys="${scope}index.html"
  local want_prefixes="${scope}img/"
  local want_walk="${scope}img/hero.png,${scope}img/icons/star.svg,${scope}img/logo.png"
  if [ "$keys" = "$want_keys" ] && [ "$prefixes" = "$want_prefixes" ] \
    && [ "$walked" = "$want_walk" ]; then
    pass_check "$title" \
      "list({ prefix: \"${scope}\", delimiter: \"/\" }) answered objects [${keys}] and prefixes [${prefixes}] - the three files under ${scope}img/ cost one entry, not three; paging that folder one object at a time took ${pages} pages and visited ${walked} in key order, each key exactly once"
  else
    fail_check "$title" \
      "expected objects [${want_keys}], prefixes [${want_prefixes}] and a walk of ${want_walk}; got objects [${keys:-<none>}], prefixes [${prefixes:-<none>}], walk [${walked:-<none>}] over ${pages} pages"
  fi
}

check_blob_head() {
  local title="context.env.BLOB reads an object's size and etag without its body"
  local headers="${LOG_DIR}/verify-blob-head.headers"

  local head
  head=$(app_request "$headers" "${BLOB_ROUTE}?op=head&key=index.html")
  local size etag type
  size=$(echo "$head" | jq -r '.size // empty' 2>/dev/null || true)
  etag=$(echo "$head" | jq -r '.etag // empty' 2>/dev/null || true)
  type=$(echo "$head" | jq -r '.contentType // empty' 2>/dev/null || true)

  local missing_status
  missing_status=$(runtime_curl -s -o /dev/null -w '%{http_code}' -H "x-deis-app-id: ${APP_ID}" \
    "$(runtime_data_url)${BLOB_ROUTE}?op=head&key=never-written" || true)

  # The etag has to be the object's sha256, because that is the value a
  # conditional write compares against; a made-up identity would still look
  # like a string here but would not condition anything. Computed here rather
  # than compared against itself, so this check would fail if the store ever
  # started answering some other identity under this name.
  if ! command -v openssl >/dev/null 2>&1; then
    fail_check "$title" \
      "openssl is not installed, so this check could not work out what the object's sha256 should be"
    return
  fi
  local expected
  expected=$(printf '0123456789abcdef' | openssl dgst -sha256 -hex 2>/dev/null | awk '{print $NF}')

  if [ "$size" = "16" ] && [ "$etag" = "$expected" ] && [ "$type" = "text/plain" ] \
    && [ "$missing_status" = "404" ]; then
    pass_check "$title" \
      "head(\"index.html\") answered size 16, content-type text/plain and etag ${etag}, which is the sha256 of the 16 bytes stored - the same value --if-match takes; head of a key that was never written answered HTTP ${missing_status} without moving a byte"
  else
    fail_check "$title" \
      "expected size 16, content-type text/plain, etag ${expected} and HTTP 404 for a missing key; got size '${size:-<none>}', type '${type:-<none>}', etag '${etag:-<none>}', missing-key HTTP ${missing_status}"
  fi
}

check_blob_range_read() {
  local title="an application forwards a Range header into blob.get and answers a correct 206"
  local headers="${LOG_DIR}/verify-blob-range.headers"

  # bytes=4-9 of "0123456789abcdef" is "456789".
  local body
  body=$(app_request "$headers" "${BLOB_ROUTE}?key=index.html" -H "range: bytes=4-9")
  local status range
  status=$(http_status "$headers")
  range=$(header_value "$headers" "content-range")

  # A suffix range, which is the form a resumed download sends.
  local suffix_headers="${LOG_DIR}/verify-blob-suffix.headers"
  local suffix
  suffix=$(app_request "$suffix_headers" "${BLOB_ROUTE}?key=index.html" -H "range: bytes=-4")
  local suffix_range
  suffix_range=$(header_value "$suffix_headers" "content-range")

  # A range that names no byte of the object. It has to arrive as something
  # other than "no such object", or the application cannot answer 416 where it
  # owes 416 and 404 where it owes 404.
  local unsatisfiable
  unsatisfiable=$(runtime_curl -s -o /dev/null -w '%{http_code}' -H "x-deis-app-id: ${APP_ID}" \
    -H "range: bytes=99-" \
    "$(runtime_data_url)${BLOB_ROUTE}?key=index.html" || true)
  local absent
  absent=$(runtime_curl -s -o /dev/null -w '%{http_code}' -H "x-deis-app-id: ${APP_ID}" \
    -H "range: bytes=0-1" \
    "$(runtime_data_url)${BLOB_ROUTE}?key=never-written" || true)

  if [ "$status" = "206" ] && [ "$body" = "456789" ] \
    && [ "$range" = "bytes 4-9/16" ] && [ "$suffix" = "cdef" ] \
    && [ "$suffix_range" = "bytes 12-15/16" ] \
    && [ "$unsatisfiable" = "416" ] && [ "$absent" = "404" ]; then
    pass_check "$title" \
      "GET with 'range: bytes=4-9' answered HTTP ${status} with body '${body}' and 'content-range: ${range}'; 'range: bytes=-4' answered '${suffix}' and '${suffix_range}', so a suffix range resolves against the real size; 'bytes=99-' on a 16-byte object answered HTTP ${unsatisfiable} while the same range on a key that does not exist answered HTTP ${absent} - the two failures an application has to tell apart"
  else
    fail_check "$title" \
      "expected 206/'456789'/'bytes 4-9/16', suffix 'cdef'/'bytes 12-15/16', 416 for an unsatisfiable range and 404 for a missing key; got ${status}/'${body}'/'${range:-<none>}', suffix '${suffix}'/'${suffix_range:-<none>}', unsatisfiable ${unsatisfiable}, missing ${absent}"
  fi
}

check_blob_conditional_write() {
  local title="two conditional writes of one key: one lands, the other is told it lost"
  local key="lock-$$"
  local headers="${LOG_DIR}/verify-blob-cas.headers"
  local url="$(runtime_data_url)${BLOB_ROUTE}"

  # Create-once. The second attempt must not overwrite, and must not look like
  # a transport failure either: the application answers 412 for exactly this.
  local first second
  first=$(runtime_curl -s -o /dev/null -w '%{http_code}' -X PUT -H "x-deis-app-id: ${APP_ID}" \
    --data-binary "held by the first writer" "${url}?key=${key}&ifNoneMatch=*" || true)
  second=$(runtime_curl -s -o "${LOG_DIR}/verify-blob-cas-second.json" -w '%{http_code}' \
    -X PUT -H "x-deis-app-id: ${APP_ID}" \
    --data-binary "held by the second writer" "${url}?key=${key}&ifNoneMatch=*" || true)

  local kept
  kept=$(app_request "$headers" "${BLOB_ROUTE}?key=${key}")

  # Compare-and-set on the etag: the current one writes, the superseded one
  # does not.
  local etag
  etag=$(app_request "$headers" "${BLOB_ROUTE}?op=head&key=${key}" | jq -r '.etag // empty' 2>/dev/null || true)
  local updated stale
  updated=$(runtime_curl -s -o /dev/null -w '%{http_code}' -X PUT -H "x-deis-app-id: ${APP_ID}" \
    --data-binary "second version" "${url}?key=${key}&ifMatch=${etag}" || true)
  stale=$(runtime_curl -s -o /dev/null -w '%{http_code}' -X PUT -H "x-deis-app-id: ${APP_ID}" \
    --data-binary "third version" "${url}?key=${key}&ifMatch=${etag}" || true)

  # And a conditional delete against the etag that is no longer current.
  local stale_delete
  stale_delete=$(runtime_curl -s -o /dev/null -w '%{http_code}' -X DELETE -H "x-deis-app-id: ${APP_ID}" \
    "${url}?key=${key}&ifMatch=${etag}" || true)

  local final
  final=$(app_request "$headers" "${BLOB_ROUTE}?key=${key}")

  if [ "$first" = "201" ] && [ "$second" = "412" ] \
    && [ "$kept" = "held by the first writer" ] \
    && [ "$updated" = "201" ] && [ "$stale" = "412" ] \
    && [ "$stale_delete" = "412" ] && [ "$final" = "second version" ]; then
    pass_check "$title" \
      "two PUTs of ${key} with onlyIf { ifNoneMatch: \"*\" } answered HTTP ${first} and ${second}, and the object still reads \"${kept}\" - the second writer did not overwrite; a PUT quoting the current etag ${etag:0:12} answered ${updated} while the same etag quoted again after it was superseded answered ${stale}, and a DELETE quoting it answered ${stale_delete}. The object is \"${final}\": every refused write left it alone"
  else
    fail_check "$title" \
      "expected 201 then 412 with the first writer's bytes kept, then 201/412/412 on the compare-and-set; got ${first}/${second} with '${kept}', then ${updated}/${stale}/${stale_delete} with '${final}'"
  fi
}

check_cli_blob_verbs() {
  local title="deis blob list, head and get --offset reach the same objects the app stored"
  local out="${LOG_DIR}/cli-blob"

  # `--binding BLOB` on every call, for the same reason `add_blob_route`
  # declares the binding: the handler stores through `context.env.BLOB`, and a
  # named binding is its own namespace inside the tenant. An operator who omits
  # the name is not looking at the application's objects, so the CLI has to be
  # given the name the application was given or this check would be comparing
  # two different namespaces and calling the difference a bug.
  if ! deis_cli blob list img/ --binding BLOB --json > "${out}-list.json" 2> "${out}-list.log"; then
    fail_check "$title" \
      "deis blob list failed: $(tail -n 3 "${out}-list.log" | tr '\n' ' ')"
    return
  fi
  if ! deis_cli blob head index.html --binding BLOB --json > "${out}-head.json" 2> "${out}-head.log"; then
    fail_check "$title" \
      "deis blob head failed: $(tail -n 3 "${out}-head.log" | tr '\n' ' ')"
    return
  fi
  if ! deis_cli blob get index.html --offset 4 --length 6 --binding BLOB --json \
    > "${out}-get.json" 2> "${out}-get.log"; then
    fail_check "$title" \
      "deis blob get --offset failed: $(tail -n 3 "${out}-get.log" | tr '\n' ' ')"
    return
  fi

  # The same listing with no binding named. It has to come back empty: that is
  # what makes the three answers above evidence that the CLI reached the
  # application's namespace rather than evidence that every namespace answers
  # the same. If this ever listed the seeded keys, a binding would not be an
  # isolation boundary at all.
  if ! deis_cli blob list img/ --json > "${out}-unnamed.json" 2> "${out}-unnamed.log"; then
    fail_check "$title" \
      "deis blob list without --binding failed: $(tail -n 3 "${out}-unnamed.log" | tr '\n' ' ')"
    return
  fi

  local listed folders headed_size headed_etag sliced offset length
  local namespace unnamed_namespace unnamed_count
  listed=$(jq -r '[.objects[].key] | join(",")' < "${out}-list.json" 2>/dev/null || true)
  folders=$(jq -r '.prefixes | join(",")' < "${out}-list.json" 2>/dev/null || true)
  # Read off the answer rather than assumed: the CLI prints the namespace it
  # presented to deis-store, and that string is the whole claim here.
  namespace=$(jq -r '.tenant // empty' < "${out}-list.json" 2>/dev/null || true)
  headed_size=$(jq -r '.size // empty' < "${out}-head.json" 2>/dev/null || true)
  headed_etag=$(jq -r '.sha256 // empty' < "${out}-head.json" 2>/dev/null || true)
  # A blob body is bytes, so --json carries it base64 for the reason
  # `deis kv get` does.
  sliced=$(jq -r '.value_base64 // empty' < "${out}-get.json" 2>/dev/null \
    | openssl base64 -d -A 2>/dev/null || true)
  offset=$(jq -r '.range.offset // empty' < "${out}-get.json" 2>/dev/null || true)
  length=$(jq -r '.range.length // empty' < "${out}-get.json" 2>/dev/null || true)
  unnamed_namespace=$(jq -r '.tenant // empty' < "${out}-unnamed.json" 2>/dev/null || true)
  unnamed_count=$(jq -r '.objects | length' < "${out}-unnamed.json" 2>/dev/null || true)

  if [ "$listed" = "img/hero.png,img/icons/star.svg,img/logo.png" ] \
    && [ "$namespace" = "${APP_ID}.${ENV_NAME}@BLOB" ] \
    && [ "$headed_size" = "16" ] && [ -n "$headed_etag" ] \
    && [ "$sliced" = "456789" ] && [ "$offset" = "4" ] && [ "$length" = "6" ] \
    && [ "$unnamed_namespace" = "${APP_ID}.${ENV_NAME}" ] && [ "$unnamed_count" = "0" ]; then
    pass_check "$title" \
      "\`deis blob list img/ --binding BLOB\` printed ${listed} (prefixes [${folders}]) and reported the namespace as ${namespace} - the one \`context.env.BLOB\` writes into; \`deis blob head index.html --binding BLOB\` reported 16 bytes and sha256 ${headed_etag:0:12}; \`deis blob get index.html --offset 4 --length 6 --binding BLOB\` returned '${sliced}' and reported range offset ${offset} length ${length}. Every one of these objects was stored by the application, not by the CLI. The same listing without --binding looks at ${unnamed_namespace} and finds ${unnamed_count} objects there, so the named namespace sits beside the unnamed one rather than on top of it"
  else
    fail_check "$title" \
      "expected the three img/ keys in namespace ${APP_ID}.${ENV_NAME}@BLOB, a 16-byte head, the slice '456789' at offset 4 length 6, and nothing under img/ in ${APP_ID}.${ENV_NAME}; got list [${listed:-<none>}] in namespace '${namespace:-<none>}', head size '${headed_size:-<none>}', slice '${sliced:-<none>}' at offset '${offset:-<none>}' length '${length:-<none>}', and ${unnamed_count:-<none>} objects in '${unnamed_namespace:-<none>}'"
  fi
}

check_cli_blob_conditional_write() {
  local title="deis blob put --if-match exits 3 when it loses, and stores nothing"
  local key="cli-lock-$$"
  local out="${LOG_DIR}/cli-blob-cas"

  # Create it exactly once. A second create-if-absent has to be refused with
  # the exit code a script can branch on, and it must not be the code an
  # unreachable store would produce.
  if ! printf 'first' | deis_cli blob put "$key" --if-none-match '*' --json \
    > "${out}-first.json" 2> "${out}-first.log"; then
    fail_check "$title" \
      "the first create failed: $(tail -n 3 "${out}-first.log" | tr '\n' ' ')"
    return
  fi
  local etag
  etag=$(jq -r '.sha256 // empty' < "${out}-first.json" 2>/dev/null || true)

  local second_code=0
  printf 'second' | deis_cli blob put "$key" --if-none-match '*' --json \
    > "${out}-second.json" 2> "${out}-second.log" || second_code=$?

  # And the same for a delete quoting an etag that is no longer current.
  printf 'updated' | deis_cli blob put "$key" --if-match "$etag" --json \
    > "${out}-update.json" 2> "${out}-update.log" || true
  local delete_code=0
  deis_cli blob delete "$key" -y --if-match "$etag" --json \
    > "${out}-delete.json" 2> "${out}-delete.log" || delete_code=$?

  local stored_now refused_etag
  stored_now=$(deis_cli blob get "$key" --json 2>/dev/null | jq -r '.value_base64 // empty' \
    | openssl base64 -d -A 2>/dev/null || true)
  refused_etag=$(jq -r '.etag // empty' < "${out}-second.json" 2>/dev/null || true)

  if [ "$second_code" = "3" ] && [ "$delete_code" = "3" ] \
    && [ "$stored_now" = "updated" ] && [ -n "$refused_etag" ]; then
    pass_check "$title" \
      "\`deis blob put ${key} --if-none-match '*'\` succeeded once and then exited ${second_code} - its own code, not the 1 an unreachable store gives - reporting the object is now ${refused_etag:0:12}; \`deis blob delete --if-match\` with the superseded etag also exited ${delete_code}, and the object still reads '${stored_now}'"
  else
    fail_check "$title" \
      "expected exit 3 from both the second create and the stale delete, with the object left reading 'updated'; got exit ${second_code} and ${delete_code}, object '${stored_now:-<none>}', reported etag '${refused_etag:-<none>}'"
  fi
}

# The checks this group asks, in the order a run asks them, and the
# heading `verify` prints before them.
group_storage_label="storage"
group_storage_checks=(
  check_kv_roundtrip
  # Named bindings. The isolation check seeds the key the CLI check reads, so
  # it runs first; the hostile-name check needs nothing of theirs and only
  # asks deis-store.
  check_named_bindings_are_separate_namespaces
  check_a_binding_cannot_reach_another_tenant
  check_cli_reads_a_named_binding
  check_storage_capability_token
  check_storage_token_route_requires_a_credential
  check_data_cli_reads_the_apps_write
  # What a tenant's SQL may cost, as opposed to what it may touch. Both go
  # through `deis db query`, which is the same Database service the
  # the declared database binding reaches.
  check_a_runaway_query_is_stopped_and_names_its_ceiling
  check_a_single_value_cannot_be_bigger_than_a_row_may_be
  # What a tenant's SQL may reach, as opposed to what it may spend. It writes
  # a table in production and reads for it from the preview, so it runs after
  # both cost checks and before anything that moves either environment.
  check_a_tenants_sql_cannot_reach_another_tenants_tables_or_files
  # Object storage past put / get / delete. The listing check seeds the
  # objects the four after it read, so it runs first.
  check_blob_listing
  check_blob_head
  check_blob_range_read
  check_blob_conditional_write
  check_cli_blob_verbs
  check_cli_blob_conditional_write
)
