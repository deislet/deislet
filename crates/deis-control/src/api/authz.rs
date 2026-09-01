//! One gate in front of the human API: who is calling, may they, and write
//! it down.
//!
//! Everything the dashboard and the `deis` CLI reach goes through
//! [`authorize`]. That is deliberate. Spreading the check across handlers is
//! how a route ends up shipped with no check at all - this repository has
//! produced that defect before - so the requirement for every route lives in
//! one table ([`classify`]) which is a pure function of the method, the path
//! and the query, and the middleware that applies it is layered where no
//! route can be mounted around it.
//!
//! # Fail closed
//!
//! [`classify`] has a final arm that demands `admin:*/*` for anything it does
//! not recognise. A route added without a line in the table therefore becomes
//! reachable only by an administrator, and is visible as such the first time
//! a scoped token is refused - rather than becoming an unguarded hole.
//!
//! The same rule governs the two [`Resource`] cases that are not a name.
//! [`Resource::Any`] is answered by *any one* scope the caller holds, so it
//! belongs only on a call whose effect is confined to one of the things the
//! caller can already reach - reading the application record, uploading a
//! bundle that is stored and put nowhere. A call that acts on all of them at
//! once is [`Resource::Every`], which only a `*` answers. Writing `Any` where
//! the effect is `Every` does not look like a hole in the table; it looks
//! like a line that is present. `DELETE /api/runtime/:node/apps/:app` was
//! exactly that, and one environment's deploy credential could stop every
//! environment of the application on a node with it.
//!
//! # What `api_auth_enabled = false` means, and what it does not
//!
//! It means no credential is *required*. It has never been allowed to mean
//! that a credential which *is* presented goes unchecked, and it must not
//! mean that the whole human API is published to whatever can route to this
//! port. With it off, [`resolve`] admits an unauthenticated caller only from
//! the machine this control plane runs on, and a credential that resolves is
//! held to its own scopes exactly as it would be with the flag on.
//!
//! # The query string is read for exactly one route
//!
//! `DELETE /api/runtime/:node/apps/:app` takes one environment of an
//! application off a node with `?environment=<name>`, and *every* environment
//! of it without. The path is identical either way, so the table reads that
//! one parameter; see [`narrowing_query_value`] for why it narrows the demand
//! only for a spelling that can mean one environment and nothing else.
//!
//! # The body is read for exactly two routes
//!
//! `POST /api/apps` and `POST /api/apps/:app/environments` name the thing
//! they create in the request body, not in the path, and `deis deploy` calls
//! both of them for a CI credential that is scoped to one environment. So the
//! gate peeks at those two bodies - and only those two, capped at
//! [`MAX_BODY_PEEK_BYTES`], with the body handed on to the handler unchanged.
//! No route that carries a secret is on that list, which is what keeps a
//! value out of the audit log by construction rather than by care.

use std::net::SocketAddr;
use std::sync::Arc;

use axum::{
    Json,
    extract::{ConnectInfo, Request, State},
    http::{HeaderMap, Method, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
};
use serde_json::json;
use tracing::{info, warn};

use crate::audit::{AuditRecord, AuditStore, Outcome};
use crate::identity::store::Resolution;
use crate::identity::{Demand, IdentityStore, Principal, Selector, Verb};

use super::middleware::{presented_credentials, secret_eq};

/// Largest request body the gate will read in order to learn which
/// application or environment a call is about. The two routes it applies to
/// carry a JSON object with one field.
pub const MAX_BODY_PEEK_BYTES: usize = 64 * 1024;

/// The identical answer every unusable credential gets. A caller must not be
/// able to tell "no such token" from "revoked" from "the account is switched
/// off" - that is a free oracle for anyone holding a stolen credential - so
/// the three cases differ only in the control plane's own log.
const UNAUTHENTICATED_DETAIL: &str = "A valid credential is required. Present it as \
                                      'Authorization: Bearer <token>' or 'X-API-Key: <token>'. \
                                      Mint one with `deis token create`.";

/// What a caller off the box is told when this control plane demands no
/// credential at all. It names the setting rather than repeating
/// [`UNAUTHENTICATED_DETAIL`], because there is no token to ask anyone for:
/// nothing has been minted, and an operator sent looking for one would find
/// nothing. Nothing is given away by saying so - a caller who cannot present
/// a credential learns only that there is none to steal.
const UNCONFIGURED_OFF_BOX_DETAIL: &str = "This control plane requires no credential \
                                          (api_auth_enabled is off), so it serves the human API \
                                          only to callers on the machine it runs on. Set \
                                          api_auth_enabled = true in its configuration and \
                                          restart: it prints one owner token at startup, and \
                                          every caller - on this machine or not - presents that.";

/// Which component of the resource a route names in its body.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BodyComponent {
    App,
    Environment,
}

/// One component of what a request is about, before the body has been read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resource {
    /// All of them at once. Only a `*` in a scope answers this.
    Every,
    /// Not about one of them - a version upload names no environment. Any
    /// one the caller holds is enough, which is why this belongs only where
    /// the call cannot reach past what that one already gives them. When the
    /// call acts on all of them, the answer is [`Resource::Every`].
    Any,
    /// This one, by name, out of the path.
    Named(String),
}

impl Resource {
    fn selector(&self) -> Selector<'_> {
        match self {
            Resource::Every => Selector::Every,
            Resource::Any => Selector::Any,
            Resource::Named(name) => Selector::One(name),
        }
    }

    fn name(&self) -> Option<String> {
        match self {
            Resource::Named(name) => Some(name.clone()),
            _ => None,
        }
    }
}

/// When a call earns a line in the audit log.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuditWhen {
    /// Every time, whatever the outcome. Every state-changing call, plus the
    /// storage-token mint, which is a read that hands back a credential.
    Always,
    /// Only when it was refused. A successful read is not an event; a refused
    /// one is somebody finding out what they cannot reach.
    OnRefusal,
}

/// What one route needs before it may run, and what the log should call it.
#[derive(Debug, Clone)]
pub struct Requirement {
    /// The stable name of the operation, as the audit log records it. Taken
    /// from here rather than from the URL so a route that moves does not
    /// break every query somebody wrote.
    pub action: &'static str,
    pub verb: Verb,
    pub app: Resource,
    pub environment: Resource,
    /// The thing inside the application this call names - a secret's name, a
    /// route id, a token id. Never a value.
    pub target: Option<String>,
    pub audit: AuditWhen,
    /// The component this route names in its body instead of in its path,
    /// and the JSON field that holds it.
    pub from_body: Option<(BodyComponent, &'static str)>,
}

impl Requirement {
    fn new(action: &'static str, verb: Verb, app: Resource, environment: Resource) -> Self {
        Self {
            action,
            verb,
            app,
            environment,
            target: None,
            audit: AuditWhen::OnRefusal,
            from_body: None,
        }
    }

    fn changes_state(mut self) -> Self {
        self.audit = AuditWhen::Always;
        self
    }

    fn about(mut self, target: &str) -> Self {
        self.target = Some(target.to_string());
        self
    }

    fn named_in_body(mut self, component: BodyComponent, field: &'static str) -> Self {
        self.from_body = Some((component, field));
        self
    }
}

fn named(value: &str) -> Resource {
    Resource::Named(value.to_string())
}

/// The one value in a query string that may narrow what a route is about, or
/// `None` if the query does not narrow it at all.
///
/// This deliberately does no percent-decoding and no `+` unescaping, and it
/// answers `None` the moment the query is anything but one plain occurrence
/// of `key=<name>`. The gate and the handler must agree on which environment
/// a call is about, and the handler reads its query through `serde_urlencoded`,
/// so any decoder written here would be a second implementation that could
/// disagree with it; a disagreement is a demand naming one environment while
/// the node is told to unload another. Answering `None` instead costs a caller
/// who wrote `%70roduction` nothing but a wildcard scope, and it makes every
/// case this cannot read exactly (an encoded spelling, an empty value, the key
/// given twice with different values) fall to the wider demand rather than the
/// narrower one.
///
/// A name that needs no encoding is what this platform's names are: an
/// environment is 1 to 64 characters of lowercase letters, digits, `-` and
/// `_` (`identity::scope::Component`), none of which a query string escapes.
fn narrowing_query_value<'a>(query: Option<&'a str>, key: &str) -> Option<&'a str> {
    let mut found: Option<&str> = None;
    for pair in query?.split('&') {
        let Some((name, value)) = pair.split_once('=') else {
            continue;
        };
        if name != key {
            continue;
        }
        let plain = !value.is_empty()
            && value
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_');
        if !plain {
            return None;
        }
        match found {
            // The same value twice is the same request; two different ones
            // are a request this cannot read as being about one environment.
            Some(first) if first == value => {}
            Some(_) => return None,
            None => found = Some(value),
        }
    }
    found
}

