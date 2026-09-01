//! A certificate authority that speaks enough of RFC 8555 to issue a
//! certificate, and nothing more.
//!
//! Why there is one here at all: the tests must never talk to a real
//! authority. Let's Encrypt's staging environment is still a shared service
//! with rate limits, it needs a name that resolves on the public internet,
//! and a test that depends on it fails for reasons that have nothing to do
//! with this code. So the authority is in this file, on a loopback port, and
//! it can be told to fail on purpose.
//!
//! It is a stub, not a toy. It checks the things a real authority checks and
//! that a client can silently get wrong:
//!
//! * the JWS signature, verified against the account's own public key, so a
//!   signature encoded as DER instead of the raw pair ES256 wants is caught
//!   here rather than at a real authority months later;
//! * the `url` and `nonce` in the protected header, with each nonce accepted
//!   once;
//! * that the certificate signing request names the identifier in a
//!   subjectAltName;
//! * that the challenge answer served on port 80 is the key authorization
//!   for this account's key and no other.
//!
//! It answers HTTP/1.1 by hand for the same reason `harness::Backend` does:
//! the bytes on both sides are bytes this file wrote, so nothing about the
//! exchange is taken on trust from a library.

// Each test binary and the demo's example compile this module whole and use
// part of it; the unused part is not dead code, it is another caller's.
#![allow(dead_code)]

use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::path::Path;
use std::sync::{Arc, Mutex};

use openssl::asn1::Asn1Time;
use openssl::bn::BigNum;
use openssl::ec::{EcGroup, EcKey};
use openssl::ecdsa::EcdsaSig;
use openssl::hash::{MessageDigest, hash};
use openssl::nid::Nid;
use openssl::pkey::{PKey, Private};
use openssl::x509::extension::{BasicConstraints, SubjectAlternativeName};
use openssl::x509::{X509, X509Name, X509Req};
use serde_json::{Value as JsonValue, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

// ----------------------------------------------------------------------
// base64url, both directions
// ----------------------------------------------------------------------

const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

pub fn encode(bytes: &[u8]) -> String {
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let triple = (b0 << 16) | (b1 << 8) | b2;
        for i in 0..chunk.len() + 1 {
            out.push(ALPHABET[((triple >> (18 - 6 * i)) & 0x3f) as usize] as char);
        }
    }
    out
}

pub fn decode(text: &str) -> Option<Vec<u8>> {
    let mut bits = 0u32;
    let mut held = 0u32;
    let mut out = Vec::new();
    for byte in text.bytes() {
        let value = ALPHABET.iter().position(|c| *c == byte)? as u32;
        bits = (bits << 6) | value;
        held += 6;
        if held >= 8 {
            held -= 8;
            out.push((bits >> held) as u8);
        }
    }
    Some(out)
}

// ----------------------------------------------------------------------
// How this authority should misbehave, if at all
// ----------------------------------------------------------------------

/// An RFC 7807 problem document the authority can be told to answer with.
#[derive(Clone)]
pub struct Problem {
    pub status: u16,
    pub kind: String,
    pub detail: String,
}

impl Problem {
    pub fn rate_limited() -> Self {
        Self {
            status: 429,
            kind: "urn:ietf:params:acme:error:rateLimited".to_string(),
            detail: "too many certificates already issued for this exact set of identifiers"
                .to_string(),
        }
    }
}

#[derive(Clone)]
pub struct CaOptions {
    /// Port the challenge fetch is aimed at, standing in for the port 80 an
    /// authority would use. Every identifier is fetched from loopback here,
    /// since none of the names in a test resolve anywhere.
    pub validation_port: u16,
    /// Refuse every order with this problem instead of creating it.
    pub refuse_orders: Option<Problem>,
    /// Compare the challenge answer against a value that cannot match, so
    /// the validation fails the way a wrong DNS record would.
    pub sabotage_validation: bool,
    /// Days of validity on the issued leaf. Short values are how a test
    /// makes a certificate that is already inside the renewal window.
    pub validity_days: u32,
    /// Answer `finalize` with `processing` and then report the order as
    /// `ready` on every poll, which RFC 8555 section 7.1.6 does not allow -
    /// an order that has begun processing does not go back to waiting for a
    /// finalize it has already had. A real authority does not do this; the
    /// option exists because a client has to notice when one does.
    pub regress_order_after_finalize: bool,
}

