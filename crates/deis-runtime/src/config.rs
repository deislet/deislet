//! Node configuration: what this deis-runtime process reads from its TOML file
//! and from the environment.
//!
//! # One name per setting
//!
//! Every setting has exactly one spelling, and the environment variable is
//! derived from the TOML path by a single mechanical rule:
//!
//! ```text
//! DEIS_RUNTIME_ + <table>_<key>, upper-cased
//! ```
//!
//! so `http_port` is `DEIS_RUNTIME_HTTP_PORT`, and
//! `[default_execution_limits] timeout_ms` is
//! `DEIS_RUNTIME_DEFAULT_EXECUTION_LIMITS_TIMEOUT_MS`. There are no aliases and
//! no second spelling; guessing the variable from the TOML key (or the other
//! way round) always works. `DEIS_RUNTIME_CONFIG`, which selects the TOML file
//! itself, is the only variable that names no setting.
//!
//! Names that used to work but no longer do are listed in [`REMOVED_ENV_VARS`]
//! and make startup fail with a pointer to the current name, so an override
//! that silently stopped applying cannot be mistaken for one that worked.

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use std::env;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Environment variables that were once honoured, paired with the name that
/// replaced them. Setting one is a hard startup error rather than a no-op: an
/// operator who exports an override expects it to take effect, and the failure
/// mode of ignoring it (the node quietly running on defaults) is invisible.
const REMOVED_ENV_VARS: &[(&str, &str)] = &[
    (
        "DEIS_RUNTIME_DEFAULT_TIMEOUT_MS",
        "DEIS_RUNTIME_DEFAULT_EXECUTION_LIMITS_TIMEOUT_MS",
    ),
    (
        "DEIS_RUNTIME_DEFAULT_RATE_LIMIT_OPS_PER_SEC",
        "DEIS_RUNTIME_DEFAULT_EXECUTION_LIMITS_RATE_LIMIT_OPS_PER_SEC",
    ),
    ("PORT", "DEIS_RUNTIME_HTTP_PORT"),
    (
        "DEIS_RUNTIME_BUNDLES_MAX_BUNDLE_BYTES",
        "DEIS_RUNTIME_BUNDLES_MAX_BYTES",
    ),
];

/// TOML keys that were once honoured, paired with the name that replaced them,
/// written as `table.key`.
///
/// Same reasoning as [`REMOVED_ENV_VARS`]: a key that is quietly ignored looks
/// exactly like one that worked, and the setting it names is a ceiling, so the
/// node would run on a default the operator thought they had overridden.
const REMOVED_CONFIG_KEYS: &[(&str, &str)] = &[("bundles.max_bundle_bytes", "bundles.max_bytes")];

/// Smallest execution timeout that can be configured.
///
/// Below this the isolate is terminated before any realistic handler can
/// finish, which turns every request on the node into a timeout. A value that
/// low is always a mistake, so it is refused at startup instead of being
/// discovered from failing traffic.
const MIN_TIMEOUT_MS: u64 = 100;

/// Smallest CPU-time ceiling that can be configured, in milliseconds.
///
/// Two reasons for a floor an order of magnitude above [`MIN_TIMEOUT_MS`].
/// The first is the same one: below this every handler that does real work is
/// cut off, and the node answers 500 to traffic it could have served. The
/// second is measurement. The watched thread's CPU time is read out of
/// `/proc`, where it is counted in scheduler ticks -- 10ms on every Linux
/// build in practice -- so a ceiling near that granularity would be enforced
/// with an error of tens of percent, and two identical requests would land on
/// opposite sides of it. At 100ms the quantisation is at most a tenth of the
/// budget, and at the 5s default it is a fifth of a percent.
const MIN_CPU_TIME_LIMIT_MS: u64 = 100;

/// Smallest V8 heap ceiling that can be configured, in MB.
///
/// The isolate's own bootstrap -- the runtime's JavaScript, the module it
/// loads, the structures deno_core builds around them -- lives in this heap
/// before a single request arrives. Below this floor the near-heap-limit
/// callback fires during startup, so the node would refuse every application
/// it was assigned and the reason would look like an application fault.
const MIN_MEMORY_LIMIT_MB: usize = 32;

/// deis-proxy's default `upstream_read_timeout_secs`, in milliseconds.
///
/// The edge stops waiting for a node's answer after this long. A node's
/// admission budget has to be below it, or a request it starts may outlive the
/// connection the answer is written to; see [`RuntimeConfig::admission_wait_warning`].
/// The two crates do not depend on each other, so this is a copy of
/// `deis_proxy::config::system::DEFAULT_UPSTREAM_READ_TIMEOUT_SECS` and the two
/// numbers have to be changed together.
const PROXY_DEFAULT_UPSTREAM_READ_TIMEOUT_MS: u64 = 30_000;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimeConfig {
    /// How this node names itself to the control plane. Unset means "use the
    /// hostname", which is what a single node per host wants; two nodes on one
    /// host must each be given a distinct id, or they would ask for and report
    /// on the same assignment list.
    #[serde(default)]
    pub node_id: Option<String>,

    /// Port serving both HTTP traffic and the gRPC management interface.
    #[serde(default = "default_http_port")]
    pub http_port: u16,

    /// Directory holding this node's SQLite database and other local state.
    #[serde(default = "default_data_dir")]
    pub data_dir: PathBuf,

    /// How long a shutdown signal gives in-flight requests to finish before
    /// the node stops waiting and exits anyway.
    #[serde(default = "default_shutdown_grace_period_secs")]
    pub shutdown_grace_period_secs: u64,

    /// Largest request body this node will read, in bytes.
    ///
    /// The body is buffered whole before a handler sees it -- application code
    /// is given a `Request` it can call `arrayBuffer()` on, not a stream -- so
    /// this is the ceiling on what one request can cost this node in resident
    /// memory, and a node runs many requests at once beside every tenant's
    /// isolates. Without it a single upload decides how much memory the node
    /// has left.
    #[serde(default = "default_max_request_body_bytes")]
    pub max_request_body_bytes: usize,

    /// Ceiling on the V8 heap of one isolate, in MB, for every application on
    /// this node.
    ///
    /// The floor every application is held to. An application's own
    /// `ResourceLimits.memory_mb` may ask for *less* (see
    /// `crate::app::types::ResourceLimits`); one asking for more is refused
    /// and held here, because the node's memory is what is being protected and
    /// no tenant gets to widen it.
    ///
    /// Not part of [`ExecutionLimits`] even though it reads like a sibling of
    /// the timeout: those bound one *invocation* and are re-armed per request,
    /// while a heap belongs to the isolate and outlives every request that
    /// runs in it. Mixing the two would suggest the heap is reset between
    /// requests, which it is not.
    #[serde(default = "default_memory_limit_mb")]
    pub default_memory_limit_mb: usize,

    /// How long work posted to an application's runtime thread may wait for it
    /// before the node refuses that work, in milliseconds.
    ///
    /// One application runs on one runtime thread, and a thread runs what it
    /// has been given one piece at a time, so a slow invocation makes
    /// everything posted behind it wait. This is the ceiling on that wait:
    /// work still queued when the budget runs out is answered `503` with a
    /// `Retry-After` header instead of being started, because by then the
    /// caller has probably gone and running it would only take the thread away
    /// from the next request that is still waiting.
    ///
    /// It has to be below the edge's `upstream_read_timeout`, or a request can
    /// be admitted after deis-proxy has already abandoned it; the node warns
    /// at startup when this value is not below that setting's default.
    ///
    /// Must be greater than 0; 0 refuses every request that had to wait at
    /// all, which is every request that finds the thread busy.
    #[serde(default = "default_admission_wait_ms")]
    pub admission_wait_ms: u64,

    /// What a refusal at admission tells its caller to wait before retrying,
    /// in whole seconds: the value of the `Retry-After` header on the `503`
    /// the node answers with.
    ///
    /// The refusal is about a thread that is busy, not an application that
    /// is down, and a busy thread is usually free again within one
    /// invocation, so the default is the shortest whole second. A longer hint
    /// keeps the caller backing off past the moment the node could have
    /// served it; it is a hint, not a promise.
    ///
    /// Must be at least 1: 0 would tell a client to retry at once, which is
    /// the one thing a refusal at capacity is not.
    #[serde(default = "default_overload_retry_after_seconds")]
    pub overload_retry_after_seconds: u64,

    /// Platform-wide defaults for per-app execution limits. An app may override
    /// `timeout_ms` and `rate_limit_ops_per_sec` via its `ResourceLimits`
    /// config (see `crate::app::types::ResourceLimits`), in which case these
    /// are only the fallback; `wait_until_timeout_ms` has no per-app override,
    /// so every app on this node is held to the value set here.
    #[serde(default)]
    pub default_execution_limits: ExecutionLimits,

    /// Where this node's `target: "app"` logs are shipped, if anywhere.
    #[serde(default)]
    pub logging: LoggingConfig,

    /// How this node reaches the control plane to ask what it should run.
    #[serde(default)]
    pub control: ControlPlaneConfig,

    /// This node's mutual-TLS identity on the machine channel.
    ///
    /// It is how this node registers, fetches its assignments and downloads
    /// its bundles, and it is what admits the control plane to this node's
    /// management plane. There is nothing else: the fleet-wide value all of
    /// that once rode on no longer exists.
    ///
    /// `Option` only so a configuration without it can be read and then
    /// refused by name. A node that started without one would serve a
    /// management plane nobody is admitted to and fetch nothing, while
    /// looking healthy, so `main` refuses to start instead.
    #[serde(default)]
    pub machine_identity: Option<MachineIdentityConfig>,

    /// Where downloaded application bundles live and how large one may be.
    #[serde(default)]
    pub bundles: BundleConfig,

    /// How static assets out of a bundle are served.
    #[serde(default)]
    pub assets: AssetsConfig,

    /// Where an application's `fetch()` is allowed to go.
    #[serde(default)]
    pub fetch: FetchConfig,

    /// What this node says about its own reachability.
    #[serde(default)]
    pub node: NodeConfig,
}

fn default_http_port() -> u16 {
    8000
}

fn default_data_dir() -> PathBuf {
    PathBuf::from("./data")
}

fn default_shutdown_grace_period_secs() -> u64 {
    30
}

/// 32 MiB: comfortably above the uploads an edge handler is written for (a
/// form post, a JSON document, an image) and far below what would put the node
/// process under memory pressure. An application that needs more than this
/// belongs behind object storage, which takes the bytes directly.
///
/// deis-proxy carries the same number as its own
/// `config::system::DEFAULT_MAX_REQUEST_BODY_BYTES`, so an over-large upload
/// is refused at the edge instead of being streamed across the whole
/// proxy-to-node hop first. Nothing enforces the two agreeing yet: they are
/// declared in two crates that do not depend on each other. Change one and
/// change the other.
pub fn default_max_request_body_bytes() -> usize {
    32 * 1024 * 1024
}

/// 256 MB: several times what a request-shaped handler ever holds live, so no
/// honest application meets it, and small enough that a node sized for a
/// handful of concurrent isolates survives one of them running away.
pub fn default_memory_limit_mb() -> usize {
    256
}

/// 10 s: long enough that a request only meets it when the thread it is
/// waiting for is genuinely wedged behind something, and short enough to be
/// well inside the edge's 30 s upstream read timeout by default, so the answer
/// reaches a client that is still there. An application whose handlers
/// routinely run longer than this is telling its operator to raise the value
/// or move the slow work off the request path.
pub fn default_admission_wait_ms() -> u64 {
    10_000
}

/// 1 s: a busy thread is usually free again within one invocation, so a
/// short hint keeps the client from backing off past the moment the node
/// could have served it. See [`RuntimeConfig::overload_retry_after_seconds`].
pub fn default_overload_retry_after_seconds() -> u64 {
    1
}

impl Default for RuntimeConfig {
    fn default() -> Self {
        Self {
            node_id: None,
            http_port: default_http_port(),
            data_dir: default_data_dir(),
            shutdown_grace_period_secs: default_shutdown_grace_period_secs(),
            max_request_body_bytes: default_max_request_body_bytes(),
            default_memory_limit_mb: default_memory_limit_mb(),
            admission_wait_ms: default_admission_wait_ms(),
            overload_retry_after_seconds: default_overload_retry_after_seconds(),
            default_execution_limits: ExecutionLimits::default(),
            logging: LoggingConfig::default(),
            control: ControlPlaneConfig::default(),
            machine_identity: None,
            bundles: BundleConfig::default(),
            assets: AssetsConfig::default(),
            fetch: FetchConfig::default(),
            node: NodeConfig::default(),
        }
    }
}

/// What this node says about its own reachability, as a member of the fleet.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct NodeConfig {
    /// Addresses that route to this node but are not on its interfaces.
    ///
    /// The node's own data port is refused to applications under every
    /// address the machine can be reached at: loopback by construction, and
    /// every interface address by enumeration at startup. An address that
    /// only exists outside the machine -- a public IP a cloud provider 1:1
    /// NATs onto this node, with hairpin routing -- cannot be enumerated and
    /// has to be stated here. Without it, a worker that can name that address
    /// can reach the data port on it and invoke another tenant's application
    /// by writing its own `x-deis-app-id` header.
    ///
    /// The enumeration is a startup snapshot: interfaces that appear or
    /// change while the node runs (DHCP renewals, hot-plugged NICs) are not
    /// picked up, which is why an operator who knows their addresses route
    /// oddly lists them here. The real fix is authentication on the data
    /// port itself; this section is the stopgap until then.
    #[serde(default)]
    pub advertised_addresses: Vec<IpAddr>,
}

/// Reaching the control plane.
///
/// On startup the node asks the control plane which versions it should be
/// running (`GET /api/nodes/{node_id}/assignments`), fetches each bundle and
/// loads it, and only then reports healthy. That direction matters: the node
/// pulls, the control plane does not have to know how to reach every node
/// before anything can run.
///
/// With `endpoint` unset the node never asks anyone. It then runs whatever is
/// still in its own bundle directory from a previous life, plus whatever the
/// control plane pushes at it later, and it reports healthy immediately
/// because there is no list it could be missing.
///
/// Named for what it configures - how this node reaches the control plane -
/// rather than `ControlConfig`, which is already taken twice in this platform:
/// `deis_control::config::ControlConfig` is the control plane's own server
/// configuration, and `deis_wire::config::ControlConfig` is a gRPC client
/// configuration. Three types of one name, two of them for different things,
/// is a name that tells a reader nothing.
#[derive(Clone, Serialize, Deserialize, PartialEq)]
pub struct ControlPlaneConfig {
    /// Base URL of the control plane's human listener, e.g.
    /// `http://127.0.0.1:8080`.
    ///
    /// Not where this node fetches its assignments: that is the machine
    /// channel, named by `[machine_identity] control_endpoint`, and reaching
    /// it needs a certificate rather than an address. What is left here is the
    /// address an operator would open, kept because a startup line naming
    /// where this node's control plane is has to name something a person can
    /// follow.
    #[serde(default)]
    pub endpoint: Option<String>,

    /// Seconds one assignment request may take before it is abandoned.
    #[serde(default = "default_control_request_timeout_secs")]
    pub request_timeout_secs: u64,

    /// Seconds between retries after the control plane could not be reached.
    /// The node keeps serving what it has while it retries.
    #[serde(default = "default_control_retry_interval_secs")]
    pub retry_interval_secs: u64,

    /// Seconds between reconciliations once the node is up.
    ///
    /// The node asks the same question it asked at startup and treats the
    /// answer as the whole truth: it loads what it is not running and unloads
    /// what it no longer is assigned. Without this the two only ever agree by
    /// accident - a push that a node missed while it was restarting, or an
    /// application deleted while it was unreachable, leaves that node serving
    /// something nobody asked for until somebody notices.
    ///
    /// Longer means drift lives longer; shorter means one more request per
    /// node per interval, which is a single small JSON response.
    #[serde(default = "default_control_reconcile_interval_secs")]
    pub reconcile_interval_secs: u64,
}

fn default_control_request_timeout_secs() -> u64 {
    30
}

fn default_control_retry_interval_secs() -> u64 {
    15
}

