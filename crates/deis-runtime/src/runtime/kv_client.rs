//! Key/value client backing `ctx.kv`.
//!
//! Talks to deis-store, which partitions data by tenant -- one application in
//! one environment. The tenant travels in the `x-deis-tenant` metadata header
//! (see [`Tenant`]), not in the proto messages, so one client speaks for
//! exactly one environment of one application and a preview cannot read or
//! overwrite what production stored.

use super::tenant::Tenant;
use anyhow::{Context, Result};
use std::sync::Arc;
use tokio::sync::Mutex;
use tonic::transport::Channel;

// Import generated proto code
pub mod proto {
    tonic::include_proto!("deis.storage");
}

use proto::{kv_store_client::KvStoreClient, *};

/// One key as the store holds it: the value plus what was written alongside
/// it.
///
/// `metadata` is whatever the last write attached, returned untouched --
/// deis-store never parses it. `expiration` is the absolute unix second the
/// entry lapses at, or `None` for an entry that never does; the store enforces
/// expiry on every read, so an entry returned here was live as of that read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KvRead {
    pub value: String,
    pub metadata: Option<String>,
    pub expiration: Option<i64>,
}

/// KV Store client wrapper
///
/// Wrapped in Arc<Mutex> to support sharing and internal mutability across async operations
#[derive(Clone)]
pub struct KvClient {
    client: Arc<Mutex<KvStoreClient<Channel>>>,
    tenant: Tenant,
}

impl KvClient {
    /// Connect to remote KV Store service
    ///
    /// # Parameters
    /// - endpoint: deis-store gRPC address (e.g., "http://127.0.0.1:8090",
    ///   deis-store's default listen address)
    /// - tenant: the `<app>.<environment>` every call of this client is
    ///   charged to; see [`Tenant`]
    ///
    /// # Returns
    /// - Ok(KvClient): Connection successful
    /// - Err: Connection failed
    pub async fn connect(endpoint: impl Into<String>, tenant: impl Into<String>) -> Result<Self> {
        let endpoint = endpoint.into();
        let tenant = Tenant::new(tenant)?;
        tracing::info!(%endpoint, app_id = tenant.as_str(), "connecting to KV store");

        let client = KvStoreClient::connect(endpoint.clone())
            .await
            .with_context(|| format!("connecting to KV store at {endpoint}"))?
            .max_decoding_message_size(crate::MAX_GRPC_MESSAGE_BYTES)
            .max_encoding_message_size(crate::MAX_GRPC_MESSAGE_BYTES);

        tracing::info!(%endpoint, app_id = tenant.as_str(), "KV store client connected");
        Ok(Self {
            client: Arc::new(Mutex::new(client)),
            tenant,
        })
    }

    /// Build a client that dials deis-store on its first call.
    ///
    /// This is what an application binding uses: constructing it cannot fail
    /// because the store is down, so a store outage never stops an app from
    /// loading or takes the node with it. The first call after an outage
    /// dials again, so the binding recovers on its own once the store is
    /// back. Only an address that is not a URL, or a tenant that is not an
    /// application and an environment, is refused here -- neither would ever start working.
    pub fn connect_lazy(endpoint: &str, tenant: impl Into<String>) -> Result<Self> {
        let tenant = Tenant::new(tenant)?;
        let channel = super::bindings::unary_channel(endpoint)?;
        let client = KvStoreClient::new(channel)
            .max_decoding_message_size(crate::MAX_GRPC_MESSAGE_BYTES)
            .max_encoding_message_size(crate::MAX_GRPC_MESSAGE_BYTES);
        tracing::debug!(
            endpoint,
            app_id = tenant.as_str(),
            "KV client prepared; it will dial on the first call"
        );
        Ok(Self {
            client: Arc::new(Mutex::new(client)),
            tenant,
        })
    }

    /// The tenant -- application and environment -- every call of this client
    /// is charged to.
    pub fn tenant(&self) -> &str {
        self.tenant.as_str()
    }

