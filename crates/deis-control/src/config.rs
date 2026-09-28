use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Every configuration structure in this file refuses a file that names a
/// field it does not know. An outdated key therefore stops this control plane
/// at startup with a message naming the key, instead of being dropped by the
/// deserializer without a word while the operator believes it took effect -
/// which is the worst answer available here, and the reason no field on these
/// structures carries special-cased refusal code any more.
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
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
    /// Directory holding this installation's own state: the SQLite database
    /// by default, the bundle directory, the one-time [`bootstrap`]
    /// credential written into it on a first start, and - in the layout
    /// ADR-012 documents - the master key under `keys/` and the machine
    /// channel's certificates under `machine/`.
    ///
    /// One directory, because a backup policy covers a directory: everything
    /// a restore needs belongs under this one, and the startup log names
    /// whatever an installation keeps somewhere else instead of leaving that
    /// to be discovered by a restore.
    ///
    /// Must be absolute, and a relative one is refused at startup rather than
    /// anchored: a relative value would be anchored to this process's working
    /// directory, which is right for the run that wrote it and wrong for the
    /// next one started from somewhere else - and what that produces is a
    /// control plane that comes up healthy on an empty database, and a bundle
    /// directory holding none of the versions it recorded.
    ///
    /// [`bootstrap`]: crate::identity::bootstrap
    #[serde(default = "default_data_dir")]
    pub data_dir: PathBuf,
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
    /// It used to be `*` - every origin, all methods, all headers - and that
    /// let any page the operator happened to visit create an owner account on
    /// their control plane, mint a root token, deploy, and read every answer.
    /// `*` is therefore rejected if it appears here: an origin that may
    /// read the audit log has to be named.
    ///
    /// Naming an origin does not hand it authority. A page on another origin
    /// presents a token like any other remote caller - there is no
    /// unauthenticated admission anywhere on the human API. See
    /// [`crate::api::authz::authorize`].
    #[serde(default)]
    pub api_cors_allowed_origins: Vec<String>,
    /// TLS configuration (optional)
    #[serde(default)]
    pub tls: Option<TlsConfig>,
    /// The machine channel's listener and identity.
    ///
    /// Absent means this control plane serves no machine channel at all: no
    /// node can register, fetch an assignment or download a bundle, and no
    /// proxy can fetch a routing table. That is the right refusal to make
    /// loudly at startup rather than to paper over, because the alternative -
    /// serving those answers to whoever asks - is what this section exists to
    /// end.
    #[serde(default)]
    pub machine: Option<MachineChannelConfig>,
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
    /// belongs on persistent storage that survives a restart - and on the
    /// same storage as the database, because a restore needs both and the
    /// documented backup policy covers `data_dir` (ADR-012).
    ///
    /// Unset, which is the default, means `<data_dir>/bundles`. An absolute
    /// path elsewhere is allowed and is then a second directory the backup
    /// has to cover explicitly; the startup log says so. A relative path is
    /// refused, for the same reason `data_dir` is.
    #[serde(default)]
    pub bundle_dir: Option<PathBuf>,
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
    /// Absolute, pre-created directory that holds verified provider driver
    /// releases. When unset or invalid, external provider execution is
    /// disabled; Control never falls back to PATH or an unregistered binary.
    /// On Unix it must be owned by the Control service identity and not be
    /// writable by group or others. On Windows its DACL must be protected,
    /// owned by the current service SID, and grant mutation rights only to
    /// that SID, LocalSystem, or built-in Administrators.
    #[serde(default)]
    pub provider_driver_installation_root: Option<PathBuf>,
    /// Absolute path to the out-of-band provider release trust root, including
    /// release, transparency-log and witness policy. External provider
    /// execution is enabled only when this and the installation root are both
    /// configured and valid. The file carries no private key and is subject
    /// to the same Unix ownership/mode or Windows service-DACL policy.
    #[serde(default)]
    pub provider_driver_trust_root: Option<PathBuf>,
    /// Optional absolute path to a DSSE envelope that authorizes the configured
    /// trust root as the exact successor of Control's durable current root.
    /// Required only while advancing the root; both the old and new root-role
    /// thresholds must sign it. The file carries no private key and is subject
    /// to the same Unix ownership/mode or Windows service-DACL policy.
    #[serde(default)]
    pub provider_driver_trust_root_rotation: Option<PathBuf>,
    /// Seconds between scans for durable provider deployments that can make
    /// progress (default: 2, clamped to 1..=300). A scan never bypasses the
    /// operation's owner/epoch lease.
    #[serde(default = "default_provider_deployment_dispatch_interval_secs")]
    pub provider_deployment_dispatch_interval_secs: u64,
    /// Minimum durable delay before retrying or reconciling a provider result
    /// that explicitly asked for another attempt (default: 30, clamped to the
    /// dispatch interval..=3600). Fresh work is not delayed.
    #[serde(default = "default_provider_deployment_retry_delay_secs")]
    pub provider_deployment_retry_delay_secs: u64,
    /// Maximum provider deployment children executed concurrently by one
    /// Control process (default: 4, clamped to 1..=64). Other Control
    /// processes remain fenced by the same durable child leases.
    #[serde(default = "default_provider_deployment_max_concurrency")]
    pub provider_deployment_max_concurrency: usize,
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
    /// Seconds a committed deployment waits for a target that has left the
    /// fleet before recording it as abandoned and letting the plan finish
    /// (default: 600).
    ///
    /// A plan freezes its target list when it is created and keeps every name
    /// in it, deliberately: a node that is merely quiet must still be pushed
    /// to when it answers again. But a plan whose pointer has already moved
    /// reaches its terminal state only when *every* frozen target has
    /// confirmed the exact generation, and a machine that never comes back
    /// cannot confirm anything. Without a deadline that plan stays
    /// non-terminal for ever, which holds the environment: the next deployment
    /// to it is refused with "a deployment plan is already in progress", and
    /// no operator action clears it.
    ///
    /// So the deadline exists, and it is only ever applied to a target that is
    /// **not on the fleet list at this moment**. A target that is listed and
    /// failing is a different thing and is waited for indefinitely, as before -
    /// that is a node with a problem, not a node that is gone.
    ///
    /// It must be comfortably longer than `node_registration_ttl_secs` plus
    /// however long a machine on this installation takes to reboot: a node
    /// that is restarting is off the list for the remainder of its
    /// registration lifetime plus its boot time, and abandoning it because it
    /// took ninety seconds too long would be this platform giving up on a
    /// machine that was coming back.
    ///
    /// Abandoning a push is not abandoning the deployment. A node asks the
    /// control plane what it should be running every time it starts, so a
    /// machine that returns after its plan has finished loads the current
    /// version from the assignment list on its own. What is lost by the
    /// deadline is the confirmation, not the delivery - which is why the
    /// target is recorded as `abandoned` rather than as active, and stays that
    /// way in the plan's observations for anyone asking later why a machine
    /// briefly ran something else.
    #[serde(default = "default_deployment_target_abandon_after_secs")]
    pub deployment_target_abandon_after_secs: u64,
}

