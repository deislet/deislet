//! Verified, host-specific provider driver releases.
//!
//! A manifest digest is only a claim about intended bytes. This registry is
//! the durable bridge from that claim to one complete installation tree that
//! the current Control host has actually verified.

use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
    sync::Arc,
};

use chrono::{DateTime, Utc};
use deis_provider_driver::{
    DriverInstallationError, DriverReleaseInstaller, DriverWorkspaceRoot, ExecutableProviderDriver,
    ExecutableProviderDriverConfig, HostOperatingSystem, InstalledDriverRelease,
    OperationInputSource, ProviderCredentialBroker, Sha256Digest,
    ValidatedDriverReleaseAuthorization, ValidatedDriverReleaseTransparencyProof,
    ValidatedDriverReleaseTrustRoot, ValidatedDriverTrustRootRotation,
    ValidatedProviderDriverManifest, ValidatedProviderDriverRelease,
    VerifiedDriverReleaseAuthorization, VerifiedDriverReleaseTransparencyProof,
    VerifiedDriverTrustRootRotation,
};
use deis_wire::CanonicalId;
use rusqlite::types::Value;
use rusqlite::{OptionalExtension, params, params_from_iter};
use serde::Serialize;
use sha2::{Digest as _, Sha256};
use utoipa::ToSchema;

use super::model::now_secs;
use super::pagination::{
    CatalogPage, KeysetDirection, KeysetResume, PagePosition, finish_page, sqlite_fetch_limit,
};
use super::store::CatalogStore;
use super::{CatalogError, CatalogProviderCredentialVault, CatalogResult};

#[derive(Clone, Debug, Eq, PartialEq, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ProviderDriverReleaseRecord {
    #[schema(value_type = String)]
    pub release_digest: Sha256Digest,
    #[schema(value_type = String)]
    pub host_operating_system: HostOperatingSystem,
    pub driver_contract_version: String,
    pub backend_id: String,
    pub driver_id: String,
    pub driver_version: String,
    #[schema(value_type = String)]
    pub manifest_digest: Sha256Digest,
    #[schema(value_type = String)]
    pub archive_digest: Sha256Digest,
    pub archive_size_bytes: u64,
    #[schema(value_type = String)]
    pub tree_digest: Sha256Digest,
    pub installation_dir_name: String,
    pub status: String,
    pub revision: u64,
    pub verified_at: DateTime<Utc>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ProviderDriverReleaseAuditEvent {
    #[schema(value_type = String, format = Uuid)]
    pub event_id: CanonicalId,
    #[schema(value_type = String)]
    pub release_digest: Sha256Digest,
    #[schema(value_type = String)]
    pub host_operating_system: HostOperatingSystem,
    pub event_type: String,
    pub detail: String,
    pub observed_at: DateTime<Utc>,
}

#[derive(Clone)]
pub struct ProviderDriverReleaseRegistry {
    store: CatalogStore,
    installer: DriverReleaseInstaller,
    host: HostOperatingSystem,
    trust_root: Arc<ValidatedDriverReleaseTrustRoot>,
}

/// How long the Control lets one driver process run before killing it.
///
/// The process holds one credential lease for its whole run, and the vault
/// grants at most five minutes
/// (`provider_credential::MAX_CREDENTIAL_LEASE`); the launcher
/// refuses to start a process whose lease cannot outlast its timeout. A
/// provider operation that uploads a Worker or installs its pinned toolchain
/// on first use can take minutes, so the budget sits just under the lease.
/// Drivers keep their own budget below this so they can still report a
/// retryable timeout outcome.
pub const DRIVER_PROCESS_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(270);

/// The launcher bounds the Control runs provider driver processes with.
#[must_use]
pub fn driver_process_config() -> ExecutableProviderDriverConfig {
    ExecutableProviderDriverConfig {
        credential_lease_duration: super::provider_credential::MAX_CREDENTIAL_LEASE,
        process_timeout: DRIVER_PROCESS_TIMEOUT,
        ..ExecutableProviderDriverConfig::default()
    }
}

#[derive(Clone)]
pub struct ProviderDriverDispatcher {
    registry: ProviderDriverReleaseRegistry,
    credential_broker: Arc<dyn ProviderCredentialBroker>,
    /// Each operation's config values, secret values and purge consent.
    inputs: Arc<dyn OperationInputSource>,
    /// Where every driver process gets its verified payload and state.
    workspaces: DriverWorkspaceRoot,
    process_config: ExecutableProviderDriverConfig,
}

#[derive(Clone, Debug)]
pub struct ResolvedProviderDriverRelease {
    pub record: ProviderDriverReleaseRecord,
    pub manifest: ValidatedProviderDriverManifest,
    pub release: ValidatedProviderDriverRelease,
    pub installed: InstalledDriverRelease,
}

struct VerifiedReleaseTrustMaterial<'a> {
    authorization: &'a ValidatedDriverReleaseAuthorization,
    verified_authorization: &'a VerifiedDriverReleaseAuthorization,
    transparency: &'a ValidatedDriverReleaseTransparencyProof,
    verified_transparency: &'a VerifiedDriverReleaseTransparencyProof,
}

impl ProviderDriverReleaseRegistry {
    pub async fn new(
        store: CatalogStore,
        installation_root: impl AsRef<Path>,
        trust_root: ValidatedDriverReleaseTrustRoot,
        trust_root_rotation: Option<ValidatedDriverTrustRootRotation>,
    ) -> CatalogResult<Self> {
        let now = Utc::now();
        if trust_root.get().expires_at <= now {
            return Err(CatalogError::invalid(
                "provider driver release trust root has expired",
            ));
        }
        if !trust_root.root_role_satisfies_threshold_at(now) {
            return Err(CatalogError::invalid(
                "provider driver release trust root has no currently usable root-role threshold",
            ));
        }
        let host = HostOperatingSystem::current().ok_or_else(|| {
            CatalogError::invalid(
                "this Control host is not one of the provider driver release contract targets",
            )
        })?;
        let installer = DriverReleaseInstaller::new(installation_root).map_err(|error| {
            CatalogError::invalid(format!(
                "invalid provider driver installation root: {error}"
            ))
        })?;
        let registry = Self {
            store,
            installer,
            host,
            trust_root: Arc::new(trust_root),
        };
        registry.record_trust_root(trust_root_rotation).await?;
        Ok(registry)
    }

    #[must_use]
    pub const fn host(&self) -> HostOperatingSystem {
        self.host
    }

    #[must_use]
    pub fn installation_root(&self) -> &Path {
        self.installer.root()
    }

    #[must_use]
    pub fn trust_root(&self) -> &ValidatedDriverReleaseTrustRoot {
        &self.trust_root
    }

    #[must_use]
    pub fn belongs_to(&self, store: &CatalogStore) -> bool {
        Arc::ptr_eq(&self.store.database(), &store.database())
    }

    async fn record_trust_root(
        &self,
        trust_root_rotation: Option<ValidatedDriverTrustRootRotation>,
    ) -> CatalogResult<()> {
        let trust_root = self.trust_root.as_ref().clone();
        let root_version = i64::try_from(trust_root.get().root_version).map_err(|_| {
            CatalogError::invalid("provider driver trust root version exceeds SQLite range")
        })?;
        let root_digest = trust_root.canonical_sha256().to_string();
        let root_json = String::from_utf8(trust_root.canonical_json().map_err(|error| {
            CatalogError::invalid(format!(
                "cannot canonicalize provider driver trust root: {error}"
            ))
        })?)
        .expect("JCS JSON is UTF-8");
        let expires_at = trust_root.get().expires_at.timestamp();
        let database = self.store.database();
        database
            .with_connection(move |connection| {
                let transaction = connection.unchecked_transaction()?;
                let highest = transaction
                    .query_row(
                        "SELECT root_version, root_sha256, root_json, expires_at
                           FROM provider_driver_trust_roots
                          ORDER BY root_version DESC LIMIT 1",
                        [],
                        |row| {
                            Ok((
                                row.get::<_, i64>(0)?,
                                row.get::<_, String>(1)?,
                                row.get::<_, String>(2)?,
                                row.get::<_, i64>(3)?,
                            ))
                        },
                    )
                    .optional()?;
                let now = now_secs();
                match highest {
                    None => {
                        if trust_root_rotation.is_some() {
                            return Err(CatalogError::conflict(
                                "provider driver trust root bootstrap cannot use an unverifiable rotation envelope",
                            ));
                        }
                        insert_trust_root(
                            &transaction,
                            root_version,
                            &root_digest,
                            &root_json,
                            expires_at,
                            now,
                        )?;
                    }
                    Some((highest_version, highest_digest, highest_json, highest_expires_at))
                        if root_version == highest_version =>
                    {
                        if (highest_digest, highest_json, highest_expires_at)
                            != (root_digest.clone(), root_json.clone(), expires_at)
                        {
                            return Err(CatalogError::conflict(
                                "provider driver trust root version has conflicting canonical content",
                            ));
                        }
                        transaction.execute(
                            "UPDATE provider_driver_trust_roots SET last_seen_at = MAX(last_seen_at, ?2)
                              WHERE root_version = ?1",
                            params![root_version, now],
                        )?;
                    }
                    Some((highest_version, _, _, _)) if root_version < highest_version => {
                        return Err(CatalogError::conflict(
                            "provider driver trust root version would roll durable trust state backward",
                        ));
                    }
                    Some((highest_version, _, highest_json, _)) => {
                        let expected = highest_version.checked_add(1).ok_or_else(|| {
                            CatalogError::invalid(
                                "provider driver trust root version exceeds SQLite range",
                            )
                        })?;
                        if root_version != expected {
                            return Err(CatalogError::conflict(
                                "provider driver trust root versions must advance exactly once",
                            ));
                        }
                        let previous_root = ValidatedDriverReleaseTrustRoot::parse_json(
                            highest_json.as_bytes(),
                        )
                        .map_err(|error| {
                            CatalogError::invalid(format!(
                                "invalid durable provider driver trust root: {error}"
                            ))
                        })?;
                        let rotation = trust_root_rotation.as_ref().ok_or_else(|| {
                            CatalogError::conflict(
                                "provider driver trust root update requires an old-root/new-root rotation envelope",
                            )
                        })?;
                        if rotation.next_root().canonical_sha256()
                            != trust_root.canonical_sha256()
                        {
                            return Err(CatalogError::conflict(
                                "provider driver trust root rotation does not carry the configured next root",
                            ));
                        }
                        let observed_at = DateTime::from_timestamp(now, 0).ok_or_else(|| {
                            CatalogError::invalid("provider driver trust root observation time is invalid")
                        })?;
                        let verified_rotation = rotation
                            .verify(&previous_root, observed_at)
                            .map_err(trust_root_rotation_error)?;
                        let historical_root_json = {
                            let mut statement = transaction.prepare(
                                "SELECT root_json FROM provider_driver_trust_roots
                                  ORDER BY root_version",
                            )?;
                            statement
                                .query_map([], |row| row.get::<_, String>(0))?
                                .collect::<Result<Vec<_>, _>>()?
                        };
                        validate_trust_root_transition(
                            &historical_root_json,
                            &previous_root,
                            &trust_root,
                        )?;
                        insert_trust_root(
                            &transaction,
                            root_version,
                            &root_digest,
                            &root_json,
                            expires_at,
                            now,
                        )?;
                        persist_trust_root_rotation(
                            &transaction,
                            rotation,
                            &verified_rotation,
                            now,
                        )?;
                    }
                }
                transaction.commit()?;
                Ok(())
            })
            .await
    }

    pub async fn release(
        &self,
        release_digest: Sha256Digest,
    ) -> CatalogResult<Option<ProviderDriverReleaseRecord>> {
        let database = self.store.database();
        let host = self.host;
        database
            .with_connection(move |connection| {
                load_release_record(connection, &release_digest, host)
            })
            .await
    }

    pub async fn releases(&self, limit: usize) -> CatalogResult<Vec<ProviderDriverReleaseRecord>> {
        Ok(self.releases_page(limit, None).await?.items)
    }

    pub async fn releases_page(
        &self,
        limit: usize,
        cursor: Option<PagePosition>,
    ) -> CatalogResult<CatalogPage<ProviderDriverReleaseRecord>> {
        let fetch_limit = sqlite_fetch_limit(limit, "provider driver release")?;
        let database = self.store.database();
        let host = self.host;
        database
            .with_connection(move |connection| {
                let transaction = connection.unchecked_transaction()?;
                let total = transaction.query_row(
                    "SELECT COUNT(*) FROM provider_driver_releases
                      WHERE host_operating_system = ?1",
                    [host.as_str()],
                    |row| row.get::<_, i64>(0),
                )?;
                let resume = KeysetResume::value_and_identity(
                    2,
                    KeysetDirection::Descending,
                    "created_at",
                    "release_sha256",
                    cursor,
                );
                let mut statement = transaction.prepare(&releases_page_sql(&resume))?;
                let digests = statement
                    .query_map(
                        params_from_iter(
                            resume.bindings([Value::from(host.as_str().to_owned())], fetch_limit),
                        ),
                        |row| row.get::<_, String>(0),
                    )?
                    .collect::<Result<Vec<_>, _>>()?;
                let releases = digests
                    .into_iter()
                    .map(|digest| {
                        let digest = parse_digest(&digest, "provider driver release")?;
                        load_release_record(&transaction, &digest, host)?.ok_or_else(|| {
                            CatalogError::not_found(format!(
                                "provider driver release {digest} for {host}"
                            ))
                        })
                    })
                    .collect::<CatalogResult<Vec<_>>>()?;
                drop(statement);
                transaction.commit()?;
                finish_page(releases, limit, total, "provider driver release")
            })
            .await
    }

