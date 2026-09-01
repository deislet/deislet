//! The `bindings` block of a manifest: the namespaces and the applications an
//! application reaches by name.
//!
//! Until this existed, an `(application, environment)` had exactly one KV, one
//! database, one object store and one default queue, and no way to say
//! anything else. Separating hot data from cold, giving one application two
//! buckets, or calling another application on this platform could not be
//! written down at all - the second of those was reachable only by leaving the
//! platform through `fetch()` and coming back in, which the outbound policy
//! refuses by default.
//!
//! A binding is a **name an application chooses** for something it reaches:
//!
//! ```json
//! "bindings": {
//!   "HOT":  { "kind": "kv" },
//!   "COLD": { "kind": "kv" },
//!   "DOCS": { "kind": "blob" },
//!   "JOBS": { "kind": "queue" },
//!   "AUTH": { "kind": "service", "app": "auth-api" }
//! }
//! ```
//!
//! # Where this shape lives, and why here
//!
//! Beside the manifest it is a field of, for the reason
//! [`crate::limits::DeclaredLimits`] and [`crate::compatibility`] live here:
//! four components have to agree exactly on what it means. The build writes it
//! out of the project file and refuses one it cannot honour with the author's
//! source still on screen; the control plane refuses one it cannot honour at
//! upload and checks who is entitled to name another application; the node
//! turns each entry into a client or a call route when it installs the
//! version; and the isolate publishes them on `context.bindings` and on `env`.
//! Four copies of these rules would drift, and the symptom - an application
//! writing into a namespace it did not name, or calling an application that
//! never agreed to be called - is invisible until somebody's data is in the
//! wrong place.
//!
//! # One flat table of names, not one per kind
//!
//! A name means exactly one thing in an application's code. `env.HOT` is one
//! object whatever its kind, so a reader never has to know a binding's kind to
//! know which object a line of code touches, and two bindings can never differ
//! only by a kind the call site does not mention. The cost is that `HOT`
//! cannot be both a KV and a bucket in one application, which is a sentence an
//! author can act on; the alternative cost - `services.kv.HOT` and
//! `services.blob.HOT` being different things - is a bug an author cannot see.
//!
//! Names are compared **ignoring ASCII case** when they are checked for
//! uniqueness, and kept as written everywhere else. A binding name becomes a
//! directory and a database file name in deis-store, and `HOT` and `hot` are
//! one directory on a case-insensitive filesystem: two namespaces that are one
//! namespace on somebody's laptop is exactly the collision this module exists
//! to make impossible.
//!
//! # What a storage binding is, on disk
//!
//! Not a second tenant. The tenant deis-store enforces is still
//! `(application, environment)` and nothing here widens it: the binding name
//! is composed *onto* the tenant by
//! [`deis_wire::conventions::namespace_id`], after the capability token for
//! that tenant has already been presented. The separator is a character
//! neither a tenant part nor a binding name may contain, so the composition is
//! injective - no pair of (tenant, binding) spells another pair's namespace,
//! and none spells a bare tenant. The worst a forged binding name can reach is
//! another namespace of the caller's own data.
//!
//! A **queue** binding is the exception, and it needs no composition at all:
//! deis-queue has always keyed messages by `(tenant, queue, id)`, so a name
//! inside a tenant is what a queue already is. A queue binding names the
//! queue, which is also the name a consumer declares in `queues`.
//!
//! # What a service binding is
//!
//! A call to another application on this platform, dispatched inside the node
//! rather than over the network. It carries **no address at all** - no host,
//! no port, no URL - so it is not, and cannot become, a way past
//! `FetchPolicy`: the thing it names is an `(application, environment)` pair
//! that this node either runs or does not.
//!
//! Which environment it means is [`ServiceEnvironment`], and the default is
//! the caller's own. See that type for why.
//!
//! Being named is not being reachable. The callee decides, in its own bundle,
//! through [`Manifest::callable_by`](crate::Manifest::callable_by); the node
//! checks the callee's own current version on every call, so consent moves
//! with the callee's code and never has to be redelivered to the caller.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// Longest binding name. The same ceiling as a tenant part, because a binding
/// name is composed onto a tenant and the result becomes a file name.
pub const MAX_BINDING_NAME_LEN: usize = 64;

