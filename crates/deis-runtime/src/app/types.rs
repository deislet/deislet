use serde::{Deserialize, Serialize};
use std::time::SystemTime;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum AppState {
    Loading,
    Running,
    Stopped,
    Error,
}

#[derive(Clone, Serialize, Deserialize, Default)]
pub struct AppConfig {
    /// What the app sees as `context.env`: the plain variables from its
    /// manifest merged with the secret values the control plane decrypted at
    /// delivery time.
    ///
    /// The values are secrets. They are never serialised (hence
    /// `skip_serializing`) and never printed (hence the hand-written `Debug`
    /// below), because this type is reachable from anything that logs or
    /// returns an [`App`].
    #[serde(default, skip_serializing)]
    pub env: std::collections::HashMap<String, String>,

    pub limits: Option<ResourceLimits>,

    /// Which of the platform's answers this application gets, resolved once
    /// from its bundle manifest when the version was installed.
    ///
    /// Not a date and not a list of names: the comparison against every
    /// behaviour's own date happens once, in
    /// `deis_bundle::compatibility::CompatibilityProfile::resolve`, and what
    /// is kept here is the outcome. Everything downstream asks
    /// `is_on(Flag::X)`, which is a bit test, so no part of the request path
    /// ever compares dates.
    ///
    /// The default is the oldest behaviour, every flag off. That is what an
    /// application deployed before any of this existed was written against,
    /// and it is deliberately also what a path that forgot to carry a profile
    /// would produce: the direction that cannot change an application's
    /// behaviour underneath it.
    #[serde(default)]
    pub compatibility: deis_bundle::CompatibilityProfile,

    #[serde(default)]
    pub resources: Vec<String>,

    #[serde(default)]
    pub routes: Vec<String>,

    #[serde(default)]
    pub permissions: AppPermissions,
}

/// Prints the shape of the configuration and the *names* of the environment
/// entries, never a value. One `{:?}` on an `App` somewhere in the crate is
/// all it would take to put every decrypted secret in the node's log and,
/// from there, in deis-telemetry.
impl std::fmt::Debug for AppConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut names: Vec<&str> = self.env.keys().map(String::as_str).collect();
        names.sort_unstable();
        f.debug_struct("AppConfig")
            .field("env", &format_args!("{names:?} = <redacted>"))
            .field("limits", &self.limits)
            .field("compatibility", &self.compatibility.describe())
            .field("resources", &self.resources)
            .field("routes", &self.routes)
            .field("permissions", &self.permissions)
            .finish()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AppPermissions {
    #[serde(default)]
    pub allow_net: Vec<String>,

    #[serde(default)]
    pub allow_env: Vec<String>,
}