/// What this method, path and query need. A pure function, so the whole
/// authorisation surface of this control plane can be read - and tested -
/// without starting one.
///
/// `query` is the raw query string, without the `?`, exactly as
/// [`axum::http::Uri::query`] hands it over. One route reads it; see the
/// module header.
pub fn classify(method: &Method, path: &str, query: Option<&str>) -> Requirement {
    let segments: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    let writing = method != Method::GET && method != Method::HEAD;

    match segments.as_slice() {
        // ---- applications ------------------------------------------------
        ["api", "apps"] if !writing => {
            Requirement::new("app.list", Verb::Read, Resource::Every, Resource::Every)
        }
        ["api", "apps"] => Requirement::new(
            "app.create",
            Verb::Deploy,
            // The name of the application being created is in the body; the
            // gate reads it so a CI credential scoped to one application can
            // still create that one and no other.
            Resource::Any,
            Resource::Any,
        )
        .named_in_body(BodyComponent::App, "id")
        .changes_state(),

        ["api", "apps", app] if !writing => {
            Requirement::new("app.get", Verb::Read, named(app), Resource::Any)
        }
        // Deleting an application takes its versions, environments and
        // secrets with it, so it answers to the platform's administrator
        // rather than to whoever may deploy into one of its environments.
        ["api", "apps", app] => {
            Requirement::new("app.delete", Verb::Admin, named(app), Resource::Every).changes_state()
        }

        // ---- names of deleted applications --------------------------------
        //
        // Reading the list names every application that was ever deleted on
        // this platform, and releasing one decides that the next application
        // of that name inherits the deleted one's stored data - so both
        // answer to the administrator that `app.delete` answers to, and
        // neither can be reached by a credential scoped to an application.
        // `Resource::Every` on the release for the same reason the delete has
        // it: what it hands over is not confined to one environment.
        ["api", "retired-apps"] => Requirement::new(
            "retired_app.list",
            Verb::Admin,
            Resource::Every,
            Resource::Every,
        ),
        ["api", "retired-apps", app] => Requirement::new(
            "retired_app.release",
            Verb::Admin,
            named(app),
            Resource::Every,
        )
        .changes_state(),

        // A version is uploaded to the application, not to an environment:
        // it is stored and put nowhere. Whoever may deploy into any one of
        // this application's environments may upload the bundle they will
        // deploy.
        ["api", "apps", app, "versions"] if !writing => {
            Requirement::new("version.list", Verb::Read, named(app), Resource::Any)
        }
        ["api", "apps", app, "versions"] => {
            Requirement::new("version.upload", Verb::Deploy, named(app), Resource::Any)
                .changes_state()
        }
        ["api", "apps", app, "versions", version] => {
            Requirement::new("version.get", Verb::Read, named(app), Resource::Any).about(version)
        }

        ["api", "apps", app, "environments"] if !writing => {
            Requirement::new("environment.list", Verb::Read, named(app), Resource::Any)
        }
        ["api", "apps", app, "environments"] => Requirement::new(
            "environment.create",
            Verb::Deploy,
            named(app),
            Resource::Any,
        )
        .named_in_body(BodyComponent::Environment, "name")
        .changes_state(),

        ["api", "apps", app, "environments", env] => {
            Requirement::new("environment.get", Verb::Read, named(app), named(env))
        }

        ["api", "apps", app, "environments", env, "deployment"] => {
            Requirement::new("deployment.get", Verb::Read, named(app), named(env))
        }
        ["api", "apps", app, "environments", env, "deployments"] if !writing => {
            Requirement::new("deployment.list", Verb::Read, named(app), named(env))
        }
        ["api", "apps", app, "environments", env, "deployments"] => {
            Requirement::new("deploy", Verb::Deploy, named(app), named(env)).changes_state()
        }
        ["api", "apps", app, "environments", env, "rollback"] => {
            Requirement::new("rollback", Verb::Deploy, named(app), named(env)).changes_state()
        }

        ["api", "apps", app, "environments", env, "routes"] if !writing => {
            Requirement::new("route.list", Verb::Read, named(app), named(env))
        }
        ["api", "apps", app, "environments", env, "routes"] => {
            Requirement::new("route.create", Verb::Deploy, named(app), named(env)).changes_state()
        }
        ["api", "apps", app, "environments", env, "routes", route] if method == Method::DELETE => {
            Requirement::new("route.delete", Verb::Deploy, named(app), named(env))
                .about(route)
                .changes_state()
        }
        ["api", "apps", app, "environments", env, "routes", route] => {
            Requirement::new("route.update", Verb::Deploy, named(app), named(env))
                .about(route)
                .changes_state()
        }

        // Listing names is a read; writing and deleting a value is its own
        // verb, so a credential that ships code cannot rotate what the code
        // runs against.
        ["api", "apps", app, "environments", env, "secrets"] => {
            Requirement::new("secret.list", Verb::Read, named(app), named(env))
        }
        ["api", "apps", app, "environments", env, "secrets", name] if method == Method::DELETE => {
            Requirement::new("secret.delete", Verb::Secrets, named(app), named(env))
                .about(name)
                .changes_state()
        }
        ["api", "apps", app, "environments", env, "secrets", name] => {
            Requirement::new("secret.write", Verb::Secrets, named(app), named(env))
                .about(name)
                .changes_state()
        }

        // The sharpest edge on this API. What comes back talks to deis-store
        // directly, as that tenant, without passing through this process
        // again, and never expires - so it has its own verb, that verb is
        // implied by nothing, and a scope carrying it has to name this exact
        // application and environment (see `identity::scope`). Recorded every
        // time, because it is the issue of a credential.
        ["api", "apps", app, "environments", env, "storage-token"] => {
            Requirement::new("storage_token.mint", Verb::Storage, named(app), named(env))
                .changes_state()
        }

        // ---- accounts, tokens, the audit log ------------------------------
        ["api", "accounts"] if !writing => Requirement::new(
            "account.list",
            Verb::Admin,
            Resource::Every,
            Resource::Every,
        ),
        ["api", "accounts"] => Requirement::new(
            "account.create",
            Verb::Admin,
            Resource::Every,
            Resource::Every,
        )
        .changes_state(),
        ["api", "accounts", name] if !writing => {
            Requirement::new("account.get", Verb::Admin, Resource::Every, Resource::Every)
                .about(name)
        }
        ["api", "accounts", name] => Requirement::new(
            "account.update",
            Verb::Admin,
            Resource::Every,
            Resource::Every,
        )
        .about(name)
        .changes_state(),
        ["api", "accounts", name, "tokens"] => {
            Requirement::new("token.mint", Verb::Admin, Resource::Every, Resource::Every)
                .about(name)
                .changes_state()
        }
        ["api", "tokens"] => {
            Requirement::new("token.list", Verb::Admin, Resource::Every, Resource::Every)
        }
        ["api", "tokens", id] => Requirement::new(
            "token.revoke",
            Verb::Admin,
            Resource::Every,
            Resource::Every,
        )
        .about(id)
        .changes_state(),
        // Reading the log is an administrator's, because it names every
        // application on the platform and everybody who touched one.
        ["api", "audit"] => {
            Requirement::new("audit.read", Verb::Admin, Resource::Every, Resource::Every)
        }

        // ---- the platform itself -----------------------------------------
        ["api", "routes"] => Requirement::new(
            "route.list_all",
            Verb::Read,
            Resource::Every,
            Resource::Every,
        ),
        ["api", "sessions", ..] => {
            Requirement::new("session.read", Verb::Read, Resource::Every, Resource::Every)
        }
        ["api", "events", ..] => {
            Requirement::new("event.read", Verb::Read, Resource::Every, Resource::Every)
        }
        ["api", "metrics", ..] => {
            Requirement::new("metrics.read", Verb::Read, Resource::Every, Resource::Every)
        }
        ["api", "configs", ..] if !writing => Requirement::new(
            "node.config.read",
            Verb::Read,
            Resource::Every,
            Resource::Every,
        ),
        ["api", "configs", ..] => Requirement::new(
            "node.config.write",
            Verb::Admin,
            Resource::Every,
            Resource::Every,
        )
        .changes_state(),
        ["api", "commands", ..] if !writing => Requirement::new(
            "node.command.read",
            Verb::Read,
            Resource::Every,
            Resource::Every,
        ),
        ["api", "commands", ..] => Requirement::new(
            "node.command",
            Verb::Admin,
            Resource::Every,
            Resource::Every,
        )
        .changes_state(),

        // The bare fleet list is which machines exist and whether they are
        // healthy. Any credential with any scope may see it, because
        // `deis status` prints it and a credential scoped to one environment
        // still deploys onto those machines.
        ["api", "runtime"] => {
            Requirement::new("node.list", Verb::Read, Resource::Any, Resource::Any)
        }
        // What one node is running names every application on it, which is
        // another tenant's business.
        ["api", "runtime", _node, "status"] => {
            Requirement::new("node.status", Verb::Read, Resource::Every, Resource::Every)
        }
        // Stopping an application on a node. `?environment=<name>` takes
        // that one off; with nothing named the node takes off *every*
        // environment of the application it is running, so the demand is the
        // whole application and only a `*` answers it. `Resource::Any` was
        // what stood here, and any single environment scope answered it - so
        // a preview pipeline's credential could stop production, repeatedly,
        // because the node re-adds the application on its next reconcile and
        // the call can simply be made again.
        ["api", "runtime", _node, "apps", app] => {
            let environment = match narrowing_query_value(query, "environment") {
                Some(name) => named(name),
                None => Resource::Every,
            };
            Requirement::new("node.app.unload", Verb::Deploy, named(app), environment)
                .changes_state()
        }

        // Anything this table does not know is an administrator's until
        // somebody writes down what it should be. See the module header.
        _ => Requirement::new(
            "unclassified",
            Verb::Admin,
            Resource::Every,
            Resource::Every,
        )
        .changes_state(),
    }
}

