//! Lifecycle operations mapped onto wrangler 4.129.0 commands.
//!
//! | Operation | wrangler |
//! |---|---|
//! | preflight | `whoami --json` |
//! | plan | (local) read `binding-plan.json`, check `wrangler.toml` renders |
//! | prepare | per resource: `kv namespace list`/`create`, `d1 list --json`/`create`, `r2 bucket info`/`create`, `queues info`/`create` |
//! | upload | `versions list --name <worker> --json` to see whether the Worker exists; for a new Worker, `deploy --message deis:placeholder` of a placeholder script first (wrangler refuses `versions upload` for a Worker that does not exist); then `versions upload --message deis:<digest> [--secrets-file /dev/stdin]` in a staged copy with the rendered `wrangler.toml` |
//! | verify | `versions list --name <worker> --json` |
//! | activate | `versions deploy <version>@100% --name <worker> --yes`, then `triggers deploy` with the rendered `wrangler.toml` |
//! | observe, inspect | `deployments status --name <worker> --json` |
//! | rollback | `versions deploy <target version>@100% --name <worker> --yes`, then `triggers deploy` with the configuration the target was activated with |
//! | reconcile | the read-back of the operation it takes over (see [`reconcile`]) |
//! | delete | `delete <worker> --force`, then the purge below when consented |
//! | cleanup | local state only, then the purge below when consented |
//!
//! Triggers: `versions upload` and `versions deploy` do not touch what
//! Cloudflare keeps per Worker rather than per version - cron schedules,
//! queue consumers, and the workers.dev route. `triggers deploy` applies them
//! from the rendered `wrangler.toml`: it PUTs `[triggers] crons` (always
//! present in the rendered file, empty when the application has none, so a
//! removed cron is cleared), attaches each `[[queues.consumers]]` entry and
//! enables the workers.dev subdomain. It adds and updates queue consumers
//! but never detaches one the configuration no longer lists.
//!
//! The placeholder that creates a new Worker answers 503 and is deployed
//! with `workers_dev = false`, so nothing is reachable before activate; its
//! version is never the one `activate` deploys.
//!
//! Purge (only with `DEIS_CLOUDFLARE_PURGE_DATA=<environment id>`): every KV
//! namespace and D1 database whose name carries this environment's scope
//! prefix, plus the R2 buckets and queues the journal or plan name, are
//! deleted with `kv namespace delete`, `d1 delete`, `r2 bucket delete` and
//! `queues delete`. Nothing outside the scope is ever touched.
//!
//! Every wrangler failure is an error: [`Wrangler::run`] turns a non-zero
//! exit into [`WranglerError::CommandFailed`], and the only place a failure
//! is treated as success is a delete of something Cloudflare reports as
//! already absent (API codes [`NOT_FOUND_CODES`]).

use std::collections::BTreeMap;
use std::io;
use std::path::Path;
use std::time::Duration;

use chrono::Utc;
use deis_build_core::BindingKind;
use deis_provider_driver::{
    DeploymentState, DriverError, DriverErrorCategory, DriverNextAction, DriverOperation,
    DriverReceipt, DriverRequest, DriverStepState, SideEffect,
};
use tracing::info;

use crate::artifact::{
    self, ArtifactError, BindingPlanDocument, RenderInputs, RequiredResource, WRANGLER_TOML_FILE,
};
use crate::credentials::CloudflareCredential;
use crate::environment::SecretValues;
use crate::journal::{DeploymentJournal, JournalResource, JournalStore, UploadedVersion};
use crate::naming::{DeploymentScope, kind_token};
use crate::wrangler::{Wrangler, WranglerError, WranglerOutput};

/// Cloudflare API codes meaning "the Worker does not exist"
/// (`workers.api.error.script_not_found`, `service_not_found`).
pub const NOT_FOUND_CODES: [u64; 2] = [10007, 10090];
/// wrangler's (code-less) user error for a Worker that exists but has never
/// had a deployment.
const NO_DEPLOYMENTS: &str = "has no deployments";
/// The message of the placeholder version that creates a new Worker.
pub const PLACEHOLDER_MESSAGE: &str = "deis:placeholder";
const PLACEHOLDER_SCRIPT: &str = "export default {\n  fetch() {\n    return new Response(\"This deployment has not been activated yet.\\n\", { status: 503 });\n  },\n};\n";
const PLACEHOLDER_COMPATIBILITY_DATE: &str = "2025-01-01";

/// Cloudflare API codes meaning the credential is not accepted.
const AUTHENTICATION_CODES: [u64; 4] = [9106, 9109, 10000, 10001];

/// Timeouts for wrangler invocations, each inside the process budget
/// ([`crate::protocol::PROCESS_TIMEOUT`]).
#[derive(Clone, Copy, Debug)]
pub struct Timeouts {
    pub command: Duration,
    pub upload: Duration,
}

impl Default for Timeouts {
    fn default() -> Self {
        Self {
            command: Duration::from_secs(90),
            upload: Duration::from_secs(200),
        }
    }
}

/// Everything one operation runs with.
pub struct OperationContext<'a> {
    pub wrangler: &'a Wrangler,
    pub credential: &'a CloudflareCredential,
    /// The build artifact: `wrangler.toml`, `binding-plan.json`, code.
    pub artifact_dir: &'a Path,
    pub journals: &'a JournalStore,
    /// Where empty working directories and staged artifacts are created.
    pub scratch_root: &'a Path,
    /// Deployment config values for `[vars]`.
    pub config: &'a BTreeMap<String, String>,
    pub secrets: &'a SecretValues,
    /// The environment id `DEIS_CLOUDFLARE_PURGE_DATA` names, if any.
    pub purge_environment_id: Option<&'a str>,
    pub timeouts: Timeouts,
}

