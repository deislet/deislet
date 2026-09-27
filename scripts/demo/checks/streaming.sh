# One group of `scripts/demo.sh verify`: responses the node streams rather
# than collects, and the ceilings and cancellations that end them. Sourced by
# `scripts/demo.sh`.

# Read a streaming response over a raw socket, recording when each piece of the
# body arrived.
#
# A socket rather than curl because the whole claim is about *timing and
# framing*, and curl reports neither: what has to be shown is that the head
# arrived long before the last byte, that each frame arrived on its own, that
# the body is chunked, and -- for the failure cases -- that the terminating
# zero-length chunk never came. All four are visible here and nowhere else.
#
# Arguments: port, the value for the Host header, an app id or nothing, the
# request target, how long to keep reading (ms), how many frames to stop
# after (0 = until the body ends), and what to offer in Accept-Encoding.
#
# The Host value is what does the choosing on the proxy's port: the edge
# routes by host alone, so a call through it names the application's
# automatic address and carries no app id. A call to the node's data port
# names the application with `x-deis-app-id`, which is what that port reads;
# there the Host value is the address dialled, as any direct call's is.
#
# Stopping after N frames closes the socket where it stands, which is exactly
# what a client hanging up looks like to the node.
stream_exchange() {
  python3 - "$1" "$2" "$3" "$4" "$5" "$6" "$7" <<'PYSTREAM'
import json
import os
import socket
import ssl
import sys
import time

port = int(sys.argv[1])
host = sys.argv[2]
app_id = sys.argv[3]
target = sys.argv[4]
max_ms = int(sys.argv[5])
stop_after = int(sys.argv[6])
accept_encoding = sys.argv[7]

# Through deis-proxy the exchange is plaintext HTTP, as any visitor's is.
# Straight to a node's data port it is mutual TLS: the node verifies the
# caller's certificate names a proxy and closes the connection otherwise, so
# the harness presents the demo proxy's leaf and verifies the node against
# the fleet root. The paths come from the environment so the one helper can
# serve both.
tls_ca = os.environ.get("STREAM_MTLS_CA")
if tls_ca:
    context = ssl.create_default_context(cafile=tls_ca)
    context.load_cert_chain(
        os.environ["STREAM_MTLS_CERT"], os.environ["STREAM_MTLS_KEY"]
    )
else:
    context = None

report = {
    "status_line": "",
    "headers": {},
    "frames": [],
    "clean_end": False,
    "error": None,
}
started = time.monotonic()


def ms():
    return int((time.monotonic() - started) * 1000)


try:
    sock = socket.create_connection(("127.0.0.1", port), timeout=5)
    if context is not None:
        sock = context.wrap_socket(sock, server_hostname="127.0.0.1")
    app_header = "x-deis-app-id: %s\r\n" % app_id if app_id else ""
    request = (
        "GET %s HTTP/1.1\r\n"
        "Host: %s\r\n"
        "%s"
        "Accept-Encoding: %s\r\n"
        "Accept: text/event-stream\r\n"
        "Connection: close\r\n\r\n" % (target, host, app_header, accept_encoding)
    )
    sock.sendall(request.encode("utf-8"))
    sock.settimeout(0.2)

    buf = b""
    closed = False

    def read_more():
        # True: something arrived. False: the peer closed. None: nothing yet.
        global buf, closed
        try:
            data = sock.recv(65536)
        except socket.timeout:
            return None
        if not data:
            closed = True
            return False
        buf += data
        return True

    while b"\r\n\r\n" not in buf:
        if ms() > max_ms:
            raise RuntimeError("no response header within %dms" % max_ms)
        if read_more() is False:
            raise RuntimeError("the connection closed before any header arrived")

    head, buf = buf.split(b"\r\n\r\n", 1)
    lines = head.decode("latin1").split("\r\n")
    report["status_line"] = lines[0]
    report["header_ms"] = ms()
    for line in lines[1:]:
        if ":" in line:
            name, value = line.split(":", 1)
            report["headers"][name.strip().lower()] = value.strip()
    chunked = report["headers"].get("transfer-encoding", "").lower() == "chunked"
    report["chunked"] = chunked

    def record(payload):
        report["frames"].append(
            {"at_ms": ms(), "text": payload.decode("utf-8", "replace")}
        )

    while ms() <= max_ms:
        if stop_after and len(report["frames"]) >= stop_after:
            break
        if chunked:
            if b"\r\n" not in buf:
                if read_more() is False:
                    break
                continue
            size_line, rest = buf.split(b"\r\n", 1)
            try:
                size = int(size_line.split(b";")[0], 16)
            except ValueError:
                report["error"] = "unparsable chunk size %r" % size_line[:32]
                break
            if size == 0:
                report["clean_end"] = True
                break
            if len(rest) < size + 2:
                if read_more() is False:
                    break
                continue
            record(rest[:size])
            buf = rest[size + 2 :]
        else:
            if buf:
                record(buf)
                buf = b""
                continue
            state = read_more()
            if state is False:
                # No chunked framing: the body was delimited by the close, so
                # reaching it is the end of a complete body.
                report["clean_end"] = True
                break
    report["timed_out"] = ms() > max_ms
    sock.close()
except Exception as failure:
    report["error"] = "%s: %s" % (type(failure).__name__, failure)

print(json.dumps(report))
PYSTREAM
}

