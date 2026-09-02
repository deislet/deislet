//! Everything automatic certificates keep on disk, and the layout that keeps
//! two authorities apart.
//!
//! ```text
//! <storage_dir>/<authority>/account.key           the account key, 0600
//! <storage_dir>/<authority>/account.url           the account this key registered
//! <storage_dir>/<authority>/certificates/<name>/fullchain.pem
//! <storage_dir>/<authority>/certificates/<name>/privkey.pem
//! <storage_dir>/<authority>/challenges/<token>    published answers, transient
//! ```
//!
//! The `<authority>` level is the host of the directory URL, and it is there
//! to stop a specific, expensive mistake. An operator tries staging first,
//! gets certificates, then edits `directory_url` to production. With one flat
//! layout the process would restart, find ninety-day certificates that are
//! nowhere near expiry, decide there was nothing to do, and keep serving
//! certificates no browser trusts - and the account URL from staging would be
//! presented to production, which does not know it. Splitting by authority
//! makes the switch obtain everything again, which is what the operator meant.
//!
//! The `challenges` directory looks redundant next to the in-memory map in
//! [`super::challenge`], and is not. During a zero-downtime upgrade two
//! processes are alive at once and the outgoing one still owns the listening
//! sockets, so the authority's fetch can land on the process that did not
//! create the order. Both read this directory, so either can answer. Each
//! answer file records the name it was published for on its first line, so
//! the process answering can refuse a fetch that arrives under another host.
//!
//! # Two processes on one store
//!
//! Two processes *reading* one store is the upgrade case above and is
//! supported. Two processes *writing* one store - two proxies pointed at the
//! same `storage_dir`, or the outgoing and incoming halves of an upgrade
//! both running a renewal pass - is held to one writer at a time by two
//! advisory locks, both released by the kernel if the holder dies:
//!
//! * `renewal.lock` at the authority level is taken for the whole of a
//!   renewal pass, without waiting. A pass that finds it held skips, says so,
//!   and re-reads the store later: whatever the other writer ordered is then
//!   found on disk and not ordered again.
//! * `.lock` beside each certificate pair is held while the key and the chain
//!   are renamed into place, and taken shared by every reader of the pair, so
//!   a reader never sees one writer's key beside another writer's chain.
//!
//! A key beside a chain it does not match is still possible after a crash
//! between the two renames; it is caught when the pair is read
//! ([`super::stored_certificate`] checks that the key matches) and the pair
//! is ordered again.

use std::fs::{self, File};
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime};

use nix::fcntl::{Flock, FlockArg};
use tracing::{debug, warn};

use super::account::AccountKey;
use super::error::AcmeError;
use super::names::is_certifiable_name;
use crate::config::CertificateConfig;

const ACCOUNT_KEY_FILE: &str = "account.key";
const ACCOUNT_URL_FILE: &str = "account.url";
const CERTIFICATES_DIR: &str = "certificates";
const CHALLENGES_DIR: &str = "challenges";
const CHAIN_FILE: &str = "fullchain.pem";
const KEY_FILE: &str = "privkey.pem";
/// Held while a pair is written, shared while it is read. Beside the pair
/// rather than inside it, and dot-prefixed so it never reads as a name.
const PAIR_LOCK_FILE: &str = ".lock";
/// Held by the one process running a renewal pass on this authority's store.
const RENEWAL_LOCK_FILE: &str = "renewal.lock";

/// A lock a renewal pass holds while it runs. Dropping it releases the
/// store to the next writer; so does the process dying.
pub struct RenewalLock {
    _file: Flock<File>,
}

/// A published `http-01` answer as the store records it: the name the
/// order was for, and the value the authority expects to read back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublishedChallenge {
    pub name: String,
    pub key_authorization: String,
}

/// The files one authority's certificates live in.
pub struct AcmeStore {
    root: PathBuf,
}