/// Names an application may not use for a binding.
///
/// `ASSETS` is the binding the platform installs on `env` for the version's
/// own static files; a binding of that name would be a second thing under one
/// name, with whichever was installed last deciding what `env.ASSETS.fetch()`
/// reaches. The `DEIS_` prefix is reserved for the same reason it is reserved
/// among environment variables - those keys are platform wiring, and a binding
/// spelled like one would be published beside them on `env`.
const RESERVED_BINDING_NAME: &str = "ASSETS";
const RESERVED_BINDING_PREFIX: &str = "DEIS_";

/// What one binding reaches.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BindingKind {
    /// A key/value namespace inside this tenant.
    Kv,
    /// A SQL database inside this tenant: its own file, its own schema.
    Database,
    /// An object-storage namespace inside this tenant.
    Blob,
    /// A queue of this tenant, by name.
    Queue,
    /// Another application on this platform.
    Service,
}

impl BindingKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Kv => "kv",
            Self::Database => "database",
            Self::Blob => "blob",
            Self::Queue => "queue",
            Self::Service => "service",
        }
    }

    /// Whether this kind is a namespace inside the tenant's own storage, as
    /// opposed to a route to another application.
    pub fn is_storage(self) -> bool {
        !matches!(self, Self::Service)
    }

    /// Whether reaching this kind needs deis-store, which is what decides
    /// whether a client is built for it. Queue is served by deis-queue and
    /// service by nothing at all.
    pub fn needs_store(self) -> bool {
        matches!(self, Self::Kv | Self::Database | Self::Blob)
    }

    /// Every kind, so an error message can list them without a second copy of
    /// this set going stale.
    pub const ALL: [BindingKind; 5] = [
        Self::Kv,
        Self::Database,
        Self::Blob,
        Self::Queue,
        Self::Service,
    ];
}

/// Which environment of the callee a service binding means.
///
/// The default is [`Self::Same`] - the caller's own - and that is the whole
/// point of the type. `(application, environment)` is the tenant: a preview of
/// one application calling the *production* copy of another would let preview
/// traffic read and write production data through the callee, which is a
/// data-crossing bug that nothing downstream can see, because by then the work
/// is being done by an application that is legitimately in production.
///
/// So a preview stack is self-contained by default: `pr-42` of the caller
/// reaches `pr-42` of the callee. When that environment of the callee is not
/// deployed, the call fails and says so, which is a loud failure at the moment
/// of the call rather than a silent one in somebody's production database.
/// An author who genuinely wants the production copy writes the environment
/// down, and it is then visible in the manifest and in the deploy report.
///
/// There is deliberately no spelling for "same" in the manifest: leaving the
/// field out *is* that spelling. A reserved word would collide with an
/// environment somebody had actually named, and the collision would be
/// discovered by a call landing in the wrong one.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ServiceEnvironment {
    /// The environment the caller is itself running in.
    Same,
    /// One environment of the callee, by name.
    Named(String),
}

impl ServiceEnvironment {
    /// Which environment this resolves to for a caller running in
    /// `caller_environment`.
    pub fn resolve<'a>(&'a self, caller_environment: &'a str) -> &'a str {
        match self {
            Self::Same => caller_environment,
            Self::Named(name) => name,
        }
    }

    /// How it reads in a log line or a deploy report.
    pub fn describe(&self) -> String {
        match self {
            Self::Same => "the caller's own environment".to_string(),
            Self::Named(name) => format!("environment {name}"),
        }
    }
}

/// One entry of the `bindings` map.
///
/// A key this build does not know is ignored and named at `warn`, the same as
/// in [`crate::limits::DeclaredLimits`] and for the same reason - see
/// [`crate::forward`]. What is *not* ignored is anything ambiguous about the
/// keys this build does read: [`DeclaredBindings::validate`] still refuses a
/// name that could be two namespaces, or a service binding with nothing to
/// call, because those are two meanings in one place rather than one meaning
/// this build has not learned yet.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct Binding {
    pub kind: BindingKind,

    /// The application a `service` binding calls. Meaningless, and refused, on
    /// every other kind.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app: Option<String>,

    /// Which environment of that application. Absent is
    /// [`ServiceEnvironment::Same`]; see that type for why that is the
    /// default. Meaningless, and refused, on every other kind.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub environment: Option<String>,
}

impl Binding {
    /// A storage namespace of this tenant.
    pub fn storage(kind: BindingKind) -> Self {
        Self {
            kind,
            app: None,
            environment: None,
        }
    }

    /// A call to another application, in the caller's own environment.
    pub fn service(app: impl Into<String>) -> Self {
        Self {
            kind: BindingKind::Service,
            app: Some(app.into()),
            environment: None,
        }
    }

