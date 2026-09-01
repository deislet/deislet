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

/// A syntactically valid `control.secret`. Nothing listens at the address it
/// is presented to; it only has to pass the configuration file's own check.
const NODE_SECRET: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

/// A control plane address on a port nothing is listening on.
///
/// The state a proxy is in whenever it starts before its control plane, or
/// while the control plane is being upgraded, or when the address is simply
/// wrong. It is not exotic: it is the first few seconds of every deployment
/// where the two come up together.
fn unreachable_control() -> String {
    let port = Proxy::pick_port();
    format!(
        "\n[control]\n\
         addr = \"http://127.0.0.1:{port}\"\n\
         associated_id = \"deis-proxy-test\"\n\
         secret = \"{NODE_SECRET}\"\n"
    )
}

fn status_line(answer: &str) -> &str {
    answer.lines().next().unwrap_or("")
}

#[tokio::test]
async fn a_control_plane_that_never_answered_refuses_instead_of_forwarding() {
    let backend = Backend::start().await;
    let proxy = Proxy::launch(ProxyOptions {
        upstream: backend.addr,
        extra_config: unreachable_control(),
        // A proxy in this state forwards nothing at all, so "the backend's
        // body came back" is not a readiness signal it can ever produce.
        wait_for_forwarding: false,
        ..Default::default()
    })
    .await;

    // Exactly the request that reached a different storage tenant through the
    // public edge before this was closed: a `Host` nothing here claims and a
    // routing pair the client chose for itself.
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
    // whose name is worth guessing.
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
    // client naming a known application and any `Host` it liked was answered
    // 301 to that host, off the edge's domain and signed by it.
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

    let answer = proxy
        .request_with_headers("/login", "evil.example", &[("x-deis-app-id", "shop")])
        .await;
    assert!(
        !status_line(&answer).starts_with("HTTP/1.1 301"),
        "the edge sent a client to a host it holds no certificate for and never routed: {}",
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
