//! Capability issuance: the control plane mints Ed25519-signed capability
//! tokens that a node presents to store, queue, or telemetry.
//!
//! ADR-025 defines short-lived (≤ 15 min) tokens carrying a node's identity,
//! the audience, an auth_epoch for mass revocation, and the set of
//! (tenant, binding) pairs the node may access.
//!
//! # The signing keys
//!
//! Keys live in `capability_signing_keys`, each named by its RFC 7638
//! thumbprint (the `kid` every token carries). The private half is stored as
//! AES-256-GCM ciphertext under the secrets master key (`secret_key_file`),
//! with the kid as associated data; the control database - and every
//! Litestream replica of it - never holds a usable signing key. There is no
//! plaintext fallback: a control plane without a master key refuses to start
//! rather than mint tokens under a key that dies with the process or sits in
//! the clear in a backup.
//!
//! # Rotation
//!
//! Each key is in one of three states, and [`CapabilityIssuer::maintain`]
//! moves them along on a schedule ([`RotationPolicy`]):
//!
//! - `pending`: published in the JWKS, not yet signing. Staged when the
//!   active key reaches `rotate_after`, so every verifier has fetched it
//!   before the first token signed with it exists.
//! - `active`: signs every new token. Exactly one.
//! - `previous`: the key that was active before; still published so the
//!   tokens it signed keep verifying, then deleted once they cannot be
//!   accepted anywhere - not even under T8 degraded-mode grace.
//!
//! Every published key is served at [`JWKS_ROUTE`].
//!
//! # What was issued, and taking it back
//!
//! Every token is written to the issuance table
//! ([`crate::capability_ledger`]) before it is handed out; a token that
//! cannot be recorded is not issued. Revocation works from that record: by
//! `jti`, by the node or tenant a token names, or wholesale by bumping the
//! auth epoch. Verifiers learn both from the revocation list, which this
//! issuer signs with its active key ([`CapabilityIssuer::revocation_list`]).

use std::sync::Arc;
use std::time::Duration;

use deis_capability::revocation::{self, RevocationList};
use deis_capability::{CapabilityClaims, CapabilityResource, Jwks, encode, key_id};
use ed25519_dalek::{SigningKey, VerifyingKey};
use rusqlite::{Connection, OptionalExtension};
use tokio::sync::RwLock;
use tracing::{info, warn};

use crate::capability_ledger::CapabilityLedger;
use crate::catalog::secrets::SecretCipher;
use crate::database::{Database, DatabaseError};

/// How long a capability token is valid for.
pub const CAPABILITY_LIFETIME: Duration = Duration::from_secs(900); // 15 minutes

/// The audience strings used for each backend.
pub use deis_capability::{AUD_QUEUE, AUD_STORE};
pub const AUD_TELEMETRY: &str = "telemetry";

/// Where every published verifying key is served, as a JWKS document: on
/// the machine listener only, where deis-store, deis-queue and
/// deis-telemetry read it under mutual TLS.
pub const JWKS_ROUTE: &str = deis_capability::JWKS_PATH;

/// Where the signed revocation list is served, on the machine listener only.
pub use deis_capability::revocation::REVOCATION_LIST_PATH as REVOCATION_LIST_ROUTE;

/// How often the rotation schedule is checked.
pub const MAINTENANCE_INTERVAL: Duration = Duration::from_secs(600);

/// When keys move between states.
#[derive(Debug, Clone)]
pub struct RotationPolicy {
    /// Age of the active key at which a successor is staged.
    pub rotate_after: Duration,
    /// How long a staged key is published before it starts signing. Must
    /// comfortably exceed a verifier's periodic JWKS refresh (5 minutes).
    pub publish_lead: Duration,
    /// How long a replaced key stays published. Must cover the longest a
    /// token it signed can still be accepted: its lifetime plus the T8
    /// degraded-mode grace window.
    pub retire_after: Duration,
}

impl Default for RotationPolicy {
    fn default() -> Self {
        Self {
            rotate_after: Duration::from_secs(30 * 24 * 3600),
            publish_lead: Duration::from_secs(3600),
            retire_after: CAPABILITY_LIFETIME
                + Duration::from_secs(deis_capability::MAX_GRACE_PERIOD_SECS)
                + Duration::from_secs(3600),
        }
    }
}