/// Execute one lifecycle operation.
pub async fn execute_operation(
    ctx: &OperationContext<'_>,
    request: &DriverRequest,
) -> Result<DriverReceipt, DriverError> {
    let op = Op::new(ctx, request);
    let result = match request.operation {
        DriverOperation::Preflight => op.preflight().await,
        DriverOperation::Plan => op.plan(),
        DriverOperation::Prepare => op.prepare().await,
        DriverOperation::Upload => op.upload().await,
        DriverOperation::Verify => op.verify().await,
        DriverOperation::Activate => op.activate().await,
        DriverOperation::Observe => op.observe().await,
        DriverOperation::Rollback => op.rollback().await,
        DriverOperation::Inspect => op.inspect().await,
        DriverOperation::Reconcile => op.reconcile().await,
        DriverOperation::Delete => op.delete().await,
        DriverOperation::Cleanup => op.cleanup().await,
    };
    result.map_err(|error| op.driver_error(error))
}

/// Why an operation failed, before it becomes a [`DriverError`].
#[derive(Debug)]
pub enum OpError {
    Wrangler(WranglerError),
    Artifact(ArtifactError),
    Journal(io::Error),
    /// A state the request cannot proceed from.
    State {
        category: DriverErrorCategory,
        code: &'static str,
        message: String,
        retryable: bool,
        next_action: DriverNextAction,
        side_effect: SideEffect,
    },
}

impl From<WranglerError> for OpError {
    fn from(error: WranglerError) -> Self {
        Self::Wrangler(error)
    }
}

impl From<ArtifactError> for OpError {
    fn from(error: ArtifactError) -> Self {
        Self::Artifact(error)
    }
}

impl From<io::Error> for OpError {
    fn from(error: io::Error) -> Self {
        Self::Journal(error)
    }
}

struct Op<'a, 'c> {
    ctx: &'a OperationContext<'c>,
    request: &'a DriverRequest,
    scope: DeploymentScope,
    /// The operation whose semantics apply: the reconciled one for a
    /// reconcile, otherwise the request's own.
    effective: DriverOperation,
}

impl<'a, 'c> Op<'a, 'c> {
    fn new(ctx: &'a OperationContext<'c>, request: &'a DriverRequest) -> Self {
        let effective = match (&request.operation, &request.reconciles) {
            (DriverOperation::Reconcile, Some(context)) => context.operation,
            (operation, _) => *operation,
        };
        Self {
            ctx,
            request,
            scope: DeploymentScope::new(&request.application_id, &request.environment_id),
            effective,
        }
    }

    fn worker(&self) -> String {
        self.scope.worker_name()
    }

    fn upload_message(&self) -> String {
        format!("deis:{}", self.request.desired_state_digest)
    }

    // ── wrangler plumbing ──────────────────────────────────────────────────

    /// Run wrangler in a fresh empty directory, so no `wrangler.toml` is in
    /// reach.
    async fn run(&self, args: &[&str]) -> Result<WranglerOutput, WranglerError> {
        let dir = self.empty_dir()?;
        self.ctx
            .wrangler
            .run(
                self.ctx.credential,
                args,
                dir.path(),
                None,
                self.ctx.timeouts.command,
            )
            .await
    }

    fn empty_dir(&self) -> Result<tempfile::TempDir, WranglerError> {
        std::fs::create_dir_all(self.ctx.scratch_root).map_err(|error| WranglerError::Io {
            context: format!("creating {}", self.ctx.scratch_root.display()),
            error,
        })?;
        tempfile::Builder::new()
            .prefix("cwd-")
            .tempdir_in(self.ctx.scratch_root)
            .map_err(|error| WranglerError::Io {
                context: "creating an empty working directory".to_owned(),
                error,
            })
    }

    fn load_journal(&self) -> Result<DeploymentJournal, OpError> {
        Ok(self.ctx.journals.load(&self.scope)?)
    }

    fn save_journal(&self, journal: &DeploymentJournal) -> Result<(), OpError> {
        Ok(self.ctx.journals.save(&self.scope, journal)?)
    }

    fn plan_and_resources(&self) -> Result<(BindingPlanDocument, Vec<RequiredResource>), OpError> {
        let plan = BindingPlanDocument::load(self.ctx.artifact_dir)?;
        let resources = plan.required_resources(&self.scope)?;
        Ok((plan, resources))
    }

    // ── operations ─────────────────────────────────────────────────────────

    async fn preflight(&self) -> Result<DriverReceipt, OpError> {
        self.run(&["whoami", "--json"]).await?;
        Ok(self.receipt(None))
    }

    fn plan(&self) -> Result<DriverReceipt, OpError> {
        let (plan, resources) = self.plan_and_resources()?;
        let names = names_by_key(&resources);
        // Render once with the names standing in for identifiers: this
        // proves every section matches the plan and every `[vars]` value is
        // provided before anything is created.
        let source = read_wrangler_toml(self.ctx.artifact_dir)?;
        artifact::render_wrangler_toml(
            &source,
            &plan,
            &RenderInputs {
                worker_name: &self.worker(),
                resolved: &names,
                names: &names,
                config: self.ctx.config,
            },
        )?;
        let mut ids = names;
        ids.insert("worker".to_owned(), self.worker());
        let mut receipt = self.receipt(Some(DeploymentState::Planned));
        receipt.provider_resource_ids = ids;
        Ok(receipt)
    }

