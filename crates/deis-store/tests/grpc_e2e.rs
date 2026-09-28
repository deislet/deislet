//! End-to-end checks against a real deis-store server.
//!
//! Everything here goes over a real TCP socket, under TLS with a test
//! authority's certificates, and the real generated clients, so it exercises
//! the parts unit tests cannot: metadata handling, the streaming blob
//! protocol, the message size ceiling, the fact that the three services are
//! actually registered on the server, and that a caller refuses a server the
//! fleet's authority did not sign for as a store.
//!
//! Every call carries a capability naming one canonical tenant, which is
//! what deis-runtime sends; nothing in the request names the tenant. The
//! tests name tenants by label - `app-one.production`, `app-two.production`
//! - and derive the canonical id from it (`CanonicalTenantId::derived_from_label`),
//! and one test pairs `app-one.production` with `app-one.preview` to check
//! that two environments of the *same* application are also two namespaces.

use deis_capability::{CapabilityClaims, CapabilityResource, KeyRing, encode};
use deis_identity::{Protocol, ServiceKind};
use deis_identity_test_pki::TestFleet;
use deis_store::config::Config;
use deis_store::proto::blob::{
    BlobInfo, PutHeader, PutRequest, blob_store_client::BlobStoreClient, get_response, put_request,
};
use deis_store::proto::storage::{
    BatchRequest, ExecutePreparedRequest, ExecuteRequest, GetRequest, HasRequest, ListRequest,
    PrepareRequest, QueryPreparedRequest, QueryRequest, SetRequest,
    database_client::DatabaseClient, kv_store_client::KvStoreClient,
};
use deis_store::tenant::BINDING_HEADER;
use deis_store::{MAX_GRPC_MESSAGE_BYTES, server};
use deis_wire::CanonicalTenantId;
use deis_wire::conventions::tenant_id;
use ed25519_dalek::SigningKey;
use std::time::Duration;
use tempfile::TempDir;
use tokio::sync::oneshot;
use tonic::Request;
use tonic::transport::{Channel, Endpoint};

const AUTHORIZATION: &str = "authorization";
const TOKEN_LIFETIME_SECS: u64 = 900;

fn test_signing_key() -> SigningKey {
    let mut seed = [0u8; 32];
    seed[0..8].copy_from_slice(b"e2e-test");
    SigningKey::from_bytes(&seed)
}

/// The canonical tenant a test label stands for.
fn canonical(label: &str) -> String {
    CanonicalTenantId::derived_from_label(label).to_string()
}

/// The on-disk key of a test tenant's unnamed namespace.
fn key_of(label: &str) -> String {
    deis_wire::DeisletCanonicalTenantKey::new(CanonicalTenantId::derived_from_label(label))
        .to_string()
}

/// A store capability naming exactly these raw (tenant, binding) pairs.
fn make_raw_store_token(resources: &[(&str, &str)]) -> String {
    let claims = CapabilityClaims::new(
        "store",
        "spiffe://deislet/control/main",
        "spiffe://deislet/node/test-node",
        1,
        Duration::from_secs(TOKEN_LIFETIME_SECS),
        resources
            .iter()
            .map(|(tenant, binding)| CapabilityResource {
                tenant: (*tenant).to_string(),
                binding: (*binding).to_string(),
                generation: None,
            })
            .collect(),
    );
    encode(&claims, &test_signing_key())
}

fn make_store_token(tenant: &str, binding: &str) -> String {
    let tenant = canonical(tenant);
    let claims = CapabilityClaims::new(
        "store",
        "spiffe://deislet/control/main",
        "spiffe://deislet/node/test-node",
        1,
        Duration::from_secs(TOKEN_LIFETIME_SECS),
        vec![CapabilityResource {
            tenant: tenant.to_string(),
            binding: binding.to_string(),
            generation: None,
        }],
    );
    encode(&claims, &test_signing_key())
}

/// Create a token that authorises all three storage bindings for one tenant.
/// This lets any test call KvStore, Database or BlobStore with the same token.
fn make_store_token_all_bindings(tenant: &str) -> String {
    let tenant = canonical(tenant);
    let claims = CapabilityClaims::new(
        "store",
        "spiffe://deislet/control/main",
        "spiffe://deislet/node/test-node",
        1,
        Duration::from_secs(TOKEN_LIFETIME_SECS),
        vec![
            CapabilityResource {
                tenant: tenant.to_string(),
                binding: "kv".to_string(),
                generation: None,
            },
            CapabilityResource {
                tenant: tenant.to_string(),
                binding: "sql".to_string(),
                generation: None,
            },
            CapabilityResource {
                tenant: tenant.to_string(),
                binding: "blob".to_string(),
                generation: None,
            },
        ],
    );
    encode(&claims, &test_signing_key())
}

/// A running server plus the temp directory holding its data.
struct Harness {
    /// `None` when the caller owns the data directory, which is how a test
    /// runs two servers over the same one.
    _data: Option<TempDir>,
    endpoint: String,
    shutdown: Option<oneshot::Sender<()>>,
    fleet: TestFleet,
}

impl Harness {
    /// All servers now require capability-based auth. The test key is used
    /// so every test can mint valid tokens.
    async fn start() -> Self {
        let data = TempDir::new().unwrap();
        let data_dir = data.path().to_path_buf();
        Self::start_over(Some(data), data_dir).await
    }

    /// A server that requires a capability token, as any deployment that is
    /// reachable off-host must. The global test key is used.
    async fn start_authenticated() -> Self {
        Self::start().await
    }

    /// A server over a data directory the caller keeps, so the same one can
    /// be served twice.
    async fn start_on(data_dir: &std::path::Path) -> Self {
        Self::start_over(None, data_dir.to_path_buf()).await
    }

    async fn start_over(owned: Option<TempDir>, data_dir: std::path::PathBuf) -> Self {
        let config = Config {
            listen: "127.0.0.1:0".to_string(),
            data_dir,
            // Small enough that the size limit is reachable in a test, large
            // enough for the multi-chunk object below.
            max_object_bytes: 8 * 1024 * 1024,
            ..Config::default()
        };
        // Bind first, so the port is known before anything is spawned and no
        // test has to wait for the server to come up.
        let listener = server::bind(&config).await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (shutdown, rx) = oneshot::channel();
        let keys = KeyRing::from_keys([test_signing_key().verifying_key()]);
        let fleet = TestFleet::new();
        let identity = fleet.service_identity(ServiceKind::Store);
        tokio::spawn(async move {
            server::serve(
                config,
                listener,
                async {
                    let _ = rx.await;
                },
                keys,
                identity,
            )
            .await
            .unwrap();
        });
        Self {
            _data: owned,
            endpoint: format!("https://{addr}"),
            shutdown: Some(shutdown),
            fleet,
        }
    }

