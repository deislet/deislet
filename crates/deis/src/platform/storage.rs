//! A gRPC client for one tenant's KV store, database and blob storage.
//!
//! Backs `deis kv`, `deis db` and `deis blob`. It deliberately does not
//! depend on `deis-runtime`, which has an identically-shaped client for the
//! same two proto files: pulling that crate in here would drag a V8 isolate
//! pool into a CLI binary for two header inserts and a handful of RPC calls.
//! Instead this module generates its own client-only stubs from the same
//! proto files (see `../../build.rs`), which is exactly the pattern this
//! crate already used for `deis-telemetry` and `deis-runtime`'s own
//! `management.proto`.
//!
//! # Identity on the wire
//!
//! Every call carries the tenant in `x-deis-tenant` and, when one was
//! handed to this session, the capability token that proves it may act as
//! that tenant in `Authorization: Bearer` - the same two headers
//! `deis-runtime` stamps for an application's own storage calls, spelled
//! once in `deis_wire::conventions` so the three components that read and
//! write them cannot drift apart. A session built with no token stamps only
//! the tenant, which deis-store refuses: every call needs a capability.
//!
//! # What is and is not implemented
//!
//! Every RPC used here already exists on deis-store - see
//! `crates/deis-store/proto/storage.proto` and `blob.proto`. What is not
//! wrapped is `BlobStore.Get`'s streaming-to-disk case: an object still
//! arrives in memory and is written out afterwards, bounded by
//! [`DEFAULT_MAX_BLOB_BYTES`], and a range is how an operator takes a slice of
//! something bigger.

use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use tonic::Request;
use tonic::metadata::{Ascii, MetadataValue};
use tonic::transport::{Channel, Endpoint};

use deis_wire::conventions::{
    BINDING_HEADER, MAX_GRPC_MESSAGE_BYTES, STREAM_CHUNK_BYTES, TENANT_HEADER,
    is_valid_binding_name, split_tenant,
};

/// KV and the SQL database share one proto file and one gRPC service
/// definition each; blob storage is a separate file with its own streaming
/// contract. Kept as two modules, matching the two files, rather than one:
/// both declare a `GetRequest` and a `DeleteRequest` with different shapes,
/// and a single glob import of both would not compile.
mod storage_proto {
    tonic::include_proto!("deis.storage");
}

mod blob_proto {
    tonic::include_proto!("deis.blob");
}

use storage_proto::database_client::DatabaseClient;
use storage_proto::kv_store_client::KvStoreClient;

use blob_proto::blob_store_client::BlobStoreClient;

/// How long dialling deis-store may take before a unary call gives up.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// How long one KV or database call may run. A blob transfer gets no such
/// deadline - see [`StorageSession::connect`].
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Largest object [`StorageSession::blob_get`] will hold in memory by
/// default. An operator moving bigger objects raises it with `--max-size` on
/// `deis blob get`; this module has no opinion beyond "some ceiling must
/// exist", because an unbounded read of an object of unknown size is an
/// unbounded amount of this CLI's memory.
pub const DEFAULT_MAX_BLOB_BYTES: usize = 64 * 1024 * 1024;

/// Upper bound on what a download preallocates from the size deis-store
/// declares up front. The declared size is a hint for `Vec::with_capacity`,
/// never a promise, so a wrong or corrupted value must not itself become a
/// huge allocation ahead of the [`DEFAULT_MAX_BLOB_BYTES`] check.
const MAX_PREALLOC_BYTES: usize = 8 * 1024 * 1024;

/// Who a call says it is, and the proof that goes with the claim.
///
/// Parsed once at construction so a tenant or token that cannot be sent as
/// gRPC metadata is refused when the session is built, not on the first call
/// a person happens to make.
#[derive(Clone)]
struct Identity {
    tenant_header: MetadataValue<Ascii>,
    /// Raw token string for building the Authorization: Bearer header.
    token: Option<String>,
    /// Which namespace inside the tenant these calls act in, or `None` for the
    /// tenant's unnamed one - the namespace an application reaches through
    /// `context.services.kv` and the one every application had before named
    /// bindings existed.
    ///
    /// Not part of the credential and deliberately not treated as one: the
    /// token authorises the tenant, and a binding is always inside the tenant
    /// it is stamped beside. Naming a binding an application never declared
    /// reaches an empty namespace of the operator's own tenant, which is why
    /// this needs no separate permission.
    binding_header: Option<MetadataValue<Ascii>>,
}

