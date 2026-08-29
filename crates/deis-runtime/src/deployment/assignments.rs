//! Principle 8: the node asks what it should be running.
//!
//! On startup this node calls `GET /api/nodes/{node_id}/assignments` on the
//! control plane, fetches and loads every version it names, and only then
//! reports healthy. The direction is deliberate. The control plane does not
//! have to know how to reach a node before that node can run anything; a node
//! that comes back from a reboot puts itself back to work on its own; and
//! adding a node is one process start rather than an edit to the control
//! plane's configuration.
//!
//! The same question is asked again on a timer, and the answer is treated as
//! the whole truth rather than as a set of additions: an environment this node
//! runs and the list does not name stops here. That is the only way a deletion
//! ever reaches a node that was unreachable when it happened, because a
//! deletion has nothing left to push. The control plane says on each answer
//! whether it *is* the whole truth ([`AssignmentList::complete`]); an answer
//! that cannot promise it is loaded from and never removed on.
//!
//! The bundle itself is never in the list: each line is a pointer - app,
//! version, environment, URL, digest - plus the environment map that a push
//! would have carried, so an app that comes back this way comes back with its
//! secrets and its storage addresses rather than with an empty environment.
//! An older control plane that sends no `env` still works; the app then runs
//! on its manifest's own variables until the next push, which is what this
//! path used to do for every app and is said out loud when it happens.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::Deserialize;
use tracing::{debug, error, info, warn};

use super::credential::NodeCredential;
use super::registration::{NodeRegistrar, Registered};
use super::{Applied, DeploymentManager, DeploymentOrder};
use crate::app::{AppKey, InstalledVersion};
use crate::health::NodeHealth;

/// One line of `GET /api/nodes/{node_id}/assignments`, in exactly the shape
/// the control plane serves (see deis-control `api::catalog::assignments`).
#[derive(Clone, Deserialize)]
pub struct Assignment {
    pub app_id: String,
    pub version_id: String,
    pub environment: String,
    pub bundle_url: String,
    pub bundle_sha: String,
    /// Variables, decrypted secrets and platform wiring, exactly as a push
    /// would have sent them. Absent from an older control plane.
    #[serde(default)]
    pub env: std::collections::HashMap<String, String>,
}

/// Hand-written so a `{:?}` on an assignment - in a log line, in an error
/// path - cannot print the secrets the control plane just decrypted.
impl std::fmt::Debug for Assignment {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut names: Vec<&str> = self.env.keys().map(String::as_str).collect();
        names.sort_unstable();
        f.debug_struct("Assignment")
            .field("app_id", &self.app_id)
            .field("version_id", &self.version_id)
            .field("environment", &self.environment)
            .field("bundle_url", &self.bundle_url)
            .field("bundle_sha", &self.bundle_sha)
            .field("env", &format_args!("{names:?} = <redacted>"))
            .finish()
    }
}

impl Assignment {
    fn into_order(self) -> DeploymentOrder {
        DeploymentOrder {
            app_id: self.app_id,
            version_id: self.version_id,
            environment: self.environment,
            bundle_url: self.bundle_url,
            bundle_sha: self.bundle_sha,
            env: self.env,
        }
    }
}

/// Why the assignment list could not be obtained.
#[derive(Debug)]
pub enum AssignmentError {
    Transport { url: String, reason: String },
    Status { url: String, code: u16 },
    Malformed { url: String, reason: String },
}

impl std::fmt::Display for AssignmentError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Transport { url, reason } => {
                write!(f, "could not reach the control plane at {url}: {reason}")
            }
            Self::Status { url, code } => {
                write!(f, "the control plane answered HTTP {code} for {url}")
            }
            Self::Malformed { url, reason } => write!(
                f,
                "the assignment list from {url} is not in the expected shape: {reason}"
            ),
        }
    }
}

impl std::error::Error for AssignmentError {}

