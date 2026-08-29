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

use std::fmt;
use std::path::Path;

use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::{Aes256Gcm, Key, Nonce};
use rand::RngCore;
use tracing::{error, info, warn};

use super::{CatalogError, CatalogResult};

/// AES-256 key length in bytes.
pub const MASTER_KEY_LEN: usize = 32;
/// AES-GCM nonce length in bytes, prefixed to every ciphertext.
pub const NONCE_LEN: usize = 12;

/// Holds the master key. Deliberately opaque: no `Debug` output, no
/// accessor, no serialization.
pub struct SecretCipher {
    cipher: Aes256Gcm,
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
        let key = Key::<Aes256Gcm>::from_slice(key);
        Ok(Self {
            cipher: Aes256Gcm::new(key),
        })
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

/// Associated data binding a ciphertext to one environment and name.
pub fn secret_aad(env_id: &str, name: &str) -> Vec<u8> {
    format!("{env_id}\u{1f}{name}").into_bytes()
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
        let aad = secret_aad("env-1", "API_KEY");
        let blob = cipher.encrypt(b"s3cr3t", &aad).unwrap();

        assert!(blob.len() > NONCE_LEN);
        // The plaintext must not be recoverable by looking at the blob.
        assert!(!blob.windows(6).any(|w| w == b"s3cr3t"));

        let plaintext = cipher.decrypt(&blob, &aad).unwrap();
        assert_eq!(plaintext, b"s3cr3t");
    }

    #[test]
    fn every_encryption_uses_a_fresh_nonce() {
        let cipher = test_cipher();
        let aad = secret_aad("env-1", "API_KEY");
        let a = cipher.encrypt(b"same", &aad).unwrap();
        let b = cipher.encrypt(b"same", &aad).unwrap();
        assert_ne!(a[..NONCE_LEN], b[..NONCE_LEN]);
        assert_ne!(a, b);
    }

    #[test]
    fn a_blob_moved_to_another_environment_does_not_decrypt() {
        let cipher = test_cipher();
        let blob = cipher
            .encrypt(b"value", &secret_aad("env-1", "API_KEY"))
            .unwrap();
        assert!(
            cipher
                .decrypt(&blob, &secret_aad("env-2", "API_KEY"))
                .is_err()
        );
        assert!(
            cipher
                .decrypt(&blob, &secret_aad("env-1", "OTHER"))
                .is_err()
        );
    }

    #[test]
    fn another_key_cannot_read_the_ciphertext() {
        let aad = secret_aad("env-1", "API_KEY");
        let blob = test_cipher().encrypt(b"value", &aad).unwrap();
        let other = SecretCipher::from_key_bytes(&[9u8; MASTER_KEY_LEN]).unwrap();
        assert!(other.decrypt(&blob, &aad).is_err());
    }

    #[test]
    fn rejects_a_truncated_blob() {
        let cipher = test_cipher();
        let aad = secret_aad("env-1", "API_KEY");
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
        let aad = secret_aad("env-1", "API_KEY");
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
    }
}
