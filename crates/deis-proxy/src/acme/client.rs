//! The RFC 8555 exchange, from the directory to a certificate in hand.
//!
//! One order per name. The alternative - one order carrying every name as a
//! subject alternative name - makes fewer requests, and was rejected for a
//! reason that matters on a self-hosted box: a single name whose DNS is not
//! pointed here yet fails the whole order, so every other name loses its
//! certificate too. On a platform where somebody adds a domain in the
//! morning and points the DNS record at lunchtime, that would be the normal
//! case rather than the exception. Per-name orders cost one certificate out
//! of the authority's weekly allowance each, which is what
//! `tls.acme.max_names` is there to bound.
//!
//! `http-01` is the only challenge implemented. `dns-01` is the other one an
//! authority offers and it is not here on purpose: it needs credentials for
//! whichever DNS provider holds the zone, one integration per provider, and
//! a second thing to rotate and to break. This proxy already answers port 80
//! for every name it serves, so `http-01` needs nothing that is not already
//! running. What that costs is wildcards - RFC 8555 section 7.1.6 issues
//! those only against `dns-01` - so a preview zone still wants one wildcard
//! certificate placed by hand.

use std::time::{Duration, Instant};

use openssl::ec::{EcGroup, EcKey};
use openssl::hash::MessageDigest;
use openssl::nid::Nid;
use openssl::pkey::{PKey, Private};
use openssl::stack::Stack;
use openssl::x509::extension::SubjectAlternativeName;
use openssl::x509::{X509Extension, X509Req, X509ReqBuilder};
use serde_json::{Value as JsonValue, json};
use tracing::{debug, info};

use super::account::AccountKey;
use super::challenge::ChallengeResponder;
use super::error::AcmeError;
use super::http::{AcmeHttp, AcmeResponse};
use super::jose::{KeyId, base64url, sign_request};
use super::status::{AuthorizationStatus, OrderStatus};

/// How long to wait between polls when the authority does not say.
const DEFAULT_POLL_INTERVAL: Duration = Duration::from_secs(2);
/// Ceiling on a `Retry-After` this client will honour. An authority under
/// load asking for a minute is normal; one asking for a day would park the
/// renewal for a name that may be days from expiry, so the wait is capped
/// and the order retried from the start instead.
const MAX_POLL_INTERVAL: Duration = Duration::from_secs(60);

/// A freshly issued certificate and the key it was issued to.
pub struct IssuedCertificate {
    /// Leaf first, then the chain the authority sent, as PEM.
    pub chain_pem: Vec<u8>,
    pub key_pem: Vec<u8>,
}

/// The resources the authority publishes.
#[derive(Debug)]
struct Directory {
    new_nonce: String,
    new_account: String,
    new_order: String,
}

impl Directory {
    fn parse(body: &JsonValue) -> Result<Self, AcmeError> {
        let field = |name: &str| {
            body.get(name)
                .and_then(JsonValue::as_str)
                .map(str::to_string)
                .ok_or_else(|| {
                    AcmeError::protocol(
                        "the directory",
                        format!("it names no {name} resource, so this is not an ACME directory"),
                    )
                })
        };
        Ok(Self {
            new_nonce: field("newNonce")?,
            new_account: field("newAccount")?,
            new_order: field("newOrder")?,
        })
    }
}

/// A client bound to one authority and one account key.
pub struct AcmeClient {
    http: AcmeHttp,
    directory: Directory,
    key: AccountKey,
    /// The account URL, once registered. Every request after registration
    /// names it instead of carrying the key.
    account_url: Option<String>,
    /// The most recent unused nonce. ACME hands one out with almost every
    /// answer, so the common path spends no request fetching one.
    nonce: Option<String>,
    order_timeout: Duration,
}

impl AcmeClient {
    /// Fetch the directory and get ready to talk.
    pub async fn connect(
        directory_url: &str,
        key: AccountKey,
        account_url: Option<String>,
        request_timeout: Duration,
        order_timeout: Duration,
    ) -> Result<Self, AcmeError> {
        let http = AcmeHttp::new(request_timeout);
        let response = http.get(directory_url).await?;
        let body = expect_json(&response, "the directory")?;
        if response.status != 200 {
            return Err(problem(&response, "the directory"));
        }
        let directory = Directory::parse(&body)?;
        debug!(
            new_order = %directory.new_order,
            "certificate authority directory read"
        );
        Ok(Self {
            http,
            directory,
            key,
            account_url,
            nonce: None,
            order_timeout,
        })
    }

