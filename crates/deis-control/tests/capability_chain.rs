//! The capability chain across process boundaries, and across a restart.
//!
//! Every other test of capabilities holds both ends in one process, with a
//! signing key the test made up. This one runs the real `deis-control` binary
//! as a child process and real deis-store and deis-queue servers over TCP,
//! and connects them only the way an installation does:
//!
//! * the verifiers start with an empty key ring and their own machine
//!   identity, before the control plane is even up, and learn its keys by
//!   fetching its JWKS from the machine channel under mutual TLS
//!   (`deis_capability::fetch`, the same refresher their binaries start,
//!   built by the same startup step);
//! * every call to them is TLS, checked the way a real caller checks it: the
//!   certificate must chain to the fleet's root and name that kind of service;
//! * a credential is obtained the way `deis kv` obtains one - log in with the
//!   bootstrap token the control plane wrote into its data directory, then
//!   `GET .../storage-token` - so the bindings and audiences are whatever the
//!   real issuance path writes, not what a test chose;
//! * the control plane is then killed and started again on the same data
//!   directory and master key, which is the case that used to break the whole
//!   fleet: an ephemeral issuer signed with a new key after every restart.
//!
//! After the restart the keys it publishes must be the ones it published
//! before, tokens minted before must still be accepted, and a token minted
//! after must be accepted by verifiers that have never seen the control plane
//! before - a fresh store and queue started only now. Tokens are also shown
//! not to reach past what they were minted for: another tenant, or the other
//! service's audience and bindings.

use std::net::{Ipv4Addr, SocketAddr, TcpListener as StdTcpListener};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use deis_capability::KeyRing;
use deis_capability::fetch::{RefreshPolicy, spawn_refresher};
use deis_identity::{Protocol, ServiceKind, ServiceTrust, names};
use deis_identity_test_pki::{TestCa, write_files};
use deis_queue::proto::queue::{ListDeadLettersRequest, SendRequest, queue_client::QueueClient};
use deis_store::proto::blob::{ListRequest as BlobListRequest, blob_store_client::BlobStoreClient};
use deis_store::proto::storage::{
    ExecuteRequest, GetRequest, SetRequest, database_client::DatabaseClient,
    kv_store_client::KvStoreClient,
};
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::sync::oneshot;
use tonic::transport::Channel;
use tonic::{Code, Request};

const TRUST_DOMAIN: &str = "deis.internal";
const APP: &str = "shop";
const ENV: &str = "production";
const TENANT: &str = "shop.production";

/// How long anything here may take to come up or converge.
const DEADLINE: Duration = Duration::from_secs(30);

/// A port nothing is listening on right now. The window between dropping the
/// probe and the child binding it is small, and nothing else in this test
/// binds by number.
fn free_port() -> u16 {
    StdTcpListener::bind((Ipv4Addr::LOCALHOST, 0))
        .and_then(|listener| listener.local_addr())
        .map(|addr| addr.port())
        .expect("a free port")
}

/// The files a control plane needs on disk: its configuration, its master
/// key and its machine identity. Kept across the restart - that the second
/// start reads exactly what the first wrote is the point.
struct Installation {
    dir: TempDir,
    config: PathBuf,
    port: u16,
    machine_port: u16,
    ca: TestCa,
}

impl Installation {
    fn new(store: SocketAddr, queue: SocketAddr) -> Self {
        let dir = TempDir::new().expect("a directory");
        let root = dir.path();
        let port = free_port();
        let machine_port = free_port();

        let ca = TestCa::new();
        let control_id = names::control(TRUST_DOMAIN).expect("a control name");
        let identity = write_files(
            root,
            "control",
            &ca.issue_with_addresses(&control_id, &[Ipv4Addr::LOCALHOST.into()]),
            &ca.root_pem,
        );

        // An installer creates the data directory; the control plane refuses
        // to invent one for its database.
        std::fs::create_dir_all(root.join("data")).expect("a data directory");

        let master_key = root.join("master.key");
        std::fs::write(&master_key, "7f".repeat(32)).expect("write master key");

        let config = root.join("control.toml");
        std::fs::write(
            &config,
            format!(
                r#"bind_addr = "127.0.0.1"
port = {port}
heartbeat_interval = 30
heartbeat_timeout = 90
data_dir = "{data}"
bundle_dir = "{bundles}"
secret_key_file = "{key}"
store_endpoint = "https://{store}"
queue_endpoint = "https://{queue}"

[machine]
bind_addr = "127.0.0.1"
port = {machine_port}
certificate = "{cert}"
private_key = "{private_key}"
trust_root = "{trust_root}"
"#,
                data = root.join("data").display(),
                bundles = root.join("bundles").display(),
                key = master_key.display(),
                cert = identity.certificate.display(),
                private_key = identity.private_key.display(),
                trust_root = identity.trust_root.display(),
            ),
        )
        .expect("write config");

        Self {
            dir,
            config,
            port,
            machine_port,
            ca,
        }
    }