    async fn prepare(&self) -> Result<DriverReceipt, OpError> {
        let (_, resources) = self.plan_and_resources()?;
        let mut journal = self.load_journal()?;
        journal.worker_name = Some(self.worker());
        for resource in &resources {
            let id = match resource.kind {
                BindingKind::Kv => self.ensure_kv(&resource.name).await?,
                BindingKind::Database => self.ensure_d1(&resource.name).await?,
                BindingKind::Blob => self.ensure_named(&["r2", "bucket"], &resource.name).await?,
                BindingKind::Queue => self.ensure_named(&["queues"], &resource.name).await?,
            };
            info!(
                kind = kind_token(resource.kind),
                name = %resource.name,
                "Cloudflare resource ready"
            );
            journal.resources.insert(
                resource.key(),
                JournalResource {
                    kind: kind_token(resource.kind).to_owned(),
                    name: resource.name.clone(),
                    id,
                },
            );
            // Saved after each resource, so a crash mid-prepare still knows
            // what exists.
            self.save_journal(&journal)?;
        }
        let mut receipt = self.receipt(Some(DeploymentState::Preparing));
        receipt.side_effect = SideEffect::Confirmed;
        receipt.provider_resource_ids = self.resource_ids(&journal);
        Ok(receipt)
    }

    /// The artifact's `wrangler.toml` with every placeholder resolved from
    /// the journal.
    fn render_for_deploy(&self, journal: &DeploymentJournal) -> Result<String, OpError> {
        let (plan, resources) = self.plan_and_resources()?;
        let mut resolved = BTreeMap::new();
        for resource in &resources {
            let entry = journal.resources.get(&resource.key()).ok_or_else(|| {
                ArtifactError::Unresolved {
                    placeholder: deis_build_core::unresolved_resource_ref(
                        resource.kind,
                        &resource.logical,
                    ),
                }
            })?;
            resolved.insert(resource.key(), entry.id.clone());
        }
        let names = names_by_key(&resources);
        let source = read_wrangler_toml(self.ctx.artifact_dir)?;
        Ok(artifact::render_wrangler_toml(
            &source,
            &plan,
            &RenderInputs {
                worker_name: &self.worker(),
                resolved: &resolved,
                names: &names,
                config: self.ctx.config,
            },
        )?)
    }

    async fn upload(&self) -> Result<DriverReceipt, OpError> {
        let mut journal = self.load_journal()?;
        let rendered = self.render_for_deploy(&journal)?;
        let worker = self.worker();
        if !self.worker_exists().await? {
            self.create_worker().await?;
        }

        std::fs::create_dir_all(self.ctx.scratch_root)?;
        let staging = tempfile::Builder::new()
            .prefix("stage-")
            .tempdir_in(self.ctx.scratch_root)?;
        artifact::stage(self.ctx.artifact_dir, staging.path())?;
        std::fs::write(staging.path().join(WRANGLER_TOML_FILE), &rendered)?;

        let message = self.upload_message();
        let mut args = vec!["versions", "upload", "--message", message.as_str()];
        let stdin = if self.ctx.secrets.is_empty() {
            None
        } else {
            // wrangler reads the file with readFileSync; /dev/stdin keeps the
            // values off argv and off disk, and binds them to this version.
            args.extend(["--secrets-file", "/dev/stdin"]);
            Some(self.ctx.secrets.to_stdin_json())
        };
        let output = self
            .ctx
            .wrangler
            .run(
                self.ctx.credential,
                &args,
                staging.path(),
                stdin.as_deref(),
                self.ctx.timeouts.upload,
            )
            .await?;
        let version_id = output
            .record("version-upload")
            .and_then(|record| record["version_id"].as_str())
            .map(str::to_owned)
            .ok_or_else(|| WranglerError::OutputParse {
                command: "versions upload".to_owned(),
                message: "no version-upload record with a version_id in the output file".to_owned(),
                output: output.stdout.clone(),
            })?;
        journal.worker_name = Some(worker);
        journal.uploaded = Some(UploadedVersion {
            version_id: version_id.clone(),
            desired_state_digest: self.request.desired_state_digest.to_string(),
        });
        self.save_journal(&journal)?;
        let mut receipt = self.receipt(Some(DeploymentState::Uploaded));
        receipt.side_effect = SideEffect::Confirmed;
        receipt.provider_version_id = Some(version_id);
        receipt.provider_resource_ids = self.resource_ids(&journal);
        Ok(receipt)
    }

    /// The version `upload` recorded for this request's desired state.
    fn uploaded_version(&self, journal: &DeploymentJournal) -> Result<String, OpError> {
        match &journal.uploaded {
            Some(uploaded)
                if uploaded.desired_state_digest == self.request.desired_state_digest.as_str() =>
            {
                Ok(uploaded.version_id.clone())
            }
            _ => Err(OpError::State {
                category: DriverErrorCategory::Configuration,
                code: "UPLOAD_MISSING",
                message: format!(
                    "no version was uploaded for desired state {}; run upload first",
                    self.request.desired_state_digest
                ),
                retryable: false,
                next_action: DriverNextAction::None,
                side_effect: SideEffect::None,
            }),
        }
    }

    async fn verify(&self) -> Result<DriverReceipt, OpError> {
        let journal = self.load_journal()?;
        let version_id = self.uploaded_version(&journal)?;
        let versions = self.list_versions().await?;
        if !versions
            .iter()
            .any(|version| version["id"] == version_id.as_str())
        {
            return Err(OpError::State {
                category: DriverErrorCategory::Verification,
                code: "VERSION_NOT_LISTED",
                message: format!(
                    "version {version_id} is not among the deployable versions of {}",
                    self.worker()
                ),
                retryable: true,
                next_action: DriverNextAction::Retry,
                side_effect: SideEffect::None,
            });
        }
        let mut receipt = self.receipt(Some(DeploymentState::Ready));
        receipt.provider_version_id = Some(version_id);
        Ok(receipt)
    }

