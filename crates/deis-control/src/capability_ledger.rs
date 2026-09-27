//! What this control plane has signed, and what it has taken back.
//!
//! Every capability [`crate::capability_issuer::CapabilityIssuer`] signs is
//! written here before it is handed out: its `jti`, who it was minted for,
//! its audience, key, epoch, lifetime and every (tenant, binding) it grants.
//! That record is what makes revocation more than "bump the epoch and hurt
//! everybody": an operator can take back one token by `jti`, or every token a
//! node or a tenant holds, because this table says which ones those are.
//!
//! A revocation names the capability's expiry as well, so the list verifiers
//! poll ([`Self::revoked_jtis`]) carries only what could still be accepted
//! and stays small however much is ever revoked.

use std::sync::Arc;

use deis_capability::CapabilityClaims;
use rusqlite::{Connection, params};

use crate::database::{Database, DatabaseError};

/// How long past its expiry a revoked `jti` stays on the list.
///
/// A verifier accepts an expired node capability under T8 grace only if it
/// had not expired, give or take the skew, when that verifier last adopted a
/// list - so a revocation must still be on the list that verifier adopted.
/// Two poll intervals plus skew covers every list a verifier can have
/// adopted while the token could still pass that test; this is comfortably
/// more.
pub const LIST_MARGIN_SECS: i64 = 120;

/// How long an issuance record is kept after the capability expired: long
/// enough to answer "what did this node hold last week", short enough that a
/// fleet renewing every ten minutes does not grow the table for ever.
pub const ISSUANCE_RETENTION_SECS: i64 = 30 * 24 * 3600;

/// Why a set of capabilities is being revoked, as the `reason` column spells
/// it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RevocationScope {
    Jti(String),
    Node(String),
    Tenant(String),
}

impl RevocationScope {
    fn reason(&self) -> String {
        match self {
            RevocationScope::Jti(_) => "jti".to_string(),
            RevocationScope::Node(node) => format!("node:{node}"),
            RevocationScope::Tenant(tenant) => format!("tenant:{tenant}"),
        }
    }
}

/// The issuance and revocation tables, over the control database.
#[derive(Clone)]
pub struct CapabilityLedger {
    db: Arc<Database>,
}

impl CapabilityLedger {
    pub fn new(db: Arc<Database>) -> Self {
        Self { db }
    }

    /// The database the tables live in.
    pub fn database(&self) -> &Arc<Database> {
        &self.db
    }

    /// Write down a capability about to be handed out.
    pub async fn record(&self, claims: &CapabilityClaims) -> Result<(), DatabaseError> {
        let claims = claims.clone();
        self.db
            .with_connection(move |conn| record_on(conn, &claims))
            .await
    }

    /// Revoke every unexpired capability `scope` covers; returns their jtis.
    ///
    /// Revoking a `jti` that was never issued here, or has already expired,
    /// revokes nothing and returns an empty list - the caller decides whether
    /// that is an error.
    pub async fn revoke(&self, scope: RevocationScope) -> Result<Vec<String>, DatabaseError> {
        self.db
            .with_connection(move |conn| revoke_on(conn, &scope, now_secs()))
            .await
    }

    /// The jtis every verifier must refuse right now.
    pub async fn revoked_jtis(&self) -> Result<Vec<String>, DatabaseError> {
        self.db
            .with_connection(move |conn| revoked_jtis_on(conn, now_secs()))
            .await
    }

    /// Drop revocations no verifier can need any more, and issuance records
    /// past their retention. Returns how many of each went.
    pub async fn prune(&self) -> Result<(usize, usize), DatabaseError> {
        self.db
            .with_connection(move |conn| prune_on(conn, now_secs()))
            .await
    }
}

