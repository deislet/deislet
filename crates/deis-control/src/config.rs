use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use tracing::{info, warn};

#[derive(Clone, Serialize, Deserialize)]
pub struct ControlConfig {
    pub bind_addr: String,
    pub port: u16,
    /// How often a node is asked to beat, in seconds. Handed back to the node
    /// in every heartbeat acknowledgement, and also the rate at which
    /// [`crate::reaper`] reads the two clocks below.
    pub heartbeat_interval: u64,
    /// How long a node may be silent, in seconds, before this control plane
    /// stops reporting it as live and moves its session to `Unresponsive`.
    ///
    /// This has to be comfortably longer than `heartbeat_interval` - two or
    /// three times it - because a node that is perfectly healthy is still
    /// silent for one whole interval between beats. A value at or below the
    /// interval makes the whole fleet flap, and is warned about at startup.
    ///
    /// A node is reported unresponsive between this and this plus one
    /// interval after it goes quiet: the timeout is when the claim expires,
    /// the interval is how long it can take to be noticed.
    pub heartbeat_timeout: u64,
    /// The credential shared with every node, in both directions: what a node
    /// presents on its gRPC control channel and on the two HTTP endpoints
    /// that serve nodes, and what this control plane presents back when it
    /// calls into a node's own management plane. One credential per node, not
    /// two.
    ///
    /// More than one value is accepted at once so the fleet can be rotated
    /// without an outage. Which value goes *out* to a node is decided per
    /// node, not by position in this list: a node that registered is driven
    /// with the value it proved when it did, and a node from
    /// `deis_runtime_nodes` that has not registered yet with the first one -
    /// see `crate::runtime_client::NodeKeyStatus`. Ordering therefore only
    /// matters for those configured-but-silent nodes.
    ///
    /// It is never allowed to be empty on a running control plane: an empty
    /// list would leave `PUT /api/nodes/{id}/registration` open to anyone who
    /// can reach the port, and a caller that can inject a node becomes a
    /// delivery target that is handed every environment's decrypted secrets.
    /// [`ControlConfig::ensure_node_credential`] mints one when the file names
    /// none, and the endpoints refuse everything while the list is empty.
    pub node_preshared_keys: Vec<String>,
    /// API Keys for HTTP API authentication (optional)
    #[serde(default)]
    pub api_keys: Vec<String>,
    /// Enable API authentication (default: false for backward compatibility)
    #[serde(default)]
    pub api_auth_enabled: bool,
    /// Origins a web page may read this control plane's answers from, each
    /// written in full - "https://console.example.com", scheme and port
    /// included, no trailing slash.
    ///
    /// Empty, which is the default, means none. The console this binary
    /// serves is delivered from this same port, so every call it makes is
    /// same-origin and needs no entry here; `npm run dev` proxies `/api`
    /// through the vite server, so that is same-origin too. An entry is
    /// needed only for a console hosted somewhere else.
    ///
    /// It used to be `*` - every origin, all methods, all headers - and
    /// combined with `api_auth_enabled` defaulting to off that let any page
    /// the operator happened to visit create an owner account on their
    /// control plane, mint a root token, deploy, and read every answer.
    /// `*` is therefore rejected if it appears here: an origin that may
    /// read the audit log has to be named.
    ///
    /// Naming an origin does not hand it authority. A page on another origin
    /// never gets the unauthenticated admission that a caller on this
    /// machine gets - it presents a token like any other remote caller. See
    /// [`crate::api::authz::authorize`].
    #[serde(default)]
    pub api_cors_allowed_origins: Vec<String>,
    /// TLS configuration (optional)
    #[serde(default)]
    pub tls: Option<TlsConfig>,
    /// Runtime nodes this control plane knows about before any of them has
    /// said anything - the fallback, not the only source.
    ///
    /// A node registers itself with `PUT /api/nodes/{id}/registration` and
    /// is then eligible for deployment pushes and cron fires like any other,
    /// so a machine can join a running platform without this file being
    /// edited.
    ///
    /// What the list is still for: a control plane and its nodes started
    /// together by one script, where the control plane may need to reach a
    /// node before that node has managed to call in, and a node that must
    /// stay a delivery target even while it is down (an entry from here never
    /// expires, a registration does).
    #[serde(default)]
    pub deis_runtime_nodes: Vec<RuntimeNodeConfig>,
    /// Interval in seconds between deis-runtime node health polls (default: 15)
    #[serde(default = "default_deis_runtime_poll_interval")]
    pub deis_runtime_poll_interval: u64,
    /// Directory holding uploaded application bundles, one sub-directory
    /// per app and one content-addressed zip per version.
    ///
    /// This is the only copy of a version's code the platform keeps, so it
    /// belongs on persistent storage that survives a restart.
    ///
    /// Should be absolute. A relative value - and the default is one - is
    /// anchored to this process's working directory by
    /// [`crate::catalog::BundleStore::new`], which is right for the run that
    /// wrote it and wrong for the next one started from somewhere else.
    #[serde(default = "default_bundle_dir")]
    pub bundle_dir: PathBuf,
    /// Absolute base URL nodes use to reach this control plane, for
    /// instance "https://control.internal:8080".
    ///
    /// It ends up in the bundle download links handed to nodes. Leave it
    /// unset and the URL is derived from the bind address. Set it whenever
    /// nodes run on another host or reach this process through a proxy.
    #[serde(default)]
    pub public_base_url: Option<String>,
    /// File holding the 32-byte master key that encrypts environment
    /// secrets (AES-256-GCM). Either 32 raw bytes or 64 hex characters,
    /// for instance from `openssl rand -hex 32`.
    ///
    /// The key never enters the database and is never logged. Without it
    /// the control plane still runs, but every secret endpoint answers
    /// 503 instead of storing anything in the clear.
    #[serde(default)]
    pub secret_key_file: Option<PathBuf>,
    /// gRPC address of deis-store, as a node must dial it, for instance
    /// "http://127.0.0.1:8090".
    ///
    /// The control plane owns this address and hands it down with every
    /// deployment (docs/architecture.zh.md section 6); it is deliberately not
    /// a node-side setting, so moving deis-store does not mean editing every
    /// node's file. Leave it unset and applications that declare `kv`,
    /// `database` or `blob` load fine but every `ctx.services` call they make
    /// answers "service not available".
    ///
    /// It is an address, not a credential: it is logged like any other
    /// variable.
    #[serde(default)]
    pub store_endpoint: Option<String>,
    /// gRPC address of deis-queue, same contract as [`Self::store_endpoint`].
    /// Without it an app's `ctx.services.queue` fails and its declared queue
    /// consumers never start, so messages pile up in deis-queue unread.
    #[serde(default)]
    pub queue_endpoint: Option<String>,
    /// gRPC address of deis-telemetry's ingest/query and optional backend
    /// migration service. It is not handed to application code by Control;
    /// the migration coordinator uses it to inventory the same environment
    /// identity across logs, metrics and traces.
    #[serde(default)]
    pub telemetry_endpoint: Option<String>,
    /// Shared secret the storage capability tokens are derived from. It must
    /// be the identical string that deis-store and deis-queue carry as their
    /// own `auth_secret`.
    ///
    /// Unlike [`Self::store_endpoint`] this is a credential, so it is never
    /// logged and never travels to a node as itself: the control plane mints
    /// one token per tenant with
    /// [`deis_wire::conventions::storage_token`] and ships only that. A token
    /// authorises exactly the one tenant it was derived for, so a value that
    /// leaks out of one application is useless against another's data.
    ///
    /// Unset - the single-box default - means no token is minted and none is
    /// shipped. That is only safe while both storage services are bound to
    /// loopback and have no `auth_secret` of their own; they refuse to start
    /// on any other address without one. Setting it here while the services
    /// have no secret is harmless, and setting it on the services while it is
    /// unset here refuses every storage call every application makes.
    ///
    /// A blank or whitespace-only value counts as unset, so an empty string
    /// in the file cannot become an HMAC key that happens to compute.
    #[serde(default)]
    pub storage_auth_secret: Option<String>,
    /// Independent administrator credential used only when Control calls the
    /// deis-store backend-migration service. It is never derived from, equal
    /// to, or sent in place of an application storage capability.
    #[serde(default)]
    pub store_migration_admin_token: Option<String>,
    /// Independent administrator credential used only when Control calls the
    /// deis-queue backend-migration service. Keeping this separate from the
    /// store credential limits a compromise to one backend and permits their
    /// rotation and deployment lifecycles to differ.
    #[serde(default)]
    pub queue_migration_admin_token: Option<String>,
    /// Independent administrator credential for deis-telemetry migration
    /// management. It must differ from every storage, queue, node and human
    /// credential configured here.
    #[serde(default)]
    pub telemetry_migration_admin_token: Option<String>,
    /// Absolute path of the reviewed embedded-backend topology driver. When
    /// paired with `backend_restore_driver_config`, Control may execute or
    /// resume a durable Restore Operation. Arguments are fixed by Control;
    /// restore input travels over stdin and no shell is involved.
    #[serde(default)]
    pub backend_restore_driver_executable: Option<PathBuf>,
    /// Absolute path of the topology driver's own service/data-dir config.
    #[serde(default)]
    pub backend_restore_driver_config: Option<PathBuf>,
    /// Hard ceiling for one driver invocation. Timeout leaves the operation
    /// waiting for reconciliation rather than guessing whether data moved.
    #[serde(default = "default_backend_restore_driver_timeout_secs")]
    pub backend_restore_driver_timeout_secs: u64,
    /// Seconds between cron scheduler ticks (default: 60, clamped to 1..=60).
    ///
    /// A tick does two things: it rebuilds the task list from the catalog,
    /// and it fires whatever came due. Both are bounded by this, so the
    /// default means a newly deployed schedule can take up to a minute to be
    /// noticed and then fires within a minute of being due. Lower it when a
    /// deployment should start ticking promptly - a demo, a test - at the
    /// cost of one catalog read per tick.
    #[serde(default = "default_cron_tick_seconds")]
    pub cron_tick_seconds: u64,
    /// Seconds a self-registered node stays on the fleet list without
    /// refreshing its registration (default: 90).
    ///
    /// A node refreshes every third of this, so two lost refreshes - a dropped
    /// request, a restart of this process - do not cost a healthy node its
    /// place. When it does expire, the node stops receiving deployment pushes
    /// and cron fires; it is not told, and it comes back on its own the next
    /// time it registers.
    ///
    /// Nodes listed in `deis_runtime_nodes` are not subject to this: the file
    /// says they exist.
    #[serde(default = "default_node_registration_ttl")]
    pub node_registration_ttl_secs: u64,
}

