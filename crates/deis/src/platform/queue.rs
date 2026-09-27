//! A gRPC client for one tenant's queues, as an operator uses them.
//!
//! Backs `deis queue dead-letters` and `deis queue replay`. Two calls out of
//! the seven deis-queue serves, and the other five are deliberately absent:
//! `Send`, `Pull`, `Ack` and `Nack` belong to the application and its node,
//! and a CLI that could pull would be a CLI that takes work out of the queue
//! the running consumer is meant to get - at-least-once delivery says the
//! message comes back, but only after the lease lapses, and nothing tells the
//! consumer why it went missing. What is left is the half of the service
//! nobody but a person can do: read what was given up on, and put chosen
//! messages back.
//!
//! # Identity on the wire
//!
//! The same two headers `deis kv` stamps - `x-deis-tenant` and the capability
//! token in `x-deis-storage-token` - because deis-queue checks the token the
//! same way deis-store does, against the same shared secret and derived from
//! the tenant the call itself claims (see `deis-queue/src/auth.rs`). So the
//! credential this CLI already fetches for storage is the one that works
//! here, and there is no second secret to manage.
//!
//! There is no binding: a binding names a namespace inside one tenant's
//! *storage*, and deis-queue has no such dimension - one tenant has one set
//! of queues. Accepting `--binding` here would have been a flag that changed
//! nothing.

use std::time::Duration;

use anyhow::{Result, anyhow, bail};
use tonic::Request;
use tonic::metadata::{Ascii, MetadataValue};
use tonic::transport::{Channel, Endpoint};

use deis_wire::conventions::{
    MAX_GRPC_MESSAGE_BYTES, STORAGE_TOKEN_HEADER, TENANT_HEADER, split_tenant,
};

mod queue_proto {
    tonic::include_proto!("deis.queue");
}

use queue_proto::queue_client::QueueClient;

/// How long dialling deis-queue may take before a call gives up.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// How long one call may run. Both calls here are a single bounded page or a
/// single bounded transaction on the server's side, so this is generous
/// rather than tight.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// One message deis-queue gave up on.
///
/// Mirrors `DeadLetterMessage` in `deis-queue/proto/queue.proto` so nothing
/// outside this module has to name a generated type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeadLetter {
    pub queue: String,
    /// The id it had while it was queued, and the id a replay takes.
    pub id: String,
    /// Whatever the producer sent. Bytes, not text: a body that is not valid
    /// UTF-8 is exactly the kind of message that ends up here.
    pub body: Vec<u8>,
    /// Deliveries before it was given up on.
    pub attempts: i32,
    /// Unix second it was first enqueued.
    pub created_at: i64,
    /// Unix second it was moved to the dead letter table.
    pub died_at: i64,
    /// Which of the two ways it ran out of attempts, in deis-queue's own
    /// words. `None` for a message dead lettered by a queue too old to record
    /// it, which is shown as unknown rather than guessed at.
    pub reason: Option<String>,
}

impl From<queue_proto::DeadLetterMessage> for DeadLetter {
    fn from(message: queue_proto::DeadLetterMessage) -> Self {
        Self {
            queue: message.queue_name,
            id: message.id,
            body: message.body,
            attempts: message.attempts,
            created_at: message.created_at,
            died_at: message.died_at,
            // Empty means "this queue recorded no reason", which is not the
            // same as a reason that happens to be blank - and the difference
            // is what stops the operator's view printing an empty cell where
            // it should print "unknown".
            reason: Some(message.reason).filter(|reason| !reason.is_empty()),
        }
    }
}

/// One page of the dead letter table.
#[derive(Debug, Clone, Default)]
pub struct DeadLetterPage {
    pub messages: Vec<DeadLetter>,
    /// `Some` while there is somewhere to resume from. Not the same as a
    /// full page: deis-queue also cuts a page short at a byte budget, and
    /// this is the only signal that says which happened.
    pub next_cursor: Option<String>,
}

/// What one replay did.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ReplayReport {
    /// Messages put back on the queue, each with its attempts reset.
    pub replayed: i32,
    /// Ids that were not in the dead letter table: already replayed, already
    /// swept by the retention, or never there at all.
    pub not_found: i32,
}

