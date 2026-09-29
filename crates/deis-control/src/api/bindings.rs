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
//! # Consent is the protection, not the gate
//!
//! The credential gate in [`super::authz`] decides who may bind at all; what
//! a binding may name is decided here, by the consent the callee wrote. The
//! gate knowing a caller is entitled to read an application does not make its
//! kind of binding readable to this control plane, and a node one release
//! behind still cannot be talked into a kind it does not know (see below).
//!
//! # A binding this control plane cannot judge
//!
//! `bindings.<name>.kind` is an allow-list that grows, and a node one release
//! behind the builder reads a kind it does not know *narrowly*: the entry is
//! not published, so the application does less there and nothing else is lost
//! (`deis_bundle::forward`). That is the right answer for a node and the wrong
//! one here. This function is the only place that ever asks whether the
//! uploader may wire this application to another one - nothing re-checks at
//! delivery, and the node checks consent, not authority - and "not published
//! on an older node" is not "authorised": a newer node reads the same bundle
//! out of the archive, publishes the binding, and honours whatever a kind this
//! build has never heard of reaches, with no authority check ever having run.
//! An unknown kind may be a storage namespace or it may be a route into
//! another team's application; from here there is no telling, and a gate that
//! passes what it cannot read is not a gate.
//!
//! So a manifest carrying any kind this build cannot read is **refused at
//! upload**, whatever the credential holds and whether or not the entry names
//! an `app`, and the refusal names the binding, the kind and the way out: a
//! control plane that knows the kind. That is a refusal where the author is
//! present and the failure is loud, not the rollback failure the narrow
//! reading exists to prevent - a version already in the catalog was judged by
//! the control plane that accepted it, and neither a deploy nor a rollback
//! runs this gate again, so rolling a control plane back never makes an
//! accepted version undeployable. Today no such kind exists; the gate is here
//! so that the day one does, a control plane that predates it fails closed.

use deis_bundle::{BindingKind, Manifest, ServiceEnvironment};

use crate::identity::{Demand, Principal, Selector, Verb};

/// A binding this upload may not carry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BindingRefusal {
    /// A service binding this credential is not entitled to create.
    Unentitled {
        /// The binding name in the manifest, so the author can find the line.
        binding: String,
        /// The application it wanted to call.
        callee: String,
        /// The scope that would have allowed it.
        required_scope: String,
        /// What the credential does hold, for the message and the log line.
        held: String,
    },
    /// A binding of a kind this control plane cannot read, and therefore
    /// cannot judge - see the module documentation for why that is a refusal
    /// and not a pass.
    Unjudgeable {
        /// The binding name in the manifest.
        binding: String,
        /// The kind the manifest asked for, as written.
        kind: String,
        /// The `app` the entry names, when it names one. Quoted in the
        /// message because it is the strongest hint of what the binding
        /// reaches; not relied on, because an unknown kind may reach another
        /// application without one.
        app: Option<String>,
    },
}

impl BindingRefusal {
    /// The binding name, whichever refusal this is.
    pub fn binding(&self) -> &str {
        match self {
            Self::Unentitled { binding, .. } | Self::Unjudgeable { binding, .. } => binding,
        }
    }
}

impl std::fmt::Display for BindingRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unentitled {
                binding,
                callee,
                required_scope,
                held,
            } => write!(
                f,
                "binding {binding:?} calls application {callee:?}, which this credential is not \
                 entitled to: it would need {required_scope} and holds {held}. Nothing was \
                 uploaded"
            ),
            Self::Unjudgeable { binding, kind, app } => {
                let reaches = match app {
                    Some(app) => format!(
                        "it names application {app:?}, and an unknown kind may reach it in a way \
                         this build cannot see"
                    ),
                    None => "it names no application, but an unknown kind may cross into another \
                             tenant in a way this build cannot see"
                        .to_string(),
                };
                write!(
                    f,
                    "binding {binding:?} has kind {kind:?}, which this control plane cannot read, \
                     so it cannot judge whether the binding reaches another application: \
                     {reaches}. A binding is only delivered once this gate has judged it - a \
                     node that cannot read the kind would leave it unpublished, but a newer node \
                     would honour it with no authority check ever having run. Upgrade the \
                     control plane to a release that knows kind {kind:?}, or build against the \
                     kinds this one has ({}). Nothing was uploaded",
                    BindingKind::ALL
                        .iter()
                        .map(|kind| kind.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            }
        }
    }
}

