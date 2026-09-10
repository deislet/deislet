//! How this fleet spells its own machines.
//!
//! [`SpiffeId`] knows what a well-formed name looks like; it does not know
//! that a Deislet control plane is called `/deislet/control`. That is a
//! convention, and a convention with no single definition is a convention two
//! processes disagree about: a node that proves
//! `spiffe://fleet/deislet/node/07` while the control plane expects
//! `spiffe://fleet/node/07` is a fleet that authenticates nothing, and the
//! only symptom is a handshake refused for a reason neither log spells out.
//!
//! # Is this the crate's business?
//!
//! The module doc next door says this crate answers "who is calling" and
//! never "what may this caller do", and this module is neither of those - it
//! is how a name is written. It lives here anyway because the alternative is
//! worse: the paths are built out of a [`SpiffeId`], so putting them in
//! `deis-wire` beside the other shared spellings would point that crate's
//! dependency arrow at this one, and pull a TLS stack and an X.509 parser
//! into every crate that only wanted a header name.
//!
//! # The shape
//!
//! ```text
//! spiffe://<trust domain>/deislet/control
//! spiffe://<trust domain>/deislet/node/<node id>
//! spiffe://<trust domain>/deislet/proxy/<proxy id>
//! ```
//!
//! One control plane per trust domain, so it has no id of its own: a second
//! one would be a second authority over the same fleet, which is a topology
//! this platform does not have. Nodes and proxies carry the id they are known
//! by everywhere else - the one in an API path, in `deis status` and in a log
//! line - so that "which machine is this certificate for" needs no lookup
//! table to answer.
//!
//! # What the ids may hold
//!
//! A node id is already limited to letters, digits, `.`, `-` and `_` by the
//! control plane, which is exactly what a SPIFFE path segment admits, so
//! every id this platform accepts is spellable here. The two exceptions are
//! `.` and `..`, which are valid node ids and are not valid path segments;
//! they are refused rather than escaped, for the reason
//! [`crate::spiffe`] refuses rather than normalises.

use crate::spiffe::{SpiffeId, SpiffeIdError, check_path_segment};

/// The first segment of every name this fleet issues.
///
/// Present so that a trust domain can be shared with something that is not
/// Deislet without the two colliding.
const PREFIX: &str = "deislet";

const CONTROL: &str = "control";
const NODE: &str = "node";
const PROXY: &str = "proxy";

/// What kind of process a name belongs to.
///
/// Read off the name and never taken from the caller's own word for it: a
/// peer that has proved `spiffe://fleet/deislet/node/07` is a node, whatever
/// the request says about itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServiceKind {
    Control,
    Node,
    Proxy,
}

/// The control plane of `trust_domain`.
pub fn control(trust_domain: &str) -> Result<SpiffeId, SpiffeIdError> {
    SpiffeId::parse(&format!("spiffe://{trust_domain}/{PREFIX}/{CONTROL}"))
}

/// The runtime node known as `node_id`.
///
/// The id is checked as one path segment before it is put in, so an id with a
/// `/` in it is a refusal rather than a name one segment longer than anybody
/// meant.
pub fn node(trust_domain: &str, node_id: &str) -> Result<SpiffeId, SpiffeIdError> {
    check_path_segment(node_id)?;
    SpiffeId::parse(&format!(
        "spiffe://{trust_domain}/{PREFIX}/{NODE}/{node_id}"
    ))
}

/// The proxy known as `proxy_id`.
///
/// The id is checked as one path segment, as in [`node`].
pub fn proxy(trust_domain: &str, proxy_id: &str) -> Result<SpiffeId, SpiffeIdError> {
    check_path_segment(proxy_id)?;
    SpiffeId::parse(&format!(
        "spiffe://{trust_domain}/{PREFIX}/{PROXY}/{proxy_id}"
    ))
}

/// What `id` names, or `None` if it is not a name this fleet issues.
///
/// A name from our own trust domain that does not fit any of the three shapes
/// is `None` and not a guess. A certificate our root signed into a shape
/// nobody serves is a mistake somewhere, and the safe reading of a mistake is
/// "I do not know what this is".
pub fn service_kind(id: &SpiffeId) -> Option<ServiceKind> {
    match segments(id)?.as_slice() {
        [CONTROL] => Some(ServiceKind::Control),
        [NODE, _] => Some(ServiceKind::Node),
        [PROXY, _] => Some(ServiceKind::Proxy),
        _ => None,
    }
}