# The same exchange, straight to the node's data port - which speaks mutual
# TLS, so the harness carries the demo proxy's leaf and verifies the node
# against the fleet root. The certificates travel by environment because the
# python inside `stream_exchange` reads them from there; the wrapper is the
# only caller that sets them.
runtime_stream_exchange() {
  STREAM_MTLS_CA="${PKI_DIR}/ca.crt" \
    STREAM_MTLS_CERT="${PKI_DIR}/proxy.crt" STREAM_MTLS_KEY="${PKI_DIR}/proxy.key" \
    stream_exchange "$@"
}

# One field out of a streaming report, empty when it is not there.
stream_field() {
  jq -r "$2 // empty" < "$1" 2>/dev/null || true
}

# The frames a report collected, as "n@Xms" for the evidence string.
stream_frame_times() {
  jq -r '[.frames[] | "\(.at_ms)ms"] | join(", ")' < "$1" 2>/dev/null || true
}

check_server_sent_events_reach_the_client_frame_by_frame() {
  local title="server-sent events reach a client through deis-proxy frame by frame"
  local report="${LOG_DIR}/verify-sse.json"
  local frames=5
  local gap=150

  if ! command -v python3 >/dev/null 2>&1; then
    fail_check "$title" "python3 is not installed, so no stream was opened"
    return
  fi

  # Through the edge, named by Host like any visitor: the proxy routes by
  # host, and no request header selects an application.
  stream_exchange "$DEIS_PROXY_PORT" "${ACME_PRODUCTION_NAME}" "" \
    "${STREAM_ROUTE}?mode=sse&frames=${frames}&gapMs=${gap}&tag=sse" \
    6000 0 "gzip" > "$report" 2>"${LOG_DIR}/verify-sse.log" || true

  local error
  error=$(stream_field "$report" '.error')
  if [ -n "$error" ]; then
    fail_check "$title" "127.0.0.1:${DEIS_PROXY_PORT}${STREAM_ROUTE} -> ${error}"
    return
  fi

  local status ctype count first last header_ms clean
  status=$(stream_field "$report" '.status_line')
  ctype=$(stream_field "$report" '.headers["content-type"]')
  count=$(jq '.frames | length' < "$report")
  first=$(jq -r '.frames[0].at_ms // -1' < "$report")
  last=$(jq -r '.frames[-1].at_ms // -1' < "$report")
  header_ms=$(stream_field "$report" '.header_ms')
  clean=$(stream_field "$report" '.clean_end')

  # The gap the handler was asked for is what makes this checkable: with
  # ${frames} frames ${gap}ms apart the last one cannot exist until
  # $(( (frames - 1) * gap ))ms after the first, so a body that had been
  # collected before the head went out could not show this spread.
  local spread=$(( last - first ))
  local expected=$(( (frames - 1) * gap * 2 / 3 ))

  case "$status" in
    *200*) ;;
    *)
      fail_check "$title" "got \"${status}\" instead of HTTP 200; a demo started before this check existed serves 404 here - run \`demo.sh stop\` and \`demo.sh start\` again"
      return
      ;;
  esac

  if [ "$count" = "$frames" ] && [ "$clean" = "true" ] && [ "$spread" -ge "$expected" ]; then
    pass_check "$title" \
      "${status}, content-type ${ctype}; the head arrived at ${header_ms}ms and the ${count} frames at $(stream_frame_times "$report") - ${spread}ms apart end to end, for a handler that pauses ${gap}ms between them. The body ended with its terminating chunk, so the client saw a complete response"
  else
    fail_check "$title" \
      "expected ${frames} frames spread over at least ${expected}ms and a clean end; got ${count} frame(s) at $(stream_frame_times "$report"), spread ${spread}ms, clean_end=${clean}"
  fi
}

