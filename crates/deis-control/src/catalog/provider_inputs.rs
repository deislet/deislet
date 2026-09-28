//! What a provider driver process is handed besides its request: the
//! deployment's config values, its secret values and, for an undeploy the
//! operator consented to, permission to destroy the environment's data
//! (`purgeData` on the workflow, handed over as consent naming exactly the
//! workflow's own environment).
//!
//! The values come from the environment snapshot the workflow's plan binds
//! (`bindingSnapshotDigest`) and from nowhere else. A snapshot names secret
//! *revisions*, so a secret rotated after the deployment was begun is not
//! what the driver receives: the driver deploys exactly what the desired
//! state - and every receipt bound to it - says.
//!
//! Secret values follow the rules `FetchSecretValues` follows for a node:
//! only the revisions the snapshot binds are decrypted, only for the one
//! step that sends them to the provider (`upload`), and only once the audit
//! row saying they left this process is durable. An audit log that cannot
//! be written means no values and a retryable failure, never values and a
//! log line. The launcher passes them to the driver in its environment,
//! never on argv; see [`deis_provider_driver::OperationInputSource`].

use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;
use deis_provider_driver::{
    DriverExecutionContext, DriverOperation, OperationInputError, OperationInputSource,
    OperationInputs,
};
use deis_wire::EnvironmentBinding;
use tracing::{error, info, warn};
use zeroize::Zeroizing;

use super::provider_driver_operation::BoundEnvironmentSnapshot;
use super::store::CatalogStore;
use super::{CatalogError, SecretsBackend};
use crate::audit::{AuditRecord, AuditStore, Outcome as AuditOutcome};

/// The audit action of a secret delivery to a provider driver.
pub const PROVIDER_SECRET_DELIVERY_ACTION: &str = "secret.provider-delivery";

/// The catalog's [`OperationInputSource`].
pub struct CatalogOperationInputs {
    store: CatalogStore,
    secrets: Arc<SecretsBackend>,
    audit: Arc<AuditStore>,
}

impl CatalogOperationInputs {
    #[must_use]
    pub fn new(store: CatalogStore, secrets: Arc<SecretsBackend>, audit: Arc<AuditStore>) -> Self {
        Self {
            store,
            secrets,
            audit,
        }
    }
}

/// The one step that sends secret values to the provider. A reconcile of an
/// upload only looks for the version the upload left, and a retry of it is a
/// new `upload` attempt that is given them again.
const fn sends_secrets(operation: DriverOperation) -> bool {
    matches!(operation, DriverOperation::Upload)
}

#[async_trait]
impl OperationInputSource for CatalogOperationInputs {
    async fn inputs(
        &self,
        context: &DriverExecutionContext<'_>,
    ) -> Result<OperationInputs, OperationInputError> {
        let request = context.request();
        let facts = self
            .store
            .provider_step_input_facts(request.operation_id)
            .await
            .map_err(|error| catalog_input_error(&error))?;
        let Some(facts) = facts else {
            // A driver operation begun on its own, outside any workflow,
            // binds no snapshot and carries no consent.
            return Ok(OperationInputs::none());
        };
        if facts.application_id != request.application_id
            || facts.environment_id != request.environment_id
        {
            return Err(OperationInputError::invalid(
                "the driver request names another application or environment than its workflow",
            ));
        }
        let mut inputs = OperationInputs::none();
        if let Some(bound) = &facts.snapshot {
            // Config is not secret, and every step that renders the
            // provider configuration - plan, upload, activate and a
            // reconcile of either - needs all of it.
            inputs = inputs.with_config(config_values(bound)?);
            if sends_secrets(request.operation) {
                let secrets = self
                    .deliver_secrets(bound, facts.workflow_operation_id, request.operation_id)
                    .await?;
                inputs = inputs.with_secrets(secrets);
            }
        }
        if facts.purge_data {
            // Only an undeploy the operator consented to carries this, and
            // the consent names the workflow's own environment: a driver
            // destroys nothing of any other.
            info!(
                operation_id = %facts.workflow_operation_id,
                child_operation_id = %request.operation_id,
                environment_id = %facts.environment_id,
                "Handing a provider driver the operator's consent to destroy this environment's provider data"
            );
            inputs = inputs.with_purge(facts.environment_id);
        }
        Ok(inputs)
    }
}

