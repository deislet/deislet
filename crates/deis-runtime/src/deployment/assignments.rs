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
//! The bundle itself is never in the list: each line is a pointer - app,
//! version, environment, URL, digest - plus the environment map that a push
//! would have carried, so an app that comes back this way comes back with its
//! secrets and its storage addresses rather than with an empty environment.
//! An older control plane that sends no `env` still works; the app then runs
//! on its manifest's own variables until the next push, which is what this
//! path used to do for every app and is said out loud when it happens.

use std::sync::Arc;
use std::time::Duration;

use serde::Deserialize;
use tracing::{error, info, warn};

use super::credential::NodeCredential;
use super::{Applied, DeploymentManager, DeploymentOrder};
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
            "{}/api/nodes/{}/assignments",
            endpoint.trim_end_matches('/'),
            urlencode(node_id)
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

    pub async fn fetch(&self) -> Result<Vec<Assignment>, AssignmentError> {
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

        response
            .json::<Vec<Assignment>>()
            .await
            .map_err(|e| AssignmentError::Malformed {
                url: self.url.clone(),
                reason: e.to_string(),
            })
    }
}

/// Percent-encode the few characters that would otherwise change which path a
/// node id addresses. Node ids are hostnames in practice, but an id with a `/`
/// in it would silently ask the control plane a different question.
fn urlencode(value: &str) -> String {
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
}

/// Fetch the assignment list once and load everything in it.
pub async fn sync_once(
    client: &AssignmentClient,
    deployer: &DeploymentManager,
) -> Result<SyncOutcome, AssignmentError> {
    let assignments = client.fetch().await?;
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

    Ok(outcome)
}

/// Bring the node up to what the control plane says, retrying until it does.
///
/// Runs for the life of the process. It returns only when there is no control
/// plane configured at all, which is the standalone case: the node loads what
/// it has and calls itself ready, because there is no list it could be
/// missing.
pub async fn run_startup_sync(
    client: Option<AssignmentClient>,
    deployer: Arc<DeploymentManager>,
    health: Arc<NodeHealth>,
    retry_interval: Duration,
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
    let mut attempt: u32 = 0;

    loop {
        if attempt > 0 {
            tokio::time::sleep(retry_delay(retry_interval, attempt)).await;
        }
        attempt += 1;

        match sync_once(&client, &deployer).await {
            Ok(outcome) => {
                if outcome.failed > 0 {
                    // The control plane answered, so the node is not cut off;
                    // but it is not running everything it was told to.
                    health.mark_degraded(
                        format!(
                            "{} of {} assigned versions could not be loaded",
                            outcome.failed, outcome.assigned
                        ),
                        outcome.installed + outcome.unchanged,
                    );
                    continue;
                }
                health.mark_ready(outcome.installed + outcome.unchanged);
                return;
            }
            Err(e) => {
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
                }
                let apps = deployer.installed_app_count();
                health.mark_degraded(format!("control plane unreachable: {e}"), apps);
            }
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