check_a_streamed_response_is_chunked_and_left_uncompressed() {
  local title="a streamed response crosses deis-proxy chunked and uncompressed"
  local report="${LOG_DIR}/verify-sse.json"

  if [ ! -s "$report" ]; then
    fail_check "$title" "the events check produced no report to read"
    return
  fi

  local chunked length encoding
  chunked=$(stream_field "$report" '.chunked')
  length=$(stream_field "$report" '.headers["content-length"]')
  encoding=$(stream_field "$report" '.headers["content-encoding"]')

  # Both halves matter and they fail differently. A Content-Length would mean
  # the node knew the size, which it can only do by having collected the body.
  # A Content-Encoding would mean the proxy compressed it - and pingora's
  # compressors are not flushed per chunk, so a 40-byte frame handed to gzip
  # comes back out as nothing and the client waits for bytes the proxy is
  # holding.
  if [ "$chunked" = "true" ] && [ -z "$length" ] && [ -z "$encoding" ]; then
    pass_check "$title" \
      "Transfer-Encoding: chunked, no Content-Length, and no Content-Encoding even though the request offered \"Accept-Encoding: gzip\" - so nothing between the isolate and the client is holding the body to measure or compress it"
  else
    fail_check "$title" \
      "chunked=${chunked}, Content-Length=\"${length}\", Content-Encoding=\"${encoding}\"; a length means the body was collected and an encoding means the proxy is buffering it inside a compressor"
  fi
}

check_a_stream_fed_from_fetch_is_not_collected() {
  local title="a body taken from fetch() is passed on without being collected"
  local report="${LOG_DIR}/verify-stream-relay.json"
  local pieces=6
  local gap=150

  if ! command -v python3 >/dev/null 2>&1; then
    fail_check "$title" "python3 is not installed, so no stream was opened"
    return
  fi
  if [ -z "${DEIS_ORIGIN_PORT:-}" ]; then
    fail_check "$title" "the demo's origin server is not running, so there is nothing to relay"
    return
  fi

  # The upstream writes its pieces slowly and announces no length. A handler
  # that read the body before answering could not send its first byte until the
  # upstream had sent its last, so the two timings below are the whole proof.
  local upstream="http%3A//127.0.0.1%3A${DEIS_ORIGIN_PORT}/slow%3Fpieces%3D${pieces}%26gapMs%3D${gap}"
  stream_exchange "$DEIS_PROXY_PORT" "${ACME_PRODUCTION_NAME}" "" \
    "${STREAM_ROUTE}?mode=relay&url=${upstream}" \
    8000 0 "gzip" > "$report" 2>"${LOG_DIR}/verify-stream-relay.log" || true

  local error
  error=$(stream_field "$report" '.error')
  if [ -n "$error" ]; then
    fail_check "$title" "${error}"
    return
  fi

  local status body count first last
  status=$(stream_field "$report" '.status_line')
  body=$(jq -r '[.frames[].text] | join("")' < "$report")
  count=$(jq '.frames | length' < "$report")
  first=$(jq -r '.frames[0].at_ms // -1' < "$report")
  last=$(jq -r '.frames[-1].at_ms // -1' < "$report")
  local spread=$(( last - first ))
  local expected=$(( (pieces - 1) * gap * 2 / 3 ))

  # Built the same way the body was read, so the trailing newline the upstream
  # sends is stripped from both sides rather than from one of them.
  local wanted
  wanted=$(seq 1 "$pieces" | sed 's/^/piece /')

  if [ "$status" != "${status#*200}" ] && [ "$body" = "$wanted" ] && [ "$spread" -ge "$expected" ]; then
    pass_check "$title" \
      "the handler returned new Response(upstream.body) and the ${pieces} pieces reached the client at $(stream_frame_times "$report") - ${spread}ms end to end, matching the pace the upstream wrote them at. The application's manifest asks for a 32MB heap and it never held the object at all"
  else
    fail_check "$title" \
      "expected ${pieces} pieces spread over at least ${expected}ms; got ${count} frame(s) spread ${spread}ms, status \"${status}\", body $(printf '%s' "$body" | tr '\n' '/' | head -c 200)"
  fi
}