/// Why the issuer could not load or change its keys.
#[derive(Debug, thiserror::Error)]
pub enum IssuerError {
    #[error("capability signing key storage: {0}")]
    Database(#[from] rusqlite::Error),
    #[error("capability signing key storage: {0}")]
    Connection(#[from] DatabaseError),
    #[error("capability signing key {kid}: {reason}")]
    Key { kid: String, reason: String },
    #[error("no active capability signing key is stored")]
    NoActiveKey,
}

/// The issuer of capability tokens.
///
/// Holds the key state behind a read-write lock so rotation can replace it
/// without stopping traffic.
#[derive(Clone)]
pub struct CapabilityIssuer {
    inner: Arc<RwLock<KeyState>>,
    /// The SPIFFE ID used as the `iss` field in every token.
    issuer_id: String,
    /// The master key the stored private keys are encrypted under. `None`
    /// for an in-memory issuer, which never rotates.
    vault: Option<Arc<SecretCipher>>,
    policy: RotationPolicy,
    /// Where every issued token is recorded, and revocations kept. Every
    /// issuer has one: an issuer that could not say what it revoked would
    /// answer every verifier "nothing".
    ledger: CapabilityLedger,
}

/// What the issuer signs and publishes with.
struct KeyState {
    signing: SigningKey,
    /// Every published key: pending, active and previous.
    published: Vec<VerifyingKey>,
    /// Current auth epoch, loaded from DB or defaulted to 1.
    auth_epoch: u64,
}

/// The state a key is generated into. A key only reaches `previous` by
/// being replaced, which [`rotation_step`] does in SQL.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stage {
    Pending,
    Active,
}

impl Stage {
    fn as_str(self) -> &'static str {
        match self {
            Stage::Pending => "pending",
            Stage::Active => "active",
        }
    }
}

impl CapabilityIssuer {
    /// An in-memory issuer with a freshly generated key that is never
    /// stored, and a ledger in a database of its own. For tests: tokens it
    /// signs stop verifying when it is dropped.
    pub async fn ephemeral(issuer_id: &str) -> Self {
        Self::for_test(issuer_id)
    }

    /// Synchronous form of [`Self::ephemeral`].
    #[doc(hidden)]
    pub fn for_test(issuer_id: &str) -> Self {
        let ledger = Database::in_memory_blocking()
            .expect("an in-memory database for a test issuer's ledger");
        let signing = generate_key();
        Self {
            inner: Arc::new(RwLock::new(KeyState {
                published: vec![signing.verifying_key()],
                signing,
                auth_epoch: 1,
            })),
            issuer_id: issuer_id.to_string(),
            vault: None,
            policy: RotationPolicy::default(),
            ledger: CapabilityLedger::new(Arc::new(ledger)),
        }
    }

    /// Load the stored keys, generating and storing the first one on a
    /// fresh database. Private keys are encrypted under `vault`; every token
    /// issued is recorded, and revocations kept, in `ledger`'s tables.
    pub fn open(
        conn: &Connection,
        vault: Arc<SecretCipher>,
        issuer_id: &str,
        ledger: Arc<Database>,
    ) -> Result<Self, IssuerError> {
        let now = now_secs();
        let tx = conn.unchecked_transaction()?;
        let has_active: bool = tx
            .query_row(
                "SELECT 1 FROM capability_signing_keys WHERE state = 'active'",
                [],
                |_| Ok(()),
            )
            .optional()?
            .is_some();
        if !has_active {
            let kid = insert_key(&tx, &vault, Stage::Active, now)?;
            info!(kid = %kid, "Generated the first capability signing key");
        }
        tx.execute(
            "INSERT OR IGNORE INTO capability_auth_epoch (id, epoch, created_at) VALUES (1, 1, ?1)",
            rusqlite::params![now],
        )?;
        tx.commit()?;

        let state = load_state(conn, &vault)?;
        info!(
            kid = %key_id(&state.signing.verifying_key()),
            published = state.published.len(),
            auth_epoch = state.auth_epoch,
            "Capability issuer ready; its private keys are stored encrypted under the secrets master key"
        );
        Ok(Self {
            inner: Arc::new(RwLock::new(state)),
            issuer_id: issuer_id.to_string(),
            vault: Some(vault),
            policy: RotationPolicy::default(),
            ledger: CapabilityLedger::new(ledger),
        })
    }

