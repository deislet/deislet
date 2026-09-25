# One group of `scripts/demo.sh verify`: certificates the proxy obtains from
# the stub authority by itself. Sourced by `scripts/demo.sh`.

# --- what the CLI itself proves -------------------------------------------
#
# The checks below are about `deis` rather than about the platform. They
# exist because the CLI is the only interface a developer has: the control
# plane can be perfectly correct while the one command anybody types is
# broken, and nothing else in this file would notice.

# ----------------------------------------------------------------------
# Automatic certificates
# ----------------------------------------------------------------------
#
# Five claims, and what each is judged on:
#
#   1. the names certified are the ones the control plane says are served -
#      judged on the authority's own record of what it issued;
#   2. a client that trusts the authority is served that certificate over the
#      proxy's TLS port, chosen by SNI - judged by curl completing a verified
#      handshake and the application answering;
#   3. the challenge path answers only what the proxy published - judged on
#      what the proxy answers for a token it never published and for a path
#      that tries to walk out of the challenge directory;
#   4. a certificate that is still current is not ordered again - judged on
#      the authority's count staying put across several renewal passes;
#   5. a second process over the same store reuses the account key and the
#      certificates - judged on the authority registering no second account
#      and issuing nothing more.

# Where the store keeps one name's files. Mirrors the layout in
# crates/deis-proxy/src/acme/store.rs: <storage>/<authority>/certificates/<name>/.
acme_certificate_file() {
  local name=$1
  local file=$2
  echo "${ACME_STORE_DIR}/127.0.0.1-${ACME_CA_PORT}/certificates/${name}/${file}"
}

# Poll until the authority reports it has issued for `$1` names, or give up.
# A real condition on every round - the authority's own count - and never a
# wait for a duration picked to be long enough.
wait_for_acme_issue() {
  local wanted=$1
  local attempts=${2:-60}
  local issued=0
  while [ "$attempts" -gt 0 ]; do
    issued=$(acme_ca_stats | jq -r '.issued | length' 2>/dev/null || echo 0)
    if [ "${issued:-0}" -ge "$wanted" ]; then
      return 0
    fi
    attempts=$((attempts - 1))
    poll_pause
  done
  return 1
}

check_acme_certificate_follows_the_routing_table() {
  local claim="the names certified are the ones the control plane says are served"

  if ! wait_for_acme_issue 2; then
    fail_check "$claim" \
      "the authority issued $(acme_ca_stats | jq -c '.issued') within the wait; the proxy log says: $(grep -a -i "certificate" "${LOG_DIR}/deis_proxy_error.log" 2>/dev/null | tail -n 3 | tr '\n' ' ')"
    return
  fi

  local issued
  issued=$(acme_ca_stats | jq -r '.issued | sort | join(", ")')
  case "$issued" in
    *"${ACME_PRODUCTION_NAME}"*)
      case "$issued" in
        *"${ACME_PREVIEW_NAME}"*) : ;;
        *)
          fail_check "$claim" "the authority issued for [${issued}], which does not include the preview environment's address ${ACME_PREVIEW_NAME}"
          return
          ;;
      esac
      ;;
    *)
      fail_check "$claim" "the authority issued for [${issued}], which does not include ${ACME_PRODUCTION_NAME}"
      return
      ;;
  esac

  # The names are nowhere in the proxy's configuration file. They were
  # synthesised from the applications and environments the control plane
  # publishes, which is what "derived rather than listed" means.
  if grep -q "$ACME_PRODUCTION_NAME" "$DEIS_PROXY_CONFIG"; then
    fail_check "$claim" \
      "${ACME_PRODUCTION_NAME} is written in ${DEIS_PROXY_CONFIG}, so it was configured rather than derived"
    return
  fi

  local name
  for name in "$ACME_PRODUCTION_NAME" "$ACME_PREVIEW_NAME"; do
    if [ ! -f "$(acme_certificate_file "$name" fullchain.pem)" ] \
      || [ ! -f "$(acme_certificate_file "$name" privkey.pem)" ]; then
      fail_check "$claim" \
        "the authority issued for ${name} but $(acme_certificate_file "$name" fullchain.pem) and its key are not both on disk, so a restart would order it again"
      return
    fi
  done

  # What the authority actually fetched from this proxy, and for which names.
  # The evidence line has always ended by saying the authority validated these
  # names over http-01, and nothing checked it: a stub that issued on request
  # without ever calling back would have left this list empty and the sentence
  # standing.
  local validated
  validated=$(acme_ca_stats | jq -r '[.validations[] | select(.matched) | .name] | sort | join(", ")')
  for name in "$ACME_PRODUCTION_NAME" "$ACME_PREVIEW_NAME"; do
    case ", ${validated}," in
      *", ${name},"*) ;;
      *)
        fail_check "$claim" \
          "the authority issued for [${issued}], but its record of the challenges it fetched back from this proxy does not name ${name}: [${validated:-none}]. A certificate nobody was challenged for says nothing about whether this proxy can answer for the name"
        return
        ;;
    esac
  done

  pass_check "$claim" \
    "the authority issued for [${issued}] and neither name appears anywhere in ${DEIS_PROXY_CONFIG}; both came from routing.preview_zone = \"${ACME_ZONE}\" applied to the app and environments the control plane publishes. It validated [${validated}] by fetching /.well-known/acme-challenge/<token> from the proxy's own HTTP port"
}