    pub fn account_url(&self) -> Option<&str> {
        self.account_url.as_deref()
    }

    /// Register the account key, or confirm the registration already stored.
    ///
    /// `onlyReturnExisting` is not used: sending the registration again is
    /// how RFC 8555 section 7.3 says to look an account up, and the
    /// authority answers 200 with the same URL rather than creating a second
    /// one. That makes a lost `account.url` file recoverable without a new
    /// registration and without spending a registration limit.
    pub async fn ensure_account(
        &mut self,
        contact_email: &str,
        terms_agreed: bool,
    ) -> Result<(), AcmeError> {
        let payload = json!({
            "termsOfServiceAgreed": terms_agreed,
            "contact": [format!("mailto:{contact_email}")],
        })
        .to_string();

        let url = self.directory.new_account.clone();
        let response = self.post(&url, &payload, None).await?;
        if response.status != 200 && response.status != 201 {
            return Err(problem(&response, "the account registration"));
        }
        let account_url = response
            .header("location")
            .ok_or_else(|| {
                AcmeError::protocol(
                    "the account registration",
                    "it succeeded but sent no Location header, so there is no account URL to sign \
                     later requests with",
                )
            })?
            .to_string();

        let created = response.status == 201;
        if self.account_url.as_deref() != Some(account_url.as_str()) {
            info!(
                account = %account_url,
                created,
                contact = %contact_email,
                "ACME account in use; the authority writes to this address when a certificate is \
                 close to expiring and has not been renewed"
            );
        }
        self.account_url = Some(account_url);
        Ok(())
    }

    /// Obtain one certificate for one name.
    pub async fn issue(
        &mut self,
        name: &str,
        responder: &ChallengeResponder,
    ) -> Result<IssuedCertificate, AcmeError> {
        let started = Instant::now();
        let account = self.require_account()?;

        let payload = json!({
            "identifiers": [{ "type": "dns", "value": name }],
        })
        .to_string();
        let new_order = self.directory.new_order.clone();
        let response = self.post(&new_order, &payload, Some(account)).await?;
        if response.status != 201 {
            return Err(problem(&response, &format!("the order for {name}")));
        }
        let order_url = response
            .header("location")
            .ok_or_else(|| {
                AcmeError::protocol(
                    format!("the order for {name}"),
                    "it was created but sent no Location header, so it cannot be polled",
                )
            })?
            .to_string();
        let mut order = expect_json(&response, &format!("the order for {name}"))?;
        info!(name, order = %order_url, "certificate order created");

        for authorization in string_array(&order, "authorizations") {
            self.solve_authorization(name, &authorization, responder, started)
                .await?;
        }

        // Every identifier is validated; ask for the certificate.
        let finalize = order
            .get("finalize")
            .and_then(JsonValue::as_str)
            .ok_or_else(|| {
                AcmeError::protocol(
                    format!("the order for {name}"),
                    "it names no finalize resource",
                )
            })?
            .to_string();

        let (certificate_key, csr) = certificate_request(name)?;
        let payload = json!({ "csr": base64url(&csr) }).to_string();
        let response = self.post_as_account(&finalize, &payload).await?;
        if response.status != 200 && response.status != 201 {
            return Err(problem(
                &response,
                &format!("finalizing the order for {name}"),
            ));
        }
        order = expect_json(&response, &format!("finalizing the order for {name}"))?;

        // The authority signs asynchronously; the order carries the
        // certificate URL only once it is `valid`.
        //
        // The finalize answer is where this client starts watching the
        // order's status, not where the order started. An order is created
        // `pending` and moves to `ready` when its authorizations all pass,
        // and that step happens between two requests this client makes: it
        // posts finalize without re-reading the order first, so it never sees
        // `ready`. Seeding [`OrderStatus`] from the finalize answer is what
        // keeps a step this client did not observe from looking like a step
        // the authority was not allowed to make.
        let mut status = order_status(&order, name)?;
        let mut interval = response.retry_after().unwrap_or(DEFAULT_POLL_INTERVAL);
        while !status.is_settled() {
            self.wait_before_next_poll(name, status.as_str(), started, interval)
                .await?;
            let response = self.post_as_account(&order_url, "").await?;
            interval = response.retry_after().unwrap_or(DEFAULT_POLL_INTERVAL);
            order = expect_json(&response, &format!("the order for {name}"))?;

            let reported = order_status(&order, name)?;
            status = status.advance(reported).map_err(|step| {
                AcmeError::protocol(format!("the order for {name}"), step.to_string())
            })?;
        }

        if status == OrderStatus::Invalid {
            return Err(AcmeError::ChallengeFailed {
                name: name.to_string(),
                detail: order_problem(&order),
            });
        }

        let certificate_url = order
            .get("certificate")
            .and_then(JsonValue::as_str)
            .ok_or_else(|| {
                AcmeError::protocol(
                    format!("the order for {name}"),
                    "it is valid but names no certificate to download",
                )
            })?
            .to_string();
        let response = self.post_as_account(&certificate_url, "").await?;
        if response.status != 200 {
            return Err(problem(
                &response,
                &format!("downloading the certificate for {name}"),
            ));
        }
        if !response.body.starts_with(b"-----BEGIN CERTIFICATE-----") {
            return Err(AcmeError::protocol(
                format!("the certificate for {name}"),
                "the download is not a PEM certificate chain".to_string(),
            ));
        }

        let key_pem = certificate_key
            .private_key_to_pem_pkcs8()
            .map_err(|e| AcmeError::Crypto(format!("serialising the certificate key: {e}")))?;
        info!(
            name,
            elapsed_secs = started.elapsed().as_secs(),
            chain_bytes = response.body.len(),
            "certificate issued"
        );
        Ok(IssuedCertificate {
            chain_pem: response.body,
            key_pem,
        })
    }

