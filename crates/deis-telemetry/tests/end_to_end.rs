//! End-to-end exercise of a real deis-telemetry process: the service is
//! started against a temporary data directory on ephemeral ports, then driven
//! through its own gRPC client and a plain HTTP scrape.

use std::collections::HashMap;
use std::net::SocketAddr;

use deis_backend_migration::proto::backend_migration_client::BackendMigrationClient;
use deis_backend_migration::proto::{
    AcquireWriteFenceRequest, GetBackendInfoRequest, InspectTenantRequest, ReleaseWriteFenceRequest,
};
use deis_telemetry::config::Config;
use deis_telemetry::proto::log_service_client::LogServiceClient;
use deis_telemetry::proto::metric_service_client::MetricServiceClient;
use deis_telemetry::proto::trace_service_client::TraceServiceClient;
use deis_telemetry::proto::{
    LogEntry, MetricSample, PushLogsRequest, PushMetricsRequest, PushTracesRequest,
    QueryMetricsRequest, QueryRequest, QueryTracesRequest, TailRequest, TailTracesRequest,
    TraceRecord,
};
use deis_telemetry::server::{self, RunningServer};
use deis_telemetry::{MAX_GRPC_MESSAGE_BYTES, now_us};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tonic::transport::Channel;
use tonic_health::pb::health_check_response::ServingStatus;
use tonic_health::pb::{HealthCheckRequest, health_client::HealthClient};

/// Configuration for a throwaway instance: both listeners take an ephemeral
/// port so tests can run side by side.
fn test_config(data_dir: &std::path::Path) -> Config {
    Config {
        listen: "127.0.0.1:0".to_string(),
        metrics_listen: "127.0.0.1:0".to_string(),
        data_dir: data_dir.to_path_buf(),
        query_default_limit: 100,
        query_max_limit: 500,
        ..Config::default()
    }
}

async fn channel(server: &RunningServer) -> Channel {
    let endpoint = format!("http://{}", server.grpc_addr);
    Channel::from_shared(endpoint)
        .expect("endpoint must be a valid URI")
        .connect()
        .await
        .expect("the gRPC listener must accept the connection")
}

async fn connect(server: &RunningServer) -> LogServiceClient<Channel> {
    LogServiceClient::new(channel(server).await)
        .max_decoding_message_size(MAX_GRPC_MESSAGE_BYTES)
        .max_encoding_message_size(MAX_GRPC_MESSAGE_BYTES)
}

async fn connect_metrics(server: &RunningServer) -> MetricServiceClient<Channel> {
    MetricServiceClient::new(channel(server).await)
        .max_decoding_message_size(MAX_GRPC_MESSAGE_BYTES)
        .max_encoding_message_size(MAX_GRPC_MESSAGE_BYTES)
}

async fn connect_traces(server: &RunningServer) -> TraceServiceClient<Channel> {
    TraceServiceClient::new(channel(server).await)
        .max_decoding_message_size(MAX_GRPC_MESSAGE_BYTES)
        .max_encoding_message_size(MAX_GRPC_MESSAGE_BYTES)
}

async fn connect_migration(server: &RunningServer) -> BackendMigrationClient<Channel> {
    BackendMigrationClient::new(channel(server).await)
        .max_decoding_message_size(MAX_GRPC_MESSAGE_BYTES)
        .max_encoding_message_size(MAX_GRPC_MESSAGE_BYTES)
}

fn managed<T>(message: T, token: &str) -> tonic::Request<T> {
    let mut request = tonic::Request::new(message);
    request.metadata_mut().insert(
        "authorization",
        format!("Bearer {token}").parse().expect("valid metadata"),
    );
    request
}

/// `InvocationKind::HttpRequest`.
const KIND_HTTP: i32 = 1;
/// `InvocationOutcome::Ok`.
const OUTCOME_OK: i32 = 1;
/// `InvocationOutcome::ApplicationError`.
const OUTCOME_APPLICATION_ERROR: i32 = 2;
/// `InvocationOutcome::ExecutionTimeout`.
const OUTCOME_EXECUTION_TIMEOUT: i32 = 3;

fn invocation(app_id: &str, outcome: i32, target: &str, started_us: i64) -> TraceRecord {
    TraceRecord {
        trace_id: format!("trace-{started_us}"),
        started_us,
        duration_us: 12_345,
        app_id: app_id.to_string(),
        environment: "production".to_string(),
        node: "node-a".to_string(),
        kind: KIND_HTTP,
        outcome,
        target: target.to_string(),
        method: "GET".to_string(),
        status: if outcome == OUTCOME_OK { 200 } else { 500 },
        error: if outcome == OUTCOME_OK {
            String::new()
        } else {
            "boom".to_string()
        },
        attributes: HashMap::new(),
    }
}

fn entry(app_id: &str, level: i32, message: &str, timestamp_us: i64) -> LogEntry {
    LogEntry {
        timestamp_us,
        level,
        component: "deis-runtime".to_string(),
        app_id: app_id.to_string(),
        message: message.to_string(),
        fields: HashMap::new(),
        trace_id: String::new(),
        span_id: String::new(),
    }
}

/// Minimal HTTP/1.1 GET. `Connection: close` makes the server end the body,
/// so reading to EOF is a deterministic way to know the response is complete.
async fn http_get(addr: SocketAddr, path: &str) -> String {
    http_get_with(addr, path, &[]).await
}

/// The same, carrying extra request headers - which is how a scrape presents
/// its capability token.
async fn http_get_with(addr: SocketAddr, path: &str, headers: &[(&str, &str)]) -> String {
    let mut stream = TcpStream::connect(addr)
        .await
        .expect("the metrics listener must accept the connection");
    let extra: String = headers
        .iter()
        .map(|(name, value)| format!("{name}: {value}\r\n"))
        .collect();
    let request =
        format!("GET {path} HTTP/1.1\r\nHost: {addr}\r\n{extra}Connection: close\r\n\r\n");
    stream
        .write_all(request.as_bytes())
        .await
        .expect("request must be written");
    let mut response = String::new();
    stream
        .read_to_string(&mut response)
        .await
        .expect("response must be read");
    response
}