impl Identity {
    fn new(tenant: String, token: Option<&str>, binding: Option<&str>) -> Result<Self> {
        if split_tenant(&tenant).is_none() {
            bail!(
                "{tenant:?} is not a storage tenant; it must be an application and an environment \
                 joined by '.', for example \"demo.production\". This is built from --app and \
                 --environment by deis_wire::conventions::tenant_id, so seeing this means one of \
                 them failed its own name check first."
            );
        }
        let tenant_header = tenant.parse::<MetadataValue<Ascii>>().map_err(|_| {
            anyhow!("tenant {tenant:?} cannot be sent as gRPC metadata; every call would fail")
        })?;
        let trimmed_token = token.map(str::trim).filter(|t| !t.is_empty());
        // Validate the token can be sent as gRPC metadata at construction time,
        // rather than failing on the first call.
        if let Some(t) = trimmed_token {
            t.parse::<MetadataValue<Ascii>>().map_err(|_| {
                anyhow!(
                    "the storage capability token for {tenant} cannot be sent as gRPC metadata; \
                     the control plane is expected to hand this CLI a base64 value"
                )
            })?;
        }
        let binding_header =
            match binding.map(str::trim).filter(|b| !b.is_empty()) {
                Some(binding) => {
                    if !is_valid_binding_name(binding) {
                        bail!(
                            "{binding:?} is not a binding name: a name is 1 to 64 characters of \
                         ASCII letters, digits, '_' or '-', because it becomes part of a \
                         directory and a database file name in deis-store. Leave --binding out \
                         to read and write the namespace `context.services.kv` uses."
                        );
                    }
                    Some(binding.parse::<MetadataValue<Ascii>>().map_err(|_| {
                        anyhow!("binding {binding:?} cannot be sent as gRPC metadata")
                    })?)
                }
                None => None,
            };
        Ok(Self {
            tenant_header,
            token: trimmed_token.map(str::to_string),
            binding_header,
        })
    }

    fn stamp<T>(&self, message: T) -> Request<T> {
        let mut request = Request::new(message);
        request
            .metadata_mut()
            .insert(TENANT_HEADER, self.tenant_header.clone());
        if let Some(token) = &self.token {
            let bearer = format!("Bearer {}", token);
            request
                .metadata_mut()
                .insert("authorization", bearer.parse().expect("valid metadata"));
        }
        // Stamped here rather than per call for the reason the token is: a
        // call that named the tenant and forgot the binding would read and
        // write the unnamed namespace while the operator believed they were
        // looking at the one they asked for, and nothing would fail.
        if let Some(binding) = &self.binding_header {
            request
                .metadata_mut()
                .insert(BINDING_HEADER, binding.clone());
        }
        request
    }
}

/// Never derived: a derived `Debug` would put the capability token into any
/// panic message or log line that formats this session, which is exactly the
/// place a credential must never appear. `token` therefore prints only
/// whether one is present - see `deis-store`'s own `StorageAuth::fmt` for the
/// same reasoning on the side that checks it.
impl std::fmt::Debug for Identity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Identity")
            .field(
                "tenant",
                &self.tenant_header.to_str().unwrap_or("<unprintable>"),
            )
            .field(
                "token",
                &if self.token.is_some() {
                    "<present>"
                } else {
                    "<absent>"
                },
            )
            .field(
                "binding",
                &self
                    .binding_header
                    .as_ref()
                    .and_then(|binding| binding.to_str().ok())
                    .unwrap_or("<unnamed>"),
            )
            .finish()
    }
}

/// One value as the KV store holds it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KvValue {
    pub value: Vec<u8>,
    pub metadata: Option<String>,
    pub expires_at: Option<i64>,
}