/// What the gate needs in order to answer a request.
#[derive(Clone)]
pub struct AuthzState {
    /// `api_auth_enabled`. It decides one thing only: whether a caller has to
    /// present a credential at all. It has never decided whether a credential
    /// that *is* presented is checked - see [`authorize`] - and with it off
    /// the caller who presents nothing is admitted only from this machine,
    /// see [`resolve`].
    pub enforced: bool,
    /// The `api_keys` entries from the configuration file. Still checked
    /// here, against the file, and never against the database.
    pub config_keys: Arc<Vec<String>>,
    pub identity: Arc<IdentityStore>,
    pub audit: Arc<AuditStore>,
}

/// Authenticate, authorise, and record.
///
/// The order matters and is the same on every route:
///
/// 1. resolve the presented credential into a [`Principal`], or refuse;
/// 2. work out what this route needs from [`classify`], reading the body only
///    for the two routes that name their resource there;
/// 3. check the principal against it;
/// 4. run the request;
/// 5. write the audit line, with the outcome the response actually had.
///
/// With `api_auth_enabled` off, step 1 admits a caller who presented nothing
/// as anonymous - but only from a loopback peer, because a control plane
/// listening on 0.0.0.0 with nothing required is otherwise handing the whole
/// human API to anything that can route to the port. Step 3 runs whatever the
/// flag says: not requiring a credential cannot mean that a `deploy:app/preview`
/// token which is presented may mint a storage capability for another tenant.
/// Step 5 runs on every path, so even an unauthenticated platform gets a
/// record of every deploy, rollback and secret change.
pub async fn authorize(State(state): State<AuthzState>, request: Request, next: Next) -> Response {
    let path = request.uri().path().to_string();
    // Unchanged from the flat-key days: the health probe has no caller, and
    // Prometheus generally cannot present a credential.
    if path == "/health" || path == "/api/metrics" {
        return next.run(request).await;
    }

    let method = request.method().clone();
    let headers = request.headers().clone();
    let query = request.uri().query().map(str::to_string);
    // Kept as an address and not only as text: with nothing enforced it is
    // what decides whether an unauthenticated caller is admitted at all.
    let peer_addr = request
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|ConnectInfo(addr)| *addr);
    let peer = peer_addr.map(|addr| addr.to_string());

    let principal = match resolve(&state, &headers, peer_addr).await {
        Ok(principal) => principal,
        Err(refusal) => {
            let requirement = classify(&method, &path, query.as_deref());
            warn!(
                %method,
                path = %path,
                peer = peer.as_deref().unwrap_or("an unknown address"),
                reason = refusal.log_reason,
                credential = refusal.credential.as_deref().unwrap_or("none"),
                "Refused a call to the human API because nothing it presented is a credential this control plane will act on; what the caller is told carries no clue why, so a stolen token does not learn from it that it was withdrawn"
            );
            state
                .audit
                .record(
                    AuditRecord::by_unknown_caller()
                        .action(requirement.action)
                        .request(method.as_str(), &path)
                        .resource(requirement.app.name(), requirement.environment.name())
                        .target(requirement.target.clone())
                        // A revoked token names itself here even though its
                        // account could not be used: that id is what an
                        // operator needs in order to find whatever is still
                        // configured with a credential they withdrew.
                        .credential(refusal.credential.clone())
                        .peer(peer)
                        .outcome(Outcome::Refused, 401)
                        .detail(refusal.log_reason)
                        .build(),
                )
                .await;
            return unauthenticated(refusal.caller_detail);
        }
    };

    let mut requirement = classify(&method, &path, query.as_deref());

    // The two routes that name their resource in the body. Everything else
    // keeps its body untouched and unread.
    let request = match requirement.from_body {
        None => request,
        Some((component, field)) => match peek_body_field(request, field).await {
            Ok((request, value)) => {
                if let Some(value) = value {
                    match component {
                        BodyComponent::App => requirement.app = Resource::Named(value),
                        BodyComponent::Environment => {
                            requirement.environment = Resource::Named(value)
                        }
                    }
                }
                request
            }
            Err(response) => return response,
        },
    };

    let demand = Demand {
        verb: requirement.verb,
        app: requirement.app.selector(),
        environment: requirement.environment.selector(),
    };

    // Unconditionally, whatever `api_auth_enabled` says. That flag decides
    // whether a credential is *required*; a credential that was presented and
    // resolved carries scopes its holder was given on the understanding that
    // they are the limit, and skipping this handed a token scoped to one
    // preview environment the whole platform on every install that had not
    // turned the flag on. The anonymous caller of such an install is
    // `Authority::Unscoped` and still passes here, exactly as it always did.
    if let Err(denial) = principal.permits(&demand) {
        warn!(
            %method,
            path = %path,
            caller = %principal.label(),
            peer = peer.as_deref().unwrap_or("an unknown address"),
            needed = %demand.required_scope(),
            held = %principal.authority.describe(),
            action = requirement.action,
            "Refused a call to the human API: the caller authenticated and is not entitled to it. Nothing was changed, and the refusal is in the audit log"
        );
        let detail = denial.to_string();
        state
            .audit
            .record(
                AuditRecord::by(&principal)
                    .action(requirement.action)
                    .request(method.as_str(), &path)
                    .resource(requirement.app.name(), requirement.environment.name())
                    .target(requirement.target.clone())
                    .peer(peer)
                    .outcome(Outcome::Refused, 403)
                    .detail(detail.clone())
                    .build(),
            )
            .await;
        return forbidden(&demand.required_scope(), &detail);
    }

    // Handlers that want to know who is calling read it from here. Inserted
    // even when nothing is enforced, so a handler never has to ask whether
    // there is a caller.
    let mut request = request;
    request.extensions_mut().insert(principal.clone());

    let response = next.run(request).await;
    let status = response.status();

    // A refusal is recorded whatever the route, because it is the line that
    // shows a credential reaching for something it may not have. Everything
    // else follows the route's own rule: a 404 on a plain read is the caller
    // naming something that is not there, and recording every one of those
    // would turn this table into the request log it must not become.
    let refused = matches!(status, StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN);
    if requirement.audit == AuditWhen::Always || refused {
        let outcome = Outcome::from_status(status.as_u16());
        state
            .audit
            .record(
                AuditRecord::by(&principal)
                    .action(requirement.action)
                    .request(method.as_str(), &path)
                    .resource(requirement.app.name(), requirement.environment.name())
                    .target(requirement.target)
                    .peer(peer)
                    .outcome(outcome, status.as_u16())
                    .build(),
            )
            .await;
    }

    response
}

/// Why a call could not be admitted. The reason is kept out of the answer to
/// the caller on purpose; it exists for the control plane's own log.
struct Refusal {
    log_reason: &'static str,
    /// What the caller is told. Every unusable *credential* gets
    /// [`UNAUTHENTICATED_DETAIL`] and nothing else, because telling them
    /// apart is a free oracle. The one refusal that says something different
    /// is the caller who presented nothing to a control plane that demands
    /// nothing: there is no credential there to keep anything from, and an
    /// operator whose CLI has just started answering 401 has to be able to
    /// tell "you need a token" from "this port answers only its own machine".
    caller_detail: &'static str,
    /// The public half of whatever was presented, when there was one to name.
    credential: Option<String>,
}

/// Whether a peer is on the machine this control plane runs on.
///
/// A caller that reached a dual-stack socket over IPv4 arrives as
/// `::ffff:127.0.0.1`, and `Ipv6Addr::is_loopback` is false for that address -
/// the loopback it names is the IPv4 one. Unmapping first is what stops a
/// local `deis` from being refused as remote on a control plane bound to
/// `::`.
///
/// `None` - no `ConnectInfo` on the request - is not local. The binary always
/// serves with one (`api::serve`), so an address this cannot see means the
/// router was mounted by something other than the server, and "we cannot tell
/// where this came from" must not be the single case that gets in.
fn is_local_peer(peer: Option<SocketAddr>) -> bool {
    match peer.map(|addr| addr.ip()) {
        Some(std::net::IpAddr::V4(v4)) => v4.is_loopback(),
        Some(std::net::IpAddr::V6(v6)) => match v6.to_ipv4_mapped() {
            Some(v4) => v4.is_loopback(),
            None => v6.is_loopback(),
        },
        None => false,
    }
}

/// The caller nobody authenticated, on a control plane that requires nobody
/// to be - admitted from this machine, refused from anywhere else.
///
/// This is the whole of the "unconfigured is not unguarded" rule for the
/// human API, and it is the one shape that fits. deis-queue answers the same
/// question by refusing to start on a non-loopback bind, and that remedy is
/// not available here: this listener also carries the node gRPC channel, the
/// assignment fetch and the bundle download, so every node in the fleet needs
/// the port reachable and moving it to loopback would leave a one-machine
/// platform. The address therefore stays open and the peer decides. Turning
/// `api_auth_enabled` on by default was the other candidate and is worse: it
/// stops an existing installation's dashboard, CLI and deploy pipeline the
/// moment the binary is replaced. This leaves every one of those working on
/// the machine the control plane runs on, and refuses only the callers who
/// are somewhere else - which is exactly the set that was never meant to be
/// served without a credential.
fn anonymous_from(peer: Option<SocketAddr>) -> Result<Principal, Refusal> {
    if is_local_peer(peer) {
        return Ok(Principal::anonymous());
    }
    Err(Refusal {
        log_reason: "this control plane requires no credential, so it admits an unauthenticated \
                     caller only from its own machine, and this one is not on it",
        caller_detail: UNCONFIGURED_OFF_BOX_DETAIL,
        credential: None,
    })
}

