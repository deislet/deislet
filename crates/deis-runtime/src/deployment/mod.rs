//! Taking delivery of a deployment.
//!
//! The control plane never sends an application, only a pointer to one: app,
//! version, environment, bundle URL, digest, and the environment map the app
//! will see (docs/implementation-plan.zh.md section 4). Everything after that
//! is this module's job - fetch, verify, unpack, load - and the ordering is
//! the whole point:
//!
//! ```text
//! download → digest → manifest → entry point → assets → swap the isolate
//! ```
//!
//! Every step before the swap can fail without the app noticing. A bad digest,
//! a truncated zip, a manifest for another app, an asset the manifest promised
//! and the bundle does not have: all of them end with the node still serving
//! the version it was already serving. Only the last step touches a running
//! app, and even that keeps the previous version in hand.
//!
//! Secrets arrive in `env` and stay in this process's memory: not in the
//! bundle, not in the on-disk record, not in a log line, not in any answer the
//! management API gives.
//!
//! The node's own side of the control-plane channel lives here too, because it
//! is the same connection and the same credential: [`registration`] announces
//! that this node exists and [`assignments`] asks what it should be running
//! and keeps it matching the answer.

mod assignments;
mod credential;
mod fetch;
mod registration;
mod store;

use std::collections::HashMap;
use std::io::{Cursor, Read};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use dashmap::DashMap;
use tracing::{debug, error, info, warn};

use crate::app::types::{AppConfig, ResourceLimits};
use crate::app::{AppKey, ApplicationManager, VersionInstall};
use crate::assets::{AssetBundle, AssetError};
use crate::config::RuntimeConfig;

pub use assignments::{
    run_control_loop, Assignment, AssignmentClient, AssignmentError, AssignmentList, Reconciler,
    SyncOutcome,
};
pub use credential::NodeCredential;
pub use fetch::{sha256_hex, BundleFetcher, FetchError, FetchedBundle};
pub use registration::{NodeRegistrar, NodeRegistration, Registered, RegistrationError};
pub use store::{check_identifier, BundleStore, InstalledRecord, InvalidIdentifier};

/// One deployment the node has been told to run.
///
/// `app_id` and `environment` arrive as two fields because that is how the
/// control plane sends them; on this node they are one key
/// ([`DeploymentOrder::key`]), because a preview and production of the same
/// application are two deployments that must not share a slot.
///
/// `env` holds decrypted secret values, which is why this type has a
/// hand-written `Debug`: it is built straight from a gRPC request, and a
/// `{:?}` on the request would print every secret the control plane just
/// decrypted.
pub struct DeploymentOrder {
    pub app_id: String,
    pub version_id: String,
    pub environment: String,
    pub bundle_url: String,
    pub bundle_sha: String,
    pub env: HashMap<String, String>,
    /// What this version's manifest asked to be held to, as the control plane
    /// read it out of the manifest and sent it. `None` means the application
    /// declared nothing and takes this node's own ceilings.
    ///
    /// Never a grant: every field is clamped against the node's configuration
    /// in [`crate::app::ApplicationManager::effective_limits`], which is the
    /// only place that knows what this node allows. What travels here is a
    /// request to be held to *less*.
    pub limits: Option<ResourceLimits>,
    /// Which of the platform's answers this version expects, as the control
    /// plane resolved them out of the manifest it stored.
    ///
    /// `None` means the control plane said nothing - an older one, or the
    /// recovery path below, which reads nothing off the wire at all. It does
    /// **not** mean "the oldest behaviour": the bundle's own manifest carries
    /// the declaration and is what [`DeploymentManager::install`] resolves, so
    /// a version recovered from this node's disk comes back on exactly the
    /// date it was deployed with. That is the one difference from
    /// [`Self::limits`], which cannot survive a disk recovery, and it is
    /// deliberate: a narrower ceiling lost for a while is a slower
    /// application, while a compatibility date lost for a while is an
    /// application that quietly behaves differently.
    ///
    /// What this field is for is the refusal that has to happen *before* a
    /// bundle is downloaded, and the cross-check afterwards: if the control
    /// plane and the bundle disagree about what a version declared, one of
    /// them is wrong and an operator has to be told which.
    pub compatibility: Option<deis_bundle::CompatibilityProfile>,
}

impl DeploymentOrder {
    /// Which running application this order is about.
    pub fn key(&self) -> AppKey {
        AppKey::new(self.app_id.clone(), self.environment.clone())
    }
}

impl std::fmt::Debug for DeploymentOrder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut names: Vec<&str> = self.env.keys().map(String::as_str).collect();
        names.sort_unstable();
        f.debug_struct("DeploymentOrder")
            .field("app_id", &self.app_id)
            .field("version_id", &self.version_id)
            .field("environment", &self.environment)
            .field("bundle_url", &self.bundle_url)
            .field("bundle_sha", &self.bundle_sha)
            .field("env", &format_args!("{names:?} = <redacted>"))
            .field("limits", &self.limits)
            .field("compatibility", &self.compatibility.map(|c| c.describe()))
            .finish()
    }
}

