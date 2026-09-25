//! The routing table this instance last published, kept on disk.
//!
//! A poll loop that fails changes nothing, which holds a control-plane
//! outage away from traffic - but only for as long as the process lives. A
//! proxy restarted while the control plane is unreachable has fetched
//! nothing, so it would serve no catalog route at all, and the management
//! outage would reach customers one restart later. So every table this
//! instance publishes is written to one file, and a starting instance reads
//! that file back before it polls.
//!
//! This is not a second source of truth. The file is a copy of what the
//! control plane last served, and a successful poll replaces the table in
//! memory and the file together - the live answer always wins. A file that
//! is missing, unreadable, truncated or does not validate is ignored, one
//! warning is logged, and the instance starts with no catalog route, which
//! is the state it would have been in without this module.
//!
//! Every write is atomic: a temporary file beside the destination, fsynced,
//! renamed over it, then the directory fsynced. A reader - this process or
//! the next one - sees the whole previous table or the whole new one, never
//! a document that stops in the middle of a route. Each write carries a
//! generation that is one more than the last, so two copies of one table
//! version can still be told apart and a stale file recognised.

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use deis_wire::durability::sync_parent_directory;
use serde::{Deserialize, Serialize};
use tracing::{debug, warn};

use super::wire::RoutingTableDocument;

/// A routing table as it is kept on disk: the document, and the write that
/// put this copy there.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct StoredRoutingTable {
    /// One more than the generation of the write before it, and monotonic
    /// across restarts, so which of two copies of one table version came
    /// later is answerable from the files themselves.
    pub generation: u64,
    /// Unix seconds of that write. This is when *this disk* learned the
    /// table, which is not `document.generated_at`: a table built a week ago
    /// can be written today, and that is exactly the gap an operator looking
    /// at a stale route needs to see.
    pub written_at: i64,
    /// The document, as the control plane served it.
    pub document: RoutingTableDocument,
}

/// What is written: [`StoredRoutingTable`] with the document borrowed rather
/// than cloned, because a routing table is large enough that copying one to
/// serialize it is worth avoiding.
#[derive(Serialize)]
struct StoredRoutingTableRef<'a> {
    generation: u64,
    written_at: i64,
    document: &'a RoutingTableDocument,
}

/// The file a starting proxy reads when the control plane does not answer,
/// and the poll loop writes after every table it publishes.
pub struct RoutingTableCache {
    path: PathBuf,
    /// The generation last seen on disk and the version it held. Written
    /// only by [`Self::load`] and [`Self::persist`], both of which hold this
    /// lock across their file I/O: there is one poll loop and one write at a
    /// time, and a caller that had to serialize these itself would be a
    /// second place to get it wrong.
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    generation: u64,
    /// The version already on disk. A poll that returns the table that is
    /// already there - which is every poll between changes - must not rewrite
    /// the file, generation and all, every few seconds.
    persisted_version: Option<String>,
}

impl RoutingTableCache {
    /// A cache over `path`. Nothing is read or created until [`Self::load`]
    /// or [`Self::persist`] is called.
    pub fn new(path: PathBuf) -> Self {
        Self {
            path,
            state: Mutex::new(State::default()),
        }
    }

    /// The file this cache reads and writes.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The table this instance last published, or `None` when the file holds
    /// none that may be applied.
    ///
    /// `None` covers the whole range the caller treats identically - no file
    /// yet, a file that cannot be read, bytes that are not this format, a
    /// document that does not validate - and the last three warn, because
    /// each means the file is no longer what was written. A missing file is
    /// a first start rather than a fault, and is logged at debug.
    ///
    /// On success the generation and version found are remembered, so the
    /// next write continues the sequence instead of restarting it at one.
    pub fn load(&self) -> Option<StoredRoutingTable> {
        let bytes = match fs::read(&self.path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                debug!(
                    path = %self.path.display(),
                    "no routing table has been persisted here yet"
                );
                return None;
            }
            Err(error) => {
                warn!(
                    path = %self.path.display(),
                    %error,
                    "the persisted routing table could not be read and is ignored; this instance \
                     has no catalog route until a poll succeeds"
                );
                return None;
            }
        };

