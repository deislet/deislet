//! The credential this node presents to deis-telemetry, and the task that
//! keeps it current.
//!
//! # What it is
//!
//! A capability minted by deis-control over the machine channel
//! (`IssueCapability`, audience `telemetry`), naming this node as its subject
//! and one [`TELEMETRY_INGEST`] resource for every application environment
//! the node runs, plus [`TELEMETRY_PLATFORM_TENANT`] for the node's own
//! records (process metrics, component log lines - anything with no
//! application). deis-telemetry stores a pushed record only if the
//! capability names its owner, so a node ships exactly what it runs and
//! nothing else, and it reads nothing at all: ingest does not imply read.
//!
//! There is no secret in this node's configuration to derive a signing key
//! from. The control plane is the only signer, and deis-telemetry verifies
//! against its published keys.
//!
//! # How it stays current
//!
//! [`spawn_renewal`] asks for a capability as soon as the node starts, again
//! before the current one's 15 minutes run out, and again as soon as the set
//! of application environments the node runs changes - a newly deployed
//! application's lines would otherwise be refused until the next renewal.
//! A failed request is retried every 30 seconds or so; until one succeeds the
//! shippers keep their batches queued, exactly as they do when deis-telemetry
//! is down.
//!
//! # Why the node does not stop when it has none
//!
//! Log delivery is never allowed to become a reason a request fails. A node
//! whose credential is missing or refused keeps serving and keeps its
//! batches; what it says about that is loud, in [`report_posture`] and in
//! each shipper's refusal message.

use std::collections::BTreeSet;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use deis_capability::{
    CapabilityResource, AUD_TELEMETRY, TELEMETRY_INGEST, TELEMETRY_PLATFORM_TENANT,
};
use deis_wire::machine::machine_channel_client::MachineChannelClient;
use deis_wire::machine::IssueCapabilityRequest;
use tonic::metadata::{Ascii, MetadataValue};
use tonic::transport::Channel;
use tonic::Request;

/// Renew this long before the current capability expires. Issued
/// capabilities live 15 minutes.
const RENEW_EVERY: Duration = Duration::from_secs(10 * 60);

/// Most jitter taken off [`RENEW_EVERY`], so a fleet restarted together does
/// not renew in lockstep.
const RENEW_JITTER: Duration = Duration::from_secs(60);

/// Wait before retrying a failed request.
const RETRY_EVERY: Duration = Duration::from_secs(30);

/// Most jitter added to [`RETRY_EVERY`].
const RETRY_JITTER: Duration = Duration::from_secs(10);

/// How often the set of running application environments is compared with
/// the one the current capability names.
const TENANT_CHECK_EVERY: Duration = Duration::from_secs(5);

/// The token this node presents on every telemetry call.
///
/// Cheap to clone, and deliberately shared by the three channels and the
/// renewal task: logs, metrics and traces go to one service over one
/// credential, and a renewal reaches every shipper at once.
#[derive(Clone, Default)]
pub struct TelemetryCredential {
    /// Pre-formatted `Bearer <token>` value.
    bearer: Arc<RwLock<Option<MetadataValue<Ascii>>>>,
}

impl TelemetryCredential {
    /// A credential holding `token` from the start, for tests and for
    /// callers that obtained one some other way.
    pub fn with_token(token: &str) -> Result<Self, String> {
        let credential = Self::default();
        credential.set(token)?;
        Ok(credential)
    }

    /// Replace the token every shipper presents.
    pub fn set(&self, token: &str) -> Result<(), String> {
        let bearer = format!("Bearer {token}")
            .parse::<MetadataValue<Ascii>>()
            .map_err(|_| "the telemetry capability is not valid gRPC metadata".to_string())?;
        *self.bearer.write().unwrap_or_else(|e| e.into_inner()) = Some(bearer);
        Ok(())
    }

    /// Whether this node holds a capability at all.
    pub fn is_present(&self) -> bool {
        self.bearer
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .is_some()
    }

    /// Wrap one message in a request carrying the token.
    ///
    /// Stamped here rather than in each call so a push that forgot it would
    /// be refused, retried, and refused again for as long as the node ran.
    pub fn stamp<T>(&self, message: T) -> Request<T> {
        let mut request = Request::new(message);
        if let Some(bearer) = self
            .bearer
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
        {
            request
                .metadata_mut()
                .insert("authorization", bearer.clone());
        }
        request
    }
}

/// Redacted on purpose: a derived `Debug` would put the token into any line
/// that formats a shipper.
impl std::fmt::Debug for TelemetryCredential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TelemetryCredential")
            .field(
                "bearer",
                &if self.is_present() {
                    "<present>"
                } else {
                    "<absent>"
                },
            )
            .finish()
    }
}

/// What a shipper says when it starts, so an operator can tell from the
/// first lines of a node's log where its credential comes from.
///
/// Not a refusal to start: a node that cannot ship telemetry must still
/// serve requests.
pub fn report_posture(credential: &TelemetryCredential, endpoint: &str) {
    if credential.is_present() {
        tracing::info!(
            %endpoint,
            "This node presents a control-plane-issued telemetry capability on every log, trace \
             and metric push"
        );
    } else {
        tracing::info!(
            %endpoint,
            "This node has no telemetry capability yet; it asks the control plane for one over \
             the machine channel, and pushes are queued until it arrives. A node with no \
             control plane configured never gets one, and deis-telemetry refuses everything it \
             ships"
        );
    }
}

