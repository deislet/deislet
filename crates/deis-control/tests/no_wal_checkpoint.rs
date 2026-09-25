//! Guard test from ADR-012: the control plane must never call the WAL
//! checkpoint pragma — checkpoints are Litestream's job, and the control
//! code only keeps SQLite's own PASSIVE auto-checkpoints. See
//! docs/adr/012-pluggable-persistence.md. The search string below is
//! built by concatenation so this test does not match itself.

use std::fs;
use std::path::{Path, PathBuf};

/// Recursively collect every `.rs` file under `dir`.
fn collect_rs_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_rs_files(&path, out);
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            out.push(path);
        }
    }
}

#[test]
fn control_plane_must_not_checkpoint_the_wal() {
    let needle = "wal_".to_string() + "checkpoint";

    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut files = Vec::new();
    collect_rs_files(&root.join("src"), &mut files);
    collect_rs_files(&root.join("tests"), &mut files);

    let offenders: Vec<String> = files
        .iter()
        .filter_map(|path| {
            let text = fs::read_to_string(path).ok()?;
            text.contains(&needle).then(|| path.display().to_string())
        })
        .collect();

    assert!(
        offenders.is_empty(),
        "control-plane source must not reference the WAL checkpoint pragma \
         (checkpoints belong to Litestream, see ADR-012), but found it in: {offenders:?}"
    );
}
