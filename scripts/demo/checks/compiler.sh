# One group of `scripts/demo.sh verify`: the targets `deis build` can produce
# from one source tree, and the project shape it refuses. Sourced by
# `scripts/demo.sh`.

# ----------------------------------------------------------------------
# The compiler
# ----------------------------------------------------------------------
#
# The README says one project builds for four hosts. The demo built one of
# them: `deis deploy` compiles for `deislet` because that is the only vendor
# the example app enables, and nothing here had ever run the other three
# adapters at all. Three of the four claims in that sentence were carried by
# the sentence.
#
# This builds the portable fixture for all four and looks at what came out. It
# deploys nothing: three of these artifacts are for platforms that are not
# running anywhere near this machine, so what can be checked is that each one
# exists, carries this application rather than a stub, and is accompanied by
# the file that host needs to accept it - `wrangler.toml` for Cloudflare,
# `deno.json` for Deno Deploy, `edgeone.json` for EdgeOne. Without that last
# part the check would pass on four copies of one bundle in four directories.
#
# `deis build` is run directly rather than through `deis_cli`: a build talks to
# nothing, and handing it the demo's control-plane endpoint and credential
# would suggest otherwise.

# One file out of a build, checked for existence and for carrying the fixture's
# marker. Echoes what is wrong, or nothing when it is right.
portable_artifact_fault() {
  local file=$1
  local what=$2
  if [ ! -f "$file" ]; then
    echo "${what} is missing (${file})"
    return
  fi
  if ! grep -q "$PORTABLE_MARKER" "$file"; then
    echo "${what} does not carry the handler's marker, so it is not this application"
  fi
}

# A provider payload is immutable and named by its complete file-set digest.
# This demo builds each fresh fixture once, so exactly one child is expected.
provider_payload_dir() {
  local project=$1
  local vendor=$2
  local found=""
  local candidate
  for candidate in "${project}/dist/providers/${vendor}/"*; do
    [ -d "$candidate" ] || continue
    if [ -n "$found" ]; then
      return 1
    fi
    found=$candidate
  done
  [ -n "$found" ] || return 1
  echo "$found"
}

# What the build's own log says was verified against a vendor's tool and what
# was not, one clause per vendor target, for the evidence line below.
#
# `deis build --target all` runs `wrangler deploy --dry-run` and `deno check` when
# those tools are on PATH and says at ⚠ when they are not; the tencent target
# has no local dry-run at all and says so every time. The files and markers
# the check asserts on are the same either way, so a ✓ that did not quote
# this read identically on a machine with both tools, on one with neither,
# and on ubuntu-latest, which has neither - the local run silently carried a
# wrangler acceptance that CI can never reproduce, and nothing said which of
# the two runs a reader was looking at. A target the log says nothing about,
# neither verified nor skipped, is reported as such and the check treats it
# as a fault: it means the build stopped saying, and a check that fills the
# silence in would be back where it started.
vendor_verification_summary() {
  local log=$1
  local cloudflare deno tencent
  if grep -q "wrangler accepted the Worker" "$log" 2>/dev/null; then
    cloudflare="cloudflare verified (wrangler deploy --dry-run accepted the Worker)"
  elif grep -q "the cloudflare target was not verified" "$log" 2>/dev/null; then
    cloudflare="cloudflare NOT verified (wrangler is not on PATH)"
  else
    cloudflare="cloudflare: the build log says nothing about verification"
  fi
  if grep -q "deno check accepted the program" "$log" 2>/dev/null; then
    deno="deno verified (deno check accepted the program)"
  elif grep -q "the deno target was not verified" "$log" 2>/dev/null; then
    deno="deno NOT verified (deno is not on PATH)"
  else
    deno="deno: the build log says nothing about verification"
  fi
  if grep -q "the tencent target was not verified against EdgeOne" "$log" 2>/dev/null; then
    tencent="tencent NOT verified (EdgeOne has no local dry-run)"
  else
    tencent="tencent: the build log says nothing about verification"
  fi
  echo "${cloudflare}; ${deno}; ${tencent}"
}

