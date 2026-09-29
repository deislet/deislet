# One group of `scripts/demo.sh verify`: what a version's static site says
# about itself, and the one binding name an application may not declare.
# Sourced by `scripts/demo.sh`.

check_static_asset() {
  local headers="${LOG_DIR}/verify-asset.headers"
  local body
  body=$(app_request "$headers" "/")

  local status etag ctype
  status=$(http_status "$headers")
  etag=$(header_value "$headers" ETag)
  ctype=$(header_value "$headers" Content-Type)

  if [ "$status" != "200" ] || ! echo "$body" | grep -q "static asset"; then
    fail_check "static asset served from the bundle" "GET / answered ${status}"
    return
  fi
  if [ -z "$etag" ]; then
    fail_check "static asset carries an ETag" "no ETag header on GET /"
    return
  fi
  pass_check "static asset served from the bundle" \
    "HTTP ${status}, content-type ${ctype}, ETag ${etag}; the body is the packaged file, not a handler's answer"
}

# The conditional request, kept apart from the check above so that each entry in
# the group array prints exactly one verdict: the roll call compares the number
# of registered checks with the number of verdicts a complete run printed.
check_static_asset_conditional() {
  local headers="${LOG_DIR}/verify-asset.headers"
  local etag
  etag=$(header_value "$headers" ETag)
  if [ -z "$etag" ]; then
    fail_check "If-None-Match on that ETag answers 304 with no body" \
      "no ETag header on GET /"
    return
  fi

  local code
  code=$(runtime_curl -s -o /dev/null -w '%{http_code}' \
    -H "x-deis-app-id: ${APP_ID}" -H "If-None-Match: ${etag}" \
    "$(runtime_data_url)/" || true)
  local length
  length=$(runtime_curl -s -o /dev/null -w '%{size_download}' \
    -H "x-deis-app-id: ${APP_ID}" -H "If-None-Match: ${etag}" \
    "$(runtime_data_url)/" || true)

  if [ "$code" = "304" ] && [ "$length" = "0" ]; then
    pass_check "If-None-Match on that ETag answers 304 with no body" \
      "HTTP 304, ${length} bytes"
  else
    fail_check "If-None-Match on that ETag answers 304 with no body" \
      "got HTTP ${code} with ${length} bytes"
  fi
}

# ----------------------------------------------------------------------
# What a version's static site says about itself
# ----------------------------------------------------------------------
#
# `_redirects`, `_headers` and the not-found behaviour are three files' worth
# of behaviour an author gets without writing a function. They are parsed when
# the bundle is built, travel in the manifest, are compiled once when the
# version loads, and are applied by the HTTP entry before an isolate is
# started - so every check below is answered without one.
#
# The example app ships all three (scripts/demo-assets/example-app/public).
# Nothing else in this file would notice if the entry went back to a plain
# index lookup: every other asset check asks for a path that names a real
# file, which a lookup answers on its own.

check_site_redirect() {
  local title="a _redirects rule answers with a redirect, and the query rides along"
  local headers="${LOG_DIR}/verify-site-redirect.headers"
  # A query the target does not carry one of its own, so what arrives at the
  # target can only have come from the request. A rule that dropped it would
  # land a campaign link on the new page with the tracking the author was
  # counting gone, and the redirect would still look correct.
  local query="ref=demo-campaign"

  app_request "$headers" "${SITE_OLD_PATH}?${query}" > /dev/null

  local status location
  status=$(http_status "$headers")
  location=$(header_value "$headers" Location)

  local expected="${SITE_REDIRECT_TARGET}?${query}"
  if [ "$status" = "301" ] && [ "$location" = "$expected" ]; then
    pass_check "$title" \
      "GET ${SITE_OLD_PATH}?${query} answered HTTP ${status} with location: ${location}, without starting an isolate. 301 and not the 302 a two-field rule defaults to, because the rule in _redirects carries a third field"
  else
    fail_check "$title" \
      "GET ${SITE_OLD_PATH}?${query} answered HTTP ${status} with location '${location:-<none>}'; expected HTTP 301 to ${expected}"
  fi
}