/// Asks the control plane what this node should run.
pub struct AssignmentClient {
    client: reqwest::Client,
    url: String,
    credential: Option<NodeCredential>,
}

impl AssignmentClient {
    /// `credential` is the node key from `[control] secret`. Without it the
    /// request is anonymous, which a control plane with `node_preshared_keys`
    /// configured answers with 401 - it will not hand this node's decrypted
    /// secrets to a caller that cannot prove it is a node.
    pub fn new(
        endpoint: &str,
        node_id: &str,
        timeout: Duration,
        credential: Option<NodeCredential>,
    ) -> anyhow::Result<Self> {
        let client = reqwest::Client::builder().timeout(timeout).build()?;
        let url = format!(
            "{}{}",
            endpoint.trim_end_matches('/'),
            deis_wire::conventions::node_assignments_path(&urlencode(node_id))
        );
        Ok(Self {
            client,
            url,
            credential,
        })
    }

    pub fn url(&self) -> &str {
        &self.url
    }

    pub async fn fetch(&self) -> Result<AssignmentList, AssignmentError> {
        let mut request = self.client.get(&self.url);
        if let Some(credential) = &self.credential {
            request = request.header(reqwest::header::AUTHORIZATION, credential.header_value());
        }

        let response = request
            .send()
            .await
            .map_err(|e| AssignmentError::Transport {
                url: self.url.clone(),
                reason: e.to_string(),
            })?;

        if !response.status().is_success() {
            let code = response.status().as_u16();
            if code == 401 || code == 403 {
                let has_credential = self.credential.is_some();
                error!(
                    url = %self.url,
                    code,
                    has_credential,
                    "The control plane refused this node's credential, so this node gets no \
                     assignment list at all: it keeps serving whatever it already had and picks \
                     up no new version. Set control.secret to one of the control plane's \
                     node_preshared_keys"
                );
            }
            return Err(AssignmentError::Status {
                url: self.url.clone(),
                code,
            });
        }

        // Read before the body is consumed. Absence is deliberately read as
        // "not complete": a control plane older than the header cannot tell
        // this node whether its list is the whole truth, and acting on a
        // short list as though it were would unload applications that are
        // running perfectly well.
        let complete = response
            .headers()
            .get(ASSIGNMENTS_COMPLETE_HEADER)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| value.eq_ignore_ascii_case("true"));
        let withheld = response
            .headers()
            .get(ASSIGNMENTS_WITHHELD_HEADER)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.trim().parse::<u32>().ok())
            .unwrap_or(0);

        let assignments =
            response
                .json::<Vec<Assignment>>()
                .await
                .map_err(|e| AssignmentError::Malformed {
                    url: self.url.clone(),
                    reason: e.to_string(),
                })?;

        Ok(AssignmentList {
            assignments,
            complete,
            withheld,
        })
    }
}

use deis_wire::conventions::{ASSIGNMENTS_COMPLETE_HEADER, ASSIGNMENTS_WITHHELD_HEADER};

/// One answer from the control plane.
///
/// The array on its own cannot be acted on for removal: an application that is
/// absent because it was deleted and one that is absent because this control
/// plane could not build its row look identical. `complete` is how the control
/// plane tells the two apart, and a node that removes on an incomplete answer
/// turns somebody else's problem into an outage for code that is running fine.
#[derive(Debug, Clone)]
pub struct AssignmentList {
    pub assignments: Vec<Assignment>,
    /// True only when the control plane said so. An answer with no such
    /// header - a control plane older than it - is treated as incomplete, so
    /// this node loads from it and removes nothing.
    pub complete: bool,
    /// Assignments the control plane left out, when `complete` is false.
    pub withheld: u32,
}

