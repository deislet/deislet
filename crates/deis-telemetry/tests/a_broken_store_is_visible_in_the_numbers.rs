//! What this service's own numbers say while its database is refusing writes.
//!
//! The failure this file is about is the quiet one. Every storage failure used
//! to produce a line in this process's own log and move no counter at all, so
//! an operator watching `/metrics` during a full disk saw `log_pushes_total`
//! climbing, `logs_accepted_total` flat, and every counter with "rejected" or
//! "failed" in its name reading zero - which is the shape of a service nobody
//! is pushing to, not the shape of a broken one. The `grpc.health.v1` status
//! was worse: set to `SERVING` once at startup and never touched again, so a
//! supervisor probing it was told the collector was fine while it was losing
//! every line it was handed.
//!
//! # How the failure is produced here
//!
//! A real one is a full disk, a read-only mount or a corrupt file, and all
//! three reach the service the same way: the write returns an error while
//! reads keep working. That is the case worth testing, because it is the one
//! where the scrape itself still answers and the numbers on it are all an
//! operator has.
//!
//! It is produced deterministically by putting a table of the wrong shape
//! where today's log partition goes. `insert_logs` creates a partition with
//! `CREATE TABLE IF NOT EXISTS`, which is then a no-op, and the insert that
//! follows fails on the columns that are not there. Nothing else in the
//! database is touched, so queries, the metric table and the scrape all keep
//! working - exactly as they would on a disk with no space left for a commit.

mod support;

use std::collections::HashMap;
use std::net::SocketAddr;

use deis_telemetry::config::Config;
use deis_telemetry::proto::log_service_client::LogServiceClient;
use deis_telemetry::proto::{LogEntry, PushLogsRequest};
use deis_telemetry::server::{self, RunningServer};
use deis_telemetry::stats::ServiceStats;
use deis_telemetry::{MAX_GRPC_MESSAGE_BYTES, now_us};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tonic::transport::Channel;
use tonic_health::pb::health_check_response::ServingStatus;
use tonic_health::pb::{HealthCheckRequest, health_client::HealthClient};

fn test_config(data_dir: &std::path::Path) -> Config {
    Config {
        listen: "127.0.0.1:0".to_string(),
        metrics_listen: "127.0.0.1:0".to_string(),
        data_dir: data_dir.to_path_buf(),
        machine_identity: test_identity(),
        ..Config::default()
    }
}

/// The one authority every server in this file is issued a certificate by.
///
/// A static, because a configuration outlives the call that builds it and the
/// files have to outlive every server started from it.
fn fleet() -> &'static deis_identity_test_pki::TestFleet {
    static FLEET: std::sync::OnceLock<deis_identity_test_pki::TestFleet> =
        std::sync::OnceLock::new();
    FLEET.get_or_init(deis_identity_test_pki::TestFleet::new)
}

/// The `[machine_identity]` a test server is started with.
fn test_identity() -> Option<deis_capability::ServiceIdentityConfig> {
    let fleet = fleet();
    let id = deis_identity::names::telemetry(&fleet.trust_domain, "test").unwrap();
    let files = fleet.files_for("telemetry", &id);
    Some(deis_capability::ServiceIdentityConfig {
        certificate: files.certificate,
        private_key: files.private_key,
        trust_root: files.trust_root,
        control_endpoint: "http://127.0.0.1:1".to_string(),
        identity_reload_seconds: 60,
    })
}

/// A channel to a test server's gRPC listener, checking the far end is a
/// telemetry service of this fleet the way every real caller does.
async fn tls_channel(addr: SocketAddr) -> Channel {
    tonic::transport::Endpoint::from_shared(format!("https://{addr}"))
        .expect("endpoint must be a valid URI")
        .connect_with_connector(fleet().service_trust().dialler(
            deis_identity::ServiceKind::Telemetry,
            &[deis_identity::Protocol::Http2],
        ))
        .await
        .expect("the gRPC listener must accept the connection")
}

async fn channel(server: &RunningServer) -> Channel {
    tls_channel(server.grpc_addr).await
}

fn entry(message: &str) -> LogEntry {
    LogEntry {
        timestamp_us: now_us(),
        level: 3,
        component: "deis-runtime".to_string(),
        app_id: "shop".to_string(),
        message: message.to_string(),
        fields: HashMap::new(),
        trace_id: String::new(),
        span_id: String::new(),
    }
}

