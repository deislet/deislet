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
//! Both ends of that encoding live here, in one place, so the side that writes
//! the map and the side that reads it cannot drift apart:
//! [`ServiceWiring::write_to_env`] writes what [`ServiceWiring::take_from_env`]
//! reads. Whoever assembles the deployment payload should call the writer
//! rather than spelling the keys itself.
//!
//! # What gets constructed
//!
//! Only what the app's manifest declared under `services` (see
//! [`deis_bundle::Manifest::services`]). An app that never asked for a
//! database holds no database connection, which is both cheaper and the
//! honest answer to "what can this app reach".
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

use anyhow::{Context, Result};
use deno_core::OpState;
use tonic::transport::{Channel, Endpoint};

use super::blob_client::BlobClient;
use super::db_client::DbClient;
use super::kv_client::KvClient;
use super::queue_client::QueueClient;

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
/// `name[:batch_size]` entries separated by commas, e.g. `jobs:10,emails`.
pub const QUEUE_CONSUMERS_KEY: &str = "DEIS_QUEUE_CONSUMERS";

/// Every key this module claims. Anything spelled this way in a deployment's
/// env map is platform wiring and is removed before the app sees the map.
pub const RESERVED_ENV_KEYS: [&str; 4] = [
    SERVICES_KEY,
    STORE_ENDPOINT_KEY,
    QUEUE_ENDPOINT_KEY,
    QUEUE_CONSUMERS_KEY,
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
    /// The name used in `manifest.services`, in `AppConfig.resources` and in
    /// the reserved env key.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Kv => "kv",
            Self::Database => "database",
            Self::Blob => "blob",
            Self::Queue => "queue",
        }
    }

    /// Parse a declared service name. `db` is accepted as a spelling of
    /// `database` because the JS side calls it `ctx.services.database` while
    /// older configs wrote `db`; anything else is unknown.
    pub fn parse(name: &str) -> Option<Self> {
        match name.trim().to_ascii_lowercase().as_str() {
            "kv" => Some(Self::Kv),
            "database" | "db" | "sql" => Some(Self::Database),
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
}

/// Everything about an application's platform services that has to survive the
/// trip from the control plane to the isolate.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ServiceWiring {
    pub declaration: ServiceDeclaration,
    pub endpoints: ServiceEndpoints,
    pub consumers: Vec<QueueConsumerSpec>,
}

