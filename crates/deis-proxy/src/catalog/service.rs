//! The poll loop, as a pingora background service.
//!
//! One of these exists per process. It is registered with the server, so it
//! is shut down with it and a zero-downtime upgrade replaces it rather than
//! adding another; see the module comment in [`super`] for why the table is
//! polled at all.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use pingora::server::ShutdownWatch;
use pingora::services::background::BackgroundService;
use tokio::time::{MissedTickBehavior, interval};
use tracing::{error, info};

use super::CatalogClient;
use crate::config::ConfigManager;

pub struct CatalogPoller {
    client: CatalogClient,
    config: Arc<ConfigManager>,
    interval: Duration,
}

impl CatalogPoller {
    pub fn new(client: CatalogClient, config: Arc<ConfigManager>, interval: Duration) -> Self {
        Self {
            client,
            config,
            interval,
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
        let authority = self.client.endpoint().authority();
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
}

#[async_trait]
impl BackgroundService for CatalogPoller {
    async fn start(&self, mut shutdown: ShutdownWatch) {
        let authority = self.client.endpoint().authority();
        info!(
            control = %authority,
            path = super::ROUTING_TABLE_PATH,
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
    use crate::catalog::mock::{Answer, MockControl};
    use crate::catalog::{CatalogClient, ControlEndpoint};
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

    fn poller(control: &MockControl, config: Arc<ConfigManager>) -> CatalogPoller {
        let endpoint = ControlEndpoint::parse(&control.addr).unwrap();
        CatalogPoller::new(
            CatalogClient::new(
                endpoint,
                "node-psk".to_string(),
                Duration::from_secs(5),
                1024 * 1024,
            ),
            config,
            Duration::from_secs(5),
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
        let poller = poller(&control, Arc::clone(&config));

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
            control.last_authorization().as_deref(),
            Some("Bearer node-psk"),
            "the routing table is on the node channel, so the node credential has to be presented"
        );
    }

    #[tokio::test]
    async fn an_unreachable_control_plane_leaves_the_last_table_in_force() {
        let mut control = MockControl::start(Answer::Ok(V1.to_string())).await;
        let config = config_manager().await;
        let poller = poller(&control, Arc::clone(&config));
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
        let poller = poller(&control, Arc::clone(&config));
        poller.poll_once(0).await;

        // The second answer declares a full body and sends half of it: the
        // half that arrives is a valid prefix of the table, which is exactly
        // the shape that must never be applied.
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
        let poller = poller(&control, Arc::clone(&config));
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
        // The dangerous reading of 401 would be "the control plane has no
        // routes"; that would silently unroute the whole platform.
        let control = MockControl::start(Answer::Ok(V1.to_string())).await;
        let config = config_manager().await;
        let poller = poller(&control, Arc::clone(&config));
        poller.poll_once(0).await;

        control.answer(Answer::Status(
            401,
            r#"{"error":"Unauthorized"}"#.to_string(),
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
        let endpoint = ControlEndpoint::parse(&control.addr).unwrap();
        let poller = CatalogPoller::new(
            CatalogClient::new(
                endpoint,
                "node-psk".to_string(),
                Duration::from_secs(5),
                // Smaller than the answer, so the ceiling is what refuses it.
                32,
            ),
            Arc::clone(&config),
            Duration::from_secs(5),
        );
        assert_eq!(poller.poll_once(0).await, 1);
        assert!(config.catalog_routes().version().is_empty());
    }

    #[tokio::test]
    async fn the_failure_count_resets_once_the_control_plane_answers() {
        let control = MockControl::start(Answer::Ok(V1.to_string())).await;
        let config = config_manager().await;
        let poller = poller(&control, Arc::clone(&config));
        assert_eq!(poller.poll_once(4).await, 0);
    }
}
