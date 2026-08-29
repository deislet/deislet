use std::io::{Cursor, Write};

use deis_bundle::{Asset, Manifest};
use sha2::{Digest, Sha256};

use super::*;

fn cache_control() -> Arc<str> {
    Arc::from("public, max-age=0, must-revalidate")
}

fn sha_of(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

/// Build a zip in memory the way a deislet build would lay one out.
fn archive(files: &[(&str, &[u8])]) -> Vec<u8> {
    let mut buf = Vec::new();
    {
        let mut writer = zip::ZipWriter::new(Cursor::new(&mut buf));
        for (name, body) in files {
            writer
                .start_file(*name, zip::write::FileOptions::default())
                .unwrap();
            writer.write_all(body).unwrap();
        }
        writer.finish().unwrap();
    }
    buf
}

fn manifest_with(assets: Vec<Asset>) -> Manifest {
    let mut manifest = Manifest::parse(r#"{"schema":1,"app":"demo","entry":"worker/index.js"}"#)
        .expect("the minimal manifest must parse");
    manifest.assets = assets;
    manifest
}

fn asset(path: &str, sha256: Option<String>) -> Asset {
    Asset {
        path: path.to_string(),
        sha256,
        content_type: None,
        size: None,
    }
}

fn build(manifest: &Manifest, zip_bytes: &[u8], max_bytes: u64) -> Result<AssetBundle, AssetError> {
    let mut archive = zip::ZipArchive::new(Cursor::new(zip_bytes.to_vec())).unwrap();
    AssetBundle::from_archive(manifest, &mut archive, cache_control(), max_bytes)
}

#[test]
fn serves_a_declared_file_with_its_digest_as_the_etag() {
    let body = b"body{}" as &[u8];
    let zip_bytes = archive(&[("worker/index.js", b"export {}"), ("assets/app.css", body)]);
    let manifest = manifest_with(vec![asset("app.css", None)]);

    let bundle = build(&manifest, &zip_bytes, 1024).unwrap();
    let hit = bundle.resolve("/app.css").unwrap();

    assert_eq!(&hit.bytes[..], body);
    assert_eq!(hit.etag, format!("\"{}\"", sha_of(body)));
    assert_eq!(hit.content_type, "text/css; charset=utf-8");
    assert_eq!(&*hit.cache_control, "public, max-age=0, must-revalidate");
}

#[test]
fn serves_index_html_for_the_root() {
    let zip_bytes = archive(&[
        ("worker/index.js", b"export {}"),
        ("assets/index.html", b"<h1>hi</h1>"),
    ]);
    let manifest = manifest_with(vec![asset("index.html", None)]);

    let bundle = build(&manifest, &zip_bytes, 1024).unwrap();
    assert_eq!(bundle.resolve("/").unwrap().path, "index.html");
}

#[test]
fn a_path_with_no_asset_falls_through_to_the_isolate() {
    let zip_bytes = archive(&[
        ("worker/index.js", b"export {}"),
        ("assets/index.html", b"<h1>hi</h1>"),
    ]);
    let manifest = manifest_with(vec![asset("index.html", None)]);

    let bundle = build(&manifest, &zip_bytes, 1024).unwrap();
    assert!(bundle.resolve("/api/users").is_none());
}

#[test]
fn accepts_a_manifest_digest_that_describes_the_bytes() {
    let body = b"console.log(1)" as &[u8];
    let zip_bytes = archive(&[("worker/index.js", b"export {}"), ("assets/a.js", body)]);
    let manifest = manifest_with(vec![asset("a.js", Some(sha_of(body).to_uppercase()))]);

    // Case is not part of a hex digest's meaning, so an upper-case one must
    // still match rather than fail the whole version.
    let bundle = build(&manifest, &zip_bytes, 1024).unwrap();
    assert!(bundle.resolve("/a.js").is_some());
}

#[test]
fn refuses_a_version_whose_manifest_digest_does_not_match_the_bytes() {
    let zip_bytes = archive(&[("worker/index.js", b"export {}"), ("assets/a.js", b"real")]);
    let manifest = manifest_with(vec![asset("a.js", Some(sha_of(b"different")))]);

    let err = build(&manifest, &zip_bytes, 1024).unwrap_err();
    assert!(matches!(err, AssetError::DigestMismatch { .. }), "{err:?}");
}

#[test]
fn refuses_a_version_missing_a_file_its_manifest_promises() {
    let zip_bytes = archive(&[("worker/index.js", b"export {}")]);
    let manifest = manifest_with(vec![asset("index.html", None)]);

    let err = build(&manifest, &zip_bytes, 1024).unwrap_err();
    assert_eq!(
        err,
        AssetError::Missing {
            path: "index.html".to_string()
        }
    );
}

#[test]
fn refuses_a_version_whose_assets_exceed_the_memory_ceiling() {
    let big = vec![b'x'; 4096];
    let zip_bytes = archive(&[("worker/index.js", b"export {}"), ("assets/big.bin", &big)]);
    let manifest = manifest_with(vec![asset("big.bin", None)]);

    let err = build(&manifest, &zip_bytes, 1024).unwrap_err();
    assert!(
        matches!(err, AssetError::TooLarge { limit: 1024, .. }),
        "{err:?}"
    );
}

#[test]
fn counts_files_the_manifest_never_names() {
    let zip_bytes = archive(&[
        ("worker/index.js", b"export {}"),
        ("assets/index.html", b"<h1>hi</h1>"),
        ("assets/orphan.txt", b"nobody asked for me"),
    ]);
    let manifest = manifest_with(vec![asset("index.html", None)]);

    let bundle = build(&manifest, &zip_bytes, 4096).unwrap();
    assert_eq!(bundle.len(), 1);
    assert_eq!(bundle.undeclared_files(), 1);
    assert!(bundle.resolve("/orphan.txt").is_none());
}

#[test]
fn an_app_with_no_assets_resolves_nothing() {
    let bundle = AssetBundle::empty(cache_control());
    assert!(bundle.is_empty());
    assert_eq!(bundle.total_bytes(), 0);
    assert!(bundle.resolve("/").is_none());
}

#[test]
fn a_repeated_manifest_entry_is_counted_rather_than_read_twice() {
    let zip_bytes = archive(&[("worker/index.js", b"export {}"), ("assets/a.txt", b"a")]);
    // Which of two entries for one path wins would otherwise depend on
    // manifest ordering, so the duplicate is counted and dropped rather than
    // quietly overwriting the first.
    let manifest = manifest_with(vec![asset("a.txt", None), asset("a.txt", None)]);

    let bundle = build(&manifest, &zip_bytes, 1024).unwrap();
    assert_eq!(bundle.len(), 1);
    assert_eq!(bundle.duplicate_paths(), 1);
}

#[test]
fn conditional_request_matching_follows_weak_comparison() {
    assert!(if_none_match_matches("\"abc\"", "\"abc\""));
    assert!(if_none_match_matches("W/\"abc\"", "\"abc\""));
    assert!(if_none_match_matches("*", "\"abc\""));
    assert!(if_none_match_matches("\"x\", \"abc\"", "\"abc\""));
    assert!(!if_none_match_matches("\"other\"", "\"abc\""));
    assert!(!if_none_match_matches("", "\"abc\""));
}