check_every_compiler_target_is_built() {
  local title="one project compiles for all four targets, each with the files that host needs"
  local dir="${DEMO_DIR}/app-portable"
  local log="${LOG_DIR}/cli-build-portable.log"

  if [ ! -f "${PORTABLE_SRC_DIR}/.config.json" ]; then
    fail_check "$title" "no portable fixture at ${PORTABLE_SRC_DIR}"
    return
  fi
  if ! command -v python3 >/dev/null 2>&1; then
    fail_check "$title" "python3 is needed to look inside the deislet bundle"
    return
  fi

  # A fresh copy every time: a build writes dist/ into the project it is
  # given, and artifacts left by a previous run would be indistinguishable
  # from artifacts this one produced.
  rm -rf "$dir"
  mkdir -p "$dir"
  cp -R "${PORTABLE_SRC_DIR}/." "$dir/"

  if ! ( cd "$dir" && "${BIN_DIR}/deis" build --target all ) > "$log" 2>&1; then
    fail_check "$title" \
      "deis build --target all failed: $(tail -n 6 "$log" | tr '\n' ' ' | head -c 300)"
    return
  fi

  # What each vendor has to have produced. The entry file is the application;
  # the second file is what makes the directory deployable to that host and to
  # no other.
  local cloudflare deno tencent deislet
  cloudflare=$(provider_payload_dir "$dir" cloudflare || true)
  deno=$(provider_payload_dir "$dir" deno || true)
  tencent=$(provider_payload_dir "$dir" tencent || true)
  deislet=$(provider_payload_dir "$dir" deislet || true)
  local faults=""
  [ -n "$cloudflare" ] || faults="${faults}Cloudflare payload identity was not published "
  [ -n "$deno" ] || faults="${faults}Deno payload identity was not published "
  [ -n "$tencent" ] || faults="${faults}EdgeOne payload identity was not published "
  [ -n "$deislet" ] || faults="${faults}Deislet payload identity was not published "
  faults="${faults}$(portable_artifact_fault "${cloudflare}/index.js" "Cloudflare's index.js") "
  faults="${faults}$(portable_artifact_fault "${deno}/project/functions/[[all]].js" "Deno's canonical module") "
  faults="${faults}$(portable_artifact_fault "${tencent}/edge-functions/index.js" "EdgeOne's index.js") "

  local file
  for file in "${cloudflare}/wrangler.toml" "${cloudflare}/package.json" \
    "${deno}/deno.json" "${deno}/_deis_internal/router.ts" \
    "${tencent}/edgeone.json" "${deislet}/bundle.zip"; do
    [ -f "$file" ] || faults="${faults}${file} was not written "
  done

  # The deislet bundle is a zip, so its entry is read out of the archive
  # rather than grepped through the compression.
  local bundled=""
  if [ -f "${deislet}/bundle.zip" ]; then
    bundled=$(python3 - "${deislet}/bundle.zip" "$PORTABLE_MARKER" <<'BUNDLE'
import sys, zipfile
with zipfile.ZipFile(sys.argv[1]) as archive:
    names = archive.namelist()
    worker = archive.read("worker/index.js").decode("utf-8", "replace")
print("ok" if sys.argv[2] in worker and "manifest.json" in names else "")
BUNDLE
)
    [ "$bundled" = "ok" ] || faults="${faults}the deislet bundle has no manifest.json, or its worker is not this application "
  fi

  # And that the three JavaScript entries are three files rather than one
  # copied three times: each adapter builds the context its own host hands a
  # handler, so two vendors producing byte-identical output would mean one of
  # them was never really targeted.
  local cf_sum eo_sum
  cf_sum=$(openssl dgst -sha256 "${cloudflare}/index.js" 2>/dev/null | awk '{print $NF}')
  eo_sum=$(openssl dgst -sha256 "${tencent}/edge-functions/index.js" 2>/dev/null | awk '{print $NF}')
  if [ -n "$cf_sum" ] && [ "$cf_sum" = "$eo_sum" ]; then
    faults="${faults}Cloudflare's and EdgeOne's entry files are byte-identical, so one adapter emitted the other's output "
  fi

  # The vendor's own configuration has to be about this project.
  grep -q '^name = "portable"' "${cloudflare}/wrangler.toml" 2>/dev/null \
    || faults="${faults}wrangler.toml does not name the project "
  grep -q '"portable"' "${tencent}/edgeone.json" 2>/dev/null \
    || faults="${faults}edgeone.json does not name the project "
  grep -q '"/\*"' "${deno}/_deis_internal/router.ts" 2>/dev/null \
    || faults="${faults}Deno's provider router does not invoke the canonical module through its catch-all "

  # Which vendor tools stood behind this run, from the build's own log.
  local verified
  verified=$(vendor_verification_summary "$log")
  case "$verified" in
    *"says nothing about verification"*)
      faults="${faults}the build log no longer says, per vendor target, whether the vendor's tool verified the artifact or was skipped (${verified}) " ;;
  esac

  faults=$(echo "$faults" | sed 's/^ *//; s/ *$//')
  if [ -n "$faults" ]; then
    fail_check "$title" \
      "deis build --target all reported success, but: ${faults}. The build said: $(tail -n 5 "$log" | tr '\n' ' ' | head -c 200)"
    return
  fi

  pass_check "$title" \
    "one \`deis build --target all\` over ${PORTABLE_SRC_DIR} published one canonical parent and four immutable payload directories under dist/providers/<vendor>/<digest>. Deislet carries bundle.zip, Cloudflare carries index.js plus the wrangler.toml naming \"portable\", Deno carries the reconstructed canonical module and its generated catch-all router, and EdgeOne carries edge-functions/index.js plus edgeone.json. Every entry carries the handler's own marker, and Cloudflare's and EdgeOne's differ from one another, so each packager built the shape its host expects from the same parent rather than copying one output four times. The files are this compiler's own work; what a vendor's tool said about them on this machine, from the build log: ${verified}. A target marked NOT verified was verified nowhere in this run"
}

