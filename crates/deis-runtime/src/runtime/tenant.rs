//! The tenant every storage call is charged to.
//!
//! A tenant is one application *in one environment*, named by its canonical
//! id `v1/<application id>/<environment id>` - permanent, so a renamed or
//! re-created application never inherits another's data. deis-store and
//! deis-queue take the tenant of a call from the capability it carries, never
//! from the request, so this type does not stamp anything: it names the
//! tenant this node asks the control plane for capabilities for (see
//! `crate::deployment::capability_renewer`), and what log lines print.
//!
//! # In log lines
//!
//! Log lines still carry the `<app>.<environment>` label in the field named
//! `app_id`, because deis-telemetry attributes records to that label and an
//! operator reads names (see `crate::logging::layer`).

use anyhow::{Result, anyhow};
use deis_wire::CanonicalTenantId;
use tonic::Request;

/// One application environment's identity on the storage wire.
#[derive(Clone, Debug)]
pub struct Tenant {
    tenant: String,
}

impl Tenant {
    /// Bind to one tenant, given its canonical spelling
    /// `v1/<application id>/<environment id>`.
    pub fn new(tenant: impl Into<String>) -> Result<Self> {
        let tenant = tenant.into();
        tenant.parse::<CanonicalTenantId>().map_err(|error| {
            anyhow!(
                "{tenant:?} is not a canonical storage tenant ({error}); it must be \
                 v1/<application id>/<environment id>, which the control plane sends with every \
                 deployment. Every storage call of this application would be refused"
            )
        })?;
        Ok(Self { tenant })
    }

    /// The tenant as capabilities name it.
    pub fn as_str(&self) -> &str {
        &self.tenant
    }

    /// Wrap a message in a request. The tenant is not stamped: the services
    /// take it from the capability the caller adds.
    pub fn stamp<T>(&self, message: T) -> Request<T> {
        Request::new(message)
    }
}

/// The canonical tenant a test names by label (`"demo.production"`), or the
/// value unchanged when it already is one.
#[cfg(test)]
pub(crate) fn test_tenant(label: &str) -> String {
    if label.parse::<CanonicalTenantId>().is_ok() {
        return label.to_string();
    }
    CanonicalTenantId::derived_from_label(label).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn canonical(label: &str) -> String {
        CanonicalTenantId::derived_from_label(label).to_string()
    }

    #[test]
    fn stamps_no_tenant_header() {
        let tenant = Tenant::new(canonical("demo.production")).expect("canonical");
        let request = tenant.stamp(());
        assert!(request.metadata().get("x-deis-tenant").is_none());
        assert!(request.metadata().get(crate::APP_ID_HEADER).is_none());
    }

    #[test]
    fn two_environments_of_one_app_are_two_tenants() {
        let production = Tenant::new(canonical("demo.production")).expect("valid");
        let preview = Tenant::new(canonical("demo.preview")).expect("valid");
        assert_ne!(production.as_str(), preview.as_str());
    }

    #[test]
    fn refuses_anything_but_a_canonical_tenant() {
        for bad in ["", "demo", "demo.production", "../etc.production", "v1/a/b"] {
            let err = Tenant::new(bad).expect_err("not canonical");
            assert!(
                err.to_string().contains("not a canonical storage tenant"),
                "{err}"
            );
        }
    }
}
