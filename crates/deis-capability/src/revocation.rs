//! The revocation list the control plane signs and every verifier polls.
//!
//! # What it carries
//!
//! The current `auth_epoch` (a capability below it is refused - that is mass
//! revocation), the `jti` of every revoked capability that could still be
//! accepted somewhere, and every node revoked as a principal. It is small on
//! purpose: capabilities live fifteen minutes, so a revoked `jti` drops off
//! the list shortly after the token it names would have expired anyway; and
//! a revoked node is one entry however many tokens it holds or asks for.
//!
//! A `jti` names tokens that exist; a node names a holder. Revoking only the
//! jtis a compromised node held would last until its next renewal, so a
//! revoked node is carried by name and every capability whose subject names
//! it is refused - including ones issued after the revocation, ones this
//! control plane never recorded, and expired ones offered under T8 grace -
//! until an operator lifts it.
//!
//! # Why it is signed
//!
//! A verifier that takes this list on the transport's word is one a
//! man-in-the-middle can steer both ways: roll the epoch back to undo a mass
//! revocation, or push it to `u64::MAX` and have every storage call on the
//! platform refused. So the list is signed with the same Ed25519 key the
//! control plane signs capabilities with, in the same `v1.<payload>.<sig>`
//! form, and a verifier checks it against the same key ring before believing
//! a word of it. The transport (the machine channel, mutual TLS) is a second
//! layer, not the only one.
//!
//! The payload carries `typ` = [`REVOCATION_LIST_TYPE`], which no capability
//! has, and lacks every field a capability requires, so neither can be passed
//! off as the other even though one key signs both.
//!
//! # Replay
//!
//! A signed list stays validly signed forever, so a signature alone would let
//! an attacker keep replaying an old one - hiding every revocation made since
//! and keeping the verifier looking "in sync" while it is not. The verifier
//! therefore refuses a list that is older than [`MAX_LIST_AGE_SECS`], issued
//! in the future beyond the clock skew, or older than the last one it
//! accepted; and it never lowers its epoch floor, whatever a list says.

use serde::{Deserialize, Serialize};

use crate::keys::{KeyRing, key_id};
use crate::{CapabilityError, TOKEN_SEPARATOR, VERSION_PREFIX, base64url, now_secs, parse_token};

/// The `typ` every revocation list carries and no capability does.
pub const REVOCATION_LIST_TYPE: &str = "deis-revocation-list";

/// Where the control plane serves the signed list, on its machine listener.
pub const REVOCATION_LIST_PATH: &str = "/capability/revocation-list";

/// The oldest a list may be, by its own `issued_at`, and still be accepted.
///
/// The control plane signs a fresh list for every request, so a list older
/// than a couple of poll intervals is not one this verifier just asked for.
pub const MAX_LIST_AGE_SECS: u64 = 120;

/// How far a list's `issued_at` may be ahead of the verifier's clock.
const FUTURE_SKEW_SECS: u64 = 30;

/// The claims of a revocation list.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RevocationList {
    /// Always [`REVOCATION_LIST_TYPE`].
    pub typ: String,
    /// The signing key's thumbprint; set by [`sign`].
    pub kid: String,
    /// The control plane that signed it (same value as a capability's `iss`).
    pub iss: String,
    /// When the control plane signed this list, Unix seconds.
    pub issued_at: u64,
    /// The minimum `auth_epoch` a capability must carry.
    pub auth_epoch: u64,
    /// Revoked capabilities that could still be accepted somewhere.
    pub revoked_jtis: Vec<String>,
    /// Node ids revoked as principals: a capability whose subject is one of
    /// these nodes' SPIFFE names is refused, whatever its `jti`.
    pub revoked_nodes: Vec<String>,
}

impl RevocationList {
    /// A list signed now.
    pub fn new(iss: impl Into<String>, auth_epoch: u64, revoked_jtis: Vec<String>) -> Self {
        Self {
            typ: REVOCATION_LIST_TYPE.to_string(),
            kid: String::new(),
            iss: iss.into(),
            issued_at: now_secs(),
            auth_epoch,
            revoked_jtis,
            revoked_nodes: Vec::new(),
        }
    }