    /// A channel that checks the far end is a store of this fleet, the way
    /// every real caller does.
    async fn channel(&self) -> Channel {
        Endpoint::from_shared(self.endpoint.clone())
            .unwrap()
            .connect_with_connector(
                self.fleet
                    .service_trust()
                    .dialler(ServiceKind::Store, &[Protocol::Http2]),
            )
            .await
            .unwrap()
    }

    async fn kv(&self) -> KvStoreClient<Channel> {
        KvStoreClient::new(self.channel().await)
            .max_decoding_message_size(MAX_GRPC_MESSAGE_BYTES)
            .max_encoding_message_size(MAX_GRPC_MESSAGE_BYTES)
    }

    async fn database(&self) -> DatabaseClient<Channel> {
        DatabaseClient::new(self.channel().await)
            .max_decoding_message_size(MAX_GRPC_MESSAGE_BYTES)
            .max_encoding_message_size(MAX_GRPC_MESSAGE_BYTES)
    }

    async fn blob(&self) -> BlobStoreClient<Channel> {
        BlobStoreClient::new(self.channel().await)
            .max_decoding_message_size(MAX_GRPC_MESSAGE_BYTES)
            .max_encoding_message_size(MAX_GRPC_MESSAGE_BYTES)
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
    }
}

/// Carry a capability for the tenant, the way deis-runtime does.
fn as_tenant<T>(tenant: &str, message: T) -> Request<T> {
    let mut request = Request::new(message);
    request.metadata_mut().insert(
        AUTHORIZATION,
        format!("Bearer {}", make_store_token_all_bindings(tenant))
            .parse()
            .unwrap(),
    );
    request
}

/// A call WITHOUT a capability token. Only use this in tests that
/// deliberately check that calls without tokens are refused.
fn as_tenant_no_token<T>(_tenant: &str, message: T) -> Request<T> {
    Request::new(message)
}

/// Carry a capability for the tenant and select a binding inside it.
fn as_binding<T>(tenant: &str, binding: &str, message: T) -> Request<T> {
    let mut request = as_tenant(tenant, message);
    request
        .metadata_mut()
        .insert(BINDING_HEADER, binding.parse().unwrap());
    request
}

/// Carry a capability for `token_tenant` authorising only `binding`. A
/// `claimed` tenant other than `token_tenant` is stamped in the retired
/// `x-deis-tenant` header, which the service must ignore.
fn as_tenant_with_token<T>(
    claimed: &str,
    token_tenant: &str,
    binding: &str,
    message: T,
) -> Request<T> {
    let mut request = Request::new(message);
    if claimed != token_tenant {
        request
            .metadata_mut()
            .insert("x-deis-tenant", canonical(claimed).parse().unwrap());
    }
    request.metadata_mut().insert(
        AUTHORIZATION,
        format!("Bearer {}", make_store_token(token_tenant, binding))
            .parse()
            .unwrap(),
    );
    request
}