    pub async fn audit_events(
        &self,
        release_digest: Sha256Digest,
        limit: usize,
    ) -> CatalogResult<Vec<ProviderDriverReleaseAuditEvent>> {
        Ok(self
            .audit_events_page(release_digest, limit, None)
            .await?
            .items)
    }

    pub async fn audit_events_page(
        &self,
        release_digest: Sha256Digest,
        limit: usize,
        cursor: Option<PagePosition>,
    ) -> CatalogResult<CatalogPage<ProviderDriverReleaseAuditEvent>> {
        let fetch_limit = sqlite_fetch_limit(limit, "provider driver release audit")?;
        let database = self.store.database();
        let host = self.host;
        database
            .with_connection(move |connection| {
                let transaction = connection.unchecked_transaction()?;
                let total = transaction.query_row(
                    "SELECT COUNT(*) FROM provider_driver_release_audit
                      WHERE release_sha256 = ?1 AND host_operating_system = ?2",
                    params![release_digest.as_str(), host.as_str()],
                    |row| row.get::<_, i64>(0),
                )?;
                let resume = KeysetResume::value_and_identity(
                    3,
                    KeysetDirection::Ascending,
                    "observed_at",
                    "event_id",
                    cursor,
                );
                let mut statement = transaction.prepare(&release_audit_events_page_sql(&resume))?;
                let events = statement
                    .query_map(
                        params_from_iter(resume.bindings(
                            [
                                Value::from(release_digest.as_str().to_owned()),
                                Value::from(host.as_str().to_owned()),
                            ],
                            fetch_limit,
                        )),
                        |row| {
                            Ok((
                                row.get::<_, String>(0)?,
                                row.get::<_, String>(1)?,
                                row.get::<_, String>(2)?,
                                row.get::<_, i64>(3)?,
                            ))
                        },
                    )?
                    .map(|row| {
                        let row = row?;
                        Ok(ProviderDriverReleaseAuditEvent {
                            event_id: parse_id(&row.0, "provider driver release audit")?,
                            release_digest: release_digest.clone(),
                            host_operating_system: host,
                            event_type: row.1,
                            detail: row.2,
                            observed_at: timestamp(row.3, "provider driver release audit time")?,
                        })
                    })
                    .collect::<CatalogResult<Vec<_>>>()?;
                drop(statement);
                transaction.commit()?;
                finish_page(events, limit, total, "provider driver release audit")
            })
            .await
    }

    pub async fn install_and_register(
        &self,
        release: ValidatedProviderDriverRelease,
        manifest: ValidatedProviderDriverManifest,
        authorization: ValidatedDriverReleaseAuthorization,
        transparency: ValidatedDriverReleaseTransparencyProof,
        archive_path: impl AsRef<Path>,
    ) -> CatalogResult<ProviderDriverReleaseRecord> {
        let now = Utc::now();
        let verified_authorization = authorization
            .verify(&release, &manifest, &self.trust_root, now)
            .map_err(authorization_error)?;
        let verified_transparency = transparency
            .verify(&authorization, &self.trust_root, now)
            .map_err(transparency_error)?;
        let installer = self.installer.clone();
        let host = self.host;
        let archive_path = archive_path.as_ref().to_path_buf();
        let release_for_install = release.clone();
        let manifest_for_install = manifest.clone();
        let installed = tokio::task::spawn_blocking(move || {
            installer.install(
                &release_for_install,
                &manifest_for_install,
                host,
                archive_path,
            )
        })
        .await
        .map_err(|error| CatalogError::invalid(format!("driver installer task failed: {error}")))?
        .map_err(installation_error)?;
        self.register_verified(
            &release,
            &manifest,
            VerifiedReleaseTrustMaterial {
                authorization: &authorization,
                verified_authorization: &verified_authorization,
                transparency: &transparency,
                verified_transparency: &verified_transparency,
            },
            &installed,
        )
        .await
    }

    /// Verify every small signed document before an HTTP caller is allowed
    /// to stream the much larger archive to disk. Installation repeats the
    /// same checks so elapsed time or a future call-site cannot weaken the
    /// actual publish boundary.
    pub fn validate_release_upload(
        &self,
        release: &ValidatedProviderDriverRelease,
        manifest: &ValidatedProviderDriverManifest,
        authorization: &ValidatedDriverReleaseAuthorization,
        transparency: &ValidatedDriverReleaseTransparencyProof,
    ) -> CatalogResult<u64> {
        release.bind_to_manifest(manifest).map_err(|error| {
            CatalogError::invalid(format!(
                "provider driver release does not match its manifest: {error}"
            ))
        })?;
        let now = Utc::now();
        authorization
            .verify(release, manifest, &self.trust_root, now)
            .map_err(authorization_error)?;
        transparency
            .verify(authorization, &self.trust_root, now)
            .map_err(transparency_error)?;
        release
            .distribution_for(self.host)
            .map(|distribution| distribution.archive.size_bytes)
            .ok_or_else(|| {
                CatalogError::invalid(format!(
                    "provider driver release has no archive for {}",
                    self.host
                ))
            })
    }

    pub async fn register_existing(
        &self,
        release: ValidatedProviderDriverRelease,
        manifest: ValidatedProviderDriverManifest,
        authorization: ValidatedDriverReleaseAuthorization,
        transparency: ValidatedDriverReleaseTransparencyProof,
    ) -> CatalogResult<ProviderDriverReleaseRecord> {
        let now = Utc::now();
        let verified_authorization = authorization
            .verify(&release, &manifest, &self.trust_root, now)
            .map_err(authorization_error)?;
        let verified_transparency = transparency
            .verify(&authorization, &self.trust_root, now)
            .map_err(transparency_error)?;
        let installer = self.installer.clone();
        let host = self.host;
        let release_for_verify = release.clone();
        let manifest_for_verify = manifest.clone();
        let installed = tokio::task::spawn_blocking(move || {
            installer.resolve(&release_for_verify, &manifest_for_verify, host)
        })
        .await
        .map_err(|error| CatalogError::invalid(format!("driver verifier task failed: {error}")))?
        .map_err(installation_error)?;
        self.register_verified(
            &release,
            &manifest,
            VerifiedReleaseTrustMaterial {
                authorization: &authorization,
                verified_authorization: &verified_authorization,
                transparency: &transparency,
                verified_transparency: &verified_transparency,
            },
            &installed,
        )
        .await
    }

    pub async fn resolve_for_target(
        &self,
        target_id: CanonicalId,
    ) -> CatalogResult<ResolvedProviderDriverRelease> {
        self.resolve_for_target_at(target_id, Utc::now()).await
    }

    async fn resolve_for_target_at(
        &self,
        target_id: CanonicalId,
        now: DateTime<Utc>,
    ) -> CatalogResult<ResolvedProviderDriverRelease> {
        let stored = load_release_for_target(&self.store, target_id, self.host).await?;
        let authorization_is_trusted = stored.verification_materials.iter().any(|material| {
            material
                .authorization
                .verify(&stored.release, &stored.manifest, &self.trust_root, now)
                .is_ok()
                && material
                    .transparency
                    .verify(&material.authorization, &self.trust_root, now)
                    .is_ok_and(|verified| {
                        verified.log_origins == material.log_origins
                            && verified.log_operators == material.log_operators
                            && verified.witness_operators == material.witness_operators
                            && verified.earliest_witnessed_at == material.earliest_witnessed_at
                            && verified.latest_witnessed_at == material.latest_witnessed_at
                    })
        });
        if !authorization_is_trusted {
            return Err(CatalogError::conflict(
                "registered provider driver release has no current transparent authorization under the active trust root",
            ));
        }
        let installer = self.installer.clone();
        let release_for_verify = stored.release.clone();
        let manifest_for_verify = stored.manifest.clone();
        let host = self.host;
        let verified = tokio::task::spawn_blocking(move || {
            installer.resolve(&release_for_verify, &manifest_for_verify, host)
        })
        .await
        .map_err(|error| CatalogError::invalid(format!("driver verifier task failed: {error}")))?;
        match verified {
            Ok(installed) => Ok(ResolvedProviderDriverRelease {
                record: stored.record,
                manifest: stored.manifest,
                release: stored.release,
                installed,
            }),
            Err(error) => {
                quarantine_release(
                    &self.store,
                    stored.record.release_digest.clone(),
                    self.host,
                    "full installation-tree verification failed before driver selection",
                )
                .await?;
                Err(CatalogError::conflict(format!(
                    "registered provider driver release failed full-tree verification: {error}"
                )))
            }
        }
    }

    async fn register_verified(
        &self,
        release: &ValidatedProviderDriverRelease,
        manifest: &ValidatedProviderDriverManifest,
        trust: VerifiedReleaseTrustMaterial<'_>,
        installed: &InstalledDriverRelease,
    ) -> CatalogResult<ProviderDriverReleaseRecord> {
        let installed_for_verify = installed.clone();
        tokio::task::spawn_blocking(move || installed_for_verify.verify())
            .await
            .map_err(|error| {
                CatalogError::invalid(format!("driver registration verifier task failed: {error}"))
            })?
            .map_err(installation_error)?;
        release.bind_to_manifest(manifest).map_err(|error| {
            CatalogError::invalid(format!(
                "driver release does not bind its manifest: {error}"
            ))
        })?;
        let release_digest = release.canonical_sha256().map_err(|error| {
            CatalogError::invalid(format!("cannot identify driver release: {error}"))
        })?;
        if installed.release_digest != release_digest
            || installed.host_operating_system != self.host
            || installed.distribution()
                != release.distribution_for(self.host).ok_or_else(|| {
                    CatalogError::invalid(
                        "driver release has no distribution for this Control host",
                    )
                })?
        {
            return Err(CatalogError::invalid(
                "verified installation does not match the release selected for registration",
            ));
        }
        let manifest_digest = manifest.canonical_sha256().map_err(|error| {
            CatalogError::invalid(format!("cannot identify driver manifest: {error}"))
        })?;
        let release_json = String::from_utf8(release.canonical_json().map_err(|error| {
            CatalogError::invalid(format!("cannot canonicalize driver release: {error}"))
        })?)
        .expect("JCS JSON is UTF-8");
        let manifest_json = String::from_utf8(manifest.canonical_json().map_err(|error| {
            CatalogError::invalid(format!("cannot canonicalize driver manifest: {error}"))
        })?)
        .expect("JCS JSON is UTF-8");
        let authorization_json =
            String::from_utf8(trust.authorization.canonical_json().map_err(|error| {
                CatalogError::invalid(format!(
                    "cannot canonicalize driver release authorization: {error}"
                ))
            })?)
            .expect("JCS JSON is UTF-8");
        let transparency_json =
            String::from_utf8(trust.transparency.canonical_json()).expect("JCS JSON is UTF-8");
        let distribution = installed.distribution();
        let expected_dir_name = format!("{}-{}", release_digest.as_str(), self.host);
        let actual_dir_name = installed
            .root
            .file_name()
            .and_then(|value| value.to_str())
            .ok_or_else(|| {
                CatalogError::invalid("installed driver directory is not portable UTF-8")
            })?;
        if actual_dir_name != expected_dir_name {
            return Err(CatalogError::invalid(
                "installed driver directory does not match its content-addressed identity",
            ));
        }
        let facts = ReleaseFacts {
            release_digest,
            host: self.host,
            driver_contract_version: release.get().driver_contract_version.clone(),
            backend_id: manifest.get().backend.id.clone(),
            driver_id: release.get().driver.id.clone(),
            driver_version: release.get().driver.version.clone(),
            manifest_digest,
            manifest_json,
            release_json,
            archive_digest: distribution.archive.sha256.value.clone(),
            archive_size_bytes: distribution.archive.size_bytes,
            tree_digest: distribution.tree_digest.value.clone(),
            installation_dir_name: expected_dir_name,
            authorization: Some(ReleaseAuthorizationFacts {
                envelope_digest: trust.verified_authorization.envelope_sha256.clone(),
                envelope_json: authorization_json,
                issued_at: trust.verified_authorization.issued_at,
                expires_at: trust.verified_authorization.expires_at,
                signer_key_ids: trust.verified_authorization.signer_key_ids.clone(),
                trust_root_version: trust.verified_authorization.trust_root_version,
                trust_root_digest: trust.verified_authorization.trust_root_sha256.clone(),
            }),
            transparency: Some(ReleaseTransparencyFacts {
                proof_digest: trust.verified_transparency.proof_sha256.clone(),
                proof_json: transparency_json,
                authorization_digest: trust.verified_transparency.authorization_sha256.clone(),
                log_origins: trust.verified_transparency.log_origins.clone(),
                log_operators: trust.verified_transparency.log_operators.clone(),
                witness_operators: trust.verified_transparency.witness_operators.clone(),
                earliest_witnessed_at: trust.verified_transparency.earliest_witnessed_at,
                latest_witnessed_at: trust.verified_transparency.latest_witnessed_at,
            }),
        };
        persist_verified_release(&self.store, facts).await
    }
}

fn insert_trust_root(
    connection: &rusqlite::Connection,
    root_version: i64,
    root_digest: &str,
    root_json: &str,
    expires_at: i64,
    observed_at: i64,
) -> CatalogResult<()> {
    connection.execute(
        "INSERT INTO provider_driver_trust_roots
           (root_version, root_sha256, root_json, expires_at, first_seen_at, last_seen_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?5)",
        params![
            root_version,
            root_digest,
            root_json,
            expires_at,
            observed_at
        ],
    )?;
    Ok(())
}

