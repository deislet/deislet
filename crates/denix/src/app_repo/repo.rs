use super::types::{AppManifest, AppPackage, AppPackageStatus};
use anyhow::{Context, Result};
use arc_swap::ArcSwap;
use rusqlite::{params, Connection, OptionalExtension};
use std::collections::HashMap;
use std::io::{Cursor, Read};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::RwLock;
use tracing::{debug, info};

pub struct AppRepo {
    db_path: String,
    cache: Arc<ArcSwap<HashMap<String, AppPackage>>>,
    write_lock: Arc<RwLock<()>>,
}

impl AppRepo {
    pub async fn new(db_path: &str) -> Result<Self> {
        info!("Initializing AppRepo: {}", db_path);

        if let Some(parent) = std::path::Path::new(db_path).parent() {
            tokio::fs::create_dir_all(parent).await?;
        }

        let db_path_owned = db_path.to_string();
        let db_path_clone = db_path_owned.clone();

        tokio::task::spawn_blocking(move || {
            let conn = Connection::open(&db_path_clone)?;
            Self::init_schema(&conn)?;
            Ok::<_, anyhow::Error>(())
        })
        .await??;

        let cache = Self::load_all_metadata(&db_path_owned).await?;

        info!("AppRepo initialized, loaded {} apps", cache.len());

        Ok(Self {
            db_path: db_path_owned,
            cache: Arc::new(ArcSwap::from_pointee(cache)),
            write_lock: Arc::new(RwLock::new(())),
        })
    }

    fn init_schema(conn: &Connection) -> Result<()> {
        conn.execute(
            r#"
            CREATE TABLE IF NOT EXISTS apps (
                id TEXT PRIMARY KEY,
                manifest TEXT NOT NULL,
                package_size INTEGER NOT NULL,
                checksum TEXT NOT NULL,
                status TEXT NOT NULL,
                uploaded_at INTEGER NOT NULL,
                deployed_at INTEGER,
                updated_at INTEGER NOT NULL
            )
            "#,
            [],
        )?;

        conn.execute(
            r#"
            CREATE TABLE IF NOT EXISTS app_files (
                app_id TEXT NOT NULL,
                file_path TEXT NOT NULL,
                content BLOB NOT NULL,
                PRIMARY KEY (app_id, file_path),
                FOREIGN KEY (app_id) REFERENCES apps(id) ON DELETE CASCADE
            )
            "#,
            [],
        )?;

        conn.execute(
            "CREATE INDEX IF NOT EXISTS idx_apps_status ON apps(status)",
            [],
        )?;

        Ok(())
    }

    async fn load_all_metadata(db_path: &str) -> Result<HashMap<String, AppPackage>> {
        let db_path = db_path.to_string();

        tokio::task::spawn_blocking(move || {
            let conn = Connection::open(&db_path)?;
            let mut stmt = conn.prepare(
                "SELECT id, manifest, package_size, checksum, status,
                        uploaded_at, deployed_at, updated_at
                 FROM apps",
            )?;

            let apps = stmt
                .query_map([], |row| {
                    let id: String = row.get(0)?;
                    let manifest_json: String = row.get(1)?;
                    let package_size: i64 = row.get(2)?;
                    let checksum: String = row.get(3)?;
                    let status_str: String = row.get(4)?;
                    let uploaded_at_secs: i64 = row.get(5)?;
                    let deployed_at_secs: Option<i64> = row.get(6)?;
                    let updated_at_secs: i64 = row.get(7)?;

                    let manifest: AppManifest = serde_json::from_str(&manifest_json)
                        .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;

                    let status = AppPackageStatus::parse(&status_str).ok_or_else(|| {
                        rusqlite::Error::ToSqlConversionFailure(
                            format!("Unknown app status: {}", status_str).into(),
                        )
                    })?;

                    let uploaded_at = UNIX_EPOCH + std::time::Duration::from_secs(uploaded_at_secs as u64);
                    let deployed_at = deployed_at_secs
                        .map(|secs| UNIX_EPOCH + std::time::Duration::from_secs(secs as u64));
                    let updated_at = UNIX_EPOCH + std::time::Duration::from_secs(updated_at_secs as u64);

                    Ok((
                        id.clone(),
                        AppPackage {
                            id,
                            manifest,
                            package_size,
                            checksum,
                            status,
                            uploaded_at,
                            deployed_at,
                            updated_at,
                        },
                    ))
                })?
                .collect::<Result<HashMap<_, _>, _>>()?;

            Ok(apps)
        })
        .await?
    }