check_acme_certificate_is_served_over_tls() {
  local claim="a client that trusts the authority is served the issued certificate over TLS"
  local headers="${LOG_DIR}/verify-acme-tls.headers"
  local body status presented

  if [ ! -f "${ACME_CA_ROOT}" ]; then
    fail_check "$claim" "the authority's root was not written to ${ACME_CA_ROOT}, so nothing can verify the handshake"
    return
  fi

  # --cacert with nothing else: the handshake is verified against the stub
  # authority's root and against the SNI name. A proxy presenting a
  # self-signed certificate, or one for another name, fails here rather than
  # being reported as a success. --resolve is what stands in for the DNS
  # record that would point the name at this machine.
  # --noproxy '*' because every address in this demo is loopback and a
  # machine with https_proxy set would otherwise send this through it: curl
  # honours the proxy for a host name even when --resolve pins the address,
  # and the tunnel fails with an SSL error that looks like a certificate
  # problem and is not one.
  body=$(curl -s -D "$headers" -m 15 \
    --noproxy '*' \
    --cacert "${ACME_CA_ROOT}" \
    --resolve "${ACME_PRODUCTION_NAME}:${DEIS_PROXY_TLS_PORT}:127.0.0.1" \
    "https://${ACME_PRODUCTION_NAME}:${DEIS_PROXY_TLS_PORT}/" 2>"${LOG_DIR}/verify-acme-tls.log" || true)

  status=$(http_status "$headers" 2>/dev/null || echo "")
  if [ -z "$status" ]; then
    fail_check "$claim" \
      "the TLS handshake did not complete: $(tr '\n' ' ' < "${LOG_DIR}/verify-acme-tls.log" | head -c 300)"
    return
  fi

  # What was actually presented for that SNI name, printed as the evidence.
  # Read a second time through s_client rather than inferred from curl's
  # success, so the line says which certificate rather than only that one
  # worked.
  presented=$(openssl s_client -connect "127.0.0.1:${DEIS_PROXY_TLS_PORT}" \
    -servername "${ACME_PRODUCTION_NAME}" </dev/null 2>/dev/null \
    | openssl x509 -noout -subject -issuer -ext subjectAltName 2>/dev/null \
    | tr '\n' ' ' | tr -s ' ')

  case "$presented" in
    *"${ACME_PRODUCTION_NAME}"*) : ;;
    *)
      fail_check "$claim" "the certificate presented for SNI ${ACME_PRODUCTION_NAME} was: ${presented:-nothing}"
      return
      ;;
  esac

  if [ "$status" != "200" ]; then
    fail_check "$claim" \
      "the handshake verified but the application answered ${status}: $(echo "$body" | tr '\n' ' ' | head -c 200)"
    return
  fi

  pass_check "$claim" \
    "curl --cacert ${ACME_CA_ROOT} https://${ACME_PRODUCTION_NAME}:${DEIS_PROXY_TLS_PORT}/ verified the chain against the authority's root and got HTTP ${status}; the certificate chosen for that SNI name was ${presented}"
}

