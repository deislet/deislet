//! Process wiring: listeners, the retention sweeper, and orderly shutdown.
//!
//! Both the binary and the integration tests come through here, so what the
//! tests exercise is the same assembly the binary runs.
//!
//! # There is one door per surface, not four
//!
//! The capability token is checked by an interceptor applied in [`guarded`],
//! which is the only way a gRPC service is handed to the server below, and by
//! one layer over the scrape router in [`crate::http`]. A handler therefore
//! cannot forget the check, and a fourth service registered later cannot
//! become a fourth unguarded entrance without deleting the call. The health
//! service is registered without it on purpose - it names no application and
//! reads no data. See [`crate::auth`].
//!
//! # The health status is not a constant
//!
//! It was one: `SERVING` for all three services, set once at startup and never
//! touched again. That made it a liveness probe wearing a health probe's
//! name, and a process whose database had filled its disk answered every
//! check with `SERVING` while refusing every log line it was handed.
//! [`health_follows_the_store`] now keeps it on what the database is actually
//! doing, woken by [`crate::stats::StoreHealth`].

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use tokio::net::TcpListener;
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio::time::MissedTickBehavior;
use tokio_stream::wrappers::TcpListenerStream;
use tonic::service::interceptor::InterceptedService;
use tonic::transport::Server;
use tracing::{error, info, warn};

use crate::auth::TelemetryAuth;
use crate::config::Config;
use crate::http::{self, MetricsState};
use crate::proto::log_service_server::LogServiceServer;
use crate::proto::metric_service_server::MetricServiceServer;
use crate::proto::trace_service_server::TraceServiceServer;
use crate::service::{LogService, MetricService, QueryLimits, TraceService};
use crate::stats::{Access, ServiceStats, StoreState};
use crate::store::TelemetryStore;
use crate::tail::{TailHub, TraceHub};
use crate::{MAX_GRPC_MESSAGE_BYTES, now_us};

/// Put a service behind the capability check.
///
/// Every service that can read or write telemetry goes through here, so the
/// check is spelled once and applies to all of them - including any added
/// later, which is the point of the helper existing rather than three copies
/// of the same wrapping.
fn guarded<S>(service: S, auth: &TelemetryAuth) -> InterceptedService<S, TelemetryAuth> {
    InterceptedService::new(service, auth.clone())
}

/// A started service: both listeners are already bound, so `grpc_addr` and
/// `metrics_addr` are the real addresses even when the configuration asked
/// for port 0.
pub struct RunningServer {
    pub grpc_addr: SocketAddr,
    pub metrics_addr: SocketAddr,
    pub store: TelemetryStore,
    pub stats: Arc<ServiceStats>,
    hub: Arc<TailHub>,
    trace_hub: Arc<TraceHub>,
    shutdown: watch::Sender<bool>,
    grpc: JoinHandle<()>,
    metrics: JoinHandle<()>,
    retention: JoinHandle<()>,
    health: JoinHandle<()>,
}

impl RunningServer {
    /// Ask every task to stop and wait for them.
    pub async fn shutdown(self) -> Result<()> {
        info!("deis-telemetry shutting down, draining in-flight requests");
        // Follow streams have no natural end, so they are closed first.
        // Otherwise the gRPC server would wait for them and the process would
        // never stop while anyone is following logs or traces.
        self.hub.close();
        self.trace_hub.close();
        // A send failure only means every receiver is already gone, which is
        // the state we are trying to reach anyway.
        let _ = self.shutdown.send(true);
        for (name, handle) in [
            ("grpc", self.grpc),
            ("metrics-http", self.metrics),
            ("retention", self.retention),
            ("health", self.health),
        ] {
            if let Err(e) = handle.await {
                warn!(task = name, error = %e, "task did not stop cleanly; shutdown continues");
            }
        }
        info!("deis-telemetry stopped");
        Ok(())
    }

    /// Run until the process is asked to stop, then shut down.
    pub async fn wait_for_signal(self) -> Result<()> {
        shutdown_signal().await;
        self.shutdown().await
    }
}