/// Default lifetime of a node registration, in seconds.
///
/// Public because the fleet is built with it in places that have no
/// configuration to read - tests, and any caller that has no opinion.
pub fn default_node_registration_ttl() -> u64 {
    90
}

fn default_cron_tick_seconds() -> u64 {
    60
}

fn default_deis_runtime_poll_interval() -> u64 {
    15
}

fn default_bundle_dir() -> PathBuf {
    PathBuf::from("data/bundles")
}

fn default_backend_restore_driver_timeout_secs() -> u64 {
    900
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimeNodeConfig {
    /// Stable identifier used in API paths and logs
    pub id: String,
    /// gRPC endpoint of the deis-runtime management plane, e.g. "http://127.0.0.1:8086"
    pub addr: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TlsConfig {
    /// Enable TLS on the port this control plane serves, which carries the
    /// REST API and the node gRPC channel together.
    #[serde(default)]
    pub api_enabled: bool,
    /// Path to TLS certificate file (PEM format)
    pub cert_path: PathBuf,
    /// Path to TLS private key file (PEM format)
    pub key_path: PathBuf,
}

impl Default for ControlConfig {
    fn default() -> Self {
        Self {
            bind_addr: "0.0.0.0".to_string(),
            port: 8080,
            heartbeat_interval: 30,
            heartbeat_timeout: 90,
            node_preshared_keys: Vec::new(),
            api_keys: Vec::new(),
            api_auth_enabled: false,
            api_cors_allowed_origins: Vec::new(),
            tls: None,
            deis_runtime_nodes: Vec::new(),
            deis_runtime_poll_interval: default_deis_runtime_poll_interval(),
            bundle_dir: default_bundle_dir(),
            public_base_url: None,
            secret_key_file: None,
            store_endpoint: None,
            queue_endpoint: None,
            telemetry_endpoint: None,
            storage_auth_secret: None,
            store_migration_admin_token: None,
            queue_migration_admin_token: None,
            telemetry_migration_admin_token: None,
            backend_restore_driver_executable: None,
            backend_restore_driver_config: None,
            backend_restore_driver_timeout_secs: default_backend_restore_driver_timeout_secs(),
            cron_tick_seconds: default_cron_tick_seconds(),
            node_registration_ttl_secs: default_node_registration_ttl(),
        }
    }
}

impl std::fmt::Debug for ControlConfig {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ControlConfig")
            .field("bind_addr", &self.bind_addr)
            .field("port", &self.port)
            .field("heartbeat_interval", &self.heartbeat_interval)
            .field("heartbeat_timeout", &self.heartbeat_timeout)
            .field(
                "node_preshared_keys",
                &format_args!("<redacted:{} configured>", self.node_preshared_keys.len()),
            )
            .field(
                "api_keys",
                &format_args!("<redacted:{} configured>", self.api_keys.len()),
            )
            .field("api_auth_enabled", &self.api_auth_enabled)
            .field("api_cors_allowed_origins", &self.api_cors_allowed_origins)
            .field("tls", &self.tls)
            .field("deis_runtime_nodes", &self.deis_runtime_nodes)
            .field(
                "deis_runtime_poll_interval",
                &self.deis_runtime_poll_interval,
            )
            .field("bundle_dir", &self.bundle_dir)
            .field("public_base_url", &self.public_base_url)
            .field("secret_key_file", &self.secret_key_file)
            .field("store_endpoint", &self.store_endpoint)
            .field("queue_endpoint", &self.queue_endpoint)
            .field("telemetry_endpoint", &self.telemetry_endpoint)
            .field(
                "storage_auth_secret",
                &self.storage_auth_secret.as_ref().map(|_| "<redacted>"),
            )
            .field(
                "store_migration_admin_token",
                &self
                    .store_migration_admin_token
                    .as_ref()
                    .map(|_| "<redacted>"),
            )
            .field(
                "queue_migration_admin_token",
                &self
                    .queue_migration_admin_token
                    .as_ref()
                    .map(|_| "<redacted>"),
            )
            .field(
                "telemetry_migration_admin_token",
                &self
                    .telemetry_migration_admin_token
                    .as_ref()
                    .map(|_| "<redacted>"),
            )
            .field(
                "backend_restore_driver_executable",
                &self.backend_restore_driver_executable,
            )
            .field(
                "backend_restore_driver_config",
                &self.backend_restore_driver_config,
            )
            .field(
                "backend_restore_driver_timeout_secs",
                &self.backend_restore_driver_timeout_secs,
            )
            .field("cron_tick_seconds", &self.cron_tick_seconds)
            .field(
                "node_registration_ttl_secs",
                &self.node_registration_ttl_secs,
            )
            .finish()
    }
}

/// Where the node channel's credential came from, so startup can say it
/// out loud.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NodeCredentialSource {
    /// The configuration file named at least one key.
    Configured { keys: usize },
    /// The file named none, so one was minted for this run. The value is
    /// handed back so it can be printed once - it exists nowhere else.
    Generated(String),
}

