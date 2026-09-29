//! Secret encryption for the control plane.
//!
//! Secrets are stored as AES-256-GCM ciphertext with the 12-byte nonce
//! prefixed, keyed by a 32-byte master key that lives only in the file
//! named by the `secret_key_file` config option. The key is never written
//! to the database and never logged, not even truncated: a partial key in
//! a log file is still a gift to whoever reads the logs.
//!
//! If the key file is missing or malformed the control plane keeps
//! running - node management does not depend on secrets - but every secret
//! endpoint fails loudly with 503. Storing a plaintext value instead would
//! turn a configuration mistake into a silent leak.
//!
//! The master key also roots every purpose-bound subkey the control plane
//! needs ([`SecretCipher::derive_key`]): HKDF-SHA256 with the purpose
//! string as the `info` input, so two purposes never share a key and a
//! subkey reveals nothing about the master key or its siblings.

use std::fmt;
use std::path::Path;

use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::{Aes256Gcm, Key, Nonce};
use hkdf::Hkdf;
use rand::RngCore;
use sha2::Sha256;
use tracing::{error, info, warn};

use super::{CatalogError, CatalogResult};

/// AES-256 key length in bytes.
pub const MASTER_KEY_LEN: usize = 32;
/// AES-GCM nonce length in bytes, prefixed to every ciphertext.
pub const NONCE_LEN: usize = 12;
/// Length in bytes of every subkey [`SecretCipher::derive_key`] produces.
pub const DERIVED_KEY_LEN: usize = 32;

/// Holds the master key. Deliberately opaque: no `Debug` output, no
/// accessor, no serialization.
pub struct SecretCipher {
    cipher: Aes256Gcm,
    /// The raw material the AES schedule was built from, kept so subkeys
    /// can be derived from it; never exposed outside this module.
    master_key: [u8; MASTER_KEY_LEN],
}

/// A subkey derived from the master key for one named purpose.
///
/// As opaque as the master key itself: no `Debug` output of the material,
/// no serialization. The bytes are reachable only through
/// [`DerivedKey::as_bytes`], for the primitive that consumes them.
pub struct DerivedKey {
    purpose: &'static str,
    material: [u8; DERIVED_KEY_LEN],
}

impl fmt::Debug for DerivedKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "DerivedKey({}, <material redacted>)", self.purpose)
    }
}

impl DerivedKey {
    /// The purpose string the key was derived for.
    pub fn purpose(&self) -> &'static str {
        self.purpose
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.material
    }
}

impl fmt::Debug for SecretCipher {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SecretCipher(<master key redacted>)")
    }
}

impl SecretCipher {
    /// Build a cipher from raw key material.
    pub fn from_key_bytes(key: &[u8]) -> CatalogResult<Self> {
        if key.len() != MASTER_KEY_LEN {
            return Err(CatalogError::SecretsUnavailable(format!(
                "master key must be {MASTER_KEY_LEN} bytes, got {}",
                key.len()
            )));
        }
        let mut master_key = [0u8; MASTER_KEY_LEN];
        master_key.copy_from_slice(key);
        let key = Key::<Aes256Gcm>::from_slice(key);
        Ok(Self {
            cipher: Aes256Gcm::new(key),
            master_key,
        })
    }

    /// Derive the subkey for `purpose` (HKDF-SHA256, no salt, the purpose
    /// as `info`). Deterministic: the same master key and purpose always
    /// give the same subkey, which is what lets a keyed digest computed
    /// today match one stored yesterday. `purpose` must be a non-empty,
    /// versioned, application-unique string such as
    /// `deislet-control-idempotency-digest-v1`; changing it is a key
    /// rotation for everything that used the old one.
    pub fn derive_key(&self, purpose: &'static str) -> DerivedKey {
        assert!(
            !purpose.is_empty(),
            "a derived key needs a purpose string to bind it to"
        );
        let mut material = [0u8; DERIVED_KEY_LEN];
        Hkdf::<Sha256>::new(None, &self.master_key)
            .expand(purpose.as_bytes(), &mut material)
            .expect("HKDF-SHA256 expands to 32 bytes for any input");
        DerivedKey { purpose, material }
    }