impl AcmeStore {
    /// The store for one authority, under the configured storage directory.
    pub fn new(storage_dir: &Path, directory_url: &str) -> Self {
        Self {
            root: storage_dir.join(authority_key(directory_url)),
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Create the directories, with permissions that keep the account key
    /// and the certificate keys to this user. A key readable by everyone on
    /// the machine is the whole certificate given away.
    pub fn prepare(&self) -> Result<(), AcmeError> {
        for dir in [
            self.root.clone(),
            self.root.join(CERTIFICATES_DIR),
            self.root.join(CHALLENGES_DIR),
        ] {
            fs::create_dir_all(&dir)
                .map_err(|e| AcmeError::Storage(format!("creating {}: {e}", dir.display())))?;
            if let Err(e) = fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)) {
                warn!(
                    dir = %dir.display(),
                    error = %e,
                    "could not restrict the certificate store to this user; the private keys \
                     under it may be readable by other accounts on this machine"
                );
            }
        }
        Ok(())
    }

    // ------------------------------------------------------------------
    // The account
    // ------------------------------------------------------------------

    pub fn account_key_path(&self) -> PathBuf {
        self.root.join(ACCOUNT_KEY_FILE)
    }

    /// The stored account key, or a new one written out on the spot.
    ///
    /// Returns whether it had to create one, because that is worth a
    /// different log line: creating a key on every start would be a fresh
    /// registration on every start, and the authority's registration limit
    /// would eventually refuse them all.
    pub fn load_or_create_account_key(&self) -> Result<(AccountKey, bool), AcmeError> {
        let path = self.account_key_path();
        if path.exists() {
            let pem = fs::read(&path)
                .map_err(|e| AcmeError::Storage(format!("reading {}: {e}", path.display())))?;
            return Ok((AccountKey::from_pem(&pem)?, false));
        }
        let key = AccountKey::generate()?;
        write_private(&path, &key.to_pem()?)?;
        Ok((key, true))
    }

    /// The account URL this key is registered at, when it has been.
    pub fn load_account_url(&self) -> Option<String> {
        let text = fs::read_to_string(self.root.join(ACCOUNT_URL_FILE)).ok()?;
        let text = text.trim().to_string();
        (!text.is_empty()).then_some(text)
    }

    pub fn save_account_url(&self, url: &str) -> Result<(), AcmeError> {
        write_atomic(&self.root.join(ACCOUNT_URL_FILE), url.as_bytes(), 0o600)
    }

    // ------------------------------------------------------------------
    // Certificates
    // ------------------------------------------------------------------

    pub fn certificate_dir(&self, name: &str) -> PathBuf {
        self.root.join(CERTIFICATES_DIR).join(name)
    }

    /// Where the chain and the key for one name are, whether or not they
    /// exist yet.
    pub fn certificate_config(&self, name: &str) -> CertificateConfig {
        let dir = self.certificate_dir(name);
        CertificateConfig {
            cert_file: dir.join(CHAIN_FILE).to_string_lossy().into_owned(),
            key_file: dir.join(KEY_FILE).to_string_lossy().into_owned(),
        }
    }

    /// Write a freshly issued certificate.
    ///
    /// Both files are written to a temporary name and renamed into place, so
    /// the route table - which is rebuilt straight afterwards, and which any
    /// restart also reads - never sees half a PEM file. The key is written
    /// first: a chain with no key beside it is refused when it is loaded and
    /// simply reissued, while a key with no chain would be a private key
    /// lying around for a certificate that does not exist.
    ///
    /// The pair lock is held from before the first rename to after the
    /// second, and [`read_certificate_pair`] takes it shared, so a reader in
    /// any process sees the old pair or the new pair and never a key from one
    /// beside a chain from the other.
    pub fn save_certificate(
        &self,
        name: &str,
        chain_pem: &[u8],
        key_pem: &[u8],
    ) -> Result<(), AcmeError> {
        self.save_certificate_observed(name, chain_pem, key_pem, || {})
    }

