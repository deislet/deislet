//! One gate in front of the human API: who is calling, may they, and write
//! it down.
//!
//! Everything the dashboard and the `deis` CLI reach goes through
//! [`authorize`]. That is deliberate. Spreading the check across handlers is
//! how a route ends up shipped with no check at all - this repository has
//! produced that defect before - so the requirement for every route lives in
//! one table ([`classify`]) which is a pure function of the method and the
//! path, and the middleware that applies it is layered where no route can be
//! mounted around it.
//!
//! # Fail closed
//!
//! [`classify`] has a final arm that demands `admin:*/*` for anything it does
//! not recognise. A route added without a line in the table therefore becomes
//! reachable only by an administrator, and is visible as such the first time
//! a scoped token is refused - rather than becoming an unguarded hole.
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
    /// the caller holds is enough.
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

/// What this method and path need. A pure function, so the whole
/// authorisation surface of this control plane can be read - and tested -
/// without starting one.
pub fn classify(method: &Method, path: &str) -> Requirement {
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
        ["api", "runtime", _node, "apps", app] => {
            Requirement::new("node.app.unload", Verb::Deploy, named(app), Resource::Any)
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
    /// `api_auth_enabled`. When it is off the human API is open exactly as it
    /// always was, and this gate records rather than refuses - see
    /// [`authorize`].
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
/// With `api_auth_enabled` off, step 1 admits everybody as anonymous and
/// step 3 is skipped - the human API stays exactly as open as it was before
/// accounts existed, because an upgrade must not brick a control plane whose
/// operator never turned authentication on. Step 5 still runs, so even an
/// unauthenticated platform gets a record of every deploy, rollback and
/// secret change.
pub async fn authorize(State(state): State<AuthzState>, request: Request, next: Next) -> Response {
    let path = request.uri().path().to_string();
    // Unchanged from the flat-key days: the health probe has no caller, and
    // Prometheus generally cannot present a credential.
    if path == "/health" || path == "/api/metrics" {
        return next.run(request).await;
    }

    let method = request.method().clone();
    let headers = request.headers().clone();
    let peer = request
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|ConnectInfo(addr)| addr.to_string());

    let principal = match resolve(&state, &headers).await {
        Ok(principal) => principal,
        Err(refusal) => {
            let requirement = classify(&method, &path);
            warn!(
                %method,
                path = %path,
                peer = peer.as_deref().unwrap_or("an unknown address"),
                reason = refusal.log_reason,
                credential = refusal.credential.as_deref().unwrap_or("none"),
                "Refused a call to the human API because the credential presented cannot be used; the caller is told only that a valid credential is required, so a stolen token learns nothing about why it stopped working"
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
            return unauthenticated();
        }
    };

    let mut requirement = classify(&method, &path);

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

    if state.enforced
        && let Err(denial) = principal.permits(&demand)
    {
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

/// Why a presented credential could not be used. Kept out of the answer to
/// the caller on purpose; it exists for the control plane's own log.
struct Refusal {
    log_reason: &'static str,
    /// The public half of whatever was presented, when there was one to name.
    credential: Option<String>,
}

/// Turn what a request carries into the caller it belongs to.
///
/// A configured `api_keys` entry is checked against the configuration file,
/// in constant time, exactly as it was before accounts existed; the database
/// holds no copy of it and cannot admit anyone. Anything shaped like a minted
/// token is looked up by its public id and its secret compared against a
/// stored digest.
async fn resolve(state: &AuthzState, headers: &HeaderMap) -> Result<Principal, Refusal> {
    let presented: Vec<String> = presented_credentials(headers)
        .into_iter()
        .map(|c| c.to_string())
        .collect();

    if presented.is_empty() {
        if !state.enforced {
            return Ok(Principal::anonymous());
        }
        return Err(Refusal {
            log_reason: "no credential was presented",
            credential: None,
        });
    }

    let mut refusal = Refusal {
        log_reason: "the credential presented matches no token this control plane knows",
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
                    credential: Some(token),
                };
            }
            Ok(Resolution::AccountDisabled { token, .. }) => {
                refusal = Refusal {
                    log_reason: "the token presented is valid and its account is switched off",
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
        // call goes through as it always did, and the audit line says
        // anonymous rather than inventing an account.
        return Ok(Principal::anonymous());
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

    let value = serde_json::from_slice::<serde_json::Value>(&bytes)
        .ok()
        .and_then(|body| {
            body.get(field)
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
        });

    // A body that is not JSON, or names nothing, is not this gate's problem:
    // the handler answers 400 for it. What matters here is that the resource
    // stays unnamed, which leaves the requirement at "any", and the handler
    // then refuses the request on its own terms.
    Ok((
        Request::from_parts(parts, axum::body::Body::from(bytes)),
        value,
    ))
}

fn unauthenticated() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        Json(json!({
            "error": "Unauthorized",
            "details": UNAUTHENTICATED_DETAIL,
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
            "api_auth_enabled is off: the dashboard and the human API are OPEN, so anyone who can \
             reach this port can deploy, rotate a secret and mint a storage token. Accounts, \
             scopes and tokens are recorded and NOT enforced while it is off; the audit log still \
             records every change, attributed to \"anonymous\". Set api_auth_enabled = true to \
             turn the scope checks on"
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
        classify(&Method::from_bytes(method.as_bytes()).unwrap(), path)
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
    use axum::routing::{get, post, put};
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
            .route("/api/audit", get(ok))
            .layer(axum::middleware::from_fn_with_state(
                state.clone(),
                authorize,
            ));

        Gate { router, state }
    }

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
        let mut builder = HttpRequest::builder()
            .method(method)
            .uri(path)
            .header("content-type", "application/json");
        if let Some(credential) = credential {
            builder = builder.header("Authorization", format!("Bearer {credential}"));
        }
        let response = gate
            .router
            .clone()
            .oneshot(builder.body(Body::from(body.to_string())).unwrap())
            .await
            .unwrap();
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
}