    /// Record every token issued from now on in `db`'s issuance table, and
    /// serve revocations from it - for a test that keeps its catalog and
    /// its ledger in one database, as a running control plane does.
    pub fn with_ledger(mut self, db: Arc<Database>) -> Self {
        self.ledger = CapabilityLedger::new(db);
        self
    }

    /// The issuance and revocation tables.
    pub fn ledger(&self) -> &CapabilityLedger {
        &self.ledger
    }

    /// Replace the rotation schedule; tests use a short one.
    pub fn with_policy(mut self, policy: RotationPolicy) -> Self {
        self.policy = policy;
        self
    }

    /// Issue a capability token, valid for [`CAPABILITY_LIFETIME`].
    ///
    /// Recorded in the issuance table before it is returned; if the record
    /// cannot be written, nothing is issued - a token nobody can find is a
    /// token nobody can revoke.
    pub async fn issue(
        &self,
        aud: &str,
        subject: &str,
        resources: Vec<CapabilityResource>,
    ) -> Result<String, IssuerError> {
        let (token, claims) = {
            let inner = self.inner.read().await;
            let claims = CapabilityClaims::new(
                aud,
                &self.issuer_id,
                subject,
                inner.auth_epoch,
                CAPABILITY_LIFETIME,
                resources,
            );
            let token = encode(&claims, &inner.signing);
            let claims = CapabilityClaims {
                kid: key_id(&inner.signing.verifying_key()),
                ..claims
            };
            (token, claims)
        };
        self.ledger.record(&claims).await?;
        Ok(token)
    }

    /// The revocation list every verifier polls, signed with the active key:
    /// the current auth epoch, every revoked capability that could still be
    /// accepted somewhere, and every revoked node.
    pub async fn revocation_list(&self) -> Result<String, IssuerError> {
        let revoked = self.ledger.revoked_jtis().await?;
        let revoked_nodes = self.ledger.revoked_nodes().await?;
        let inner = self.inner.read().await;
        Ok(revocation::sign(
            &RevocationList::new(&self.issuer_id, inner.auth_epoch, revoked)
                .with_revoked_nodes(revoked_nodes),
            &inner.signing,
        ))
    }

    /// The key new tokens are signed with.
    pub async fn verifying_key(&self) -> VerifyingKey {
        self.inner.read().await.signing.verifying_key()
    }

    /// Every key a verifier must accept right now.
    pub async fn published_keys(&self) -> Vec<VerifyingKey> {
        self.inner.read().await.published.clone()
    }

    /// The JWKS document served at [`JWKS_ROUTE`].
    pub async fn jwks(&self) -> Jwks {
        Jwks::from_keys(&self.inner.read().await.published)
    }

    /// Run one step of the rotation schedule against `db` and install the
    /// result. A no-op for an in-memory issuer.
    pub async fn maintain(&self, db: &Database) -> Result<(), IssuerError> {
        let Some(vault) = self.vault.clone() else {
            return Ok(());
        };
        let policy = self.policy.clone();
        let changed = db
            .with_connection(move |conn| rotation_step(conn, &vault, &policy, now_secs()))
            .await?;
        if let Some(state) = changed {
            self.install(state).await;
        }
        let (revocations, issuances) = self.ledger.prune().await?;
        if revocations + issuances > 0 {
            info!(
                revocations,
                issuances,
                "Pruned revocations no verifier can need and issuance records past retention"
            );
        }
        Ok(())
    }

    /// [`Self::maintain`] on a bare connection at a chosen time, for tests.
    #[doc(hidden)]
    pub async fn maintain_at(&self, conn: &Connection, now: i64) -> Result<(), IssuerError> {
        let Some(vault) = self.vault.as_ref() else {
            return Ok(());
        };
        if let Some(state) = rotation_step(conn, vault, &self.policy, now)? {
            self.install(state).await;
        }
        Ok(())
    }

