//! How this instance gets the catalog's routing table, and whether it
//! offers an automatic address per environment.
//!
//! These belong in the local file and nowhere else. Two of them point at the
//! control plane, which by the same rule as `[control]` itself must not be
//! settable by the control plane; the third is a fact about DNS and about
//! which certificate this edge holds, so the operator who created the
//! wildcard record is the one who writes it.

use serde::{Deserialize, Serialize};

use crate::catalog::PreviewZone;
use crate::config::{ConfigError, Validatable};

/// Seconds between two reads of the control plane's routing table.
pub const DEFAULT_CONTROL_POLL_INTERVAL_SECS: u64 = 5;

/// Largest routing table this instance will read into memory.
///
/// Roughly one line of JSON per route, so the default is several tens of
/// thousands of routes - far past anything a private platform has. It exists
/// because the body arrives before it can be judged, and an unbounded read
/// from a compromised or broken control plane would be an easy way to take
/// the edge down.
pub const DEFAULT_MAX_TABLE_BYTES: usize = 8 * 1024 * 1024;

/// Below this a table could not carry a single route, so a value under it is
/// a mistake rather than a tight limit.
const MIN_MAX_TABLE_BYTES: usize = 1024;

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct RoutingConfig {
    /// Seconds between polls of `GET /api/routing-table`. Lower means a new
    /// route takes effect sooner and costs one small request more often.
    pub control_poll_interval_secs: Option<u64>,
    /// Ceiling on the fetched table, in bytes.
    pub max_table_bytes: Option<usize>,
    /// Domain the automatic per-environment addresses live under, for
    /// instance `preview.example.com`; an environment is then reachable at
    /// `<env>--<app>.<zone>` and, for the default environment, also at
    /// `<app>.<zone>`.
    ///
    /// Unset - the default - turns the whole mechanism off: only routes an
    /// operator created exist, and no address is invented for anything.
    ///
    /// Point a wildcard DNS record at this instance and give it a wildcard
    /// certificate (`tls.default_certificate` is the usual place) or the
    /// addresses resolve to nothing and the handshake fails.
    pub preview_zone: Option<String>,
}

impl RoutingConfig {
    pub fn control_poll_interval_secs(&self) -> u64 {
        self.control_poll_interval_secs
            .unwrap_or(DEFAULT_CONTROL_POLL_INTERVAL_SECS)
    }

    pub fn max_table_bytes(&self) -> usize {
        self.max_table_bytes.unwrap_or(DEFAULT_MAX_TABLE_BYTES)
    }

    /// The configured zone, normalised. `None` means automatic addresses are
    /// off; a value that is not usable as a domain suffix is refused by
    /// [`Validatable::validate`] rather than silently turning them off.
    pub fn preview_zone(&self) -> Option<PreviewZone> {
        self.preview_zone.as_deref().and_then(PreviewZone::new)
    }
}

impl Validatable for RoutingConfig {
    fn validate(&self) -> Result<(), ConfigError> {
        if self.control_poll_interval_secs == Some(0) {
            return Err(ConfigError::ValidationError {
                message: "routing.control_poll_interval_secs must be at least 1; 0 would poll the \
                          control plane without pause"
                    .to_string(),
            });
        }
        if let Some(bytes) = self.max_table_bytes
            && bytes < MIN_MAX_TABLE_BYTES
        {
            return Err(ConfigError::ValidationError {
                message: format!(
                    "routing.max_table_bytes must be at least {MIN_MAX_TABLE_BYTES}; below that no \
                     routing table fits and every poll would be refused, leaving the edge on \
                     whatever it last knew"
                ),
            });
        }
        if let Some(zone) = &self.preview_zone
            && PreviewZone::new(zone).is_none()
        {
            return Err(ConfigError::ValidationError {
                message: format!(
                    "routing.preview_zone {zone:?} is not a domain; remove it to turn automatic \
                     per-environment addresses off"
                ),
            });
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_the_documented_ones() {
        let routing = RoutingConfig::default();
        assert_eq!(
            routing.control_poll_interval_secs(),
            DEFAULT_CONTROL_POLL_INTERVAL_SECS
        );
        assert_eq!(routing.max_table_bytes(), DEFAULT_MAX_TABLE_BYTES);
        assert!(
            routing.preview_zone().is_none(),
            "automatic addresses must be off unless an operator asked for them"
        );
        assert!(routing.validate().is_ok());
    }

    #[test]
    fn a_zone_that_is_not_a_domain_is_refused_rather_than_ignored() {
        let routing = RoutingConfig {
            preview_zone: Some("   ".to_string()),
            ..Default::default()
        };
        assert!(routing.validate().is_err());
    }

    #[test]
    fn unusable_numbers_are_refused() {
        assert!(
            RoutingConfig {
                control_poll_interval_secs: Some(0),
                ..Default::default()
            }
            .validate()
            .is_err()
        );
        assert!(
            RoutingConfig {
                max_table_bytes: Some(16),
                ..Default::default()
            }
            .validate()
            .is_err()
        );
    }

    #[test]
    fn a_configured_zone_is_normalised() {
        let routing = RoutingConfig {
            preview_zone: Some("Preview.Example.COM".to_string()),
            ..Default::default()
        };
        assert!(routing.validate().is_ok());
        assert_eq!(
            routing.preview_zone().unwrap().as_str(),
            "preview.example.com"
        );
    }
}
