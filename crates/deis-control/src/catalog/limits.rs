//! What an application asks for in its bundle manifest's `limits` block.
//!
//! An application declares its own ceilings the same way it declares its
//! routes, its cron expressions and the services it uses: in `manifest.json`,
//! inside the bundle. The control plane reads them here, once, out of the
//! verbatim manifest text it stored with the version, and carries them to
//! every node with the deployment - on the `ApplyDeployment` push and in the
//! assignment list - so that a node never has to open a bundle to find out
//! what its ceilings are, and so that there is exactly one parser for this
//! block on the platform.
//!
//! **Narrowing only.** Everything in here is a request to be held to *less*
//! than the node allows. A node clamps every value against its own
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

use serde::{Deserialize, Serialize};

/// The `limits` object of a bundle manifest.
///
/// Every field is optional and absent means "this node's default". A field
/// present but zero is refused rather than treated as absent: on the wire zero
/// *is* the absent sentinel, so an application that wrote `"timeout_ms": 0`
/// would silently get the node default instead of the refusal it deserves for
/// asking for a budget nothing can run in.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DeclaredLimits {
    /// V8 heap ceiling for this application's isolates, in MB.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_mb: Option<u32>,

    /// Wall-clock ceiling for one invocation, in milliseconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,

    /// Ceiling on the rate at which this application's code may dispatch ops.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_limit_ops_per_sec: Option<f64>,

    /// CPU-time ceiling for one invocation, in milliseconds. The one limit
    /// that measures what the node actually pays for a busy handler.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu_time_limit_ms: Option<u64>,

    /// How many entries this application's isolate may hold in its resource
    /// table at once.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resource_table_max: Option<u32>,
}

/// Only the part of a manifest this module cares about.
///
/// Deliberately not `deny_unknown_fields`: a manifest carries a dozen other
/// keys, and this envelope exists purely to reach into `limits`. The
/// strictness lives one level down, on [`DeclaredLimits`], where a misspelled
/// limit would otherwise be dropped without a word and leave the application
/// running on node defaults it thought it had narrowed.
#[derive(Deserialize)]
struct LimitsEnvelope {
    #[serde(default)]
    limits: Option<DeclaredLimits>,
}

impl DeclaredLimits {
    /// True when the application declared nothing at all, which is the usual
    /// case and is not worth a log line of its own.
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }

    /// A one-line rendering for an operator's log: only the fields the
    /// application actually asked for.
    pub fn describe(&self) -> String {
        let mut parts: Vec<String> = Vec::new();
        if let Some(mb) = self.memory_mb {
            parts.push(format!("memory {mb}MB"));
        }
        if let Some(ms) = self.timeout_ms {
            parts.push(format!("timeout {ms}ms"));
        }
        if let Some(ms) = self.cpu_time_limit_ms {
            parts.push(format!("cpu time {ms}ms"));
        }
        if let Some(rate) = self.rate_limit_ops_per_sec {
            parts.push(format!("{rate} ops/sec"));
        }
        if let Some(max) = self.resource_table_max {
            parts.push(format!("{max} resource-table entries"));
        }
        if parts.is_empty() {
            "nothing".to_string()
        } else {
            parts.join(", ")
        }
    }

    /// Refuse a declaration no node could act on.
    ///
    /// Not a policy check - a node decides whether a value is acceptable, and
    /// narrows it if it is not. This only rejects values that have no reading
    /// at all, so that they are caught when a version is uploaded rather than
    /// silently discarded on every node that loads it.
    fn validate(&self) -> Result<(), String> {
        if self.memory_mb == Some(0) {
            return Err(
                "limits.memory_mb is 0; an isolate with no heap cannot run a line of \
                        code. Leave it out to use the node's default"
                    .to_string(),
            );
        }
        if self.timeout_ms == Some(0) {
            return Err(
                "limits.timeout_ms is 0; every invocation would be cut off before it \
                        started. Leave it out to use the node's default"
                    .to_string(),
            );
        }
        if self.cpu_time_limit_ms == Some(0) {
            return Err(
                "limits.cpu_time_limit_ms is 0; every invocation would be cut off before \
                        it ran an instruction. Leave it out to use the node's default"
                    .to_string(),
            );
        }
        if self.resource_table_max == Some(0) {
            return Err(
                "limits.resource_table_max is 0; the application could not open a single \
                        resource. Leave it out to use the node's default"
                    .to_string(),
            );
        }
        if let Some(rate) = self.rate_limit_ops_per_sec
            && !(rate.is_finite() && rate > 0.0)
        {
            return Err(format!(
                "limits.rate_limit_ops_per_sec is {rate}; it must be a positive number of ops \
                 per second. Leave it out to use the node's default"
            ));
        }
        Ok(())
    }
}

