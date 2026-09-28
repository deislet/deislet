//! Which storage services an app gets, and where they live.
//!
//! Until wave B nothing in this crate ever built a [`KvClient`], [`DbClient`],
//! [`BlobClient`] or [`QueueClient`]: the clients existed and were tested, but
//! no code path constructed one or put it in an isolate's `OpState`, so every
//! `ctx.services.*` call answered "service not available". This module is the
//! missing half.
//!
//! # Where the addresses come from
//!
//! From the control plane, not from this node's TOML file
//! (`docs/architecture.zh.md` section 6 is explicit: the control plane hands
//! the addresses down, they are not written into a node's own config). The
//! control plane's per-deployment channel into a node is `ApplyDeployment`'s
//! `env` map, so that is what carries them, under the reserved keys below.
//! [`ServiceWiring::take_from_env`] lifts them *out* of the map before the
//! rest of it becomes `ctx.env`, so an application never sees platform wiring
//! among its own variables, and an application variable named like one of
//! these cannot redirect a storage client.
//!
//! # What proves a call may act as its tenant
//!
//! Not anything in that map. A capability for this deployment's tenant is
//! something this node asks the control plane for itself, over the machine
//! channel and under its own name (`crate::deployment::capability_renewer`),
//! so nothing a deployment message carries can be replayed as access to a
//! tenant. [`StorageIdentity`] is where it meets the tenant header -- every
//! storage client in this crate holds one and stamps both on every call, and
//! the renewer writes each fresh capability into the identities that asked
//! for it.
//!
//! Both ends of that encoding live here, in one place, so the side that writes
//! the map and the side that reads it cannot drift apart:
//! [`ServiceWiring::write_to_env`] writes what [`ServiceWiring::take_from_env`]
//! reads. Whoever assembles the deployment payload should call the writer
//! rather than spelling the keys itself.
//!
//! # What gets constructed
//!
//! Only what the app's manifest declared: the four `services` booleans, plus
//! every `bindings` entry (see [`deis_bundle::Manifest::bindings`]). An app
//! that never asked for a database holds no database connection, which is both
//! cheaper and the honest answer to "what can this app reach".
//!
//! # Named bindings, and what the unnamed one still is
//!
//! `services.kv = true` turns on the tenant's **unnamed** KV namespace, which
//! is what `context.services.kv` reaches and what every application deployed
//! before bindings existed has been writing into. Nothing about it moves: the
//! calls it makes carry no binding header, so deis-store keys them by the
//! tenant alone, exactly as before. A binding is a *second* namespace beside
//! it, reached by the name the author chose, and it is empty on the day it is
//! declared because it has never existed before.
//!
//! The two declarations are separate on purpose. `services.kv` answers "does
//! this application use the unnamed KV"; a `kv` binding answers "and this
//! other namespace as well". An application may have either, both or neither,
//! and an application that has only bindings gets a connection to deis-store
//! but no `context.services.kv` - which is the honest answer, because it never
//! asked for that namespace.
//!
//! **Renaming a binding does not move its data.** The namespace is the name;
//! a version that declares `WARM` where the last one declared `HOT` gets an
//! empty namespace, and everything written through `HOT` is still in the store
//! under `HOT`, unreachable from the new version. Nothing here can move it,
//! because deis-store has no rename and a namespace holds an unbounded number
//! of rows, files and objects. So what this module does instead is say so:
//! [`ServiceWiring::describe_dropped`] is compared against the previous
//! version's table when a new one becomes the one that serves, and the log
//! line names the binding that went away, says its data is still there, and
//! gives the one action that reaches it again.
//!
//! # What happens when a service is down
//!
//! Nothing, until the app calls it. Every client here is built on a *lazy*
//! channel: no connection is dialled while the isolate is being created, so a
//! deis-store outage cannot stop an app from loading or take a node down.
//! The first call attempts the connection and, if it fails, that one call
//! fails with the transport error in the message; the next call tries again.
//! A malformed address is different -- it can never work and is a
//! configuration mistake -- so it is rejected when the clients are built.

use std::collections::{BTreeSet, HashMap};
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use deis_wire::conventions::BINDING_HEADER;
use deno_core::OpState;
use tonic::metadata::{Ascii, MetadataValue};
use tonic::transport::{Channel, Endpoint};
use tonic::Request;

use super::blob_client::BlobClient;
use super::db_client::DbClient;
use super::kv_client::KvClient;
use super::queue_client::QueueClient;
use super::tenant::Tenant;

/// Reserved deployment-env key holding the comma-separated list of services
/// the app's manifest declares, e.g. `kv,database,queue`.
pub const SERVICES_KEY: &str = "DEIS_SERVICES";

/// Reserved deployment-env key holding the deis-store gRPC address, which
/// serves KV, per-app SQL and blobs, e.g. `http://127.0.0.1:8090`.
///
/// Spelled once for the whole platform in [`deis_wire::conventions`], because
/// deis-control writes this key and this module reads it; two copies of the
/// name would drift into an app whose storage silently never connects.
pub const STORE_ENDPOINT_KEY: &str = deis_wire::conventions::STORE_ENDPOINT_ENV;

/// Reserved deployment-env key holding the deis-queue gRPC address,
/// e.g. `http://127.0.0.1:8091`.
pub const QUEUE_ENDPOINT_KEY: &str = deis_wire::conventions::QUEUE_ENDPOINT_ENV;

/// Reserved deployment-env key listing the queues this app consumes, as
/// `name[:batch_size[:max_concurrency]]` entries separated by commas, e.g.
/// `jobs:10:2,emails`. A field left out is the runtime's own default, and the
/// fields are positional, so a consumer that wants only the second one spells
/// the first as well.
pub const QUEUE_CONSUMERS_KEY: &str = "DEIS_QUEUE_CONSUMERS";

/// The name `env.ASSETS` is published under.
///
/// Not an address and not a credential: it is the one *binding* that arrives
/// through this map's namespace rather than through it, an object
/// `js/runtime.js` installs on the environment table for
/// [`crate::assets::AssetBundle::fetch_asset`]. It is reserved for the same
/// reason the endpoints are. A deployment that carried `ASSETS=...` as an
/// ordinary variable would put a string where the binding goes, and whichever
/// of the two wrote last would decide whether `env.ASSETS.fetch()` reached the
/// version's own files or threw "fetch is not a function" -- with the choice
/// belonging to the application rather than to the platform.
///
/// Bare, not `DEIS_`-prefixed, because it is the name an author writes; the
/// spelling is Cloudflare's, so a Pages project's code reads the same here.
///
/// The same name is refused as a schema-2 environment declaration and as a
/// logical resource, both of them from
/// [`deis_wire::conventions::RESERVED_PLATFORM_BINDING_NAME`], which is where
/// this spelling comes from: three components have to agree on it and none of
/// them can see the others' copy.
pub const ASSETS_BINDING_KEY: &str = deis_wire::conventions::RESERVED_PLATFORM_BINDING_NAME;

