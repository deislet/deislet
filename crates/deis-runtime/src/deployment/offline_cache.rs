//! What a node needs to come back up while the control plane is down.
//!
//! A node that restarts with the control plane unreachable used to come back
//! with only what its bundle store records: code without its secrets, and no
//! capability to reach storage with. Under the T8 outage rules it should go
//! on serving for up to 24 hours instead, and that takes three things the
//! bundle store deliberately never holds:
//!
//! - the last assignment list - what to run, with each deployment's snapshot;
//! - the secret values those deployments bind, by revision;
//! - the capabilities this node last held, which a verifier in its degraded
//!   mode may still accept for a bounded time after they expire.
//!
//! All three are written here, sealed, on every successful sync
//! ([`OfflineCache::record_sync`]) and after every capability renewal
//! ([`OfflineCache::record_capabilities`]), and read back only when the
//! control plane cannot be reached at startup ([`OfflineCache::load`]).
//!
//! # How it is sealed
//!
//! One file, `<data_dir>/offline-cache/state.bin`, AES-256-GCM under a key
//! derived with HKDF from this node's own private key
//! ([`deis_identity::MachineIdentity::derive_local_key`]). The key is never
//! stored: whoever can read this file and not the node's key learns nothing,
//! and a changed byte fails the tag rather than being believed. The node id
//! and the format are bound in as associated data, so a file copied from
//! another node - or from a different format - does not open either. A key
//! rotation makes the file unreadable, which is the right direction: it only
//! ever holds what the node can fetch again.
//!
//! The file is written to a temporary name with mode 0600, synced, renamed
//! over the old one and the directory synced, so a crash leaves either the
//! previous state or the new one and never half of each.
//!
//! # How long it is believed
//!
//! A state written more than [`MAX_AGE_SECS`] (24 hours) ago is refused
//! whole: past that no verifier accepts the capabilities in it, and serving
//! a day-old list with no way to hear about a deletion is the outage budget
//! T8 draws. Capabilities past their own grace window are dropped when they
//! are taken back ([`super::capability_renewer::adopt`]).

use std::collections::{BTreeMap, HashMap};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use aes_gcm::aead::{Aead, AeadCore, KeyInit, OsRng, Payload};
use aes_gcm::{Aes256Gcm, Key, Nonce};
use serde::{Deserialize, Serialize};
use tracing::{debug, warn};

use super::capability_renewer::{self, HeldCapability};

/// The file under the cache directory.
const STATE_FILE: &str = "state.bin";

/// What the file starts with, and what the associated data names.
const MAGIC: &[u8; 8] = b"DEISOC01";

/// The HKDF purpose the sealing key is derived under.
pub const KEY_PURPOSE: &[u8] = b"deislet/offline-cache/v1";

/// How old a state may be and still be read: 24 hours, the T8 outage budget.
pub const MAX_AGE_SECS: u64 = 24 * 60 * 60;

const NONCE_LEN: usize = 12;

/// Secret values by deployment id, then by revision.
pub type SecretValues = HashMap<String, HashMap<String, String>>;

/// Everything the file holds, before it is sealed.
#[derive(Clone, Default, Serialize, Deserialize)]
pub struct OfflineState {
    /// Unix seconds when this was written.
    pub written_at: u64,
    /// The assignment list exactly as the control plane served it.
    pub assignments_json: String,
    /// Secret values of the deployments on that list.
    pub secrets: BTreeMap<String, BTreeMap<String, String>>,
    /// The capabilities this node held.
    pub capabilities: Vec<HeldCapability>,
}

impl std::fmt::Debug for OfflineState {
    /// Never a value: this is the one struct in the node that holds secrets
    /// and credentials side by side.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OfflineState")
            .field("written_at", &self.written_at)
            .field("deployments_with_secrets", &self.secrets.len())
            .field("capabilities", &self.capabilities.len())
            .finish_non_exhaustive()
    }
}

