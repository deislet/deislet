//! What the driver remembers about one application environment between
//! driver processes.
//!
//! Each lifecycle operation runs in its own process, and the Control passes
//! no provider state back in a request, so `activate` learns which version
//! `upload` created from here. The journal lives in the driver's state
//! directory, keyed by deployment scope - never in the artifact directory,
//! which may be read-only and is shared by every environment the artifact is
//! deployed to. Writes are atomic (temp file + rename).

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::naming::DeploymentScope;

/// One provisioned resource.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct JournalResource {
    /// `kv`, `d1`, `r2` or `q`.
    pub kind: String,
    /// The scoped Cloudflare name.
    pub name: String,
    /// The identifier wrangler.toml takes: namespace id, database uuid,
    /// bucket name, or queue name.
    pub id: String,
}

/// A version `upload` created and `activate` has not yet made live.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct UploadedVersion {
    pub version_id: String,
    /// The desired state the version was uploaded for; `activate` refuses to
    /// deploy a version uploaded for another one.
    pub desired_state_digest: String,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DeploymentJournal {
    /// The Worker name this environment deploys to.
    #[serde(default)]
    pub worker_name: Option<String>,
    /// Provisioned resources by `RequiredResource::key`.
    #[serde(default)]
    pub resources: BTreeMap<String, JournalResource>,
    /// The most recent upload.
    #[serde(default)]
    pub uploaded: Option<UploadedVersion>,
    /// The version the last activate or rollback deployed.
    #[serde(default)]
    pub active_version_id: Option<String>,
    /// The deployment id wrangler reported for it.
    #[serde(default)]
    pub deployment_id: Option<String>,
}

/// Where journals are kept.
#[derive(Clone, Debug)]
pub struct JournalStore {
    root: PathBuf,
}

impl JournalStore {
    pub fn new(state_dir: &Path) -> Self {
        Self {
            root: state_dir.join("cloudflare"),
        }
    }

    pub fn path(&self, scope: &DeploymentScope) -> PathBuf {
        self.root.join(scope.token()).join("journal.json")
    }

    /// The journal for `scope`, or an empty one if none was written. A journal
    /// that exists but cannot be read is an error: guessing would lose track
    /// of provisioned resources.
    pub fn load(&self, scope: &DeploymentScope) -> io::Result<DeploymentJournal> {
        let path = self.path(scope);
        match fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes).map_err(|error| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("{} is not a driver journal: {error}", path.display()),
                )
            }),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                Ok(DeploymentJournal::default())
            }
            Err(error) => Err(error),
        }
    }

    pub fn save(&self, scope: &DeploymentScope, journal: &DeploymentJournal) -> io::Result<()> {
        let path = self.path(scope);
        let dir = path.parent().expect("journal path has a parent");
        fs::create_dir_all(dir)?;
        let bytes = serde_json::to_vec_pretty(journal).map_err(io::Error::other)?;
        let mut temp = tempfile::NamedTempFile::new_in(dir)?;
        io::Write::write_all(&mut temp, &bytes)?;
        temp.as_file().sync_all()?;
        temp.persist(&path).map_err(|error| error.error)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn journal_round_trips_per_scope() {
        let dir = tempfile::tempdir().unwrap();
        let store = JournalStore::new(dir.path());
        let a = DeploymentScope::new(
            &"01890a5d-ac96-774b-bcce-b302099a8057".parse().unwrap(),
            &"01890a5d-ac96-774b-bcce-b302099a8059".parse().unwrap(),
        );
        let b = DeploymentScope::new(
            &"01890a5d-ac96-774b-bcce-b302099a8058".parse().unwrap(),
            &"01890a5d-ac96-774b-bcce-b302099a8059".parse().unwrap(),
        );
        let mut journal = DeploymentJournal {
            worker_name: Some(a.worker_name()),
            ..DeploymentJournal::default()
        };
        journal.resources.insert(
            "kv:HOT".to_owned(),
            JournalResource {
                kind: "kv".to_owned(),
                name: a.resource_name(deis_build_core::BindingKind::Kv, "HOT"),
                id: "ns-1".to_owned(),
            },
        );
        store.save(&a, &journal).unwrap();
        assert_eq!(store.load(&a).unwrap(), journal);
        assert_eq!(store.load(&b).unwrap(), DeploymentJournal::default());
    }

    #[test]
    fn a_corrupt_journal_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let store = JournalStore::new(dir.path());
        let scope = DeploymentScope::new(
            &"01890a5d-ac96-774b-bcce-b302099a8057".parse().unwrap(),
            &"01890a5d-ac96-774b-bcce-b302099a8059".parse().unwrap(),
        );
        let path = store.path(&scope);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "{not json").unwrap();
        assert!(store.load(&scope).is_err());
    }
}
