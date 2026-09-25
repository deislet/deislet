# One group of `scripts/demo.sh verify`: the compatibility date and the flags
# an application names. Sourced by `scripts/demo.sh`.

# ----------------------------------------------------------------------
# Compatibility date and flags
# ----------------------------------------------------------------------
#
# An application says which day's platform it was written against, and the
# platform keeps giving it that day's answers. Without it, every fix that
# changes an observable behaviour is a breaking change for the whole fleet,
# delivered on a node restart nobody scheduled.
#
# The demo runs both sides at once, on one node, at the same moment:
# production is built from sources naming 2026-09-01 and the preview tree has
# the key deleted, which is what an application deployed before any of this
# existed looks like. The behaviour they differ on is real and was a defect:
# a response carrying two Set-Cookie headers used to arrive with one.

# Ask the cookie handler and count the Set-Cookie headers that came back.
set_cookie_count() {
  local environment=$1
  local headers="${LOG_DIR}/verify-cookies-${environment}.headers"
  env_request "$environment" "$headers" "$COOKIE_ROUTE" > /dev/null
  grep -ci '^set-cookie:' "$headers" || true
}

check_compatibility_date_is_visible() {
  local title="deis deploy and deis versions name the date an application runs under"
  local report="${LOG_DIR}/cli-deploy-v1.json"
  local json="${LOG_DIR}/cli-compat-versions.json"
  local text="${LOG_DIR}/cli-compat-versions.txt"

  if ! deis_cli versions --json > "$json" 2> "${LOG_DIR}/cli-compat-versions.log" \
    || ! deis_cli versions > "$text" 2>> "${LOG_DIR}/cli-compat-versions.log"; then
    fail_check "$title" \
      "deis versions failed: $(tail -n 3 "${LOG_DIR}/cli-compat-versions.log" | tr '\n' ' ')"
    return
  fi

  local deployed live_date preview_date column
  deployed=$(jq -r '.version.compatibility.date // empty' < "$report" 2>/dev/null || true)
  live_date=$(jq -r --arg env "$ENV_NAME" \
    '.versions[] | select(.environments | index($env)) | .compatibility.date // "none"' \
    < "$json" 2>/dev/null || true)
  preview_date=$(jq -r --arg env "$PREVIEW_ENV_NAME" \
    '.versions[] | select(.environments | index($env)) | .compatibility.date // "none"' \
    < "$json" 2>/dev/null || true)
  column=$(grep -c 'COMPAT' "$text" || true)

  if [ -n "$deployed" ] && [ "$live_date" = "$deployed" ] \
    && [ "$preview_date" = "none" ] && [ "${column:-0}" -ge 1 ] \
    && grep -q 'none (oldest)' "$text"; then
    pass_check "$title" \
      "the deploy report says the version it uploaded runs under ${deployed}; \`deis versions\` prints a COMPAT column where ${ENV_NAME}'s version reads ${live_date} and ${PREVIEW_ENV_NAME}'s reads \"none (oldest)\". Nobody had to open a bundle"
  else
    fail_check "$title" \
      "deploy report date '${deployed:-<none>}', ${ENV_NAME} version date '${live_date:-<none>}', ${PREVIEW_ENV_NAME} version date '${preview_date:-<none>}', COMPAT header found ${column:-0} time(s); the table was: $(tr '\n' '|' < "$text" | cut -c1-400)"
  fi
}

check_every_set_cookie_reaches_a_dated_application() {
  local title="an application written against 2026-09-01 gets every Set-Cookie it sets"
  local count
  count=$(set_cookie_count "$ENV_NAME")

  local date
  date=$(jq -r '.version.compatibility.date // empty' \
    < "${LOG_DIR}/cli-deploy-v1.json" 2>/dev/null || true)

  if [ "$count" = "2" ]; then
    pass_check "$title" \
      "GET ${COOKIE_ROUTE} on ${ENV_NAME} returned ${count} Set-Cookie headers - the session cookie and the CSRF cookie the handler set with headers.append - because this version's manifest names compatibility date ${date:-<none>}"
  else
    fail_check "$title" \
      "expected 2 Set-Cookie headers from ${ENV_NAME} (manifest date '${date:-<none>}'), got ${count}: $(tr -d '\r' < "${LOG_DIR}/verify-cookies-${ENV_NAME}.headers" | grep -i '^set-cookie' | tr '\n' ' ')"
  fi
}

