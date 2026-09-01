//! The queue itself: enqueue, lease, acknowledge, return, dead letter.
//!
//! One SQLite file holds every tenant's messages, partitioned by the `tenant`
//! column and keyed on `(tenant, queue, id)`. Two tables:
//!
//! ```text
//! messages     what is still to be delivered, plus what is currently leased
//! dead_letter  what used up its attempts, kept for an operator
//! ```
//!
//! A leased message is not a separate state: it is a row whose `visible_at`
//! sits in the future. That is the whole lease mechanism, and it is why a
//! consumer crashing costs nothing - the row was never moved anywhere, so
//! when the clock passes `visible_at` it is simply ready again.
//!
//! Beside `visible_at` the row carries a `lease_token`, rewritten on every
//! delivery, and `Ack` and `Nack` match on it as well as on the id. That is
//! what stops the failure an id-only handle cannot see: a consumer whose
//! lease lapsed while it was still working finishes, acknowledges, and
//! deletes a message that has since been handed to somebody else - who is
//! part-way through the same work and will never be allowed to finish it.
//! With the token, the late acknowledgement matches nothing, is counted as a
//! stale lease and logged, and the current holder keeps its message.
//!
//! Note what the token does *not* do. A lease that lapses with nobody else
//! having taken the message still has the same token, so a late
//! acknowledgement of work genuinely done is honoured rather than rejected -
//! the fence closes only when the message actually belongs to someone else.
//!
//! Every read-then-write here runs in one `IMMEDIATE` transaction. `Pull` is
//! the reason: two consumers polling the same queue at the same moment both
//! read the same candidate rows, and whichever writes second must wait rather
//! than be refused. See [`crate::sqlite::SqlitePool::with_tx`].

use crate::Clock;
use crate::MAX_GRPC_MESSAGE_BYTES;
use crate::error::{QueueError, QueueResult};
use crate::proto::queue::{
    AckRequest, AckResponse, DeadLetterMessage, ListDeadLettersRequest, ListDeadLettersResponse,
    NackRequest, NackResponse, PullRequest, PullResponse, QueueMessage, ReplayDeadLettersRequest,
    ReplayDeadLettersResponse, SendBatchRequest, SendBatchResponse, SendRequest, SendResponse,
    queue_server::Queue,
};
use crate::sqlite::{PoolSize, SqlitePool};
use crate::tenant::{TenantId, tenant_of};
use rusqlite::{OptionalExtension, Transaction};
use tonic::{Request, Response, Status};

/// Longest accepted queue name. A name lives in a primary key and in log
/// lines; past this it is an accident, not a design.
const MAX_QUEUE_NAME_LEN: usize = 128;

/// Dead letters returned by `ListDeadLetters` when the caller asks for no
/// particular number. Small: an operator reads these, and a page is meant to
/// be looked at rather than scrolled past.
const DEFAULT_DEAD_LETTER_PAGE: i32 = 50;

/// Hard ceiling on one dead letter page, whatever the caller asks for.
const MAX_DEAD_LETTER_PAGE: i32 = 500;

/// A message given up on because the consumer said it could not handle it.
///
/// Stored on the row and not only logged, because it is the first thing an
/// operator reading the dead letter table has to know and the log line it was
/// written to has usually rotated away by then. This and
/// [`DIED_LEASE_LAPSED`] are the only two ways a message reaches that table,
/// and they call for different fixes: this one means the handler ran and
/// refused the body, so the body is what to look at.
const DIED_CONSUMER_FAILED: &str = "the consumer reported it failed";

/// A message given up on because nobody ever answered for it.
///
/// The other reason, and the one attempts alone cannot be told apart from:
/// each delivery lapsed without an `Ack` or a `Nack` - a handler slower than
/// its lease, or a node that died holding the message - so the body may be
/// perfectly good and the lease or the handler is what to look at.
const DIED_LEASE_LAPSED: &str = "its lease lapsed";

/// Ceiling on the bodies one `PullResponse` may carry.
///
/// Half the gRPC message ceiling, leaving the other half for framing and for
/// the ids and counters beside each body. A pull that would go over stops
/// early and leaves the rest for the next call, because a response the
/// transport refuses to encode would burn a delivery attempt on every message
/// in it without any of them reaching the consumer.
pub const MAX_PULL_RESPONSE_BYTES: usize = MAX_GRPC_MESSAGE_BYTES / 2;

/// The ceilings queue operations are held to.
#[derive(Debug, Clone)]
pub struct QueueLimits {
    /// Largest single message body accepted.
    pub max_body_bytes: usize,
    /// Most messages in one `SendBatch`.
    pub max_batch_messages: usize,
    /// Lease used when the caller does not ask for one.
    pub default_lease_secs: i64,
    /// Ceiling on a requested lease, and on a `Nack` delay.
    pub max_lease_secs: i64,
    /// Messages returned when the caller does not ask for a count.
    pub default_pull_messages: i32,
    /// Ceiling on a requested pull size, and on how many ids one `Ack` or
    /// `Nack` may name - those ids come from a pull, so the two bounds are
    /// the same bound.
    pub max_pull_messages: i32,
    /// How many times a message may be delivered before it is dead lettered.
    pub max_attempts: i32,
    /// How long a dead letter is kept. Zero keeps it forever.
    pub dead_letter_retention_secs: i64,
    /// Ceiling on the bodies one pull response may carry. Derived from the
    /// gRPC message ceiling rather than chosen by an operator; it is a field
    /// only so a test can lower it to something it can fill.
    pub max_pull_response_bytes: usize,
}

impl Default for QueueLimits {
    fn default() -> Self {
        // Kept in step with `config::Config::default`, which is what the
        // binary actually runs on; these are for tests and for callers
        // building the service directly.
        Self {
            max_body_bytes: 1024 * 1024,
            max_batch_messages: 100,
            default_lease_secs: 30,
            max_lease_secs: 3600,
            default_pull_messages: 10,
            max_pull_messages: 100,
            max_attempts: 5,
            dead_letter_retention_secs: 0,
            max_pull_response_bytes: MAX_PULL_RESPONSE_BYTES,
        }
    }
}

/// A message handed to a consumer, with its lease already taken.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LeasedMessage {
    pub id: String,
    pub body: Vec<u8>,
    /// Deliveries so far, including this one. Starts at 1.
    pub attempts: i32,
    /// Unix second this lease lapses.
    pub lease_expires_at: i64,
    pub created_at: i64,
    /// Proof that this delivery is the current one. Reissued on every
    /// delivery, and required by `Ack` and `Nack`; see the module docs.
    pub lease_token: String,
}

/// One message a consumer is answering for, and its proof of holding it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessageHandle {
    pub id: String,
    pub lease_token: String,
}

/// What one `Ack` did.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct AckOutcome {
    /// Messages deleted.
    pub acknowledged: usize,
    /// Handles whose message is still queued under a *newer* lease. Those
    /// rows were left alone: the message belongs to another consumer now.
    pub stale_leases: usize,
}

/// A message that used up its attempts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeadLetter {
    pub queue: String,
    pub id: String,
    pub body: Vec<u8>,
    pub attempts: i32,
    pub created_at: i64,
    pub died_at: i64,
    /// Which of the two ways it ran out of attempts: [`DIED_CONSUMER_FAILED`]
    /// or [`DIED_LEASE_LAPSED`]. Empty for a row written before the column
    /// existed, which a reader shows as unknown rather than guessing.
    pub reason: String,
}

/// What one `Nack` did.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct NackOutcome {
    /// Messages that will be delivered again.
    pub released: usize,
    /// Messages moved to the dead letter table instead.
    pub dead_lettered: usize,
    /// Handles whose message is still queued under a newer lease; see
    /// [`AckOutcome::stale_leases`].
    pub stale_leases: usize,
}

/// What one `ReplayDeadLetters` did.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ReplayOutcome {
    /// Messages put back on the queue with their attempts reset.
    pub replayed: usize,
    /// Ids that were not in the dead letter table at all.
    pub not_found: usize,
}

/// The queue service, backed by one shared SQLite file.
#[derive(Clone)]
pub struct QueueService {
    pool: SqlitePool,
    limits: QueueLimits,
    clock: Clock,
}

impl QueueService {
    /// Open `<data_dir>/queue.sqlite` and make sure the schema is there.
    pub async fn open(
        data_dir: &std::path::Path,
        connections: usize,
        limits: QueueLimits,
    ) -> QueueResult<Self> {
        Self::open_with_clock(data_dir, connections, limits, Clock::System).await
    }

