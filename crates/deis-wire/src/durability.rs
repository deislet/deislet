//! Getting names onto the disk before a caller is told they are there.
//!
//! A successful `create`, `write` or `rename` makes a file visible, but on
//! most filesystems the *name* is not durable until the directory that lists
//! it is fsynced: after a power loss a file whose bytes were synced can still
//! be gone because its directory entry never reached the platter. The helpers
//! here close that gap for the one operation every component needs. Whether a
//! component actually calls them before answering its caller is that
//! component's own durability contract.

use std::fs::File;
use std::io;
use std::path::Path;

/// Flush `path`'s directory entry to stable storage by fsyncing the
/// directory that lists it.
///
/// Call this after creating or renaming a file into place, on the file's own
/// path. Syncing the parent is what makes the entry durable; the file's bytes
/// need their own `sync_all` beforehand. On platforms without directory file
/// descriptors (Windows) this is a no-op, which matches what `File::sync_all`
/// can promise there.
///
/// Note the limit of the guarantee: this orders the name against a power
/// loss, nothing more. A unit test can check that the call is made, but it
/// cannot simulate the power loss, so a caller that must not lose a file
/// keeps this call on its critical path.
pub fn sync_parent_directory(path: &Path) -> io::Result<()> {
    let Some(parent) = path.parent() else {
        return Ok(());
    };
    #[cfg(unix)]
    File::open(parent)?.sync_all()?;
    #[cfg(not(unix))]
    let _ = parent;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn a_created_file_has_a_fsyncable_parent() {
        let dir = std::env::temp_dir().join(format!(
            "deis-wire-durability-{}",
            uuid::Uuid::new_v4()
        ));
        fs::create_dir_all(&dir).unwrap();
        let file = dir.join("entry");
        fs::write(&file, b"bytes").unwrap();

        // The assertion is only that the call succeeds on a real directory
        // entry; that the entry survives a power loss is not observable in a
        // unit test, which is why callers keep this on the critical path.
        sync_parent_directory(&file).expect("the parent directory fsyncs");

        fs::remove_dir_all(&dir).unwrap();
    }
}