/// Percent-encode the few characters that would otherwise change which path a
/// node id addresses. Node ids are hostnames in practice, but an id with a `/`
/// in it would silently ask the control plane a different question.
///
/// Shared with [`super::registration`], which builds a sibling URL from the
/// same id: two spellings of this would let a node register under one path and
/// fetch its assignments from another.
pub(super) fn urlencode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// Result of one pass over the assignment list.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SyncOutcome {
    pub assigned: usize,
    pub installed: usize,
    pub unchanged: usize,
    pub failed: usize,
    /// Environments this node was running and the control plane no longer
    /// assigns, which this pass stopped.
    pub unloaded: usize,
    /// Environments that should have been stopped and could not be. They are
    /// still serving, which is the half of a reconciliation that leaves the
    /// node further from the list than it started.
    pub unload_failed: usize,
}

/// Fetch the assignment list and make this node match it.
///
/// The answer is the whole truth, not a set of additions: an environment this
/// node runs and the list does not name is stopped here. Both halves matter
/// and only one of them used to happen. An application deleted from the
/// catalog, an environment torn down, a node taken out of an application's
/// placement - none of those reach a node as an instruction, because the
/// control plane's record of them is a row that no longer exists. A node that
/// only ever added would keep serving them, with their secrets, until somebody
/// restarted the process.
///
/// It is also what makes the answer safe to act on repeatedly: running this
/// on a node that is already correct changes nothing and says nothing.
pub async fn reconcile_once(
    client: &AssignmentClient,
    deployer: &DeploymentManager,
) -> Result<SyncOutcome, AssignmentError> {
    let AssignmentList {
        assignments,
        complete,
        withheld,
    } = client.fetch().await?;

    let assigned: HashMap<AppKey, String> = assignments
        .iter()
        .map(|assignment| {
            (
                AppKey::new(&assignment.app_id, &assignment.environment),
                assignment.version_id.clone(),
            )
        })
        .collect();
    let running = deployer.running();
    report_drift(&assigned, &running, complete);

    let mut outcome = SyncOutcome {
        assigned: assignments.len(),
        ..Default::default()
    };

    for assignment in assignments {
        let app_id = assignment.app_id.clone();
        let environment = assignment.environment.clone();
        let version_id = assignment.version_id.clone();
        match deployer
            .apply_from_assignment(assignment.into_order())
            .await
        {
            Ok(Applied::Installed { reused_bundle }) => {
                outcome.installed += 1;
                info!(
                    app_id = %app_id,
                    environment = %environment,
                    version_id = %version_id,
                    reused_bundle,
                    "Loaded an assigned version"
                );
            }
            Ok(Applied::AlreadyRunning) => outcome.unchanged += 1,
            Err(e) => {
                outcome.failed += 1;
                error!(
                    app_id = %app_id,
                    environment = %environment,
                    version_id = %version_id,
                    error = %e,
                    "Could not load assigned version {} of {} environment {}; that environment \
                     is not serving on this node",
                    version_id,
                    app_id,
                    environment
                );
            }
        }
    }

    if !complete {
        // The control plane says it could not describe every assignment it
        // holds, so absence from this list is not an instruction: an
        // application missing because the control plane failed to build its
        // row looks exactly like one that was deleted. Loading is still
        // right - everything listed is real.
        warn!(
            url = client.url(),
            withheld,
            listed = outcome.assigned,
            "The control plane marked this assignment list incomplete ({} assignment(s) it \
             could not describe), so nothing is stopped on the strength of it. This node loads \
             what is listed and keeps everything else running; anything that should have been \
             stopped stays up until the control plane can answer in full",
            withheld
        );
        return Ok(outcome);
    }

    for (key, installed) in running {
        if assigned.contains_key(&key) {
            continue;
        }
        match deployer.unload(key.app_id(), Some(key.environment())).await {
            Ok(_) => {
                outcome.unloaded += 1;
                info!(
                    app_id = %key.app_id(),
                    environment = %key.environment(),
                    version_id = %installed.version_id,
                    "Stopped serving {} here: the control plane's assignment list no longer \
                     names it, so this node was running a version nobody asked it for. Its \
                     bundles were removed, so a restart will not bring it back",
                    key
                );
            }
            Err(e) => {
                outcome.unload_failed += 1;
                error!(
                    app_id = %key.app_id(),
                    environment = %key.environment(),
                    version_id = %installed.version_id,
                    error = %e,
                    "{} is no longer assigned to this node and could not be stopped; it keeps \
                     serving, with its secrets, until this node is restarted",
                    key
                );
            }
        }
    }

    Ok(outcome)
}

