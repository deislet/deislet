//! A real TLS handshake against the real `deis-proxy` binary.
//!
//! Certificate selection happens inside pingora's TLS accept callback
//! ([`deis_proxy::tls::SniCertificates`]), which is the one place in this
//! crate that pingora calls rather than the other way round, and the one
//! nothing could reach from a unit test. `config::routes` already proves which
//! certificate *would* be chosen for a name; this proves that the chosen one
//! is installed into the handshake, that its chain goes out with it, and that
//! a request over the resulting connection reaches a node.
//!
//! The certificate is minted here, signed by a throwaway CA that the client
//! trusts and nothing else does. The client verifies the hostname, so a proxy
//! that served a certificate for some other name fails the test rather than
//! quietly succeeding.

mod harness;

use std::io::{Read, Write};
use std::net::TcpStream as StdTcpStream;
use std::time::Duration;

use harness::{BACKEND_BODY, Backend, Proxy};
use openssl::asn1::Asn1Time;
use openssl::bn::{BigNum, MsbOption};
use openssl::hash::MessageDigest;
use openssl::pkey::{PKey, Private};
use openssl::rsa::Rsa;
use openssl::ssl::{SslConnector, SslMethod, SslVerifyMode};
use openssl::x509::extension::{BasicConstraints, SubjectAlternativeName};
use openssl::x509::{X509, X509Builder, X509NameBuilder};

/// The name the client asks for, and the only name the leaf certificate
/// attests to.
const SERVER_NAME: &str = "shop.example.test";

/// A key and the certificate it belongs to.
struct Issued {
    cert: X509,
    key: PKey<Private>,
}

/// Mint a CA, and a leaf for `SERVER_NAME` signed by it.
///
/// A chain rather than a lone self-signed certificate on purpose: the leaf and
/// the CA go into one PEM, which is the shape an operator's certificate file
/// actually has, and it is the only way `ssl_add_chain_cert` in
/// `SniCertificates::certificate_callback` is exercised at all.
fn issue_chain() -> (Issued, Issued) {
    let ca_key = PKey::from_rsa(Rsa::generate(2048).expect("a CA key")).expect("a CA key");
    let mut name = X509NameBuilder::new().expect("a name builder");
    name.append_entry_by_text("CN", "deis-proxy test CA")
        .expect("the CA common name");
    let ca_name = name.build();

    let mut builder = X509Builder::new().expect("a certificate builder");
    builder.set_version(2).expect("X.509 v3");
    builder.set_subject_name(&ca_name).expect("the subject");
    builder.set_issuer_name(&ca_name).expect("the issuer");
    builder.set_pubkey(&ca_key).expect("the public key");
    builder
        .set_not_before(&Asn1Time::days_from_now(0).expect("a start date"))
        .expect("the start date");
    builder
        .set_not_after(&Asn1Time::days_from_now(1).expect("an end date"))
        .expect("the end date");
    builder
        .append_extension(
            BasicConstraints::new()
                .critical()
                .ca()
                .build()
                .expect("the CA constraint"),
        )
        .expect("the CA constraint");
    builder
        .sign(&ca_key, MessageDigest::sha256())
        .expect("a signed CA");
    let ca = Issued {
        cert: builder.build(),
        key: ca_key,
    };

    let leaf_key = PKey::from_rsa(Rsa::generate(2048).expect("a leaf key")).expect("a leaf key");
    let mut name = X509NameBuilder::new().expect("a name builder");
    name.append_entry_by_text("CN", SERVER_NAME)
        .expect("the leaf common name");
    let leaf_name = name.build();

    let mut builder = X509Builder::new().expect("a certificate builder");
    builder.set_version(2).expect("X.509 v3");
    // A serial number distinct from the CA's, which some verifiers insist on.
    let mut serial = BigNum::new().expect("a serial");
    serial
        .rand(64, MsbOption::MAYBE_ZERO, false)
        .expect("a random serial");
    builder
        .set_serial_number(&serial.to_asn1_integer().expect("the serial"))
        .expect("the serial");
    builder.set_subject_name(&leaf_name).expect("the subject");
    builder
        .set_issuer_name(ca.cert.subject_name())
        .expect("the issuer");
    builder.set_pubkey(&leaf_key).expect("the public key");
    builder
        .set_not_before(&Asn1Time::days_from_now(0).expect("a start date"))
        .expect("the start date");
    builder
        .set_not_after(&Asn1Time::days_from_now(1).expect("an end date"))
        .expect("the end date");
    let context = builder.x509v3_context(Some(&ca.cert), None);
    let san = SubjectAlternativeName::new()
        .dns(SERVER_NAME)
        .build(&context)
        .expect("the subject alternative name");
    builder.append_extension(san).expect("the SAN");
    builder
        .sign(&ca.key, MessageDigest::sha256())
        .expect("a signed leaf");
    let leaf = Issued {
        cert: builder.build(),
        key: leaf_key,
    };

    (ca, leaf)
}

