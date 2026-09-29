//! Key/value client backing `ctx.kv`.
//!
//! Talks to deis-store, which partitions data by tenant -- one application in
//! one environment. The tenant is the one named by the capability token this
//! node presents in `Authorization: Bearer` (stamped by [`StorageIdentity`]),
//! not a field of the proto messages, so one client speaks for exactly one environment of one
//! application and a preview cannot read or overwrite what production
//! stored.

use super::bindings::CapabilityChannel;
use super::bindings::StorageIdentity;
use anyhow::Result;

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
    /// The bytes the last write stored, handed back untouched. Bytes and not
    /// a string because a KV value is whatever the application wrote -- an
    /// image, a compressed frame, a serialised record -- and only the caller
    /// knows how to read it.
    pub value: Vec<u8>,
    pub metadata: Option<String>,
    pub expiration: Option<i64>,
}

/// One key as a *listing* reports it.
///
/// The stored value is deliberately absent. A page holds up to ten thousand
/// keys and deis-store caps one gRPC message at 16 MB, so a listing that
/// carried values would fail on exactly the tenants that most need to
/// enumerate. An application that wants a value asks for it by name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KvListEntry {
    /// The key itself.
    pub name: String,
    /// Whatever the last write attached, returned untouched; `None` when the
    /// write attached nothing.
    pub metadata: Option<String>,
    /// Absolute unix second this entry lapses at; `None` never does.
    pub expiration: Option<i64>,
}

/// One page of a prefix scan, and where to pick the scan up again.
///
/// `cursor` is `Some` exactly when deis-store filled the page to the limit,
/// which is *not* the same as "there is more". A prefix holding four keys
/// listed two at a time answers with a cursor after the second page, and
/// following it yields an empty page with no cursor. That is the store's
/// contract (`Kv::list` in deis-store), and it is why a caller must page
/// until the cursor is gone rather than until a page comes back short.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KvListPage {
    /// The keys of this page, in key order.
    pub keys: Vec<KvListEntry>,
    /// What to pass as the next call's `cursor`, or `None` when the scan is
    /// finished.
    pub cursor: Option<String>,
}

/// KV client for one tenant.
///
/// Cloning is cheap and every call clones: the inner tonic client wraps an
/// HTTP/2 `Channel`, which is a reference-counted handle that multiplexes
/// concurrent calls over one connection. Holding one shared client behind a
/// lock instead would serialise the tenant's KV traffic, so a single slow
/// call would stall every other one the application had in flight.
#[derive(Clone)]
pub struct KvClient {
    client: KvStoreClient<CapabilityChannel>,
    identity: StorageIdentity,
}

impl KvClient {
    /// The same client, speaking for one named binding of this tenant.
    ///
    /// Shares the channel: the tonic client is a reference-counted handle over
    /// one HTTP/2 connection, so a version declaring four namespaces holds
    /// four identities and one connection rather than four. `None` gives the
    /// tenant-level client this one was derived from.
    pub(crate) fn for_binding(&self, binding: Option<&str>) -> Result<Self> {
        let identity = self.identity.for_binding(binding)?;
        tracing::debug!(
            app_id = identity.tenant(),
            binding = identity.binding().unwrap_or("<unnamed>"),
            "KV calls of this client are charged to {}",
            identity.describe()
        );
        Ok(Self {
            identity,
            ..self.clone()
        })
    }

    /// Build a client that dials deis-store on its first call.
    ///
    /// This is what an application binding uses: constructing it cannot fail
    /// because the store is down, so a store outage never stops an app from
    /// loading or takes the node with it. The first call after an outage
    /// dials again, so the binding recovers on its own once the store is
    /// back. Only an address that is not a URL is refused here -- and it would
    /// never start working.
    pub(crate) fn connect_lazy(endpoint: &str, identity: &StorageIdentity) -> Result<Self> {
        let channel =
            super::bindings::unary_channel(endpoint, deis_identity::ServiceKind::Store, identity)?;
        let client = KvStoreClient::new(channel)
            .max_decoding_message_size(crate::MAX_GRPC_MESSAGE_BYTES)
            .max_encoding_message_size(crate::MAX_GRPC_MESSAGE_BYTES);
        tracing::debug!(
            endpoint,
            app_id = identity.tenant(),
            "KV client prepared; it will dial on the first call"
        );
        Ok(Self {
            client,
            identity: identity.clone(),
        })
    }

