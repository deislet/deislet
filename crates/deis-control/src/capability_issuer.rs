//! Capability issuance: the control plane mints Ed25519-signed capability
//! tokens that a node presents to store, queue, or telemetry.
//!
//! ADR-025 defines short-lived (≤ 15 min) tokens carrying a node's identity,
//! the audience, an auth_epoch for mass revocation, and the set of
//! (tenant, binding) pairs the node may access.
//!
//! This module owns the signing key, which is either stored encrypted in the
//! database or generated fresh on every start (single-box mode, when no
//! master key is available).

use std::sync::Arc;
use std::time::Duration;

use deis_capability::{CapabilityClaims, CapabilityResource, encode};
use ed25519_dalek::SigningKey;
use rusqlite::Connection;
use tokio::sync::RwLock;
use tracing::info;

/// How long a capability token is valid for.
pub const CAPABILITY_LIFETIME: Duration = Duration::from_secs(900); // 15 minutes

/// The audience strings used for each backend.
pub const AUD_STORE: &str = "store";
pub const AUD_QUEUE: &str = "queue";
pub const AUD_TELEMETRY: &str = "telemetry";

/// The issuer of capability tokens.
///
/// Holds the active signing key behind a read-write lock so key rotation can
/// replace it without stopping traffic.
#[derive(Clone)]
pub struct CapabilityIssuer {
    inner: Arc<RwLock<IssuerInner>>,
    /// The SPIFFE ID used as the `iss` field in every token.
    issuer_id: String,
}

struct IssuerInner {
    key: SigningKey,
    /// Current auth epoch, loaded from DB or defaulted to 1.
    auth_epoch: u64,
}

impl CapabilityIssuer {
    /// Create an ephemeral issuer with a freshly-generated signing key.
    ///
    /// Used when the database is not yet available (early startup) or when
    /// DB-backed persistence is not needed.
    pub async fn ephemeral(issuer_id: &str) -> Self {
        let mut seed = [0u8; 32];
        rand::Rng::fill(&mut rand::rngs::OsRng, &mut seed);
        let key = SigningKey::from_bytes(&seed);

        let public_hex = hex::encode(key.verifying_key().as_bytes());
        info!(
            key_id = %public_hex,
            "Capability issuer ready (ephemeral)"
        );

        Self {
            inner: Arc::new(RwLock::new(IssuerInner { key, auth_epoch: 1 })),
            issuer_id: issuer_id.to_string(),
        }
    }

    /// Synchronous constructor for tests that can't use `ephemeral()`.
    /// Generates a fresh key every time; the caller must be inside a Tokio
    /// runtime for `RwLock` to function.
    #[doc(hidden)]
    pub fn for_test(issuer_id: &str) -> Self {
        let mut seed = [0u8; 32];
        rand::Rng::fill(&mut rand::rngs::OsRng, &mut seed);
        let key = SigningKey::from_bytes(&seed);
        Self {
            inner: Arc::new(RwLock::new(IssuerInner { key, auth_epoch: 1 })),
            issuer_id: issuer_id.to_string(),
        }
    }

    /// Load or generate the signing key from the database.
    ///
    /// When `master_key` is `None` (single-box mode), the key is stored
    /// plain in the database.
    pub async fn open(
        conn: &Connection,
        master_key: Option<&[u8; 32]>,
        issuer_id: &str,
    ) -> rusqlite::Result<Self> {
        let (key, auth_epoch) = load_or_create_key(conn, master_key)?;

        info!(auth_epoch = auth_epoch, "Capability issuer ready");

        Ok(Self {
            inner: Arc::new(RwLock::new(IssuerInner { key, auth_epoch })),
            issuer_id: issuer_id.to_string(),
        })
    }

    /// Issue a capability token for a node.
    pub async fn issue(
        &self,
        aud: &str,
        subject: &str,
        resources: Vec<CapabilityResource>,
    ) -> String {
        let inner = self.inner.read().await;
        let claims = CapabilityClaims::new(
            aud,
            &self.issuer_id,
            subject,
            inner.auth_epoch,
            CAPABILITY_LIFETIME,
            resources,
        );
        encode(&claims, &inner.key)
    }

    /// The verifying key that store/queue/telemetry need in order to
    /// verify tokens issued by this control plane.
    pub async fn verifying_key(&self) -> ed25519_dalek::VerifyingKey {
        self.inner.read().await.key.verifying_key()
    }

    /// The current auth epoch.
    pub async fn auth_epoch(&self) -> u64 {
        self.inner.read().await.auth_epoch
    }