/// Why a deployment did not take.
///
/// Every variant leaves the node running whatever it ran before.
#[derive(Debug)]
pub enum DeployError {
    /// An identifier could not be used as a directory name.
    Identifier(InvalidIdentifier),
    /// The bundle could not be downloaded, or is not what was promised.
    Fetch(FetchError),
    /// The bundle is not readable as a deislet bundle.
    Bundle(deis_bundle::BundleError),
    /// The bundle's static files cannot be served as declared.
    Assets(AssetError),
    /// The bundle is fine but the node could not put it on disk.
    Storage(String),
    /// The version names a compatibility date or a behaviour flag this build
    /// of the node cannot honour.
    ///
    /// Its own variant rather than a `Bundle` or a `Load`, because it is the
    /// one refusal that is about *this node* rather than about the bundle: the
    /// same bundle runs fine on a node built after the date it names. The
    /// message says both dates so an operator reads it as "upgrade this node",
    /// not "the application is broken".
    Compatibility(String),
    /// The isolate would not take the new code.
    Load(String),
}

impl std::fmt::Display for DeployError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Identifier(e) => write!(f, "{e}"),
            Self::Fetch(e) => write!(f, "{e}"),
            Self::Bundle(e) => write!(f, "{e}"),
            Self::Assets(e) => write!(f, "{e}"),
            Self::Storage(m) => write!(f, "could not store the bundle: {m}"),
            Self::Compatibility(m) => {
                write!(f, "this node cannot honour what the version declares: {m}")
            }
            Self::Load(m) => write!(f, "the bundle would not load: {m}"),
        }
    }
}

impl std::error::Error for DeployError {}

/// What happened to one deployment order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Applied {
    /// The version was fetched (or reused from disk) and is now serving.
    Installed { reused_bundle: bool },
    /// This environment was already on this exact version, with an environment
    /// the control plane had confirmed, and was left alone. Only the startup
    /// path can produce this; a push always reloads, because a secret may have
    /// changed under a version that did not move.
    AlreadyRunning,
    /// The order was older than what this node has already done to this
    /// environment, so it was not applied. Only a reconciliation can produce
    /// this: it acts on a list that was true at the moment it was asked for,
    /// and a push that landed since then is the newer instruction.
    Superseded {
        /// The version the newer instruction left running here, if any. `None`
        /// means the newer instruction was an unload.
        running_version_id: Option<String>,
    },
}

/// Fetches, verifies and installs deployments.
pub struct DeploymentManager {
    app_manager: Arc<ApplicationManager>,
    fetcher: BundleFetcher,
    store: BundleStore,
    cache_control: Arc<str>,
    max_asset_bytes: u64,
    /// One lock per (app, environment), so two deliveries for the same running
    /// application cannot interleave while deliveries for anything else - a
    /// different app, or another environment of the same app - still run side
    /// by side. Startup and a control-plane push race exactly here.
    locks: DashMap<AppKey, Arc<tokio::sync::Mutex<()>>>,
    /// Counts every change this node has made to what it runs, in the order
    /// the changes happened.
    ///
    /// It exists because nothing else on this node can order two instructions
    /// in time. A version id is a random uuid minted by the control plane, and
    /// a rollback deliberately points an environment back at an older version,
    /// so neither comparing two version ids nor merely noticing that they
    /// differ says which of them is the later instruction. What this node can
    /// order is when each instruction reached it, and that is this counter.
    changes: AtomicU64,
    /// The stamp of the last change to each (app, environment), so a caller
    /// holding a stamp from before it asked the control plane anything can ask
    /// whether this environment moved since.
    last_change: DashMap<AppKey, u64>,
}

impl DeploymentManager {
    pub async fn from_config(
        app_manager: Arc<ApplicationManager>,
        config: &RuntimeConfig,
    ) -> anyhow::Result<Self> {
        let fetcher = BundleFetcher::new(
            Duration::from_secs(config.bundles.download_timeout_secs),
            config.bundles.max_bytes,
            config.node_credential(),
        )?;
        let store = BundleStore::open(config.bundle_dir()).await?;
        Ok(Self {
            app_manager,
            fetcher,
            store,
            cache_control: Arc::from(config.assets.cache_control.as_str()),
            max_asset_bytes: config.assets.max_bytes,
            locks: DashMap::new(),
            changes: AtomicU64::new(0),
            last_change: DashMap::new(),
        })
    }

    pub fn store(&self) -> &BundleStore {
        &self.store
    }

