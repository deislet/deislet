//! Object storage client backing `ctx.blob`.
//!
//! Bodies stream in both directions (see `deis-store/proto/blob.proto`):
//! `Put` sends one `PutHeader` followed by ordered body chunks, `Get` returns
//! one `BlobInfo` followed by ordered body chunks. Neither side may assume a
//! whole object fits in a single gRPC message, so this client always frames
//! uploads and always reassembles downloads.
//!
//! The tenant travels out of band in the `x-deis-app-id` metadata header, the
//! same header deis-proxy stamps on the HTTP data path. deis-store refuses any
//! request that arrives without it, so a client is bound to exactly one
//! application for its whole life.

use super::tenant::Tenant;
use anyhow::{anyhow, Context, Result};
use futures_util::{stream, Stream, StreamExt};
use tonic::transport::Channel;
use tonic::{Code, Status};

pub mod proto {
    tonic::include_proto!("deis.blob");
}

use proto::{
    blob_store_client::BlobStoreClient as GrpcBlobClient, get_response, put_request, DeleteRequest,
    GetRequest, GetResponse, PutHeader, PutRequest,
};

/// Size of one uploaded body slice, matching `deis_store::blob::CHUNK_BYTES`.
///
/// Chunk boundaries carry no meaning on the wire, so this only has to stay
/// comfortably below [`crate::MAX_GRPC_MESSAGE_SIZE`]; 1 MiB is also what the
/// store sends back on `Get`.
const CHUNK_BYTES: usize = 1024 * 1024;

/// Upper bound on how much a download preallocates from the size the store
/// declares. The declared size is a hint for `Vec::reserve`, never a promise:
/// a wrong or corrupted value must not turn into a huge allocation.
const MAX_PREALLOC_BYTES: usize = 8 * 1024 * 1024;

/// Client for one application's object storage.
///
/// Cloning is cheap: the inner tonic client wraps an HTTP/2 `Channel`, which
/// is a reference-counted handle. Each call clones it instead of locking one
/// shared client, so a slow multi-megabyte upload cannot block every other
/// blob call behind it.
#[derive(Clone)]
pub struct BlobClient {
    client: GrpcBlobClient<Channel>,
    tenant: Tenant,
}

impl BlobClient {
    /// Connect to deis-store's blob service on behalf of one application.
    ///
    /// # Parameters
    /// - endpoint: gRPC address, e.g. `http://127.0.0.1:50053`
    /// - app_id: the tenant every call of this client is charged to
    pub async fn connect(endpoint: impl Into<String>, app_id: impl Into<String>) -> Result<Self> {
        let endpoint = endpoint.into();
        let tenant = Tenant::new(app_id)?;

        tracing::info!(%endpoint, app_id = tenant.app_id(), "connecting to blob store");
        let client = GrpcBlobClient::connect(endpoint.clone())
            .await
            .with_context(|| format!("connecting to blob store at {endpoint}"))?
            .max_decoding_message_size(crate::MAX_GRPC_MESSAGE_SIZE)
            .max_encoding_message_size(crate::MAX_GRPC_MESSAGE_SIZE);
        tracing::info!(%endpoint, app_id = tenant.app_id(), "blob store client connected");

        Ok(Self { client, tenant })
    }

    /// The application every call of this client is charged to.
    pub fn app_id(&self) -> &str {
        self.tenant.app_id()
    }

    /// Fetch an object whole. `Ok(None)` means no such key.
    pub async fn get(&self, key: String) -> Result<Option<Vec<u8>>> {
        tracing::debug!(app_id = self.app_id(), %key, "blob get");
        let mut client = self.client.clone();

        let inbound = match client
            .get(self.tenant.stamp(GetRequest { key: key.clone() }))
            .await
        {
            Ok(response) => response.into_inner(),
            // A missing key is an ordinary answer for `ctx.blob.get`, not a
            // failure: the caller gets `null`, nothing is logged above debug.
            Err(status) if status.code() == Code::NotFound => {
                tracing::debug!(app_id = self.app_id(), %key, "blob get found no such object");
                return Ok(None);
            }
            Err(status) => {
                tracing::warn!(
                    app_id = self.app_id(),
                    %key,
                    code = ?status.code(),
                    message = status.message(),
                    "blob get was refused; the application sees an exception and reads nothing"
                );
                return Err(status.into());
            }
        };

        let body = collect_body(self.app_id(), &key, inbound).await?;
        tracing::debug!(
            app_id = self.app_id(),
            %key,
            size = body.len(),
            "blob get complete"
        );
        Ok(Some(body))
    }