/// What to ask the control plane for: ingest for every tenant in `tenants`
/// and for the node's own records.
fn resources_for(tenants: &BTreeSet<String>) -> Vec<CapabilityResource> {
    std::iter::once(TELEMETRY_PLATFORM_TENANT)
        .chain(tenants.iter().map(String::as_str))
        .map(|tenant| CapabilityResource {
            tenant: tenant.to_string(),
            binding: TELEMETRY_INGEST.to_string(),
            generation: None,
        })
        .collect()
}

/// Ask for one capability covering `tenants`.
async fn issue(
    client: &mut MachineChannelClient<Channel>,
    node_id: &str,
    tenants: &BTreeSet<String>,
) -> Result<String, String> {
    let resources_json = serde_json::to_string(&resources_for(tenants))
        .map_err(|e| format!("cannot encode the requested resources: {e}"))?;
    let response = client
        .issue_capability(Request::new(IssueCapabilityRequest {
            node_id: node_id.to_string(),
            audience: AUD_TELEMETRY.to_string(),
            resources_json,
        }))
        .await
        .map_err(|status| format!("{}: {}", status.code(), status.message()))?;
    Ok(response.into_inner().capability_token)
}

/// Keep `credential` holding a capability for whatever `tenants` returns,
/// for as long as the process runs.
///
/// `tenants` is asked often (every few seconds) and must be cheap: it is the
/// set of `<app>.<environment>` this node currently runs.
pub fn spawn_renewal<F>(
    credential: TelemetryCredential,
    client: MachineChannelClient<Channel>,
    node_id: String,
    tenants: F,
) -> tokio::task::JoinHandle<()>
where
    F: Fn() -> BTreeSet<String> + Send + 'static,
{
    tokio::spawn(async move {
        let mut client = client;
        // What the capability held now names, and when it has to be renewed
        // regardless. `None` until the first request succeeds.
        let mut issued: Option<(BTreeSet<String>, tokio::time::Instant)> = None;
        loop {
            let wanted = tenants();
            let due = match &issued {
                None => true,
                Some((named, renew_at)) => {
                    *named != wanted || tokio::time::Instant::now() >= *renew_at
                }
            };
            if !due {
                tokio::time::sleep(TENANT_CHECK_EVERY).await;
                continue;
            }
            match issue(&mut client, &node_id, &wanted).await {
                Ok(token) => match credential.set(&token) {
                    Ok(()) => {
                        tracing::info!(
                            tenants = wanted.len(),
                            "Obtained a telemetry capability for this node's application \
                             environments and its own records"
                        );
                        let renew_at =
                            tokio::time::Instant::now() + RENEW_EVERY - jitter(RENEW_JITTER);
                        issued = Some((wanted, renew_at));
                    }
                    Err(error) => {
                        tracing::error!(%error, "The control plane returned an unusable telemetry capability");
                        tokio::time::sleep(RETRY_EVERY + jitter(RETRY_JITTER)).await;
                    }
                },
                Err(error) => {
                    tracing::warn!(
                        %error,
                        held = credential.is_present(),
                        "Could not obtain a telemetry capability; pushes stay queued, and the \
                         request is retried in about {}s",
                        RETRY_EVERY.as_secs()
                    );
                    tokio::time::sleep(RETRY_EVERY + jitter(RETRY_JITTER)).await;
                }
            }
        }
    })
}

/// A uniformly random duration in `[0, max)`, from the standard library's
/// randomly keyed hasher. Only meant to spread load across a fleet.
fn jitter(max: Duration) -> Duration {
    use std::hash::{BuildHasher, Hasher};
    let max_ms = max.as_millis() as u64;
    if max_ms == 0 {
        return Duration::ZERO;
    }
    let random = std::collections::hash_map::RandomState::new()
        .build_hasher()
        .finish();
    Duration::from_millis(random % max_ms)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_node_without_a_capability_stamps_nothing() {
        let credential = TelemetryCredential::default();
        assert!(!credential.is_present());
        assert!(credential
            .stamp(())
            .metadata()
            .get("authorization")
            .is_none());
    }

    #[test]
    fn a_renewed_token_reaches_every_holder() {
        let credential = TelemetryCredential::default();
        let shipper = credential.clone();
        credential.set("v1.first.sig").unwrap();
        credential.set("v1.second.sig").unwrap();
        let stamped = shipper.stamp(());
        assert_eq!(
            stamped
                .metadata()
                .get("authorization")
                .and_then(|value| value.to_str().ok()),
            Some("Bearer v1.second.sig")
        );
    }

    #[test]
    fn the_request_names_every_tenant_and_the_platform_for_ingest_only() {
        let tenants: BTreeSet<String> = ["shop.production", "shop.preview"]
            .into_iter()
            .map(String::from)
            .collect();
        let resources = resources_for(&tenants);
        assert_eq!(resources.len(), 3);
        assert!(resources.iter().all(|r| r.binding == TELEMETRY_INGEST));
        assert_eq!(resources[0].tenant, TELEMETRY_PLATFORM_TENANT);
        assert!(resources.iter().any(|r| r.tenant == "shop.preview"));
    }

    #[test]
    fn a_token_that_is_not_metadata_is_refused() {
        assert!(TelemetryCredential::default().set("line\nbreak").is_err());
    }
}