impl Default for CaOptions {
    fn default() -> Self {
        Self {
            validation_port: 0,
            refuse_orders: None,
            sabotage_validation: false,
            validity_days: 90,
            regress_order_after_finalize: false,
        }
    }
}

// ----------------------------------------------------------------------
// The authority
// ----------------------------------------------------------------------

struct Account {
    /// The account's public key, for verifying every later signature.
    key: EcKey<openssl::pkey::Public>,
    thumbprint: String,
    contact: Vec<String>,
}

#[derive(Clone, PartialEq)]
enum AuthzStatus {
    Pending,
    Valid,
    Invalid(String),
}

struct Authorization {
    identifier: String,
    account: String,
    token: String,
    status: AuthzStatus,
}

struct Order {
    identifier: String,
    account: String,
    authorization: usize,
    status: String,
    certificate: Option<usize>,
}

struct CaState {
    options: CaOptions,
    base: String,
    root_key: PKey<Private>,
    root_cert: X509,
    nonces: HashSet<String>,
    next_nonce: u64,
    accounts: HashMap<String, Account>,
    authorizations: Vec<Authorization>,
    orders: Vec<Order>,
    certificates: Vec<Vec<u8>>,
    /// Every name a certificate has been issued for, in order. The witness
    /// for "a restart did not order this again".
    issued: Vec<String>,
    /// Every challenge the authority fetched, and what came back.
    validations: Vec<(String, bool)>,
}

pub struct StubCa {
    pub directory_url: String,
    pub addr: SocketAddr,
    state: Arc<Mutex<CaState>>,
    server: tokio::task::JoinHandle<()>,
}

impl Drop for StubCa {
    fn drop(&mut self) {
        self.server.abort();
    }
}

impl StubCa {
    /// On a port the kernel picks, which is what a test wants.
    pub async fn start(options: CaOptions) -> StubCa {
        StubCa::start_on(0, options).await
    }

    /// On a named port, which is what a script wants: the proxy's
    /// configuration file has to carry the directory URL, so the port has to
    /// be decided before either process starts.
    pub async fn start_on(port: u16, options: CaOptions) -> StubCa {
        let listener = TcpListener::bind(("127.0.0.1", port))
            .await
            .expect("a port for the stub certificate authority");
        let addr = listener.local_addr().expect("the authority's address");
        let base = format!("http://{addr}");

        let (root_key, root_cert) = self_signed_root();
        let state = Arc::new(Mutex::new(CaState {
            options,
            base: base.clone(),
            root_key,
            root_cert,
            nonces: HashSet::new(),
            next_nonce: 0,
            accounts: HashMap::new(),
            authorizations: Vec::new(),
            orders: Vec::new(),
            certificates: Vec::new(),
            issued: Vec::new(),
            validations: Vec::new(),
        }));

        let shared = Arc::clone(&state);
        let server = tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                let state = Arc::clone(&shared);
                tokio::spawn(async move {
                    serve_one(stream, state).await;
                });
            }
        });

        StubCa {
            directory_url: format!("{base}/directory"),
            addr,
            state,
            server,
        }
    }

    /// The root this authority signs with, as PEM, for a client that has to
    /// trust it.
    pub fn root_pem(&self) -> Vec<u8> {
        self.state
            .lock()
            .unwrap()
            .root_cert
            .to_pem()
            .expect("the root certificate serialises")
    }

    pub fn write_root_pem(&self, path: &Path) {
        std::fs::write(path, self.root_pem()).expect("the root certificate is written");
    }

    /// Every name this authority has issued a certificate for, in order.
    pub fn issued(&self) -> Vec<String> {
        self.state.lock().unwrap().issued.clone()
    }

    pub fn issued_count(&self) -> usize {
        self.state.lock().unwrap().issued.len()
    }

    /// Every challenge fetch it made, and whether the answer matched.
    pub fn validations(&self) -> Vec<(String, bool)> {
        self.state.lock().unwrap().validations.clone()
    }

    /// How many accounts were registered here. The witness for "a restart
    /// reused the stored account key".
    pub fn account_count(&self) -> usize {
        self.state.lock().unwrap().accounts.len()
    }

    pub fn contacts(&self) -> Vec<String> {
        self.state
            .lock()
            .unwrap()
            .accounts
            .values()
            .flat_map(|account| account.contact.clone())
            .collect()
    }

    /// Start refusing every order, or stop.
    pub fn refuse_orders(&self, problem: Option<Problem>) {
        self.state.lock().unwrap().options.refuse_orders = problem;
    }

    pub fn sabotage_validation(&self, sabotage: bool) {
        self.state.lock().unwrap().options.sabotage_validation = sabotage;
    }
}

