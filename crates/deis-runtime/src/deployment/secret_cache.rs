//! On-demand secret value cache.
//!
//! Secrets no longer travel in the deployment message itself.  A node
//! receives the snapshot (which tells it *which* revisions are needed) and
//! then fetches the plaintext values through `FetchSecretValues` on the
//! machine channel.
//!
//! This cache holds the fetched values, keyed by
//! `"{deployment_id}\x00{revision}"`, and is consulted before an isolate
//! is built.  A deployment whose secrets are not yet cached is refused
//! rather than started with missing values.

use std::collections::HashMap;
use tonic::transport::Channel;

use deis_identity::SpiffeId;
use deis_wire::conventions::SecretFetchEntry;
use deis_wire::machine::machine_channel_client::MachineChannelClient;
use deis_wire::machine::FetchSecretValuesRequest;

/// On-demand cache of secret values fetched from the control plane.
///
/// This is a dumb cache: no expiry, no refresh, no background worker.
/// Secrets are immutable by revision, so a value never needs to be
/// re-fetched once it is here.  If the deployment changes, a new snapshot
/// arrives with new revisions, and the cache is consulted again.
#[derive(Debug, Default)]
pub struct SecretCache {
    /// Key: `"{deployment_id}\x00{revision}"`, Value: plaintext secret.
    values: HashMap<String, String>,
}

impl SecretCache {
    /// Create an empty cache.
    pub fn new() -> Self {
        Self::default()
    }

    /// Get a cached secret value, if it has already been fetched.
    pub fn get(&self, deployment_id: &str, revision: &str) -> Option<&str> {
        let key = secret_key(deployment_id, revision);
        self.values.get(&key).map(String::as_str)
    }

    /// Fetch secret values for the given deployment ids from the control
    /// plane and store them in the cache.
    ///
    /// The `entries` list tells the control plane which deployments to look
    /// up; each entry carries the deployment id and the set of revisions
    /// (use an empty vec to fetch all known secrets for that deployment).
    ///
    /// Returns the number of values successfully loaded.
    pub async fn fetch(
        &mut self,
        client: &MachineChannelClient<Channel>,
        identity: &SpiffeId,
        node_id: &str,
        entries: Vec<SecretFetchEntry>,
    ) -> Result<usize, tonic::Status> {
        let entries_json = serde_json::to_string(&entries).map_err(|error| {
            tonic::Status::internal(format!("could not serialise secret fetch entries: {error}"))
        })?;

        let request = tonic::Request::new(FetchSecretValuesRequest {
            node_id: node_id.to_string(),
            entries_json,
        });

        let response = client
            .clone()
            .fetch_secret_values(request)
            .await?
            .into_inner();

        let fetched: HashMap<String, String> = serde_json::from_str(&response.values_json)
            .map_err(|error| {
                tonic::Status::internal(format!("could not parse secret fetch response: {error}"))
            })?;

        let count = fetched.len();
        self.values.extend(fetched);
        let _ = identity; // Reserved for future audit logging.
        Ok(count)
    }

    /// Check that every revision referenced by `snapshot` is present in
    /// the cache for the given `deployment_id`.  Returns `Ok(())` if all
    /// are present, or `Err(missing_revisions)` otherwise.
    pub fn check_ready(
        &self,
        deployment_id: &str,
        snapshot: &deis_wire::EnvironmentBindingSnapshot,
    ) -> Result<(), Vec<String>> {
        let missing: Vec<String> = snapshot
            .bindings
            .iter()
            .filter_map(|binding| match binding {
                deis_wire::EnvironmentBinding::Secret(secret) => {
                    let revision = &secret.revision;
                    if self.get(deployment_id, revision).is_none() {
                        Some(revision.clone())
                    } else {
                        None
                    }
                }
                deis_wire::EnvironmentBinding::Config(_) => None,
            })
            .collect();
        if missing.is_empty() {
            Ok(())
        } else {
            Err(missing)
        }
    }

    /// Populate the `StandardEnvironmentOrder` with secrets from this
    /// cache, validating that all required revisions are present.
    pub fn populate_order(
        &self,
        deployment_id: &str,
        order: super::StandardEnvironmentOrder,
    ) -> Result<super::StandardEnvironmentOrder, String> {
        let secrets: HashMap<String, String> = order
            .snapshot
            .bindings
            .iter()
            .filter_map(|binding| match binding {
                deis_wire::EnvironmentBinding::Secret(secret) => {
                    let revision = &secret.revision;
                    self.get(deployment_id, revision)
                        .map(|value| (revision.clone(), value.to_string()))
                }
                deis_wire::EnvironmentBinding::Config(_) => None,
            })
            .collect();

        let expected_count = order
            .snapshot
            .bindings
            .iter()
            .filter(|binding| matches!(binding, deis_wire::EnvironmentBinding::Secret(_)))
            .count();

        if secrets.len() != expected_count {
            return Err(
                "EC_ENV_SECRET_REVISION_UNAVAILABLE: not all secret revisions are cached"
                    .to_string(),
            );
        }

        order.with_secrets(secrets)
    }

    /// Number of cached entries.
    pub fn len(&self) -> usize {
        self.values.len()
    }

    /// Whether the cache is empty.
    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }
}

fn secret_key(deployment_id: &str, revision: &str) -> String {
    format!("{deployment_id}\x00{revision}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_cache_has_nothing() {
        let cache = SecretCache::new();
        assert!(cache.is_empty());
        assert_eq!(cache.len(), 0);
        assert!(cache.get("d1", "r1").is_none());
    }

    #[test]
    fn a_manually_inserted_value_is_found() {
        let mut cache = SecretCache::new();
        let key = secret_key("d1", "r1");
        cache.values.insert(key, "s3cr3t".to_string());
        assert_eq!(cache.get("d1", "r1"), Some("s3cr3t"));
    }
}
