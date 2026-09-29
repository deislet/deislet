//! What a node may be issued a capability for.
//!
//! # The rule
//!
//! A node asks `IssueCapability` for an audience and a list of
//! (tenant, binding, generation) resources. It gets a capability only when
//! every one of them is something it runs:
//!
//! - the audience is one whose services check per-tenant bindings
//!   ([`deis_capability::bindings_for_audience`]) - no `telemetry.read`, no
//!   audience string nothing verifies;
//! - the tenant is the canonical `v1/<application id>/<environment id>` of
//!   a deployment
//!   [`deployments_visible_to`](crate::api::catalog::assignments::deployments_visible_to)
//!   that node - the one place placement will narrow;
//! - the binding is one that audience defines;
//! - a generation, if the resource names one, is that deployment's routing
//!   generation;
//! - for deis-telemetry, the binding is `ingest` only: a node pushes the
//!   records of what it runs and never reads anyone's. It may also always
//!   ask for `ingest` on the reserved `@platform` tenant, which covers its
//!   own node-level records and nothing of any application.
//!
//! One resource outside that refuses the whole request, and the refusal is
//! written to the audit log with the name the node proved. This is what
//! keeps a compromised node from minting access to tenants it does not run:
//! the trusted-tenant model accepts that a node sees what it runs, not that
//! it can reach everything.

use std::collections::{BTreeMap, BTreeSet};

use deis_capability::{
    AUD_TELEMETRY, CapabilityResource, TELEMETRY_INGEST, TELEMETRY_PLATFORM_TENANT,
    bindings_for_audience,
};

use crate::api::catalog::assignments::VisibleDeployment;

/// The canonical tenants a node may name, each with the routing generations
/// it may name for that tenant.
pub(crate) fn tenants_of(
    visible: &BTreeMap<String, VisibleDeployment>,
) -> BTreeMap<&str, BTreeSet<&str>> {
    let mut tenants: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
    for deployment in visible.values() {
        tenants
            .entry(deployment.canonical_tenant.as_str())
            .or_default()
            .insert(deployment.routing_generation.as_str());
    }
    tenants
}

