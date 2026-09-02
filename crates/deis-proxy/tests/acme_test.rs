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
    acme_config_with(ca, storage_dir, renew_before_days, 20)
}

fn acme_config_with(
    ca: &StubCa,
    storage_dir: &Path,
    renew_before_days: i64,
    max_names: usize,
) -> String {
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
         order_timeout_secs = 30\n\
         max_names = {max_names}\n",
        directory = ca.directory_url,
        storage = storage_dir.display(),
    )
}

/// How many renewal passes the proxy has finished, from its log.
fn passes_finished(proxy: &Proxy) -> usize {
    proxy
        .log_text()
        .matches("automatic certificate pass finished")
        .count()
}

/// A self-signed pair for `name` written into the store where the proxy
/// would have put one it obtained, ending `ending_in_secs` from now -
/// negative for a certificate that has already expired. The key written is
/// the certificate's own unless `matching` is false.
fn plant_pair(dir: &Path, name: &str, ending_in_secs: i64, matching: bool) {
    use openssl::asn1::Asn1Time;
    use openssl::hash::MessageDigest;
    use openssl::pkey::PKey;
    use openssl::x509::X509NameBuilder;
    use openssl::x509::extension::SubjectAlternativeName;

    let key = PKey::generate_ed25519().unwrap();
    let mut subject = X509NameBuilder::new().unwrap();
    subject.append_entry_by_text("CN", name).unwrap();
    let subject = subject.build();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    let mut builder = X509::builder().unwrap();
    builder.set_version(2).unwrap();
    builder.set_subject_name(&subject).unwrap();
    builder.set_issuer_name(&subject).unwrap();
    builder.set_pubkey(&key).unwrap();
    builder
        .set_not_before(&Asn1Time::from_unix(now + ending_in_secs - 90 * 86_400).unwrap())
        .unwrap();
    builder
        .set_not_after(&Asn1Time::from_unix(now + ending_in_secs).unwrap())
        .unwrap();
    let context = builder.x509v3_context(None, None);
    let san = SubjectAlternativeName::new()
        .dns(name)
        .build(&context)
        .unwrap();
    builder.append_extension(san).unwrap();
    builder.sign(&key, MessageDigest::null()).unwrap();
    let certificate = builder.build();

    std::fs::create_dir_all(dir).unwrap();
    let written_key = if matching {
        key
    } else {
        PKey::generate_ed25519().unwrap()
    };
    std::fs::write(dir.join("fullchain.pem"), certificate.to_pem().unwrap()).unwrap();
    std::fs::write(
        dir.join("privkey.pem"),
        written_key.private_key_to_pem_pkcs8().unwrap(),
    )
    .unwrap();
}