/// Reserved deployment-env key carrying this version's binding table.
///
/// Entries separated by commas, fields by colons:
/// `HOT:kv,DOCS:blob,AUTH:service:auth-api,BILLING:service:billing/production`.
/// Every field is a name from the manifest and every name is ASCII letters,
/// digits, `_` and `-`, so none of the three separators can occur inside one -
/// which is what makes the encoding unambiguous without quoting.
///
/// It travels here rather than being read from the bundle a second time for
/// the reason the service list does: this map is the channel the control plane
/// already has into a node, and a node that had to reopen the archive to learn
/// what the isolate may reach would be reading the same manifest twice and
/// could disagree with itself.
pub const BINDINGS_KEY: &str = "DEIS_BINDINGS";

/// Reserved deployment-env key listing the applications this version agrees to
/// be called by through a service binding, comma-separated.
///
/// The callee's half of a service call, taken verbatim from its own manifest's
/// `callableBy`. It travels with the deployment rather than being looked up
/// anywhere else so that consent is a property of the version that is serving:
/// a version that stops consenting stops being callable the moment it becomes
/// the one that serves, and nothing has to be redelivered to whoever named it.
pub const CALLABLE_BY_KEY: &str = "DEIS_CALLABLE_BY";

/// Every key this module claims. Anything spelled this way in a deployment's
/// env map is platform wiring and is removed before the app sees the map.
pub const RESERVED_ENV_KEYS: [&str; 7] = [
    SERVICES_KEY,
    STORE_ENDPOINT_KEY,
    QUEUE_ENDPOINT_KEY,
    QUEUE_CONSUMERS_KEY,
    ASSETS_BINDING_KEY,
    BINDINGS_KEY,
    CALLABLE_BY_KEY,
];

/// How long one dial to a storage service may take before the call that
/// triggered it fails. Without a bound, a store that accepts connections but
/// never completes them would hold an app's request until the isolate's own
/// execution timeout fired, hiding the real reason.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// How long one unary storage call may take. Long enough for a slow query,
/// short enough that a wedged store surfaces as a failed call rather than as
/// an app that appears to hang.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// One of the four platform services an application can declare.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ServiceKind {
    Kv,
    Database,
    Blob,
    Queue,
}

impl ServiceKind {
    /// The name used in `manifest.services` and in the reserved env key
    /// [`SERVICES_KEY`]. Those are the only two places a service is named:
    /// `AppConfig.resources` looks like a third but nothing reads it
    /// (`docs/architecture.zh.md` section 2 records it as undefined and
    /// unconsumed).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Kv => "kv",
            Self::Database => "database",
            Self::Blob => "blob",
            Self::Queue => "queue",
        }
    }

    /// Every kind, so the union in [`ServiceWiring::connected_kinds`] and an
    /// error message can list them without a second copy of this set going
    /// stale.
    pub const ALL: [ServiceKind; 4] = [Self::Kv, Self::Database, Self::Blob, Self::Queue];

    /// Which service a binding of this kind is served by, or `None` for a
    /// kind that is served by nothing this module dials.
    ///
    /// [`deis_bundle::BindingKind::Service`] is that kind: a call to another
    /// application is dispatched inside this node and has no address of its
    /// own. Spelled as a match rather than a name lookup so that adding a
    /// binding kind is a compile error here rather than a binding that
    /// silently builds no client.
    pub fn of_binding(kind: deis_bundle::BindingKind) -> Option<Self> {
        match kind {
            deis_bundle::BindingKind::Kv => Some(Self::Kv),
            deis_bundle::BindingKind::Database => Some(Self::Database),
            deis_bundle::BindingKind::Blob => Some(Self::Blob),
            deis_bundle::BindingKind::Queue => Some(Self::Queue),
            deis_bundle::BindingKind::Service => None,
        }
    }

    /// Parse a declared service name.
    ///
    /// Exactly the four spellings [`Self::as_str`] produces, because they are
    /// the only ones anything writes: `deis_bundle::Manifest::services` keys
    /// and [`SERVICES_KEY`], which this module writes itself in
    /// [`ServiceWiring::write_to_env`]. An alias with no producer would only
    /// tell a reader that some other spelling is supported somewhere.
    pub fn parse(name: &str) -> Option<Self> {
        match name.trim().to_ascii_lowercase().as_str() {
            "kv" => Some(Self::Kv),
            "database" => Some(Self::Database),
            "blob" => Some(Self::Blob),
            "queue" => Some(Self::Queue),
            _ => None,
        }
    }
}

/// The services an application declared it uses.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ServiceDeclaration {
    kinds: BTreeSet<ServiceKind>,
}

impl ServiceDeclaration {
    pub fn new() -> Self {
        Self::default()
    }

    /// Build from names, ignoring blanks. An unrecognised name is logged at
    /// warn and skipped: the app declared something this runtime has no
    /// binding for, so that service will simply be absent and every call to
    /// it will answer "not available".
    pub fn from_names<I, S>(names: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let mut kinds = BTreeSet::new();
        for name in names {
            let name = name.as_ref().trim();
            if name.is_empty() {
                continue;
            }
            match ServiceKind::parse(name) {
                Some(kind) => {
                    kinds.insert(kind);
                }
                None => tracing::warn!(
                    service = name,
                    "Application declares an unknown service {:?}; no binding will be created \
                     for it and every call to it will fail with \"service not available\"",
                    name
                ),
            }
        }
        Self { kinds }
    }

    /// Build from a bundle manifest's `services` map, taking the entries set
    /// to true.
    ///
    /// The `bindings` block deliberately does **not** contribute here. This
    /// set means one thing and one thing only: which of the tenant's *unnamed*
    /// namespaces the application asked for, which is what
    /// `context.services.kv` and its three siblings reach. Which kinds need a
    /// connection at all is a wider question and is answered once, by
    /// [`ServiceWiring::connected_kinds`]. Folding the two together here would
    /// give an application a `context.services.kv` it never declared as soon
    /// as it declared any KV binding.
    pub fn from_manifest(manifest: &deis_bundle::Manifest) -> Self {
        Self::from_names(
            manifest
                .services
                .iter()
                .filter(|(_, enabled)| **enabled)
                .map(|(name, _)| name.as_str()),
        )
    }

    pub fn declares(&self, kind: ServiceKind) -> bool {
        self.kinds.contains(&kind)
    }

    pub fn is_empty(&self) -> bool {
        self.kinds.is_empty()
    }

    pub fn insert(&mut self, kind: ServiceKind) {
        self.kinds.insert(kind);
    }

    /// The declared names, comma-separated, in a stable order.
    pub fn to_csv(&self) -> String {
        self.kinds
            .iter()
            .map(|kind| kind.as_str())
            .collect::<Vec<_>>()
            .join(",")
    }
}

/// Where the storage services live, as told to this node by the control
/// plane.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ServiceEndpoints {
    /// deis-store: KV, per-app SQL and blobs.
    pub store: Option<String>,
    /// deis-queue.
    pub queue: Option<String>,
}

