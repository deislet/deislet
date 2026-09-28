//! Object storage client backing `ctx.blob`.
//!
//! Bodies stream in both directions (see `deis-store/proto/blob.proto`):
//! `Put` sends one `PutHeader` followed by ordered body chunks, `Get` returns
//! one `BlobInfo` followed by ordered body chunks. Neither side may assume a
//! whole object fits in a single gRPC message, so this client always frames
//! uploads and always reassembles downloads.
//!
//! The tenant is the one application environment named by the capability
//! token this node presents in `Authorization: Bearer` (see
//! [`StorageIdentity`] and [`Tenant`]). deis-store refuses any request that
//! arrives without one, so a client is bound to exactly one environment of
//! one application for its whole life, and a preview cannot read or
//! overwrite an object production stored.

use super::bindings::CapabilityChannel;
use super::bindings::StorageIdentity;
use crate::STREAM_CHUNK_BYTES;
use anyhow::{anyhow, Result};
use futures_util::{stream, Stream, StreamExt};
use sha2::{Digest, Sha256};
use tonic::{Code, Status};

pub mod proto {
    tonic::include_proto!("deis.blob");
}

use proto::{
    blob_store_client::BlobStoreClient as GrpcBlobClient, byte_range, get_response, precondition,
    put_request, ByteRange, ByteSpan, DeleteRequest, GetRequest, GetResponse, HeadRequest,
    ListRequest, Precondition, PutHeader, PutRequest,
};

/// Upper bound on how much a download preallocates from the size the store
/// declares. The declared size is a hint for `Vec::reserve`, never a promise:
/// a wrong or corrupted value must not turn into a huge allocation.
const MAX_PREALLOC_BYTES: usize = 8 * 1024 * 1024;

/// Default ceiling on a single downloaded object.
///
/// `ctx.blob.get` hands the app one contiguous `Vec<u8>`, which is then copied
/// again into V8, so a read costs roughly twice the object on this node. An
/// isolate's `memory_limit_mb` only bounds the V8 heap and cannot see the Rust
/// side at all, so without a ceiling here one app reading one large object of
/// its own -- deis-store accepts uploads up to 512 MiB by default -- can drive
/// the whole node out of memory and take down every other app on it.
///
/// 64 MiB is well under any realistic node and still far above what a request
/// handler sensibly loads whole. Anything larger needs a streaming read, which
/// is wave B work.
pub const DEFAULT_MAX_OBJECT_BYTES: usize = 64 * 1024 * 1024;

/// What the store knows about one object, as an application sees it.
///
/// `etag` is the store's `sha256` under the name it is used by: it is the
/// object's version identity for a conditional write and the value an
/// application puts in an HTTP `ETag` header. One string with one meaning,
/// renamed exactly once, at this boundary, rather than carried twice.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlobMeta {
    pub key: String,
    pub etag: String,
    pub size: u64,
    pub content_type: Option<String>,
    pub created_at: i64,
}

impl BlobMeta {
    fn from_proto(info: proto::BlobInfo) -> Self {
        Self {
            key: info.key,
            etag: info.sha256,
            size: info.size,
            content_type: info.content_type,
            created_at: info.created_at,
        }
    }
}

/// Which bytes of an object a read asked for, in the terms RFC 9110 gives
/// HTTP's `Range` header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RangeRequest {
    /// `bytes=<offset>-` when `length` is absent, `bytes=<offset>-<last>`
    /// when it is not.
    Span { offset: u64, length: Option<u64> },
    /// `bytes=-<n>`: the last `n` bytes.
    Suffix(u64),
}

impl RangeRequest {
    fn into_proto(self) -> ByteRange {
        ByteRange {
            spec: Some(match self {
                Self::Span { offset, length } => {
                    byte_range::Spec::Span(ByteSpan { offset, length })
                }
                Self::Suffix(n) => byte_range::Spec::Suffix(n),
            }),
        }
    }
}

/// Which bytes a ranged read actually carried, resolved by the store against
/// the object's real size. With [`BlobMeta::size`] these are the three numbers
/// a `Content-Range` header needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RangeAnswer {
    pub offset: u64,
    pub length: u64,
}

/// One object read, whole or in part.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlobBody {
    pub meta: BlobMeta,
    /// `None` for a whole-object read; otherwise which slice `data` holds.
    pub range: Option<RangeAnswer>,
    pub data: Vec<u8>,
}