fn default_control_reconcile_interval_secs() -> u64 {
    60
}

/// Hand-written, and kept hand-written after the credential it was written to
/// hide was removed. Nothing in this struct is secret any more, but a derived
/// `Debug` would print whatever is added to it next, and the next thing added
/// to a control-plane configuration is as likely as not to be a credential.
impl std::fmt::Debug for ControlPlaneConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ControlPlaneConfig")
            .field("endpoint", &self.endpoint)
            .field("request_timeout_secs", &self.request_timeout_secs)
            .field("retry_interval_secs", &self.retry_interval_secs)
            .field("reconcile_interval_secs", &self.reconcile_interval_secs)
            .finish()
    }
}

impl Default for ControlPlaneConfig {
    fn default() -> Self {
        Self {
            endpoint: None,
            request_timeout_secs: default_control_request_timeout_secs(),
            retry_interval_secs: default_control_retry_interval_secs(),
            reconcile_interval_secs: default_control_reconcile_interval_secs(),
        }
    }
}

/// This node's mutual-TLS identity, and the name it expects the control plane
/// to prove when it dials the machine channel.
///
/// Everything here is a no-op by itself: nothing in this crate yet dials with
/// it or accepts it in place of `[control] secret`. It exists so that an
/// operator can stage the three files and the reload loop ahead of the change
/// that switches the assignment client onto them, and so that a node running
/// with this section filled in is, from a log, visibly different from one
/// that is not.
///
/// # Why `trust_domain` and not just a name
///
/// `[control] endpoint` is an address - `client_config` needs the peer's
/// *name* as well, and that is a value this file has never had to state
/// before. Two ways to get it: an operator writes the control plane's full
/// SPIFFE identity out, or this node derives it from a trust domain plus the
/// fixed path every control plane answers to
/// (`deis_identity::names::control`). Derive-with-override is what is here:
/// `trust_domain` is the normal setting, and `control_identity` is an escape
/// hatch for whatever derivation cannot cover - a control plane addressed
/// under a name this fleet's own convention does not produce - that wins when
/// it is set.
///
/// This is the same choice deis-proxy has to make for the same reason, and
/// the two must not diverge: a node and a proxy pointed at the same control
/// plane have to derive the same name from the same trust domain, or one of
/// them refuses a handshake the other accepts and the disagreement is
/// diagnosed from two logs that each look correct on their own.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MachineIdentityConfig {
    /// This node's certificate chain, end-entity certificate first.
    #[serde(default)]
    pub certificate: Option<PathBuf>,

    /// The private key belonging to that certificate.
    #[serde(default)]
    pub private_key: Option<PathBuf>,

    /// What the control plane's certificate must chain to.
    #[serde(default)]
    pub trust_root: Option<PathBuf>,

    /// How often the three files above are re-read, in seconds.
    ///
    /// Same default as the control plane's own `[machine]
    /// identity_reload_seconds`, for the same reason: it wants to be far
    /// shorter than the shortest certificate lifetime in use, and there is no
    /// reason for one side of a rotation to notice it faster than the other.
    #[serde(default = "default_identity_reload_seconds")]
    pub identity_reload_seconds: u64,

    /// This fleet's trust domain, e.g. `deis.internal`, combined with the
    /// fixed control path to name the control plane this node expects to
    /// reach. See the struct doc for why this exists instead of just asking
    /// for the name outright.
    #[serde(default)]
    pub trust_domain: Option<String>,

    /// Where the control plane's machine channel is, as this node dials it -
    /// `https://host:port`.
    ///
    /// # Why the address lives with the identity and not with `[control]`
    ///
    /// A certificate and a destination are the two halves of one act: prove
    /// this, to that. Kept apart, each can be configured without the other,
    /// and both halves alone are useless in a way nothing notices until the
    /// first dial - an identity with nowhere to present it, or an address
    /// with nothing to prove. Kept in one table, "all of it or none of it" is
    /// a rule this table enforces about itself, and there is no second place
    /// that has to remember to cross-check.
    ///
    /// It is a different port from `[control] endpoint`, which is the human
    /// listener a browser reaches. This node announces itself here -
    /// registering over this channel is what gets it recorded by the name its
    /// certificate carries, and a node recorded by name is the only kind the
    /// control plane can dial back. The bundle download is served here too,
    /// which is why a bundle URL in an assignment names this address and not
    /// the human one.
    #[serde(default)]
    pub control_endpoint: Option<String>,

    /// Explicit override of the control plane's expected SPIFFE identity,
    /// e.g. `spiffe://deis.internal/deislet/control`. Wins over `trust_domain`
    /// when both are set.
    #[serde(default)]
    pub control_identity: Option<String>,

    /// Where this node serves the other half of the machine channel: its own
    /// management plane, under mutual TLS, for the control plane to drive it
    /// through. An `ip:port` literal, e.g. `0.0.0.0:8187`.
    ///
    /// # Why this is a second socket and not the one the node already has
    ///
    /// The node's other listener carries public application traffic, so a
    /// demand for a client certificate there would end every browser request
    /// during the handshake. The management plane has the opposite
    /// requirement - no caller without a certificate, ever - so the two
    /// cannot share a socket, and this one is separate for that reason rather
    /// than for tidiness.
    ///
    /// Unset - the default - means this node serves no management plane, and
    /// the control plane can drive it in no way at all: it can be deployed to
    /// only by the assignments this node pulls for itself. `main` refuses to
    /// start in that state rather than leave a node that looks reachable and
    /// is not.
    ///
    /// It must be an `ip:port` literal and not a hostname: which interface a
    /// management plane binds is not a question DNS should be answering at
    /// startup, and an address that resolved differently on two nodes would
    /// be an installation whose exposure nobody can read off the file.
    #[serde(default)]
    pub management_listen: Option<String>,
}

fn default_identity_reload_seconds() -> u64 {
    60
}

impl Default for MachineIdentityConfig {
    fn default() -> Self {
        Self {
            certificate: None,
            private_key: None,
            trust_root: None,
            identity_reload_seconds: default_identity_reload_seconds(),
            control_endpoint: None,
            trust_domain: None,
            control_identity: None,
            management_listen: None,
        }
    }
}

/// Bundle download and local storage.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct BundleConfig {
    /// Directory holding downloaded bundles. Unset means `<data_dir>/bundles`.
    #[serde(default)]
    pub dir: Option<PathBuf>,

    /// Largest bundle this node will accept. A download is abandoned the
    /// moment it goes over, so a wrong or hostile URL cannot fill the disk.
    ///
    /// Reads as `bundles.max_bytes`, the way the sibling setting reads as
    /// `assets.max_bytes`; the table already says which kind of bytes.
    #[serde(default = "default_max_bundle_size")]
    pub max_bytes: u64,

    /// Seconds one bundle download may take before it is abandoned and the
    /// currently running version is kept.
    #[serde(default = "default_download_timeout_secs")]
    pub download_timeout_secs: u64,
}

fn default_max_bundle_size() -> u64 {
    256 * 1024 * 1024
}

fn default_download_timeout_secs() -> u64 {
    300
}

impl Default for BundleConfig {
    fn default() -> Self {
        Self {
            dir: None,
            max_bytes: default_max_bundle_size(),
            download_timeout_secs: default_download_timeout_secs(),
        }
    }
}

/// Serving the static files a bundle carries.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AssetsConfig {
    /// `Cache-Control` sent with every asset.
    ///
    /// The default revalidates on every request and lets the `ETag` (the
    /// asset's sha256) turn that into a 304 with no body. Bundle paths are not
    /// content-addressed, so a long `max-age` here would pin a stale file in
    /// browser caches across a deploy.
    #[serde(default = "default_asset_cache_control")]
    pub cache_control: String,

    /// Ceiling on the assets of one version, which are held in memory so that
    /// serving one does not reopen the zip. A version above it is refused and
    /// whatever is already running stays up.
    #[serde(default = "default_max_asset_bytes")]
    pub max_bytes: u64,
}

fn default_asset_cache_control() -> String {
    "public, max-age=0, must-revalidate".to_string()
}

fn default_max_asset_bytes() -> u64 {
    128 * 1024 * 1024
}

impl Default for AssetsConfig {
    fn default() -> Self {
        Self {
            cache_control: default_asset_cache_control(),
            max_bytes: default_max_asset_bytes(),
        }
    }
}

/// Where an application's outbound `fetch()` may go.
///
/// The policy itself is deny-by-default and lives in
/// `crate::runtime::fetch_policy`, which explains what it refuses and why.
/// This is only the one dial an operator has: the list of destinations that
/// are opened up despite not being public.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct FetchConfig {
    /// Non-public destinations applications on this node may nevertheless
    /// reach, as `host` or `host:port` (an entry with no port covers every
    /// port on that host), e.g. `["internal-api.svc:8443", "10.4.0.7"]`.
    /// Once a hostname is listed with a port, requests to that hostname on
    /// other ports are refused even if it currently resolves to a public
    /// address. This keeps the restriction valid across the connector's
    /// second DNS lookup.
    ///
    /// This is the escape hatch for a deployment whose applications are meant
    /// to call an internal service. It cannot be widened into the platform's
    /// own services: deis-store, deis-queue, deis-control and deis-telemetry
    /// are refused after this list has been consulted, so an entry that
    /// happens to cover one of them still does not reach it.
    ///
    /// Empty, the default, means applications may reach public internet
    /// destinations and nothing else.
    #[serde(default)]
    pub allow: Vec<String>,

    /// Whether an HTTP proxy named in this node's environment
    /// (`HTTP_PROXY`, `HTTPS_PROXY`, `ALL_PROXY`) may carry applications'
    /// outbound calls.
    ///
    /// `false`, the default, takes those variables out of the process
    /// environment at startup. `deno_fetch` reads them unconditionally when it
    /// builds an application's HTTP client and offers no way to say no, so
    /// left in place they -- not this file -- decide where every `fetch()`
    /// actually connects. This node's own control-plane traffic still uses the
    /// proxy: the values are captured before they are removed and handed back
    /// to those clients explicitly.
    ///
    /// `true` puts it back for applications too. That is a real choice for a
    /// deployment whose applications are meant to egress through a proxy, and
    /// it costs something specific: a proxied connection dials the proxy and
    /// carries the destination as a `CONNECT` target, so the outbound policy's
    /// own DNS resolver -- the layer that keeps a name from resolving to one
    /// address for the check and another for the socket -- never sees the
    /// destination's addresses. The pre-flight check on the URL, and the
    /// refusal of this platform's own endpoints, are unaffected. The node logs
    /// the trade at warn on every start.
    ///
    /// See `crate::egress_proxy`.
    #[serde(default)]
    pub proxy_from_env: bool,
}

impl FetchConfig {
    fn validate(&self) -> Result<()> {
        // Parsed here rather than at first use so that a typo is an operator's
        // startup error and not a request that mysteriously still fails months
        // later. The parse is the same one the policy does.
        crate::runtime::FetchPolicy::new(&self.allow, Vec::<String>::new())
            .map_err(|e| anyhow!("fetch.allow is not a list of host or host:port entries: {e}"))?;
        Ok(())
    }
}

/// Default execution limits applied to each app's isolate.
///
/// These bound what one invocation of one app may do; they are not limits on
/// the node process itself. An app's `ResourceLimits` may override the request
/// timeout and the rate limit, in which case these are only the fallback; the
/// tail budget is the platform's alone.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ExecutionLimits {
    /// Maximum wall-clock time (ms) a single fetch invocation may run before
    /// the isolate is terminated and the request fails.
    #[serde(default = "default_timeout_ms")]
    pub timeout_ms: u64,

    /// Maximum wall-clock time (ms) the `waitUntil` tail of one invocation may
    /// run after the response has already been sent.
    ///
    /// A limit of its own rather than what is left of `timeout_ms`: the tail is
    /// meant to outlive the response -- that is the whole point of
    /// `waitUntil` -- so by the time it starts the request's budget is spent,
    /// and measuring it against that would refuse every tail. It still needs a
    /// ceiling, because the tail runs on the runtime thread that every other
    /// application on that thread is waiting for: a tail that never finishes
    /// holds the node behind a request that answered 200 and looks perfectly
    /// healthy.
    #[serde(default = "default_wait_until_timeout_ms")]
    pub wait_until_timeout_ms: u64,

    /// Maximum wall-clock time (ms) one streaming response body may take, from
    /// the moment its head reached the client to the moment its last byte does.
    ///
    /// A limit of its own for the same reason the tail has one, and a much
    /// larger one, because it bounds something different. `timeout_ms` is how
    /// long a caller waits for an *answer*; a stream has already answered. A
    /// server-sent-events feed, a progress channel or a large object being
    /// passed through is expected to run for minutes, and holding it to the
    /// request's ceiling would refuse the whole feature.
    ///
    /// It still needs a ceiling, because an open stream is a queue, a socket
    /// and a producer this node holds on the application's behalf, and a
    /// producer that never finishes would hold them for the life of the
    /// process. When it runs out the node closes the stream: the client's
    /// transfer fails part-way -- there is no status left to change, the 200
    /// went out with the head -- and the node logs the number it was held to.
    ///
    /// Enforced by the node rather than by terminating the isolate, so that one
    /// overrunning stream does not take the other streams and requests of the
    /// same application with it. The CPU half is `cpu_time_limit_ms`, which is
    /// what catches a producer that will not yield at all.
    #[serde(default = "default_stream_timeout_ms")]
    pub stream_timeout_ms: u64,

    /// Maximum rate (ops/sec) of JS-op dispatch before the isolate is
    /// terminated and the in-flight request fails.
    #[serde(default = "default_rate_limit_ops_per_sec")]
    pub rate_limit_ops_per_sec: f64,

    /// Maximum CPU time (ms) one invocation may consume on the thread that is
    /// running it, before the isolate is terminated.
    ///
    /// A separate ceiling from `timeout_ms`, and the one that measures what
    /// the node actually pays. A handler that waits 25 seconds on a slow
    /// upstream and a handler that spins for 25 seconds are the same thing to
    /// a wall clock, and differ by orders of magnitude to the machine: the
    /// first is an application doing IO and costs a parked thread, the second
    /// is one tenant holding a core that every other application on this node
    /// is queued behind. Only this limit can tell them apart, and on a box the
    /// operator owns it is the limit that matters.
    ///
    /// Enforced by the same watchdog thread as `timeout_ms`, which samples the
    /// watched thread's CPU clock; see `crate::runtime::isolate`. It applies
    /// per armed stretch of application code -- one request, one `waitUntil`
    /// tail, one queue batch, one cron tick, one WebSocket message -- exactly
    /// as the wall-clock budget does, and is re-armed with the same
    /// granularity.
    ///
    /// Setting it at or above `timeout_ms` disables it in practice: a single
    /// thread cannot burn more CPU time than wall-clock time, so the duration
    /// limit would always fire first. `ApplicationManager` says so per
    /// application at startup rather than leaving it to be discovered.
    #[serde(default = "default_cpu_time_limit_ms")]
    pub cpu_time_limit_ms: u64,

    /// Most outbound calls one invocation may make before the next one is
    /// refused.
    ///
    /// The limit that bounds *fan-out*, which none of the others do. A
    /// handler that answers one inbound request with two hundred outbound
    /// ones is spending this node's outbound capacity, its sockets and its
    /// upstreams' patience, and it does so while looking perfectly healthy on
    /// every other ceiling here: the calls are concurrent, so the wall clock
    /// sees one slow request, the CPU clock sees almost nothing, and the op
    /// rate limit sees a couple of hundred dispatches spread over a second.
    /// One tenant can take a node's egress that way without ever tripping
    /// anything, which is why Cloudflare bounds subrequests per invocation
    /// and why this exists beside the others rather than inside one of them.
    ///
    /// Counted per armed stretch of application code, exactly as the
    /// wall-clock and CPU budgets are: a request together with its `waitUntil`
    /// tail, one queue batch, one cron tick, one WebSocket message. Every
    /// destination that is actually dialled costs one, redirect hops
    /// included -- `js/bootstrap.js` follows redirects by hand, and each hop
    /// is a connection this node opens.
    ///
    /// An application may narrow it in its manifest (`limits.max_subrequests`)
    /// and may not widen it, like every other limit here.
    #[serde(default = "default_max_subrequests")]
    pub max_subrequests: u32,
}