/// A self-signed root, so a client can be handed something to trust.
fn self_signed_root() -> (PKey<Private>, X509) {
    let group = EcGroup::from_curve_name(Nid::X9_62_PRIME256V1).unwrap();
    let key = PKey::from_ec_key(EcKey::generate(&group).unwrap()).unwrap();

    let mut name = X509Name::builder().unwrap();
    name.append_entry_by_text("CN", "deis-proxy test authority")
        .unwrap();
    let name = name.build();

    let mut builder = X509::builder().unwrap();
    builder.set_version(2).unwrap();
    builder.set_subject_name(&name).unwrap();
    builder.set_issuer_name(&name).unwrap();
    builder.set_pubkey(&key).unwrap();
    builder
        .set_not_before(&Asn1Time::days_from_now(0).unwrap())
        .unwrap();
    builder
        .set_not_after(&Asn1Time::days_from_now(3650).unwrap())
        .unwrap();
    builder
        .set_serial_number(&BigNum::from_u32(1).unwrap().to_asn1_integer().unwrap())
        .unwrap();
    builder
        .append_extension(BasicConstraints::new().critical().ca().build().unwrap())
        .unwrap();
    builder.sign(&key, MessageDigest::sha256()).unwrap();
    (key, builder.build())
}

// ----------------------------------------------------------------------
// Serving
// ----------------------------------------------------------------------

struct Request {
    method: String,
    path: String,
    body: Vec<u8>,
}

async fn serve_one(mut stream: TcpStream, state: Arc<Mutex<CaState>>) {
    let Some(request) = read_request(&mut stream).await else {
        return;
    };
    let (status, headers, body) = route(&request, &state).await;

    let mut response = format!("HTTP/1.1 {status} {}\r\n", reason(status));
    for (name, value) in &headers {
        response.push_str(&format!("{name}: {value}\r\n"));
    }
    response.push_str(&format!("Content-Length: {}\r\n", body.len()));
    response.push_str("Connection: close\r\n\r\n");

    let mut out = response.into_bytes();
    if request.method != "HEAD" {
        out.extend_from_slice(&body);
    }
    let _ = stream.write_all(&out).await;
    let _ = stream.shutdown().await;
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        201 => "Created",
        400 => "Bad Request",
        404 => "Not Found",
        405 => "Method Not Allowed",
        429 => "Too Many Requests",
        _ => "Error",
    }
}

async fn read_request(stream: &mut TcpStream) -> Option<Request> {
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        match stream.read(&mut byte).await {
            Ok(0) | Err(_) => return None,
            Ok(_) => head.push(byte[0]),
        }
        if head.len() > 16384 {
            return None;
        }
    }
    let head = String::from_utf8(head).ok()?;
    let mut lines = head.lines();
    let mut request_line = lines.next()?.split_whitespace();
    let method = request_line.next()?.to_string();
    let path = request_line.next()?.to_string();

    let length: usize = lines
        .filter_map(|line| line.split_once(':'))
        .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
        .and_then(|(_, value)| value.trim().parse().ok())
        .unwrap_or(0);

    let mut body = vec![0u8; length];
    if length > 0 && stream.read_exact(&mut body).await.is_err() {
        return None;
    }
    Some(Request { method, path, body })
}

type Answer = (u16, Vec<(String, String)>, Vec<u8>);

fn json_answer(
    status: u16,
    value: JsonValue,
    extra: Vec<(String, String)>,
    nonce: String,
) -> Answer {
    let mut headers = vec![
        ("Content-Type".to_string(), "application/json".to_string()),
        ("Replay-Nonce".to_string(), nonce),
    ];
    headers.extend(extra);
    (status, headers, value.to_string().into_bytes())
}

fn problem_answer(problem: &Problem, nonce: String) -> Answer {
    (
        problem.status,
        vec![
            (
                "Content-Type".to_string(),
                "application/problem+json".to_string(),
            ),
            ("Replay-Nonce".to_string(), nonce),
        ],
        json!({ "type": problem.kind, "detail": problem.detail })
            .to_string()
            .into_bytes(),
    )
}