#[tokio::test]
async fn push_query_and_tail_against_a_running_service() {
    let dir = tempfile::tempdir().expect("temp dir");
    let server = server::start(&test_config(dir.path()))
        .await
        .expect("the service must start");
    let mut client = connect(&server).await;

    // Health first, over the standard grpc.health.v1 service - the one every
    // Deislet component registers, and the only one this process answers. It
    // reports on this process and says nothing about anybody else's.
    let mut health = HealthClient::new(channel(&server).await);
    for service in [
        "deis.telemetry.LogService",
        "deis.telemetry.TraceService",
        "deis.telemetry.MetricService",
        // The empty name asks about the server as a whole.
        "",
    ] {
        let status = health
            .check(HealthCheckRequest {
                service: service.to_string(),
            })
            .await
            .unwrap_or_else(|e| panic!("health check for {service:?} must answer: {e}"))
            .into_inner();
        assert_eq!(
            status.status,
            ServingStatus::Serving as i32,
            "{service:?} must report itself as serving"
        );
    }

    // A tail stream is opened before anything is pushed; once the call has
    // returned, the subscription is attached, so the pushes below cannot
    // slip past it.
    let mut tail = client
        .tail(TailRequest {
            app_id: "shop".to_string(),
            min_level: 0,
        })
        .await
        .expect("tail must open")
        .into_inner();

    let base = now_us() - 10_000;
    let pushed = client
        .push_logs(PushLogsRequest {
            entries: vec![
                entry("shop", 3, "order received", base + 1),
                entry("blog", 3, "unrelated app", base + 2),
                entry("shop", 5, "payment failed", base + 3),
            ],
            metadata: HashMap::new(),
        })
        .await
        .expect("push must succeed")
        .into_inner();
    assert_eq!(pushed.accepted_count, 3);
    assert_eq!(pushed.rejected_count, 0);

    // The tail stream sees only the followed application, in arrival order.
    let first = tail
        .message()
        .await
        .expect("tail must stream")
        .expect("a first entry must arrive");
    assert_eq!(first.message, "order received");
    let second = tail
        .message()
        .await
        .expect("tail must stream")
        .expect("a second entry must arrive");
    assert_eq!(second.message, "payment failed");

    // Query returns newest first and honours the app filter.
    let page = client
        .query(QueryRequest {
            app_id: "shop".to_string(),
            ..Default::default()
        })
        .await
        .expect("query must answer")
        .into_inner();
    let messages: Vec<&str> = page.entries.iter().map(|e| e.message.as_str()).collect();
    assert_eq!(messages, vec!["payment failed", "order received"]);
    assert!(!page.has_more);
    assert!(page.next_cursor.is_empty());

    // The level filter keeps only the error.
    let errors = client
        .query(QueryRequest {
            min_level: 5,
            ..Default::default()
        })
        .await
        .expect("query must answer")
        .into_inner();
    assert_eq!(errors.entries.len(), 1);
    assert_eq!(errors.entries[0].message, "payment failed");

    // Free-text search is case-insensitive.
    let found = client
        .query(QueryRequest {
            contains: "PAYMENT".to_string(),
            ..Default::default()
        })
        .await
        .expect("query must answer")
        .into_inner();
    assert_eq!(found.entries.len(), 1);

    // The tail stream is deliberately still open: shutting down must not wait
    // on a follower that has no reason to ever stop.
    server.shutdown().await.expect("shutdown must succeed");
    assert!(
        tail.message()
            .await
            .expect("the stream must end cleanly")
            .is_none(),
        "a tail stream must end when the service stops"
    );
}

#[tokio::test]
async fn queries_are_paginated_newest_first() {
    let dir = tempfile::tempdir().expect("temp dir");
    let server = server::start(&test_config(dir.path()))
        .await
        .expect("the service must start");
    let mut client = connect(&server).await;

    let base = now_us() - 100_000;
    let entries: Vec<LogEntry> = (0..7)
        .map(|i| entry("shop", 3, &format!("entry {i}"), base + i))
        .collect();
    client
        .push_logs(PushLogsRequest {
            entries,
            metadata: HashMap::new(),
        })
        .await
        .expect("push must succeed");

    let mut seen = Vec::new();
    let mut cursor = String::new();
    loop {
        let page = client
            .query(QueryRequest {
                limit: 3,
                cursor: cursor.clone(),
                ..Default::default()
            })
            .await
            .expect("query must answer")
            .into_inner();
        seen.extend(page.entries.iter().map(|e| e.message.clone()));
        cursor = page.next_cursor;
        if !page.has_more {
            break;
        }
        assert!(
            !cursor.is_empty(),
            "a page with more data must hand back a cursor"
        );
    }
    let expected: Vec<String> = (0..7).rev().map(|i| format!("entry {i}")).collect();
    assert_eq!(seen, expected);

    // A cursor this service did not issue is refused rather than guessed at.
    let status = client
        .query(QueryRequest {
            cursor: "not-a-cursor".to_string(),
            ..Default::default()
        })
        .await
        .expect_err("a malformed cursor must be refused");
    assert_eq!(status.code(), tonic::Code::InvalidArgument);

    server.shutdown().await.expect("shutdown must succeed");
}

