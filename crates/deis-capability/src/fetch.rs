//! Keeping a [`KeyRing`] in step with the control plane's published JWKS.
//!
//! A verifier starts with an empty ring and a [`ControlClient`] for the
//! control plane's machine channel, where the document is served at
//! [`crate::JWKS_PATH`]. The refresher fetches the
//! document at once (retrying with backoff until the control plane answers,
//! so a verifier may start before it), then again on a fixed interval, and
//! again whenever a token names a key the ring does not hold - rate limited,
//! so a stream of tokens carrying made-up `kid`s costs one fetch per
//! [`RefreshPolicy::min_on_demand_gap`] rather than one per request.
//!
//! A failed fetch never empties the ring: the keys already held go on
//! verifying until a fetch succeeds and replaces them.

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::Notify;
use tokio::time::Instant;

use crate::control::ControlClient;
use crate::keys::{Jwks, KeyRing};

/// Timings of the refresher. The defaults suit production; tests shorten
/// them.
#[derive(Debug, Clone)]
pub struct RefreshPolicy {
    /// How often the document is fetched when nothing asks for it sooner.
    /// Must be well below the control plane's publish lead, so a new key is
    /// known everywhere before anything is signed with it.
    pub periodic: Duration,
    /// The least time between two fetches triggered by unknown `kid`s.
    pub min_on_demand_gap: Duration,
    /// First retry delay after a failed fetch; doubles up to `max_backoff`.
    pub initial_backoff: Duration,
    pub max_backoff: Duration,
    /// Per-request timeout.
    pub request_timeout: Duration,
}

impl Default for RefreshPolicy {
    fn default() -> Self {
        Self {
            periodic: Duration::from_secs(300),
            min_on_demand_gap: Duration::from_secs(10),
            initial_backoff: Duration::from_secs(1),
            max_backoff: Duration::from_secs(30),
            request_timeout: Duration::from_secs(10),
        }
    }
}

/// Fetch and parse the control plane's JWKS.
pub async fn fetch_jwks(
    client: &ControlClient,
) -> Result<Vec<ed25519_dalek::VerifyingKey>, String> {
    let jwks: Jwks = client.get_json(crate::JWKS_PATH).await?;
    jwks.verifying_keys().map_err(|e| e.to_string())
}

/// Spawn the task that keeps `ring` current from the control plane `client`
/// reaches. Runs until the process exits.
pub fn spawn_refresher(
    ring: KeyRing,
    client: ControlClient,
    policy: RefreshPolicy,
) -> tokio::task::JoinHandle<()> {
    let wanted = Arc::new(Notify::new());
    {
        let wanted = Arc::clone(&wanted);
        ring.on_unknown_kid(move |_| wanted.notify_one());
    }
    let client = client.with_timeout(policy.request_timeout);
    let url = client.url_of(crate::JWKS_PATH);
    tokio::spawn(async move {
        let mut backoff = policy.initial_backoff;
        loop {
            let fetched_at = Instant::now();
            let next = match fetch_jwks(&client).await {
                Ok(keys) => {
                    ring.replace(keys);
                    backoff = policy.initial_backoff;
                    tracing::debug!(url = %url, kids = ?ring.kids(), "Refreshed capability verifying keys");
                    policy.periodic
                }
                Err(error) => {
                    let wait = backoff;
                    backoff = (backoff * 2).min(policy.max_backoff);
                    tracing::warn!(
                        url = %url,
                        %error,
                        held_keys = ring.kids().len(),
                        retry_secs = wait.as_secs_f32(),
                        "Could not fetch the control plane's capability keys; the keys already \
                         held keep verifying, and a token signed by any other key is refused \
                         until a fetch succeeds"
                    );
                    wait
                }
            };
            tokio::select! {
                _ = tokio::time::sleep(next) => {}
                _ = wanted.notified() => {
                    // Rate limit: at most one on-demand fetch per gap,
                    // counted from the previous fetch.
                    tokio::time::sleep_until(fetched_at + policy.min_on_demand_gap).await;
                }
            }
        }
    })
}

/// Fetch one signed revocation list. The poller is handed one of these
/// rather than a client so its tests can serve lists of their own making;
/// every process reaches the control plane through
/// [`control_revocation_source`].
pub type RevocationSource = Box<
    dyn Fn() -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<String, String>> + Send>>
        + Send
        + Sync,
>;