/// Default deadline for a committed deployment's departed target.
///
/// Ten minutes: long enough for a machine to reboot and re-register several
/// times over, short enough that an environment is not held for an afternoon
/// by one machine that was decommissioned mid-deployment.
pub fn default_deployment_target_abandon_after_secs() -> u64 {
    600
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

fn default_provider_deployment_dispatch_interval_secs() -> u64 {
    2
}

fn default_provider_deployment_retry_delay_secs() -> u64 {
    30
}

fn default_provider_deployment_max_concurrency() -> usize {
    4
}

fn default_deis_runtime_poll_interval() -> u64 {
    15
}

/// Where an installation keeps its state when it says nothing else: the same
/// directory the example configuration and the systemd unit use.
///
/// Under `/var/lib` rather than `/srv` or `/opt` because this is state the
/// platform has to survive, not content an administrator put there - and
/// because it has to be one directory, so that one backup policy covers an
/// installation (ADR-012).
fn default_data_dir() -> PathBuf {
    PathBuf::from("/var/lib/deis-control")
}

fn default_backend_restore_driver_timeout_secs() -> u64 {
    900
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeNodeConfig {
    /// Stable identifier used in API paths and logs
    pub id: String,
    /// gRPC endpoint of the deis-runtime management plane, e.g. "http://127.0.0.1:8086"
    pub addr: String,
}

/// The listener Deislet's own processes talk to, and the identity this
/// control plane proves on it.
///
/// # Why it is a listener of its own
///
/// The machine channel answers with every application's decrypted secrets and
/// with a map of every hostname on the platform. Until this existed it shared
/// a socket with the dashboard and the human API, which meant those answers
/// were reachable from wherever an operator's browser was - guarded only by a
/// bearer secret every node in the fleet held a copy of. A separate listener
/// can be bound to an interface nodes reach and nothing else does, and it can
/// demand a certificate from its callers without a browser ever meeting that
/// demand.
///
/// # The three files
///
/// See `deis_identity`: this control plane's own certificate chain and key,
/// and the trust root every node and proxy must chain to. Nothing here issues
/// or renews them, and the identity is re-read on a timer so a rotation needs
/// no restart.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MachineChannelConfig {
    /// Interface to listen on. Deliberately not defaulted to `0.0.0.0`: the
    /// point of this listener is that an operator says where it is reachable
    /// from.
    pub bind_addr: String,
    pub port: u16,
    /// This control plane's certificate chain, end-entity certificate first.
    pub certificate: PathBuf,
    /// The private key belonging to that certificate.
    pub private_key: PathBuf,
    /// What every peer's certificate must chain to.
    pub trust_root: PathBuf,
    /// How often the three files above are re-read, in seconds.
    ///
    /// The longest a renewal can go unnoticed. It wants to be far shorter
    /// than the shortest certificate lifetime in use, and that is the only
    /// thing it trades against.
    #[serde(default = "default_identity_reload_seconds")]
    pub identity_reload_seconds: u64,
    /// The address nodes reach this listener at, used to build the bundle
    /// URLs an assignment carries.
    ///
    /// Needed because the bundle download is served here rather than on the
    /// human listener, so `public_base_url` - which names where a browser
    /// finds the dashboard - is the wrong address to hand a node.
    #[serde(default)]
    pub public_base_url: Option<String>,
}

fn default_identity_reload_seconds() -> u64 {
    60
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
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
            data_dir: default_data_dir(),
            api_cors_allowed_origins: Vec::new(),
            tls: None,
            machine: None,
            deis_runtime_nodes: Vec::new(),
            deis_runtime_poll_interval: default_deis_runtime_poll_interval(),
            bundle_dir: None,
            public_base_url: None,
            secret_key_file: None,
            store_endpoint: None,
            queue_endpoint: None,
            backend_restore_driver_executable: None,
            backend_restore_driver_config: None,
            backend_restore_driver_timeout_secs: default_backend_restore_driver_timeout_secs(),
            provider_driver_installation_root: None,
            provider_driver_trust_root: None,
            provider_driver_trust_root_rotation: None,
            provider_deployment_dispatch_interval_secs:
                default_provider_deployment_dispatch_interval_secs(),
            provider_deployment_retry_delay_secs: default_provider_deployment_retry_delay_secs(),
            provider_deployment_max_concurrency: default_provider_deployment_max_concurrency(),
            cron_tick_seconds: default_cron_tick_seconds(),
            node_registration_ttl_secs: default_node_registration_ttl(),
            deployment_target_abandon_after_secs: default_deployment_target_abandon_after_secs(),
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
            .field("data_dir", &self.data_dir)
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
            .field(
                "provider_driver_installation_root",
                &self.provider_driver_installation_root,
            )
            .field(
                "provider_driver_trust_root",
                &self.provider_driver_trust_root,
            )
            .field(
                "provider_driver_trust_root_rotation",
                &self.provider_driver_trust_root_rotation,
            )
            .field(
                "provider_deployment_dispatch_interval_secs",
                &self.provider_deployment_dispatch_interval_secs,
            )
            .field(
                "provider_deployment_retry_delay_secs",
                &self.provider_deployment_retry_delay_secs,
            )
            .field(
                "provider_deployment_max_concurrency",
                &self.provider_deployment_max_concurrency,
            )
            .field("cron_tick_seconds", &self.cron_tick_seconds)
            .field(
                "node_registration_ttl_secs",
                &self.node_registration_ttl_secs,
            )
            .field(
                "deployment_target_abandon_after_secs",
                &self.deployment_target_abandon_after_secs,
            )
            .finish()
    }
}

