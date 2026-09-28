//! Deployment delivery: telling nodes what to run.
//!
//! The rule is small message, large data fetched by the receiver
//! (docs/implementation-plan.zh.md section 4). The control plane sends a
//! pointer - app, version, environment, bundle URL, digest, environment
//! variables - and the node downloads the bundle over HTTP itself. The
//! bundle never travels inside a gRPC message: an application with real
//! static assets is megabytes, and one call carrying all of it hit the
//! tonic message ceiling instead of failing gracefully.
//!
//! Three things start a delivery:
//!
//! * a deployment, which moves an environment onto another version;
//! * a rollback, which is the same operation aimed backwards;
//! * a secret change, which produces no new version at all but must still
//!   reach the nodes, because the value they hold in memory is now stale.
//!
//! Secrets are decrypted here and nowhere else, at the moment of sending.
//! They are never stored decrypted and never logged - which is why
//! [`DeploymentPayload`] has a hand-written `Debug` and why the proto
//! request built from it must not be printed.
//!
//! There are exactly **two** places where a decrypted value leaves this
//! process, and both of them go to a node:
//!
//! * the `ApplyDeployment` push built by [`DeploymentPayload::to_request`];
//! * the answer to `GET /api/nodes/:id/assignments`, which a node that
//!   restarts uses to come back with the environment it had.
//!
//! Both are served on the machine channel only: a caller proves a certificate
//! there and may only ask for its own environment.
//! No other read API returns a value: the dashboard and the CLI can write
//! and delete a secret and list its name, never read it back
//! (docs/architecture.zh.md section 8).

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use axum::http::HeaderMap;
use serde::Serialize;
use tracing::{debug, error, info, warn};
use utoipa::ToSchema;

use crate::capability_issuer::{AUD_QUEUE, CapabilityIssuer};
use crate::catalog::secrets::secret_revision_aad;
use crate::catalog::{
    CatalogError, CatalogResult, CatalogStore, DeclaredCompatibility, DeclaredLimits, Environment,
    SecretsBackend, Version, declared_compatibility, declared_limits,
};
use crate::runtime_client::{RuntimeNodeManager, proto};
use deis_capability::resources_for_audience;
use deis_wire::CanonicalTenantId;

/// How the control plane names itself, once for each audience that is given
/// an absolute URL.
///
/// Nodes fetch bundles over HTTP from an absolute URL, so both the
/// assignment list and every deployment push have to carry an address that
/// is reachable from the node, not from the control plane's own point of
/// view.
///
/// There are two such addresses because there are two listeners. A browser
/// finds the dashboard and the human API on the address an operator publishes
/// as `public_base_url`; a node downloads its bundle from the machine channel,
/// which is a separate socket with a separate port and a mutual-TLS handshake
/// no browser would survive. Handing one audience the other's address gives it
/// a URL it cannot use, so the two are resolved apart and every caller of
/// [`bundle_url`] picks the one it is building for - see
/// [`Self::resolve_human`] and [`Self::resolve_machine`].
///
/// The machine listener is still being built, so no installation configures
/// one yet and both resolutions currently answer with the same address. The
/// split is made now because the day the bundle route moves, the code that
/// hands a node a URL must already be pointing at the listener that serves
/// it, and a fleet learns otherwise only by failing to download anything.
#[derive(Debug, Clone)]
pub struct PublicAddress {
    /// Explicit `public_base_url` from the config. Always wins for a human.
    pub configured: Option<String>,
    /// Last resort, derived from the bind address.
    pub fallback: String,
    /// The machine listener's address, present exactly when `[machine]` is
    /// configured. `None` means this installation has no machine channel yet,
    /// and everything still runs on the one listener the human uses.
    pub machine: Option<MachineAddress>,
}

/// Where the machine channel is reachable from, as a node sees it.
///
/// Separate from the human address rather than two more `Option` fields on it,
/// because "no machine listener at all" and "a machine listener whose public
/// address the operator did not spell out" are different situations with
/// different answers, and flattening them would let the two disagree.
#[derive(Debug, Clone)]
pub struct MachineAddress {
    /// Explicit `machine.public_base_url` from the config. Always wins.
    pub configured: Option<String>,
    /// Derived from the machine listener's bind address and port.
    pub fallback: String,
}

impl MachineAddress {
    /// The base a node downloads bundles from once `[machine]` exists.
    fn resolve(&self) -> String {
        if let Some(configured) = &self.configured {
            return configured.trim_end_matches('/').to_string();
        }
        self.fallback.clone()
    }
}

impl PublicAddress {
    /// Resolve the base URL a person's browser is pointed at.
    ///
    /// The configured value wins. Without one, use the bind-derived fallback.
    ///
    /// The request's `Host` header is deliberately not consulted. It names the
    /// address the API caller used, not necessarily one a runtime node can
    /// reach, and it is caller-controlled. Putting it into a deployment would
    /// let a deploy-scoped caller make every node issue a request to an
    /// arbitrary host.
    pub fn resolve_human(&self, _headers: &HeaderMap) -> String {
        if let Some(configured) = &self.configured {
            return configured.trim_end_matches('/').to_string();
        }
        warn!(
            fallback = %self.fallback,
            "public_base_url is unset, so bundle URLs use the bind-derived fallback; nodes on another host may not be able to reach it"
        );
        self.fallback.clone()
    }

    /// Resolve the base URL a node downloads bundles from.
    ///
    /// `machine.public_base_url` wins, then the machine listener's own bind
    /// address, and only when there is no machine listener at all does this
    /// fall back to the human base.
    ///
    /// The fallback runs in that direction and never the other. While
    /// `[machine]` is absent the bundle route is still served by the one
    /// listener this process has, so borrowing the human base is exactly
    /// today's behaviour and this whole split changes nothing. A human base
    /// that fell back to the machine one would instead hand a browser the
    /// address of a socket that demands a client certificate, which no browser
    /// in this fleet holds.
    pub fn resolve_machine(&self, headers: &HeaderMap) -> String {
        match &self.machine {
            Some(machine) => machine.resolve(),
            None => self.resolve_human(headers),
        }
    }

    /// The same machine base for a caller holding no request.
    ///
    /// Restart reconciliation rebuilds a payload without any HTTP request
    /// behind it. The headers were never consulted anyway, so the machine
    /// branch is the same answer either way. The human fallback differs from
    /// [`Self::resolve_human`] in two ways, both inherited from the code this
    /// replaced rather than chosen here: it does not log the warning, because
    /// reconciliation runs on a timer and would repeat that line for as long
    /// as the address stayed unset; and it does not trim a trailing slash off
    /// a configured value. The second is a real inconsistency and is left
    /// alone deliberately - changing it here would alter the address a
    /// reconciling node is given, which is not what this change is for.
    pub fn resolve_machine_without_request(&self) -> String {
        match &self.machine {
            Some(machine) => machine.resolve(),
            None => self
                .configured
                .clone()
                .unwrap_or_else(|| self.fallback.clone()),
        }
    }
}

/// Absolute URL a node uses to download one version's bundle.
pub fn bundle_url(base: &str, app_id: &str, version_id: &str) -> String {
    format!("{base}/api/apps/{app_id}/versions/{version_id}/bundle")
}

/// What the platform tells a node about storage: where the services live.
///
/// The control plane owns the addresses; a node never reads them from its own
/// file (docs/architecture.zh.md section 6). They travel in the same `env`
/// map as an application's variables, under reserved keys spelled once in
/// [`deis_wire::conventions`], and deis-runtime lifts them back out before the
/// application sees the map.
///
/// No credential travels with them. A node proves itself to deis-store and
/// deis-queue with capabilities it asks for itself over the machine channel
/// (`IssueCapability`, subject = the node's own SPIFFE name), so nothing in a
/// deployment message can be replayed as access to a tenant.
///
/// An unset address means "this platform has no such service": the deployment
/// still goes out, the application still loads, and only its `ctx.services`
/// calls fail - which is better than refusing to deploy an application that
/// declared a service it never actually calls.
#[derive(Clone)]
pub struct StorageWiring {
    /// deis-store: KV, per-app SQL and blobs.
    pub store: Option<String>,
    /// deis-queue.
    pub queue: Option<String>,
    /// Signs the capabilities this control plane presents on its own calls
    /// to deis-queue (the consumer-generation barrier). Never sent to a node
    /// and never logged.
    issuer: CapabilityIssuer,
    /// What this control plane checks deis-queue's certificate against when
    /// it calls the queue itself: its own machine identity. Unset means it
    /// cannot call the queue at all, rather than calling it unverified.
    service_trust: Option<deis_identity::ServiceTrust>,
}

impl std::fmt::Debug for StorageWiring {
    /// The addresses are plain configuration and print; the issuer is the one
    /// value in this struct that can mint a token for *any* tenant, so it is
    /// left out entirely. A derived `Debug` would put it in whatever log line
    /// printed a payload.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StorageWiring")
            .field("store", &self.store)
            .field("queue", &self.queue)
            .field("service_trust", &self.service_trust)
            .finish_non_exhaustive()
    }
}

impl StorageWiring {
    /// Read storage endpoints from the control plane's configuration, dropping
    /// blank entries so an empty string in the file does not become an address
    /// nothing can dial.
    pub fn new(config: &crate::config::ControlConfig, issuer: CapabilityIssuer) -> Self {
        let clean = |value: &Option<String>| {
            value
                .as_ref()
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty())
        };
        Self {
            store: clean(&config.store_endpoint),
            queue: clean(&config.queue_endpoint),
            issuer,
            service_trust: None,
        }
    }

    /// Attach what deis-queue's certificate is checked against.
    pub fn with_service_trust(mut self, trust: Option<deis_identity::ServiceTrust>) -> Self {
        self.service_trust = trust;
        self
    }

    /// A channel to deis-queue that dials on first use and refuses any
    /// server whose certificate does not name a queue of this fleet.
    ///
    /// The queue serves TLS only, so an `http://` address is refused here
    /// rather than dialled into a handshake failure; so is a control plane
    /// with no identity to check the certificate against, since sending a
    /// capability to an unverified peer is what the TLS is there to prevent.
    fn queue_channel(&self) -> anyhow::Result<tonic::transport::Channel> {
        let endpoint = self
            .queue
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("queue endpoint is not configured"))?;
        if !endpoint.starts_with("https://") {
            anyhow::bail!(
                "queue_endpoint {endpoint:?} is not https://; deis-queue serves TLS only, and a \
                 capability is never sent to it in the clear"
            );
        }
        let trust = self.service_trust.as_ref().ok_or_else(|| {
            anyhow::anyhow!(
                "this control plane has no machine identity to check deis-queue's certificate \
                 against, so it does not call the queue"
            )
        })?;
        Ok(
            tonic::transport::Endpoint::from_shared(endpoint.to_string())?
                .connect_timeout(std::time::Duration::from_secs(3))
                .timeout(std::time::Duration::from_secs(10))
                .connect_with_connector_lazy(trust.dialler(
                    deis_identity::ServiceKind::Queue,
                    &[deis_identity::Protocol::Http2],
                )),
        )
    }

    /// The capability this control plane presents to deis-queue for one
    /// tenant.
    ///
    /// It names the canonical tenant, the `queue` audience and the `queue`
    /// binding; deis-queue acts for exactly the tenant it names, so nothing
    /// in the request has to say which one.
    pub async fn queue_token_for(&self, tenant: &CanonicalTenantId) -> anyhow::Result<String> {
        self.issuer
            .issue(
                AUD_QUEUE,
                &format!("spiffe://deislet/control/{}", tenant.application_id()),
                resources_for_audience(&tenant.to_string(), AUD_QUEUE),
            )
            .await
            .map_err(|error| anyhow::anyhow!("could not issue a queue capability: {error}"))
    }

    /// Add the addresses to a deployment's environment map.
    ///
    /// Every reserved key is *removed* first and only then written back,
    /// which is the part that matters when a value is not configured: an
    /// application that declares `DEIS_STORE_ENDPOINT` in its own manifest
    /// would otherwise keep that value - the platform having nothing to
    /// overwrite it with - and the node would build its KV, SQL and blob
    /// clients against a host the application chose.
    fn write_into(&self, app_id: &str, env: &mut BTreeMap<String, String>) {
        let mut put = |key: &str, value: &Option<String>| {
            if env.remove(key).is_some() {
                warn!(
                    app_id = %app_id,
                    key = %key,
                    replaced_by_platform = value.is_some(),
                    "The manifest declares a variable named like a reserved platform key; it is dropped, so the value in the bundle never reaches the application and can never point its storage client at another host"
                );
            }
            if let Some(value) = value {
                env.insert(key.to_string(), value.clone());
            }
        };
        put(deis_wire::conventions::STORE_ENDPOINT_ENV, &self.store);
        put(deis_wire::conventions::QUEUE_ENDPOINT_ENV, &self.queue);
    }

    /// True when no address is configured, which is worth saying once at
    /// startup rather than discovering per deployment.
    pub fn has_no_endpoints(&self) -> bool {
        self.store.is_none() && self.queue.is_none()
    }

    /// Whether this version has asynchronous consumers whose lease admission
    /// must participate in deployment activation.
    pub fn requires_consumer_fence(&self, version: &Version) -> CatalogResult<bool> {
        if self.queue.is_none() {
            return Ok(false);
        }
        let manifest = deis_bundle::Manifest::parse(&version.manifest)
            .map_err(|error| CatalogError::BundleStorage(error.to_string()))?;
        Ok(!manifest.queues.is_empty())
    }

    /// Close queue Pull admission and wait for the previous generation's
    /// leases without admitting the candidate before pointer commit.
    pub async fn prepare_consumer_generation(
        &self,
        tenant: &CanonicalTenantId,
        deployment_id: &str,
        generation: &str,
        expected_previous_generation: Option<&str>,
        expected_previous_deployment_id: Option<&str>,
    ) -> anyhow::Result<deis_queue::proto::queue::PrepareConsumerGenerationResponse> {
        use deis_queue::proto::queue::{
            PrepareConsumerGenerationRequest, queue_client::QueueClient,
        };

        let channel = self.queue_channel()?;
        let mut request = tonic::Request::new(PrepareConsumerGenerationRequest {
            deployment_id: deployment_id.to_string(),
            generation: generation.to_string(),
            expected_previous_generation: expected_previous_generation
                .unwrap_or_default()
                .to_string(),
            expected_previous_deployment_id: expected_previous_deployment_id.map(str::to_string),
        });
        self.stamp_queue_capability(&mut request, tenant).await?;
        Ok(QueueClient::new(channel)
            .prepare_consumer_generation(request)
            .await?
            .into_inner())
    }

    /// Admit a queue generation whose pre-commit barrier completed.
    pub async fn activate_consumer_generation(
        &self,
        tenant: &CanonicalTenantId,
        deployment_id: &str,
        generation: &str,
    ) -> anyhow::Result<deis_queue::proto::queue::ActivateConsumerGenerationResponse> {
        use deis_queue::proto::queue::{
            ActivateConsumerGenerationRequest, queue_client::QueueClient,
        };
        let channel = self.queue_channel()?;
        let mut request = tonic::Request::new(ActivateConsumerGenerationRequest {
            deployment_id: deployment_id.to_string(),
            generation: generation.to_string(),
        });
        self.stamp_queue_capability(&mut request, tenant).await?;
        Ok(QueueClient::new(channel)
            .activate_consumer_generation(request)
            .await?
            .into_inner())
    }

    /// Cancel an uncommitted queue barrier and restore old Pull admission.
    pub async fn abort_consumer_generation(
        &self,
        tenant: &CanonicalTenantId,
        deployment_id: &str,
        generation: &str,
    ) -> anyhow::Result<deis_queue::proto::queue::AbortConsumerGenerationResponse> {
        use deis_queue::proto::queue::{AbortConsumerGenerationRequest, queue_client::QueueClient};
        let channel = self.queue_channel()?;
        let mut request = tonic::Request::new(AbortConsumerGenerationRequest {
            deployment_id: deployment_id.to_string(),
            generation: generation.to_string(),
        });
        self.stamp_queue_capability(&mut request, tenant).await?;
        Ok(QueueClient::new(channel)
            .abort_consumer_generation(request)
            .await?
            .into_inner())
    }

    /// Put this control plane's queue capability in `Authorization: Bearer`,
    /// the one header deis-queue reads it from.
    async fn stamp_queue_capability<T>(
        &self,
        request: &mut tonic::Request<T>,
        tenant: &CanonicalTenantId,
    ) -> anyhow::Result<()> {
        let token = self.queue_token_for(tenant).await?;
        request.metadata_mut().insert(
            "authorization",
            format!("Bearer {token}").parse().map_err(|error| {
                anyhow::anyhow!("queue capability is not valid gRPC metadata: {error}")
            })?,
        );
        Ok(())
    }
}

