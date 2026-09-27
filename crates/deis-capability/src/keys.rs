//! Key identifiers, the verifier's key ring, and the JWKS document that
//! carries the control plane's public keys to every verifier.
//!
//! # Key identifiers
//!
//! Every key is named by its RFC 7638 thumbprint: the SHA-256 of the
//! canonical JWK members (`crv`, `kty`, `x`), base64url without padding.
//! The name is derived from the key itself, so a verifier never has to trust
//! the `kid` a JWKS entry claims - it recomputes it - and two keys can never
//! share one.
//!
//! # Rotation
//!
//! A [`KeyRing`] holds every key the control plane currently publishes: the
//! one it signs with, the next one (published ahead of use so verifiers learn
//! it before the first token signed with it arrives), and the previous one
//! (kept until every token it signed has expired). A token names its signing
//! key in its `kid` claim and the ring picks the key by that name.

use std::collections::BTreeMap;
use std::sync::{Arc, RwLock};

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use ed25519_dalek::VerifyingKey;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::CapabilityError;

/// JWK `kty` for Edwards-curve keys (RFC 8037).
const KTY_OKP: &str = "OKP";
/// JWK `crv` for Ed25519 (RFC 8037).
const CRV_ED25519: &str = "Ed25519";
/// JWS `alg` for Ed25519 signatures (RFC 8037).
const ALG_EDDSA: &str = "EdDSA";

/// The RFC 7638 thumbprint of an Ed25519 public key, used as its `kid`.
pub fn key_id(key: &VerifyingKey) -> String {
    let x = URL_SAFE_NO_PAD.encode(key.as_bytes());
    // RFC 7638 §3.2: the required members only, lexicographic order, no
    // whitespace. Written out rather than serialized so the bytes cannot
    // drift with a serializer's formatting choices.
    let canonical = format!(r#"{{"crv":"{CRV_ED25519}","kty":"{KTY_OKP}","x":"{x}"}}"#);
    URL_SAFE_NO_PAD.encode(Sha256::digest(canonical.as_bytes()))
}

/// Called with the unknown `kid` whenever a token names a key the ring does
/// not hold, so a refresher can fetch the published set again.
type UnknownKidHook = Arc<dyn Fn(&str) + Send + Sync>;

/// The set of public keys a verifier accepts, shared between the verifier and
/// whatever keeps it current.
///
/// Cheap to clone: every clone sees the same keys, so a background refresher
/// can replace them under a verifier that is serving requests.
#[derive(Clone, Default)]
pub struct KeyRing {
    inner: Arc<RingInner>,
}

#[derive(Default)]
struct RingInner {
    keys: RwLock<BTreeMap<String, VerifyingKey>>,
    on_unknown_kid: RwLock<Option<UnknownKidHook>>,
}

impl KeyRing {
    /// An empty ring. Every token is refused with
    /// [`CapabilityError::UnknownKey`] until keys are installed.
    pub fn new() -> Self {
        Self::default()
    }

    /// A ring holding exactly `keys`.
    pub fn from_keys(keys: impl IntoIterator<Item = VerifyingKey>) -> Self {
        let ring = Self::new();
        ring.replace(keys);
        ring
    }

    /// Replace the whole set. Keys absent from `keys` stop verifying at once,
    /// which is how a retired key is dropped.
    pub fn replace(&self, keys: impl IntoIterator<Item = VerifyingKey>) {
        let keys = keys.into_iter().map(|key| (key_id(&key), key)).collect();
        *self.inner.keys.write().expect("key ring lock poisoned") = keys;
    }

    /// The key named `kid`, if the ring holds it.
    pub fn get(&self, kid: &str) -> Option<VerifyingKey> {
        self.inner
            .keys
            .read()
            .expect("key ring lock poisoned")
            .get(kid)
            .copied()
    }

    /// The names of every key in the ring, sorted.
    pub fn kids(&self) -> Vec<String> {
        self.inner
            .keys
            .read()
            .expect("key ring lock poisoned")
            .keys()
            .cloned()
            .collect()
    }

    /// Whether the ring holds no key at all.
    pub fn is_empty(&self) -> bool {
        self.inner
            .keys
            .read()
            .expect("key ring lock poisoned")
            .is_empty()
    }

    /// Install the callback run when a token names a key this ring does not
    /// hold. Replaces any previous one.
    pub fn on_unknown_kid(&self, hook: impl Fn(&str) + Send + Sync + 'static) {
        *self
            .inner
            .on_unknown_kid
            .write()
            .expect("key ring lock poisoned") = Some(Arc::new(hook));
    }

    /// The key named `kid`, or [`CapabilityError::UnknownKey`] after telling
    /// the unknown-kid hook about the miss.
    pub(crate) fn lookup(&self, kid: &str) -> Result<VerifyingKey, CapabilityError> {
        if let Some(key) = self.get(kid) {
            return Ok(key);
        }
        let hook = self
            .inner
            .on_unknown_kid
            .read()
            .expect("key ring lock poisoned")
            .clone();
        if let Some(hook) = hook {
            hook(kid);
        }
        Err(CapabilityError::UnknownKey {
            kid: kid.to_string(),
        })
    }
}

/// Only the key names: the keys are public, but a list of 64-character
/// strings in a log line helps nobody.
impl std::fmt::Debug for KeyRing {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KeyRing").field("kids", &self.kids()).finish()
    }
}

/// One public key in a JWKS document (RFC 7517, RFC 8037).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Jwk {
    pub kty: String,
    #[serde(default)]
    pub crv: String,
    #[serde(default)]
    pub x: String,
    #[serde(default)]
    pub kid: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub alg: Option<String>,
    #[serde(rename = "use", default, skip_serializing_if = "Option::is_none")]
    pub use_: Option<String>,
}