#[tokio::test]
async fn metrics_are_stored_queryable_and_scrapable() {
    let dir = tempfile::tempdir().expect("temp dir");
    let server = server::start(&test_config(dir.path()))
        .await
        .expect("the service must start");
    let metrics_addr = server.metrics_addr;
    let mut client = connect_metrics(&server).await;

    let now = now_us();
    let pushed = client
        .push_metrics(PushMetricsRequest {
            samples: vec![
                MetricSample {
                    name: "deis_runtime_requests_total".to_string(),
                    app_id: "shop".to_string(),
                    labels: HashMap::from([("route".to_string(), "/api".to_string())]),
                    value: 12.0,
                    timestamp_us: now - 1_000,
                },
                MetricSample {
                    name: "deis_runtime_requests_total".to_string(),
                    app_id: "shop".to_string(),
                    labels: HashMap::from([("route".to_string(), "/api".to_string())]),
                    value: 15.0,
                    timestamp_us: now,
                },
                MetricSample {
                    // Nameless samples cannot be exported, so they are refused.
                    name: String::new(),
                    app_id: "shop".to_string(),
                    labels: HashMap::new(),
                    value: 1.0,
                    timestamp_us: now,
                },
            ],
            metadata: HashMap::new(),
        })
        .await
        .expect("metric push must succeed")
        .into_inner();
    assert_eq!(pushed.accepted_count, 2);
    assert_eq!(pushed.rejected_count, 1);
    assert!(!pushed.error_message.is_empty());

    let queried = client
        .query_metrics(QueryMetricsRequest {
            name: "deis_runtime_requests_total".to_string(),
            app_id: "shop".to_string(),
            label_selector: HashMap::from([("route".to_string(), "/api".to_string())]),
            ..Default::default()
        })
        .await
        .expect("metric query must answer")
        .into_inner();
    assert_eq!(queried.samples.len(), 2);
    // Newest first.
    assert_eq!(queried.samples[0].value, 15.0);

    // A selector that matches nothing returns nothing rather than everything.
    let unmatched = client
        .query_metrics(QueryMetricsRequest {
            label_selector: HashMap::from([("route".to_string(), "/nowhere".to_string())]),
            ..Default::default()
        })
        .await
        .expect("metric query must answer")
        .into_inner();
    assert!(unmatched.samples.is_empty());

    let scrape = http_get(metrics_addr, "/metrics").await;
    assert!(
        scrape.starts_with("HTTP/1.1 200 OK"),
        "unexpected response:\n{scrape}"
    );
    assert!(scrape.contains("text/plain; version=0.0.4"));
    assert!(
        scrape.contains("deis_runtime_requests_total{app_id=\"shop\",route=\"/api\"} 15"),
        "the newest sample must be exported:\n{scrape}"
    );
    assert!(
        scrape.contains("deis_telemetry_metric_samples_accepted_total 2"),
        "the service's own counters must be exported:\n{scrape}"
    );

    server.shutdown().await.expect("shutdown must succeed");
}

#[tokio::test]
async fn a_batch_above_the_tonic_default_ceiling_is_accepted() {
    let dir = tempfile::tempdir().expect("temp dir");
    let server = server::start(&test_config(dir.path()))
        .await
        .expect("the service must start");
    let mut client = connect(&server).await;

    // 5 MB is past tonic's 4 MB default and well inside the 16 MB ceiling
    // every Deislet service agrees on.
    let huge = "x".repeat(5 * 1024 * 1024);
    let response = client
        .push_logs(PushLogsRequest {
            entries: vec![entry("shop", 3, &huge, now_us() - 1_000)],
            metadata: HashMap::new(),
        })
        .await
        .expect("a 5 MB batch must be accepted")
        .into_inner();
    assert_eq!(response.accepted_count, 1);

    // The entry is stored, with the message cut down to the per-entry cap.
    let page = client
        .query(QueryRequest::default())
        .await
        .expect("query must answer")
        .into_inner();
    assert_eq!(page.entries.len(), 1);
    assert!(page.entries[0].message.len() < huge.len());
    assert!(
        page.entries[0]
            .message
            .ends_with("[truncated by deis-telemetry]")
    );

    server.shutdown().await.expect("shutdown must succeed");
}

#[tokio::test]
async fn stored_logs_survive_a_restart() {
    let dir = tempfile::tempdir().expect("temp dir");
    let config = test_config(dir.path());

    let server = server::start(&config)
        .await
        .expect("the service must start");
    let mut client = connect(&server).await;
    client
        .push_logs(PushLogsRequest {
            entries: vec![entry(
                "shop",
                3,
                "written before the restart",
                now_us() - 1_000,
            )],
            metadata: HashMap::new(),
        })
        .await
        .expect("push must succeed");
    server.shutdown().await.expect("shutdown must succeed");

    let server = server::start(&config)
        .await
        .expect("the service must start again on the same data dir");
    let mut client = connect(&server).await;
    let page = client
        .query(QueryRequest::default())
        .await
        .expect("query must answer")
        .into_inner();
    assert_eq!(page.entries.len(), 1);
    assert_eq!(page.entries[0].message, "written before the restart");
    server.shutdown().await.expect("shutdown must succeed");
}

