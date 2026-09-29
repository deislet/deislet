//! The control plane's HTTP API, as this CLI uses it.
//!
//! Every type here mirrors what `deis-control` already serves, and every
//! field the CLI prints is deserialised straight off what the server
//! returns, so an added field there does not break an older CLI.

pub mod queue;
pub mod service_tls;
pub mod storage;
pub mod telemetry;

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use deis_wire::{CanonicalId, OperationState};
use reqwest::{
    Method, StatusCode,
    header::{ACCEPT, HeaderMap},
};
use serde::{Deserialize, Serialize};

/// How long one control-plane call may take.
///
/// Generous because a deploy pushes to every node before it answers, and a
/// node that is slow to load a version is not a reason to abandon a deploy
/// that has already been recorded.
const REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);
/// A CLI mutation remains synchronous from a developer's point of view even
/// when Control returns a durable `202 Accepted` plan while queues drain.
const DEPLOYMENT_WAIT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(300);
const DEPLOYMENT_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(500);
/// How long `--wait` follows one provider deployment before handing the
/// operator back their shell.
///
/// Far longer than a deployment plan's window because the work is different
/// in kind: a plan pushes a bundle to nodes this platform runs, while a
/// provider workflow runs seven driver steps against somebody else's cloud
/// and waits for that cloud to converge. Giving up is not cancelling - the
/// operation keeps running and the message says where its status lives.
const PROVIDER_DEPLOYMENT_WAIT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(900);
/// How often `--wait` reads the workflow. Slower than a plan's poll because
/// a provider step takes seconds at best, and every read is a request an
/// operator's credential is spending on the control plane.
const PROVIDER_DEPLOYMENT_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_secs(2);
/// The header that makes a repeated mutation answer the first one rather
/// than start a second. Spelled once here; both provider-deployment
/// mutations send it.
const IDEMPOTENCY_KEY: &str = "idempotency-key";

/// The version of the control plane's list envelope this build reads.
const LIST_SCHEMA_VERSION: u8 = 1;
/// The largest page any list route serves; a larger `limit=` is refused,
/// not clamped, so a caller that asks for one page of "the latest N" caps
/// N here itself.
///
/// Public because a command that prints "20 of 4000 shown" has to be able to
/// say what raising `--limit` can actually reach, and a second copy of the
/// number in the renderer would be the copy that goes stale.
pub const MAX_LIST_LIMIT: usize = 1_000;
/// How many pages one list may be read as before this CLI gives up.
///
/// A control plane that keeps handing out a `nextCursor` forever would
/// otherwise turn one `deis versions` into an unbounded stream of requests.
/// At the server's default page size this cap is a million rows, so nothing
/// a real catalog holds ever reaches it.
const MAX_LIST_PAGES: usize = 10_000;

/// Path prefix of the product API this build speaks: applications,
/// versions, environments, deployments, secrets, accounts, tokens and the
/// audit log. Every route below is spelled without it and [`Control`] puts
/// it on, so this is the one place the major version is written.
pub const API_MAJOR: &str = "/api/v1";
/// Path prefix of the platform diagnostics API: the runtime fleet and what
/// each node reports it is running.
pub const API_PLATFORM: &str = "/api/platform";
/// The response header every versioned answer carries: the id the control
/// plane logged the request under, which is what to quote when asking an
/// operator what happened.
const REQUEST_ID_HEADER: &str = "x-request-id";

/// One page of the control plane's unified list envelope.
///
/// Every paginated list route answers this exact shape, so one type reads
/// all of them and one helper walks them to the end. `T` is whatever the
/// route lists.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ListPage<T> {
    pub schema_version: u8,
    pub items: Vec<T>,
    pub page: PageMeta,
}

/// What the control plane says about the page it just answered with.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PageMeta {
    /// The page size actually applied, which is the server's default when
    /// the request named none.
    pub limit: usize,
    /// How many rows this page carries. Equal to `items.len()`.
    pub count: usize,
    /// How many rows the same query has in total, counted by the control
    /// plane in the same read as the items.
    pub total: usize,
    /// The token that reads the page after this one, absent on the last.
    #[serde(default)]
    pub next_cursor: Option<String>,
}

/// Add `cursor=<token>` to a list path, keeping whatever query it already
/// carries.
///
/// The token is opaque to this CLI - it is the control plane's own string -
/// so it is percent-encoded rather than trusted to be URL-safe.
fn append_cursor(path: &str, cursor: &str) -> String {
    let separator = if path.contains('?') { '&' } else { '?' };
    let encoded: String = url::form_urlencoded::byte_serialize(cursor.as_bytes()).collect();
    format!("{path}{separator}cursor={encoded}")
}

/// The two things every page of the unified envelope is held to, wherever it
/// is read: an envelope version this build does not know, and a page that
/// counted itself differently than it filled itself.
///
/// A free function rather than a method because there are two readers of the
/// same envelope and they differ only in whether they follow the cursor -
/// [`Control::list_all`] does, [`Control::list_first_page`] deliberately does
/// not. A second copy of these checks in the second reader is exactly how a
/// "latest N" command ends up silently reading an envelope the paging reader
/// would have warned about.
fn check_list_envelope<T>(path: &str, page_number: usize, page: &ListPage<T>) {
    if page.schema_version != LIST_SCHEMA_VERSION {
        // Readable, since the items themselves parsed - but say so once,
        // because a field this CLI prints may have moved.
        tracing::warn!(
            path,
            page_number,
            schema_version = page.schema_version,
            expected = LIST_SCHEMA_VERSION,
            "control plane answered a list envelope this CLI does not know"
        );
    }
    if page.page.count != page.items.len() {
        // Only ever a control-plane bug: the contract says the two are the
        // same number. What arrived is still what is used.
        tracing::warn!(
            path,
            page_number,
            claimed = page.page.count,
            received = page.items.len(),
            "control plane counted a page differently than it filled it"
        );
    }
    tracing::debug!(
        path,
        page_number,
        limit = page.page.limit,
        count = page.items.len(),
        total = page.page.total,
        has_next = page.page.next_cursor.is_some(),
        "read one page of a control-plane list"
    );
}

/// The page size a "latest N" read may actually ask for, and a warning when
/// that is not the size the operator typed.
///
/// The control plane *refuses* an oversized `limit=` rather than clamping it,
/// so a command that passed the number through unchanged would answer a
/// person who asked for 5000 rows with a bad request instead of with rows.
/// Clamping here keeps the command working and says, at warn, that the answer
/// is shorter than what was asked for - which is the sort of quiet difference
/// an operator otherwise discovers by miscounting an incident.
fn bounded_page_limit(requested: usize, what: &str) -> usize {
    let bounded = requested.clamp(1, MAX_LIST_LIMIT);
    if bounded != requested {
        tracing::warn!(
            what,
            requested,
            applied = bounded,
            "the control plane serves at most {MAX_LIST_LIMIT} rows per page; reading that many"
        );
    }
    bounded
}

/// An application as the catalog records it.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct App {
    pub id: String,
    /// Permanent product identity.
    pub canonical_id: CanonicalId,
    pub created_at: i64,
}

/// One immutable version.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Version {
    pub id: String,
    /// Permanent product identity.
    pub canonical_id: CanonicalId,
    pub app_id: String,
    /// The per-application counter a person says out loud: "v3".
    pub number: i64,
    pub bundle_sha: String,
    /// Exact canonical parent and provider derivation. Absent on versions
    /// uploaded through the pre-lineage compatibility endpoint.
    #[serde(default)]
    pub canonical_artifact_id: Option<CanonicalId>,
    #[serde(default)]
    pub provider_derivation_sha256: Option<String>,
    pub created_at: i64,
    #[serde(default)]
    pub manifest: serde_json::Value,
    /// Which day's platform this version was written against, and which
    /// behaviours that leaves on, as the control plane resolved it out of the
    /// manifest it stored.
    ///
    /// `None` covers two cases that read the same to a person: a control plane
    /// too old to say, and a stored manifest it could not resolve. Both are
    /// printed as "unknown", which is the honest answer - what is never
    /// printed as unknown is a version that simply declares no date, because
    /// that one comes back as `Some` with a null date and is exactly what an
    /// operator is looking for.
    #[serde(default)]
    pub compatibility: Option<deis_bundle::CompatibilityProfile>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct UploadResponse {
    pub version: Version,
    /// False when a byte-identical bundle was already stored, in which case
    /// the existing version is returned unchanged and no new number is used.
    pub created: bool,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CanonicalArtifactRecord {
    pub artifact_id: CanonicalId,
    pub application_id: CanonicalId,
    pub state: String,
    pub deployable: bool,
    pub canonical_output: CanonicalOutputIdentity,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CanonicalOutputIdentity {
    pub artifact_sha256: String,
    pub content_root_sha256: String,
    pub file_count: usize,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ArtifactUploadResponse {
    pub artifact: CanonicalArtifactRecord,
    pub created: bool,
}

/// A stored provider payload, as `POST …/provider-payloads` answers it.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderPayloadUpload {
    pub artifact_id: CanonicalId,
    pub provider: String,
    pub standard_version: String,
    pub manifest_sha256: String,
    pub payload_root_sha256: String,
    pub created: bool,
}

/// One environment's authorization to deploy to one provider target.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderTargetGrant {
    pub grant_id: CanonicalId,
    pub provider_target_id: CanonicalId,
    /// `cloudflare-workers-pages` for Cloudflare.
    pub backend_id: String,
    pub target_name: String,
    pub target_status: String,
    pub status: String,
}

/// The exact artifact a provider deployment deploys.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderArtifactRef {
    pub artifact_version_id: CanonicalId,
    pub canonical_artifact_sha256: String,
    pub provider_payload_sha256: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Environment {
    pub id: String,
    /// Permanent product identity.
    pub canonical_id: CanonicalId,
    pub app_id: String,
    pub name: String,
    pub created_at: i64,
}

/// One pointer row: this environment ran this version from this moment.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Deployment {
    pub id: String,
    /// Permanent product identity.
    pub canonical_id: CanonicalId,
    pub app_id: String,
    pub env_id: String,
    pub version_id: String,
    pub created_at: i64,
    #[serde(default)]
    pub note: Option<String>,
    /// Immutable Edge Canon environment snapshot selected by this pointer.
    pub snapshot_id: String,
}

/// The top of one environment's deployment history, and how deep the history
/// goes.
///
/// `total` is the whole environment's count as the control plane counted it
/// in the same read that produced the rows, not the number printed. Keeping
/// the two apart is what lets `deis deployments` say "20 of 4000" instead of
/// leaving a reader to assume that twenty is all there ever was.
#[derive(Debug, Clone)]
pub struct DeploymentHistory {
    /// Newest first, as the route serves them.
    pub deployments: Vec<Deployment>,
    pub total: usize,
}

/// What a deploy, a rollback or a read of the current pointer answers.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct DeploymentResponse {
    pub deployment: Deployment,
    pub environment: String,
    pub version: Version,
    /// Absent when nothing was pushed, which is the case for a plain read.
    #[serde(default)]
    pub delivery: Option<DeliveryReport>,
    /// Durable operation identity and terminal state on current Control
    /// versions. Absent when reading an older deployment.
    #[serde(default)]
    pub plan: Option<DeploymentPlanSummary>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct DeploymentPlanSummary {
    pub id: String,
    pub routing_generation: CanonicalId,
    pub state: String,
    #[serde(default)]
    pub last_error: Option<String>,
}

/// Mutation responses may be accepted before a production pointer exists.
/// This shape is deliberately private so ordinary current-deployment callers
/// still receive a statically required pointer.
#[derive(Debug, Deserialize)]
struct DeploymentMutationResponse {
    #[serde(default)]
    deployment: Option<Deployment>,
    environment: String,
    version: Version,
    #[serde(default)]
    delivery: Option<DeliveryReport>,
    #[serde(default)]
    plan: Option<DeploymentPlanSummary>,
}

#[derive(Debug, Deserialize)]
struct DeploymentPlanStatusResponse {
    plan: DeploymentPlanSummary,
}

/// One durable operation, as every provider-deployment answer carries it.
///
/// `kind` and `state` are read as strings rather than as the `deis_wire`
/// enums they mirror, and that is deliberate: a control plane newer than this
/// build may name a state this build has never heard of, and an operator
/// staring at an unfinished deployment needs to be *shown* that name. Refusing
/// the whole document instead would turn "your CLI is older than the platform"
/// into "the deployment cannot be read at all". [`Self::settled`] is where the
/// contract's own state machine is consulted, so nothing here second-guesses
/// which states are terminal.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderOperationSummary {
    pub operation_id: CanonicalId,
    pub kind: String,
    pub state: String,
    #[serde(default)]
    pub phase: String,
    /// The revision `If-Match` names. Every conditional call sends the one
    /// the ETag of the read that produced it carried, not this number; it is
    /// here so `--json` can show what the caller acted on.
    #[serde(default)]
    pub revision: u64,
    /// Basis points, or `None` when the control plane has no honest
    /// denominator to divide by.
    #[serde(default)]
    pub progress: Option<u16>,
    #[serde(default)]
    pub cancellable: bool,
    #[serde(default)]
    pub retryable: bool,
    #[serde(default)]
    pub error: Option<OperationFailure>,
    pub created_at: DateTime<Utc>,
    #[serde(default)]
    pub updated_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub finished_at: Option<DateTime<Utc>>,
}

impl ProviderOperationSummary {
    /// Whether the operation has stopped moving, or `None` when this build
    /// does not know the state the control plane named.
    ///
    /// `None` is not "still running": a waiter that treated an unknown state
    /// as unfinished would poll a settled operation until it timed out, and
    /// one that treated it as finished would report a running deployment as
    /// done. The caller has to say which of those is safe for what it is
    /// doing, so this hands the ignorance back rather than picking.
    #[must_use]
    pub fn settled(&self) -> Option<bool> {
        OperationState::parse(&self.state).map(OperationState::terminal)
    }
}

/// Why an operation ended badly, as the control plane records it.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OperationFailure {
    pub code: String,
    pub message: String,
    #[serde(default)]
    pub retryable: bool,
    #[serde(default)]
    pub retry_after_seconds: Option<u64>,
    #[serde(default)]
    pub remediation: Option<String>,
}

/// One step of a provider workflow and the child operation that ran it.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderDeploymentStep {
    pub sequence: usize,
    /// The driver call this step makes: `validate`, `plan`, `apply`,
    /// `observe`, `rollback` and so on. A string, because the set belongs to
    /// the driver contract and grows there, not here.
    pub operation: String,
    pub child_operation_id: CanonicalId,
}

/// One provider deployment or rollback workflow, as the catalog records it.
///
/// Mirrors `ProviderDeploymentWorkflow` in `deis-control`. Every field this
/// CLI prints is read straight off the answer and everything optional is
/// defaulted, so a control plane that grows a field does not stop an older
/// CLI from printing the ones it already knows.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderDeployment {
    pub operation: ProviderOperationSummary,
    /// `deploy` runs the driver's standard steps; `rollback` restores the
    /// deployment named by `rolled_back_from_operation_id`; `undeploy` takes
    /// the environment's deployment down.
    pub kind: String,
    /// For a rollback workflow, the succeeded deployment it restores.
    #[serde(default)]
    pub rolled_back_from_operation_id: Option<CanonicalId>,
    /// For an undeploy workflow, whether the environment's provider data is
    /// destroyed along with the deployment.
    #[serde(default)]
    pub purge_data: bool,
    /// Every rollback that names this workflow as its source, oldest first,
    /// whatever state each reached.
    #[serde(default)]
    pub rollback_operation_ids: Vec<CanonicalId>,
    /// When a cancel was accepted. Set once; the operation's state says how
    /// far the cancel then got.
    #[serde(default)]
    pub cancel_requested_at: Option<DateTime<Utc>>,
    pub provider_target_id: CanonicalId,
    #[serde(default)]
    pub target_revision: u64,
    #[serde(default)]
    pub provider_target_grant_id: Option<CanonicalId>,
    #[serde(default)]
    pub target_grant_revision: Option<u64>,
    #[serde(default)]
    pub desired_state_digest: Option<String>,
    #[serde(default)]
    pub next_step_index: usize,
    #[serde(default)]
    pub dispatch_failure_count: u64,
    #[serde(default)]
    pub last_dispatch_error_code: Option<String>,
    #[serde(default)]
    pub last_dispatch_error_message: Option<String>,
    #[serde(default)]
    pub steps: Vec<ProviderDeploymentStep>,
}

/// One workflow and the exact revision the read of it observed.
///
/// The two travel together because a cancel is only correct against the
/// revision the caller actually saw: sending back a revision recomputed from
/// the body would defeat the fence the moment the body and the ETag disagree.
#[derive(Debug, Clone)]
pub struct ProviderDeploymentRead {
    pub workflow: ProviderDeployment,
    /// The `ETag` of the read, verbatim, ready to be an `If-Match`.
    pub etag: String,
}

/// What a cancel request did: what the fence was taken against, and what the
/// control plane answered.
///
/// Both halves are printed. `observed` is the state the operator's cancel was
/// aimed at, `operation` is where the operation stands now - `cancelling`
/// while in-flight provider work settles, `cancelled` once it has. Showing
/// only the second would leave "cancelled a deployment that had already
/// finished" and "cancelled one that was still running" looking identical.
#[derive(Debug, Clone)]
pub struct ProviderDeploymentCancel {
    /// The workflow as it was read to fence the cancel.
    pub observed: ProviderDeployment,
    /// The operation as the control plane answered the cancel.
    pub operation: ProviderOperationSummary,
}

/// A fresh `Idempotency-Key`.
///
/// A UUIDv7, which comfortably clears the control plane's rule of 8 to 255
/// non-control bytes and carries its own creation time, so a replayed key can
/// be dated from the key itself when a record is being chased. One per call
/// site invocation, never reused across attempts of *different* requests: the
/// whole point is that one operator action has one key.
fn new_idempotency_key() -> String {
    CanonicalId::new_v7().to_string()
}

