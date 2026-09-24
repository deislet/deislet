//! What reaches a node, and what does not, when the edge cannot place a
//! request.
//!
//! Both cases here were found against a running proxy and are asserted
//! against one: the binary from `target/debug` with a real configuration
//! file, and a backend that records the head of every request it is handed.
//! The routing headers are the reason the backend has to be real - a node
//! reads `x-deis-app-id` to decide which application answers and which
//! storage tenant it opens, and it cannot tell a value the client wrote from
//! one the edge stamped, so the only place the difference is visible is on
//! this side of the hop.

mod harness;

use harness::{BACKEND_BODY, Backend, Proxy, ProxyOptions, TlsOptions};

/// The trust domain the certificate below is issued in. Any name will do:
/// nothing on the other end ever reads it, because nothing on the other end
/// exists.
const TRUST_DOMAIN: &str = "edge-authority.test";

/// A control plane address on a port nothing is listening on, and the identity
/// this proxy would prove to it.
///
/// The state a proxy is in whenever it starts before its control plane, or
/// while the control plane is being upgraded, or when the address is simply
/// wrong. It is not exotic: it is the first few seconds of every deployment
/// where the two come up together.
///
/// The certificate has to be real even though no handshake will ever happen
/// with it: a proxy attached to a control plane refuses to start without a
/// `[machine_identity]` table, because everything it would ask that control
/// plane for is served only on the machine channel, and one that started
/// without a certificate would poll a route that answers 404 for as long as it
/// ran. The files are minted here rather than checked in so that an expiry
/// cannot make this test fail on a date rather than on a behaviour.
///
/// The directory comes back with the configuration because deleting it would
/// take the certificate out from under the running process.
fn unreachable_control() -> (String, tempfile::TempDir) {
    let port = Proxy::pick_port();
    let directory = tempfile::tempdir().expect("a directory for this proxy's identity");
    let authority = deis_identity_test_pki::TestCa::new();
    let issued = authority.issue(
        &deis_identity::names::proxy(TRUST_DOMAIN, "deis-proxy-test").expect("a proxy name"),
    );
    let files = deis_identity_test_pki::write_files(
        directory.path(),
        "proxy",
        &issued,
        &authority.root_pem,
    );
    let config = format!(
        "\n[control]\n\
         addr = \"http://127.0.0.1:{port}\"\n\
         associated_id = \"deis-proxy-test\"\n\
         \n[machine_identity]\n\
         control_endpoint = \"https://127.0.0.1:{port}\"\n\
         certificate = \"{}\"\n\
         private_key = \"{}\"\n\
         trust_root = \"{}\"\n\
         trust_domain = \"{TRUST_DOMAIN}\"\n",
        files.certificate.display(),
        files.private_key.display(),
        files.trust_root.display(),
    );
    (config, directory)
}

fn status_line(answer: &str) -> &str {
    answer.lines().next().unwrap_or("")
}

#[tokio::test]
async fn a_control_plane_that_never_answered_refuses_instead_of_forwarding() {
    let backend = Backend::start().await;
    // Held for the life of the test: the proxy reads these files on an
    // interval, so a directory dropped here would be one the process watches
    // disappear.
    let (control_config, _identity) = unreachable_control();
    let proxy = Proxy::launch(ProxyOptions {
        upstream: backend.addr,
        extra_config: control_config,
        // A proxy in this state forwards nothing at all, so "the backend's
        // body came back" is not a readiness signal it can ever produce.
        wait_for_forwarding: false,
        ..Default::default()
    })
    .await;

    // Exactly the request that reached a different storage tenant through the
    // public edge before this was closed: a `Host` nothing here claims and a
    // routing pair the client chose for itself. The routing pair is deleted
    // before routing is even attempted now, and the unmatched `Host` alone
    // decides the refusal.
    let answer = proxy
        .request_with_headers(
            "/whatever",
            "victim.example",
            &[("x-deis-app-id", "demo"), ("x-deis-environment", "preview")],
        )
        .await;
    assert!(
        status_line(&answer).starts_with("HTTP/1.1 503"),
        "an instance told where its routing comes from, and never told what it is, must refuse \
         the request rather than route it by the client's own header; it answered {:?}",
        status_line(&answer)
    );

    // And the same for the node's internal application, which is the one
    // whose name is worth guessing. The header is gone either way; a host of
    // that name would not route in any state of this instance.
    let answer = proxy
        .request_with_headers("/", "victim.example", &[("x-deis-app-id", "_system")])
        .await;
    assert!(
        status_line(&answer).starts_with("HTTP/1.1 503"),
        "it answered {:?}",
        status_line(&answer)
    );

    assert!(
        backend.heads().is_empty(),
        "not one of these may cross the hop; the node was handed {:#?}",
        backend.heads()
    );
    assert!(
        proxy
            .log_text()
            .contains("no routing table has ever been fetched"),
        "the refusal has to say why, or an operator sees 503s and no cause; the log said:\n{}",
        proxy.log_tail()
    );
    backend.stop();
}

