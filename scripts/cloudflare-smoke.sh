#!/usr/bin/env bash
# Real-account smoke test for the Cloudflare target.
#
# Everything else about Cloudflare deployment is tested against a fake
# `wrangler`. This is the one check that talks to Cloudflare: it builds a
# sample application that uses a KV namespace, a D1 database, an R2 bucket, a
# queue (producer and consumer), a secret and a cron trigger; drives the
# Cloudflare provider driver through the same deploy steps the control plane
# runs (preflight, plan, prepare, upload, verify, activate, observe), with the
# same process envelope and private per-operation work directory; checks
# through the Cloudflare API that the first upload created the Worker without
# publishing it (no workers.dev route, no schedule) and that activate applied
# the cron schedule and the queue consumer; requests the deployed Worker and
# checks every binding, the queue and the cron answer; then deletes the
# Worker, purges the run's resources and checks nothing named for the run is
# left.
#
# The environment is throwaway: its id is minted per run, and the driver
# namespaces every Cloudflare resource by application and environment id, so
# a run never touches another application's resources and two runs never
# share one.
#
# The driver is driven directly rather than through a deis-control: standing
# up a control plane needs a signed, transparently logged driver release,
# which is release engineering rather than a smoke test. The control plane's
# half (payload upload, digest re-verification, work directory, process
# launch) is covered by the workspace's own tests.
#
# Usage:
#   CLOUDFLARE_API_TOKEN=... CLOUDFLARE_ACCOUNT_ID=... scripts/cloudflare-smoke.sh
#
# Optional:
#   DEIS_BIN, DEIS_CLOUDFLARE_DRIVER_BIN  prebuilt binaries (otherwise cargo builds them)
#   SMOKE_KEEP_WORKDIR=1                  keep the local work directory for inspection
#   SMOKE_CRON_WAIT_SECONDS               how long to wait for a cron tick (default 150)
#
# Needs: bash, python3, curl, node >= 22 (the driver runs wrangler on it).

set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
PROTOCOL="edge-canon-provider-driver-v1"
CRON_WAIT_SECONDS="${SMOKE_CRON_WAIT_SECONDS:-150}"
CF_API="https://api.cloudflare.com/client/v4"

say() { printf '\n== %s\n' "$*"; }
fail() { printf '\nFAIL: %s\n' "$*" >&2; exit 1; }

: "${CLOUDFLARE_API_TOKEN:?set CLOUDFLARE_API_TOKEN to a token with Workers Scripts, KV, D1, R2 and Queues edit on the account}"
: "${CLOUDFLARE_ACCOUNT_ID:?set CLOUDFLARE_ACCOUNT_ID}"
for tool in python3 curl node; do
  command -v "$tool" >/dev/null || fail "$tool is required"
done
node_major="$(node -p 'process.versions.node.split(".")[0]')"
[ "$node_major" -ge 22 ] || fail "node >= 22 is required, found $(node --version)"

# ---- binaries -----------------------------------------------------------

if [ -z "${DEIS_BIN:-}" ] || [ -z "${DEIS_CLOUDFLARE_DRIVER_BIN:-}" ]; then
  say "building deis and deis-provider-driver-cloudflare"
  (
    cd "$ROOT_DIR"
    if [ -x /usr/local/bin/rust-target-env ]; then
      eval "$(/usr/local/bin/rust-target-env)"
    fi
    cargo build -p deis -p deis-provider-driver-cloudflare
  )
  target_dir="$(cd "$ROOT_DIR" && { [ -x /usr/local/bin/rust-target-env ] && eval "$(/usr/local/bin/rust-target-env)"; cargo metadata --format-version 1 --no-deps | python3 -c 'import json,sys; print(json.load(sys.stdin)["target_directory"])'; })"
  DEIS_BIN="${DEIS_BIN:-$target_dir/debug/deis}"
  DEIS_CLOUDFLARE_DRIVER_BIN="${DEIS_CLOUDFLARE_DRIVER_BIN:-$target_dir/debug/deis-provider-driver-cloudflare}"
fi
[ -x "$DEIS_BIN" ] || fail "no deis binary at $DEIS_BIN"
[ -x "$DEIS_CLOUDFLARE_DRIVER_BIN" ] || fail "no driver binary at $DEIS_CLOUDFLARE_DRIVER_BIN"

# ---- identities ----------------------------------------------------------