/// The revision an answer was read at, spelled as an `If-Match` value.
///
/// The `ETag` header is preferred over the revision in the body because the
/// header is what the control plane's own precondition check compares
/// against; recomputing the value from the body would quietly paper over a
/// control plane whose two answers disagree, and the fence exists to catch
/// exactly that class of disagreement. The body is the fallback for a control
/// plane that answers without the header at all, which is said at debug
/// because the fence is then weaker than it looks.
fn read_etag(headers: &HeaderMap, operation: &ProviderOperationSummary) -> String {
    match headers
        .get(reqwest::header::ETAG)
        .and_then(|value| value.to_str().ok())
    {
        Some(etag) => etag.to_string(),
        None => {
            tracing::debug!(
                operation_id = %operation.operation_id,
                revision = operation.revision,
                "the control plane answered a provider deployment without an ETag; fencing on \
                 the revision in the body instead"
            );
            format!("\"{}\"", operation.revision)
        }
    }
}

/// The top of one environment's provider-deployment history, and how deep it
/// goes - the same shape, and the same one-page rule, as
/// [`DeploymentHistory`].
#[derive(Debug, Clone)]
pub struct ProviderDeploymentHistory {
    /// Newest first, as the route serves them.
    pub deployments: Vec<ProviderDeployment>,
    pub total: usize,
}

#[cfg(test)]
mod canonical_identity_tests {
    use super::App;

    #[test]
    fn an_app_without_its_canonical_identity_is_refused() {
        assert!(serde_json::from_str::<App>(r#"{"id":"demo","created_at":1}"#).is_err());
    }

    #[test]
    fn present_canonical_identity_is_validated() {
        let valid: App = serde_json::from_str(
            r#"{"id":"demo","canonical_id":"018f2518-7e38-7f80-8000-000000000001","created_at":1}"#,
        )
        .unwrap();
        assert_eq!(
            valid.canonical_id.to_string(),
            "018f2518-7e38-7f80-8000-000000000001"
        );

        let malformed = serde_json::from_str::<App>(
            r#"{"id":"demo","canonical_id":"550e8400-e29b-41d4-a716-446655440000","created_at":1}"#,
        );
        assert!(malformed.is_err());
    }
}

/// Where a version actually landed.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct DeliveryReport {
    pub reason: String,
    /// Exact wire phase observed by this report.
    pub phase: String,
    pub app_id: String,
    pub environment: String,
    pub version_id: String,
    /// Nodes that applied the version and now serve it.
    #[serde(default)]
    pub delivered: Vec<String>,
    /// Nodes that did not. They keep serving whatever they had.
    #[serde(default)]
    pub failed: Vec<NodeFailure>,
    /// Set when nothing was pushed at all, with the reason.
    #[serde(default)]
    pub skipped: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct NodeFailure {
    pub node: String,
    pub error: String,
}

/// A route as the catalog records it: what the edge matches a request
/// against, and which environment answers it.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Route {
    pub id: String,
    pub app_id: String,
    pub env_id: String,
    /// `None` matches any domain. That is the *widest* claim on this
    /// platform rather than the mildest one, which is why it is spelled out
    /// in the table rather than left blank.
    #[serde(default)]
    pub host: Option<String>,
    pub path_prefix: String,
    /// Higher wins when more than one route matches a request.
    pub priority: i64,
}

/// One row of the hostname registry: which application, in which
/// environment, may claim which name.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct DomainGrant {
    /// `example.com`, `*.example.com`, `(no-host)` or `*`.
    pub pattern: String,
    pub app_id: String,
    /// One environment's name, or [`EVERY_ENVIRONMENT`].
    pub environment: String,
    pub created_at: i64,
    /// Free text an operator left for the next operator. Never interpreted.
    #[serde(default)]
    pub note: Option<String>,
}

/// What a grant's environment column holds when the grant covers every
/// environment of its application.
///
/// One character of wire format, spelled here because this CLI does not
/// depend on deis-control and cannot import the constant it is a copy of
/// (`catalog::domains::EVERY_ENVIRONMENT`). What keeps the two the same is
/// `list_page_tests::the_every_environment_marker_is_the_control_planes_own`,
/// which reads the sibling crate's source and fails if it changed.
pub const EVERY_ENVIRONMENT: &str = "*";

/// A secret's metadata. There is deliberately no field for the value: the
/// control plane has no endpoint that returns one.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SecretMeta {
    pub name: String,
    /// Opaque immutable revision.
    pub revision: String,
    pub updated_at: i64,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SecretChangeResponse {
    pub secret: SecretMeta,
    pub delivery: DeliveryReport,
    #[serde(default)]
    pub plan: Option<DeploymentPlanSummary>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SecretDeletedResponse {
    pub deleted: String,
    pub delivery: DeliveryReport,
    #[serde(default)]
    pub plan: Option<DeploymentPlanSummary>,
}

/// What `deis kv`, `deis db` and `deis blob` need to reach deis-store as one
/// tenant.
///
/// Both fields stay optional even though the control plane that serves this
/// route today refuses rather than answering with either one missing: this
/// is a deserialisation target for whatever a control plane sends, and an
/// older or newer one that answers with only an address must be reported by
/// the message below rather than by a parse failure that names a field.
#[derive(Debug, Clone, Deserialize)]
pub struct StorageCredentials {
    #[serde(default)]
    pub store_endpoint: Option<String>,
    #[serde(default)]
    pub token: Option<String>,
    /// The deis-queue capability for the same tenant. deis-queue accepts
    /// only its own audience, so the store token is refused there.
    #[serde(default)]
    pub queue_token: Option<String>,
}

/// A deis-telemetry read capability and the tenants it reads.
#[derive(Deserialize)]
pub struct TelemetryCredentials {
    pub tenants: Vec<String>,
    pub token: String,
}

/// Never derived: the token must not reach a log line.
impl std::fmt::Debug for TelemetryCredentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TelemetryCredentials")
            .field("tenants", &self.tenants)
            .finish_non_exhaustive()
    }
}

/// What this credential can see of the runtime fleet.
///
/// Fleet scope belongs to the platform operator, so a credential that only
/// reaches one application is refused this list. That refusal is an answer
/// a command can print, not a failure: `deis status` is about an
/// application, and the node section is one part of its report.
#[derive(Debug, Clone)]
pub enum FleetView {
    /// The nodes this control plane knows about.
    Nodes(Vec<Node>),
    /// The credential in hand does not hold fleet scope.
    NotPermitted,
}

/// What this credential can see of one environment's current deployment.
///
/// A token scoped to one environment of an application can list that
/// application's environments and is refused a look at the others, so "I may
/// not see this one" is an ordinary answer here rather than a failure of the
/// whole command. `deis status` and `deis versions` walk every environment an
/// application has, and both would stop at the first refusal if this were an
/// error; instead they print what the credential can see and say which rows
/// it cannot.
#[derive(Debug, Clone)]
pub enum EnvironmentView {
    /// It runs this version. Boxed because it is much larger than the other
    /// two variants and every caller holds a list of these.
    Running(Box<DeploymentResponse>),
    /// It exists and has never been deployed. A state, not an error.
    NeverDeployed,
    /// The credential in hand does not reach this environment.
    NotPermitted,
}

/// One runtime node as the control plane knows it.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Node {
    pub id: String,
    pub addr: String,
    #[serde(default)]
    pub healthy: bool,
    #[serde(default)]
    pub version: Option<String>,
    #[serde(default)]
    pub last_error: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct NodesResponse {
    pub nodes: Vec<Node>,
}

/// One (application, environment) as one node reports it right now.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct NodeAppStatus {
    pub app_id: String,
    pub environment: String,
    pub state: String,
    #[serde(default)]
    pub request_count: i64,
    #[serde(default)]
    pub error_count: i64,
    #[serde(default)]
    pub error_message: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct NodeStatusResponse {
    #[serde(default)]
    pub apps: Vec<NodeAppStatus>,
}

/// One account, as the control plane records it.
///
/// The role is the ceiling on everything this account's tokens may do, so it
/// is the first thing a listing shows: a token can only ever be narrower.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Account {
    pub id: String,
    pub name: String,
    pub role: String,
    pub created_at: i64,
    /// Set while the account is switched off. Its tokens are refused and not
    /// revoked, so switching it back on restores exactly those credentials.
    #[serde(default)]
    pub disabled_at: Option<i64>,
}

/// One credential, as a listing shows it. There is deliberately no field a
/// secret could arrive in: the control plane stores a digest and has no
/// endpoint that reads a token back.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ApiToken {
    pub id: String,
    pub account: String,
    pub name: String,
    /// `minted` for a token this control plane issued, `config` for an
    /// `api_keys` entry in its configuration file.
    pub origin: String,
    /// The scope list, or the word `unscoped`.
    pub scopes: String,
    /// True when this credential answers to no scope at all - the old flat
    /// key. Read as a field rather than by recognising the word above.
    #[serde(default)]
    pub unscoped: bool,
    pub created_at: i64,
    #[serde(default)]
    pub last_used_at: Option<i64>,
    #[serde(default)]
    pub revoked_at: Option<i64>,
}

/// What `deis token list` prints: every credential the filters admit, and
/// how many live credentials answer to no scope at all. The count is the
/// control plane's own (the total of a `scope=unscoped` listing), so every
/// reader gets the same number.
#[derive(Debug, Clone)]
pub struct TokenListing {
    pub tokens: Vec<ApiToken>,
    pub unscoped: usize,
}

/// What minting a token answers. The only message in this protocol that
/// carries a credential.
#[derive(Debug, Clone, Deserialize)]
pub struct MintedToken {
    pub token: ApiToken,
    pub secret: String,
}

/// One line of the audit log.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct AuditEntry {
    pub id: i64,
    pub at: i64,
    pub actor_kind: String,
    #[serde(default)]
    pub actor_account: Option<String>,
    #[serde(default)]
    pub actor_credential: Option<String>,
    pub action: String,
    #[serde(default)]
    pub app_id: Option<String>,
    #[serde(default)]
    pub environment: Option<String>,
    #[serde(default)]
    pub target: Option<String>,
    pub method: String,
    pub path: String,
    #[serde(default)]
    pub peer: Option<String>,
    pub outcome: String,
    pub status: i64,
    #[serde(default)]
    pub detail: Option<String>,
    /// The id the control plane handled the request under, which is the
    /// same id it printed in the refusal the caller saw and the one its own
    /// log lines carry. Absent on lines written before it was recorded.
    #[serde(default, rename = "requestId")]
    pub request_id: Option<String>,
}

/// What a versioned surface answers instead of a record when it refuses a
/// call: one `error` object carrying a stable code, a sentence, the id the
/// control plane logged the refusal under, and - for a bad request - which
/// fields were wrong.
///
/// Mirrors `contracts/v1/control-error.schema.json`. Everything past the
/// three fields this CLI prints is read leniently, so a member added there
/// does not stop an older CLI from printing the message.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ControlError {
    pub code: String,
    pub message: String,
    #[serde(default)]
    pub request_id: Option<String>,
    #[serde(default)]
    pub violations: Vec<Violation>,
    #[serde(default)]
    pub retryable: bool,
    #[serde(default)]
    pub retry_after_seconds: Option<u64>,
}

/// One field a request got wrong, as the control plane names it.
#[derive(Debug, Clone, Deserialize)]
pub struct Violation {
    pub field: String,
    pub code: String,
    pub message: String,
}

/// The whole body of a refusal on a versioned surface.
#[derive(Debug, Clone, Deserialize)]
struct ErrorEnvelope {
    error: ControlError,
}

/// A refusal in either of the two shapes a control plane sends one.
#[derive(Debug)]
enum Refusal {
    /// The envelope of `/api/v1` and `/api/platform`.
    Structured(ControlError),
    /// The flat `{"error": "<message>"}` of the authorization gate, which
    /// answers that way on every surface.
    Plain(String),
}

impl Refusal {
    /// Read whichever shape the body is, or `None` for a body that is
    /// neither, such as an empty one or a proxy's HTML.
    fn parse(body: &[u8]) -> Option<Self> {
        let value: serde_json::Value = serde_json::from_slice(body).ok()?;
        match value.get("error")? {
            serde_json::Value::String(message) => Some(Self::Plain(message.clone())),
            serde_json::Value::Object(_) => serde_json::from_value::<ErrorEnvelope>(value.clone())
                .ok()
                .map(|envelope| Self::Structured(envelope.error)),
            _ => None,
        }
    }
}

/// One answer from the control plane, body already read.
///
/// Everything a caller decides on is here, so a response is never half read:
/// the status, the headers (`Location` on an accepted mutation, the request
/// id on everything) and the bytes.
struct Reply {
    status: StatusCode,
    headers: HeaderMap,
    body: Vec<u8>,
}

impl Reply {
    async fn read(response: reqwest::Response) -> Self {
        let status = response.status();
        let headers = response.headers().clone();
        let body = response
            .bytes()
            .await
            .map(|bytes| bytes.to_vec())
            .unwrap_or_default();
        Self {
            status,
            headers,
            body,
        }
    }

    /// The id the control plane logged this exchange under, when it said.
    fn request_id(&self) -> Option<String> {
        self.headers
            .get(REQUEST_ID_HEADER)
            .and_then(|value| value.to_str().ok())
            .map(|value| value.to_string())
    }
}

/// What `deis audit` is asking for. Every field is optional and they are
/// combined with "and", which is how an operator narrows a log down to the
/// one thing they are chasing.
#[derive(Debug, Clone, Default)]
pub struct AuditFilter {
    pub app: Option<String>,
    pub environment: Option<String>,
    pub account: Option<String>,
    pub action: Option<String>,
    pub outcome: Option<String>,
    pub since: Option<i64>,
    /// Most lines to read, newest first: one page of at most this many.
    pub limit: Option<usize>,
}

/// A client for one control plane.
pub struct Control {
    base: String,
    token: Option<String>,
    http: reqwest::Client,
}