    /// The same list, also revoking every node in `nodes` as a principal.
    pub fn with_revoked_nodes(self, nodes: Vec<String>) -> Self {
        Self {
            revoked_nodes: nodes,
            ..self
        }
    }
}

/// The body the control plane answers [`REVOCATION_LIST_PATH`] with.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SignedRevocationList {
    /// `v1.<base64url(JCS claims)>.<base64url(Ed25519 signature)>`.
    pub signed_list: String,
}

/// Why a fetched list was not believed.
#[derive(Debug, thiserror::Error)]
pub enum RevocationListError {
    /// Unparseable, unsigned by any held key, or badly signed.
    #[error(transparent)]
    Signature(#[from] CapabilityError),
    /// Signed, but not a revocation list.
    #[error("not a revocation list: {0}")]
    NotAList(String),
    /// Signed and well-formed, but too old, from the future, or older than
    /// the last list accepted: a replay, or a clock badly off.
    #[error("stale revocation list: {0}")]
    Stale(String),
}

/// Sign `list` with `signing_key`, naming the key in `kid`.
pub fn sign(list: &RevocationList, signing_key: &ed25519_dalek::SigningKey) -> String {
    use ed25519_dalek::Signer;
    let list = RevocationList {
        kid: key_id(&signing_key.verifying_key()),
        typ: REVOCATION_LIST_TYPE.to_string(),
        ..list.clone()
    };
    let payload = serde_jcs::to_vec(&list).expect("JCS serialization of a revocation list");
    let signature = signing_key.sign(&payload);
    format!(
        "{VERSION_PREFIX}{}{TOKEN_SEPARATOR}{}",
        base64url(&payload),
        base64url(signature.to_bytes().as_ref())
    )
}

/// Check the signature and shape of a signed list against `keys`.
///
/// Freshness is not checked here - it depends on what the verifier accepted
/// before; see [`crate::CapabilityVerifier::apply_revocation_list`].
pub fn verify(signed: &str, keys: &KeyRing) -> Result<RevocationList, RevocationListError> {
    let (payload, signature) = parse_token(signed)?;

    #[derive(Deserialize)]
    struct KeyHint {
        #[serde(default)]
        kid: String,
    }
    let hint: KeyHint = serde_json::from_slice(&payload)
        .map_err(|e| RevocationListError::NotAList(format!("payload is not JSON: {e}")))?;
    let key = keys.lookup(&hint.kid)?;
    let signature = ed25519_dalek::Signature::from_slice(&signature)
        .map_err(|e| CapabilityError::ParseError(format!("invalid signature bytes: {e}")))?;
    key.verify_strict(&payload, &signature)
        .map_err(|e| CapabilityError::BadSignature(e.to_string()))?;

    let list: RevocationList = serde_json::from_slice(&payload)
        .map_err(|e| RevocationListError::NotAList(e.to_string()))?;
    if list.typ != REVOCATION_LIST_TYPE {
        return Err(RevocationListError::NotAList(format!(
            "typ is {:?}, not {REVOCATION_LIST_TYPE:?}",
            list.typ
        )));
    }
    Ok(list)
}

/// Whether a verified list is fresh enough to believe at `now`, given the
/// `issued_at` of the last list accepted (`0` if none).
pub(crate) fn check_fresh(
    list: &RevocationList,
    now: u64,
    last_issued_at: u64,
) -> Result<(), RevocationListError> {
    if list.issued_at > now.saturating_add(FUTURE_SKEW_SECS) {
        return Err(RevocationListError::Stale(format!(
            "issued at {} which is ahead of this clock ({now}) by more than {FUTURE_SKEW_SECS}s",
            list.issued_at
        )));
    }
    if list.issued_at.saturating_add(MAX_LIST_AGE_SECS) < now {
        return Err(RevocationListError::Stale(format!(
            "issued at {}, more than {MAX_LIST_AGE_SECS}s before now ({now})",
            list.issued_at
        )));
    }
    if list.issued_at < last_issued_at {
        return Err(RevocationListError::Stale(format!(
            "issued at {}, before the list already accepted (issued at {last_issued_at})",
            list.issued_at
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::SigningKey;

    fn key() -> SigningKey {
        SigningKey::from_bytes(&[7u8; 32])
    }

    #[test]
    fn a_signed_list_verifies_against_the_issuers_key() {
        let signing = key();
        let list = RevocationList::new("iss", 3, vec!["a".into(), "b".into()])
            .with_revoked_nodes(vec!["runtime-a".into()]);
        let signed = sign(&list, &signing);
        let verified = verify(&signed, &KeyRing::from_keys([signing.verifying_key()])).unwrap();
        assert_eq!(verified.auth_epoch, 3);
        assert_eq!(verified.revoked_jtis, vec!["a", "b"]);
        assert_eq!(verified.revoked_nodes, vec!["runtime-a"]);
        assert_eq!(verified.kid, key_id(&signing.verifying_key()));
    }

    /// A list edited on the way - the epoch rolled back, a jti dropped - no
    /// longer verifies, which is the whole reason it is signed.
    #[test]
    fn a_tampered_list_is_refused() {
        let signing = key();
        let keys = KeyRing::from_keys([signing.verifying_key()]);
        let signed = sign(
            &RevocationList::new("iss", 5, vec!["gone".into()]),
            &signing,
        );

        let (payload, signature) = parse_token(&signed).unwrap();
        let mut claims: serde_json::Value = serde_json::from_slice(&payload).unwrap();
        claims["auth_epoch"] = 0.into();
        claims["revoked_jtis"] = serde_json::json!([]);
        claims["revoked_nodes"] = serde_json::json!([]);
        let edited = format!(
            "{VERSION_PREFIX}{}{TOKEN_SEPARATOR}{}",
            base64url(&serde_jcs::to_vec(&claims).unwrap()),
            base64url(&signature)
        );
        assert!(matches!(
            verify(&edited, &keys),
            Err(RevocationListError::Signature(
                CapabilityError::BadSignature(_)
            ))
        ));
    }

    #[test]
    fn a_list_signed_by_a_stranger_is_refused() {
        let stranger = SigningKey::from_bytes(&[9u8; 32]);
        let signed = sign(&RevocationList::new("iss", 1, vec![]), &stranger);
        assert!(matches!(
            verify(&signed, &KeyRing::from_keys([key().verifying_key()])),
            Err(RevocationListError::Signature(
                CapabilityError::UnknownKey { .. }
            ))
        ));
    }

    /// One key signs both, so each must be refused as the other.
    #[test]
    fn a_capability_is_not_a_revocation_list() {
        let signing = key();
        let claims = crate::CapabilityClaims::new(
            "store",
            "iss",
            "sub",
            1,
            std::time::Duration::from_secs(60),
            vec![],
        );
        let token = crate::encode(&claims, &signing);
        assert!(matches!(
            verify(&token, &KeyRing::from_keys([signing.verifying_key()])),
            Err(RevocationListError::NotAList(_))
        ));

        let list = sign(&RevocationList::new("iss", 1, vec![]), &signing);
        assert!(matches!(
            crate::decode(&list, &signing.verifying_key()),
            Err(CapabilityError::InvalidClaims(_))
        ));
    }

    #[test]
    fn freshness_refuses_old_future_and_backwards_lists() {
        let mut list = RevocationList::new("iss", 1, vec![]);
        let now = list.issued_at;
        assert!(check_fresh(&list, now, 0).is_ok());
        assert!(check_fresh(&list, now + MAX_LIST_AGE_SECS + 1, 0).is_err());
        assert!(check_fresh(&list, now, now + 1).is_err());
        list.issued_at = now + FUTURE_SKEW_SECS + 1;
        assert!(check_fresh(&list, now, 0).is_err());
    }
}