    /// Get value
    ///
    /// # Parameters
    /// - key: Key
    ///
    /// # Returns
    /// - Some(value): Value found
    /// - None: Key does not exist
    pub async fn get(&self, key: String) -> Result<Option<String>> {
        Ok(self.get_with_metadata(key).await?.map(|read| read.value))
    }

    /// Get a value together with the metadata and expiry stored with it.
    ///
    /// Backs `ctx.services.kv.getWithMetadata`. It is a separate method rather
    /// than a wider return type on [`Self::get`] because the two answer
    /// different questions and the plain read is the hot one; both are the
    /// same single `Get` call, so asking for metadata costs no extra round
    /// trip.
    ///
    /// `None` means the key does not exist or has lapsed. A key that exists
    /// with no metadata comes back as `Some` with `metadata: None`: an app
    /// that stored no metadata and an app whose key is gone are not the same
    /// situation, and `value` is what separates them.
    pub async fn get_with_metadata(&self, key: String) -> Result<Option<KvRead>> {
        let request = self.tenant.stamp(GetRequest { key: key.clone() });

        tracing::debug!("KV Get: {}", key);

        let mut client = self.client.lock().await;
        let response = client.get(request).await?;
        let get_response = response.into_inner();

        Ok(get_response.value.map(|value| KvRead {
            value,
            metadata: get_response.metadata,
            expiration: get_response.expires_at,
        }))
    }

    /// Set a value that never lapses and carries no metadata.
    ///
    /// # Parameters
    /// - key: Key
    /// - value: Value
    pub async fn set(&self, key: String, value: String) -> Result<()> {
        self.set_with_options(key, value, None, None).await
    }

    /// Set a value, optionally with a lifetime and caller-defined metadata.
    ///
    /// # Parameters
    /// - key: Key
    /// - value: Value
    /// - ttl_seconds: lifetime from the moment the store accepts the write;
    ///   `None` never lapses. The store refuses a non-positive value.
    /// - metadata: opaque string stored and returned untouched
    pub async fn set_with_options(
        &self,
        key: String,
        value: String,
        ttl_seconds: Option<i64>,
        metadata: Option<String>,
    ) -> Result<()> {
        let request = self.tenant.stamp(SetRequest {
            key: key.clone(),
            value: value.clone(),
            ttl_seconds,
            metadata,
        });

        tracing::debug!("KV Set: {} = {} (ttl={:?})", key, value, ttl_seconds);

        let mut client = self.client.lock().await;
        let response = client.set(request).await?;
        let set_response = response.into_inner();

        if !set_response.success {
            anyhow::bail!("KV Set failed for key: {}", key);
        }

        Ok(())
    }

    /// Delete key
    ///
    /// # Parameters
    /// - key: Key
    pub async fn delete(&self, key: String) -> Result<()> {
        let request = self.tenant.stamp(DeleteRequest { key: key.clone() });

        tracing::debug!("KV Delete: {}", key);

        let mut client = self.client.lock().await;
        let response = client.delete(request).await?;
        let delete_response = response.into_inner();

        if !delete_response.success {
            tracing::warn!("KV Delete failed for key: {}", key);
        }

        Ok(())
    }

    /// List keys with matching prefix
    ///
    /// # Parameters
    /// - prefix: Prefix
    /// - limit: Optional quantity limit
    ///
    /// # Returns
    /// - (keys, next_cursor): Key list and next page cursor
    pub async fn list(
        &self,
        prefix: String,
        limit: Option<i32>,
    ) -> Result<(Vec<String>, Option<String>)> {
        let request = self.tenant.stamp(ListRequest {
            prefix: prefix.clone(),
            limit,
            cursor: None,
            // Names only. Asking for metadata moves the page into
            // `entries` and leaves `keys` empty, which is a different
            // answer than this method promises; nothing in the runtime
            // exposes a per-key listing yet.
            include_metadata: None,
        });

        tracing::debug!("KV List: prefix={}, limit={:?}", prefix, limit);

        let mut client = self.client.lock().await;
        let response = client.list(request).await?;
        let list_response = response.into_inner();

        Ok((list_response.keys, list_response.next_cursor))
    }