/// Read the `limits` block out of a manifest's verbatim text.
///
/// `Ok(None)` means the manifest declares none, which is what almost every
/// bundle does and is not an error. `Err` carries a message meant for whoever
/// uploaded the bundle: it is returned at upload time so a version that no
/// node could honour never becomes deployable.
///
/// The manifest as a whole is parsed elsewhere (`deis_bundle::Manifest`), which
/// ignores this key. Reading it separately here is what lets the limits land
/// without a bundle-format change; the field belongs in `deis_bundle::Manifest`
/// itself, and this parser goes away when it gets there.
pub fn declared_limits(manifest_json: &str) -> Result<Option<DeclaredLimits>, String> {
    let envelope: LimitsEnvelope = serde_json::from_str(manifest_json)
        .map_err(|e| format!("manifest \"limits\" block is not readable: {e}"))?;
    let Some(limits) = envelope.limits else {
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

    fn manifest_with(limits: &str) -> String {
        format!(r#"{{"schema":1,"app":"demo","entry":"worker/index.js","limits":{limits}}}"#)
    }

    #[test]
    fn a_manifest_without_a_limits_block_declares_nothing() {
        let json = r#"{"schema":1,"app":"demo","entry":"worker/index.js"}"#;
        assert_eq!(declared_limits(json), Ok(None));
    }

    #[test]
    fn the_fields_an_application_can_narrow_are_all_read() {
        let json = manifest_with(
            r#"{"memory_mb":32,"timeout_ms":1500,"cpu_time_limit_ms":250,
                "rate_limit_ops_per_sec":50.5,"resource_table_max":16}"#,
        );
        let limits = declared_limits(&json).expect("valid").expect("declared");
        assert_eq!(limits.memory_mb, Some(32));
        assert_eq!(limits.timeout_ms, Some(1500));
        assert_eq!(limits.cpu_time_limit_ms, Some(250));
        assert_eq!(limits.rate_limit_ops_per_sec, Some(50.5));
        assert_eq!(limits.resource_table_max, Some(16));
    }

    /// The failure this whole module exists to make impossible: a limit that
    /// looks set, is not, and is discovered only when the ceiling an operator
    /// thought was in force turns out never to have been.
    #[test]
    fn a_misspelled_limit_is_refused_rather_than_dropped() {
        let json = manifest_with(r#"{"timeout_millis":1500}"#);
        let err = declared_limits(&json).expect_err("a key nobody reads must not be accepted");
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
            let json = manifest_with(&format!(r#"{{"{field}":{value}}}"#));
            let err = declared_limits(&json)
                .expect_err("a zero limit must be refused, not silently treated as unset");
            assert!(err.contains(field), "{field}: {err}");
        }
    }

    #[test]
    fn an_empty_limits_object_is_the_same_as_declaring_none() {
        let json = manifest_with("{}");
        assert_eq!(declared_limits(&json), Ok(None));
    }

    #[test]
    fn a_limits_block_that_is_not_an_object_is_refused() {
        let json = manifest_with("\"tight\"");
        let err = declared_limits(&json).expect_err("a string is not a limits block");
        assert!(err.contains("limits"), "{err}");
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