fn record_on(conn: &Connection, claims: &CapabilityClaims) -> Result<(), DatabaseError> {
    let node_id = deis_identity::SpiffeId::parse(&claims.sub)
        .ok()
        .and_then(|id| deis_identity::names::node_id_of(&id).map(str::to_string));
    let tx = conn.unchecked_transaction()?;
    tx.execute(
        "INSERT INTO capability_issuances
             (jti, subject, node_id, audience, kid, auth_epoch, issued_at, expires_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![
            claims.jti,
            claims.sub,
            node_id,
            claims.aud,
            claims.kid,
            to_i64(claims.auth_epoch),
            to_i64(claims.iat),
            to_i64(claims.exp),
        ],
    )?;
    for resource in &claims.resources {
        tx.execute(
            "INSERT INTO capability_issuance_resources (jti, tenant, binding, generation)
             VALUES (?1, ?2, ?3, ?4)",
            params![claims.jti, resource.tenant, resource.binding, resource.generation],
        )?;
    }
    tx.commit()?;
    Ok(())
}

fn revoke_on(
    conn: &Connection,
    scope: &RevocationScope,
    now: i64,
) -> Result<Vec<String>, DatabaseError> {
    let tx = conn.unchecked_transaction()?;
    let targets: Vec<(String, i64)> = {
        let (sql, key) = match scope {
            RevocationScope::Jti(jti) => (
                "SELECT jti, expires_at FROM capability_issuances
                 WHERE jti = ?1 AND expires_at > ?2",
                jti,
            ),
            RevocationScope::Node(node) => (
                "SELECT jti, expires_at FROM capability_issuances
                 WHERE node_id = ?1 AND expires_at > ?2",
                node,
            ),
            RevocationScope::Tenant(tenant) => (
                "SELECT DISTINCT i.jti, i.expires_at
                 FROM capability_issuances i
                 JOIN capability_issuance_resources r ON r.jti = i.jti
                 WHERE r.tenant = ?1 AND i.expires_at > ?2",
                tenant,
            ),
        };
        // Expired past the list margin: already refused everywhere, so there
        // is nothing to revoke.
        let mut statement = tx.prepare(sql)?;
        let rows = statement.query_map(params![key, now - LIST_MARGIN_SECS], |row| {
            Ok((row.get(0)?, row.get(1)?))
        })?;
        rows.collect::<Result<_, _>>()?
    };
    let reason = scope.reason();
    for (jti, expires_at) in &targets {
        tx.execute(
            "INSERT OR IGNORE INTO capability_revocations (jti, expires_at, revoked_at, reason)
             VALUES (?1, ?2, ?3, ?4)",
            params![jti, expires_at, now, reason],
        )?;
    }
    tx.commit()?;
    Ok(targets.into_iter().map(|(jti, _)| jti).collect())
}

fn revoked_jtis_on(conn: &Connection, now: i64) -> Result<Vec<String>, DatabaseError> {
    let mut statement = conn.prepare(
        "SELECT jti FROM capability_revocations WHERE expires_at > ?1 ORDER BY jti",
    )?;
    let rows = statement.query_map(params![now - LIST_MARGIN_SECS], |row| row.get(0))?;
    Ok(rows.collect::<Result<_, _>>()?)
}

fn prune_on(conn: &Connection, now: i64) -> Result<(usize, usize), DatabaseError> {
    let tx = conn.unchecked_transaction()?;
    let revocations = tx.execute(
        "DELETE FROM capability_revocations WHERE expires_at <= ?1",
        params![now - LIST_MARGIN_SECS],
    )?;
    // The resource rows and any remaining revocation go with their issuance
    // by the foreign keys' cascade.
    let issuances = tx.execute(
        "DELETE FROM capability_issuances WHERE expires_at <= ?1",
        params![now - ISSUANCE_RETENTION_SECS],
    )?;
    tx.commit()?;
    Ok((revocations, issuances))
}

fn to_i64(value: u64) -> i64 {
    value.try_into().unwrap_or(i64::MAX)
}

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
        .try_into()
        .unwrap_or(i64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use deis_capability::{AUD_QUEUE, AUD_STORE, resources_for_audience};
    use std::time::Duration;

    async fn ledger() -> CapabilityLedger {
        CapabilityLedger::new(Arc::new(Database::init(":memory:").await.unwrap()))
    }

    fn claims(aud: &str, sub: &str, tenant: &str) -> CapabilityClaims {
        CapabilityClaims::new(
            aud,
            "iss",
            sub,
            1,
            Duration::from_secs(900),
            resources_for_audience(tenant, aud),
        )
    }

    const NODE: &str = "spiffe://deis.internal/deislet/node/runtime-a";

    #[tokio::test]
    async fn revoking_by_node_or_tenant_finds_exactly_what_they_hold() {
        let ledger = ledger().await;
        let a_store = claims(AUD_STORE, NODE, "a.production");
        let a_queue = claims(AUD_QUEUE, NODE, "b.production");
        let other = claims(
            AUD_STORE,
            "spiffe://deis.internal/deislet/node/runtime-b",
            "b.production",
        );
        let human = claims(AUD_STORE, "spiffe://deislet/human/root", "a.production");
        for c in [&a_store, &a_queue, &other, &human] {
            ledger.record(c).await.unwrap();
        }

        let mut by_node = ledger
            .revoke(RevocationScope::Node("runtime-a".into()))
            .await
            .unwrap();
        by_node.sort();
        let mut expected = vec![a_store.jti.clone(), a_queue.jti.clone()];
        expected.sort();
        assert_eq!(by_node, expected);

        let mut by_tenant = ledger
            .revoke(RevocationScope::Tenant("b.production".into()))
            .await
            .unwrap();
        by_tenant.sort();
        let mut expected = vec![a_queue.jti.clone(), other.jti.clone()];
        expected.sort();
        assert_eq!(by_tenant, expected);

        let listed = ledger.revoked_jtis().await.unwrap();
        assert_eq!(listed.len(), 3);
        assert!(!listed.contains(&human.jti));
    }

    #[tokio::test]
    async fn an_unknown_or_long_expired_jti_revokes_nothing() {
        let ledger = ledger().await;
        let mut expired = claims(AUD_STORE, NODE, "a.production");
        expired.exp = expired.iat.saturating_sub(10_000);
        ledger.record(&expired).await.unwrap();
        assert!(
            ledger
                .revoke(RevocationScope::Jti("no-such".into()))
                .await
                .unwrap()
                .is_empty()
        );
        assert!(
            ledger
                .revoke(RevocationScope::Jti(expired.jti.clone()))
                .await
                .unwrap()
                .is_empty()
        );
    }

    /// The list is for what could still be accepted: a revocation drops off
    /// once its token is past expiry by more than the margin.
    #[tokio::test]
    async fn the_list_leaves_out_revocations_whose_tokens_are_long_expired() {
        let ledger = ledger().await;
        let live = claims(AUD_STORE, NODE, "a.production");
        ledger.record(&live).await.unwrap();
        ledger
            .revoke(RevocationScope::Jti(live.jti.clone()))
            .await
            .unwrap();
        ledger
            .db
            .with_connection(|conn| {
                conn.execute(
                    "UPDATE capability_revocations SET expires_at = expires_at - 10000",
                    [],
                )
                .map_err(DatabaseError::from)
            })
            .await
            .unwrap();
        assert!(ledger.revoked_jtis().await.unwrap().is_empty());
        let (revocations, _) = ledger.prune().await.unwrap();
        assert_eq!(revocations, 1);
    }
}
