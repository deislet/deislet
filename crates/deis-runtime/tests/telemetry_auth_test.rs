//! What this node presents to deis-telemetry, against a real collector.
//!
//! The collector accepts only capabilities the control plane signed, and
//! stores a pushed record only if the capability names the record's tenant
//! for ingest. The node holds whatever the control plane last issued it in a
//! shared [`TelemetryCredential`]; these tests stand in for the control plane
//! with one signing key and check the agreement end to end: the header the
//! shippers stamp, the audience and binding the collector wants, and the
//! tenant spelling both sides compute. Drift in any of them is silent in
//! production - the collector is up, the node is up, and every line is
//! refused while the node retries it for as long as it runs.
//!
//! `deis-telemetry` is a test-only dependency here for the same reason
//! `deis-store` is: running against the real server is what catches a change
//! on that side. It is not linked into the node.

use std::sync::Arc;
use std::time::Duration;

use deis_capability::{
    encode, CapabilityClaims, CapabilityResource, KeyRing, AUD_TELEMETRY, TELEMETRY_INGEST,
    TELEMETRY_PLATFORM_TENANT,
};
use deis_runtime::config::LoggingConfig;
use deis_runtime::logging::{LogBuffer, LogEntry, LogLevel, LogShipper, ShipperConfig};
use deis_runtime::telemetry::TelemetryCredential;
use deis_telemetry::config::Config;
use deis_telemetry::server::{self, RunningServer};
use deis_telemetry::store::LogFilter;
use ed25519_dalek::SigningKey;

/// The control plane's signing key, as far as these tests are concerned.
fn control_key() -> SigningKey {
    SigningKey::from_bytes(&[0x24; 32])
}

/// The control plane the collector polls its revocation list from.
fn fake_control() -> &'static deis_capability::testing::FakeControl {
    static CONTROL: std::sync::OnceLock<deis_capability::testing::FakeControl> =
        std::sync::OnceLock::new();
    CONTROL.get_or_init(|| deis_capability::testing::FakeControl::start(control_key()))
}

/// What the control plane would issue this node for `tenants`.
fn issued_for(tenants: &[&str]) -> String {
    let claims = CapabilityClaims::new(
        AUD_TELEMETRY,
        "spiffe://deis.test/deislet/control/main",
        "spiffe://deis.test/deislet/node/node-under-test",
        1,
        Duration::from_secs(900),
        std::iter::once(TELEMETRY_PLATFORM_TENANT)
            .chain(tenants.iter().copied())
            .map(|tenant| CapabilityResource {
                tenant: tenant.to_string(),
                binding: TELEMETRY_INGEST.to_string(),
                generation: None,
            })
            .collect(),
    );
    encode(&claims, &control_key())
}

/// The authority the collector's certificate is issued by, installed once as
/// the trust this node checks data services against.
fn fleet() -> &'static deis_identity_test_pki::TestFleet {
    static FLEET: std::sync::OnceLock<deis_identity_test_pki::TestFleet> =
        std::sync::OnceLock::new();
    FLEET.get_or_init(|| {
        let fleet = deis_identity_test_pki::TestFleet::new();
        deis_runtime::runtime::service_tls::install(deis_identity::ServiceTrust::from_identity(
            fleet.service_identity(deis_identity::ServiceKind::Node),
        ));
        fleet
    })
}

/// The collector's `[machine_identity]`: served over TLS, as every
/// deis-telemetry is.
fn collector_identity() -> Option<deis_capability::ServiceIdentityConfig> {
    let fleet = fleet();
    let id = deis_identity::names::telemetry(&fleet.trust_domain, "test").unwrap();
    let files = fleet.files_for("telemetry", &id);
    Some(deis_capability::ServiceIdentityConfig {
        certificate: files.certificate,
        private_key: files.private_key,
        trust_root: files.trust_root,
        control_endpoint: fake_control().url().to_string(),
        identity_reload_seconds: 60,
    })
}

/// A collector on ephemeral loopback ports trusting the control key.
async fn collector(data_dir: &std::path::Path) -> RunningServer {
    let config = Config {
        machine_identity: collector_identity(),
        listen: "127.0.0.1:0".to_string(),
        metrics_listen: "127.0.0.1:0".to_string(),
        data_dir: data_dir.to_path_buf(),
        ..Config::default()
    };
    server::start(&config, KeyRing::from_keys([control_key().verifying_key()]))
        .await
        .expect("the collector must start")
}

fn line(tenant: &str, message: &str) -> LogEntry {
    LogEntry::new(
        "node-under-test".to_string(),
        LogLevel::Info,
        message.to_string(),
    )
    .with_app_id(tenant.to_string())
}

/// Ship one line with `credential` and count what the collector stored under
/// that message.
async fn ship_one_line(
    server: &RunningServer,
    credential: TelemetryCredential,
    tenant: &str,
    message: &str,
) -> usize {
    let buffer = Arc::new(LogBuffer::new(16));
    buffer.push(line(tenant, message));

    let mut config = ShipperConfig::from(&LoggingConfig {
        enabled: true,
        endpoint: Some(format!("https://{}", server.grpc_addr)),
        flush_interval_secs: 1,
        max_retries: 1,
        ..LoggingConfig::default()
    });
    config.credential = credential;
    let shipper = LogShipper::new(buffer, config);
    shipper.start().await;
    // `shutdown` stops the loop, flushes what is left, and returns only once
    // the task has finished.
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
async fn a_node_holding_an_issued_capability_has_its_logs_kept() {
    let dir = tempfile::tempdir().unwrap();
    let server = collector(dir.path()).await;
    let credential = TelemetryCredential::with_token(&issued_for(&["demo.production"])).unwrap();

    let count = ship_one_line(&server, credential, "demo.production", "issued-capability").await;
    assert_eq!(
        count, 1,
        "the line must be stored; 0 means the collector refused the node's capability"
    );

    server.shutdown().await.expect("the collector stops");
}

#[tokio::test]
async fn a_line_for_an_application_the_node_does_not_run_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let server = collector(dir.path()).await;
    let credential = TelemetryCredential::with_token(&issued_for(&["demo.production"])).unwrap();

    let count = ship_one_line(&server, credential, "bank.production", "not-this-node").await;
    assert_eq!(count, 0, "a capability for demo must not write bank's logs");

    server.shutdown().await.expect("the collector stops");
}

#[tokio::test]
async fn a_node_without_a_capability_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let server = collector(dir.path()).await;

    let count = ship_one_line(
        &server,
        TelemetryCredential::default(),
        "demo.production",
        "no-capability",
    )
    .await;
    assert_eq!(
        count, 0,
        "there is no mode in which the collector skips the check"
    );

    server.shutdown().await.expect("the collector stops");
}