fn default_timeout_ms() -> u64 {
    30_000
}

/// The tail gets the same allowance as the request by default: it is the same
/// application code doing the same kind of work, only after the answer went
/// out. An operator who wants background work held to less (or given more)
/// than a request sets this on its own.
fn default_wait_until_timeout_ms() -> u64 {
    30_000
}

/// Five minutes, against a 30-second request budget.
///
/// The two numbers are an order of magnitude apart because they bound
/// different things; see `stream_timeout_ms` above. Five minutes is chosen
/// against what the shapes that need streaming actually look like: a browser's
/// `EventSource` reconnects on its own, so an events feed cut at five minutes
/// resumes without the visitor seeing anything, and a large object passed
/// through at even a single megabyte a second is 300 MB inside the budget. An
/// operator running feeds that must stay up longer than that raises it; one
/// running a node of ordinary request-shaped applications can drop it to a few
/// seconds and find out immediately which of them stream at all.
fn default_stream_timeout_ms() -> u64 {
    300_000
}

fn default_rate_limit_ops_per_sec() -> f64 {
    1000.0
}

/// 5 seconds of CPU, against a 30-second wall clock.
///
/// The two numbers are deliberately far apart, because they bound different
/// things. A request-shaped handler spends single- or double-digit
/// milliseconds of CPU; even a heavy one -- resizing an image, hashing a few
/// megabytes -- stays under a second. 5s is therefore two orders of magnitude
/// above what an honest handler costs, so no legitimate application meets it,
/// while a handler that spins holds a core for a sixth of the time the
/// duration limit alone would have let it, and a handler that is merely
/// waiting on a slow upstream still gets its full 30 seconds.
///
/// An operator who runs deliberately CPU-heavy applications raises this; one
/// running a node full of small IO-bound handlers can drop it to a few hundred
/// milliseconds and get a much earlier warning about the tenant that is
/// burning the box.
fn default_cpu_time_limit_ms() -> u64 {
    5_000
}

/// Fifty outbound calls per invocation, which is Cloudflare's number for a
/// Worker on its free plan.
///
/// Chosen against what handlers actually do rather than against what a node
/// could survive: an API that talks to a database and two upstreams uses three
/// or four, a page that assembles a dozen fragments uses a dozen, and both are
/// an order of magnitude under this. What it refuses is the shape nothing else
/// on this node refuses -- one inbound request fanning out to hundreds -- and
/// an operator running deliberate fan-out raises it knowing that the number is
/// multiplied by every concurrent request the node is serving.
fn default_max_subrequests() -> u32 {
    50
}

impl Default for ExecutionLimits {
    fn default() -> Self {
        Self {
            timeout_ms: default_timeout_ms(),
            wait_until_timeout_ms: default_wait_until_timeout_ms(),
            stream_timeout_ms: default_stream_timeout_ms(),
            rate_limit_ops_per_sec: default_rate_limit_ops_per_sec(),
            cpu_time_limit_ms: default_cpu_time_limit_ms(),
            max_subrequests: default_max_subrequests(),
        }
    }
}

/// Log shipping to deis-telemetry.
///
/// With `enabled = false` (the default) this node's logs exist only in its own
/// stdout and in the in-memory buffer; nothing reaches deis-telemetry and
/// `deis tail` shows nothing for this node.
///
/// This section is what the metric and trace shippers are configured from as
/// well: all three go to the same service over the same connection, so the
/// address, the on/off switch and the credential are one decision. See
/// `crate::runtime::MetricShipperConfig`.
#[derive(Clone, Serialize, Deserialize, PartialEq)]
pub struct LoggingConfig {
    /// Whether to ship logs at all.
    #[serde(default)]
    pub enabled: bool,

    /// deis-telemetry `LogService` address, e.g. `http://127.0.0.1:8092`.
    /// Required when `enabled` is true.
    #[serde(default)]
    pub endpoint: Option<String>,

    /// The secret deis-telemetry's own `auth_secret` is set to, from which
    /// this node derives the capability token it presents on every log, trace
    /// and metric push.
    ///
    /// Unset means the node ships without a token. That is correct for the
    /// single-box install, where deis-telemetry has no `auth_secret` either
    /// and therefore both of its listeners are on loopback -- it refuses to
    /// start otherwise. Against any collector reachable from another host it
    /// is fatal to observability and to nothing else: every push is refused
    /// with `unauthenticated`, the node retries them for as long as it runs,
    /// and `deis logs` and `deis trace` are empty for every application on it.
    /// See [`crate::telemetry`].
    ///
    /// It is not refused at startup. A node that cannot ship telemetry has to
    /// keep serving requests, so the mismatch is a warning when the shippers
    /// start and a named error the first time a push is refused.
    #[serde(default)]
    pub auth_secret: Option<String>,

    /// How many entries the in-memory buffer holds before the oldest are
    /// dropped to make room.
    #[serde(default = "default_buffer_size")]
    pub buffer_size: usize,

    /// Seconds between flush attempts.
    #[serde(default = "default_flush_interval_secs")]
    pub flush_interval_secs: u64,

    /// Maximum entries sent in one `PushLogs` call.
    #[serde(default = "default_batch_size")]
    pub batch_size: usize,

    /// How many times one batch is retried before it is given up on.
    #[serde(default = "default_max_retries")]
    pub max_retries: u32,

    /// Ceiling on the retry queue: entries taken out of the buffer but not yet
    /// accepted by deis-telemetry. Once it is full the node stops draining the
    /// buffer, so a telemetry outage costs old logs rather than memory.
    #[serde(default = "default_max_pending_entries")]
    pub max_pending_entries: usize,
}

fn default_buffer_size() -> usize {
    10_000
}

fn default_flush_interval_secs() -> u64 {
    5
}

fn default_batch_size() -> usize {
    100
}

fn default_max_retries() -> u32 {
    3
}

fn default_max_pending_entries() -> usize {
    10_000
}

impl Default for LoggingConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            endpoint: None,
            auth_secret: None,
            buffer_size: default_buffer_size(),
            flush_interval_secs: default_flush_interval_secs(),
            batch_size: default_batch_size(),
            max_retries: default_max_retries(),
            max_pending_entries: default_max_pending_entries(),
        }
    }
}

impl RuntimeConfig {
    /// Read the TOML file (if any), apply environment overrides, and reject
    /// values the node cannot run on.
    ///
    /// Answers with the file it read as well as what it read, because the node
    /// says so in its startup log: "which file is this node actually running
    /// on" is the question an operator gets wrong most often and sees least,
    /// and `None` there means default ports, no control plane and no outbound
    /// allowlist.
    pub fn load(named: Option<&Path>) -> Result<(Self, Option<PathBuf>)> {
        reject_removed_env_vars()?;

        let source = Self::source(named)?;
        let mut config = match &source {
            Some(path) => Self::load_from(path)?,
            None => Self::default(),
        };

        config.apply_env_overrides()?;
        config.validate()?;
        Ok((config, source))
    }

    /// Which file this node reads its configuration from, if any.
    ///
    /// Three answers, in the order of how explicitly each was asked for: the
    /// path given on the command line, then `DEIS_RUNTIME_CONFIG`, then
    /// `./deis-runtime.toml` if it happens to be there.
    ///
    /// The first two were *named* by an operator, so a file that is not there
    /// stops the node rather than being quietly replaced by the defaults: a
    /// node that came up on default ports with no control plane, no allowlist
    /// and no application -- and a `/health` saying it was fine -- is the
    /// failure this refusal exists to prevent. The third is the convenience of
    /// running from the source tree and is allowed to be absent.
    pub fn source(named: Option<&Path>) -> Result<Option<PathBuf>> {
        if let Some(path) = named {
            if !path.exists() {
                return Err(anyhow!(
                    "the configuration file named with --config does not exist: {}",
                    path.display()
                ));
            }
            return Ok(Some(path.to_path_buf()));
        }

        if let Ok(named) = env::var("DEIS_RUNTIME_CONFIG") {
            let path = PathBuf::from(named);
            if !path.exists() {
                return Err(anyhow!(
                    "DEIS_RUNTIME_CONFIG specified file does not exist: {}",
                    path.display()
                ));
            }
            return Ok(Some(path));
        }

        let default_path = PathBuf::from("deis-runtime.toml");
        if default_path.exists() {
            Ok(Some(default_path))
        } else {
            Ok(None)
        }
    }

    /// How long in-flight requests get to finish once a shutdown signal
    /// arrives.
    pub fn shutdown_grace_period(&self) -> Duration {
        Duration::from_secs(self.shutdown_grace_period_secs)
    }

    fn load_from(path: &Path) -> Result<Self> {
        let content = std::fs::read_to_string(path)
            .with_context(|| format!("Failed to read config file: {}", path.display()))?;
        reject_removed_config_keys(&content, path)?;
        toml::from_str(&content)
            .with_context(|| format!("Failed to parse config file: {}", path.display()))
    }

    fn apply_env_overrides(&mut self) -> Result<()> {
        if let Some(value) = env_var("DEIS_RUNTIME_NODE_ID") {
            self.node_id = Some(value);
        }

        if let Some(value) = env_var("DEIS_RUNTIME_HTTP_PORT") {
            self.http_port = parse_env(&value, "DEIS_RUNTIME_HTTP_PORT", "a TCP port")?;
        }

        if let Some(value) = env_var("DEIS_RUNTIME_DATA_DIR") {
            self.data_dir = PathBuf::from(value);
        }

        if let Some(value) = env_var("DEIS_RUNTIME_SHUTDOWN_GRACE_PERIOD_SECS") {
            self.shutdown_grace_period_secs = parse_env(
                &value,
                "DEIS_RUNTIME_SHUTDOWN_GRACE_PERIOD_SECS",
                "a whole number of seconds",
            )?;
        }

        if let Some(value) = env_var("DEIS_RUNTIME_MAX_REQUEST_BODY_BYTES") {
            self.max_request_body_bytes = parse_env(
                &value,
                "DEIS_RUNTIME_MAX_REQUEST_BODY_BYTES",
                "a whole number of bytes",
            )?;
        }

        if let Some(value) = env_var("DEIS_RUNTIME_DEFAULT_MEMORY_LIMIT_MB") {
            self.default_memory_limit_mb = parse_env(
                &value,
                "DEIS_RUNTIME_DEFAULT_MEMORY_LIMIT_MB",
                "a whole number of megabytes",
            )?;
        }

        if let Some(value) = env_var("DEIS_RUNTIME_ADMISSION_WAIT_MS") {
            self.admission_wait_ms = parse_env(
                &value,
                "DEIS_RUNTIME_ADMISSION_WAIT_MS",
                "a whole number of milliseconds",
            )?;
        }

        if let Some(value) = env_var("DEIS_RUNTIME_OVERLOAD_RETRY_AFTER_SECONDS") {
            self.overload_retry_after_seconds = parse_env(
                &value,
                "DEIS_RUNTIME_OVERLOAD_RETRY_AFTER_SECONDS",
                "a whole number of seconds",
            )?;
        }

        if let Some(value) = env_var("DEIS_RUNTIME_DEFAULT_EXECUTION_LIMITS_TIMEOUT_MS") {
            self.default_execution_limits.timeout_ms = parse_env(
                &value,
                "DEIS_RUNTIME_DEFAULT_EXECUTION_LIMITS_TIMEOUT_MS",
                "a whole number of milliseconds",
            )?;
        }

        if let Some(value) = env_var("DEIS_RUNTIME_DEFAULT_EXECUTION_LIMITS_WAIT_UNTIL_TIMEOUT_MS")
        {
            self.default_execution_limits.wait_until_timeout_ms = parse_env(
                &value,
                "DEIS_RUNTIME_DEFAULT_EXECUTION_LIMITS_WAIT_UNTIL_TIMEOUT_MS",
                "a whole number of milliseconds",
            )?;
        }

        if let Some(value) = env_var("DEIS_RUNTIME_DEFAULT_EXECUTION_LIMITS_STREAM_TIMEOUT_MS") {
            self.default_execution_limits.stream_timeout_ms = parse_env(
                &value,
                "DEIS_RUNTIME_DEFAULT_EXECUTION_LIMITS_STREAM_TIMEOUT_MS",
                "a number of milliseconds",
            )?;
        }

        if let Some(value) = env_var("DEIS_RUNTIME_DEFAULT_EXECUTION_LIMITS_RATE_LIMIT_OPS_PER_SEC")
        {
            self.default_execution_limits.rate_limit_ops_per_sec = parse_env(
                &value,
                "DEIS_RUNTIME_DEFAULT_EXECUTION_LIMITS_RATE_LIMIT_OPS_PER_SEC",
                "a decimal ops-per-second rate",
            )?;
        }

        if let Some(value) = env_var("DEIS_RUNTIME_DEFAULT_EXECUTION_LIMITS_CPU_TIME_LIMIT_MS") {
            self.default_execution_limits.cpu_time_limit_ms = parse_env(
                &value,
                "DEIS_RUNTIME_DEFAULT_EXECUTION_LIMITS_CPU_TIME_LIMIT_MS",
                "a whole number of milliseconds",
            )?;
        }

        if let Some(value) = env_var("DEIS_RUNTIME_DEFAULT_EXECUTION_LIMITS_MAX_SUBREQUESTS") {
            self.default_execution_limits.max_subrequests = parse_env(
                &value,
                "DEIS_RUNTIME_DEFAULT_EXECUTION_LIMITS_MAX_SUBREQUESTS",
                "a whole number of outbound calls per invocation",
            )?;
        }

        if let Some(value) = env_var("DEIS_RUNTIME_FETCH_ALLOW") {
            // Comma-separated, because that is what an environment variable
            // can carry; the TOML form is a list.
            self.fetch.allow = value
                .split(',')
                .map(str::trim)
                .filter(|entry| !entry.is_empty())
                .map(str::to_string)
                .collect();
        }

        if let Some(value) = env_var("DEIS_RUNTIME_FETCH_PROXY_FROM_ENV") {
            self.fetch.proxy_from_env =
                parse_env(&value, "DEIS_RUNTIME_FETCH_PROXY_FROM_ENV", "true|false")?;
        }

        if let Some(value) = env_var("DEIS_RUNTIME_LOGGING_ENABLED") {
            self.logging.enabled = parse_env(&value, "DEIS_RUNTIME_LOGGING_ENABLED", "true|false")?;
        }

        if let Some(value) = env_var("DEIS_RUNTIME_LOGGING_ENDPOINT") {
            self.logging.endpoint = Some(value);
        }

        if let Some(value) = env_var("DEIS_RUNTIME_LOGGING_AUTH_SECRET") {
            self.logging.auth_secret = Some(value);
        }

        if let Some(value) = env_var("DEIS_RUNTIME_LOGGING_BUFFER_SIZE") {
            self.logging.buffer_size =
                parse_env(&value, "DEIS_RUNTIME_LOGGING_BUFFER_SIZE", "a count")?;
        }

        if let Some(value) = env_var("DEIS_RUNTIME_LOGGING_FLUSH_INTERVAL_SECS") {
            self.logging.flush_interval_secs = parse_env(
                &value,
                "DEIS_RUNTIME_LOGGING_FLUSH_INTERVAL_SECS",
                "a whole number of seconds",
            )?;
        }

        if let Some(value) = env_var("DEIS_RUNTIME_LOGGING_BATCH_SIZE") {
            self.logging.batch_size =
                parse_env(&value, "DEIS_RUNTIME_LOGGING_BATCH_SIZE", "a count")?;
        }

        if let Some(value) = env_var("DEIS_RUNTIME_LOGGING_MAX_RETRIES") {
            self.logging.max_retries =
                parse_env(&value, "DEIS_RUNTIME_LOGGING_MAX_RETRIES", "a count")?;
        }

        if let Some(value) = env_var("DEIS_RUNTIME_LOGGING_MAX_PENDING_ENTRIES") {
            self.logging.max_pending_entries = parse_env(
                &value,
                "DEIS_RUNTIME_LOGGING_MAX_PENDING_ENTRIES",
                "a count",
            )?;
        }

        if let Some(value) = env_var("DEIS_RUNTIME_NODE_ADVERTISED_ADDRESSES") {
            // Comma-separated, because that is what an environment variable
            // can carry; the TOML form is a list.
            self.node.advertised_addresses = value
                .split(',')
                .map(str::trim)
                .filter(|entry| !entry.is_empty())
                .map(|entry| {
                    entry.parse::<IpAddr>().map_err(|e| {
                        anyhow!(
                            "DEIS_RUNTIME_NODE_ADVERTISED_ADDRESSES contains {entry:?}, which \
                             is not an IP address: {e}"
                        )
                    })
                })
                .collect::<Result<Vec<_>>>()?;
        }

        if let Some(value) = env_var("DEIS_RUNTIME_CONTROL_ENDPOINT") {
            self.control.endpoint = Some(value);
        }

        if let Some(value) = env_var("DEIS_RUNTIME_MACHINE_IDENTITY_CONTROL_ENDPOINT") {
            self.machine_identity
                .get_or_insert_with(Default::default)
                .control_endpoint = Some(value);
        }

        if let Some(value) = env_var("DEIS_RUNTIME_CONTROL_REQUEST_TIMEOUT_SECS") {
            self.control.request_timeout_secs = parse_env(
                &value,
                "DEIS_RUNTIME_CONTROL_REQUEST_TIMEOUT_SECS",
                "a whole number of seconds",
            )?;
        }

        if let Some(value) = env_var("DEIS_RUNTIME_CONTROL_RETRY_INTERVAL_SECS") {
            self.control.retry_interval_secs = parse_env(
                &value,
                "DEIS_RUNTIME_CONTROL_RETRY_INTERVAL_SECS",
                "a whole number of seconds",
            )?;
        }

        if let Some(value) = env_var("DEIS_RUNTIME_CONTROL_RECONCILE_INTERVAL_SECS") {
            self.control.reconcile_interval_secs = parse_env(
                &value,
                "DEIS_RUNTIME_CONTROL_RECONCILE_INTERVAL_SECS",
                "a whole number of seconds",
            )?;
        }

        if let Some(value) = env_var("DEIS_RUNTIME_MACHINE_IDENTITY_CERTIFICATE") {
            self.machine_identity
                .get_or_insert_with(MachineIdentityConfig::default)
                .certificate = Some(PathBuf::from(value));
        }

        if let Some(value) = env_var("DEIS_RUNTIME_MACHINE_IDENTITY_PRIVATE_KEY") {
            self.machine_identity
                .get_or_insert_with(MachineIdentityConfig::default)
                .private_key = Some(PathBuf::from(value));
        }

        if let Some(value) = env_var("DEIS_RUNTIME_MACHINE_IDENTITY_TRUST_ROOT") {
            self.machine_identity
                .get_or_insert_with(MachineIdentityConfig::default)
                .trust_root = Some(PathBuf::from(value));
        }

        if let Some(value) = env_var("DEIS_RUNTIME_MACHINE_IDENTITY_RELOAD_SECONDS") {
            self.machine_identity
                .get_or_insert_with(MachineIdentityConfig::default)
                .identity_reload_seconds = parse_env(
                &value,
                "DEIS_RUNTIME_MACHINE_IDENTITY_RELOAD_SECONDS",
                "a whole number of seconds",
            )?;
        }

        if let Some(value) = env_var("DEIS_RUNTIME_MACHINE_IDENTITY_TRUST_DOMAIN") {
            self.machine_identity
                .get_or_insert_with(MachineIdentityConfig::default)
                .trust_domain = Some(value);
        }

        if let Some(value) = env_var("DEIS_RUNTIME_MACHINE_IDENTITY_CONTROL_IDENTITY") {
            self.machine_identity
                .get_or_insert_with(MachineIdentityConfig::default)
                .control_identity = Some(value);
        }

        if let Some(value) = env_var("DEIS_RUNTIME_MACHINE_IDENTITY_MANAGEMENT_LISTEN") {
            self.machine_identity
                .get_or_insert_with(MachineIdentityConfig::default)
                .management_listen = Some(value);
        }

        if let Some(value) = env_var("DEIS_RUNTIME_BUNDLES_DIR") {
            self.bundles.dir = Some(PathBuf::from(value));
        }

        if let Some(value) = env_var("DEIS_RUNTIME_BUNDLES_MAX_BYTES") {
            self.bundles.max_bytes = parse_env(
                &value,
                "DEIS_RUNTIME_BUNDLES_MAX_BYTES",
                "a whole number of bytes",
            )?;
        }

        if let Some(value) = env_var("DEIS_RUNTIME_BUNDLES_DOWNLOAD_TIMEOUT_SECS") {
            self.bundles.download_timeout_secs = parse_env(
                &value,
                "DEIS_RUNTIME_BUNDLES_DOWNLOAD_TIMEOUT_SECS",
                "a whole number of seconds",
            )?;
        }

        if let Some(value) = env_var("DEIS_RUNTIME_ASSETS_CACHE_CONTROL") {
            self.assets.cache_control = value;
        }

        if let Some(value) = env_var("DEIS_RUNTIME_ASSETS_MAX_BYTES") {
            self.assets.max_bytes = parse_env(
                &value,
                "DEIS_RUNTIME_ASSETS_MAX_BYTES",
                "a whole number of bytes",
            )?;
        }

        Ok(())
    }