/// Mint a node credential: 32 random bytes as hex, the same shape the
/// documented example keys have, so an operator can paste it into both the
/// control plane's file and every node's `[control] secret` unchanged.
fn generate_node_key() -> String {
    let bytes: [u8; 32] = rand::random();
    hex::encode(bytes)
}

impl ControlConfig {
    pub fn load(path: std::path::PathBuf) -> Result<Self, anyhow::Error> {
        let content = std::fs::read_to_string(path)?;
        Ok(toml::from_str(&content)?)
    }

    /// Guarantee this control plane has a node credential before it serves
    /// anything, and say where that credential came from.
    ///
    /// Blank entries are dropped first: a `""` left in the file is a typo,
    /// not a credential, and treating it as one would make the fleet's
    /// authentication a formality.
    ///
    /// If nothing is left, one key is generated for this run. That is
    /// deliberately not written to disk: the configuration file is where a
    /// node credential belongs, and a key quietly cached in a data directory
    /// would be a second source of truth that nobody edits and nobody
    /// rotates. Each restart therefore mints a new one until the file names
    /// one - loud, and self-correcting - while an unconfigured control plane
    /// is never open.
    pub fn ensure_node_credential(&mut self) -> NodeCredentialSource {
        self.node_preshared_keys
            .retain(|key| !key.trim().is_empty());

        if let Some(count) = std::num::NonZeroUsize::new(self.node_preshared_keys.len()) {
            info!(
                node_keys = count.get(),
                "Node credential taken from the configuration file; the node gRPC channel and the two HTTP endpoints that serve nodes accept only these keys"
            );
            return NodeCredentialSource::Configured { keys: count.get() };
        }

        let key = generate_node_key();
        self.node_preshared_keys.push(key.clone());
        warn!(
            "node_preshared_keys is empty, so a node credential was generated for this run only. Nothing can register, fetch assignments or download a bundle without it, and it changes on every restart until the configuration file names one"
        );
        NodeCredentialSource::Generated(key)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The one file operators are told to copy has to deserialize into
    /// this struct. A `[tls]` section missing `cert_path`, for instance,
    /// turns "copy and edit" into a control plane that refuses to start
    /// with a message about a missing field - and the example is exactly
    /// where nobody thinks to look.
    #[test]
    fn the_example_configuration_parses() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/deis-control.example.toml");
        let text = std::fs::read_to_string(path).expect("the example configuration must exist");
        let config: ControlConfig = toml::from_str(&text)
            .unwrap_or_else(|e| panic!("deis-control.example.toml does not parse: {e}"));

        assert!(
            !config.node_preshared_keys.is_empty(),
            "the example must show a node key: without one the assignment list serves no secret to any node"
        );
        assert!(config.api_auth_enabled);
        // The file people copy has to show the shape that survives being
        // started from another directory; a relative example is the reason
        // every uploaded version went missing on the first restart from
        // somewhere else.
        assert!(
            config.bundle_dir.is_absolute(),
            "the example must show an absolute bundle_dir, got {}",
            config.bundle_dir.display()
        );
    }