/// A [`RevocationSource`] reading the list from the control plane's machine
/// channel through `client`: under mutual TLS with this service's identity,
/// or plain HTTP only on loopback - the same rule, and the same client, the
/// JWKS is fetched with. The list is signed either way; the channel is what
/// keeps someone on the path from withholding every list to push a verifier
/// into degraded mode on purpose.
pub fn control_revocation_source(client: ControlClient) -> RevocationSource {
    Box::new(move || {
        let client = client.clone();
        Box::pin(async move {
            client
                .get_json::<crate::SignedRevocationList>(crate::revocation::REVOCATION_LIST_PATH)
                .await
                .map(|body| body.signed_list)
        })
    })
}

/// Keep `verifier` in step with the control plane's revocation list.
///
/// Makes the first attempt before returning, so a service that starts beside
/// a reachable control plane answers its first call with a list in force
/// rather than refusing it; until a list is adopted the verifier refuses every
/// capability ([`crate::CapabilityError::NoRevocationList`]). Then fetches
/// every `interval`; after a failure retries sooner, backing off to
/// `interval`. Every list is checked against the verifier's own key ring and
/// for freshness before it is adopted, so a list that was tampered with,
/// signed by anyone else or replayed is logged and ignored - it neither
/// changes what is revoked nor resets the degraded-mode clock.
///
/// Entering and leaving T8 degraded mode are each logged once, at `warn` and
/// `info`; `service` names this process in those lines.
pub async fn spawn_revocation_poller(
    verifier: crate::SharedVerifier,
    service: &'static str,
    interval: Duration,
    source: RevocationSource,
) -> tokio::task::JoinHandle<()> {
    {
        let mut guard = verifier.lock().unwrap_or_else(|e| e.into_inner());
        guard.set_pull_interval_secs(interval.as_secs().max(1));
    }
    let mut poller = Poller {
        verifier,
        service,
        interval,
        source,
        degraded: false,
        backoff: Duration::from_secs(1).min(interval),
    };
    let first_wait = poller.attempt().await;
    tokio::spawn(async move {
        let mut wait = first_wait;
        loop {
            tokio::time::sleep(wait).await;
            wait = poller.attempt().await;
        }
    })
}

/// The state of one revocation poller between attempts.
struct Poller {
    verifier: crate::SharedVerifier,
    service: &'static str,
    interval: Duration,
    source: RevocationSource,
    degraded: bool,
    backoff: Duration,
}

impl Poller {
    /// Fetch and adopt one list; answers how long to wait before the next.
    async fn attempt(&mut self) -> Duration {
        let service = self.service;
        let outcome = match (self.source)().await {
            Ok(signed) => {
                let keys = self
                    .verifier
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .key_ring()
                    .clone();
                crate::revocation::verify(&signed, &keys)
                    .map_err(|e| e.to_string())
                    .and_then(|list| {
                        let epoch = list.auth_epoch;
                        let revoked = list.revoked_jtis.len() + list.revoked_nodes.len();
                        self.verifier
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .apply_revocation_list(list)
                            .map(|()| (epoch, revoked))
                            .map_err(|e| e.to_string())
                    })
            }
            Err(error) => Err(error),
        };
        let wait = match outcome {
            Ok((auth_epoch, revoked)) => {
                tracing::debug!(
                    service,
                    auth_epoch,
                    revoked,
                    "Adopted the control plane's revocation list"
                );
                self.backoff = Duration::from_secs(1).min(self.interval);
                self.interval
            }
            Err(error) => {
                let never = self
                    .verifier
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .last_sync_unix()
                    .is_none();
                if never {
                    tracing::warn!(
                        service,
                        %error,
                        "Could not adopt the control plane's revocation list, and none has been \
                         adopted yet: every capability is refused until one is"
                    );
                } else {
                    tracing::warn!(
                        service,
                        %error,
                        "Could not adopt the control plane's revocation list; what was last \
                         adopted stays in force"
                    );
                }
                let wait = self.backoff;
                self.backoff = (self.backoff * 2).min(self.interval);
                wait
            }
        };
        let now_degraded = self
            .verifier
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_degraded();
        if now_degraded && !self.degraded {
            tracing::warn!(
                service,
                "Entered degraded mode: no revocation list for two poll intervals, so the \
                 control plane is treated as unreachable. Expired node capabilities are \
                 accepted for at most 24 hours past their expiry if they were valid when the \
                 last list arrived; revocations made since are not known here"
            );
        } else if !now_degraded && self.degraded {
            tracing::info!(
                service,
                "Left degraded mode: a fresh revocation list was adopted and expired capabilities are refused again"
            );
        }
        self.degraded = now_degraded;
        wait
    }
}