/// One queue this application consumes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueueConsumerSpec {
    pub name: String,
    /// Messages to lease per poll. `None` leaves the choice to the consumer's
    /// own default.
    pub batch_size: Option<u32>,
    /// Batches this node may have outstanding for this queue at once. `None`
    /// leaves the choice to the consumer's own default, which is one.
    ///
    /// Outstanding, not parallel; see
    /// [`deis_bundle::QueueConsumer::max_concurrency`] and
    /// [`super::queue_consumer::ConsumerSettings::max_concurrency`], which is
    /// where the value is clamped and applied.
    pub max_concurrency: Option<u32>,
}

/// Everything about an application's platform services that has to survive the
/// trip from the control plane to the isolate.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct ServiceWiring {
    pub declaration: ServiceDeclaration,
    pub endpoints: ServiceEndpoints,
    pub consumers: Vec<QueueConsumerSpec>,
    /// A store capability (`aud = "store"`) to start from, for an in-process
    /// caller that already holds one. Never read from a deployment: a node's
    /// clients start empty and are filled by the capability renewer, which
    /// leaves a preset value alone until it has a fresher one.
    pub storage_token: Option<String>,
    /// The same for the queue (`aud = "queue"`), kept apart because the two
    /// audiences are different capabilities.
    pub queue_token: Option<String>,
    /// The namespaces and the applications this version reaches by name,
    /// exactly as its manifest declared them.
    ///
    /// The isolate's copy of the manifest's `bindings` block: it decides which
    /// clients are built, which names `context.bindings` publishes, and which
    /// `(application, environment)` a service binding resolves to. It carries
    /// no credential and no address - a storage binding is a name composed
    /// onto this tenant, and a service binding is a pair of names this node
    /// either runs or does not.
    pub bindings: deis_bundle::DeclaredBindings,
    /// Which applications this version agrees to be called by, verbatim from
    /// its manifest. Empty consents to nobody, which is what an application
    /// that never thought about being called should mean.
    pub callable_by: Vec<String>,
}

impl std::fmt::Debug for ServiceWiring {
    /// The token is a credential: a derived `Debug` would print it in
    /// whatever line logged an app's wiring, and that value is enough to read
    /// and overwrite everything the tenant has stored. Whether one arrived is
    /// the part worth seeing.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ServiceWiring")
            .field("declaration", &self.declaration)
            .field("endpoints", &self.endpoints)
            .field("consumers", &self.consumers)
            .field("bindings", &self.bindings.describe())
            .field("callable_by", &self.callable_by)
            .field("storage_token", &token_state(&self.storage_token))
            .field("queue_token", &token_state(&self.queue_token))
            .finish()
    }
}

/// How a capability token is allowed to appear in a log line.
fn token_state(token: &Option<String>) -> &'static str {
    match token {
        Some(_) => "<present>",
        None => "<absent>",
    }
}

impl ServiceWiring {
    /// Assemble from a manifest plus the addresses the control plane holds.
    pub fn from_manifest(manifest: &deis_bundle::Manifest, endpoints: ServiceEndpoints) -> Self {
        Self {
            declaration: ServiceDeclaration::from_manifest(manifest),
            endpoints,
            bindings: manifest.bindings.clone(),
            callable_by: manifest.callable_by.clone(),
            // Not a manifest concept: the node asks for its own, see the
            // field's documentation.
            storage_token: None,
            queue_token: None,
            consumers: manifest
                .queues
                .iter()
                .map(|queue| QueueConsumerSpec {
                    name: queue.name.clone(),
                    batch_size: queue.batch_size,
                    max_concurrency: queue.max_concurrency,
                })
                .collect(),
        }
    }

    /// Remove the reserved keys from a deployment's env map and read the
    /// wiring out of them.
    ///
    /// The keys are *removed*, not copied: what is left is exactly the
    /// application's own variables, which is what becomes `ctx.env`.
    pub fn take_from_env(env: &mut HashMap<String, String>) -> Self {
        let services = env.remove(SERVICES_KEY).unwrap_or_default();
        let store = env.remove(STORE_ENDPOINT_KEY);
        let queue = env.remove(QUEUE_ENDPOINT_KEY);
        let consumers = env.remove(QUEUE_CONSUMERS_KEY).unwrap_or_default();
        let bindings = env.remove(BINDINGS_KEY).unwrap_or_default();
        let callable_by = env.remove(CALLABLE_BY_KEY).unwrap_or_default();
        // Nothing is read out of it -- the binding is built from the version's
        // own asset bundle, not from a string. Removing it is the whole point:
        // `js/runtime.js` writes `env.ASSETS` over whatever this map held, and
        // an application that shipped its own value would otherwise see it in
        // `Deno.env` and in the map the queue and cron handlers are handed.
        env.remove(ASSETS_BINDING_KEY);

        Self {
            declaration: ServiceDeclaration::from_names(services.split(',')),
            endpoints: ServiceEndpoints {
                store: non_empty(store),
                queue: non_empty(queue),
            },
            consumers: parse_consumers(&consumers),
            storage_token: None,
            queue_token: None,
            bindings: parse_bindings(&bindings),
            callable_by: callable_by
                .split(',')
                .map(str::trim)
                .filter(|name| !name.is_empty())
                .map(str::to_string)
                .collect(),
        }
    }

    /// Write the reserved keys into a deployment's env map.
    ///
    /// The counterpart of [`Self::take_from_env`]; whoever builds the payload
    /// the control plane sends should use this rather than spelling the keys
    /// out, so the two halves cannot drift.
    pub fn write_to_env(&self, env: &mut HashMap<String, String>) {
        if !self.declaration.is_empty() {
            env.insert(SERVICES_KEY.to_string(), self.declaration.to_csv());
        }
        if let Some(store) = &self.endpoints.store {
            env.insert(STORE_ENDPOINT_KEY.to_string(), store.clone());
        }
        if let Some(queue) = &self.endpoints.queue {
            env.insert(QUEUE_ENDPOINT_KEY.to_string(), queue.clone());
        }
        if !self.bindings.is_empty() {
            env.insert(BINDINGS_KEY.to_string(), encode_bindings(&self.bindings));
        }
        if !self.callable_by.is_empty() {
            env.insert(CALLABLE_BY_KEY.to_string(), self.callable_by.join(","));
        }
        if !self.consumers.is_empty() {
            let list = self
                .consumers
                .iter()
                .map(
                    |consumer| match (consumer.batch_size, consumer.max_concurrency) {
                        // Positional fields, so a consumer that named only the
                        // concurrency still has to occupy the batch-size slot.
                        // `0` is the spelling for "leave it to the runtime": it is
                        // never a legal batch size, and `parse_consumers` reads it
                        // back as absent.
                        (None, None) => consumer.name.clone(),
                        (Some(size), None) => format!("{}:{}", consumer.name, size),
                        (size, Some(concurrency)) => {
                            format!("{}:{}:{}", consumer.name, size.unwrap_or(0), concurrency)
                        }
                    },
                )
                .collect::<Vec<_>>()
                .join(",");
            env.insert(QUEUE_CONSUMERS_KEY.to_string(), list);
        }
    }