    pub async fn install_package(&self, id: String, zip_data: Vec<u8>) -> Result<AppPackage> {
        let _lock = self.write_lock.write().await;
        
        let db_path = self.db_path.clone();
        let id_clone = id.clone();
        let size = zip_data.len() as i64;
        
        let checksum = {
            use sha2::{Digest, Sha256};
            let mut hasher = Sha256::new();
            hasher.update(&zip_data);
            format!("{:x}", hasher.finalize())
        };

        let checksum_clone = checksum.clone();

        let package = tokio::task::spawn_blocking(move || {
            let mut conn = Connection::open(&db_path)?;
            let tx = conn.transaction()?;

            let reader = Cursor::new(&zip_data);
            let mut archive = zip::ZipArchive::new(reader)?;
            
            let mut manifest: Option<AppManifest> = None;
            let mut files: Vec<(String, Vec<u8>)> = Vec::new();

            for i in 0..archive.len() {
                let mut file = archive.by_index(i)?;
                if file.is_dir() {
                    continue;
                }

                let name = file.name().to_string();
                let mut content = Vec::new();
                file.read_to_end(&mut content)?;

                if name == "manifest.json" {
                    manifest = Some(serde_json::from_slice(&content)?);
                }

                files.push((name, content));
            }

            let manifest = manifest.ok_or_else(|| anyhow::anyhow!("manifest.json not found in package"))?;
            
            if manifest.id != id_clone {
                return Err(anyhow::anyhow!("App ID mismatch: manifest says {}, install request says {}", manifest.id, id_clone));
            }

            let package = AppPackage::new(id_clone.clone(), manifest.clone(), size, checksum_clone);

            let manifest_json = serde_json::to_string(&manifest)?;
            let uploaded_at_secs = package.uploaded_at.duration_since(UNIX_EPOCH)?.as_secs() as i64;
            let updated_at_secs = package.updated_at.duration_since(UNIX_EPOCH)?.as_secs() as i64;

            tx.execute(
                r#"
                INSERT OR REPLACE INTO apps
                (id, manifest, package_size, checksum, status, uploaded_at, updated_at)
                VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
                "#,
                params![
                    package.id,
                    manifest_json,
                    package.package_size,
                    package.checksum,
                    package.status.as_str(),
                    uploaded_at_secs,
                    updated_at_secs,
                ],
            )?;

            tx.execute("DELETE FROM app_files WHERE app_id = ?1", params![package.id])?;

            let mut stmt = tx.prepare("INSERT INTO app_files (app_id, file_path, content) VALUES (?1, ?2, ?3)")?;
            for (path, content) in files {
                stmt.execute(params![package.id, path, content])?;
            }
            drop(stmt);

            tx.commit()?;
            Ok(package)
        })
        .await??;

        self.cache.rcu(|cache| {
            let mut new_cache = (**cache).clone();
            new_cache.insert(id.clone(), package.clone());
            new_cache
        });

        info!("App installed: {}", id);
        Ok(package)
    }

    pub fn get_package(&self, id: &str) -> Option<AppPackage> {
        self.cache.load().get(id).cloned()
    }
    
    pub fn get_manifest(&self, id: &str) -> Option<AppManifest> {
        self.cache.load().get(id).map(|p| p.manifest.clone())
    }

    pub fn list_packages(&self) -> Vec<AppPackage> {
        self.cache.load().values().cloned().collect()
    }