/// Why a delivery is happening. Only used for logs and for the answer the
/// API gives, but it is the difference between "someone deployed" and "a
/// secret changed under a version that did not move".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum DeliveryReason {
    Deployment,
    Rollback,
    SecretChange,
}

impl DeliveryReason {
    pub fn as_str(self) -> &'static str {
        match self {
            DeliveryReason::Deployment => "deployment",
            DeliveryReason::Rollback => "rollback",
            DeliveryReason::SecretChange => "secret change",
        }
    }
}

/// Everything one node needs in order to run one environment of one app.
///
/// `secret_values_by_revision` holds decrypted secret values, which is why
/// this type exists instead of a bare proto message: it does not derive
/// `Debug`, `Serialize` or `Clone`-into-a-log. Those values never reach the
/// wire from here - [`Self::to_request`] carries only the snapshot that names
/// their revisions, and a node fetches the values through
/// `FetchSecretValues`.
pub struct DeploymentPayload {
    pub app_id: String,
    /// Permanent application identity. The legacy name above remains for
    /// routing during a rolling migration; storage authority is derived from
    /// this id and `canonical_environment_id` once the migration fence cuts
    /// the environment over.
    pub canonical_application_id: deis_wire::CanonicalId,
    pub version_id: String,
    /// Environment *name*, never the id: this is what application code
    /// branches on.
    pub environment: String,
    /// Permanent identity of the environment row.
    pub canonical_environment_id: deis_wire::CanonicalId,
    pub bundle_url: String,
    pub bundle_sha: String,
    /// Plain variables from the manifest. The application's own
    /// environment, and nothing the platform added; never a secret value.
    env: BTreeMap<String, String>,
    /// Which secrets the snapshot binds. Names only - they are safe to log,
    /// the values are not.
    secret_names: Vec<String>,
    /// Standard declaration and snapshot are public only to the internal
    /// delivery/runtime boundary. Neither contains secret plaintext.
    environment_declarations: Option<deis_wire::EnvironmentDeclarations>,
    environment_snapshot: Option<deis_wire::EnvironmentBindingSnapshot>,
    /// Canonical wire documents are prepared while the payload can still
    /// fail. `to_request` must never turn a serialization error into three
    /// empty fields that look like a legacy deployment.
    environment_declarations_json: Option<String>,
    environment_snapshot_json: Option<String>,
    environment_snapshot_id: Option<String>,
    /// The only standard secret plaintext carrier, keyed by immutable
    /// revision rather than name. Never serialized to management APIs/logs.
    pub secret_values_by_revision: BTreeMap<String, String>,
    /// The storage addresses and the secret this tenant's capability token
    /// is derived from. Kept apart from `env` so the report and the log
    /// lines stay about the application, and merged in only when the wire
    /// message is built.
    storage: StorageWiring,
    /// What this version's manifest asked to be held to, or `None` when it
    /// asked for nothing and takes the node's own ceilings.
    ///
    /// Plain configuration, not a secret: it is printed in this type's
    /// `Debug`, logged with every delivery, and served in the assignment list
    /// that a restarting node reads. Every value is clamped on the node
    /// (`ApplicationManager::effective_limits`), so what travels here is a
    /// request to be held to less, never a grant of more.
    pub limits: Option<DeclaredLimits>,
    /// Which day's platform this version was written against, *verbatim* from
    /// its manifest.
    ///
    /// The declaration, not this control plane's resolution of it, and that is
    /// load bearing. Resolved answers are lossy in one direction: a manifest
    /// naming a date and then a flag that holds one behaviour *back* resolves
    /// to a set of enabled names that no longer mentions the flag, and a node
    /// re-reading that set would apply the date and turn the behaviour on
    /// again. Sending what the author wrote lets each build answer it with its
    /// own table, which is the whole point of a fleet where the control plane
    /// and a node may be on different versions.
    ///
    /// Carried at all so a node can refuse a deployment it cannot honour
    /// before it downloads a bundle. Not an override: the same declaration is
    /// inside the bundle, and once the node has unpacked it that copy is the
    /// authority, which keeps a version's semantics attached to the version
    /// rather than to the message that happened to deliver it.
    pub compatibility: DeclaredCompatibility,
    /// Durable operation identity and immutable route generation. Kept out of
    /// application environment material and attached only by the activation
    /// coordinator after it has selected a deployment identity.
    deployment_id: Option<String>,
    routing_generation: Option<String>,
    action: proto::DeploymentAction,
}

impl std::fmt::Debug for DeploymentPayload {
    /// Renders the shape, never the contents. A payload that printed its
    /// `env` map would put every secret of an environment into whatever
    /// log line touched it.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DeploymentPayload")
            .field("app_id", &self.app_id)
            .field("canonical_application_id", &self.canonical_application_id)
            .field("version_id", &self.version_id)
            .field("environment", &self.environment)
            .field("canonical_environment_id", &self.canonical_environment_id)
            .field("bundle_url", &self.bundle_url)
            .field("bundle_sha", &self.bundle_sha)
            .field("variables", &self.variable_names())
            .field("secrets", &self.secret_names)
            .field("values", &"<redacted>")
            .field("environment_snapshot_id", &self.environment_snapshot_id)
            .field("deployment_id", &self.deployment_id)
            .field("routing_generation", &self.routing_generation)
            .field("action", &self.action.as_str_name())
            .field("store_endpoint", &self.storage.store)
            .field("queue_endpoint", &self.storage.queue)
            .field("limits", &self.limits)
            .field("compatibility", &self.compatibility)
            .finish()
    }
}

impl DeploymentPayload {
    /// Bind this payload to one durable deployment instruction. The
    /// generation is immutable and must be the same value published in the
    /// routing table.
    pub fn set_instruction(
        &mut self,
        deployment_id: impl Into<String>,
        routing_generation: impl Into<String>,
        action: proto::DeploymentAction,
    ) {
        self.deployment_id = Some(deployment_id.into());
        self.routing_generation = Some(routing_generation.into());
        self.action = action;
    }

    pub fn deployment_id(&self) -> Option<&str> {
        self.deployment_id.as_deref()
    }

    pub fn routing_generation(&self) -> Option<&str> {
        self.routing_generation.as_deref()
    }

    pub fn action(&self) -> proto::DeploymentAction {
        self.action
    }

    /// Names of the entries that came from the manifest rather than from a
    /// secret. Safe to log.
    pub fn variable_names(&self) -> Vec<&str> {
        if let Some(snapshot) = &self.environment_snapshot {
            return snapshot
                .bindings
                .iter()
                .filter_map(|binding| match binding {
                    deis_wire::EnvironmentBinding::Config(config) => Some(config.name.as_str()),
                    deis_wire::EnvironmentBinding::Secret(_) => None,
                })
                .collect();
        }
        self.env
            .keys()
            .map(String::as_str)
            .filter(|name| !self.secret_names.iter().any(|s| s == name))
            .collect()
    }

    pub fn secret_names(&self) -> &[String] {
        &self.secret_names
    }

    pub fn environment_snapshot(&self) -> Option<&deis_wire::EnvironmentBindingSnapshot> {
        self.environment_snapshot.as_ref()
    }

    /// Secret heads observed while a fresh standard snapshot was prepared.
    /// Absence is meaningful for optional declarations: activation must fail
    /// if a concurrent writer creates that head before the pointer commits.
    pub fn expected_secret_heads(&self) -> Option<BTreeMap<String, Option<String>>> {
        let declarations = self.environment_declarations.as_ref()?;
        let snapshot = self.environment_snapshot.as_ref()?;
        let selected = snapshot
            .bindings
            .iter()
            .filter_map(|binding| match binding {
                deis_wire::EnvironmentBinding::Secret(secret) => {
                    Some((secret.name.as_str(), secret.revision.as_str()))
                }
                deis_wire::EnvironmentBinding::Config(_) => None,
            })
            .collect::<BTreeMap<_, _>>();
        Some(
            declarations
                .declarations
                .iter()
                .filter(|declaration| declaration.kind == deis_wire::EnvironmentKind::Secret)
                .map(|declaration| {
                    (
                        declaration.name.clone(),
                        selected
                            .get(declaration.name.as_str())
                            .map(|revision| (*revision).to_string()),
                    )
                })
                .collect(),
        )
    }

    pub fn standard_environment_for_wire(
        &self,
    ) -> Option<(
        deis_wire::EnvironmentDeclarations,
        deis_wire::EnvironmentBindingSnapshot,
        String,
    )> {
        let declarations = self.environment_declarations.clone()?;
        let snapshot = self.environment_snapshot.clone()?;
        let identity = self.environment_snapshot_id.clone()?;
        Some((declarations, snapshot, identity))
    }

    pub fn variable_count(&self) -> usize {
        self.variable_names().len()
    }

    pub fn secret_count(&self) -> usize {
        self.secret_names.len()
    }

    /// The complete environment a node should see: the application's own
    /// variables and the platform's storage addresses.
    ///
    /// No secret value and no credential is in it: secrets reach a node
    /// through the snapshot and `FetchSecretValues`, and storage
    /// capabilities through the node's own `IssueCapability` calls.
    pub fn env_for_wire(&self) -> BTreeMap<String, String> {
        let mut env: BTreeMap<String, String> = self.env.clone();
        // Written last, so an application variable spelled like a reserved
        // key cannot point the app's storage client somewhere else.
        self.storage.write_into(&self.app_id, &mut env);
        env
    }

    /// Build the wire message. It carries the application's variables but
    /// no secret value; still, hand it straight to the RPC rather than a log.
    pub fn to_request(&self) -> proto::ApplyDeploymentRequest {
        let env = self.env_for_wire();

        proto::ApplyDeploymentRequest {
            app_id: self.app_id.clone(),
            version_id: self.version_id.clone(),
            environment: self.environment.clone(),
            bundle_url: self.bundle_url.clone(),
            bundle_sha: self.bundle_sha.clone(),
            env: env.into_iter().collect(),
            limits: self.limits.as_ref().map(limits_to_wire),
            compatibility: Some(compatibility_to_wire(&self.compatibility)),
            canonical_application_id: self.canonical_application_id.to_string(),
            canonical_environment_id: self.canonical_environment_id.to_string(),
            environment_declarations_json: self
                .environment_declarations_json
                .clone()
                .unwrap_or_default(),
            environment_snapshot_json: self.environment_snapshot_json.clone().unwrap_or_default(),
            environment_snapshot_id: self.environment_snapshot_id.clone().unwrap_or_default(),
            deployment_id: self.deployment_id.clone().unwrap_or_default(),
            routing_generation: self.routing_generation.clone().unwrap_or_default(),
            action: self.action as i32,
        }
    }
}

/// Put a version's compatibility declaration on the wire, word for word.
///
/// Deliberately not this control plane's resolution of it. Two builds resolve
/// the same declaration with their own tables and that is intended; a resolved
/// list, by contrast, cannot express "the date would turn this on and the
/// author asked for it to stay off", so a node re-reading one would silently
/// turn the behaviour back on.
///
/// Always sent, including for a version that declares nothing: an empty date
/// with no flags is a complete answer ("the oldest behaviour"), and an absent
/// message would be indistinguishable from an older control plane that has no
/// opinion, which would leave a node guessing.
fn compatibility_to_wire(declared: &DeclaredCompatibility) -> proto::CompatibilityDeclaration {
    proto::CompatibilityDeclaration {
        date: declared
            .date
            .map(|date| date.to_string())
            .unwrap_or_default(),
        flags: declared.flags.clone(),
    }
}

pub fn decrypt_secret(
    backend: &SecretsBackend,
    env_id: &str,
    stored: &crate::catalog::store::SecretCiphertext,
) -> CatalogResult<String> {
    let aad = match stored.encryption_scheme.as_str() {
        "revision-v1" => secret_revision_aad(env_id, &stored.name, &stored.revision),
        _ => {
            return Err(CatalogError::SecretsUnavailable(
                "stored secret uses an unsupported encryption scheme".to_string(),
            ));
        }
    };
    let plaintext = backend.cipher()?.decrypt(&stored.ciphertext, &aad)?;
    String::from_utf8(plaintext).map_err(|_| {
        CatalogError::SecretsUnavailable(format!(
            "secret {} does not decrypt to text and cannot be materialized",
            stored.name
        ))
    })
}