check_acme_challenge_answers_only_what_was_published() {
  local claim="the challenge path answers only tokens this instance published"
  local unknown traversal encoded

  unknown=$(curl -s -o /dev/null -w '%{http_code}' -m 5 --noproxy '*' \
    -H "Host: ${ACME_PRODUCTION_NAME}" \
    "http://127.0.0.1:${DEIS_PROXY_PORT}/.well-known/acme-challenge/never-published-token" || true)
  if [ "$unknown" != "404" ]; then
    fail_check "$claim" \
      "a token nothing published was answered ${unknown} rather than 404; an authority given anything else could believe a validation had succeeded"
    return
  fi

  # The token is a path segment chosen by whoever sends the request, and it
  # is used to name a file. Neither of these may read one.
  traversal=$(curl -s -m 5 --path-as-is --noproxy '*' \
    -H "Host: ${ACME_PRODUCTION_NAME}" \
    "http://127.0.0.1:${DEIS_PROXY_PORT}/.well-known/acme-challenge/../../../etc/passwd" | head -c 200 || true)
  encoded=$(curl -s -m 5 --path-as-is --noproxy '*' \
    -H "Host: ${ACME_PRODUCTION_NAME}" \
    "http://127.0.0.1:${DEIS_PROXY_PORT}/.well-known/acme-challenge/..%2f..%2f..%2fetc%2fpasswd" | head -c 200 || true)

  case "${traversal}${encoded}" in
    *root:*)
      fail_check "$claim" "a challenge path walked out of the challenge directory and read a file: ${traversal} ${encoded}"
      return
      ;;
  esac

  # The positive, without which the two refusals above prove nothing: a proxy
  # that answered 404 to every challenge path in existence would satisfy both
  # of them. This number was already printed in the evidence and never
  # asserted, which is a sentence about a real challenge answer resting on a
  # count that was allowed to be zero.
  # `|| true` and not `|| echo 0`: grep prints its own zero before exiting
  # non-zero, so the second form makes the value two lines long on exactly
  # the count this assertion is about.
  local served
  served=$(grep -a -c "an http-01 challenge answer was served" "${LOG_DIR}/deis_proxy_error.log" 2>/dev/null || true)
  if [ "${served:-0}" -lt 1 ]; then
    fail_check "$claim" \
      "the proxy answered 404 to a token nothing published and served no file for a traversal, but it has never served a real challenge answer either (${served} in ${LOG_DIR}/deis_proxy_error.log). A proxy that refused every challenge path would pass both halves above, so with nothing served there is no evidence the path answers what was published"
    return
  fi

  pass_check "$claim" \
    "the proxy served ${served} real challenge answer(s) to the authority, answered 404 to a token nothing published, and returned no file for ../../../etc/passwd in either its plain or its percent-encoded form"
}

# How many renewal passes this proxy has finished. Zero is a real answer here
# - it is what a proxy that has never got that far reports - so the count has
# to survive being zero, which `grep -c ... || echo 0` does not: grep prints
# its own zero first and the value becomes two lines.
acme_pass_count() {
  local counted
  counted=$(grep -a -c "automatic certificate pass finished" \
    "${LOG_DIR}/deis_proxy_error.log" 2>/dev/null || true)
  echo "${counted:-0}"
}

check_acme_does_not_reissue_a_current_certificate() {
  local claim="a certificate that is still current is not ordered again"
  local before after passes

  before=$(acme_ca_stats | jq -r '.issued | length')
  # Wait for the renewal loop to run several more passes. The condition is
  # the proxy's own count of finished passes, read from its log, not a
  # duration: check_interval_secs is 2 in this configuration.
  local start_passes attempts
  start_passes=$(acme_pass_count)
  passes=$start_passes
  attempts=30
  while [ "$attempts" -gt 0 ]; do
    passes=$(acme_pass_count)
    if [ "$((passes - start_passes))" -ge 3 ]; then
      break
    fi
    attempts=$((attempts - 1))
    poll_pause
  done

  after=$(acme_ca_stats | jq -r '.issued | length')
  if [ "$((passes - start_passes))" -lt 3 ]; then
    fail_check "$claim" \
      "only $((passes - start_passes)) renewal pass(es) ran while this check watched, so nothing was really observed; the proxy log is ${LOG_DIR}/deis_proxy_error.log"
    return
  fi
  if [ "$after" != "$before" ]; then
    fail_check "$claim" \
      "the authority issued ${before} certificate(s) before three more renewal passes and ${after} after; a current certificate is being reordered, which on a real authority spends the fifty a registered domain gets per week"
    return
  fi

  pass_check "$claim" \
    "the renewal loop ran $((passes - start_passes)) more passes (check_interval_secs = 2) and the authority's issued count stayed at ${after}; the passes found the certificates on disk with more than renew_before_days left and asked for nothing"
}