    /// [`Self::save_certificate`] with a hook called between the two
    /// renames, while the pair lock is held. The hook is how a test proves
    /// the lock really spans both files; production passes a no-op.
    fn save_certificate_observed(
        &self,
        name: &str,
        chain_pem: &[u8],
        key_pem: &[u8],
        between_renames: impl FnOnce(),
    ) -> Result<(), AcmeError> {
        let dir = self.certificate_dir(name);
        fs::create_dir_all(&dir)
            .map_err(|e| AcmeError::Storage(format!("creating {}: {e}", dir.display())))?;
        let _ = fs::set_permissions(&dir, fs::Permissions::from_mode(0o700));
        let lock = lock_pair(&dir, FlockArg::LockExclusive)?;
        write_atomic(&dir.join(KEY_FILE), key_pem, 0o600)?;
        between_renames();
        write_atomic(&dir.join(CHAIN_FILE), chain_pem, 0o644)?;
        drop(lock);
        Ok(())
    }

    /// Whether a certificate for `name` could be written right now.
    ///
    /// Asked before an order is placed, because the order is the expensive
    /// half: the authority counts a certificate against the domain's weekly
    /// allowance the moment it signs it, and a store that cannot take the
    /// result turns every retry into one more certificate spent for nothing.
    /// The probe writes and removes a file in the directory the pair would
    /// land in, and checks that neither target is something a rename cannot
    /// replace, which is what a directory left in a file's place is.
    pub fn probe_writable(&self, name: &str) -> Result<(), AcmeError> {
        let dir = self.certificate_dir(name);
        fs::create_dir_all(&dir)
            .map_err(|e| AcmeError::Storage(format!("creating {}: {e}", dir.display())))?;
        for target in [dir.join(KEY_FILE), dir.join(CHAIN_FILE)] {
            if target.exists() && !target.is_file() {
                return Err(AcmeError::Storage(format!(
                    "{} exists and is not a file, so a certificate cannot be renamed over it",
                    target.display()
                )));
            }
        }
        let probe = temporary_path(&dir.join(".probe"));
        fs::write(&probe, b"probe")
            .map_err(|e| AcmeError::Storage(format!("writing {}: {e}", probe.display())))?;
        fs::remove_file(&probe)
            .map_err(|e| AcmeError::Storage(format!("removing {}: {e}", probe.display())))?;
        Ok(())
    }

    /// Take the store's renewal lock, or learn that another process holds it.
    ///
    /// `Ok(None)` is the other-process answer and is not an error: the caller
    /// skips its pass and re-reads the store later. Anything else that stops
    /// the lock being taken is a storage fault and is returned as one.
    pub fn try_renewal_lock(&self) -> Result<Option<RenewalLock>, AcmeError> {
        let path = self.root.join(RENEWAL_LOCK_FILE);
        let file = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(&path)
            .map_err(|e| AcmeError::Storage(format!("opening {}: {e}", path.display())))?;
        match Flock::lock(file, FlockArg::LockExclusiveNonblock) {
            Ok(file) => Ok(Some(RenewalLock { _file: file })),
            Err((_, nix::errno::Errno::EWOULDBLOCK)) => Ok(None),
            Err((_, e)) => Err(AcmeError::Storage(format!(
                "locking {}: {e}",
                path.display()
            ))),
        }
    }

    /// Every name this store holds a complete pair of files for.
    ///
    /// Reading the directory rather than a manifest is deliberate: the
    /// directory is what the route table will load and what a restart will
    /// find, so a manifest could disagree with it, and the disagreement
    /// would be invisible until a handshake failed.
    pub fn stored_certificates(&self) -> Vec<(String, CertificateConfig)> {
        let dir = self.root.join(CERTIFICATES_DIR);
        let entries = match fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Vec::new(),
            Err(e) => {
                warn!(
                    dir = %dir.display(),
                    error = %e,
                    "the automatic certificate store cannot be read; every name it holds will be \
                     ordered again, which spends the authority's rate limit"
                );
                return Vec::new();
            }
        };