fn persist_trust_root_rotation(
    connection: &rusqlite::Connection,
    rotation: &ValidatedDriverTrustRootRotation,
    verified: &VerifiedDriverTrustRootRotation,
    observed_at: i64,
) -> CatalogResult<()> {
    let envelope_json = String::from_utf8(rotation.canonical_json().map_err(|error| {
        CatalogError::invalid(format!(
            "cannot canonicalize provider driver trust root rotation: {error}"
        ))
    })?)
    .expect("JCS JSON is UTF-8");
    let previous_signers_json = serde_json::to_string(&verified.previous_root_signer_key_ids)
        .map_err(|error| {
            CatalogError::invalid(format!(
                "cannot encode previous provider driver root signer IDs: {error}"
            ))
        })?;
    let next_signers_json =
        serde_json::to_string(&verified.next_root_signer_key_ids).map_err(|error| {
            CatalogError::invalid(format!(
                "cannot encode next provider driver root signer IDs: {error}"
            ))
        })?;
    connection.execute(
        "INSERT INTO provider_driver_trust_root_rotations
           (from_root_version, to_root_version, envelope_sha256, envelope_json,
            previous_signer_key_ids_json, next_signer_key_ids_json, observed_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            i64::try_from(verified.previous_root_version).map_err(|_| {
                CatalogError::invalid("previous provider driver root version exceeds SQLite range")
            })?,
            i64::try_from(verified.next_root_version).map_err(|_| {
                CatalogError::invalid("next provider driver root version exceeds SQLite range")
            })?,
            verified.envelope_sha256.as_str(),
            envelope_json,
            previous_signers_json,
            next_signers_json,
            observed_at,
        ],
    )?;
    Ok(())
}

fn validate_trust_root_transition(
    historical_root_json: &[String],
    previous_root: &ValidatedDriverReleaseTrustRoot,
    next_root: &ValidatedDriverReleaseTrustRoot,
) -> CatalogResult<()> {
    let previous = previous_root.get();
    let next = next_root.get();
    let previous_key_ids = previous
        .keys
        .iter()
        .map(|key| key.key_id.as_str())
        .collect::<BTreeSet<_>>();
    let previous_root_role_ids = previous
        .root
        .key_ids
        .iter()
        .map(String::as_str)
        .collect::<BTreeSet<_>>();
    let previous_release_role_ids = previous
        .provider_driver_release
        .key_ids
        .iter()
        .map(String::as_str)
        .collect::<BTreeSet<_>>();
    let previous_log_role_ids = previous
        .provider_driver_transparency
        .iter()
        .flat_map(|policy| policy.logs.iter().map(|log| log.key_id.as_str()))
        .collect::<BTreeSet<_>>();
    let previous_witness_role_ids = previous
        .provider_driver_transparency
        .iter()
        .flat_map(|policy| {
            policy
                .witnesses
                .iter()
                .map(|witness| witness.key_id.as_str())
        })
        .collect::<BTreeSet<_>>();
    let previous_log_identities = previous
        .provider_driver_transparency
        .iter()
        .flat_map(|policy| {
            policy.logs.iter().map(|log| {
                (
                    log.key_id.as_str(),
                    (log.origin.as_str(), log.operator.as_str()),
                )
            })
        })
        .collect::<BTreeMap<_, _>>();
    let previous_witness_identities = previous
        .provider_driver_transparency
        .iter()
        .flat_map(|policy| {
            policy.witnesses.iter().map(|witness| {
                (
                    witness.key_id.as_str(),
                    (witness.name.as_str(), witness.operator.as_str()),
                )
            })
        })
        .collect::<BTreeMap<_, _>>();
    let mut historical_key_ids = BTreeSet::new();
    let mut historical_root_role_ids = BTreeSet::new();
    let mut historical_release_role_ids = BTreeSet::new();
    let mut historical_log_role_ids = BTreeSet::new();
    let mut historical_witness_role_ids = BTreeSet::new();
    let mut permanently_revoked_key_ids = BTreeSet::new();

    for root_json in historical_root_json {
        let root =
            ValidatedDriverReleaseTrustRoot::parse_json(root_json.as_bytes()).map_err(|error| {
                CatalogError::invalid(format!(
                    "invalid durable provider driver trust root: {error}"
                ))
            })?;
        historical_key_ids.extend(root.get().keys.iter().map(|key| key.key_id.clone()));
        historical_root_role_ids.extend(root.get().root.key_ids.iter().cloned());
        historical_release_role_ids
            .extend(root.get().provider_driver_release.key_ids.iter().cloned());
        if let Some(policy) = &root.get().provider_driver_transparency {
            historical_log_role_ids.extend(policy.logs.iter().map(|log| log.key_id.clone()));
            historical_witness_role_ids.extend(
                policy
                    .witnesses
                    .iter()
                    .map(|witness| witness.key_id.clone()),
            );
        }
        permanently_revoked_key_ids.extend(
            root.get()
                .keys
                .iter()
                .filter(|key| {
                    key.status == deis_provider_driver::DriverReleaseSigningKeyStatus::Revoked
                })
                .map(|key| key.key_id.clone()),
        );
    }

    if next.keys.iter().any(|key| {
        key.status == deis_provider_driver::DriverReleaseSigningKeyStatus::Active
            && permanently_revoked_key_ids.contains(&key.key_id)
    }) {
        return Err(CatalogError::conflict(
            "provider driver trust root cannot reactivate a historically revoked key",
        ));
    }
    if next.keys.iter().any(|key| {
        historical_key_ids.contains(&key.key_id) && !previous_key_ids.contains(key.key_id.as_str())
    }) {
        return Err(CatalogError::conflict(
            "provider driver trust root cannot reintroduce a retired key",
        ));
    }
    if next.root.key_ids.iter().any(|key_id| {
        historical_root_role_ids.contains(key_id)
            && !previous_root_role_ids.contains(key_id.as_str())
    }) {
        return Err(CatalogError::conflict(
            "provider driver trust root cannot restore a key to the root role after retirement",
        ));
    }
    if next.provider_driver_release.key_ids.iter().any(|key_id| {
        historical_release_role_ids.contains(key_id)
            && !previous_release_role_ids.contains(key_id.as_str())
    }) {
        return Err(CatalogError::conflict(
            "provider driver trust root cannot restore a key to the release role after retirement",
        ));
    }
    if next.root.key_ids.iter().any(|key_id| {
        !previous_root_role_ids.contains(key_id.as_str())
            && (historical_release_role_ids.contains(key_id)
                || historical_log_role_ids.contains(key_id)
                || historical_witness_role_ids.contains(key_id))
    }) || next.provider_driver_release.key_ids.iter().any(|key_id| {
        !previous_release_role_ids.contains(key_id.as_str())
            && (historical_root_role_ids.contains(key_id)
                || historical_log_role_ids.contains(key_id)
                || historical_witness_role_ids.contains(key_id))
    }) {
        return Err(CatalogError::conflict(
            "provider driver trust root cannot move a historical key between trust roles",
        ));
    }
    if let Some(policy) = &next.provider_driver_transparency {
        if policy.logs.iter().any(|log| {
            !previous_log_role_ids.contains(log.key_id.as_str())
                && (historical_root_role_ids.contains(&log.key_id)
                    || historical_release_role_ids.contains(&log.key_id)
                    || historical_witness_role_ids.contains(&log.key_id))
        }) || policy.witnesses.iter().any(|witness| {
            !previous_witness_role_ids.contains(witness.key_id.as_str())
                && (historical_root_role_ids.contains(&witness.key_id)
                    || historical_release_role_ids.contains(&witness.key_id)
                    || historical_log_role_ids.contains(&witness.key_id))
        }) {
            return Err(CatalogError::conflict(
                "provider driver trust root cannot move a historical key between trust roles",
            ));
        }
        if policy.logs.iter().any(|log| {
            previous_log_identities
                .get(log.key_id.as_str())
                .is_some_and(|identity| *identity != (log.origin.as_str(), log.operator.as_str()))
        }) {
            return Err(CatalogError::conflict(
                "provider driver trust root cannot rename or reassign a retained transparency log key",
            ));
        }
        if policy.witnesses.iter().any(|witness| {
            previous_witness_identities
                .get(witness.key_id.as_str())
                .is_some_and(|identity| {
                    *identity != (witness.name.as_str(), witness.operator.as_str())
                })
        }) {
            return Err(CatalogError::conflict(
                "provider driver trust root cannot rename or reassign a retained transparency witness key",
            ));
        }
        if policy.logs.iter().any(|log| {
            historical_log_role_ids.contains(&log.key_id)
                && !previous_log_role_ids.contains(log.key_id.as_str())
        }) {
            return Err(CatalogError::conflict(
                "provider driver trust root cannot restore a key to the transparency log role after retirement",
            ));
        }
        if policy.witnesses.iter().any(|witness| {
            historical_witness_role_ids.contains(&witness.key_id)
                && !previous_witness_role_ids.contains(witness.key_id.as_str())
        }) {
            return Err(CatalogError::conflict(
                "provider driver trust root cannot restore a key to the transparency witness role after retirement",
            ));
        }
    }
    Ok(())
}

impl ProviderDriverDispatcher {
    /// `inputs` supplies each operation's config values, secret values and
    /// purge consent; see [`crate::catalog::CatalogOperationInputs`].
    pub fn new(
        registry: ProviderDriverReleaseRegistry,
        credential_vault: Arc<CatalogProviderCredentialVault>,
        inputs: Arc<dyn OperationInputSource>,
        workspaces: DriverWorkspaceRoot,
        process_config: ExecutableProviderDriverConfig,
    ) -> CatalogResult<Self> {
        if !credential_vault.belongs_to(&registry.store) {
            return Err(CatalogError::invalid(
                "provider driver registry and credential vault belong to different catalog databases",
            ));
        }
        let process_config = process_config.validate().map_err(|error| {
            CatalogError::invalid(format!("invalid provider driver process config: {error}"))
        })?;
        let credential_broker: Arc<dyn ProviderCredentialBroker> = credential_vault;
        Ok(Self {
            registry,
            credential_broker,
            inputs,
            workspaces,
            process_config,
        })
    }

    #[must_use]
    pub fn registry(&self) -> &ProviderDriverReleaseRegistry {
        &self.registry
    }

    #[must_use]
    pub fn belongs_to(&self, store: &CatalogStore) -> bool {
        self.registry.belongs_to(store)
    }

    pub(crate) async fn driver_for_target(
        &self,
        target_id: CanonicalId,
    ) -> CatalogResult<ExecutableProviderDriver> {
        let resolved = self.registry.resolve_for_target(target_id).await?;
        let credential_broker = Arc::clone(&self.credential_broker);
        let inputs = Arc::clone(&self.inputs);
        let workspaces = self.workspaces.clone();
        let process_config = self.process_config;
        tokio::task::spawn_blocking(move || {
            ExecutableProviderDriver::new(
                resolved.manifest,
                resolved.installed,
                credential_broker,
                inputs,
                workspaces,
                process_config,
            )
        })
        .await
        .map_err(|error| {
            CatalogError::invalid(format!("driver entrypoint verifier task failed: {error}"))
        })?
        .map_err(|error| {
            CatalogError::conflict(format!(
                "verified provider driver release has no usable process entrypoint: {error}"
            ))
        })
    }
}

#[derive(Clone)]
struct ReleaseFacts {
    release_digest: Sha256Digest,
    host: HostOperatingSystem,
    driver_contract_version: String,
    backend_id: String,
    driver_id: String,
    driver_version: String,
    manifest_digest: Sha256Digest,
    manifest_json: String,
    release_json: String,
    archive_digest: Sha256Digest,
    archive_size_bytes: u64,
    tree_digest: Sha256Digest,
    installation_dir_name: String,
    authorization: Option<ReleaseAuthorizationFacts>,
    transparency: Option<ReleaseTransparencyFacts>,
}

#[derive(Clone)]
struct ReleaseAuthorizationFacts {
    envelope_digest: Sha256Digest,
    envelope_json: String,
    issued_at: DateTime<Utc>,
    expires_at: DateTime<Utc>,
    signer_key_ids: Vec<String>,
    trust_root_version: u64,
    trust_root_digest: Sha256Digest,
}

#[derive(Clone)]
struct ReleaseTransparencyFacts {
    proof_digest: Sha256Digest,
    proof_json: String,
    authorization_digest: Sha256Digest,
    log_origins: Vec<String>,
    log_operators: Vec<String>,
    witness_operators: Vec<String>,
    earliest_witnessed_at: DateTime<Utc>,
    latest_witnessed_at: DateTime<Utc>,
}

struct StoredRelease {
    record: ProviderDriverReleaseRecord,
    manifest: ValidatedProviderDriverManifest,
    release: ValidatedProviderDriverRelease,
    verification_materials: Vec<StoredVerificationMaterial>,
}

struct StoredVerificationMaterial {
    authorization: ValidatedDriverReleaseAuthorization,
    transparency: ValidatedDriverReleaseTransparencyProof,
    log_origins: Vec<String>,
    log_operators: Vec<String>,
    witness_operators: Vec<String>,
    earliest_witnessed_at: DateTime<Utc>,
    latest_witnessed_at: DateTime<Utc>,
}

