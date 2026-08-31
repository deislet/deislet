//! Which day's platform a version was written against.
//!
//! The shape and the table of behaviours are
//! [`deis_bundle::compatibility`], next to the manifest they are a field of -
//! the build writes the block, this crate checks it, a node resolves it, and
//! four copies of one table would drift. What lives here is the *gate*: the
//! control plane reads the block out of the verbatim manifest text it stored
//! with the version, refuses one this build cannot honour, and carries the
//! rest to every node with the deployment - on the `ApplyDeployment` push and
//! in the assignment list - so that a node never has to open a bundle to find
//! out which answers an application expects.
//!
//! Unlike the `limits` block next door, a declaration that cannot be read is
//! **not** degraded to "declares nothing" here. The two failures are not the
//! same kind: a limit nobody can read costs the application the node's own
//! ceiling, which is never wider than what it asked for, while a compatibility
//! declaration nobody can read would leave every component guessing which
//! answers to give - which is the exact silent behaviour change the block
//! exists to prevent. So an unreadable one refuses the upload, and refuses the
//! delivery if a version stored by an older build somehow carries one.
//!
//! An application that declares **nothing** is a different case and is
//! allowed: it predates the mechanism, it is held to the oldest behaviour the
//! platform has, and the only thing owed to an operator is that this is never
//! quiet. That is why [`describe_version`] exists and why every upload and
//! every delivery of an undated version writes a line naming it.

use deis_bundle::Manifest;
pub use deis_bundle::{CompatibilityProfile, DeclaredCompatibility};

/// Read the `compatibility` block a manifest carries and resolve it.
///
/// The one reader in this crate. `Ok` carries the resolved answers - an
/// application that declared nothing resolves to the oldest behaviour, which
/// is not an error and is what almost every existing bundle does. `Err`
/// carries a message meant for whoever uploaded the bundle: a flag name this
/// build does not know, a contradiction between two flags, or a date later
/// than the newest one this build can honour.
pub fn declared_compatibility(manifest: &Manifest) -> Result<CompatibilityProfile, String> {
    manifest.compatibility_profile()
}

/// One line for an operator, in the two registers a version can be in.
///
/// Written into the upload log and the delivery log, so that "which of my
/// applications have no date" can be answered from the control plane's own
/// output as well as from `deis versions`.
pub fn describe_version(profile: &CompatibilityProfile) -> String {
    match profile.date() {
        Some(_) => profile.describe(),
        None => format!(
            "{}; it will never receive a behaviour change this platform makes from here on. \
             Add \"compatibilityDate\" to the project and redeploy to opt in",
            profile.describe()
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use deis_bundle::Flag;

    /// Everything here goes through the real manifest parse, because that is
    /// where a malformed date and an unknown key are caught and this module
    /// only ever sees what survived it.
    fn read(block: &str) -> Result<CompatibilityProfile, String> {
        let json = format!(
            r#"{{"schema":1,"app":"demo","entry":"worker/index.js","compatibility":{block}}}"#
        );
        let manifest = Manifest::parse(&json).map_err(|e| e.to_string())?;
        declared_compatibility(&manifest)
    }

    #[test]
    fn a_manifest_without_the_block_is_held_to_the_oldest_behaviour() {
        let manifest =
            Manifest::parse(r#"{"schema":1,"app":"demo","entry":"worker/index.js"}"#).unwrap();
        let profile = declared_compatibility(&manifest).unwrap();
        assert_eq!(profile.date(), None);
        assert!(!profile.is_on(Flag::MultipleSetCookieHeaders));
    }

    #[test]
    fn a_dated_manifest_gets_the_behaviours_of_its_day() {
        let profile = read(r#"{"date":"2026-09-01"}"#).expect("a date this build knows");
        assert_eq!(
            profile.date().map(|d| d.to_string()).as_deref(),
            Some("2026-09-01")
        );
        assert!(profile.is_on(Flag::MultipleSetCookieHeaders));
    }

    #[test]
    fn a_flag_holds_one_behaviour_back_past_the_date() {
        let profile = read(r#"{"date":"2026-09-01","flags":["single_set_cookie_header"]}"#)
            .expect("a named flag is a declaration, not an error");
        assert!(!profile.is_on(Flag::MultipleSetCookieHeaders));
    }

    /// The refusal that matters most: a bundle asking for semantics this build
    /// does not have must never be accepted, because there is no honest answer
    /// to give it afterwards.
    #[test]
    fn a_date_this_build_cannot_honour_is_refused() {
        let err = read(r#"{"date":"2099-01-01"}"#).expect_err("a future date must be refused");
        assert!(err.contains("2099-01-01"), "{err}");
        assert!(
            err.contains(&deis_bundle::PLATFORM_KNOWN_THROUGH.to_string()),
            "{err}"
        );
    }

    #[test]
    fn a_flag_nobody_knows_is_refused_rather_than_ignored() {
        let err = read(r#"{"flags":["turbo_cookies"]}"#)
            .expect_err("a flag nothing reads must not be accepted");
        assert!(err.contains("turbo_cookies"), "{err}");
    }

    #[test]
    fn an_undated_version_is_described_as_frozen_not_as_current() {
        let profile = CompatibilityProfile::default();
        let line = describe_version(&profile);
        assert!(line.contains("no date"), "{line}");
        assert!(line.contains("compatibilityDate"), "{line}");
    }

    #[test]
    fn a_dated_version_is_described_by_its_date() {
        let profile = read(r#"{"date":"2026-09-01"}"#).unwrap();
        assert_eq!(
            describe_version(&profile),
            "2026-09-01, multiple_set_cookie_headers"
        );
    }
}
