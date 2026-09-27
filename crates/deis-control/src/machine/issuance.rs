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
//! - the tenant is `<app>.<environment>` of a deployment
//!   [`deployments_visible_to`](crate::api::catalog::assignments::deployments_visible_to)
//!   that node - the one place placement will narrow;
//! - the binding is one that audience defines;
//! - a generation, if the resource names one, is that deployment's routing
//!   generation.
//!
//! One resource outside that refuses the whole request, and the refusal is
//! written to the audit log with the name the node proved. This is what
//! keeps a compromised node from minting access to tenants it does not run:
//! the trusted-tenant model accepts that a node sees what it runs, not that
//! it can reach everything.

use std::collections::{BTreeMap, BTreeSet};

use deis_capability::{CapabilityResource, bindings_for_audience};

use crate::api::catalog::assignments::VisibleDeployment;

/// The tenants a node may name, each with the routing generations it may
/// name for that tenant.
pub(crate) fn tenants_of(
    visible: &BTreeMap<String, VisibleDeployment>,
) -> BTreeMap<&str, BTreeSet<&str>> {
    let mut tenants: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
    for deployment in visible.values() {
        tenants
            .entry(deployment.tenant.as_str())
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
    use deis_capability::{AUD_QUEUE, AUD_STORE, resources_for_audience};

    fn visible() -> BTreeMap<String, VisibleDeployment> {
        BTreeMap::from([(
            "dep-a".to_string(),
            VisibleDeployment {
                env_id: "env-a".to_string(),
                snapshot_id: None,
                tenant: "a.production".to_string(),
                routing_generation: "gen-a".to_string(),
            },
        )])
    }

    #[test]
    fn a_node_may_have_what_it_runs() {
        for aud in [AUD_STORE, AUD_QUEUE] {
            authorize("n", aud, &resources_for_audience("a.production", aud), &visible())
                .unwrap();
        }
        let mut pinned = resources_for_audience("a.production", AUD_STORE);
        pinned[0].generation = Some("gen-a".to_string());
        authorize("n", AUD_STORE, &pinned, &visible()).unwrap();
    }

    #[test]
    fn one_resource_outside_what_it_runs_refuses_the_request() {
        let mut asked = resources_for_audience("a.production", AUD_STORE);
        asked.extend(resources_for_audience("b.production", AUD_STORE));
        let refusal = authorize("n", AUD_STORE, &asked, &visible()).unwrap_err();
        assert!(refusal.contains("b.production"), "{refusal}");
    }

    #[test]
    fn a_binding_or_audience_nothing_checks_is_refused() {
        let mut wrong = resources_for_audience("a.production", AUD_STORE);
        wrong[0].binding = "queue".to_string();
        assert!(authorize("n", AUD_STORE, &wrong, &visible()).is_err());
        assert!(
            authorize(
                "n",
                "telemetry.read",
                &resources_for_audience("a.production", AUD_STORE),
                &visible()
            )
            .is_err()
        );
        assert!(authorize("n", AUD_STORE, &[], &visible()).is_err());
    }

    #[test]
    fn a_generation_that_is_not_deployed_is_refused() {
        let mut pinned = resources_for_audience("a.production", AUD_STORE);
        pinned[0].generation = Some("gen-old".to_string());
        assert!(authorize("n", AUD_STORE, &pinned, &visible()).is_err());
    }

    #[test]
    fn a_node_that_runs_nothing_gets_nothing() {
        assert!(
            authorize(
                "n",
                AUD_STORE,
                &resources_for_audience("a.production", AUD_STORE),
                &BTreeMap::new()
            )
            .is_err()
        );
    }
}