/// Say what the control plane's answer changes, before anything acts on it.
///
/// At info, because a node that quietly loads and unloads applications on a
/// timer is a node whose behaviour nobody can reconstruct afterwards. Silence
/// when nothing changed, which is the normal case: the interesting line is the
/// one that appears.
fn report_drift(
    assigned: &HashMap<AppKey, String>,
    running: &[(AppKey, InstalledVersion)],
    complete: bool,
) {
    let running_versions: HashMap<&AppKey, &str> = running
        .iter()
        .map(|(key, installed)| (key, installed.version_id.as_str()))
        .collect();

    let mut added: Vec<String> = Vec::new();
    let mut changed: Vec<String> = Vec::new();
    for (key, version_id) in assigned {
        match running_versions.get(key) {
            None => added.push(format!("{key}={version_id}")),
            Some(running_version) if *running_version != version_id => {
                changed.push(format!("{key}: {running_version} -> {version_id}"))
            }
            Some(_) => {}
        }
    }

    // Only when the answer is the whole truth. On an incomplete one nothing
    // is removed, so listing candidates would report a change that is not
    // going to happen.
    let removed: Vec<String> = if complete {
        running
            .iter()
            .filter(|(key, _)| !assigned.contains_key(key))
            .map(|(key, installed)| format!("{key}={}", installed.version_id))
            .collect()
    } else {
        Vec::new()
    };

    if added.is_empty() && changed.is_empty() && removed.is_empty() {
        debug!(
            assigned = assigned.len(),
            "This node already runs exactly what the control plane assigns it"
        );
        return;
    }

    added.sort();
    changed.sort();
    let mut removed = removed;
    removed.sort();

    info!(
        added = ?added,
        changed = ?changed,
        removed = ?removed,
        "The control plane's assignment list differs from what this node runs: {} to load, {} \
         to move to another version, {} to stop because the list no longer names them",
        added.len(),
        changed.len(),
        removed.len()
    );

    if !removed.is_empty() && assigned.is_empty() {
        // The shape of a control plane that lost its catalog rather than of
        // somebody deleting things: acting on it takes every application off
        // this node at once.
        warn!(
            removed = removed.len(),
            "The control plane assigns this node nothing at all while it runs {} environment(s); \
             every one of them is about to stop serving here. If that was not intended, the \
             control plane's catalog is what to look at",
            removed.len()
        );
    }
}

/// Whether this node has told the control plane it exists.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RegistrationState {
    /// Not announced yet, or announced and then contact was lost. The next
    /// pass says it again.
    Pending,
    /// The control plane has it, until `refresh_at`.
    ///
    /// A registration is a lease, not a fact: deis-control drops a node that
    /// has not refreshed within its TTL, and a dropped node stops receiving
    /// deployment pushes and stops being picked for cron while still serving
    /// every request perfectly. Nothing looks wrong, which is why the deadline
    /// is carried here rather than the node registering once and assuming it
    /// is known forever.
    Done { refresh_at: Instant },
    /// This control plane has no registration endpoint, so there is nothing
    /// to keep trying. Said once and then left alone.
    NotSupported,
}

impl RegistrationState {
    /// How long the loop may sleep before this node must register again.
    ///
    /// `None` when no deadline applies: nothing to refresh, or a control plane
    /// that does not do registration at all.
    fn time_until_refresh(&self, now: Instant) -> Option<Duration> {
        match self {
            Self::Done { refresh_at } => Some(refresh_at.saturating_duration_since(now)),
            Self::Pending | Self::NotSupported => None,
        }
    }
}

