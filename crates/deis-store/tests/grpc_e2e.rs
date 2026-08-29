//! End-to-end checks against a real deis-store server.
//!
//! Everything here goes over a real TCP socket and the real generated
//! clients, so it exercises the parts unit tests cannot: metadata handling,
//! the streaming blob protocol, the message size ceiling, and the fact that
//! the three services are actually registered on the server.
//!
//! Every call stamps `x-deis-tenant` with an `<app>.<environment>` value,
//! which is what deis-runtime has to send. The tests below use
//! `app-one.production` and `app-two.production` as two unrelated tenants,
//! and one test pairs `app-one.production` with `app-one.preview` to check
//! that two environments of the *same* application are also two namespaces.

use deis_store::config::Config;
use deis_store::proto::blob::{
    BlobInfo, PutHeader, PutRequest, blob_store_client::BlobStoreClient, get_response, put_request,
};
use deis_store::proto::storage::{
    BatchRequest, ExecutePreparedRequest, ExecuteRequest, GetRequest, HasRequest, ListRequest,
    PrepareRequest, QueryPreparedRequest, QueryRequest, SetRequest,
    database_client::DatabaseClient, kv_store_client::KvStoreClient,
};
use deis_store::tenant::TENANT_HEADER;
use deis_store::{MAX_GRPC_MESSAGE_BYTES, server};
use deis_wire::conventions::{APP_ID_HEADER, tenant_id};
use tempfile::TempDir;
use tokio::sync::oneshot;
use tonic::Request;
use tonic::transport::Channel;

/// A running server plus the temp directory holding its data.
struct Harness {
    _data: TempDir,
    endpoint: String,
    shutdown: Option<oneshot::Sender<()>>,
}

impl Harness {
    async fn start() -> Self {
        let data = TempDir::new().unwrap();
        let config = Config {
            listen: "127.0.0.1:0".to_string(),
            data_dir: data.path().to_path_buf(),
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
        tokio::spawn(async move {
            server::serve(config, listener, async {
                let _ = rx.await;
            })
            .await
            .unwrap();
        });
        Self {
            _data: data,
            endpoint: format!("http://{addr}"),
            shutdown: Some(shutdown),
        }
    }

    async fn channel(&self) -> Channel {
        Channel::from_shared(self.endpoint.clone())
            .unwrap()
            .connect()
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

/// Stamp the tenant the way deis-runtime will have to.
fn as_tenant<T>(tenant: &str, message: T) -> Request<T> {
    let mut request = Request::new(message);
    request
        .metadata_mut()
        .insert(TENANT_HEADER, tenant.parse().unwrap());
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
            value: "hello".to_string(),
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
    assert_eq!(got.value.as_deref(), Some("hello"));

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
            value: "token".to_string(),
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
    assert_eq!(got.value.as_deref(), Some("token"));

    // A lifetime that cannot be honoured is refused outright, rather than
    // stored as something no reader will ever see again.
    let refused = kv
        .set(as_tenant(
            "app-one.production",
            SetRequest {
                key: "doomed".to_string(),
                value: "v".to_string(),
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
                value: "v".to_string(),
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
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(rest.keys, vec!["user:3"]);
    assert_eq!(rest.next_cursor, None);
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
    assert_eq!(status.code(), tonic::Code::PermissionDenied);

    for bad in [
        // Would escape the data directory.
        "../../etc",
        // A bare application id: what every caller sent before the tenant
        // contract. It names no environment, so it names no tenant.
        "app-one",
        // Neither too many parts nor an empty one is a tenant.
        "a.b.c",
        "app-one.",
    ] {
        let status = kv
            .get(as_tenant(
                bad,
                GetRequest {
                    key: "greeting".to_string(),
                },
            ))
            .await
            .unwrap_err();
        assert_eq!(
            status.code(),
            tonic::Code::PermissionDenied,
            "{bad} should be refused"
        );
    }
}

#[tokio::test]
async fn a_value_larger_than_the_tonic_default_still_goes_through() {
    let harness = Harness::start().await;
    let mut kv = harness.kv().await;
    // 5 MiB: over the 4 MB tonic default, under the 16 MB Deislet ceiling.
    let big = "a".repeat(5 * 1024 * 1024);

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

#[tokio::test]
async fn blob_delete_and_missing_keys() {
    let harness = Harness::start().await;
    let mut blob = harness.blob().await;

    let missing = blob
        .get(as_tenant(
            "app-one.production",
            deis_store::proto::blob::GetRequest {
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
                value: value.to_string(),
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
        assert_eq!(got.value.as_deref(), Some(expected), "{tenant} kv");
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

/// The application header is not the storage tenant. A caller that stamps
/// only `x-deis-app-id` - which is what every caller did before this change -
/// is refused rather than served production's data.
#[tokio::test]
async fn the_application_header_alone_does_not_identify_a_tenant() {
    let harness = Harness::start().await;
    let mut kv = harness.kv().await;
    let mut request = Request::new(GetRequest {
        key: "anything".to_string(),
    });
    request
        .metadata_mut()
        .insert(APP_ID_HEADER, "app-one".parse().unwrap());
    let status = kv.get(request).await.unwrap_err();
    assert_eq!(status.code(), tonic::Code::PermissionDenied);
}