/// Translate a manifest's `limits` block into the wire message.
///
/// The proto uses `0` for "not set" on every numeric field, which is why the
/// declaration side refuses an explicit zero: the two would be
/// indistinguishable here, and an application that asked for a zero budget
/// would quietly get the node's default instead of being told its manifest is
/// wrong.
///
/// `max_concurrency` is left at zero on purpose. It is field 3 of the message
/// and nothing on a node enforces it (see `deis.management.ResourceLimits`);
/// sending a number for something that is not applied is how a platform ends
/// up documenting a limit it does not have.
fn limits_to_wire(limits: &DeclaredLimits) -> proto::ResourceLimits {
    proto::ResourceLimits {
        memory_mb: limits.memory_mb.unwrap_or(0) as i32,
        max_concurrency: 0,
        resource_table_max: limits.resource_table_max.unwrap_or(0) as i32,
        timeout_ms: limits.timeout_ms.unwrap_or(0) as i64,
        rate_limit_ops_per_sec: limits.rate_limit_ops_per_sec.unwrap_or(0.0),
        cpu_time_limit_ms: limits.cpu_time_limit_ms.unwrap_or(0) as i64,
        max_outbound_connections: limits.max_outbound_connections.unwrap_or(0) as i64,
    }
}

/// One node that did not take the deployment.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct NodeFailure {
    pub node: String,
    /// Transport error or the node's own rejection message. Never carries
    /// anything from the env map.
    pub error: String,
}

/// What came of one delivery. Returned to the API caller so a deploy says
/// where it actually landed instead of only that a row was written.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct DeliveryReport {
    pub reason: DeliveryReason,
    /// Wire phase this report actually observed. This prevents a successful
    /// PREPARE acknowledgement from being presented as though production had
    /// already activated.
    pub phase: &'static str,
    pub app_id: String,
    pub environment: String,
    pub version_id: String,
    /// Nodes that acknowledged the requested phase with the exact plan and
    /// generation identity.
    pub delivered: Vec<String>,
    /// Nodes that did not. They keep serving whatever they had until they
    /// refetch their assignments.
    pub failed: Vec<NodeFailure>,
    /// Set when nothing was sent at all, with the reason. A deployment
    /// coordinator must not read this as successful prepare.
    pub skipped: Option<String>,
}

/// Result of preparing a secret-head change. Standard deployments return a
/// candidate payload before either the secret head or production pointer is
/// changed; legacy/undeployed cases finish locally and retain their explicit
/// delivery report.
pub enum SecretChangePreparation {
    Finished(DeliveryReport),
    Prepared {
        version: Version,
        payload: Box<DeploymentPayload>,
    },
}

impl DeliveryReport {
    /// True when every node that was asked took the version.
    pub fn fully_delivered(&self) -> bool {
        !self.delivered.is_empty() && self.failed.is_empty() && self.skipped.is_none()
    }
}

/// What came of taking an application off the fleet.
///
/// Deleting an application is the one operation whose failure is silent
/// without this: the rows are gone from the catalog, so nothing will ever
/// mention the app again, while a node that was not reached keeps serving
/// it - with its code, its secrets in memory and its queue consumers still
/// polling - until that process restarts.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct RemovalReport {
    pub app_id: String,
    /// Environment names the catalog had for this app when it was deleted.
    /// Informational: the nodes are told to drop every environment of it.
    pub environments: Vec<String>,
    /// Nodes that dropped it and no longer serve it.
    pub unloaded: Vec<String>,
    /// Nodes that did not. **These still serve the deleted application.**
    pub failed: Vec<NodeFailure>,
    /// Set when nothing was sent at all, with the reason.
    pub skipped: Option<String>,
}

impl RemovalReport {
    /// True when no node is left serving the application.
    pub fn fully_removed(&self) -> bool {
        self.failed.is_empty()
    }
}

/// Builds deployment payloads and pushes them to the fleet.
///
/// Every node runs every application (architecture principle 2), so a
/// delivery goes to all configured nodes; there is no placement decision
/// to make here.
pub struct DeploymentDeliverer {
    store: Arc<CatalogStore>,
    secrets: Arc<SecretsBackend>,
    nodes: Arc<RuntimeNodeManager>,
    address: Arc<PublicAddress>,
    storage: StorageWiring,
}

impl DeploymentDeliverer {
    const CONTESTED_TARGET_PREFIX: &'static str = "contested:";

    pub fn new(
        store: Arc<CatalogStore>,
        secrets: Arc<SecretsBackend>,
        nodes: Arc<RuntimeNodeManager>,
        address: Arc<PublicAddress>,
        storage: StorageWiring,
    ) -> Self {
        Self {
            store,
            secrets,
            nodes,
            address,
            storage,
        }
    }

    /// The fleet this deliverer pushes to.
    ///
    /// Handed out so the machine channel can register a node against the same
    /// manager the pushes read, rather than a second one built from the same
    /// configuration: two would be two fleets that could disagree about which
    /// addresses exist.
    pub fn nodes(&self) -> &Arc<RuntimeNodeManager> {
        &self.nodes
    }

    /// The base URL bundle links in this delivery should carry.
    ///
    /// Everything this deliverer sends is read by a node, so this is what
    /// almost every caller wants. The one exception is the upload answer,
    /// which shows a person the link they just created and takes
    /// [`Self::human_base_url`].
    pub fn machine_base_url(&self, headers: &HeaderMap) -> String {
        self.address.resolve_machine(headers)
    }

    /// The base URL a link shown to a person should carry.
    ///
    /// It lives next to [`Self::machine_base_url`] only because this is where
    /// the resolved [`PublicAddress`] is held; nothing this deliverer pushes
    /// to a node uses it.
    pub fn human_base_url(&self, headers: &HeaderMap) -> String {
        self.address.resolve_human(headers)
    }

    /// Stable address used by restart reconciliation, where no original HTTP
    /// request headers exist.
    ///
    /// A node is what reads it, so it resolves the machine audience like
    /// every other push - see [`PublicAddress::resolve_machine_without_request`]
    /// for the two ways its human fallback differs from the one a live
    /// request takes.
    pub fn machine_reconciliation_base_url(&self) -> String {
        self.address.resolve_machine_without_request()
    }

    pub fn requires_consumer_fence(&self, version: &Version) -> CatalogResult<bool> {
        self.storage.requires_consumer_fence(version)
    }

    /// The canonical tenant of `environment`: what a store or queue
    /// capability for it names.
    async fn canonical_tenant(&self, environment: &Environment) -> CatalogResult<CanonicalTenantId> {
        let app = self.store.get_app(&environment.app_id).await?;
        Ok(CanonicalTenantId::new(app.canonical_id, environment.canonical_id))
    }

    pub async fn prepare_consumer_generation(
        &self,
        environment: &Environment,
        plan: &crate::catalog::DeploymentPlan,
    ) -> anyhow::Result<deis_queue::proto::queue::PrepareConsumerGenerationResponse> {
        let tenant = self.canonical_tenant(environment).await?;
        let progress = self
            .storage
            .prepare_consumer_generation(
                &tenant,
                &plan.id,
                &plan.routing_generation.to_string(),
                plan.previous_generation.as_deref(),
                plan.previous_deployment_id.as_deref(),
            )
            .await?;
        validate_prepare_consumer_generation_response(
            &progress,
            &plan.id,
            &plan.routing_generation.to_string(),
            plan.previous_generation.as_deref(),
            plan.previous_deployment_id.as_deref(),
        )?;
        Ok(progress)
    }

    pub async fn activate_consumer_generation(
        &self,
        environment: &Environment,
        plan: &crate::catalog::DeploymentPlan,
    ) -> anyhow::Result<deis_queue::proto::queue::ActivateConsumerGenerationResponse> {
        let tenant = self.canonical_tenant(environment).await?;
        let progress = self
            .storage
            .activate_consumer_generation(
                &tenant,
                &plan.id,
                &plan.routing_generation.to_string(),
            )
            .await?;
        validate_activate_consumer_generation_response(
            &progress,
            &plan.id,
            &plan.routing_generation.to_string(),
            plan.previous_generation.as_deref(),
            plan.previous_deployment_id.as_deref(),
        )?;
        Ok(progress)
    }

    pub async fn abort_consumer_generation(
        &self,
        environment: &Environment,
        plan: &crate::catalog::DeploymentPlan,
    ) -> anyhow::Result<deis_queue::proto::queue::AbortConsumerGenerationResponse> {
        let tenant = self.canonical_tenant(environment).await?;
        let progress = self
            .storage
            .abort_consumer_generation(
                &tenant,
                &plan.id,
                &plan.routing_generation.to_string(),
            )
            .await?;
        validate_abort_consumer_generation_response(
            &progress,
            &plan.id,
            plan.previous_generation.as_deref(),
            plan.previous_deployment_id.as_deref(),
        )?;
        Ok(progress)
    }

    /// Freeze the runtime serving set for a deployment plan. Lost nodes stay
    /// in the set, and a refused duplicate-id claimant gets its own durable
    /// target key so it cannot be overwritten by the holder's successful ACK.
    pub fn serving_target_ids(&self) -> Vec<String> {
        let mut ids = self.nodes.ids();
        ids.extend(self.nodes.departed().into_iter().map(|node| node.id));
        ids.extend(self.nodes.contested().into_iter().map(|node| {
            format!(
                "{}{}@{}",
                Self::CONTESTED_TARGET_PREFIX,
                node.id,
                node.claimant_addr
            )
        }));
        ids.sort();
        ids.dedup();
        ids
    }

    /// Assemble what the nodes need for one version of one environment.
    ///
    /// Fails - before anything is pushed and, at the deploy call site,
    /// before the deployment pointer is even written - when the secrets of
    /// this environment cannot be decrypted. Delivering an application
    /// with half its secrets missing is worse than not deploying: the code
    /// runs and misbehaves instead of the operator seeing a clear error.
    pub async fn payload(
        &self,
        env: &Environment,
        version: &Version,
        base_url: &str,
    ) -> CatalogResult<DeploymentPayload> {
        self.payload_inner(env, version, base_url, BTreeMap::new(), None)
            .await
    }

    pub async fn payload_with_config(
        &self,
        env: &Environment,
        version: &Version,
        base_url: &str,
        config_values: BTreeMap<String, serde_json::Value>,
    ) -> CatalogResult<DeploymentPayload> {
        self.payload_inner(env, version, base_url, config_values, None)
            .await
    }

    pub async fn payload_for_snapshot(
        &self,
        env: &Environment,
        version: &Version,
        base_url: &str,
        snapshot: deis_wire::EnvironmentBindingSnapshot,
    ) -> CatalogResult<DeploymentPayload> {
        self.payload_inner(env, version, base_url, BTreeMap::new(), Some(snapshot))
            .await
    }

