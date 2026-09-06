//! Background execution for durable provider deployment workflows.
//!
//! HTTP handlers create intent; this loop is the process-owned worker that
//! makes that intent progress after the request has returned and after a
//! restart. Every actual provider call still goes through the child
//! operation's durable owner/epoch lease.

use std::{sync::Arc, time::Duration};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use deis_provider_driver::DriverOrchestratorConfig;
use deis_wire::CanonicalId;
use tokio::task::JoinSet;
use tracing::{debug, info, warn};

use super::{
    CatalogResult, CatalogStore, ProviderDeploymentDriveOutcome, ProviderDriverDispatcher,
};
use crate::shutdown::Shutdown;

const MAX_SCAN: usize = 1_000;
const MAX_CONCURRENCY: usize = 64;
const MAX_INTERVAL_SECONDS: u64 = 300;
const MAX_RETRY_DELAY_SECONDS: u64 = 3_600;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ProviderDeploymentReconcilerConfig {
    pub poll_interval: Duration,
    pub retry_delay: Duration,
    pub max_concurrency: usize,
    pub batch_limit: usize,
    pub orchestrator: DriverOrchestratorConfig,
}

impl ProviderDeploymentReconcilerConfig {
    #[must_use]
    pub fn normalized(
        poll_interval_seconds: u64,
        retry_delay_seconds: u64,
        max_concurrency: usize,
    ) -> Self {
        let poll_interval_seconds = poll_interval_seconds.clamp(1, MAX_INTERVAL_SECONDS);
        let retry_delay_seconds =
            retry_delay_seconds.clamp(poll_interval_seconds, MAX_RETRY_DELAY_SECONDS);
        let max_concurrency = max_concurrency.clamp(1, MAX_CONCURRENCY);
        Self {
            poll_interval: Duration::from_secs(poll_interval_seconds),
            retry_delay: Duration::from_secs(retry_delay_seconds),
            max_concurrency,
            batch_limit: max_concurrency.saturating_mul(4).min(MAX_SCAN),
            orchestrator: DriverOrchestratorConfig::default(),
        }
    }
}

#[derive(Clone)]
pub struct ProviderDeploymentReconciler {
    executor: Arc<dyn ProviderDeploymentExecutor>,
    config: ProviderDeploymentReconcilerConfig,
}

impl ProviderDeploymentReconciler {
    #[must_use]
    pub fn new(
        store: Arc<CatalogStore>,
        dispatcher: Arc<ProviderDriverDispatcher>,
        poll_interval_seconds: u64,
        retry_delay_seconds: u64,
        max_concurrency: usize,
    ) -> Self {
        let config = ProviderDeploymentReconcilerConfig::normalized(
            poll_interval_seconds,
            retry_delay_seconds,
            max_concurrency,
        );
        if config.poll_interval.as_secs() != poll_interval_seconds
            || config.retry_delay.as_secs() != retry_delay_seconds
            || config.max_concurrency != max_concurrency
        {
            warn!(
                requested_poll_interval_seconds = poll_interval_seconds,
                poll_interval_seconds = config.poll_interval.as_secs(),
                requested_retry_delay_seconds = retry_delay_seconds,
                retry_delay_seconds = config.retry_delay.as_secs(),
                requested_max_concurrency = max_concurrency,
                max_concurrency = config.max_concurrency,
                "Provider deployment dispatcher settings were outside safe bounds and were clamped"
            );
        }
        Self {
            executor: Arc::new(CatalogProviderDeploymentExecutor {
                store,
                dispatcher,
                orchestrator: config.orchestrator,
            }),
            config,
        }
    }