WORK="$(mktemp -d "${TMPDIR:-/tmp}/cloudflare-smoke.XXXXXX")"
chmod 700 "$WORK"
uuid7() {
  python3 - <<'PY'
import os, time, uuid
ms = int(time.time() * 1000)
b = bytearray(ms.to_bytes(6, "big") + os.urandom(10))
b[6] = (b[6] & 0x0F) | 0x70
b[8] = (b[8] & 0x3F) | 0x80
print(uuid.UUID(bytes=bytes(b)))
PY
}
APP_ID="$(uuid7)"
ENV_ID="$(uuid7)"
TARGET_ID="$(uuid7)"
INSTALLATION_ID="$(uuid7)"
DESIRED="$(python3 -c 'import hashlib,sys; print(hashlib.sha256(sys.argv[1].encode()).hexdigest())' "$APP_ID/$ENV_ID")"
SECRET_VALUE="smoke-secret-$(python3 -c 'import secrets; print(secrets.token_hex(8))')"
SECRET_SHA="$(python3 -c 'import hashlib,sys; print(hashlib.sha256(sys.argv[1].encode()).hexdigest())' "$SECRET_VALUE")"
# The driver's namespace for this application/environment; every resource it
# creates carries it. Also read back from the activate receipt below.
SCOPE="deis-$(python3 -c 'import hashlib,sys; print(hashlib.sha256(("deis-cloudflare-scope/v1\n%s\n%s" % (sys.argv[1], sys.argv[2])).encode()).hexdigest()[:12])' "$APP_ID" "$ENV_ID")"
CREDENTIAL="$(python3 -c 'import json,os; print(json.dumps({"api_token": os.environ["CLOUDFLARE_API_TOKEN"], "account_id": os.environ["CLOUDFLARE_ACCOUNT_ID"]}))')"
echo "run: application $APP_ID, environment $ENV_ID, scope $SCOPE, work $WORK"

# ---- the sample application ---------------------------------------------

PROJECT="$WORK/project"
mkdir -p "$PROJECT/functions/api"
cat > "$PROJECT/.config.json" <<'JSON'
{
  "version": "1.0.0",
  "name": "cloudflare-smoke",
  "runtime": "standard-v1",
  "language": "typescript",
  "functionRoot": "./functions",
  "compatibilityDate": "2026-09-01",
  "environment": {
    "declarations": [
      {"name": "SMOKE_SECRET", "kind": "secret", "valueType": "string", "required": true}
    ]
  },
  "bindings": {
    "HOT": {"kind": "kv"},
    "DB": {"kind": "database"},
    "FILES": {"kind": "blob"},
    "JOBS": {"kind": "queue"}
  },
  "queues": [{"name": "JOBS", "batchSize": 1}],
  "cron": ["* * * * *"],
  "vendors": {"deislet": {"enabled": true}, "cloudflare": {"enabled": true}},
  "build": {"outDir": "./dist"}
}
JSON
cat > "$PROJECT/functions/api/check.ts" <<'TS'
// One request exercises KV, D1, R2 and the secret, and reports each.
async function sha256(text: string): Promise<string> {
  const digest = await crypto.subtle.digest("SHA-256", new TextEncoder().encode(text));
  return [...new Uint8Array(digest)].map((b) => b.toString(16).padStart(2, "0")).join("");
}
export default async function handler(context: any): Promise<Response> {
  const env = context.env;
  const nonce = crypto.randomUUID();
  const result: Record<string, unknown> = {};
  await env.HOT.put("smoke:kv", nonce);
  result.kv = (await env.HOT.get("smoke:kv")) === nonce;
  const row = await env.DB.prepare("SELECT ?1 AS echo").bind(nonce).first();
  result.d1 = row !== null && row.echo === nonce;
  await env.FILES.put("smoke/object.txt", nonce);
  const object = await env.FILES.get("smoke/object.txt");
  result.r2 = object !== null && (await object.text()) === nonce;
  result.secretSha256 = env.SMOKE_SECRET ? await sha256(env.SMOKE_SECRET) : null;
  return new Response(JSON.stringify(result), { headers: { "content-type": "application/json" } });
}
TS
cat > "$PROJECT/functions/api/enqueue.ts" <<'TS'
export default async function handler(context: any): Promise<Response> {
  const id = new URL(context.request.url).searchParams.get("id") ?? "none";
  await context.env.JOBS.send({ id });
  return new Response(JSON.stringify({ queued: id }), { status: 202 });
}
TS
cat > "$PROJECT/functions/api/state.ts" <<'TS'
// What the queue consumer and the cron handler have recorded.
export default async function handler(context: any): Promise<Response> {
  const id = new URL(context.request.url).searchParams.get("id") ?? "none";
  const body = {
    queue: await context.env.HOT.get(`smoke:queue:${id}`),
    cron: await context.env.HOT.get("smoke:cron"),
  };
  return new Response(JSON.stringify(body), { headers: { "content-type": "application/json" } });
}
TS
cat > "$PROJECT/functions/_queue.ts" <<'TS'
export default async function queue(batch: any, env: any, _context: any): Promise<void> {
  for (const message of batch.messages) {
    const body = typeof message.json === "function" ? message.json() : message.body;
    await env.HOT.put(`smoke:queue:${body.id}`, "consumed");
  }
}
TS
cat > "$PROJECT/functions/_scheduled.ts" <<'TS'
export default async function scheduled(event: any, env: any, _context: any): Promise<void> {
  await env.HOT.put("smoke:cron", new Date(event.scheduledTime).toISOString());
}
TS

