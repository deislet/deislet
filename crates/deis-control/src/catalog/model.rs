//! Row types of the catalog plus the identifier rules that keep
//! user-supplied names safe to put in a file path or a route table.
//!
//! Every timestamp in this module is Unix epoch seconds, matching the
//! `INTEGER` columns declared in `database::schema`.

use deis_wire::CanonicalId;
use serde::Serialize;
use utoipa::ToSchema;

use super::CatalogError;

/// Longest accepted app id, environment name or route host label.
pub const MAX_NAME_LEN: usize = 64;
/// Longest accepted secret name. Secrets end up as process environment
/// variable names in the runtime, which is why they get their own limit.
pub const MAX_SECRET_NAME_LEN: usize = 128;
/// Longest accepted route path prefix.
pub const MAX_PATH_PREFIX_LEN: usize = 512;

/// An application. During the identity migration, `id` remains the legacy
/// app name used by existing routes and tenants while `canonical_id` is the
/// permanent product identity. New code must not derive authority or storage
/// isolation from the display name.
///
/// The dumb services (deis-store, deis-queue) see a single opaque tenant id
/// built from the pair by `deis_wire::conventions::tenant_id`, so a preview
/// deployment cannot read or overwrite what production stored while those
/// services still understand exactly one tenant dimension.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct App {
    pub id: String,
    #[schema(value_type = String, format = Uuid)]
    pub canonical_id: CanonicalId,
    /// Monotonic catalog revision used to fence identity migration. It moves
    /// whenever an environment, version, deployment, route or secret changes.
    pub revision: i64,
    pub created_at: i64,
}

/// An immutable version: one uploaded bundle plus its parsed manifest.
///
/// Deliberately not `Serialize`: `bundle_path` is a control-plane local
/// filesystem path and must not leak into an API response. The REST layer
/// builds its own view.
#[derive(Debug, Clone)]
pub struct Version {
    pub id: String,
    pub canonical_id: CanonicalId,
    pub app_id: String,
    /// Per-app counter starting at 1. This is what a human calls "v3".
    pub number: i64,
    pub bundle_sha: String,
    /// Where the zip lives on the control plane. Internal only.
    pub bundle_path: String,
    /// Verbatim `manifest.json` text as it was found inside the bundle.
    pub manifest: String,
    /// Exact provider-neutral parent. `None` identifies an upload made
    /// through the legacy raw-bundle endpoint before lineage was recorded.
    pub canonical_artifact_id: Option<CanonicalId>,
    /// SHA-256 identity of the immutable provider derivation manifest.
    pub provider_derivation_sha256: Option<String>,
    pub created_at: i64,
}

/// One verified provider-neutral artifact held by the control plane.
/// `record` is the schema-conforming public control-plane record; the local
/// storage path is kept separate and never serialized by the REST layer.
#[derive(Debug, Clone)]
pub struct CanonicalArtifact {
    pub id: CanonicalId,
    pub app_id: String,
    pub artifact_sha256: String,
    pub content_root_sha256: String,
    pub standard_version: String,
    pub file_count: i64,
    pub storage_path: String,
    pub record: serde_json::Value,
    pub created_at: i64,
}

/// Immutable evidence that a provider payload was derived from a canonical
/// parent. Its identity is the SHA-256 of `manifest`'s canonical bytes.
#[derive(Debug, Clone)]
pub struct ProviderDerivation {
    pub manifest_sha256: String,
    pub artifact_id: CanonicalId,
    pub provider: String,
    pub standard_version: String,
    pub payload_root_sha256: String,
    pub manifest: String,
    pub created_at: i64,
}

/// An environment: `production` or any preview name. Deployment pointers,
/// routes and secrets all hang off this, never off the bare app.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct Environment {
    pub id: String,
    #[schema(value_type = String, format = Uuid)]
    pub canonical_id: CanonicalId,
    pub app_id: String,
    pub name: String,
    pub created_at: i64,
}