    /// Check the rotation schedule every [`MAINTENANCE_INTERVAL`] until
    /// `shutdown` fires.
    pub fn spawn_maintenance(
        self,
        db: Arc<Database>,
        shutdown: crate::shutdown::Shutdown,
    ) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = tokio::time::sleep(MAINTENANCE_INTERVAL) => {}
                    _ = shutdown.wait() => return,
                }
                if let Err(error) = self.maintain(&db).await {
                    warn!(
                        %error,
                        "Could not advance the capability key rotation; the current keys keep \
                         signing and verifying, and the step is retried on the next check"
                    );
                }
            }
        })
    }

    async fn install(&self, state: KeyState) {
        let kid = key_id(&state.signing.verifying_key());
        let published: Vec<String> = state.published.iter().map(key_id).collect();
        let mut inner = self.inner.write().await;
        inner.signing = state.signing;
        inner.published = state.published;
        info!(signing_kid = %kid, published = ?published, "Capability signing keys changed");
    }

    /// The current auth epoch.
    pub async fn auth_epoch(&self) -> u64 {
        self.inner.read().await.auth_epoch
    }

    /// Bump the auth epoch: every token issued so far is refused by every
    /// verifier once it adopts the next revocation list, and every token
    /// issued from now on carries the new epoch. Persisted, so it survives a
    /// restart. Mass revocation - nodes renew, anything else has to ask again.
    pub async fn bump_auth_epoch(&self) -> Result<u64, IssuerError> {
        let epoch = self
            .ledger
            .database()
            .with_connection(|conn| bump_epoch_on(conn).map_err(IssuerError::from))
            .await?;
        self.set_auth_epoch(epoch).await;
        Ok(epoch)
    }

    /// [`Self::bump_auth_epoch`] on a bare connection, for tests.
    #[doc(hidden)]
    pub async fn bump_auth_epoch_on(&self, conn: &Connection) -> Result<u64, IssuerError> {
        let epoch = bump_epoch_on(conn)?;
        self.set_auth_epoch(epoch).await;
        Ok(epoch)
    }

    async fn set_auth_epoch(&self, epoch: u64) {
        let mut inner = self.inner.write().await;
        inner.auth_epoch = epoch;
        info!(auth_epoch = epoch, "Auth epoch bumped; every capability issued before now is refused once verifiers adopt the next revocation list");
    }
}

/// Increment the stored epoch and return the new value.
fn bump_epoch_on(conn: &Connection) -> rusqlite::Result<u64> {
    conn.execute(
        "INSERT OR REPLACE INTO capability_auth_epoch (id, epoch, created_at)
         VALUES (1, COALESCE((SELECT epoch + 1 FROM capability_auth_epoch), 2), ?1)",
        rusqlite::params![now_secs()],
    )?;
    let epoch: i64 = conn.query_row(
        "SELECT epoch FROM capability_auth_epoch WHERE id = 1",
        [],
        |row| row.get(0),
    )?;
    Ok(epoch.max(0) as u64)
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

fn generate_key() -> SigningKey {
    let mut seed = [0u8; 32];
    rand::Rng::fill(&mut rand::rngs::OsRng, &mut seed);
    SigningKey::from_bytes(&seed)
}

/// Associated data for a stored private key: binds the ciphertext to its
/// kid and keeps it from ever decrypting as a secret value, or a secret
/// value as a key.
fn key_aad(kid: &str) -> Vec<u8> {
    format!("capability-signing-key\u{1f}{kid}").into_bytes()
}

/// Generate a key, store it in `stage`, and return its kid.
fn insert_key(
    conn: &Connection,
    vault: &SecretCipher,
    stage: Stage,
    now: i64,
) -> Result<String, IssuerError> {
    let key = generate_key();
    let public = key.verifying_key();
    let kid = key_id(&public);
    let ciphertext = vault
        .encrypt(&key.to_bytes(), &key_aad(&kid))
        .map_err(|e| IssuerError::Key {
            kid: kid.clone(),
            reason: e.to_string(),
        })?;
    conn.execute(
        "INSERT INTO capability_signing_keys
             (kid, public_key, private_key, state, created_at, state_changed_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?5)",
        rusqlite::params![kid, &public.as_bytes()[..], ciphertext, stage.as_str(), now],
    )?;
    Ok(kid)
}

/// Read every stored key; decrypt the active one.
fn load_state(conn: &Connection, vault: &SecretCipher) -> Result<KeyState, IssuerError> {
    let mut statement = conn.prepare(
        "SELECT kid, public_key, private_key, state FROM capability_signing_keys
         ORDER BY created_at, kid",
    )?;
    let rows = statement.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, Vec<u8>>(1)?,
            row.get::<_, Vec<u8>>(2)?,
            row.get::<_, String>(3)?,
        ))
    })?;

    let mut signing = None;
    let mut published = Vec::new();
    for row in rows {
        let (kid, public, private, state) = row?;
        let bad = |reason: &str| IssuerError::Key {
            kid: kid.clone(),
            reason: reason.to_string(),
        };
        let public: [u8; 32] = public
            .try_into()
            .map_err(|_| bad("stored public key is not 32 bytes"))?;
        let public =
            VerifyingKey::from_bytes(&public).map_err(|_| bad("stored public key is invalid"))?;
        if key_id(&public) != kid {
            return Err(bad("stored kid is not the thumbprint of the stored public key"));
        }
        if state == Stage::Active.as_str() {
            let raw = vault.decrypt(&private, &key_aad(&kid)).map_err(|_| {
                bad("private key does not decrypt with the configured secret_key_file; the \
                     master key was replaced or the row was copied from another installation")
            })?;
            let raw: [u8; 32] = raw
                .try_into()
                .map_err(|_| bad("decrypted private key is not 32 bytes"))?;
            let key = SigningKey::from_bytes(&raw);
            if key.verifying_key() != public {
                return Err(bad("private key does not match the stored public key"));
            }
            signing = Some(key);
        }
        published.push(public);
    }

    let auth_epoch: Option<i64> = conn
        .query_row(
            "SELECT epoch FROM capability_auth_epoch WHERE id = 1",
            [],
            |row| row.get(0),
        )
        .optional()?;

    Ok(KeyState {
        signing: signing.ok_or(IssuerError::NoActiveKey)?,
        published,
        auth_epoch: auth_epoch.unwrap_or(1) as u64,
    })
}

