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
//! A declaration this build cannot honour refuses the upload, and refuses the
//! delivery if a version stored by a *newer* build carries one. There is
//! exactly one of those left, and it is the date: a date past
//! [`deis_bundle::PLATFORM_KNOWN_THROUGH`] asks for the complete set of
//! answers as of a day this build has never seen, and a build that served a
//! subset would be giving an application semantics it did not ask for with
//! nothing downstream to say so.
//!
//! A flag name this build does not know is **not** one of those and is
//! ignored, at `warn`, by [`deis_bundle::CompatibilityProfile::resolve`]. It
//! used to refuse, and that refusal landed here twice over - at the upload and
//! again at every delivery - so a control plane one release behind the builder
//! could not deliver any version naming a flag added since, and the manifest
//! is inside the archive whose digest is the version id, so nothing could
//! repair one. `deis_bundle::forward` carries the rule and the reasoning. The
//! author's own misspelling is still refused, at the build, by
//! `DeclaredCompatibility::validate_as_written`.
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
        let json = deis_bundle::complete_manifest_json(&format!(
            r#"{{"app":"demo","entry":"worker/index.js","compatibility":{block}}}"#
        ));
        let manifest = Manifest::parse(&json).map_err(|e| e.to_string())?;
        declared_compatibility(&manifest)
    }

    #[test]
    fn a_manifest_without_the_block_is_held_to_the_oldest_behaviour() {
        let manifest = Manifest::parse(&deis_bundle::complete_manifest_json(
            r#"{"app":"demo","entry":"worker/index.js"}"#,
        ))
        .unwrap();
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

    /// The mirror of the test above, and the reason the two are next to each
    /// other: a date this build cannot honour is refused, a flag it does not
    /// know is not. A flag names one behaviour this build does not have in
    /// either direction, so ignoring it gives exactly what this build was
    /// already giving; refusing it took the whole version away from every
    /// component one release behind the builder that wrote it.
    #[test]
    fn a_flag_nobody_knows_is_ignored_rather_than_refused() {
        let profile = read(r#"{"date":"2026-09-01","flags":["turbo_cookies"]}"#)
            .expect("a flag from a newer builder must not cost the version");
        assert_eq!(
            profile.date().map(|d| d.to_string()).as_deref(),
            Some("2026-09-01"),
            "everything this build does read is still in force"
        );
        assert!(profile.is_on(Flag::MultipleSetCookieHeaders));
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