    /// Run until shutdown. Once a pass has claimed work, shutdown stops new
    /// passes but lets the current provider calls settle. The process-level
    /// drain deadline is the final bound; if it cuts the task, expired child
    /// leases force the replacement process to reconcile before retrying.
    pub async fn run(self, shutdown: Shutdown) {
        info!(
            poll_interval_seconds = self.config.poll_interval.as_secs(),
            retry_delay_seconds = self.config.retry_delay.as_secs(),
            max_concurrency = self.config.max_concurrency,
            batch_limit = self.config.batch_limit,
            "Provider deployment background dispatcher started"
        );
        let mut interval = tokio::time::interval(self.config.poll_interval);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                _ = shutdown.wait() => break,
                _ = interval.tick() => {}
            }
            if shutdown.is_triggered() {
                break;
            }
            match self.dispatch_once().await {
                Ok(summary) if summary.attempted > 0 || summary.errors > 0 => debug!(
                    discovered = summary.discovered,
                    attempted = summary.attempted,
                    busy = summary.busy,
                    advanced = summary.advanced,
                    waiting = summary.waiting,
                    succeeded = summary.succeeded,
                    failed = summary.failed,
                    errors = summary.errors,
                    "Provider deployment dispatch pass finished"
                ),
                Ok(_) => {}
                Err(error) => warn!(
                    %error,
                    "Could not enumerate provider deployments; durable work remains queued for the next pass"
                ),
            }
        }
        info!("Provider deployment background dispatcher stopped");
    }

    async fn dispatch_once(&self) -> CatalogResult<DispatchPassSummary> {
        let operation_ids = self
            .executor
            .ready_operation_ids(Utc::now(), self.config.retry_delay, self.config.batch_limit)
            .await?;
        let mut summary = DispatchPassSummary {
            discovered: operation_ids.len(),
            ..DispatchPassSummary::default()
        };
        let mut operation_ids = operation_ids.into_iter();
        let mut running = JoinSet::new();
        for _ in 0..self.config.max_concurrency {
            let Some(operation_id) = operation_ids.next() else {
                break;
            };
            spawn_dispatch(&mut running, Arc::clone(&self.executor), operation_id);
        }
        while let Some(joined) = running.join_next().await {
            summary.attempted += 1;
            match joined {
                Ok((operation_id, Ok(outcome))) => {
                    summary.record(outcome);
                    match outcome {
                        DispatchOutcome::Succeeded => info!(
                            %operation_id,
                            "Provider deployment reached its durable terminal success state"
                        ),
                        DispatchOutcome::Failed => warn!(
                            %operation_id,
                            "Provider deployment reached its durable terminal failure state"
                        ),
                        DispatchOutcome::Busy
                        | DispatchOutcome::Advanced
                        | DispatchOutcome::Waiting => {}
                    }
                }
                Ok((operation_id, Err(error))) => {
                    summary.errors += 1;
                    warn!(
                        %operation_id,
                        %error,
                        "Provider deployment dispatch attempt did not make a durable transition; it will be reconsidered by a later pass"
                    );
                }
                Err(error) => {
                    summary.errors += 1;
                    warn!(
                        %error,
                        "Provider deployment worker task failed before reporting its durable outcome"
                    );
                }
            }
            if let Some(operation_id) = operation_ids.next() {
                spawn_dispatch(&mut running, Arc::clone(&self.executor), operation_id);
            }
        }
        Ok(summary)
    }

    #[cfg(test)]
    fn for_test(
        executor: Arc<dyn ProviderDeploymentExecutor>,
        config: ProviderDeploymentReconcilerConfig,
    ) -> Self {
        Self { executor, config }
    }
}

