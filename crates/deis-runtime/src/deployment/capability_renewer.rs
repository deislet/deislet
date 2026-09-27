//! Background capability renewal loop.
//!
//! Each node holds Ed25519-signed capability tokens for the store, queue, and
//! telemetry services. These tokens have a 15-minute lifetime (ADR-025 upper
//! bound). The renewal loop refreshes them roughly every 10 minutes (leaving
//! 5 minutes of safety margin, minus a random jitter so a fleet restarted
//! together does not renew in lockstep), and after a failure retries every 30
//! seconds plus jitter until it succeeds.
//!
//! The renewal loop is spawned as a tokio task by the assignment control loop,
//! which holds a [`MachineChannelClient`] connected to the control plane over
//! mTLS. Each token lives behind an [`Arc<RwLock<Option<String>>>`] shared with
//! the [`StorageIdentity`]s that stamp it onto gRPC requests, so a successful
//! renewal reaches every in-flight storage client without rebuilding isolates.
//!
//! # Registration
//!
//! Call [`register`] from [`AppServices::connect()`](crate::runtime::AppServices::connect)
//! for each token lock created. The registry holds the locks weakly: when the
//! last client holding a lock is dropped (the isolate is replaced or the app
//! removed), the entry disappears on the next cycle instead of being renewed
//! for the life of the process.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock, RwLock, Weak};
use std::time::Duration;

use tokio::time::sleep;
use tonic::transport::Channel;

use deis_capability::{CapabilityResource, resources_for_audience};
use deis_wire::machine::IssueCapabilityRequest;
use deis_wire::machine::machine_channel_client::MachineChannelClient;

/// How far ahead of expiry the loop renews.
const RENEW_BEFORE_SECS: u64 = 5 * 60; // 5 minutes

/// Capability lifetime from the issuer's perspective (ADR-025 upper bound).
const CAPABILITY_LIFETIME_SECS: u64 = 15 * 60; // 15 minutes

/// Most jitter taken off the renewal interval, in seconds. Subtracted, never
/// added, so a renewal is never later than [`RENEW_BEFORE_SECS`] before expiry.
const RENEW_JITTER_SECS: u64 = 60;

/// Base retry interval when the control plane is unreachable.
const RETRY_BASE_SECS: u64 = 30;

/// Jitter ceiling added on each retry, in seconds.
const RETRY_JITTER_SECS: u64 = 10;

// ── Global token registry ──────────────────────────────────────────────────

type TokenLock = Arc<RwLock<Option<String>>>;
type TenantAudience = (String, &'static str);
type TokenMap = BTreeMap<TenantAudience, Vec<Weak<RwLock<Option<String>>>>>;

static TOKEN_LOCKS: OnceLock<RwLock<TokenMap>> = OnceLock::new();

fn registry() -> &'static RwLock<TokenMap> {
    TOKEN_LOCKS.get_or_init(|| RwLock::new(BTreeMap::new()))
}

/// Register a token lock so the renewal loop will keep it fresh for as long
/// as anything else holds it.
///
/// `audience` is `"store"`, `"queue"`, or `"telemetry"` (the same literals
/// the control plane's capability issuer recognises).
pub fn register(tenant: &str, audience: &'static str, lock: TokenLock) {
    let mut map = registry().write().unwrap();
    let locks = map.entry((tenant.to_string(), audience)).or_default();
    if !locks
        .iter()
        .any(|held| held.upgrade().is_some_and(|held| Arc::ptr_eq(&held, &lock)))
    {
        locks.push(Arc::downgrade(&lock));
    }
}

/// Take a snapshot of every live token lock, dropping the ones nothing holds
/// any more - and the tenant+audience entries left with no lock at all.
fn snapshot() -> Vec<TokenEntry> {
    let mut map = registry().write().unwrap();
    map.retain(|_, locks| {
        locks.retain(|lock| lock.strong_count() > 0);
        !locks.is_empty()
    });
    map.iter()
        .map(|((tenant, audience), locks)| TokenEntry {
            tenant: tenant.clone(),
            audience: *audience,
            locks: locks.iter().filter_map(Weak::upgrade).collect(),
        })
        .filter(|entry| !entry.locks.is_empty())
        .collect()
}

// ── Public types ───────────────────────────────────────────────────────────