/// Who a call says it is, and the proof that goes with the claim.
///
/// Parsed once when the session is built so a tenant or token that cannot be
/// sent as gRPC metadata is refused here rather than on the first call an
/// operator happens to make.
#[derive(Clone)]
struct Identity {
    tenant_header: MetadataValue<Ascii>,
    token_header: Option<MetadataValue<Ascii>>,
}

impl Identity {
    fn new(tenant: String, token: Option<&str>) -> Result<Self> {
        if split_tenant(&tenant).is_none() {
            bail!(
                "{tenant:?} is not a queue tenant; it must be an application and an environment \
                 joined by '.', for example \"demo.production\". This is built from --app and \
                 --environment by deis_wire::conventions::tenant_id, so seeing this means one of \
                 them failed its own name check first."
            );
        }
        let tenant_header = tenant.parse::<MetadataValue<Ascii>>().map_err(|_| {
            anyhow!("tenant {tenant:?} cannot be sent as gRPC metadata; every call would fail")
        })?;
        let token_header = match token.map(str::trim).filter(|t| !t.is_empty()) {
            Some(token) => Some(token.parse::<MetadataValue<Ascii>>().map_err(|_| {
                anyhow!(
                    "the storage capability token for {tenant} cannot be sent as gRPC metadata; \
                     the control plane is expected to hand this CLI a base64 value"
                )
            })?),
            None => None,
        };
        Ok(Self {
            tenant_header,
            token_header,
        })
    }

    fn stamp<T>(&self, message: T) -> Request<T> {
        let mut request = Request::new(message);
        request
            .metadata_mut()
            .insert(TENANT_HEADER, self.tenant_header.clone());
        if let Some(token) = &self.token_header {
            request
                .metadata_mut()
                .insert(STORAGE_TOKEN_HEADER, token.clone());
        }
        request
    }
}

/// Never derived: a derived `Debug` would put the capability token into any
/// panic message or log line that formats this session, which is exactly
/// where a credential must never appear.
impl std::fmt::Debug for Identity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Identity")
            .field(
                "tenant",
                &self.tenant_header.to_str().unwrap_or("<unprintable>"),
            )
            .field(
                "token",
                &if self.token_header.is_some() {
                    "<present>"
                } else {
                    "<absent>"
                },
            )
            .finish()
    }
}

/// A connection to deis-queue, bound to one tenant.
#[derive(Clone)]
pub struct QueueSession {
    identity: Identity,
    queue: QueueClient<Channel>,
}

impl std::fmt::Debug for QueueSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("QueueSession")
            .field("identity", &self.identity)
            .finish()
    }
}

impl QueueSession {
    /// Bind to `tenant` at `endpoint`, presenting `token` on every call when
    /// one was handed to this session.
    ///
    /// Nothing dials yet - `connect_lazy` defers that to the first RPC - so a
    /// deis-queue that is down does not stop this session from being built,
    /// only from answering.
    pub fn connect(endpoint: &str, tenant: &str, token: Option<&str>) -> Result<Self> {
        let identity = Identity::new(tenant.to_string(), token)?;
        let channel = checked_endpoint(endpoint)?
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(REQUEST_TIMEOUT)
            .connect_lazy();
        Ok(Self {
            identity,
            queue: QueueClient::new(channel)
                .max_decoding_message_size(MAX_GRPC_MESSAGE_BYTES)
                .max_encoding_message_size(MAX_GRPC_MESSAGE_BYTES),
        })
    }

    /// One page of what this tenant has given up on, oldest death first.
    ///
    /// `queue` of `None` reads every queue of the tenant, which is where an
    /// operator starts: the question is "what is stuck?", not "what is stuck
    /// in this one queue". The limit is a request, not a promise - deis-queue
    /// clamps it and also stops early when the bodies would make a response
    /// too large to send.
    pub async fn dead_letters(
        &self,
        queue: Option<&str>,
        limit: Option<i32>,
        cursor: Option<String>,
    ) -> Result<DeadLetterPage> {
        let request = self.identity.stamp(queue_proto::ListDeadLettersRequest {
            // An empty name means every queue on the wire: proto3 cannot tell
            // "not set" from "empty string", and deis-queue refuses an empty
            // queue name everywhere else, so there is no second meaning to
            // lose.
            queue_name: queue.unwrap_or_default().to_string(),
            limit: limit.unwrap_or(0),
            cursor: cursor.unwrap_or_default(),
        });
        let response = self
            .queue
            .clone()
            .list_dead_letters(request)
            .await
            .map_err(|status| status_error(status, "listing the dead letters"))?
            .into_inner();
        Ok(DeadLetterPage {
            messages: response
                .messages
                .into_iter()
                .map(DeadLetter::from)
                .collect(),
            next_cursor: Some(response.next_cursor).filter(|cursor| !cursor.is_empty()),
        })
    }