impl Control {
    pub fn new(base: &str, token: Option<&str>) -> Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .build()
            .context("building the HTTP client")?;
        Ok(Self {
            base: base.trim_end_matches('/').to_string(),
            token: token.map(|t| t.to_string()),
            http,
        })
    }

    pub fn base_url(&self) -> &str {
        &self.base
    }

    fn url(&self, prefix: &str, route: &str) -> String {
        format!("{}{prefix}{route}", self.base)
    }

    /// A request to a product route: `route` is spelled without any prefix
    /// (`/apps/demo`), and [`API_MAJOR`] is put in front of it here.
    fn request(&self, method: Method, route: &str) -> reqwest::RequestBuilder {
        self.build(method, API_MAJOR, route)
    }

    /// A request to a platform diagnostics route, prefixed the same way.
    fn platform_request(&self, method: Method, route: &str) -> reqwest::RequestBuilder {
        self.build(method, API_PLATFORM, route)
    }

    fn build(&self, method: Method, prefix: &str, route: &str) -> reqwest::RequestBuilder {
        let builder = self
            .http
            .request(method, self.url(prefix, route))
            .header(ACCEPT, "application/json");
        match &self.token {
            Some(token) => builder.bearer_auth(token),
            None => builder,
        }
    }

    /// Send one request and read the whole answer: the seam every call goes
    /// through.
    async fn dispatch(&self, builder: reqwest::RequestBuilder) -> Result<Reply> {
        let request = builder
            .build()
            .context("building a request to the control plane")?;
        let response = self
            .http
            .execute(request)
            .await
            .map_err(|e| self.transport_error(e))?;
        Ok(Reply::read(response).await)
    }

    /// Send a request and read its body as `T`, turning every failure into a
    /// message that says what to do next.
    async fn send<T: serde::de::DeserializeOwned>(
        &self,
        builder: reqwest::RequestBuilder,
        what: &str,
    ) -> Result<T> {
        let (_, _, body) = self.send_with_metadata(builder, what).await?;
        Ok(body)
    }

    /// Send a request while retaining status and response headers. Durable
    /// mutations use this to distinguish a completed `201`/`200` from a
    /// `202` whose `Location` must be reconciled before the CLI reports
    /// success.
    async fn send_with_metadata<T: serde::de::DeserializeOwned>(
        &self,
        builder: reqwest::RequestBuilder,
        what: &str,
    ) -> Result<(StatusCode, HeaderMap, T)> {
        let reply = self.dispatch(builder).await?;
        if !reply.status.is_success() {
            bail!(self.http_error(&reply, what));
        }
        let status = reply.status;
        let body = serde_json::from_slice(&reply.body).with_context(|| {
            format!(
                "{what}: the control plane answered {status} with a body this CLI could not read; \
                 it may be a newer or older version than this build"
            )
        })?;
        Ok((status, reply.headers, body))
    }

    /// Read a paginated list route to its end, following `nextCursor` until
    /// the control plane stops handing one out.
    ///
    /// The page size is left to the server's default; a caller that wants a
    /// different one writes `limit=` into `path` itself, and this helper
    /// carries that query along when it appends the cursor.
    async fn list_all<T: serde::de::DeserializeOwned>(
        &self,
        path: &str,
        what: &str,
    ) -> Result<Vec<T>> {
        let mut items: Vec<T> = Vec::new();
        let mut cursor: Option<String> = None;
        for page_number in 1..=MAX_LIST_PAGES {
            let url = match &cursor {
                Some(token) => append_cursor(path, token),
                None => path.to_string(),
            };
            let page: ListPage<T> = self.send(self.request(Method::GET, &url), what).await?;
            check_list_envelope(path, page_number, &page);
            let empty = page.items.is_empty();
            items.extend(page.items);
            let next = page.page.next_cursor;
            match &next {
                None => return Ok(items),
                Some(_) if empty => {
                    // Following this would ask for page after page of
                    // nothing, so refuse instead of hanging on a control
                    // plane that contradicts itself.
                    tracing::error!(
                        path,
                        page_number,
                        "control plane offered another page after an empty one"
                    );
                    bail!(
                        "{what}: the control plane answered an empty page and still offered \
                         another one; it is not paginating this list correctly"
                    );
                }
                Some(token) if cursor.as_deref() == Some(token.as_str()) => {
                    // A cursor handed back unchanged points at the page that
                    // was just read, so following it reads the same rows
                    // again. Left alone that is not a hang but something
                    // worse: the loop only stops at the page ceiling, and
                    // what it returns by then is the same page repeated
                    // thousands of times, which a caller has no way to tell
                    // from a genuinely long history. Refuse on the second
                    // sighting instead, while the answer is still small.
                    tracing::error!(
                        path,
                        page_number,
                        "control plane handed back the cursor it was sent"
                    );
                    bail!(
                        "{what}: the control plane answered with the same cursor it was given, so \
                         the page it offers next is the one just read; it is not paginating this \
                         list correctly"
                    );
                }
                Some(_) => {}
            }
            cursor = next;
        }
        tracing::error!(
            path,
            pages = MAX_LIST_PAGES,
            "gave up following a control-plane list that never ended"
        );
        bail!(
            "{what}: the control plane is still offering more pages after {MAX_LIST_PAGES}; \
             stopping rather than reading forever"
        )
    }

    /// Read the first page of a paginated list route and stop there.
    ///
    /// For the commands that print "the latest N" - `deis audit`, `deis
    /// deployments` - where the newest rows are the whole answer and the rest
    /// of an unbounded history is not worth the requests. The envelope is
    /// checked exactly as [`Self::list_all`] checks every page; what differs
    /// is only that the cursor is handed back to the caller instead of
    /// followed, so the caller can say how much it did not print.
    async fn list_first_page<T: serde::de::DeserializeOwned>(
        &self,
        path: &str,
        what: &str,
    ) -> Result<ListPage<T>> {
        let page: ListPage<T> = self.send(self.request(Method::GET, path), what).await?;
        check_list_envelope(path, 1, &page);
        Ok(page)
    }

    fn transport_error(&self, e: reqwest::Error) -> anyhow::Error {
        anyhow::anyhow!(
            "could not reach the control plane at {}: {e}\nCheck that deis-control is running and \
             that the address is right (see `deis config list`).",
            self.base
        )
    }

    /// One sentence for a refusal, whichever shape it arrived in.
    ///
    /// The versioned envelope is printed as `code: message`, then each field
    /// it names as wrong, then the request id - the one thing an operator can
    /// search the control plane's log for. The flat string is printed as
    /// it is, with the request id from the header when there was one.
    fn http_error(&self, reply: &Reply, what: &str) -> String {
        let status = reply.status;
        let (detail, request_id, retry) = match Refusal::parse(&reply.body) {
            Some(Refusal::Structured(error)) => {
                let mut detail = format!("{}: {}", error.code, error.message);
                for violation in &error.violations {
                    detail.push_str(&format!(
                        "\n  - {}: {} ({})",
                        violation.field, violation.message, violation.code
                    ));
                }
                let retry = error.retryable.then(|| match error.retry_after_seconds {
                    Some(seconds) => {
                        format!("\nThe control plane says to try again in {seconds} seconds.")
                    }
                    None => "\nThe control plane says this may succeed if tried again.".to_string(),
                });
                (
                    detail,
                    error.request_id.or_else(|| reply.request_id()),
                    retry,
                )
            }
            Some(Refusal::Plain(message)) => (message, reply.request_id(), None),
            None => (
                String::from_utf8_lossy(&reply.body).trim().to_string(),
                reply.request_id(),
                None,
            ),
        };
        let detail = if detail.is_empty() {
            "no detail given".to_string()
        } else {
            detail
        };
        let reference = request_id
            .map(|id| format!(" [request {id}]"))
            .unwrap_or_default();
        let retry = retry.unwrap_or_default();

        let advice = match status {
            // 401 and 403 are different problems and lead to different next
            // steps, so they are not one arm: the first is a credential that
            // does not work at all, the second is one that works and does not
            // reach this far. The control plane's own message already names
            // the scope that would have been enough, so this only says who to
            // ask.
            StatusCode::UNAUTHORIZED => match self.token.is_some() {
                true => "\nThe credential this CLI sent was refused: it is unknown to this \
                         control plane, or it was revoked, or its account is switched off. Store \
                         a working one with `deis config set control.token <value>`."
                    .to_string(),
                false => format!(
                    "\nNo credential was sent. Store one with `deis config set control.token \
                     <value>`, or set ${} for this shell.",
                    crate::settings::ENV_CONTROL_TOKEN
                ),
            },
            StatusCode::FORBIDDEN => "\nThis credential works and is not allowed to do that. Ask \
                                      an owner of the platform for a token carrying the scope \
                                      named above (`deis token create`), or for a wider role on \
                                      your account."
                .to_string(),
            StatusCode::NOT_FOUND => {
                // A 404 means this CLI named something the control plane
                // does not have - an application, environment or version
                // most often, but `deis token list` and `deis token create`
                // provoke one for an unknown `--account` too. The control
                // plane's own message says which, so this only names the
                // commands that list each kind.
                "\nThe control plane has no such record; its message above names it. `deis \
                 versions` lists an application's versions, `deis status` its environments and \
                 what each runs, and `deis account list` the accounts."
                    .to_string()
            }
            StatusCode::SERVICE_UNAVAILABLE => {
                "\nThe control plane is missing something it needs - a secret master key, most \
                 often. Its own log says which."
                    .to_string()
            }
            _ => String::new(),
        };

        format!(
            "{what} failed: the control plane answered {status}: {detail}{reference}{retry}{advice}"
        )
    }

    // ---- applications -------------------------------------------------

    pub async fn get_app(&self, app_id: &str) -> Result<Option<App>> {
        let reply = self
            .dispatch(self.request(Method::GET, &format!("/apps/{app_id}")))
            .await?;
        if reply.status == StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if !reply.status.is_success() {
            bail!(self.http_error(&reply, &format!("looking up app {app_id}")));
        }
        Ok(Some(serde_json::from_slice(&reply.body).with_context(
            || format!("reading the description of app {app_id}"),
        )?))
    }

    pub async fn create_app(&self, app_id: &str) -> Result<App> {
        self.send(
            self.request(Method::POST, "/apps")
                .json(&serde_json::json!({ "id": app_id })),
            &format!("creating app {app_id}"),
        )
        .await
    }

    /// Create the application unless it is already there. Returns true when
    /// this call is what created it.
    pub async fn ensure_app(&self, app_id: &str) -> Result<bool> {
        if self.get_app(app_id).await?.is_some() {
            return Ok(false);
        }
        self.create_app(app_id).await?;
        Ok(true)
    }

    // ---- versions -----------------------------------------------------

    /// Upload and revalidate the provider-neutral Build Output before any
    /// provider payload derived from it can become a version.
    pub async fn upload_canonical_artifact(
        &self,
        app_id: &str,
        transport: Vec<u8>,
        artifact_sha256: &str,
    ) -> Result<ArtifactUploadResponse> {
        self.send(
            self.request(
                Method::POST,
                &format!("/apps/{app_id}/artifacts?sha256={artifact_sha256}"),
            )
            .header(reqwest::header::CONTENT_TYPE, "application/zip")
            .body(transport),
            &format!("uploading canonical artifact for app {app_id}"),
        )
        .await
    }

    /// Upload a native bundle only through the derivation that binds it to a
    /// previously verified canonical parent.
    pub async fn upload_version_with_lineage(
        &self,
        app_id: &str,
        artifact_id: CanonicalId,
        envelope: Vec<u8>,
        bundle_sha256: &str,
    ) -> Result<UploadResponse> {
        self.send(
            self.request(
                Method::POST,
                &format!("/apps/{app_id}/artifacts/{artifact_id}/versions?sha256={bundle_sha256}"),
            )
            .header(
                reqwest::header::CONTENT_TYPE,
                "application/vnd.deislet.provider-envelope.v1+zip",
            )
            .body(envelope),
            &format!("uploading a lineage-bound bundle for app {app_id}"),
        )
        .await
    }

    /// Store a payload an external provider driver deploys, bound to the
    /// canonical artifact it was derived from.
    pub async fn upload_provider_payload(
        &self,
        app_id: &str,
        artifact_id: CanonicalId,
        envelope: Vec<u8>,
    ) -> Result<ProviderPayloadUpload> {
        self.send(
            self.request(
                Method::POST,
                &format!("/apps/{app_id}/artifacts/{artifact_id}/provider-payloads"),
            )
            .header(
                reqwest::header::CONTENT_TYPE,
                "application/vnd.deislet.provider-envelope.v1+zip",
            )
            .body(envelope),
            &format!("uploading a provider payload for app {app_id}"),
        )
        .await
    }

    pub async fn list_versions(&self, app_id: &str) -> Result<Vec<Version>> {
        self.list_all(
            &format!("/apps/{app_id}/versions"),
            &format!("listing the versions of app {app_id}"),
        )
        .await
    }

    // ---- environments -------------------------------------------------

    pub async fn list_environments(&self, app_id: &str) -> Result<Vec<Environment>> {
        self.list_all(
            &format!("/apps/{app_id}/environments"),
            &format!("listing the environments of app {app_id}"),
        )
        .await
    }

    /// Create the environment unless it is already there. Returns true when
    /// this call is what created it.
    pub async fn ensure_environment(&self, app_id: &str, environment: &str) -> Result<bool> {
        let reply = self
            .dispatch(self.request(
                Method::GET,
                &format!("/apps/{app_id}/environments/{environment}"),
            ))
            .await?;
        if reply.status.is_success() {
            return Ok(false);
        }
        if reply.status != StatusCode::NOT_FOUND {
            bail!(self.http_error(
                &reply,
                &format!("looking up environment {environment} of app {app_id}")
            ));
        }
        let _: Environment = self
            .send(
                self.request(Method::POST, &format!("/apps/{app_id}/environments"))
                    .json(&serde_json::json!({ "name": environment })),
                &format!("creating environment {environment} of app {app_id}"),
            )
            .await?;
        Ok(true)
    }

    // ---- deployments --------------------------------------------------

    pub async fn deploy(
        &self,
        app_id: &str,
        environment: &str,
        version_id: &str,
        note: Option<&str>,
        config_values: &std::collections::BTreeMap<String, serde_json::Value>,
    ) -> Result<DeploymentResponse> {
        let mut body = serde_json::json!({ "version_id": version_id });
        if let Some(note) = note {
            body["note"] = serde_json::Value::String(note.to_string());
        }
        if !config_values.is_empty() {
            body["config_values"] = serde_json::to_value(config_values)?;
        }
        let what = format!("pointing {environment} of app {app_id} at a version");
        let (status, headers, response) = self
            .send_with_metadata::<DeploymentMutationResponse>(
                self.request(
                    Method::POST,
                    &format!("/apps/{app_id}/environments/{environment}/deployments"),
                )
                .json(&body),
                &what,
            )
            .await?;
        self.finish_deployment_mutation(app_id, environment, status, &headers, response, &what)
            .await
    }

    /// Move the pointer backwards. `version_number` unset means "the previous
    /// distinct version this environment ran".
    pub async fn rollback(
        &self,
        app_id: &str,
        environment: &str,
        version_number: Option<i64>,
        note: Option<&str>,
    ) -> Result<DeploymentResponse> {
        let mut body = serde_json::Map::new();
        if let Some(number) = version_number {
            body.insert("version_number".to_string(), serde_json::json!(number));
        }
        if let Some(note) = note {
            body.insert("note".to_string(), serde_json::json!(note));
        }
        let what = format!("rolling {environment} of app {app_id} back");
        let (status, headers, response) = self
            .send_with_metadata::<DeploymentMutationResponse>(
                self.request(
                    Method::POST,
                    &format!("/apps/{app_id}/environments/{environment}/rollback"),
                )
                .json(&serde_json::Value::Object(body)),
                &what,
            )
            .await?;
        self.finish_deployment_mutation(app_id, environment, status, &headers, response, &what)
            .await
    }

    async fn finish_deployment_mutation(
        &self,
        app_id: &str,
        environment: &str,
        status: StatusCode,
        headers: &HeaderMap,
        mut response: DeploymentMutationResponse,
        what: &str,
    ) -> Result<DeploymentResponse> {
        let plan = self
            .await_mutation_plan(
                app_id,
                environment,
                status,
                headers,
                response.plan.take(),
                what,
            )
            .await?;
        if let Some(plan) = &plan {
            response.plan = Some(plan.clone());
        }

        // A waited plan is complete only when its exact pointer is visible
        // through the ordinary scoped read path. This also prevents a stale
        // or malformed 202 body from being printed as the current deployment.
        if plan.as_ref().is_some_and(|plan| plan.state == "active")
            && (status == StatusCode::ACCEPTED || response.deployment.is_none())
        {
            let current = match self.current_deployment(app_id, environment).await? {
                EnvironmentView::Running(current) => *current,
                EnvironmentView::NeverDeployed => bail!(
                    "deployment plan {} is active but {environment} of app {app_id} has no production pointer",
                    plan.as_ref().expect("active plan was checked").id
                ),
                EnvironmentView::NotPermitted => bail!(
                    "deployment plan {} completed, but this credential cannot verify the production pointer of {environment} of app {app_id}",
                    plan.as_ref().expect("active plan was checked").id
                ),
            };
            let plan_id = &plan.as_ref().expect("active plan was checked").id;
            if current.deployment.id != *plan_id {
                bail!(
                    "deployment plan {plan_id} became active, but {environment} of app {app_id} points at deployment {}; refusing to report the wrong version as this operation's result",
                    current.deployment.id
                );
            }
            response.deployment = Some(current.deployment);
        }

        let deployment = response.deployment.ok_or_else(|| {
            anyhow::anyhow!(
                "{what}: the control plane returned no production deployment after the operation completed"
            )
        })?;
        if let Some(delivery) = &mut response.delivery
            && plan.as_ref().is_some_and(|plan| plan.state == "active")
        {
            delivery.phase = "activate".to_string();
        }
        Ok(DeploymentResponse {
            deployment,
            environment: response.environment,
            version: response.version,
            delivery: response.delivery,
            plan: response.plan,
        })
    }

    /// Wait for an accepted plan through the same application/environment
    /// scope as the mutation. `Location` is validated rather than followed as
    /// an arbitrary URL, so a compromised or incompatible Control response
    /// cannot turn the CLI into a credential-bearing redirect client.
    async fn await_mutation_plan(
        &self,
        app_id: &str,
        environment: &str,
        status: StatusCode,
        headers: &HeaderMap,
        plan: Option<DeploymentPlanSummary>,
        what: &str,
    ) -> Result<Option<DeploymentPlanSummary>> {
        let Some(plan) = plan else {
            if status == StatusCode::ACCEPTED {
                bail!("{what}: the control plane accepted the operation without a durable plan");
            }
            return Ok(None);
        };
        if plan.state == "active" && status != StatusCode::ACCEPTED {
            return Ok(Some(plan));
        }

        let route = format!(
            "/apps/{app_id}/environments/{environment}/deployment-plans/{}",
            plan.id
        );
        let path = format!("{API_MAJOR}{route}");
        let location = headers
            .get(reqwest::header::LOCATION)
            .ok_or_else(|| anyhow::anyhow!("{what}: accepted plan {} has no Location", plan.id))?
            .to_str()
            .context("reading deployment plan Location")?;
        if location != path {
            bail!(
                "{what}: deployment plan {} returned Location {location:?}, expected the scoped path {path:?}",
                plan.id
            );
        }
        self.wait_for_deployment_plan(&route, &path, &plan.id, what)
            .await
            .map(Some)
    }

    /// Poll a plan's status at `route` until it settles. `path` is the same
    /// resource with its prefix, for the message that names where the
    /// status stays when this gives up.
    async fn wait_for_deployment_plan(
        &self,
        route: &str,
        path: &str,
        expected_plan_id: &str,
        what: &str,
    ) -> Result<DeploymentPlanSummary> {
        let deadline = tokio::time::Instant::now() + DEPLOYMENT_WAIT_TIMEOUT;
        loop {
            let response: DeploymentPlanStatusResponse = self
                .send(
                    self.request(Method::GET, route),
                    &format!("reconciling deployment plan {expected_plan_id}"),
                )
                .await?;
            if response.plan.id != expected_plan_id {
                bail!(
                    "{what}: plan status returned {}, expected {expected_plan_id}",
                    response.plan.id
                );
            }
            match response.plan.state.as_str() {
                "active" => return Ok(response.plan),
                "failed" | "aborted" => {
                    let detail = response
                        .plan
                        .last_error
                        .as_deref()
                        .unwrap_or("no failure detail was recorded");
                    bail!(
                        "{what}: deployment plan {expected_plan_id} ended as {}: {detail}",
                        response.plan.state
                    );
                }
                "preparing" | "prepared" | "aborting" | "activating" | "reconciling"
                | "degraded" => {}
                state => bail!(
                    "{what}: deployment plan {expected_plan_id} returned unknown state {state:?}"
                ),
            }
            if tokio::time::Instant::now() >= deadline {
                bail!(
                    "{what}: deployment plan {expected_plan_id} is still {} after {} seconds; its durable status remains at {path}",
                    response.plan.state,
                    DEPLOYMENT_WAIT_TIMEOUT.as_secs()
                );
            }
            tokio::time::sleep(DEPLOYMENT_POLL_INTERVAL).await;
        }
    }

    /// The version an environment currently points at, or why this credential
    /// cannot say.
    pub async fn current_deployment(
        &self,
        app_id: &str,
        environment: &str,
    ) -> Result<EnvironmentView> {
        let reply = self
            .dispatch(self.request(
                Method::GET,
                &format!("/apps/{app_id}/environments/{environment}/deployment"),
            ))
            .await?;
        if reply.status == StatusCode::NOT_FOUND {
            return Ok(EnvironmentView::NeverDeployed);
        }
        // Not an error: a credential scoped to one environment is *meant* to
        // be refused the others, and a command that walks every environment
        // of an application must be able to say so rather than stop.
        if reply.status == StatusCode::FORBIDDEN {
            return Ok(EnvironmentView::NotPermitted);
        }
        if !reply.status.is_success() {
            bail!(self.http_error(
                &reply,
                &format!("reading what {environment} of app {app_id} runs")
            ));
        }
        Ok(EnvironmentView::Running(Box::new(
            serde_json::from_slice(&reply.body)
                .with_context(|| format!("reading what {environment} of app {app_id} runs"))?,
        )))
    }

    /// The newest `limit` deployments of one environment, and how many the
    /// environment has behind them.
    ///
    /// One page on purpose. An environment gains a deployment on every
    /// deploy, every rollback and every secret change, so its history has no
    /// ceiling; `deis deployments` prints the top of it and says how far down
    /// it goes rather than walking a year of rows to show twenty.
    pub async fn list_deployments(
        &self,
        app_id: &str,
        environment: &str,
        limit: usize,
    ) -> Result<DeploymentHistory> {
        let what = format!("listing the deployments of {environment} of app {app_id}");
        let limit = bounded_page_limit(limit, &what);
        let path = format!("/apps/{app_id}/environments/{environment}/deployments?limit={limit}");
        let page: ListPage<Deployment> = self.list_first_page(&path, &what).await?;
        if page.page.next_cursor.is_some() {
            tracing::debug!(
                app_id,
                environment,
                shown = page.items.len(),
                total = page.page.total,
                "this environment has more deployments than one page holds; raise --limit to \
                 read further back"
            );
        }
        Ok(DeploymentHistory {
            deployments: page.items,
            total: page.page.total,
        })
    }

    // ---- provider deployments -----------------------------------------

    /// The path of one workflow below the product prefix.
    ///
    /// `operation_id` is a [`CanonicalId`] rather than a string precisely
    /// because this becomes a URL path: an identity that has already been
    /// parsed cannot carry a slash, a `..` or a query string into it, so no
    /// call site has to remember to check.
    fn provider_deployment_route(
        app_id: &str,
        environment: &str,
        operation_id: CanonicalId,
    ) -> String {
        format!("/apps/{app_id}/environments/{environment}/provider-deployments/{operation_id}")
    }

    /// The newest `limit` provider deployments of one environment, and how
    /// many the environment has behind them.
    ///
    /// One page, for the same reason [`Self::list_deployments`] reads one: an
    /// environment gains a workflow on every provider deploy, cancel and
    /// rollback, so the history has no ceiling and the newest rows are the
    /// answer.
    pub async fn list_provider_deployments(
        &self,
        app_id: &str,
        environment: &str,
        limit: usize,
    ) -> Result<ProviderDeploymentHistory> {
        let what = format!("listing the provider deployments of {environment} of app {app_id}");
        let limit = bounded_page_limit(limit, &what);
        let path =
            format!("/apps/{app_id}/environments/{environment}/provider-deployments?limit={limit}");
        let page: ListPage<ProviderDeployment> = self.list_first_page(&path, &what).await?;
        if page.page.next_cursor.is_some() {
            tracing::debug!(
                app_id,
                environment,
                shown = page.items.len(),
                total = page.page.total,
                "this environment has more provider deployments than one page holds; raise \
                 --limit to read further back"
            );
        }
        Ok(ProviderDeploymentHistory {
            deployments: page.items,
            total: page.page.total,
        })
    }

    /// One provider deployment workflow, and the revision this exact read
    /// observed.
    pub async fn get_provider_deployment(
        &self,
        app_id: &str,
        environment: &str,
        operation_id: CanonicalId,
    ) -> Result<ProviderDeploymentRead> {
        let what =
            format!("reading provider deployment {operation_id} of {environment} of app {app_id}");
        let route = Self::provider_deployment_route(app_id, environment, operation_id);
        let (_, headers, workflow) = self
            .send_with_metadata::<ProviderDeployment>(self.request(Method::GET, &route), &what)
            .await?;
        if workflow.operation.operation_id != operation_id {
            bail!(
                "{what}: the control plane answered with operation {}, which is not the one that \
                 was asked for",
                workflow.operation.operation_id
            );
        }
        let etag = read_etag(&headers, &workflow.operation);
        Ok(ProviderDeploymentRead { workflow, etag })
    }

    /// Ask one provider workflow to stop.
    ///
    /// Two calls on purpose, and they cannot be folded into one. The route is
    /// fenced on the revision the caller *saw*, so the workflow is read first
    /// and that read's `ETag` goes back as `If-Match`. If the operation moved
    /// in between - it succeeded, it failed, somebody else cancelled it - the
    /// control plane answers 412 and this call has changed nothing, which is
    /// the fence working rather than a call to repeat blindly.
    ///
    /// The route makes an `Idempotency-Key` optional; one is sent anyway. It
    /// covers the case the fence does not: a request that reached the control
    /// plane and whose *answer* did not come back - a reset connection, a
    /// proxy that resends - is one set of bytes delivered twice, and the key
    /// is what makes the second delivery replay the first cancel rather than
    /// record a second one. A fresh key per invocation is deliberate: two
    /// deliveries of one command share a key, two commands do not.
    pub async fn cancel_provider_deployment(
        &self,
        app_id: &str,
        environment: &str,
        operation_id: CanonicalId,
    ) -> Result<ProviderDeploymentCancel> {
        let read = self
            .get_provider_deployment(app_id, environment, operation_id)
            .await?;
        let what = format!(
            "cancelling provider deployment {operation_id} of {environment} of app {app_id}"
        );
        let route = format!(
            "{}/cancel",
            Self::provider_deployment_route(app_id, environment, operation_id)
        );
        tracing::info!(
            app_id,
            environment,
            %operation_id,
            observed_state = %read.workflow.operation.state,
            if_match = %read.etag,
            "requesting cancellation of a provider deployment"
        );
        let (_, _, operation) = self
            .send_with_metadata::<ProviderOperationSummary>(
                self.request(Method::POST, &route)
                    .header(reqwest::header::IF_MATCH, &read.etag)
                    .header(IDEMPOTENCY_KEY, new_idempotency_key()),
                &what,
            )
            .await?;
        if operation.operation_id != operation_id {
            bail!(
                "{what}: the control plane answered with operation {}, which is not the one that \
                 was cancelled",
                operation.operation_id
            );
        }
        Ok(ProviderDeploymentCancel {
            observed: read.workflow,
            operation,
        })
    }

    /// Restore the provider deployment one succeeded workflow left behind.
    ///
    /// This does not undo the named operation; it records a *new* rollback
    /// operation that drives the driver back to that deployment's exact
    /// desired state. The answer is therefore a different operation id from
    /// the one that was asked about, and that id is what the caller polls.
    ///
    /// The `Idempotency-Key` is required by the route, and a fresh one is
    /// minted per invocation. That is the right grain: a redelivery of these
    /// exact bytes replays the one rollback, while an operator who runs the
    /// command twice has asked for two rollbacks and gets two - a second
    /// rollback of an unchanged deployment is harmless, and silently
    /// answering the first would hide a control plane that never started it.
    pub async fn rollback_provider_deployment(
        &self,
        app_id: &str,
        environment: &str,
        operation_id: CanonicalId,
    ) -> Result<ProviderOperationSummary> {
        let what = format!(
            "rolling provider deployment {operation_id} of {environment} of app {app_id} back"
        );
        let route = format!(
            "{}/rollback",
            Self::provider_deployment_route(app_id, environment, operation_id)
        );
        tracing::info!(
            app_id,
            environment,
            rolled_back_from = %operation_id,
            "requesting a provider deployment rollback"
        );
        let (_, headers, operation) = self
            .send_with_metadata::<ProviderOperationSummary>(
                self.request(Method::POST, &route)
                    .header(IDEMPOTENCY_KEY, new_idempotency_key()),
                &what,
            )
            .await?;
        if operation.operation_id == operation_id {
            bail!(
                "{what}: the control plane answered with the operation that was rolled back \
                 rather than the new rollback operation"
            );
        }
        // The `Location` is checked against the path this CLI would have
        // built rather than followed as an arbitrary URL, exactly as an
        // accepted deployment plan is: a control plane that is compromised or
        // simply incompatible must not be able to redirect a credential-
        // bearing client somewhere of its choosing.
        let expected = format!(
            "{}{}",
            API_MAJOR,
            Self::provider_deployment_route(app_id, environment, operation.operation_id)
        );
        let location = headers
            .get(reqwest::header::LOCATION)
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "{what}: rollback operation {} was accepted without a Location to poll",
                    operation.operation_id
                )
            })?
            .to_str()
            .context("reading the rollback operation Location")?;
        if location != expected {
            bail!(
                "{what}: rollback operation {} returned Location {location:?}, expected the \
                 scoped path {expected:?}",
                operation.operation_id
            );
        }
        Ok(operation)
    }

    /// The provider targets this environment may deploy to right now.
    pub async fn list_provider_target_grants(
        &self,
        app_id: &str,
        environment: &str,
    ) -> Result<Vec<ProviderTargetGrant>> {
        self.list_all(
            &format!(
                "/apps/{app_id}/environments/{environment}/provider-target-grants?status=active"
            ),
            &format!("listing the provider targets {environment} of app {app_id} may deploy to"),
        )
        .await
    }

    /// Record a provider deployment of `artifact` to `provider_target_id`.
    ///
    /// Idempotent per invocation, like a rollback: a redelivery of these
    /// bytes answers the one operation, while running the command again asks
    /// for a second deployment.
    pub async fn begin_provider_deployment(
        &self,
        app_id: &str,
        environment: &str,
        provider_target_id: CanonicalId,
        artifact: &ProviderArtifactRef,
        config_values: &std::collections::BTreeMap<String, serde_json::Value>,
    ) -> Result<ProviderOperationSummary> {
        let what = format!(
            "starting a provider deployment of {environment} of app {app_id} to target {provider_target_id}"
        );
        let mut body = serde_json::json!({
            "providerTargetId": provider_target_id,
            "artifact": artifact,
        });
        if !config_values.is_empty() {
            body["configValues"] = serde_json::to_value(config_values)?;
        }
        let (_, headers, operation) = self
            .send_with_metadata::<ProviderOperationSummary>(
                self.request(
                    Method::POST,
                    &format!("/apps/{app_id}/environments/{environment}/provider-deployments"),
                )
                .header(IDEMPOTENCY_KEY, new_idempotency_key())
                .json(&body),
                &what,
            )
            .await?;
        // Checked against the path this CLI would have built, never followed
        // as an arbitrary URL; see `rollback_provider_deployment`.
        let expected = format!(
            "{}{}",
            API_MAJOR,
            Self::provider_deployment_route(app_id, environment, operation.operation_id)
        );
        let location = headers
            .get(reqwest::header::LOCATION)
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "{what}: operation {} was accepted without a Location to poll",
                    operation.operation_id
                )
            })?
            .to_str()
            .context("reading the provider deployment Location")?;
        if location != expected {
            bail!(
                "{what}: operation {} returned Location {location:?}, expected the scoped path \
                 {expected:?}",
                operation.operation_id
            );
        }
        Ok(operation)
    }

    /// Ask the control plane to take an environment's provider deployment
    /// down. `purge_environment`, when given, is the consent to destroy the
    /// environment's provider data and must name `environment`.
    pub async fn undeploy_provider_deployment(
        &self,
        app_id: &str,
        environment: &str,
        provider_target_id: Option<CanonicalId>,
        purge_environment: Option<&str>,
    ) -> Result<ProviderOperationSummary> {
        let what = format!("taking down the provider deployment of {environment} of app {app_id}");
        let mut body = serde_json::json!({});
        if let Some(target) = provider_target_id {
            body["providerTargetId"] = serde_json::to_value(target)?;
        }
        if let Some(purge) = purge_environment {
            body["purgeData"] = serde_json::json!({ "environment": purge });
        }
        let (_, headers, operation) = self
            .send_with_metadata::<ProviderOperationSummary>(
                self.request(
                    Method::POST,
                    &format!("/apps/{app_id}/environments/{environment}/provider-undeployments"),
                )
                .header(IDEMPOTENCY_KEY, new_idempotency_key())
                .json(&body),
                &what,
            )
            .await?;
        // As for a deployment: checked against the path this CLI would have
        // built, never followed as an arbitrary URL.
        let expected = format!(
            "{}{}",
            API_MAJOR,
            Self::provider_deployment_route(app_id, environment, operation.operation_id)
        );
        let location = headers
            .get(reqwest::header::LOCATION)
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "{what}: operation {} was accepted without a Location to poll",
                    operation.operation_id
                )
            })?
            .to_str()
            .context("reading the provider undeploy Location")?;
        if location != expected {
            bail!(
                "{what}: operation {} returned Location {location:?}, expected the scoped path \
                 {expected:?}",
                operation.operation_id
            );
        }
        Ok(operation)
    }

    /// Poll one provider workflow until its operation stops moving.
    ///
    /// Paced by a [`tokio::time::interval`] rather than by sleeping for the
    /// gap after each read, so a control plane that answers slowly does not
    /// silently stretch the polling period, and a burst of missed ticks does
    /// not turn into a burst of requests ([`MissedTickBehavior::Delay`]).
    ///
    /// A state this build does not know is *not* treated as finished. The
    /// wait then ends at the timeout with a message naming that state, which
    /// is a slow honest answer; calling it finished would be a fast wrong one
    /// that reports a running deployment as done.
    pub async fn wait_for_provider_deployment(
        &self,
        app_id: &str,
        environment: &str,
        operation_id: CanonicalId,
    ) -> Result<ProviderDeployment> {
        let what = format!(
            "waiting for provider deployment {operation_id} of {environment} of app {app_id}"
        );
        let path = format!(
            "{}{}",
            API_MAJOR,
            Self::provider_deployment_route(app_id, environment, operation_id)
        );
        let deadline = tokio::time::Instant::now() + PROVIDER_DEPLOYMENT_WAIT_TIMEOUT;
        let mut ticker = tokio::time::interval(PROVIDER_DEPLOYMENT_POLL_INTERVAL);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut warned_about_unknown_state = false;
        loop {
            // The first tick completes immediately, so the workflow is read
            // once before any waiting happens.
            ticker.tick().await;
            let read = self
                .get_provider_deployment(app_id, environment, operation_id)
                .await?;
            match read.workflow.operation.settled() {
                Some(true) => return Ok(read.workflow),
                Some(false) => {}
                None if !warned_about_unknown_state => {
                    warned_about_unknown_state = true;
                    tracing::warn!(
                        %operation_id,
                        state = %read.workflow.operation.state,
                        cli_version = env!("CARGO_PKG_VERSION"),
                        "the control plane reports a provider operation state this CLI does not \
                         know; waiting on it as unfinished, which will end at the timeout even if \
                         it is in fact a settled state - upgrade this CLI"
                    );
                }
                None => {}
            }
            if tokio::time::Instant::now() >= deadline {
                bail!(
                    "{what}: it is still {} after {} seconds; its durable status remains at {path}",
                    read.workflow.operation.state,
                    PROVIDER_DEPLOYMENT_WAIT_TIMEOUT.as_secs()
                );
            }
        }
    }

    // ---- routes -------------------------------------------------------

    /// One environment's routes, in the order the edge applies them: highest
    /// priority first, then by path prefix.
    ///
    /// Followed to the end rather than read one page deep, because a routing
    /// table read that stopped halfway would show an operator a prefix and
    /// hide the higher-priority one that actually wins - and unlike a
    /// deployment history, a routing table is small by construction.
    pub async fn list_routes(&self, app_id: &str, environment: &str) -> Result<Vec<Route>> {
        self.list_all(
            &format!("/apps/{app_id}/environments/{environment}/routes"),
            &format!("listing the routes of {environment} of app {app_id}"),
        )
        .await
    }

    // ---- domain grants ------------------------------------------------

    /// The whole hostname registry, in `(pattern, application, environment)`
    /// order.
    ///
    /// Platform-wide, and read with `admin:*/*`: a hostname belongs to
    /// nobody's application, which is the whole reason the registry exists.
    pub async fn list_domain_grants(&self) -> Result<Vec<DomainGrant>> {
        self.list_all("/domains", "listing the domain grants").await
    }

    // ---- secrets ------------------------------------------------------

    pub async fn put_secret(
        &self,
        app_id: &str,
        environment: &str,
        name: &str,
        value: &str,
    ) -> Result<SecretChangeResponse> {
        let what = format!("storing secret {name} of {environment}");
        let (status, headers, mut response) = self
            .send_with_metadata::<SecretChangeResponse>(
                self.request(
                    Method::PUT,
                    &format!("/apps/{app_id}/environments/{environment}/secrets/{name}"),
                )
                .json(&serde_json::json!({ "value": value })),
                &what,
            )
            .await?;
        response.plan = self
            .await_mutation_plan(
                app_id,
                environment,
                status,
                &headers,
                response.plan.take(),
                &what,
            )
            .await?;
        if response
            .plan
            .as_ref()
            .is_some_and(|plan| plan.state == "active")
        {
            response.delivery.phase = "activate".to_string();
        }
        Ok(response)
    }

    /// Every secret name of one environment, in name order. Values never
    /// travel on this route; see the control plane's secret endpoints.
    pub async fn list_secrets(&self, app_id: &str, environment: &str) -> Result<Vec<SecretMeta>> {
        self.list_all(
            &format!("/apps/{app_id}/environments/{environment}/secrets"),
            &format!("listing the secrets of {environment} of app {app_id}"),
        )
        .await
    }

    pub async fn delete_secret(
        &self,
        app_id: &str,
        environment: &str,
        name: &str,
    ) -> Result<SecretDeletedResponse> {
        let what = format!("deleting secret {name} of {environment}");
        let (status, headers, mut response) = self
            .send_with_metadata::<SecretDeletedResponse>(
                self.request(
                    Method::DELETE,
                    &format!("/apps/{app_id}/environments/{environment}/secrets/{name}"),
                ),
                &what,
            )
            .await?;
        response.plan = self
            .await_mutation_plan(
                app_id,
                environment,
                status,
                &headers,
                response.plan.take(),
                &what,
            )
            .await?;
        if response
            .plan
            .as_ref()
            .is_some_and(|plan| plan.state == "active")
        {
            response.delivery.phase = "activate".to_string();
        }
        Ok(response)
    }

    // ---- storage --------------------------------------------------------

    /// The address and capability token `deis kv`, `deis db` and `deis blob`
    /// need to act as one tenant of deis-store.
    ///
    /// Served by `deis-control`'s
    /// `api::catalog::storage::storage_token`, behind the same API-key
    /// middleware as the deploy route and gated on the same application and
    /// environment having to exist. The token is signed there, for 15
    /// minutes, and recorded in the issuance table: a leaked CLI credential
    /// still only ever proves the holder may deploy and inspect *through the
    /// control plane's own checks*, and never hands out the key capabilities
    /// are signed with.
    ///
    /// A control plane with no deis-store address
    /// answers 409 with a message naming the key to set, which the generic
    /// error path below prints as it stands: the caller is told what to fix
    /// rather than handed an empty token that fails later inside deis-store.
    pub async fn storage_credentials(
        &self,
        app_id: &str,
        environment: &str,
    ) -> Result<StorageCredentials> {
        self.send(
            self.request(
                Method::GET,
                &format!("/apps/{app_id}/environments/{environment}/storage-token"),
            ),
            &format!("obtaining a storage capability token for {environment} of app {app_id}"),
        )
        .await
    }

    // ---- telemetry --------------------------------------------------------

    /// A read capability for deis-telemetry: `deis logs`, `deis tail` and
    /// `deis trace` present it on every call.
    ///
    /// Served by `deis-control`'s `api::catalog::telemetry`, gated on
    /// `read` over exactly what is asked for - one environment, or every
    /// environment of the application when `environment` is `None` - and
    /// recorded in the audit log every time.
    pub async fn telemetry_credentials(
        &self,
        app_id: &str,
        environment: Option<&str>,
    ) -> Result<TelemetryCredentials> {
        let (path, what) = match environment {
            Some(environment) => (
                format!("/apps/{app_id}/environments/{environment}/telemetry-token"),
                format!("obtaining a telemetry read capability for {environment} of app {app_id}"),
            ),
            None => (
                format!("/apps/{app_id}/telemetry-token"),
                format!(
                    "obtaining a telemetry read capability for every environment of app {app_id}"
                ),
            ),
        };
        self.send(self.request(Method::GET, &path), &what).await
    }

    // ---- accounts, tokens and the audit log ---------------------------

    pub async fn list_accounts(&self) -> Result<Vec<Account>> {
        self.list_all("/accounts", "listing the accounts").await
    }

    pub async fn create_account(&self, name: &str, role: &str) -> Result<Account> {
        self.send(
            self.request(Method::POST, "/accounts")
                .json(&serde_json::json!({ "name": name, "role": role })),
            &format!("creating account {name}"),
        )
        .await
    }

    /// Change an account's role, switch it off, or switch it back on. Every
    /// argument that is `None` leaves that half alone.
    pub async fn update_account(
        &self,
        name: &str,
        role: Option<&str>,
        disabled: Option<bool>,
    ) -> Result<Account> {
        let mut body = serde_json::Map::new();
        if let Some(role) = role {
            body.insert("role".to_string(), serde_json::json!(role));
        }
        if let Some(disabled) = disabled {
            body.insert("disabled".to_string(), serde_json::json!(disabled));
        }
        self.send(
            self.request(Method::PATCH, &format!("/accounts/{name}"))
                .json(&serde_json::Value::Object(body)),
            &format!("updating account {name}"),
        )
        .await
    }

    /// Mint a token. The secret in the answer is the only copy that will ever
    /// exist; the control plane keeps a digest and cannot show it again.
    ///
    /// `scopes` empty means "the account's role over every application",
    /// which is what the control plane fills in; `unscoped` asks for the old
    /// flat behaviour and is refused together with a scope list.
    pub async fn create_token(
        &self,
        account: &str,
        name: &str,
        scopes: &[String],
        unscoped: bool,
    ) -> Result<MintedToken> {
        let mut body = serde_json::Map::new();
        body.insert("name".to_string(), serde_json::json!(name));
        if unscoped {
            body.insert("unscoped".to_string(), serde_json::json!(true));
        } else if !scopes.is_empty() {
            body.insert("scopes".to_string(), serde_json::json!(scopes));
        }
        self.send(
            self.request(Method::POST, &format!("/accounts/{account}/tokens"))
                .json(&serde_json::Value::Object(body)),
            &format!("minting a token for account {account}"),
        )
        .await
    }

    /// Every credential the control plane knows about, and how many of
    /// them answer to no scope at all.
    ///
    /// Two reads: the listing itself, paged to the end, and one page of
    /// size one under `scope=unscoped` whose total is the number of live
    /// root credentials - live whatever `include_revoked` says, because
    /// that is the number an operator drives to zero.
    pub async fn list_tokens(
        &self,
        account: Option<&str>,
        include_revoked: bool,
    ) -> Result<TokenListing> {
        let account_query = account
            .map(|account| format!("&account={account}"))
            .unwrap_or_default();
        let tokens = self
            .list_all(
                &format!("/tokens?include_revoked={include_revoked}&scope=all{account_query}"),
                "listing the tokens",
            )
            .await?;
        let unscoped: ListPage<ApiToken> = self
            .send(
                self.request(
                    Method::GET,
                    &format!("/tokens?include_revoked=false&scope=unscoped&limit=1{account_query}"),
                ),
                "counting the unscoped tokens",
            )
            .await?;
        Ok(TokenListing {
            tokens,
            unscoped: unscoped.page.total,
        })
    }

    pub async fn revoke_token(&self, id: &str) -> Result<ApiToken> {
        self.send(
            self.request(Method::DELETE, &format!("/tokens/{id}")),
            &format!("revoking token {id}"),
        )
        .await
    }

    pub async fn read_audit(&self, filter: &AuditFilter) -> Result<Vec<AuditEntry>> {
        let mut query: Vec<String> = Vec::new();
        let mut push = |key: &str, value: Option<String>| {
            if let Some(value) = value {
                query.push(format!("{key}={value}"));
            }
        };
        push("app", filter.app.clone());
        push("environment", filter.environment.clone());
        push("account", filter.account.clone());
        push("action", filter.action.clone());
        push("outcome", filter.outcome.clone());
        push("since", filter.since.map(|s| s.to_string()));
        if let Some(limit) = filter.limit {
            push(
                "limit",
                Some(bounded_page_limit(limit, "the audit log").to_string()),
            );
        }

        let path = if query.is_empty() {
            "/audit".to_string()
        } else {
            format!("/audit?{}", query.join("&"))
        };
        // One page, newest first: `deis audit` prints "the latest N", not
        // the whole log, so the cursor is read for the log line only.
        let page: ListPage<AuditEntry> =
            self.list_first_page(&path, "reading the audit log").await?;
        if page.page.next_cursor.is_some() {
            tracing::debug!(
                shown = page.items.len(),
                total = page.page.total,
                "the audit log continues past this page; raise --limit or narrow the filters"
            );
        }
        Ok(page.items)
    }

    // ---- nodes --------------------------------------------------------

    /// Every runtime node this control plane knows about, or the fact that
    /// this credential may not see the fleet.
    ///
    /// The fleet list is a platform diagnostic: each row carries a node's
    /// management address, its key status and its key fingerprint, and
    /// `docs/adr/016-api-cli-web-console.md` keeps fleet scope with the
    /// platform operator. An application credential is therefore *meant* to
    /// be refused here, and `deis status` says so in its node section
    /// rather than failing a command whose subject is an application.
    pub async fn list_nodes(&self) -> Result<FleetView> {
        let reply = self
            .dispatch(self.platform_request(Method::GET, "/runtime"))
            .await?;
        if reply.status == StatusCode::FORBIDDEN {
            return Ok(FleetView::NotPermitted);
        }
        if !reply.status.is_success() {
            bail!(self.http_error(&reply, "listing the runtime nodes"));
        }
        let response: NodesResponse = serde_json::from_slice(&reply.body)
            .with_context(|| "listing the runtime nodes".to_string())?;
        Ok(FleetView::Nodes(response.nodes))
    }

    /// What one node reports it is running.
    ///
    /// Answers `None` rather than failing when that one node cannot be
    /// reached: a fleet report is more useful with a hole in it than not at
    /// all, and the hole is printed.
    pub async fn node_status(&self, node: &str) -> Result<Option<Vec<NodeAppStatus>>> {
        let Ok(reply) = self
            .dispatch(
                self.platform_request(Method::GET, &format!("/runtime/{node}/status?apps=true")),
            )
            .await
        else {
            return Ok(None);
        };
        if !reply.status.is_success() {
            return Ok(None);
        }
        match serde_json::from_slice::<NodeStatusResponse>(&reply.body) {
            Ok(status) => Ok(Some(status.apps)),
            Err(_) => Ok(None),
        }
    }
}