    /// Read the master key from `path`.
    ///
    /// Two encodings are accepted, in this order: exactly 32 raw bytes, or
    /// 64 hex characters with optional surrounding whitespace (what
    /// `openssl rand -hex 32` prints). Raw is checked first so a random
    /// 32-byte key that happens to end in a newline byte is not mangled.
    pub fn load_from_file(path: &Path) -> CatalogResult<Self> {
        let raw = std::fs::read(path).map_err(|e| {
            CatalogError::SecretsUnavailable(format!(
                "cannot read secret_key_file {}: {e}",
                path.display()
            ))
        })?;

        warn_if_world_readable(path);

        if raw.len() == MASTER_KEY_LEN {
            return Self::from_key_bytes(&raw);
        }

        let text = String::from_utf8(raw)
            .map_err(|_| {
                CatalogError::SecretsUnavailable(format!(
                    "secret_key_file {} is neither {MASTER_KEY_LEN} raw bytes nor hex text",
                    path.display()
                ))
            })?
            .trim()
            .to_string();

        if text.len() == MASTER_KEY_LEN * 2 {
            let decoded = hex::decode(&text).map_err(|e| {
                CatalogError::SecretsUnavailable(format!(
                    "secret_key_file {} looks like hex but does not decode: {e}",
                    path.display()
                ))
            })?;
            return Self::from_key_bytes(&decoded);
        }

        Err(CatalogError::SecretsUnavailable(format!(
            "secret_key_file {} must hold {MASTER_KEY_LEN} raw bytes or {} hex characters; generate one with: openssl rand -hex 32",
            path.display(),
            MASTER_KEY_LEN * 2
        )))
    }

    /// Encrypt one secret value. The returned blob is `nonce || ciphertext`
    /// and is what goes into the `secrets.ciphertext` column.
    ///
    /// `associated_data` binds the ciphertext to where it is stored, so a
    /// blob copied into another environment's row fails to decrypt instead
    /// of silently leaking across environments.
    pub fn encrypt(&self, plaintext: &[u8], associated_data: &[u8]) -> CatalogResult<Vec<u8>> {
        let mut nonce_bytes = [0u8; NONCE_LEN];
        rand::rngs::OsRng.fill_bytes(&mut nonce_bytes);
        let nonce = Nonce::from_slice(&nonce_bytes);

        let ciphertext = self
            .cipher
            .encrypt(
                nonce,
                Payload {
                    msg: plaintext,
                    aad: associated_data,
                },
            )
            .map_err(|_| {
                // The AEAD error type carries no detail by design; there is
                // nothing safe to add here beyond the fact that it failed.
                CatalogError::SecretsUnavailable("secret encryption failed".to_string())
            })?;

        let mut blob = Vec::with_capacity(NONCE_LEN + ciphertext.len());
        blob.extend_from_slice(&nonce_bytes);
        blob.extend_from_slice(&ciphertext);
        Ok(blob)
    }

    /// Decrypt a `nonce || ciphertext` blob produced by [`Self::encrypt`].
    pub fn decrypt(&self, blob: &[u8], associated_data: &[u8]) -> CatalogResult<Vec<u8>> {
        if blob.len() <= NONCE_LEN {
            return Err(CatalogError::SecretsUnavailable(format!(
                "stored secret is {} bytes, too short to hold a {NONCE_LEN}-byte nonce and a tag",
                blob.len()
            )));
        }
        let (nonce_bytes, ciphertext) = blob.split_at(NONCE_LEN);
        let nonce = Nonce::from_slice(nonce_bytes);
        self.cipher
            .decrypt(
                nonce,
                Payload {
                    msg: ciphertext,
                    aad: associated_data,
                },
            )
            .map_err(|_| {
                CatalogError::SecretsUnavailable(
                    "stored secret does not decrypt with the configured master key; the key was rotated or replaced".to_string(),
                )
            })
    }
}

/// Associated data for an immutable revision. Including the revision stops a
/// valid ciphertext from being substituted into another revision row of the
/// same environment and name.
pub fn secret_revision_aad(env_id: &str, name: &str, revision: &str) -> Vec<u8> {
    format!("{env_id}\u{1f}{name}\u{1f}{revision}").into_bytes()
}

/// Associated data for one immutable provider-management credential
/// revision. Organization, backend, binding and revision are all included so
/// copied ciphertext cannot be made valid in another account or generation.
pub fn provider_credential_revision_aad(
    organization_id: &str,
    backend_id: &str,
    binding_id: &str,
    revision: u64,
) -> Vec<u8> {
    format!(
        "provider-credential\u{1f}{organization_id}\u{1f}{backend_id}\u{1f}{binding_id}\u{1f}{revision}"
    )
    .into_bytes()
}

/// Whether this process can serve secret endpoints at all.
///
/// Held as a single value in the API state so a missing key produces one
/// clear 503 per request instead of an `unwrap` somewhere deeper.
pub enum SecretsBackend {
    /// Boxed because the AES key schedule is by far the largest thing in
    /// this enum, and the unavailable case is the one that gets copied
    /// around in error paths.
    Ready(Box<SecretCipher>),
    Unavailable(String),
}

impl fmt::Debug for SecretsBackend {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SecretsBackend::Ready(_) => f.write_str("SecretsBackend::Ready"),
            SecretsBackend::Unavailable(reason) => {
                write!(f, "SecretsBackend::Unavailable({reason})")
            }
        }
    }
}