say "building the Cloudflare payload"
(cd "$PROJECT" && "$DEIS_BIN" build --target cloudflare)
PAYLOAD_STORE="$PROJECT/dist/providers/cloudflare"
mapfile -t payloads < <(find "$PAYLOAD_STORE" -mindepth 1 -maxdepth 1 -type d)
[ "${#payloads[@]}" -eq 1 ] || fail "expected one Cloudflare payload under $PAYLOAD_STORE, found ${#payloads[@]}"
PAYLOAD="${payloads[0]}"
PAYLOAD_SHA="$(basename "$PAYLOAD")"
grep -q 'deis:unresolved/' "$PAYLOAD/wrangler.toml" || fail "the payload's wrangler.toml carries no placeholders to fill"
echo "payload $PAYLOAD_SHA"

# ---- driving the driver --------------------------------------------------

mkdir -p "$WORK/provider-work/operations" "$WORK/provider-work/state/$TARGET_ID/$ENV_ID" "$WORK/provider-work/home"
chmod -R 700 "$WORK/provider-work"
STATE_DIR="$WORK/provider-work/state/$TARGET_ID/$ENV_ID"
LAST_RECEIPT="$WORK/last-receipt.json"

# run_step <operation> <with-artifact:yes|no> [extra env assignments...]
run_step() {
  local operation="$1" with_artifact="$2"
  shift 2
  local operation_id directory artifact_dir envelope outcome
  operation_id="$(uuid7)"
  directory="$WORK/provider-work/operations/$operation_id-$(uuid7)"
  mkdir -m 700 "$directory"
  artifact_dir=""
  if [ "$with_artifact" = yes ]; then
    artifact_dir="$directory/$PAYLOAD_SHA"
    cp -R "$PAYLOAD" "$artifact_dir"
  fi
  envelope="$(python3 - "$operation" "$operation_id" "$directory" "$artifact_dir" <<PY
import json, sys
operation, operation_id, directory, artifact_dir = sys.argv[1:5]
artifact = None
if artifact_dir:
    artifact = {
        "artifactVersionId": "$(uuid7)",
        "canonicalArtifactSha256": "$DESIRED",
        "providerPayloadSha256": "$PAYLOAD_SHA",
    }
print(json.dumps({
    "messageType": "request",
    "schemaVersion": 1,
    "protocol": "$PROTOCOL",
    "request": {
        "schemaVersion": 1,
        "operationId": operation_id,
        "operation": operation,
        "desiredStateDigest": "$DESIRED",
        "applicationId": "$APP_ID",
        "environmentId": "$ENV_ID",
        "target": {
            "providerTargetId": "$TARGET_ID",
            "providerInstallationId": "$INSTALLATION_ID",
            "backendId": "cloudflare-workers-pages",
            "targetConfigDigest": "$DESIRED",
        },
        "artifact": artifact,
        "plan": {
            "bindingSnapshotDigest": None,
            "routePlanDigest": None,
            "triggerPlanDigest": None,
            "trafficPolicyDigest": None,
            "priorDeploymentId": None,
        },
    },
    "workspace": {
        "directory": directory,
        "artifactDirectory": artifact_dir or None,
        "stateDirectory": "$STATE_DIR",
    },
}))
PY
)"
  printf '  %-9s ' "$operation"
  set +e
  outcome="$(cd "$directory" && printf '%s' "$envelope" | env -i \
    PATH="$(dirname "$(command -v node)"):/usr/local/bin:/usr/bin:/bin" \
    HOME="$WORK/provider-work/home" \
    TMPDIR="$directory" \
    DEIS_PROVIDER_NODE="$(command -v node)" \
    EDGE_CANON_PROVIDER_CREDENTIAL="$CREDENTIAL" \
    EDGE_CANON_PROVIDER_SECRETS="{\"SMOKE_SECRET\":\"$SECRET_VALUE\"}" \
    "$@" \
    "$DEIS_CLOUDFLARE_DRIVER_BIN" execute --protocol "$PROTOCOL" 2>"$directory.stderr")"
  local status=$?
  set -e
  rm -rf "$directory"
  printf '%s' "$outcome" > "$LAST_RECEIPT"
  python3 - "$LAST_RECEIPT" "$status" "$directory.stderr" <<'PY'
