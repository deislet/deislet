//! Which hosts the node key is sent to, and which it is not.

use super::*;

fn credential() -> NodeCredential {
    NodeCredential::new("node-key", Some("http://control.internal:8080"))
}

#[test]
fn the_configured_control_plane_gets_the_key() {
    let credential = credential();
    assert_eq!(
        credential.header_value_for("http://control.internal:8080/api/apps/a/versions/v/bundle"),
        Some("Bearer node-key")
    );
}

#[test]
fn a_bundle_url_on_another_host_goes_out_anonymous() {
    let credential = credential();
    assert_eq!(
        credential.header_value_for("http://cdn.example.com:8080/bundles/v.zip"),
        None,
        "the fleet's node key must not reach a host the operator never named"
    );
}

#[test]
fn another_port_on_the_same_host_is_another_origin() {
    let credential = credential();
    assert_eq!(
        credential.header_value_for("http://control.internal:9090/bundle"),
        None
    );
}

#[test]
fn the_default_port_and_the_written_port_are_one_origin() {
    let credential = NodeCredential::new("node-key", Some("https://control.internal"));
    assert_eq!(
        credential.header_value_for("https://control.internal:443/bundle"),
        Some("Bearer node-key")
    );
    assert_eq!(
        credential.header_value_for("http://control.internal:443/bundle"),
        None,
        "the scheme is part of the origin: the key must not be sent in the clear"
    );
}

#[test]
fn userinfo_cannot_disguise_a_foreign_host() {
    let credential = credential();
    assert_eq!(
        credential.header_value_for("http://control.internal:8080@evil.example.com/bundle"),
        None
    );
}

#[test]
fn the_secret_never_reaches_a_debug_line() {
    let rendered = format!("{:?}", credential());
    assert!(!rendered.contains("node-key"), "rendered as {rendered}");
    assert!(rendered.contains("<redacted>"));
}
