//! Which physical copy of a data service's directory is being served.
//!
//! deis-store, deis-queue and deis-telemetry each keep one data directory.
//! A restore replaces that directory with a copy of an earlier one, and the
//! topology driver and the restore report have to be able to say which copy
//! a directory is: the one that was there before, or the one a restore put
//! in its place. That is the backend incarnation, a UUIDv7 written once into
//! [`INCARNATION_FILE`] the first time a service opens an empty directory.
//!
//! A snapshot never carries the file. A restore therefore always leaves a
//! directory without one, and the first open after it mints a fresh
//! incarnation, so a restored directory can never claim to be the source it
//! was copied from.

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use crate::CanonicalId;
use crate::durability::sync_parent_directory;

/// The file inside a data directory that names its incarnation.
pub const INCARNATION_FILE: &str = "backend-incarnation";

/// Where `data_dir` keeps its incarnation.
#[must_use]
pub fn incarnation_path(data_dir: &Path) -> PathBuf {
    data_dir.join(INCARNATION_FILE)
}

/// The incarnation recorded in `data_dir`, if one has been minted.
///
/// A file that exists but does not hold exactly one canonical UUIDv7 is an
/// error, not an absent incarnation: treating it as absent would mint a new
/// one over a directory that already has an identity.
pub fn read(data_dir: &Path) -> io::Result<Option<CanonicalId>> {
    let path = incarnation_path(data_dir);
    let text = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let value = text.strip_suffix('\n').unwrap_or(&text);
    value.parse().map(Some).map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "{} does not hold one canonical UUIDv7: {error}",
                path.display()
            ),
        )
    })
}

/// The incarnation of `data_dir`, minting and durably recording one when the
/// directory has none yet.
///
/// The file is created exclusively, synced, and its directory entry synced,
/// before the id is returned; two openers racing on an empty directory agree
/// on whichever id reached the disk first.
pub fn load_or_create(data_dir: &Path) -> io::Result<CanonicalId> {
    if let Some(existing) = read(data_dir)? {
        return Ok(existing);
    }
    fs::create_dir_all(data_dir)?;
    let path = incarnation_path(data_dir);
    let id = CanonicalId::new_v7();
    match OpenOptions::new().write(true).create_new(true).open(&path) {
        Ok(mut file) => {
            file.write_all(format!("{id}\n").as_bytes())?;
            file.sync_all()?;
            drop(file);
            sync_parent_directory(&path)?;
            Ok(id)
        }
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            read(data_dir)?.ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("{} vanished while it was being read", path.display()),
                )
            })
        }
        Err(error) => Err(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch() -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("deis-wire-incarnation-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn an_empty_directory_gets_one_stable_incarnation() {
        let dir = scratch();
        assert_eq!(read(&dir).unwrap(), None);
        let first = load_or_create(&dir).unwrap();
        assert_eq!(load_or_create(&dir).unwrap(), first);
        assert_eq!(read(&dir).unwrap(), Some(first));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_directory_without_the_file_is_a_new_incarnation() {
        let dir = scratch();
        let first = load_or_create(&dir).unwrap();
        fs::remove_file(incarnation_path(&dir)).unwrap();
        assert_ne!(load_or_create(&dir).unwrap(), first);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_damaged_record_is_refused_rather_than_replaced() {
        let dir = scratch();
        fs::write(incarnation_path(&dir), "not-an-id\n").unwrap();
        assert!(read(&dir).is_err());
        assert!(load_or_create(&dir).is_err());
        fs::remove_dir_all(&dir).unwrap();
    }
}
