//! Collecting the static files a bundle carries.
//!
//! Asset-first is the platform's first design principle: a request whose path
//! names a file in the bundle is answered from that file and never starts an
//! isolate. For that to work the manifest has to describe each file well
//! enough to serve it without opening it - its media type, its length, and a
//! digest strong enough to use as an `ETag`.

use std::path::Path;

use anyhow::{Context, Result};
use sha2::{Digest, Sha256};
use walkdir::WalkDir;

/// One static file, read and measured, on its way into the archive.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StagedAsset {
    /// Path relative to the assets root, no leading slash, `/` separators.
    /// This is both the manifest's `assets[].path` and, with the `assets/`
    /// prefix, the name of the entry in the archive.
    pub path: String,
    pub bytes: Vec<u8>,
    pub sha256: String,
    pub content_type: &'static str,
}

impl StagedAsset {
    pub fn size(&self) -> u64 {
        self.bytes.len() as u64
    }
}

/// Read every file under `root`, in sorted order.
///
/// A missing directory is not an error: an app with no static files is a
/// perfectly ordinary app, and failing the build over an absent `public/`
/// would make the assets feature mandatory.
///
/// Symlinks are not followed. A link is skipped rather than resolved, because
/// following one would let a bundle carry a copy of whatever it points at,
/// including files from outside the project.
pub fn collect_assets(root: &Path) -> Result<Vec<StagedAsset>> {
    if !root.exists() {
        return Ok(Vec::new());
    }

    let mut assets = Vec::new();

    for entry in WalkDir::new(root).sort_by_file_name() {
        let entry = entry.with_context(|| format!("reading assets under {}", root.display()))?;
        if !entry.file_type().is_file() {
            continue;
        }

        let relative = entry
            .path()
            .strip_prefix(root)
            .expect("walkdir yields paths under its root")
            .to_string_lossy()
            .replace('\\', "/");

        let bytes = std::fs::read(entry.path())
            .with_context(|| format!("reading asset {}", entry.path().display()))?;

        assets.push(StagedAsset {
            sha256: hex::encode(Sha256::digest(&bytes)),
            // The same table deis-runtime falls back to when an entry has
            // no content_type, so a file gets one answer whichever side
            // decides it.
            content_type: deis_bundle::guess_content_type(&relative),
            path: relative,
            bytes,
        });
    }

    // `sort_by_file_name` orders each directory's entries, but a parent's
    // files and its subdirectories interleave by depth. Sorting the finished
    // list is what makes the manifest and the archive byte-identical between
    // one machine and the next.
    assets.sort_by(|a, b| a.path.cmp(&b.path));

    Ok(assets)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(root: &Path, relative: &str, body: &[u8]) {
        let path = root.join(relative);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, body).unwrap();
    }

    fn temp_dir(name: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("deislet-assets-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_missing_assets_directory_is_not_an_error() {
        let assets = collect_assets(Path::new("/nonexistent/public")).unwrap();
        assert!(assets.is_empty());
    }

    #[test]
    fn files_come_back_sorted_with_their_digest_and_type() {
        let root = temp_dir("sorted");
        write(&root, "index.html", b"<h1>hi</h1>");
        write(&root, "css/app.css", b"body{}");
        write(&root, "img/logo.png", b"\x89PNG");

        let assets = collect_assets(&root).unwrap();
        let paths: Vec<&str> = assets.iter().map(|a| a.path.as_str()).collect();
        assert_eq!(paths, vec!["css/app.css", "img/logo.png", "index.html"]);

        let css = &assets[0];
        assert_eq!(css.content_type, "text/css; charset=utf-8");
        assert_eq!(css.size(), 6);
        assert_eq!(
            css.sha256,
            hex::encode(Sha256::digest(b"body{}")),
            "the digest is over the file's bytes"
        );

        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn an_unknown_extension_is_opaque_bytes() {
        let root = temp_dir("opaque");
        write(&root, "data.bin", b"\x00\x01");
        let assets = collect_assets(&root).unwrap();
        assert_eq!(assets[0].content_type, "application/octet-stream");
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn the_order_is_the_same_whatever_order_the_files_were_created_in() {
        let first = temp_dir("order-a");
        write(&first, "b.txt", b"b");
        write(&first, "a.txt", b"a");
        write(&first, "nested/c.txt", b"c");

        let second = temp_dir("order-b");
        write(&second, "nested/c.txt", b"c");
        write(&second, "a.txt", b"a");
        write(&second, "b.txt", b"b");

        let a: Vec<String> = collect_assets(&first)
            .unwrap()
            .into_iter()
            .map(|x| x.path)
            .collect();
        let b: Vec<String> = collect_assets(&second)
            .unwrap()
            .into_iter()
            .map(|x| x.path)
            .collect();
        assert_eq!(a, b);
        assert_eq!(a, vec!["a.txt", "b.txt", "nested/c.txt"]);

        std::fs::remove_dir_all(&first).unwrap();
        std::fs::remove_dir_all(&second).unwrap();
    }
}