async fn route(request: &Request, state: &Arc<Mutex<CaState>>) -> Answer {
    let path = request.path.clone();

    if path == "/directory" {
        let base = state.lock().unwrap().base.clone();
        return json_answer(
            200,
            json!({
                "newNonce": format!("{base}/new-nonce"),
                "newAccount": format!("{base}/new-account"),
                "newOrder": format!("{base}/new-order"),
                "meta": { "termsOfService": format!("{base}/terms") },
            }),
            Vec::new(),
            mint_nonce(state),
        );
    }
    // Not part of RFC 8555. A window into what this authority has been
    // asked for, so a script driving it can assert on the authority's own
    // record instead of on the client's log.
    if path == "/stats" {
        let state = state.lock().unwrap();
        let body = json!({
            "issued": state.issued,
            "accounts": state.accounts.len(),
            "validations": state
                .validations
                .iter()
                .map(|(name, matched)| json!({ "name": name, "matched": matched }))
                .collect::<Vec<_>>(),
        });
        return (
            200,
            vec![("Content-Type".to_string(), "application/json".to_string())],
            body.to_string().into_bytes(),
        );
    }
    if path == "/new-nonce" {
        return (
            200,
            vec![("Replay-Nonce".to_string(), mint_nonce(state))],
            Vec::new(),
        );
    }
    if request.method != "POST" {
        return (405, Vec::new(), b"only POST\n".to_vec());
    }

    // Everything past here is a signed request.
    let signed = match verify_jws(request, state, &path) {
        Ok(signed) => signed,
        Err(problem) => return problem_answer(&problem, mint_nonce(state)),
    };

    if path == "/new-account" {
        return new_account(state, signed);
    }
    if path == "/new-order" {
        return new_order(state, signed);
    }
    if let Some(index) = path.strip_prefix("/authz/").and_then(parse_index) {
        return authorization(state, index);
    }
    if let Some(index) = path.strip_prefix("/chall/").and_then(parse_index) {
        return accept_challenge(state, index).await;
    }
    if let Some(index) = path
        .strip_prefix("/order/")
        .and_then(|rest| rest.strip_suffix("/finalize"))
        .and_then(parse_index)
    {
        return finalize(state, index, &signed.payload);
    }
    if let Some(index) = path.strip_prefix("/order/").and_then(parse_index) {
        return order_state(state, index);
    }
    if let Some(index) = path.strip_prefix("/cert/").and_then(parse_index) {
        return certificate(state, index);
    }

    (
        404,
        vec![("Replay-Nonce".to_string(), mint_nonce(state))],
        b"no such resource\n".to_vec(),
    )
}

fn parse_index(text: &str) -> Option<usize> {
    text.parse().ok()
}

fn mint_nonce(state: &Arc<Mutex<CaState>>) -> String {
    let mut state = state.lock().unwrap();
    state.next_nonce += 1;
    let nonce = format!("nonce-{}", state.next_nonce);
    state.nonces.insert(nonce.clone());
    nonce
}

struct SignedRequest {
    account: Option<String>,
    jwk: Option<JsonValue>,
    payload: JsonValue,
}