/// Resolve when the process is asked to stop.
///
/// Both signals are honoured, because both are used: an operator pressing
/// Ctrl-C sends SIGINT, while systemd, Docker and Kubernetes stop a container
/// with SIGTERM. Listening for SIGINT alone would leave this service killed
/// outright by every supervisor there is, skipping the shutdown below, so
/// open tail streams would be cut without notice and the last accepted batch
/// might never reach the database.
async fn shutdown_signal() {
    let mut terminate = match signal(SignalKind::terminate()) {
        Ok(stream) => Some(stream),
        Err(e) => {
            error!(
                error = %e,
                "cannot listen for SIGTERM; a supervisor stopping this service will kill it \
                 outright, cutting tail streams and dropping whatever was still being written"
            );
            None
        }
    };

    match terminate.as_mut() {
        Some(terminate) => {
            tokio::select! {
                result = tokio::signal::ctrl_c() => report_interrupt(result),
                _ = terminate.recv() => info!("received SIGTERM, starting shutdown"),
            }
        }
        None => report_interrupt(tokio::signal::ctrl_c().await),
    }
}

fn report_interrupt(result: std::io::Result<()>) {
    match result {
        Ok(()) => info!("received an interrupt, starting shutdown"),
        Err(e) => error!(error = %e, "cannot listen for an interrupt, shutting down now"),
    }
}

/// Start every part of the service against `config`.
pub async fn start(config: &Config) -> Result<RunningServer> {
    config.validate()?;
    let store = TelemetryStore::open(&config.data_dir)?;
    let stats = Arc::new(ServiceStats::new());
    let hub = Arc::new(TailHub::new(config.tail_capacity, Arc::clone(&stats)));
    let trace_hub = Arc::new(TraceHub::new(
        config.trace_tail_capacity,
        Arc::clone(&stats),
    ));
    let limits = QueryLimits {
        default_limit: config.query_default_limit,
        max_limit: config.query_max_limit,
    };
    let logs = LogService::new(store.clone(), Arc::clone(&hub), Arc::clone(&stats), limits);
    let traces = TraceService::new(
        store.clone(),
        Arc::clone(&trace_hub),
        Arc::clone(&stats),
        limits,
    );
    let metrics_service = MetricService::new(store.clone(), Arc::clone(&stats), limits);

    let grpc_listener = TcpListener::bind(&config.listen)
        .await
        .with_context(|| format!("cannot bind gRPC listener on {}", config.listen))?;
    let grpc_addr = grpc_listener
        .local_addr()
        .context("cannot read the gRPC listener address")?;
    let metrics_listener = TcpListener::bind(&config.metrics_listen)
        .await
        .with_context(|| {
            format!(
                "cannot bind Prometheus listener on {}",
                config.metrics_listen
            )
        })?;
    let metrics_addr = metrics_listener
        .local_addr()
        .context("cannot read the Prometheus listener address")?;

    // Both addresses, and only now: `listen` may name a hostname, and it is
    // what the sockets actually resolved to that decides who can reach them.
    // A refusal here happens before either listener is served, so a service
    // that must not run unauthenticated never answers a single call.
    let auth = TelemetryAuth::for_listeners(config, grpc_addr, metrics_addr)?;
    // Read off before the interceptor copies are handed to the tasks, so the
    // startup line below can say which regime is in force.
    let enforcing = auth.is_enforcing();

    let (shutdown, _) = watch::channel(false);

    // The standard grpc.health.v1 service is the only liveness surface this
    // process offers, which is what deis-store and deis-queue offer too: one
    // probe shape for the whole platform, and no second answer that could
    // disagree with it. Each service is reported separately so a caller can
    // ask about the one it actually uses. It answers for this process only.
    let (mut health_reporter, health_service) = tonic_health::server::health_reporter();
    health_reporter
        .set_serving::<LogServiceServer<LogService>>()
        .await;
    health_reporter
        .set_serving::<TraceServiceServer<TraceService>>()
        .await;
    health_reporter
        .set_serving::<MetricServiceServer<MetricService>>()
        .await;

    // Log batches are the reason this ceiling exists: a runtime shipping a few
    // thousand lines at once must not be cut off at tonic's 4 MB default.
    let log_service = LogServiceServer::new(logs)
        .max_decoding_message_size(MAX_GRPC_MESSAGE_BYTES)
        .max_encoding_message_size(MAX_GRPC_MESSAGE_BYTES);
    // Trace batches need the same ceiling for the same reason: a busy node
    // hands over one record per invocation, not one per log line.
    let trace_service = TraceServiceServer::new(traces)
        .max_decoding_message_size(MAX_GRPC_MESSAGE_BYTES)
        .max_encoding_message_size(MAX_GRPC_MESSAGE_BYTES);
    let metric_service = MetricServiceServer::new(metrics_service)
        .max_decoding_message_size(MAX_GRPC_MESSAGE_BYTES)
        .max_encoding_message_size(MAX_GRPC_MESSAGE_BYTES);

    // Wrapped here rather than at `add_service`, so the three services reach
    // the server already behind the check and no registration below can be
    // written without it.
    let log_service = guarded(log_service, &auth);
    let trace_service = guarded(trace_service, &auth);
    let metric_service = guarded(metric_service, &auth);

    let grpc_shutdown = subscribe(&shutdown);
    let grpc = tokio::spawn(async move {
        let result = Server::builder()
            .add_service(health_service)
            .add_service(log_service)
            .add_service(trace_service)
            .add_service(metric_service)
            .serve_with_incoming_shutdown(TcpListenerStream::new(grpc_listener), grpc_shutdown)
            .await;
        match result {
            Ok(()) => info!("gRPC server stopped"),
            Err(e) => {
                error!(error = %e, "gRPC server stopped with an error, log ingest and queries are no longer served")
            }
        }
    });

    let metrics_app = http::router(
        MetricsState::new(
            store.clone(),
            Arc::clone(&stats),
            config.metrics_export_window_secs,
        ),
        auth,
    );
    let metrics_shutdown = subscribe(&shutdown);
    let metrics = tokio::spawn(async move {
        let result = axum::serve(metrics_listener, metrics_app)
            .with_graceful_shutdown(metrics_shutdown)
            .await;
        match result {
            Ok(()) => info!("Prometheus endpoint stopped"),
            Err(e) => {
                error!(error = %e, "Prometheus endpoint stopped with an error, scrapes will fail until restart")
            }
        }
    });

    // The status above used to be the last word: set once, at startup, and
    // never touched again. A process whose database had filled its disk, or
    // had been restored unwritable, went on answering SERVING while refusing
    // every push - an observability service that reports healthy and collects
    // nothing, which nothing downstream has any other way to notice. From here
    // on the three statuses follow the database itself.
    let health = tokio::spawn(health_follows_the_store(
        health_reporter,
        stats.store_health.subscribe(),
        subscribe(&shutdown),
    ));

    let retention = tokio::spawn(retention_loop(
        store.clone(),
        Arc::clone(&stats),
        RetentionPolicy {
            log_days: config.log_retention_days,
            metric_days: config.metric_retention_days,
            trace_days: config.trace_retention_days,
            interval: Duration::from_secs(config.retention_sweep_interval_secs),
        },
        subscribe(&shutdown),
    ));

    info!(
        %grpc_addr,
        %metrics_addr,
        authenticated = enforcing,
        data_dir = %config.data_dir.display(),
        log_retention_days = config.log_retention_days,
        metric_retention_days = config.metric_retention_days,
        trace_retention_days = config.trace_retention_days,
        "deis-telemetry started"
    );

    Ok(RunningServer {
        grpc_addr,
        metrics_addr,
        store,
        stats,
        hub,
        trace_hub,
        shutdown,
        grpc,
        metrics,
        retention,
        health,
    })
}

