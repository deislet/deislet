//! The control plane's HTTP API, as this CLI uses it.
//!
//! Every type here mirrors what `deis-control` already serves, and every
//! field the CLI prints is deserialised straight off what the server
//! returns, so an added field there does not break an older CLI.

pub mod queue;
pub mod storage;
pub mod telemetry;

use anyhow::{Context, Result, bail};
use deis_wire::CanonicalId;
use reqwest::{
    Method, StatusCode,
    header::{ACCEPT, HeaderMap},
};
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicBool, Ordering};

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
/// The flat prefix every route lived under before the two above existed.
///
/// A control plane from before that split still answers here and nowhere
/// else, and a newer one keeps it for a deprecation window. This CLI speaks
/// the versioned prefixes and drops back to this one only after a control
/// plane has shown it does not know them - see [`Control::dispatch`].
const API_LEGACY: &str = "/api";
/// The response header every versioned answer carries: the id the control
/// plane logged the request under, which is what to quote when asking an
/// operator what happened.
const REQUEST_ID_HEADER: &str = "x-request-id";

/// Spell a versioned path the way the legacy tree spells it, or `None` when
/// the path is not under a versioned prefix and there is nothing to fall
/// back to.
fn legacy_spelling(path: &str) -> Option<String> {
    [API_MAJOR, API_PLATFORM].into_iter().find_map(|prefix| {
        let rest = path.strip_prefix(prefix)?;
        (rest.is_empty() || rest.starts_with('/')).then(|| format!("{API_LEGACY}{rest}"))
    })
}

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
    /// Permanent product identity. `None` only when talking to a control
    /// plane from before the canonical-identity migration.
    #[serde(default)]
    pub canonical_id: Option<CanonicalId>,
    pub created_at: i64,
}

/// One immutable version.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Version {
    pub id: String,
    /// Permanent product identity; absent only on an older control plane.
    #[serde(default)]
    pub canonical_id: Option<CanonicalId>,
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

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Environment {
    pub id: String,
    /// Permanent product identity; absent only on an older control plane.
    #[serde(default)]
    pub canonical_id: Option<CanonicalId>,
    pub app_id: String,
    pub name: String,
    pub created_at: i64,
}

/// One pointer row: this environment ran this version from this moment.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Deployment {
    pub id: String,
    /// Permanent product identity; absent only on an older control plane.
    #[serde(default)]
    pub canonical_id: Option<CanonicalId>,
    pub app_id: String,
    pub env_id: String,
    pub version_id: String,
    pub created_at: i64,
    #[serde(default)]
    pub note: Option<String>,
    /// Immutable Edge Canon environment snapshot selected by this pointer.
    /// Absent for legacy schema-1 deployments and older control planes.
    #[serde(default)]
    pub snapshot_id: Option<String>,
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

#[cfg(test)]
mod canonical_identity_tests {
    use super::App;

    #[test]
    fn old_control_without_canonical_identity_remains_readable() {
        let app: App = serde_json::from_str(r#"{"id":"demo","created_at":1}"#).unwrap();
        assert!(app.canonical_id.is_none());
    }

    #[test]
    fn present_canonical_identity_is_validated() {
        let valid: App = serde_json::from_str(
            r#"{"id":"demo","canonical_id":"018f2518-7e38-7f80-8000-000000000001","created_at":1}"#,
        )
        .unwrap();
        assert_eq!(
            valid.canonical_id.unwrap().to_string(),
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
    /// Exact wire phase observed by this report. Older controls omitted it
    /// and therefore read as the legacy single-step apply path.
    #[serde(default = "legacy_delivery_phase")]
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

fn legacy_delivery_phase() -> String {
    "legacy_apply".to_string()
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
    /// Opaque immutable revision; absent only on an older control plane.
    #[serde(default)]
    pub revision: Option<String>,
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
    /// The flat `{"error": "<message>"}` of the legacy tree - and of the
    /// authorization gate, which answers that way on every surface.
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

    /// Whether a body is the versioned envelope. A 404 that carries one is
    /// an answer about a record from a control plane that knows the route;
    /// a 404 without one is a control plane that does not know the route.
    fn is_structured(body: &[u8]) -> bool {
        matches!(Self::parse(body), Some(Self::Structured(_)))
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
    /// Set once this control plane has answered a versioned path with a
    /// 404 that carried no envelope: it predates [`API_MAJOR`], and every
    /// later call goes straight to [`API_LEGACY`] rather than probing again.
    legacy: AtomicBool,
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
            legacy: AtomicBool::new(false),
        })
    }

    pub fn base_url(&self) -> &str {
        &self.base
    }

    /// Whether this control plane has shown it predates the versioned
    /// prefixes and is being driven through the legacy tree.
    fn speaks_legacy(&self) -> bool {
        self.legacy.load(Ordering::Relaxed)
    }

    /// The prefix a product route is sent under right now.
    fn product_prefix(&self) -> &'static str {
        if self.speaks_legacy() {
            API_LEGACY
        } else {
            API_MAJOR
        }
    }

    /// The prefix a platform route is sent under right now.
    fn platform_prefix(&self) -> &'static str {
        if self.speaks_legacy() {
            API_LEGACY
        } else {
            API_PLATFORM
        }
    }

