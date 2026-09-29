//! The routing table `deis-control` hands out over the machine channel's
//! `FetchRoutingTable`, and the
//! rules a copy of it has to pass before any of it is believed.
//!
//! Validation here is deliberately all-or-nothing. Every field below ends up
//! either in a routing decision or, for `app_id` and `environment`, verbatim
//! in a request header on the way to a node; a single unusable value means
//! the document was not produced by a control plane this build understands,
//! and applying the rest of it would send live traffic somewhere on the
//! strength of a payload we have already caught being wrong.
//!
//! `Serialize` is here for [`super::persist`] alone, which writes a fetched
//! document to a local file and reads it back through the same validation.
//! The control plane still owns the shape: this side only has to be able to
//! put a document it accepted into a file byte for byte.

use serde::{Deserialize, Serialize};

/// One route: a host and path prefix bound to one environment of one app.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct RoutingEntry {
    pub app_id: String,
    pub environment: String,
    /// Immutable deployment selected for this target. Absent only when the
    /// environment has never been deployed.
    #[serde(default)]
    pub deployment_generation: Option<String>,
    /// Absent means "any domain". `*.example.com` is a wildcard.
    #[serde(default)]
    pub host: Option<String>,
    pub path_prefix: String,
    #[serde(default)]
    pub priority: i64,
}

/// One environment that exists, whether or not a route points at it. This is
/// what an automatic preview address is built from.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct EnvironmentRef {
    pub app_id: String,
    pub environment: String,
    #[serde(default)]
    pub deployment_generation: Option<String>,
}

/// The whole document.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct RoutingTableDocument {
    /// Content digest computed by the control plane. Two polls that return
    /// the same version describe the same table, which is how "nothing
    /// changed" is told from "changed" without a diff.
    pub version: String,
    /// Unix seconds the control plane built this answer. Reported in log
    /// lines so an operator can see how old a table that is still in force
    /// actually is.
    #[serde(default)]
    pub generated_at: i64,
    pub routes: Vec<RoutingEntry>,
    #[serde(default)]
    pub environments: Vec<EnvironmentRef>,
}

/// Why a fetched document was refused whole.
#[derive(Debug, thiserror::Error)]
pub enum DocumentError {
    #[error("the routing table carries no version")]
    NoVersion,
    #[error("route {index} has an empty {field}")]
    Empty { index: usize, field: &'static str },
    #[error(
        "route {index} has a {field} carrying whitespace or a control character ({value:?}); it would go on the wire as a request header"
    )]
    NotHeaderSafe {
        index: usize,
        field: &'static str,
        value: String,
    },
    #[error("route {index} has path_prefix {value:?}, which does not start with '/'")]
    RelativePrefix { index: usize, value: String },
    #[error("environment {index} has an empty {field}")]
    EmptyEnvironment { index: usize, field: &'static str },
    #[error(
        "environment {index} has a {field} carrying whitespace or a control character ({value:?}); it would go on the wire as a request header"
    )]
    NotHeaderSafeEnvironment {
        index: usize,
        field: &'static str,
        value: String,
    },
    #[error(
        "{location} {index} has deployment_generation {value:?}, which is not a canonical UUIDv7"
    )]
    InvalidGeneration {
        location: &'static str,
        index: usize,
        value: String,
    },
}

impl RoutingTableDocument {
    /// Check every entry. On the first fault the whole document is refused:
    /// see the module comment for why nothing partial is applied.
    pub fn validate(&self) -> Result<(), DocumentError> {
        if self.version.trim().is_empty() {
            return Err(DocumentError::NoVersion);
        }
        for (index, route) in self.routes.iter().enumerate() {
            check_name(index, "app_id", &route.app_id)?;
            check_name(index, "environment", &route.environment)?;
            if let Some(generation) = &route.deployment_generation {
                check_generation("route", index, generation)?;
            }
            if route.path_prefix.is_empty() {
                return Err(DocumentError::Empty {
                    index,
                    field: "path_prefix",
                });
            }
            if !route.path_prefix.starts_with('/') {
                return Err(DocumentError::RelativePrefix {
                    index,
                    value: route.path_prefix.clone(),
                });
            }
            if has_unusable_char(&route.path_prefix) {
                return Err(DocumentError::NotHeaderSafe {
                    index,
                    field: "path_prefix",
                    value: route.path_prefix.clone(),
                });
            }
            if let Some(host) = &route.host {
                if host.trim().is_empty() {
                    return Err(DocumentError::Empty {
                        index,
                        field: "host",
                    });
                }
                if has_unusable_char(host) {
                    return Err(DocumentError::NotHeaderSafe {
                        index,
                        field: "host",
                        value: host.clone(),
                    });
                }
            }
        }
        for (index, env) in self.environments.iter().enumerate() {
            if env.app_id.trim().is_empty() {
                return Err(DocumentError::EmptyEnvironment {
                    index,
                    field: "app_id",
                });
            }
            if env.environment.trim().is_empty() {
                return Err(DocumentError::EmptyEnvironment {
                    index,
                    field: "environment",
                });
            }
            if let Some(generation) = &env.deployment_generation {
                check_generation("environment", index, generation)?;
            }
        }
        Ok(())
    }
}

fn check_generation(
    location: &'static str,
    index: usize,
    value: &str,
) -> Result<(), DocumentError> {
    value
        .parse::<deis_wire::CanonicalId>()
        .map(|_| ())
        .map_err(|_| DocumentError::InvalidGeneration {
            location,
            index,
            value: value.to_string(),
        })
}

