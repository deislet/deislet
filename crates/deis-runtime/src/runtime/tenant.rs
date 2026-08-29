//! The tenant every storage call is charged to.
//!
//! A tenant is one application *in one environment*. deis-store and deis-queue
//! understand exactly one dimension of the world outside themselves and know
//! nothing about environments: to them a tenant id is one opaque key, arriving
//! out of band in the [`deis_wire::conventions::TENANT_HEADER`] gRPC metadata
//! header, which is why none of the storage proto messages carry a tenant
//! field. A request that arrives without it is refused with PermissionDenied
//! before any storage is touched, so every storage client in this crate holds
//! a [`Tenant`] and stamps it on every request it sends.
//!
//! # Why not the app id
//!
//! Because production and a preview of one application must not share a
//! namespace. A preview that could read or overwrite what production stored
//! is an unrecoverable failure, so the two halves are joined by
//! [`deis_wire::conventions::tenant_id`] (`demo.production`) and the pair is
//! what the storage services see. [`crate::APP_ID_HEADER`] keeps its old
//! meaning on the data path -- it names the application, not the tenant -- and
//! is deliberately not sent here.
//!
//! # Where the value comes from
//!
//! Already joined: an application is identified on this node by
//! [`crate::app::AppKey`], and the string the runtime is handed for it is
//! [`crate::app::AppKey::tenant_id`]. This module therefore validates rather
//! than builds, with [`deis_wire::conventions::split_tenant`]: exactly two
//! legal parts, so no value can name a parent directory and a bare
//! application id -- what callers sent before this contract existed -- is
//! refused instead of quietly becoming production's tenant.
//!
//! # In log lines
//!
//! The pair is carried in the field named `app_id`, here and everywhere else
//! in the node. That name is not a leftover: deis-telemetry stores exactly one
//! tenant dimension and calls it `app_id`, and every query filters on it (see
//! `crate::logging::layer`). Putting the pair in it is what makes a preview's
//! lines separable from production's; naming the field anything else would
//! drop the line's attribution instead.

use anyhow::{anyhow, Result};
use deis_wire::conventions::{split_tenant, TENANT_HEADER};
use tonic::metadata::{Ascii, MetadataValue};
use tonic::Request;

/// One application-and-environment's identity on the storage wire.
///
/// The metadata value is parsed once, at construction, so a tenant that cannot
/// be sent as a header fails when the client is built rather than on every
/// call afterwards.
#[derive(Clone, Debug)]
pub struct Tenant {
    tenant: String,
    value: MetadataValue<Ascii>,
}

impl Tenant {
    /// Bind to one tenant, given its wire spelling `<app>.<environment>`.
    ///
    /// The check is the platform's own: the character set has no `.`, `/` or
    /// `\`, which is what makes the id safe as a directory name in deis-store,
    /// and exactly two parts, which is what makes one pair unable to
    /// impersonate another.
    pub fn new(tenant: impl Into<String>) -> Result<Self> {
        let tenant = tenant.into();
        let Some((app_id, environment)) = split_tenant(&tenant) else {
            return Err(anyhow!(
                "{tenant:?} is not a storage tenant; it must be an application and an \
                 environment joined by '.', for example \"demo.production\". Every storage call \
                 of this application would be refused"
            ));
        };
        debug_assert!(!app_id.is_empty() && !environment.is_empty());
        let value: MetadataValue<Ascii> = tenant.parse().map_err(|_| {
            anyhow!(
                "tenant {tenant:?} cannot be sent as gRPC metadata; storage would be unusable \
                 for this application"
            )
        })?;
        Ok(Self { tenant, value })
    }

    /// The tenant as the storage services will see it, e.g. `demo.production`.
    pub fn as_str(&self) -> &str {
        &self.tenant
    }

    /// The application half, for log lines an operator reads per application.
    pub fn app_id(&self) -> &str {
        split_tenant(&self.tenant)
            .map(|(app_id, _)| app_id)
            .unwrap_or(&self.tenant)
    }

    /// The environment half.
    pub fn environment(&self) -> &str {
        split_tenant(&self.tenant)
            .map(|(_, environment)| environment)
            .unwrap_or_default()
    }

    /// Wrap a message in a request carrying this tenant.
    pub fn stamp<T>(&self, message: T) -> Request<T> {
        let mut request = Request::new(message);
        request
            .metadata_mut()
            .insert(TENANT_HEADER, self.value.clone());
        request
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stamps_the_header_the_store_reads() {
        let tenant = Tenant::new("demo-app_1.production").expect("two plain ascii parts");
        let request = tenant.stamp(());
        assert_eq!(
            request
                .metadata()
                .get(TENANT_HEADER)
                .and_then(|v| v.to_str().ok()),
            Some("demo-app_1.production")
        );
    }

    #[test]
    fn does_not_send_the_app_id_header() {
        // The app id names the application on the data path and is no longer
        // the storage tenant; sending it here would invite the store to key
        // on it again and put two environments back in one namespace.
        let request = Tenant::new("demo.preview-7").expect("valid").stamp(());
        assert!(request.metadata().get(crate::APP_ID_HEADER).is_none());
    }

    #[test]
    fn keeps_both_halves_readable_for_logs() {
        let tenant = Tenant::new("demo.preview-7").expect("valid");
        assert_eq!(tenant.as_str(), "demo.preview-7");
        assert_eq!(tenant.app_id(), "demo");
        assert_eq!(tenant.environment(), "preview-7");
    }

    #[test]
    fn two_environments_of_one_app_are_two_tenants() {
        // The whole point: a preview must not be able to read or overwrite
        // what production stored.
        let production = Tenant::new("demo.production").expect("valid");
        let preview = Tenant::new("demo.preview").expect("valid");
        assert_ne!(production.as_str(), preview.as_str());
    }

    #[test]
    fn refuses_a_bare_app_id() {
        // What every caller sent before this contract existed. Accepting it
        // as "production" would put a preview's writes in production's
        // namespace the first time an environment was left unset.
        let err = Tenant::new("demo").expect_err("an app id alone names no environment");
        assert!(err.to_string().contains("not a storage tenant"), "{err}");
    }

    #[test]
    fn refuses_a_tenant_that_could_escape_its_directory() {
        for bad in [
            "",
            "..",
            "../etc.production",
            "a.b.c",
            "demo.",
            ".production",
        ] {
            assert!(
                Tenant::new(bad).is_err(),
                "{bad:?} must be refused: it names a directory in deis-store"
            );
        }
    }
}