/// One key as a listing reports it. The value is never included - see
/// `KvEntry` in `storage.proto`: a page can hold thousands of keys, and one
/// carrying every value could itself exceed the gRPC message ceiling.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KvListEntry {
    pub key: String,
    pub metadata: Option<String>,
    pub expires_at: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KvListPage {
    pub entries: Vec<KvListEntry>,
    /// `Some` exactly when deis-store filled the page to the limit - not the
    /// same as "there is more". See `ListResponse` in `storage.proto`.
    pub next_cursor: Option<String>,
}

/// The result of a successful `Query`: column names in the order deis-store
/// sent them, and the rows as it encoded them - one JSON object per row,
/// keyed by column name.
#[derive(Debug, Clone)]
pub struct QueryResult {
    pub columns: Vec<String>,
    pub rows: serde_json::Value,
}

/// What deis-store knew about a blob object.
#[derive(Debug, Clone)]
pub struct BlobInfo {
    pub sha256: String,
    pub size: u64,
    pub content_type: Option<String>,
    pub created_at: i64,
}

#[derive(Debug, Clone)]
pub struct BlobValue {
    pub info: BlobInfo,
    /// Which bytes `data` holds; `None` for a whole-object read.
    pub range: Option<BlobRangeAnswer>,
    pub data: Vec<u8>,
}

#[derive(Debug, Clone)]
pub struct BlobPutResult {
    pub sha256: String,
    pub size: u64,
}

/// Which bytes of an object to read, in the terms RFC 9110 gives HTTP's
/// `Range` header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlobRange {
    /// From `offset` to the end, or `length` bytes of it.
    Span { offset: u64, length: Option<u64> },
    /// The last `n` bytes.
    Suffix(u64),
}

/// Which bytes a ranged read actually carried, resolved by deis-store against
/// the object's real size.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BlobRangeAnswer {
    pub offset: u64,
    pub length: u64,
}

/// What a conditional write requires of the object first.
///
/// The etag is the object's `sha256`: the same string `deis blob head` prints
/// and `deis blob put` answers with, which is what makes a compare-and-set
/// from the command line possible at all.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BlobCondition {
    /// `--if-match`: the etag must be this one, or `*` for any existing
    /// object.
    IfMatch(String),
    /// `--if-none-match`: the etag must not be this one, or `*` for a key
    /// that must not exist at all.
    IfNoneMatch(String),
}

impl BlobCondition {
    fn into_proto(self) -> blob_proto::Precondition {
        blob_proto::Precondition {
            condition: Some(match self {
                Self::IfMatch(etag) => blob_proto::precondition::Condition::IfMatch(etag),
                Self::IfNoneMatch(etag) => blob_proto::precondition::Condition::IfNoneMatch(etag),
            }),
        }
    }
}

/// How a call that carried a precondition ended.
///
/// A refusal is a value rather than an error because it is the one outcome a
/// script answers by reading the object again and retrying; every error this
/// module returns means the opposite.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Conditional<T> {
    Done(T),
    Refused {
        condition: String,
        /// The etag the object carries now; `None` when it does not exist.
        etag: Option<String>,
    },
}

/// One page of a blob listing.
#[derive(Debug, Clone, Default)]
pub struct BlobListPage {
    pub objects: Vec<BlobListEntry>,
    /// Folder names rolled up by the delimiter, each ending with it.
    pub prefixes: Vec<String>,
    /// `Some` while there is somewhere to resume from; see `ListResponse` in
    /// `blob.proto`. A short page is not the end signal - the cursor is.
    pub next_cursor: Option<String>,
}

/// One object as a listing reports it: everything `head` says, and no body.
#[derive(Debug, Clone)]
pub struct BlobListEntry {
    pub key: String,
    pub sha256: String,
    pub size: u64,
    pub content_type: Option<String>,
    pub created_at: i64,
}

impl From<blob_proto::BlobInfo> for BlobListEntry {
    fn from(info: blob_proto::BlobInfo) -> Self {
        Self {
            key: info.key,
            sha256: info.sha256,
            size: info.size,
            content_type: info.content_type,
            created_at: info.created_at,
        }
    }
}