# The application the check below tries, and fails, to deploy. A name of its
# own so that the refusal is filed under nothing the rest of the run reads.
WEBSOCKET_APP_ID="${WEBSOCKET_APP_ID:-demo-websocket}"

# Lay down a project whose one handler is the WebSocket echo endpoint this
# demo used to deploy, and echo the directory.
#
# Until the standard was locked the handler below was written into the staged
# copy of the example app, and two checks drove a real RFC 6455 handshake
# through deis-proxy and echoed a text frame and a binary frame over it. Edge
# Canon v1 defines no WebSocket application API - no WebSocketPair, no
# WebSocketServer, no 101 with a socket attached - because only some of the
# providers it covers have one, and a handler that runs on those alone is not
# portable. So what the platform promises about this handler changed: not
# that it works, but that it is refused by the build, which is the first step
# of `deis deploy`, before an application, an environment or a version exists
# for it. The handler is staged in a project of its own so that the refusal
# cannot cost the example app its deployment, and the project is otherwise
# the smallest one the compiler accepts, so that nothing else in it can be
# what was refused.
prepare_websocket_source() {
  local dest="${DEMO_DIR}/app-websocket"

  rm -rf "$dest"
  mkdir -p "${dest}/functions/api"

  cat > "${dest}/.config.json" <<JSON
{
  "version": "1.0.0",
  "name": "${WEBSOCKET_APP_ID}",
  "runtime": "standard-v1",
  "language": "typescript",
  "functionRoot": "./functions",
  "compatibilityDate": "2026-09-01",
  "vendors": { "deislet": { "enabled": true } },
  "build": { "outDir": "./dist" }
}
JSON

  cat > "${dest}/functions/api/ws.ts" <<'TS'
// A WebSocket echo endpoint in the shape some providers describe: make a
// WebSocketPair, accept the server half, and answer 101 with the client half
// attached to the Response. Edge Canon v1 has no portable form of any of it,
// so the build refuses this file at the reference to WebSocketPair.
export default async function handler(context: any): Promise<Response> {
  if (context.request.headers.get("upgrade") !== "websocket") {
    return new Response("this path answers a WebSocket handshake\n", {
      status: 426,
      headers: { "content-type": "text/plain; charset=utf-8" },
    });
  }

  const pair = new WebSocketPair();
  const client = pair[0];
  const server = pair[1];
  server.accept();
  server.addEventListener("message", (event: any) => {
    server.send("text:" + String(event.data));
  });

  return new Response(null, { status: 101, webSocket: client });
}
TS

  echo "$dest"
}