fn spawn_dispatch(
    running: &mut JoinSet<(CanonicalId, CatalogResult<DispatchOutcome>)>,
    executor: Arc<dyn ProviderDeploymentExecutor>,
    operation_id: CanonicalId,
) {
    running.spawn(async move {
        let owner_id = CanonicalId::new_v7();
        let outcome = executor.drive_once(operation_id, owner_id).await;
        (operation_id, outcome)
    });
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DispatchOutcome {
    Busy,
    Advanced,
    Waiting,
    Succeeded,
    Failed,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct DispatchPassSummary {
    discovered: usize,
    attempted: usize,
    busy: usize,
    advanced: usize,
    waiting: usize,
    succeeded: usize,
    failed: usize,
    errors: usize,
}

impl DispatchPassSummary {
    fn record(&mut self, outcome: DispatchOutcome) {
        match outcome {
            DispatchOutcome::Busy => self.busy += 1,
            DispatchOutcome::Advanced => self.advanced += 1,
            DispatchOutcome::Waiting => self.waiting += 1,
            DispatchOutcome::Succeeded => self.succeeded += 1,
            DispatchOutcome::Failed => self.failed += 1,
        }
    }
}

#[async_trait]
trait ProviderDeploymentExecutor: Send + Sync {
    async fn ready_operation_ids(
        &self,
        now: DateTime<Utc>,
        retry_delay: Duration,
        limit: usize,
    ) -> CatalogResult<Vec<CanonicalId>>;

    async fn drive_once(
        &self,
        operation_id: CanonicalId,
        owner_id: CanonicalId,
    ) -> CatalogResult<DispatchOutcome>;
}

struct CatalogProviderDeploymentExecutor {
    store: Arc<CatalogStore>,
    dispatcher: Arc<ProviderDriverDispatcher>,
    orchestrator: DriverOrchestratorConfig,
}

#[async_trait]
impl ProviderDeploymentExecutor for CatalogProviderDeploymentExecutor {
    async fn ready_operation_ids(
        &self,
        now: DateTime<Utc>,
        retry_delay: Duration,
        limit: usize,
    ) -> CatalogResult<Vec<CanonicalId>> {
        self.store
            .provider_deployments_ready_for_dispatch(now, retry_delay, limit)
            .await
    }

    async fn drive_once(
        &self,
        operation_id: CanonicalId,
        owner_id: CanonicalId,
    ) -> CatalogResult<DispatchOutcome> {
        Ok(
            match self
                .store
                .drive_provider_deployment_once(
                    operation_id,
                    owner_id,
                    &self.dispatcher,
                    self.orchestrator,
                )
                .await?
            {
                ProviderDeploymentDriveOutcome::Busy(_) => DispatchOutcome::Busy,
                ProviderDeploymentDriveOutcome::Advanced(_) => DispatchOutcome::Advanced,
                ProviderDeploymentDriveOutcome::Waiting(_) => DispatchOutcome::Waiting,
                ProviderDeploymentDriveOutcome::Succeeded(_) => DispatchOutcome::Succeeded,
                ProviderDeploymentDriveOutcome::Failed(_) => DispatchOutcome::Failed,
            },
        )
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::HashSet,
        sync::{
            Mutex,
            atomic::{AtomicUsize, Ordering},
        },
    };

    use super::*;

    struct FakeExecutor {
        operations: Vec<CanonicalId>,
        owners: Mutex<HashSet<CanonicalId>>,
        active: AtomicUsize,
        peak: AtomicUsize,
    }

    #[async_trait]
    impl ProviderDeploymentExecutor for FakeExecutor {
        async fn ready_operation_ids(
            &self,
            _now: DateTime<Utc>,
            _retry_delay: Duration,
            limit: usize,
        ) -> CatalogResult<Vec<CanonicalId>> {
            Ok(self.operations.iter().copied().take(limit).collect())
        }

        async fn drive_once(
            &self,
            _operation_id: CanonicalId,
            owner_id: CanonicalId,
        ) -> CatalogResult<DispatchOutcome> {
            self.owners.lock().unwrap().insert(owner_id);
            let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
            self.peak.fetch_max(active, Ordering::SeqCst);
            tokio::time::sleep(Duration::from_millis(10)).await;
            self.active.fetch_sub(1, Ordering::SeqCst);
            Ok(DispatchOutcome::Advanced)
        }
    }

    #[tokio::test]
    async fn dispatch_pass_is_bounded_and_uses_one_owner_per_attempt() {
        let executor = Arc::new(FakeExecutor {
            operations: (0..5).map(|_| CanonicalId::new_v7()).collect(),
            owners: Mutex::new(HashSet::new()),
            active: AtomicUsize::new(0),
            peak: AtomicUsize::new(0),
        });
        let config = ProviderDeploymentReconcilerConfig {
            poll_interval: Duration::from_secs(1),
            retry_delay: Duration::from_secs(30),
            max_concurrency: 2,
            batch_limit: 5,
            orchestrator: DriverOrchestratorConfig::default(),
        };
        let reconciler = ProviderDeploymentReconciler::for_test(executor.clone(), config);
        let summary = reconciler.dispatch_once().await.unwrap();
        assert_eq!(summary.discovered, 5);
        assert_eq!(summary.attempted, 5);
        assert_eq!(summary.advanced, 5);
        assert_eq!(executor.peak.load(Ordering::SeqCst), 2);
        assert_eq!(executor.owners.lock().unwrap().len(), 5);
    }

    #[tokio::test]
    async fn a_pretriggered_shutdown_starts_no_dispatch_pass() {
        let executor = Arc::new(FakeExecutor {
            operations: vec![CanonicalId::new_v7()],
            owners: Mutex::new(HashSet::new()),
            active: AtomicUsize::new(0),
            peak: AtomicUsize::new(0),
        });
        let reconciler = ProviderDeploymentReconciler::for_test(
            executor.clone(),
            ProviderDeploymentReconcilerConfig::normalized(1, 30, 1),
        );
        let shutdown = Shutdown::new();
        shutdown.trigger();
        reconciler.run(shutdown).await;
        assert!(executor.owners.lock().unwrap().is_empty());
    }

    #[test]
    fn unsafe_settings_are_normalized_to_documented_bounds() {
        let low = ProviderDeploymentReconcilerConfig::normalized(0, 0, 0);
        assert_eq!(low.poll_interval, Duration::from_secs(1));
        assert_eq!(low.retry_delay, Duration::from_secs(1));
        assert_eq!(low.max_concurrency, 1);
        assert_eq!(low.batch_limit, 4);

        let high = ProviderDeploymentReconcilerConfig::normalized(u64::MAX, u64::MAX, usize::MAX);
        assert_eq!(high.poll_interval, Duration::from_secs(300));
        assert_eq!(high.retry_delay, Duration::from_secs(3_600));
        assert_eq!(high.max_concurrency, 64);
        assert_eq!(high.batch_limit, 256);
    }
}