    /// Bump the auth epoch, forcing all existing tokens to become invalid.
    /// Persisted to the database.
    pub async fn bump_auth_epoch(&self, conn: &Connection) -> rusqlite::Result<u64> {
        let now = now_secs();

        conn.execute(
            "INSERT OR REPLACE INTO capability_auth_epoch (id, epoch, created_at)
             VALUES (1, COALESCE((SELECT epoch + 1 FROM capability_auth_epoch), 1), ?1)",
            rusqlite::params![now],
        )?;

        let new_epoch: i64 = conn.query_row(
            "SELECT epoch FROM capability_auth_epoch WHERE id = 1",
            [],
            |row| row.get(0),
        )?;

        let mut inner = self.inner.write().await;
        inner.auth_epoch = new_epoch as u64;

        info!(auth_epoch = new_epoch, "Auth epoch bumped");
        Ok(new_epoch as u64)
    }

    /// Record a revoked jti so future verifications reject it.
    pub fn record_revocation(&self, conn: &Connection, jti: &str) -> rusqlite::Result<()> {
        let now = now_secs();
        conn.execute(
            "INSERT OR IGNORE INTO capability_revocations (jti, revoked_at) VALUES (?1, ?2)",
            rusqlite::params![jti, now],
        )?;
        Ok(())
    }

    /// Prune revocations older than the maximum token lifetime.
    pub fn prune_revocations(&self, conn: &Connection) -> rusqlite::Result<usize> {
        let cutoff = now_secs().saturating_sub((CAPABILITY_LIFETIME.as_secs() + 60) as i64);
        let removed = conn.execute(
            "DELETE FROM capability_revocations WHERE revoked_at < ?1",
            rusqlite::params![cutoff],
        )?;
        Ok(removed)
    }
}

// ---- internal helpers ----------------------------------------------------

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
        .try_into()
        .unwrap_or(i64::MAX)
}

/// Load the active signing key from the database, or generate a fresh one.
fn load_or_create_key(
    conn: &Connection,
    master_key: Option<&[u8; 32]>,
) -> rusqlite::Result<(SigningKey, u64)> {
    let now = now_secs();
    let key_id: Option<String> = conn
        .query_row(
            "SELECT key_id FROM capability_signing_keys WHERE active = 1 LIMIT 1",
            [],
            |row| row.get(0),
        )
        .optional()?;

    match key_id {
        Some(_kid) => {
            let private_key_bytes: Vec<u8> = conn.query_row(
                "SELECT private_key FROM capability_signing_keys WHERE active = 1",
                [],
                |row| row.get(0),
            )?;

            let raw_key = if let Some(mk) = master_key {
                decrypt_key(&private_key_bytes, mk).map_err(|_| {
                    rusqlite::Error::InvalidParameterName(
                        "cannot decrypt stored signing key: master key mismatch".to_string(),
                    )
                })?
            } else {
                // Single-box: the stored key is plain
                private_key_bytes
            };

            let raw: [u8; 32] = raw_key.try_into().map_err(|_| {
                rusqlite::Error::InvalidParameterName(
                    "stored signing key is not 32 bytes".to_string(),
                )
            })?;

            let signing_key = SigningKey::from_bytes(&raw);

            let auth_epoch: Option<i64> = conn
                .query_row(
                    "SELECT epoch FROM capability_auth_epoch WHERE id = 1",
                    [],
                    |row| row.get(0),
                )
                .optional()?;

            let epoch = auth_epoch.unwrap_or(1) as u64;

            info!(
                key_id = %hex::encode(signing_key.verifying_key().as_bytes()),
                auth_epoch = epoch,
                "Loaded existing signing key"
            );
            Ok((signing_key, epoch))
        }
        None => {
            // No existing key: generate a fresh one.
            let mut seed = [0u8; 32];
            rand::Rng::fill(&mut rand::rngs::OsRng, &mut seed);
            let signing_key = SigningKey::from_bytes(&seed);
            let public_key = signing_key.verifying_key().to_bytes();
            let private_key_raw = signing_key.to_bytes();

            let stored = if let Some(mk) = master_key {
                encrypt_key(&private_key_raw, mk).map_err(|_| {
                    rusqlite::Error::InvalidParameterName("cannot encrypt signing key".to_string())
                })?
            } else {
                private_key_raw.to_vec()
            };

            conn.execute(
                "INSERT INTO capability_signing_keys (key_id, public_key, private_key, created_at, active)
                 VALUES (?1, ?2, ?3, ?4, 1)",
                rusqlite::params![hex::encode(public_key), &public_key[..], stored, now],
            )?;

            // Seed the auth_epoch
            conn.execute(
                "INSERT OR IGNORE INTO capability_auth_epoch (id, epoch, created_at) VALUES (1, 1, ?1)",
                rusqlite::params![now],
            )?;

            info!(
                key_id = %hex::encode(public_key),
                "Generated new signing key"
            );

            Ok((signing_key, 1))
        }
    }
}