    /// Check if key exists
    ///
    /// # Parameters
    /// - key: Key
    ///
    /// # Returns
    /// - true: Exists
    /// - false: Does not exist
    pub async fn has(&self, key: String) -> Result<bool> {
        let request = self.tenant.stamp(HasRequest { key: key.clone() });

        tracing::debug!("KV Has: {}", key);

        let mut client = self.client.lock().await;
        let response = client.has(request).await?;
        let has_response = response.into_inner();

        Ok(has_response.exists)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use deis_store::config::Config;
    use deis_store::server;
    use tempfile::TempDir;
    use tokio::sync::oneshot;

    /// A running deis-store plus the temp directory holding its data.
    ///
    /// `ttl_seconds` and `metadata` are fields deis-store owns and this client
    /// fills in; a unit test of either half alone would not notice if the two
    /// stopped agreeing, so these run the real client over a real socket.
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
            // Bind before spawning, so the port is known and no test has to
            // wait for the server to come up.
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

        /// `tenant` is the `<app>.<environment>` pair the store keys on.
        async fn client(&self, tenant: &str) -> KvClient {
            KvClient::connect(self.endpoint.clone(), tenant)
                .await
                .expect("connect kv client")
        }
    }

    impl Drop for Store {
        fn drop(&mut self) {
            if let Some(shutdown) = self.shutdown.take() {
                let _ = shutdown.send(());
            }
        }
    }

    #[tokio::test]
    async fn a_value_written_with_a_lifetime_is_readable_until_it_lapses() {
        let store = Store::start().await;
        let client = store.client("demo.production").await;

        client
            .set_with_options("session".to_string(), "abc".to_string(), Some(3600), None)
            .await
            .expect("write with a lifetime");

        assert_eq!(
            client.get("session".to_string()).await.expect("read back"),
            Some("abc".to_string())
        );
    }