#[tokio::test]
async fn a_plain_reverse_proxy_strips_the_routing_headers_the_client_sent() {
    // No [control] section and no applications: the supported bare
    // deployment, where every request is forwarded because deis-proxy has no
    // opinion about any of them. Having no opinion is exactly why the
    // client's own pair cannot be allowed through - the node downstream
    // would read it as this edge's decision.
    let backend = Backend::start().await;
    let proxy = Proxy::start(backend.addr).await;

    let answer = proxy
        .request_with_headers(
            "/",
            "anything.example",
            &[
                ("x-deis-app-id", "_system"),
                ("x-deis-environment", "preview"),
            ],
        )
        .await;
    assert!(
        answer.ends_with(BACKEND_BODY),
        "a plain reverse proxy still forwards the request; it answered {answer:?}"
    );
    assert_eq!(
        backend.last_header("x-deis-app-id"),
        None,
        "the node was handed an application id nothing on this edge decided; the head was:\n{:?}",
        backend.heads().last()
    );
    assert_eq!(
        backend.last_header("x-deis-environment"),
        None,
        "the node was handed an environment nothing on this edge decided; the head was:\n{:?}",
        backend.heads().last()
    );
    backend.stop();
}

#[tokio::test]
async fn a_client_chosen_host_is_not_a_redirect_target() {
    // `auto_redirect_to_https` is on by default, the `Location` is built from
    // the request's own `Host`, and a default certificate is served for every
    // name that matches nothing - so "is HTTPS available for this host" used
    // to be true for every host on earth once one certificate existed. A
    // client pairing a known application with any `Host` it liked was routed
    // by that header and answered 301 to the host it chose, off the edge's
    // domain and signed by it. The header is deleted before routing now, so
    // the pairing routes nothing at all.
    let backend = Backend::start().await;
    let dir = tempfile::tempdir().expect("a directory for the certificate");
    let (cert_file, key_file) = certificate(dir.path(), "edge.example");

    // The application names the certificate, so it genuinely terminates TLS
    // and the redirect is a feature it is entitled to. The same file is also
    // the default certificate, which is the piece that used to widen "HTTPS
    // is available for this host" to every host there is.
    let proxy = Proxy::launch(ProxyOptions {
        upstream: backend.addr,
        tls: Some(TlsOptions {
            port: None,
            default_certificate: Some((cert_file.clone(), key_file.clone())),
        }),
        apps: vec![serde_json::json!({
            "id": "shop",
            "description": "",
            "domains": ["edge.example"],
            "certificates": [{
                "cert_file": cert_file.to_string_lossy(),
                "key_file": key_file.to_string_lossy(),
            }],
            "enabled": true,
            "cache": [],
        })],
        wait_for_forwarding: false,
        ..Default::default()
    })
    .await;

    // The same Host the client chose, still paired with the header: the
    // header no longer routes, so the request is answered 404 on a Host
    // nothing here claims - and in particular not a 301 built from it.
    let answer = proxy
        .request_with_headers("/login", "evil.example", &[("x-deis-app-id", "shop")])
        .await;
    assert!(
        status_line(&answer).starts_with("HTTP/1.1 404"),
        "a Host nothing here claims does not route, whatever its header names; it answered {}",
        answer.lines().take(3).collect::<Vec<_>>().join(" | ")
    );

    // The name the edge does hold a certificate for still redirects, so this
    // is a narrower rule and not a switched-off feature.
    let answer = proxy.request("/login", "edge.example").await;
    assert!(
        status_line(&answer).starts_with("HTTP/1.1 301") && answer.contains("https://edge.example"),
        "a name this edge serves over HTTPS still redirects to it: {}",
        answer.lines().take(3).collect::<Vec<_>>().join(" | ")
    );
    backend.stop();
}