impl Jwk {
    /// The JWK for an Ed25519 signature-verification key.
    pub fn ed25519(key: &VerifyingKey) -> Self {
        Self {
            kty: KTY_OKP.to_string(),
            crv: CRV_ED25519.to_string(),
            x: URL_SAFE_NO_PAD.encode(key.as_bytes()),
            kid: key_id(key),
            alg: Some(ALG_EDDSA.to_string()),
            use_: Some("sig".to_string()),
        }
    }
}

/// A JWK Set: every key the control plane currently publishes.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Jwks {
    pub keys: Vec<Jwk>,
}

/// Why a JWKS document was refused.
#[derive(Debug, thiserror::Error)]
pub enum JwksError {
    #[error("JWKS entry {kid:?} is not a usable Ed25519 key: {reason}")]
    BadKey { kid: String, reason: String },
    #[error("JWKS entry claims kid {claimed:?} but its key's thumbprint is {actual:?}")]
    KidMismatch { claimed: String, actual: String },
    #[error("JWKS holds no Ed25519 signature key")]
    Empty,
}

impl Jwks {
    /// The document publishing `keys`.
    pub fn from_keys<'a>(keys: impl IntoIterator<Item = &'a VerifyingKey>) -> Self {
        Self {
            keys: keys.into_iter().map(Jwk::ed25519).collect(),
        }
    }

    /// Every Ed25519 signature key in the document.
    ///
    /// Entries of another key type or intended for encryption are skipped,
    /// so the document can grow other members without breaking verifiers. An
    /// Ed25519 entry that does not decode, or whose `kid` is not its own
    /// thumbprint, refuses the whole document: that is a publisher bug or a
    /// tampered response, and half a key set is not something to verify
    /// with.
    pub fn verifying_keys(&self) -> Result<Vec<VerifyingKey>, JwksError> {
        let mut keys = Vec::new();
        for jwk in &self.keys {
            if jwk.kty != KTY_OKP || jwk.crv != CRV_ED25519 {
                continue;
            }
            if jwk.use_.as_deref().is_some_and(|u| u != "sig") {
                continue;
            }
            let bad = |reason: String| JwksError::BadKey {
                kid: jwk.kid.clone(),
                reason,
            };
            let bytes = URL_SAFE_NO_PAD
                .decode(&jwk.x)
                .map_err(|e| bad(format!("x is not base64url: {e}")))?;
            let bytes: [u8; 32] = bytes
                .try_into()
                .map_err(|_| bad("x is not 32 bytes".to_string()))?;
            let key = VerifyingKey::from_bytes(&bytes).map_err(|e| bad(e.to_string()))?;
            let actual = key_id(&key);
            if !jwk.kid.is_empty() && jwk.kid != actual {
                return Err(JwksError::KidMismatch {
                    claimed: jwk.kid.clone(),
                    actual,
                });
            }
            keys.push(key);
        }
        if keys.is_empty() {
            return Err(JwksError::Empty);
        }
        Ok(keys)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::SigningKey;

    fn key(seed: u8) -> VerifyingKey {
        SigningKey::from_bytes(&[seed; 32]).verifying_key()
    }

    /// RFC 8037 appendix A.3 publishes the thumbprint of its example key;
    /// matching it proves the canonical form is the standard one.
    #[test]
    fn key_id_matches_the_rfc_8037_example() {
        let x = URL_SAFE_NO_PAD
            .decode("11qYAYKxCrfVS_7TyWQHOg7hcvPapiMlrwIaaPcHURo")
            .unwrap();
        let key = VerifyingKey::from_bytes(&x.try_into().unwrap()).unwrap();
        assert_eq!(key_id(&key), "kPrK_qmxVWaYVA9wwBF6Iuo3vVzz7TxHCTwXBygrS4k");
    }

    #[test]
    fn jwks_round_trips_every_key() {
        let keys = [key(1), key(2)];
        let json = serde_json::to_string(&Jwks::from_keys(&keys)).unwrap();
        let parsed: Jwks = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.verifying_keys().unwrap(), keys.to_vec());
        assert!(json.contains(&key_id(&keys[0])));
    }

    #[test]
    fn a_kid_that_is_not_the_thumbprint_refuses_the_document() {
        let mut jwks = Jwks::from_keys(&[key(1)]);
        jwks.keys[0].kid = key_id(&key(2));
        assert!(matches!(
            jwks.verifying_keys(),
            Err(JwksError::KidMismatch { .. })
        ));
    }

    #[test]
    fn foreign_key_types_are_skipped_and_an_empty_set_is_refused() {
        let jwks: Jwks =
            serde_json::from_str(r#"{"keys":[{"kty":"RSA","n":"abc","e":"AQAB"}]}"#).unwrap();
        assert!(matches!(jwks.verifying_keys(), Err(JwksError::Empty)));
    }

    #[test]
    fn replacing_the_ring_drops_keys_not_in_the_new_set() {
        let ring = KeyRing::from_keys([key(1), key(2)]);
        assert_eq!(ring.kids().len(), 2);
        ring.replace([key(2)]);
        assert!(ring.get(&key_id(&key(1))).is_none());
        assert!(ring.get(&key_id(&key(2))).is_some());
    }

    #[test]
    fn an_unknown_kid_runs_the_hook() {
        let ring = KeyRing::new();
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink = Arc::clone(&seen);
        ring.on_unknown_kid(move |kid| sink.lock().unwrap().push(kid.to_string()));
        assert!(matches!(
            ring.lookup("nope"),
            Err(CapabilityError::UnknownKey { .. })
        ));
        assert_eq!(*seen.lock().unwrap(), vec!["nope".to_string()]);
    }
}
