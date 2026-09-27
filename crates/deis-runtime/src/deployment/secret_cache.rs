//! On-demand secret values.
//!
//! Secrets no longer travel in the deployment message itself. A node
//! receives the snapshot, which says *which* revisions a deployment binds,
//! and fetches the plaintext values through `FetchSecretValues` on the
//! machine channel right before it builds an isolate for that deployment.
//!
//! [`SecretSource`] is that fetch plus a cache in front of it, keyed by
//! deployment and revision. A revision is immutable, so a cached value never
//! goes stale; what bounds the cache is the assignment list, which evicts
//! every deployment this node is no longer told to run
//! ([`SecretSource::retain`]). A deployment whose values cannot be fetched is
//! refused rather than started with holes in its environment, and the next
//! reconcile pass asks again.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::sync::Mutex;

use deis_wire::conventions::SecretFetchEntry;
use deis_wire::machine::machine_channel_client::MachineChannelClient;
use deis_wire::machine::FetchSecretValuesRequest;
use tonic::transport::Channel;
use tracing::{debug, warn};

use super::StandardEnvironmentOrder;

/// Fetched secret values: deployment id, then revision, then plaintext.
///
/// No expiry and no refresh: revisions are immutable, and a rotation arrives
/// as a new snapshot naming a new revision.
#[derive(Default)]
struct SecretCache {
    values: HashMap<String, HashMap<String, String>>,
}

impl SecretCache {
    fn get(&self, deployment_id: &str, revision: &str) -> Option<&str> {
        self.values
            .get(deployment_id)?
            .get(revision)
            .map(String::as_str)
    }

    /// The revisions `order` binds that are not cached for `deployment_id`.
    fn missing(&self, deployment_id: &str, order: &StandardEnvironmentOrder) -> Vec<String> {
        secret_revisions(order)
            .into_iter()
            .filter(|revision| self.get(deployment_id, revision).is_none())
            .collect()
    }

    fn insert(&mut self, deployment_id: &str, revision: String, value: String) {
        self.values
            .entry(deployment_id.to_string())
            .or_default()
            .insert(revision, value);
    }

    /// Hand `order` every value it binds, or say that some are missing.
    fn populate_order(
        &self,
        deployment_id: &str,
        order: StandardEnvironmentOrder,
    ) -> Result<StandardEnvironmentOrder, String> {
        let mut secrets = HashMap::new();
        for revision in secret_revisions(&order) {
            let value = self.get(deployment_id, &revision).ok_or_else(|| {
                format!(
                    "EC_ENV_SECRET_REVISION_UNAVAILABLE: the control plane did not return secret \
                     revision {revision} of deployment {deployment_id}"
                )
            })?;
            secrets.insert(revision, value.to_string());
        }
        order.with_secrets(secrets)
    }

    fn retain(&mut self, deployment_ids: &HashSet<String>) -> usize {
        let before = self.values.len();
        self.values.retain(|id, _| deployment_ids.contains(id));
        before - self.values.len()
    }
}

/// Every secret revision a standard-environment order binds.
fn secret_revisions(order: &StandardEnvironmentOrder) -> BTreeSet<String> {
    order
        .snapshot
        .bindings
        .iter()
        .filter_map(|binding| match binding {
            deis_wire::EnvironmentBinding::Secret(secret) => Some(secret.revision.clone()),
            deis_wire::EnvironmentBinding::Config(_) => None,
        })
        .collect()
}

/// Fetches secret values over the machine channel, and remembers them.
pub struct SecretSource {
    /// A handle on the same channel the assignment client dials; cloning it
    /// shares the connection rather than opening another.
    client: MachineChannelClient<Channel>,
    /// The name this node asks as. The control plane checks it against the
    /// certificate and answers only for deployments assigned to that name.
    node_id: String,
    /// Held only between awaits, never across the RPC.
    cache: Mutex<SecretCache>,
}

impl SecretSource {
    pub fn new(client: MachineChannelClient<Channel>, node_id: impl Into<String>) -> Self {
        Self {
            client,
            node_id: node_id.into(),
            cache: Mutex::new(SecretCache::default()),
        }
    }

