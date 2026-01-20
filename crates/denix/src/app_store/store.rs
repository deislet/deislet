// 应用包存储实现

use super::types::{AppManifest, AppPackage, AppPackageStatus};
use anyhow::{Context, Result};
use arc_swap::ArcSwap;
use rusqlite::{params, Connection};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::RwLock;
use tracing::{debug, info, warn};

/// 应用包存储
pub struct AppStore {
    /// 数据库路径
    db_path: String,
    /// 内存缓存（应用 ID -> 应用包）
    cache: Arc<ArcSwap<HashMap<String, AppPackage>>>,
    /// 写操作锁
    write_lock: Arc<RwLock<()>>,
}

impl AppStore {
    /// 创建新的应用存储
    pub async fn new(db_path: &str) -> Result<Self> {
        info!("初始化应用存储: {}", db_path);

        // 确保数据库目录存在
        if let Some(parent) = std::path::Path::new(db_path).parent() {
            tokio::fs::create_dir_all(parent).await?;
        }

        // 初始化数据库
        let db_path_owned = db_path.to_string();
        let db_path_clone = db_path_owned.clone();

        tokio::task::spawn_blocking(move || {
            let conn = Connection::open(&db_path_clone)?;
            Self::init_schema(&conn)?;
            Ok::<_, anyhow::Error>(())
        })
        .await??;

        // 加载所有应用到缓存
        let cache = Self::load_all_apps(&db_path_owned).await?;

        info!("应用存储初始化完成，已加载 {} 个应用", cache.len());

        Ok(Self {
            db_path: db_path_owned,
            cache: Arc::new(ArcSwap::from_pointee(cache)),
            write_lock: Arc::new(RwLock::new(())),
        })
    }