        let mut found = Vec::new();
        for entry in entries.flatten() {
            let Some(name) = entry.file_name().to_str().map(str::to_string) else {
                continue;
            };
            // A directory name that is not a host name cannot have been
            // written by this code, and joining it into a path is how a
            // store somebody else wrote into becomes a file read.
            if !is_certifiable_name(&name) {
                debug!(entry = %name, "ignoring an entry in the certificate store that is not a host name");
                continue;
            }
            let config = self.certificate_config(&name);
            if Path::new(&config.cert_file).is_file() && Path::new(&config.key_file).is_file() {
                found.push((name, config));
            }
        }
        found.sort_by(|a, b| a.0.cmp(&b.0));
        found
    }

    // ------------------------------------------------------------------
    // Challenge answers
    // ------------------------------------------------------------------

    fn challenge_path(&self, token: &str) -> PathBuf {
        self.root.join(CHALLENGES_DIR).join(token)
    }

    /// Publish an answer where any process serving these sockets can find it.
    ///
    /// The file carries the name first and the answer second, one per line.
    /// The name is what lets the process that answers refuse a fetch for the
    /// same token under a different host; a file with no name line is not
    /// one this code wrote and is not answered.
    pub fn publish_challenge(
        &self,
        name: &str,
        token: &str,
        key_authorization: &str,
    ) -> Result<(), AcmeError> {
        let dir = self.root.join(CHALLENGES_DIR);
        fs::create_dir_all(&dir)
            .map_err(|e| AcmeError::Storage(format!("creating {}: {e}", dir.display())))?;
        let contents = format!("{name}\n{key_authorization}");
        write_atomic(&self.challenge_path(token), contents.as_bytes(), 0o644)
    }

    /// The published answer for a token, if this token was published here.
    ///
    /// The caller has already checked the token against the character set a
    /// token may use; this is the second check, because the value reaching
    /// it comes from a request path and one missing check here is a file
    /// read chosen by whoever sent the request.
    pub fn published_challenge(&self, token: &str) -> Option<PublishedChallenge> {
        if !is_challenge_token(token) {
            return None;
        }
        let text = fs::read_to_string(self.challenge_path(token)).ok()?;
        let (name, key_authorization) = text.split_once('\n')?;
        let name = name.trim();
        let key_authorization = key_authorization.trim();
        if !is_certifiable_name(name) || key_authorization.is_empty() {
            debug!(
                token,
                "a file in the challenge directory does not have the shape this store writes and \
                 is not answered"
            );
            return None;
        }
        Some(PublishedChallenge {
            name: name.to_string(),
            key_authorization: key_authorization.to_string(),
        })
    }

    pub fn withdraw_challenge(&self, token: &str) {
        if !is_challenge_token(token) {
            return;
        }
        let path = self.challenge_path(token);
        if let Err(e) = fs::remove_file(&path)
            && e.kind() != std::io::ErrorKind::NotFound
        {
            debug!(path = %path.display(), error = %e, "a spent challenge answer could not be removed");
        }
    }

    /// Remove answers older than `age`.
    ///
    /// A process that was killed between publishing an answer and finishing
    /// the order leaves one behind. It is harmless - the token is random and
    /// the answer is public by construction - but a directory that only ever
    /// grows is a directory nobody can read, so each renewal pass sweeps it.
    pub fn sweep_challenges(&self, age: Duration) -> usize {
        let dir = self.root.join(CHALLENGES_DIR);
        let Ok(entries) = fs::read_dir(&dir) else {
            return 0;
        };
        let mut removed = 0;
        for entry in entries.flatten() {
            let stale = entry
                .metadata()
                .and_then(|meta| meta.modified())
                .map(|modified| {
                    SystemTime::now()
                        .duration_since(modified)
                        .map(|elapsed| elapsed > age)
                        .unwrap_or(false)
                })
                .unwrap_or(false);
            if stale && fs::remove_file(entry.path()).is_ok() {
                removed += 1;
            }
        }
        removed
    }
}

/// The directory level that keeps two authorities' material apart.
///
/// The host of the directory URL, reduced to characters that are safe as a
/// path component. Anything else becomes `-`, so a URL cannot name a
/// directory outside the store however it is written.
fn authority_key(directory_url: &str) -> String {
    let without_scheme = directory_url
        .strip_prefix("https://")
        .or_else(|| directory_url.strip_prefix("http://"))
        .unwrap_or(directory_url);
    let authority = without_scheme
        .split(['/', '?', '#'])
        .next()
        .unwrap_or(without_scheme);
    let cleaned: String = authority
        .chars()
        .map(|c| match c {
            'a'..='z' | '0'..='9' | '.' | '-' => c,
            'A'..='Z' => c.to_ascii_lowercase(),
            _ => '-',
        })
        .collect();
    // `.` and `..` are the two names that would put the store somewhere else
    // entirely, and a leading dot only hides it.
    let cleaned = cleaned.trim_matches('.').to_string();
    if cleaned.is_empty() {
        "unnamed-authority".to_string()
    } else {
        cleaned
    }
}

