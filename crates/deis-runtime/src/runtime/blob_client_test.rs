//! The blob client against a real deis-store server.
//!
//! `blob.proto` makes both `Get` and `Put` streaming, and the framing lives on
//! two sides of a crate boundary: deis-store cuts the body into chunks, this
//! crate reassembles it, and neither half is proven correct by unit tests of
//! its own. These tests run the real client over a real socket against the
//! real server, so a change to that contract fails here rather than the first
//! time an application reads an object back.
//!
//! The same reasoning covers everything added beside `Get` and `Put`: a
//! listing's paging, a range's resolution and a conditional write's outcome
//! are all decided by the server and interpreted here, so only a test that
//! runs both halves can show that the two agree.
//!
//! They were an integration test under `tests/` until the eager `connect`
//! constructor they used was removed; a test module inside the crate reaches
//! `connect_lazy`, which is how an application binding is actually built.

use super::{BlobClient, BlobListRequest, BlobPrecondition, Conditional, RangeRequest};
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

/// Store an object with no condition attached, which cannot be refused.
async fn put(client: &BlobClient, key: &str, data: Vec<u8>) -> String {
    match client
        .write(key.to_string(), data, None, None)
        .await
        .expect("put")
    {
        Conditional::Done(meta) => meta.etag,
        Conditional::Refused { condition, .. } => {
            panic!("an unconditional put was refused for {condition}")
        }
    }
}

/// Read a whole object, or `None` when the key is not there.
async fn get(client: &BlobClient, key: &str) -> Option<Vec<u8>> {
    client
        .read(key.to_string(), None)
        .await
        .expect("get")
        .map(|body| body.data)
}

#[tokio::test]
async fn small_object_round_trips() {
    let store = TestStore::start().await;
    let client = client(&store, "demo.production");

    put(&client, "greeting", b"hello".to_vec()).await;

    assert_eq!(
        get(&client, "greeting").await.as_deref(),
        Some(&b"hello"[..])
    );
}

#[tokio::test]
async fn multi_chunk_object_round_trips_byte_for_byte() {
    let store = TestStore::start().await;
    let client = client(&store, "demo.production");
    let data = body(MULTI_CHUNK_LEN);

    put(&client, "big", data.clone()).await;
    let got = get(&client, "big").await.expect("the object exists");

    assert_eq!(got.len(), data.len(), "chunk boundaries lost bytes");
    assert_eq!(got, data, "chunks were reassembled out of order");
}

#[tokio::test]
async fn empty_object_round_trips() {
    let store = TestStore::start().await;
    let client = client(&store, "demo.production");

    put(&client, "nothing", Vec::new()).await;

    assert_eq!(
        get(&client, "nothing").await,
        Some(Vec::new()),
        "an empty object is not a missing one"
    );
}

#[tokio::test]
async fn a_missing_key_reads_as_none_rather_than_an_error() {
    let store = TestStore::start().await;
    let client = client(&store, "demo.production");

    assert_eq!(get(&client, "never-written").await, None);
}

#[tokio::test]
async fn delete_removes_the_object_and_is_idempotent() {
    let store = TestStore::start().await;
    let client = client(&store, "demo.production");

    put(&client, "temp", b"bye".to_vec()).await;
    client
        .delete("temp".to_string(), None)
        .await
        .expect("delete");
    assert_eq!(get(&client, "temp").await, None);

    client
        .delete("temp".to_string(), None)
        .await
        .expect("deleting an absent key succeeds");
}

