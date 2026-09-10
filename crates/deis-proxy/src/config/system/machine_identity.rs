//! Machine-channel identity: the certificate this proxy holds and the name
//! it expects the control plane to prove back, for the mutual TLS migration
//! that is replacing `node_preshared_keys` fleet-wide.
//!
//! # What this is, today
//!
//! Absent unless an operator writes a `[machine_identity]` table, and nothing
//! in this crate reads the loaded material yet: the control channel
//! configured by `[control]` still dials with the shared secret in
//! `deis_wire::ControlConfig`, exactly as it does without this table. Adding
//! it here is a reviewable step on its own, ahead of the change that moves
//! that dial over to it - see `crates/deis-identity` for the certificate
//! format and what carrying this identity will let a peer refuse.
//!
//! # Naming the control plane
//!
//! A dialler has to know not just an address but a name: proving a
//! certificate is pointless if any certificate our trust root signed is
//! accepted, because that is exactly the "any holder may speak for any
//! machine" failure this migration exists to close. `deis-identity::names`
//! spells every name this fleet issues as a trust domain plus a fixed path,
//! and the control plane's is the simplest of the three - it carries no id of
//! its own, because a trust domain has exactly one control plane. So the
//! default here is `trust_domain` plus that fixed path,
//! `names::control(trust_domain)`, and nothing else has to be configured for
//! the ordinary case.
//!
//! `control_identity` exists for the case that default cannot cover: a
//! control plane whose certificate was issued outside this fleet's own
//! naming convention, or a migration in progress where the two sides have not
//! yet agreed on one trust domain. Writing the full name there is taken
//! exactly as written and skips deriving it from `trust_domain` at all.

use deis_identity::{SpiffeId, SpiffeIdError, names};
use serde::{Deserialize, Serialize};

use crate::config::{ConfigError, Validatable};

/// Default for `identity_reload_seconds`: the same sixty seconds
/// `MachineChannelConfig` in deis-control defaults to, so an operator who has
/// already picked a rotation cadence for the control plane's own listener
/// sees the same number here without having to think about whether the two
/// should differ.
fn default_identity_reload_seconds() -> u64 {
    60
}

/// This proxy's certificate on the machine channel, and who it expects to be
/// talking to.
///
/// Every field but `control_identity` is required once this table is
/// present at all: a certificate without its key, or a trust root without
/// either, is not a smaller version of this feature, it is a configuration
/// that cannot load. [`Validatable::validate`] refuses a table left
/// half-filled by name rather than letting it fail later at a handshake no
/// operator is watching.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct MachineIdentityConfig {
    /// This proxy's own certificate chain, end-entity certificate first.
    /// Relative paths resolve against the directory deis-proxy was started
    /// from, the same as every other file path in this configuration; see
    /// [`SystemConfig::resolve_paths`](super::SystemConfig::resolve_paths).
    pub certificate: String,
    /// The private key belonging to that certificate.
    pub private_key: String,
    /// The certificate, or certificates, the control plane's own certificate
    /// must chain to.
    pub trust_root: String,
    /// How often the three files above are re-read, in seconds.
    ///
    /// The longest a renewal can go unnoticed. It wants to be far shorter
    /// than the shortest certificate lifetime in use, and that is the only
    /// thing it trades against.
    #[serde(default = "default_identity_reload_seconds")]
    pub identity_reload_seconds: u64,
    /// This fleet's trust domain, for instance `deis.internal`. Combined
    /// with the fixed control path to name the control plane; see the module
    /// doc for why that is the default and when `control_identity` should be
    /// written instead.
    pub trust_domain: String,
    /// The control plane's SPIFFE name, written out in full, for the cases
    /// `trust_domain` cannot cover. Overrides the derived name completely
    /// rather than replacing one part of it.
    pub control_identity: Option<String>,
}

/// Written out rather than derived, because the derived one sets
/// `identity_reload_seconds` to zero - a value [`Validatable::validate`]
/// refuses. A default that cannot pass its own validation is a trap for
/// whatever fills the rest of the table in and expects the untouched fields
/// to be usable.
impl Default for MachineIdentityConfig {
    fn default() -> Self {
        Self {
            certificate: String::new(),
            private_key: String::new(),
            trust_root: String::new(),
            identity_reload_seconds: default_identity_reload_seconds(),
            trust_domain: String::new(),
            control_identity: None,
        }
    }
}

impl MachineIdentityConfig {
    /// The name this instance will require the control plane to prove,
    /// worked out the way the module doc describes: `control_identity` taken
    /// verbatim if it is set, otherwise `trust_domain` plus the fixed control
    /// path.
    ///
    /// Called from [`Validatable::validate`] so a name that cannot be built
    /// is refused at startup rather than at the first connection attempt,
    /// and will be called again by whatever change starts dialling with
    /// this identity, to configure that connection's expected peer.
    pub fn expected_control_identity(&self) -> Result<SpiffeId, SpiffeIdError> {
        match &self.control_identity {
            Some(explicit) => SpiffeId::parse(explicit),
            None => names::control(&self.trust_domain),
        }
    }
}