        let stored: StoredRoutingTable = match serde_json::from_slice(&bytes) {
            Ok(stored) => stored,
            Err(error) => {
                warn!(
                    path = %self.path.display(),
                    %error,
                    "the persisted routing table is not the document this build writes and is \
                     ignored; this instance has no catalog route until a poll succeeds"
                );
                return None;
            }
        };
        if let Err(error) = stored.document.validate() {
            warn!(
                path = %self.path.display(),
                %error,
                generation = stored.generation,
                "the persisted routing table does not validate and is ignored; this instance has \
                 no catalog route until a poll succeeds"
            );
            return None;
        }

        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        state.generation = stored.generation;
        state.persisted_version = Some(stored.document.version.clone());
        debug!(
            path = %self.path.display(),
            generation = stored.generation,
            version = %stored.document.version,
            routes = stored.document.routes.len(),
            "routing table read back from disk"
        );
        Some(stored)
    }

    /// Keep `document` as the most recent table, replacing whatever the file
    /// held.
    ///
    /// Writing the version that is already on disk does nothing: the file
    /// already holds this table.
    pub fn persist(&self, document: &RoutingTableDocument) -> Result<(), CacheError> {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        if state.persisted_version.as_deref() == Some(document.version.as_str()) {
            return Ok(());
        }

        let generation = state.generation.saturating_add(1);
        let bytes = serde_json::to_vec(&StoredRoutingTableRef {
            generation,
            written_at: chrono::Utc::now().timestamp(),
            document,
        })?;
        write_atomic(&self.path, &bytes)?;

        state.generation = generation;
        state.persisted_version = Some(document.version.clone());
        debug!(
            path = %self.path.display(),
            generation,
            bytes = bytes.len(),
            version = %document.version,
            "routing table persisted"
        );
        Ok(())
    }
}

/// What keeping a table on disk can fail with.
#[derive(Debug, thiserror::Error)]
pub enum CacheError {
    #[error(
        "the routing table could not be persisted at {}: {source}",
        path.display()
    )]
    Write {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("the routing table could not be encoded for persistence: {0}")]
    Encode(#[from] serde_json::Error),
}

