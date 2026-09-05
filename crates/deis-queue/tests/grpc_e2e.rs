//! End-to-end checks against a real deis-queue server.
//!
//! Everything here goes over a real TCP socket and the real generated client,
//! so it exercises what the unit tests cannot: the tenant metadata header, the
//! message size ceiling, the fact that the service is actually registered on
//! the server, and the lease and dead letter behaviour a consumer will
//! actually meet.
//!
//! Every call stamps `x-deis-tenant` with an `<app>.<environment>` value,
//! which is what deis-runtime has to send. The tests below use
//! `app-one.production` and `app-two.production` as two unrelated tenants,
//! and one test pairs `app-one.production` with `app-one.preview` to check
//! that two environments of the *same* application are also two namespaces.
//!
//! The two tests that wait for a lease to lapse use a one-second lease and
//! poll for the message to reappear, under a hard deadline. They wait on the
//! event itself - the message becoming visible again - rather than assuming a
//! fixed pause is enough, so a broken lease fails the test instead of making
//! it flaky.

use deis_backend_migration::proto::backend_migration_client::BackendMigrationClient;
use deis_backend_migration::proto::{
    AcquireWriteFenceRequest, GetBackendInfoRequest, InspectTenantRequest, ReleaseWriteFenceRequest,
};
use deis_queue::config::Config;
use deis_queue::proto::queue::{
    AbortConsumerGenerationRequest, AckRequest, ActivateConsumerGenerationRequest,
    ListDeadLettersRequest, MessageHandle, NackRequest, PrepareConsumerGenerationRequest,
    PullRequest, QueueMessage, ReplayDeadLettersRequest, SendBatchRequest, SendRequest,
    queue_client::QueueClient,
};
use deis_queue::tenant::TENANT_HEADER;
use deis_queue::{MAX_GRPC_MESSAGE_BYTES, server};
use deis_wire::conventions::{
    APP_ID_HEADER, DEPLOYMENT_GENERATION_HEADER, STORAGE_TOKEN_HEADER, storage_token, tenant_id,
};
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
    /// The default single-box install: loopback, no `auth_secret`, so calls
    /// need no token. Most tests below are about queue behaviour rather than
    /// about who may call it.
    async fn start() -> Self {
        Self::start_with(None).await
    }

    /// A server that requires a capability token, as any deployment that is
    /// reachable off-host must.
    async fn start_authenticated(secret: &str) -> Self {
        Self::start_with(Some(secret.to_string())).await
    }

    async fn start_migration_authenticated(token: &str) -> Self {
        Self::start_with_options(None, Some(token.to_string())).await
    }

    async fn start_with(auth_secret: Option<String>) -> Self {
        Self::start_with_options(auth_secret, None).await
    }

    async fn start_with_options(
        auth_secret: Option<String>,
        migration_admin_token: Option<String>,
    ) -> Self {
        let data = TempDir::new().unwrap();
        let config = Config {
            listen: "127.0.0.1:0".to_string(),
            data_dir: data.path().to_path_buf(),
            max_attempts: MAX_ATTEMPTS,
            max_body_bytes: MAX_BODY_BYTES,
            auth_secret,
            migration_admin_token,
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

    async fn migration(&self) -> BackendMigrationClient<Channel> {
        BackendMigrationClient::new(
            Channel::from_shared(self.endpoint.clone())
                .unwrap()
                .connect()
                .await
                .unwrap(),
        )
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

#[tokio::test]
async fn migration_management_is_absent_by_default_and_freezes_queue_mutations() {
    const ADMIN: &str = "migration-admin-token-000000000000000001";
    const MIGRATION: &str = "01990f44-1000-7000-8000-000000000050";
    const FENCE: &str = "migration-fence-token-00000000000000001";
    const UNKNOWN_INCARNATION: &str = "01990f44-1000-7000-8000-000000000099";
    let acquire = |backend_incarnation_id: &str| AcquireWriteFenceRequest {
        tenant: "shop.production".to_string(),
        migration_id: MIGRATION.to_string(),
        expected_application_revision: 3,
        fence_token: FENCE.to_string(),
        expected_backend_incarnation_id: backend_incarnation_id.to_string(),
    };

    let without_management = Harness::start().await;
    assert_eq!(
        without_management
            .migration()
            .await
            .acquire_write_fence(Request::new(acquire(UNKNOWN_INCARNATION)))
            .await
            .unwrap_err()
            .code(),
        Code::Unimplemented
    );
    drop(without_management);

    let harness = Harness::start_migration_authenticated(ADMIN).await;
    let mut queue = harness.client().await;
    queue
        .send(as_tenant(
            "shop.production",
            SendRequest {
                queue_name: "jobs".to_string(),
                message_body: b"one".to_vec(),
            },
        ))
        .await
        .unwrap();
    let mut migration = harness.migration().await;
    let backend_info = migration
        .get_backend_info(as_migration_admin(ADMIN, GetBackendInfoRequest {}))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(backend_info.backend, "deis-queue");
    let backend_incarnation_id = backend_info.backend_incarnation_id;
    assert_eq!(
        migration
            .acquire_write_fence(Request::new(acquire(&backend_incarnation_id)))
            .await
            .unwrap_err()
            .code(),
        Code::Unauthenticated
    );
    migration
        .acquire_write_fence(as_migration_admin(ADMIN, acquire(&backend_incarnation_id)))
        .await
        .unwrap();

    assert_eq!(
        queue
            .pull(as_tenant(
                "shop.production",
                PullRequest {
                    queue_name: "jobs".to_string(),
                    max_messages: 1,
                    lease_seconds: 30,
                },
            ))
            .await
            .unwrap_err()
            .code(),
        Code::FailedPrecondition
    );
    let inventory = migration
        .inspect_tenant(as_migration_admin(
            ADMIN,
            InspectTenantRequest {
                tenant: "shop.production".to_string(),
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(inventory.backend, "deis-queue");
    assert_eq!(inventory.components[0].item_count, 1);

    migration
        .release_write_fence(as_migration_admin(
            ADMIN,
            ReleaseWriteFenceRequest {
                tenant: "shop.production".to_string(),
                migration_id: MIGRATION.to_string(),
                expected_application_revision: 3,
                fence_token: FENCE.to_string(),
                expected_backend_incarnation_id: backend_incarnation_id,
            },
        ))
        .await
        .unwrap();
    queue
        .send(as_tenant(
            "shop.production",
            SendRequest {
                queue_name: "jobs".to_string(),
                message_body: b"two".to_vec(),
            },
        ))
        .await
        .unwrap();
}

/// Stamp the tenant the way deis-runtime has to.
fn as_tenant<T>(tenant: &str, message: T) -> Request<T> {
    let mut request = Request::new(message);
    request
        .metadata_mut()
        .insert(TENANT_HEADER, tenant.parse().unwrap());
    request
}

fn as_migration_admin<T>(token: &str, message: T) -> Request<T> {
    let mut request = Request::new(message);
    request
        .metadata_mut()
        .insert("authorization", format!("Bearer {token}").parse().unwrap());
    request
}

/// Stamp the tenant *and* the capability token minted for `token_tenant`
/// under `secret`, the way deis-runtime has to once a secret is configured.
///
/// The two tenants are separate arguments on purpose: presenting a token
/// minted for one tenant on a call that claims another is exactly the attack
/// the derivation is meant to defeat, and it has to be expressible here.
fn as_tenant_with_token<T>(
    tenant: &str,
    secret: &str,
    token_tenant: &str,
    message: T,
) -> Request<T> {
    let mut request = as_tenant(tenant, message);
    request.metadata_mut().insert(
        STORAGE_TOKEN_HEADER,
        storage_token(secret, token_tenant).parse().unwrap(),
    );
    request
}

async fn send(client: &mut QueueClient<Channel>, tenant: &str, queue: &str, body: &str) -> String {
    let response = client
        .send(as_tenant(
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
    tenant: &str,
    queue: &str,
    max_messages: i32,
    lease_seconds: i32,
) -> Vec<QueueMessage> {
    client
        .pull(as_tenant(
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
    tenant: &str,
    queue: &str,
) -> Vec<QueueMessage> {
    let deadline = tokio::time::Instant::now() + LEASE_DEADLINE;
    loop {
        let messages = pull(client, tenant, queue, 10, 30).await;
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
    tenant: &str,
    queue: &str,
    messages: Vec<MessageHandle>,
) -> (i32, i32) {
    let response = client
        .ack(as_tenant(
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
    tenant: &str,
    queue: &str,
    messages: Vec<MessageHandle>,
    delay_seconds: i32,
) -> (i32, i32) {
    let response = client
        .nack(as_tenant(
            tenant,
            NackRequest {
                queue_name: queue.to_string(),
                messages,
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

    let id = send(&mut client, "app-one.production", "jobs", "work").await;

    let leased = pull(&mut client, "app-one.production", "jobs", 10, 30).await;
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
        ack(&mut client, "app-one.production", "jobs", handles(&leased)).await,
        (1, 0)
    );

    // Nothing is left, and the empty answer comes back immediately rather
    // than the server holding the call open waiting for work.
    let empty = tokio::time::timeout(
        Duration::from_secs(5),
        pull(&mut client, "app-one.production", "jobs", 10, 30),
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

    let leased = pull(&mut client, "app-one.production", "jobs", 10, 30).await;
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
    let id = send(&mut client, "app-one.production", "jobs", "work").await;

    // Lease it and then behave like a consumer that died: never ack, never
    // nack, just stop. Three seconds, not one: the stored lease is a whole
    // second, so a one-second lease taken at the end of a second could lapse
    // before the very next line runs.
    let first = pull(&mut client, "app-one.production", "jobs", 10, 3).await;
    assert_eq!(first.len(), 1);
    assert_eq!(first[0].attempts, 1);
    // Inside the lease nobody else can see it.
    assert!(
        pull(&mut client, "app-one.production", "jobs", 10, 30)
            .await
            .is_empty(),
        "a leased message must be invisible while its lease holds"
    );

    let again = pull_until_delivered(&mut client, "app-one.production", "jobs").await;
    assert_eq!(again.len(), 1);
    assert_eq!(again[0].id, id);
    assert_eq!(again[0].attempts, 2, "the redelivery is a second attempt");
    assert_eq!(again[0].body, b"work");
}

#[tokio::test]
async fn a_nack_with_a_delay_holds_the_message_back_then_returns_it() {
    let harness = Harness::start().await;
    let mut client = harness.client().await;
    let id = send(&mut client, "app-one.production", "jobs", "work").await;

    let leased = pull(&mut client, "app-one.production", "jobs", 10, 30).await;
    assert_eq!(leased.len(), 1);

    assert_eq!(
        nack(
            &mut client,
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
        pull(&mut client, "app-one.production", "jobs", 10, 30)
            .await
            .is_empty(),
        "the delay must actually hold the message back"
    );

    let back = pull_until_delivered(&mut client, "app-one.production", "jobs").await;
    assert_eq!(back.len(), 1);
    assert_eq!(back[0].id, id);
    assert_eq!(back[0].attempts, 2);
}

#[tokio::test]
async fn a_message_that_keeps_failing_is_dead_lettered_and_never_seen_again() {
    let harness = Harness::start().await;
    let mut client = harness.client().await;
    let poisoned = send(&mut client, "app-one.production", "jobs", "poison").await;

    for attempt in 1..=MAX_ATTEMPTS {
        let leased = pull(&mut client, "app-one.production", "jobs", 10, 30).await;
        assert_eq!(leased.len(), 1, "attempt {attempt} should be delivered");
        assert_eq!(leased[0].attempts, attempt);
        let outcome = nack(
            &mut client,
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
        pull(&mut client, "app-one.production", "jobs", 10, 30)
            .await
            .is_empty(),
        "a dead lettered message must never be delivered again"
    );
    // The queue still works for everything else.
    let next = send(&mut client, "app-one.production", "jobs", "healthy").await;
    let leased = pull(&mut client, "app-one.production", "jobs", 10, 30).await;
    assert_eq!(leased.len(), 1);
    assert_eq!(leased[0].id, next);

    // And the message that was given up on is readable rather than only
    // reachable by opening the database by hand.
    let dead = client
        .list_dead_letters(as_tenant(
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
    let id = send(&mut slow, "app-one.production", "jobs", "work").await;

    // A one-second lease, then wait for the message to actually come back:
    // the event, not a guessed pause.
    let taken = pull(&mut slow, "app-one.production", "jobs", 10, 1).await;
    assert_eq!(taken.len(), 1);
    let holder = pull_until_delivered(&mut quick, "app-one.production", "jobs").await;
    assert_eq!(holder.len(), 1);
    assert_eq!(holder[0].id, id);
    assert_ne!(
        holder[0].lease_token, taken[0].lease_token,
        "a redelivery must issue a new token"
    );

    // The slow consumer finishes and acknowledges what it was given.
    assert_eq!(
        ack(&mut slow, "app-one.production", "jobs", handles(&taken)).await,
        (0, 1),
        "a lapsed lease must not delete a message somebody else is working on"
    );
    // The current holder still has it and can still finish it.
    assert_eq!(
        ack(&mut quick, "app-one.production", "jobs", handles(&holder)).await,
        (1, 0)
    );
}

#[tokio::test]
async fn a_handle_without_a_lease_token_is_refused() {
    let harness = Harness::start().await;
    let mut client = harness.client().await;
    let id = send(&mut client, "app-one.production", "jobs", "work").await;
    let leased = pull(&mut client, "app-one.production", "jobs", 10, 30).await;

    let tokenless = vec![MessageHandle {
        id: id.clone(),
        lease_token: String::new(),
    }];
    let status = client
        .ack(as_tenant(
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
        ack(&mut client, "app-one.production", "jobs", handles(&leased)).await,
        (1, 0)
    );
}

/// What an operator does after fixing the cause: read the dead letters, then
/// put the ones that should run again back on the queue.
#[tokio::test]
async fn a_dead_letter_can_be_replayed_back_onto_its_queue() {
    let harness = Harness::start().await;
    let mut client = harness.client().await;
    let id = send(&mut client, "app-one.production", "jobs", "needs a fix").await;

    for _ in 1..=MAX_ATTEMPTS {
        let leased = pull(&mut client, "app-one.production", "jobs", 10, 30).await;
        nack(
            &mut client,
            "app-one.production",
            "jobs",
            handles(&leased),
            0,
        )
        .await;
    }

    let replayed = client
        .replay_dead_letters(as_tenant(
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
    let back = pull(&mut client, "app-one.production", "jobs", 10, 30).await;
    assert_eq!(back.len(), 1);
    assert_eq!(back[0].id, id);
    assert_eq!(back[0].attempts, 1);

    // And out of the dead letter table.
    let dead = client
        .list_dead_letters(as_tenant(
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
    let id = send(&mut client, "app-one.production", "jobs", "poison").await;
    for _ in 1..=MAX_ATTEMPTS {
        let leased = pull(&mut client, "app-one.production", "jobs", 10, 30).await;
        nack(
            &mut client,
            "app-one.production",
            "jobs",
            handles(&leased),
            0,
        )
        .await;
    }

    let theirs = client
        .list_dead_letters(as_tenant(
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
    let id = send(&mut client, "app-one.production", "jobs", "private").await;

    // Same queue name, different tenant: nothing.
    assert!(
        pull(&mut client, "app-two.production", "jobs", 10, 30)
            .await
            .is_empty()
    );
    // Nor can the other tenant acknowledge or return it, even holding its id.
    assert_eq!(
        ack(&mut client, "app-two.production", "jobs", vec![forged(&id)]).await,
        (0, 0)
    );
    assert_eq!(
        nack(
            &mut client,
            "app-two.production",
            "jobs",
            vec![forged(&id)],
            0
        )
        .await,
        (0, 0)
    );

    // The owner still has it, untouched and on its first attempt.
    let leased = pull(&mut client, "app-one.production", "jobs", 10, 30).await;
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

    send(&mut client, tenant, "jobs", "old lease").await;
    send(&mut client, tenant, "jobs", "next generation").await;
    let old = pull(&mut client, tenant, "jobs", 1, 30).await;
    assert_eq!(old.len(), 1);

    let first = client
        .prepare_consumer_generation(as_tenant(
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
        ack(&mut client, tenant, "jobs", handles(&old)).await,
        (1, 0)
    );
    let prepared = client
        .prepare_consumer_generation(as_tenant(
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
    send(&mut client, tenant, "jobs", "still for the old generation").await;

    let wrong_owner = client
        .prepare_consumer_generation(as_tenant(
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
async fn a_call_without_the_tenant_header_is_refused() {
    let harness = Harness::start().await;
    let mut client = harness.client().await;

    let status = client
        .send(Request::new(SendRequest {
            queue_name: "jobs".to_string(),
            message_body: b"work".to_vec(),
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

    // The application header is not the storage tenant. A caller that stamps
    // only `x-deis-app-id` - which is what every caller did before this
    // change - is refused rather than served production's queue.
    let mut request = Request::new(PullRequest {
        queue_name: "jobs".to_string(),
        max_messages: 10,
        lease_seconds: 30,
    });
    request
        .metadata_mut()
        .insert(APP_ID_HEADER, "app-one".parse().unwrap());
    let status = client
        .pull(request)
        .await
        .expect_err("the application header alone must not identify a tenant");
    assert_eq!(status.code(), Code::PermissionDenied);

    for bad in [
        // A bare application id names no environment, so it names no tenant.
        "app-one",
        "../../etc",
        "a.b.c",
        "app-one.",
    ] {
        let status = client
            .pull(as_tenant(
                bad,
                PullRequest {
                    queue_name: "jobs".to_string(),
                    max_messages: 10,
                    lease_seconds: 30,
                },
            ))
            .await
            .expect_err("a malformed tenant must be refused");
        assert_eq!(
            status.code(),
            Code::PermissionDenied,
            "{bad} should be refused"
        );
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

    let live = send(&mut client, &production, "jobs", "live-work").await;
    let draft = send(&mut client, &preview, "jobs", "draft-work").await;

    let live_lease = pull(&mut client, &production, "jobs", 10, 30).await;
    assert_eq!(
        live_lease.len(),
        1,
        "production must see only its own message"
    );
    assert_eq!(live_lease[0].id, live);
    assert_eq!(live_lease[0].body, b"live-work");

    let draft_lease = pull(&mut client, &preview, "jobs", 10, 30).await;
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
        ack(&mut client, &preview, "jobs", handles(&live_lease)).await,
        (0, 0)
    );
    assert_eq!(
        nack(&mut client, &production, "jobs", handles(&draft_lease), 0).await,
        (0, 0)
    );
    assert_eq!(
        ack(&mut client, &production, "jobs", handles(&live_lease)).await,
        (1, 0)
    );
}

#[tokio::test]
async fn an_oversized_message_is_refused_and_nothing_is_queued() {
    let harness = Harness::start().await;
    let mut client = harness.client().await;

    let status = client
        .send(as_tenant(
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
        pull(&mut client, "app-one.production", "jobs", 10, 30)
            .await
            .is_empty()
    );
}

#[tokio::test]
async fn a_message_at_the_size_ceiling_still_round_trips() {
    let harness = Harness::start().await;
    let mut client = harness.client().await;
    let body = "x".repeat(MAX_BODY_BYTES);
    let id = send(&mut client, "app-one.production", "jobs", &body).await;

    let leased = pull(&mut client, "app-one.production", "jobs", 10, 30).await;
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
            "app-one.production",
            "jobs",
            &format!("m{i}"),
        )
        .await;
    }

    let mut consumers = Vec::new();
    for _ in 0..8 {
        let mut client = harness.client().await;
        consumers.push(tokio::spawn(async move {
            pull(&mut client, "app-one.production", "jobs", 5, 60).await
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
// applies, so the server is the thing being asked here.

#[tokio::test]
async fn a_signed_call_is_served_when_a_secret_is_configured() {
    let secret = "an-operator-secret";
    let harness = Harness::start_authenticated(secret).await;
    let tenant = tenant_id("app-one", "production");
    let mut client = harness.client().await;

    let sent = client
        .send(as_tenant_with_token(
            &tenant,
            secret,
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
        .pull(as_tenant_with_token(
            &tenant,
            secret,
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
async fn a_call_with_no_token_is_refused_once_a_secret_is_configured() {
    let harness = Harness::start_authenticated("an-operator-secret").await;
    let status = harness
        .client()
        .await
        .pull(as_tenant(
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
async fn a_token_minted_for_another_tenant_drains_nothing() {
    // Draining is destructive: a message another tenant's consumer takes is
    // one its owner never receives. This is the check that stops a token that
    // leaked out of one application from being used to do that.
    let secret = "an-operator-secret";
    let harness = Harness::start_authenticated(secret).await;
    let mine = tenant_id("app-one", "production");

    for claimed in [
        tenant_id("app-two", "production"),
        tenant_id("app-one", "preview"),
    ] {
        let status = harness
            .client()
            .await
            .pull(as_tenant_with_token(
                &claimed,
                secret,
                &mine,
                PullRequest {
                    queue_name: "jobs".to_string(),
                    max_messages: 10,
                    lease_seconds: 30,
                },
            ))
            .await
            .unwrap_err();
        assert_eq!(
            status.code(),
            Code::Unauthenticated,
            "a token for {mine} must not drain {claimed}"
        );
    }
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

    let error = server::serve(config, public, async {})
        .await
        .expect_err("a public bind with no auth_secret must not serve");
    assert!(
        format!("{error:#}").contains("auth_secret"),
        "the refusal must name the missing setting: {error:#}"
    );
}
