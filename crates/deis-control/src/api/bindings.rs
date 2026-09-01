//! The second gate in front of a version upload: may this credential wire
//! this application to that one?
//!
//! [`super::authz`] answers "may this caller reach this route, for this
//! application, in this environment". It cannot answer the question a service
//! binding raises, because that question is about the *contents* of the bundle
//! being uploaded: a manifest saying `"AUTH": { "kind": "service", "app":
//! "auth-api" }` is one application claiming a right to call another, and the
//! route it arrives on names only the caller.
//!
//! # Two independent gates, and why neither is enough alone
//!
//! * **Consent** belongs to the callee and is checked on the node, on every
//!   call, against the callee's own current manifest
//!   (`deis_bundle::callable_by_allows`). It is not checked here at all, and
//!   deliberately: the callee may not be deployed yet, may be redeployed a
//!   minute later with a different list, and is in any case a different
//!   application whose state this upload has no business depending on.
//!   Refusing an upload because a callee has not consented *yet* would make
//!   two applications undeployable in either order.
//! * **Authority** belongs to whoever is uploading and is checked here, once,
//!   while somebody is watching. Consent alone would let anyone who can deploy
//!   application A point A at any application that has said `"*"`; authority
//!   alone would let a credential entitled to see B make B callable by
//!   something B never agreed to. Both have to open.
//!
//! # What authority a service binding demands
//!
//! [`Verb::Read`] over the callee, in the environment the binding names. Read
//! is the weakest verb in the grammar and every other verb implies it, so a
//! credential that may deploy, hold secrets for, or administer the callee also
//! passes - which is what makes this a boundary between *teams* rather than a
//! second deploy permission. What it stops is the credential scoped to one
//! application quietly acquiring a route into another team's.
//!
//! A binding that names no environment follows the caller's own wherever it is
//! deployed, so it can land in **any** environment of the callee and the
//! authority has to cover all of them: the demand is `read:<callee>/*`. A
//! binding that names one environment demands only that one. That asymmetry is
//! the honest reading of what each declaration can reach, and it gives an
//! author a way to need less authority - by writing the environment down.
//!
//! # When nothing is enforced
//!
//! `api_auth_enabled = false` leaves every caller anonymous and unscoped, so
//! every demand here passes, exactly as it does in [`super::authz`]. The
//! consent half is unaffected and remains the whole of the protection on such
//! an install - which is the right half to keep, because it is the one the
//! callee wrote.

use deis_bundle::{BindingKind, Manifest, ServiceEnvironment};

use crate::identity::{Demand, Principal, Selector, Verb};

/// A service binding this credential may not create.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BindingRefusal {
    /// The binding name in the manifest, so the author can find the line.
    pub binding: String,
    /// The application it wanted to call.
    pub callee: String,
    /// The scope that would have allowed it.
    pub required_scope: String,
    /// What the credential does hold, for the message and the log line.
    pub held: String,
}

impl std::fmt::Display for BindingRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "binding {:?} calls application {:?}, which this credential is not entitled to: it \
             would need {} and holds {}. Nothing was uploaded",
            self.binding, self.callee, self.required_scope, self.held
        )
    }
}