    fn url(&self, prefix: &str, route: &str) -> String {
        format!("{}{prefix}{route}", self.base)
    }

    /// A request to a product route: `route` is spelled without any prefix
    /// (`/apps/demo`), and the one the control plane answers on is put in
    /// front of it here.
    fn request(&self, method: Method, route: &str) -> reqwest::RequestBuilder {
        self.build(method, self.product_prefix(), route)
    }

    /// A request to a platform diagnostics route, prefixed the same way.
    fn platform_request(&self, method: Method, route: &str) -> reqwest::RequestBuilder {
        self.build(method, self.platform_prefix(), route)
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

    /// Send one request and read the whole answer.
    ///
    /// The seam every call goes through, and the one place the deprecation
    /// window is handled. A control plane from before the versioned prefixes
    /// answers `/api/v1/...` with its route fallback: a 404 whose body is the
    /// flat legacy string, where every versioned route - including for its
    /// own 404 about a missing record - answers the envelope. That answer is
    /// taken once as "this control plane is older than this CLI", said at
    /// warn, and the same request is re-sent under the legacy prefix; every
    /// later request is built for the legacy tree directly. A 404 that does
    /// carry the envelope is returned as the answer it is.
    async fn dispatch(&self, builder: reqwest::RequestBuilder) -> Result<Reply> {
        let request = builder
            .build()
            .context("building a request to the control plane")?;
        // `None` only for a streaming body, which this CLI never sends.
        let retry = request.try_clone();
        let path = request.url().path().to_string();
        let response = self
            .http
            .execute(request)
            .await
            .map_err(|e| self.transport_error(e))?;
        let reply = Reply::read(response).await;
        if reply.status != StatusCode::NOT_FOUND || Refusal::is_structured(&reply.body) {
            return Ok(reply);
        }
        let (Some(mut retry), Some(legacy_path)) = (retry, legacy_spelling(&path)) else {
            return Ok(reply);
        };
        if !self.legacy.swap(true, Ordering::Relaxed) {
            tracing::warn!(
                control = %self.base,
                path,
                cli_version = env!("CARGO_PKG_VERSION"),
                "the control plane does not serve {API_MAJOR}: it is most likely a deis-control \
                 older than this CLI, from before the versioned API; driving it through \
                 {API_LEGACY} instead, which newer control planes keep only for a deprecation \
                 window - upgrade it before that window closes"
            );
        }
        tracing::debug!(
            from = path,
            to = legacy_path,
            "re-sending under the legacy prefix"
        );
        retry.url_mut().set_path(&legacy_path);
        let response = self
            .http
            .execute(retry)
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
            cursor = page.page.next_cursor;
            match cursor {
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
                Some(_) => {}
            }
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
    /// search the control plane's log for. The legacy string is printed as
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

    /// Upload a bundle as a new version.
    ///
    /// `sha256` is sent along so the control plane refuses a truncated upload
    /// instead of recording it as a version that no node can load.
    pub async fn upload_version(
        &self,
        app_id: &str,
        bundle: Vec<u8>,
        sha256: &str,
    ) -> Result<UploadResponse> {
        self.send(
            self.request(
                Method::POST,
                &format!("/apps/{app_id}/versions?sha256={sha256}"),
            )
            .header(reqwest::header::CONTENT_TYPE, "application/zip")
            .body(bundle),
            &format!("uploading a bundle for app {app_id}"),
        )
        .await
    }

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

        // Spelled under whichever prefix the mutation itself went out on,
        // which `dispatch` has settled by now: a control plane that knows
        // the versioned prefix points at it, and one that was fallen back
        // to points at the legacy tree.
        let route = format!(
            "/apps/{app_id}/environments/{environment}/deployment-plans/{}",
            plan.id
        );
        let path = format!("{}{route}", self.product_prefix());
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
    /// environment having to exist. The token is minted there rather than
    /// this CLI being handed the shared `auth_secret`, which is the whole
    /// point of a capability token: a leaked CLI credential still only ever
    /// proves the holder may deploy and inspect *through the control plane's
    /// own checks*, and never hands out the key every tenant's token is
    /// derived from.
    ///
    /// A control plane with no deis-store address or no `storage_auth_secret`
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

    /// Every runtime node this control plane knows about.
    pub async fn list_nodes(&self) -> Result<Vec<Node>> {
        let response: NodesResponse = self
            .send(
                self.platform_request(Method::GET, "/runtime"),
                "listing the runtime nodes",
            )
            .await?;
        Ok(response.nodes)
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
            "created_at": 2
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

    /// A control plane from before the versioned prefixes points at its
    /// plan under `/api/...`, and the CLI - having fallen back for the
    /// mutation itself - expects exactly that spelling and polls it there.
    async fn accepted_deploy_on_the_legacy_tree() -> impl IntoResponse {
        (
            StatusCode::ACCEPTED,
            [(LOCATION, LEGACY_PLAN_PATH)],
            Json(serde_json::json!({
                "environment": "production",
                "version": version(),
                "delivery": delivery("prepare"),
                "plan": plan("prepared")
            })),
        )
    }

    const LEGACY_PLAN_PATH: &str = "/api/apps/demo/environments/production/deployment-plans/018f2518-7e38-7f80-8000-000000000001";

    #[tokio::test]
    async fn an_accepted_plan_on_an_older_control_plane_is_polled_under_the_legacy_prefix() {
        let router = Router::new()
            .route(
                "/api/apps/demo/environments/production/deployments",
                axum::routing::post(accepted_deploy_on_the_legacy_tree),
            )
            .route(LEGACY_PLAN_PATH, get(active_plan))
            .route(
                "/api/apps/demo/environments/production/deployment",
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
        assert!(control.speaks_legacy());
        assert_eq!(response.deployment.id, PLAN_ID);
        assert_eq!(response.plan.unwrap().state, "active");
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

    /// Deliberately full of characters a URL gives a meaning to, so a token
    /// that survives this round trip survives any the control plane mints.
    const CURSOR: &str = "sort=created-at-desc,id-desc-v1|after=2 versions/one+two==";

    fn version(id: &str, number: i64) -> serde_json::Value {
        serde_json::json!({
            "id": id,
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
            "items": [{"id": "env-1", "app_id": "demo", "name": "production", "created_at": 1}],
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
        assert_eq!(secrets[0].revision.as_deref(), Some("rev-1"));
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
        assert!(history.deployments[0].canonical_id.is_some());
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
    const ROUTE_FALLBACK: &str = r#"{"error":"Not Found","details":"No such endpoint. The API surface is listed in the component's API_DOCUMENTATION.md."}"#;

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
            "app_id": "demo",
            "number": 1,
            "bundle_sha": "sha256",
            "created_at": 1
        })
    }

    /// A control plane from before the versioned prefixes: every route it
    /// has lives under `/api`, and anything else gets the route fallback.
    /// `probes` counts how often the fallback was reached.
    fn older_control_plane(probes: Arc<AtomicUsize>) -> Router {
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
    fn a_versioned_path_has_exactly_one_legacy_spelling() {
        assert_eq!(
            legacy_spelling("/api/v1/apps/demo?limit=1").as_deref(),
            Some("/api/apps/demo?limit=1")
        );
        assert_eq!(
            legacy_spelling("/api/platform/runtime").as_deref(),
            Some("/api/runtime")
        );
        // Already the legacy spelling, or a lookalike: nothing to fall back to.
        assert_eq!(legacy_spelling("/api/apps"), None);
        assert_eq!(legacy_spelling("/api/v10/apps"), None);
        assert_eq!(legacy_spelling("/api/platformer"), None);
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
        assert!(!Refusal::is_structured(ROUTE_FALLBACK.as_bytes()));
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
    async fn an_older_control_plane_is_probed_once_and_then_driven_through_the_legacy_tree() {
        let probes = Arc::new(AtomicUsize::new(0));
        let (control, shutdown) = control_for(older_control_plane(probes.clone())).await;
        assert!(!control.speaks_legacy());

        let versions = control.list_versions("demo").await.unwrap();
        assert_eq!(versions[0].id, "version-1");
        assert!(control.speaks_legacy());
        assert_eq!(probes.load(Ordering::SeqCst), 1);

        // The second call is built for the legacy tree outright: no probe.
        let accounts = control.list_accounts().await.unwrap();
        assert_eq!(accounts[0].name, "alice");
        assert_eq!(probes.load(Ordering::SeqCst), 1);

        // And its flat refusal is still read as the string it is, with no
        // request id invented for it.
        let error = control.create_app("demo").await.unwrap_err().to_string();
        assert!(error.contains("409"), "{error}");
        assert!(error.contains("app demo already exists"), "{error}");
        assert!(!error.contains("[request"), "{error}");
        assert_eq!(probes.load(Ordering::SeqCst), 1);
        let _ = shutdown.send(());
    }

    /// A 404 that carries the envelope is the control plane saying "no such
    /// record" on a surface it does know; the legacy tree is not consulted.
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
        assert!(!control.speaks_legacy());
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
        let nodes = control.list_nodes().await.unwrap();
        assert_eq!(nodes[0].id, "n1");
        let apps = control.node_status("n1").await.unwrap().unwrap();
        assert_eq!(apps[0].app_id, "demo");
        assert!(!control.speaks_legacy());
        let _ = shutdown.send(());
    }
}
