// Application manager

use super::key::AppKey;
use super::types::{App, AppConfig, AppState, ResourceLimits};
use crate::app_repo::AppRepo;
use crate::assets::AssetBundle;
use crate::config::ExecutionLimits;
use crate::runtime::{FetchPolicy, IsolateSpec, OpMetricsSummary, RuntimePool};
use anyhow::{anyhow, Result};
use dashmap::DashMap;
use std::sync::Arc;
use std::time::SystemTime;

/// One deployed version of one app, as this node currently runs it.
///
/// Held next to the isolate rather than inside it for two reasons: the HTTP
/// entry needs the asset index on every request and must not reopen the zip
/// for it, and a version that fails to load needs something to fall back to.
pub struct VersionRecord {
    pub version_id: String,
    /// Whether the `env` this copy is running with came from the control
    /// plane.
    ///
    /// False means the version was loaded from this node's own disk, where
    /// secret *values* are deliberately never written: the code is the right
    /// code but every secret the app declares is missing. Two copies of one
    /// version are therefore not interchangeable, and this is what stops a
    /// later delivery of the same version id from being waved through as
    /// "already running" - see `DeploymentManager::apply_from_assignment`.
    pub env_confirmed: bool,
    pub entry_point: String,
    /// Bundled module source. Kept because an isolate is built from it, and
    /// a version change builds a second one while this record is still the
    /// one serving; without it the code would have to be fetched again to
    /// stage a replacement.
    code: String,
    config: AppConfig,
    pub assets: Arc<AssetBundle>,
}

impl std::fmt::Debug for VersionRecord {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VersionRecord")
            .field("version_id", &self.version_id)
            .field("env_confirmed", &self.env_confirmed)
            .field("entry_point", &self.entry_point)
            .field("assets", &self.assets)
            .finish()
    }
}

/// Everything needed to put one version of one app on this node.
pub struct VersionInstall {
    /// Which application *and which environment of it*: the two are one key,
    /// so a preview cannot land in production's slot.
    pub key: AppKey,
    pub version_id: String,
    pub entry_point: String,
    pub code: String,
    /// Carries the decrypted `env`; see [`AppConfig`] for why it neither
    /// serialises nor prints its values.
    pub config: AppConfig,
    /// See [`VersionRecord::env_confirmed`]. False for a version recovered
    /// from local disk.
    pub env_confirmed: bool,
    pub assets: Arc<AssetBundle>,
}

/// What the node is running for one (app, environment) pair.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstalledVersion {
    pub version_id: String,
    /// See [`VersionRecord::env_confirmed`].
    pub env_confirmed: bool,
}

/// Why a scheduled run did not complete.
///
/// The two cases look the same from the outside and mean opposite things to
/// whoever fired the tick, which is why they are separate variants rather than
/// one error string: one asks for another node, the other asks for no node at
/// all. See `crate::trigger`.
#[derive(Debug)]
pub enum ScheduledFailure {
    /// This node never entered the application's code: it does not have this
    /// version of this environment loaded. Another node can still run the
    /// tick, and nothing has happened here that a second attempt would repeat.
    NotRunnableHere(String),
    /// The application's own scheduled handler was entered and did not
    /// finish - it threw, it timed out, or the bundle exports no scheduled
    /// handler at all. The same code on another node fails the same way, and
    /// whatever the handler did before it failed (a queue send, a write) has
    /// already happened once.
    HandlerFailed(String),
}

impl std::fmt::Display for ScheduledFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotRunnableHere(m) | Self::HandlerFailed(m) => write!(f, "{m}"),
        }
    }
}

impl std::error::Error for ScheduledFailure {}

/// Application manager
pub struct ApplicationManager {
    /// Loaded applications, keyed by (app, environment). Both maps are keyed
    /// the same way on purpose: an application whose preview and production
    /// shared one entry would have one of them silently overwrite the other's
    /// version, bindings and secrets.
    apps: DashMap<AppKey, App>,
    /// Deployed versions. Only apps installed from a bundle appear here; the
    /// built-in `_system` app has no version.
    versions: DashMap<AppKey, Arc<VersionRecord>>,
    runtime_pool: Arc<RuntimePool>,
    /// Debug sessions: (app, environment) -> (token, expires_at)
    inspect_sessions: DashMap<AppKey, (String, SystemTime)>,
    // Platform-wide defaults for per-app execution limits (timeout, rate limit).
    // An app's own `ResourceLimits` may override either value; see
    // `effective_limits`.
    default_limits: ExecutionLimits,
    /// V8 heap ceiling (MB) every isolate on this node runs with unless its
    /// app asked for less. Not part of `default_limits` because
    /// `ExecutionLimits` bounds one invocation and is re-armed per request,
    /// while a heap belongs to the isolate and outlives every request in it.
    default_memory_limit_mb: usize,
    /// Where every isolate on this node may send an outbound `fetch()`. One
    /// policy for the node, because it is the node's network that is being
    /// protected, not any one application's. The default refuses everything
    /// that is not a public internet address.
    fetch_policy: FetchPolicy,
}

impl ApplicationManager {
    /// Create a new application manager
    pub fn new() -> Self {
        Self::with_threads(None)
    }

    pub fn new_with_repo(app_repo: Arc<AppRepo>) -> Self {
        Self::with_threads_and_repo(None, app_repo)
    }

    /// Create a new application manager (with specified thread count)
    pub fn with_threads(num_threads: Option<usize>) -> Self {
        Self {
            apps: DashMap::new(),
            versions: DashMap::new(),
            runtime_pool: Arc::new(RuntimePool::new(num_threads)),
            inspect_sessions: DashMap::new(),
            default_limits: ExecutionLimits::default(),
            default_memory_limit_mb: crate::config::default_memory_limit_mb(),
            fetch_policy: FetchPolicy::default(),
        }
    }

    pub fn with_threads_and_repo(num_threads: Option<usize>, app_repo: Arc<AppRepo>) -> Self {
        Self {
            apps: DashMap::new(),
            versions: DashMap::new(),
            runtime_pool: Arc::new(RuntimePool::new_with_repo(num_threads, Some(app_repo))),
            inspect_sessions: DashMap::new(),
            default_limits: ExecutionLimits::default(),
            default_memory_limit_mb: crate::config::default_memory_limit_mb(),
            fetch_policy: FetchPolicy::default(),
        }
    }