async fn http_get(addr: SocketAddr, path: &str) -> String {
    let mut stream = TcpStream::connect(addr)
        .await
        .expect("the metrics listener must accept the connection");
    // As an operator: the page is every tenant's series, so only the
    // platform-wide read may have it.
    let token = support::operator_token();
    let request = format!(
        "GET {path} HTTP/1.1\r\nHost: {addr}\r\nAuthorization: Bearer {token}\r\nConnection: close\r\n\r\n"
    );
    stream
        .write_all(request.as_bytes())
        .await
        .expect("the request must be written");
    let mut response = String::new();
    stream
        .read_to_string(&mut response)
        .await
        .expect("the response must be readable");
    response
}

/// The table name `insert_logs` will reach for, for a record stamped now.
fn todays_log_partition() -> String {
    let day = chrono::Utc::now().format("%Y%m%d");
    format!("logs_{day}")
}

/// Put a table of the wrong shape where today's log partition goes, so that
/// every log write fails and everything else keeps working.
fn break_log_writes(data_dir: &std::path::Path) {
    let conn = rusqlite::Connection::open(data_dir.join("telemetry.sqlite"))
        .expect("the test can open the same database the service opened");
    conn.execute_batch(&format!(
        "DROP TABLE IF EXISTS {table}; CREATE TABLE {table} (id INTEGER PRIMARY KEY);",
        table = todays_log_partition()
    ))
    .expect("the injection must apply");
}

/// Undo it, so the same instance can be watched recovering.
fn repair_log_writes(data_dir: &std::path::Path) {
    let conn = rusqlite::Connection::open(data_dir.join("telemetry.sqlite"))
        .expect("the test can open the same database the service opened");
    conn.execute_batch(&format!("DROP TABLE {};", todays_log_partition()))
        .expect("the repair must apply");
}

/// Ask `grpc.health.v1` until it gives the answer expected, or give up.
///
/// A poll rather than a wait on a clock: the health task is woken by the
/// change the failed push already made, so what is being waited for is a task
/// wakeup, and yielding until it has happened is the real condition. A sleep
/// would either be longer than it needs to be or shorter than it can be.
async fn health_settles_on(server: &RunningServer, expected: ServingStatus) -> ServingStatus {
    health_of_settles_on(server, "deis.telemetry.LogService", expected).await
}

/// The same, for any service name - including the empty one, which is what
/// `grpc_health_probe` and a Kubernetes gRPC probe ask about by default.
async fn health_of_settles_on(
    server: &RunningServer,
    service: &str,
    expected: ServingStatus,
) -> ServingStatus {
    let mut client = HealthClient::new(channel(server).await);
    let mut last = ServingStatus::Unknown;
    for _ in 0..1_000 {
        let response = client
            .check(HealthCheckRequest {
                service: service.to_string(),
            })
            .await
            .expect("the health service answers without a token");
        last = response.into_inner().status();
        if last == expected {
            return last;
        }
        tokio::task::yield_now().await;
    }
    last
}

