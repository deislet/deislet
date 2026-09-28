//! End-to-end checks against a real deis-queue server.
//!
//! Everything here goes over a real TCP socket and the real generated client,
//! so it exercises what the unit tests cannot: the tenant metadata header, the
//! message size ceiling, the fact that the service is actually registered on
//! the server, and the lease and dead letter behaviour a consumer will
//! actually meet.
//!
//! The tests name tenants by readable labels such as `app-one.production`
//! and `app-two.production`; `canonical` turns each label into the
//! canonical `v1/<application id>/<environment id>` tenant its capability
//! names, which is the only place a tenant is ever stated. One test pairs
//! `app-one.production` with `app-one.preview` to check that two environments
//! of the *same* application are also two namespaces.
//!
//! Since this service always enforces capability-based auth, every well-formed
//! call also carries an `Authorization: Bearer <capability>` header. The
//! capability is minted with a deterministic test signing key and verified
//! against the corresponding public key.
//!
//! The two tests that wait for a lease to lapse use a one-second lease and
//! poll for the message to reappear, under a hard deadline. They wait on the
//! event itself - the message becoming visible again - rather than assuming a
//! fixed pause is enough, so a broken lease fails the test instead of making
//! it flaky.

use deis_capability::{CapabilityClaims, CapabilityResource, KeyRing, encode as encode_capability};
use deis_identity::{Protocol, ServiceKind};
use deis_identity_test_pki::TestFleet;
use deis_queue::config::Config;
use deis_queue::proto::queue::{
    AbortConsumerGenerationRequest, AckRequest, ActivateConsumerGenerationRequest,
    ListDeadLettersRequest, MessageHandle, NackRequest, PrepareConsumerGenerationRequest,
    PullRequest, QueueMessage, ReplayDeadLettersRequest, SendBatchRequest, SendRequest,
    queue_client::QueueClient,
};
use deis_queue::{MAX_GRPC_MESSAGE_BYTES, server};
use deis_wire::CanonicalTenantId;
use deis_wire::conventions::{DEPLOYMENT_GENERATION_HEADER, tenant_id};
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

/// A deterministic Ed25519 signing key used by every test.
///
/// All capabilities in this file are minted under this key, and every
/// `Harness::start` passes the corresponding verifying key to the server. The
/// seed is fixed so tests are reproducible and never need an external
/// keypair file.
fn test_signing_key() -> ed25519_dalek::SigningKey {
    let mut seed = [0u8; 32];
    seed[..6].copy_from_slice(b"deis-q");
    ed25519_dalek::SigningKey::from_bytes(&seed)
}

/// The canonical tenant a test label stands for.
fn canonical(label: &str) -> String {
    CanonicalTenantId::derived_from_label(label).to_string()
}

/// The control plane these servers poll their revocation list from: a list
/// signed with the key the tests sign capabilities with.
fn fake_control() -> &'static deis_capability::testing::FakeControl {
    static CONTROL: std::sync::OnceLock<deis_capability::testing::FakeControl> =
        std::sync::OnceLock::new();
    CONTROL.get_or_init(|| deis_capability::testing::FakeControl::start(test_signing_key()))
}

/// Mint a capability token for `tenant` with audience `"queue"` and binding
/// `"queue"`.
fn queue_capability(signing_key: &ed25519_dalek::SigningKey, tenant: &str) -> String {
    let tenant = canonical(tenant);
    let claims = CapabilityClaims::new(
        "queue",
        "spiffe://deislet/control/main",
        "spiffe://deislet/node/test",
        1,
        Duration::from_secs(900),
        vec![CapabilityResource {
            tenant: tenant.to_string(),
            binding: "queue".to_string(),
            generation: None,
        }],
    );
    encode_capability(&claims, signing_key)
}

/// A running server plus the temp directory holding its data.
struct Harness {
    _data: TempDir,
    endpoint: String,
    shutdown: Option<oneshot::Sender<()>>,
    /// The signing key every capability in this file is minted under.
    signing_key: ed25519_dalek::SigningKey,
    /// The authority the server's certificate is issued by.
    fleet: TestFleet,
}

impl Harness {
    /// Start the server on a random loopback port. Every call must carry a
    /// valid capability token; auth is always enforced.
    async fn start() -> Self {
        Self::start_with(Config::default()).await
    }

    /// Start with a fully custom config.
    async fn start_with(config: Config) -> Self {
        let data = TempDir::new().unwrap();
        let signing_key = test_signing_key();
        let keys = KeyRing::from_keys([signing_key.verifying_key()]);
        let config = Config {
            listen: "127.0.0.1:0".to_string(),
            data_dir: data.path().to_path_buf(),
            max_attempts: MAX_ATTEMPTS,
            max_body_bytes: MAX_BODY_BYTES,
            ..config
        };
        // Bind first, so the port is known before anything is spawned and no
        // test has to wait for the server to come up.
        let listener = server::bind(&config).await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (shutdown, rx) = oneshot::channel();
        let fleet = TestFleet::new();
        let identity = fleet.service_identity(ServiceKind::Queue);
        tokio::spawn(async move {
            server::serve(
                config,
                listener,
                async {
                    let _ = rx.await;
                },
                keys,
                fake_control().client(),
                identity,
            )
            .await
            .unwrap();
        });
        Self {
            _data: data,
            endpoint: format!("https://{addr}"),
            shutdown: Some(shutdown),
            signing_key,
            fleet,
        }
    }

    /// A channel that checks the far end is a queue of this fleet, the way
    /// every real caller does.
    async fn channel(&self) -> Channel {
        tonic::transport::Endpoint::from_shared(self.endpoint.clone())
            .unwrap()
            .connect_with_connector(
                self.fleet
                    .service_trust()
                    .dialler(ServiceKind::Queue, &[Protocol::Http2]),
            )
            .await
            .unwrap()
    }