/// A connection to deis-store, bound to one tenant.
///
/// Cloning is cheap: every field wraps a tonic `Channel`, a reference-counted
/// HTTP/2 handle, so a clone shares the connection rather than opening a new
/// one. Nothing here dials eagerly - see [`StorageSession::connect`] - so
/// building one never fails because deis-store happens to be unreachable;
/// only the first call that actually needs it can fail that way.
#[derive(Clone)]
pub struct StorageSession {
    identity: Identity,
    kv: KvStoreClient<Channel>,
    db: DatabaseClient<Channel>,
    blob: BlobStoreClient<Channel>,
}

/// Delegates to [`Identity`]'s redacted `Debug` and says nothing about the
/// three gRPC clients - they carry no state worth printing beyond what
/// `identity` already says about which tenant and credential they were built
/// with.
impl std::fmt::Debug for StorageSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StorageSession")
            .field("identity", &self.identity)
            .finish()
    }
}

impl StorageSession {
    /// Bind to `tenant` (already built by `deis_wire::conventions::tenant_id`;
    /// this function only validates it) at `endpoint`, presenting `token` on
    /// every call when one is given.
    ///
    /// Two channels are built against the same endpoint rather than one:
    /// the KV and database calls are unary and get [`REQUEST_TIMEOUT`], but a
    /// blob transfer's honest duration depends on the object's size, so it
    /// gets a channel with no per-call deadline and relies on
    /// [`Self::blob_get`]'s own size ceiling instead. Neither channel opens a
    /// socket yet - `connect_lazy` defers that to the first RPC - so a
    /// deis-store that is down does not stop this session from being built,
    /// only from doing anything.
    /// `binding` names one of the tenant's named namespaces, or `None` for
    /// its unnamed one - which is what `context.services.kv`, `.database` and
    /// `.blob` reach, and where an application that declares no binding keeps
    /// everything it has.
    pub fn connect(
        endpoint: &str,
        tenant: &str,
        token: Option<&str>,
        binding: Option<&str>,
    ) -> Result<Self> {
        let identity = Identity::new(tenant.to_string(), token, binding)?;
        let unary = lazy_channel(endpoint, Some(REQUEST_TIMEOUT))?;
        let streaming = lazy_channel(endpoint, None)?;
        Ok(Self {
            identity,
            kv: KvStoreClient::new(unary.clone())
                .max_decoding_message_size(MAX_GRPC_MESSAGE_BYTES)
                .max_encoding_message_size(MAX_GRPC_MESSAGE_BYTES),
            db: DatabaseClient::new(unary)
                .max_decoding_message_size(MAX_GRPC_MESSAGE_BYTES)
                .max_encoding_message_size(MAX_GRPC_MESSAGE_BYTES),
            blob: BlobStoreClient::new(streaming)
                .max_decoding_message_size(MAX_GRPC_MESSAGE_BYTES)
                .max_encoding_message_size(MAX_GRPC_MESSAGE_BYTES),
        })
    }

    // ---- kv -------------------------------------------------------------

    pub async fn kv_get(&self, key: &str) -> Result<Option<KvValue>> {
        let request = self.identity.stamp(storage_proto::GetRequest {
            key: key.to_string(),
        });
        let response = self
            .kv
            .clone()
            .get(request)
            .await
            .map_err(|status| call_failed("reading kv key", key, status))?
            .into_inner();
        Ok(response.value.map(|value| KvValue {
            value,
            metadata: response.metadata,
            expires_at: response.expires_at,
        }))
    }

    pub async fn kv_put(
        &self,
        key: &str,
        value: Vec<u8>,
        ttl_seconds: Option<i64>,
        metadata: Option<String>,
    ) -> Result<()> {
        let request = self.identity.stamp(storage_proto::SetRequest {
            key: key.to_string(),
            value,
            ttl_seconds,
            metadata,
        });
        let response = self
            .kv
            .clone()
            .set(request)
            .await
            .map_err(|status| call_failed("writing kv key", key, status))?
            .into_inner();
        if !response.success {
            bail!("deis-store refused to write kv key {key:?} and gave no further reason");
        }
        Ok(())
    }

