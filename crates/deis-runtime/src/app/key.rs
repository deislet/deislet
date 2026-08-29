//! What identifies a running application on a node.
//!
//! An application is not one thing on a node. Its production environment and
//! every preview of it run side by side, each with its own version pointer,
//! its own bindings and its own secrets (docs/architecture.zh.md section 2).
//! Keying anything by the application alone therefore makes two environments
//! of one application share a slot, and the second delivery silently replaces
//! the first: production ends up serving preview code with preview secrets,
//! or the other way round depending on which order the deliveries arrive in.
//!
//! So the pair is the key, everywhere: the isolate, the asset index, the
//! bundle directory on disk and the installed-version record.
//!
//! The pair also has a wire spelling, and it is the one the storage services
//! already understand: [`AppKey::tenant_id`] is
//! [`deis_wire::conventions::tenant_id`], e.g. `demo.production`. That is
//! deliberate - it means a preview cannot read or overwrite what production
//! stored, and it means the node has exactly one string to hand to anything
//! that only understands one dimension.

use std::fmt;

use deis_wire::conventions::{self, DEFAULT_ENVIRONMENT};

/// One application in one environment: the identity of a running application
/// on this node.
///
/// `Display` (and [`AppKey::tenant_id`]) produce the storage tenant spelling
/// `app.environment`, which is also the key this crate hands to
/// [`crate::runtime::RuntimePool`] and to the metrics registry, so a preview's
/// requests, errors and storage never land in production's.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct AppKey {
    app_id: String,
    environment: String,
}

impl AppKey {
    /// Name one environment of one application.
    ///
    /// An empty environment means the caller did not say, which is
    /// [`DEFAULT_ENVIRONMENT`] - the same rule the data path applies when a
    /// request carries no environment header, so an older caller reaches
    /// production instead of being refused.
    pub fn new(app_id: impl Into<String>, environment: impl Into<String>) -> Self {
        let environment = environment.into();
        let environment = if environment.trim().is_empty() {
            DEFAULT_ENVIRONMENT.to_string()
        } else {
            environment
        };
        Self {
            app_id: app_id.into(),
            environment,
        }
    }

    /// The production environment of an application, for callers that carry no
    /// environment of their own.
    pub fn production(app_id: impl Into<String>) -> Self {
        Self::new(app_id, DEFAULT_ENVIRONMENT)
    }

    /// Read a key back out of its tenant spelling (`demo.production`).
    ///
    /// Rejects anything that is not exactly two legal parts, which is the same
    /// rule the storage services apply, so a value that round-trips here is a
    /// value they will accept.
    pub fn parse_tenant(tenant: &str) -> Option<Self> {
        conventions::split_tenant(tenant).map(|(app_id, environment)| Self {
            app_id: app_id.to_string(),
            environment: environment.to_string(),
        })
    }

    pub fn app_id(&self) -> &str {
        &self.app_id
    }

    pub fn environment(&self) -> &str {
        &self.environment
    }

    pub fn is_production(&self) -> bool {
        self.environment == DEFAULT_ENVIRONMENT
    }

    /// The tenant this application's storage is charged to, and the one string
    /// that stands for this key wherever only one dimension fits.
    pub fn tenant_id(&self) -> String {
        conventions::tenant_id(&self.app_id, &self.environment)
    }

    /// Refuse a pair that could not be a tenant.
    ///
    /// Both halves become a directory name on this node and a tenant on the
    /// storage services, so the check is the narrow one from
    /// [`conventions::is_valid_tenant_part`]: no `.`, no `/`, no `..`, which
    /// means no pair of legal names can name something outside its own
    /// directory or collide with another pair.
    pub fn validate(&self) -> Result<(), InvalidAppKey> {
        if !conventions::is_valid_tenant_part(&self.app_id) {
            return Err(InvalidAppKey {
                field: "app_id",
                value: self.app_id.clone(),
            });
        }
        if !conventions::is_valid_tenant_part(&self.environment) {
            return Err(InvalidAppKey {
                field: "environment",
                value: self.environment.clone(),
            });
        }
        Ok(())
    }
}

impl fmt::Display for AppKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.tenant_id())
    }
}

/// Why an application/environment pair cannot be used.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvalidAppKey {
    pub field: &'static str,
    pub value: String,
}

impl fmt::Display for InvalidAppKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} {:?} is not usable: it must be 1 to 64 characters of ASCII letters, digits, '_' \
             or '-'. The pair names a directory on this node and a tenant on the storage \
             services, so anything else could read or overwrite another environment's data",
            self.field, self.value
        )
    }
}

impl std::error::Error for InvalidAppKey {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn two_environments_of_one_app_are_two_keys() {
        // The whole point: a preview must never land in production's slot.
        let production = AppKey::new("demo", "production");
        let preview = AppKey::new("demo", "preview");
        assert_ne!(production, preview);
        assert_eq!(production.app_id(), preview.app_id());
    }

    #[test]
    fn an_absent_environment_means_production() {
        assert_eq!(AppKey::new("demo", ""), AppKey::production("demo"));
        assert_eq!(AppKey::new("demo", "   "), AppKey::production("demo"));
        assert!(AppKey::production("demo").is_production());
    }

    #[test]
    fn the_key_spells_itself_as_a_storage_tenant() {
        let key = AppKey::new("demo", "preview-7");
        assert_eq!(key.tenant_id(), "demo.preview-7");
        assert_eq!(key.to_string(), "demo.preview-7");
        assert_eq!(AppKey::parse_tenant("demo.preview-7"), Some(key));
    }

    #[test]
    fn a_tenant_that_is_not_two_legal_parts_is_not_a_key() {
        for bad in ["demo", "a.b.c", "..", "demo.", ".production"] {
            assert_eq!(AppKey::parse_tenant(bad), None, "{bad:?} must be rejected");
        }
    }

    #[test]
    fn a_key_that_could_escape_its_directory_is_refused() {
        let err = AppKey::new("demo", "../other")
            .validate()
            .expect_err("a path traversal must not be a key");
        assert_eq!(err.field, "environment");
        assert!(AppKey::new("..", "production").validate().is_err());
        // A dot would make the tenant id split into three parts, so a name
        // carrying one could impersonate a different pair.
        assert!(AppKey::new("demo.production", "preview")
            .validate()
            .is_err());
        assert!(AppKey::production("_system").validate().is_ok());
    }
}