    /// The tenant -- application and environment -- every call of this client
    /// is charged to.
    pub fn tenant(&self) -> &str {
        self.identity.tenant()
    }

    /// Get value
    ///
    /// # Parameters
    /// - key: Key
    ///
    /// # Returns
    /// - Some(value): Value found
    /// - None: Key does not exist
    pub async fn get(&self, key: String) -> Result<Option<Vec<u8>>> {
        Ok(self.get_with_metadata(key).await?.map(|read| read.value))
    }

    /// Get a value together with the metadata and expiry stored with it.
    ///
    /// Backs `ctx.env.KV.getWithMetadata`. It is a separate method rather
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
        let request = self.identity.stamp(GetRequest { key: key.clone() });

        tracing::debug!("KV Get: {}", key);

        let mut client = self.client.clone();
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
    pub async fn set(&self, key: String, value: Vec<u8>) -> Result<()> {
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
        value: Vec<u8>,
        ttl_seconds: Option<i64>,
        metadata: Option<String>,
    ) -> Result<()> {
        // The size, not the value: a value may be megabytes of binary, and a
        // log line is not the place for it.
        let bytes = value.len();
        let request = self.identity.stamp(SetRequest {
            key: key.clone(),
            value,
            ttl_seconds,
            metadata,
        });

        tracing::debug!("KV Set: {} ({} bytes, ttl={:?})", key, bytes, ttl_seconds);

        let mut client = self.client.clone();
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
        let request = self.identity.stamp(DeleteRequest { key: key.clone() });

        tracing::debug!("KV Delete: {}", key);

        let mut client = self.client.clone();
        let response = client.delete(request).await?;
        let delete_response = response.into_inner();

        if !delete_response.success {
            tracing::warn!("KV Delete failed for key: {}", key);
        }

        Ok(())
    }

    /// One page of the keys under `prefix`, in key order.
    ///
    /// `limit` is a *request*: deis-store substitutes its default (1000) for
    /// an absent or non-positive one and caps anything above its ceiling
    /// (10000), so a caller that asks for a million gets ten thousand and a
    /// cursor rather than an error. `cursor` resumes a previous page and is
    /// `None` for the first one.
    ///
    /// Every listing asks for metadata, because `ctx.env.KV.list`
    /// promises it and the columns are on rows the scan already visits -- what
    /// the flag buys is response *size*, and a page of names alone is not a
    /// shape anything in this runtime wants.
    pub async fn list(
        &self,
        prefix: String,
        limit: Option<i32>,
        cursor: Option<String>,
    ) -> Result<KvListPage> {
        let request = self.identity.stamp(ListRequest {
            prefix: prefix.clone(),
            limit,
            cursor: cursor.clone(),
            include_metadata: Some(true),
        });

        tracing::debug!(
            prefix = %prefix,
            limit = ?limit,
            resuming = cursor.is_some(),
            "KV List"
        );

        let mut client = self.client.clone();
        let response = client.list(request).await?;
        let list_response = response.into_inner();

        // Asked with `include_metadata`, deis-store answers the page in
        // `entries`, each key with its metadata and expiry.
        let keys = list_response
            .entries
            .into_iter()
            .map(|entry| KvListEntry {
                name: entry.key,
                metadata: entry.metadata,
                expiration: entry.expires_at,
            })
            .collect();

        Ok(KvListPage {
            keys,
            cursor: list_response.next_cursor,
        })
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
        let request = self.identity.stamp(HasRequest { key: key.clone() });

        tracing::debug!("KV Has: {}", key);

        let mut client = self.client.clone();
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
        signing_key: ed25519_dalek::SigningKey,
        shutdown: Option<oneshot::Sender<()>>,
    }

    impl Store {
        /// Start a store with capability-based auth using an ephemeral key.
        async fn start() -> Self {
            Self::start_with_key(crate::runtime::test_store::test_signing_key()).await
        }