async fn persist_verified_release(
    store: &CatalogStore,
    facts: ReleaseFacts,
) -> CatalogResult<ProviderDriverReleaseRecord> {
    let database = store.database();
    database
        .with_connection(move |connection| {
            let transaction = connection.unchecked_transaction()?;
            let existing = load_release_record(
                &transaction,
                &facts.release_digest,
                facts.host,
            )?;
            let now = now_secs();
            let event_type = if let Some(existing) = existing {
                if existing.status != "active" {
                    return Err(CatalogError::conflict(
                        "a quarantined or revoked driver release cannot be reactivated by registration",
                    ));
                }
                ensure_same_release_facts(&existing, &facts)?;
                transaction.execute(
                    "UPDATE provider_driver_releases
                        SET verified_at = ?3, revision = revision + 1, updated_at = ?3
                      WHERE release_sha256 = ?1 AND host_operating_system = ?2",
                    params![facts.release_digest.as_str(), facts.host.as_str(), now],
                )?;
                "verified"
            } else {
                transaction.execute(
                    "INSERT INTO provider_driver_releases
                       (release_sha256, host_operating_system, driver_contract_version,
                        backend_id, driver_id, driver_version, manifest_sha256,
                        manifest_json, release_json, archive_sha256, archive_size_bytes,
                        tree_sha256, installation_dir_name, status, revision,
                        verified_at, created_at, updated_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11,
                             ?12, ?13, 'active', 1, ?14, ?14, ?14)",
                    params![
                        facts.release_digest.as_str(),
                        facts.host.as_str(),
                        facts.driver_contract_version,
                        facts.backend_id,
                        facts.driver_id,
                        facts.driver_version,
                        facts.manifest_digest.as_str(),
                        facts.manifest_json,
                        facts.release_json,
                        facts.archive_digest.as_str(),
                        i64::try_from(facts.archive_size_bytes).map_err(|_| {
                            CatalogError::invalid("driver archive size exceeds SQLite range")
                        })?,
                        facts.tree_digest.as_str(),
                        facts.installation_dir_name,
                        now,
                    ],
                )?;
                "registered"
            };
            let audit_detail = if let Some(authorization) = &facts.authorization {
                persist_release_authorization(
                    &transaction,
                    &facts.release_digest,
                    facts.host,
                    authorization,
                    now,
                )?;
                let transparency = facts.transparency.as_ref().ok_or_else(|| {
                    CatalogError::invalid(
                        "provider driver release registration requires a transparency proof",
                    )
                })?;
                if transparency.authorization_digest != authorization.envelope_digest {
                    return Err(CatalogError::invalid(
                        "provider driver transparency proof and authorization identities differ",
                    ));
                }
                persist_release_transparency(
                    &transaction,
                    &facts.release_digest,
                    facts.host,
                    transparency,
                    now,
                )?;
                format!(
                    "complete installation tree, DSSE authorization {} ({} signer keys), RFC 6962 inclusion proof {}, signed checkpoint, and witness threshold verified with trust root {} ({}), {} log operators, and {} witness operators",
                    authorization.envelope_digest,
                    authorization.signer_key_ids.len(),
                    transparency.proof_digest,
                    authorization.trust_root_version,
                    authorization.trust_root_digest,
                    transparency.log_operators.len(),
                    transparency.witness_operators.len()
                )
            } else {
                #[cfg(not(test))]
                return Err(CatalogError::invalid(
                    "provider driver release registration requires a signed authorization",
                ));
                #[cfg(test)]
                "test-only unsigned and non-transparent release seed; never available in production"
                    .to_owned()
            };
            insert_audit(
                &transaction,
                &facts.release_digest,
                facts.host,
                event_type,
                &audit_detail,
                now,
            )?;
            let record = load_release_record(
                &transaction,
                &facts.release_digest,
                facts.host,
            )?
            .ok_or_else(|| CatalogError::not_found("registered provider driver release"))?;
            transaction.commit()?;
            Ok(record)
        })
        .await
}

async fn load_release_for_target(
    store: &CatalogStore,
    target_id: CanonicalId,
    host: HostOperatingSystem,
) -> CatalogResult<StoredRelease> {
    let database = store.database();
    database
        .with_connection(move |connection| {
            let row = connection
                .query_row(
                    "SELECT r.release_sha256, r.driver_contract_version, r.backend_id,
                            r.driver_id, r.driver_version, r.manifest_sha256,
                            r.manifest_json, r.release_json, r.archive_sha256,
                            r.archive_size_bytes, r.tree_sha256, r.installation_dir_name,
                            r.status, r.revision, r.verified_at, r.created_at, r.updated_at,
                            t.driver_release_sha256, t.driver_manifest_sha256
                       FROM provider_targets t
                       JOIN provider_installations i ON i.installation_id = t.installation_id
                       JOIN provider_driver_releases r
                         ON r.release_sha256 = t.driver_release_sha256
                        AND r.host_operating_system = ?2
                      WHERE t.target_id = ?1 AND t.status = 'active'
                        AND i.status = 'active' AND r.status = 'active'
                        AND EXISTS (
                            SELECT 1 FROM provider_driver_release_authorizations a
                            JOIN provider_driver_release_transparency p
                              ON p.release_sha256 = a.release_sha256
                             AND p.host_operating_system = a.host_operating_system
                             AND p.authorization_sha256 = a.authorization_sha256
                             WHERE a.release_sha256 = r.release_sha256
                               AND a.host_operating_system = r.host_operating_system
                        )",
                    params![target_id.to_string(), host.as_str()],
                    |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, String>(1)?,
                            row.get::<_, String>(2)?,
                            row.get::<_, String>(3)?,
                            row.get::<_, String>(4)?,
                            row.get::<_, String>(5)?,
                            row.get::<_, String>(6)?,
                            row.get::<_, String>(7)?,
                            row.get::<_, String>(8)?,
                            row.get::<_, i64>(9)?,
                            row.get::<_, String>(10)?,
                            row.get::<_, String>(11)?,
                            row.get::<_, String>(12)?,
                            row.get::<_, i64>(13)?,
                            row.get::<_, i64>(14)?,
                            row.get::<_, i64>(15)?,
                            row.get::<_, i64>(16)?,
                            row.get::<_, String>(17)?,
                            row.get::<_, String>(18)?,
                        ))
                    },
                )
                .optional()?
                .ok_or_else(|| {
                    CatalogError::conflict(
                        "provider target has no active verified driver release for this Control host",
                    )
                })?;
            let release_digest: Sha256Digest = parse_digest(&row.0, "driver release")?;
            let manifest_digest: Sha256Digest = parse_digest(&row.5, "driver manifest")?;
            if row.17 != row.0 || row.18 != row.5 {
                return Err(CatalogError::invalid(
                    "provider target driver pin differs from its registered release",
                ));
            }
            let manifest = ValidatedProviderDriverManifest::parse_json(row.6.as_bytes()).map_err(
                |error| CatalogError::invalid(format!("invalid registered driver manifest: {error}")),
            )?;
            let release = ValidatedProviderDriverRelease::parse_json(row.7.as_bytes()).map_err(
                |error| CatalogError::invalid(format!("invalid registered driver release: {error}")),
            )?;
            let mut statement = connection.prepare(
                "SELECT a.authorization_sha256, a.authorization_json, a.issued_at,
                        a.expires_at, a.signer_key_ids_json,
                        p.proof_sha256, p.proof_json, p.log_origins_json,
                        p.log_operators_json, p.witness_operators_json,
                        p.earliest_witnessed_at, p.latest_witnessed_at
                   FROM provider_driver_release_authorizations a
                   JOIN provider_driver_release_transparency p
                     ON p.release_sha256 = a.release_sha256
                    AND p.host_operating_system = a.host_operating_system
                    AND p.authorization_sha256 = a.authorization_sha256
                  WHERE a.release_sha256 = ?1 AND a.host_operating_system = ?2
                  ORDER BY a.expires_at DESC, a.issued_at DESC,
                           a.authorization_sha256, p.proof_sha256",
            )?;
            let authorization_rows = statement
                .query_map(params![row.0, host.as_str()], |authorization_row| {
                    Ok((
                        authorization_row.get::<_, String>(0)?,
                        authorization_row.get::<_, String>(1)?,
                        authorization_row.get::<_, i64>(2)?,
                        authorization_row.get::<_, i64>(3)?,
                        authorization_row.get::<_, String>(4)?,
                        authorization_row.get::<_, String>(5)?,
                        authorization_row.get::<_, String>(6)?,
                        authorization_row.get::<_, String>(7)?,
                        authorization_row.get::<_, String>(8)?,
                        authorization_row.get::<_, String>(9)?,
                        authorization_row.get::<_, i64>(10)?,
                        authorization_row.get::<_, i64>(11)?,
                    ))
                })?
                .collect::<Result<Vec<_>, _>>()?;
            let verification_materials = authorization_rows
                .into_iter()
                .map(|authorization_row| {
                    let authorization = ValidatedDriverReleaseAuthorization::parse_json(
                        authorization_row.1.as_bytes(),
                    )
                    .map_err(|error| {
                        CatalogError::invalid(format!(
                            "invalid registered driver release authorization: {error}"
                        ))
                    })?;
                    validate_loaded_authorization(
                        &authorization,
                        &authorization_row.0,
                        authorization_row.2,
                        authorization_row.3,
                        &authorization_row.4,
                    )?;
                    let transparency = ValidatedDriverReleaseTransparencyProof::parse_json(
                        authorization_row.6.as_bytes(),
                    )
                    .map_err(|error| {
                        CatalogError::invalid(format!(
                            "invalid registered driver release transparency proof: {error}"
                        ))
                    })?;
                    let log_origins: Vec<String> =
                        serde_json::from_str(&authorization_row.7).map_err(|error| {
                            CatalogError::invalid(format!(
                                "invalid registered transparency log origins: {error}"
                            ))
                        })?;
                    let log_operators: Vec<String> =
                        serde_json::from_str(&authorization_row.8).map_err(|error| {
                            CatalogError::invalid(format!(
                                "invalid registered transparency log operators: {error}"
                            ))
                        })?;
                    let witness_operators: Vec<String> =
                        serde_json::from_str(&authorization_row.9).map_err(|error| {
                            CatalogError::invalid(format!(
                                "invalid registered transparency witness operators: {error}"
                            ))
                        })?;
                    let earliest_witnessed_at = timestamp(
                        authorization_row.10,
                        "driver transparency earliest witness time",
                    )?;
                    let latest_witnessed_at = timestamp(
                        authorization_row.11,
                        "driver transparency latest witness time",
                    )?;
                    validate_loaded_transparency(
                        &authorization,
                        &transparency,
                        &authorization_row.5,
                        &log_origins,
                        &log_operators,
                        &witness_operators,
                        earliest_witnessed_at,
                        latest_witnessed_at,
                    )?;
                    Ok(StoredVerificationMaterial {
                        authorization,
                        transparency,
                        log_origins,
                        log_operators,
                        witness_operators,
                        earliest_witnessed_at,
                        latest_witnessed_at,
                    })
                })
                .collect::<CatalogResult<Vec<_>>>()?;
            validate_loaded_documents(
                &release,
                &manifest,
                &release_digest,
                &manifest_digest,
                host,
                &row.2,
                &row.3,
                &row.4,
                &row.8,
                row.9,
                &row.10,
                &row.11,
            )?;
            Ok(StoredRelease {
                record: ProviderDriverReleaseRecord {
                    release_digest,
                    host_operating_system: host,
                    driver_contract_version: row.1,
                    backend_id: row.2,
                    driver_id: row.3,
                    driver_version: row.4,
                    manifest_digest,
                    archive_digest: parse_digest(&row.8, "driver archive")?,
                    archive_size_bytes: to_u64(row.9, "driver archive size")?,
                    tree_digest: parse_digest(&row.10, "driver tree")?,
                    installation_dir_name: row.11,
                    status: row.12,
                    revision: to_u64(row.13, "driver release revision")?,
                    verified_at: timestamp(row.14, "driver release verification time")?,
                    created_at: timestamp(row.15, "driver release creation time")?,
                    updated_at: timestamp(row.16, "driver release update time")?,
                },
                manifest,
                release,
                verification_materials,
            })
        })
        .await
}

