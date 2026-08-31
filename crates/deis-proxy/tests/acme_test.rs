//! Automatic certificates, end to end, against a certificate authority that
//! lives in this test run.
//!
//! Everything here goes through the real `deis-proxy` binary with a real
//! configuration file, because the question these tests answer is not "does
//! the ACME client work" but "does a name this proxy serves end up with a
//! certificate a client is served". A protocol client that worked and was
//! wired to nothing would pass a narrower test and fail this one.
//!
//! No test talks to a real authority. `support::acme_ca` is on a loopback
//! port and can be told to refuse orders or to fail a validation, which is
//! how the failure paths are exercised without waiting for a real refusal.

mod harness;
mod support;

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use openssl::ssl::{SslConnector, SslMethod, SslVerifyMode};
use openssl::x509::X509;
use serde_json::json;

use harness::{BACKEND_BODY, Backend, Proxy, ProxyOptions, TlsOptions};
use support::acme_ca::{CaOptions, Problem, StubCa};

/// The name every test certifies. `.test` is reserved by RFC 2606 and
/// resolves nowhere, which is the point: nothing here can accidentally reach
/// a real host, and the stub authority aims its challenge fetch at loopback
/// whatever the identifier says.
const NAME: &str = "shop.test";
const OTHER_NAME: &str = "blog.test";

/// How long a test waits for the renewal task to do something. Generous
/// because it is a real process doing real work over real sockets, and never
/// reached when things go right.
const DEADLINE: Duration = Duration::from_secs(30);

fn app(id: &str, domains: &[&str]) -> serde_json::Value {
    json!({
        "id": id,
        "description": "",
        "domains": domains,
        "certificates": [],
        "enabled": true,
        "cache": [],
    })
}

/// The `[tls.acme]` table a test proxy runs with.
///
/// The intervals are seconds rather than hours so a pass happens while the
/// test is watching. Everything else is what an operator would write.
fn acme_config(ca: &StubCa, storage_dir: &Path, renew_before_days: i64) -> String {
    format!(
        "\n[tls.acme]\n\
         enabled = true\n\
         directory_url = \"{directory}\"\n\
         contact_email = \"ops@example.com\"\n\
         terms_of_service_agreed = true\n\
         storage_dir = \"{storage}\"\n\
         renew_before_days = {renew_before_days}\n\
         alarm_before_days = 1\n\
         check_interval_secs = 1\n\
         retry_backoff_secs = 1\n\
         max_retry_backoff_secs = 2\n\
         request_timeout_secs = 10\n\
         order_timeout_secs = 30\n",
        directory = ca.directory_url,
        storage = storage_dir.display(),
    )
}

/// Where the store puts one name's files, mirroring `acme::store`'s layout.
fn certificate_path(storage_dir: &Path, authority: &str, name: &str, file: &str) -> PathBuf {
    storage_dir
        .join(authority)
        .join("certificates")
        .join(name)
        .join(file)
}

/// The authority directory level: the host of the directory URL.
fn authority_key(ca: &StubCa) -> String {
    ca.addr.to_string().replace(':', "-")
}

