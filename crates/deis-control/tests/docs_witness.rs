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
//! Two of them were rewritten that way on 2026-09-11, when the fleet-wide
//! value stopped being what opens the assignment list and a per-machine
//! certificate took its place. Both sentences they held had become false. What
//! each was protecting had not: see the comments on those two tests for what
//! survived the change and why a weaker sentence would have been worse than no
//! sentence at all.
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

/// This test used to hold the sentence "the credential equals every tenant's
/// plaintext", about the fleet-wide shared value. That value no longer opens
/// the assignment list at all - a certificate naming one machine does - so the
/// sentence was rewritten in the same change that made it false.
///
/// What it was protecting did not change, and that is why there is still a
/// test here. The assignment list is still not scoped to the machine that
/// asked for it: `assignments_for` takes a `node_id`, uses it for log lines,
/// and returns the whole catalog. So one machine's private key still reads
/// every tenant's plaintext. The credential got narrower - it names a machine
/// now, and cannot be copied out of a configuration file - and what it opens
/// did not get narrower at all. A document that said "each machine has its own
/// identity" and stopped there would read as isolation, which this is not.
#[test]
fn one_machines_certificate_is_still_said_to_read_every_tenants_plaintext() {
    assert_says(
        TRUST_BOUNDARIES,
        "docs/trust-boundaries.md",
        "every tenant's plaintext, not only that node's own",
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

/// deis-proxy is the internet-facing process, and it used to hold the fleet
/// key - so a compromised proxy was every tenant's secrets and every node.
/// That is no longer what a bare `[control] secret` buys it, and the sentence
/// this test used to hold was rewritten with the change that made it false.
///
/// The reason a test stays here is that the tempting conclusion - "the proxy
/// is safe to put in a DMZ now" - is wrong. A proxy with a `[machine_identity]`
/// table holds a private key, and a private key on an internet-facing host is
/// a credential on an internet-facing host whatever shape it has. The document
/// has to keep saying which side of the boundary such a host is on.
#[test]
fn a_host_holding_a_private_key_is_still_named_as_inside_the_boundary() {
    let document = unwrapped(TRUST_BOUNDARIES);
    assert!(
        document.contains("proxy included, as inside the control plane's trust boundary"),
        "docs/trust-boundaries.md no longer says which side of the trust \
         boundary a machine holding a [machine_identity] private key is on. \
         The proxy is the one that matters: it faces the internet and it holds \
         a key. Put it back."
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
