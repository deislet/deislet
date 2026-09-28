#!/usr/bin/env bash
# A stand-in for wrangler 4.129.0 used by the driver's integration tests.
#
# It keeps a tiny Cloudflare account in files under $STATE, answers the
# commands the driver issues with wrangler's real output shapes (JSON on
# stdout for list/--json commands, ND-JSON records in
# $WRANGLER_OUTPUT_FILE_PATH, `[code: N]` in error text, non-zero exit on
# failure), and records every invocation.
#
# It mirrors the parts of wrangler 4.129.0's behaviour the driver depends on
# (wrangler-dist/cli.js):
#
#   * `versions upload` refuses a Worker that does not exist yet, with the
#     code-less user error of preUploadApiChecks; only `deploy` creates one;
#   * `versions upload` and `versions deploy` never touch cron schedules,
#     queue consumers or the workers.dev route; only `deploy` and
#     `triggers deploy` do (triggersDeploy: PUT schedules when `[triggers]
#     crons` is present, updateQueueConsumers, subdomainDeploy);
#   * version ids are UUIDs, and `versions deploy` refuses anything else;
#   * `deployments status` of a Worker that was never deployed is the
#     code-less user error "The Worker <name> has no deployments.".
#
# Account state:
#
#   $STATE/workers/<name>     exists = the Worker exists; holds the deployed
#                             version id (empty: never deployed)
#   $STATE/versions/<name>/<id>  one per version; holds its message
#   $STATE/schedules/<name>   the cron list last applied (`crons = [...]`)
#   $STATE/consumers/<name>   the queues this Worker consumes, one per line
#   $STATE/subdomain/<name>   exists = the Worker is on workers.dev
#
# Invocation records:
#
#   $STATE/argv.log      one line per call: the arguments, space-joined
#   $STATE/env.log       one line per call: token|account|HOME set?
#   $STATE/stdin.<n>     what the n-th call read from stdin
#   $STATE/cwd.<n>       the n-th call's wrangler.toml, if its cwd had one
#
# Failure injection: if $STATE/fail/<key> exists, where <key> is the first
# one or two arguments joined by '-' (e.g. `whoami`, `versions-upload`,
# `delete`), the call fails with the file's `exit|code|message`.
set -u
STATE="__STATE__"
mkdir -p "$STATE/kv" "$STATE/d1" "$STATE/r2" "$STATE/queues" "$STATE/workers" "$STATE/versions" \
  "$STATE/schedules" "$STATE/consumers" "$STATE/subdomain" "$STATE/fail"

n=$(( $(cat "$STATE/count" 2>/dev/null || echo 0) + 1 ))
echo "$n" > "$STATE/count"
printf '%s\n' "$*" >> "$STATE/argv.log"
printf '%s|%s|%s\n' "${CLOUDFLARE_API_TOKEN:-}" "${CLOUDFLARE_ACCOUNT_ID:-}" "${HOME:+home}" >> "$STATE/env.log"
cat > "$STATE/stdin.$n"
[ -f wrangler.toml ] && cp wrangler.toml "$STATE/cwd.$n"

record() {
  if [ -n "${WRANGLER_OUTPUT_FILE_PATH:-}" ]; then
    printf '%s\n' "$1" >> "$WRANGLER_OUTPUT_FILE_PATH"
  fi
}

fail() { # exit code message
  echo "✘ [ERROR] $3 [code: $2]" >&2
  record "{\"type\":\"command-failed\",\"version\":1,\"code\":$2,\"message\":\"$3\"}"
  exit "$1"
}

