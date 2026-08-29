//! Process wiring: open the three stores, register them on one gRPC server,
//! and keep the KV sweeper running alongside.

use crate::MAX_GRPC_MESSAGE_BYTES;
use crate::blob::BlobService;
use crate::config::Config;
use crate::database::DatabaseService;
use crate::kv::KvService;
use crate::proto::blob::blob_store_server::BlobStoreServer;
use crate::proto::storage::{database_server::DatabaseServer, kv_store_server::KvStoreServer};
use anyhow::{Context, Result};
use std::future::Future;
use tokio::net::TcpListener;
use tokio::signal::unix::{SignalKind, signal};
use tokio::task::JoinHandle;
use tokio_stream::wrappers::TcpListenerStream;
use tonic::transport::Server;

/// The three stores, opened and ready to serve.
#[derive(Clone)]
pub struct Stores {
    pub kv: KvService,
    pub database: DatabaseService,
    pub blob: BlobService,
}

impl Stores {
    /// Open every store under `config.data_dir`, creating it if needed.
    pub async fn open(config: &Config) -> Result<Self> {
        std::fs::create_dir_all(&config.data_dir)
            .with_context(|| format!("cannot create data dir {}", config.data_dir.display()))?;
        let kv = KvService::open(&config.data_dir, config.shared_connections)
            .await
            .context("cannot open the KV store")?;
        let database = DatabaseService::open(&config.data_dir, config.tenant_connections)
            .context("cannot open the database service")?;
        let blob = BlobService::open(
            &config.data_dir,
            config.shared_connections,
            config.blob_limits(),
        )
        .await
        .context("cannot open the blob store")?;
        Ok(Self { kv, database, blob })
    }
}

/// Delete expired KV entries on a fixed interval until the handle is dropped.
///
/// Expiry is already enforced on every read, so this task only reclaims
/// space. It is therefore allowed to fail: a failed sweep is logged and
/// retried at the next tick.
pub fn spawn_kv_sweeper(kv: KvService, interval_secs: u64) -> JoinHandle<()> {
    // Zero would spin the loop with no delay at all.
    let period = std::time::Duration::from_secs(interval_secs.max(1));
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(period);
        // The first tick fires immediately; skip past it so startup is not
        // slowed by a full-table delete.
        ticker.tick().await;
        loop {
            ticker.tick().await;
            match kv.sweep_expired().await {
                Ok(0) => tracing::trace!("kv sweep found nothing expired"),
                Ok(removed) => {
                    tracing::debug!(removed, "kv sweep reclaimed expired entries")
                }
                Err(e) => tracing::warn!(
                    error = %e,
                    "kv sweep failed; expired rows stay on disk and the data directory keeps \
                     growing until a later sweep succeeds"
                ),
            }
        }
    })
}

/// Bind the address in `config`.
pub async fn bind(config: &Config) -> Result<TcpListener> {
    TcpListener::bind(&config.listen)
        .await
        .with_context(|| format!("cannot bind {}", config.listen))
}

/// Resolve when the process is asked to stop.
///
/// Both signals are honoured, because both are used: an operator pressing
/// Ctrl-C sends SIGINT, while a process supervisor (systemd, Docker,
/// Kubernetes - not deis-control, which never stops this process) stops a
/// container with SIGTERM. Listening for SIGINT alone would leave this
/// service killed outright by every such supervisor, skipping the drain
/// below, so in-flight KV writes, SQL transactions and blob uploads would be
/// lost.
pub async fn shutdown_signal() {
    let mut terminate = match signal(SignalKind::terminate()) {
        Ok(stream) => Some(stream),
        Err(e) => {
            tracing::error!(
                error = %e,
                "cannot listen for SIGTERM; a process supervisor stopping this service will \
                 kill it outright and in-flight requests will be lost"
            );
            None
        }
    };

    match terminate.as_mut() {
        Some(terminate) => {
            tokio::select! {
                result = tokio::signal::ctrl_c() => report_interrupt(result),
                _ = terminate.recv() => {
                    tracing::info!("SIGTERM received, draining in-flight requests");
                }
            }
        }
        None => report_interrupt(tokio::signal::ctrl_c().await),
    }
}