impl CatalogOperationInputs {
    /// Decrypt exactly the revisions `bound` names, and write down that
    /// they are leaving before handing them over.
    async fn deliver_secrets(
        &self,
        bound: &BoundEnvironmentSnapshot,
        workflow_operation_id: deis_wire::CanonicalId,
        child_operation_id: deis_wire::CanonicalId,
    ) -> Result<BTreeMap<String, Zeroizing<String>>, OperationInputError> {
        let mut values = BTreeMap::new();
        let mut revisions = Vec::new();
        for binding in &bound.snapshot.bindings {
            let EnvironmentBinding::Secret(secret) = binding else {
                continue;
            };
            let decrypted = async {
                let stored = self
                    .store
                    .secret_revision_ciphertext(&bound.env_id, &secret.name, &secret.revision)
                    .await?;
                crate::delivery::decrypt_secret(self.secrets.as_ref(), &bound.env_id, &stored)
            }
            .await;
            match decrypted {
                Ok(value) => {
                    revisions.push(secret.revision.clone());
                    values.insert(secret.name.clone(), Zeroizing::new(value));
                }
                Err(failure) => {
                    let message = format!(
                        "secret {} revision {} bound by environment snapshot {} could not be read: {failure}",
                        secret.name, secret.revision, bound.snapshot_id
                    );
                    self.audit
                        .record(delivery_record(
                            bound,
                            workflow_operation_id,
                            AuditOutcome::Failed,
                            500,
                            format!("operation={child_operation_id} {message}"),
                        ))
                        .await;
                    warn!(
                        operation_id = %workflow_operation_id,
                        child_operation_id = %child_operation_id,
                        secret = %secret.name,
                        revision = %secret.revision,
                        "Refused to start a provider upload whose bound secret cannot be read"
                    );
                    return Err(catalog_input_error(&CatalogError::invalid(message)));
                }
            }
        }
        if values.is_empty() {
            return Ok(values);
        }

        // Fail closed, as `FetchSecretValues` does: no value leaves this
        // process until the row saying it did is durable.
        let record = delivery_record(
            bound,
            workflow_operation_id,
            AuditOutcome::Accepted,
            200,
            format!(
                "operation={child_operation_id} snapshot={} revisions={} count={}",
                bound.snapshot_id,
                revisions.join(","),
                values.len()
            ),
        );
        if let Err(failure) = self.audit.try_record(record).await {
            error!(
                operation_id = %workflow_operation_id,
                child_operation_id = %child_operation_id,
                error = %failure,
                "Withheld secret values from a provider driver because the audit row for the delivery could not be written"
            );
            return Err(OperationInputError::unavailable(
                "the audit record for this secret delivery could not be written, so no value is handed to the driver; the step is retried once the control plane's database accepts writes",
            ));
        }
        info!(
            operation_id = %workflow_operation_id,
            child_operation_id = %child_operation_id,
            snapshot_id = %bound.snapshot_id,
            count = values.len(),
            "Handing the secret values an environment snapshot binds to a provider driver"
        );
        Ok(values)
    }
}

/// The snapshot's config values as the strings a provider's configuration
/// holds: a `string` value as itself, a `json` value as its compact JSON.
fn config_values(
    bound: &BoundEnvironmentSnapshot,
) -> Result<BTreeMap<String, String>, OperationInputError> {
    let mut config = BTreeMap::new();
    for binding in &bound.snapshot.bindings {
        let EnvironmentBinding::Config(value) = binding else {
            continue;
        };
        let text = match &value.value {
            serde_json::Value::String(text) => text.clone(),
            other => serde_json::to_string(other).map_err(|error| {
                OperationInputError::invalid(format!(
                    "config value {} could not be encoded: {error}",
                    value.name
                ))
            })?,
        };
        config.insert(value.name.clone(), text);
    }
    Ok(config)
}

fn delivery_record(
    bound: &BoundEnvironmentSnapshot,
    workflow_operation_id: deis_wire::CanonicalId,
    outcome: AuditOutcome,
    status: u16,
    detail: String,
) -> AuditRecord {
    AuditRecord {
        actor_kind: "provider-driver".to_string(),
        actor_account: None,
        actor_credential: None,
        action: PROVIDER_SECRET_DELIVERY_ACTION.to_string(),
        app_id: Some(bound.app_id.clone()),
        environment: Some(bound.env_id.clone()),
        target: Some(workflow_operation_id.to_string()),
        method: "DRIVER".to_string(),
        path: "provider-driver/upload".to_string(),
        peer: None,
        outcome,
        status,
        detail: Some(detail),
        request_id: None,
    }
}

fn catalog_input_error(error: &CatalogError) -> OperationInputError {
    match error {
        CatalogError::Database(_) | CatalogError::Backend(_) => {
            OperationInputError::unavailable(error.to_string())
        }
        _ => OperationInputError::invalid(error.to_string()),
    }
}