    /// Publish the answer for one authorization and wait for the authority's
    /// verdict on it.
    async fn solve_authorization(
        &mut self,
        name: &str,
        authorization_url: &str,
        responder: &ChallengeResponder,
        started: Instant,
    ) -> Result<(), AcmeError> {
        let response = self.post_as_account(authorization_url, "").await?;
        if response.status != 200 {
            return Err(problem(&response, &format!("the authorization for {name}")));
        }
        let authorization = expect_json(&response, &format!("the authorization for {name}"))?;

        // Already valid: the authority reuses a recent validation for the
        // same name, which is why renewing usually makes no challenge at all.
        if authorization_status(&authorization).ok() == Some(AuthorizationStatus::Valid) {
            debug!(
                name,
                "the authority already holds a valid authorization for this name"
            );
            return Ok(());
        }

        let challenge = authorization
            .get("challenges")
            .and_then(JsonValue::as_array)
            .and_then(|challenges| {
                challenges.iter().find(|challenge| {
                    challenge.get("type").and_then(JsonValue::as_str) == Some("http-01")
                })
            })
            .ok_or_else(|| {
                AcmeError::protocol(
                    format!("the authorization for {name}"),
                    "it offers no http-01 challenge. This client answers only http-01, on port \
                     80; an authority that offers only dns-01 for this identifier - which is what \
                     a wildcard name gets - cannot be satisfied here"
                        .to_string(),
                )
            })?;

        let challenge_url = challenge
            .get("url")
            .and_then(JsonValue::as_str)
            .ok_or_else(|| {
                AcmeError::protocol(
                    format!("the http-01 challenge for {name}"),
                    "it names no URL to accept",
                )
            })?
            .to_string();
        let token = challenge
            .get("token")
            .and_then(JsonValue::as_str)
            .ok_or_else(|| {
                AcmeError::protocol(
                    format!("the http-01 challenge for {name}"),
                    "it carries no token",
                )
            })?
            .to_string();

        let key_authorization = self.key.key_authorization(&token)?;
        // Published before the authority is told to look, and withdrawn
        // however this ends - including on the error paths below, which is
        // why the result is held rather than returned straight away.
        responder.publish(name, &token, &key_authorization);
        info!(
            name,
            "http-01 answer published; the authority will now fetch \
             http://{name}/.well-known/acme-challenge/<token> from whatever that name resolves to"
        );

        let outcome = self
            .await_validation(name, &challenge_url, authorization_url, started)
            .await;
        responder.withdraw(&token);
        outcome
    }