impl ControlConfig {
    pub fn load(path: std::path::PathBuf) -> Result<Self, anyhow::Error> {
        let content = std::fs::read_to_string(path)?;
        Ok(toml::from_str(&content)?)
    }

    /// Directory holding uploaded application bundles, resolved against
    /// `data_dir` when the operator named none.
    pub fn bundle_dir(&self) -> PathBuf {
        self.bundle_dir
            .clone()
            .unwrap_or_else(|| self.data_dir.join("bundles"))
    }

    /// Refuse a path this installation writes to that is not absolute.
    ///
    /// Every one of them names state the next process has to find again -
    /// after a restart, an upgrade, a host rebuild - and a relative path
    /// names a different file to each of them, according to where it was
    /// started. The failure is quiet rather than loud: a control plane that
    /// comes up healthy on a database it just created, and a bundle
    /// directory whose versions are all "recorded but missing on disk".
    ///
    /// `database` is an argument because the flag that carries it belongs to
    /// the binary, not to this file.
    pub fn refuse_relative_paths(&self, database: &Path) -> Result<(), anyhow::Error> {
        let relative: Vec<String> = [
            ("data_dir", self.data_dir.clone()),
            ("bundle_dir", self.bundle_dir()),
            ("the --database path", database.to_path_buf()),
        ]
        .iter()
        .filter(|(_, path)| !path.is_absolute())
        .map(|(name, path)| format!("{name} {}", path.display()))
        .collect();

        if !relative.is_empty() {
            anyhow::bail!(
                "these paths are relative and every one of them has to be absolute: {}. A \
                 relative path names a different file to every process started from a different \
                 working directory - the next start of this control plane would serve a database \
                 it created empty and lose every uploaded version",
                relative.join(", ")
            );
        }
        Ok(())
    }

