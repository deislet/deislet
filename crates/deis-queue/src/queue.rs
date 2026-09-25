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
//! The expiry is part of that proof too. Even if nobody has taken the message
//! again yet, a handler whose lease elapsed may not Ack or Nack across a
//! deployment barrier; the row stays available for a current consumer.
//!
//! Every read-then-write here runs in one `IMMEDIATE` transaction. `Pull` is
//! the reason: two consumers polling the same queue at the same moment both
//! read the same candidate rows, and whichever writes second must wait rather
//! than be refused. See [`crate::sqlite::SqlitePool::with_tx`].

use crate::Clock;
use crate::MAX_GRPC_MESSAGE_BYTES;
use crate::error::{QueueError, QueueResult};
use crate::proto::queue::{
    AbortConsumerGenerationRequest, AbortConsumerGenerationResponse, AckRequest, AckResponse,
    ActivateConsumerGenerationRequest, ActivateConsumerGenerationResponse, DeadLetterMessage,
    ListDeadLettersRequest, ListDeadLettersResponse, NackRequest, NackResponse,
    PrepareConsumerGenerationRequest, PrepareConsumerGenerationResponse, PullRequest, PullResponse,
    QueueMessage, ReplayDeadLettersRequest, ReplayDeadLettersResponse, SendBatchRequest,
    SendBatchResponse, SendRequest, SendResponse, queue_server::Queue,
};
use crate::sqlite::{PoolSize, SqlitePool};
use crate::tenant::{TenantId, tenant_of};
use deis_backend_migration::{ComponentInventory, InventoryHasher, TenantWriteFences};
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
    fences: TenantWriteFences,
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
        limits: QueueLimits,
        clock: Clock,
    ) -> QueueResult<Self> {
        let fences = TenantWriteFences::open(data_dir.join("migration-fences.sqlite")).await?;
        Self::open_with_clock_and_fences(data_dir, connections, limits, clock, fences).await
    }

    async fn open_with_clock_and_fences(
        data_dir: &std::path::Path,
        connections: usize,
        mut limits: QueueLimits,
        clock: Clock,
        fences: TenantWriteFences,
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
                     -- Deployment generation that acquired the current
                     -- lease; empty for a legacy consumer.
                     lease_generation TEXT NOT NULL DEFAULT '',
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
                     ON dead_letter(died_at);
                 CREATE TABLE IF NOT EXISTS consumer_generation_fences (
                     tenant                       TEXT PRIMARY KEY,
                     active_generation            TEXT NOT NULL,
                     active_deployment_id         TEXT NOT NULL DEFAULT '',
                     pending_generation           TEXT,
                     pending_deployment_id         TEXT,
                     expected_previous_generation TEXT,
                     expected_previous_deployment_id TEXT,
                     last_aborted_deployment_id    TEXT,
                     last_aborted_generation       TEXT,
                     updated_at                   INTEGER NOT NULL,
                     CHECK((last_aborted_deployment_id IS NULL) =
                           (last_aborted_generation IS NULL))
                 );",
            )?;
            add_lease_token_column(conn)?;
            add_lease_generation_column(conn)?;
            add_active_deployment_id_column(conn)?;
            add_expected_previous_deployment_id_column(conn)?;
            add_consumer_abort_tombstone_columns(conn)?;
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
            fences,
            limits,
            clock,
        })
    }

    /// The ceilings this service applies.
    pub fn limits(&self) -> &QueueLimits {
        &self.limits
    }

    pub(crate) fn fences(&self) -> &TenantWriteFences {
        &self.fences
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
        let _permit = self.fences.permit(tenant.as_str()).await?;
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
        self.pull_for_generation(tenant, queue, max_messages, lease_seconds, None)
            .await
    }

    /// Generation-aware lease admission used by the gRPC consumer path.
    /// Validation and leasing share one IMMEDIATE transaction, so a cutover
    /// cannot slip between "is this generation active?" and the lease write.
    pub async fn pull_for_generation(
        &self,
        tenant: &TenantId,
        queue: &str,
        max_messages: Option<i32>,
        lease_seconds: Option<i32>,
        consumer_generation: Option<&str>,
    ) -> QueueResult<Vec<LeasedMessage>> {
        let _permit = self.fences.permit(tenant.as_str()).await?;
        let queue = self.check_queue(queue)?;
        let wanted = self.clamp_pull_size(max_messages);
        let lease = self.clamp_lease(lease_seconds);
        let now = self.clock.now();
        let lease_expires_at = now.saturating_add(lease);
        let ceiling = self.limits.max_attempts;
        let budget = self.limits.max_pull_response_bytes;

        let tenant_id = tenant.as_str().to_string();
        let table_queue = queue.clone();
        let consumer_generation = consumer_generation.unwrap_or_default().to_string();
        let (leased, retired) = self
            .pool
            .with_tx(move |tx| {
                enforce_consumer_generation(tx, &tenant_id, &consumer_generation)?;
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
                        SET visible_at = ?4, attempts = attempts + 1, lease_token = ?5,
                            lease_generation = ?6
                      WHERE tenant = ?1 AND queue = ?2 AND id = ?3",
                )?;
                for message in &leased {
                    lease_stmt.execute(rusqlite::params![
                        tenant_id,
                        table_queue,
                        message.id,
                        lease_expires_at,
                        message.lease_token,
                        consumer_generation,
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

    /// Close lease admission and wait for the active generation to drain,
    /// without admitting the candidate before the control-plane commit.
    pub async fn prepare_consumer_generation(
        &self,
        tenant: &TenantId,
        deployment_id: &str,
        generation: &str,
        expected_previous_generation: Option<&str>,
        expected_previous_deployment_id: Option<&str>,
    ) -> QueueResult<PrepareConsumerGenerationResponse> {
        let _permit = self.fences.permit(tenant.as_str()).await?;
        validate_generation_identity("deployment_id", deployment_id)?;
        validate_generation_identity("generation", generation)?;
        if let Some(previous) = expected_previous_generation.filter(|value| !value.is_empty()) {
            validate_generation_identity("expected_previous_generation", previous)?;
        }
        if let Some(previous_owner) =
            expected_previous_deployment_id.filter(|value| !value.is_empty())
        {
            validate_previous_deployment_identity(previous_owner)?;
        }
        let tenant_id = tenant.as_str().to_string();
        let deployment_id = deployment_id.to_string();
        let generation = generation.to_string();
        let expected = expected_previous_generation
            .filter(|value| !value.is_empty())
            .map(str::to_string);
        let expected_owner = expected_previous_deployment_id.map(str::to_string);
        validate_expected_consumer_baseline(expected.as_deref(), expected_owner.as_deref())?;
        let now = self.clock.now();
        self.pool
            .with_tx(move |tx| {
                prepare_consumer_generation_tx(
                    tx,
                    &tenant_id,
                    &deployment_id,
                    &generation,
                    expected.as_deref(),
                    expected_owner.as_deref(),
                    now,
                )
            })
            .await
    }

    /// Admit an exactly prepared candidate. This never creates a barrier;
    /// calling it before Prepare completed is rejected.
    pub async fn activate_consumer_generation(
        &self,
        tenant: &TenantId,
        deployment_id: &str,
        generation: &str,
    ) -> QueueResult<ActivateConsumerGenerationResponse> {
        let _permit = self.fences.permit(tenant.as_str()).await?;
        validate_generation_identity("deployment_id", deployment_id)?;
        validate_generation_identity("generation", generation)?;
        let tenant = tenant.as_str().to_string();
        let deployment_id = deployment_id.to_string();
        let generation = generation.to_string();
        let now = self.clock.now();
        self.pool
            .with_tx(move |tx| {
                activate_consumer_generation_tx(tx, &tenant, &deployment_id, &generation, now)
            })
            .await
    }

    /// Reopen the previous generation after an uncommitted deployment fails.
    pub async fn abort_consumer_generation(
        &self,
        tenant: &TenantId,
        deployment_id: &str,
        generation: &str,
    ) -> QueueResult<AbortConsumerGenerationResponse> {
        let _permit = self.fences.permit(tenant.as_str()).await?;
        validate_generation_identity("deployment_id", deployment_id)?;
        validate_generation_identity("generation", generation)?;
        let tenant = tenant.as_str().to_string();
        let deployment_id = deployment_id.to_string();
        let generation = generation.to_string();
        let now = self.clock.now();
        self.pool
            .with_tx(move |tx| {
                abort_consumer_generation_tx(tx, &tenant, &deployment_id, &generation, now)
            })
            .await
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
        let _permit = self.fences.permit(tenant.as_str()).await?;
        let queue = self.check_queue(queue)?;
        if handles.is_empty() {
            return Ok(AckOutcome::default());
        }
        self.check_handles(&queue, &handles)?;
        let tenant_id = tenant.as_str().to_string();
        let table_queue = queue.clone();
        let asked = handles.len();
        let now = self.clock.now();

        let (removed, stale) = self
            .pool
            .with_tx(move |tx| {
                let mut delete = tx.prepare(
                    "DELETE FROM messages
                      WHERE tenant = ?1 AND queue = ?2 AND id = ?3
                        AND lease_token = ?4 AND visible_at > ?5",
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
                        handle.lease_token,
                        now
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
    /// `refund_attempt` gives a delivery attempt back: attempts are counted
    /// the moment a message is handed out, so a consumer that never actually
    /// ran a message would otherwise burn an attempt on a delivery that did
    /// nothing. The dead-letter decision then weighs `attempts - 1` (a
    /// message at its ceiling is released rather than dead lettered), and the
    /// released row's stored count is stepped back by the same amount, so
    /// every later check of the ceiling -- at the next nack, and at pull, when
    /// an exhausted message retires on its own lease -- agrees with the one
    /// made here. Only the platform's own consumer may refund, when the
    /// runtime thread was at capacity and refused the batch before the
    /// application saw it. A consumer that failed real work must not, or a
    /// poison message would be retried without ever being retired by the
    /// ceiling that exists to stop it.
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
        refund_attempt: bool,
    ) -> QueueResult<NackOutcome> {
        let _permit = self.fences.permit(tenant.as_str()).await?;
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
                    let row: Option<(i32, String, i64)> = tx
                        .query_row(
                            "SELECT attempts, lease_token, visible_at FROM messages
                              WHERE tenant = ?1 AND queue = ?2 AND id = ?3",
                            rusqlite::params![tenant_id, table_queue, handle.id],
                            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                        )
                        .optional()?;
                    // An id that is not there is skipped, not an error: it
                    // was already acknowledged, or it has been dead lettered.
                    let Some((attempts, token, lease_expires_at)) = row else {
                        continue;
                    };
                    if token != handle.lease_token || lease_expires_at <= now {
                        stale.push(handle.id.clone());
                        continue;
                    }

                    // The attempt this delivery burned has already been
                    // counted at the pull, so a refund takes it back before
                    // the ceiling is weighed. Saturating keeps the check
                    // honest for a row that somehow has no attempts.
                    let counted = attempts.saturating_sub(i32::from(refund_attempt));
                    if counted >= ceiling {
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
                        // handle just used cannot be used a second time. A
                        // refund also steps the stored count back: the
                        // ceiling is weighed on this column in more than one
                        // place -- here, and at pull, where a message whose
                        // lease lapsed at the ceiling retires on its own --
                        // and any of them seeing the un-refunded count would
                        // undo the refund one delivery later.
                        tx.execute(
                            "UPDATE messages SET visible_at = ?4, lease_token = '', \
                             attempts = MAX(0, attempts - ?5)
                              WHERE tenant = ?1 AND queue = ?2 AND id = ?3",
                            rusqlite::params![
                                tenant_id,
                                table_queue,
                                handle.id,
                                visible_at,
                                i32::from(refund_attempt)
                            ],
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
        let _permit = self.fences.permit(tenant.as_str()).await?;
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

    /// Deterministic evidence for ready/delayed/leased and dead-letter rows
    /// under an active writer fence.
    pub async fn inventory(&self, tenant: &TenantId) -> QueueResult<ComponentInventory> {
        let (_receipt, _fence) = self.fences.fenced_read(tenant.as_str()).await?;
        self.inventory_with_fence_held(tenant).await
    }

    /// Build inventory while the caller holds this tenant's fence read guard.
    pub(crate) async fn inventory_with_fence_held(
        &self,
        tenant: &TenantId,
    ) -> QueueResult<ComponentInventory> {
        let tenant_id = tenant.as_str().to_string();
        self.pool
            .with_conn(move |conn| {
                let tx = conn.transaction()?;
                let mut hash = InventoryHasher::new("queue");
                let mut count = 0u64;
                let mut logical_bytes = 0u64;
                {
                    let mut statement = tx.prepare(
                        "SELECT queue, id, body, attempts, visible_at, created_at, lease_token
                           FROM messages
                          WHERE tenant = ?1
                          ORDER BY queue, id",
                    )?;
                    let mut rows = statement.query([&tenant_id])?;
                    while let Some(row) = rows.next()? {
                        let queue: String = row.get(0)?;
                        let id: String = row.get(1)?;
                        let body: Vec<u8> = row.get(2)?;
                        let attempts: i32 = row.get(3)?;
                        let visible_at: i64 = row.get(4)?;
                        let created_at: i64 = row.get(5)?;
                        let lease_token: String = row.get(6)?;
                        hash.field(b"message");
                        hash.field(queue.as_bytes());
                        hash.field(id.as_bytes());
                        hash.field(&body);
                        hash.field(&attempts.to_be_bytes());
                        hash.field(&visible_at.to_be_bytes());
                        hash.field(&created_at.to_be_bytes());
                        hash.field(lease_token.as_bytes());
                        count += 1;
                        logical_bytes = logical_bytes.saturating_add(body.len() as u64);
                    }
                }
                {
                    let mut statement = tx.prepare(
                        "SELECT queue, id, body, attempts, created_at, died_at, reason
                           FROM dead_letter
                          WHERE tenant = ?1
                          ORDER BY queue, id",
                    )?;
                    let mut rows = statement.query([&tenant_id])?;
                    while let Some(row) = rows.next()? {
                        let queue: String = row.get(0)?;
                        let id: String = row.get(1)?;
                        let body: Vec<u8> = row.get(2)?;
                        let attempts: i32 = row.get(3)?;
                        let created_at: i64 = row.get(4)?;
                        let died_at: i64 = row.get(5)?;
                        let reason: String = row.get(6)?;
                        hash.field(b"dead-letter");
                        hash.field(queue.as_bytes());
                        hash.field(id.as_bytes());
                        hash.field(&body);
                        hash.field(&attempts.to_be_bytes());
                        hash.field(&created_at.to_be_bytes());
                        hash.field(&died_at.to_be_bytes());
                        hash.field(reason.as_bytes());
                        count += 1;
                        logical_bytes = logical_bytes.saturating_add(body.len() as u64);
                    }
                }
                tx.commit()?;
                Ok(hash.finish("queue", count, logical_bytes, 0, 0))
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
        let tenants = self
            .pool
            .with_conn(|conn| {
                let mut statement =
                    conn.prepare("SELECT DISTINCT tenant FROM dead_letter ORDER BY tenant")?;
                let tenants = statement
                    .query_map([], |row| row.get::<_, String>(0))?
                    .collect::<Result<Vec<_>, _>>()?;
                Ok(tenants)
            })
            .await?;
        let mut removed = 0usize;
        for tenant in tenants {
            // Retention is a mutation too. A fenced tenant keeps its dead
            // letters unchanged until migration completes or rolls back.
            let permit = match self.fences.permit(&tenant).await {
                Ok(permit) => permit,
                Err(deis_backend_migration::FenceError::Fenced(_)) => continue,
                Err(error) => return Err(error.into()),
            };
            let scoped_tenant = tenant.clone();
            removed += self
                .pool
                .with_conn(move |conn| {
                    let count = conn.execute(
                        "DELETE FROM dead_letter WHERE tenant = ?1 AND died_at <= ?2",
                        rusqlite::params![scoped_tenant, cutoff],
                    )?;
                    Ok(count)
                })
                .await?;
            drop(permit);
        }
        Ok(removed)
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

/// Refuse a pull that could cross a deployment cutover. This is called from
/// inside the same write transaction that leases messages.
fn enforce_consumer_generation(
    tx: &Transaction<'_>,
    tenant: &str,
    consumer_generation: &str,
) -> QueueResult<()> {
    let fence: Option<(String, Option<String>)> = tx
        .query_row(
            "SELECT active_generation, pending_generation
               FROM consumer_generation_fences WHERE tenant = ?1",
            [tenant],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    match fence {
        None if consumer_generation.is_empty() => Ok(()),
        None => Err(QueueError::ConsumerGenerationFenced(format!(
            "tenant {tenant:?} has not activated generation {consumer_generation}"
        ))),
        Some((active, Some(pending))) => Err(QueueError::ConsumerGenerationFenced(format!(
            "tenant {tenant:?} is draining generation {active:?} before activating {pending:?}"
        ))),
        Some((active, None)) if active == consumer_generation => Ok(()),
        Some((active, None)) => Err(QueueError::ConsumerGenerationFenced(format!(
            "tenant {tenant:?} admits generation {active:?}, not {consumer_generation:?}"
        ))),
    }
}

type ConsumerFenceRow = (
    String,
    String,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
);

fn validate_generation_identity(field: &str, value: &str) -> QueueResult<()> {
    value
        .parse::<deis_wire::CanonicalId>()
        .map(|_| ())
        .map_err(|error| QueueError::InvalidArgument(format!("{field}: {error}")))
}

fn validate_previous_deployment_identity(value: &str) -> QueueResult<()> {
    let id = uuid::Uuid::parse_str(value).map_err(|_| {
        QueueError::InvalidArgument("expected_previous_deployment_id is not a UUID".to_string())
    })?;
    if !matches!(id.get_version_num(), 4 | 7) {
        return Err(QueueError::InvalidArgument(
            "expected_previous_deployment_id must be UUIDv4 or UUIDv7".to_string(),
        ));
    }
    if id.to_string() != value {
        return Err(QueueError::InvalidArgument(
            "expected_previous_deployment_id must use lowercase hyphenated UUID spelling"
                .to_string(),
        ));
    }
    Ok(())
}

fn validate_expected_consumer_baseline(
    generation: Option<&str>,
    deployment_id: Option<&str>,
) -> QueueResult<()> {
    if generation.is_some() == deployment_id.is_some() {
        return Ok(());
    }
    Err(QueueError::InvalidArgument(
        "expected_previous_generation and expected_previous_deployment_id must either both be present or both be absent"
            .to_string(),
    ))
}

fn consumer_baseline_matches(
    active_generation: &str,
    active_deployment_id: &str,
    expected_generation: Option<&str>,
    expected_deployment_id: Option<&str>,
) -> bool {
    if active_generation.is_empty() && active_deployment_id.is_empty() {
        return expected_generation.is_none() && expected_deployment_id.is_none();
    }
    expected_generation == Some(active_generation)
        && expected_deployment_id == Some(active_deployment_id)
}

fn load_consumer_fence(
    tx: &Transaction<'_>,
    tenant: &str,
) -> QueueResult<Option<ConsumerFenceRow>> {
    tx.query_row(
        "SELECT active_generation, active_deployment_id, pending_generation,
                pending_deployment_id, expected_previous_generation,
                expected_previous_deployment_id, last_aborted_deployment_id,
                last_aborted_generation
           FROM consumer_generation_fences WHERE tenant = ?1",
        [tenant],
        |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
                row.get(5)?,
                row.get(6)?,
                row.get(7)?,
            ))
        },
    )
    .optional()
    .map_err(QueueError::from)
}

fn outstanding_generation_leases(tx: &Transaction<'_>, tenant: &str, now: i64) -> QueueResult<i64> {
    tx.query_row(
        "SELECT count(*) FROM messages
          WHERE tenant = ?1 AND lease_token <> '' AND visible_at > ?2",
        rusqlite::params![tenant, now],
        |row| row.get(0),
    )
    .map_err(QueueError::from)
}

fn prepare_consumer_generation_tx(
    tx: &Transaction<'_>,
    tenant: &str,
    deployment_id: &str,
    generation: &str,
    expected_previous_generation: Option<&str>,
    expected_previous_deployment_id: Option<&str>,
    now: i64,
) -> QueueResult<PrepareConsumerGenerationResponse> {
    let stored = load_consumer_fence(tx, tenant)?;
    let active_before = stored
        .as_ref()
        .map(|(active, _, _, _, _, _, _, _)| active.clone())
        .unwrap_or_else(|| expected_previous_generation.unwrap_or_default().to_string());
    let active_owner_before = stored
        .as_ref()
        .map(|(_, active_owner, _, _, _, _, _, _)| active_owner.clone())
        .unwrap_or_else(|| {
            expected_previous_deployment_id
                .unwrap_or_default()
                .to_string()
        });

    match stored {
        None => {
            // Adopt the generation already selected by a pre-fence control
            // plane when one exists; otherwise the active side is the legacy
            // unlabelled consumer generation. Either way pending closes all
            // Pull admission before leases are inspected.
            tx.execute(
                "INSERT INTO consumer_generation_fences
                   (tenant, active_generation, active_deployment_id, pending_generation,
                    pending_deployment_id, expected_previous_generation,
                    expected_previous_deployment_id, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                rusqlite::params![
                    tenant,
                    active_before,
                    active_owner_before,
                    generation,
                    deployment_id,
                    expected_previous_generation,
                    expected_previous_deployment_id,
                    now
                ],
            )?;
        }
        Some((
            active,
            mut active_deployment,
            pending,
            pending_deployment,
            stored_expected,
            stored_expected_owner,
            _,
            _,
        )) => {
            if active == generation && pending.is_none() {
                if active_deployment.is_empty() {
                    let Some(previous_owner) = expected_previous_deployment_id else {
                        return Err(QueueError::ConsumerGenerationFenced(format!(
                            "tenant {tenant:?} generation {generation:?} has no recorded deployment owner; PrepareConsumerGeneration must provide its legacy ownership baseline"
                        )));
                    };
                    if expected_previous_generation != Some(active.as_str()) {
                        return Err(QueueError::ConsumerGenerationFenced(format!(
                            "tenant {tenant:?} cannot adopt generation {active:?} from a different previous generation"
                        )));
                    }
                    let adopted_owner = if previous_owner.is_empty() {
                        deployment_id
                    } else if previous_owner == deployment_id {
                        previous_owner
                    } else {
                        return Err(QueueError::ConsumerGenerationFenced(format!(
                            "tenant {tenant:?} generation {generation:?} is reported as owned by deployment {previous_owner:?}, not {deployment_id:?}"
                        )));
                    };
                    tx.execute(
                        "UPDATE consumer_generation_fences
                            SET active_deployment_id = ?2, updated_at = ?3
                          WHERE tenant = ?1 AND active_deployment_id = ''",
                        rusqlite::params![tenant, adopted_owner, now],
                    )?;
                } else if active_deployment != deployment_id {
                    return Err(QueueError::ConsumerGenerationFenced(format!(
                        "tenant {tenant:?} generation {generation:?} is owned by deployment {active_deployment:?}, not {deployment_id:?}"
                    )));
                }
                return Ok(PrepareConsumerGenerationResponse {
                    prepared: true,
                    active_generation: active,
                    pending_generation: String::new(),
                    outstanding_leases: 0,
                    active_deployment_id: if active_deployment.is_empty() {
                        deployment_id.to_string()
                    } else {
                        active_deployment
                    },
                    pending_deployment_id: String::new(),
                });
            }
            if let Some(pending) = pending {
                if pending != generation
                    || pending_deployment.as_deref() != Some(deployment_id)
                    || stored_expected.as_deref() != expected_previous_generation
                {
                    return Err(QueueError::ConsumerGenerationFenced(format!(
                        "tenant {tenant:?} already has generation {pending:?} pending under deployment {:?}",
                        pending_deployment.as_deref().unwrap_or("<unknown>")
                    )));
                }
                let stored_owner = stored_expected_owner.as_deref();
                let adopts_known_legacy_owner = active_deployment.is_empty()
                    && expected_previous_generation == Some(active.as_str())
                    && expected_previous_deployment_id.is_some_and(|owner| !owner.is_empty())
                    && stored_owner.is_none_or(str::is_empty);
                if stored_owner != expected_previous_deployment_id && !adopts_known_legacy_owner {
                    return Err(QueueError::ConsumerGenerationFenced(format!(
                        "tenant {tenant:?} pending generation {pending:?} was prepared against deployment owner {:?}, not {:?}",
                        stored_owner.unwrap_or("<none>"),
                        expected_previous_deployment_id.unwrap_or("<none>")
                    )));
                }
                if active_deployment.is_empty()
                    && expected_previous_generation == Some(active.as_str())
                    && let Some(previous_owner) =
                        expected_previous_deployment_id.filter(|owner| !owner.is_empty())
                {
                    tx.execute(
                        "UPDATE consumer_generation_fences
                            SET active_deployment_id = ?2,
                                expected_previous_deployment_id = ?2,
                                updated_at = ?3
                          WHERE tenant = ?1 AND active_deployment_id = ''",
                        rusqlite::params![tenant, previous_owner, now],
                    )?;
                }
            } else {
                if active_deployment.is_empty()
                    && !active.is_empty()
                    && expected_previous_generation == Some(active.as_str())
                    && let Some(previous_owner) =
                        expected_previous_deployment_id.filter(|owner| !owner.is_empty())
                {
                    tx.execute(
                        "UPDATE consumer_generation_fences
                            SET active_deployment_id = ?2, updated_at = ?3
                          WHERE tenant = ?1 AND active_deployment_id = ''",
                        rusqlite::params![tenant, previous_owner, now],
                    )?;
                    active_deployment = previous_owner.to_string();
                }
                if !consumer_baseline_matches(
                    &active,
                    &active_deployment,
                    expected_previous_generation,
                    expected_previous_deployment_id,
                ) {
                    return Err(QueueError::ConsumerGenerationFenced(format!(
                        "tenant {tenant:?} currently admits generation {active:?} owned by {:?}; deployment {deployment_id} expected generation {:?} owned by {:?}",
                        if active_deployment.is_empty() {
                            "<legacy>"
                        } else {
                            active_deployment.as_str()
                        },
                        expected_previous_generation.unwrap_or("<none>"),
                        expected_previous_deployment_id.unwrap_or("<none>")
                    )));
                }
                tx.execute(
                    "UPDATE consumer_generation_fences
                        SET pending_generation = ?2,
                            pending_deployment_id = ?3,
                            expected_previous_generation = ?4,
                            expected_previous_deployment_id = ?5,
                            last_aborted_deployment_id = NULL,
                            last_aborted_generation = NULL,
                            updated_at = ?6
                      WHERE tenant = ?1",
                    rusqlite::params![
                        tenant,
                        generation,
                        deployment_id,
                        expected_previous_generation,
                        expected_previous_deployment_id,
                        now
                    ],
                )?;
            }
        }
    }

    // PREPARE deliberately leaves pending in place after the count reaches
    // zero. Until an explicit ACTIVATE or ABORT, no generation may Pull.
    let outstanding = outstanding_generation_leases(tx, tenant, now)?;
    let (active, active_owner, pending, pending_owner, _, _, _, _) =
        load_consumer_fence(tx, tenant)?
            .expect("PrepareConsumerGeneration persisted the tenant fence");
    Ok(PrepareConsumerGenerationResponse {
        prepared: outstanding == 0,
        active_generation: active,
        pending_generation: pending.unwrap_or_default(),
        outstanding_leases: outstanding,
        active_deployment_id: active_owner,
        pending_deployment_id: pending_owner.unwrap_or_default(),
    })
}

fn activate_consumer_generation_tx(
    tx: &Transaction<'_>,
    tenant: &str,
    deployment_id: &str,
    generation: &str,
    now: i64,
) -> QueueResult<ActivateConsumerGenerationResponse> {
    let Some((active, active_deployment, pending, pending_deployment, _, _, _, _)) =
        load_consumer_fence(tx, tenant)?
    else {
        return Err(QueueError::ConsumerGenerationFenced(format!(
            "tenant {tenant:?} has no prepared consumer generation"
        )));
    };

    if pending.is_none() && active == generation {
        if !active_deployment.is_empty() && active_deployment != deployment_id {
            return Err(QueueError::ConsumerGenerationFenced(format!(
                "tenant {tenant:?} generation {generation:?} is owned by deployment {active_deployment:?}, not {deployment_id:?}"
            )));
        }
        if active_deployment.is_empty() {
            return Err(QueueError::ConsumerGenerationFenced(format!(
                "tenant {tenant:?} generation {generation:?} has no recorded deployment owner; PrepareConsumerGeneration must explicitly adopt the legacy generation first"
            )));
        }
        return Ok(ActivateConsumerGenerationResponse {
            activated: true,
            active_generation: active,
            pending_generation: String::new(),
            outstanding_leases: 0,
            active_deployment_id: active_deployment,
            pending_deployment_id: String::new(),
        });
    }

    if pending.as_deref() != Some(generation)
        || pending_deployment.as_deref() != Some(deployment_id)
    {
        return Err(QueueError::ConsumerGenerationFenced(format!(
            "tenant {tenant:?} has no generation {generation:?} prepared under deployment {deployment_id:?}"
        )));
    }
    let outstanding = outstanding_generation_leases(tx, tenant, now)?;
    if outstanding != 0 {
        return Ok(ActivateConsumerGenerationResponse {
            activated: false,
            active_generation: active,
            pending_generation: generation.to_string(),
            outstanding_leases: outstanding,
            active_deployment_id: active_deployment,
            pending_deployment_id: deployment_id.to_string(),
        });
    }
    tx.execute(
        "UPDATE consumer_generation_fences
            SET active_generation = ?2,
                active_deployment_id = ?3,
                pending_generation = NULL,
                pending_deployment_id = NULL,
                expected_previous_generation = NULL,
                expected_previous_deployment_id = NULL,
                last_aborted_deployment_id = NULL,
                last_aborted_generation = NULL,
                updated_at = ?4
          WHERE tenant = ?1",
        rusqlite::params![tenant, generation, deployment_id, now],
    )?;
    Ok(ActivateConsumerGenerationResponse {
        activated: true,
        active_generation: generation.to_string(),
        pending_generation: String::new(),
        outstanding_leases: 0,
        active_deployment_id: deployment_id.to_string(),
        pending_deployment_id: String::new(),
    })
}

fn abort_consumer_generation_tx(
    tx: &Transaction<'_>,
    tenant: &str,
    deployment_id: &str,
    generation: &str,
    now: i64,
) -> QueueResult<AbortConsumerGenerationResponse> {
    let Some((
        active,
        active_deployment,
        pending,
        pending_deployment,
        _,
        _,
        last_aborted_deployment,
        last_aborted_generation,
    )) = load_consumer_fence(tx, tenant)?
    else {
        return Err(QueueError::ConsumerGenerationFenced(format!(
            "tenant {tenant:?} has no generation prepared under deployment {deployment_id:?}"
        )));
    };

    let aborted = match pending {
        Some(pending)
            if pending == generation && pending_deployment.as_deref() == Some(deployment_id) =>
        {
            tx.execute(
                "UPDATE consumer_generation_fences
                    SET pending_generation = NULL,
                        pending_deployment_id = NULL,
                        expected_previous_generation = NULL,
                        expected_previous_deployment_id = NULL,
                        last_aborted_deployment_id = ?2,
                        last_aborted_generation = ?3,
                        updated_at = ?4
                  WHERE tenant = ?1",
                rusqlite::params![tenant, deployment_id, generation, now],
            )?;
            true
        }
        Some(pending) => {
            return Err(QueueError::ConsumerGenerationFenced(format!(
                "tenant {tenant:?} has generation {pending:?} pending under deployment {:?}",
                pending_deployment.as_deref().unwrap_or("<unknown>")
            )));
        }
        None if active == generation => {
            return Err(QueueError::ConsumerGenerationFenced(format!(
                "tenant {tenant:?} generation {generation:?} is already active under deployment {:?} and cannot be aborted",
                if active_deployment.is_empty() {
                    "<unknown>"
                } else {
                    active_deployment.as_str()
                }
            )));
        }
        None if last_aborted_deployment.as_deref() == Some(deployment_id)
            && last_aborted_generation.as_deref() == Some(generation) =>
        {
            false
        }
        None => {
            return Err(QueueError::ConsumerGenerationFenced(format!(
                "tenant {tenant:?} has no aborted generation {generation:?} under deployment {deployment_id:?}"
            )));
        }
    };
    Ok(AbortConsumerGenerationResponse {
        aborted,
        active_generation: active,
        active_deployment_id: active_deployment,
    })
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

/// Add generation attribution to leases written by older queue builds.
fn add_lease_generation_column(conn: &rusqlite::Connection) -> QueueResult<()> {
    let mut present = false;
    conn.pragma(None, "table_info", "messages", |row| {
        let column: String = row.get(1)?;
        if column == "lease_generation" {
            present = true;
        }
        Ok(())
    })?;
    if present {
        return Ok(());
    }
    conn.execute_batch(
        "ALTER TABLE messages ADD COLUMN lease_generation TEXT NOT NULL DEFAULT ''",
    )?;
    tracing::info!(
        "added generation attribution to existing queue leases; legacy in-flight leases drain before the first fenced deployment activates"
    );
    Ok(())
}

/// Bind an active consumer generation to the deployment intent that created
/// it. Older rows are adopted by the first exact retry after upgrade.
fn add_active_deployment_id_column(conn: &rusqlite::Connection) -> QueueResult<()> {
    let mut present = false;
    conn.pragma(None, "table_info", "consumer_generation_fences", |row| {
        let column: String = row.get(1)?;
        if column == "active_deployment_id" {
            present = true;
        }
        Ok(())
    })?;
    if present {
        return Ok(());
    }
    conn.execute_batch(
        "ALTER TABLE consumer_generation_fences ADD COLUMN active_deployment_id TEXT NOT NULL DEFAULT ''",
    )?;
    tracing::info!(
        "added active deployment attribution to existing queue generation fences; the first exact post-upgrade retry adopts each legacy row"
    );
    Ok(())
}

/// Persist the deployment owner that formed a pending transition's CAS
/// baseline. Existing pending rows predate this field, so their current
/// active owner is the only lossless baseline that can be backfilled.
fn add_expected_previous_deployment_id_column(conn: &rusqlite::Connection) -> QueueResult<()> {
    let mut present = false;
    conn.pragma(None, "table_info", "consumer_generation_fences", |row| {
        let column: String = row.get(1)?;
        if column == "expected_previous_deployment_id" {
            present = true;
        }
        Ok(())
    })?;
    if present {
        return Ok(());
    }
    conn.execute_batch(
        "ALTER TABLE consumer_generation_fences
             ADD COLUMN expected_previous_deployment_id TEXT;
         UPDATE consumer_generation_fences
            SET expected_previous_deployment_id = active_deployment_id
          WHERE pending_generation IS NOT NULL;",
    )?;
    tracing::info!(
        "added previous deployment ownership to consumer generation fences; pending legacy rows inherit their active owner"
    );
    Ok(())
}

/// Retain the identity of the most recently completed abort so a lost gRPC
/// response can be retried without turning every unrelated candidate into a
/// successful no-op.
fn add_consumer_abort_tombstone_columns(conn: &rusqlite::Connection) -> QueueResult<()> {
    let mut deployment_present = false;
    let mut generation_present = false;
    conn.pragma(None, "table_info", "consumer_generation_fences", |row| {
        let column: String = row.get(1)?;
        deployment_present |= column == "last_aborted_deployment_id";
        generation_present |= column == "last_aborted_generation";
        Ok(())
    })?;
    if !deployment_present {
        conn.execute(
            "ALTER TABLE consumer_generation_fences ADD COLUMN last_aborted_deployment_id TEXT",
            [],
        )?;
    }
    if !generation_present {
        conn.execute(
            "ALTER TABLE consumer_generation_fences ADD COLUMN last_aborted_generation TEXT",
            [],
        )?;
    }
    if !deployment_present || !generation_present {
        tracing::info!("added exact abort retry tombstones to existing consumer generation fences");
    }
    conn.execute_batch(
        "CREATE TRIGGER IF NOT EXISTS consumer_abort_tombstone_pair_insert_v1
         BEFORE INSERT ON consumer_generation_fences
         WHEN (NEW.last_aborted_deployment_id IS NULL) !=
              (NEW.last_aborted_generation IS NULL)
         BEGIN
             SELECT RAISE(ABORT, 'consumer abort tombstone identity must be complete');
         END;
         CREATE TRIGGER IF NOT EXISTS consumer_abort_tombstone_pair_update_v1
         BEFORE UPDATE OF last_aborted_deployment_id, last_aborted_generation
         ON consumer_generation_fences
         WHEN (NEW.last_aborted_deployment_id IS NULL) !=
              (NEW.last_aborted_generation IS NULL)
         BEGIN
             SELECT RAISE(ABORT, 'consumer abort tombstone identity must be complete');
         END;",
    )?;
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
        let consumer_generation = request
            .metadata()
            .get(deis_wire::conventions::DEPLOYMENT_GENERATION_HEADER)
            .map(|value| {
                let value = value.to_str().map_err(|_| {
                    QueueError::InvalidArgument(
                        "deployment generation metadata is not ASCII".to_string(),
                    )
                })?;
                value.parse::<deis_wire::CanonicalId>().map_err(|error| {
                    QueueError::InvalidArgument(format!("deployment generation: {error}"))
                })?;
                Ok::<_, QueueError>(value.to_string())
            })
            .transpose()?;
        let req = request.into_inner();
        let leased = self
            .pull_for_generation(
                &tenant,
                &req.queue_name,
                Some(req.max_messages),
                Some(req.lease_seconds),
                consumer_generation.as_deref(),
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

    async fn prepare_consumer_generation(
        &self,
        request: Request<PrepareConsumerGenerationRequest>,
    ) -> Result<Response<PrepareConsumerGenerationResponse>, Status> {
        let tenant = tenant_of(&request)?;
        let req = request.into_inner();
        let response = self
            .prepare_consumer_generation(
                &tenant,
                &req.deployment_id,
                &req.generation,
                (!req.expected_previous_generation.is_empty())
                    .then_some(req.expected_previous_generation.as_str()),
                req.expected_previous_deployment_id.as_deref(),
            )
            .await?;
        Ok(Response::new(response))
    }

    async fn activate_consumer_generation(
        &self,
        request: Request<ActivateConsumerGenerationRequest>,
    ) -> Result<Response<ActivateConsumerGenerationResponse>, Status> {
        let tenant = tenant_of(&request)?;
        let req = request.into_inner();
        let response = self
            .activate_consumer_generation(&tenant, &req.deployment_id, &req.generation)
            .await?;
        Ok(Response::new(response))
    }

    async fn abort_consumer_generation(
        &self,
        request: Request<AbortConsumerGenerationRequest>,
    ) -> Result<Response<AbortConsumerGenerationResponse>, Status> {
        let tenant = tenant_of(&request)?;
        let req = request.into_inner();
        let response = self
            .abort_consumer_generation(&tenant, &req.deployment_id, &req.generation)
            .await?;
        Ok(Response::new(response))
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
                req.refund_attempt,
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

    /// Send and Ack throughput of one producer and one consumer, measured at
    /// both `synchronous` settings in one process, alternating rounds, so
    /// whatever else the host is doing lands on both and the comparison is
    /// between paired samples rather than between two moments in time.
    ///
    /// Ignored by default: it is a measurement, not an assertion. Run it with
    /// `cargo nextest run -p deis-queue --run-ignored ignored-only queue_throughput`
    /// and read the per-mode rates off stdout; the README records the result.
    ///
    /// `TMPDIR` must point at a real disk before running: on a tmpfs the
    /// commit fsync is a no-op and both modes read the in-memory speed,
    /// which makes the setting under test invisible rather than measured.
    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "benchmark; run with --run-ignored ignored-only to print the send/ack rate"]
    async fn queue_throughput() {
        const MESSAGES: usize = 2000;
        const ROUNDS: usize = 9;

        let dir = TempDir::new().unwrap();
        // One connection, so the mode a round pins is the mode every
        // statement of that round runs under: `with_conn` hands out the same
        // connection every time when the pool holds exactly one.
        let service = QueueService::open_with_clock(
            dir.path(),
            1,
            QueueLimits::default(),
            Clock::fixed(T0),
        )
        .await
        .unwrap();
        let tenant = tenant("bench.sandbox");

        let mut send_rates: Vec<(&str, f64)> = Vec::new();
        let mut ack_rates: Vec<(&str, f64)> = Vec::new();
        for round in 0..ROUNDS {
            for mode in ["NORMAL", "FULL"] {
                service
                    .pool
                    .with_conn(move |conn| {
                        conn.pragma_update(None, "synchronous", mode)?;
                        conn.execute("DELETE FROM messages", [])?;
                        Ok(())
                    })
                    .await
                    .unwrap();

                let started = std::time::Instant::now();
                for _ in 0..MESSAGES {
                    service
                        .send(&tenant, "bench", vec![0u8; 256])
                        .await
                        .unwrap();
                }
                let send_rate = MESSAGES as f64 / started.elapsed().as_secs_f64();

                let started = std::time::Instant::now();
                let mut acknowledged = 0usize;
                loop {
                    let leased = service
                        .pull(&tenant, "bench", Some(100), None)
                        .await
                        .unwrap();
                    if leased.is_empty() {
                        break;
                    }
                    acknowledged += service
                        .ack(&tenant, "bench", handles(&leased))
                        .await
                        .unwrap()
                        .acknowledged;
                }
                let ack_rate = acknowledged as f64 / started.elapsed().as_secs_f64();
                assert_eq!(acknowledged, MESSAGES);

                println!(
                    "queue_throughput: round {round} {mode} send {send_rate:.0}/s ack {ack_rate:.0}/s"
                );
                send_rates.push((mode, send_rate));
                ack_rates.push((mode, ack_rate));
            }
        }

        // The rate of one mode on its own moves with whatever else the host
        // is doing, which is worth several times the difference between the
        // two modes. So the headline is the paired ratio within each round,
        // whose median cancels that drift, and the medians are printed beside
        // it only to show the absolute scale.
        for (what, rates) in [("send", &send_rates), ("ack", &ack_rates)] {
            let mut normal = rates_in(rates, "NORMAL");
            let mut full = rates_in(rates, "FULL");
            let ratios = paired_ratios(&normal, &full);
            println!(
                "queue_throughput: {what}: NORMAL median {:.0}/s, FULL median {:.0}/s, \
                 paired FULL/NORMAL ratios {ratios:?}",
                median(&mut normal),
                median(&mut full),
            );
        }
    }

    /// The rates one mode reached, in round order.
    fn rates_in(rates: &[(&str, f64)], mode: &str) -> Vec<f64> {
        rates
            .iter()
            .filter(|(m, _)| *m == mode)
            .map(|(_, rate)| *rate)
            .collect()
    }

    fn paired_ratios(normal: &[f64], full: &[f64]) -> Vec<String> {
        normal
            .iter()
            .zip(full)
            .map(|(normal, full)| format!("{:.2}", full / normal))
            .collect()
    }

    fn median(values: &mut [f64]) -> f64 {
        values.sort_by(|a, b| a.partial_cmp(b).unwrap());
        values[values.len() / 2]
    }

    fn canonical_tenant() -> TenantId {
        TenantId::from_canonical(deis_wire::CanonicalTenantId::new(
            "018f2518-7e38-7f80-8000-000000000001".parse().unwrap(),
            "018f2518-7e38-7f80-8000-000000000002".parse().unwrap(),
        ))
    }

    fn bodies(messages: &[LeasedMessage]) -> Vec<String> {
        messages
            .iter()
            .map(|m| String::from_utf8(m.body.clone()).unwrap())
            .collect()
    }

    #[tokio::test]
    async fn deployment_generation_fence_drains_old_leases_before_cutover() {
        const PLAN_ONE: &str = "018f2518-7e38-7f80-8000-000000000011";
        const GEN_ONE: &str = "018f2518-7e38-7f80-8000-000000000012";
        const PLAN_TWO: &str = "018f2518-7e38-7f80-8000-000000000013";
        const GEN_TWO: &str = "018f2518-7e38-7f80-8000-000000000014";
        let f = fixture().await;
        let tenant = tenant("shop.production");
        f.service
            .send_batch(
                &tenant,
                "jobs",
                vec![b"old lease".to_vec(), b"next".to_vec()],
            )
            .await
            .unwrap();
        let old = f
            .service
            .pull(&tenant, "jobs", Some(1), Some(60))
            .await
            .unwrap();

        let draining = f
            .service
            .prepare_consumer_generation(&tenant, PLAN_ONE, GEN_ONE, None, None)
            .await
            .unwrap();
        assert!(!draining.prepared);
        assert_eq!(draining.outstanding_leases, 1);
        assert_eq!(draining.active_deployment_id, "");
        assert_eq!(draining.pending_deployment_id, PLAN_ONE);
        assert!(matches!(
            f.service
                .pull_for_generation(&tenant, "jobs", Some(1), None, None)
                .await,
            Err(QueueError::ConsumerGenerationFenced(_))
        ));
        assert!(matches!(
            f.service
                .pull_for_generation(&tenant, "jobs", Some(1), None, Some(GEN_ONE))
                .await,
            Err(QueueError::ConsumerGenerationFenced(_))
        ));

        f.service.ack(&tenant, "jobs", handles(&old)).await.unwrap();
        let prepared = f
            .service
            .prepare_consumer_generation(&tenant, PLAN_ONE, GEN_ONE, None, None)
            .await
            .unwrap();
        assert!(prepared.prepared);
        assert_eq!(prepared.active_generation, "");
        assert_eq!(prepared.pending_generation, GEN_ONE);
        assert_eq!(prepared.active_deployment_id, "");
        assert_eq!(prepared.pending_deployment_id, PLAN_ONE);
        assert!(matches!(
            f.service
                .pull_for_generation(&tenant, "jobs", Some(1), None, Some(GEN_ONE))
                .await,
            Err(QueueError::ConsumerGenerationFenced(_))
        ));
        let active = f
            .service
            .activate_consumer_generation(&tenant, PLAN_ONE, GEN_ONE)
            .await
            .unwrap();
        assert!(active.activated);
        assert_eq!(active.active_generation, GEN_ONE);
        assert_eq!(active.active_deployment_id, PLAN_ONE);
        assert!(active.pending_deployment_id.is_empty());
        let active_retry = f
            .service
            .activate_consumer_generation(&tenant, PLAN_ONE, GEN_ONE)
            .await
            .unwrap();
        assert!(active_retry.activated);
        assert_eq!(active_retry.active_deployment_id, PLAN_ONE);
        assert!(active_retry.pending_deployment_id.is_empty());
        assert!(matches!(
            f.service
                .activate_consumer_generation(&tenant, PLAN_TWO, GEN_ONE)
                .await,
            Err(QueueError::ConsumerGenerationFenced(_))
        ));
        assert!(matches!(
            f.service.pull(&tenant, "jobs", Some(1), None).await,
            Err(QueueError::ConsumerGenerationFenced(_))
        ));

        let first_generation = f
            .service
            .pull_for_generation(&tenant, "jobs", Some(1), Some(60), Some(GEN_ONE))
            .await
            .unwrap();
        assert_eq!(bodies(&first_generation), ["next"]);
        assert!(matches!(
            f.service
                .prepare_consumer_generation(&tenant, PLAN_TWO, GEN_TWO, None, None)
                .await,
            Err(QueueError::ConsumerGenerationFenced(_))
        ));
        assert!(matches!(
            f.service
                .prepare_consumer_generation(
                    &tenant,
                    PLAN_TWO,
                    GEN_TWO,
                    Some(GEN_ONE),
                    Some(PLAN_TWO),
                )
                .await,
            Err(QueueError::ConsumerGenerationFenced(_))
        ));
        let second_draining = f
            .service
            .prepare_consumer_generation(&tenant, PLAN_TWO, GEN_TWO, Some(GEN_ONE), Some(PLAN_ONE))
            .await
            .unwrap();
        assert!(!second_draining.prepared);
        assert_eq!(second_draining.active_deployment_id, PLAN_ONE);
        assert_eq!(second_draining.pending_deployment_id, PLAN_TWO);
        f.service
            .ack(&tenant, "jobs", handles(&first_generation))
            .await
            .unwrap();
        assert!(
            f.service
                .prepare_consumer_generation(
                    &tenant,
                    PLAN_TWO,
                    GEN_TWO,
                    Some(GEN_ONE),
                    Some(PLAN_ONE),
                )
                .await
                .unwrap()
                .prepared
        );
        assert!(
            f.service
                .activate_consumer_generation(&tenant, PLAN_TWO, GEN_TWO)
                .await
                .unwrap()
                .activated
        );
    }

    #[tokio::test]
    async fn a_legacy_active_generation_requires_explicit_owner_takeover() {
        const LEGACY_GENERATION: &str = "018f2518-7e38-7f80-8000-000000000071";
        const CANDIDATE_PLAN: &str = "018f2518-7e38-7f80-8000-000000000072";
        const CANDIDATE_GENERATION: &str = "018f2518-7e38-7f80-8000-000000000073";
        const ADOPTING_PLAN: &str = "018f2518-7e38-7f80-8000-000000000074";
        let f = fixture().await;
        let tenant = tenant("shop.production");

        // A queue with no fence may be introduced to a control-plane
        // generation whose historical deployment owner was never stored.
        assert!(
            f.service
                .prepare_consumer_generation(
                    &tenant,
                    CANDIDATE_PLAN,
                    CANDIDATE_GENERATION,
                    Some(LEGACY_GENERATION),
                    Some(""),
                )
                .await
                .unwrap()
                .prepared
        );
        f.service
            .abort_consumer_generation(&tenant, CANDIDATE_PLAN, CANDIDATE_GENERATION)
            .await
            .unwrap();

        assert!(matches!(
            f.service
                .activate_consumer_generation(&tenant, ADOPTING_PLAN, LEGACY_GENERATION)
                .await,
            Err(QueueError::ConsumerGenerationFenced(_))
        ));
        assert!(matches!(
            f.service
                .prepare_consumer_generation(
                    &tenant,
                    ADOPTING_PLAN,
                    LEGACY_GENERATION,
                    Some(LEGACY_GENERATION),
                    None,
                )
                .await,
            Err(QueueError::InvalidArgument(_))
        ));
        let adopted = f
            .service
            .prepare_consumer_generation(
                &tenant,
                ADOPTING_PLAN,
                LEGACY_GENERATION,
                Some(LEGACY_GENERATION),
                Some(""),
            )
            .await
            .unwrap();
        assert!(adopted.prepared);
        assert_eq!(adopted.active_generation, LEGACY_GENERATION);
        assert_eq!(adopted.active_deployment_id, ADOPTING_PLAN);
        assert!(adopted.pending_generation.is_empty());
        assert!(adopted.pending_deployment_id.is_empty());
        assert!(
            f.service
                .activate_consumer_generation(&tenant, ADOPTING_PLAN, LEGACY_GENERATION)
                .await
                .unwrap()
                .activated
        );
    }

    #[tokio::test]
    async fn a_historical_uuidv4_previous_owner_is_a_valid_strict_baseline() {
        const HISTORICAL_PLAN: &str = "550e8400-e29b-41d4-a716-446655440000";
        const PREVIOUS_GENERATION: &str = "018f2518-7e38-7f80-8000-000000000081";
        const PLAN_ONE: &str = "018f2518-7e38-7f80-8000-000000000082";
        const GENERATION_ONE: &str = "018f2518-7e38-7f80-8000-000000000083";
        const PLAN_TWO: &str = "018f2518-7e38-7f80-8000-000000000084";
        const GENERATION_TWO: &str = "018f2518-7e38-7f80-8000-000000000085";
        let f = fixture().await;
        let primary_tenant = tenant("shop.production");

        assert!(
            f.service
                .prepare_consumer_generation(
                    &primary_tenant,
                    PLAN_ONE,
                    GENERATION_ONE,
                    Some(PREVIOUS_GENERATION),
                    Some(HISTORICAL_PLAN),
                )
                .await
                .unwrap()
                .prepared
        );
        f.service
            .abort_consumer_generation(&primary_tenant, PLAN_ONE, GENERATION_ONE)
            .await
            .unwrap();
        assert!(
            f.service
                .prepare_consumer_generation(
                    &primary_tenant,
                    PLAN_TWO,
                    GENERATION_TWO,
                    Some(PREVIOUS_GENERATION),
                    Some(HISTORICAL_PLAN),
                )
                .await
                .unwrap()
                .prepared
        );

        let bad_spelling = tenant("shop.preview");
        assert!(matches!(
            f.service
                .prepare_consumer_generation(
                    &bad_spelling,
                    PLAN_ONE,
                    GENERATION_ONE,
                    Some(PREVIOUS_GENERATION),
                    Some("550E8400-E29B-41D4-A716-446655440000"),
                )
                .await,
            Err(QueueError::InvalidArgument(_))
        ));
        let bad_version = tenant("shop.staging");
        assert!(matches!(
            f.service
                .prepare_consumer_generation(
                    &bad_version,
                    PLAN_ONE,
                    GENERATION_ONE,
                    Some(PREVIOUS_GENERATION),
                    Some("550e8400-e29b-11d4-a716-446655440000"),
                )
                .await,
            Err(QueueError::InvalidArgument(_))
        ));
        let v4_candidate = tenant("shop.canary");
        assert!(matches!(
            f.service
                .prepare_consumer_generation(
                    &v4_candidate,
                    HISTORICAL_PLAN,
                    GENERATION_ONE,
                    None,
                    None,
                )
                .await,
            Err(QueueError::InvalidArgument(_))
        ));
    }

    #[tokio::test]
    async fn old_fence_rows_adopt_the_catalog_previous_owner_during_prepare() {
        const HISTORICAL_PLAN: &str = "550e8400-e29b-41d4-a716-446655440000";
        const PREVIOUS_GENERATION: &str = "018f2518-7e38-7f80-8000-000000000091";
        const PENDING_PLAN: &str = "018f2518-7e38-7f80-8000-000000000092";
        const PENDING_GENERATION: &str = "018f2518-7e38-7f80-8000-000000000093";
        const OTHER_PLAN: &str = "018f2518-7e38-7f80-8000-000000000094";
        const OTHER_GENERATION: &str = "018f2518-7e38-7f80-8000-000000000095";
        let dir = TempDir::new().unwrap();
        let database = dir.path().join("queue.sqlite");
        {
            let connection = rusqlite::Connection::open(&database).unwrap();
            connection
                .execute_batch(&format!(
                    "CREATE TABLE consumer_generation_fences (
                         tenant                       TEXT PRIMARY KEY,
                         active_generation            TEXT NOT NULL,
                         active_deployment_id         TEXT NOT NULL DEFAULT '',
                         pending_generation           TEXT,
                         pending_deployment_id         TEXT,
                         expected_previous_generation TEXT,
                         updated_at                   INTEGER NOT NULL
                     );
                     INSERT INTO consumer_generation_fences
                         (tenant, active_generation, active_deployment_id,
                          pending_generation, pending_deployment_id,
                          expected_previous_generation, updated_at)
                     VALUES
                         ('shop.production', '{PREVIOUS_GENERATION}', '',
                          '{PENDING_GENERATION}', '{PENDING_PLAN}',
                          '{PREVIOUS_GENERATION}', {T0}),
                         ('shop.preview', '{PREVIOUS_GENERATION}', '',
                          NULL, NULL, NULL, {T0});"
                ))
                .unwrap();
        }
        let queue =
            QueueService::open_with_clock(dir.path(), 4, QueueLimits::default(), Clock::fixed(T0))
                .await
                .unwrap();
        let pending_tenant = tenant("shop.production");

        assert!(matches!(
            queue
                .prepare_consumer_generation(
                    &pending_tenant,
                    PENDING_PLAN,
                    PENDING_GENERATION,
                    Some(OTHER_GENERATION),
                    Some(HISTORICAL_PLAN),
                )
                .await,
            Err(QueueError::ConsumerGenerationFenced(_))
        ));
        assert!(matches!(
            queue
                .prepare_consumer_generation(
                    &pending_tenant,
                    OTHER_PLAN,
                    OTHER_GENERATION,
                    Some(PREVIOUS_GENERATION),
                    Some(HISTORICAL_PLAN),
                )
                .await,
            Err(QueueError::ConsumerGenerationFenced(_))
        ));
        let owner_before: String = queue
            .pool
            .with_conn(|connection| {
                connection
                    .query_row(
                        "SELECT active_deployment_id
                           FROM consumer_generation_fences
                          WHERE tenant = 'shop.production'",
                        [],
                        |row| row.get(0),
                    )
                    .map_err(QueueError::from)
            })
            .await
            .unwrap();
        assert!(owner_before.is_empty());

        assert!(
            queue
                .prepare_consumer_generation(
                    &pending_tenant,
                    PENDING_PLAN,
                    PENDING_GENERATION,
                    Some(PREVIOUS_GENERATION),
                    Some(HISTORICAL_PLAN),
                )
                .await
                .unwrap()
                .prepared
        );
        assert!(
            queue
                .prepare_consumer_generation(
                    &pending_tenant,
                    PENDING_PLAN,
                    PENDING_GENERATION,
                    Some(PREVIOUS_GENERATION),
                    Some(HISTORICAL_PLAN),
                )
                .await
                .unwrap()
                .prepared,
            "the upgraded pending tuple must remain idempotent"
        );
        let adopted: (String, String) = queue
            .pool
            .with_conn(|connection| {
                connection
                    .query_row(
                        "SELECT active_deployment_id, expected_previous_deployment_id
                           FROM consumer_generation_fences
                          WHERE tenant = 'shop.production'",
                        [],
                        |row| Ok((row.get(0)?, row.get(1)?)),
                    )
                    .map_err(QueueError::from)
            })
            .await
            .unwrap();
        assert_eq!(
            adopted,
            (HISTORICAL_PLAN.to_string(), HISTORICAL_PLAN.to_string())
        );

        let idle_tenant = tenant("shop.preview");
        assert!(
            queue
                .prepare_consumer_generation(
                    &idle_tenant,
                    OTHER_PLAN,
                    OTHER_GENERATION,
                    Some(PREVIOUS_GENERATION),
                    Some(HISTORICAL_PLAN),
                )
                .await
                .unwrap()
                .prepared
        );
    }

    #[tokio::test]
    async fn aborting_a_prepared_generation_reopens_only_the_previous_one() {
        const PLAN_ONE: &str = "018f2518-7e38-7f80-8000-000000000021";
        const GEN_ONE: &str = "018f2518-7e38-7f80-8000-000000000022";
        const PLAN_TWO: &str = "018f2518-7e38-7f80-8000-000000000023";
        const GEN_TWO: &str = "018f2518-7e38-7f80-8000-000000000024";
        let f = fixture().await;
        let tenant = tenant("shop.production");
        assert!(
            f.service
                .prepare_consumer_generation(&tenant, PLAN_ONE, GEN_ONE, None, None)
                .await
                .unwrap()
                .prepared
        );
        f.service
            .activate_consumer_generation(&tenant, PLAN_ONE, GEN_ONE)
            .await
            .unwrap();
        assert!(
            f.service
                .prepare_consumer_generation(
                    &tenant,
                    PLAN_TWO,
                    GEN_TWO,
                    Some(GEN_ONE),
                    Some(PLAN_ONE),
                )
                .await
                .unwrap()
                .prepared
        );
        let aborted = f
            .service
            .abort_consumer_generation(&tenant, PLAN_TWO, GEN_TWO)
            .await
            .unwrap();
        assert!(aborted.aborted);
        assert_eq!(aborted.active_generation, GEN_ONE);
        assert_eq!(aborted.active_deployment_id, PLAN_ONE);
        let retry = f
            .service
            .abort_consumer_generation(&tenant, PLAN_TWO, GEN_TWO)
            .await
            .expect("a lost abort response is safe to retry");
        assert!(!retry.aborted, "an exact retry has no new side effect");
        assert!(matches!(
            f.service
                .abort_consumer_generation(&tenant, PLAN_ONE, GEN_TWO)
                .await,
            Err(QueueError::ConsumerGenerationFenced(_))
        ));
        assert!(
            f.service
                .pull_for_generation(&tenant, "jobs", Some(1), None, Some(GEN_ONE))
                .await
                .is_ok()
        );
        assert!(matches!(
            f.service
                .pull_for_generation(&tenant, "jobs", Some(1), None, Some(GEN_TWO))
                .await,
            Err(QueueError::ConsumerGenerationFenced(_))
        ));
    }

    #[tokio::test]
    async fn activate_before_prepare_is_refused_without_creating_a_fence() {
        const PLAN: &str = "018f2518-7e38-7f80-8000-000000000031";
        const GENERATION: &str = "018f2518-7e38-7f80-8000-000000000032";
        let f = fixture().await;
        let tenant = tenant("shop.production");
        f.service
            .send(&tenant, "jobs", b"still legacy".to_vec())
            .await
            .unwrap();

        assert!(matches!(
            f.service
                .activate_consumer_generation(&tenant, PLAN, GENERATION)
                .await,
            Err(QueueError::ConsumerGenerationFenced(_))
        ));
        assert!(matches!(
            f.service
                .abort_consumer_generation(&tenant, PLAN, GENERATION)
                .await,
            Err(QueueError::ConsumerGenerationFenced(_))
        ));
        assert_eq!(
            bodies(&f.service.pull(&tenant, "jobs", None, None).await.unwrap()),
            ["still legacy"]
        );
    }

    #[tokio::test]
    async fn leases_in_every_queue_must_drain_before_prepare_completes() {
        const PLAN: &str = "018f2518-7e38-7f80-8000-000000000041";
        const GENERATION: &str = "018f2518-7e38-7f80-8000-000000000042";
        let f = fixture().await;
        let tenant = tenant("shop.production");
        f.service
            .send(&tenant, "jobs", b"job".to_vec())
            .await
            .unwrap();
        f.service
            .send(&tenant, "mail", b"mail".to_vec())
            .await
            .unwrap();
        let job = f
            .service
            .pull(&tenant, "jobs", Some(1), Some(60))
            .await
            .unwrap();
        let mail = f
            .service
            .pull(&tenant, "mail", Some(1), Some(60))
            .await
            .unwrap();

        let draining = f
            .service
            .prepare_consumer_generation(&tenant, PLAN, GENERATION, None, None)
            .await
            .unwrap();
        assert!(!draining.prepared);
        assert_eq!(draining.outstanding_leases, 2);

        f.service.ack(&tenant, "jobs", handles(&job)).await.unwrap();
        let one_left = f
            .service
            .prepare_consumer_generation(&tenant, PLAN, GENERATION, None, None)
            .await
            .unwrap();
        assert!(!one_left.prepared);
        assert_eq!(one_left.outstanding_leases, 1);

        f.service
            .ack(&tenant, "mail", handles(&mail))
            .await
            .unwrap();
        assert!(
            f.service
                .prepare_consumer_generation(&tenant, PLAN, GENERATION, None, None)
                .await
                .unwrap()
                .prepared
        );
    }

    #[tokio::test]
    async fn expired_old_generation_ack_and_nack_are_stale_after_activation() {
        const PLAN: &str = "018f2518-7e38-7f80-8000-000000000051";
        const GENERATION: &str = "018f2518-7e38-7f80-8000-000000000052";
        let f = fixture().await;
        let tenant = tenant("shop.production");
        f.service
            .send_batch(
                &tenant,
                "jobs",
                vec![b"late ack".to_vec(), b"late nack".to_vec()],
            )
            .await
            .unwrap();
        let old = f
            .service
            .pull(&tenant, "jobs", Some(2), Some(10))
            .await
            .unwrap();
        assert_eq!(old.len(), 2);
        assert!(
            !f.service
                .prepare_consumer_generation(&tenant, PLAN, GENERATION, None, None)
                .await
                .unwrap()
                .prepared
        );

        f.clock.advance(10);
        assert!(
            f.service
                .prepare_consumer_generation(&tenant, PLAN, GENERATION, None, None)
                .await
                .unwrap()
                .prepared
        );
        assert!(
            f.service
                .activate_consumer_generation(&tenant, PLAN, GENERATION)
                .await
                .unwrap()
                .activated
        );

        assert_eq!(
            f.service
                .ack(&tenant, "jobs", vec![handle(&old[0])])
                .await
                .unwrap(),
            AckOutcome {
                acknowledged: 0,
                stale_leases: 1,
            }
        );
        assert_eq!(
            f.service
                .nack(&tenant, "jobs", vec![handle(&old[1])], Some(0), false)
                .await
                .unwrap(),
            NackOutcome {
                released: 0,
                dead_lettered: 0,
                stale_leases: 1,
            }
        );
        let next = f
            .service
            .pull_for_generation(&tenant, "jobs", Some(2), None, Some(GENERATION))
            .await
            .unwrap();
        assert_eq!(bodies(&next), ["late ack", "late nack"]);
    }

    #[tokio::test]
    async fn a_prepared_generation_survives_queue_reopen() {
        const PLAN_ONE: &str = "018f2518-7e38-7f80-8000-000000000061";
        const GENERATION_ONE: &str = "018f2518-7e38-7f80-8000-000000000062";
        const PLAN_TWO: &str = "018f2518-7e38-7f80-8000-000000000063";
        const GENERATION_TWO: &str = "018f2518-7e38-7f80-8000-000000000064";
        let dir = TempDir::new().unwrap();
        let clock = Clock::fixed(T0);
        let tenant = tenant("shop.production");
        {
            let queue =
                QueueService::open_with_clock(dir.path(), 4, QueueLimits::default(), clock.clone())
                    .await
                    .unwrap();
            assert!(
                queue
                    .prepare_consumer_generation(&tenant, PLAN_ONE, GENERATION_ONE, None, None,)
                    .await
                    .unwrap()
                    .prepared
            );
            queue
                .activate_consumer_generation(&tenant, PLAN_ONE, GENERATION_ONE)
                .await
                .unwrap();
            assert!(
                queue
                    .prepare_consumer_generation(
                        &tenant,
                        PLAN_TWO,
                        GENERATION_TWO,
                        Some(GENERATION_ONE),
                        Some(PLAN_ONE),
                    )
                    .await
                    .unwrap()
                    .prepared
            );
        }

        let reopened = QueueService::open_with_clock(dir.path(), 4, QueueLimits::default(), clock)
            .await
            .unwrap();
        assert!(matches!(
            reopened
                .pull_for_generation(&tenant, "jobs", Some(1), None, Some(GENERATION_TWO))
                .await,
            Err(QueueError::ConsumerGenerationFenced(_))
        ));
        assert!(matches!(
            reopened
                .prepare_consumer_generation(
                    &tenant,
                    PLAN_TWO,
                    GENERATION_TWO,
                    Some(GENERATION_ONE),
                    Some(PLAN_TWO),
                )
                .await,
            Err(QueueError::ConsumerGenerationFenced(_))
        ));
        assert!(
            reopened
                .prepare_consumer_generation(
                    &tenant,
                    PLAN_TWO,
                    GENERATION_TWO,
                    Some(GENERATION_ONE),
                    Some(PLAN_ONE),
                )
                .await
                .unwrap()
                .prepared
        );
        assert!(
            reopened
                .activate_consumer_generation(&tenant, PLAN_TWO, GENERATION_TWO)
                .await
                .unwrap()
                .activated
        );
        assert!(
            reopened
                .pull_for_generation(&tenant, "jobs", Some(1), None, Some(GENERATION_TWO))
                .await
                .is_ok()
        );
    }

    #[tokio::test]
    async fn canonical_migration_target_rows_remain_separate_and_survive_reopen() {
        let dir = TempDir::new().unwrap();
        let canonical = canonical_tenant();
        let legacy = tenant("shop.production");
        {
            let queue = QueueService::open(dir.path(), 4, QueueLimits::default())
                .await
                .unwrap();
            queue
                .send(&canonical, "jobs", b"canonical".to_vec())
                .await
                .unwrap();
            queue
                .send(&legacy, "jobs", b"legacy".to_vec())
                .await
                .unwrap();
        }

        let decoded = TenantId::parse_persisted(canonical.as_str()).unwrap();
        let queue = QueueService::open(dir.path(), 4, QueueLimits::default())
            .await
            .unwrap();
        assert_eq!(
            bodies(&queue.pull(&decoded, "jobs", None, None).await.unwrap()),
            ["canonical"]
        );
        assert_eq!(
            bodies(&queue.pull(&legacy, "jobs", None, None).await.unwrap()),
            ["legacy"]
        );
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
            .nack(&a, "jobs", handles(&leased), Some(45), false)
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
            .nack(&a, "jobs", handles(&leased), None, false)
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
                .nack(&a, "jobs", handles(&leased), None, false)
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

    /// A delivery that the runtime refused at capacity burned no attempt, so
    /// refunding it at the ceiling must release the message rather than dead
    /// letter work nothing ever ran.
    ///
    /// The refund buys exactly one delivery, not an exemption: the next
    /// attempt that is nacked for real is still the last.
    #[tokio::test]
    async fn a_refunded_nack_releases_a_message_at_the_ceiling_and_a_plain_one_dead_letters_it() {
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

        // Two failures the application really had, then the delivery that
        // reaches the ceiling -- which the runtime refuses at capacity, so
        // its nack gives the attempt back.
        for attempt in 1..=3 {
            let leased = f.service.pull(&a, "jobs", None, None).await.unwrap();
            assert_eq!(leased.len(), 1, "attempt {attempt} should be delivered");
            assert_eq!(leased[0].attempts, attempt);
            let refund = attempt == 3;
            let outcome = f
                .service
                .nack(&a, "jobs", handles(&leased), None, refund)
                .await
                .unwrap();
            assert_eq!(
                outcome,
                NackOutcome {
                    released: 1,
                    dead_lettered: 0,
                    stale_leases: 0
                },
                "a refund at the ceiling releases instead of dead lettering"
            );
        }

        // Visible again, and the refund only bought one delivery: the stored
        // count was stepped back by the refunded attempt, so this pull is the
        // third counted one and the last.
        let again = f.service.pull(&a, "jobs", None, None).await.unwrap();
        assert_eq!(again.len(), 1);
        assert_eq!(again[0].id, id);
        assert_eq!(
            again[0].attempts, 3,
            "the refunded attempt is not counted, so the ceiling still binds"
        );

        // Nacked without a refund, it retires the way the test above ended:
        // the refund is one more delivery, not a new ceiling.
        let outcome = f
            .service
            .nack(&a, "jobs", handles(&again), None, false)
            .await
            .unwrap();
        assert_eq!(
            outcome,
            NackOutcome {
                released: 0,
                dead_lettered: 1,
                stale_leases: 0
            },
            "a plain nack of the delivery the refund bought must still dead letter"
        );
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
    }

    /// One below the ceiling the refund cannot change the outcome -- the
    /// message is released either way -- but the stored count is still
    /// stepped back, so the ceiling still binds where it is supposed to.
    #[tokio::test]
    async fn a_refunded_nack_below_the_ceiling_releases_the_message() {
        let f = fixture_with(QueueLimits {
            max_attempts: 3,
            ..QueueLimits::default()
        })
        .await;
        let a = tenant("app-one.production");
        let id = f
            .service
            .send(&a, "jobs", b"work".to_vec())
            .await
            .unwrap();

        // The first failure is real, so it keeps its attempt.
        let first = f.service.pull(&a, "jobs", None, None).await.unwrap();
        f.service
            .nack(&a, "jobs", handles(&first), None, false)
            .await
            .unwrap();

        // The second failure is an overload refusal, one below the ceiling:
        // released, and the refunded attempt is taken back off the row.
        let second = f.service.pull(&a, "jobs", None, None).await.unwrap();
        assert_eq!(second[0].attempts, 2);
        let outcome = f
            .service
            .nack(&a, "jobs", handles(&second), None, true)
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

        // The refund did not just make the message visible: it rewound the
        // count, so the next delivery is the second again rather than the
        // third and final one.
        let third = f.service.pull(&a, "jobs", None, None).await.unwrap();
        assert_eq!(third.len(), 1);
        assert_eq!(third[0].id, id);
        assert_eq!(
            third[0].attempts, 2,
            "the refunded attempt was stepped back off the row"
        );
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
            .nack(&a, "jobs", handles(&slow), Some(600), false)
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

    /// Expiry ends the proof even before another consumer takes the message.
    /// This is what prevents an old deployment from settling work after a
    /// generation barrier has declared its leases drained.
    #[tokio::test]
    async fn an_expired_acknowledgement_is_stale_before_redelivery() {
        let f = fixture().await;
        let a = tenant("app-one.production");
        f.service.send(&a, "jobs", b"work".to_vec()).await.unwrap();
        let leased = f.service.pull(&a, "jobs", None, Some(30)).await.unwrap();

        f.clock.advance(120);
        assert_eq!(
            f.service.ack(&a, "jobs", handles(&leased)).await.unwrap(),
            AckOutcome {
                acknowledged: 0,
                stale_leases: 1
            }
        );
        assert_eq!(
            f.service.pull(&a, "jobs", None, None).await.unwrap().len(),
            1
        );
    }

    #[tokio::test]
    async fn an_expired_nack_is_stale_before_redelivery() {
        let f = fixture().await;
        let a = tenant("app-one.production");
        f.service.send(&a, "jobs", b"work".to_vec()).await.unwrap();
        let leased = f.service.pull(&a, "jobs", None, Some(30)).await.unwrap();
        f.clock.advance(120);
        assert_eq!(
            f.service
                .nack(&a, "jobs", handles(&leased), None, false)
                .await
                .unwrap(),
            NackOutcome {
                released: 0,
                dead_lettered: 0,
                stale_leases: 1
            }
        );
        assert_eq!(
            f.service.pull(&a, "jobs", None, None).await.unwrap().len(),
            1
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
            .nack(&a, "jobs", handles(&leased), None, false)
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
            f.service.nack(&a, "jobs", vec![tokenless], None, false).await,
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
            .nack(&a, "jobs", handles(&leased), None, false)
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
            .nack(&a, "jobs", handles(&leased), None, false)
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
                .nack(&a, queue, handles(&leased), None, false)
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
            .nack(&other, "jobs", handles(&theirs), None, false)
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
                .nack(&a, "jobs", handles(&leased), None, false)
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
                .nack(&two, "jobs", vec![forged(&id)], None, false)
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
            f.service.nack(&a, "jobs", named, None, false).await,
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
            .nack(&a, "jobs", handles(&leased), None, false)
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
            .nack(&a, "jobs", handles(&leased), None, false)
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