    /// Override the platform-wide default execution limits (timeout, rate
    /// limit) used for apps that do not configure their own. Intended to be
    /// called once at startup with the values loaded from `RuntimeConfig`.
    pub fn with_limits(mut self, default_limits: ExecutionLimits) -> Self {
        // Said here because this is the one call that carries the operator's
        // values, and because the CPU ceiling is the newest of them: an
        // operator reading a node's startup output has to be able to see the
        // number that is in force without deploying anything to find out.
        tracing::info!(
            timeout_ms = default_limits.timeout_ms,
            wait_until_timeout_ms = default_limits.wait_until_timeout_ms,
            cpu_time_limit_ms = default_limits.cpu_time_limit_ms,
            rate_limit_ops_per_sec = default_limits.rate_limit_ops_per_sec,
            "Node execution ceilings: {}ms of wall clock and {}ms of CPU time per invocation, \
             {}ms for a waitUntil tail, {} ops/sec of op dispatch. An application may narrow the \
             CPU ceiling but not widen it",
            default_limits.timeout_ms,
            default_limits.cpu_time_limit_ms,
            default_limits.wait_until_timeout_ms,
            default_limits.rate_limit_ops_per_sec
        );
        self.default_limits = default_limits;
        self
    }

    /// Override the platform-wide V8 heap ceiling every isolate on this node
    /// runs with. Intended to be called once at startup with
    /// `RuntimeConfig::default_memory_limit_mb`, which validation has already
    /// held to a floor.
    pub fn with_memory_limit_mb(mut self, default_memory_limit_mb: usize) -> Self {
        self.default_memory_limit_mb = default_memory_limit_mb;
        self
    }

    /// Set the node's outbound-fetch policy. Intended to be called once at
    /// startup with the operator's `[fetch] allow` list and the addresses of
    /// this node's own platform services.
    ///
    /// Leaving it unset is not a hole: the default policy is the strictest
    /// one, refusing every destination that is not a public address.
    pub fn with_fetch_policy(mut self, policy: FetchPolicy) -> Self {
        self.fetch_policy = policy;
        self
    }

    /// Resolve the effective execution limits for an app: this node's
    /// ceilings, narrowed by whatever the application asked to be held to.
    ///
    /// **Every limit narrows only.** The values in a `ResourceLimits` arrive
    /// from an application's own bundle manifest, and a manifest is written by
    /// the tenant, not by the operator. Taking one at face value would let any
    /// application on a shared node raise its own ceiling - an hour of
    /// wall-clock time per request, ten thousand ops a second - simply by
    /// declaring it. So the node's configuration is the ceiling on every
    /// field, and an application may only ask for less of it.
    ///
    /// Asking for more is not an error. It is a `warn` naming the application,
    /// the value it asked for and the value it got, and then the node's value
    /// is used: refusing the deployment outright would take an application off
    /// the air over a number that is merely optimistic, and silently applying
    /// the node's value would leave an operator reading a manifest that says
    /// something the platform never did.
    fn effective_limits(&self, key: &AppKey, limits: Option<&ResourceLimits>) -> ExecutionLimits {
        let timeout_ms = self.effective_timeout_ms(key, limits);
        let rate_limit_ops_per_sec = self.effective_rate_limit(key, limits);
        let cpu_time_limit_ms = self.effective_cpu_time_limit_ms(key, limits);

        // A thread cannot burn more CPU time than wall-clock time, so a CPU
        // ceiling at or above the duration ceiling can never be the one that
        // fires. That is not dangerous -- the node is protected by the
        // stricter of the two -- but an operator who set a CPU ceiling and
        // never sees it in the logs deserves to be told why, rather than
        // concluding it works and nothing ever trips it.
        if cpu_time_limit_ms >= timeout_ms {
            tracing::warn!(
                target: "app",
                app_id = %key.app_id(),
                environment = %key.environment(),
                cpu_time_limit_ms,
                timeout_ms,
                "App {} has a {}ms CPU ceiling and a {}ms duration ceiling; one thread cannot \
                 spend more CPU time than wall-clock time, so the CPU ceiling can never fire for \
                 it and the duration limit is the only one in effect. Lower \
                 default_execution_limits.cpu_time_limit_ms below the timeout for it to mean \
                 anything",
                key,
                cpu_time_limit_ms,
                timeout_ms
            );
        }

        ExecutionLimits {
            timeout_ms,
            // No per-app override: `ResourceLimits` has no key for the tail
            // budget, so every app on this node is held to the platform value.
            wait_until_timeout_ms: self.default_limits.wait_until_timeout_ms,
            // Nor for the stream budget, and for the same reason. An open
            // stream costs the node a queue and a producer for as long as it
            // lives, so how long that may be is the operator's number, not the
            // tenant's; an application that needs longer belongs on a node
            // whose operator set `stream_timeout_ms` higher.
            stream_timeout_ms: self.default_limits.stream_timeout_ms,
            rate_limit_ops_per_sec,
            cpu_time_limit_ms,
        }
    }

    /// Resolve the wall-clock ceiling for one invocation of one app: the
    /// node's default, narrowed by the app's own `ResourceLimits.timeout_ms`
    /// when it asked for less.
    ///
    /// Narrowing only, like every other limit here. A longer timeout is not
    /// only the client's patience: an invocation holds the runtime thread it
    /// is on for its whole duration, and every other application queued behind
    /// that thread waits with it. An application that could set its own
    /// timeout to an hour could park a thread for an hour, so this is the
    /// node's number and an application may only ask for less of it.
    ///
    /// An application wanting a *longer* deadline than the node allows has a
    /// placement question, not a configuration one: it belongs on a node whose
    /// operator set `default_execution_limits.timeout_ms` that high.
    fn effective_timeout_ms(&self, key: &AppKey, limits: Option<&ResourceLimits>) -> u64 {
        let node_default = self.default_limits.timeout_ms;
        let Some(requested) = limits.and_then(|l| l.timeout_ms) else {
            return node_default;
        };

        if requested == 0 {
            // Zero is what the wire uses for "unset", so it should never
            // survive as a value here. If it does, it came from something that
            // hand-built an assignment, and applying it would cut every
            // invocation off before it ran.
            tracing::warn!(
                target: "app",
                app_id = %key.app_id(),
                environment = %key.environment(),
                applied_ms = node_default,
                "App {} asks for a 0ms execution timeout, which would fail every request before \
                 it started; it is held to this node's {}ms default instead",
                key,
                node_default
            );
            return node_default;
        }

        if requested <= node_default {
            tracing::info!(
                target: "app",
                app_id = %key.app_id(),
                environment = %key.environment(),
                "App {} narrows the default execution timeout: {}ms (platform default {}ms)",
                key,
                requested,
                node_default
            );
            return requested;
        }

        tracing::warn!(
            target: "app",
            app_id = %key.app_id(),
            environment = %key.environment(),
            requested_ms = requested,
            applied_ms = node_default,
            "App {} asks for a {}ms execution timeout, above this node's {}ms ceiling; it is \
             held to the node's value. An invocation holds a runtime thread for its whole \
             duration, so this ceiling is the node's to set: raise \
             default_execution_limits.timeout_ms if this node is meant to carry requests that \
             long",
            key,
            requested,
            node_default
        );
        node_default
    }

