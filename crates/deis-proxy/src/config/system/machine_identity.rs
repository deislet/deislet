//! Machine-channel identity: the certificate this proxy holds and the name
//! it expects the control plane to prove back.
//!
//! # What this is
//!
//! Everything this proxy says to a control plane goes over the machine
//! channel under this certificate, and the control plane proves its own name
//! back on the same connection. Two things ride on it: the catalog's routing
//! table, fetched as a `FetchRoutingTable` call (see
//! [`crate::catalog::client`]), and the long-lived gRPC control channel -
//! registration, heartbeats, commands, configuration pushes (see
//! [`crate::control::client`]).
//!
//! Both used to have another way. The routing table was an HTTP GET on the
//! control plane's human listener with a fleet-wide value in the
//! `Authorization` header, and the control channel was a plaintext dial of
//! `[control] addr` presenting the same value. `[control] secret` is gone with
//! it: the control plane admits a channel by the name the connection proved,
//! and there is nothing a caller can bring instead.
//!
//! Absent, this table means standalone: no control plane is reached at all,
//! and this proxy serves whatever its own file routes. A configuration that
//! names `[control]` without this table is refused at startup rather than
//! started into a state where nothing could ever attach. See
//! `crates/deis-identity` for the certificate format and what carrying this
//! identity lets a peer refuse.
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
//!
//! # Where the address is written, and why not beside the other one
//!
//! The machine channel is a listener of its own on a port of its own, so
//! `[control] addr` - the human listener, which is still where a proxy
//! without this table goes for everything - does not say where it is.
//! Somewhere has to, and `control_endpoint` below is it.
//!
//! Next to the identity, because an address and the name that must be proved
//! at it are one fact rather than two. A machine-channel address with no
//! certificate to present is a connection that will be closed at the
//! handshake; a certificate with no address is material that will never be
//! used. Both are half a configuration and both have to be refused at
//! startup, and keeping them in one table makes that one refusal in one
//! [`Validatable::validate`] that can name the field to fix.
//!
//! deis-runtime configures the same thing and put it in the other place, as
//! `[control] machine_endpoint` beside its own control endpoint. The
//! difference is not a disagreement about which is better: `[control]` in
//! this file is not this crate's type at all. It is
//! `deis_wire::ControlConfig`, shared with deis-control and with every
//! process that speaks the control channel, and carrying `grpc_endpoint()`,
//! which builds the URL for *that* channel. A field only deis-proxy reads
//! does not belong in it. If the two are ever to be spelled identically, the
//! move is to put `machine_endpoint` into `deis_wire::ControlConfig` and have
//! both crates read it from there.
//!
//! What does not differ, and must not, is the value: the same
//! `https://host:port` an operator writes for a node, refused for the same
//! reasons - see `check_control_endpoint`.

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
    /// Where the control plane's machine channel listens, as
    /// `https://host:port` - the address matching that control plane's
    /// `[machine] bind_addr` and `[machine] port`, which is a different port
    /// from `[control] addr`.
    ///
    /// Required whenever this table is present; see the module doc for why it
    /// is stated here rather than beside the control channel's own address,
    /// and `check_control_endpoint` for what is refused.
    ///
    /// Written exactly as deis-runtime's `control.machine_endpoint` is
    /// written, down to the scheme and the mandatory port, because it is the
    /// same listener: an operator pointing a node and a proxy at one control
    /// plane copies one string into two files.
    pub control_endpoint: String,
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
            control_endpoint: String::new(),
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