#[tokio::test]
async fn a_store_that_refuses_writes_moves_the_counters_and_the_health_status() {
    let dir = tempfile::tempdir().expect("a temporary directory");
    let server = server::start(&test_config(dir.path()), support::keys())
        .await
        .expect("the service starts");
    let stats = server.stats.clone();

    // Baseline: nothing has gone wrong, and every failure counter reads zero
    // because nothing has failed - which is the state the broken one used to
    // be indistinguishable from.
    assert_eq!(ServiceStats::get(&stats.storage_failures), 0);
    assert_eq!(ServiceStats::get(&stats.logs_lost), 0);
    assert!(stats.store_health.state().is_serving());

    let mut logs = LogServiceClient::with_interceptor(
        channel(&server).await,
        support::Bearer::new(&support::operator_token()),
    )
    .max_decoding_message_size(MAX_GRPC_MESSAGE_BYTES)
    .max_encoding_message_size(MAX_GRPC_MESSAGE_BYTES);
    logs.push_logs(PushLogsRequest {
        entries: vec![entry("before")],
        metadata: HashMap::new(),
    })
    .await
    .expect("a healthy service stores a batch");
    assert_eq!(ServiceStats::get(&stats.logs_accepted), 1);

    break_log_writes(dir.path());

    let refused = logs
        .push_logs(PushLogsRequest {
            entries: vec![entry("one"), entry("two"), entry("three")],
            metadata: HashMap::new(),
        })
        .await
        .expect_err("a write that cannot be committed must be answered with an error");
    assert_eq!(refused.code(), tonic::Code::Internal, "{refused:?}");

    // The three numbers an operator needs, and none of them moved before.
    assert_eq!(
        ServiceStats::get(&stats.storage_failures),
        1,
        "one storage call failed and the total that covers every path into the database must say so"
    );
    assert_eq!(
        ServiceStats::get(&stats.logs_lost),
        3,
        "three well-formed entries were accepted and then lost; the count of what went missing \
         is not the same question as whether a call failed"
    );
    assert_eq!(
        ServiceStats::get(&stats.logs_rejected),
        0,
        "nothing was wrong with these entries, so the malformed counter must stay still"
    );
    assert_eq!(
        ServiceStats::get(&stats.logs_accepted),
        1,
        "only the batch that really was written counts as accepted"
    );

    // And the probe stops lying. This is the half that a supervisor and the
    // control plane read; the counters above are only for whoever is looking.
    assert_eq!(
        health_settles_on(&server, ServingStatus::NotServing).await,
        ServingStatus::NotServing,
        "a collector that cannot store anything must not report SERVING"
    );
    // The unnamed service is the one a generic probe asks about, and it has
    // to say the same thing: a process whose three named services are all
    // NOT_SERVING is not healthy under any name.
    assert_eq!(
        health_of_settles_on(&server, "", ServingStatus::NotServing).await,
        ServingStatus::NotServing,
        "the unnamed service must reflect the named ones, or a default probe keeps the \
         process alive while it stores nothing"
    );

    // The scrape still works - reads are unaffected - so this is what the
    // operator watching /metrics actually sees.
    let page = http_get(server.metrics_addr, "/metrics").await;
    for expected in [
        // Writes are what this service is for, and they are what is broken.
        // Reads are not, and the page says so rather than blaming everything -
        // which is also why the export could be served at all.
        "deis_telemetry_store_writable 0",
        "deis_telemetry_store_readable 1",
        "deis_telemetry_storage_failures_total 1",
        "deis_telemetry_logs_lost_total 3",
    ] {
        assert!(
            page.contains(expected),
            "{expected} is missing from:\n{page}"
        );
    }

    // Recovery is not a restart. A disk that had space freed on it must take
    // the service back to SERVING on the next call that works, or an operator
    // who fixed the problem is left with a process that has to be bounced.
    repair_log_writes(dir.path());
    logs.push_logs(PushLogsRequest {
        entries: vec![entry("after")],
        metadata: HashMap::new(),
    })
    .await
    .expect("the write works again once the database does");
    assert!(stats.store_health.state().writable);
    assert_eq!(
        health_settles_on(&server, ServingStatus::Serving).await,
        ServingStatus::Serving,
    );
    assert_eq!(
        health_of_settles_on(&server, "", ServingStatus::Serving).await,
        ServingStatus::Serving,
        "the unnamed service recovers with the named ones"
    );
    assert_eq!(
        ServiceStats::get(&stats.storage_failures),
        1,
        "a counter only ever grows; recovery is the gauge's job, not this one's"
    );

    server.shutdown().await.expect("the service stops");
}

#[tokio::test]
async fn a_scrape_that_cannot_read_the_store_is_counted_and_said_out_loud() {
    // The export is the one storage call with no client to return an error
    // to: the scraper is told 500, and a 500 carries no body it keeps. So
    // whether anybody ever learns that the export failed depends entirely on
    // a counter a later, successful scrape can carry.
    let dir = tempfile::tempdir().expect("a temporary directory");
    let server = server::start(&test_config(dir.path()), support::keys())
        .await
        .expect("the service starts");
    let stats = server.stats.clone();

    // A metric table missing the column the export reads. Both export and the
    // retention preflight inventory need the labels column; if the startup
    // sweep overlaps this injection, both correctly report the same broken
    // database. The scrape-specific counter below proves this request was
    // one of the failures without assuming task scheduling order.
    let conn = rusqlite::Connection::open(dir.path().join("telemetry.sqlite"))
        .expect("the test can open the same database the service opened");
    conn.execute_batch(
        "DROP TABLE metrics;
         CREATE TABLE metrics (
             id           INTEGER PRIMARY KEY,
             name         TEXT NOT NULL,
             app_id       TEXT NOT NULL,
             value        REAL NOT NULL,
             timestamp_us INTEGER NOT NULL
         );",
    )
    .expect("the injection must apply");

    let response = http_get(server.metrics_addr, "/metrics").await;
    assert!(
        response.starts_with("HTTP/1.1 500"),
        "an export that read nothing must not answer 200: {response}"
    );
    assert_eq!(ServiceStats::get(&stats.metric_scrapes), 1);
    assert_eq!(ServiceStats::get(&stats.metric_scrapes_failed), 1);
    assert!(
        ServiceStats::get(&stats.storage_failures) >= 1,
        "the export reads the same database as everything else, and the total that says the \
         database is broken has to include it"
    );
    let state = stats.store_health.state();
    assert!(
        !state.readable,
        "the read failed, so the read side must say so"
    );
    assert!(
        state.writable,
        "and only the read failed: a service that can still be written to must not be reported \
         as though nothing works, or the next failure says nothing new"
    );

    server.shutdown().await.expect("the service stops");
}