key="$1"
[ $# -ge 2 ] && case "$2" in -*) ;; *) key="$1-$2" ;; esac
for k in "$key" "$1"; do
  if [ -f "$STATE/fail/$k" ]; then
    IFS='|' read -r code api msg < "$STATE/fail/$k"
    fail "$code" "$api" "$msg"
  fi
done

user_error() { # message: a wrangler UserError, which carries no API code
  echo "✘ [ERROR] $1" >&2
  record "{\"type\":\"command-failed\",\"version\":1,\"message\":\"$1\"}"
  exit 1
}

toml_name() { sed -n 's/^name = "\(.*\)"$/\1/p' wrangler.toml | head -n1; }

toml_crons() { # the `crons = [...]` value under [triggers], if any
  awk '/^\[/ { section = $0 } section == "[triggers]" && /^crons = / { sub(/^crons = /, ""); print; exit }' wrangler.toml
}

toml_consumed_queues() {
  awk '/^\[/ { section = $0 } section == "[[queues.consumers]]" && /^queue = / { gsub(/^queue = "|"$/, ""); print }' wrangler.toml
}

new_version_id() { printf '%08x-0000-4000-8000-%012x' "$n" "$n"; }

# triggersDeploy: workers.dev unless `workers_dev = false`, schedules when
# crons are present, and every consumer (its queue must exist).
apply_triggers() { # worker
  local name="$1" crons queue
  while IFS= read -r queue; do
    [ -n "$queue" ] || continue
    [ -e "$STATE/queues/$queue" ] || fail 1 11000 "Queue \\\"$queue\\\" does not exist."
  done <<< "$(toml_consumed_queues)"
  if grep -q '^workers_dev = false' wrangler.toml; then
    rm -f "$STATE/subdomain/$name"
  else
    touch "$STATE/subdomain/$name"
  fi
  crons="$(toml_crons)"
  if [ -n "$crons" ]; then
    printf '%s\n' "$crons" > "$STATE/schedules/$name"
    echo "  schedule: $crons"
  fi
  while IFS= read -r queue; do
    [ -n "$queue" ] || continue
    grep -qx "$queue" "$STATE/consumers/$name" 2>/dev/null || printf '%s\n' "$queue" >> "$STATE/consumers/$name"
    echo "  Consumer for $queue"
  done <<< "$(toml_consumed_queues)"
}

opt() { # value of --flag in the remaining args
  local flag="$1"; shift
  while [ $# -gt 0 ]; do
    if [ "$1" = "$flag" ]; then echo "$2"; return; fi
    shift
  done
}

json_list() { # dir idfield namefield
  local dir="$1" idf="$2" namef="$3" first=1
  printf '['
  for f in "$dir"/*; do
    [ -e "$f" ] || continue
    [ $first -eq 1 ] || printf ','
    first=0
    printf '\n  {"%s": "%s", "%s": "%s"}' "$idf" "$(cat "$f")" "$namef" "$(basename "$f")"
  done
  printf '\n]\n'
}

case "$1 ${2:-} ${3:-}" in
  "whoami --json "*)
    echo '{"loggedIn": true, "authType": "API Token", "accounts": []}' ;;
  "kv namespace list")
    json_list "$STATE/kv" id title ;;
  "kv namespace create")
    title="$4"
    [ -e "$STATE/kv/$title" ] && fail 1 10014 "A KV namespace with the title \\\"$title\\\" already exists."
    printf 'kvid%04d' "$n" > "$STATE/kv/$title"
    echo "🌀 Creating namespace with title \"$title\""; echo "✨ Success!" ;;
  "kv namespace delete")
    id="$(opt --namespace-id "$@")"
    for f in "$STATE/kv"/*; do
      if [ -e "$f" ] && [ "$(cat "$f")" = "$id" ]; then rm "$f"; echo "Deleted"; exit 0; fi
    done
    fail 1 10013 "namespace not found" ;;
  "d1 list --json")
    json_list "$STATE/d1" uuid name ;;
  "d1 create "*)
    [ -e "$STATE/d1/$3" ] && fail 1 7502 "A database with that name already exists"
    printf '0000d1%02d-0000-4000-8000-000000000000' "$n" > "$STATE/d1/$3"
    echo "✅ Successfully created DB '$3'" ;;
  "d1 delete "*)
    [ -e "$STATE/d1/$3" ] || fail 1 7404 "database not found"
    rm "$STATE/d1/$3"; echo "Deleted" ;;
  "r2 bucket info"|"r2 bucket create"|"r2 bucket delete")
    b="$4"
    case "$3" in
      info) [ -e "$STATE/r2/$b" ] || fail 1 10006 "The specified bucket does not exist."; echo "name: $b" ;;
      create) [ -e "$STATE/r2/$b" ] && fail 1 10004 "The bucket you tried to create already exists"; touch "$STATE/r2/$b"; echo "✅ Created bucket '$b'" ;;
      delete) [ -e "$STATE/r2/$b" ] || fail 1 10006 "The specified bucket does not exist."; rm "$STATE/r2/$b"; echo "Deleted" ;;
    esac ;;
  "queues info "*|"queues create "*|"queues delete "*)
    q="$3"
    case "$2" in
      info) [ -e "$STATE/queues/$q" ] || fail 1 11000 "Queue does not exist"; echo "Queue Name: $q"; echo "Queue ID: $(cat "$STATE/queues/$q")" ;;
      create) [ -e "$STATE/queues/$q" ] && fail 1 11009 "Queue name already taken"; printf 'qid%04d' "$n" > "$STATE/queues/$q"; echo "✅ Created queue '$q'" ;;
      delete) [ -e "$STATE/queues/$q" ] || fail 1 11000 "Queue does not exist"; rm "$STATE/queues/$q"; echo "Deleted queue $q." ;;
    esac ;;
  "deploy "*)
    [ -f wrangler.toml ] || user_error "Missing entry-point to Worker script or to assets directory"
    main="$(sed -n 's/^main = "\(.*\)"$/\1/p' wrangler.toml | head -n1)"
    [ -f "$main" ] || user_error "The entry-point file at \\\"$main\\\" was not found."
    name="$(toml_name)"
    msg="$(opt --message "$@")"
    id="$(new_version_id)"
    mkdir -p "$STATE/versions/$name"
    printf '%s' "$msg" > "$STATE/versions/$name/$id"
    printf '%s' "$id" > "$STATE/workers/$name"
    echo "Uploaded $name"
    apply_triggers "$name"
    echo "Current Version ID: $id"
    record "{\"type\":\"deploy\",\"version\":1,\"worker_name\":\"$name\",\"worker_tag\":\"tag-$name\",\"version_id\":\"$id\"}" ;;
  "triggers deploy"*)
    [ -f wrangler.toml ] || user_error "You need to provide a name when uploading a Worker Version."
    name="$(toml_name)"
    [ -e "$STATE/workers/$name" ] || fail 1 10007 "This Worker does not exist on your account."
    apply_triggers "$name" ;;
  "versions upload"*)
    [ -f wrangler.toml ] || user_error "Missing entry-point to Worker script or to assets directory"
    name="$(toml_name)"
    [ -e "$STATE/workers/$name" ] || user_error "You cannot upload a new version of a Worker that does not yet exist. Please run the \`deploy\` command first."
    msg="$(opt --message "$@")"
    id="$(new_version_id)"
    mkdir -p "$STATE/versions/$name"
    printf '%s' "$msg" > "$STATE/versions/$name/$id"
    echo "Worker Version ID: $id"
    echo "Changes to triggers (routes, custom domains, cron schedules, etc) must be applied with the command wrangler triggers deploy"
    record "{\"type\":\"version-upload\",\"version\":1,\"worker_name\":\"$name\",\"version_id\":\"$id\"}" ;;
  "versions list"*)
    name="$(opt --name "$@")"
    [ -e "$STATE/workers/$name" ] || fail 1 10007 "This Worker does not exist on your account."
    printf '['; first=1
    for f in "$STATE/versions/$name"/*; do
      [ -e "$f" ] || continue
      [ $first -eq 1 ] || printf ','; first=0
      printf '\n{"id": "%s", "annotations": {"workers/message": "%s"}}' "$(basename "$f")" "$(cat "$f")"
    done
    printf '\n]\n' ;;
  "versions deploy"*)
    name="$(opt --name "$@")"
    id="${3%@*}"
    printf '%s' "$id" | grep -Eq '^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$' \
      || user_error "\\\"$id\\\" is not a valid Version ID."
    [ -e "$STATE/versions/$name/$id" ] || fail 1 10225 "version not found"
    printf '%s' "$id" > "$STATE/workers/$name"
    record "{\"type\":\"version-deploy\",\"version\":1,\"worker_name\":\"$name\",\"deployment_id\":\"dep$n\",\"version_traffic\":{\"$id\":100}}" ;;
  "deployments status"*)
    name="$(opt --name "$@")"
    [ -e "$STATE/workers/$name" ] || fail 1 10007 "This Worker does not exist on your account."
    [ -s "$STATE/workers/$name" ] || user_error "The Worker $name has no deployments."
    printf '{"id": "dep-current", "versions": [{"version_id": "%s", "percentage": 100}]}\n' "$(cat "$STATE/workers/$name")" ;;
  "delete "*)
    [ -e "$STATE/workers/$2" ] || fail 1 10090 "This Worker does not exist on your account."
    rm -f "$STATE/workers/$2" "$STATE/schedules/$2" "$STATE/consumers/$2" "$STATE/subdomain/$2"
    rm -rf "$STATE/versions/$2"; echo "Successfully deleted $2" ;;
  *)
    echo "fake wrangler: unsupported command: $*" >&2
    exit 2 ;;
esac
