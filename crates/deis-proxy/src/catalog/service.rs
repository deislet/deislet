//! The poll loop, as a pingora background service.
//!
//! One of these exists per process. It is registered with the server, so it
//! is shut down with it and a zero-downtime upgrade replaces it rather than
//! adding another; see the module comment in [`super`] for why the table is
//! polled at all.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use pingora::server::ShutdownWatch;
use pingora::services::background::BackgroundService;
use tokio::time::{MissedTickBehavior, interval};
use tracing::{error, info, warn};

use super::CatalogClient;
use super::persist::RoutingTableCache;
use super::wire::RoutingTableDocument;
use crate::config::ConfigManager;

pub struct CatalogPoller {
    client: CatalogClient,
    config: Arc<ConfigManager>,
    interval: Duration,
    /// The file every published table is kept in, or nothing when the caller
    /// asked for none. A real configuration always names a path
    /// ([`crate::SystemConfig::routing_cache_path`]); a test that is about
    /// the poll and not about the file leaves it out.
    cache: Option<RoutingTableCache>,
}

impl CatalogPoller {
    pub fn new(
        client: CatalogClient,
        config: Arc<ConfigManager>,
        interval: Duration,
        cache_path: Option<PathBuf>,
    ) -> Self {
        Self {
            client,
            config,
            interval,
            cache: cache_path.map(RoutingTableCache::new),
        }
    }

    /// One fetch and, if it worked, one publish.
    ///
    /// `failures_so_far` is how many polls in a row have already failed;
    /// the answer is the same count updated, which is what the caller
    /// carries into the next poll. Nothing else survives a poll: a failure
    /// changes no routing state at all, which is the whole contract of this
    /// module - the table that was in force a moment ago is still in force.
    async fn poll_once(&self, failures_so_far: u64) -> u64 {
        let authority = self.client.authority();
        match self.client.fetch().await {
            Ok(document) => {
                if failures_so_far > 0 {
                    info!(
                        control = %authority,
                        after_failures = failures_so_far,
                        "the control plane is answering again; the routing table is current"
                    );
                }
                self.config.apply_routing_table(&document);
                self.client.observe_routing_table(document.version.clone());
                self.persist(&document);
                0
            }
            Err(e) => {
                let failures = failures_so_far.saturating_add(1);
                let current = self.config.catalog_routes();
                if current.version().is_empty() {
                    error!(
                        control = %authority,
                        error = %e,
                        consecutive_failures = failures,
                        retry_in_secs = self.interval.as_secs(),
                        "could not fetch the routing table, and none has ever been fetched, so no \
                         catalog route is in force at all: requests are matched only against the \
                         applications configured on this machine, and every route created through \
                         the control plane is unreachable until this succeeds"
                    );
                } else {
                    error!(
                        control = %authority,
                        error = %e,
                        consecutive_failures = failures,
                        retry_in_secs = self.interval.as_secs(),
                        in_force_version = %current.version(),
                        in_force_built_at = current.generated_at(),
                        in_force_routes = current.route_count(),
                        "could not fetch the routing table; the previous one is still in force and \
                         traffic is unaffected. Route changes made in the control plane since it \
                         was fetched have not reached this instance"
                    );
                }
                failures
            }
        }
    }

    /// Put the table this instance last published back in force.
    ///
    /// Called once before the first poll, which is what a proxy restarted
    /// while the control plane is unreachable has instead of a fetch: without
    /// it, a restart an hour into an outage would leave every route created
    /// through the control plane unroutable until the control plane came
    /// back. The file is not trusted over a live answer - the first
    /// successful poll replaces the table and the file together.
    fn restore_persisted_table(&self) {
        let Some(cache) = &self.cache else {
            return;
        };
        let Some(stored) = cache.load() else {
            return;
        };
        self.config.apply_routing_table(&stored.document);
        info!(
            path = %cache.path().display(),
            generation = stored.generation,
            version = %stored.document.version,
            built_at = stored.document.generated_at,
            written_at = stored.written_at,
            routes = stored.document.routes.len(),
            environments = stored.document.environments.len(),
            "the routing table this instance last published was loaded from disk and is in force; \
             a control plane that does not answer leaves it routing, and the next successful poll \
             replaces it"
        );
    }

    /// Keep the table just published for the next start.
    ///
    /// A failure is logged and otherwise ignored: routing is unaffected, and
    /// the only thing lost is the case this exists for - a restart before
    /// the control plane answers again.
    fn persist(&self, document: &RoutingTableDocument) {
        let Some(cache) = &self.cache else {
            return;
        };
        if let Err(error) = cache.persist(document) {
            warn!(
                path = %cache.path().display(),
                %error,
                "the routing table could not be persisted; a restart before the control plane \
                 answers again would leave every route it created unroutable"
            );
        }
    }
}

