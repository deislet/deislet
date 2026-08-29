//! Process wiring: open the queue, register it on one gRPC server, and keep
//! the dead letter sweep running alongside.
//!
//! # Nothing here can be held open by a quiet client
//!
//! Wave A found that a client which stops reading a server stream pins the
//! sending task and its file descriptor with no time bound at all. Three
//! things keep that shape out of this service:
//!
//! 1. Every RPC is unary and answers from the table as it stands. There is no
//!    long poll: an empty queue returns an empty list immediately, so a
//!    consumer that stops calling leaves nothing behind.
//! 2. A per-request timeout. Every call here is a bounded query against a
//!    local SQLite file, so a request that approaches the ceiling is a wedged
//!    writer; without the ceiling it would hold a connection pool permit
//!    until the process was restarted.
//! 3. HTTP/2 keepalive pings. A peer that vanishes without closing its TCP
//!    connection - a killed node, a dropped route - is detected and its
//!    connection dropped, rather than counting against the server for the
//!    life of the process.
//!
//! A consumer that pulls messages and then goes quiet is a separate case, and
//! it costs nothing: the lease lapses on its own and the messages are handed
//! to someone else.

use crate::MAX_GRPC_MESSAGE_BYTES;
use crate::config::Config;
use crate::proto::queue::queue_server::QueueServer;
use crate::queue::QueueService;
use anyhow::{Context, Result};
use std::future::Future;
use std::time::Duration;
use tokio::net::TcpListener;
use tokio::signal::unix::{SignalKind, signal};
use tokio::task::JoinHandle;
use tokio_stream::wrappers::TcpListenerStream;
use tonic::transport::Server;

/// How often the server pings an idle connection.
const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(60);

/// How long a ping may go unanswered before the connection is dropped.
const KEEPALIVE_TIMEOUT: Duration = Duration::from_secs(20);

/// Open the queue under `config.data_dir`, creating the directory if needed.
pub async fn open(config: &Config) -> Result<QueueService> {
    std::fs::create_dir_all(&config.data_dir)
        .with_context(|| format!("cannot create data dir {}", config.data_dir.display()))?;
    QueueService::open(&config.data_dir, config.connections, config.limits())
        .await
        .context("cannot open the queue")
}

/// Delete dead letters past their retention on a fixed interval, until the
/// handle is dropped.
///
/// Returns `None` when retention is off, which is the default: dead letters
/// are evidence, and deleting them on a timer would destroy the only record
/// of why a queue failed. Nothing depends on this task, so a failed sweep is
/// logged and retried at the next tick.
pub fn spawn_dead_letter_sweeper(
    queue: QueueService,
    interval_secs: u64,
) -> Option<JoinHandle<()>> {
    if queue.limits().dead_letter_retention_secs <= 0 {
        tracing::info!(
            "dead letters are kept forever; the table grows until an operator prunes it"
        );
        return None;
    }
    // Zero would spin the loop with no delay at all.
    let period = Duration::from_secs(interval_secs.max(1));
    Some(tokio::spawn(async move {
        let mut ticker = tokio::time::interval(period);
        // The first tick fires immediately; skip past it so startup is not
        // slowed by a full-table delete.
        ticker.tick().await;
        loop {
            ticker.tick().await;
            match queue.sweep_dead_letters().await {
                Ok(0) => tracing::trace!("dead letter sweep found nothing past its retention"),
                Ok(removed) => tracing::info!(
                    removed,
                    "dead letter sweep deleted messages past their retention; they are gone for \
                     good and can no longer be inspected"
                ),
                Err(e) => tracing::warn!(
                    error = %e,
                    "dead letter sweep failed; expired rows stay on disk and the data directory \
                     keeps growing until a later sweep succeeds"
                ),
            }
        }
    }))
}

/// Bind the address in `config`.
pub async fn bind(config: &Config) -> Result<TcpListener> {
    TcpListener::bind(&config.listen)
        .await
        .with_context(|| format!("cannot bind {}", config.listen))
}

/// Resolve when the process is asked to stop.
///
/// Both signals are honoured, because both are used: an operator pressing
/// Ctrl-C sends SIGINT, while a process supervisor (systemd, Docker,
/// Kubernetes - not deis-control, which never stops this process) stops a
/// container with SIGTERM. Listening for SIGINT alone would leave this
/// service killed outright by every such supervisor, skipping the drain
/// below, so an in-flight send would be lost and an in-flight pull would
/// lease messages nobody ever receives.
pub async fn shutdown_signal() {
    let mut terminate = match signal(SignalKind::terminate()) {
        Ok(stream) => Some(stream),
        Err(e) => {
            tracing::error!(
                error = %e,
                "cannot listen for SIGTERM; a process supervisor stopping this service will kill \
                 it outright and in-flight requests will be lost"
            );
            None
        }
    };

    match terminate.as_mut() {
        Some(terminate) => {
            tokio::select! {
                result = tokio::signal::ctrl_c() => report_interrupt(result),
                _ = terminate.recv() => {
                    tracing::info!("SIGTERM received, draining in-flight requests");
                }
            }
        }
        None => report_interrupt(tokio::signal::ctrl_c().await),
    }
}

fn report_interrupt(result: std::io::Result<()>) {
    match result {
        Ok(()) => tracing::info!("interrupt received, draining in-flight requests"),
        Err(e) => tracing::error!(
            error = %e,
            "cannot listen for the interrupt signal; the process will only stop when killed, \
             which drops in-flight requests"
        ),
    }
}

