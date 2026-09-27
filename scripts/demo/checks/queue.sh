# One group of `scripts/demo.sh verify`: queues - delivery, batches,
# concurrency, dead letters and the credential the queue answers for. Sourced
# by `scripts/demo.sh`.

check_queue_roundtrip() {
  local key="job-$$"
  local value="handled-by-the-queue-consumer"
  local headers="${LOG_DIR}/verify-queue.headers"

  local sent
  sent=$(runtime_curl -s -D "$headers" -X POST -H "x-deis-app-id: ${APP_ID}" \
    -H 'content-type: application/json' \
    -d "{\"key\":\"${key}\",\"value\":\"${value}\"}" \
    "$(runtime_data_url)/api/enqueue" || true)
  if [ "$(http_status "$headers")" != "202" ]; then
    fail_check "a queue message is produced and consumed" \
      "POST /api/enqueue answered $(http_status "$headers"): ${sent}"
    return
  fi

  # The consumer is a poller: it leases the batch on its own schedule, so the
  # only thing to do is wait for the effect it has. curl paces its own
  # retries a second apart and gives up after the budget below.
  local got
  got=$(runtime_curl -sf --retry 45 --retry-delay 1 --retry-max-time 90 --retry-all-errors \
    -H "x-deis-app-id: ${APP_ID}" \
    "$(runtime_data_url)/api/kv/job:${key}" 2>/dev/null || echo '')

  if [ "$(echo "$got" | jq -r '.value // empty' 2>/dev/null || true)" = "$value" ]; then
    pass_check "a queue message is produced and consumed" \
      "the handler sent it to deis-queue, the node's poller leased it, and the consumer wrote job:${key}"
  else
    fail_check "a queue message is produced and consumed" \
      "nothing was written for job:${key} within 90s; last answer: ${got:-<none>}"
  fi
}

# The producer half of queueing past one message at a time.
#
# Fifty messages sent with `send` in a loop are fifty op dispatches against
# this application's op-rate ceiling and fifty round trips to deis-queue, and
# there is no point at which the fifty are one thing: a handler cut off half
# way has enqueued half of them. `sendBatch` is one dispatch, one round trip
# and one transaction on the queue's side, so the array either arrives whole or
# not at all.
#
# What can be observed from outside is the two ends: one request goes in, and
# every message comes out of the consumer. The middle - that it was one call
# rather than five - is read off the handler's own log line, which only the
# `sendBatch` path writes.
check_queue_send_batch() {
  local title="a producer enqueues a whole array in one call and every message arrives"
  local stamp="batch-$$-${RANDOM}"
  local headers="${LOG_DIR}/verify-queue-batch.headers"
  local size=5

  local jobs="[" i
  for i in $(seq 1 $size); do
    [ "$i" = 1 ] || jobs="${jobs},"
    jobs="${jobs}{\"key\":\"${stamp}-${i}\",\"value\":\"batched-${i}\"}"
  done
  jobs="${jobs}]"

  # Taken before the request, so that the line read below is one this request
  # could have written and nothing earlier can be.
  local mark
  mark=$(node_log_mark)

  local sent
  sent=$(runtime_curl -s -D "$headers" -X POST -H "x-deis-app-id: ${APP_ID}" \
    -H 'content-type: application/json' -d "$jobs" \
    "$(runtime_data_url)/api/enqueue-batch" || true)
  if [ "$(http_status "$headers")" != "202" ]; then
    fail_check "$title" \
      "POST /api/enqueue-batch answered $(http_status "$headers"): ${sent}"
    return
  fi

  local queued
  queued=$(echo "$sent" | jq -r '.queued // empty' 2>/dev/null || true)

  # The consumer is a poller, so the only thing to do is wait for the effect.
  # curl paces its own retries a second apart, and only the first key needs the
  # long budget: the batch is one transaction and one delivery, so by the time
  # one of them has been handled the rest are in the same batch.
  local missing="" got
  for i in $(seq 1 $size); do
    got=$(runtime_curl -sf --retry 45 --retry-delay 1 --retry-max-time 90 --retry-all-errors \
      -H "x-deis-app-id: ${APP_ID}" \
      "$(runtime_data_url)/api/kv/job:${stamp}-${i}" 2>/dev/null || echo '')
    if [ "$(echo "$got" | jq -r '.value // empty' 2>/dev/null || true)" != "batched-${i}" ]; then
      missing="${missing} job:${stamp}-${i}"
    fi
  done

  # Written by the handler only on the `sendBatch` path; /api/enqueue logs one
  # line per message and never this one.
  # Written by the handler on the `sendBatch` path of the request this check
  # made a moment ago, so only the part of the log that request could have
  # written is read. The wording alone is a fact about the file: this check
  # runs more than once across runs against a long-lived node, and anything
  # that can reach /api/enqueue can put the sentence there directly.
  local batch_line
  batch_line=$(node_log_since "$mark" | grep -m 1 -F "queued ${size} job(s) in one batch" || true)

  if [ "$queued" = "$size" ] && [ -z "$missing" ] && [ -n "$batch_line" ]; then
    pass_check "$title" \
      "one POST to /api/enqueue-batch answered 202 with queued=${size}, the handler logged \"$(echo "$batch_line" | log_message)\", and the consumer wrote all ${size} keys job:${stamp}-1 .. job:${stamp}-${size}"
  else
    fail_check "$title" \
      "expected 202 with queued=${size} (got '${queued:-<none>}'), all ${size} keys written (missing:${missing:- none}) and the handler's one-batch line (${batch_line:+present}${batch_line:-never written})"
  fi
}