/// Whether a string can be a challenge token, which decides whether it may
/// be joined onto a path.
///
/// RFC 8555 section 8.3 says a token is base64url, so this is exactly that
/// character set. It is also the check that makes the challenge responder
/// safe: `..` and `/` are not in it, so no request path can walk out of the
/// challenge directory.
pub fn is_challenge_token(token: &str) -> bool {
    !token.is_empty()
        && token.len() <= 128
        && token
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// Write a file only this user can read.
fn write_private(path: &Path, contents: &[u8]) -> Result<(), AcmeError> {
    write_atomic(path, contents, 0o600)
}

/// Read a certificate pair as one unit.
///
/// Takes the pair lock shared for the duration of both reads, so a writer
/// in another process - which holds it exclusively across both of its
/// renames - can never be caught between them. Without the lock, the two
/// reads were two independent snapshots, and the key from one writer beside
/// the chain from another read as a pair that did not match. The lock file
/// is created if it is missing; a pair somebody placed by hand has none.
pub fn read_certificate_pair(config: &CertificateConfig) -> std::io::Result<(Vec<u8>, Vec<u8>)> {
    let chain = Path::new(&config.cert_file);
    let key = Path::new(&config.key_file);
    // Best effort: a directory that cannot take a lock file - read-only
    // media, say - still has readable files in it, and refusing to read
    // them would turn a missing lock into a missing certificate.
    let _lock = chain
        .parent()
        .and_then(|dir| lock_pair(dir, FlockArg::LockShared).ok());
    Ok((fs::read(chain)?, fs::read(key)?))
}

/// Take the pair lock in `dir`: exclusive for a writer, shared for a reader.
fn lock_pair(dir: &Path, how: FlockArg) -> Result<Flock<File>, AcmeError> {
    let path = dir.join(PAIR_LOCK_FILE);
    let file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(&path)
        .map_err(|e| AcmeError::Storage(format!("opening {}: {e}", path.display())))?;
    Flock::lock(file, how)
        .map_err(|(_, e)| AcmeError::Storage(format!("locking {}: {e}", path.display())))
}

/// Whether the pair lock in `dir` is currently held exclusively by someone.
///
/// For tests: the question "is a writer inside its two renames right now"
/// asked without waiting.
#[cfg(test)]
fn pair_is_locked_for_writing(dir: &Path) -> bool {
    lock_pair(dir, FlockArg::LockSharedNonblock).is_err()
}

/// A temporary name beside `path` that no other writer - in this process or
/// another one - is using at the same time.
///
/// The process id keeps two processes apart and the counter keeps two
/// writes in one process apart. A fixed `.<file>.tmp` was how a second
/// writer truncated the first writer's temporary file from under it, so the
/// first writer's rename then failed with a file-not-found that said nothing
/// about the other process.
fn temporary_path(path: &Path) -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    parent.join(format!(
        ".{}.{}.{}.tmp",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("acme"),
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ))
}

