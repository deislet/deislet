//! End-to-end checks against a real deis-queue server.
//!
//! Everything here goes over a real TCP socket and the real generated client,
//! so it exercises what the unit tests cannot: the tenant metadata header, the
//! message size ceiling, the fact that the service is actually registered on
//! the server, and the lease and dead letter behaviour a consumer will
//! actually meet.
//!
//! The two tests that wait for a lease to lapse use a one-second lease and
//! poll for the message to reappear, under a hard deadline. They wait on the
//! event itself - the message becoming visible again - rather than assuming a
//! fixed pause is enough, so a broken lease fails the test instead of making
//! it flaky.

use deis_queue::config::Config;
use deis_queue::proto::queue::{
    AckRequest, NackRequest, PullRequest, QueueMessage, SendBatchRequest, SendRequest,
    queue_client::QueueClient,
};
use deis_queue::tenant::APP_ID_HEADER;
use deis_queue::{MAX_GRPC_MESSAGE_BYTES, server};
use std::time::Duration;
use tempfile::TempDir;
use tokio::sync::oneshot;
use tonic::transport::Channel;
use tonic::{Code, Request};

/// The attempt ceiling every test here runs against. Two is the smallest
/// number that still shows a retry happening before the message is retired.
const MAX_ATTEMPTS: i32 = 2;

/// Largest body the test server accepts. Small enough that the ceiling is
/// reachable without building a megabyte.
const MAX_BODY_BYTES: usize = 1024;

/// How long a test will wait for a lapsed lease before failing.
const LEASE_DEADLINE: Duration = Duration::from_secs(15);

/// A running server plus the temp directory holding its data.
struct Harness {
    _data: TempDir,
    endpoint: String,
    shutdown: Option<oneshot::Sender<()>>,
}

impl Harness {
    async fn start() -> Self {
        let data = TempDir::new().unwrap();
        let config = Config {
            listen: "127.0.0.1:0".to_string(),
            data_dir: data.path().to_path_buf(),
            max_attempts: MAX_ATTEMPTS,
            max_body_bytes: MAX_BODY_BYTES,
            ..Config::default()
        };
        // Bind first, so the port is known before anything is spawned and no
        // test has to wait for the server to come up.
        let listener = server::bind(&config).await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (shutdown, rx) = oneshot::channel();
        tokio::spawn(async move {
            server::serve(config, listener, async {
                let _ = rx.await;
            })
            .await
            .unwrap();
        });
        Self {
            _data: data,
            endpoint: format!("http://{addr}"),
            shutdown: Some(shutdown),
        }
    }

    async fn client(&self) -> QueueClient<Channel> {
        let channel = Channel::from_shared(self.endpoint.clone())
            .unwrap()
            .connect()
            .await
            .unwrap();
        QueueClient::new(channel)
            .max_decoding_message_size(MAX_GRPC_MESSAGE_BYTES)
            .max_encoding_message_size(MAX_GRPC_MESSAGE_BYTES)
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
    }
}

/// Stamp the tenant the way deis-runtime has to.
fn as_app<T>(app: &str, message: T) -> Request<T> {
    let mut request = Request::new(message);
    request
        .metadata_mut()
        .insert(APP_ID_HEADER, app.parse().unwrap());
    request
}

async fn send(client: &mut QueueClient<Channel>, app: &str, queue: &str, body: &str) -> String {
    let response = client
        .send(as_app(
            app,
            SendRequest {
                queue_name: queue.to_string(),
                message_body: body.to_string(),
            },
        ))
        .await
        .expect("send should be accepted")
        .into_inner();
    assert!(response.success);
    assert!(response.error.is_empty());
    assert!(!response.message_id.is_empty(), "a send must return an id");
    response.message_id
}

async fn pull(
    client: &mut QueueClient<Channel>,
    app: &str,
    queue: &str,
    max_messages: i32,
    lease_seconds: i32,
) -> Vec<QueueMessage> {
    client
        .pull(as_app(
            app,
            PullRequest {
                queue_name: queue.to_string(),
                max_messages,
                lease_seconds,
            },
        ))
        .await
        .expect("pull should be accepted")
        .into_inner()
        .messages
}