    /// Store an object, framed into [`CHUNK_BYTES`] slices.
    pub async fn put(&self, key: String, data: Vec<u8>) -> Result<()> {
        self.put_typed(key, data, None).await
    }

    /// Store an object, optionally declaring its media type.
    pub async fn put_typed(
        &self,
        key: String,
        data: Vec<u8>,
        content_type: Option<String>,
    ) -> Result<()> {
        let size = data.len();
        tracing::debug!(
            app_id = self.app_id(),
            %key,
            size,
            chunks = size.div_ceil(CHUNK_BYTES),
            "blob put streaming"
        );

        let mut client = self.client.clone();
        let response = client
            .put(self.tenant.stamp(put_messages(&key, content_type, data)))
            .await
            .map_err(|status| {
                tracing::warn!(
                    app_id = self.app_id(),
                    %key,
                    size,
                    code = ?status.code(),
                    message = status.message(),
                    "blob put was refused mid-stream; nothing was stored and the upload is lost"
                );
                anyhow::Error::from(status)
            })?
            .into_inner();

        if !response.success {
            let reason = response
                .error
                .unwrap_or_else(|| "no reason given".to_string());
            tracing::warn!(
                app_id = self.app_id(),
                %key,
                size,
                %reason,
                "blob store rejected the object; nothing was stored"
            );
            anyhow::bail!("blob put failed for key {key}: {reason}");
        }

        tracing::debug!(
            app_id = self.app_id(),
            %key,
            sha256 = %response.sha256,
            stored = response.size,
            "blob put stored"
        );
        Ok(())
    }

    /// Remove an object. Deleting a key that is already absent succeeds.
    pub async fn delete(&self, key: String) -> Result<()> {
        tracing::debug!(app_id = self.app_id(), %key, "blob delete");
        let mut client = self.client.clone();
        let response = client
            .delete(self.tenant.stamp(DeleteRequest { key: key.clone() }))
            .await?
            .into_inner();

        if !response.success {
            let reason = response
                .error
                .unwrap_or_else(|| "no reason given".to_string());
            tracing::warn!(
                app_id = self.app_id(),
                %key,
                %reason,
                "blob delete failed; the object is still stored"
            );
            anyhow::bail!("blob delete failed for key {key}: {reason}");
        }
        Ok(())
    }
}