        async fn start_with_key(signing_key: ed25519_dalek::SigningKey) -> Self {
            let data = TempDir::new().expect("temp dir");
            let verifying_key = signing_key.verifying_key();
            let config = Config {
                listen: "127.0.0.1:0".to_string(),
                data_dir: data.path().to_path_buf(),
                ..Config::default()
            };
            let listener = server::bind(&config).await.expect("bind");
            let addr = listener.local_addr().expect("local addr");
            let (shutdown, rx) = oneshot::channel();
            let identity = crate::runtime::test_store::fleet()
                .service_identity(deis_identity::ServiceKind::Store);
            // The store polls its revocation list from a stand-in control
            // plane signing with the same key, and accepts nothing until it
            // has one.
            let control =
                deis_capability::testing::FakeControl::start(signing_key.clone()).client();
            tokio::spawn(async move {
                server::serve(
                    config,
                    listener,
                    async {
                        let _ = rx.await;
                    },
                    deis_capability::KeyRing::from_keys([verifying_key]),
                    control,
                    identity,
                )
                .await
                .expect("serve");
            });
            Self {
                _data: data,
                endpoint: format!("https://{addr}"),
                signing_key,
                shutdown: Some(shutdown),
            }
        }

        fn capability_token_for(&self, tenant: &str) -> String {
            use deis_capability::{encode, CapabilityClaims, CapabilityResource};
            use std::time::Duration;
            let claims = CapabilityClaims::new(
                "store",
                "spiffe://deislet/control/main",
                "spiffe://deislet/node/test",
                1,
                Duration::from_secs(900),
                vec![CapabilityResource {
                    tenant: crate::runtime::tenant::test_tenant(tenant),
                    binding: "kv".to_string(),
                    generation: None,
                }],
            );
            encode(&claims, &self.signing_key)
        }

        /// A client for one tenant, wired the way an application binding is:
        /// lazily dialled, carrying whatever proof the node was handed.
        /// `tenant` is the `<app>.<environment>` pair the store keys on.
        fn binding(&self, tenant: &str, token: Option<&str>) -> KvClient {
            let identity = StorageIdentity::new(crate::runtime::tenant::test_tenant(tenant), token)
                .expect("a legal tenant");
            KvClient::connect_lazy(&self.endpoint, &identity).expect("client is built")
        }

        /// The common case: a store that always yields a proven client.
        fn client(&self, tenant: &str) -> KvClient {
            let token = self.capability_token_for(tenant);
            self.binding(tenant, Some(&token))
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
        let client = store.client("demo.production");

        client
            .set_with_options("session".to_string(), b"abc".to_vec(), Some(3600), None)
            .await
            .expect("write with a lifetime");

        assert_eq!(
            client.get("session".to_string()).await.expect("read back"),
            Some(b"abc".to_vec())
        );
    }