    /// How many deployed versions this node currently runs, counting each
    /// environment of an application separately - which is how they run.
    pub fn installed_app_count(&self) -> usize {
        self.app_manager.installed_versions().len()
    }

    /// Every (app, environment) this node runs a deployed version of, and
    /// which version.
    ///
    /// This is the "before" half of a reconciliation: what the node is doing,
    /// against what the control plane says it should be. The built-in
    /// `_system` app is not here - it has no deployed version - so nothing
    /// that reconciles against an assignment list can stop it.
    pub fn running(&self) -> Vec<(AppKey, crate::app::InstalledVersion)> {
        self.app_manager.installed_versions()
    }

    /// The stamp to read *before* asking the control plane what to run.
    ///
    /// Everything this node does to itself after this point is newer than the
    /// answer that question returns, however long the request takes, so the
    /// answer must not be allowed to undo it. See [`Self::changed_since`].
    pub fn change_stamp(&self) -> u64 {
        self.changes.load(Ordering::SeqCst)
    }

    /// Whether this node changed what it runs for `key` after `stamp` was
    /// taken - installed a version, or stopped serving one.
    pub fn changed_since(&self, key: &AppKey, stamp: u64) -> bool {
        self.last_change
            .get(key)
            .is_some_and(|last| *last.value() > stamp)
    }

    /// Record that `key` just changed, and stamp it.
    ///
    /// Called from inside the per-key lock, after the change has taken effect,
    /// so a stamp is never visible before the change it describes.
    fn record_change(&self, key: &AppKey) {
        let stamp = self.changes.fetch_add(1, Ordering::SeqCst) + 1;
        self.last_change.insert(key.clone(), stamp);
    }

    fn lock_for(&self, key: &AppKey) -> Arc<tokio::sync::Mutex<()>> {
        self.locks
            .entry(key.clone())
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone()
    }

    /// Apply a deployment the control plane pushed.
    ///
    /// Always reloads, even when the version has not moved: a secret change
    /// produces no new version but does change what the isolate must see, and
    /// `context.env` is read when the isolate is built.
    pub async fn apply(&self, order: DeploymentOrder) -> Result<Applied, DeployError> {
        let lock = self.lock_for(&order.key());
        let _guard = lock.lock().await;
        self.install(order, false).await
    }

    /// Apply a deployment from an assignment list.
    ///
    /// `as_of` is [`Self::change_stamp`] read *before* the list was asked for,
    /// which is what dates the list. An assignment list describes what the
    /// control plane wanted at that moment; a push that landed while the
    /// request was in flight is a later instruction about the same
    /// environment, and applying the list on top of it would silently roll the
    /// push back. So an environment this node changed after `as_of` is left
    /// exactly as that newer change left it, and the next pass - which reads a
    /// stamp taken after it - reconciles against the real state.
    ///
    /// Ordering by stamp rather than by version id is not a shortcut: version
    /// ids are random uuids and a rollback points an environment at an older
    /// one, so "the version differs" says nothing about which instruction came
    /// first.
    ///
    /// Skips an app that is already running the same version *with an
    /// environment the control plane confirmed*, because that can only mean a
    /// control-plane push landed while startup was still working through its
    /// list - and that push is the newer instruction, plus it is the one that
    /// carried the secrets.
    ///
    /// The second half of that condition is what makes it safe. The other way
    /// to be "already on this version" is to have recovered it from local
    /// disk, where secret values are never written: that copy runs the right
    /// code with an empty environment. Comparing version ids alone would wave
    /// the real delivery through, and the app would keep serving with no
    /// secrets and no storage addresses for as long as the process lived,
    /// while the node reported itself ready.
    pub async fn apply_from_assignment(
        &self,
        order: DeploymentOrder,
        as_of: u64,
    ) -> Result<Applied, DeployError> {
        let key = order.key();
        let lock = self.lock_for(&key);
        let _guard = lock.lock().await;

        if self.changed_since(&key, as_of) {
            let running_version_id = self
                .app_manager
                .installed_version(&key)
                .map(|current| current.version_id);
            info!(
                app_id = %key.app_id(),
                environment = %key.environment(),
                assigned_version_id = %order.version_id,
                running_version_id = %running_version_id.as_deref().unwrap_or("<none>"),
                "Not applying assigned version {} of {}: this node changed that environment \
                 after the assignment list was asked for, so the list is the older instruction \
                 of the two. The newer one stands and the next reconcile checks against it",
                order.version_id,
                key
            );
            return Ok(Applied::Superseded { running_version_id });
        }

        if let Some(current) = self.app_manager.installed_version(&key) {
            if current.version_id == order.version_id {
                // The version record is this node's memory of what it
                // installed; whether an isolate is still there is a separate
                // fact, and the two come apart when one is lost without
                // anything being unloaded -- a panic on its runtime thread.
                // Answering "already running" from the record alone is what
                // made that permanent: the control plane was told the
                // assignment was satisfied on every pass, so the one thing
                // that could have put the application back never ran.
                if current.env_confirmed && self.app_manager.is_hosted(&key).await {
                    debug!(
                        app_id = %key.app_id(),
                        environment = %key.environment(),
                        version_id = %order.version_id,
                        "App is already on the assigned version with a confirmed environment; \
                         leaving it alone"
                    );
                    return Ok(Applied::AlreadyRunning);
                }
                if current.env_confirmed {
                    warn!(
                        app_id = %key.app_id(),
                        environment = %key.environment(),
                        version_id = %order.version_id,
                        "This node has version {} of {} installed but nothing running for it: \
                         its isolate is gone. Installing it again so the application serves \
                         here",
                        order.version_id,
                        key
                    );
                } else {
                    info!(
                        app_id = %key.app_id(),
                        environment = %key.environment(),
                        version_id = %order.version_id,
                        "This version was recovered from local disk and is running without the \
                         secrets and storage addresses it declares; reloading it from the \
                         control plane's copy so that its environment is complete"
                    );
                }
            }
        }

        self.install(order, false).await
    }

