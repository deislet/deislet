# One group of `scripts/demo.sh verify`: deis-proxy choosing an application by
# host and path prefix. Sourced by `scripts/demo.sh`.

check_route_parameter() {
  local headers="${LOG_DIR}/verify-route.headers"
  local body
  body=$(app_request "$headers" "/hello/world")

  local greeting
  greeting=$(echo "$body" | jq -r '.greeting // empty' 2>/dev/null || true)
  if [ "$greeting" = "Hello, world!" ]; then
    pass_check "a route placeholder reaches context.params" \
      "GET /hello/world -> ${body}"
  else
    fail_check "a route placeholder reaches context.params" \
      "GET /hello/world -> $(http_status "$headers") ${body}"
  fi
}

# ----------------------------------------------------------------------
# The proxy choosing an application by host *and* path prefix
# ----------------------------------------------------------------------
#
# The README puts "host and path prefix" in bold, and until this check nothing
# exercised the second half. Every request the demo sends through deis-proxy
# reaches an environment's automatic address under `routing.preview_zone`, or
# a host a catalog route claims - a host, and a path prefix where one is
# claimed. No request header names an application to the edge: the host is
# the whole of the choice. A proxy that ignored `path_prefix` entirely, or
# matched it as a plain string prefix, left all of that green.
#
# The catalog is what this reads from, and it is reached with curl rather than
# with `deis`: routes are an operator's object and the CLI has no verb for one
# (the same reason `upload_altered_bundle` above talks to the control plane
# directly). What is under test is the proxy, not the command.

# A host two applications are bound to here, one of them under a path prefix
# of the other.
#
# One label on purpose. deis-proxy orders a certificate for every host in the
# routing table, and `is_certifiable_name` in crates/deis-proxy/src/acme/names.rs
# leaves out anything that is not a public host name - a single label among
# them, since there is no registered domain for an authority to check. So this
# name routes without adding an order at the stub authority, which is what the
# certificate checks below count.
ROUTED_HOST="${ROUTED_HOST:-deislet-edge}"
# The prefix the second application answers under, and a path that begins with
# the same letters while being outside it. `/verifyx` is the case a plain
# string prefix would swallow.
ROUTED_PREFIX="/verify"
ROUTED_NEAR_MISS="/verifyx"

# Grant one application the right to claim a hostname, unless the registry
# already says so.
#
# A hostname is global on this platform: a route claiming one is refused
# unless a domain grant covers it, whoever the caller is (the refusal itself
# is exercised by deis-control's own API tests, which drive the hijack
# reproduction end to end). So this demo grants exactly the one name its
# routing checks claim, to exactly the two applications that claim it, and
# nothing wider. Both grants are equal-specificity peers on one name; the
# (host, path prefix) uniqueness rule is what keeps their two routes apart.
ensure_domain_grant() {
  local pattern=$1 app=$2
  local existing
  # The whole registry, not its first page: a grant that sits past the
  # hundredth row would otherwise be granted again, and the 409 that answers
  # a duplicate would be read by the routing checks as a refusal.
  existing=$(control_list_all /api/v1/domains \
    | jq -r --arg pattern "$pattern" --arg app "$app" \
      '[.[] | select(.pattern == $pattern and .app_id == $app)] | length' \
    2>/dev/null || echo 0)
  if [ "${existing:-0}" -ge 1 ]; then
    return 0
  fi
  curl -s -o /dev/null -w '%{http_code}' -X POST \
    -H "Authorization: Bearer ${OPERATOR_TOKEN}" -H 'content-type: application/json' \
    -d "{\"pattern\":\"${pattern}\",\"app_id\":\"${app}\"}" \
    "$(control_url)/api/v1/domains" || true
}

# Create one route unless the catalog already has it.
#
# `verify` can be run twice against one demo and the catalog accepts duplicate
# routes without complaint, so a second run would otherwise pile up rules that
# say the same thing.
ensure_catalog_route() {
  local app=$1 environment=$2 host=$3 prefix=$4
  local existing
  existing=$(curl -s -H "Authorization: Bearer ${OPERATOR_TOKEN}" "$(control_url)/api/platform/routes" \
    | jq -r --arg app "$app" --arg host "$host" --arg prefix "$prefix" \
      '[.routes[] | select(.app_id == $app and .host == $host and .path_prefix == $prefix)] | length' \
    2>/dev/null || echo 0)
  if [ "${existing:-0}" -ge 1 ]; then
    return 0
  fi
  curl -s -o /dev/null -w '%{http_code}' -X POST \
    -H "Authorization: Bearer ${OPERATOR_TOKEN}" -H 'content-type: application/json' \
    -d "{\"host\":\"${host}\",\"path_prefix\":\"${prefix}\",\"priority\":0}" \
    "$(control_url)/api/v1/apps/${app}/environments/${environment}/routes" || true
}