    /// Resolve the op-dispatch rate ceiling for one app: the node's default,
    /// narrowed by the app's own `ResourceLimits.rate_limit_ops_per_sec` when
    /// it asked for less.
    ///
    /// Narrowing only. Every op dispatch is work this node does on the
    /// application's behalf, so the rate at which one tenant may ask for it is
    /// the node's number; an application raising its own would be helping
    /// itself to a bigger share of the box.
    fn effective_rate_limit(&self, key: &AppKey, limits: Option<&ResourceLimits>) -> f64 {
        let node_default = self.default_limits.rate_limit_ops_per_sec;
        let Some(requested) = limits.and_then(|l| l.rate_limit_ops_per_sec) else {
            return node_default;
        };

        // NaN compares false against everything, so it would slip past both
        // branches below and be installed as a ceiling nothing can cross.
        if !requested.is_finite() || requested <= 0.0 {
            tracing::warn!(
                target: "app",
                app_id = %key.app_id(),
                environment = %key.environment(),
                requested_ops_per_sec = requested,
                applied_ops_per_sec = node_default,
                "App {} asks for a rate limit of {} ops/sec, which is not a usable rate; it is \
                 held to this node's {} ops/sec default instead",
                key,
                requested,
                node_default
            );
            return node_default;
        }

        if requested <= node_default {
            tracing::info!(
                target: "app",
                app_id = %key.app_id(),
                environment = %key.environment(),
                "App {} narrows the default op rate limit: {} ops/sec (platform default {} \
                 ops/sec)",
                key,
                requested,
                node_default
            );
            return requested;
        }

        tracing::warn!(
            target: "app",
            app_id = %key.app_id(),
            environment = %key.environment(),
            requested_ops_per_sec = requested,
            applied_ops_per_sec = node_default,
            "App {} asks for {} ops/sec of op dispatch, above this node's {} ops/sec ceiling; it \
             is held to the node's value. Raise \
             default_execution_limits.rate_limit_ops_per_sec if this node is meant to carry \
             applications that chatty",
            key,
            requested,
            node_default
        );
        node_default
    }

    /// Resolve the CPU-time ceiling for one app: the node's default, narrowed
    /// by the app's own `ResourceLimits.cpu_time_limit_ms` when it asked for
    /// less.
    ///
    /// Narrowing only, like every other limit here. CPU time is a core taken
    /// from every other tenant on this node, so an app may hold itself to less
    /// and may not help itself to more.
    fn effective_cpu_time_limit_ms(&self, key: &AppKey, limits: Option<&ResourceLimits>) -> u64 {
        let node_default = self.default_limits.cpu_time_limit_ms;
        let Some(requested) = limits.and_then(|l| l.cpu_time_limit_ms) else {
            return node_default;
        };

        if requested == 0 {
            // Same sentinel the wire uses: an absent field and a zero are one
            // and the same to every producer of an `ApplyDeployment`, and a
            // ceiling of zero would cut off every invocation before it ran an
            // instruction.
            tracing::warn!(
                target: "app",
                app_id = %key.app_id(),
                environment = %key.environment(),
                applied_ms = node_default,
                "App {} asks for a 0ms CPU ceiling, which would terminate every invocation before \
                 it ran; it is held to this node's {}ms default instead",
                key,
                node_default
            );
            return node_default;
        }

        if requested <= node_default {
            tracing::info!(
                target: "app",
                app_id = %key.app_id(),
                environment = %key.environment(),
                "App {} narrows the default CPU ceiling: {}ms (platform default {}ms)",
                key,
                requested,
                node_default
            );
            return requested;
        }

        tracing::warn!(
            target: "app",
            app_id = %key.app_id(),
            environment = %key.environment(),
            requested_ms = requested,
            applied_ms = node_default,
            "App {} asks for a {}ms CPU ceiling, above this node's {}ms ceiling; it is held to \
             the node's value. Raise default_execution_limits.cpu_time_limit_ms if this node is \
             meant to carry applications that compute that long",
            key,
            requested,
            node_default
        );
        node_default
    }

    /// Resolve the V8 heap ceiling for one app: the node's default, narrowed
    /// by the app's own `ResourceLimits.memory_mb` when it asked for less.
    ///
    /// Always a value, never `None`. An isolate built with no ceiling runs on
    /// whatever V8 defaults to, which on a 64-bit host is large enough that an
    /// app allocating in a loop takes the node process down and every other
    /// tenant's isolate with it. The node default is therefore the floor, and
    /// the only direction an app may move it is down.
    fn effective_memory_limit_mb(&self, key: &AppKey, limits: Option<&ResourceLimits>) -> usize {
        // 0 is "unset", not "no memory at all"; see `ResourceLimits::memory_mb`.
        let requested = limits.map(|l| l.memory_mb).filter(|mb| *mb > 0);
        match requested {
            None => self.default_memory_limit_mb,
            Some(mb) if mb <= self.default_memory_limit_mb => {
                tracing::info!(
                    target: "app",
                    app_id = %key.app_id(),
                    environment = %key.environment(),
                    "App {} narrows the default heap ceiling: {}MB (platform default {}MB)",
                    key,
                    mb,
                    self.default_memory_limit_mb
                );
                mb
            }
            Some(mb) => {
                tracing::warn!(
                    target: "app",
                    app_id = %key.app_id(),
                    environment = %key.environment(),
                    requested_mb = mb,
                    applied_mb = self.default_memory_limit_mb,
                    "App {} asks for a {}MB heap, above this node's {}MB ceiling; it is held to \
                     the node's value. Raise default_memory_limit_mb if this node is meant to \
                     carry apps that large",
                    key,
                    mb,
                    self.default_memory_limit_mb
                );
                self.default_memory_limit_mb
            }
        }
    }