    /// Whether this app asked for anything at all.
    pub fn is_empty(&self) -> bool {
        self.declaration.is_empty() && self.consumers.is_empty() && self.bindings.is_empty()
    }

    /// Whether the manifest's `services` map turned on this kind's unnamed
    /// namespace, which is the one `context.services.*` reaches.
    pub fn services_declares(&self, kind: ServiceKind) -> bool {
        self.declaration.declares(kind)
    }

    /// Which kinds need a client built: the `services` map, plus the kind of
    /// every declared binding.
    ///
    /// The union, and the one place it is taken. A `kv` binding needs a
    /// connection to deis-store whether or not the application also asked for
    /// the unnamed KV, and a queue binding needs a connection to deis-queue
    /// whether or not it consumes anything.
    pub fn connected_kinds(&self) -> BTreeSet<ServiceKind> {
        let mut kinds: BTreeSet<ServiceKind> = ServiceKind::ALL
            .into_iter()
            .filter(|kind| self.declaration.declares(*kind))
            .collect();
        for kind in self.bindings.kinds() {
            if let Some(kind) = ServiceKind::of_binding(kind) {
                kinds.insert(kind);
            }
        }
        kinds
    }

    /// The binding names this version declares of one kind, in manifest order.
    pub fn binding_names(&self, kind: deis_bundle::BindingKind) -> Vec<String> {
        self.bindings
            .names_of(kind)
            .into_iter()
            .map(str::to_string)
            .collect()
    }

    /// What to tell an operator about the bindings a new version dropped.
    ///
    /// A binding name *is* its namespace, so a version that stops declaring a
    /// name does not rename anything: the new name is an empty namespace and
    /// everything written through the old one is still in the store, under the
    /// old name, and no longer reachable from this application. Nothing on
    /// this node can move it - deis-store has no rename, and a namespace holds
    /// an unbounded number of rows, files and objects - so the one thing worth
    /// doing is saying it while somebody is deploying.
    ///
    /// Returns `None` when nothing was dropped, which is the ordinary case and
    /// is not worth a log line. Only storage kinds are reported: a service
    /// binding has no data behind it, so dropping one loses nothing.
    pub fn describe_dropped(&self, previous: &deis_bundle::DeclaredBindings) -> Option<String> {
        let dropped: Vec<String> = previous
            .iter()
            .filter(|(name, binding)| {
                binding.kind.is_storage() && self.bindings.get(name).is_none()
            })
            .map(|(name, binding)| format!("{name} ({})", binding.kind.as_str()))
            .collect();
        if dropped.is_empty() {
            return None;
        }
        Some(dropped.join(", "))
    }

    /// The queues this app should be polled for: the declared consumers, but
    /// only if the app also declared the queue service and an address exists.
    ///
    /// Returns an empty list, with the reason logged, in every case where
    /// polling could not work -- a consumer that silently fails forever is
    /// worse than one that says why it never started.
    pub fn active_consumers(&self, tenant: &str) -> Vec<QueueConsumerSpec> {
        if self.consumers.is_empty() {
            return Vec::new();
        }

        let Some(_endpoint) = self.endpoints.queue.as_ref() else {
            tracing::warn!(
                app_id = tenant,
                queues = self.consumers.len(),
                "{} declares {} queue consumer(s) but the control plane sent no queue \
                 address; nothing will be consumed and the messages will pile up in deis-queue",
                tenant,
                self.consumers.len()
            );
            return Vec::new();
        };

        if !self.connected_kinds().contains(&ServiceKind::Queue) {
            tracing::warn!(
                app_id = tenant,
                "{} declares queue consumers but does not declare the queue service; \
                 nothing will be consumed. Add \"queue\" to the manifest's services",
                tenant
            );
            return Vec::new();
        }

        self.consumers.clone()
    }
}

fn non_empty(value: Option<String>) -> Option<String> {
    value.filter(|v| !v.trim().is_empty())
}

/// Write a binding table into the one line [`BINDINGS_KEY`] carries.
///
/// `NAME:kind` for a namespace of this tenant, `NAME:service:app` or
/// `NAME:service:app/environment` for a call to another application. The
/// counterpart is [`parse_bindings`], and both live here so the side that
/// writes the map and the side that reads it cannot drift apart.
fn encode_bindings(bindings: &deis_bundle::DeclaredBindings) -> String {
    bindings
        .iter()
        .map(|(name, binding)| match binding.kind {
            deis_bundle::BindingKind::Service => {
                let app = binding.app.as_deref().unwrap_or_default();
                match binding.service_environment() {
                    deis_bundle::ServiceEnvironment::Same => {
                        format!("{name}:service:{app}")
                    }
                    deis_bundle::ServiceEnvironment::Named(environment) => {
                        format!("{name}:service:{app}/{environment}")
                    }
                }
            }
            kind => format!("{name}:{}", kind.as_str()),
        })
        .collect::<Vec<_>>()
        .join(",")
}

/// Read a binding table back out of [`BINDINGS_KEY`].
///
/// An entry that does not parse is dropped with the reason said out loud, and
/// the rest are kept. That is the recoverable half: a binding this build does
/// not understand is one name that answers "no such binding" where the
/// application uses it, while refusing the whole line would take away every
/// other namespace the application has - including ones it is mid-write on.
/// The declaration is validated at build, at upload and again when the node
/// parsed the manifest, so anything reaching here that does not parse is a
/// disagreement between two builds of the platform rather than an author's
/// mistake.
fn parse_bindings(list: &str) -> deis_bundle::DeclaredBindings {
    let mut bindings = deis_bundle::DeclaredBindings::new();
    for entry in list.split(',') {
        let entry = entry.trim();
        if entry.is_empty() {
            continue;
        }
        let mut fields = entry.split(':');
        let name = fields.next().unwrap_or("").trim();
        let kind = fields.next().unwrap_or("").trim();
        let target = fields.next().map(str::trim);
        let Some(kind) = deis_bundle::BindingKind::ALL
            .into_iter()
            .find(|candidate| candidate.as_str() == kind)
        else {
            tracing::warn!(
                binding = name,
                kind,
                "This deployment carries a binding {:?} of kind {:?}, which this build of the \
                 node has no binding for; every use of that name will fail with \"no such \
                 binding\" until the node is upgraded",
                name,
                kind
            );
            continue;
        };
        let binding = match kind {
            deis_bundle::BindingKind::Service => {
                let Some(target) = target.filter(|target| !target.is_empty()) else {
                    tracing::warn!(
                        binding = name,
                        "This deployment carries a service binding {:?} that names no \
                         application to call; every use of that name will fail",
                        name
                    );
                    continue;
                };
                let (app, environment) = match target.split_once('/') {
                    Some((app, environment)) => (app, Some(environment.to_string())),
                    None => (target, None),
                };
                deis_bundle::Binding {
                    kind,
                    app: Some(app.to_string()),
                    environment,
                }
            }
            kind => deis_bundle::Binding::storage(kind),
        };
        bindings.insert(name, binding);
    }
    bindings
}