/// Keep the `grpc.health.v1` status of all three services on the truth about
/// the database.
///
/// Woken by a change rather than polling, so the gap between a failed storage
/// call and a probe that says so is one task wakeup, not one poll interval. A
/// probe answering `SERVING` for even a few seconds after the store has gone
/// is a probe a supervisor will believe.
///
/// All three services move together because all three sit on one database.
/// Reporting them apart would suggest a caller could route around the failure
/// by using a different one, and there is nowhere to route to.
async fn health_follows_the_store(
    mut reporter: tonic_health::server::HealthReporter,
    mut state: watch::Receiver<StoreState>,
    shutdown: impl std::future::Future<Output = ()> + Send,
) {
    tokio::pin!(shutdown);
    loop {
        tokio::select! {
            changed = state.changed() => {
                // An error means the sender is gone, which only happens as the
                // process goes away; there is nothing left to report to.
                if changed.is_err() {
                    return;
                }
                let serving = state.borrow_and_update().is_serving();
                if serving {
                    reporter.set_serving::<LogServiceServer<LogService>>().await;
                    reporter.set_serving::<TraceServiceServer<TraceService>>().await;
                    reporter.set_serving::<MetricServiceServer<MetricService>>().await;
                    info!("grpc.health.v1 now reports SERVING for all three services again");
                } else {
                    reporter.set_not_serving::<LogServiceServer<LogService>>().await;
                    reporter.set_not_serving::<TraceServiceServer<TraceService>>().await;
                    reporter.set_not_serving::<MetricServiceServer<MetricService>>().await;
                    warn!(
                        "grpc.health.v1 now reports NOT_SERVING for all three services: the \
                         database behind them stopped answering, so anything this process is \
                         handed from here on is refused rather than stored"
                    );
                }
            }
            _ = &mut shutdown => return,
        }
    }
}