/// Keep this node matching what the control plane says, for as long as the
/// process runs.
///
/// Three things happen on every pass and in this order:
///
/// 1. **Register**, if the control plane has not been told about this node
///    since the last time contact was lost. A control plane that restarted
///    with an empty node list learns about this node again without anybody
///    touching either side.
/// 2. **Reconcile**: fetch the assignment list and make this node match it,
///    loading what is missing and stopping what is no longer assigned.
/// 3. **Report** readiness, so "serving what it was told to" and "serving
///    something older, or something nobody asked for" never look alike.
///
/// It returns only when there is no control plane configured at all, which is
/// the standalone case: the node loads what it has and calls itself ready,
/// because there is no list it could be missing and nobody to register with.
///
/// A pass that cannot reach the control plane backs off and puts registration
/// back to pending: contact was lost, so nothing this node said before can be
/// assumed to still be known on the other side.
pub async fn run_control_loop(
    client: Option<AssignmentClient>,
    registrar: Option<NodeRegistrar>,
    deployer: Arc<DeploymentManager>,
    health: Arc<NodeHealth>,
    retry_interval: Duration,
    reconcile_interval: Duration,
) {
    let Some(client) = client else {
        let recovered = deployer.recover_from_disk().await;
        info!(
            apps = recovered,
            "No control plane endpoint is configured (control.endpoint), so this node runs only \
             what is on its disk and what is pushed to it; it is ready"
        );
        health.mark_ready(recovered);
        return;
    };

    info!(url = client.url(), "Asking the control plane what to run");

    let mut recovered_locally = false;
    let mut ready_once = false;
    let mut attempt: u32 = 0;
    let mut registration = RegistrationState::Pending;

    loop {
        if attempt > 0 {
            tokio::time::sleep(retry_delay(retry_interval, attempt)).await;
        }
        attempt += 1;

        if let Some(registrar) = &registrar {
            registration = announce(registrar, registration).await;
        }

        match reconcile_once(&client, &deployer).await {
            Ok(outcome) => {
                let serving = outcome.installed + outcome.unchanged;
                if outcome.failed > 0 || outcome.unload_failed > 0 {
                    // The control plane answered, so the node is not cut off;
                    // but it is not running exactly what it was told to.
                    health.mark_degraded(
                        format!(
                            "{} of {} assigned versions could not be loaded and {} could not be \
                             stopped",
                            outcome.failed, outcome.assigned, outcome.unload_failed
                        ),
                        serving,
                    );
                    // Straight back round with the backoff: something is wrong
                    // and waiting a whole reconcile interval to try again
                    // leaves the node wrong for that long.
                    continue;
                }

                health.mark_ready(serving);
                if !ready_once {
                    ready_once = true;
                    info!(
                        apps = serving,
                        reconcile_interval_secs = reconcile_interval.as_secs(),
                        "This node now runs exactly what the control plane assigns it, and will \
                         re-check every {}s",
                        reconcile_interval.as_secs()
                    );
                }
                attempt = 0;
                // Never sleep past the moment the registration has to be
                // refreshed. reconcile_interval is configurable and the
                // control plane's TTL is not this node's to choose, so a
                // 5 minute reconcile interval against a 90 second TTL would
                // otherwise drop this node out of the fleet between passes.
                let nap = match registration.time_until_refresh(Instant::now()) {
                    Some(until) => reconcile_interval.min(until),
                    None => reconcile_interval,
                };
                tokio::time::sleep(nap).await;
            }
            Err(e) => {
                // Contact is lost, so whatever the control plane knew about
                // this node cannot be assumed to still hold.
                if matches!(registration, RegistrationState::Done { .. }) {
                    registration = RegistrationState::Pending;
                }

                if !recovered_locally {
                    recovered_locally = true;
                    let recovered = deployer.recover_from_disk().await;
                    warn!(
                        url = client.url(),
                        error = %e,
                        recovered,
                        "The control plane could not be reached at startup. This node is NOT \
                         running a list anyone confirmed: it loaded {} app(s) from its own disk \
                         and will keep retrying, starting at {}s and backing off. It reports \
                         unhealthy until the control plane answers",
                        recovered,
                        retry_interval.as_secs()
                    );
                } else {
                    warn!(
                        url = client.url(),
                        error = %e,
                        "Lost contact with the control plane; this node keeps serving what it \
                         has, stops picking up new versions, and reports unhealthy until the \
                         control plane answers again"
                    );
                }
                let apps = deployer.installed_app_count();
                health.mark_degraded(format!("control plane unreachable: {e}"), apps);
            }
        }
    }
}