    /// As [`Self::open`], but reading time from `clock`. Tests use a manual
    /// clock so a lease can lapse without waiting for it.
    pub async fn open_with_clock(
        data_dir: &std::path::Path,
        connections: usize,
        mut limits: QueueLimits,
        clock: Clock,
    ) -> QueueResult<Self> {
        if limits.max_body_bytes > limits.max_pull_response_bytes {
            tracing::warn!(
                configured = limits.max_body_bytes,
                limit = limits.max_pull_response_bytes,
                "max_body_bytes is larger than one pull response can carry; capping it, because a \
                 message accepted above this size could be stored and then never delivered"
            );
            limits.max_body_bytes = limits.max_pull_response_bytes;
        }
        if limits.max_attempts < 1 {
            tracing::warn!(
                configured = limits.max_attempts,
                "max_attempts is below one, which would dead letter every message before its \
                 first delivery; using one attempt instead"
            );
            limits.max_attempts = 1;
        }

        let pool = SqlitePool::new(
            data_dir.join("queue.sqlite"),
            "queue",
            PoolSize::new(connections),
        );
        pool.migrate(|conn| {
            // Before anything is created: a queue.sqlite from before the
            // tenant change would otherwise gain a second, empty pair of
            // tables beside the rows nobody can reach any more.
            for table in ["messages", "dead_letter"] {
                crate::legacy::reject_pre_tenant_table(conn, table)?;
            }
            conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS messages (
                     tenant      TEXT    NOT NULL,
                     queue       TEXT    NOT NULL,
                     id          TEXT    NOT NULL,
                     body        BLOB    NOT NULL,
                     attempts    INTEGER NOT NULL DEFAULT 0,
                     visible_at  INTEGER NOT NULL,
                     created_at  INTEGER NOT NULL,
                     -- Rewritten on every delivery; empty while the message
                     -- is waiting, because nobody holds it.
                     lease_token TEXT    NOT NULL DEFAULT '',
                     PRIMARY KEY (tenant, queue, id)
                 );
                 -- The one index Pull needs: it looks up a single tenant's
                 -- single queue and wants the rows that are ready first.
                 CREATE INDEX IF NOT EXISTS messages_ready
                     ON messages(tenant, queue, visible_at);
                 CREATE TABLE IF NOT EXISTS dead_letter (
                     tenant     TEXT    NOT NULL,
                     queue      TEXT    NOT NULL,
                     id         TEXT    NOT NULL,
                     body       BLOB    NOT NULL,
                     attempts   INTEGER NOT NULL,
                     created_at INTEGER NOT NULL,
                     died_at    INTEGER NOT NULL,
                     -- Why it was given up on; empty on a row written before
                     -- the queue recorded it.
                     reason     TEXT    NOT NULL DEFAULT '',
                     PRIMARY KEY (tenant, queue, id)
                 );
                 CREATE INDEX IF NOT EXISTS dead_letter_died_at
                     ON dead_letter(died_at);",
            )?;
            add_lease_token_column(conn)?;
            add_dead_letter_reason_column(conn)?;
            Ok(())
        })
        .await?;
        tracing::info!(
            path = %pool.path().display(),
            max_attempts = limits.max_attempts,
            default_lease_secs = limits.default_lease_secs,
            "queue ready"
        );
        Ok(Self {
            pool,
            limits,
            clock,
        })
    }

    /// The ceilings this service applies.
    pub fn limits(&self) -> &QueueLimits {
        &self.limits
    }

    /// Enqueue one message, visible immediately. Returns its id.
    pub async fn send(&self, tenant: &TenantId, queue: &str, body: Vec<u8>) -> QueueResult<String> {
        let ids = self.send_batch(tenant, queue, vec![body]).await?;
        Ok(ids
            .into_iter()
            .next()
            .expect("one body in, one id out of send_batch"))
    }

    /// Enqueue several messages as one transaction: either all of them are
    /// stored or none are. Returns the ids in the order the bodies were
    /// given.
    pub async fn send_batch(
        &self,
        tenant: &TenantId,
        queue: &str,
        bodies: Vec<Vec<u8>>,
    ) -> QueueResult<Vec<String>> {
        let queue = self.check_queue(queue)?;
        if bodies.is_empty() {
            return Err(QueueError::InvalidArgument(
                "send was given no message bodies".to_string(),
            ));
        }
        if bodies.len() > self.limits.max_batch_messages {
            tracing::warn!(
                tenant = %tenant,
                queue = %queue,
                messages = bodies.len(),
                limit = self.limits.max_batch_messages,
                "refusing an oversized batch; nothing in it was enqueued"
            );
            return Err(QueueError::TooLarge(format!(
                "batch holds {} messages, limit is {}",
                bodies.len(),
                self.limits.max_batch_messages
            )));
        }
        for body in &bodies {
            self.check_body(&queue, body.len())?;
        }

        let ids: Vec<String> = (0..bodies.len())
            .map(|_| uuid::Uuid::new_v4().to_string())
            .collect();
        let tenant_id = tenant.as_str().to_string();
        let now = self.clock.now();
        let rows: Vec<(String, Vec<u8>)> = ids.iter().cloned().zip(bodies).collect();
        let bytes: usize = rows.iter().map(|(_, body)| body.len()).sum();
        let queued = rows.len();
        let table_queue = queue.clone();

        self.pool
            .with_tx(move |tx| {
                let mut insert = tx.prepare(
                    "INSERT INTO messages
                            (tenant, queue, id, body, attempts, visible_at, created_at)
                     VALUES (?1, ?2, ?3, ?4, 0, ?5, ?5)",
                )?;
                for (id, body) in &rows {
                    insert.execute(rusqlite::params![tenant_id, table_queue, id, body, now])?;
                }
                Ok(())
            })
            .await?;

        tracing::debug!(tenant = %tenant, queue = %queue, messages = queued, bytes, "enqueued");
        Ok(ids)
    }

    /// Lease up to `max_messages` ready messages for `lease_seconds`.
    ///
    /// One transaction does three things, in this order: retire anything that
    /// came back visible with its attempts already used up, pick the messages
    /// that have been ready longest, and push their visibility past the lease
    /// while counting the delivery and issuing a fresh lease token. Splitting
    /// those apart would let a second consumer see a message this call has
    /// already claimed.
    ///
    /// The token is what [`Self::ack`] and [`Self::nack`] check: it is new on
    /// every delivery, so the previous holder of a message cannot act on it
    /// once it has moved on.
    pub async fn pull(
        &self,
        tenant: &TenantId,
        queue: &str,
        max_messages: Option<i32>,
        lease_seconds: Option<i32>,
    ) -> QueueResult<Vec<LeasedMessage>> {
        let queue = self.check_queue(queue)?;
        let wanted = self.clamp_pull_size(max_messages);
        let lease = self.clamp_lease(lease_seconds);
        let now = self.clock.now();
        let lease_expires_at = now.saturating_add(lease);
        let ceiling = self.limits.max_attempts;
        let budget = self.limits.max_pull_response_bytes;

        let tenant_id = tenant.as_str().to_string();
        let table_queue = queue.clone();
        let (leased, retired) = self
            .pool
            .with_tx(move |tx| {
                // A message whose lease lapsed after its last allowed attempt
                // is not owed another one. Retiring it here rather than
                // handing it out means the ceiling holds even for consumers
                // that die instead of reporting failure.
                let retired = retire_exhausted(tx, &tenant_id, &table_queue, now, ceiling)?;

                // Ready longest first, and among messages that became ready
                // in the same second, the one that arrived first. `rowid` is
                // what makes that second half true: `visible_at` has
                // one-second granularity, so a batch sent in one call would
                // otherwise come back in the order of its random ids. SQLite
                // hands out rowids in insertion order, and the only one it
                // ever reuses is the largest - a row that is by definition
                // newer than everything it would be compared against.
                let mut select = tx.prepare(
                    "SELECT id, body, attempts, created_at
                       FROM messages
                      WHERE tenant = ?1 AND queue = ?2 AND visible_at <= ?3
                      ORDER BY visible_at ASC, rowid ASC
                      LIMIT ?4",
                )?;
                // A token per message, not per pull: the row is what a later
                // Ack names, so the proof has to live on the row.
                let token = || uuid::Uuid::new_v4().to_string();
                let mut rows =
                    select.query(rusqlite::params![tenant_id, table_queue, now, wanted])?;

                let mut leased: Vec<LeasedMessage> = Vec::new();
                let mut used = 0usize;
                while let Some(row) = rows.next()? {
                    let body: Vec<u8> = row.get(1)?;
                    // The first message is always included even if it alone
                    // fills the budget: bodies are capped below the budget at
                    // startup, so this cannot loop forever on one message.
                    if !leased.is_empty() && used + body.len() > budget {
                        tracing::debug!(
                            tenant = %tenant_id,
                            queue = %table_queue,
                            returned = leased.len(),
                            "pull stopped at the response size budget; the rest stay ready for \
                             the next call"
                        );
                        break;
                    }
                    used += body.len();
                    leased.push(LeasedMessage {
                        id: row.get(0)?,
                        body,
                        attempts: row.get::<_, i32>(2)? + 1,
                        lease_expires_at,
                        created_at: row.get(3)?,
                        lease_token: token(),
                    });
                }
                drop(rows);
                drop(select);

                let mut lease_stmt = tx.prepare(
                    "UPDATE messages
                        SET visible_at = ?4, attempts = attempts + 1, lease_token = ?5
                      WHERE tenant = ?1 AND queue = ?2 AND id = ?3",
                )?;
                for message in &leased {
                    lease_stmt.execute(rusqlite::params![
                        tenant_id,
                        table_queue,
                        message.id,
                        lease_expires_at,
                        message.lease_token
                    ])?;
                }
                drop(lease_stmt);
                Ok((leased, retired))
            })
            .await?;

        for (id, attempts) in &retired {
            warn_dead_lettered(tenant, &queue, id, *attempts, ceiling, DIED_LEASE_LAPSED);
        }
        if leased.is_empty() {
            tracing::trace!(tenant = %tenant, queue = %queue, "pull found nothing ready");
        } else {
            tracing::debug!(
                tenant = %tenant,
                queue = %queue,
                returned = leased.len(),
                lease_seconds = lease,
                "leased messages"
            );
        }
        Ok(leased)
    }

    /// Delete handled messages.
    ///
    /// A handle whose message is not there is ignored, so a repeated
    /// acknowledgement is harmless. A handle whose token no longer matches
    /// the row is *not* acted on: the message has moved on since this
    /// consumer was given it - handed to another consumer, or put back on the
    /// queue - so deleting it would throw away work somebody else is in the
    /// middle of, or work that is queued to be done.
    pub async fn ack(
        &self,
        tenant: &TenantId,
        queue: &str,
        handles: Vec<MessageHandle>,
    ) -> QueueResult<AckOutcome> {
        let queue = self.check_queue(queue)?;
        if handles.is_empty() {
            return Ok(AckOutcome::default());
        }
        self.check_handles(&queue, &handles)?;
        let tenant_id = tenant.as_str().to_string();
        let table_queue = queue.clone();
        let asked = handles.len();

        let (removed, stale) = self
            .pool
            .with_tx(move |tx| {
                let mut delete = tx.prepare(
                    "DELETE FROM messages
                      WHERE tenant = ?1 AND queue = ?2 AND id = ?3 AND lease_token = ?4",
                )?;
                let mut still_queued = tx.prepare(
                    "SELECT 1 FROM messages WHERE tenant = ?1 AND queue = ?2 AND id = ?3",
                )?;
                let mut removed = 0usize;
                let mut stale = Vec::new();
                for handle in &handles {
                    let deleted = delete.execute(rusqlite::params![
                        tenant_id,
                        table_queue,
                        handle.id,
                        handle.lease_token
                    ])?;
                    if deleted > 0 {
                        removed += 1;
                        continue;
                    }
                    // Nothing deleted has two very different causes, and the
                    // difference is worth one extra lookup: the message is
                    // gone (already acknowledged - ordinary), or it is still
                    // there under a newer lease (this consumer lost it and is
                    // duplicating work - worth an operator's attention).
                    let present: Option<i64> = still_queued
                        .query_row(
                            rusqlite::params![tenant_id, table_queue, handle.id],
                            |row| row.get(0),
                        )
                        .optional()?;
                    if present.is_some() {
                        stale.push(handle.id.clone());
                    }
                }
                Ok((removed, stale))
            })
            .await?;

        for id in &stale {
            warn_stale_lease(tenant, &queue, id, "acknowledge");
        }
        if removed + stale.len() < asked {
            // Not an error: the usual cause is a retried acknowledgement of a
            // message that is already gone.
            tracing::debug!(
                tenant = %tenant,
                queue = %queue,
                acknowledged = removed,
                asked,
                "some acknowledged messages were no longer queued; they had already been \
                 acknowledged, or they were dead lettered in the meantime"
            );
        } else {
            tracing::debug!(
                tenant = %tenant,
                queue = %queue,
                acknowledged = removed,
                "acknowledged"
            );
        }
        Ok(AckOutcome {
            acknowledged: removed,
            stale_leases: stale.len(),
        })
    }

    /// Hand messages back unhandled. Each becomes visible again after
    /// `delay_seconds`, unless it has used up its attempts, in which case it
    /// is dead lettered instead.
    ///
    /// The lease token is checked exactly as it is in [`Self::ack`]: a
    /// consumer whose lease lapsed must not shorten, or end, the lease of
    /// whoever holds the message now.
    pub async fn nack(
        &self,
        tenant: &TenantId,
        queue: &str,
        handles: Vec<MessageHandle>,
        delay_seconds: Option<i32>,
    ) -> QueueResult<NackOutcome> {
        let queue = self.check_queue(queue)?;
        if handles.is_empty() {
            return Ok(NackOutcome::default());
        }
        self.check_handles(&queue, &handles)?;
        let delay = self.clamp_delay(delay_seconds);
        let now = self.clock.now();
        let visible_at = now.saturating_add(delay);
        let ceiling = self.limits.max_attempts;
        let tenant_id = tenant.as_str().to_string();
        let table_queue = queue.clone();

        let (released, dead, stale) = self
            .pool
            .with_tx(move |tx| {
                let mut released = 0usize;
                let mut dead: Vec<(String, i32)> = Vec::new();
                let mut stale: Vec<String> = Vec::new();
                for handle in &handles {
                    // One lookup answers both questions: is this message
                    // still queued, and is this consumer still the one
                    // holding it.
                    let row: Option<(i32, String)> = tx
                        .query_row(
                            "SELECT attempts, lease_token FROM messages
                              WHERE tenant = ?1 AND queue = ?2 AND id = ?3",
                            rusqlite::params![tenant_id, table_queue, handle.id],
                            |row| Ok((row.get(0)?, row.get(1)?)),
                        )
                        .optional()?;
                    // An id that is not there is skipped, not an error: it
                    // was already acknowledged, or it has been dead lettered.
                    let Some((attempts, token)) = row else {
                        continue;
                    };
                    if token != handle.lease_token {
                        stale.push(handle.id.clone());
                        continue;
                    }

                    if attempts >= ceiling {
                        move_to_dead_letter(
                            tx,
                            &tenant_id,
                            &table_queue,
                            &handle.id,
                            now,
                            DIED_CONSUMER_FAILED,
                        )?;
                        dead.push((handle.id.clone(), attempts));
                    } else {
                        // The token goes back to empty along with the
                        // visibility: the message is nobody's again, and the
                        // handle just used cannot be used a second time.
                        tx.execute(
                            "UPDATE messages SET visible_at = ?4, lease_token = ''
                              WHERE tenant = ?1 AND queue = ?2 AND id = ?3",
                            rusqlite::params![tenant_id, table_queue, handle.id, visible_at],
                        )?;
                        released += 1;
                    }
                }
                Ok((released, dead, stale))
            })
            .await?;

        for (id, attempts) in &dead {
            warn_dead_lettered(tenant, &queue, id, *attempts, ceiling, DIED_CONSUMER_FAILED);
        }
        for id in &stale {
            warn_stale_lease(tenant, &queue, id, "return");
        }
        tracing::debug!(
            tenant = %tenant,
            queue = %queue,
            released,
            dead_lettered = dead.len(),
            stale_leases = stale.len(),
            delay_seconds = delay,
            "returned messages"
        );
        Ok(NackOutcome {
            released,
            dead_lettered: dead.len(),
            stale_leases: stale.len(),
        })
    }

    /// Every dead letter of one tenant's queue, oldest death first.
    ///
    /// Unpaged, so only the tests use it; [`Self::list_dead_letters`] is what
    /// the RPC answers from and it is bounded in both count and bytes.
    #[cfg(test)]
    pub async fn dead_letters(
        &self,
        tenant: &TenantId,
        queue: &str,
    ) -> QueueResult<Vec<DeadLetter>> {
        let (page, _) = self
            .list_dead_letters(tenant, Some(queue), Some(MAX_DEAD_LETTER_PAGE), None)
            .await?;
        Ok(page)
    }

    /// One page of dead letters, oldest death first.
    ///
    /// `queue` of `None` reads every queue of this tenant, which is the view
    /// an operator starts from: the question is usually "what is stuck?", not
    /// "what is stuck in this one queue".
    ///
    /// Ordering by `(died_at, id)` is what makes the cursor stable. Dead
    /// letters only ever arrive at the *end* of that order, so a page
    /// boundary cannot shift under a reader the way it would if new rows
    /// could appear before it.
    pub async fn list_dead_letters(
        &self,
        tenant: &TenantId,
        queue: Option<&str>,
        limit: Option<i32>,
        cursor: Option<&str>,
    ) -> QueueResult<(Vec<DeadLetter>, Option<String>)> {
        let queue = queue.map(|q| self.check_queue(q)).transpose()?;
        let limit = clamp_dead_letter_page(limit);
        let after = cursor.map(parse_dead_letter_cursor).transpose()?;
        let tenant_id = tenant.as_str().to_string();
        let budget = self.limits.max_pull_response_bytes;

        let (page, stopped_at_budget) = self
            .pool
            .with_conn(move |conn| {
                // One statement whatever was asked for: a NULL parameter
                // switches its own filter off. Building the SQL by
                // concatenation instead would mean the placeholder numbers
                // and the bound values have to be kept in step by hand, and
                // getting that wrong is a query that reads the wrong rows
                // rather than one that fails to compile.
                //
                // `(died_at, id)` is compared as a pair, so a page never
                // repeats or skips a row when several died in the same
                // second.
                let mut stmt = conn.prepare(
                    "SELECT queue, id, body, attempts, created_at, died_at, reason
                       FROM dead_letter
                      WHERE tenant = ?1
                        AND (?3 IS NULL OR queue = ?3)
                        AND (?4 IS NULL
                             OR died_at > ?4
                             OR (died_at = ?4 AND id > ?5))
                      ORDER BY died_at ASC, id ASC
                      LIMIT ?2",
                )?;
                let (after_died_at, after_id) = match &after {
                    Some((died_at, id)) => (Some(*died_at), Some(id.as_str())),
                    None => (None, None),
                };
                let mut rows = stmt.query(rusqlite::params![
                    tenant_id,
                    limit,
                    queue,
                    after_died_at,
                    after_id
                ])?;

                let mut page = Vec::new();
                let mut used = 0usize;
                let mut stopped_at_budget = false;
                while let Some(row) = rows.next()? {
                    let body: Vec<u8> = row.get(2)?;
                    // The first message always fits, so a single body larger
                    // than the budget is still readable rather than making
                    // the whole page unanswerable.
                    if !page.is_empty() && used + body.len() > budget {
                        stopped_at_budget = true;
                        break;
                    }
                    used += body.len();
                    page.push(DeadLetter {
                        queue: row.get(0)?,
                        id: row.get(1)?,
                        body,
                        attempts: row.get(3)?,
                        created_at: row.get(4)?,
                        died_at: row.get(5)?,
                        reason: row.get(6)?,
                    });
                }
                Ok((page, stopped_at_budget))
            })
            .await?;

        // A page that was cut short may have more behind it, and *both* ways
        // of being cut short count. Offering a cursor only when the count
        // filled up would hide every dead letter behind a page the byte
        // budget ended early - which is exactly what a queue of large bodies
        // produces.
        let next_cursor = match page.last() {
            Some(last) if stopped_at_budget || page.len() as i32 == limit => {
                Some(format_dead_letter_cursor(last.died_at, &last.id))
            }
            _ => None,
        };
        tracing::debug!(
            tenant = %tenant,
            returned = page.len(),
            more = next_cursor.is_some(),
            "listed dead letters"
        );
        Ok((page, next_cursor))
    }

    /// Put named dead letters back on their queue with their attempts reset.
    ///
    /// The attempt count starts again from zero on purpose: a replay happens
    /// after the cause has been fixed, and a message that came back with its
    /// attempts already spent would be dead lettered again by its very first
    /// failure - or, with a ceiling of one, without being delivered at all.
    ///
    /// `created_at` is *not* reset. It says when the work was first asked
    /// for, and a replayed message is the same work, not new work.
    ///
    /// One transaction: either every named message is back on the queue or
    /// none of them is, so a replay that fails half way does not leave an
    /// operator guessing which half went.
    pub async fn replay_dead_letters(
        &self,
        tenant: &TenantId,
        queue: &str,
        ids: Vec<String>,
        delay_seconds: Option<i32>,
    ) -> QueueResult<ReplayOutcome> {
        let queue = self.check_queue(queue)?;
        if ids.is_empty() {
            tracing::warn!(
                tenant = %tenant,
                queue = %queue,
                "replay named no messages, refusing it; list the dead letters and name the ones \
                 to put back, because replaying a queue full of poison messages cannot be undone"
            );
            return Err(QueueError::InvalidArgument(
                "replay names no messages; list the dead letters and name the ones to put back"
                    .to_string(),
            ));
        }
        if ids.len() > self.limits.max_batch_messages {
            tracing::warn!(
                tenant = %tenant,
                queue = %queue,
                messages = ids.len(),
                limit = self.limits.max_batch_messages,
                "refusing an oversized replay; nothing was put back, because one transaction that \
                 large would hold the write lock against every producer"
            );
            return Err(QueueError::TooLarge(format!(
                "replay names {} messages, limit is {}",
                ids.len(),
                self.limits.max_batch_messages
            )));
        }
        let delay = self.clamp_delay(delay_seconds);
        let now = self.clock.now();
        let visible_at = now.saturating_add(delay);
        let tenant_id = tenant.as_str().to_string();
        let table_queue = queue.clone();
        let asked = ids.len();

        let replayed = self
            .pool
            .with_tx(move |tx| {
                let mut restore = tx.prepare(
                    "INSERT INTO messages
                            (tenant, queue, id, body, attempts, visible_at, created_at, lease_token)
                          SELECT tenant, queue, id, body, 0, ?4, created_at, ''
                            FROM dead_letter
                           WHERE tenant = ?1 AND queue = ?2 AND id = ?3
                     ON CONFLICT(tenant, queue, id) DO UPDATE SET
                            attempts    = 0,
                            visible_at  = excluded.visible_at,
                            lease_token = ''",
                )?;
                let mut discard = tx.prepare(
                    "DELETE FROM dead_letter WHERE tenant = ?1 AND queue = ?2 AND id = ?3",
                )?;
                let mut replayed = 0usize;
                for id in &ids {
                    let restored = restore.execute(rusqlite::params![
                        tenant_id,
                        table_queue,
                        id,
                        visible_at
                    ])?;
                    if restored == 0 {
                        continue;
                    }
                    discard.execute(rusqlite::params![tenant_id, table_queue, id])?;
                    replayed += 1;
                }
                Ok(replayed)
            })
            .await?;

        let not_found = asked - replayed;
        tracing::info!(
            tenant = %tenant,
            queue = %queue,
            replayed,
            not_found,
            delay_seconds = delay,
            "replayed dead letters; they are queued again with a fresh set of delivery attempts, \
             so whatever made them fail will be tried again"
        );
        if not_found > 0 {
            tracing::warn!(
                tenant = %tenant,
                queue = %queue,
                not_found,
                "some replayed ids were not in the dead letter table; they had already been \
                 replayed, or the retention sweep had deleted them, and those messages are not \
                 back on the queue"
            );
        }
        Ok(ReplayOutcome {
            replayed,
            not_found,
        })
    }

    /// Delete dead letters older than the configured retention, across all
    /// tenants. A retention of zero keeps them forever and this does nothing.
    /// Returns how many rows went away.
    pub async fn sweep_dead_letters(&self) -> QueueResult<usize> {
        let retention = self.limits.dead_letter_retention_secs;
        if retention <= 0 {
            return Ok(0);
        }
        let cutoff = self.clock.now().saturating_sub(retention);
        self.pool
            .with_conn(move |conn| {
                let removed =
                    conn.execute("DELETE FROM dead_letter WHERE died_at <= ?1", [cutoff])?;
                Ok(removed)
            })
            .await
    }

    /// Reject queue names that are empty, oversized, or carry characters that
    /// have no business in a name an operator will read back out of a log.
    fn check_queue(&self, queue: &str) -> QueueResult<String> {
        if queue.is_empty() {
            tracing::warn!("queue request with an empty queue name, refusing it");
            return Err(QueueError::InvalidArgument(
                "queue name is empty".to_string(),
            ));
        }
        if queue.len() > MAX_QUEUE_NAME_LEN {
            tracing::warn!(
                len = queue.len(),
                limit = MAX_QUEUE_NAME_LEN,
                "queue name exceeds the length limit, refusing the request"
            );
            return Err(QueueError::InvalidArgument(format!(
                "queue name is {} bytes, limit is {MAX_QUEUE_NAME_LEN}",
                queue.len()
            )));
        }
        if let Some(bad) = queue
            .chars()
            .find(|c| !(c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | ':')))
        {
            tracing::warn!(
                queue = %queue.escape_debug(),
                "queue name has a character outside the accepted set, refusing the request"
            );
            return Err(QueueError::InvalidArgument(format!(
                "queue name contains {bad:?}, only ASCII letters, digits, '_', '-', '.' and ':' \
                 are allowed"
            )));
        }
        Ok(queue.to_string())
    }

    fn check_body(&self, queue: &str, len: usize) -> QueueResult<()> {
        if len > self.limits.max_body_bytes {
            tracing::warn!(
                queue = %queue,
                bytes = len,
                limit = self.limits.max_body_bytes,
                "refusing an oversized message; it was not enqueued, and a payload this big \
                 belongs in blob storage with only its key sent through the queue"
            );
            return Err(QueueError::TooLarge(format!(
                "message body is {len} bytes, limit is {}",
                self.limits.max_body_bytes
            )));
        }
        Ok(())
    }

    /// Check the handles one Ack or Nack names.
    ///
    /// Two things. How many: the handles come from a pull, so a call naming
    /// more than a pull could ever return is a caller bug, and serving it
    /// would let one request hold the write lock for as long as it liked.
    /// And whether each one actually proves anything: a handle with no lease
    /// token is refused rather than quietly matched, because an unproved
    /// claim on a message is exactly what the token exists to stop. The whole
    /// call is refused rather than the one handle skipped - a caller sending
    /// tokenless handles has a bug, and half-applying its request would hide
    /// it.
    fn check_handles(&self, queue: &str, handles: &[MessageHandle]) -> QueueResult<()> {
        let limit = self.limits.max_pull_messages as usize;
        if handles.len() > limit {
            tracing::warn!(
                queue = %queue,
                messages = handles.len(),
                limit,
                "refusing a call that names more messages than one pull can return; none of them \
                 were acknowledged or returned"
            );
            return Err(QueueError::TooLarge(format!(
                "call names {} messages, limit is {limit}",
                handles.len()
            )));
        }
        for handle in handles {
            if handle.id.is_empty() {
                return Err(QueueError::InvalidArgument(
                    "a message handle carries no id".to_string(),
                ));
            }
            if handle.lease_token.is_empty() {
                tracing::warn!(
                    queue = %queue,
                    message_id = %handle.id,
                    "refusing a call whose handle carries no lease token; nothing was \
                     acknowledged or returned. The token comes back with every pulled message and \
                     is what proves the caller still holds the message it is answering for"
                );
                return Err(QueueError::InvalidArgument(format!(
                    "the handle for {} carries no lease token; pass back the lease_token the \
                     message was delivered with",
                    handle.id
                )));
            }
        }
        Ok(())
    }

    fn clamp_pull_size(&self, requested: Option<i32>) -> i32 {
        let default = self.limits.default_pull_messages.max(1);
        match requested {
            None => default,
            Some(n) if n <= 0 => default,
            Some(n) if n > self.limits.max_pull_messages => {
                tracing::debug!(
                    requested = n,
                    limit = self.limits.max_pull_messages,
                    "pull size capped; the caller must poll again for the rest"
                );
                self.limits.max_pull_messages.max(1)
            }
            Some(n) => n,
        }
    }

    fn clamp_lease(&self, requested: Option<i32>) -> i64 {
        let default = self.limits.default_lease_secs.max(1);
        match requested.map(i64::from) {
            None => default,
            Some(n) if n <= 0 => default,
            Some(n) if n > self.limits.max_lease_secs => {
                tracing::debug!(
                    requested = n,
                    limit = self.limits.max_lease_secs,
                    "lease capped; a handler that runs longer than this will see its message \
                     delivered a second time"
                );
                self.limits.max_lease_secs.max(1)
            }
            Some(n) => n,
        }
    }

    /// A redelivery delay. Unlike a lease, zero is meaningful here - it asks
    /// for the message back immediately - so only a negative delay is
    /// corrected.
    fn clamp_delay(&self, requested: Option<i32>) -> i64 {
        match requested.map(i64::from) {
            None => 0,
            Some(n) if n < 0 => {
                tracing::debug!(
                    requested = n,
                    "negative redelivery delay treated as no delay"
                );
                0
            }
            Some(n) if n > self.limits.max_lease_secs => {
                tracing::debug!(
                    requested = n,
                    limit = self.limits.max_lease_secs,
                    "redelivery delay capped; the message comes back sooner than asked"
                );
                self.limits.max_lease_secs
            }
            Some(n) => n,
        }
    }
}