#[tokio::test]
async fn a_client_chosen_app_id_header_does_not_route() {
    // Before the header was deleted before routing, `x-deis-app-id` named any
    // application the edge knew, whether or not anything routed to it
    // publicly: an application with no route at all, and the node's own
    // `_system`, were both reachable by asking. The header is deleted before
    // the `Host` is even read now, so a request addressed to one application
    // while naming another in the header is answered by the one its Host
    // names.
    let backend = Backend::start().await;
    let proxy = Proxy::launch(ProxyOptions {
        upstream: backend.addr,
        apps: vec![
            serde_json::json!({
                "id": "shop",
                "description": "",
                "domains": ["shop.example"],
                "certificates": [],
                "enabled": true,
                "cache": [],
            }),
            serde_json::json!({
                "id": "blog",
                "description": "",
                "domains": ["blog.example"],
                "certificates": [],
                "enabled": true,
                "cache": [],
            }),
        ],
        // With applications configured an unknown host is a 404, so a
        // forwarded request cannot be the readiness probe.
        wait_for_forwarding: false,
        ..Default::default()
    })
    .await;

    // Addressed to shop, naming blog in the deleted namespace: blog is not
    // reachable this way, and the request goes where its Host says.
    let answer = proxy
        .request_with_headers("/", "shop.example", &[("x-deis-app-id", "blog")])
        .await;
    assert!(
        answer.ends_with(BACKEND_BODY),
        "the request routes by its Host; it answered {answer:?}"
    );
    assert_eq!(
        backend.last_header("x-deis-app-id").as_deref(),
        Some("shop"),
        "the node was handed the id this edge resolved, not the one the client typed; the head \
         was:\n{:?}",
        backend.heads().last()
    );

    // The node's own system application is not reachable by name either: a
    // Host no route claims is a 404 whatever the header used to say.
    let answer = proxy
        .request_with_headers("/", "victim.example", &[("x-deis-app-id", "_system")])
        .await;
    assert!(
        status_line(&answer).starts_with("HTTP/1.1 404"),
        "a Host nothing here claims does not route; it answered {}",
        answer.lines().take(3).collect::<Vec<_>>().join(" | ")
    );
    backend.stop();
}