    /// Every file and directory a restore needs, each paired with the name a
    /// backup manifest should use for it, in the order ADR-012 lists them.
    ///
    /// The database comes first because its absence is the loudest, and the
    /// whole list is what has to be captured together: a restore that has
    /// the database and not the bundles answers `/health` with `OK`, lists
    /// every application, and cannot run one of them.
    ///
    /// The one-time bootstrap credential is deliberately not here. It is a
    /// way in rather than state, it expires 24 hours after it was minted
    /// unused, and a restored installation mints a fresh one on its own.
    ///
    /// `database` is an argument for the same reason as in
    /// [`Self::refuse_relative_paths`].
    pub fn backup_material(&self, database: &Path) -> Vec<(&'static str, PathBuf)> {
        let mut material = vec![
            ("control database", database.to_path_buf()),
            ("uploaded bundles", self.bundle_dir()),
        ];
        if let Some(path) = self.secret_key_file.as_ref() {
            material.push(("secret master key", path.clone()));
        }
        if let Some(machine) = self.machine.as_ref() {
            material.push(("machine channel certificate", machine.certificate.clone()));
            material.push(("machine channel private key", machine.private_key.clone()));
            material.push(("fleet trust root", machine.trust_root.clone()));
        }
        material
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The four fields this structure has no default for: every file has to
    /// name them, so every file below starts with them.
    const REQUIRED: &str =
        "bind_addr = '127.0.0.1'\nport = 8080\nheartbeat_interval = 30\nheartbeat_timeout = 90\n";

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

        // The file people copy has to show the shape that survives being
        // started from another directory; a relative example is the reason
        // every uploaded version went missing on the first restart from
        // somewhere else.
        assert!(
            config.bundle_dir().is_absolute(),
            "the example must show an absolute bundle_dir, got {}",
            config.bundle_dir().display()
        );
        assert!(
            config.data_dir.is_absolute(),
            "the example must show an absolute data_dir, got {}",
            config.data_dir.display()
        );
    }

    /// A field this build does not know is refused with a message naming it,
    /// never dropped silently: an operator whose file still carries a setting
    /// that has been removed has to be told at startup, not left believing it
    /// took effect. This covers retired settings generally - there is no
    /// per-field code for any of them.
    #[test]
    fn an_unknown_field_is_refused_and_named() {
        for removed in [
            "api_keys = [\"whatever\"]",
            "api_auth_enabled = true",
            "node_preshared_keys = [\"whatever\"]",
            "no_such_setting = 1",
        ] {
            let refusal: Result<ControlConfig, _> =
                toml::from_str(&format!("port = 8080\n{removed}\n"));
            let err = refusal.expect_err("an unknown field must be refused");
            let said = err.to_string();
            assert!(
                said.contains("unknown field"),
                "the refusal has to say the field is unknown: {said}"
            );
            assert!(
                said.contains(removed.split('=').next().unwrap().trim()),
                "the refusal has to name the field: {said}"
            );
        }
    }

    /// A nested section is held to the same rule as the top level: an
    /// unknown key inside `[machine]` or `[tls]` is refused too, not just an
    /// unknown top-level key.
    #[test]
    fn an_unknown_field_in_a_nested_section_is_refused() {
        let refusal: Result<ControlConfig, _> = toml::from_str(
            "[tls]\napi_enabled = true\ncert_path = '/x'\nkey_path = '/y'\nno_such_key = 1\n",
        );
        assert!(
            refusal.is_err(),
            "an unknown field in [tls] must be refused"
        );
    }