/// Every registered token for one tenant and audience.
#[derive(Clone)]
pub struct TokenEntry {
    /// The tenant these tokens are scoped to.
    pub tenant: String,
    /// `"store"`, `"queue"`, or `"telemetry"`.
    pub audience: &'static str,
    /// Every live lock for the pair - one per isolate or generation that
    /// built a client. Writes replace the token; reads (from
    /// `StorageIdentity`) pick up the latest value without rebuilding.
    pub locks: Vec<TokenLock>,
}

/// Exported metrics from the renewal loop, intended to be wired to
/// Prometheus `deis_capability_renew_failures_total` and a health gauge.
#[derive(Debug, Default)]
pub struct RenewalMetrics {
    /// Total number of failed renewal attempts since start.
    pub failures_total: Arc<AtomicU64>,
    /// Unix timestamp of the most recent successful renewal, or `None` if
    /// none has completed yet.
    pub last_success_at: Arc<RwLock<Option<u64>>>,
}

// ── Spawn ──────────────────────────────────────────────────────────────────

/// Spawns a background task that keeps every registered token fresh.
///
/// The returned [`RenewalMetrics`] are updated by the task and can be
/// inspected for observability (Prometheus `/metrics`, health endpoints).
pub fn spawn(client: MachineChannelClient<Channel>, node_id: String) -> RenewalMetrics {
    let metrics = RenewalMetrics::default();
    let failures = Arc::clone(&metrics.failures_total);
    let last_success = Arc::clone(&metrics.last_success_at);

    tokio::spawn(async move {
        // Sleep first so the initial tokens from the deployment env are not
        // replaced immediately.
        let mut wait = renewal_interval();
        loop {
            sleep(wait).await;

            // One RPC per audience, each covering every tenant registered
            // for it. A failure of one audience does not hold up the others;
            // it only shortens the wait before the next cycle.
            let mut failed = false;
            for (audience, group) in group_by_audience(&snapshot()) {
                match renew_audience(client.clone(), &node_id, audience, &group).await {
                    Ok(()) => {
                        *last_success.write().unwrap() = Some(unix_now());
                        tracing::info!(
                            audience,
                            tenants = group.len(),
                            "Renewed capability token"
                        );
                    }
                    Err(e) => {
                        failed = true;
                        failures.fetch_add(1, Ordering::Relaxed);
                        tracing::warn!(
                            audience,
                            error = %e,
                            "Capability renewal failed for audience; retrying in about {}s",
                            RETRY_BASE_SECS
                        );
                    }
                }
            }
            wait = if failed {
                retry_interval()
            } else {
                renewal_interval()
            };
        }
    });

    metrics
}

// ── Internal helpers ───────────────────────────────────────────────────────

/// What to ask the control plane for: every binding of `audience` for every
/// tenant in `entries`, so the one token returned is accepted by each service
/// behind that audience for each of them.
fn resources_for(audience: &str, entries: &[TokenEntry]) -> Vec<CapabilityResource> {
    entries
        .iter()
        .flat_map(|entry| resources_for_audience(&entry.tenant, audience))
        .collect()
}

/// Write `token` into every lock of every entry.
fn apply_token(entries: &[TokenEntry], token: &str) {
    for lock in entries.iter().flat_map(|entry| &entry.locks) {
        *lock.write().unwrap() = Some(token.to_string());
    }
}

/// Call `IssueCapability` for one audience and write the result into every
/// registered lock in `entries`.
async fn renew_audience(
    mut client: MachineChannelClient<Channel>,
    node_id: &str,
    audience: &str,
    entries: &[TokenEntry],
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let resources = resources_for(audience, entries);
    if resources.is_empty() {
        return Err(format!("audience {audience} has no per-tenant bindings to renew").into());
    }

    let request = tonic::Request::new(IssueCapabilityRequest {
        node_id: node_id.to_string(),
        audience: audience.to_string(),
        resources_json: serde_json::to_string(&resources)?,
    });

    let response = client.issue_capability(request).await?;
    let token = response.into_inner().capability_token;

    // Validate: a token that is not ASCII cannot be sent as gRPC metadata.
    // This is a safety-net check; the control plane mints base64-encoded
    // tokens which are always ASCII.
    let _ = token
        .parse::<tonic::metadata::MetadataValue<tonic::metadata::Ascii>>()
        .map_err(|_| "capability token returned by control plane is not ASCII")?;

    apply_token(entries, &token);
    Ok(())
}

/// The wait before a normal renewal: the lifetime minus the safety margin,
/// minus up to [`RENEW_JITTER_SECS`].
fn renewal_interval() -> Duration {
    Duration::from_secs(CAPABILITY_LIFETIME_SECS - RENEW_BEFORE_SECS)
        - jitter(Duration::from_secs(RENEW_JITTER_SECS))
}