/// Frame one object body into the message sequence `Put` expects: the header
/// first, then the body in [`CHUNK_BYTES`] slices.
///
/// The slices are cut as the stream is polled rather than all at once, so
/// sending a large object costs the body plus one chunk instead of two full
/// copies of it.
///
/// An empty body yields the header alone, which stores a zero-length object.
fn put_messages(
    key: &str,
    content_type: Option<String>,
    data: Vec<u8>,
) -> impl Stream<Item = PutRequest> + Send + 'static {
    let header = PutRequest {
        payload: Some(put_request::Payload::Header(PutHeader {
            key: key.to_string(),
            content_type,
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
            let end = (sent + CHUNK_BYTES).min(data.len());
            let chunk = PutRequest {
                payload: Some(put_request::Payload::Chunk(data[sent..end].to_vec())),
            };
            Some((chunk, (None, data, end)))
        },
    )
}

/// Reassemble a `Get` stream: one `info` message, then body chunks in order.
///
/// A stream that breaks that shape is an error rather than a short read: the
/// caller asked for an object, and handing it a silently truncated body would
/// look like success.
async fn collect_body<S>(app_id: &str, key: &str, inbound: S) -> Result<Vec<u8>>
where
    S: Stream<Item = Result<GetResponse, Status>>,
{
    tokio::pin!(inbound);

    let mut info = None;
    let mut body: Vec<u8> = Vec::new();

    while let Some(message) = inbound.next().await {
        match message?.payload {
            Some(get_response::Payload::Info(header)) => {
                if info.is_some() {
                    anyhow::bail!("blob get stream for {key} sent a second header");
                }
                let hint = usize::try_from(header.size)
                    .unwrap_or(usize::MAX)
                    .min(MAX_PREALLOC_BYTES);
                body.reserve(hint);
                info = Some(header);
            }
            Some(get_response::Payload::Chunk(chunk)) => {
                if info.is_none() {
                    anyhow::bail!("blob get stream for {key} sent a body chunk before its header");
                }
                body.extend_from_slice(&chunk);
            }
            // Forward compatibility: a payload this build does not know about
            // decodes to `None`. Skipping it keeps the body contiguous.
            None => {
                tracing::warn!(
                    %app_id,
                    %key,
                    "blob get stream carried a message with no payload; skipping it"
                );
            }
        }
    }

    let info =
        info.ok_or_else(|| anyhow!("blob get stream for {key} ended without sending its header"))?;

    if body.len() as u64 != info.size {
        tracing::error!(
            %app_id,
            %key,
            expected = info.size,
            received = body.len(),
            sha256 = %info.sha256,
            "blob get body length disagrees with its metadata; refusing to hand the application a \
             corrupt object, the read fails instead"
        );
        anyhow::bail!(
            "blob get for {key} returned {} bytes, metadata says {}",
            body.len(),
            info.size
        );
    }

    Ok(body)
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

    fn info_message(size: u64) -> GetResponse {
        GetResponse {
            payload: Some(get_response::Payload::Info(BlobInfo {
                key: "k".to_string(),
                sha256: "aa".to_string(),
                size,
                content_type: None,
                created_at: 0,
            })),
        }
    }

    fn chunk_message(bytes: &[u8]) -> GetResponse {
        GetResponse {
            payload: Some(get_response::Payload::Chunk(bytes.to_vec())),
        }
    }

    async fn framed(key: &str, content_type: Option<String>, data: Vec<u8>) -> Vec<PutRequest> {
        put_messages(key, content_type, data).collect().await
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
        let data: Vec<u8> = (0..CHUNK_BYTES * 2 + 7).map(|i| (i % 251) as u8).collect();
        let messages = framed("k", None, data.clone()).await;

        assert_eq!(messages.len(), 4, "one header plus three chunks");
        assert_eq!(chunk_of(&messages[1]).len(), CHUNK_BYTES);
        assert_eq!(chunk_of(&messages[2]).len(), CHUNK_BYTES);
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
            Ok(info_message(6)),
            Ok(chunk_message(b"abc")),
            Ok(chunk_message(b"def")),
        ];
        let body = collect_body("app", "k", stream::iter(messages))
            .await
            .expect("stream is well formed");
        assert_eq!(body, b"abcdef");
    }

    #[tokio::test]
    async fn download_accepts_a_zero_length_object() {
        let body = collect_body("app", "k", stream::iter(vec![Ok(info_message(0))]))
            .await
            .expect("header alone is a complete empty object");
        assert!(body.is_empty());
    }

    #[tokio::test]
    async fn download_rejects_a_body_that_never_declared_a_header() {
        let messages = vec![Ok(chunk_message(b"abc"))];
        let err = collect_body("app", "k", stream::iter(messages))
            .await
            .expect_err("a chunk before the header is a protocol violation");
        assert!(err.to_string().contains("before its header"), "{err}");
    }

    #[tokio::test]
    async fn download_rejects_an_empty_stream() {
        let messages: Vec<Result<GetResponse, Status>> = Vec::new();
        let err = collect_body("app", "k", stream::iter(messages))
            .await
            .expect_err("a stream with no header answered nothing");
        assert!(
            err.to_string().contains("without sending its header"),
            "{err}"
        );
    }

    #[tokio::test]
    async fn download_rejects_a_truncated_body() {
        let messages = vec![Ok(info_message(10)), Ok(chunk_message(b"abc"))];
        let err = collect_body("app", "k", stream::iter(messages))
            .await
            .expect_err("a short body must not look like a successful read");
        assert!(err.to_string().contains("metadata says 10"), "{err}");
    }

    #[tokio::test]
    async fn download_propagates_a_mid_stream_status() {
        let messages = vec![
            Ok(info_message(6)),
            Err(Status::data_loss("content file is missing")),
        ];
        let err = collect_body("app", "k", stream::iter(messages))
            .await
            .expect_err("a status inside the stream must not be swallowed");
        assert!(err.to_string().contains("content file is missing"), "{err}");
    }

    #[tokio::test]
    async fn download_skips_a_payload_it_cannot_decode() {
        let messages = vec![
            Ok(info_message(3)),
            Ok(GetResponse { payload: None }),
            Ok(chunk_message(b"abc")),
        ];
        let body = collect_body("app", "k", stream::iter(messages))
            .await
            .expect("an unknown payload is skipped, not fatal");
        assert_eq!(body, b"abc");
    }
}