    /// Directory holding downloaded bundles, resolved against `data_dir` when
    /// the operator did not name one.
    pub fn bundle_dir(&self) -> PathBuf {
        self.bundles
            .dir
            .clone()
            .unwrap_or_else(|| self.data_dir.join("bundles"))
    }

    /// The id this node answers to. Falls back to the hostname.
    pub fn resolved_node_id(&self) -> String {
        match self.node_id.as_deref() {
            Some(id) if !id.trim().is_empty() => id.trim().to_string(),
            _ => hostname::get()
                .unwrap_or_default()
                .to_string_lossy()
                .to_string(),
        }
    }

    /// The line to log when the admission budget can outlast the edge's
    /// patience, if it can.
    ///
    /// A request is admitted only while somebody is waiting to read the
    /// answer, and the caller on the edge is deis-proxy, which gives up on the
    /// upstream read after its own `upstream_read_timeout`. This crate does not
    /// depend on deis-proxy and cannot read what that instance was configured
    /// with, so what it compares against is the setting's default; an operator
    /// who raised the edge's timeout may raise this past it and be right to,
    /// and is told so rather than being stopped.
    ///
    /// `None` when the budget is inside the default.
    pub fn admission_wait_warning(&self) -> Option<String> {
        if self.admission_wait_ms < PROXY_DEFAULT_UPSTREAM_READ_TIMEOUT_MS {
            return None;
        }
        Some(format!(
            "admission_wait_ms is {}ms, which is not below deis-proxy's default \
             upstream_read_timeout ({}s): a request admitted this late may already have been \
             abandoned by the edge, so the thread would spend itself on an answer nobody is \
             reading. Lower admission_wait_ms under the edge's configured value, or raise the \
             edge's upstream_read_timeout_secs above this one",
            self.admission_wait_ms,
            PROXY_DEFAULT_UPSTREAM_READ_TIMEOUT_MS / 1000
        ))
    }

    /// Reject configurations the node cannot serve traffic on.
    ///
    /// Every rule here guards a value that would otherwise fail every request
    /// on the node with a message that points at the symptom rather than at
    /// the config: a zero timeout terminates each isolate immediately, and a
    /// zero rate limit gives the token bucket no capacity, so the very first
    /// op dispatch kills the request. Failing at startup keeps that class of
    /// mistake in the operator's terminal instead of in the traffic.
    pub fn validate(&self) -> Result<()> {
        if self.http_port == 0 {
            return Err(anyhow!(
                "http_port must be a real port (1-65535); 0 asks the OS for an arbitrary port, \
                 which the control plane cannot address"
            ));
        }

        if self.shutdown_grace_period_secs == 0 {
            return Err(anyhow!(
                "shutdown_grace_period_secs must be greater than 0; 0 would kill in-flight \
                 requests the instant a signal arrives"
            ));
        }

        if self.max_request_body_bytes == 0 {
            return Err(anyhow!(
                "max_request_body_bytes must be greater than 0; 0 answers 413 to every request \
                 that carries a body, so no form post, upload or JSON document would reach an \
                 application on this node"
            ));
        }

        if self.default_memory_limit_mb < MIN_MEMORY_LIMIT_MB {
            return Err(anyhow!(
                "default_memory_limit_mb is {}, which is below the {}MB floor; a heap that small \
                 is exhausted by the runtime's own bootstrap, so every isolate on this node would \
                 be terminated before it ever ran application code",
                self.default_memory_limit_mb,
                MIN_MEMORY_LIMIT_MB
            ));
        }

        if self.admission_wait_ms == 0 {
            return Err(anyhow!(
                "admission_wait_ms must be greater than 0; 0 refuses every request that had to \
                 wait at all, and a request that finds the application's runtime thread busy has \
                 always waited, so the node would answer 503 to everything but the first request \
                 in each quiet moment"
            ));
        }

        if self.overload_retry_after_seconds < 1 {
            return Err(anyhow!(
                "overload_retry_after_seconds must be at least 1; 0 would tell a client to \
                 retry at once, which is the one thing a refusal at capacity is not"
            ));
        }

        // Said and not enforced: the value being compared against is the edge's
        // default, and an operator whose edge waits longer than that has a
        // legitimate reason to go past it.
        if let Some(warning) = self.admission_wait_warning() {
            tracing::warn!("{warning}");
        }

        let limits = &self.default_execution_limits;
        if limits.timeout_ms < MIN_TIMEOUT_MS {
            return Err(anyhow!(
                "default_execution_limits.timeout_ms is {}, which is below the {}ms floor; every \
                 request on this node would be terminated before its handler could finish",
                limits.timeout_ms,
                MIN_TIMEOUT_MS
            ));
        }

        if limits.wait_until_timeout_ms < MIN_TIMEOUT_MS {
            return Err(anyhow!(
                "default_execution_limits.wait_until_timeout_ms is {}, which is below the {}ms \
                 floor; the waitUntil tail of every invocation on this node would be terminated \
                 before the work it was handed could finish",
                limits.wait_until_timeout_ms,
                MIN_TIMEOUT_MS
            ));
        }

        if limits.stream_timeout_ms < MIN_TIMEOUT_MS {
            return Err(anyhow!(
                "default_execution_limits.stream_timeout_ms is {}, which is below the {}ms floor; \
                 every streaming response on this node would be cut off before its producer had \
                 written a second chunk, and the client would see a transfer that fails part-way",
                limits.stream_timeout_ms,
                MIN_TIMEOUT_MS
            ));
        }

        if limits.cpu_time_limit_ms < MIN_CPU_TIME_LIMIT_MS {
            return Err(anyhow!(
                "default_execution_limits.cpu_time_limit_ms is {}, which is below the {}ms floor; \
                 the CPU clock this is measured against advances in scheduler ticks, so a ceiling \
                 that small is enforced with an error of tens of percent and would cut off \
                 handlers that are doing nothing unusual",
                limits.cpu_time_limit_ms,
                MIN_CPU_TIME_LIMIT_MS
            ));
        }

        if limits.max_subrequests == 0 {
            return Err(anyhow!(
                "default_execution_limits.max_subrequests is 0; no application on this node \
                 could make a single outbound call, so every fetch() would be refused. Leave it \
                 out for the default of {}",
                default_max_subrequests()
            ));
        }

        if !limits.rate_limit_ops_per_sec.is_finite() || limits.rate_limit_ops_per_sec <= 0.0 {
            return Err(anyhow!(
                "default_execution_limits.rate_limit_ops_per_sec is {}, expected a finite value \
                 greater than 0; a non-positive rate gives the token bucket no capacity, so the \
                 first op every request dispatches terminates the isolate",
                limits.rate_limit_ops_per_sec
            ));
        }

        self.logging.validate()?;
        self.control.validate()?;

        // A node that is attached to a control plane must be able to reach it,
        // and there is one way left: the machine channel. Registration, the
        // assignment list and the bundle download all moved there, and a
        // control plane of this version does not serve their HTTP forms on the
        // listener `control.endpoint` names - that listener answers 404 to
        // them, the same as to any path it does not have.
        //
        // So a node configured the old way does not fail, which is the problem.
        // It starts, binds its data port, asks for its assignments, is told the
        // route does not exist, and retries for the life of the process - never
        // loading a version, never announcing itself, and never saying anything
        // that names the cause. Refusing here turns a fleet member that is
        // permanently and quietly useless into a startup error an operator
        // reads once.
        //
        // A node with no `control.endpoint` at all is a different thing and is
        // left alone: it asks nobody what to run and serves what is already in
        // its local store, which is a supported way to run one.
        if self.control.endpoint.is_some() && self.machine_identity.is_none() {
            return Err(anyhow!(
                "control.endpoint is set but [machine_identity] is not. Everything this node \
                 asks its control plane for - registering itself, fetching its assignment list, \
                 downloading a bundle - is served on the control plane's machine channel now, \
                 over mutual TLS, and a control plane of this version does not serve the HTTP \
                 forms of them at all. Without a certificate this node would retry a route that \
                 answers 404 for as long as it ran, loading nothing and announcing nothing. Give \
                 it a [machine_identity] table naming its certificate, private key, trust root \
                 and the control plane's machine endpoint, or remove control.endpoint and let it \
                 serve only what is already in its local store"
            ));
        }

        if let Some(identity) = &self.machine_identity {
            identity.validate()?;

            // Three listeners, three sockets. The data listener binds
            // `0.0.0.0:http_port`, which covers every address on this host,
            // so a machine listener sharing that port could not bind at all
            // and would take the node's startup down with it. Sharing a port
            // with the plaintext management plane is the worse failure of the
            // two: the two doors authenticate their callers differently, and
            // which of them a control plane met would be decided by which
            // bound first. Refused here rather than discovered from an
            // "address already in use" that names a port and neither setting.
            if let Some(machine) = identity.management_listen_addr()? {
                if machine.port() == self.http_port {
                    return Err(anyhow!(
                        "machine_identity.management_listen {machine} names the same port as \
                         http_port ({}), which the data listener binds on every address of this \
                         host. Give the mutually authenticated management plane a port of its \
                         own",
                        self.http_port
                    ));
                }
            }
        }

        self.bundles.validate()?;
        self.assets.validate()?;
        self.fetch.validate()?;

        Ok(())
    }
}

impl ControlPlaneConfig {
    fn validate(&self) -> Result<()> {
        if let Some(endpoint) = self.endpoint.as_deref() {
            if endpoint.trim().is_empty() {
                return Err(anyhow!(
                    "control.endpoint is empty; either remove it or set it to the control plane \
                     base URL, e.g. http://127.0.0.1:8080. An empty value would make this node \
                     start with no idea what it should be running"
                ));
            }
            if !(endpoint.starts_with("http://") || endpoint.starts_with("https://")) {
                return Err(anyhow!(
                    "control.endpoint {endpoint:?} has no http:// or https:// scheme; the node \
                     builds an absolute URL from it and would never reach the control plane"
                ));
            }
        }

        if self.request_timeout_secs == 0 {
            return Err(anyhow!(
                "control.request_timeout_secs must be greater than 0; 0 would abandon the \
                 assignment request before it could be sent, so the node would never learn what \
                 to run"
            ));
        }

        if self.retry_interval_secs == 0 {
            return Err(anyhow!(
                "control.retry_interval_secs must be greater than 0; 0 spins the retry loop on a \
                 hot loop against the control plane"
            ));
        }

        if self.reconcile_interval_secs == 0 {
            return Err(anyhow!(
                "control.reconcile_interval_secs must be greater than 0; 0 would re-ask for the \
                 assignment list without pausing, which is a hot loop against the control plane \
                 and re-installs every deployment as fast as it can"
            ));
        }

        Ok(())
    }
}