/// Why a request is refused, in words the audit line and the node's log can
/// both carry; `Ok` when every resource is one the node runs.
pub(crate) fn authorize(
    node_id: &str,
    audience: &str,
    resources: &[CapabilityResource],
    visible: &BTreeMap<String, VisibleDeployment>,
) -> Result<(), String> {
    let Some(bindings) = bindings_for_audience(audience) else {
        return Err(format!(
            "audience {audience:?} is not one a node may be issued a capability for"
        ));
    };
    if resources.is_empty() {
        return Err("at least one resource must be requested".to_string());
    }
    let tenants = tenants_of(visible);
    for resource in resources {
        if audience == AUD_TELEMETRY && resource.binding != TELEMETRY_INGEST {
            return Err(format!(
                "binding {:?} of the {audience} audience is not one a node may be issued; \
                 a node pushes telemetry and reads none",
                resource.binding
            ));
        }
        if audience == AUD_TELEMETRY
            && resource.tenant == TELEMETRY_PLATFORM_TENANT
            && resource.generation.is_none()
        {
            continue;
        }
        let Some(generations) = tenants.get(resource.tenant.as_str()) else {
            return Err(format!(
                "tenant {:?} is not deployed to node {node_id}",
                resource.tenant
            ));
        };
        if !bindings.contains(&resource.binding.as_str()) {
            return Err(format!(
                "binding {:?} is not one the {audience} audience defines ({})",
                resource.binding,
                bindings.join(", ")
            ));
        }
        if let Some(generation) = resource.generation.as_deref()
            && !generations.contains(generation)
        {
            return Err(format!(
                "generation {generation:?} of tenant {:?} is not deployed to node {node_id}",
                resource.tenant
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use deis_capability::{AUD_QUEUE, AUD_STORE, TELEMETRY_READ, resources_for_audience};

    fn canonical(label: &str) -> String {
        deis_wire::CanonicalTenantId::derived_from_label(label).to_string()
    }

    fn visible() -> BTreeMap<String, VisibleDeployment> {
        BTreeMap::from([(
            "dep-a".to_string(),
            VisibleDeployment {
                env_id: "env-a".to_string(),
                snapshot_id: "snapshot-a".to_string(),
                canonical_tenant: canonical("a.production"),
                routing_generation: "gen-a".to_string(),
            },
        )])
    }

    #[test]
    fn a_data_service_capability_names_the_canonical_tenant_not_the_label() {
        for aud in [AUD_STORE, AUD_QUEUE] {
            assert!(
                authorize(
                    "n",
                    aud,
                    &resources_for_audience("a.production", aud),
                    &visible()
                )
                .is_err()
            );
        }
    }

    #[test]
    fn a_node_may_have_what_it_runs() {
        for aud in [AUD_STORE, AUD_QUEUE] {
            authorize(
                "n",
                aud,
                &resources_for_audience(&canonical("a.production"), aud),
                &visible(),
            )
            .unwrap();
        }
        let mut pinned = resources_for_audience(&canonical("a.production"), AUD_STORE);
        pinned[0].generation = Some("gen-a".to_string());
        authorize("n", AUD_STORE, &pinned, &visible()).unwrap();
    }

    #[test]
    fn one_resource_outside_what_it_runs_refuses_the_request() {
        let mut asked = resources_for_audience(&canonical("a.production"), AUD_STORE);
        asked.extend(resources_for_audience(
            &canonical("b.production"),
            AUD_STORE,
        ));
        let refusal = authorize("n", AUD_STORE, &asked, &visible()).unwrap_err();
        assert!(refusal.contains(&canonical("b.production")), "{refusal}");
    }

    #[test]
    fn a_binding_or_audience_nothing_checks_is_refused() {
        let mut wrong = resources_for_audience(&canonical("a.production"), AUD_STORE);
        wrong[0].binding = "queue".to_string();
        assert!(authorize("n", AUD_STORE, &wrong, &visible()).is_err());
        assert!(
            authorize(
                "n",
                "telemetry.read",
                &resources_for_audience(&canonical("a.production"), AUD_STORE),
                &visible()
            )
            .is_err()
        );
        assert!(authorize("n", AUD_STORE, &[], &visible()).is_err());
    }

    fn ingest(tenant: &str) -> CapabilityResource {
        CapabilityResource {
            tenant: tenant.to_string(),
            binding: TELEMETRY_INGEST.to_string(),
            generation: None,
        }
    }

    #[test]
    fn a_node_may_push_telemetry_for_what_it_runs_and_its_own_records() {
        authorize(
            "n",
            AUD_TELEMETRY,
            &[
                ingest(&canonical("a.production")),
                ingest(TELEMETRY_PLATFORM_TENANT),
            ],
            &visible(),
        )
        .unwrap();
        // Its own records need no deployment at all.
        authorize(
            "n",
            AUD_TELEMETRY,
            &[ingest(TELEMETRY_PLATFORM_TENANT)],
            &BTreeMap::new(),
        )
        .unwrap();
        assert!(
            authorize(
                "n",
                AUD_TELEMETRY,
                &[ingest(&canonical("b.production"))],
                &visible()
            )
            .is_err()
        );
        // The label names no tenant: a recreated application of the same
        // name would otherwise push into the old one's records.
        assert!(authorize("n", AUD_TELEMETRY, &[ingest("a.production")], &visible()).is_err());
    }

    #[test]
    fn a_node_may_never_read_telemetry() {
        for tenant in [
            canonical("a.production"),
            TELEMETRY_PLATFORM_TENANT.to_string(),
        ] {
            let read = CapabilityResource {
                binding: TELEMETRY_READ.to_string(),
                ..ingest(&tenant)
            };
            let refusal = authorize("n", AUD_TELEMETRY, &[read], &visible()).unwrap_err();
            assert!(refusal.contains("reads none"), "{refusal}");
        }
    }

    #[test]
    fn a_generation_that_is_not_deployed_is_refused() {
        let mut pinned = resources_for_audience(&canonical("a.production"), AUD_STORE);
        pinned[0].generation = Some("gen-old".to_string());
        assert!(authorize("n", AUD_STORE, &pinned, &visible()).is_err());
    }

    #[test]
    fn a_node_that_runs_nothing_gets_nothing() {
        assert!(
            authorize(
                "n",
                AUD_STORE,
                &resources_for_audience(&canonical("a.production"), AUD_STORE),
                &BTreeMap::new()
            )
            .is_err()
        );
    }
}