fn report_interrupt(result: std::io::Result<()>) {
    match result {
        Ok(()) => tracing::info!("interrupt received, draining in-flight requests"),
        Err(e) => tracing::error!(
            error = %e,
            "cannot listen for the interrupt signal; the process will only stop when killed, \
             which drops in-flight requests"
        ),
    }
}

/// Serve until `shutdown` resolves.
///
/// The listener is passed in already bound so a caller (the tests, for one)
/// can learn the port before the server starts accepting.
pub async fn serve(
    config: Config,
    listener: TcpListener,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> Result<()> {
    let addr = listener.local_addr().context("listener has no address")?;
    let stores = Stores::open(&config).await?;
    let sweeper = spawn_kv_sweeper(stores.kv.clone(), config.kv_sweep_interval_secs);

    // Health reporting is always on so the control plane has a uniform
    // liveness probe across every Deislet service. The stores are open by
    // the time this is set, so "serving" means requests can actually be
    // answered, not merely that the process started.
    let (mut health_reporter, health_service) = tonic_health::server::health_reporter();
    health_reporter
        .set_serving::<KvStoreServer<KvService>>()
        .await;
    health_reporter
        .set_serving::<DatabaseServer<DatabaseService>>()
        .await;
    health_reporter
        .set_serving::<BlobStoreServer<BlobService>>()
        .await;

    info_ready(&config, addr);

    let result = Server::builder()
        .add_service(health_service)
        .add_service(
            KvStoreServer::new(stores.kv)
                .max_decoding_message_size(MAX_GRPC_MESSAGE_BYTES)
                .max_encoding_message_size(MAX_GRPC_MESSAGE_BYTES),
        )
        .add_service(
            DatabaseServer::new(stores.database)
                .max_decoding_message_size(MAX_GRPC_MESSAGE_BYTES)
                .max_encoding_message_size(MAX_GRPC_MESSAGE_BYTES),
        )
        .add_service(
            BlobStoreServer::new(stores.blob)
                .max_decoding_message_size(MAX_GRPC_MESSAGE_BYTES)
                .max_encoding_message_size(MAX_GRPC_MESSAGE_BYTES),
        )
        .serve_with_incoming_shutdown(TcpListenerStream::new(listener), shutdown)
        .await;

    // Nothing else keeps the sweeper alive, and leaving it running would hold
    // a connection to a database the process is done with.
    sweeper.abort();

    match result {
        Ok(()) => {
            tracing::info!(%addr, "deis-store stopped serving");
            Ok(())
        }
        Err(e) => {
            tracing::error!(
                %addr,
                error = %e,
                "gRPC server terminated; every storage request now fails until it is restarted"
            );
            Err(anyhow::Error::new(e).context("gRPC server terminated"))
        }
    }
}

fn info_ready(config: &Config, addr: std::net::SocketAddr) {
    tracing::info!(
        %addr,
        data_dir = %config.data_dir.display(),
        max_message_size = MAX_GRPC_MESSAGE_BYTES,
        max_object_bytes = config.max_object_bytes,
        upload_idle_timeout_secs = config.upload_idle_timeout_secs,
        download_idle_timeout_secs = config.download_idle_timeout_secs,
        kv_sweep_interval_secs = config.kv_sweep_interval_secs,
        "deis-store serving KvStore, Database and BlobStore"
    );
    if !addr.ip().is_loopback() {
        tracing::warn!(
            %addr,
            "listening off the local host with no caller authentication; the tenant header is \
             taken at face value, so anything that can route to this port can read, overwrite and \
             delete any tenant's KV, SQL and blob data by naming its tenant id - including one \
             environment of an application reaching another. Bind a loopback address unless \
             caller identity has been added first"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tenant::TenantId;
    use tempfile::TempDir;

    fn config_for(dir: &TempDir) -> Config {
        Config {
            data_dir: dir.path().to_path_buf(),
            ..Config::default()
        }
    }

    #[tokio::test]
    async fn opening_creates_the_expected_layout() {
        let dir = TempDir::new().unwrap();
        let _stores = Stores::open(&config_for(&dir)).await.unwrap();
        assert!(dir.path().join("kv.sqlite").exists());
        assert!(dir.path().join("blob.sqlite").exists());
        assert!(dir.path().join("db").is_dir());
        assert!(dir.path().join("blob").is_dir());
    }

    /// A data directory written before the tenant change holds data nothing
    /// will look for again. Opening over it would create empty stores beside
    /// it and answer every read with "not found", so each of the three
    /// stores refuses to open instead. One case per store, because they are
    /// three separate pieces of evidence on disk.
    #[tokio::test]
    async fn a_data_directory_from_before_the_tenant_change_is_refused() {
        for (what, plant) in [
            ("kv.sqlite", plant_pre_tenant_kv as fn(&std::path::Path)),
            ("blob.sqlite", plant_pre_tenant_blob_metadata),
            ("db/<app>.sqlite", plant_pre_tenant_database_file),
            ("blob/<app>/", plant_pre_tenant_blob_directory),
        ] {
            let dir = TempDir::new().unwrap();
            plant(dir.path());
            let error = match Stores::open(&config_for(&dir)).await {
                Ok(_) => panic!("{what} must not be served over"),
                Err(e) => e,
            };
            // `{:#}` so the whole chain is searched: the outer context only
            // says which store failed to open.
            assert!(
                format!("{error:#}").contains("predates the tenant contract"),
                "the refusal for {what} must say what is wrong: {error:#}"
            );
        }
    }

    fn plant_pre_tenant_kv(data_dir: &std::path::Path) {
        let conn = rusqlite::Connection::open(data_dir.join("kv.sqlite")).unwrap();
        conn.execute_batch(
            "CREATE TABLE kv (
                 app_id     TEXT NOT NULL,
                 key        TEXT NOT NULL,
                 value      BLOB NOT NULL,
                 metadata   TEXT,
                 expires_at INTEGER,
                 PRIMARY KEY (app_id, key)
             )",
        )
        .unwrap();
    }

    fn plant_pre_tenant_blob_metadata(data_dir: &std::path::Path) {
        let conn = rusqlite::Connection::open(data_dir.join("blob.sqlite")).unwrap();
        conn.execute_batch(
            "CREATE TABLE blob (
                 app_id       TEXT NOT NULL,
                 key          TEXT NOT NULL,
                 sha256       TEXT NOT NULL,
                 size         INTEGER NOT NULL,
                 content_type TEXT,
                 created_at   INTEGER NOT NULL,
                 PRIMARY KEY (app_id, key)
             )",
        )
        .unwrap();
    }

    fn plant_pre_tenant_database_file(data_dir: &std::path::Path) {
        std::fs::create_dir_all(data_dir.join("db")).unwrap();
        std::fs::write(data_dir.join("db").join("app-one.sqlite"), b"").unwrap();
    }

    fn plant_pre_tenant_blob_directory(data_dir: &std::path::Path) {
        std::fs::create_dir_all(data_dir.join("blob").join("app-one")).unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn the_sweeper_reclaims_expired_entries() {
        let dir = TempDir::new().unwrap();
        let stores = Stores::open(&config_for(&dir)).await.unwrap();
        let tenant = TenantId::parse("app-one.production").unwrap();
        stores
            .kv
            .set(
                &tenant,
                "stale",
                b"v".to_vec(),
                None,
                Some(crate::unix_seconds() - 1),
            )
            .await
            .unwrap();

        let sweeper = spawn_kv_sweeper(stores.kv.clone(), 1);
        // Paused time: advancing the clock fires the ticker without waiting.
        tokio::time::advance(std::time::Duration::from_secs(3)).await;
        tokio::task::yield_now().await;

        // The row is gone, so a second sweep finds nothing left to do.
        let mut remaining = stores.kv.sweep_expired().await.unwrap();
        for _ in 0..10 {
            if remaining == 0 {
                break;
            }
            tokio::task::yield_now().await;
            remaining = stores.kv.sweep_expired().await.unwrap();
        }
        assert_eq!(remaining, 0);
        sweeper.abort();
    }
}