    /// Tell the authority to validate, then poll the authorization until it
    /// stops being pending.
    async fn await_validation(
        &mut self,
        name: &str,
        challenge_url: &str,
        authorization_url: &str,
        started: Instant,
    ) -> Result<(), AcmeError> {
        // An empty JSON object is what section 7.5.1 says to POST to accept
        // a challenge; it is not the same as POST-as-GET's empty payload.
        let response = self.post_as_account(challenge_url, "{}").await?;
        if response.status != 200 && response.status != 202 {
            return Err(problem(
                &response,
                &format!("accepting the challenge for {name}"),
            ));
        }
        let mut interval = response.retry_after().unwrap_or(DEFAULT_POLL_INTERVAL);

        // The challenge has just been accepted, so the authorization is
        // pending by definition. Everything after this is the authority's
        // answer, and the table decides which answers may follow it.
        let mut status = AuthorizationStatus::Pending;
        loop {
            self.wait_before_next_poll(name, status.as_str(), started, interval)
                .await?;
            let response = self.post_as_account(authorization_url, "").await?;
            if response.status != 200 {
                return Err(problem(&response, &format!("the authorization for {name}")));
            }
            interval = response.retry_after().unwrap_or(DEFAULT_POLL_INTERVAL);
            let authorization = expect_json(&response, &format!("the authorization for {name}"))?;

            // A status this client does not know, and a step the RFC does not
            // allow, are both the authority saying the name will not be
            // validated. They are reported as one thing because to whoever
            // reads the log they are one thing: the answer names what was
            // seen, and the problem document says why.
            let failed = |detail: String| AcmeError::ChallengeFailed {
                name: name.to_string(),
                detail: format!("{detail}: {}", authorization_problem(&authorization)),
            };
            let reported = authorization_status(&authorization).map_err(|e| match e {
                UnreadableStatus::Unknown(status) => failed(status),
                UnreadableStatus::Missing => AcmeError::protocol(
                    format!("the authorization for {name}"),
                    "it carries no status",
                ),
            })?;
            status = status
                .advance(reported)
                .map_err(|step| failed(step.to_string()))?;

            if status == AuthorizationStatus::Valid {
                info!(name, "the authority validated this name");
                return Ok(());
            }
            if status.is_decided() {
                return Err(failed(status.as_str().to_string()));
            }
        }
    }

    /// Wait one poll interval, unless the order has already run out of time.
    ///
    /// The wait is a poll interval and nothing else: what decides whether
    /// this loop ends is the resource fetched after it, never the passage of
    /// time on its own.
    async fn wait_before_next_poll(
        &self,
        name: &str,
        status: &str,
        started: Instant,
        interval: Duration,
    ) -> Result<(), AcmeError> {
        let elapsed = started.elapsed();
        if elapsed >= self.order_timeout {
            return Err(AcmeError::OrderStalled {
                name: name.to_string(),
                status: status.to_string(),
                elapsed,
            });
        }
        let remaining = self.order_timeout - elapsed;
        tokio::time::sleep(
            interval
                .clamp(Duration::from_millis(100), MAX_POLL_INTERVAL)
                .min(remaining),
        )
        .await;
        Ok(())
    }

    fn require_account(&self) -> Result<String, AcmeError> {
        self.account_url.clone().ok_or_else(|| {
            AcmeError::protocol(
                "an order",
                "no account has been registered yet, so there is nothing to sign the order with",
            )
        })
    }

    /// A signed request naming the registered account.
    async fn post_as_account(
        &mut self,
        url: &str,
        payload: &str,
    ) -> Result<AcmeResponse, AcmeError> {
        let account = self.require_account()?;
        self.post(url, payload, Some(account)).await
    }