/// Add the `lease_token` column to a `messages` table written before it
/// existed.
///
/// The rows are otherwise fine - a queue.sqlite from the tenant era holds
/// messages that still have to be delivered - so this widens the table rather
/// than refusing it. Existing rows get the empty token, which is what a
/// message that nobody holds carries anyway, so the first `Pull` issues a
/// real one. `CREATE TABLE IF NOT EXISTS` above cannot do this: it does
/// nothing at all when the table is already there.
fn add_lease_token_column(conn: &rusqlite::Connection) -> QueueResult<()> {
    let mut present = false;
    conn.pragma(None, "table_info", "messages", |row| {
        let column: String = row.get(1)?;
        if column == "lease_token" {
            present = true;
        }
        Ok(())
    })?;
    if present {
        return Ok(());
    }
    conn.execute_batch("ALTER TABLE messages ADD COLUMN lease_token TEXT NOT NULL DEFAULT ''")?;
    tracing::info!(
        "added the lease_token column to an existing queue database; messages already in it keep \
         their place in line and are given a token by their next delivery"
    );
    Ok(())
}

/// Add the `reason` column to a `dead_letter` table written before it
/// existed.
///
/// Same shape as [`add_lease_token_column`] and for the same reason: the rows
/// are fine, they simply predate the column, and `CREATE TABLE IF NOT EXISTS`
/// does nothing at all to a table that is already there. Existing rows keep
/// the empty string, which is the one value a reader renders as "unknown" -
/// inventing a reason for a message this service never recorded one for would
/// point an operator at the wrong half of the system.
fn add_dead_letter_reason_column(conn: &rusqlite::Connection) -> QueueResult<()> {
    let mut present = false;
    conn.pragma(None, "table_info", "dead_letter", |row| {
        let column: String = row.get(1)?;
        if column == "reason" {
            present = true;
        }
        Ok(())
    })?;
    if present {
        return Ok(());
    }
    conn.execute_batch("ALTER TABLE dead_letter ADD COLUMN reason TEXT NOT NULL DEFAULT ''")?;
    tracing::info!(
        "added the reason column to an existing queue database; dead letters already in it are \
         listed with no reason, because this service never recorded one for them"
    );
    Ok(())
}