/// Whether a TLS handshake for `server_name` completes at all, with no
/// verification: the question is "does the proxy present anything", not
/// "is it trusted".
fn handshake_succeeds(addr: std::net::SocketAddr, server_name: &str) -> bool {
    let mut builder = SslConnector::builder(SslMethod::tls_client()).expect("a TLS client");
    builder.set_verify(SslVerifyMode::NONE);
    let connector = builder.build();
    let Ok(stream) = TcpStream::connect(addr) else {
        return false;
    };
    connector.connect(server_name, stream).is_ok()
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
        Fixture::start_with(apps, options, renew_before_days, 20, |_| {}).await
    }

    /// As [`Fixture::start`], with the `max_names` ceiling chosen and a hook
    /// run on the store's authority directory before the proxy starts - how
    /// a test plants a pair the proxy has to find already on disk.
    async fn start_with(
        apps: Vec<serde_json::Value>,
        options: CaOptions,
        renew_before_days: i64,
        max_names: usize,
        prepare_store: impl FnOnce(&Path),
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
        let extra_config = acme_config_with(&ca, &storage_dir, renew_before_days, max_names);
        prepare_store(&storage_dir.join(authority_key(&ca)));

        let proxy = Proxy::launch(ProxyOptions {
            upstream: backend.addr,
            http_port: Some(http_port),
            tls: Some(TlsOptions {
                port: None,
                default_certificate: None,
            }),
            extra_config,
            upgrade_sock: None,
            top_level_config: String::new(),
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

    /// The store's directory for this authority.
    fn authority_dir(&self) -> PathBuf {
        self.storage_dir.join(authority_key(&self.ca))
    }

    /// Wait until the proxy has finished `more` passes beyond `since`.
    async fn passes(&self, since: usize, more: usize) {
        until("the renewal task runs more passes", || {
            passes_finished(&self.proxy) >= since + more
        })
        .await;
    }
}

// ----------------------------------------------------------------------
// The pair on disk is judged by one rule, and what is served follows it
// ----------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn a_key_that_stopped_matching_its_certificate_is_reordered_not_called_current() {
    // The route table refused this pair and the renewal pass called it
    // "still current" for the rest of its ninety days: no handshake for the
    // name, a DEBUG line per pass, and a WARN at startup promising a
    // reorder that never came. Now the pass reads the same verdict the
    // loader does, orders a replacement, and the name serves again.
    let fixture = Fixture::start(vec![app("shop", &[NAME])], CaOptions::default(), 30).await;
    until("the first certificate is installed", || {
        fixture.proxy.log_text().contains("certificate installed")
    })
    .await;
    assert_eq!(fixture.ca.issued_count(), 1);
    let stale_chain = std::fs::read(fixture.chain_path(NAME)).unwrap();

    // The key file replaced by a key for nothing in particular: the shape a
    // crash between the two renames leaves, and the shape a second writer
    // on the same store produced in the round that found this.
    let other_key = openssl::pkey::PKey::generate_ed25519().unwrap();
    std::fs::write(
        fixture.key_path(NAME),
        other_key.private_key_to_pem_pkcs8().unwrap(),
    )
    .unwrap();

    until("the mismatch is noticed and a replacement ordered", || {
        fixture.ca.issued_count() >= 2
    })
    .await;
    let log = fixture.proxy.log_text();
    assert!(
        log.contains("does not match the certificate"),
        "the pass has to say what was wrong with the pair:\n{log}"
    );
    assert!(
        log.contains("cannot be served and has not been, so a replacement is being ordered"),
        "the reorder has to be announced at error, not debug:\n{log}"
    );
    until("the replacement is installed and served", || {
        std::fs::read(fixture.chain_path(NAME)).ok().as_deref() != Some(&stale_chain[..])
            && fixture
                .proxy
                .log_text()
                .matches("certificate installed")
                .count()
                >= 2
    })
    .await;
    let tls_addr = fixture.proxy.tls_addr.expect("a TLS listener");
    let root = fixture.ca.root_pem();
    let (leaf, _) = tokio::task::spawn_blocking(move || https_get(tls_addr, NAME, &root))
        .await
        .expect("the handshake task finishes");
    assert_eq!(subject_alt_names(&leaf), vec![NAME.to_string()]);
    drop(fixture.backend);
}

#[tokio::test(flavor = "multi_thread")]
async fn an_expired_certificate_is_not_served_and_plain_http_is_not_redirected_onto_it() {
    // The renewal-failure ERROR promises "plain HTTP only, no redirect".
    // With an expired pair on disk and every order failing, the proxy used
    // to keep the expired certificate loaded, present it, and answer every
    // plain request with a 301 onto it. The route table now refuses an
    // expired automatic pair, and the redirect is computed from the table.
    let fixture = Fixture::start_with(
        vec![app("shop", &[NAME])],
        CaOptions {
            sabotage_validation: true,
            ..Default::default()
        },
        30,
        20,
        |authority| {
            plant_pair(
                &authority.join("certificates").join(NAME),
                NAME,
                -10 * 86_400,
                true,
            )
        },
    )
    .await;

    until("the failed renewal is reported", || {
        fixture.proxy.log_text().contains("obtaining a certificate")
    })
    .await;
    let log = fixture.proxy.log_text();
    assert!(
        log.contains("has expired and is not served"),
        "the loader has to say the expired pair was left out:\n{log}"
    );

    let answer = fixture.proxy.request("/", NAME).await;
    assert!(
        !answer.starts_with("HTTP/1.1 301"),
        "plain HTTP must not be redirected onto an expired certificate: {answer}"
    );
    let tls_addr = fixture.proxy.tls_addr.expect("a TLS listener");
    let served = tokio::task::spawn_blocking(move || handshake_succeeds(tls_addr, NAME))
        .await
        .unwrap();
    assert!(
        !served,
        "an expired automatic certificate must not be presented"
    );

    // The redirect follows the TLS state in both directions: once a renewal
    // succeeds, the name is served again and plain HTTP is redirected.
    fixture.ca.sabotage_validation(false);
    until("the renewal succeeds", || {
        fixture.proxy.log_text().contains("certificate installed")
    })
    .await;
    let deadline = Instant::now() + DEADLINE;
    loop {
        if fixture
            .proxy
            .request("/", NAME)
            .await
            .starts_with("HTTP/1.1 301")
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "plain HTTP was not redirected again within {DEADLINE:?} of the renewal succeeding"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    drop(fixture.backend);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_name_the_ceiling_leaves_out_is_named_on_every_pass() {
    // With `max_names = 1` and two names, one is not covered. It used to be
    // counted once at startup and never mentioned again, while whatever
    // certificate it held was served with the redirect on until it expired.
    let fixture = Fixture::start_with(
        vec![app("shop", &[NAME]), app("blog", &[OTHER_NAME])],
        CaOptions::default(),
        30,
        1,
        |_| {},
    )
    .await;
    // Names out of this instance's own configuration are taken in order:
    // blog.test fits, shop.test does not.
    fixture.passes(0, 3).await;
    let log = fixture.proxy.log_text();
    let per_name = log
        .lines()
        .filter(|line| {
            line.contains("does not fit under") && line.contains(&format!("name={NAME}"))
        })
        .count();
    assert!(
        per_name >= 3,
        "the uncovered name has to be named on every pass, saw it {per_name} times:\n{log}"
    );
    assert!(
        log.contains("uncovered=1"),
        "the pass summary has to carry the count:\n{log}"
    );
    assert_eq!(fixture.ca.issued(), vec![OTHER_NAME.to_string()]);
    drop(fixture.backend);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_store_that_cannot_be_written_costs_no_certificate() {
    // The store is checked before the order is placed. It used to be
    // checked after: issue, fail to save, back off, issue again - one of the
    // authority's weekly certificates per retry, five inside a quarter of an
    // hour with the production backoff.
    let fixture = Fixture::start(vec![app("shop", &[NAME])], CaOptions::default(), 30).await;
    until("the first certificate is installed", || {
        fixture.proxy.log_text().contains("certificate installed")
    })
    .await;
    assert_eq!(fixture.ca.issued_count(), 1);

    // Break the store the way the round did: the pair gone and a directory
    // where the key file belongs, which no rename can replace.
    std::fs::remove_file(fixture.chain_path(NAME)).unwrap();
    std::fs::remove_file(fixture.key_path(NAME)).unwrap();
    std::fs::create_dir(fixture.key_path(NAME)).unwrap();

    until("the store fault is reported before any order", || {
        fixture
            .proxy
            .log_text()
            .contains("the certificate store cannot be written, so no certificate is ordered")
    })
    .await;
    let since = passes_finished(&fixture.proxy);
    fixture.passes(since, 3).await;
    assert_eq!(
        fixture.ca.issued_count(),
        1,
        "a broken store must not cost a certificate per retry: {:?}",
        fixture.ca.issued()
    );
    assert!(
        !fixture
            .proxy
            .log_text()
            .contains("was issued and could not be stored"),
        "nothing should have been issued into a store known to be broken"
    );

    // Fixed, the next attempt orders exactly one.
    std::fs::remove_dir(fixture.key_path(NAME)).unwrap();
    until("the name is certified again", || {
        fixture.ca.issued_count() == 2
    })
    .await;
    until("and installed", || {
        fixture
            .proxy
            .log_text()
            .matches("certificate installed")
            .count()
            >= 2
    })
    .await;
    drop(fixture.backend);
}

#[tokio::test(flavor = "multi_thread")]
async fn two_proxies_on_one_store_order_each_name_once() {
    // Two processes pointed at the same storage directory: the second half
    // of a zero-downtime upgrade, or plainly two proxies. Both used to
    // decide every due name was due and order it, and their two-file
    // writes crossed. The renewal lock makes the pass one writer at a time;
    // the other finds the certificate on disk and orders nothing.
    let fixture = Fixture::start(vec![app("shop", &[NAME])], CaOptions::default(), 30).await;
    let second = Proxy::launch(ProxyOptions {
        upstream: fixture.backend.addr,
        http_port: None,
        tls: Some(TlsOptions {
            port: None,
            default_certificate: None,
        }),
        extra_config: acme_config(&fixture.ca, &fixture.storage_dir, 30),
        upgrade_sock: None,
        top_level_config: String::new(),
        apps: vec![app("shop", &[NAME])],
        dir: None,
        wait_for_forwarding: false,
    })
    .await;

    until("both proxies have run three passes or skipped them", || {
        let ran = |proxy: &Proxy| {
            passes_finished(proxy)
                + proxy
                    .log_text()
                    .matches("another process is running a renewal pass")
                    .count()
        };
        ran(&fixture.proxy) >= 3 && ran(&second) >= 3
    })
    .await;
    assert_eq!(
        fixture.ca.issued(),
        vec![NAME.to_string()],
        "one name, two writers, one certificate"
    );
    // And both serve it: the one that did not write it found it on disk.
    let tls_a = fixture.proxy.tls_addr.unwrap();
    let tls_b = second.tls_addr.unwrap();
    until("both proxies present the certificate", || {
        handshake_succeeds(tls_a, NAME) && handshake_succeeds(tls_b, NAME)
    })
    .await;
    drop(second);
    drop(fixture.backend);
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
        upgrade_sock: None,
        top_level_config: String::new(),
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

/// An authority that walks an order backwards is named for what it is,
/// straight away.
///
/// Before the order status became a table, this client held a list of the
/// statuses that meant "keep polling" and `ready` was on it. An authority
/// that answered `processing` to finalize and then `ready` on every poll
/// therefore looked like an order still in progress: the client polled it
/// for the whole `order_timeout` - two minutes by default - and then
/// reported a stall, which says the authority was slow rather than that it
/// broke the protocol. RFC 8555 section 7.1.6 has no edge from `processing`
/// back to `ready`, so it is now refused on the first poll that shows it.
#[tokio::test(flavor = "multi_thread")]
async fn an_authority_that_moves_an_order_backwards_is_refused_at_once() {
    let fixture = Fixture::start(
        vec![app("shop", &[NAME])],
        CaOptions {
            regress_order_after_finalize: true,
            ..Default::default()
        },
        30,
    )
    .await;

    until("the illegal step is reported", || {
        fixture
            .proxy
            .log_text()
            .contains("RFC 8555 section 7.1.6 does not allow")
    })
    .await;

    let log = fixture.proxy.log_text();
    assert!(
        log.contains("from processing to ready"),
        "the log has to say which step the authority made:\n{log}"
    );
    assert!(
        log.contains(NAME),
        "the log has to name which order it was:\n{log}"
    );
    assert!(
        !log.contains("was still"),
        "this is a protocol fault, not the order timeout it used to be \
         reported as:\n{log}"
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

    // An answer is served only under the host it was published for. The
    // challenge directory is the handover path between two processes, and
    // before this it answered any token-shaped file for any Host on either
    // listener - a file server in front of every routing decision.
    let challenges = fixture.authority_dir().join("challenges");
    std::fs::write(
        challenges.join("planted-for-shop"),
        format!("{NAME}\nplanted"),
    )
    .unwrap();
    std::fs::write(challenges.join("planted-bare"), "planted").unwrap();
    let path = "/.well-known/acme-challenge/planted-for-shop";
    let under_own_host = fixture.proxy.request(path, NAME).await;
    assert!(
        under_own_host.starts_with("HTTP/1.1 200") && under_own_host.ends_with("planted"),
        "a handover answer is served under the name it was published for: {under_own_host}"
    );
    let under_other_host = fixture.proxy.request(path, "evil.example").await;
    assert!(
        under_other_host.starts_with("HTTP/1.1 404"),
        "the same token under another host must be refused: {under_other_host}"
    );
    let bare = fixture
        .proxy
        .request("/.well-known/acme-challenge/planted-bare", NAME)
        .await;
    assert!(
        bare.starts_with("HTTP/1.1 404"),
        "a file with no name line was not written by this store and is not an answer: {bare}"
    );
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
        upgrade_sock: None,
        top_level_config: String::new(),
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
