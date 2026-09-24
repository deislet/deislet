//! The one-time bootstrap credential file: `<data_dir>/bootstrap.token`.
//!
//! A first-run control plane with no credential anywhere would otherwise be
//! a brick - every call to the human API refused, and no way in to create a
//! way in. So the first start mints a bootstrap token (see
//! [`crate::identity::store::IdentityStore::ensure_bootstrap_token`]) and
//! writes it to this file. Whoever can read the file holds the operator's
//! identity, which is why the file is written `0600` and created exclusively:
//! a second start never overwrites a file that is already there, and the
//! gate deletes the file the moment the credential has done its one job.
//!
//! Nothing but the path and the token's public id is ever logged; the secret
//! exists in the database only as a SHA-256 and here only until the file is
//! deleted.

use std::io;
use std::path::{Path, PathBuf};

/// Where the one-time bootstrap credential lives for one control plane.
#[derive(Debug, Clone)]
pub struct BootstrapFile {
    path: PathBuf,
}

impl BootstrapFile {
    /// The file at `<data_dir>/bootstrap.token`.
    pub fn in_data_dir(data_dir: &Path) -> Self {
        Self {
            path: data_dir.join("bootstrap.token"),
        }
    }

    /// Where the file is, for the log line that tells an operator where to
    /// log in from.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Write the secret to the file, creating it exclusively with `0600`
    /// permissions.
    ///
    /// Exclusive creation is the point: a restart while an unused bootstrap
    /// credential is still live mints nothing (the store refuses a second
    /// root), and if a start ever did try to write again, overwriting an
    /// existing file would silently hand the platform a new root that
    /// whatever process once read the old file no longer knows about. The
    /// data directory itself is created if missing, so a first start with a
    /// fresh data directory does not have to be preceded by a `mkdir`.
    pub fn write(&self, secret: &str) -> io::Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        write_exclusive(&self.path, secret.as_bytes())
    }

    /// Delete the file after its one successful use. A file that is already
    /// gone is exactly what was asked for, not an error: the gate and the
    /// operator cleaning up between runs are racing for the same `rm`.
    pub fn delete(&self) -> io::Result<()> {
        match std::fs::remove_file(&self.path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e),
        }
    }
}

#[cfg(unix)]
fn write_exclusive(path: &Path, bytes: &[u8]) -> io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;

    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(bytes)
}

#[cfg(not(unix))]
fn write_exclusive(path: &Path, bytes: &[u8]) -> io::Result<()> {
    use std::io::Write;

    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)?;
    file.write_all(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_file_is_written_once_with_owner_only_permissions_and_then_gone() {
        let dir = tempfile::tempdir().unwrap();
        let file = BootstrapFile::in_data_dir(dir.path());

        file.write("deis_pat_0011223344556677_deadbeef").unwrap();

        let meta = std::fs::metadata(file.path()).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                meta.permissions().mode() & 0o777,
                0o600,
                "the credential file must be readable by its owner only"
            );
        }
        assert_eq!(
            std::fs::read_to_string(file.path()).unwrap(),
            "deis_pat_0011223344556677_deadbeef"
        );

        // A second write refuses rather than overwriting: one way in at a
        // time, and nobody's file silently replaced underneath them.
        assert!(
            file.write("another-secret").is_err(),
            "an existing bootstrap file must never be overwritten"
        );

        file.delete().unwrap();
        assert!(!file.path().exists(), "the file is gone after its use");
        // Deleting twice is fine: the second is a no-op, not an error.
        file.delete().unwrap();
    }

    /// The data directory itself need not exist: a first start points the
    /// control plane at a fresh directory and expects this to work.
    #[test]
    fn a_missing_data_directory_is_created_on_the_way_to_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let file = BootstrapFile::in_data_dir(&dir.path().join("state").join("control"));

        file.write("deis_pat_0011223344556677_deadbeef").unwrap();
        assert!(file.path().exists());
    }
}