/// Why a state could not be read back.
#[derive(Debug, thiserror::Error)]
pub enum OfflineCacheError {
    #[error("the offline cache at {path} could not be read: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error(
        "the offline cache at {0} does not open with this node's key: it was changed, written by \
         another node, or sealed under a key this node has since rotated"
    )]
    Unsealed(PathBuf),
    #[error("the offline cache at {path} is not in a shape this build reads: {reason}")]
    Malformed { path: PathBuf, reason: String },
    #[error(
        "the offline cache at {path} was written {age_secs}s ago, past the {max_secs}s a node \
         may run on it"
    )]
    Stale {
        path: PathBuf,
        age_secs: u64,
        max_secs: u64,
    },
}

/// The sealed file, and the last state written to it.
pub struct OfflineCache {
    dir: PathBuf,
    key: [u8; 32],
    node_id: String,
    /// What was last written, so a capability renewal can rewrite the file
    /// without the assignment list it did not see.
    last: Mutex<OfflineState>,
}

impl std::fmt::Debug for OfflineCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OfflineCache")
            .field("dir", &self.dir)
            .field("node_id", &self.node_id)
            .finish_non_exhaustive()
    }
}

impl OfflineCache {
    /// A cache under `dir`, sealed with `key`, for the node named `node_id`.
    ///
    /// Nothing is read or written until asked.
    pub fn new(dir: impl Into<PathBuf>, key: [u8; 32], node_id: impl Into<String>) -> Self {
        Self {
            dir: dir.into(),
            key,
            node_id: node_id.into(),
            last: Mutex::new(OfflineState::default()),
        }
    }

    /// The cache for a node's identity, under `<data_dir>/offline-cache`.
    pub fn for_identity(
        data_dir: &Path,
        identity: &deis_identity::MachineIdentity,
        node_id: impl Into<String>,
    ) -> Self {
        Self::new(
            data_dir.join("offline-cache"),
            identity.derive_local_key(KEY_PURPOSE),
            node_id,
        )
    }

    pub fn path(&self) -> PathBuf {
        self.dir.join(STATE_FILE)
    }

    fn last(&self) -> std::sync::MutexGuard<'_, OfflineState> {
        self.last
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Write down a successful sync: the list as served, the secret values
    /// of the deployments on it, and the capabilities held right now.
    pub fn record_sync(&self, assignments_json: &str, secrets: SecretValues) {
        let state = {
            let mut last = self.last();
            last.assignments_json = assignments_json.to_string();
            last.secrets = secrets
                .into_iter()
                .map(|(deployment, values)| (deployment, values.into_iter().collect()))
                .collect();
            last.capabilities = capability_renewer::held();
            last.written_at = unix_now();
            last.clone()
        };
        self.persist(&state);
    }

    /// Rewrite the file with the capabilities held now, keeping the list and
    /// the secrets from the last sync. Does nothing before the first sync:
    /// capabilities alone are not something a node can restart on.
    pub fn record_capabilities(&self) {
        let state = {
            let mut last = self.last();
            if last.written_at == 0 {
                return;
            }
            last.capabilities = capability_renewer::held();
            last.clone()
        };
        self.persist(&state);
    }

    fn persist(&self, state: &OfflineState) {
        match self.write(state) {
            Ok(()) => debug!(
                path = %self.path().display(),
                "Sealed this node's offline cache"
            ),
            Err(error) => warn!(
                path = %self.path().display(),
                error = %error,
                "Could not write this node's offline cache; it keeps serving, but a restart while \
                 the control plane is unreachable would come back without what the last sync \
                 handed it"
            ),
        }
    }

    fn aad(&self) -> Vec<u8> {
        let mut aad = MAGIC.to_vec();
        aad.extend_from_slice(self.node_id.as_bytes());
        aad
    }

    /// Seal `state` and replace the file with it, atomically.
    pub fn write(&self, state: &OfflineState) -> std::io::Result<()> {
        let plaintext = serde_json::to_vec(state).map_err(std::io::Error::other)?;
        let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(&self.key));
        let nonce = Aes256Gcm::generate_nonce(&mut OsRng);
        let sealed = cipher
            .encrypt(
                &nonce,
                Payload {
                    msg: &plaintext,
                    aad: &self.aad(),
                },
            )
            .map_err(|_| std::io::Error::other("sealing the offline cache failed"))?;

