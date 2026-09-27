//! Keeping a [`KeyRing`] in step with the control plane's published JWKS.
//!
//! A verifier starts with an empty ring and a URL. The refresher fetches the
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

/// Fetch and parse the JWKS at `url`.
pub async fn fetch_jwks(
    client: &reqwest::Client,
    url: &str,
) -> Result<Vec<ed25519_dalek::VerifyingKey>, String> {
    let response = client
        .get(url)
        .send()
        .await
        .map_err(|e| format!("request failed: {e}"))?;
    let status = response.status();
    if !status.is_success() {
        return Err(format!("answered {status}"));
    }
    let jwks: Jwks = response
        .json()
        .await
        .map_err(|e| format!("body is not a JWKS document: {e}"))?;
    jwks.verifying_keys().map_err(|e| e.to_string())
}

/// Spawn the task that keeps `ring` current from `url`. Runs until the
/// process exits.
pub fn spawn_refresher(
    ring: KeyRing,
    url: String,
    policy: RefreshPolicy,
) -> tokio::task::JoinHandle<()> {
    let wanted = Arc::new(Notify::new());
    {
        let wanted = Arc::clone(&wanted);
        ring.on_unknown_kid(move |_| wanted.notify_one());
    }
    tokio::spawn(async move {
        let client = match reqwest::Client::builder()
            .timeout(policy.request_timeout)
            .build()
        {
            Ok(client) => client,
            Err(error) => {
                tracing::error!(
                    %error,
                    "Cannot build the HTTP client for the capability key refresher; no key will \
                     ever be loaded and every capability will be refused"
                );
                return;
            }
        };
        let mut backoff = policy.initial_backoff;
        loop {
            let fetched_at = Instant::now();
            let next = match fetch_jwks(&client, &url).await {
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
        format!("http://{addr}/jwks.json")
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
        spawn_refresher(ring.clone(), url, policy);
        eventually("the first fetch", || ring.get(&key_id(&first)).is_some()).await;

        // Rotation on the control plane: a token naming the new key arrives.
        *served.lock().unwrap() = Jwks::from_keys(&[first, second]);
        let before = *hits.lock().unwrap();
        for _ in 0..20 {
            let _ = ring.lookup(&key_id(&second));
        }
        eventually("the on-demand fetch", || ring.get(&key_id(&second)).is_some()).await;
        tokio::time::sleep(Duration::from_millis(400)).await;
        let fetches = *hits.lock().unwrap() - before;
        assert!(
            fetches <= 2,
            "twenty unknown kids within the gap cost {fetches} fetches"
        );
    }
}