fn validate_loaded_authorization(
    authorization: &ValidatedDriverReleaseAuthorization,
    envelope_digest: &str,
    issued_at: i64,
    expires_at: i64,
    signer_key_ids_json: &str,
) -> CatalogResult<()> {
    let canonical = authorization.canonical_json().map_err(|error| {
        CatalogError::invalid(format!(
            "cannot identify registered driver release authorization: {error}"
        ))
    })?;
    let computed_digest = hex::encode(Sha256::digest(&canonical));
    let signer_key_ids: Vec<String> =
        serde_json::from_str(signer_key_ids_json).map_err(|error| {
            CatalogError::invalid(format!("invalid registered driver signer IDs: {error}"))
        })?;
    let expected_signers = authorization
        .get()
        .signatures
        .iter()
        .map(|signature| signature.keyid.clone())
        .collect::<Vec<_>>();
    if computed_digest != envelope_digest
        || authorization.authorization().issued_at.timestamp() != issued_at
        || authorization.authorization().expires_at.timestamp() != expires_at
        || signer_key_ids != expected_signers
    {
        return Err(CatalogError::invalid(
            "registered provider driver release authorization facts differ from its canonical envelope",
        ));
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn validate_loaded_transparency(
    authorization: &ValidatedDriverReleaseAuthorization,
    transparency: &ValidatedDriverReleaseTransparencyProof,
    proof_digest: &str,
    log_origins: &[String],
    log_operators: &[String],
    witness_operators: &[String],
    earliest_witnessed_at: DateTime<Utc>,
    latest_witnessed_at: DateTime<Utc>,
) -> CatalogResult<()> {
    let authorization_digest = hex::encode(Sha256::digest(
        authorization.canonical_json().map_err(|error| {
            CatalogError::invalid(format!(
                "cannot identify registered driver authorization for transparency: {error}"
            ))
        })?,
    ));
    let proof_origins = transparency
        .get()
        .entries
        .iter()
        .map(|entry| entry.log_origin.clone())
        .collect::<Vec<_>>();
    if transparency.canonical_sha256().as_str() != proof_digest
        || transparency.get().authorization_sha256.value.as_str() != authorization_digest
        || proof_origins != log_origins
        || log_operators.is_empty()
        || witness_operators.is_empty()
        || !strictly_ordered(log_operators)
        || !strictly_ordered(witness_operators)
        || earliest_witnessed_at > latest_witnessed_at
    {
        return Err(CatalogError::invalid(
            "registered provider driver transparency facts differ from the canonical proof",
        ));
    }
    Ok(())
}

fn strictly_ordered(values: &[String]) -> bool {
    values
        .windows(2)
        .all(|pair| pair[0].as_str() < pair[1].as_str())
}

fn persist_release_authorization(
    connection: &rusqlite::Connection,
    release_digest: &Sha256Digest,
    host: HostOperatingSystem,
    authorization: &ReleaseAuthorizationFacts,
    now: i64,
) -> CatalogResult<()> {
    let signer_key_ids_json =
        serde_json::to_string(&authorization.signer_key_ids).map_err(|error| {
            CatalogError::invalid(format!("cannot encode driver signer IDs: {error}"))
        })?;
    let existing = connection
        .query_row(
            "SELECT authorization_sha256, authorization_json, issued_at, expires_at,
                    signer_key_ids_json
               FROM provider_driver_release_authorizations
              WHERE release_sha256 = ?1 AND host_operating_system = ?2
                AND authorization_sha256 = ?3",
            params![
                release_digest.as_str(),
                host.as_str(),
                authorization.envelope_digest.as_str()
            ],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, String>(4)?,
                ))
            },
        )
        .optional()?;
    let issued_at = authorization.issued_at.timestamp();
    let expires_at = authorization.expires_at.timestamp();
    if let Some(existing) = existing {
        if existing.0 != authorization.envelope_digest.as_str()
            || existing.1 != authorization.envelope_json
            || existing.2 != issued_at
            || existing.3 != expires_at
            || existing.4 != signer_key_ids_json
        {
            return Err(CatalogError::conflict(
                "provider driver release authorization conflicts with its immutable durable record",
            ));
        }
        return Ok(());
    }
    connection.execute(
        "INSERT INTO provider_driver_release_authorizations
           (release_sha256, host_operating_system, authorization_sha256,
            authorization_json, issued_at, expires_at, signer_key_ids_json, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![
            release_digest.as_str(),
            host.as_str(),
            authorization.envelope_digest.as_str(),
            authorization.envelope_json,
            issued_at,
            expires_at,
            signer_key_ids_json,
            now,
        ],
    )?;
    Ok(())
}

fn persist_release_transparency(
    connection: &rusqlite::Connection,
    release_digest: &Sha256Digest,
    host: HostOperatingSystem,
    transparency: &ReleaseTransparencyFacts,
    now: i64,
) -> CatalogResult<()> {
    let log_origins_json = serde_json::to_string(&transparency.log_origins).map_err(|error| {
        CatalogError::invalid(format!("cannot encode transparency log origins: {error}"))
    })?;
    let log_operators_json =
        serde_json::to_string(&transparency.log_operators).map_err(|error| {
            CatalogError::invalid(format!("cannot encode transparency log operators: {error}"))
        })?;
    let witness_operators_json =
        serde_json::to_string(&transparency.witness_operators).map_err(|error| {
            CatalogError::invalid(format!(
                "cannot encode transparency witness operators: {error}"
            ))
        })?;
    let earliest = transparency.earliest_witnessed_at.timestamp();
    let latest = transparency.latest_witnessed_at.timestamp();
    let existing = connection
        .query_row(
            "SELECT proof_json, authorization_sha256, log_origins_json,
                    log_operators_json, witness_operators_json,
                    earliest_witnessed_at, latest_witnessed_at
               FROM provider_driver_release_transparency
              WHERE release_sha256 = ?1 AND host_operating_system = ?2
                AND proof_sha256 = ?3",
            params![
                release_digest.as_str(),
                host.as_str(),
                transparency.proof_digest.as_str()
            ],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, i64>(5)?,
                    row.get::<_, i64>(6)?,
                ))
            },
        )
        .optional()?;
    if let Some(existing) = existing {
        if existing
            != (
                transparency.proof_json.clone(),
                transparency.authorization_digest.to_string(),
                log_origins_json,
                log_operators_json,
                witness_operators_json,
                earliest,
                latest,
            )
        {
            return Err(CatalogError::conflict(
                "provider driver transparency proof conflicts with its immutable durable record",
            ));
        }
        return Ok(());
    }
    connection.execute(
        "INSERT INTO provider_driver_release_transparency
           (release_sha256, host_operating_system, proof_sha256, proof_json,
            authorization_sha256, log_origins_json, log_operators_json,
            witness_operators_json, earliest_witnessed_at, latest_witnessed_at,
            created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
        params![
            release_digest.as_str(),
            host.as_str(),
            transparency.proof_digest.as_str(),
            transparency.proof_json,
            transparency.authorization_digest.as_str(),
            log_origins_json,
            log_operators_json,
            witness_operators_json,
            earliest,
            latest,
            now,
        ],
    )?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn validate_loaded_documents(
    release: &ValidatedProviderDriverRelease,
    manifest: &ValidatedProviderDriverManifest,
    release_digest: &Sha256Digest,
    manifest_digest: &Sha256Digest,
    host: HostOperatingSystem,
    backend_id: &str,
    driver_id: &str,
    driver_version: &str,
    archive_digest: &str,
    archive_size: i64,
    tree_digest: &str,
    installation_dir_name: &str,
) -> CatalogResult<()> {
    release.bind_to_manifest(manifest).map_err(|error| {
        CatalogError::invalid(format!(
            "registered release/manifest binding failed: {error}"
        ))
    })?;
    let distribution = release.distribution_for(host).ok_or_else(|| {
        CatalogError::invalid("registered release has no distribution for its recorded host")
    })?;
    let computed_release = release.canonical_sha256().map_err(|error| {
        CatalogError::invalid(format!(
            "cannot identify registered driver release: {error}"
        ))
    })?;
    let computed_manifest = manifest.canonical_sha256().map_err(|error| {
        CatalogError::invalid(format!(
            "cannot identify registered driver manifest: {error}"
        ))
    })?;
    if &computed_release != release_digest
        || &computed_manifest != manifest_digest
        || manifest.get().backend.id != backend_id
        || release.get().driver.id != driver_id
        || release.get().driver.version != driver_version
        || distribution.archive.sha256.value.as_str() != archive_digest
        || i64::try_from(distribution.archive.size_bytes).ok() != Some(archive_size)
        || distribution.tree_digest.value.as_str() != tree_digest
        || installation_dir_name != format!("{}-{host}", release_digest.as_str())
    {
        return Err(CatalogError::invalid(
            "registered provider driver release facts differ from canonical documents",
        ));
    }
    Ok(())
}

/// The installed-release list for one host, newest first, and one
/// release's audit trail, oldest first.
///
/// Both are written in the two forms a keyset page has: the cursor
/// predicate appears only when the request resumes from one, which is what
/// lets SQLite seek the page index rather than walk the host's whole
/// registry in sort order (see `pagination::KeysetResume`).
/// `pagination_plans` explains both forms of both.
pub(crate) fn releases_page_sql(resume: &KeysetResume) -> String {
    format!(
        "SELECT release_sha256 FROM provider_driver_releases
          WHERE host_operating_system = ?1{resume_clause}
          ORDER BY created_at DESC, release_sha256 DESC LIMIT ?{limit}",
        resume_clause = resume.and_clause(),
        limit = resume.limit_index(),
    )
}

pub(crate) fn release_audit_events_page_sql(resume: &KeysetResume) -> String {
    format!(
        "SELECT event_id, event_type, detail, observed_at
           FROM provider_driver_release_audit
          WHERE release_sha256 = ?1 AND host_operating_system = ?2{resume_clause}
          ORDER BY observed_at, event_id LIMIT ?{limit}",
        resume_clause = resume.and_clause(),
        limit = resume.limit_index(),
    )
}

fn load_release_record(
    connection: &rusqlite::Connection,
    release_digest: &Sha256Digest,
    host: HostOperatingSystem,
) -> CatalogResult<Option<ProviderDriverReleaseRecord>> {
    connection
        .query_row(
            "SELECT driver_contract_version, backend_id, driver_id, driver_version,
                    manifest_sha256, archive_sha256, archive_size_bytes, tree_sha256,
                    installation_dir_name, status, revision, verified_at, created_at, updated_at
               FROM provider_driver_releases
              WHERE release_sha256 = ?1 AND host_operating_system = ?2",
            params![release_digest.as_str(), host.as_str()],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, String>(5)?,
                    row.get::<_, i64>(6)?,
                    row.get::<_, String>(7)?,
                    row.get::<_, String>(8)?,
                    row.get::<_, String>(9)?,
                    row.get::<_, i64>(10)?,
                    row.get::<_, i64>(11)?,
                    row.get::<_, i64>(12)?,
                    row.get::<_, i64>(13)?,
                ))
            },
        )
        .optional()?
        .map(|row| {
            Ok(ProviderDriverReleaseRecord {
                release_digest: release_digest.clone(),
                host_operating_system: host,
                driver_contract_version: row.0,
                backend_id: row.1,
                driver_id: row.2,
                driver_version: row.3,
                manifest_digest: parse_digest(&row.4, "driver manifest")?,
                archive_digest: parse_digest(&row.5, "driver archive")?,
                archive_size_bytes: to_u64(row.6, "driver archive size")?,
                tree_digest: parse_digest(&row.7, "driver tree")?,
                installation_dir_name: row.8,
                status: row.9,
                revision: to_u64(row.10, "driver release revision")?,
                verified_at: timestamp(row.11, "driver release verification time")?,
                created_at: timestamp(row.12, "driver release creation time")?,
                updated_at: timestamp(row.13, "driver release update time")?,
            })
        })
        .transpose()
}

fn ensure_same_release_facts(
    existing: &ProviderDriverReleaseRecord,
    facts: &ReleaseFacts,
) -> CatalogResult<()> {
    if existing.release_digest != facts.release_digest
        || existing.host_operating_system != facts.host
        || existing.driver_contract_version != facts.driver_contract_version
        || existing.backend_id != facts.backend_id
        || existing.driver_id != facts.driver_id
        || existing.driver_version != facts.driver_version
        || existing.manifest_digest != facts.manifest_digest
        || existing.archive_digest != facts.archive_digest
        || existing.archive_size_bytes != facts.archive_size_bytes
        || existing.tree_digest != facts.tree_digest
        || existing.installation_dir_name != facts.installation_dir_name
    {
        return Err(CatalogError::conflict(
            "registered provider driver release identity conflicts with durable facts",
        ));
    }
    Ok(())
}

async fn quarantine_release(
    store: &CatalogStore,
    release_digest: Sha256Digest,
    host: HostOperatingSystem,
    detail: &'static str,
) -> CatalogResult<()> {
    let database = store.database();
    database
        .with_connection(move |connection| {
            let transaction = connection.unchecked_transaction()?;
            let now = now_secs();
            let changed = transaction.execute(
                "UPDATE provider_driver_releases
                    SET status = 'quarantined', revision = revision + 1, updated_at = ?3
                  WHERE release_sha256 = ?1 AND host_operating_system = ?2
                    AND status = 'active'",
                params![release_digest.as_str(), host.as_str(), now],
            )?;
            if changed == 1 {
                insert_audit(
                    &transaction,
                    &release_digest,
                    host,
                    "quarantined",
                    detail,
                    now,
                )?;
            }
            transaction.commit()?;
            Ok(())
        })
        .await
}

fn insert_audit(
    connection: &rusqlite::Connection,
    release_digest: &Sha256Digest,
    host: HostOperatingSystem,
    event_type: &str,
    detail: &str,
    now: i64,
) -> CatalogResult<()> {
    connection.execute(
        "INSERT INTO provider_driver_release_audit
           (event_id, release_sha256, host_operating_system, event_type, detail, observed_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            CanonicalId::new_v7().to_string(),
            release_digest.as_str(),
            host.as_str(),
            event_type,
            detail,
            now,
        ],
    )?;
    Ok(())
}

fn installation_error(error: DriverInstallationError) -> CatalogError {
    CatalogError::invalid(format!(
        "provider driver installation verification failed: {error}"
    ))
}

fn authorization_error(
    error: deis_provider_driver::DriverReleaseAuthorizationError,
) -> CatalogError {
    CatalogError::invalid(format!(
        "provider driver release authorization verification failed: {error}"
    ))
}

fn trust_root_rotation_error(
    error: deis_provider_driver::DriverReleaseAuthorizationError,
) -> CatalogError {
    CatalogError::invalid(format!(
        "provider driver trust root rotation verification failed: {error}"
    ))
}

fn transparency_error(
    error: deis_provider_driver::DriverReleaseAuthorizationError,
) -> CatalogError {
    CatalogError::invalid(format!(
        "provider driver release transparency verification failed: {error}"
    ))
}

fn parse_digest(value: &str, label: &str) -> CatalogResult<Sha256Digest> {
    value
        .parse()
        .map_err(|error| CatalogError::invalid(format!("invalid {label} digest: {error}")))
}

fn parse_id(value: &str, label: &str) -> CatalogResult<CanonicalId> {
    value
        .parse()
        .map_err(|error| CatalogError::invalid(format!("invalid {label} identity: {error}")))
}

fn to_u64(value: i64, label: &str) -> CatalogResult<u64> {
    u64::try_from(value).map_err(|_| CatalogError::invalid(format!("invalid {label}: {value}")))
}

