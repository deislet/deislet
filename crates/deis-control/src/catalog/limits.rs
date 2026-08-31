//! What an application asks for in its bundle manifest's `limits` block.
//!
//! The shape itself is [`deis_bundle::DeclaredLimits`], next to the manifest
//! it is a field of - the build writes it, this crate checks it, a node
//! applies it, and three copies of one schema would drift. What lives here is
//! the *gate*: the control plane reads the block out of the verbatim manifest
//! text it stored with the version, refuses one no node could act on, and
//! carries the rest to every node with the deployment - on the
//! `ApplyDeployment` push and in the assignment list - so that a node never
//! has to open a bundle to find out what its ceilings are.
//!
//! **Narrowing only.** A node clamps every value against its own
//! configuration (`deis-runtime`'s `ApplicationManager::effective_limits`), so
//! a manifest asking for an hour of CPU gets the node's ceiling and a log line
//! naming it, not an hour. Nothing an application writes here can widen
//! anything; this module refuses only what is *meaningless*, and leaves what
//! is merely greedy to the node that has to pay for it.
//!
//! The limits reach a node in two shapes and neither of them is this struct:
//! the gRPC push carries `deis.management.ResourceLimits`, and the assignment
//! list carries this struct's JSON. Both spell the same field names, which is
//! why the assignment list can serialise this type directly.

pub use deis_bundle::DeclaredLimits;
use deis_bundle::Manifest;

/// Read the `limits` block a manifest carries, and refuse one no node could
/// act on.
///
/// `Ok(None)` means the manifest declares none, which is what almost every
/// bundle does and is not an error. `Err` carries a message meant for whoever
/// uploaded the bundle: it is returned at upload time so a version that no
/// node could honour never becomes deployable.
///
/// Spelling is already settled by the time this runs -
/// [`deis_bundle::DeclaredLimits`] denies unknown fields, so a misspelled
/// limit fails the manifest parse rather than being dropped in silence. What
/// is left for here is meaning: a budget of zero, a rate that is not a
/// number. Those parse perfectly and are the ones a node would have to
/// discard on every load.
pub fn declared_limits(manifest: &Manifest) -> Result<Option<DeclaredLimits>, String> {
    let Some(limits) = manifest.limits.clone() else {
        return Ok(None);
    };
    limits.validate()?;
    if limits.is_empty() {
        // An empty object is not a declaration; treating it as one would put a
        // "declares limits" line in the log of every delivery that says
        // nothing.
        return Ok(None);
    }
    Ok(Some(limits))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Everything here goes through the real manifest parse, because that is
    /// where a misspelled key is now caught and this module only sees what
    /// survived it.
    fn read(limits: &str) -> Result<Option<DeclaredLimits>, String> {
        let json =
            format!(r#"{{"schema":1,"app":"demo","entry":"worker/index.js","limits":{limits}}}"#);
        let manifest = Manifest::parse(&json).map_err(|e| e.to_string())?;
        declared_limits(&manifest)
    }

    #[test]
    fn a_manifest_without_a_limits_block_declares_nothing() {
        let manifest =
            Manifest::parse(r#"{"schema":1,"app":"demo","entry":"worker/index.js"}"#).unwrap();
        assert_eq!(declared_limits(&manifest), Ok(None));
    }

    #[test]
    fn the_fields_an_application_can_narrow_are_all_read() {
        let limits = read(
            r#"{"memory_mb":32,"timeout_ms":1500,"cpu_time_limit_ms":250,
                "rate_limit_ops_per_sec":50.5,"resource_table_max":16}"#,
        )
        .expect("valid")
        .expect("declared");
        assert_eq!(limits.memory_mb, Some(32));
        assert_eq!(limits.timeout_ms, Some(1500));
        assert_eq!(limits.cpu_time_limit_ms, Some(250));
        assert_eq!(limits.rate_limit_ops_per_sec, Some(50.5));
        assert_eq!(limits.resource_table_max, Some(16));
    }

    /// The failure this whole module exists to make impossible: a limit that
    /// looks set, is not, and is discovered only when the ceiling an operator
    /// thought was in force turns out never to have been. The manifest parse
    /// is what refuses it now, which means a bundle carrying one never even
    /// reaches an upload handler.
    #[test]
    fn a_misspelled_limit_is_refused_rather_than_dropped() {
        let err = read(r#"{"timeout_millis":1500}"#)
            .expect_err("a key nobody reads must not be accepted");
        assert!(err.contains("timeout_millis"), "{err}");
    }

    #[test]
    fn a_zero_is_refused_because_zero_is_the_absent_sentinel_on_the_wire() {
        for (field, value) in [
            ("memory_mb", "0"),
            ("timeout_ms", "0"),
            ("cpu_time_limit_ms", "0"),
            ("resource_table_max", "0"),
            ("rate_limit_ops_per_sec", "0"),
        ] {
            let err = read(&format!(r#"{{"{field}":{value}}}"#))
                .expect_err("a zero limit must be refused, not silently treated as unset");
            assert!(err.contains(field), "{field}: {err}");
        }
    }

    #[test]
    fn an_empty_limits_object_is_the_same_as_declaring_none() {
        assert_eq!(read("{}"), Ok(None));
    }

    /// A `limits` that is not an object at all never reaches this module:
    /// the manifest parse refuses it, naming the shape it wanted.
    #[test]
    fn a_limits_block_that_is_not_an_object_is_refused() {
        let err = read("\"tight\"").expect_err("a string is not a limits block");
        assert!(err.contains("DeclaredLimits"), "{err}");
    }

    #[test]
    fn describe_names_only_what_was_asked_for() {
        let limits = DeclaredLimits {
            cpu_time_limit_ms: Some(250),
            ..Default::default()
        };
        assert_eq!(limits.describe(), "cpu time 250ms");
    }
}