    async fn payload_inner(
        &self,
        env: &Environment,
        version: &Version,
        base_url: &str,
        config_values: BTreeMap<String, serde_json::Value>,
        prepared_snapshot: Option<deis_wire::EnvironmentBindingSnapshot>,
    ) -> CatalogResult<DeploymentPayload> {
        // The legacy foreign key on the environment names the app row; read
        // that durable row so the wire carries its permanent identity rather
        // than deriving authority from the mutable display name.
        let app = self.store.get_app(&env.app_id).await?;
        let version_id = version.id.as_str();
        let manifest = deis_bundle::Manifest::parse(&version.manifest).map_err(|e| {
            error!(
                app_id = %env.app_id,
                environment = %env.name,
                version_id = %version_id,
                error = %e,
                "Stored manifest of this version no longer parses, so its declared variables cannot be built; the deployment is refused rather than sent with an empty environment"
            );
            CatalogError::BundleStorage(format!(
                "stored manifest of version {version_id} no longer parses: {e}"
            ))
        })?;

        let mut env_map: BTreeMap<String, String> = manifest.env.clone();

        // The same block the upload already accepted, read again here because
        // this is the message that carries it to a node. A version stored by
        // an older build - or by a build that did not check - can still hold a
        // limits block nothing can act on; that must not stop the deployment,
        // because the alternative is an application that will not deploy at
        // all over a field that only ever narrows what it is allowed. It runs
        // on the node's ceilings and the reason is said out loud.
        let limits = match declared_limits(&manifest) {
            Ok(limits) => limits,
            Err(reason) => {
                warn!(
                    app_id = %env.app_id,
                    environment = %env.name,
                    version_id = %version_id,
                    reason = %reason,
                    "This version's manifest declares resource limits that cannot be read, so none are sent with the deployment; the application runs on this node's default ceilings, which are never narrower than what it asked for. Rebuild and upload the bundle with a valid limits block"
                );
                None
            }
        };
        match &limits {
            Some(declared) => info!(
                app_id = %env.app_id,
                environment = %env.name,
                version_id = %version_id,
                limits = %declared.describe(),
                "Delivering with the resource limits this version's manifest declares: {}. A node applies whichever is narrower, its own ceiling or this",
                declared.describe()
            ),
            None => debug!(
                app_id = %env.app_id,
                environment = %env.name,
                version_id = %version_id,
                "This version declares no resource limits; every node holds it to its own default ceilings"
            ),
        }

        // Read from the same stored manifest, and refused rather than
        // degraded - the opposite of the limits above, on purpose. A version
        // whose limits cannot be read still runs correctly, just on the node's
        // own ceilings; a version whose compatibility declaration cannot be
        // read has no correct behaviour to fall back on, because every answer
        // this platform could pick is one the application did not ask for.
        // Refusing the delivery leaves whatever is currently deployed running.
        // Resolved for the log line and for the refusal below; what travels is
        // the declaration itself, a few lines down.
        let resolved = declared_compatibility(&manifest).map_err(|reason| {
            error!(
                app_id = %env.app_id,
                environment = %env.name,
                version_id = %version_id,
                reason = %reason,
                "This version's manifest declares a compatibility date or flag this build of the control plane cannot honour, so the deployment is refused; the environment keeps running whatever it was running. Rebuild the bundle against a date this platform knows, or upgrade the platform"
            );
            CatalogError::invalid(format!(
                "version {version_id} declares a compatibility date or flag this platform cannot \
                 honour: {reason}"
            ))
        })?;
        match resolved.date() {
            Some(date) => info!(
                app_id = %env.app_id,
                environment = %env.name,
                version_id = %version_id,
                compatibility_date = %date,
                compatibility = %resolved.describe(),
                "Delivering under the platform behaviour of {}: {}",
                date,
                resolved.describe()
            ),
            None => warn!(
                app_id = %env.app_id,
                environment = %env.name,
                version_id = %version_id,
                "Delivering a version that names no compatibility date, so every node holds it to the oldest behaviour this platform has: {}",
                crate::catalog::describe_version(&resolved)
            ),
        }
        let compatibility = manifest.compatibility.clone().unwrap_or_default();

        let mut secret_names = Vec::new();
        let mut environment_declarations = None;
        let mut environment_snapshot = None;
        let mut secret_values_by_revision = BTreeMap::new();

        if let Some(declarations) = manifest.environment.clone() {
            env_map.clear();
            declarations
                .validate(
                    deis_wire::CURRENT_EDGE_CANON_STANDARD_VERSION,
                    manifest.bindings.all_names(),
                )
                .map_err(|error| CatalogError::invalid(error.to_string()))?;
            let declaration_identity = declarations
                .identity()
                .map_err(|error| CatalogError::invalid(error.to_string()))?;
            let mut available_revisions = BTreeSet::new();
            let snapshot = if let Some(snapshot) = prepared_snapshot {
                if !config_values.is_empty() {
                    return Err(CatalogError::invalid(
                        "EC_ENV_SNAPSHOT_INVALID: config values cannot replace a historical snapshot",
                    ));
                }
                for binding in &snapshot.bindings {
                    if let deis_wire::EnvironmentBinding::Secret(secret) = binding {
                        let stored = self
                            .store
                            .secret_revision_ciphertext(&env.id, &secret.name, &secret.revision)
                            .await
                            .map_err(|_| {
                                CatalogError::invalid(
                                    "EC_ENV_SECRET_REVISION_UNAVAILABLE: historical secret revision is unavailable",
                                )
                            })?;
                        let value = decrypt_secret(self.secrets.as_ref(), &env.id, &stored)?;
                        deis_wire::validate_secret_value(&value)
                            .map_err(|error| CatalogError::invalid(error.to_string()))?;
                        available_revisions.insert(stored.revision.clone());
                        secret_values_by_revision.insert(stored.revision, value);
                        secret_names.push(stored.name);
                    }
                }
                snapshot
            } else {
                let current = self
                    .store
                    .secret_ciphertexts(&env.id)
                    .await?
                    .into_iter()
                    .map(|stored| (stored.name.clone(), stored))
                    .collect::<BTreeMap<_, _>>();
                let declared = declarations.by_name();
                for name in config_values.keys() {
                    let Some(declaration) = declared.get(name.as_str()) else {
                        return Err(CatalogError::invalid(
                            "EC_ENV_BINDING_UNDECLARED: deployment config contains an undeclared name",
                        ));
                    };
                    if declaration.kind != deis_wire::EnvironmentKind::Config {
                        return Err(CatalogError::invalid(
                            "EC_ENV_BINDING_TYPE_MISMATCH: deployment config supplies a secret declaration",
                        ));
                    }
                }

                let mut bindings = Vec::new();
                for declaration in &declarations.declarations {
                    match declaration.kind {
                        deis_wire::EnvironmentKind::Config => {
                            if let Some(value) = config_values.get(&declaration.name) {
                                bindings.push(deis_wire::EnvironmentBinding::Config(
                                    deis_wire::EnvironmentConfigBinding::new(
                                        &declaration.name,
                                        declaration.value_type,
                                        value.clone(),
                                    )
                                    .map_err(|error| CatalogError::invalid(error.to_string()))?,
                                ));
                            } else if declaration.required {
                                return Err(CatalogError::invalid(
                                    "EC_ENV_REQUIRED_MISSING: required config binding is unavailable",
                                ));
                            }
                        }
                        deis_wire::EnvironmentKind::Secret => {
                            if let Some(stored) = current.get(&declaration.name) {
                                let value = decrypt_secret(self.secrets.as_ref(), &env.id, stored)?;
                                deis_wire::validate_secret_value(&value)
                                    .map_err(|error| CatalogError::invalid(error.to_string()))?;
                                available_revisions.insert(stored.revision.clone());
                                secret_values_by_revision.insert(stored.revision.clone(), value);
                                secret_names.push(stored.name.clone());
                                bindings.push(deis_wire::EnvironmentBinding::Secret(
                                    deis_wire::EnvironmentSecretBinding::new(
                                        &stored.name,
                                        &stored.revision,
                                    ),
                                ));
                            } else if declaration.required {
                                return Err(CatalogError::invalid(
                                    "EC_ENV_REQUIRED_MISSING: required secret binding is unavailable",
                                ));
                            }
                        }
                    }
                }
                deis_wire::EnvironmentBindingSnapshot::new(
                    declarations.standard_version.clone(),
                    version_id,
                    env.canonical_id.to_string(),
                    declaration_identity,
                    bindings,
                )
            };
            snapshot
                .validate(
                    &declarations,
                    deis_wire::CURRENT_EDGE_CANON_STANDARD_VERSION,
                    |revision| available_revisions.contains(revision),
                )
                .map_err(|error| CatalogError::invalid(error.to_string()))?;
            environment_declarations = Some(declarations);
            environment_snapshot = Some(snapshot);
        } else {
            // No `environment` declarations: nothing binds a secret, so none
            // is delivered. A secret value reaches a node only through a
            // snapshot and `FetchSecretValues`, never in the deployment's env.
            let stored = self.store.secret_ciphertexts(&env.id).await?.len();
            if stored > 0 || !manifest.secrets.is_empty() {
                warn!(
                    app_id = %env.app_id,
                    environment = %env.name,
                    version_id = %version_id,
                    stored_secrets = stored,
                    manifest_secrets = manifest.secrets.len(),
                    "This version's manifest has no `environment` declarations, so it binds no secret and none is delivered; declare each secret under `environment` to have the node fetch it"
                );
            }
        }

        let (environment_declarations_json, environment_snapshot_json, environment_snapshot_id) =
            match (&environment_declarations, &environment_snapshot) {
                (Some(declarations), Some(snapshot)) => {
                    let declarations_json = String::from_utf8(
                        declarations
                            .canonical_json()
                            .map_err(|error| CatalogError::invalid(error.to_string()))?,
                    )
                    .map_err(|_| {
                        CatalogError::invalid(
                            "EC_ENV_DOCUMENT_INVALID: canonical declaration is not UTF-8",
                        )
                    })?;
                    let snapshot_json = String::from_utf8(
                        snapshot
                            .canonical_json()
                            .map_err(|error| CatalogError::invalid(error.to_string()))?,
                    )
                    .map_err(|_| {
                        CatalogError::invalid(
                            "EC_ENV_SNAPSHOT_INVALID: canonical snapshot is not UTF-8",
                        )
                    })?;
                    let snapshot_id = snapshot
                        .identity()
                        .map_err(|error| CatalogError::invalid(error.to_string()))?;
                    (
                        Some(declarations_json),
                        Some(snapshot_json),
                        Some(snapshot_id),
                    )
                }
                (None, None) => (None, None, None),
                _ => {
                    return Err(CatalogError::invalid(
                        "EC_ENV_SNAPSHOT_INVALID: declaration and snapshot must be prepared together",
                    ));
                }
            };

        Ok(DeploymentPayload {
            app_id: env.app_id.clone(),
            canonical_application_id: app.canonical_id,
            version_id: version_id.to_string(),
            environment: env.name.clone(),
            canonical_environment_id: env.canonical_id,
            bundle_url: bundle_url(base_url, &env.app_id, version_id),
            bundle_sha: version.bundle_sha.clone(),
            env: env_map,
            secret_names,
            environment_declarations,
            environment_snapshot,
            environment_declarations_json,
            environment_snapshot_json,
            environment_snapshot_id,
            secret_values_by_revision,
            storage: self.storage.clone(),
            limits,
            compatibility,
            deployment_id: None,
            routing_generation: None,
            action: proto::DeploymentAction::Unspecified,
        })
    }

    /// Build the payload for one assignment row, the way a push would.
    ///
    /// The assignment list is the other half of a node's life: a node that
    /// restarts asks for it and loads what it names. Until it carried an
    /// environment, an app that came back this way came back with no secrets
    /// and - worse, because it is silent - no storage addresses, so its
    /// `ctx.services` calls failed until someone deployed again.
    ///
    /// Errors are the caller's to report per row: one environment whose
    /// secrets will not decrypt must not stop a node from loading the rest.
    pub async fn assignment_payload(
        &self,
        assignment: &crate::catalog::Assignment,
        base_url: &str,
    ) -> CatalogResult<DeploymentPayload> {
        // Load the durable row instead of synthesising a partial Environment:
        // once canonical identity exists, even an internal model value must
        // not carry an empty or invented identity.
        let env = self
            .store
            .get_environment(&assignment.app_id, &assignment.environment)
            .await?;
        let version = self
            .store
            .get_version(&assignment.app_id, &assignment.version_id)
            .await?;
        let mut payload = if let Some(snapshot_id) = &assignment.snapshot_id {
            let snapshot = self.store.get_environment_snapshot(snapshot_id).await?;
            self.payload_for_snapshot(&env, &version, base_url, snapshot)
                .await
        } else {
            self.payload(&env, &version, base_url).await
        }?;
        payload.set_instruction(
            &assignment.deployment_id,
            &assignment.routing_generation,
            match assignment.deployment_state.as_deref() {
                // A pointer exists only from `activating` onward in the new
                // protocol, but accepting the two earlier non-terminal
                // states here keeps a restored/hand-repaired catalog safe:
                // an uncertain generation may be made addressable, never
                // allowed to start triggers early.
                Some("preparing" | "prepared" | "activating" | "reconciling" | "degraded") => {
                    proto::DeploymentAction::Prepare
                }
                Some("active") | None => proto::DeploymentAction::Unspecified,
                Some(state) => {
                    return Err(CatalogError::conflict(format!(
                        "current deployment {} has terminal or unknown plan state {state}",
                        assignment.deployment_id
                    )));
                }
            },
        );
        Ok(payload)
    }

    /// Decrypt exactly `revisions` of one immutable environment snapshot.
    ///
    /// This is what `FetchSecretValues` answers with, and it is deliberately
    /// narrower than [`Self::assignment_payload`]: it reads only the secret
    /// bindings of the snapshot, decrypts only what was asked for, and refuses
    /// the whole request when any revision is not one the snapshot binds. A
    /// node asking for a revision its deployment does not reference is either
    /// broken or probing, and neither gets a partial answer.
    pub async fn snapshot_secret_values(
        &self,
        env_id: &str,
        snapshot_id: &str,
        revisions: &BTreeSet<String>,
    ) -> CatalogResult<BTreeMap<String, String>> {
        let snapshot = self.store.get_environment_snapshot(snapshot_id).await?;
        let bound: BTreeMap<&str, &str> = snapshot
            .bindings
            .iter()
            .filter_map(|binding| match binding {
                deis_wire::EnvironmentBinding::Secret(secret) => {
                    Some((secret.revision.as_str(), secret.name.as_str()))
                }
                deis_wire::EnvironmentBinding::Config(_) => None,
            })
            .collect();

        let mut values = BTreeMap::new();
        for revision in revisions {
            let Some(name) = bound.get(revision.as_str()) else {
                return Err(CatalogError::forbidden(format!(
                    "secret revision {revision} is not bound by snapshot {snapshot_id}"
                )));
            };
            let stored = self
                .store
                .secret_revision_ciphertext(env_id, name, revision)
                .await?;
            let value = decrypt_secret(self.secrets.as_ref(), env_id, &stored)?;
            values.insert(revision.clone(), value);
        }
        Ok(values)
    }

    /// Push a payload to every configured node, concurrently.
    ///
    /// Never fails as a whole: a node that is down is represented in the
    /// report. The strict coordinator decides whether that means abort before
    /// pointer commit or durable reconciliation after commit; transport code
    /// must not erase that distinction.
    pub async fn push(
        &self,
        payload: &DeploymentPayload,
        reason: DeliveryReason,
    ) -> DeliveryReport {
        let targets = self.serving_target_ids();
        self.push_to(payload, reason, &targets).await
    }

    /// Push one phase to the serving set frozen in the plan. Fleet membership
    /// may change while a deployment is in flight; recomputing it here could
    /// silently omit a lost node or require a newly joined node to satisfy an
    /// intent that never named it.
    pub async fn push_to(
        &self,
        payload: &DeploymentPayload,
        reason: DeliveryReason,
        target_ids: &[String],
    ) -> DeliveryReport {
        let phase = match payload.action() {
            proto::DeploymentAction::Prepare => "prepare",
            proto::DeploymentAction::Activate => "activate",
            proto::DeploymentAction::Abort => "abort",
            proto::DeploymentAction::Drain => "drain",
            proto::DeploymentAction::Unspecified => "apply",
        };
        let mut report = DeliveryReport {
            reason,
            phase,
            app_id: payload.app_id.clone(),
            environment: payload.environment.clone(),
            version_id: payload.version_id.clone(),
            delivered: Vec::new(),
            failed: Vec::new(),
            skipped: None,
        };

        if target_ids.is_empty() {
            let reason_text = "no runtime nodes belong to this deployment plan; strict activation requires at least one prepared serving target"
                .to_string();
            info!(
                app_id = %payload.app_id,
                environment = %payload.environment,
                version_id = %payload.version_id,
                reason = reason.as_str(),
                "{reason_text}"
            );
            report.skipped = Some(reason_text);
            return report;
        }

        debug!(
            app_id = %payload.app_id,
            environment = %payload.environment,
            version_id = %payload.version_id,
            reason = reason.as_str(),
            nodes = target_ids.len(),
            variables = payload.variable_count(),
            secrets = ?payload.secret_names(),
            "Delivering a deployment to every runtime node"
        );

        let mut unavailable = std::collections::BTreeMap::new();
        for node in self.nodes.departed() {
            unavailable.insert(node.id.clone(), node.describe());
        }
        for node in self.nodes.contested() {
            unavailable.insert(
                format!(
                    "{}{}@{}",
                    Self::CONTESTED_TARGET_PREFIX,
                    node.id,
                    node.claimant_addr
                ),
                node.describe(),
            );
        }
        let results = futures::future::join_all(target_ids.iter().map(|target| {
            let request = payload.to_request();
            let unavailable = unavailable.get(target).cloned();
            async move {
                if let Some(detail) = unavailable {
                    return (
                        target.clone(),
                        Err(crate::runtime_client::NodeError::Transport(detail)),
                    );
                }
                (
                    target.clone(),
                    self.nodes.apply_deployment(target, request).await,
                )
            }
        }))
        .await;

        let expected_state = match payload.action() {
            proto::DeploymentAction::Prepare => "prepared",
            proto::DeploymentAction::Activate => "active",
            proto::DeploymentAction::Abort => "aborted",
            proto::DeploymentAction::Drain => "drained",
            proto::DeploymentAction::Unspecified => "applied",
        };
        for (node, result) in results {
            match result {
                Ok(resp)
                    if resp.success
                        && payload.deployment_id().is_none_or(|expected| {
                            resp.deployment_id == expected
                                && resp.routing_generation
                                    == payload.routing_generation().unwrap_or_default()
                        })
                        && resp.observed_state == expected_state =>
                {
                    report.delivered.push(node)
                }
                Ok(resp) if resp.success => report.failed.push(NodeFailure {
                    node,
                    error: format!(
                        "node acknowledged the wrong deployment observation: plan {:?}, generation {:?}, state {:?}",
                        resp.deployment_id, resp.routing_generation, resp.observed_state
                    ),
                }),
                Ok(resp) => report.failed.push(NodeFailure {
                    node,
                    error: if resp.message.is_empty() {
                        "node rejected the deployment without saying why".to_string()
                    } else {
                        resp.message
                    },
                }),
                Err(e) => report.failed.push(NodeFailure {
                    node,
                    error: e.to_string(),
                }),
            }
        }

        let failed_nodes: Vec<&str> = report.failed.iter().map(|f| f.node.as_str()).collect();
        if report.failed.is_empty() {
            info!(
                app_id = %payload.app_id,
                environment = %payload.environment,
                version_id = %payload.version_id,
                reason = reason.as_str(),
                nodes = report.delivered.len(),
                "Every runtime node is serving this version"
            );
        } else if report.delivered.is_empty() {
            error!(
                app_id = %payload.app_id,
                environment = %payload.environment,
                version_id = %payload.version_id,
                reason = reason.as_str(),
                nodes = ?failed_nodes,
                errors = ?report.failed.iter().map(|f| f.error.as_str()).collect::<Vec<_>>(),
                "No runtime node took this version; the whole fleet still serves the previous one until a node refetches its assignments"
            );
        } else {
            warn!(
                app_id = %payload.app_id,
                environment = %payload.environment,
                version_id = %payload.version_id,
                reason = reason.as_str(),
                delivered = report.delivered.len(),
                failed = ?failed_nodes,
                "{} of the fleet took this version and {} did not; the listed nodes still serve the previous one, so requests hit two different versions until they catch up",
                report.delivered.len(),
                failed_nodes.len()
            );
        }

        report
    }