/// Parse `name[:batch_size[:max_concurrency]]` entries. A field that does not
/// parse keeps the queue but drops that field, which is the recoverable half:
/// the consumer polls with the runtime's own default rather than not at all.
///
/// `0` in either numeric field means "the runtime decides". It is the spelling
/// [`ServiceWiring::write_to_env`] uses for a batch size nobody declared when
/// a concurrency after it was, because the fields are positional and the slot
/// still has to be filled.
fn parse_consumers(list: &str) -> Vec<QueueConsumerSpec> {
    let mut consumers = Vec::new();
    for entry in list.split(',') {
        let entry = entry.trim();
        if entry.is_empty() {
            continue;
        }
        let mut fields = entry.split(':');
        let name = fields.next().unwrap_or("").trim();
        if name.is_empty() {
            continue;
        }
        let batch_size = consumer_number(name, "batch size", fields.next());
        let max_concurrency = consumer_number(name, "max concurrency", fields.next());
        consumers.push(QueueConsumerSpec {
            name: name.to_string(),
            batch_size,
            max_concurrency,
        });
    }
    consumers
}

/// One positional number of a `DEIS_QUEUE_CONSUMERS` entry.
///
/// Absent and `0` both mean "leave it to the runtime". Anything else that does
/// not read as a number is said out loud and then treated as absent: a queue
/// consumed with the default batch size is a queue that still gets consumed,
/// while refusing the whole entry would leave the messages piling up in
/// deis-queue with nothing saying why.
fn consumer_number(queue: &str, field: &str, value: Option<&str>) -> Option<u32> {
    let text = value?.trim();
    if text.is_empty() {
        return None;
    }
    match text.parse::<u32>() {
        Ok(0) => None,
        Ok(number) => Some(number),
        Err(_) => {
            tracing::warn!(
                queue,
                field,
                value = text,
                "Queue consumer {:?} declares an unreadable {} {:?}; polling it with the runtime \
                 default for that setting instead",
                queue,
                field,
                text
            );
            None
        }
    }
}

/// A channel that has not dialled anything yet.
///
/// `connect_lazy` is what keeps a storage outage out of the app-loading path:
/// the channel is usable immediately and only attempts a connection when a
/// call is made on it, retrying on the call after that. Only a malformed
/// address fails here.
///
/// The connection is TLS, checked against this node's installed trust as a
/// service of `kind`; see [`super::service_tls`].
fn lazy_channel(
    endpoint: &str,
    kind: deis_identity::ServiceKind,
    request_timeout: Option<Duration>,
) -> Result<Channel> {
    let mut builder = Endpoint::from_shared(endpoint.to_string())
        .with_context(|| format!("{endpoint:?} is not a usable gRPC address"))?
        .connect_timeout(CONNECT_TIMEOUT);
    if let Some(timeout) = request_timeout {
        builder = builder.timeout(timeout);
    }
    super::service_tls::lazy(builder, endpoint, kind)
}

/// A unary-call channel: bounded per call.
pub(crate) fn unary_channel(
    endpoint: &str,
    kind: deis_identity::ServiceKind,
    identity: &StorageIdentity,
) -> Result<CapabilityChannel> {
    Ok(CapabilityChannel::new(
        lazy_channel(endpoint, kind, Some(REQUEST_TIMEOUT))?,
        identity,
    ))
}

/// A streaming channel: bounded only on the dial.
///
/// A blob download is a stream whose legitimate duration depends on the object
/// size, so a per-call deadline would cut off large-but-healthy transfers. The
/// blob client applies its own size ceiling instead.
pub(crate) fn streaming_channel(
    endpoint: &str,
    kind: deis_identity::ServiceKind,
    identity: &StorageIdentity,
) -> Result<CapabilityChannel> {
    Ok(CapabilityChannel::new(
        lazy_channel(endpoint, kind, None)?,
        identity,
    ))
}

/// A channel to deis-store or deis-queue that tells the capability renewer
/// when the service refuses the capability a call presented.
///
/// A revoked capability, or one issued under an auth epoch since bumped, is
/// refused on every call until it is replaced; left to its schedule, the
/// renewer would replace it only five minutes before it expires. Watching
/// here, under every client rather than in each of their calls, means no
/// call can forget to report it.
///
/// The service refuses a credential before it answers anything, so the
/// refusal is a trailers-only response: its `grpc-status` is in the response
/// headers, where this reads it, for unary and streaming calls alike.
#[derive(Clone)]
pub struct CapabilityChannel {
    inner: Channel,
    token: std::sync::Arc<std::sync::RwLock<Option<String>>>,
}

impl CapabilityChannel {
    fn new(inner: Channel, identity: &StorageIdentity) -> Self {
        Self {
            inner,
            token: identity.token_lock(),
        }
    }
}

/// `grpc-status` values that refuse the credential itself.
fn refuses_the_credential(headers: &tonic::codegen::http::HeaderMap) -> bool {
    let unauthenticated = tonic::Code::Unauthenticated as i32;
    let permission_denied = tonic::Code::PermissionDenied as i32;
    headers
        .get("grpc-status")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<i32>().ok())
        .is_some_and(|code| code == unauthenticated || code == permission_denied)
}

impl tower::Service<tonic::codegen::http::Request<tonic::body::BoxBody>> for CapabilityChannel {
    type Response =
        <Channel as tower::Service<tonic::codegen::http::Request<tonic::body::BoxBody>>>::Response;
    type Error =
        <Channel as tower::Service<tonic::codegen::http::Request<tonic::body::BoxBody>>>::Error;
    type Future = std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<Self::Response, Self::Error>> + Send>,
    >;

    fn poll_ready(
        &mut self,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        tower::Service::poll_ready(&mut self.inner, cx)
    }

    fn call(
        &mut self,
        request: tonic::codegen::http::Request<tonic::body::BoxBody>,
    ) -> Self::Future {
        let presented = request
            .headers()
            .get("authorization")
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.strip_prefix("Bearer "))
            .map(str::to_string);
        let token = std::sync::Arc::clone(&self.token);
        let response = tower::Service::call(&mut self.inner, request);
        Box::pin(async move {
            let response = response.await?;
            if let Some(presented) = presented {
                if refuses_the_credential(response.headers()) {
                    crate::deployment::capability_renewer::refused(&token, &presented);
                }
            }
            Ok(response)
        })
    }
}

