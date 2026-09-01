use super::*;
use deis_wire::conventions::node_auth_value;

fn addr(text: &str) -> SocketAddr {
    text.parse().unwrap()
}

fn call(header: Option<(&str, &str)>) -> HeaderMap {
    let mut headers = HeaderMap::new();
    if let Some((name, value)) = header {
        headers.insert(
            axum::http::HeaderName::from_bytes(name.as_bytes()).unwrap(),
            value.parse().unwrap(),
        );
    }
    headers
}

const RPC: &str = "/deis.management.RuntimeManagement/GetStatus";

#[test]
fn a_routable_management_plane_with_no_key_serves_nothing() {
    // The defect this module exists for: the node's management plane used to
    // be dispatched on the node's own `0.0.0.0` data port with no check at
    // all, so anything that could reach an application could also unload it.
    // With no key there is nothing a caller could present, so the plane is
    // shut rather than opened - the same answer deis-control's node endpoints
    // give when `node_preshared_keys` is empty.
    for text in ["0.0.0.0:8086", "192.168.1.10:8086", "[::]:8086"] {
        let auth = ManagementAuth::for_listener(Vec::new(), addr(text));
        assert!(auth.is_shut(), "{text} must not be served unauthenticated");
        assert!(!auth.is_enforcing());

        for header in [None, Some((NODE_AUTH_METADATA, "Bearer anything"))] {
            let status = auth.check(RPC, &call(header)).unwrap_err();
            assert_eq!(
                status.code(),
                tonic::Code::Unavailable,
                "{text} must refuse every caller"
            );
            assert!(
                status.message().contains("[control] secret"),
                "the refusal must name the missing setting: {}",
                status.message()
            );
        }
    }
}

#[test]
fn a_routable_management_plane_with_a_key_serves() {
    let auth = ManagementAuth::for_listener(vec!["node-psk".to_string()], addr("0.0.0.0:8086"));
    assert!(auth.is_enforcing());
    assert!(!auth.is_shut());
}

#[test]
fn loopback_with_no_key_keeps_working() {
    // The single-box install, where the port cannot be reached from another
    // host. It stays unauthenticated on purpose; the startup warning is what
    // makes that visible.
    for text in ["127.0.0.1:8186", "[::1]:8186"] {
        let auth = ManagementAuth::for_listener(Vec::new(), addr(text));
        assert!(!auth.is_enforcing());
        assert!(!auth.is_shut());
        assert!(auth.check(RPC, &call(None)).is_ok());
    }
}

#[test]
fn a_blank_key_is_not_a_key() {
    // A secret left as an empty string in a file is an operator who meant to
    // fill it in, not one who chose to run without authentication.
    let blanks = vec!["".to_string(), "   ".to_string()];
    assert!(ManagementAuth::for_listener(blanks.clone(), addr("0.0.0.0:8086")).is_shut());
    let auth = ManagementAuth::for_listener(blanks, addr("127.0.0.1:8186"));
    assert!(!auth.is_enforcing());
    assert!(!auth.is_shut());
}

#[test]
fn a_call_with_no_credential_is_refused_once_a_key_exists() {
    // Both attackers reached GetStatus, UnloadApp and StartInspect with
    // exactly this: a gRPC call carrying nothing at all.
    let auth = ManagementAuth::with_keys(["node-psk"]);
    let status = auth.check(RPC, &call(None)).unwrap_err();
    assert_eq!(status.code(), tonic::Code::Unauthenticated);
    assert!(
        status.message().contains(NODE_AUTH_METADATA),
        "the refusal must name the header the caller is missing: {}",
        status.message()
    );
}

#[test]
fn the_control_planes_own_credential_is_accepted() {
    let auth = ManagementAuth::with_keys(["node-psk"]);
    assert!(auth
        .check(
            RPC,
            &call(Some((NODE_AUTH_METADATA, &node_auth_value("node-psk"))))
        )
        .is_ok());
}

#[test]
fn a_bare_key_and_the_alternate_header_are_accepted_too() {
    // An operator reaching for the same key on this node and on the control
    // plane's HTTP node endpoints must not have to remember which surface
    // takes which spelling.
    let auth = ManagementAuth::with_keys(["node-psk"]);
    for header in [
        (NODE_AUTH_METADATA, "node-psk"),
        (NODE_AUTH_ALTERNATE_HEADER, "node-psk"),
        (NODE_AUTH_ALTERNATE_HEADER, "Bearer node-psk"),
    ] {
        assert!(
            auth.check(RPC, &call(Some(header))).is_ok(),
            "{header:?} should have been accepted"
        );
    }
}

#[test]
fn a_key_from_another_fleet_is_refused() {
    let auth = ManagementAuth::with_keys(["node-psk"]);
    for wrong in ["", "Bearer ", "Bearer node-psl", "Bearer node-psk-longer"] {
        let status = auth
            .check(RPC, &call(Some((NODE_AUTH_METADATA, wrong))))
            .unwrap_err();
        assert_eq!(
            status.code(),
            tonic::Code::Unauthenticated,
            "{wrong:?} should have been refused"
        );
    }
}

#[test]
fn a_node_mid_rotation_accepts_both_the_old_key_and_the_new_one() {
    // What `[management] keys` is for: a fleet cannot change a shared secret
    // on the control plane and on every node in the same instant, so there
    // has to be a window where a node answers to both.
    let auth = ManagementAuth::with_keys(["old-key", "new-key"]);
    for key in ["old-key", "new-key"] {
        assert!(
            auth.check(
                RPC,
                &call(Some((NODE_AUTH_METADATA, &node_auth_value(key))))
            )
            .is_ok(),
            "{key} should have been accepted during a rotation"
        );
    }
}

#[test]
fn the_credential_never_reaches_a_debug_line() {
    // A dispatcher formatted into a panic message or an error context must
    // not carry the fleet's node key with it.
    let auth = ManagementAuth::with_keys(["node-psk"]);
    let rendered = format!("{auth:?}");
    assert!(!rendered.contains("node-psk"), "{rendered}");
    assert!(rendered.contains("enforcing"), "{rendered}");
}