/// A control plane on a loopback port, for the tests below.
///
/// Both test modules here drive the real HTTP client against a real server
/// rather than a stub, so this lives once instead of in each of them.
#[cfg(test)]
mod test_server {
    use super::Control;
    use axum::Router;

    /// Serve `router` on a free loopback port and return a `Control` pointed
    /// at it. Sending on the returned channel stops the server.
    pub async fn control_for(router: Router) -> (Control, tokio::sync::oneshot::Sender<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (shutdown, stopped) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            axum::serve(listener, router)
                .with_graceful_shutdown(async move {
                    let _ = stopped.await;
                })
                .await
                .unwrap();
        });
        (
            Control::new(&format!("http://{address}"), None).unwrap(),
            shutdown,
        )
    }
}

#[cfg(test)]
mod deployment_plan_tests {
    use super::test_server::control_for;
    use super::*;
    use axum::{Json, Router, http::header::LOCATION, response::IntoResponse, routing::get};

    const PLAN_ID: &str = "018f2518-7e38-7f80-8000-000000000001";
    const GENERATION: &str = "018f2518-7e38-7f80-8000-000000000002";
    const PLAN_PATH: &str = "/api/v1/apps/demo/environments/production/deployment-plans/018f2518-7e38-7f80-8000-000000000001";