/// What a caller requires of the object before its write may land.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BlobPrecondition {
    /// `If-Match`: the etag must be this one, or `*` for any existing object.
    IfMatch(String),
    /// `If-None-Match`: the etag must not be this one, or `*` for a key that
    /// must not exist at all.
    IfNoneMatch(String),
}

impl BlobPrecondition {
    fn into_proto(self) -> Precondition {
        Precondition {
            condition: Some(match self {
                Self::IfMatch(etag) => precondition::Condition::IfMatch(etag),
                Self::IfNoneMatch(etag) => precondition::Condition::IfNoneMatch(etag),
            }),
        }
    }
}

/// How an operation that carried a precondition ended.
///
/// A refusal is a value rather than an error for the reason the store makes it
/// one: it is the single outcome an application answers by reading the object
/// again and retrying, and every error here means the opposite. Collapsing the
/// two would make an application retry a permission failure forever.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Conditional<T> {
    Done(T),
    Refused {
        /// The condition as it was sent, for the application's own log line.
        condition: String,
        /// The etag the object carries now; `None` when it does not exist.
        etag: Option<String>,
    },
}

/// What one page of a listing asks for.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BlobListRequest {
    pub prefix: String,
    /// Roll keys up at this separator, S3 style. `None` lists flat.
    pub delimiter: Option<String>,
    /// Entries this page may hold. The store clamps it to its own ceiling, so
    /// this is a request rather than a promise.
    pub limit: Option<i32>,
    /// The `cursor` a previous page answered with; opaque.
    pub cursor: Option<String>,
}

/// One page of a listing.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BlobListPage {
    pub objects: Vec<BlobMeta>,
    pub prefixes: Vec<String>,
    /// `None` means the scan reached the end. A short page does not.
    pub cursor: Option<String>,
}

/// Client for one application's object storage.
///
/// Cloning is cheap: the inner tonic client wraps an HTTP/2 `Channel`, which
/// is a reference-counted handle. Each call clones it instead of locking one
/// shared client, so a slow multi-megabyte upload cannot block every other
/// blob call behind it.
#[derive(Clone)]
pub struct BlobClient {
    client: GrpcBlobClient<CapabilityChannel>,
    identity: StorageIdentity,
    max_object_bytes: usize,
}

impl BlobClient {
    /// The same client, speaking for one named binding of this tenant.
    ///
    /// Shares the channel: the tonic client is a reference-counted handle over
    /// one HTTP/2 connection, so a version declaring four namespaces holds
    /// four identities and one connection rather than four. `None` gives the
    /// tenant's unnamed namespace, which is what an application that declares
    /// no binding of this kind has always used.
    pub(crate) fn for_binding(&self, binding: Option<&str>) -> Result<Self> {
        let identity = self.identity.for_binding(binding)?;
        tracing::debug!(
            app_id = identity.tenant(),
            binding = identity.binding().unwrap_or("<unnamed>"),
            "object storage calls of this client are charged to {}",
            identity.describe()
        );
        Ok(Self {
            identity,
            ..self.clone()
        })
    }

    /// Build a client that dials deis-store on its first call; see
    /// [`super::kv_client::KvClient::connect_lazy`] for why bindings use this
    /// rather than connecting eagerly.
    ///
    /// Unlike the KV and database clients this one gets no per-call deadline:
    /// a blob transfer is a stream whose honest duration depends on the size
    /// of the object, so a fixed deadline would cut off large-but-healthy
    /// downloads. [`DEFAULT_MAX_OBJECT_BYTES`] bounds the cost instead.
    pub(crate) fn connect_lazy(endpoint: &str, identity: &StorageIdentity) -> Result<Self> {
        let channel = super::bindings::streaming_channel(
            endpoint,
            deis_identity::ServiceKind::Store,
            identity,
        )?;
        let client = GrpcBlobClient::new(channel)
            .max_decoding_message_size(crate::MAX_GRPC_MESSAGE_BYTES)
            .max_encoding_message_size(crate::MAX_GRPC_MESSAGE_BYTES);
        tracing::debug!(
            endpoint,
            app_id = identity.tenant(),
            "blob client prepared; it will dial on the first call"
        );
        Ok(Self {
            client,
            identity: identity.clone(),
            max_object_bytes: DEFAULT_MAX_OBJECT_BYTES,
        })
    }