/// Replace `path` with `contents`: a temporary file in the same directory,
/// then a rename over the destination.
///
/// The rename is what makes this atomic, and the two fsyncs are what keep it
/// atomic across a power loss. Without the first, a restart could read a
/// document that is halfway written; without the second, the name could be
/// gone after a crash while the bytes are there, which a reader cannot tell
/// from "this proxy has never persisted anything".
fn write_atomic(path: &Path, contents: &[u8]) -> Result<(), CacheError> {
    let parent = match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        // A bare file name in the working directory, which is what the
        // default path beside a relative `database` comes out as.
        _ => Path::new("."),
    };
    fs::create_dir_all(parent).map_err(|source| CacheError::Write {
        path: parent.to_path_buf(),
        source,
    })?;

    // A name no other writer - in this process or another one - is using, so
    // that two writers cannot truncate each other's temporary file and leave
    // one of them renaming a file that is already gone.
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let temporary = parent.join(format!(
        ".{}.{}.{}.tmp",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("routing-table"),
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed),
    ));

    let write = || -> io::Result<()> {
        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            // The table names every application and host this platform
            // serves. That is not a credential, but it is a map of the fleet
            // and there is no reason for another identity on this machine to
            // be able to read it.
            .mode(0o600)
            .open(&temporary)?;
        file.write_all(contents)?;
        // Before the rename: the point of the rename is that a reader sees
        // one whole document or the other, and that only holds if the bytes
        // are on the disk before the name is.
        file.sync_all()
    };

    if let Err(source) = write() {
        // Nothing will ever rename this file, and a leftover temporary is a
        // full copy of a routing table nobody will clean up.
        let _ = fs::remove_file(&temporary);
        return Err(CacheError::Write {
            path: temporary,
            source,
        });
    }

    if let Err(source) = fs::rename(&temporary, path) {
        let _ = fs::remove_file(&temporary);
        return Err(CacheError::Write {
            path: path.to_path_buf(),
            source,
        });
    }

    sync_parent_directory(path).map_err(|source| CacheError::Write {
        path: path.to_path_buf(),
        source,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cache_at(path: &Path) -> RoutingTableCache {
        RoutingTableCache::new(path.to_path_buf())
    }

    fn document(version: &str, host: &str) -> RoutingTableDocument {
        let document: RoutingTableDocument = serde_json::from_str(&format!(
            r#"{{"version":"{version}","generated_at":11,"routes":[
                {{"app_id":"shop","environment":"production","host":"{host}","path_prefix":"/","priority":0}}
            ],"environments":[]}}"#
        ))
        .expect("a document");
        document.validate().expect("a valid document");
        document
    }

    #[test]
    fn a_persisted_table_is_read_back_whole() {
        let directory = tempfile::tempdir().expect("a directory");
        let path = directory.path().join("routing-table.json");
        let cache = cache_at(&path);

        cache
            .persist(&document("v1", "shop.test"))
            .expect("a persisted table");

        let stored = cache_at(&path).load().expect("the table just written");
        assert_eq!(stored.generation, 1);
        assert_eq!(stored.document.version, "v1");
        assert_eq!(stored.document.routes.len(), 1);
        assert_eq!(stored.document.routes[0].app_id, "shop");
        assert_eq!(stored.document.routes[0].host.as_deref(), Some("shop.test"));
        assert!(
            stored.written_at > 0,
            "the write time is what tells an operator how old a table still in force is"
        );
    }

    #[test]
    fn every_write_of_a_newer_table_has_a_higher_generation() {
        let directory = tempfile::tempdir().expect("a directory");
        let path = directory.path().join("routing-table.json");
        let cache = cache_at(&path);

        cache.persist(&document("v1", "shop.test")).unwrap();
        assert_eq!(cache_at(&path).load().unwrap().generation, 1);

        cache.persist(&document("v2", "shop.test")).unwrap();
        assert_eq!(
            cache_at(&path).load().unwrap().generation,
            2,
            "a later table has to be recognisable as the later one"
        );

        // The next process continues the sequence rather than restarting it:
        // a generation that went back to one would make an old file look
        // newer than the one beside it.
        let restarted = cache_at(&path);
        let stored = restarted.load().expect("the table the last process wrote");
        assert_eq!(stored.document.version, "v2");
        restarted.persist(&document("v3", "shop.test")).unwrap();
        assert_eq!(cache_at(&path).load().unwrap().generation, 3);
    }

    #[test]
    fn the_table_already_on_disk_is_not_written_again() {
        // Every poll between two changes returns the same table. Rewriting
        // it five seconds at a time would spend an fsync per poll on a file
        // whose contents are already there.
        let directory = tempfile::tempdir().expect("a directory");
        let path = directory.path().join("routing-table.json");
        let cache = cache_at(&path);

        let document = document("v1", "shop.test");
        cache.persist(&document).unwrap();
        let written = fs::read(&path).expect("the file just written");

        cache.persist(&document).unwrap();
        assert_eq!(
            fs::read(&path).expect("the file"),
            written,
            "nothing changed, so nothing on disk may change"
        );
    }

    #[test]
    fn a_file_that_is_not_a_validated_document_is_ignored() {
        // Every shape a copy of this file can arrive in without being one we
        // may apply: cut short, written by something else, from a build with
        // a field this one does not know, and complete but failing the same
        // all-or-nothing validation a fetched table passes.
        let cases: Vec<(&str, Vec<u8>)> = vec![
            ("cut short", br#"{"generation":1,"written_at":11,"doc"#.to_vec()),
            ("not json at all", b"a routing table, honestly".to_vec()),
            (
                "an object of another shape",
                br#"{"version":"v1","routes":[]}"#.to_vec(),
            ),
            (
                "no generation",
                br#"{"written_at":11,"document":{"version":"v1","generated_at":11,"routes":[],"environments":[]}}"#
                    .to_vec(),
            ),
            (
                "a field this build does not write",
                br#"{"generation":1,"written_at":11,"generation_note":"x","document":{"version":"v1","generated_at":11,"routes":[],"environments":[]}}"#
                    .to_vec(),
            ),
            (
                "an entry that fails validation",
                br#"{"generation":1,"written_at":11,"document":{"version":"v1","generated_at":11,"routes":[{"app_id":"shop","environment":"pro duction","path_prefix":"/"}],"environments":[]}}"#
                    .to_vec(),
            ),
        ];

        for (what, bytes) in cases {
            let directory = tempfile::tempdir().expect("a directory");
            let path = directory.path().join("routing-table.json");
            fs::write(&path, &bytes).expect("the corrupt file");

            assert!(
                cache_at(&path).load().is_none(),
                "{what} was applied as if it were a table"
            );
        }
    }

    #[test]
    fn nothing_has_been_persisted_yet_is_not_a_fault() {
        let directory = tempfile::tempdir().expect("a directory");
        let path = directory.path().join("routing-table.json");
        assert!(cache_at(&path).load().is_none());
        assert!(
            !path.exists(),
            "reading a file that is not there must not create one"
        );
    }

    #[test]
    fn a_write_leaves_the_destination_and_no_temporary_behind() {
        // The temporary is a full copy of the table and is renamed, not
        // deleted: one left behind is a stale copy of the fleet's routes
        // that nothing will ever clean up.
        let directory = tempfile::tempdir().expect("a directory");
        let path = directory.path().join("routing-table.json");
        cache_at(&path)
            .persist(&document("v1", "shop.test"))
            .unwrap();

        let names: Vec<String> = fs::read_dir(directory.path())
            .expect("the directory")
            .map(|entry| {
                entry
                    .expect("an entry")
                    .file_name()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        assert_eq!(names, vec!["routing-table.json".to_string()]);
    }

    #[test]
    fn a_missing_parent_directory_is_created() {
        // The configured path is a directory an operator chose, which on a
        // first start need not exist yet.
        let directory = tempfile::tempdir().expect("a directory");
        let path = directory.path().join("var/state/routing-table.json");
        cache_at(&path)
            .persist(&document("v1", "shop.test"))
            .unwrap();
        assert!(cache_at(&path).load().is_some());
    }
}