#[tokio::test]
async fn the_platform_namespace_does_not_survive_the_edge() {
    // The routing pair was being replaced and nothing else in `x-deis-` was
    // touched, so a request the edge routed perfectly well still arrived at
    // the node carrying whatever the client had written under that prefix.
    // `x-deis-caller` is the one that pays: the node stamps it over a service
    // call and the platform documents it as evidence a callee may branch on,
    // so an outside client sending `x-deis-caller: admin-app.production`
    // through this hop was read by a callee following that documented pattern
    // as proof an admin application had called it. The storage three are the
    // same shape one hop further in.
    let backend = Backend::start().await;
    let proxy = Proxy::launch(ProxyOptions {
        upstream: backend.addr,
        apps: vec![serde_json::json!({
            "id": "shop",
            "description": "",
            "domains": ["shop.example"],
            "certificates": [],
            "enabled": true,
            "cache": [],
        })],
        // With an application configured an unknown host is a 404, so a
        // forwarded request cannot be the readiness probe.
        wait_for_forwarding: false,
        ..Default::default()
    })
    .await;

    let answer = proxy
        .request_with_headers(
            "/",
            "shop.example",
            &[
                ("x-deis-caller", "admin-app.production"),
                ("x-deis-tenant", "victim.production"),
                ("x-deis-storage-token", "forged"),
                ("x-deis-binding", "HOT"),
                // Not a header any component reads today. The rule is the
                // prefix, so the one a future component invents is already
                // covered rather than arriving forgeable.
                ("x-deis-not-a-header-yet", "whatever"),
                // An application's own header, which is not the platform's
                // business and has to keep crossing.
                ("x-shop-request-id", "abc123"),
            ],
        )
        .await;
    assert!(
        answer.ends_with(BACKEND_BODY),
        "the request is still routed and forwarded; it answered {answer:?}"
    );

    for name in [
        "x-deis-caller",
        "x-deis-tenant",
        "x-deis-storage-token",
        "x-deis-binding",
        "x-deis-not-a-header-yet",
    ] {
        assert_eq!(
            backend.last_header(name),
            None,
            "the node was handed {name} with a value the client wrote, and it cannot tell that \
             from one this edge stamped; the head was:\n{:?}",
            backend.heads().last()
        );
    }
    assert_eq!(
        backend.last_header("x-shop-request-id").as_deref(),
        Some("abc123"),
        "only the platform's own prefix is closed; the head was:\n{:?}",
        backend.heads().last()
    );

    // The edge's own decision still crosses, so this is a namespace taken
    // back rather than a hop that stopped saying anything.
    assert_eq!(
        backend.last_header("x-deis-app-id").as_deref(),
        Some("shop")
    );
    assert_eq!(
        backend.last_header("x-deis-environment").as_deref(),
        Some("production")
    );

    assert!(
        proxy
            .log_text()
            .contains("platform's own x-deis- namespace"),
        "an operator has to see that someone tried to forge platform evidence, even though it \
         never reached the node; the log said:\n{}",
        proxy.log_tail()
    );
    backend.stop();
}

/// A throwaway self-signed certificate on disk, for the default certificate
/// slot. Its subject names are what decides which hosts this edge will admit
/// to serving over HTTPS.
fn certificate(dir: &std::path::Path, name: &str) -> (std::path::PathBuf, std::path::PathBuf) {
    use openssl::asn1::Asn1Time;
    use openssl::hash::MessageDigest;
    use openssl::pkey::PKey;
    use openssl::x509::extension::SubjectAlternativeName;
    use openssl::x509::{X509, X509NameBuilder};

    let key = PKey::generate_ed25519().expect("a key");
    let mut subject = X509NameBuilder::new().expect("a name builder");
    subject.append_entry_by_text("CN", name).expect("a CN");
    let subject = subject.build();

    let mut builder = X509::builder().expect("a certificate builder");
    builder.set_version(2).expect("v3");
    builder.set_subject_name(&subject).expect("the subject");
    builder.set_issuer_name(&subject).expect("the issuer");
    builder.set_pubkey(&key).expect("the public key");
    builder
        .set_not_before(&Asn1Time::days_from_now(0).expect("now"))
        .expect("not before");
    builder
        .set_not_after(&Asn1Time::days_from_now(1).expect("tomorrow"))
        .expect("not after");
    let alt = SubjectAlternativeName::new()
        .dns(name)
        .build(&builder.x509v3_context(None, None))
        .expect("a subject alternative name");
    builder.append_extension(alt).expect("the extension");
    // Ed25519 signatures embed their own digest.
    builder
        .sign(&key, MessageDigest::null())
        .expect("a signature");
    let certificate = builder.build();

    let cert_file = dir.join("default.pem");
    let key_file = dir.join("default.key");
    std::fs::write(&cert_file, certificate.to_pem().expect("PEM")).expect("the certificate");
    std::fs::write(
        &key_file,
        key.private_key_to_pem_pkcs8().expect("the key in PEM"),
    )
    .expect("the key");
    (cert_file, key_file)
}
