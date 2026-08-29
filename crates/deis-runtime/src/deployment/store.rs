//! Where a node keeps the bundles it has been given.
//!
//! Two things live here. The bundles themselves, so that a redelivery of the
//! same version costs no download and a node that comes up without a control
//! plane still has something to serve. And a small record of what was
//! installed, so that same node knows which version each app was on.
//!
//! What never lives here is a secret. The record holds app, version,
//! environment and digest - all of which the control plane already logs - and
//! the bundle itself carries only secret *names*. Decrypted values exist in
//! this process's memory and nowhere else.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use tracing::{debug, warn};

/// Name of the file recording what this node has installed.
const RECORD_FILE: &str = "installed.json";

/// Directory partial downloads are written to before they are trusted.
const TEMP_DIR: &str = "incoming";

/// One line of the installed record.
///
/// Deliberately no `env`: see the module docs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstalledRecord {
    pub app_id: String,
    pub version_id: String,
    pub environment: String,
    pub bundle_sha: String,
}

/// Why an identifier cannot be used as a path segment.
///
/// The control plane is trusted to be honest, but it is still remote input,
/// and these ids are concatenated into a filesystem path. An id containing a
/// separator or a `..` would let a deployment write outside the bundle
/// directory entirely.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvalidIdentifier {
    pub field: &'static str,
    pub value: String,
}

impl std::fmt::Display for InvalidIdentifier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} {:?} is not a usable name: it must be one or more of letters, digits, '.', '_' \
             or '-', and not \".\" or \"..\". It is used as a directory name, so anything else \
             could write outside the bundle directory",
            self.field, self.value
        )
    }
}

impl std::error::Error for InvalidIdentifier {}

/// Accept only what is safe to paste into a path.
pub fn check_identifier(field: &'static str, value: &str) -> Result<(), InvalidIdentifier> {
    let usable = !value.is_empty()
        && value != "."
        && value != ".."
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'));
    if usable {
        Ok(())
    } else {
        Err(InvalidIdentifier {
            field,
            value: value.to_string(),
        })
    }
}

/// The node's bundle directory.
pub struct BundleStore {
    root: PathBuf,
}