    fn base_url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    /// The machine channel, where the JWKS is served and nowhere else.
    fn machine_url(&self) -> String {
        format!("https://127.0.0.1:{}", self.machine_port)
    }

    /// A data service's `[machine_identity]`: a certificate this
    /// installation's authority issued for `name`, and the machine channel.
    fn service_identity(
        &self,
        file: &str,
        name: &deis_identity::SpiffeId,
    ) -> deis_capability::ServiceIdentityConfig {
        let files = write_files(
            self.dir.path(),
            file,
            &self.ca.issue(name),
            &self.ca.root_pem,
        );
        deis_capability::ServiceIdentityConfig {
            certificate: files.certificate,
            private_key: files.private_key,
            trust_root: files.trust_root,
            control_endpoint: self.machine_url(),
            identity_reload_seconds: 60,
        }
    }

    /// What a caller holding only this installation's root trusts.
    fn trust(&self) -> ServiceTrust {
        let root = self.dir.path().join("fleet-root.pem");
        std::fs::write(&root, &self.ca.root_pem).expect("write root");
        ServiceTrust::from_trust_root(&root).expect("the root loads")
    }

    fn bootstrap_file(&self) -> PathBuf {
        self.dir.path().join("data").join("bootstrap.token")
    }

    fn log(&self, run: usize) -> PathBuf {
        self.dir.path().join(format!("control-{run}.log"))
    }