check_a_stream_stops_when_the_client_hangs_up() {
  local title="a client that hangs up mid-stream stops the producer"
  local report="${LOG_DIR}/verify-stream-hangup.json"
  local tag="hangup-$$"

  if ! command -v python3 >/dev/null 2>&1; then
    fail_check "$title" "python3 is not installed, so no stream was opened"
    return
  fi

  # Two frames and then the socket is closed where it stands, which is what a
  # visitor closing a tab looks like from inside the node.
  local gap=80
  runtime_stream_exchange "$DEIS_RUNTIME_PORT" "127.0.0.1" "$APP_ID" \
    "${STREAM_ROUTE}?mode=endless&gapMs=${gap}&tag=${tag}" \
    6000 2 "identity" > "$report" 2>"${LOG_DIR}/verify-stream-hangup.log" || true

  local frames
  frames=$(jq '.frames | length' < "$report")
  if [ "$frames" -lt 2 ]; then
    fail_check "$title" "the stream produced ${frames} frame(s) before the client left, so there was nothing to interrupt: $(stream_field "$report" '.error')"
    return
  fi

  # Poll the application until its producer says it was cancelled. The isolate
  # is reused across requests, so the handler that was cancelled can be asked
  # about it by a later one; polling the fact itself rather than waiting a
  # fixed time is what keeps this from being a race in either direction.
  local waited=0 cancelled="" produced_first="" produced_second=""
  while [ "$waited" -lt 100 ]; do
    cancelled=$(app_request "${LOG_DIR}/verify-stream-hangup.headers" \
      "${STREAM_ROUTE}?mode=report" | jq -r '.lastCancelled // empty')
    [ "$cancelled" = "$tag" ] && break
    waited=$(( waited + 1 ))
  done

  if [ "$cancelled" != "$tag" ]; then
    fail_check "$title" \
      "the producer was never told its client had gone: after ${waited} polls the isolate still reports lastCancelled=\"${cancelled}\". It would go on filling a queue nobody reads until stream_timeout_ms"
    return
  fi

  # cancel() having run is one half. The other half - the producer stopped -
  # is measured, not inferred from it, because a runtime whose cancel() fires
  # while its pull loop goes on writing into a closed stream would pass the
  # first half with the sentence below intact.
  #
  # The measurement has to be longer than the producer's period. It writes a
  # frame every ${gap}ms, so two reads of its counter tens of milliseconds
  # apart are equal whether or not it is still running - measured at 8 of 8
  # trials against a producer known to be alive - and say nothing. A producer
  # has stopped when its counter does not move across an interval longer than
  # its period: read the counter, let a full second pass (poll_pause, which is
  # curl's own retry pacing and not a shell sleep - twelve periods), read it
  # again. One frame may still land after cancel(): the pull that was already
  # past its timer when the client left. So a counter that moved is given
  # another interval before it is called a running producer; a live one moves
  # in every interval and never settles.
  local rounds=0 settled="" produced_first="" produced_second=""
  while [ "$rounds" -lt 3 ]; do
    produced_first=$(app_request "${LOG_DIR}/verify-stream-hangup.headers" \
      "${STREAM_ROUTE}?mode=report" | jq -r '.framesProduced')
    poll_pause
    produced_second=$(app_request "${LOG_DIR}/verify-stream-hangup.headers" \
      "${STREAM_ROUTE}?mode=report" | jq -r '.framesProduced')
    rounds=$(( rounds + 1 ))
    if [ -n "$produced_first" ] && [ "$produced_first" = "$produced_second" ]; then
      settled=yes
      break
    fi
  done

  if [ "$settled" = "yes" ]; then
    pass_check "$title" \
      "the client read ${frames} frame(s) and closed the socket; the handler's cancel() ran with tag ${tag}, and the frame counter stood still at ${produced_first} across a full second - more than twelve of its ${gap}ms periods, in round ${rounds} of at most 3 - so the producer stopped rather than writing into nothing"
  else
    fail_check "$title" \
      "cancel() ran, but the producer kept going: the frame counter moved from ${produced_first} to ${produced_second} across a full second after the client had gone, and did so in each of ${rounds} round(s)"
  fi
}

