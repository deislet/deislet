//! Which URLs count as the same origin, and which do not.
//!
//! These used to be about a credential: the node key was sent to the control
//! plane the operator named and to nothing else, and `header_value_for`
//! decided that by comparing origins. The key is gone, but the comparison is
//! not - `super::origin_of` is what `deployment::fetch` uses to decide whether
//! a bundle URL is on this node's machine listener, which decides whether the
//! download goes over mutual TLS or over plain HTTP to somewhere else.
//!
//! So the cases below are the same cases, asked of the function that survived.
//! The last two matter most: a URL that differs only by scheme, or one that
//! hides a foreign host behind userinfo, must not be mistaken for the address
//! this node was configured with.

use super::origin_of;

/// The origin of the control plane these tests compare against.
fn control() -> Option<String> {
    origin_of("http://control.internal:8080")
}

#[test]
fn the_configured_control_plane_is_its_own_origin() {
    assert_eq!(
        origin_of("http://control.internal:8080/api/apps/a/versions/v/bundle"),
        control()
    );
}

#[test]
fn a_bundle_url_on_another_host_is_another_origin() {
    assert_ne!(
        origin_of("http://cdn.example.com:8080/bundles/v.zip"),
        control(),
        "a host the operator never named is not the one they named"
    );
}

#[test]
fn another_port_on_the_same_host_is_another_origin() {
    assert_ne!(origin_of("http://control.internal:9090/bundle"), control());
}

#[test]
fn the_default_port_and_the_written_port_are_one_origin() {
    let implied = origin_of("https://control.internal");
    assert_eq!(origin_of("https://control.internal:443/bundle"), implied);
    assert_ne!(
        origin_of("http://control.internal:443/bundle"),
        implied,
        "the scheme is part of the origin"
    );
}

#[test]
fn userinfo_cannot_disguise_a_foreign_host() {
    assert_ne!(
        origin_of("http://control.internal:8080@evil.example.com/bundle"),
        control(),
        "the host is what follows the last @, and this one is not the control plane"
    );
}