import json, sys
path, status, stderr = sys.argv[1], int(sys.argv[2]), sys.argv[3]
try:
    outcome = json.load(open(path))
except Exception:
    print(f"no outcome (exit {status}); stderr:\n" + open(stderr).read()[-4000:])
    sys.exit(1)
receipt = outcome.get("receipt") or {}
if outcome.get("outcome") == "succeeded" and receipt.get("state") == "succeeded":
    print("succeeded", json.dumps(receipt.get("providerResourceIds", {}))[:300])
    sys.exit(0)
print(f"failed (exit {status}): " + json.dumps(outcome.get("error", outcome))[:2000])
sys.exit(1)
PY
}

cf_api() {
  curl -fsS -H "Authorization: Bearer $CLOUDFLARE_API_TOKEN" "$CF_API$1"
}

# Everything named for this run, as Cloudflare lists it.
leftovers() {
  # An unreachable API is reported rather than read as "nothing left".
  local listing
  if ! listing="$(list_named_resources)"; then
    echo "could-not-list resources (Cloudflare API unreachable or refused)"
    return
  fi
  printf '%s\n' "$listing" | grep -F -- "$SCOPE" || true
}

list_named_resources() {
  set -o pipefail
  {
    cf_api "/accounts/$CLOUDFLARE_ACCOUNT_ID/workers/scripts" | python3 -c 'import json,sys; [print("worker", s["id"]) for s in json.load(sys.stdin)["result"]]' || return 1
    cf_api "/accounts/$CLOUDFLARE_ACCOUNT_ID/storage/kv/namespaces?per_page=100" | python3 -c 'import json,sys; [print("kv", s["title"]) for s in json.load(sys.stdin)["result"]]' || return 1
    cf_api "/accounts/$CLOUDFLARE_ACCOUNT_ID/d1/database?per_page=100" | python3 -c 'import json,sys; [print("d1", s["name"]) for s in json.load(sys.stdin)["result"]]' || return 1
    cf_api "/accounts/$CLOUDFLARE_ACCOUNT_ID/r2/buckets" | python3 -c 'import json,sys; r=json.load(sys.stdin)["result"]; [print("r2", s["name"]) for s in (r.get("buckets", []) if isinstance(r, dict) else r)]' || return 1
    cf_api "/accounts/$CLOUDFLARE_ACCOUNT_ID/queues" | python3 -c 'import json,sys; [print("queue", s["queue_name"]) for s in json.load(sys.stdin)["result"]]' || return 1
  }
}

CLEANED=0
cleanup() {
  local status=$?
  if [ "$CLEANED" -eq 0 ]; then
    CLEANED=1
    say "cleaning up (delete, then purge this run's resources)"
    run_step delete no || echo "  delete failed; see above"
    # Purging data resources requires explicit consent naming this exact
    # environment; nothing else is ever destroyed.
    run_step cleanup no DEIS_PROVIDER_PURGE_DATA="$ENV_ID" || echo "  cleanup failed; see above"
    local left
    left="$(leftovers)"
    if [ -n "$left" ]; then
      printf 'resources named for this run are still on the account:\n%s\n' "$left" >&2
      status=1
    else
      echo "  nothing named $SCOPE is left on the account"
    fi
  fi
  if [ "${SMOKE_KEEP_WORKDIR:-0}" != 1 ]; then rm -rf "$WORK"; fi
  exit "$status"
}
trap cleanup EXIT

# The Worker is named by the scope (the driver's naming, mirrored above).
WORKER="$SCOPE"
JOBS_QUEUE="$SCOPE-q-jobs"

schedules() {
  cf_api "/accounts/$CLOUDFLARE_ACCOUNT_ID/workers/scripts/$WORKER/schedules" \
    | python3 -c 'import json,sys; [print(s["cron"]) for s in json.load(sys.stdin)["result"].get("schedules", [])]'
}