    /// `true` when the key existed and was removed. Deleting an absent key is
    /// not an error - `deis-store`'s own `Delete` RPC has the same contract -
    /// so the caller decides whether that is worth mentioning.
    pub async fn kv_delete(&self, key: &str) -> Result<bool> {
        let request = self.identity.stamp(storage_proto::DeleteRequest {
            key: key.to_string(),
        });
        let response = self
            .kv
            .clone()
            .delete(request)
            .await
            .map_err(|status| call_failed("deleting kv key", key, status))?
            .into_inner();
        Ok(response.success)
    }

    pub async fn kv_list(
        &self,
        prefix: &str,
        limit: i32,
        cursor: Option<String>,
    ) -> Result<KvListPage> {
        let request = self.identity.stamp(storage_proto::ListRequest {
            prefix: prefix.to_string(),
            limit: Some(limit),
            cursor,
            // Always requested: a listing without it would need a second
            // call to answer "when does this expire", and the page is
            // already bounded by `limit` either way.
            include_metadata: Some(true),
        });
        let response = self
            .kv
            .clone()
            .list(request)
            .await
            .map_err(|status| call_failed("listing kv keys under prefix", prefix, status))?
            .into_inner();
        Ok(KvListPage {
            entries: response
                .entries
                .into_iter()
                .map(|entry| KvListEntry {
                    key: entry.key,
                    metadata: entry.metadata,
                    expires_at: entry.expires_at,
                })
                .collect(),
            next_cursor: response.next_cursor,
        })
    }

    // ---- database ---------------------------------------------------------

    pub async fn db_query(&self, sql: &str, params: Option<String>) -> Result<QueryResult> {
        let request = self.identity.stamp(storage_proto::QueryRequest {
            sql: sql.to_string(),
            params,
        });
        let response = self
            .db
            .clone()
            .query(request)
            .await
            .map_err(|status| call_failed("running the query against", "deis-store", status))?
            .into_inner();
        if !response.success {
            bail!(
                "query failed: {}",
                response
                    .error
                    .unwrap_or_else(|| "no reason given".to_string())
            );
        }
        let rows = serde_json::from_str(&response.rows).with_context(|| {
            format!(
                "deis-store answered a query with rows that are not valid JSON: {:?}",
                response.rows
            )
        })?;
        Ok(QueryResult {
            columns: response.columns,
            rows,
        })
    }

    /// `(rows_affected, last_insert_id)`.
    pub async fn db_execute(
        &self,
        sql: &str,
        params: Option<String>,
    ) -> Result<(i64, Option<i64>)> {
        let request = self.identity.stamp(storage_proto::ExecuteRequest {
            sql: sql.to_string(),
            params,
        });
        let response = self
            .db
            .clone()
            .execute(request)
            .await
            .map_err(|status| call_failed("running the statement against", "deis-store", status))?
            .into_inner();
        if !response.success {
            bail!(
                "statement failed: {}",
                response
                    .error
                    .unwrap_or_else(|| "no reason given".to_string())
            );
        }
        Ok((response.rows_affected, response.last_insert_id))
    }

    // ---- blob ---------------------------------------------------------