/// Announce this node, unless it is already announced or this control plane
/// does not do registration.
///
/// A failure here is reported and left pending rather than propagated: the
/// assignment fetch on the very next line is what decides whether this node
/// can work at all, and a node the control plane does not know about still
/// serves the traffic it is already handling.
async fn announce(registrar: &NodeRegistrar, state: RegistrationState) -> RegistrationState {
    let now = Instant::now();
    match state {
        // Registration is a lease. Letting it lapse costs deployment pushes
        // and cron ticks silently, so the refresh is not optional and not
        // something to skip because this node registered once already.
        RegistrationState::Done { refresh_at } if now < refresh_at => return state,
        RegistrationState::NotSupported => return state,
        RegistrationState::Done { .. } | RegistrationState::Pending => {}
    }

    let first_time = !matches!(state, RegistrationState::Done { .. });

    match registrar.register().await {
        Ok(Registered::Accepted { refresh_after }) => {
            let node = registrar.node();
            if first_time {
                info!(
                    url = registrar.url(),
                    node_id = %node.node_id,
                    role = node.role,
                    version = node.version,
                    host = %node.host,
                    port = node.port,
                    refresh_after_secs = refresh_after.as_secs(),
                    "Registered this node with the control plane as {} at {}; it now receives \
                     deployment pushes and can be picked for cron, and re-registers every {}s to \
                     keep that",
                    node.node_id,
                    node.address(),
                    refresh_after.as_secs()
                );
            } else {
                debug!(
                    url = registrar.url(),
                    refresh_after_secs = refresh_after.as_secs(),
                    "Refreshed this node's registration"
                );
            }
            RegistrationState::Done {
                refresh_at: Instant::now() + refresh_after,
            }
        }
        Ok(Registered::NotSupported) => {
            warn!(
                url = registrar.url(),
                "This control plane has no node registration endpoint, so it knows about this \
                 node only if an operator wrote it into the control plane's own configuration. \
                 Everything else works; what is missing is the control plane addressing a node \
                 it was never told about - cron ticks and the dashboard. Not retried"
            );
            RegistrationState::NotSupported
        }
        Err(e) => {
            warn!(
                url = registrar.url(),
                error = %e,
                was_registered = !first_time,
                "Could not register this node with the control plane; it will try again on the \
                 next pass. Until it succeeds the control plane may not know this node exists, \
                 so it will not be picked for a cron tick and will not appear in the dashboard"
            );
            RegistrationState::Pending
        }
    }
}

/// How long to wait before retry number `attempt`.
///
/// Doubles up to a ceiling. A control plane that is down for an hour would
/// otherwise be asked 240 times and log 240 failures at the configured 15
/// second interval; backing off keeps the node trying without turning an
/// outage into a log flood or into load on a control plane that is already
/// struggling.
fn retry_delay(base: Duration, attempt: u32) -> Duration {
    /// Ceiling as a multiple of the configured interval.
    const MAX_MULTIPLIER: u32 = 20;

    let multiplier = 1u32
        .checked_shl(attempt.saturating_sub(1))
        .unwrap_or(MAX_MULTIPLIER)
        .min(MAX_MULTIPLIER);
    base.saturating_mul(multiplier)
}

#[cfg(test)]
#[path = "assignments_test.rs"]
mod assignments_test;
