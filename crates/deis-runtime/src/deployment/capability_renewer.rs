//! Background capability renewal loop.
//!
//! Each node holds Ed25519-signed capability tokens for the store and queue
//! services, which it asks the control plane for itself - over the machine
//! channel, under its own certificate's name. Nothing in a deployment carries
//! one. A token lives 15 minutes (ADR-025 upper bound) and is renewed once it
//! has 5 minutes or less left, minus a random jitter so a fleet restarted
//! together does not renew in lockstep; after a failure the loop retries every
//! 30 seconds plus jitter until it succeeds.
//!
//! A new tenant is not left waiting for a timer: [`register`] wakes the loop,
//! so the first capability for an application is asked for the moment its
//! storage client is built.
//!
//! The renewal loop is spawned as a tokio task by the assignment control loop,
//! which holds a [`MachineChannelClient`] connected to the control plane over
//! mTLS. Each token lives behind an [`Arc<RwLock<Option<String>>>`] shared with
//! the [`StorageIdentity`](crate::runtime::StorageIdentity)s that stamp it onto
//! gRPC requests, so a successful renewal reaches every in-flight storage
//! client without rebuilding isolates.
//!
//! # Registration
//!
//! Call [`register`] from [`AppServices::connect()`](crate::runtime::AppServices::connect)
//! for each token lock created. The registry holds the locks weakly: when the
//! last client holding a lock is dropped (the isolate is replaced or the app
//! removed), the entry disappears on the next cycle instead of being renewed
//! for the life of the process.
//!
//! # Across a restart with the control plane down
//!
//! What the registry holds is also what the node's offline cache keeps
//! ([`held`], [`adopt`]): a node that restarts while the control plane is
//! unreachable goes on presenting the capabilities it last had, which a
//! verifier in its degraded mode may still accept for a bounded time after
//! they expire. Nothing here extends a capability; it only keeps presenting
//! the newest one it was given.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock, RwLock, Weak};
use std::time::Duration;

use tokio::sync::Notify;
use tokio::time::sleep;
use tonic::transport::Channel;

use deis_capability::{resources_for_audience, CapabilityResource};
use deis_wire::machine::machine_channel_client::MachineChannelClient;
use deis_wire::machine::IssueCapabilityRequest;

/// How far ahead of expiry the loop renews.
const RENEW_BEFORE_SECS: u64 = 5 * 60; // 5 minutes

/// Capability lifetime from the issuer's perspective (ADR-025 upper bound),
/// assumed for a token whose own `exp` cannot be read.
const CAPABILITY_LIFETIME_SECS: u64 = 15 * 60; // 15 minutes

/// Most jitter taken off the renewal interval, in seconds. Subtracted, never
/// added, so a renewal is never later than [`RENEW_BEFORE_SECS`] before expiry.
const RENEW_JITTER_SECS: u64 = 60;

/// Base retry interval when the control plane is unreachable.
const RETRY_BASE_SECS: u64 = 30;

/// Jitter ceiling added on each retry, in seconds.
const RETRY_JITTER_SECS: u64 = 10;

/// How long the loop sleeps when it holds nothing at all. A registration
/// wakes it sooner.
const IDLE_SECS: u64 = 5 * 60;

/// How long after its expiry a held capability is still kept to present: the
/// longest a verifier in degraded mode accepts one (T8). Past that nobody
/// will take it, so it is not worth keeping or writing to disk.
pub const GRACE_MAX_SECS: u64 = 24 * 60 * 60;

// ── Global token registry ──────────────────────────────────────────────────

type TokenLock = Arc<RwLock<Option<String>>>;
type TenantAudience = (String, &'static str);

/// Everything registered for one tenant and audience.
#[derive(Default)]
struct Slot {
    locks: Vec<Weak<RwLock<Option<String>>>>,
    /// The newest capability the control plane issued for this pair.
    held: Option<Held>,
    /// Taken from the offline cache and not yet claimed by any client. Kept
    /// without a lock until one registers; any other slot left with no lock
    /// is dropped.
    adopted: bool,
}

/// A capability this node holds, and when it expires.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Held {
    token: String,
    exp: u64,
}

type TokenMap = BTreeMap<TenantAudience, Slot>;

static TOKEN_LOCKS: OnceLock<RwLock<TokenMap>> = OnceLock::new();
static WAKE: OnceLock<Notify> = OnceLock::new();

fn registry() -> &'static RwLock<TokenMap> {
    TOKEN_LOCKS.get_or_init(|| RwLock::new(BTreeMap::new()))
}