/// Who a storage call says it is, and the proof that goes with the claim.
///
/// The tenant header is a routing key: it says *which* namespace the call
/// wants, and on its own it is only a claim - anything that can reach the
/// storage port can name any tenant. The capability token is the proof,
/// issued by deis-control to this node, under this node's name, for the
/// tenants it runs.
///
/// The two are stamped together, here, rather than in each of the four
/// clients: a client that stamped one and forgot the other would either read
/// another tenant's data or be refused on every call, and neither failure
/// shows up until something is deployed against a store that checks.
///
/// Until the first capability arrives a call stamps only the tenant and is
/// refused by a verifying store, which is why the renewer is woken the moment
/// an identity is registered rather than on its next tick.
///
/// The token is held behind an [`Arc`]`<`[`RwLock`]`>` so that the node's
/// background capability renewal loop can fill and refresh it without
/// rebuilding the isolate. [`StorageIdentity::new`] populates the lock from
/// the initial value, and clones share it.
#[derive(Clone)]
pub struct StorageIdentity {
    tenant: Tenant,
    /// Held behind a lock so the renewal loop can refresh it. An empty or
    /// missing token is `None` (or `Some("")`): both mean no credential was
    /// handed to this node, and every call stamps only the tenant.
    token: std::sync::Arc<std::sync::RwLock<Option<String>>>,
    /// Which namespace inside the tenant this client speaks for, or `None`
    /// for the tenant's unnamed one.
    ///
    /// Beside the token rather than folded into it, because it is not part of
    /// what is being proved: the token authorises the tenant, and the binding
    /// selects a namespace *inside* the tenant it is stamped next to. That is
    /// the whole security argument for named bindings - a wrong or forged
    /// binding reaches another namespace of the caller's own data and can
    /// never reach another tenant's, because the tenant half is the half the
    /// storage service checks a credential against.
    binding: Option<MetadataValue<Ascii>>,
    /// The binding name as written, for log lines and for [`Self::binding`].
    binding_name: Option<String>,
}

impl std::fmt::Debug for StorageIdentity {
    /// Never the token itself: it is enough to read and overwrite everything
    /// this tenant has stored.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StorageIdentity")
            .field("tenant", &self.tenant.as_str())
            .field(
                "binding",
                &self.binding_name.as_deref().unwrap_or("<unnamed>"),
            )
            .field(
                "token",
                &if self
                    .token
                    .read()
                    .unwrap()
                    .as_ref()
                    .map_or(false, |t| !t.is_empty())
                {
                    "<present>"
                } else {
                    "<absent>"
                },
            )
            .finish()
    }
}

impl StorageIdentity {
    /// Bind to one tenant, optionally carrying that tenant's capability
    /// token.
    ///
    /// `tenant` is the `<app>.<environment>` pair as the storage services
    /// read it. A blank token counts as none: an empty header would be
    /// presented as a token and refused, where sending nothing at least
    /// works against a service that requires no proof.
    ///
    /// The token is wrapped in a private [`Arc`]`<`[`RwLock`]`>` so callers
    /// that also hold a reference through [`Self::token_lock`] can update
    /// it.
    ///
    /// Fails on a tenant that is not two legal parts, and on a token that is
    /// not sendable as gRPC metadata. Both are configuration faults that no
    /// retry fixes: the control plane's token is base64, so a value that will
    /// not parse means this node was handed something that was never a token,
    /// and every storage call it made would be refused with a message about
    /// the tenant rather than about the credential.
    pub fn new(tenant: impl Into<String>, token: Option<&str>) -> Result<Self> {
        let tenant = Tenant::new(tenant)?;
        let packed = match token.map(str::trim).filter(|token| !token.is_empty()) {
            Some(token) => {
                // Parse eagerly: a token that cannot be sent as metadata is
                // caught before the first storage call.
                let _ = token.parse::<MetadataValue<Ascii>>().map_err(|_| {
                    anyhow!(
                        "the storage capability token handed to {} cannot be sent as gRPC \
                         metadata, so every storage call of this application would be refused; \
                         the control plane mints it as base64, which means this value did not \
                         come from one",
                        tenant.as_str()
                    )
                })?;
                Some(token.to_string())
            }
            None => None,
        };
        Ok(Self {
            tenant,
            token: std::sync::Arc::new(std::sync::RwLock::new(packed)),
            binding: None,
            binding_name: None,
        })
    }

    /// The same tenant and the same proof, speaking for one named namespace
    /// inside it.
    ///
    /// `None` gives back the unnamed namespace, which is what
    /// `context.services.kv` and every application deployed before bindings
    /// existed address. The name is checked here, once when the client is
    /// built, rather than on every call: a name the storage service would
    /// refuse is a manifest that should never have been accepted, and finding
    /// out at load time names the application instead of one request.
    pub fn for_binding(&self, binding: Option<&str>) -> Result<Self> {
        let Some(name) = binding.map(str::trim).filter(|name| !name.is_empty()) else {
            return Ok(Self {
                tenant: self.tenant.clone(),
                token: std::sync::Arc::clone(&self.token),
                binding: None,
                binding_name: None,
            });
        };
        if !deis_wire::conventions::is_valid_binding_name(name) {
            return Err(anyhow!(
                "{name:?} is not a usable binding name for {}: a name becomes part of a directory \
                 and a database file name in deis-store, so it must be 1 to 64 characters of \
                 ASCII letters, digits, '_' or '-'",
                self.tenant.as_str()
            ));
        }
        Ok(Self {
            tenant: self.tenant.clone(),
            token: std::sync::Arc::clone(&self.token),
            binding: Some(name.parse::<MetadataValue<Ascii>>().map_err(|_| {
                anyhow!(
                    "the binding name {name:?} of {} cannot be sent as gRPC metadata",
                    self.tenant.as_str()
                )
            })?),
            binding_name: Some(name.to_string()),
        })
    }

    /// Which namespace inside the tenant this client speaks for, or `None`
    /// for the unnamed one.
    pub fn binding(&self) -> Option<&str> {
        self.binding_name.as_deref()
    }

    /// How this identity reads in a log line: the tenant, and the namespace
    /// inside it when there is one.
    pub fn describe(&self) -> String {
        match &self.binding_name {
            Some(name) => format!("{}@{name}", self.tenant.as_str()),
            None => self.tenant.as_str().to_string(),
        }
    }

    /// The tenant -- application and environment -- calls are charged to.
    pub fn tenant(&self) -> &str {
        self.tenant.as_str()
    }

    /// Whether this node holds a capability for the tenant yet. False until
    /// the first `IssueCapability` answer lands, and every call made before
    /// then is refused, so it is worth a log field wherever a client is built.
    pub fn is_proven(&self) -> bool {
        self.token
            .read()
            .unwrap()
            .as_ref()
            .map_or(false, |t| !t.is_empty())
    }

    /// Returns the shared lock behind this identity's token so a background
    /// renewal loop can update it.
    pub fn token_lock(&self) -> std::sync::Arc<std::sync::RwLock<Option<String>>> {
        std::sync::Arc::clone(&self.token)
    }

    /// Wrap a message in a request carrying the tenant and, when there is
    /// one, the token that proves the caller may be it.
    pub fn stamp<T>(&self, message: T) -> Request<T> {
        let mut request = self.tenant.stamp(message);
        if let Some(ref token) = *self.token.read().unwrap() {
            if !token.is_empty() {
                let bearer = format!(
                    "Bearer {}",
                    token
                        .parse::<MetadataValue<Ascii>>()
                        .expect("a valid capability token is always ASCII")
                        .to_str()
                        .expect("ASCII capability token is valid metadata")
                );
                request.metadata_mut().insert(
                    "authorization",
                    bearer.parse().expect("bearer token is valid metadata"),
                );
            }
        }
        // Stamped here rather than in each client, for the reason the token
        // is: a client that stamped the tenant and forgot the binding would
        // read and write the tenant's unnamed namespace while the application
        // believed it was using the one it named, and nothing would fail.
        if let Some(binding) = &self.binding {
            request
                .metadata_mut()
                .insert(BINDING_HEADER, binding.clone());
        }
        request
    }
}

