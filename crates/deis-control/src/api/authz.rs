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
//! # Every call carries a credential
//!
//! There is no credential-free admission, from any peer address, under any
//! `Host`, through any proxy. What a caller without one gets is the identical
//! 401 of [`UNAUTHENTICATED_DETAIL`], audited as a caller nobody
//! authenticated; the one way in on a fresh install is the one-time bootstrap
//! credential, which is good for one successful call (see
//! [`BOOTSTRAP_ACTIONS`]). A credential that resolves is held to its own
//! scopes, and its account's role is the ceiling over them.
//!
//! A loopback peer address is not an identity, and neither is a page the
//! operator's own browser has open. The gate reads no peer address and no
//! browser header for admission: what admits a caller is a credential that
//! resolves, nothing else.
//!
//! # The query string is read for exactly one route
//!
//! `DELETE /api/runtime/:node/apps/:app` takes one environment of an
//! application off a node with `?environment=<name>`, and *every* environment
//! of it without. The path is identical either way, so the table reads that
//! one parameter; see [`narrowing_query_value`] for why it narrows the demand
//! only for a spelling that can mean one environment and nothing else.
//!
//! # The body is read for exactly four routes
//!
//! `POST /api/apps` and `POST /api/apps/:app/environments` name the thing
//! they create in the request body, not in the path, and `deis deploy` calls
//! both of them for a CI credential that is scoped to one environment. So the
//! gate peeks at those two bodies and narrows the demand to the name it finds.
//!
//! The two route writes - `POST` and `PUT` on
//! `/api/apps/:app/environments/:env/routes` - are read for a different
//! reason, and it is worth being exact about which. Their body names a
//! **hostname**, and a hostname is global: `deploy:<app>/<env>` has no
//! spelling for one, so there is nothing here for the demand to become. What
//! decides whether this application may claim that name is the platform's
//! grant registry, read in `catalog::store::CatalogStore::create_route`
//! inside the same database call that writes the row - one rule, in one
//! place. What the gate takes from the body is the *record*: the host, the
//! path prefix and the priority go into the audit line's target, on the
//! refusals as well as on the successes. Before that, the three rows a
//! hostname hijack left in the log were byte-identical with `"target": null`,
//! and the one record that could have found the abuse afterwards did not
//! contain the hostname.
//!
//! Four bodies, and only those four, capped at [`MAX_BODY_PEEK_BYTES`], with
//! the body handed on to the handler unchanged. No route that carries a
//! secret is on that list, which is what keeps a value out of the audit log
//! by construction rather than by care.
//!
//! # Two routes' demands are settled from a record
//!
//! `GET /api/operations/:id` and `POST /api/operations/:id/cancel` name an
//! operation by an id that carries no application or environment, and which
//! application and environment it is about - and therefore which scope may
//! read or cancel it - is written in the operation record: the application
//! and environment of a provider `deploy` or `rollback` (`read:<app>/<env>`
//! to read it, `deploy:<app>/<env>` to cancel it), `admin:*/*` for an
//! identity migration, a backend restore, and every other kind. This gate
//! holds no catalog handle and reads no record, so the table marks both
//! routes [`RecordClaim::OperationScope`], demands only that the caller hold
//! *some* scope, and the handlers (`api::migrations::get_operation` and
//! `api::migrations::cancel_operation`, through one shared check) load the
//! record and check the caller against the exact demand before anything
//! else, refusing with the same `insufficient-scope` 403 this gate would
//! give and with one fixed body whether or not the id exists. The 403 is
//! audited here like every other refusal: the audit line is written from the
//! status the response actually had, so a handler's refusal is recorded the
//! way the gate's own is. The marker is what keeps this table the one place
//! that says what a route is: a route so marked and a handler that forgot
//! the check would be visible as a pair, not as a line that is present.

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
//! # No credential, no admission
//!
//! Every caller presents a token; there is no anonymous path. A loopback peer
//! address is not an identity - other users on the same machine, containers
//! sharing its network namespace, an SSRF from another local service and a
//! DNS-rebinding page in the operator's browser all present a loopback
//! credential-free connection - so nothing about where a request came from
//! admits it. A caller who presents nothing, or something that resolves to
//! no token, is refused with 401 whatever their peer address, whatever the
//! `Host` header says, and whatever forwarding headers a proxy added. The
//! dev and demo flows log in through the bootstrap credential like
//! production does (see [`crate::identity::store`] and the bootstrap file).
//!
//! # The bootstrap credential is good for exactly one thing
//!
//! A request that resolved to [`PrincipalKind::Bootstrap`] may only create
//! the first owner account or mint its first token (the two actions in
//! [`BOOTSTRAP_ACTIONS`]); anything else is refused with 403. Its first
//! successful use revokes it and deletes the bootstrap file, so it is
//! one-time even though nothing on the wire marks it as such.
//!
//! # The query string is read for exactly one route
//!
//! `DELETE /api/runtime/:node/apps/:app` takes one environment of an
//! application off a node with `?environment=<name>`, and *every* environment
//! of it without. The path is identical either way, so the table reads that
//! one parameter; see [`narrowing_query_value`] for why it narrows the demand
//! only for a spelling that can mean one environment and nothing else.
//!
//! # The body is read for exactly four routes
//!
//! `POST /api/apps` and `POST /api/apps/:app/environments` name the thing
//! they create in the request body, not in the path, and `deis deploy` calls
//! both of them for a CI credential that is scoped to one environment. So the
//! gate peeks at those two bodies and narrows the demand to the name it finds.
//!
//! The two route writes - `POST` and `PUT` on
//! `/api/apps/:app/environments/:env/routes` - are read for a different
//! reason, and it is worth being exact about which. Their body names a
//! **hostname**, and a hostname is global: `deploy:<app>/<env>` has no
//! spelling for one, so there is nothing here for the demand to become. What
//! decides whether this application may claim that name is the platform's
//! grant registry, read in `catalog::store::CatalogStore::create_route`
//! inside the same database call that writes the row - one rule, in one
//! place. What the gate takes from the body is the *record*: the host, the
//! path prefix and the priority go into the audit line's target, on the
//! refusals as well as on the successes. Before that, the three rows a
//! hostname hijack left in the log were byte-identical with `"target": null`,
//! and the one record that could have found the abuse afterwards did not
//! contain the hostname.
//!
//! Four bodies, and only those four, capped at [`MAX_BODY_PEEK_BYTES`], with
//! the body handed on to the handler unchanged. No route that carries a
//! secret is on that list, which is what keeps a value out of the audit log
//! by construction rather than by care.
//!
//! # Two routes' demands are settled from a record
//!
//! `GET /api/operations/:id` and `POST /api/operations/:id/cancel` name an
//! operation by an id that carries no application or environment, and which
//! application and environment it is about - and therefore which scope may
//! read or cancel it - is written in the operation record: the application
//! and environment of a provider `deploy` or `rollback` (`read:<app>/<env>`
//! to read it, `deploy:<app>/<env>` to cancel it), `admin:*/*` for an
//! identity migration, a backend restore, and every other kind. This gate
//! holds no catalog handle and reads no record, so the table marks both
//! routes [`RecordClaim::OperationScope`], demands only that the caller hold
//! *some* scope, and the handlers (`api::migrations::get_operation` and
//! `api::migrations::cancel_operation`, through one shared check) load the
//! record and check the caller against the exact demand before anything
//! else, refusing with the same `insufficient-scope` 403 this gate would
//! give and with one fixed body whether or not the id exists. The 403 is
//! audited here like every other refusal: the audit line is written from the
//! status the response actually had, so a handler's refusal is recorded the
//! way the gate's own is. The marker is what keeps this table the one place
//! that says what a route is: a route so marked and a handler that forgot
//! the check would be visible as a pair, not as a line that is present.

use std::net::SocketAddr;
use std::sync::Arc;

use axum::{
    extract::{ConnectInfo, Request, State},
    http::{HeaderMap, Method, StatusCode},
    middleware::Next,
    response::Response,
};
use deis_wire::CanonicalId;
use tracing::{error, info, warn};

use crate::api::error::{ApiError, ErrorDetail};
use crate::api::versioning::{ApiSurface, REQUEST_ID_HEADER, RequestContext};
use crate::audit::{AuditRecord, AuditStore, Outcome};
use crate::identity::store::Resolution;
use crate::identity::{Demand, IdentityStore, Principal, PrincipalKind, Selector, Verb};

/// The route actions the one-time bootstrap credential may perform: create
/// the first owner account, and mint its first token. Everything else is
/// refused, and the first successful one of these revokes the credential and
/// deletes the bootstrap file.
const BOOTSTRAP_ACTIONS: [&str; 2] = ["account.create", "token.mint"];

/// Largest request body the gate will read in order to learn which
/// application or environment a call is about. The two routes it applies to
/// carry a JSON object with one field.
pub const MAX_BODY_PEEK_BYTES: usize = 64 * 1024;

/// The identical answer every unusable credential gets. A caller must not be
/// able to tell "no such token" from "revoked" from "the account is switched
/// off" - that is a free oracle for anyone holding a stolen credential - so
/// those cases differ only in the control plane's own log.
const UNAUTHENTICATED_DETAIL: &str = "A valid credential is required. Present it as \
                                      'Authorization: Bearer <token>' or 'X-API-Key: <token>'. \
                                      Mint one with `deis token create`; on a fresh install, \
                                      log in with `deis login --bootstrap` using the \
                                      bootstrap.token file the control plane wrote to its \
                                      data directory.";

/// Every path [`authorize`] hands straight to the handler without resolving a
/// caller, checking a scope or writing a line.
///
/// Kept as a list rather than spelled into the `if`, because an exemption and
/// [`classify`] are two descriptions of the same set of routes and they can
/// disagree. `/api/metrics` was in both: exempt here and `metrics.read`
/// there, with the exemption winning and the table entry never running, so a
/// route that looked guarded was served to any host with nothing presented.
/// `nothing_the_gate_stands_aside_for_is_a_route_the_table_guards` holds the
/// two together.
const UNGATED_PATHS: [&str; 1] = ["/health"];

/// What a route names in its request body rather than in its URL, and what
/// the gate does about it.
///
/// Two of these narrow the *demand* - the caller must hold a scope for the
/// application or environment they are creating. The third narrows nothing,
/// because no scope can spell a hostname: it fills in the audit line's
/// target so the record of a route write says which address was claimed. The
/// claim itself is authorised against the platform's grant registry, in
/// `catalog::store::CatalogStore::create_route`, inside the same database
/// call that writes the row. Splitting that between here and there would be
/// two rules that can disagree, and this one would be the copy that went
/// stale.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BodyClaim {
    /// The application being created, named by `id`.
    App,
    /// The environment being created, named by `name`.
    Environment,
    /// The address a route claims: `host`, `path_prefix`, `priority`. Read
    /// for the record, not for the demand.
    RouteAddress,
}

/// What a route's exact demand is read from *after* the gate, by the handler,
/// because it is written in a record this gate cannot load.
///
/// The gate's own demand for such a route is only "hold some scope": it
/// refuses the caller who holds nothing, and it leaves the exact question to
/// the handler, which loads the record and answers it before doing anything
/// else. See the module header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecordClaim {
    /// The application and environment an operation is scoped to, and the
    /// verb its kind answers to, read from the `ProductOperation` record.
    /// Carried by the generic operation read and the generic operation
    /// cancel; the two differ only in the verb a provider workflow answers
    /// to (`Read` to read it, `Deploy` to cancel it).
    OperationScope,
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

/// Which face of the API a route belongs to: the product API, whose
/// versioned spelling is `/api/v1`, or the platform diagnostics, whose
/// spelling is `/api/platform`.
///
/// Written on the requirement rather than worked out from the path, so the
/// table stays the one place that says what a route is. The gate reads it to
/// refuse a route spelled under the other prefix: `/api/platform/apps` names
/// no route, and a caller who spells it is not thereby an application
/// reader - see [`classify`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequirementScope {
    /// Applications and everything inside them, accounts, tokens, the audit
    /// log, hostname grants, providers and long operations.
    Product,
    /// The fleet and the control plane itself: node sessions, node
    /// configuration and commands, events, metrics, the runtime nodes and the
    /// global route table.
    Operator,
}

/// What one route needs before it may run, and what the log should call it.
#[derive(Debug, Clone, PartialEq, Eq)]
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
    /// What this route names in its body instead of in its path.
    pub from_body: Option<BodyClaim>,
    /// What the handler reads the exact demand from, when the gate's own
    /// demand is only the coarse "hold some scope".
    pub from_record: Option<RecordClaim>,
    /// Which surface the route is served on. Product unless the table says
    /// otherwise; the closing arm is an operator's, like everything it
    /// refuses.
    pub scope: RequirementScope,
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
            from_record: None,
            scope: RequirementScope::Product,
        }
    }

    fn changes_state(mut self) -> Self {
        self.audit = AuditWhen::Always;
        self
    }

    /// A platform diagnostic: served under `/api/platform`, never `/api/v1`.
    fn operator(mut self) -> Self {
        self.scope = RequirementScope::Operator;
        self
    }

    fn about(mut self, target: &str) -> Self {
        self.target = Some(target.to_string());
        self
    }

    fn named_in_body(mut self, claim: BodyClaim) -> Self {
        self.from_body = Some(claim);
        self
    }

    /// The exact demand is in a record the handler loads; what this gate
    /// demands is that the caller hold some scope at all.
    fn settled_from_record(mut self, claim: RecordClaim) -> Self {
        self.from_record = Some(claim);
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
///
/// The table is written once, in the legacy spelling. A path under
/// `/api/v1` or `/api/platform` is the same route with its surface named, and
/// is folded onto the legacy spelling by [`canonical_segments`] before the
/// match; a route folded from the *wrong* surface - `/api/v1/runtime`,
/// `/api/platform/apps` - is nothing this control plane mounts, and is
/// answered with the closing arm rather than with the requirement of a route
/// the caller did not name.
pub fn classify(method: &Method, path: &str, query: Option<&str>) -> Requirement {
    let segments: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    let (surface, segments) = canonical_segments(&segments);
    let writing = method != Method::GET && method != Method::HEAD;

    let requirement = classify_canonical(method, &segments, query, writing);
    match (surface, requirement.scope) {
        (ApiSurface::V1, RequirementScope::Operator)
        | (ApiSurface::Platform, RequirementScope::Product) => unclassified(),
        _ => requirement,
    }
}

/// The legacy spelling of a path's segments, and the surface the path named.
///
/// `["api", "v1", rest..]` and `["api", "platform", rest..]` become
/// `["api", rest..]`. Exactly one marker is folded, and only in second
/// position, so `/api/v1/v1/apps` and `/api/platform/v1/apps` keep their extra
/// segment and fall to the closing arm; the comparison is byte-exact, so
/// `/api/V1/apps` does too. Nothing else is touched: `/api/nodes/..` and
/// `/api/routing-table` are the node channel, which is never versioned and
/// never reaches this gate.
fn canonical_segments<'a>(segments: &[&'a str]) -> (ApiSurface, Vec<&'a str>) {
    match segments {
        ["api", "v1", rest @ ..] => (ApiSurface::V1, fold(rest)),
        ["api", "platform", rest @ ..] => (ApiSurface::Platform, fold(rest)),
        _ => (ApiSurface::Legacy, segments.to_vec()),
    }
}