/// Move every ready message that has already used its attempts into the dead
/// letter table. Returns the ids and attempt counts, for the caller to log.
fn retire_exhausted(
    tx: &Transaction<'_>,
    tenant: &str,
    queue: &str,
    now: i64,
    ceiling: i32,
) -> QueueResult<Vec<(String, i32)>> {
    let mut select = tx.prepare(
        "SELECT id, attempts FROM messages
          WHERE tenant = ?1 AND queue = ?2 AND visible_at <= ?3 AND attempts >= ?4",
    )?;
    let mut rows = select.query(rusqlite::params![tenant, queue, now, ceiling])?;
    let mut exhausted = Vec::new();
    while let Some(row) = rows.next()? {
        exhausted.push((row.get::<_, String>(0)?, row.get::<_, i32>(1)?));
    }
    drop(rows);
    drop(select);

    for (id, _) in &exhausted {
        move_to_dead_letter(tx, tenant, queue, id, now, DIED_LEASE_LAPSED)?;
    }
    Ok(exhausted)
}

/// Copy one message into the dead letter table and remove it from the queue.
/// The caller is inside the transaction, so both halves land together.
///
/// `reason` is one of the two constants at the top of this module and is
/// written on the row rather than only into the log: the log line an operator
/// would otherwise have to find has usually gone by the time anybody reads
/// the table.
fn move_to_dead_letter(
    tx: &Transaction<'_>,
    tenant: &str,
    queue: &str,
    id: &str,
    now: i64,
    reason: &str,
) -> QueueResult<()> {
    tx.execute(
        "INSERT INTO dead_letter (tenant, queue, id, body, attempts, created_at, died_at, reason)
              SELECT tenant, queue, id, body, attempts, created_at, ?4, ?5
                FROM messages
               WHERE tenant = ?1 AND queue = ?2 AND id = ?3
         ON CONFLICT(tenant, queue, id) DO UPDATE SET
              attempts = excluded.attempts,
              died_at  = excluded.died_at,
              reason   = excluded.reason",
        rusqlite::params![tenant, queue, id, now, reason],
    )?;
    tx.execute(
        "DELETE FROM messages WHERE tenant = ?1 AND queue = ?2 AND id = ?3",
        rusqlite::params![tenant, queue, id],
    )?;
    Ok(())
}

/// Bound one dead letter page.
fn clamp_dead_letter_page(requested: Option<i32>) -> i32 {
    match requested {
        None => DEFAULT_DEAD_LETTER_PAGE,
        Some(n) if n <= 0 => DEFAULT_DEAD_LETTER_PAGE,
        Some(n) if n > MAX_DEAD_LETTER_PAGE => MAX_DEAD_LETTER_PAGE,
        Some(n) => n,
    }
}

/// The cursor is the `(died_at, id)` of the last row of a page: the exact
/// pair the ordering is on, so resuming after it can neither repeat nor skip
/// a row.
fn format_dead_letter_cursor(died_at: i64, id: &str) -> String {
    format!("{died_at}:{id}")
}

/// Read a cursor back.
///
/// A malformed one is refused rather than ignored. Silently restarting at the
/// beginning would hand a paging operator the first page over and over, which
/// looks like a queue that never empties.
fn parse_dead_letter_cursor(cursor: &str) -> QueueResult<(i64, String)> {
    let refuse = || {
        QueueError::InvalidArgument(format!(
            "{cursor:?} is not a dead letter cursor; pass back the next_cursor from the previous \
             page, or nothing to start at the beginning"
        ))
    };
    let (died_at, id) = cursor.split_once(':').ok_or_else(refuse)?;
    let died_at: i64 = died_at.parse().map_err(|_| refuse())?;
    if id.is_empty() {
        return Err(refuse());
    }
    Ok((died_at, id.to_string()))
}

/// One line per message a consumer tried to answer for after losing it.
///
/// Warn, not error: nothing is broken and nothing is lost - the queue did
/// exactly what it should. It is worth an operator's attention because of
/// what it says about timing: the handler outlived its lease, so either the
/// message is being processed twice right now (somebody else took it) or it
/// is queued to be processed again (it was given back). Either way the lease
/// is shorter than the handler actually takes.
fn warn_stale_lease(tenant: &TenantId, queue: &str, id: &str, action: &str) {
    tracing::warn!(
        tenant = %tenant,
        queue = %queue,
        message_id = %id,
        "a consumer tried to {action} a message it no longer holds - its lease lapsed and the \
         message has since been handed to another consumer or put back on the queue. The request \
         was ignored, so whoever holds it now keeps it, and the work this consumer just finished \
         will be done again. Raise the lease above the handler's worst-case run time"
    );
}

/// One line per dead lettered message, saying what stops now.
fn warn_dead_lettered(
    tenant: &TenantId,
    queue: &str,
    id: &str,
    attempts: i32,
    ceiling: i32,
    why: &str,
) {
    tracing::warn!(
        tenant = %tenant,
        queue = %queue,
        message_id = %id,
        attempts,
        max_attempts = ceiling,
        "message used up its delivery attempts ({why}) and was moved to the dead letter table; it \
         will not be delivered again, and whatever it was meant to do has not been done - an \
         operator has to read it out of the database and decide"
    );
}

#[tonic::async_trait]
impl Queue for QueueService {
    async fn send(&self, request: Request<SendRequest>) -> Result<Response<SendResponse>, Status> {
        let tenant = tenant_of(&request)?;
        let req = request.into_inner();
        let id = self
            .send(&tenant, &req.queue_name, req.message_body)
            .await?;
        Ok(Response::new(SendResponse {
            success: true,
            message_id: id,
            error: String::new(),
        }))
    }

    async fn send_batch(
        &self,
        request: Request<SendBatchRequest>,
    ) -> Result<Response<SendBatchResponse>, Status> {
        let tenant = tenant_of(&request)?;
        let req = request.into_inner();
        let ids = self
            .send_batch(&tenant, &req.queue_name, req.messages)
            .await?;
        Ok(Response::new(SendBatchResponse {
            success: true,
            message_ids: ids,
            error: String::new(),
        }))
    }

    async fn pull(&self, request: Request<PullRequest>) -> Result<Response<PullResponse>, Status> {
        let tenant = tenant_of(&request)?;
        let req = request.into_inner();
        let leased = self
            .pull(
                &tenant,
                &req.queue_name,
                Some(req.max_messages),
                Some(req.lease_seconds),
            )
            .await?;

        // Every leased message is handed back, whatever its bytes are.
        // `QueueMessage.body` was a string until the queue learned to carry
        // bytes, and a body that was not UTF-8 was withheld from the pull and
        // left to climb its attempt count until the ceiling dead lettered a
        // message nothing was ever wrong with.
        let messages = leased
            .into_iter()
            .map(|message| QueueMessage {
                id: message.id,
                body: message.body,
                attempts: message.attempts,
                lease_expires_at: message.lease_expires_at,
                created_at: message.created_at,
                lease_token: message.lease_token,
            })
            .collect();
        Ok(Response::new(PullResponse { messages }))
    }

    async fn ack(&self, request: Request<AckRequest>) -> Result<Response<AckResponse>, Status> {
        let tenant = tenant_of(&request)?;
        let req = request.into_inner();
        let outcome = self
            .ack(&tenant, &req.queue_name, handles_of(req.messages))
            .await?;
        Ok(Response::new(AckResponse {
            acknowledged: outcome.acknowledged as i32,
            stale_leases: outcome.stale_leases as i32,
        }))
    }