/// The wait before retrying after a failure.
fn retry_interval() -> Duration {
    Duration::from_secs(RETRY_BASE_SECS) + jitter(Duration::from_secs(RETRY_JITTER_SECS))
}

/// A uniformly random duration in `[0, max)`.
///
/// Drawn from the standard library's randomly seeded SipHash keys: each
/// `RandomState` gets fresh keys, so hashing nothing yields a new random
/// value per call. Only meant to spread load across a fleet; not a security
/// primitive.
fn jitter(max: Duration) -> Duration {
    use std::hash::{BuildHasher, Hasher};
    let max_ms = max.as_millis() as u64;
    if max_ms == 0 {
        return Duration::ZERO;
    }
    let random = std::collections::hash_map::RandomState::new()
        .build_hasher()
        .finish();
    Duration::from_millis(random % max_ms)
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// Group entries by audience so we issue one RPC per audience.
fn group_by_audience(entries: &[TokenEntry]) -> Vec<(&'static str, Vec<TokenEntry>)> {
    let mut groups: BTreeMap<&'static str, Vec<TokenEntry>> = BTreeMap::new();
    for entry in entries {
        groups
            .entry(entry.audience)
            .or_default()
            .push(entry.clone());
    }
    groups.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lock() -> TokenLock {
        Arc::new(RwLock::new(None))
    }

    fn entries_for(tenant: &str) -> Vec<TokenEntry> {
        snapshot()
            .into_iter()
            .filter(|entry| entry.tenant == tenant)
            .collect()
    }

    /// Every lock for a tenant is renewed, not just the first one: two
    /// isolates of one app each hold their own lock.
    #[test]
    fn every_lock_of_a_tenant_receives_the_renewed_token() {
        let tenant = "renew-all.production";
        let (first, second) = (lock(), lock());
        register(tenant, "store", Arc::clone(&first));
        register(tenant, "store", Arc::clone(&second));
        register(tenant, "store", Arc::clone(&first)); // not twice

        let entries = entries_for(tenant);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].locks.len(), 2);
        apply_token(&entries, "v1.renewed.sig");
        assert_eq!(first.read().unwrap().as_deref(), Some("v1.renewed.sig"));
        assert_eq!(second.read().unwrap().as_deref(), Some("v1.renewed.sig"));
    }

    /// The registry does not keep a lock alive, and forgets it once the
    /// clients holding it are gone.
    #[test]
    fn a_dropped_lock_leaves_the_registry() {
        let tenant = "renew-drop.production";
        let kept = lock();
        let dropped = lock();
        register(tenant, "queue", Arc::clone(&kept));
        register(tenant, "queue", Arc::clone(&dropped));
        drop(dropped);
        assert_eq!(entries_for(tenant)[0].locks.len(), 1);

        drop(kept);
        assert!(entries_for(tenant).is_empty());
        assert!(
            !registry()
                .read()
                .unwrap()
                .contains_key(&(tenant.to_string(), "queue")),
            "the empty entry is removed, not kept forever"
        );
    }

    /// The renewal asks for every binding the verifiers check - the empty
    /// binding it used to send matched none of them.
    #[test]
    fn a_renewal_requests_every_binding_of_the_audience() {
        let entries = vec![TokenEntry {
            tenant: "app.production".to_string(),
            audience: "store",
            locks: vec![],
        }];
        let bindings: Vec<String> = resources_for("store", &entries)
            .into_iter()
            .map(|r| r.binding)
            .collect();
        assert_eq!(bindings, ["kv", "sql", "blob"]);
        let queue: Vec<String> = resources_for("queue", &entries)
            .into_iter()
            .map(|r| r.binding)
            .collect();
        assert_eq!(queue, ["queue"]);
    }

    #[test]
    fn jitter_varies_and_stays_in_range() {
        let max = Duration::from_secs(10);
        let draws: Vec<Duration> = (0..32).map(|_| jitter(max)).collect();
        assert!(draws.iter().all(|d| *d < max));
        assert!(
            draws.iter().any(|d| *d != draws[0]),
            "32 draws were all {:?}",
            draws[0]
        );
        let interval = renewal_interval();
        assert!(interval <= Duration::from_secs(CAPABILITY_LIFETIME_SECS - RENEW_BEFORE_SECS));
        assert!(
            interval
                > Duration::from_secs(
                    CAPABILITY_LIFETIME_SECS - RENEW_BEFORE_SECS - RENEW_JITTER_SECS
                )
        );
    }
}