/// Write `contents` to `path` through a temporary file in the same directory.
///
/// The rename is what makes it atomic: a reader either sees the previous
/// file or the whole new one. Writing in place would let the route table -
/// or a restarting process - load a PEM file that stops in the middle of a
/// base64 block, and the certificate would be dropped with a parse error
/// that says nothing about a race.
fn write_atomic(path: &Path, contents: &[u8], mode: u32) -> Result<(), AcmeError> {
    let parent = path
        .parent()
        .ok_or_else(|| AcmeError::Storage(format!("{} has no parent directory", path.display())))?;
    fs::create_dir_all(parent)
        .map_err(|e| AcmeError::Storage(format!("creating {}: {e}", parent.display())))?;
    let temporary = temporary_path(path);

    let mut file = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(mode)
        .open(&temporary)
        .map_err(|e| AcmeError::Storage(format!("creating {}: {e}", temporary.display())))?;
    file.write_all(contents)
        .map_err(|e| AcmeError::Storage(format!("writing {}: {e}", temporary.display())))?;
    // Forced to disk before the rename: the point of the rename is that a
    // reader after a crash sees one whole file or the other, and that only
    // holds if the bytes are there before the name is.
    file.sync_all()
        .map_err(|e| AcmeError::Storage(format!("flushing {}: {e}", temporary.display())))?;
    drop(file);

    if let Err(e) = fs::rename(&temporary, path) {
        // Nothing else will ever rename this file; leaving it would be a
        // key on disk with no certificate, or a chain nobody serves.
        let _ = fs::remove_file(&temporary);
        return Err(AcmeError::Storage(format!(
            "renaming {} to {}: {e}",
            temporary.display(),
            path.display()
        )));
    }
    // `mode` is what the file is created with, but an existing file being
    // replaced keeps whatever it had; set it explicitly so a key written
    // once with a loose umask does not stay loose.
    let _ = fs::set_permissions(path, fs::Permissions::from_mode(mode));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store(dir: &Path, url: &str) -> AcmeStore {
        let store = AcmeStore::new(dir, url);
        store.prepare().unwrap();
        store
    }

    #[test]
    fn two_authorities_never_share_a_directory() {
        // The mistake this prevents: staging certificates being served, and
        // never renewed, after the operator switched to production. Their
        // ninety days of validity make every "is it due?" answer no.
        let temp = tempfile::tempdir().unwrap();
        let staging = AcmeStore::new(
            temp.path(),
            "https://acme-staging-v02.api.letsencrypt.org/directory",
        );
        let production = AcmeStore::new(
            temp.path(),
            "https://acme-v02.api.letsencrypt.org/directory",
        );
        assert_ne!(staging.root(), production.root());
        assert!(
            staging
                .root()
                .to_string_lossy()
                .contains("acme-staging-v02.api.letsencrypt.org")
        );
    }

    #[test]
    fn an_authority_url_cannot_name_a_directory_outside_the_store() {
        // The directory URL is configuration rather than a request, so this
        // is not defence against an attacker; it is defence against a typo
        // silently putting the account key and every certificate key
        // somewhere other than the directory the operator named.
        let temp = tempfile::tempdir().unwrap();
        for url in [
            "https://../../etc/directory",
            "https://..%2f..%2fetc/directory",
            "https://./directory",
            "https://.../directory",
            "not-a-url",
        ] {
            let store = AcmeStore::new(temp.path(), url);
            let root = store.root().to_path_buf();
            let below: Vec<_> = root
                .strip_prefix(temp.path())
                .unwrap_or(&root)
                .components()
                .collect();
            assert_eq!(
                below.len(),
                1,
                "{url} put the store at {}, which is not one directory under the storage \
                 directory",
                root.display()
            );
            assert!(
                below
                    .iter()
                    .all(|component| matches!(component, std::path::Component::Normal(_))),
                "{url} produced a path component that walks somewhere else: {}",
                root.display()
            );
        }
    }

    #[test]
    fn the_account_key_is_created_once_and_then_reused() {
        // Read this test as the rate limit it protects: Let's Encrypt allows
        // a small number of new registrations per IP address per three hours,
        // and a key regenerated on each start would spend them on restarts.
        let temp = tempfile::tempdir().unwrap();
        let store = store(temp.path(), "https://ca.test/dir");

        let (first, created) = store.load_or_create_account_key().unwrap();
        assert!(created, "the first start has no key to find");
        let (second, created) = store.load_or_create_account_key().unwrap();
        assert!(
            !created,
            "the second start must find the key the first wrote"
        );
        assert_eq!(first.thumbprint().unwrap(), second.thumbprint().unwrap());
    }

    #[test]
    fn the_account_key_is_not_readable_by_anyone_else() {
        let temp = tempfile::tempdir().unwrap();
        let store = store(temp.path(), "https://ca.test/dir");
        store.load_or_create_account_key().unwrap();
        let mode = fs::metadata(store.account_key_path())
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600, "the account key was written as {mode:o}");
    }

    #[test]
    fn a_saved_certificate_is_found_again_by_name() {
        let temp = tempfile::tempdir().unwrap();
        let store = store(temp.path(), "https://ca.test/dir");
        store
            .save_certificate("shop.example.com", b"-----BEGIN CERTIFICATE-----\n", b"key")
            .unwrap();

        let found = store.stored_certificates();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].0, "shop.example.com");
        assert_eq!(
            fs::read(&found[0].1.cert_file).unwrap(),
            b"-----BEGIN CERTIFICATE-----\n"
        );
        let mode = fs::metadata(&found[0].1.key_file)
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600, "the certificate key was written as {mode:o}");
    }

    #[test]
    fn a_half_written_pair_is_not_offered_to_the_route_table() {
        // A chain with no key beside it cannot be served, and reporting it
        // as stored would make the renewal pass believe the name was done.
        let temp = tempfile::tempdir().unwrap();
        let store = store(temp.path(), "https://ca.test/dir");
        let dir = store.certificate_dir("half.example.com");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(CHAIN_FILE), b"chain").unwrap();
        assert!(store.stored_certificates().is_empty());

        fs::write(dir.join(KEY_FILE), b"key").unwrap();
        assert_eq!(store.stored_certificates().len(), 1);
    }

    #[test]
    fn a_published_answer_is_readable_and_a_traversal_is_not() {
        let temp = tempfile::tempdir().unwrap();
        let store = store(temp.path(), "https://ca.test/dir");
        store
            .publish_challenge("shop.example.com", "tok-1_A", "tok-1_A.thumb")
            .unwrap();
        assert_eq!(
            store.published_challenge("tok-1_A"),
            Some(PublishedChallenge {
                name: "shop.example.com".to_string(),
                key_authorization: "tok-1_A.thumb".to_string(),
            })
        );
        assert_eq!(store.published_challenge("tok-2"), None);

        // The token arrives in a request path from anyone on the internet.
        // Without the character check these would be arbitrary file reads.
        fs::write(temp.path().join("secret"), "top secret").unwrap();
        for hostile in ["../../secret", "..", "/etc/passwd", "a/b"] {
            assert_eq!(
                store.published_challenge(hostile),
                None,
                "{hostile} was allowed to name a file"
            );
        }
    }

    #[test]
    fn a_file_planted_in_the_challenge_directory_is_not_an_answer() {
        // Anything under the challenge directory used to be served for any
        // token-shaped path, whoever wrote it. An answer this store writes
        // names the host it is for on its first line; a file without that
        // line was not written by this code and is not answered.
        let temp = tempfile::tempdir().unwrap();
        let store = store(temp.path(), "https://ca.test/dir");
        fs::write(store.challenge_path("planted"), "planted").unwrap();
        assert_eq!(store.published_challenge("planted"), None);
        fs::write(store.challenge_path("planted2"), "not a host name\nx").unwrap();
        assert_eq!(store.published_challenge("planted2"), None);
    }

    #[test]
    fn a_spent_answer_is_withdrawn_and_a_forgotten_one_is_swept() {
        let temp = tempfile::tempdir().unwrap();
        let store = store(temp.path(), "https://ca.test/dir");
        store
            .publish_challenge("shop.example.com", "tok-1", "answer")
            .unwrap();
        store.withdraw_challenge("tok-1");
        assert_eq!(store.published_challenge("tok-1"), None);

        store
            .publish_challenge("shop.example.com", "tok-2", "answer")
            .unwrap();
        assert_eq!(
            store.sweep_challenges(Duration::from_secs(3600)),
            0,
            "an answer published a moment ago is still in use"
        );
        assert_eq!(store.sweep_challenges(Duration::ZERO), 1);
        assert_eq!(store.published_challenge("tok-2"), None);
    }

    #[test]
    fn the_pair_lock_is_held_from_the_first_rename_to_the_second() {
        // Two files renamed one after the other are two moments a reader
        // can land between. The lock is what turns them into one moment for
        // every reader that takes it, in this process or another; if it were
        // released between the renames - or never taken - a reader would see
        // the new key beside the old chain, which is a pair that does not
        // match and used to be reordered for it.
        let temp = tempfile::tempdir().unwrap();
        let store = store(temp.path(), "https://ca.test/dir");
        let dir = store.certificate_dir("shop.example.com");
        let mut observed_locked = None;
        store
            .save_certificate_observed("shop.example.com", b"chain", b"key", || {
                observed_locked = Some(pair_is_locked_for_writing(&dir));
            })
            .unwrap();
        assert_eq!(
            observed_locked,
            Some(true),
            "between the key rename and the chain rename the pair must be locked against readers"
        );
        assert!(
            !pair_is_locked_for_writing(&dir),
            "once both files are in place the lock must be released"
        );
        let config = store.certificate_config("shop.example.com");
        assert_eq!(
            read_certificate_pair(&config).unwrap(),
            (b"chain".to_vec(), b"key".to_vec())
        );
    }

    #[test]
    fn one_renewal_pass_at_a_time_per_store() {
        // The second writer is another process in real life - a second
        // proxy on the same storage directory, or the outgoing half of an
        // upgrade still in its pass. Both would otherwise decide the same
        // name was due and order it twice.
        let temp = tempfile::tempdir().unwrap();
        let store = store(temp.path(), "https://ca.test/dir");
        let other = AcmeStore::new(temp.path(), "https://ca.test/dir");

        let held = store.try_renewal_lock().unwrap();
        assert!(held.is_some(), "the first writer takes the lock");
        assert!(
            other.try_renewal_lock().unwrap().is_none(),
            "a second writer must be told the store is being renewed, not given the lock"
        );
        drop(held);
        assert!(
            other.try_renewal_lock().unwrap().is_some(),
            "the lock is free again once the pass that held it is over"
        );
    }

    #[test]
    fn two_writes_never_share_a_temporary_name() {
        // With one fixed name, a second writer truncated the first writer's
        // temporary file and the first writer's rename failed with a
        // not-found that pointed nowhere near the cause.
        let path = Path::new("/store/certificates/shop.example.com/privkey.pem");
        let first = temporary_path(path);
        let second = temporary_path(path);
        assert_ne!(first, second);
        let pid = std::process::id().to_string();
        for temporary in [&first, &second] {
            let name = temporary.file_name().unwrap().to_string_lossy();
            assert!(name.starts_with(".privkey.pem."), "{name}");
            assert!(name.contains(&pid), "{name} does not carry the process id");
            assert!(name.ends_with(".tmp"), "{name}");
        }
    }

    #[test]
    fn a_target_a_rename_cannot_replace_is_found_before_anything_is_ordered() {
        // A directory where a file should be is the one shape a rename cannot
        // recover from, and the probe has to say so before the authority is
        // asked for anything.
        let temp = tempfile::tempdir().unwrap();
        let store = store(temp.path(), "https://ca.test/dir");
        assert!(store.probe_writable("shop.example.com").is_ok());
        let dir = store.certificate_dir("shop.example.com");
        fs::create_dir_all(dir.join(KEY_FILE)).unwrap();
        let error = store
            .probe_writable("shop.example.com")
            .unwrap_err()
            .to_string();
        assert!(error.contains("privkey.pem"), "{error}");
        assert!(error.contains("not a file"), "{error}");
        // And a probe leaves nothing behind.
        fs::remove_dir(dir.join(KEY_FILE)).unwrap();
        store.probe_writable("shop.example.com").unwrap();
        let left: Vec<_> = fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name.starts_with(".probe"))
            .collect();
        assert!(left.is_empty(), "the probe left {left:?} behind");
    }

    #[test]
    fn the_token_character_set_is_the_one_the_rfc_specifies() {
        assert!(is_challenge_token("A-Za-z0-9_abcXYZ019"));
        assert!(!is_challenge_token(""));
        assert!(!is_challenge_token("has.a.dot"));
        assert!(!is_challenge_token("has/slash"));
        assert!(!is_challenge_token(".."));
        assert!(!is_challenge_token(&"a".repeat(129)));
    }
}