check_a_producer_that_throws_leaves_a_failed_transfer() {
  local title="a producer that throws part-way leaves the client a failed transfer"
  local report="${LOG_DIR}/verify-stream-throw.json"

  if ! command -v python3 >/dev/null 2>&1; then
    fail_check "$title" "python3 is not installed, so no stream was opened"
    return
  fi

  stream_exchange "$DEIS_PROXY_PORT" "${ACME_PRODUCTION_NAME}" "" \
    "${STREAM_ROUTE}?mode=throw&after=2&gapMs=80&tag=throw" \
    6000 0 "identity" > "$report" 2>"${LOG_DIR}/verify-stream-throw.log" || true

  local status frames clean
  status=$(stream_field "$report" '.status_line')
  frames=$(jq '.frames | length' < "$report")
  clean=$(stream_field "$report" '.clean_end')

  # 200 and an incomplete body is the only honest ending available: the status
  # line left the node before the handler failed, so there is no 500 left to
  # send. What must not happen is a *clean* end, which the client would read as
  # a short but complete answer.
  if [ "$status" != "${status#*200}" ] && [ "$frames" -eq 2 ] && [ "$clean" != "true" ]; then
    pass_check "$title" \
      "${status} went out first, the 2 frames the handler managed arrived at $(stream_frame_times "$report"), and then the body ended without its terminating chunk - so the client's read fails instead of succeeding on half an answer"
  else
    fail_check "$title" \
      "expected HTTP 200, 2 frames and no terminating chunk; got \"${status}\", ${frames} frame(s), clean_end=${clean}"
  fi
}

# How many times this node has said, at warn, that it closed *one named
# invocation's* stream for running past its budget.
#
# By invocation, not by wording. Several checks here leave a stream behind
# that meets the same fate and is logged in the same words, so the count of
# lines that match the wording says nothing about any one of them - and,
# worse, it is a count over a file anything reaching the node can add to. See
# `trace_of_witness`. `$1` is the invocation id the stream belonged to; a
# count over that is a count of what the node said about this stream and
# nothing else.
stream_budget_log_lines() {
  local trace=$1
  if [ -z "$trace" ]; then
    echo 0
    return 0
  fi
  node_log | grep " WARN " | grep "is past stream_timeout_ms" \
    | lines_of_invocation "$trace" | grep -c . || true
}