/// One pointer row. The newest row of an environment is its current
/// version; older rows are the deployment history.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct Deployment {
    pub id: String,
    #[schema(value_type = String, format = Uuid)]
    pub canonical_id: CanonicalId,
    pub app_id: String,
    pub env_id: String,
    pub version_id: String,
    pub created_at: i64,
    pub note: Option<String>,
    /// Immutable Edge Canon environment snapshot selected with this pointer.
    /// Absent only on deployments written by the legacy schema-1 path.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub snapshot_id: Option<String>,
}

/// Immutable deployment intent plus its durable coordinator state.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct DeploymentPlan {
    pub id: String,
    #[schema(value_type = String, format = Uuid)]
    pub routing_generation: CanonicalId,
    pub app_id: String,
    pub env_id: String,
    pub version_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub snapshot_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub previous_deployment_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub previous_generation: Option<String>,
    pub expected_secret_heads: std::collections::BTreeMap<String, Option<String>>,
    /// Fresh deploys compare every selected head; historical rollback
    /// deliberately reactivates retained revisions regardless of current heads.
    pub compare_secret_heads: bool,
    pub target_ids: Vec<String>,
    pub proxy_ids: Vec<String>,
    pub policy: serde_json::Value,
    pub plan_sha256: String,
    pub state: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    /// Short-lived owner of the next coordinator action. This is mutable
    /// execution state and is deliberately outside `plan_sha256`.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<String>, format = Uuid)]
    pub coordinator_id: Option<CanonicalId>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lease_expires_at: Option<i64>,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct DeploymentTargetObservation {
    pub plan_id: String,
    pub node_id: String,
    pub observed_state: String,
    pub version_id: Option<String>,
    pub routing_generation: Option<String>,
    pub message: Option<String>,
    pub observed_at: i64,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct DeploymentProxyObservation {
    pub plan_id: String,
    pub proxy_id: String,
    pub observed_state: String,
    pub routing_table_version: String,
    pub routing_generation: String,
    pub observed_at: i64,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct DeploymentQueueObservation {
    pub plan_id: String,
    pub observed_state: String,
    pub active_generation: String,
    pub active_deployment_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pending_generation: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pending_deployment_id: Option<String>,
    pub outstanding_leases: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    pub observed_at: i64,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct DeploymentEvent {
    pub sequence: i64,
    pub plan_id: String,
    pub from_state: Option<String>,
    pub to_state: String,
    pub detail: Option<String>,
    pub created_at: i64,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct DeploymentGenerationDrain {
    pub plan_id: String,
    pub generation: String,
    pub target_ids: Vec<String>,
    pub not_before: i64,
    pub state: String,
    pub last_error: Option<String>,
    pub updated_at: i64,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct DeploymentDrainObservation {
    pub plan_id: String,
    pub node_id: String,
    pub generation: String,
    pub observed_state: String,
    pub message: Option<String>,
    pub observed_at: i64,
}

/// A host plus path prefix bound to one environment. `host: None` matches
/// any domain.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct Route {
    pub id: String,
    pub app_id: String,
    pub env_id: String,
    pub host: Option<String>,
    pub path_prefix: String,
    pub priority: i64,
}

/// What a caller is allowed to learn about a secret: that it exists and
/// when it last changed. The value never leaves the control plane except
/// as part of a deployment payload to a node.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct SecretMeta {
    pub name: String,
    /// Opaque immutable revision identity. A later write creates another one.
    pub revision: String,
    pub updated_at: i64,
}

/// What one node should be running, before the bundle download URL is
/// filled in by the REST layer (which knows the public base address).
///
/// The two environment fields are deliberately spelled apart: everything
/// ending in `_id` is a primary key, and `environment` is the human name
/// (`production`, `pr-42`). That is the name the wire contract carries
/// (docs/implementation-plan.zh.md section 4) and the one the runtime
/// branches on, so putting an id in it would reach the application code
/// as a uuid without anything on the way noticing.
#[derive(Debug, Clone)]
pub struct Assignment {
    pub app_id: String,
    /// Primary key of the environment row. Control-plane internal.
    pub environment_id: String,
    /// Environment name, as sent to the node.
    pub environment: String,
    pub version_id: String,
    pub bundle_sha: String,
    pub snapshot_id: Option<String>,
    pub deployment_id: String,
    pub routing_generation: String,
    /// Durable coordinator state for this pointer. `None` identifies a
    /// deployment written before plans existed and is treated as active for
    /// rolling compatibility.
    pub deployment_state: Option<String>,
}

/// One environment's current version together with the manifest text that
/// version carried.
///
/// This is what the cron scheduler works from: which cron expressions an
/// application wants is a property of the version an environment currently
/// runs, so it moves with every deploy and rollback and disappears with
/// the last deployment.
#[derive(Debug, Clone)]
pub struct DeployedVersion {
    pub app_id: String,
    /// Environment name, `production` or a preview name.
    pub environment: String,
    pub version_id: String,
    /// Verbatim `manifest.json` text of that version.
    pub manifest: String,
}

/// Everything deleting an app removed. Returned so the caller can clean up
/// the bundle files on disk and log exactly how much history was destroyed.
#[derive(Debug, Clone, Default, Serialize, ToSchema)]
pub struct DeletedApp {
    pub canonical_artifacts: usize,
    pub provider_derivations: usize,
    pub versions: usize,
    pub environments: usize,
    pub deployments: usize,
    pub routes: usize,
    pub secrets: usize,
    /// Bundle files that are now unreferenced. Not serialized: these are
    /// control-plane local paths.
    #[serde(skip)]
    pub bundle_paths: Vec<String>,
    /// Canonical artifact directories now unreferenced by the catalog.
    #[serde(skip)]
    pub artifact_paths: Vec<String>,
    /// Names of the environments that were removed, read before the rows
    /// were deleted.
    ///
    /// The delete path needs them after the transaction: an application on a
    /// node is keyed by `(app_id, environment)`, and nothing can look them
    /// up once the rows are gone. Not serialized here - the API answer
    /// reports them alongside which nodes actually dropped the app.
    #[serde(skip)]
    pub environment_names: Vec<String>,
    /// What the same transaction wrote down about the name.
    ///
    /// The counted fields above are what was destroyed; this is what was
    /// *not* - the stored data, which stays where it is, and the name, which
    /// is held so that nothing can be deployed onto that data by accident.
    /// Not serialized inside this struct, whose whole meaning is "removed":
    /// the API answer carries it as a field of its own.
    #[serde(skip)]
    pub retired: RetiredAppName,
}

/// A name that belonged to an application which has been deleted, and which
/// is therefore not free.
///
/// Deleting an application removes it from this catalog and leaves everything
/// it stored where it was: deis-store and deis-queue key a tenant by
/// `(application, environment)` and neither of them has an operation that
/// drops one. So the name is retained rather than the data, and nothing can
/// be deployed onto it until somebody says out loud that the next application
/// of that name may have what the last one left - see
/// `CatalogStore::release_retired_name`.
#[derive(Debug, Clone, Default, Serialize, ToSchema)]
pub struct RetiredAppName {
    pub app_id: String,
    /// When the application was deleted, Unix epoch seconds.
    pub retired_at: i64,
    /// The environments it had when it was deleted, in name order.
    ///
    /// One environment is one tenant, so this is the list of places its data
    /// still is. It can be empty - an application deleted before it had an
    /// environment - and the name is retained all the same: a storage token
    /// is minted per environment on request, and an environment that was
    /// created and used is not distinguishable here from one that was never
    /// touched.
    ///
    /// A name that has been used, deleted, released and used again carries
    /// the environments of every application that had it, because every one
    /// of those tenants may still hold data.
    pub environments: Vec<String>,
    /// When an operator freed the name again, if they have.
    ///
    /// `None` is a name that is still held, which is the only state that
    /// stops an application being created under it. The row survives the
    /// release rather than being deleted, because what it records - which
    /// tenants hold data under this name - stays true afterwards, and the
    /// next deletion of the same name has to add to that list rather than
    /// start a new one.
    pub released_at: Option<i64>,
}

impl RetiredAppName {
    /// Whether the name is still held, which is the state that refuses a
    /// creation.
    pub fn is_held(&self) -> bool {
        self.released_at.is_none()
    }

    /// The tenant ids whose data is still in deis-store and deis-queue,
    /// spelled the way those services see them.
    pub fn tenants(&self) -> Vec<String> {
        self.environments
            .iter()
            .map(|environment| deis_wire::conventions::tenant_id(&self.app_id, environment))
            .collect()
    }

    /// One line naming what is still out there, for a log or a refusal.
    pub fn describe_data(&self) -> String {
        let tenants = self.tenants();
        if tenants.is_empty() {
            format!(
                "no environment of {} was recorded when it was deleted, so no tenant name is \
                 known - data stored under one that was created and removed earlier would still \
                 be there",
                self.app_id
            )
        } else {
            format!(
                "the kv, database, object and queue data of {} is still in deis-store and \
                 deis-queue under tenant(s) {}",
                self.app_id,
                tenants.join(", ")
            )
        }
    }
}

/// Current wall clock as the Unix epoch seconds stored in every
/// `created_at` / `updated_at` column of the catalog tables.
pub fn now_secs() -> i64 {
    chrono::Utc::now().timestamp()
}

/// App ids become directory names under the bundle root and appear in
/// route tables, so they are restricted to a conservative slug: lowercase
/// letters, digits, `-` and `_`, starting with a letter or digit.
pub fn validate_app_id(id: &str) -> Result<(), CatalogError> {
    validate_slug(id, "app id", MAX_NAME_LEN, false)
}

/// Environment names follow the same rule as app ids, and for a reason
/// beyond tidiness: the pair `(app_id, environment)` is joined into the
/// storage tenant id with `.` as the separator
/// (`deis_wire::conventions::tenant_id`), and deis-store turns that id into
/// a directory and a database file name. A name carrying a `.` would make
/// the tenant id split into three parts, which the storage services reject:
/// the app would load and serve, and only its kv, database, blob and queue
/// calls would fail.
pub fn validate_environment_name(name: &str) -> Result<(), CatalogError> {
    validate_slug(name, "environment name", MAX_NAME_LEN, false)?;
    // Belt and braces: the rule above is ours, this one is the storage
    // services'. If they ever diverge, the environment is refused here
    // rather than becoming an application whose bindings silently fail.
    if !deis_wire::conventions::is_valid_tenant_part(name) {
        return Err(CatalogError::invalid(format!(
            "environment name {name:?} cannot be part of a storage tenant id, so this environment could never use kv, database, blob or queue"
        )));
    }
    Ok(())
}

fn validate_slug(
    value: &str,
    what: &str,
    max_len: usize,
    allow_dot: bool,
) -> Result<(), CatalogError> {
    if value.is_empty() {
        return Err(CatalogError::invalid(format!("{what} must not be empty")));
    }
    if value.len() > max_len {
        return Err(CatalogError::invalid(format!(
            "{what} is longer than {max_len} characters"
        )));
    }
    let first = value.as_bytes()[0];
    if !first.is_ascii_lowercase() && !first.is_ascii_digit() {
        return Err(CatalogError::invalid(format!(
            "{what} must start with a lowercase letter or a digit, got {value:?}"
        )));
    }
    for ch in value.chars() {
        let ok = ch.is_ascii_lowercase()
            || ch.is_ascii_digit()
            || ch == '-'
            || ch == '_'
            || (allow_dot && ch == '.');
        if !ok {
            return Err(CatalogError::invalid(format!(
                "{what} may only contain lowercase letters, digits, '-'{} and '_', got {value:?}",
                if allow_dot { ", '.'" } else { "" }
            )));
        }
    }
    Ok(())
}

/// Secret names are injected as environment variables into the isolate,
/// so they follow the shell variable rule: letter or `_` first, then
/// letters, digits and `_`.
pub fn validate_secret_name(name: &str) -> Result<(), CatalogError> {
    if name.is_empty() {
        return Err(CatalogError::invalid("secret name must not be empty"));
    }
    if name.len() > MAX_SECRET_NAME_LEN {
        return Err(CatalogError::invalid(format!(
            "secret name is longer than {MAX_SECRET_NAME_LEN} characters"
        )));
    }
    let first = name.as_bytes()[0];
    if !first.is_ascii_alphabetic() && first != b'_' {
        return Err(CatalogError::invalid(format!(
            "secret name must start with a letter or '_', got {name:?}"
        )));
    }
    if !name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') {
        return Err(CatalogError::invalid(format!(
            "secret name may only contain letters, digits and '_', got {name:?}"
        )));
    }
    Ok(())
}

/// Route path prefixes are matched literally against the request path, so
/// they must be absolute and free of the traversal segments that would let
/// one environment shadow another.
///
/// One spelling only, for the same reason [`super::domains::HostClaim`] has
/// one: the edge trims a trailing `/` before it compares
/// (`deis_proxy::catalog::table::CompiledRoute::covers`), so `/api` and
/// `/api/` are one rule written two ways - two rows that the uniqueness
/// index in `database::schema` would happily hold side by side while the
/// edge treated them as the same claim. The trailing slash is refused rather
/// than trimmed, so there is nothing to trim inconsistently later.
pub fn validate_path_prefix(prefix: &str) -> Result<(), CatalogError> {
    if !prefix.starts_with('/') {
        return Err(CatalogError::invalid(format!(
            "route path_prefix must start with '/', got {prefix:?}"
        )));
    }
    if prefix.len() > MAX_PATH_PREFIX_LEN {
        return Err(CatalogError::invalid(format!(
            "route path_prefix is longer than {MAX_PATH_PREFIX_LEN} characters"
        )));
    }
    if prefix.contains("..") {
        return Err(CatalogError::invalid(
            "route path_prefix must not contain '..'",
        ));
    }
    if prefix.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err(CatalogError::invalid(
            "route path_prefix must not contain whitespace or control characters",
        ));
    }
    if prefix.contains("//") {
        return Err(CatalogError::invalid(format!(
            "route path_prefix {prefix:?} has an empty segment; the edge matches whole path \
             segments, so '//' is a prefix nothing can arrive on"
        )));
    }
    if prefix.len() > 1 && prefix.ends_with('/') {
        return Err(CatalogError::invalid(format!(
            "route path_prefix {prefix:?} ends with '/'. The edge trims that before it compares, \
             so this and {:?} are the same claim written two ways and only one of them can be \
             held. Write it without the trailing slash",
            prefix.trim_end_matches('/')
        )));
    }
    Ok(())
}

/// Widest and narrowest ordering hint a route may carry.
///
/// `priority` is a caller-supplied integer that becomes an ordering key in
/// the edge's match (`deis_proxy::catalog::table::candidate_order`, highest
/// first). Unbounded, it is a "go first" button: `i64::MAX` put an
/// attacker's route ahead of every other route on the same host, and ahead
/// of every other tenant's in the wildcard and catch-all buckets, which is
/// where routes from different applications do share an ordering. A hint has
/// a range; this is it, and a caller outside it is told so rather than
/// silently clamped, because a clamp is a second value nobody wrote down.
pub const MIN_ROUTE_PRIORITY: i64 = -1_000;
pub const MAX_ROUTE_PRIORITY: i64 = 1_000;

pub fn validate_route_priority(priority: i64) -> Result<(), CatalogError> {
    if !(MIN_ROUTE_PRIORITY..=MAX_ROUTE_PRIORITY).contains(&priority) {
        return Err(CatalogError::invalid(format!(
            "route priority {priority} is outside {MIN_ROUTE_PRIORITY}..={MAX_ROUTE_PRIORITY}. \
             Priority breaks ties between routes that match the same request; it is a small hint, \
             not a place in a global queue"
        )));
    }
    Ok(())
}

/// How many routes one environment, and one application, may hold.
///
/// Ownership of a name is not the whole story. An application granted a zone
/// can still create a route per name inside it, and every one of those names
/// goes to the edge's certificate pass, where a public authority counts new
/// certificates per registered domain per week. Thirty routes named
/// `a001..a030` under a granted zone spend that allowance and the renewal
/// ceiling, which is why the ceiling is shared fairly
/// (`deis_proxy::acme::names`) *and* why the number of claims one tenant can
/// make is bounded here.
pub const MAX_ROUTES_PER_ENVIRONMENT: usize = 20;
pub const MAX_ROUTES_PER_APP: usize = 100;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn app_id_rules() {
        assert!(validate_app_id("my-app").is_ok());
        assert!(validate_app_id("app_1").is_ok());
        assert!(validate_app_id("").is_err());
        assert!(validate_app_id("-leading").is_err());
        assert!(validate_app_id("Upper").is_err());
        assert!(validate_app_id("has space").is_err());
        // A traversal attempt must never reach the bundle directory.
        assert!(validate_app_id("../etc").is_err());
        assert!(validate_app_id("a/b").is_err());
        assert!(validate_app_id(&"a".repeat(MAX_NAME_LEN + 1)).is_err());
    }

    /// An environment name is half of a storage tenant id, so it may not
    /// carry the separator that joins the two halves.
    #[test]
    fn environment_names_must_be_usable_as_a_storage_tenant_half() {
        assert!(validate_environment_name("production").is_ok());
        assert!(validate_environment_name("pr-42").is_ok());
        assert!(validate_environment_name("pr/42").is_err());
        assert!(
            validate_environment_name("pr-42.1").is_err(),
            "a dot would split the tenant id into three parts, which deis-store rejects"
        );

        // What the rule is really protecting: every accepted pair joins and
        // splits back to itself.
        for (app, env) in [("demo", "production"), ("my_app-2", "pr-42")] {
            assert!(validate_app_id(app).is_ok());
            assert!(validate_environment_name(env).is_ok());
            let tenant = deis_wire::conventions::tenant_id(app, env);
            assert_eq!(
                deis_wire::conventions::split_tenant(&tenant),
                Some((app, env))
            );
        }
    }

    #[test]
    fn secret_names_follow_environment_variable_rules() {
        assert!(validate_secret_name("API_KEY").is_ok());
        assert!(validate_secret_name("_private2").is_ok());
        assert!(validate_secret_name("2FA").is_err());
        assert!(validate_secret_name("api-key").is_err());
    }

    #[test]
    fn path_prefix_rules() {
        assert!(validate_path_prefix("/").is_ok());
        assert!(validate_path_prefix("/api/users").is_ok());
        assert!(validate_path_prefix("api").is_err());
        assert!(validate_path_prefix("/a/../b").is_err());
        assert!(validate_path_prefix("/a b").is_err());
    }

    /// The trailing slash and the empty segment are the two spellings the
    /// edge would treat as a claim this control plane had stored under a
    /// different string.
    #[test]
    fn a_path_prefix_has_one_spelling() {
        assert!(
            validate_path_prefix("/").is_ok(),
            "the root is its own case"
        );
        assert!(validate_path_prefix("/api/").is_err());
        assert!(validate_path_prefix("/api//v2").is_err());
    }

    #[test]
    fn a_priority_is_a_hint_and_not_a_place_in_a_global_queue() {
        assert!(validate_route_priority(0).is_ok());
        assert!(validate_route_priority(MAX_ROUTE_PRIORITY).is_ok());
        assert!(validate_route_priority(MIN_ROUTE_PRIORITY).is_ok());
        assert!(validate_route_priority(i64::MAX).is_err());
        assert!(validate_route_priority(i64::MIN).is_err());
        assert!(validate_route_priority(MAX_ROUTE_PRIORITY + 1).is_err());
    }
}