#[async_trait]
impl BackgroundService for CatalogPoller {
    async fn start(&self, mut shutdown: ShutdownWatch) {
        let authority = self.client.authority();
        // The transport is logged because it is the one thing about this loop
        // an operator cannot see from anywhere else, and it is the thing they
        // have just changed: a `[machine_identity]` table that did not take
        // effect looks exactly like one that did until this line says which
        // channel the table is coming over.
        info!(
            control = %authority,
            transport = self.client.transport(),
            path = self.client.path(),
            interval_secs = self.interval.as_secs(),
            "polling the control plane for the routing table; the last table accepted stays in \
             force whenever a poll fails"
        );

        let mut ticker = interval(self.interval);
        // The first tick fires immediately, so the table is fetched at
        // startup rather than one interval later. Delay rather than Burst: a
        // poll that took longer than the interval must not be followed by a
        // burst of catch-up polls at a control plane that is already slow.
        ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);

        self.restore_persisted_table();

        let mut failures: u64 = 0;
        loop {
            // The fetch is inside the select so that shutdown cancels it,
            // rather than the process waiting out a request timeout against
            // a control plane that has stopped answering.
            failures = tokio::select! {
                failures = async {
                    ticker.tick().await;
                    self.poll_once(failures).await
                } => failures,
                _ = shutdown.changed() => {
                    info!(
                        control = %authority,
                        "server shutting down; stopped polling for the routing table"
                    );
                    return;
                }
            };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::CatalogClient;
    use crate::catalog::mock::{Answer, MockControl};
    use crate::catalog::persist::RoutingTableCache;
    use crate::config::system::RoutingConfig;
    use crate::config::{ConfigManager, SystemConfig};

    const V1: &str = r#"{"version":"v1","generated_at":11,"routes":[
        {"app_id":"shop","environment":"production","host":"shop.test","path_prefix":"/","priority":0}
    ],"environments":[]}"#;

    const V2: &str = r#"{"version":"v2","generated_at":12,"routes":[
        {"app_id":"shop","environment":"pr-7","host":"shop.test","path_prefix":"/api","priority":0},
        {"app_id":"shop","environment":"production","host":"shop.test","path_prefix":"/","priority":0}
    ],"environments":[]}"#;

    async fn config_manager() -> Arc<ConfigManager> {
        let mut system = SystemConfig::baseline();
        system.database = Some(":memory:".to_string());
        system.routing = Some(RoutingConfig::default());
        Arc::new(ConfigManager::with_system_config(system).await.unwrap())
    }

    /// A poller dialling `control` as the proxy `edge-1`, over the same
    /// mutual TLS handshake a real proxy's machine channel uses: a
    /// certificate `control` minted from its own root, naming this proxy and
    /// nothing else. `cache_path` is where it keeps the tables it publishes,
    /// or nothing when the test is about the poll and not about the file.
    fn poller(
        control: &MockControl,
        config: Arc<ConfigManager>,
        cache_path: Option<PathBuf>,
    ) -> CatalogPoller {
        CatalogPoller::new(
            CatalogClient::over_machine_channel(
                control.addr.clone(),
                control.issue_proxy("edge-1"),
                control.control_identity(),
                Duration::from_secs(5),
                1024 * 1024,
            )
            .with_proxy_id("edge-1".to_string()),
            config,
            Duration::from_secs(5),
            cache_path,
        )
    }

    fn app_for(config: &ConfigManager, host: &str, path: &str) -> Option<String> {
        config
            .catalog_routes()
            .match_request(host, path)
            .map(|found| format!("{}/{}", found.target.app_id, found.target.environment))
    }

    #[tokio::test]
    async fn a_route_created_in_the_control_plane_reaches_the_data_plane() {
        let control = MockControl::start(Answer::Ok(V2.to_string())).await;
        let config = config_manager().await;
        let poller = poller(&control, Arc::clone(&config), None);

        assert_eq!(poller.poll_once(0).await, 0);
        assert_eq!(
            app_for(&config, "shop.test", "/api/orders").as_deref(),
            Some("shop/pr-7"),
            "a path-prefix route from the catalog has to be the thing that decides"
        );
        assert_eq!(
            app_for(&config, "shop.test", "/").as_deref(),
            Some("shop/production")
        );
        assert_eq!(
            control.last_request().proxy_id,
            "edge-1",
            "the routing table is on the machine channel, authenticated by the certificate this \
             proxy proved during the handshake rather than a shared credential; the id it asks \
             for still has to arrive on the request"
        );
    }

    #[tokio::test]
    async fn a_proxy_reports_only_a_table_it_has_already_published() {
        let control = MockControl::start(Answer::Ok(V1.to_string())).await;
        let config = config_manager().await;
        let poller = CatalogPoller::new(
            CatalogClient::over_machine_channel(
                control.addr.clone(),
                control.issue_proxy("proxy-a"),
                control.control_identity(),
                Duration::from_secs(5),
                1024 * 1024,
            )
            .with_proxy_id("proxy-a".to_string()),
            Arc::clone(&config),
            Duration::from_secs(5),
            None,
        );

        assert_eq!(poller.poll_once(0).await, 0);
        assert_eq!(control.last_request().proxy_id, "proxy-a");
        assert!(
            control
                .last_request()
                .observed_routing_table_version
                .is_empty(),
            "receipt of the first response is not yet an applied observation"
        );

        assert_eq!(poller.poll_once(0).await, 0);
        assert_eq!(
            control.last_request().observed_routing_table_version,
            "v1"
        );
    }

    #[tokio::test]
    async fn an_unreachable_control_plane_leaves_the_last_table_in_force() {
        let mut control = MockControl::start(Answer::Ok(V1.to_string())).await;
        let config = config_manager().await;
        let poller = poller(&control, Arc::clone(&config), None);
        assert_eq!(poller.poll_once(0).await, 0);
        assert_eq!(
            app_for(&config, "shop.test", "/").as_deref(),
            Some("shop/production")
        );

        control.stop().await;

        // Three failed polls in a row change nothing about what is served.
        let mut failures = 0;
        for expected in 1..=3 {
            failures = poller.poll_once(failures).await;
            assert_eq!(failures, expected);
            assert_eq!(
                app_for(&config, "shop.test", "/").as_deref(),
                Some("shop/production"),
                "a data plane that dropped its routing table because the control plane restarted \
                 would turn a management outage into a customer-visible one"
            );
            assert_eq!(config.catalog_routes().version(), "v1");
        }
    }

    #[tokio::test]
    async fn a_table_cut_short_is_refused_whole() {
        let control = MockControl::start(Answer::Ok(V1.to_string())).await;
        let config = config_manager().await;
        let poller = poller(&control, Arc::clone(&config), None);
        poller.poll_once(0).await;

        // The second answer's routing table document is cut off partway
        // through: what arrives is a valid prefix of the table's text, which
        // is exactly the shape that must never be applied.
        control.answer(Answer::CutShort(V2.to_string(), V2.len() / 2));
        assert_eq!(poller.poll_once(0).await, 1);
        assert_eq!(config.catalog_routes().version(), "v1");
        assert_eq!(
            app_for(&config, "shop.test", "/api/orders").as_deref(),
            Some("shop/production"),
            "none of the half-delivered table may be in force"
        );
    }

    #[tokio::test]
    async fn one_unusable_entry_refuses_the_whole_table() {
        let control = MockControl::start(Answer::Ok(V1.to_string())).await;
        let config = config_manager().await;
        let poller = poller(&control, Arc::clone(&config), None);
        poller.poll_once(0).await;

        // The first entry is perfectly good; the second is not. Neither is
        // applied.
        control.answer(Answer::Ok(
            r#"{"version":"v3","generated_at":13,"routes":[
                {"app_id":"good","environment":"production","host":"shop.test","path_prefix":"/","priority":0},
                {"app_id":"bad","environment":"pro duction","host":"other.test","path_prefix":"/","priority":0}
            ],"environments":[]}"#
                .to_string(),
        ));
        assert_eq!(poller.poll_once(0).await, 1);
        assert_eq!(config.catalog_routes().version(), "v1");
        assert_eq!(
            app_for(&config, "shop.test", "/").as_deref(),
            Some("shop/production")
        );
    }

    #[tokio::test]
    async fn a_refused_credential_is_a_failure_and_not_an_empty_table() {
        // The dangerous reading of a refusal would be "the control plane has
        // no routes"; that would silently unroute the whole platform. What a
        // refusal looks like now is the machine channel answering
        // PermissionDenied - what the control plane sends a proxy asking for
        // a table under a name its connection did not prove.
        let control = MockControl::start(Answer::Ok(V1.to_string())).await;
        let config = config_manager().await;
        let poller = poller(&control, Arc::clone(&config), None);
        poller.poll_once(0).await;

        control.answer(Answer::Denied(
            "this connection proved a different proxy".to_string(),
        ));
        assert_eq!(poller.poll_once(0).await, 1);
        assert_eq!(config.catalog_routes().version(), "v1");
        assert_eq!(
            app_for(&config, "shop.test", "/").as_deref(),
            Some("shop/production")
        );
    }

    #[tokio::test]
    async fn a_table_larger_than_the_ceiling_is_refused() {
        let control = MockControl::start(Answer::Ok(V1.to_string())).await;
        let config = config_manager().await;
        let poller = CatalogPoller::new(
            CatalogClient::over_machine_channel(
                control.addr.clone(),
                control.issue_proxy("edge-1"),
                control.control_identity(),
                Duration::from_secs(5),
                // Smaller than the answer, so the ceiling is what refuses it.
                32,
            ),
            Arc::clone(&config),
            Duration::from_secs(5),
            None,
        );
        assert_eq!(poller.poll_once(0).await, 1);
        assert!(config.catalog_routes().version().is_empty());
    }

    #[tokio::test]
    async fn the_failure_count_resets_once_the_control_plane_answers() {
        let control = MockControl::start(Answer::Ok(V1.to_string())).await;
        let config = config_manager().await;
        let poller = poller(&control, Arc::clone(&config), None);
        assert_eq!(poller.poll_once(4).await, 0);
    }

    #[tokio::test]
    async fn a_restart_during_a_control_plane_outage_keeps_routing() {
        // The case the persisted copy exists for: the control plane is down
        // and the edge comes back up, so nothing can be fetched and what it
        // routes on is what it wrote before it stopped.
        let mut control = MockControl::start(Answer::Ok(V1.to_string())).await;
        let directory = tempfile::tempdir().expect("a directory");
        let path = directory.path().join("routing-table.json");

        let before = config_manager().await;
        let first = poller(&control, Arc::clone(&before), Some(path.clone()));
        assert_eq!(first.poll_once(0).await, 0);
        assert_eq!(before.catalog_routes().version(), "v1");
        drop(first);

        control.stop().await;

        // A new process: a manager with nothing in force, a poller, and the
        // same file. What `start` does first is load it.
        let after = config_manager().await;
        assert!(after.catalog_routes().version().is_empty());
        let restarted = poller(&control, Arc::clone(&after), Some(path));
        restarted.restore_persisted_table();
        assert_eq!(after.catalog_routes().version(), "v1");
        assert_eq!(
            app_for(&after, "shop.test", "/").as_deref(),
            Some("shop/production")
        );

        // And the failures that follow - every poll for as long as the
        // control plane is down - change nothing about that.
        assert_eq!(restarted.poll_once(0).await, 1);
        assert_eq!(
            app_for(&after, "shop.test", "/").as_deref(),
            Some("shop/production")
        );
    }

    #[tokio::test]
    async fn a_live_table_replaces_the_one_on_disk() {
        // The stored copy is never a second source of truth: a poll that
        // brings a newer table is in force immediately and on disk a moment
        // later, so a restart an hour afterwards starts from the newest table
        // rather than from whichever one the file happened to hold.
        let control = MockControl::start(Answer::Ok(V1.to_string())).await;
        let directory = tempfile::tempdir().expect("a directory");
        let path = directory.path().join("routing-table.json");
        let config = config_manager().await;
        let polling = poller(&control, Arc::clone(&config), Some(path.clone()));
        assert_eq!(polling.poll_once(0).await, 0);

        control.answer(Answer::Ok(V2.to_string()));
        assert_eq!(polling.poll_once(0).await, 0);
        assert_eq!(config.catalog_routes().version(), "v2");

        let stored = RoutingTableCache::new(path)
            .load()
            .expect("the table just polled");
        assert_eq!(stored.document.version, "v2");
        assert_eq!(stored.document.routes.len(), 2);
        assert_eq!(
            stored.generation, 2,
            "the second table this instance published is the second write"
        );
    }

    #[tokio::test]
    async fn a_corrupt_persisted_table_starts_empty_and_the_poll_still_works() {
        let control = MockControl::start(Answer::Ok(V1.to_string())).await;
        let directory = tempfile::tempdir().expect("a directory");
        let path = directory.path().join("routing-table.json");
        std::fs::write(&path, b"{ this is not a routing table").expect("a corrupt file");

        let config = config_manager().await;
        let polling = poller(&control, Arc::clone(&config), Some(path.clone()));
        polling.restore_persisted_table();
        assert!(
            config.catalog_routes().version().is_empty(),
            "a file that cannot be read is not a routing table"
        );

        assert_eq!(polling.poll_once(0).await, 0);
        assert_eq!(
            app_for(&config, "shop.test", "/").as_deref(),
            Some("shop/production")
        );
        assert_eq!(
            RoutingTableCache::new(path)
                .load()
                .expect("a table")
                .document
                .version,
            "v1",
            "the live answer is what ends up on disk"
        );
    }
}