/// Turn what a request carries into the caller it belongs to.
///
/// A configured `api_keys` entry is checked against the configuration file,
/// in constant time, exactly as it was before accounts existed; the database
/// holds no copy of it and cannot admit anyone. Anything shaped like a minted
/// token is looked up by its public id and its secret compared against a
/// stored digest.
async fn resolve(
    state: &AuthzState,
    headers: &HeaderMap,
    peer: Option<SocketAddr>,
) -> Result<Principal, Refusal> {
    let presented: Vec<String> = presented_credentials(headers)
        .into_iter()
        .map(|c| c.to_string())
        .collect();

    if presented.is_empty() {
        if !state.enforced {
            return anonymous_from(peer);
        }
        return Err(Refusal {
            log_reason: "no credential was presented",
            caller_detail: UNAUTHENTICATED_DETAIL,
            credential: None,
        });
    }

    let mut refusal = Refusal {
        log_reason: "the credential presented matches no token this control plane knows",
        caller_detail: UNAUTHENTICATED_DETAIL,
        credential: None,
    };

    for candidate in &presented {
        if state
            .config_keys
            .iter()
            .any(|key| secret_eq(key, candidate))
        {
            let fingerprint = crate::identity::token::fingerprint(candidate);
            match state.identity.resolve_config_key(&fingerprint).await {
                Ok(principal) => return Ok(principal),
                Err(e) => {
                    // The file says this key is good, so the caller gets in
                    // whatever the database says; what is lost is the
                    // attribution, and that is worth a line.
                    warn!(
                        error = %e,
                        credential = %fingerprint,
                        "Could not read the record of a configured api_keys entry; the call is still allowed because the configuration file is the authority for these keys, and the audit line names the key rather than an account"
                    );
                    return Ok(Principal {
                        kind: crate::identity::PrincipalKind::ConfigKey,
                        account: None,
                        role: Some(crate::identity::Role::Owner),
                        credential_id: Some(fingerprint),
                        authority: crate::identity::Authority::Unscoped,
                    });
                }
            }
        }

        match state.identity.resolve_token(candidate).await {
            Ok(Resolution::Allowed(principal)) => return Ok(principal),
            Ok(Resolution::Revoked { token, .. }) => {
                refusal = Refusal {
                    log_reason: "the token presented was revoked, so something out there is still \
                                 configured with a credential that has been withdrawn",
                    caller_detail: UNAUTHENTICATED_DETAIL,
                    credential: Some(token),
                };
            }
            Ok(Resolution::AccountDisabled { token, .. }) => {
                refusal = Refusal {
                    log_reason: "the token presented is valid and its account is switched off",
                    caller_detail: UNAUTHENTICATED_DETAIL,
                    credential: Some(token),
                };
            }
            Ok(Resolution::Unknown) => {}
            Err(e) => {
                warn!(
                    error = %e,
                    "Could not look a credential up; this call is refused, and every authenticated call fails the same way until the database is readable"
                );
            }
        }
    }

    if !state.enforced {
        // Nothing was required, and what was offered did not resolve. The
        // call is treated as the one that offered nothing at all - admitted
        // from this machine, refused from anywhere else - and the audit line
        // says anonymous rather than inventing an account.
        return anonymous_from(peer);
    }
    Err(refusal)
}

/// Read one field out of a JSON body and hand the body on untouched.
///
/// Applied to exactly two routes; see the module header for why, and for why
/// no route carrying a secret is among them.
async fn peek_body_field(
    request: Request,
    field: &str,
) -> Result<(Request, Option<String>), Response> {
    let (parts, body) = request.into_parts();
    let bytes = match axum::body::to_bytes(body, MAX_BODY_PEEK_BYTES).await {
        Ok(bytes) => bytes,
        Err(_) => {
            warn!(
                path = %parts.uri.path(),
                limit = MAX_BODY_PEEK_BYTES,
                "Refused a call because its body is larger than the gate will read; this route creates one named thing and its body is a small JSON object"
            );
            return Err((
                StatusCode::PAYLOAD_TOO_LARGE,
                Json(json!({
                    "error": format!(
                        "This request's body is larger than {MAX_BODY_PEEK_BYTES} bytes, and this \
                         route takes a small JSON object naming what to create."
                    )
                })),
            )
                .into_response());
        }
    };

    // Trimmed, because the handler trims: `catalog::apps::create_app` stores
    // `request.id.trim()` and `catalog::environments::create_environment`
    // stores `request.name.trim()`. Judging the untrimmed spelling would mean
    // checking one name and creating another - and since no scope can spell a
    // name with a space in it (`identity::scope::Component`), a credential
    // that holds exactly `deploy:myapp/*` would be refused its own
    // application over a stray newline, and told to go and ask for
    // `deploy: myapp\n/*`, which is not a scope anyone can be granted.
    let value = serde_json::from_slice::<serde_json::Value>(&bytes)
        .ok()
        .and_then(|body| {
            body.get(field)
                .and_then(|v| v.as_str())
                .map(str::trim)
                .filter(|name| !name.is_empty())
                .map(|s| s.to_string())
        });

    // A body that is not JSON, or names nothing, is not this gate's problem:
    // the handler answers 400 for it - `CatalogStore::create_app` and
    // `create_environment` both refuse an empty name. What matters here is
    // that the resource stays unnamed, which leaves the requirement at "any",
    // and the handler then refuses the request on its own terms.
    Ok((
        Request::from_parts(parts, axum::body::Body::from(bytes)),
        value,
    ))
}

fn unauthenticated(detail: &str) -> Response {
    (
        StatusCode::UNAUTHORIZED,
        Json(json!({
            "error": "Unauthorized",
            "details": detail,
        })),
    )
        .into_response()
}

fn forbidden(required_scope: &str, detail: &str) -> Response {
    (
        StatusCode::FORBIDDEN,
        Json(json!({
            "error": format!("This credential may not do that: {detail}"),
            "details": format!(
                "A token carrying {required_scope} would be allowed. Ask an owner for one with \
                 `deis token create --account <you> --scope {required_scope}`."
            ),
        })),
    )
        .into_response()
}

