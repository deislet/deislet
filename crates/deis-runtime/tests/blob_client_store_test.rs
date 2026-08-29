//! The blob client against a real deis-store server.
//!
//! `blob.proto` makes both `Get` and `Put` streaming, and the framing lives on
//! two sides of a crate boundary: deis-store cuts the body into chunks, this
//! crate reassembles it, and neither half is proven correct by unit tests of
//! its own. These tests run the real client over a real socket against the
//! real server, so a change to that contract fails here rather than the first
//! time an application reads an object back.

use deis_runtime::runtime::BlobClient;
use deis_store::config::Config;
use deis_store::server;
use tempfile::TempDir;
use tokio::sync::oneshot;

/// Larger than the 1 MiB chunk the two sides agree on, and deliberately not a
/// multiple of it, so the last chunk is a short one.
const MULTI_CHUNK_LEN: usize = 2 * 1024 * 1024 + 4242;

/// A running deis-store plus the temp directory holding its data.
struct Store {
    _data: TempDir,
    endpoint: String,
    shutdown: Option<oneshot::Sender<()>>,
}

impl Store {
    async fn start() -> Self {
        let data = TempDir::new().expect("temp dir");
        let config = Config {
            listen: "127.0.0.1:0".to_string(),
            data_dir: data.path().to_path_buf(),
            ..Config::default()
        };
        // Bind before spawning, so the port is known and no test has to wait
        // for the server to come up.
        let listener = server::bind(&config).await.expect("bind");
        let addr = listener.local_addr().expect("local addr");
        let (shutdown, rx) = oneshot::channel();
        tokio::spawn(async move {
            server::serve(config, listener, async {
                let _ = rx.await;
            })
            .await
            .expect("serve");
        });
        Self {
            _data: data,
            endpoint: format!("http://{addr}"),
            shutdown: Some(shutdown),
        }
    }

    /// `tenant` is the composite `<app>.<environment>` key the store partitions
    /// by; a bare application id is refused by the client.
    async fn client(&self, tenant: &str) -> BlobClient {
        BlobClient::connect(self.endpoint.clone(), tenant)
            .await
            .expect("connect blob client")
    }
}

impl Drop for Store {
    fn drop(&mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
    }
}

/// Deterministic filler, so a mismatch says which byte moved.
fn body(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i % 251) as u8).collect()
}

#[tokio::test]
async fn small_object_round_trips() {
    let store = Store::start().await;
    let client = store.client("demo.production").await;

    client
        .put("greeting".to_string(), b"hello".to_vec())
        .await
        .expect("put");
    let got = client.get("greeting".to_string()).await.expect("get");

    assert_eq!(got.as_deref(), Some(&b"hello"[..]));
}

#[tokio::test]
async fn multi_chunk_object_round_trips_byte_for_byte() {
    let store = Store::start().await;
    let client = store.client("demo.production").await;
    let data = body(MULTI_CHUNK_LEN);

    client
        .put("big".to_string(), data.clone())
        .await
        .expect("put a multi-chunk object");
    let got = client
        .get("big".to_string())
        .await
        .expect("get")
        .expect("the object exists");

    assert_eq!(got.len(), data.len(), "chunk boundaries lost bytes");
    assert_eq!(got, data, "chunks were reassembled out of order");
}

#[tokio::test]
async fn empty_object_round_trips() {
    let store = Store::start().await;
    let client = store.client("demo.production").await;

    client
        .put("nothing".to_string(), Vec::new())
        .await
        .expect("a header with no body stores an empty object");
    let got = client.get("nothing".to_string()).await.expect("get");

    assert_eq!(
        got,
        Some(Vec::new()),
        "an empty object is not a missing one"
    );
}

#[tokio::test]
async fn a_missing_key_reads_as_none_rather_than_an_error() {
    let store = Store::start().await;
    let client = store.client("demo.production").await;

    let got = client.get("never-written".to_string()).await.expect("get");

    assert_eq!(got, None);
}

#[tokio::test]
async fn delete_removes_the_object_and_is_idempotent() {
    let store = Store::start().await;
    let client = store.client("demo.production").await;

    client
        .put("temp".to_string(), b"bye".to_vec())
        .await
        .expect("put");
    client.delete("temp".to_string()).await.expect("delete");
    assert_eq!(client.get("temp".to_string()).await.expect("get"), None);

    client
        .delete("temp".to_string())
        .await
        .expect("deleting an absent key succeeds");
}

#[tokio::test]
async fn one_application_cannot_read_another_ones_objects() {
    let store = Store::start().await;
    let owner = store.client("owner-app.production").await;
    let other = store.client("other-app.production").await;

    owner
        .put("shared-name".to_string(), b"owner data".to_vec())
        .await
        .expect("put");

    assert_eq!(
        other.get("shared-name".to_string()).await.expect("get"),
        None,
        "the tenant header must partition the store, not just label the call"
    );
}

/// A preview environment sharing the store with production is the failure the
/// composite tenant exists to prevent: a preview build that overwrites a
/// production object cannot be undone by rolling the version back.
#[tokio::test]
async fn two_environments_of_one_app_do_not_share_objects() {
    let store = Store::start().await;
    let production = store.client("demo.production").await;
    let preview = store.client("demo.preview").await;

    production
        .put("report.txt".to_string(), b"production".to_vec())
        .await
        .expect("put");
    preview
        .put("report.txt".to_string(), b"preview".to_vec())
        .await
        .expect("put");

    assert_eq!(
        production.get("report.txt".to_string()).await.expect("get"),
        Some(b"production".to_vec()),
        "the preview environment overwrote what production stored"
    );
    assert_eq!(
        preview.get("report.txt".to_string()).await.expect("get"),
        Some(b"preview".to_vec())
    );
}