impl BundleStore {
    /// Open (creating if needed) the bundle directory.
    ///
    /// Any partial download left behind by a previous run is swept here: those
    /// files were never verified, so nothing may read them, and keeping them
    /// would leak disk one crashed deploy at a time.
    pub async fn open(root: PathBuf) -> std::io::Result<Self> {
        tokio::fs::create_dir_all(&root).await?;
        let temp = root.join(TEMP_DIR);
        if temp.exists() {
            if let Err(e) = tokio::fs::remove_dir_all(&temp).await {
                warn!(
                    dir = %temp.display(),
                    error = %e,
                    "Could not clear partial bundle downloads left by a previous run; they \
                     occupy disk but are never served"
                );
            }
        }
        tokio::fs::create_dir_all(&temp).await?;
        Ok(Self { root })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// A unique name inside the temp directory for one download in flight.
    pub fn temp_path(&self) -> PathBuf {
        self.root
            .join(TEMP_DIR)
            .join(format!("{}.part", uuid::Uuid::new_v4()))
    }

    /// Where one version's bundle lives once it has been verified.
    pub fn bundle_path(
        &self,
        app_id: &str,
        version_id: &str,
    ) -> Result<PathBuf, InvalidIdentifier> {
        check_identifier("app_id", app_id)?;
        check_identifier("version_id", version_id)?;
        Ok(self
            .root
            .join("apps")
            .join(app_id)
            .join(format!("{version_id}.zip")))
    }

    /// Move a verified download into place, replacing any earlier copy.
    pub async fn adopt(
        &self,
        temp_path: &Path,
        app_id: &str,
        version_id: &str,
    ) -> std::io::Result<PathBuf> {
        let target = self
            .bundle_path(app_id, version_id)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e.to_string()))?;
        if let Some(parent) = target.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        tokio::fs::rename(temp_path, &target).await?;
        Ok(target)
    }

    pub async fn read_bundle(&self, path: &Path) -> std::io::Result<Vec<u8>> {
        tokio::fs::read(path).await
    }

    /// Delete every bundle of `app_id` except the one being kept.
    ///
    /// Older versions are already immutable and stored on the control plane;
    /// keeping them here only costs disk, and a rollback re-downloads.
    pub async fn prune(&self, app_id: &str, keep_version: &str) {
        let Ok(keep) = self.bundle_path(app_id, keep_version) else {
            return;
        };
        let Some(dir) = keep.parent().map(|p| p.to_path_buf()) else {
            return;
        };
        let mut entries = match tokio::fs::read_dir(&dir).await {
            Ok(entries) => entries,
            Err(_) => return,
        };
        while let Ok(Some(entry)) = entries.next_entry().await {
            let path = entry.path();
            if path == keep {
                continue;
            }
            if let Err(e) = tokio::fs::remove_file(&path).await {
                warn!(
                    path = %path.display(),
                    error = %e,
                    "Could not remove a superseded bundle; it occupies disk but is never loaded"
                );
            }
        }
    }

    /// Forget one app entirely: its bundles and its line in the record.
    pub async fn forget(&self, app_id: &str) {
        if check_identifier("app_id", app_id).is_ok() {
            let dir = self.root.join("apps").join(app_id);
            if dir.exists() {
                if let Err(e) = tokio::fs::remove_dir_all(&dir).await {
                    warn!(
                        dir = %dir.display(),
                        error = %e,
                        "Could not remove the bundles of an unloaded app; they occupy disk and \
                         would be picked up again if this node restarts without reaching the \
                         control plane"
                    );
                }
            }
        }

        let mut records = self.records().await;
        records.retain(|r| r.app_id != app_id);
        self.write_records(&records).await;
    }

    /// Record that `record.app_id` now runs `record.version_id`, replacing any
    /// earlier line for that app.
    pub async fn remember(&self, record: InstalledRecord) {
        let mut records = self.records().await;
        records.retain(|r| r.app_id != record.app_id);
        records.push(record);
        records.sort_by(|a, b| a.app_id.cmp(&b.app_id));
        self.write_records(&records).await;
    }

    /// What this node installed last, as far as its own disk knows.
    ///
    /// A record whose bundle file has since gone missing is dropped: it names
    /// a version this node can no longer load, and reporting it would promise
    /// something that is not there.
    pub async fn records(&self) -> Vec<InstalledRecord> {
        let path = self.root.join(RECORD_FILE);
        let text = match tokio::fs::read_to_string(&path).await {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Vec::new(),
            Err(e) => {
                warn!(
                    path = %path.display(),
                    error = %e,
                    "Could not read the installed-version record; if the control plane is also \
                     unreachable this node will come up with nothing loaded"
                );
                return Vec::new();
            }
        };

        match serde_json::from_str::<Vec<InstalledRecord>>(&text) {
            Ok(records) => records,
            Err(e) => {
                warn!(
                    path = %path.display(),
                    error = %e,
                    "The installed-version record is not readable and is being ignored; this \
                     node can still be told what to run, but it cannot recover its previous \
                     versions on its own"
                );
                Vec::new()
            }
        }
    }

    async fn write_records(&self, records: &[InstalledRecord]) {
        let path = self.root.join(RECORD_FILE);
        let json = match serde_json::to_string_pretty(records) {
            Ok(json) => json,
            Err(e) => {
                warn!(error = %e, "Could not serialise the installed-version record");
                return;
            }
        };
        // Write beside the record and rename, so a crash mid-write leaves the
        // previous record intact rather than a truncated one.
        let temp = path.with_extension("json.new");
        if let Err(e) = tokio::fs::write(&temp, json).await {
            warn!(
                path = %temp.display(),
                error = %e,
                "Could not write the installed-version record; a restart without a reachable \
                 control plane would not know what this node was running"
            );
            return;
        }
        if let Err(e) = tokio::fs::rename(&temp, &path).await {
            warn!(
                path = %path.display(),
                error = %e,
                "Could not replace the installed-version record; a restart without a reachable \
                 control plane would recover an out-of-date version list"
            );
            return;
        }
        debug!(records = records.len(), "Installed-version record updated");
    }
}

#[cfg(test)]
#[path = "store_test.rs"]
mod store_test;