impl MachineIdentityConfig {
    /// The three files, ready to hand to [`deis_identity::MachineIdentity::load`].
    ///
    /// Panics if `certificate`, `private_key` or `trust_root` is unset.
    /// `RuntimeConfig::validate` refuses a section with any of the three
    /// missing before this node does anything else, so by the time startup
    /// reaches this call that has already been checked; this is the promise
    /// kept, not a panic waiting to happen.
    pub fn identity_files(&self) -> deis_identity::IdentityFiles {
        deis_identity::IdentityFiles::new(
            self.certificate
                .clone()
                .expect("machine_identity.certificate is required and validated at startup"),
            self.private_key
                .clone()
                .expect("machine_identity.private_key is required and validated at startup"),
            self.trust_root
                .clone()
                .expect("machine_identity.trust_root is required and validated at startup"),
        )
    }

    /// The SPIFFE name this node expects the control plane to prove when it
    /// dials the machine channel.
    ///
    /// `control_identity` wins when it is set; otherwise the name is derived
    /// from `trust_domain` by [`deis_identity::names::control`]. See the
    /// struct doc for why this is derive-with-override rather than one field
    /// or the other.
    pub fn expected_control_identity(&self) -> Result<deis_identity::SpiffeId> {
        if let Some(explicit) = self.control_identity.as_deref() {
            return deis_identity::SpiffeId::parse(explicit).map_err(|e| {
                anyhow!(
                    "machine_identity.control_identity {explicit:?} is not a valid SPIFFE \
                     identity: {e}"
                )
            });
        }

        let trust_domain = self.trust_domain.as_deref().ok_or_else(|| {
            anyhow!(
                "machine_identity has neither trust_domain nor control_identity set, so this \
                 node has no way to know which name the control plane must prove; set \
                 trust_domain to this fleet's trust domain, or control_identity to the control \
                 plane's full SPIFFE identity"
            )
        })?;

        deis_identity::names::control(trust_domain).map_err(|e| {
            anyhow!(
                "machine_identity.trust_domain {trust_domain:?} cannot name the control plane: {e}"
            )
        })
    }

    /// Where this node serves its own machine channel, parsed.
    ///
    /// `Ok(None)` - the default - means it serves none: this node dials the
    /// control plane by certificate and can be driven by nobody, because the
    /// plaintext management plane this used to fall back to no longer exists.
    pub fn management_listen_addr(&self) -> Result<Option<std::net::SocketAddr>> {
        let Some(listen) = self
            .management_listen
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        else {
            return Ok(None);
        };
        listen
            .parse::<std::net::SocketAddr>()
            .map(Some)
            .map_err(|e| {
                anyhow!(
                    "machine_identity.management_listen {listen:?} is not an ip:port address \
                     ({e}); write it as 0.0.0.0:8187 or 127.0.0.1:8187. A hostname is refused \
                     because which interface this node offers its management plane on is not a \
                     question DNS should be answering at startup"
                )
            })
    }

    /// Reject a section that cannot be loaded or that names no one.
    ///
    /// Every rule here is the same shape: a value that would otherwise turn
    /// into a startup failure three layers down - a missing file
    /// `MachineIdentity::load` reports against a path with no context, a
    /// handshake refused because the peer name was never resolved - is
    /// refused here instead, against the setting that is actually wrong.
    fn validate(&self) -> Result<()> {
        // The address and the files are one setting, with one exception. A
        // certificate this node never presents anywhere, and an address with
        // nothing to prove when it gets there, are both useless in a way
        // nothing notices until the first dial - so they are refused together,
        // here, because this table is the only thing that has to see both.
        //
        // The exception is a node that serves a management plane and dials
        // nobody. Its certificate is not useless: it is what that listener
        // presents to whoever drives it. That is what `deis dev` runs - a node
        // driven entirely by the CLI beside it, with no control plane anywhere
        // to fetch assignments from - and refusing it would be refusing a
        // configuration that works.
        if self.control_endpoint.is_none() && self.management_listen.is_none() {
            return Err(anyhow!(
                "machine_identity has neither control_endpoint nor management_listen, so this \
                 node would load its certificate, log that it had, and then neither ask anything \
                 of a control plane nor let one drive it. Set control_endpoint to the control \
                 plane's machine listener - a different port from [control] endpoint, which is \
                 the human one - or management_listen if this node is driven locally and fetches \
                 nothing"
            ));
        }
        if let Some(endpoint) = self.control_endpoint.as_deref() {
            if endpoint.trim().is_empty() {
                return Err(anyhow!(
                    "machine_identity.control_endpoint is empty; either remove it - along with the \
                     [machine_identity] section, which is useless without it - or set it to the \
                     control plane's machine listener, e.g. https://127.0.0.1:8443"
                ));
            }
            if !endpoint.starts_with("https://") {
                return Err(anyhow!(
                    "machine_identity.control_endpoint {endpoint:?} is not an https:// URL; the machine \
                     channel is mutual TLS on every route it carries, and an http:// spelling of \
                     it would also stop this node recognising the bundle URLs the control plane \
                     hands out as belonging to that listener, so those downloads would go out \
                     over plain HTTP and be refused"
                ));
            }
            if !names_an_explicit_port(endpoint) {
                return Err(anyhow!(
                    "machine_identity.control_endpoint {endpoint:?} names no port. This channel has no \
                     default port to fall back on: it is not the web, and guessing 443 would \
                     dial whatever else happens to be listening there. Add the port the control \
                     plane's [machine] listen is bound to"
                ));
            }
        }

        let missing: Vec<&str> = [
            ("certificate", self.certificate.is_none()),
            ("private_key", self.private_key.is_none()),
            ("trust_root", self.trust_root.is_none()),
        ]
        .into_iter()
        .filter_map(|(name, absent)| absent.then_some(name))
        .collect();

        if !missing.is_empty() {
            return Err(anyhow!(
                "machine_identity is missing {}; certificate, private_key and trust_root are \
                 required together, because a section that loads two of the three files and \
                 fails on the connection using the third is a startup that looked like it \
                 succeeded. Either set all three, or remove the section entirely to keep this \
                 node on [control] secret",
                missing.join(", ")
            ));
        }

        if self.identity_reload_seconds == 0 {
            return Err(anyhow!(
                "machine_identity.identity_reload_seconds must be greater than 0; 0 re-reads the \
                 three files in a hot loop instead of on the interval an operator meant to set"
            ));
        }

        // An address that was meant to be filled in and was not. Left as an
        // empty string it would read, to `management_listen_addr`, exactly
        // like the section that never asked to serve at all - so the node
        // would come up dialling happily and serving no management plane, and
        // the setting an operator wrote would be the thing that said
        // otherwise.
        if let Some(listen) = self.management_listen.as_deref() {
            if listen.trim().is_empty() {
                return Err(anyhow!(
                    "machine_identity.management_listen is empty; either remove it, which leaves \
                     this node with no management plane at all and so nothing that can deploy to \
                     it, unload from it or fire its crons - or set it to the ip:port this node \
                     should serve that plane on, e.g. 0.0.0.0:8187"
                ));
            }
        }

        // Also refuses a hostname, and it is parsed here as well as where it
        // is used so a typo is a startup error naming the setting rather than
        // a node that quietly never bound the listener it was told to.
        if let Some(addr) = self.management_listen_addr()? {
            if addr.port() == 0 {
                return Err(anyhow!(
                    "machine_identity.management_listen {addr} asks the OS for an arbitrary port; \
                     the control plane reaches this node at an address it was told about, so a \
                     port that changes on every restart is a management plane nothing can find"
                ));
            }
        }

        // Also confirms trust_domain/control_identity resolve to something
        // this node could actually present as an expected peer, so a typo'd
        // trust domain is refused here and not in the first handshake this
        // node's client makes.
        self.expected_control_identity()?;

        Ok(())
    }
}

impl BundleConfig {
    fn validate(&self) -> Result<()> {
        if self.max_bytes == 0 {
            return Err(anyhow!(
                "bundles.max_bytes must be greater than 0; 0 rejects every bundle, so no \
                 deployment could ever be applied on this node"
            ));
        }

        if self.download_timeout_secs == 0 {
            return Err(anyhow!(
                "bundles.download_timeout_secs must be greater than 0; 0 abandons every download \
                 before a byte arrives"
            ));
        }

        Ok(())
    }
}

impl AssetsConfig {
    fn validate(&self) -> Result<()> {
        if self.max_bytes == 0 {
            return Err(anyhow!(
                "assets.max_bytes must be greater than 0; 0 refuses every version that carries a \
                 single static file, which is most of them"
            ));
        }

        if self.cache_control.trim().is_empty() {
            return Err(anyhow!(
                "assets.cache_control is empty; set a directive, e.g. \
                 \"public, max-age=0, must-revalidate\". An empty header value would be sent \
                 verbatim and caches would be free to invent their own freshness"
            ));
        }

        Ok(())
    }
}

/// Hand-written so dumping the configuration - in a startup line, in an error
/// context - cannot print the telemetry secret.
impl std::fmt::Debug for LoggingConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LoggingConfig")
            .field("enabled", &self.enabled)
            .field("endpoint", &self.endpoint)
            .field(
                "auth_secret",
                &self.auth_secret.as_ref().map(|_| "<redacted>"),
            )
            .field("buffer_size", &self.buffer_size)
            .field("flush_interval_secs", &self.flush_interval_secs)
            .field("batch_size", &self.batch_size)
            .field("max_retries", &self.max_retries)
            .field("max_pending_entries", &self.max_pending_entries)
            .finish()
    }
}

impl LoggingConfig {
    fn validate(&self) -> Result<()> {
        if self.buffer_size == 0 {
            return Err(anyhow!(
                "logging.buffer_size must be greater than 0; a zero-length buffer drops every \
                 entry the moment it is recorded"
            ));
        }

        if !self.enabled {
            // The remaining values only drive the shipper, which is not
            // running. Leave them alone rather than rejecting a node over a
            // setting that has no effect.
            return Ok(());
        }

        let Some(endpoint) = self.endpoint.as_deref() else {
            return Err(anyhow!(
                "logging.enabled is true but logging.endpoint is unset; nothing would reach \
                 deis-telemetry and the logs would only exist in this node's own output"
            ));
        };

        if endpoint.trim().is_empty() {
            return Err(anyhow!(
                "logging.endpoint is empty; set it to the deis-telemetry LogService address, \
                 e.g. http://127.0.0.1:8092"
            ));
        }

        if self.flush_interval_secs == 0 {
            return Err(anyhow!(
                "logging.flush_interval_secs must be greater than 0; a zero interval spins the \
                 shipper on a hot loop"
            ));
        }

        if self.batch_size == 0 {
            return Err(anyhow!(
                "logging.batch_size must be greater than 0; a zero batch ships nothing, so the \
                 buffer would grow until it starts dropping entries"
            ));
        }

        if self.max_retries == 0 {
            return Err(anyhow!(
                "logging.max_retries must be at least 1; 0 means no batch is ever attempted"
            ));
        }

        if self.max_pending_entries < self.batch_size {
            return Err(anyhow!(
                "logging.max_pending_entries ({}) is smaller than logging.batch_size ({}); the \
                 retry queue could not hold a single batch",
                self.max_pending_entries,
                self.batch_size
            ));
        }

        Ok(())
    }
}

/// Read an environment variable, treating an empty value as unset.
///
/// `FOO=` in a shell script or unit file usually means "I did not set this",
/// and letting it through would fail parsing for a value nobody chose.
fn env_var(name: &str) -> Option<String> {
    match env::var(name) {
        Ok(value) if !value.trim().is_empty() => Some(value),
        _ => None,
    }
}

fn parse_env<T>(value: &str, name: &str, expected: &str) -> Result<T>
where
    T: std::str::FromStr,
    T::Err: std::fmt::Display,
{
    value
        .parse::<T>()
        .map_err(|e| anyhow!("{name} is not {expected}: {value:?} ({e})"))
}

/// Refuse a configuration file that still sets a key this node no longer
/// reads.
///
/// The check is on the raw text rather than on the deserialised struct,
/// because serde simply drops an unknown key: the node would come up on a
/// default while the operator's file said otherwise, and nothing anywhere
/// would say so.
fn reject_removed_config_keys(content: &str, path: &Path) -> Result<()> {
    let parsed: toml::Value = toml::from_str(content)
        .with_context(|| format!("Failed to parse config file: {}", path.display()))?;

    for (removed, replacement) in REMOVED_CONFIG_KEYS {
        let mut node = Some(&parsed);
        for segment in removed.split('.') {
            node = node.and_then(|value| value.get(segment));
        }
        if node.is_some() {
            return Err(anyhow!(
                "{} sets {removed}, which this node no longer reads. Rename it to {replacement}; \
                 left as it is, the setting has no effect and the node runs on the default",
                path.display()
            ));
        }
    }

    Ok(())
}

fn reject_removed_env_vars() -> Result<()> {
    for (removed, replacement) in REMOVED_ENV_VARS {
        if env::var(removed).is_ok() {
            return Err(anyhow!(
                "{removed} is no longer read; it was renamed to {replacement} so that the \
                 environment variable and the TOML key spell the same setting. Refusing to start \
                 rather than run on defaults while the override looks applied."
            ));
        }
    }
    Ok(())
}