check_the_proxy_routes_by_host_and_path_prefix() {
  local title="deis-proxy picks the application by host and path prefix, not by host alone"
  local created_root created_prefix

  local granted_root granted_prefix
  granted_root=$(ensure_domain_grant "$ROUTED_HOST" "$APP_ID")
  granted_prefix=$(ensure_domain_grant "$ROUTED_HOST" "$CALLEE_APP_ID")
  case "${granted_root}${granted_prefix}" in
    *4[0-9][0-9]*|*5[0-9][0-9]*)
      fail_check "$title" \
        "granting ${ROUTED_HOST} to the demo applications failed (${granted_root}/${granted_prefix}), so their routes cannot be claimed"
      return
      ;;
  esac
  created_root=$(ensure_catalog_route "$APP_ID" "$ENV_NAME" "$ROUTED_HOST" "/")
  created_prefix=$(ensure_catalog_route "$CALLEE_APP_ID" "$ENV_NAME" "$ROUTED_HOST" "$ROUTED_PREFIX")
  case "${created_root}${created_prefix}" in
    *4[0-9][0-9]*|*5[0-9][0-9]*)
      fail_check "$title" \
        "the catalog refused a route: / -> ${APP_ID} answered '${created_root}', ${ROUTED_PREFIX} -> ${CALLEE_APP_ID} answered '${created_prefix}'"
      return
      ;;
  esac

  # The proxy pulls the routing table on its own schedule, so the condition is
  # polled rather than waited out: what ends this loop is the prefix route
  # answering, which is the same fact the check is about.
  local headers="${LOG_DIR}/verify-routed.headers"
  local rounds=30 body=""
  while [ "$rounds" -gt 0 ]; do
    body=$(routed_request "$ROUTED_HOST" "$ROUTED_PREFIX" "$headers")
    case "$body" in
      *'"handlerOf"'*) break ;;
    esac
    rounds=$((rounds - 1))
    poll_pause
  done

  local prefix_status prefix_handler
  prefix_status=$(http_status "$headers")
  prefix_handler=$(echo "$body" | jq -r '.handlerOf // empty' 2>/dev/null || true)
  if [ "$prefix_handler" != "verify" ]; then
    fail_check "$title" \
      "http://${ROUTED_HOST}${ROUTED_PREFIX} through deis-proxy did not reach ${CALLEE_APP_ID}: HTTP ${prefix_status:-none}, body $(printf '%s' "$body" | tr '\n' ' ' | head -c 200)"
    return
  fi

  # The same host, a path the prefix does not cover: the other application.
  local root_body root_label
  root_body=$(routed_request "$ROUTED_HOST" "/hello/routed" "${LOG_DIR}/verify-routed-root.headers")
  root_label=$(echo "$root_body" | jq -r '.label // empty' 2>/dev/null || true)

  # A path that starts with the same letters and is not inside the prefix. It
  # belongs to the `/` route, so the answer is the other application's own
  # not-found page - which no other application on this demo serves.
  local near_body near_status
  near_body=$(routed_request "$ROUTED_HOST" "$ROUTED_NEAR_MISS" "${LOG_DIR}/verify-routed-near.headers")
  near_status=$(http_status "${LOG_DIR}/verify-routed-near.headers")

  # And a host nothing claims, so that "the host decided it" is observed and
  # not assumed: without this the two answers above would also be produced by
  # a proxy that routed on the path alone.
  local stranger_status
  stranger_status=$(curl -s -o /dev/null -w '%{http_code}' -m 10 --noproxy '*' \
    -H "Host: nothing-claims-this" "http://127.0.0.1:${DEIS_PROXY_PORT}${ROUTED_PREFIX}" || true)

  if [ "$root_label" != "v1" ]; then
    fail_check "$title" \
      "${ROUTED_PREFIX} reached ${CALLEE_APP_ID}, but /hello/routed on the same host did not reach ${APP_ID}: $(printf '%s' "$root_body" | tr '\n' ' ' | head -c 200)"
    return
  fi
  if [ "$near_status" != "404" ] || ! echo "$near_body" | grep -q "$SITE_NOT_FOUND_MARKER"; then
    fail_check "$title" \
      "${ROUTED_NEAR_MISS} answered HTTP ${near_status} without ${APP_ID}'s own not-found page, so the ${ROUTED_PREFIX} route is matching on letters rather than on path segments and has captured a path that is not under it: $(printf '%s' "$near_body" | tr '\n' ' ' | head -c 160)"
    return
  fi
  if [ "$stranger_status" != "404" ]; then
    fail_check "$title" \
      "${ROUTED_PREFIX} on a host no route claims answered HTTP ${stranger_status} rather than 404, so the host is not part of the decision and these routes are answering for every name that reaches this proxy"
    return
  fi

  pass_check "$title" \
    "two routes on the one host ${ROUTED_HOST}: / to ${APP_ID}/${ENV_NAME} and ${ROUTED_PREFIX} to ${CALLEE_APP_ID}/${ENV_NAME}. With no x-deis-app-id on any of them, GET ${ROUTED_PREFIX} answered HTTP ${prefix_status} from ${CALLEE_APP_ID}'s own handler (handlerOf=${prefix_handler}), GET /hello/routed answered from ${APP_ID} version ${root_label}, GET ${ROUTED_NEAR_MISS} answered ${APP_ID}'s 404 page because a prefix covers whole path segments and not the letters they start with, and the same ${ROUTED_PREFIX} on a host nothing claims answered HTTP ${stranger_status}"
}

# The checks this group asks, in the order a run asks them, and the
# heading `verify` prints before them.
group_routing_label="routing"
group_routing_checks=(
  check_route_parameter
  # Through deis-proxy and with no x-deis-app-id header, which is the only
  # arrangement in which a route decides anything. It leaves two rules in the
  # catalog on a host nothing else here uses.
  check_the_proxy_routes_by_host_and_path_prefix
)