impl SecretsBackend {
    /// Resolve the configured `secret_key_file` once, at startup.
    pub fn load(path: Option<&Path>) -> Self {
        let Some(path) = path else {
            let reason = "secret_key_file is not set in the control plane config".to_string();
            warn!(
                "{reason}; secret endpoints will refuse every request instead of storing plaintext"
            );
            return SecretsBackend::Unavailable(reason);
        };

        match SecretCipher::load_from_file(path) {
            Ok(cipher) => {
                info!(
                    secret_key_file = %path.display(),
                    "Master key loaded; environment secrets will be stored AES-256-GCM encrypted"
                );
                SecretsBackend::Ready(Box::new(cipher))
            }
            Err(e) => {
                let reason = e.to_string();
                error!(
                    secret_key_file = %path.display(),
                    error = %reason,
                    "Master key unusable; every secret endpoint will fail with 503 and no secret can be stored or delivered to a node until this is fixed"
                );
                SecretsBackend::Unavailable(reason)
            }
        }
    }

    pub fn cipher(&self) -> CatalogResult<&SecretCipher> {
        match self {
            SecretsBackend::Ready(cipher) => Ok(cipher),
            SecretsBackend::Unavailable(reason) => {
                Err(CatalogError::SecretsUnavailable(reason.clone()))
            }
        }
    }

    pub fn is_ready(&self) -> bool {
        matches!(self, SecretsBackend::Ready(_))
    }

    /// Derive a purpose-bound subkey from the master key, or fail with the
    /// same `SecretsUnavailable` a secret endpoint would when no usable
    /// master key is configured. See [`SecretCipher::derive_key`].
    pub fn derive_key(&self, purpose: &'static str) -> CatalogResult<DerivedKey> {
        Ok(self.cipher()?.derive_key(purpose))
    }
}

/// A master key other local users can read is only nominally a secret.
/// Worth a warning, not worth refusing to start.
#[cfg(unix)]
fn warn_if_world_readable(path: &Path) {
    use std::os::unix::fs::PermissionsExt;

    let Ok(metadata) = std::fs::metadata(path) else {
        return;
    };
    let mode = metadata.permissions().mode() & 0o777;
    if mode & 0o077 != 0 {
        warn!(
            secret_key_file = %path.display(),
            mode = format!("{mode:o}"),
            "Master key file is readable by users other than its owner; anyone with local access can decrypt every stored secret. Consider chmod 600"
        );
    }
}