# What `maxConcurrency` means on a platform where one application in one
# environment is one isolate on one thread.
#
# It cannot mean parallelism, because the handler is serialised by the isolate
# whatever the number says. It means how many batches the node's poller may
# hold at once - leased, waiting their turn on the runtime thread, or being
# acknowledged - so the next batch is already in hand when the handler
# finishes. The number is only half of what has to be true; the other half is
# that the node says which of the two it is, because the name reads as
# parallelism on every other platform.
check_queue_consumer_concurrency() {
  local title="a queue consumer holds the batches its manifest asks for, and the node says that is not parallelism"

  local declared
  declared=$(jq -r '.queues[0].maxConcurrency // empty' "${APP_SRC_DIR}/.config.json" \
    2>/dev/null || true)
  [ -n "$declared" ] || declared=2

  # Startup evidence: written once, by the consumer task as it was spawned,
  # long before this phase began. The prefix that existed then is where it is,
  # and is a window nothing this phase provokes can reach into.
  local line applied
  line=$(node_log_before_verify \
    | grep -m 1 -F "Consuming queue jobs for app ${APP_ID}.${ENV_NAME}:" || true)
  applied=$(echo "$line" | sed -n 's/.*at most \([0-9]*\) batch(es).*/\1/p')

  # The line above is written once, by the consumer task as it is spawned, and
  # then it sits in the log for the rest of the run. So a check that read only
  # the line was reporting what the node *said when it started*, and this one
  # passed with deis-queue killed and its port free -- measured, not supposed.
  # The claim is about a consumer that is holding batches, which is the present
  # tense, so the present tense has to be asked: one message of this check's
  # own, enqueued now, and consumed now.
  #
  # Its own message rather than another check's: an assertion resting on what
  # some earlier check left behind is the same defect as one resting on a line
  # some earlier run left behind. The consumer writes the value under
  # `job:<key>` in KV, which is the effect an outsider cannot produce for this
  # key because nobody else knows it.
  local key="concurrency-$$-${RANDOM}"
  local value="consumed-while-this-check-watched"
  local headers="${LOG_DIR}/verify-queue-concurrency.headers"
  local sent
  sent=$(runtime_curl -s -D "$headers" -X POST -H "x-deis-app-id: ${APP_ID}" \
    -H 'content-type: application/json' \
    -d "{\"key\":\"${key}\",\"value\":\"${value}\"}" \
    "$(runtime_data_url)/api/enqueue" || true)
  local accepted
  accepted=$(http_status "$headers")

  # The consumer is a poller, so what there is to do is wait for its effect.
  # curl paces its own retries a second apart; nothing here sleeps.
  local got consumed=""
  if [ "$accepted" = "202" ]; then
    got=$(runtime_curl -sf --retry 45 --retry-delay 1 --retry-max-time 90 --retry-all-errors \
      -H "x-deis-app-id: ${APP_ID}" \
      "$(runtime_data_url)/api/kv/job:${key}" 2>/dev/null || echo '')
    consumed=$(echo "$got" | jq -r '.value // empty' 2>/dev/null || true)
  fi

  if [ "$applied" = "$declared" ] && [ -n "$line" ] \
    && echo "$line" | grep -q "Outstanding, not parallel" \
    && [ "$consumed" = "$value" ]; then
    pass_check "$title" \
      "the project asks for maxConcurrency ${declared} on queue jobs and the node reported \"$(echo "$line" | log_message)\". The consumer that line describes is the one running now: job:${key}, enqueued by this check, came back out of it"
  else
    fail_check "$title" \
      "expected the node to log that it holds ${declared} batch(es) of queue jobs at once, to say the number is not parallelism, and that consumer to still be consuming; it reported '${applied:-<none>}' in the line $(if [ -n "$line" ]; then echo "\"$(echo "$line" | log_message)\""; else echo "which was never written"; fi), POST /api/enqueue answered ${accepted:-nothing} and job:${key} read back '${consumed:-<nothing>}' within 90s"
  fi
}