    /// Fetch an object whole, refusing to hold more than `max_bytes` of it in
    /// memory. `Ok(None)` means no such key - `BlobStore.Get` fails that case
    /// with `NOT_FOUND` before sending anything, per `blob.proto`, which is
    /// what is translated into the ordinary answer here.
    pub async fn blob_get(
        &self,
        key: &str,
        range: Option<BlobRange>,
        max_bytes: usize,
    ) -> Result<Option<BlobValue>> {
        let request = self.identity.stamp(blob_proto::GetRequest {
            key: key.to_string(),
            range: range.map(|range| blob_proto::ByteRange {
                spec: Some(match range {
                    BlobRange::Span { offset, length } => {
                        blob_proto::byte_range::Spec::Span(blob_proto::ByteSpan { offset, length })
                    }
                    BlobRange::Suffix(n) => blob_proto::byte_range::Spec::Suffix(n),
                }),
            }),
        });
        let mut stream = match self.blob.clone().get(request).await {
            Ok(response) => response.into_inner(),
            Err(status) if status.code() == tonic::Code::NotFound => return Ok(None),
            Err(status) => return Err(call_failed("reading blob key", key, status)),
        };

        let first = stream
            .message()
            .await
            .with_context(|| format!("reading the header of blob key {key:?}"))?
            .ok_or_else(|| {
                anyhow!("deis-store closed the stream for blob key {key:?} before sending a header")
            })?;
        let info = match first.payload {
            Some(blob_proto::get_response::Payload::Info(info)) => info,
            _ => bail!("deis-store sent a body chunk before the header for blob key {key:?}"),
        };

        // The range deis-store resolved, when one was asked for. What is
        // measured against the ceiling is what will actually be assembled: a
        // ranged read of a slice of a huge object is exactly how an operator
        // takes something bigger than this command holds.
        let served = first.range.map(|r| BlobRangeAnswer {
            offset: r.offset,
            length: r.length,
        });
        let expected = served.map_or(info.size, |r| r.length);

        if expected as usize > max_bytes {
            bail!(
                "reading blob key {key:?} would take {expected} bytes, over the {max_bytes} byte \
                 limit this command holds in memory. Ask for a byte range with --offset / \
                 --length / --suffix, or raise the limit with --max-size once you are sure that \
                 is what you want to download."
            );
        }

        let mut data = Vec::with_capacity((expected as usize).min(MAX_PREALLOC_BYTES));
        while let Some(message) = stream
            .message()
            .await
            .with_context(|| format!("reading the body of blob key {key:?}"))?
        {
            match message.payload {
                Some(blob_proto::get_response::Payload::Chunk(chunk)) => {
                    data.extend_from_slice(&chunk)
                }
                Some(_) => bail!("deis-store sent a second header for blob key {key:?}"),
                // The trailer of a ranged read, carrying the digest of the
                // slice that was streamed; a message with no payload and no
                // digest is a kind this build does not know and is skipped.
                None => {}
            }
        }

        if data.len() as u64 != expected {
            bail!(
                "blob key {key:?} arrived as {} bytes, but deis-store said it was sending \
                 {expected}. The object is not what this command received; do not trust what was \
                 written out.",
                data.len()
            );
        }

        Ok(Some(BlobValue {
            info: BlobInfo {
                sha256: info.sha256,
                size: info.size,
                content_type: info.content_type,
                created_at: info.created_at,
            },
            range: served,
            data,
        }))
    }

    /// What deis-store knows about one object, without moving its body.
    /// `Ok(None)` means no such key - `BlobStore.Head` answers that as an
    /// absent `info` rather than a status, per `blob.proto`.
    pub async fn blob_head(&self, key: &str) -> Result<Option<BlobListEntry>> {
        let request = self.identity.stamp(blob_proto::HeadRequest {
            key: key.to_string(),
        });
        let response = self
            .blob
            .clone()
            .head(request)
            .await
            .map_err(|status| call_failed("reading the metadata of blob key", key, status))?
            .into_inner();
        Ok(response.info.map(BlobListEntry::from))
    }

    /// One page of this tenant's objects. deis-store clamps `limit` to its own
    /// ceiling, so this is a request rather than a promise; page until
    /// `next_cursor` is `None`.
    pub async fn blob_list(
        &self,
        prefix: &str,
        delimiter: Option<String>,
        limit: Option<i32>,
        cursor: Option<String>,
    ) -> Result<BlobListPage> {
        let request = self.identity.stamp(blob_proto::ListRequest {
            prefix: prefix.to_string(),
            limit,
            cursor,
            delimiter,
        });
        let response = self
            .blob
            .clone()
            .list(request)
            .await
            .map_err(|status| call_failed("listing blob keys under prefix", prefix, status))?
            .into_inner();
        Ok(BlobListPage {
            objects: response
                .objects
                .into_iter()
                .map(BlobListEntry::from)
                .collect(),
            prefixes: response.prefixes,
            next_cursor: response.next_cursor,
        })
    }