    /// Tell every node to stop serving an application.
    ///
    /// Called after the catalog rows are gone. A node only ever *adds* from
    /// its assignment list, so without this call a deleted application keeps
    /// running on every node until that node's process restarts: it still
    /// answers requests routed by app id, still holds its decrypted secrets
    /// in memory, and its queue consumers still poll. For a deletion done to
    /// revoke access that is the difference between revoking it and not.
    ///
    /// `environments` is only for the report; each node is told to drop
    /// every environment of the app, because the catalog rows that would say
    /// which ones exist have already been deleted, and a node may hold an
    /// environment this control plane forgot about earlier.
    ///
    /// Never fails as a whole: the rows are already gone, so a node that
    /// cannot be reached is reported rather than propagated.
    pub async fn remove_app(&self, app_id: &str, environments: Vec<String>) -> RemovalReport {
        let mut report = RemovalReport {
            app_id: app_id.to_string(),
            environments,
            unloaded: Vec::new(),
            failed: Vec::new(),
            skipped: None,
        };

        let node_ids = self.nodes.ids();
        // The same read as in `push`, and it matters more here: a deployment
        // that misses a node is corrected by that node's next assignment
        // reconcile, while a *deletion* that misses one leaves it running the
        // application - its code, its decrypted secrets, its queue consumers -
        // until it manages to reconcile against a catalog that no longer names
        // it. A node that stopped refreshing its registration is by definition
        // not reconciling either.
        let departed = self.nodes.departed();
        // And the machine that was refused a node id another machine holds is
        // exactly as unreachable, with the same consequence one step worse:
        // after this call it goes on serving the deleted application, its code
        // and its decrypted secrets.
        let contested = self.nodes.contested();
        if node_ids.is_empty() && departed.is_empty() && contested.is_empty() {
            let reason = "no runtime nodes are configured on this control plane, so there is \
                 nothing serving the deleted application here"
                .to_string();
            info!(app_id = %app_id, "{reason}");
            report.skipped = Some(reason);
            return report;
        }

        let results = futures::future::join_all(node_ids.iter().map(|node| async move {
            // An empty environment means every environment of this app;
            // see UnloadAppRequest in management.proto.
            (node.clone(), self.nodes.unload_app(node, app_id, "").await)
        }))
        .await;

        for (node, result) in results {
            match result {
                Ok(resp) if resp.success => report.unloaded.push(node),
                Ok(resp) => report.failed.push(NodeFailure {
                    node,
                    error: if resp.message.is_empty() {
                        "node refused to unload the app without saying why".to_string()
                    } else {
                        resp.message
                    },
                }),
                Err(e) => report.failed.push(NodeFailure {
                    node,
                    error: e.to_string(),
                }),
            }
        }

        for node in &departed {
            report.failed.push(NodeFailure {
                node: node.id.clone(),
                error: node.describe(),
            });
        }
        for node in &contested {
            report.failed.push(NodeFailure {
                node: node.id.clone(),
                error: node.describe(),
            });
        }

        if report.failed.is_empty() {
            info!(
                app_id = %app_id,
                nodes = report.unloaded.len(),
                "Every runtime node dropped the deleted application; nothing serves it any more"
            );
        } else {
            error!(
                app_id = %app_id,
                unloaded = report.unloaded.len(),
                lost_nodes = departed.len(),
                contested_ids = contested.len(),
                nodes = ?report.failed.iter().map(|f| f.node.as_str()).collect::<Vec<_>>(),
                errors = ?report.failed.iter().map(|f| f.error.as_str()).collect::<Vec<_>>(),
                "These runtime nodes were not told to drop the deleted application and are still serving it, with its secrets still in memory; one that can still reach this control plane stops on its own the next time it reconciles against its assignment list, which no longer names the application, so the exposure lasts until that reconcile - while one counted in lost_nodes stopped talking to this control plane altogether and is not reconciling either, so it serves the deleted application until it is restarted or comes back"
            );
        }

        report
    }

    /// Build and push in one step for the version an environment runs now.
    ///
    /// This is the secret-change path: nothing about the version moved, but
    /// the values the nodes hold in memory are stale, so the same version
    /// is delivered again with a fresh env map.
    ///
    /// Never fails: the change that triggered it is already recorded, so
    /// anything that goes wrong here is reported in the returned report
    /// and in the log rather than handed back to the caller as if the
    /// change had not happened.
    pub async fn reload_current(
        &self,
        env: &Environment,
        base_url: &str,
        reason: DeliveryReason,
    ) -> DeliveryReport {
        let skipped = |version_id: String, text: String| DeliveryReport {
            reason,
            phase: "legacy_apply",
            app_id: env.app_id.clone(),
            environment: env.name.clone(),
            version_id,
            delivered: Vec::new(),
            failed: Vec::new(),
            skipped: Some(text),
        };

        let deployment = match self.store.current_deployment(&env.id).await {
            Ok(Some(deployment)) => deployment,
            Ok(None) => {
                let text = format!(
                    "environment {} of app {} has never been deployed, so there is nothing to reload; the change takes effect on its first deployment",
                    env.name, env.app_id
                );
                info!(
                    app_id = %env.app_id,
                    environment = %env.name,
                    reason = reason.as_str(),
                    "{text}"
                );
                return skipped(String::new(), text);
            }
            Err(e) => {
                error!(
                    app_id = %env.app_id,
                    environment = %env.name,
                    reason = reason.as_str(),
                    error = %e,
                    "Could not read which version this environment runs, so the nodes were not updated; they keep serving the values they already hold until the next deployment"
                );
                return skipped(String::new(), e.to_string());
            }
        };

        let version = match self
            .store
            .get_version(&env.app_id, &deployment.version_id)
            .await
        {
            Ok(version) => version,
            Err(e) => {
                error!(
                    app_id = %env.app_id,
                    environment = %env.name,
                    version_id = %deployment.version_id,
                    error = %e,
                    "The version this environment points at could not be read, so the nodes were not updated and keep serving the values they already hold"
                );
                return skipped(deployment.version_id.clone(), e.to_string());
            }
        };

        let payload = if let Some(snapshot_id) = &deployment.snapshot_id {
            match self.store.get_environment_snapshot(snapshot_id).await {
                Ok(snapshot) => {
                    self.payload_for_snapshot(env, &version, base_url, snapshot)
                        .await
                }
                Err(error) => Err(error),
            }
        } else {
            self.payload(env, &version, base_url).await
        };
        match payload {
            Ok(payload) => self.push(&payload, reason).await,
            Err(e) => {
                error!(
                    app_id = %env.app_id,
                    environment = %env.name,
                    version_id = %version.id,
                    reason = reason.as_str(),
                    error = %e,
                    "The change is recorded but could not be turned into a deployment payload, so no node was updated; every node keeps the environment it already had"
                );
                skipped(version.id.clone(), e.to_string())
            }
        }
    }

    /// Activate a new snapshot for a secret rotation/deletion without changing
    /// the application version. Legacy schema-1 deployments keep their old
    /// reload behavior; schema-2 deployments always append a pointer row.
    pub async fn prepare_secret_change(
        &self,
        env: &Environment,
        base_url: &str,
        name: &str,
        revision: Option<&str>,
        expected_secret_revision: Option<&str>,
    ) -> CatalogResult<SecretChangePreparation> {
        let reason = DeliveryReason::SecretChange;
        let skipped = |version_id: String, text: String| DeliveryReport {
            reason,
            phase: "legacy_apply",
            app_id: env.app_id.clone(),
            environment: env.name.clone(),
            version_id,
            delivered: Vec::new(),
            failed: Vec::new(),
            skipped: Some(text),
        };
        let deployment = match self.store.current_deployment(&env.id).await? {
            Some(deployment) => deployment,
            None => {
                self.store
                    .promote_secret_change(env, name, revision, expected_secret_revision)
                    .await?;
                return Ok(SecretChangePreparation::Finished(skipped(
                    String::new(),
                    "the environment has never been deployed; the secret head will be selected on its first deployment"
                        .to_string(),
                )));
            }
        };
        let Some(snapshot_id) = deployment.snapshot_id.as_deref() else {
            self.store
                .promote_secret_change(env, name, revision, expected_secret_revision)
                .await?;
            return Ok(SecretChangePreparation::Finished(
                self.reload_current(env, base_url, reason).await,
            ));
        };
        let version = self
            .store
            .get_version(&env.app_id, &deployment.version_id)
            .await?;
        let manifest = deis_bundle::Manifest::parse(&version.manifest).map_err(|error| {
            CatalogError::BundleStorage(format!(
                "stored manifest of version {} no longer parses: {error}",
                version.id
            ))
        })?;
        let Some(declarations) = manifest.environment else {
            return Err(CatalogError::invalid(
                "EC_ENV_SNAPSHOT_INVALID: deployment snapshot exists but its bundle has no environment declaration",
            ));
        };
        let Some(declaration) = declarations
            .declarations
            .iter()
            .find(|declaration| declaration.name == name)
        else {
            self.store
                .promote_secret_change(env, name, revision, expected_secret_revision)
                .await?;
            return Ok(SecretChangePreparation::Finished(skipped(
                version.id,
                format!(
                    "secret {name} is not declared by the running version; its active snapshot is unchanged"
                ),
            )));
        };
        if declaration.kind != deis_wire::EnvironmentKind::Secret {
            self.store
                .promote_secret_change(env, name, revision, expected_secret_revision)
                .await?;
            return Ok(SecretChangePreparation::Finished(skipped(
                version.id,
                format!(
                    "{name} is declared as config, so a secret write cannot replace its active value"
                ),
            )));
        }
        if revision.is_none() && declaration.required {
            self.store
                .promote_secret_change(env, name, None, expected_secret_revision)
                .await?;
            return Ok(SecretChangePreparation::Finished(skipped(
                version.id,
                format!(
                    "required secret {name} was deleted from the current head; the active snapshot keeps its retained revision until a deploy supplies a valid replacement"
                ),
            )));
        }
        let mut snapshot = self.store.get_environment_snapshot(snapshot_id).await?;
        snapshot.bindings.retain(|binding| binding.name() != name);
        if let Some(revision) = revision {
            snapshot
                .bindings
                .push(deis_wire::EnvironmentBinding::Secret(
                    deis_wire::EnvironmentSecretBinding::new(name, revision),
                ));
        }
        snapshot
            .bindings
            .sort_by(|left, right| left.name().cmp(right.name()));
        let payload = self
            .payload_for_snapshot(env, &version, base_url, snapshot.clone())
            .await?;
        Ok(SecretChangePreparation::Prepared {
            version,
            payload: Box::new(payload),
        })
    }
}

fn validate_prepare_consumer_generation_response(
    progress: &deis_queue::proto::queue::PrepareConsumerGenerationResponse,
    plan_id: &str,
    generation: &str,
    previous_generation: Option<&str>,
    previous_deployment_id: Option<&str>,
) -> anyhow::Result<()> {
    if progress.active_generation != previous_generation.unwrap_or_default()
        || progress.active_deployment_id != previous_deployment_id.unwrap_or_default()
        || progress.pending_generation != generation
        || progress.pending_deployment_id != plan_id
        || progress.outstanding_leases < 0
        || progress.prepared != (progress.outstanding_leases == 0)
    {
        anyhow::bail!(
            "queue PREPARE returned an observation that does not match plan {plan_id} generation {generation}"
        );
    }
    Ok(())
}

fn validate_activate_consumer_generation_response(
    progress: &deis_queue::proto::queue::ActivateConsumerGenerationResponse,
    plan_id: &str,
    generation: &str,
    previous_generation: Option<&str>,
    previous_deployment_id: Option<&str>,
) -> anyhow::Result<()> {
    let valid = if progress.activated {
        progress.active_generation == generation
            && progress.active_deployment_id == plan_id
            && progress.pending_generation.is_empty()
            && progress.pending_deployment_id.is_empty()
            && progress.outstanding_leases == 0
    } else {
        progress.active_generation == previous_generation.unwrap_or_default()
            && progress.active_deployment_id == previous_deployment_id.unwrap_or_default()
            && progress.pending_generation == generation
            && progress.pending_deployment_id == plan_id
            && progress.outstanding_leases > 0
    };
    if !valid {
        anyhow::bail!(
            "queue ACTIVATE returned an observation that does not match plan {plan_id} generation {generation}"
        );
    }
    Ok(())
}