#[cfg(not(unix))]
fn warn_if_world_readable(_path: &Path) {}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn test_cipher() -> SecretCipher {
        SecretCipher::from_key_bytes(&[7u8; MASTER_KEY_LEN]).unwrap()
    }

    #[test]
    fn round_trips_a_value() {
        let cipher = test_cipher();
        let aad = secret_revision_aad("env-1", "API_KEY", "rev-1");
        let blob = cipher.encrypt(b"s3cr3t", &aad).unwrap();

        assert!(blob.len() > NONCE_LEN);
        // The plaintext must not be recoverable by looking at the blob.
        assert!(!blob.windows(6).any(|w| w == b"s3cr3t"));

        let plaintext = cipher.decrypt(&blob, &aad).unwrap();
        assert_eq!(plaintext, b"s3cr3t");
    }

    #[test]
    fn provider_credential_aad_binds_every_identity_dimension() {
        let baseline = provider_credential_revision_aad("org-a", "deislet", "binding-a", 1);
        assert_ne!(
            baseline,
            provider_credential_revision_aad("org-b", "deislet", "binding-a", 1)
        );
        assert_ne!(
            baseline,
            provider_credential_revision_aad("org-a", "deno", "binding-a", 1)
        );
        assert_ne!(
            baseline,
            provider_credential_revision_aad("org-a", "deislet", "binding-b", 1)
        );
        assert_ne!(
            baseline,
            provider_credential_revision_aad("org-a", "deislet", "binding-a", 2)
        );
    }

    #[test]
    fn every_encryption_uses_a_fresh_nonce() {
        let cipher = test_cipher();
        let aad = secret_revision_aad("env-1", "API_KEY", "rev-1");
        let a = cipher.encrypt(b"same", &aad).unwrap();
        let b = cipher.encrypt(b"same", &aad).unwrap();
        assert_ne!(a[..NONCE_LEN], b[..NONCE_LEN]);
        assert_ne!(a, b);
    }

    #[test]
    fn a_blob_moved_to_another_environment_does_not_decrypt() {
        let cipher = test_cipher();
        let blob = cipher
            .encrypt(b"value", &secret_revision_aad("env-1", "API_KEY", "rev-1"))
            .unwrap();
        assert!(
            cipher
                .decrypt(&blob, &secret_revision_aad("env-2", "API_KEY", "rev-1"))
                .is_err()
        );
        assert!(
            cipher
                .decrypt(&blob, &secret_revision_aad("env-1", "OTHER", "rev-1"))
                .is_err()
        );
    }

    #[test]
    fn another_key_cannot_read_the_ciphertext() {
        let aad = secret_revision_aad("env-1", "API_KEY", "rev-1");
        let blob = test_cipher().encrypt(b"value", &aad).unwrap();
        let other = SecretCipher::from_key_bytes(&[9u8; MASTER_KEY_LEN]).unwrap();
        assert!(other.decrypt(&blob, &aad).is_err());
    }

    #[test]
    fn rejects_a_truncated_blob() {
        let cipher = test_cipher();
        let aad = secret_revision_aad("env-1", "API_KEY", "rev-1");
        assert!(cipher.decrypt(&[0u8; NONCE_LEN], &aad).is_err());
        assert!(cipher.decrypt(&[], &aad).is_err());
    }

    #[test]
    fn loads_raw_and_hex_key_files() {
        let dir = tempfile::tempdir().unwrap();

        let raw_path = dir.path().join("raw.key");
        std::fs::write(&raw_path, [3u8; MASTER_KEY_LEN]).unwrap();
        assert!(SecretCipher::load_from_file(&raw_path).is_ok());

        let hex_path = dir.path().join("hex.key");
        let mut file = std::fs::File::create(&hex_path).unwrap();
        writeln!(file, "{}", hex::encode([3u8; MASTER_KEY_LEN])).unwrap();
        drop(file);
        let from_hex = SecretCipher::load_from_file(&hex_path).unwrap();

        // Both encodings must yield the same key.
        let aad = secret_revision_aad("env-1", "API_KEY", "rev-1");
        let blob = from_hex.encrypt(b"value", &aad).unwrap();
        let from_raw = SecretCipher::load_from_file(&raw_path).unwrap();
        assert_eq!(from_raw.decrypt(&blob, &aad).unwrap(), b"value");
    }

    #[test]
    fn rejects_a_key_of_the_wrong_length() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("short.key");
        std::fs::write(&path, b"too short").unwrap();
        assert!(SecretCipher::load_from_file(&path).is_err());
        assert!(SecretCipher::from_key_bytes(&[0u8; 16]).is_err());
    }

    #[test]
    fn a_missing_key_file_disables_secrets_loudly() {
        let backend = SecretsBackend::load(Some(Path::new("/nonexistent/deis-master.key")));
        assert!(!backend.is_ready());
        let err = backend.cipher().unwrap_err();
        assert!(matches!(err, CatalogError::SecretsUnavailable(_)));

        let unset = SecretsBackend::load(None);
        assert!(!unset.is_ready());
        assert!(unset.cipher().is_err());
    }

    #[test]
    fn debug_output_never_carries_key_material() {
        let cipher = test_cipher();
        let rendered = format!("{cipher:?}");
        assert!(rendered.contains("redacted"));
        assert!(!rendered.contains("07"));

        let derived = cipher.derive_key("test-purpose-v1");
        let rendered = format!("{derived:?}");
        assert!(rendered.contains("redacted"));
        assert!(rendered.contains("test-purpose-v1"));
        assert!(!rendered.contains(&hex::encode(derived.as_bytes())));
    }

    #[test]
    fn derived_keys_are_deterministic_and_bound_to_master_key_and_purpose() {
        let a = test_cipher().derive_key("purpose-a-v1");
        let again = test_cipher().derive_key("purpose-a-v1");
        assert_eq!(a.as_bytes(), again.as_bytes());
        assert_eq!(a.as_bytes().len(), DERIVED_KEY_LEN);
        assert_eq!(a.purpose(), "purpose-a-v1");

        let other_purpose = test_cipher().derive_key("purpose-b-v1");
        assert_ne!(a.as_bytes(), other_purpose.as_bytes());

        let other_master = SecretCipher::from_key_bytes(&[9u8; MASTER_KEY_LEN]).unwrap();
        assert_ne!(
            a.as_bytes(),
            other_master.derive_key("purpose-a-v1").as_bytes()
        );

        // A subkey is never the master key itself.
        assert_ne!(a.as_bytes(), &[7u8; MASTER_KEY_LEN]);

        let unavailable = SecretsBackend::Unavailable("no key".to_string());
        assert!(matches!(
            unavailable.derive_key("purpose-a-v1"),
            Err(CatalogError::SecretsUnavailable(_))
        ));
        let ready = SecretsBackend::Ready(Box::new(test_cipher()));
        assert_eq!(
            ready.derive_key("purpose-a-v1").unwrap().as_bytes(),
            a.as_bytes()
        );
    }
}