    /// Load application
    pub async fn load_app(
        &self,
        key: AppKey,
        code: String,
        config: AppConfig,
        entry_point: Option<String>,
    ) -> Result<()> {
        // Check if application already exists
        if self.apps.contains_key(&key) {
            return Err(anyhow!("App {} already exists", key));
        }

        tracing::info!("Loading app: {}", key);

        // Create application information
        let now = SystemTime::now();
        let mut app_info = App {
            id: key.app_id().to_string(),
            environment: key.environment().to_string(),
            state: AppState::Loading,
            config: config.clone(),
            created_at: now,
            updated_at: now,
            error: None,
        };

        // Load application in runtime pool
        let spec = IsolateSpec {
            entry_point,
            env: config.env.clone(),
            permissions: config.permissions.clone(),
            memory_limit_mb: Some(self.effective_memory_limit_mb(&key, config.limits.as_ref())),
            resource_table_max: config.limits.as_ref().and_then(|l| l.resource_table_max),
            route_patterns: config.routes.clone(),
            limits: self.effective_limits(&key, config.limits.as_ref()),
            fetch_policy: self.fetch_policy.clone(),
            // This path loads code without a bundle -- the node's own system
            // app, and tests -- so there is nothing for `env.ASSETS` to serve.
            assets: None,
            // Whatever the caller put on the config, which for the system app
            // and for tests is the default: the oldest behaviour. Code that
            // came from no bundle declared no date, and answering as though it
            // had would be inventing a claim on its behalf.
            compatibility: config.compatibility,
        };
        match self
            .runtime_pool
            .load_app(key.tenant_id(), code, spec)
            .await
        {
            Ok(()) => {
                app_info.state = AppState::Running;
                app_info.updated_at = SystemTime::now();
                self.apps.insert(key.clone(), app_info);
                tracing::info!("App {} loaded successfully", key);
                Ok(())
            }
            Err(e) => {
                app_info.state = AppState::Error;
                app_info.error = Some(e.to_string());
                app_info.updated_at = SystemTime::now();
                tracing::error!("Failed to load app {}: {}", key, e);
                Err(e)
            }
        }
    }

    /// Put one version of one app on this node, replacing whatever it was
    /// running.
    ///
    /// "Whatever it was running" is per environment: installing a preview
    /// leaves production exactly where it was, because the two are separate
    /// keys down to the isolate.
    ///
    /// The caller has already proven the bundle: digest checked, manifest
    /// parsed, entry point present, assets read. What is left is the one step
    /// that can still fail on code the node has never executed - creating the
    /// isolate - and that step happens *beside* the running version, not in
    /// place of it: the new isolate is built on a runtime thread of its own
    /// and only then made the one that serves
    /// ([`RuntimePool::stage_app`] followed by [`RuntimePool::activate`]).
    ///
    /// So a request during a version change gets the old version or the new
    /// one and never a gap, however long the new isolate takes to build. The
    /// cost is that the application briefly holds two threads and two
    /// isolates.
    ///
    /// A new version that will not build leaves the old one serving and
    /// untouched -- nothing was taken away, so there is nothing to put back.
    pub async fn install_version(&self, install: VersionInstall) -> Result<()> {
        let VersionInstall {
            key,
            version_id,
            entry_point,
            code,
            config,
            env_confirmed,
            assets,
        } = install;

        let record = Arc::new(VersionRecord {
            version_id: version_id.clone(),
            env_confirmed,
            entry_point: entry_point.clone(),
            code: code.clone(),
            config: config.clone(),
            assets: assets.clone(),
        });

        let previous = self.versions.get(&key).map(|entry| entry.value().clone());
        let spec = self.spec_for(&key, &record);

        let staged = match self
            .runtime_pool
            .stage_app(key.tenant_id(), record.code.clone(), spec)
            .await
        {
            Ok(staged) => staged,
            Err(e) => {
                match &previous {
                    Some(previous) => tracing::error!(
                        app_id = %key.app_id(),
                        environment = %key.environment(),
                        version_id = %version_id,
                        serving_version = %previous.version_id,
                        error = %e,
                        "Version {} of {} would not build; it was built beside the running \
                         isolate and never reached traffic, so {} keeps serving version {} and \
                         no request was affected. The deployment did not take",
                        version_id,
                        key,
                        key,
                        previous.version_id
                    ),
                    None => tracing::error!(
                        app_id = %key.app_id(),
                        environment = %key.environment(),
                        version_id = %version_id,
                        error = %e,
                        "Version {} of {} would not build and it had no earlier version to fall \
                         back to; it is not serving and requests for it will fail until a \
                         working version is deployed",
                        version_id,
                        key
                    ),
                }
                return Err(anyhow!(
                    "version {version_id} of {key} would not build: {e}"
                ));
            }
        };

        if let Err(e) = self.runtime_pool.activate(staged).await {
            // The only way here is the application having been unloaded while
            // the new isolate was being built, which discarded it.
            self.apps.remove(&key);
            self.versions.remove(&key);
            tracing::error!(
                app_id = %key.app_id(),
                environment = %key.environment(),
                version_id = %version_id,
                error = %e,
                "Version {} of {} was built but could not be made the one that serves; {} is \
                 not running on this node",
                version_id,
                key,
                key
            );
            return Err(anyhow!(
                "version {version_id} of {key} could not be activated: {e}"
            ));
        }

        let now = SystemTime::now();
        self.apps.insert(
            key.clone(),
            App {
                id: key.app_id().to_string(),
                environment: key.environment().to_string(),
                state: AppState::Running,
                config,
                created_at: now,
                updated_at: now,
                error: None,
            },
        );
        self.versions.insert(key.clone(), record);
        tracing::info!(
            app_id = %key.app_id(),
            environment = %key.environment(),
            version_id = %version_id,
            replaced_version = previous.as_ref().map(|p| p.version_id.as_str()),
            env_confirmed,
            assets = assets.len(),
            asset_bytes = assets.total_bytes(),
            "App {} is now serving version {} of environment {}",
            key.app_id(),
            version_id,
            key.environment()
        );
        Ok(())
    }

    /// How one version's code is to be built into an isolate.
    fn spec_for(&self, key: &AppKey, record: &VersionRecord) -> IsolateSpec {
        IsolateSpec {
            entry_point: Some(record.entry_point.clone()),
            env: record.config.env.clone(),
            permissions: record.config.permissions.clone(),
            memory_limit_mb: Some(
                self.effective_memory_limit_mb(key, record.config.limits.as_ref()),
            ),
            resource_table_max: record
                .config
                .limits
                .as_ref()
                .and_then(|l| l.resource_table_max),
            route_patterns: record.config.routes.clone(),
            limits: self.effective_limits(key, record.config.limits.as_ref()),
            fetch_policy: self.fetch_policy.clone(),
            // Resolved once, when the deployment was installed, out of the
            // version's own manifest. Copied rather than recomputed here so
            // that every isolate this version is ever rebuilt into -- a
            // version swap, a pool restart -- answers exactly what the
            // deployment said it would.
            compatibility: record.config.compatibility,
            // The same bundle the HTTP entry serves from, so `env.ASSETS`
            // inside the isolate and asset-first outside it answer a path
            // identically. Shared, not copied: it is an `Arc` over bytes that
            // were read once when the version loaded.
            assets: Some(record.assets.clone()),
        }
    }