        create_private_dir(&self.dir)?;
        let path = self.path();
        let temporary = self.dir.join(format!("{STATE_FILE}.tmp"));
        {
            let mut file = private_file(&temporary)?;
            file.write_all(MAGIC)?;
            file.write_all(&nonce)?;
            file.write_all(&sealed)?;
            file.sync_all()?;
        }
        std::fs::rename(&temporary, &path)?;
        // The rename is durable only once the directory entry is.
        std::fs::File::open(&self.dir)?.sync_all()?;
        Ok(())
    }

    /// Read the state back, if there is one this node may run on.
    ///
    /// `Ok(None)` is no file at all. Every other failure - a changed byte,
    /// another node's file, a rotated key, a state past [`MAX_AGE_SECS`] - is
    /// an error, and the caller runs without it.
    pub fn load(&self) -> Result<Option<OfflineState>, OfflineCacheError> {
        self.load_at(unix_now())
    }

    fn load_at(&self, now: u64) -> Result<Option<OfflineState>, OfflineCacheError> {
        let path = self.path();
        let bytes = match std::fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(source) => return Err(OfflineCacheError::Io { path, source }),
        };
        let Some(rest) = bytes.strip_prefix(MAGIC.as_slice()) else {
            return Err(OfflineCacheError::Malformed {
                path,
                reason: "it does not start with this format's marker".to_string(),
            });
        };
        if rest.len() < NONCE_LEN {
            return Err(OfflineCacheError::Malformed {
                path,
                reason: "it is shorter than its own header".to_string(),
            });
        }
        let (nonce, sealed) = rest.split_at(NONCE_LEN);
        let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(&self.key));
        let plaintext = cipher
            .decrypt(
                Nonce::from_slice(nonce),
                Payload {
                    msg: sealed,
                    aad: &self.aad(),
                },
            )
            .map_err(|_| OfflineCacheError::Unsealed(path.clone()))?;
        let state: OfflineState =
            serde_json::from_slice(&plaintext).map_err(|error| OfflineCacheError::Malformed {
                path: path.clone(),
                reason: error.to_string(),
            })?;
        let age_secs = now.saturating_sub(state.written_at);
        if age_secs > MAX_AGE_SECS {
            return Err(OfflineCacheError::Stale {
                path,
                age_secs,
                max_secs: MAX_AGE_SECS,
            });
        }
        *self.last() = state.clone();
        Ok(Some(state))
    }
}

fn create_private_dir(dir: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)?;
        // An existing directory keeps whatever mode it had; this one holds
        // secrets and is the node's alone.
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
    }
    #[cfg(not(unix))]
    {
        std::fs::create_dir_all(dir)
    }
}