    /// One signed request, retried once if the nonce we used was stale.
    ///
    /// A nonce is single-use and an authority may also expire them, so
    /// `badNonce` is a normal answer rather than a fault - RFC 8555 section
    /// 6.5 requires the client to retry with the fresh nonce that came back
    /// with the rejection. Without the retry, a proxy that had been idle for
    /// a day would fail its first renewal every time.
    /// `account` is the account URL, or `None` for the one request made
    /// before there is an account: the registration itself.
    async fn post(
        &mut self,
        url: &str,
        payload: &str,
        account: Option<String>,
    ) -> Result<AcmeResponse, AcmeError> {
        for attempt in 0..2 {
            let nonce = self.take_nonce().await?;
            let key_id = match &account {
                Some(account) => KeyId::Kid(account.as_str()),
                None => KeyId::Jwk,
            };
            let body = sign_request(&self.key, key_id, url, &nonce, payload)?;
            let response = self.http.post_jose(url, &body).await?;
            if let Some(nonce) = response.header("replay-nonce") {
                self.nonce = Some(nonce.to_string());
            }
            if attempt == 0 && is_bad_nonce(&response) {
                debug!(
                    url,
                    "the authority rejected a stale nonce; retrying with the fresh one"
                );
                continue;
            }
            return Ok(response);
        }
        unreachable!("the loop above returns on its second pass")
    }

    /// The nonce to sign the next request with, fetching one if the last
    /// answer did not carry one.
    async fn take_nonce(&mut self) -> Result<String, AcmeError> {
        if let Some(nonce) = self.nonce.take() {
            return Ok(nonce);
        }
        let response = self.http.head(&self.directory.new_nonce).await?;
        response
            .header("replay-nonce")
            .map(str::to_string)
            .ok_or_else(|| {
                AcmeError::protocol(
                    "the nonce resource",
                    "it answered without a Replay-Nonce header, so no request can be signed",
                )
            })
    }
}

/// A new key and a certificate signing request for one name.
///
/// A fresh key per certificate, never the account key: RFC 8555 section 11.1
/// requires the two to differ, and an authority refuses a CSR signed by the
/// account key. Beyond the requirement, a leaked certificate key should not
/// also be the key that can order more certificates.
fn certificate_request(name: &str) -> Result<(PKey<Private>, Vec<u8>), AcmeError> {
    let group = EcGroup::from_curve_name(Nid::X9_62_PRIME256V1)
        .map_err(|e| AcmeError::Crypto(format!("selecting the P-256 group: {e}")))?;
    let ec = EcKey::generate(&group)
        .map_err(|e| AcmeError::Crypto(format!("generating the certificate key: {e}")))?;
    let key = PKey::from_ec_key(ec)
        .map_err(|e| AcmeError::Crypto(format!("wrapping the certificate key: {e}")))?;

    let mut builder = X509ReqBuilder::new()
        .map_err(|e| AcmeError::Crypto(format!("starting the certificate request: {e}")))?;
    builder
        .set_pubkey(&key)
        .map_err(|e| AcmeError::Crypto(format!("setting the request public key: {e}")))?;

    // The name goes in a subjectAltName and nowhere else. A common name is
    // ignored by every authority worth using and by every current browser,
    // and `crate::tls::certificate_names` only falls back to one when a
    // certificate carries no SAN at all.
    let mut extensions = Stack::<X509Extension>::new()
        .map_err(|e| AcmeError::Crypto(format!("building the extension stack: {e}")))?;
    let context = builder.x509v3_context(None);
    let san = SubjectAlternativeName::new()
        .dns(name)
        .build(&context)
        .map_err(|e| AcmeError::Crypto(format!("building the subjectAltName for {name}: {e}")))?;
    extensions
        .push(san)
        .map_err(|e| AcmeError::Crypto(format!("adding the subjectAltName: {e}")))?;
    // The context borrows the builder, and the next call needs it mutably.
    // Ending the borrow explicitly rather than leaning on where the compiler
    // decides the last use is.
    let _ = context;
    builder
        .add_extensions(&extensions)
        .map_err(|e| AcmeError::Crypto(format!("attaching the extensions: {e}")))?;
    builder
        .sign(&key, MessageDigest::sha256())
        .map_err(|e| AcmeError::Crypto(format!("signing the certificate request: {e}")))?;

    let request: X509Req = builder.build();
    let der = request
        .to_der()
        .map_err(|e| AcmeError::Crypto(format!("encoding the certificate request: {e}")))?;
    Ok((key, der))
}