# The dead letter table, from a terminal.
#
# A message that fails more times than the queue's ceiling allows is not
# deleted: deis-queue moves it to a dead letter table and stops delivering it.
# Until `deis queue` existed nothing outside deis-queue's own tests ever read
# that table, so the work in it was reachable only by opening queue.sqlite
# with the sqlite3 shell on whichever host holds it - which made the dead
# letter table a place work went to disappear rather than a place it waits.
#
# The poison needs no change to the application. A job whose key is longer
# than the 1024 bytes deis-store accepts (crates/deis-store/src/kv.rs) can
# never be written, so the consumer's put throws on every delivery, the
# handler asks for that message back, and the queue gives up on it once the
# attempts run out.
check_queue_dead_letters() {
  local title="a message the consumer cannot handle is dead lettered, and deis queue shows it and puts it back"
  local out="${LOG_DIR}/cli-queue-dead-letters"
  local headers="${LOG_DIR}/verify-queue-dead-letter.headers"
  local key
  key="poison-$$-${RANDOM}-$(printf '%1100s' '' | tr ' ' 'x')"

  local sent
  sent=$(runtime_curl -s -D "$headers" -X POST -H "x-deis-app-id: ${APP_ID}" \
    -H 'content-type: application/json' \
    -d "{\"key\":\"${key}\",\"value\":\"never-stored\"}" \
    "$(runtime_data_url)/api/enqueue" || true)
  if [ "$(http_status "$headers")" != "202" ]; then
    fail_check "$title" "POST /api/enqueue answered $(http_status "$headers"): ${sent}"
    return
  fi

  # The wait is real: the node hands the message back with a ten-second retry
  # delay and the queue only gives up at the fifth delivery, so this is around
  # a minute. Polled on the condition - the message appearing in the table -
  # rather than assumed, and every poll is the operator's own command; the
  # second between rounds is poll_pause, curl's retry pacing, like every
  # other poll in this file.
  local waited=0 found="" listed=""
  while [ "$waited" -lt 180 ]; do
    if deis_cli queue dead-letters --queue jobs --json \
      > "${out}-list.json" 2> "${out}-list.log"; then
      # Selected by body rather than by position: two verify runs against one
      # demo would each have their own poison message in this table.
      found=$(jq -r --arg key "$key" \
        '.messages[] | select((.body_base64 | @base64d) | contains($key)) | .id' \
        < "${out}-list.json" 2>/dev/null | head -n 1)
      if [ -n "$found" ]; then
        listed=$(jq -c --arg id "$found" '.messages[] | select(.id == $id)' \
          < "${out}-list.json" 2>/dev/null || true)
        break
      fi
    fi
    poll_pause
    waited=$((waited + 1))
  done

  if [ -z "$found" ]; then
    fail_check "$title" \
      "nothing was dead lettered for ${key:0:24}... within ${waited}s; the last listing was $(head -c 300 "${out}-list.json" 2>/dev/null || echo '<none>'), the last error $(tail -n 2 "${out}-list.log" 2>/dev/null | tr '\n' ' ')"
    return
  fi

  local attempts reason ceiling
  attempts=$(echo "$listed" | jq -r '.attempts // empty' 2>/dev/null || true)
  reason=$(echo "$listed" | jq -r '.reason // empty' 2>/dev/null || true)
  ceiling=3

  # The human report, which is what an operator actually reads.
  if ! deis_cli queue dead-letters --queue jobs > "${out}-report.txt" 2>&1; then
    fail_check "$title" \
      "deis queue dead-letters failed without --json: $(tail -n 3 "${out}-report.txt" | tr '\n' ' ')"
    return
  fi

  # And back out again. --delay is what an operator uses to put work back
  # while a fix is still rolling out; an hour here also keeps this demo's
  # consumer from spending the rest of the run re-poisoning itself.
  if ! deis_cli queue replay --queue jobs "$found" --delay 3600 --json \
    > "${out}-replay.json" 2> "${out}-replay.log"; then
    fail_check "$title" \
      "deis queue replay failed: $(tail -n 3 "${out}-replay.log" | tr '\n' ' ')"
    return
  fi
  local replayed not_found
  replayed=$(jq -r '.replayed // empty' < "${out}-replay.json" 2>/dev/null || true)
  not_found=$(jq -r '.not_found // empty' < "${out}-replay.json" 2>/dev/null || true)

  # It has to be gone from the table: the replay moves the row, it does not
  # copy it, and a message still listed here is one nobody has put back.
  local still_there=""
  if deis_cli queue dead-letters --queue jobs --json \
    > "${out}-after.json" 2> "${out}-after.log"; then
    still_there=$(jq -r --arg id "$found" '.messages[] | select(.id == $id) | .id' \
      < "${out}-after.json" 2>/dev/null || true)
  fi

  if [ "$attempts" = "$ceiling" ] \
    && [ "$reason" = "the consumer reported it failed" ] \
    && grep -q "$found" "${out}-report.txt" \
    && grep -q "the consumer reported it failed" "${out}-report.txt" \
    && [ "$replayed" = "1" ] && [ "$not_found" = "0" ] && [ -z "$still_there" ]; then
    pass_check "$title" \
      "the consumer failed the message ${ceiling} times over ${waited}s and the queue gave up on it; \`deis queue dead-letters\` showed it as ${found} with ${attempts} attempts and the reason \"${reason}\" - not \"its lease lapsed\", which is the other way a message gets here and needs a different fix - and \`deis queue replay --delay 3600\` put it back and left the table without it"
  else
    fail_check "$title" \
      "expected ${ceiling} attempts with the reason \"the consumer reported it failed\", the id and the reason in the human report, and a replay of 1 with 0 not found that empties the row; got attempts '${attempts:-<none>}', reason '${reason:-<none>}', replayed '${replayed:-<none>}', not_found '${not_found:-<none>}', still listed '${still_there:-<no>}'"
  fi
}