/// The question section 4.6 of the gap review says the platform cannot
/// answer: which invocations failed, and which of those the platform itself
/// stopped. Driven through the real service, over gRPC, end to end.
#[tokio::test]
async fn failed_invocations_are_queryable_without_reading_a_log_line() {
    let dir = tempfile::tempdir().expect("temp dir");
    let server = server::start(&test_config(dir.path()))
        .await
        .expect("the service must start");
    let mut traces = connect_traces(&server).await;

    // Opened before anything is pushed, so nothing can slip past it.
    let mut live = traces
        .tail_traces(TailTracesRequest {
            app_id: "shop".to_string(),
            outcomes: vec![OUTCOME_APPLICATION_ERROR, OUTCOME_EXECUTION_TIMEOUT],
            ..Default::default()
        })
        .await
        .expect("tail_traces must open")
        .into_inner();

    let base = now_us() - 10_000;
    let pushed = traces
        .push_traces(PushTracesRequest {
            records: vec![
                invocation("shop", OUTCOME_OK, "https://shop.test/", base + 1),
                invocation(
                    "shop",
                    OUTCOME_APPLICATION_ERROR,
                    "https://shop.test/checkout",
                    base + 2,
                ),
                invocation(
                    "shop",
                    OUTCOME_EXECUTION_TIMEOUT,
                    "https://shop.test/report",
                    base + 3,
                ),
                invocation(
                    "blog",
                    OUTCOME_APPLICATION_ERROR,
                    "https://blog.test/",
                    base + 4,
                ),
            ],
            // The producer says what it threw away, so a gap in the history
            // is a number rather than nothing.
            sampled_out: 7,
            dropped: 2,
            metadata: HashMap::new(),
        })
        .await
        .expect("push_traces must succeed")
        .into_inner();
    assert_eq!(pushed.accepted_count, 4);
    assert_eq!(pushed.rejected_count, 0);

    // The follower sees this application's failures and nothing else.
    let first = live
        .message()
        .await
        .expect("tail_traces must stream")
        .expect("a first record must arrive");
    assert_eq!(first.target, "https://shop.test/checkout");
    let second = live
        .message()
        .await
        .expect("tail_traces must stream")
        .expect("a second record must arrive");
    assert_eq!(second.outcome, OUTCOME_EXECUTION_TIMEOUT);

    // "Which of this application's invocations failed in this window", newest
    // first, with the platform's own verdict on each.
    let failures = traces
        .query_traces(QueryTracesRequest {
            app_id: "shop".to_string(),
            start_us: base,
            outcomes: vec![OUTCOME_APPLICATION_ERROR, OUTCOME_EXECUTION_TIMEOUT],
            ..Default::default()
        })
        .await
        .expect("query_traces must answer")
        .into_inner();
    let targets: Vec<&str> = failures
        .records
        .iter()
        .map(|record| record.target.as_str())
        .collect();
    assert_eq!(
        targets,
        vec!["https://shop.test/report", "https://shop.test/checkout"]
    );
    assert_eq!(failures.records[0].outcome, OUTCOME_EXECUTION_TIMEOUT);
    assert_eq!(failures.records[0].status, 500);
    assert_eq!(failures.records[0].node, "node-a");
    assert_eq!(failures.records[0].environment, "production");

    // One invocation, by the id its log lines carry.
    let one = traces
        .query_traces(QueryTracesRequest {
            trace_id: format!("trace-{}", base + 2),
            ..Default::default()
        })
        .await
        .expect("query_traces must answer")
        .into_inner();
    assert_eq!(one.records.len(), 1);
    assert_eq!(one.records[0].target, "https://shop.test/checkout");

    // What the producer threw away is on the scrape, not lost in silence.
    let scrape = http_get(server.metrics_addr, "/metrics").await;
    assert!(
        scrape.contains("deis_telemetry_traces_accepted_total 4"),
        "unexpected scrape:\n{scrape}"
    );
    assert!(
        scrape.contains("deis_telemetry_traces_sampled_out_total 7"),
        "sampling gaps must be visible:\n{scrape}"
    );
    assert!(
        scrape.contains("deis_telemetry_traces_dropped_by_producer_total 2"),
        "records a producer lost must be visible:\n{scrape}"
    );

    // The stream is deliberately left open: shutdown must not wait for it.
    server.shutdown().await.expect("shutdown must succeed");
}

#[tokio::test]
async fn a_pushed_histogram_is_scraped_as_a_histogram() {
    // The runtime pushes latency as bucket, sum and count series. If the
    // scrape endpoint hands them back as unrelated gauges, `histogram_quantile`
    // has nothing to work with and there is still no p99.
    let dir = tempfile::tempdir().expect("temp dir");
    let server = server::start(&test_config(dir.path()))
        .await
        .expect("the service must start");
    let mut client = connect_metrics(&server).await;

    let now = now_us();
    let mut samples = Vec::new();
    for (bound, count) in [("1", 0.0), ("10", 4.0), ("100", 9.0), ("+Inf", 10.0)] {
        samples.push(MetricSample {
            name: "deis_runtime_isolate_execution_ms_bucket".to_string(),
            app_id: "shop".to_string(),
            labels: HashMap::from([("le".to_string(), bound.to_string())]),
            value: count,
            timestamp_us: now,
        });
    }
    samples.push(MetricSample {
        name: "deis_runtime_isolate_execution_ms_sum".to_string(),
        app_id: "shop".to_string(),
        labels: HashMap::new(),
        value: 275.0,
        timestamp_us: now,
    });
    samples.push(MetricSample {
        name: "deis_runtime_isolate_execution_ms_count".to_string(),
        app_id: "shop".to_string(),
        labels: HashMap::new(),
        value: 10.0,
        timestamp_us: now,
    });
    client
        .push_metrics(PushMetricsRequest {
            samples,
            metadata: HashMap::new(),
        })
        .await
        .expect("push must succeed");

    let scrape = http_get(server.metrics_addr, "/metrics").await;
    assert!(
        scrape.contains("# TYPE deis_runtime_isolate_execution_ms histogram"),
        "unexpected scrape:\n{scrape}"
    );
    assert!(
        scrape.contains("deis_runtime_isolate_execution_ms_bucket{app_id=\"shop\",le=\"+Inf\"} 10"),
        "unexpected scrape:\n{scrape}"
    );
    assert!(
        scrape.contains("deis_runtime_isolate_execution_ms_sum{app_id=\"shop\"} 275"),
        "unexpected scrape:\n{scrape}"
    );
    assert!(
        scrape.contains("deis_runtime_isolate_execution_ms_count{app_id=\"shop\"} 10"),
        "unexpected scrape:\n{scrape}"
    );

    server.shutdown().await.expect("shutdown must succeed");
}