/// Check the envelope the way a real authority does: the nonce is one we
/// issued and have not seen before, the URL is the one being requested, and
/// the signature verifies against the account's key.
fn verify_jws(
    request: &Request,
    state: &Arc<Mutex<CaState>>,
    path: &str,
) -> Result<SignedRequest, Problem> {
    let malformed = |detail: &str| Problem {
        status: 400,
        kind: "urn:ietf:params:acme:error:malformed".to_string(),
        detail: detail.to_string(),
    };

    let jws: JsonValue =
        serde_json::from_slice(&request.body).map_err(|e| malformed(&format!("not JSON: {e}")))?;
    let protected_b64 = jws["protected"]
        .as_str()
        .ok_or_else(|| malformed("no protected header"))?;
    let payload_b64 = jws["payload"]
        .as_str()
        .ok_or_else(|| malformed("no payload"))?;
    let signature = decode(
        jws["signature"]
            .as_str()
            .ok_or_else(|| malformed("no signature"))?,
    )
    .ok_or_else(|| malformed("the signature is not base64url"))?;

    let header: JsonValue = serde_json::from_slice(
        &decode(protected_b64).ok_or_else(|| malformed("the header is not base64url"))?,
    )
    .map_err(|_| malformed("the header is not JSON"))?;

    if header["alg"] != "ES256" {
        return Err(malformed("this authority only accepts ES256"));
    }
    let base = state.lock().unwrap().base.clone();
    let expected_url = format!("{base}{path}");
    if header["url"].as_str() != Some(expected_url.as_str()) {
        return Err(malformed(&format!(
            "the protected header names {:?} but the request went to {expected_url}",
            header["url"]
        )));
    }

    let nonce = header["nonce"]
        .as_str()
        .ok_or_else(|| malformed("no nonce"))?
        .to_string();
    {
        let mut state = state.lock().unwrap();
        if !state.nonces.remove(&nonce) {
            return Err(Problem {
                status: 400,
                kind: "urn:ietf:params:acme:error:badNonce".to_string(),
                detail: "the nonce has been used already or was never issued".to_string(),
            });
        }
    }

    // The signature, against the key the request claims - either inline for
    // a registration, or the one stored for the named account.
    let (key, account, jwk) = match (header.get("jwk"), header.get("kid")) {
        (Some(jwk), None) => (
            public_key_from_jwk(jwk).ok_or_else(|| malformed("the jwk is not a P-256 key"))?,
            None,
            Some(jwk.clone()),
        ),
        (None, Some(kid)) => {
            let kid = kid
                .as_str()
                .ok_or_else(|| malformed("kid is not a string"))?
                .to_string();
            let state = state.lock().unwrap();
            let account = state.accounts.get(&kid).ok_or_else(|| Problem {
                status: 400,
                kind: "urn:ietf:params:acme:error:accountDoesNotExist".to_string(),
                detail: format!("no account at {kid}"),
            })?;
            (account.key.clone(), Some(kid), None)
        }
        _ => return Err(malformed("exactly one of jwk and kid is required")),
    };

    let signing_input = format!("{protected_b64}.{payload_b64}");
    if signature.len() != 64 {
        return Err(malformed(&format!(
            "an ES256 signature is 64 raw bytes; this one is {} - a DER-encoded signature was \
             probably passed through unchanged",
            signature.len()
        )));
    }
    let digest = hash(MessageDigest::sha256(), signing_input.as_bytes()).unwrap();
    let r = BigNum::from_slice(&signature[..32]).unwrap();
    let s = BigNum::from_slice(&signature[32..]).unwrap();
    let signature = EcdsaSig::from_private_components(r, s).unwrap();
    if !signature.verify(&digest, &key).unwrap_or(false) {
        return Err(malformed(
            "the signature does not verify against the account key",
        ));
    }

    let payload = if payload_b64.is_empty() {
        JsonValue::Null
    } else {
        serde_json::from_slice(
            &decode(payload_b64).ok_or_else(|| malformed("payload not base64url"))?,
        )
        .map_err(|_| malformed("the payload is not JSON"))?
    };

    Ok(SignedRequest {
        account,
        jwk,
        payload,
    })
}

fn public_key_from_jwk(jwk: &JsonValue) -> Option<EcKey<openssl::pkey::Public>> {
    if jwk["kty"] != "EC" || jwk["crv"] != "P-256" {
        return None;
    }
    let x = BigNum::from_slice(&decode(jwk["x"].as_str()?)?).ok()?;
    let y = BigNum::from_slice(&decode(jwk["y"].as_str()?)?).ok()?;
    let group = EcGroup::from_curve_name(Nid::X9_62_PRIME256V1).ok()?;
    EcKey::from_public_key_affine_coordinates(&group, &x, &y).ok()
}

/// RFC 7638 over the JWK, the same canonical form the client builds.
fn thumbprint(jwk: &JsonValue) -> String {
    let canonical = format!(
        r#"{{"crv":"P-256","kty":"EC","x":"{}","y":"{}"}}"#,
        jwk["x"].as_str().unwrap_or_default(),
        jwk["y"].as_str().unwrap_or_default()
    );
    encode(&hash(MessageDigest::sha256(), canonical.as_bytes()).unwrap())
}