    /// Start the real binary and wait until it answers `/health`.
    async fn start(&self, run: usize) -> ControlProcess {
        let log = std::fs::File::create(self.log(run)).expect("a log file");
        let child = Command::new(env!("CARGO_BIN_EXE_deis-control"))
            .arg("--config")
            .arg(&self.config)
            .env("RUST_LOG", "info")
            .stdout(Stdio::from(log.try_clone().expect("clone log")))
            .stderr(Stdio::from(log))
            .spawn()
            .expect("deis-control starts");
        let mut process = ControlProcess { child };

        let client = reqwest::Client::new();
        let health = format!("{}/health", self.base_url());
        let started = Instant::now();
        loop {
            if let Ok(Some(status)) = process.child.try_wait() {
                panic!(
                    "deis-control exited during startup ({status}):\n{}",
                    read(&self.log(run))
                );
            }
            if let Ok(response) = client.get(&health).send().await
                && response.status().is_success()
            {
                return process;
            }
            assert!(
                started.elapsed() < DEADLINE,
                "deis-control never answered /health:\n{}",
                read(&self.log(run))
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }
}

fn read(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_default()
}

/// Killed on drop, so a failing assertion never leaves a control plane
/// running behind the test.
struct ControlProcess {
    child: Child,
}

impl ControlProcess {
    /// SIGKILL, not a graceful stop: nothing the issuer needs may depend on
    /// a shutdown path running.
    fn kill(mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for ControlProcess {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Refresher timings short enough for a test; the logic is the production
/// one.
fn quick_refresh() -> RefreshPolicy {
    RefreshPolicy {
        periodic: Duration::from_secs(300),
        min_on_demand_gap: Duration::from_millis(200),
        initial_backoff: Duration::from_millis(100),
        max_backoff: Duration::from_millis(500),
        request_timeout: Duration::from_secs(5),
    }
}

/// A deis-store or deis-queue server over a real socket, keyed only by what
/// it fetches from the control plane's machine channel.
struct Service {
    _data: TempDir,
    addr: SocketAddr,
    kind: ServiceKind,
    trust: ServiceTrust,
    _shutdown: oneshot::Sender<()>,
}

impl Service {
    async fn store(
        listener: tokio::net::TcpListener,
        installation: &Installation,
        id: &str,
    ) -> Self {
        let data = TempDir::new().expect("a directory");
        let addr = listener.local_addr().expect("an address");
        let name = names::store(TRUST_DOMAIN, id).expect("a store name");
        let config = deis_store::config::Config {
            listen: addr.to_string(),
            data_dir: data.path().to_path_buf(),
            machine_identity: Some(installation.service_identity(id, &name)),
            ..deis_store::config::Config::default()
        };
        // The binary's own startup step: identity, and a control-plane client
        // that proves it.
        let (identity, control) =
            deis_store::server::machine_identity(&config).expect("the store's identity loads");
        let keys = KeyRing::new();
        spawn_refresher(keys.clone(), control, quick_refresh());
        let (shutdown, rx) = oneshot::channel::<()>();
        tokio::spawn(async move {
            deis_store::server::serve(
                config,
                listener,
                async {
                    let _ = rx.await;
                },
                keys,
                identity,
            )
            .await
            .expect("deis-store serves");
        });
        Self {
            _data: data,
            addr,
            kind: ServiceKind::Store,
            trust: installation.trust(),
            _shutdown: shutdown,
        }
    }

    async fn queue(
        listener: tokio::net::TcpListener,
        installation: &Installation,
        id: &str,
    ) -> Self {
        let data = TempDir::new().expect("a directory");
        let addr = listener.local_addr().expect("an address");
        let name = names::queue(TRUST_DOMAIN, id).expect("a queue name");
        let config = deis_queue::config::Config {
            listen: addr.to_string(),
            data_dir: data.path().to_path_buf(),
            machine_identity: Some(installation.service_identity(id, &name)),
            ..deis_queue::config::Config::default()
        };
        let (identity, control) =
            deis_queue::server::machine_identity(&config).expect("the queue's identity loads");
        let keys = KeyRing::new();
        spawn_refresher(keys.clone(), control, quick_refresh());
        let (shutdown, rx) = oneshot::channel::<()>();
        tokio::spawn(async move {
            deis_queue::server::serve(
                config,
                listener,
                async {
                    let _ = rx.await;
                },
                keys,
                identity,
            )
            .await
            .expect("deis-queue serves");
        });
        Self {
            _data: data,
            addr,
            kind: ServiceKind::Queue,
            trust: installation.trust(),
            _shutdown: shutdown,
        }
    }

    /// A TLS channel that accepts only a certificate this installation's
    /// authority issued for this kind of service.
    async fn channel(&self) -> Channel {
        tonic::transport::Endpoint::from_shared(format!("https://{}", self.addr))
            .expect("a URI")
            .connect_with_connector(self.trust.dialler(self.kind, &[Protocol::Http2]))
            .await
            .expect("connects")
    }
}

async fn local_listener() -> tokio::net::TcpListener {
    tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
        .await
        .expect("a listener")
}

/// `message` carrying `token`. The tenant is the one the token names; the
/// label is only here so a reader sees which tenant a call is meant for.
fn authed<T>(_tenant: &str, token: &str, message: T) -> Request<T> {
    let mut request = Request::new(message);
    request.metadata_mut().insert(
        "authorization",
        format!("Bearer {token}").parse().expect("a header value"),
    );
    request
}

/// The human API, as the CLI speaks it.
struct Api {
    client: reqwest::Client,
    base: String,
    token: String,
}

impl Api {
    async fn call(&self, method: reqwest::Method, path: &str, body: Option<Value>) -> Value {
        let mut request = self
            .client
            .request(method.clone(), format!("{}/api/v1{path}", self.base))
            .bearer_auth(&self.token);
        if let Some(body) = body {
            request = request.json(&body);
        }
        let response = request.send().await.expect("the control plane answers");
        let status = response.status();
        let text = response.text().await.expect("a body");
        assert!(
            status.is_success(),
            "{method} {path} answered {status}: {text}"
        );
        serde_json::from_str(&text).unwrap_or(Value::Null)
    }

    /// Trade the one-time bootstrap credential for a standing one, exactly
    /// as `deis login --bootstrap` does.
    async fn log_in(installation: &Installation) -> Self {
        let bootstrap = std::fs::read_to_string(installation.bootstrap_file())
            .expect("the first start writes a bootstrap token")
            .trim()
            .to_string();
        let mut api = Self {
            client: reqwest::Client::new(),
            base: installation.base_url(),
            token: bootstrap,
        };
        let minted = api
            .call(
                reqwest::Method::POST,
                "/accounts/bootstrap/tokens",
                Some(json!({"name": "owner", "unscoped": true})),
            )
            .await;
        api.token = minted["secret"]
            .as_str()
            .expect("a minted secret")
            .to_string();
        api
    }

    /// `(store token, queue token)` for the test tenant.
    async fn storage_tokens(&self) -> (String, String) {
        let answer = self
            .call(
                reqwest::Method::GET,
                &format!("/apps/{APP}/environments/{ENV}/storage-token"),
                None,
            )
            .await;
        let tenant = answer["tenant"].as_str().expect("a tenant");
        assert!(
            tenant.parse::<deis_wire::CanonicalTenantId>().is_ok(),
            "the storage token names a canonical tenant, not {tenant}"
        );
        (
            answer["token"].as_str().expect("a store token").to_string(),
            answer["queue_token"]
                .as_str()
                .expect("a queue token")
                .to_string(),
        )
    }
}

/// The kids the control plane publishes, read the way a verifier reads them:
/// over the machine channel, proving a data service's certificate.
async fn published_kids(installation: &Installation) -> Vec<String> {
    let name = names::store(TRUST_DOMAIN, "kid-reader").expect("a store name");
    let identity = installation.service_identity("kid-reader", &name);
    let control = deis_capability::ControlClient::connect(&identity.control_client_config())
        .expect("a control client");
    let jwks: deis_capability::Jwks = control
        .get_json(deis_capability::JWKS_PATH)
        .await
        .expect("the JWKS answers on the machine channel");
    let mut kids: Vec<String> = jwks.keys.into_iter().map(|key| key.kid).collect();
    kids.sort();
    kids
}

/// One kv write and read, one SQL statement and one blob listing, each
/// against its own service and so its own binding check. Retried until the
/// deadline because a verifier started before the control plane may still be
/// waiting for its first JWKS fetch; any refusal still standing at the
/// deadline is reported.
async fn use_store(store: &Service, token: &str, marker: &str) {
    let channel = store.channel().await;
    let started = Instant::now();
    loop {
        match try_store(channel.clone(), token, marker).await {
            Ok(()) => return,
            Err(_) if started.elapsed() < DEADLINE => {
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            Err(status) => panic!("deis-store refused a control-issued capability: {status:?}"),
        }
    }
}

async fn try_store(channel: Channel, token: &str, marker: &str) -> Result<(), tonic::Status> {
    let mut kv = KvStoreClient::new(channel.clone());
    kv.set(authed(
        TENANT,
        token,
        SetRequest {
            key: marker.to_string(),
            value: marker.as_bytes().to_vec(),
            ttl_seconds: None,
            metadata: None,
        },
    ))
    .await?;
    let got = kv
        .get(authed(
            TENANT,
            token,
            GetRequest {
                key: marker.to_string(),
            },
        ))
        .await?
        .into_inner();
    assert_eq!(got.value.as_deref(), Some(marker.as_bytes()));

    DatabaseClient::new(channel.clone())
        .execute(authed(
            TENANT,
            token,
            ExecuteRequest {
                sql: "CREATE TABLE IF NOT EXISTS t (v TEXT)".to_string(),
                params: None,
            },
        ))
        .await?;

    BlobStoreClient::new(channel)
        .list(authed(TENANT, token, BlobListRequest::default()))
        .await?;
    Ok(())
}

async fn use_queue(queue: &Service, token: &str, marker: &str) {
    let mut client = QueueClient::new(queue.channel().await);
    let started = Instant::now();
    loop {
        let sent = client
            .send(authed(
                TENANT,
                token,
                SendRequest {
                    queue_name: "jobs".to_string(),
                    message_body: marker.as_bytes().to_vec(),
                },
            ))
            .await;
        match sent {
            Ok(_) => return,
            Err(_) if started.elapsed() < DEADLINE => {
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            Err(status) => panic!("deis-queue refused a control-issued capability: {status:?}"),
        }
    }
}

/// Everything a token must *not* open: the other service, whose audience and
/// bindings it was not minted with. (Another tenant's data is out of reach by
/// construction: the tenant of a call is the one its token names.)
async fn assert_confined(store: &Service, queue: &Service, store_token: &str, queue_token: &str) {
    let store_channel = store.channel().await;

    let queue_token_at_store = KvStoreClient::new(store_channel)
        .get(authed(
            TENANT,
            queue_token,
            GetRequest {
                key: "k".to_string(),
            },
        ))
        .await
        .expect_err("a queue capability must not open deis-store");
    assert_eq!(
        queue_token_at_store.code(),
        Code::PermissionDenied,
        "{queue_token_at_store:?}"
    );

    let store_token_at_queue = QueueClient::new(queue.channel().await)
        .list_dead_letters(authed(
            TENANT,
            store_token,
            ListDeadLettersRequest {
                queue_name: "jobs".to_string(),
                limit: 1,
                cursor: String::new(),
            },
        ))
        .await
        .expect_err("a store capability must not open deis-queue");
    assert_eq!(
        store_token_at_queue.code(),
        Code::PermissionDenied,
        "{store_token_at_queue:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn storage_keeps_working_across_a_control_plane_restart() {
    // The verifiers come up first, before there is a control plane to fetch
    // keys from - the order an installation boots in is not guaranteed.
    let store_listener = local_listener().await;
    let queue_listener = local_listener().await;
    let installation = Installation::new(
        store_listener.local_addr().unwrap(),
        queue_listener.local_addr().unwrap(),
    );
    let store = Service::store(store_listener, &installation, "store-1").await;
    let queue = Service::queue(queue_listener, &installation, "queue-1").await;

    // First life of the control plane.
    let control = installation.start(1).await;
    let api = Api::log_in(&installation).await;
    api.call(reqwest::Method::POST, "/apps", Some(json!({"id": APP})))
        .await;
    api.call(
        reqwest::Method::POST,
        &format!("/apps/{APP}/environments"),
        Some(json!({"name": ENV})),
    )
    .await;
    let kids_before = published_kids(&installation).await;
    assert!(
        !kids_before.is_empty(),
        "a fresh control plane publishes a key"
    );
    // And publishes it nowhere else: the human listener is reachable by
    // whatever reaches a dashboard, and whoever answers for this document
    // decides which capabilities the data services honour.
    let on_the_human_listener = reqwest::get(format!(
        "{}{}",
        installation.base_url(),
        deis_capability::JWKS_PATH
    ))
    .await
    .expect("the human listener answers")
    .json::<deis_capability::Jwks>()
    .await;
    assert!(
        on_the_human_listener.is_err(),
        "the JWKS must not be served on the human listener"
    );
    let (store_before, queue_before) = api.storage_tokens().await;

    use_store(&store, &store_before, "before-restart").await;
    use_queue(&queue, &queue_before, "before-restart").await;
    assert_confined(&store, &queue, &store_before, &queue_before).await;

    // Kill it outright and start it again on the same files.
    control.kill();
    let _control = installation.start(2).await;

    assert_eq!(
        published_kids(&installation).await,
        kids_before,
        "a restarted control plane must publish the keys it published before, not new ones"
    );
    let (store_after, queue_after) = api.storage_tokens().await;

    // The verifiers that were running all along accept both generations.
    use_store(&store, &store_before, "old-token-after-restart").await;
    use_store(&store, &store_after, "new-token-after-restart").await;
    use_queue(&queue, &queue_before, "old-token-after-restart").await;
    use_queue(&queue, &queue_after, "new-token-after-restart").await;

    // And verifiers that have only ever seen the restarted control plane
    // accept the tokens its previous life minted: nothing about trusting a
    // token depends on having been running before the restart.
    let fresh_store = Service::store(local_listener().await, &installation, "store-2").await;
    let fresh_queue = Service::queue(local_listener().await, &installation, "queue-2").await;
    use_store(&fresh_store, &store_before, "old-token-fresh-store").await;
    use_store(&fresh_store, &store_after, "new-token-fresh-store").await;
    use_queue(&fresh_queue, &queue_before, "old-token-fresh-queue").await;
    use_queue(&fresh_queue, &queue_after, "new-token-fresh-queue").await;
    assert_confined(&fresh_store, &fresh_queue, &store_after, &queue_after).await;
}

/// The `jti` a token carries, read without verifying it - the test is the
/// one who asked for the token.
fn jti_of(token: &str) -> String {
    let (payload, _) = deis_capability::parse_token(token).expect("a capability");
    let claims: Value = serde_json::from_slice(&payload).expect("JSON claims");
    claims["jti"].as_str().expect("a jti").to_string()
}

/// Wait until a kv read with `token` is refused with `code`.
async fn refused_eventually(store: &Service, token: &str, code: Code, within: Duration) {
    let mut kv = KvStoreClient::new(store.channel().await);
    let started = Instant::now();
    loop {
        let answer = kv
            .get(authed(
                TENANT,
                token,
                GetRequest {
                    key: "k".to_string(),
                },
            ))
            .await;
        match answer {
            Err(status) if status.code() == code => return,
            _ if started.elapsed() < within => {
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
            other => panic!("the capability was still not refused with {code:?}: {other:?}"),
        }
    }
}

/// Revocation across processes: an operator revokes one capability by `jti`
/// and bumps the auth epoch through the human API; the store learns of it
/// from the list the control plane signs on its machine listener, and
/// refuses within one poll - on kv, and on sql and blob, which share the one
/// verifier.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_revocation_reaches_the_store_within_one_poll() {
    let store_listener = local_listener().await;
    let queue_listener = local_listener().await;
    let installation = Installation::new(
        store_listener.local_addr().unwrap(),
        queue_listener.local_addr().unwrap(),
    );
    let _control = installation.start(1).await;
    let store = Service::store(store_listener, &installation, "store-1").await;

    let api = Api::log_in(&installation).await;
    api.call(reqwest::Method::POST, "/apps", Some(json!({"id": APP})))
        .await;
    api.call(
        reqwest::Method::POST,
        &format!("/apps/{APP}/environments"),
        Some(json!({"name": ENV})),
    )
    .await;
    let (revoked, _) = api.storage_tokens().await;
    let (kept, _) = api.storage_tokens().await;
    use_store(&store, &revoked, "before-revocation").await;
    use_store(&store, &kept, "kept-before-revocation").await;

    let answer = api
        .call(
            reqwest::Method::DELETE,
            &format!("/capabilities/{}", jti_of(&revoked)),
            None,
        )
        .await;
    assert_eq!(answer["revoked"], json!([jti_of(&revoked)]));

    // The store that was running polls every 30 seconds: within one poll.
    refused_eventually(
        &store,
        &revoked,
        Code::PermissionDenied,
        Duration::from_secs(40),
    )
    .await;
    use_store(&store, &kept, "kept-after-revocation").await;
    let sql = DatabaseClient::new(store.channel().await)
        .execute(authed(
            TENANT,
            &revoked,
            ExecuteRequest {
                sql: "SELECT 1".to_string(),
                params: None,
            },
        ))
        .await
        .expect_err("sql still accepted a revoked capability");
    assert_eq!(sql.code(), Code::PermissionDenied);

    // Mass revocation: everything issued so far stops, what is issued after
    // works. A store started now polls at once.
    let bumped = api
        .call(reqwest::Method::POST, "/capabilities/auth-epoch", None)
        .await;
    assert_eq!(bumped["auth_epoch"], 2);
    let fresh = Service::store(local_listener().await, &installation, "store-2").await;
    refused_eventually(&fresh, &kept, Code::PermissionDenied, DEADLINE).await;
    let (after, _) = api.storage_tokens().await;
    use_store(&fresh, &after, "after-epoch-bump").await;
}