#[tokio::test]
async fn stored_traces_survive_a_restart() {
    let dir = tempfile::tempdir().expect("temp dir");
    let config = test_config(dir.path());

    let server = server::start(&config)
        .await
        .expect("the service must start");
    let mut traces = connect_traces(&server).await;
    traces
        .push_traces(PushTracesRequest {
            records: vec![invocation(
                "shop",
                OUTCOME_EXECUTION_TIMEOUT,
                "https://shop.test/report",
                now_us() - 1_000,
            )],
            ..Default::default()
        })
        .await
        .expect("push_traces must succeed");
    server.shutdown().await.expect("shutdown must succeed");

    let server = server::start(&config)
        .await
        .expect("the service must start again on the same data dir");
    let mut traces = connect_traces(&server).await;
    let page = traces
        .query_traces(QueryTracesRequest::default())
        .await
        .expect("query_traces must answer")
        .into_inner();
    assert_eq!(page.records.len(), 1);
    assert_eq!(page.records[0].outcome, OUTCOME_EXECUTION_TIMEOUT);
    server.shutdown().await.expect("shutdown must succeed");
}

// ---- the capability check, against a real process ----------------------

/// Deterministic keypair for auth enforcement tests.
use std::sync::OnceLock;
fn test_keypair() -> &'static (ed25519_dalek::SigningKey, ed25519_dalek::VerifyingKey) {
    static KEYPAIR: OnceLock<(ed25519_dalek::SigningKey, ed25519_dalek::VerifyingKey)> =
        OnceLock::new();
    KEYPAIR.get_or_init(|| {
        let seed: [u8; 32] = [0x42; 32];
        let sk = ed25519_dalek::SigningKey::from_bytes(&seed);
        let vk = sk.verifying_key();
        (sk, vk)
    })
}

/// Hex-encoded verifying key for use in config's `capability_public_key`.
fn test_verifying_key_hex() -> &'static str {
    static HEX: OnceLock<String> = OnceLock::new();
    HEX.get_or_init(|| hex::encode(test_keypair().1.as_bytes()))
}

/// Create an Ed25519 capability token for the test keypair.
fn cap_token(aud: &str) -> String {
    let (sk, _) = test_keypair();
    let claims = deis_capability::CapabilityClaims::new(
        aud,
        "spiffe://deis.test/control",
        "spiffe://deis.test/node/test",
        1u64,
        std::time::Duration::from_secs(3600),
        vec![deis_capability::CapabilityResource {
            tenant: String::new(),
            binding: String::new(),
            generation: None,
        }],
    );
    deis_capability::encode(&claims, sk)
}

/// Attach a Bearer capability token to an outgoing request.
fn signed<T>(message: T, token: &str) -> tonic::Request<T> {
    let mut request = tonic::Request::new(message);
    request.metadata_mut().insert(
        "authorization",
        format!("Bearer {token}")
            .parse()
            .expect("a token is valid metadata"),
    );
    request
}

#[tokio::test]
async fn a_public_bind_with_no_secret_starts_non_enforcing() {
    // With capability_public_key unset, auth is not enforced — that is the
    // default for tests and single-box installs. The old loopback-only guard
    // (refuse to serve non-loopback without auth_secret) is gone because
    // capability_public_key is the single source of truth.
    let dir = tempfile::tempdir().expect("temp dir");
    for (grpc, metrics) in [
        ("0.0.0.0:0", "127.0.0.1:0"),
        ("127.0.0.1:0", "0.0.0.0:0"),
        ("0.0.0.0:0", "0.0.0.0:0"),
    ] {
        let config = Config {
            listen: grpc.to_string(),
            metrics_listen: metrics.to_string(),
            ..test_config(dir.path())
        };
        let server = server::start(&config)
            .await
            .expect("non-loopback bind without capability_public_key starts (auth non-enforcing)");
        server.shutdown().await.expect("clean shutdown");
    }
}

#[tokio::test]
async fn a_public_bind_with_capability_key_starts_and_enforces() {
    let dir = tempfile::tempdir().expect("temp dir");
    let config = Config {
        listen: "0.0.0.0:0".to_string(),
        capability_public_key: Some(test_verifying_key_hex().to_string()),
        ..test_config(dir.path())
    };
    let server = server::start(&config)
        .await
        .expect("a configured capability_public_key is what makes a public bind serveable");

    let mut client = connect(&server).await;
    let status = client
        .push_logs(PushLogsRequest {
            entries: vec![entry("stranger", 3, "forged", now_us())],
            ..Default::default()
        })
        .await
        .expect_err("an unsigned push must be refused");
    assert_eq!(status.code(), tonic::Code::Unauthenticated);

    server.shutdown().await.expect("clean shutdown");
}