    async fn activate(&self) -> Result<DriverReceipt, OpError> {
        let mut journal = self.load_journal()?;
        let version_id = self.uploaded_version(&journal)?;
        // Rendered before anything changes, so an artifact that no longer
        // renders fails without touching traffic.
        let rendered = self.render_for_deploy(&journal)?;
        let deployment_id = self.deploy_version(&version_id).await?;
        journal.active_version_id = Some(version_id.clone());
        journal.deployment_id = deployment_id.clone();
        // Saved before the triggers, so a failure there is reconciled from
        // "the version serves" onwards.
        self.save_journal(&journal)?;
        self.activate_triggers(&version_id, &rendered).await?;
        let mut receipt = self.receipt(Some(DeploymentState::Active));
        receipt.side_effect = SideEffect::Confirmed;
        receipt.provider_version_id = Some(version_id);
        receipt.provider_deployment_id = deployment_id;
        receipt.provider_resource_ids = self.resource_ids(&journal);
        Ok(receipt)
    }

    async fn rollback(&self) -> Result<DriverReceipt, OpError> {
        let version_id = self.rollback_version()?;
        let mut journal = self.load_journal()?;
        let deployment_id = self.deploy_version(&version_id).await?;
        journal.active_version_id = Some(version_id.clone());
        journal.deployment_id = deployment_id.clone();
        self.save_journal(&journal)?;
        let diagnostic = self.rollback_triggers(&version_id).await?;
        let mut receipt = self.receipt(Some(DeploymentState::RolledBack));
        receipt.diagnostic = diagnostic;
        receipt.side_effect = SideEffect::Confirmed;
        receipt.provider_version_id = Some(version_id);
        receipt.provider_deployment_id = deployment_id;
        receipt.provider_resource_ids = self.resource_ids(&journal);
        Ok(receipt)
    }