/// Poll `condition` until it holds, and fail with `what` if it never does.
///
/// A real condition on every round - what the authority has issued, what is
/// on disk - never a wait for a duration chosen to be long enough.
async fn until<F: FnMut() -> bool>(what: &str, mut condition: F) {
    let deadline = Instant::now() + DEADLINE;
    while Instant::now() < deadline {
        if condition() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("{what} did not happen within {DEADLINE:?}");
}

/// Everything a test needs standing up: a backend, an authority, and a proxy
/// with automatic certificates on.
struct Fixture {
    backend: Backend,
    ca: StubCa,
    proxy: Proxy,
    storage_dir: PathBuf,
    dir: Arc<tempfile::TempDir>,
}

impl Fixture {
    async fn start(
        apps: Vec<serde_json::Value>,
        options: CaOptions,
        renew_before_days: i64,
    ) -> Fixture {
        let backend = Backend::start().await;
        // The HTTP port is chosen first: the authority has to be told where
        // to fetch the challenge from, and that is the port the proxy will
        // answer port 80's job on.
        let http_port = Proxy::pick_port();
        let ca = StubCa::start(CaOptions {
            validation_port: http_port,
            ..options
        })
        .await;

        let dir = Arc::new(tempfile::tempdir().expect("a directory for the proxy"));
        let storage_dir = dir.path().join("acme");
        let extra_config = acme_config(&ca, &storage_dir, renew_before_days);

        let proxy = Proxy::launch(ProxyOptions {
            upstream: backend.addr,
            http_port: Some(http_port),
            tls: Some(TlsOptions {
                port: None,
                default_certificate: None,
            }),
            extra_config,
            apps,
            dir: Some(Arc::clone(&dir)),
            wait_for_forwarding: false,
        })
        .await;

        Fixture {
            backend,
            ca,
            proxy,
            storage_dir,
            dir,
        }
    }

    fn chain_path(&self, name: &str) -> PathBuf {
        certificate_path(
            &self.storage_dir,
            &authority_key(&self.ca),
            name,
            "fullchain.pem",
        )
    }

    fn key_path(&self, name: &str) -> PathBuf {
        certificate_path(
            &self.storage_dir,
            &authority_key(&self.ca),
            name,
            "privkey.pem",
        )
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_name_this_proxy_serves_gets_a_certificate_and_then_serves_it() {
    let fixture = Fixture::start(vec![app("shop", &[NAME])], CaOptions::default(), 30).await;

    until("the authority issues a certificate", || {
        fixture.ca.issued() == vec![NAME.to_string()]
    })
    .await;

    // The authority really did fetch the challenge from this process, and
    // got the answer it was looking for. Without this the test would pass
    // against a client that skipped validation entirely.
    let validations = fixture.ca.validations();
    assert_eq!(
        validations,
        vec![(NAME.to_string(), true)],
        "the authority's own record of what it fetched and whether it matched"
    );

    // The name was never written in the proxy's configuration file: it comes
    // from the application list, which is where the control plane puts it.
    let config = std::fs::read_to_string(fixture.dir.path().join("deis-proxy.toml")).unwrap();
    assert!(
        !config.contains(NAME),
        "the certified name must be derived from what is served, not listed in the configuration"
    );

    until("the certificate reaches the store", || {
        fixture.chain_path(NAME).is_file() && fixture.key_path(NAME).is_file()
    })
    .await;

    // And now the part that matters: a client that trusts this authority
    // gets that certificate over the TLS port, chosen by SNI, and the
    // application behind it answers.
    let tls_addr = fixture.proxy.tls_addr.expect("a TLS listener");
    let root = fixture.ca.root_pem();
    let (leaf, body) = tokio::task::spawn_blocking(move || https_get(tls_addr, NAME, &root))
        .await
        .expect("the handshake task finishes");

    let names = subject_alt_names(&leaf);
    assert_eq!(
        names,
        vec![NAME.to_string()],
        "the certificate served for this SNI name has to be the one that was issued for it"
    );
    assert!(
        body.contains(BACKEND_BODY),
        "the request should have reached the backend through the TLS listener: {body}"
    );
    drop(fixture.backend);
}

#[tokio::test(flavor = "multi_thread")]
async fn every_name_the_route_table_holds_is_certified_and_no_other() {
    // Two applications, and a third name nothing serves. The third proves
    // the names really are derived rather than guessed at.
    let fixture = Fixture::start(
        vec![app("shop", &[NAME]), app("blog", &[OTHER_NAME])],
        CaOptions::default(),
        30,
    )
    .await;

    until("both names are certified", || {
        fixture.ca.issued().len() == 2
    })
    .await;
    let mut issued = fixture.ca.issued();
    issued.sort();
    assert_eq!(issued, vec![OTHER_NAME.to_string(), NAME.to_string()]);

    // A pass or two later, still two: a name that already has a current
    // certificate is not ordered again.
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert_eq!(
        fixture.ca.issued().len(),
        2,
        "a current certificate must not be reordered on every pass"
    );
    drop(fixture.backend);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_restart_finds_the_certificate_on_disk_and_asks_the_authority_for_nothing() {
    // The failure this guards against is expensive rather than merely
    // untidy: a proxy that reissued on every start would spend Let's
    // Encrypt's fifty certificates a week per registered domain on restarts,
    // and once that is spent nothing under the domain can be issued at all.
    let mut fixture = Fixture::start(vec![app("shop", &[NAME])], CaOptions::default(), 30).await;
    // The condition is the chain being *on disk*, not the authority having
    // issued it. The authority counts the issue as it answers, and the proxy
    // writes the file a moment later; waiting on the count and then reading the
    // file races that gap, and loses often enough to be seen.
    until("the first process stores a certificate", || {
        fixture.chain_path(NAME).exists()
    })
    .await;
    let before = std::fs::read(fixture.chain_path(NAME)).expect("the stored chain");
    assert_eq!(fixture.ca.issued_count(), 1);
    assert_eq!(fixture.ca.account_count(), 1);

    fixture.proxy.stop();

    // The same directory: the same database, the same account key, the same
    // certificate store. This is what an operator's restart looks like.
    let restarted = Proxy::launch(ProxyOptions {
        upstream: fixture.backend.addr,
        http_port: Some(fixture.proxy.addr.port()),
        tls: Some(TlsOptions {
            port: None,
            default_certificate: None,
        }),
        extra_config: acme_config(&fixture.ca, &fixture.storage_dir, 30),
        apps: Vec::new(),
        dir: Some(Arc::clone(&fixture.dir)),
        wait_for_forwarding: false,
    })
    .await;

    until("the restarted process finishes a renewal pass", || {
        restarted
            .log_text()
            .contains("automatic certificate pass finished")
    })
    .await;

    assert_eq!(
        fixture.ca.issued_count(),
        1,
        "the restarted process ordered a certificate it already had: {:?}",
        fixture.ca.issued()
    );
    assert_eq!(
        fixture.ca.account_count(),
        1,
        "the restarted process registered a second account instead of reusing the stored key"
    );
    assert_eq!(
        std::fs::read(fixture.chain_path(NAME)).expect("the stored chain"),
        before,
        "the certificate on disk was replaced by a restart"
    );

    let log = restarted.log_text();
    assert!(
        log.contains("the stored ACME account key was loaded"),
        "the restarted process should say it reused the account key; its log said:\n{log}"
    );
    drop(fixture.backend);
}

#[tokio::test(flavor = "multi_thread")]
async fn an_authority_that_refuses_the_order_is_reported_and_retried_until_it_stops() {
    let fixture = Fixture::start(
        vec![app("shop", &[NAME])],
        CaOptions {
            refuse_orders: Some(Problem::rate_limited()),
            ..Default::default()
        },
        30,
    )
    .await;

    until("the refusal is logged", || {
        fixture.proxy.log_text().contains("rateLimited")
    })
    .await;

    let log = fixture.proxy.log_text();
    assert!(
        log.contains("too many certificates already issued"),
        "the authority's own explanation has to reach the log; it said:\n{log}"
    );
    assert!(
        log.contains("rate_limited=true"),
        "a rate limit must be distinguishable from any other refusal:\n{log}"
    );
    assert!(
        log.contains("retry_in_secs="),
        "every failure has to say when the next attempt is:\n{log}"
    );
    assert!(
        !fixture.chain_path(NAME).exists(),
        "no certificate should exist after a refused order"
    );
    assert_eq!(fixture.ca.issued_count(), 0);

    // The retry is real, not just announced: stop refusing and the next
    // attempt succeeds with no restart and no operator action.
    fixture.ca.refuse_orders(None);
    until("the retry after the refusal succeeds", || {
        fixture.ca.issued() == vec![NAME.to_string()]
    })
    .await;
    drop(fixture.backend);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_challenge_the_authority_polls_and_rejects_fails_the_order_by_name() {
    // What a name pointed at the wrong machine looks like from here: the
    // authority fetches, the answer does not match, and the order dies.
    let fixture = Fixture::start(
        vec![app("shop", &[NAME])],
        CaOptions {
            sabotage_validation: true,
            ..Default::default()
        },
        30,
    )
    .await;

    until("the authority rejects the challenge", || {
        fixture
            .ca
            .validations()
            .iter()
            .any(|(name, matched)| name == NAME && !matched)
    })
    .await;
    until("the failure is logged", || {
        fixture.proxy.log_text().contains("could not validate")
    })
    .await;

    let log = fixture.proxy.log_text();
    assert!(
        log.contains(NAME),
        "the log has to name which host failed:\n{log}"
    );
    assert!(
        log.contains(".well-known/acme-challenge"),
        "the log has to say what the authority actually did:\n{log}"
    );
    assert_eq!(fixture.ca.issued_count(), 0);
    assert!(!fixture.chain_path(NAME).exists());
    drop(fixture.backend);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_certificate_inside_the_renewal_window_is_replaced_before_it_expires() {
    // The authority issues certificates valid for twenty days and the proxy
    // renews at thirty, so every certificate is inside the window the moment
    // it arrives. That makes the renewal path - which would otherwise only
    // run sixty days after a test started - observable in a few seconds.
    let fixture = Fixture::start(
        vec![app("shop", &[NAME])],
        CaOptions {
            validity_days: 20,
            ..Default::default()
        },
        30,
    )
    .await;

    until("a renewal follows the first issue", || {
        fixture.ca.issued_count() >= 2
    })
    .await;
    assert!(
        fixture.ca.issued().iter().all(|name| name == NAME),
        "the renewal should be for the same name: {:?}",
        fixture.ca.issued()
    );

    let log = fixture.proxy.log_text();
    assert!(
        log.contains("entered the renewal window"),
        "the renewal has to say why it happened:\n{log}"
    );
    drop(fixture.backend);
}

#[tokio::test(flavor = "multi_thread")]
async fn the_challenge_path_answers_only_what_this_instance_published() {
    let fixture = Fixture::start(vec![app("shop", &[NAME])], CaOptions::default(), 30).await;
    until("the certificate is issued", || {
        fixture.ca.issued_count() == 1
    })
    .await;

    // A token nothing published. Answering anything but a refusal would let
    // an authority believe a validation had succeeded.
    let answer = fixture
        .proxy
        .request("/.well-known/acme-challenge/never-published", NAME)
        .await;
    assert!(
        answer.starts_with("HTTP/1.1 404"),
        "an unknown token must be refused: {answer}"
    );

    // The token is a path segment chosen by whoever sends the request, and
    // it is used to name a file. These must not read one.
    for hostile in [
        "/.well-known/acme-challenge/../../../etc/passwd",
        "/.well-known/acme-challenge/..%2f..%2fetc%2fpasswd",
        "/.well-known/acme-challenge/",
    ] {
        let answer = fixture.proxy.request(hostile, NAME).await;
        assert!(
            answer.starts_with("HTTP/1.1 404"),
            "{hostile} was not refused: {answer}"
        );
        assert!(
            !answer.contains("root:"),
            "{hostile} read a file it should never have named: {answer}"
        );
    }
    drop(fixture.backend);
}

#[tokio::test(flavor = "multi_thread")]
async fn an_instance_with_its_own_certificate_files_never_asks_the_authority() {
    // The opt-in half. Nothing about a proxy without `[tls.acme]` changes,
    // and this is the test that would fail if the renewal task were ever
    // started unconditionally.
    let backend = Backend::start().await;
    let http_port = Proxy::pick_port();
    let ca = StubCa::start(CaOptions {
        validation_port: http_port,
        ..Default::default()
    })
    .await;

    let proxy = Proxy::launch(ProxyOptions {
        upstream: backend.addr,
        http_port: Some(http_port),
        tls: None,
        extra_config: String::new(),
        apps: vec![app("shop", &[NAME])],
        dir: None,
        wait_for_forwarding: false,
    })
    .await;

    // The challenge path is routed like any other path, which for an unknown
    // host is a 404 from the router rather than from a responder.
    let answer = proxy
        .request("/.well-known/acme-challenge/anything", "unknown.test")
        .await;
    assert!(answer.starts_with("HTTP/1.1 404"), "{answer}");

    tokio::time::sleep(Duration::from_secs(2)).await;
    assert_eq!(
        ca.issued_count(),
        0,
        "a proxy with no [tls.acme] table must never contact an authority"
    );
    assert_eq!(ca.account_count(), 0);
    assert!(
        proxy.log_text().contains("automatic certificates are off"),
        "the log should say so once, so an operator is not left guessing; it said:\n{}",
        proxy.log_text()
    );
    drop(backend);
}

#[test]
fn a_storage_directory_that_cannot_be_created_stops_the_process_at_startup() {
    // Where this has to fail is the whole point. Preparing the store creates
    // directories and writes the account key, and it happens before the
    // process announces readiness - because during a zero-downtime upgrade
    // that announcement is what makes the running process send itself
    // SIGQUIT. A failure discovered after it would take the service down
    // rather than leave the old process serving.
    let dir = tempfile::tempdir().expect("a directory for the proxy");
    // A file, not a directory: `create_dir_all` under it cannot succeed.
    let blocker = dir.path().join("not-a-directory");
    std::fs::write(&blocker, b"this is a file").unwrap();

    let config_path = dir.path().join("deis-proxy.toml");
    std::fs::write(
        &config_path,
        format!(
            "listen_addr = [\"127.0.0.1\"]\n\
             listen_ports = [{http}]\n\
             database = \"{db}\"\n\
             pid_file = \"{pid}\"\n\
             upgrade_sock = \"{sock}\"\n\
             \n\
             [[upstreams]]\n\
             host = \"127.0.0.1\"\n\
             port = 1\n\
             \n\
             [tls]\n\
             ports = [{tls}]\n\
             \n\
             [tls.acme]\n\
             enabled = true\n\
             directory_url = \"http://127.0.0.1:1/directory\"\n\
             contact_email = \"ops@example.com\"\n\
             terms_of_service_agreed = true\n\
             storage_dir = \"{storage}\"\n",
            http = Proxy::pick_port(),
            tls = Proxy::pick_port(),
            db = dir.path().join("deis-proxy.db").display(),
            pid = dir.path().join("deis-proxy.pid").display(),
            sock = dir.path().join("upgrade.sock").display(),
            storage = blocker.join("acme").display(),
        ),
    )
    .unwrap();

    let output = std::process::Command::new(Proxy::binary_path())
        .arg("-c")
        .arg(&config_path)
        .output()
        .expect("the proxy binary runs");

    assert!(
        !output.status.success(),
        "a proxy that cannot prepare its certificate store must not start"
    );
    let said = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        said.contains("automatic certificates"),
        "the failure has to name what it was doing; it said:\n{said}"
    );
}

// ----------------------------------------------------------------------
// A TLS client that trusts exactly one root
// ----------------------------------------------------------------------

/// One HTTPS request through the proxy, returning the leaf certificate that
/// was presented and the whole answer.
///
/// The trust store holds the stub authority's root and nothing else, and
/// verification is on: a proxy that presented a self-signed certificate, or
/// one for another name, fails the handshake here rather than being reported
/// as a success.
fn https_get(addr: std::net::SocketAddr, server_name: &str, root_pem: &[u8]) -> (X509, String) {
    let mut builder = SslConnector::builder(SslMethod::tls_client()).expect("a TLS client");
    builder.set_verify(SslVerifyMode::PEER);
    let root = X509::from_pem(root_pem).expect("the authority's root parses");
    builder
        .cert_store_mut()
        .add_cert(root)
        .expect("the root is trusted");
    let connector = builder.build();

    let stream = TcpStream::connect(addr).expect("the TLS port accepts a connection");
    let mut stream = connector
        .connect(server_name, stream)
        .expect("the handshake succeeds against a certificate signed by the stub authority");

    let leaf = stream
        .ssl()
        .peer_certificate()
        .expect("the server presented a certificate");
    let request = format!("GET / HTTP/1.1\r\nHost: {server_name}\r\nConnection: close\r\n\r\n");
    stream
        .write_all(request.as_bytes())
        .expect("the request is sent");
    let mut answer = Vec::new();
    let _ = stream.read_to_end(&mut answer);
    (leaf, String::from_utf8_lossy(&answer).into_owned())
}

fn subject_alt_names(certificate: &X509) -> Vec<String> {
    certificate
        .subject_alt_names()
        .map(|names| {
            names
                .iter()
                .filter_map(|name| name.dnsname().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}