fn fold<'a>(rest: &[&'a str]) -> Vec<&'a str> {
    std::iter::once("api").chain(rest.iter().copied()).collect()
}

/// Anything the table does not know is an administrator's until somebody
/// writes down what it should be. See the module header.
fn unclassified() -> Requirement {
    Requirement::new(
        "unclassified",
        Verb::Admin,
        Resource::Every,
        Resource::Every,
    )
    .changes_state()
    .operator()
}

/// The route table proper, over segments already in the legacy spelling.
fn classify_canonical(
    method: &Method,
    segments: &[&str],
    query: Option<&str>,
    writing: bool,
) -> Requirement {
    match segments {
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
        .named_in_body(BodyClaim::App)
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

        // Identity migration freezes every environment and reaches backend
        // management services with credentials no application operator may
        // hold. Both starting it and reading its cross-backend inventory are
        // therefore platform-administrator operations over the whole app.
        ["api", "apps", app, "identity-migration"] if !writing => Requirement::new(
            "identity_migration.get",
            Verb::Admin,
            named(app),
            Resource::Every,
        ),
        ["api", "apps", app, "identity-migration"] => Requirement::new(
            "identity_migration.start",
            Verb::Admin,
            named(app),
            Resource::Every,
        )
        .changes_state(),

        // A backend restore replaces the physical data incarnation for every
        // application on this installation. Its scope can therefore never be
        // narrowed to an application or environment credential.
        ["api", "installation"] => Requirement::new(
            "installation.get",
            Verb::Admin,
            Resource::Every,
            Resource::Every,
        ),
        ["api", "backend-restores"] => Requirement::new(
            "backend_restore.start",
            Verb::Admin,
            Resource::Every,
            Resource::Every,
        )
        .changes_state(),
        ["api", "backend-restores", restore_id] => Requirement::new(
            "backend_restore.get",
            Verb::Admin,
            Resource::Every,
            Resource::Every,
        )
        .about(restore_id),

        // Provider credentials and account installations are platform
        // authority, not an application-scoped deployment secret. Opaque
        // IDs cannot narrow that authority, so every management route stays
        // administrative even when it is read-only.
        ["api", "providers", "driver-releases"] if !writing => Requirement::new(
            "provider_driver_release.list",
            Verb::Admin,
            Resource::Every,
            Resource::Every,
        ),
        ["api", "providers", "driver-releases"] => Requirement::new(
            "provider_driver_release.install",
            Verb::Admin,
            Resource::Every,
            Resource::Every,
        )
        .changes_state(),
        ["api", "providers", "driver-releases", release_digest] => Requirement::new(
            "provider_driver_release.get",
            Verb::Admin,
            Resource::Every,
            Resource::Every,
        )
        .about(release_digest),
        [
            "api",
            "providers",
            "driver-releases",
            release_digest,
            "audit",
        ] => Requirement::new(
            "provider_driver_release.audit",
            Verb::Admin,
            Resource::Every,
            Resource::Every,
        )
        .about(release_digest),
        ["api", "providers", "credentials"] if !writing => Requirement::new(
            "provider_credential.list",
            Verb::Admin,
            Resource::Every,
            Resource::Every,
        ),
        ["api", "providers", "credentials"] => Requirement::new(
            "provider_credential.provision",
            Verb::Admin,
            Resource::Every,
            Resource::Every,
        )
        .changes_state(),
        ["api", "providers", "credentials", binding_id] if !writing => Requirement::new(
            "provider_credential.get",
            Verb::Admin,
            Resource::Every,
            Resource::Every,
        )
        .about(binding_id),
        ["api", "providers", "credentials", binding_id] => Requirement::new(
            "provider_credential.revoke",
            Verb::Admin,
            Resource::Every,
            Resource::Every,
        )
        .about(binding_id)
        .changes_state(),
        ["api", "providers", "credentials", binding_id, "rotate"] => Requirement::new(
            "provider_credential.rotate",
            Verb::Admin,
            Resource::Every,
            Resource::Every,
        )
        .about(binding_id)
        .changes_state(),
        ["api", "providers", "credentials", binding_id, "audit"] => Requirement::new(
            "provider_credential.audit",
            Verb::Admin,
            Resource::Every,
            Resource::Every,
        )
        .about(binding_id),
        ["api", "providers", "installations"] if !writing => Requirement::new(
            "provider_installation.list",
            Verb::Admin,
            Resource::Every,
            Resource::Every,
        ),
        ["api", "providers", "installations"] => Requirement::new(
            "provider_installation.register",
            Verb::Admin,
            Resource::Every,
            Resource::Every,
        )
        .changes_state(),
        ["api", "providers", "installations", installation_id] if !writing => Requirement::new(
            "provider_installation.get",
            Verb::Admin,
            Resource::Every,
            Resource::Every,
        )
        .about(installation_id),
        ["api", "providers", "installations", installation_id] => Requirement::new(
            "provider_installation.delete",
            Verb::Admin,
            Resource::Every,
            Resource::Every,
        )
        .about(installation_id)
        .changes_state(),
        [
            "api",
            "providers",
            "installations",
            installation_id,
            "disable",
        ] => Requirement::new(
            "provider_installation.disable",
            Verb::Admin,
            Resource::Every,
            Resource::Every,
        )
        .about(installation_id)
        .changes_state(),
        [
            "api",
            "providers",
            "installations",
            installation_id,
            "enable",
        ] => Requirement::new(
            "provider_installation.enable",
            Verb::Admin,
            Resource::Every,
            Resource::Every,
        )
        .about(installation_id)
        .changes_state(),
        [
            "api",
            "providers",
            "installations",
            installation_id,
            "audit",
        ] => Requirement::new(
            "provider_installation.audit",
            Verb::Admin,
            Resource::Every,
            Resource::Every,
        )
        .about(installation_id),
        [
            "api",
            "providers",
            "installations",
            installation_id,
            "targets",
        ] if !writing => Requirement::new(
            "provider_target.list_for_installation",
            Verb::Admin,
            Resource::Every,
            Resource::Every,
        )
        .about(installation_id),
        [
            "api",
            "providers",
            "installations",
            installation_id,
            "targets",
        ] => Requirement::new(
            "provider_target.register",
            Verb::Admin,
            Resource::Every,
            Resource::Every,
        )
        .about(installation_id)
        .changes_state(),
        ["api", "providers", "targets"] => Requirement::new(
            "provider_target.list",
            Verb::Admin,
            Resource::Every,
            Resource::Every,
        ),
        ["api", "providers", "targets", target_id] if !writing => Requirement::new(
            "provider_target.get",
            Verb::Admin,
            Resource::Every,
            Resource::Every,
        )
        .about(target_id),
        ["api", "providers", "targets", target_id] => Requirement::new(
            "provider_target.delete",
            Verb::Admin,
            Resource::Every,
            Resource::Every,
        )
        .about(target_id)
        .changes_state(),
        ["api", "providers", "targets", target_id, "disable"] => Requirement::new(
            "provider_target.disable",
            Verb::Admin,
            Resource::Every,
            Resource::Every,
        )
        .about(target_id)
        .changes_state(),
        ["api", "providers", "targets", target_id, "enable"] => Requirement::new(
            "provider_target.enable",
            Verb::Admin,
            Resource::Every,
            Resource::Every,
        )
        .about(target_id)
        .changes_state(),
        ["api", "providers", "targets", target_id, "audit"] => Requirement::new(
            "provider_target.audit",
            Verb::Admin,
            Resource::Every,
            Resource::Every,
        )
        .about(target_id),

        // The global grant history spans every application, including ones
        // that no longer exist, so no application scope can cover it.
        ["api", "providers", "target-grants"] => Requirement::new(
            "provider_target_grant.history",
            Verb::Admin,
            Resource::Every,
            Resource::Every,
        ),
        ["api", "providers", "target-grants", grant_id] => Requirement::new(
            "provider_target_grant.history_get",
            Verb::Admin,
            Resource::Every,
            Resource::Every,
        )
        .about(grant_id),
        ["api", "providers", "target-grants", grant_id, "audit"] => Requirement::new(
            "provider_target_grant.history_audit",
            Verb::Admin,
            Resource::Every,
            Resource::Every,
        )
        .about(grant_id),

        // Neither reading an operation nor cancelling one is an
        // administrator's alone: a provider `deploy` or `rollback` answers
        // to the scope of the application and environment in its record -
        // `read:<app>/<env>` to read it, exactly as its scoped
        // `provider_deployment.get` route does, and `deploy:<app>/<env>` to
        // cancel it, exactly as `provider_deployment.cancel` does - and only
        // a migration, a restore or a kind with no scope of its own answers
        // to `admin:*/*`. Which it is can only be read from the record, so
        // the gate asks for some scope - `Read` is what every scope grants -
        // and the handlers in `api::migrations` load the record and refuse
        // with `insufficient-scope` before they answer or act. See the
        // module header. The read changes nothing, so it is recorded only
        // when refused, like every other read.
        ["api", "operations", operation_id] if !writing => {
            Requirement::new("operation.get", Verb::Read, Resource::Any, Resource::Any)
                .about(operation_id)
                .settled_from_record(RecordClaim::OperationScope)
        }
        ["api", "operations", operation_id, "cancel"] => {
            Requirement::new("operation.cancel", Verb::Read, Resource::Any, Resource::Any)
                .about(operation_id)
                .changes_state()
                .settled_from_record(RecordClaim::OperationScope)
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
        ["api", "apps", app, "artifacts"] if !writing => {
            Requirement::new("artifact.list", Verb::Read, named(app), Resource::Any)
        }
        ["api", "apps", app, "artifacts"] => {
            Requirement::new("artifact.upload", Verb::Deploy, named(app), Resource::Any)
                .changes_state()
        }
        ["api", "apps", app, "artifacts", artifact] if !writing => {
            Requirement::new("artifact.get", Verb::Read, named(app), Resource::Any).about(artifact)
        }
        ["api", "apps", app, "artifacts", artifact, "versions"] => Requirement::new(
            "version.upload_lineage",
            Verb::Deploy,
            named(app),
            Resource::Any,
        )
        .about(artifact)
        .changes_state(),

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
        .named_in_body(BodyClaim::Environment)
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
        [
            "api",
            "apps",
            app,
            "environments",
            env,
            "provider-target-grants",
        ] if !writing => Requirement::new(
            "provider_target_grant.list",
            Verb::Read,
            named(app),
            named(env),
        ),
        [
            "api",
            "apps",
            _app,
            "environments",
            _env,
            "provider-target-grants",
        ] => Requirement::new(
            "provider_target_grant.create",
            Verb::Admin,
            Resource::Every,
            Resource::Every,
        )
        .changes_state(),
        [
            "api",
            "apps",
            app,
            "environments",
            env,
            "provider-target-grants",
            grant_id,
        ] if !writing => Requirement::new(
            "provider_target_grant.get",
            Verb::Read,
            named(app),
            named(env),
        )
        .about(grant_id),
        [
            "api",
            "apps",
            _app,
            "environments",
            _env,
            "provider-target-grants",
            grant_id,
        ] => Requirement::new(
            "provider_target_grant.revoke",
            Verb::Admin,
            Resource::Every,
            Resource::Every,
        )
        .about(grant_id)
        .changes_state(),
        [
            "api",
            "apps",
            app,
            "environments",
            env,
            "provider-target-grants",
            grant_id,
            "audit",
        ] if !writing => Requirement::new(
            "provider_target_grant.audit",
            Verb::Read,
            named(app),
            named(env),
        )
        .about(grant_id),
        [
            "api",
            "apps",
            app,
            "environments",
            env,
            "provider-deployments",
        ] if !writing => Requirement::new(
            "provider_deployment.list",
            Verb::Read,
            named(app),
            named(env),
        ),
        [
            "api",
            "apps",
            app,
            "environments",
            env,
            "provider-deployments",
        ] => Requirement::new(
            "provider_deployment.start",
            // The catalog transaction independently requires an exact active
            // target grant for this canonical application/environment. Route
            // scope alone never confers access to a provider account.
            Verb::Deploy,
            named(app),
            named(env),
        )
        .changes_state(),
        [
            "api",
            "apps",
            app,
            "environments",
            env,
            "provider-deployments",
            operation_id,
        ] => Requirement::new(
            "provider_deployment.get",
            Verb::Read,
            named(app),
            named(env),
        )
        .about(operation_id),
        [
            "api",
            "apps",
            app,
            "environments",
            env,
            "provider-deployments",
            operation_id,
            "cancel",
        ] => Requirement::new(
            "provider_deployment.cancel",
            Verb::Deploy,
            named(app),
            named(env),
        )
        .about(operation_id)
        .changes_state(),
        [
            "api",
            "apps",
            app,
            "environments",
            env,
            "provider-deployments",
            operation_id,
            "rollback",
        ] => Requirement::new(
            "provider_deployment.rollback",
            // The catalog transaction requires the rolled-back deployment to
            // be a succeeded one on this environment and its target grant to
            // be the active one; route scope alone never confers that.
            Verb::Deploy,
            named(app),
            named(env),
        )
        .about(operation_id)
        .changes_state(),
        [
            "api",
            "apps",
            app,
            "environments",
            env,
            "deployment-plans",
            plan,
        ] => {
            Requirement::new("deployment.plan.get", Verb::Read, named(app), named(env)).about(plan)
        }
        // Opaque plan ids do not encode their tenant. The scoped route above
        // is for application credentials; this cluster-wide fallback is an
        // operator diagnostic and therefore explicitly administrative.
        ["api", "deployment-plans", plan] => Requirement::new(
            "deployment.plan.get_global",
            Verb::Admin,
            Resource::Every,
            Resource::Every,
        )
        .about(plan),
        ["api", "apps", app, "environments", env, "rollback"] => {
            Requirement::new("rollback", Verb::Deploy, named(app), named(env)).changes_state()
        }

        ["api", "apps", app, "environments", env, "routes"] if !writing => {
            Requirement::new("route.list", Verb::Read, named(app), named(env))
        }
        // The two calls that claim a hostname. The demand is the caller's own
        // application and environment, because that is all a scope can say;
        // the address being claimed is read out of the body so the audit line
        // names it, and is authorised against the platform's grant registry
        // in `catalog::store`. Three byte-identical `"target": null` rows
        // were what the log had of the hijack that prompted this.
        ["api", "apps", app, "environments", env, "routes"] => {
            Requirement::new("route.create", Verb::Deploy, named(app), named(env))
                .named_in_body(BodyClaim::RouteAddress)
                .changes_state()
        }
        ["api", "apps", app, "environments", env, "routes", route] if method == Method::DELETE => {
            Requirement::new("route.delete", Verb::Deploy, named(app), named(env))
                .about(route)
                .changes_state()
        }
        ["api", "apps", app, "environments", env, "routes", route] => {
            Requirement::new("route.update", Verb::Deploy, named(app), named(env))
                .about(route)
                .named_in_body(BodyClaim::RouteAddress)
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

        // ---- who may claim which hostname ---------------------------------
        //
        // `Resource::Every` on all three, and `Verb::Admin`, because a grant
        // decides which tenant receives somebody else's cookies. It is a
        // statement about the whole platform, exactly like an account or a
        // token, and it names an application that is deliberately *not* the
        // caller's - `named(app)` here would mean an application could grant
        // itself the name it wanted, which is the hole this registry exists
        // to close.
        ["api", "domains"] if !writing => Requirement::new(
            "domain_grant.list",
            Verb::Admin,
            Resource::Every,
            Resource::Every,
        ),
        ["api", "domains"] => Requirement::new(
            "domain_grant.create",
            Verb::Admin,
            Resource::Every,
            Resource::Every,
        )
        .changes_state(),
        ["api", "domains", pattern] => Requirement::new(
            "domain_grant.delete",
            Verb::Admin,
            Resource::Every,
            Resource::Every,
        )
        .about(pattern)
        .changes_state(),

        // ---- the platform itself -----------------------------------------
        ["api", "routes"] => Requirement::new(
            "route.list_all",
            Verb::Read,
            Resource::Every,
            Resource::Every,
        )
        .operator(),
        ["api", "sessions", ..] => {
            Requirement::new("session.read", Verb::Read, Resource::Every, Resource::Every)
                .operator()
        }
        ["api", "events", ..] => {
            Requirement::new("event.read", Verb::Read, Resource::Every, Resource::Every).operator()
        }
        // Both the Prometheus text at `/api/metrics` and the JSON summary
        // next to it. The exposition names no application, but it counts all
        // of them - cluster size, node count, uptime, event totals - so it is
        // `Every` rather than `Any`: a credential scoped to one preview
        // environment has no business reading the size of the fleet.
        //
        // A scrape from another host therefore carries a token minted for it
        // (`deis token create --scopes 'read:*/*'`), which is a real
        // requirement Prometheus meets with `authorization` in the scrape
        // config. A scrape from this machine needs one too, exactly like
        // every other read.
        ["api", "metrics", ..] => {
            Requirement::new("metrics.read", Verb::Read, Resource::Every, Resource::Every)
                .operator()
        }
        ["api", "configs", ..] if !writing => Requirement::new(
            "node.config.read",
            Verb::Read,
            Resource::Every,
            Resource::Every,
        )
        .operator(),
        ["api", "configs", ..] => Requirement::new(
            "node.config.write",
            Verb::Admin,
            Resource::Every,
            Resource::Every,
        )
        .changes_state()
        .operator(),
        ["api", "commands", ..] if !writing => Requirement::new(
            "node.command.read",
            Verb::Read,
            Resource::Every,
            Resource::Every,
        )
        .operator(),
        ["api", "commands", ..] => Requirement::new(
            "node.command",
            Verb::Admin,
            Resource::Every,
            Resource::Every,
        )
        .changes_state()
        .operator(),

        // The fleet list is not "which machines exist": every row carries a
        // node's management address, its key status and its key
        // fingerprint. `docs/adr/016-api-cli-web-console.md` puts fleet
        // scope with the platform operator - an application user does not
        // get node shells, global configuration or another tenant's
        // telemetry by being able to deploy - so this demands the same
        // `read:*/*` as every other diagnostic under `/api/platform`.
        // `deis status` prints the fleet when the credential holds it and
        // says it cannot see it when it does not, rather than failing.
        ["api", "runtime"] => {
            Requirement::new("node.list", Verb::Read, Resource::Every, Resource::Every).operator()
        }
        // What one node is running names every application on it, which is
        // another tenant's business.
        ["api", "runtime", _node, "status"] => {
            Requirement::new("node.status", Verb::Read, Resource::Every, Resource::Every).operator()
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
                .operator()
        }

        // Anything this table does not know: see `unclassified`.
        _ => unclassified(),
    }
}

/// What the gate needs in order to answer a request.
#[derive(Clone)]
pub struct AuthzState {
    pub identity: Arc<IdentityStore>,
    pub audit: Arc<AuditStore>,
    /// Where the one-time bootstrap credential file lives
    /// (`<data_dir>/bootstrap.token`). The gate deletes it right after the
    /// bootstrap token's first successful use, next to the revocation. `None`
    /// only in tests that never present a bootstrap credential: a binary
    /// always knows its data directory, and a bootstrap credential whose file
    /// cannot be located could never be cleaned up.
    pub bootstrap_file: Option<Arc<crate::identity::bootstrap::BootstrapFile>>,
}

/// Authenticate, authorise, and record.
///
/// The order matters and is the same on every route:
///
/// 1. resolve the presented credential into a [`Principal`], or refuse with
///    401 - there is no credential-free admission, from any peer address,
///    under any `Host`, through any proxy;
/// 2. work out what this route needs from [`classify`], reading the body only
///    for the routes that name their resource there;
/// 3. check the principal against it; a bootstrap credential is further held
///    to [`BOOTSTRAP_ACTIONS`];
/// 4. run the request;
/// 5. write the audit line, with the outcome the response actually had - and
///    for a bootstrap credential, revoke it and delete its file after the
///    first successful use.
pub async fn authorize(State(state): State<AuthzState>, request: Request, next: Next) -> Response {
    let path = request.uri().path().to_string();
    // The one route in front of this gate, and the only one that can be:
    // `/health` answers the constant "OK", so a prober learns nothing a TCP
    // connect did not already tell it, and the load balancer or orchestrator
    // asking has no credential to present and no way to be given one.
    //
    // `/api/metrics` used to be on this line too. It is not a probe: it hands
    // out the cluster size, the node count, the uptime and the event totals,
    // and being here meant it ignored the credential requirement
    // altogether, so any host that could route to the port read them. A
    // scrape goes through the gate like every other read.
    if UNGATED_PATHS.contains(&path.as_str()) {
        return next.run(request).await;
    }

    let method = request.method().clone();
    let headers = request.headers().clone();
    let query = request.uri().query().map(str::to_string);
    // Settled before anything is decided, so a refusal is audited and
    // answered under one id. The gate runs outside the surface mounts, so
    // this is the caller's own id when it sent a usable one and a fresh one
    // otherwise; an accepted call is re-read from the answer below, because
    // the mount it went on to settles its own.
    let context = gate_request_context(&request);
    let request_id = context.request_id;
    let surface = ApiSurface::of_path(&path);
    let peer = request
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|ConnectInfo(addr)| addr.to_string());

    let principal = match resolve(&state, &headers).await {
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
                        .request_id(Some(request_id.to_string()))
                        .build(),
                )
                .await;
            return ApiError::unauthenticated(refusal.caller_detail)
                .into_response_for(surface, context);
        }
    };

    let mut requirement = classify(&method, &path, query.as_deref());

    // The four routes that name something in the body. Everything else keeps
    // its body untouched and unread.
    let request = match requirement.from_body {
        None => request,
        Some(claim) => match peek_body(request).await {
            Ok((request, body)) => {
                apply_body_claim(&mut requirement, claim, body.as_ref());
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

    // A credential that was presented and resolved carries scopes its holder
    // was given on the understanding that they are the limit. The bootstrap
    // credential is narrower still: it exists to create the first owner
    // account and a first token for it, and anything else it might reach for
    // - a deploy, a secret, the audit log - is refused before the demand is
    // even considered.
    if principal.kind == PrincipalKind::Bootstrap
        && !BOOTSTRAP_ACTIONS.contains(&requirement.action)
    {
        warn!(
            %method,
            path = %path,
            caller = %principal.label(),
            action = requirement.action,
            "Refused a call made with the one-time bootstrap credential: it may only create the first owner account or mint its first token, and this route is neither"
        );
        let detail = "The one-time bootstrap credential may only create the first owner \
                      account or mint its first token. Log in with `deis login --bootstrap`, \
                      then mint a token with `deis token create`.";
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
                    .detail(detail)
                    .request_id(Some(request_id.to_string()))
                    .build(),
            )
            .await;
        return ApiError::insufficient_scope("admin:*/*", detail)
            .into_response_for(surface, context);
    }
    // The bootstrap credential carries no scope list - there is nothing to
    // hold it to. Its limit is [`BOOTSTRAP_ACTIONS`], checked above, and its
    // account's role; the empty list is not a denial.
    let permitted = if principal.kind == PrincipalKind::Bootstrap {
        Ok(())
    } else {
        principal.permits(&demand)
    };
    if let Err(denial) = permitted {
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
                    .request_id(Some(request_id.to_string()))
                    .build(),
            )
            .await;
        return ApiError::insufficient_scope(&demand.required_scope(), &detail)
            .into_response_for(surface, context);
    }

    // Handlers that want to know who is calling read it from here.
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
        let mut record = AuditRecord::by(&principal)
            .action(requirement.action)
            .request(method.as_str(), &path)
            .resource(requirement.app.name(), requirement.environment.name())
            .target(requirement.target)
            .peer(peer)
            .outcome(outcome, status.as_u16())
            // What the answer echoes is what the caller will quote:
            // the id the mount settled, which may be one it minted.
            .request_id(echoed_request_id(&response).or(Some(request_id.to_string())));
        // A handler that refused on a demand it read from a record - one
        // the body must not describe - leaves the demand on the answer's
        // error detail, so this line says what was unmet exactly as the
        // gate's own refusals above do, while the caller is told nothing.
        if let Some(detail) = response
            .extensions()
            .get::<ErrorDetail>()
            .and_then(|detail| detail.audit_detail.clone())
        {
            record = record.detail(detail);
        }
        state.audit.record(record.build()).await;
    }

    // The bootstrap credential is one-time, and this is where "once" is
    // decided: its first successful use revokes it in the database and
    // deletes the file that held it, so neither half of the credential can
    // ever be presented again. A failed attempt uses nothing up - an
    // operator's first spelling of the account name must not lock them out.
    if principal.kind == PrincipalKind::Bootstrap && response.status().is_success() {
        if let Some(credential) = principal.credential_id.as_deref()
            && let Err(e) = state.identity.revoke_bootstrap_token(credential).await
        {
            error!(
                error = %e,
                token = %credential,
                "The bootstrap credential was used successfully but could not be revoked; present it again and this control plane revokes it then, but until then a copy of the file still admits its holder"
            );
        }
        if let Some(file) = state.bootstrap_file.as_ref()
            && let Err(e) = file.delete()
        {
            warn!(
                error = %e,
                path = %file.path().display(),
                "The bootstrap credential was revoked but its file could not be deleted; remove it by hand - the credential it held no longer works"
            );
        }
    }

    response
}