    /// Change the largest object [`BlobClient::get`] will assemble in memory.
    ///
    /// Raising it raises what one read can cost this node; see
    /// [`DEFAULT_MAX_OBJECT_BYTES`].
    pub fn with_max_object_bytes(mut self, max_object_bytes: usize) -> Self {
        self.max_object_bytes = max_object_bytes;
        self
    }

    /// The tenant -- application and environment -- every call of this client
    /// is charged to.
    pub fn tenant(&self) -> &str {
        self.identity.tenant()
    }

    /// Fetch an object, whole or one slice of it. `Ok(None)` means no such
    /// key.
    ///
    /// A range is resolved by the store, not here: the answer carries the
    /// offset and length that were actually served, so an application builds
    /// its `Content-Range` from what happened rather than from what it asked
    /// for. A range that names no byte of the object comes back as an error
    /// carrying `Code::OutOfRange`, which is what an application turns into a
    /// 416 - and is why it must not be flattened into "not found".
    pub async fn read(&self, key: String, range: Option<RangeRequest>) -> Result<Option<BlobBody>> {
        tracing::debug!(app_id = self.tenant(), %key, ranged = range.is_some(), "blob read");
        let mut client = self.client.clone();

        let request = GetRequest {
            key: key.clone(),
            range: range.map(RangeRequest::into_proto),
        };
        let inbound = match client.get(self.identity.stamp(request)).await {
            Ok(response) => response.into_inner(),
            // A missing key is an ordinary answer for `ctx.blob.get`, not a
            // failure: the caller gets `null`, nothing is logged above debug.
            Err(status) if status.code() == Code::NotFound => {
                tracing::debug!(app_id = self.tenant(), %key, "blob get found no such object");
                return Ok(None);
            }
            Err(status) => {
                tracing::warn!(
                    app_id = self.tenant(),
                    %key,
                    code = ?status.code(),
                    message = status.message(),
                    "blob get was refused; the application sees an exception and reads nothing"
                );
                return Err(status.into());
            }
        };

        let body = collect_body(self.tenant(), &key, inbound, self.max_object_bytes).await?;
        tracing::debug!(
            app_id = self.tenant(),
            %key,
            size = body.data.len(),
            offset = body.range.map(|r| r.offset).unwrap_or(0),
            "blob read complete"
        );
        Ok(Some(body))
    }

    /// What the store knows about one object, without moving its body.
    /// `Ok(None)` means no such key.
    pub async fn head(&self, key: String) -> Result<Option<BlobMeta>> {
        tracing::debug!(app_id = self.tenant(), %key, "blob head");
        let mut client = self.client.clone();
        let response = client
            .head(self.identity.stamp(HeadRequest { key: key.clone() }))
            .await
            .map_err(|status| {
                tracing::warn!(
                    app_id = self.tenant(),
                    %key,
                    code = ?status.code(),
                    message = status.message(),
                    "blob head was refused; the application sees an exception"
                );
                anyhow::Error::from(status)
            })?
            .into_inner();
        Ok(response.info.map(BlobMeta::from_proto))
    }

    /// One page of this application's objects.
    ///
    /// The store decides the page size: it substitutes its default for an
    /// absent limit and caps anything above its ceiling, so an application
    /// asking for a million keys receives the ceiling and a cursor rather than
    /// an error. See `ListRequest` in blob.proto.
    pub async fn list(&self, request: BlobListRequest) -> Result<BlobListPage> {
        tracing::debug!(
            app_id = self.tenant(),
            prefix = %request.prefix,
            delimiter = request.delimiter.as_deref().unwrap_or(""),
            "blob list"
        );
        let mut client = self.client.clone();
        let wire = ListRequest {
            prefix: request.prefix.clone(),
            limit: request.limit,
            cursor: request.cursor,
            delimiter: request.delimiter,
        };
        let response = client
            .list(self.identity.stamp(wire))
            .await
            .map_err(|status| {
                tracing::warn!(
                    app_id = self.tenant(),
                    prefix = %request.prefix,
                    code = ?status.code(),
                    message = status.message(),
                    "blob list was refused; the application sees an exception and lists nothing"
                );
                anyhow::Error::from(status)
            })?
            .into_inner();
        Ok(BlobListPage {
            objects: response
                .objects
                .into_iter()
                .map(BlobMeta::from_proto)
                .collect(),
            prefixes: response.prefixes,
            cursor: response.next_cursor,
        })
    }

