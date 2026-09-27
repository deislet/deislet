//! Process wiring: open the three stores, register them on one gRPC server
//! behind one authentication check, and keep the background tasks - the KV
//! sweeper and the scheduled snapshots - running alongside.
//!
//! # There is one door, not three
//!
//! The capability token is checked by an interceptor applied in [`guarded`],
//! which is the only way a service is handed to the server below. A handler
//! therefore cannot forget the check, and a fourth service registered later
//! cannot become a fourth unguarded entrance without deleting the call. The
//! health service is registered without it on purpose - it names no tenant
//! and reads no data.
//!
//! # And it is behind TLS
//!
//! Every connection completes a TLS handshake presenting this store's
//! machine identity before tonic sees a byte of it (see
//! `deis_identity::accept_tls`). A caller checks that the certificate chains
//! to the fleet's trust root and names a deis-store, so the bearer capability
//! it then sends is neither readable on the wire nor handed to a machine that
//! merely answers at the store's address. There is no plaintext listener.

use crate::MAX_GRPC_MESSAGE_BYTES;
use crate::auth::StorageAuth;
use crate::backup;
use crate::blob::BlobService;
use crate::config::Config;
use crate::database::{DatabaseService, SqlLimits, TransactionLimits};
use crate::kv::KvService;
use crate::migration::StoreMigrationService;
use crate::proto::blob::blob_store_server::BlobStoreServer;
use crate::proto::storage::{database_server::DatabaseServer, kv_store_server::KvStoreServer};
use anyhow::{Context, Result};
use deis_backend_migration::proto::backend_migration_server::BackendMigrationServer;
use deis_backend_migration::{MigrationAuth, TenantWriteFences};
use std::future::Future;
use std::sync::Arc;
use tokio::net::TcpListener;
#[cfg(unix)]
use tokio::signal::unix::{SignalKind, signal};
use tokio::task::JoinHandle;
use tokio_stream::StreamExt;
use tokio_stream::wrappers::ReceiverStream;
use tonic::service::interceptor::InterceptedService;
use tonic::transport::Server;

/// Put a service behind the capability check.
///
/// Every service that can reach tenant data goes through here, so the check
/// is spelled once and applies to all of them - including any added later,
/// which is the point of the helper existing rather than three copies of the
/// same wrapping.
fn guarded<S>(service: S, auth: &StorageAuth) -> InterceptedService<S, StorageAuth> {
    InterceptedService::new(service, auth.clone())
}

/// The three stores, opened and ready to serve.
#[derive(Clone)]
pub struct Stores {
    pub kv: KvService,
    pub database: DatabaseService,
    pub blob: BlobService,
    pub(crate) fences: TenantWriteFences,
}