fn timestamp(value: i64, label: &str) -> CatalogResult<DateTime<Utc>> {
    DateTime::from_timestamp(value, 0)
        .ok_or_else(|| CatalogError::invalid(format!("invalid {label}: {value}")))
}

#[cfg(test)]
pub(crate) async fn seed_test_provider_driver_release(
    store: &CatalogStore,
    manifest: &ValidatedProviderDriverManifest,
) {
    let release = ValidatedProviderDriverRelease::parse_json(include_bytes!(
        "../../../../contracts/v1/fixtures/valid/provider-driver-release.json"
    ))
    .unwrap();
    release.bind_to_manifest(manifest).unwrap();
    let host = HostOperatingSystem::current().unwrap();
    let distribution = release.distribution_for(host).unwrap();
    let facts = ReleaseFacts {
        release_digest: release.canonical_sha256().unwrap(),
        host,
        driver_contract_version: release.get().driver_contract_version.clone(),
        backend_id: manifest.get().backend.id.clone(),
        driver_id: release.get().driver.id.clone(),
        driver_version: release.get().driver.version.clone(),
        manifest_digest: manifest.canonical_sha256().unwrap(),
        manifest_json: String::from_utf8(manifest.canonical_json().unwrap()).unwrap(),
        release_json: String::from_utf8(release.canonical_json().unwrap()).unwrap(),
        archive_digest: distribution.archive.sha256.value.clone(),
        archive_size_bytes: distribution.archive.size_bytes,
        tree_digest: distribution.tree_digest.value.clone(),
        installation_dir_name: format!("{}-{host}", release.canonical_sha256().unwrap().as_str()),
        authorization: Some(ReleaseAuthorizationFacts {
            envelope_digest: "a".repeat(64).parse().unwrap(),
            envelope_json: "{}".to_owned(),
            issued_at: Utc::now() - chrono::TimeDelta::minutes(1),
            expires_at: Utc::now() + chrono::TimeDelta::days(1),
            signer_key_ids: vec!["b".repeat(64)],
            trust_root_version: 1,
            trust_root_digest: "c".repeat(64).parse().unwrap(),
        }),
        transparency: Some(ReleaseTransparencyFacts {
            proof_digest: "d".repeat(64).parse().unwrap(),
            proof_json: "{}".to_owned(),
            authorization_digest: "a".repeat(64).parse().unwrap(),
            log_origins: vec!["test-only.invalid/log".to_owned()],
            log_operators: vec!["test-only.invalid".to_owned()],
            witness_operators: vec!["test-only.invalid".to_owned()],
            earliest_witnessed_at: Utc::now() - chrono::TimeDelta::minutes(1),
            latest_witnessed_at: Utc::now(),
        }),
    };
    persist_verified_release(store, facts).await.unwrap();
}

#[cfg(test)]
pub(crate) mod tests {
    use std::{
        collections::BTreeMap,
        fs::{self, File},
        path::{Path, PathBuf},
        sync::Arc,
    };

    use base64::{Engine as _, engine::general_purpose::STANDARD};
    use chrono::TimeDelta;
    use deis_provider_driver::{
        CredentialMaterial, DSSE_ENVELOPE_PAYLOAD_TYPE, DSSE_TRUST_ROOT_PAYLOAD_TYPE,
        DriverReleaseAuthorization, DriverReleaseAuthorizationSubject, DriverReleaseComponent,
        DriverReleaseDsseEnvelope, DriverReleaseDsseSignature, DriverReleaseFile,
        DriverReleaseRootRole, DriverReleaseSigningKey, DriverReleaseSigningKeyStatus,
        DriverReleaseSigningRole, DriverReleaseTransparencyEntry, DriverReleaseTransparencyLog,
        DriverReleaseTransparencyPolicy, DriverReleaseTransparencyProof,
        DriverReleaseTransparencyWitness, DriverReleaseTrustRoot, DriverTrustRootDsseEnvelope,
        ProviderDriver, ProviderDriverManifest, ProviderDriverRelease, SupportedStandard,
        TRANSPARENCY_PROOF_MEDIA_TYPE, ToolDigest,
    };
    use ed25519_dalek::{Signer as _, SigningKey};
    use serde::Serialize;
    use sha2::{Digest as _, Sha256};
    use tempfile::TempDir;

    use super::*;
    use crate::catalog::{
        CatalogProviderCredentialVault, OperationRequestContext, ProviderCredentialBindingSpec,
        ProviderInstallationSpec, ProviderTargetSpec, SecretCipher, SecretsBackend,
    };
    use crate::database::Database;

    const RELEASE_FIXTURE: &[u8] =
        include_bytes!("../../../../contracts/v1/fixtures/valid/provider-driver-release.json");
    const MANIFEST_FIXTURE: &[u8] =
        include_bytes!("../../../../contracts/v1/fixtures/valid/provider-driver.json");

    pub(crate) struct Fixture {
        temporary: TempDir,
        pub(crate) install_root: PathBuf,
        pub(crate) archive: PathBuf,
        pub(crate) release: ValidatedProviderDriverRelease,
        pub(crate) manifest: ValidatedProviderDriverManifest,
        pub(crate) trust_root: ValidatedDriverReleaseTrustRoot,
        pub(crate) authorization: ValidatedDriverReleaseAuthorization,
        pub(crate) transparency: ValidatedDriverReleaseTransparencyProof,
        contents: BTreeMap<String, Vec<u8>>,
    }

    impl Fixture {
        pub(crate) fn new() -> Self {
            Self::build(None)
        }

        /// A release whose driver entrypoint is `driver`, so a test can
        /// launch a real process through the installed, verified tree.
        pub(crate) fn with_driver_executable(driver: Vec<u8>) -> Self {
            Self::build(Some(driver))
        }

        fn build(driver: Option<Vec<u8>>) -> Self {
            let temporary = tempfile::tempdir().unwrap();
            let install_root = temporary.path().join("installed");
            fs::create_dir(&install_root).unwrap();
            make_install_root_private(&install_root);
            let archive = temporary.path().join("driver.tar");
            let host = HostOperatingSystem::current().unwrap();
            let template: ProviderDriverRelease = serde_json::from_slice(RELEASE_FIXTURE).unwrap();
            let mut distribution = template
                .distributions
                .iter()
                .find(|distribution| distribution.host_operating_system == host)
                .unwrap()
                .clone();
            let contents = distribution
                .files
                .iter()
                .map(|file| {
                    let is_driver = file.path.contains("driver");
                    let content = match (&driver, is_driver) {
                        (Some(driver), true) => driver.clone(),
                        (None, true) => vec![b'd'; 128],
                        (_, false) => vec![b'w'; 256],
                    };
                    (file.path.clone(), content)
                })
                .collect::<BTreeMap<_, _>>();
            bind_file_table(&mut distribution.files, &contents);
            bind_components(&mut distribution.components, &distribution.files);
            distribution.tree_digest.value = digest_jcs(&distribution.files);
            write_archive(&archive, &contents);
            let archive_bytes = fs::read(&archive).unwrap();
            distribution.archive.size_bytes = archive_bytes.len() as u64;
            distribution.archive.sha256.value = digest_bytes(&archive_bytes);

            let mut release = template;
            release.distributions = vec![distribution];
            let release = ValidatedProviderDriverRelease::new(release).unwrap();
            let mut manifest: ProviderDriverManifest =
                serde_json::from_slice(MANIFEST_FIXTURE).unwrap();
            manifest.supported_host_operating_systems = vec![host.as_str().to_owned()];
            manifest.driver.distribution_digest.value = release.canonical_sha256().unwrap();
            manifest
                .supported_standard_versions
                .push(SupportedStandard {
                    contract_id: deis_wire::CURRENT_EDGE_CANON_STANDARD_VERSION
                        .split_once('@')
                        .unwrap()
                        .0
                        .to_owned(),
                    version: deis_wire::CURRENT_EDGE_CANON_STANDARD_VERSION.to_owned(),
                    contract_digest: ToolDigest {
                        algorithm: "sha256".to_owned(),
                        value: deis_wire::CURRENT_EDGE_CANON_CONTRACT_SHA256
                            .parse()
                            .unwrap(),
                    },
                });
            let manifest = ValidatedProviderDriverManifest::new(manifest).unwrap();
            let (trust_root, authorization, transparency) =
                signed_authorization(&release, &manifest);

            Self {
                temporary,
                install_root,
                archive,
                release,
                manifest,
                trust_root,
                authorization,
                transparency,
                contents,
            }
        }

        fn installed_file(&self) -> PathBuf {
            let release_digest = self.release.canonical_sha256().unwrap();
            let relative = self.contents.keys().next().unwrap();
            self.install_root
                .join(format!(
                    "{}-{}",
                    release_digest.as_str(),
                    HostOperatingSystem::current().unwrap()
                ))
                .join(relative)
        }
    }

    fn signed_authorization(
        release: &ValidatedProviderDriverRelease,
        manifest: &ValidatedProviderDriverManifest,
    ) -> (
        ValidatedDriverReleaseTrustRoot,
        ValidatedDriverReleaseAuthorization,
        ValidatedDriverReleaseTransparencyProof,
    ) {
        let now = Utc::now();
        let signing_key = SigningKey::from_bytes(&[29; 32]);
        let public_key = signing_key.verifying_key().to_bytes();
        let key_id = hex::encode(Sha256::digest(public_key));
        let root_signing_key = SigningKey::from_bytes(&[23; 32]);
        let root_public_key = root_signing_key.verifying_key().to_bytes();
        let root_key_id = hex::encode(Sha256::digest(root_public_key));
        let log_signing_key = SigningKey::from_bytes(&[31; 32]);
        let log_public_key = log_signing_key.verifying_key().to_bytes();
        let log_key_id = hex::encode(Sha256::digest(log_public_key));
        let witness_signing_key = SigningKey::from_bytes(&[33; 32]);
        let witness_public_key = witness_signing_key.verifying_key().to_bytes();
        let witness_key_id = hex::encode(Sha256::digest(witness_public_key));
        let log_origin = "log.example.test/deislet";
        let witness_name = "witness.example.test/deislet";
        let mut keys = vec![
            DriverReleaseSigningKey {
                key_id: key_id.clone(),
                key_type: "ed25519".to_owned(),
                scheme: "ed25519".to_owned(),
                public_key: STANDARD.encode(public_key),
                status: DriverReleaseSigningKeyStatus::Active,
                valid_from: now - TimeDelta::days(1),
                valid_until: now + TimeDelta::days(90),
                revoked_at: None,
            },
            DriverReleaseSigningKey {
                key_id: root_key_id.clone(),
                key_type: "ed25519".to_owned(),
                scheme: "ed25519".to_owned(),
                public_key: STANDARD.encode(root_public_key),
                status: DriverReleaseSigningKeyStatus::Active,
                valid_from: now - TimeDelta::days(1),
                valid_until: now + TimeDelta::days(90),
                revoked_at: None,
            },
            DriverReleaseSigningKey {
                key_id: log_key_id.clone(),
                key_type: "ed25519".to_owned(),
                scheme: "ed25519".to_owned(),
                public_key: STANDARD.encode(log_public_key),
                status: DriverReleaseSigningKeyStatus::Active,
                valid_from: now - TimeDelta::days(1),
                valid_until: now + TimeDelta::days(90),
                revoked_at: None,
            },
            DriverReleaseSigningKey {
                key_id: witness_key_id.clone(),
                key_type: "ed25519".to_owned(),
                scheme: "ed25519".to_owned(),
                public_key: STANDARD.encode(witness_public_key),
                status: DriverReleaseSigningKeyStatus::Active,
                valid_from: now - TimeDelta::days(1),
                valid_until: now + TimeDelta::days(90),
                revoked_at: None,
            },
        ];
        keys.sort_by(|left, right| left.key_id.cmp(&right.key_id));
        let trust_root = ValidatedDriverReleaseTrustRoot::new(DriverReleaseTrustRoot {
            schema: Some("provider-driver-trust-root.schema.json".to_owned()),
            schema_version: 1,
            root_version: 1,
            expires_at: now + TimeDelta::days(30),
            root: DriverReleaseRootRole {
                key_ids: vec![root_key_id.clone()],
                threshold: 1,
            },
            provider_driver_release: DriverReleaseSigningRole {
                key_ids: vec![key_id.clone()],
                threshold: 1,
                max_authorization_lifetime_seconds: 7 * 24 * 60 * 60,
            },
            provider_driver_transparency: Some(DriverReleaseTransparencyPolicy {
                log_operator_threshold: 1,
                witness_operator_threshold: 1,
                logs: vec![DriverReleaseTransparencyLog {
                    origin: log_origin.to_owned(),
                    operator: "log-operator.example.test".to_owned(),
                    key_id: log_key_id,
                }],
                witnesses: vec![DriverReleaseTransparencyWitness {
                    name: witness_name.to_owned(),
                    operator: "witness-operator.example.test".to_owned(),
                    key_id: witness_key_id,
                }],
            }),
            keys,
        })
        .unwrap();
        let authorization = DriverReleaseAuthorization {
            schema: Some("provider-driver-release-authorization.schema.json".to_owned()),
            schema_version: 1,
            subject: DriverReleaseAuthorizationSubject {
                release_sha256: ToolDigest {
                    algorithm: "sha256".to_owned(),
                    value: release.canonical_sha256().unwrap(),
                },
                manifest_sha256: ToolDigest {
                    algorithm: "sha256".to_owned(),
                    value: manifest.canonical_sha256().unwrap(),
                },
                backend_id: manifest.get().backend.id.clone(),
                driver_id: release.get().driver.id.clone(),
                driver_version: release.get().driver.version.clone(),
            },
            issued_at: now - TimeDelta::minutes(1),
            expires_at: now + TimeDelta::days(1),
        };
        let payload = serde_jcs::to_vec(&authorization).unwrap();
        let mut pae = format!(
            "DSSEv1 {} {} {} ",
            DSSE_ENVELOPE_PAYLOAD_TYPE.len(),
            DSSE_ENVELOPE_PAYLOAD_TYPE,
            payload.len()
        )
        .into_bytes();
        pae.extend_from_slice(&payload);
        let authorization = ValidatedDriverReleaseAuthorization::new(DriverReleaseDsseEnvelope {
            payload_type: DSSE_ENVELOPE_PAYLOAD_TYPE.to_owned(),
            payload: STANDARD.encode(payload),
            signatures: vec![DriverReleaseDsseSignature {
                keyid: key_id,
                sig: STANDARD.encode(signing_key.sign(&pae).to_bytes()),
            }],
        })
        .unwrap();
        let authorization_json = authorization.canonical_json().unwrap();
        let mut leaf_hasher = Sha256::new();
        leaf_hasher.update([0]);
        leaf_hasher.update(&authorization_json);
        let leaf_hash: [u8; 32] = leaf_hasher.finalize().into();
        let body = format!("{log_origin}\n1\n{}\n", STANDARD.encode(leaf_hash));
        let mut log_id_hasher = Sha256::new();
        log_id_hasher.update(log_origin.as_bytes());
        log_id_hasher.update(b"\n");
        log_id_hasher.update([0x01]);
        log_id_hasher.update(log_public_key);
        let mut log_signature = log_id_hasher.finalize()[..4].to_vec();
        log_signature.extend_from_slice(&log_signing_key.sign(body.as_bytes()).to_bytes());
        let witnessed_at = now.timestamp();
        let witness_message = format!("cosignature/v1\ntime {witnessed_at}\n{body}");
        let mut witness_id_hasher = Sha256::new();
        witness_id_hasher.update(witness_name.as_bytes());
        witness_id_hasher.update(b"\n");
        witness_id_hasher.update([0x04]);
        witness_id_hasher.update(witness_public_key);
        let mut witness_signature = witness_id_hasher.finalize()[..4].to_vec();
        witness_signature.extend_from_slice(&u64::try_from(witnessed_at).unwrap().to_be_bytes());
        witness_signature.extend_from_slice(
            &witness_signing_key
                .sign(witness_message.as_bytes())
                .to_bytes(),
        );
        let checkpoint = format!(
            "{body}\n— {log_origin} {}\n— {witness_name} {}\n",
            STANDARD.encode(log_signature),
            STANDARD.encode(witness_signature)
        );
        let transparency =
            ValidatedDriverReleaseTransparencyProof::new(DriverReleaseTransparencyProof {
                schema: Some("provider-driver-transparency-proof.schema.json".to_owned()),
                schema_version: 1,
                media_type: TRANSPARENCY_PROOF_MEDIA_TYPE.to_owned(),
                authorization_sha256: ToolDigest {
                    algorithm: "sha256".to_owned(),
                    value: hex::encode(Sha256::digest(&authorization_json))
                        .parse()
                        .unwrap(),
                },
                entries: vec![DriverReleaseTransparencyEntry {
                    log_origin: log_origin.to_owned(),
                    log_index: 0,
                    inclusion_path: Vec::new(),
                    checkpoint,
                }],
            })
            .unwrap();
        (trust_root, authorization, transparency)
    }