// ---- trivial encryption helpers for key-at-rest -------------------------
//
// In a standalone deployment the signing key is stored plain in the SQLite
// database. When a master key is configured the key bytes are encrypted with
// AES-256-GCM. This is the same crypto that protects environment secrets.
// The nonce is prefixed, and the tag is appended.

fn encrypt_key(plain: &[u8], master_key: &[u8; 32]) -> Result<Vec<u8>, &'static str> {
    use aes_gcm::aead::Aead;
    use aes_gcm::{Aes256Gcm, KeyInit, Nonce};
    use rand::Rng;

    let cipher = Aes256Gcm::new_from_slice(master_key).map_err(|_| "bad key size")?;
    let nonce_bytes: [u8; 12] = rand::rngs::OsRng.r#gen();
    let nonce = Nonce::from_slice(&nonce_bytes);
    let mut ciphertext = cipher
        .encrypt(nonce, plain.as_ref())
        .map_err(|_| "encrypt failure")?;
    // Prepend nonce (12 bytes) to ciphertext (plain + 16-byte tag)
    let mut out = nonce_bytes.to_vec();
    out.append(&mut ciphertext);
    Ok(out)
}

fn decrypt_key(encrypted: &[u8], master_key: &[u8; 32]) -> Result<Vec<u8>, &'static str> {
    use aes_gcm::aead::Aead;
    use aes_gcm::{Aes256Gcm, KeyInit, Nonce};

    if encrypted.len() < 12 + 16 {
        return Err("ciphertext too short");
    }
    let (nonce_bytes, ciphertext) = encrypted.split_at(12);
    let cipher = Aes256Gcm::new_from_slice(master_key).map_err(|_| "bad key size")?;
    let nonce = Nonce::from_slice(nonce_bytes);
    cipher
        .decrypt(nonce, ciphertext.as_ref())
        .map_err(|_| "decrypt failure")
}

// The `optional()` combinator rusqlite does not provide by default.
use rusqlite::OptionalExtension;

#[cfg(test)]
mod tests {
    use super::*;

    fn in_memory_db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        // Run the capability migration manually for tests.
        // We can't rely on the full migration framework here, so run the SQL directly.
        conn.execute_batch(include_str!(
            "database/migrations/0003_capability_signing.sql"
        ))
        .unwrap();
        conn
    }

    #[tokio::test]
    async fn issuer_generates_key_on_first_start() {
        let conn = in_memory_db();
        let issuer = CapabilityIssuer::open(&conn, None, "spiffe://deislet/control/main")
            .await
            .unwrap();

        assert_eq!(issuer.auth_epoch().await, 1);

        let token = issuer.issue(
            AUD_STORE,
            "spiffe://deislet/node/n1",
            vec![CapabilityResource {
                tenant: "v1/018f2518-7e38-7f80-8000-000000000001/018f2518-7e38-7f80-8000-000000000002".into(),
                binding: "kv".into(),
                generation: None,
            }],
        ).await;

        assert!(token.starts_with("v1."));

        // Verify with the matching key
        let vk = issuer.verifying_key().await;
        let claims = deis_capability::decode(&token, &vk).unwrap();
        assert_eq!(claims.aud, AUD_STORE);
        assert_eq!(claims.sub, "spiffe://deislet/node/n1");
        assert!(claims.resources.len() == 1);
    }

    #[tokio::test]
    async fn issuer_persists_and_reloads_key() {
        let conn = in_memory_db();
        let issuer = CapabilityIssuer::open(&conn, None, "spiffe://deislet/control/main")
            .await
            .unwrap();

        let vk_before = issuer.verifying_key().await;
        let token = issuer
            .issue(AUD_STORE, "spiffe://deislet/node/n1", vec![])
            .await;

        // Re-open the same issuer (simulates restart)
        let issuer2 = CapabilityIssuer::open(&conn, None, "spiffe://deislet/control/main")
            .await
            .unwrap();

        let vk_after = issuer2.verifying_key().await;
        assert_eq!(vk_before, vk_after, "key must survive restart");

        // Token from before must verify after restart
        let claims = deis_capability::decode(&token, &vk_after).unwrap();
        assert_eq!(claims.sub, "spiffe://deislet/node/n1");
    }

    #[tokio::test]
    async fn bump_auth_epoch_increments() {
        let conn = in_memory_db();
        let issuer = CapabilityIssuer::open(&conn, None, "spiffe://deislet/control/main")
            .await
            .unwrap();

        assert_eq!(issuer.auth_epoch().await, 1);

        let epoch = issuer.bump_auth_epoch(&conn).await.unwrap();
        assert_eq!(epoch, 2);

        let epoch = issuer.bump_auth_epoch(&conn).await.unwrap();
        assert_eq!(epoch, 3);
    }
}