    /// Which environment of the callee this binding means. Only meaningful for
    /// [`BindingKind::Service`].
    pub fn service_environment(&self) -> ServiceEnvironment {
        match self.environment.as_deref().map(str::trim) {
            None | Some("") => ServiceEnvironment::Same,
            Some(name) => ServiceEnvironment::Named(name.to_string()),
        }
    }

    /// How this binding reads in a log line or a deploy report.
    pub fn describe(&self) -> String {
        match self.kind {
            BindingKind::Service => format!(
                "calls {} in {}",
                self.app.as_deref().unwrap_or("<no application>"),
                self.service_environment().describe()
            ),
            kind => format!("a {} namespace of this tenant", kind.as_str()),
        }
    }
}

/// The `bindings` object of a bundle manifest.
///
/// A `BTreeMap` so the order is the author's names sorted, which is what makes
/// a manifest byte-identical across builds and a log line stable.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(transparent)]
pub struct DeclaredBindings(BTreeMap<String, Binding>);

impl DeclaredBindings {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn get(&self, name: &str) -> Option<&Binding> {
        self.0.get(name)
    }

    pub fn iter(&self) -> impl Iterator<Item = (&str, &Binding)> {
        self.0
            .iter()
            .map(|(name, binding)| (name.as_str(), binding))
    }

    /// The names of one kind, in the order they appear in the manifest.
    pub fn names_of(&self, kind: BindingKind) -> Vec<&str> {
        self.0
            .iter()
            .filter(|(_, binding)| binding.kind == kind)
            .map(|(name, _)| name.as_str())
            .collect()
    }

    /// Every kind this application declares a binding of.
    pub fn kinds(&self) -> Vec<BindingKind> {
        let mut kinds: Vec<BindingKind> = self.0.values().map(|binding| binding.kind).collect();
        kinds.sort_unstable();
        kinds.dedup();
        kinds
    }

    /// Add one, for the builders and the tests. Returns the previous entry of
    /// that exact name, if there was one.
    pub fn insert(&mut self, name: impl Into<String>, binding: Binding) -> Option<Binding> {
        self.0.insert(name.into(), binding)
    }

    /// A one-line rendering for an operator's log.
    pub fn describe(&self) -> String {
        if self.0.is_empty() {
            return "nothing".to_string();
        }
        self.0
            .iter()
            .map(|(name, binding)| format!("{name} ({})", binding.describe()))
            .collect::<Vec<_>>()
            .join(", ")
    }

