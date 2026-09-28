//! A telemetry record carries application and environment in different places
//! for each signal. This module is the one translation into the tenant a
//! capability names, which is what decides who may push or read a record.

use std::collections::BTreeMap;

use deis_wire::conventions::{DEFAULT_ENVIRONMENT, is_valid_tenant_part, tenant_id};

use crate::auth::Owner;
use crate::store::{LogRecord, MetricRecord, TraceRecord};

/// Missing and empty environments from older producers mean production, the
/// same compatibility rule used on the request path throughout the platform.
fn environment(value: Option<&str>) -> &str {
    match value {
        Some(value) if !value.is_empty() => value,
        _ => DEFAULT_ENVIRONMENT,
    }
}

fn map_environment(values: &BTreeMap<String, String>) -> &str {
    environment(values.get("environment").map(String::as_str))
}

pub(crate) fn from_parts(app_id: &str, environment: Option<&str>) -> Result<String, String> {
    let environment = self::environment(environment);
    if !is_valid_tenant_part(app_id) {
        return Err(format!(
            "app_id {app_id:?} is not a valid canonical tenant part"
        ));
    }
    if !is_valid_tenant_part(environment) {
        return Err(format!(
            "environment {environment:?} is not a valid canonical tenant part"
        ));
    }
    Ok(tenant_id(app_id, environment))
}

/// Whose a record is. No application at all means a node's own record, which
/// belongs to the platform rather than to any tenant; anything else has to
/// name a valid application environment.
fn owner(app_id: &str, environment: Option<&str>) -> Result<Owner, String> {
    if app_id.is_empty() {
        return Ok(Owner::Platform);
    }
    from_parts(app_id, environment).map(Owner::Tenant)
}

pub(crate) fn log(record: &LogRecord) -> Result<Owner, String> {
    owner(&record.app_id, Some(map_environment(&record.fields)))
}

pub(crate) fn metric(record: &MetricRecord) -> Result<Owner, String> {
    owner(&record.app_id, Some(map_environment(&record.labels)))
}

pub(crate) fn trace(record: &TraceRecord) -> Result<Owner, String> {
    owner(&record.app_id, Some(&record.environment))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_missing_or_empty_environment_maps_to_production() {
        assert_eq!(from_parts("shop", None).unwrap(), "shop.production");
        assert_eq!(from_parts("shop", Some("")).unwrap(), "shop.production");
    }

    #[test]
    fn unsafe_or_ambiguous_parts_are_rejected() {
        assert!(from_parts("shop.preview", Some("production")).is_err());
        assert!(from_parts("shop", Some("../../preview")).is_err());
        assert!(from_parts("", None).is_err());
    }

    #[test]
    fn no_application_means_the_platform() {
        assert_eq!(owner("", Some("production")).unwrap(), Owner::Platform);
        assert_eq!(
            owner("shop", None).unwrap(),
            Owner::Tenant("shop.production".to_string())
        );
    }
}