    fn rollback_version(&self) -> Result<String, OpError> {
        let missing = |code: &'static str, message: &str| OpError::State {
            category: DriverErrorCategory::Configuration,
            code,
            message: message.to_owned(),
            retryable: false,
            next_action: DriverNextAction::None,
            side_effect: SideEffect::None,
        };
        let target = self.request.rollback_target.as_ref().ok_or_else(|| {
            missing(
                "ROLLBACK_TARGET_MISSING",
                "rollback request has no rollback target",
            )
        })?;
        target
            .provider_version_id
            .clone()
            .ok_or_else(|| missing("ROLLBACK_VERSION_MISSING", "rollback target has no version"))
    }

    async fn deploy_version(&self, version_id: &str) -> Result<Option<String>, OpError> {
        let worker = self.worker();
        let spec = format!("{version_id}@100%");
        let message = self.upload_message();
        let output = self
            .run(&[
                "versions",
                "deploy",
                spec.as_str(),
                "--name",
                worker.as_str(),
                "--message",
                message.as_str(),
                "--yes",
            ])
            .await?;
        Ok(output
            .record("version-deploy")
            .and_then(|record| record["deployment_id"].as_str())
            .map(str::to_owned))
    }

    /// Apply the per-Worker configuration of an activated version and keep
    /// it for a later rollback to this version.
    async fn activate_triggers(&self, version_id: &str, rendered: &str) -> Result<(), OpError> {
        self.ctx
            .journals
            .save_version_config(&self.scope, version_id, rendered)?;
        self.deploy_triggers(rendered).await
    }

    /// Re-apply the per-Worker configuration the rollback target was
    /// activated with. A version this driver has no configuration for (its
    /// state directory was lost) is served as it is, with the triggers left
    /// unchanged and said so in the diagnostic.
    async fn rollback_triggers(&self, version_id: &str) -> Result<Option<String>, OpError> {
        match self
            .ctx
            .journals
            .load_version_config(&self.scope, version_id)?
        {
            Some(rendered) => {
                self.deploy_triggers(&rendered).await?;
                Ok(None)
            }
            None => Ok(Some(format!(
                "no stored configuration for version {version_id}: cron schedules, queue \
                 consumers and routes of {} were left as they were",
                self.worker()
            ))),
        }
    }

    /// `wrangler triggers deploy` in a directory holding only the rendered
    /// `wrangler.toml` (it reads the configuration, never the code).
    async fn deploy_triggers(&self, rendered: &str) -> Result<(), OpError> {
        let dir = self.empty_dir()?;
        std::fs::write(dir.path().join(WRANGLER_TOML_FILE), rendered)?;
        self.ctx
            .wrangler
            .run(
                self.ctx.credential,
                &["triggers", "deploy"],
                dir.path(),
                None,
                self.ctx.timeouts.command,
            )
            .await?;
        Ok(())
    }

    /// Whether the Worker exists on the account.
    async fn worker_exists(&self) -> Result<bool, OpError> {
        match self.list_versions().await {
            Ok(_) => Ok(true),
            Err(OpError::Wrangler(error))
                if error
                    .api_code()
                    .is_some_and(|code| NOT_FOUND_CODES.contains(&code)) =>
            {
                Ok(false)
            }
            Err(error) => Err(error),
        }
    }

    /// Create the Worker with a placeholder that answers 503 and is not on
    /// workers.dev: wrangler uploads versions only for a Worker that exists,
    /// and `wrangler deploy` of the real code would make it serve before
    /// verify and activate.
    async fn create_worker(&self) -> Result<(), OpError> {
        std::fs::create_dir_all(self.ctx.scratch_root)?;
        let dir = tempfile::Builder::new()
            .prefix("placeholder-")
            .tempdir_in(self.ctx.scratch_root)?;
        std::fs::write(
            dir.path().join(WRANGLER_TOML_FILE),
            format!(
                "name = \"{}\"\nmain = \"placeholder.js\"\ncompatibility_date = \"{PLACEHOLDER_COMPATIBILITY_DATE}\"\nworkers_dev = false\n",
                self.worker()
            ),
        )?;
        std::fs::write(dir.path().join("placeholder.js"), PLACEHOLDER_SCRIPT)?;
        self.ctx
            .wrangler
            .run(
                self.ctx.credential,
                &["deploy", "--message", PLACEHOLDER_MESSAGE],
                dir.path(),
                None,
                self.ctx.timeouts.upload,
            )
            .await?;
        info!(worker = %self.worker(), "Created Worker with a placeholder");
        Ok(())
    }

    async fn observe(&self) -> Result<DriverReceipt, OpError> {
        let journal = self.load_journal()?;
        let status = self.deployment_status().await?;
        let serving = serving_version(&status);
        let expected = journal.active_version_id.clone();
        let healthy = expected.is_some() && serving == expected;
        let mut receipt = self.receipt(Some(if healthy {
            DeploymentState::Active
        } else {
            DeploymentState::Degraded
        }));
        receipt.provider_deployment_id = status["id"].as_str().map(str::to_owned);
        receipt.provider_version_id = serving.clone();
        if !healthy {
            receipt.diagnostic = Some(format!(
                "{} serves {serving:?} at 100%, expected {expected:?}",
                self.worker()
            ));
        }
        Ok(receipt)
    }

    async fn inspect(&self) -> Result<DriverReceipt, OpError> {
        let status = self.deployment_status().await?;
        let mut receipt = self.receipt(None);
        receipt.provider_deployment_id = status["id"].as_str().map(str::to_owned);
        receipt.provider_version_id = serving_version(&status);
        receipt.diagnostic = Some(format!("versions: {}", status["versions"]));
        Ok(receipt)
    }

    /// Answer "did the reconciled operation reach its intended state?" from
    /// provider state.
    ///
    /// * upload: a deployable version carrying this desired state's message;
    /// * activate / rollback: the target version serving 100% of traffic;
    /// * delete: the Worker is gone;
    /// * prepare, cleanup: re-run (both are idempotent);
    /// * read-only operations: re-run.
    async fn reconcile(&self) -> Result<DriverReceipt, OpError> {
        match self.effective {
            DriverOperation::Upload => {
                let message = self.upload_message();
                let versions = self.list_versions().await?;
                let found = versions
                    .iter()
                    .rev()
                    .find(|version| version["annotations"]["workers/message"] == message.as_str())
                    .and_then(|version| version["id"].as_str())
                    .map(str::to_owned);
                let Some(version_id) = found else {
                    return Err(not_applied(
                        "the upload left no version for this desired state",
                    ));
                };
                let mut journal = self.load_journal()?;
                journal.worker_name = Some(self.worker());
                journal.uploaded = Some(UploadedVersion {
                    version_id: version_id.clone(),
                    desired_state_digest: self.request.desired_state_digest.to_string(),
                });
                self.save_journal(&journal)?;
                let mut receipt = self.receipt(Some(DeploymentState::Uploaded));
                receipt.side_effect = SideEffect::Confirmed;
                receipt.provider_version_id = Some(version_id);
                Ok(receipt)
            }
            DriverOperation::Activate | DriverOperation::Rollback => {
                let target = if self.effective == DriverOperation::Rollback {
                    self.rollback_version()?
                } else {
                    self.uploaded_version(&self.load_journal()?)?
                };
                let status = match self.worker_status().await? {
                    WorkerStatus::Deployed(status) => Some(status),
                    WorkerStatus::Absent | WorkerStatus::NoDeployment => None,
                };
                let serving = status.as_ref().and_then(serving_version);
                if serving.as_deref() != Some(target.as_str()) {
                    return Err(not_applied(&format!(
                        "{} serves {serving:?}, not {target}",
                        self.worker()
                    )));
                }
                let mut journal = self.load_journal()?;
                journal.active_version_id = Some(target.clone());
                journal.deployment_id = status
                    .as_ref()
                    .and_then(|status| status["id"].as_str())
                    .map(str::to_owned);
                self.save_journal(&journal)?;
                // The version serves; the triggers step after it may not have
                // run. It is idempotent, so it runs again.
                let diagnostic = if self.effective == DriverOperation::Rollback {
                    self.rollback_triggers(&target).await?
                } else {
                    let rendered = self.render_for_deploy(&journal)?;
                    self.activate_triggers(&target, &rendered).await?;
                    None
                };
                let mut receipt =
                    self.receipt(Some(if self.effective == DriverOperation::Rollback {
                        DeploymentState::RolledBack
                    } else {
                        DeploymentState::Active
                    }));
                receipt.diagnostic = diagnostic;
                receipt.side_effect = SideEffect::Confirmed;
                receipt.provider_version_id = Some(target);
                receipt.provider_deployment_id = journal.deployment_id.clone();
                Ok(receipt)
            }
            DriverOperation::Delete => {
                if !matches!(self.worker_status().await?, WorkerStatus::Absent) {
                    return Err(not_applied(&format!("{} still exists", self.worker())));
                }
                let mut receipt = self.receipt(None);
                receipt.side_effect = SideEffect::Confirmed;
                Ok(receipt)
            }
            DriverOperation::Prepare => self.prepare().await,
            DriverOperation::Cleanup => self.cleanup().await,
            DriverOperation::Preflight => self.preflight().await,
            DriverOperation::Plan => self.plan(),
            DriverOperation::Verify => self.verify().await,
            DriverOperation::Observe => self.observe().await,
            DriverOperation::Inspect => self.inspect().await,
            DriverOperation::Reconcile => Err(OpError::State {
                category: DriverErrorCategory::DriverIntegrity,
                code: "RECONCILE_CONTEXT_MISSING",
                message: "a reconcile request must name the operation it takes over".to_owned(),
                retryable: false,
                next_action: DriverNextAction::None,
                side_effect: SideEffect::None,
            }),
        }
    }

    async fn delete(&self) -> Result<DriverReceipt, OpError> {
        let worker = self.worker();
        match self.run(&["delete", worker.as_str(), "--force"]).await {
            Ok(_) => info!(%worker, "Deleted Worker"),
            Err(error)
                if error
                    .api_code()
                    .is_some_and(|code| NOT_FOUND_CODES.contains(&code)) =>
            {
                info!(%worker, "Worker already absent");
            }
            Err(error) => return Err(error.into()),
        }
        let mut journal = self.load_journal()?;
        journal.uploaded = None;
        journal.active_version_id = None;
        journal.deployment_id = None;
        self.save_journal(&journal)?;
        let purged = if self.purge_consented() {
            self.purge(&mut journal).await?
        } else {
            Vec::new()
        };
        let mut receipt = self.receipt(None);
        receipt.side_effect = SideEffect::Confirmed;
        receipt.diagnostic = Some(self.purge_diagnostic(&journal, &purged));
        Ok(receipt)
    }

    async fn cleanup(&self) -> Result<DriverReceipt, OpError> {
        let mut journal = self.load_journal()?;
        // An upload that was never activated is abandoned; Cloudflare keeps
        // versions as history and has no per-version delete.
        if journal.uploaded.as_ref().is_some_and(|uploaded| {
            Some(&uploaded.version_id) != journal.active_version_id.as_ref()
        }) {
            journal.uploaded = None;
        }
        self.save_journal(&journal)?;
        let purged = if self.purge_consented() {
            if !matches!(self.worker_status().await?, WorkerStatus::Absent) {
                return Err(OpError::State {
                    category: DriverErrorCategory::Resource,
                    code: "PURGE_WHILE_WORKER_EXISTS",
                    message: format!(
                        "refusing to destroy the data of {} while the Worker still serves it; \
                         delete the Worker first",
                        self.worker()
                    ),
                    retryable: false,
                    next_action: DriverNextAction::ManualIntervention,
                    side_effect: SideEffect::None,
                });
            }
            self.purge(&mut journal).await?
        } else {
            Vec::new()
        };
        let mut receipt = self.receipt(None);
        receipt.side_effect = if purged.is_empty() {
            SideEffect::None
        } else {
            SideEffect::Confirmed
        };
        receipt.diagnostic = Some(self.purge_diagnostic(&journal, &purged));
        Ok(receipt)
    }

    fn purge_consented(&self) -> bool {
        self.ctx.purge_environment_id == Some(self.request.environment_id.to_string().as_str())
    }

    fn purge_diagnostic(&self, journal: &DeploymentJournal, purged: &[String]) -> String {
        if self.purge_consented() {
            format!("purged {} resource(s): {}", purged.len(), purged.join(", "))
        } else {
            format!(
                "{} data resource(s) retained; set DEIS_CLOUDFLARE_PURGE_DATA={} to destroy them",
                journal.resources.len(),
                self.request.environment_id
            )
        }
    }

    /// Destroy every data resource of this scope. Returns the deleted names.
    async fn purge(&self, journal: &mut DeploymentJournal) -> Result<Vec<String>, OpError> {
        let mut known: BTreeMap<String, String> = journal
            .resources
            .values()
            .map(|resource| (resource.name.clone(), resource.kind.clone()))
            .collect();
        if let Ok((_, resources)) = self.plan_and_resources() {
            for resource in resources {
                known.insert(resource.name, kind_token(resource.kind).to_owned());
            }
        }
        let mut deleted = Vec::new();

        for namespace in self.kv_namespaces().await? {
            let (Some(id), Some(title)) = (namespace["id"].as_str(), namespace["title"].as_str())
            else {
                continue;
            };
            if self.scope.owns(title) {
                self.run(&[
                    "kv",
                    "namespace",
                    "delete",
                    "--namespace-id",
                    id,
                    "--skip-confirmation",
                ])
                .await?;
                deleted.push(title.to_owned());
            }
        }
        for database in self.d1_databases().await? {
            let Some(name) = database["name"].as_str() else {
                continue;
            };
            if self.scope.owns(name) {
                self.run(&["d1", "delete", name, "--skip-confirmation"])
                    .await?;
                deleted.push(name.to_owned());
            }
        }
        for (name, kind) in &known {
            let group: &[&str] = match kind.as_str() {
                "r2" => &["r2", "bucket"],
                "q" => &["queues"],
                _ => continue,
            };
            if !self.scope.owns(name) || !self.named_exists(group, name).await? {
                continue;
            }
            let mut args = group.to_vec();
            args.extend(["delete", name.as_str()]);
            self.run(&args).await?;
            deleted.push(name.clone());
        }

        journal.resources.clear();
        self.save_journal(journal)?;
        Ok(deleted)
    }

    // ── resource helpers ───────────────────────────────────────────────────

    async fn kv_namespaces(&self) -> Result<Vec<serde_json::Value>, OpError> {
        let output = self.run(&["kv", "namespace", "list"]).await?;
        json_array(&output, "kv namespace list")
    }

    async fn d1_databases(&self) -> Result<Vec<serde_json::Value>, OpError> {
        let output = self.run(&["d1", "list", "--json"]).await?;
        json_array(&output, "d1 list --json")
    }

    async fn ensure_kv(&self, name: &str) -> Result<String, OpError> {
        let find = |namespaces: &[serde_json::Value]| {
            namespaces
                .iter()
                .find(|namespace| namespace["title"] == name)
                .and_then(|namespace| namespace["id"].as_str())
                .map(str::to_owned)
        };
        if let Some(id) = find(&self.kv_namespaces().await?) {
            return Ok(id);
        }
        self.run(&["kv", "namespace", "create", name]).await?;
        find(&self.kv_namespaces().await?).ok_or_else(|| {
            OpError::Wrangler(WranglerError::OutputParse {
                command: "kv namespace list".to_owned(),
                message: format!("namespace {name:?} is missing right after it was created"),
                output: String::new(),
            })
        })
    }

    async fn ensure_d1(&self, name: &str) -> Result<String, OpError> {
        let find = |databases: &[serde_json::Value]| {
            databases
                .iter()
                .find(|database| database["name"] == name)
                .and_then(|database| database["uuid"].as_str())
                .map(str::to_owned)
        };
        if let Some(id) = find(&self.d1_databases().await?) {
            return Ok(id);
        }
        self.run(&["d1", "create", name]).await?;
        find(&self.d1_databases().await?).ok_or_else(|| {
            OpError::Wrangler(WranglerError::OutputParse {
                command: "d1 list --json".to_owned(),
                message: format!("database {name:?} is missing right after it was created"),
                output: String::new(),
            })
        })
    }

    /// R2 buckets and queues are addressed by name. `info` succeeds only for
    /// one that exists; otherwise it is created (a create that fails, for
    /// whatever reason, is the error reported).
    async fn ensure_named(&self, group: &[&str], name: &str) -> Result<String, OpError> {
        if !self.named_exists(group, name).await? {
            let mut args = group.to_vec();
            args.extend(["create", name]);
            self.run(&args).await?;
        }
        Ok(name.to_owned())
    }

    async fn named_exists(&self, group: &[&str], name: &str) -> Result<bool, OpError> {
        let mut args = group.to_vec();
        args.extend(["info", name]);
        match self.run(&args).await {
            Ok(_) => Ok(true),
            // A rejected credential says nothing about existence.
            Err(error @ WranglerError::CommandFailed { .. })
                if error
                    .api_code()
                    .is_some_and(|code| AUTHENTICATION_CODES.contains(&code)) =>
            {
                Err(error.into())
            }
            Err(WranglerError::CommandFailed { .. }) => Ok(false),
            Err(error) => Err(error.into()),
        }
    }

    async fn list_versions(&self) -> Result<Vec<serde_json::Value>, OpError> {
        let worker = self.worker();
        let output = self
            .run(&["versions", "list", "--name", worker.as_str(), "--json"])
            .await?;
        json_array(&output, "versions list --json")
    }

    async fn deployment_status(&self) -> Result<serde_json::Value, OpError> {
        let worker = self.worker();
        let output = self
            .run(&["deployments", "status", "--name", worker.as_str(), "--json"])
            .await?;
        Ok(output.stdout_json("deployments status --json")?)
    }

    /// Like [`Self::deployment_status`], but telling an absent Worker and
    /// one without any deployment apart from a failure.
    async fn worker_status(&self) -> Result<WorkerStatus, OpError> {
        match self.deployment_status().await {
            Ok(status) => Ok(WorkerStatus::Deployed(status)),
            Err(OpError::Wrangler(error))
                if error
                    .api_code()
                    .is_some_and(|code| NOT_FOUND_CODES.contains(&code)) =>
            {
                Ok(WorkerStatus::Absent)
            }
            Err(OpError::Wrangler(error)) if error.failure_mentions(NO_DEPLOYMENTS) => {
                Ok(WorkerStatus::NoDeployment)
            }
            Err(error) => Err(error),
        }
    }

    // ── receipts and errors ────────────────────────────────────────────────

    fn resource_ids(&self, journal: &DeploymentJournal) -> BTreeMap<String, String> {
        let mut ids = journal
            .resources
            .iter()
            .map(|(key, resource)| (key.clone(), resource.id.clone()))
            .collect::<BTreeMap<_, _>>();
        ids.insert("worker".to_owned(), self.worker());
        ids
    }

    fn receipt(&self, deployment_state: Option<DeploymentState>) -> DriverReceipt {
        DriverReceipt {
            schema_version: self.request.schema_version,
            operation_id: self.request.operation_id,
            operation: self.request.operation,
            desired_state_digest: self.request.desired_state_digest.clone(),
            state: DriverStepState::Succeeded,
            deployment_state,
            side_effect: SideEffect::None,
            provider_request_id: None,
            provider_version_id: None,
            provider_deployment_id: None,
            provider_resource_ids: BTreeMap::new(),
            observed_at: Utc::now(),
            next_action: DriverNextAction::None,
            diagnostic: None,
        }
    }

    fn mutating(&self) -> bool {
        matches!(
            self.effective,
            DriverOperation::Prepare
                | DriverOperation::Upload
                | DriverOperation::Activate
                | DriverOperation::Rollback
                | DriverOperation::Delete
                | DriverOperation::Cleanup
        )
    }

    fn driver_error(&self, error: OpError) -> DriverError {
        let mutating = self.mutating();
        let (possible, after) = if mutating {
            (SideEffect::Possible, DriverNextAction::Reconcile)
        } else {
            (SideEffect::None, DriverNextAction::Retry)
        };
        let (category, code, retryable, side_effect, next_action, message) = match error {
            OpError::Wrangler(error) => {
                let message = error.to_string();
                match &error {
                    WranglerError::CommandFailed { api_code, .. }
                        if api_code.is_some_and(|code| AUTHENTICATION_CODES.contains(&code)) =>
                    {
                        (
                            DriverErrorCategory::Authentication,
                            "WRANGLER_AUTHENTICATION",
                            false,
                            SideEffect::None,
                            DriverNextAction::None,
                            message,
                        )
                    }
                    WranglerError::CommandFailed { .. } => (
                        self.failure_category(),
                        "WRANGLER_COMMAND_FAILED",
                        true,
                        possible,
                        after,
                        message,
                    ),
                    WranglerError::Timeout { .. } => (
                        DriverErrorCategory::TransientProvider,
                        "WRANGLER_TIMEOUT",
                        true,
                        possible,
                        after,
                        message,
                    ),
                    WranglerError::OutputParse { .. } => (
                        DriverErrorCategory::Internal,
                        "WRANGLER_OUTPUT_PARSE",
                        false,
                        possible,
                        if mutating {
                            DriverNextAction::Reconcile
                        } else {
                            DriverNextAction::None
                        },
                        message,
                    ),
                    WranglerError::Spawn { .. } => (
                        DriverErrorCategory::Internal,
                        "WRANGLER_SPAWN",
                        true,
                        SideEffect::None,
                        DriverNextAction::Retry,
                        message,
                    ),
                    WranglerError::Io { .. } => (
                        DriverErrorCategory::Internal,
                        "WRANGLER_IO",
                        true,
                        possible,
                        after,
                        message,
                    ),
                    WranglerError::Integrity { .. } => (
                        DriverErrorCategory::DriverIntegrity,
                        "WRANGLER_INTEGRITY",
                        false,
                        SideEffect::None,
                        DriverNextAction::None,
                        message,
                    ),
                    WranglerError::Download { .. } | WranglerError::Node { .. } => (
                        DriverErrorCategory::Internal,
                        "WRANGLER_INSTALL",
                        true,
                        SideEffect::None,
                        DriverNextAction::Retry,
                        message,
                    ),
                }
            }
            OpError::Artifact(error) => (
                DriverErrorCategory::Build,
                match error {
                    ArtifactError::Missing { .. } => "ARTIFACT_MISSING",
                    ArtifactError::Invalid { .. } => "ARTIFACT_INVALID",
                    ArtifactError::Unresolved { .. } => "PLACEHOLDER_UNRESOLVED",
                },
                false,
                SideEffect::None,
                DriverNextAction::None,
                error.to_string(),
            ),
            OpError::Journal(error) => (
                DriverErrorCategory::Internal,
                "DRIVER_STATE",
                true,
                possible,
                after,
                format!("driver journal: {error}"),
            ),
            OpError::State {
                category,
                code,
                message,
                retryable,
                next_action,
                side_effect,
            } => (category, code, retryable, side_effect, next_action, message),
        };
        let message = self
            .ctx
            .secrets
            .redact(&self.ctx.credential.redact(&message));
        DriverError {
            operation_id: self.request.operation_id,
            operation: self.request.operation,
            desired_state_digest: self.request.desired_state_digest.clone(),
            category,
            code: code.to_owned(),
            message,
            retryable,
            side_effect,
            next_action,
            provider_request_id: None,
        }
    }

    fn failure_category(&self) -> DriverErrorCategory {
        match self.effective {
            DriverOperation::Preflight => DriverErrorCategory::Authentication,
            DriverOperation::Prepare | DriverOperation::Delete | DriverOperation::Cleanup => {
                DriverErrorCategory::Resource
            }
            DriverOperation::Upload => DriverErrorCategory::Upload,
            DriverOperation::Activate | DriverOperation::Rollback => {
                DriverErrorCategory::Activation
            }
            DriverOperation::Verify | DriverOperation::Observe | DriverOperation::Inspect => {
                DriverErrorCategory::Verification
            }
            DriverOperation::Plan | DriverOperation::Reconcile => {
                DriverErrorCategory::TransientProvider
            }
        }
    }
}