impl ServiceWiring {
    /// Assemble from a manifest plus the addresses the control plane holds.
    pub fn from_manifest(manifest: &deis_bundle::Manifest, endpoints: ServiceEndpoints) -> Self {
        Self {
            declaration: ServiceDeclaration::from_manifest(manifest),
            endpoints,
            consumers: manifest
                .queues
                .iter()
                .map(|queue| QueueConsumerSpec {
                    name: queue.name.clone(),
                    batch_size: queue.batch_size,
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

        Self {
            declaration: ServiceDeclaration::from_names(services.split(',')),
            endpoints: ServiceEndpoints {
                store: non_empty(store),
                queue: non_empty(queue),
            },
            consumers: parse_consumers(&consumers),
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
        if !self.consumers.is_empty() {
            let list = self
                .consumers
                .iter()
                .map(|consumer| match consumer.batch_size {
                    Some(size) => format!("{}:{}", consumer.name, size),
                    None => consumer.name.clone(),
                })
                .collect::<Vec<_>>()
                .join(",");
            env.insert(QUEUE_CONSUMERS_KEY.to_string(), list);
        }
    }

    /// Whether this app asked for anything at all.
    pub fn is_empty(&self) -> bool {
        self.declaration.is_empty() && self.consumers.is_empty()
    }

    /// The queues this app should be polled for: the declared consumers, but
    /// only if the app also declared the queue service and an address exists.
    ///
    /// Returns an empty list, with the reason logged, in every case where
    /// polling could not work -- a consumer that silently fails forever is
    /// worse than one that says why it never started.
    pub fn active_consumers(&self, app_id: &str) -> Vec<QueueConsumerSpec> {
        if self.consumers.is_empty() {
            return Vec::new();
        }

        let Some(_endpoint) = self.endpoints.queue.as_ref() else {
            tracing::warn!(
                app_id,
                queues = self.consumers.len(),
                "App {} declares {} queue consumer(s) but the control plane sent no queue \
                 address; nothing will be consumed and the messages will pile up in deis-queue",
                app_id,
                self.consumers.len()
            );
            return Vec::new();
        };

        if !self.declaration.declares(ServiceKind::Queue) {
            tracing::warn!(
                app_id,
                "App {} declares queue consumers but does not declare the queue service; \
                 nothing will be consumed. Add \"queue\" to the manifest's services",
                app_id
            );
            return Vec::new();
        }

        self.consumers.clone()
    }
}

fn non_empty(value: Option<String>) -> Option<String> {
    value.filter(|v| !v.trim().is_empty())
}

/// Parse `name[:batch_size]` entries. An entry whose batch size does not
/// parse keeps the queue but drops the size, which is the recoverable half:
/// the consumer polls with its own default rather than not at all.
fn parse_consumers(list: &str) -> Vec<QueueConsumerSpec> {
    let mut consumers = Vec::new();
    for entry in list.split(',') {
        let entry = entry.trim();
        if entry.is_empty() {
            continue;
        }
        let (name, batch_size) = match entry.split_once(':') {
            Some((name, size)) => match size.trim().parse::<u32>() {
                Ok(size) => (name.trim(), Some(size)),
                Err(_) => {
                    tracing::warn!(
                        queue = name.trim(),
                        value = size.trim(),
                        "Queue consumer {:?} declares an unreadable batch size {:?}; polling it \
                         with the runtime default instead",
                        name.trim(),
                        size.trim()
                    );
                    (name.trim(), None)
                }
            },
            None => (entry, None),
        };
        if name.is_empty() {
            continue;
        }
        consumers.push(QueueConsumerSpec {
            name: name.to_string(),
            batch_size,
        });
    }
    consumers
}

/// A channel that has not dialled anything yet.
///
/// `connect_lazy` is what keeps a storage outage out of the app-loading path:
/// the channel is usable immediately and only attempts a connection when a
/// call is made on it, retrying on the call after that. Only a malformed
/// address fails here.
fn lazy_channel(endpoint: &str, request_timeout: Option<Duration>) -> Result<Channel> {
    let mut builder = Endpoint::from_shared(endpoint.to_string())
        .with_context(|| format!("{endpoint:?} is not a usable gRPC address"))?
        .connect_timeout(CONNECT_TIMEOUT);
    if let Some(timeout) = request_timeout {
        builder = builder.timeout(timeout);
    }
    Ok(builder.connect_lazy())
}

/// A unary-call channel: bounded per call.
pub(crate) fn unary_channel(endpoint: &str) -> Result<Channel> {
    lazy_channel(endpoint, Some(REQUEST_TIMEOUT))
}

/// A streaming channel: bounded only on the dial.
///
/// A blob download is a stream whose legitimate duration depends on the object
/// size, so a per-call deadline would cut off large-but-healthy transfers. The
/// blob client applies its own size ceiling instead.
pub(crate) fn streaming_channel(endpoint: &str) -> Result<Channel> {
    lazy_channel(endpoint, None)
}

/// The clients one application's isolate holds.
///
/// Absent means "not declared" (or, for queue, "no address"), and every op
/// that looks for a missing client answers "service not available".
#[derive(Clone, Default)]
pub struct AppServices {
    pub kv: Option<KvClient>,
    pub database: Option<DbClient>,
    pub blob: Option<BlobClient>,
    pub queue: Option<QueueClient>,
}

impl AppServices {
    /// Build exactly the clients this app declared.
    ///
    /// Fails only on things that can never work: an app id that cannot be a
    /// gRPC header, or an address that is not a URL. A service that is merely
    /// unreachable is not an error here -- see the module docs.
    ///
    /// Must be called from inside a Tokio runtime: building a lazy channel
    /// registers a connector with the reactor. Every caller in this crate is
    /// on a runtime thread already.
    pub fn connect(app_id: &str, wiring: &ServiceWiring) -> Result<Self> {
        let mut services = Self::default();

        let store_kinds: Vec<ServiceKind> =
            [ServiceKind::Kv, ServiceKind::Database, ServiceKind::Blob]
                .into_iter()
                .filter(|kind| wiring.declaration.declares(*kind))
                .collect();

        if !store_kinds.is_empty() {
            match wiring.endpoints.store.as_deref() {
                Some(endpoint) => {
                    for kind in store_kinds {
                        match kind {
                            ServiceKind::Kv => {
                                services.kv = Some(KvClient::connect_lazy(endpoint, app_id)?)
                            }
                            ServiceKind::Database => {
                                services.database = Some(DbClient::connect_lazy(endpoint, app_id)?)
                            }
                            ServiceKind::Blob => {
                                services.blob = Some(BlobClient::connect_lazy(endpoint, app_id)?)
                            }
                            ServiceKind::Queue => unreachable!("queue is not served by the store"),
                        }
                    }
                    tracing::info!(
                        app_id,
                        endpoint,
                        services = wiring.declaration.to_csv(),
                        "Bound app {} to deis-store at {} for {}",
                        app_id,
                        endpoint,
                        wiring.declaration.to_csv()
                    );
                }
                None => tracing::error!(
                    app_id,
                    services = wiring.declaration.to_csv(),
                    "App {} declares storage services ({}) but the control plane sent no \
                     deis-store address; every ctx.services call will fail with \"service not \
                     available\" until a deployment carries one",
                    app_id,
                    wiring.declaration.to_csv()
                ),
            }
        }

        if wiring.declaration.declares(ServiceKind::Queue) {
            match wiring.endpoints.queue.as_deref() {
                Some(endpoint) => {
                    services.queue = Some(QueueClient::connect_lazy(endpoint, app_id)?);
                    tracing::info!(
                        app_id,
                        endpoint,
                        "Bound app {} to deis-queue at {}",
                        app_id,
                        endpoint
                    );
                }
                None => tracing::error!(
                    app_id,
                    "App {} declares the queue service but the control plane sent no deis-queue \
                     address; ctx.services.queue.send will fail until a deployment carries one",
                    app_id
                ),
            }
        }

        Ok(services)
    }

    /// Whether anything at all was constructed.
    pub fn is_empty(&self) -> bool {
        self.kv.is_none() && self.database.is_none() && self.blob.is_none() && self.queue.is_none()
    }

    /// Hand the clients to the isolate.
    ///
    /// Only what exists is inserted; the ops look the clients up by type and
    /// treat an absent one as "service not available", which is exactly the
    /// answer an app that did not declare the service should get.
    pub fn install(&self, state: &mut OpState) {
        if let Some(kv) = &self.kv {
            state.put(kv.clone());
        }
        if let Some(database) = &self.database {
            state.put(database.clone());
        }
        if let Some(blob) = &self.blob {
            state.put(blob.clone());
        }
        if let Some(queue) = &self.queue {
            state.put(queue.clone());
        }
    }
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
/// Afterwards the map is what `IsolateSpec::env` should carry: the runtime
/// lifts the reserved keys back out before the app sees it.
pub fn wire_deployment_env(manifest: &deis_bundle::Manifest, env: &mut HashMap<String, String>) {
    let endpoints = ServiceEndpoints {
        store: non_empty(env.get(STORE_ENDPOINT_KEY).cloned()),
        queue: non_empty(env.get(QUEUE_ENDPOINT_KEY).cloned()),
    };
    ServiceWiring::from_manifest(manifest, endpoints).write_to_env(env);
}

/// Build a queue client for a poller, which needs one even when the app's own
/// code never sends a message.
pub(crate) fn consumer_queue_client(app_id: &str, endpoint: &str) -> Result<QueueClient> {
    QueueClient::connect_lazy(endpoint, app_id)
        .with_context(|| format!("building the queue consumer client for app {app_id}"))
}

#[cfg(test)]
#[path = "bindings_test.rs"]
mod bindings_test;