#[tokio::test]
async fn head_answers_the_metadata_without_the_body() {
    let store = TestStore::start().await;
    let client = client(&store, "demo.production");

    let etag = match client
        .write(
            "report.pdf".to_string(),
            b"%PDF-1.7".to_vec(),
            Some("application/pdf".to_string()),
            None,
        )
        .await
        .expect("put")
    {
        Conditional::Done(meta) => meta.etag,
        other => panic!("unconditional put refused: {other:?}"),
    };

    let meta = client
        .head("report.pdf".to_string())
        .await
        .expect("head")
        .expect("the object exists");
    assert_eq!(meta.key, "report.pdf");
    assert_eq!(meta.size, 8);
    assert_eq!(meta.content_type.as_deref(), Some("application/pdf"));
    assert_eq!(
        meta.etag, etag,
        "the etag Head reports must be the one Put answered with, or a caller cannot use one to \
         condition a write against the other"
    );

    assert!(
        client
            .head("no-such-object".to_string())
            .await
            .expect("head")
            .is_none(),
        "a missing key is an answer, not an error"
    );
}

#[tokio::test]
async fn a_range_answers_the_bytes_and_where_they_came_from() {
    let store = TestStore::start().await;
    let client = client(&store, "demo.production");
    let data = body(4096);
    put(&client, "clip.bin", data.clone()).await;

    let read = client
        .read(
            "clip.bin".to_string(),
            Some(RangeRequest::Span {
                offset: 1000,
                length: Some(24),
            }),
        )
        .await
        .expect("ranged get")
        .expect("the object exists");
    assert_eq!(read.data, data[1000..1024]);
    let range = read.range.expect("a ranged read reports its range");
    assert_eq!((range.offset, range.length), (1000, 24));
    assert_eq!(
        read.meta.size, 4096,
        "the whole object's size is what a Content-Range's total is built from"
    );

    // Open ended: from an offset to the end, whatever the end turns out to be.
    let tail = client
        .read(
            "clip.bin".to_string(),
            Some(RangeRequest::Span {
                offset: 4000,
                length: None,
            }),
        )
        .await
        .expect("ranged get")
        .expect("the object exists");
    assert_eq!(tail.data, data[4000..]);

    // A suffix range, which is the form a resumed download uses.
    let suffix = client
        .read("clip.bin".to_string(), Some(RangeRequest::Suffix(96)))
        .await
        .expect("suffix get")
        .expect("the object exists");
    assert_eq!(suffix.data, data[4000..]);
    let range = suffix.range.expect("a ranged read reports its range");
    assert_eq!((range.offset, range.length), (4000, 96));
}

#[tokio::test]
async fn a_range_that_runs_past_the_end_is_clamped_but_one_that_starts_past_it_is_not() {
    let store = TestStore::start().await;
    let client = client(&store, "demo.production");
    put(&client, "short", b"0123456789".to_vec()).await;

    // Every HTTP client asks for "the first megabyte, or all of it if it is
    // smaller" by naming a last byte it hopes exists.
    let clamped = client
        .read(
            "short".to_string(),
            Some(RangeRequest::Span {
                offset: 5,
                length: Some(1_000_000),
            }),
        )
        .await
        .expect("a range that runs past the end is answered with what exists")
        .expect("the object exists");
    assert_eq!(clamped.data, b"56789");

    // Starting past the end names no byte at all. It has to be told apart
    // from a missing key, because an application owes its caller 416 for this
    // one and 404 for that one.
    let status = client
        .read(
            "short".to_string(),
            Some(RangeRequest::Span {
                offset: 10,
                length: None,
            }),
        )
        .await
        .expect_err("a range naming no byte must not read as success");
    let status = status
        .downcast::<tonic::Status>()
        .expect("the store's status survives the trip");
    assert_eq!(status.code(), tonic::Code::OutOfRange);
}

#[tokio::test]
async fn a_ranged_read_is_checked_against_a_digest_of_the_slice() {
    // The whole-object digest cannot validate a slice, so the store sends one
    // for exactly the bytes it streamed. This test is what proves the second
    // digest is actually produced and actually agrees: without it, a range
    // would be the one read on this platform that nobody checks.
    let store = TestStore::start().await;
    let client = client(&store, "demo.production");
    let data = body(MULTI_CHUNK_LEN);
    put(&client, "film", data.clone()).await;

    let read = client
        .read(
            "film".to_string(),
            Some(RangeRequest::Span {
                offset: 1_000_000,
                length: Some(1_000_000),
            }),
        )
        .await
        .expect("a range whose digest matches is handed over")
        .expect("the object exists");
    assert_eq!(read.data, data[1_000_000..2_000_000]);
}