    fn cache(&self) -> std::sync::MutexGuard<'_, SecretCache> {
        // A panic while holding this lock cannot leave a half-written entry
        // that matters: an entry is one value, inserted whole.
        self.cache
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Fill in every secret value `order` binds, fetching what is not cached.
    ///
    /// `deployment_id` is what the values are fetched and cached under; an
    /// order that binds secrets and carries no deployment id cannot be
    /// resolved, because the control plane authorises a fetch per deployment.
    pub async fn resolve(
        &self,
        deployment_id: Option<&str>,
        order: StandardEnvironmentOrder,
    ) -> Result<StandardEnvironmentOrder, String> {
        if secret_revisions(&order).is_empty() {
            return Ok(order);
        }
        let deployment_id = deployment_id.ok_or_else(|| {
            "EC_ENV_SECRET_REVISION_UNAVAILABLE: this deployment binds secrets but carries no \
             deployment id to fetch them under"
                .to_string()
        })?;

        let missing = self.cache().missing(deployment_id, &order);
        if !missing.is_empty() {
            self.fetch(deployment_id, missing).await?;
        }
        self.cache().populate_order(deployment_id, order)
    }

    async fn fetch(&self, deployment_id: &str, revisions: Vec<String>) -> Result<(), String> {
        let requested: HashSet<String> = revisions.iter().cloned().collect();
        let entries_json = serde_json::to_string(&[SecretFetchEntry {
            deployment_id: deployment_id.to_string(),
            revisions,
        }])
        .map_err(|error| format!("could not encode the secret fetch request: {error}"))?;

        let answer = self
            .client
            .clone()
            .fetch_secret_values(FetchSecretValuesRequest {
                node_id: self.node_id.clone(),
                entries_json,
            })
            .await
            .map_err(|status| {
                warn!(
                    deployment_id = %deployment_id,
                    code = ?status.code(),
                    message = status.message(),
                    "Could not fetch this deployment's secret values from the control plane; it \
                     is not started here, and the next reconcile pass asks again"
                );
                format!(
                    "EC_ENV_SECRET_REVISION_UNAVAILABLE: the control plane did not hand over \
                     this deployment's secret values ({:?}: {})",
                    status.code(),
                    status.message()
                )
            })?
            .into_inner();

        let fetched: HashMap<String, String> = serde_json::from_str(&answer.values_json)
            .map_err(|error| format!("could not parse the secret fetch answer: {error}"))?;

        let mut cache = self.cache();
        let mut stored = 0usize;
        for (key, value) in fetched {
            // Only what was asked for is kept. Anything else in the answer is
            // a control plane that disagrees with this node about the
            // request, and holding it would only widen what this node keeps.
            let Some((id, revision)) = key.split_once('\x00') else {
                continue;
            };
            if id == deployment_id && requested.contains(revision) {
                cache.insert(deployment_id, revision.to_string(), value);
                stored += 1;
            }
        }
        debug!(
            deployment_id = %deployment_id,
            requested = requested.len(),
            stored,
            "Fetched secret values over the machine channel"
        );
        Ok(())
    }

    /// Forget every deployment not in `deployment_ids`.
    ///
    /// Called with the deployments of a complete assignment list, so the
    /// cache never holds more than what this node is told to run.
    pub fn retain(&self, deployment_ids: &HashSet<String>) {
        let evicted = self.cache().retain(deployment_ids);
        if evicted > 0 {
            debug!(
                evicted,
                "Dropped the cached secret values of deployments no longer assigned here"
            );
        }
    }

    /// How many deployments have cached values. For tests.
    #[cfg(test)]
    pub(crate) fn cached_deployments(&self) -> usize {
        self.cache().values.len()
    }

    #[cfg(test)]
    pub(crate) fn insert_for_test(&self, deployment_id: &str, revision: &str, value: &str) {
        self.cache()
            .insert(deployment_id, revision.to_string(), value.to_string());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retain_drops_only_deployments_not_named() {
        let mut cache = SecretCache::default();
        cache.insert("d1", "r1".to_string(), "one".to_string());
        cache.insert("d2", "r2".to_string(), "two".to_string());
        let keep = HashSet::from(["d1".to_string()]);
        assert_eq!(cache.retain(&keep), 1);
        assert_eq!(cache.get("d1", "r1"), Some("one"));
        assert!(cache.get("d2", "r2").is_none());
    }

    #[test]
    fn values_are_scoped_to_their_deployment() {
        let mut cache = SecretCache::default();
        cache.insert("d1", "r1".to_string(), "one".to_string());
        assert!(cache.get("d2", "r1").is_none());
    }
}