check_site_headers() {
  local title="a _headers block puts its headers on the asset it names and on nothing else"
  local styled="${LOG_DIR}/verify-site-headers-css.headers"
  local plain="${LOG_DIR}/verify-site-headers-root.headers"

  app_request "$styled" "$SITE_STYLESHEET" > /dev/null
  app_request "$plain" "/" > /dev/null

  local styled_cache styled_referrer plain_cache plain_referrer
  styled_cache=$(header_value "$styled" Cache-Control)
  styled_referrer=$(header_value "$styled" Referrer-Policy)
  plain_cache=$(header_value "$plain" Cache-Control)
  plain_referrer=$(header_value "$plain" Referrer-Policy)

  # Two files, not one. A node that applied the block to every response would
  # put the year-long Cache-Control and the Referrer-Policy on the HTML too,
  # and asking only about the stylesheet could not tell that apart from the
  # block matching the path it names.
  #
  # The file carries one block. It used to carry two - a `/*` block with the
  # Referrer-Policy above the `/css/*` block - and this check used to read
  # the Referrer-Policy on the stylesheet as proof that both matching blocks
  # were composed. The locked Edge Canon standard lowers `_headers` with
  # first-match semantics, and a file with more than one block is refused at
  # build (EC_ROUTING_SOURCE_UNREPRESENTABLE), so composition is no longer
  # something an application can be promised. What is promised is the block:
  # both of its headers on the path it names, neither anywhere else.
  if [ "$styled_cache" = "$SITE_STYLESHEET_CACHE_CONTROL" ] \
    && [ "$styled_referrer" = "$SITE_STYLESHEET_REFERRER_POLICY" ] \
    && [ "$plain_cache" != "$SITE_STYLESHEET_CACHE_CONTROL" ] \
    && [ -z "$plain_referrer" ]; then
    pass_check "$title" \
      "GET ${SITE_STYLESHEET} carries cache-control: ${styled_cache} and referrer-policy: ${styled_referrer}, both from the one /css/* block; GET / matches no block, so it carries no referrer-policy and keeps the node's own cache-control: ${plain_cache}. One block and not the /* block this file used to open with: the locked standard lowers _headers first-match, and a bundle with two blocks is refused at build (EC_ROUTING_SOURCE_UNREPRESENTABLE) rather than have one target compose them and another not"
  else
    fail_check "$title" \
      "expected ${SITE_STYLESHEET} to carry cache-control: ${SITE_STYLESHEET_CACHE_CONTROL} and referrer-policy: ${SITE_STYLESHEET_REFERRER_POLICY}, and / to carry no referrer-policy and keep the node's default cache-control; got '${styled_cache:-<none>}' and '${styled_referrer:-<none>}' on the stylesheet, '${plain_cache:-<none>}' and '${plain_referrer:-<none>}' on /"
  fi
}

check_site_not_found() {
  local title="a path naming no file falls back the way the app configured"
  local headers="${LOG_DIR}/verify-site-404.headers"
  local body
  body=$(app_request "$headers" "$SITE_MISSING_PATH")

  local status
  status=$(http_status "$headers")

  if [ "$status" = "404" ] && echo "$body" | grep -q "$SITE_NOT_FOUND_MARKER"; then
    pass_check "$title" \
      "GET ${SITE_MISSING_PATH} answered HTTP ${status} with the bundle's own 404.html - it carries ${SITE_NOT_FOUND_MARKER}, which nothing else does. The project asks for \"notFound\": \"404-page\", so that page is served instead of the isolate's plain \"Not Found\"; a path a function route claims is still handed to the isolate, which is why /hello/world answers from the handler and not from here"
  else
    fail_check "$title" \
      "GET ${SITE_MISSING_PATH} answered HTTP ${status} with a body that does not carry ${SITE_NOT_FOUND_MARKER}: $(echo "$body" | tr '\n' ' ' | head -c 200)"
  fi
}

# ----------------------------------------------------------------------
# env.ASSETS
# ----------------------------------------------------------------------
#
# The other half of asset-first: the entry answers a request for a file
# before an isolate exists, and this is the way back in for the handler that
# ran anyway and wants the asset layer's answer for a path of its own.
#
# functions/api/asset.ts reports what `context.env.ASSETS` actually is as well
# as what it answered, which is what lets the second check below tell a
# working binding from a string the application put there.