/// Every client of one kind an isolate holds: the tenant's unnamed namespace,
/// and one per named binding of that kind.
///
/// Two collections and not one map with an optional key, because the two are
/// different declarations with different meanings. The unnamed namespace
/// exists when `services.<kind>` is true, and it is the one an application
/// written before bindings existed has been using all along; a named one
/// exists when the manifest declares a binding, and it starts empty. Folding
/// them together would make "the application declared kv" and "the application
/// declared a kv binding called HOT" one fact, and the first symptom would be
/// `context.services.kv` quietly answering out of somebody's `HOT`.
#[derive(Clone)]
pub struct Bound<T> {
    unnamed: Option<T>,
    named: std::collections::BTreeMap<String, T>,
}

impl<T> Default for Bound<T> {
    fn default() -> Self {
        Self {
            unnamed: None,
            named: std::collections::BTreeMap::new(),
        }
    }
}

impl<T> Bound<T> {
    /// The client for one binding name, or for the unnamed namespace when the
    /// name is empty.
    ///
    /// `None` means the application never declared it, which is the answer an
    /// op turns into "service not available" or "no such binding". There is no
    /// fallback from a named binding to the unnamed one: an application that
    /// asked for `HOT` and got the unnamed namespace would be writing into the
    /// wrong place with nothing failing.
    pub fn get(&self, binding: &str) -> Option<&T> {
        match binding.trim() {
            "" => self.unnamed.as_ref(),
            name => self.named.get(name),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.unnamed.is_none() && self.named.is_empty()
    }

    /// The binding names, for a log line. The unnamed namespace is not one.
    pub fn names(&self) -> Vec<&str> {
        self.named.keys().map(String::as_str).collect()
    }
}

/// The clients one application's isolate holds.
///
/// Absent means "not declared" (or, for queue, "no address"), and every op
/// that looks for a missing client answers "service not available". There is
/// no in-process stand-in for any of them: an app that believes it wrote
/// something is worse off than one told plainly that it could not.
///
/// The queue is deliberately one client and not a [`Bound`]: deis-queue has
/// always keyed messages by `(tenant, queue, id)`, so a name inside a tenant
/// is what a queue already is. A queue binding names the queue, which is the
/// same name a consumer declares in the manifest's `queues`, and it needs no
/// second namespace underneath it.
#[derive(Clone, Default)]
pub struct AppServices {
    pub kv: Bound<KvClient>,
    pub database: Bound<DbClient>,
    pub blob: Bound<BlobClient>,
    pub queue: Option<QueueClient>,
}

impl AppServices {
    /// Build exactly the clients this app declared, all charged to `tenant`.
    ///
    /// `tenant` is the application *and its environment*, spelled the way the
    /// storage services read it (`demo.production`): it is
    /// [`crate::app::AppKey::tenant_id`], which is what this node identifies a
    /// running application by. Passing a bare application id is refused rather
    /// than treated as production, because two environments sharing one
    /// namespace is the failure this pair exists to prevent and it is not
    /// recoverable once a preview has written over production's data.
    ///
    /// A binding of one of these kinds adds a *second* namespace inside that
    /// same tenant, and never a second tenant: the name is composed onto the
    /// tenant after the capability token for it has already been chosen, so a
    /// binding widens what this application can address inside its own data
    /// and nothing else.
    ///
    /// Fails only on things that can never work, and only for an app that
    /// actually reaches a storage service: a tenant that is not two legal
    /// parts, an address that is not a URL, a capability token that cannot
    /// be sent as metadata, or a binding name that is not a legal namespace.
    /// A service that is merely unreachable is not an error here -- see the
    /// module docs.
    ///
    /// Must be called from inside a Tokio runtime: building a lazy channel
    /// registers a connector with the reactor. Every caller in this crate is
    /// on a runtime thread already.
    pub fn connect(tenant: &str, wiring: &ServiceWiring) -> Result<Self> {
        let mut services = Self::default();

        let connected = wiring.connected_kinds();
        let store_kinds: Vec<ServiceKind> =
            [ServiceKind::Kv, ServiceKind::Database, ServiceKind::Blob]
                .into_iter()
                .filter(|kind| connected.contains(kind))
                .collect();
        let declares_queue = connected.contains(&ServiceKind::Queue);

        // Who the clients speak as: the tenant they are charged to and the
        // token that proves this node may act as it. Built where a client is
        // actually built and nowhere else -- an application that reaches no
        // storage service is identified on this node by a key that need never
        // split into an app and an environment, and demanding one of it here
        // would stop it from loading over storage it does not use.
        let identity = || StorageIdentity::new(tenant, wiring.storage_token.as_deref());
        // Queue clients use a separate token because the queue audience
        // differs from the store audience in capability tokens.
        let queue_identity = || StorageIdentity::new(tenant, wiring.queue_token.as_deref());

        if !store_kinds.is_empty() {
            match wiring.endpoints.store.as_deref() {
                Some(endpoint) => {
                    let identity = identity()?;
                    crate::deployment::capability_renewer::register(
                        tenant,
                        "store",
                        identity.token_lock(),
                    );
                    for kind in store_kinds {
                        // The unnamed namespace exists only when the manifest
                        // says `services.<kind>`. A version that declares only
                        // bindings gets the connection and the named
                        // namespaces, and `context.services.<kind>` answers
                        // "not available" -- which is true: it never asked for
                        // that namespace.
                        let unnamed = wiring.services_declares(kind);
                        match kind {
                            ServiceKind::Kv => {
                                let base = KvClient::connect_lazy(endpoint, &identity)?;
                                services.kv = bind_all(
                                    &base,
                                    unnamed,
                                    &wiring.binding_names(deis_bundle::BindingKind::Kv),
                                    KvClient::for_binding,
                                )?;
                            }
                            ServiceKind::Database => {
                                let base = DbClient::connect_lazy(endpoint, &identity)?;
                                services.database = bind_all(
                                    &base,
                                    unnamed,
                                    &wiring.binding_names(deis_bundle::BindingKind::Database),
                                    DbClient::for_binding,
                                )?;
                            }
                            ServiceKind::Blob => {
                                let base = BlobClient::connect_lazy(endpoint, &identity)?;
                                services.blob = bind_all(
                                    &base,
                                    unnamed,
                                    &wiring.binding_names(deis_bundle::BindingKind::Blob),
                                    BlobClient::for_binding,
                                )?;
                            }
                            ServiceKind::Queue => unreachable!("queue is not served by the store"),
                        }
                    }
                    tracing::info!(
                        app_id = tenant,
                        endpoint,
                        services = wiring.declaration.to_csv(),
                        bindings = %wiring.bindings.describe(),
                        proven = identity.is_proven(),
                        "Bound {} to deis-store at {} for {}; every read and write is charged to \
                         that tenant, so another environment of this app sees none of it. Named \
                         bindings are further namespaces inside that same tenant: {}",
                        tenant,
                        endpoint,
                        wiring.declaration.to_csv(),
                        wiring.bindings.describe()
                    );
                }
                None => tracing::error!(
                    app_id = tenant,
                    services = wiring.declaration.to_csv(),
                    "{} declares storage services ({}) but the control plane sent no \
                     deis-store address; every ctx.services call will fail with \"service not \
                     available\" until a deployment carries one",
                    tenant,
                    wiring.declaration.to_csv()
                ),
            }
        }

        if declares_queue {
            match wiring.endpoints.queue.as_deref() {
                Some(endpoint) => {
                    let identity = queue_identity()?;
                    crate::deployment::capability_renewer::register(
                        tenant,
                        "queue",
                        identity.token_lock(),
                    );
                    services.queue = Some(QueueClient::connect_lazy(endpoint, &identity)?);
                    tracing::info!(
                        app_id = tenant,
                        endpoint,
                        proven = identity.is_proven(),
                        "Bound {} to deis-queue at {}",
                        tenant,
                        endpoint
                    );
                }
                None => tracing::error!(
                    app_id = tenant,
                    "{} declares the queue service but the control plane sent no deis-queue \
                     address; every ctx.services.queue.send will fail with \"service not \
                     available\" -- the messages are refused, not held anywhere",
                    tenant
                ),
            }
        }

        Ok(services)
    }