#[tokio::test]
async fn a_tls_handshake_serves_the_configured_certificate_and_forwards_the_request() {
    let backend = Backend::start().await;

    let dir = tempfile::tempdir().expect("a directory for the certificate");
    let (ca, leaf) = issue_chain();
    let cert_file = dir.path().join("leaf.pem");
    let key_file = dir.path().join("leaf.key");
    // Leaf first, then the issuer: `LoadedCertificate::load` takes the first
    // certificate in the file as the leaf and everything after it as the
    // chain.
    let mut pem = leaf.cert.to_pem().expect("the leaf in PEM");
    pem.extend_from_slice(&ca.cert.to_pem().expect("the CA in PEM"));
    std::fs::write(&cert_file, &pem).expect("the certificate file is written");
    std::fs::write(
        &key_file,
        leaf.key
            .private_key_to_pem_pkcs8()
            .expect("the leaf key in PEM"),
    )
    .expect("the key file is written");

    let proxy = Proxy::start_with_tls(backend.addr, &cert_file, &key_file).await;
    let tls_addr = proxy.tls_addr.expect("a TLS listener was configured");

    // The client's own trust store holds the throwaway CA and nothing else,
    // and hostname verification is on, so the handshake only succeeds if the
    // proxy installed this certificate for this name.
    let ca_pem = ca.cert.to_pem().expect("the CA in PEM");
    let answer = tokio::task::spawn_blocking(move || {
        let mut connector = SslConnector::builder(SslMethod::tls()).expect("a TLS client");
        connector.set_verify(SslVerifyMode::PEER);
        connector
            .cert_store_mut()
            .add_cert(X509::from_pem(&ca_pem).expect("the CA parses"))
            .expect("the CA is trusted");
        let connector = connector.build();

        let tcp = StdTcpStream::connect(tls_addr).expect("connecting to the TLS port");
        tcp.set_read_timeout(Some(Duration::from_secs(10)))
            .expect("a read timeout");
        let mut stream = connector
            .configure()
            .expect("a TLS configuration")
            .verify_hostname(true)
            .use_server_name_indication(true)
            .connect(SERVER_NAME, tcp)
            .expect("the TLS handshake succeeds and the name verifies");

        let served = stream
            .ssl()
            .peer_certificate()
            .expect("the server sent a certificate");
        let served_names: Vec<String> = served
            .subject_alt_names()
            .map(|sans| {
                sans.iter()
                    .filter_map(|name| name.dnsname().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        assert_eq!(
            served_names,
            vec![SERVER_NAME.to_string()],
            "the proxy served a certificate for some other name"
        );

        let chain_len = stream
            .ssl()
            .peer_cert_chain()
            .map(|chain| chain.len())
            .unwrap_or(0);
        assert!(
            chain_len >= 2,
            "the issuer did not go out with the leaf, so a client that does not already hold \
             the CA cannot build a path; the server sent {chain_len} certificate(s)"
        );

        // The point of terminating TLS at all: the request underneath it is
        // proxied to a node like any other.
        let request = format!("GET / HTTP/1.1\r\nHost: {SERVER_NAME}\r\nConnection: close\r\n\r\n");
        stream
            .write_all(request.as_bytes())
            .expect("the request is sent over TLS");
        let mut answer = Vec::new();
        // An abrupt close after the body is how a peer that sends
        // `Connection: close` often ends; the bytes already read still count.
        let _ = stream.read_to_end(&mut answer);
        String::from_utf8_lossy(&answer).to_string()
    })
    .await
    .expect("the TLS client thread finishes");

    assert!(
        answer.starts_with("HTTP/1.1 200 "),
        "a request over TLS did not reach the backend; the proxy answered:\n{answer}\n\
         its log said:\n{}",
        proxy.log_tail()
    );
    assert!(
        answer.ends_with(BACKEND_BODY),
        "the body over TLS is not the backend's; the proxy answered:\n{answer}"
    );

    drop(proxy);
    backend.stop();
}