#[tokio::test]
async fn listing_pages_in_key_order_and_stops_on_a_cursor() {
    let store = TestStore::start().await;
    let client = client(&store, "demo.production");
    for n in 0..7 {
        put(&client, &format!("logs/{n:02}.txt"), vec![b'x'; n]).await;
    }
    put(&client, "other", b"not under the prefix".to_vec()).await;

    let mut seen = Vec::new();
    let mut cursor = None;
    let mut pages = 0;
    loop {
        let page = client
            .list(BlobListRequest {
                prefix: "logs/".to_string(),
                limit: Some(3),
                cursor: cursor.clone(),
                ..Default::default()
            })
            .await
            .expect("list");
        pages += 1;
        assert!(page.objects.len() <= 3, "the page limit was not honoured");
        seen.extend(page.objects.into_iter().map(|o| o.key));
        cursor = page.cursor;
        if cursor.is_none() {
            break;
        }
        assert!(pages < 10, "paging did not terminate");
    }

    assert_eq!(
        seen,
        (0..7)
            .map(|n| format!("logs/{n:02}.txt"))
            .collect::<Vec<_>>(),
        "a listing must be in key order and must not repeat or skip a key across pages"
    );
    assert!(
        !seen.iter().any(|key| key == "other"),
        "the prefix did not bound the scan"
    );
}

#[tokio::test]
async fn a_delimiter_rolls_a_folder_up_into_one_entry() {
    let store = TestStore::start().await;
    let client = client(&store, "demo.production");
    for key in [
        "index.html",
        "img/a.png",
        "img/b.png",
        "img/deep/c.png",
        "js/app.js",
    ] {
        put(&client, key, b"x".to_vec()).await;
    }

    let page = client
        .list(BlobListRequest {
            delimiter: Some("/".to_string()),
            ..Default::default()
        })
        .await
        .expect("list");

    assert_eq!(
        page.objects.iter().map(|o| &o.key).collect::<Vec<_>>(),
        vec!["index.html"],
        "only the keys with no separator left in them belong at this level"
    );
    assert_eq!(page.prefixes, vec!["img/".to_string(), "js/".to_string()]);

    // One level down: the roll-up is relative to the prefix, so `img/deep/`
    // appears here and `img/a.png` is a plain key.
    let page = client
        .list(BlobListRequest {
            prefix: "img/".to_string(),
            delimiter: Some("/".to_string()),
            ..Default::default()
        })
        .await
        .expect("list");
    assert_eq!(
        page.objects.iter().map(|o| &o.key).collect::<Vec<_>>(),
        vec!["img/a.png", "img/b.png"]
    );
    assert_eq!(page.prefixes, vec!["img/deep/".to_string()]);
}

#[tokio::test]
async fn a_conditional_create_lets_exactly_one_of_two_writers_win() {
    // The interleaving an application actually hits: two request handlers
    // racing to create the same key, each believing it is first. Exactly one
    // may be told it wrote, and the loser has to be told which - not handed an
    // error it might retry into an overwrite.
    let store = TestStore::start().await;
    let one = client(&store, "demo.production");
    let two = one.clone();

    let first = tokio::spawn(async move {
        one.write(
            "lock".to_string(),
            b"held by one".to_vec(),
            None,
            Some(BlobPrecondition::IfNoneMatch("*".to_string())),
        )
        .await
    });
    let second = tokio::spawn(async move {
        two.write(
            "lock".to_string(),
            b"held by two".to_vec(),
            None,
            Some(BlobPrecondition::IfNoneMatch("*".to_string())),
        )
        .await
    });

    let outcomes = [
        first.await.unwrap().expect("the call itself succeeded"),
        second.await.unwrap().expect("the call itself succeeded"),
    ];
    let winners = outcomes
        .iter()
        .filter(|o| matches!(o, Conditional::Done(_)))
        .count();
    assert_eq!(
        winners, 1,
        "exactly one create-if-absent may win; got {outcomes:?}"
    );

    let loser = outcomes
        .iter()
        .find(|o| matches!(o, Conditional::Refused { .. }))
        .expect("one of them must have lost");
    match loser {
        Conditional::Refused { condition, etag } => {
            assert_eq!(condition, "If-None-Match: *");
            assert!(
                etag.is_some(),
                "the loser is owed the etag the winner left behind, or it cannot re-read and \
                 retry against it"
            );
        }
        other => panic!("expected a refusal, got {other:?}"),
    }
}