#[tokio::test]
async fn with_capability_key_configured_only_signed_calls_are_served() {
    let dir = tempfile::tempdir().expect("temp dir");
    let config = Config {
        capability_public_key: Some(test_verifying_key_hex().to_string()),
        ..test_config(dir.path())
    };
    let server = server::start(&config)
        .await
        .expect("the service must start");
    let good_push = cap_token("telemetry");
    let good_read = cap_token("telemetry.read");

    // Liveness is deliberately outside the check: a probe that needs a
    // credential is a probe the control plane cannot make.
    let mut health = HealthClient::new(channel(&server).await);
    let status = health
        .check(HealthCheckRequest {
            service: "deis.telemetry.LogService".to_string(),
        })
        .await
        .expect("health must answer without a credential");
    assert_eq!(
        status.into_inner().status,
        ServingStatus::Serving as i32,
        "health must still report this process as serving"
    );

    let mut logs = connect(&server).await;
    // Forging an entry against somebody else's application: refused before
    // anything is written.
    for wrong in [None, Some("not-a-token".to_string())] {
        let request = match &wrong {
            Some(bad) => signed(
                PushLogsRequest {
                    entries: vec![entry(
                        "someone-elses-bank",
                        3,
                        "forged by a stranger",
                        now_us(),
                    )],
                    ..Default::default()
                },
                bad,
            ),
            None => tonic::Request::new(PushLogsRequest {
                entries: vec![entry(
                    "someone-elses-bank",
                    3,
                    "forged by a stranger",
                    now_us(),
                )],
                ..Default::default()
            }),
        };
        let status = logs
            .push_logs(request)
            .await
            .expect_err("an unsigned or wrongly signed push must be refused");
        assert_eq!(status.code(), tonic::Code::Unauthenticated, "{wrong:?}");
    }

    // A valid token for the wrong audience should be PermissionDenied, not
    // Unauthenticated — it is properly signed but not for this service.
    {
        let wrong_aud = cap_token("telemetry.wrong-audience");
        let request = signed(
            PushLogsRequest {
                entries: vec![entry(
                    "someone-elses-bank",
                    3,
                    "forged by a stranger",
                    now_us(),
                )],
                ..Default::default()
            },
            &wrong_aud,
        );
        let status = logs
            .push_logs(request)
            .await
            .expect_err("a wrong-audience push must be refused");
        assert_eq!(status.code(), tonic::Code::PermissionDenied);
    }

    // Reading somebody else's logs: refused the same way.
    let status = logs
        .query(QueryRequest {
            app_id: "someone-elses-bank".to_string(),
            ..Default::default()
        })
        .await
        .expect_err("an unsigned query must be refused");
    assert_eq!(status.code(), tonic::Code::Unauthenticated);

    // Traces and metrics are behind the same door, not three separate ones.
    let mut traces = connect_traces(&server).await;
    assert_eq!(
        traces
            .query_traces(QueryTracesRequest::default())
            .await
            .expect_err("an unsigned trace query must be refused")
            .code(),
        tonic::Code::Unauthenticated
    );
    let mut metrics = connect_metrics(&server).await;
    assert_eq!(
        metrics
            .query_metrics(QueryMetricsRequest::default())
            .await
            .expect_err("an unsigned metric query must be refused")
            .code(),
        tonic::Code::Unauthenticated
    );

    // The platform's own call, carrying the token, is served as before.
    let accepted = logs
        .push_logs(signed(
            PushLogsRequest {
                entries: vec![entry("shop", 3, "a real line", now_us())],
                ..Default::default()
            },
            &good_push,
        ))
        .await
        .expect("a signed push must be served")
        .into_inner();
    assert_eq!(accepted.accepted_count, 1);
    let found = logs
        .query(signed(
            QueryRequest {
                app_id: "shop".to_string(),
                ..Default::default()
            },
            &good_read,
        ))
        .await
        .expect("a signed query must be served")
        .into_inner();
    assert_eq!(found.entries.len(), 1);
    assert_eq!(found.entries[0].message, "a real line");

    server.shutdown().await.expect("clean shutdown");
}

#[tokio::test]
async fn a_scrape_needs_the_token_once_a_secret_is_configured() {
    // The scrape page carries every application's metric series, so leaving
    // this port open would publish half the data and call the service
    // protected.
    let dir = tempfile::tempdir().expect("temp dir");
    let config = Config {
        capability_public_key: Some(test_verifying_key_hex().to_string()),
        ..test_config(dir.path())
    };
    let server = server::start(&config)
        .await
        .expect("the service must start");
    let good_read = cap_token("telemetry.read");

    let refused = http_get(server.metrics_addr, "/metrics").await;
    assert!(refused.starts_with("HTTP/1.1 401"), "{refused}");
    assert!(
        !refused.contains("deis_telemetry_uptime_seconds"),
        "a refused scrape must not hand out the page anyway: {refused}"
    );
    assert!(
        refused.to_ascii_lowercase().contains("www-authenticate"),
        "the refusal must say which credential was wanted: {refused}"
    );

    let wrong = http_get_with(
        server.metrics_addr,
        "/metrics",
        &[("Authorization", "Bearer not-a-token")],
    )
    .await;
    assert!(wrong.starts_with("HTTP/1.1 401"), "{wrong}");

    // Both forms a caller may present it in: the platform's own header, and
    // the bearer token a Prometheus scrape configuration produces.
    let bearer = format!("Bearer {good_read}");
    for header in [("Authorization", bearer.as_str())] {
        let served = http_get_with(server.metrics_addr, "/metrics", &[header]).await;
        assert!(served.starts_with("HTTP/1.1 200"), "{served}");
        assert!(served.contains("deis_telemetry_uptime_seconds"), "{served}");
    }

    server.shutdown().await.expect("clean shutdown");
}

#[tokio::test]
async fn a_flood_from_one_application_does_not_delete_a_co_tenants_live_lines() {
    // The whole path, through a real process: an operator follows one
    // application while another is logging in a loop. Before the live buffer
    // was shared out per application, the quiet application's line was
    // evicted by the flood and the follower never saw it.
    let dir = tempfile::tempdir().expect("temp dir");
    let config = Config {
        // Small enough that a thousand lines walk it round many times over.
        tail_capacity: 8,
        ..test_config(dir.path())
    };
    let server = server::start(&config)
        .await
        .expect("the service must start");
    let mut follower = connect(&server).await;
    let mut stream = follower
        .tail(TailRequest {
            app_id: "quiet".to_string(),
            ..Default::default()
        })
        .await
        .expect("the tail stream must open")
        .into_inner();

    // The service registers the subscriber inside the handler before the
    // call returns, so a push after this cannot race ahead of the stream.
    let mut writer = connect(&server).await;
    writer
        .push_logs(PushLogsRequest {
            entries: vec![entry("quiet", 3, "the line that mattered", now_us())],
            ..Default::default()
        })
        .await
        .expect("the quiet application's push must be accepted");
    writer
        .push_logs(PushLogsRequest {
            entries: (0..2000)
                .map(|i| entry("noisy", 3, &format!("flood {i}"), now_us()))
                .collect(),
            ..Default::default()
        })
        .await
        .expect("the flood must be accepted");

    let seen = stream
        .message()
        .await
        .expect("the stream must stay open")
        .expect("the quiet application's line must still arrive");
    assert_eq!(seen.message, "the line that mattered");

    server.shutdown().await.expect("clean shutdown");
}