    async fn nack(&self, request: Request<NackRequest>) -> Result<Response<NackResponse>, Status> {
        let tenant = tenant_of(&request)?;
        let req = request.into_inner();
        let outcome = self
            .nack(
                &tenant,
                &req.queue_name,
                handles_of(req.messages),
                Some(req.delay_seconds),
            )
            .await?;
        Ok(Response::new(NackResponse {
            released: outcome.released as i32,
            dead_lettered: outcome.dead_lettered as i32,
            stale_leases: outcome.stale_leases as i32,
        }))
    }

    async fn list_dead_letters(
        &self,
        request: Request<ListDeadLettersRequest>,
    ) -> Result<Response<ListDeadLettersResponse>, Status> {
        let tenant = tenant_of(&request)?;
        let req = request.into_inner();
        // An empty queue name means every queue: proto3 cannot tell "not set"
        // from "empty string" for a plain field, and an empty queue name is
        // refused everywhere else anyway, so there is no ambiguity to lose.
        let queue = Some(req.queue_name.as_str()).filter(|q| !q.is_empty());
        let cursor = Some(req.cursor.as_str()).filter(|c| !c.is_empty());
        let (page, next_cursor) = self
            .list_dead_letters(&tenant, queue, Some(req.limit), cursor)
            .await?;
        Ok(Response::new(ListDeadLettersResponse {
            messages: page.into_iter().map(DeadLetterMessage::from).collect(),
            next_cursor: next_cursor.unwrap_or_default(),
        }))
    }

    async fn replay_dead_letters(
        &self,
        request: Request<ReplayDeadLettersRequest>,
    ) -> Result<Response<ReplayDeadLettersResponse>, Status> {
        let tenant = tenant_of(&request)?;
        let req = request.into_inner();
        let outcome = self
            .replay_dead_letters(
                &tenant,
                &req.queue_name,
                req.message_ids,
                Some(req.delay_seconds),
            )
            .await?;
        Ok(Response::new(ReplayDeadLettersResponse {
            replayed: outcome.replayed as i32,
            not_found: outcome.not_found as i32,
        }))
    }
}

/// Turn the wire handles into the storage layer's. Kept separate so the
/// storage layer never sees a protobuf type.
fn handles_of(messages: Vec<crate::proto::queue::MessageHandle>) -> Vec<MessageHandle> {
    messages
        .into_iter()
        .map(|handle| MessageHandle {
            id: handle.id,
            lease_token: handle.lease_token,
        })
        .collect()
}