    /// Store an object, framed into [`STREAM_CHUNK_BYTES`] slices.
    ///
    /// With a precondition, [`Conditional::Refused`] is the answer to "somebody
    /// else got there first" and is not an error: it is the one outcome the
    /// caller answers by re-reading the object, and every error this returns is
    /// one it must not retry as it stands.
    pub async fn write(
        &self,
        key: String,
        data: Vec<u8>,
        content_type: Option<String>,
        precondition: Option<BlobPrecondition>,
    ) -> Result<Conditional<BlobMeta>> {
        let size = data.len();
        tracing::debug!(
            app_id = self.tenant(),
            %key,
            size,
            chunks = size.div_ceil(STREAM_CHUNK_BYTES),
            conditional = precondition.is_some(),
            "blob put streaming"
        );

        let mut client = self.client.clone();
        let messages = put_messages(&key, content_type.clone(), precondition, data);
        let response = client
            .put(self.identity.stamp(messages))
            .await
            .map_err(|status| {
                tracing::warn!(
                    app_id = self.tenant(),
                    %key,
                    size,
                    code = ?status.code(),
                    message = status.message(),
                    "blob put was refused mid-stream; nothing was stored and the upload is lost"
                );
                anyhow::Error::from(status)
            })?
            .into_inner();

        if let Some(failure) = response.precondition_failed {
            tracing::debug!(
                app_id = self.tenant(),
                %key,
                condition = %failure.condition,
                found = failure.etag.as_deref().unwrap_or("<absent>"),
                "blob put lost its conditional write; nothing was stored"
            );
            return Ok(Conditional::Refused {
                condition: failure.condition,
                etag: failure.etag,
            });
        }

        if !response.success {
            let reason = response
                .error
                .unwrap_or_else(|| "no reason given".to_string());
            tracing::warn!(
                app_id = self.tenant(),
                %key,
                size,
                %reason,
                "blob store rejected the object; nothing was stored"
            );
            anyhow::bail!("blob put failed for key {key}: {reason}");
        }

        tracing::debug!(
            app_id = self.tenant(),
            %key,
            sha256 = %response.sha256,
            stored = response.size,
            "blob put stored"
        );
        Ok(Conditional::Done(BlobMeta {
            key,
            etag: response.sha256,
            size: response.size,
            content_type,
            created_at: 0,
        }))
    }

    /// Remove an object. Deleting a key that is already absent succeeds unless
    /// a precondition says otherwise.
    pub async fn delete(
        &self,
        key: String,
        precondition: Option<BlobPrecondition>,
    ) -> Result<Conditional<()>> {
        tracing::debug!(
            app_id = self.tenant(),
            %key,
            conditional = precondition.is_some(),
            "blob delete"
        );
        let mut client = self.client.clone();
        let request = DeleteRequest {
            key: key.clone(),
            precondition: precondition.map(BlobPrecondition::into_proto),
        };
        let response = client
            .delete(self.identity.stamp(request))
            .await?
            .into_inner();

        if let Some(failure) = response.precondition_failed {
            tracing::debug!(
                app_id = self.tenant(),
                %key,
                condition = %failure.condition,
                found = failure.etag.as_deref().unwrap_or("<absent>"),
                "blob delete lost its conditional write; the object is still stored"
            );
            return Ok(Conditional::Refused {
                condition: failure.condition,
                etag: failure.etag,
            });
        }

        if !response.success {
            let reason = response
                .error
                .unwrap_or_else(|| "no reason given".to_string());
            tracing::warn!(
                app_id = self.tenant(),
                %key,
                %reason,
                "blob delete failed; the object is still stored"
            );
            anyhow::bail!("blob delete failed for key {key}: {reason}");
        }
        Ok(Conditional::Done(()))
    }
}

/// Frame one object body into the message sequence `Put` expects: the header
/// first, then the body in [`STREAM_CHUNK_BYTES`] slices.
///
/// The slices are cut as the stream is polled rather than all at once, so
/// sending a large object costs the body plus one chunk instead of two full
/// copies of it.
///
/// An empty body yields the header alone, which stores a zero-length object.
fn put_messages(
    key: &str,
    content_type: Option<String>,
    precondition: Option<BlobPrecondition>,
    data: Vec<u8>,
) -> impl Stream<Item = PutRequest> + Send + 'static {
    let header = PutRequest {
        payload: Some(put_request::Payload::Header(PutHeader {
            key: key.to_string(),
            content_type,
            precondition: precondition.map(BlobPrecondition::into_proto),
        })),
    };

    stream::unfold(
        (Some(header), data, 0usize),
        |(header, data, sent)| async move {
            if let Some(header) = header {
                return Some((header, (None, data, sent)));
            }
            if sent >= data.len() {
                return None;
            }
            let end = (sent + STREAM_CHUNK_BYTES).min(data.len());
            let chunk = PutRequest {
                payload: Some(put_request::Payload::Chunk(data[sent..end].to_vec())),
            };
            Some((chunk, (None, data, end)))
        },
    )
}