    /// Refuse a declaration no node could act on.
    ///
    /// Unlike [`crate::limits::DeclaredLimits::validate`], which only refuses
    /// what is *meaningless*, this refuses what is **ambiguous** as well: two
    /// names that are one namespace on a case-insensitive filesystem, a name
    /// that would be composed into something other than the namespace it
    /// spells, a service binding with nothing to call. Ambiguity here is not a
    /// greedy request a node can clamp - it is two pieces of data in one
    /// place, and there is no later moment at which it can be noticed.
    ///
    /// `app` is the application the manifest belongs to, so that a binding
    /// naming its own application and environment - which is a call into
    /// itself, and a guaranteed cycle - is refused where it is written rather
    /// than on the first request that takes it.
    pub fn validate(&self, app: &str) -> Result<(), String> {
        let mut lowercased: BTreeMap<String, &str> = BTreeMap::new();

        for (name, binding) in &self.0 {
            if !deis_wire::conventions::is_valid_binding_name(name) {
                return Err(format!(
                    "binding name {name:?} is not usable: it must be 1 to {MAX_BINDING_NAME_LEN} \
                     characters of ASCII letters, digits, '_' or '-'. The name becomes part of a \
                     directory and a database file name in the storage service, so anything else \
                     could address a namespace outside this application's own"
                ));
            }
            if name.eq_ignore_ascii_case(RESERVED_BINDING_NAME) {
                return Err(format!(
                    "binding name {name:?} is reserved: {RESERVED_BINDING_NAME} is the platform's \
                     own binding for this version's static files, and a second thing under that \
                     name would leave whichever was installed last deciding what \
                     env.{RESERVED_BINDING_NAME}.fetch() reaches"
                ));
            }
            if name
                .to_ascii_uppercase()
                .starts_with(RESERVED_BINDING_PREFIX)
            {
                return Err(format!(
                    "binding name {name:?} is reserved: names beginning with \
                     {RESERVED_BINDING_PREFIX} are platform wiring, and a binding is published on \
                     the environment table beside them"
                ));
            }

            let folded = name.to_ascii_lowercase();
            if let Some(first) = lowercased.insert(folded, name.as_str()) {
                return Err(format!(
                    "bindings {first:?} and {name:?} differ only in case. A binding name becomes a \
                     directory and a file name in the storage service, where those are one name on \
                     a case-insensitive filesystem, so the two would silently share a namespace on \
                     some machines and not on others"
                ));
            }

            match binding.kind {
                BindingKind::Service => {
                    let target = binding.app.as_deref().map(str::trim).unwrap_or_default();
                    if target.is_empty() {
                        return Err(format!(
                            "binding {name:?} is a service binding but names no application to \
                             call; add \"app\""
                        ));
                    }
                    if !deis_wire::conventions::is_valid_tenant_part(target) {
                        return Err(format!(
                            "binding {name:?} calls {target:?}, which is not an application id: \
                             ids are 1 to 64 characters of ASCII letters, digits, '_' or '-'"
                        ));
                    }
                    if let ServiceEnvironment::Named(environment) = binding.service_environment() {
                        if !deis_wire::conventions::is_valid_tenant_part(&environment) {
                            return Err(format!(
                                "binding {name:?} calls {target:?} in {environment:?}, which is \
                                 not an environment name: names are 1 to 64 characters of ASCII \
                                 letters, digits, '_' or '-'. Leave \"environment\" out to call \
                                 the caller's own environment"
                            ));
                        }
                    } else if target == app {
                        return Err(format!(
                            "binding {name:?} calls {target:?} in the caller's own environment, \
                             which is this application calling itself: every such call would be \
                             refused at the first hop as a cycle. Call a handler directly, or name \
                             another environment explicitly"
                        ));
                    }
                }
                kind => {
                    if binding.app.is_some() {
                        return Err(format!(
                            "binding {name:?} is a {} binding and carries \"app\", which only a \
                             service binding has. A storage binding is a namespace inside this \
                             application's own tenant and can never name another application",
                            kind.as_str()
                        ));
                    }
                    if binding.environment.is_some() {
                        return Err(format!(
                            "binding {name:?} is a {} binding and carries \"environment\", which \
                             only a service binding has. A storage binding is always in the \
                             environment the application is running in",
                            kind.as_str()
                        ));
                    }
                }
            }
        }
        Ok(())
    }
}

impl<'a> IntoIterator for &'a DeclaredBindings {
    type Item = (&'a str, &'a Binding);
    type IntoIter = Box<dyn Iterator<Item = (&'a str, &'a Binding)> + 'a>;

    fn into_iter(self) -> Self::IntoIter {
        Box::new(self.iter())
    }
}

/// Who a bundle agrees to be called by, as it appears in the manifest.
///
/// The wildcard `*` is a real answer and not a hole: it is consent, written
/// down by the application that is consenting, and a node says so out loud
/// when it loads a version carrying it.
pub const CALLABLE_BY_EVERYONE: &str = "*";

/// Refuse a `callable_by` list a node could not act on.
///
/// Every entry must be an application id or the wildcard. A malformed entry is
/// refused rather than skipped: an entry that was silently dropped would leave
/// an author looking at a list that says a caller is allowed while the node
/// refuses it.
pub fn validate_callable_by(callable_by: &[String]) -> Result<(), String> {
    for entry in callable_by {
        let entry = entry.trim();
        if entry == CALLABLE_BY_EVERYONE {
            continue;
        }
        if !deis_wire::conventions::is_valid_tenant_part(entry) {
            return Err(format!(
                "callable_by entry {entry:?} is not an application id: ids are 1 to 64 characters \
                 of ASCII letters, digits, '_' or '-', and {CALLABLE_BY_EVERYONE:?} means every \
                 application on this platform"
            ));
        }
    }
    Ok(())
}

/// Whether `caller` is on a `callable_by` list.
///
/// The single reader of that list, called by the node on every service call
/// with the **callee's own current manifest**, so consent moves with the
/// callee's code: an application that stops consenting stops being callable at
/// its next deployment, with nothing to redeliver to anybody who named it.
pub fn callable_by_allows(callable_by: &[String], caller_app: &str) -> bool {
    callable_by
        .iter()
        .map(|entry| entry.trim())
        .any(|entry| entry == CALLABLE_BY_EVERYONE || entry == caller_app)
}

#[cfg(test)]
#[path = "bindings_test.rs"]
mod bindings_test;