    /// The static files of the version currently running, for the HTTP entry
    /// to consult before anything reaches an isolate.
    pub fn assets(&self, key: &AppKey) -> Option<Arc<AssetBundle>> {
        self.versions.get(key).map(|e| e.assets.clone())
    }

    /// Which version this node runs for one environment of one app.
    pub fn installed_version(&self, key: &AppKey) -> Option<InstalledVersion> {
        self.versions.get(key).map(|e| InstalledVersion {
            version_id: e.version_id.clone(),
            env_confirmed: e.env_confirmed,
        })
    }

    /// Every (app, environment) with a deployed version, and what it runs.
    pub fn installed_versions(&self) -> Vec<(AppKey, InstalledVersion)> {
        self.versions
            .iter()
            .map(|e| {
                (
                    e.key().clone(),
                    InstalledVersion {
                        version_id: e.version_id.clone(),
                        env_confirmed: e.env_confirmed,
                    },
                )
            })
            .collect()
    }

    /// Every environment of one app that this node currently has loaded.
    ///
    /// Used by callers that only know the application - the management API
    /// unloads by app id, and the trigger service is told an app id with no
    /// environment - so they can say out loud which environments they mean
    /// instead of silently acting on one of them.
    pub fn environments_of(&self, app_id: &str) -> Vec<AppKey> {
        let mut keys: Vec<AppKey> = self
            .apps
            .iter()
            .map(|e| e.key().clone())
            .filter(|key| key.app_id() == app_id)
            .collect();
        keys.sort();
        keys
    }

    pub fn is_loaded(&self, key: &AppKey) -> bool {
        self.apps.contains_key(key)
    }

    /// Start debug session
    pub fn start_inspect(&self, key: AppKey, ttl_seconds: u64) -> Result<(String, String)> {
        // Check if application exists
        if !self.apps.contains_key(&key) {
            return Err(anyhow!("App {} not found", key));
        }

        // Generate token
        let token = uuid::Uuid::new_v4().to_string();
        let expires_at = SystemTime::now() + std::time::Duration::from_secs(ttl_seconds);

        let inspect_app_id = format!("_inspect:{}", key.app_id());

        // Store session
        self.inspect_sessions
            .insert(key.clone(), (token.clone(), expires_at));

        tracing::info!(
            app_id = %key.app_id(),
            environment = %key.environment(),
            "Started debug session for app {}: token={}, ttl={}s",
            key,
            token,
            ttl_seconds
        );

        Ok((inspect_app_id, token))
    }

    /// Validate debug token
    pub fn validate_inspect_token(&self, key: &AppKey, token: &str) -> bool {
        if let Some(entry) = self.inspect_sessions.get(key) {
            let (stored_token, expires_at) = entry.value();
            if stored_token == token && SystemTime::now() < *expires_at {
                return true;
            }
        }
        false
    }

    /// Connect debugger
    pub async fn connect_inspector(
        &self,
        key: &AppKey,
    ) -> Result<(
        tokio::sync::mpsc::UnboundedSender<String>,
        tokio::sync::mpsc::UnboundedReceiver<String>,
    )> {
        self.runtime_pool.connect_inspector(&key.tenant_id()).await
    }

    /// Unload one environment of one application.
    pub async fn unload_app(&self, key: &AppKey) -> Result<()> {
        tracing::info!("Unloading app: {}", key);

        // Unload from runtime pool
        self.runtime_pool.unload_app(&key.tenant_id()).await?;

        // Remove application information. The version record goes with it: its
        // asset index must not answer requests for an app that is no longer
        // loaded.
        self.versions.remove(key);
        self.inspect_sessions.remove(key);
        match self.apps.remove(key) {
            Some(_) => {
                tracing::info!("App {} unloaded successfully", key);
                Ok(())
            }
            None => Err(anyhow!("App {} not found", key)),
        }
    }

    /// Execute application.
    ///
    /// The request body goes in as bytes: what an application receives is not
    /// always text, and text is what this used to be. The response body comes
    /// back as a [`crate::runtime::ResponseBody`], which is bytes for nearly
    /// every answer and a body still being produced for the few that stream.
    ///
    /// The response headers come back as a list rather than a map: one name
    /// may legitimately appear twice - `Set-Cookie` always may - and a map
    /// here silently kept one of them. Whether repeats survive was already
    /// decided inside the isolate, from the application's compatibility date;
    /// see `crate::runtime::response_headers`.
    pub async fn execute_app(
        &self,
        key: &AppKey,
        url: String,
        method: String,
        headers: std::collections::HashMap<String, String>,
        body: Option<Vec<u8>>,
    ) -> Result<(
        u16,
        crate::runtime::HeaderPairs,
        crate::runtime::ResponseBody,
    )> {
        self.require_running(key)?;

        // Execute request via runtime pool
        self.runtime_pool
            .execute_request(&key.tenant_id(), url, method, headers, body)
            .await
    }

    /// Hand a batch of queue messages to one application's queue handler.
    ///
    /// Used by the push path (`Trigger/OnQueue`); this node's own pollers
    /// reach the same code through the runtime pool directly. The returned
    /// outcome names the messages the handler did not complete, which is what
    /// the caller acknowledges or hands back.
    pub async fn execute_queue_batch(
        &self,
        key: &AppKey,
        queue: String,
        messages: Vec<crate::runtime::LeasedMessage>,
    ) -> Result<crate::runtime::BatchOutcome> {
        self.require_running(key)?;

        self.runtime_pool
            .execute_queue_batch(&key.tenant_id(), queue, messages)
            .await
    }

    /// Run one application's scheduled handler for one due cron expression.
    ///
    /// The schedule lives in the control plane, which picks a single node per
    /// tick (architecture principle 6), so the answer has to say *which* kind
    /// of failure happened: a node that cannot run the app at all should be
    /// replaced by another node, while a handler that threw has already had
    /// whatever effect it had and must not be run again elsewhere. Everything
    /// from the isolate inwards counts as the second kind, because by then the
    /// application's own code has been entered.
    pub async fn execute_scheduled(
        &self,
        key: &AppKey,
        cron: String,
        scheduled_time_ms: i64,
    ) -> Result<(), ScheduledFailure> {
        self.require_running(key)
            .map_err(|e| ScheduledFailure::NotRunnableHere(e.to_string()))?;

        self.runtime_pool
            .execute_scheduled(&key.tenant_id(), cron, scheduled_time_ms)
            .await
            .map_err(|e| ScheduledFailure::HandlerFailed(e.to_string()))
    }