# The half that is the whole point: the old answer survives for the
# application that was written against it, on the same node, in the same
# second, from the same handler source.
check_an_undated_application_keeps_the_old_answer() {
  local title="an application with no compatibility date still gets the answer it was written against"
  # Before the request, because the drop this reads is that request's.
  local mark
  mark=$(node_log_mark)

  local count
  count=$(set_cookie_count "$PREVIEW_ENV_NAME")

  # The node's own account of what it dropped and why, so this quotes the
  # node rather than this script's idea of it - and out of the lines the
  # request just made added, so the sentence cannot be one left behind by an
  # earlier run against the same node or written into the file from outside.
  local dropped
  dropped=$(node_log_since "$mark" \
    | grep -m 1 -F "App ${APP_ID}.${PREVIEW_ENV_NAME} set 2 Set-Cookie headers" || true)

  if [ "$count" = "1" ] && [ -n "$dropped" ]; then
    pass_check "$title" \
      "the same handler on ${PREVIEW_ENV_NAME}, whose manifest names no date, returned ${count} Set-Cookie header where ${ENV_NAME} returned 2. The node said so: \"$(echo "$dropped" | log_message | cut -c1-200)\""
  else
    fail_check "$title" \
      "expected exactly 1 Set-Cookie header from ${PREVIEW_ENV_NAME} and a node log line naming the drop; got ${count} header(s) and the line was $(if [ -n "$dropped" ]; then echo "present"; else echo "never written"; fi)"
  fi
}

# Upload a bundle whose manifest has been altered, straight to the control
# plane, and print the HTTP status.
#
# Past the CLI on purpose. Everything else in this demo goes through `deis`,
# but the claim here is about what the *control plane* refuses to store, and
# the CLI would have refused these bundles itself - which would have proved
# the wrong half. A fleet where one node is older than the date a version
# names is the case this protects, and the check that stops it is the one at
# upload.
upload_altered_bundle() {
  local label=$1
  local mutation=$2
  local source
  source=$(jq -r '.bundle.path // empty' < "${LOG_DIR}/cli-deploy-v1.json" 2>/dev/null || true)
  local patched="${DEMO_DIR}/${label}.zip"
  local body="${LOG_DIR}/${label}-upload.json"

  if [ ! -f "$source" ]; then
    echo "the deployed bundle path is absent or no longer exists" >&2
    return 1
  fi

  python3 - "$source" "$patched" "$mutation" <<'ALTER' || return 1
import json, sys, zipfile

source, target, mutation = sys.argv[1], sys.argv[2], sys.argv[3]
with zipfile.ZipFile(source) as archive:
    entries = [(info.filename, archive.read(info.filename)) for info in archive.infolist()]

with zipfile.ZipFile(target, "w", zipfile.ZIP_DEFLATED) as out:
    for name, data in entries:
        if name == "manifest.json":
            manifest = json.loads(data)
            exec(mutation, {"manifest": manifest})
            data = json.dumps(manifest).encode()
        out.writestr(name, data)
ALTER

  curl -s -o "$body" -w '%{http_code}' -X POST \
    -H "Authorization: Bearer ${OPERATOR_TOKEN}" \
    -H "Content-Type: application/zip" \
    --data-binary "@${patched}" \
    "$(control_url)/api/v1/apps/${APP_ID}/versions"
}

# Read whatever the control plane said about a refused upload.
refusal_message() {
  local file=$1
  local message
  message=$(jq -r '.error.message // empty' < "$file" 2>/dev/null || true)
  if [ -z "$message" ]; then
    message=$(head -c 300 "$file" 2>/dev/null || true)
  fi
  echo "$message"
}