    fn version() -> serde_json::Value {
        serde_json::json!({
            "id": "version-1",
            "canonical_id": "018f2518-7e38-7f80-8000-000000000009",
            "app_id": "demo",
            "number": 1,
            "bundle_sha": "sha256",
            "created_at": 1
        })
    }

    fn delivery(phase: &str) -> serde_json::Value {
        serde_json::json!({
            "reason": "deployment",
            "phase": phase,
            "app_id": "demo",
            "environment": "production",
            "version_id": "version-1",
            "delivered": ["runtime-1"],
            "failed": []
        })
    }

    fn plan(state: &str) -> serde_json::Value {
        serde_json::json!({
            "id": PLAN_ID,
            "routing_generation": GENERATION,
            "state": state
        })
    }

    fn deployment() -> serde_json::Value {
        serde_json::json!({
            "id": PLAN_ID,
            "canonical_id": GENERATION,
            "app_id": "demo",
            "env_id": "environment-1",
            "version_id": "version-1",
            "created_at": 2,
            "snapshot_id": "snapshot-1"
        })
    }

    async fn accepted_deploy() -> impl IntoResponse {
        (
            StatusCode::ACCEPTED,
            [(LOCATION, PLAN_PATH)],
            Json(serde_json::json!({
                "environment": "production",
                "version": version(),
                "delivery": delivery("prepare"),
                "plan": plan("prepared")
            })),
        )
    }

    async fn accepted_without_location() -> impl IntoResponse {
        (
            StatusCode::ACCEPTED,
            Json(serde_json::json!({
                "environment": "production",
                "version": version(),
                "delivery": delivery("prepare"),
                "plan": plan("prepared")
            })),
        )
    }

    async fn active_plan() -> impl IntoResponse {
        Json(serde_json::json!({"plan": plan("active")}))
    }

    async fn current_deployment() -> impl IntoResponse {
        Json(serde_json::json!({
            "deployment": deployment(),
            "environment": "production",
            "version": version(),
            "plan": plan("active")
        }))
    }

    #[tokio::test]
    async fn accepted_deploy_waits_for_the_exact_plan_and_pointer() {
        let router = Router::new()
            .route(
                "/api/v1/apps/demo/environments/production/deployments",
                axum::routing::post(accepted_deploy),
            )
            .route(PLAN_PATH, get(active_plan))
            .route(
                "/api/v1/apps/demo/environments/production/deployment",
                get(current_deployment),
            );
        let (control, shutdown) = control_for(router).await;
        let response = control
            .deploy(
                "demo",
                "production",
                "version-1",
                None,
                &std::collections::BTreeMap::new(),
            )
            .await
            .unwrap();
        assert_eq!(response.deployment.id, PLAN_ID);
        assert_eq!(response.plan.unwrap().state, "active");
        assert_eq!(response.delivery.unwrap().phase, "activate");
        let _ = shutdown.send(());
    }

    #[tokio::test]
    async fn accepted_deploy_without_location_is_not_reported_as_success() {
        let router = Router::new().route(
            "/api/v1/apps/demo/environments/production/deployments",
            axum::routing::post(accepted_without_location),
        );
        let (control, shutdown) = control_for(router).await;
        let error = control
            .deploy(
                "demo",
                "production",
                "version-1",
                None,
                &std::collections::BTreeMap::new(),
            )
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("has no Location"), "{error}");
        let _ = shutdown.send(());
    }
}

#[cfg(test)]
mod list_page_tests {
    use super::test_server::control_for;
    use super::*;
    use axum::{
        Json, Router,
        extract::Query,
        response::{IntoResponse, Response},
        routing::get,
    };
    use std::collections::HashMap;
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    /// Deliberately full of characters a URL gives a meaning to, so a token
    /// that survives this round trip survives any the control plane mints.
    const CURSOR: &str = "sort=created-at-desc,id-desc-v1|after=2 versions/one+two==";

    fn version(id: &str, number: i64) -> serde_json::Value {
        serde_json::json!({
            "id": id,
            "canonical_id": "018f2518-7e38-7f80-8000-000000000009",
            "app_id": "demo",
            "number": number,
            "bundle_sha": "sha256",
            "created_at": number
        })
    }

    fn page(items: Vec<serde_json::Value>, next: Option<&str>) -> serde_json::Value {
        let count = items.len();
        serde_json::json!({
            "schemaVersion": 1,
            "items": items,
            "page": {
                "limit": 100,
                "count": count,
                "total": 3,
                "nextCursor": next
            }
        })
    }