/// Reassemble a `Get` stream: one `info` message, then body chunks in order,
/// and - for a ranged read - a final message carrying the digest of exactly
/// the bytes that were sent.
///
/// A stream that breaks that shape is an error rather than a short read: the
/// caller asked for an object, and handing it a silently truncated body would
/// look like success.
///
/// Three things can end the read in failure, and all three are deliberate:
/// what was asked for is larger than `max_object_bytes`, the assembled length
/// disagrees with what the store said it was sending, or the assembled digest
/// disagrees with it. In every case the application sees an exception rather
/// than bytes.
///
/// The ceiling is measured against the bytes that will actually be assembled,
/// not against the object. That is the whole point of a range: reading the
/// first megabyte of a gigabyte object has to be allowed, or the feature would
/// be refused in exactly the case it exists for.
async fn collect_body<S>(
    tenant: &str,
    key: &str,
    inbound: S,
    max_object_bytes: usize,
) -> Result<BlobBody>
where
    S: Stream<Item = Result<GetResponse, Status>>,
{
    tokio::pin!(inbound);

    let mut info: Option<proto::BlobInfo> = None;
    let mut range: Option<RangeAnswer> = None;
    let mut range_digest: Option<String> = None;
    let mut expected: u64 = 0;
    let mut body: Vec<u8> = Vec::new();
    let mut digest = Sha256::new();

    while let Some(message) = inbound.next().await {
        let message = message?;
        // A message with no payload is either the trailer of a ranged read or
        // a payload this build does not know about; the digest field is what
        // tells them apart.
        if let Some(sent) = message.range_sha256 {
            if range_digest.replace(sent).is_some() {
                anyhow::bail!("blob get stream for {key} sent a second range digest");
            }
        }
        match message.payload {
            Some(get_response::Payload::Info(header)) => {
                if info.is_some() {
                    anyhow::bail!("blob get stream for {key} sent a second header");
                }
                range = message.range.map(|r| RangeAnswer {
                    offset: r.offset,
                    length: r.length,
                });
                expected = range.map_or(header.size, |r| r.length);

                // Refuse before reading a single chunk when the store already
                // says this is too big; there is no point paying for the
                // transfer of something that will be rejected.
                if expected > max_object_bytes as u64 {
                    tracing::error!(
                        app_id = %tenant,
                        %key,
                        size = header.size,
                        wanted = expected,
                        limit = max_object_bytes,
                        "blob read exceeds this node's read ceiling; the read fails and the \
                         application sees an exception. Assembling it would cost roughly twice \
                         its size on this node and could exhaust memory for every app here. A \
                         ranged read of a smaller slice is the way to take an object this large"
                    );
                    anyhow::bail!(
                        "blob get for {key} refused: the read is {expected} bytes, this node \
                         reads at most {max_object_bytes}",
                    );
                }

                let hint = usize::try_from(expected)
                    .unwrap_or(usize::MAX)
                    .min(MAX_PREALLOC_BYTES);
                body.reserve(hint);
                info = Some(header);
            }
            Some(get_response::Payload::Chunk(chunk)) => {
                if info.is_none() {
                    anyhow::bail!("blob get stream for {key} sent a body chunk before its header");
                }

                // The header's size is a claim, not a guarantee. Check the
                // running total too, so a stream that keeps sending past what
                // it declared cannot grow the buffer without bound.
                if body.len().saturating_add(chunk.len()) > max_object_bytes {
                    tracing::error!(
                        app_id = %tenant,
                        %key,
                        received = body.len(),
                        limit = max_object_bytes,
                        "blob get stream ran past this node's read ceiling; aborting the read, \
                         the application sees an exception"
                    );
                    anyhow::bail!(
                        "blob get for {key} aborted: stream exceeded the {max_object_bytes} byte \
                         read ceiling"
                    );
                }

                digest.update(&chunk);
                body.extend_from_slice(&chunk);
            }
            // Forward compatibility: a payload this build does not know about
            // decodes to `None`, and so does the trailer handled above.
            // Skipping it keeps the body contiguous.
            None => {
                if range_digest.is_none() {
                    tracing::warn!(
                        app_id = %tenant,
                        %key,
                        "blob get stream carried a message with no payload; skipping it"
                    );
                }
            }
        }
    }

    let info =
        info.ok_or_else(|| anyhow!("blob get stream for {key} ended without sending its header"))?;

    if body.len() as u64 != expected {
        tracing::error!(
            app_id = %tenant,
            %key,
            expected,
            received = body.len(),
            sha256 = %info.sha256,
            "blob get body length disagrees with what the store said it was sending; refusing to \
             hand the application a corrupt object, the read fails instead"
        );
        anyhow::bail!(
            "blob get for {key} returned {} bytes, the store said {expected}",
            body.len(),
        );
    }

    // A whole read is checked against the object's digest; a slice against the
    // digest the store computed over exactly the bytes it streamed. Neither
    // read reaches an application unchecked.
    match &range {
        None => verify_digest(tenant, key, &info.sha256, digest)?,
        Some(_) => match &range_digest {
            Some(expected) => verify_digest(tenant, key, expected, digest)?,
            // Tolerated for the reason an absent whole-object digest is: a
            // store that stops sending it is a protocol change, not a corrupt
            // object. Said out loud so the gap is visible in the log.
            None => tracing::warn!(
                app_id = %tenant,
                %key,
                "blob ranged read carried no digest for the bytes it sent; the slice was handed \
                 to the application without an integrity check"
            ),
        },
    }

    Ok(BlobBody {
        meta: BlobMeta::from_proto(info),
        range,
        data: body,
    })
}