/// Poll until the queue hands something back, or fail at the deadline.
///
/// This is how a lapsed lease is observed without guessing at a pause: the
/// loop watches for the message to become visible again, and the deadline
/// turns a lease that never lapses into a failure rather than a hang.
async fn pull_until_delivered(
    client: &mut QueueClient<Channel>,
    app: &str,
    queue: &str,
) -> Vec<QueueMessage> {
    let deadline = tokio::time::Instant::now() + LEASE_DEADLINE;
    loop {
        let messages = pull(client, app, queue, 10, 30).await;
        if !messages.is_empty() {
            return messages;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "nothing was redelivered within {LEASE_DEADLINE:?}; the lease never lapsed"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

async fn ack(client: &mut QueueClient<Channel>, app: &str, queue: &str, ids: Vec<String>) -> i32 {
    client
        .ack(as_app(
            app,
            AckRequest {
                queue_name: queue.to_string(),
                message_ids: ids,
            },
        ))
        .await
        .expect("ack should be accepted")
        .into_inner()
        .acknowledged
}

async fn nack(
    client: &mut QueueClient<Channel>,
    app: &str,
    queue: &str,
    ids: Vec<String>,
    delay_seconds: i32,
) -> (i32, i32) {
    let response = client
        .nack(as_app(
            app,
            NackRequest {
                queue_name: queue.to_string(),
                message_ids: ids,
                delay_seconds,
            },
        ))
        .await
        .expect("nack should be accepted")
        .into_inner();
    (response.released, response.dead_lettered)
}

#[tokio::test]
async fn send_pull_ack_and_the_message_is_gone() {
    let harness = Harness::start().await;
    let mut client = harness.client().await;

    let id = send(&mut client, "app-one", "jobs", "work").await;

    let leased = pull(&mut client, "app-one", "jobs", 10, 30).await;
    assert_eq!(leased.len(), 1);
    assert_eq!(leased[0].id, id);
    assert_eq!(leased[0].body, "work");
    assert_eq!(leased[0].attempts, 1);
    assert!(
        leased[0].lease_expires_at >= leased[0].created_at + 30,
        "the lease should run 30 seconds past now"
    );

    assert_eq!(ack(&mut client, "app-one", "jobs", vec![id]).await, 1);

    // Nothing is left, and the empty answer comes back immediately rather
    // than the server holding the call open waiting for work.
    let empty = tokio::time::timeout(
        Duration::from_secs(5),
        pull(&mut client, "app-one", "jobs", 10, 30),
    )
    .await
    .expect("an empty pull must answer at once, not wait for work");
    assert!(empty.is_empty());
}

#[tokio::test]
async fn a_batch_is_sent_and_pulled_in_order() {
    let harness = Harness::start().await;
    let mut client = harness.client().await;

    let ids = client
        .send_batch(as_app(
            "app-one",
            SendBatchRequest {
                queue_name: "jobs".to_string(),
                messages: vec!["one".to_string(), "two".to_string(), "three".to_string()],
            },
        ))
        .await
        .unwrap()
        .into_inner()
        .message_ids;
    assert_eq!(ids.len(), 3);

    let leased = pull(&mut client, "app-one", "jobs", 10, 30).await;
    let bodies: Vec<&str> = leased.iter().map(|m| m.body.as_str()).collect();
    assert_eq!(bodies, vec!["one", "two", "three"]);
}

#[tokio::test]
async fn a_message_whose_lease_lapses_is_delivered_again() {
    let harness = Harness::start().await;
    let mut client = harness.client().await;
    let id = send(&mut client, "app-one", "jobs", "work").await;

    // Lease it and then behave like a consumer that died: never ack, never
    // nack, just stop. Three seconds, not one: the stored lease is a whole
    // second, so a one-second lease taken at the end of a second could lapse
    // before the very next line runs.
    let first = pull(&mut client, "app-one", "jobs", 10, 3).await;
    assert_eq!(first.len(), 1);
    assert_eq!(first[0].attempts, 1);
    // Inside the lease nobody else can see it.
    assert!(
        pull(&mut client, "app-one", "jobs", 10, 30)
            .await
            .is_empty(),
        "a leased message must be invisible while its lease holds"
    );

    let again = pull_until_delivered(&mut client, "app-one", "jobs").await;
    assert_eq!(again.len(), 1);
    assert_eq!(again[0].id, id);
    assert_eq!(again[0].attempts, 2, "the redelivery is a second attempt");
    assert_eq!(again[0].body, "work");
}

#[tokio::test]
async fn a_nack_with_a_delay_holds_the_message_back_then_returns_it() {
    let harness = Harness::start().await;
    let mut client = harness.client().await;
    let id = send(&mut client, "app-one", "jobs", "work").await;

    let leased = pull(&mut client, "app-one", "jobs", 10, 30).await;
    assert_eq!(leased.len(), 1);

    assert_eq!(
        nack(&mut client, "app-one", "jobs", vec![id.clone()], 3).await,
        (1, 0),
        "an attempt is still left, so the message is released rather than retired"
    );
    assert!(
        pull(&mut client, "app-one", "jobs", 10, 30)
            .await
            .is_empty(),
        "the delay must actually hold the message back"
    );

    let back = pull_until_delivered(&mut client, "app-one", "jobs").await;
    assert_eq!(back.len(), 1);
    assert_eq!(back[0].id, id);
    assert_eq!(back[0].attempts, 2);
}

#[tokio::test]
async fn a_message_that_keeps_failing_is_dead_lettered_and_never_seen_again() {
    let harness = Harness::start().await;
    let mut client = harness.client().await;
    let id = send(&mut client, "app-one", "jobs", "poison").await;

    for attempt in 1..=MAX_ATTEMPTS {
        let leased = pull(&mut client, "app-one", "jobs", 10, 30).await;
        assert_eq!(leased.len(), 1, "attempt {attempt} should be delivered");
        assert_eq!(leased[0].attempts, attempt);
        let outcome = nack(&mut client, "app-one", "jobs", vec![id.clone()], 0).await;
        if attempt < MAX_ATTEMPTS {
            assert_eq!(outcome, (1, 0), "attempt {attempt} should be retried");
        } else {
            assert_eq!(outcome, (0, 1), "the last attempt should dead letter it");
        }
    }

    assert!(
        pull(&mut client, "app-one", "jobs", 10, 30)
            .await
            .is_empty(),
        "a dead lettered message must never be delivered again"
    );
    // The queue still works for everything else.
    let next = send(&mut client, "app-one", "jobs", "healthy").await;
    let leased = pull(&mut client, "app-one", "jobs", 10, 30).await;
    assert_eq!(leased.len(), 1);
    assert_eq!(leased[0].id, next);
}

#[tokio::test]
async fn one_app_cannot_see_or_touch_another_apps_messages() {
    let harness = Harness::start().await;
    let mut client = harness.client().await;
    let id = send(&mut client, "app-one", "jobs", "private").await;

    // Same queue name, different tenant: nothing.
    assert!(
        pull(&mut client, "app-two", "jobs", 10, 30)
            .await
            .is_empty()
    );
    // Nor can the other app acknowledge or return it.
    assert_eq!(
        ack(&mut client, "app-two", "jobs", vec![id.clone()]).await,
        0
    );
    assert_eq!(
        nack(&mut client, "app-two", "jobs", vec![id.clone()], 0).await,
        (0, 0)
    );

    // The owner still has it, untouched and on its first attempt.
    let leased = pull(&mut client, "app-one", "jobs", 10, 30).await;
    assert_eq!(leased.len(), 1);
    assert_eq!(leased[0].id, id);
    assert_eq!(leased[0].attempts, 1);
}

#[tokio::test]
async fn a_call_without_the_tenant_header_is_refused() {
    let harness = Harness::start().await;
    let mut client = harness.client().await;

    let status = client
        .send(Request::new(SendRequest {
            queue_name: "jobs".to_string(),
            message_body: "work".to_string(),
        }))
        .await
        .expect_err("a call with no tenant must be refused");
    assert_eq!(status.code(), Code::PermissionDenied);

    let status = client
        .pull(Request::new(PullRequest {
            queue_name: "jobs".to_string(),
            max_messages: 10,
            lease_seconds: 30,
        }))
        .await
        .expect_err("a call with no tenant must be refused");
    assert_eq!(status.code(), Code::PermissionDenied);
}

#[tokio::test]
async fn an_oversized_message_is_refused_and_nothing_is_queued() {
    let harness = Harness::start().await;
    let mut client = harness.client().await;

    let status = client
        .send(as_app(
            "app-one",
            SendRequest {
                queue_name: "jobs".to_string(),
                message_body: "x".repeat(MAX_BODY_BYTES + 1),
            },
        ))
        .await
        .expect_err("an oversized body must be refused");
    assert_eq!(status.code(), Code::ResourceExhausted);
    assert!(
        pull(&mut client, "app-one", "jobs", 10, 30)
            .await
            .is_empty()
    );
}

#[tokio::test]
async fn a_message_at_the_size_ceiling_still_round_trips() {
    let harness = Harness::start().await;
    let mut client = harness.client().await;
    let body = "x".repeat(MAX_BODY_BYTES);
    let id = send(&mut client, "app-one", "jobs", &body).await;

    let leased = pull(&mut client, "app-one", "jobs", 10, 30).await;
    assert_eq!(leased.len(), 1);
    assert_eq!(leased[0].id, id);
    assert_eq!(leased[0].body.len(), MAX_BODY_BYTES);
}

/// Two consumers of the same queue, polling at the same moment. Under a
/// deferred transaction one of them would be refused outright; under the
/// lease neither may see the other's message.
#[tokio::test]
async fn concurrent_consumers_never_receive_the_same_message() {
    let harness = Harness::start().await;
    let mut producer = harness.client().await;
    for i in 0..40 {
        send(&mut producer, "app-one", "jobs", &format!("m{i}")).await;
    }

    let mut consumers = Vec::new();
    for _ in 0..8 {
        let mut client = harness.client().await;
        consumers.push(tokio::spawn(async move {
            pull(&mut client, "app-one", "jobs", 5, 60).await
        }));
    }

    let mut seen = std::collections::HashSet::new();
    let mut total = 0;
    for consumer in consumers {
        for message in consumer.await.unwrap() {
            total += 1;
            assert!(
                seen.insert(message.id.clone()),
                "delivered to two consumers"
            );
            assert_eq!(message.attempts, 1);
        }
    }
    assert_eq!(
        total, 40,
        "every message should have been leased exactly once"
    );
}