    /// Store an object, framed into [`STREAM_CHUNK_BYTES`] slices as
    /// `BlobStore.Put` expects. `data` is buffered whole before this call, the
    /// same tradeoff `deis-runtime`'s own blob client makes: framing it while
    /// it is read off disk would save a copy, but at the cost of a second
    /// streaming contract in this crate for a CLI upload path that is not
    /// the hot one anywhere on this platform.
    pub async fn blob_put(
        &self,
        key: &str,
        content_type: Option<String>,
        condition: Option<BlobCondition>,
        data: Vec<u8>,
    ) -> Result<Conditional<BlobPutResult>> {
        let request =
            self.identity
                .stamp(put_messages(key.to_string(), content_type, condition, data));
        let response = self
            .blob
            .clone()
            .put(request)
            .await
            .map_err(|status| call_failed("uploading blob key", key, status))?
            .into_inner();
        if let Some(failure) = response.precondition_failed {
            return Ok(Conditional::Refused {
                condition: failure.condition,
                etag: failure.etag,
            });
        }
        if !response.success {
            bail!(
                "blob store rejected key {key:?}: {}",
                response
                    .error
                    .unwrap_or_else(|| "no reason given".to_string())
            );
        }
        Ok(Conditional::Done(BlobPutResult {
            sha256: response.sha256,
            size: response.size,
        }))
    }

    /// Deleting a key that is already absent succeeds - `BlobStore.Delete`'s
    /// own contract, per `blob.proto`.
    pub async fn blob_delete(
        &self,
        key: &str,
        condition: Option<BlobCondition>,
    ) -> Result<Conditional<()>> {
        let request = self.identity.stamp(blob_proto::DeleteRequest {
            key: key.to_string(),
            precondition: condition.map(BlobCondition::into_proto),
        });
        let response = self
            .blob
            .clone()
            .delete(request)
            .await
            .map_err(|status| call_failed("deleting blob key", key, status))?
            .into_inner();
        if let Some(failure) = response.precondition_failed {
            return Ok(Conditional::Refused {
                condition: failure.condition,
                etag: failure.etag,
            });
        }
        if !response.success {
            bail!(
                "blob store failed to delete key {key:?}: {}",
                response
                    .error
                    .unwrap_or_else(|| "no reason given".to_string())
            );
        }
        Ok(Conditional::Done(()))
    }
}

/// Frame one object body into the message sequence `BlobStore.Put` expects:
/// the header first, then the body in [`STREAM_CHUNK_BYTES`] slices. An empty
/// body yields the header alone, which stores a zero-length object.
fn put_messages(
    key: String,
    content_type: Option<String>,
    condition: Option<BlobCondition>,
    data: Vec<u8>,
) -> impl tokio_stream::Stream<Item = blob_proto::PutRequest> + Send + 'static {
    let mut messages = Vec::with_capacity(1 + data.len().div_ceil(STREAM_CHUNK_BYTES));
    messages.push(blob_proto::PutRequest {
        payload: Some(blob_proto::put_request::Payload::Header(
            blob_proto::PutHeader {
                key,
                content_type,
                precondition: condition.map(BlobCondition::into_proto),
            },
        )),
    });
    // Cut eagerly rather than lazily as the stream is polled: the whole body
    // is already resident (see `StorageSession::blob_put`'s doc comment), so
    // there is no streaming-from-disk benefit left to preserve, and an eager
    // `Vec<PutRequest>` is simpler than a generator that borrows `data`.
    for chunk in data.chunks(STREAM_CHUNK_BYTES) {
        messages.push(blob_proto::PutRequest {
            payload: Some(blob_proto::put_request::Payload::Chunk(chunk.to_vec())),
        });
    }
    tokio_stream::iter(messages)
}

/// A gRPC failure, worded for the operator running the command rather than
/// for a caller that already knows which RPC it made.
///
/// `Unauthenticated` gets its own message: it is the one failure a person
/// cannot fix by trying again with the same input, because it means the
/// capability token this session was handed did not satisfy deis-store - most
/// likely because the platform's storage secret rotated between the moment
/// `deis-control` minted the token and this call. A fresh run fetches a fresh
/// token, so that is the suggested next step rather than "check your key".
fn call_failed(action: &str, target: &str, status: tonic::Status) -> anyhow::Error {
    if status.code() == tonic::Code::Unauthenticated {
        anyhow!(
            "{action} {target:?} was refused: {}. The storage capability token this command \
             fetched from the control plane did not satisfy deis-store - most likely because the \
             platform's storage secret rotated since. Run the command again; each run fetches a \
             fresh token.",
            status.message()
        )
    } else {
        anyhow!("{action} {target:?} failed: {status}")
    }
}