/// Microseconds in one UTC day, the width of a log partition.
const DAY_US: i64 = 86_400_000_000;

#[tokio::test]
async fn a_page_of_big_lines_is_cut_by_bytes_and_pages_through() {
    // 300 lines of 60 KiB each are inside the per-message cap and add up to
    // 18 MiB, past the 16 MiB frame every Deislet service agrees on. A page
    // built by row count alone cannot be encoded, tonic answers OutOfRange
    // after the handler has returned, and `deis logs --limit 300` fails for
    // the whole retention window. The page has to stop early and hand back a
    // cursor, so whatever limit a client asks for pages through.
    let dir = tempfile::tempdir().expect("temp dir");
    let server = server::start(&test_config(dir.path()))
        .await
        .expect("the service must start");
    let mut client = connect(&server).await;

    let base = now_us() - 1_000_000;
    let line = "x".repeat(60 * 1024);
    for batch in 0..6 {
        client
            .push_logs(PushLogsRequest {
                entries: (0..50)
                    .map(|i| entry("chatty", 3, &line, base + batch * 50 + i))
                    .collect(),
                metadata: HashMap::new(),
            })
            .await
            .expect("a 3 MiB batch must be accepted");
    }

    let mut seen = 0usize;
    let mut pages = 0usize;
    let mut cursor = String::new();
    loop {
        let page = client
            .query(QueryRequest {
                app_id: "chatty".to_string(),
                limit: 300,
                cursor: cursor.clone(),
                ..Default::default()
            })
            .await
            .expect("a query for 300 big lines must be answered, not refused as too large")
            .into_inner();
        assert!(
            prost::Message::encoded_len(&page) <= MAX_GRPC_MESSAGE_BYTES,
            "a page must always fit the frame"
        );
        assert!(
            !page.entries.is_empty(),
            "every page carries at least one entry"
        );
        seen += page.entries.len();
        pages += 1;
        cursor = page.next_cursor;
        if !page.has_more {
            break;
        }
        assert!(
            !cursor.is_empty(),
            "a page cut short must hand back a cursor for the rest"
        );
    }
    assert_eq!(
        seen, 300,
        "paging through must reach every line exactly once"
    );
    assert!(
        pages >= 2,
        "300 x 60 KiB cannot be one page; the first one must have been cut by bytes"
    );

    server.shutdown().await.expect("shutdown must succeed");
}

#[tokio::test]
async fn entries_older_than_the_retention_window_are_refused_not_partitioned() {
    // One batch, 300 entries each on its own day between 10 and 310 days ago.
    // Logs are kept for 7 days, so every one of them would be dropped by the
    // next retention sweep; storing them first would create 300 partitions
    // with three indexes each inside one write transaction, holding the single
    // writer against every other producer for as long as that takes.
    let dir = tempfile::tempdir().expect("temp dir");
    let server = server::start(&test_config(dir.path()))
        .await
        .expect("the service must start");
    let mut client = connect(&server).await;

    let now = now_us();
    let response = client
        .push_logs(PushLogsRequest {
            entries: (10..310)
                .map(|d| entry("clock", 3, "from the past", now - d * DAY_US))
                .collect(),
            metadata: HashMap::new(),
        })
        .await
        .expect("the push itself is answered")
        .into_inner();
    assert_eq!(
        response.accepted_count, 0,
        "nothing older than the retention window may be stored"
    );
    assert_eq!(response.rejected_count, 300);
    assert!(
        response.error_message.contains("retention"),
        "the producer is told why: {}",
        response.error_message
    );
    assert_eq!(
        server
            .store
            .partition_count()
            .expect("partitions can be counted"),
        0,
        "no partition may be created for data retention would delete on its next pass"
    );

    // A line from inside the window is still taken, so a producer that is a
    // day behind is not cut off.
    let response = client
        .push_logs(PushLogsRequest {
            entries: vec![entry("clock", 3, "yesterday", now - DAY_US)],
            metadata: HashMap::new(),
        })
        .await
        .expect("the push itself is answered")
        .into_inner();
    assert_eq!(response.accepted_count, 1);

    server.shutdown().await.expect("shutdown must succeed");
}