check_a_stream_past_its_budget_is_closed_by_the_node() {
  local title="a stream that runs past stream_timeout_ms is closed by the node"
  local report="${LOG_DIR}/verify-stream-budget.json"
  # A tag this run invents. It reaches the handler, which logs it, and nothing
  # else knows it - which is what makes the node's line about this stream
  # findable and what stops any other line from standing in for it. `$$` alone
  # was not enough even before anybody was being adversarial: two `verify` runs
  # against one demo can be the same shell.
  local tag="budget-$$-${RANDOM}"

  if ! command -v python3 >/dev/null 2>&1; then
    fail_check "$title" "python3 is not installed, so no stream was opened"
    return
  fi

  # Read for well past the ceiling. The node is what has to end this, and the
  # client keeps asking until it does.
  local patience=$(( DEMO_STREAM_TIMEOUT_MS * 3 ))
  runtime_stream_exchange "$DEIS_RUNTIME_PORT" "127.0.0.1" "$APP_ID" \
    "${STREAM_ROUTE}?mode=endless&gapMs=120&tag=${tag}" \
    "$patience" 0 "identity" > "$report" 2>"${LOG_DIR}/verify-stream-budget.log" || true

  local frames last clean timed_out
  frames=$(jq '.frames | length' < "$report")
  last=$(jq -r '.frames[-1].at_ms // -1' < "$report")
  clean=$(stream_field "$report" '.clean_end')
  timed_out=$(stream_field "$report" '.timed_out')

  local floor=$(( DEMO_STREAM_TIMEOUT_MS / 2 ))
  local ceiling=$(( DEMO_STREAM_TIMEOUT_MS * 2 ))
  # Which invocation this stream was, read off the marker the handler logged
  # under the tag above. Everything below is then asked about that invocation
  # rather than about the wording of a line, so neither a second `verify` run,
  # nor the parallel-request check's abandoned stream, nor a request from
  # outside can supply the evidence this check reports as its own.
  local trace
  trace=$(trace_of_witness "stream witness ${tag}")
  if [ -z "$trace" ]; then
    fail_check "$title" \
      "the node logged no 'stream witness ${tag}' line, so this check cannot tell which invocation its own stream was and will not read another one's evidence as its own; the log is ${LOG_DIR}/deis-runtime.log"
    return
  fi

  # Only the lines this invocation caused, and only lines carrying the level
  # the evidence claims. `stream_budget_log_lines` counts at warn, so a line
  # demoted to debug - which is where a message stops being something an
  # operator sees - fails here rather than passing on the wording alone.
  #
  # `|| true` on the quotation: a grep that matches nothing exits 1, pipefail
  # makes that the pipeline's status, and an unguarded assignment under
  # `set -e` ends the run right here - with no line for this check, no
  # summary and no clue, which is what a run that had merely not finished
  # yet looks like. The line being absent is one of the two outcomes this
  # check exists to tell apart, so it has to reach the `if` below rather
  # than kill the script on the way; the failure branch already prints
  # `${said:-<none>}` for it.
  local logged said
  logged=$(stream_budget_log_lines "$trace")
  said=$(node_log | grep " WARN " | grep "is past stream_timeout_ms" \
    | lines_of_invocation "$trace" | tail -n 1 || true)

  if [ "$timed_out" != "true" ] && [ "$clean" != "true" ] \
    && [ "$last" -ge "$floor" ] && [ "$last" -le "$ceiling" ] && [ "$logged" -ge 1 ]; then
    pass_check "$title" \
      "an endless stream ran for ${last}ms against the ${DEMO_STREAM_TIMEOUT_MS}ms ceiling this node is configured with, delivered ${frames} frame(s), and was then ended without its terminating chunk. The node wrote ${logged} line(s) at warn about invocation ${trace} - the one this check started, and no other - the last of them: \"$(printf '%s' "$said" | log_message | head -c 200)\""
  else
    fail_check "$title" \
      "expected the node to close the stream between ${floor}ms and ${ceiling}ms and to say so at warn; last frame at ${last}ms, clean_end=${clean}, client gave up=${timed_out}, warn line(s) about the ceiling filed under this check's own invocation ${trace}=${logged}"
  fi
}