    /// The defect this exists to prevent: a control plane started with no
    /// configuration file used to run with an empty node key list, which
    /// left `PUT /api/nodes/{id}/registration` open to anyone who could
    /// reach the port. Now the default grows a credential before it serves.
    #[test]
    fn a_control_plane_with_no_configured_node_key_mints_one() {
        let mut config = ControlConfig::default();
        assert!(config.node_preshared_keys.is_empty());

        let source = config.ensure_node_credential();
        let NodeCredentialSource::Generated(key) = source else {
            panic!("an empty list must be reported as generated, not as configured");
        };
        assert_eq!(config.node_preshared_keys, vec![key.clone()]);
        assert_eq!(key.len(), 64, "32 random bytes as hex");
        assert!(key.chars().all(|c| c.is_ascii_hexdigit()));

        // Two runs do not share a credential, which is what makes the
        // generated one useless to anyone who saw an earlier log.
        let mut other = ControlConfig::default();
        let NodeCredentialSource::Generated(second) = other.ensure_node_credential() else {
            panic!("expected a generated key");
        };
        assert_ne!(key, second);
    }

    /// A blank entry is a typo, not a credential. Counting it would leave
    /// the fleet authenticating against the empty string.
    #[test]
    fn blank_entries_are_not_credentials() {
        let mut config = ControlConfig {
            node_preshared_keys: vec!["  ".to_string(), String::new()],
            ..Default::default()
        };
        assert!(matches!(
            config.ensure_node_credential(),
            NodeCredentialSource::Generated(_)
        ));
        assert_eq!(config.node_preshared_keys.len(), 1);
    }

    #[test]
    fn a_configured_key_is_left_alone() {
        let mut config = ControlConfig {
            node_preshared_keys: vec!["node-psk".to_string(), " ".to_string()],
            ..Default::default()
        };
        assert_eq!(
            config.ensure_node_credential(),
            NodeCredentialSource::Configured { keys: 1 }
        );
        assert_eq!(config.node_preshared_keys, vec!["node-psk".to_string()]);
    }
}