/// Say, once, what the human API answers to.
///
/// Printed at startup next to the node credential so an operator sees both,
/// and says out loud how many credentials are still unscoped - which is the
/// number the whole model exists to let them drive to zero.
pub fn log_authorization_ready(enforced: bool, config_keys: usize, unscoped: usize) {
    if !enforced {
        warn!(
            unscoped,
            "api_auth_enabled is off, so no caller has to present a credential: anyone on THIS \
             machine can deploy, rotate a secret and mint a storage token as \"anonymous\", and \
             every change of theirs is in the audit log under that name. A caller from any other \
             host is refused with 401 rather than served, because a port bound off loopback with \
             nothing required would otherwise be the whole platform published to the network. A \
             credential that IS presented is held to its scopes either way. Set \
             api_auth_enabled = true to require one from everybody"
        );
        return;
    }
    if unscoped > 0 {
        warn!(
            unscoped,
            config_keys,
            "The human API is authenticated, and that many live credentials are UNSCOPED: each one \
             may deploy any application, rotate any secret and mint any tenant's storage token. \
             `deis token list` names them; replace them with scoped tokens and revoke these"
        );
    } else {
        info!(
            config_keys,
            "The human API is authenticated and every live credential is scoped; every change and \
             every refusal is in the audit log"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn requirement(method: &str, path: &str) -> Requirement {
        let (path, query) = match path.split_once('?') {
            Some((path, query)) => (path, Some(query)),
            None => (path, None),
        };
        classify(&Method::from_bytes(method.as_bytes()).unwrap(), path, query)
    }

    /// The route table, spelled out. Every route the human router mounts has
    /// a line here; a route that is missing would fall to the closing arm and
    /// be visible as `unclassified`.
    #[test]
    fn every_mounted_route_has_a_requirement_of_its_own() {
        let routes = [
            ("GET", "/api/apps", "app.list", Verb::Read),
            ("POST", "/api/apps", "app.create", Verb::Deploy),
            ("GET", "/api/apps/demo", "app.get", Verb::Read),
            ("DELETE", "/api/apps/demo", "app.delete", Verb::Admin),
            ("GET", "/api/retired-apps", "retired_app.list", Verb::Admin),
            (
                "DELETE",
                "/api/retired-apps/demo",
                "retired_app.release",
                Verb::Admin,
            ),
            ("GET", "/api/apps/demo/versions", "version.list", Verb::Read),
            (
                "POST",
                "/api/apps/demo/versions",
                "version.upload",
                Verb::Deploy,
            ),
            (
                "GET",
                "/api/apps/demo/versions/v1",
                "version.get",
                Verb::Read,
            ),
            (
                "GET",
                "/api/apps/demo/environments",
                "environment.list",
                Verb::Read,
            ),
            (
                "POST",
                "/api/apps/demo/environments",
                "environment.create",
                Verb::Deploy,
            ),
            (
                "GET",
                "/api/apps/demo/environments/production",
                "environment.get",
                Verb::Read,
            ),
            (
                "GET",
                "/api/apps/demo/environments/production/deployment",
                "deployment.get",
                Verb::Read,
            ),
            (
                "GET",
                "/api/apps/demo/environments/production/deployments",
                "deployment.list",
                Verb::Read,
            ),
            (
                "POST",
                "/api/apps/demo/environments/production/deployments",
                "deploy",
                Verb::Deploy,
            ),
            (
                "POST",
                "/api/apps/demo/environments/production/rollback",
                "rollback",
                Verb::Deploy,
            ),
            (
                "GET",
                "/api/apps/demo/environments/production/routes",
                "route.list",
                Verb::Read,
            ),
            (
                "POST",
                "/api/apps/demo/environments/production/routes",
                "route.create",
                Verb::Deploy,
            ),
            (
                "PUT",
                "/api/apps/demo/environments/production/routes/r1",
                "route.update",
                Verb::Deploy,
            ),
            (
                "DELETE",
                "/api/apps/demo/environments/production/routes/r1",
                "route.delete",
                Verb::Deploy,
            ),
            (
                "GET",
                "/api/apps/demo/environments/production/secrets",
                "secret.list",
                Verb::Read,
            ),
            (
                "PUT",
                "/api/apps/demo/environments/production/secrets/TOKEN",
                "secret.write",
                Verb::Secrets,
            ),
            (
                "DELETE",
                "/api/apps/demo/environments/production/secrets/TOKEN",
                "secret.delete",
                Verb::Secrets,
            ),
            (
                "GET",
                "/api/apps/demo/environments/production/storage-token",
                "storage_token.mint",
                Verb::Storage,
            ),
            ("GET", "/api/routes", "route.list_all", Verb::Read),
            ("GET", "/api/sessions", "session.read", Verb::Read),
            (
                "GET",
                "/api/sessions/abc/heartbeats",
                "session.read",
                Verb::Read,
            ),
            ("GET", "/api/configs", "node.config.read", Verb::Read),
            (
                "POST",
                "/api/configs/node-1",
                "node.config.write",
                Verb::Admin,
            ),
            (
                "DELETE",
                "/api/configs/node-1/key",
                "node.config.write",
                Verb::Admin,
            ),
            (
                "POST",
                "/api/configs/push/node-1",
                "node.config.write",
                Verb::Admin,
            ),
            ("GET", "/api/commands", "node.command.read", Verb::Read),
            (
                "POST",
                "/api/commands/execute/node-1",
                "node.command",
                Verb::Admin,
            ),
            (
                "POST",
                "/api/commands/cancel/c1",
                "node.command",
                Verb::Admin,
            ),
            ("GET", "/api/events", "event.read", Verb::Read),
            ("GET", "/api/events/stats", "event.read", Verb::Read),
            ("GET", "/api/metrics/summary", "metrics.read", Verb::Read),
            ("GET", "/api/runtime", "node.list", Verb::Read),
            (
                "GET",
                "/api/runtime/node-1/status",
                "node.status",
                Verb::Read,
            ),
            (
                "DELETE",
                "/api/runtime/node-1/apps/demo",
                "node.app.unload",
                Verb::Deploy,
            ),
            ("GET", "/api/accounts", "account.list", Verb::Admin),
            ("POST", "/api/accounts", "account.create", Verb::Admin),
            ("GET", "/api/accounts/alice", "account.get", Verb::Admin),
            (
                "PATCH",
                "/api/accounts/alice",
                "account.update",
                Verb::Admin,
            ),
            (
                "POST",
                "/api/accounts/alice/tokens",
                "token.mint",
                Verb::Admin,
            ),
            ("GET", "/api/tokens", "token.list", Verb::Admin),
            ("DELETE", "/api/tokens/abcd", "token.revoke", Verb::Admin),
            ("GET", "/api/audit", "audit.read", Verb::Admin),
        ];
        for (method, path, action, verb) in routes {
            let requirement = requirement(method, path);
            assert_eq!(requirement.action, action, "{method} {path}");
            assert_eq!(requirement.verb, verb, "{method} {path}");
        }
    }

    /// A route nobody wrote a line for must be an administrator's, not
    /// everybody's. This is the arm that keeps a forgotten route from being
    /// an open one.
    #[test]
    fn an_unknown_route_is_an_administrators() {
        let requirement = requirement("POST", "/api/something-new");
        assert_eq!(requirement.action, "unclassified");
        assert_eq!(requirement.verb, Verb::Admin);
        assert_eq!(requirement.app, Resource::Every);
        assert_eq!(requirement.audit, AuditWhen::Always);
    }

    /// Every call that changes something is recorded whatever happens to it;
    /// a plain read is recorded only when it was refused. The storage-token
    /// mint is the one read that is always recorded, because what it returns
    /// is a credential.
    #[test]
    fn state_changes_and_credential_issues_are_always_recorded() {
        for (method, path) in [
            ("POST", "/api/apps/demo/environments/production/deployments"),
            ("POST", "/api/apps/demo/environments/production/rollback"),
            (
                "PUT",
                "/api/apps/demo/environments/production/secrets/TOKEN",
            ),
            (
                "DELETE",
                "/api/apps/demo/environments/production/secrets/TOKEN",
            ),
            ("POST", "/api/apps/demo/versions"),
            ("POST", "/api/apps"),
            ("POST", "/api/accounts"),
            ("POST", "/api/accounts/alice/tokens"),
            ("DELETE", "/api/tokens/abcd"),
            ("POST", "/api/apps/demo/environments/production/routes"),
            (
                "GET",
                "/api/apps/demo/environments/production/storage-token",
            ),
        ] {
            assert_eq!(
                requirement(method, path).audit,
                AuditWhen::Always,
                "{method} {path} must always be recorded"
            );
        }
        for (method, path) in [
            ("GET", "/api/apps/demo/versions"),
            ("GET", "/api/apps/demo/environments/production/secrets"),
            ("GET", "/api/audit"),
        ] {
            assert_eq!(requirement(method, path).audit, AuditWhen::OnRefusal);
        }
    }

    /// A secret write records the secret's name and never touches the body,
    /// which is what keeps a value out of the log by construction.
    #[test]
    fn a_secret_write_names_the_secret_and_reads_no_body() {
        let write = requirement(
            "PUT",
            "/api/apps/demo/environments/production/secrets/DB_URL",
        );
        assert_eq!(write.target.as_deref(), Some("DB_URL"));
        assert!(
            write.from_body.is_none(),
            "the gate must never read the body of a route that carries a value"
        );
        // And the only two routes it does read carry a name, not a value.
        assert_eq!(
            requirement("POST", "/api/apps").from_body,
            Some((BodyComponent::App, "id"))
        );
        assert_eq!(
            requirement("POST", "/api/apps/demo/environments").from_body,
            Some((BodyComponent::Environment, "name"))
        );
    }

    /// The CI credential from the specification, walked through the exact
    /// sequence `deis deploy -e preview` makes, and then through everything
    /// it must not be able to do.
    #[test]
    fn a_token_scoped_to_one_environment_can_deploy_there_and_nothing_else() {
        let ci = Principal {
            kind: crate::identity::PrincipalKind::Account,
            account: Some("ci".to_string()),
            role: Some(crate::identity::Role::Deployer),
            credential_id: Some("0011223344556677".to_string()),
            authority: crate::identity::Authority::Scoped(
                crate::identity::Scope::parse_list("deploy:myapp/preview").unwrap(),
            ),
        };

        let allowed = [
            ("GET", "/api/apps/myapp"),
            ("POST", "/api/apps/myapp/versions"),
            ("GET", "/api/apps/myapp/environments/preview"),
            ("POST", "/api/apps/myapp/environments/preview/deployments"),
            ("POST", "/api/apps/myapp/environments/preview/rollback"),
            ("GET", "/api/apps/myapp/versions"),
            ("GET", "/api/runtime"),
        ];
        for (method, path) in allowed {
            let requirement = requirement(method, path);
            let demand = Demand {
                verb: requirement.verb,
                app: requirement.app.selector(),
                environment: requirement.environment.selector(),
            };
            assert!(
                ci.permits(&demand).is_ok(),
                "{method} {path} needs {demand} and was refused"
            );
        }

        let refused = [
            (
                "POST",
                "/api/apps/myapp/environments/production/deployments",
            ),
            ("GET", "/api/apps/myapp/environments/production/secrets"),
            ("PUT", "/api/apps/myapp/environments/preview/secrets/TOKEN"),
            ("GET", "/api/apps/myapp/environments/preview/storage-token"),
            ("GET", "/api/apps/otherapp"),
            ("GET", "/api/apps"),
            ("GET", "/api/audit"),
            ("GET", "/api/tokens"),
            ("POST", "/api/accounts"),
            ("GET", "/api/routes"),
            ("POST", "/api/commands/execute/node-1"),
            ("DELETE", "/api/apps/myapp"),
        ];
        for (method, path) in refused {
            let requirement = requirement(method, path);
            let demand = Demand {
                verb: requirement.verb,
                app: requirement.app.selector(),
                environment: requirement.environment.selector(),
            };
            assert!(
                ci.permits(&demand).is_err(),
                "{method} {path} needs only {demand} and was allowed"
            );
        }
    }

    /// Unloading an application from a node stops every environment of it
    /// unless the query names one, so that is what the table has to demand.
    ///
    /// The defect: this line said `Resource::Any` for the environment, which
    /// *any single* environment scope answers. A pipeline holding
    /// `deploy:myapp/preview` - correctly refused a deploy to production -
    /// could take production off a node, and take it off again after every
    /// reconcile put it back.
    #[test]
    fn unloading_an_application_from_a_node_is_about_every_environment_of_it() {
        let whole = requirement("DELETE", "/api/runtime/node-1/apps/myapp");
        assert_eq!(whole.app, Resource::Named("myapp".to_string()));
        assert_eq!(
            whole.environment,
            Resource::Every,
            "with nothing named the node takes off every environment of the application"
        );

        let one = requirement(
            "DELETE",
            "/api/runtime/node-1/apps/myapp?environment=production",
        );
        assert_eq!(one.environment, Resource::Named("production".to_string()));

        // Anything this cannot read as exactly one environment falls to the
        // wider demand rather than the narrower one.
        for query in [
            "environment=",
            "environment=pro%64uction",
            "environment=preview&environment=production",
            "environment=Production",
            "other=production",
        ] {
            assert_eq!(
                requirement("DELETE", &format!("/api/runtime/n/apps/myapp?{query}")).environment,
                Resource::Every,
                "?{query} must not narrow what this call is judged against"
            );
        }
        // The same value twice is still one environment.
        assert_eq!(
            requirement(
                "DELETE",
                "/api/runtime/n/apps/myapp?environment=preview&environment=preview"
            )
            .environment,
            Resource::Named("preview".to_string())
        );

        // And the pipeline credential, against the table, in both spellings.
        let ci = Principal {
            kind: crate::identity::PrincipalKind::Account,
            account: Some("ci".to_string()),
            role: Some(crate::identity::Role::Deployer),
            credential_id: Some("0011223344556677".to_string()),
            authority: crate::identity::Authority::Scoped(
                crate::identity::Scope::parse_list("deploy:myapp/preview").unwrap(),
            ),
        };
        fn demand(requirement: &Requirement) -> Demand<'_> {
            Demand {
                verb: requirement.verb,
                app: requirement.app.selector(),
                environment: requirement.environment.selector(),
            }
        }
        assert!(
            ci.permits(&demand(&whole)).is_err(),
            "one environment's credential must not stop the whole application"
        );
        assert!(ci.permits(&demand(&one)).is_err());
        assert!(
            ci.permits(&demand(&requirement(
                "DELETE",
                "/api/runtime/node-1/apps/myapp?environment=preview"
            )))
            .is_ok(),
            "the environment it does hold stays reachable, or the route is unusable to a \
             scoped credential"
        );
    }

    /// A peer this cannot place is not on this machine. The mapped-IPv4 case
    /// is the one that bites: a local `deis` reaching a control plane bound
    /// to `::` arrives as ::ffff:127.0.0.1, whose `Ipv6Addr::is_loopback` is
    /// false.
    #[test]
    fn only_an_address_on_this_machine_counts_as_local() {
        for local in [
            "127.0.0.1:1",
            "127.0.0.53:1",
            "[::1]:1",
            "[::ffff:127.0.0.1]:1",
        ] {
            assert!(
                is_local_peer(Some(local.parse().unwrap())),
                "{local} is on this machine"
            );
        }
        for remote in [
            "10.0.0.7:1",
            "192.168.1.4:1",
            "[2001:db8::1]:1",
            "[::ffff:10.0.0.7]:1",
        ] {
            assert!(
                !is_local_peer(Some(remote.parse().unwrap())),
                "{remote} is not on this machine"
            );
        }
        assert!(
            !is_local_peer(None),
            "an address this cannot see must not be the one case that gets in"
        );
    }

    /// The storage-token route asks for a verb nothing else implies, over
    /// this exact application and environment.
    #[test]
    fn the_storage_token_route_demands_that_exact_tenant() {
        let requirement = requirement(
            "GET",
            "/api/apps/demo/environments/production/storage-token",
        );
        assert_eq!(requirement.verb, Verb::Storage);
        assert_eq!(requirement.app, Resource::Named("demo".to_string()));
        assert_eq!(
            requirement.environment,
            Resource::Named("production".to_string())
        );

        let demand = Demand {
            verb: requirement.verb,
            app: requirement.app.selector(),
            environment: requirement.environment.selector(),
        };
        // A maintainer of everything, without that tenant written down, is
        // refused; the one who has it written down is not.
        let with = |scopes: &str| Principal {
            kind: crate::identity::PrincipalKind::Account,
            account: Some("someone".to_string()),
            role: Some(crate::identity::Role::Maintainer),
            credential_id: Some("0011223344556677".to_string()),
            authority: crate::identity::Authority::Scoped(
                crate::identity::Scope::parse_list(scopes).unwrap(),
            ),
        };
        assert!(
            with("deploy:*/* secrets:*/* read:*/*")
                .permits(&demand)
                .is_err()
        );
        assert!(with("storage:demo/production").permits(&demand).is_ok());
        assert!(with("storage:demo/preview").permits(&demand).is_err());
    }
}

#[cfg(test)]
mod gate_tests {
    use super::*;
    use crate::database::Database;
    use crate::identity::{Authority, Role, Scope};

    use axum::body::Body;
    use axum::http::Request as HttpRequest;
    use axum::routing::{delete, get, post, put};
    use axum::{Json, Router};
    use tower::ServiceExt;

    /// A router shaped like the human API: three real routes behind the real
    /// gate, over a real database. Everything below asserts on what a caller
    /// gets and on what the audit log then holds, which is the only pair that
    /// matters.
    struct Gate {
        router: Router,
        state: AuthzState,
    }

    async fn gate(enforced: bool, config_keys: Vec<String>) -> Gate {
        let db = Arc::new(Database::init(":memory:").await.unwrap());
        let identity = Arc::new(IdentityStore::new(Arc::clone(&db)));
        let audit = Arc::new(AuditStore::new(Arc::clone(&db)));
        identity.sync_config_keys(&config_keys).await.unwrap();

        let state = AuthzState {
            enforced,
            config_keys: Arc::new(config_keys),
            identity,
            audit,
        };

        async fn ok() -> &'static str {
            "OK"
        }
        async fn created(Json(_body): Json<serde_json::Value>) -> &'static str {
            "OK"
        }

        let router = Router::new()
            .route("/health", get(ok))
            .route("/api/apps", post(created))
            .route("/api/apps/:app/versions", post(ok))
            .route(
                "/api/apps/:app/environments/:env/deployments",
                post(ok).get(ok),
            )
            .route(
                "/api/apps/:app/environments/:env/secrets/:name",
                put(created),
            )
            .route("/api/apps/:app/environments/:env/storage-token", get(ok))
            .route("/api/runtime/:node/apps/:app", delete(ok))
            .route("/api/audit", get(ok))
            .layer(axum::middleware::from_fn_with_state(
                state.clone(),
                authorize,
            ));

        Gate { router, state }
    }

    /// The peer every call below comes from unless it says otherwise: the
    /// machine the control plane runs on, which is what `deis` on the box and
    /// the dashboard in a browser on the box are. The real server always puts
    /// an address on the request (`api::serve` uses
    /// `into_make_service_with_connect_info`), so a test that left it off
    /// would be asserting about a request shape that never reaches this gate.
    const ON_THE_BOX: &str = "127.0.0.1:54321";
    /// Any other machine that can route to the port.
    const OFF_THE_BOX: &str = "10.0.0.7:54321";

    async fn call(gate: &Gate, method: &str, path: &str, credential: Option<&str>) -> StatusCode {
        call_with_body(gate, method, path, credential, "{}").await.0
    }

    async fn call_with_body(
        gate: &Gate,
        method: &str,
        path: &str,
        credential: Option<&str>,
        body: &str,
    ) -> (StatusCode, String) {
        call_from(gate, ON_THE_BOX, method, path, credential, body).await
    }

    async fn call_from(
        gate: &Gate,
        peer: &str,
        method: &str,
        path: &str,
        credential: Option<&str>,
        body: &str,
    ) -> (StatusCode, String) {
        let mut builder = HttpRequest::builder()
            .method(method)
            .uri(path)
            .header("content-type", "application/json");
        if let Some(credential) = credential {
            builder = builder.header("Authorization", format!("Bearer {credential}"));
        }
        let mut request = builder.body(Body::from(body.to_string())).unwrap();
        request.extensions_mut().insert(ConnectInfo(
            peer.parse::<SocketAddr>().expect("a peer address"),
        ));
        let response = gate.router.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), 64 * 1024)
            .await
            .unwrap();
        (status, String::from_utf8_lossy(&bytes).to_string())
    }

    async fn lines(gate: &Gate) -> Vec<crate::audit::AuditEntry> {
        gate.state
            .audit
            .query(crate::audit::AuditQuery::default())
            .await
            .unwrap()
    }

    /// The migration promise: a control plane upgraded with an `api_keys`
    /// entry in its file authenticates exactly as it did, and that key can
    /// still do everything - including mint a storage token.
    #[tokio::test]
    async fn an_existing_api_key_keeps_working_and_is_still_root() {
        let gate = gate(true, vec!["an-existing-key".to_string()]).await;

        for (method, path) in [
            ("POST", "/api/apps/demo/versions"),
            ("POST", "/api/apps/demo/environments/production/deployments"),
            (
                "PUT",
                "/api/apps/demo/environments/production/secrets/TOKEN",
            ),
            (
                "GET",
                "/api/apps/demo/environments/production/storage-token",
            ),
            ("GET", "/api/audit"),
        ] {
            assert_eq!(
                call(&gate, method, path, Some("an-existing-key")).await,
                StatusCode::OK,
                "{method} {path} was refused a key that used to be able to do everything"
            );
        }

        // And it is visible as unscoped, which is the thing an operator has
        // to be able to see in order to retire it.
        let listed = gate.state.identity.list_tokens(None, false).await.unwrap();
        assert_eq!(listed.len(), 1);
        assert!(listed[0].unscoped);
        assert_eq!(gate.state.identity.unscoped_count().await.unwrap(), 1);
    }

    /// The headline case. A CI credential that may deploy to one environment
    /// walks the whole `deis deploy -e preview` sequence, and is refused
    /// everything else - including the storage token for the very environment
    /// it deploys to.
    #[tokio::test]
    async fn a_ci_token_deploys_to_one_environment_and_nothing_else() {
        let gate = gate(true, vec![]).await;
        gate.state
            .identity
            .create_account("ci", Role::Deployer)
            .await
            .unwrap();
        let (_, secret) = gate
            .state
            .identity
            .create_token(
                "ci",
                "preview-pipeline",
                Authority::Scoped(Scope::parse_list("deploy:myapp/preview").unwrap()),
            )
            .await
            .unwrap();

        assert_eq!(
            call_with_body(
                &gate,
                "POST",
                "/api/apps",
                Some(&secret),
                r#"{"id":"myapp"}"#
            )
            .await
            .0,
            StatusCode::OK,
            "the application this credential is scoped to must be creatable by it"
        );
        assert_eq!(
            call(&gate, "POST", "/api/apps/myapp/versions", Some(&secret)).await,
            StatusCode::OK
        );
        assert_eq!(
            call(
                &gate,
                "POST",
                "/api/apps/myapp/environments/preview/deployments",
                Some(&secret)
            )
            .await,
            StatusCode::OK
        );

        for (method, path) in [
            (
                "POST",
                "/api/apps/myapp/environments/production/deployments",
            ),
            ("PUT", "/api/apps/myapp/environments/preview/secrets/TOKEN"),
            ("GET", "/api/apps/myapp/environments/preview/storage-token"),
            ("POST", "/api/apps/other/versions"),
            ("GET", "/api/audit"),
        ] {
            assert_eq!(
                call(&gate, method, path, Some(&secret)).await,
                StatusCode::FORBIDDEN,
                "{method} {path} was allowed to a token scoped to deploy:myapp/preview"
            );
        }

        // A different application in the body of the create route is refused
        // too, or the body peek would be decoration.
        assert_eq!(
            call_with_body(
                &gate,
                "POST",
                "/api/apps",
                Some(&secret),
                r#"{"id":"someone-elses"}"#
            )
            .await
            .0,
            StatusCode::FORBIDDEN
        );
    }

    /// A refusal has to name the scope that would have worked, because the
    /// person reading it has a terminal and no access to this control plane's
    /// log.
    #[tokio::test]
    async fn a_refusal_tells_the_caller_which_scope_would_have_worked() {
        let gate = gate(true, vec![]).await;
        gate.state
            .identity
            .create_account("ci", Role::Deployer)
            .await
            .unwrap();
        let (_, secret) = gate
            .state
            .identity
            .create_token(
                "ci",
                "pipeline",
                Authority::Scoped(Scope::parse_list("deploy:myapp/preview").unwrap()),
            )
            .await
            .unwrap();

        let (status, body) = call_with_body(
            &gate,
            "POST",
            "/api/apps/myapp/environments/production/deployments",
            Some(&secret),
            "{}",
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert!(body.contains("deploy:myapp/production"), "{body}");
        assert!(body.contains("deis token create"), "{body}");
    }

    /// A revoked credential stops working at its next call, and the caller
    /// cannot tell that from a credential that never existed - otherwise
    /// whoever holds a stolen token learns whether it was noticed.
    #[tokio::test]
    async fn a_revoked_token_is_refused_and_looks_exactly_like_an_unknown_one() {
        let gate = gate(true, vec![]).await;
        gate.state
            .identity
            .create_account("alice", Role::Owner)
            .await
            .unwrap();
        let (token, secret) = gate
            .state
            .identity
            .create_token("alice", "laptop", Authority::Unscoped)
            .await
            .unwrap();

        assert_eq!(
            call(&gate, "POST", "/api/apps/demo/versions", Some(&secret)).await,
            StatusCode::OK
        );
        gate.state.identity.revoke_token(&token.id).await.unwrap();

        let revoked = call_with_body(
            &gate,
            "POST",
            "/api/apps/demo/versions",
            Some(&secret),
            "{}",
        )
        .await;
        let never_existed = call_with_body(
            &gate,
            "POST",
            "/api/apps/demo/versions",
            Some("deis_pat_00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff0011"),
            "{}",
        )
        .await;
        assert_eq!(revoked.0, StatusCode::UNAUTHORIZED);
        assert_eq!(revoked, never_existed, "the two must be indistinguishable");

        // The control plane's own record does tell them apart, which is the
        // point: one of them means a credential is still deployed somewhere.
        let refusals = lines(&gate).await;
        let detail: Vec<&str> = refusals
            .iter()
            .filter_map(|line| line.detail.as_deref())
            .collect();
        assert!(
            detail.iter().any(|d| d.contains("revoked")),
            "the log has to say a revoked token was presented: {detail:?}"
        );

        // And it names which token, because that id is how an operator finds
        // whatever is still configured with a credential they withdrew.
        let named = refusals
            .iter()
            .find(|line| {
                line.detail
                    .as_deref()
                    .is_some_and(|d| d.contains("revoked"))
            })
            .and_then(|line| line.actor_credential.clone());
        assert_eq!(named.as_deref(), Some(token.id.as_str()));
    }

    /// Disabling an account stops its tokens at once, without revoking them.
    #[tokio::test]
    async fn a_disabled_account_stops_working_immediately() {
        let gate = gate(true, vec![]).await;
        gate.state
            .identity
            .create_account("bob", Role::Owner)
            .await
            .unwrap();
        let (_, secret) = gate
            .state
            .identity
            .create_token("bob", "laptop", Authority::Unscoped)
            .await
            .unwrap();
        assert_eq!(
            call(&gate, "POST", "/api/apps/demo/versions", Some(&secret)).await,
            StatusCode::OK
        );

        gate.state
            .identity
            .update_account("bob", None, Some(true))
            .await
            .unwrap();
        assert_eq!(
            call(&gate, "POST", "/api/apps/demo/versions", Some(&secret)).await,
            StatusCode::UNAUTHORIZED
        );
    }

    /// Demoting an account narrows every token it already holds, without any
    /// of them being reissued.
    #[tokio::test]
    async fn demoting_an_account_narrows_the_tokens_it_already_issued() {
        let gate = gate(true, vec![]).await;
        gate.state
            .identity
            .create_account("carol", Role::Maintainer)
            .await
            .unwrap();
        let (_, secret) = gate
            .state
            .identity
            .create_token(
                "carol",
                "laptop",
                Authority::Scoped(Scope::parse_list("secrets:demo/production").unwrap()),
            )
            .await
            .unwrap();
        let secret_route = "/api/apps/demo/environments/production/secrets/TOKEN";
        assert_eq!(
            call(&gate, "PUT", secret_route, Some(&secret)).await,
            StatusCode::OK
        );

        gate.state
            .identity
            .update_account("carol", Some(Role::Viewer), None)
            .await
            .unwrap();
        assert_eq!(
            call(&gate, "PUT", secret_route, Some(&secret)).await,
            StatusCode::FORBIDDEN,
            "a viewer must not be able to write a secret, whatever their token says"
        );
    }

    /// What the audit log holds after a change, a refusal and a read: the
    /// first two, and not the third.
    #[tokio::test]
    async fn the_log_holds_changes_and_refusals_and_not_successful_reads() {
        let gate = gate(true, vec!["an-existing-key".to_string()]).await;

        call_with_body(
            &gate,
            "PUT",
            "/api/apps/demo/environments/production/secrets/DB_PASSWORD",
            Some("an-existing-key"),
            r#"{"value":"hunter2-do-not-log-me"}"#,
        )
        .await;
        call(
            &gate,
            "GET",
            "/api/apps/demo/environments/production/deployments",
            Some("an-existing-key"),
        )
        .await;
        call(&gate, "POST", "/api/apps/demo/versions", None).await;

        let lines = lines(&gate).await;
        let actions: Vec<&str> = lines.iter().map(|line| line.action.as_str()).collect();
        assert!(actions.contains(&"secret.write"), "{actions:?}");
        assert!(actions.contains(&"version.upload"), "{actions:?}");
        assert!(
            !actions.contains(&"deployment.list"),
            "a successful read is not an event: {actions:?}"
        );

        let write = lines
            .iter()
            .find(|line| line.action == "secret.write")
            .unwrap();
        assert_eq!(write.app_id.as_deref(), Some("demo"));
        assert_eq!(write.environment.as_deref(), Some("production"));
        assert_eq!(write.target.as_deref(), Some("DB_PASSWORD"));
        assert_eq!(write.outcome, Outcome::Accepted);
        assert_eq!(write.actor_kind, "config_key");

        // The value is never in the log, whatever else is.
        let dump = serde_json::to_string(&lines).unwrap();
        assert!(dump.contains("DB_PASSWORD"), "the name has to be there");
        assert!(
            !dump.contains("hunter2"),
            "a secret value reached the audit log"
        );

        let refusal = lines
            .iter()
            .find(|line| line.outcome == Outcome::Refused)
            .expect("an anonymous call has to be recorded");
        assert_eq!(refusal.action, "version.upload");
        assert_eq!(refusal.status, 401);
        assert_eq!(refusal.actor_kind, "anonymous");
    }

    /// With `api_auth_enabled` off, the human API is as open as it always
    /// was - and the audit log still records every change, so an operator who
    /// has not turned authentication on still gets a history.
    #[tokio::test]
    async fn an_unauthenticated_control_plane_is_still_audited() {
        let gate = gate(false, vec![]).await;

        assert_eq!(
            call(
                &gate,
                "POST",
                "/api/apps/demo/environments/production/deployments",
                None
            )
            .await,
            StatusCode::OK,
            "turning accounts on must not brick a platform that never demanded a credential"
        );
        assert_eq!(call(&gate, "GET", "/api/audit", None).await, StatusCode::OK);

        let lines = lines(&gate).await;
        let deploy = lines.iter().find(|line| line.action == "deploy").unwrap();
        assert_eq!(deploy.actor_kind, "anonymous");
        assert_eq!(deploy.actor_account, None);
        assert_eq!(deploy.app_id.as_deref(), Some("demo"));
    }

    /// The probe and the metrics scrape stay open, as they were: neither has
    /// a caller that could present anything.
    #[tokio::test]
    async fn the_health_probe_is_not_behind_the_gate() {
        let gate = gate(true, vec![]).await;
        assert_eq!(call(&gate, "GET", "/health", None).await, StatusCode::OK);
        assert!(lines(&gate).await.is_empty());
    }

    /// Unloading an application from a node is a real outage: the node stops
    /// serving it, and it stays stopped until the next reconcile - which the
    /// same call can undo again. A credential that may deploy to one preview
    /// environment must not be able to make it.
    #[tokio::test]
    async fn a_credential_for_one_environment_cannot_unload_the_application_from_a_node() {
        let gate = gate(true, vec![]).await;
        gate.state
            .identity
            .create_account("ci", Role::Deployer)
            .await
            .unwrap();
        let (_, secret) = gate
            .state
            .identity
            .create_token(
                "ci",
                "preview-pipeline",
                Authority::Scoped(Scope::parse_list("deploy:myapp/preview").unwrap()),
            )
            .await
            .unwrap();

        // The whole application, and one environment of it that this
        // credential has no business touching.
        for path in [
            "/api/runtime/node-1/apps/myapp",
            "/api/runtime/node-1/apps/myapp?environment=production",
            "/api/runtime/node-1/apps/otherapp",
        ] {
            assert_eq!(
                call(&gate, "DELETE", path, Some(&secret)).await,
                StatusCode::FORBIDDEN,
                "DELETE {path} was allowed to a token scoped to deploy:myapp/preview"
            );
        }

        // And the environment it does hold is still reachable, or the route
        // would be unusable to every scoped credential there is.
        assert_eq!(
            call(
                &gate,
                "DELETE",
                "/api/runtime/node-1/apps/myapp?environment=preview",
                Some(&secret)
            )
            .await,
            StatusCode::OK
        );

        // The refusal names the scope that would have worked, and it is the
        // wildcard rather than one environment.
        let (_, body) = call_with_body(
            &gate,
            "DELETE",
            "/api/runtime/node-1/apps/myapp",
            Some(&secret),
            "{}",
        )
        .await;
        assert!(body.contains("deploy:myapp/*"), "{body}");

        // Every one of those is on the record: an unload changes state, so
        // it is audited whatever its outcome.
        let refused: Vec<_> = lines(&gate)
            .await
            .into_iter()
            .filter(|line| line.action == "node.app.unload" && line.outcome == Outcome::Refused)
            .collect();
        assert_eq!(refused.len(), 4, "{refused:?}");
    }

    /// `api_auth_enabled = false` means no credential is required. It does
    /// not mean the human API is published: an operator who never turned it
    /// on used to be handing every application, the audit log, the account
    /// list and the storage-token mint to anything that could route to the
    /// port.
    #[tokio::test]
    async fn a_control_plane_that_requires_no_credential_answers_only_its_own_machine() {
        let gate = gate(false, vec![]).await;

        // On the box: exactly as open as it always was. This is the demo,
        // the single-host install and the operator who is mid-upgrade with a
        // dashboard on localhost.
        assert_eq!(
            call(
                &gate,
                "POST",
                "/api/apps/demo/environments/production/deployments",
                None
            )
            .await,
            StatusCode::OK
        );

        // From anywhere else: nothing at all, including the two calls that
        // hurt most - the audit log, which names every application and
        // everybody who touched one, and the storage-token mint, which hands
        // back a credential that never expires.
        for (method, path) in [
            ("POST", "/api/apps/demo/environments/production/deployments"),
            ("GET", "/api/audit"),
            (
                "GET",
                "/api/apps/demo/environments/production/storage-token",
            ),
        ] {
            let (status, body) = call_from(&gate, OFF_THE_BOX, method, path, None, "{}").await;
            assert_eq!(
                status,
                StatusCode::UNAUTHORIZED,
                "{method} {path} was served to a caller off the box"
            );
            assert!(
                body.contains("api_auth_enabled"),
                "the answer has to name the one setting that fixes this: {body}"
            );
        }

        // And the refusals are in the log, with the address, which is how an
        // operator sees that something off the box was reaching for this.
        let refusals: Vec<_> = lines(&gate)
            .await
            .into_iter()
            .filter(|line| line.outcome == Outcome::Refused)
            .collect();
        assert_eq!(refusals.len(), 3, "{refusals:?}");
        assert!(
            refusals
                .iter()
                .all(|line| line.peer.as_deref() == Some(OFF_THE_BOX)),
            "{refusals:?}"
        );
    }

    /// The other half of the same flag. Not requiring a credential cannot
    /// mean that one which is presented goes unchecked: the scopes on it were
    /// written down as its limit, and a control plane that ignores them hands
    /// a preview pipeline the storage credential of any tenant it names.
    #[tokio::test]
    async fn a_credential_that_is_presented_is_held_to_its_scopes_even_when_none_is_required() {
        let gate = gate(false, vec![]).await;
        gate.state
            .identity
            .create_account("ci", Role::Deployer)
            .await
            .unwrap();
        let (_, secret) = gate
            .state
            .identity
            .create_token(
                "ci",
                "preview-pipeline",
                Authority::Scoped(Scope::parse_list("deploy:myapp/preview").unwrap()),
            )
            .await
            .unwrap();

        assert_eq!(
            call(
                &gate,
                "POST",
                "/api/apps/myapp/environments/preview/deployments",
                Some(&secret)
            )
            .await,
            StatusCode::OK,
            "what the token says it may do, it may still do"
        );
        for (method, path) in [
            (
                "POST",
                "/api/apps/myapp/environments/production/deployments",
            ),
            (
                "GET",
                "/api/apps/other/environments/production/storage-token",
            ),
            ("GET", "/api/audit"),
        ] {
            assert_eq!(
                call(&gate, method, path, Some(&secret)).await,
                StatusCode::FORBIDDEN,
                "{method} {path} was allowed to deploy:myapp/preview because nothing was enforced"
            );
        }
    }

    /// The gate reads the name a create route puts in its body, and the
    /// handler trims that name before it stores it. Reading the two
    /// differently means judging one application and creating another - and
    /// since no scope can contain a space, the credential that owns the
    /// application is the one that gets refused.
    #[tokio::test]
    async fn a_padded_name_in_the_body_is_judged_as_the_name_that_gets_created() {
        let gate = gate(true, vec![]).await;
        gate.state
            .identity
            .create_account("ci", Role::Deployer)
            .await
            .unwrap();
        let (_, secret) = gate
            .state
            .identity
            .create_token(
                "ci",
                "pipeline",
                Authority::Scoped(Scope::parse_list("deploy:myapp/preview").unwrap()),
            )
            .await
            .unwrap();

        assert_eq!(
            call_with_body(
                &gate,
                "POST",
                "/api/apps",
                Some(&secret),
                "{\"id\":\"  myapp\\n\"}"
            )
            .await
            .0,
            StatusCode::OK,
            "the handler would have created \"myapp\", which this credential owns"
        );
        // Padding is not a way past the check either: the name still has to
        // be one the credential holds.
        assert_eq!(
            call_with_body(
                &gate,
                "POST",
                "/api/apps",
                Some(&secret),
                "{\"id\":\"  someone-elses \"}"
            )
            .await
            .0,
            StatusCode::FORBIDDEN
        );
    }
}