    /// Out of the box, everything a restore needs is under one absolute
    /// directory, because a backup policy covers a directory and this is the
    /// policy an installation is handed: one path to capture, and the same
    /// path to restore into. A relative default was the version of this that
    /// only worked from the directory it happened to start in.
    #[test]
    fn the_defaults_are_one_absolute_directory() {
        let config = ControlConfig::default();

        assert_eq!(config.data_dir, PathBuf::from("/var/lib/deis-control"));
        assert_eq!(
            config.bundle_dir(),
            PathBuf::from("/var/lib/deis-control/bundles")
        );
        config
            .refuse_relative_paths(Path::new("/var/lib/deis-control/control.db"))
            .expect("the built-in defaults are absolute");
    }

    /// A data directory of the operator's own carries the bundle directory
    /// with it, so naming one directory keeps the database, the bundles and
    /// the key material in one place.
    #[test]
    fn a_data_directory_carries_the_bundle_directory_with_it() {
        let config: ControlConfig = toml::from_str(&format!("{REQUIRED}data_dir = '/srv/deis'\n"))
            .expect("data_dir alone is a valid file");

        assert_eq!(config.bundle_dir(), PathBuf::from("/srv/deis/bundles"));
    }

    /// And an operator who wants the bundles somewhere else still gets what
    /// they asked for, on the filesystem that suits them.
    #[test]
    fn an_explicit_bundle_directory_is_honoured() {
        let config: ControlConfig = toml::from_str(&format!(
            "{REQUIRED}data_dir = '/srv/deis'\nbundle_dir = '/mnt/bundles'\n"
        ))
        .expect("both paths may be named");

        assert_eq!(config.bundle_dir(), PathBuf::from("/mnt/bundles"));
    }

    /// A relative path is refused rather than anchored, and the refusal
    /// names every one of them: an operator who fixes only the one that was
    /// reported first would be back a restart later.
    #[test]
    fn a_relative_storage_path_is_refused_by_name() {
        let config: ControlConfig = toml::from_str(&format!("{REQUIRED}data_dir = 'data'\n"))
            .expect("the file itself parses");
        let refusal = config
            .refuse_relative_paths(Path::new("/srv/deis/control.db"))
            .expect_err("a relative data directory must be refused");
        let said = refusal.to_string();
        assert!(
            said.contains("data_dir data"),
            "the refusal names it: {said}"
        );

        let config = ControlConfig::default();
        let refusal = config
            .refuse_relative_paths(Path::new("deis-control.db"))
            .expect_err("a relative --database must be refused");
        let said = refusal.to_string();
        assert!(
            said.contains("--database path deis-control.db"),
            "the refusal names the flag: {said}"
        );
        assert!(
            !said.contains("data_dir"),
            "and only what is actually relative: {said}"
        );
    }

    /// The list a backup is taken against, and the reason it is a method on
    /// the configuration rather than a paragraph in a document: a restore
    /// missing any one of these comes up healthy and useless.
    #[test]
    fn the_restore_set_names_the_database_bundles_key_and_machine_material() {
        let config: ControlConfig = toml::from_str(&format!(
            "{REQUIRED}data_dir = '/srv/deis'\n\
             secret_key_file = '/srv/deis/keys/master.key'\n\
             [machine]\n\
             bind_addr = '127.0.0.1'\n\
             port = 8443\n\
             certificate = '/srv/deis/machine/control.crt.pem'\n\
             private_key = '/srv/deis/machine/control.key.pem'\n\
             trust_root = '/srv/deis/machine/fleet-ca.crt.pem'\n"
        ))
        .expect("a full installation parses");

        let material = config.backup_material(Path::new("/srv/deis/control.db"));
        assert_eq!(
            material,
            vec![
                ("control database", PathBuf::from("/srv/deis/control.db")),
                ("uploaded bundles", PathBuf::from("/srv/deis/bundles")),
                (
                    "secret master key",
                    PathBuf::from("/srv/deis/keys/master.key")
                ),
                (
                    "machine channel certificate",
                    PathBuf::from("/srv/deis/machine/control.crt.pem")
                ),
                (
                    "machine channel private key",
                    PathBuf::from("/srv/deis/machine/control.key.pem")
                ),
                (
                    "fleet trust root",
                    PathBuf::from("/srv/deis/machine/fleet-ca.crt.pem")
                ),
            ]
        );
    }

    /// An installation that has configured neither a master key nor a
    /// machine channel still has a restore set - and it is exactly the two
    /// things the database cannot be served without.
    #[test]
    fn the_restore_set_is_short_when_nothing_else_is_configured() {
        let material =
            ControlConfig::default().backup_material(Path::new("/var/lib/deis-control/control.db"));

        assert_eq!(
            material.iter().map(|(name, _)| *name).collect::<Vec<_>>(),
            vec!["control database", "uploaded bundles"]
        );
    }
}