    /// Put named messages back on `queue` with their attempts reset.
    ///
    /// Ids are named one by one on purpose, all the way down to deis-queue,
    /// which refuses a replay that names none: replaying a whole queue of
    /// poison messages is a mistake with no undo.
    pub async fn replay(
        &self,
        queue: &str,
        ids: Vec<String>,
        delay_seconds: Option<i32>,
    ) -> Result<ReplayReport> {
        let named = ids.len();
        let request = self.identity.stamp(queue_proto::ReplayDeadLettersRequest {
            queue_name: queue.to_string(),
            message_ids: ids,
            delay_seconds: delay_seconds.unwrap_or(0),
        });
        let response = self
            .queue
            .clone()
            .replay_dead_letters(request)
            .await
            .map_err(|status| {
                status_error(
                    status,
                    &format!("replaying {named} dead letter(s) onto queue {queue:?}"),
                )
            })?
            .into_inner();
        Ok(ReplayReport {
            replayed: response.replayed,
            not_found: response.not_found,
        })
    }
}

/// The address, refused here rather than at the first call if it is not one.
///
/// A bare `host:port` parses as a URI with no scheme, which tonic accepts and
/// only fails on when it actually dials - as a transport error that says
/// nothing about the missing `http://`. This address is typed by hand into
/// `deis.toml` or an environment variable, so the mistake is worth catching
/// where the fix can be named.
fn checked_endpoint(endpoint: &str) -> Result<Endpoint> {
    let uri: tonic::transport::Uri = endpoint.parse().map_err(|e| {
        anyhow!(
            "{endpoint:?} is not a usable address for deis-queue: {e}\nWrite it with a scheme, \
             for example http://127.0.0.1:8091."
        )
    })?;
    if !matches!(uri.scheme_str(), Some("http") | Some("https")) {
        bail!(
            "{endpoint:?} is not a usable address for deis-queue: it names no http scheme. Write \
             it as http://host:port, for example http://127.0.0.1:8091."
        );
    }
    Endpoint::from_shared(endpoint.to_string())
        .map_err(|e| anyhow!("{endpoint:?} is not a usable address for deis-queue: {e}"))
}