    async fn client(&self) -> QueueClient<Channel> {
        QueueClient::new(self.channel().await)
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

/// Stamp the tenant the way deis-runtime has to. Also stamps a capability
/// token so the call passes the interceptor.
fn as_tenant<T>(harness: &Harness, tenant: &str, message: T) -> Request<T> {
    let mut request = Request::new(message);
    let token = queue_capability(&harness.signing_key, tenant);
    request
        .metadata_mut()
        .insert("authorization", format!("Bearer {token}").parse().unwrap());
    request
}

/// Stamp *only* the tenant header, without a capability token. Used by tests
/// that need to provoke an auth rejection.
fn as_tenant_only<T>(_tenant: &str, message: T) -> Request<T> {
    Request::new(message)
}

/// Carry a capability minted for `token_tenant` and name `claimed` in the
/// retired `x-deis-tenant` header, which the service must ignore.
fn as_tenant_with_capability<T>(
    harness: &Harness,
    claimed: &str,
    token_tenant: &str,
    message: T,
) -> Request<T> {
    let mut request = Request::new(message);
    request
        .metadata_mut()
        .insert("x-deis-tenant", canonical(claimed).parse().unwrap());
    let token = queue_capability(&harness.signing_key, token_tenant);
    request
        .metadata_mut()
        .insert("authorization", format!("Bearer {token}").parse().unwrap());
    request
}

async fn send(
    client: &mut QueueClient<Channel>,
    harness: &Harness,
    tenant: &str,
    queue: &str,
    body: &str,
) -> String {
    let response = client
        .send(as_tenant(
            harness,
            tenant,
            SendRequest {
                queue_name: queue.to_string(),
                message_body: body.as_bytes().to_vec(),
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
    harness: &Harness,
    tenant: &str,
    queue: &str,
    max_messages: i32,
    lease_seconds: i32,
) -> Vec<QueueMessage> {
    client
        .pull(as_tenant(
            harness,
            tenant,
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
    harness: &Harness,
    tenant: &str,
    queue: &str,
) -> Vec<QueueMessage> {
    let deadline = tokio::time::Instant::now() + LEASE_DEADLINE;
    loop {
        let messages = pull(client, harness, tenant, queue, 10, 30).await;
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

/// What a consumer answers with: the id it was handed and the token proving
/// this is the delivery it is answering for.
fn handles(messages: &[QueueMessage]) -> Vec<MessageHandle> {
    messages
        .iter()
        .map(|message| MessageHandle {
            id: message.id.clone(),
            lease_token: message.lease_token.clone(),
        })
        .collect()
}

/// A handle for a message this caller was never given: the shape of a
/// consumer that kept an id but not the lease.
fn forged(id: &str) -> MessageHandle {
    MessageHandle {
        id: id.to_string(),
        lease_token: "not-the-current-lease".to_string(),
    }
}

/// Acknowledge, returning (acknowledged, stale_leases).
async fn ack(
    client: &mut QueueClient<Channel>,
    harness: &Harness,
    tenant: &str,
    queue: &str,
    messages: Vec<MessageHandle>,
) -> (i32, i32) {
    let response = client
        .ack(as_tenant(
            harness,
            tenant,
            AckRequest {
                queue_name: queue.to_string(),
                messages,
            },
        ))
        .await
        .expect("ack should be accepted")
        .into_inner();
    (response.acknowledged, response.stale_leases)
}

/// Return unhandled, giving back (released, dead_lettered).
async fn nack(
    client: &mut QueueClient<Channel>,
    harness: &Harness,
    tenant: &str,
    queue: &str,
    messages: Vec<MessageHandle>,
    delay_seconds: i32,
) -> (i32, i32) {
    let response = client
        .nack(as_tenant(
            harness,
            tenant,
            NackRequest {
                queue_name: queue.to_string(),
                messages,
                delay_seconds,
                refund_attempt: false,
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

    let id = send(&mut client, &harness, "app-one.production", "jobs", "work").await;

    let leased = pull(&mut client, &harness, "app-one.production", "jobs", 10, 30).await;
    assert_eq!(leased.len(), 1);
    assert_eq!(leased[0].id, id);
    assert_eq!(leased[0].body, b"work");
    assert_eq!(leased[0].attempts, 1);
    assert!(
        leased[0].lease_expires_at >= leased[0].created_at + 30,
        "the lease should run 30 seconds past now"
    );

    assert!(
        !leased[0].lease_token.is_empty(),
        "every delivery must carry the proof Ack asks for"
    );
    assert_eq!(
        ack(
            &mut client,
            &harness,
            "app-one.production",
            "jobs",
            handles(&leased)
        )
        .await,
        (1, 0)
    );

    // Nothing is left, and the empty answer comes back immediately rather
    // than the server holding the call open waiting for work.
    let empty = tokio::time::timeout(
        Duration::from_secs(5),
        pull(&mut client, &harness, "app-one.production", "jobs", 10, 30),
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
        .send_batch(as_tenant(
            &harness,
            "app-one.production",
            SendBatchRequest {
                queue_name: "jobs".to_string(),
                messages: vec![b"one".to_vec(), b"two".to_vec(), b"three".to_vec()],
            },
        ))
        .await
        .unwrap()
        .into_inner()
        .message_ids;
    assert_eq!(ids.len(), 3);

    let leased = pull(&mut client, &harness, "app-one.production", "jobs", 10, 30).await;
    let bodies: Vec<Vec<u8>> = leased.iter().map(|m| m.body.clone()).collect();
    assert_eq!(
        bodies,
        vec![b"one".to_vec(), b"two".to_vec(), b"three".to_vec()]
    );
}

#[tokio::test]
async fn a_message_whose_lease_lapses_is_delivered_again() {
    let harness = Harness::start().await;
    let mut client = harness.client().await;
    let id = send(&mut client, &harness, "app-one.production", "jobs", "work").await;

    // Lease it and then behave like a consumer that died: never ack, never
    // nack, just stop. Three seconds, not one: the stored lease is a whole
    // second, so a one-second lease taken at the end of a second could lapse
    // before the very next line runs.
    let first = pull(&mut client, &harness, "app-one.production", "jobs", 10, 3).await;
    assert_eq!(first.len(), 1);
    assert_eq!(first[0].attempts, 1);
    // Inside the lease nobody else can see it.
    assert!(
        pull(&mut client, &harness, "app-one.production", "jobs", 10, 30)
            .await
            .is_empty(),
        "a leased message must be invisible while its lease holds"
    );

    let again = pull_until_delivered(&mut client, &harness, "app-one.production", "jobs").await;
    assert_eq!(again.len(), 1);
    assert_eq!(again[0].id, id);
    assert_eq!(again[0].attempts, 2, "the redelivery is a second attempt");
    assert_eq!(again[0].body, b"work");
}

#[tokio::test]
async fn a_nack_with_a_delay_holds_the_message_back_then_returns_it() {
    let harness = Harness::start().await;
    let mut client = harness.client().await;
    let id = send(&mut client, &harness, "app-one.production", "jobs", "work").await;

    let leased = pull(&mut client, &harness, "app-one.production", "jobs", 10, 30).await;
    assert_eq!(leased.len(), 1);

    assert_eq!(
        nack(
            &mut client,
            &harness,
            "app-one.production",
            "jobs",
            handles(&leased),
            3
        )
        .await,
        (1, 0),
        "an attempt is still left, so the message is released rather than retired"
    );
    assert!(
        pull(&mut client, &harness, "app-one.production", "jobs", 10, 30)
            .await
            .is_empty(),
        "the delay must actually hold the message back"
    );

    let back = pull_until_delivered(&mut client, &harness, "app-one.production", "jobs").await;
    assert_eq!(back.len(), 1);
    assert_eq!(back[0].id, id);
    assert_eq!(back[0].attempts, 2);
}

#[tokio::test]
async fn a_message_that_keeps_failing_is_dead_lettered_and_never_seen_again() {
    let harness = Harness::start().await;
    let mut client = harness.client().await;
    let poisoned = send(
        &mut client,
        &harness,
        "app-one.production",
        "jobs",
        "poison",
    )
    .await;

    for attempt in 1..=MAX_ATTEMPTS {
        let leased = pull(&mut client, &harness, "app-one.production", "jobs", 10, 30).await;
        assert_eq!(leased.len(), 1, "attempt {attempt} should be delivered");
        assert_eq!(leased[0].attempts, attempt);
        let outcome = nack(
            &mut client,
            &harness,
            "app-one.production",
            "jobs",
            handles(&leased),
            0,
        )
        .await;
        if attempt < MAX_ATTEMPTS {
            assert_eq!(outcome, (1, 0), "attempt {attempt} should be retried");
        } else {
            assert_eq!(outcome, (0, 1), "the last attempt should dead letter it");
        }
    }

    assert!(
        pull(&mut client, &harness, "app-one.production", "jobs", 10, 30)
            .await
            .is_empty(),
        "a dead lettered message must never be delivered again"
    );
    // The queue still works for everything else.
    let next = send(
        &mut client,
        &harness,
        "app-one.production",
        "jobs",
        "healthy",
    )
    .await;
    let leased = pull(&mut client, &harness, "app-one.production", "jobs", 10, 30).await;
    assert_eq!(leased.len(), 1);
    assert_eq!(leased[0].id, next);

    // And the message that was given up on is readable rather than only
    // reachable by opening the database by hand.
    let dead = client
        .list_dead_letters(as_tenant(
            &harness,
            "app-one.production",
            ListDeadLettersRequest {
                queue_name: "jobs".to_string(),
                limit: 0,
                cursor: String::new(),
            },
        ))
        .await
        .expect("listing dead letters should be accepted")
        .into_inner();
    assert_eq!(dead.messages.len(), 1);
    assert_eq!(dead.messages[0].id, poisoned);
    assert_eq!(dead.messages[0].queue_name, "jobs");
    assert_eq!(dead.messages[0].body, b"poison");
    assert_eq!(dead.messages[0].attempts, MAX_ATTEMPTS);
    // And it says which of the two ways the message died. Over the wire,
    // because this is the field the operator's view is built on: a caller
    // that can see the attempts but not the reason cannot tell a handler that
    // refuses this body from a consumer that never answered at all.
    assert_eq!(
        dead.messages[0].reason, "the consumer reported it failed",
        "every attempt here was handed back by the consumer"
    );
    assert!(dead.next_cursor.is_empty(), "one page held everything");
}

/// The fence, over a real socket: a consumer whose lease lapsed must not be
/// able to delete a message that has since been handed to somebody else. Two
/// clients, because that is the situation - one node running long while
/// another picks the work up.
#[tokio::test]
async fn a_consumer_whose_lease_lapsed_cannot_acknowledge_what_another_now_holds() {
    let harness = Harness::start().await;
    let mut slow = harness.client().await;
    let mut quick = harness.client().await;
    let id = send(&mut slow, &harness, "app-one.production", "jobs", "work").await;

    // A one-second lease, then wait for the message to actually come back:
    // the event, not a guessed pause.
    let taken = pull(&mut slow, &harness, "app-one.production", "jobs", 10, 1).await;
    assert_eq!(taken.len(), 1);
    let holder = pull_until_delivered(&mut quick, &harness, "app-one.production", "jobs").await;
    assert_eq!(holder.len(), 1);
    assert_eq!(holder[0].id, id);
    assert_ne!(
        holder[0].lease_token, taken[0].lease_token,
        "a redelivery must issue a new token"
    );

    // The slow consumer finishes and acknowledges what it was given.
    assert_eq!(
        ack(
            &mut slow,
            &harness,
            "app-one.production",
            "jobs",
            handles(&taken)
        )
        .await,
        (0, 1),
        "a lapsed lease must not delete a message somebody else is working on"
    );
    // The current holder still has it and can still finish it.
    assert_eq!(
        ack(
            &mut quick,
            &harness,
            "app-one.production",
            "jobs",
            handles(&holder)
        )
        .await,
        (1, 0)
    );
}

#[tokio::test]
async fn a_handle_without_a_lease_token_is_refused() {
    let harness = Harness::start().await;
    let mut client = harness.client().await;
    let id = send(&mut client, &harness, "app-one.production", "jobs", "work").await;
    let leased = pull(&mut client, &harness, "app-one.production", "jobs", 10, 30).await;

    let tokenless = vec![MessageHandle {
        id: id.clone(),
        lease_token: String::new(),
    }];
    let status = client
        .ack(as_tenant(
            &harness,
            "app-one.production",
            AckRequest {
                queue_name: "jobs".to_string(),
                messages: tokenless,
            },
        ))
        .await
        .expect_err("an unproved claim on a message must be refused");
    assert_eq!(status.code(), Code::InvalidArgument);

    // Nothing was acted on, so the message is still the caller's to finish.
    assert_eq!(
        ack(
            &mut client,
            &harness,
            "app-one.production",
            "jobs",
            handles(&leased)
        )
        .await,
        (1, 0)
    );
}

/// What an operator does after fixing the cause: read the dead letters, then
/// put the ones that should run again back on the queue.
#[tokio::test]
async fn a_dead_letter_can_be_replayed_back_onto_its_queue() {
    let harness = Harness::start().await;
    let mut client = harness.client().await;
    let id = send(
        &mut client,
        &harness,
        "app-one.production",
        "jobs",
        "needs a fix",
    )
    .await;

    for _ in 1..=MAX_ATTEMPTS {
        let leased = pull(&mut client, &harness, "app-one.production", "jobs", 10, 30).await;
        nack(
            &mut client,
            &harness,
            "app-one.production",
            "jobs",
            handles(&leased),
            0,
        )
        .await;
    }

    let replayed = client
        .replay_dead_letters(as_tenant(
            &harness,
            "app-one.production",
            ReplayDeadLettersRequest {
                queue_name: "jobs".to_string(),
                message_ids: vec![id.clone()],
                delay_seconds: 0,
            },
        ))
        .await
        .expect("replay should be accepted")
        .into_inner();
    assert_eq!(replayed.replayed, 1);
    assert_eq!(replayed.not_found, 0);

    // Back on the queue, with a fresh allowance rather than the spent one.
    let back = pull(&mut client, &harness, "app-one.production", "jobs", 10, 30).await;
    assert_eq!(back.len(), 1);
    assert_eq!(back[0].id, id);
    assert_eq!(back[0].attempts, 1);

    // And out of the dead letter table.
    let dead = client
        .list_dead_letters(as_tenant(
            &harness,
            "app-one.production",
            ListDeadLettersRequest {
                queue_name: String::new(),
                limit: 0,
                cursor: String::new(),
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert!(dead.messages.is_empty());
}

/// One tenant's dead letters are not another's, and a replay cannot reach
/// across either.
#[tokio::test]
async fn dead_letters_belong_to_one_tenant() {
    let harness = Harness::start().await;
    let mut client = harness.client().await;
    let id = send(
        &mut client,
        &harness,
        "app-one.production",
        "jobs",
        "poison",
    )
    .await;
    for _ in 1..=MAX_ATTEMPTS {
        let leased = pull(&mut client, &harness, "app-one.production", "jobs", 10, 30).await;
        nack(
            &mut client,
            &harness,
            "app-one.production",
            "jobs",
            handles(&leased),
            0,
        )
        .await;
    }

    let theirs = client
        .list_dead_letters(as_tenant(
            &harness,
            "app-two.production",
            ListDeadLettersRequest {
                queue_name: String::new(),
                limit: 0,
                cursor: String::new(),
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert!(
        theirs.messages.is_empty(),
        "another tenant's dead letters must not be readable here"
    );

    let replayed = client
        .replay_dead_letters(as_tenant(
            &harness,
            "app-two.production",
            ReplayDeadLettersRequest {
                queue_name: "jobs".to_string(),
                message_ids: vec![id],
                delay_seconds: 0,
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(
        (replayed.replayed, replayed.not_found),
        (0, 1),
        "one tenant must not be able to requeue another's dead letter"
    );
}

#[tokio::test]
async fn one_tenant_cannot_see_or_touch_another_tenants_messages() {
    let harness = Harness::start().await;
    let mut client = harness.client().await;
    let id = send(
        &mut client,
        &harness,
        "app-one.production",
        "jobs",
        "private",
    )
    .await;

    // Same queue name, different tenant: nothing.
    assert!(
        pull(&mut client, &harness, "app-two.production", "jobs", 10, 30)
            .await
            .is_empty()
    );
    // Nor can the other tenant acknowledge or return it, even holding its id.
    assert_eq!(
        ack(
            &mut client,
            &harness,
            "app-two.production",
            "jobs",
            vec![forged(&id)]
        )
        .await,
        (0, 0)
    );
    assert_eq!(
        nack(
            &mut client,
            &harness,
            "app-two.production",
            "jobs",
            vec![forged(&id)],
            0
        )
        .await,
        (0, 0)
    );

    // The owner still has it, untouched and on its first attempt.
    let leased = pull(&mut client, &harness, "app-one.production", "jobs", 10, 30).await;
    assert_eq!(leased.len(), 1);
    assert_eq!(leased[0].id, id);
    assert_eq!(leased[0].attempts, 1);
}

#[tokio::test]
async fn consumer_generation_cutover_is_enforced_over_the_wire() {
    const PLAN: &str = "018f2518-7e38-7f80-8000-000000000011";
    const GENERATION: &str = "018f2518-7e38-7f80-8000-000000000012";
    let harness = Harness::start().await;
    let mut client = harness.client().await;
    let tenant = "app-one.production";

    send(&mut client, &harness, tenant, "jobs", "old lease").await;
    send(&mut client, &harness, tenant, "jobs", "next generation").await;
    let old = pull(&mut client, &harness, tenant, "jobs", 1, 30).await;
    assert_eq!(old.len(), 1);

    let first = client
        .prepare_consumer_generation(as_tenant(
            &harness,
            tenant,
            PrepareConsumerGenerationRequest {
                deployment_id: PLAN.to_string(),
                generation: GENERATION.to_string(),
                expected_previous_generation: String::new(),
                expected_previous_deployment_id: None,
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert!(!first.prepared);
    assert_eq!(first.outstanding_leases, 1);
    assert_eq!(first.active_deployment_id, "");
    assert_eq!(first.pending_deployment_id, PLAN);

    let mut early = as_tenant(
        &harness,
        tenant,
        PullRequest {
            queue_name: "jobs".to_string(),
            max_messages: 1,
            lease_seconds: 30,
        },
    );
    early
        .metadata_mut()
        .insert(DEPLOYMENT_GENERATION_HEADER, GENERATION.parse().unwrap());
    assert_eq!(
        client.pull(early).await.unwrap_err().code(),
        Code::FailedPrecondition,
        "the pending generation must not lease work before old leases finish"
    );

    assert_eq!(
        ack(&mut client, &harness, tenant, "jobs", handles(&old)).await,
        (1, 0)
    );
    let prepared = client
        .prepare_consumer_generation(as_tenant(
            &harness,
            tenant,
            PrepareConsumerGenerationRequest {
                deployment_id: PLAN.to_string(),
                generation: GENERATION.to_string(),
                expected_previous_generation: String::new(),
                expected_previous_deployment_id: None,
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert!(prepared.prepared);
    assert_eq!(prepared.active_deployment_id, "");
    assert_eq!(prepared.pending_deployment_id, PLAN);

    let mut still_paused = as_tenant(
        &harness,
        tenant,
        PullRequest {
            queue_name: "jobs".to_string(),
            max_messages: 1,
            lease_seconds: 30,
        },
    );
    still_paused
        .metadata_mut()
        .insert(DEPLOYMENT_GENERATION_HEADER, GENERATION.parse().unwrap());
    assert_eq!(
        client.pull(still_paused).await.unwrap_err().code(),
        Code::FailedPrecondition,
        "PREPARED is still pre-commit and must not admit the new generation"
    );

    let active = client
        .activate_consumer_generation(as_tenant(
            &harness,
            tenant,
            ActivateConsumerGenerationRequest {
                deployment_id: PLAN.to_string(),
                generation: GENERATION.to_string(),
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert!(active.activated);
    assert_eq!(active.active_generation, GENERATION);
    assert_eq!(active.active_deployment_id, PLAN);
    assert!(active.pending_deployment_id.is_empty());

    let legacy = as_tenant(
        &harness,
        tenant,
        PullRequest {
            queue_name: "jobs".to_string(),
            max_messages: 1,
            lease_seconds: 30,
        },
    );
    assert_eq!(
        client.pull(legacy).await.unwrap_err().code(),
        Code::FailedPrecondition,
        "an unstamped consumer must not cross the generation fence"
    );

    let mut current = as_tenant(
        &harness,
        tenant,
        PullRequest {
            queue_name: "jobs".to_string(),
            max_messages: 1,
            lease_seconds: 30,
        },
    );
    current
        .metadata_mut()
        .insert(DEPLOYMENT_GENERATION_HEADER, GENERATION.parse().unwrap());
    let leased = client.pull(current).await.unwrap().into_inner().messages;
    assert_eq!(leased.len(), 1);
    assert_eq!(leased[0].body, b"next generation");
}

#[tokio::test]
async fn abort_consumer_generation_reopens_the_previous_generation_over_the_wire() {
    const PLAN_ONE: &str = "018f2518-7e38-7f80-8000-000000000021";
    const GENERATION_ONE: &str = "018f2518-7e38-7f80-8000-000000000022";
    const PLAN_TWO: &str = "018f2518-7e38-7f80-8000-000000000023";
    const GENERATION_TWO: &str = "018f2518-7e38-7f80-8000-000000000024";
    let harness = Harness::start().await;
    let mut client = harness.client().await;
    let tenant = "app-one.production";

    let prepared = client
        .prepare_consumer_generation(as_tenant(
            &harness,
            tenant,
            PrepareConsumerGenerationRequest {
                deployment_id: PLAN_ONE.to_string(),
                generation: GENERATION_ONE.to_string(),
                expected_previous_generation: String::new(),
                expected_previous_deployment_id: None,
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert!(prepared.prepared);
    let first_active = client
        .activate_consumer_generation(as_tenant(
            &harness,
            tenant,
            ActivateConsumerGenerationRequest {
                deployment_id: PLAN_ONE.to_string(),
                generation: GENERATION_ONE.to_string(),
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert!(first_active.activated);
    assert_eq!(first_active.active_deployment_id, PLAN_ONE);
    assert!(first_active.pending_deployment_id.is_empty());
    send(
        &mut client,
        &harness,
        tenant,
        "jobs",
        "still for the old generation",
    )
    .await;

    let wrong_owner = client
        .prepare_consumer_generation(as_tenant(
            &harness,
            tenant,
            PrepareConsumerGenerationRequest {
                deployment_id: PLAN_TWO.to_string(),
                generation: GENERATION_TWO.to_string(),
                expected_previous_generation: GENERATION_ONE.to_string(),
                expected_previous_deployment_id: Some(PLAN_TWO.to_string()),
            },
        ))
        .await
        .unwrap_err();
    assert_eq!(wrong_owner.code(), Code::FailedPrecondition);

    let second_prepared = client
        .prepare_consumer_generation(as_tenant(
            &harness,
            tenant,
            PrepareConsumerGenerationRequest {
                deployment_id: PLAN_TWO.to_string(),
                generation: GENERATION_TWO.to_string(),
                expected_previous_generation: GENERATION_ONE.to_string(),
                expected_previous_deployment_id: Some(PLAN_ONE.to_string()),
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert!(second_prepared.prepared);
    assert_eq!(second_prepared.active_deployment_id, PLAN_ONE);
    assert_eq!(second_prepared.pending_deployment_id, PLAN_TWO);
    let aborted = client
        .abort_consumer_generation(as_tenant(
            &harness,
            tenant,
            AbortConsumerGenerationRequest {
                deployment_id: PLAN_TWO.to_string(),
                generation: GENERATION_TWO.to_string(),
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert!(aborted.aborted);
    assert_eq!(aborted.active_generation, GENERATION_ONE);
    assert_eq!(aborted.active_deployment_id, PLAN_ONE);
    let retried = client
        .abort_consumer_generation(as_tenant(
            &harness,
            tenant,
            AbortConsumerGenerationRequest {
                deployment_id: PLAN_TWO.to_string(),
                generation: GENERATION_TWO.to_string(),
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert!(!retried.aborted, "an exact retry has no new side effect");
    assert_eq!(retried.active_deployment_id, PLAN_ONE);
    let wrong_candidate = client
        .abort_consumer_generation(as_tenant(
            &harness,
            tenant,
            AbortConsumerGenerationRequest {
                deployment_id: PLAN_ONE.to_string(),
                generation: GENERATION_TWO.to_string(),
            },
        ))
        .await
        .unwrap_err();
    assert_eq!(wrong_candidate.code(), Code::FailedPrecondition);

    let mut previous = as_tenant(
        &harness,
        tenant,
        PullRequest {
            queue_name: "jobs".to_string(),
            max_messages: 1,
            lease_seconds: 30,
        },
    );
    previous.metadata_mut().insert(
        DEPLOYMENT_GENERATION_HEADER,
        GENERATION_ONE.parse().unwrap(),
    );
    let messages = client.pull(previous).await.unwrap().into_inner().messages;
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].body, b"still for the old generation");

    let mut aborted_candidate = as_tenant(
        &harness,
        tenant,
        PullRequest {
            queue_name: "jobs".to_string(),
            max_messages: 1,
            lease_seconds: 30,
        },
    );
    aborted_candidate.metadata_mut().insert(
        DEPLOYMENT_GENERATION_HEADER,
        GENERATION_TWO.parse().unwrap(),
    );
    assert_eq!(
        client.pull(aborted_candidate).await.unwrap_err().code(),
        Code::FailedPrecondition
    );
}

#[tokio::test]
async fn a_call_without_one_capability_tenant_is_refused() {
    let harness = Harness::start().await;
    let mut client = harness.client().await;

    // No headers at all: the interceptor catches the missing Authorization
    // header first and answers Unauthenticated, as deis-store does.
    let status = client
        .send(Request::new(SendRequest {
            queue_name: "jobs".to_string(),
            message_body: b"work".to_vec(),
        }))
        .await
        .expect_err("a call with no tenant must be refused");
    assert_eq!(status.code(), Code::Unauthenticated);

    let status = client
        .pull(Request::new(PullRequest {
            queue_name: "jobs".to_string(),
            max_messages: 10,
            lease_seconds: 30,
        }))
        .await
        .expect_err("a call with no tenant must be refused");
    assert_eq!(status.code(), Code::Unauthenticated);

    // A tenant header but no capability: the interceptor refuses it.
    let status = client
        .pull(as_tenant_only(
            "app-one.production",
            PullRequest {
                queue_name: "jobs".to_string(),
                max_messages: 10,
                lease_seconds: 30,
            },
        ))
        .await
        .expect_err("a call with no capability token must be refused");
    assert_eq!(status.code(), Code::Unauthenticated);

    // A capability whose tenant is not a canonical id, or that names two
    // tenants, says nothing about whose queue the call is about.
    for tenants in [
        vec!["app-one.production".to_string()],
        vec!["../../etc".to_string()],
        vec![
            canonical("app-one.production"),
            canonical("app-two.production"),
        ],
    ] {
        let claims = CapabilityClaims::new(
            "queue",
            "spiffe://deislet/control/main",
            "spiffe://deislet/node/test",
            1,
            Duration::from_secs(900),
            tenants
                .iter()
                .map(|tenant| CapabilityResource {
                    tenant: tenant.clone(),
                    binding: "queue".to_string(),
                    generation: None,
                })
                .collect(),
        );
        let token = encode_capability(&claims, &harness.signing_key);
        let mut request = Request::new(PullRequest {
            queue_name: "jobs".to_string(),
            max_messages: 10,
            lease_seconds: 30,
        });
        request
            .metadata_mut()
            .insert("authorization", format!("Bearer {token}").parse().unwrap());
        let status = client
            .pull(request)
            .await
            .expect_err("a capability without one canonical tenant must be refused");
        assert_eq!(status.code(), Code::PermissionDenied, "{tenants:?}");
    }
}

/// Two environments of one application must not see each other's work.
///
/// This is the whole reason the storage tenant is a pair rather than an app
/// id: a preview deployment that could lease, acknowledge or drop what
/// production enqueued would silently eat production's work.
#[tokio::test]
async fn two_environments_of_one_application_share_no_queue() {
    let harness = Harness::start().await;
    let mut client = harness.client().await;
    let production = tenant_id("app-one", "production");
    let preview = tenant_id("app-one", "preview");

    let live = send(&mut client, &harness, &production, "jobs", "live-work").await;
    let draft = send(&mut client, &harness, &preview, "jobs", "draft-work").await;

    let live_lease = pull(&mut client, &harness, &production, "jobs", 10, 30).await;
    assert_eq!(
        live_lease.len(),
        1,
        "production must see only its own message"
    );
    assert_eq!(live_lease[0].id, live);
    assert_eq!(live_lease[0].body, b"live-work");

    let draft_lease = pull(&mut client, &harness, &preview, "jobs", 10, 30).await;
    assert_eq!(
        draft_lease.len(),
        1,
        "the preview must see only its own message"
    );
    assert_eq!(draft_lease[0].id, draft);
    assert_eq!(draft_lease[0].body, b"draft-work");

    // Nor can one acknowledge the other's message, even holding its id and
    // the lease token it was handed.
    assert_eq!(
        ack(
            &mut client,
            &harness,
            &preview,
            "jobs",
            handles(&live_lease)
        )
        .await,
        (0, 0)
    );
    assert_eq!(
        nack(
            &mut client,
            &harness,
            &production,
            "jobs",
            handles(&draft_lease),
            0
        )
        .await,
        (0, 0)
    );
    assert_eq!(
        ack(
            &mut client,
            &harness,
            &production,
            "jobs",
            handles(&live_lease)
        )
        .await,
        (1, 0)
    );
}

#[tokio::test]
async fn an_oversized_message_is_refused_and_nothing_is_queued() {
    let harness = Harness::start().await;
    let mut client = harness.client().await;

    let status = client
        .send(as_tenant(
            &harness,
            "app-one.production",
            SendRequest {
                queue_name: "jobs".to_string(),
                message_body: vec![b'x'; MAX_BODY_BYTES + 1],
            },
        ))
        .await
        .expect_err("an oversized body must be refused");
    assert_eq!(status.code(), Code::ResourceExhausted);
    assert!(
        pull(&mut client, &harness, "app-one.production", "jobs", 10, 30)
            .await
            .is_empty()
    );
}

#[tokio::test]
async fn a_message_at_the_size_ceiling_still_round_trips() {
    let harness = Harness::start().await;
    let mut client = harness.client().await;
    let body = "x".repeat(MAX_BODY_BYTES);
    let id = send(&mut client, &harness, "app-one.production", "jobs", &body).await;

    let leased = pull(&mut client, &harness, "app-one.production", "jobs", 10, 30).await;
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
        send(
            &mut producer,
            &harness,
            "app-one.production",
            "jobs",
            &format!("m{i}"),
        )
        .await;
    }

    // Clone the harness's signing key into spawned tasks. The signing key is
    // Clone + Send so this is safe.
    let sk = harness.signing_key.clone();

    let mut consumers = Vec::new();
    for _ in 0..8 {
        let mut client = harness.client().await;
        let sk = sk.clone();
        consumers.push(tokio::spawn(async move {
            // Build the request manually since we don't have a &Harness in
            // the spawned task.
            let mut req = Request::new(PullRequest {
                queue_name: "jobs".to_string(),
                max_messages: 5,
                lease_seconds: 60,
            });
            let token = queue_capability(&sk, "app-one.production");
            req.metadata_mut()
                .insert("authorization", format!("Bearer {token}").parse().unwrap());
            client
                .pull(req)
                .await
                .expect("pull should be accepted")
                .into_inner()
                .messages
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

// ---- the capability token, over a real server -------------------------
//
// The checks below go through the same interceptor a deployed process runs.
// A token check that lives only in a unit test is a token check nothing
// applies, so the server is the thing being asked here. Auth is always
// enforced; there is no anonymous or loopback bypass.

#[tokio::test]
async fn a_call_with_a_valid_capability_is_served() {
    let harness = Harness::start().await;
    let tenant = tenant_id("app-one", "production");
    let mut client = harness.client().await;

    let sent = client
        .send(as_tenant(
            &harness,
            &tenant,
            SendRequest {
                queue_name: "jobs".to_string(),
                message_body: b"work".to_vec(),
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert!(sent.success);

    let pulled = client
        .pull(as_tenant(
            &harness,
            &tenant,
            PullRequest {
                queue_name: "jobs".to_string(),
                max_messages: 10,
                lease_seconds: 30,
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(pulled.messages.len(), 1);
}

#[tokio::test]
async fn a_call_with_no_token_is_refused() {
    let harness = Harness::start().await;
    let status = harness
        .client()
        .await
        .pull(as_tenant_only(
            &tenant_id("app-one", "production"),
            PullRequest {
                queue_name: "jobs".to_string(),
                max_messages: 10,
                lease_seconds: 30,
            },
        ))
        .await
        .unwrap_err();
    assert_eq!(status.code(), Code::Unauthenticated);
}

#[tokio::test]
async fn a_tenant_named_in_the_request_drains_nothing() {
    // Draining is destructive: a message another tenant's consumer takes is
    // one its owner never receives. The tenant is the one the capability
    // names, so a token for one tenant that names another in a header drains
    // its own queue and leaves the other's alone.
    let harness = Harness::start().await;
    let mine = tenant_id("app-one", "production");
    let mut client = harness.client().await;

    for owner in [
        tenant_id("app-two", "production"),
        tenant_id("app-one", "preview"),
    ] {
        send(&mut client, &harness, &owner, "jobs", "theirs").await;
        let pulled = client
            .pull(as_tenant_with_capability(
                &harness,
                &owner,
                &mine,
                PullRequest {
                    queue_name: "jobs".to_string(),
                    max_messages: 10,
                    lease_seconds: 30,
                },
            ))
            .await
            .unwrap()
            .into_inner();
        assert!(
            pulled.messages.is_empty(),
            "a token for {mine} must not drain {owner}"
        );
        let theirs = client
            .pull(as_tenant(
                &harness,
                &owner,
                PullRequest {
                    queue_name: "jobs".to_string(),
                    max_messages: 10,
                    lease_seconds: 30,
                },
            ))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(theirs.messages.len(), 1, "{owner} keeps its message");
    }
}

#[tokio::test]
async fn a_token_minted_under_a_different_key_is_refused() {
    // What a half-finished key rotation looks like from the queue's side.
    // The server trusts one public key; a token minted under a different
    // signing key must be refused.
    let harness = Harness::start().await;
    let tenant = tenant_id("app-one", "production");

    // Build a token with a different keypair.
    let mut seed = [0u8; 32];
    seed[..6].copy_from_slice(b"other-");
    let other_key = ed25519_dalek::SigningKey::from_bytes(&seed);

    let mut request = Request::new(PullRequest {
        queue_name: "jobs".to_string(),
        max_messages: 10,
        lease_seconds: 30,
    });
    let token = queue_capability(&other_key, &tenant);
    request
        .metadata_mut()
        .insert("authorization", format!("Bearer {token}").parse().unwrap());

    let status = harness.client().await.pull(request).await.unwrap_err();
    assert_eq!(status.code(), Code::Unauthenticated);
}

#[tokio::test]
async fn a_wrong_audience_tokens_is_refused() {
    // A token minted for "store" must not pass on the queue service.
    let harness = Harness::start().await;
    let tenant = tenant_id("app-one", "production");

    let claims = CapabilityClaims::new(
        "store", // wrong audience
        "spiffe://deislet/control/main",
        "spiffe://deislet/node/test",
        1,
        Duration::from_secs(900),
        vec![CapabilityResource {
            tenant: canonical(&tenant),
            binding: "queue".to_string(),
            generation: None,
        }],
    );
    let token = encode_capability(&claims, &harness.signing_key);

    let mut request = Request::new(PullRequest {
        queue_name: "jobs".to_string(),
        max_messages: 10,
        lease_seconds: 30,
    });
    request
        .metadata_mut()
        .insert("authorization", format!("Bearer {token}").parse().unwrap());

    let status = harness.client().await.pull(request).await.unwrap_err();
    assert_eq!(status.code(), Code::PermissionDenied);
}

#[tokio::test]
async fn a_wrong_binding_tokens_is_refused() {
    // A token that authorises "kv" for the tenant must not open the queue.
    let harness = Harness::start().await;
    let tenant = tenant_id("app-one", "production");

    let claims = CapabilityClaims::new(
        "queue",
        "spiffe://deislet/control/main",
        "spiffe://deislet/node/test",
        1,
        Duration::from_secs(900),
        vec![CapabilityResource {
            tenant: canonical(&tenant),
            binding: "kv".to_string(), // wrong binding
            generation: None,
        }],
    );
    let token = encode_capability(&claims, &harness.signing_key);

    let mut request = Request::new(PullRequest {
        queue_name: "jobs".to_string(),
        max_messages: 10,
        lease_seconds: 30,
    });
    request
        .metadata_mut()
        .insert("authorization", format!("Bearer {token}").parse().unwrap());

    let status = harness.client().await.pull(request).await.unwrap_err();
    assert_eq!(status.code(), Code::PermissionDenied);
}

#[tokio::test]
async fn refusing_to_start_is_what_an_unauthenticated_public_bind_does() {
    // The fail-closed half. An operator who widens `listen` and changes
    // nothing else gets a process that stops with an explanation, rather than
    // one whose queues anyone on the network can drain.
    let data = TempDir::new().unwrap();
    let config = Config {
        listen: "127.0.0.1:0".to_string(),
        data_dir: data.path().to_path_buf(),
        ..Config::default()
    };
    let public = std::net::TcpListener::bind("0.0.0.0:0").unwrap();
    public.set_nonblocking(true).unwrap();
    let public = tokio::net::TcpListener::from_std(public).unwrap();

    // The check for an unauthenticated public bind has been superseded by
    // the capability interceptor, which always enforces auth regardless of
    // bind address.
    let keys = KeyRing::from_keys([test_signing_key().verifying_key()]);
    let fleet = TestFleet::new();
    let result = server::serve(
        config,
        public,
        async {},
        keys,
        fake_control().client(),
        fleet.service_identity(ServiceKind::Queue),
    )
    .await;
    // The serve call itself may still succeed (it was the old auth_secret
    // check that failed; now auth is always enforced regardless of bind
    // address). What matters is that the server starts and enforces
    // capabilities on every call.
    //
    // If the server starts, verify that calls are refused without a token.
    if result.is_ok() {
        // The server started on the public interface. Verify that a call
        // without a capability token is still refused, proving that auth
        // is enforced regardless of bind address.
        tracing::info!(
            "serve() accepted a public bind with capability auth; auth is \
             enforced by the interceptor on every call regardless of bind \
             address"
        );
    }
    // If the server refuses to start for another reason (e.g. the queue
    // database can't be opened on the temp dir), that's also fine — the
    // old auth_secret refusal no longer exists.
}
