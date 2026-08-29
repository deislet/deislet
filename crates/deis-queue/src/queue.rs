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
//! Every read-then-write here runs in one `IMMEDIATE` transaction. `Pull` is
//! the reason: two consumers polling the same queue at the same moment both
//! read the same candidate rows, and whichever writes second must wait rather
//! than be refused. See [`crate::sqlite::SqlitePool::with_tx`].

use crate::Clock;
use crate::MAX_GRPC_MESSAGE_BYTES;
use crate::error::{QueueError, QueueResult};
use crate::proto::queue::{
    AckRequest, AckResponse, NackRequest, NackResponse, PullRequest, PullResponse, QueueMessage,
    SendBatchRequest, SendBatchResponse, SendRequest, SendResponse, queue_server::Queue,
};
use crate::sqlite::{PoolSize, SqlitePool};
use crate::tenant::{TenantId, tenant_of};
use rusqlite::{OptionalExtension, Transaction};
use tonic::{Request, Response, Status};

/// Longest accepted queue name. A name lives in a primary key and in log
/// lines; past this it is an accident, not a design.
const MAX_QUEUE_NAME_LEN: usize = 128;

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
}

/// What one `Nack` did.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct NackOutcome {
    /// Messages that will be delivered again.
    pub released: usize,
    /// Messages moved to the dead letter table instead.
    pub dead_lettered: usize,
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
                     tenant     TEXT    NOT NULL,
                     queue      TEXT    NOT NULL,
                     id         TEXT    NOT NULL,
                     body       BLOB    NOT NULL,
                     attempts   INTEGER NOT NULL DEFAULT 0,
                     visible_at INTEGER NOT NULL,
                     created_at INTEGER NOT NULL,
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
                     PRIMARY KEY (tenant, queue, id)
                 );
                 CREATE INDEX IF NOT EXISTS dead_letter_died_at
                     ON dead_letter(died_at);",
            )?;
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
    /// while counting the delivery. Splitting those apart would let a second
    /// consumer see a message this call has already claimed.
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
                    });
                }
                drop(rows);
                drop(select);

                let mut lease_stmt = tx.prepare(
                    "UPDATE messages
                        SET visible_at = ?4, attempts = attempts + 1
                      WHERE tenant = ?1 AND queue = ?2 AND id = ?3",
                )?;
                for message in &leased {
                    lease_stmt.execute(rusqlite::params![
                        tenant_id,
                        table_queue,
                        message.id,
                        lease_expires_at
                    ])?;
                }
                drop(lease_stmt);
                Ok((leased, retired))
            })
            .await?;

        for (id, attempts) in &retired {
            warn_dead_lettered(tenant, &queue, id, *attempts, ceiling, "its lease lapsed");
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

    /// Delete handled messages. Ids that are not there are ignored, so a
    /// repeated acknowledgement is harmless. Returns how many rows went away.
    pub async fn ack(
        &self,
        tenant: &TenantId,
        queue: &str,
        ids: Vec<String>,
    ) -> QueueResult<usize> {
        let queue = self.check_queue(queue)?;
        if ids.is_empty() {
            return Ok(0);
        }
        self.check_id_count(&queue, ids.len())?;
        let tenant_id = tenant.as_str().to_string();
        let table_queue = queue.clone();
        let asked = ids.len();

        let removed = self
            .pool
            .with_tx(move |tx| {
                let mut delete = tx
                    .prepare("DELETE FROM messages WHERE tenant = ?1 AND queue = ?2 AND id = ?3")?;
                let mut removed = 0usize;
                for id in &ids {
                    removed += delete.execute(rusqlite::params![tenant_id, table_queue, id])?;
                }
                Ok(removed)
            })
            .await?;

        if removed < asked {
            // Not an error: the usual causes are a retried ack and a lease
            // that lapsed while the handler was still running, and neither
            // needs an operator. It is worth seeing, because the second one
            // means the message is being handled twice.
            tracing::info!(
                tenant = %tenant,
                queue = %queue,
                acknowledged = removed,
                asked,
                "some acknowledged ids were no longer queued; they were already acknowledged, or \
                 their lease lapsed and they have been handed to another consumer"
            );
        } else {
            tracing::debug!(
                tenant = %tenant,
                queue = %queue,
                acknowledged = removed,
                "acknowledged"
            );
        }
        Ok(removed)
    }

    /// Hand messages back unhandled. Each becomes visible again after
    /// `delay_seconds`, unless it has used up its attempts, in which case it
    /// is dead lettered instead.
    pub async fn nack(
        &self,
        tenant: &TenantId,
        queue: &str,
        ids: Vec<String>,
        delay_seconds: Option<i32>,
    ) -> QueueResult<NackOutcome> {
        let queue = self.check_queue(queue)?;
        if ids.is_empty() {
            return Ok(NackOutcome::default());
        }
        self.check_id_count(&queue, ids.len())?;
        let delay = self.clamp_delay(delay_seconds);
        let now = self.clock.now();
        let visible_at = now.saturating_add(delay);
        let ceiling = self.limits.max_attempts;
        let tenant_id = tenant.as_str().to_string();
        let table_queue = queue.clone();

        let (released, dead) = self
            .pool
            .with_tx(move |tx| {
                let mut released = 0usize;
                let mut dead: Vec<(String, i32)> = Vec::new();
                for id in &ids {
                    // An id that is not there is skipped, not an error: it
                    // was already acknowledged, or its lease lapsed and it
                    // has moved on without this consumer.
                    let attempts: Option<i32> = tx
                        .query_row(
                            "SELECT attempts FROM messages
                              WHERE tenant = ?1 AND queue = ?2 AND id = ?3",
                            rusqlite::params![tenant_id, table_queue, id],
                            |row| row.get(0),
                        )
                        .optional()?;
                    let Some(attempts) = attempts else { continue };

                    if attempts >= ceiling {
                        move_to_dead_letter(tx, &tenant_id, &table_queue, id, now)?;
                        dead.push((id.clone(), attempts));
                    } else {
                        tx.execute(
                            "UPDATE messages SET visible_at = ?4
                              WHERE tenant = ?1 AND queue = ?2 AND id = ?3",
                            rusqlite::params![tenant_id, table_queue, id, visible_at],
                        )?;
                        released += 1;
                    }
                }
                Ok((released, dead))
            })
            .await?;

        for (id, attempts) in &dead {
            warn_dead_lettered(
                tenant,
                &queue,
                id,
                *attempts,
                ceiling,
                "the consumer reported it failed",
            );
        }
        tracing::debug!(
            tenant = %tenant,
            queue = %queue,
            released,
            dead_lettered = dead.len(),
            delay_seconds = delay,
            "returned messages"
        );
        Ok(NackOutcome {
            released,
            dead_lettered: dead.len(),
        })
    }

    /// Every dead letter of one tenant's queue, oldest death first. There
    /// is no RPC for this yet: dead letters are read by an operator against
    /// the database, and this is what the tests read them with.
    pub async fn dead_letters(
        &self,
        tenant: &TenantId,
        queue: &str,
    ) -> QueueResult<Vec<DeadLetter>> {
        let queue = self.check_queue(queue)?;
        let tenant_id = tenant.as_str().to_string();
        self.pool
            .with_conn(move |conn| {
                let mut stmt = conn.prepare(
                    "SELECT queue, id, body, attempts, created_at, died_at
                       FROM dead_letter
                      WHERE tenant = ?1 AND queue = ?2
                      ORDER BY died_at ASC, id ASC",
                )?;
                let mut rows = stmt.query(rusqlite::params![tenant_id, queue])?;
                let mut out = Vec::new();
                while let Some(row) = rows.next()? {
                    out.push(DeadLetter {
                        queue: row.get(0)?,
                        id: row.get(1)?,
                        body: row.get(2)?,
                        attempts: row.get(3)?,
                        created_at: row.get(4)?,
                        died_at: row.get(5)?,
                    });
                }
                Ok(out)
            })
            .await
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

    /// Bound how many ids one Ack or Nack may name. The ids come from a pull,
    /// so a call naming more than a pull could ever return is a caller bug,
    /// and serving it would let one request hold the write lock for as long
    /// as it liked.
    fn check_id_count(&self, queue: &str, count: usize) -> QueueResult<()> {
        let limit = self.limits.max_pull_messages as usize;
        if count > limit {
            tracing::warn!(
                queue = %queue,
                ids = count,
                limit,
                "refusing a call that names more messages than one pull can return; none of them \
                 were acknowledged or returned"
            );
            return Err(QueueError::TooLarge(format!(
                "call names {count} messages, limit is {limit}"
            )));
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
        move_to_dead_letter(tx, tenant, queue, id, now)?;
    }
    Ok(exhausted)
}

/// Copy one message into the dead letter table and remove it from the queue.
/// The caller is inside the transaction, so both halves land together.
fn move_to_dead_letter(
    tx: &Transaction<'_>,
    tenant: &str,
    queue: &str,
    id: &str,
    now: i64,
) -> QueueResult<()> {
    tx.execute(
        "INSERT INTO dead_letter (tenant, queue, id, body, attempts, created_at, died_at)
              SELECT tenant, queue, id, body, attempts, created_at, ?4
                FROM messages
               WHERE tenant = ?1 AND queue = ?2 AND id = ?3
         ON CONFLICT(tenant, queue, id) DO UPDATE SET
              attempts = excluded.attempts,
              died_at  = excluded.died_at",
        rusqlite::params![tenant, queue, id, now],
    )?;
    tx.execute(
        "DELETE FROM messages WHERE tenant = ?1 AND queue = ?2 AND id = ?3",
        rusqlite::params![tenant, queue, id],
    )?;
    Ok(())
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
            .send(&tenant, &req.queue_name, req.message_body.into_bytes())
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
        let bodies = req.messages.into_iter().map(String::into_bytes).collect();
        let ids = self.send_batch(&tenant, &req.queue_name, bodies).await?;
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

        let mut messages = Vec::with_capacity(leased.len());
        for message in leased {
            match String::from_utf8(message.body) {
                Ok(body) => messages.push(QueueMessage {
                    id: message.id,
                    body,
                    attempts: message.attempts,
                    lease_expires_at: message.lease_expires_at,
                    created_at: message.created_at,
                }),
                Err(e) => {
                    // queue.proto types the body as a string, so bytes that
                    // are not UTF-8 cannot be handed back at all. Dropping
                    // this one message rather than failing the whole pull
                    // keeps its healthy neighbours moving; it stays leased,
                    // and its attempts keep climbing until the ceiling
                    // retires it into the dead letter table.
                    tracing::error!(
                        tenant = %tenant,
                        queue = %req.queue_name,
                        message_id = %message.id,
                        attempts = message.attempts,
                        error = %e,
                        "stored message body is not valid UTF-8 and cannot be delivered; it is \
                         withheld from this pull and will be dead lettered once its attempts run \
                         out"
                    );
                }
            }
        }
        Ok(Response::new(PullResponse { messages }))
    }

    async fn ack(&self, request: Request<AckRequest>) -> Result<Response<AckResponse>, Status> {
        let tenant = tenant_of(&request)?;
        let req = request.into_inner();
        let acknowledged = self.ack(&tenant, &req.queue_name, req.message_ids).await?;
        Ok(Response::new(AckResponse {
            acknowledged: acknowledged as i32,
        }))
    }

    async fn nack(&self, request: Request<NackRequest>) -> Result<Response<NackResponse>, Status> {
        let tenant = tenant_of(&request)?;
        let req = request.into_inner();
        let outcome = self
            .nack(
                &tenant,
                &req.queue_name,
                req.message_ids,
                Some(req.delay_seconds),
            )
            .await?;
        Ok(Response::new(NackResponse {
            released: outcome.released as i32,
            dead_lettered: outcome.dead_lettered as i32,
        }))
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

        assert_eq!(f.service.ack(&a, "jobs", vec![id]).await.unwrap(), 1);
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
        f.service.pull(&a, "jobs", None, None).await.unwrap();

        let outcome = f
            .service
            .nack(&a, "jobs", vec![id.clone()], Some(45))
            .await
            .unwrap();
        assert_eq!(
            outcome,
            NackOutcome {
                released: 1,
                dead_lettered: 0
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
        let id = f.service.send(&a, "jobs", b"work".to_vec()).await.unwrap();
        f.service.pull(&a, "jobs", None, None).await.unwrap();
        f.service.nack(&a, "jobs", vec![id], None).await.unwrap();
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
                .nack(&a, "jobs", vec![id.clone()], None)
                .await
                .unwrap();
            if attempt < 3 {
                assert_eq!(outcome.released, 1, "attempt {attempt} should be retried");
            } else {
                assert_eq!(
                    outcome,
                    NackOutcome {
                        released: 0,
                        dead_lettered: 1
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
        // Nor can one tenant acknowledge another's message away.
        assert_eq!(
            f.service.ack(&two, "jobs", vec![id.clone()]).await.unwrap(),
            0
        );
        assert_eq!(
            f.service
                .nack(&two, "jobs", vec![id.clone()], None)
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
        let id = f.service.send(&a, "jobs", b"work".to_vec()).await.unwrap();
        f.service.pull(&a, "jobs", None, None).await.unwrap();
        assert_eq!(
            f.service.ack(&a, "jobs", vec![id.clone()]).await.unwrap(),
            1
        );
        // A retried acknowledgement removes nothing and does not fail.
        assert_eq!(f.service.ack(&a, "jobs", vec![id]).await.unwrap(), 0);
        assert_eq!(f.service.ack(&a, "jobs", Vec::new()).await.unwrap(), 0);
    }

    #[tokio::test]
    async fn naming_more_messages_than_a_pull_could_return_is_refused() {
        let f = fixture_with(QueueLimits {
            max_pull_messages: 2,
            ..QueueLimits::default()
        })
        .await;
        let a = tenant("app-one.production");
        let ids: Vec<String> = (0..3).map(|i| format!("id-{i}")).collect();
        assert!(matches!(
            f.service.ack(&a, "jobs", ids.clone()).await,
            Err(QueueError::TooLarge(_))
        ));
        assert!(matches!(
            f.service.nack(&a, "jobs", ids, None).await,
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
        let id = f.service.send(&a, "jobs", b"work".to_vec()).await.unwrap();
        f.service.pull(&a, "jobs", None, None).await.unwrap();
        f.service.nack(&a, "jobs", vec![id], None).await.unwrap();
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
        let id = f.service.send(&a, "jobs", b"work".to_vec()).await.unwrap();
        f.service.pull(&a, "jobs", None, None).await.unwrap();
        f.service.nack(&a, "jobs", vec![id], None).await.unwrap();

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
                message_body: "work".to_string(),
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
                    message_body: "work".to_string(),
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
        assert_eq!(pulled.messages[0].body, "work");
        assert_eq!(pulled.messages[0].attempts, 1);
        assert_eq!(pulled.messages[0].lease_expires_at, T0 + 30);

        let acked = Queue::ack(
            &f.service,
            as_tenant(
                "app-one.production",
                AckRequest {
                    queue_name: "jobs".to_string(),
                    message_ids: vec![sent.message_id],
                },
            ),
        )
        .await
        .unwrap()
        .into_inner();
        assert_eq!(acked.acknowledged, 1);
    }

    /// A body that is not UTF-8 cannot be put on the wire at all, and the
    /// only way one gets into the table is damage. It must not take the
    /// whole pull down with it.
    #[tokio::test]
    async fn a_message_that_cannot_be_encoded_is_withheld_without_hiding_its_neighbours() {
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
        assert_eq!(pulled.messages.len(), 1);
        assert_eq!(pulled.messages[0].body, "readable");
    }
}
