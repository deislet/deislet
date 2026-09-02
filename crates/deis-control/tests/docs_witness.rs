//! `docs/trust-boundaries.md` is the one place the fleet credential's trust
//! boundary is written down, and these tests hold it to that.
//!
//! The decision it records - the node credential is not being split in this
//! release, so the boundary is stated truthfully instead - lives nowhere in
//! the code. Nothing would fail if the sentences that carry it were softened
//! or deleted, and a boundary that only a document enforces is exactly the
//! kind that gets edited away by someone tidying prose. So the claims are
//! asserted here, in the same shape `architecture_diagrams.rs` uses to hold
//! `ARCHITECTURE.md` to what generated it.
//!
//! What this checks is that the claims are still stated, and that the
//! documents which used to state a half of them still point at the whole.
//! It cannot check that they are still true: if the credential is ever split,
//! or per-tenant keys arrive, these tests are what has to be rewritten
//! alongside the document, deliberately.
//!
//! Paths are relative to this file, so a document that moves fails to compile
//! rather than silently stopping being checked.

const TRUST_BOUNDARIES: &str = include_str!("../../../docs/trust-boundaries.md");
const README: &str = include_str!("../../../README.md");
const ARCHITECTURE_ZH: &str = include_str!("../../../docs/architecture.zh.md");
const API_DOCUMENTATION: &str = include_str!("../API_DOCUMENTATION.md");
const DEPLOYMENT: &str = include_str!("../DEPLOYMENT.md");
const CONTROL_ARCHITECTURE: &str = include_str!("../ARCHITECTURE.md");
const PROXY_README: &str = include_str!("../../deis-proxy/README.md");
const RUNTIME_README: &str = include_str!("../../deis-runtime/README.md");
const WIRE_README: &str = include_str!("../../deis-wire/README.md");

/// Assert on a document with the newlines taken out of the way.
///
/// The sentences below are wrapped in the source, so a match on the literal
/// would break the first time somebody reflowed a paragraph - which is a
/// change that alters nothing about what the document says, and must not be
/// what this test reports.
fn unwrapped(document: &str) -> String {
    document.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn assert_says(document: &str, name: &str, phrase: &str) {
    assert!(
        unwrapped(document).contains(phrase),
        "{name} no longer says \"{phrase}\".\n\nThat sentence is the decision \
         this release took about the fleet credential. If the decision itself \
         changed, change this test in the same commit and say so; if it did \
         not, put the sentence back."
    );
}

#[test]
fn the_credential_is_still_said_to_equal_every_tenants_plaintext() {
    // The join, not one of its halves. Every other document is allowed to
    // describe only the endpoint it is about; this one is not.
    assert_says(
        TRUST_BOUNDARIES,
        "docs/trust-boundaries.md",
        "the credential equals every tenant's plaintext",
    );
}

#[test]
fn the_node_data_port_is_still_named_a_trust_boundary() {
    assert_says(
        TRUST_BOUNDARIES,
        "docs/trust-boundaries.md",
        "The node's data port is a trust boundary",
    );
}

#[test]
fn the_missing_per_tenant_key_separation_is_still_stated() {
    // The one claim an operator cannot check for themselves by reading the
    // configuration file, and the reason the boundaries above carry the whole
    // weight.
    assert_says(
        TRUST_BOUNDARIES,
        "docs/trust-boundaries.md",
        "Per-tenant key separation (envelope encryption) is not implemented in this release",
    );
}

#[test]
fn the_proxy_is_still_named_as_a_holder() {
    // deis-proxy is the internet-facing process, and its credential used to be
    // described by the one endpoint it calls. A document that stopped naming
    // it would be back to that half-truth.
    let document = unwrapped(TRUST_BOUNDARIES);
    assert!(
        document.contains("Compromise of the deis-proxy process"),
        "docs/trust-boundaries.md no longer says what a compromised deis-proxy \
         costs. It holds a node credential, so the answer is every tenant's \
         secrets and every node; put it back."
    );
}

#[test]
fn the_rotation_procedure_is_still_there() {
    // "Rotate regularly" without a procedure is advice nobody can follow. The
    // procedure only works because the control plane accepts every key in its
    // list while presenting the first, and a node accepts `[management] keys`
    // as well as `[control] secret` - so both are named.
    let document = unwrapped(TRUST_BOUNDARIES);
    for phrase in [
        "## Rotating the credential",
        "[management] keys",
        "node_preshared_keys",
    ] {
        assert!(
            document.contains(phrase),
            "docs/trust-boundaries.md no longer names \"{phrase}\" in its \
             rotation procedure; without it the procedure cannot be carried out"
        );
    }
}

#[test]
fn every_document_that_states_a_half_still_points_at_the_whole() {
    // Each of these says something true about the credential that is only a
    // part of what it opens. The pointer is what keeps a reader who arrives
    // there from concluding the part is the whole.
    for (name, document) in [
        ("README.md", README),
        ("docs/architecture.zh.md", ARCHITECTURE_ZH),
        (
            "crates/deis-control/API_DOCUMENTATION.md",
            API_DOCUMENTATION,
        ),
        ("crates/deis-control/DEPLOYMENT.md", DEPLOYMENT),
        ("crates/deis-control/ARCHITECTURE.md", CONTROL_ARCHITECTURE),
        ("crates/deis-proxy/README.md", PROXY_README),
        ("crates/deis-runtime/README.md", RUNTIME_README),
        ("crates/deis-wire/README.md", WIRE_README),
    ] {
        assert!(
            document.contains("trust-boundaries.md"),
            "{name} mentions the node credential and no longer links \
             docs/trust-boundaries.md. Either restore the link, or - if that \
             document is gone - this whole test file is what has to be revised."
        );
    }
}