    /// Two pages: the first hands out `CURSOR`, the second ends the list.
    async fn versions_page(Query(query): Query<HashMap<String, String>>) -> Response {
        match query.get("cursor").map(String::as_str) {
            None => Json(page(
                vec![version("version-3", 3), version("version-2", 2)],
                Some(CURSOR),
            ))
            .into_response(),
            Some(CURSOR) => Json(page(vec![version("version-1", 1)], None)).into_response(),
            Some(other) => (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({
                    "error": format!("cursor arrived mangled: {other}")
                })),
            )
                .into_response(),
        }
    }

    /// Contradicts itself: nothing on the page, and another page promised.
    async fn endless_empty_page() -> Response {
        Json(serde_json::json!({
            "schemaVersion": 1,
            "items": [],
            "page": {"limit": 100, "count": 0, "total": 7, "nextCursor": "again"}
        }))
        .into_response()
    }

    #[test]
    fn the_unified_envelope_is_read_as_typed_items() {
        let body = serde_json::json!({
            "schemaVersion": 1,
            "items": [version("version-2", 2), version("version-1", 1)],
            "page": {
                "limit": 2,
                "count": 2,
                "total": 5,
                "nextCursor": "opaque-token"
            }
        });
        let read: ListPage<Version> = serde_json::from_value(body).unwrap();
        assert_eq!(read.schema_version, LIST_SCHEMA_VERSION);
        assert_eq!(read.items.len(), read.page.count);
        assert_eq!(read.page.limit, 2);
        assert_eq!(read.page.total, 5);
        assert_eq!(read.page.next_cursor.as_deref(), Some("opaque-token"));
        assert_eq!(read.items[0].id, "version-2");
        assert_eq!(read.items[1].number, 1);
    }

    #[test]
    fn a_last_page_reads_as_no_cursor() {
        let read: ListPage<Environment> = serde_json::from_value(serde_json::json!({
            "schemaVersion": 1,
            "items": [{"id": "env-1", "canonical_id": "018f2518-7e38-7f80-8000-000000000009", "app_id": "demo", "name": "production", "created_at": 1}],
            "page": {"limit": 100, "count": 1, "total": 1, "nextCursor": null}
        }))
        .unwrap();
        assert!(read.page.next_cursor.is_none());
        assert_eq!(read.items[0].name, "production");
    }

    #[test]
    fn a_cursor_is_appended_and_percent_encoded() {
        let url = append_cursor("/api/v1/apps/demo/versions", CURSOR);
        assert!(
            url.starts_with("/api/v1/apps/demo/versions?cursor="),
            "{url}"
        );
        assert!(!url.contains(' '), "{url}");
        assert!(url.contains("%2C"), "{url}");
        assert!(url.contains("%3D"), "{url}");

        let with_query = append_cursor("/api/v1/apps/demo/versions?limit=1000", CURSOR);
        assert!(with_query.contains("?limit=1000&cursor="), "{with_query}");
    }

    #[tokio::test]
    async fn every_page_is_followed_until_the_cursor_runs_out() {
        let router = Router::new().route("/api/v1/apps/demo/versions", get(versions_page));
        let (control, shutdown) = control_for(router).await;
        let versions = control.list_versions("demo").await.unwrap();
        let ids: Vec<&str> = versions.iter().map(|v| v.id.as_str()).collect();
        assert_eq!(ids, ["version-3", "version-2", "version-1"]);
        let _ = shutdown.send(());
    }

    /// Two pages of secret names: metadata only, and the second page ends
    /// the list.
    async fn secrets_page(Query(query): Query<HashMap<String, String>>) -> Response {
        let secret =
            |name: &str| serde_json::json!({"name": name, "revision": "rev-1", "updated_at": 1});
        match query.get("cursor").map(String::as_str) {
            None => Json(page(
                vec![secret("API_KEY"), secret("DB_URL")],
                Some(CURSOR),
            ))
            .into_response(),
            Some(CURSOR) => Json(page(vec![secret("TOKEN")], None)).into_response(),
            Some(other) => (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({
                    "error": format!("cursor arrived mangled: {other}")
                })),
            )
                .into_response(),
        }
    }

    #[tokio::test]
    async fn secret_names_are_read_through_the_unified_envelope() {
        let router = Router::new().route(
            "/api/v1/apps/demo/environments/production/secrets",
            get(secrets_page),
        );
        let (control, shutdown) = control_for(router).await;
        let secrets = control.list_secrets("demo", "production").await.unwrap();
        let names: Vec<&str> = secrets.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["API_KEY", "DB_URL", "TOKEN"]);
        assert_eq!(secrets[0].revision, "rev-1");
        let _ = shutdown.send(());
    }

    /// Accounts arrive through the same envelope and are followed to the
    /// end like every other list.
    async fn accounts_page(Query(query): Query<HashMap<String, String>>) -> Response {
        let account = |name: &str| {
            serde_json::json!({
                "id": format!("id-{name}"),
                "name": name,
                "role": "viewer",
                "created_at": 1
            })
        };
        match query.get("cursor").map(String::as_str) {
            None => {
                Json(page(vec![account("alice"), account("bob")], Some(CURSOR))).into_response()
            }
            Some(CURSOR) => Json(page(vec![account("carol")], None)).into_response(),
            Some(other) => (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({
                    "error": format!("cursor arrived mangled: {other}")
                })),
            )
                .into_response(),
        }
    }

    #[tokio::test]
    async fn accounts_are_read_through_the_unified_envelope() {
        let router = Router::new().route("/api/v1/accounts", get(accounts_page));
        let (control, shutdown) = control_for(router).await;
        let accounts = control.list_accounts().await.unwrap();
        let names: Vec<&str> = accounts.iter().map(|a| a.name.as_str()).collect();
        assert_eq!(names, ["alice", "bob", "carol"]);
        let _ = shutdown.send(());
    }

    /// The token route answers the listing under `scope=all` and the
    /// unscoped count as the total of a one-row `scope=unscoped` page.
    async fn tokens_page(Query(query): Query<HashMap<String, String>>) -> Response {
        let token = |name: &str, unscoped: bool| {
            serde_json::json!({
                "id": format!("tok-{name}"),
                "account": "ops",
                "name": name,
                "origin": "minted",
                "scopes": if unscoped { "unscoped" } else { "deploy:demo/preview" },
                "unscoped": unscoped,
                "created_at": 1
            })
        };
        if query.get("scope").map(String::as_str) == Some("unscoped") {
            assert_eq!(query.get("limit").map(String::as_str), Some("1"));
            assert_eq!(
                query.get("include_revoked").map(String::as_str),
                Some("false")
            );
            return Json(serde_json::json!({
                "schemaVersion": 1,
                "items": [token("old-root", true)],
                "page": {"limit": 1, "count": 1, "total": 2, "nextCursor": "more"}
            }))
            .into_response();
        }
        assert_eq!(
            query.get("include_revoked").map(String::as_str),
            Some("true")
        );
        match query.get("cursor").map(String::as_str) {
            None => Json(page(
                vec![token("preview", false), token("old-root", true)],
                Some(CURSOR),
            ))
            .into_response(),
            Some(CURSOR) => Json(page(vec![token("laptop", true)], None)).into_response(),
            Some(other) => (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({
                    "error": format!("cursor arrived mangled: {other}")
                })),
            )
                .into_response(),
        }
    }

    #[tokio::test]
    async fn tokens_are_listed_to_the_end_and_the_unscoped_count_is_the_filtered_total() {
        let router = Router::new().route("/api/v1/tokens", get(tokens_page));
        let (control, shutdown) = control_for(router).await;
        let listing = control.list_tokens(None, true).await.unwrap();
        let names: Vec<&str> = listing.tokens.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, ["preview", "old-root", "laptop"]);
        assert_eq!(listing.unscoped, 2);
        let _ = shutdown.send(());
    }

    /// `deis audit` reads one page of the latest lines; the filters and
    /// the bounded page size travel as query parameters.
    async fn audit_page(Query(query): Query<HashMap<String, String>>) -> Response {
        assert_eq!(query.get("app").map(String::as_str), Some("demo"));
        assert_eq!(query.get("limit").map(String::as_str), Some("1000"));
        assert!(!query.contains_key("before"));
        let entry = |id: i64, action: &str| {
            serde_json::json!({
                "id": id,
                "at": id,
                "actor_kind": "account",
                "actor_account": "alice",
                "action": action,
                "app_id": "demo",
                "method": "POST",
                "path": "/api/v1/apps",
                "outcome": "accepted",
                "status": 200
            })
        };
        Json(serde_json::json!({
            "schemaVersion": 1,
            "items": [entry(9, "deploy"), entry(8, "rollback")],
            "page": {"limit": 1000, "count": 2, "total": 40, "nextCursor": CURSOR}
        }))
        .into_response()
    }

    #[tokio::test]
    async fn the_audit_log_is_read_as_one_bounded_page() {
        let router = Router::new().route("/api/v1/audit", get(audit_page));
        let (control, shutdown) = control_for(router).await;
        let entries = control
            .read_audit(&AuditFilter {
                app: Some("demo".to_string()),
                limit: Some(5_000),
                ..Default::default()
            })
            .await
            .unwrap();
        let actions: Vec<&str> = entries.iter().map(|e| e.action.as_str()).collect();
        assert_eq!(actions, ["deploy", "rollback"]);
        let _ = shutdown.send(());
    }

    /// One page of the newest deployments, with a cursor this CLI
    /// deliberately does not follow. The route also proves the `limit=` a
    /// caller asked for reached it.
    async fn deployments_page(Query(query): Query<HashMap<String, String>>) -> Response {
        assert_eq!(query.get("limit").map(String::as_str), Some("1000"));
        assert!(!query.contains_key("cursor"));
        let deployment = |id: &str, at: i64, note: Option<&str>| {
            serde_json::json!({
                "id": id,
                "canonical_id": "018f2518-7e38-7f80-8000-000000000001",
                "app_id": "demo",
                "env_id": "env-1",
                "version_id": format!("version-{id}"),
                "created_at": at,
                "note": note,
                "snapshot_id": "snapshot-1"
            })
        };
        Json(serde_json::json!({
            "schemaVersion": 1,
            "items": [
                deployment("dep-9", 900, Some("hotfix")),
                deployment("dep-8", 800, None)
            ],
            "page": {"limit": 1000, "count": 2, "total": 137, "nextCursor": CURSOR}
        }))
        .into_response()
    }

    #[tokio::test]
    async fn a_deployment_history_is_one_bounded_page_and_says_how_deep_it_goes() {
        let router = Router::new().route(
            "/api/v1/apps/demo/environments/production/deployments",
            get(deployments_page),
        );
        let (control, shutdown) = control_for(router).await;
        // Asked for more than any page serves: clamped to the ceiling rather
        // than sent on and refused, which the route asserts on arrival.
        let history = control
            .list_deployments("demo", "production", 5_000)
            .await
            .unwrap();
        let ids: Vec<&str> = history.deployments.iter().map(|d| d.id.as_str()).collect();
        assert_eq!(ids, ["dep-9", "dep-8"]);
        assert_eq!(history.deployments[0].version_id, "version-dep-9");
        assert_eq!(history.deployments[0].note.as_deref(), Some("hotfix"));
        assert!(history.deployments[1].note.is_none());
        // The rest of the history is counted, not read.
        assert_eq!(history.total, 137);
        let _ = shutdown.send(());
    }

    /// Two pages of one environment's routes, in the order the edge applies
    /// them; the CLI must not re-sort them.
    async fn routes_page(Query(query): Query<HashMap<String, String>>) -> Response {
        let route = |id: &str, host: Option<&str>, prefix: &str, priority: i64| {
            serde_json::json!({
                "id": id,
                "app_id": "demo",
                "env_id": "env-1",
                "host": host,
                "path_prefix": prefix,
                "priority": priority
            })
        };
        match query.get("cursor").map(String::as_str) {
            None => Json(page(
                vec![
                    route("route-1", Some("shop.example.com"), "/checkout", 100),
                    route("route-2", Some("shop.example.com"), "/", 50),
                ],
                Some(CURSOR),
            ))
            .into_response(),
            Some(CURSOR) => Json(page(vec![route("route-3", None, "/", 0)], None)).into_response(),
            Some(other) => (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({
                    "error": format!("cursor arrived mangled: {other}")
                })),
            )
                .into_response(),
        }
    }

    #[tokio::test]
    async fn routes_are_read_through_the_unified_envelope_in_the_servers_order() {
        let router = Router::new().route(
            "/api/v1/apps/demo/environments/production/routes",
            get(routes_page),
        );
        let (control, shutdown) = control_for(router).await;
        let routes = control.list_routes("demo", "production").await.unwrap();
        let ids: Vec<&str> = routes.iter().map(|r| r.id.as_str()).collect();
        assert_eq!(ids, ["route-1", "route-2", "route-3"]);
        assert_eq!(routes[0].priority, 100);
        assert_eq!(routes[0].path_prefix, "/checkout");
        assert_eq!(routes[0].host.as_deref(), Some("shop.example.com"));
        // A route that names no host is the widest claim there is, and it
        // has to survive the round trip as absent rather than as "".
        assert!(routes[2].host.is_none());
        let _ = shutdown.send(());
    }

    /// Two pages of the hostname registry.
    async fn domains_page(Query(query): Query<HashMap<String, String>>) -> Response {
        let grant = |pattern: &str, environment: &str, note: Option<&str>| {
            serde_json::json!({
                "pattern": pattern,
                "app_id": "demo",
                "environment": environment,
                "created_at": 1,
                "note": note
            })
        };
        match query.get("cursor").map(String::as_str) {
            None => Json(page(
                vec![
                    grant("*.example.com", "*", Some("the zone")),
                    grant("shop.example.com", "production", None),
                ],
                Some(CURSOR),
            ))
            .into_response(),
            Some(CURSOR) => {
                Json(page(vec![grant("(no-host)", "preview", None)], None)).into_response()
            }
            Some(other) => (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({
                    "error": format!("cursor arrived mangled: {other}")
                })),
            )
                .into_response(),
        }
    }

    #[tokio::test]
    async fn domain_grants_are_read_through_the_unified_envelope() {
        let router = Router::new().route("/api/v1/domains", get(domains_page));
        let (control, shutdown) = control_for(router).await;
        let grants = control.list_domain_grants().await.unwrap();
        let patterns: Vec<&str> = grants.iter().map(|g| g.pattern.as_str()).collect();
        assert_eq!(patterns, ["*.example.com", "shop.example.com", "(no-host)"]);
        assert_eq!(grants[0].environment, EVERY_ENVIRONMENT);
        assert_eq!(grants[0].note.as_deref(), Some("the zone"));
        assert_eq!(grants[1].environment, "production");
        assert!(grants[1].note.is_none());
        let _ = shutdown.send(());
    }

    /// [`EVERY_ENVIRONMENT`] is this crate's copy of one character of the
    /// control plane's wire format, and the two crates cannot import each
    /// other's constant. So the sibling's source is read: if the control
    /// plane ever spells "every environment" differently, this fails rather
    /// than `deis domains list` quietly printing a grant as covering one
    /// environment named `*`.
    #[test]
    fn the_every_environment_marker_is_the_control_planes_own() {
        let sibling_path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../deis-control/src/catalog/domains.rs"
        );
        let sibling = std::fs::read_to_string(sibling_path)
            .expect("the control plane's domain rules are in this workspace");
        let declaration = format!("EVERY_ENVIRONMENT: &str = \"{EVERY_ENVIRONMENT}\";");
        assert!(
            sibling.contains(&declaration),
            "deis-control no longer declares {declaration}; the CLI's copy has drifted from the \
             control plane's registry format"
        );
    }

    #[tokio::test]
    async fn an_empty_page_that_still_offers_another_is_refused() {
        let router = Router::new().route("/api/v1/apps/demo/environments", get(endless_empty_page));
        let (control, shutdown) = control_for(router).await;
        let error = control
            .list_environments("demo")
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("empty page"), "{error}");
        let _ = shutdown.send(());
    }

    /// A control plane that repeats its own cursor - a bug in its keyset
    /// paging, or a cache in front of it replaying one answer - hands back a
    /// page that is full, so the empty-page refusal above never fires. The
    /// refusal has to happen on the second sighting of the token: waiting
    /// for the page ceiling would mean ten thousand requests and a list of
    /// duplicates the caller would print as though they were distinct rows.
    #[tokio::test]
    async fn a_cursor_the_control_plane_repeats_is_refused_on_its_second_use() {
        let requests = Arc::new(AtomicUsize::new(0));
        let counted = requests.clone();
        let router = Router::new().route(
            "/api/v1/apps/demo/environments",
            get(move || {
                let counted = counted.clone();
                async move {
                    counted.fetch_add(1, Ordering::SeqCst);
                    Json(serde_json::json!({
                        "schemaVersion": 1,
                        "items": [{
                            "id": "env-1",
                            "canonical_id": "018f2518-7e38-7f80-8000-000000000009",
                            "app_id": "demo",
                            "name": "production",
                            "created_at": 1
                        }],
                        "page": {"limit": 100, "count": 1, "total": 7, "nextCursor": "stuck"}
                    }))
                }
            }),
        );
        let (control, shutdown) = control_for(router).await;
        let error = control
            .list_environments("demo")
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("same cursor it was given"), "{error}");
        assert_eq!(
            requests.load(Ordering::SeqCst),
            2,
            "the repeat must be caught on the request that came back with it, not at the page \
             ceiling"
        );
        let _ = shutdown.send(());
    }
}

/// The two prefixes, the deprecation window, and the two error shapes.
#[cfg(test)]
mod surface_tests {
    use super::test_server::control_for;
    use super::*;
    use axum::{
        Json, Router,
        extract::Path,
        http::header::CONTENT_TYPE,
        response::{IntoResponse, Response},
        routing::{get, post},
    };
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    const REQUEST: &str = "018f2518-7e38-7f80-8000-0000000000aa";
    /// What `deis-control`'s route fallback answers, byte for byte, for a
    /// path it has no route for - on every control plane there has been.
    const ROUTE_FALLBACK: &str = r#"{"error":"Not Found","details":"No such endpoint. The API surface is the OpenAPI contract, contracts/openapi/v1.json."}"#;

    fn page(items: Vec<serde_json::Value>) -> serde_json::Value {
        let count = items.len();
        serde_json::json!({
            "schemaVersion": 1,
            "items": items,
            "page": {"limit": 100, "count": count, "total": count, "nextCursor": null}
        })
    }

    fn version(id: &str) -> serde_json::Value {
        serde_json::json!({
            "id": id,
            "canonical_id": "018f2518-7e38-7f80-8000-000000000009",
            "app_id": "demo",
            "number": 1,
            "bundle_sha": "sha256",
            "created_at": 1
        })
    }

    /// A server whose only routes live under the flat `/api` prefix, which
    /// deis-control does not serve; anything else gets the route fallback.
    /// `probes` counts how often the fallback was reached.
    fn flat_control_plane(probes: Arc<AtomicUsize>) -> Router {
        Router::new()
            .route(
                "/api/apps/demo/versions",
                get(|| async { Json(page(vec![version("version-1")])) }),
            )
            .route(
                "/api/accounts",
                get(|| async {
                    Json(page(vec![serde_json::json!({
                        "id": "id-alice", "name": "alice", "role": "viewer", "created_at": 1
                    })]))
                }),
            )
            .route(
                "/api/apps",
                post(|| async {
                    (
                        StatusCode::CONFLICT,
                        Json(serde_json::json!({"error": "app demo already exists"})),
                    )
                }),
            )
            .fallback(move || {
                let probes = probes.clone();
                async move {
                    probes.fetch_add(1, Ordering::SeqCst);
                    (
                        StatusCode::NOT_FOUND,
                        [(CONTENT_TYPE, "application/json")],
                        ROUTE_FALLBACK,
                    )
                        .into_response()
                }
            })
    }