    pub async fn get_file(&self, id: &str, path: &str) -> Result<Option<Vec<u8>>> {
        let db_path = self.db_path.clone();
        let id = id.to_string();
        let path = path.to_string();

        tokio::task::spawn_blocking(move || {
            let conn = Connection::open(&db_path)?;
            let mut stmt = conn.prepare("SELECT content FROM app_files WHERE app_id = ?1 AND file_path = ?2")?;
            let content: Option<Vec<u8>> = stmt
                .query_row(params![id, path], |row| row.get(0))
                .optional()?;
            Ok(content)
        })
        .await?
    }

    pub async fn update_status(&self, id: &str, status: AppPackageStatus) -> Result<()> {
        let mut package = self.get_package(id).with_context(|| format!("App not found: {}", id))?;

        match status {
            AppPackageStatus::Deployed => package.mark_deployed(),
            AppPackageStatus::Stopped => package.mark_stopped(),
            AppPackageStatus::Failed => package.mark_failed(),
            AppPackageStatus::Uploaded => {
                package.status = AppPackageStatus::Uploaded;
                package.updated_at = SystemTime::now();
            }
        }

        let _lock = self.write_lock.write().await;
        let db_path = self.db_path.clone();
        let pkg_clone = package.clone();

        let pkg_for_db = pkg_clone.clone();

        tokio::task::spawn_blocking(move || {
            let conn = Connection::open(&db_path)?;
            let updated_at_secs = pkg_for_db.updated_at.duration_since(UNIX_EPOCH)?.as_secs() as i64;
            let deployed_at_secs = pkg_for_db.deployed_at.map(|t| t.duration_since(UNIX_EPOCH).unwrap().as_secs() as i64);

            conn.execute(
                "UPDATE apps SET status = ?1, updated_at = ?2, deployed_at = ?3 WHERE id = ?4",
                params![pkg_for_db.status.as_str(), updated_at_secs, deployed_at_secs, pkg_for_db.id],
            )?;
            Ok::<_, anyhow::Error>(())
        })
        .await??;

        self.cache.rcu(move |cache| {
            let mut new_cache = (**cache).clone();
            new_cache.insert(pkg_clone.id.clone(), pkg_clone.clone());
            new_cache
        });

        debug!("App status updated: {} -> {:?}", id, status);
        Ok(())
    }

    pub async fn delete_package(&self, id: &str) -> Result<()> {
        let _lock = self.write_lock.write().await;
        let db_path = self.db_path.clone();
        let id_owned = id.to_string();

        let id_for_db = id_owned.clone();

        tokio::task::spawn_blocking(move || {
            let conn = Connection::open(&db_path)?;
            conn.execute("DELETE FROM apps WHERE id = ?1", params![id_for_db])?;
            Ok::<_, anyhow::Error>(())
        })
        .await??;

        self.cache.rcu(move |cache| {
            let mut new_cache = (**cache).clone();
            new_cache.remove(&id_owned);
            new_cache
        });

        info!("App deleted: {}", id);
        Ok(())
    }

    pub fn exists(&self, id: &str) -> bool {
        self.cache.load().contains_key(id)
    }

    pub fn stats(&self) -> AppRepoStats {
        let cache = self.cache.load();

        let mut stats = AppRepoStats {
            total: cache.len(),
            ..Default::default()
        };

        for package in cache.values() {
            match package.status {
                AppPackageStatus::Uploaded => stats.uploaded += 1,
                AppPackageStatus::Deployed => stats.deployed += 1,
                AppPackageStatus::Stopped => stats.stopped += 1,
                AppPackageStatus::Failed => stats.failed += 1,
            }
            stats.total_size += package.package_size;
        }

        stats
    }
}

#[derive(Debug, Default, Clone)]
pub struct AppRepoStats {
    pub total: usize,
    pub uploaded: usize,
    pub deployed: usize,
    pub stopped: usize,
    pub failed: usize,
    pub total_size: i64,
}