impl From<DeadLetter> for DeadLetterMessage {
    fn from(dead: DeadLetter) -> Self {
        Self {
            queue_name: dead.queue,
            id: dead.id,
            // Bytes rather than a string: a body that is not valid UTF-8 is
            // exactly the kind of message that ends up dead lettered, and an
            // operator has to be able to see it.
            body: dead.body,
            attempts: dead.attempts,
            created_at: dead.created_at,
            died_at: dead.died_at,
            // Which of the two ways it died. A caller that shows an operator
            // the table has to be able to say this: "the handler refused it"
            // and "nobody ever answered" are the same row otherwise, and the
            // fix for one is not the fix for the other.
            reason: dead.reason,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tenant::TENANT_HEADER;
    use tempfile::TempDir;

    /// A fixed instant well past 1970, so a lease never lands on a negative
    /// second and the numbers in a failure message are readable.
    const T0: i64 = 1_700_000_000;

    struct Fixture {
        _dir: TempDir,
        service: QueueService,
        clock: Clock,
    }

    async fn fixture_with(limits: QueueLimits) -> Fixture {
        let dir = TempDir::new().unwrap();
        let clock = Clock::fixed(T0);
        let service = QueueService::open_with_clock(dir.path(), 4, limits, clock.clone())
            .await
            .unwrap();
        Fixture {
            _dir: dir,
            service,
            clock,
        }
    }

    async fn fixture() -> Fixture {
        fixture_with(QueueLimits::default()).await
    }

    fn tenant(name: &str) -> TenantId {
        TenantId::parse(name).unwrap()
    }

    fn bodies(messages: &[LeasedMessage]) -> Vec<String> {
        messages
            .iter()
            .map(|m| String::from_utf8(m.body.clone()).unwrap())
            .collect()
    }

    /// What a consumer answers with: the id it was handed and the token
    /// proving this is the delivery it is answering for.
    fn handle(message: &LeasedMessage) -> MessageHandle {
        MessageHandle {
            id: message.id.clone(),
            lease_token: message.lease_token.clone(),
        }
    }

    /// The handles for a whole leased batch.
    fn handles(messages: &[LeasedMessage]) -> Vec<MessageHandle> {
        messages.iter().map(handle).collect()
    }

    /// A handle for a message this caller was never given: the shape of a
    /// consumer that kept an id but not the lease.
    fn forged(id: &str) -> MessageHandle {
        MessageHandle {
            id: id.to_string(),
            lease_token: "not-the-current-lease".to_string(),
        }
    }

    /// Stamp the tenant the way deis-runtime does, for the tests that go
    /// through the gRPC surface rather than the queue methods.
    fn as_tenant<T>(tenant: &str, message: T) -> Request<T> {
        let mut request = Request::new(message);
        request
            .metadata_mut()
            .insert(TENANT_HEADER, tenant.parse().unwrap());
        request
    }

    #[tokio::test]
    async fn a_sent_message_is_pulled_then_acknowledged_and_never_seen_again() {
        let f = fixture().await;
        let a = tenant("app-one.production");
        let id = f.service.send(&a, "jobs", b"work".to_vec()).await.unwrap();

        let leased = f.service.pull(&a, "jobs", None, None).await.unwrap();
        assert_eq!(leased.len(), 1);
        assert_eq!(leased[0].id, id);
        assert_eq!(leased[0].body, b"work");
        assert_eq!(leased[0].attempts, 1, "the first delivery counts as one");
        assert_eq!(leased[0].created_at, T0);
        assert_eq!(leased[0].lease_expires_at, T0 + 30);
        assert!(
            !leased[0].lease_token.is_empty(),
            "every delivery has to carry the proof Ack will ask for"
        );

        assert_eq!(
            f.service.ack(&a, "jobs", handles(&leased)).await.unwrap(),
            AckOutcome {
                acknowledged: 1,
                stale_leases: 0
            }
        );
        // Even far past the lease, an acknowledged message is gone.
        f.clock.advance(10_000);
        assert!(
            f.service
                .pull(&a, "jobs", None, None)
                .await
                .unwrap()
                .is_empty()
        );
        assert!(f.service.dead_letters(&a, "jobs").await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_leased_message_is_invisible_until_its_lease_lapses() {
        let f = fixture().await;
        let a = tenant("app-one.production");
        f.service.send(&a, "jobs", b"work".to_vec()).await.unwrap();

        let first = f.service.pull(&a, "jobs", None, Some(60)).await.unwrap();
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].lease_expires_at, T0 + 60);

        // Another consumer polling inside the lease sees nothing: this is what
        // stops two nodes running the same job.
        f.clock.advance(59);
        assert!(
            f.service
                .pull(&a, "jobs", None, None)
                .await
                .unwrap()
                .is_empty()
        );

        // One second later the lease has lapsed and the message comes back,
        // with the earlier attempt still counted.
        f.clock.advance(1);
        let again = f.service.pull(&a, "jobs", None, None).await.unwrap();
        assert_eq!(again.len(), 1);
        assert_eq!(again[0].id, first[0].id);
        assert_eq!(again[0].attempts, 2, "the redelivery is a second attempt");
    }

    #[tokio::test]
    async fn a_nack_with_a_delay_holds_the_message_back_for_exactly_that_long() {
        let f = fixture().await;
        let a = tenant("app-one.production");
        let id = f.service.send(&a, "jobs", b"work".to_vec()).await.unwrap();
        let leased = f.service.pull(&a, "jobs", None, None).await.unwrap();

        let outcome = f
            .service
            .nack(&a, "jobs", handles(&leased), Some(45))
            .await
            .unwrap();
        assert_eq!(
            outcome,
            NackOutcome {
                released: 1,
                dead_lettered: 0,
                stale_leases: 0
            }
        );

        f.clock.advance(44);
        assert!(
            f.service
                .pull(&a, "jobs", None, None)
                .await
                .unwrap()
                .is_empty()
        );
        f.clock.advance(1);
        let back = f.service.pull(&a, "jobs", None, None).await.unwrap();
        assert_eq!(back.len(), 1);
        assert_eq!(back[0].id, id);
        assert_eq!(back[0].attempts, 2);
    }

    #[tokio::test]
    async fn a_nack_without_a_delay_makes_the_message_ready_at_once() {
        let f = fixture().await;
        let a = tenant("app-one.production");
        f.service.send(&a, "jobs", b"work".to_vec()).await.unwrap();
        let leased = f.service.pull(&a, "jobs", None, None).await.unwrap();
        f.service
            .nack(&a, "jobs", handles(&leased), None)
            .await
            .unwrap();
        assert_eq!(
            f.service.pull(&a, "jobs", None, None).await.unwrap().len(),
            1
        );
    }

    #[tokio::test]
    async fn a_message_that_keeps_failing_is_dead_lettered_at_the_ceiling() {
        let f = fixture_with(QueueLimits {
            max_attempts: 3,
            ..QueueLimits::default()
        })
        .await;
        let a = tenant("app-one.production");
        let id = f
            .service
            .send(&a, "jobs", b"poison".to_vec())
            .await
            .unwrap();

        for attempt in 1..=3 {
            let leased = f.service.pull(&a, "jobs", None, None).await.unwrap();
            assert_eq!(leased.len(), 1, "attempt {attempt} should be delivered");
            assert_eq!(leased[0].attempts, attempt);
            let outcome = f
                .service
                .nack(&a, "jobs", handles(&leased), None)
                .await
                .unwrap();
            if attempt < 3 {
                assert_eq!(outcome.released, 1, "attempt {attempt} should be retried");
            } else {
                assert_eq!(
                    outcome,
                    NackOutcome {
                        released: 0,
                        dead_lettered: 1,
                        stale_leases: 0
                    },
                    "the last attempt must retire the message"
                );
            }
        }

        // Delivered exactly max_attempts times, then never again.
        assert!(
            f.service
                .pull(&a, "jobs", None, None)
                .await
                .unwrap()
                .is_empty()
        );
        let dead = f.service.dead_letters(&a, "jobs").await.unwrap();
        assert_eq!(dead.len(), 1);
        assert_eq!(dead[0].id, id);
        assert_eq!(dead[0].attempts, 3);
        assert_eq!(dead[0].body, b"poison");
        assert_eq!(dead[0].created_at, T0);
        assert_eq!(dead[0].died_at, T0);
        // Which of the two ways it died, kept on the row. The handler ran and
        // refused this body every time, so what an operator reads next is the
        // body - not the lease, which was never the problem here.
        assert_eq!(dead[0].reason, DIED_CONSUMER_FAILED);
    }

    /// The consumer never reports anything - it crashes every time. The
    /// ceiling has to hold on lease expiry alone, otherwise a node that dies
    /// mid-handler redelivers its message forever.
    #[tokio::test]
    async fn a_consumer_that_only_ever_crashes_still_exhausts_the_ceiling() {
        let f = fixture_with(QueueLimits {
            max_attempts: 2,
            ..QueueLimits::default()
        })
        .await;
        let a = tenant("app-one.production");
        let id = f.service.send(&a, "jobs", b"work".to_vec()).await.unwrap();

        for attempt in 1..=2 {
            let leased = f.service.pull(&a, "jobs", None, Some(10)).await.unwrap();
            assert_eq!(leased.len(), 1, "attempt {attempt} should be delivered");
            assert_eq!(leased[0].attempts, attempt);
            f.clock.advance(10);
        }

        // The lease has lapsed for the last time; the next pull retires it
        // instead of handing it out.
        assert!(
            f.service
                .pull(&a, "jobs", None, None)
                .await
                .unwrap()
                .is_empty()
        );
        let dead = f.service.dead_letters(&a, "jobs").await.unwrap();
        assert_eq!(dead.len(), 1);
        assert_eq!(dead[0].id, id);
        assert_eq!(dead[0].attempts, 2);
        // The other reason, and the whole point of recording it: nobody ever
        // answered for this message. Attempts alone read identically to the
        // test above, where the handler did answer and refused the body, and
        // the two send an operator to opposite halves of the system.
        assert_eq!(dead[0].reason, DIED_LEASE_LAPSED);
        assert_ne!(dead[0].reason, DIED_CONSUMER_FAILED);
    }

    /// The failure the lease token exists for.
    ///
    /// A consumer takes a message, runs long, and its lease lapses. Another
    /// consumer picks the message up and starts the same work. Then the first
    /// one finishes and acknowledges. Identified by id alone, that
    /// acknowledgement deletes the message the second consumer is holding:
    /// the work is thrown away half-done, and nothing anywhere says so.
    #[tokio::test]
    async fn a_lapsed_consumer_cannot_acknowledge_a_message_now_held_by_another() {
        let f = fixture().await;
        let a = tenant("app-one.production");
        let id = f.service.send(&a, "jobs", b"work".to_vec()).await.unwrap();

        let slow = f.service.pull(&a, "jobs", None, Some(30)).await.unwrap();
        assert_eq!(slow.len(), 1);

        // The lease lapses and somebody else takes the message.
        f.clock.advance(31);
        let holder = f.service.pull(&a, "jobs", None, Some(30)).await.unwrap();
        assert_eq!(holder.len(), 1);
        assert_eq!(holder[0].id, id);
        assert_ne!(
            holder[0].lease_token, slow[0].lease_token,
            "a redelivery has to issue a new token, or the old holder still proves nothing"
        );

        // The slow consumer finishes and acknowledges what it was given.
        let outcome = f.service.ack(&a, "jobs", handles(&slow)).await.unwrap();
        assert_eq!(
            outcome,
            AckOutcome {
                acknowledged: 0,
                stale_leases: 1
            },
            "a lapsed consumer must not delete a message somebody else is working on"
        );

        // The current holder still has it, and can still finish it.
        assert_eq!(
            f.service.ack(&a, "jobs", handles(&holder)).await.unwrap(),
            AckOutcome {
                acknowledged: 1,
                stale_leases: 0
            }
        );
    }

    /// The same fence on the other call: a lapsed consumer must not push back
    /// the visibility of a message that is being worked on right now, nor
    /// dead letter it.
    #[tokio::test]
    async fn a_lapsed_consumer_cannot_return_a_message_now_held_by_another() {
        let f = fixture().await;
        let a = tenant("app-one.production");
        f.service.send(&a, "jobs", b"work".to_vec()).await.unwrap();

        let slow = f.service.pull(&a, "jobs", None, Some(30)).await.unwrap();
        f.clock.advance(31);
        let holder = f.service.pull(&a, "jobs", None, Some(30)).await.unwrap();

        let outcome = f
            .service
            .nack(&a, "jobs", handles(&slow), Some(600))
            .await
            .unwrap();
        assert_eq!(
            outcome,
            NackOutcome {
                released: 0,
                dead_lettered: 0,
                stale_leases: 1
            }
        );
        // The lease the current holder was given is untouched: the message is
        // still invisible to everyone else, and still theirs to acknowledge.
        assert!(
            f.service
                .pull(&a, "jobs", None, None)
                .await
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            f.service.ack(&a, "jobs", handles(&holder)).await.unwrap(),
            AckOutcome {
                acknowledged: 1,
                stale_leases: 0
            }
        );
    }

    /// A lapsed lease that nobody else took is not a fence: the consumer did
    /// the work, and refusing its acknowledgement would only cause the
    /// message to be handled a second time for nothing.
    #[tokio::test]
    async fn a_late_acknowledgement_still_counts_when_nobody_else_took_the_message() {
        let f = fixture().await;
        let a = tenant("app-one.production");
        f.service.send(&a, "jobs", b"work".to_vec()).await.unwrap();
        let leased = f.service.pull(&a, "jobs", None, Some(30)).await.unwrap();

        f.clock.advance(120);
        assert_eq!(
            f.service.ack(&a, "jobs", handles(&leased)).await.unwrap(),
            AckOutcome {
                acknowledged: 1,
                stale_leases: 0
            }
        );
        assert!(
            f.service
                .pull(&a, "jobs", None, None)
                .await
                .unwrap()
                .is_empty()
        );
    }

    /// A returned message belongs to nobody, so the handle that returned it
    /// must not work a second time either.
    #[tokio::test]
    async fn a_handle_is_spent_once_the_message_has_been_returned() {
        let f = fixture().await;
        let a = tenant("app-one.production");
        f.service.send(&a, "jobs", b"work".to_vec()).await.unwrap();
        let leased = f.service.pull(&a, "jobs", None, None).await.unwrap();
        f.service
            .nack(&a, "jobs", handles(&leased), None)
            .await
            .unwrap();

        assert_eq!(
            f.service.ack(&a, "jobs", handles(&leased)).await.unwrap(),
            AckOutcome {
                acknowledged: 0,
                stale_leases: 1
            },
            "the handle used to return the message must not then delete it"
        );
        assert_eq!(
            f.service.pull(&a, "jobs", None, None).await.unwrap().len(),
            1,
            "the message must still be there to be delivered again"
        );
    }

    /// A handle with no token proves nothing, so it is refused rather than
    /// quietly matched - and the whole call is refused, because a caller
    /// sending one has a bug that half-applying would hide.
    #[tokio::test]
    async fn a_handle_without_a_lease_token_is_refused() {
        let f = fixture().await;
        let a = tenant("app-one.production");
        f.service.send(&a, "jobs", b"work".to_vec()).await.unwrap();
        let leased = f.service.pull(&a, "jobs", None, None).await.unwrap();

        let tokenless = MessageHandle {
            id: leased[0].id.clone(),
            lease_token: String::new(),
        };
        assert!(matches!(
            f.service.ack(&a, "jobs", vec![tokenless.clone()]).await,
            Err(QueueError::InvalidArgument(_))
        ));
        assert!(matches!(
            f.service.nack(&a, "jobs", vec![tokenless], None).await,
            Err(QueueError::InvalidArgument(_))
        ));
        // Nothing was acted on, so the message is still leased to whoever
        // holds it.
        assert_eq!(
            f.service.ack(&a, "jobs", handles(&leased)).await.unwrap(),
            AckOutcome {
                acknowledged: 1,
                stale_leases: 0
            }
        );
    }

    #[tokio::test]
    async fn dead_letters_can_be_listed_and_replayed_back_onto_their_queue() {
        let f = fixture_with(QueueLimits {
            max_attempts: 1,
            ..QueueLimits::default()
        })
        .await;
        let a = tenant("app-one.production");
        let id = f
            .service
            .send(&a, "jobs", b"needs a fix".to_vec())
            .await
            .unwrap();
        let leased = f.service.pull(&a, "jobs", None, None).await.unwrap();
        f.service
            .nack(&a, "jobs", handles(&leased), None)
            .await
            .unwrap();

        let (page, cursor) = f
            .service
            .list_dead_letters(&a, Some("jobs"), None, None)
            .await
            .unwrap();
        assert_eq!(page.len(), 1);
        assert_eq!(page[0].id, id);
        assert_eq!(page[0].queue, "jobs");
        assert_eq!(page[0].body, b"needs a fix");
        assert_eq!(page[0].attempts, 1);
        assert_eq!(page[0].created_at, T0);
        assert!(cursor.is_none(), "one page held everything");

        // The cause is fixed; put it back.
        f.clock.advance(60);
        let outcome = f
            .service
            .replay_dead_letters(&a, "jobs", vec![id.clone()], None)
            .await
            .unwrap();
        assert_eq!(
            outcome,
            ReplayOutcome {
                replayed: 1,
                not_found: 0
            }
        );
        assert!(
            f.service.dead_letters(&a, "jobs").await.unwrap().is_empty(),
            "a replayed message must leave the dead letter table"
        );

        let back = f.service.pull(&a, "jobs", None, None).await.unwrap();
        assert_eq!(back.len(), 1);
        assert_eq!(back[0].id, id);
        assert_eq!(
            back[0].attempts, 1,
            "a replay hands the message a fresh allowance, or the very first failure retires it \
             again"
        );
        assert_eq!(
            back[0].created_at, T0,
            "a replayed message is the same work, so it keeps the time it was first asked for"
        );
    }

    #[tokio::test]
    async fn a_replay_delay_holds_the_message_back() {
        let f = fixture_with(QueueLimits {
            max_attempts: 1,
            ..QueueLimits::default()
        })
        .await;
        let a = tenant("app-one.production");
        let id = f.service.send(&a, "jobs", b"work".to_vec()).await.unwrap();
        let leased = f.service.pull(&a, "jobs", None, None).await.unwrap();
        f.service
            .nack(&a, "jobs", handles(&leased), None)
            .await
            .unwrap();

        f.service
            .replay_dead_letters(&a, "jobs", vec![id], Some(45))
            .await
            .unwrap();
        assert!(
            f.service
                .pull(&a, "jobs", None, None)
                .await
                .unwrap()
                .is_empty()
        );
        f.clock.advance(45);
        assert_eq!(
            f.service.pull(&a, "jobs", None, None).await.unwrap().len(),
            1
        );
    }

    #[tokio::test]
    async fn a_replay_of_something_that_is_not_dead_lettered_changes_nothing() {
        let f = fixture().await;
        let a = tenant("app-one.production");
        let live = f.service.send(&a, "jobs", b"live".to_vec()).await.unwrap();

        let outcome = f
            .service
            .replay_dead_letters(&a, "jobs", vec![live.clone(), "never".to_string()], None)
            .await
            .unwrap();
        assert_eq!(
            outcome,
            ReplayOutcome {
                replayed: 0,
                not_found: 2
            },
            "a live message is not a dead letter, and must not be touched by a replay"
        );
        let leased = f.service.pull(&a, "jobs", None, None).await.unwrap();
        assert_eq!(leased.len(), 1);
        assert_eq!(leased[0].id, live);
        assert_eq!(leased[0].attempts, 1);
    }

    /// Replaying everything by accident cannot be undone, so the call names
    /// what it means.
    #[tokio::test]
    async fn a_replay_that_names_nothing_is_refused() {
        let f = fixture().await;
        let a = tenant("app-one.production");
        assert!(matches!(
            f.service
                .replay_dead_letters(&a, "jobs", Vec::new(), None)
                .await,
            Err(QueueError::InvalidArgument(_))
        ));
    }

    #[tokio::test]
    async fn dead_letters_page_and_can_be_read_across_every_queue_at_once() {
        let f = fixture_with(QueueLimits {
            max_attempts: 1,
            ..QueueLimits::default()
        })
        .await;
        let a = tenant("app-one.production");
        let other = tenant("app-two.production");

        // Four in one queue and one in another, each dying a second apart so
        // the order is unambiguous.
        let mut expected = Vec::new();
        for (queue, body) in [
            ("jobs", "one"),
            ("jobs", "two"),
            ("emails", "three"),
            ("jobs", "four"),
        ] {
            let id = f
                .service
                .send(&a, queue, body.as_bytes().to_vec())
                .await
                .unwrap();
            let leased = f.service.pull(&a, queue, None, None).await.unwrap();
            f.service
                .nack(&a, queue, handles(&leased), None)
                .await
                .unwrap();
            expected.push(id);
            f.clock.advance(1);
        }
        // Another tenant's dead letter must never appear in this listing.
        f.service
            .send(&other, "jobs", b"theirs".to_vec())
            .await
            .unwrap();
        let theirs = f.service.pull(&other, "jobs", None, None).await.unwrap();
        f.service
            .nack(&other, "jobs", handles(&theirs), None)
            .await
            .unwrap();

        // Every queue of this tenant, two at a time.
        let (first, cursor) = f
            .service
            .list_dead_letters(&a, None, Some(2), None)
            .await
            .unwrap();
        assert_eq!(first.len(), 2);
        let cursor = cursor.expect("a full page must offer a way to continue");
        let (second, cursor) = f
            .service
            .list_dead_letters(&a, None, Some(2), Some(&cursor))
            .await
            .unwrap();
        assert_eq!(second.len(), 2);
        let (third, last) = f
            .service
            .list_dead_letters(&a, None, Some(2), cursor.as_deref())
            .await
            .unwrap();
        assert!(third.is_empty());
        assert!(last.is_none());

        let seen: Vec<String> = first
            .iter()
            .chain(second.iter())
            .map(|dead| dead.id.clone())
            .collect();
        assert_eq!(seen, expected, "oldest death first, across queues");

        // Narrowed to one queue, the other queue's message is not there.
        let (jobs, _) = f
            .service
            .list_dead_letters(&a, Some("jobs"), None, None)
            .await
            .unwrap();
        assert_eq!(jobs.len(), 3);
        assert!(jobs.iter().all(|dead| dead.queue == "jobs"));
    }

    /// A page cut short by the response size budget still has to say there is
    /// more. Without a cursor there, every dead letter behind the first big
    /// body would be unreachable through the API.
    #[tokio::test]
    async fn a_page_the_byte_budget_cut_short_still_offers_a_cursor() {
        let f = fixture_with(QueueLimits {
            max_attempts: 1,
            max_body_bytes: 512,
            // Room for one body and not two, so the second message is always
            // pushed to the next page.
            max_pull_response_bytes: 600,
            ..QueueLimits::default()
        })
        .await;
        let a = tenant("app-one.production");

        for _ in 0..3 {
            f.service.send(&a, "jobs", vec![b'x'; 512]).await.unwrap();
            let leased = f.service.pull(&a, "jobs", Some(1), None).await.unwrap();
            f.service
                .nack(&a, "jobs", handles(&leased), None)
                .await
                .unwrap();
            f.clock.advance(1);
        }

        // Asked for fifty, given one - and told there is more.
        let mut seen = 0;
        let mut cursor = None;
        for _ in 0..4 {
            let (page, next) = f
                .service
                .list_dead_letters(&a, Some("jobs"), None, cursor.as_deref())
                .await
                .unwrap();
            seen += page.len();
            match next {
                Some(next) => cursor = Some(next),
                None => break,
            }
        }
        assert_eq!(
            seen, 3,
            "every dead letter must be reachable by paging, not only the ones that fit in one \
             response"
        );
    }

    #[tokio::test]
    async fn a_malformed_dead_letter_cursor_is_refused_rather_than_restarting_the_listing() {
        let f = fixture().await;
        let a = tenant("app-one.production");
        for bad in ["nonsense", "12345", ":", "abc:def"] {
            assert!(
                matches!(
                    f.service.list_dead_letters(&a, None, None, Some(bad)).await,
                    Err(QueueError::InvalidArgument(_))
                ),
                "{bad} should be refused, not silently restarted at the first page"
            );
        }
    }

    #[test]
    fn a_dead_letter_cursor_round_trips() {
        let cursor = format_dead_letter_cursor(1_700_000_000, "an-id");
        assert_eq!(
            parse_dead_letter_cursor(&cursor).unwrap(),
            (1_700_000_000, "an-id".to_string())
        );
        assert_eq!(clamp_dead_letter_page(None), DEFAULT_DEAD_LETTER_PAGE);
        assert_eq!(clamp_dead_letter_page(Some(0)), DEFAULT_DEAD_LETTER_PAGE);
        assert_eq!(clamp_dead_letter_page(Some(10)), 10);
        assert_eq!(clamp_dead_letter_page(Some(100_000)), MAX_DEAD_LETTER_PAGE);
    }

    /// A queue.sqlite written before the lease token existed holds messages
    /// that still have to be delivered. Refusing it would strand them; adding
    /// the column keeps them in line and gives them a token on their next
    /// delivery.
    #[tokio::test]
    async fn a_database_written_before_lease_tokens_is_widened_rather_than_refused() {
        let dir = TempDir::new().unwrap();
        let conn = rusqlite::Connection::open(dir.path().join("queue.sqlite")).unwrap();
        conn.execute_batch(
            "CREATE TABLE messages (
                 tenant     TEXT    NOT NULL,
                 queue      TEXT    NOT NULL,
                 id         TEXT    NOT NULL,
                 body       BLOB    NOT NULL,
                 attempts   INTEGER NOT NULL DEFAULT 0,
                 visible_at INTEGER NOT NULL,
                 created_at INTEGER NOT NULL,
                 PRIMARY KEY (tenant, queue, id)
             );
             INSERT INTO messages (tenant, queue, id, body, attempts, visible_at, created_at)
                  VALUES ('app-one.production', 'jobs', 'waiting', CAST('work' AS BLOB), 0, 1, 1);",
        )
        .unwrap();
        drop(conn);

        let clock = Clock::fixed(T0);
        let service = QueueService::open_with_clock(dir.path(), 4, QueueLimits::default(), clock)
            .await
            .expect("an existing queue must open, not be refused");
        let a = tenant("app-one.production");

        let leased = service.pull(&a, "jobs", None, None).await.unwrap();
        assert_eq!(leased.len(), 1, "the waiting message must still be there");
        assert_eq!(leased[0].id, "waiting");
        assert!(
            !leased[0].lease_token.is_empty(),
            "its first delivery under the new schema issues a token"
        );
        assert_eq!(
            service.ack(&a, "jobs", handles(&leased)).await.unwrap(),
            AckOutcome {
                acknowledged: 1,
                stale_leases: 0
            }
        );
    }

    /// The same for the dead letter table. A queue.sqlite written before the
    /// reason was recorded holds messages an operator still has to be able to
    /// list and replay, so the column is added and the rows already there are
    /// listed with no reason rather than with an invented one.
    #[tokio::test]
    async fn dead_letters_from_before_the_reason_existed_are_still_readable() {
        let dir = TempDir::new().unwrap();
        let conn = rusqlite::Connection::open(dir.path().join("queue.sqlite")).unwrap();
        conn.execute_batch(
            "CREATE TABLE dead_letter (
                 tenant     TEXT    NOT NULL,
                 queue      TEXT    NOT NULL,
                 id         TEXT    NOT NULL,
                 body       BLOB    NOT NULL,
                 attempts   INTEGER NOT NULL,
                 created_at INTEGER NOT NULL,
                 died_at    INTEGER NOT NULL,
                 PRIMARY KEY (tenant, queue, id)
             );
             INSERT INTO dead_letter (tenant, queue, id, body, attempts, created_at, died_at)
                  VALUES ('app-one.production', 'jobs', 'old', CAST('poison' AS BLOB), 5, 1, 2);",
        )
        .unwrap();
        drop(conn);

        let clock = Clock::fixed(T0);
        let service = QueueService::open_with_clock(dir.path(), 4, QueueLimits::default(), clock)
            .await
            .expect("an existing queue must open, not be refused");
        let a = tenant("app-one.production");

        let dead = service.dead_letters(&a, "jobs").await.unwrap();
        assert_eq!(dead.len(), 1, "the message must still be listed");
        assert_eq!(dead[0].id, "old");
        assert_eq!(dead[0].attempts, 5);
        assert_eq!(
            dead[0].reason, "",
            "no reason was ever recorded for this row, and none may be invented for it"
        );
        // And it can still be put back, which is the whole point of listing it.
        assert_eq!(
            service
                .replay_dead_letters(&a, "jobs", vec!["old".to_string()], None)
                .await
                .unwrap(),
            ReplayOutcome {
                replayed: 1,
                not_found: 0
            }
        );
    }