check_acme_restart_reuses_the_account_and_certificates() {
  local claim="a second process over the same store reuses the account key and the certificates"
  local config="${DEMO_DIR}/deis-proxy-restart.toml"
  local log="${LOG_DIR}/deis-proxy-restart.log"
  local error_log="${LOG_DIR}/deis_proxy_restart_error.log"
  local http_port tls_port
  local accounts_before issued_before accounts_after issued_after

  accounts_before=$(acme_ca_stats | jq -r '.accounts')
  issued_before=$(acme_ca_stats | jq -r '.issued | length')

  # Its own ports, its own database and its own pid file, but the same ACME
  # storage directory. That directory is the whole question: an operator's
  # restart is a new process reading what the last one left.
  claim_port http_port "$((DEIS_PROXY_PORT + 100))"
  claim_port tls_port "$((DEIS_PROXY_TLS_PORT + 100))"

  cat > "$config" <<EOF
listen_addr = ["127.0.0.1"]
listen_ports = [${http_port}]
database = "${DEMO_DIR}/deis-proxy-restart.db"
pid_file = "${DEMO_DIR}/deis-proxy-restart.pid"
upgrade_sock = "${DEMO_DIR}/deis_proxy_restart_upgrade.sock"

[[upstreams]]
host = "127.0.0.1"
port = ${DEIS_RUNTIME_PORT}

[log.error_log]
level = "info"
path = "${error_log}"

[control]
addr = "http://127.0.0.1:${DEIS_CONTROL_PORT}"
associated_id = "deis-proxy-demo-restart"

[machine_identity]
control_endpoint = "https://127.0.0.1:${DEIS_CONTROL_MACHINE_PORT}"
certificate = "${PKI_DIR}/proxy-restart.crt"
private_key = "${PKI_DIR}/proxy-restart.key"
trust_root = "${PKI_DIR}/ca.crt"
trust_domain = "${FLEET_TRUST_DOMAIN}"

[routing]
preview_zone = "${ACME_ZONE}"

[tls]
ports = [${tls_port}]

[tls.acme]
enabled = true
directory_url = "${ACME_CA_DIRECTORY:-http://127.0.0.1:${ACME_CA_PORT}/directory}"
contact_email = "ops@deislet.test"
terms_of_service_agreed = true
storage_dir = "${ACME_STORE_DIR}"
check_interval_secs = 2
retry_backoff_secs = 1
max_retry_backoff_secs = 4
include_preview_addresses = true
EOF
  # The same mode as every other generated file that carries a credential:
  # this one names the private key behind this proxy's own certificate under
  # [machine_identity], and docs/trust-boundaries.md says the demo writes
  # every such file 0600. This one was the exception
  # until a round read the directory listing against that sentence, and it
  # stays behind after the check (only the db and pid file are removed), so
  # the mode is for the life of .demo/ and not for the seconds the process
  # runs. scripts/test-demo-witnesses.sh holds every credential-bearing
  # writer in this file to it.
  chmod 600 "$config"

  rm -f "$error_log"
  "${BIN_DIR}/deis-proxy" -c "$config" > "$log" 2>&1 &
  local pid=$!

  # Wait for it to finish a renewal pass, which is the moment it has decided
  # whether anything needs ordering.
  local attempts=30
  local finished=0
  while [ "$attempts" -gt 0 ]; do
    if ! pid_running "$pid"; then
      break
    fi
    if grep -a -q "automatic certificate pass finished" "$error_log" 2>/dev/null; then
      finished=1
      break
    fi
    attempts=$((attempts - 1))
    poll_pause
  done

  accounts_after=$(acme_ca_stats | jq -r '.accounts')
  issued_after=$(acme_ca_stats | jq -r '.issued | length')

  local reused=0
  if grep -a -q "the stored ACME account key was loaded" "$error_log" 2>/dev/null; then
    reused=1
  fi

  stop_service "Restarted proxy" "$pid" >/dev/null 2>&1 || true
  rm -f "${DEMO_DIR}/deis-proxy-restart.db" "${DEMO_DIR}/deis-proxy-restart.pid"

  if [ "$finished" -ne 1 ]; then
    fail_check "$claim" \
      "the second process never finished a renewal pass; its log said: $(tail -n 5 "$error_log" 2>/dev/null | tr '\n' ' ' | head -c 300)"
    return
  fi
  if [ "$reused" -ne 1 ]; then
    fail_check "$claim" \
      "the second process did not report loading the stored account key, so it registered a new one; its log said: $(grep -a -i "account" "$error_log" 2>/dev/null | tail -n 3 | tr '\n' ' ' | head -c 300)"
    return
  fi
  if [ "$accounts_after" != "$accounts_before" ] || [ "$issued_after" != "$issued_before" ]; then
    fail_check "$claim" \
      "the authority went from ${accounts_before} account(s) and ${issued_before} certificate(s) to ${accounts_after} and ${issued_after}; a restart that re-registers or reissues spends limits that are counted per address and per registered domain"
    return
  fi

  pass_check "$claim" \
    "a second deis-proxy over ${ACME_STORE_DIR} logged \"the stored ACME account key was loaded\", finished a renewal pass, and left the authority at ${accounts_after} account and ${issued_after} certificates - it asked for nothing"
}

# The checks this group asks, in the order a run asks them, and the
# heading `verify` prints before them.
group_acme_label="automatic certificates"
group_acme_checks=(
  # Last, because the final check starts a second deis-proxy over the same
  # certificate store and registers it with the control plane; running that
  # before the fleet checks would put an extra instance in front of them.
  check_acme_certificate_follows_the_routing_table
  check_acme_certificate_is_served_over_tls
  check_acme_challenge_answers_only_what_was_published
  check_acme_does_not_reissue_a_current_certificate
  check_acme_restart_reuses_the_account_and_certificates
)