    #[test]
    fn a_refusal_is_read_in_either_shape() {
        match Refusal::parse(br#"{"error":"plain"}"#) {
            Some(Refusal::Plain(message)) => assert_eq!(message, "plain"),
            other => panic!("{other:?}"),
        }
        match Refusal::parse(
            br#"{"error":{"code":"not-found","message":"no app demo","requestId":"r","violations":[],"retryable":false}}"#,
        ) {
            Some(Refusal::Structured(error)) => {
                assert_eq!(error.code, "not-found");
                assert_eq!(error.request_id.as_deref(), Some("r"));
                assert!(error.violations.is_empty());
            }
            other => panic!("{other:?}"),
        }
        // An envelope missing a required member is not one; nor is a body
        // whose `error` is neither shape, nor a body that is not JSON.
        assert!(Refusal::parse(br#"{"error":{"message":"no code"}}"#).is_none());
        assert!(Refusal::parse(br#"{"error":7}"#).is_none());
        assert!(Refusal::parse(b"<html>gateway</html>").is_none());
        assert!(matches!(
            Refusal::parse(ROUTE_FALLBACK.as_bytes()),
            Some(Refusal::Plain(_))
        ));
    }

    #[test]
    fn an_audit_line_reads_its_request_id_by_the_wire_name() {
        let line: AuditEntry = serde_json::from_value(serde_json::json!({
            "id": 1, "at": 1, "actor_kind": "account", "action": "deploy",
            "method": "POST", "path": "/api/v1/apps", "outcome": "accepted", "status": 200,
            "requestId": REQUEST
        }))
        .unwrap();
        assert_eq!(line.request_id.as_deref(), Some(REQUEST));
        let older: AuditEntry = serde_json::from_value(serde_json::json!({
            "id": 1, "at": 1, "actor_kind": "account", "action": "deploy",
            "method": "POST", "path": "/api/apps", "outcome": "accepted", "status": 200,
            "requestId": null
        }))
        .unwrap();
        assert!(older.request_id.is_none());
    }

    #[tokio::test]
    async fn a_missing_route_is_an_error_and_never_retried_under_another_prefix() {
        let probes = Arc::new(AtomicUsize::new(0));
        let (control, shutdown) = control_for(flat_control_plane(probes.clone())).await;
        let error = control.list_versions("demo").await.unwrap_err().to_string();
        assert!(error.contains("404"), "{error}");
        assert_eq!(probes.load(Ordering::SeqCst), 1);
        let _ = shutdown.send(());
    }

    /// A 404 that carries the envelope is the control plane saying "no such
    /// record" on a surface it does know.
    #[tokio::test]
    async fn a_versioned_404_about_a_record_is_an_answer_not_a_missing_surface() {
        let router = Router::new()
            .route(
                "/api/v1/apps/:id",
                get(|Path(id): Path<String>| async move {
                    (
                        StatusCode::NOT_FOUND,
                        [(REQUEST_ID_HEADER, REQUEST)],
                        Json(serde_json::json!({
                            "error": {
                                "code": "not-found",
                                "message": format!("no app {id}"),
                                "requestId": REQUEST,
                                "violations": [],
                                "retryable": false
                            }
                        })),
                    )
                }),
            )
            .route(
                "/api/apps/:id",
                get(|| async { Json(serde_json::json!({"id": "demo", "created_at": 1})) }),
            );
        let (control, shutdown) = control_for(router).await;
        assert!(control.get_app("demo").await.unwrap().is_none());
        let _ = shutdown.send(());
    }

    #[tokio::test]
    async fn a_structured_refusal_prints_its_code_its_fields_and_its_request_id() {
        async fn refuse() -> Response {
            (
                StatusCode::BAD_REQUEST,
                [(REQUEST_ID_HEADER, REQUEST)],
                Json(serde_json::json!({
                    "error": {
                        "code": "invalid-argument",
                        "message": "the request names no application",
                        "requestId": REQUEST,
                        "target": {"resource": "app", "field": "id"},
                        "violations": [
                            {"field": "id", "code": "required", "message": "must be given"}
                        ],
                        "retryable": false
                    }
                })),
            )
                .into_response()
        }
        let router = Router::new().route("/api/v1/apps", post(refuse));
        let (control, shutdown) = control_for(router).await;
        let error = control.create_app("demo").await.unwrap_err().to_string();
        assert!(
            error.contains("400 Bad Request: invalid-argument: the request names no application"),
            "{error}"
        );
        assert!(
            error.contains("\n  - id: must be given (required)"),
            "{error}"
        );
        assert!(error.contains(&format!("[request {REQUEST}]")), "{error}");
        assert!(!error.contains("tried again"), "{error}");
        let _ = shutdown.send(());
    }

    /// A refusal whose envelope names no request id still prints the one
    /// from the header, and a retryable one says so.
    #[tokio::test]
    async fn a_retryable_refusal_says_when_to_try_again() {
        async fn refuse() -> Response {
            (
                StatusCode::SERVICE_UNAVAILABLE,
                [(REQUEST_ID_HEADER, REQUEST)],
                Json(serde_json::json!({
                    "error": {
                        "code": "backend-unavailable",
                        "message": "the runtime fleet is not answering",
                        "retryable": true,
                        "retryAfterSeconds": 30
                    }
                })),
            )
                .into_response()
        }
        let router = Router::new().route("/api/v1/accounts", post(refuse));
        let (control, shutdown) = control_for(router).await;
        let error = control
            .create_account("alice", "viewer")
            .await
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("backend-unavailable: the runtime fleet"),
            "{error}"
        );
        assert!(error.contains(&format!("[request {REQUEST}]")), "{error}");
        assert!(error.contains("try again in 30 seconds"), "{error}");
        let _ = shutdown.send(());
    }

    /// Fleet scope is the platform operator's, so an application
    /// credential is refused this list. `deis status` reports that as a
    /// section of its answer, so the client must hand it back as one
    /// rather than as a failed command.
    #[tokio::test]
    async fn a_credential_without_fleet_scope_is_told_so_and_not_failed() {
        let router = Router::new().route(
            "/api/platform/runtime",
            get(|| async {
                (
                    StatusCode::FORBIDDEN,
                    Json(serde_json::json!({
                        "error": {
                            "code": "insufficient-scope",
                            "message": "this call needs read:*/*",
                            "retryable": false
                        }
                    })),
                )
            }),
        );
        let (control, shutdown) = control_for(router).await;
        assert!(matches!(
            control.list_nodes().await.unwrap(),
            FleetView::NotPermitted
        ));
        let _ = shutdown.send(());
    }

    #[tokio::test]
    async fn the_fleet_is_read_from_the_platform_surface() {
        let router = Router::new()
            .route(
                "/api/platform/runtime",
                get(|| async {
                    Json(serde_json::json!({
                        "nodes": [{"id": "n1", "addr": "10.0.0.1:50051", "healthy": true}]
                    }))
                }),
            )
            .route(
                "/api/platform/runtime/n1/status",
                get(|| async {
                    Json(serde_json::json!({
                        "apps": [{"app_id": "demo", "environment": "production", "state": "running"}]
                    }))
                }),
            );
        let (control, shutdown) = control_for(router).await;
        let FleetView::Nodes(nodes) = control.list_nodes().await.unwrap() else {
            panic!("the fleet is readable with this credential");
        };
        assert_eq!(nodes[0].id, "n1");
        let apps = control.node_status("n1").await.unwrap().unwrap();
        assert_eq!(apps[0].app_id, "demo");
        let _ = shutdown.send(());
    }
}

/// The provider-deployment surface: what one workflow reads as, and the two
/// round trips - a fenced cancel and a refusal - that a stub control plane is
/// the only honest way to exercise.
#[cfg(test)]
mod provider_deployment_tests {
    use super::test_server::control_for;
    use super::*;
    use axum::http::HeaderMap as AxumHeaderMap;
    use axum::{
        Json, Router,
        http::header::{ETAG, IF_MATCH, LOCATION},
        response::{IntoResponse, Response},
        routing::{get, post},
    };

    const DEPLOYMENT_ID: &str = "018f2518-7e38-7f80-8000-0000000000a1";
    const ROLLBACK_ID: &str = "018f2518-7e38-7f80-8000-0000000000a2";
    const TARGET_ID: &str = "018f2518-7e38-7f80-8000-0000000000b1";
    const GRANT_ID: &str = "018f2518-7e38-7f80-8000-0000000000b2";
    const CHILD_ID: &str = "018f2518-7e38-7f80-8000-0000000000c1";
    const WORKFLOW_PATH: &str = "/api/v1/apps/demo/environments/production/provider-deployments/018f2518-7e38-7f80-8000-0000000000a1";
    const ROLLBACK_PATH: &str = "/api/v1/apps/demo/environments/production/provider-deployments/018f2518-7e38-7f80-8000-0000000000a2";

    fn id(text: &str) -> CanonicalId {
        text.parse().expect("test id is a canonical identity")
    }

    /// One operation exactly as `contracts/v1/operation.schema.json` puts it
    /// on the wire, fields this CLI does not read included: they are the
    /// point of the fixture, since dropping one of them must not be what
    /// makes a document readable.
    fn operation(operation_id: &str, kind: &str, state: &str, revision: u64) -> serde_json::Value {
        serde_json::json!({
            "$schema": "https://github.com/deislet/deislet/contracts/v1/operation.schema.json",
            "schemaVersion": 1,
            "operationId": operation_id,
            "kind": kind,
            "scope": {
                "organizationId": "018f2518-7e38-7f80-8000-0000000000f1",
                "applicationId": "018f2518-7e38-7f80-8000-0000000000f2",
                "environmentId": "018f2518-7e38-7f80-8000-0000000000f3"
            },
            "target": {
                "kind": "provider-deployment",
                "id": TARGET_ID,
                "expectedRevision": 3
            },
            "state": state,
            "phase": "apply",
            "revision": revision,
            "idempotencyKeyDigest": {"algorithm": "sha256", "value": "00"},
            "actorId": "018f2518-7e38-7f80-8000-0000000000f4",
            "requestId": "018f2518-7e38-7f80-8000-0000000000f5",
            "correlationId": "018f2518-7e38-7f80-8000-0000000000f6",
            "progress": 4200,
            "cancellable": true,
            "retryable": false,
            "childOperationIds": [CHILD_ID],
            "providerOperations": [],
            "error": null,
            "createdAt": "2026-09-06T10:00:00Z",
            "startedAt": "2026-09-06T10:00:01Z",
            "updatedAt": "2026-09-06T10:04:00Z",
            "finishedAt": null
        })
    }

    fn workflow(state: &str, revision: u64) -> serde_json::Value {
        serde_json::json!({
            "operation": operation(DEPLOYMENT_ID, "deploy", state, revision),
            "kind": "deploy",
            "rolledBackFromOperationId": null,
            "rollbackOperationIds": [ROLLBACK_ID],
            "cancelRequestedAt": null,
            "providerTargetId": TARGET_ID,
            "targetRevision": 3,
            "providerTargetGrantId": GRANT_ID,
            "targetGrantRevision": 2,
            "desiredStateDigest": "9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08",
            "artifact": {"artifactId": "018f2518-7e38-7f80-8000-0000000000d1"},
            "plan": {"schemaVersion": 1},
            "nextStepIndex": 2,
            "dispatchNotBefore": null,
            "dispatchFailureCount": 1,
            "dispatchFailureChildOperationId": CHILD_ID,
            "lastDispatchErrorCode": "driver-unreachable",
            "lastDispatchErrorMessage": "the driver did not answer",
            "steps": [
                {"sequence": 0, "operation": "validate", "childOperationId": CHILD_ID},
                {"sequence": 1, "operation": "plan", "childOperationId": CHILD_ID},
                {"sequence": 2, "operation": "apply", "childOperationId": CHILD_ID}
            ]
        })
    }

    #[test]
    fn a_workflow_reads_as_every_field_this_cli_prints() {
        let read: ProviderDeployment = serde_json::from_value(workflow("running", 7)).unwrap();
        assert_eq!(read.kind, "deploy");
        assert_eq!(read.operation.operation_id, id(DEPLOYMENT_ID));
        assert_eq!(read.operation.state, "running");
        assert_eq!(read.operation.phase, "apply");
        assert_eq!(read.operation.revision, 7);
        assert_eq!(read.operation.progress, Some(4200));
        assert!(read.operation.cancellable);
        assert_eq!(read.operation.finished_at, None);
        assert_eq!(read.cancel_requested_at, None);
        assert_eq!(read.rolled_back_from_operation_id, None);
        assert_eq!(read.rollback_operation_ids, vec![id(ROLLBACK_ID)]);
        assert_eq!(read.provider_target_id, id(TARGET_ID));
        assert_eq!(read.provider_target_grant_id, Some(id(GRANT_ID)));
        assert_eq!(read.target_grant_revision, Some(2));
        assert_eq!(read.next_step_index, 2);
        assert_eq!(read.dispatch_failure_count, 1);
        assert_eq!(
            read.last_dispatch_error_code.as_deref(),
            Some("driver-unreachable")
        );
        assert_eq!(read.steps.len(), 3);
        assert_eq!(read.steps[2].operation, "apply");
        assert_eq!(read.steps[2].child_operation_id, id(CHILD_ID));
        assert_eq!(read.operation.settled(), Some(false));
    }

    /// A rollback workflow is the same document with the two link fields the
    /// other way round, and that is the only thing that says which deployment
    /// an operator is actually looking at.
    #[test]
    fn a_rollback_workflow_names_the_deployment_it_restores() {
        let mut document = workflow("succeeded", 9);
        document["kind"] = serde_json::json!("rollback");
        document["operation"]["kind"] = serde_json::json!("rollback");
        document["operation"]["operationId"] = serde_json::json!(ROLLBACK_ID);
        document["rolledBackFromOperationId"] = serde_json::json!(DEPLOYMENT_ID);
        document["rollbackOperationIds"] = serde_json::json!([]);
        document["cancelRequestedAt"] = serde_json::json!("2026-09-06T10:03:00Z");

        let read: ProviderDeployment = serde_json::from_value(document).unwrap();
        assert_eq!(read.kind, "rollback");
        assert_eq!(read.rolled_back_from_operation_id, Some(id(DEPLOYMENT_ID)));
        assert!(read.rollback_operation_ids.is_empty());
        assert!(read.cancel_requested_at.is_some());
        assert_eq!(read.operation.settled(), Some(true));
    }

    /// A control plane newer than this build stays readable: an added field
    /// is ignored, and a state this build has never heard of is carried
    /// through as text so it can be *printed*, with `settled` admitting it
    /// does not know rather than guessing.
    #[test]
    fn a_newer_control_plane_stays_readable_and_its_unknown_state_is_not_guessed() {
        let mut document = workflow("reconciling", 11);
        document["quarantinedAt"] = serde_json::json!("2026-09-06T10:05:00Z");
        document["operation"]["escalation"] = serde_json::json!({"tier": 2});

        let read: ProviderDeployment = serde_json::from_value(document).unwrap();
        assert_eq!(read.operation.state, "reconciling");
        assert_eq!(read.operation.settled(), None);
    }

    /// The workflow, its ETag, and a cancel that must arrive carrying that
    /// exact ETag as `If-Match` and an `Idempotency-Key` of its own.
    async fn serve_workflow() -> Response {
        let mut headers = AxumHeaderMap::new();
        headers.insert(ETAG, "\"7\"".parse().unwrap());
        (headers, Json(workflow("running", 7))).into_response()
    }

    async fn serve_cancel(headers: AxumHeaderMap) -> Response {
        let if_match = headers.get(IF_MATCH).and_then(|value| value.to_str().ok());
        if if_match != Some("\"7\"") {
            return (
                StatusCode::PRECONDITION_FAILED,
                Json(serde_json::json!({
                    "error": {
                        "code": "precondition-failed",
                        "message": format!("If-Match arrived as {if_match:?}")
                    }
                })),
            )
                .into_response();
        }
        let key = headers
            .get(IDEMPOTENCY_KEY)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default();
        if key.parse::<CanonicalId>().is_err() {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({
                    "error": {
                        "code": "idempotency-key-invalid",
                        "message": format!("Idempotency-Key arrived as {key:?}")
                    }
                })),
            )
                .into_response();
        }
        let mut answer = AxumHeaderMap::new();
        answer.insert(ETAG, "\"8\"".parse().unwrap());
        answer.insert(LOCATION, WORKFLOW_PATH.parse().unwrap());
        (
            StatusCode::ACCEPTED,
            answer,
            Json(operation(DEPLOYMENT_ID, "deploy", "cancelling", 8)),
        )
            .into_response()
    }

    #[tokio::test]
    async fn a_cancel_fences_on_the_etag_of_the_read_that_preceded_it() {
        let router = Router::new()
            .route(WORKFLOW_PATH, get(serve_workflow))
            .route(&format!("{WORKFLOW_PATH}/cancel"), post(serve_cancel));
        let (control, shutdown) = control_for(router).await;

        let cancelled = control
            .cancel_provider_deployment("demo", "production", id(DEPLOYMENT_ID))
            .await
            .unwrap();

        assert_eq!(cancelled.observed.operation.state, "running");
        assert_eq!(cancelled.observed.operation.revision, 7);
        assert_eq!(cancelled.operation.state, "cancelling");
        assert_eq!(cancelled.operation.revision, 8);
        assert_eq!(cancelled.operation.operation_id, id(DEPLOYMENT_ID));
        let _ = shutdown.send(());
    }

    /// A workflow that has already finished is refused, and the refusal is
    /// what reaches the operator: the control plane's own code, sentence and
    /// request id, not a reworded summary of them.
    #[tokio::test]
    async fn a_refused_cancel_is_reported_as_the_control_planes_own_error() {
        async fn refuse() -> Response {
            (
                StatusCode::CONFLICT,
                Json(serde_json::json!({
                    "error": {
                        "code": "operation-not-cancellable",
                        "message": "provider deployment already succeeded",
                        "requestId": "018f2518-7e38-7f80-8000-0000000000e1"
                    }
                })),
            )
                .into_response()
        }

        let router = Router::new()
            .route(WORKFLOW_PATH, get(serve_workflow))
            .route(&format!("{WORKFLOW_PATH}/cancel"), post(refuse));
        let (control, shutdown) = control_for(router).await;

        let error = control
            .cancel_provider_deployment("demo", "production", id(DEPLOYMENT_ID))
            .await
            .unwrap_err()
            .to_string();

        assert!(error.contains("409"), "{error}");
        assert!(error.contains("operation-not-cancellable"), "{error}");
        assert!(error.contains("already succeeded"), "{error}");
        assert!(
            error.contains("018f2518-7e38-7f80-8000-0000000000e1"),
            "{error}"
        );
        let _ = shutdown.send(());
    }

    /// A rollback answers a *different* operation, and its `Location` is
    /// checked against the path this CLI would have built for that new id
    /// rather than followed wherever it points.
    #[tokio::test]
    async fn a_rollback_answers_the_new_operation_at_its_own_scoped_location() {
        async fn accept(headers: AxumHeaderMap) -> Response {
            assert!(headers.contains_key(IDEMPOTENCY_KEY));
            let mut answer = AxumHeaderMap::new();
            answer.insert(LOCATION, ROLLBACK_PATH.parse().unwrap());
            (
                StatusCode::ACCEPTED,
                answer,
                Json(operation(ROLLBACK_ID, "rollback", "queued", 1)),
            )
                .into_response()
        }

        let router = Router::new().route(&format!("{WORKFLOW_PATH}/rollback"), post(accept));
        let (control, shutdown) = control_for(router).await;

        let accepted = control
            .rollback_provider_deployment("demo", "production", id(DEPLOYMENT_ID))
            .await
            .unwrap();

        assert_eq!(accepted.operation_id, id(ROLLBACK_ID));
        assert_eq!(accepted.kind, "rollback");
        assert_eq!(accepted.state, "queued");
        let _ = shutdown.send(());
    }

    /// The check that keeps this CLI from becoming a credential-bearing
    /// redirect client: a `Location` that is not the new operation's own
    /// scoped path is refused rather than polled.
    #[tokio::test]
    async fn a_rollback_pointing_somewhere_else_is_refused() {
        async fn misdirect() -> Response {
            let mut answer = AxumHeaderMap::new();
            answer.insert(LOCATION, "https://elsewhere.invalid/steal".parse().unwrap());
            (
                StatusCode::ACCEPTED,
                answer,
                Json(operation(ROLLBACK_ID, "rollback", "queued", 1)),
            )
                .into_response()
        }

        let router = Router::new().route(&format!("{WORKFLOW_PATH}/rollback"), post(misdirect));
        let (control, shutdown) = control_for(router).await;

        let error = control
            .rollback_provider_deployment("demo", "production", id(DEPLOYMENT_ID))
            .await
            .unwrap_err()
            .to_string();

        assert!(error.contains("expected the scoped path"), "{error}");
        let _ = shutdown.send(());
    }

    /// One page, newest first, read the same way `deis deployments` reads
    /// one - and the total behind it kept apart from the rows shown.
    #[tokio::test]
    async fn one_page_of_workflows_is_read_through_the_unified_envelope() {
        async fn page() -> Response {
            Json(serde_json::json!({
                "schemaVersion": 1,
                "items": [workflow("running", 7)],
                "page": {"limit": 20, "count": 1, "total": 4, "nextCursor": "more"}
            }))
            .into_response()
        }

        let router = Router::new().route(
            "/api/v1/apps/demo/environments/production/provider-deployments",
            get(page),
        );
        let (control, shutdown) = control_for(router).await;

        let history = control
            .list_provider_deployments("demo", "production", 20)
            .await
            .unwrap();

        assert_eq!(history.deployments.len(), 1);
        assert_eq!(history.total, 4);
        assert_eq!(history.deployments[0].operation.state, "running");
        let _ = shutdown.send(());
    }

    /// A control plane that answers a read with a different operation than
    /// the one asked for is refused, rather than being allowed to become the
    /// workflow a cancel is then fenced against.
    #[tokio::test]
    async fn a_read_that_answers_about_another_operation_is_refused() {
        async fn wrong_operation() -> Response {
            Json(workflow("running", 7)).into_response()
        }

        let router = Router::new().route(ROLLBACK_PATH, get(wrong_operation));
        let (control, shutdown) = control_for(router).await;

        let error = control
            .get_provider_deployment("demo", "production", id(ROLLBACK_ID))
            .await
            .unwrap_err()
            .to_string();

        assert!(
            error.contains("is not the one that was asked for"),
            "{error}"
        );
        let _ = shutdown.send(());
    }

    /// The revision the fence is taken against comes from the `ETag` of the
    /// read, and falls back to the body only when the control plane sent no
    /// `ETag` at all.
    #[test]
    fn the_fence_prefers_the_etag_over_the_revision_in_the_body() {
        let read: ProviderDeployment = serde_json::from_value(workflow("running", 7)).unwrap();

        let mut headers = HeaderMap::new();
        headers.insert(reqwest::header::ETAG, "\"9\"".parse().unwrap());
        assert_eq!(read_etag(&headers, &read.operation), "\"9\"");

        assert_eq!(read_etag(&HeaderMap::new(), &read.operation), "\"7\"");
    }

    /// Every key is a fresh canonical identity, and no two calls share one:
    /// a key reused across two different operator actions would make the
    /// second one silently answer the first.
    #[test]
    fn each_idempotency_key_is_a_fresh_canonical_identity() {
        let first = new_idempotency_key();
        let second = new_idempotency_key();
        assert!(first.parse::<CanonicalId>().is_ok(), "{first}");
        assert_ne!(first, second);
        assert!((8..=255).contains(&first.len()), "{first}");
    }
}

/// The control plane's own captured answers, read the way the commands read
/// them.
///
/// `contracts/openapi/v1-examples.json` holds one real response body per
/// schema, captured from `deis-control`'s handlers and regenerated with
/// `UPDATE_OPENAPI=1`. `docs/adr/016-api-cli-web-console.md` records that this
/// CLI's client stays hand-written and is held to those captures instead of
/// being generated from the document - an arrangement that only means
/// anything if something on this side actually reads them, and until these
/// tests nothing did. The control plane's own gate compares the file against
/// its handlers and is satisfied when the two agree, so renaming a member on
/// the server and regenerating the file left every gate green while the CLI
/// that still asked for the old name failed at runtime, in front of an
/// operator, with a parse error naming a field nobody had touched. This is
/// the missing half of the arrangement: the captures are read here as the
/// commands read a live answer, so that rename fails in CI instead.
///
/// Two surfaces this CLI speaks have no capture to be held to yet, and both
/// are gaps in `deis-control`'s capture rather than here: the provider
/// deployment workflow (`ProviderDeployment`) and one node's status
/// (`NodeStatusResponse`). Neither has an example in the file, so the
/// classification below has nothing to name and the mirrors stay unguarded.
#[cfg(test)]
mod captured_example_tests {
    use super::*;
    use serde_json::Value;
    use std::collections::BTreeMap;