    #[tokio::test]
    async fn a_value_that_is_not_valid_utf8_survives_the_round_trip() {
        // A KV value is whatever the application wrote. While the wire typed
        // it as a string, the store held these bytes intact and `Get` refused
        // to hand them back, so a write that reported success produced a key
        // that could never be read again. Over a real socket, because the two
        // halves are owned by different crates.
        let store = Store::start().await;
        let client = store.client("demo.production");

        // A PNG signature followed by a lone continuation byte and 0xFF:
        // valid as bytes, invalid as UTF-8 at the first character.
        let png = vec![0x89u8, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 0x80, 0xFF];
        assert!(
            String::from_utf8(png.clone()).is_err(),
            "the fixture has to be bytes a string could not hold"
        );

        client
            .set("avatar".to_string(), png.clone())
            .await
            .expect("write raw bytes");

        assert_eq!(
            client.get("avatar".to_string()).await.expect("read back"),
            Some(png)
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
        let client = store.client("demo.production");

        client
            .set_with_options(
                "avatar".to_string(),
                b"png-bytes".to_vec(),
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
        assert_eq!(read.value, b"png-bytes");
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
        let client = store.client("demo.production");

        client
            .set("plain".to_string(), b"value".to_vec())
            .await
            .expect("plain write");

        let read = client
            .get_with_metadata("plain".to_string())
            .await
            .expect("read back")
            .expect("the key exists");
        assert_eq!(read.value, b"value");
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
        let client = store.client("demo.production");

        let before = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock is after the epoch")
            .as_secs() as i64;
        client
            .set_with_options("session".to_string(), b"abc".to_vec(), Some(3600), None)
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
        let client = store.client("demo.production");

        let error = client
            .set_with_options("k".to_string(), b"v".to_vec(), Some(0), None)
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
        let client = store.client("demo.production");

        client
            .set("greeting".to_string(), b"hello".to_vec())
            .await
            .expect("plain write");

        assert_eq!(
            client.get("greeting".to_string()).await.expect("read back"),
            Some(b"hello".to_vec())
        );
        assert!(client.has("greeting".to_string()).await.expect("has"));
    }

    #[tokio::test]
    async fn a_listing_carries_each_key_s_metadata_and_pages_on_the_store_s_cursor() {
        // The seam deis-store owns: this client asks for `include_metadata`,
        // and the store answers that request in `entries` while leaving
        // `keys` empty. A client that read the wrong one of the two would
        // report every listing as empty, and no unit test on either half
        // alone would notice.
        let store = Store::start().await;
        let client = store.client("demo.production");

        client
            .set_with_options(
                "session:a".to_string(),
                b"one".to_vec(),
                Some(3600),
                Some(r#"{"user":7}"#.to_string()),
            )
            .await
            .expect("write with a lifetime and metadata");
        for key in ["session:b", "session:c"] {
            client
                .set(key.to_string(), b"x".to_vec())
                .await
                .expect("plain write");
        }
        client
            .set("other".to_string(), b"x".to_vec())
            .await
            .expect("plain write");

        let first = client
            .list("session:".to_string(), Some(2), None)
            .await
            .expect("the first page");
        assert_eq!(
            first
                .keys
                .iter()
                .map(|entry| entry.name.as_str())
                .collect::<Vec<_>>(),
            vec!["session:a", "session:b"],
            "a page is in key order and stops at the limit"
        );
        assert_eq!(first.keys[0].metadata.as_deref(), Some(r#"{"user":7}"#));
        assert!(
            first.keys[0].expiration.is_some(),
            "a key written with a lifetime has to report when it lapses"
        );
        assert_eq!(first.keys[1].metadata, None);
        assert_eq!(first.keys[1].expiration, None);

        let cursor = first
            .cursor
            .expect("a page filled to the limit hands back where to resume");
        let second = client
            .list("session:".to_string(), Some(2), Some(cursor))
            .await
            .expect("the second page");
        assert_eq!(
            second
                .keys
                .iter()
                .map(|entry| entry.name.as_str())
                .collect::<Vec<_>>(),
            vec!["session:c"],
            "the cursor resumes after the last key of the previous page, and the prefix still \
             holds: `other` is not in it"
        );
        assert_eq!(
            second.cursor, None,
            "a page that came back short is the end of the scan"
        );
    }

    #[tokio::test]
    async fn one_app_cannot_see_another_app_s_keys() {
        // The tenant travels in the header, not in the message; a client that
        // stopped stamping it would read another application's data.
        let store = Store::start().await;
        let first = store.client("app-one.production");
        let second = store.client("app-two.production");

        first
            .set("shared-name".to_string(), b"one".to_vec())
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
        let production = store.client("demo.production");
        let preview = store.client("demo.preview-7");

        production
            .set("customer".to_string(), b"real".to_vec())
            .await
            .expect("write as production");
        preview
            .set("customer".to_string(), b"fixture".to_vec())
            .await
            .expect("write as the preview");

        assert_eq!(
            production
                .get("customer".to_string())
                .await
                .expect("read as production"),
            Some(b"real".to_vec()),
            "a preview deployment overwrote production's data"
        );
        assert_eq!(
            preview
                .get("customer".to_string())
                .await
                .expect("read as the preview"),
            Some(b"fixture".to_vec())
        );
    }

    /// The gRPC code a refusal must arrive as, so a caller can tell "you are
    /// not who you say" from "that tenant does not exist for me".
    fn refusal(error: &anyhow::Error) -> tonic::Code {
        error
            .downcast_ref::<tonic::Status>()
            .unwrap_or_else(|| {
                panic!("the store answered with something that is not a gRPC status: {error:#}")
            })
            .code()
    }

    #[tokio::test]
    async fn a_store_that_requires_a_token_refuses_a_client_without_one_and_serves_one_with_it() {
        // The whole delivery path in one test, over a real socket: the store
        // trusts the control plane's signing key, the control plane would sign
        // this capability with it, and the client stamps it beside the tenant
        // it claims. Asserting on the header alone would not catch the two
        // halves disagreeing.
        let store = Store::start().await;
        let tenant = "demo.production";
        let token = store.capability_token_for(tenant);

        let unproven = store.binding(tenant, None);
        let error = unproven
            .set("k".to_string(), b"v".to_vec())
            .await
            .expect_err("a store must not serve an unproven call");
        assert_eq!(
            refusal(&error),
            tonic::Code::Unauthenticated,
            "a missing token is a credential problem, not a bad tenant: {error:#}"
        );

        let proven = store.binding(tenant, Some(&token));
        proven
            .set("k".to_string(), b"v".to_vec())
            .await
            .expect("a call carrying this tenant's token is served");
        assert_eq!(
            proven.get("k".to_string()).await.expect("read back"),
            Some(b"v".to_vec())
        );

        assert_eq!(
            refusal(
                &unproven
                    .get("k".to_string())
                    .await
                    .expect_err("still unproven")
            ),
            tonic::Code::Unauthenticated,
            "an unproven client must not read what a proven one wrote"
        );
    }

    #[tokio::test]
    async fn the_capability_not_the_client_decides_the_tenant() {
        // deis-store acts for the tenant the capability names. A client
        // labelled for the preview but carrying production's token writes
        // production's data - and the preview, with its own token, sees none
        // of it.
        let store = Store::start().await;
        let production_token = store.capability_token_for("demo.production");

        store
            .binding("demo.preview-7", Some(&production_token))
            .set("k".to_string(), b"v".to_vec())
            .await
            .expect("production's token is served");
        assert_eq!(
            store
                .client("demo.production")
                .get("k".to_string())
                .await
                .unwrap(),
            Some(b"v".to_vec())
        );
        assert_eq!(
            store
                .client("demo.preview-7")
                .get("k".to_string())
                .await
                .unwrap(),
            None
        );
    }

    /// A token the store refuses is reported to the renewer from under the
    /// client, so the node replaces it now rather than when it falls due.
    #[tokio::test]
    async fn a_refused_token_is_reported_to_the_renewer() {
        use crate::deployment::capability_renewer;
        let store = Store::start().await;
        let tenant = &crate::runtime::tenant::test_tenant("kv-refused.production");
        // A genuine signature for this tenant, but for the sql binding: the
        // kv service finds no tenant it may act for and refuses it.
        let wrong = {
            use deis_capability::{encode, CapabilityClaims, CapabilityResource};
            encode(
                &CapabilityClaims::new(
                    "store",
                    "spiffe://deislet/control/main",
                    "spiffe://deislet/node/test",
                    1,
                    std::time::Duration::from_secs(900),
                    vec![CapabilityResource {
                        tenant: tenant.clone(),
                        binding: "sql".to_string(),
                        generation: None,
                    }],
                ),
                &store.signing_key,
            )
        };
        let identity = StorageIdentity::new(tenant.as_str(), Some(&wrong)).expect("a legal tenant");
        capability_renewer::register(tenant, "store", identity.token_lock());
        let client = KvClient::connect_lazy(&store.endpoint, &identity).expect("client is built");
        assert!(!capability_renewer::is_refused(tenant, "store"));

        let error = client
            .get("k".to_string())
            .await
            .expect_err("a token naming no tenant for kv is refused");
        assert_eq!(refusal(&error), tonic::Code::PermissionDenied, "{error:#}");
        assert!(
            capability_renewer::is_refused(tenant, "store"),
            "the refusal did not reach the renewer"
        );
    }

    #[tokio::test]
    async fn a_lazy_client_reports_the_transport_error_on_the_call() {
        // Nothing listens on port 1. Building the client must succeed -- an
        // app may not be blocked from loading by a store outage -- and the
        // call must fail with something an operator can act on.
        let identity =
            StorageIdentity::new(crate::runtime::tenant::test_tenant("demo.production"), None)
                .expect("a legal tenant");
        let client =
            KvClient::connect_lazy("http://127.0.0.1:1", &identity).expect("client is built");
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
        let client = store.client("demo.production");

        client
            .set("k".to_string(), b"v".to_vec())
            .await
            .expect("write over a lazily dialled channel");
        assert_eq!(
            client.get("k".to_string()).await.expect("read back"),
            Some(b"v".to_vec())
        );
    }
}
