//! Verified, host-specific provider driver releases.
//!
//! A manifest digest is only a claim about intended bytes. This registry is
//! the durable bridge from that claim to one complete installation tree that
//! the current Control host has actually verified.

use std::{path::Path, sync::Arc};

use chrono::{DateTime, Utc};
use deis_provider_driver::{
    DriverInstallationError, DriverReleaseInstaller, ExecutableProviderDriver,
    ExecutableProviderDriverConfig, HostOperatingSystem, InstalledDriverRelease,
    ProviderCredentialBroker, Sha256Digest, ValidatedProviderDriverManifest,
    ValidatedProviderDriverRelease,
};
use deis_wire::CanonicalId;
use rusqlite::{OptionalExtension, params};

use super::model::now_secs;
use super::store::CatalogStore;
use super::{CatalogError, CatalogProviderCredentialVault, CatalogResult};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProviderDriverReleaseRecord {
    pub release_digest: Sha256Digest,
    pub host_operating_system: HostOperatingSystem,
    pub driver_contract_version: String,
    pub backend_id: String,
    pub driver_id: String,
    pub driver_version: String,
    pub manifest_digest: Sha256Digest,
    pub archive_digest: Sha256Digest,
    pub archive_size_bytes: u64,
    pub tree_digest: Sha256Digest,
    pub installation_dir_name: String,
    pub status: String,
    pub revision: u64,
    pub verified_at: DateTime<Utc>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProviderDriverReleaseAuditEvent {
    pub event_id: CanonicalId,
    pub release_digest: Sha256Digest,
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
}

#[derive(Clone)]
pub struct ProviderDriverDispatcher {
    registry: ProviderDriverReleaseRegistry,
    credential_broker: Arc<dyn ProviderCredentialBroker>,
    process_config: ExecutableProviderDriverConfig,
}

#[derive(Clone, Debug)]
pub struct ResolvedProviderDriverRelease {
    pub record: ProviderDriverReleaseRecord,
    pub manifest: ValidatedProviderDriverManifest,
    pub release: ValidatedProviderDriverRelease,
    pub installed: InstalledDriverRelease,
}

impl ProviderDriverReleaseRegistry {
    pub fn new(store: CatalogStore, installation_root: impl AsRef<Path>) -> CatalogResult<Self> {
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
        Ok(Self {
            store,
            installer,
            host,
        })
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
    pub fn belongs_to(&self, store: &CatalogStore) -> bool {
        Arc::ptr_eq(&self.store.database(), &store.database())
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

    pub async fn audit_events(
        &self,
        release_digest: Sha256Digest,
        limit: usize,
    ) -> CatalogResult<Vec<ProviderDriverReleaseAuditEvent>> {
        if !(1..=1_000).contains(&limit) {
            return Err(CatalogError::invalid(
                "provider driver release audit limit must be between 1 and 1000",
            ));
        }
        let database = self.store.database();
        let host = self.host;
        database
            .with_connection(move |connection| {
                let mut statement = connection.prepare(
                    "SELECT event_id, event_type, detail, observed_at
                       FROM provider_driver_release_audit
                      WHERE release_sha256 = ?1 AND host_operating_system = ?2
                      ORDER BY observed_at, event_id LIMIT ?3",
                )?;
                statement
                    .query_map(
                        params![
                            release_digest.as_str(),
                            host.as_str(),
                            i64::try_from(limit).map_err(|_| {
                                CatalogError::invalid(
                                    "provider driver release audit limit exceeds SQLite range",
                                )
                            })?,
                        ],
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
                    .collect()
            })
            .await
    }

    pub async fn install_and_register(
        &self,
        release: ValidatedProviderDriverRelease,
        manifest: ValidatedProviderDriverManifest,
        archive_path: impl AsRef<Path>,
    ) -> CatalogResult<ProviderDriverReleaseRecord> {
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
        self.register_verified(&release, &manifest, &installed)
            .await
    }

    pub async fn register_existing(
        &self,
        release: ValidatedProviderDriverRelease,
        manifest: ValidatedProviderDriverManifest,
    ) -> CatalogResult<ProviderDriverReleaseRecord> {
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
        self.register_verified(&release, &manifest, &installed)
            .await
    }

    pub async fn resolve_for_target(
        &self,
        target_id: CanonicalId,
    ) -> CatalogResult<ResolvedProviderDriverRelease> {
        let stored = load_release_for_target(&self.store, target_id, self.host).await?;
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
        };
        persist_verified_release(&self.store, facts).await
    }
}

impl ProviderDriverDispatcher {
    pub fn new(
        registry: ProviderDriverReleaseRegistry,
        credential_vault: Arc<CatalogProviderCredentialVault>,
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
        let process_config = self.process_config;
        tokio::task::spawn_blocking(move || {
            ExecutableProviderDriver::new(
                resolved.manifest,
                resolved.installed,
                credential_broker,
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
}

struct StoredRelease {
    record: ProviderDriverReleaseRecord,
    manifest: ValidatedProviderDriverManifest,
    release: ValidatedProviderDriverRelease,
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
            insert_audit(
                &transaction,
                &facts.release_digest,
                facts.host,
                event_type,
                "complete installation tree verified against canonical release index",
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
                        AND i.status = 'active' AND r.status = 'active'",
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
            })
        })
        .await
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
    };
    persist_verified_release(store, facts).await.unwrap();
}

#[cfg(test)]
mod tests {
    use std::{
        collections::BTreeMap,
        fs::{self, File},
        path::{Path, PathBuf},
        sync::Arc,
    };

    use deis_provider_driver::{
        CredentialMaterial, DriverReleaseComponent, DriverReleaseFile, ProviderDriver,
        ProviderDriverManifest, ProviderDriverRelease,
    };
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

    struct Fixture {
        temporary: TempDir,
        install_root: PathBuf,
        archive: PathBuf,
        release: ValidatedProviderDriverRelease,
        manifest: ValidatedProviderDriverManifest,
        contents: BTreeMap<String, Vec<u8>>,
    }

    impl Fixture {
        fn new() -> Self {
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
                    let byte = if file.path.contains("driver") {
                        b'd'
                    } else {
                        b'w'
                    };
                    (
                        file.path.clone(),
                        vec![byte; if byte == b'd' { 128 } else { 256 }],
                    )
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
            let manifest = ValidatedProviderDriverManifest::new(manifest).unwrap();

            Self {
                temporary,
                install_root,
                archive,
                release,
                manifest,
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
        let registry = ProviderDriverReleaseRegistry::new(store, &fixture.install_root).unwrap();

        assert!(
            registry
                .register_existing(fixture.release.clone(), fixture.manifest.clone())
                .await
                .unwrap_err()
                .to_string()
                .contains("verification failed")
        );
        let registered = registry
            .install_and_register(
                fixture.release.clone(),
                fixture.manifest.clone(),
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
            .register_existing(fixture.release.clone(), fixture.manifest.clone())
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
                    connection.execute("DELETE FROM provider_driver_release_audit", [])?;
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
        let registry =
            ProviderDriverReleaseRegistry::new(store.clone(), &fixture.install_root).unwrap();
        let registered = registry
            .install_and_register(
                fixture.release.clone(),
                fixture.manifest.clone(),
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
                },
                CredentialMaterial::new(b"registry-test-credential".to_vec()).unwrap(),
                context("registry-credential"),
            )
            .await
            .unwrap();
        let installation = store
            .register_provider_installation(ProviderInstallationSpec {
                backend_id: "cloudflare-workers-pages".to_owned(),
                account_reference: "registry-account".to_owned(),
                region: None,
                credential_binding_id: credential.binding_id,
            })
            .await
            .unwrap();
        let target = store
            .register_provider_target(ProviderTargetSpec {
                installation_id: installation.installation_id,
                name: "registry-target".to_owned(),
                standard_version: "edge-canon-proposal-0001".to_owned(),
                target_config_digest: "b".repeat(64).parse().unwrap(),
                driver_manifest: fixture.manifest.clone(),
            })
            .await
            .unwrap();
        let dispatcher = ProviderDriverDispatcher::new(
            registry.clone(),
            Arc::clone(&vault),
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
                .register_existing(fixture.release.clone(), fixture.manifest.clone())
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

    #[cfg(not(unix))]
    fn make_install_root_private(_path: &Path) {}

    #[cfg(unix)]
    fn make_file_writable(path: &Path) {
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
    }

    #[cfg(windows)]
    fn make_file_writable(path: &Path) {
        let mut permissions = fs::metadata(path).unwrap().permissions();
        #[allow(clippy::permissions_set_readonly_false)]
        permissions.set_readonly(false);
        fs::set_permissions(path, permissions).unwrap();
    }

    #[cfg(unix)]
    fn make_installed_file_read_only(path: &Path) {
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(path, fs::Permissions::from_mode(0o555)).unwrap();
    }

    #[cfg(windows)]
    fn make_installed_file_read_only(path: &Path) {
        let mut permissions = fs::metadata(path).unwrap().permissions();
        permissions.set_readonly(true);
        fs::set_permissions(path, permissions).unwrap();
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

    #[cfg(windows)]
    fn make_cleanup_entry_writable(path: &Path, _directory: bool) {
        if let Ok(metadata) = fs::metadata(path) {
            let mut permissions = metadata.permissions();
            #[allow(clippy::permissions_set_readonly_false)]
            permissions.set_readonly(false);
            let _ = fs::set_permissions(path, permissions);
        }
    }
}
