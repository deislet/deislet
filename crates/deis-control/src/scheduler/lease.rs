//! Which control plane fires cron, when more than one shares a database.
//!
//! The design assumes one control plane, and `fire_due` used to assume it
//! silently: two processes started on one SQLite file - a manual start beside
//! a service unit, a blue/green cut-over that overlaps for a minute - each
//! rebuilt the same task list from the same catalog and each fired every
//! occurrence, so every scheduled side effect happened twice.
//!
//! This is not leader election. It is one row: whoever holds it fires, and
//! refreshes its timestamp every tick to say it is still there. A process
//! that finds the row held by somebody else, and fresh, fires nothing; it
//! still rebuilds its task list and advances past the occurrences it did not
//! fire, so that the moment it does take over it fires only what comes due
//! from then on, not a backlog. It takes over when the holder has not
//! refreshed for [`lease ttl`](crate::scheduler::CronScheduler) - a stopped
//! holder is noticed within a few ticks - or at once when the holder released
//! the row on its way out.
//!
//! What this does not do: it does not share the task list between processes
//! or hand over an occurrence mid-flight. An occurrence that comes due during
//! the hand-over window is skipped the way an occurrence that comes due while
//! no control plane runs is skipped, and both are said in the log. The two
//! processes also have to agree on the clock to within the ttl, which two
//! processes on one machine - the only way they share one SQLite file - do.

use rusqlite::OptionalExtension;

use crate::database::{Database, DatabaseError};

/// What one claim of the lease found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LeaseState {
    /// This process holds the lease and may fire.
    Held,
    /// Another process holds it and refreshed it recently enough.
    HeldElsewhere {
        holder: String,
        /// Seconds since that holder last refreshed, as of the claim.
        refreshed_secs_ago: i64,
    },
}

/// Take or refresh the lease for `holder`.
///
/// One statement decides it, so two processes claiming at the same instant
/// cannot both win: the row is written only when it is absent, already ours,
/// or older than `ttl_secs`. `now_secs` is the caller's clock rather than
/// SQLite's so that the scheduler's tick time and the lease agree, and so
/// that a test can move time without waiting.
pub async fn claim(
    db: &Database,
    holder: &str,
    now_secs: i64,
    ttl_secs: i64,
) -> Result<LeaseState, DatabaseError> {
    let holder = holder.to_string();
    db.with_connection(move |conn| {
        let taken = conn.execute(
            "INSERT INTO scheduler_lease (id, holder, refreshed_at) VALUES (1, ?1, ?2)
                 ON CONFLICT(id) DO UPDATE
                 SET holder = excluded.holder, refreshed_at = excluded.refreshed_at
                 WHERE scheduler_lease.holder = excluded.holder
                    OR scheduler_lease.refreshed_at <= ?2 - ?3",
            rusqlite::params![holder, now_secs, ttl_secs],
        )?;
        if taken > 0 {
            return Ok(LeaseState::Held);
        }
        let current: Option<(String, i64)> = conn
            .query_row(
                "SELECT holder, refreshed_at FROM scheduler_lease WHERE id = 1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        match current {
            Some((other, refreshed_at)) => Ok(LeaseState::HeldElsewhere {
                holder: other,
                refreshed_secs_ago: now_secs - refreshed_at,
            }),
            // The row vanished between the two statements: somebody released
            // it. This tick is theirs to have skipped; the next claim takes
            // it.
            None => Ok(LeaseState::HeldElsewhere {
                holder: "(released)".to_string(),
                refreshed_secs_ago: 0,
            }),
        }
    })
    .await
}

/// Give the lease up, if this process holds it, so another control plane
/// takes over on its next tick rather than after the ttl. Answers whether
/// there was one to give up.
pub async fn release(db: &Database, holder: &str) -> Result<bool, DatabaseError> {
    let holder = holder.to_string();
    db.with_connection(move |conn| {
        let released = conn.execute(
            "DELETE FROM scheduler_lease WHERE id = 1 AND holder = ?1",
            [holder],
        )?;
        Ok::<_, DatabaseError>(released > 0)
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn one_holder_at_a_time_and_a_stale_holder_is_replaced() {
        let db = Database::init(":memory:").await.unwrap();

        assert_eq!(claim(&db, "a", 1_000, 30).await.unwrap(), LeaseState::Held);
        // Refreshing is claiming again.
        assert_eq!(claim(&db, "a", 1_010, 30).await.unwrap(), LeaseState::Held);
        assert_eq!(
            claim(&db, "b", 1_020, 30).await.unwrap(),
            LeaseState::HeldElsewhere {
                holder: "a".to_string(),
                refreshed_secs_ago: 10
            }
        );
        // Still fresh at exactly the boundary minus one.
        assert!(matches!(
            claim(&db, "b", 1_039, 30).await.unwrap(),
            LeaseState::HeldElsewhere { .. }
        ));
        // Stale: b takes over, and a is now the one shut out.
        assert_eq!(claim(&db, "b", 1_040, 30).await.unwrap(), LeaseState::Held);
        assert!(matches!(
            claim(&db, "a", 1_041, 30).await.unwrap(),
            LeaseState::HeldElsewhere { ref holder, .. } if holder == "b"
        ));
    }

    #[tokio::test]
    async fn releasing_hands_over_at_once_and_only_by_the_holder() {
        let db = Database::init(":memory:").await.unwrap();
        claim(&db, "a", 1_000, 30).await.unwrap();

        assert!(
            !release(&db, "b").await.unwrap(),
            "b holds nothing to release"
        );
        assert!(matches!(
            claim(&db, "b", 1_001, 30).await.unwrap(),
            LeaseState::HeldElsewhere { .. }
        ));

        assert!(release(&db, "a").await.unwrap());
        assert_eq!(claim(&db, "b", 1_002, 30).await.unwrap(), LeaseState::Held);
        assert!(!release(&db, "a").await.unwrap());
    }
}