fn new_account(state: &Arc<Mutex<CaState>>, signed: SignedRequest) -> Answer {
    let Some(jwk) = signed.jwk else {
        return problem_answer(
            &Problem {
                status: 400,
                kind: "urn:ietf:params:acme:error:malformed".to_string(),
                detail: "a registration must carry the key inline".to_string(),
            },
            mint_nonce(state),
        );
    };
    if signed.payload["termsOfServiceAgreed"] != json!(true) {
        return problem_answer(
            &Problem {
                status: 400,
                kind: "urn:ietf:params:acme:error:userActionRequired".to_string(),
                detail: "the terms of service have not been agreed to".to_string(),
            },
            mint_nonce(state),
        );
    }

    let thumb = thumbprint(&jwk);
    let contact: Vec<String> = signed.payload["contact"]
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(JsonValue::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();

    let nonce = mint_nonce(state);
    let (url, existing) = {
        let mut state = state.lock().unwrap();
        let base = state.base.clone();
        // Look the account up by key, the way a real authority does, so a
        // client that lost its account URL does not create a second account.
        if let Some((url, _)) = state
            .accounts
            .iter()
            .find(|(_, account)| account.thumbprint == thumb)
        {
            (url.clone(), true)
        } else {
            let url = format!("{base}/acct/{}", state.accounts.len() + 1);
            let key = public_key_from_jwk(&jwk).expect("verified above");
            state.accounts.insert(
                url.clone(),
                Account {
                    key,
                    thumbprint: thumb,
                    contact,
                },
            );
            (url, false)
        }
    };

    json_answer(
        if existing { 200 } else { 201 },
        json!({ "status": "valid" }),
        vec![("Location".to_string(), url)],
        nonce,
    )
}

fn new_order(state: &Arc<Mutex<CaState>>, signed: SignedRequest) -> Answer {
    // Taken out of the lock before it is looked at. Reading it inside an
    // `if let` scrutinee keeps the guard alive through the body, and the
    // body mints a nonce, which locks the same mutex.
    let refusal = state.lock().unwrap().options.refuse_orders.clone();
    if let Some(problem) = refusal {
        return problem_answer(&problem, mint_nonce(state));
    }
    let Some(account) = signed.account else {
        return problem_answer(
            &Problem {
                status: 400,
                kind: "urn:ietf:params:acme:error:malformed".to_string(),
                detail: "an order must name an account".to_string(),
            },
            mint_nonce(state),
        );
    };
    let identifier = signed.payload["identifiers"][0]["value"]
        .as_str()
        .unwrap_or_default()
        .to_string();

    let nonce = mint_nonce(state);
    let (order_index, authz_index, base) = {
        let mut state = state.lock().unwrap();
        let base = state.base.clone();
        let token = format!("token{}", state.authorizations.len() + 1);
        state.authorizations.push(Authorization {
            identifier: identifier.clone(),
            account: account.clone(),
            token,
            status: AuthzStatus::Pending,
        });
        let authz_index = state.authorizations.len() - 1;
        state.orders.push(Order {
            identifier: identifier.clone(),
            account,
            authorization: authz_index,
            status: "pending".to_string(),
            certificate: None,
        });
        (state.orders.len() - 1, authz_index, base)
    };

    json_answer(
        201,
        json!({
            "status": "pending",
            "identifiers": [{ "type": "dns", "value": identifier }],
            "authorizations": [format!("{base}/authz/{authz_index}")],
            "finalize": format!("{base}/order/{order_index}/finalize"),
        }),
        vec![(
            "Location".to_string(),
            format!("{base}/order/{order_index}"),
        )],
        nonce,
    )
}

fn authorization(state: &Arc<Mutex<CaState>>, index: usize) -> Answer {
    let nonce = mint_nonce(state);
    let state = state.lock().unwrap();
    let Some(authz) = state.authorizations.get(index) else {
        return (404, Vec::new(), b"no such authorization\n".to_vec());
    };
    let base = &state.base;
    let (status, error) = match &authz.status {
        AuthzStatus::Pending => ("pending", None),
        AuthzStatus::Valid => ("valid", None),
        AuthzStatus::Invalid(detail) => ("invalid", Some(detail.clone())),
    };
    let mut challenge = json!({
        "type": "http-01",
        "url": format!("{base}/chall/{index}"),
        "token": authz.token,
        "status": status,
    });
    if let Some(detail) = error {
        challenge["error"] = json!({
            "type": "urn:ietf:params:acme:error:unauthorized",
            "detail": detail,
        });
    }
    json_answer(
        200,
        json!({
            "status": status,
            "identifier": { "type": "dns", "value": authz.identifier },
            "challenges": [challenge],
        }),
        Vec::new(),
        nonce,
    )
}

/// Accept the challenge and go and fetch it, exactly as an authority would.
async fn accept_challenge(state: &Arc<Mutex<CaState>>, index: usize) -> Answer {
    let nonce = mint_nonce(state);
    let (identifier, token, expected, port, sabotage, base) = {
        let state = state.lock().unwrap();
        let Some(authz) = state.authorizations.get(index) else {
            return (404, Vec::new(), b"no such challenge\n".to_vec());
        };
        let thumb = state
            .accounts
            .get(&authz.account)
            .map(|account| account.thumbprint.clone())
            .unwrap_or_default();
        (
            authz.identifier.clone(),
            authz.token.clone(),
            format!("{}.{}", authz.token, thumb),
            state.options.validation_port,
            state.options.sabotage_validation,
            state.base.clone(),
        )
    };

    // Synchronously, because the whole exchange is on loopback and a client
    // that polls once and gets an answer is exercising the same code as one
    // that polls ten times.
    let fetched = fetch_challenge(port, &identifier, &token).await;
    let wanted = if sabotage {
        format!("{expected}-not-the-answer")
    } else {
        expected
    };
    let matched = fetched.as_deref() == Some(wanted.as_str());

    {
        let mut state = state.lock().unwrap();
        state.validations.push((identifier.clone(), matched));
        if let Some(authz) = state.authorizations.get_mut(index) {
            authz.status = if matched {
                AuthzStatus::Valid
            } else {
                AuthzStatus::Invalid(match fetched {
                    Some(body) => format!(
                        "fetching http://{identifier}/.well-known/acme-challenge/{token} returned \
                         {body:?}, which is not the expected key authorization"
                    ),
                    None => format!(
                        "fetching http://{identifier}/.well-known/acme-challenge/{token} did not \
                         return 200"
                    ),
                })
            };
        }
    }

    json_answer(
        200,
        json!({
            "type": "http-01",
            "url": format!("{base}/chall/{index}"),
            "token": token,
            "status": if matched { "valid" } else { "invalid" },
        }),
        Vec::new(),
        nonce,
    )
}

/// The fetch the authority makes on port 80.
///
/// Every identifier is aimed at loopback, since no name in a test resolves
/// anywhere; the `Host` header carries the identifier so the proxy sees the
/// request the authority would really have sent.
async fn fetch_challenge(port: u16, identifier: &str, token: &str) -> Option<String> {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).await.ok()?;
    let request = format!(
        "GET /.well-known/acme-challenge/{token} HTTP/1.1\r\n\
         Host: {identifier}\r\n\
         Connection: close\r\n\r\n"
    );
    stream.write_all(request.as_bytes()).await.ok()?;
    let mut answer = Vec::new();
    stream.read_to_end(&mut answer).await.ok()?;
    let answer = String::from_utf8_lossy(&answer).into_owned();
    let (head, body) = answer.split_once("\r\n\r\n")?;
    if !head.starts_with("HTTP/1.1 200") {
        return None;
    }
    Some(body.to_string())
}