check_a_request_is_served_while_a_stream_is_open() {
  local title="another request is answered while a stream is still open"
  local report="${LOG_DIR}/verify-stream-parallel.json"
  local tag="parallel-$$"

  if ! command -v python3 >/dev/null 2>&1; then
    fail_check "$title" "python3 is not installed, so no stream was opened"
    return
  fi

  # The application counts every frame it has ever produced, so "the stream is
  # producing right now" is a fact that can be asked for rather than waited
  # out. Each poll below is a real request to the same isolate, which is also
  # what paces the loop.
  local before
  before=$(app_request "${LOG_DIR}/verify-stream-parallel.headers" \
    "${STREAM_ROUTE}?mode=report" | jq -r '.framesProduced')

  # Held open in the background for long enough to make an ordinary request
  # underneath it. Both go to the same application, which on this node is one
  # isolate on one thread -- the arrangement that would let a slow reader block
  # everybody if a stream owned the isolate for its lifetime.
  runtime_stream_exchange "$DEIS_RUNTIME_PORT" "127.0.0.1" "$APP_ID" \
    "${STREAM_ROUTE}?mode=endless&gapMs=200&tag=${tag}" \
    4000 0 "identity" > "$report" 2>"${LOG_DIR}/verify-stream-parallel.log" &
  local holder=$!

  local waited=0 producing=""
  while [ "$waited" -lt 60 ]; do
    producing=$(app_request "${LOG_DIR}/verify-stream-parallel.headers" \
      "${STREAM_ROUTE}?mode=report" | jq -r '.framesProduced')
    [ -n "$producing" ] && [ "$producing" -gt "$before" ] && break
    waited=$(( waited + 1 ))
  done

  if [ -z "$producing" ] || [ "$producing" -le "$before" ]; then
    kill "$holder" 2>/dev/null || true
    wait "$holder" 2>/dev/null || true
    fail_check "$title" \
      "the stream never started producing: the frame counter stayed at ${before} across ${waited} requests. $(stream_field "$report" '.error')"
    return
  fi

  local headers="${LOG_DIR}/verify-stream-parallel.headers"
  local started ended body status length
  started=$(date +%s%N)
  body=$(app_request "$headers" "/hello/parallel")
  ended=$(date +%s%N)
  status=$(http_status "$headers")
  length=$(header_value "$headers" "content-length")
  local took=$(( (ended - started) / 1000000 ))

  # Still running means the stream was open for the whole of the request above,
  # which is the claim: the answer did not have to wait for the stream to end.
  local still_open=no
  kill -0 "$holder" 2>/dev/null && still_open=yes

  wait "$holder" 2>/dev/null || true
  local frames
  frames=$(jq '.frames | length' < "$report")

  if [ "$status" = "200" ] && [ "$still_open" = "yes" ] && [ -n "$length" ]; then
    pass_check "$title" \
      "the isolate had a stream open and producing (the frame counter had moved from ${before} to ${producing}) when GET /hello/parallel answered ${status} in ${took}ms with Content-Length ${length} - an ordinary answer on the ordinary road. The stream was still open afterwards and went on to deliver ${frames} frames"
  else
    fail_check "$title" \
      "GET /hello/parallel answered \"${status}\" in ${took}ms (Content-Length \"${length}\", stream still open: ${still_open}) while the stream delivered ${frames} frame(s); a stream that owned the isolate would have queued this request behind it. Body: $(printf '%s' "$body" | head -c 200)"
  fi
}

# The checks this group asks, in the order a run asks them, and the
# heading `verify` prints before them.
group_streaming_label="streaming responses"
group_streaming_checks=(
  # The first two go through deis-proxy, which is the only hop that could
  # quietly collect a body: everything else addresses the node directly, so a
  # proxy that buffered one would leave every other check here green.
  check_server_sent_events_reach_the_client_frame_by_frame
  check_a_streamed_response_is_chunked_and_left_uncompressed
  check_a_stream_fed_from_fetch_is_not_collected
  check_a_producer_that_throws_leaves_a_failed_transfer
  check_a_stream_stops_when_the_client_hangs_up
  check_a_request_is_served_while_a_stream_is_open
  # Last of the streaming checks: it is the only one that waits, because the
  # thing being watched is a ceiling running out.
  check_a_stream_past_its_budget_is_closed_by_the_node
)