    /// Load whatever this node already has on disk.
    ///
    /// This is the answer to an unreachable control plane: the node keeps
    /// serving what it was serving instead of coming up empty. The bundles on
    /// disk carry no secrets, so an app whose manifest declares any starts up
    /// without them; that is said out loud here rather than discovered from a
    /// handler that reads `context.env` and finds nothing.
    pub async fn recover_from_disk(&self) -> usize {
        let records = self.store.records().await;
        if records.is_empty() {
            return 0;
        }

        let mut recovered = 0;
        for record in records {
            let key = record.key();
            let lock = self.lock_for(&key);
            let _guard = lock.lock().await;

            if self.app_manager.installed_version(&key).is_some() {
                continue;
            }

            let order = DeploymentOrder {
                app_id: record.app_id.clone(),
                version_id: record.version_id.clone(),
                environment: record.environment.clone(),
                bundle_url: String::new(),
                bundle_sha: record.bundle_sha.clone(),
                env: HashMap::new(),
                // Not on disk, for the same reason the secrets are not: the
                // record this node keeps is a pointer, and the limits arrive
                // with the deployment. Coming back on the node's own ceilings
                // is the safe direction - they are what the operator allows,
                // and an application can only ever have asked for less - but
                // an application that narrowed its own budget loses that
                // narrowing until the control plane redelivers, which is why
                // the recovery log below says so.
                limits: None,
                // Nothing is lost here, unlike the limits above: the
                // compatibility declaration lives in the bundle's own
                // manifest, which is on this node's disk, so `install` reads
                // it back and the application comes up on the same date it
                // was deployed with. A node restart that quietly moved an
                // application onto different platform answers is exactly the
                // failure the mechanism exists to prevent, so it must not be
                // the recovery path that causes it.
                compatibility: None,
            };

            match self.install(order, true).await {
                Ok(_) => {
                    recovered += 1;
                    warn!(
                        app_id = %record.app_id,
                        version_id = %record.version_id,
                        environment = %record.environment,
                        "Recovered {} version {} from this node's own disk because the control \
                         plane could not be reached; it is serving, but any secret this app \
                         declares is missing until the control plane redelivers, because \
                         secret values are never written to disk, and it runs on this node's \
                         default ceilings rather than any narrower ones its manifest asked for",
                        record.app_id,
                        record.version_id
                    );
                }
                Err(e) => error!(
                    app_id = %record.app_id,
                    version_id = %record.version_id,
                    error = %e,
                    "Could not recover {} from local disk; it is not serving on this node until \
                     the control plane becomes reachable",
                    record.app_id
                ),
            }
        }

        recovered
    }

