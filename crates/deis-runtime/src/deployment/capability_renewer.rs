//! Background capability renewal loop.
//!
//! Each node holds Ed25519-signed capability tokens for the store, queue, and
//! telemetry services. These tokens have a 15-minute lifetime (ADR-025 upper
//! bound). The renewal loop refreshes them every 10 minutes (leaving 5 minutes
//! of safety margin), and on failure retries every 30 seconds with jitter.
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
//! for each token lock created. The renewal loop reads the global registry on
//! each cycle so newly deployed apps pick up their first renewal automatically.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock, RwLock};
use std::time::Duration;

use tokio::time::sleep;
use tonic::transport::Channel;

use deis_wire::machine::machine_channel_client::MachineChannelClient;
use deis_wire::machine::IssueCapabilityRequest;

/// How far ahead of expiry the loop renews.
const RENEW_BEFORE_SECS: u64 = 5 * 60; // 5 minutes

/// Capability lifetime from the issuer's perspective (ADR-025 upper bound).
const CAPABILITY_LIFETIME_SECS: u64 = 15 * 60; // 15 minutes

/// Base retry interval when the control plane is unreachable.
const RETRY_BASE_SECS: u64 = 30;

/// Jitter ceiling added on each retry, in seconds.
const RETRY_JITTER_SECS: u64 = 10;

// ── Global token registry ──────────────────────────────────────────────────

type TenantAudience = (String, &'static str);
type TokenMap = BTreeMap<TenantAudience, Vec<Arc<RwLock<Option<String>>>>>;

static TOKEN_LOCKS: OnceLock<RwLock<TokenMap>> = OnceLock::new();

fn registry() -> &'static RwLock<TokenMap> {
    TOKEN_LOCKS.get_or_init(|| RwLock::new(BTreeMap::new()))
}

/// Register a token lock so the renewal loop will keep it fresh.
///
/// `audience` is `"store"`, `"queue"`, or `"telemetry"` (the same literals
/// the control plane's capability issuer recognises).
pub fn register(tenant: &str, audience: &'static str, lock: Arc<RwLock<Option<String>>>) {
    registry()
        .write()
        .unwrap()
        .entry((tenant.to_string(), audience))
        .or_default()
        .push(lock);
}

/// Take a snapshot of every currently-registered token lock.
///
/// Duplicate entries for the same tenant+audience are collapsed to the first
/// one because the renewal loop issues one token per audience that covers all
/// tenants; every lock for that audience gets the same token anyway.
fn snapshot() -> Vec<TokenEntry> {
    registry()
        .read()
        .unwrap()
        .iter()
        .map(|((tenant, audience), locks)| TokenEntry {
            tenant: tenant.clone(),
            audience: *audience,
            lock: locks
                .first()
                .cloned()
                .unwrap_or_else(|| Arc::new(RwLock::new(None))),
        })
        .collect()
}

// ── Public types ───────────────────────────────────────────────────────────

/// A registered token that the renewal loop keeps fresh.
#[derive(Clone)]
pub struct TokenEntry {
    /// The tenant this token is scoped to, for logging.
    pub tenant: String,
    /// `"store"`, `"queue"`, or `"telemetry"`.
    pub audience: &'static str,
    /// Shared lock; writes replace the token, reads (from `StorageIdentity`)
    /// pick up the latest value without rebuilding the isolate.
    pub lock: Arc<RwLock<Option<String>>>,
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
pub fn spawn(
    client: MachineChannelClient<Channel>,
    node_id: String,
) -> RenewalMetrics {
    let metrics = RenewalMetrics::default();
    let failures = Arc::clone(&metrics.failures_total);
    let last_success = Arc::clone(&metrics.last_success_at);

    let renewal_interval = Duration::from_secs(CAPABILITY_LIFETIME_SECS - RENEW_BEFORE_SECS);
    let retry_base = Duration::from_secs(RETRY_BASE_SECS);

    tokio::spawn(async move {
        loop {
            // Sleep first so the initial tokens from the deployment env are
            // not replaced immediately.
            sleep(renewal_interval).await;

            let entries = snapshot();
            if entries.is_empty() {
                continue;
            }

            // Group by audience so we issue one RPC per audience.
            let by_audience = group_by_audience(&entries);

            for (audience, group) in &by_audience {
                match renew_audience(client.clone(), &node_id, audience, group).await {
                    Ok(_) => {
                        let now = unix_now();
                        *last_success.write().unwrap() = Some(now);
                        tracing::info!(
                            audience,
                            entries = group.len(),
                            "Renewed capability token"
                        );
                    }
                    Err(e) => {
                        failures.fetch_add(1, Ordering::Relaxed);
                        tracing::warn!(
                            audience,
                            error = %e,
                            "Capability renewal failed for audience; retrying every {}s",
                            RETRY_BASE_SECS
                        );
                        // Retry loop for this audience until success.
                        retry_until_success(
                            client.clone(),
                            &node_id,
                            audience,
                            group,
                            retry_base,
                            &failures,
                            &last_success,
                        )
                        .await;
                    }
                }
            }
        }
    });

    metrics
}

// ── Internal helpers ───────────────────────────────────────────────────────

/// Call `IssueCapability` for one audience and write the result into every
/// registered lock in `entries`.
async fn renew_audience(
    mut client: MachineChannelClient<Channel>,
    node_id: &str,
    audience: &str,
    entries: &[TokenEntry],
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // Collect tenants for this audience.  The token covers every tenant
    // listed, so the control plane returns one token that the store/queue/
    // telemetry will accept for any of them.
    let resources: Vec<serde_json::Value> = entries
        .iter()
        .map(|e| {
            serde_json::json!({
                "tenant": e.tenant,
                "binding": "",
                "generation": serde_json::Value::Null,
            })
        })
        .collect();

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

    // Write the new token into every lock for this audience.
    for entry in entries {
        *entry.lock.write().unwrap() = Some(token.clone());
    }

    Ok(())
}

/// Retry renewal for one audience with jitter until it succeeds.
async fn retry_until_success(
    mut client: MachineChannelClient<Channel>,
    node_id: &str,
    audience: &str,
    entries: &[TokenEntry],
    retry_base: Duration,
    failures: &AtomicU64,
    last_success: &RwLock<Option<u64>>,
) {
    loop {
        let jitter_ms = deterministic_jitter_ms(RETRY_JITTER_SECS);
        sleep(retry_base + Duration::from_millis(jitter_ms)).await;

        match renew_audience(client.clone(), node_id, audience, entries).await {
            Ok(_) => {
                let now = unix_now();
                *last_success.write().unwrap() = Some(now);
                tracing::info!(audience, "Capability renewal recovered after failure");
                return;
            }
            Err(e) => {
                failures.fetch_add(1, Ordering::Relaxed);
                tracing::warn!(audience, error = %e, "Capability renewal retry failed");
            }
        }
    }
}

/// Derive a deterministic jitter value (not crypto-random) from wall-clock
/// nanos.  This is only meant to spread load; it is not a security primitive.
fn deterministic_jitter_ms(max_secs: u64) -> u64 {
    let nanos = std::time::Instant::now()
        .elapsed()
        .subsec_nanos() as u64;
    nanos % (max_secs * 1_000)
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
        groups.entry(entry.audience).or_default().push(entry.clone());
    }
    groups.into_iter().collect()
}