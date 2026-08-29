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

/// A running deis-store, stopped when the value is dropped.
pub(crate) struct TestStore {
    _data: tempfile::TempDir,
    pub(crate) endpoint: String,
    shutdown: Option<oneshot::Sender<()>>,
}

impl TestStore {
    pub(crate) async fn start() -> Self {
        let data = tempfile::TempDir::new().expect("temp dir");
        let config = deis_store::config::Config {
            listen: "127.0.0.1:0".to_string(),
            data_dir: data.path().to_path_buf(),
            ..deis_store::config::Config::default()
        };
        let listener = deis_store::server::bind(&config).await.expect("bind");
        let addr = listener.local_addr().expect("local addr");
        let (shutdown, rx) = oneshot::channel();
        tokio::spawn(async move {
            deis_store::server::serve(config, listener, async {
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

    /// The reserved env entries that carry KV wiring to an isolate.
    ///
    /// `ApplicationManager` has no wiring parameter: the endpoints ride in the
    /// deployment's env map under reserved keys and `RuntimePool` lifts them
    /// out (`ServiceWiring::take_from_env`). A test that goes through the
    /// manager therefore has to write them the same way a real deployment
    /// does, which is what `write_to_env` is for.
    pub(crate) fn kv_env(&self) -> std::collections::HashMap<String, String> {
        let mut env = std::collections::HashMap::new();
        self.kv_wiring().write_to_env(&mut env);
        env
    }

    /// Wiring that declares only KV and points it at this store.
    pub(crate) fn kv_wiring(&self) -> ServiceWiring {
        self.wiring_for(["kv"])
    }

    /// Wiring that declares the named services and points them all at this
    /// store. The queue endpoint stays empty: deis-queue is a separate
    /// process and nothing here speaks to it.
    pub(crate) fn wiring_for<'a>(&self, names: impl IntoIterator<Item = &'a str>) -> ServiceWiring {
        ServiceWiring {
            declaration: ServiceDeclaration::from_names(names),
            endpoints: ServiceEndpoints {
                store: Some(self.endpoint.clone()),
                queue: None,
            },
            consumers: Vec::new(),
        }
    }
}

impl Drop for TestStore {
    fn drop(&mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
    }
}