/// What one application asked to be held to, and never what it was granted.
///
/// These arrive from the application's own bundle manifest: the control plane
/// reads the `limits` block out of it, sends it with the deployment
/// (`ApplyDeployment.limits`, and the same shape in the assignment list), and
/// this node stores it on the app's [`AppConfig`]. A manifest is written by
/// the tenant, so **every field narrows only**: the node's configuration is
/// the ceiling, and
/// `crate::app::manager::ApplicationManager::effective_limits` clamps each
/// value against it and warns when an application asked for more.
///
/// Deserialised straight from the assignment list's JSON, which is why the
/// field names here are the manifest's own spelling and why every field has a
/// `serde` default.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ResourceLimits {
    /// Narrowing override of `crate::config::RuntimeConfig`'s
    /// `default_memory_limit_mb`: the V8 heap ceiling this app's isolate runs
    /// with, in MB.
    ///
    /// `0` means "no override" -- the app is held to the node's default. It is
    /// a sentinel rather than an `Option` because this struct arrives over the
    /// wire in an `ApplyDeployment`, where an absent field and a zero are the
    /// same thing to every producer, and because a heap of 0 MB has no useful
    /// reading of its own: it would refuse the app before it ran a line.
    ///
    /// A value *above* the node default is refused and the node default is
    /// used instead. The ceiling protects the node's memory, which every other
    /// tenant on it is sharing, so it narrows only -- the same direction
    /// `timeout_ms` and `rate_limit_ops_per_sec` are meant to move in.
    #[serde(default)]
    pub memory_mb: usize,

    /// A CPU *share* in Kubernetes millicores, carried on the wire since the
    /// first version of this message and read by nothing on this node.
    ///
    /// Nothing enforces it and nothing is planned to: a share only means
    /// something to a scheduler that can hand out fractions of a core, and an
    /// isolate here is pinned to one runtime thread that its application has
    /// to itself. What one invocation may cost the node is
    /// `cpu_time_limit_ms` below, which is a ceiling on time consumed rather
    /// than a share of a core, and which is enforced. Named as dead here
    /// rather than removed because it is field 2 of the `ResourceLimits`
    /// message and older control planes still send it.
    #[serde(default)]
    pub cpu_milli: u32,

    #[serde(default)]
    pub resource_table_max: Option<usize>,

    /// Narrowing override of `crate::config::ExecutionLimits::timeout_ms`: the
    /// wall-clock ceiling on one invocation of this app. `None` means "use the
    /// platform default".
    ///
    /// Narrowing only, like every other field here. An invocation holds the
    /// runtime thread it is on for its whole duration, and every other
    /// application queued behind that thread waits with it, so how long one
    /// may run is the node's number to set. A value above the node's is
    /// refused and the node's is applied.
    #[serde(default)]
    pub timeout_ms: Option<u64>,

    /// Narrowing override of
    /// `crate::config::ExecutionLimits::rate_limit_ops_per_sec`: how fast this
    /// app's code may dispatch ops. `None` means "use the platform default".
    ///
    /// Narrowing only. Every op dispatch is work the node does on the app's
    /// behalf, so a value above the node's is refused and the node's is
    /// applied.
    #[serde(default)]
    pub rate_limit_ops_per_sec: Option<f64>,

    /// Narrowing override of `crate::config::ExecutionLimits::cpu_time_limit_ms`:
    /// how much CPU time one invocation of this app may consume before its
    /// isolate is terminated. `None` means "use the platform default".
    ///
    /// Narrowing only, like `memory_mb` and for the same reason. An
    /// application asking for *more* CPU than the node allows is asking to
    /// take more of a core away from every other tenant on the box, so a value
    /// above the node's is refused and the node's is applied -- see
    /// `crate::app::manager::ApplicationManager::effective_limits`. An
    /// application asking for less is welcome to: a handler that knows it is
    /// IO-bound and sets 200ms gets told at once when a change makes it
    /// compute instead.
    #[serde(default)]
    pub cpu_time_limit_ms: Option<u64>,

    /// Narrowing override of `crate::config::ExecutionLimits::max_subrequests`:
    /// how many outbound calls one invocation of this app may make. `None`
    /// means "use the platform default".
    ///
    /// Narrowing only, and this is the field where that matters most
    /// obviously. The ceiling exists to stop one application's fan-out from
    /// taking the node's outbound capacity away from every other tenant on it,
    /// so an application that could raise its own would be deciding how much
    /// of a shared resource it may have. An application that narrows it is
    /// buying something for itself instead: a handler that should call two
    /// upstreams and starts calling twenty fails loudly on the change that
    /// introduced it.
    #[serde(default)]
    pub max_subrequests: Option<u32>,
}

/// One application, in one environment, as this node holds it.
///
/// `id` and `environment` are two halves of one identity (see
/// [`crate::app::AppKey`]): the same `id` appears once per environment this
/// node runs, and the pair is what the manager keys on.
#[derive(Debug, Clone, Serialize)]
pub struct App {
    pub id: String,
    /// Environment *name* (`production` or a preview name), which is what
    /// application code branches on and what separates this copy from the
    /// other environments of the same application.
    pub environment: String,
    pub state: AppState,
    pub config: AppConfig,
    pub created_at: SystemTime,
    pub updated_at: SystemTime,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}