    /// 初始化数据库表结构
    fn init_schema(conn: &Connection) -> Result<()> {
        conn.execute(
            r#"
            CREATE TABLE IF NOT EXISTS app_packages (
                id TEXT PRIMARY KEY,
                manifest TEXT NOT NULL,
                package_data BLOB NOT NULL,
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

        // 创建索引
        conn.execute(
            "CREATE INDEX IF NOT EXISTS idx_app_status ON app_packages(status)",
            [],
        )?;

        conn.execute(
            "CREATE INDEX IF NOT EXISTS idx_app_uploaded_at ON app_packages(uploaded_at)",
            [],
        )?;

        Ok(())
    }

    /// 从数据库加载所有应用
    async fn load_all_apps(db_path: &str) -> Result<HashMap<String, AppPackage>> {
        let db_path = db_path.to_string();

        tokio::task::spawn_blocking(move || {
            let conn = Connection::open(&db_path)?;
            let mut stmt = conn.prepare(
                "SELECT id, manifest, package_data, package_size, checksum, status,
                        uploaded_at, deployed_at, updated_at
                 FROM app_packages",
            )?;

            let apps = stmt
                .query_map([], |row| {
                    let id: String = row.get(0)?;
                    let manifest_json: String = row.get(1)?;
                    let package_data: Vec<u8> = row.get(2)?;
                    let package_size: i64 = row.get(3)?;
                    let checksum: String = row.get(4)?;
                    let status_str: String = row.get(5)?;
                    let uploaded_at_secs: i64 = row.get(6)?;
                    let deployed_at_secs: Option<i64> = row.get(7)?;
                    let updated_at_secs: i64 = row.get(8)?;

                    let manifest: AppManifest = serde_json::from_str(&manifest_json)
                        .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;

                    let status = AppPackageStatus::from_str(&status_str).ok_or_else(|| {
                        rusqlite::Error::ToSqlConversionFailure(
                            format!("未知的应用状态: {}", status_str).into(),
                        )
                    })?;

                    let uploaded_at =
                        UNIX_EPOCH + std::time::Duration::from_secs(uploaded_at_secs as u64);
                    let deployed_at = deployed_at_secs
                        .map(|secs| UNIX_EPOCH + std::time::Duration::from_secs(secs as u64));
                    let updated_at =
                        UNIX_EPOCH + std::time::Duration::from_secs(updated_at_secs as u64);

                    Ok((
                        id.clone(),
                        AppPackage {
                            id,
                            manifest,
                            package_data,
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

    /// 保存应用包
    pub async fn save_package(&self, package: AppPackage) -> Result<()> {
        let _lock = self.write_lock.write().await;

        let db_path = self.db_path.clone();
        let package_clone = package.clone();

        // 写入数据库
        tokio::task::spawn_blocking(move || {
            let conn = Connection::open(&db_path)?;

            let manifest_json = serde_json::to_string(&package_clone.manifest)?;
            let uploaded_at_secs = package_clone
                .uploaded_at
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_secs() as i64;
            let deployed_at_secs = package_clone
                .deployed_at
                .map(|t| t.duration_since(UNIX_EPOCH).unwrap().as_secs() as i64);
            let updated_at_secs = package_clone
                .updated_at
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_secs() as i64;

            conn.execute(
                r#"
                INSERT OR REPLACE INTO app_packages
                (id, manifest, package_data, package_size, checksum, status,
                 uploaded_at, deployed_at, updated_at)
                VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
                "#,
                params![
                    package_clone.id,
                    manifest_json,
                    package_clone.package_data,
                    package_clone.package_size,
                    package_clone.checksum,
                    package_clone.status.as_str(),
                    uploaded_at_secs,
                    deployed_at_secs,
                    updated_at_secs,
                ],
            )?;

            Ok::<_, anyhow::Error>(())
        })
        .await??;

        // 更新缓存
        let package_id = package.id.clone();
        let package_clone = package.clone();
        self.cache.rcu(|cache| {
            let mut new_cache = (**cache).clone();
            new_cache.insert(package_id.clone(), package_clone.clone());
            new_cache
        });

        debug!("应用包已保存");
        Ok(())
    }

    /// 获取应用包（零拷贝读取）
    pub fn get_package(&self, id: &str) -> Option<AppPackage> {
        let cache = self.cache.load();
        cache.get(id).cloned()
    }

    /// 获取应用清单
    pub fn get_manifest(&self, id: &str) -> Option<AppManifest> {
        let cache = self.cache.load();
        cache.get(id).map(|pkg| pkg.manifest.clone())
    }

    /// 列出所有应用
    pub fn list_packages(&self) -> Vec<AppPackage> {
        let cache = self.cache.load();
        cache.values().cloned().collect()
    }

    /// 列出指定状态的应用
    pub fn list_packages_by_status(&self, status: AppPackageStatus) -> Vec<AppPackage> {
        let cache = self.cache.load();
        cache
            .values()
            .filter(|pkg| pkg.status == status)
            .cloned()
            .collect()
    }

    /// 更新应用状态
    pub async fn update_status(&self, id: &str, status: AppPackageStatus) -> Result<()> {
        let mut package = self
            .get_package(id)
            .with_context(|| format!("应用不存在: {}", id))?;

        match status {
            AppPackageStatus::Deployed => package.mark_deployed(),
            AppPackageStatus::Stopped => package.mark_stopped(),
            AppPackageStatus::Failed => package.mark_failed(),
            AppPackageStatus::Uploaded => {
                package.status = AppPackageStatus::Uploaded;
                package.updated_at = SystemTime::now();
            }
        }

        self.save_package(package).await?;
        debug!("应用 {} 状态已更新为 {:?}", id, status);
        Ok(())
    }

    /// 删除应用包
    pub async fn delete_package(&self, id: &str) -> Result<()> {
        let _lock = self.write_lock.write().await;

        let db_path = self.db_path.clone();
        let id_owned = id.to_string();

        // 从数据库删除
        tokio::task::spawn_blocking(move || {
            let conn = Connection::open(&db_path)?;
            let rows_affected =
                conn.execute("DELETE FROM app_packages WHERE id = ?1", params![id_owned])?;

            if rows_affected == 0 {
                warn!("尝试删除不存在的应用: {}", id_owned);
            }

            Ok::<_, anyhow::Error>(())
        })
        .await??;

        // 从缓存删除
        let id_owned = id.to_string();
        self.cache.rcu(|cache| {
            let mut new_cache = (**cache).clone();
            new_cache.remove(&id_owned);
            new_cache
        });

        info!("应用包已删除: {}", id);
        Ok(())
    }

    /// 检查应用是否存在
    pub fn exists(&self, id: &str) -> bool {
        let cache = self.cache.load();
        cache.contains_key(id)
    }

    /// 获取统计信息
    pub fn stats(&self) -> AppStoreStats {
        let cache = self.cache.load();

        let mut stats = AppStoreStats::default();
        stats.total = cache.len();

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

/// 应用存储统计信息
#[derive(Debug, Default, Clone)]
pub struct AppStoreStats {
    /// 应用总数
    pub total: usize,
    /// 已上传
    pub uploaded: usize,
    /// 已部署
    pub deployed: usize,
    /// 已停止
    pub stopped: usize,
    /// 失败
    pub failed: usize,
    /// 总大小（字节）
    pub total_size: i64,
}