/// Why a call could not be admitted. The reason is kept out of the answer to
/// the caller on purpose; it exists for the control plane's own log.
struct Refusal {
    log_reason: &'static str,
    /// What the caller is told. Every refusal gets
    /// [`UNAUTHENTICATED_DETAIL`] and nothing else, because telling reasons
    /// apart is a free oracle: whoever holds a stolen credential must not be
    /// able to learn from the answer whether it has been noticed.
    caller_detail: &'static str,
    /// The public half of whatever was presented, when there was one to name.
    credential: Option<String>,
}

/// The credentials a request carries, in the two forms this API accepts:
/// `Authorization: Bearer <token>` (or the bare token) and `X-API-Key: <token>`.
///
/// Private to this module because this module is the only thing that reads a
/// caller's credential. It was shared while the endpoints that served nodes
/// read one the same way; those are gone, and a helper kept visible for a
/// caller that no longer exists is an invitation to add a second gate.
fn presented_credentials(headers: &HeaderMap) -> Vec<&str> {
    let mut found = Vec::with_capacity(2);
    if let Some(value) = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
    {
        found.push(value.strip_prefix("Bearer ").unwrap_or(value));
    }
    if let Some(value) = headers.get("X-API-Key").and_then(|v| v.to_str().ok()) {
        found.push(value);
    }
    found
}