/// The node id inside a node's name, or `None` if it is not one.
pub fn node_id_of(id: &SpiffeId) -> Option<&str> {
    match segments(id)?.as_slice() {
        [NODE, node_id] => Some(node_id),
        _ => None,
    }
}

/// The proxy id inside a proxy's name, or `None` if it is not one.
pub fn proxy_id_of(id: &SpiffeId) -> Option<&str> {
    match segments(id)?.as_slice() {
        [PROXY, proxy_id] => Some(proxy_id),
        _ => None,
    }
}

/// The path segments after `/deislet`, or `None` if the name does not begin
/// there.
fn segments(id: &SpiffeId) -> Option<Vec<&str>> {
    let rest = id.path().strip_prefix(&format!("/{PREFIX}/"))?;
    Some(rest.split('/').collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_kind_is_spelled_one_way_and_read_back() {
        let control = control("deis.internal").expect("a control name");
        assert_eq!(control.as_str(), "spiffe://deis.internal/deislet/control");
        assert_eq!(service_kind(&control), Some(ServiceKind::Control));
        assert_eq!(node_id_of(&control), None);

        let node = node("deis.internal", "node-07").expect("a node name");
        assert_eq!(node.as_str(), "spiffe://deis.internal/deislet/node/node-07");
        assert_eq!(service_kind(&node), Some(ServiceKind::Node));
        assert_eq!(node_id_of(&node), Some("node-07"));
        assert_eq!(proxy_id_of(&node), None);

        let proxy = proxy("deis.internal", "edge.1").expect("a proxy name");
        assert_eq!(
            proxy.as_str(),
            "spiffe://deis.internal/deislet/proxy/edge.1"
        );
        assert_eq!(service_kind(&proxy), Some(ServiceKind::Proxy));
        assert_eq!(proxy_id_of(&proxy), Some("edge.1"));
        assert_eq!(node_id_of(&proxy), None);
    }

    /// The check the whole migration turns on: a machine that holds a real
    /// fleet certificate is still only the machine its own name says it is.
    #[test]
    fn one_node_cannot_be_read_as_another() {
        let seven = node("deis.internal", "node-07").expect("a node name");
        let eight = node("deis.internal", "node-08").expect("a node name");
        assert_ne!(seven, eight);
        assert_ne!(node_id_of(&seven), node_id_of(&eight));
    }

    /// Every id the control plane accepts is spellable, because its character
    /// set is the path segment's character set. These are the awkward ones.
    #[test]
    fn the_ids_this_platform_accepts_are_spellable() {
        for node_id in ["a", "node_07", "node-07", "node.07", &"n".repeat(128)] {
            let name = node("deis.internal", node_id).expect(node_id);
            assert_eq!(node_id_of(&name), Some(node_id));
        }
    }

    /// `.` and `..` pass the control plane's own id check and are not path
    /// segments. Refused here rather than escaped into something that would
    /// then be a second spelling of another name.
    #[test]
    fn the_two_ids_that_are_not_path_segments_are_refused() {
        for node_id in [".", ".."] {
            assert!(
                node("deis.internal", node_id).is_err(),
                "{node_id} was accepted as a path segment"
            );
        }
    }

    /// An id with a slash in it would otherwise reach into the namespace: a
    /// node calling itself `07/../control` must not be able to spell the
    /// control plane's name.
    #[test]
    fn an_id_cannot_reach_out_of_its_own_segment() {
        assert!(node("deis.internal", "07/../control").is_err());
        assert!(node("deis.internal", "07/08").is_err());
        assert!(proxy("deis.internal", "../node/07").is_err());
    }

    #[test]
    fn a_name_this_fleet_does_not_issue_is_not_guessed_at() {
        for text in [
            "spiffe://deis.internal/deislet/store/01",
            "spiffe://deis.internal/node/07",
            "spiffe://deis.internal/deislet",
            "spiffe://deis.internal/deislet/control/extra",
            "spiffe://deis.internal/deislet/node",
        ] {
            let id = SpiffeId::parse(text).expect(text);
            assert_eq!(service_kind(&id), None, "{text}");
            assert_eq!(node_id_of(&id), None, "{text}");
        }
    }
}
