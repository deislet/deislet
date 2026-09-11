//! Naming a node key without writing it down.
//!
//! `node_preshared_keys` is a list of equivalent values, and during a
//! rotation it holds two of them at once. The control plane has to remember
//! *which* of the two an address proved when it registered, because that is
//! the value the address is entitled to be driven with afterwards - handing
//! it the other one would give whoever holds the retiring value the one that
//! replaces it. What is remembered must not be the value itself: the fleet
//! list is served by the API, printed by `deis status` and written to logs,
//! and none of those may carry a credential.
//!
//! So a key is named by its SHA-256, and only a prefix of that ever leaves
//! the process. The prefix is enough for an operator to tell "this node is on
//! the new key" from "this one is still on the old one" by comparing against
//! `sha256sum` of the values in their own file; it is not enough to recover a
//! key of the shape the documentation asks for (`openssl rand -hex 32`).

use sha2::{Digest, Sha256};

/// How many hex characters of the digest are shown in logs and API answers.
const SHOWN_HEX: usize = 12;

/// The identity of one configured node key: its SHA-256, kept whole for
/// comparison and shown only as a prefix.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct NodeKeyFingerprint([u8; 32]);

impl NodeKeyFingerprint {
    /// Fingerprint of one key value, exactly as it is compared: the
    /// configured value trimmed, because that is what
    /// [`crate::api::middleware::NodeAuthConfig`] compares against.
    pub fn of(key: &str) -> Self {
        Self(Sha256::digest(key.trim().as_bytes()).into())
    }

    /// The prefix an operator sees. The same string `sha256sum` prints,
    /// truncated, so a value in a configuration file can be matched against
    /// it by hand.
    pub fn short(&self) -> String {
        hex::encode(self.0)[..SHOWN_HEX].to_string()
    }
}

impl std::fmt::Display for NodeKeyFingerprint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.short())
    }
}

/// Redacted to the prefix on purpose, like `Display`: a fingerprint ends up
/// in error contexts and fleet snapshots, and the full digest of a weak key
/// is more than an operator needs to see.
impl std::fmt::Debug for NodeKeyFingerprint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "NodeKeyFingerprint({})", self.short())
    }
}

impl serde::Serialize for NodeKeyFingerprint {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.short())
    }
}

/// The key a node-endpoint request proved, put on the request by
/// [`crate::api::middleware::node_auth`] so the handler behind it can record
/// which credential the caller actually held. A handler that needs this and
/// does not find it was mounted without the layer, which the routers here
/// make impossible by construction.
#[derive(Debug, Clone)]
pub struct ProvedNodeKey(pub NodeKeyFingerprint);

/// What an address proved about itself when it last registered.
///
/// Two things can be proved, and they say different amounts. A fleet key says
/// "whoever is here holds a secret every machine in this installation holds",
/// which identifies the installation and not the machine. A certificate says
/// "whoever is here holds the private key for this one name", which is the
/// thing a shared secret could never say.
///
/// Both are recorded because both can happen while the fleet is moving from
/// one to the other, and because what the control plane does next differs: a
/// key has to be checked against the list it came from, and can be retired out
/// from under a node; a name cannot be retired by editing a list, and is
/// either still signed by this fleet's authority at the next handshake or is
/// not.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NodeProof {
    /// The fleet key this address presented, named by fingerprint.
    PresharedKey(NodeKeyFingerprint),
    /// The name this address proved with a certificate on the machine channel.
    Certificate(deis_identity::SpiffeId),
}

impl NodeProof {
    /// The fingerprint, for the rules that are about a key and only a key.
    ///
    /// `None` for a certificate, which is not a weaker answer but a different
    /// one: there is no key to compare against a rotating list, because
    /// nothing was proved by holding a shared value.
    pub fn fingerprint(&self) -> Option<&NodeKeyFingerprint> {
        match self {
            Self::PresharedKey(fingerprint) => Some(fingerprint),
            Self::Certificate(_) => None,
        }
    }
}

impl std::fmt::Display for NodeProof {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::PresharedKey(fingerprint) => write!(f, "the fleet key {fingerprint}"),
            Self::Certificate(name) => write!(f, "a certificate naming {name}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fingerprint_names_the_key_without_containing_it() {
        let fingerprint = NodeKeyFingerprint::of("node-psk");
        for rendered in [
            fingerprint.to_string(),
            format!("{fingerprint:?}"),
            serde_json::to_string(&fingerprint).unwrap(),
        ] {
            assert!(!rendered.contains("node-psk"), "{rendered}");
            assert!(rendered.contains(&fingerprint.short()), "{rendered}");
        }
        assert_eq!(fingerprint.short().len(), SHOWN_HEX);
        // The whole digest is what is compared, so two keys sharing a prefix
        // by accident would still be told apart.
        assert_eq!(fingerprint, NodeKeyFingerprint::of("node-psk"));
        assert_ne!(fingerprint, NodeKeyFingerprint::of("node-psk2"));
    }

    /// The middleware trims what it compares, so the fingerprint has to be of
    /// the trimmed value or a key written with a stray space in the file
    /// would be proved under one name and looked up under another.
    #[test]
    fn a_fingerprint_is_of_the_trimmed_value() {
        assert_eq!(
            NodeKeyFingerprint::of(" node-psk "),
            NodeKeyFingerprint::of("node-psk")
        );
    }
}