check_a_date_this_platform_cannot_honour_is_refused() {
  local title="a bundle naming a compatibility date this build does not know is refused at upload"

  if ! command -v python3 >/dev/null 2>&1; then
    fail_check "$title" "python3 is needed to alter a bundle's manifest"
    return
  fi

  local status message
  status=$(upload_altered_bundle "future-date-bundle" \
    'manifest["compatibility"] = {"date": "2099-01-01"}')
  message=$(refusal_message "${LOG_DIR}/future-date-bundle-upload.json")

  if [ "$status" = "400" ] && echo "$message" | grep -q "2099-01-01"; then
    pass_check "$title" \
      "POST /api/v1/apps/${APP_ID}/versions with a manifest naming 2099-01-01 answered HTTP ${status}: \"$(echo "$message" | cut -c1-220)\". No version row was written, so no node was ever asked for semantics this build does not have"
  else
    fail_check "$title" \
      "expected HTTP 400 naming 2099-01-01, got HTTP ${status} and message '${message:-<empty>}'"
  fi
}

# The mirror of the check above, and the two are next to each other because
# the difference between them is the rule (`deis_bundle::forward`). A date this
# build cannot honour is refused: it asks for the whole set of answers as of a
# day nobody here has seen, and there is no honest subset of that to serve. A
# flag name this build does not know is accepted and ignored: it names one
# behaviour this build does not have in either direction, so ignoring it is
# exactly what this build was already doing.
#
# This used to expect the refusal, and that refusal was the rollback failure
# in miniature - a control plane one release behind the builder could not take,
# or later re-deliver, any version naming a flag added since, and the manifest
# is inside the archive whose digest is the version id, so nothing repaired
# one. The author's own misspelling is still refused, at the build.
check_an_unknown_compatibility_flag_is_carried_not_refused() {
  local title="a bundle naming a behaviour flag this build does not know is stored, not refused"

  if ! command -v python3 >/dev/null 2>&1; then
    fail_check "$title" "python3 is needed to alter a bundle's manifest"
    return
  fi

  local status message
  status=$(upload_altered_bundle "unknown-flag-bundle" \
    'manifest["compatibility"] = {"date": "2026-09-01", "flags": ["cookies_but_faster"]}')
  message=$(refusal_message "${LOG_DIR}/unknown-flag-bundle-upload.json")

  local stored_date
  stored_date=$(jq -r '.version.manifest.compatibility.date // empty' \
    < "${LOG_DIR}/unknown-flag-bundle-upload.json" 2>/dev/null || true)

  if [ "$status" = "201" ] && [ "$stored_date" = "2026-09-01" ]; then
    pass_check "$title" \
      "POST /api/v1/apps/${APP_ID}/versions with the flag \"cookies_but_faster\" answered HTTP ${status} and stored the version with its date 2026-09-01 intact. The flag names a behaviour this build does not have in either direction, so it is ignored and named at warn rather than costing the whole version - which is what a control plane one release behind a builder has to be able to do"
  else
    fail_check "$title" \
      "expected HTTP 201 with the compatibility date carried through, got HTTP ${status}, date '${stored_date:-<none>}' and message '${message:-<empty>}'"
  fi
}

# The checks this group asks, in the order a run asks them, and the
# heading `verify` prints before them.
group_compatibility_label="compatibility date"
group_compatibility_checks=(
  # Both halves need production on v1, whose sources name a date, and the
  # preview on v2, whose staged copy has the key deleted. They run before the
  # rollback for the same reason the limit checks do.
  check_compatibility_date_is_visible
  check_every_set_cookie_reaches_a_dated_application
  check_an_undated_application_keeps_the_old_answer
  check_a_date_this_platform_cannot_honour_is_refused
  check_an_unknown_compatibility_flag_is_carried_not_refused
)