check_assets_binding() {
  local title="env.ASSETS.fetch() answers a handler with this version's own file"
  local body
  body=$(app_request "${LOG_DIR}/verify-assets-binding.headers" \
    "/api/asset?path=${SITE_STYLESHEET}")
  answered_json "$title" assets-binding "$body" || return 0

  local status ctype got reported_bytes
  status=$(echo "$body" | jq -r '.status // empty' 2>/dev/null || true)
  ctype=$(echo "$body" | jq -r '.contentType // empty' 2>/dev/null || true)
  got=$(echo "$body" | jq -r '.body // empty' 2>/dev/null || true)
  # What the handler measured on its side of the boundary, which is the one
  # number a shell cannot fake: `$(...)` eats a trailing newline, so a
  # comparison of the text alone would accept an answer one byte short.
  reported_bytes=$(echo "$body" | jq -r '.bytes // empty' 2>/dev/null || true)

  # The bytes on disk, from the tree this version was built from. Comparing
  # digests rather than eyeballing a prefix: a binding that answered with the
  # right content type and half the file would pass anything looser.
  local file="${DEMO_DIR}/app-v1/public${SITE_STYLESHEET}"
  local expected want_digest got_digest on_disk_bytes
  expected=$(cat "$file" 2>/dev/null || true)
  on_disk_bytes=$(wc -c < "$file" 2>/dev/null | tr -d ' ' || true)
  want_digest=$(printf '%s' "$expected" | openssl dgst -sha256 | awk '{print $NF}' | cut -c1-12)
  got_digest=$(printf '%s' "$got" | openssl dgst -sha256 | awk '{print $NF}' | cut -c1-12)

  if [ -z "$expected" ]; then
    fail_check "$title" \
      "the staged source tree has no ${file}, so there is nothing to compare the binding's answer against"
    return
  fi

  if [ "$status" = "200" ] && [ "$got" = "$expected" ] \
    && [ "$reported_bytes" = "$on_disk_bytes" ]; then
    pass_check "$title" \
      "the handler called env.ASSETS.fetch(\"${SITE_STYLESHEET}\") and got HTTP ${status}, content-type ${ctype} and ${reported_bytes} bytes - the size of ${file} on disk - whose text has sha256 ${got_digest}, the digest of that file. Nothing was fetched over the network: those bytes have been in the node's memory since the version loaded"
  else
    fail_check "$title" \
      "env.ASSETS.fetch(\"${SITE_STYLESHEET}\") answered HTTP ${status:-<none>} with ${reported_bytes:-0} bytes and sha256 ${got_digest:-<none>}; expected 200 with ${on_disk_bytes:-0} bytes and ${want_digest} (the packaged file)"
  fi
}

# The application the check below tries, and fails, to deploy. A name of its
# own, and a project of its own, so that the refusal cannot cost the example
# app its deployment and nothing else in this run reads what it left behind.
RESERVED_ENV_APP_ID="${RESERVED_ENV_APP_ID:-demo-reserved-env}"

# Lay down a project that writes `ASSETS` into its own environment, and echo
# the directory.
#
# Until the reserved-name rule reached the environment declarations, this was
# the example app itself: it shipped a variable called ASSETS, the node lifted
# it back out of the map, and the check below read the node's log to prove it
# had. That was the weaker claim. A variable of that name is now refused where
# it is written - the compiler will not mint a declaration for a name the
# platform publishes - so an author is told before anything is built, and the
# demo makes the earlier claim instead.
#
# Otherwise the smallest project the compiler accepts, so that nothing else in
# it can be what was refused.
prepare_reserved_env_source() {
  local dest="${DEMO_DIR}/app-reserved-env"

  rm -rf "$dest"
  mkdir -p "${dest}/functions/api"

  cat > "${dest}/.config.json" <<JSON
{
  "version": "1.0.0",
  "name": "${RESERVED_ENV_APP_ID}",
  "runtime": "standard-v1",
  "language": "typescript",
  "functionRoot": "./functions",
  "compatibilityDate": "2026-09-01",
  "environment": {
    "declarations": [
      {"name": "ASSETS", "kind": "config", "valueType": "string", "required": true}
    ],
    "values": {
      "ASSETS": "an-application-cannot-take-this-name"
    }
  },
  "vendors": { "deislet": { "enabled": true } },
  "build": { "outDir": "./dist" }
}
JSON

  cat > "${dest}/functions/api/asset.ts" <<'TS'
// A handler that reads the name it declared a value under. The value is the
// point: if the project could be built, this would answer with the string,
// and `env.ASSETS.fetch()` in any other handler of the same application would
// throw "fetch is not a function" instead of reaching the version's files.
export default async function handler(context: any): Promise<Response> {
  return new Response(String(context.env.ASSETS) + "\n", {
    headers: { "content-type": "text/plain; charset=utf-8" },
  });
}
TS

  echo "$dest"
}