fn validate_abort_consumer_generation_response(
    progress: &deis_queue::proto::queue::AbortConsumerGenerationResponse,
    plan_id: &str,
    previous_generation: Option<&str>,
    previous_deployment_id: Option<&str>,
) -> anyhow::Result<()> {
    if progress.active_generation != previous_generation.unwrap_or_default()
        || progress.active_deployment_id != previous_deployment_id.unwrap_or_default()
    {
        anyhow::bail!(
            "queue ABORT returned an observation that does not match plan {plan_id} baseline"
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::SecretCipher;
    use crate::catalog::secrets::MASTER_KEY_LEN;
    use crate::database::Database;
    use chrono::Utc;

    const MANIFEST: &str = r#"{"schema":1,"app":"demo","entry":"worker/index.js",
        "env":{"LOG_LEVEL":"info","API_KEY":"placeholder-from-manifest"},
        "secrets":["API_KEY","MISSING_ONE"]}"#;

    #[test]
    fn queue_barrier_observations_are_bound_to_both_generation_and_deployment_owner() {
        const PREVIOUS_PLAN: &str = "018f2518-7e38-7f80-8000-000000000001";
        const PREVIOUS_GENERATION: &str = "018f2518-7e38-7f80-8000-000000000002";
        const PLAN: &str = "018f2518-7e38-7f80-8000-000000000003";
        const GENERATION: &str = "018f2518-7e38-7f80-8000-000000000004";

        let mut prepared = deis_queue::proto::queue::PrepareConsumerGenerationResponse {
            prepared: true,
            active_generation: PREVIOUS_GENERATION.to_string(),
            pending_generation: GENERATION.to_string(),
            outstanding_leases: 0,
            active_deployment_id: PREVIOUS_PLAN.to_string(),
            pending_deployment_id: PLAN.to_string(),
        };
        assert!(
            validate_prepare_consumer_generation_response(
                &prepared,
                PLAN,
                GENERATION,
                Some(PREVIOUS_GENERATION),
                Some(PREVIOUS_PLAN),
            )
            .is_ok()
        );
        prepared.pending_deployment_id = PREVIOUS_PLAN.to_string();
        assert!(
            validate_prepare_consumer_generation_response(
                &prepared,
                PLAN,
                GENERATION,
                Some(PREVIOUS_GENERATION),
                Some(PREVIOUS_PLAN),
            )
            .is_err(),
            "a matching generation reported under the wrong deployment must fail closed"
        );

        let mut active = deis_queue::proto::queue::ActivateConsumerGenerationResponse {
            activated: true,
            active_generation: GENERATION.to_string(),
            pending_generation: String::new(),
            outstanding_leases: 0,
            active_deployment_id: PLAN.to_string(),
            pending_deployment_id: String::new(),
        };
        assert!(
            validate_activate_consumer_generation_response(
                &active,
                PLAN,
                GENERATION,
                Some(PREVIOUS_GENERATION),
                Some(PREVIOUS_PLAN),
            )
            .is_ok()
        );
        active.active_deployment_id = PREVIOUS_PLAN.to_string();
        assert!(
            validate_activate_consumer_generation_response(
                &active,
                PLAN,
                GENERATION,
                Some(PREVIOUS_GENERATION),
                Some(PREVIOUS_PLAN),
            )
            .is_err()
        );

        let mut aborted = deis_queue::proto::queue::AbortConsumerGenerationResponse {
            aborted: true,
            active_generation: PREVIOUS_GENERATION.to_string(),
            active_deployment_id: PREVIOUS_PLAN.to_string(),
        };
        assert!(
            validate_abort_consumer_generation_response(
                &aborted,
                PLAN,
                Some(PREVIOUS_GENERATION),
                Some(PREVIOUS_PLAN),
            )
            .is_ok()
        );
        aborted.aborted = false;
        assert!(
            validate_abort_consumer_generation_response(
                &aborted,
                PLAN,
                Some(PREVIOUS_GENERATION),
                Some(PREVIOUS_PLAN),
            )
            .is_ok(),
            "an exact tombstone retry is converged even though it has no new side effect"
        );
        aborted.active_deployment_id = PLAN.to_string();
        assert!(
            validate_abort_consumer_generation_response(
                &aborted,
                PLAN,
                Some(PREVIOUS_GENERATION),
                Some(PREVIOUS_PLAN),
            )
            .is_err()
        );
    }

    /// A version row as the catalog would have written it. Never inserted
    /// here: these tests exercise what is built from one, not the store.
    fn version(manifest: &str) -> Version {
        Version {
            id: "v-1".to_string(),
            canonical_id: "018f2518-7e38-7f80-8000-000000000001".parse().unwrap(),
            app_id: "demo".to_string(),
            number: 1,
            bundle_sha: "abc123".to_string(),
            bundle_path: "/tmp/never-read.zip".to_string(),
            manifest: manifest.to_string(),
            canonical_artifact_id: None,
            provider_derivation_sha256: None,
            created_at: 0,
        }
    }

    async fn deliverer(secrets_ready: bool) -> (DeploymentDeliverer, Arc<CatalogStore>) {
        deliverer_with(
            secrets_ready,
            authenticated_wiring(),
        )
        .await
    }

    /// A test identity whose signing key is deterministic, so two tests
    /// that each build their own issuer from this string will still produce
    /// tokens the other can verify.
    const ISSUER_ID: &str = "spiffe://deislet/control/test";

    /// A control plane wired the way every install is: addresses plus the
    /// CapabilityIssuer its own queue calls are signed by.
    fn authenticated_wiring() -> StorageWiring {
        StorageWiring {
            store: Some("http://127.0.0.1:8090".to_string()),
            queue: Some("http://127.0.0.1:8091".to_string()),
            issuer: CapabilityIssuer::for_test(ISSUER_ID),
            service_trust: None,
        }
    }

    /// The same issuer, and no service configured at all.
    fn no_services_wiring() -> StorageWiring {
        StorageWiring {
            store: None,
            queue: None,
            issuer: CapabilityIssuer::for_test(ISSUER_ID),
            service_trust: None,
        }
    }

    async fn deliverer_with(
        secrets_ready: bool,
        storage: StorageWiring,
    ) -> (DeploymentDeliverer, Arc<CatalogStore>) {
        let database = Arc::new(Database::init(":memory:").await.unwrap());
        let store = Arc::new(CatalogStore::new(database));
        let secrets = if secrets_ready {
            SecretsBackend::Ready(Box::new(
                SecretCipher::from_key_bytes(&[42u8; MASTER_KEY_LEN]).unwrap(),
            ))
        } else {
            SecretsBackend::Unavailable("no key file in this test".to_string())
        };
        let deliverer = DeploymentDeliverer::new(
            Arc::clone(&store),
            Arc::new(secrets),
            // No nodes: these tests are about what gets built, not about
            // what a node does with it.
            Arc::new(RuntimeNodeManager::new(Vec::new(), 15)),
            Arc::new(PublicAddress {
                configured: Some("http://control.test".to_string()),
                fallback: "http://127.0.0.1:8080".to_string(),
                machine: None,
            }),
            storage,
        );
        (deliverer, store)
    }

    async fn environment(store: &CatalogStore) -> Environment {
        store.create_app("demo").await.unwrap();
        store
            .create_environment("demo", "production")
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn a_manifest_without_declarations_is_delivered_no_secret() {
        let (deliverer, store) = deliverer(true).await;
        let env = environment(&store).await;
        let app = store.get_app("demo").await.unwrap();

        let cipher = deliverer.secrets.cipher().unwrap();
        let revision = deis_wire::CanonicalId::new_v7().to_string();
        let blob = cipher
            .encrypt(
                b"s3cr3t",
                &secret_revision_aad(&env.id, "API_KEY", &revision),
            )
            .unwrap();
        store
            .put_secret_revision(&env, "API_KEY", &revision, blob)
            .await
            .unwrap();

        let payload = deliverer
            .payload(&env, &version(MANIFEST), "http://control.test")
            .await
            .unwrap();

        assert_eq!(payload.app_id, "demo");
        assert_eq!(payload.canonical_application_id, app.canonical_id);
        assert_eq!(payload.environment, "production");
        assert_eq!(payload.canonical_environment_id, env.canonical_id);
        assert_eq!(
            payload.bundle_url,
            "http://control.test/api/apps/demo/versions/v-1/bundle"
        );
        // A value is stored, but nothing in this manifest binds it: a secret
        // reaches a node only through a snapshot and `FetchSecretValues`.
        assert!(payload.secret_names().is_empty());
        assert_eq!(payload.variable_names(), ["API_KEY", "LOG_LEVEL"]);

        let request = payload.to_request();
        assert_eq!(request.env.get("LOG_LEVEL").unwrap(), "info");
        assert_eq!(
            request.env.get("API_KEY").unwrap(),
            "placeholder-from-manifest"
        );
        assert!(
            request.env.values().all(|value| value != "s3cr3t"),
            "a decrypted secret travelled in the deployment's env"
        );
        assert!(request.environment_snapshot_json.is_empty());
        assert_eq!(request.bundle_sha, "abc123");
        assert_eq!(
            request.canonical_application_id,
            app.canonical_id.to_string()
        );
        assert_eq!(
            request.canonical_environment_id,
            env.canonical_id.to_string()
        );
    }

    fn standard_manifest(declarations: &deis_wire::EnvironmentDeclarations) -> String {
        serde_json::json!({
            "schema": 2,
            "app": "demo",
            "entry": "worker/index.js",
            "environment": declarations,
        })
        .to_string()
    }

    #[tokio::test]
    async fn standard_payload_binds_typed_config_and_an_exact_secret_revision() {
        let (deliverer, store) = deliverer(true).await;
        let env = environment(&store).await;
        let declarations = deis_wire::EnvironmentDeclarations::new(
            deis_wire::CURRENT_EDGE_CANON_STANDARD_VERSION,
            vec![
                deis_wire::EnvironmentDeclaration {
                    name: "LABEL".to_string(),
                    kind: deis_wire::EnvironmentKind::Config,
                    value_type: deis_wire::EnvironmentValueType::String,
                    required: true,
                },
                deis_wire::EnvironmentDeclaration {
                    name: "OPTIONS".to_string(),
                    kind: deis_wire::EnvironmentKind::Config,
                    value_type: deis_wire::EnvironmentValueType::Json,
                    required: true,
                },
                deis_wire::EnvironmentDeclaration {
                    name: "TOKEN".to_string(),
                    kind: deis_wire::EnvironmentKind::Secret,
                    value_type: deis_wire::EnvironmentValueType::String,
                    required: true,
                },
            ],
        );
        let revision = deis_wire::CanonicalId::new_v7().to_string();
        let sentinel = "standard-secret-sentinel";
        let ciphertext = deliverer
            .secrets
            .cipher()
            .unwrap()
            .encrypt(
                sentinel.as_bytes(),
                &secret_revision_aad(&env.id, "TOKEN", &revision),
            )
            .unwrap();
        store
            .put_secret_revision(&env, "TOKEN", &revision, ciphertext)
            .await
            .unwrap();
        let config_values = BTreeMap::from([
            ("LABEL".to_string(), serde_json::json!("edge")),
            (
                "OPTIONS".to_string(),
                serde_json::json!({"retries": 2, "nested": [true, null]}),
            ),
        ]);

        let payload = deliverer
            .payload_with_config(
                &env,
                &version(&standard_manifest(&declarations)),
                "http://control.test",
                config_values,
            )
            .await
            .unwrap();
        assert_eq!(payload.variable_count(), 2);
        assert_eq!(payload.secret_count(), 1);
        assert!(!format!("{payload:?}").contains(sentinel));

        let request = payload.to_request();
        assert!(!request.env.contains_key("LABEL"));
        assert!(!request.env.contains_key("OPTIONS"));
        assert!(!request.env.contains_key("TOKEN"));
        let snapshot: deis_wire::EnvironmentBindingSnapshot =
            serde_json::from_str(&request.environment_snapshot_json).unwrap();
        assert_eq!(
            snapshot.identity().unwrap(),
            request.environment_snapshot_id
        );
        assert_eq!(snapshot.environment_id, env.canonical_id.to_string());
        assert_eq!(snapshot.bindings.len(), 3);
        assert_eq!(
            serde_json::from_str::<deis_wire::EnvironmentDeclarations>(
                &request.environment_declarations_json
            )
            .unwrap(),
            declarations
        );
    }

    #[tokio::test]
    async fn standard_payload_rejects_missing_and_undeclared_config() {
        let (deliverer, store) = deliverer(true).await;
        let env = environment(&store).await;
        let declarations = deis_wire::EnvironmentDeclarations::new(
            deis_wire::CURRENT_EDGE_CANON_STANDARD_VERSION,
            vec![deis_wire::EnvironmentDeclaration {
                name: "REQUIRED".to_string(),
                kind: deis_wire::EnvironmentKind::Config,
                value_type: deis_wire::EnvironmentValueType::String,
                required: true,
            }],
        );
        let version = version(&standard_manifest(&declarations));

        let missing = deliverer
            .payload_with_config(&env, &version, "http://control.test", BTreeMap::new())
            .await
            .unwrap_err();
        assert!(missing.to_string().contains("EC_ENV_REQUIRED_MISSING"));

        let extra = deliverer
            .payload_with_config(
                &env,
                &version,
                "http://control.test",
                BTreeMap::from([
                    ("REQUIRED".to_string(), serde_json::json!("present")),
                    ("UNDECLARED".to_string(), serde_json::json!("no")),
                ]),
            )
            .await
            .unwrap_err();
        assert!(extra.to_string().contains("EC_ENV_BINDING_UNDECLARED"));
    }

    /// What a node has to be told before it downloads anything: which day's
    /// platform this version expects.
    #[tokio::test]
    async fn a_manifest_that_names_a_compatibility_date_puts_it_on_the_wire() {
        let (deliverer, store) = deliverer(true).await;
        let env = environment(&store).await;

        let manifest = r#"{"schema":1,"app":"demo","entry":"worker/index.js",
            "compatibility":{"date":"2026-09-01"}}"#;

        let payload = deliverer
            .payload(&env, &version(manifest), "http://control.test")
            .await
            .unwrap();

        assert_eq!(
            payload.compatibility.date.map(|d| d.to_string()).as_deref(),
            Some("2026-09-01")
        );

        let wire = payload
            .to_request()
            .compatibility
            .expect("the wire message must always carry an answer, even an empty one");
        assert_eq!(wire.date, "2026-09-01");
        assert!(
            wire.flags.is_empty(),
            "the manifest named no flag, so neither does the wire"
        );
    }

    /// The reason the declaration travels rather than the answers it resolves
    /// to: a flag that holds a behaviour *back* past its date disappears from
    /// any list of what is on, and a node reading such a list would apply the
    /// date and turn the behaviour on again - silently giving the application
    /// the one answer its author wrote a line to avoid.
    #[tokio::test]
    async fn a_flag_that_holds_a_behaviour_back_survives_the_wire() {
        let (deliverer, store) = deliverer(true).await;
        let env = environment(&store).await;

        let manifest = r#"{"schema":1,"app":"demo","entry":"worker/index.js",
            "compatibility":{"date":"2026-09-01","flags":["single_set_cookie_header"]}}"#;

        let wire = deliverer
            .payload(&env, &version(manifest), "http://control.test")
            .await
            .unwrap()
            .to_request()
            .compatibility
            .expect("an answer always travels");

        assert_eq!(wire.date, "2026-09-01");
        assert_eq!(wire.flags, vec!["single_set_cookie_header"]);
    }

    /// A version that declares nothing is not a version with nothing to say.
    /// The empty answer travels too, because an absent message on the wire
    /// would be indistinguishable from a control plane too old to have an
    /// opinion - and a node that had to guess between those two is the whole
    /// failure this mechanism exists to prevent.
    #[tokio::test]
    async fn a_manifest_with_no_date_still_puts_an_answer_on_the_wire() {
        let (deliverer, store) = deliverer(true).await;
        let env = environment(&store).await;

        let payload = deliverer
            .payload(&env, &version(MANIFEST), "http://control.test")
            .await
            .unwrap();

        assert_eq!(payload.compatibility.date, None);
        let wire = payload
            .to_request()
            .compatibility
            .expect("an undated version still carries its answer");
        assert_eq!(wire.date, "");
        assert!(wire.flags.is_empty());
    }

    /// A stored version this build cannot honour stops the delivery, where an
    /// unreadable `limits` block only costs the application its own narrower
    /// ceiling. The two are not the same kind of failure: an application run
    /// on the node's ceilings is slower than it asked to be, while an
    /// application run on answers it never named is wrong in a way nothing
    /// downstream will report.
    #[tokio::test]
    async fn a_stored_version_this_build_cannot_honour_is_not_delivered() {
        let (deliverer, store) = deliverer(true).await;
        let env = environment(&store).await;

        let manifest = r#"{"schema":1,"app":"demo","entry":"worker/index.js",
            "compatibility":{"date":"2099-01-01"}}"#;

        let err = deliverer
            .payload(&env, &version(manifest), "http://control.test")
            .await
            .expect_err("a date this build cannot honour must not be delivered");
        let message = err.to_string();
        assert!(message.contains("2099-01-01"), "{message}");
    }

    /// The gap this closed: `ApplyDeploymentRequest` carried `env` and
    /// nothing else, so the `ResourceLimits` message existed on the wire and
    /// no deployment ever filled it. Every application on every node ran on
    /// the node defaults whatever its manifest said.
    #[tokio::test]
    async fn a_manifest_that_narrows_its_limits_puts_them_on_the_wire() {
        let (deliverer, store) = deliverer(true).await;
        let env = environment(&store).await;

        let manifest = r#"{"schema":1,"app":"demo","entry":"worker/index.js",
            "limits":{"timeout_ms":2000,"cpu_time_limit_ms":250,"memory_mb":48,
                      "rate_limit_ops_per_sec":40.0,"resource_table_max":16}}"#;

        let payload = deliverer
            .payload(&env, &version(manifest), "http://control.test")
            .await
            .unwrap();

        let limits = payload
            .limits
            .clone()
            .expect("the manifest declares limits, so the payload must carry them");
        assert_eq!(limits.timeout_ms, Some(2_000));

        let wire = payload
            .to_request()
            .limits
            .expect("the wire message must carry what the payload holds");
        assert_eq!(wire.timeout_ms, 2_000);
        assert_eq!(wire.cpu_time_limit_ms, 250);
        assert_eq!(wire.memory_mb, 48);
        assert_eq!(wire.rate_limit_ops_per_sec, 40.0);
        assert_eq!(wire.resource_table_max, 16);
        // Fields nothing on a node enforces stay at the absent sentinel
        // rather than carrying a number the platform would not apply.
        assert_eq!(wire.max_concurrency, 0);
    }

    #[tokio::test]
    async fn an_application_that_declares_no_limits_sends_none() {
        let (deliverer, store) = deliverer(true).await;
        let env = environment(&store).await;

        let payload = deliverer
            .payload(&env, &version(MANIFEST), "http://control.test")
            .await
            .unwrap();

        assert!(payload.limits.is_none());
        assert!(
            payload.to_request().limits.is_none(),
            "an absent limits block must not become a message full of zeroes, which a node would \
             have to tell apart from a real declaration"
        );
    }

    /// A version stored before the upload check existed can still hold a
    /// limits block nothing can read. Refusing to deploy it would take a
    /// running application off the air over a field that only ever narrows
    /// what it is allowed, so the delivery goes out on the node's ceilings.
    #[tokio::test]
    async fn a_limits_block_that_cannot_be_read_does_not_stop_the_deployment() {
        let (deliverer, store) = deliverer(true).await;
        let env = environment(&store).await;

        let manifest =
            r#"{"schema":1,"app":"demo","entry":"worker/index.js","limits":{"timeout_ms":0}}"#;

        let payload = deliverer
            .payload(&env, &version(manifest), "http://control.test")
            .await
            .expect("an unreadable limits block must not fail the delivery");
        assert!(payload.limits.is_none());
        assert!(payload.to_request().limits.is_none());
    }

    #[tokio::test]
    async fn the_wire_message_carries_the_storage_addresses_the_app_never_declared() {
        // Without these two keys deis-runtime builds no storage client at
        // all and every ctx.services call answers "service not available".
        let (deliverer, store) = deliverer(true).await;
        let env = environment(&store).await;

        let payload = deliverer
            .payload(&env, &version(MANIFEST), "http://control.test")
            .await
            .unwrap();

        // The report stays about the application: the addresses are the
        // platform's, not a variable someone wrote in a manifest. No secret
        // is stored here, so API_KEY is still a plain manifest variable.
        assert_eq!(payload.variable_names(), ["API_KEY", "LOG_LEVEL"]);

        let request = payload.to_request();
        assert_eq!(
            request.env.get(deis_wire::conventions::STORE_ENDPOINT_ENV),
            Some(&"http://127.0.0.1:8090".to_string())
        );
        assert_eq!(
            request.env.get(deis_wire::conventions::QUEUE_ENDPOINT_ENV),
            Some(&"http://127.0.0.1:8091".to_string())
        );
    }

    #[tokio::test]
    async fn an_application_cannot_point_its_storage_client_somewhere_else() {
        let (deliverer, store) = deliverer(true).await;
        let env = environment(&store).await;
        let manifest = format!(
            r#"{{"schema":1,"app":"demo","entry":"worker/index.js","env":{{"{}":"http://attacker.invalid"}}}}"#,
            deis_wire::conventions::STORE_ENDPOINT_ENV
        );

        let request = deliverer
            .payload(&env, &version(&manifest), "http://control.test")
            .await
            .unwrap()
            .to_request();

        assert_eq!(
            request.env.get(deis_wire::conventions::STORE_ENDPOINT_ENV),
            Some(&"http://127.0.0.1:8090".to_string()),
            "the platform address must win over anything the bundle declared"
        );
    }

    /// No credential travels with a deployment: a node asks for its own
    /// capabilities over the machine channel, under its own name. A token in
    /// the env map would be a bearer credential for the tenant that anyone
    /// who saw the deployment message could replay.
    #[tokio::test]
    async fn a_deployment_carries_no_storage_or_queue_credential() {
        let (deliverer, store) = deliverer_with(true, authenticated_wiring()).await;
        let production = environment(&store).await;
        let preview = store.create_environment("demo", "preview-7").await.unwrap();

        for env in [&production, &preview] {
            let request = deliverer
                .payload(env, &version(MANIFEST), "http://control.test")
                .await
                .unwrap()
                .to_request();
            for (key, value) in &request.env {
                assert!(
                    !key.contains("TOKEN") && !value.starts_with("v1."),
                    "{key} travelled in the deployment of {}",
                    env.name
                );
            }
            assert!(request.env.contains_key(deis_wire::conventions::STORE_ENDPOINT_ENV));
        }
    }

    /// The control plane's own queue calls carry a queue-audience token in
    /// `Authorization: Bearer`, which is where deis-queue reads it.
    #[tokio::test]
    async fn a_queue_call_from_control_carries_a_queue_capability_as_a_bearer() {
        let wiring = authenticated_wiring();
        let issuer = wiring.issuer.clone();
        let mut request = tonic::Request::new(());
        wiring
            .stamp_queue_capability(&mut request, &CanonicalTenantId::derived_from_label("demo.production"))
            .await
            .unwrap();
        let header = request.metadata().get("authorization").unwrap().to_str().unwrap();
        let token = header.strip_prefix("Bearer ").expect("a bearer credential");
        deis_capability::CapabilityVerifier::new(issuer.verifying_key().await)
            .verify(
                token,
                AUD_QUEUE,
                &CanonicalTenantId::derived_from_label("demo.production").to_string(),
                "queue",
            )
            .expect("deis-queue accepts it");
    }

    #[test]
    fn the_issuer_never_reaches_a_log_line() {
        // The one value here that can mint a token for any tenant. A
        // derived Debug would put it in whatever line printed a payload.
        let rendered = format!("{:?}", authenticated_wiring());
        assert!(!rendered.contains("issuer"), "{rendered}");
        assert!(rendered.contains("127.0.0.1:8090"), "{rendered}");
    }

    #[tokio::test]
    async fn a_control_plane_without_storage_configured_sends_no_addresses() {
        // A valid setup: the deployment still goes out, and only the app's
        // ctx.services calls fail.
        let (deliverer, store) = deliverer_with(true, no_services_wiring()).await;
        let env = environment(&store).await;

        let request = deliverer
            .payload(&env, &version(MANIFEST), "http://control.test")
            .await
            .unwrap()
            .to_request();

        assert!(
            !request
                .env
                .contains_key(deis_wire::conventions::STORE_ENDPOINT_ENV)
        );
        assert!(
            !request
                .env
                .contains_key(deis_wire::conventions::QUEUE_ENDPOINT_ENV)
        );
    }

    /// Without a configured address the platform has nothing to overwrite
    /// the manifest's value with, so the key has to be *removed*. Leaving
    /// it would let a bundle name the host its own kv, database and blob
    /// clients dial.
    #[tokio::test]
    async fn an_application_cannot_name_a_storage_address_the_platform_never_configured() {
        let (deliverer, store) = deliverer_with(true, no_services_wiring()).await;
        let env = environment(&store).await;
        let manifest = format!(
            r#"{{"schema":1,"app":"demo","entry":"worker/index.js","env":{{"{}":"http://attacker.invalid","{}":"http://attacker.invalid","KEEP":"mine"}}}}"#,
            deis_wire::conventions::STORE_ENDPOINT_ENV,
            deis_wire::conventions::QUEUE_ENDPOINT_ENV
        );

        let request = deliverer
            .payload(&env, &version(&manifest), "http://control.test")
            .await
            .unwrap()
            .to_request();

        for key in [
            deis_wire::conventions::STORE_ENDPOINT_ENV,
            deis_wire::conventions::QUEUE_ENDPOINT_ENV,
        ] {
            assert!(
                !request.env.contains_key(key),
                "{key} reached the node as the bundle declared it"
            );
        }
        assert_eq!(request.env.get("KEEP").unwrap(), "mine");
    }

    /// Deleting an application has to reach the fleet: a node only ever
    /// adds what its assignments name, so one that is not told keeps
    /// serving the app - with its secrets - until it restarts.
    #[tokio::test]
    async fn removing_an_app_reports_every_node_that_is_still_serving_it() {
        let (ready, store) = deliverer(true).await;
        let deliverer = DeploymentDeliverer::new(
            Arc::clone(&store),
            Arc::new(SecretsBackend::Unavailable("not needed here".to_string())),
            Arc::new(RuntimeNodeManager::new(
                vec![crate::config::RuntimeNodeConfig {
                    id: "node-a".to_string(),
                    // Nothing listens here, so the call is refused at once.
                    addr: "http://127.0.0.1:1".to_string(),
                }],
                15,
            )),
            Arc::clone(&ready.address),
            no_services_wiring(),
        );

        let report = deliverer
            .remove_app("demo", vec!["production".to_string()])
            .await;
        assert!(report.unloaded.is_empty());
        assert_eq!(report.failed.len(), 1);
        assert_eq!(report.failed[0].node, "node-a");
        assert!(
            !report.fully_removed(),
            "a node that was not reached is still serving the deleted app"
        );
        assert_eq!(report.environments, ["production"]);
    }

    #[tokio::test]
    async fn removing_an_app_on_a_fleet_of_no_nodes_is_skipped_rather_than_failed() {
        let (deliverer, _store) = deliverer(true).await;
        let report = deliverer.remove_app("demo", Vec::new()).await;
        assert!(report.skipped.is_some());
        assert!(
            report.fully_removed(),
            "nothing is serving it, so nothing is left behind"
        );
    }

    /// The payload is passed around and may end up in an error path; its
    /// debug rendering must never carry a value.
    #[tokio::test]
    async fn debug_output_never_carries_a_secret_value() {
        let (deliverer, store) = deliverer(true).await;
        let env = environment(&store).await;
        let revision = deis_wire::CanonicalId::new_v7().to_string();
        let blob = deliverer
            .secrets
            .cipher()
            .unwrap()
            .encrypt(
                b"s3cr3t",
                &secret_revision_aad(&env.id, "API_KEY", &revision),
            )
            .unwrap();
        store
            .put_secret_revision(&env, "API_KEY", &revision, blob)
            .await
            .unwrap();

        let payload = deliverer
            .payload(&env, &version(MANIFEST), "http://control.test")
            .await
            .unwrap();
        let rendered = format!("{payload:?}");
        assert!(!rendered.contains("s3cr3t"), "got {rendered}");
        assert!(
            !rendered.contains("placeholder-from-manifest"),
            "{rendered}"
        );
        assert!(
            rendered.contains("API_KEY"),
            "secret names are safe to show; got {rendered}"
        );
        assert!(
            rendered.contains("redacted"),
            "values field still uses the redacted guard; got {rendered}"
        );
    }

    /// An app with no stored secrets must deploy even when the control
    /// plane has no master key, or a misconfiguration nobody needs would
    /// block every deployment.
    #[tokio::test]
    async fn an_app_without_secrets_needs_no_master_key() {
        let (deliverer, store) = deliverer(false).await;
        let env = environment(&store).await;

        let payload = deliverer
            .payload(&env, &version(MANIFEST), "http://control.test")
            .await
            .unwrap();
        assert_eq!(payload.secret_count(), 0);
        assert_eq!(payload.variable_count(), 2);
    }

    /// With a declared secret stored but no usable key, the deployment must
    /// fail rather than reach the node with the secret silently absent.
    #[tokio::test]
    async fn a_stored_secret_without_a_master_key_refuses_to_deliver() {
        let (ready, store) = deliverer(true).await;
        let env = environment(&store).await;
        let revision = deis_wire::CanonicalId::new_v7().to_string();
        let blob = ready
            .secrets
            .cipher()
            .unwrap()
            .encrypt(
                b"s3cr3t",
                &secret_revision_aad(&env.id, "API_KEY", &revision),
            )
            .unwrap();
        store
            .put_secret_revision(&env, "API_KEY", &revision, blob)
            .await
            .unwrap();
        let declarations = deis_wire::EnvironmentDeclarations::new(
            deis_wire::CURRENT_EDGE_CANON_STANDARD_VERSION,
            vec![deis_wire::EnvironmentDeclaration {
                name: "API_KEY".to_string(),
                kind: deis_wire::EnvironmentKind::Secret,
                value_type: deis_wire::EnvironmentValueType::String,
                required: true,
            }],
        );

        let broken = DeploymentDeliverer::new(
            Arc::clone(&store),
            Arc::new(SecretsBackend::Unavailable(
                "key file went away".to_string(),
            )),
            Arc::new(RuntimeNodeManager::new(Vec::new(), 15)),
            Arc::clone(&ready.address),
            no_services_wiring(),
        );
        let err = broken
            .payload(
                &env,
                &version(&standard_manifest(&declarations)),
                "http://control.test",
            )
            .await
            .unwrap_err();
        assert!(
            matches!(err, CatalogError::SecretsUnavailable(_)),
            "got {err:?}"
        );
    }

    #[tokio::test]
    async fn a_corrupt_manifest_refuses_to_deliver() {
        let (deliverer, store) = deliverer(true).await;
        let env = environment(&store).await;
        let err = deliverer
            .payload(&env, &version("{not json"), "http://control.test")
            .await
            .unwrap_err();
        assert!(matches!(err, CatalogError::BundleStorage(_)), "got {err:?}");
    }

    #[tokio::test]
    async fn a_fleet_of_no_nodes_reports_the_push_as_skipped() {
        let (deliverer, store) = deliverer(true).await;
        let env = environment(&store).await;
        let payload = deliverer
            .payload(&env, &version(MANIFEST), "http://control.test")
            .await
            .unwrap();

        let report = deliverer.push(&payload, DeliveryReason::Deployment).await;
        assert!(report.skipped.is_some());
        assert!(report.delivered.is_empty());
        assert!(report.failed.is_empty());
        assert!(!report.fully_delivered());
    }

    /// A node that cannot be reached is represented in the report; the
    /// coordinator, not this transport helper, owns the phase consequence.
    #[tokio::test]
    async fn an_unreachable_node_is_reported_and_never_fails_the_push() {
        let (ready, store) = deliverer(true).await;
        let env = environment(&store).await;
        let deliverer = DeploymentDeliverer::new(
            Arc::clone(&store),
            Arc::new(SecretsBackend::Ready(Box::new(
                SecretCipher::from_key_bytes(&[42u8; MASTER_KEY_LEN]).unwrap(),
            ))),
            Arc::new(RuntimeNodeManager::new(
                vec![crate::config::RuntimeNodeConfig {
                    id: "node-a".to_string(),
                    // Nothing listens here, so the connection is refused
                    // immediately rather than waiting out a timeout.
                    addr: "http://127.0.0.1:1".to_string(),
                }],
                15,
            )),
            Arc::clone(&ready.address),
            no_services_wiring(),
        );

        let payload = deliverer
            .payload(&env, &version(MANIFEST), "http://control.test")
            .await
            .unwrap();
        let report = deliverer.push(&payload, DeliveryReason::Deployment).await;

        assert!(report.delivered.is_empty());
        assert_eq!(report.failed.len(), 1);
        assert_eq!(report.failed[0].node, "node-a");
        assert!(report.skipped.is_none(), "a node was contacted");
        assert!(!report.fully_delivered());
        // The report travels back to an API caller; it must not leak the
        // environment it was built from.
        let rendered = serde_json::to_string(&report).unwrap();
        assert!(!rendered.contains("s3cr3t"));
    }

    /// A fleet with one node whose registration has already run out. The
    /// node is not on `ids()` any more, which is the whole point: this
    /// control plane cannot reach it and it is not asking for its
    /// assignments either, so nothing corrects it.
    fn fleet_with_one_lost_node() -> Arc<RuntimeNodeManager> {
        let nodes = Arc::new(RuntimeNodeManager::new_with_ttl(Vec::new(), 15, 60));
        nodes
            .register(
                "node-b",
                crate::runtime_client::NodeClaim {
                    addr: "http://127.0.0.1:1".to_string(),
                    machine_addr: None,
                    version: None,
                    identity: deis_identity::names::node("test.deislet.local", "node-b")
                        .expect("a node name"),
                },
                Utc::now() - chrono::Duration::hours(1),
            )
            .unwrap();
        assert_eq!(nodes.expire_stale_registrations(Utc::now()), ["node-b"]);
        assert!(nodes.ids().is_empty(), "it has left the fleet list");
        nodes
    }

    /// A fleet where one machine was refused a node id another machine is
    /// still answering for. The refused machine has no entry of its own, so
    /// `ids()` shows one node where two are running.
    async fn fleet_with_one_refused_machine() -> Arc<RuntimeNodeManager> {
        let nodes = Arc::new(RuntimeNodeManager::new_with_ttl(Vec::new(), 15, 60));
        let now = Utc::now();
        nodes
            .register_claim_with(
                "node-a",
                crate::runtime_client::NodeClaim {
                    addr: "http://127.0.0.1:1".to_string(),
                    machine_addr: None,
                    version: None,
                    identity: deis_identity::names::node("test.deislet.local", "node-a")
                        .expect("a node name"),
                },
                now,
                |_| async { None },
            )
            .await
            .unwrap();
        nodes
            .register_claim_with(
                "node-a",
                crate::runtime_client::NodeClaim {
                    addr: "http://127.0.0.2:1".to_string(),
                    machine_addr: None,
                    version: None,
                    identity: deis_identity::names::node("test.deislet.local", "node-a")
                        .expect("a node name"),
                },
                now,
                |_| async {
                    Some(crate::runtime_client::NodeIdentity {
                        node_id: "node-a".to_string(),
                        hostname: "box-one".to_string(),
                        uptime_seconds: 4_212,
                    })
                },
            )
            .await
            .unwrap_err();
        assert_eq!(nodes.ids(), ["node-a".to_string()], "one id, two machines");
        nodes
    }

    /// Two machines sharing a node id used to be one node to this control
    /// plane: the push went to whichever address had registered last, was
    /// counted against the single entry, and the deploy came back reporting
    /// the whole fleet as delivered while the other machine went on serving
    /// the previous version.
    #[tokio::test]
    async fn a_deploy_reports_the_machine_that_was_refused_the_node_id() {
        let (ready, store) = deliverer(true).await;
        let env = environment(&store).await;
        let deliverer = DeploymentDeliverer::new(
            Arc::clone(&store),
            Arc::new(SecretsBackend::Unavailable("not needed here".to_string())),
            fleet_with_one_refused_machine().await,
            Arc::clone(&ready.address),
            no_services_wiring(),
        );

        let payload = deliverer
            .payload(&env, &version(MANIFEST), "http://control.test")
            .await
            .unwrap();
        let report = deliverer.push(&payload, DeliveryReason::Deployment).await;

        // Two entries for one id, which is the truth: one machine was asked
        // and one was never reachable at all.
        assert_eq!(report.failed.len(), 2, "got {:?}", report.failed);
        let refused = report
            .failed
            .iter()
            .find(|failure| failure.error.contains("registered this node id"))
            .expect("the refused machine has to be named");
        assert!(
            refused.error.contains("http://127.0.0.2:1"),
            "the answer has to name the machine nothing was sent to: {}",
            refused.error
        );
        assert!(!report.fully_delivered());
    }

    /// A node that fell off the fleet list used to appear in neither
    /// `delivered` nor `failed`, so a deploy that reached nothing at all
    /// came back describing an empty, entirely successful fleet.
    #[tokio::test]
    async fn a_deploy_reports_the_node_that_fell_off_the_fleet_list() {
        let (ready, store) = deliverer(true).await;
        let env = environment(&store).await;
        let deliverer = DeploymentDeliverer::new(
            Arc::clone(&store),
            Arc::new(SecretsBackend::Unavailable("not needed here".to_string())),
            fleet_with_one_lost_node(),
            Arc::clone(&ready.address),
            no_services_wiring(),
        );

        let payload = deliverer
            .payload(&env, &version(MANIFEST), "http://control.test")
            .await
            .unwrap();
        let report = deliverer.push(&payload, DeliveryReason::Deployment).await;

        assert!(report.delivered.is_empty());
        assert_eq!(report.failed.len(), 1, "got {:?}", report.failed);
        assert_eq!(report.failed[0].node, "node-b");
        assert!(
            report.failed[0].error.contains("stopped refreshing"),
            "the answer has to say why: {}",
            report.failed[0].error
        );
        assert!(
            report.skipped.is_none(),
            "an empty list is not an empty fleet when a node was lost from it"
        );
        assert!(!report.fully_delivered());
    }

    /// The same hole, on the operation that cannot be repaired afterwards:
    /// a node that never hears the unload keeps running the deleted
    /// application, its decrypted secrets and its queue consumers.
    #[tokio::test]
    async fn deleting_an_app_reports_the_node_that_fell_off_the_fleet_list() {
        let (ready, store) = deliverer(true).await;
        let deliverer = DeploymentDeliverer::new(
            Arc::clone(&store),
            Arc::new(SecretsBackend::Unavailable("not needed here".to_string())),
            fleet_with_one_lost_node(),
            Arc::clone(&ready.address),
            no_services_wiring(),
        );

        let report = deliverer
            .remove_app("demo", vec!["production".to_string()])
            .await;
        assert!(report.unloaded.is_empty());
        assert_eq!(report.failed.len(), 1, "got {:?}", report.failed);
        assert_eq!(report.failed[0].node, "node-b");
        assert!(report.skipped.is_none());
        assert!(
            !report.fully_removed(),
            "this node is still serving the deleted application"
        );
    }

    /// A node coming back clears the record, so the fleet stops being
    /// reported as incomplete once it really is complete again.
    #[tokio::test]
    async fn a_node_that_registers_again_is_no_longer_reported_as_lost() {
        let (ready, store) = deliverer(true).await;
        let nodes = fleet_with_one_lost_node();
        let deliverer = DeploymentDeliverer::new(
            Arc::clone(&store),
            Arc::new(SecretsBackend::Unavailable("not needed here".to_string())),
            Arc::clone(&nodes),
            Arc::clone(&ready.address),
            no_services_wiring(),
        );

        nodes
            .register(
                "node-b",
                crate::runtime_client::NodeClaim {
                    addr: "http://127.0.0.1:1".to_string(),
                    machine_addr: None,
                    version: None,
                    identity: deis_identity::names::node("test.deislet.local", "node-b")
                        .expect("a node name"),
                },
                Utc::now(),
            )
            .unwrap();
        assert!(nodes.departed().is_empty());

        let report = deliverer.remove_app("demo", Vec::new()).await;
        // Back on the list, so it is dialled and fails as an unreachable
        // node rather than as a node nobody can account for.
        assert_eq!(report.failed.len(), 1);
        assert!(!report.failed[0].error.contains("stopped refreshing"));
    }

    /// An operator whose machine is really gone has to be able to say so,
    /// or every deploy from now on names a node that no longer exists.
    #[test]
    fn deregistering_a_lost_node_clears_the_record() {
        let nodes = fleet_with_one_lost_node();
        assert_eq!(nodes.departed().len(), 1);
        assert_eq!(
            nodes.deregister("node-b"),
            Some(crate::runtime_client::Deregistered::Forgotten)
        );
        assert!(nodes.departed().is_empty());
        assert_eq!(nodes.deregister("node-b"), None);
    }

    #[tokio::test]
    async fn reloading_an_environment_that_was_never_deployed_is_not_an_error() {
        let (deliverer, store) = deliverer(true).await;
        let env = environment(&store).await;

        let report = deliverer
            .reload_current(&env, "http://control.test", DeliveryReason::SecretChange)
            .await;
        assert!(report.skipped.unwrap().contains("never been deployed"));
    }

    #[test]
    fn public_address_uses_only_configuration_or_the_safe_fallback() {
        let address = PublicAddress {
            configured: Some("https://deis.example.com/".to_string()),
            fallback: "http://127.0.0.1:8080".to_string(),
            machine: None,
        };
        let mut headers = HeaderMap::new();
        headers.insert(
            axum::http::header::HOST,
            "attacker.invalid:9000".parse().unwrap(),
        );
        assert_eq!(address.resolve_human(&headers), "https://deis.example.com");

        let address = PublicAddress {
            configured: None,
            fallback: "http://127.0.0.1:8080".to_string(),
            machine: None,
        };
        assert_eq!(address.resolve_human(&headers), "http://127.0.0.1:8080");
        assert_eq!(
            address.resolve_human(&HeaderMap::new()),
            "http://127.0.0.1:8080"
        );
    }

    #[test]
    fn machine_base_prefers_the_machine_listener_public_url() {
        let address = PublicAddress {
            configured: Some("https://deis.example.com".to_string()),
            fallback: "http://127.0.0.1:8080".to_string(),
            machine: Some(MachineAddress {
                configured: Some("https://machine.example.com/".to_string()),
                fallback: "https://10.0.0.1:8443".to_string(),
            }),
        };

        // The trailing slash is dropped here and not where the URL is joined,
        // so a configured value written either way produces the same link.
        assert_eq!(
            address.resolve_machine(&HeaderMap::new()),
            "https://machine.example.com"
        );
        assert_eq!(
            address.resolve_machine_without_request(),
            "https://machine.example.com"
        );
        // The human address is untouched by any of this.
        assert_eq!(
            address.resolve_human(&HeaderMap::new()),
            "https://deis.example.com"
        );
    }

    #[test]
    fn machine_base_falls_back_to_the_machine_listener_bind_address() {
        // `[machine]` is configured without a public_base_url: the node is
        // pointed at the machine listener's own address, never at the human
        // one, because the bundle route has moved off the human listener.
        let address = PublicAddress {
            configured: Some("https://deis.example.com".to_string()),
            fallback: "http://127.0.0.1:8080".to_string(),
            machine: Some(MachineAddress {
                configured: None,
                fallback: "https://10.0.0.1:8443".to_string(),
            }),
        };

        assert_eq!(
            address.resolve_machine(&HeaderMap::new()),
            "https://10.0.0.1:8443"
        );
        assert_eq!(
            address.resolve_machine_without_request(),
            "https://10.0.0.1:8443"
        );
    }

    #[test]
    fn machine_base_without_a_machine_listener_is_the_human_base() {
        // The property this whole split is built to preserve: with no
        // `[machine]` section, a node is handed exactly the URL it is handed
        // today, whether the human address is configured or derived.
        let address = PublicAddress {
            configured: Some("https://deis.example.com/".to_string()),
            fallback: "http://127.0.0.1:8080".to_string(),
            machine: None,
        };
        assert_eq!(
            address.resolve_machine(&HeaderMap::new()),
            address.resolve_human(&HeaderMap::new())
        );
        assert_eq!(
            address.resolve_machine(&HeaderMap::new()),
            "https://deis.example.com"
        );

        let address = PublicAddress {
            configured: None,
            fallback: "http://127.0.0.1:8080".to_string(),
            machine: None,
        };
        assert_eq!(
            address.resolve_machine(&HeaderMap::new()),
            "http://127.0.0.1:8080"
        );
        assert_eq!(
            address.resolve_machine_without_request(),
            "http://127.0.0.1:8080"
        );
    }
}