/// What `deployments status` says about the Worker.
enum WorkerStatus {
    Absent,
    /// The Worker exists, but no version was ever deployed.
    NoDeployment,
    Deployed(serde_json::Value),
}

/// A reconcile that found the operation had no effect: safe to run again.
fn not_applied(message: &str) -> OpError {
    OpError::State {
        category: DriverErrorCategory::UnknownState,
        code: "NOT_APPLIED",
        message: message.to_owned(),
        retryable: true,
        next_action: DriverNextAction::Retry,
        side_effect: SideEffect::None,
    }
}

/// The version serving 100% of traffic in a `deployments status --json`
/// document.
fn serving_version(status: &serde_json::Value) -> Option<String> {
    status["versions"]
        .as_array()?
        .iter()
        .find(|version| version["percentage"].as_f64() == Some(100.0))
        .and_then(|version| version["version_id"].as_str())
        .map(str::to_owned)
}

fn json_array(output: &WranglerOutput, command: &str) -> Result<Vec<serde_json::Value>, OpError> {
    match output.stdout_json(command)? {
        serde_json::Value::Array(items) => Ok(items),
        other => Err(OpError::Wrangler(WranglerError::OutputParse {
            command: command.to_owned(),
            message: format!("expected a JSON array, got {}", json_kind(&other)),
            output: output.stdout.clone(),
        })),
    }
}