workers_dev_enabled() {
  cf_api "/accounts/$CLOUDFLARE_ACCOUNT_ID/workers/scripts/$WORKER/subdomain" \
    | python3 -c 'import json,sys; print("yes" if json.load(sys.stdin)["result"].get("enabled") else "no")'
}

say "deploying through the driver"
for step in preflight plan prepare upload; do
  run_step "$step" yes || fail "$step failed"
done

# A first upload creates the Worker with an unpublished placeholder, then
# uploads the real version without deploying it.
[ "$(workers_dev_enabled)" = no ] || fail "the Worker is on workers.dev before activate"
[ -z "$(schedules)" ] || fail "a cron schedule was applied before activate"
echo "  after upload: the Worker exists, is not on workers.dev and has no schedule"

for step in verify activate observe; do
  run_step "$step" yes || fail "$step failed"
done

# Activate applies what Cloudflare keeps per Worker: the schedule, the queue
# consumer and the workers.dev route.
[ "$(schedules)" = "* * * * *" ] || fail "activate did not apply the cron schedule (got: $(schedules | tr '\n' ' '))"
[ "$(workers_dev_enabled)" = yes ] || fail "activate did not enable workers.dev"
JOBS_QUEUE_ID="$(cf_api "/accounts/$CLOUDFLARE_ACCOUNT_ID/queues" | python3 -c 'import json,sys; [print(q["queue_id"]) for q in json.load(sys.stdin)["result"] if q["queue_name"] == sys.argv[1]]' "$JOBS_QUEUE")"
[ -n "$JOBS_QUEUE_ID" ] || fail "queue $JOBS_QUEUE does not exist"
cf_api "/accounts/$CLOUDFLARE_ACCOUNT_ID/queues/$JOBS_QUEUE_ID/consumers" | grep -qF "\"$WORKER\"" \
  || fail "activate did not attach $WORKER as the consumer of $JOBS_QUEUE"
echo "  after activate: schedule, queue consumer and workers.dev are applied"
SUBDOMAIN="$(cf_api "/accounts/$CLOUDFLARE_ACCOUNT_ID/workers/subdomain" | python3 -c 'import json,sys; print(json.load(sys.stdin)["result"]["subdomain"])')"
URL="https://$WORKER.$SUBDOMAIN.workers.dev"
echo "worker $WORKER at $URL"

say "checking the deployed Worker"
check=""
for _ in $(seq 1 30); do
  if check="$(curl -fsS "$URL/api/check")"; then break; fi
  sleep 5
done
[ -n "$check" ] || fail "$URL/api/check never answered"
echo "  $check"
python3 - "$check" "$SECRET_SHA" <<'PY' || fail "a binding check failed"
import json, sys
result, secret_sha = json.loads(sys.argv[1]), sys.argv[2]
failed = [name for name in ("kv", "d1", "r2") if result.get(name) is not True]
if result.get("secretSha256") != secret_sha:
    failed.append("secret")
if failed:
    print("  failed:", ", ".join(failed))
    sys.exit(1)
print("  kv, d1, r2 and the secret all answer")
PY

MESSAGE_ID="$(uuid7)"
curl -fsS -X POST "$URL/api/enqueue?id=$MESSAGE_ID" >/dev/null || fail "enqueue failed"
queue_ok=0
for _ in $(seq 1 30); do
  state="$(curl -fsS "$URL/api/state?id=$MESSAGE_ID" || true)"
  if printf '%s' "$state" | python3 -c 'import json,sys; sys.exit(0 if json.load(sys.stdin).get("queue") == "consumed" else 1)' 2>/dev/null; then
    queue_ok=1; break
  fi
  sleep 5
done
[ "$queue_ok" -eq 1 ] || fail "the queue consumer never recorded message $MESSAGE_ID"
echo "  queue: produced and consumed $MESSAGE_ID"

cron_ok=0
deadline=$((SECONDS + CRON_WAIT_SECONDS))
while [ "$SECONDS" -lt "$deadline" ]; do
  state="$(curl -fsS "$URL/api/state?id=$MESSAGE_ID" || true)"
  if printf '%s' "$state" | python3 -c 'import json,sys; sys.exit(0 if json.load(sys.stdin).get("cron") else 1)' 2>/dev/null; then
    cron_ok=1; break
  fi
  sleep 10
done
[ "$cron_ok" -eq 1 ] || fail "no cron tick was recorded within ${CRON_WAIT_SECONDS}s"
echo "  cron: a scheduled run was recorded"

say "smoke passed; cleaning up"