fn check_name(index: usize, field: &'static str, value: &str) -> Result<(), DocumentError> {
    if value.is_empty() {
        return Err(DocumentError::Empty { index, field });
    }
    if has_unusable_char(value) {
        return Err(DocumentError::NotHeaderSafe {
            index,
            field,
            value: value.to_string(),
        });
    }
    Ok(())
}

/// Anything that cannot travel in an HTTP header value, plus whitespace,
/// which no host, path prefix, app id or environment name is allowed to
/// carry in the catalog either.
fn has_unusable_char(value: &str) -> bool {
    value
        .chars()
        .any(|c| c.is_whitespace() || c.is_control() || !c.is_ascii())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn document(routes_json: &str) -> RoutingTableDocument {
        serde_json::from_str(&format!(
            r#"{{"version":"v1","generated_at":1,"routes":{routes_json},"environments":[]}}"#
        ))
        .unwrap()
    }

    #[test]
    fn a_well_formed_document_passes() {
        let doc = document(
            r#"[{"app_id":"shop","environment":"production","host":"example.com","path_prefix":"/","priority":0}]"#,
        );
        assert!(doc.validate().is_ok());
    }

    #[test]
    fn a_missing_host_means_any_domain_not_a_fault() {
        let doc =
            document(r#"[{"app_id":"shop","environment":"production","path_prefix":"/api"}]"#);
        assert!(doc.validate().is_ok());
        assert!(doc.routes[0].host.is_none());
    }

    #[test]
    fn one_bad_entry_refuses_the_whole_document() {
        // The good entry is first on purpose: what is being asserted is that
        // it does not get applied because of the bad one behind it.
        let doc = document(
            r#"[{"app_id":"shop","environment":"production","path_prefix":"/"},
                {"app_id":"other","environment":"pro duction","path_prefix":"/"}]"#,
        );
        assert!(matches!(
            doc.validate(),
            Err(DocumentError::NotHeaderSafe { index: 1, .. })
        ));
    }

    #[test]
    fn a_header_value_that_would_not_travel_is_refused() {
        for bad in [
            r#"[{"app_id":"sh\nop","environment":"production","path_prefix":"/"}]"#,
            r#"[{"app_id":"shop","environment":"","path_prefix":"/"}]"#,
            r#"[{"app_id":"shop","environment":"production","path_prefix":"api"}]"#,
            r#"[{"app_id":"shop","environment":"production","host":"  ","path_prefix":"/"}]"#,
        ] {
            assert!(document(bad).validate().is_err(), "accepted {bad}");
        }
    }

    #[test]
    fn deployment_generations_must_be_canonical_uuidv7_on_both_wire_lists() {
        let valid = "018f2518-7e38-7f80-8000-000000000001";
        let route = document(&format!(
            r#"[{{"app_id":"shop","environment":"production","deployment_generation":"{valid}","path_prefix":"/"}}]"#
        ));
        route.validate().unwrap();

        for invalid in [
            "550e8400-e29b-41d4-a716-446655440000",
            "018F2518-7E38-7F80-8000-000000000001",
            "not-a-generation",
        ] {
            let route = document(&format!(
                r#"[{{"app_id":"shop","environment":"production","deployment_generation":"{invalid}","path_prefix":"/"}}]"#
            ));
            assert!(matches!(
                route.validate(),
                Err(DocumentError::InvalidGeneration {
                    location: "route",
                    index: 0,
                    ..
                })
            ));

            let environment: RoutingTableDocument = serde_json::from_str(&format!(
                r#"{{"version":"v1","routes":[],"environments":[{{"app_id":"shop","environment":"production","deployment_generation":"{invalid}"}}]}}"#
            ))
            .unwrap();
            assert!(matches!(
                environment.validate(),
                Err(DocumentError::InvalidGeneration {
                    location: "environment",
                    index: 0,
                    ..
                })
            ));
        }
    }

    /// The half of the contract this crate owns.
    ///
    /// Nothing type-checks the JSON across the two crates, so this test and
    /// its twin in deis-control are what hold the shape together.
    #[test]
    fn the_wire_contract_is_the_one_deis_control_serves() {
        // Written out the way the control plane serialises it, field for
        // field, rather than built with the types under test.
        let document: RoutingTableDocument = serde_json::from_str(
            r#"{
                "version": "d1e5",
                "generated_at": 1756512000,
                "routes": [
                    {"app_id":"shop","environment":"pr-42","host":"example.com","path_prefix":"/api","priority":3}
                ],
                "environments": [
                    {"app_id":"shop","environment":"production"}
                ]
            }"#,
        )
        .unwrap();
        document.validate().unwrap();
        assert_eq!(document.version, "d1e5");
        assert_eq!(document.generated_at, 1_756_512_000);
        assert_eq!(document.routes[0].app_id, "shop");
        assert_eq!(document.routes[0].environment, "pr-42");
        assert_eq!(document.routes[0].host.as_deref(), Some("example.com"));
        assert_eq!(document.routes[0].path_prefix, "/api");
        assert_eq!(document.routes[0].priority, 3);
        assert_eq!(document.environments[0].app_id, "shop");
        assert_eq!(document.environments[0].environment, "production");
    }

    #[test]
    fn a_document_without_a_version_is_refused() {
        let mut doc = document(r#"[]"#);
        doc.version = String::new();
        assert!(matches!(doc.validate(), Err(DocumentError::NoVersion)));
    }
}
