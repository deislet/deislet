//! A real deis-store, started in-process for the length of one test.
//!
//! Several clients in this module speak a contract that deis-store owns - the
//! KV service, the SQL database service, the chunked blob streaming protocol.
//! A unit test on either half alone proves nothing about the seam between
//! them, and the seam is exactly where a change on one side breaks the other.
//! So the tests that matter run against the real server, which is cheap: it
//! binds port 0, writes into a temporary directory and shuts down when the
//! handle is dropped.

use tokio::sync::oneshot;

use super::bindings::{ServiceDeclaration, ServiceEndpoints, ServiceWiring};

/// The one authority every test store in this process is issued a
/// certificate by, and the trust the clients under test check it against.
///
/// One per process because the clients read a process-wide trust (see
/// [`super::service_tls`]), and it can be installed once.
pub(crate) fn fleet() -> &'static deis_identity_test_pki::TestFleet {
    static FLEET: std::sync::OnceLock<deis_identity_test_pki::TestFleet> =
        std::sync::OnceLock::new();
    FLEET.get_or_init(|| {
        let fleet = deis_identity_test_pki::TestFleet::new();
        super::service_tls::install(deis_identity::ServiceTrust::from_identity(
            fleet.service_identity(deis_identity::ServiceKind::Node),
        ));
        fleet
    })
}

/// Generate a deterministic Ed25519 signing key for in-process test stores.
pub fn test_signing_key() -> ed25519_dalek::SigningKey {
    let mut seed = [0u8; 32];
    seed[0..8].copy_from_slice(b"rt-store");
    ed25519_dalek::SigningKey::from_bytes(&seed)
}

/// A running deis-store, stopped when the value is dropped.
pub(crate) struct TestStore {
    _data: tempfile::TempDir,
    pub(crate) endpoint: String,
    pub(crate) signing_key: ed25519_dalek::SigningKey,
    shutdown: Option<oneshot::Sender<()>>,
}

impl TestStore {
    /// Start with a fresh self-signed key for capability-based auth.
    pub(crate) async fn start() -> Self {
        Self::start_with_key(test_signing_key()).await
    }

    /// A store that uses a different signing key, verifying that tokens from
    /// the wrong key are refused.
    pub(crate) async fn start_with_secret(secret: &str) -> Self {
        use sha2::Digest;
        let hash = sha2::Sha256::digest(secret.as_bytes());
        let mut seed = [0u8; 32];
        seed.copy_from_slice(&hash);
        Self::start_with_key(ed25519_dalek::SigningKey::from_bytes(&seed)).await
    }

    async fn start_with_key(signing_key: ed25519_dalek::SigningKey) -> Self {
        let data = tempfile::TempDir::new().expect("temp dir");
        let verifying_key = signing_key.verifying_key();
        let config = deis_store::config::Config {
            listen: "127.0.0.1:0".to_string(),
            data_dir: data.path().to_path_buf(),
            ..deis_store::config::Config::default()
        };
        let listener = deis_store::server::bind(&config).await.expect("bind");
        let addr = listener.local_addr().expect("local addr");
        let (shutdown, rx) = oneshot::channel();
        let identity = fleet().service_identity(deis_identity::ServiceKind::Store);
        tokio::spawn(async move {
            deis_store::server::serve(
                config,
                listener,
                async {
                    let _ = rx.await;
                },
                deis_capability::KeyRing::from_keys([verifying_key]),
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

    pub(crate) fn capability_token_for(&self, tenant: &str, binding: &str) -> String {
        use deis_capability::{encode, CapabilityClaims, CapabilityResource};
        use std::time::Duration;
        let claims = CapabilityClaims::new(
            "store",
            "spiffe://deislet/control/main",
            "spiffe://deislet/node/test",
            1,
            Duration::from_secs(900),
            vec![CapabilityResource {
                tenant: tenant.to_string(),
                binding: binding.to_string(),
                generation: None,
            }],
        );
        encode(&claims, &self.signing_key)
    }

    /// A capability token that covers kv, sql, and blob for a tenant.
    ///
    /// Used by `wiring_for_tenant` so that a single wiring can serve
    /// database, KV, and blob tests alike without a binding-level mismatch.
    pub(crate) fn capability_token_for_all_bindings(&self, tenant: &str) -> String {
        use deis_capability::{encode, CapabilityClaims, CapabilityResource};
        use std::time::Duration;
        let claims = CapabilityClaims::new(
            "store",
            "spiffe://deislet/control/main",
            "spiffe://deislet/node/test",
            1,
            Duration::from_secs(900),
            vec![
                CapabilityResource {
                    tenant: tenant.to_string(),
                    binding: "kv".into(),
                    generation: None,
                },
                CapabilityResource {
                    tenant: tenant.to_string(),
                    binding: "sql".into(),
                    generation: None,
                },
                CapabilityResource {
                    tenant: tenant.to_string(),
                    binding: "blob".into(),
                    generation: None,
                },
            ],
        );
        encode(&claims, &self.signing_key)
    }

    /// Give this node's capability renewer a capability for `tenant`, the
    /// way a renewal would, so any client built for the tenant afterwards
    /// presents it.
    pub(crate) fn hand_node_capability(&self, tenant: &str) {
        let token = self.capability_token_for_all_bindings(tenant);
        crate::deployment::capability_renewer::adopt(vec![
            crate::deployment::capability_renewer::HeldCapability {
                tenant: tenant.to_string(),
                audience: "store".to_string(),
                token,
                exp: std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_secs()
                    + 900,
            },
        ]);
    }

    /// Wiring that declares only KV and points it at this store, minting a
    /// capability token for `tenant`.
    pub(crate) fn kv_wiring_for(&self, tenant: &str) -> ServiceWiring {
        self.wiring_for_tenant(tenant, ["kv"])
    }

    /// Wiring that declares the named services and points them all at this
    /// store, minting a capability token for `tenant`. The queue endpoint
    /// stays empty: deis-queue is a separate process and nothing here speaks
    /// to it.
    pub(crate) fn wiring_for_tenant<'a>(
        &self,
        tenant: &str,
        names: impl IntoIterator<Item = &'a str>,
    ) -> ServiceWiring {
        ServiceWiring {
            storage_token: Some(self.capability_token_for_all_bindings(tenant)),
            declaration: ServiceDeclaration::from_names(names),
            endpoints: ServiceEndpoints {
                store: Some(self.endpoint.clone()),
                queue: None,
            },
            consumers: Vec::new(),
            ..ServiceWiring::default()
        }
    }

    /// The same, with named bindings of one kind declared beside the unnamed
    /// namespace this store already serves.
    ///
    /// Every name becomes a second namespace inside the same tenant, which is
    /// the property the tests below actually check.
    pub(crate) fn wiring_with_bindings_for<'a>(
        &self,
        tenant: &str,
        names: impl IntoIterator<Item = &'a str>,
        kind: deis_bundle::BindingKind,
        bindings: impl IntoIterator<Item = &'a str>,
    ) -> ServiceWiring {
        let mut wiring = self.wiring_for_tenant(tenant, names);
        for binding in bindings {
            wiring
                .bindings
                .insert(binding, deis_bundle::Binding::storage(kind));
        }
        wiring
    }
}

impl Drop for TestStore {
    fn drop(&mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
    }
}