#[tokio::test]
async fn if_match_is_a_compare_and_set_on_the_etag() {
    let store = TestStore::start().await;
    let client = client(&store, "demo.production");
    let first = put(&client, "counter", b"1".to_vec()).await;

    // The read-modify-write that holds: the object still carries what this
    // writer based its change on.
    let second = match client
        .write(
            "counter".to_string(),
            b"2".to_vec(),
            None,
            Some(BlobPrecondition::IfMatch(first.clone())),
        )
        .await
        .expect("put")
    {
        Conditional::Done(meta) => meta.etag,
        other => panic!("the etag was current, so this must have been written: {other:?}"),
    };
    assert_ne!(
        first, second,
        "different content must have a different etag"
    );

    // The same write again, still quoting the old etag: this is the writer
    // that was overtaken, and it must be told so rather than overwrite.
    match client
        .write(
            "counter".to_string(),
            b"3".to_vec(),
            None,
            Some(BlobPrecondition::IfMatch(first)),
        )
        .await
        .expect("put")
    {
        Conditional::Refused { etag, .. } => assert_eq!(etag.as_ref(), Some(&second)),
        other => panic!("a stale etag must not write: {other:?}"),
    }
    assert_eq!(
        get(&client, "counter").await.as_deref(),
        Some(&b"2"[..]),
        "the refused write must not have landed"
    );

    // A quoted etag is what an application forwarding an `If-Match` header
    // holds, and it names the same object.
    match client
        .write(
            "counter".to_string(),
            b"4".to_vec(),
            None,
            Some(BlobPrecondition::IfMatch(format!("\"{second}\""))),
        )
        .await
        .expect("put")
    {
        Conditional::Done(_) => {}
        other => panic!("a quoted etag names the same object: {other:?}"),
    }
}

#[tokio::test]
async fn a_conditional_delete_refuses_rather_than_removing_the_wrong_version() {
    let store = TestStore::start().await;
    let client = client(&store, "demo.production");
    let stale = put(&client, "draft", b"first".to_vec()).await;
    let current = put(&client, "draft", b"second".to_vec()).await;

    match client
        .delete("draft".to_string(), Some(BlobPrecondition::IfMatch(stale)))
        .await
        .expect("delete")
    {
        Conditional::Refused { etag, .. } => assert_eq!(etag.as_ref(), Some(&current)),
        other => panic!("deleting against a stale etag must be refused: {other:?}"),
    }
    assert_eq!(get(&client, "draft").await.as_deref(), Some(&b"second"[..]));

    match client
        .delete(
            "draft".to_string(),
            Some(BlobPrecondition::IfMatch(current)),
        )
        .await
        .expect("delete")
    {
        Conditional::Done(()) => {}
        other => panic!("the current etag must delete: {other:?}"),
    }
    assert_eq!(get(&client, "draft").await, None);
}

#[tokio::test]
async fn one_application_cannot_read_another_ones_objects() {
    let store = TestStore::start().await;
    let owner = client(&store, "owner-app.production");
    let other = client(&store, "other-app.production");

    put(&owner, "shared-name", b"owner data".to_vec()).await;

    assert_eq!(
        get(&other, "shared-name").await,
        None,
        "the tenant header must partition the store, not just label the call"
    );
}