/// Refuse a bundle whose service bindings reach further than the credential
/// uploading it.
///
/// Storage bindings are not consulted: a namespace inside the uploader's own
/// tenant reaches nothing the credential does not already have, and demanding
/// a scope for one would mean a credential could deploy an application without
/// being allowed to give it a second bucket.
///
/// The first refusal wins and the whole upload is rejected. Accepting the rest
/// would produce a version whose manifest does not describe what it can do,
/// which is worse than a failed upload: the manifest is what an author and an
/// operator read afterwards.
pub fn check_service_bindings(
    principal: &Principal,
    manifest: &Manifest,
) -> Result<(), BindingRefusal> {
    for (name, binding) in manifest.bindings.iter() {
        if binding.kind != BindingKind::Service {
            continue;
        }
        let Some(callee) = binding.app.as_deref() else {
            // `Manifest::validate` has already refused this; nothing to judge.
            continue;
        };
        let named_environment = binding.service_environment();
        let environment = match &named_environment {
            // Follows the caller wherever it is deployed, so it can land in
            // any environment of the callee and the authority must cover all
            // of them. `Every`, not `Any`: `Any` would be answered by a scope
            // naming a single environment, which is precisely the reading that
            // is wrong here.
            ServiceEnvironment::Same => Selector::Every,
            ServiceEnvironment::Named(named) => Selector::One(named.as_str()),
        };
        let demand = Demand {
            verb: Verb::Read,
            app: Selector::One(callee),
            environment,
        };
        if principal.permits(&demand).is_err() {
            return Err(BindingRefusal {
                binding: name.to_string(),
                callee: callee.to_string(),
                required_scope: demand.required_scope(),
                held: principal.authority.describe(),
            });
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::identity::{Authority, PrincipalKind, Role, Scope};

    fn manifest_with(bindings: &str) -> Manifest {
        Manifest::parse(&format!(
            r#"{{"schema":1,"app":"frontend","entry":"worker/index.js","bindings":{bindings}}}"#
        ))
        .expect("the manifest itself is valid")
    }

    fn scoped(scopes: &[&str]) -> Principal {
        Principal {
            kind: PrincipalKind::Account,
            account: Some("ci".to_string()),
            role: Some(Role::Deployer),
            credential_id: Some("pub".to_string()),
            authority: Authority::Scoped(
                scopes
                    .iter()
                    .map(|text| Scope::parse(text).expect("a legal scope"))
                    .collect(),
            ),
        }
    }

    #[test]
    fn a_storage_binding_needs_no_authority_over_anything() {
        // It is a namespace inside the uploader's own tenant. Demanding a
        // scope for it would mean a credential could deploy an application
        // without being allowed to give it a second bucket.
        let manifest = manifest_with(r#"{"HOT":{"kind":"kv"},"DOCS":{"kind":"blob"}}"#);
        assert_eq!(
            check_service_bindings(&scoped(&["deploy:frontend/preview"]), &manifest),
            Ok(())
        );
    }

    #[test]
    fn a_credential_scoped_to_one_application_cannot_wire_it_to_another() {
        let manifest = manifest_with(r#"{"AUTH":{"kind":"service","app":"auth-api"}}"#);
        let refusal = check_service_bindings(&scoped(&["deploy:frontend/*"]), &manifest)
            .expect_err("must be refused");
        assert_eq!(refusal.callee, "auth-api");
        assert_eq!(refusal.required_scope, "read:auth-api/*");
        assert!(refusal.to_string().contains("AUTH"), "{refusal}");
    }

    #[test]
    fn the_weakest_verb_over_the_callee_is_enough() {
        // Read, not deploy: this is a boundary between teams, not a second
        // deploy permission. A credential that may look at the callee may
        // point something at it.
        let manifest = manifest_with(r#"{"AUTH":{"kind":"service","app":"auth-api"}}"#);
        assert_eq!(
            check_service_bindings(
                &scoped(&["deploy:frontend/*", "read:auth-api/*"]),
                &manifest
            ),
            Ok(())
        );
        // And every stronger verb implies it, so a credential that can deploy
        // the callee is not asked for a second scope.
        assert_eq!(
            check_service_bindings(
                &scoped(&["deploy:frontend/*", "deploy:auth-api/*"]),
                &manifest
            ),
            Ok(())
        );
    }

    #[test]
    fn a_binding_that_follows_the_caller_needs_authority_over_every_environment() {
        // It can land in any environment of the callee, because it lands in
        // whichever one the caller is deployed to. A scope covering one
        // environment does not answer that.
        let manifest = manifest_with(r#"{"AUTH":{"kind":"service","app":"auth-api"}}"#);
        let refusal = check_service_bindings(&scoped(&["read:auth-api/production"]), &manifest)
            .expect_err("one environment does not cover 'wherever the caller is'");
        assert_eq!(refusal.required_scope, "read:auth-api/*");

        // Writing the environment down is how an author asks for less.
        let pinned = manifest_with(
            r#"{"AUTH":{"kind":"service","app":"auth-api","environment":"production"}}"#,
        );
        assert_eq!(
            check_service_bindings(&scoped(&["read:auth-api/production"]), &pinned),
            Ok(())
        );
    }

    #[test]
    fn an_unscoped_credential_passes_exactly_as_it_does_on_every_other_route() {
        // The old flat key and the anonymous caller of an install with
        // `api_auth_enabled = false`. Consent, which the callee wrote, is the
        // whole of the protection there and is checked on the node.
        let manifest = manifest_with(r#"{"AUTH":{"kind":"service","app":"auth-api"}}"#);
        assert_eq!(
            check_service_bindings(&Principal::anonymous(), &manifest),
            Ok(())
        );
    }
}