    /// Refuse work for an app this node is not serving.
    fn require_running(&self, key: &AppKey) -> Result<()> {
        let entry = self
            .apps
            .get(key)
            .ok_or_else(|| anyhow!("App {} not found", key))?;

        if entry.state != AppState::Running {
            return Err(anyhow!(
                "App {} is not running (state: {:?})",
                key,
                entry.state
            ));
        }

        Ok(())
    }

    /// Get application information
    #[allow(dead_code)]
    pub fn get_app(&self, key: &AppKey) -> Option<App> {
        self.apps.get(key).map(|entry| entry.clone())
    }

    /// List all applications
    pub fn list_apps(&self) -> Vec<App> {
        self.apps.iter().map(|entry| entry.clone()).collect()
    }

    /// Get statistics
    #[allow(dead_code)]
    pub async fn get_stats(&self) -> serde_json::Value {
        let apps: Vec<App> = self.apps.iter().map(|entry| entry.clone()).collect();
        let pool_stats = self.runtime_pool.get_stats().await;

        let mut stats = serde_json::json!({
            "total": apps.len(),
            "running": apps.iter().filter(|a| a.state == AppState::Running).count(),
            "stopped": apps.iter().filter(|a| a.state == AppState::Stopped).count(),
            "error": apps.iter().filter(|a| a.state == AppState::Error).count(),
            "loading": apps.iter().filter(|a| a.state == AppState::Loading).count(),
        });

        // Add thread pool statistics
        if let Some(obj) = stats.as_object_mut() {
            obj.insert(
                "pool".to_string(),
                serde_json::to_value(pool_stats).unwrap(),
            );
        }

        stats
    }

    /// Get aggregate OpMetrics for each (app, environment) this node runs.
    pub async fn get_op_metrics(&self) -> Vec<(AppKey, OpMetricsSummary)> {
        let keys: Vec<AppKey> = self.apps.iter().map(|entry| entry.key().clone()).collect();
        let mut results = Vec::new();

        for key in keys {
            match self.runtime_pool.get_op_metrics(&key.tenant_id()).await {
                Ok(summary) => {
                    results.push((key, summary));
                }
                Err(e) => {
                    tracing::warn!(
                        app_id = %key.app_id(),
                        environment = %key.environment(),
                        error = %e,
                        "Failed to get OpMetrics summary for {}; its op counts are missing from \
                         this status answer",
                        key
                    );
                }
            }
        }

        results
    }

    /// Register WebSocket connection
    ///
    /// Establish bidirectional message channel between Gateway and JavaScript WebSocketServer
    ///
    /// # Parameters
    /// - key: which environment of which application
    /// - ws_id: WebSocket connection ID
    /// - js_to_client_tx: Channel for JavaScript to send messages to the client
    pub async fn register_ws_connection(
        &self,
        key: &AppKey,
        ws_id: String,
        js_to_client_tx: tokio::sync::mpsc::UnboundedSender<crate::runtime::WsServerMessage>,
    ) -> Result<()> {
        self.runtime_pool
            .register_ws_connection(&key.tenant_id(), ws_id, js_to_client_tx)
            .await
    }

    /// Send WebSocket message to JavaScript
    ///
    /// # Parameters
    /// - key: which environment of which application
    /// - ws_id: WebSocket connection ID
    /// - message: Message content
    pub async fn send_ws_message(
        &self,
        key: &AppKey,
        ws_id: String,
        message: String,
    ) -> Result<()> {
        self.runtime_pool
            .send_ws_message(&key.tenant_id(), ws_id, message)
            .await
    }

    /// Unregister WebSocket connection
    ///
    /// # Parameters
    /// - key: which environment of which application
    /// - ws_id: WebSocket connection ID
    pub async fn unregister_ws_connection(&self, key: &AppKey, ws_id: String) -> Result<()> {
        self.runtime_pool
            .unregister_ws_connection(&key.tenant_id(), ws_id)
            .await
    }
}

impl Default for ApplicationManager {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
#[path = "manager_test.rs"]
mod manager_test;

/// The node-level heap ceiling, exercised against a real isolate.
///
/// Not in `manager_test.rs` with the rest: this is the one property of the
/// manager that only shows up in V8, so it costs a real runtime thread and a
/// real allocation loop, and it is worth having the reason for that in one
/// place rather than mixed into the fast tests.
#[cfg(test)]
mod memory_ceiling {
    use super::*;
    use crate::app::types::{AppConfig, ResourceLimits};
    use std::collections::HashMap;

    /// Allocates until something stops it. `new Array(n).fill(0)` is used
    /// rather than a bare `new Array(n)`, which V8 is free to leave as a
    /// hole-backed array that costs nothing: the `fill` forces a real backing
    /// store, so the heap genuinely grows on every iteration.
    const RUNAWAY: &str = r#"
        export default function handler() {
            const held = [];
            for (;;) {
                held.push(new Array(100000).fill(0));
            }
        }
    "#;

    const HEALTHY: &str = r#"
        export default function handler() {
            return new Response("still serving");
        }
    "#;

    fn limits(memory_mb: usize) -> AppConfig {
        AppConfig {
            limits: Some(ResourceLimits {
                memory_mb,
                cpu_milli: 0,
                resource_table_max: None,
                timeout_ms: None,
                rate_limit_ops_per_sec: None,
                cpu_time_limit_ms: None,
            }),
            ..AppConfig::default()
        }
    }

    async fn call(manager: &ApplicationManager, key: &AppKey) -> Result<(u16, Option<Vec<u8>>)> {
        manager
            .execute_app(
                key,
                "http://localhost/".to_string(),
                "GET".to_string(),
                HashMap::new(),
                None,
            )
            .await
            .map(|(status, _, body)| (status, body.into_bytes()))
    }