/// Serve until `shutdown` resolves.
///
/// The listener is passed in already bound so a caller (the tests, for one)
/// can learn the port before the server starts accepting.
pub async fn serve(
    config: Config,
    listener: TcpListener,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> Result<()> {
    let addr = listener.local_addr().context("listener has no address")?;
    let queue = open(&config).await?;
    let sweeper = spawn_dead_letter_sweeper(queue.clone(), config.dead_letter_sweep_interval_secs);

    // Health reporting is always on so the control plane has a uniform
    // liveness probe across every Deislet service. The queue is open by the
    // time this is set, so "serving" means messages can actually be enqueued
    // and delivered, not merely that the process started.
    let (mut health_reporter, health_service) = tonic_health::server::health_reporter();
    health_reporter
        .set_serving::<QueueServer<QueueService>>()
        .await;

    info_ready(&config, addr);

    let mut builder = Server::builder()
        .http2_keepalive_interval(Some(KEEPALIVE_INTERVAL))
        .http2_keepalive_timeout(Some(KEEPALIVE_TIMEOUT));
    if let Some(timeout) = config.request_timeout() {
        builder = builder.timeout(timeout);
    }

    let result = builder
        .add_service(health_service)
        .add_service(
            QueueServer::new(queue)
                .max_decoding_message_size(MAX_GRPC_MESSAGE_BYTES)
                .max_encoding_message_size(MAX_GRPC_MESSAGE_BYTES),
        )
        .serve_with_incoming_shutdown(TcpListenerStream::new(listener), shutdown)
        .await;

    // Nothing else keeps the sweeper alive, and leaving it running would hold
    // a connection to a database the process is done with.
    if let Some(sweeper) = sweeper {
        sweeper.abort();
    }

    match result {
        Ok(()) => {
            tracing::info!(%addr, "deis-queue stopped serving");
            Ok(())
        }
        Err(e) => {
            tracing::error!(
                %addr,
                error = %e,
                "gRPC server terminated; nothing can be enqueued or consumed until it is \
                 restarted, and queued messages sit untouched in the meantime"
            );
            Err(anyhow::Error::new(e).context("gRPC server terminated"))
        }
    }
}

fn info_ready(config: &Config, addr: std::net::SocketAddr) {
    tracing::info!(
        %addr,
        data_dir = %config.data_dir.display(),
        max_message_size = MAX_GRPC_MESSAGE_BYTES,
        max_body_bytes = config.max_body_bytes,
        max_attempts = config.max_attempts,
        default_lease_secs = config.default_lease_secs,
        request_timeout_secs = config.request_timeout_secs,
        "deis-queue serving Queue"
    );
    if config.request_timeout().is_none() {
        tracing::warn!(
            "no request timeout is configured; a request that wedges on the database will hold a \
             connection pool permit until the process is restarted"
        );
    }
    if !addr.ip().is_loopback() {
        tracing::warn!(
            %addr,
            "listening off the local host with no caller authentication; the tenant header is \
             taken at face value, so anything that can route to this port can enqueue, drain and \
             delete any app's messages by naming its app id. Bind a loopback address unless \
             caller identity has been added first"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::queue::QueueLimits;
    use crate::tenant::AppId;
    use tempfile::TempDir;

    fn config_for(dir: &TempDir) -> Config {
        Config {
            data_dir: dir.path().to_path_buf(),
            ..Config::default()
        }
    }

    #[tokio::test]
    async fn opening_creates_the_database() {
        let dir = TempDir::new().unwrap();
        let _queue = open(&config_for(&dir)).await.unwrap();
        assert!(dir.path().join("queue.sqlite").exists());
    }

    #[tokio::test]
    async fn no_sweeper_runs_when_dead_letters_are_kept_forever() {
        let dir = TempDir::new().unwrap();
        let queue = open(&config_for(&dir)).await.unwrap();
        assert!(
            spawn_dead_letter_sweeper(queue, 1).is_none(),
            "a sweeper with nothing to sweep would just wake the process up"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn the_sweeper_reclaims_dead_letters_past_their_retention() {
        let dir = TempDir::new().unwrap();
        // A manual clock so the retention is waited out by arithmetic rather
        // than by this test standing still for ten minutes.
        let clock = crate::Clock::fixed(1_700_000_000);
        let queue = QueueService::open_with_clock(
            dir.path(),
            4,
            QueueLimits {
                max_attempts: 1,
                dead_letter_retention_secs: 600,
                ..QueueLimits::default()
            },
            clock.clone(),
        )
        .await
        .unwrap();

        let app = AppId::parse("app-one").unwrap();
        let id = queue.send(&app, "jobs", b"work".to_vec()).await.unwrap();
        queue.pull(&app, "jobs", None, None).await.unwrap();
        queue.nack(&app, "jobs", vec![id], None).await.unwrap();
        assert_eq!(queue.dead_letters(&app, "jobs").await.unwrap().len(), 1);

        let sweeper = spawn_dead_letter_sweeper(queue.clone(), 1).expect("retention is on");
        clock.advance(601);

        // Paused time: each step fires the ticker without waiting for it, and
        // each database round trip gives the sweeper's blocking work a chance
        // to land. Stepping rather than jumping keeps the test independent of
        // when the runtime first polls the spawned task. The loop is bounded,
        // so a sweeper that never runs fails the test instead of hanging it.
        let mut remaining = queue.dead_letters(&app, "jobs").await.unwrap().len();
        for _ in 0..20 {
            if remaining == 0 {
                break;
            }
            tokio::time::advance(Duration::from_secs(1)).await;
            tokio::task::yield_now().await;
            remaining = queue.dead_letters(&app, "jobs").await.unwrap().len();
        }
        sweeper.abort();
        assert_eq!(
            remaining, 0,
            "the sweeper should have deleted the dead letter"
        );
    }
}