/// Advance the rotation schedule by whatever is due at `now`, in one
/// transaction. Returns the new key state when anything changed.
fn rotation_step(
    conn: &Connection,
    vault: &SecretCipher,
    policy: &RotationPolicy,
    now: i64,
) -> Result<Option<KeyState>, IssuerError> {
    let age = |secs: Duration| now.saturating_sub(secs.as_secs() as i64);
    let tx = conn.unchecked_transaction()?;
    let mut changed = false;

    // Retire: a previous key whose every token is past acceptance.
    let retired = tx.execute(
        "DELETE FROM capability_signing_keys WHERE state = 'previous' AND state_changed_at <= ?1",
        rusqlite::params![age(policy.retire_after)],
    )?;
    if retired > 0 {
        info!(retired, "Retired capability signing key(s); tokens they signed no longer verify anywhere");
        changed = true;
    }

    // Activate: a pending key published for long enough that every verifier
    // has fetched it. The key it replaces becomes previous.
    let ready: Option<String> = tx
        .query_row(
            "SELECT kid FROM capability_signing_keys
             WHERE state = 'pending' AND state_changed_at <= ?1
             ORDER BY created_at LIMIT 1",
            rusqlite::params![age(policy.publish_lead)],
            |row| row.get(0),
        )
        .optional()?;
    if let Some(kid) = ready {
        tx.execute(
            "UPDATE capability_signing_keys SET state = 'previous', state_changed_at = ?1
             WHERE state = 'active'",
            rusqlite::params![now],
        )?;
        tx.execute(
            "UPDATE capability_signing_keys SET state = 'active', state_changed_at = ?1
             WHERE kid = ?2",
            rusqlite::params![now, kid],
        )?;
        info!(kid = %kid, "A new capability signing key is now signing every token");
        changed = true;
    }

    // Stage: the active key is due for replacement and no successor is
    // published yet.
    let due: bool = tx
        .query_row(
            "SELECT 1 FROM capability_signing_keys
             WHERE state = 'active' AND state_changed_at <= ?1",
            rusqlite::params![age(policy.rotate_after)],
            |_| Ok(()),
        )
        .optional()?
        .is_some();
    let pending: bool = tx
        .query_row(
            "SELECT 1 FROM capability_signing_keys WHERE state = 'pending'",
            [],
            |_| Ok(()),
        )
        .optional()?
        .is_some();
    if due && !pending {
        let kid = insert_key(&tx, vault, Stage::Pending, now)?;
        info!(
            kid = %kid,
            publish_lead_secs = policy.publish_lead.as_secs(),
            "Staged the next capability signing key; it is published now and starts signing after the publish lead"
        );
        changed = true;
    }

    if !changed {
        tx.finish()?;
        return Ok(None);
    }
    tx.commit()?;
    Ok(Some(load_state(conn, vault)?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::secrets::MASTER_KEY_LEN;
    use deis_capability::{CapabilityVerifier, KeyRing, resources_for_audience};

    const ISSUER: &str = "spiffe://deislet/control/main";
    const TENANT: &str = "demo.production";

    fn in_memory_db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        crate::database::schema::run_migrations(&conn).unwrap();
        conn
    }

    /// What every verifier does before it accepts anything: adopt a
    /// revocation list, here one naming nothing.
    fn adopt_empty_list(verifier: &mut CapabilityVerifier) {
        verifier
            .apply_revocation_list(deis_capability::RevocationList::new("iss", 0, vec![]))
            .unwrap();
    }

    /// A ledger for an issuer whose ledger no assertion reads.
    fn scratch_ledger() -> Arc<Database> {
        Arc::new(Database::in_memory_blocking().unwrap())
    }

    fn vault(byte: u8) -> Arc<SecretCipher> {
        Arc::new(SecretCipher::from_key_bytes(&[byte; MASTER_KEY_LEN]).unwrap())
    }

    async fn store_token(issuer: &CapabilityIssuer) -> String {
        issuer
            .issue(AUD_STORE, "spiffe://deislet/node/n1", resources_for_audience(TENANT, AUD_STORE))
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn issuer_generates_key_on_first_start() {
        let conn = in_memory_db();
        let issuer = CapabilityIssuer::open(&conn, vault(7), ISSUER, scratch_ledger()).unwrap();
        assert_eq!(issuer.auth_epoch().await, 1);

        let token = store_token(&issuer).await;
        let claims = deis_capability::decode(&token, &issuer.verifying_key().await).unwrap();
        assert_eq!(claims.aud, AUD_STORE);
        assert_eq!(claims.kid, key_id(&issuer.verifying_key().await));
        assert_eq!(issuer.jwks().await.keys.len(), 1);
    }

    /// The property the ephemeral issuer lacked: a token minted before a
    /// restart verifies against the key set published after it.
    #[tokio::test]
    async fn issuer_persists_and_reloads_key() {
        let conn = in_memory_db();
        let issuer = CapabilityIssuer::open(&conn, vault(7), ISSUER, scratch_ledger()).unwrap();
        let token = store_token(&issuer).await;

        let restarted = CapabilityIssuer::open(&conn, vault(7), ISSUER, scratch_ledger()).unwrap();
        assert_eq!(issuer.verifying_key().await, restarted.verifying_key().await);

        let ring = KeyRing::from_keys(restarted.jwks().await.verifying_keys().unwrap());
        let mut verifier = CapabilityVerifier::with_key_ring(ring);
        adopt_empty_list(&mut verifier);
        verifier
            .verify(&token, AUD_STORE, TENANT, "kv")
            .expect("a token from before the restart verifies after it");
    }

    #[tokio::test]
    async fn the_stored_private_key_is_ciphertext_bound_to_the_master_key() {
        let conn = in_memory_db();
        let issuer = CapabilityIssuer::open(&conn, vault(7), ISSUER, scratch_ledger()).unwrap();
        let signing = issuer.inner.read().await.signing.to_bytes();
        let stored: Vec<u8> = conn
            .query_row("SELECT private_key FROM capability_signing_keys", [], |r| r.get(0))
            .unwrap();
        assert!(
            !stored.windows(signing.len()).any(|w| w == signing),
            "the private key must not be stored in the clear"
        );

        match CapabilityIssuer::open(&conn, vault(8), ISSUER, scratch_ledger()) {
            Err(IssuerError::Key { .. }) => {}
            Err(other) => panic!("expected a key error, got {other}"),
            Ok(_) => panic!("a different master key must not open the stored key"),
        }
    }

    /// The whole schedule: stage (published, not signing), activate (old
    /// key still verifies), retire (old key's tokens stop).
    #[tokio::test]
    async fn rotation_publishes_first_signs_next_and_retires_last() {
        let conn = in_memory_db();
        let policy = RotationPolicy {
            rotate_after: Duration::from_secs(100),
            publish_lead: Duration::from_secs(10),
            retire_after: Duration::from_secs(50),
        };
        let issuer = CapabilityIssuer::open(&conn, vault(7), ISSUER, scratch_ledger())
            .unwrap()
            .with_policy(policy);
        let t0 = now_secs();
        let first = issuer.verifying_key().await;
        let old_token = store_token(&issuer).await;

        issuer.maintain_at(&conn, t0 + 50).await.unwrap();
        assert_eq!(issuer.published_keys().await, vec![first], "nothing due yet");

        issuer.maintain_at(&conn, t0 + 101).await.unwrap();
        let published = issuer.published_keys().await;
        assert_eq!(published.len(), 2, "the successor is published");
        assert_eq!(issuer.verifying_key().await, first, "but does not sign yet");

        issuer.maintain_at(&conn, t0 + 112).await.unwrap();
        let second = issuer.verifying_key().await;
        assert_ne!(second, first, "the successor signs after the publish lead");
        let ring = KeyRing::from_keys(issuer.published_keys().await);
        let mut verifier = CapabilityVerifier::with_key_ring(ring.clone());
        adopt_empty_list(&mut verifier);
        verifier
            .verify(&old_token, AUD_STORE, TENANT, "kv")
            .expect("the previous key still verifies");
        let new_token = store_token(&issuer).await;
        assert_eq!(
            deis_capability::decode(&new_token, &second).unwrap().kid,
            key_id(&second)
        );

        issuer.maintain_at(&conn, t0 + 163).await.unwrap();
        assert_eq!(issuer.published_keys().await, vec![second]);
        ring.replace(issuer.published_keys().await);
        assert!(verifier.verify(&old_token, AUD_STORE, TENANT, "kv").is_err());
        verifier
            .verify(&new_token, AUD_STORE, TENANT, "kv")
            .expect("the active key verifies");

        // And the state survives a restart.
        let restarted = CapabilityIssuer::open(&conn, vault(7), ISSUER, scratch_ledger()).unwrap();
        assert_eq!(restarted.verifying_key().await, second);
    }

    #[tokio::test]
    async fn bump_auth_epoch_increments() {
        let conn = in_memory_db();
        let issuer = CapabilityIssuer::open(&conn, vault(7), ISSUER, scratch_ledger()).unwrap();

        assert_eq!(issuer.auth_epoch().await, 1);
        assert_eq!(issuer.bump_auth_epoch_on(&conn).await.unwrap(), 2);
        assert_eq!(issuer.bump_auth_epoch_on(&conn).await.unwrap(), 3);
        let restarted = CapabilityIssuer::open(&conn, vault(7), ISSUER, scratch_ledger()).unwrap();
        assert_eq!(restarted.auth_epoch().await, 3);
    }

    /// Every token is in the issuance table the moment it exists, and the
    /// signed list carries what was revoked and the epoch, verifiable
    /// against the published keys.
    #[tokio::test]
    async fn issued_tokens_are_recorded_and_the_list_is_signed() {
        let db = Arc::new(Database::init(":memory:").await.unwrap());
        let issuer = CapabilityIssuer::for_test(ISSUER).with_ledger(Arc::clone(&db));
        let token = issuer
            .issue(
                AUD_STORE,
                "spiffe://deis.internal/deislet/node/n1",
                resources_for_audience(TENANT, AUD_STORE),
            )
            .await
            .unwrap();
        let claims = deis_capability::decode(&token, &issuer.verifying_key().await).unwrap();
        let revoked = issuer
            .ledger()
            .revoke(crate::capability_ledger::RevocationScope::Node("n1".into()))
            .await
            .unwrap();
        assert_eq!(revoked, vec![claims.jti.clone()]);
        let kid: String = db
            .with_connection({
                let jti = claims.jti.clone();
                move |conn| {
                    conn.query_row(
                        "SELECT kid FROM capability_issuances WHERE jti = ?1",
                        [jti],
                        |row| row.get(0),
                    )
                    .map_err(DatabaseError::from)
                }
            })
            .await
            .unwrap();
        assert_eq!(kid, claims.kid);

        issuer.bump_auth_epoch().await.unwrap();
        let ring = KeyRing::from_keys(issuer.published_keys().await);
        let list = revocation::verify(&issuer.revocation_list().await.unwrap(), &ring).unwrap();
        assert_eq!(list.revoked_jtis, vec![claims.jti]);
        assert_eq!(list.revoked_nodes, vec!["n1"]);
        assert_eq!(list.auth_epoch, 2);
    }
}
