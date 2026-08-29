//! End-to-end exercise of a real deis-telemetry process: the service is
//! started against a temporary data directory on ephemeral ports, then driven
//! through its own gRPC client and a plain HTTP scrape.

use std::collections::HashMap;
use std::net::SocketAddr;

use deis_telemetry::config::Config;
use deis_telemetry::proto::log_service_client::LogServiceClient;
use deis_telemetry::proto::metric_service_client::MetricServiceClient;
use deis_telemetry::proto::{
    LogEntry, MetricSample, PushLogsRequest, PushMetricsRequest, QueryMetricsRequest, QueryRequest,
    TailRequest,
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
    let mut stream = TcpStream::connect(addr)
        .await
        .expect("the metrics listener must accept the connection");
    let request = format!("GET {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n");
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