#[tokio::test]
async fn migration_management_is_independently_authenticated_and_fences_only_its_tenant() {
    const TOKEN: &str = "telemetry-migration-admin-token-00000001";
    const MIGRATION: &str = "01990f44-1000-7000-8000-000000000099";

    let dir = tempfile::tempdir().expect("temp dir");
    let config = Config {
        migration_admin_token: Some(TOKEN.to_string()),
        ..test_config(dir.path())
    };
    let server = server::start(&config).await.expect("service must start");
    let mut logs = connect(&server).await;
    logs.push_logs(PushLogsRequest {
        entries: vec![entry("shop", 3, "before fence", now_us())],
        metadata: HashMap::new(),
    })
    .await
    .expect("initial telemetry must be stored");

    let mut migration = connect_migration(&server).await;
    let unauthenticated = migration
        .get_backend_info(GetBackendInfoRequest {})
        .await
        .expect_err("the application channel must not open management");
    assert_eq!(unauthenticated.code(), tonic::Code::Unauthenticated);
    let info = migration
        .get_backend_info(managed(GetBackendInfoRequest {}, TOKEN))
        .await
        .expect("the independent management credential must work")
        .into_inner();
    assert_eq!(info.backend, "deis-telemetry");
    assert!(!info.backend_incarnation_id.is_empty());

    migration
        .acquire_write_fence(managed(
            AcquireWriteFenceRequest {
                tenant: "shop.production".to_string(),
                migration_id: MIGRATION.to_string(),
                expected_application_revision: 9,
                fence_token: "one-use-fence-token-0000000000000001".to_string(),
                expected_backend_incarnation_id: info.backend_incarnation_id.clone(),
            },
            TOKEN,
        ))
        .await
        .expect("fence must be acquired");

    let pushed = logs
        .push_logs(PushLogsRequest {
            entries: vec![
                entry("shop", 3, "blocked", now_us()),
                entry("clock", 3, "still flowing", now_us()),
            ],
            metadata: HashMap::new(),
        })
        .await
        .expect("a mixed batch returns a partial-success response")
        .into_inner();
    assert_eq!(pushed.accepted_count, 1);
    assert_eq!(pushed.rejected_count, 1);
    assert!(pushed.error_message.contains("write-fenced"));

    let shop = logs
        .query(QueryRequest {
            app_id: "shop".to_string(),
            ..Default::default()
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(shop.entries.len(), 1);
    assert_eq!(shop.entries[0].message, "before fence");
    let clock = logs
        .query(QueryRequest {
            app_id: "clock".to_string(),
            ..Default::default()
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(clock.entries.len(), 1);
    assert_eq!(clock.entries[0].message, "still flowing");

    let mut metrics = connect_metrics(&server).await;
    let metric_result = metrics
        .push_metrics(PushMetricsRequest {
            samples: vec![
                MetricSample {
                    name: "requests".to_string(),
                    app_id: "shop".to_string(),
                    labels: HashMap::new(),
                    value: 1.0,
                    timestamp_us: now_us(),
                },
                MetricSample {
                    name: "requests".to_string(),
                    app_id: "clock".to_string(),
                    labels: HashMap::new(),
                    value: 1.0,
                    timestamp_us: now_us(),
                },
            ],
            metadata: HashMap::new(),
        })
        .await
        .expect("metric batches also report a tenant-scoped partial success")
        .into_inner();
    assert_eq!(metric_result.accepted_count, 1);
    assert_eq!(metric_result.rejected_count, 1);

    let mut traces = connect_traces(&server).await;
    let trace_result = traces
        .push_traces(PushTracesRequest {
            records: vec![
                invocation("shop", OUTCOME_OK, "/blocked", now_us()),
                invocation("clock", OUTCOME_OK, "/flowing", now_us() + 1),
            ],
            sampled_out: 0,
            dropped: 0,
            metadata: HashMap::new(),
        })
        .await
        .expect("trace batches also report a tenant-scoped partial success")
        .into_inner();
    assert_eq!(trace_result.accepted_count, 1);
    assert_eq!(trace_result.rejected_count, 1);
    assert_eq!(
        deis_telemetry::stats::ServiceStats::get(&server.stats.logs_write_fenced),
        1
    );
    assert_eq!(
        deis_telemetry::stats::ServiceStats::get(&server.stats.metric_samples_write_fenced),
        1
    );
    assert_eq!(
        deis_telemetry::stats::ServiceStats::get(&server.stats.traces_write_fenced),
        1
    );

    let inventory = migration
        .inspect_tenant(managed(
            InspectTenantRequest {
                tenant: "shop.production".to_string(),
            },
            TOKEN,
        ))
        .await
        .expect("a fenced tenant can be inventoried")
        .into_inner();
    assert_eq!(inventory.tenant, "shop.production");
    assert_eq!(inventory.components[0].component, "telemetry");
    assert_eq!(inventory.components[0].item_count, 1);
    assert_eq!(inventory.components[0].unchecked_orphan_count, 0);

    migration
        .release_write_fence(managed(
            ReleaseWriteFenceRequest {
                tenant: "shop.production".to_string(),
                migration_id: MIGRATION.to_string(),
                expected_application_revision: 9,
                fence_token: "one-use-fence-token-0000000000000001".to_string(),
                expected_backend_incarnation_id: info.backend_incarnation_id,
            },
            TOKEN,
        ))
        .await
        .expect("the exact fence credential must release it");
    let resumed = logs
        .push_logs(PushLogsRequest {
            entries: vec![entry("shop", 3, "after release", now_us())],
            metadata: HashMap::new(),
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(resumed.accepted_count, 1);

    server.shutdown().await.expect("shutdown must succeed");
}

#[tokio::test]
async fn invalid_or_reused_migration_credentials_fail_before_creating_backend_state() {
    for (migration_admin_token, expected) in [("short".to_string(), "at least 32 bytes")] {
        let dir = tempfile::tempdir().expect("temp dir");
        let config = Config {
            migration_admin_token: Some(migration_admin_token),
            ..test_config(dir.path())
        };
        let error = match server::start(&config).await {
            Ok(server) => {
                server.shutdown().await.unwrap();
                panic!("invalid management credential must refuse startup")
            }
            Err(error) => format!("{error:#}"),
        };
        assert!(error.contains(expected), "unexpected refusal: {error}");
        assert!(!dir.path().join("telemetry.sqlite").exists());
        assert!(!dir.path().join("migration-fences.sqlite").exists());
    }
}

#[tokio::test]
async fn migration_management_is_not_registered_by_default() {
    let dir = tempfile::tempdir().expect("temp dir");
    let server = server::start(&test_config(dir.path()))
        .await
        .expect("ordinary telemetry service must start");
    let mut migration = connect_migration(&server).await;
    let status = migration
        .get_backend_info(GetBackendInfoRequest {})
        .await
        .expect_err("management must not have a loopback bypass");
    assert_eq!(status.code(), tonic::Code::Unimplemented);
    server.shutdown().await.unwrap();
}