/// Compare what we actually received against the digest the store sent with
/// it.
///
/// Length alone only catches truncation. A flipped bit on disk, a
/// content-addressed file replaced underneath the store, or a server that
/// opened the wrong shard all produce a body of exactly the right length and
/// entirely the wrong contents -- and this module's whole promise is that a
/// corrupt object never reaches the application. The store hashes by content
/// anyway and sends the digest in the same stream, so this costs one pass that
/// was already happening chunk by chunk.
fn verify_digest(tenant: &str, key: &str, expected: &str, digest: Sha256) -> Result<()> {
    let expected = expected.trim().to_ascii_lowercase();
    if expected.is_empty() {
        // Tolerated rather than fatal: a store that stops sending the digest
        // is a protocol change, not a corrupt object. Say plainly that this
        // body went out unchecked so the gap is visible in the log.
        tracing::warn!(
            app_id = %tenant,
            %key,
            "blob get response carried no sha256; the body was handed to the application without \
             an integrity check"
        );
        return Ok(());
    }

    let actual = format!("{:x}", digest.finalize());
    if actual != expected {
        tracing::error!(
            app_id = %tenant,
            %key,
            %expected,
            %actual,
            "blob get body does not hash to its metadata digest; refusing to hand the application \
             a corrupt object, the read fails instead"
        );
        anyhow::bail!(
            "blob get for {key} returned a body whose sha256 is {actual}, metadata says {expected}"
        );
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use proto::BlobInfo;

    fn header_of(message: &PutRequest) -> &PutHeader {
        match message.payload.as_ref() {
            Some(put_request::Payload::Header(header)) => header,
            other => panic!("expected a header, got {other:?}"),
        }
    }

    fn chunk_of(message: &PutRequest) -> &[u8] {
        match message.payload.as_ref() {
            Some(put_request::Payload::Chunk(chunk)) => chunk,
            other => panic!("expected a chunk, got {other:?}"),
        }
    }

    /// A header claiming `size` bytes and `sha256`, whatever they are.
    /// Used where the check under test fires before the digest comparison.
    fn info_message(size: u64, sha256: &str) -> GetResponse {
        GetResponse {
            payload: Some(get_response::Payload::Info(BlobInfo {
                key: "k".to_string(),
                sha256: sha256.to_string(),
                size,
                content_type: None,
                created_at: 0,
            })),
            range: None,
            range_sha256: None,
        }
    }

    /// A header that honestly describes `body`, the way deis-store sends it.
    fn info_for(body: &[u8]) -> GetResponse {
        let mut digest = Sha256::new();
        digest.update(body);
        info_message(body.len() as u64, &format!("{:x}", digest.finalize()))
    }

    /// The ceiling used by tests that are not about the ceiling.
    const TEST_MAX_OBJECT_BYTES: usize = 1024;

    fn chunk_message(bytes: &[u8]) -> GetResponse {
        GetResponse {
            payload: Some(get_response::Payload::Chunk(bytes.to_vec())),
            range: None,
            range_sha256: None,
        }
    }

    async fn framed(key: &str, content_type: Option<String>, data: Vec<u8>) -> Vec<PutRequest> {
        put_messages(key, content_type, None, data).collect().await
    }

    #[tokio::test]
    async fn empty_body_sends_the_header_alone() {
        let messages = framed("k", None, Vec::new()).await;
        assert_eq!(messages.len(), 1);
        assert_eq!(header_of(&messages[0]).key, "k");
    }

    #[tokio::test]
    async fn small_body_sends_one_chunk() {
        let messages = framed("k", Some("text/plain".to_string()), b"hello".to_vec()).await;
        assert_eq!(messages.len(), 2);
        assert_eq!(
            header_of(&messages[0]).content_type.as_deref(),
            Some("text/plain")
        );
        assert_eq!(chunk_of(&messages[1]), b"hello");
    }

    #[tokio::test]
    async fn large_body_is_split_at_the_chunk_size_and_reassembles() {
        let data: Vec<u8> = (0..STREAM_CHUNK_BYTES * 2 + 7)
            .map(|i| (i % 251) as u8)
            .collect();
        let messages = framed("k", None, data.clone()).await;

        assert_eq!(messages.len(), 4, "one header plus three chunks");
        assert_eq!(chunk_of(&messages[1]).len(), STREAM_CHUNK_BYTES);
        assert_eq!(chunk_of(&messages[2]).len(), STREAM_CHUNK_BYTES);
        assert_eq!(chunk_of(&messages[3]).len(), 7);

        let rejoined: Vec<u8> = messages[1..]
            .iter()
            .flat_map(|m| chunk_of(m).to_vec())
            .collect();
        assert_eq!(rejoined, data, "chunk boundaries must not lose bytes");
    }

    #[tokio::test]
    async fn download_reassembles_chunks_in_order() {
        let messages = vec![
            Ok(info_for(b"abcdef")),
            Ok(chunk_message(b"abc")),
            Ok(chunk_message(b"def")),
        ];
        let body = collect_body(
            "app.production",
            "k",
            stream::iter(messages),
            TEST_MAX_OBJECT_BYTES,
        )
        .await
        .expect("stream is well formed");
        assert_eq!(body.data, b"abcdef");
    }

    #[tokio::test]
    async fn download_accepts_a_zero_length_object() {
        let body = collect_body(
            "app.production",
            "k",
            stream::iter(vec![Ok(info_for(b""))]),
            TEST_MAX_OBJECT_BYTES,
        )
        .await
        .expect("header alone is a complete empty object");
        assert!(body.data.is_empty());
    }

    #[tokio::test]
    async fn download_rejects_a_body_that_never_declared_a_header() {
        let messages = vec![Ok(chunk_message(b"abc"))];
        let err = collect_body(
            "app.production",
            "k",
            stream::iter(messages),
            TEST_MAX_OBJECT_BYTES,
        )
        .await
        .expect_err("a chunk before the header is a protocol violation");
        assert!(err.to_string().contains("before its header"), "{err}");
    }

    #[tokio::test]
    async fn download_rejects_an_empty_stream() {
        let messages: Vec<Result<GetResponse, Status>> = Vec::new();
        let err = collect_body(
            "app.production",
            "k",
            stream::iter(messages),
            TEST_MAX_OBJECT_BYTES,
        )
        .await
        .expect_err("a stream with no header answered nothing");
        assert!(
            err.to_string().contains("without sending its header"),
            "{err}"
        );
    }

    #[tokio::test]
    async fn download_rejects_a_truncated_body() {
        let messages = vec![Ok(info_message(10, "aa")), Ok(chunk_message(b"abc"))];
        let err = collect_body(
            "app.production",
            "k",
            stream::iter(messages),
            TEST_MAX_OBJECT_BYTES,
        )
        .await
        .expect_err("a short body must not look like a successful read");
        assert!(err.to_string().contains("the store said 10"), "{err}");
    }

    #[tokio::test]
    async fn download_propagates_a_mid_stream_status() {
        let messages = vec![
            Ok(info_message(6, "aa")),
            Err(Status::data_loss("content file is missing")),
        ];
        let err = collect_body(
            "app.production",
            "k",
            stream::iter(messages),
            TEST_MAX_OBJECT_BYTES,
        )
        .await
        .expect_err("a status inside the stream must not be swallowed");
        assert!(err.to_string().contains("content file is missing"), "{err}");
    }

    #[tokio::test]
    async fn download_skips_a_payload_it_cannot_decode() {
        let messages = vec![
            Ok(info_for(b"abc")),
            Ok(GetResponse {
                payload: None,
                range: None,
                range_sha256: None,
            }),
            Ok(chunk_message(b"abc")),
        ];
        let body = collect_body(
            "app.production",
            "k",
            stream::iter(messages),
            TEST_MAX_OBJECT_BYTES,
        )
        .await
        .expect("an unknown payload is skipped, not fatal");
        assert_eq!(body.data, b"abc");
    }

    #[tokio::test]
    async fn download_rejects_a_body_that_hashes_to_something_else() {
        // Right length, wrong contents: only the digest can catch this, and
        // handing it to the application is exactly what this client promises
        // never to do.
        let messages = vec![Ok(info_for(b"abcdef")), Ok(chunk_message(b"abcdeX"))];
        let err = collect_body(
            "app.production",
            "k",
            stream::iter(messages),
            TEST_MAX_OBJECT_BYTES,
        )
        .await
        .expect_err("a body that does not match its digest is corrupt");
        assert!(err.to_string().contains("sha256"), "{err}");
    }

    #[tokio::test]
    async fn download_compares_the_digest_case_insensitively() {
        let mut digest = Sha256::new();
        digest.update(b"abc");
        let upper = format!("{:x}", digest.finalize()).to_ascii_uppercase();

        let messages = vec![Ok(info_message(3, &upper)), Ok(chunk_message(b"abc"))];
        let body = collect_body(
            "app.production",
            "k",
            stream::iter(messages),
            TEST_MAX_OBJECT_BYTES,
        )
        .await
        .expect("hex case is not part of the digest");
        assert_eq!(body.data, b"abc");
    }

    #[tokio::test]
    async fn download_accepts_a_response_without_a_digest() {
        // A store that stops sending the digest is a protocol change, not a
        // corrupt object; the body goes through with a warning rather than
        // failing every read on the node.
        let messages = vec![Ok(info_message(3, "")), Ok(chunk_message(b"abc"))];
        let body = collect_body(
            "app.production",
            "k",
            stream::iter(messages),
            TEST_MAX_OBJECT_BYTES,
        )
        .await
        .expect("an absent digest is tolerated");
        assert_eq!(body.data, b"abc");
    }

    #[tokio::test]
    async fn download_refuses_an_object_larger_than_the_ceiling_before_reading_it() {
        // The refusal must come from the header alone: nothing is allocated
        // and no chunk is transferred for an object that cannot be accepted.
        let messages = vec![Ok(info_message(TEST_MAX_OBJECT_BYTES as u64 + 1, "aa"))];
        let err = collect_body(
            "app.production",
            "k",
            stream::iter(messages),
            TEST_MAX_OBJECT_BYTES,
        )
        .await
        .expect_err("an oversized object must not be assembled");
        assert!(err.to_string().contains("this node reads at most"), "{err}");
    }

    #[tokio::test]
    async fn download_aborts_a_stream_that_grows_past_the_ceiling() {
        // The declared size is a claim. A server (or a corrupted stream) that
        // keeps sending past it must not be able to grow the buffer without
        // bound, which is what would exhaust the node's memory.
        let limit = 8;
        let messages = vec![
            Ok(info_message(4, "aa")),
            Ok(chunk_message(b"abcd")),
            Ok(chunk_message(b"efgh")),
            Ok(chunk_message(b"ijkl")),
        ];
        let err = collect_body("app.production", "k", stream::iter(messages), limit)
            .await
            .expect_err("a stream past the ceiling must be aborted");
        assert!(err.to_string().contains("read ceiling"), "{err}");
    }

    #[tokio::test]
    async fn download_accepts_an_object_exactly_at_the_ceiling() {
        let limit = 4;
        let messages = vec![Ok(info_for(b"abcd")), Ok(chunk_message(b"abcd"))];
        let body = collect_body("app.production", "k", stream::iter(messages), limit)
            .await
            .expect("the ceiling is inclusive");
        assert_eq!(body.data, b"abcd");
    }
}

#[cfg(test)]
#[path = "blob_client_test.rs"]
mod blob_client_test;
