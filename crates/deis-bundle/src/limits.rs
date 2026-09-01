//! The `limits` block of a manifest: what an application asks to be held to.
//!
//! An application declares its own ceilings the same way it declares its
//! routes, its cron expressions and the services it uses - in the manifest,
//! inside the bundle. The shape lives here, beside the manifest it is a field
//! of, because three components read it and they must agree exactly: the build
//! writes it out of the project's `limits`, the control plane checks it at
//! upload and carries it to every node with the deployment, and the node
//! clamps each field against its own configuration.
//!
//! **Narrowing only.** Everything here is a request to be held to *less* than
//! the node allows. `deis-runtime`'s `ApplicationManager::effective_limits`
//! clamps every value against the node's own ceiling, so a manifest asking for
//! an hour of CPU gets the node's number and a log line naming both, not an
//! hour. Nothing an application writes here can widen anything.
//!
//! Judging a value is therefore not this module's job, and neither is refusing
//! a greedy one: only what is *meaningless* is refused, by
//! [`DeclaredLimits::validate`], and only where somebody is watching - at
//! upload, in `deis-control`. What is merely greedy is left to the node that
//! has to pay for it.

use serde::{Deserialize, Serialize};

/// The `limits` object of a bundle manifest.
///
/// Every field is optional and absent means "this node's default". A field
/// present but zero is refused rather than treated as absent: on the wire zero
/// *is* the absent sentinel, so an application that wrote `"timeout_ms": 0`
/// would silently get the node default instead of the refusal it deserves for
/// asking for a budget nothing can run in.
///
/// A key this build does not know is **ignored**, not refused, and named at
/// `warn` when the bundle is read - see [`crate::forward`] for the rule. This
/// struct used to be `deny_unknown_fields`, so that a misspelled limit could
/// not parse and be silently dropped; the protection was real but it was
/// bought in the wrong currency. Serde's refusal is per document, so one key
/// an older build had never heard of threw away the whole manifest, and with
/// it the application's code, routes and bindings - on every node still on
/// the older build, with no re-upload able to repair it, because the manifest
/// is inside the archive whose digest is the version id. The misspelling is
/// still caught, closer to the author than a node ever was:
/// `deis_build_core::config::LimitsConfig` keeps `deny_unknown_fields`, so a
/// ceiling typed wrong in the project file is refused there, with the file
/// still on screen.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
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

    /// How many outbound calls one invocation of this application may make.
    ///
    /// Counted per invocation - one request and its `waitUntil` tail, one
    /// queue batch, one cron tick - and every hop counts: a `fetch()` that
    /// follows two redirects costs three, because three connections left the
    /// node. An application that knows it calls one upstream twice sets this
    /// to two and is told the moment a change makes it call ten.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_subrequests: Option<u32>,
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
        if let Some(max) = self.max_subrequests {
            parts.push(format!("{max} subrequests per invocation"));
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
    pub fn validate(&self) -> Result<(), String> {
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
        if self.max_subrequests == Some(0) {
            return Err(
                "limits.max_subrequests is 0; the application could not make a single outbound \
                        call. Leave it out to use the node's default"
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_subrequest_ceiling_is_described_and_a_zero_one_is_refused() {
        let limits = DeclaredLimits {
            max_subrequests: Some(4),
            ..Default::default()
        };
        assert_eq!(limits.describe(), "4 subrequests per invocation");
        assert_eq!(
            serde_json::to_string(&limits).unwrap(),
            r#"{"max_subrequests":4}"#
        );

        // Zero is the wire's spelling for "unset", so an application that
        // wrote it would silently get the node's number instead of the
        // refusal it deserves for asking to make no calls at all.
        let zero = DeclaredLimits {
            max_subrequests: Some(0),
            ..Default::default()
        };
        let err = zero.validate().expect_err("a zero ceiling must be refused");
        assert!(err.contains("max_subrequests"), "{err}");
    }

    #[test]
    fn describe_names_only_what_was_asked_for() {
        let limits = DeclaredLimits {
            cpu_time_limit_ms: Some(250),
            ..Default::default()
        };
        assert_eq!(limits.describe(), "cpu time 250ms");
    }

    #[test]
    fn an_absent_field_is_left_out_of_the_manifest_entirely() {
        let limits = DeclaredLimits {
            timeout_ms: Some(1500),
            ..Default::default()
        };
        assert_eq!(
            serde_json::to_string(&limits).unwrap(),
            r#"{"timeout_ms":1500}"#
        );
    }

    #[test]
    fn a_limit_this_build_does_not_know_is_ignored_and_named() {
        // The rollback case, at this struct's own level: a ceiling a newer
        // builder wrote must cost this build that one declaration and nothing
        // else. Refusing it here would have refused the manifest that carried
        // it, and with it the application.
        let limits: DeclaredLimits =
            serde_json::from_str(r#"{"timeout_ms":1500,"max_open_sockets":8}"#)
                .expect("a key this build does not read must not cost the block that carries it");
        assert_eq!(limits.timeout_ms, Some(1500));

        // Ignored is not unsaid. `crate::forward` is what names it, and a
        // misspelling reaches an author through the same line.
        assert_eq!(
            crate::forward::unknown_fields(
                r#"{"schema":1,"app":"d","entry":"e.js","limits":{"timeout_millis":1500}}"#
            ),
            vec!["limits.timeout_millis".to_string()]
        );
    }
}