/// A gRPC failure as a sentence, with the one hint that is worth attaching.
///
/// `Unauthenticated` is the failure an operator will actually hit, and the
/// cause is never in the status text: deis-queue never says which secret it
/// checked against, because saying so would leak how the token is derived.
fn status_error(status: tonic::Status, doing: &str) -> anyhow::Error {
    let message = status.message().to_string();
    if status.code() == tonic::Code::Unauthenticated {
        return anyhow!(
            "deis-queue refused this CLI's capability token while {doing}: {message}\nThe token \
             is minted by the control plane from `storage_auth_secret` and checked by deis-queue \
             against its own `auth_secret`; when the two are not the same string, every call from \
             every node is being refused as well."
        );
    }
    anyhow!("deis-queue refused the call while {doing}: {message}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_tenant_that_is_not_two_parts_is_refused_before_any_call() {
        for bad in ["demo", "demo.production.extra", ""] {
            let error = Identity::new(bad.to_string(), None)
                .unwrap_err()
                .to_string();
            assert!(error.contains("queue tenant"), "{error}");
        }
        Identity::new("demo.production".to_string(), Some("token")).unwrap();
    }

    #[test]
    fn the_token_never_reaches_a_debug_line() {
        let identity = Identity::new("demo.production".to_string(), Some("s3cret")).unwrap();
        let rendered = format!("{identity:?}");
        assert!(!rendered.contains("s3cret"), "{rendered}");
        assert!(rendered.contains("<present>"), "{rendered}");
        assert!(rendered.contains("demo.production"), "{rendered}");
    }

    #[test]
    fn both_headers_are_stamped_on_every_call() {
        let identity = Identity::new("demo.production".to_string(), Some("token")).unwrap();
        let request = identity.stamp(());
        assert_eq!(
            request.metadata().get(TENANT_HEADER).unwrap(),
            "demo.production"
        );
        assert_eq!(
            request.metadata().get(STORAGE_TOKEN_HEADER).unwrap(),
            "token"
        );

        // No token is the loopback install with no secret configured, where
        // deis-queue takes the tenant at face value. The header is left off
        // rather than sent empty, which would be a token that never matches.
        let anonymous = Identity::new("demo.production".to_string(), None).unwrap();
        assert!(
            anonymous
                .stamp(())
                .metadata()
                .get(STORAGE_TOKEN_HEADER)
                .is_none()
        );
    }

    #[test]
    fn an_address_without_a_scheme_is_refused_with_an_example() {
        // Caught here rather than at the first call: dialling a scheme-less
        // address fails with a transport error that never mentions the
        // missing "http://", and this address is one a person types by hand.
        for written in ["127.0.0.1:8091", "grpc://127.0.0.1:8091"] {
            let error = checked_endpoint(written).unwrap_err().to_string();
            assert!(error.contains("http://127.0.0.1:8091"), "{error}");
        }
        checked_endpoint("http://127.0.0.1:8091").unwrap();
        checked_endpoint("https://queue.example").unwrap();
    }

    #[test]
    fn a_message_with_no_recorded_reason_reads_as_unknown_rather_than_blank() {
        let message = queue_proto::DeadLetterMessage {
            queue_name: "jobs".to_string(),
            id: "m1".to_string(),
            body: b"{}".to_vec(),
            attempts: 5,
            created_at: 1,
            died_at: 2,
            reason: String::new(),
        };
        assert_eq!(DeadLetter::from(message).reason, None);

        let recorded = queue_proto::DeadLetterMessage {
            reason: "the consumer reported it failed".to_string(),
            ..queue_proto::DeadLetterMessage {
                queue_name: "jobs".to_string(),
                id: "m2".to_string(),
                body: Vec::new(),
                attempts: 5,
                created_at: 1,
                died_at: 2,
                reason: String::new(),
            }
        };
        assert_eq!(
            DeadLetter::from(recorded).reason.as_deref(),
            Some("the consumer reported it failed")
        );
    }

    /// The whole caller path, over a socket, against the real service.
    ///
    /// `ListDeadLetters` and `ReplayDeadLetters` were implemented, registered
    /// and unit-tested with nothing anywhere calling them, so what was never
    /// checked is exactly this: that a client outside deis-queue can reach
    /// them, prove it may act as the tenant, read what is in the table and
    /// put a message back. A stub server would have proved none of it.
    ///
    /// The dead letter is produced the only way one is ever produced - a
    /// consumer that hands the message back until the attempts run out -
    /// using the queue's own client for the halves this CLI deliberately does
    /// not offer.
    #[tokio::test]
    async fn the_operator_view_reads_and_replays_a_real_dead_letter() {
        use deis_queue::proto::queue::{
            MessageHandle, NackRequest, PullRequest, SendRequest, queue_client::QueueClient,
        };

        const SECRET: &str = "shared-between-the-control-plane-and-the-queue";
        const TENANT: &str = "demo.production";
        const CEILING: i32 = 2;

        let data = tempfile::tempdir().unwrap();
        let signing_key = {
            let mut seed = [0u8; 32];
            seed[..SECRET.len()].copy_from_slice(SECRET.as_bytes());
            ed25519_dalek::SigningKey::from_bytes(&seed)
        };
        let verifying_key = signing_key.verifying_key();
        let config = deis_queue::config::Config {
            listen: "127.0.0.1:0".to_string(),
            data_dir: data.path().to_path_buf(),
            max_attempts: CEILING,
            capability_public_key: Some(hex::encode(verifying_key.as_bytes())),
            ..deis_queue::config::Config::default()
        };
        // Bound before anything is spawned, so the port is known and no part
        // of this test has to wait for the server to come up.
        let listener = deis_queue::server::bind(&config).await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
        let server = tokio::spawn(async move {
            deis_queue::server::serve(config, listener, async {
                let _ = stopped.await;
            }, verifying_key)
            .await
            .unwrap();
        });

        // The producer and consumer halves, which this CLI has no commands
        // for: an application enqueues, its node leases and hands back.
        let token = deis_wire::conventions::storage_token(SECRET, TENANT);
        // A function rather than a closure: it stamps three different request
        // types, and a closure would be fixed to whichever one it saw first.
        fn stamp<T>(token: &str, message: T) -> Request<T> {
            let mut request = Request::new(message);
            request
                .metadata_mut()
                .insert(TENANT_HEADER, TENANT.parse().unwrap());
            request
                .metadata_mut()
                .insert(STORAGE_TOKEN_HEADER, token.parse().unwrap());
            request
        }
        let mut producer = QueueClient::connect(endpoint.clone()).await.unwrap();
        let id = producer
            .send(stamp(
                &token,
                SendRequest {
                    queue_name: "jobs".to_string(),
                    message_body: b"{\"key\":\"order-7\"}".to_vec(),
                },
            ))
            .await
            .unwrap()
            .into_inner()
            .message_id;

        for _ in 0..CEILING {
            let leased = producer
                .pull(stamp(
                    &token,
                    PullRequest {
                        queue_name: "jobs".to_string(),
                        max_messages: 10,
                        lease_seconds: 30,
                    },
                ))
                .await
                .unwrap()
                .into_inner()
                .messages;
            assert_eq!(leased.len(), 1);
            producer
                .nack(stamp(
                    &token,
                    NackRequest {
                        queue_name: "jobs".to_string(),
                        delay_seconds: 0,
                        refund_attempt: false,
                        messages: leased
                            .iter()
                            .map(|message| MessageHandle {
                                id: message.id.clone(),
                                lease_token: message.lease_token.clone(),
                            })
                            .collect(),
                    },
                ))
                .await
                .unwrap();
        }

        // Everything from here is what an operator's command actually does.
        let session = QueueSession::connect(&endpoint, TENANT, Some(&token)).unwrap();
        let page = session.dead_letters(None, None, None).await.unwrap();
        assert_eq!(page.messages.len(), 1, "the queue gave up on exactly one");
        let dead = &page.messages[0];
        assert_eq!(dead.id, id);
        assert_eq!(dead.queue, "jobs");
        assert_eq!(dead.body, b"{\"key\":\"order-7\"}");
        assert_eq!(dead.attempts, CEILING);
        assert_eq!(
            dead.reason.as_deref(),
            Some("the consumer reported it failed"),
            "the operator has to be able to tell this from a lapsed lease"
        );
        assert!(dead.died_at >= dead.created_at);
        assert!(page.next_cursor.is_none(), "one page held everything");

        // And back out again, which is the half that changes something.
        assert_eq!(
            session
                .replay("jobs", vec![id.clone()], None)
                .await
                .unwrap(),
            ReplayReport {
                replayed: 1,
                not_found: 0
            }
        );
        assert!(
            session
                .dead_letters(None, None, None)
                .await
                .unwrap()
                .messages
                .is_empty(),
            "a replayed message leaves the dead letter table"
        );
        // Naming an id that is not there is reported, not an error: an
        // operator replaying from a list somebody else has already worked
        // through has to be told which of their ids did nothing.
        assert_eq!(
            session
                .replay("jobs", vec![id], None)
                .await
                .unwrap()
                .not_found,
            1
        );

        // A token minted for another tenant reaches nothing, and the refusal
        // is the one this module words itself.
        let other = deis_wire::conventions::storage_token(SECRET, "other-app.production");
        let refused = QueueSession::connect(&endpoint, TENANT, Some(&other))
            .unwrap()
            .dead_letters(None, None, None)
            .await
            .unwrap_err()
            .to_string();
        assert!(refused.contains("storage_auth_secret"), "{refused}");

        let _ = stop.send(());
        server.await.unwrap();
    }

    #[test]
    fn a_refused_token_says_which_two_settings_have_to_match() {
        let error = status_error(
            tonic::Status::unauthenticated("the x-deis-storage-token presented is not valid"),
            "listing the dead letters",
        )
        .to_string();
        assert!(error.contains("storage_auth_secret"), "{error}");
        assert!(error.contains("auth_secret"), "{error}");
    }
}
