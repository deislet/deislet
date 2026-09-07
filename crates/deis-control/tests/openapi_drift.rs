//! `contracts/openapi/v1.json` is what `deis_control::api::openapi` renders,
//! byte for byte.
//!
//! The file is generated, and committed so that reviewers, the CLI and the
//! compatibility gate in `scripts/check-openapi-compat.py` see the contract
//! without building the control plane. This test is what keeps the two
//! from drifting: it fails with a readable diff when the code changed and
//! the file did not, and with `UPDATE_OPENAPI=1` it rewrites the file and
//! still fails, so the regeneration is a deliberate step and not a silent
//! side effect of running the suite.
//!
//! The web console's types are rendered from the same file by
//! `scripts/generate-openapi-types.py`, and CI holds that rendering to the
//! document as well; every message below names that step, because a
//! regenerated document with a stale client is the next failure along.

use std::path::PathBuf;

use deis_control::api::openapi;

fn contract_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join(openapi::CONTRACT_PATH)
}

/// The first differing line and a little context around it, so a failure
/// says what changed rather than dumping two documents of several hundred
/// kilobytes into the test log.
fn first_difference(expected: &str, actual: &str) -> String {
    let expected: Vec<&str> = expected.lines().collect();
    let actual: Vec<&str> = actual.lines().collect();
    let common = expected
        .iter()
        .zip(actual.iter())
        .take_while(|(a, b)| a == b)
        .count();
    if common == expected.len() && common == actual.len() {
        return "the documents differ only in their trailing bytes".to_string();
    }
    let from = common.saturating_sub(3);
    let mut out = format!("first difference at line {}:\n", common + 1);
    out.push_str("--- committed (contracts/openapi/v1.json)\n");
    for line in expected.iter().skip(from).take(8) {
        out.push_str("  ");
        out.push_str(line);
        out.push('\n');
    }
    out.push_str("+++ rendered by the code\n");
    for line in actual.iter().skip(from).take(8) {
        out.push_str("  ");
        out.push_str(line);
        out.push('\n');
    }
    out
}

#[test]
fn the_committed_document_is_what_the_code_renders() {
    let path = contract_path();
    let rendered = openapi::render();
    let committed = std::fs::read_to_string(&path).unwrap_or_default();
    if committed == rendered {
        return;
    }
    if std::env::var_os("UPDATE_OPENAPI").is_some_and(|value| value == "1") {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, &rendered).unwrap();
        panic!(
            "{} regenerated from the code. Run `python3 scripts/generate-openapi-types.py` to \
             regenerate the web console's client from it, run scripts/check-openapi-compat.py \
             to see whether the change is breaking, then commit both files.",
            openapi::CONTRACT_PATH
        );
    }
    panic!(
        "{} does not match what deis_control::api::openapi renders.\n{}\nRun \
         `UPDATE_OPENAPI=1 cargo test -p deis-control --test openapi_drift` to regenerate it, \
         then `python3 scripts/generate-openapi-types.py` to regenerate the web console's \
         client from it, then commit both files.",
        openapi::CONTRACT_PATH,
        first_difference(&committed, &rendered)
    );
}

#[test]
fn the_committed_document_carries_the_declared_version() {
    let committed = std::fs::read_to_string(contract_path())
        .expect("contracts/openapi/v1.json exists; regenerate it with UPDATE_OPENAPI=1");
    let document: serde_json::Value = serde_json::from_str(&committed).unwrap();
    assert_eq!(document["info"]["version"], openapi::DOCUMENT_VERSION);
    assert_eq!(document["openapi"], "3.1.0");
}