    #[tokio::test]
    async fn tenants_cannot_see_each_other() {
        let f = fixture().await;
        let one = tenant("app-one.production");
        let two = tenant("app-two.production");
        let id = f.service.send(&one, "jobs", b"one".to_vec()).await.unwrap();

        assert!(
            f.service
                .pull(&two, "jobs", None, None)
                .await
                .unwrap()
                .is_empty()
        );
        // Nor can one tenant acknowledge another's message away, even
        // holding its id.
        assert_eq!(
            f.service
                .ack(&two, "jobs", vec![forged(&id)])
                .await
                .unwrap(),
            AckOutcome::default()
        );
        assert_eq!(
            f.service
                .nack(&two, "jobs", vec![forged(&id)], None)
                .await
                .unwrap(),
            NackOutcome::default()
        );

        let leased = f.service.pull(&one, "jobs", None, None).await.unwrap();
        assert_eq!(leased.len(), 1);
        assert_eq!(leased[0].id, id);
    }

    #[tokio::test]
    async fn queues_of_one_app_are_separate() {
        let f = fixture().await;
        let a = tenant("app-one.production");
        f.service.send(&a, "jobs", b"job".to_vec()).await.unwrap();
        f.service
            .send(&a, "emails", b"email".to_vec())
            .await
            .unwrap();

        assert_eq!(
            bodies(&f.service.pull(&a, "jobs", None, None).await.unwrap()),
            vec!["job".to_string()]
        );
        assert_eq!(
            bodies(&f.service.pull(&a, "emails", None, None).await.unwrap()),
            vec!["email".to_string()]
        );
    }

    #[tokio::test]
    async fn messages_come_back_oldest_first_and_only_up_to_the_asked_count() {
        let f = fixture().await;
        let a = tenant("app-one.production");
        for i in 0..5 {
            f.service
                .send(&a, "jobs", format!("m{i}").into_bytes())
                .await
                .unwrap();
            f.clock.advance(1);
        }

        let first = f.service.pull(&a, "jobs", Some(2), None).await.unwrap();
        assert_eq!(bodies(&first), vec!["m0".to_string(), "m1".to_string()]);
        let second = f.service.pull(&a, "jobs", Some(2), None).await.unwrap();
        assert_eq!(bodies(&second), vec!["m2".to_string(), "m3".to_string()]);
        let third = f.service.pull(&a, "jobs", Some(10), None).await.unwrap();
        assert_eq!(bodies(&third), vec!["m4".to_string()]);
    }

    #[tokio::test]
    async fn a_batch_is_stored_whole_or_not_at_all() {
        let f = fixture_with(QueueLimits {
            max_body_bytes: 8,
            ..QueueLimits::default()
        })
        .await;
        let a = tenant("app-one.production");

        let err = f
            .service
            .send_batch(
                &a,
                "jobs",
                vec![b"fine".to_vec(), vec![b'x'; 9], b"also fine".to_vec()],
            )
            .await
            .unwrap_err();
        assert!(matches!(err, QueueError::TooLarge(_)), "got {err:?}");
        assert!(
            f.service
                .pull(&a, "jobs", None, None)
                .await
                .unwrap()
                .is_empty(),
            "a refused batch must not have stored its good messages"
        );

        let ids = f
            .service
            .send_batch(&a, "jobs", vec![b"one".to_vec(), b"two".to_vec()])
            .await
            .unwrap();
        assert_eq!(ids.len(), 2);
        assert_eq!(
            bodies(&f.service.pull(&a, "jobs", None, None).await.unwrap()),
            vec!["one".to_string(), "two".to_string()]
        );
    }

    #[tokio::test]
    async fn a_pull_stops_at_the_response_size_budget() {
        let f = fixture_with(QueueLimits {
            max_body_bytes: 1024,
            max_pull_response_bytes: 2048,
            ..QueueLimits::default()
        })
        .await;
        let a = tenant("app-one.production");
        for _ in 0..4 {
            f.service.send(&a, "jobs", vec![b'x'; 1024]).await.unwrap();
        }

        // Two bodies fill the budget exactly; the third would go over.
        let first = f.service.pull(&a, "jobs", Some(4), None).await.unwrap();
        assert_eq!(first.len(), 2);
        // The ones left behind were not leased, so they are ready right away.
        let second = f.service.pull(&a, "jobs", Some(4), None).await.unwrap();
        assert_eq!(second.len(), 2);
        for message in first.iter().chain(second.iter()) {
            assert_eq!(message.attempts, 1, "no message may be counted twice");
        }
    }