    /// The bug this ceiling exists for: nothing ever set `memory_limit_mb`, so
    /// every isolate ran on whatever V8 defaults to and an application
    /// allocating in a loop grew until the node process was killed -- taking
    /// every other application in that process with it.
    ///
    /// Both applications share one runtime thread here on purpose. The second
    /// call is what proves the runaway was contained: if the heap ceiling were
    /// enforced by killing the process, or if the terminated isolate left the
    /// thread wedged, there would be nothing left to answer it.
    #[tokio::test]
    async fn an_app_allocating_without_bound_is_stopped_and_the_node_keeps_serving() {
        let manager = ApplicationManager::with_threads(Some(2)).with_memory_limit_mb(64);
        let runaway = AppKey::production("runaway_allocator");
        let neighbour = AppKey::production("quiet_neighbour");

        manager
            .load_app(
                runaway.clone(),
                RUNAWAY.to_string(),
                AppConfig::default(),
                None,
            )
            .await
            .expect("an app with no limits of its own still loads");
        manager
            .load_app(
                neighbour.clone(),
                HEALTHY.to_string(),
                AppConfig::default(),
                None,
            )
            .await
            .expect("the neighbour loads");

        // The isolate logs the heap it was stopped at; with a test writer it
        // is visible under `--nocapture`, which is how a failure here is
        // diagnosed.
        let _ = tracing_subscriber::fmt().with_test_writer().try_init();

        let started = std::time::Instant::now();
        let refused = call(&manager, &runaway).await;
        let elapsed = started.elapsed();
        assert!(
            refused.is_err(),
            "an app allocating without bound must be stopped, not answered; it returned {refused:?}"
        );

        // What stopped it has to be the heap and not the clock. The execution
        // timeout is the other thing that terminates an isolate, and it is a
        // whole platform budget away; the ceiling bites in a fraction of it.
        let budget = std::time::Duration::from_millis(manager.default_limits.timeout_ms);
        assert!(
            elapsed * 4 < budget,
            "the runaway was stopped after {elapsed:?}, close enough to the {budget:?} execution \
             timeout that the timeout, not the heap ceiling, may be what stopped it"
        );

        let (status, body) = call(&manager, &neighbour)
            .await
            .expect("the node still serves its other applications");
        assert_eq!(status, 200);
        assert_eq!(
            String::from_utf8(body.unwrap_or_default()).as_deref(),
            Ok("still serving")
        );

        let _ = manager.unload_app(&runaway).await;
        let _ = manager.unload_app(&neighbour).await;
    }

    #[test]
    fn an_app_that_asks_for_nothing_is_held_to_the_node_ceiling() {
        let manager = ApplicationManager::with_threads(Some(1)).with_memory_limit_mb(128);
        let key = AppKey::production("no_limits_of_its_own");
        assert_eq!(manager.effective_memory_limit_mb(&key, None), 128);
        // A deployment that carries a `ResourceLimits` but leaves the memory
        // key at its default is the same case: it asked for nothing.
        let unset = limits(0);
        assert_eq!(
            manager.effective_memory_limit_mb(&key, unset.limits.as_ref()),
            128
        );
    }

    #[test]
    fn an_app_may_narrow_the_ceiling_but_not_widen_it() {
        let manager = ApplicationManager::with_threads(Some(1)).with_memory_limit_mb(128);
        let key = AppKey::production("bounded_by_the_node");

        let narrower = limits(32);
        assert_eq!(
            manager.effective_memory_limit_mb(&key, narrower.limits.as_ref()),
            32
        );

        let wider = limits(4096);
        assert_eq!(
            manager.effective_memory_limit_mb(&key, wider.limits.as_ref()),
            128,
            "an app must not be able to raise the node's ceiling"
        );
    }

    /// The call site the bug lived at: `IsolateSpec.memory_limit_mb` was fed
    /// `config.limits.as_ref().map(|l| l.memory_mb)` and nothing ever produced
    /// a `limits`, so it was `None` on every isolate on every node.
    #[test]
    fn every_isolate_is_built_with_a_ceiling_even_with_no_deployment_limits() {
        let manager = ApplicationManager::with_threads(Some(1)).with_memory_limit_mb(96);
        let key = AppKey::production("spec_carries_a_ceiling");
        assert_eq!(
            manager.effective_memory_limit_mb(&key, AppConfig::default().limits.as_ref()),
            96
        );
    }
}

/// The CPU ceiling, resolved the same way the heap ceiling is: the node's
/// value, which an application may hold itself below and may not raise.
#[cfg(test)]
mod cpu_ceiling {
    use super::*;
    use crate::app::types::{AppConfig, ResourceLimits};
    use crate::config::ExecutionLimits;

    fn key(app_id: &str) -> AppKey {
        AppKey::production(app_id)
    }

    fn asking_for(cpu_time_limit_ms: Option<u64>) -> AppConfig {
        AppConfig {
            limits: Some(ResourceLimits {
                memory_mb: 0,
                cpu_milli: 0,
                resource_table_max: None,
                timeout_ms: None,
                rate_limit_ops_per_sec: None,
                cpu_time_limit_ms,
            }),
            ..AppConfig::default()
        }
    }

    fn node_with_cpu_ceiling(cpu_time_limit_ms: u64) -> ApplicationManager {
        ApplicationManager::with_threads(Some(1)).with_limits(ExecutionLimits {
            cpu_time_limit_ms,
            ..ExecutionLimits::default()
        })
    }

    /// The reachability check the heap ceiling failed for months: a default
    /// that only a test can produce is not a default. Every path that builds
    /// an isolate goes through `effective_limits`, and a deployment that
    /// carries no `ResourceLimits` at all -- which is every deployment the
    /// control plane sends today -- has to come out of it holding the node's
    /// number.
    #[test]
    fn every_isolate_is_built_with_a_cpu_ceiling_even_with_no_deployment_limits() {
        let manager = node_with_cpu_ceiling(1_500);
        let effective = manager.effective_limits(
            &key("no_limits_at_all"),
            AppConfig::default().limits.as_ref(),
        );
        assert_eq!(effective.cpu_time_limit_ms, 1_500);

        // And the same through the struct an isolate is actually built from,
        // so a ceiling resolved correctly and then dropped on the floor
        // between here and `AppRuntime::new` would still fail.
        let spec_limits = manager.effective_limits(&key("no_limits_at_all"), None);
        assert_eq!(spec_limits.cpu_time_limit_ms, 1_500);
    }

    #[test]
    fn an_app_may_narrow_the_cpu_ceiling_but_not_widen_it() {
        let manager = node_with_cpu_ceiling(1_000);

        let narrower = asking_for(Some(250));
        assert_eq!(
            manager
                .effective_limits(&key("modest"), narrower.limits.as_ref())
                .cpu_time_limit_ms,
            250
        );

        let wider = asking_for(Some(60_000));
        assert_eq!(
            manager
                .effective_limits(&key("greedy"), wider.limits.as_ref())
                .cpu_time_limit_ms,
            1_000,
            "CPU time is the node's resource; an application must not be able to help itself to \
             more of it than the operator allowed"
        );
    }

