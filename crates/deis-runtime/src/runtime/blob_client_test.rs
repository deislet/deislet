//! The blob client against a real deis-store server.
//!
//! `blob.proto` makes both `Get` and `Put` streaming, and the framing lives on
//! two sides of a crate boundary: deis-store cuts the body into chunks, this
//! crate reassembles it, and neither half is proven correct by unit tests of
//! its own. These tests run the real client over a real socket against the
//! real server, so a change to that contract fails here rather than the first
//! time an application reads an object back.
//!
//! They were an integration test under `tests/` until the eager `connect`
//! constructor they used was removed; a test module inside the crate reaches
//! `connect_lazy`, which is how an application binding is actually built.

use super::BlobClient;
use crate::runtime::bindings::StorageIdentity;
use crate::runtime::test_store::TestStore;

/// Larger than the 1 MiB chunk the two sides agree on, and deliberately not a
/// multiple of it, so the last chunk is a short one.
const MULTI_CHUNK_LEN: usize = 2 * 1024 * 1024 + 4242;

/// A client wired the way an application binding is: dialled on first use,
/// speaking for the `<app>.<environment>` pair the store partitions by. A
/// bare application id is refused by [`StorageIdentity`].
fn client(store: &TestStore, tenant: &str) -> BlobClient {
    let identity = StorageIdentity::new(tenant, None).expect("a legal tenant");
    BlobClient::connect_lazy(&store.endpoint, &identity).expect("client is built")
}

/// Deterministic filler, so a mismatch says which byte moved.
fn body(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i % 251) as u8).collect()
}

#[tokio::test]
async fn small_object_round_trips() {
    let store = TestStore::start().await;
    let client = client(&store, "demo.production");

    client
        .put("greeting".to_string(), b"hello".to_vec())
        .await
        .expect("put");
    let got = client.get("greeting".to_string()).await.expect("get");

    assert_eq!(got.as_deref(), Some(&b"hello"[..]));
}

#[tokio::test]
async fn multi_chunk_object_round_trips_byte_for_byte() {
    let store = TestStore::start().await;
    let client = client(&store, "demo.production");
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
    let store = TestStore::start().await;
    let client = client(&store, "demo.production");

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
    let store = TestStore::start().await;
    let client = client(&store, "demo.production");

    let got = client.get("never-written".to_string()).await.expect("get");

    assert_eq!(got, None);
}

#[tokio::test]
async fn delete_removes_the_object_and_is_idempotent() {
    let store = TestStore::start().await;
    let client = client(&store, "demo.production");

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
    let store = TestStore::start().await;
    let owner = client(&store, "owner-app.production");
    let other = client(&store, "other-app.production");

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
    let store = TestStore::start().await;
    let production = client(&store, "demo.production");
    let preview = client(&store, "demo.preview");

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