fn private_file(path: &Path) -> std::io::Result<std::fs::File> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options.open(path)?;
    #[cfg(unix)]
    {
        // `mode` applies only when the file is created; a leftover temporary
        // from a crash keeps its old mode otherwise.
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(file)
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cache(dir: &Path, key: u8, node: &str) -> OfflineCache {
        OfflineCache::new(dir.join("offline-cache"), [key; 32], node)
    }

    fn state(written_at: u64) -> OfflineState {
        OfflineState {
            written_at,
            assignments_json: r#"[{"app_id":"demo"}]"#.to_string(),
            secrets: BTreeMap::from([(
                "d1".to_string(),
                BTreeMap::from([("r1".to_string(), "sealed-secret-sentinel".to_string())]),
            )]),
            capabilities: vec![HeldCapability {
                tenant: "demo.production".to_string(),
                audience: "store".to_string(),
                token: "v1.capability-sentinel".to_string(),
                exp: written_at + 900,
            }],
        }
    }

    #[test]
    fn a_written_state_reads_back_and_is_sealed_on_disk() {
        let dir = tempfile::tempdir().unwrap();
        let cache = cache(dir.path(), 1, "node-a");
        cache.write(&state(unix_now())).unwrap();

        let raw = std::fs::read(cache.path()).unwrap();
        let text = String::from_utf8_lossy(&raw);
        assert!(
            !text.contains("sealed-secret-sentinel"),
            "a secret is on disk in the clear"
        );
        assert!(
            !text.contains("capability-sentinel"),
            "a capability is on disk in the clear"
        );

        let back = cache.load().unwrap().expect("a state");
        assert_eq!(back.secrets["d1"]["r1"], "sealed-secret-sentinel");
        assert_eq!(back.capabilities[0].token, "v1.capability-sentinel");
        assert_eq!(back.assignments_json, r#"[{"app_id":"demo"}]"#);
    }

    #[cfg(unix)]
    #[test]
    fn the_file_and_its_directory_are_the_nodes_alone() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let cache = cache(dir.path(), 1, "node-a");
        cache.write(&state(unix_now())).unwrap();
        let mode = |path: &Path| std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&cache.path()), 0o600);
        assert_eq!(mode(&dir.path().join("offline-cache")), 0o700);
        assert!(
            !dir.path().join("offline-cache/state.bin.tmp").exists(),
            "the temporary was left behind"
        );
    }

    #[test]
    fn no_file_is_no_state_rather_than_an_error() {
        let dir = tempfile::tempdir().unwrap();
        assert!(cache(dir.path(), 1, "node-a").load().unwrap().is_none());
    }

    /// Any changed byte - header, nonce or body - fails the tag.
    #[test]
    fn a_changed_byte_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let cache = cache(dir.path(), 1, "node-a");
        cache.write(&state(unix_now())).unwrap();
        let original = std::fs::read(cache.path()).unwrap();
        for at in [MAGIC.len(), MAGIC.len() + NONCE_LEN, original.len() - 1] {
            let mut tampered = original.clone();
            tampered[at] ^= 0x01;
            std::fs::write(cache.path(), &tampered).unwrap();
            assert!(
                matches!(cache.load(), Err(OfflineCacheError::Unsealed(_))),
                "a flip at byte {at} was believed"
            );
        }
    }

    /// Another node's file, or this node's after a key rotation, does not
    /// open.
    #[test]
    fn another_nodes_file_or_key_does_not_open_it() {
        let dir = tempfile::tempdir().unwrap();
        cache(dir.path(), 1, "node-a")
            .write(&state(unix_now()))
            .unwrap();
        assert!(matches!(
            cache(dir.path(), 1, "node-b").load(),
            Err(OfflineCacheError::Unsealed(_))
        ));
        assert!(matches!(
            cache(dir.path(), 2, "node-a").load(),
            Err(OfflineCacheError::Unsealed(_))
        ));
    }

    #[test]
    fn a_state_older_than_a_day_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let cache = cache(dir.path(), 1, "node-a");
        let now = unix_now();
        cache.write(&state(now - MAX_AGE_SECS - 1)).unwrap();
        assert!(matches!(
            cache.load_at(now),
            Err(OfflineCacheError::Stale { .. })
        ));
        cache.write(&state(now - MAX_AGE_SECS + 60)).unwrap();
        assert!(cache.load_at(now).unwrap().is_some());
    }

    /// A renewal rewrites the capabilities and keeps what the last sync
    /// wrote; before any sync it writes nothing.
    #[test]
    fn a_renewal_before_any_sync_writes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let cache = cache(dir.path(), 1, "node-a");
        cache.record_capabilities();
        assert!(!cache.path().exists());

        cache.record_sync(
            "[]",
            HashMap::from([(
                "d1".to_string(),
                HashMap::from([("r1".to_string(), "v".to_string())]),
            )]),
        );
        cache.record_capabilities();
        let back = cache.load().unwrap().unwrap();
        assert_eq!(back.assignments_json, "[]");
        assert_eq!(back.secrets["d1"]["r1"], "v");
    }

    #[test]
    fn debug_output_carries_no_value() {
        let rendered = format!("{:?}", state(1));
        assert!(!rendered.contains("sentinel"), "{rendered}");
    }
}