    /// Whether anything at all was constructed.
    pub fn is_empty(&self) -> bool {
        self.kv.is_empty()
            && self.database.is_empty()
            && self.blob.is_empty()
            && self.queue.is_none()
    }

    /// Hand the clients to the isolate.
    ///
    /// The whole set of each kind goes in, not one client per kind: an op is
    /// handed a binding name and looks it up, so an application reaching for
    /// `HOT` and an application reaching for its unnamed namespace go through
    /// the same op and differ only in what they asked for. An absent set, or a
    /// name that is not in one, is "service not available" -- which is exactly
    /// the answer an app that did not declare it should get.
    pub fn install(&self, state: &mut OpState) {
        if !self.kv.is_empty() {
            state.put(self.kv.clone());
        }
        if !self.database.is_empty() {
            state.put(self.database.clone());
        }
        if !self.blob.is_empty() {
            state.put(self.blob.clone());
        }
        if let Some(queue) = &self.queue {
            state.put(queue.clone());
        }
    }
}

/// Build the unnamed client and one per named binding, off one base client
/// that already holds the tenant and its token.
///
/// One place rather than three copies of the same loop, because the thing that
/// must not vary between the kinds is exactly this: which namespaces exist and
/// that each is built from the same identity.
fn bind_all<T: Clone>(
    base: &T,
    unnamed: bool,
    names: &[String],
    rebind: impl Fn(&T, Option<&str>) -> Result<T>,
) -> Result<Bound<T>> {
    let mut bound = Bound::default();
    if unnamed {
        bound.unnamed = Some(rebind(base, None)?);
    }
    for name in names {
        bound.named.insert(name.clone(), rebind(base, Some(name))?);
    }
    Ok(bound)
}

/// Remove any reserved platform key from a map that an application authored.
///
/// Returns the keys that were present, so the caller can say which application
/// did it. The *values* are deliberately not returned and must not be logged:
/// they are the application's own text, and a key spelled like one of these
/// can just as easily hold a secret.
///
/// Call it on the manifest's own `env` before merging in what the control
/// plane sent. Without it, a bundle that declares `DEIS_STORE_ENDPOINT` picks
/// where its own KV, SQL and blob clients connect whenever the platform has no
/// address of its own to overwrite it with -- deis-control closes exactly this
/// hole when it assembles a delivery, and a node that takes deployments from
/// anywhere else (a direct `ApplyDeployment`, a bundle recovered from its own
/// disk) has to close it too.
pub fn strip_reserved_env(env: &mut HashMap<String, String>) -> Vec<&'static str> {
    RESERVED_ENV_KEYS
        .into_iter()
        .filter(|key| env.remove(*key).is_some())
        .collect()
}

/// Fill in the platform wiring for a deployment that is about to be loaded.
///
/// Call this on the env map that arrived with the deployment, passing the
/// manifest of the bundle that deployment points at. The two halves come from
/// different places and this is where they meet:
///
/// - the **addresses** are the control plane's to know, and are already in the
///   map under [`STORE_ENDPOINT_KEY`] / [`QUEUE_ENDPOINT_KEY`];
/// - the **declaration** and the **queue consumers** exist only in the
///   manifest, which the node has because it just unpacked the bundle.
///
/// Every reserved key is *cleared* and only then written back, the way
/// deis-control does when it assembles a delivery. Writing without clearing
/// leaves whatever the map already held under a key this manifest has nothing
/// to say about, and each of the four fails differently: a stale
/// `DEIS_SERVICES` builds a storage client for a service the new version no
/// longer declares; a stale `DEIS_QUEUE_CONSUMERS` starts pollers that lease
/// messages nothing will handle; and a stale endpoint -- which an application
/// can put there itself by naming a variable after one -- points its storage
/// client at a host the application chose rather than the one the platform
/// runs.
///
/// Afterwards the map is what `IsolateSpec::env` should carry: the runtime
/// lifts the reserved keys back out before the app sees it.
pub fn wire_deployment_env(manifest: &deis_bundle::Manifest, env: &mut HashMap<String, String>) {
    // Read before the clear: the addresses are the half of the wiring this
    // node cannot recompute from the manifest.
    let endpoints = ServiceEndpoints {
        store: non_empty(env.get(STORE_ENDPOINT_KEY).cloned()),
        queue: non_empty(env.get(QUEUE_ENDPOINT_KEY).cloned()),
    };
    strip_reserved_env(env);
    ServiceWiring::from_manifest(manifest, endpoints).write_to_env(env);
}

/// Build a queue client for a poller, which needs one even when the app's own
/// code never sends a message.
///
/// Charged to the same tenant as the app's own client, and carrying the same
/// queue capability, so a preview polls its own queues and never leases a
/// message production was meant to handle, and deis-queue answers the poller
/// instead of refusing it. Dropping the token here would leave an application
/// whose own `queue.send` works while nothing is ever consumed.
pub(crate) fn consumer_queue_client(
    tenant: &str,
    endpoint: &str,
    token: Option<&str>,
    generation: Option<&str>,
) -> Result<QueueClient> {
    let identity = StorageIdentity::new(tenant, token)
        .with_context(|| format!("building the queue consumer client for {tenant}"))?;
    // Renewed like the app's own queue client: a poller outlives any one
    // token, and one that stopped renewing would stop consuming.
    crate::deployment::capability_renewer::register(tenant, "queue", identity.token_lock());
    QueueClient::connect_lazy(endpoint, &identity)?
        .with_consumer_generation(generation)
        .with_context(|| format!("building the queue consumer client for {tenant}"))
}

#[cfg(test)]
#[path = "bindings_test.rs"]
mod bindings_test;