# The capability token on the queue, asked the one question nothing else in
# this demo asks it.
#
# Every queue check above reaches the queue through the node, which always
# carries the capability the control plane handed it with the deployment. So
# DEIS_QUEUE_PORT was configured, started, waited on and printed in the
# summary, and never dialled by anything without a credential: deis-queue's
# interceptor could have been left off the service and every queue check
# would still have been green. That is the hole check_storage_capability_token
# closes on deis-store; this is the queue's half of it
# (crates/deis-queue/src/auth.rs).
#
# ListDeadLetters is the call to ask it with: it is the operator's read of the
# dead letter table and changes nothing, so neither the refusal nor the served
# call moves a message the rest of the demo depends on. Four probes, because
# a refusal alone would also be what a queue that refuses everything looks
# like: no token, a garbled one, the control plane's deis-store token (a real
# signature for this very tenant, but not for deis-queue's audience), and the
# queue token the control plane mints for it.
check_queue_capability_token() {
  local title="a queue call presenting no capability token is refused"

  if ! command -v grpcurl >/dev/null 2>&1; then
    fail_check "$title" "grpcurl is not installed, so this check could not be run"
    return
  fi

  local tenant="${APP_ID}.${ENV_NAME}"
  local request='{"queue_name":"jobs","limit":1}'
  local none wrong other served none_rc wrong_rc other_rc served_rc

  none=$(grpcurl -plaintext -max-time 5 \
    -import-path "${ROOT_DIR}/crates/deis-queue/proto" -proto queue.proto \
    -H "x-deis-tenant: ${tenant}" \
    -d "$request" \
    "127.0.0.1:${DEIS_QUEUE_PORT}" deis.queue.Queue/ListDeadLetters 2>&1) && none_rc=0 || none_rc=$?

  wrong=$(grpcurl -plaintext -max-time 5 \
    -import-path "${ROOT_DIR}/crates/deis-queue/proto" -proto queue.proto \
    -H "x-deis-tenant: ${tenant}" \
    -H "authorization: Bearer not-a-capability" \
    -d "$request" \
    "127.0.0.1:${DEIS_QUEUE_PORT}" deis.queue.Queue/ListDeadLetters 2>&1) && wrong_rc=0 || wrong_rc=$?

  other=$(grpcurl -plaintext -max-time 5 \
    -import-path "${ROOT_DIR}/crates/deis-queue/proto" -proto queue.proto \
    -H "x-deis-tenant: ${tenant}" \
    -H "authorization: Bearer $(storage_token store)" \
    -d "$request" \
    "127.0.0.1:${DEIS_QUEUE_PORT}" deis.queue.Queue/ListDeadLetters 2>&1) && other_rc=0 || other_rc=$?

  served=$(grpcurl -plaintext -max-time 5 \
    -import-path "${ROOT_DIR}/crates/deis-queue/proto" -proto queue.proto \
    -H "x-deis-tenant: ${tenant}" \
    -H "authorization: Bearer $(storage_token queue)" \
    -d "$request" \
    "127.0.0.1:${DEIS_QUEUE_PORT}" deis.queue.Queue/ListDeadLetters 2>&1) && served_rc=0 || served_rc=$?

  if [ "$none_rc" -ne 0 ] && echo "$none" | grep -q 'Code: Unauthenticated' \
    && [ "$wrong_rc" -ne 0 ] && echo "$wrong" | grep -q 'Code: Unauthenticated' \
    && [ "$other_rc" -ne 0 ] && echo "$other" | grep -q 'Code: PermissionDenied' \
    && [ "$served_rc" -eq 0 ]; then
    pass_check "$title" \
      "ListDeadLetters at deis-queue's own port for tenant ${tenant} was refused with no Authorization header - $(echo "$none" | tr '\n' ' ' | sed 's/  */ /g' | head -c 120) - refused again with a garbled token, and refused as PermissionDenied with the control plane's genuine deis-store token for this tenant, while the same call carrying the queue capability the control plane mints was served. Nothing that can merely reach the queue's port can list this tenant's dead letters, and a token for one service does not open another. Drain and replay were not probed here: they sit behind the same interceptor (crates/deis-queue/src/server.rs wraps the whole service in it), which this check shows is switched on and does not show is complete"
    return
  fi

  fail_check "$title" \
    "expected Code: Unauthenticated with no token (grpcurl exited ${none_rc}, said $(echo "$none" | tr '\n' ' ' | head -c 140)), the same with a garbled one (exited ${wrong_rc}, said $(echo "$wrong" | tr '\n' ' ' | head -c 140)), PermissionDenied with the deis-store token (exited ${other_rc}, said $(echo "$other" | tr '\n' ' ' | head -c 140)), and the queue token to be served (exited ${served_rc}, said $(echo "$served" | tr '\n' ' ' | head -c 140))"
}

# The checks this group asks, in the order a run asks them, and the
# heading `verify` prints before them.
group_queue_label="queue"
group_queue_checks=(
  check_queue_roundtrip
  check_queue_send_batch
  check_queue_consumer_concurrency
  # Last of the queue checks that move messages: it is the only one that has
  # to wait for the attempt ceiling to be reached, and it leaves a message on
  # the queue with a long delay rather than in the dead letter table.
  check_queue_dead_letters
  # And the one that goes at deis-queue's own port instead of through the
  # node. It only reads the dead letter table, so it runs after the check that
  # fills and empties it and disturbs nothing either way.
  check_queue_capability_token
)