    /// Stop serving an application here.
    ///
    /// `environment` names one of them; `None` means all of them, which is
    /// what deleting an application from the catalog asks for - by then the
    /// rows that would name its environments are gone, and this node may still
    /// hold one the control plane has already forgotten.
    ///
    /// Returns the environments that were actually running. An application
    /// this node was not running is not a failure: there is nothing left to
    /// serve, which is the outcome the caller asked for. Its bundles are
    /// removed from disk either way, because a node that restarts without a
    /// reachable control plane loads what its disk holds.
    pub async fn unload(
        &self,
        app_id: &str,
        environment: Option<&str>,
    ) -> anyhow::Result<Vec<String>> {
        let loaded = self.app_manager.environments_of(app_id);
        let targets: Vec<AppKey> = match environment {
            Some(environment) => {
                let wanted = AppKey::new(app_id, environment);
                loaded.into_iter().filter(|key| *key == wanted).collect()
            }
            None => loaded,
        };

        let mut unloaded: Vec<String> = Vec::new();
        let mut failures: Vec<String> = Vec::new();
        for key in &targets {
            let lock = self.lock_for(key);
            let _guard = lock.lock().await;
            match self.app_manager.unload_app(key).await {
                Ok(()) => {
                    // Same reason as the stamp at the end of `install`: this
                    // is a change to what the node runs, and a list fetched
                    // before it must not put the application back.
                    self.record_change(key);
                    unloaded.push(key.environment().to_string());
                }
                Err(e) => failures.push(format!("{key}: {e}")),
            }
            // The lock stays in the map on purpose. Removing it while holding
            // it would let the next caller mint a fresh mutex and run
            // alongside whoever is still inside this one; the map holds one
            // small entry per pair this node has ever seen, which is not worth
            // that race.
        }

        match environment {
            Some(environment) => {
                self.store
                    .forget_environment(&AppKey::new(app_id, environment))
                    .await
            }
            None => self.store.forget(app_id).await,
        }

        if !failures.is_empty() {
            return Err(anyhow::anyhow!(
                "could not unload {app_id}: {}. It may still be serving on this node",
                failures.join("; ")
            ));
        }

        if unloaded.is_empty() {
            info!(
                app_id = %app_id,
                environment = %environment.unwrap_or("<all>"),
                "Asked to unload {} but this node was not running it; its bundles were removed \
                 so a restart will not bring it back",
                app_id
            );
        } else {
            info!(
                app_id = %app_id,
                environments = ?unloaded,
                "Unloaded {} environment(s) of {}: {}",
                unloaded.len(),
                app_id,
                unloaded.join(", ")
            );
        }

        Ok(unloaded)
    }