    #[tokio::test]
    async fn an_oversized_message_is_refused_rather_than_stored() {
        let f = fixture_with(QueueLimits {
            max_body_bytes: 16,
            ..QueueLimits::default()
        })
        .await;
        let a = tenant("app-one.production");
        let err = f
            .service
            .send(&a, "jobs", vec![b'x'; 17])
            .await
            .unwrap_err();
        assert!(matches!(err, QueueError::TooLarge(_)), "got {err:?}");
        assert!(
            f.service
                .pull(&a, "jobs", None, None)
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn a_body_larger_than_a_pull_response_cannot_be_configured() {
        // Otherwise a message could be accepted, stored, and then never
        // delivered, which is the one failure a queue must not have.
        let f = fixture_with(QueueLimits {
            max_body_bytes: MAX_PULL_RESPONSE_BYTES + 1,
            ..QueueLimits::default()
        })
        .await;
        assert_eq!(f.service.limits().max_body_bytes, MAX_PULL_RESPONSE_BYTES);
    }

    #[tokio::test]
    async fn an_attempt_ceiling_below_one_is_corrected() {
        let f = fixture_with(QueueLimits {
            max_attempts: 0,
            ..QueueLimits::default()
        })
        .await;
        assert_eq!(f.service.limits().max_attempts, 1);
        let a = tenant("app-one.production");
        f.service.send(&a, "jobs", b"work".to_vec()).await.unwrap();
        // One delivery is still one delivery: the message is not retired
        // before anyone has seen it.
        assert_eq!(
            f.service.pull(&a, "jobs", None, None).await.unwrap().len(),
            1
        );
    }

    #[tokio::test]
    async fn bad_queue_names_are_refused() {
        let f = fixture().await;
        let a = tenant("app-one.production");
        for name in ["", "with space", "slash/name", &"q".repeat(129)] {
            assert!(
                matches!(
                    f.service.send(&a, name, b"v".to_vec()).await,
                    Err(QueueError::InvalidArgument(_))
                ),
                "{} should be refused",
                name.escape_debug()
            );
        }
        // The characters a manifest actually uses are accepted.
        for name in ["jobs", "email-out", "a.b", "tenant:jobs", "q_1"] {
            assert!(
                f.service.send(&a, name, b"v".to_vec()).await.is_ok(),
                "{name}"
            );
        }
    }

    #[tokio::test]
    async fn acknowledging_an_unknown_id_is_harmless() {
        let f = fixture().await;
        let a = tenant("app-one.production");
        f.service.send(&a, "jobs", b"work".to_vec()).await.unwrap();
        let leased = f.service.pull(&a, "jobs", None, None).await.unwrap();
        assert_eq!(
            f.service.ack(&a, "jobs", handles(&leased)).await.unwrap(),
            AckOutcome {
                acknowledged: 1,
                stale_leases: 0
            }
        );
        // A retried acknowledgement removes nothing and does not fail. It is
        // not a stale lease either: the message is gone, not somebody
        // else's.
        assert_eq!(
            f.service.ack(&a, "jobs", handles(&leased)).await.unwrap(),
            AckOutcome::default()
        );
        assert_eq!(
            f.service.ack(&a, "jobs", Vec::new()).await.unwrap(),
            AckOutcome::default()
        );
    }

    #[tokio::test]
    async fn naming_more_messages_than_a_pull_could_return_is_refused() {
        let f = fixture_with(QueueLimits {
            max_pull_messages: 2,
            ..QueueLimits::default()
        })
        .await;
        let a = tenant("app-one.production");
        let named: Vec<MessageHandle> = (0..3).map(|i| forged(&format!("id-{i}"))).collect();
        assert!(matches!(
            f.service.ack(&a, "jobs", named.clone()).await,
            Err(QueueError::TooLarge(_))
        ));
        assert!(matches!(
            f.service.nack(&a, "jobs", named, None).await,
            Err(QueueError::TooLarge(_))
        ));
    }

    #[tokio::test]
    async fn requested_sizes_leases_and_delays_are_clamped() {
        let f = fixture_with(QueueLimits {
            default_pull_messages: 10,
            max_pull_messages: 20,
            default_lease_secs: 30,
            max_lease_secs: 100,
            ..QueueLimits::default()
        })
        .await;
        let svc = &f.service;
        assert_eq!(svc.clamp_pull_size(None), 10);
        assert_eq!(svc.clamp_pull_size(Some(0)), 10, "zero means the default");
        assert_eq!(svc.clamp_pull_size(Some(-1)), 10);
        assert_eq!(svc.clamp_pull_size(Some(5)), 5);
        assert_eq!(svc.clamp_pull_size(Some(1_000)), 20);

        assert_eq!(svc.clamp_lease(None), 30);
        assert_eq!(svc.clamp_lease(Some(0)), 30, "zero means the default");
        assert_eq!(svc.clamp_lease(Some(45)), 45);
        assert_eq!(svc.clamp_lease(Some(10_000)), 100);

        // A zero delay is meaningful - hand it back now - so it is kept.
        assert_eq!(svc.clamp_delay(Some(0)), 0);
        assert_eq!(svc.clamp_delay(None), 0);
        assert_eq!(svc.clamp_delay(Some(-5)), 0);
        assert_eq!(svc.clamp_delay(Some(60)), 60);
        assert_eq!(svc.clamp_delay(Some(10_000)), 100);
    }

    #[tokio::test]
    async fn dead_letters_are_kept_until_the_retention_lapses() {
        let f = fixture_with(QueueLimits {
            max_attempts: 1,
            dead_letter_retention_secs: 600,
            ..QueueLimits::default()
        })
        .await;
        let a = tenant("app-one.production");
        f.service.send(&a, "jobs", b"work".to_vec()).await.unwrap();
        let leased = f.service.pull(&a, "jobs", None, None).await.unwrap();
        f.service
            .nack(&a, "jobs", handles(&leased), None)
            .await
            .unwrap();
        assert_eq!(f.service.dead_letters(&a, "jobs").await.unwrap().len(), 1);

        f.clock.advance(599);
        assert_eq!(f.service.sweep_dead_letters().await.unwrap(), 0);
        assert_eq!(f.service.dead_letters(&a, "jobs").await.unwrap().len(), 1);

        f.clock.advance(1);
        assert_eq!(f.service.sweep_dead_letters().await.unwrap(), 1);
        assert!(f.service.dead_letters(&a, "jobs").await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_zero_retention_keeps_dead_letters_forever() {
        let f = fixture_with(QueueLimits {
            max_attempts: 1,
            dead_letter_retention_secs: 0,
            ..QueueLimits::default()
        })
        .await;
        let a = tenant("app-one.production");
        f.service.send(&a, "jobs", b"work".to_vec()).await.unwrap();
        let leased = f.service.pull(&a, "jobs", None, None).await.unwrap();
        f.service
            .nack(&a, "jobs", handles(&leased), None)
            .await
            .unwrap();

        f.clock.advance(10_000_000);
        assert_eq!(f.service.sweep_dead_letters().await.unwrap(), 0);
        assert_eq!(f.service.dead_letters(&a, "jobs").await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn two_consumers_pulling_at_once_never_get_the_same_message() {
        let f = fixture().await;
        let a = tenant("app-one.production");
        for i in 0..40 {
            f.service
                .send(&a, "jobs", format!("m{i}").into_bytes())
                .await
                .unwrap();
        }

        let mut tasks = Vec::new();
        for _ in 0..8 {
            let svc = f.service.clone();
            let tenant = a.clone();
            tasks.push(tokio::spawn(async move {
                svc.pull(&tenant, "jobs", Some(5), Some(60)).await
            }));
        }
        let mut seen = std::collections::HashSet::new();
        let mut total = 0;
        for task in tasks {
            for message in task.await.unwrap().expect("no pull may be refused") {
                total += 1;
                assert!(
                    seen.insert(message.id.clone()),
                    "delivered twice under load"
                );
            }
        }
        assert_eq!(total, 40, "every message should have been leased once");
    }

    #[tokio::test]
    async fn a_call_without_a_tenant_header_is_refused_before_anything_is_queued() {
        let f = fixture().await;
        let status = Queue::send(
            &f.service,
            Request::new(SendRequest {
                queue_name: "jobs".to_string(),
                message_body: b"work".to_vec(),
            }),
        )
        .await
        .unwrap_err();
        assert_eq!(status.code(), tonic::Code::PermissionDenied);
    }

    #[tokio::test]
    async fn the_grpc_surface_round_trips_a_message() {
        let f = fixture().await;
        let sent = Queue::send(
            &f.service,
            as_tenant(
                "app-one.production",
                SendRequest {
                    queue_name: "jobs".to_string(),
                    message_body: b"work".to_vec(),
                },
            ),
        )
        .await
        .unwrap()
        .into_inner();
        assert!(sent.success);
        assert!(sent.error.is_empty());

        let pulled = Queue::pull(
            &f.service,
            as_tenant(
                "app-one.production",
                PullRequest {
                    queue_name: "jobs".to_string(),
                    max_messages: 0,
                    lease_seconds: 0,
                },
            ),
        )
        .await
        .unwrap()
        .into_inner();
        assert_eq!(pulled.messages.len(), 1);
        assert_eq!(pulled.messages[0].id, sent.message_id);
        assert_eq!(pulled.messages[0].body, b"work");
        assert_eq!(pulled.messages[0].attempts, 1);
        assert_eq!(pulled.messages[0].lease_expires_at, T0 + 30);
        assert!(!pulled.messages[0].lease_token.is_empty());

        let acked = Queue::ack(
            &f.service,
            as_tenant(
                "app-one.production",
                AckRequest {
                    queue_name: "jobs".to_string(),
                    messages: vec![crate::proto::queue::MessageHandle {
                        id: sent.message_id,
                        lease_token: pulled.messages[0].lease_token.clone(),
                    }],
                },
            ),
        )
        .await
        .unwrap()
        .into_inner();
        assert_eq!(acked.acknowledged, 1);
        assert_eq!(acked.stale_leases, 0);
    }

    /// A body that is not UTF-8 is delivered like any other.
    ///
    /// It used to be withheld from the pull, because `QueueMessage.body` was
    /// a string: the message stayed leased, its attempts climbed, and the
    /// ceiling eventually dead lettered a message nothing was wrong with. The
    /// dead letter table has always typed its bodies as bytes for exactly
    /// this case; now the whole path does.
    #[tokio::test]
    async fn a_body_that_is_not_utf8_is_delivered_rather_than_withheld() {
        let f = fixture().await;
        let a = tenant("app-one.production");
        f.service.send(&a, "jobs", vec![0xff, 0xfe]).await.unwrap();
        f.service
            .send(&a, "jobs", b"readable".to_vec())
            .await
            .unwrap();

        let pulled = Queue::pull(
            &f.service,
            as_tenant(
                "app-one.production",
                PullRequest {
                    queue_name: "jobs".to_string(),
                    max_messages: 10,
                    lease_seconds: 0,
                },
            ),
        )
        .await
        .unwrap()
        .into_inner();
        let bodies: Vec<Vec<u8>> = pulled.messages.iter().map(|m| m.body.clone()).collect();
        assert_eq!(bodies, vec![vec![0xff, 0xfe], b"readable".to_vec()]);
    }

    /// The producer half, over the generated types: bytes that are not text
    /// go in through Send and come back out of Pull unchanged.
    #[tokio::test]
    async fn the_grpc_surface_round_trips_a_body_that_is_not_text() {
        let f = fixture().await;
        let body = vec![0x00, 0x1f, 0x80, 0xff, 0xfe];
        Queue::send(
            &f.service,
            as_tenant(
                "app-one.production",
                SendRequest {
                    queue_name: "jobs".to_string(),
                    message_body: body.clone(),
                },
            ),
        )
        .await
        .unwrap();

        let pulled = Queue::pull(
            &f.service,
            as_tenant(
                "app-one.production",
                PullRequest {
                    queue_name: "jobs".to_string(),
                    max_messages: 10,
                    lease_seconds: 0,
                },
            ),
        )
        .await
        .unwrap()
        .into_inner();
        assert_eq!(pulled.messages.len(), 1);
        assert_eq!(pulled.messages[0].body, body);
    }
}