    fn signed_root_rotation(next_root: DriverReleaseTrustRoot) -> ValidatedDriverTrustRootRotation {
        signed_root_rotation_with_seeds(next_root, &[23])
    }

    fn signed_root_rotation_with_seeds(
        next_root: DriverReleaseTrustRoot,
        seeds: &[u8],
    ) -> ValidatedDriverTrustRootRotation {
        let payload = serde_jcs::to_vec(&next_root).unwrap();
        let mut pae = format!(
            "DSSEv1 {} {} {} ",
            DSSE_TRUST_ROOT_PAYLOAD_TYPE.len(),
            DSSE_TRUST_ROOT_PAYLOAD_TYPE,
            payload.len()
        )
        .into_bytes();
        pae.extend_from_slice(&payload);
        let mut signatures = seeds
            .iter()
            .map(|seed| {
                let signing_key = SigningKey::from_bytes(&[*seed; 32]);
                DriverReleaseDsseSignature {
                    keyid: hex::encode(Sha256::digest(signing_key.verifying_key().to_bytes())),
                    sig: STANDARD.encode(signing_key.sign(&pae).to_bytes()),
                }
            })
            .collect::<Vec<_>>();
        signatures.sort_by(|left, right| left.keyid.cmp(&right.keyid));
        ValidatedDriverTrustRootRotation::new(DriverTrustRootDsseEnvelope {
            payload_type: DSSE_TRUST_ROOT_PAYLOAD_TYPE.to_owned(),
            payload: STANDARD.encode(payload),
            signatures,
        })
        .unwrap()
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            make_writable_for_cleanup(self.temporary.path());
        }
    }

    #[tokio::test]
    async fn only_a_complete_real_installation_is_registered_and_audited() {
        let fixture = Fixture::new();
        let database = Arc::new(
            Database::init(fixture.temporary.path().join("control.sqlite"))
                .await
                .unwrap(),
        );
        let store = CatalogStore::new(Arc::clone(&database));
        let registry = ProviderDriverReleaseRegistry::new(
            store,
            &fixture.install_root,
            fixture.trust_root.clone(),
            None,
        )
        .await
        .unwrap();

        assert!(
            registry
                .register_existing(
                    fixture.release.clone(),
                    fixture.manifest.clone(),
                    fixture.authorization.clone(),
                    fixture.transparency.clone(),
                )
                .await
                .unwrap_err()
                .to_string()
                .contains("verification failed")
        );
        let registered = registry
            .install_and_register(
                fixture.release.clone(),
                fixture.manifest.clone(),
                fixture.authorization.clone(),
                fixture.transparency.clone(),
                &fixture.archive,
            )
            .await
            .unwrap();
        assert_eq!(registered.status, "active");
        assert_eq!(registered.revision, 1);
        assert_eq!(
            registry
                .release(registered.release_digest.clone())
                .await
                .unwrap(),
            Some(registered.clone())
        );

        let verified = registry
            .register_existing(
                fixture.release.clone(),
                fixture.manifest.clone(),
                fixture.authorization.clone(),
                fixture.transparency.clone(),
            )
            .await
            .unwrap();
        assert_eq!(verified.revision, 2);
        let events = registry
            .audit_events(registered.release_digest.clone(), 10)
            .await
            .unwrap();
        assert_eq!(
            events
                .iter()
                .map(|event| event.event_type.as_str())
                .collect::<Vec<_>>(),
            ["registered", "verified"]
        );
        let (_, renewed_authorization, renewed_transparency) =
            signed_authorization(&fixture.release, &fixture.manifest);
        let renewed = registry
            .register_existing(
                fixture.release.clone(),
                fixture.manifest.clone(),
                renewed_authorization,
                renewed_transparency,
            )
            .await
            .unwrap();
        assert_eq!(renewed.revision, 3);
        let release_digest = renewed.release_digest.to_string();
        let authorization_count: i64 = database
            .with_connection(move |connection| {
                Ok::<i64, crate::database::DatabaseError>(connection.query_row(
                    "SELECT count(*) FROM provider_driver_release_authorizations
                      WHERE release_sha256 = ?1",
                    [release_digest],
                    |row| row.get(0),
                )?)
            })
            .await
            .unwrap();
        assert_eq!(authorization_count, 2);
        let release_digest = renewed.release_digest.to_string();
        let transparency_count: i64 = database
            .with_connection(move |connection| {
                Ok::<i64, crate::database::DatabaseError>(connection.query_row(
                    "SELECT count(*) FROM provider_driver_release_transparency
                      WHERE release_sha256 = ?1",
                    [release_digest],
                    |row| row.get(0),
                )?)
            })
            .await
            .unwrap();
        assert_eq!(transparency_count, 2);
        assert!(
            registry
                .audit_events(registered.release_digest.clone(), 0)
                .await
                .is_err()
        );

        let release_digest = registered.release_digest.to_string();
        let host = registry.host().as_str().to_owned();
        assert!(
            database
                .with_connection(move |connection| {
                    connection.execute(
                        "UPDATE provider_driver_releases SET backend_id = 'deislet'
                          WHERE release_sha256 = ?1 AND host_operating_system = ?2",
                        params![release_digest, host],
                    )?;
                    Ok::<(), crate::database::DatabaseError>(())
                })
                .await
                .is_err()
        );
        assert!(
            database
                .with_connection(|connection| {
                    connection.execute(
                        "UPDATE provider_driver_release_transparency
                            SET witness_operators_json = '[]'",
                        [],
                    )?;
                    Ok::<(), crate::database::DatabaseError>(())
                })
                .await
                .is_err()
        );
        assert!(
            database
                .with_connection(|connection| {
                    connection.execute("DELETE FROM provider_driver_release_transparency", [])?;
                    Ok::<(), crate::database::DatabaseError>(())
                })
                .await
                .is_err()
        );
        assert!(
            database
                .with_connection(|connection| {
                    connection.execute("DELETE FROM provider_driver_release_audit", [])?;
                    Ok::<(), crate::database::DatabaseError>(())
                })
                .await
                .is_err()
        );
        assert!(
            database
                .with_connection(|connection| {
                    connection.execute(
                        "UPDATE provider_driver_release_authorizations
                            SET signer_key_ids_json = '[]'",
                        [],
                    )?;
                    Ok::<(), crate::database::DatabaseError>(())
                })
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn invalid_dsse_is_rejected_before_archive_installation() {
        let fixture = Fixture::new();
        let database = Arc::new(
            Database::init(fixture.temporary.path().join("invalid-signature.sqlite"))
                .await
                .unwrap(),
        );
        let store = CatalogStore::new(database);
        let registry = ProviderDriverReleaseRegistry::new(
            store,
            &fixture.install_root,
            fixture.trust_root.clone(),
            None,
        )
        .await
        .unwrap();
        let mut tampered = fixture.authorization.get().clone();
        tampered.signatures[0].sig = STANDARD.encode([0; 64]);
        let tampered = ValidatedDriverReleaseAuthorization::new(tampered).unwrap();
        let error = registry
            .install_and_register(
                fixture.release.clone(),
                fixture.manifest.clone(),
                tampered,
                fixture.transparency.clone(),
                &fixture.archive,
            )
            .await
            .unwrap_err();
        assert!(error.to_string().contains("signature verification failed"));
        assert_eq!(fs::read_dir(&fixture.install_root).unwrap().count(), 0);

        let mut wrong_subject = fixture.transparency.get().clone();
        wrong_subject.authorization_sha256.value = "0".repeat(64).parse().unwrap();
        let wrong_subject = ValidatedDriverReleaseTransparencyProof::new(wrong_subject).unwrap();
        let error = registry
            .install_and_register(
                fixture.release.clone(),
                fixture.manifest.clone(),
                fixture.authorization.clone(),
                wrong_subject,
                &fixture.archive,
            )
            .await
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("canonical authorization envelope")
        );
        assert_eq!(fs::read_dir(&fixture.install_root).unwrap().count(), 0);
    }

    #[test]
    fn retained_transparency_keys_cannot_change_operator_identity() {
        let fixture = Fixture::new();
        let previous = fixture.trust_root.clone();
        let historical = vec![String::from_utf8(previous.canonical_json().unwrap()).unwrap()];

        let mut renamed_log = previous.get().clone();
        renamed_log.root_version += 1;
        renamed_log
            .provider_driver_transparency
            .as_mut()
            .unwrap()
            .logs[0]
            .operator = "different-log-operator.example.test".to_owned();
        let renamed_log = ValidatedDriverReleaseTrustRoot::new(renamed_log).unwrap();
        let error =
            validate_trust_root_transition(&historical, &previous, &renamed_log).unwrap_err();
        assert!(error.to_string().contains("retained transparency log key"));

        let mut renamed_witness = previous.get().clone();
        renamed_witness.root_version += 1;
        renamed_witness
            .provider_driver_transparency
            .as_mut()
            .unwrap()
            .witnesses[0]
            .name = "different-witness.example.test/deislet".to_owned();
        let renamed_witness = ValidatedDriverReleaseTrustRoot::new(renamed_witness).unwrap();
        let error =
            validate_trust_root_transition(&historical, &previous, &renamed_witness).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("retained transparency witness key")
        );

        let mut reassigned_roles = previous.get().clone();
        reassigned_roles.root_version += 1;
        let release_key_id = reassigned_roles.provider_driver_release.key_ids[0].clone();
        let log_key_id = reassigned_roles
            .provider_driver_transparency
            .as_ref()
            .unwrap()
            .logs[0]
            .key_id
            .clone();
        reassigned_roles.provider_driver_release.key_ids = vec![log_key_id];
        reassigned_roles
            .provider_driver_transparency
            .as_mut()
            .unwrap()
            .logs[0]
            .key_id = release_key_id;
        let reassigned_roles = ValidatedDriverReleaseTrustRoot::new(reassigned_roles).unwrap();
        let error =
            validate_trust_root_transition(&historical, &previous, &reassigned_roles).unwrap_err();
        assert!(error.to_string().contains("between trust roles"));
    }

    #[tokio::test]
    async fn every_driver_selection_rechecks_current_key_revocation() {
        let fixture = Fixture::new();
        let database = Arc::new(
            Database::init(fixture.temporary.path().join("revocation.sqlite"))
                .await
                .unwrap(),
        );
        let store = CatalogStore::new(Arc::clone(&database));
        let registry = ProviderDriverReleaseRegistry::new(
            store.clone(),
            &fixture.install_root,
            fixture.trust_root.clone(),
            None,
        )
        .await
        .unwrap();
        let registered = registry
            .install_and_register(
                fixture.release.clone(),
                fixture.manifest.clone(),
                fixture.authorization.clone(),
                fixture.transparency.clone(),
                &fixture.archive,
            )
            .await
            .unwrap();
        let vault = Arc::new(CatalogProviderCredentialVault::new(
            store.clone(),
            Arc::new(SecretsBackend::Ready(Box::new(
                SecretCipher::from_key_bytes(&[31; 32]).unwrap(),
            ))),
        ));
        let credential = vault
            .provision(
                ProviderCredentialBindingSpec {
                    backend_id: "cloudflare-workers-pages".to_owned(),
                    name: "revocation-test".to_owned(),
                    account_id: None,
                },
                CredentialMaterial::new(b"revocation-test-credential".to_vec()).unwrap(),
                context("revocation-credential"),
            )
            .await
            .unwrap();
        let installation = store
            .register_provider_installation(
                ProviderInstallationSpec {
                    backend_id: "cloudflare-workers-pages".to_owned(),
                    account_reference: "revocation-account".to_owned(),
                    region: None,
                    credential_binding_id: credential.binding_id,
                },
                None,
            )
            .await
            .unwrap();
        let target = store
            .register_provider_target(
                ProviderTargetSpec {
                    installation_id: installation.installation_id,
                    name: "revocation-target".to_owned(),
                    standard_version: deis_wire::CURRENT_EDGE_CANON_STANDARD_VERSION.to_owned(),
                    target_config_digest: "d".repeat(64).parse().unwrap(),
                    driver_manifest: fixture.manifest.clone(),
                },
                None,
            )
            .await
            .unwrap();
        registry.resolve_for_target(target.target_id).await.unwrap();
        let expired_error = registry
            .resolve_for_target_at(target.target_id, Utc::now() + TimeDelta::days(2))
            .await
            .unwrap_err();
        assert!(
            expired_error
                .to_string()
                .contains("no current transparent authorization")
        );
        assert_eq!(
            registry
                .release(registered.release_digest.clone())
                .await
                .unwrap()
                .unwrap()
                .status,
            "active"
        );
        registry.resolve_for_target(target.target_id).await.unwrap();

        let mut equivocated_root = fixture.trust_root.get().clone();
        equivocated_root.expires_at += TimeDelta::hours(1);
        let equivocation_error = ProviderDriverReleaseRegistry::new(
            store.clone(),
            &fixture.install_root,
            ValidatedDriverReleaseTrustRoot::new(equivocated_root).unwrap(),
            None,
        )
        .await
        .err()
        .unwrap();
        assert!(
            equivocation_error
                .to_string()
                .contains("conflicting canonical content")
        );

        let mut revoked_root = fixture.trust_root.get().clone();
        revoked_root.root_version += 1;
        let revoked_release_key_id = revoked_root.provider_driver_release.key_ids[0].clone();
        let revoked_release_key = revoked_root
            .keys
            .iter_mut()
            .find(|key| key.key_id == revoked_release_key_id)
            .unwrap();
        revoked_release_key.status = DriverReleaseSigningKeyStatus::Revoked;
        revoked_release_key.revoked_at = Some(Utc::now());
        let missing_rotation_error = ProviderDriverReleaseRegistry::new(
            store.clone(),
            &fixture.install_root,
            ValidatedDriverReleaseTrustRoot::new(revoked_root.clone()).unwrap(),
            None,
        )
        .await
        .err()
        .unwrap();
        assert!(
            missing_rotation_error
                .to_string()
                .contains("requires an old-root/new-root rotation envelope")
        );
        let revoked_rotation = signed_root_rotation(revoked_root.clone());
        let revoked_registry = ProviderDriverReleaseRegistry::new(
            store,
            &fixture.install_root,
            ValidatedDriverReleaseTrustRoot::new(revoked_root).unwrap(),
            Some(revoked_rotation),
        )
        .await
        .unwrap();
        let error = revoked_registry
            .resolve_for_target(target.target_id)
            .await
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("no current transparent authorization")
        );
        assert_eq!(
            revoked_registry
                .release(registered.release_digest)
                .await
                .unwrap()
                .unwrap()
                .status,
            "active"
        );
        let mut skipped_root = revoked_registry.trust_root().get().clone();
        skipped_root.root_version += 2;
        let skipped_error = ProviderDriverReleaseRegistry::new(
            revoked_registry.store.clone(),
            &fixture.install_root,
            ValidatedDriverReleaseTrustRoot::new(skipped_root).unwrap(),
            None,
        )
        .await
        .err()
        .unwrap();
        assert!(skipped_error.to_string().contains("advance exactly once"));
        let mut reactivated_root = fixture.trust_root.get().clone();
        reactivated_root.root_version = 3;
        let reactivation_rotation = signed_root_rotation(reactivated_root.clone());
        let reactivation_error = ProviderDriverReleaseRegistry::new(
            revoked_registry.store.clone(),
            &fixture.install_root,
            ValidatedDriverReleaseTrustRoot::new(reactivated_root).unwrap(),
            Some(reactivation_rotation),
        )
        .await
        .err()
        .unwrap();
        assert!(
            reactivation_error
                .to_string()
                .contains("cannot reactivate a historically revoked key")
        );
        let rollback_error = ProviderDriverReleaseRegistry::new(
            revoked_registry.store.clone(),
            &fixture.install_root,
            fixture.trust_root.clone(),
            None,
        )
        .await
        .err()
        .unwrap();
        assert!(
            rollback_error
                .to_string()
                .contains("roll durable trust state backward")
        );

        let rotation_count: i64 = database
            .with_connection(|connection| {
                Ok::<i64, crate::database::DatabaseError>(connection.query_row(
                    "SELECT count(*) FROM provider_driver_trust_root_rotations",
                    [],
                    |row| row.get(0),
                )?)
            })
            .await
            .unwrap();
        assert_eq!(rotation_count, 1);
        assert!(
            database
                .with_connection(|connection| {
                    connection.execute(
                        "UPDATE provider_driver_trust_root_rotations SET observed_at = observed_at + 1",
                        [],
                    )?;
                    Ok::<(), crate::database::DatabaseError>(())
                })
                .await
                .is_err()
        );
        assert!(
            database
                .with_connection(|connection| {
                    connection.execute("DELETE FROM provider_driver_trust_root_rotations", [])?;
                    Ok::<(), crate::database::DatabaseError>(())
                })
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn target_resolution_quarantines_a_tree_that_changes_and_never_reactivates_it() {
        let fixture = Fixture::new();
        let database = Arc::new(
            Database::init(fixture.temporary.path().join("control.sqlite"))
                .await
                .unwrap(),
        );
        let store = CatalogStore::new(Arc::clone(&database));
        let registry = ProviderDriverReleaseRegistry::new(
            store.clone(),
            &fixture.install_root,
            fixture.trust_root.clone(),
            None,
        )
        .await
        .unwrap();
        let registered = registry
            .install_and_register(
                fixture.release.clone(),
                fixture.manifest.clone(),
                fixture.authorization.clone(),
                fixture.transparency.clone(),
                &fixture.archive,
            )
            .await
            .unwrap();
        let vault = Arc::new(CatalogProviderCredentialVault::new(
            store.clone(),
            Arc::new(SecretsBackend::Ready(Box::new(
                SecretCipher::from_key_bytes(&[17; 32]).unwrap(),
            ))),
        ));
        let credential = vault
            .provision(
                ProviderCredentialBindingSpec {
                    backend_id: "cloudflare-workers-pages".to_owned(),
                    name: "registry-test".to_owned(),
                    account_id: None,
                },
                CredentialMaterial::new(b"registry-test-credential".to_vec()).unwrap(),
                context("registry-credential"),
            )
            .await
            .unwrap();
        let installation = store
            .register_provider_installation(
                ProviderInstallationSpec {
                    backend_id: "cloudflare-workers-pages".to_owned(),
                    account_reference: "registry-account".to_owned(),
                    region: None,
                    credential_binding_id: credential.binding_id,
                },
                None,
            )
            .await
            .unwrap();
        let target = store
            .register_provider_target(
                ProviderTargetSpec {
                    installation_id: installation.installation_id,
                    name: "registry-target".to_owned(),
                    standard_version: deis_wire::CURRENT_EDGE_CANON_STANDARD_VERSION.to_owned(),
                    target_config_digest: "b".repeat(64).parse().unwrap(),
                    driver_manifest: fixture.manifest.clone(),
                },
                None,
            )
            .await
            .unwrap();
        let dispatcher = ProviderDriverDispatcher::new(
            registry.clone(),
            Arc::clone(&vault),
            Arc::new(deis_provider_driver::NoOperationInputs),
            crate::catalog::provider_payload::test_workspaces(),
            ExecutableProviderDriverConfig::default(),
        )
        .unwrap();
        let selected = dispatcher
            .driver_for_target(target.target_id)
            .await
            .unwrap();
        assert_eq!(
            selected.manifest().canonical_sha256().unwrap(),
            fixture.manifest.canonical_sha256().unwrap()
        );
        registry.resolve_for_target(target.target_id).await.unwrap();

        let installed_file = fixture.installed_file();
        let relative = fixture.contents.keys().next().unwrap();
        let original = fixture.contents[relative].clone();
        make_file_writable(&installed_file);
        fs::write(&installed_file, b"tampered driver bytes").unwrap();
        assert!(
            registry
                .resolve_for_target(target.target_id)
                .await
                .unwrap_err()
                .to_string()
                .contains("full-tree verification")
        );
        let quarantined = registry
            .release(registered.release_digest.clone())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(quarantined.status, "quarantined");
        assert_eq!(quarantined.revision, 2);
        assert_eq!(
            registry
                .audit_events(registered.release_digest.clone(), 10)
                .await
                .unwrap()
                .last()
                .unwrap()
                .event_type,
            "quarantined"
        );
        assert!(registry.resolve_for_target(target.target_id).await.is_err());

        fs::write(&installed_file, original).unwrap();
        make_installed_file_read_only(&installed_file);
        assert!(
            registry
                .register_existing(
                    fixture.release.clone(),
                    fixture.manifest.clone(),
                    fixture.authorization.clone(),
                    fixture.transparency.clone(),
                )
                .await
                .unwrap_err()
                .to_string()
                .contains("cannot be reactivated")
        );
    }

    fn context(key: &str) -> OperationRequestContext {
        OperationRequestContext::new(
            key,
            "POST\n/provider-driver-releases\nv1",
            "account:registry-test".to_owned(),
            CanonicalId::new_v7(),
            CanonicalId::new_v7(),
        )
        .unwrap()
    }

    fn digest_bytes(bytes: &[u8]) -> Sha256Digest {
        hex::encode(Sha256::digest(bytes)).parse().unwrap()
    }

    fn digest_jcs(value: &impl Serialize) -> Sha256Digest {
        digest_bytes(&serde_jcs::to_vec(value).unwrap())
    }

    fn bind_file_table(files: &mut [DriverReleaseFile], contents: &BTreeMap<String, Vec<u8>>) {
        for file in files {
            let content = &contents[&file.path];
            file.size_bytes = content.len() as u64;
            file.sha256.value = digest_bytes(content);
        }
    }

    fn bind_components(components: &mut [DriverReleaseComponent], files: &[DriverReleaseFile]) {
        for component in components {
            let paths = component.file_paths.as_ref().unwrap();
            let component_files = paths
                .iter()
                .map(|path| files.iter().find(|file| &file.path == path).unwrap())
                .collect::<Vec<_>>();
            component.materialization_digest.value = digest_jcs(&component_files);
            let executable = component.executable.as_mut().unwrap();
            executable.sha256 = files
                .iter()
                .find(|file| file.path == executable.path)
                .unwrap()
                .sha256
                .clone();
        }
    }

    fn write_archive(path: &Path, contents: &BTreeMap<String, Vec<u8>>) {
        let file = File::create(path).unwrap();
        let mut builder = tar::Builder::new(file);
        for (path, content) in contents {
            let mut header = tar::Header::new_gnu();
            header.set_entry_type(tar::EntryType::Regular);
            header.set_mode(0o755);
            header.set_size(content.len() as u64);
            header.set_cksum();
            builder
                .append_data(&mut header, path, content.as_slice())
                .unwrap();
        }
        builder.finish().unwrap();
        builder.into_inner().unwrap().sync_all().unwrap();
    }

    #[cfg(unix)]
    fn make_install_root_private(path: &Path) {
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
    }

    #[cfg(unix)]
    fn make_file_writable(path: &Path) {
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
    }

    #[cfg(unix)]
    fn make_installed_file_read_only(path: &Path) {
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(path, fs::Permissions::from_mode(0o555)).unwrap();
    }

    fn make_writable_for_cleanup(root: &Path) {
        let Ok(metadata) = fs::symlink_metadata(root) else {
            return;
        };
        if metadata.is_dir()
            && !metadata.file_type().is_symlink()
            && let Ok(entries) = fs::read_dir(root)
        {
            make_cleanup_entry_writable(root, true);
            for entry in entries.flatten() {
                make_writable_for_cleanup(&entry.path());
            }
        } else if metadata.is_file() {
            make_cleanup_entry_writable(root, false);
        }
    }

    #[cfg(unix)]
    fn make_cleanup_entry_writable(path: &Path, directory: bool) {
        use std::os::unix::fs::PermissionsExt as _;
        let mode = if directory { 0o700 } else { 0o600 };
        let _ = fs::set_permissions(path, fs::Permissions::from_mode(mode));
    }
}