check_a_websocket_handler_is_refused_by_the_build() {
  local title="a handler that reaches for WebSocketPair is refused by the build, before anything is deployed"
  local json="${LOG_DIR}/cli-deploy-websocket.json"
  local errors="${LOG_DIR}/cli-deploy-websocket.log"

  local dir
  if ! dir=$(prepare_websocket_source); then
    fail_check "$title" "the project could not be staged under ${DEMO_DIR}"
    return
  fi

  # Through `deis deploy` and not `deis build`, because the claim is about
  # what a person who tried to ship this would meet: the deploy's first step
  # is the build, and a deploy that is refused there has nothing left to do.
  # `--app` for the reason the callee's deploy spells it: DEIS_APP in the
  # CLI's environment names the example app.
  local status=0
  (cd "$dir" && deis_cli deploy -e "$ENV_NAME" --app "$WEBSOCKET_APP_ID" --json) \
    > "$json" 2> "$errors" || status=$?

  local message
  message=$(grep -a -m 1 -o 'EC_STREAM_WEBSOCKET_NONPORTABLE.*' "$errors" 2>/dev/null || true)

  # What was left behind, on both sides: a bundle under the project, or an
  # application in the catalog. The list is walked whole, so an application
  # on a later page could not hide from this.
  local built="no"
  [ -e "${dir}/dist" ] && built="yes"
  local listed
  listed=$(control_list_all /api/v1/apps \
    | jq -r --arg id "$WEBSOCKET_APP_ID" '[.[] | select(.app_id == $id)] | length' 2>/dev/null || true)

  if [ "$status" -ne 0 ] && echo "$message" | grep -q "WebSocketPair" \
    && [ "$built" = "no" ] && [ "${listed:-x}" = "0" ]; then
    pass_check "$title" \
      "deis deploy --app ${WEBSOCKET_APP_ID} exited ${status} at its first step, the build: \"$(echo "$message" | cut -c1-200)\". No dist/ was written under the project and the control plane lists no application called ${WEBSOCKET_APP_ID}. This handler used to be deployed by this demo and driven through deis-proxy with a real handshake; the locked standard removed the server-side WebSocket API rather than leave it to the providers that have one, so what an application is promised is this refusal, and not a 101"
  else
    fail_check "$title" \
      "expected a non-zero exit naming EC_STREAM_WEBSOCKET_NONPORTABLE and WebSocketPair, no dist/ and no application in the catalog; got exit ${status}, message '${message:-<none>}', dist written: ${built}, applications called ${WEBSOCKET_APP_ID}: ${listed:-<unreadable>}. The CLI said: $(tail -n 3 "$errors" | tr '\n' ' ' | head -c 300)"
  fi
}

# The checks this group asks, in the order a run asks them, and the
# heading `verify` prints before them.
group_compiler_label="the compiler"
group_compiler_checks=(
  # Beside the deploy, because it is about the same command's first step. It
  # builds a second, much smaller project and deploys none of it: three of the
  # four targets are for platforms nothing here runs.
  check_every_compiler_target_is_built
  # And the other half of what the compiler is for: a project the standard
  # has no portable shape for is refused by the same command, at the same
  # step, before an application, environment or version exists for it.
  check_a_websocket_handler_is_refused_by_the_build
)