    /// The one path that ever changes what an app runs.
    ///
    /// `local_only` is the recovery case: no URL to fetch from, so the bundle
    /// must already be on disk and is used as it stands.
    async fn install(
        &self,
        mut order: DeploymentOrder,
        local_only: bool,
    ) -> Result<Applied, DeployError> {
        let key = order.key();
        // Taken before the rest of the order is consumed piece by piece below.
        let limits = order.limits.take();
        // Both halves of the key become a directory name here and a storage
        // tenant later, so they are checked together and before anything is
        // written.
        key.validate().map_err(|e| {
            DeployError::Identifier(InvalidIdentifier {
                field: e.field,
                value: e.value,
            })
        })?;
        check_identifier("version_id", &order.version_id).map_err(DeployError::Identifier)?;

        let (bytes, reused_bundle) = self.bundle_bytes(&key, &order, local_only).await?;
        let bundle_bytes = bytes.len();

        // Inflating a zip and hashing every file in it is CPU work measured in
        // whole megabytes. On the async runtime it would stall the thread it
        // lands on, and with it every request being served by that thread, so
        // it happens on the blocking pool instead.
        let Unpacked {
            manifest,
            code,
            assets,
        } = {
            let app_id = order.app_id.clone();
            let cache_control = self.cache_control.clone();
            let max_asset_bytes = self.max_asset_bytes;
            tokio::task::spawn_blocking(move || {
                unpack(&bytes, &app_id, cache_control, max_asset_bytes)
            })
            .await
            .map_err(|e| {
                DeployError::Storage(format!("unpacking the bundle did not finish: {e}"))
            })?
        }?;

        if assets.duplicate_paths() > 0 {
            warn!(
                app_id = %key.app_id(),
                environment = %key.environment(),
                version_id = %order.version_id,
                duplicates = assets.duplicate_paths(),
                "The manifest names the same asset path more than once; the first entry wins \
                 and the rest are unreachable"
            );
        }
        if assets.undeclared_files() > 0 {
            warn!(
                app_id = %key.app_id(),
                environment = %key.environment(),
                version_id = %order.version_id,
                undeclared = assets.undeclared_files(),
                "The bundle carries files under assets/ that its manifest does not list; they \
                 cost bundle size and can never be served"
            );
        }

        // The bundle's own manifest is the authority on which of the
        // platform's answers this version expects. It has to be: it is inside
        // the archive whose digest this node just verified, it is what the
        // control plane read, and it is still here on the recovery path where
        // no order was ever received. A version's semantics belong to the
        // version, not to the message that happened to deliver it.
        //
        // Refused rather than defaulted when it cannot be resolved. Every
        // answer this node could pick for an application whose declaration it
        // does not understand is an answer that application did not ask for,
        // and the node that quietly picks one is the node nobody can debug.
        let compatibility = manifest.compatibility_profile().map_err(|e| {
            error!(
                app_id = %key.app_id(),
                environment = %key.environment(),
                version_id = %order.version_id,
                error = %e,
                "This version's manifest declares a compatibility date or behaviour flag \
                 this build of the node cannot honour, so the deployment is refused and this \
                 environment keeps running whatever it was running. Upgrade this node, or \
                 rebuild the application against a date this build knows"
            );
            DeployError::Compatibility(e)
        })?;

        // What the control plane said it was sending, against what the bundle
        // actually carries. They can only differ if the two read different
        // manifests for one version id, which is a platform fault rather than
        // an application one - and one that would otherwise show up as an
        // application behaving unlike what `deis versions` prints.
        if order
            .compatibility
            .is_some_and(|delivered| delivered != compatibility)
        {
            let delivered = order
                .compatibility
                .expect("just checked that the order carries one");
            error!(
                app_id = %key.app_id(),
                environment = %key.environment(),
                version_id = %order.version_id,
                delivered = %delivered.describe(),
                in_bundle = %compatibility.describe(),
                "The control plane and this version's bundle disagree about which platform \
                 behaviour it was written against; the bundle wins, because that is what the \
                 digest covers. The control plane's own listing of this version is wrong until \
                 the version is uploaded again"
            );
        }

        match compatibility.date() {
            Some(date) => info!(
                app_id = %key.app_id(),
                environment = %key.environment(),
                version_id = %order.version_id,
                compatibility_date = %date,
                compatibility = %compatibility.describe(),
                "Installing this version under the platform behaviour of {}: {}",
                date,
                compatibility.describe()
            ),
            None => warn!(
                app_id = %key.app_id(),
                environment = %key.environment(),
                version_id = %order.version_id,
                "This version's manifest names no compatibility date, so it is held to the \
                 oldest behaviour this platform has and no later fix that changes an observable \
                 behaviour will reach it. Add \"compatibilityDate\" to the project and redeploy"
            ),
        }

        // The control plane already merges manifest variables with decrypted
        // secrets, so its map wins. The manifest's own variables are the base
        // so that a bundle recovered from disk still has them.
        let mut env: HashMap<String, String> = manifest
            .env
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        // A bundle may not name a reserved platform key. deis-control drops
        // these when it assembles a delivery, but a bundle recovered from this
        // node's own disk never went through it, and a value left here would
        // be read below as the address to build this app's storage clients
        // against - letting the application pick where its own data goes.
        // The values are dropped unlogged: they are the app's own text and one
        // of these names can just as well hold a secret.
        let declared_reserved = crate::runtime::strip_reserved_env(&mut env);
        if !declared_reserved.is_empty() {
            warn!(
                app_id = %key.app_id(),
                environment = %key.environment(),
                version_id = %order.version_id,
                keys = ?declared_reserved,
                "This bundle's manifest declares {} variable(s) named like reserved platform \
                 keys; they are dropped, so the application never sees them and cannot point \
                 its storage clients at a host of its own choosing",
                declared_reserved.len()
            );
        }
        let declared_secrets = manifest.secrets.len();
        env.extend(order.env);

        if local_only && declared_secrets > 0 {
            warn!(
                app_id = %key.app_id(),
                environment = %key.environment(),
                secrets = declared_secrets,
                "This app declares {} secret(s) and is being loaded from disk, where secret \
                 values are deliberately never stored; handlers reading them will find them \
                 missing until the control plane redelivers this deployment",
                declared_secrets
            );
        }

        // The two halves of the storage wiring meet here. The control plane
        // knows the addresses and puts them in the deployment env; only the
        // bundle's manifest says which services this app declared and which
        // queues it consumes. Without this call the reserved keys are
        // incomplete, no storage client is ever built, and every
        // `ctx.services.*` call answers "service not available".
        crate::runtime::wire_deployment_env(&manifest, &mut env);
        let wired_services = env
            .get(crate::runtime::SERVICES_KEY)
            .cloned()
            .unwrap_or_default();
        if !wired_services.is_empty()
            && !env.contains_key(crate::runtime::STORE_ENDPOINT_KEY)
            && !env.contains_key(crate::runtime::QUEUE_ENDPOINT_KEY)
        {
            warn!(
                app_id = %key.app_id(),
                environment = %key.environment(),
                version_id = %order.version_id,
                services = %wired_services,
                "This deployment carries no storage addresses, so the services this app \
                 declares cannot be built; every ctx.services call it makes will fail until \
                 the control plane is configured with store_endpoint / queue_endpoint"
            );
        }

        let config = AppConfig {
            env,
            routes: manifest.route_patterns(),
            // What the order carried, with anything it left out taken from
            // the version's own manifest; unjudged either way.
            // `install_version` builds the isolate through
            // `effective_limits`, which clamps each field against this node's
            // own ceilings and says so in the log when an application asked
            // for more than it may have.
            limits: limits_with_manifest(limits, &manifest, &key),
            // Resolved once, here, and asked as a bit test on every request
            // from here on; see `deis_bundle::compatibility`.
            compatibility,
            ..Default::default()
        };

        info!(
            app_id = %key.app_id(),
            environment = %key.environment(),
            version_id = %order.version_id,
            bundle_bytes,
            reused_bundle,
            assets = assets.len(),
            routes = config.routes.len(),
            env_entries = config.env.len(),
            "Installing a deployment"
        );

        self.app_manager
            .install_version(VersionInstall {
                key: key.clone(),
                version_id: order.version_id.clone(),
                entry_point: manifest.entry.clone(),
                code,
                config,
                // A local recovery reconstructs everything except the secrets,
                // which are never on disk. Saying so here is what lets the
                // next delivery of this same version be recognised as the one
                // that completes it, instead of being skipped as redundant.
                env_confirmed: !local_only,
                assets: Arc::new(assets),
            })
            .await
            .map_err(|e| DeployError::Load(e.to_string()))?;

        if !local_only {
            self.store
                .remember(InstalledRecord {
                    app_id: order.app_id.clone(),
                    version_id: order.version_id.clone(),
                    environment: order.environment.clone(),
                    bundle_sha: order.bundle_sha.clone(),
                })
                .await;
        }
        self.store.prune(&key, &order.version_id).await;

        // Stamped after the swap, so a reconciliation that reads the stamp
        // before it asks the control plane anything can tell that this
        // environment moved under it.
        self.record_change(&key);

        Ok(Applied::Installed { reused_bundle })
    }