impl Stores {
    /// Open every store under `config.data_dir`, creating it if needed.
    pub async fn open(config: &Config) -> Result<Self> {
        std::fs::create_dir_all(&config.data_dir)
            .with_context(|| format!("cannot create data dir {}", config.data_dir.display()))?;
        let fences = TenantWriteFences::open(config.data_dir.join("migration-fences.sqlite"))
            .await
            .context("cannot open the storage migration fence journal")?;
        let kv = KvService::open_with_fences(
            &config.data_dir,
            config.shared_connections,
            config.durability,
            fences.clone(),
        )
        .await
        .context("cannot open the KV store")?;
        let database = DatabaseService::open_with_fences(
            &config.data_dir,
            config.tenant_connections,
            config.durability,
            fences.clone(),
        )
        .context("cannot open the database service")?;
        let blob = BlobService::open_with_fences(
            &config.data_dir,
            config.shared_connections,
            config.durability,
            config.blob_limits(),
            fences.clone(),
        )
        .await
        .context("cannot open the blob store")?;
        Ok(Self {
            kv,
            database,
            blob,
            fences,
        })
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

/// This store's machine identity and its client for the control plane's
/// machine channel, both from `[machine_identity]`.
///
/// Everything here is refused before a socket is bound: a missing table, a
/// certificate that does not name a store, and a control plane reached over
/// plaintext off this host - which could hand this store any signing key and
/// so decide which capabilities it honours.
pub fn machine_identity(
    config: &Config,
) -> Result<(
    Arc<deis_identity::MachineIdentity>,
    deis_capability::ControlClient,
)> {
    let identity_config = config.machine_identity.as_ref().context(
        "deis-store serves only over TLS with its own machine identity, and this configuration \
         has no [machine_identity] table; see deis-store.example.toml",
    )?;
    let identity = identity_config
        .load(deis_identity::ServiceKind::Store)
        .context("cannot load this store's machine identity")?;
    let control = identity_config
        .control_client(Arc::clone(&identity))
        .context("cannot reach the control plane's machine channel as configured")?;
    Ok((identity, control))
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
#[cfg(unix)]
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

#[cfg(not(unix))]
pub async fn shutdown_signal() {
    report_interrupt(tokio::signal::ctrl_c().await);
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
/// can learn the port before the server starts accepting. `keys` is the
/// control plane's key ring; whoever keeps it current (the binary starts a
/// JWKS refresher, tests fill it by hand) shares it with every guard here.
/// `identity` is the certificate every connection is served with.
pub async fn serve(
    config: Config,
    listener: TcpListener,
    shutdown: impl Future<Output = ()> + Send + 'static,
    keys: deis_capability::KeyRing,
    identity: Arc<deis_identity::MachineIdentity>,
) -> Result<()> {
    let addr = listener.local_addr().context("listener has no address")?;
    // One verifier behind all three guards, so the revocation list and the
    // degraded-mode clock the poller feeds are the ones every binding checks.
    let verifier = deis_capability::CapabilityVerifier::with_key_ring(keys).shared();
    let auth_kv = StorageAuth::new(verifier.clone(), "kv");
    let auth_sql = StorageAuth::new(verifier.clone(), "sql");
    let auth_blob = StorageAuth::new(verifier.clone(), "blob");
    deis_capability::fetch::poll_revocations_over_http(
        verifier.clone(),
        "store",
        &config.revocation_list_url,
    );
    if let Some(listen) = config.metrics_listen.as_deref() {
        let metrics = TcpListener::bind(listen)
            .await
            .with_context(|| format!("cannot bind the metrics listener on {listen}"))?;
        tracing::info!(
            address = %metrics.local_addr().context("metrics listener has no address")?,
            "Serving the capability verifier's metrics at /metrics"
        );
        let verifier = verifier.clone();
        deis_capability::exposition::serve_metrics(
            metrics,
            std::sync::Arc::new(move || {
                verifier
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .render_metrics("store")
            }),
        );
    }
    let migration_auth = match config.migration_admin_token.as_deref() {
        Some(token) => Some(
            MigrationAuth::new(token)
                .context("invalid migration_admin_token; the management service was not started")?,
        ),
        None => None,
    };
    let stores = Stores::open(&config).await?;
    let sweeper = spawn_kv_sweeper(stores.kv.clone(), config.kv_sweep_interval_secs);
    let snapshots = spawn_snapshots(&config);

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
    if migration_auth.is_some() {
        health_reporter
            .set_serving::<BackendMigrationServer<StoreMigrationService>>()
            .await;
    }

    let migration_service = match migration_auth {
        Some(auth) => {
            tracing::info!(
                "storage migration management enabled with independent bearer authentication"
            );
            Some(InterceptedService::new(
                BackendMigrationServer::new(StoreMigrationService::new(stores.clone()))
                    .max_decoding_message_size(MAX_GRPC_MESSAGE_BYTES)
                    .max_encoding_message_size(MAX_GRPC_MESSAGE_BYTES),
                auth,
            ))
        }
        None => {
            tracing::warn!(
                "migration_admin_token is not configured, so storage inventory and writer-fence RPCs are not registered"
            );
            None
        }
    };

    info_ready(
        &config,
        addr,
        &identity,
        &auth_kv,
        stores.database.transaction_limits(),
        stores.database.sql_limits(),
    );

    let result = Server::builder()
        .add_service(health_service)
        .add_service(guarded(
            KvStoreServer::new(stores.kv)
                .max_decoding_message_size(MAX_GRPC_MESSAGE_BYTES)
                .max_encoding_message_size(MAX_GRPC_MESSAGE_BYTES),
            &auth_kv,
        ))
        .add_service(guarded(
            DatabaseServer::new(stores.database)
                .max_decoding_message_size(MAX_GRPC_MESSAGE_BYTES)
                .max_encoding_message_size(MAX_GRPC_MESSAGE_BYTES),
            &auth_sql,
        ))
        .add_service(guarded(
            BlobStoreServer::new(stores.blob)
                .max_decoding_message_size(MAX_GRPC_MESSAGE_BYTES)
                .max_encoding_message_size(MAX_GRPC_MESSAGE_BYTES),
            &auth_blob,
        ))
        .add_optional_service(migration_service)
        .serve_with_incoming_shutdown(tls_incoming(listener, identity), shutdown)
        .await;

    // Nothing else keeps these alive, and leaving them running would hold a
    // connection to a database the process is done with - or start copying a
    // data directory nobody is serving from any more.
    sweeper.abort();
    if let Some(snapshots) = snapshots {
        snapshots.abort();
    }

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

/// Start the scheduled snapshots, or say why nothing is backing this up.
///
/// Returns `None` when no schedule is configured, which is the default. The
/// warning is at startup rather than on a timer because it describes a
/// standing state, and one line an operator can find beats a line an hour
/// that everyone learns to skip.
fn spawn_snapshots(config: &Config) -> Option<JoinHandle<()>> {
    match config.snapshot_schedule() {
        Some(schedule) => Some(backup::spawn(schedule)),
        None => {
            tracing::warn!(
                data_dir = %config.data_dir.display(),
                "no scheduled snapshots are configured, so this service never copies its own \
                 data; the data directory is the only copy of every tenant's KV, SQL and blob \
                 data, and it cannot be backed up by copying it while the service runs. Set \
                 snapshot_dir, or run `deis-store snapshot --out DIR` from outside"
            );
            None
        }
    }
}

/// Connections on `listener`, each one past its TLS handshake.
pub(crate) fn tls_incoming(
    listener: TcpListener,
    identity: Arc<deis_identity::MachineIdentity>,
) -> impl tokio_stream::Stream<
    Item = std::io::Result<
        deis_wire::served::Served<tokio_rustls::server::TlsStream<tokio::net::TcpStream>>,
    >,
> {
    ReceiverStream::new(deis_identity::accept_tls(listener, identity)).map(|accepted| {
        Ok(deis_wire::served::Served::new(
            accepted.stream,
            accepted.local_addr,
            accepted.remote_addr,
        ))
    })
}

fn info_ready(
    config: &Config,
    addr: std::net::SocketAddr,
    identity: &deis_identity::MachineIdentity,
    auth: &StorageAuth,
    transactions: TransactionLimits,
    sql: SqlLimits,
) {
    tracing::info!(
        %addr,
        identity = %identity.id(),
        tls = true,
        data_dir = %config.data_dir.display(),
        max_message_size = MAX_GRPC_MESSAGE_BYTES,
        max_object_bytes = config.max_object_bytes,
        upload_idle_timeout_secs = config.upload_idle_timeout_secs,
        download_idle_timeout_secs = config.download_idle_timeout_secs,
        kv_sweep_interval_secs = config.kv_sweep_interval_secs,
        // An interactive transaction pins one of a tenant's connections until
        // it ends, so these three are what an operator needs in front of them
        // when a `BeginTransaction` is refused or a handler reports that its
        // transaction was rolled back under it. They are the effective values,
        // after the per-tenant ceiling is clamped to the pool size.
        transaction_deadline_ms = transactions.deadline.as_millis(),
        max_open_transactions_per_tenant = transactions.max_open_per_tenant,
        max_open_transactions = transactions.max_open_total,
        // What one SQL request may cost. An operator reading that a tenant's
        // query was stopped part-way needs the number it was stopped against
        // on the same line an operator greps for, not in the source.
        statement_deadline_ms = sql.statement_deadline.as_millis(),
        max_result_rows = sql.max_result_rows,
        max_result_bytes = sql.max_result_bytes,
        // Whether callers are authenticated belongs on the line an operator
        // greps for first.
        authenticated = auth.is_enforcing(),
        "deis-store serving KvStore, Database and BlobStore"
    );
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

    fn identity_config(
        fleet: &deis_identity_test_pki::TestFleet,
        name: &str,
        id: &deis_identity::SpiffeId,
        control_endpoint: &str,
    ) -> deis_capability::ServiceIdentityConfig {
        let files = fleet.files_for(name, id);
        deis_capability::ServiceIdentityConfig {
            certificate: files.certificate,
            private_key: files.private_key,
            trust_root: files.trust_root,
            control_endpoint: control_endpoint.to_string(),
            identity_reload_seconds: 60,
        }
    }

    /// The keys a store honours capabilities against must come from the
    /// control plane and from nobody on the path, so a plaintext JWKS URL off
    /// this host is refused at startup rather than fetched.
    #[tokio::test]
    async fn a_plaintext_control_plane_off_this_host_is_refused_at_startup() {
        let fleet = deis_identity_test_pki::TestFleet::new();
        let store = deis_identity::names::store(&fleet.trust_domain, "s1").unwrap();
        for (endpoint, accepted) in [
            ("http://10.1.2.3:8101", false),
            ("http://control.internal:8101", false),
            ("http://127.0.0.1:8101", true),
            ("https://control.internal:8101", true),
        ] {
            let config = Config {
                machine_identity: Some(identity_config(&fleet, "store", &store, endpoint)),
                ..Config::default()
            };
            let outcome = machine_identity(&config);
            assert_eq!(outcome.is_ok(), accepted, "{endpoint}");
            if let Err(error) = outcome {
                assert!(
                    format!("{error:#}").contains("plain HTTP to another host"),
                    "the refusal must say why: {error:#}"
                );
            }
        }
    }

    #[tokio::test]
    async fn a_store_refuses_to_serve_without_an_identity_or_with_another_kinds() {
        assert!(machine_identity(&Config::default()).is_err());

        let fleet = deis_identity_test_pki::TestFleet::new();
        let queue = deis_identity::names::queue(&fleet.trust_domain, "q1").unwrap();
        let config = Config {
            machine_identity: Some(identity_config(
                &fleet,
                "queue",
                &queue,
                "https://127.0.0.1:8101",
            )),
            ..Config::default()
        };
        let error = machine_identity(&config).expect_err("a queue's certificate is not a store's");
        assert!(format!("{error:#}").contains("is not a Store"), "{error:#}");
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

    #[tokio::test]
    async fn migration_management_without_auth_secret_still_works() {
        // With capability-based auth, there is no auth_secret to check
        // against. The migration_admin_token is always independent.
        let dir = TempDir::new().unwrap();
        let secret = "an-independent-migration-secret".to_string();
        let config = Config {
            migration_admin_token: Some(secret),
            data_dir: dir.path().to_path_buf(),
            ..Config::default()
        };
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let fleet = deis_identity_test_pki::TestFleet::new();
        // Serve will just create the migration auth independently
        let result = serve(
            config,
            listener,
            async {},
            deis_capability::KeyRing::from_keys([ed25519_dalek::SigningKey::from_bytes(
                &[0u8; 32],
            )
            .verifying_key()]),
            fleet.service_identity(deis_identity::ServiceKind::Store),
        )
        .await;
        // This will fail on store open (empty data dir structure) but NOT on
        // the credential check - which is the point.
        let _ = result;
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

    /// The whole-node outage this pair of checks caused, driven end to end
    /// through the same calls a runtime makes.
    ///
    /// A call carrying `x-deis-binding` is stored under
    /// `<tenant>@<BINDING>`, so the first application on a node to use a
    /// named db or blob binding left `db/shop.production@MEDIA.sqlite` and
    /// `blob/shop.production@MEDIA/` behind. The next start of the same
    /// binary on the same data directory then refused - not for that tenant,
    /// for the process - and KV, SQL and blob went down for every tenant on
    /// the node, with "point data_dir at an empty directory" as the only
    /// remedy the message offered.
    #[tokio::test]
    async fn a_data_directory_holding_a_named_binding_opens_again() {
        let dir = TempDir::new().unwrap();
        let config = config_for(&dir);
        let media = TenantId::of_binding("shop.production", Some("MEDIA")).unwrap();
        assert_eq!(media.as_str(), "shop.production@MEDIA");

        {
            let stores = Stores::open(&config).await.unwrap();
            stores
                .database
                .exec_statement(&media, "CREATE TABLE t (a INTEGER)", &[])
                .await
                .unwrap();
            stores
                .kv
                .set(&media, "k", b"v".to_vec(), None, None)
                .await
                .unwrap();
            stores
                .blob
                .store_object(
                    &media,
                    "object",
                    None,
                    None,
                    Box::pin(tokio_stream::iter(vec![Ok(b"bytes".to_vec())])),
                )
                .await
                .unwrap();
        }

        // The names the binding actually put on disk, so this test keeps
        // meaning what it says if the layout is ever changed.
        assert!(
            dir.path()
                .join("db")
                .join("shop.production@MEDIA.sqlite")
                .is_file()
        );
        assert!(
            dir.path()
                .join("blob")
                .join("shop.production@MEDIA")
                .is_dir()
        );

        let stores = Stores::open(&config)
            .await
            .expect("a data directory holding a named binding must open again");
        // And the data is still the binding's own, not the tenant's unnamed
        // namespace: a check that "fixed" the refusal by rewriting the name
        // would pass the line above and fail here.
        assert_eq!(
            stores.kv.get(&media, "k").await.unwrap().map(|e| e.value),
            Some(b"v".to_vec())
        );
        assert!(stores.blob.head(&media, "object").await.unwrap().is_some());
    }

    /// A file this service never wrote must not be able to stop it. The
    /// refusal takes every tenant on the node offline, so it is reserved for
    /// evidence of pre-tenant data and never spent on a stray name.
    #[tokio::test]
    async fn a_stray_file_in_the_data_directory_does_not_stop_the_node() {
        let dir = TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join("db")).unwrap();
        std::fs::create_dir_all(dir.path().join("blob")).unwrap();
        std::fs::write(dir.path().join("db").join(".DS_Store"), b"").unwrap();
        std::fs::write(dir.path().join("db").join("app-one.sqlite.bak"), b"").unwrap();
        std::fs::create_dir(dir.path().join("blob").join("lost+found")).unwrap();
        std::fs::write(dir.path().join("blob").join("README"), b"").unwrap();

        Stores::open(&config_for(&dir))
            .await
            .expect("a stray name is not evidence that any tenant's data is unreachable");
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