    /// Where the captured answers live, relative to this crate. Read from
    /// the working tree rather than embedded, because the point is to fail
    /// when the committed file moves under a CLI that was not changed with
    /// it.
    const EXAMPLES: &str = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../contracts/openapi/v1-examples.json"
    );

    /// What this CLI does with one captured response schema.
    enum Read {
        /// Deserialised into the named mirror type by the function beside
        /// it, which does exactly the `serde` work the command that reads
        /// the route does.
        Into(&'static str, fn(Value) -> serde_json::Result<()>),
        /// Nothing in this CLI reads the route. The string says who does,
        /// so that "the CLI ignores this" stays a decision on the record
        /// rather than an omission.
        NotRead(&'static str),
    }

    /// Every schema the capture carries, and this CLI's reading of it.
    ///
    /// Exhaustive on purpose. A route added to the control plane adds a
    /// capture, and an unclassified capture fails
    /// [`every_captured_answer_is_classified`] - which is the moment to
    /// decide whether the CLI ought to be reading it, rather than a year
    /// later when somebody notices the command was never written.
    fn readings() -> Vec<(&'static str, Read)> {
        vec![
            (
                "Account",
                Read::Into("Account", |v| {
                    serde_json::from_value::<Account>(v).map(drop)
                }),
            ),
            (
                "ApiToken",
                Read::Into("ApiToken", |v| {
                    serde_json::from_value::<ApiToken>(v).map(drop)
                }),
            ),
            (
                "App",
                Read::Into("App", |v| serde_json::from_value::<App>(v).map(drop)),
            ),
            (
                "ConfigKeyOutcome",
                Read::NotRead("node configuration is a Web console surface"),
            ),
            (
                "ConfigStatsResponse",
                Read::NotRead("node configuration is a Web console surface"),
            ),
            (
                "ConfigValue",
                Read::NotRead("node configuration is a Web console surface"),
            ),
            (
                "ConfigsListing",
                Read::NotRead("node configuration is a Web console surface"),
            ),
            (
                "ConfigsUpdated",
                Read::NotRead("node configuration is a Web console surface"),
            ),
            (
                "ControlListPage_Account",
                Read::Into("ListPage<Account>", |v| {
                    serde_json::from_value::<ListPage<Account>>(v).map(drop)
                }),
            ),
            (
                "ControlListPage_ApiToken",
                Read::Into("ListPage<ApiToken>", |v| {
                    serde_json::from_value::<ListPage<ApiToken>>(v).map(drop)
                }),
            ),
            (
                "ControlListPage_App",
                Read::NotRead(
                    "this CLI names the application it acts on; it has no catalog listing",
                ),
            ),
            (
                "ControlListPage_AuditEntry",
                Read::Into("ListPage<AuditEntry>", |v| {
                    serde_json::from_value::<ListPage<AuditEntry>>(v).map(drop)
                }),
            ),
            (
                "ControlListPage_CommandSummary",
                Read::NotRead("node commands are a Web console surface"),
            ),
            (
                "ControlListPage_ConfigHistoryItem",
                Read::NotRead("node configuration is a Web console surface"),
            ),
            (
                "ControlListPage_Deployment",
                Read::Into("ListPage<Deployment>", |v| {
                    serde_json::from_value::<ListPage<Deployment>>(v).map(drop)
                }),
            ),
            (
                "ControlListPage_DomainGrant",
                Read::Into("ListPage<DomainGrant>", |v| {
                    serde_json::from_value::<ListPage<DomainGrant>>(v).map(drop)
                }),
            ),
            (
                "ControlListPage_Environment",
                Read::Into("ListPage<Environment>", |v| {
                    serde_json::from_value::<ListPage<Environment>>(v).map(drop)
                }),
            ),
            (
                "ControlListPage_EventItem",
                Read::NotRead("platform events are a Web console surface"),
            ),
            (
                "ControlListPage_ProviderCredentialBinding",
                Read::NotRead("provider credentials are administered through the Web console"),
            ),
            (
                "ControlListPage_ProviderInstallation",
                Read::NotRead("provider installations are administered through the Web console"),
            ),
            (
                "ControlListPage_ProviderTarget",
                Read::NotRead("provider targets are administered through the Web console"),
            ),
            (
                "ControlListPage_ProviderTargetGrant",
                Read::NotRead("target grant history is administered through the Web console"),
            ),
            (
                "ControlListPage_RetiredAppName",
                Read::NotRead("retired application names are administered through the Web console"),
            ),
            (
                "ControlListPage_Route",
                Read::Into("ListPage<Route>", |v| {
                    serde_json::from_value::<ListPage<Route>>(v).map(drop)
                }),
            ),
            (
                "ControlListPage_SecretMeta",
                Read::Into("ListPage<SecretMeta>", |v| {
                    serde_json::from_value::<ListPage<SecretMeta>>(v).map(drop)
                }),
            ),
            (
                "ControlListPage_VersionView",
                Read::Into("ListPage<Version>", |v| {
                    serde_json::from_value::<ListPage<Version>>(v).map(drop)
                }),
            ),
            (
                "DeleteAppResponse",
                Read::NotRead("this CLI does not delete applications"),
            ),
            (
                "DeploymentPlanResponse",
                Read::Into("DeploymentPlanStatusResponse", |v| {
                    serde_json::from_value::<DeploymentPlanStatusResponse>(v).map(drop)
                }),
            ),
            // One body, two mirrors: a synchronous answer is read as a
            // `DeploymentResponse` with a pointer, an accepted one as the
            // looser mutation shape. Both are held to it, because a member
            // renamed on the server breaks whichever of the two an operator
            // happens to hit.
            (
                "DeploymentResponse",
                Read::Into("DeploymentResponse and DeploymentMutationResponse", |v| {
                    serde_json::from_value::<DeploymentResponse>(v.clone())?;
                    serde_json::from_value::<DeploymentMutationResponse>(v).map(drop)
                }),
            ),
            (
                "Environment",
                Read::Into("Environment", |v| {
                    serde_json::from_value::<Environment>(v).map(drop)
                }),
            ),
            (
                "ErrorBody",
                Read::Into("ErrorEnvelope", |v| {
                    serde_json::from_value::<ErrorEnvelope>(v).map(drop)
                }),
            ),
            (
                "EventStatsResponse",
                Read::NotRead("platform events are a Web console surface"),
            ),
            (
                "FleetResponse",
                Read::Into("NodesResponse", |v| {
                    serde_json::from_value::<NodesResponse>(v).map(drop)
                }),
            ),
            (
                "GlobalRoutesResponse",
                Read::NotRead("`deis routes` lists one environment's routes, not the platform's"),
            ),
            (
                "GrantResponse",
                Read::NotRead("`deis domains` lists grants; it does not create them"),
            ),
            (
                "GrantWithdrawn",
                Read::NotRead("`deis domains` lists grants; it does not withdraw them"),
            ),
            (
                "MetricsSummaryResponse",
                Read::NotRead("metrics are a Web console surface"),
            ),
            (
                "NodeConfigOutcome",
                Read::NotRead("node configuration is a Web console surface"),
            ),
            (
                "NodeConfigs",
                Read::NotRead("node configuration is a Web console surface"),
            ),
            (
                "ProductInstallation",
                Read::NotRead("the installation record is a Web console surface"),
            ),
            (
                "ReleaseRetiredAppResponse",
                Read::NotRead("retired application names are administered through the Web console"),
            ),
            (
                "RouteDeleted",
                Read::NotRead("`deis routes` lists routes; it does not delete them"),
            ),
            (
                "RouteResponse",
                Read::NotRead("`deis routes` lists routes; it does not create them"),
            ),
            (
                "SecretChangeResponse",
                Read::Into("SecretChangeResponse", |v| {
                    serde_json::from_value::<SecretChangeResponse>(v).map(drop)
                }),
            ),
            (
                "SecretDeletedResponse",
                Read::Into("SecretDeletedResponse", |v| {
                    serde_json::from_value::<SecretDeletedResponse>(v).map(drop)
                }),
            ),
            (
                "SessionsResponse",
                Read::NotRead("browser sessions are a Web console surface"),
            ),
            (
                "UploadResponse",
                Read::Into("UploadResponse", |v| {
                    serde_json::from_value::<UploadResponse>(v).map(drop)
                }),
            ),
            (
                "VersionView",
                Read::Into("Version", |v| {
                    serde_json::from_value::<Version>(v).map(drop)
                }),
            ),
        ]
    }

    /// One captured answer: the body, and which operation produced it.
    struct Captured {
        body: Value,
        operation_id: String,
    }

    fn captured() -> BTreeMap<String, Captured> {
        let text = std::fs::read_to_string(EXAMPLES).expect(
            "contracts/openapi/v1-examples.json is committed beside the document this CLI speaks",
        );
        let document: Value = serde_json::from_str(&text).expect("the capture is JSON");
        document["examples"]
            .as_object()
            .expect("the capture holds an object of schemas")
            .iter()
            .map(|(schema, example)| {
                (
                    schema.clone(),
                    Captured {
                        body: example["body"].clone(),
                        operation_id: example["operationId"]
                            .as_str()
                            .unwrap_or("<unnamed>")
                            .to_string(),
                    },
                )
            })
            .collect()
    }

    #[test]
    fn every_captured_answer_is_classified() {
        let readings = readings();
        for (schema, reading) in &readings {
            if let Read::NotRead(reason) = reading {
                assert!(
                    !reason.is_empty(),
                    "{schema} is classified as unread without saying who reads it instead"
                );
            }
        }
        let mut names: Vec<&str> = readings.iter().map(|(schema, _)| *schema).collect();
        let unique: std::collections::BTreeSet<&str> = names.iter().copied().collect();
        assert_eq!(names.len(), unique.len(), "a schema is classified twice");
        names.sort_unstable();
        let captured: Vec<String> = captured().into_keys().collect();
        let captured: Vec<&str> = captured.iter().map(String::as_str).collect();
        assert_eq!(
            names, captured,
            "the captured answers and this CLI's reading of them have drifted. A schema the \
             control plane captured and this CLI does not classify is a route nobody decided \
             about; a schema classified here and no longer captured is a route that moved or \
             went away under a CLI that still asks for it."
        );
    }

    #[test]
    fn every_captured_answer_this_cli_reads_deserialises_into_its_mirror() {
        let captured = captured();
        let mut checked = 0;
        for (schema, reading) in readings() {
            let Read::Into(mirror, parse) = reading else {
                continue;
            };
            let example = captured
                .get(schema)
                .unwrap_or_else(|| panic!("{schema} is classified as read but was not captured"));
            if let Err(error) = parse(example.body.clone()) {
                panic!(
                    "the control plane's own answer to {} no longer reads as {mirror}: {error}. \
                     The capture in contracts/openapi/v1-examples.json is what the handlers \
                     produce, so this is the CLI asking for a member the server has stopped \
                     sending - fix the mirror in this file, do not regenerate around it.",
                    example.operation_id
                );
            }
            checked += 1;
        }
        assert!(
            checked >= 20,
            "only {checked} captured answers were read; the classification has lost entries"
        );
    }

    /// Walk one dotted path into a captured body. A numeric segment indexes
    /// an array, so a list page's first item can be named.
    fn member<'a>(body: &'a Value, path: &str) -> Option<&'a Value> {
        path.split('.')
            .try_fold(body, |cursor, segment| match cursor {
                Value::Array(items) => items.get(segment.parse::<usize>().ok()?),
                _ => cursor.get(segment),
            })
    }

    /// The members this CLI reads leniently, and which the capture carries
    /// today.
    ///
    /// Deserialisation alone cannot protect these. Every one of them is read
    /// with a serde default so that a control plane too old to send it is
    /// still usable, which means a control plane that *renames* it is
    /// indistinguishable from one that never had it: the mirror quietly
    /// takes the default and the command prints a blank column or "unknown"
    /// for a value the server was in fact sending all along. Naming them
    /// here turns that silence into a failure. A member the control plane
    /// legitimately omits when it has nothing to say - a version with no
    /// canonical parent, a plan that never errored - is simply absent from
    /// this list.
    const LENIENT_MEMBERS: &[(&str, &str)] = &[
        ("Account", "disabled_at"),
        ("ApiToken", "unscoped"),
        ("ApiToken", "last_used_at"),
        ("ApiToken", "revoked_at"),
        ("ControlListPage_Account", "items.0.disabled_at"),
        ("ControlListPage_ApiToken", "items.0.unscoped"),
        ("ControlListPage_ApiToken", "items.0.last_used_at"),
        ("ControlListPage_ApiToken", "items.0.revoked_at"),
        ("ControlListPage_AuditEntry", "items.0.actor_account"),
        ("ControlListPage_AuditEntry", "items.0.actor_credential"),
        ("ControlListPage_AuditEntry", "items.0.app_id"),
        ("ControlListPage_AuditEntry", "items.0.environment"),
        ("ControlListPage_AuditEntry", "items.0.target"),
        ("ControlListPage_AuditEntry", "items.0.peer"),
        ("ControlListPage_AuditEntry", "items.0.detail"),
        ("ControlListPage_AuditEntry", "items.0.requestId"),
        ("ControlListPage_Deployment", "items.0.note"),
        ("ControlListPage_DomainGrant", "items.0.note"),
        ("ControlListPage_Route", "items.0.host"),
        ("ControlListPage_SecretMeta", "items.0.revision"),
        ("ControlListPage_VersionView", "items.0.manifest"),
        ("ControlListPage_VersionView", "items.0.compatibility"),
        ("DeploymentPlanResponse", "plan.last_error"),
        ("DeploymentResponse", "deployment.note"),
        ("DeploymentResponse", "delivery"),
        ("DeploymentResponse", "delivery.phase"),
        ("DeploymentResponse", "delivery.delivered"),
        ("DeploymentResponse", "delivery.failed"),
        ("DeploymentResponse", "delivery.skipped"),
        ("DeploymentResponse", "plan"),
        ("DeploymentResponse", "plan.last_error"),
        ("ErrorBody", "error.requestId"),
        ("ErrorBody", "error.violations"),
        ("ErrorBody", "error.retryable"),
        ("SecretChangeResponse", "secret.revision"),
        ("SecretChangeResponse", "delivery.phase"),
        ("SecretChangeResponse", "delivery.skipped"),
        ("SecretDeletedResponse", "delivery.phase"),
        ("VersionView", "manifest"),
        ("VersionView", "compatibility"),
    ];

    #[test]
    fn the_leniently_read_members_are_still_in_the_captured_answers() {
        let captured = captured();
        for (schema, path) in LENIENT_MEMBERS {
            let example = captured
                .get(*schema)
                .unwrap_or_else(|| panic!("{schema} names a schema that is no longer captured"));
            assert!(
                member(&example.body, path).is_some(),
                "the control plane's answer to {} no longer carries {path}, and this CLI reads \
                 that member with a default - so it would print nothing there rather than fail. \
                 Either the member was renamed, in which case the mirror in this file has to \
                 follow, or the control plane genuinely stopped sending it, in which case drop \
                 this line and say so where the mirror declares the member.",
                example.operation_id
            );
        }
    }

    #[test]
    fn every_captured_page_still_carries_the_cursor_this_cli_follows() {
        let captured = captured();
        let mut pages = 0;
        for (schema, reading) in readings() {
            if !matches!(reading, Read::Into(..)) || !schema.starts_with("ControlListPage_") {
                continue;
            }
            let example = &captured[schema];
            for path in ["schemaVersion", "page.limit", "page.count", "page.total"] {
                assert!(
                    member(&example.body, path).is_some(),
                    "{} answered a page without {path}",
                    example.operation_id
                );
            }
            // Read with a default, and the one member that decides whether
            // `list_all` asks for another page: a rename here would stop
            // every paged listing at its first page and report the short
            // answer as the whole catalog.
            assert!(
                member(&example.body, "page.nextCursor").is_some(),
                "the page {} answered no longer names nextCursor; this CLI would read the first \
                 page and report it as the whole list",
                example.operation_id
            );
            pages += 1;
        }
        assert!(pages >= 8, "only {pages} list pages were checked");
    }
}