fn json_kind(value: &serde_json::Value) -> &'static str {
    match value {
        serde_json::Value::Null => "null",
        serde_json::Value::Bool(_) => "a boolean",
        serde_json::Value::Number(_) => "a number",
        serde_json::Value::String(_) => "a string",
        serde_json::Value::Array(_) => "an array",
        serde_json::Value::Object(_) => "an object",
    }
}

fn names_by_key(resources: &[RequiredResource]) -> BTreeMap<String, String> {
    resources
        .iter()
        .map(|resource| (resource.key(), resource.name.clone()))
        .collect()
}

fn read_wrangler_toml(artifact_dir: &Path) -> Result<String, ArtifactError> {
    let path = artifact_dir.join(WRANGLER_TOML_FILE);
    std::fs::read_to_string(&path).map_err(|error| ArtifactError::Missing {
        file: path.display().to_string(),
        message: error.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serving_version_reads_the_full_traffic_version() {
        let status = serde_json::json!({
            "id": "dep-1",
            "versions": [
                {"version_id": "old", "percentage": 0},
                {"version_id": "new", "percentage": 100}
            ]
        });
        assert_eq!(serving_version(&status).as_deref(), Some("new"));
        let split = serde_json::json!({
            "versions": [
                {"version_id": "a", "percentage": 50},
                {"version_id": "b", "percentage": 50}
            ]
        });
        assert_eq!(serving_version(&split), None);
    }
}