fn wake() -> &'static Notify {
    WAKE.get_or_init(Notify::new)
}

/// The audience literal the registry keys on, for a string read off disk.
fn audience_literal(audience: &str) -> Option<&'static str> {
    match audience {
        "store" => Some("store"),
        "queue" => Some("queue"),
        "telemetry" => Some("telemetry"),
        _ => None,
    }
}

/// Register a token lock so the renewal loop will keep it fresh for as long
/// as anything else holds it.
///
/// `audience` is `"store"`, `"queue"`, or `"telemetry"` (the same literals
/// the control plane's capability issuer recognises).
///
/// An empty lock is handed the capability already held for the pair, if
/// there is one; a lock that already carries a value (an in-process preset)
/// is left as it is until the next renewal. When nothing usable is held the
/// loop is woken, so the first capability is asked for now rather than on
/// the next tick.
pub fn register(tenant: &str, audience: &'static str, lock: TokenLock) {
    let now = unix_now();
    let due = {
        let mut map = registry().write().unwrap();
        let slot = map.entry((tenant.to_string(), audience)).or_default();
        if !slot
            .locks
            .iter()
            .any(|held| held.upgrade().is_some_and(|held| Arc::ptr_eq(&held, &lock)))
        {
            slot.locks.push(Arc::downgrade(&lock));
        }
        slot.adopted = false;
        if let Some(held) = &slot.held {
            let mut value = lock.write().unwrap();
            if value.as_deref().is_none_or(str::is_empty) {
                *value = Some(held.token.clone());
            }
        }
        slot.held.as_ref().is_none_or(|held| is_due(held, now))
    };
    if due {
        wake().notify_one();
    }
}

/// Whether `held` should be renewed at `now`.
fn is_due(held: &Held, now: u64) -> bool {
    held.exp <= now.saturating_add(RENEW_BEFORE_SECS)
}

/// A capability as the offline cache keeps it.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct HeldCapability {
    pub tenant: String,
    pub audience: String,
    pub token: String,
    pub exp: u64,
}

/// Every capability this node currently holds that a verifier could still
/// accept, for the offline cache.
pub fn held() -> Vec<HeldCapability> {
    let now = unix_now();
    registry()
        .read()
        .unwrap()
        .iter()
        .filter_map(|((tenant, audience), slot)| {
            let held = slot.held.as_ref()?;
            (now <= held.exp.saturating_add(GRACE_MAX_SECS)).then(|| HeldCapability {
                tenant: tenant.clone(),
                audience: (*audience).to_string(),
                token: held.token.clone(),
                exp: held.exp,
            })
        })
        .collect()
}

/// Take capabilities back from the offline cache.
///
/// Only a capability newer than what is already held for its pair is taken,
/// and one past [`GRACE_MAX_SECS`] after its expiry is dropped: no verifier
/// would accept it. Returns how many were taken.
pub fn adopt(capabilities: Vec<HeldCapability>) -> usize {
    let now = unix_now();
    let mut adopted = 0;
    let mut map = registry().write().unwrap();
    for capability in capabilities {
        let Some(audience) = audience_literal(&capability.audience) else {
            continue;
        };
        if now > capability.exp.saturating_add(GRACE_MAX_SECS) {
            continue;
        }
        let slot = map.entry((capability.tenant, audience)).or_default();
        if slot
            .held
            .as_ref()
            .is_some_and(|held| held.exp >= capability.exp)
        {
            continue;
        }
        for lock in slot.locks.iter().filter_map(Weak::upgrade) {
            let mut value = lock.write().unwrap();
            if value.as_deref().is_none_or(str::is_empty) {
                *value = Some(capability.token.clone());
            }
        }
        slot.adopted = slot.locks.is_empty();
        slot.held = Some(Held {
            token: capability.token,
            exp: capability.exp,
        });
        adopted += 1;
    }
    adopted
}