/// Whether the authority refused this request only because the nonce was
/// stale.
fn is_bad_nonce(response: &AcmeResponse) -> bool {
    if response.status != 400 {
        return false;
    }
    serde_json::from_slice::<JsonValue>(&response.body)
        .ok()
        .and_then(|body| {
            body.get("type")
                .and_then(JsonValue::as_str)
                .map(|kind| kind.ends_with(":badNonce"))
        })
        .unwrap_or(false)
}

fn expect_json(response: &AcmeResponse, context: &str) -> Result<JsonValue, AcmeError> {
    serde_json::from_slice(&response.body).map_err(|e| {
        AcmeError::protocol(
            context.to_string(),
            format!(
                "it is not JSON ({e}); the first bytes were {:?}",
                String::from_utf8_lossy(&response.body[..response.body.len().min(120)])
            ),
        )
    })
}

/// Turn an authority's refusal into an error that quotes it.
fn problem(response: &AcmeResponse, context: &str) -> AcmeError {
    let body: JsonValue = serde_json::from_slice(&response.body).unwrap_or(JsonValue::Null);
    let kind = body
        .get("type")
        .and_then(JsonValue::as_str)
        .unwrap_or("no problem type")
        .to_string();
    let detail = body
        .get("detail")
        .and_then(JsonValue::as_str)
        .map(|detail| format!(" ({detail})"))
        .unwrap_or_else(|| {
            let text = response.body_text();
            if text.trim().is_empty() {
                String::new()
            } else {
                format!(" ({})", text.trim().chars().take(200).collect::<String>())
            }
        });
    AcmeError::Authority {
        context: context.to_string(),
        status: response.status,
        kind,
        detail,
    }
}

/// Read an order's `status`.
///
/// Both failures are protocol faults for an order: the RFC requires the
/// member, and the five values it may hold are the five in
/// [`OrderStatus`]. An authority sending anything else is not speaking the
/// ACME this client implements, and there is nothing useful to be done with
/// the order after that.
fn order_status(order: &JsonValue, name: &str) -> Result<OrderStatus, AcmeError> {
    let context = || format!("the order for {name}");
    let raw = order
        .get("status")
        .and_then(JsonValue::as_str)
        .ok_or_else(|| AcmeError::protocol(context(), "it carries no status"))?;
    OrderStatus::parse(raw).ok_or_else(|| {
        AcmeError::protocol(context(), format!("it reports an unknown status {raw:?}"))
    })
}

/// Why an authorization's `status` could not be read.
///
/// Kept apart from [`AcmeError`] because the two failures mean different
/// things for an authorization and the caller raises different errors for
/// them: a missing member is a broken authority, while an unrecognised value
/// is the authority telling us something about this name that this client
/// cannot act on - which is a challenge that will not be satisfied, exactly
/// as it was before these statuses were typed.
enum UnreadableStatus {
    /// No `status` member at all.
    Missing,
    /// A value this client has no meaning for.
    Unknown(String),
}

/// Read an authorization's `status`.
fn authorization_status(
    authorization: &JsonValue,
) -> Result<AuthorizationStatus, UnreadableStatus> {
    let raw = authorization
        .get("status")
        .and_then(JsonValue::as_str)
        .ok_or(UnreadableStatus::Missing)?;
    AuthorizationStatus::parse(raw).ok_or_else(|| UnreadableStatus::Unknown(raw.to_string()))
}

/// The `error` a failed challenge carries, as a sentence.
fn authorization_problem(authorization: &JsonValue) -> String {
    authorization
        .get("challenges")
        .and_then(JsonValue::as_array)
        .and_then(|challenges| {
            challenges
                .iter()
                .find_map(|challenge| challenge.get("error"))
                .map(describe_problem)
        })
        .unwrap_or_else(|| "the authority gave no reason".to_string())
}