/// Refuse a bundle whose service bindings reach further than the credential
/// uploading it, or whose bindings this control plane cannot judge at all.
///
/// Storage bindings are not consulted: a namespace inside the uploader's own
/// tenant reaches nothing the credential does not already have, and demanding
/// a scope for one would mean a credential could deploy an application without
/// being allowed to give it a second bucket.
///
/// A binding whose kind this build cannot read is refused before anything is
/// judged, and regardless of the principal - an unscoped caller included,
/// because the question is not what the credential holds but whether this
/// control plane can tell what it is being asked to allow. See the module
/// documentation.
///
/// The first refusal wins and the whole upload is rejected. Accepting the rest
/// would produce a version whose manifest does not describe what it can do,
/// which is worse than a failed upload: the manifest is what an author and an
/// operator read afterwards.
pub fn check_service_bindings(
    principal: &Principal,
    manifest: &Manifest,
) -> Result<(), BindingRefusal> {
    // `iter()` below is the bindings this build can act on and deliberately
    // hides the ones it cannot; this gate has to see those too, or it passes
    // exactly the binding it cannot judge.
    if let Some((name, entry)) = manifest.bindings.unreadable_entries().next() {
        return Err(BindingRefusal::Unjudgeable {
            binding: name.to_string(),
            kind: entry
                .get("kind")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("<not a string>")
                .to_string(),
            app: entry
                .get("app")
                .and_then(serde_json::Value::as_str)
                .map(str::trim)
                .filter(|app| !app.is_empty())
                .map(str::to_string),
        });
    }

    for (name, binding) in manifest.bindings.iter() {
        if binding.kind != BindingKind::Service {
            continue;
        }
        // Trimmed, and empty means unnamed, because that is the spelling
        // every other reader of this field uses: `DeclaredBindings::validate`
        // checks `binding.app.as_deref().map(str::trim)`, so a padded name is
        // a manifest this platform accepts, and the node rebuilds its binding
        // table from the environment line with `str::trim` on every field, so
        // the padded name is *not* what the call is dispatched to. Judging
        // the raw spelling meant checking one name and running another: no
        // scope can contain a space (`identity::scope::Component`), so a
        // credential holding exactly `read:auth-api/*` was refused a binding
        // to `" auth-api "` and told to obtain `read: auth-api /*`, which is
        // not a scope anybody can be granted.
        let Some(callee) = binding
            .app
            .as_deref()
            .map(str::trim)
            .filter(|callee| !callee.is_empty())
        else {
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
            return Err(BindingRefusal::Unentitled {
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
        Manifest::parse(&deis_bundle::complete_manifest_json(&format!(
            r#"{{"app":"frontend","entry":"worker/index.js","bindings":{bindings}}}"#
        )))
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

    /// The two fields a test reads off an entitlement refusal.
    fn unentitled(refusal: &BindingRefusal) -> (&str, &str) {
        match refusal {
            BindingRefusal::Unentitled {
                callee,
                required_scope,
                ..
            } => (callee, required_scope),
            other => panic!("expected an entitlement refusal, got {other:?}"),
        }
    }

    #[test]
    fn a_credential_scoped_to_one_application_cannot_wire_it_to_another() {
        let manifest = manifest_with(r#"{"AUTH":{"kind":"service","app":"auth-api"}}"#);
        let refusal = check_service_bindings(&scoped(&["deploy:frontend/*"]), &manifest)
            .expect_err("must be refused");
        assert_eq!(unentitled(&refusal), ("auth-api", "read:auth-api/*"));
        assert_eq!(refusal.binding(), "AUTH");
        assert!(refusal.to_string().contains("AUTH"), "{refusal}");
    }

    /// A credential that holds every scope there is.
    fn holding_everything() -> Principal {
        Principal {
            kind: PrincipalKind::Account,
            account: Some("root".to_string()),
            role: Some(Role::Deployer),
            credential_id: Some("pub".to_string()),
            authority: Authority::Scoped(
                ["deploy:*/*", "read:*/*", "secrets:*/*", "admin:*/*"]
                    .iter()
                    .filter_map(|text| Scope::parse(text).ok())
                    .collect(),
            ),
        }
    }

    /// An unscoped owner credential, which is what a fresh login ends up
    /// holding: no scope list, so its account's role is the only limit.
    fn unscoped_owner() -> Principal {
        Principal {
            kind: PrincipalKind::Account,
            account: Some("root".to_string()),
            role: Some(Role::Owner),
            credential_id: Some("pub".to_string()),
            authority: Authority::Unscoped,
        }
    }

    #[test]
    fn a_binding_kind_this_control_plane_cannot_read_is_refused_whatever_the_credential_holds() {
        // The hole this closes: `iter()` hides an entry whose kind this build
        // cannot read, so a control plane one release behind the builder used
        // to walk past a cross-application binding it could not judge, and a
        // newer node then honoured it with no authority check ever having
        // run. The entry is refused here, before any scope is consulted, and
        // the refusal names the binding, the kind and the way out.
        let manifest = manifest_with(r#"{"NEIGHBOUR":{"kind":"mesh","app":"other-team-api"}}"#);
        assert!(
            manifest.bindings.get("NEIGHBOUR").is_none(),
            "the fixture only means something if this build really cannot read the kind"
        );

        for principal in [
            holding_everything(),
            scoped(&["read:other-team-api/*", "deploy:frontend/*"]),
            unscoped_owner(),
        ] {
            let refusal = check_service_bindings(&principal, &manifest).expect_err(
                "a kind this control plane cannot read must be refused even by a credential that \
                 would be entitled to any known kind of binding to that application",
            );
            assert_eq!(
                refusal,
                BindingRefusal::Unjudgeable {
                    binding: "NEIGHBOUR".to_string(),
                    kind: "mesh".to_string(),
                    app: Some("other-team-api".to_string()),
                }
            );
            let message = refusal.to_string();
            assert!(message.contains("NEIGHBOUR"), "{message}");
            assert!(message.contains("\"mesh\""), "{message}");
            assert!(message.contains("other-team-api"), "{message}");
            assert!(
                message.contains("Upgrade the control plane"),
                "the refusal has to name the way out: {message}"
            );
            assert!(message.contains("Nothing was uploaded"), "{message}");
        }
    }

    #[test]
    fn an_unreadable_kind_that_names_no_application_is_refused_too_and_says_why() {
        // A kind this build has never seen may cross into another tenant
        // without an `app` field - a future kind could name its target some
        // other way, or reach one implicitly. The gate cannot know, so it
        // does not guess: refused, with a message that says the absence of
        // `app` is not what decides it.
        let manifest = manifest_with(r#"{"HOT":{"kind":"kv"},"GRAPH":{"kind":"vector"}}"#);
        let refusal = check_service_bindings(&holding_everything(), &manifest)
            .expect_err("an unknown kind with no app is still unjudgeable");
        assert_eq!(
            refusal,
            BindingRefusal::Unjudgeable {
                binding: "GRAPH".to_string(),
                kind: "vector".to_string(),
                app: None,
            }
        );
        let message = refusal.to_string();
        assert!(message.contains("names no application"), "{message}");
        assert!(message.contains("cannot see"), "{message}");

        // And it is the *unreadable* entry that decides, not the readable
        // storage binding beside it: the same manifest without GRAPH passes.
        let readable = manifest_with(r#"{"HOT":{"kind":"kv"}}"#);
        assert_eq!(
            check_service_bindings(&holding_everything(), &readable),
            Ok(())
        );
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
        assert_eq!(unentitled(&refusal).1, "read:auth-api/*");

        // Writing the environment down is how an author asks for less.
        let pinned = manifest_with(
            r#"{"AUTH":{"kind":"service","app":"auth-api","environment":"production"}}"#,
        );
        assert_eq!(
            check_service_bindings(&scoped(&["read:auth-api/production"]), &pinned),
            Ok(())
        );
    }

    /// The name in the manifest and the name the platform acts on have to be
    /// the same one.
    ///
    /// `DeclaredBindings::validate` trims `app` before it checks it, so a
    /// padded spelling is a manifest this platform accepts; the node rebuilds
    /// its binding table from the deployment's environment line and trims
    /// every field, so the call is dispatched to the trimmed name. Judging
    /// the raw spelling therefore checked a name nothing would ever call -
    /// and, because no scope can contain a space, it refused the credential
    /// that holds exactly the right authority and told it to go and ask for a
    /// scope the parser rejects.
    #[test]
    fn a_padded_callee_is_judged_as_the_name_the_node_will_call() {
        let manifest = manifest_with(r#"{"AUTH":{"kind":"service","app":"  auth-api\n"}}"#);
        assert_eq!(
            manifest.bindings.get("AUTH").and_then(|b| b.app.as_deref()),
            Some("  auth-api\n"),
            "the manifest keeps what the author wrote; this test is about how it is read"
        );

        assert_eq!(
            check_service_bindings(
                &scoped(&["deploy:frontend/*", "read:auth-api/*"]),
                &manifest
            ),
            Ok(()),
            "the credential holds authority over the application the node will call"
        );

        // And the padding buys nothing: the callee is still judged, and the
        // refusal still names a scope somebody could actually be granted.
        let refusal = check_service_bindings(&scoped(&["deploy:frontend/*"]), &manifest)
            .expect_err("this credential has never heard of the callee");
        let (callee, required_scope) = unentitled(&refusal);
        assert_eq!(callee, "auth-api");
        assert_eq!(required_scope, "read:auth-api/*");
        assert!(
            Scope::parse(required_scope).is_ok(),
            "a refusal that names an unparseable scope leaves the caller with nothing to ask for"
        );
    }

    #[test]
    fn an_unscoped_credential_passes_exactly_as_it_does_on_every_other_route() {
        // Consent, which the callee wrote, is the whole of the protection
        // there and is checked on the node; the consent check itself asks
        // only that the caller be entitled to read what it binds to, and an
        // unscoped owner is.
        let manifest = manifest_with(r#"{"AUTH":{"kind":"service","app":"auth-api"}}"#);
        assert_eq!(check_service_bindings(&unscoped_owner(), &manifest), Ok(()));
    }
}