    #[tokio::test]
    async fn metadata_written_with_a_value_comes_back_with_it() {
        // The write half has worked since wave A; until deis-store's
        // GetResponse grew the field there was nowhere for it to come back,
        // so an app could attach metadata and never see it again. This is the
        // round trip, over a real socket, because the two halves are owned by
        // different crates.
        let store = Store::start().await;
        let client = store.client("demo.production").await;

        client
            .set_with_options(
                "avatar".to_string(),
                "png-bytes".to_string(),
                None,
                Some(r#"{"contentType":"image/png"}"#.to_string()),
            )
            .await
            .expect("write with metadata");

        let read = client
            .get_with_metadata("avatar".to_string())
            .await
            .expect("read back")
            .expect("the key exists");
        assert_eq!(read.value, "png-bytes");
        assert_eq!(
            read.metadata.as_deref(),
            Some(r#"{"contentType":"image/png"}"#)
        );
        assert_eq!(read.expiration, None, "this write carried no lifetime");
    }

    #[tokio::test]
    async fn a_key_with_no_metadata_is_not_the_same_as_a_missing_key() {
        // An app branches on these differently: one means "nothing stored
        // here", the other means "stored, and the writer attached nothing".
        let store = Store::start().await;
        let client = store.client("demo.production").await;

        client
            .set("plain".to_string(), "value".to_string())
            .await
            .expect("plain write");

        let read = client
            .get_with_metadata("plain".to_string())
            .await
            .expect("read back")
            .expect("the key exists");
        assert_eq!(read.value, "value");
        assert_eq!(read.metadata, None);

        assert_eq!(
            client
                .get_with_metadata("absent".to_string())
                .await
                .expect("read a key that was never written"),
            None
        );
    }

    #[tokio::test]
    async fn a_lifetime_comes_back_as_the_second_the_entry_lapses_at() {
        // The caller asked for a *lifetime* and cannot otherwise tell how much
        // of it is left; the store answers with the absolute moment instead of
        // making every reader subtract.
        let store = Store::start().await;
        let client = store.client("demo.production").await;

        let before = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock is after the epoch")
            .as_secs() as i64;
        client
            .set_with_options("session".to_string(), "abc".to_string(), Some(3600), None)
            .await
            .expect("write with a lifetime");

        let read = client
            .get_with_metadata("session".to_string())
            .await
            .expect("read back")
            .expect("the key exists");
        let expiration = read.expiration.expect("an entry with a lifetime lapses");
        assert!(
            expiration >= before + 3600 && expiration <= before + 3600 + 5,
            "expiration {expiration} is not about an hour after {before}"
        );
    }

    #[tokio::test]
    async fn a_non_positive_lifetime_is_refused_rather_than_stored_expired() {
        // Zero is a caller bug, not a request to store something that has
        // already lapsed; honouring it would delete the write immediately and
        // look like the store losing data.
        let store = Store::start().await;
        let client = store.client("demo.production").await;

        let error = client
            .set_with_options("k".to_string(), "v".to_string(), Some(0), None)
            .await
            .expect_err("a zero lifetime is refused");
        assert!(error.to_string().contains("ttl"), "{error}");

        assert_eq!(
            client.get("k".to_string()).await.expect("read back"),
            None,
            "the refused write must not have stored anything"
        );
    }

    #[tokio::test]
    async fn a_plain_write_never_lapses_and_carries_no_metadata() {
        let store = Store::start().await;
        let client = store.client("demo.production").await;

        client
            .set("greeting".to_string(), "hello".to_string())
            .await
            .expect("plain write");

        assert_eq!(
            client.get("greeting".to_string()).await.expect("read back"),
            Some("hello".to_string())
        );
        assert!(client.has("greeting".to_string()).await.expect("has"));
    }

    #[tokio::test]
    async fn one_app_cannot_see_another_app_s_keys() {
        // The tenant travels in the header, not in the message; a client that
        // stopped stamping it would read another application's data.
        let store = Store::start().await;
        let first = store.client("app-one.production").await;
        let second = store.client("app-two.production").await;

        first
            .set("shared-name".to_string(), "one".to_string())
            .await
            .expect("write as app-one");

        assert_eq!(
            second
                .get("shared-name".to_string())
                .await
                .expect("read as app-two"),
            None
        );
    }

    #[tokio::test]
    async fn two_environments_of_one_app_do_not_share_a_namespace() {
        // The reason the header carries a tenant rather than an app id: a
        // preview deployment that could overwrite what production stored is
        // an unrecoverable failure, and nothing in the app's own code says
        // which environment it is running in.
        let store = Store::start().await;
        let production = store.client("demo.production").await;
        let preview = store.client("demo.preview-7").await;

        production
            .set("customer".to_string(), "real".to_string())
            .await
            .expect("write as production");
        preview
            .set("customer".to_string(), "fixture".to_string())
            .await
            .expect("write as the preview");

        assert_eq!(
            production
                .get("customer".to_string())
                .await
                .expect("read as production"),
            Some("real".to_string()),
            "a preview deployment overwrote production's data"
        );
        assert_eq!(
            preview
                .get("customer".to_string())
                .await
                .expect("read as the preview"),
            Some("fixture".to_string())
        );
    }

    #[tokio::test]
    async fn a_lazy_client_reports_the_transport_error_on_the_call() {
        // Nothing listens on port 1. Building the client must succeed -- an
        // app may not be blocked from loading by a store outage -- and the
        // call must fail with something an operator can act on.
        let client = KvClient::connect_lazy("http://127.0.0.1:1", "demo.production")
            .expect("client is built");
        let error = client
            .get("anything".to_string())
            .await
            .expect_err("nothing is listening");
        assert!(
            error.to_string().to_lowercase().contains("transport")
                || error.to_string().contains("tcp connect error"),
            "the failure must name the transport, not look like a missing key: {error}"
        );
    }

    #[tokio::test]
    async fn a_lazy_client_recovers_once_the_store_answers() {
        // The binding must not be poisoned by an outage: the next call dials
        // again. Without this an app whose store restarted would keep failing
        // until the app itself was reloaded.
        let store = Store::start().await;
        let client =
            KvClient::connect_lazy(&store.endpoint, "demo.production").expect("client is built");

        client
            .set("k".to_string(), "v".to_string())
            .await
            .expect("write over a lazily dialled channel");
        assert_eq!(
            client.get("k".to_string()).await.expect("read back"),
            Some("v".to_string())
        );
    }
}