fn finalize(state: &Arc<Mutex<CaState>>, index: usize, payload: &JsonValue) -> Answer {
    let nonce = mint_nonce(state);
    let malformed = |detail: &str| Problem {
        status: 400,
        kind: "urn:ietf:params:acme:error:malformed".to_string(),
        detail: detail.to_string(),
    };

    let (identifier, authz_valid) = {
        let state = state.lock().unwrap();
        let Some(order) = state.orders.get(index) else {
            return (404, Vec::new(), b"no such order\n".to_vec());
        };
        let valid = state
            .authorizations
            .get(order.authorization)
            .map(|authz| authz.status == AuthzStatus::Valid)
            .unwrap_or(false);
        (order.identifier.clone(), valid)
    };
    if !authz_valid {
        return problem_answer(&malformed("the order's authorization is not valid"), nonce);
    }

    // An authority walking the order backwards. Nothing is signed and the
    // order is parked in `processing`, from which `order_state` then reports
    // `ready` for ever.
    if state.lock().unwrap().options.regress_order_after_finalize {
        if let Some(order) = state.lock().unwrap().orders.get_mut(index) {
            order.status = "processing".to_string();
        }
        return json_answer(
            200,
            json!({
                "status": "processing",
                "identifiers": [{ "type": "dns", "value": identifier }],
            }),
            Vec::new(),
            nonce,
        );
    }

    let Some(csr) = payload["csr"].as_str().and_then(decode) else {
        return problem_answer(&malformed("the csr is missing or not base64url"), nonce);
    };
    let Ok(request) = X509Req::from_der(&csr) else {
        return problem_answer(&malformed("the csr is not a PKCS#10 request"), nonce);
    };
    // A certificate signing request has to be signed by the key it carries,
    // and it has to name the identifier in a subjectAltName - a real
    // authority issues from the SAN and ignores the common name entirely.
    let public = request.public_key().unwrap();
    if !request.verify(&public).unwrap_or(false) {
        return problem_answer(&malformed("the csr's own signature does not verify"), nonce);
    }
    let text = String::from_utf8(request.to_text().unwrap_or_default()).unwrap_or_default();
    if !text.contains(&format!("DNS:{identifier}")) {
        return problem_answer(
            &malformed(&format!(
                "the csr does not name {identifier} in a subjectAltName"
            )),
            nonce,
        );
    }

    let base = state.lock().unwrap().base.clone();
    let chain = {
        let mut state = state.lock().unwrap();
        let days = state.options.validity_days;
        let serial = state.certificates.len() as u32 + 2;
        let leaf = sign_leaf(
            &state.root_key,
            &state.root_cert,
            &public,
            &identifier,
            days,
            serial,
        );
        let mut chain = leaf.to_pem().unwrap();
        chain.extend_from_slice(&state.root_cert.to_pem().unwrap());
        state.certificates.push(chain);
        let certificate = state.certificates.len() - 1;
        state.issued.push(identifier.clone());
        if let Some(order) = state.orders.get_mut(index) {
            order.status = "valid".to_string();
            order.certificate = Some(certificate);
        }
        certificate
    };

    json_answer(
        200,
        json!({
            "status": "valid",
            "identifiers": [{ "type": "dns", "value": identifier }],
            "certificate": format!("{base}/cert/{chain}"),
        }),
        Vec::new(),
        nonce,
    )
}