/// Refuse a `control_endpoint` that is not an `https://host:port` URL, saying
/// which shape was written and which one is wanted.
///
/// The three refusals are the three mistakes that are actually made here, and
/// each one is silent later if it is not caught now: leaving the field out
/// while filling the rest of the table in, spelling the endpoint `http://`
/// because that is what `[control] addr` says, and leaving the port off
/// because every other address in a configuration file has a default.
///
/// These are the same three rules, in the same order, that deis-runtime holds
/// its `control.machine_endpoint` to. One listener, one spelling.
fn check_control_endpoint(endpoint: &str) -> Result<(), String> {
    let endpoint = endpoint.trim();
    if endpoint.is_empty() {
        return Err(
            "machine_identity.control_endpoint is required. It is where the control plane's \
             machine channel listens, written as https://host:port - the address matching that \
             control plane's [machine] bind_addr and [machine] port, which is a different port \
             from [control] addr. A certificate with no machine to present it to would be \
             loaded, logged and never used"
                .to_string(),
        );
    }
    let authority = match endpoint.strip_prefix("https://") {
        Some(authority) => authority,
        None => {
            return Err(format!(
                "machine_identity.control_endpoint {endpoint:?} is not an https:// URL. The \
                 machine channel demands a certificate from both ends on every route it carries, \
                 so there is no plaintext spelling of it that reaches anything; an http:// here \
                 would read as a way to turn the certificates off, and is refused rather than \
                 quietly upgraded"
            ));
        }
    };
    // The path, if one was written, is not part of the address dialled; a
    // gRPC method supplies its own.
    let authority = authority.split(['/', '?', '#']).next().unwrap_or(authority);
    // An IPv6 literal is bracketed, so the port is what follows the bracket;
    // anything else splits on the last colon.
    let port = match authority.strip_prefix('[') {
        Some(rest) => match rest.split_once(']') {
            Some((host, port)) if !host.is_empty() => port.strip_prefix(':'),
            _ => {
                return Err(format!(
                    "machine_identity.control_endpoint {endpoint:?} opens an IPv6 literal it \
                     never closes"
                ));
            }
        },
        None => match authority.rsplit_once(':') {
            Some((host, port)) if !host.is_empty() => Some(port),
            _ => None,
        },
    };
    match port {
        Some(port) if port.parse::<u16>().is_ok() => Ok(()),
        Some(port) => Err(format!(
            "machine_identity.control_endpoint {endpoint:?} names {port:?} where a port number \
             belongs"
        )),
        None => Err(format!(
            "machine_identity.control_endpoint {endpoint:?} names no port. This channel has no \
             default to fall back on - it is not the web, and guessing 443 would dial whatever \
             else happens to be listening there. Write the port the control plane's [machine] \
             port is set to"
        )),
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
        // The other half of the same rule the missing-file check enforces. An
        // identity is material for proving who this proxy is to one named
        // machine; without an address it has no machine to prove it to, and
        // the process would start, load the certificate, log that it did, and
        // then fetch no routing table at all - serving whatever its own file
        // routes while looking like a proxy that had attached.
        if let Err(reason) = check_control_endpoint(&self.control_endpoint) {
            return refuse(reason);
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
            control_endpoint: "https://control.internal:8443".to_string(),
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
control_endpoint = "https://control.internal:8443"
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

    #[test]
    fn an_identity_with_nowhere_to_dial_is_refused_by_name() {
        // The failure this catches is silent otherwise: the certificate
        // loads, the log says so, and no routing table is ever fetched,
        // because there is no address to fetch it from and no second way to
        // reach a control plane left to fall back to.
        let config = MachineIdentityConfig {
            control_endpoint: String::new(),
            ..filled()
        };
        let error = config.validate().unwrap_err().to_string();
        assert!(
            error.contains("machine_identity.control_endpoint"),
            "{error}"
        );
    }

    #[test]
    fn an_address_with_nothing_to_prove_is_refused_by_name() {
        // The mirror image, and the reason the address lives in this table:
        // one validate() sees both halves, so neither can be written alone.
        let config = MachineIdentityConfig {
            certificate: String::new(),
            private_key: String::new(),
            trust_root: String::new(),
            control_endpoint: "https://control.internal:8443".to_string(),
            ..filled()
        };
        let error = config.validate().unwrap_err().to_string();
        assert!(error.contains("machine_identity.certificate"), "{error}");
    }

    #[test]
    fn a_plaintext_control_endpoint_is_refused_rather_than_quietly_upgraded() {
        // Copying `[control] addr` across is the mistake this catches, and
        // `http://` must not read as a way to turn the certificates off.
        for written in ["http://control.internal:8443", "control.internal:8443"] {
            let config = MachineIdentityConfig {
                control_endpoint: written.to_string(),
                ..filled()
            };
            let error = config.validate().unwrap_err().to_string();
            assert!(error.contains("https://"), "{written}: {error}");
        }
    }

    #[test]
    fn a_control_endpoint_without_a_port_is_refused_rather_than_defaulted() {
        let config = MachineIdentityConfig {
            control_endpoint: "https://control.internal".to_string(),
            ..filled()
        };
        let error = config.validate().unwrap_err().to_string();
        assert!(error.contains("no port"), "{error}");
    }

    #[test]
    fn a_bracketed_ipv6_control_endpoint_is_usable() {
        let config = MachineIdentityConfig {
            control_endpoint: "https://[::1]:8443".to_string(),
            ..filled()
        };
        config.validate().expect("an IPv6 literal names a machine");

        let unclosed = MachineIdentityConfig {
            control_endpoint: "https://[::1:8443".to_string(),
            ..filled()
        };
        assert!(unclosed.validate().is_err());
    }

    #[test]
    fn a_control_endpoint_whose_port_is_not_a_number_is_refused() {
        let config = MachineIdentityConfig {
            control_endpoint: "https://control.internal:https".to_string(),
            ..filled()
        };
        let error = config.validate().unwrap_err().to_string();
        assert!(error.contains("where a port number belongs"), "{error}");
    }
}