/// Turn what a request carries into the caller it belongs to, or refuse.
///
/// Every caller presents a token; there is no credential-free admission from
/// any peer address, under any `Host`, through any proxy. The only forms a
/// credential takes are the minted tokens this control plane knows - the
/// one-time bootstrap credential among them, indistinguishable on the wire
/// from any other token and narrowed to its two routes after it resolves.
async fn resolve(state: &AuthzState, headers: &HeaderMap) -> Result<Principal, Refusal> {
    let presented: Vec<String> = presented_credentials(headers)
        .into_iter()
        .map(|c| c.to_string())
        .collect();

    if presented.is_empty() {
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
            Ok(Resolution::Expired { token, .. }) => {
                refusal = Refusal {
                    log_reason: "the token presented stayed past its expiry, which is reported \
                                 like a revocation: both mean a withdrawn credential is still \
                                 out there",
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

    Err(refusal)
}

/// Read a JSON body and hand it on untouched.
///
/// Applied to exactly four routes; see the module header for why, and for
/// why no route carrying a secret is among them.
async fn peek_body(request: Request) -> Result<(Request, Option<serde_json::Value>), Response> {
    let (parts, body) = request.into_parts();
    let bytes = match axum::body::to_bytes(body, MAX_BODY_PEEK_BYTES).await {
        Ok(bytes) => bytes,
        Err(_) => {
            warn!(
                path = %parts.uri.path(),
                limit = MAX_BODY_PEEK_BYTES,
                "Refused a call because its body is larger than the gate will read; these routes take a small JSON object naming one thing"
            );
            return Err(ApiError::payload_too_large(format!(
                "This request's body is larger than {MAX_BODY_PEEK_BYTES} bytes, and this \
                 route takes a small JSON object naming what to create."
            ))
            .into_response_for(
                ApiSurface::of_path(parts.uri.path()),
                RequestContext::resolve(&parts.extensions, &parts.headers),
            ));
        }
    };

    let value = serde_json::from_slice::<serde_json::Value>(&bytes).ok();
    Ok((
        Request::from_parts(parts, axum::body::Body::from(bytes)),
        value,
    ))
}

/// One JSON field, trimmed, or `None` when it is absent or empty.
///
/// Trimmed, because the handler trims: `catalog::apps::create_app` stores
/// `request.id.trim()` and `catalog::environments::create_environment`
/// stores `request.name.trim()`. Judging the untrimmed spelling would mean
/// checking one name and creating another - and since no scope can spell a
/// name with a space in it (`identity::scope::Component`), a credential that
/// holds exactly `deploy:myapp/*` would be refused its own application over
/// a stray newline, and told to go and ask for `deploy: myapp\n/*`, which is
/// not a scope anyone can be granted.
fn body_field(body: Option<&serde_json::Value>, field: &str) -> Option<String> {
    body?
        .get(field)
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

/// Fold what the body names into the requirement.
///
/// A body that is not JSON, or names nothing, is not this gate's problem:
/// the handler answers 400 for it - `CatalogStore::create_app` and
/// `create_environment` both refuse an empty name. What matters here is that
/// the resource stays unnamed, which leaves the requirement at "any", and the
/// handler then refuses the request on its own terms.
fn apply_body_claim(
    requirement: &mut Requirement,
    claim: BodyClaim,
    body: Option<&serde_json::Value>,
) {
    match claim {
        BodyClaim::App => {
            if let Some(name) = body_field(body, "id") {
                requirement.app = Resource::Named(name);
            }
        }
        BodyClaim::Environment => {
            if let Some(name) = body_field(body, "name") {
                requirement.environment = Resource::Named(name);
            }
        }
        // Nothing about the demand changes: no scope can name a hostname, and
        // pretending one could would be a second authorisation rule beside
        // the grant registry that actually decides. What this fills in is the
        // audit line's target, so that the record of a route write says which
        // address was claimed - including on the refusals, which is where the
        // hijack attempts show up.
        //
        // Appended to what `.about()` already put there, never in its place:
        // a route *update* names the route id in the path and the address in
        // the body, and an audit row that dropped the id for the address
        // could no longer say which route was rewritten. And a body this gate
        // could not read is recorded as exactly that - `describe_route_address`
        // on a `None` body would spell out "(every host)", which is the
        // widest claim there is, positively recorded for a request that
        // named nothing. The handler answers 400 to such a body; the row
        // has to match.
        BodyClaim::RouteAddress => {
            let address = match body {
                Some(_) => describe_route_address(body),
                None => "(unreadable body: no address was named)".to_string(),
            };
            requirement.target = Some(match requirement.target.take() {
                Some(route) => format!("{route} {address}"),
                None => address,
            });
        }
    }
}

/// The address a route body claims, in one line, for the audit log.
///
/// Every field is named even when it is absent, because "no host" is the
/// *widest* claim a route can make - it answers for every name no other
/// route claims - and a target that simply omitted it would read like a
/// narrower call than it is. Nothing here is a value: a route body carries
/// an address, and addresses are already public to anyone who can make a
/// request through the edge.
fn describe_route_address(body: Option<&serde_json::Value>) -> String {
    let host = body_field(body, "host").unwrap_or_else(|| "(every host)".to_string());
    let path_prefix = body_field(body, "path_prefix").unwrap_or_else(|| "(none)".to_string());
    let priority = body
        .and_then(|body| body.get("priority"))
        .and_then(|value| value.as_i64())
        .unwrap_or(0);
    format!("host={host} path_prefix={path_prefix} priority={priority}")
}

/// The ids a call can be found by before it is answered: the ones the
/// surface's request context settled when the gate runs inside a mount,
/// otherwise the inbound `x-request-id` and `x-correlation-id` when each is
/// exactly one canonical id (the same rule the surfaces apply), otherwise
/// one minted here. A refusal is audited and answered under these ids, so
/// what the caller quotes is what the log holds; anything a client sent
/// that is not an id is never either.
///
/// The gate is layered outside all three mounts, so this is the only place
/// a refused call's ids come from: a 401 or a 403 that did not echo the
/// correlation id would leave the caller unable to tie the refusal to the
/// calls it sent around it.
fn gate_request_context(request: &Request) -> RequestContext {
    RequestContext::resolve(request.extensions(), request.headers())
}

/// The request id an answer carries, which is the one the caller holds.
fn echoed_request_id(response: &Response) -> Option<String> {
    response
        .headers()
        .get(REQUEST_ID_HEADER)?
        .to_str()
        .ok()?
        .parse::<CanonicalId>()
        .ok()
        .map(|id| id.to_string())
}

/// Say, once, what the human API answers to.
///
/// Printed at startup so an operator sees how the API admits callers, and
/// how many credentials are still unscoped - which is the number the whole
/// model exists to let them drive to zero.
pub fn log_authorization_ready(unscoped: usize) {
    if unscoped > 0 {
        warn!(
            unscoped,
            "The human API is authenticated, and that many live credentials are UNSCOPED: each one \
             may deploy any application, rotate any secret and mint any tenant's storage token. \
             `deis token list` names them; replace them with scoped tokens and revoke these"
        );
    } else {
        info!(
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

    /// The route table, spelled out in the legacy form: every route the
    /// human router mounts, with the action and verb the table gives it.
    /// Shared by the tests below, which read it three ways - as the routes
    /// that must classify, as the routes whose `/api/v1` or `/api/platform`
    /// spelling must classify identically, and as the set that decides which
    /// actions are an operator's.
    fn mounted_routes() -> Vec<(&'static str, &'static str, &'static str, Verb)> {
        vec![
            ("GET", "/api/apps", "app.list", Verb::Read),
            ("POST", "/api/apps", "app.create", Verb::Deploy),
            ("GET", "/api/apps/demo", "app.get", Verb::Read),
            ("DELETE", "/api/apps/demo", "app.delete", Verb::Admin),
            (
                "GET",
                "/api/apps/demo/identity-migration",
                "identity_migration.get",
                Verb::Admin,
            ),
            (
                "POST",
                "/api/operations/01990f44-1000-7000-8000-000000000040/cancel",
                "operation.cancel",
                Verb::Read,
            ),
            (
                "POST",
                "/api/apps/demo/identity-migration",
                "identity_migration.start",
                Verb::Admin,
            ),
            (
                "GET",
                "/api/operations/01990f44-1000-7000-8000-000000000040",
                "operation.get",
                Verb::Read,
            ),
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
                "GET",
                "/api/apps/demo/environments/production/provider-target-grants",
                "provider_target_grant.list",
                Verb::Read,
            ),
            (
                "POST",
                "/api/apps/demo/environments/production/provider-target-grants",
                "provider_target_grant.create",
                Verb::Admin,
            ),
            (
                "GET",
                "/api/apps/demo/environments/production/provider-target-grants/01990f44-1000-7000-8000-000000000040",
                "provider_target_grant.get",
                Verb::Read,
            ),
            (
                "DELETE",
                "/api/apps/demo/environments/production/provider-target-grants/01990f44-1000-7000-8000-000000000040",
                "provider_target_grant.revoke",
                Verb::Admin,
            ),
            (
                "GET",
                "/api/apps/demo/environments/production/provider-target-grants/01990f44-1000-7000-8000-000000000040/audit",
                "provider_target_grant.audit",
                Verb::Read,
            ),
            (
                "GET",
                "/api/apps/demo/environments/production/provider-deployments",
                "provider_deployment.list",
                Verb::Read,
            ),
            (
                "POST",
                "/api/apps/demo/environments/production/provider-deployments",
                "provider_deployment.start",
                Verb::Deploy,
            ),
            (
                "GET",
                "/api/apps/demo/environments/production/provider-deployments/01990f44-1000-7000-8000-000000000040",
                "provider_deployment.get",
                Verb::Read,
            ),
            (
                "POST",
                "/api/apps/demo/environments/production/provider-deployments/01990f44-1000-7000-8000-000000000040/cancel",
                "provider_deployment.cancel",
                Verb::Deploy,
            ),
            (
                "POST",
                "/api/apps/demo/environments/production/provider-deployments/01990f44-1000-7000-8000-000000000040/rollback",
                "provider_deployment.rollback",
                Verb::Deploy,
            ),
            (
                "GET",
                "/api/apps/demo/environments/production/deployment-plans/plan-1",
                "deployment.plan.get",
                Verb::Read,
            ),
            (
                "GET",
                "/api/deployment-plans/plan-1",
                "deployment.plan.get_global",
                Verb::Admin,
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
            ("GET", "/api/metrics", "metrics.read", Verb::Read),
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
            ("GET", "/api/domains", "domain_grant.list", Verb::Admin),
            ("POST", "/api/domains", "domain_grant.create", Verb::Admin),
            (
                "DELETE",
                "/api/domains/example.com",
                "domain_grant.delete",
                Verb::Admin,
            ),
            (
                "GET",
                "/api/providers/driver-releases",
                "provider_driver_release.list",
                Verb::Admin,
            ),
            (
                "POST",
                "/api/providers/driver-releases",
                "provider_driver_release.install",
                Verb::Admin,
            ),
            (
                "GET",
                "/api/providers/driver-releases/0000000000000000000000000000000000000000000000000000000000000000",
                "provider_driver_release.get",
                Verb::Admin,
            ),
            (
                "GET",
                "/api/providers/driver-releases/0000000000000000000000000000000000000000000000000000000000000000/audit",
                "provider_driver_release.audit",
                Verb::Admin,
            ),
            (
                "GET",
                "/api/providers/credentials",
                "provider_credential.list",
                Verb::Admin,
            ),
            (
                "POST",
                "/api/providers/credentials",
                "provider_credential.provision",
                Verb::Admin,
            ),
            (
                "GET",
                "/api/providers/credentials/01990f44-1000-7000-8000-000000000040",
                "provider_credential.get",
                Verb::Admin,
            ),
            (
                "DELETE",
                "/api/providers/credentials/01990f44-1000-7000-8000-000000000040",
                "provider_credential.revoke",
                Verb::Admin,
            ),
            (
                "POST",
                "/api/providers/credentials/01990f44-1000-7000-8000-000000000040/rotate",
                "provider_credential.rotate",
                Verb::Admin,
            ),
            (
                "GET",
                "/api/providers/credentials/01990f44-1000-7000-8000-000000000040/audit",
                "provider_credential.audit",
                Verb::Admin,
            ),
            (
                "GET",
                "/api/providers/installations",
                "provider_installation.list",
                Verb::Admin,
            ),
            (
                "POST",
                "/api/providers/installations",
                "provider_installation.register",
                Verb::Admin,
            ),
            (
                "GET",
                "/api/providers/installations/01990f44-1000-7000-8000-000000000040",
                "provider_installation.get",
                Verb::Admin,
            ),
            (
                "GET",
                "/api/providers/installations/01990f44-1000-7000-8000-000000000040/targets",
                "provider_target.list_for_installation",
                Verb::Admin,
            ),
            (
                "POST",
                "/api/providers/installations/01990f44-1000-7000-8000-000000000040/targets",
                "provider_target.register",
                Verb::Admin,
            ),
            (
                "GET",
                "/api/providers/targets",
                "provider_target.list",
                Verb::Admin,
            ),
            (
                "GET",
                "/api/providers/targets/01990f44-1000-7000-8000-000000000040",
                "provider_target.get",
                Verb::Admin,
            ),
            (
                "DELETE",
                "/api/providers/installations/01990f44-1000-7000-8000-000000000040",
                "provider_installation.delete",
                Verb::Admin,
            ),
            (
                "POST",
                "/api/providers/installations/01990f44-1000-7000-8000-000000000040/disable",
                "provider_installation.disable",
                Verb::Admin,
            ),
            (
                "POST",
                "/api/providers/installations/01990f44-1000-7000-8000-000000000040/enable",
                "provider_installation.enable",
                Verb::Admin,
            ),
            (
                "GET",
                "/api/providers/installations/01990f44-1000-7000-8000-000000000040/audit",
                "provider_installation.audit",
                Verb::Admin,
            ),
            (
                "DELETE",
                "/api/providers/targets/01990f44-1000-7000-8000-000000000040",
                "provider_target.delete",
                Verb::Admin,
            ),
            (
                "POST",
                "/api/providers/targets/01990f44-1000-7000-8000-000000000040/disable",
                "provider_target.disable",
                Verb::Admin,
            ),
            (
                "POST",
                "/api/providers/targets/01990f44-1000-7000-8000-000000000040/enable",
                "provider_target.enable",
                Verb::Admin,
            ),
            (
                "GET",
                "/api/providers/targets/01990f44-1000-7000-8000-000000000040/audit",
                "provider_target.audit",
                Verb::Admin,
            ),
            (
                "GET",
                "/api/providers/target-grants",
                "provider_target_grant.history",
                Verb::Admin,
            ),
            (
                "GET",
                "/api/providers/target-grants/01990f44-1000-7000-8000-000000000040",
                "provider_target_grant.history_get",
                Verb::Admin,
            ),
            (
                "GET",
                "/api/providers/target-grants/01990f44-1000-7000-8000-000000000040/audit",
                "provider_target_grant.history_audit",
                Verb::Admin,
            ),
        ]
    }

    /// Every route the human router mounts has a line in the table; a route
    /// that is missing would fall to the closing arm and be visible as
    /// `unclassified`.
    #[test]
    fn every_mounted_route_has_a_requirement_of_its_own() {
        for (method, path, action, verb) in mounted_routes() {
            let requirement = requirement(method, path);
            assert_eq!(requirement.action, action, "{method} {path}");
            assert_eq!(requirement.verb, verb, "{method} {path}");
        }
    }

    /// The generic operation read and the generic operation cancel are the
    /// two routes whose exact demand is in a record: the gate asks only that
    /// the caller hold some scope, marks the route so, and the handler
    /// settles the application and environment - or `admin:*/*` - from the
    /// operation. Nothing else is marked, and the mark never travels with a
    /// demand narrower than "some scope" - a route that already names its
    /// resource has nothing left to settle.
    #[test]
    fn the_generic_operation_routes_are_settled_from_the_operation_record() {
        const OPERATION: &str = "01990f44-1000-7000-8000-000000000040";
        let read = requirement("GET", &format!("/api/operations/{OPERATION}"));
        let cancel = requirement("POST", &format!("/api/operations/{OPERATION}/cancel"));
        for (requirement, action) in [(&read, "operation.get"), (&cancel, "operation.cancel")] {
            assert_eq!(requirement.action, action);
            assert_eq!(
                requirement.from_record,
                Some(RecordClaim::OperationScope),
                "{action} settles its exact demand from the operation record"
            );
            assert_eq!(requirement.verb, Verb::Read, "{action}");
            assert_eq!(requirement.app, Resource::Any, "{action}");
            assert_eq!(requirement.environment, Resource::Any, "{action}");
            assert_eq!(requirement.target.as_deref(), Some(OPERATION), "{action}");
            // Every scope answers the gate's coarse demand; a token with no
            // scope at all does not.
            let demand = Demand {
                verb: requirement.verb,
                app: requirement.app.selector(),
                environment: requirement.environment.selector(),
            };
            for held in ["deploy:demo/production", "read:other/preview", "admin:*/*"] {
                let scope = crate::identity::Scope::parse(held).unwrap();
                assert!(
                    scope.grants(&demand),
                    "{held} must reach the {action} handler's check"
                );
            }
            let parked = Principal {
                kind: crate::identity::PrincipalKind::Account,
                account: Some("parked".to_string()),
                role: Some(crate::identity::Role::Deployer),
                credential_id: Some("0011223344556677".to_string()),
                authority: crate::identity::Authority::Scoped(Vec::new()),
            };
            assert!(parked.permits(&demand).is_err(), "{action}");
        }
        assert_eq!(
            cancel.audit,
            AuditWhen::Always,
            "a cancel changes state and is recorded whatever the outcome, including the \
             handler's own 403"
        );
        assert_eq!(
            read.audit,
            AuditWhen::OnRefusal,
            "a read changes nothing and is recorded only when refused - which covers the \
             handler's own 403, because the gate records from the status the answer had"
        );

        for (method, path, _, _) in mounted_routes() {
            let requirement = requirement(method, path);
            if matches!(requirement.action, "operation.get" | "operation.cancel") {
                continue;
            }
            assert_eq!(
                requirement.from_record, None,
                "{method} {path} names what it is about in its URL or body; only the generic \
                 operation read and cancel settle their demand from a record"
            );
        }
    }

    /// The actions served under `/api/platform`, and nothing else: the whole
    /// of what an operator credential is for. Everything else the table
    /// knows is product API and is served under `/api/v1`.
    const OPERATOR_ACTIONS: &[&str] = &[
        "route.list_all",
        "session.read",
        "event.read",
        "metrics.read",
        "node.config.read",
        "node.config.write",
        "node.command.read",
        "node.command",
        "node.list",
        "node.status",
        "node.app.unload",
    ];

    /// The table above is written by hand and can only be as complete as
    /// whoever last edited it. What says it is not short: the OpenAPI
    /// document, which `openapi::tests::every_described_operation_is_mounted_under_its_prefix`
    /// holds to what the routers actually serve. Every operation in it must
    /// classify to something other than the closing arm - a route that fell
    /// through would answer to `admin:*/*` under one prefix and to its own
    /// narrower demand under another - and must classify identically under
    /// its versioned and its legacy spelling.
    #[test]
    fn every_documented_operation_is_classified_the_same_under_both_prefixes() {
        let document: serde_json::Value =
            serde_json::from_str(&crate::api::openapi::render()).unwrap();
        let unclassified = requirement("GET", "/api/nothing-of-the-kind").action;
        let mut seen = 0;
        for (path, item) in document["paths"].as_object().unwrap() {
            for method in item.as_object().unwrap().keys() {
                let method = method.to_ascii_uppercase();
                // Every path parameter takes a canonical id: no arm of the
                // table reads a segment's value, and an id is the spelling
                // the routes that do parse one accept.
                let filled = fill_path_parameters(path);
                let versioned = requirement(&method, &filled);
                assert_ne!(
                    versioned.action, unclassified,
                    "{method} {filled} is described in the OpenAPI document and the route table \
                     does not know it, so it answers to the closing arm"
                );
                let legacy =
                    filled
                        .replacen("/api/v1/", "/api/", 1)
                        .replacen("/api/platform/", "/api/", 1);
                assert_eq!(
                    requirement(&method, &legacy),
                    versioned,
                    "{method} {filled} and its legacy spelling {legacy} are classified differently"
                );
                seen += 1;
            }
        }
        assert!(seen > 100, "only {seen} documented operations were checked");
    }

    /// `/api/v1/apps/{app_name}/versions/{version_id}` with every `{...}`
    /// replaced by a canonical id, which every route that parses a segment
    /// accepts and no route in the table reads.
    fn fill_path_parameters(path: &str) -> String {
        const VALUE: &str = "01990f44-1000-7000-8000-000000000040";
        let mut out = String::with_capacity(path.len());
        let mut rest = path;
        while let Some(start) = rest.find('{') {
            let end = rest
                .find('}')
                .expect("a path template closes its parameter");
            out.push_str(&rest[..start]);
            out.push_str(VALUE);
            rest = &rest[end + 1..];
        }
        out.push_str(rest);
        out
    }

    /// A diagnostic under `/api/platform` is fleet scope, and fleet scope is
    /// the platform operator's: `docs/adr/016-api-cli-web-console.md` says an
    /// application user does not get node shells, global configuration or
    /// another tenant's telemetry by being able to deploy. So every operator
    /// read demands the whole platform, the bare node list included - its
    /// rows carry each node's management address, key status and key
    /// fingerprint, which is exactly the fleet detail the ADR withholds.
    #[test]
    fn every_platform_diagnostic_read_demands_the_whole_platform() {
        let mut checked = 0;
        for (method, path, action, _) in mounted_routes() {
            let requirement = requirement(method, path);
            if requirement.scope != RequirementScope::Operator || requirement.verb != Verb::Read {
                continue;
            }
            checked += 1;
            assert_eq!(
                (&requirement.app, &requirement.environment),
                (&Resource::Every, &Resource::Every),
                "{action} ({method} {path}) is a platform diagnostic that a narrower scope reaches"
            );
            let demand = Demand {
                verb: requirement.verb,
                app: requirement.app.selector(),
                environment: requirement.environment.selector(),
            };
            let narrow = crate::identity::Scope::parse("read:demo/preview").unwrap();
            assert!(
                !narrow.grants(&demand),
                "{action} is readable by a credential scoped to one environment"
            );
        }
        assert!(checked >= 6, "the platform reads are not being reached");
    }

    /// The prefix a legacy route is served under once it has moved.
    fn successor_prefix(requirement: &Requirement) -> &'static str {
        match requirement.scope {
            RequirementScope::Product => ApiSurface::V1.prefix(),
            RequirementScope::Operator => ApiSurface::Platform.prefix(),
        }
    }

    /// The same route under its versioned prefix gets the same requirement,
    /// field for field - action, verb, resources, target, audit rule, body
    /// claim and scope. The table is written once; this is what says a
    /// versioned path is not a second table that can drift.
    #[test]
    fn a_versioned_path_is_classified_exactly_like_its_legacy_spelling() {
        for (method, path, _, _) in mounted_routes() {
            let legacy = requirement(method, path);
            let moved = path.replacen("/api", successor_prefix(&legacy), 1);
            assert_eq!(requirement(method, &moved), legacy, "{method} {moved}");
        }

        // The one arm that reads the query string narrows the same way on
        // both spellings, and refuses to narrow the same way too.
        for query in [
            "",
            "?environment=production",
            "?environment=production&environment=preview",
            "?environment=%70roduction",
        ] {
            let legacy = requirement("DELETE", &format!("/api/runtime/n1/apps/demo{query}"));
            assert_eq!(legacy.action, "node.app.unload");
            assert_eq!(
                requirement(
                    "DELETE",
                    &format!("/api/platform/runtime/n1/apps/demo{query}")
                ),
                legacy,
                "{query:?}"
            );
        }
        assert_eq!(
            requirement(
                "DELETE",
                "/api/platform/runtime/n1/apps/demo?environment=preview"
            )
            .environment,
            Resource::Named("preview".to_string())
        );
        assert_eq!(
            requirement("DELETE", "/api/platform/runtime/n1/apps/demo").environment,
            Resource::Every
        );
    }

    /// The operator scope is exactly the platform routes: no product route
    /// carries it, and no platform route lacks it.
    #[test]
    fn the_operator_scope_is_exactly_the_platform_routes() {
        use std::collections::BTreeSet;

        let mut operator = BTreeSet::new();
        let mut product = BTreeSet::new();
        for (method, path, action, _) in mounted_routes() {
            match requirement(method, path).scope {
                RequirementScope::Operator => operator.insert(action),
                RequirementScope::Product => product.insert(action),
            };
        }
        let expected: BTreeSet<&str> = OPERATOR_ACTIONS.iter().copied().collect();
        assert_eq!(operator, expected);
        assert!(
            product.is_disjoint(&operator),
            "an action cannot be on both surfaces"
        );
        // The closing arm refuses as an operator's: what it guards is not
        // an application.
        assert_eq!(
            requirement("GET", "/api/nothing-of-the-kind").scope,
            RequirementScope::Operator
        );
    }

    /// A path that names a surface the route is not on, or that spells the
    /// surface wrongly, is not the route. It gets the closing arm - an
    /// administrator's, recorded every time - and nothing narrower.
    #[test]
    fn a_route_spelled_under_the_wrong_prefix_is_an_administrators() {
        for (method, path) in [
            // A marker folded twice, or once in the wrong place.
            ("GET", "/api/v1/v1/apps"),
            ("GET", "/api/platform/v1/apps"),
            ("GET", "/api/v1/platform/runtime"),
            ("GET", "/v1/api/apps"),
            // The wrong case, and the wrong version.
            ("GET", "/api/V1/apps"),
            ("GET", "/api/Platform/runtime"),
            ("GET", "/api/v2/apps"),
            // A product route under the platform prefix, and the reverse.
            ("GET", "/api/platform/apps"),
            ("POST", "/api/platform/apps/demo/versions"),
            ("GET", "/api/platform/accounts"),
            ("GET", "/api/v1/runtime"),
            ("DELETE", "/api/v1/runtime/n1/apps/demo?environment=preview"),
            ("GET", "/api/v1/sessions"),
            ("GET", "/api/v1/metrics/summary"),
            ("POST", "/api/v1/configs/n1"),
        ] {
            let got = requirement(method, path);
            assert_eq!(got.action, "unclassified", "{method} {path}");
            assert_eq!(got.verb, Verb::Admin, "{method} {path}");
            assert_eq!(got.app, Resource::Every, "{method} {path}");
            assert_eq!(got.environment, Resource::Every, "{method} {path}");
            assert_eq!(got.audit, AuditWhen::Always, "{method} {path}");
        }
    }

    /// What every state-changing route's **body** may name that its URL does
    /// not, and what answers for it.
    ///
    /// This is the question the hostname hijack came out of.
    /// `crate::api::authz` judges a caller against the application and
    /// environment in the *URL*; a body that names something else is a claim
    /// nothing in this table has looked at. There were two such routes and
    /// nobody had written down which. So they are written down here, all of
    /// them, and each says what closes it - a line that has to be added,
    /// deliberately, when a new write route is mounted.
    #[test]
    fn every_write_route_says_what_its_body_may_name_outside_the_url() {
        /// What a route's body can reach past its own URL.
        enum BodyReach {
            /// Nothing: the body carries values, or names something already
            /// inside the application the URL names.
            NothingTheUrlDoesNotAlreadyName,
            /// A name in a global namespace, and the gate reads it so the
            /// demand is about the thing being created.
            NamedInBodyAndDemanded(BodyClaim),
            /// A name in a global namespace that no scope can spell. The gate
            /// records it; something else authorises it, and that something
            /// is named here.
            AuthorisedElsewhere { claim: BodyClaim, by: &'static str },
            /// The catalog transaction authorizes an opaque identity in the
            /// body without asking this generic middleware to interpret it.
            AuthorisedByCatalog { by: &'static str },
            /// The route answers to `admin:*/*`, so a caller who can reach it
            /// at all already holds everything the body could name.
            AdministratorsAlready,
        }
        use BodyReach::*;

        let routes = [
            ("POST", "/api/apps", NamedInBodyAndDemanded(BodyClaim::App)),
            (
                "POST",
                "/api/apps/demo/environments",
                NamedInBodyAndDemanded(BodyClaim::Environment),
            ),
            // The two that were open. A hostname is global and no scope can
            // spell one, so there is nothing here for the demand to become;
            // `catalog::domains` is what says whether this application may
            // claim this name, and the gate's job is to get the address into
            // the audit line.
            (
                "POST",
                "/api/apps/demo/environments/production/routes",
                AuthorisedElsewhere {
                    claim: BodyClaim::RouteAddress,
                    by: "catalog::store::CatalogStore::create_route, against catalog::domains",
                },
            ),
            (
                "PUT",
                "/api/apps/demo/environments/production/routes/r1",
                AuthorisedElsewhere {
                    claim: BodyClaim::RouteAddress,
                    by: "catalog::store::CatalogStore::update_route, against catalog::domains",
                },
            ),
            // A bundle's manifest names other applications, in its service
            // bindings. That is a claim on somebody else's application and it
            // is refused by `crate::api::bindings::check_service_bindings`,
            // in the handler, against the same principal - the body is a zip
            // and cannot be read here.
            (
                "POST",
                "/api/apps/demo/versions",
                AuthorisedElsewhere {
                    claim: BodyClaim::App,
                    by: "api::bindings::check_service_bindings",
                },
            ),
            // A deploy names a version by id or number, and
            // `api::catalog::deployments::resolve_version` looks it up
            // *within* the application in the URL, so it cannot reach
            // another's.
            (
                "POST",
                "/api/apps/demo/environments/production/deployments",
                NothingTheUrlDoesNotAlreadyName,
            ),
            (
                "POST",
                "/api/apps/demo/environments/production/provider-deployments",
                AuthorisedByCatalog {
                    by: "catalog::provider_target_grant::active_provider_target_grant_tx",
                },
            ),
            (
                "POST",
                "/api/apps/demo/environments/production/rollback",
                NothingTheUrlDoesNotAlreadyName,
            ),
            // The body is a secret's value. The name is in the path, and this
            // body is never read here - see
            // `a_secret_write_names_the_secret_and_reads_no_body`.
            (
                "PUT",
                "/api/apps/demo/environments/production/secrets/TOKEN",
                NothingTheUrlDoesNotAlreadyName,
            ),
            ("DELETE", "/api/apps/demo", NothingTheUrlDoesNotAlreadyName),
            (
                "DELETE",
                "/api/apps/demo/environments/production/routes/r1",
                NothingTheUrlDoesNotAlreadyName,
            ),
            (
                "DELETE",
                "/api/apps/demo/environments/production/secrets/TOKEN",
                NothingTheUrlDoesNotAlreadyName,
            ),
            (
                "DELETE",
                "/api/runtime/node-1/apps/demo",
                NothingTheUrlDoesNotAlreadyName,
            ),
            // No body at all: the application is in the path, and the demand
            // is `admin` over that application and every environment of it.
            (
                "DELETE",
                "/api/retired-apps/demo",
                NothingTheUrlDoesNotAlreadyName,
            ),
            // Everything below names something global in its body - an
            // account, a scope list, a node id, a hostname pattern - and
            // every one of them answers to `admin:*/*`, which is authority
            // over all of it already.
            ("POST", "/api/accounts", AdministratorsAlready),
            ("PATCH", "/api/accounts/alice", AdministratorsAlready),
            ("POST", "/api/accounts/alice/tokens", AdministratorsAlready),
            ("DELETE", "/api/tokens/abcd", AdministratorsAlready),
            ("POST", "/api/configs/node-1", AdministratorsAlready),
            ("DELETE", "/api/configs/node-1/key", AdministratorsAlready),
            (
                "POST",
                "/api/commands/execute/node-1",
                AdministratorsAlready,
            ),
            ("POST", "/api/domains", AdministratorsAlready),
            ("DELETE", "/api/domains/example.com", AdministratorsAlready),
            (
                "POST",
                "/api/providers/driver-releases",
                AdministratorsAlready,
            ),
            ("POST", "/api/providers/credentials", AdministratorsAlready),
            (
                "POST",
                "/api/providers/credentials/01990f44-1000-7000-8000-000000000040/rotate",
                AdministratorsAlready,
            ),
            (
                "DELETE",
                "/api/providers/credentials/01990f44-1000-7000-8000-000000000040",
                AdministratorsAlready,
            ),
            (
                "POST",
                "/api/providers/installations",
                AdministratorsAlready,
            ),
            (
                "POST",
                "/api/providers/installations/01990f44-1000-7000-8000-000000000040/targets",
                AdministratorsAlready,
            ),
            (
                "POST",
                "/api/apps/demo/environments/production/provider-target-grants",
                AdministratorsAlready,
            ),
            (
                "DELETE",
                "/api/apps/demo/environments/production/provider-target-grants/01990f44-1000-7000-8000-000000000040",
                AdministratorsAlready,
            ),
            (
                "DELETE",
                "/api/providers/installations/01990f44-1000-7000-8000-000000000040",
                AdministratorsAlready,
            ),
            (
                "POST",
                "/api/providers/installations/01990f44-1000-7000-8000-000000000040/disable",
                AdministratorsAlready,
            ),
            (
                "POST",
                "/api/providers/installations/01990f44-1000-7000-8000-000000000040/enable",
                AdministratorsAlready,
            ),
            (
                "DELETE",
                "/api/providers/targets/01990f44-1000-7000-8000-000000000040",
                AdministratorsAlready,
            ),
            (
                "POST",
                "/api/providers/targets/01990f44-1000-7000-8000-000000000040/disable",
                AdministratorsAlready,
            ),
            (
                "POST",
                "/api/providers/targets/01990f44-1000-7000-8000-000000000040/enable",
                AdministratorsAlready,
            ),
            // No body: the application is in the path and the demand is
            // `admin` over it and every environment of it.
            (
                "POST",
                "/api/apps/demo/identity-migration",
                NothingTheUrlDoesNotAlreadyName,
            ),
            // The three that settle work already recorded. None of them
            // takes a body at all: `api::operations::cancel_provider_workflow`
            // and `api::catalog::providers::rollback_provider_deployment`
            // have no `Json` extractor, and the generic cancel reads its
            // demand from the operation record rather than from anything the
            // caller wrote.
            (
                "POST",
                "/api/apps/demo/environments/production/provider-deployments/01990f44-1000-7000-8000-000000000040/cancel",
                NothingTheUrlDoesNotAlreadyName,
            ),
            (
                "POST",
                "/api/apps/demo/environments/production/provider-deployments/01990f44-1000-7000-8000-000000000040/rollback",
                NothingTheUrlDoesNotAlreadyName,
            ),
            (
                "POST",
                "/api/operations/01990f44-1000-7000-8000-000000000040/cancel",
                NothingTheUrlDoesNotAlreadyName,
            ),
        ];

        // The table above is written by hand, and a line that is simply
        // never added is exactly the kind of omission it exists to catch.
        // So it also has to cover every write the route table knows: an
        // action recorded on every outcome is a state change, and a state
        // change whose body reach nobody wrote down here is the case this
        // test was written for.
        let covered: std::collections::BTreeSet<&'static str> = routes
            .iter()
            .map(|(method, path, _)| requirement(method, path).action)
            .collect();
        for (method, path, action, _) in mounted_routes() {
            let mounted = requirement(method, path);
            // A GET carries no body, so it has no reach past its URL to
            // write down - the credential-issuing reads are recorded on
            // every outcome for a different reason, and
            // `state_changes_and_credential_issues_are_always_recorded` is
            // what holds them to it.
            if method == "GET" || mounted.audit != AuditWhen::Always {
                continue;
            }
            assert!(
                covered.contains(action),
                "{method} {path} changes state and no line here says what its body may name \
                 outside its URL; add one"
            );
        }

        for (method, path, reach) in routes {
            let requirement = requirement(method, path);
            assert_eq!(
                requirement.audit,
                AuditWhen::Always,
                "{method} {path} changes state and must be recorded whatever the outcome"
            );
            match reach {
                NothingTheUrlDoesNotAlreadyName => assert!(
                    requirement.from_body.is_none(),
                    "{method} {path} reaches nothing past its URL, so the gate has no reason to \
                     read its body"
                ),
                NamedInBodyAndDemanded(claim) => {
                    assert_eq!(
                        requirement.from_body,
                        Some(claim),
                        "{method} {path} names what it creates in its body and the demand has to \
                         become that name"
                    );
                }
                AuthorisedElsewhere { claim, by } => {
                    assert!(
                        !by.is_empty(),
                        "{method} {path} carries a claim its URL does not cover, so something has \
                         to authorise it and this line has to say what"
                    );
                    if claim == BodyClaim::RouteAddress {
                        assert_eq!(
                            requirement.from_body,
                            Some(claim),
                            "{method} {path} claims a hostname; the gate reads it so the audit \
                             line names the address, whatever the outcome"
                        );
                    }
                }
                AuthorisedByCatalog { by } => {
                    assert!(
                        !by.is_empty(),
                        "{method} {path} carries an opaque identity that the catalog must authorize"
                    );
                    assert!(requirement.from_body.is_none(), "{method} {path}");
                }
                AdministratorsAlready => {
                    assert_eq!(
                        requirement.verb,
                        Verb::Admin,
                        "{method} {path} names something global in its body and is only safe \
                         because nothing short of an administrator can reach it"
                    );
                    assert_eq!(requirement.app, Resource::Every, "{method} {path}");
                    assert_eq!(requirement.environment, Resource::Every, "{method} {path}");
                }
            }
        }
    }

    /// The hostname registry is an administrator's, and it names an
    /// application that is deliberately not the caller's. `named(app)` here
    /// would have meant an application could grant itself the name it wanted,
    /// which is the hole the registry exists to close.
    #[test]
    fn the_hostname_registry_answers_to_nothing_short_of_an_administrator() {
        let ci = Principal {
            kind: crate::identity::PrincipalKind::Account,
            account: Some("ci".to_string()),
            role: Some(crate::identity::Role::Maintainer),
            credential_id: Some("0011223344556677".to_string()),
            authority: crate::identity::Authority::Scoped(
                crate::identity::Scope::parse_list("deploy:attacker/* secrets:attacker/*").unwrap(),
            ),
        };
        for (method, path) in [
            ("GET", "/api/domains"),
            ("POST", "/api/domains"),
            ("DELETE", "/api/domains/victim.example.com"),
        ] {
            let requirement = requirement(method, path);
            let demand = Demand {
                verb: requirement.verb,
                app: requirement.app.selector(),
                environment: requirement.environment.selector(),
            };
            assert!(
                ci.permits(&demand).is_err(),
                "{method} {path} must not be reachable by a credential scoped to one application"
            );
        }
    }

    /// The audit line of a route write names the address that was claimed.
    ///
    /// The three rows the hijack left in the log were byte-identical with
    /// `"target": null`; the one record that could have found the abuse
    /// afterwards did not contain the hostname.
    #[test]
    fn a_route_write_records_the_address_it_claimed() {
        let body = serde_json::json!({
            "host": "victim.example.com",
            "path_prefix": "/",
            "priority": 9223372036854775807i64,
        });
        let mut requirement =
            requirement("POST", "/api/apps/attacker/environments/production/routes");
        let claim = requirement.from_body.expect("a route write reads its body");
        apply_body_claim(&mut requirement, claim, Some(&body));
        assert_eq!(
            requirement.target.as_deref(),
            Some("host=victim.example.com path_prefix=/ priority=9223372036854775807"),
        );
        // The demand is untouched: no scope can spell a hostname, and
        // pretending one could would be a second authorisation rule beside
        // the grant registry that actually decides.
        assert_eq!(requirement.app, Resource::Named("attacker".to_string()));
        assert_eq!(
            requirement.environment,
            Resource::Named("production".to_string())
        );

        // A route with no host is the *widest* claim there is, so the record
        // has to say so rather than leaving the field out and reading like a
        // narrower call than it was.
        let mut catch_all = classify(
            &Method::POST,
            "/api/apps/attacker/environments/production/routes",
            None,
        );
        apply_body_claim(
            &mut catch_all,
            BodyClaim::RouteAddress,
            Some(&serde_json::json!({"path_prefix": "/"})),
        );
        assert_eq!(
            catch_all.target.as_deref(),
            Some("host=(every host) path_prefix=/ priority=0")
        );

        // An *update* names the route in the path and the address in the
        // body, and the record needs both: an address in place of the id
        // could not say which route was rewritten.
        let mut update = classify(
            &Method::PUT,
            "/api/apps/attacker/environments/production/routes/r42",
            None,
        );
        let claim = update.from_body.expect("a route update reads its body");
        apply_body_claim(
            &mut update,
            claim,
            Some(&serde_json::json!({"host": "victim.example.com", "path_prefix": "/"})),
        );
        assert_eq!(
            update.target.as_deref(),
            Some("r42 host=victim.example.com path_prefix=/ priority=0"),
        );

        // A body this gate could not read is recorded as unreadable - not as
        // "(every host)", which would positively record the widest claim
        // there is for a request that named nothing. The handler answers 400
        // to such a body; the row has to match.
        let mut unreadable = classify(
            &Method::POST,
            "/api/apps/attacker/environments/production/routes",
            None,
        );
        apply_body_claim(&mut unreadable, BodyClaim::RouteAddress, None);
        assert_eq!(
            unreadable.target.as_deref(),
            Some("(unreadable body: no address was named)"),
        );
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
                "POST",
                "/api/apps/demo/environments/production/provider-deployments",
            ),
            (
                "POST",
                "/api/apps/demo/environments/production/provider-target-grants",
            ),
            (
                "DELETE",
                "/api/apps/demo/environments/production/provider-target-grants/01990f44-1000-7000-8000-000000000040",
            ),
            ("POST", "/api/providers/driver-releases"),
            ("POST", "/api/providers/credentials"),
            (
                "POST",
                "/api/providers/credentials/01990f44-1000-7000-8000-000000000040/rotate",
            ),
            (
                "DELETE",
                "/api/providers/credentials/01990f44-1000-7000-8000-000000000040",
            ),
            ("POST", "/api/providers/installations"),
            (
                "POST",
                "/api/providers/installations/01990f44-1000-7000-8000-000000000040/targets",
            ),
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
        // And the only four routes it does read carry a name or an address,
        // never a value.
        assert_eq!(
            requirement("POST", "/api/apps").from_body,
            Some(BodyClaim::App)
        );
        assert_eq!(
            requirement("POST", "/api/apps/demo/environments").from_body,
            Some(BodyClaim::Environment)
        );
        assert_eq!(
            requirement("POST", "/api/apps/demo/environments/production/routes").from_body,
            Some(BodyClaim::RouteAddress)
        );
        assert_eq!(
            requirement("PUT", "/api/apps/demo/environments/production/routes/r1").from_body,
            Some(BodyClaim::RouteAddress)
        );
        assert!(
            requirement("POST", "/api/providers/credentials")
                .from_body
                .is_none(),
            "the authorization gate must never read provider credential material"
        );
        assert!(
            requirement("POST", "/api/providers/driver-releases")
                .from_body
                .is_none(),
            "the authorization gate must never parse signed release material"
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
            (
                "POST",
                "/api/apps/myapp/environments/preview/provider-deployments",
            ),
            (
                "GET",
                "/api/apps/myapp/environments/preview/provider-target-grants",
            ),
            ("GET", "/api/apps/myapp/versions"),
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
            // Fleet scope is the platform operator's, ADR-016 section
            // "Platform / Fleet scope": the rows name node addresses and
            // key fingerprints.
            ("GET", "/api/runtime"),
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
            (
                "POST",
                "/api/apps/myapp/environments/production/provider-deployments",
            ),
            (
                "POST",
                "/api/apps/myapp/environments/preview/provider-target-grants",
            ),
            ("GET", "/api/providers/credentials"),
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

    /// The exemption list and the table are two descriptions of the same
    /// routes, so a path in both is a disagreement with a silent winner: the
    /// gate returns first and the table entry never runs. `/api/metrics` was
    /// exactly that - `metrics.read` in the table, exempt here - and the
    /// result was a route that read as guarded and was served to any host
    /// with nothing presented.
    #[test]
    fn nothing_the_gate_stands_aside_for_is_a_route_the_table_guards() {
        for path in UNGATED_PATHS {
            assert_eq!(
                requirement("GET", path).action,
                "unclassified",
                "{path} is both exempt from the gate and described by the table; the exemption \
                 wins and the table entry never runs"
            );
        }
    }

    /// What the browser says about the page, which is the other half of "on
    /// this machine": the operator's browser is on it, and so is every page
    /// they have open.
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

    /// A router shaped like the human API: real routes behind the real gate,
    /// over a real database. Everything below asserts on what a caller gets
    /// and on what the audit log then holds, which is the only pair that
    /// matters.
    ///
    /// The routes are mounted in the legacy `/api/*` spelling on purpose, and
    /// stay so: that tree keeps answering for the whole deprecation window,
    /// and these are its regression tests. The two routes spelled under
    /// `/api/v1` and `/api/platform` are there for the one test that proves
    /// the gate reads a versioned path as the route it is.
    struct Gate {
        router: Router,
        state: AuthzState,
    }

    async fn gate() -> Gate {
        gate_with_bootstrap_file(None).await
    }

    /// The gate over a real store, with an optional bootstrap file. Most
    /// tests pass `None`: they never present a bootstrap credential, and a
    /// test that does wants to see the file itself.
    async fn gate_with_bootstrap_file(
        bootstrap_file: Option<Arc<crate::identity::bootstrap::BootstrapFile>>,
    ) -> Gate {
        let db = Arc::new(Database::init(":memory:").await.unwrap());
        let identity = Arc::new(IdentityStore::new(Arc::clone(&db)));
        let audit = Arc::new(AuditStore::new(Arc::clone(&db)));
        // The account every installation has, which the tests mint their
        // operator credential off.
        identity
            .create_account(crate::identity::store::BOOTSTRAP_ACCOUNT, Role::Owner)
            .await
            .unwrap();

        let state = AuthzState {
            identity,
            audit,
            bootstrap_file,
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
                "/api/apps/:app/environments/:env/provider-deployments",
                post(ok).get(ok),
            )
            .route(
                "/api/apps/:app/environments/:env/provider-target-grants",
                post(ok).get(ok),
            )
            .route(
                "/api/apps/:app/environments/:env/secrets/:name",
                put(created),
            )
            .route("/api/apps/:app/environments/:env/routes", post(created))
            .route("/api/apps/:app/environments/:env/storage-token", get(ok))
            .route("/api/runtime/:node/apps/:app", delete(ok))
            .route("/api/v1/apps/:app/versions", post(ok))
            .route("/api/platform/runtime/:node/apps/:app", delete(ok))
            .route("/api/audit", get(ok))
            .route("/api/events", get(ok))
            .route("/api/metrics", get(ok))
            .route("/api/metrics/summary", get(ok))
            // The request context sits where the real router puts it: on
            // the mount, inside the gate. A refusal is answered before it
            // runs; an accepted call comes back with the id it settled.
            .layer(axum::middleware::from_fn_with_state(
                ApiSurface::Legacy,
                crate::api::versioning::attach_request_context,
            ))
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
        call_marked(gate, peer, method, path, credential, body, &[]).await
    }

    /// The same call, plus whatever a browser would have stamped on it.
    #[allow(clippy::too_many_arguments)]
    async fn call_marked(
        gate: &Gate,
        peer: &str,
        method: &str,
        path: &str,
        credential: Option<&str>,
        body: &str,
        browser_marks: &[(&str, &str)],
    ) -> (StatusCode, String) {
        let mut builder = HttpRequest::builder()
            .method(method)
            .uri(path)
            // What a browser and `deis` alike send: the address they went to.
            // Present so the Origin comparison has its other half.
            .header("host", "localhost:8080")
            .header("content-type", "application/json");
        if let Some(credential) = credential {
            builder = builder.header("Authorization", format!("Bearer {credential}"));
        }
        for (name, value) in browser_marks {
            builder = builder.header(*name, *value);
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

    /// The operator credential, minted off the bootstrap account the way a
    /// first login does: unscoped, so its account's role is its only limit.
    async fn owner_credential(gate: &Gate) -> String {
        gate.state
            .identity
            .create_token("bootstrap", "operator", Authority::Unscoped)
            .await
            .unwrap()
            .1
    }

    async fn lines(gate: &Gate) -> Vec<crate::audit::AuditEntry> {
        gate.state
            .audit
            .query_page(crate::audit::AuditQuery::default(), 100, None)
            .await
            .unwrap()
            .items
    }

    /// The operator's own credential - the unscoped token a first login mints
    /// off the bootstrap account - can still do everything, including mint a
    /// storage token. It is the one credential with no scope list, and it is
    /// visible as unscoped, which is the thing an operator has to be able to
    /// see in order to retire it.
    #[tokio::test]
    async fn an_unscoped_owner_credential_can_still_do_everything() {
        let gate = gate().await;
        let secret = owner_credential(&gate).await;

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
                call(&gate, method, path, Some(&secret)).await,
                StatusCode::OK,
                "{method} {path} was refused the owner's own credential"
            );
        }

        let listed = gate
            .state
            .identity
            .list_tokens_page(
                crate::identity::store::TokenListFilter::default(),
                100,
                None,
            )
            .await
            .unwrap()
            .items;
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
        let gate = gate().await;
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
        assert_eq!(
            call(
                &gate,
                "POST",
                "/api/apps/myapp/environments/preview/provider-deployments",
                Some(&secret)
            )
            .await,
            StatusCode::OK
        );
        assert_eq!(
            call(
                &gate,
                "GET",
                "/api/apps/myapp/environments/preview/provider-target-grants",
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
            (
                "POST",
                "/api/apps/myapp/environments/production/provider-deployments",
            ),
            (
                "POST",
                "/api/apps/myapp/environments/preview/provider-target-grants",
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
        let gate = gate().await;
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
        let gate = gate().await;
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
        let gate = gate().await;
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
        let gate = gate().await;
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
        let gate = gate().await;
        let secret = owner_credential(&gate).await;

        call_with_body(
            &gate,
            "PUT",
            "/api/apps/demo/environments/production/secrets/DB_PASSWORD",
            Some(&secret),
            r#"{"value":"hunter2-do-not-log-me"}"#,
        )
        .await;
        call(
            &gate,
            "GET",
            "/api/apps/demo/environments/production/deployments",
            Some(&secret),
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
        assert_eq!(write.actor_kind, "account");

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
            .expect("an unauthenticated call has to be recorded");
        assert_eq!(refusal.action, "version.upload");
        assert_eq!(refusal.status, 401);
        assert_eq!(refusal.actor_kind, "unknown");
    }

    /// A caller with no credential is refused, and the attempt is still
    /// audited: an operator who reads the log sees who reached for the
    /// control plane and what they reached for.
    /// The audit line of a hostname claim names the hostname - on the
    /// refusal as much as on the success, because the refusal is where the
    /// attempt shows up.
    ///
    /// This is the third finding of the same defect. The three rows the
    /// hijack left in this table were byte-identical, with `"target": null`;
    /// the one record that could have found the abuse afterwards did not
    /// contain the name that was seized.
    #[tokio::test]
    async fn a_hostname_claim_is_recorded_with_the_hostname() {
        let gate = gate().await;
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
                Authority::Scoped(Scope::parse_list("deploy:attacker/production").unwrap()),
            )
            .await
            .unwrap();

        // Allowed by the gate - it is this credential's own environment -
        // and refused by the grant registry in the store, which this router
        // does not have. What matters here is the line that was written.
        call_with_body(
            &gate,
            "POST",
            "/api/apps/attacker/environments/production/routes",
            Some(&secret),
            r#"{"host":"victim.example.com","path_prefix":"/","priority":7}"#,
        )
        .await;

        // And one the gate itself refuses, for an application this
        // credential has nothing over.
        call_with_body(
            &gate,
            "POST",
            "/api/apps/victim/environments/production/routes",
            Some(&secret),
            r#"{"host":"victim.example.com","path_prefix":"/","priority":7}"#,
        )
        .await;

        let lines = lines(&gate).await;
        let claims: Vec<&crate::audit::AuditEntry> = lines
            .iter()
            .filter(|line| line.action == "route.create")
            .collect();
        assert_eq!(claims.len(), 2, "both attempts are recorded: {lines:?}");
        for line in &claims {
            assert_eq!(
                line.target.as_deref(),
                Some("host=victim.example.com path_prefix=/ priority=7"),
                "a route write with no hostname in its record is the abuse nobody can find \
                 afterwards"
            );
        }
        assert!(
            claims.iter().any(|line| line.outcome == Outcome::Refused),
            "the claim on another application's route list is a refusal and says so: {claims:?}"
        );
    }

    #[tokio::test]
    async fn an_unauthenticated_call_is_refused_and_audited() {
        let gate = gate().await;

        assert_eq!(
            call(
                &gate,
                "POST",
                "/api/apps/demo/environments/production/deployments",
                None
            )
            .await,
            StatusCode::UNAUTHORIZED,
            "a caller with no credential is refused like any other"
        );
        assert_eq!(
            call(&gate, "GET", "/api/audit", None).await,
            StatusCode::UNAUTHORIZED
        );

        // And the attempt is on the record, as the caller nobody
        // authenticated: that is the line an operator reads afterwards.
        let lines = lines(&gate).await;
        let deploy = lines.iter().find(|line| line.action == "deploy").unwrap();
        assert_eq!(deploy.actor_kind, "unknown");
        assert_eq!(deploy.actor_account, None);
        assert_eq!(deploy.app_id.as_deref(), Some("demo"));
        assert_eq!(deploy.outcome, Outcome::Refused);
        assert_eq!(deploy.status, 401);
    }

    /// The probe stays open, and it is now the only thing that is. It
    /// answers the constant "OK", so a prober learns nothing a TCP connect
    /// did not already tell it, and the orchestrator asking has no credential
    /// to present.
    #[tokio::test]
    async fn the_health_probe_is_not_behind_the_gate() {
        let gate = gate().await;
        assert_eq!(
            call_from(&gate, OFF_THE_BOX, "GET", "/health", None, "{}")
                .await
                .0,
            StatusCode::OK
        );
        assert!(lines(&gate).await.is_empty());
    }

    /// `/api/metrics` used to sit next to `/health` outside the gate, and it
    /// is not a probe: it hands out the cluster size, the node count, the
    /// uptime and the event totals. A scrape needs the credential the rest of
    /// the API needs, whichever spelling of the route is asked for.
    #[tokio::test]
    async fn a_metrics_scrape_needs_the_credential_every_other_read_needs() {
        let gate = gate().await;
        let secret = owner_credential(&gate).await;

        for path in ["/api/metrics", "/api/metrics/summary"] {
            assert_eq!(
                call(&gate, "GET", path, None).await,
                StatusCode::UNAUTHORIZED,
                "{path} was served with no credential"
            );
            assert_eq!(
                call(&gate, "GET", path, Some(&secret)).await,
                StatusCode::OK,
                "{path} refused a valid credential"
            );
            assert_eq!(
                call_from(&gate, OFF_THE_BOX, "GET", path, None, "{}")
                    .await
                    .0,
                StatusCode::UNAUTHORIZED,
                "{path} was served to a host off this machine with nothing presented"
            );
        }
    }

    /// The scrape a real Prometheus off the box does: a token minted for it,
    /// scoped to reading, and nothing more than reading.
    #[tokio::test]
    async fn a_read_scoped_token_scrapes_from_another_host_and_can_do_nothing_else() {
        let gate = gate().await;
        gate.state
            .identity
            .create_account("prometheus", Role::Viewer)
            .await
            .unwrap();
        let (_, secret) = gate
            .state
            .identity
            .create_token(
                "prometheus",
                "scrape",
                Authority::Scoped(Scope::parse_list("read:*/*").unwrap()),
            )
            .await
            .unwrap();

        for path in ["/api/metrics", "/api/metrics/summary"] {
            assert_eq!(
                call_from(&gate, OFF_THE_BOX, "GET", path, Some(&secret), "{}")
                    .await
                    .0,
                StatusCode::OK,
                "a scrape credential could not read {path} from another host"
            );
        }

        // The same credential is a viewer and nothing else: the audit log,
        // which names every application and everybody who touched one, is not
        // part of scraping.
        assert_eq!(
            call_from(&gate, OFF_THE_BOX, "GET", "/api/audit", Some(&secret), "{}")
                .await
                .0,
            StatusCode::FORBIDDEN
        );
    }

    /// Unloading an application from a node is a real outage: the node stops
    /// serving it, and it stays stopped until the next reconcile - which the
    /// same call can undo again. A credential that may deploy to one preview
    /// environment must not be able to make it.
    #[tokio::test]
    async fn a_credential_for_one_environment_cannot_unload_the_application_from_a_node() {
        let gate = gate().await;
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

    /// A browser on the operator's box is on this machine's network, but
    /// being a browser changes nothing: every page it has open is refused
    /// exactly like a caller off the box. What a console needs is a
    /// credential, and the CORS configuration to let a foreign origin read
    /// the answers at all.
    #[tokio::test]
    async fn a_page_on_another_origin_gets_nothing_without_a_credential() {
        let gate = gate().await;

        // Every way a browser says "this came from somewhere else", and the
        // two calls that end the game: mint a credential, read the log.
        for marks in [
            vec![("origin", "https://evil.example")],
            vec![("sec-fetch-site", "cross-site")],
            vec![("sec-fetch-site", "same-site")],
            vec![
                ("origin", "https://evil.example"),
                ("sec-fetch-site", "cross-site"),
            ],
        ] {
            for (method, path) in [
                ("POST", "/api/accounts"),
                ("GET", "/api/audit"),
                ("POST", "/api/apps/demo/environments/production/deployments"),
            ] {
                let (status, body) =
                    call_marked(&gate, ON_THE_BOX, method, path, None, "{}", &marks).await;
                assert_eq!(
                    status,
                    StatusCode::UNAUTHORIZED,
                    "{method} {path} was served to a page marked {marks:?}"
                );
                assert!(
                    body.contains("A valid credential is required"),
                    "the refusal has to say what is missing: {body}"
                );
            }
        }

        // No marking admits anything: the console this port serves holds a
        // credential, and `deis` sends none of these headers anyway.
        for marks in [
            vec![("sec-fetch-site", "same-origin")],
            vec![("sec-fetch-site", "none")],
            vec![("origin", "http://localhost:8080")],
            vec![],
        ] {
            let (status, _) =
                call_marked(&gate, ON_THE_BOX, "GET", "/api/audit", None, "{}", &marks).await;
            assert_eq!(
                status,
                StatusCode::UNAUTHORIZED,
                "a call with no credential was admitted, marked {marks:?}"
            );
        }

        // Every refusal is on the record, and named as the caller nobody
        // authenticated rather than invented into an account.
        let refused: Vec<_> = lines(&gate)
            .await
            .into_iter()
            .filter(|line| line.outcome == Outcome::Refused)
            .collect();
        assert_eq!(refused.len(), 16, "{refused:?}");
        assert!(refused.iter().all(|line| line.actor_kind == "unknown"));
    }

    /// A reverse proxy connects from loopback on behalf of callers that may be
    /// anywhere. The gate reads no proxy header for admission: a call with no
    /// credential is refused whatever the forwarding marks say, and a real
    /// credential resolves the same way it does without them.
    #[tokio::test]
    async fn a_proxied_request_gets_the_same_answer_as_a_direct_one() {
        let gate = gate().await;

        for marks in [
            vec![("forwarded", "for=203.0.113.7;proto=https")],
            vec![("x-forwarded-for", "203.0.113.7")],
            vec![("x-real-ip", "203.0.113.7")],
            vec![("x-forwarded-proto", "https")],
            vec![("via", "1.1 reverse-proxy")],
        ] {
            let (status, _) = call_marked(
                &gate,
                ON_THE_BOX,
                "POST",
                "/api/apps/demo/environments/production/deployments",
                None,
                "{}",
                &marks,
            )
            .await;
            assert_eq!(status, StatusCode::UNAUTHORIZED, "marks: {marks:?}");
        }

        // The proxy does not make the API unusable: a real credential still
        // resolves exactly as it would on a direct call.
        let secret = owner_credential(&gate).await;
        let (status, _) = call_marked(
            &gate,
            ON_THE_BOX,
            "GET",
            "/api/audit",
            Some(&secret),
            "{}",
            &[("x-forwarded-for", "203.0.113.7")],
        )
        .await;
        assert_eq!(status, StatusCode::OK);
    }

    /// A page on another origin holding a credential is a console the
    /// operator set up, and it works: the gate is about who is calling, not
    /// from what page.
    #[tokio::test]
    async fn a_credential_still_works_from_a_page_on_another_origin() {
        let gate = gate().await;
        let secret = owner_credential(&gate).await;
        let (status, _) = call_marked(
            &gate,
            ON_THE_BOX,
            "GET",
            "/api/audit",
            Some(&secret),
            "{}",
            &[("origin", "https://console.example.com")],
        )
        .await;
        assert_eq!(status, StatusCode::OK);
    }

    /// A reverse proxy connects from loopback on behalf of callers that may be
    /// anywhere. The gate reads no forwarding header, so a proxied call gets
    /// exactly the answer a direct one would: refused with no credential,
    /// served with one.
    #[tokio::test]
    async fn a_proxied_request_cannot_use_the_anonymous_loopback_exception() {
        let open = gate().await;

        for marks in [
            vec![("forwarded", "for=203.0.113.7;proto=https")],
            vec![("x-forwarded-for", "203.0.113.7")],
            vec![("x-real-ip", "203.0.113.7")],
            vec![("x-forwarded-proto", "https")],
            vec![("via", "1.1 reverse-proxy")],
        ] {
            let (status, body) = call_marked(
                &open,
                ON_THE_BOX,
                "POST",
                "/api/apps/demo/environments/production/deployments",
                None,
                "{}",
                &marks,
            )
            .await;
            assert_eq!(status, StatusCode::UNAUTHORIZED, "marks: {marks:?}");
            assert!(
                body.contains("A valid credential is required"),
                "the refusal has to say what is missing: {body}"
            );
        }

        // The proxy does not make the API unusable: a real credential still
        // resolves exactly as it would on a direct call.
        let with_key = gate().await;
        let secret = owner_credential(&with_key).await;
        let (status, _) = call_marked(
            &with_key,
            ON_THE_BOX,
            "GET",
            "/api/audit",
            Some(&secret),
            "{}",
            &[("x-forwarded-for", "203.0.113.7")],
        )
        .await;
        assert_eq!(status, StatusCode::OK);
    }

    /// Being on this machine's network is not an admission either: the
    /// refusal is the same from the box as from anywhere else, and the
    /// storage-token mint - which hands back a credential that never expires
    /// - is behind the same gate as everything else. The refusals are in the
    /// log with the address, which is how an operator sees that something was
    /// reaching for this control plane.
    #[tokio::test]
    async fn a_caller_on_this_machine_needs_a_credential_like_any_other() {
        let gate = gate().await;

        // On the box, with nothing to present: refused like anything else.
        assert_eq!(
            call(
                &gate,
                "POST",
                "/api/apps/demo/environments/production/deployments",
                None
            )
            .await,
            StatusCode::UNAUTHORIZED
        );

        // From anywhere else: nothing at all, including the two calls that
        // hurt most - the audit log, which names every application and
        // everybody who touched one, and the storage-token mint.
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
                body.contains("A valid credential is required"),
                "the refusal has to say what is missing: {body}"
            );
        }

        // And the refusals are in the log, with the address, which is how an
        // operator sees that something off the box was reaching for this.
        let refusals: Vec<_> = lines(&gate)
            .await
            .into_iter()
            .filter(|line| line.outcome == Outcome::Refused)
            .collect();
        assert_eq!(refusals.len(), 4, "{refusals:?}");
        assert!(refusals.iter().all(|line| line.actor_kind == "unknown"));
    }

    /// A credential that is presented is held to its scopes, whoever holds
    /// it: the scopes on it were written down as its limit, and a control
    /// plane that ignores them hands a preview pipeline the storage
    /// credential of any tenant it names.
    #[tokio::test]
    async fn a_credential_is_held_to_its_scopes() {
        let gate = gate().await;
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
    /// A versioned path is the legacy route with its surface named: the
    /// credential that may call one may call the other, the one that may
    /// not still may not, and the audit line records the path as it was
    /// spelled, so an operator can see which surface a client is on.
    #[tokio::test]
    async fn the_gate_reads_a_versioned_path_as_the_route_it_is() {
        let gate = gate().await;
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
            call(&gate, "POST", "/api/v1/apps/myapp/versions", Some(&secret)).await,
            StatusCode::OK,
            "the upload this credential may make on /api is the same upload on /api/v1"
        );
        assert_eq!(
            call(
                &gate,
                "DELETE",
                "/api/platform/runtime/n1/apps/myapp",
                Some(&secret)
            )
            .await,
            StatusCode::FORBIDDEN,
            "unloading every environment needs a wildcard on /api/platform exactly as on /api"
        );
        assert_eq!(
            call(
                &gate,
                "DELETE",
                "/api/platform/runtime/n1/apps/myapp?environment=preview",
                Some(&secret)
            )
            .await,
            StatusCode::OK,
            "and the query narrows it to the one environment the credential holds"
        );

        let lines = lines(&gate).await;
        let upload: Vec<_> = lines
            .iter()
            .filter(|line| line.path == "/api/v1/apps/myapp/versions")
            .collect();
        assert_eq!(upload.len(), 1, "{upload:?}");
        assert_eq!(upload[0].action, "version.upload");
        assert_eq!(upload[0].outcome, crate::audit::Outcome::Accepted);
        // The two unloads record the same path - the query is not part of
        // it - and are told apart by what the gate made of the query.
        let unloads: Vec<_> = lines
            .iter()
            .filter(|line| line.path == "/api/platform/runtime/n1/apps/myapp")
            .collect();
        assert_eq!(unloads.len(), 2, "{unloads:?}");
        for line in &unloads {
            assert_eq!(line.action, "node.app.unload");
            assert_eq!(line.app_id.as_deref(), Some("myapp"));
            match line.environment.as_deref() {
                None => assert_eq!(line.outcome, crate::audit::Outcome::Refused),
                Some("preview") => assert_eq!(line.outcome, crate::audit::Outcome::Accepted),
                other => panic!("unexpected environment {other:?}"),
            }
        }
    }

    /// The id in the audit line is the id the caller holds. An accepted call
    /// records what the answer echoed - the caller's own id when it sent a
    /// usable one, the minted one otherwise. A refusal is answered before
    /// the mount's request context runs, so it records the caller's own id
    /// when there was a usable one and nothing when there was not; garbage
    /// in the header is never written down.
    #[tokio::test]
    async fn an_audit_line_carries_the_request_id_the_caller_holds() {
        let gate = gate().await;
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

        async fn send(
            gate: &Gate,
            method: &str,
            path: &str,
            secret: Option<&str>,
            request_id: Option<&str>,
        ) -> (StatusCode, Option<String>) {
            let mut builder = HttpRequest::builder()
                .method(method)
                .uri(path)
                .header("host", "localhost:8080")
                .header("content-type", "application/json");
            if let Some(secret) = secret {
                builder = builder.header("Authorization", format!("Bearer {secret}"));
            }
            if let Some(request_id) = request_id {
                builder = builder.header(REQUEST_ID_HEADER, request_id);
            }
            let mut request = builder.body(Body::from("{}")).unwrap();
            request.extensions_mut().insert(ConnectInfo(
                ON_THE_BOX.parse::<SocketAddr>().expect("a peer address"),
            ));
            let response = gate.router.clone().oneshot(request).await.unwrap();
            let echoed = response
                .headers()
                .get(REQUEST_ID_HEADER)
                .map(|value| value.to_str().unwrap().to_string());
            (response.status(), echoed)
        }

        let sent = CanonicalId::new_v7().to_string();
        let (status, echoed) = send(
            &gate,
            "POST",
            "/api/apps/myapp/versions",
            Some(&secret),
            Some(&sent),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(echoed.as_deref(), Some(sent.as_str()));

        let (status, minted) = send(
            &gate,
            "POST",
            "/api/apps/myapp/versions",
            Some(&secret),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let minted = minted.expect("an accepted call always echoes an id");
        minted
            .parse::<CanonicalId>()
            .expect("the minted id is canonical");

        // Refused before the request context exists: the gate settles the
        // id itself - the caller's own when it is one, a minted one when it
        // is not - and answers and audits under that one id.
        let refused_with_id = CanonicalId::new_v7().to_string();
        let (status, echoed) = send(
            &gate,
            "POST",
            "/api/apps/other/versions",
            Some(&secret),
            Some(&refused_with_id),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(echoed.as_deref(), Some(refused_with_id.as_str()));
        let (status, replaced) = send(
            &gate,
            "POST",
            "/api/apps/other/versions",
            Some(&secret),
            Some("<script>not-an-id</script>"),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        let replaced = replaced.expect("a refusal echoes the id it was audited under");
        replaced
            .parse::<CanonicalId>()
            .expect("garbage is replaced by a minted id");
        let unauthenticated_id = CanonicalId::new_v7().to_string();
        let (status, echoed) = send(
            &gate,
            "POST",
            "/api/apps/myapp/versions",
            Some("not-a-token"),
            Some(&unauthenticated_id),
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(echoed.as_deref(), Some(unauthenticated_id.as_str()));

        let lines = lines(&gate).await;
        let by_status: Vec<(i64, Option<&str>)> = lines
            .iter()
            .rev()
            .map(|line| (line.status, line.request_id.as_deref()))
            .collect();
        assert_eq!(
            by_status,
            vec![
                (200, Some(sent.as_str())),
                (200, Some(minted.as_str())),
                (403, Some(refused_with_id.as_str())),
                (403, Some(replaced.as_str())),
                (401, Some(unauthenticated_id.as_str())),
            ],
            "{lines:?}"
        );
        let dump = serde_json::to_string(&lines).unwrap();
        assert!(
            !dump.contains("<script>"),
            "garbage in x-request-id must never reach the audit log"
        );
    }

    /// The gate answers in the shape of the prefix it was called through:
    /// the two-key legacy body under `/api`, byte for byte as before, and
    /// the structured envelope with the request id under `/api/v1` and
    /// `/api/platform`.
    #[tokio::test]
    async fn a_refusal_takes_the_shape_of_the_prefix_it_was_called_through() {
        let gate = gate().await;
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

        let (status, body) =
            call_with_body(&gate, "POST", "/api/apps/other/versions", None, "{}").await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert!(
            body.starts_with(r#"{"error":"Unauthorized","details":"#),
            "{body}"
        );
        let (status, body) = call_with_body(
            &gate,
            "POST",
            "/api/apps/other/versions",
            Some(&secret),
            "{}",
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert!(
            body.starts_with(r#"{"error":"This credential may not do that: "#),
            "{body}"
        );

        let (status, body) =
            call_with_body(&gate, "POST", "/api/v1/apps/other/versions", None, "{}").await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        let body: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(body["error"]["code"], "unauthenticated");
        assert!(body["error"]["requestId"].is_string(), "{body}");
        assert_eq!(body["error"]["retryable"], false);

        let (status, body) = call_with_body(
            &gate,
            "POST",
            "/api/v1/apps/other/versions",
            Some(&secret),
            "{}",
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        let body: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(body["error"]["code"], "insufficient-scope");
        assert!(
            body["error"]["message"]
                .as_str()
                .unwrap()
                .contains("deploy:other/"),
            "{body}"
        );

        let (status, body) = call_with_body(
            &gate,
            "DELETE",
            "/api/platform/runtime/n1/apps/other",
            Some(&secret),
            "{}",
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        let body: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(body["error"]["code"], "insufficient-scope");
    }

    #[tokio::test]
    async fn a_padded_name_in_the_body_is_judged_as_the_name_that_gets_created() {
        let gate = gate().await;
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