/// Take a snapshot of every live token lock, dropping the ones nothing holds
/// any more - and the tenant+audience entries left with no lock at all.
fn snapshot() -> Vec<TokenEntry> {
    let now = unix_now();
    let mut map = registry().write().unwrap();
    map.retain(|_, slot| {
        slot.locks.retain(|lock| lock.strong_count() > 0);
        // A capability adopted from the offline cache is kept until the
        // isolate that needs it registers, which is moments later - or until
        // no verifier would take it any more.
        !slot.locks.is_empty()
            || (slot.adopted
                && slot
                    .held
                    .as_ref()
                    .is_some_and(|held| now <= held.exp.saturating_add(GRACE_MAX_SECS)))
    });
    map.iter()
        .map(|((tenant, audience), slot)| TokenEntry {
            tenant: tenant.clone(),
            audience: *audience,
            locks: slot.locks.iter().filter_map(Weak::upgrade).collect(),
            held_exp: slot.held.as_ref().map(|held| held.exp),
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
    /// When the capability held for the pair expires; `None` before the
    /// first one arrives.
    held_exp: Option<u64>,
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

/// Called after every successful renewal, with nothing: whoever needs the
/// new state reads it through [`held`]. The offline cache is written this
/// way.
pub type OnRenewed = Arc<dyn Fn() + Send + Sync>;

// ── Spawn ──────────────────────────────────────────────────────────────────

/// Spawns a background task that keeps every registered token fresh.
///
/// The returned [`RenewalMetrics`] are updated by the task and can be
/// inspected for observability (Prometheus `/metrics`, health endpoints).
pub fn spawn(
    client: MachineChannelClient<Channel>,
    node_id: String,
    on_renewed: Option<OnRenewed>,
) -> RenewalMetrics {
    let metrics = RenewalMetrics::default();
    let failures = Arc::clone(&metrics.failures_total);
    let last_success = Arc::clone(&metrics.last_success_at);

    tokio::spawn(async move {
        loop {
            // One RPC per audience that has anything due, each covering every
            // tenant registered for it. A failure of one audience does not
            // hold up the others; it only shortens the wait before the next
            // cycle.
            let now = unix_now();
            let mut failed = false;
            let mut renewed = false;
            for (audience, group) in group_by_audience(&snapshot()) {
                if !group.iter().any(|entry| entry_is_due(entry, now)) {
                    continue;
                }
                match renew_audience(client.clone(), &node_id, audience, &group).await {
                    Ok(()) => {
                        renewed = true;
                        *last_success.write().unwrap() = Some(unix_now());
                        tracing::info!(audience, tenants = group.len(), "Renewed capability token");
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
            if renewed {
                if let Some(on_renewed) = &on_renewed {
                    on_renewed();
                }
            }
            let wait = if failed {
                retry_interval()
            } else {
                next_renewal_in(&snapshot(), unix_now())
            };
            tokio::select! {
                () = sleep(wait) => {}
                () = wake().notified() => {}
            }
        }
    });

    metrics
}

// ── Internal helpers ───────────────────────────────────────────────────────

fn entry_is_due(entry: &TokenEntry, now: u64) -> bool {
    entry
        .held_exp
        .is_none_or(|exp| exp <= now.saturating_add(RENEW_BEFORE_SECS))
}

/// How long until the first held capability falls due, less up to
/// [`RENEW_JITTER_SECS`]; [`IDLE_SECS`] when nothing is held.
fn next_renewal_in(entries: &[TokenEntry], now: u64) -> Duration {
    let Some(first_exp) = entries.iter().filter_map(|entry| entry.held_exp).min() else {
        return Duration::from_secs(IDLE_SECS);
    };
    let due_at = first_exp.saturating_sub(RENEW_BEFORE_SECS);
    let until = Duration::from_secs(due_at.saturating_sub(now));
    until
        .saturating_sub(jitter(Duration::from_secs(RENEW_JITTER_SECS)))
        .max(Duration::from_secs(1))
}

/// What to ask the control plane for: every binding of `audience` for every
/// tenant in `entries`, so the one token returned is accepted by each service
/// behind that audience for each of them.
fn resources_for(audience: &str, entries: &[TokenEntry]) -> Vec<CapabilityResource> {
    entries
        .iter()
        .flat_map(|entry| resources_for_audience(&entry.tenant, audience))
        .collect()
}

/// Write `token` into every lock of every entry, and remember it as what
/// each pair holds.
fn apply_token(entries: &[TokenEntry], token: &str) {
    let exp = expiry_of(token).unwrap_or_else(|| unix_now() + CAPABILITY_LIFETIME_SECS);
    let mut map = registry().write().unwrap();
    for entry in entries {
        for lock in &entry.locks {
            *lock.write().unwrap() = Some(token.to_string());
        }
        if let Some(slot) = map.get_mut(&(entry.tenant.clone(), entry.audience)) {
            slot.held = Some(Held {
                token: token.to_string(),
                exp,
            });
        }
    }
}

/// The `exp` a capability token claims, read without verifying it: this node
/// holds no key to verify with and needs the value only to schedule its own
/// renewal.
fn expiry_of(token: &str) -> Option<u64> {
    #[derive(serde::Deserialize)]
    struct Expiry {
        exp: u64,
    }
    let (payload, _) = deis_capability::parse_token(token).ok()?;
    serde_json::from_slice::<Expiry>(&payload)
        .ok()
        .map(|claims| claims.exp)
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
            held_exp: None,
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
    }

    fn entry(held_exp: Option<u64>) -> TokenEntry {
        TokenEntry {
            tenant: "app.production".to_string(),
            audience: "store",
            locks: vec![],
            held_exp,
        }
    }

    /// Nothing held yet is due at once; a fresh 15 minute capability is due
    /// with 5 minutes left, never later.
    #[test]
    fn a_capability_is_due_with_five_minutes_left() {
        let now = 1_000_000;
        assert!(entry_is_due(&entry(None), now));
        assert!(!entry_is_due(&entry(Some(now + 15 * 60)), now));
        assert!(entry_is_due(&entry(Some(now + 5 * 60)), now));
        assert!(entry_is_due(&entry(Some(now - 1)), now));

        let wait = next_renewal_in(&[entry(Some(now + 15 * 60))], now);
        assert!(wait <= Duration::from_secs(10 * 60), "{wait:?}");
        assert!(
            wait > Duration::from_secs(10 * 60 - RENEW_JITTER_SECS),
            "{wait:?}"
        );
        assert_eq!(
            next_renewal_in(&[], now),
            Duration::from_secs(IDLE_SECS),
            "nothing held, nothing to schedule"
        );
    }

    fn token_expiring_at(exp: u64) -> String {
        let key = ed25519_dalek::SigningKey::from_bytes(&[7u8; 32]);
        let mut claims = deis_capability::CapabilityClaims::new(
            "store",
            "spiffe://deislet/control",
            "spiffe://deislet/node/n1",
            1,
            Duration::from_secs(900),
            vec![],
        );
        claims.exp = exp;
        deis_capability::encode(&claims, &key)
    }

    #[test]
    fn the_expiry_is_read_off_the_token() {
        assert_eq!(expiry_of(&token_expiring_at(1234)), Some(1234));
        assert_eq!(expiry_of("not a token"), None);
    }

    /// The first capability of a tenant is asked for when its client is
    /// built, not a renewal interval later: registering wakes the loop.
    #[tokio::test]
    async fn registering_a_tenant_with_nothing_held_wakes_the_loop() {
        let kept = lock();
        register("renew-wake.production", "store", Arc::clone(&kept));
        tokio::time::timeout(Duration::from_secs(1), wake().notified())
            .await
            .expect("the loop was not woken");
    }

    /// A capability taken back from the offline cache reaches a client that
    /// registers afterwards, and one no verifier would take any more is not
    /// taken back at all.
    #[test]
    fn an_adopted_capability_reaches_the_clients_that_register_later() {
        let now = unix_now();
        let token = token_expiring_at(now - 60);
        let taken = adopt(vec![
            HeldCapability {
                tenant: "renew-adopt.production".to_string(),
                audience: "store".to_string(),
                token: token.clone(),
                exp: now - 60,
            },
            HeldCapability {
                tenant: "renew-adopt-stale.production".to_string(),
                audience: "store".to_string(),
                token: "v1.stale".to_string(),
                exp: now - GRACE_MAX_SECS - 60,
            },
        ]);
        assert_eq!(taken, 1);
        assert!(held()
            .iter()
            .any(|capability| capability.tenant == "renew-adopt.production"));

        let client = lock();
        register("renew-adopt.production", "store", Arc::clone(&client));
        assert_eq!(client.read().unwrap().as_deref(), Some(token.as_str()));

        let stale = lock();
        register("renew-adopt-stale.production", "store", Arc::clone(&stale));
        assert_eq!(*stale.read().unwrap(), None);
    }

    /// A preset token is not overwritten by what the registry holds.
    #[test]
    fn a_preset_token_is_left_alone_at_registration() {
        let now = unix_now();
        adopt(vec![HeldCapability {
            tenant: "renew-preset.production".to_string(),
            audience: "queue".to_string(),
            token: token_expiring_at(now + 600),
            exp: now + 600,
        }]);
        let preset = Arc::new(RwLock::new(Some("v1.preset".to_string())));
        register("renew-preset.production", "queue", Arc::clone(&preset));
        assert_eq!(preset.read().unwrap().as_deref(), Some("v1.preset"));
    }
}