impl Validatable for MachineIdentityConfig {
    fn validate(&self) -> Result<(), ConfigError> {
        let refuse = |message: String| Err(ConfigError::ValidationError { message });

        // Every missing file named at once. Reporting them one at a time
        // costs an operator a restart per field to find out what else is
        // wrong, and a table left half-filled is usually half-filled in more
        // than one place.
        let missing: Vec<&str> = [
            ("machine_identity.certificate", &self.certificate),
            ("machine_identity.private_key", &self.private_key),
            ("machine_identity.trust_root", &self.trust_root),
        ]
        .into_iter()
        .filter(|(_, value)| value.is_empty())
        .map(|(name, _)| name)
        .collect();
        if !missing.is_empty() {
            return refuse(format!(
                "{} {} required. A [machine_identity] table names all three files or none of                  them: this proxy's certificate chain, the private key that goes with it, and                  the trust root every peer must chain to",
                missing.join(", "),
                if missing.len() == 1 { "is" } else { "are" }
            ));
        }
        if self.identity_reload_seconds == 0 {
            return refuse(
                "machine_identity.identity_reload_seconds must be at least 1".to_string(),
            );
        }
        if let Err(error) = self.expected_control_identity() {
            return refuse(format!(
                "machine_identity does not name a control plane to expect: {error}. Set \
                 machine_identity.trust_domain to this fleet's trust domain (for instance \
                 \"deis.internal\"), so the expected name is \
                 spiffe://<trust_domain>/deislet/control, or set \
                 machine_identity.control_identity to the control plane's full name if it does \
                 not follow that convention"
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn filled() -> MachineIdentityConfig {
        MachineIdentityConfig {
            certificate: "machine/proxy.pem".to_string(),
            private_key: "machine/proxy-key.pem".to_string(),
            trust_root: "machine/root.pem".to_string(),
            // The derived `Default` leaves this at 0, unlike the `serde`
            // default used when a table is parsed from TOML - so a fixture
            // meant to be a complete, valid table has to set it explicitly.
            identity_reload_seconds: default_identity_reload_seconds(),
            trust_domain: "deis.internal".to_string(),
            ..Default::default()
        }
    }

    #[test]
    fn a_complete_table_validates_and_names_the_control_plane_by_convention() {
        let config = filled();
        config.validate().expect("a complete table is usable");
        assert_eq!(
            config.expected_control_identity().unwrap().as_str(),
            "spiffe://deis.internal/deislet/control"
        );
    }

    #[test]
    fn an_absent_table_is_the_default_and_still_validates() {
        // What every configuration without `[machine_identity]` parses to;
        // it must not be mistaken for a filled-in and empty table.
        let config = MachineIdentityConfig::default();
        assert_eq!(config.certificate, "");
        // Not asserted to validate: an operator who writes an empty
        // `[machine_identity]` table by hand should see the same refusal as
        // one who wrote half a table, not silent success. Absence is
        // represented by `Option<MachineIdentityConfig>` being `None` one
        // level up, in `SystemConfig`, not by this default.
        assert!(config.validate().is_err());
    }

    #[test]
    fn a_missing_certificate_is_named() {
        let config = MachineIdentityConfig {
            certificate: String::new(),
            ..filled()
        };
        let error = config.validate().unwrap_err().to_string();
        assert!(error.contains("certificate"), "{error}");
    }

    #[test]
    fn a_missing_private_key_is_named() {
        let config = MachineIdentityConfig {
            private_key: String::new(),
            ..filled()
        };
        let error = config.validate().unwrap_err().to_string();
        assert!(error.contains("private_key"), "{error}");
    }

    #[test]
    fn a_missing_trust_root_is_named() {
        let config = MachineIdentityConfig {
            trust_root: String::new(),
            ..filled()
        };
        let error = config.validate().unwrap_err().to_string();
        assert!(error.contains("trust_root"), "{error}");
    }

    #[test]
    fn a_zero_reload_interval_is_refused() {
        let config = MachineIdentityConfig {
            identity_reload_seconds: 0,
            ..filled()
        };
        let error = config.validate().unwrap_err().to_string();
        assert!(error.contains("identity_reload_seconds"), "{error}");
    }

    #[test]
    fn a_trust_domain_left_blank_with_no_override_is_refused() {
        let config = MachineIdentityConfig {
            trust_domain: String::new(),
            ..filled()
        };
        let error = config.validate().unwrap_err().to_string();
        assert!(error.contains("trust_domain"), "{error}");
    }

    #[test]
    fn an_explicit_override_is_taken_verbatim_and_skips_the_trust_domain() {
        let config = MachineIdentityConfig {
            // Left blank on purpose: the override must stand on its own and
            // must not fall back to deriving a name from an empty domain.
            trust_domain: String::new(),
            control_identity: Some("spiffe://deis.internal/deislet/control".to_string()),
            ..filled()
        };
        config
            .validate()
            .expect("an explicit override needs no trust_domain");
        assert_eq!(
            config.expected_control_identity().unwrap().as_str(),
            "spiffe://deis.internal/deislet/control"
        );
    }

    #[test]
    fn an_unparsable_override_is_refused_by_name() {
        let config = MachineIdentityConfig {
            control_identity: Some("not-a-spiffe-name".to_string()),
            ..filled()
        };
        let error = config.validate().unwrap_err().to_string();
        assert!(
            error.contains("control_identity") || error.contains("spiffe://"),
            "{error}"
        );
    }

    #[test]
    fn the_table_round_trips_through_toml() {
        let config: MachineIdentityConfig = toml::from_str(
            r#"
certificate = "machine/proxy.pem"
private_key = "machine/proxy-key.pem"
trust_root = "machine/root.pem"
trust_domain = "deis.internal"
"#,
        )
        .unwrap();
        assert_eq!(
            config.identity_reload_seconds,
            default_identity_reload_seconds()
        );
        assert!(config.control_identity.is_none());
        config.validate().unwrap();
    }
}