    /// `0` is what an absent field looks like on the wire, and a ceiling of
    /// zero would terminate every invocation before it ran an instruction.
    #[test]
    fn a_zero_cpu_ceiling_is_read_as_unset_and_not_as_no_cpu_at_all() {
        let manager = node_with_cpu_ceiling(2_000);
        let unset = asking_for(Some(0));
        assert_eq!(
            manager
                .effective_limits(&key("zero_means_unset"), unset.limits.as_ref())
                .cpu_time_limit_ms,
            2_000
        );
    }
}

/// The two limits that used to pass straight through: an application's own
/// `timeout_ms` and `rate_limit_ops_per_sec` were taken with `unwrap_or`, so
/// the moment a deployment could actually carry them, a tenant could have
/// raised its own ceiling on a machine it shares.
#[cfg(test)]
mod duration_and_rate_ceilings {
    use super::*;
    use crate::app::types::{AppConfig, ResourceLimits};
    use crate::config::ExecutionLimits;

    fn key(app_id: &str) -> AppKey {
        AppKey::production(app_id)
    }

    fn asking(limits: ResourceLimits) -> AppConfig {
        AppConfig {
            limits: Some(limits),
            ..AppConfig::default()
        }
    }

    fn node(timeout_ms: u64, rate_limit_ops_per_sec: f64) -> ApplicationManager {
        ApplicationManager::with_threads(Some(1)).with_limits(ExecutionLimits {
            timeout_ms,
            // Kept under the timeout so the "a CPU ceiling above the duration
            // ceiling can never fire" warning does not fire in these tests and
            // confuse whoever reads their output.
            cpu_time_limit_ms: timeout_ms / 2,
            rate_limit_ops_per_sec,
            ..ExecutionLimits::default()
        })
    }

    #[test]
    fn an_app_that_declares_nothing_takes_the_node_numbers() {
        let manager = node(30_000, 1_000.0);
        let effective = manager.effective_limits(&key("silent"), None);
        assert_eq!(effective.timeout_ms, 30_000);
        assert_eq!(effective.rate_limit_ops_per_sec, 1_000.0);
    }

    #[test]
    fn an_app_may_narrow_the_execution_timeout() {
        let manager = node(30_000, 1_000.0);
        let asked = asking(ResourceLimits {
            timeout_ms: Some(2_000),
            ..ResourceLimits::default()
        });
        assert_eq!(
            manager
                .effective_limits(&key("modest"), asked.limits.as_ref())
                .timeout_ms,
            2_000
        );
    }

    /// The hole. An application declaring an hour used to *get* an hour, and
    /// with it a runtime thread every other tenant on this node queues behind.
    #[test]
    fn an_app_may_not_widen_the_execution_timeout() {
        let manager = node(30_000, 1_000.0);
        let asked = asking(ResourceLimits {
            timeout_ms: Some(3_600_000),
            ..ResourceLimits::default()
        });
        assert_eq!(
            manager
                .effective_limits(&key("greedy"), asked.limits.as_ref())
                .timeout_ms,
            30_000,
            "an application must not be able to raise the node's duration ceiling"
        );
    }

    #[test]
    fn a_zero_timeout_is_read_as_unset_rather_than_as_no_time_at_all() {
        let manager = node(30_000, 1_000.0);
        let asked = asking(ResourceLimits {
            timeout_ms: Some(0),
            ..ResourceLimits::default()
        });
        assert_eq!(
            manager
                .effective_limits(&key("zero"), asked.limits.as_ref())
                .timeout_ms,
            30_000
        );
    }

    #[test]
    fn an_app_may_narrow_the_op_rate_limit() {
        let manager = node(30_000, 1_000.0);
        let asked = asking(ResourceLimits {
            rate_limit_ops_per_sec: Some(50.0),
            ..ResourceLimits::default()
        });
        assert_eq!(
            manager
                .effective_limits(&key("quiet"), asked.limits.as_ref())
                .rate_limit_ops_per_sec,
            50.0
        );
    }

    #[test]
    fn an_app_may_not_widen_the_op_rate_limit() {
        let manager = node(30_000, 1_000.0);
        let asked = asking(ResourceLimits {
            rate_limit_ops_per_sec: Some(1_000_000.0),
            ..ResourceLimits::default()
        });
        assert_eq!(
            manager
                .effective_limits(&key("chatty"), asked.limits.as_ref())
                .rate_limit_ops_per_sec,
            1_000.0,
            "op dispatch is work this node does; an application must not be able to award itself \
             more of it"
        );
    }

    /// NaN compares false against every bound, so a naive `requested <=
    /// default` check would let it through and install a ceiling that nothing
    /// can ever cross - a rate limit switched off by an application.
    #[test]
    fn a_rate_limit_that_is_not_a_number_is_refused() {
        let manager = node(30_000, 1_000.0);
        for asked_for in [f64::NAN, f64::INFINITY, -1.0, 0.0] {
            let asked = asking(ResourceLimits {
                rate_limit_ops_per_sec: Some(asked_for),
                ..ResourceLimits::default()
            });
            assert_eq!(
                manager
                    .effective_limits(&key("nonsense"), asked.limits.as_ref())
                    .rate_limit_ops_per_sec,
                1_000.0,
                "{asked_for} is not a usable rate and must not become this app's ceiling"
            );
        }
    }

    /// All four limits at once, through the one call every isolate is built
    /// from: two narrowed, two greedy, and the greedy pair held to the node's
    /// numbers.
    #[test]
    fn a_deployment_that_narrows_two_limits_and_widens_two_gets_exactly_that() {
        let manager = node(30_000, 1_000.0).with_memory_limit_mb(128);
        let asked = asking(ResourceLimits {
            memory_mb: 32,
            timeout_ms: Some(90_000),
            rate_limit_ops_per_sec: Some(25.0),
            cpu_time_limit_ms: Some(600_000),
            ..ResourceLimits::default()
        });
        let effective = manager.effective_limits(&key("mixed"), asked.limits.as_ref());
        assert_eq!(effective.timeout_ms, 30_000, "widened; node's value wins");
        assert_eq!(effective.cpu_time_limit_ms, 15_000, "widened; node's value");
        assert_eq!(effective.rate_limit_ops_per_sec, 25.0, "narrowed; applied");
        assert_eq!(
            manager.effective_memory_limit_mb(&key("mixed"), asked.limits.as_ref()),
            32,
            "narrowed; applied"
        );
    }
}