#[tokio::test]
async fn kv_round_trip_over_grpc() {
    let harness = Harness::start().await;
    let mut kv = harness.kv().await;

    kv.set(as_tenant(
        "app-one.production",
        SetRequest {
            key: "greeting".to_string(),
            value: b"hello".to_vec(),
            ttl_seconds: None,
            metadata: None,
        },
    ))
    .await
    .unwrap();

    let got = kv
        .get(as_tenant(
            "app-one.production",
            GetRequest {
                key: "greeting".to_string(),
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(got.value.as_deref(), Some(&b"hello"[..]));

    assert!(
        kv.has(as_tenant(
            "app-one.production",
            HasRequest {
                key: "greeting".to_string()
            }
        ))
        .await
        .unwrap()
        .into_inner()
        .exists
    );

    // A second tenant sees nothing, over the same connection.
    let other = kv
        .get(as_tenant(
            "app-two.production",
            GetRequest {
                key: "greeting".to_string(),
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(other.value, None);

    kv.delete(as_tenant(
        "app-one.production",
        deis_store::proto::storage::DeleteRequest {
            key: "greeting".to_string(),
        },
    ))
    .await
    .unwrap();
    let gone = kv
        .get(as_tenant(
            "app-one.production",
            GetRequest {
                key: "greeting".to_string(),
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(gone.value, None);
}

/// The `expires_at` column and the sweeper behind it are only reachable
/// through `ttl_seconds`, so the round trip is checked over the wire rather
/// than only against the storage method.
#[tokio::test]
async fn kv_honours_a_lifetime_over_grpc() {
    let harness = Harness::start().await;
    let mut kv = harness.kv().await;

    kv.set(as_tenant(
        "app-one.production",
        SetRequest {
            key: "session".to_string(),
            value: b"token".to_vec(),
            ttl_seconds: Some(3600),
            metadata: Some("owner=tests".to_string()),
        },
    ))
    .await
    .unwrap();

    // An hour out, so it is still readable.
    let got = kv
        .get(as_tenant(
            "app-one.production",
            GetRequest {
                key: "session".to_string(),
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(got.value.as_deref(), Some(&b"token"[..]));

    // A lifetime that cannot be honoured is refused outright, rather than
    // stored as something no reader will ever see again.
    let refused = kv
        .set(as_tenant(
            "app-one.production",
            SetRequest {
                key: "doomed".to_string(),
                value: b"v".to_vec(),
                ttl_seconds: Some(0),
                metadata: None,
            },
        ))
        .await
        .unwrap_err();
    assert_eq!(refused.code(), tonic::Code::InvalidArgument);
    let absent = kv
        .has(as_tenant(
            "app-one.production",
            HasRequest {
                key: "doomed".to_string(),
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert!(!absent.exists, "a refused write must store nothing");
}

#[tokio::test]
async fn kv_list_pages_by_prefix() {
    let harness = Harness::start().await;
    let mut kv = harness.kv().await;
    for key in ["user:1", "user:2", "user:3", "post:1"] {
        kv.set(as_tenant(
            "app-one.production",
            SetRequest {
                key: key.to_string(),
                value: b"v".to_vec(),
                ttl_seconds: None,
                metadata: None,
            },
        ))
        .await
        .unwrap();
    }

    let page = kv
        .list(as_tenant(
            "app-one.production",
            ListRequest {
                prefix: "user:".to_string(),
                limit: Some(2),
                cursor: None,
                include_metadata: None,
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(page.keys, vec!["user:1", "user:2"]);
    assert_eq!(page.next_cursor.as_deref(), Some("user:2"));

    let rest = kv
        .list(as_tenant(
            "app-one.production",
            ListRequest {
                prefix: "user:".to_string(),
                limit: Some(2),
                cursor: page.next_cursor,
                include_metadata: None,
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(rest.keys, vec!["user:3"]);
    assert_eq!(rest.next_cursor, None);
}

/// Metadata used to be write-only: `Set` took it, the column held it, and no
/// RPC gave it back. Both ways of reading it are checked over a real socket,
/// because the two shapes of `List` are a wire concern - one of them is what
/// keeps a ten-thousand-key page from carrying every name twice.
#[tokio::test]
async fn kv_metadata_can_be_read_back_by_get_and_by_list() {
    let harness = Harness::start().await;
    let mut kv = harness.kv().await;

    kv.set(as_tenant(
        "app-one.production",
        SetRequest {
            key: "session:1".to_string(),
            value: b"token".to_vec(),
            ttl_seconds: Some(3600),
            metadata: Some(r#"{"user":"7"}"#.to_string()),
        },
    ))
    .await
    .unwrap();

    let got = kv
        .get(as_tenant(
            "app-one.production",
            GetRequest {
                key: "session:1".to_string(),
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(got.value.as_deref(), Some(&b"token"[..]));
    assert_eq!(got.metadata.as_deref(), Some(r#"{"user":"7"}"#));
    let expires_at = got.expires_at.expect("a lifetime was asked for");

    // Asking for metadata gives entries and no bare key list; not asking
    // gives exactly what every existing caller already reads.
    let detailed = kv
        .list(as_tenant(
            "app-one.production",
            ListRequest {
                prefix: "session:".to_string(),
                limit: None,
                cursor: None,
                include_metadata: Some(true),
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert!(detailed.keys.is_empty());
    assert_eq!(detailed.entries.len(), 1);
    assert_eq!(detailed.entries[0].key, "session:1");
    assert_eq!(
        detailed.entries[0].metadata.as_deref(),
        Some(r#"{"user":"7"}"#)
    );
    assert_eq!(detailed.entries[0].expires_at, Some(expires_at));

    let plain = kv
        .list(as_tenant(
            "app-one.production",
            ListRequest {
                prefix: "session:".to_string(),
                limit: None,
                cursor: None,
                include_metadata: None,
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(plain.keys, vec!["session:1"]);
    assert!(plain.entries.is_empty());
}

#[tokio::test]
async fn a_request_without_a_usable_tenant_is_refused() {
    let harness = Harness::start().await;
    let mut kv = harness.kv().await;
    // No header at all. There is no default namespace to fall back to, and
    // inventing one would put every unstamped caller in the same place.
    let status = kv
        .get(Request::new(GetRequest {
            key: "greeting".to_string(),
        }))
        .await
        .unwrap_err();
    assert_eq!(status.code(), tonic::Code::Unauthenticated);

    // A capability whose tenant is not a canonical id, or that names no
    // tenant or two for the binding, says nothing about whose data the call
    // is about; the request has nothing that could say it instead.
    let other = canonical("app-two.production");
    let mine = canonical("app-one.production");
    for (what, token) in [
        ("../../etc", make_raw_store_token(&[("../../etc", "kv")])),
        (
            "a name label",
            make_raw_store_token(&[("app-one.production", "kv")]),
        ),
        (
            "no tenant for kv",
            make_raw_store_token(&[(mine.as_str(), "sql")]),
        ),
        (
            "two tenants",
            make_raw_store_token(&[(mine.as_str(), "kv"), (other.as_str(), "kv")]),
        ),
    ] {
        let mut request = Request::new(GetRequest {
            key: "greeting".to_string(),
        });
        request
            .metadata_mut()
            .insert(AUTHORIZATION, format!("Bearer {token}").parse().unwrap());
        let status = kv.get(request).await.unwrap_err();
        assert_eq!(
            status.code(),
            tonic::Code::PermissionDenied,
            "{what} should be refused"
        );
    }
}

#[tokio::test]
async fn a_value_larger_than_the_tonic_default_still_goes_through() {
    let harness = Harness::start().await;
    let mut kv = harness.kv().await;
    // 5 MiB: over the 4 MB tonic default, under the 16 MB Deislet ceiling.
    let big = vec![b'a'; 5 * 1024 * 1024];

    kv.set(as_tenant(
        "app-one.production",
        SetRequest {
            key: "big".to_string(),
            value: big.clone(),
            ttl_seconds: None,
            metadata: None,
        },
    ))
    .await
    .unwrap();

    let got = kv
        .get(as_tenant(
            "app-one.production",
            GetRequest {
                key: "big".to_string(),
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(got.value.map(|v| v.len()), Some(big.len()));
}

#[tokio::test]
async fn database_executes_queries_batches_and_prepared_statements() {
    let harness = Harness::start().await;
    let mut db = harness.database().await;

    let created = db
        .execute(as_tenant(
            "app-one.production",
            ExecuteRequest {
                sql: "CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT)".to_string(),
                params: None,
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert!(created.success, "{:?}", created.error);

    let inserted = db
        .execute(as_tenant(
            "app-one.production",
            ExecuteRequest {
                sql: "INSERT INTO users(name) VALUES(?1)".to_string(),
                params: Some(r#"["Alice"]"#.to_string()),
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert!(inserted.success);
    assert_eq!(inserted.rows_affected, 1);
    assert_eq!(inserted.last_insert_id, Some(1));

    let queried = db
        .query(as_tenant(
            "app-one.production",
            QueryRequest {
                sql: "SELECT id, name FROM users".to_string(),
                params: None,
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert!(queried.success);
    assert_eq!(queried.columns, vec!["id".to_string(), "name".to_string()]);
    let rows: serde_json::Value = serde_json::from_str(&queried.rows).unwrap();
    assert_eq!(rows, serde_json::json!([{ "id": 1, "name": "Alice" }]));

    // A batch that fails halfway leaves nothing behind.
    let batch = db
        .batch(as_tenant(
            "app-one.production",
            BatchRequest {
                statements: vec![
                    ExecuteRequest {
                        sql: "INSERT INTO users(name) VALUES('Bob')".to_string(),
                        params: None,
                    },
                    ExecuteRequest {
                        sql: "INSERT INTO nope(name) VALUES('Carol')".to_string(),
                        params: None,
                    },
                ],
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert!(!batch.success);
    assert!(batch.error.unwrap().contains("rolled back"));

    let after = db
        .query(as_tenant(
            "app-one.production",
            QueryRequest {
                sql: "SELECT count(*) AS n FROM users".to_string(),
                params: None,
            },
        ))
        .await
        .unwrap()
        .into_inner();
    let rows: serde_json::Value = serde_json::from_str(&after.rows).unwrap();
    assert_eq!(rows, serde_json::json!([{ "n": 1 }]));

    // Prepare once, use twice.
    let prepared = db
        .prepare(as_tenant(
            "app-one.production",
            PrepareRequest {
                sql: "INSERT INTO users(name) VALUES(?1)".to_string(),
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert!(prepared.success, "{:?}", prepared.error);
    for name in ["Bob", "Carol"] {
        let done = db
            .execute_prepared(as_tenant(
                "app-one.production",
                ExecutePreparedRequest {
                    statement_id: prepared.statement_id.clone(),
                    params: Some(format!(r#"["{name}"]"#)),
                },
            ))
            .await
            .unwrap()
            .into_inner();
        assert!(done.success, "{:?}", done.error);
    }

    let select = db
        .prepare(as_tenant(
            "app-one.production",
            PrepareRequest {
                sql: "SELECT name FROM users WHERE name = ?1".to_string(),
            },
        ))
        .await
        .unwrap()
        .into_inner();
    let found = db
        .query_prepared(as_tenant(
            "app-one.production",
            QueryPreparedRequest {
                statement_id: select.statement_id.clone(),
                params: Some(r#"["Carol"]"#.to_string()),
            },
        ))
        .await
        .unwrap()
        .into_inner();
    let rows: serde_json::Value = serde_json::from_str(&found.rows).unwrap();
    assert_eq!(rows, serde_json::json!([{ "name": "Carol" }]));

    // Another tenant cannot use that handle, and has no such table anyway.
    let stolen = db
        .query_prepared(as_tenant(
            "app-two.production",
            QueryPreparedRequest {
                statement_id: select.statement_id,
                params: Some(r#"["Carol"]"#.to_string()),
            },
        ))
        .await
        .unwrap_err();
    assert_eq!(stolen.code(), tonic::Code::PermissionDenied);

    let missing = db
        .query(as_tenant(
            "app-two.production",
            QueryRequest {
                sql: "SELECT * FROM users".to_string(),
                params: None,
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert!(!missing.success);
    assert!(missing.error.unwrap().contains("users"));
}

#[tokio::test]
async fn bad_sql_is_reported_in_the_response_not_as_a_transport_error() {
    let harness = Harness::start().await;
    let mut db = harness.database().await;
    let response = db
        .execute(as_tenant(
            "app-one.production",
            ExecuteRequest {
                sql: "SELCT 1".to_string(),
                params: None,
            },
        ))
        .await
        .expect("a SQL typo must not look like the store is down")
        .into_inner();
    assert!(!response.success);
    assert!(response.error.is_some());
}

#[tokio::test]
async fn blob_put_and_get_stream_in_chunks() {
    let harness = Harness::start().await;
    let mut blob = harness.blob().await;

    // Two and a half chunks, so the streaming path is genuinely exercised.
    let body: Vec<u8> = (0..(2 * 1024 * 1024 + 512 * 1024))
        .map(|i| (i % 251) as u8)
        .collect();

    let mut messages = vec![PutRequest {
        payload: Some(put_request::Payload::Header(PutHeader {
            key: "video.bin".to_string(),
            precondition: None,
            content_type: Some("application/octet-stream".to_string()),
        })),
    }];
    for chunk in body.chunks(deis_store::STREAM_CHUNK_BYTES) {
        messages.push(PutRequest {
            payload: Some(put_request::Payload::Chunk(chunk.to_vec())),
        });
    }
    assert_eq!(messages.len(), 4, "header plus three body chunks");

    let stored = blob
        .put(as_tenant(
            "app-one.production",
            tokio_stream::iter(messages),
        ))
        .await
        .unwrap()
        .into_inner();
    assert!(stored.success, "{:?}", stored.error);
    assert_eq!(stored.size, body.len() as u64);
    assert_eq!(stored.sha256.len(), 64);

    let mut stream = blob
        .get(as_tenant(
            "app-one.production",
            deis_store::proto::blob::GetRequest {
                range: None,
                key: "video.bin".to_string(),
            },
        ))
        .await
        .unwrap()
        .into_inner();

    let mut info: Option<BlobInfo> = None;
    let mut received = Vec::new();
    let mut chunks = 0usize;
    while let Some(message) = stream.message().await.unwrap() {
        match message.payload.expect("every message carries a payload") {
            get_response::Payload::Info(i) => {
                assert!(info.is_none(), "info must arrive exactly once, first");
                info = Some(i);
            }
            get_response::Payload::Chunk(bytes) => {
                assert!(info.is_some(), "the header must precede the body");
                assert!(bytes.len() <= deis_store::STREAM_CHUNK_BYTES);
                chunks += 1;
                received.extend_from_slice(&bytes);
            }
        }
    }
    let info = info.unwrap();
    assert_eq!(info.key, "video.bin");
    assert_eq!(info.size, body.len() as u64);
    assert_eq!(info.sha256, stored.sha256);
    assert_eq!(
        info.content_type.as_deref(),
        Some("application/octet-stream")
    );
    assert_eq!(chunks, 3);
    assert_eq!(received, body);
}

/// Everything `BlobStore` grew beside the original three RPCs, over the wire
/// rather than through the storage type: `Head`, `List` with a delimiter and a
/// cursor, a ranged `Get` with the trailer that lets the slice be checked, and
/// a conditional `Put`. Each of those is a message shape two crates have to
/// agree on, and the agreement is only real if it is exercised on a socket.
#[tokio::test]
async fn blob_head_list_range_and_conditional_writes_over_grpc() {
    use deis_store::proto::blob::{
        ByteRange, ByteSpan, DeleteRequest as BlobDeleteRequest, HeadRequest,
        ListRequest as BlobListRequest, Precondition, byte_range, precondition,
    };

    let harness = Harness::start().await;
    let mut blob = harness.blob().await;
    let tenant = "app-one.production";

    let store = |blob: BlobStoreClient<Channel>,
                 key: &str,
                 body: &[u8],
                 condition: Option<Precondition>| {
        let mut blob = blob;
        let messages = vec![
            PutRequest {
                payload: Some(put_request::Payload::Header(PutHeader {
                    key: key.to_string(),
                    content_type: Some("text/plain".to_string()),
                    precondition: condition,
                })),
            },
            PutRequest {
                payload: Some(put_request::Payload::Chunk(body.to_vec())),
            },
        ];
        async move {
            blob.put(as_tenant(tenant, tokio_stream::iter(messages)))
                .await
                .unwrap()
                .into_inner()
        }
    };

    for key in ["img/a.png", "img/b.png", "index.html"] {
        let stored = store(blob.clone(), key, b"0123456789", None).await;
        assert!(stored.success, "{:?}", stored.error);
    }

    // Head: the metadata without the body, and an absent key as an answer
    // rather than a status.
    let head = blob
        .head(as_tenant(
            tenant,
            HeadRequest {
                key: "index.html".to_string(),
            },
        ))
        .await
        .unwrap()
        .into_inner()
        .info
        .expect("the object exists");
    assert_eq!(head.size, 10);
    assert_eq!(head.content_type.as_deref(), Some("text/plain"));
    assert_eq!(head.sha256.len(), 64);
    assert!(
        blob.head(as_tenant(
            tenant,
            HeadRequest {
                key: "nothing".to_string()
            }
        ))
        .await
        .unwrap()
        .into_inner()
        .info
        .is_none()
    );

    // List with a delimiter: one folder, one key, and a cursor because the
    // page was filled exactly.
    let page = blob
        .list(as_tenant(
            tenant,
            BlobListRequest {
                prefix: String::new(),
                limit: Some(2),
                cursor: None,
                delimiter: Some("/".to_string()),
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(page.prefixes, vec!["img/".to_string()]);
    assert_eq!(
        page.objects.iter().map(|o| &o.key).collect::<Vec<_>>(),
        vec!["index.html"]
    );
    let cursor = page.next_cursor.expect("a full page answers a cursor");
    let page = blob
        .list(as_tenant(
            tenant,
            BlobListRequest {
                prefix: String::new(),
                limit: Some(2),
                cursor: Some(cursor),
                delimiter: Some("/".to_string()),
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert!(page.objects.is_empty() && page.prefixes.is_empty());
    assert!(page.next_cursor.is_none());

    // A ranged Get: the header says which bytes are coming, the chunks carry
    // them, and a final message with no payload carries the digest of exactly
    // those bytes.
    let mut stream = blob
        .get(as_tenant(
            tenant,
            deis_store::proto::blob::GetRequest {
                key: "index.html".to_string(),
                range: Some(ByteRange {
                    spec: Some(byte_range::Spec::Span(ByteSpan {
                        offset: 2,
                        length: Some(4),
                    })),
                }),
            },
        ))
        .await
        .unwrap()
        .into_inner();
    let mut served = None;
    let mut body = Vec::new();
    let mut range_digest = None;
    let mut total = 0;
    while let Some(message) = stream.message().await.unwrap() {
        if let Some(digest) = message.range_sha256 {
            range_digest = Some(digest);
        }
        match message.payload {
            Some(get_response::Payload::Info(info)) => {
                total = info.size;
                served = message.range;
            }
            Some(get_response::Payload::Chunk(chunk)) => body.extend_from_slice(&chunk),
            None => {}
        }
    }
    assert_eq!(body, b"2345");
    let served = served.expect("a ranged read says which bytes it sent");
    assert_eq!((served.offset, served.length), (2, 4));
    assert_eq!(
        total, 10,
        "the whole object's size is what a Content-Range's total is built from"
    );
    let digest = range_digest.expect("a ranged read carries the digest of its slice");
    assert_eq!(
        digest,
        format!("{:x}", <sha2::Sha256 as sha2::Digest>::digest(b"2345")),
        "the trailer must hash the bytes that were sent, not the object"
    );

    // A range naming no byte: its own status code, so an application can tell
    // it from a missing key and answer 416 rather than 404.
    let status = blob
        .get(as_tenant(
            tenant,
            deis_store::proto::blob::GetRequest {
                key: "index.html".to_string(),
                range: Some(ByteRange {
                    spec: Some(byte_range::Spec::Span(ByteSpan {
                        offset: 10,
                        length: None,
                    })),
                }),
            },
        ))
        .await
        .unwrap_err();
    assert_eq!(status.code(), tonic::Code::OutOfRange);

    // A conditional Put that loses says so in its own field, not in prose.
    let refused = store(
        blob.clone(),
        "index.html",
        b"overwritten",
        Some(Precondition {
            condition: Some(precondition::Condition::IfNoneMatch("*".to_string())),
        }),
    )
    .await;
    assert!(!refused.success);
    let failure = refused
        .precondition_failed
        .expect("a lost conditional write is a field, not a sentence");
    assert_eq!(failure.condition, "If-None-Match: *");
    assert_eq!(failure.etag.as_deref(), Some(head.sha256.as_str()));

    // And one that holds, quoting the etag the way an HTTP header would.
    let written = store(
        blob.clone(),
        "index.html",
        b"replaced",
        Some(Precondition {
            condition: Some(precondition::Condition::IfMatch(format!(
                "\"{}\"",
                head.sha256
            ))),
        }),
    )
    .await;
    assert!(written.success, "{:?}", written.error);

    // A conditional Delete against the etag that has just been superseded.
    let refused = blob
        .delete(as_tenant(
            tenant,
            BlobDeleteRequest {
                key: "index.html".to_string(),
                precondition: Some(Precondition {
                    condition: Some(precondition::Condition::IfMatch(head.sha256.clone())),
                }),
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert!(!refused.success);
    assert!(refused.precondition_failed.is_some());
}

#[tokio::test]
async fn blob_delete_and_missing_keys() {
    let harness = Harness::start().await;
    let mut blob = harness.blob().await;

    let missing = blob
        .get(as_tenant(
            "app-one.production",
            deis_store::proto::blob::GetRequest {
                range: None,
                key: "absent".to_string(),
            },
        ))
        .await
        .unwrap_err();
    assert_eq!(missing.code(), tonic::Code::NotFound);

    let messages = vec![
        PutRequest {
            payload: Some(put_request::Payload::Header(PutHeader {
                key: "note.txt".to_string(),
                precondition: None,
                content_type: Some("text/plain".to_string()),
            })),
        },
        PutRequest {
            payload: Some(put_request::Payload::Chunk(b"hello".to_vec())),
        },
    ];
    let stored = blob
        .put(as_tenant(
            "app-one.production",
            tokio_stream::iter(messages),
        ))
        .await
        .unwrap()
        .into_inner();
    assert!(stored.success);

    // Another tenant cannot read it.
    let other = blob
        .get(as_tenant(
            "app-two.production",
            deis_store::proto::blob::GetRequest {
                range: None,
                key: "note.txt".to_string(),
            },
        ))
        .await
        .unwrap_err();
    assert_eq!(other.code(), tonic::Code::NotFound);

    let deleted = blob
        .delete(as_tenant(
            "app-one.production",
            deis_store::proto::blob::DeleteRequest {
                precondition: None,
                key: "note.txt".to_string(),
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert!(deleted.success);

    // Deleting again still succeeds: the post-condition already holds.
    let again = blob
        .delete(as_tenant(
            "app-one.production",
            deis_store::proto::blob::DeleteRequest {
                precondition: None,
                key: "note.txt".to_string(),
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert!(again.success);
}

#[tokio::test]
async fn blob_put_must_start_with_a_header() {
    let harness = Harness::start().await;
    let mut blob = harness.blob().await;
    let status = blob
        .put(as_tenant(
            "app-one.production",
            tokio_stream::iter(vec![PutRequest {
                payload: Some(put_request::Payload::Chunk(b"orphan".to_vec())),
            }]),
        ))
        .await
        .unwrap_err();
    assert_eq!(status.code(), tonic::Code::InvalidArgument);
}

#[tokio::test]
async fn an_oversized_object_is_refused_in_the_response() {
    let harness = Harness::start().await;
    let mut blob = harness.blob().await;

    // The harness caps objects at 8 MiB; send nine.
    let mut messages = vec![PutRequest {
        payload: Some(put_request::Payload::Header(PutHeader {
            key: "too-big".to_string(),
            precondition: None,
            content_type: None,
        })),
    }];
    for _ in 0..9 {
        messages.push(PutRequest {
            payload: Some(put_request::Payload::Chunk(vec![
                0u8;
                deis_store::STREAM_CHUNK_BYTES
            ])),
        });
    }

    let response = blob
        .put(as_tenant(
            "app-one.production",
            tokio_stream::iter(messages),
        ))
        .await
        .unwrap()
        .into_inner();
    assert!(!response.success);
    assert!(response.error.unwrap().contains("limit"));

    let absent = blob
        .get(as_tenant(
            "app-one.production",
            deis_store::proto::blob::GetRequest {
                range: None,
                key: "too-big".to_string(),
            },
        ))
        .await
        .unwrap_err();
    assert_eq!(absent.code(), tonic::Code::NotFound);
}

#[tokio::test]
async fn health_reports_every_service_as_serving() {
    let harness = Harness::start().await;
    let mut health = tonic_health::pb::health_client::HealthClient::new(harness.channel().await);
    for service in [
        "deis.storage.KvStore",
        "deis.storage.Database",
        "deis.blob.BlobStore",
    ] {
        let status = health
            .check(tonic_health::pb::HealthCheckRequest {
                service: service.to_string(),
            })
            .await
            .unwrap()
            .into_inner();
        assert_eq!(
            status.status,
            tonic_health::pb::health_check_response::ServingStatus::Serving as i32,
            "{service} should be serving"
        );
    }
}

/// Two environments of one application must not see each other's data.
///
/// This is the whole reason the storage tenant is a pair rather than an app
/// id: a preview deployment that could read or overwrite what production
/// stored would be an unrecoverable failure, not a confusing one.
#[tokio::test]
async fn two_environments_of_one_application_share_nothing() {
    let harness = Harness::start().await;
    let production = tenant_id("app-one", "production");
    let preview = tenant_id("app-one", "preview");

    // KV: the same key in both, with different values.
    let mut kv = harness.kv().await;
    for (tenant, value) in [(&production, "live"), (&preview, "draft")] {
        kv.set(as_tenant(
            tenant,
            SetRequest {
                key: "banner".to_string(),
                value: value.as_bytes().to_vec(),
                ttl_seconds: None,
                metadata: None,
            },
        ))
        .await
        .unwrap();
    }
    for (tenant, expected) in [(&production, "live"), (&preview, "draft")] {
        let got = kv
            .get(as_tenant(
                tenant,
                GetRequest {
                    key: "banner".to_string(),
                },
            ))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(
            got.value.as_deref(),
            Some(expected.as_bytes()),
            "{tenant} kv"
        );
    }

    // SQL: a table created by one environment does not exist in the other.
    let mut database = harness.database().await;
    database
        .execute(as_tenant(
            &production,
            ExecuteRequest {
                sql: "CREATE TABLE orders (id INTEGER)".to_string(),
                params: None,
            },
        ))
        .await
        .unwrap();
    let missing = database
        .query(as_tenant(
            &preview,
            QueryRequest {
                sql: "SELECT * FROM orders".to_string(),
                params: None,
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert!(
        !missing.error.unwrap_or_default().is_empty(),
        "the preview environment must not see production's table"
    );

    // Blob: the same key in both, with different bytes.
    let mut blob = harness.blob().await;
    for (tenant, body) in [(&production, b"live-bytes"), (&preview, b"prev-bytes")] {
        let messages = vec![
            PutRequest {
                payload: Some(put_request::Payload::Header(PutHeader {
                    key: "logo".to_string(),
                    precondition: None,
                    content_type: None,
                })),
            },
            PutRequest {
                payload: Some(put_request::Payload::Chunk(body.to_vec())),
            },
        ];
        let response = blob
            .put(as_tenant(tenant, tokio_stream::iter(messages)))
            .await
            .unwrap()
            .into_inner();
        assert!(response.success, "{tenant} put");
    }
    for (tenant, expected) in [
        (&production, b"live-bytes".as_slice()),
        (&preview, b"prev-bytes".as_slice()),
    ] {
        let mut stream = blob
            .get(as_tenant(
                tenant,
                deis_store::proto::blob::GetRequest {
                    key: "logo".to_string(),
                    range: None,
                },
            ))
            .await
            .unwrap()
            .into_inner();
        let mut body = Vec::new();
        while let Some(message) = stream.message().await.unwrap() {
            if let Some(get_response::Payload::Chunk(chunk)) = message.payload {
                body.extend_from_slice(&chunk);
            }
        }
        assert_eq!(body, expected, "{tenant} blob");
    }
}

// ---- the capability token, over a real server -------------------------
//
// The checks below go through the same interceptor a deployed process runs,
// which is the point: a token check that lives only in a unit test is a token
// check nothing applies. Every service that touches tenant data is exercised,
// because each one is registered separately and each one could have been
// registered without the guard.

#[tokio::test]
async fn a_signed_call_is_served_when_a_secret_is_configured() {
    let harness = Harness::start_authenticated().await;
    let tenant = tenant_id("app-one", "production");
    let mut kv = harness.kv().await;

    kv.set(as_tenant_with_token(
        &tenant,
        &tenant,
        "kv",
        SetRequest {
            key: "greeting".to_string(),
            value: b"hello".to_vec(),
            ttl_seconds: None,
            metadata: None,
        },
    ))
    .await
    .unwrap();

    let got = kv
        .get(as_tenant_with_token(
            &tenant,
            &tenant,
            "kv",
            GetRequest {
                key: "greeting".to_string(),
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(got.value, Some(b"hello".to_vec()));
}

#[tokio::test]
async fn every_service_refuses_a_call_that_carries_no_token() {
    // One case per service: they are registered one by one, so this is three
    // separate pieces of evidence that the guard was applied.
    let harness = Harness::start_authenticated().await;
    let tenant = tenant_id("app-one", "production");

    let status = harness
        .kv()
        .await
        .get(as_tenant_no_token(
            &tenant,
            GetRequest {
                key: "greeting".to_string(),
            },
        ))
        .await
        .unwrap_err();
    assert_eq!(status.code(), tonic::Code::Unauthenticated, "KvStore");

    let status = harness
        .database()
        .await
        .query(as_tenant_no_token(
            &tenant,
            QueryRequest {
                sql: "SELECT 1".to_string(),
                params: None,
            },
        ))
        .await
        .unwrap_err();
    assert_eq!(status.code(), tonic::Code::Unauthenticated, "Database");

    let status = harness
        .blob()
        .await
        .delete(as_tenant_no_token(
            &tenant,
            deis_store::proto::blob::DeleteRequest {
                precondition: None,
                key: "an-object".to_string(),
            },
        ))
        .await
        .unwrap_err();
    assert_eq!(status.code(), tonic::Code::Unauthenticated, "BlobStore");

    // And a streaming method, which opens its response before the handler
    // has produced anything: the guard has to refuse it at the call rather
    // than partway down a body the caller is already reading.
    let status = harness
        .blob()
        .await
        .get(as_tenant_no_token(
            &tenant,
            deis_store::proto::blob::GetRequest {
                range: None,
                key: "an-object".to_string(),
            },
        ))
        .await
        .map(|_| ())
        .unwrap_err();
    assert_eq!(
        status.code(),
        tonic::Code::Unauthenticated,
        "BlobStore.Get streams, and must be refused before the first chunk"
    );
}

#[tokio::test]
async fn a_tenant_named_in_the_request_opens_nothing() {
    // The tenant is the one the capability names. A caller holding a token
    // for its own tenant that also names another one in a header - the
    // retired `x-deis-tenant` - is served its own data, never the other's.
    let harness = Harness::start_authenticated().await;
    let mine = tenant_id("app-one", "production");
    let mut kv = harness.kv().await;
    for owner in [
        tenant_id("app-two", "production"),
        tenant_id("app-one", "preview"),
    ] {
        kv.set(as_tenant(
            &owner,
            SetRequest {
                key: "greeting".to_string(),
                value: owner.as_bytes().to_vec(),
                ttl_seconds: None,
                metadata: None,
            },
        ))
        .await
        .unwrap();
        let got = kv
            .get(as_tenant_with_token(
                &owner,
                &mine,
                "kv",
                GetRequest {
                    key: "greeting".to_string(),
                },
            ))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(got.value, None, "a token for {mine} must not open {owner}");
    }
}

#[tokio::test]
async fn a_token_from_a_different_secret_is_refused() {
    // What a half-finished key rotation looks like from the store's side.
    // The server verifies with test_signing_key, but we mint a token with
    // a different key — the signature will not verify.
    let harness = Harness::start_authenticated().await;
    let tenant = tenant_id("app-one", "production");

    // Sign with a different key than the one the server trusts.
    let mut other_seed = [0u8; 32];
    other_seed[0..8].copy_from_slice(b"otherkey");
    let other_key = SigningKey::from_bytes(&other_seed);
    let bad_claims = CapabilityClaims::new(
        "store",
        "spiffe://deislet/control/main",
        "spiffe://deislet/node/test-node",
        1,
        Duration::from_secs(TOKEN_LIFETIME_SECS),
        vec![CapabilityResource {
            tenant: tenant.to_string(),
            binding: "kv".to_string(),
            generation: None,
        }],
    );
    let bad_token = encode(&bad_claims, &other_key);
    let mut request = as_tenant(
        &tenant,
        GetRequest {
            key: "greeting".to_string(),
        },
    );
    request.metadata_mut().insert(
        AUTHORIZATION,
        format!("Bearer {bad_token}").parse().unwrap(),
    );
    let status = harness.kv().await.get(request).await.unwrap_err();
    assert_eq!(status.code(), tonic::Code::Unauthenticated);
}

#[tokio::test]
async fn health_answers_without_a_token() {
    // The control plane probes liveness without knowing any tenant, and the
    // answer names none: a probe that needed a tenant credential is a probe
    // that cannot be made.
    let harness = Harness::start_authenticated().await;
    let mut health = tonic_health::pb::health_client::HealthClient::new(harness.channel().await);
    let status = health
        .check(tonic_health::pb::HealthCheckRequest {
            service: "deis.storage.KvStore".to_string(),
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(
        status.status,
        tonic_health::pb::health_check_response::ServingStatus::Serving as i32
    );
}

#[tokio::test]
async fn a_public_bind_still_works_when_authenticated() {
    // All servers require capability-based auth. Serving on a public
    // interface is fine as long as a verifying key is provided.
    let data = TempDir::new().unwrap();
    let config = Config {
        listen: "127.0.0.1:0".to_string(),
        data_dir: data.path().to_path_buf(),
        ..Config::default()
    };
    let listener = server::bind(&config).await.unwrap();
    let public = std::net::TcpListener::bind("0.0.0.0:0").unwrap();
    public.set_nonblocking(true).unwrap();
    let public = tokio::net::TcpListener::from_std(public).unwrap();
    drop(listener);

    let keys = KeyRing::from_keys([test_signing_key().verifying_key()]);
    let fleet = TestFleet::new();
    let result = server::serve(
        config,
        public,
        async {},
        keys,
        fleet.service_identity(ServiceKind::Store),
    )
    .await;
    // With a verifying key always provided, binding on a public interface
    // is allowed — capability auth secures every call regardless of
    // interface.
    assert!(result.is_ok());
}

/// The whole-node outage a named binding caused, driven over the wire and
/// then restarted the way the process is.
///
/// A call carrying `x-deis-binding` is stored under `<tenant>@<BINDING>`, so
/// the first application on a node with a `db` or `blob` binding leaves
/// `db/shop.production@MEDIA.sqlite` and `blob/shop.production@MEDIA/` behind.
/// The startup scan then had to open over them and refused, so the process
/// exited and KV, SQL and blob were down for *every* tenant on the node -
/// with "point data_dir at an empty directory" as the only remedy offered.
///
/// The names are asserted from what the server wrote in response to the
/// headers alone, so this cannot pass by agreeing with a name the test
/// invented.
#[tokio::test]
async fn a_named_binding_written_over_the_wire_survives_a_restart() {
    let data = TempDir::new().unwrap();

    {
        let harness = Harness::start_on(data.path()).await;
        harness
            .kv()
            .await
            .set(as_binding(
                "shop.production",
                "MEDIA",
                SetRequest {
                    key: "greeting".to_string(),
                    value: b"in the binding".to_vec(),
                    ttl_seconds: None,
                    metadata: None,
                },
            ))
            .await
            .unwrap();
        harness
            .database()
            .await
            .execute(as_binding(
                "shop.production",
                "MEDIA",
                ExecuteRequest {
                    sql: "CREATE TABLE t(a TEXT)".to_string(),
                    params: None,
                },
            ))
            .await
            .unwrap();
        let stored = harness
            .blob()
            .await
            .put(as_binding(
                "shop.production",
                "MEDIA",
                tokio_stream::iter(vec![
                    PutRequest {
                        payload: Some(put_request::Payload::Header(PutHeader {
                            key: "photo".to_string(),
                            precondition: None,
                            content_type: None,
                        })),
                    },
                    PutRequest {
                        payload: Some(put_request::Payload::Chunk(b"bytes".to_vec())),
                    },
                ]),
            ))
            .await
            .unwrap()
            .into_inner();
        assert!(stored.success, "{:?}", stored.error);
    }

    // What the binding actually put on disk, in the server's own spelling.
    assert!(
        data.path()
            .join("db")
            .join(format!("{}@MEDIA.sqlite", key_of("shop.production")))
            .is_file(),
        "a database binding is stored under the namespace, not the tenant: {:?}",
        std::fs::read_dir(data.path().join("db"))
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect::<Vec<_>>()
    );
    assert!(
        data.path()
            .join("blob")
            .join(format!("{}@MEDIA", key_of("shop.production")))
            .is_dir()
    );

    // The restart itself: exactly the call `server::serve` makes before it
    // accepts anything, which is where the refusal was.
    let reopened = server::Stores::open(&Config {
        data_dir: data.path().to_path_buf(),
        ..Config::default()
    })
    .await;
    assert!(
        reopened.is_ok(),
        "a data directory holding a named binding must open again, or every tenant on the node \
         loses KV, SQL and blob: {:?}",
        reopened.err()
    );
    drop(reopened);

    // And the data is the binding's own, not the tenant's unnamed namespace.
    let harness = Harness::start_on(data.path()).await;
    let got = harness
        .kv()
        .await
        .get(as_binding(
            "shop.production",
            "MEDIA",
            GetRequest {
                key: "greeting".to_string(),
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(got.value.as_deref(), Some(&b"in the binding"[..]));

    let unnamed = harness
        .kv()
        .await
        .get(as_tenant(
            "shop.production",
            GetRequest {
                key: "greeting".to_string(),
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(
        unnamed.value, None,
        "the tenant's unnamed namespace never held this key"
    );
}

#[tokio::test]
async fn a_single_chunk_above_the_message_ceiling_is_refused_not_an_internal_error() {
    // The client is told to send 1 MiB chunks. One that sends a single 17 MiB
    // message is breaking the protocol, and used to be answered Internal - the
    // code for "the store is broken" - because the decode failure on the
    // inbound stream was folded into an I/O error. It is the caller's fault
    // and the answer has to say so, naming the ceiling it went over.
    let harness = Harness::start().await;
    let mut blob = BlobStoreClient::new(harness.channel().await)
        .max_decoding_message_size(MAX_GRPC_MESSAGE_BYTES)
        // Raised on the client only, so the oversized frame leaves at all.
        .max_encoding_message_size(2 * MAX_GRPC_MESSAGE_BYTES);

    let messages = vec![
        PutRequest {
            payload: Some(put_request::Payload::Header(PutHeader {
                key: "one-giant-chunk".to_string(),
                precondition: None,
                content_type: None,
            })),
        },
        PutRequest {
            payload: Some(put_request::Payload::Chunk(vec![
                0u8;
                MAX_GRPC_MESSAGE_BYTES
                    + 1024
            ])),
        },
    ];

    let response = blob
        .put(as_tenant(
            "app-one.production",
            tokio_stream::iter(messages),
        ))
        .await
        .expect("a protocol violation by the caller is not an internal error")
        .into_inner();
    assert!(!response.success);
    let error = response.error.unwrap_or_default();
    assert!(
        error.contains(&MAX_GRPC_MESSAGE_BYTES.to_string()),
        "the refusal names the ceiling: {error}"
    );

    let absent = blob
        .get(as_tenant(
            "app-one.production",
            deis_store::proto::blob::GetRequest {
                range: None,
                key: "one-giant-chunk".to_string(),
            },
        ))
        .await
        .unwrap_err();
    assert_eq!(absent.code(), tonic::Code::NotFound);
}

/// The transport half of the store's contract: a caller that trusts this
/// fleet's authority refuses a server whose certificate that authority did
/// not sign, and refuses one it did sign for another kind of machine - so a
/// bearer capability is never handed to whoever merely answers at the
/// store's address.
#[tokio::test]
async fn a_caller_refuses_a_store_the_fleet_did_not_sign_for() {
    let harness = Harness::start().await;

    // Signed by another authority altogether.
    let stranger = TestFleet::new();
    let refused = Endpoint::from_shared(harness.endpoint.clone())
        .unwrap()
        .connect_with_connector(
            stranger
                .service_trust()
                .dialler(ServiceKind::Store, &[Protocol::Http2]),
        )
        .await;
    assert!(
        refused.is_err(),
        "a store certificate from an unknown authority was accepted"
    );

    // Signed by this fleet, but the caller meant to reach a queue.
    let refused = Endpoint::from_shared(harness.endpoint.clone())
        .unwrap()
        .connect_with_connector(
            harness
                .fleet
                .service_trust()
                .dialler(ServiceKind::Queue, &[Protocol::Http2]),
        )
        .await;
    assert!(
        refused.is_err(),
        "a store's certificate answered for a queue"
    );

    // And plaintext gets no answer a client could mistake for a store.
    let plaintext = Endpoint::from_shared(harness.endpoint.replace("https://", "http://"))
        .unwrap()
        .connect()
        .await
        .unwrap();
    let mut kv = KvStoreClient::new(plaintext);
    assert!(
        kv.get(as_tenant(
            "app-one.production",
            GetRequest { key: "k".into() }
        ))
        .await
        .is_err(),
        "the store answered a plaintext call"
    );
}