/// Whether `endpoint` spells a port out.
///
/// The machine channel's dialler refuses a URI without one - it has no default
/// port to fall back on, and guessing 443 would dial whatever else is
/// listening there - so an address missing it fails on every connection
/// attempt rather than at startup. This is what lets that be said once, in an
/// error naming the setting.
///
/// Only the authority is looked at, and an IPv6 literal's own colons do not
/// count: only digits after the closing bracket are a port.
fn names_an_explicit_port(endpoint: &str) -> bool {
    let Some((_, rest)) = endpoint.split_once("://") else {
        return false;
    };
    let Some(authority) = rest.split(['/', '?', '#']).next().filter(|a| !a.is_empty()) else {
        return false;
    };
    let authority = authority
        .rsplit_once('@')
        .map_or(authority, |(_, host)| host);
    match authority.rfind(':') {
        Some(index) => {
            let port = &authority[index + 1..];
            !port.is_empty() && port.chars().all(|c| c.is_ascii_digit())
        }
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_execution_limits_defaults() {
        let limits = ExecutionLimits::default();
        assert_eq!(limits.timeout_ms, 30_000);
        assert_eq!(limits.wait_until_timeout_ms, 30_000);
        assert_eq!(limits.rate_limit_ops_per_sec, 1000.0);
        assert_eq!(limits.cpu_time_limit_ms, 5_000);
    }

    #[test]
    fn a_config_that_names_no_cpu_ceiling_gets_the_default() {
        // Every configuration file written before this setting existed omits
        // it, and those are the nodes the ceiling has to reach: a default that
        // only a test can produce is the shape the heap ceiling had for
        // months, where the mechanism was correct and nothing ever fed it a
        // value.
        let config: RuntimeConfig =
            toml::from_str("[default_execution_limits]\ntimeout_ms = 5000\n").unwrap();
        assert_eq!(config.default_execution_limits.cpu_time_limit_ms, 5_000);
        config.validate().unwrap();
    }

    #[test]
    fn the_cpu_ceiling_is_configured_on_its_own() {
        let toml_str = r#"
            [default_execution_limits]
            timeout_ms = 30000
            cpu_time_limit_ms = 250
        "#;
        let config: RuntimeConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.default_execution_limits.timeout_ms, 30_000);
        assert_eq!(config.default_execution_limits.cpu_time_limit_ms, 250);
        config.validate().unwrap();
    }

    #[test]
    fn test_validate_rejects_cpu_ceiling_below_floor() {
        let mut config = RuntimeConfig::default();
        config.default_execution_limits.cpu_time_limit_ms = MIN_CPU_TIME_LIMIT_MS - 1;
        let err = config.validate().unwrap_err().to_string();
        assert!(err.contains("cpu_time_limit_ms"), "{err}");
    }

    #[test]
    fn the_tail_budget_is_configured_on_its_own() {
        // The tail is allowed to outlive the response, so an operator has to be
        // able to give background work a different allowance from a request
        // without changing what a request gets.
        let toml_str = r#"
            [default_execution_limits]
            timeout_ms = 5000
            wait_until_timeout_ms = 120000
        "#;
        let config: RuntimeConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.default_execution_limits.timeout_ms, 5_000);
        assert_eq!(
            config.default_execution_limits.wait_until_timeout_ms,
            120_000
        );
        config.validate().unwrap();
    }

    #[test]
    fn a_config_that_names_no_tail_budget_gets_the_default() {
        // Every configuration written before this setting existed omits it, and
        // an unbounded tail is what this closes; the default has to apply.
        let config: RuntimeConfig =
            toml::from_str("[default_execution_limits]\ntimeout_ms = 5000\n").unwrap();
        assert_eq!(
            config.default_execution_limits.wait_until_timeout_ms,
            30_000
        );
    }

    #[test]
    fn test_validate_rejects_tail_budget_below_floor() {
        let mut config = RuntimeConfig::default();
        config.default_execution_limits.wait_until_timeout_ms = MIN_TIMEOUT_MS - 1;
        let err = config.validate().unwrap_err().to_string();
        assert!(err.contains("wait_until_timeout_ms"), "{err}");
    }

    #[test]
    fn test_runtime_config_default_includes_limits() {
        let config = RuntimeConfig::default();
        assert_eq!(config.default_execution_limits, ExecutionLimits::default());
        assert_eq!(config.logging, LoggingConfig::default());
    }

    #[test]
    fn test_parse_toml_with_limits_override() {
        let toml_str = r#"
            http_port = 9000

            [default_execution_limits]
            timeout_ms = 5000
            rate_limit_ops_per_sec = 250.0
        "#;
        let config: RuntimeConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.http_port, 9000);
        assert_eq!(config.default_execution_limits.timeout_ms, 5000);
        assert_eq!(
            config.default_execution_limits.rate_limit_ops_per_sec,
            250.0
        );
    }

    #[test]
    fn the_fetch_allowlist_is_read_and_defaults_to_empty() {
        let config: RuntimeConfig = toml::from_str("http_port = 9000").unwrap();
        assert!(
            config.fetch.allow.is_empty(),
            "a node that says nothing about fetch must let applications reach public \
             destinations and nothing else"
        );

        let toml_str = r#"
            [fetch]
            allow = ["internal-api.svc:8443", "10.4.0.7"]
        "#;
        let config: RuntimeConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.fetch.allow, ["internal-api.svc:8443", "10.4.0.7"]);
        config
            .validate()
            .expect("both of those are shapes the policy accepts");
    }

    #[test]
    fn advertised_addresses_are_read_from_the_node_table_and_default_to_empty() {
        // The node's own data port is refused under every interface address
        // by enumeration; this section is for the addresses only the
        // operator knows -- the public IP a cloud 1:1 NAT maps onto this
        // node. Absent section, no addresses.
        let config: RuntimeConfig = toml::from_str("http_port = 9000").unwrap();
        assert!(config.node.advertised_addresses.is_empty());

        let toml_str = r#"
            [node]
            advertised_addresses = ["203.0.113.7", "2001:db8::10"]
        "#;
        let config: RuntimeConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(
            config.node.advertised_addresses,
            ["203.0.113.7", "2001:db8::10"].map(|a| a.parse::<IpAddr>().unwrap())
        );
        config
            .validate()
            .expect("a list of IP addresses is a valid configuration");
    }

    #[test]
    fn an_advertised_address_that_is_not_an_ip_address_fails_the_parse() {
        // serde refuses it here rather than the policy discovering it per
        // request, which is the same rule every other setting in this file
        // follows: a typo is an operator's startup error.
        assert!(toml::from_str::<RuntimeConfig>(
            "[node]\nadvertised_addresses = [\"example.com\"]\n"
        )
        .is_err());
    }

    #[test]
    fn advertised_addresses_can_be_set_from_the_environment() {
        let _guard = ENV_TEST_LOCK.lock().unwrap();

        env::set_var(
            "DEIS_RUNTIME_NODE_ADVERTISED_ADDRESSES",
            "203.0.113.7, 2001:db8::10",
        );
        let mut config = RuntimeConfig::default();
        let result = config.apply_env_overrides();
        env::remove_var("DEIS_RUNTIME_NODE_ADVERTISED_ADDRESSES");

        result.expect("a comma-separated list of IP addresses applies");
        assert_eq!(
            config.node.advertised_addresses,
            ["203.0.113.7", "2001:db8::10"].map(|a| a.parse::<IpAddr>().unwrap())
        );
    }

    #[test]
    fn an_advertised_address_in_the_environment_that_is_not_an_ip_fails() {
        let _guard = ENV_TEST_LOCK.lock().unwrap();

        env::set_var("DEIS_RUNTIME_NODE_ADVERTISED_ADDRESSES", "not-an-ip");
        let mut config = RuntimeConfig::default();
        let result = config.apply_env_overrides();
        env::remove_var("DEIS_RUNTIME_NODE_ADVERTISED_ADDRESSES");

        let err = result.expect_err("a name is not an address the fetch policy can match");
        assert!(err.to_string().contains("not-an-ip"), "{err}");
    }

    #[test]
    fn a_fetch_allowlist_entry_that_is_not_a_host_fails_startup() {
        // Silently ignoring it would leave the operator believing an internal
        // service was reachable; they would only find out from an application
        // failing in production.
        let toml_str = r#"
            [fetch]
            allow = ["not a host at all"]
        "#;
        let config: RuntimeConfig = toml::from_str(toml_str).unwrap();
        let error = config.validate().expect_err("that entry is not a host");
        assert!(error.to_string().contains("fetch.allow"), "{error}");
    }

    #[test]
    fn applications_do_not_inherit_the_nodes_proxy_unless_it_is_asked_for() {
        // The default has to be the closed one. `deno_fetch` reads the proxy
        // variables unconditionally, so a node that says nothing about them
        // would otherwise have its applications' egress decided by whatever
        // shell, unit file or container env the binary happened to start in.
        let config: RuntimeConfig = toml::from_str("http_port = 9000").unwrap();
        assert!(
            !config.fetch.proxy_from_env,
            "a node that says nothing must not hand applications the node's proxy"
        );

        let asked: RuntimeConfig = toml::from_str(
            r#"
            [fetch]
            proxy_from_env = true
        "#,
        )
        .unwrap();
        assert!(asked.fetch.proxy_from_env);
    }

    #[test]
    fn test_parse_toml_without_limits_uses_defaults() {
        let toml_str = "http_port = 9000";
        let config: RuntimeConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.default_execution_limits, ExecutionLimits::default());
    }

    #[test]
    fn test_parse_toml_logging_section() {
        let toml_str = r#"
            [logging]
            enabled = true
            endpoint = "http://127.0.0.1:8092"
            buffer_size = 512
            flush_interval_secs = 2
            batch_size = 64
            max_retries = 5
            max_pending_entries = 2048
        "#;
        let config: RuntimeConfig = toml::from_str(toml_str).unwrap();
        assert!(config.logging.enabled);
        assert_eq!(
            config.logging.endpoint.as_deref(),
            Some("http://127.0.0.1:8092")
        );
        assert_eq!(config.logging.buffer_size, 512);
        assert_eq!(config.logging.flush_interval_secs, 2);
        assert_eq!(config.logging.batch_size, 64);
        assert_eq!(config.logging.max_retries, 5);
        assert_eq!(config.logging.max_pending_entries, 2048);
        config.validate().unwrap();
    }

    #[test]
    fn test_validate_accepts_defaults() {
        RuntimeConfig::default().validate().unwrap();
    }

    #[test]
    fn test_validate_rejects_zero_admission_wait() {
        let config = RuntimeConfig {
            admission_wait_ms: 0,
            ..Default::default()
        };
        let err = config.validate().unwrap_err().to_string();
        assert!(err.contains("admission_wait_ms"), "{err}");
    }

    #[test]
    fn test_validate_rejects_zero_overload_retry_after() {
        let config = RuntimeConfig {
            overload_retry_after_seconds: 0,
            ..Default::default()
        };
        let err = config.validate().unwrap_err().to_string();
        assert!(err.contains("overload_retry_after_seconds"), "{err}");
    }

    #[test]
    fn test_admission_wait_beyond_the_edge_read_timeout_is_warned_about() {
        // A warning rather than a refusal, and for a reason the check has to
        // carry: what it compares against is the edge's *default* read timeout,
        // because these two crates do not depend on each other, and an operator
        // whose edge waits longer than the default is entitled to raise the
        // budget past it.
        let default = RuntimeConfig::default();
        assert_eq!(default.admission_wait_ms, 10_000);
        assert!(default.admission_wait_warning().is_none());

        let config = RuntimeConfig {
            admission_wait_ms: PROXY_DEFAULT_UPSTREAM_READ_TIMEOUT_MS,
            ..Default::default()
        };
        let warning = config
            .admission_wait_warning()
            .expect("a budget at the edge's default read timeout must be reported");
        assert!(warning.contains("admission_wait_ms"), "{warning}");
        assert!(
            config.validate().is_ok(),
            "an over-long admission budget is reported at startup, not refused"
        );
    }

    #[test]
    fn test_validate_rejects_zero_timeout() {
        let mut config = RuntimeConfig::default();
        config.default_execution_limits.timeout_ms = 0;
        let err = config.validate().unwrap_err().to_string();
        assert!(err.contains("timeout_ms"), "{err}");
    }

    #[test]
    fn test_validate_rejects_timeout_below_floor() {
        let mut config = RuntimeConfig::default();
        config.default_execution_limits.timeout_ms = MIN_TIMEOUT_MS - 1;
        assert!(config.validate().is_err());
    }

    #[test]
    fn test_validate_rejects_zero_rate_limit() {
        let mut config = RuntimeConfig::default();
        config.default_execution_limits.rate_limit_ops_per_sec = 0.0;
        let err = config.validate().unwrap_err().to_string();
        assert!(err.contains("rate_limit_ops_per_sec"), "{err}");
    }

    #[test]
    fn test_validate_rejects_negative_rate_limit() {
        let mut config = RuntimeConfig::default();
        config.default_execution_limits.rate_limit_ops_per_sec = -1.0;
        assert!(config.validate().is_err());
    }

    #[test]
    fn test_validate_rejects_nan_rate_limit() {
        let mut config = RuntimeConfig::default();
        config.default_execution_limits.rate_limit_ops_per_sec = f64::NAN;
        assert!(config.validate().is_err());
    }

    #[test]
    fn test_validate_rejects_infinite_rate_limit() {
        let mut config = RuntimeConfig::default();
        config.default_execution_limits.rate_limit_ops_per_sec = f64::INFINITY;
        assert!(config.validate().is_err());
    }

    #[test]
    fn test_validate_rejects_zero_http_port() {
        let config = RuntimeConfig {
            http_port: 0,
            ..Default::default()
        };
        assert!(config.validate().is_err());
    }

    #[test]
    fn test_validate_rejects_zero_shutdown_grace() {
        let config = RuntimeConfig {
            shutdown_grace_period_secs: 0,
            ..Default::default()
        };
        assert!(config.validate().is_err());
    }

    #[test]
    fn test_validate_rejects_shipping_without_endpoint() {
        let mut config = RuntimeConfig::default();
        config.logging.enabled = true;
        let err = config.validate().unwrap_err().to_string();
        assert!(err.contains("logging.endpoint"), "{err}");
    }

    #[test]
    fn test_validate_rejects_retry_queue_smaller_than_batch() {
        let mut config = RuntimeConfig::default();
        config.logging.enabled = true;
        config.logging.endpoint = Some("http://127.0.0.1:8092".to_string());
        config.logging.batch_size = 100;
        config.logging.max_pending_entries = 10;
        assert!(config.validate().is_err());
    }

    #[test]
    fn test_validate_ignores_shipper_values_when_disabled() {
        let mut config = RuntimeConfig::default();
        config.logging.enabled = false;
        config.logging.batch_size = 0;
        config.logging.max_retries = 0;
        config.validate().unwrap();
    }

    #[test]
    fn test_validate_rejects_zero_buffer_even_when_disabled() {
        let mut config = RuntimeConfig::default();
        config.logging.buffer_size = 0;
        assert!(config.validate().is_err());
    }

    // `std::env` is process-global, and `cargo test` runs tests in parallel
    // threads within one process by default. The tests below mutate the same
    // variables, so they must never run concurrently with each other (or they
    // would observe each other's values mid-test); this lock is the only thing
    // serializing them.
    static ENV_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// The defect, stated as a test: a node that was told which file to run
    /// on and did not read it.
    ///
    /// Nothing read this binary's command line at all, so
    /// `deis-runtime -c /etc/deis-runtime.toml` started a node on default
    /// ports with no control plane and no outbound allowlist, and reported
    /// itself ready. Every other component of this platform takes `-c`.
    #[test]
    fn the_file_named_on_the_command_line_is_the_one_read() {
        let _guard = ENV_TEST_LOCK.lock().unwrap();

        let dir = tempfile::tempdir().expect("a temporary directory");
        let named = dir.path().join("named.toml");
        std::fs::write(&named, "http_port = 9101\n").expect("the file is written");
        let by_variable = dir.path().join("by-variable.toml");
        std::fs::write(&by_variable, "http_port = 9102\n").expect("the file is written");

        // With nothing named, the variable still decides, which is what every
        // installation that predates the flag depends on.
        env::set_var("DEIS_RUNTIME_CONFIG", &by_variable);
        let (config, source) = RuntimeConfig::load(None).expect("the variable's file is read");
        assert_eq!(config.http_port, 9102);
        assert_eq!(source.as_deref(), Some(by_variable.as_path()));

        // And a file named on the command line wins over it: the flag is the
        // more explicit of the two, and an operator running two nodes on one
        // host says so there.
        let (config, source) = RuntimeConfig::load(Some(&named)).expect("the named file is read");
        assert_eq!(
            config.http_port, 9101,
            "the file named on the command line was not the one read"
        );
        assert_eq!(source.as_deref(), Some(named.as_path()));

        env::remove_var("DEIS_RUNTIME_CONFIG");
    }

    /// A named file that is not there stops the node.
    ///
    /// The alternative is the failure this whole finding is about: falling
    /// back to the built-in defaults means default ports, no control plane and
    /// no allowlist, on a node whose `/health` says it is fine.
    #[test]
    fn a_named_configuration_file_that_is_missing_is_refused() {
        let _guard = ENV_TEST_LOCK.lock().unwrap();
        env::remove_var("DEIS_RUNTIME_CONFIG");

        let missing = Path::new("/nonexistent/deis-runtime.toml");
        let err = RuntimeConfig::load(Some(missing))
            .expect_err("a node told to run on a file that is not there must not start")
            .to_string();
        assert!(
            err.contains("/nonexistent/deis-runtime.toml"),
            "the refusal has to name the file: {err}"
        );
    }

    #[test]
    fn test_env_overrides_limits() {
        let _guard = ENV_TEST_LOCK.lock().unwrap();

        env::set_var("DEIS_RUNTIME_DEFAULT_EXECUTION_LIMITS_TIMEOUT_MS", "12345");
        env::set_var(
            "DEIS_RUNTIME_DEFAULT_EXECUTION_LIMITS_WAIT_UNTIL_TIMEOUT_MS",
            "54321",
        );
        env::set_var(
            "DEIS_RUNTIME_DEFAULT_EXECUTION_LIMITS_RATE_LIMIT_OPS_PER_SEC",
            "42.5",
        );
        env::set_var(
            "DEIS_RUNTIME_DEFAULT_EXECUTION_LIMITS_CPU_TIME_LIMIT_MS",
            "777",
        );

        let mut config = RuntimeConfig::default();
        config.apply_env_overrides().unwrap();

        env::remove_var("DEIS_RUNTIME_DEFAULT_EXECUTION_LIMITS_TIMEOUT_MS");
        env::remove_var("DEIS_RUNTIME_DEFAULT_EXECUTION_LIMITS_WAIT_UNTIL_TIMEOUT_MS");
        env::remove_var("DEIS_RUNTIME_DEFAULT_EXECUTION_LIMITS_RATE_LIMIT_OPS_PER_SEC");
        env::remove_var("DEIS_RUNTIME_DEFAULT_EXECUTION_LIMITS_CPU_TIME_LIMIT_MS");

        assert_eq!(config.default_execution_limits.timeout_ms, 12345);
        assert_eq!(config.default_execution_limits.wait_until_timeout_ms, 54321);
        assert_eq!(config.default_execution_limits.rate_limit_ops_per_sec, 42.5);
        assert_eq!(config.default_execution_limits.cpu_time_limit_ms, 777);
    }

    #[test]
    fn test_env_override_rejects_invalid_timeout() {
        let _guard = ENV_TEST_LOCK.lock().unwrap();

        env::set_var(
            "DEIS_RUNTIME_DEFAULT_EXECUTION_LIMITS_TIMEOUT_MS",
            "not-a-number",
        );
        let mut config = RuntimeConfig::default();
        let result = config.apply_env_overrides();
        env::remove_var("DEIS_RUNTIME_DEFAULT_EXECUTION_LIMITS_TIMEOUT_MS");
        assert!(result.is_err());
    }

    #[test]
    fn test_env_overrides_logging() {
        let _guard = ENV_TEST_LOCK.lock().unwrap();

        env::set_var("DEIS_RUNTIME_LOGGING_ENABLED", "true");
        env::set_var("DEIS_RUNTIME_LOGGING_ENDPOINT", "http://127.0.0.1:8092");
        env::set_var("DEIS_RUNTIME_LOGGING_BATCH_SIZE", "7");

        let mut config = RuntimeConfig::default();
        config.apply_env_overrides().unwrap();

        env::remove_var("DEIS_RUNTIME_LOGGING_ENABLED");
        env::remove_var("DEIS_RUNTIME_LOGGING_ENDPOINT");
        env::remove_var("DEIS_RUNTIME_LOGGING_BATCH_SIZE");

        assert!(config.logging.enabled);
        assert_eq!(
            config.logging.endpoint.as_deref(),
            Some("http://127.0.0.1:8092")
        );
        assert_eq!(config.logging.batch_size, 7);
    }

    #[test]
    fn test_empty_env_value_is_treated_as_unset() {
        let _guard = ENV_TEST_LOCK.lock().unwrap();

        env::set_var("DEIS_RUNTIME_DEFAULT_EXECUTION_LIMITS_TIMEOUT_MS", "");
        let mut config = RuntimeConfig::default();
        let result = config.apply_env_overrides();
        env::remove_var("DEIS_RUNTIME_DEFAULT_EXECUTION_LIMITS_TIMEOUT_MS");

        assert!(result.is_ok());
        assert_eq!(config.default_execution_limits.timeout_ms, 30_000);
    }

    #[test]
    fn the_two_node_ceilings_can_be_set_from_the_environment() {
        let _guard = ENV_TEST_LOCK.lock().unwrap();

        env::set_var("DEIS_RUNTIME_MAX_REQUEST_BODY_BYTES", "1048576");
        env::set_var("DEIS_RUNTIME_DEFAULT_MEMORY_LIMIT_MB", "64");
        let mut config = RuntimeConfig::default();
        let result = config.apply_env_overrides();
        env::remove_var("DEIS_RUNTIME_MAX_REQUEST_BODY_BYTES");
        env::remove_var("DEIS_RUNTIME_DEFAULT_MEMORY_LIMIT_MB");

        result.expect("both overrides parse");
        assert_eq!(config.max_request_body_bytes, 1_048_576);
        assert_eq!(config.default_memory_limit_mb, 64);
    }

    #[test]
    fn the_two_node_ceilings_come_out_of_the_toml() {
        let config: RuntimeConfig =
            toml::from_str("max_request_body_bytes = 8192\ndefault_memory_limit_mb = 512\n")
                .expect("the keys parse");
        assert_eq!(config.max_request_body_bytes, 8192);
        assert_eq!(config.default_memory_limit_mb, 512);
        config.validate().expect("both values are usable");
    }

    #[test]
    fn a_zero_request_body_ceiling_is_refused() {
        let config = RuntimeConfig {
            max_request_body_bytes: 0,
            ..Default::default()
        };
        let err = config.validate().unwrap_err().to_string();
        assert!(err.contains("max_request_body_bytes"), "{err}");
    }

    #[test]
    fn a_heap_ceiling_below_the_floor_is_refused() {
        // Small enough that the isolate's own bootstrap would exhaust it, so
        // the node would refuse every application it was assigned and the
        // reason would read like an application fault.
        let config = RuntimeConfig {
            default_memory_limit_mb: 1,
            ..Default::default()
        };
        let err = config.validate().unwrap_err().to_string();
        assert!(err.contains("default_memory_limit_mb"), "{err}");
        assert!(err.contains(&MIN_MEMORY_LIMIT_MB.to_string()), "{err}");
    }

    #[test]
    fn test_removed_env_var_fails_loudly() {
        let _guard = ENV_TEST_LOCK.lock().unwrap();

        env::set_var("DEIS_RUNTIME_DEFAULT_TIMEOUT_MS", "12345");
        let result = reject_removed_env_vars();
        env::remove_var("DEIS_RUNTIME_DEFAULT_TIMEOUT_MS");

        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("DEIS_RUNTIME_DEFAULT_EXECUTION_LIMITS_TIMEOUT_MS"),
            "{err}"
        );
    }

    #[test]
    fn test_parse_toml_delivery_sections() {
        let toml_str = r#"
            node_id = "node-a"

            [control]
            endpoint = "http://127.0.0.1:8080"
            request_timeout_secs = 5
            retry_interval_secs = 3

            # A node that names a control plane has to be able to prove itself
            # to it; this section is not what the test is about, it is what
            # makes the configuration a legal one.
            [machine_identity]
            certificate = "/etc/deislet/node.crt"
            private_key = "/etc/deislet/node.key"
            trust_root = "/etc/deislet/root.pem"
            trust_domain = "deis.internal"
            control_endpoint = "https://127.0.0.1:8443"

            [bundles]
            dir = "/var/lib/deis/bundles"
            max_bytes = 1048576
            download_timeout_secs = 60

            [assets]
            cache_control = "public, max-age=60"
            max_bytes = 2048
        "#;
        let config: RuntimeConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.node_id.as_deref(), Some("node-a"));
        assert_eq!(
            config.control.endpoint.as_deref(),
            Some("http://127.0.0.1:8080")
        );
        assert_eq!(config.control.request_timeout_secs, 5);
        assert_eq!(config.bundles.max_bytes, 1_048_576);
        assert_eq!(config.assets.cache_control, "public, max-age=60");
        assert_eq!(config.assets.max_bytes, 2048);
        config.validate().unwrap();
    }

    #[test]
    fn test_bundle_dir_defaults_under_the_data_dir() {
        let config = RuntimeConfig {
            data_dir: PathBuf::from("/srv/deis"),
            ..Default::default()
        };
        assert_eq!(config.bundle_dir(), PathBuf::from("/srv/deis/bundles"));
    }

    #[test]
    fn test_bundle_dir_honours_an_explicit_setting() {
        let mut config = RuntimeConfig::default();
        config.bundles.dir = Some(PathBuf::from("/mnt/bundles"));
        assert_eq!(config.bundle_dir(), PathBuf::from("/mnt/bundles"));
    }

    #[test]
    fn test_node_id_falls_back_to_the_hostname() {
        let mut config = RuntimeConfig::default();
        assert!(!config.resolved_node_id().is_empty());
        config.node_id = Some("  spaced-node  ".to_string());
        assert_eq!(config.resolved_node_id(), "spaced-node");
    }

    #[test]
    fn test_validate_rejects_a_control_endpoint_without_a_scheme() {
        let mut config = RuntimeConfig::default();
        config.control.endpoint = Some("127.0.0.1:8080".to_string());
        let err = config.validate().unwrap_err().to_string();
        assert!(err.contains("http://"), "{err}");
    }

    #[test]
    fn test_validate_rejects_an_empty_control_endpoint() {
        let mut config = RuntimeConfig::default();
        config.control.endpoint = Some("   ".to_string());
        assert!(config.validate().is_err());
    }

    #[test]
    fn test_validate_rejects_a_zero_bundle_ceiling() {
        let mut config = RuntimeConfig::default();
        config.bundles.max_bytes = 0;
        let err = config.validate().unwrap_err().to_string();
        assert!(err.contains("max_bytes"), "{err}");
    }

    #[test]
    fn test_validate_rejects_a_zero_asset_ceiling() {
        let mut config = RuntimeConfig::default();
        config.assets.max_bytes = 0;
        assert!(config.validate().is_err());
    }

    #[test]
    fn test_validate_rejects_an_empty_cache_control() {
        let mut config = RuntimeConfig::default();
        config.assets.cache_control = String::new();
        assert!(config.validate().is_err());
    }

    #[test]
    fn test_validate_rejects_a_zero_retry_interval() {
        let mut config = RuntimeConfig::default();
        config.control.retry_interval_secs = 0;
        assert!(config.validate().is_err());
    }

    #[test]
    fn test_env_overrides_delivery_settings() {
        let _guard = ENV_TEST_LOCK.lock().unwrap();

        env::set_var("DEIS_RUNTIME_NODE_ID", "node-from-env");
        env::set_var("DEIS_RUNTIME_CONTROL_ENDPOINT", "http://control:8080");
        env::set_var("DEIS_RUNTIME_BUNDLES_MAX_BYTES", "4096");
        env::set_var("DEIS_RUNTIME_ASSETS_MAX_BYTES", "8192");

        let mut config = RuntimeConfig::default();
        let result = config.apply_env_overrides();

        env::remove_var("DEIS_RUNTIME_NODE_ID");
        env::remove_var("DEIS_RUNTIME_CONTROL_ENDPOINT");
        env::remove_var("DEIS_RUNTIME_BUNDLES_MAX_BYTES");
        env::remove_var("DEIS_RUNTIME_ASSETS_MAX_BYTES");

        result.unwrap();
        assert_eq!(config.node_id.as_deref(), Some("node-from-env"));
        assert_eq!(
            config.control.endpoint.as_deref(),
            Some("http://control:8080")
        );
        assert_eq!(config.bundles.max_bytes, 4096);
        assert_eq!(config.assets.max_bytes, 8192);
    }

    #[test]
    fn test_machine_identity_is_absent_by_default() {
        // Absent means today's behaviour exactly: no section to half-fill,
        // nothing to reject.
        let config = RuntimeConfig::default();
        assert!(config.machine_identity.is_none());
        assert!(config.validate().is_ok());
    }

    #[test]
    fn test_validate_rejects_a_machine_identity_missing_its_key_and_trust_root() {
        let config = with_machine_channel(MachineIdentityConfig {
            certificate: Some(PathBuf::from("/etc/deislet/node.crt")),
            trust_domain: Some("deis.internal".to_string()),
            control_endpoint: Some("https://control.internal:8443".to_string()),
            ..MachineIdentityConfig::default()
        });
        let err = config.validate().unwrap_err().to_string();
        // Names both missing files in one pass, and not the one that was
        // already set.
        assert!(err.contains("is missing private_key, trust_root"), "{err}");
    }

    #[test]
    fn test_validate_rejects_a_machine_identity_with_no_way_to_name_the_control_plane() {
        let config = with_machine_channel(MachineIdentityConfig {
            certificate: Some(PathBuf::from("/etc/deislet/node.crt")),
            private_key: Some(PathBuf::from("/etc/deislet/node.key")),
            trust_root: Some(PathBuf::from("/etc/deislet/root.pem")),
            control_endpoint: Some("https://control.internal:8443".to_string()),
            ..MachineIdentityConfig::default()
        });
        let err = config.validate().unwrap_err().to_string();
        assert!(err.contains("trust_domain"), "{err}");
        assert!(err.contains("control_identity"), "{err}");
    }

    #[test]
    fn test_validate_rejects_a_trust_domain_that_is_not_a_valid_spiffe_segment() {
        let config = with_machine_channel(MachineIdentityConfig {
            certificate: Some(PathBuf::from("/etc/deislet/node.crt")),
            private_key: Some(PathBuf::from("/etc/deislet/node.key")),
            trust_root: Some(PathBuf::from("/etc/deislet/root.pem")),
            trust_domain: Some("Not A Domain".to_string()),
            control_endpoint: Some("https://control.internal:8443".to_string()),
            ..MachineIdentityConfig::default()
        });
        assert!(config.validate().is_err());
    }

    #[test]
    fn test_validate_rejects_a_malformed_control_identity_override() {
        let config = with_machine_channel(MachineIdentityConfig {
            certificate: Some(PathBuf::from("/etc/deislet/node.crt")),
            private_key: Some(PathBuf::from("/etc/deislet/node.key")),
            trust_root: Some(PathBuf::from("/etc/deislet/root.pem")),
            control_identity: Some("not-a-spiffe-uri".to_string()),
            control_endpoint: Some("https://control.internal:8443".to_string()),
            ..MachineIdentityConfig::default()
        });
        let err = config.validate().unwrap_err().to_string();
        assert!(err.contains("control_identity"), "{err}");
    }

    #[test]
    fn test_validate_rejects_a_zero_identity_reload_interval() {
        let config = with_machine_channel(MachineIdentityConfig {
            certificate: Some(PathBuf::from("/etc/deislet/node.crt")),
            private_key: Some(PathBuf::from("/etc/deislet/node.key")),
            trust_root: Some(PathBuf::from("/etc/deislet/root.pem")),
            trust_domain: Some("deis.internal".to_string()),
            identity_reload_seconds: 0,
            control_endpoint: Some("https://control.internal:8443".to_string()),
            ..MachineIdentityConfig::default()
        });
        let err = config.validate().unwrap_err().to_string();
        assert!(err.contains("identity_reload_seconds"), "{err}");
    }

    #[test]
    fn test_validate_accepts_a_complete_machine_identity_derived_from_a_trust_domain() {
        let config = with_machine_channel(MachineIdentityConfig {
            certificate: Some(PathBuf::from("/etc/deislet/node.crt")),
            private_key: Some(PathBuf::from("/etc/deislet/node.key")),
            trust_root: Some(PathBuf::from("/etc/deislet/root.pem")),
            trust_domain: Some("deis.internal".to_string()),
            control_endpoint: Some("https://control.internal:8443".to_string()),
            ..MachineIdentityConfig::default()
        });
        config
            .validate()
            .expect("a complete section must be accepted");
        assert_eq!(
            config
                .machine_identity
                .as_ref()
                .unwrap()
                .expected_control_identity()
                .unwrap()
                .to_string(),
            "spiffe://deis.internal/deislet/control"
        );
    }

    #[test]
    fn test_explicit_control_identity_wins_over_a_configured_trust_domain() {
        // Both set at once is not a conflict this refuses: `control_identity`
        // is the named escape hatch for whatever the derivation cannot cover,
        // so it simply wins - see the struct doc.
        let config = with_machine_channel(MachineIdentityConfig {
            certificate: Some(PathBuf::from("/etc/deislet/node.crt")),
            private_key: Some(PathBuf::from("/etc/deislet/node.key")),
            trust_root: Some(PathBuf::from("/etc/deislet/root.pem")),
            trust_domain: Some("wrong.internal".to_string()),
            control_identity: Some("spiffe://deis.internal/deislet/control".to_string()),
            control_endpoint: Some("https://control.internal:8443".to_string()),
            ..MachineIdentityConfig::default()
        });
        config
            .validate()
            .expect("a complete section must be accepted");
        assert_eq!(
            config
                .machine_identity
                .as_ref()
                .unwrap()
                .expected_control_identity()
                .unwrap()
                .to_string(),
            "spiffe://deis.internal/deislet/control"
        );
    }

    /// The three files plus a trust domain, and no address: a node that would
    /// have loaded a certificate, logged that it had, and then asked nobody
    /// for anything. Refused, naming both settings, because the operator who
    /// wrote the section believes the opposite.
    #[test]
    fn test_validate_rejects_an_identity_with_nowhere_to_dial() {
        let config = with_machine_channel(MachineIdentityConfig {
            certificate: Some(PathBuf::from("/etc/deislet/node.crt")),
            private_key: Some(PathBuf::from("/etc/deislet/node.key")),
            trust_root: Some(PathBuf::from("/etc/deislet/root.pem")),
            trust_domain: Some("deis.internal".to_string()),
            ..MachineIdentityConfig::default()
        });
        let err = config.validate().unwrap_err().to_string();
        assert!(err.contains("control_endpoint"), "{err}");
        assert!(err.contains("management_listen"), "{err}");
    }

    /// The one shape that has an identity, no address to dial, and is still
    /// useful: a node driven by something beside it and fetching nothing.
    /// `deis dev` is exactly this, and it is the reason the rule above is
    /// "neither of the two" rather than "not control_endpoint".
    #[test]
    fn an_identity_that_only_serves_a_management_plane_is_accepted() {
        let config = with_machine_channel(MachineIdentityConfig {
            certificate: Some(PathBuf::from("/etc/deislet/node.crt")),
            private_key: Some(PathBuf::from("/etc/deislet/node.key")),
            trust_root: Some(PathBuf::from("/etc/deislet/root.pem")),
            control_identity: Some("spiffe://deis.dev/deislet/control".to_string()),
            management_listen: Some("127.0.0.1:8187".to_string()),
            ..MachineIdentityConfig::default()
        });
        config
            .validate()
            .expect("a node that only serves its management plane is a configuration, not a typo");
    }

    /// A machine channel with no `control.endpoint` beside it used to be
    /// refused, because registration was still an HTTP route on the human
    /// listener and a node with only a certificate could be told what to run
    /// and never say it exists. Registration moved, so that stopped being
    /// true, and `control.endpoint` is now read only by a node that has no
    /// identity at all - it must not be demanded of one that does.
    #[test]
    fn a_machine_channel_needs_no_second_address_beside_it() {
        let config = RuntimeConfig {
            machine_identity: Some(complete_identity()),
            control: ControlPlaneConfig {
                endpoint: None,
                ..ControlPlaneConfig::default()
            },
            ..RuntimeConfig::default()
        };
        config
            .validate()
            .expect("everything this node asks for is on the machine channel");
    }

    /// And the mirror image, which is the quieter of the two.
    ///
    /// An address to dial with nothing to prove when it gets there used to be
    /// unwritable: the endpoint and the certificate that goes with it live in
    /// one table, so they could not be configured apart. `[control]` is the
    /// older spelling and still has an address of its own, so the pair can be
    /// split there - and a node that splits them does not fail.
    ///
    /// It starts, binds its data port and retries a route that answers 404 for
    /// as long as it runs: no version loaded, nothing announced, and nothing
    /// in its log that names the cause. Refusing at startup is the whole
    /// point, so the message has to name the table that is missing.
    #[test]
    fn a_node_that_names_a_control_plane_and_can_prove_nothing_is_refused() {
        let config = RuntimeConfig {
            machine_identity: None,
            control: ControlPlaneConfig {
                endpoint: Some("http://control.internal:8080".to_string()),
                ..ControlPlaneConfig::default()
            },
            ..RuntimeConfig::default()
        };
        let err = config.validate().unwrap_err().to_string();
        assert!(err.contains("machine_identity"), "{err}");
        assert!(
            err.contains("404"),
            "the message has to say what would have happened instead: {err}"
        );
    }

    #[test]
    fn test_a_configuration_with_no_machine_channel_is_still_valid() {
        RuntimeConfig::default()
            .validate()
            .expect("a node with no machine channel configured is the ordinary case");
    }

    #[test]
    fn test_validate_rejects_a_machine_endpoint_that_is_not_tls() {
        let config = with_machine_channel(MachineIdentityConfig {
            control_endpoint: Some("http://control.internal:8443".to_string()),
            ..complete_identity()
        });
        let err = config.validate().unwrap_err().to_string();
        assert!(err.contains("https://"), "{err}");
    }

    /// The dialler has no default port, so an address without one fails on
    /// every connection instead of at startup. An IPv6 literal is the case
    /// that has to be got right rather than merely rejected: its own colons
    /// are not a port, and its bracketed form with a port is ordinary.
    #[test]
    fn test_validate_rejects_a_machine_endpoint_with_no_port() {
        let at = |endpoint: &str| {
            with_machine_channel(MachineIdentityConfig {
                control_endpoint: Some(endpoint.to_string()),
                ..complete_identity()
            })
        };

        let err = at("https://control.internal")
            .validate()
            .unwrap_err()
            .to_string();
        assert!(err.contains("names no port"), "{err}");

        assert!(
            at("https://[::1]").validate().is_err(),
            "an IPv6 literal's own colons are not a port"
        );

        at("https://[::1]:8443")
            .validate()
            .expect("a bracketed IPv6 address with a port is an ordinary machine endpoint");
    }

    /// Neither half set is the default and the only configuration that must
    /// keep behaving in every respect as it did before the machine channel
    /// existed.
    #[test]
    fn test_validate_accepts_a_node_with_neither_half_of_the_machine_channel() {
        let config = RuntimeConfig::default();
        assert!(config.machine_identity.is_none());
        assert!(config.machine_identity.is_none());
        config.validate().expect("the default must stay valid");
    }

    /// A node with a machine channel is still a node that has to register,
    /// and registration has not moved off the human listener - so a valid
    /// configuration carries both addresses. Tests about the machine channel
    /// say only what they are about and take the rest from here.
    fn with_machine_channel(identity: MachineIdentityConfig) -> RuntimeConfig {
        RuntimeConfig {
            machine_identity: Some(identity),
            control: ControlPlaneConfig {
                endpoint: Some("http://control.internal:8080".to_string()),
                ..ControlPlaneConfig::default()
            },
            ..RuntimeConfig::default()
        }
    }

    fn complete_identity() -> MachineIdentityConfig {
        MachineIdentityConfig {
            certificate: Some(PathBuf::from("/etc/deislet/node.crt")),
            private_key: Some(PathBuf::from("/etc/deislet/node.key")),
            trust_root: Some(PathBuf::from("/etc/deislet/root.pem")),
            trust_domain: Some("deis.internal".to_string()),
            control_endpoint: Some("https://control.internal:8443".to_string()),
            ..MachineIdentityConfig::default()
        }
    }

    #[test]
    fn test_a_machine_identity_serves_no_management_plane_unless_told_where() {
        // The default, and every node configured before the setting existed.
        // It has to stay a complete configuration: the direction that has
        // moved to certificates is this node's outbound half, and refusing a
        // section for not also serving would turn a staged migration into a
        // startup failure on nodes that are dialling correctly.
        let identity = complete_identity();
        assert!(identity.management_listen.is_none());
        assert!(identity.management_listen_addr().unwrap().is_none());
        with_machine_channel(identity).validate().unwrap();
    }

    #[test]
    fn test_a_machine_management_listener_is_read_as_an_address() {
        let identity = MachineIdentityConfig {
            management_listen: Some("0.0.0.0:8187".to_string()),
            ..complete_identity()
        };
        assert_eq!(
            identity.management_listen_addr().unwrap(),
            Some("0.0.0.0:8187".parse().unwrap())
        );
        with_machine_channel(identity).validate().unwrap();
    }

    #[test]
    fn test_validate_rejects_a_machine_management_listener_that_says_nothing() {
        // Empty reads exactly like absent to `management_listen_addr`, so an
        // operator who meant to fill it in would get a node that came up
        // dialling happily and serving no management plane at all.
        for listen in ["", "   "] {
            let config = with_machine_channel(MachineIdentityConfig {
                management_listen: Some(listen.to_string()),
                ..complete_identity()
            });
            let err = config.validate().unwrap_err().to_string();
            assert!(err.contains("machine_identity.management_listen"), "{err}");
        }
    }

    #[test]
    fn test_validate_rejects_a_machine_management_listener_that_is_not_an_address() {
        // A hostname is refused for the reason management.listen refuses one:
        // which interface a management plane binds must be readable off the
        // file and not off whatever DNS answered at startup.
        for listen in ["node.internal:8187", "8187", "0.0.0.0"] {
            let config = with_machine_channel(MachineIdentityConfig {
                management_listen: Some(listen.to_string()),
                ..complete_identity()
            });
            let err = config.validate().unwrap_err().to_string();
            assert!(
                err.contains("machine_identity.management_listen"),
                "{listen} must be refused by name: {err}"
            );
        }
    }

    #[test]
    fn test_validate_rejects_a_machine_management_listener_on_an_arbitrary_port() {
        let config = with_machine_channel(MachineIdentityConfig {
            management_listen: Some("0.0.0.0:0".to_string()),
            ..complete_identity()
        });
        let err = config.validate().unwrap_err().to_string();
        assert!(err.contains("machine_identity.management_listen"), "{err}");
    }

    #[test]
    fn test_validate_rejects_a_machine_management_listener_sharing_a_port() {
        // The data port. `0.0.0.0:http_port` covers every address on this
        // host, so any spelling of this port is the same socket.
        let config = with_machine_channel(MachineIdentityConfig {
            management_listen: Some(format!("127.0.0.1:{}", RuntimeConfig::default().http_port)),
            ..complete_identity()
        });
        let err = config.validate().unwrap_err().to_string();
        assert!(err.contains("http_port"), "{err}");
    }

    #[test]
    fn test_machine_identity_config_defaults_the_reload_interval() {
        let identity = MachineIdentityConfig::default();
        assert_eq!(identity.identity_reload_seconds, 60);
    }

    #[test]
    fn test_parse_toml_machine_identity_section() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("deis-runtime.toml");
        std::fs::write(
            &path,
            r#"
            [control]
            endpoint = "http://127.0.0.1:8080"

            [machine_identity]
            control_endpoint = "https://127.0.0.1:8443"
            certificate = "/etc/deislet/node.crt"
            private_key = "/etc/deislet/node.key"
            trust_root = "/etc/deislet/root.pem"
            trust_domain = "deis.internal"
            identity_reload_seconds = 45
            "#,
        )
        .unwrap();

        let config = RuntimeConfig::load_from(&path).expect("must parse");
        config.validate().expect("must validate");
        assert_eq!(
            config
                .machine_identity
                .as_ref()
                .and_then(|m| m.control_endpoint.as_deref()),
            Some("https://127.0.0.1:8443")
        );
        let identity = config.machine_identity.expect("section must be read");
        assert_eq!(
            identity.certificate,
            Some(PathBuf::from("/etc/deislet/node.crt"))
        );
        assert_eq!(identity.identity_reload_seconds, 45);
        assert_eq!(identity.trust_domain.as_deref(), Some("deis.internal"));
        assert!(identity.control_identity.is_none());
    }

    #[test]
    fn test_env_overrides_machine_identity() {
        let _guard = ENV_TEST_LOCK.lock().unwrap();

        env::set_var("DEIS_RUNTIME_MACHINE_IDENTITY_CERTIFICATE", "/env/node.crt");
        env::set_var("DEIS_RUNTIME_MACHINE_IDENTITY_PRIVATE_KEY", "/env/node.key");
        env::set_var("DEIS_RUNTIME_MACHINE_IDENTITY_TRUST_ROOT", "/env/root.pem");
        env::set_var(
            "DEIS_RUNTIME_MACHINE_IDENTITY_TRUST_DOMAIN",
            "deis.internal",
        );
        env::set_var("DEIS_RUNTIME_MACHINE_IDENTITY_RELOAD_SECONDS", "10");
        env::set_var(
            "DEIS_RUNTIME_MACHINE_IDENTITY_CONTROL_ENDPOINT",
            "https://127.0.0.1:8443",
        );

        let mut config = RuntimeConfig::default();
        let result = config.apply_env_overrides();

        env::remove_var("DEIS_RUNTIME_MACHINE_IDENTITY_CERTIFICATE");
        env::remove_var("DEIS_RUNTIME_MACHINE_IDENTITY_PRIVATE_KEY");
        env::remove_var("DEIS_RUNTIME_MACHINE_IDENTITY_TRUST_ROOT");
        env::remove_var("DEIS_RUNTIME_MACHINE_IDENTITY_TRUST_DOMAIN");
        env::remove_var("DEIS_RUNTIME_MACHINE_IDENTITY_RELOAD_SECONDS");
        env::remove_var("DEIS_RUNTIME_MACHINE_IDENTITY_CONTROL_ENDPOINT");

        result.unwrap();
        // Registration still lives on the human listener, and validate now says
        // so; this test is about the env overrides, not about that rule.
        config.control.endpoint = Some("http://127.0.0.1:8080".to_string());
        let identity = config
            .machine_identity
            .as_ref()
            .expect("an env override must create the section");
        assert_eq!(identity.certificate, Some(PathBuf::from("/env/node.crt")));
        assert_eq!(identity.private_key, Some(PathBuf::from("/env/node.key")));
        assert_eq!(identity.trust_root, Some(PathBuf::from("/env/root.pem")));
        assert_eq!(identity.trust_domain.as_deref(), Some("deis.internal"));
        assert_eq!(identity.identity_reload_seconds, 10);
        assert_eq!(
            config
                .machine_identity
                .as_ref()
                .and_then(|m| m.control_endpoint.as_deref()),
            Some("https://127.0.0.1:8443")
        );
        config
            .validate()
            .expect("the env-built section must validate");
    }

    #[test]
    fn test_the_shipped_example_config_is_loadable() {
        // The file next to this crate is what an operator copies. A setting
        // documented there that this build does not accept - a renamed key, a
        // value outside its range - would only show up when someone tried to
        // start a node with it.
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("deis-runtime.toml");
        let config = RuntimeConfig::load_from(&path).expect("deis-runtime.toml must parse");
        config
            .validate()
            .expect("deis-runtime.toml must pass validation");
        assert_eq!(config.http_port, 8000);
        assert_eq!(
            config.assets.cache_control,
            "public, max-age=0, must-revalidate"
        );
        // Read out of the file rather than taken from the default, so a
        // ceiling that exists in the code and not in the file an operator
        // copies is a failing test rather than a setting nobody can find.
        assert_eq!(config.default_execution_limits.cpu_time_limit_ms, 5_000);
        assert_eq!(
            config.default_execution_limits.wait_until_timeout_ms,
            30_000
        );
    }

    #[test]
    fn a_config_file_that_still_sets_a_renamed_key_fails_loudly() {
        // Serde drops an unknown key, so a file that still says
        // `max_bundle_bytes` would leave the node on the 256 MB default while
        // the operator's file said 1 MB, and nothing would report it.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("deis-runtime.toml");
        std::fs::write(
            &path,
            "[bundles]\nmax_bundle_bytes = 1048576\ndownload_timeout_secs = 60\n",
        )
        .unwrap();

        let err = RuntimeConfig::load_from(&path)
            .expect_err("a key this node no longer reads must stop startup")
            .to_string();

        assert!(err.contains("bundles.max_bundle_bytes"), "{err}");
        assert!(err.contains("bundles.max_bytes"), "{err}");
    }

    #[test]
    fn test_every_removed_config_key_points_at_a_current_name() {
        // The replacement has to be a key this build actually reads, or the
        // error sends the operator to a second name that also does nothing.
        let rendered = toml::to_string(&RuntimeConfig::default())
            .expect("the default configuration serialises");
        let parsed: toml::Value = toml::from_str(&rendered).expect("and parses back");
        for (removed, replacement) in REMOVED_CONFIG_KEYS {
            let mut node = Some(&parsed);
            for segment in replacement.split('.') {
                node = node.and_then(|value| value.get(segment));
            }
            assert!(
                node.is_some(),
                "{removed} points at {replacement}, which this build does not read either"
            );
        }
    }

    #[test]
    fn test_every_removed_env_var_points_at_a_current_name() {
        // A replacement name that does not follow the DEIS_RUNTIME_<path>
        // rule would send the operator to a variable that also does nothing.
        for (removed, replacement) in REMOVED_ENV_VARS {
            assert!(
                replacement.starts_with("DEIS_RUNTIME_"),
                "{removed} points at {replacement}, which is not a DEIS_RUNTIME_ name"
            );
        }
    }
}
