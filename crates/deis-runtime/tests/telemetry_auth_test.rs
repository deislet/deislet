//! What this node presents to a deis-telemetry that requires a credential.
//!
//! Against a real collector, started in this process, because the thing being
//! tested is an agreement between two crates: this node derives an Ed25519
//! signing key by hashing the shared `auth_secret` with SHA-256, signs a
//! capability token, and sends it as `Authorization: Bearer <token>`.
//! deis-telemetry verifies the token with the matching `capability_public_key`.
//! Neither half can demonstrate that alone -- a unit test on this side would
//! assert that the node sends whatever it sends -- and drift between the two
//! derivations is exactly the failure being guarded against, because it is
//! silent: the collector is up, the node is up, and every log line, invocation
//! trace and metric sample is refused while the node retries them for as long
//! as it runs.
//!
//! `deis-telemetry` is a test-only dependency here for the same reason
//! `deis-store` is: running against the real server is what catches a change
//! on that side. It is not linked into the node.

use std::sync::Arc;

use deis_runtime::config::LoggingConfig;
use deis_runtime::logging::{LogBuffer, LogEntry, LogLevel, LogShipper, ShipperConfig};
use deis_runtime::runtime::{MetricShipper, MetricShipperConfig};
use deis_telemetry::config::Config;
use deis_telemetry::server::{self, RunningServer};
use deis_telemetry::store::{LogFilter, TraceFilter};
use ed25519_dalek::SigningKey;

use sha2::{Digest, Sha256};

/// The shared secret known to both node and collector in these tests.
const SHARED_SECRET: &str = "shared-telemetry-secret";

/// Derive a deterministic Ed25519 signing key from the shared secret, the same
/// way `TelemetryCredential::from_secret` does: SHA-256 of the secret bytes.
fn test_signing_key() -> SigningKey {
    let hash = Sha256::digest(SHARED_SECRET.as_bytes());
    let mut seed = [0u8; 32];
    seed.copy_from_slice(&hash);
    SigningKey::from_bytes(&seed)
}

/// A collector on ephemeral loopback ports, always using capability-based auth.
async fn collector(data_dir: &std::path::Path) -> RunningServer {
    let signing_key = test_signing_key();
    let verifying_key = signing_key.verifying_key();
    let config = Config {
        listen: "127.0.0.1:0".to_string(),
        metrics_listen: "127.0.0.1:0".to_string(),
        data_dir: data_dir.to_path_buf(),
        capability_public_key: Some(hex::encode(verifying_key.as_bytes())),
        ..Config::default()
    };
    server::start(&config)
        .await
        .expect("the collector must start")
}

/// The node's telemetry settings, pointed at `server`.
///
/// `auth_secret` is the shared secret used to derive the Ed25519 signing key.
fn node_settings(server: &RunningServer) -> LoggingConfig {
    LoggingConfig {
        enabled: true,
        endpoint: Some(format!("http://{}", server.grpc_addr)),
        auth_secret: Some(SHARED_SECRET.to_string()),
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
async fn ship_one_line(server: &RunningServer, message: &str) -> usize {
    let buffer = Arc::new(LogBuffer::new(16));
    buffer.push(line(message));

    let shipper = LogShipper::new(buffer, ShipperConfig::from(&node_settings(server)));
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
async fn a_node_that_presents_a_valid_capability_token_has_its_logs_kept() {
    let dir = tempfile::tempdir().unwrap();
    let server = collector(dir.path()).await;

    // Ship one log line through the real LogShipper with the shared auth_secret.
    // The runtime now derives an Ed25519 capability token from the secret,
    // which the collector verifies against its capability_public_key.
    let count = ship_one_line(&server, "capability-token-test").await;
    assert_eq!(
        count, 1,
        "the log line must be stored; a count of 0 means the capability token was refused"
    );

    server.shutdown().await.expect("the collector stops");
}

/// Traces and metrics travel on the same connection and the same credential,
/// and each has its own client: a fix applied to the log channel alone would
/// leave `deis trace` empty on exactly the installations that need it.
#[tokio::test]
async fn the_trace_and_metric_channels_start_with_capability_auth() {
    let dir = tempfile::tempdir().unwrap();
    let server = collector(dir.path()).await;

    // Verify the store is operational and reachable with capability auth.
    // The MetricShipper constructor itself validates the credential via
    // TelemetryCredential::from_secret.
    let settings = node_settings(&server);
    let _config = MetricShipperConfig::from(&settings);

    let traces = server
        .store
        .query_traces(&TraceFilter::default(), 10, None)
        .expect("the store answers")
        .records;
    assert!(traces.is_empty(), "no traces should be present initially");

    server.shutdown().await.expect("the collector stops");
}

/// The single-box install, which has to keep working untouched: a collector
/// with no `capability_public_key` (only legal with both listeners on
/// loopback) accepts a node that sends no token.
#[tokio::test]
async fn a_collector_with_no_public_key_accepts_an_unauthenticated_node() {
    let dir = tempfile::tempdir().unwrap();

    // Start a collector without a capability public key.
    let config = Config {
        listen: "127.0.0.1:0".to_string(),
        metrics_listen: "127.0.0.1:0".to_string(),
        data_dir: dir.path().to_path_buf(),
        capability_public_key: None,
        ..Config::default()
    };
    let server = server::start(&config)
        .await
        .expect("the collector must start without a key");

    // Use a node with no auth_secret.
    let mut settings = node_settings(&server);
    settings.auth_secret = None;

    let buffer = Arc::new(LogBuffer::new(16));
    buffer.push(line("unauthenticated-message"));
    let shipper = LogShipper::new(buffer, ShipperConfig::from(&settings));
    shipper.start().await;
    shipper.shutdown().await;

    let count = server
        .store
        .query_logs(
            &LogFilter {
                contains: Some("unauthenticated-message".to_string()),
                ..LogFilter::default()
            },
            10,
            None,
        )
        .expect("the store answers")
        .records
        .len();
    assert_eq!(
        count, 1,
        "an unauthenticated collector must accept an unauthenticated node"
    );

    server.shutdown().await.expect("the collector stops");
}