/// The listing has to be partitioned by tenant exactly as a read is, and it is
/// the riskier of the two: a read has to name a key to get anything, while a
/// listing asks an open question and is answered with whatever the scan finds.
/// A missing tenant term in one `WHERE` clause would publish every
/// application's key names to every other one.
#[tokio::test]
async fn a_listing_never_shows_another_tenants_keys_however_it_is_asked() {
    let store = TestStore::start().await;
    let owner = client(&store, "owner-app.production");
    let snooper = client(&store, "other-app.production");

    for key in ["secret/a", "secret/b", "public"] {
        put(&owner, key, b"owner data".to_vec()).await;
    }
    put(&snooper, "mine", b"my data".to_vec()).await;

    // The open question: everything, no prefix, no delimiter.
    let page = snooper
        .list(BlobListRequest::default())
        .await
        .expect("list");
    assert_eq!(
        page.objects.iter().map(|o| &o.key).collect::<Vec<_>>(),
        vec!["mine"],
        "an unprefixed listing must answer only this tenant's keys"
    );

    // Naming the other tenant's prefix outright.
    let page = snooper
        .list(BlobListRequest {
            prefix: "secret/".to_string(),
            ..Default::default()
        })
        .await
        .expect("list");
    assert!(
        page.objects.is_empty() && page.prefixes.is_empty(),
        "another tenant's prefix must answer nothing, not its keys"
    );

    // A delimiter turns key names into folder names, which would leak the
    // shape of another tenant's namespace even without a single object.
    let page = snooper
        .list(BlobListRequest {
            delimiter: Some("/".to_string()),
            ..Default::default()
        })
        .await
        .expect("list");
    assert!(
        page.prefixes.is_empty(),
        "rolled-up prefixes must not name another tenant's folders: {:?}",
        page.prefixes
    );

    // A cursor handed over from the other tenant's own scan. It is the only
    // caller-supplied value that names a position rather than a key, so it is
    // the one worth trying to smuggle across.
    let owners_page = owner
        .list(BlobListRequest {
            limit: Some(1),
            ..Default::default()
        })
        .await
        .expect("list");
    let owners_cursor = owners_page
        .cursor
        .expect("a full page answers a cursor to resume from");
    let page = snooper
        .list(BlobListRequest {
            cursor: Some(owners_cursor),
            ..Default::default()
        })
        .await
        .expect("list");
    assert!(
        page.objects.iter().all(|o| o.key == "mine"),
        "a cursor from another tenant's scan must not resume inside their data: {:?}",
        page.objects.iter().map(|o| &o.key).collect::<Vec<_>>()
    );

    // And the reverse direction: what the owner sees is unchanged by any of
    // it.
    let page = owner.list(BlobListRequest::default()).await.expect("list");
    assert_eq!(page.objects.len(), 3);
}

/// A conditional write is scoped by tenant like everything else: two
/// environments racing on the same key name are not racing at all.
#[tokio::test]
async fn a_conditional_create_in_one_environment_does_not_block_another() {
    let store = TestStore::start().await;
    let production = client(&store, "demo.production");
    let preview = client(&store, "demo.preview");

    for client in [&production, &preview] {
        match client
            .write(
                "lock".to_string(),
                b"held".to_vec(),
                None,
                Some(BlobPrecondition::IfNoneMatch("*".to_string())),
            )
            .await
            .expect("put")
        {
            Conditional::Done(_) => {}
            other => panic!("each environment creates its own key: {other:?}"),
        }
    }
}

/// A preview environment sharing the store with production is the failure the
/// composite tenant exists to prevent: a preview build that overwrites a
/// production object cannot be undone by rolling the version back.
#[tokio::test]
async fn two_environments_of_one_app_do_not_share_objects() {
    let store = TestStore::start().await;
    let production = client(&store, "demo.production");
    let preview = client(&store, "demo.preview");

    put(&production, "report.txt", b"production".to_vec()).await;
    put(&preview, "report.txt", b"preview".to_vec()).await;

    assert_eq!(
        get(&production, "report.txt").await,
        Some(b"production".to_vec()),
        "the preview environment overwrote what production stored"
    );
    assert_eq!(get(&preview, "report.txt").await, Some(b"preview".to_vec()));
}