check_the_assets_name_cannot_be_declared_by_an_application() {
  local title="an application declaring ASSETS in its own env is refused by the build, and a legal one gets the platform's binding"
  local json="${LOG_DIR}/cli-deploy-reserved-env.json"
  local errors="${LOG_DIR}/cli-deploy-reserved-env.log"

  local dir
  if ! dir=$(prepare_reserved_env_source); then
    fail_check "$title" "the project could not be staged under ${DEMO_DIR}"
    return
  fi

  # Through `deis deploy` and not `deis build`, for the reason the WebSocket
  # refusal spells out: the claim is about what a person who tried to ship
  # this would meet, and the deploy's first step is the build.
  local status=0
  (cd "$dir" && deis_cli deploy -e "$ENV_NAME" --app "$RESERVED_ENV_APP_ID" --json) \
    > "$json" 2> "$errors" || status=$?

  local message
  message=$(grep -a -m 1 -o 'EC_ENV_NAMESPACE_COLLISION.*' "$errors" 2>/dev/null || true)

  # What was left behind, on both sides: a bundle under the project, or an
  # application in the catalog. The list is walked whole, so an application on
  # a later page could not hide from this.
  local built="no"
  [ -e "${dir}/dist" ] && built="yes"
  local listed
  listed=$(control_list_all /api/v1/apps \
    | jq -r --arg id "$RESERVED_ENV_APP_ID" '[.[] | select(.app_id == $id)] | length' 2>/dev/null || true)

  # The other half of the same claim, from the application that *was* built:
  # the name the refusal protects holds the platform's object, and no value of
  # the application's is visible under it. Without this half the check would
  # only say the platform refuses a name, not that it uses it.
  local body kind callable leaked
  body=$(app_request "${LOG_DIR}/verify-assets-shadow.headers" "/api/asset")
  answered_json "$title" assets-shadow "$body" || return 0
  kind=$(echo "$body" | jq -r '.bindingKind // empty' 2>/dev/null || true)
  callable=$(echo "$body" | jq -r '.bindingIsCallable // false' 2>/dev/null || true)
  leaked=$(echo "$body" | jq -r '.declaredValueVisible // empty' 2>/dev/null || true)

  if [ "$status" -ne 0 ] && echo "$message" | grep -q "ASSETS" \
    && [ "$built" = "no" ] && [ "${listed:-x}" = "0" ] \
    && [ "$kind" = "object" ] && [ "$callable" = "true" ] && [ -z "$leaked" ]; then
    pass_check "$title" \
      "deis deploy --app ${RESERVED_ENV_APP_ID} exited ${status} at its first step, the build: \"$(echo "$message" | cut -c1-200)\". No dist/ was written under the project and the control plane lists no application called ${RESERVED_ENV_APP_ID}. Meanwhile ${APP_ID}, which declares no such name, sees env.ASSETS as an ${kind} whose fetch() is a function. The platform owns this name at both ends: an application cannot take it, and what a handler finds under it is the platform's own binding"
  else
    fail_check "$title" \
      "expected a non-zero exit naming EC_ENV_NAMESPACE_COLLISION and ASSETS, no dist/ and no application in the catalog, and env.ASSETS on ${APP_ID} an object with a callable fetch; got exit ${status}, message '${message:-<none>}', dist written: ${built}, applications called ${RESERVED_ENV_APP_ID}: ${listed:-<unreadable>}, env.ASSETS a ${kind:-<none>} (callable: ${callable}) with the application's own value $(if [ -n "$leaked" ]; then echo "visible as '${leaked}'"; else echo "not visible"; fi). The CLI said: $(tail -n 3 "$errors" | tr '\n' ' ' | head -c 300)"
  fi
}

# The checks this group asks, in the order a run asks them, and the
# heading `verify` prints before them.
group_assets_label="asset-first serving"
group_assets_checks=(
  check_static_asset
  check_static_asset_conditional
  # The three files a static site configures itself with, and the way back
  # into the asset layer from inside a function. All four run before the
  # rollback moves production off v1, which is the version whose bundle
  # carries them.
  check_site_redirect
  check_site_headers
  check_site_not_found
  check_assets_binding
  check_the_assets_name_cannot_be_declared_by_an_application
)