    /// Get the bundle's bytes, downloading only if the node does not already
    /// hold exactly these bytes.
    async fn bundle_bytes(
        &self,
        key: &AppKey,
        order: &DeploymentOrder,
        local_only: bool,
    ) -> Result<(Vec<u8>, bool), DeployError> {
        let path = self
            .store
            .bundle_path(key, &order.version_id)
            .map_err(DeployError::Identifier)?;

        if path.exists() {
            match self.store.read_bundle(&path).await {
                Ok(bytes) => {
                    // Hashing a whole bundle is CPU work; keep it off the
                    // thread that is also serving requests. The bytes move
                    // into the blocking task and come back with the digest, so
                    // a large bundle is never held twice.
                    let (bytes, digest) = match tokio::task::spawn_blocking(move || {
                        let digest = sha256_hex(&bytes);
                        (bytes, digest)
                    })
                    .await
                    {
                        Ok(pair) => pair,
                        Err(e) => {
                            return Err(DeployError::Storage(format!(
                                "checking the bundle already on disk did not finish: {e}"
                            )))
                        }
                    };
                    if digest.eq_ignore_ascii_case(&order.bundle_sha) {
                        debug!(
                            app_id = %key.app_id(),
                            environment = %key.environment(),
                            version_id = %order.version_id,
                            "Reusing the bundle already on disk; its digest matches"
                        );
                        return Ok((bytes, true));
                    }
                    warn!(
                        app_id = %key.app_id(),
                        environment = %key.environment(),
                        version_id = %order.version_id,
                        expected = %order.bundle_sha,
                        actual = %digest,
                        "The bundle on disk for this version does not hash to what the control \
                         plane says; it is being discarded and downloaded again"
                    );
                }
                Err(e) => warn!(
                    path = %path.display(),
                    error = %e,
                    "Could not read the bundle already on disk; downloading it again"
                ),
            }
        }

        if local_only {
            return Err(DeployError::Storage(format!(
                "no verified bundle for {} version {} on this node's disk, and there is no \
                 control plane to fetch one from",
                key, order.version_id
            )));
        }

        let temp = self.store.temp_path();
        let fetched = self
            .fetcher
            .fetch(&order.bundle_url, &order.bundle_sha, &temp)
            .await
            .map_err(DeployError::Fetch)?;

        let stored = self
            .store
            .adopt(&fetched.path, key, &order.version_id)
            .await
            .map_err(|e| DeployError::Storage(e.to_string()))?;

        let bytes = self
            .store
            .read_bundle(&stored)
            .await
            .map_err(|e| DeployError::Storage(e.to_string()))?;

        Ok((bytes, false))
    }
}