/// Build a channel that dials on first use rather than at construction, so a
/// deis-store that is down does not stop a [`StorageSession`] from being
/// built - only from doing anything.
fn lazy_channel(endpoint: &str, request_timeout: Option<Duration>) -> Result<Channel> {
    let mut builder = Endpoint::from_shared(endpoint.to_string())
        .with_context(|| format!("{endpoint:?} is not a usable gRPC address for deis-store"))?
        .connect_timeout(CONNECT_TIMEOUT);
    if let Some(timeout) = request_timeout {
        builder = builder.timeout(timeout);
    }
    super::service_tls::lazy(builder, endpoint, deis_identity::ServiceKind::Store)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_bare_app_id_is_not_a_tenant() {
        let error = Identity::new("demo".to_string(), None, None)
            .unwrap_err()
            .to_string();
        assert!(error.contains("demo.production"), "{error}");
    }

    #[test]
    fn a_valid_tenant_with_no_token_builds() {
        let identity = Identity::new("demo.production".to_string(), None, None).unwrap();
        assert!(identity.token.is_none());
    }

    #[test]
    fn a_blank_token_counts_as_none() {
        // An empty header would be presented as a token and refused by a
        // deis-store that checks one; sending nothing at least works against
        // a service that requires no proof.
        let identity = Identity::new("demo.production".to_string(), Some("   "), None).unwrap();
        assert!(identity.token.is_none());
    }

    #[test]
    fn a_real_token_is_carried() {
        let identity =
            Identity::new("demo.production".to_string(), Some("abc123=="), None).unwrap();
        assert!(identity.token.is_some());
    }

    #[tokio::test]
    async fn an_unreachable_connect_lazy_still_builds() {
        // `connect_lazy` must not dial: a session over an address nothing is
        // listening on has to build successfully, and only fail once a call
        // is actually made. It also needs a Tokio runtime in scope to build
        // at all - hence `#[tokio::test]` rather than `#[test]` - even
        // though nothing here is awaited.
        StorageSession::connect("http://127.0.0.1:1", "demo.production", None, None)
            .expect("building a session must not require deis-store to be reachable");
    }

    #[test]
    fn a_malformed_endpoint_is_refused_at_connect() {
        let error = StorageSession::connect("not a url", "demo.production", None, None)
            .unwrap_err()
            .to_string();
        assert!(error.contains("not a url"), "{error}");
    }

    #[test]
    fn a_binding_is_a_namespace_and_not_a_second_credential() {
        // Its own header beside the tenant and the token, never folded into
        // either: the token authorises the tenant, and the binding chooses a
        // namespace inside the tenant it is stamped next to. A name this CLI
        // could not send would refuse the session rather than silently reach
        // the unnamed namespace under the name the operator typed.
        let identity = Identity::new("demo.production".to_string(), None, Some("HOT")).unwrap();
        let request = identity.stamp(());
        assert_eq!(
            request.metadata().get(TENANT_HEADER).unwrap(),
            "demo.production"
        );
        assert_eq!(request.metadata().get(BINDING_HEADER).unwrap(), "HOT");

        // No binding is the unnamed namespace, and the header is simply not
        // sent - which is exactly what every call this CLI made before named
        // bindings existed looked like.
        let unnamed = Identity::new("demo.production".to_string(), None, None).unwrap();
        assert!(unnamed.stamp(()).metadata().get(BINDING_HEADER).is_none());
        // A blank one means the same thing rather than an empty namespace.
        let blank = Identity::new("demo.production".to_string(), None, Some("  ")).unwrap();
        assert!(blank.stamp(()).metadata().get(BINDING_HEADER).is_none());
    }

    #[test]
    fn a_binding_name_that_could_address_another_namespace_is_refused() {
        for hostile in ["a@b", "a.b", "..", "a/b", &"x".repeat(65)] {
            let error = Identity::new("demo.production".to_string(), None, Some(hostile))
                .expect_err("must be refused")
                .to_string();
            assert!(
                error.contains("is not a binding name"),
                "{hostile}: {error}"
            );
        }
    }
}