/// Poll the control plane's revocation list through `client` into
/// `verifier` every [`crate::DEFAULT_PULL_INTERVAL_SECS`].
///
/// `client` is the one this service fetches the JWKS with, so a service that
/// can verify capabilities at all can also learn which are revoked. Until the
/// first list arrives the verifier refuses every capability.
///
/// T8 grace is limited to nodes of `trust_domain`, this installation's: a
/// node name from another installation is never a node of this one.
pub async fn poll_revocations(
    verifier: crate::SharedVerifier,
    service: &'static str,
    client: ControlClient,
    trust_domain: &str,
) -> tokio::task::JoinHandle<()> {
    verifier
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .set_trust_domain(trust_domain);
    let client = client.with_timeout(RefreshPolicy::default().request_timeout);
    spawn_revocation_poller(
        verifier,
        service,
        Duration::from_secs(crate::DEFAULT_PULL_INTERVAL_SECS),
        control_revocation_source(client),
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keys::key_id;
    use ed25519_dalek::SigningKey;
    use std::sync::Mutex;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// A one-route HTTP server answering every request with whatever JWKS
    /// `served` holds at that moment, and counting the requests.
    async fn serve(served: Arc<Mutex<Jwks>>, hits: Arc<Mutex<u32>>) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let (mut socket, _) = listener.accept().await.unwrap();
                let body = serde_json::to_string(&*served.lock().unwrap()).unwrap();
                *hits.lock().unwrap() += 1;
                let mut buf = [0u8; 1024];
                let _ = socket.read(&mut buf).await;
                let response = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = socket.write_all(response.as_bytes()).await;
            }
        });
        format!("http://{addr}")
    }

    async fn eventually(what: &str, mut check: impl FnMut() -> bool) {
        for _ in 0..200 {
            if check() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("timed out waiting for {what}");
    }

    /// A one-route HTTP server answering every request with whatever body
    /// `served` holds at that moment.
    async fn serve_text(served: Arc<Mutex<String>>) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let (mut socket, _) = listener.accept().await.unwrap();
                let body = served.lock().unwrap().clone();
                let mut buf = [0u8; 1024];
                let _ = socket.read(&mut buf).await;
                let response = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = socket.write_all(response.as_bytes()).await;
            }
        });
        format!("http://{addr}{}", crate::revocation::REVOCATION_LIST_PATH)
    }

    fn list_body(signed: String) -> String {
        serde_json::to_string(&crate::SignedRevocationList {
            signed_list: signed,
        })
        .unwrap()
    }

    /// The whole loop over a socket: a revoked jti is refused within one
    /// poll, a list edited on the way is not adopted (what was adopted stays
    /// in force), and an epoch bump refuses every older token.
    #[tokio::test]
    async fn the_poller_adopts_signed_lists_and_ignores_tampered_ones() {
        use crate::revocation::{RevocationList, sign};
        use crate::{AUD_STORE, CapabilityClaims, CapabilityError, CapabilityVerifier, encode};

        let signing = SigningKey::from_bytes(&[3u8; 32]);
        let tenant = "app.production";
        let claims = CapabilityClaims::new(
            AUD_STORE,
            "iss",
            "spiffe://deis.internal/deislet/node/n1",
            1,
            Duration::from_secs(900),
            crate::resources_for_audience(tenant, AUD_STORE),
        );
        let token = encode(&claims, &signing);
        let verifier = CapabilityVerifier::new(signing.verifying_key()).shared();
        let check = |verifier: &crate::SharedVerifier| {
            verifier
                .lock()
                .unwrap()
                .verify_with_grace(&token, AUD_STORE, tenant, "kv")
        };
        // Nothing adopted yet: an unexpired token is refused, not waved
        // through on the strength of a list this verifier has never seen.
        assert!(matches!(
            check(&verifier),
            Err(CapabilityError::NoRevocationList)
        ));

        let served = Arc::new(Mutex::new(list_body(sign(
            &RevocationList::new("iss", 1, vec![claims.jti.clone()]),
            &signing,
        ))));
        let url = serve_text(Arc::clone(&served)).await;
        spawn_revocation_poller(
            Arc::clone(&verifier),
            "test",
            Duration::from_millis(100),
            control_revocation_source(ControlClient::loopback(&url).unwrap()),
        )
        .await;
        eventually("the revocation", || {
            matches!(check(&verifier), Err(CapabilityError::Revoked { .. }))
        })
        .await;
        assert!(!verifier.lock().unwrap().is_degraded());

        // Someone on the path strips the revocation and rolls the epoch
        // back, keeping the signature.
        let genuine = sign(
            &RevocationList::new("iss", 1, vec![claims.jti.clone()]),
            &signing,
        );
        let (payload, signature) = crate::parse_token(&genuine).unwrap();
        let mut edited: serde_json::Value = serde_json::from_slice(&payload).unwrap();
        edited["revoked_jtis"] = serde_json::json!([]);
        edited["auth_epoch"] = 0.into();
        *served.lock().unwrap() = list_body(format!(
            "v1.{}.{}",
            crate::base64url(&serde_jcs::to_vec(&edited).unwrap()),
            crate::base64url(&signature)
        ));
        tokio::time::sleep(Duration::from_millis(400)).await;
        assert!(
            matches!(check(&verifier), Err(CapabilityError::Revoked { .. })),
            "a tampered list was adopted"
        );

        // Mass revocation.
        *served.lock().unwrap() = list_body(sign(&RevocationList::new("iss", 2, vec![]), &signing));
        eventually("the epoch bump", || {
            matches!(
                check(&verifier),
                Err(CapabilityError::AuthEpochTooLow { .. })
            )
        })
        .await;
    }

    /// A control plane that never answers leaves the verifier refusing every
    /// capability, however long it waits; the first list it serves opens
    /// the door.
    #[tokio::test]
    async fn nothing_is_accepted_before_the_first_list_arrives() {
        use crate::revocation::{RevocationList, sign};
        use crate::{AUD_STORE, CapabilityClaims, CapabilityError, CapabilityVerifier, encode};

        let signing = SigningKey::from_bytes(&[5u8; 32]);
        let tenant = "app.production";
        let claims = CapabilityClaims::new(
            AUD_STORE,
            "iss",
            "spiffe://deis.internal/deislet/node/n1",
            1,
            Duration::from_secs(900),
            crate::resources_for_audience(tenant, AUD_STORE),
        );
        let token = encode(&claims, &signing);
        let verifier = CapabilityVerifier::new(signing.verifying_key()).shared();
        let answering = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let source: RevocationSource = {
            let answering = Arc::clone(&answering);
            let signing = signing.clone();
            Box::new(move || {
                let answer = if answering.load(std::sync::atomic::Ordering::SeqCst) {
                    Ok(list_body_signed(&signing))
                } else {
                    Err("connection refused".to_string())
                };
                Box::pin(async move { answer })
            })
        };
        fn list_body_signed(signing: &SigningKey) -> String {
            sign(&RevocationList::new("iss", 1, vec![]), signing)
        }
        spawn_revocation_poller(
            Arc::clone(&verifier),
            "test",
            Duration::from_millis(50),
            source,
        )
        .await;
        let check = || {
            verifier
                .lock()
                .unwrap()
                .verify_with_grace(&token, AUD_STORE, tenant, "kv")
        };
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(matches!(check(), Err(CapabilityError::NoRevocationList)));

        answering.store(true, std::sync::atomic::Ordering::SeqCst);
        eventually("the first list", || check().is_ok()).await;
    }

    #[tokio::test]
    async fn the_ring_follows_the_published_set_on_demand_and_rate_limited() {
        let first = SigningKey::from_bytes(&[1u8; 32]).verifying_key();
        let second = SigningKey::from_bytes(&[2u8; 32]).verifying_key();
        let served = Arc::new(Mutex::new(Jwks::from_keys(&[first])));
        let hits = Arc::new(Mutex::new(0));
        let url = serve(Arc::clone(&served), Arc::clone(&hits)).await;

        let ring = KeyRing::new();
        let policy = RefreshPolicy {
            periodic: Duration::from_secs(3600),
            min_on_demand_gap: Duration::from_millis(300),
            ..RefreshPolicy::default()
        };
        spawn_refresher(
            ring.clone(),
            ControlClient::loopback(&url).expect("a loopback URL"),
            policy,
        );
        eventually("the first fetch", || ring.get(&key_id(&first)).is_some()).await;

        // Rotation on the control plane: a token naming the new key arrives.
        *served.lock().unwrap() = Jwks::from_keys(&[first, second]);
        let before = *hits.lock().unwrap();
        for _ in 0..20 {
            let _ = ring.lookup(&key_id(&second));
        }
        eventually("the on-demand fetch", || {
            ring.get(&key_id(&second)).is_some()
        })
        .await;
        tokio::time::sleep(Duration::from_millis(400)).await;
        let fetches = *hits.lock().unwrap() - before;
        assert!(
            fetches <= 2,
            "twenty unknown kids within the gap cost {fetches} fetches"
        );
    }
}