/// The ceilings a version is held to: what the deployment carried, with every
/// field it left out taken from the version's own manifest.
///
/// Two sources for one thing, which is worth explaining. The `limits` block is
/// written by the application's author, inside the bundle. The control plane
/// reads it there, checks it and relays it with the deployment so that a node
/// has the numbers before it has unpacked anything - but by the time this runs
/// the bundle *is* unpacked and its manifest is in hand, and the two can only
/// ever say the same thing: the bundle's sha is checked, so this is the same
/// file the control plane read.
///
/// So the manifest fills gaps and never overrides. A control plane older than
/// this build - or one whose wire message has no field for a limit this build
/// understands - carries nothing for it, and without this an application's
/// declaration would silently not be in force: the node would apply its own
/// default, the manifest would go on saying otherwise, and the first anybody
/// would know is the day the ceiling was needed. `max_subrequests` is exactly
/// that case today, because `deis.management.ResourceLimits` has no field for
/// it.
///
/// Nothing is judged here. Whether a value is one this node will grant is
/// `crate::app::ApplicationManager::effective_limits`'s decision, made once,
/// where the node's own configuration is in scope.
fn limits_with_manifest(
    carried: Option<ResourceLimits>,
    manifest: &deis_bundle::Manifest,
    key: &AppKey,
) -> Option<ResourceLimits> {
    let Some(declared) = manifest.limits.as_ref() else {
        return carried;
    };

    let mut limits = carried.unwrap_or_default();
    let mut filled: Vec<&str> = Vec::new();

    // `memory_mb` uses 0 for "unset" because the wire message does; the rest
    // are already `Option`.
    if let Some(mb) = declared.memory_mb.filter(|_| limits.memory_mb == 0) {
        limits.memory_mb = mb as usize;
        filled.push("memory_mb");
    }
    if limits.timeout_ms.is_none() && declared.timeout_ms.is_some() {
        limits.timeout_ms = declared.timeout_ms;
        filled.push("timeout_ms");
    }
    if limits.cpu_time_limit_ms.is_none() && declared.cpu_time_limit_ms.is_some() {
        limits.cpu_time_limit_ms = declared.cpu_time_limit_ms;
        filled.push("cpu_time_limit_ms");
    }
    if limits.rate_limit_ops_per_sec.is_none() && declared.rate_limit_ops_per_sec.is_some() {
        limits.rate_limit_ops_per_sec = declared.rate_limit_ops_per_sec;
        filled.push("rate_limit_ops_per_sec");
    }
    if let Some(max) = declared
        .resource_table_max
        .filter(|_| limits.resource_table_max.is_none())
    {
        limits.resource_table_max = Some(max as usize);
        filled.push("resource_table_max");
    }
    if limits.max_subrequests.is_none() && declared.max_subrequests.is_some() {
        limits.max_subrequests = declared.max_subrequests;
        filled.push("max_subrequests");
    }

    if !filled.is_empty() {
        debug!(
            app_id = %key.app_id(),
            environment = %key.environment(),
            fields = ?filled,
            "This deployment carried no value for {} limit(s) the version's manifest declares; \
             they are read out of the bundle instead, so what the author wrote is what the node \
             clamps against its own ceilings",
            filled.len()
        );
    }

    Some(limits)
}

/// A bundle taken apart into the three things a version needs.
struct Unpacked {
    manifest: deis_bundle::Manifest,
    code: String,
    assets: AssetBundle,
}

/// Read a verified bundle: manifest, entry module, static files.
///
/// Runs on the blocking pool. Every failure here is a rejection of the bundle
/// as a whole, and the caller has not touched the running app yet.
fn unpack(
    bytes: &[u8],
    app_id: &str,
    cache_control: Arc<str>,
    max_asset_bytes: u64,
) -> Result<Unpacked, DeployError> {
    let parsed = deis_bundle::read_manifest(bytes).map_err(DeployError::Bundle)?;
    parsed
        .manifest
        .check_app(app_id)
        .map_err(DeployError::Bundle)?;
    let manifest = parsed.manifest;

    let mut archive = zip::ZipArchive::new(Cursor::new(bytes))
        .map_err(|e| DeployError::Bundle(deis_bundle::BundleError::NotAnArchive(e.to_string())))?;

    let code = read_entry(&mut archive, &manifest.entry)?;
    let assets = AssetBundle::from_archive(&manifest, &mut archive, cache_control, max_asset_bytes)
        .map_err(DeployError::Assets)?;

    Ok(Unpacked {
        manifest,
        code,
        assets,
    })
}

/// Read the module the manifest names as the entry point.
fn read_entry<R: Read + std::io::Seek>(
    archive: &mut zip::ZipArchive<R>,
    entry: &str,
) -> Result<String, DeployError> {
    let mut file = archive.by_name(entry).map_err(|_| {
        DeployError::Bundle(deis_bundle::BundleError::InvalidManifest(format!(
            "entry {entry:?} is not in the archive"
        )))
    })?;
    let mut code = String::new();
    file.read_to_string(&mut code).map_err(|e| {
        DeployError::Bundle(deis_bundle::BundleError::Malformed(format!(
            "entry {entry:?} is not valid UTF-8 and cannot be a JavaScript module: {e}"
        )))
    })?;
    Ok(code)
}

#[cfg(test)]
#[path = "deployment_test.rs"]
mod deployment_test;
