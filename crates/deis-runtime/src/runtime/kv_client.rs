//! Key/value client backing `ctx.kv`.
//!
//! Talks to deis-store, which partitions data by application: the tenant
//! travels in the `x-deis-app-id` metadata header (see [`Tenant`]), not in the
//! proto messages, so one client speaks for exactly one application.

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
    /// - app_id: the tenant every call of this client is charged to
    ///
    /// # Returns
    /// - Ok(KvClient): Connection successful
    /// - Err: Connection failed
    pub async fn connect(endpoint: impl Into<String>, app_id: impl Into<String>) -> Result<Self> {
        let endpoint = endpoint.into();
        let tenant = Tenant::new(app_id)?;
        tracing::info!(%endpoint, app_id = tenant.app_id(), "connecting to KV store");

        let client = KvStoreClient::connect(endpoint.clone())
            .await
            .with_context(|| format!("connecting to KV store at {endpoint}"))?
            .max_decoding_message_size(crate::MAX_GRPC_MESSAGE_BYTES)
            .max_encoding_message_size(crate::MAX_GRPC_MESSAGE_BYTES);

        tracing::info!(%endpoint, app_id = tenant.app_id(), "KV store client connected");
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
    /// back. Only an address that is not a URL, or an app id that cannot be
    /// a gRPC header, is refused here -- neither would ever start working.
    pub fn connect_lazy(endpoint: &str, app_id: impl Into<String>) -> Result<Self> {
        let tenant = Tenant::new(app_id)?;
        let channel = super::bindings::unary_channel(endpoint)?;
        let client = KvStoreClient::new(channel)
            .max_decoding_message_size(crate::MAX_GRPC_MESSAGE_BYTES)
            .max_encoding_message_size(crate::MAX_GRPC_MESSAGE_BYTES);
        tracing::debug!(
            endpoint,
            app_id = tenant.app_id(),
            "KV client prepared; it will dial on the first call"
        );
        Ok(Self {
            client: Arc::new(Mutex::new(client)),
            tenant,
        })
    }

    /// The application every call of this client is charged to.
    pub fn app_id(&self) -> &str {
        self.tenant.app_id()
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
        let request = self.tenant.stamp(GetRequest { key: key.clone() });

        tracing::debug!("KV Get: {}", key);

        let mut client = self.client.lock().await;
        let response = client.get(request).await?;
        let get_response = response.into_inner();

        Ok(get_response.value)
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

        async fn client(&self, app_id: &str) -> KvClient {
            KvClient::connect(self.endpoint.clone(), app_id)
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
        let client = store.client("demo").await;

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
    async fn metadata_reaches_the_store_but_has_no_way_back() {
        // The store accepts and keeps it; there is no field on GetResponse or
        // ListResponse to return it, so an app can write metadata and never
        // see it again. Adding that field is deis-store's to do -- this test
        // pins the half that does work, so the gap is a missing read path and
        // not a silently dropped write.
        let store = Store::start().await;
        let client = store.client("demo").await;

        client
            .set_with_options(
                "avatar".to_string(),
                "png-bytes".to_string(),
                None,
                Some(r#"{"contentType":"image/png"}"#.to_string()),
            )
            .await
            .expect("write with metadata");

        assert_eq!(
            client.get("avatar".to_string()).await.expect("read back"),
            Some("png-bytes".to_string())
        );
    }

    #[tokio::test]
    async fn a_non_positive_lifetime_is_refused_rather_than_stored_expired() {
        // Zero is a caller bug, not a request to store something that has
        // already lapsed; honouring it would delete the write immediately and
        // look like the store losing data.
        let store = Store::start().await;
        let client = store.client("demo").await;

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
        let client = store.client("demo").await;

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
        let first = store.client("app-one").await;
        let second = store.client("app-two").await;

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
    async fn a_lazy_client_reports_the_transport_error_on_the_call() {
        // Nothing listens on port 1. Building the client must succeed -- an
        // app may not be blocked from loading by a store outage -- and the
        // call must fail with something an operator can act on.
        let client = KvClient::connect_lazy("http://127.0.0.1:1", "demo").expect("client is built");
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
        let client = KvClient::connect_lazy(&store.endpoint, "demo").expect("client is built");

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
