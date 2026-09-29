//! Whose a telemetry record is.
//!
//! Every record an application produces carries its canonical tenant,
//! `v1/<application id>/<environment id>`, beside the `app_id` and
//! environment names an operator reads. The tenant is what a capability
//! names and what decides who may push or read the record. Names are not:
//! an application deleted and created again under the same name gets new
//! ids, so it is another tenant and none of the old one's records reach its
//! readers.

use deis_wire::CanonicalTenantId;

use crate::auth::Owner;
use crate::store::{LogRecord, MetricRecord, TraceRecord};

/// Whose a record is. No application at all means a node's own record, which
/// belongs to the platform rather than to any tenant and carries no tenant;
/// a record of an application must name its canonical tenant.
fn owner(app_id: &str, tenant: &str) -> Result<Owner, String> {
    match (app_id.is_empty(), tenant.is_empty()) {
        (true, true) => Ok(Owner::Platform),
        (true, false) => Err(format!(
            "tenant {tenant:?} is set on a record that names no application"
        )),
        (false, true) => Err(format!(
            "a record of application {app_id:?} names no canonical tenant"
        )),
        (false, false) => tenant
            .parse::<CanonicalTenantId>()
            .map(|_| Owner::Tenant(tenant.to_string()))
            .map_err(|error| format!("tenant {tenant:?} is not canonical: {error}")),
    }
}

pub(crate) fn log(record: &LogRecord) -> Result<Owner, String> {
    owner(&record.app_id, &record.tenant)
}

pub(crate) fn metric(record: &MetricRecord) -> Result<Owner, String> {
    owner(&record.app_id, &record.tenant)
}

pub(crate) fn trace(record: &TraceRecord) -> Result<Owner, String> {
    owner(&record.app_id, &record.tenant)
}

/// The canonical tenant a fixture record of `app_id` in `environment`
/// carries; empty for a record of no application.
#[cfg(test)]
pub(crate) fn fixture_tenant(app_id: &str, environment: &str) -> String {
    if app_id.is_empty() {
        return String::new();
    }
    CanonicalTenantId::derived_from_label(&format!("{app_id}.{environment}")).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn canonical(label: &str) -> String {
        CanonicalTenantId::derived_from_label(label).to_string()
    }

    #[test]
    fn an_application_record_is_owned_by_its_canonical_tenant() {
        let tenant = canonical("shop.production");
        assert_eq!(owner("shop", &tenant).unwrap(), Owner::Tenant(tenant));
    }

    #[test]
    fn a_name_is_never_an_owner() {
        assert!(owner("shop", "").is_err());
        assert!(owner("shop", "shop.production").is_err());
        assert!(owner("", &canonical("shop.production")).is_err());
    }

    #[test]
    fn no_application_means_the_platform() {
        assert_eq!(owner("", "").unwrap(), Owner::Platform);
    }
}