/// A future that resolves once shutdown is requested.
fn subscribe(
    shutdown: &watch::Sender<bool>,
) -> impl std::future::Future<Output = ()> + Send + use<> {
    let mut rx = shutdown.subscribe();
    async move {
        // `changed()` errors only when the sender is gone, which also means
        // the process is on its way out.
        let _ = rx.changed().await;
    }
}

#[derive(Debug, Clone, Copy)]
struct RetentionPolicy {
    log_days: u32,
    metric_days: u32,
    trace_days: u32,
    interval: Duration,
}

/// Drop expired day partitions and metric samples on a timer.
async fn retention_loop(
    store: TelemetryStore,
    stats: Arc<ServiceStats>,
    policy: RetentionPolicy,
    shutdown: impl std::future::Future<Output = ()> + Send,
) {
    let mut ticker = tokio::time::interval(policy.interval);
    // A sweep that ran late must not cause a burst of catch-up sweeps.
    ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
    tokio::pin!(shutdown);
    info!(
        log_days = policy.log_days,
        metric_days = policy.metric_days,
        trace_days = policy.trace_days,
        interval_secs = policy.interval.as_secs(),
        "retention sweeper started; expired data is deleted permanently"
    );
    loop {
        tokio::select! {
            // The first tick fires immediately, which is the startup sweep.
            _ = ticker.tick() => sweep_once(&store, &stats, policy).await,
            _ = &mut shutdown => {
                info!("retention sweeper stopped");
                break;
            }
        }
    }
}

async fn sweep_once(store: &TelemetryStore, stats: &Arc<ServiceStats>, policy: RetentionPolicy) {
    let store = store.clone();
    let now = now_us();
    let outcome = tokio::task::spawn_blocking(move || {
        store.sweep_retention(now, policy.log_days, policy.metric_days, policy.trace_days)
    })
    .await;
    match outcome {
        Ok(Ok(outcome)) => {
            stats.store_health.record_success(Access::Write);
            if outcome.is_empty() {
                return;
            }
            let partitions: Vec<&str> = outcome
                .dropped_partitions
                .iter()
                .map(|p| p.table.as_str())
                .collect();
            let trace_partitions: Vec<&str> = outcome
                .dropped_trace_partitions
                .iter()
                .map(|p| p.table.as_str())
                .collect();
            ServiceStats::add(
                &stats.retention_partitions_dropped,
                outcome.dropped_partitions.len() as u64,
            );
            ServiceStats::add(
                &stats.retention_log_rows_dropped,
                outcome.dropped_log_rows().max(0) as u64,
            );
            ServiceStats::add(
                &stats.retention_metric_samples_dropped,
                outcome.deleted_metric_samples as u64,
            );
            ServiceStats::add(
                &stats.retention_trace_partitions_dropped,
                outcome.dropped_trace_partitions.len() as u64,
            );
            ServiceStats::add(
                &stats.retention_trace_rows_dropped,
                outcome.dropped_trace_rows().max(0) as u64,
            );
            info!(
                partitions = ?partitions,
                trace_partitions = ?trace_partitions,
                log_rows = outcome.dropped_log_rows(),
                trace_rows = outcome.dropped_trace_rows(),
                metric_samples = outcome.deleted_metric_samples,
                log_retention_days = policy.log_days,
                metric_retention_days = policy.metric_days,
                trace_retention_days = policy.trace_days,
                "retention dropped expired data; those logs, traces and samples are gone for good"
            );
        }
        // Both arms move two counters and the health state, not just the log.
        // The sweeper is the one path into the database that runs without a
        // caller, so on a service nobody is pushing to it is the first - and
        // for an hour at a time the only - thing to notice that the database
        // has stopped answering.
        Ok(Err(e)) => {
            ServiceStats::incr(&stats.storage_failures);
            ServiceStats::incr(&stats.retention_sweeps_failed);
            stats
                .store_health
                .record_failure(Access::Write, "retention sweep");
            error!(
                error = ?e,
                interval_secs = policy.interval.as_secs(),
                "retention sweep failed, expired data stays on disk and the database keeps growing; \
                 the next attempt is one interval away"
            );
        }
        Err(e) => {
            ServiceStats::incr(&stats.storage_failures);
            ServiceStats::incr(&stats.retention_sweeps_failed);
            stats
                .store_health
                .record_failure(Access::Write, "retention sweep");
            error!(
                error = %e,
                interval_secs = policy.interval.as_secs(),
                "retention worker panicked or was cancelled, expired data stays on disk; \
                 the next attempt is one interval away"
            );
        }
    }
}
