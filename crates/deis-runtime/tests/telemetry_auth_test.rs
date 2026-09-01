//! What this node presents to a deis-telemetry that requires a credential.
//!
//! Against a real collector, started in this process, because the thing being
//! tested is an agreement between two crates: this node derives a token over a
//! subject, deis-telemetry recomputes the expected token over its own subject,
//! and a push is accepted only if the two strings match. Neither half can
//! demonstrate that alone -- a unit test on this side would assert that the
//! node sends whatever it sends -- and drift between the two subjects is
//! exactly the failure being guarded against, because it is silent: the
//! collector is up, the node is up, and every log line, invocation trace and
//! metric sample is refused while the node retries them for as long as it
//! runs.
//!
//! `deis-telemetry` is a test-only dependency here for the same reason
//! `deis-store` is: running against the real server is what catches a change
//! on that side. It is not linked into the node.

use std::sync::Arc;

use deis_runtime::config::LoggingConfig;
use deis_runtime::logging::{LogBuffer, LogEntry, LogLevel, LogShipper, ShipperConfig};
use deis_runtime::runtime::{
    recorder, InvocationKind, InvocationTrace, MetricShipper, MetricShipperConfig,
};
use deis_telemetry::config::Config;
use deis_telemetry::server::{self, RunningServer};
use deis_telemetry::store::{LogFilter, TraceFilter};

/// The secret an operator would put in both files.
const SHARED_SECRET: &str = "the-installation-secret";

/// A collector on ephemeral loopback ports, guarding itself with
/// [`SHARED_SECRET`].
///
/// Loopback so that the *only* reason it enforces is the secret: bound
/// anywhere else it would refuse to start without one, and this test would
/// then be about the bind address rather than about the credential.
async fn collector(data_dir: &std::path::Path, secret: Option<&str>) -> RunningServer {
    let config = Config {
        listen: "127.0.0.1:0".to_string(),
        metrics_listen: "127.0.0.1:0".to_string(),
        data_dir: data_dir.to_path_buf(),
        auth_secret: secret.map(str::to_string),
        ..Config::default()
    };
    server::start(&config)
        .await
        .expect("the collector must start")
}

/// The node's telemetry settings, pointed at `server`.
fn node_settings(server: &RunningServer, secret: Option<&str>) -> LoggingConfig {
    LoggingConfig {
        enabled: true,
        endpoint: Some(format!("http://{}", server.grpc_addr)),
        auth_secret: secret.map(str::to_string),
        // One second, but nothing waits for a tick: every push in this file is
        // driven by `shutdown`, which flushes and returns once the task has
        // finished. A test that waited for the timer would be a test of the
        // timer.
        flush_interval_secs: 1,
        ..LoggingConfig::default()
    }
}

fn line(message: &str) -> LogEntry {
    LogEntry::new(
        "node-under-test".to_string(),
        LogLevel::Info,
        message.to_string(),
    )
    .with_app_id("demo.production".to_string())
}

/// Ship one line and hand back what the collector stored under that message.
async fn ship_one_line(server: &RunningServer, secret: Option<&str>, message: &str) -> usize {
    let buffer = Arc::new(LogBuffer::new(16));
    buffer.push(line(message));

    let shipper = LogShipper::new(buffer, ShipperConfig::from(&node_settings(server, secret)));
    shipper.start().await;
    // The synchronisation point: `shutdown` stops the loop, flushes what is
    // left, and returns only once the task has finished. Whatever was going to
    // reach the collector has reached it by the time this returns.
    shipper.shutdown().await;

    server
        .store
        .query_logs(
            &LogFilter {
                contains: Some(message.to_string()),
                ..LogFilter::default()
            },
            10,
            None,
        )
        .expect("the store answers")
        .records
        .len()
}

#[tokio::test]
async fn a_node_that_carries_the_token_is_the_only_one_whose_logs_are_kept() {
    let dir = tempfile::tempdir().unwrap();
    let server = collector(dir.path(), Some(SHARED_SECRET)).await;

    // The whole point, in one assertion: the node derives the token from the
    // same secret the collector does, and the line is stored.
    assert_eq!(
        ship_one_line(
            &server,
            Some(SHARED_SECRET),
            "a line from a node that proved itself"
        )
        .await,
        1,
        "a node configured with the collector's own auth_secret must be able to ship at all; \
         this is what was impossible before -- the shipper had no field to put a secret in and \
         sent no credential, so on any installation whose collector has a secret every line from \
         every node was refused"
    );

    // And the two failures an operator can actually make, each of which used
    // to be indistinguishable from a telemetry outage.
    assert_eq!(
        ship_one_line(&server, None, "a line from a node with no secret at all").await,
        0,
        "no credential means refused, not accepted"
    );
    assert_eq!(
        ship_one_line(
            &server,
            Some("a different secret"),
            "a line from the wrong secret"
        )
        .await,
        0,
        "the token is derived, not sent: two different secrets cannot produce one token"
    );

    server.shutdown().await.expect("the collector stops");
}

/// Traces and metrics travel on the same connection and the same credential,
/// and each has its own client: a fix applied to the log channel alone would
/// leave `deis trace` empty on exactly the installations that need it.
#[tokio::test]
async fn the_trace_and_metric_channels_carry_the_same_credential() {
    let dir = tempfile::tempdir().unwrap();
    let server = collector(dir.path(), Some(SHARED_SECRET)).await;

    // A tenant of this test's own, so the process-wide recorder that every
    // test in this crate shares cannot make the count wrong.
    let tenant = "telemetry-auth-test.production";
    let mut trace = InvocationTrace::new(tenant, InvocationKind::HttpRequest);
    trace.trace_id = "0123456789abcdef0123456789abcdef".to_string();
    trace.target = "http://localhost/proved".to_string();
    recorder().record_invocation(trace);

    let shipper = MetricShipper::for_process(
        MetricShipperConfig::from(&node_settings(&server, Some(SHARED_SECRET))),
        "node-under-test".to_string(),
    );
    shipper.start().await;
    shipper.shutdown().await;

    let stored = server
        .store
        .query_traces(
            &TraceFilter {
                trace_id: Some("0123456789abcdef0123456789abcdef".to_string()),
                ..TraceFilter::default()
            },
            10,
            None,
        )
        .expect("the store answers")
        .records;
    assert_eq!(
        stored.len(),
        1,
        "an invocation trace pushed with the credential has to be accepted; without one the \
         records queue on the node until its buffer fills and `deis trace` never shows an \
         invocation this node served"
    );
    assert_eq!(stored[0].target, "http://localhost/proved");

    // The metric channel is the third client on the same connection. Its
    // samples are cumulative totals for every application this process
    // recorded, so what is asserted is that the push was accepted at all --
    // a refused one stores nothing.
    assert!(
        server
            .store
            .metric_sample_count()
            .expect("the store answers")
            > 0,
        "the metric client has to present the credential too, or every counter and every latency \
         bucket from this node is refused"
    );

    server.shutdown().await.expect("the collector stops");
}

/// The single-box install, which has to keep working untouched: a collector
/// with no `auth_secret` (only legal with both listeners on loopback) accepts
/// a node that sends no token.
#[tokio::test]
async fn a_collector_that_requires_nothing_still_takes_a_node_that_sends_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let server = collector(dir.path(), None).await;

    assert_eq!(
        ship_one_line(&server, None, "a line on a single-box install").await,
        1,
        "the default install has no secret on either side and must not have been broken by \
         giving the node one to send"
    );

    server.shutdown().await.expect("the collector stops");
}