fn order_problem(order: &JsonValue) -> String {
    order
        .get("error")
        .map(describe_problem)
        .unwrap_or_else(|| "the authority gave no reason".to_string())
}

fn describe_problem(problem: &JsonValue) -> String {
    let kind = problem
        .get("type")
        .and_then(JsonValue::as_str)
        .unwrap_or("no problem type");
    match problem.get("detail").and_then(JsonValue::as_str) {
        Some(detail) => format!("{kind}: {detail}"),
        None => kind.to_string(),
    }
}

fn string_array(value: &JsonValue, field: &str) -> Vec<String> {
    value
        .get(field)
        .and_then(JsonValue::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(JsonValue::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use openssl::nid::Nid as OpensslNid;

    #[test]
    fn a_certificate_request_names_the_host_in_a_subject_alt_name() {
        // Where the name has to be. A CSR that only carried a common name
        // would be accepted by the stub in these tests and rejected, or
        // silently stripped, by a real authority.
        let (key, der) = certificate_request("shop.example.com").unwrap();
        let request = X509Req::from_der(&der).unwrap();
        assert!(request.verify(&request.public_key().unwrap()).unwrap());

        let extensions = request.extensions().unwrap();
        assert_eq!(extensions.len(), 1);
        let text = String::from_utf8(request.to_text().expect("the request prints")).unwrap();
        assert!(
            text.contains("shop.example.com"),
            "the request does not mention the name: {text}"
        );
        assert!(
            key.ec_key().unwrap().group().curve_name() == Some(OpensslNid::X9_62_PRIME256V1),
            "the certificate key should be P-256"
        );
    }

    #[test]
    fn each_certificate_gets_a_key_of_its_own() {
        // RFC 8555 section 11.1 forbids reusing the account key, and a
        // certificate key shared between two names would make one leak into
        // two.
        let (first, _) = certificate_request("a.example.com").unwrap();
        let (second, _) = certificate_request("b.example.com").unwrap();
        assert!(!first.public_eq(&second));
    }

    fn response(status: u16, body: &str) -> AcmeResponse {
        AcmeResponse {
            status,
            headers: Vec::new(),
            body: body.as_bytes().to_vec(),
        }
    }

    #[test]
    fn a_stale_nonce_is_recognised_and_nothing_else_is() {
        assert!(is_bad_nonce(&response(
            400,
            r#"{"type":"urn:ietf:params:acme:error:badNonce","detail":"JWS has an invalid anti-replay nonce"}"#
        )));
        assert!(!is_bad_nonce(&response(
            400,
            r#"{"type":"urn:ietf:params:acme:error:malformed"}"#
        )));
        assert!(!is_bad_nonce(&response(
            429,
            r#"{"type":"urn:ietf:params:acme:error:badNonce"}"#
        )));
        assert!(!is_bad_nonce(&response(400, "not json")));
    }

    #[test]
    fn a_refusal_is_reported_with_the_authoritys_own_words() {
        // The difference between a log line an operator can act on and one
        // they cannot: "rateLimited (too many certificates already issued
        // for example.com)" says what to do, "order failed" does not.
        let error = problem(
            &response(
                429,
                r#"{"type":"urn:ietf:params:acme:error:rateLimited","detail":"too many certificates already issued for example.com"}"#,
            ),
            "the order for shop.example.com",
        );
        assert!(error.is_rate_limit());
        let message = error.to_string();
        assert!(message.contains("rateLimited"), "{message}");
        assert!(
            message.contains("too many certificates already issued"),
            "{message}"
        );
        assert!(message.contains("429"), "{message}");
    }

    #[test]
    fn a_refusal_with_no_problem_document_still_quotes_what_arrived() {
        let error = problem(&response(502, "<html>gateway</html>"), "the directory");
        let message = error.to_string();
        assert!(message.contains("502"), "{message}");
        assert!(message.contains("gateway"), "{message}");
    }

    #[test]
    fn a_directory_that_is_not_one_is_refused_by_name() {
        let error = Directory::parse(&json!({"newNonce": "https://ca.test/nonce"})).unwrap_err();
        assert!(error.to_string().contains("newAccount"), "{error}");
    }
}