fn sign_leaf(
    root_key: &PKey<Private>,
    root_cert: &X509,
    public: &PKey<openssl::pkey::Public>,
    name: &str,
    validity_days: u32,
    serial: u32,
) -> X509 {
    let mut subject = X509Name::builder().unwrap();
    subject.append_entry_by_text("CN", name).unwrap();
    let subject = subject.build();

    let mut builder = X509::builder().unwrap();
    builder.set_version(2).unwrap();
    builder.set_subject_name(&subject).unwrap();
    builder.set_issuer_name(root_cert.subject_name()).unwrap();
    builder.set_pubkey(public).unwrap();
    builder
        .set_not_before(&Asn1Time::days_from_now(0).unwrap())
        .unwrap();
    builder
        .set_not_after(&Asn1Time::days_from_now(validity_days).unwrap())
        .unwrap();
    builder
        .set_serial_number(&BigNum::from_u32(serial).unwrap().to_asn1_integer().unwrap())
        .unwrap();
    let context = builder.x509v3_context(Some(root_cert), None);
    let san = SubjectAlternativeName::new()
        .dns(name)
        .build(&context)
        .unwrap();
    let _ = context;
    builder.append_extension(san).unwrap();
    builder.sign(root_key, MessageDigest::sha256()).unwrap();
    builder.build()
}

fn order_state(state: &Arc<Mutex<CaState>>, index: usize) -> Answer {
    let nonce = mint_nonce(state);
    let state = state.lock().unwrap();
    let Some(order) = state.orders.get(index) else {
        return (404, Vec::new(), b"no such order\n".to_vec());
    };
    let base = &state.base;
    let reported = if state.options.regress_order_after_finalize && order.status == "processing" {
        "ready"
    } else {
        order.status.as_str()
    };
    let mut body = json!({
        "status": reported,
        "identifiers": [{ "type": "dns", "value": order.identifier }],
        "authorizations": [format!("{base}/authz/{}", order.authorization)],
        "finalize": format!("{base}/order/{index}/finalize"),
    });
    if let Some(certificate) = order.certificate {
        body["certificate"] = json!(format!("{base}/cert/{certificate}"));
    }
    json_answer(200, body, Vec::new(), nonce)
}

fn certificate(state: &Arc<Mutex<CaState>>, index: usize) -> Answer {
    let nonce = mint_nonce(state);
    let state = state.lock().unwrap();
    match state.certificates.get(index) {
        Some(chain) => (
            200,
            vec![
                (
                    "Content-Type".to_string(),
                    "application/pem-certificate-chain".to_string(),
                ),
                ("Replay-Nonce".to_string(), nonce),
            ],
            chain.clone(),
        ),
        None => (404, Vec::new(), b"no such certificate\n".to_vec()),
    }
}
